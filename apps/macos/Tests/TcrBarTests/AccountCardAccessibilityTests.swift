import XCTest

@testable import TcrBarCore

/// The account card's accessible surface: what VoiceOver announces on arriving
/// at it, and the sentences behind its pills.
///
/// Before this the card was `.accessibilityElement(children: .contain)` with no
/// label at all. A container with no accessible name cannot take focus, so a
/// user stepping through the panel was told nothing about which account they had
/// reached and had to walk roughly eight stops — name, each pill, the plan line,
/// each bar — to find out.
///
/// Account names are obviously fake; this repository is public.
final class AccountCardAccessibilityTests: XCTestCase {

    private let now = Date(timeIntervalSince1970: 1_757_000_000)

    private func resetIn(minutes: Int) -> Int64 {
        Int64((now.timeIntervalSince1970 + Double(minutes) * 60) * 1000)
    }

    // MARK: - The card's one-sentence summary

    func testTheSummaryNamesTheAccountItsStateAndItsWindows() {
        let account = cardAccount(
            "alice@example.com", plan: "Max 20x", quota: 0.12,
            fiveHour: 0.12, fiveHourState: .ok, fiveHourResetAtMs: resetIn(minutes: 130),
            sevenDay: 0.30, sevenDayState: .ok)
        XCTAssertEqual(
            account.cardSummaryLabel(now: now),
            "alice@example.com, Max 20x, ready, "
                + "5h 12% used, within limit, resets 2h 10m, 7d 30% used, within limit")
    }

    /// A card with no fable window says nothing about one.
    func testTheSummaryOmitsTheFableWindowWhenThereIsNone() {
        let account = cardAccount("bob@example.com", quota: 0.5, fiveHour: 0.5, sevenDay: 0.5)
        XCTAssertFalse(account.cardSummaryLabel(now: now).contains("fable"))
    }

    func testTheSummaryNamesTheFableWindowWhenThereIsOne() {
        let account = cardAccount(
            "carol@example.com", quota: 0.5, fiveHour: 0.5, sevenDay: 0.5,
            sevenDayOi: 0.71, sevenDayOiState: .near)
        XCTAssertTrue(
            account.cardSummaryLabel(now: now).hasSuffix("fable 71% used, near the limit"))
    }

    /// The summary may not claim a state the card's own pill does not draw.
    func testTheSummarySpeaksTheSameStateTheStatePillDraws() {
        let parked = cardAccount(
            "dave@example.com", quota: 0.1, fiveHour: 0.1, sevenDay: 0.1,
            groups: ["codereview"], parkedGroups: ["codereview"])
        XCTAssertEqual(FleetTally.Kind(account: parked), .disabled)
        XCTAssertTrue(
            parked.cardSummaryLabel(now: now).contains("parked"),
            "the pill reads PARKED; the summary must not say ready")
        XCTAssertFalse(parked.cardSummaryLabel(now: now).contains("rotating"))
    }

    /// Silence, spoken. The card draws no pool pill on a healthy account, so
    /// the summary says no pool word either: a listener told "rotating" over a
    /// card that shows nothing is told something no sighted reader is, and the
    /// summary's whole rule is that it may not claim what the card does not
    /// draw. The state word right after the plan ("ready") is still spoken.
    func testTheSummarySpeaksNoPoolWordWhereTheCardDrawsNoPoolPill() {
        let healthy = cardAccount(
            "mira@example.com", plan: "Max 20x", quota: 0.12, fiveHour: 0.12,
            fiveHourState: .ok, sevenDay: 0.30, sevenDayState: .ok)
        let summary = healthy.cardSummaryLabel(now: now)
        XCTAssertEqual(healthy.rotation, .rotating)
        XCTAssertFalse(
            summary.contains("rotating"),
            "the card draws no ROTATING pill; the summary must not speak one: \(summary)")
        XCTAssertTrue(summary.contains("Max 20x, ready"))
    }

    /// The reserved account is the other arm: its pill is drawn, so its word
    /// is spoken.
    func testTheSummarySpeaksTheReservedWordTheCardStillDraws() {
        let reserved = cardAccount(
            "nadia@example.com", quota: 0.2, fiveHour: 0.2, sevenDay: 0.2,
            groups: ["research"], reservedGroups: ["research"])
        XCTAssertTrue(reserved.cardSummaryLabel(now: now).contains("group only"))
    }

    /// An unmeasured window is left out of the summary rather than spoken as a
    /// zero — the same rule the row itself follows.
    func testAnUnmeasuredWindowIsNotSpokenAsAReading() {
        let account = cardAccount(
            "erin@example.com", quota: 0.98, fiveHour: nil, sevenDay: 0.98,
            sevenDayState: .spent)
        let summary = account.cardSummaryLabel(now: now)
        XCTAssertFalse(summary.contains("5h"), "there is no 5h reading to speak")
        XCTAssertTrue(summary.contains("7d 98% used, spent"))
    }

