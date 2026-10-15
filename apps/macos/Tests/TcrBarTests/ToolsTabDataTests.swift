import XCTest

@testable import TcrBarCore

/// What the Tools tab draws, minus the drawing: the timeout classes, the
/// running order, the one-line BY TOOL footer, and the wire keys all four read
/// off.
final class ToolsTabDataTests: XCTestCase {
    private func fleet(_ tools: [SessionTools]) -> Fleet {
        Fleet(
            accounts: [],
            sessions: tools.enumerated().map { index, tool in
                Session(
                    sessionId: "session-\(index)", firstSeenMs: 1_000, lastSeenMs: 2_000,
                    tools: tool)
            })
    }

    // MARK: - TIMED OUT TODAY

    /// The section is HIDDEN against a server that does not send the field —
    /// never a card of zeroes, which would claim a measurement nobody made.
    func testTimeoutClassesAreNilWhenNoSessionReportsThem() {
        XCTAssertNil(fleet([SessionTools(timeouts: 3)]).toolsTimeoutClasses)
    }

    func testTimeoutClassesSumAcrossSessionsBiggestFirst() {
        let classes = fleet([
            SessionTools(timeoutsByClass: ["wait": 8, "build": 9]),
            SessionTools(timeoutsByClass: ["wait": 4, "git-net": 4]),
        ]).toolsTimeoutClasses
        XCTAssertEqual(classes?.map(\.name), ["wait", "build", "git-net"])
        XCTAssertEqual(classes?.map(\.count), [12, 9, 4])
    }

    /// A tie sorts by name, so two polls carrying identical data draw the rows
    /// in identical order rather than in whatever order the dictionary hashed.
    func testEqualCountsSortByName() {
        let classes = fleet([SessionTools(timeoutsByClass: ["wait": 2, "build": 2, "other": 2])])
            .toolsTimeoutClasses
        XCTAssertEqual(classes?.map(\.name), ["build", "other", "wait"])
    }

    /// A Bash call files under its class; anything else under its own tool
    /// name, which is how a timed-out `WebFetch` reaches the card.
    func testTimedOutCallsAttachToTheirClass() {
        let classes = fleet([
            SessionTools(
                timeoutsByClass: ["wait": 1, "WebFetch": 1],
                timedOut: [
                    ToolCall(
                        tool: "Bash", commandHead: "until grep -q ready log", commandClass: "wait"),
                    ToolCall(tool: "WebFetch", commandHead: "example.com"),
                ])
        ]).toolsTimeoutClasses
        XCTAssertEqual(
            classes?.first(where: { $0.name == "wait" })?.calls.map(\.call.commandHead),
            ["until grep -q ready log"])
        XCTAssertEqual(
            classes?.first(where: { $0.name == "WebFetch" })?.calls.map(\.call.tool), ["WebFetch"])
        // The session survives the grouping: an expanded class row names who
        // ran each command, the same as every other item on the tab.
        XCTAssertEqual(
            classes?.first(where: { $0.name == "wait" })?.calls.map(\.sessionId), ["session-0"])
    }

    /// Three classes carry a gloss; the rest say only their own name rather
    /// than take a caption that repeats the word.
    func testCaptions() {
        XCTAssertEqual(ToolTimeoutClass(name: "wait", count: 1).caption, "until / sleep loops")
        XCTAssertEqual(ToolTimeoutClass(name: "build", count: 1).caption, "cargo · swift")
        XCTAssertEqual(ToolTimeoutClass(name: "git-net", count: 1).caption, "push · fetch")
        XCTAssertNil(ToolTimeoutClass(name: "compound", count: 1).caption)
    }

    // MARK: - RUNNING NOW

    /// Among calls that share the same cappedness, oldest first: the call
    /// nearest its timeout (or simply the longest-standing) is the one the
    /// operator came to the tab for.
    func testRunningCallsSortOldestFirst() {
        let running = fleet([
            SessionTools(running: [
                ToolCall(tool: "Bash", commandHead: "young", startedMs: 5_000),
                ToolCall(tool: "Bash", commandHead: "no start time"),
                ToolCall(tool: "Bash", commandHead: "old", startedMs: 1_000),
            ])
        ]).toolsRunning
        XCTAssertEqual(running.map(\.call.commandHead), ["old", "young", "no start time"])
    }

    /// A capped call (`Bash`) outranks an uncapped one (`Agent`) even when it
    /// started far more recently — only the capped call can be near a
    /// timeout, so it must not be buried under an Agent that has simply run
    /// longer. Watched failing before the fix: pre-fix `toolsRunning` sorted
    /// purely on `startedMs`, so the 2-hour Agent (older `startedMs`) sorted
    /// FIRST and the near-timeout Bash call sorted second — the assertion
    /// below failed with `["long-running agent", "near timeout"]`.
    func testCappedCallsOutrankUncappedRegardlessOfAge() {
        let twoHoursAgoMs: Int64 = 5_000
        let nineMinutesAgoMs: Int64 = 4_000_000
        let running = fleet([
            SessionTools(running: [
                ToolCall(tool: "Agent", commandHead: "long-running agent", startedMs: twoHoursAgoMs),
                ToolCall(tool: "Bash", commandHead: "near timeout", startedMs: nineMinutesAgoMs),
            ])
        ]).toolsRunning
        XCTAssertEqual(running.map(\.call.commandHead), ["near timeout", "long-running agent"])
    }

