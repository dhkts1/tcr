import Combine
import Foundation

/// Which way a typed `claude` goes through tcr, as `tcr setup status --json`
/// reports it in its `route` key (`src/setup.rs`, `Route`).
public enum ClaudeRoute: String, Decodable, Equatable, Sendable, CaseIterable {
    /// Neither route is installed.
    case off
    /// `tcr setup shell`: a `claude` function in the shell's startup file.
    case shell
    /// `tcr setup claude-settings`: `ANTHROPIC_BASE_URL` in
    /// `~/.claude/settings.json`, so every launch routes.
    case everyLaunch
    /// Both are installed.
    case both

    /// The Settings row's words for it.
    public var label: String {
        switch self {
        case .off: return "Off"
        case .shell: return "In my shell"
        case .everyLaunch: return "Every launch"
        case .both: return "Both"
        }
    }
}

/// One thing that already decides what `claude` does, as `tcr` found it.
public struct ClaudeRouteConflict: Decodable, Equatable, Sendable {
    public let route: String
    public let kind: String
    public let path: String
    public let line: Int?
    public let text: String

    public init(route: String, kind: String, path: String, line: Int?, text: String) {
        self.route = route
        self.kind = kind
        self.path = path
        self.line = line
        self.text = text
    }

    /// `file:line: text`, the same shape `tcr` prints, with the home
    /// directory shortened so the line fits a caption.
    public func summary(home: String = NSHomeDirectory()) -> String {
        let shortPath =
            path.hasPrefix(home + "/") ? "~" + path.dropFirst(home.count) : Substring(path)
        guard let line else { return "\(shortPath): \(text)" }
        return "\(shortPath):\(line): \(text)"
    }
}

/// `tcr setup status --json`, the fields this app reads. Every other key is
/// ignored, so a newer `tcr` that adds one does not break the decode.
public struct ClaudeRouteStatus: Decodable, Equatable, Sendable {
    public let route: ClaudeRoute
    public let conflicts: [ClaudeRouteConflict]
    public let port: Int

    public init(route: ClaudeRoute, conflicts: [ClaudeRouteConflict] = [], port: Int = 3456) {
        self.route = route
        self.conflicts = conflicts
        self.port = port
    }
}

/// The argv this app hands `tcr` for the routes, and the one rule for
/// reading what came back. This app never edits a startup file or
/// `settings.json` itself: every change is a `tcr setup` subprocess.
public enum ClaudeRouteCommand {
    public static let statusArguments = ["setup", "status", "--json"]
    public static let installShell = ["setup", "shell"]
    public static let removeShell = ["setup", "shell", "--remove"]
    public static let installSettings = ["setup", "claude-settings"]
    public static let removeSettings = ["setup", "claude-settings", "--remove"]

    /// What one status read established. "Could not ask" is kept apart from
    /// every answer: an older `tcr` with no `setup` verb must never read as
    /// "nothing installed", which would put the prompt in front of a person
    /// whose answer this build cannot carry out.
    public enum Reading: Equatable, Sendable {
        case status(ClaudeRouteStatus)
        case unavailable(reason: String)
    }

    public static func classifyStatus(exitCode: Int32, stdout: Data, stderr: String) -> Reading {
        guard exitCode == 0 else {
            let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            return .unavailable(reason: text.isEmpty ? "exit \(exitCode)" : text)
        }
        do {
            return .status(try JSONDecoder().decode(ClaudeRouteStatus.self, from: stdout))
        } catch {
            return .unavailable(reason: "tcr setup status --json did not decode: \(error)")
        }
    }

    /// The invocations that move the machine from `current` to `target`,
    /// in order: install the new route first, then take the old one out, so a
    /// refused install leaves the route that was working in place.
    public static func steps(from current: ClaudeRoute, to target: ClaudeRoute) -> [[String]] {
        let hasShell = current == .shell || current == .both
        let hasSettings = current == .everyLaunch || current == .both
        let wantShell = target == .shell || target == .both
        let wantSettings = target == .everyLaunch || target == .both
        var out: [[String]] = []
        if wantShell && !hasShell { out.append(installShell) }
        if wantSettings && !hasSettings { out.append(installSettings) }
        if hasShell && !wantShell { out.append(removeShell) }
        if hasSettings && !wantSettings { out.append(removeSettings) }
        return out
    }

