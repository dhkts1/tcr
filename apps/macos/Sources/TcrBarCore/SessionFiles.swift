import Foundation

/// The join for the Sessions tab: `~/.claude/sessions/*.json`, one file per
/// live Claude Code process, keyed by `sessionId` to line up with
/// ``Session/sessionId`` from the proxy's own wire.
///
/// One field of this file (`bridgeSessionId`) is read by the sessions-wire
/// lane server-side, not by this build — see `panel-tabs.md`'s "Not
/// verified" note on a resumed session's id. Nothing here re-derives that.
public struct SessionFile: Decodable, Equatable, Sendable {
    public let sessionId: String
    public let cwd: String?
    public let name: String?
    /// `"busy"`, `"idle"` or `"waiting"`, verbatim from the file — never
    /// pattern-matched here beyond ``SessionActivity``'s own mapping.
    public let status: String?
    public let updatedAt: Int64?
    /// The Claude Code process's own pid — the file is NAMED for it
    /// (`~/.claude/sessions/31389.json`) and carries it as a field too.
    ///
    /// Read for one purpose: the Tools tab matches a running Bash call to the
    /// shell this pid spawned, and offers a kill for that shell's process
    /// group (``ProcessMatch/shellChild(ofSessionPid:startedMs:in:tolerance:)``).
    /// Optional because a harness on the other end of the proxy that is not
    /// Claude Code writes no such file at all, and an older Claude Code may
    /// write one without the field: both mean "no process to match", which is
    /// the case that draws no stats and no button rather than a guess.
    public let pid: Int32?

    public init(
        sessionId: String,
        cwd: String? = nil,
        name: String? = nil,
        status: String? = nil,
        updatedAt: Int64? = nil,
        pid: Int32? = nil
    ) {
        self.sessionId = sessionId
        self.cwd = cwd
        self.name = name
        self.status = status
        self.updatedAt = updatedAt
        self.pid = pid
    }
}

/// Reads every session file on disk, keyed by ``SessionFile/sessionId``.
public enum SessionFiles {
    /// `~/.claude/sessions`.
    public static var defaultDirectory: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".claude", isDirectory: true)
            .appendingPathComponent("sessions", isDirectory: true)
    }

    /// One poll's worth of session files, tolerant of everything that can go
    /// wrong reading them — a missing directory (most machines running
    /// `tcr` have never run Claude Code's CLI at all) and a file that is not
    /// valid JSON or does not carry ``SessionFile``'s shape (a partial write
    /// mid-save is routine for a file another process rewrites continuously).
    /// Both skip rather than fail the whole read: tolerate a missing
    /// directory and unparsable files — skip, never fail the poll.
    public static func read(directory: URL = defaultDirectory) -> [String: SessionFile] {
        guard
            let urls = try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)
        else { return [:] }

        let decoder = JSONDecoder()
        var out: [String: SessionFile] = [:]
        for url in urls where url.pathExtension == "json" {
            guard
                let data = try? Data(contentsOf: url),
                let file = try? decoder.decode(SessionFile.self, from: data)
            else { continue }
            out[file.sessionId] = file
        }
        return out
    }
}

/// A live/idle/waiting reading for a joined session — the file side of
/// ``JoinedSession/activity``, kept as its own type rather than a raw string
/// the same way ``Account/AccountHealth`` keeps `status` from leaking its raw
/// spelling into every call site. `.unknown` is the join-has-no-file case,
/// not a fourth server-reported state.
public enum SessionActivity: Equatable, Sendable {
    case busy, idle, waiting, unknown

    init(rawStatus: String?) {
        switch rawStatus?.lowercased() {
        case "busy": self = .busy
        case "idle": self = .idle
        case "waiting": self = .waiting
        default: self = .unknown
        }
    }
}

/// A wire ``Session`` joined to its ``SessionFile``, if any — the row the
/// Sessions and Tools tabs actually draw.
///
/// `file == nil` is routine, not an error: a wire session with no file
/// shows its id's first 8 chars and no project, for the case where the harness on the other end of the proxy is
/// not Claude Code at all, or its session file has already aged out.
public struct JoinedSession: Identifiable, Equatable, Sendable {
    public let session: Session
    public let file: SessionFile?

    public init(session: Session, file: SessionFile?) {
        self.session = session
        self.file = file
    }

    public var id: String { session.sessionId }

    /// The file's `name`, else the session id's first 8 characters — never
    /// the bare full id, which is wider than this panel and no more readable.
    public var displayName: String {
        if let name = file?.name, !name.isEmpty { return name }
        return String(session.sessionId.prefix(8))
    }

    /// `cwd`'s last path component, `nil` when there is no file to read a
    /// `cwd` from at all. Never guessed from the session id.
    public var project: String? {
        guard let cwd = file?.cwd, !cwd.isEmpty else { return nil }
        return (cwd as NSString).lastPathComponent
    }

