import AppKit
import Combine
import Foundation

/// Runs `tcr login --non-interactive` as a child of this app, so signing in is
/// something the panel does rather than something it hands to a Terminal
/// window.
///
/// ## Why this can exist now, when ``LoginLauncher`` could not
///
/// `LoginLauncher`'s doc-comment names two reasons a GUI could not spawn
/// `tcr login`, and both are gone for this path:
///
///  1. A modern proxy takes a login live while serving (`a385f0f`), so the
///     port no longer refuses one.
///  2. The CLI was interactive — a name prompt and a pasted-code fallback,
///     both on a stdin a background child does not have. `--non-interactive`
///     removes both: the loopback callback is the only completion path, and
///     the name is the identity the browser returns.
///
/// The browser is opened HERE, not by the CLI, because only this app can bring
/// its own window forward afterwards — the flag prints the URL and leaves it
/// to the caller for exactly that reason.
///
/// ``LoginLauncher`` stays for `tcr mint` (a different flow, still
/// interactive) and as the fallback when the `tcr` on this machine predates
/// the flag — see ``LoginCapability``.
@MainActor
public final class LoginSession: ObservableObject {

    /// Spelled out as ``LoginPhase``, which is deliberately NOT nested: this
    /// class is `@MainActor` and a nested type would inherit that isolation,
    /// putting the state machine — a plain value type — behind the main actor
    /// for no reason.
    public typealias Phase = LoginPhase

    @Published public private(set) var phase: Phase
    /// The URL the browser was sent to, kept for the "Copy link" button: the
    /// one recovery when the default browser did not come forward.
    @Published public private(set) var authorizeURL: URL?
    /// Everything the sheet says beyond the phase: which sign-in this is, tcr's
    /// notices, the plan a save named, the code a refusal named.
    @Published public private(set) var facts: LoginSheetFacts

    /// The sign-in this session runs, or will run once the person chooses one.
    /// `nil` only while the chooser is up.
    public private(set) var request: LoginRequest?

    /// The reason tcr refused this sign-in with, when it sent one: an `error`
    /// event's own words. A child that ended without one refused nothing.
    public var refusal: String? { flow.refusal }

    private var flow: LoginFlow
    private var process: Process?
    private var buffer = LineBuffer()
    private var stderrText = ""
    private var cancelled = false

    private let openURL: (URL) -> Void
    private let resolve: () -> Result<URL, TcrTool.NotFound>
    private let environment: [String: String]
    private let storedCodexExecutable: () -> String?
    private let resolvedCodexExecutable: () -> String?

    /// `request` is the sign-in to run, `nil` for the chooser, which runs nothing
    /// until ``begin(_:)``. `phase` is where the sheet opens: `.choosing`, or
    /// `.confirmingImport` for a re-import, or `.opening` for everything that
    /// starts at once. `codexSupported` is whether this `tcr` has
    /// `tcr login --codex`, when that is known, for the chooser's help.
    ///
    /// The facts are seeded from `request` here, so a session presented new, a
    /// Codex re-login say, draws as one from its first frame.
    ///
    /// `open`, `resolve`, `environment` and the two codex paths are injectable so
    /// a test can drive the whole session without a browser, an installed `tcr`
    /// or this Mac's codex. `storedCodexExecutable` is the path `Choose codex…`
    /// saved; `resolvedCodexExecutable` the one this Mac's search found.
    public init(
        request: LoginRequest?,
        phase: LoginPhase = .opening,
        codexSupported: Bool? = nil,
        open: @escaping (URL) -> Void = { NSWorkspace.shared.open($0) },
        resolve: @escaping () -> Result<URL, TcrTool.NotFound> = { TcrTool.resolve() },
        environment: [String: String] = ProcessInfo.processInfo.environment,
        storedCodexExecutable: @escaping () -> String? = {
            UserDefaults.standard.string(forKey: TcrTool.codexOverrideDefaultsKey)
        },
        resolvedCodexExecutable: @escaping () -> String? = { nil }
    ) {
        self.request = request
        self.phase = phase
        self.facts = LoginSheetFacts(request: request, codexSupported: codexSupported)
        self.openURL = open
        self.resolve = resolve
        self.environment = environment
        self.storedCodexExecutable = storedCodexExecutable
        self.resolvedCodexExecutable = resolvedCodexExecutable
        self.flow = LoginFlow(requesting: request?.account)
    }