    /// Ties among capped calls still break oldest-first.
    func testCappedTiesStayOldestFirst() {
        let running = fleet([
            SessionTools(running: [
                ToolCall(tool: "Bash", commandHead: "younger bash", startedMs: 5_000),
                ToolCall(tool: "Agent", commandHead: "an agent", startedMs: 1_000),
                ToolCall(tool: "Bash", commandHead: "older bash", startedMs: 2_000),
            ])
        ]).toolsRunning
        XCTAssertEqual(running.map(\.call.commandHead), ["older bash", "younger bash", "an agent"])
    }

    /// A call with no `startedMs` still sorts last, capped or not.
    func testCallWithNoStartTimeStillSortsLast() {
        let running = fleet([
            SessionTools(running: [
                ToolCall(tool: "Bash", commandHead: "no start time"),
                ToolCall(tool: "Agent", commandHead: "an agent", startedMs: 9_000),
                ToolCall(tool: "Bash", commandHead: "a bash call", startedMs: 1_000),
            ])
        ]).toolsRunning
        XCTAssertEqual(
            running.map(\.call.commandHead), ["a bash call", "an agent", "no start time"])
    }

    /// A call with its own, shorter timeout outranks an older `Bash` call when its deadline is
    /// nearer: the Bash call started 400 s earlier but has 600 s to run, so it is killed at
    /// 1,600,000 ms, while the Codex call's 120 s run out at 1,520,000 ms.
    func testRunningCallsSortNearestTheirOwnDeadlineFirst() throws {
        let payload = """
            {"supported": true, "sessions": [
              {"sessionId": "a1f3c2d4-0000-4000-8000-000000000001",
               "account": "alice@example.com", "firstSeenMs": 900000, "lastSeenMs": 1000000,
               "tools": {"running": [
                 {"tool": "Bash", "commandHead": "cargo test", "startedMs": 1000000}]}},
              {"sessionId": "019a3c5e-0000-7000-8000-000000000002",
               "account": "bob@example.com/codex", "provider": "codex",
               "firstSeenMs": 1300000, "lastSeenMs": 1400000,
               "tools": {"running": [
                 {"tool": "exec_command", "commandHead": "cargo build", "startedMs": 1400000,
                  "timeoutSeconds": 120}]}}]}
            """
        let read = try Fleet.decodeSessions(Data(payload.utf8))
        XCTAssertEqual(read.sessions.count, 2)
        let fleet = Fleet(accounts: [], sessions: read.sessions, sessionsSupported: true)
        XCTAssertEqual(fleet.toolsRunning.map(\.call.tool), ["exec_command", "Bash"])
    }

    // MARK: - Codex calls

    /// A Codex session's calls sit in the same three lists as Claude's, each entry carrying the
    /// provider of the session that made it, and BY TOOL files them under the Codex buckets.
    func testCodexCallsJoinRunningSlowestAndByTool() throws {
        let canonical = try Fleet.decodeSessions(CanonicalFixtures.data("codex-sessions.json"))
        let fleet = Fleet(accounts: [], sessions: canonical.sessions, sessionsSupported: true)

        let codexCall = try XCTUnwrap(fleet.toolsRunning.first { $0.call.tool == "exec_command" })
        XCTAssertEqual(codexCall.provider, .codex)
        // codex's default shell tool is the interactive `exec_command`, which takes no timeout.
        XCTAssertNil(codexCall.call.timeoutSeconds)
        XCTAssertEqual(fleet.toolsRunning.first { $0.call.tool == "Bash" }?.provider, .anthropic)

        let categories = try XCTUnwrap(fleet.toolsByCategory)
        XCTAssertEqual(
            categories.map(\.name), ["Bash", "Read/Grep/Edit", "Codex shell", "Codex edit"])
        XCTAssertEqual(categories.map(\.calls), [8, 4, 2, 1])

        let slowCodexSession = """
            {"supported": true, "sessions": [
              {"sessionId": "019a3c5e-0000-7000-8000-000000000005",
               "account": "bob@example.com/codex", "provider": "codex",
               "firstSeenMs": 1767225425000, "lastSeenMs": 1767225550000,
               "tools": {"calls": 1, "slowest": [
                 {"tool": "exec_command", "commandHead": "cargo test",
                  "endedMs": 1767225500000, "seconds": 42.0}]}}]}
            """
        let added = try Fleet.decodeSessions(Data(slowCodexSession.utf8)).sessions
        XCTAssertEqual(added.count, 1)
        let wider = Fleet(
            accounts: [], sessions: canonical.sessions + added, sessionsSupported: true)
        let slowest = try XCTUnwrap(wider.toolsSlowest.first)
        XCTAssertEqual(slowest.call.tool, "exec_command")
        XCTAssertEqual(slowest.call.seconds, 42.0)
        XCTAssertEqual(slowest.provider, .codex)
    }

