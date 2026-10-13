import CoreGraphics
import XCTest

@testable import TcrBarCore

/// The hover card's placement and lifetime (option H, unit H2).
///
/// The panel-size half of the rule (the card never changes the list's or the
/// panel's height) is measured where it can be: `--render-states` draws every
/// hover scene twice, with and without the card, and exits non-zero when the
/// two sizes differ (`RenderStates.hoverCardKeepsPanelSize`).
final class HoverCardTests: XCTestCase {
    /// A list 380 pt tall starting 160 pt down a panel 700 pt tall and 372 wide,
    /// roughly the 13-account render.
    private let panel = CGRect(x: 0, y: 0, width: 372, height: 700)
    private let list = CGRect(x: 10, y: 160, width: 352, height: 380)

    private func row(at y: CGFloat) -> CGRect {
        CGRect(x: 18, y: y, width: 336, height: 26)
    }

    // MARK: - Edge

    func testARowInTheUpperHalfOpensBelow() {
        XCTAssertEqual(HoverCardPlacement.edge(row: row(at: 166), list: list), .below)
    }

    func testARowInTheLowerHalfOpensAbove() {
        XCTAssertEqual(HoverCardPlacement.edge(row: row(at: 480), list: list), .above)
    }

    /// The list's middle is at 350: a row centred exactly there opens below.
    func testARowCentredOnTheMiddleOpensBelow() {
        XCTAssertEqual(HoverCardPlacement.edge(row: row(at: 337), list: list), .below)
    }

    // MARK: - Region

    func testBelowStartsAtTheRowsBottomAndRunsToThePanelsBottom() {
        let region = HoverCardPlacement.region(
            row: row(at: 166), list: list, panel: panel, inset: 10)
        XCTAssertEqual(region, CGRect(x: 10, y: 192, width: 352, height: 508))
    }

    func testAboveRunsFromThePanelsTopToTheRowsTop() {
        let region = HoverCardPlacement.region(
            row: row(at: 480), list: list, panel: panel, inset: 10)
        XCTAssertEqual(region, CGRect(x: 10, y: 0, width: 352, height: 480))
    }

    /// A row scrolled partly past the panel's edge still gets a box inside the
    /// panel, never one with a negative height or past the edge.
    func testTheRegionNeverLeavesThePanel() {
        for y in stride(from: CGFloat(-40), through: 740, by: 20) {
            let region = HoverCardPlacement.region(
                row: row(at: y), list: list, panel: panel, inset: 10)
            XCTAssertTrue(
                panel.contains(region) || region.height == 0,
                "row at \(y): \(region) is outside \(panel)")
            XCTAssertGreaterThanOrEqual(region.height, 0, "row at \(y)")
        }
    }

    // MARK: - Tracker

    func testEnteringARowShowsItsCard() {
        var tracker = HoverCardTracker()
        tracker.rowEntered("alice@example.com")
        XCTAssertEqual(tracker.shown, "alice@example.com")
    }

    func testLeavingTheRowHidesTheCardOnceItSettles() {
        var tracker = HoverCardTracker()
        tracker.rowEntered("alice@example.com")
        tracker.rowLeft("alice@example.com")
        XCTAssertEqual(tracker.shown, "alice@example.com", "a leave waits for settle")
        tracker.settle()
        XCTAssertNil(tracker.shown)
    }

    /// Row to card: the card stays, so its gear can be reached.
    func testMovingFromTheRowOntoTheCardKeepsIt() {
        var tracker = HoverCardTracker()
        tracker.rowEntered("alice@example.com")
        tracker.rowLeft("alice@example.com")
        tracker.cardEntered()
        tracker.settle()
        XCTAssertEqual(tracker.shown, "alice@example.com")
        tracker.cardLeft()
        tracker.settle()
        XCTAssertNil(tracker.shown)
    }

    /// Row to row: the newer row's card, and the older row's late leave does
    /// not take it away.
    func testAStaleLeaveFromThePreviousRowKeepsTheNewCard() {
        var tracker = HoverCardTracker()
        tracker.rowEntered("alice@example.com")
        tracker.rowEntered("bob@example.com")
        tracker.rowLeft("alice@example.com")
        tracker.settle()
        XCTAssertEqual(tracker.shown, "bob@example.com")
    }

    /// The render harness's seed shows a card with no pointer, and settles
    /// only when something has moved.
    func testASeededCardShowsWithNoPointer() {
        let tracker = HoverCardTracker(shown: "near@example.com")
        XCTAssertEqual(tracker.shown, "near@example.com")
    }
}