    /// A Claude session's reading comes from its Claude Code session file's `status`.
    ///
    /// A Codex session's is the one fact the wire states: `.busy` while one of its tool calls
    /// is running, `.unknown` otherwise. It is never inferred from codex's transcript, which
    /// this build does not read, and never from a file's status, which codex does not write;
    /// the rollout file joined to it gives its project, not its activity. So a Codex session
    /// is never "idle" or "waiting on you": its row says how long ago it was last seen.
    public var activity: SessionActivity {
        if session.provider == .codex {
            return session.tools.running.isEmpty ? .unknown : .busy
        }
        return SessionActivity(rawStatus: file?.status)
    }

    public var lastSeenAt: Date { Date(timeIntervalSince1970: Double(session.lastSeenMs) / 1000) }

    /// `"3m"`, `"2h"`, `"4d"` — the same tiering ``HeldWindow/duration(minutes:)``
    /// already gives a countdown, read backwards for an elapsed span instead.
    /// `now` is a required parameter, not a default, so a test stays
    /// deterministic the way every other clock-reading function here is.
    public func ageLabel(now: Date) -> String {
        let elapsedSeconds = max(0, now.timeIntervalSince(lastSeenAt))
        let minutes = Int((elapsedSeconds / 60).rounded())
        return minutes < 1 ? "now" : HeldWindow.duration(minutes: minutes)
    }
}

/// Joins the wire's sessions to the files on disk. A file with no matching
/// wire session is dropped — it never went through
/// the proxy — so this always returns exactly `sessions.count` rows.
public enum SessionJoin {
    public static func join(sessions: [Session], files: [String: SessionFile]) -> [JoinedSession] {
        sessions.map { JoinedSession(session: $0, file: files[$0.sessionId]) }
    }
}

/// The project of a Codex session, from the first line of the rollout file codex writes for
/// it under `$CODEX_HOME/sessions`.
///
/// What is read, and why it is enough. codex names each rollout for its thread,
/// `sessions/YYYY/MM/DD/rollout-<timestamp>-<thread id>.jsonl`, and after it reverts a thread,
/// `rollout-<timestamp>-<thread id>_<rollout id>.jsonl`, the thread id first either way. Line 1
/// is its `session_meta` record: the thread's id and the folder it runs in. The `session-id`
/// header codex sends, which the proxy names a Codex session by, is that thread's id. So a
/// session joins the file named for its id whose line 1 names the same id, and keeps the
/// folder as its project.
///
/// Only line 1 is read, and only for an id the proxy's own sessions named. The transcript
/// after it is never read: activity read from it would cost the privacy this costs none of.
/// It is the precedent Claude Code's session files already set (``SessionFiles``), which name
/// a session's folder and nothing it said. A compressed rollout (`.jsonl.zst`) and one codex
/// archived (`archived_sessions/`, beside `sessions/`) are not read; nor is any file when codex
/// keys its requests by something other than the thread, in which case no name matches.
public enum CodexRollouts {
    /// Files for the ids given that have a rollout with a matching line 1, keyed by id. The
    /// result's ``SessionFile`` carries the id and the folder only: no name, no status and no
    /// process, which codex's rollout does not hold.
    ///
    /// The walk goes newest first: at every level of `sessions/` the names sort descending, so
    /// the `YYYY/MM/DD` folders come newest first, and so do a folder's rollouts, by the
    /// timestamp they are named with. It stops once every id has a file, after the folder it
    /// is in, so the newest rollout of an id is the one read. A file whose line 1 does not
    /// hold is passed over for the next newest. Hidden files are skipped, and a symbolic link
    /// to a folder is not followed.
    public static func read(
        codexHome: String, sessionIds: Set<String>,
        readFirstLine: (URL) -> String? = { firstLine(at: $0) }
    ) -> [String: SessionFile] {
        guard !sessionIds.isEmpty else { return [:] }
        let sessions = URL(fileURLWithPath: codexHome, isDirectory: true)
            .appendingPathComponent("sessions", isDirectory: true)
        var found: [String: SessionFile] = [:]
        _ = walk(sessions, wanted: sessionIds, found: &found, readFirstLine: readFirstLine)
        return found
    }

    /// Line 1 of the file at `url`, without its newline, read `chunkSize` bytes at a time and
    /// no further than the first newline. `nil` when the file cannot be read, when line 1 is
    /// longer than `limit` bytes, which is read no further than the chunk that passes it, or
    /// when it is not UTF-8. codex sets no size for line 1; at codex's own instructions it is
    /// about 20 KB, so the 1 MiB limit is room, not a guess at the size.
    public static func firstLine(
        at url: URL, chunkSize: Int = 65_536, limit: Int = 1_048_576,
        onRead: (Int) -> Void = { _ in }
    ) -> String? {
        guard chunkSize > 0, let handle = try? FileHandle(forReadingFrom: url) else { return nil }
        defer { try? handle.close() }
        var line = Data()
        while let chunk = try? handle.read(upToCount: chunkSize), !chunk.isEmpty {
            onRead(chunk.count)
            if let newline = chunk.firstIndex(of: UInt8(ascii: "\n")) {
                line.append(chunk[chunk.startIndex..<newline])
                return line.count <= limit ? String(data: line, encoding: .utf8) : nil
            }
            line.append(chunk)
            guard line.count <= limit else { return nil }
        }
        // The file ended before a newline: all of it is its first line.
        return line.isEmpty ? nil : String(data: line, encoding: .utf8)
    }

