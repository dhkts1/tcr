import XCTest

@testable import TcrBarCore

/// Which sign-in a `tcr` offers for a Codex account, read from its `login --help`.
///
/// The question is asked of whole tokens, never of substrings: `--codex-home` contains
/// `--codex`, a looser probe for `codex` also matches `--from-codex`, and a `tcr` that has
/// only those two predates the in-app Codex sign-in.
final class LoginCapabilityTests: XCTestCase {

    /// `tcr login --help` as the last build before `--codex` prints it, embedded here so
    /// this test never reads `src/main.rs`: a later change adds `--codex` there, and the
    /// test is about the help of a `tcr` that predates it.
    static let helpBeforeCodexSignIn = #"""
        Authenticate a Claude account via the browser and add it to the config

        Usage: tcr login [OPTIONS]

        Options:
              --config <CONFIG>    Path to the config file (default: ~/.config/teamclaude.json)
              --force              Skip the refusal a running proxy would otherwise trigger, and write the login to the config file directly. Still probes the proxy first and still prefers its live account-add route when one is confirmed and safe — this only overrides the cases login would otherwise refuse on: an older proxy with no account-add route, or one that answered but not usably (wedged or timed out). Never overrides a confirmed live route (already safe, nothing to force) or a rejected api-key (proof the proxy is alive, so writing the file beside it is the worst-informed moment to do it) — both still refuse under --force. Unsafe when it takes the file path: the running server's next token refresh can overwrite what was just written
              --account <ACCOUNT>  Re-login a specific existing account. The identity that comes back must match, or nothing is written
              --token              Add an account from a `claude setup-token` credential instead of the browser flow — no value here. The token is read from stdin (prompted when stdin is a TTY), never from argv: an argv value is visible in `ps` and lands in shell history, both worse leaks than a stdin prompt. A setup-token credential carries only the `user:inference` scope, so there is no refresh token (the account serves until the token expires, about a year, then goes dead — see the warning `tcr login --token` prints) and usually no email (name it with `--name`, or answer the prompt). Refuses to combine with `--account`: an inference-only token carries no identity for that flag to confirm, and an assertion that cannot be evaluated must fail closed
              --from-claude-code   Add an account from the login the `claude` CLI on this machine already holds, instead of the browser flow — no value here. The credential is read from the login Keychain (item `Claude Code-credentials`) on macOS, else from `~/.claude/.credentials.json`; `TCR_CLAUDE_CODE_CREDENTIALS` overrides both with a file path. It carries a refresh token and a real expiry, so the account keeps working the same way a browser login's does. The cost, printed on import: refresh tokens are single-use, so the first time tcr refreshes this one, the `claude` CLI's own copy dies and `claude` asks for a browser login once. Refuses to combine with `--token`, which is a different credential from a different place. A first run does this by itself when it finds no accounts — this flag is the explicit redo (after `tcr remove`, or onto a second config)
              --from-codex         Import the login the `codex` CLI already holds (`$CODEX_HOME/auth.json`, default `~/.codex/auth.json`; `TCR_CODEX_AUTH` names a file to read instead) as a Codex account — no value here. The account is named by the login's email, or `<email>/codex` when another account holds that name; with `--account <email>`, the login must carry that email or nothing is written. The cost, printed on import: Codex refresh tokens are single-use too, so the first time tcr refreshes this one, `codex` asks you to log in again once. A second account: `mkdir -p <dir>` and `CODEX_HOME=<dir> codex login`, then this with `--codex-home <dir>`
              --codex-home <DIR>   Read `<DIR>/auth.json` instead of `$CODEX_HOME` / `~/.codex` (for a second login made with `mkdir -p <DIR>` and `CODEX_HOME=<DIR> codex login`)
              --non-interactive    Drive the login from another program instead of a terminal: stdin is never read, the browser is never opened here, and progress goes to stdout as one JSON object per line — `{"event":"browser","url":…}`, `{"event":"waiting"}`, `{"event":"saved","account":…}`, `{"event":"error","reason":…}`. The caller opens the URL; the loopback callback is then the only way the login can complete, and the same 2-minute timeout exits non-zero with the reason on one stderr line. Refuses to combine with `--token`, which reads the credential from stdin: that is the one input this mode has no way to supply
              --name <NAME>        Name this account explicitly, overriding the name login would mint for it. Refused if some other account already has that name — names are unique, and taking one from an existing row is how a login overwrites the wrong credential. `--token` needs it most (an inference-only credential's profile fetch usually comes back with no email to name it from), but the browser flow honours it too
          -h, --help               Print help
        """#

    @MainActor
    func testAFailedCodexHelpProbeNeverLaunchesATemporaryTerminalLogin() async {
        struct UnreadableHelp: Error {}
        let path = await LoginCapability.readCodexPath(
            executable: URL(fileURLWithPath: "/opt/example/tcr-unreadable-help"),
            readHelp: { _ in throw UnreadableHelp() })
        XCTAssertNotEqual(path, .terminal)
    }

    @MainActor
    func testVersionProbeRunsOffTheMainThread() async {
        let version = await LoginCapability.readVersion(
            executable: URL(fileURLWithPath: "/opt/example/tcr-version-thread"),
            readVersion: { _ in
                XCTAssertFalse(Thread.isMainThread)
                return "tcr 1.2.1"
            })
        XCTAssertEqual(version, "1.2.1")
    }

    @MainActor
    func testAllHelpProbesRunOffTheMainThread() async {
        let executable = URL(fileURLWithPath: "/opt/example/tcr-help-thread-test")
        let read: @Sendable (URL) throws -> String = { _ in
            XCTAssertFalse(Thread.isMainThread)
            return "--codex --non-interactive"
        }
        let nonInteractive = await LoginCapability.probe(executable: executable, readHelp: read)
        let codex = await LoginCapability.probeCodex(executable: executable, readHelp: read)
        let path = await LoginCapability.readCodexPath(executable: executable, readHelp: read)
        XCTAssertTrue(nonInteractive)
        XCTAssertEqual(codex, true)
        XCTAssertEqual(path, .inSheet)
    }

    /// One failed `tcr login --help` says nothing about the tcr: the help reads as before
    /// any probe, and the next probe asks again rather than repeating "predates Codex
    /// sign-in" for the rest of the app's run.
    @MainActor
    func testAFailedCodexProbeIsForgottenAndAskedAgain() async {
        struct TimedOut: Error {}
        let executable = URL(fileURLWithPath: "/opt/example/tcr-busy-mac")
        let attempts = HelpAttempts(failFirst: 1)
        let read: @Sendable (URL) throws -> String = { _ in try attempts.read() }
        let fixed: (URL) -> String? = { _ in "1@1" }

        let first = await LoginCapability.probeCodex(
            executable: executable, readHelp: read, fileIdentity: fixed)
        XCTAssertNil(first)
        XCTAssertNil(LoginCapability.knownCodexSupport(executable: executable))

        let second = await LoginCapability.probeCodex(
            executable: executable, readHelp: read, fileIdentity: fixed)
        XCTAssertEqual(second, true)
        XCTAssertEqual(LoginCapability.knownCodexSupport(executable: executable), true)
        XCTAssertEqual(attempts.count, 2)

        let failedClick = await LoginCapability.readCodexPath(
            executable: executable, readHelp: { _ in throw TimedOut() }, fileIdentity: fixed)
        XCTAssertEqual(failedClick, .probeFailed)
        XCTAssertNil(
            LoginCapability.knownCodexSupport(executable: executable),
            "a failed click forgets the answer rather than recording false")
    }

    /// A tcr updated in place keeps its path and changes its file, so its answer is asked
    /// again; the same file is answered from memory.
    @MainActor
    func testATcrUpdatedInPlaceIsAskedAgain() async {
        let executable = URL(fileURLWithPath: "/opt/example/tcr-updated-in-place")
        let attempts = HelpAttempts(failFirst: 0, help: Self.helpBeforeCodexSignIn)
        let read: @Sendable (URL) throws -> String = { _ in try attempts.read() }
        let old = await LoginCapability.probeCodex(
            executable: executable, readHelp: read, fileIdentity: { _ in "1@1" })
        XCTAssertEqual(old, false)
        _ = await LoginCapability.probeCodex(
            executable: executable, readHelp: read, fileIdentity: { _ in "1@1" })
        XCTAssertEqual(attempts.count, 1, "the same file is answered from memory")

        attempts.help = "      --codex  Sign in a Codex account\n"
        let updated = await LoginCapability.probeCodex(
            executable: executable, readHelp: read, fileIdentity: { _ in "2@2" })
        XCTAssertEqual(updated, true)
        XCTAssertEqual(attempts.count, 2)
    }

    /// `--codex` as a whole token is the in-app sign-in, wherever it sits on its line.
    func testTheWholeTokenCodexPasses() {
        XCTAssertTrue(
            LoginCapability.supportsCodex(
                help: "Usage: tcr login [OPTIONS]\n      --codex  Sign in a Codex account\n"))
        XCTAssertTrue(LoginCapability.supportsCodex(help: "      --codex=yes"))
    }

    /// `--codex-home` contains `--codex` and is not it.
    func testCodexHomeAloneFails() {
        XCTAssertFalse(
            LoginCapability.supportsCodex(help: "      --codex-home <DIR>  Read <DIR>/auth.json\n"))
    }

    /// `--from-codex` ends in `codex` and is the import, not the sign-in.
    func testFromCodexAloneFails() {
        XCTAssertFalse(LoginCapability.supportsCodex(help: "      --from-codex  Import the login\n"))
    }

    /// The help of the last `tcr` before the in-app sign-in names `--codex-home` and
    /// `--from-codex` and not `--codex`. The positive control says the literal is a real
    /// help text: it names `--non-interactive`.
    func testTodaysHelpTextFails() {
        XCTAssertFalse(LoginCapability.supportsCodex(help: Self.helpBeforeCodexSignIn))
        XCTAssertTrue(LoginCapability.supportsNonInteractive(help: Self.helpBeforeCodexSignIn))
    }

    /// The sheet with `--codex`, the two-command Terminal script with `--from-codex` only,
    /// and nothing at all from a `tcr` with neither.
    func testTheCodexPathFollowsTheWholeTokens() {
        XCTAssertEqual(LoginCapability.codexPath(help: Self.helpBeforeCodexSignIn), .terminal)
        XCTAssertEqual(
            LoginCapability.codexPath(help: "      --non-interactive  Drive the login\n"),
            .predatesCodex)
        XCTAssertEqual(
            LoginCapability.codexPath(
                help: "      --codex  Sign in a Codex account\n      --from-codex  Import the login\n"),
            .inSheet)
    }
}

/// A stand-in `tcr login --help`: fails its first `failFirst` runs, then prints `help`.
private final class HelpAttempts: @unchecked Sendable {
    struct TimedOut: Error {}
    private let lock = NSLock()
    private var runs = 0
    private var failures: Int
    private var text: String

    init(failFirst: Int, help: String = "      --codex  Sign in a Codex account\n") {
        failures = failFirst
        text = help
    }

    var count: Int { lock.withLock { runs } }
    var help: String {
        get { lock.withLock { text } }
        set { lock.withLock { text = newValue } }
    }

    func read() throws -> String {
        try lock.withLock {
            runs += 1
            if failures > 0 {
                failures -= 1
                throw TimedOut()
            }
            return text
        }
    }
}
