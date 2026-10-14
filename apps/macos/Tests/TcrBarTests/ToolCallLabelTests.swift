import XCTest

@testable import TcrBarCore

/// What the Tools tab's duration pill prints, and what VoiceOver hears.
///
/// The finding these pin (review #4): a
/// call the proxy KILLED at the Bash timeout and one that merely ran long drew
/// the same string in the same pill, red against grey the only difference — and
/// the pill's accessibility label was that same string, so a listener was told
/// nothing at all about the kill.
///
/// The timeout arrives as a parameter rather than being read from `V4` here on
/// purpose: `V4` is in the executable target, which this test bundle does not
/// link, and an assertion that restated `600` would be an assertion about this
/// file. The view passes `V4.toolTimeoutSeconds`; what is under test is the
/// RULE, driven at a timeout of the test's own choosing.
final class ToolCallLabelTests: XCTestCase {
    func testDurationTiersAtOneMinute() {
        XCTAssertEqual(ToolCallLabel.duration(45), "45s")
        XCTAssertEqual(ToolCallLabel.duration(252), "4m 12s")
        XCTAssertEqual(ToolCallLabel.duration(600), "10m 0s")
    }

    /// The tier that only an UNCAPPED call can reach. A three hour `Agent` is
    /// ordinary in RUNNING NOW, and the two-tier form printed it `180m 3s`,
    /// which truncated to `180m…` in the Sessions tab's "oldest" column.
    func testDurationTiersAgainAtOneHour() {
        XCTAssertEqual(ToolCallLabel.duration(3600), "1h 0m")
        XCTAssertEqual(ToolCallLabel.duration(10803), "3h 0m")
        XCTAssertEqual(ToolCallLabel.duration(11100), "3h 5m")
        // The boundary belongs to the minute tier on its low side: 3599s is
        // still under an hour and must not round up into "1h 0m".
        XCTAssertEqual(ToolCallLabel.duration(3599), "59m 59s")
    }

    func testACallAtTheTimeoutSaysSoInWords() {
        XCTAssertEqual(ToolCallLabel.pill(seconds: 600, timeout: 600), "timed out")
        // Past the deadline — a kill lands a hair after the figure it was
        // measured against, and the pill must not fall back to the duration.
        XCTAssertEqual(ToolCallLabel.pill(seconds: 601.4, timeout: 600), "timed out")
    }

    func testACallThatMerelyRanLongKeepsItsDuration() {
        XCTAssertEqual(ToolCallLabel.pill(seconds: 583, timeout: 600), "9m 43s")
        XCTAssertNil(ToolCallLabel.spoken(seconds: 583, timeout: 600))
    }

    /// The spoken value is the whole point: it carries BOTH facts, the elapsed
    /// time the pill gave up and the kill the colour used to carry alone.
    func testTheSpokenValueNamesTheKillAndTheElapsedTime() {
        XCTAssertEqual(
            ToolCallLabel.spoken(seconds: 600, timeout: 600),
            "ran 10m 0s, killed at the 600 second timeout")
    }

    /// Two calls 17 seconds apart used to be indistinguishable to a reader who
    /// cannot see the tint. They are not any more, in print or out loud.
    func testTheKilledCallAndTheSlowOneDifferWithoutColour() {
        let killed = ToolCallLabel.pill(seconds: 600, timeout: 600)
        let slow = ToolCallLabel.pill(seconds: 583, timeout: 600)
        XCTAssertNotEqual(killed, slow)
        XCTAssertNotEqual(
            ToolCallLabel.spoken(seconds: 600, timeout: 600),
            ToolCallLabel.spoken(seconds: 583, timeout: 600))
    }

    /// A running call prints its elapsed time, and inside the warning band
    /// how long is left as well — never colour alone.
    func testARunningCallNamesTheSecondsLeftInsideTheWarningBand() {
        XCTAssertEqual(
            ToolCallLabel.running(elapsed: 580, remaining: 20, warnWithin: 60),
            "9m 40s · 20s left")
        XCTAssertEqual(
            ToolCallLabel.running(elapsed: 252, remaining: 348, warnWithin: 60),
            "4m 12s")
    }

