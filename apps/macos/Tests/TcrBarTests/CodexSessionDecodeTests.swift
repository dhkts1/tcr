import XCTest

@testable import TcrBarCore

/// The Codex keys of `tcr sessions --json`, read from the canonical sessions fixture: a Claude
/// session beside a Codex one that has an `exec_command` running. A running call's timeout, which
/// no call in that fixture carries, is read from an inline payload.
final class CodexSessionDecodeTests: XCTestCase {

    func testCanonicalSessionsDecode() throws {
        let read = try canonicalRead()

        XCTAssertEqual(read.channel, .live)
        XCTAssertEqual(read.unreadable, [])
        XCTAssertEqual(
            read.sessions.map(\.sessionId),
            ["a1f3c2d4-0000-4000-8000-000000000001", "019a3c5e-0000-7000-8000-000000000002"])
        XCTAssertEqual(read.sessions.map(\.provider), [.anthropic, .codex])
    }

    func testACodexSessionCarriesProviderUnpricedAndReasoning() throws {
        let bob = try session(1)

        XCTAssertEqual(bob.provider, .codex)
        XCTAssertEqual(bob.unpricedRequests, 7)
        XCTAssertEqual(bob.reasoningTokens, 1_200)
        XCTAssertEqual(bob.account, "bob@example.com/codex")
        XCTAssertEqual(bob.model, "gpt-5.1-codex")
        // The wire keeps its 0.0-for-unpriced meaning, and nothing here reinterprets it.
        XCTAssertEqual(bob.costUsd, 0.0)
        XCTAssertEqual(bob.tools.byTool.map(\.tool), ["exec_command", "apply_patch"])
    }

    /// The canonical Codex call runs under codex's default tool set, whose `exec_command` is the
    /// interactive one and takes no timeout, so the call carries none. A Codex call carries a
    /// timeout only when codex runs its one-shot `exec_command`, which managed requirements select.
    func testTheCanonicalCodexCallCarriesNoTimeout() throws {
        let running = try session(1).tools.running

        XCTAssertEqual(
            running,
            [
                ToolCall(
                    tool: "exec_command", commandHead: "cargo build", commandClass: "build",
                    startedMs: 1_767_225_555_000)
            ])
        XCTAssertNil(running.first?.timeoutSeconds)
    }

    /// Where a call does carry a timeout, it reaches the field: a one-shot `exec_command` decodes
    /// its 120 seconds, and a Claude `Bash` call beside it decodes none. Inline, because the
    /// canonical fixture runs codex's default tool set, under which no Codex call carries one.
    /// Each session carries only the keys a row needs to decode, plus its running call.
    func testAOneShotCodexCallCarriesItsTimeoutAndAClaudeCallNone() throws {
        let payload = """
            {"supported": true, "sessions": [
              {"sessionId": "a1f3c2d4-0000-4000-8000-000000000003",
               "account": "alice@example.com",
               "firstSeenMs": 1767223200000, "lastSeenMs": 1767225597000,
               "tools": {"running": [
                 {"tool": "Bash", "commandHead": "cargo test", "commandClass": "build",
                  "startedMs": 1767225580000}]}},
              {"sessionId": "019a3c5e-0000-7000-8000-000000000004",
               "account": "bob@example.com/codex", "provider": "codex",
               "firstSeenMs": 1767225425000, "lastSeenMs": 1767225550000,
               "tools": {"running": [
                 {"tool": "exec_command", "commandHead": "cargo build", "commandClass": "build",
                  "startedMs": 1767225555000, "timeoutSeconds": 120}]}}]}
            """

        let read = try Fleet.decodeSessions(Data(payload.utf8))

        XCTAssertEqual(read.unreadable, [])
        XCTAssertEqual(read.sessions.map(\.provider), [.anthropic, .codex])
        let running = read.sessions.flatMap { $0.tools.running }
        XCTAssertEqual(running.map(\.tool), ["Bash", "exec_command"])
        XCTAssertEqual(running.map(\.timeoutSeconds), [nil, 120])
    }

