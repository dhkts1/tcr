import AppKit
import XCTest

@testable import TcrBarCore

/// The live line's widths (option H, lead rulings on H1b 2026-10-14): one bar
/// width for the whole panel, every row labelled, every bar at the same x, in a
/// group box or out of one; the signal column as wide as the widest signal a
/// line can draw, decided once (2026-10-15); and a name too long for its column
/// cut in the middle.
///
/// The tokens are option H's, as `V4` holds them, at the two densities' type
/// sizes. The room is the panel's: 372 wide, 10 of side padding, 8 of line box
/// padding each side, and a group's own 8 each side.
final class AccountLineFitTests: XCTestCase {
    private let room = AccountLineFit.Room(looseWidth: 372 - 2 * 10 - 2 * 8, groupInset: 8)

    private func metrics(nameSize: CGFloat, labelSize: CGFloat, percentSize: CGFloat)
        -> AccountLineFit.Metrics
    {
        AccountLineFit.Metrics(
            dot: 8, gap: 7, meterGap: 4, labelMinWidth: 14, percentMinWidth: 33,
            signalWidth: 56, nameMinWidth: 54, barMaxWidth: 32, nameSize: nameSize,
            labelSize: labelSize, percentSize: percentSize, signalSize: labelSize + 0.5)
    }

    private var densities: [(String, AccountLineFit.Metrics)] {
        [
            ("compact", metrics(nameSize: 13, labelSize: 11, percentSize: 12)),
            ("comfortable", metrics(nameSize: 15, labelSize: 12, percentSize: 13)),
        ]
    }

    private let longName = "henry.fitzgerald.the.third"

    /// The width a string draws at in the panel's font, measured here with the
    /// font itself rather than through the code under test.
    private func drawn(_ text: String, size: CGFloat, weight: NSFont.Weight = .regular)
        -> CGFloat
    {
        (text as NSString)
            .size(withAttributes: [
                .font: NSFont.monospacedDigitSystemFont(ofSize: size, weight: weight)
            ]).width
    }

    /// The rule the lead asked for: at the panel's one bar width, a line inside
    /// a group has room for both labels and "100%" twice and its signal,
    /// however long the names around it are. A name too long for the room is
    /// cut; the labels never drop.
    func testTheWorstCaseDigitsFitInsideAGroupAtBothDensities() {
        for (density, m) in densities {
            let bar = AccountLineFit.barWidth(room, m)
            XCTAssertGreaterThan(bar, 0, density)
            let signal = AccountLineFit.signalColumn(m)
            let loose = AccountLineFit.nameColumn(
                looseNames: [longName], groupedNames: [longName], bar: bar, room, m)
            let grouped = loose - room.groupInset
            XCTAssertLessThanOrEqual(
                AccountLineFit.lineWidth(name: grouped, bar: bar, signal: signal, m),
                room.groupedWidth,
                "\(density): a grouped line with bar \(bar) and name \(grouped) overflows")
            XCTAssertLessThanOrEqual(
                AccountLineFit.lineWidth(name: loose, bar: bar, signal: signal, m),
                room.looseWidth, density)
        }
    }

    /// "100%" as the panel DRAWS it, tabular digits, is what the percent
    /// column is sized for, so a row's own digits cannot move the bar after
    /// them.
    func testThePercentColumnHoldsAFullWindowInTabularDigits() {
        for (density, m) in densities {
            XCTAssertGreaterThanOrEqual(
                AccountLineFit.percentColumn(m), drawn("100%", size: m.percentSize), density)
        }
    }

    /// The signal column holds the longest status a live line can say, so no
    /// status a poll brings can be wider than the column the names and bars
    /// were placed against. Sized from what was drawn at the moment, a panel
    /// saying only "ready" gave its names that room, and the first countdown
    /// pushed every bar sideways (2026-10-15).
    func testTheSignalColumnHoldsTheLongestStatusALineCanSay() {
        let longest = [
            "ready", "near limit", "unknown", "unmeasured", "re-login", "rejected",
            "in 6d 23h", "in 23h 59m", "in 7d", "in 47m",
        ]
        for (density, m) in densities {
            let column = AccountLineFit.signalColumn(m)
            for text in longest {
                XCTAssertLessThanOrEqual(
                    drawn(text, size: m.signalSize), column, "\(density): \(text)")
            }
        }
    }