    /// A Claude sign-in: `account` is the row a re-login targets, `nil` for a
    /// fresh add.
    public convenience init(
        account: String? = nil,
        open: @escaping (URL) -> Void = { NSWorkspace.shared.open($0) },
        resolve: @escaping () -> Result<URL, TcrTool.NotFound> = { TcrTool.resolve() }
    ) {
        self.init(request: .claude(account: account), open: open, resolve: resolve)
    }

    /// `tcr login --non-interactive [--account <name>]`. Passed as argv, never
    /// through a shell, so nothing here needs quoting — unlike
    /// ``LoginLauncher/script(forExecutableAt:reloggingIn:)``, which composes a
    /// real `.command` file and must.
    ///
    /// `--force` is never passed, for the same reason `LoginLauncher` never
    /// passes it: a GUI silently forcing past a guard the CLI put up is the
    /// wrong use of a button.
    public nonisolated static func arguments(account: String?) -> [String] {
        arguments(for: .claude(account: account))
    }

    /// The argv for each sign-in. A Codex sign-in is `tcr login --codex`, which
    /// runs codex's own sign-in in a folder of tcr's own; the import reads the
    /// folder a person picked. `--account` names the row a re-login or re-import
    /// repairs, and `--force` is never passed (``arguments(account:)``).
    public nonisolated static func arguments(for request: LoginRequest) -> [String] {
        switch request {
        case .claude(let account):
            var arguments = ["login", "--non-interactive"]
            if let account { arguments.append(contentsOf: ["--account", account]) }
            return arguments
        case .codex(let account):
            var arguments = ["login", "--codex", "--non-interactive"]
            if let account { arguments.append(contentsOf: ["--account", account]) }
            return arguments
        case .codexImport(let home, let account):
            var arguments = ["login", "--from-codex", "--codex-home", home]
            if let account { arguments.append(contentsOf: ["--account", account]) }
            arguments.append("--non-interactive")
            return arguments
        }
    }

    /// The child's environment: `inherited`, plus `TCR_CODEX_BIN` for a Codex
    /// sign-in.
    ///
    /// A TcrBar started from Finder has no shell `PATH`, so the `tcr` it starts
    /// would not find a codex that npm, nvm, volta or asdf put on this Mac. The
    /// codex a person chose with `Choose codex…` wins; without one, the codex this
    /// Mac's search found. With neither, and for every other sign-in, the inherited
    /// environment is unchanged.
    public nonisolated static func childEnvironment(
        for request: LoginRequest, inherited: [String: String], storedCodexExecutable: String?,
        resolvedCodexExecutable: String?
    ) -> [String: String] {
        guard case .codex = request,
            let codex = [storedCodexExecutable, resolvedCodexExecutable]
                .compactMap({ $0 }).first(where: { !$0.isEmpty })
        else { return inherited }
        var environment = inherited
        environment["TCR_CODEX_BIN"] = codex
        return environment
    }

    /// Start `request`, the chooser's choice or a confirmed import, in this
    /// sheet, and draw it from its first frame.
    ///
    /// Does nothing while a sign-in is running or after one has ended: a second
    /// click must not change the request a running child was started for.
    public func begin(_ request: LoginRequest) {
        guard process == nil, !phase.isTerminal else { return }
        self.request = request
        phase = .opening
        authorizeURL = nil
        facts = LoginSheetFacts(request: request, codexSupported: facts.codexSupported)
        start()
    }

    /// Show the import's confirmation for the folder `home`, which states the cost
    /// of sharing a codex login before anything runs. Nothing runs until the person
    /// confirms (``begin(_:)``). Does nothing while a sign-in is running or after
    /// one has ended.
    public func confirmImport(home: String, account: String?) {
        guard process == nil, !phase.isTerminal else { return }
        let request = LoginRequest.codexImport(home: home, account: account)
        self.request = request
        phase = .confirmingImport(home: home, account: account)
        facts.request = request
    }

