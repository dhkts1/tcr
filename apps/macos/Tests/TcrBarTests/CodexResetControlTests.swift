import Foundation
import XCTest

@testable import TcrBarCore

final class CodexResetControlTests: XCTestCase {
    private let alice = AccountRef(name: "alice@example.com")
    private let bob = AccountRef(name: "bob@example.com")

    @MainActor
    func testCancelAndRetryWithoutAnAttemptRunNothing() async {
        let calls = ResetCalls()
        let controller = CodexResetController(runner: calls.run)
        let cancelled = await controller.redeem(account: alice, confirmed: false)
        let retry = await controller.retry(account: alice)
        XCTAssertEqual(cancelled, .skipped)
        XCTAssertEqual(retry, .skipped)
        XCTAssertEqual(calls.count, 0)
        XCTAssertTrue(controller.pending.isEmpty)
        XCTAssertNil(controller.outcome(for: alice))
    }

    @MainActor
    func testExplicitRetryKeepsTheAttemptAndNewConsentCreatesAnother() async {
        let calls = ResetCalls(output: .init(exitCode: 4, stdout: Data(), stderr: "Reset outcome unknown.\n"))
        let controller = CodexResetController(runner: calls.run)
        _ = await controller.redeem(account: alice, confirmed: true)
        XCTAssertEqual(calls.count, 1)
        _ = await controller.retry(account: alice)
        _ = await controller.redeem(account: alice, confirmed: false)
        XCTAssertEqual(calls.count, 2)
        _ = await controller.redeem(account: bob, confirmed: true)
        _ = await controller.redeem(account: alice, confirmed: true)
        XCTAssertEqual(calls.count, 4)
        XCTAssertTrue(calls.sameAttempt(0, 1))
        XCTAssertFalse(calls.sameAttempt(0, 2))
        XCTAssertFalse(calls.sameAttempt(0, 3))
        XCTAssertTrue(calls.validCommands(names: [alice.name, alice.name, bob.name, alice.name]))
        XCTAssertTrue(calls.allOffMain)
    }

    @MainActor
    func testPendingClickAndRetryCannotRunAnotherCommand() async {
        let started = expectation(description: "fake runner entered")
        let release = DispatchSemaphore(value: 0)
        let calls = ResetCalls(started: started, release: release)
        let controller = CodexResetController(runner: calls.run)
        let first = Task { await controller.redeem(account: alice, confirmed: true) }
        await fulfillment(of: [started], timeout: 5)
        XCTAssertTrue(controller.isPending(alice))
        XCTAssertFalse(controller.isPending(bob))
        let duplicate = await controller.redeem(account: alice, confirmed: true)
        let retry = await controller.retry(account: alice)
        XCTAssertEqual(duplicate, .skipped)
        XCTAssertEqual(retry, .skipped)
        XCTAssertEqual(calls.count, 1)
        release.signal()
        _ = await first.value
        XCTAssertFalse(controller.isPending(alice))
        XCTAssertNotNil(controller.outcome(for: alice))
        XCTAssertNil(controller.outcome(for: bob))
    }

    @MainActor
    func testEveryCommandOutcomeAndWarningIsKeptVerbatim() async {
        for (code, stdout, stderr) in [
            (Int32(0), "Applied.\n", ""),
            (0, "Nothing to reset.\n", ""),
            (6, "No reset credit available.\n", ""),
            (0, "Already redeemed.\n", ""),
            (0, "Applied.\n", "Usage refresh failed.\n"),
            (5, "Applied.\n", "Usage refresh failed.\n"),
            (3, "", "No running proxy.\n"),
            (4, "", "Reset failed.\n"),
            (4, "", "Reset outcome unknown.\n"),
        ] {
            let calls = ResetCalls(output: .init(exitCode: code, stdout: Data(stdout.utf8), stderr: stderr))
            let controller = CodexResetController(runner: calls.run)
            let attempt = await controller.redeem(account: alice, confirmed: true)
            let outcome = CodexResetCommand.Outcome(exitCode: code, stdout: stdout, stderr: stderr)
            XCTAssertEqual(attempt, .finished(outcome))
            XCTAssertEqual(controller.outcome(for: alice), outcome)
            XCTAssertEqual(
                outcome.text,
                [stdout, stderr].map {
                    $0.trimmingCharacters(in: .whitespacesAndNewlines)
                }.filter { !$0.isEmpty }.joined(separator: "\n"))
            XCTAssertEqual(calls.count, 1)
        }
    }

