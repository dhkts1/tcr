import XCTest

@testable import TcrBarCore

/// What the v4 account card is allowed to claim about an account.
///
/// The card draws two pills from two model properties and nothing else:
/// ``FleetTally/Kind/init(account:)`` for the state pill and
/// ``Account/rotationLabel`` for the one beside it. Both used to overclaim, in
/// ways the fleet's own summary then contradicted:
///
///  - Inside one box legended PARKED, two equally idle accounts were labelled
///    `OK` and `PARKED`, and the headline counted the `OK` one as capacity.
///  - A card read `ROTATING` next to `NEEDS RE-LOGIN` — "traffic is landing
///    here" over an account serving none.
///  - An account Anthropic itself had rejected drew a lone green `OK`: the
///    tally had no bucket for the gate the pre-v4 row has drawn for months.
///
/// Every assertion below is a spoken or printed STRING, because that is the
/// surface: `swift test` links `TcrBarCore` only, so the card itself cannot be
/// instantiated here — but every word it draws comes from these two properties
/// and they can.
///
/// Account names are obviously fake; this repository is public.
final class AccountCardStateTests: XCTestCase {

    // MARK: - The rotation pill (review #7)

    /// The state is real; the pill is not. A healthy ungrouped account IS
    /// rotating and every exclusion is already `nil`, so drawing the word here
    /// put it on every healthy card at once, where it separated none of them
    /// and spent a pill's width on the card's only identifying line.
    func testAHealthyUngroupedAccountRotatesAndSaysSoWithNoPill() {
        let healthy = stateAccount("alice@example.com")
        XCTAssertEqual(healthy.rotation, .rotating)
        XCTAssertNil(
            healthy.rotationLabel,
            "the ordinary rotating account draws no pool pill; a card with no pool word IS "
                + "the card the pool is sending traffic to")
        XCTAssertNil(
            healthy.rotationHelp,
            "a sentence behind a pill nothing draws is a sentence no hover can reach")
    }

    /// The other half of the same rule, in one test so the two cannot be
    /// updated apart: the rare state keeps its word.
    func testTheReservedStateKeepsItsPillWhileTheRotatingOneDoesNot() {
        let reserved = stateAccount(
            "irene@example.com", groups: ["research"], reservedGroups: ["research"])
        XCTAssertNil(stateAccount("ivan@example.com").rotationLabel)
        XCTAssertEqual(reserved.rotationLabel, "Group only")
    }

    func testADeadCredentialNeverClaimsToBeRotating() {
        let broken = stateAccount("bob@example.com", status: "error")
        XCTAssertEqual(broken.health, .needsRelogin)
        XCTAssertNil(
            broken.rotationLabel,
            "the card drew ROTATING beside NEEDS RE-LOGIN, which reads as "
                + "\"traffic is landing here\" over an account serving none")
    }

    func testAnAccountRejectedByAnthropicNeverClaimsToBeRotating() {
        let rejected = stateAccount("carol@example.com", gate: .rejected)
        XCTAssertTrue(rejected.isRejected)
        XCTAssertNil(rejected.rotationLabel)
    }

    func testAParkedGroupMemberNeverClaimsToBeRotating() {
        let parked = stateAccount(
            "dave@example.com", groups: ["codereview"], parkedGroups: ["codereview"])
        XCTAssertTrue(parked.isParkedByGroup)
        XCTAssertNil(parked.rotationLabel)
    }

    func testAReservedAccountSaysGroupOnlyRatherThanRotating() {
        let reserved = stateAccount(
            "erin@example.com", groups: ["codereview"], reservedGroups: ["codereview"])
        XCTAssertTrue(reserved.servesGroupTrafficOnly)
        XCTAssertEqual(
            reserved.rotationLabel, "Group only",
            "a reserved account serves no pool traffic; the pre-v4 row has said "
                + "\"group only\" here since the reserved state shipped")
    }

    func testADisabledAccountDrawsNoRotationPill() {
        XCTAssertNil(stateAccount("frank@example.com", disabled: true).rotationLabel)
    }

    /// The two states stay distinguishable as CASES even though only one of
    /// them has a word: a caller that needs to know whether the pool is
    /// sending traffic asks `rotation`, never the pill string it used to
    /// compare against.
    func testTheTwoRotationStatesAreDistinguishableWithoutReadingTheirWords() {
        let reserved = stateAccount(
            "gwen@example.com", groups: ["research"], reservedGroups: ["research"])
        XCTAssertEqual(reserved.rotation, .groupOnly)
        XCTAssertEqual(stateAccount("hal@example.com").rotation, .rotating)
        XCTAssertNil(RotationState.rotating.label)
        XCTAssertEqual(RotationState.groupOnly.label, "Group only")
    }

