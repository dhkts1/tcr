import Foundation

/// A poll's session-file lookup. The join is copied in and returned so the view
/// publishes one complete reading and retains the miss cache between polls.
public enum PanelSessionFiles {
    public struct Reading: Sendable {
        public let files: [String: SessionFile]
        public let join: CodexRolloutJoin
    }

    @MainActor
    public static func read(
        codexHome: String?, sessionIds: Set<String>, join: CodexRolloutJoin, now: Date,
        readClaude: @escaping @Sendable () -> [String: SessionFile] = { SessionFiles.read() },
        walk: @escaping @Sendable (String, Set<String>) -> [String: SessionFile] = {
            CodexRollouts.read(codexHome: $0, sessionIds: $1)
        }
    ) async -> Reading {
        await Task.detached(priority: .utility) {
            var join = join
            let claude = readClaude()
            guard let codexHome else { return Reading(files: claude, join: join) }
            let rollouts = join.files(for: sessionIds, codexHome: codexHome, now: now, walk: walk)
            return Reading(files: claude.merging(rollouts) { file, _ in file }, join: join)
        }.value
    }
}
