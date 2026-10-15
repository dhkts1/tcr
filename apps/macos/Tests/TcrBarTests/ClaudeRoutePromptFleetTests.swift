import Foundation
import XCTest

@testable import TcrBarCore

/// The one-time "Send `claude` through tcr?" prompt, in a fleet that holds a Codex account.
///
/// A Codex account cannot serve claude. On a fleet with no enabled Claude account the proxy
/// refuses every request a routed claude sends, and `tcr run` warns so itself, so the prompt
/// would be offering to break the command it names. These tests pin the fleet half of the
/// prompt's decision, by the rule the Claude no-requests banner already keeps: a Claude-only
/// fleet is asked as it always was, and a fleet holding a Codex account also needs an enabled
/// Claude account. `ClaudeRouteSetupTests` pins the rest of the decision.
///
/// Account names are obviously fake. This repository is public.
final class ClaudeRoutePromptFleetTests: XCTestCase {

    func testACodexOnlyFleetIsNeverAskedToRouteClaude() throws {
        let fleet = try decodeFleet([codexAccount(name: "bob@example.com/codex")])
        XCTAssertFalse(ClaudeRoutePrompt.hasFleet(fleet))
    }

    func testAFleetHoldingCodexIsAskedOnlyWithAnEnabledClaudeAccount() throws {
        let mixed = try decodeFleet([
            account(name: "alice@example.com"), codexAccount(name: "bob@example.com/codex"),
        ])
        XCTAssertTrue(ClaudeRoutePrompt.hasFleet(mixed))
        let claudeSwitchedOff = try decodeFleet([
            account(name: "alice@example.com", disabled: true),
            codexAccount(name: "bob@example.com/codex"),
        ])
        XCTAssertFalse(ClaudeRoutePrompt.hasFleet(claudeSwitchedOff))
    }

    func testAClaudeOnlyFleetKeepsTheRuleThePromptShippedWith() throws {
        XCTAssertTrue(ClaudeRoutePrompt.hasFleet(try decodeFleet([account(name: "alice@example.com")])))
        XCTAssertTrue(
            ClaudeRoutePrompt.hasFleet(
                try decodeFleet([account(name: "alice@example.com", disabled: true)])),
            "a Claude-only fleet was asked whatever its accounts' state, and still is")
        XCTAssertFalse(ClaudeRoutePrompt.hasFleet(try decodeFleet([])))
    }

    /// The decision the shell makes before the panel's first frame, from the fleet it opens on.
    @MainActor
    func testOpeningThePanelDecidesFromTheFleet() throws {
        let suite = "tcrbar.tests.claude-route-fleet.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let controller = ClaudeRouteController(
            pinned: .status(ClaudeRouteStatus(route: .off)), defaults: defaults)

        controller.decideAtOpen(fleet: try decodeFleet([codexAccount(name: "bob@example.com/codex")]))
        XCTAssertFalse(controller.promptShown, "a Codex-only fleet is not asked about claude")
        controller.decideAtOpen(
            fleet: try decodeFleet([
                account(name: "alice@example.com"), codexAccount(name: "bob@example.com/codex"),
            ]))
        XCTAssertTrue(controller.promptShown, "a fleet with a Claude account to serve claude is")
        controller.decideAtOpen(fleet: nil)
        XCTAssertFalse(controller.promptShown, "no fleet has loaded yet")
    }

    /// The shim never compiles the app target, so this pins the one call that decides the
    /// prompt: the shell hands it the fleet the panel opens on, and no longer counts the
    /// fleet's accounts itself.
    func testTheShellDecidesThePromptFromTheFleet() throws {
        let shell = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .appendingPathComponent("Sources/TcrBar/MenuBarShell.swift")
        let contents = try String(contentsOf: shell, encoding: .utf8)
        XCTAssertTrue(
            contents.contains("claudeRoute.decideAtOpen(fleet: "),
            "MenuBarShell no longer decides the claude prompt from the fleet it opens on")
        XCTAssertFalse(
            contents.contains("decideAtOpen(hasFleet: "),
            "MenuBarShell still decides the claude prompt from an account count, which asks "
                + "a Codex-only fleet to route claude")
    }

    // MARK: - fixture

    private func decodeFleet(_ rows: [String]) throws -> Fleet {
        try Fleet.decode(Data("[\(rows.joined(separator: ","))]".utf8))
    }

    private func account(name: String, disabled: Bool = false) -> String {
        """
        {"name":"\(name)","priority":0,"status":"active",
         "disabled":\(disabled),"quota":0.5,"quotaState":"ok",
         "fiveHour":0.1,"sevenDay":0.1,
         "sevenDayOi":0.1,"sevenDayOiState":"ok","sevenDayOiResetAtMs":null,
         "held":[],
         "requests":0,"inputTokens":0,"outputTokens":0,
         "cacheReadTokens":0,"cacheHitRatio":0.0,"probeStatus":"ok",
         "probeError":null,
         "lastStreamError":null,"streamErrorCount":0,"source":"live",
         "serverSha":"abc1234","serverDirty":false}
        """
    }

    /// ``account(name:disabled:)`` as a Codex row.
    private func codexAccount(name: String, disabled: Bool = false) -> String {
        let row = account(name: name, disabled: disabled)
        return "{\"provider\":\"codex\"," + row.drop { $0 != "{" }.dropFirst()
    }
}
