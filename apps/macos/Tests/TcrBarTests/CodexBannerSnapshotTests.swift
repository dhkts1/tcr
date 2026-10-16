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

    /// The first opening after launch draws the Codex lines. The panel's view does not
    /// exist before that opening, so nothing it records can reach it: the shell decides
    /// from the fleet it already polled, and no view ever touches this snapshot first.
    @MainActor
    func testTheFirstOpeningDecidesFromTheFleetWithNoViewInvolved() throws {
        let fleet = try snapshotFleet(traffic: [
            "refused10m": 4, "lastRefusal": "usage_limit_reached", "unpooled10m": 0,
        ])
        let snapshot = CodexBannerSnapshot()
        snapshot.observePoll(fleet)
        snapshot.decideAtOpen(fleet: fleet, bundledTcrWithoutCLI: nil) {
            XCTFail("a refusal line needs no process walk")
            return nil
        }
        XCTAssertEqual(snapshot.shown, .refused(count: 4, last: .usageLimit, peerSharing: false))
    }

    /// The quiet clock advances on polls that land while the panel is shut, so an opening
    /// five minutes after the Codex count stopped moving names the codex that bypasses tcr.
    @MainActor
    func testPollsWhileShutAdvanceTheQuietClock() throws {
        let fleet = try snapshotFleet(traffic: ["refused10m": 0, "unpooled10m": 0])
        let snapshot = CodexBannerSnapshot()
        let start = Date(timeIntervalSince1970: 1_767_225_600)
        for seconds in stride(from: 0.0, through: 330, by: 3) {
            snapshot.observePoll(fleet, now: start.addingTimeInterval(seconds))
        }
        snapshot.decideAtOpen(
            fleet: fleet, bundledTcrWithoutCLI: nil, now: start.addingTimeInterval(330)
        ) { 1 }
        XCTAssertEqual(snapshot.shown, .noRequests(processes: 1, bundledTcrWithoutCLI: nil))

        snapshot.decideAtOpen(
            fleet: fleet, bundledTcrWithoutCLI: nil, now: start.addingTimeInterval(200)
        ) { 1 }
        XCTAssertNil(snapshot.shown, "positive control: a clock short of five minutes draws nothing")
    }

    /// The shell feeds the snapshot: every poll advances its clock, and each opening decides
    /// from the shell's own read before the show. The test target links TcrBarCore alone, so
    /// the shell is read as text, whitespace removed.
    func testTheShellFeedsTheSnapshot() throws {
        let file = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // TcrBarTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // apps/macos
            .appendingPathComponent("Sources/TcrBar/MenuBarShell.swift")
        let source = try String(contentsOf: file, encoding: .utf8)
        let code = source.split(separator: "\n", omittingEmptySubsequences: false)
            .filter { !$0.trimmingCharacters(in: .whitespaces).hasPrefix("//") }
            .joined()
            .components(separatedBy: .whitespacesAndNewlines).joined()
        XCTAssertTrue(code.contains("self?.codexBannerSnapshot.observePoll(state.loadedFleet)"))
        let decide = try XCTUnwrap(code.range(of: "codexBannerSnapshot.decideAtOpen(fleet:fleet,"))
        let show = try XCTUnwrap(code.range(of: "popover.show(relativeTo:button.bounds"))
        XCTAssertLessThan(decide.lowerBound, show.lowerBound, "decided before the show")
    }
}

/// bob (Codex) and dana (Codex) from apps/macos/Tests/Fixtures/codex-status.json, pooled,
/// with `traffic` on each as the server sends it, and the request count standing at 42.
private func snapshotFleet(traffic: [String: Any]) throws -> Fleet {
    let url = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TcrBarTests
        .deletingLastPathComponent()  // Tests
        .appendingPathComponent("Fixtures/codex-status.json")
    let rows = try XCTUnwrap(
        JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
    let picked = try ["bob@example.com/codex", "dana@example.com"].map { name in
        var row = try XCTUnwrap(rows.first { $0["name"] as? String == name })
        row["codexRoute"] = ["pooled": true]
        row["codexTraffic"] = traffic
        row["requests"] = 21
        return row
    }
    return try Fleet.decode(JSONSerialization.data(withJSONObject: picked))
}