    /// The names the column is measured over do not change when an account runs
    /// out of tokens or a group is collapsed or opened: the same accounts give
    /// the same names, so the same column and the same bar x. Measured over the
    /// lines drawn at the moment, the long name leaving the live band shrank
    /// the column and every bar slid left (2026-10-15).
    func testTheNamesMeasuredIgnoreBandsAndCollapse() {
        let long = "\(longName)@example.com"
        let big = (1...4).map { lineAccount("big-\($0)@example.com", groups: ["big"]) }
        let calm = Fleet(accounts: [lineAccount("bob@example.com"), lineAccount(long)] + big)
        let busy = Fleet(
            accounts: [lineAccount("bob@example.com"), lineAccount(long, quotaState: .spent)]
                + big)
        func names(_ fleet: Fleet, open: Set<String> = []) -> AccountLineFit.LineNames {
            AccountLineFit.lineNames(fleet.accountsTabSections(expandedGroups: open))
        }
        XCTAssertEqual(names(calm).loose, ["bob", longName])
        XCTAssertEqual(names(calm), names(busy), "an account running out moved the column")
        XCTAssertEqual(
            names(calm), names(calm, open: ["g:big"]), "opening a group moved the column")
        XCTAssertEqual(names(calm).grouped, (1...4).map { "big-\($0)" })
    }

    /// A fleet of short names keeps option H's own name column at either
    /// density, with the signal column at its widest: the bars, not the names,
    /// give up what the widest status needs past option H's 56.
    func testShortNamesKeepOptionHsColumn() {
        for (density, m) in densities {
            let bar = AccountLineFit.barWidth(room, m)
            XCTAssertGreaterThan(bar, 0, density)
            XCTAssertLessThanOrEqual(bar, 32, density)
            XCTAssertEqual(
                AccountLineFit.nameColumn(
                    looseNames: ["bob"], groupedNames: ["carol"], bar: bar, room, m), 54,
                density)
        }
    }

    // MARK: - Cutting a name

    /// Lead ruling: two rows that both read "henr…" are a defect. Names that
    /// share a long prefix and differ only at the end must still draw as two
    /// different strings when both are cut.
    func testNamesThatDifferOnlyAtTheEndStayDifferentWhenCut() {
        for (density, m) in densities {
            let width = drawn("henr\u{2026}", size: m.nameSize, weight: .semibold) + 1
            let nine = AccountLineFit.fitName("henry9", width: width, m)
            let ten = AccountLineFit.fitName("henry10", width: width, m)
            XCTAssertNotEqual(nine, ten, "\(density): both read \(nine)")
            XCTAssertEqual(nine.last, "9", density)
            XCTAssertEqual(ten.last, "0", density)
            for cut in [nine, ten] {
                XCTAssertLessThanOrEqual(
                    drawn(cut, size: m.nameSize, weight: .semibold), width, "\(density): \(cut)")
            }
        }
    }

    /// A name that fits is drawn whole, never cut.
    func testANameThatFitsIsDrawnWhole() {
        let m = densities[0].1
        XCTAssertEqual(AccountLineFit.fitName("henry10", width: 200, m), "henry10")
    }
}

/// A measured account in the live band unless `quotaState` says otherwise.
private func lineAccount(
    _ name: String, quotaState: QuotaState = .ok, groups: [String]? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: false,
        quota: 0.5,
        quotaState: quotaState,
        fiveHour: quotaState == .spent ? 1.0 : 0.1,
        sevenDay: 0.3,
        sevenDayOi: nil,
        fiveHourState: nil,
        sevenDayState: nil,
        fiveHourResetAtMs: nil,
        sevenDayResetAtMs: nil,
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
        reservedGroups: nil,
        parkedGroups: nil,
        groupColors: nil
    )
}
