import Foundation
import XCTest

@testable import TcrBarCore

/// When TcrBar runs `tcr migrate --json`, and what it makes of every answer.
/// Paths and pids are made up; this repository is public.
final class NameMigrationTests: XCTestCase {
    // MARK: gate

    private let home = URL(fileURLWithPath: "/Users/example")

    /// `oldNamesPresent` over a fake disk holding exactly `paths`.
    private func present(_ paths: Set<String>, environment: [String: String] = [:]) -> Bool {
        NameMigrationGate.oldNamesPresent(
            home: home, environment: environment, fileExists: { paths.contains($0) })
    }

    func testOldConfigOnlyIsPresent() {
        XCTAssertTrue(present(["/Users/example/.config/teamclaude.json"]))
    }

    func testOldCacheUnderHomeIsPresentWhenXdgIsUnset() {
        XCTAssertTrue(present(["/Users/example/.cache/teamclaude"]))
        // An empty XDG_CACHE_HOME counts as unset, as in src/paths.rs.
        XCTAssertTrue(present(["/Users/example/.cache/teamclaude"], environment: ["XDG_CACHE_HOME": ""]))
    }

    func testOldCacheUnderXdgIsPresentWhenXdgIsSet() {
        let xdg = ["XDG_CACHE_HOME": "/tmp/example-xdg"]
        XCTAssertTrue(present(["/tmp/example-xdg/teamclaude"], environment: xdg))
        // With XDG set, the HOME cache is not where tcr looks.
        XCTAssertFalse(present(["/Users/example/.cache/teamclaude"], environment: xdg))
    }

    func testNeitherIsNotPresent() {
        XCTAssertFalse(present([]))
        // The new names alone are not old names.
        XCTAssertFalse(present(["/Users/example/.config/tcr.json", "/Users/example/.cache/tcr"]))
    }

    func testABundledBuildRunsExactlyWhenOldNamesArePresent() {
        XCTAssertEqual(NameMigrationGate.decide(buildNumber: "1234", oldNamesPresent: true), .run)
        XCTAssertEqual(NameMigrationGate.decide(buildNumber: "1234", oldNamesPresent: false), .skip)
    }

    func testAnUnbundledBuildNeverRuns() {
        XCTAssertEqual(NameMigrationGate.decide(buildNumber: nil, oldNamesPresent: true), .skip)
        XCTAssertEqual(NameMigrationGate.decide(buildNumber: "", oldNamesPresent: true), .skip)
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