    /// Say why the codex a person chose cannot be used, under tcr's reason.
    /// Changes nothing else: the sheet keeps its buttons, so they can choose again.
    public func refuseCodexChoice(_ sentence: String) {
        facts.refusedCodexChoice = sentence
    }

    /// Spawn the child and start reading its events. Safe to call once; a
    /// second call while one is in flight does nothing, and so does a call
    /// while the sheet still waits on a choice.
    public func start() {
        guard process == nil, !phase.isTerminal, let request else { return }
        switch phase {
        case .choosing, .confirmingImport: return
        case .opening, .waitingForBrowser, .saved, .failed: break
        }

        let executable: URL
        switch resolve() {
        case .success(let url): executable = url
        case .failure(let missing):
            phase = .failed(
                reason: "tcr not found (searched \(missing.searched.count) locations). "
                    + TcrTool.overrideRemedy)
            return
        }

        flow = LoginFlow(requesting: request.account)
        buffer = LineBuffer()
        stderrText = ""
        cancelled = false

        let child = Process()
        child.executableURL = executable
        child.arguments = Self.arguments(for: request)
        let childEnvironment = Self.childEnvironment(
            for: request, inherited: environment,
            storedCodexExecutable: storedCodexExecutable(),
            resolvedCodexExecutable: resolvedCodexExecutable())
        if childEnvironment != environment {
            child.environment = childEnvironment
        }
        let out = Pipe()
        let err = Pipe()
        child.standardOutput = out
        child.standardError = err
        // No stdin at all, rather than an inherited one: `--non-interactive`
        // never reads it, and a child that inherits the GUI's descriptor is
        // one CLI change away from blocking on a prompt nobody can answer.
        child.standardInput = FileHandle.nullDevice

        out.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            Task { @MainActor [weak self] in self?.ingest(data) }
        }
        err.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            let text = String(data: data, encoding: .utf8) ?? ""
            Task { @MainActor [weak self] in self?.stderrText += text }
        }
        child.terminationHandler = { [weak self] finished in
            Task { @MainActor [weak self] in
                self?.childEnded(exitCode: finished.terminationStatus)
            }
        }

        do {
            try child.run()
            process = child
        } catch {
            phase = .failed(reason: "could not run \(executable.path): \(error.localizedDescription)")
        }
    }

    /// Stop the login. The child is terminated rather than left running: it
    /// holds a loopback listener and would otherwise sit there for the rest of
    /// its 2-minute timeout, ready to accept a callback for a flow the person
    /// just abandoned.
    ///
    /// `terminate()` sends SIGTERM to tcr's whole process group, which also stops
    /// the `codex login` that `tcr login --codex` runs as a child of its own.
    /// Never change it to a SIGKILL, which tcr cannot clean up after, or to a
    /// signal sent to tcr's pid alone, which codex would outlive.
    public func cancel() {
        cancelled = true
        process?.terminate()
        process = nil
    }

    /// One chunk of the child's stdout. A chunk is not a line — the split and
    /// the carry-over live in ``LineBuffer`` so a URL arriving in two reads is
    /// still one event.
    private func ingest(_ data: Data) {
        for line in buffer.append(data) {
            guard let event = LoginProgressEvent.parse(line) else { continue }
            let toOpen = flow.apply(event)
            publishFlow()
            if let toOpen { openURL(toOpen) }
        }
    }

    /// The flow's state, as the sheet reads it.
    private func publishFlow() {
        phase = flow.phase
        authorizeURL = flow.authorizeURL
        var next = facts
        next.notices = flow.notices
        next.savedPlan = flow.savedPlan
        next.failureCode = flow.failureCode
        if next != facts { facts = next }
    }

    /// The child exited. A terminal phase already tells the whole story; this
    /// is for the case where it did not — a crash, a `terminate()`, or an exit
    /// that said nothing — which would otherwise leave the sheet spinning
    /// forever on a process that is gone.
    private func childEnded(exitCode: Int32) {
        process = nil
        guard !cancelled else { return }
        flow.finish(exitCode: exitCode, stderr: stderrText)
        publishFlow()
    }
}