    /// `tcr`'s own words for a refusal: the first line of its stderr with the
    /// `Error: ` prefix anyhow adds taken off, and every conflict line after
    /// it kept.
    public static func failureText(exitCode: Int32, stderr: String) -> String {
        var text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        if text.hasPrefix("Error: ") { text.removeFirst("Error: ".count) }
        return text.isEmpty ? "tcr exited \(exitCode)" : text
    }

    /// Blocking status read, always off the main actor.
    nonisolated static func performStatus() -> Reading {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .unavailable(
                reason: "tcr not found (searched \(notFound.searched.count) locations)")
        case .success(let executable):
            do {
                let output = try TcrTool.run(executable: executable, arguments: statusArguments)
                return classifyStatus(
                    exitCode: output.exitCode, stdout: output.stdout, stderr: output.stderr)
            } catch {
                return .unavailable(reason: error.localizedDescription)
            }
        }
    }

    /// Blocking: run each step in order, stopping at the first refusal.
    /// `nil` when every step exited 0, otherwise `tcr`'s reason.
    nonisolated static func performSteps(_ steps: [[String]]) -> String? {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return "tcr not found (searched \(notFound.searched.count) locations)"
        case .success(let executable):
            for arguments in steps {
                do {
                    let output = try TcrTool.run(executable: executable, arguments: arguments)
                    if output.exitCode != 0 {
                        return failureText(exitCode: output.exitCode, stderr: output.stderr)
                    }
                } catch {
                    return error.localizedDescription
                }
            }
            return nil
        }
    }
}

/// The Accounts tab's one-time question, "Send `claude` through tcr?".
///
/// # It never moves anything on screen
///
/// Gil, 2026-10-14: nothing on the panel may move because data changed. So
/// whether the prompt shows is decided ONCE per opening of the panel, before
/// its first frame, from the status read cached by the previous one
/// (``ClaudeRouteController/decideAtOpen(hasFleet:)``), and it then stays
/// for as long as the panel is open. Answering it swaps the buttons for an
/// outcome line drawn in the same space at the same height
/// (``ClaudeRoutePromptView``), never removes the row.
public enum ClaudeRoutePrompt {
    /// The `UserDefaults` key that remembers an answer. Any answer, "Not now"
    /// included: the prompt asks once, and Settings > General is where the
    /// choice is changed afterwards.
    public static let answeredKey = "claudeRoutePromptAnswered"

    /// The three answers the prompt offers.
    public enum Answer: Equatable, Sendable {
        case shell
        case everyLaunch
        case notNow
    }

    /// Show the prompt? Only with a fleet to route to, never after an
    /// answer, and only on a status read that says neither route is
    /// installed and nothing is in the way. An unreadable status shows
    /// nothing, and so does a conflict: the prompt would offer a choice
    /// `tcr` is going to refuse.
    public static func shouldShow(
        hasFleet: Bool, reading: ClaudeRouteCommand.Reading?, answered: Bool
    ) -> Bool {
        guard hasFleet, !answered, case .status(let status) = reading else { return false }
        return status.route == .off && status.conflicts.isEmpty
    }

    /// The line that takes the buttons' place once answered.
    public static func outcome(for answer: Answer, failure: String?) -> String {
        if let failure { return "Not set up: \(failure)" }
        switch answer {
        case .shell:
            return "Done. claude typed in a new terminal now goes through tcr. Change it in Settings."
        case .everyLaunch:
            return "Done. Every claude launch now goes through tcr. Change it in Settings."
        case .notNow:
            return "Not asking again. Settings > General can turn it on."
        }
    }

    /// The confirm "Every launch…" opens, stating the cost.
    public static let everyLaunchCost =
        "Every claude process, from any app, will talk to tcr. While tcr is not running, "
        + "claude cannot reach Anthropic until tcr starts or this is turned off in Settings."
}

