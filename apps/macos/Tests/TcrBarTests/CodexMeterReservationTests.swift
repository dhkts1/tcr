import XCTest

@testable import TcrBarCore

final class CodexMeterReservationTests: XCTestCase {
    /// Every window the fleet reported is reserved, the Codex widest label included.
    /// `panelLabels` takes no visibility input; that hidden rows still count is a fact of
    /// its call site, which ``testTheTabSizesTheMetersFromTheWholeFleet()`` reads.
    func testMeterLabelsReserveEveryReportedWindow() throws {
        let fleet = try fixture()
        let all = AccountLineFit.panelLabels(fleet.accounts)
        XCTAssertEqual(all, ["5h", "7d", AccountLineFit.codexWidestLabel, "30d"])
    }

    /// The Accounts tab sizes its meters from every account in the fleet and its name
    /// column from every row it can draw, never from the lines on screen: built from the
    /// visible rows, collapsing a group, spending out a row or keeping a card open moved
    /// every bar. The test target links TcrBarCore alone, so the tab is read as text,
    /// comment lines dropped and whitespace removed.
    func testTheTabSizesTheMetersFromTheWholeFleet() throws {
        let file = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // TcrBarTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // apps/macos
            .appendingPathComponent("Sources/TcrBar/PanelV4/AccountsTabV4.swift")
        let source = try String(contentsOf: file, encoding: .utf8)
        let code = source.split(separator: "\n", omittingEmptySubsequences: false)
            .filter { !$0.trimmingCharacters(in: .whitespaces).hasPrefix("//") }
            .joined()
            .components(separatedBy: .whitespacesAndNewlines).joined()
        let start = try XCTUnwrap(
            code.range(of: "privatevarliveLineFit:LiveLineFit{"),
            "AccountsTabV4 has no liveLineFit — update this test's anchors")
        let end = try XCTUnwrap(
            code.range(of: "returnLiveLineFit(", range: start.upperBound..<code.endIndex))
        let body = code[start.upperBound..<end.lowerBound]
        let expected =
            "letnames=AccountLineFit.lineNames(lists.flatMap(\\.sections))"
            + "letlabels=AccountLineFit.panelLabels(fleet.accounts)"
            + "letbar=V4.lineBarWidth(labels:labels)"
        XCTAssertEqual(
            String(body), expected,
            "the meters or the name column are sized from something other than the whole fleet")
    }

    /// A Codex account's first probe can report a `30d` window while the panel
    /// is open. The label column is already that wide before it does, so the
    /// report moves no bar (2026-10-15).
    func testACodexWindowReportedLaterMovesNoBar() throws {
        let reported = try fixture()
        let unreported = try fixture(sevenDayWindowMinutes: nil)
        XCTAssertFalse(AccountLineFit.panelLabels(unreported.accounts).contains("30d"))
        let m = metrics
        XCTAssertEqual(
            AccountLineFit.labelColumn(m, labels: AccountLineFit.panelLabels(unreported.accounts)),
            AccountLineFit.labelColumn(m, labels: AccountLineFit.panelLabels(reported.accounts)))
    }

    /// A fleet of Claude accounts alone reserves nothing past `5h` and `7d`.
    func testAClaudeOnlyFleetReservesNoCodexLabel() throws {
        let claudeOnly = Fleet(accounts: try fixture().accounts.filter { $0.provider != .codex })
        XCTAssertEqual(AccountLineFit.panelLabels(claudeOnly.accounts), AccountLineFit.labels)
    }

    private var metrics: AccountLineFit.Metrics {
        AccountLineFit.Metrics(
            dot: 8, gap: 7, meterGap: 4, labelMinWidth: 14, percentMinWidth: 33,
            signalWidth: 56, nameMinWidth: 54, barMaxWidth: 32, nameSize: 13,
            labelSize: 11, percentSize: 12, signalSize: 11.5)
    }

    private func fixture(sevenDayWindowMinutes: Int? = 43_200) throws -> Fleet {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Fixtures/codex-status.json")
        let rows = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
        let claude = try XCTUnwrap(rows.first { $0["provider"] as? String != "codex" })
        var codex = try XCTUnwrap(rows.first { $0["name"] as? String == "bob@example.com/codex" })
        if let sevenDayWindowMinutes {
            codex["sevenDayWindowMinutes"] = sevenDayWindowMinutes
        } else {
            codex.removeValue(forKey: "sevenDayWindowMinutes")
        }
        return try Fleet.decode(JSONSerialization.data(withJSONObject: [claude, codex]))
    }
}
