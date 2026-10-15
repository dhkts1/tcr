import XCTest

@testable import TcrBarCore

final class SessionFilesTests: XCTestCase {
    private func makeTempDirectory() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-session-files-tests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// A missing directory reads as no files at all, never a thrown error:
    /// tolerate a missing directory, skip, never fail the poll. Most
    /// machines running `tcr` have never run Claude
    /// Code's own CLI, so this is the routine case, not the edge one.
    func testMissingDirectoryReadsAsEmpty() {
        let missing = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-does-not-exist-\(UUID().uuidString)")
        XCTAssertTrue(SessionFiles.read(directory: missing).isEmpty)
    }

    /// A file that does not parse is skipped, not fatal — the sibling files
    /// in the same directory still decode.
    func testUnparsableFileIsSkippedWithoutFailingTheRead() throws {
        let dir = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }

        try "not json at all".write(
            to: dir.appendingPathComponent("broken.json"), atomically: true, encoding: .utf8)
        let goodJSON = """
            {"sessionId":"good-session","cwd":"/Users/alice/git/example",
             "name":"example-c1","status":"busy"}
            """
        try goodJSON.write(
            to: dir.appendingPathComponent("good.json"), atomically: true, encoding: .utf8)

        let files = SessionFiles.read(directory: dir)
        XCTAssertEqual(files.count, 1)
        XCTAssertEqual(files["good-session"]?.name, "example-c1")
    }

    /// A non-`.json` file in the same directory is ignored outright, not
    /// attempted and skipped — it never counts against the read.
    func testNonJsonFilesAreIgnored() throws {
        let dir = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        try "hello".write(to: dir.appendingPathComponent("notes.txt"), atomically: true, encoding: .utf8)
        XCTAssertTrue(SessionFiles.read(directory: dir).isEmpty)
    }

    // MARK: JoinedSession

    private func session(id: String, account: String? = nil, lastSeenMs: Int64 = 0) -> Session {
        Session(
            sessionId: id, account: account, model: "claude-opus-5", firstSeenMs: 0, lastSeenMs: lastSeenMs)
    }

    /// A wire session with no file shows its id's
    /// first 8 chars and no project.
    func testWireSessionWithNoFileShowsIdHeadAndNoProject() {
        let joined = JoinedSession(session: session(id: "11111111-aaaa-bbbb"), file: nil)
        XCTAssertEqual(joined.displayName, "11111111")
        XCTAssertNil(joined.project)
        XCTAssertEqual(joined.activity, .unknown)
    }

    /// A joined session reads its name and project from the file, and its
    /// activity from the file's `status`.
    func testJoinedSessionReadsNameProjectAndActivityFromTheFile() {
        let file = SessionFile(
            sessionId: "11111111-aaaa-bbbb", cwd: "/Users/alice/git/teamclaude-rs",
            name: "teamclaude-rs-c7", status: "busy")
        let joined = JoinedSession(session: session(id: "11111111-aaaa-bbbb"), file: file)
        XCTAssertEqual(joined.displayName, "teamclaude-rs-c7")
        XCTAssertEqual(joined.project, "teamclaude-rs")
        XCTAssertEqual(joined.activity, .busy)
    }

    /// A file with no matching wire session is dropped — it never went
    /// through the proxy — so the join always returns exactly
    /// `sessions.count` rows, never `files.count`.
    func testAFileWithNoMatchingWireSessionIsDropped() {
        let sessions = [session(id: "s1")]
        let files = [
            "s1": SessionFile(sessionId: "s1", name: "kept"),
            "orphan": SessionFile(sessionId: "orphan", name: "never went through the proxy"),
        ]
        let joined = SessionJoin.join(sessions: sessions, files: files)
        XCTAssertEqual(joined.count, 1)
        XCTAssertEqual(joined[0].displayName, "kept")
    }

    func testAgeLabelTiers() {
        let joined = JoinedSession(session: session(id: "s1", lastSeenMs: 0), file: nil)
        let now = Date(timeIntervalSince1970: 5 * 60)  // 5 minutes later
        XCTAssertEqual(joined.ageLabel(now: now), "5m")
    }

    // MARK: - Codex sessions

    /// Sessions decoded from the wire's JSON: each entry is one row's members beyond the keys
    /// every row carries.
    private func decodedSessions(_ rows: [String]) throws -> [Session] {
        let body = rows.map { #"{"firstSeenMs": 0, "lastSeenMs": 0, \#($0)}"# }
        let payload = #"{"supported": true, "sessions": [\#(body.joined(separator: ","))]}"#
        let read = try Fleet.decodeSessions(Data(payload.utf8))
        XCTAssertEqual(read.unreadable, [])
        return read.sessions
    }

    /// A Codex session is busy exactly while one of its calls runs, which the wire states.
    /// Nothing else is read for it: not a session file's status, and never codex's transcript.
    /// A Claude session keeps reading its file.
    func testACodexSessionIsBusyOnlyWhileACallRuns() throws {
        let sessions = try decodedSessions([
            #""sessionId": "c1", "provider": "codex", "tools": {"running": [{"tool": "exec_command", "startedMs": 0}]}"#,
            #""sessionId": "c2", "provider": "codex""#,
            #""sessionId": "a1""#,
        ])
        let (running, quiet, claude) = (sessions[0], sessions[1], sessions[2])

        XCTAssertEqual(JoinedSession(session: running, file: nil).activity, .busy)
        XCTAssertEqual(
            JoinedSession(session: running, file: SessionFile(sessionId: "c1", status: "idle"))
                .activity,
            .busy)
        XCTAssertEqual(JoinedSession(session: quiet, file: nil).activity, .unknown)
        XCTAssertEqual(
            JoinedSession(session: quiet, file: SessionFile(sessionId: "c2", status: "waiting"))
                .activity,
            .unknown)
        XCTAssertEqual(
            JoinedSession(session: claude, file: SessionFile(sessionId: "a1", status: "waiting"))
                .activity,
            .waiting)
    }

    // MARK: - Codex rollout files

    private let codexId = "019a3c5e-0000-7000-8000-000000000002"

    /// codex's line 1 for a rollout: its `session_meta` record, naming the thread and its
    /// working folder.
    private func metaLine(id: String, cwd: String = "/Users/example/git/teamclaude-rs") -> String {
        #"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"\#(id)","cwd":"\#(cwd)"}}"#
    }

    /// Writes `lines` as a file at `relative` under `home`, one per line, making its folders.
    @discardableResult
    private func write(_ lines: [String], at relative: String, in home: URL) throws -> URL {
        let url = home.appendingPathComponent(relative)
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try (lines.joined(separator: "\n") + "\n").write(to: url, atomically: true, encoding: .utf8)
        return url
    }

    /// The join reads line 1 of the rollout file named for a Codex session, keeps its folder,
    /// and makes nothing else up: no name, no status, no process.
    func testACodexRolloutIsJoinedByItsFirstLineOnly() throws {
        let home = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: home) }
        try write(
            [
                metaLine(id: codexId),
                #"{"type":"response_item","payload":{"type":"message","content":"transcript text"}}"#,
            ],
            at: "sessions/2026/01/01/rollout-2026-01-01T00-00-00-\(codexId).jsonl", in: home)

        let files = CodexRollouts.read(codexHome: home.path, sessionIds: [codexId])
        XCTAssertEqual(Array(files.keys), [codexId])
        let file = try XCTUnwrap(files[codexId])
        XCTAssertEqual(file.sessionId, codexId)
        XCTAssertEqual(file.cwd, "/Users/example/git/teamclaude-rs")
        XCTAssertNil(file.name)
        XCTAssertNil(file.status)
        XCTAssertNil(file.pid)

        let bob = try XCTUnwrap(
            Fleet.decodeSessions(CanonicalFixtures.data("codex-sessions.json")).sessions.last)
        let joined = JoinedSession(session: bob, file: file)
        XCTAssertEqual(joined.project, "teamclaude-rs")
        XCTAssertEqual(joined.displayName, "019a3c5e")

        let unknown = "019a3c5e-0000-7000-8000-000000000099"
        let both = CodexRollouts.read(codexHome: home.path, sessionIds: [codexId, unknown])
        XCTAssertEqual(Array(both.keys), [codexId])
    }

    /// The name only says which file to open: line 1 must be codex's `session_meta` record and
    /// name the same thread, or the file joins nothing. A file whose name is not a rollout of
    /// that id is never opened at all.
    func testAMismatchedIdJoinsNothing() throws {
        let home = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: home) }
        let idA = "019a3c5e-0000-7000-8000-00000000000a"
        let idB = "019a3c5e-0000-7000-8000-00000000000b"
        let idC = "019a3c5e-0000-7000-8000-00000000000c"
        try write(
            [metaLine(id: idB)],
            at: "sessions/2026/01/01/rollout-2026-01-01T00-00-00-\(idA).jsonl", in: home)
        XCTAssertEqual(CodexRollouts.read(codexHome: home.path, sessionIds: [idA]), [:])

        try write(
            [metaLine(id: idC).replacingOccurrences(of: "session_meta", with: "turn_context")],
            at: "sessions/2026/01/01/rollout-2026-01-01T01-00-00-\(idC).jsonl", in: home)
        XCTAssertEqual(CodexRollouts.read(codexHome: home.path, sessionIds: [idC]), [:])

        for name in [
            "rollout-2026-01-02T00-00-00-\(idB)-copy.jsonl",
            "notes-2026-01-02T00-00-00-\(idB).jsonl",
            "rollout-2026-01-02T00-00-00-\(idB).json",
        ] {
            try write([metaLine(id: idB)], at: "sessions/2026/01/02/\(name)", in: home)
        }
        var handed: [URL] = []
        let found = CodexRollouts.read(
            codexHome: home.path, sessionIds: [idB],
            readFirstLine: { url in
                handed.append(url)
                return CodexRollouts.firstLine(at: url)
            })
        XCTAssertEqual(found, [:])
        XCTAssertEqual(handed.map(\.lastPathComponent), [])
    }

    /// After codex reverts a thread it writes a new rollout, `…-<id>_<rollout id>.jsonl`, and
    /// the newest file for the id is the one read. A newer file whose line 1 does not hold
    /// goes unjoined, and the next newest is tried.
    func testARevertedRolloutIsReadAndTheNewestWins() throws {
        let home = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: home) }
        let rolloutId = "019a3c5e-0000-7000-8000-0000000000ff"
        try write(
            [metaLine(id: codexId, cwd: "/Users/example/git/first")],
            at: "sessions/2026/01/01/rollout-2026-01-01T00-00-00-\(codexId).jsonl", in: home)
        try write(
            [metaLine(id: codexId, cwd: "/Users/example/git/same-day")],
            at: "sessions/2026/01/01/rollout-2026-01-01T08-00-00-\(codexId)_\(rolloutId).jsonl",
            in: home)
        try write(
            [metaLine(id: codexId, cwd: "/Users/example/git/reverted")],
            at: "sessions/2026/01/03/rollout-2026-01-03T09-30-00-\(codexId)_\(rolloutId).jsonl",
            in: home)
        try write(
            [#"{"type":"response_item","payload":{"type":"message"}}"#],
            at: "sessions/2026/01/03/rollout-2026-01-03T10-00-00-\(codexId)_\(rolloutId).jsonl",
            in: home)

        var handed: [String] = []
        let found = CodexRollouts.read(
            codexHome: home.path, sessionIds: [codexId],
            readFirstLine: { url in
                handed.append(url.lastPathComponent)
                return CodexRollouts.firstLine(at: url)
            })
        XCTAssertEqual(found[codexId]?.cwd, "/Users/example/git/reverted")
        // The newest first, and nothing older once one holds: the reverted file in the newest
        // folder is the answer, so neither January 1 file is opened.
        XCTAssertEqual(
            handed,
            [
                "rollout-2026-01-03T10-00-00-\(codexId)_\(rolloutId).jsonl",
                "rollout-2026-01-03T09-30-00-\(codexId)_\(rolloutId).jsonl",
            ])

        // Within one folder, the later file wins too.
        let sameDay = CodexRollouts.read(
            codexHome: home.path, sessionIds: [codexId],
            readFirstLine: { url in
                url.path.contains("/2026/01/03/") ? nil : CodexRollouts.firstLine(at: url)
            })
        XCTAssertEqual(sameDay[codexId]?.cwd, "/Users/example/git/same-day")
    }

    /// Only a live rollout is read: a compressed one, and one codex archived, are not opened.
    func testOnlyALiveRolloutUnderSessionsIsRead() throws {
        let home = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: home) }
        try write(
            [metaLine(id: codexId)],
            at: "sessions/2026/01/01/rollout-2026-01-01T00-00-00-\(codexId).jsonl.zst", in: home)
        try write(
            [metaLine(id: codexId)],
            at: "archived_sessions/rollout-2026-01-01T00-00-00-\(codexId).jsonl", in: home)

        var handed: [URL] = []
        let found = CodexRollouts.read(
            codexHome: home.path, sessionIds: [codexId],
            readFirstLine: { url in
                handed.append(url)
                return CodexRollouts.firstLine(at: url)
            })
        XCTAssertEqual(found, [:])
        XCTAssertEqual(handed, [])
    }

    /// The reader stops at the first newline: the transcript after line 1 is never read.
    func testNothingPastLineOneIsRead() throws {
        let home = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: home) }
        let line = metaLine(id: codexId)
        let url = try write(
            [line, String(repeating: "x", count: 1_048_576)],
            at: "sessions/2026/01/01/rollout-2026-01-01T00-00-00-\(codexId).jsonl", in: home)

        var total = 0
        XCTAssertEqual(CodexRollouts.firstLine(at: url, chunkSize: 512, onRead: { total += $0 }), line)
        XCTAssertLessThanOrEqual(total, 512)
    }

    /// codex sets no size for line 1, so the reader does: a first line longer than its limit
    /// is unreadable, and the reader stops reading once it is past the limit.
    func testALineOneOverTheLimitIsUnreadable() throws {
        let home = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: home) }
        let url = try write(
            [String(repeating: "x", count: 100), "second line"],
            at: "sessions/2026/01/01/rollout-2026-01-01T00-00-00-\(codexId).jsonl", in: home)

        var total = 0
        XCTAssertNil(CodexRollouts.firstLine(at: url, chunkSize: 16, limit: 64, onRead: { total += $0 }))
        XCTAssertLessThanOrEqual(total, 64 + 16)
        XCTAssertEqual(
            CodexRollouts.firstLine(at: url, chunkSize: 16, limit: 100),
            String(repeating: "x", count: 100))

        let long = try write(
            [String(repeating: "x", count: 1_048_577)],
            at: "sessions/2026/01/02/rollout-2026-01-02T00-00-00-\(codexId).jsonl", in: home)
        XCTAssertNil(CodexRollouts.firstLine(at: long))
    }

    /// A join found is asked for once; an id with no rollout is asked again only after a
    /// minute, so a session codex wrote no file for costs one walk a minute, not one a poll.
    func testAJoinIsLookedUpOnceAndAMissRetriesAfterAMinute() {
        let t0 = Date(timeIntervalSince1970: 1_767_225_600)
        let home = "/Users/example/.codex"
        let fileA = SessionFile(sessionId: "A", cwd: "/Users/example/git/a")
        var walks: [Set<String>] = []
        var join = CodexRolloutJoin()
        func files(_ ids: Set<String>, at seconds: TimeInterval) -> [String: SessionFile] {
            join.files(
                for: ids, codexHome: home, now: t0.addingTimeInterval(seconds),
                walk: { _, wanted in
                    walks.append(wanted)
                    return wanted.contains("A") ? ["A": fileA] : [:]
                })
        }

        XCTAssertEqual(files(["A"], at: 0), ["A": fileA])
        XCTAssertEqual(walks, [["A"]])
        XCTAssertEqual(files(["A"], at: 3), ["A": fileA])
        XCTAssertEqual(walks, [["A"]])
        XCTAssertEqual(files(["B"], at: 3), [:])
        XCTAssertEqual(walks, [["A"], ["B"]])
        XCTAssertEqual(files(["B"], at: 30), [:])
        XCTAssertEqual(walks, [["A"], ["B"]])
        XCTAssertEqual(files(["B"], at: 61), [:])
        XCTAssertEqual(walks, [["A"], ["B"], ["B"]])
    }

    /// Joins are remembered for one codex home: pointed at another, the join walks afresh.
    func testANewCodexHomeIsWalkedAfresh() {
        let t0 = Date(timeIntervalSince1970: 1_767_225_600)
        var walked: [String] = []
        var join = CodexRolloutJoin()
        for home in ["/Users/example/.codex", "/Users/example/.codex", "/Users/example/.codex-b"] {
            _ = join.files(
                for: ["A"], codexHome: home, now: t0,
                walk: { codexHome, _ in
                    walked.append(codexHome)
                    return ["A": SessionFile(sessionId: "A", cwd: "/Users/example/git/a")]
                })
        }
        XCTAssertEqual(walked, ["/Users/example/.codex", "/Users/example/.codex-b"])
    }
}