    /// A tool with no cap — an Agent, a Read — can never be "20s from"
    /// anything, so it prints its age and stops. The first render of the
    /// redesigned tab printed "9m 43s · 17s left" beside an Agent call that
    /// drew no ring, which is the panel claiming a deadline it does not have.
    func testAnUncappedCallNeverPrintsSecondsLeft() {
        XCTAssertEqual(
            ToolCallLabel.running(elapsed: 583, remaining: nil, warnWithin: 60), "9m 43s")
    }

    /// A call the wire gave no start time draws an empty label rather than a
    /// made-up zero.
    func testACallWithNoStartTimePrintsNothing() {
        XCTAssertEqual(ToolCallLabel.running(elapsed: nil, remaining: nil, warnWithin: 60), "")
    }

    /// Past the deadline the clause floors at zero: a kill lands a hair after
    /// the deadline it was measured against, and "-2s left" is not a reading.
    func testSecondsLeftFloorsAtZero() {
        XCTAssertEqual(
            ToolCallLabel.running(elapsed: 602, remaining: -2, warnWithin: 60),
            "10m 2s · 0s left")
    }

    // MARK: - The tool word on a row

    /// A row names a codex call's tool by the kind its BY TOOL bucket names: codex's own
    /// names do not fit beside a running call's ring, time and `no Kill`, and a cut
    /// `exec_c…` reads as nothing. Any other codex tool keeps its name, and a Claude call
    /// keeps its own, whatever it is called.
    func testACodexRowNamesItsToolInAWordThatFits() {
        for tool in ["shell", "exec_command", "local_shell_call"] {
            XCTAssertEqual(ToolCallLabel.rowWord(forTool: tool, provider: .codex), "shell", tool)
            XCTAssertEqual(ToolCallLabel.category(forTool: tool, provider: .codex), "Codex shell")
        }
        XCTAssertEqual(ToolCallLabel.rowWord(forTool: "apply_patch", provider: .codex), "edit")
        XCTAssertEqual(
            ToolCallLabel.rowWord(forTool: "update_plan", provider: .codex), "update_plan")
        for tool in ["Bash", "Agent", "Read", "exec_command"] {
            XCTAssertEqual(ToolCallLabel.rowWord(forTool: tool, provider: .anthropic), tool)
        }
    }

    // MARK: - SLOWEST TODAY's timeout

    /// SLOWEST TODAY judges a Claude call by the Bash tool's 600 s, as it always has, and a
    /// codex call by its own timeout. codex's default `exec_command` names none and has none,
    /// so a codex call that ran ten minutes or more under it keeps its duration: "timed out",
    /// spoken "killed at the 600 second timeout", would be false. A one-shot call that ran
    /// into its own timeout says so.
    func testACodexCallIsJudgedByItsOwnTimeout() {
        let interactive = SessionToolEntry(
            sessionId: "s", call: ToolCall(tool: "exec_command", seconds: 900), provider: .codex)
        XCTAssertNil(ToolCallLabel.judgedTimeout(for: interactive, bashTimeout: 600))
        let none = ToolCallLabel.judgedTimeout(for: interactive, bashTimeout: 600)
        XCTAssertEqual(ToolCallLabel.pill(seconds: 900, timeout: none), "15m 0s")
        XCTAssertFalse(ToolCallLabel.timedOut(seconds: 900, timeout: none))
        XCTAssertNil(ToolCallLabel.spoken(seconds: 900, timeout: none))

        let oneShot = SessionToolEntry(
            sessionId: "s", call: ToolCall(tool: "exec_command", seconds: 120, timeoutSeconds: 120),
            provider: .codex)
        let own = ToolCallLabel.judgedTimeout(for: oneShot, bashTimeout: 600)
        XCTAssertEqual(own, 120)
        XCTAssertEqual(ToolCallLabel.pill(seconds: 120, timeout: own), "timed out")
        XCTAssertEqual(
            ToolCallLabel.spoken(seconds: 120, timeout: own),
            "ran 2m 0s, killed at the 120 second timeout")
        XCTAssertEqual(ToolCallLabel.pill(seconds: 700, timeout: own), "timed out")

        for tool in ["Bash", "Agent", "Read"] {
            let claude = SessionToolEntry(sessionId: "s", call: ToolCall(tool: tool, seconds: 600))
            XCTAssertEqual(ToolCallLabel.judgedTimeout(for: claude, bashTimeout: 600), 600, tool)
        }
    }