/// Which sign-in a session runs.
public enum LoginRequest: Equatable, Sendable {
    /// `tcr login`: a Claude account, or the Claude row `account` names.
    case claude(account: String?)
    /// `tcr login --codex`: a Codex account, signed in by codex's own sign-in run
    /// in a folder of tcr's own, or the Codex row `account` names.
    case codex(account: String?)
    /// `tcr login --from-codex`: the login the codex CLI holds in `home`, shared,
    /// for a new account or for the row `account` names.
    case codexImport(home: String, account: String?)

    /// The row this request repairs, `nil` for a new account.
    public var account: String? {
        switch self {
        case .claude(let account), .codex(let account): return account
        case .codexImport(_, let account): return account
        }
    }

    /// Whether this signs in a Codex account, by either route.
    public var isCodex: Bool {
        switch self {
        case .claude: return false
        case .codex, .codexImport: return true
        }
    }
}

/// What the sign-in sheet says beyond its phase. Every field defaults to empty, so
/// a sheet given none draws exactly what it drew before Codex accounts.
public struct LoginSheetFacts: Equatable, Sendable {
    /// The sign-in the sheet is about; `nil` and `.claude` draw the Claude sheet.
    public var request: LoginRequest?
    /// Every `notice` tcr sent, in order, each shown verbatim.
    public var notices: [String]
    /// The plan a Codex save named (`ChatGPT Pro`).
    public var savedPlan: String?
    /// The code a refusal named, which titles the failed sheet.
    public var failureCode: String?
    /// Whether this `tcr` has `tcr login --codex`, when known.
    public var codexSupported: Bool?
    /// The `tcr` inside TcrBar, when this Mac has no other, for the saved
    /// sheet's way to start codex.
    public var bundledTcrWithoutCLI: String?
    /// Why the codex a person chose cannot be used, under tcr's reason.
    public var refusedCodexChoice: String?

    public init(
        request: LoginRequest? = nil, notices: [String] = [], savedPlan: String? = nil,
        failureCode: String? = nil, codexSupported: Bool? = nil,
        bundledTcrWithoutCLI: String? = nil, refusedCodexChoice: String? = nil
    ) {
        self.request = request
        self.notices = notices
        self.savedPlan = savedPlan
        self.failureCode = failureCode
        self.codexSupported = codexSupported
        self.bundledTcrWithoutCLI = bundledTcrWithoutCLI
        self.refusedCodexChoice = refusedCodexChoice
    }
}

/// What the sheet draws. No "idle": a session exists only while a login is in
/// flight or about to be chosen, and is discarded when the sheet closes.
public enum LoginPhase: Equatable, Sendable {
    /// Add account… on a Mac that shows signs of codex: the person picks a Claude
    /// or a Codex account, or the import, and nothing runs until they do.
    case choosing
    /// The import's confirmation, which states what sharing a codex login costs
    /// before anything runs. `account` is the row a re-import repairs.
    case confirmingImport(home: String, account: String?)
    /// Started, and nothing has come back yet.
    case opening
    /// The authorize URL is out and the callback is what settles this.
    /// `email` is the identity this login was aimed at (`--account`), not one
    /// the CLI reported — a fresh add has none to name yet.
    case waitingForBrowser(email: String?)
    /// The account is written, under this name.
    case saved(account: String)
    /// Nothing was written, and this is why, on one line.
    case failed(reason: String)

    /// Whether the login is over either way. A terminal phase is what stops
    /// the sheet spinning.
    public var isTerminal: Bool {
        switch self {
        case .choosing, .confirmingImport, .opening, .waitingForBrowser: return false
        case .saved, .failed: return true
        }
    }

    /// Where Add account… opens: the chooser on evidence of codex, and otherwise
    /// the Claude sign-in, at once, exactly as it always did.
    public static func firstPhase(for presence: CodexPresence) -> LoginPhase {
        presence.isPresent ? .choosing : .opening
    }

