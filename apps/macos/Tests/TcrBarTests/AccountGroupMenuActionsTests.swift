import XCTest

@testable import TcrBarCore

/// `Account.groupMenuActions` — the row-level right-click menu that changes
/// group membership; originally
/// added alongside deck cards, whose own deck-card
/// summary-line tests this file used to carry were deleted along with
/// `GroupSection`/`GroupDeckCard`). Account names are obviously fake — this
/// repository is public.
final class AccountGroupMenuActionsTests: XCTestCase {

    /// An account in two groups lists a "Remove from <group>" for each,
    /// plus "Remove from all groups", plus "Add to group…" last.
    func testAccountInTwoGroupsListsBothRemovalsPlusRemoveAllPlusAdd() {
        let account = menuAccount("a@example.com", groups: ["dev", "codereview"])
        XCTAssertEqual(
            account.groupMenuActions,
            [
                .remove(group: "codereview"),
                .remove(group: "dev"),
                .removeAll,
                .addToGroup,
            ]
        )
    }

    /// A single membership gets its own removal action but NOT
    /// "Remove from all groups" — a second control doing the exact same
    /// thing as the first is noise, not a convenience.
    func testAccountInOneGroupOffersNoRemoveAll() {
        let account = menuAccount("a@example.com", groups: ["dev"])
        XCTAssertEqual(account.groupMenuActions, [.remove(group: "dev"), .addToGroup])
    }

    /// An account in no group offers only "Add to group…" — the missing
    /// affordance the bridge exists to add, with nothing to remove.
    func testAccountInNoGroupOffersOnlyAddToGroup() {
        let account = menuAccount("a@example.com", groups: nil)
        XCTAssertEqual(account.groupMenuActions, [.addToGroup])
    }

    /// Same for an explicitly empty (not nil) groups array — the wire's
    /// other spelling of "no membership".
    func testAccountWithEmptyGroupsArrayOffersOnlyAddToGroup() {
        let account = menuAccount("a@example.com", groups: [])
        XCTAssertEqual(account.groupMenuActions, [.addToGroup])
    }

    // MARK: - What one group's submenu holds, by provider
    //
    // Over the canonical fleet (apps/macos/Tests/Fixtures/codex-status.json): alice is
    // the one Claude row and sits in orchard with ivy, a Codex row; kim is a Codex row
    // alone in research, which research's own members reserve.

    /// The routing warning a group serving nothing gets, word for word.
    private let warn = "\u{26A0}\u{FE0E} “orchard” routes nothing — control account only"

    /// A Codex row's submenu opens with the fact that explains what it lacks, then
    /// the routing line only because orchard has a Claude member, then what works.
    func testACodexRowsGroupMenuStartsWithTheStatement() throws {
        let fleet = try canonicalFleet()
        let ivy = try member("ivy@example.com", of: fleet)
        let park = GroupMenuItem.park(title: "Park group “orchard” (1 Claude, 1 Codex)", parked: false)
        XCTAssertEqual(
            AccountActions.groupItems(
                for: ivy, group: "orchard", accounts: fleet.accounts, controlName: nil),
            [.statement("Codex sessions can't ask for a group"), park, .removeFromGroup, .deleteGroup])
        XCTAssertEqual(
            AccountActions.groupItems(
                for: ivy, group: "orchard", accounts: fleet.accounts,
                controlName: "alice@example.com"),
            [
                .statement("Codex sessions can't ask for a group"), .statement(warn), park,
                .removeFromGroup, .deleteGroup,
            ])
    }

    /// No `tcr run --group` copy and no group mint on a Codex row, and no routing
    /// line in a group with no Claude member: the statement alone says why.
    func testCopyRunIsHiddenOnCodexRows() throws {
        let fleet = try canonicalFleet()
        let kim = try member("kim@example.com", of: fleet)
        XCTAssertEqual(
            AccountActions.groupItems(
                for: kim, group: "research", accounts: fleet.accounts, controlName: nil),
            [
                .statement("Codex sessions can't ask for a group"),
                .park(title: "Park group “research”", parked: false), .removeFromGroup,
                .deleteGroup,
            ])
    }

    /// A mint over a group with Codex members can only mint its Claude accounts,
    /// and its item says so.
    func testTheGroupMintNamesClaudeInAMixedGroup() throws {
        let fleet = try canonicalFleet()
        let alice = try member("alice@example.com", of: fleet)
        let mixed: [GroupMenuItem] = [
            .copyRun,
            .mintGroup(title: "Mint Long-Lived Tokens for “orchard”’s Claude Accounts…"),
            .park(title: "Park group “orchard” (1 Claude, 1 Codex)", parked: false),
            .removeFromGroup, .deleteGroup,
        ]
        XCTAssertEqual(
            AccountActions.groupItems(
                for: alice, group: "orchard", accounts: fleet.accounts, controlName: nil),
            mixed)
        XCTAssertEqual(
            AccountActions.groupItems(
                for: alice, group: "orchard", accounts: fleet.accounts,
                controlName: "alice@example.com"),
            [.statement(warn)] + mixed)
    }

