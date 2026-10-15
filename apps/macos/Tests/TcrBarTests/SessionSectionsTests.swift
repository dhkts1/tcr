import XCTest

@testable import TcrBarCore

/// ``SessionSections/byProject(_:)`` — the Sessions tab's project grouping.
/// Fixtures use obviously-fake session ids, account names and project paths
/// only (this repo is public — see CLAUDE.md).
final class SessionSectionsTests: XCTestCase {
    private func joined(
        id: String, account: String? = nil, cwd: String?, costUsd: Double? = nil,
        errors: Int = 0
    ) -> JoinedSession {
        JoinedSession(
            session: Session(
                sessionId: id, account: account, firstSeenMs: 0, lastSeenMs: 0,
                tools: SessionTools(errors: errors), costUsd: costUsd),
            file: SessionFile(sessionId: id, cwd: cwd))
    }

    /// The measured shape this unit exists for: grouping by project splits a
    /// fleet an account-keyed grouping would have piled into one bucket.
    func testSessionsGroupByProjectNotAccount() {
        let sessions = [
            joined(id: "s1", account: "acct-a", cwd: "/Users/alice/token", costUsd: 5),
            joined(id: "s2", account: "acct-a", cwd: "/Users/alice/token", costUsd: 3),
            joined(id: "s3", account: "acct-a", cwd: "/Users/alice/teamclaude-rs", costUsd: 1),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(sections.map(\.key), [.named("token"), .named("teamclaude-rs")])
        XCTAssertEqual(sections[0].rows.map(\.id), ["s1", "s2"])
    }

    /// A project with fewer than two sessions draws no header — its session
    /// is still IN the list, just flagged as headerless, so the renderer can
    /// draw it as a plain tagged row instead of a one-row card.
    func testAProjectWithFewerThanTwoSessionsHasNoHeader() {
        let sessions = [
            joined(id: "s1", cwd: "/Users/alice/token"),
            joined(id: "s2", cwd: "/Users/alice/token"),
            joined(id: "s3", cwd: "/Users/alice/widgets"),
        ]
        let sections = SessionSections.byProject(sessions)
        let token = try! XCTUnwrap(sections.first { $0.key == .named("token") })
        let widgets = try! XCTUnwrap(sections.first { $0.key == .named("widgets") })
        XCTAssertTrue(token.hasHeader)
        XCTAssertFalse(widgets.hasHeader)
    }

    /// A session with no file, or a file with no `cwd`, has no project — those
    /// collapse into ONE `.unassigned` bucket rather than each becoming its
    /// own headerless singleton, matching ``FleetGroupKey/ungrouped``'s own
    /// collapsing rule for accounts.
    func testSessionsWithNoProjectCollapseIntoOneUnassignedBucket() {
        let sessions = [
            joined(id: "s1", cwd: nil),
            joined(id: "s2", cwd: ""),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(sections.count, 1)
        XCTAssertEqual(sections[0].key, .unassigned)
        XCTAssertEqual(sections[0].rows.count, 2)
    }

    /// Sections order by spend, highest first — the bridge's own rule — and a
    /// section with nothing priced sorts LAST rather than claiming a $0 spend
    /// it never measured.
    func testSectionsOrderBySpendHighestFirstAndUnpricedLast() {
        let sessions = [
            joined(id: "s1", cwd: "/Users/alice/low", costUsd: 1),
            joined(id: "s2", cwd: "/Users/alice/high", costUsd: 100),
            joined(id: "s3", cwd: "/Users/alice/unpriced", costUsd: nil),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(
            sections.map(\.key), [.named("high"), .named("low"), .named("unpriced")])
    }

    func testHeaderSummaryNamesCountSpendAndErrors() {
        let sessions = [
            joined(id: "s1", cwd: "/Users/alice/token", costUsd: 500, errors: 100),
            joined(id: "s2", cwd: "/Users/alice/token", costUsd: 480, errors: 66),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(sections[0].headerSummary, "2 · $980 · 166 err")
    }

    /// Zero errors drops the clause entirely rather than printing "0 err".
    func testHeaderSummaryDropsTheErrorClauseAtZero() {
        let sessions = [
            joined(id: "s1", cwd: "/Users/alice/token", costUsd: 10),
            joined(id: "s2", cwd: "/Users/alice/token", costUsd: 10),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(sections[0].headerSummary, "2 · $20.0")
    }

    func testPartlyPricedCodexSessionsReportOnlyALowerBound() {
        for (cost, expected) in [(0.4, "1 · $0.40+"), (0.0, "1"), (0.001, "1")] {
            let session = Session(
                sessionId: "11111111-1111-1111-1111-111111111111", firstSeenMs: 0,
                lastSeenMs: 0, requests: 7, costUsd: cost, provider: .codex,
                unpricedRequests: 3)
            let section = SessionProjectSection(
                key: .unassigned, rows: [JoinedSession(session: session, file: nil)])
            XCTAssertEqual(section.headerSummary, expected)
        }
    }

    // MARK: - Codex sessions

    /// The canonical sessions: alice's Claude session first, bob's Codex session second.
    private func canonicalSessions() throws -> [Session] {
        let read = try Fleet.decodeSessions(CanonicalFixtures.data("codex-sessions.json"))
        XCTAssertEqual(read.sessions.map(\.provider), [.anthropic, .codex])
        return read.sessions
    }

    /// A Codex session joined to the project its rollout file names sits in that project
    /// beside the Claude session working in the same folder, under one header.
    func testACodexSessionGroupsByItsRolloutProject() throws {
        let sessions = try canonicalSessions()
        let cwd = "/Users/example/git/teamclaude-rs"
        let rows = [
            JoinedSession(
                session: sessions[0],
                file: SessionFile(sessionId: sessions[0].sessionId, cwd: cwd)),
            JoinedSession(
                session: sessions[1],
                file: SessionFile(sessionId: "019a3c5e-0000-7000-8000-000000000002", cwd: cwd)),
        ]
        let sections = SessionSections.byProject(rows)
        XCTAssertEqual(sections.map(\.key), [.named("teamclaude-rs")])
        XCTAssertEqual(sections.first?.rows.map(\.id), sessions.map(\.sessionId))
        XCTAssertEqual(sections.first?.hasHeader, true)
    }

    /// With no rollout file joined, a Codex session has no project: it sits under Unassigned
    /// and is named by its id's head, never guessed.
    func testAnUnjoinedCodexSessionStaysUnassignedWithItsIdHead() throws {
        let codex = JoinedSession(session: try canonicalSessions()[1], file: nil)
        XCTAssertEqual(SessionSections.byProject([codex]).map(\.key), [.unassigned])
        XCTAssertEqual(codex.displayName, "019a3c5e")
    }

    /// A section whose Codex sessions priced nothing has no spend, never `$0.00`, and sorts
    /// after a section that spent something.
    func testASectionOfUnpricedCodexSessionsShowsNoSpend() throws {
        let payload = """
            {"supported": true, "sessions": [
              {"sessionId": "019a3c5e-0000-7000-8000-000000000011", "provider": "codex",
               "firstSeenMs": 0, "lastSeenMs": 0, "requests": 7, "unpricedRequests": 7,
               "costUsd": 0.0},
              {"sessionId": "019a3c5e-0000-7000-8000-000000000012", "provider": "codex",
               "firstSeenMs": 0, "lastSeenMs": 0, "requests": 3, "unpricedRequests": 3,
               "costUsd": 0.0},
              {"sessionId": "a1f3c2d4-0000-4000-8000-000000000013",
               "firstSeenMs": 0, "lastSeenMs": 0, "requests": 2, "costUsd": 0.3}]}
            """
        let sessions = try Fleet.decodeSessions(Data(payload.utf8)).sessions
        XCTAssertEqual(sessions.count, 3)
        let codexCwd = "/Users/example/git/codex-only"
        let rows = [
            JoinedSession(
                session: sessions[0],
                file: SessionFile(sessionId: sessions[0].sessionId, cwd: codexCwd)),
            JoinedSession(
                session: sessions[1],
                file: SessionFile(sessionId: sessions[1].sessionId, cwd: codexCwd)),
            JoinedSession(
                session: sessions[2],
                file: SessionFile(
                    sessionId: sessions[2].sessionId, cwd: "/Users/example/git/priced")),
        ]
        let sections = SessionSections.byProject(rows)
        XCTAssertEqual(sections.map(\.key), [.named("priced"), .named("codex-only")])
        let codex = try XCTUnwrap(sections.last)
        XCTAssertNil(codex.spend)
        XCTAssertEqual(codex.headerSummary, "2")
    }
}
