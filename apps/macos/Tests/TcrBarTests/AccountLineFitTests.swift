import AppKit
import XCTest

@testable import TcrBarCore

/// The live line's widths (option H, lead rulings on H1b 2026-10-14): one bar
/// width for the whole panel, every row labelled, every bar at the same x, in a
/// group box or out of one; the signal column as wide as what it says, the
/// slack to the names; and a name too long for its column cut in the middle.
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
            let signals = ["unmeasured", "in 3d 22h"]
            let signal = AccountLineFit.signalColumn(signals, m)
            let loose = AccountLineFit.nameColumn(
                looseNames: [longName], groupedNames: [longName], signals: signals, bar: bar,
                room, m)
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

    /// The signal column is the widest signal drawn, not a reserve.
    func testTheSignalColumnIsTheWidestSignalDrawn() {
        let m = densities[0].1
        let widest = drawn("in 3d 22h", size: m.signalSize)
        XCTAssertEqual(
            AccountLineFit.signalColumn(["ready", "in 3d 22h"], m), widest.rounded(.up))
    }

    /// What a short signal leaves goes to the names: a panel that says only
    /// "ready" gives a long name more room than one that says "in 3d 22h".
    func testAShortSignalGivesItsRoomToTheNames() {
        for (density, m) in densities {
            let bar = AccountLineFit.barWidth(room, m)
            let ready = AccountLineFit.nameColumn(
                looseNames: [longName], groupedNames: [longName], signals: ["ready"],
                bar: bar, room, m)
            let long = AccountLineFit.nameColumn(
                looseNames: [longName], groupedNames: [longName], signals: ["in 3d 22h"],
                bar: bar, room, m)
            XCTAssertGreaterThan(ready, long, density)
        }
    }

    /// A fleet of short names keeps option H's own column and bar.
    func testShortNamesKeepOptionHsColumnAndBar() {
        let m = densities[0].1
        let bar = AccountLineFit.barWidth(room, m)
        XCTAssertEqual(bar, 32)
        XCTAssertEqual(
            AccountLineFit.nameColumn(
                looseNames: ["bob"], groupedNames: ["carol"], signals: ["ready"], bar: bar,
                room, m), 54)
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

    // MARK: - Labels other than 5h and 7d

    /// A Codex plan's 30-day window draws a `30d` label. The label column widens to
    /// hold it, and the one bar narrows rather than pushing a line past its room.
    func testAThirtyDayLabelWidensTheLabelColumnAndNeverTheBar() {
        let labels = ["5h", "7d", "30d"]
        for (density, m) in densities {
            XCTAssertGreaterThan(
                AccountLineFit.labelColumn(m, labels: labels), AccountLineFit.labelColumn(m),
                density)
            XCTAssertLessThanOrEqual(
                AccountLineFit.barWidth(room, m, labels: labels),
                AccountLineFit.barWidth(room, m), density)
        }
    }

    /// Main's two labels, passed explicitly, give every one of main's widths: a fleet
    /// with no other label draws exactly as before.
    func testMainsLabelsGiveMainsWidths() {
        let labels = AccountLineFit.labels
        for (density, m) in densities {
            XCTAssertEqual(
                AccountLineFit.labelColumn(m, labels: labels), AccountLineFit.labelColumn(m),
                density)
            let bar = AccountLineFit.barWidth(room, m)
            XCTAssertEqual(AccountLineFit.barWidth(room, m, labels: labels), bar, density)
            XCTAssertEqual(
                AccountLineFit.meterWidth(bar: bar, m, labels: labels),
                AccountLineFit.meterWidth(bar: bar, m), density)
            XCTAssertEqual(
                AccountLineFit.nameColumn(
                    looseNames: [longName], groupedNames: [longName], signals: ["ready"],
                    bar: bar, room, m, labels: labels),
                AccountLineFit.nameColumn(
                    looseNames: [longName], groupedNames: [longName], signals: ["ready"],
                    bar: bar, room, m),
                density)
        }
    }
}
