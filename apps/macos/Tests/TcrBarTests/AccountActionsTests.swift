import XCTest

@testable import TcrBarCore

/// What a row may offer, by provider. ``Account/availableActions`` is the one value
/// the gear, the right-click menu and the card's own lines read, so the three cannot
/// disagree; this file pins it one action at a time.
///
/// The rows are the canonical fixture's (`apps/macos/Tests/Fixtures/codex-status.json`):
/// alice is the one Claude row, bob a ready Codex row with no imported login, erin a
/// signed-out Codex row, ivy a Codex row in orchard. Every name is fake.
final class AccountActionsTests: XCTestCase {

    /// A Claude row is offered everything it was offered before Codex accounts
    /// existed. A Codex row loses the Claude concepts (the control account, a
    /// minted token, `tcr run --group` and a group mint), swaps the exit picker for
    /// a statement, and is never inside a lease.
    ///
    /// bob has no `codexHome`, so his set stays this one when re-import arrives.
    func testTheWholeSetForEachProvider() throws {
        XCTAssertEqual(
            try canonical("alice@example.com").availableActions,
            [
                .toggleEnabled, .relogin, .controlAccount, .copyName, .copyAccessToken,
                .mintToken, .copyRunGroup, .mintGroupTokens, .parkGroup, .removeFromGroup,
                .addToGroup, .deleteAccount, .exitPicker, .lentToLine, .lend,
            ])
        XCTAssertEqual(
            try canonical("bob@example.com/codex").availableActions,
            [
                .toggleEnabled, .relogin, .copyName, .copyAccessToken, .groupStatement,
                .parkGroup, .removeFromGroup, .addToGroup, .deleteAccount, .exitStatement,
            ])
    }