    /// Parking a group parks every member, Claude and Codex, and the item counts
    /// both.
    func testParkCountsBothProvidersInAMixedGroup() throws {
        let fleet = try decodeFleet(
            canonicalNames.map { name in
                try canonicalRow(
                    name,
                    ["alice@example.com", "ivy@example.com"].contains(name)
                        ? ["parkedGroups": ["orchard"]] : [:])
            })
        let alice = try member("alice@example.com", of: fleet)
        let parkItems = AccountActions.groupItems(
            for: alice, group: "orchard", accounts: fleet.accounts, controlName: nil
        ).filter { item in
            if case .park = item { return true }
            return false
        }
        XCTAssertEqual(
            parkItems, [.park(title: "Unpark group “orchard” (1 Claude, 1 Codex)", parked: true)])
    }

    /// No Codex request can reach a reserved group, so a Codex row is shown one
    /// disabled, with no copy entry. A Claude row is offered it as today.
    func testAReservedGroupIsListedDisabledForACodexRow() throws {
        let fleet = try canonicalFleet()
        let reserved = AddToGroupEntry(
            group: "research", title: "research (reserved: no Codex request can reach it)",
            isEnabled: false, offersCopy: false)
        XCTAssertEqual(
            AccountActions.addToGroupEntries(
                for: try member("ivy@example.com", of: fleet), accounts: fleet.accounts),
            [reserved])
        XCTAssertEqual(
            AccountActions.addToGroupEntries(
                for: try member("alice@example.com", of: fleet), accounts: fleet.accounts),
            [AddToGroupEntry(group: "research", title: "research", isEnabled: true, offersCopy: true)])
        XCTAssertEqual(
            AccountActions.addToGroupEntries(
                for: try member("bob@example.com/codex", of: fleet), accounts: fleet.accounts),
            [
                AddToGroupEntry(group: "orchard", title: "orchard", isEnabled: true, offersCopy: true),
                reserved,
            ])
    }

    /// A Claude-only fleet's submenu is today's, item for item: the routing line,
    /// the run copy, the mint, park, and the two removals. Its Add to group…
    /// candidates are today's too, in today's order, a reserved group among them
    /// as a plain, enabled entry.
    ///
    /// The candidates are written as a literal: what today's panel offered was a
    /// private property of the app target, which this target cannot reach. The two
    /// copies are named nina and henry, not carol, because carol is a canonical
    /// Codex row.
    func testAClaudeOnlyGroupMenuIsTodays() throws {
        let alone = try decodeFleet([canonicalRow("alice@example.com", ["groups": ["dev"]])])
        let alice = try member("alice@example.com", of: alone)
        let todays: [GroupMenuItem] = [
            .copyRun, .mintGroup(title: "Mint Long-Lived Tokens for “dev”…"),
            .park(title: "Park group “dev”", parked: false), .removeFromGroup, .deleteGroup,
        ]
        XCTAssertEqual(
            AccountActions.groupItems(
                for: alice, group: "dev", accounts: alone.accounts, controlName: nil),
            todays)
        XCTAssertEqual(
            AccountActions.groupItems(
                for: alice, group: "dev", accounts: alone.accounts,
                controlName: "alice@example.com"),
            [.statement("\u{26A0}\u{FE0E} “dev” routes nothing — control account only")] + todays)
        let optedIn = try decodeFleet([
            canonicalRow(
                "alice@example.com", ["groups": ["dev"], "controlAllowedGroups": ["dev"]])
        ])
        XCTAssertEqual(
            AccountActions.groupItems(
                for: try member("alice@example.com", of: optedIn), group: "dev",
                accounts: optedIn.accounts, controlName: "alice@example.com"),
            [.statement("“dev” may use the control account")] + todays)

        let three = try decodeFleet([
            canonicalRow("alice@example.com", ["groups": ["dev"]]),
            canonicalRow(
                "alice@example.com",
                [
                    "name": "nina@example.com",
                    "orgUuid": "22222222-2222-4222-8222-222222222222",
                    "groups": ["zeta", "dev"], "reservedGroups": ["zeta"],
                ]),
            canonicalRow(
                "alice@example.com",
                [
                    "name": "henry@example.com",
                    "orgUuid": "33333333-3333-4333-8333-333333333333", "groups": ["alpha"],
                ]),
        ])
        XCTAssertEqual(
            AccountActions.addToGroupEntries(
                for: try member("alice@example.com", of: three), accounts: three.accounts),
            [
                AddToGroupEntry(group: "alpha", title: "alpha", isEnabled: true, offersCopy: true),
                AddToGroupEntry(group: "zeta", title: "zeta", isEnabled: true, offersCopy: true),
            ])
    }

    private func member(_ name: String, of fleet: Fleet) throws -> Account {
        try XCTUnwrap(fleet.accounts.first { $0.name == name }, "no \(name) in the fleet")
    }
}

/// The ten canonical rows' names: alice (Claude, in orchard) and nine Codex rows.
private let canonicalNames = [
    "alice@example.com", "bob@example.com/codex", "carol@example.com", "dana@example.com",
    "erin@example.com/codex", "gil@example.com", "hal@example.com", "ivy@example.com",
    "jo@example.com", "kim@example.com",
]

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

/// All ten canonical rows.
private func canonicalFleet() throws -> Fleet {
    try decodeFleet(canonicalNames.map { try canonicalRow($0) })
}

/// Hand-built accounts with the fields this file's assertions touch.
private func menuAccount(
    _ name: String,
    groups: [String]?
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: false,
        quota: 0,
        quotaState: .ok,
        fiveHour: 0,
        sevenDay: 0,
        sevenDayOi: 0,
        held: [],
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheHitRatio: nil,
        probeStatus: .ok,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false,
        groups: groups
    )
}