    // MARK: - BY TOOL

    /// The line the bars were replaced with, exactly as
    /// `docs/design/tools-tab.md` writes it.
    func testSummaryLine() {
        let categories = [
            ToolCategory(name: "Bash", calls: 20_000, medianSeconds: nil),
            ToolCategory(name: "Agent", calls: 410, medianSeconds: nil),
            ToolCategory(name: "Read/Grep/Edit", calls: 4_400, medianSeconds: nil),
        ]
        XCTAssertEqual(
            ToolCategory.summaryLine(categories, medianSeconds: 2.1),
            "Bash 20,000 · Agent 410 · Read/Grep/Edit 4,400 · median 2.1s")
    }

    /// No p50 anywhere means no median clause — the same silence-over-a-guess
    /// rule the rest of the tab follows.
    func testSummaryLineDropsTheMedianWhenNobodyReportedOne() {
        XCTAssertEqual(
            ToolCategory.summaryLine(
                [ToolCategory(name: "Bash", calls: 7, medianSeconds: nil)], medianSeconds: nil),
            "Bash 7")
    }

    /// The median call's bucket, not a mean: 200 Agent calls at 400s must not
    /// drag the figure a fleet of 19,913 two-second Bash calls earns.
    func testMedianIsTheBucketTheMedianCallFallsIn() {
        let fleet = self.fleet([
            SessionTools(byTool: [
                ToolBucketRow(tool: "Bash", calls: 20_000, secondsP50: 2.1),
                ToolBucketRow(tool: "Agent", calls: 410, secondsP50: 400),
                ToolBucketRow(tool: "Read", calls: 4_400, secondsP50: 0.2),
            ])
        ])
        XCTAssertEqual(fleet.toolsMedianSeconds, 2.1)
    }

    func testMedianIsNilWhenNoBucketReportsAP50() {
        XCTAssertNil(
            fleet([SessionTools(byTool: [ToolBucketRow(tool: "Bash", calls: 9)])])
                .toolsMedianSeconds)
    }

    // MARK: - Wire keys

    /// The keys the SERVER actually sends. Every struct in
    /// `crates/tcr-status-wire/src/lib.rs` carries
    /// `#[serde(rename_all = "camelCase")]`, and this build decoded `by_tool`
    /// and `seconds_p50` — keys no server has ever sent — so the panel's BY
    /// TOOL section was `nil` against a live proxy while every test passed.
    /// No test decoded this type from JSON at all; this one does.
    func testSessionToolsDecodesTheWiresOwnKeys() throws {
        let json = """
            {
              "calls": 42,
              "errors": 1,
              "timeouts": 3,
              "overOneMinute": 7,
              "running": [
                {"tool": "Bash", "commandHead": "cargo test", "commandClass": "build",
                 "startedMs": 1700}
              ],
              "slowest": [{"tool": "Bash", "seconds": 600, "endedMs": 1800}],
              "byTool": [{"tool": "Bash", "calls": 42, "errors": 1, "secondsP50": 2.5,
                          "overOneMinute": 7}],
              "timeoutsByClass": {"wait": 2, "build": 1},
              "timedOut": [
                {"tool": "Bash", "commandHead": "until green", "commandClass": "wait",
                 "seconds": 600, "endedMs": 1900}
              ]
            }
            """
        let tools = try JSONDecoder().decode(SessionTools.self, from: Data(json.utf8))
        XCTAssertEqual(tools.overOneMinute, 7)
        XCTAssertEqual(tools.byTool.first?.secondsP50, 2.5)
        XCTAssertEqual(tools.byTool.first?.overOneMinute, 7)
        XCTAssertEqual(tools.running.first?.commandClass, "build")
        XCTAssertEqual(tools.timeoutsByClass, ["wait": 2, "build": 1])
        XCTAssertEqual(tools.timedOut.first?.commandClass, "wait")
    }

    /// A server built before any of the three new fields still decodes — the
    /// tab loses a section, never the session.
    func testOldServerPayloadStillDecodes() throws {
        let json = #"{"calls": 5, "running": [{"tool": "Read"}]}"#
        let tools = try JSONDecoder().decode(SessionTools.self, from: Data(json.utf8))
        XCTAssertEqual(tools.calls, 5)
        XCTAssertEqual(tools.timeoutsByClass, [:])
        XCTAssertEqual(tools.timedOut, [])
        XCTAssertNil(tools.running.first?.commandClass)
    }
}
