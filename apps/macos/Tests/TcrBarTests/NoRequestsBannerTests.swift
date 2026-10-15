import Foundation
import XCTest

@testable import TcrBarCore

/// A colleague installed tcr, added a shared account, and Claude Code kept
/// answering from its own keychain account. The added account sat at zero
/// requests with no line on the panel that said why. These tests pin the
/// pure decision behind the Accounts tab's no-requests banner and the
/// sentence it builds.
///
/// Account names are obviously fake. This repository is public.
final class NoRequestsBannerTests: XCTestCase {

    // MARK: - totalRequests

    func testTotalRequestsSumsALiveFleet() throws {
        let fleet = try decodeFleet([
            account(name: "alice@example.com", requests: 2, source: "live"),
            account(name: "bob@example.com", requests: 3, source: "live"),
        ])
        XCTAssertEqual(NoRequestsBanner.totalRequests(fleet), 5)
    }

    /// The arm this whole feature exists for: a live read, every account at
    /// zero, is a measured zero and totals to `0`, not `nil`.
    func testTotalRequestsIsAMeasuredZeroOnALiveFleet() throws {
        let fleet = try decodeFleet([
            account(name: "alice@example.com", requests: 0, source: "live")
        ])
        XCTAssertEqual(NoRequestsBanner.totalRequests(fleet), 0)
    }

    /// An offline read's `requests` is structurally absent, not a measured
    /// zero. Summing it as `0` would draw this banner on a proxy that is not
    /// even running, which is a different fact from "running and unreached".
    func testTotalRequestsIsNilOnAnOfflineFleet() throws {
        let fleet = try decodeFleet([
            account(name: "alice@example.com", requests: 0, source: "offline")
        ])
        XCTAssertNil(NoRequestsBanner.totalRequests(fleet))
    }

    // MARK: - totalRequests, in a fleet holding Codex accounts

    /// The banner says no CLAUDE request reached the proxy, so it counts Claude rows only:
    /// a Codex row's requests say nothing about where Claude Code is routed.
    func testTheClaudeBannerCountsClaudeRowsOnly() throws {
        let fleet = try decodeFleet([
            account(name: "alice@example.com", requests: 0, source: "live"),
            codexAccount(name: "bob@example.com/codex", requests: 5, source: "live"),
        ])
        XCTAssertEqual(NoRequestsBanner.totalRequests(fleet), 0)
    }

    /// A fleet holding Codex accounts is not nagged about Claude unless it has a Claude
    /// account that could take a request.
    func testTheClaudeBannerNeedsAnEnabledClaudeAccount() throws {
        let codexOnly = try decodeFleet([
            codexAccount(name: "bob@example.com/codex", requests: 0, source: "live")
        ])
        XCTAssertNil(NoRequestsBanner.totalRequests(codexOnly))

        let claudeDisabled = try decodeFleet([
            account(name: "alice@example.com", requests: 0, source: "live", disabled: true),
            codexAccount(name: "bob@example.com/codex", requests: 0, source: "live"),
        ])
        XCTAssertNil(NoRequestsBanner.totalRequests(claudeDisabled))
    }

    /// A fleet with no Codex account keeps today's rule, which never asked whether an
    /// account was enabled.
    func testAnAllDisabledClaudeOnlyFleetKeepsTodaysTotal() throws {
        let fleet = try decodeFleet([
            account(name: "alice@example.com", requests: 0, source: "live", disabled: true)
        ])
        XCTAssertEqual(NoRequestsBanner.totalRequests(fleet), 0)
    }

    // MARK: - text

    private let route = ClaudeRouteRead.Route(url: "https://gateway.example:9443", source: "settings.json")

    func testTextIsNilBeforeTheQuietWindowElapses() {
        XCTAssertNil(
            NoRequestsBanner.text(liveFor: 299, claudeCount: nil, route: route))
    }

    func testTextAppearsOnceTheQuietWindowElapses() {
        let text = NoRequestsBanner.text(liveFor: 300, claudeCount: nil, route: route)
        XCTAssertEqual(
            text,
            "No request has reached this proxy since it started. "
                + "Claude is routed to https://gateway.example:9443 (settings.json).")
    }