    /// Whether the sheet offers Copy link: only for a sign-in still running whose
    /// browser was sent to a URL. Nothing has started while the sheet waits on a
    /// choice, so there is no link to copy then.
    public func offersCopyLink(authorizeURL: URL?) -> Bool {
        guard authorizeURL != nil, !isTerminal else { return false }
        switch self {
        case .choosing, .confirmingImport: return false
        case .opening, .waitingForBrowser, .saved, .failed: return true
        }
    }
}

/// One line of `tcr login --non-interactive`'s stdout, parsed.
///
/// The wire format is `LoginEvent::line` in `src/oauth.rs` — one JSON object
/// per line, `event` naming which.
public enum LoginProgressEvent: Equatable, Sendable {
    /// Open this URL. Nothing has been read or written yet. `opened` means the
    /// sign-in opened a browser itself.
    case browser(url: String, opened: Bool = false)
    /// The URL is out; the loopback callback is what settles it now.
    case waiting
    /// One sentence tcr wants shown verbatim, before it saves or fails. Never a
    /// state change.
    case notice(text: String)
    /// Written, under this account name. `provider` and `plan` are tcr's own
    /// tokens, sent on a Codex save only.
    case saved(account: String, provider: String? = nil, plan: String? = nil)
    /// Nothing was written, and this is the whole reason. `code` names a
    /// refusal, so a sheet can title it without parsing the reason.
    case failed(reason: String, code: String? = nil)

    /// `nil` for anything that is not one of the five events: a blank line, a
    /// line of prose, a JSON object with an `event` this version does not
    /// know, or an event missing the field that carries its meaning.
    ///
    /// Ignoring rather than failing is deliberate. A parser that treated an
    /// unrecognised line as an error would turn any future diagnostic line —
    /// or a `tcr` newer than this app — into a failed login. The one thing a
    /// stray line must never do is change the state.
    public static func parse(_ line: String) -> LoginProgressEvent? {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("{"), let data = trimmed.data(using: .utf8),
            let object = try? JSONSerialization.jsonObject(with: data),
            let fields = object as? [String: Any],
            let event = fields["event"] as? String
        else { return nil }

        func text(_ key: String) -> String? {
            guard let value = fields[key] as? String, !value.isEmpty else { return nil }
            return value
        }

        switch event {
        case "browser":
            guard let url = text("url") else { return nil }
            return .browser(url: url, opened: fields["opened"] as? Bool ?? false)
        case "waiting":
            return .waiting
        case "notice":
            guard let notice = text("text") else { return nil }
            return .notice(text: notice)
        case "saved":
            guard let account = text("account") else { return nil }
            return .saved(account: account, provider: text("provider"), plan: text("plan"))
        case "error":
            guard let reason = text("reason") else { return nil }
            return .failed(reason: reason, code: text("code"))
        default:
            return nil
        }
    }
}

/// The state machine behind ``LoginSession``, with no `Process` in it, so the
/// whole sequence — including the ways it ends badly — is testable.
public struct LoginFlow {
    /// The account this login was aimed at, carried into
    /// ``LoginSession/Phase/waitingForBrowser(email:)`` so the sheet can say
    /// who is being signed in.
    public let requested: String?
    public private(set) var phase: LoginPhase = .opening
    /// Every notice this login sent, in order, for the sheet to show under its
    /// result.
    public private(set) var notices: [String] = []
    /// The URL a `browser` event sent the browser to, whether this app opened it
    /// or the sign-in did: Copy link needs it either way.
    public private(set) var authorizeURL: URL?
    /// The plan a `saved` event named, sent on a Codex save only.
    public private(set) var savedPlan: String?
    /// The code an `error` event named, which titles the failed sheet.
    public private(set) var failureCode: String?
    /// The reason an `error` event gave: tcr's refusal, in its own words. A login
    /// that ended without an `error` event refused nothing.
    public private(set) var refusal: String?

    public init(requesting: String? = nil) {
        self.requested = requesting
    }