    /// Pinned at the source, because the row is a view this bundle cannot build: SLOWEST
    /// TODAY's pill, its colour and its spoken value all read the call's judged timeout, and
    /// none of them the Bash figure directly.
    func testSlowestTodayJudgesEachCallByItsJudgedTimeout() throws {
        let source = try fleetViewSource()
        let start = try XCTUnwrap(
            source.range(of: "private func slowestToolItem(_ entry: SessionToolEntry)"),
            "no slowestToolItem — update this test")
        let end = try XCTUnwrap(
            source.range(of: "private func timedOutItem(", range: start.upperBound..<source.endIndex))
        let item = source[start.upperBound..<end.lowerBound]
            .components(separatedBy: .whitespacesAndNewlines).joined()
        XCTAssertTrue(
            item.contains("ToolCallLabel.judgedTimeout(for:entry,bashTimeout:bashTimeoutSeconds)"),
            "SLOWEST TODAY no longer judges a call by its own timeout")
        XCTAssertFalse(
            item.contains("timeout:bashTimeoutSeconds)"),
            "SLOWEST TODAY judges a call against the Bash timeout directly again, so a codex "
                + "call with no timeout reads timed out at 600 s")
    }

    /// Pinned at the source: every Tools-tab row, running, slowest or timed out, names its
    /// tool through the one row word, so the three lists never name a call two ways.
    func testEveryToolRowNamesItsToolByTheRowWord() throws {
        let source = try fleetViewSource()
        let sharedItem = try XCTUnwrap(
            source.range(of: "private func toolItem<Trailing: View>("),
            "no toolItem — update this test")
        let itemEnd = try XCTUnwrap(
            source.range(
                of: "private func sectionHeading(", range: sharedItem.upperBound..<source.endIndex))
        XCTAssertTrue(
            source[sharedItem.upperBound..<itemEnd.lowerBound]
                .components(separatedBy: .whitespacesAndNewlines).joined()
                .contains(
                    "ToolCallLabel.rowWord(forTool:entry.call.tool,provider:entry.provider)"),
            "the Tools tab's rows no longer name a codex call's tool in a word that fits")
    }

    // MARK: - Categories

    /// codex's shell tools file under one bucket and its patch tool under another, each named
    /// for the provider; every other Codex tool shares a third rather than claiming a Claude one.
    func testTheCodexShellAndEditCategories() {
        for tool in ["shell", "exec_command", "local_shell_call"] {
            XCTAssertEqual(
                ToolCallLabel.category(forTool: tool, provider: .codex), "Codex shell", tool)
        }
        XCTAssertEqual(ToolCallLabel.category(forTool: "apply_patch", provider: .codex), "Codex edit")
        XCTAssertEqual(
            ToolCallLabel.category(forTool: "update_plan", provider: .codex), "Codex other")
        XCTAssertEqual(ToolCallLabel.category(forTool: "Bash", provider: .codex), "Codex other")
    }

    /// A Claude tool keeps the buckets it always had, whatever its name: a Claude call never
    /// lands in a Codex bucket, even one named like a codex tool.
    func testAClaudeToolKeepsClaudesBuckets() {
        XCTAssertEqual(ToolCallLabel.category(forTool: "Bash", provider: .anthropic), "Bash")
        XCTAssertEqual(ToolCallLabel.category(forTool: "Agent", provider: .anthropic), "Agent")
        XCTAssertEqual(ToolCallLabel.category(forTool: "Task", provider: .anthropic), "Agent")
        XCTAssertEqual(
            ToolCallLabel.category(forTool: "Read", provider: .anthropic), "Read/Grep/Edit")
        XCTAssertEqual(
            ToolCallLabel.category(forTool: "shell", provider: .anthropic), "Read/Grep/Edit")
        XCTAssertEqual(
            ToolCallLabel.category(forTool: "update_plan", provider: .anthropic), "Read/Grep/Edit")
        XCTAssertEqual(
            ToolCallLabel.categoryOrder,
            ["Bash", "Agent", "Read/Grep/Edit", "Codex shell", "Codex edit", "Codex other"])
    }

    /// FleetView.swift, which the test target cannot build, read as text.
    private func fleetViewSource() throws -> String {
        try String(
            contentsOf: URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent()  // TcrBarTests
                .deletingLastPathComponent()  // Tests
                .deletingLastPathComponent()  // apps/macos
                .appendingPathComponent("Sources/TcrBar/FleetView.swift"),
            encoding: .utf8)
    }
}
