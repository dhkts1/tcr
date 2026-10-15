import AppKit
import XCTest

@testable import TcrBarCore

/// What the Sessions and Tools tabs say once the fleet holds a Codex account: the footers, the
/// empty and old-server lines, the subtitle's lead, the per-pool summary, the RUNNING head and
/// the kill slot. A Claude-only fleet must read exactly as it did before Codex existed, so each
/// test pins that arm too.
///
/// Rows come from the canonical fixtures: alice is the one Claude account, bob and dana are
/// Codex accounts, and the sessions are alice's Claude session and bob's Codex one.
final class SessionsCodexCopyTests: XCTestCase {
    private let alice = "alice@example.com"
    private let bob = "bob@example.com/codex"
    private let dana = "dana@example.com"

    // MARK: - Fixtures

    /// The canonical status rows, decoded, keeping the named rows in that order. With
    /// `codexLastHourRequests`, every Codex row's `usage.lastHour.requests` is rewritten first,
    /// in the row's JSON, so the figure goes through the real decoder.
    private func accounts(_ names: [String], codexLastHourRequests: Int? = nil) throws -> [Account] {
        var rows = try XCTUnwrap(
            JSONSerialization.jsonObject(with: CanonicalFixtures.data("codex-status.json"))
                as? [[String: Any]])
        if let requests = codexLastHourRequests {
            rows = try rows.map { row in
                guard row["provider"] as? String == "codex" else { return row }
                var row = row
                var usage = try XCTUnwrap(row["usage"] as? [String: Any])
                var lastHour = try XCTUnwrap(usage["lastHour"] as? [String: Any])
                lastHour["requests"] = requests
                usage["lastHour"] = lastHour
                row["usage"] = usage
                return row
            }
        }
        let decoded = try Fleet.decode(JSONSerialization.data(withJSONObject: rows))
        XCTAssertEqual(decoded.unreadable, [])
        return try names.map { name in
            try XCTUnwrap(decoded.accounts.first { $0.name == name }, "no canonical \(name)")
        }
    }

    /// alice's Claude session and bob's Codex session, in that order.
    private func canonicalSessions() throws -> (alice: Session, bob: Session) {
        let sessions = try Fleet.decodeSessions(CanonicalFixtures.data("codex-sessions.json"))
            .sessions
        XCTAssertEqual(sessions.map(\.provider), [.anthropic, .codex])
        return (sessions[0], sessions[1])
    }

    private func fleet(
        _ names: [String], sessions: [Session], codexLastHourRequests: Int? = nil
    ) throws -> Fleet {
        Fleet(
            accounts: try accounts(names, codexLastHourRequests: codexLastHourRequests),
            sessions: sessions, sessionsSupported: true)
    }

    // MARK: - Sessions tab

    func testTheSessionsFooterFollowsTheFleet() throws {
        let s = try canonicalSessions()
        XCTAssertEqual(
            SessionsCodexCopy.sessionsFooter(try fleet([alice], sessions: [s.alice])),
            "proxy + session files")
        XCTAssertEqual(
            SessionsCodexCopy.sessionsFooter(try fleet([alice, bob], sessions: [s.alice, s.bob])),
            "proxy + session files")
        XCTAssertEqual(
            SessionsCodexCopy.sessionsFooter(try fleet([bob], sessions: [s.bob])),
            "proxy + codex session files")
    }

    func testTheEmptyDetailFollowsTheFleet() throws {
        XCTAssertEqual(
            SessionsCodexCopy.emptyDetail(try fleet([alice], sessions: [])),
            "Nothing has spoken to the proxy in the last hour.")
        XCTAssertEqual(
            SessionsCodexCopy.emptyDetail(try fleet([alice, bob], sessions: [])),
            "No Claude Code or Codex session has spoken to the proxy in the last hour.")
        XCTAssertEqual(
            SessionsCodexCopy.emptyDetail(try fleet([bob, dana], sessions: [])),
            "No Codex session has spoken to the proxy in the last hour.")
    }

