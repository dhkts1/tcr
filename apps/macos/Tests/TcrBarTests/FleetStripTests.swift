import XCTest

@testable import TcrBarCore

/// The strip at the top of the Accounts tab (option H, unit H3): the count
/// headline, "next back in", and one segment per account in the order the
/// tab draws its rows.
///
/// Every countdown is measured from a fixed `now`, so each assertion is about
/// the rule and never about the clock. Account names are obviously fake: this
/// repository is public.
final class FleetStripTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_800_000_000)

    /// Unix milliseconds `minutes` after ``now``.
    private func at(_ minutes: Int) -> Int64 {
        Int64((now.timeIntervalSince1970 + Double(minutes) * 60) * 1000)
    }

    /// Option H's own fleet in miniature: two live, two out of tokens, one
    /// parked, one never measured and parked.
    private var mixed: Fleet {
        Fleet(accounts: [
            stripAccount("bob@example.com", fiveHour: 0.1),
            stripAccount(
                "alice@example.com", quotaState: .near, fiveHour: 0.9, fiveHourState: .near),
            stripAccount(
                "frank@example.com", quotaState: .spent, fiveHour: 1.0,
                held: [hold("5h", at(72))]),
            stripAccount(
                "grace@example.com", quotaState: .spent, fiveHour: 1.0,
                held: [hold("5h", at(42))]),
            stripAccount("niaj@example.com", disabled: true, fiveHour: 0.2),
            stripAccount(
                "mallory@example.com", disabled: true, quota: nil, probe: .never, fiveHour: nil),
        ])
    }

    private func strip(_ fleet: Fleet, expanded: Set<String> = []) -> FleetStrip {
        FleetStrip(
            drawnRows: fleet.accountsTabSections(expandedGroups: expanded)
                .rowsInDrawOrder(expandedGroups: expanded, parkedCap: 2) { _ in false },
            now: now)
    }

    // MARK: - Headline

    /// N is the live band, M every account: a count, no plan weighting.
    func testHeadlineCountsTheLiveBandOfAllAccounts() {
        let strip = strip(mixed)
        XCTAssertEqual(strip.canTakeWork, 2)
        XCTAssertEqual(strip.total, 6)
        XCTAssertEqual(strip.headline, "2 of 6 can take work now")
    }

    /// An account whose login is dead sits in the live band (it is neither
    /// spent nor parked) but the router sends it nothing, so it is not
    /// capacity: `FleetTally.Kind.needsRelogin` exists for exactly this. The
    /// headline read "2 of 2 can take work now" over a fleet whose second
    /// line said re-login.
    func testAnAccountThatNeedsReloginIsNotCountedAsTakingWork() {
        let fleet = Fleet(accounts: [
            stripAccount("alice@example.com"),
            stripAccount("dave@example.com", fiveHour: nil, status: "error"),
        ])
        XCTAssertEqual(strip(fleet).headline, "1 of 2 can take work now")
        XCTAssertFalse(strip(fleet).isOutOfCapacity)
    }

    /// "0 of 3 can take work now" drew in the ready green.
    func testAFleetWithNothingToServeIsOutOfCapacity() {
        let fleet = Fleet(accounts: [
            stripAccount("frank@example.com", quotaState: .spent, fiveHour: 1.0),
            stripAccount("dave@example.com", fiveHour: nil, status: "error"),
        ])
        XCTAssertEqual(strip(fleet).headline, "0 of 2 can take work now")
        XCTAssertTrue(strip(fleet).isOutOfCapacity)
        XCTAssertFalse(strip(mixed).isOutOfCapacity)
    }

    /// The same figure as the out-of-tokens heading's "first back in".
    func testNextBackIsTheSoonestFirstBack() {
        let fleet = mixed
        let heading = fleet.sectionsInDisplayOrder().first { $0.band == .outOfTokens }
        XCTAssertEqual(strip(fleet).nextBackCountdown, "in 42m")
        XCTAssertEqual(strip(fleet).nextBackCountdown, heading?.firstBackCountdown(now: now))
    }

    /// Out-of-tokens accounts in two different groups make two headings; the
    /// strip takes the soonest of both.
    func testNextBackIsTheSoonestAcrossGroups() {
        let fleet = Fleet(accounts: [
            stripAccount(
                "frank@example.com", quotaState: .spent, held: [hold("5h", at(90))],
                groups: ["dev"]),
            stripAccount(
                "grace@example.com", quotaState: .spent, held: [hold("5h", at(30))],
                groups: ["ops"]),
        ])
        XCTAssertEqual(strip(fleet).nextBackCountdown, "in 30m")
    }

    /// No account waiting: no "next back in" at all.
    func testNoWaitingAccountMeansNoNextBack() {
        let fleet = Fleet(accounts: [
            stripAccount("bob@example.com"), stripAccount("carol@example.com"),
        ])
        XCTAssertNil(strip(fleet).nextBackCountdown)
        XCTAssertEqual(strip(fleet).headline, "2 of 2 can take work now")
    }

    // MARK: - Segments

    /// Left to right is the tab's top to bottom: the live box (by name), then
    /// out of tokens, then parked.
    func testSegmentOrderIsRowOrder() {
        let fleet = mixed
        let rows = fleet.accountsTabSections(expandedGroups: []).flatMap(\.rows)
        XCTAssertEqual(
            strip(fleet).segments.map(\.id),
            [
                "alice@example.com", "bob@example.com", "frank@example.com",
                "grace@example.com", "mallory@example.com", "niaj@example.com",
            ])
        XCTAssertEqual(strip(fleet).segments.map(\.id), rows.map(\.account.id))
    }

    /// The tab draws loose lines first, then open groups, then groups closed
    /// to one line; the segments follow it, not `sectionsInDisplayOrder`.
    func testSegmentOrderFollowsTheTabNotTheSectionList() {
        let fleet = Fleet(accounts: [
            stripAccount("alice@example.com", groups: ["dev"]),
            stripAccount("zed@example.com"),
        ])
        // The section list puts the named group first; the tab puts loose first.
        XCTAssertEqual(
            fleet.sectionsInDisplayOrder().flatMap(\.rows).map(\.account.id),
            ["alice@example.com", "zed@example.com"])
        XCTAssertEqual(
            strip(fleet).segments.map(\.id), ["zed@example.com", "alice@example.com"])
    }

    /// A group closed to its one summary line is drawn after the open ones,
    /// so its accounts' segments come last too, though its name sorts first.
    func testAClosedGroupsSegmentsComeAfterTheOpenGroups() {
        let big = (1...4).map { stripAccount("big-\($0)@example.com", groups: ["big"]) }
        let fleet = Fleet(accounts: big + [stripAccount("carol@example.com", groups: ["dev"])])
        XCTAssertEqual(
            strip(fleet).segments.map(\.id),
            ["carol@example.com"] + (1...4).map { "big-\($0)@example.com" })
        // Opened, it is an ordinary group and sorts by name again.
        XCTAssertEqual(
            strip(fleet, expanded: ["g:big"]).segments.map(\.id),
            (1...4).map { "big-\($0)@example.com" } + ["carol@example.com"])
    }

    /// A card (lease line or exit picker) is drawn after its section's lines,
    /// and so is its segment.
    func testACardSegmentFollowsItsSectionsLines() {
        let fleet = Fleet(accounts: [
            stripAccount("alice@example.com"), stripAccount("bob@example.com"),
        ])
        let rows = fleet.accountsTabSections(expandedGroups: [])
            .rowsInDrawOrder(expandedGroups: [], parkedCap: 2) {
                $0.account.name == "alice@example.com"
            }
        XCTAssertEqual(rows.map(\.account.id), ["bob@example.com", "alice@example.com"])
    }

    /// An account in two groups draws two lines and gets ONE segment, at its
    /// first line.
    func testAnAccountInTwoGroupsGetsOneSegment() {
        let fleet = Fleet(accounts: [
            stripAccount("alice@example.com", groups: ["dev", "ops"]),
            stripAccount("bob@example.com", groups: ["ops"]),
        ])
        let strip = strip(fleet)
        XCTAssertEqual(strip.segments.map(\.id), ["alice@example.com", "bob@example.com"])
        XCTAssertEqual(strip.total, 2)
        XCTAssertEqual(strip.canTakeWork, 2)
    }

    /// Each live segment is filled by its own 5-hour room, in its row's
    /// colour; out of tokens is red, parked and never measured are grey.
    func testSegmentFillAndState() {
        let kinds = Dictionary(
            uniqueKeysWithValues: strip(mixed).segments.map { ($0.id, $0.kind) })
        XCTAssertEqual(kinds["bob@example.com"], .room(0.9, .ok))
        guard case .room(let room, let tone) = kinds["alice@example.com"] else {
            return XCTFail("alice should be a filled live segment")
        }
        XCTAssertEqual(room, 0.1, accuracy: 1e-9)
        XCTAssertEqual(tone, .near)
        XCTAssertEqual(kinds["frank@example.com"], .outOfTokens)
        XCTAssertEqual(kinds["grace@example.com"], .outOfTokens)
        XCTAssertEqual(kinds["niaj@example.com"], .off)
        XCTAssertEqual(kinds["mallory@example.com"], .off)
    }

    /// Live but never measured: grey, never an empty green that reads as 0 %.
    func testALiveAccountNeverMeasuredIsGrey() {
        let fleet = Fleet(accounts: [
            stripAccount("erin@example.com", quota: nil, probe: .never, fiveHour: nil)
        ])
        XCTAssertEqual(strip(fleet).segments.map(\.kind), [.off])
        XCTAssertEqual(strip(fleet).canTakeWork, 1)
    }

    /// A reading past 100 % fills nothing rather than a negative height.
    func testRoomIsClampedToTheSegment() {
        let fleet = Fleet(accounts: [stripAccount("bob@example.com", fiveHour: 1.2)])
        XCTAssertEqual(strip(fleet).segments.map(\.kind), [.room(0, .ok)])
    }

    // MARK: - Pointing

    /// "Past ~25 the strip stops being pointable": drawn, but not pointed at.
    func testPointingStopsPastTwentyFive() {
        let accounts = { (count: Int) in
            Fleet(accounts: (1...count).map { stripAccount("member-\($0)@example.com") })
        }
        XCTAssertTrue(strip(accounts(25)).isPointable)
        XCTAssertFalse(strip(accounts(26)).isPointable)
        XCTAssertEqual(strip(accounts(26)).segments.count, 26)
    }
}

private func hold(_ window: String, _ resetAtMs: Int64) -> HeldWindow {
    HeldWindow(window: window, minutesUntilReset: 0, resetAtMs: resetAtMs)
}

private func stripAccount(
    _ name: String,
    quotaState: QuotaState = .ok,
    disabled: Bool = false,
    quota: Double? = 0.5,
    probe: ProbeState = .ok,
    fiveHour: Double? = 0.1,
    fiveHourState: QuotaState? = nil,
    held: [HeldWindow] = [],
    groups: [String]? = nil,
    status: String = "active"
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: status,
        disabled: disabled,
        quota: quota,
        quotaState: quotaState,
        fiveHour: fiveHour,
        sevenDay: 0.3,
        sevenDayOi: nil,
        fiveHourState: fiveHourState,
        sevenDayState: nil,
        fiveHourResetAtMs: nil,
        sevenDayResetAtMs: nil,
        held: held,
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheHitRatio: nil,
        probeStatus: probe,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false,
        groups: groups,
        reservedGroups: nil,
        parkedGroups: nil,
        groupColors: nil
    )
}