    /// Fold one event in. Returns the URL the caller must open, and only on
    /// the event that asks for one — the side effect stays outside the machine.
    /// A `browser` event with `opened` returns nothing: the sign-in opened the
    /// browser itself, and a second window would be one too many.
    ///
    /// A notice never changes the phase, so a late one cannot undo a result,
    /// and it is kept even after `saved` or `error`. Every other event after a
    /// terminal phase is ignored: once a login has said `saved` or `error`,
    /// nothing that follows can unsay it.
    @discardableResult
    public mutating func apply(_ event: LoginProgressEvent) -> URL? {
        switch event {
        case .notice(let text):
            // Kept, and nothing else: a notice never moves or ends the login.
            notices.append(text)
            return nil
        case _ where phase.isTerminal:
            return nil
        case .browser(let url, let opened):
            // An unopenable URL is a failure, not a silently skipped step:
            // the browser is the only human part of this flow.
            guard let parsed = URL(string: url), parsed.scheme == "https" else {
                phase = .failed(reason: "tcr printed an authorize URL this app cannot open: \(url)")
                return nil
            }
            authorizeURL = parsed
            return opened ? nil : parsed
        case .waiting:
            phase = .waitingForBrowser(email: requested)
            return nil
        case .saved(let account, _, let plan):
            phase = .saved(account: account)
            savedPlan = plan
            return nil
        case .failed(let reason, let code):
            phase = .failed(reason: reason)
            failureCode = code
            refusal = reason
            return nil
        }
    }

    /// The child process ended. Only meaningful when it ended without saying
    /// how — an exit code and whatever reached stderr are all there is to go
    /// on, and "it stopped" must still become a visible failure rather than a
    /// spinner that never resolves.
    public mutating func finish(exitCode: Int32, stderr: String) {
        guard !phase.isTerminal else { return }
        let spoken = stderr.split(separator: "\n").last.map(String.init)?
            .trimmingCharacters(in: .whitespaces)
        if let spoken, !spoken.isEmpty {
            phase = .failed(reason: spoken)
        } else {
            phase = .failed(reason: "tcr login exited (code \(exitCode)) without saving an account")
        }
    }
}

/// Splits a byte stream into lines, keeping whatever the last read cut in
/// half.
///
/// A pipe read boundary lands wherever the scheduler puts it, so the authorize
/// URL — the longest line this stream carries by a wide margin — is exactly
/// the one likely to arrive in two pieces. Treating each read as a line loses
/// it.
public struct LineBuffer {
    private var carry = Data()

    public init() {}

    /// Every COMPLETE line in `data`, with any trailing partial line held for
    /// the next call.
    public mutating func append(_ data: Data) -> [String] {
        carry.append(data)
        var lines: [String] = []
        while let newline = carry.firstIndex(of: UInt8(ascii: "\n")) {
            let line = carry[carry.startIndex..<newline]
            lines.append(String(decoding: line, as: UTF8.self))
            carry = carry[carry.index(after: newline)...]
        }
        // Re-base the slice so the buffer does not keep growing its indices
        // across a long stream.
        carry = Data(carry)
        return lines
    }
}

/// Whether the `tcr` on this machine has `login --non-interactive` at all.
///
/// TcrBar and `tcr` update independently — the app resolves whichever binary
/// is on this machine (``TcrTool/resolve()``), which can be older than the app
/// that bundles it if someone put one on `PATH`. An app that assumed the flag
/// would spawn a child that exits immediately on an unknown argument, and the
/// person would see "exited without saving an account" where they used to see
/// a Terminal window that worked.
public enum LoginCapability {
    /// The answer, for one `tcr`'s `login --help` text. Pure so the parse is
    /// testable without a binary.
    public static func supportsNonInteractive(help: String) -> Bool {
        help.contains("--non-interactive")
    }

    private static var cached: [String: Bool] = [:]

    /// Ask a binary once and remember. Keyed by path: two different `tcr`s can
    /// answer differently, and the resolved path is what distinguishes them.
    ///
    /// A failed probe answers `false` — the Terminal hand-off works against
    /// every `tcr` ever shipped, so an unreadable answer must fall back to it
    /// rather than onto a flag that may not exist.
    @MainActor
    public static func probe(executable: URL) -> Bool {
        if let known = cached[executable.path] { return known }
        let answer: Bool
        do {
            let output = try TcrTool.run(
                executable: executable, arguments: ["login", "--help"])
            let help = (String(data: output.stdout, encoding: .utf8) ?? "") + output.stderr
            answer = output.exitCode == 0 && supportsNonInteractive(help: help)
        } catch {
            answer = false
        }
        cached[executable.path] = answer
        return answer
    }