    /// A proxy built before Codex sessions records none, so a Codex row that served requests
    /// in the last hour beside a sessions list with no Codex session in it means the proxy is
    /// too old, not that codex was silent. Any Codex session on the list proves the proxy
    /// records them, and with no Codex traffic there is nothing to explain.
    func testTheOldServerLineNeedsCodexTrafficAndNoProviderOnAnySession() throws {
        let s = try canonicalSessions()
        let seven =
            "Codex served 7 requests in the last hour, but no Codex session was reported. Requests without a session-id are counted without a session row."
        XCTAssertEqual(
            SessionsCodexCopy.trafficWithoutSessionsLine(try fleet([alice, bob], sessions: [s.alice])), seven)
        XCTAssertEqual(
            SessionsCodexCopy.trafficWithoutSessionsLine(try fleet([alice, bob], sessions: [])), seven)
        XCTAssertNil(
            SessionsCodexCopy.trafficWithoutSessionsLine(
                try fleet([alice, bob], sessions: [s.alice, s.bob])))
        XCTAssertNil(SessionsCodexCopy.trafficWithoutSessionsLine(try fleet([alice], sessions: [s.alice])))
        XCTAssertNil(
            SessionsCodexCopy.trafficWithoutSessionsLine(
                try fleet([alice, bob], sessions: [s.alice], codexLastHourRequests: 0)))
        XCTAssertEqual(
            SessionsCodexCopy.trafficWithoutSessionsLine(
                try fleet([alice, bob], sessions: [s.alice], codexLastHourRequests: 1)),
            "Codex served 1 request in the last hour, but no Codex session was reported. Requests without a session-id are counted without a session row."
        )
    }

    /// In a mixed fleet a Codex session's subtitle leads with the provider; alone in a
    /// Codex-only fleet it needs no lead, and a Claude session never gets one.
    func testTheSubtitleLeadsWithCodexInAMixedFleet() throws {
        let s = try canonicalSessions()
        let mixed = try fleet([alice, bob], sessions: [s.alice, s.bob])
        XCTAssertEqual(SessionsCodexCopy.subtitleLead(for: s.bob, in: mixed), "Codex")
        XCTAssertNil(SessionsCodexCopy.subtitleLead(for: s.alice, in: mixed))
        XCTAssertNil(
            SessionsCodexCopy.subtitleLead(for: s.bob, in: try fleet([bob], sessions: [s.bob])))

        XCTAssertEqual(SessionsCodexCopy.trailing(age: "2m"), "2m ago")
        XCTAssertEqual(SessionsCodexCopy.trailing(age: "now"), "now")
    }

    /// Each pool gets its own clauses: Claude's money and account ratio from Claude rows and
    /// sessions alone, then Codex's tokens and ratio. The fleet's tool errors close the line.
    func testTheMixedSummaryHasOneClauseGroupPerPool() throws {
        let s = try canonicalSessions()
        let mixed = try fleet([alice, bob], sessions: [s.alice, s.bob])
        let claude = Fleet(accounts: try accounts([alice]), sessions: [s.alice], sessionsSupported: true)
        let expected = [
            "Claude " + (try XCTUnwrap(claude.burnRateSegment)),
            QuotaFormat.usd(claude.todayCost) + " today",
            QuotaFormat.usd(claude.sessionsLiveCost) + " live",
            "1/1 accounts",
            "Codex " + QuotaFormat.tokens(22_000) + " tokens today",
            "1/1 accounts",
        ]
        XCTAssertEqual(SessionsCodexCopy.summaryClauses(mixed), expected)

        // A session with no account and no cost: it changes neither pool's figures, and its
        // errors are the fleet's.
        let erroring = try XCTUnwrap(
            Fleet.decodeSessions(
                Data(
                    """
                    {"supported": true, "sessions": [
                      {"sessionId": "a1f3c2d4-0000-4000-8000-000000000009",
                       "firstSeenMs": 1767223200000, "lastSeenMs": 1767225597000,
                       "tools": {"calls": 3, "errors": 3}}]}
                    """.utf8)
            ).sessions.first)
        let withErrors = try fleet([alice, bob], sessions: [s.alice, s.bob, erroring])
        XCTAssertEqual(SessionsCodexCopy.summaryClauses(withErrors), expected + ["3 err"])

        XCTAssertNil(SessionsCodexCopy.summaryClauses(try fleet([alice], sessions: [s.alice])))
    }