    // MARK: - The state pill (review #7, #8)

    func testAParkedGroupMemberIsParkedNotOK() {
        let parked = stateAccount(
            "gina@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
            quota: 0.1, probeStatus: .ok)
        XCTAssertEqual(
            FleetTally.Kind(account: parked), .disabled,
            "a member of a parked group serves nothing; drawing OK over it is "
                + "the capacity overclaim the PARKED legend contradicts")
        XCTAssertEqual(FleetTally.Kind(account: parked).token, "disabled")
    }

    func testARejectedAccountIsItsOwnBucketRatherThanOK() {
        let rejected = stateAccount(
            "hank@example.com", gate: .rejected, quota: 0.1, probeStatus: .ok)
        XCTAssertEqual(
            FleetTally.Kind(account: rejected), .rejected,
            "a rejected account keeps its last-learned .ok quota state, so "
                + "without its own bucket it draws a lone green OK")
        XCTAssertEqual(FleetTally.Kind(account: rejected).token, "rejected")
        XCTAssertEqual(FleetTally.Kind(account: rejected).phrase, "rejected by Anthropic")
    }

    /// Every word the card's state pill can draw, pinned, because the pill
    /// text moved out of the card into ``FleetTally/Kind/pillText`` and a move
    /// that only compiles proves no word survived it. The three registers
    /// differ on purpose (`Parked` / `parked` / `disabled`), so this also
    /// checks the pill did not collapse into either of the others.
    func testEveryKindDrawsItsOwnPillWord() {
        let expected: [(FleetTally.Kind, String)] = [
            (.ok, "OK"),
            (.near, "Near"),
            (.spent, "Spent"),
            (.unknown, "Unknown"),
            (.needsRelogin, "Needs re-login"),
            (.rejected, "Rejected"),
            (.unmeasured, "Unmeasured"),
            (.disabled, "Parked"),
        ]
        for (kind, word) in expected {
            XCTAssertEqual(kind.pillText, word, "the pill for \(kind.token)")
        }
        XCTAssertNotEqual(FleetTally.Kind.disabled.pillText, FleetTally.Kind.disabled.token)
        XCTAssertNotEqual(FleetTally.Kind.disabled.pillText, FleetTally.Kind.disabled.phrase)
    }

    // MARK: - Codex cards

    /// Every state a Codex card can be in has its own pill word, read from the same
    /// classifier the tally and the strip read: the nine canonical Codex rows, and one
    /// before its first probe.
    func testEachCodexCardStateHasItsPill() throws {
        let expected: [(String, [String: Any], FleetTally.Kind, String)] = [
            ("bob@example.com/codex", [:], .ok, "OK"),
            ("carol@example.com", [:], .spent, "Spent"),
            ("dana@example.com", [:], .ok, "OK"),
            ("erin@example.com/codex", [:], .needsRelogin, "Needs re-login"),
            ("gil@example.com", [:], .rejected, "Refused"),
            ("hal@example.com", [:], .rejected, "Refused"),
            ("ivy@example.com", [:], .near, "Near"),
            ("jo@example.com", [:], .ok, "OK"),
            ("kim@example.com", [:], .unreachable, "Unreachable"),
            (
                "bob@example.com/codex",
                ["quota": NSNull(), "quotaWindows": NSNull(), "probeStatus": "never"],
                .unmeasured, "Unmeasured"
            ),
        ]
        for (name, overrides, kind, word) in expected {
            let account = try canonical(name, overrides)
            XCTAssertEqual(FleetTally.Kind(account: account), kind, name)
            XCTAssertEqual(
                FleetTally.Kind(account: account).pillText(for: account.provider), word, name)
        }
    }

    /// A Codex account in a reserved group cannot be reached by any Codex request, so
    /// its UNREACHABLE pill is its whole story. A second `Group only` pill would say it
    /// serves its group's requests, and no Codex request carries a group. The same
    /// group facts on a Claude account keep their pill, which is true there.
    func testAnUnreachableCodexCardDrawsOnePill() throws {
        let kim = try canonical("kim@example.com")
        XCTAssertTrue(kim.servesGroupTrafficOnly)
        XCTAssertNil(kim.rotation, "kim draws a Group only pill beside UNREACHABLE")

        let alice = try canonical(
            "alice@example.com", ["groups": ["research"], "reservedGroups": ["research"]])
        XCTAssertEqual(alice.rotation, .groupOnly)
    }