    /// Every word of a help text, split on whitespace and on the punctuation clap
    /// prints around a flag (`,`, brackets, braces and `=`), with no empty pieces.
    ///
    /// A substring probe is wrong for `--codex`: `--codex-home` contains it, and a
    /// looser probe for `codex` also matches `--from-codex`, while a `tcr` that has
    /// only those two predates the in-app Codex sign-in. A flag is present only when
    /// it is one whole token.
    public static func helpTokens(_ help: String) -> Set<String> {
        let separators = CharacterSet.whitespacesAndNewlines
            .union(CharacterSet(charactersIn: ",[]()<>{}="))
        return Set(help.components(separatedBy: separators).filter { !$0.isEmpty })
    }

    /// Whether this help offers `tcr login --codex`, the in-app Codex sign-in.
    public static func supportsCodex(help: String) -> Bool { helpTokens(help).contains("--codex") }

    private static var codexCached: [String: Bool] = [:]

    /// Ask a binary once whether it has `tcr login --codex`, and remember, the way
    /// ``probe(executable:)`` asks about `--non-interactive`. A failed probe answers
    /// `false`, which sends a Codex sign-in to the Terminal script, the path every
    /// `tcr` that can import a codex login takes.
    @MainActor
    public static func probeCodex(executable: URL) -> Bool {
        if let known = codexCached[executable.path] { return known }
        let answer = (try? loginHelp(executable: executable)).map(supportsCodex(help:)) ?? false
        codexCached[executable.path] = answer
        return answer
    }

    /// The answer a probe already gave for this binary, or `nil` when none has run.
    /// Runs nothing: it words a menu item's help, which must not start a process.
    @MainActor
    public static func knownCodexSupport(executable: URL) -> Bool? {
        codexCached[executable.path]
    }

    /// Where a Codex sign-in or import runs for one `tcr`.
    public enum CodexLoginPath: Equatable, Sendable {
        /// `tcr login --codex` runs inside the sheet.
        case inSheet
        /// An older `tcr` that can import a codex login but cannot sign one in: a
        /// Terminal window runs `codex login`, then the import.
        case terminal
        /// A `tcr` that predates Codex accounts altogether, which no script can help.
        case predatesCodex
    }

    /// `.inSheet` when `--codex` is a whole token of the help, `.terminal` when only
    /// `--from-codex` is, and `.predatesCodex` when neither is.
    public static func codexPath(help: String) -> CodexLoginPath {
        let tokens = helpTokens(help)
        if tokens.contains("--codex") { return .inSheet }
        if tokens.contains("--from-codex") { return .terminal }
        return .predatesCodex
    }

    /// Read `tcr login --help` afresh and say where a Codex sign-in runs.
    ///
    /// Never answered from the cache, because a click decides from it: a `tcr` that
    /// gains `--codex` resolves `--account` as a row name first, so the sheet and the
    /// Terminal script are not interchangeable, and a stale answer would pick the
    /// wrong one. It refreshes the cache ``knownCodexSupport(executable:)`` reads. A
    /// failed run answers `.terminal`, as a `false` from ``probeCodex(executable:)``
    /// does.
    @MainActor
    public static func readCodexPath(executable: URL) -> CodexLoginPath {
        guard let help = try? loginHelp(executable: executable) else {
            codexCached[executable.path] = false
            return .terminal
        }
        let path = codexPath(help: help)
        codexCached[executable.path] = path == .inSheet
        return path
    }

    /// `tcr login --help`'s text, both streams; a non-zero exit is a failed run.
    private static func loginHelp(executable: URL) throws -> String {
        let output = try TcrTool.run(executable: executable, arguments: ["login", "--help"])
        guard output.exitCode == 0 else { throw HelpFailed() }
        return (String(data: output.stdout, encoding: .utf8) ?? "") + output.stderr
    }

    private struct HelpFailed: Error {}
}