    /// Reads one folder, then its subfolders, newest first; `true` once every wanted id has a
    /// file.
    private static func walk(
        _ directory: URL, wanted: Set<String>, found: inout [String: SessionFile],
        readFirstLine: (URL) -> String?
    ) -> Bool {
        guard
            let entries = try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: [.isDirectoryKey],
                options: [.skipsHiddenFiles])
        else { return false }
        var folders: [URL] = []
        var candidates: [String: [URL]] = [:]
        for entry in entries.sorted(by: { $0.lastPathComponent > $1.lastPathComponent }) {
            if (try? entry.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory == true {
                folders.append(entry)
                continue
            }
            guard let head = threadHead(ofRolloutNamed: entry.lastPathComponent) else { continue }
            for id in wanted where found[id] == nil && head.hasSuffix("-" + id) {
                candidates[id, default: []].append(entry)
            }
        }
        for (id, urls) in candidates {
            for url in urls {
                guard let line = readFirstLine(url), let file = sessionFile(line, id: id)
                else { continue }
                found[id] = file
                break
            }
        }
        if wanted.allSatisfy({ found[$0] != nil }) { return true }
        for folder in folders {
            if walk(folder, wanted: wanted, found: &found, readFirstLine: readFirstLine) {
                return true
            }
        }
        return false
    }

    /// A live rollout's name up to and including its thread id: without `.jsonl`, and without
    /// the `_<rollout id>` a reverted thread's file adds after the thread id. `nil` for any
    /// other name, a compressed `.jsonl.zst` included.
    private static func threadHead(ofRolloutNamed name: String) -> Substring? {
        guard name.hasPrefix("rollout-"), name.hasSuffix(".jsonl") else { return nil }
        let stem = name.dropLast(".jsonl".count)
        return stem.split(separator: "_", maxSplits: 1, omittingEmptySubsequences: false).first
    }

    /// The file line 1 makes for `id`: codex's `session_meta` record whose `payload.id` is
    /// `id`, with its `payload.cwd` as the folder. `nil` for any other line.
    private static func sessionFile(_ line: String, id: String) -> SessionFile? {
        guard
            let object = (try? JSONSerialization.jsonObject(with: Data(line.utf8)))
                as? [String: Any],
            object["type"] as? String == "session_meta",
            let payload = object["payload"] as? [String: Any],
            payload["id"] as? String == id
        else { return nil }
        return SessionFile(sessionId: id, cwd: payload["cwd"] as? String)
    }
}

/// ``CodexRollouts`` across polls: the panel asks every few seconds, and a rollout's line 1
/// never changes, so a join, once found, is not looked up again while its session is live.
/// An id with no rollout is not looked up again until a minute has passed, so a session codex
/// wrote no file for costs one walk a minute rather than one a poll.
///
/// What is remembered is for one codex home; pointed at another, the join starts over. A
/// session that leaves the list is forgotten.
public struct CodexRolloutJoin: Sendable {
    /// How long a miss is remembered before its id is looked up again.
    public static let missRetrySeconds: TimeInterval = 60

    private var codexHome: String?
    private var found: [String: SessionFile] = [:]
    private var missed: Set<String> = []
    /// When ``missed`` was last emptied. Every miss is forgotten once ``missRetrySeconds``
    /// have passed since then, together, so a poll retries them in one walk.
    private var missesSince: Date?

    public init() {}

    /// The rollout files for `sessionIds`, keyed by id, looking up only the ids it has neither
    /// found nor missed in the last minute.
    public mutating func files(
        for sessionIds: Set<String>, codexHome: String, now: Date,
        walk: (String, Set<String>) -> [String: SessionFile] = {
            CodexRollouts.read(codexHome: $0, sessionIds: $1)
        }
    ) -> [String: SessionFile] {
        if codexHome != self.codexHome {
            self.codexHome = codexHome
            found = [:]
            missed = []
            missesSince = now
        }
        if let since = missesSince, now < since || now.timeIntervalSince(since) >= Self.missRetrySeconds {
            missed = []
            missesSince = now
        }
        found = found.filter { sessionIds.contains($0.key) }
        missed.formIntersection(sessionIds)
        let wanted = sessionIds.subtracting(found.keys).subtracting(missed)
        if !wanted.isEmpty {
            let read = walk(codexHome, wanted)
            for id in wanted {
                if let file = read[id] { found[id] = file } else { missed.insert(id) }
            }
        }
        return found
    }
}
