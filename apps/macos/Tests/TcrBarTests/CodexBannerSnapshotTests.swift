import XCTest

@testable import TcrBarCore

final class CodexBannerSnapshotTests: XCTestCase {
    @MainActor
    func testPollingCannotInsertOrRemoveASessionsNotice() {
        let snapshot = CodexBannerSnapshot()
        snapshot.observeSessionsLine(nil)
        snapshot.decideAtOpen()
        snapshot.observeSessionsLine("Some Codex requests have no session row.")
        XCTAssertNil(snapshot.sessionsLine)
        snapshot.decideAtOpen()
        let shown = snapshot.sessionsLine
        snapshot.observeSessionsLine(nil)
        XCTAssertEqual(snapshot.sessionsLine, shown)
        snapshot.observeSessionsLine("A longer notice must wait for the next opening.")
        XCTAssertEqual(snapshot.sessionsLine, shown)
        snapshot.decideAtOpen()
        XCTAssertEqual(snapshot.sessionsLine, "A longer notice must wait for the next opening.")
    }

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