    /// A Claude session from a server that knows Codex reads exactly as it did before.
    func testAClaudeSessionDecodesAsToday() throws {
        let alice = try session(0)

        XCTAssertEqual(alice.provider, .anthropic)
        XCTAssertEqual(alice.unpricedRequests, 0)
        XCTAssertEqual(alice.reasoningTokens, 0)
        XCTAssertEqual(alice.account, "alice@example.com")
        XCTAssertEqual(alice.model, "claude-opus-5")
        XCTAssertEqual(alice.firstSeenMs, 1_767_223_200_000)
        XCTAssertEqual(alice.lastSeenMs, 1_767_225_597_000)
        XCTAssertEqual(alice.requests, 24)
        XCTAssertEqual(alice.inputTokens, 180_000)
        XCTAssertEqual(alice.outputTokens, 9_200)
        XCTAssertEqual(alice.cacheReadTokens, 160_000)
        XCTAssertEqual(alice.costUsd, 1.2034)
        XCTAssertEqual(alice.cacheTtlSecs, 3_600)
        let perMinute: [UInt16] = Array(repeating: 0, count: 22) + [1, 2, 3, 2, 4, 3, 5, 0]
        XCTAssertEqual(alice.reqPerMinute, perMinute)
        XCTAssertEqual(
            alice.tools,
            SessionTools(
                calls: 12, errors: 0, timeouts: 0,
                running: [
                    ToolCall(
                        tool: "Bash", commandHead: "cargo test", commandClass: "build",
                        startedMs: 1_767_225_580_000)
                ],
                slowest: [
                    ToolCall(
                        tool: "Bash", commandHead: "cargo build --release", commandClass: "build",
                        endedMs: 1_767_225_305_000, seconds: 12.0),
                    ToolCall(
                        tool: "Bash", commandHead: "cargo build", commandClass: "build",
                        endedMs: 1_767_225_370_000, seconds: 6.0),
                    ToolCall(
                        tool: "Bash", commandHead: "rg -n TODO", commandClass: "search",
                        endedMs: 1_767_225_425_000, seconds: 3.0),
                    ToolCall(
                        tool: "Bash", commandHead: "cargo test -p example", commandClass: "build",
                        endedMs: 1_767_225_350_000, seconds: 2.5),
                    ToolCall(
                        tool: "Bash", commandHead: "git status", commandClass: "git-local",
                        endedMs: 1_767_225_440_000, seconds: 2.5),
                    ToolCall(
                        tool: "Bash", commandHead: "git status", commandClass: "git-local",
                        endedMs: 1_767_225_280_000, seconds: 2.0),
                    ToolCall(
                        tool: "Bash", commandHead: "cargo test", commandClass: "build",
                        endedMs: 1_767_225_490_000, seconds: 2.0),
                    ToolCall(
                        tool: "Bash", commandHead: "git status", commandClass: "git-local",
                        endedMs: 1_767_225_475_000, seconds: 1.5),
                    ToolCall(
                        tool: "Read",
                        commandHead: "Read /Users/example/git/tcr/src/session_wire.rs",
                        endedMs: 1_767_225_320_000, seconds: 0.4),
                    ToolCall(
                        tool: "Read", commandHead: "Read /Users/example/git/tcr/src/proxy.rs",
                        endedMs: 1_767_225_250_000, seconds: 0.1),
                ],
                overOneMinute: 0,
                byTool: [
                    ToolBucketRow(tool: "Bash", calls: 8, secondsP50: 2.5),
                    ToolBucketRow(tool: "Read", calls: 4, secondsP50: 0.1),
                ],
                timeoutsByClass: [:], timedOut: []))
    }

    /// A session follows the row rule: a provider this build cannot name costs that session's
    /// row, with the same reason, and only that row.
    func testASessionOfAnUnknownProviderIsUnreadable() throws {
        let fixture = try CanonicalFixtures.data("codex-sessions.json")
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: fixture) as? [String: Any])
        var sessions = try XCTUnwrap(object["sessions"] as? [[String: Any]])
        sessions[1]["provider"] = "gemini"
        object["sessions"] = sessions

        let read = try Fleet.decodeSessions(JSONSerialization.data(withJSONObject: object))

        XCTAssertEqual(read.sessions.map(\.sessionId), ["a1f3c2d4-0000-4000-8000-000000000001"])
        XCTAssertEqual(
            read.unreadable,
            [Fleet.UnreadableRow(index: 1, message: "provider 'gemini' is newer than this TcrBar")])
    }

    // MARK: - Helpers

    private func canonicalRead() throws -> Fleet.SessionsRead {
        try Fleet.decodeSessions(CanonicalFixtures.data("codex-sessions.json"))
    }

    /// Alice's session is 0, bob's is 1.
    private func session(_ index: Int) throws -> Session {
        let sessions = try canonicalRead().sessions
        return try XCTUnwrap(
            sessions.indices.contains(index) ? sessions[index] : nil, "no session \(index)")
    }
}