    /// The result dialog shows `outcome.text`. The fake answers as the real CLI does: it
    /// asks its "[y/N]" question unless it was run with `--yes`, so a controller that
    /// runs the interactive form puts a question in the dialog nobody can answer there.
    @MainActor
    func testResultTextIsTheOutcomeAndNeverTheCommandsQuestion() async {
        let calls = ResetCalls()
        let controller = CodexResetController(runner: calls.run)
        _ = await controller.redeem(account: alice, confirmed: true)
        let text = controller.outcome(for: alice)?.text
        XCTAssertEqual(text, "Applied.")
        XCTAssertFalse(text?.contains("[y/N]") ?? true, "the result dialog must not ask a question")
    }

    @MainActor
    func testRunnerFailureClearsPendingAndKeepsRetry() async {
        let controller = CodexResetController(runner: { _, _ in throw FakeResetFailure() })
        _ = await controller.redeem(account: alice, confirmed: true)
        XCTAssertFalse(controller.isPending(alice))
        let outcome = controller.outcome(for: alice)
        XCTAssertEqual(outcome?.stderr, "Reset failed.")
        XCTAssertFalse(outcome?.text.contains("fake command unavailable") ?? true)
        XCTAssertEqual(outcome?.exitCode, -1)
        guard case .finished = await controller.retry(account: alice) else {
            return XCTFail("an explicit retry must retain a failed attempt")
        }
    }
}

private struct FakeResetFailure: LocalizedError {
    var errorDescription: String? { "fake command unavailable" }
}

/// Only this fake sees the generated attempts. Assertions report booleans, never keys.
private final class ResetCalls: @unchecked Sendable {
    private let lock = NSLock()
    private var commands: [([String], String, Bool)] = []
    private let output: TcrTool.Output
    private let started: XCTestExpectation?
    private let release: DispatchSemaphore?

    init(
        output: TcrTool.Output = .init(exitCode: 0, stdout: Data("Applied.\n".utf8), stderr: ""),
        started: XCTestExpectation? = nil, release: DispatchSemaphore? = nil
    ) {
        self.output = output
        self.started = started
        self.release = release
    }

    /// Answers as `tcr codex reset` does: without `--yes` it asks "Redeem one reset for
    /// <account>? [y/N]" before its outcome, and that question reaches the caller's output.
    func run(_ arguments: [String], _ stdin: String) throws -> TcrTool.Output {
        lock.lock()
        commands.append((arguments, stdin, Thread.isMainThread))
        lock.unlock()
        started?.fulfill()
        if let release { _ = release.wait(timeout: .now() + 10) }
        guard !arguments.contains("--yes"), arguments.count > 2 else { return output }
        let question = "Redeem one reset for \(arguments[2])? [y/N]\n"
        return .init(
            exitCode: output.exitCode, stdout: output.stdout, stderr: question + output.stderr)
    }

    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return commands.count
    }

    var allOffMain: Bool {
        lock.lock()
        defer { lock.unlock() }
        return commands.allSatisfy { !$0.2 }
    }

    func sameAttempt(_ a: Int, _ b: Int) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard commands.indices.contains(a), commands.indices.contains(b),
            commands[a].0.count == 6, commands[b].0.count == 6
        else { return false }
        return commands[a].0[4] == commands[b].0[4]
    }

    func validCommands(names: [String]) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard commands.count == names.count else { return false }
        return zip(commands, names).allSatisfy { command, name in
            let (args, stdin, _) = command
            return args.count == 6 && Array(args.prefix(4)) == ["codex", "reset", name, "--idempotency-key"]
                && UUID(uuidString: args[4]) != nil && args[5] == "--yes" && stdin.isEmpty
        }
    }
}