    /// The Claude pill words are the register main pins in
    /// ``testEveryKindDrawsItsOwnPillWord``, untouched by the Codex word: the one
    /// word main's list does not hold is checked here, and the Claude-provider form
    /// is the register itself for every kind.
    func testEveryClaudePillWordIsTodays() {
        XCTAssertEqual(FleetTally.Kind.unreachable.pillText, "Unreachable")
        let kinds: [FleetTally.Kind] = [
            .ok, .near, .spent, .unknown, .needsRelogin, .rejected, .unmeasured, .disabled,
            .unreachable,
        ]
        for kind in kinds {
            XCTAssertEqual(kind.pillText(for: .anthropic), kind.pillText, kind.token)
        }
    }

    func testADeadCredentialStillOutranksAParkedGroupsQuotaCases() {
        let broken = stateAccount("iris@example.com", status: "error", quota: 0.1, probeStatus: .ok)
        XCTAssertEqual(FleetTally.Kind(account: broken), .needsRelogin)
    }

    func testAnOperatorDisabledAccountIsStillParked() {
        XCTAssertEqual(
            FleetTally.Kind(account: stateAccount("jane@example.com", disabled: true)), .disabled)
    }

    // MARK: - The summary counts the same buckets the pills draw (review #8)

    /// The fleet in finding #8: four accounts, two of which can serve nothing.
    /// The summary used to read "3 ready · 1 parked" because the loop ran over
    /// `enabledAccounts` and then ASSIGNED `counts[.disabled]`, overwriting
    /// whatever the classifier had put there.
    func testSentenceBreakdownCountsAParkedGroupMemberAsParked() {
        let fleet = Fleet(accounts: [
            stateAccount("ken@example.com", quota: 0.1, probeStatus: .ok),
            stateAccount("lena@example.com", quota: 0.1, probeStatus: .ok),
            stateAccount("mia@example.com", disabled: true),
            stateAccount(
                "nick@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
                quota: 0.1, probeStatus: .ok),
        ])
        XCTAssertEqual(
            fleet.sentenceBreakdown.map(\.sentenceLabel), ["2 ready", "2 parked"],
            "two of these four serve nothing; the line used to claim three ready")
        XCTAssertEqual(
            fleet.sentenceBreakdown.map(\.count).reduce(0, +), fleet.accounts.count,
            "every account lands in exactly one bucket, so the buckets sum")
    }

    func testBreakdownCountsAParkedGroupMemberAsParked() {
        let fleet = Fleet(accounts: [
            stateAccount("opal@example.com", quota: 0.1, probeStatus: .ok),
            stateAccount(
                "pete@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
                quota: 0.1, probeStatus: .ok),
        ])
        XCTAssertEqual(fleet.breakdownLabel, "1 ok · 1 disabled")
    }

    func testSentenceBreakdownNamesARejectedAccount() {
        let fleet = Fleet(accounts: [
            stateAccount("quinn@example.com", quota: 0.1, probeStatus: .ok),
            stateAccount("rita@example.com", gate: .rejected, quota: 0.1, probeStatus: .ok),
        ])
        XCTAssertEqual(
            fleet.sentenceBreakdown.map(\.sentenceLabel), ["1 ready", "1 rejected by Anthropic"],
            "the rejected account used to be counted as the second ready one")
    }

    /// A section's own tally is the collapsed group's only visible content, so
    /// it runs through the same classifier — never a second one.
    func testASectionTallyUsesTheSameClassifier() {
        let fleet = Fleet(accounts: [
            stateAccount(
                "sam@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
                quota: 0.1, probeStatus: .ok),
            stateAccount(
                "tess@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
                quota: 0.1, probeStatus: .ok),
        ])
        let sections = fleet.sectionsInDisplayOrder()
        XCTAssertEqual(
            sections.first { $0.title == "codereview" }?.breakdown.map(\.label),
            ["2 disabled"])
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

/// The canonical row, decoded, failing the test if it does not decode.
private func canonical(_ name: String, _ overrides: [String: Any] = [:]) throws -> Account {
    let fleet = try decodeFleet([try canonicalRow(name, overrides)])
    XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
    return try XCTUnwrap(fleet.accounts.first)
}

private func stateAccount(
    _ name: String,
    status: String = "active",
    disabled: Bool = false,
    groups: [String]? = nil,
    reservedGroups: [String]? = nil,
    parkedGroups: [String]? = nil,
    gate: GateReason? = nil,
    quota: Double? = nil,
    probeStatus: ProbeState = .never
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: status,
        disabled: disabled,
        quota: quota,
        quotaState: .ok,
        fiveHour: quota,
        sevenDay: quota,
        sevenDayOi: nil,
        held: [],
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheHitRatio: nil,
        probeStatus: probeStatus,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false,
        groups: groups,
        reservedGroups: reservedGroups,
        parkedGroups: parkedGroups,
        gate: gate
    )
}
