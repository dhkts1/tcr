import XCTest

@testable import TcrBarCore

/// ``GroupRouting/routes(group:accounts:controlName:)`` — the panel's copy of a
/// proxy rule: an inference pick never selects the control account, so a group
/// with no other member serves nothing.
///
/// The shape these guard is one that shipped on a live fleet and looked healthy
/// from every surface — one member, a colour, an `active` row — while every
/// request asking for the group was served from the whole pool instead.
///
/// Account names are obviously fake — this repository is public.
final class GroupRoutingTests: XCTestCase {

    /// The defect, on the panel side. `research` has exactly one member and it
    /// is the control account.
    func testAGroupHoldingOnlyTheControlAccountDoesNotRoute() {
        let accounts = [
            routingAccount("gil@example.com", groups: ["research"]),
            routingAccount("worker@example.com", groups: ["dev"]),
        ]
        XCTAssertFalse(
            GroupRouting.routes(
                group: "research", accounts: accounts, controlName: "gil@example.com"))
    }

    /// The remedy the warning points at: one ordinary account joining the group
    /// makes it routable again. Without this, the check could be a constant
    /// `false` for any group the control account is in and still pass the test
    /// above.
    func testASecondOrdinaryMemberMakesTheGroupRoute() {
        let accounts = [
            routingAccount("gil@example.com", groups: ["research"]),
            routingAccount("worker@example.com", groups: ["research"]),
        ]
        XCTAssertTrue(
            GroupRouting.routes(
                group: "research", accounts: accounts, controlName: "gil@example.com"))
    }

    /// An ordinary group is never flagged — the other direction of the same
    /// control, so neither a constant `true` nor a constant `false` survives.
    func testAnOrdinaryGroupRoutes() {
        let accounts = [
            routingAccount("a@example.com", groups: ["dev"]),
            routingAccount("b@example.com", groups: ["dev"]),
        ]
        XCTAssertTrue(
            GroupRouting.routes(group: "dev", accounts: accounts, controlName: "gil@example.com"))
    }

    /// No control account set (or the panel could not resolve one): membership
    /// alone decides, and the same fleet that failed above now routes. The panel
    /// must not invent a block it cannot see.
    func testWithNoControlAccountMembershipAloneDecides() {
        let accounts = [routingAccount("gil@example.com", groups: ["research"])]
        XCTAssertTrue(
            GroupRouting.routes(group: "research", accounts: accounts, controlName: nil))
    }

    /// The opt-in makes a control-only group routable — the panel must not keep
    /// warning about a group the proxy will now happily serve.
    func testAnOptedInControlOnlyGroupRoutes() {
        let accounts = [
            routingAccount("gil@example.com", groups: ["research"], controlAllowed: ["research"])
        ]
        XCTAssertTrue(
            GroupRouting.routes(
                group: "research", accounts: accounts, controlName: "gil@example.com"))
        XCTAssertTrue(GroupRouting.allowsControlAccount(group: "research", accounts: accounts))
    }

    /// Opting in one group must not quietly opt in a different one.
    func testTheOptInIsPerGroup() {
        let accounts = [
            routingAccount("gil@example.com", groups: ["research", "secret"], controlAllowed: ["research"])
        ]
        XCTAssertFalse(GroupRouting.allowsControlAccount(group: "secret", accounts: accounts))
        XCTAssertFalse(
            GroupRouting.routes(group: "secret", accounts: accounts, controlName: "gil@example.com"))
    }

    /// A server too old to send `controlAllowedGroups` reports nil, which must
    /// read as "not opted in" — the pre-feature behaviour, and the safe way to
    /// be wrong: warn about a group that works rather than stay silent about one
    /// that does not.
    func testAMissingFieldReadsAsNotOptedIn() {
        let accounts = [routingAccount("gil@example.com", groups: ["research"], controlAllowed: nil)]
        XCTAssertFalse(GroupRouting.allowsControlAccount(group: "research", accounts: accounts))
        XCTAssertFalse(
            GroupRouting.routes(
                group: "research", accounts: accounts, controlName: "gil@example.com"))
    }

    /// A label nobody carries routes nothing — there is no member to serve it.
    func testALabelNoAccountCarriesDoesNotRoute() {
        let accounts = [routingAccount("a@example.com", groups: ["dev"])]
        XCTAssertFalse(
            GroupRouting.routes(group: "resarch", accounts: accounts, controlName: nil))
    }

    /// Claude routing is judged over the members that can serve a Claude request.
    ///
    /// orchard holds alice, the control account, and ivy, a Codex account. ivy
    /// cannot serve a Claude request, so the group routes nothing. And research,
    /// whose one member is the Codex row kim, has no Claude member at all.
    func testRoutingCountsAnthropicMembersOnly() throws {
        let fleet = try decodeFleet(
            ["alice@example.com", "ivy@example.com", "kim@example.com"].map {
                try canonicalRow($0)
            })
        let alice = try XCTUnwrap(fleet.accounts.first { $0.name == "alice@example.com" })
        let ivy = try XCTUnwrap(fleet.accounts.first { $0.name == "ivy@example.com" })
        let kim = try XCTUnwrap(fleet.accounts.first { $0.name == "kim@example.com" })
        XCTAssertFalse(
            GroupRouting.routes(
                group: "orchard", accounts: [alice, ivy], controlName: "alice@example.com"),
            "a Codex member made a group whose only Claude member is the control account "
                + "look routable")
        XCTAssertFalse(GroupRouting.routes(group: "research", accounts: [kim], controlName: nil))
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

/// Hand-built accounts with the fields this file's assertions touch.
private func routingAccount(
    _ name: String, groups: [String]?, controlAllowed: [String]? = nil
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
        controlAllowedGroups: controlAllowed
    )
}
