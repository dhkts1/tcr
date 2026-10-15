import Foundation
import XCTest

@testable import TcrBarCore

/// `ServerController.State.startDisabledReason`, the plain sentence for a
/// disabled "Start server" control. A colleague's "Start server" was
/// disabled with no reason at all; this pins what the reason says once a
/// non-proxy holder is the cause, and that every other state stays silent.
final class StartServerReasonTests: XCTestCase {

    private let em = "\u{2014}"

    /// The real shape `takeover_port` (`src/singleton.rs`) prints ahead of the
    /// bind failure: the non-proxy holder line, then the failed-bind line
    /// `ServerController.classifyExit` matches on to reach `.incumbentHoldsPort`
    /// in the first place.
    private func nonProxyMessage(port: Int = 3456, pid: Int = 4242, cmd: String) -> String {
        "[tcr] :\(port) is held by a non-proxy process (pid \(pid)): \(cmd) \(em) "
            + "not replacing it; the bind will fail if it stays.\n"
            + "failed to bind 127.0.0.1:\(port): Address already in use (os error 48)"
    }

    func testNamesTheHolderAndThePort() {
        let state = ServerController.State.incumbentHoldsPort(
            message: nonProxyMessage(cmd: "OtherGatewayServer"))
        XCTAssertEqual(
            state.startDisabledReason,
            "Port 3456 is held by OtherGatewayServer. Take over stops it.")
    }

    /// The common, benign case: another `tcr` already holds the port. No
    /// non-proxy line was ever printed, so there is nothing to name and
    /// starting again is not futile: it just reports "already running".
    func testNilWhenAnotherTcrHoldsThePort() {
        let state = ServerController.State.incumbentHoldsPort(
            message: "[tcr] another proxy holds :3456 (pid 55) and it is still listening")
        XCTAssertNil(state.startDisabledReason)
    }

    func testNilWhenSupervisingOurOwnChild() {
        XCTAssertNil(ServerController.State.supervising(pid: 1).startDisabledReason)
    }

    func testNilWhenTheHolderAnsweredNothing() {
        XCTAssertNil(
            ServerController.State.incumbentNotAnswering(message: "no answer").startDisabledReason)
    }

    func testNilWhenIdle() {
        XCTAssertNil(ServerController.State.idle.startDisabledReason)
    }

    /// What a `tcr` built before Codex accounts prints, and exits 1 on, when the config
    /// it loads holds a Codex account: the config loader refuses a credential shape newer
    /// than it understands. The account name is fake.
    private let refusal =
        "Error: account \"bob@example.com/codex\" carries credentialVersion 2, which this build "
        + "only understands up to 1; refusing to load rather than silently reinterpret its "
        + "credential shape"

    func testAFutureCredentialShapeDoesNotBlameCodex() {
        let refusal =
            "account alice@example.com carries credentialVersion 3, which this build only understands up to 2"
        let displayed = PredatesCodexRefusal.displayed(refusal, executablePath: "/opt/example/tcr")
        XCTAssertFalse(displayed.contains("Codex"))
        XCTAssertTrue(displayed.contains("credentials"))
        XCTAssertTrue(displayed.contains("/opt/example/tcr"))
    }

    func testTheCredentialRefusalNamesTheExecutableThatActuallyRan() {
        let path = "/opt/example/tcr"
        XCTAssertTrue(PredatesCodexRefusal.displayed(refusal, executablePath: path).contains(path))
        XCTAssertTrue(CodexSignInCopy.predatesCodexDetail(executablePath: path).contains(path))
        XCTAssertTrue(
            ServerController.State.exited(exitCode: 1, message: refusal)
                .summary(executablePath: path).contains(path))
        let state = StatusPoller.classify(
            TcrTool.Output(exitCode: 1, stdout: Data(), stderr: refusal), executablePath: path)
        XCTAssertTrue(state.summary.contains(path))
        XCTAssertFalse(state.summary.contains("Update TcrBar"))
    }

    /// The server's exit, and any other single line carrying that refusal, says what is
    /// wrong in words a person can act on: the tcr inside TcrBar is too old for this
    /// config. Anything else keeps tcr's own words.
    func testTheCredentialVersionRefusalMapsToTheBundledTcrNotice() throws {
        XCTAssertTrue(PredatesCodexRefusal.matches(refusal))
        XCTAssertFalse(PredatesCodexRefusal.matches("connection refused"))

        let state = ServerController.classifyExit(exitCode: 1, stderr: refusal + "\n")
        XCTAssertEqual(state, .exited(exitCode: 1, message: refusal))
        XCTAssertEqual(state.summary, CodexSignInCopy.predatesCodexSentence)

        XCTAssertEqual(
            PredatesCodexRefusal.displayed("tcr status failed: " + refusal),
            CodexSignInCopy.predatesCodexSentence)
        XCTAssertEqual(PredatesCodexRefusal.displayed("connection refused"), "connection refused")

        let loader = try String(
            contentsOf: repoRoot().appendingPathComponent("src/config.rs"), encoding: .utf8)
        for words in [
            "carries credentialVersion {v}, which this build", "only understands up to {understood}",
        ] {
            XCTAssertTrue(
                loader.contains(words),
                "a change to tcr reworded the config loader's refusal of a newer credential "
                    + "shape, which TcrBar matches to say the tcr inside it predates Codex "
                    + "accounts; it now lacks: \(words)")
        }
    }

    /// A row's failure lines carry the same sentence, once: a tcr that predates Codex
    /// accounts refuses every command with the same words, and three equal lines would
    /// say one thing three times. A note on a success is left as it was.
    func testARowNoticeNamesTheBundledTcrThatPredatesCodex() {
        XCTAssertEqual(
            AccountRowNotice.collect(
                deleteFailure: RemoveAccountCommand.Failure(exitCode: 1, message: refusal),
                deleteNotice: nil,
                groupFailure: (
                    group: "orchard", failure: GroupCommand.Failure(exitCode: 1, message: refusal)
                ),
                toggleFailure: AccountCommand.Failure(enabling: false, exitCode: 1, message: refusal)),
            [AccountRowNotice(text: CodexSignInCopy.predatesCodexSentence, tone: .failure)])
        XCTAssertEqual(
            AccountRowNotice.collect(
                deleteFailure: nil, deleteNotice: "keeps listing it until it restarts",
                groupFailure: (
                    group: "red", failure: GroupCommand.Failure(exitCode: 2, message: refusal)
                ),
                toggleFailure: nil),
            [
                AccountRowNotice(text: CodexSignInCopy.predatesCodexSentence, tone: .failure),
                AccountRowNotice(text: "keeps listing it until it restarts", tone: .note),
            ])
    }

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
    }
}
