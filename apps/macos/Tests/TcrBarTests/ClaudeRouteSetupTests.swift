import Foundation
import XCTest

@testable import TcrBarCore

/// The claude-route prompt and Settings row: what `tcr setup status --json`
/// decodes to, when the prompt may show, which `tcr setup` calls a change
/// runs, and the rule that nothing but opening the panel decides whether the
/// prompt is drawn.
final class ClaudeRouteSetupTests: XCTestCase {
    /// Captured from `tcr setup status --json` against a scratch HOME whose
    /// `.zshrc` held `alias claude=x` (paths rewritten to example ones).
    private let realStatus = """
        {
          "route": "off",
          "shell": "zsh",
          "shellRoute": { "installed": false, "files": [] },
          "claudeSettings": {
            "installed": false,
            "path": "/Users/alice/.claude/settings.json",
            "baseUrl": null,
            "problem": null
          },
          "conflicts": [
            {
              "route": "shell",
              "kind": "alias",
              "path": "/Users/alice/.zshrc",
              "line": 1,
              "text": "alias claude=x"
            }
          ],
          "port": 3456
        }
        """

    func testTheRealStatusDocumentDecodes() {
        let reading = ClaudeRouteCommand.classifyStatus(
            exitCode: 0, stdout: Data(realStatus.utf8), stderr: "")
        guard case .status(let status) = reading else {
            return XCTFail("expected a status, got \(reading)")
        }
        XCTAssertEqual(status.route, .off)
        XCTAssertEqual(status.port, 3456)
        XCTAssertEqual(status.conflicts.count, 1)
        XCTAssertEqual(
            status.conflicts[0].summary(home: "/Users/alice"), "~/.zshrc:1: alias claude=x")
    }

    func testAnOlderTcrIsUnavailableNotOff() {
        let reading = ClaudeRouteCommand.classifyStatus(
            exitCode: 2, stdout: Data(), stderr: "error: unrecognized subcommand 'setup'\n")
        XCTAssertEqual(reading, .unavailable(reason: "error: unrecognized subcommand 'setup'"))
        XCTAssertFalse(
            ClaudeRoutePrompt.shouldShow(hasFleet: true, reading: reading, answered: false))
    }

    func testThePromptShowsOnlyWithAFleetNoRouteNoConflictAndNoAnswer() {
        let off = ClaudeRouteCommand.Reading.status(ClaudeRouteStatus(route: .off))
        XCTAssertTrue(ClaudeRoutePrompt.shouldShow(hasFleet: true, reading: off, answered: false))
        XCTAssertFalse(ClaudeRoutePrompt.shouldShow(hasFleet: false, reading: off, answered: false))
        XCTAssertFalse(ClaudeRoutePrompt.shouldShow(hasFleet: true, reading: off, answered: true))
        XCTAssertFalse(ClaudeRoutePrompt.shouldShow(hasFleet: true, reading: nil, answered: false))
        for route in [ClaudeRoute.shell, .everyLaunch, .both] {
            XCTAssertFalse(
                ClaudeRoutePrompt.shouldShow(
                    hasFleet: true, reading: .status(ClaudeRouteStatus(route: route)),
                    answered: false), "\(route)")
        }
        let conflicted = ClaudeRouteCommand.Reading.status(
            ClaudeRouteStatus(
                route: .off,
                conflicts: [
                    ClaudeRouteConflict(
                        route: "shell", kind: "alias", path: "/Users/alice/.zshrc", line: 1,
                        text: "alias claude=x")
                ]))
        XCTAssertFalse(
            ClaudeRoutePrompt.shouldShow(hasFleet: true, reading: conflicted, answered: false))
    }

    func testStepsInstallTheNewRouteBeforeRemovingTheOld() {
        XCTAssertEqual(
            ClaudeRouteCommand.steps(from: .off, to: .shell), [ClaudeRouteCommand.installShell])
        XCTAssertEqual(
            ClaudeRouteCommand.steps(from: .shell, to: .everyLaunch),
            [ClaudeRouteCommand.installSettings, ClaudeRouteCommand.removeShell])
        XCTAssertEqual(
            ClaudeRouteCommand.steps(from: .both, to: .off),
            [ClaudeRouteCommand.removeShell, ClaudeRouteCommand.removeSettings])
        XCTAssertEqual(ClaudeRouteCommand.steps(from: .shell, to: .shell), [])
        XCTAssertEqual(ClaudeRouteCommand.installShell, ["setup", "shell"])
        XCTAssertEqual(ClaudeRouteCommand.removeSettings, ["setup", "claude-settings", "--remove"])
    }

    func testARefusalKeepsTcrsWordsWithoutTheErrorPrefix() {
        let stderr =
            "Error: refusing to install the shell route: `claude` is already defined\n  /Users/alice/.zshrc:1: alias claude=x (an alias)\n"
        XCTAssertEqual(
            ClaudeRouteCommand.failureText(exitCode: 1, stderr: stderr),
            "refusing to install the shell route: `claude` is already defined\n  /Users/alice/.zshrc:1: alias claude=x (an alias)"
        )
        XCTAssertEqual(ClaudeRouteCommand.failureText(exitCode: 1, stderr: ""), "tcr exited 1")
    }

    /// Only opening the panel decides whether the prompt is drawn. A status
    /// read landing, or an answer, while it is open changes neither
    /// ``ClaudeRouteController/promptShown`` nor removes the row.
    @MainActor
    func testOnlyOpeningThePanelDecidesThePrompt() async throws {
        let suite = "tcrbar.tests.claude-route.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let controller = ClaudeRouteController(
            pinned: .status(ClaudeRouteStatus(route: .off)), defaults: defaults)
        XCTAssertFalse(controller.promptShown, "nothing shows before the panel opens")

        controller.decideAtOpen(hasFleet: true)
        XCTAssertTrue(controller.promptShown)
        XCTAssertNil(controller.promptOutcome)

        await controller.refresh()
        XCTAssertTrue(controller.promptShown, "a status read does not take the row away")

        await controller.answerPrompt(.notNow)
        XCTAssertTrue(controller.promptShown, "answering keeps the row for this opening")
        XCTAssertEqual(
            controller.promptOutcome, ClaudeRoutePrompt.outcome(for: .notNow, failure: nil))
        XCTAssertTrue(defaults.bool(forKey: ClaudeRoutePrompt.answeredKey))

        controller.decideAtOpen(hasFleet: true)
        XCTAssertFalse(controller.promptShown, "the next opening never asks again")
        XCTAssertNil(controller.promptOutcome)
    }
}
