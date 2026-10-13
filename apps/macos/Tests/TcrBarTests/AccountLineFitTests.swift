import CoreGraphics
import XCTest

@testable import TcrBarCore

/// The live line's widths (option H, lead ruling on H1b 2026-10-14): one bar
/// width for the whole panel, every row labelled, every bar at the same x, in a
/// group box or out of one.
///
/// The tokens are option H's, as `V4` holds them, at the two densities' type
/// sizes. The room is the panel's: 372 wide, 10 of side padding, 8 of line box
/// padding each side, and a group's own 8 each side.
final class AccountLineFitTests: XCTestCase {
    private let room = AccountLineFit.Room(looseWidth: 372 - 2 * 10 - 2 * 8, groupInset: 8)

    private func metrics(labelSize: CGFloat, percentSize: CGFloat) -> AccountLineFit.Metrics {
        AccountLineFit.Metrics(
            dot: 8, gap: 7, meterGap: 4, labelMinWidth: 14, percentMinWidth: 33,
            signalWidth: 56, nameMinWidth: 54, barMaxWidth: 32,
            labelSize: labelSize, percentSize: percentSize)
    }

    private var densities: [(String, AccountLineFit.Metrics)] {
        [
            ("compact", metrics(labelSize: 11, percentSize: 12)),
            ("comfortable", metrics(labelSize: 12, percentSize: 13)),
        ]
    }

    /// The rule the lead asked for: at the panel's one bar width, a line inside
    /// a group has room for both labels and "100%" twice, however long the
    /// names around it are. A name too long for the room truncates; the labels
    /// never drop.
    func testTheWorstCaseDigitsFitInsideAGroupAtBothDensities() {
        for (density, m) in densities {
            let bar = AccountLineFit.barWidth(room, m)
            XCTAssertGreaterThan(bar, 0, density)
            let loose = AccountLineFit.nameColumn(
                widestLoose: 400, widestGrouped: 400, bar: bar, room, m)
            let grouped = loose - room.groupInset
            XCTAssertLessThanOrEqual(
                AccountLineFit.lineWidth(name: grouped, bar: bar, m), room.groupedWidth,
                "\(density): a grouped line with bar \(bar) and name \(grouped) overflows")
            XCTAssertLessThanOrEqual(
                AccountLineFit.lineWidth(name: loose, bar: bar, m), room.looseWidth, density)
        }
    }

    /// "100%" is what the percent column is sized for, so a row's own digits
    /// cannot move the bar after them.
    func testThePercentColumnHoldsAFullWindow() {
        for (density, m) in densities {
            let full = NameRowFit.textWidth("100%", size: m.percentSize, weight: .regular)
            XCTAssertGreaterThanOrEqual(AccountLineFit.percentColumn(m), full, density)
        }
    }

    /// A fleet of short names keeps option H's own column and bar.
    func testShortNamesKeepOptionHsColumnAndBar() {
        let m = densities[0].1
        let bar = AccountLineFit.barWidth(room, m)
        XCTAssertEqual(bar, 32)
        XCTAssertEqual(
            AccountLineFit.nameColumn(widestLoose: 30, widestGrouped: 30, bar: bar, room, m), 54)
    }

    /// A grouped name widens the column for every line, by its own width plus
    /// the group's inset, so the bars still start at one x.
    func testAGroupedNameWidensTheSharedColumn() {
        let m = densities[0].1
        let bar = AccountLineFit.barWidth(room, m)
        XCTAssertEqual(
            AccountLineFit.nameColumn(widestLoose: 30, widestGrouped: 50, bar: bar, room, m), 58)
    }
}