/// Panel and Settings state for the claude route: the last status read, the
/// prompt's once-per-open decision, and the outcome of the last change.
@MainActor
public final class ClaudeRouteController: ObservableObject {
    /// The last `tcr setup status --json`, `nil` before the first read.
    @Published public private(set) var reading: ClaudeRouteCommand.Reading?
    /// Whether this opening of the panel draws the prompt. Written only by
    /// ``decideAtOpen(hasFleet:)``, so a status read landing while the panel
    /// is open cannot add or take away the row.
    @Published public private(set) var promptShown: Bool
    /// The line drawn in the prompt's place after an answer, `nil` before.
    @Published public private(set) var promptOutcome: String?
    @Published public private(set) var pending = false
    /// Why the last Settings change did not happen, in `tcr`'s words.
    @Published public private(set) var failure: String?

    private let defaults: UserDefaults
    /// The harness never spawns `tcr` and never writes `UserDefaults`.
    private let inert: Bool

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        self.inert = false
        self.promptShown = false
    }

    /// A controller pinned to one state, for the render harness and tests.
    /// Nothing it does runs a subprocess or writes a default.
    public init(
        pinned reading: ClaudeRouteCommand.Reading?,
        promptShown: Bool = false,
        promptOutcome: String? = nil,
        defaults: UserDefaults = UserDefaults(suiteName: "tcrbar.harness.claude-route") ?? .standard
    ) {
        self.defaults = defaults
        self.inert = true
        self.reading = reading
        self.promptShown = promptShown
        self.promptOutcome = promptOutcome
    }

    public var answered: Bool { defaults.bool(forKey: ClaudeRoutePrompt.answeredKey) }

    /// The route the last read reported, `nil` when there is none.
    public var route: ClaudeRoute? {
        if case .status(let status) = reading { return status.route }
        return nil
    }

    /// Decide, before the panel's first frame, whether this opening draws the
    /// prompt. Called by the shell right before the popover shows; nothing
    /// else writes ``promptShown``.
    public func decideAtOpen(hasFleet: Bool) {
        promptOutcome = nil
        promptShown = ClaudeRoutePrompt.shouldShow(
            hasFleet: hasFleet, reading: reading, answered: answered)
    }

    /// Re-read `tcr setup status --json`. Changes what the NEXT opening
    /// decides and what Settings shows, never the prompt already on screen.
    public func refresh() async {
        guard !inert else { return }
        reading = await Task.detached(priority: .utility) {
            ClaudeRouteCommand.performStatus()
        }.value
    }

    /// The prompt's answer. Remembers that it was asked whatever the answer,
    /// runs the install when there is one, and fills ``promptOutcome``.
    public func answerPrompt(_ answer: ClaudeRoutePrompt.Answer) async {
        guard !pending else { return }
        defaults.set(true, forKey: ClaudeRoutePrompt.answeredKey)
        let target: ClaudeRoute?
        switch answer {
        case .shell: target = .shell
        case .everyLaunch: target = .everyLaunch
        case .notNow: target = nil
        }
        var failure: String?
        if let target {
            failure = await run(ClaudeRouteCommand.steps(from: route ?? .off, to: target))
            await refresh()
        }
        promptOutcome = ClaudeRoutePrompt.outcome(for: answer, failure: failure)
    }

    /// Settings > General: move to `target`. `true` when every step took.
    @discardableResult
    public func choose(_ target: ClaudeRoute) async -> Bool {
        guard !pending, let current = route else { return false }
        failure = await run(ClaudeRouteCommand.steps(from: current, to: target))
        await refresh()
        return failure == nil
    }

    private func run(_ steps: [[String]]) async -> String? {
        guard !steps.isEmpty else { return nil }
        guard !inert else { return nil }
        pending = true
        defer { pending = false }
        return await Task.detached(priority: .userInitiated) {
            ClaudeRouteCommand.performSteps(steps)
        }.value
    }
}