    /// `tcr enable` and `tcr disable` take a Codex row as they take a Claude one,
    /// and a disabled row still offers the way back.
    func testEnableAndDisableWorkOnBothProviders() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.toggleEnabled))
        XCTAssertTrue(
            try canonical("bob@example.com/codex").availableActions.contains(.toggleEnabled))
        let disabledBob = try XCTUnwrap(
            decodeFleet([canonicalRow("bob@example.com/codex", ["disabled": true])]).accounts.first)
        XCTAssertTrue(disabledBob.disabled, "the override did not reach the row")
        XCTAssertTrue(disabledBob.availableActions.contains(.toggleEnabled))
    }

    /// One verb for both providers: a signed-out Codex row is offered Re-login like
    /// a broken Claude row, and so is a healthy one.
    func testReloginIsOfferedOnBothProviders() throws {
        for name in ["alice@example.com", "bob@example.com/codex", "erin@example.com/codex"] {
            XCTAssertTrue(
                try canonical(name).availableActions.contains(.relogin), "\(name) lost Re-login")
        }
    }

    /// A control account is a Claude concept, and an item that can only fail is
    /// worse than none.
    func testTheControlAccountItemsAreClaudeOnly() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.controlAccount))
        XCTAssertFalse(
            try canonical("bob@example.com/codex").availableActions.contains(.controlAccount))
    }

    func testCopyAccountNameWorksOnBothProviders() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.copyName))
        XCTAssertTrue(try canonical("bob@example.com/codex").availableActions.contains(.copyName))
    }

    /// The token item stays on a Codex row, named for what it copies: a ChatGPT
    /// bearer.
    func testCopyAccessTokenIsOfferedOnBothAndNamedForChatGPTOnCodex() throws {
        let alice = try canonical("alice@example.com")
        let bob = try canonical("bob@example.com/codex")
        XCTAssertTrue(alice.availableActions.contains(.copyAccessToken))
        XCTAssertTrue(bob.availableActions.contains(.copyAccessToken))
        XCTAssertEqual(TokenCommand.menuTitle(for: alice), "Copy Access Token")
        XCTAssertEqual(TokenCommand.menuTitle(for: bob), "Copy ChatGPT Access Token")
    }

    /// `tcr mint` refuses a Codex row, so the item is not offered there.
    func testMintIsClaudeOnly() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.mintToken))
        XCTAssertFalse(try canonical("bob@example.com/codex").availableActions.contains(.mintToken))
    }

    /// A Codex row's group menu opens with the fact that explains what it lacks.
    func testGroupInfoLinesStartWithTheCodexStatement() throws {
        let fleet = try canonicalFleet()
        XCTAssertTrue(try canonical("bob@example.com/codex").availableActions.contains(.groupStatement))
        XCTAssertFalse(try canonical("alice@example.com").availableActions.contains(.groupStatement))
        let ivy = try XCTUnwrap(fleet.accounts.first { $0.name == "ivy@example.com" })
        XCTAssertEqual(
            AccountActions.groupItems(
                for: ivy, group: "orchard", accounts: fleet.accounts, controlName: nil
            ).first,
            .statement("Codex sessions can't ask for a group"))
    }

    func testCopyRunGroupIsClaudeOnly() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.copyRunGroup))
        XCTAssertFalse(
            try canonical("bob@example.com/codex").availableActions.contains(.copyRunGroup))
    }

    func testGroupMintIsClaudeOnly() throws {
        XCTAssertTrue(
            try canonical("alice@example.com").availableActions.contains(.mintGroupTokens))
        XCTAssertFalse(
            try canonical("bob@example.com/codex").availableActions.contains(.mintGroupTokens))
    }

    func testParkWorksOnBothProviders() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.parkGroup))
        XCTAssertTrue(try canonical("bob@example.com/codex").availableActions.contains(.parkGroup))
    }

    func testRemoveFromGroupWorksOnBothProviders() throws {
        XCTAssertTrue(
            try canonical("alice@example.com").availableActions.contains(.removeFromGroup))
        XCTAssertTrue(
            try canonical("bob@example.com/codex").availableActions.contains(.removeFromGroup))
    }

    func testAddToGroupWorksOnBothProviders() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.addToGroup))
        XCTAssertTrue(try canonical("bob@example.com/codex").availableActions.contains(.addToGroup))
    }

    func testDeleteAccountWorksOnBothProviders() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.deleteAccount))
        XCTAssertTrue(
            try canonical("bob@example.com/codex").availableActions.contains(.deleteAccount))
    }

    /// A Codex request always leaves from this Mac, so a Codex card states that
    /// instead of offering a picker whose every peer choice the CLI refuses.
    func testTheExitRowIsAStatementOnCodexRows() throws {
        let alice = try canonical("alice@example.com")
        let bob = try canonical("bob@example.com/codex")
        XCTAssertTrue(alice.availableActions.contains(.exitPicker))
        XCTAssertFalse(bob.availableActions.contains(.exitPicker))
        XCTAssertTrue(bob.availableActions.contains(.exitStatement))
        XCTAssertFalse(alice.availableActions.contains(.exitStatement))
    }

    /// The server's lease never lends a Codex account, so its card never says it is
    /// lent.
    func testTheLentToLineIsClaudeOnly() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.lentToLine))
        XCTAssertFalse(
            try canonical("bob@example.com/codex").availableActions.contains(.lentToLine))
    }

    func testTheLeasePickerOffersClaudeAccountsOnly() throws {
        XCTAssertTrue(try canonical("alice@example.com").availableActions.contains(.lend))
        XCTAssertFalse(try canonical("bob@example.com/codex").availableActions.contains(.lend))
        XCTAssertEqual(
            PeerLease.eligibleLendAccounts(try canonicalFleet().accounts), ["alice@example.com"])
    }

    /// The two proxy-wide alerts change their words only once the fleet holds a
    /// Codex account. `RestartAlertCopyTests` pins the words themselves.
    func testTheTakeoverAndRestartAlertsChangeOnlyWithACodexAccount() {
        XCTAssertNotEqual(
            RestartAlertCopy.takeoverInformative(holdsCodex: false),
            RestartAlertCopy.takeoverInformative(holdsCodex: true))
        XCTAssertNotEqual(
            RestartAlertCopy.restartInformative(holdsCodex: false),
            RestartAlertCopy.restartInformative(holdsCodex: true))
    }

    /// The card draws its lent-to line, its exit row and its exit waiting pill
    /// through the same set, and keeps every anchor its own wiring tests hold.
    ///
    /// Read from the source, because `AccountCard` is a view in the app target,
    /// which this test target does not link.
    func testTheCardGatesItsLinesThroughAvailableActions() throws {
        let card = try repoFile("apps/macos/Sources/TcrBar/PanelV4/AccountCard.swift")
        for gate in [
            "account.availableActions.contains(.lentToLine)",
            "AccountExit.statement(for: account, exit: exit)",
            "AccountExitStatement(",
            "account.availableActions.contains(.exitPicker)",
        ] {
            XCTAssertTrue(card.contains(gate), "AccountCard.swift no longer holds \(gate)")
        }
        for anchor in [
            "if let line = PeerLease.lentToLine(lentTo) {",
            "onOpenLender(first)",
            "exit?.waitingPill(peers:",
            "if exitWaitingPillText == nil {",
            "V4Pill(text: waiting, role: .warn",
            "V4Pill(text: statePillText, role: statePillRole",
        ] {
            XCTAssertTrue(card.contains(anchor), "AccountCard.swift lost \(anchor)")
        }
    }

    /// A Codex row with a reported exit takes the tall card, as a Claude row does,
    /// so its exit statement is on screen without hover: neither the floating card
    /// nor the kept-open card receives the exit.
    func testACodexRowWithAnExitKeepsTheTallCard() throws {
        let bob = try canonical("bob@example.com/codex")
        let row = FleetSectionRow(
            account: bob, group: .ungrouped, band: FleetBand(bob), isControl: false)
        XCTAssertEqual(row.form(carriesLease: false, carriesExit: true), .card)
        XCTAssertEqual(
            row.form(carriesLease: false, carriesExit: true, keptOpen: KeptOpenCards([bob.id])),
            .card)
        XCTAssertNotEqual(
            row.form(carriesLease: false, carriesExit: false), .card,
            "a Codex row with no exit is a line, which is what makes the two cases above "
                + "a test of the exit rather than of every Codex row")
    }

    /// One canonical row, decoded inside the whole canonical fleet.
    private func canonical(_ name: String) throws -> Account {
        try XCTUnwrap(
            canonicalFleet().accounts.first { $0.name == name }, "no canonical \(name)")
    }
}

