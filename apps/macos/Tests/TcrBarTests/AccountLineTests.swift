import XCTest

@testable import TcrBarCore

/// The words on an account's one line (option H): the live signal, the
/// waiting line, the band heading's count and its "first back in".
///
/// Every countdown is measured from a fixed `now`, so each assertion is about
/// the rule and never about the clock. Account names are obviously fake: this
/// repository is public.
final class AccountLineTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_800_000_000)

    /// Unix milliseconds `minutes` after ``now``.
    private func at(_ minutes: Int) -> Int64 {
        Int64((now.timeIntervalSince1970 + Double(minutes) * 60) * 1000)
    }

    // MARK: - Live signal

    func testReadyAccountSaysReady() {
        let account = lineAccount("bob@example.com", fiveHour: 0.1, sevenDay: 0.3)
        XCTAssertEqual(account.liveSignal(now: now), AccountLineText("ready", .quiet))
    }

    /// Option H's alice row: 5h near, so the signal is the 5-hour reset.
    func testNearAccountCountsDownToItsNearWindowsReset() {
        let account = lineAccount(
            "alice@example.com", quotaState: .near, fiveHour: 0.9, sevenDay: 0.9,
            fiveHourState: .near, sevenDayState: .near,
            fiveHourResetAtMs: at(47), sevenDayResetAtMs: at(6_000))
        XCTAssertEqual(account.liveSignal(now: now), AccountLineText("in 47m", .near))
    }

    /// A near WEEK with a calm 5-hour window: the 5-hour reset relieves
    /// nothing, so the countdown is to the week's reset.
    func testNearWeekWithCalmFiveHourCountsDownToTheWeek() {
        let account = lineAccount(
            "alice@example.com", quotaState: .near, fiveHour: 0.1, sevenDay: 0.9,
            fiveHourState: .ok, sevenDayState: .near,
            fiveHourResetAtMs: at(47), sevenDayResetAtMs: at(1_500))
        XCTAssertEqual(account.liveSignal(now: now), AccountLineText("in 1d 1h", .near))
    }

    /// No reset on the wire: words, never an invented time.
    func testNearWithNoResetSaysNearLimit() {
        let account = lineAccount(
            "alice@example.com", quotaState: .near, fiveHour: 0.9, sevenDay: 0.2,
            fiveHourState: .near, sevenDayState: .ok)
        XCTAssertEqual(account.liveSignal(now: now), AccountLineText("near limit", .near))
    }

    func testBrokenAndUnmeasuredAccountsSayWhatTheyAre() {
        XCTAssertEqual(
            lineAccount("erin@example.com", status: "error").liveSignal(now: now),
            AccountLineText("re-login", .bad))
        XCTAssertEqual(
            lineAccount("erin@example.com", quota: nil, probe: .never).liveSignal(now: now),
            AccountLineText("unmeasured", .info))
    }

    // MARK: - Waiting line

    func testOutOfTokensLineSaysWeekUsedAndBackIn() {
        let account = lineAccount(
            "frank@example.com", quotaState: .spent, fiveHour: 1.0, sevenDay: 0.6,
            held: [hold("5h", at(42))])
        XCTAssertEqual(account.waitingLineText, "week 60% used")
        XCTAssertEqual(
            account.waitingLineSignal(in: .outOfTokens, now: now),
            AccountLineText("back in 42m", .bad))
    }

    /// Held by BOTH windows: back when the later one resets, not the sooner.
    func testBackInIsTheLatestHoldNotTheSoonest() {
        let account = lineAccount(
            "frank@example.com", quotaState: .spent, fiveHour: 1.0, sevenDay: 1.0,
            held: [hold("5h", at(42)), hold("7d", at(3_000))])
        XCTAssertEqual(account.backInCaption(now: now), "back in 2d 2h")
    }

    /// Nothing holds it on the wire: no countdown at all, never "back in 0m".
    func testOutOfTokensWithNoHoldDrawsNoCountdown() {
        let account = lineAccount("frank@example.com", quotaState: .spent, fiveHour: 1.0)
        XCTAssertNil(account.waitingLineSignal(in: .outOfTokens, now: now))
    }

    func testParkedLinesSayNeverMeasuredOrWeekUsed() {
        let never = lineAccount("mallory@example.com", disabled: true, quota: nil, probe: .never)
        XCTAssertEqual(never.waitingLineText, "never measured")
        XCTAssertEqual(
            never.waitingLineSignal(in: .parked, now: now), AccountLineText("parked", .quiet))

        let measured = lineAccount("niaj@example.com", disabled: true, sevenDay: 0.2)
        XCTAssertEqual(measured.waitingLineText, "week 20% used")
    }

    /// Measured, but the 7-day window has no reading: the line says nothing
    /// rather than "week n/a used" or "week 0% used".
    func testMeasuredAccountWithNoWeekReadingDrawsNoMiddleText() {
        let account = lineAccount("niaj@example.com", disabled: true, sevenDay: nil)
        XCTAssertNil(account.waitingLineText)
    }

    // MARK: - Band heading

    func testOutOfTokensHeadingCountsItsRowsAndNamesTheFirstBack() {
        let fleet = Fleet(accounts: [
            lineAccount("bob@example.com"),
            lineAccount(
                "frank@example.com", quotaState: .spent, held: [hold("5h", at(72))]),
            lineAccount(
                "grace@example.com", quotaState: .spent, held: [hold("5h", at(42))]),
            lineAccount("mallory@example.com", disabled: true),
        ])
        let sections = fleet.sectionsInDisplayOrder()
        let spent = sections.first { $0.band == .outOfTokens }
        let parked = sections.first { $0.band == .parked }

        XCTAssertEqual(spent?.bandHeadingText, "Out of tokens · 2")
        XCTAssertEqual(spent?.firstBackCaption(now: now), "first back in 42m")
        XCTAssertEqual(parked?.bandHeadingText, "Parked · 1")
        XCTAssertNil(parked?.firstBackCaption(now: now))
    }

    /// A row whose hold has already passed must not hide the others' real
    /// countdowns behind a nil.
    func testFirstBackIgnoresAResetAlreadyPast() {
        let fleet = Fleet(accounts: [
            lineAccount(
                "frank@example.com", quotaState: .spent, held: [hold("5h", at(-5))]),
            lineAccount(
                "grace@example.com", quotaState: .spent, held: [hold("5h", at(90))]),
        ])
        let spent = fleet.sectionsInDisplayOrder().first { $0.band == .outOfTokens }
        XCTAssertEqual(spent?.firstBackCaption(now: now), "first back in 1h 30m")
    }

    // MARK: - Row form

    /// Inside a group section an account gets the same one line as outside
    /// one (lead ruling on H1b, 2026-10-14), chosen by the same band: a
    /// fleet mixing cards and lines reads as unfinished.
    func testAGroupedAccountResolvesToTheLineOfItsBand() {
        let fleet = Fleet(accounts: [
            lineAccount("bob@example.com", groups: ["dev"]),
            lineAccount("frank@example.com", quotaState: .spent, fiveHour: 1.0, groups: ["dev"]),
            lineAccount(
                "mallory@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"]),
            lineAccount("carol@example.com"),
        ])
        let forms = Dictionary(
            uniqueKeysWithValues: fleet.sectionsInDisplayOrder().flatMap(\.rows).map {
                ($0.account.name, $0.form(carriesLease: false, carriesExit: false))
            })
        XCTAssertEqual(forms["bob@example.com"], .liveLine)
        XCTAssertEqual(forms["frank@example.com"], .waitingLine)
        XCTAssertEqual(forms["mallory@example.com"], .waitingLine)
        XCTAssertEqual(forms["carol@example.com"], .liveLine)
    }

    /// A lease line or an exit picker keeps the card, in a group or out of
    /// one: neither fits a line, and neither belongs only in a hover card.
    func testALeaseOrAnExitKeepsTheCardInAGroupToo() {
        for group in [FleetGroupKey.named("dev"), .ungrouped] {
            let row = FleetSectionRow(
                account: lineAccount("bob@example.com"), group: group, band: .live,
                isControl: false)
            XCTAssertEqual(row.form(carriesLease: true, carriesExit: false), .card, "\(group)")
            XCTAssertEqual(row.form(carriesLease: false, carriesExit: true), .card, "\(group)")
        }
    }
}

private func hold(_ window: String, _ resetAtMs: Int64) -> HeldWindow {
    HeldWindow(window: window, minutesUntilReset: 0, resetAtMs: resetAtMs)
}

private func lineAccount(
    _ name: String,
    status: String = "active",
    quotaState: QuotaState = .ok,
    disabled: Bool = false,
    quota: Double? = 0.5,
    probe: ProbeState = .ok,
    fiveHour: Double? = 0.1,
    sevenDay: Double? = 0.3,
    fiveHourState: QuotaState? = nil,
    sevenDayState: QuotaState? = nil,
    fiveHourResetAtMs: Int64? = nil,
    sevenDayResetAtMs: Int64? = nil,
    held: [HeldWindow] = [],
    groups: [String]? = nil,
    parkedGroups: [String]? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: status,
        disabled: disabled,
        quota: quota,
        quotaState: quotaState,
        fiveHour: fiveHour,
        sevenDay: sevenDay,
        sevenDayOi: nil,
        fiveHourState: fiveHourState,
        sevenDayState: sevenDayState,
        fiveHourResetAtMs: fiveHourResetAtMs,
        sevenDayResetAtMs: sevenDayResetAtMs,
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
        parkedGroups: parkedGroups,
        groupColors: nil
    )
}