    /// The control account's card says so in the same sentence as its plan,
    /// before the state word, the order its `CONTROL` pill draws in.
    func testTheSummaryNamesTheControlAccount() {
        let account = cardAccount(
            "kate@example.com", plan: "Max 20x", quota: 0.12,
            fiveHour: 0.12, fiveHourState: .ok, sevenDay: 0.30, sevenDayState: .ok)
        XCTAssertEqual(
            account.cardSummaryLabel(now: now, isControl: true),
            "kate@example.com, Max 20x, control account, ready, "
                + "5h 12% used, within limit, 7d 30% used, within limit")
    }

    /// An account that is not the control says nothing about one — the
    /// default the existing tests above already exercise, named explicitly so
    /// a future change to the default cannot slip past unnoticed.
    func testAnOrdinaryAccountsSummaryOmitsControl() {
        let account = cardAccount("liam@example.com", quota: 0.5, fiveHour: 0.5, sevenDay: 0.5)
        XCTAssertFalse(account.cardSummaryLabel(now: now).contains("control"))
    }

    // MARK: - Which provider a card serves

    /// A fleet with no Codex account builds today's string: the third parameter
    /// defaults to `false`, and `false` changes nothing.
    func testNamesProviderDefaultsToFalseAndBuildsTodaysString() throws {
        let accounts = [
            try canonical("alice@example.com"),
            cardAccount(
                "alice@example.com", plan: "Max 20x", quota: 0.12,
                fiveHour: 0.12, fiveHourState: .ok, fiveHourResetAtMs: resetIn(minutes: 130),
                sevenDay: 0.30, sevenDayState: .ok),
            cardAccount("bob@example.com", quota: 0.5, fiveHour: 0.5, sevenDay: 0.5),
            cardAccount(
                "carol@example.com", quota: 0.5, fiveHour: 0.5, sevenDay: 0.5,
                sevenDayOi: 0.71, sevenDayOiState: .near),
            cardAccount(
                "dave@example.com", quota: 0.1, fiveHour: 0.1, sevenDay: 0.1,
                groups: ["codereview"], parkedGroups: ["codereview"]),
            cardAccount(
                "mira@example.com", plan: "Max 20x", quota: 0.12, fiveHour: 0.12,
                fiveHourState: .ok, sevenDay: 0.30, sevenDayState: .ok),
            cardAccount(
                "nadia@example.com", quota: 0.2, fiveHour: 0.2, sevenDay: 0.2,
                groups: ["research"], reservedGroups: ["research"]),
            cardAccount(
                "erin@example.com", quota: 0.98, fiveHour: nil, sevenDay: 0.98,
                sevenDayState: .spent),
            cardAccount(
                "kate@example.com", plan: "Max 20x", quota: 0.12,
                fiveHour: 0.12, fiveHourState: .ok, sevenDay: 0.30, sevenDayState: .ok),
            cardAccount("liam@example.com", quota: 0.5, fiveHour: 0.5, sevenDay: 0.5),
            cardAccount(
                "frank@example.com", quota: 0.1, fiveHour: 0.1, sevenDay: 0.1,
                groups: ["codereview"], parkedGroups: ["codereview"]),
            cardAccount("gina@example.com", disabled: true),
            cardAccount("hank@example.com", status: "error"),
            cardAccount("iris@example.com", quota: 0.12, fiveHour: 0.12, sevenDay: 0.12),
            cardAccount(
                "jane@example.com", quota: 0.1, fiveHour: 0.1, sevenDay: 0.1,
                groups: ["research"], reservedGroups: ["research"]),
        ]
        for account in accounts {
            XCTAssertEqual(
                account.cardSummaryLabel(now: now),
                account.cardSummaryLabel(now: now, isControl: false, namesProvider: false),
                account.name)
            XCTAssertEqual(
                account.cardSummaryLabel(now: now, isControl: true),
                account.cardSummaryLabel(now: now, isControl: true, namesProvider: false),
                account.name)
        }
    }

    /// In a fleet that also holds Codex accounts, a Claude card says which provider
    /// it serves, right after its name, and nothing else in it moves.
    func testAClaudeCardInAMixedFleetSaysClaudeAccount() throws {
        let alice = try canonical("alice@example.com")
        let today = alice.cardSummaryLabel(now: fixtureNow)
        let name = try XCTUnwrap(today.range(of: "alice@example.com, "))
        XCTAssertEqual(
            alice.cardSummaryLabel(now: fixtureNow, namesProvider: true),
            today.replacingCharacters(in: name, with: "alice@example.com, Claude account, "))
    }