/// One canonical row from apps/macos/Tests/Fixtures/codex-status.json, with `overrides`
/// written over its JSON object (`NSNull()` writes a JSON null).
private func canonicalRow(_ name: String, _ overrides: [String: Any] = [:]) throws -> [String: Any] {
    let url = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TcrBarTests
        .deletingLastPathComponent()  // Tests
        .appendingPathComponent("Fixtures/codex-status.json")
    let rows = try XCTUnwrap(
        JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
    var row = try XCTUnwrap(rows.first { $0["name"] as? String == name }, "no canonical \(name)")
    for (key, value) in overrides { row[key] = value }
    return row
}

private func decodeFleet(_ rows: [[String: Any]]) throws -> Fleet {
    try Fleet.decode(JSONSerialization.data(withJSONObject: rows))
}

/// All ten canonical rows: alice (Claude, in orchard) and nine Codex rows.
private func canonicalFleet() throws -> Fleet {
    try decodeFleet(
        [
            "alice@example.com", "bob@example.com/codex", "carol@example.com", "dana@example.com",
            "erin@example.com/codex", "gil@example.com", "hal@example.com", "ivy@example.com",
            "jo@example.com", "kim@example.com",
        ].map { try canonicalRow($0) })
}

/// A file of this repository, as text.
private func repoFile(_ path: String) throws -> String {
    let root = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TcrBarTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // apps/macos
        .deletingLastPathComponent()  // apps
        .deletingLastPathComponent()  // repo root
    return try String(contentsOf: root.appendingPathComponent(path), encoding: .utf8)
}