    /// A server that records no Codex session cannot say how many Codex accounts are in use,
    /// so the Codex ratio is left out rather than read as zero.
    func testTheCodexRatioIsLeftOutWithNoCodexSession() throws {
        let s = try canonicalSessions()
        let clauses = try XCTUnwrap(
            SessionsCodexCopy.summaryClauses(try fleet([alice, bob], sessions: [s.alice])))
        XCTAssertEqual(clauses.last, "Codex " + QuotaFormat.tokens(22_000) + " tokens today")
        XCTAssertEqual(clauses.filter { $0.hasSuffix(" accounts") }, ["1/1 accounts"])
    }

    /// A Codex-only summary is the Codex group alone, told in tokens: no request is counted
    /// as unpriced, because a ChatGPT plan prices none of them.
    func testACodexOnlySummaryCountsNoUnpricedRequests() throws {
        let s = try canonicalSessions()
        let clauses = SessionsCodexCopy.summaryClauses(try fleet([bob, dana], sessions: [s.bob]))
        XCTAssertEqual(clauses, ["Codex " + QuotaFormat.tokens(282_000) + " tokens today", "1/2 accounts"])
        XCTAssertFalse(clauses?.contains { $0.contains("unpriced") } ?? true)
    }

    // MARK: - Tools tab

    func testTheToolsFooter() {
        XCTAssertEqual(
            SessionsCodexCopy.toolsFooter(holdsCodex: true),
            "request and response bodies · nothing logged")
        XCTAssertEqual(
            SessionsCodexCopy.toolsFooter(holdsCodex: false),
            "request bodies only · nothing logged")
    }

    func testNoKillIsSaidInTheSlot() {
        XCTAssertEqual(SessionsCodexCopy.noKill, "no Kill")
        XCTAssertEqual(
            SessionsCodexCopy.noKillHelp,
            "tcr cannot match this call to a codex process, so it offers no Kill.")
    }

    /// The words take the ✕'s place, so they must fit the slot the row gives them, measured
    /// at the panel's mute size the way the card measures its own strings: the comfortable
    /// size, the larger of the two, so the fit holds at Compact as well.
    func testNoKillFitsItsSlot() throws {
        let slot = try v4Token("static let noKillWidth: CGFloat = ([0-9.]+)\\b")
        let size = try v4Token(
            "staticvarmuteSize:CGFloat\\{compact\\?[0-9.]+:([0-9.]+)\\}", squashed: true)
        let width = NameRowFit.textWidth(SessionsCodexCopy.noKill, size: size, weight: .regular)
        XCTAssertLessThanOrEqual(
            width, slot, String(format: "\"no Kill\" is %.1f pt in a %.1f pt slot", width, slot))
    }

    // MARK: - Reading V4's tokens out of the app target's source

    /// The number the pattern's first group captures in `PanelV4/V4.swift`, read from the
    /// source with every space removed when `squashed`.
    private func v4Token(_ pattern: String, squashed: Bool = false) throws -> CGFloat {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<3 { url = url.deletingLastPathComponent() }  // file, TcrBarTests, Tests
        let raw = try String(
            contentsOf: url.appendingPathComponent("Sources/TcrBar/PanelV4/V4.swift"),
            encoding: .utf8)
        let source = squashed ? raw.components(separatedBy: .whitespacesAndNewlines).joined() : raw
        let regex = try NSRegularExpression(pattern: pattern)
        let match = try XCTUnwrap(
            regex.firstMatch(in: source, range: NSRange(source.startIndex..., in: source)),
            "V4.swift no longer matches \(pattern)")
        let value = try XCTUnwrap(Range(match.range(at: 1), in: source).map { source[$0] })
        return CGFloat(try XCTUnwrap(Double(value)))
    }
}