    func testTheCodexCardLabels() throws {
        let expected: [(String, [String: Any], String)] = [
            (
                "bob@example.com/codex", [:],
                "bob@example.com/codex, Codex account, ChatGPT Pro, ready, weekly window only, "
                    + "7d 34% used, within limit, resets 4d"
            ),
            (
                "jo@example.com", [:],
                "jo@example.com, Codex account, ChatGPT Business, ready, no usage windows "
                    + "reported, credits unlimited"
            ),
            (
                "gil@example.com", [:],
                "gil@example.com, Codex account, ChatGPT Team, refused by ChatGPT, 5h 15% used, "
                    + "within limit, resets 3h 2m, 7d 10% used, within limit, resets 6d"
            ),
            (
                "kim@example.com", [:],
                "kim@example.com, Codex account, ChatGPT Pro, unreachable, weekly window only, "
                    + "7d 5% used, within limit, resets 6d"
            ),
            (
                "carol@example.com", [:],
                "carol@example.com, Codex account, ChatGPT Pro, spent, weekly window only, "
                    + "7d 100% used, spent, resets 3d 4h, credits 12.50"
            ),
            (
                "bob@example.com/codex", ["sevenDayWindowMinutes": 43200],
                "bob@example.com/codex, Codex account, ChatGPT Pro, ready, 30d window only, "
                    + "30d 34% used, within limit, resets 4d"
            ),
            (
                "bob@example.com/codex",
                [
                    "gate": "refused", "freeAtMs": 1_767_225_690_000,
                    "rateLimitedUntilMs": 1_767_225_690_000, "secondsUntilFree": 90,
                ],
                "bob@example.com/codex, Codex account, ChatGPT Pro, refused by ChatGPT, weekly "
                    + "window only, 7d 34% used, within limit, resets 4d"
            ),
        ]
        for (name, overrides, label) in expected {
            let account = try canonical(name, overrides)
            XCTAssertEqual(account.cardSummaryLabel(now: fixtureNow), label, name)
            XCTAssertEqual(
                account.cardSummaryLabel(now: fixtureNow, namesProvider: true), label,
                "a Codex card says the same whatever the fleet holds: \(name)")
        }
    }

    // MARK: - The sentences behind the pills (review #13)

    /// A PARKED pill names a state. Without its help it names nothing the
    /// operator can do about it.
    func testTheParkedPillNamesTheGroupAndTheRemedy() throws {
        let parked = cardAccount(
            "frank@example.com", quota: 0.1, fiveHour: 0.1, sevenDay: 0.1,
            groups: ["codereview"], parkedGroups: ["codereview"])
        let help = try XCTUnwrap(parked.stateHelp)
        XCTAssertTrue(help.contains("codereview"), "the help does not say WHICH group is parked")
        XCTAssertTrue(
            help.contains("tcr group unpark codereview"),
            "the help does not carry the remedy the pre-v4 row already wrote")
    }

    func testTheDisabledPillNamesTheCommandThatUndoesIt() throws {
        let disabled = cardAccount("gina@example.com", disabled: true)
        let help = try XCTUnwrap(disabled.stateHelp)
        XCTAssertTrue(help.contains("tcr enable gina@example.com"))
    }

    func testABrokenCredentialSaysNoSweepWillFixIt() throws {
        let broken = cardAccount("hank@example.com", status: "error")
        let help = try XCTUnwrap(broken.stateHelp)
        XCTAssertTrue(help.contains("Re-login"))
    }

    /// `OK` and `NEAR` are already the whole sentence. A tooltip restating the
    /// word is noise on every card in the fleet.
    func testAHealthyAccountHasNoStateHelp() {
        XCTAssertNil(
            cardAccount("iris@example.com", quota: 0.12, fiveHour: 0.12, sevenDay: 0.12)
                .stateHelp)
    }

    /// `Group only` is not a phrase an operator meets anywhere else.
    func testTheGroupOnlyPillExplainsItself() throws {
        let reserved = cardAccount(
            "jane@example.com", quota: 0.1, fiveHour: 0.1, sevenDay: 0.1,
            groups: ["research"], reservedGroups: ["research"])
        let help = try XCTUnwrap(reserved.rotationHelp)
        XCTAssertTrue(help.contains("research"))
        XCTAssertTrue(help.contains("no pool traffic"))
    }
}

/// 2026-01-01T00:00:00Z: the clock every canonical timestamp is written against.
private let fixtureNow = Date(timeIntervalSince1970: 1_767_225_600)

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

private func cardAccount(
    _ name: String,
    status: String = "active",
    disabled: Bool = false,
    plan: String? = nil,
    quota: Double? = nil,
    fiveHour: Double? = nil,
    fiveHourState: QuotaState? = nil,
    fiveHourResetAtMs: Int64? = nil,
    sevenDay: Double? = nil,
    sevenDayState: QuotaState? = nil,
    sevenDayOi: Double? = nil,
    sevenDayOiState: QuotaState? = nil,
    groups: [String]? = nil,
    reservedGroups: [String]? = nil,
    parkedGroups: [String]? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: status,
        disabled: disabled,
        quota: quota,
        quotaState: .ok,
        fiveHour: fiveHour,
        sevenDay: sevenDay,
        sevenDayOi: sevenDayOi,
        fiveHourState: fiveHourState,
        sevenDayState: sevenDayState,
        sevenDayOiState: sevenDayOiState,
        fiveHourResetAtMs: fiveHourResetAtMs,
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
        plan: plan
    )
}