    /// No process probe at all reads as "unknown", never as "no Claude
    /// running": the two must not collapse into one.
    func testTextIgnoresClaudeCountWhenNoProbeRan() {
        XCTAssertNotNil(
            NoRequestsBanner.text(liveFor: 600, claudeCount: nil, route: route))
    }

    /// With a real probe, zero running Claude processes withholds the
    /// banner. An idle Mac with no session anywhere is not the case this
    /// banner exists for.
    func testTextIsNilWhenAProbeFoundNoClaudeRunning() {
        XCTAssertNil(
            NoRequestsBanner.text(liveFor: 600, claudeCount: 0, route: route))
    }

    func testTextAppearsWhenAProbeFoundClaudeRunning() {
        XCTAssertNotNil(
            NoRequestsBanner.text(liveFor: 600, claudeCount: 2, route: route))
    }

    /// Beside Codex accounts the proxy does serve requests, so "no request" would be
    /// false: the sentence names Claude. The gates are the same ones.
    func testTextNamesClaudeBesideCodex() {
        XCTAssertEqual(
            NoRequestsBanner.text(liveFor: 300, claudeCount: nil, route: route, namesClaude: true),
            "No Claude request has reached this proxy since it started. "
                + "Claude is routed to https://gateway.example:9443 (settings.json).")
        XCTAssertNil(
            NoRequestsBanner.text(liveFor: 299, claudeCount: nil, route: route, namesClaude: true))
        XCTAssertNil(
            NoRequestsBanner.text(liveFor: 600, claudeCount: 0, route: route, namesClaude: true))
    }

    /// The shim never compiles the app target, so this pins the one call that
    /// chooses the sentence: the panel names Claude exactly when the fleet holds
    /// a Codex account.
    func testThePanelNamesClaudeWhenTheFleetHoldsCodex() throws {
        let fleetView = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .appendingPathComponent("Sources/TcrBar/FleetView.swift")
        let contents = try String(contentsOf: fleetView, encoding: .utf8)
        guard
            let start = contents.range(
                of: "private func noRequestsBannerText(_ fleet: Fleet) -> String? {"),
            let end = contents.range(
                of: "\n    }\n", range: start.upperBound..<contents.endIndex)
        else {
            XCTFail("noRequestsBannerText is gone from FleetView.swift")
            return
        }
        let body = contents[start.upperBound..<end.lowerBound]
        XCTAssertTrue(
            body.contains("namesClaude: fleet.holdsCodex"),
            "noRequestsBannerText no longer names Claude beside Codex: \(body)")
    }

    // MARK: - fixture

    private func decodeFleet(_ rows: [String]) throws -> Fleet {
        let json = "[\(rows.joined(separator: ","))]"
        return try Fleet.decode(Data(json.utf8))
    }

    private func account(
        name: String, requests: Int, source: String, disabled: Bool = false
    ) -> String {
        """
        {"name":"\(name)","priority":0,"status":"active",
         "disabled":\(disabled),"quota":0.5,"quotaState":"ok",
         "fiveHour":0.1,"sevenDay":0.1,
         "sevenDayOi":0.1,"sevenDayOiState":"ok","sevenDayOiResetAtMs":null,
         "held":[],
         "requests":\(requests),"inputTokens":\(requests),"outputTokens":\(requests),
         "cacheReadTokens":\(requests),"cacheHitRatio":0.0,"probeStatus":"ok",
         "probeError":null,
         "lastStreamError":null,"streamErrorCount":0,"source":"\(source)",
         "serverSha":"abc1234","serverDirty":false}
        """
    }

    /// ``account(name:requests:source:disabled:)`` as a Codex row.
    private func codexAccount(
        name: String, requests: Int, source: String, disabled: Bool = false
    ) -> String {
        let row = account(name: name, requests: requests, source: source, disabled: disabled)
        return "{\"provider\":\"codex\"," + row.drop { $0 != "{" }.dropFirst()
    }
}
