import XCTest

@testable import TcrBarCore

/// ``GroupSummary/summarize(_:)`` — every group name the fleet's accounts
/// carry, once each, with the facts the Settings window's Groups & Rotation
/// pane needs. The mockup review's S8 finding is the reason this derives from
/// the FULL account list rather than whatever the panel happens to have drawn
/// (a collapsed group card, or one filtered view): two groups there existed
/// only as bare names with none of their settings shown.
///
/// Account names are obviously fake — this repository is public.
final class GroupSummaryTests: XCTestCase {

    func testEveryGroupNameIsCounted() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["dev"]),
            summaryAccount("b@example.com", groups: ["dev", "gil"]),
            summaryAccount("c@example.com", groups: ["gil"]),
        ]
        let groups = GroupSummary.summarize(accounts)
        XCTAssertEqual(groups.map(\.name), ["dev", "gil"])
        XCTAssertEqual(groups[0].memberCount, 2)
        XCTAssertEqual(groups[1].memberCount, 2)
    }

    /// A group with no member carrying `parkedGroups` for it is not parked —
    /// the negative case, since every other fixture here opts in.
    func testAGroupWithNoParkedMemberIsNotParked() {
        let accounts = [summaryAccount("a@example.com", groups: ["dev"])]
        XCTAssertEqual(GroupSummary.summarize(accounts).first?.isParked, false)
    }

    func testAGroupIsParkedWhenAMemberCarriesIt() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["dev"], parkedGroups: ["dev"])
        ]
        XCTAssertEqual(GroupSummary.summarize(accounts).first?.isParked, true)
    }

    func testAGroupIsReservedWhenAMemberCarriesIt() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["dev"], reservedGroups: ["dev"])
        ]
        XCTAssertEqual(GroupSummary.summarize(accounts).first?.isReserved, true)
    }

    /// The colour comes from the first member that carries one for this
    /// group; a group with no coloured member reports `nil`, the same
    /// neutral-fallback shape ``GroupChip`` already uses.
    func testTheGroupColorIsReadFromAMemberThatCarriesOne() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["dev"]),
            summaryAccount("b@example.com", groups: ["dev"], groupColors: ["dev": "#0a84ff"]),
        ]
        XCTAssertEqual(GroupSummary.summarize(accounts).first?.colorHex, "#0a84ff")
    }

    func testAGroupWithNoColoredMemberReportsNilColor() {
        let accounts = [summaryAccount("a@example.com", groups: ["dev"])]
        XCTAssertNil(GroupSummary.summarize(accounts).first?.colorHex)
    }

    /// Sorted by name, so the pane's row order does not depend on account
    /// order or on `Set` iteration order.
    func testGroupsAreSortedByName() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["zeta"]),
            summaryAccount("b@example.com", groups: ["alpha"]),
        ]
        XCTAssertEqual(GroupSummary.summarize(accounts).map(\.name), ["alpha", "zeta"])
    }

    func testNoGroupsProducesAnEmptyList() {
        XCTAssertEqual(GroupSummary.summarize([summaryAccount("a@example.com", groups: nil)]), [])
    }

    // MARK: - A box per provider

    /// In a fleet holding both providers each group box holds one provider's members, so its
    /// legend count and its collapsed line cover that pool alone. A Codex box never prints
    /// `$0.00`: tokens are the Codex default, and a measured zero or a total that rounds to
    /// nothing says no more than the account count does.
    func testEachBoxCoversOneProvider() throws {
        let lists = try XCTUnwrap(try canonicalFleet().accountsTabSectionsByProvider(expandedGroups: []))

        let claudeOrchard = try box(in: lists, .anthropic, "orchard")
        XCTAssertEqual(claudeOrchard.legendText, "ORCHARD · ACTIVE · 1")
        XCTAssertEqual(claudeOrchard.collapsedSummaryLine, "1 account · $14.2 today")

        let codexOrchard = try box(in: lists, .codex, "orchard")
        XCTAssertEqual(codexOrchard.legendText, "ORCHARD · ACTIVE · 1")
        XCTAssertEqual(codexOrchard.collapsedSummaryLine, "1 account")

        let research = try box(in: lists, .codex, "research")
        XCTAssertEqual(research.legendText, "RESEARCH · ACTIVE · 1")
        XCTAssertEqual(
            research.collapsedSummaryLine, "1 account", "kim's measured 0.0 printed as dollars")

        // kim priced at 0.004: a total `QuotaFormat.usd` writes as `$0.00`.
        var usage = try XCTUnwrap(try canonicalRow("kim@example.com")["usage"] as? [String: Any])
        var today = try XCTUnwrap(usage["today"] as? [String: Any])
        today["costUsd"] = 0.004
        usage["today"] = today
        let priced = try decodeFleet([
            try canonicalRow("alice@example.com"),
            try canonicalRow("kim@example.com", ["usage": usage]),
        ])
        let pricedBox = try box(
            in: try XCTUnwrap(priced.accountsTabSectionsByProvider(expandedGroups: [])), .codex,
            "research")
        XCTAssertEqual(pricedBox.collapsedSummaryLine, "1 account")
        XCTAssertEqual(
            FleetSection(
                band: pricedBox.band, group: pricedBox.group, rows: pricedBox.rows,
                provider: .anthropic
            ).collapsedSummaryLine,
            "1 account · $0.00 today",
            "positive control: the same rows in a Claude box print that total as $0.00")
    }

    /// A Codex box's tally says the word its own lines say: ChatGPT refused the row, where
    /// Anthropic rejected a Claude one. A 403 hold on a Codex row is counted the same way.
    func testTheRefusedAndUnreachableTallyWords() throws {
        let lists = try XCTUnwrap(try canonicalFleet().accountsTabSectionsByProvider(expandedGroups: []))
        let codex = try XCTUnwrap(lists.first { $0.provider == .codex }).sections
        let loose = try XCTUnwrap(codex.first { $0.band == .live && $0.group == .ungrouped })
        XCTAssertEqual(
            loose.breakdown.map { loose.tallyLabel($0) }, ["3 ok", "1 need re-login", "2 refused"])
        let research = try box(in: lists, .codex, "research")
        XCTAssertEqual(research.breakdown.map { research.tallyLabel($0) }, ["1 unreachable"])

        let rejected = FleetTally(kind: .rejected, count: 1)
        XCTAssertEqual(
            FleetSection(band: .live, group: .ungrouped, rows: [], provider: .anthropic)
                .tallyLabel(rejected),
            "1 rejected")
        XCTAssertEqual(
            FleetSection(band: .live, group: .ungrouped, rows: [], provider: .codex)
                .tallyLabel(rejected),
            "1 refused")

        let held = try decodeFleet([
            try canonicalRow(
                "bob@example.com/codex",
                [
                    "gate": "refused", "freeAtMs": 1_767_225_690_000,
                    "rateLimitedUntilMs": 1_767_225_690_000, "secondsUntilFree": 90,
                ])
        ])
        let heldBox = try XCTUnwrap(
            held.accountsTabSectionsByProvider(expandedGroups: [])?.first?.sections.first)
        XCTAssertEqual(heldBox.breakdown.map { heldBox.tallyLabel($0) }, ["1 refused"])
    }

    /// The Settings pane's group rows can say which providers a group spans, and a Claude-only
    /// fleet's summary is the value it always was.
    func testAGroupCountsItsMembersPerProvider() throws {
        let groups = GroupSummary.summarize(try canonicalFleet().accounts)
        let orchard = try XCTUnwrap(groups.first { $0.name == "orchard" })
        XCTAssertEqual(orchard.memberCount, 2)
        XCTAssertEqual(orchard.codexMemberCount, 1)
        XCTAssertEqual(orchard.claudeMemberCount, 1)
        XCTAssertTrue(orchard.spansBothProviders)

        let research = try XCTUnwrap(groups.first { $0.name == "research" })
        XCTAssertEqual(research.memberCount, 1)
        XCTAssertEqual(research.codexMemberCount, 1)
        XCTAssertEqual(research.claudeMemberCount, 0)
        XCTAssertFalse(research.spansBothProviders)

        let claudeOnly = GroupSummary.summarize(
            try decodeFleet([try canonicalRow("alice@example.com")]).accounts)
        XCTAssertEqual(claudeOnly.first?.codexMemberCount, 0)
        XCTAssertEqual(claudeOnly.first?.claudeMemberCount, 1)
        XCTAssertEqual(
            claudeOnly,
            [
                GroupSummary(
                    name: "orchard", memberCount: 1, isParked: false, isReserved: false,
                    colorHex: "#30d158")
            ])
    }

    /// The box for `group` in `provider`'s list.
    private func box(
        in lists: [ProviderSections], _ provider: Provider, _ group: String
    ) throws -> FleetSection {
        let list = try XCTUnwrap(lists.first { $0.provider == provider }, "no \(provider.token) list")
        return try XCTUnwrap(
            list.sections.first { $0.group == .named(group) }, "no \(group) box in \(provider.token)")
    }
}

/// Hand-built accounts with the fields this file's assertions touch —
/// mirrors `GroupRoutingTests`'s own `routingAccount` fixture.
private func summaryAccount(
    _ name: String,
    groups: [String]?,
    reservedGroups: [String]? = nil,
    parkedGroups: [String]? = nil,
    groupColors: [String: String]? = nil
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
        groups: groups,
        reservedGroups: reservedGroups,
        parkedGroups: parkedGroups,
        groupColors: groupColors
    )
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
