import Foundation
import XCTest

@testable import TcrBarCore

/// When TcrBar runs `tcr migrate --json`, and what it makes of every answer.
/// Paths and pids are made up; this repository is public.
final class NameMigrationTests: XCTestCase {
    // MARK: gate

    func testNoStoredVersionRuns() {
        XCTAssertEqual(NameMigrationGate.decide(lastLaunched: nil, current: "1234"), .run)
        XCTAssertEqual(NameMigrationGate.decide(lastLaunched: "", current: "1234"), .run)
    }

    func testSameVersionSkips() {
        XCTAssertEqual(NameMigrationGate.decide(lastLaunched: "1234", current: "1234"), .skip)
    }

    func testDifferentVersionRuns() {
        XCTAssertEqual(NameMigrationGate.decide(lastLaunched: "1234", current: "1240"), .run)
    }

    func testUnknownCurrentVersionNeverRuns() {
        XCTAssertEqual(NameMigrationGate.decide(lastLaunched: nil, current: nil), .skip)
        XCTAssertEqual(NameMigrationGate.decide(lastLaunched: "1234", current: ""), .skip)
    }

    func testTheKeyIsPinned() {
        XCTAssertEqual(NameMigrationMarker.lastLaunchedBuildKey, "lastLaunchedBundleVersion")
    }

    /// Nothing stored runs, the same build again skips, a new build runs once.
    @MainActor
    func testMarkerRunsOnTheFirstLaunchOfEachBuild() {
        let suite = "NameMigrationTests.marker.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        XCTAssertTrue(NameMigrationMarker(defaults: defaults).noteLaunch(current: "1234"))
        XCTAssertFalse(NameMigrationMarker(defaults: defaults).noteLaunch(current: "1234"))
        XCTAssertTrue(NameMigrationMarker(defaults: defaults).noteLaunch(current: "1240"))
        XCTAssertFalse(NameMigrationMarker(defaults: defaults).noteLaunch(current: "1240"))
        XCTAssertFalse(NameMigrationMarker(defaults: defaults).noteLaunch(current: nil))
        XCTAssertEqual(defaults.string(forKey: NameMigrationMarker.lastLaunchedBuildKey), "1240")
    }

    @MainActor
    func testMarkerWithNilDefaultsNeverRuns() {
        XCTAssertFalse(NameMigrationMarker(defaults: nil).noteLaunch(current: "1234"))
    }

    // MARK: parsing every outcome

    private func parse(_ json: String, exit: Int32 = 0, stderr: String = "") -> NameMigrationResult {
        NameMigrationResult.parse(exitCode: exit, stdout: Data(json.utf8), stderr: stderr)
    }

    func testParsesMoved() {
        let result = parse(
            """
            {"outcome":"moved","refusal":null,
             "units":[{"unit":"config","result":"moved","from":"/h/.config/old.json","to":"/h/.config/tcr.json"},
                      {"unit":"cache","result":"moved","from":"/h/.cache/old","to":"/h/.cache/tcr"}],
             "logs":[{"result":"renamed","from":"/h/.cache/tcr/logs/old.log.1","to":"/h/.cache/tcr/logs/tcr.log.1"}]}
            """)
        guard case .report(let report) = result else { return XCTFail("\(result)") }
        XCTAssertEqual(report.outcome, .moved)
        XCTAssertEqual(report.units.count, 2)
        XCTAssertEqual(report.units[0].to, "/h/.config/tcr.json")
        XCTAssertEqual(
            result.logLines,
            [
                "tcr migrate moved the old-name files",
                "config: /h/.config/old.json -> /h/.config/tcr.json",
                "cache: /h/.cache/old -> /h/.cache/tcr",
                "log: /h/.cache/tcr/logs/old.log.1 -> /h/.cache/tcr/logs/tcr.log.1",
            ])
    }

    func testParsesNothingToMoveIncludingBothExist() {
        let result = parse(
            """
            {"outcome":"nothing_to_move","refusal":null,
             "units":[{"unit":"config","result":"both_exist","old":"/h/a.json","new":"/h/tcr.json"},
                      {"unit":"cache","result":"neither","new":"/h/.cache/tcr"}],
             "logs":[]}
            """)
        guard case .report(let report) = result else { return XCTFail("\(result)") }
        XCTAssertEqual(report.outcome, .nothingToMove)
        XCTAssertEqual(
            result.logLines,
            [
                "tcr migrate: nothing to move",
                "config: both /h/a.json and /h/tcr.json exist; the new one is used",
            ])
    }

    func testParsesFailed() {
        let result = parse(
            """
            {"outcome":"failed","refusal":null,
             "units":[{"unit":"config","result":"failed","from":"/h/a.json","to":"/h/tcr.json","error":"Permission denied (os error 13)"},
                      {"unit":"cache","result":"already_new","path":"/h/.cache/tcr"}],
             "logs":[]}
            """, exit: 1)
        guard case .report(let report) = result else { return XCTFail("\(result)") }
        XCTAssertEqual(report.outcome, .failed)
        XCTAssertEqual(report.units[0].error, "Permission denied (os error 13)")
        XCTAssertEqual(result.logLines.count, 2)
        XCTAssertTrue(result.logLines[1].contains("Permission denied"), result.logLines[1])
    }

    func testParsesRefused() {
        let result = parse(
            """
            {"outcome":"refused","refusal":{"port":45678,"pid":4242,"message":"a tcr server is running on port 45678 (pid 4242)"},
             "units":[],"logs":[]}
            """, exit: 3)
        guard case .report(let report) = result else { return XCTFail("\(result)") }
        XCTAssertEqual(report.outcome, .refused)
        XCTAssertEqual(report.refusal?.pid, 4242)
        XCTAssertEqual(
            result.logLines, ["tcr migrate refused: a tcr server is running on port 45678 (pid 4242)"])
    }

    /// An older `tcr` without the verb: clap prints a usage error and exits 2.
    func testAnOlderTcrIsUnreadableAndSaysWhy() {
        let result = parse("", exit: 2, stderr: "error: unrecognized subcommand 'migrate'\n")
        XCTAssertEqual(
            result, .unreadable(exitCode: 2, detail: "error: unrecognized subcommand 'migrate'"))
        XCTAssertEqual(
            result.logLines,
            ["tcr migrate gave no report (exit 2): error: unrecognized subcommand 'migrate'"])
    }

    func testAnUnknownOutcomeIsUnreadable() {
        let result = parse(#"{"outcome":"exploded","refusal":null,"units":[],"logs":[]}"#)
        guard case .unreadable(let code, _) = result else { return XCTFail("\(result)") }
        XCTAssertEqual(code, 0)
    }
}
