import XCTest

@testable import TcrBarCore

final class CodexBannerSnapshotTests: XCTestCase {
    @MainActor
    func testPollingCannotInsertABannerIntoAnOpenPanel() {
        let snapshot = CodexBannerSnapshot()
        snapshot.observe(nil)
        snapshot.decideAtOpen()
        snapshot.observe(.refused(count: 1, last: .usageLimit, peerSharing: false))
        XCTAssertNil(snapshot.shown)
        snapshot.decideAtOpen()
        XCTAssertEqual(snapshot.shown, .refused(count: 1, last: .usageLimit, peerSharing: false))
    }

    @MainActor
    func testOnePersistentPanelKeepsItsTextAndTintUntilReopened() {
        let snapshot = CodexBannerSnapshot()
        let original = CodexBanner.refused(count: 1, last: .usageLimit, peerSharing: false)
        snapshot.observe(original)
        snapshot.decideAtOpen()
        for candidate in [CodexBanner.unpooled(count: 200), nil] {
            snapshot.observe(candidate)
            XCTAssertEqual(snapshot.shown?.text, original.text)
            XCTAssertEqual(snapshot.shown?.tint, original.tint)
        }
        snapshot.decideAtOpen()
        XCTAssertNil(snapshot.shown)
    }
}
