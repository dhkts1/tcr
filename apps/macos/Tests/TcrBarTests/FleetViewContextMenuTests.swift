import XCTest

/// `AccountRow`'s `contextMenuItems` is a `@ViewBuilder` built from live
/// `AccountController`/`ControlAccountController`/`GroupController`/
/// `RemoveAccountController` dependencies with no lightweight test doubles in
/// this target, and ViewInspector is not a dependency here — so this reads
/// the source directly, the same technique `FleetStatusTests` already uses
/// (`testAccountStatusErrorTokenStillExistsInRustSource`) for an assertion a
/// runtime test can't reach cheaply.
final class ContextMenuReloginAvailabilityTests: XCTestCase {
    /// The bug this test guards: "Re-login…" used to disappear from the
    /// context menu the moment an account came back healthy, which made it
    /// useless as the migration path off the eight-hour credentials PR #214
    /// replaced with year-long ones — a healthy row is exactly the row an
    /// operator would want to migrate ahead of its old token's own expiry.
    func testReloginItemHasNoHealthGateInContextMenu() throws {
        let body = try contextMenuItemsBody()
        XCTAssertFalse(
            body.contains("account.health == .needsRelogin"),
            "contextMenuItems still gates \"Re-login…\" on .needsRelogin — "
                + "the item is hidden on every healthy row, so an account "
                + "that logged in before PR #214 has no way to pick up its "
                + "one-year credential without first breaking"
        )
        XCTAssertTrue(
            body.contains(#"Button("Re-login…") { onRelogin() }"#),
            "the \"Re-login…\" button itself has moved or been renamed in "
                + "contextMenuItems — update this test's anchor to match"
        )
    }

    /// "Keep Card Open" / "Close Card" lives in the row's right-click menu
    /// (Gil, 2026-10-14), wired to the toggle, beside every existing item.
    func testKeepCardOpenItemIsInTheContextMenu() throws {
        let body = try contextMenuItemsBody()
        XCTAssertTrue(
            body.contains(#"Button(kept ? "Close Card" : "Keep Card Open") { onToggleKeptOpen() }"#),
            "the Keep Card Open item is missing from contextMenuItems")
        for item in [
            #"Button(enabling ? "Enable" : "Disable")"#, #"Button("Copy Account Name")"#,
            #"Button("Delete Account…")"#,
        ] {
            XCTAssertTrue(body.contains(item), "\(item) is gone from contextMenuItems")
        }
    }

    /// The legacy panel (`TCRBAR_LEGACY_PANEL=1`) words its six account strings
    /// through `LegacyRowCopy`, which keeps a Claude row's words and gives a Codex
    /// row ChatGPT's, so no Codex row is credited to Anthropic. Read from the source
    /// because the legacy row is a view in the app target; the words themselves are
    /// tested in `CodexCardFactsTests`. The four refusal branches also open to a
    /// refused Codex row, whatever held it, or it would read as rotating.
    func testEachLegacySiteCallsLegacyRowCopy() throws {
        let source = try String(
            contentsOf: repoRoot().appendingPathComponent(
                "apps/macos/Sources/TcrBar/FleetView.swift"),
            encoding: .utf8)
        let calls: [(call: String, sites: Int)] = [
            ("LegacyRowCopy.planHelp(account)", 1),
            ("LegacyRowCopy.orgTagHelp(account)", 1),
            ("LegacyRowCopy.rotatingHelp(account)", 1),
            ("LegacyRowCopy.rejectedPillWord(account)", 2),
            ("LegacyRowCopy.rejectedPillHelp(account", 1),
            ("LegacyRowCopy.rejectedSpoken(account", 1),
        ]
        for (call, sites) in calls {
            XCTAssertEqual(
                source.components(separatedBy: call).count - 1, sites,
                "FleetView.swift no longer calls \(call) at its \(sites) legacy site(s)")
        }
        for literal in [
            "This account's plan, as Anthropic reports it for its organization.",
            "The organization half of this account's name",
            "In the rotation — this account can be picked for traffic",
            #"StatusPill("rejected""#,
            "Anthropic has rejected this account, so `tcr` sends it no",
            "rejected by Anthropic, not in rotation",
        ] {
            XCTAssertFalse(
                source.contains(literal),
                "FleetView.swift still holds \(literal), which a Codex row would read")
        }
        XCTAssertEqual(
            source.components(
                separatedBy:
                    "account.isRejected || account.isRefusedHold || account.isForbiddenHold"
            ).count - 1, 4,
            "the four legacy refusal branches no longer open to a refused Codex row")
        XCTAssertFalse(source.contains("} else if account.isRejected {"))
    }

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // FleetViewContextMenuTests.swift -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
    }

    /// Isolates the `contextMenuItems` computed property's source text so
    /// the assertion above cannot accidentally match some other `.needsRelogin`
    /// check elsewhere in the file (e.g. the standalone `reloginButton`'s own
    /// gate, which is deliberately left alone).
    private func contextMenuItemsBody() throws -> String {
        let thisFile = URL(fileURLWithPath: #filePath)
        let repoRoot =
            thisFile
            .deletingLastPathComponent()  // FleetViewContextMenuTests.swift -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
        let fleetView = repoRoot.appendingPathComponent(
            "apps/macos/Sources/TcrBar/FleetView.swift")
        let contents = try String(contentsOf: fleetView, encoding: .utf8)
        guard
            let start = contents.range(
                of: "private var contextMenuItems: some View {"),
            let end = contents.range(
                of: "private var groupMenuItems: some View {",
                range: start.upperBound..<contents.endIndex)
        else {
            XCTFail(
                "contextMenuItems or groupMenuItems has moved or been renamed "
                    + "in \(fleetView.path) — update this test's anchors")
            return ""
        }
        return String(contents[start.upperBound..<end.lowerBound])
    }
}
