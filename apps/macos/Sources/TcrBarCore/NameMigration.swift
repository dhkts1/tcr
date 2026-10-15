import Foundation

// MARK: - When

/// Whether this launch runs `tcr migrate --json`, the step that moves an
/// install's `teamclaude` config file and cache directory to the `tcr` names.
///
/// It runs on the first launch after the bundle's `CFBundleVersion` changed: a
/// Sparkle update or a manual install. That moment is safe because the update
/// quit the old app, and quitting stops the server it supervised, so nothing
/// is writing the files. `tcr migrate` checks again and refuses if a server is
/// still running; a refusal costs nothing, the next update tries again.
///
/// No stored version means RUN, unlike ``LaunchVersionMarker``'s "a fresh
/// install is not an update". The key is new in the build that brings the
/// migration, so every existing install upgrading to it has nothing stored,
/// and that launch is exactly the one that has to move the files. On a truly
/// fresh install the run finds nothing to move.
public enum NameMigrationGate {
    public enum Decision: Equatable, Sendable {
        case run
        case skip
    }

    /// - Parameters:
    ///   - lastLaunched: the `CFBundleVersion` the previous launch recorded.
    ///   - current: `AppBuild.buildNumber`. `nil` (an unbundled `swift build`
    ///     binary, a test host) never runs: no bundle, no update.
    public static func decide(lastLaunched: String?, current: String?) -> Decision {
        guard let current, !current.isEmpty else { return .skip }
        guard let lastLaunched, !lastLaunched.isEmpty else { return .run }
        return lastLaunched == current ? .skip : .run
    }
}

/// Remembers the `CFBundleVersion` that last launched, for ``NameMigrationGate``.
///
/// Recorded on every launch, whatever the migration did: a refused or failed
/// move must not retry on every launch until it works, it retries at the next
/// update. The key string is load-bearing for the same reason as
/// ``LaunchVersionMarker/lastLaunchedVersionKey``; `NameMigrationTests` pins it.
@MainActor
public final class NameMigrationMarker {
    /// The `UserDefaults` key. Do not change it.
    public nonisolated static let lastLaunchedBuildKey = "lastLaunchedBundleVersion"

    private let defaults: UserDefaults?

    /// `nil` defaults remember nothing and never run, for render and probe
    /// harnesses that must not move files or write preferences.
    public init(defaults: UserDefaults? = .standard) {
        self.defaults = defaults
    }

    /// Record `current` as launched and say whether this launch migrates.
    public func noteLaunch(current: String?) -> Bool {
        guard let defaults else { return false }
        let previous = defaults.string(forKey: Self.lastLaunchedBuildKey)
        if let current, !current.isEmpty {
            defaults.set(current, forKey: Self.lastLaunchedBuildKey)
        }
        return NameMigrationGate.decide(lastLaunched: previous, current: current) == .run
    }
}

// MARK: - What it said

/// `tcr migrate --json`'s document, decoded. The CLI always prints the same
/// four keys (`src/migrate.rs`, `Run::to_json`), so a missing key is a
/// document from something else, not an outcome.
public struct NameMigrationReport: Equatable, Sendable, Decodable {
    public enum Outcome: String, Equatable, Sendable, Decodable {
        case moved
        case nothingToMove = "nothing_to_move"
        case failed
        case refused
    }

    public struct Refusal: Equatable, Sendable, Decodable {
        public let port: Int
        public let pid: Int
        public let message: String
    }

    /// One unit (`config` or `cache`) or one log file. Which paths are set
    /// depends on `result`; the CLI sends exactly the ones that apply.
    public struct Entry: Equatable, Sendable, Decodable {
        public let unit: String?
        public let result: String
        public let from: String?
        public let to: String?
        public let old: String?
        public let new: String?
        public let path: String?
        public let error: String?
    }

    public let outcome: Outcome
    public let refusal: Refusal?
    public let units: [Entry]
    public let logs: [Entry]
}

/// What one `tcr migrate --json` run came to.
public enum NameMigrationResult: Equatable, Sendable {
    /// The CLI answered with its document.
    case report(NameMigrationReport)
    /// It did not: an older `tcr` without the verb (clap exits 2), a crash, or
    /// output that is not the document. The detail is what it said instead.
    case unreadable(exitCode: Int32, detail: String)

    /// Decode a finished run. Exit codes are not trusted on their own: 0, 1
    /// and 3 all carry the document, and anything without it is unreadable.
    public static func parse(exitCode: Int32, stdout: Data, stderr: String) -> NameMigrationResult {
        if let report = try? JSONDecoder().decode(NameMigrationReport.self, from: stdout) {
            return .report(report)
        }
        let out = String(data: stdout, encoding: .utf8) ?? ""
        let detail =
            [stderr, out]
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .first { !$0.isEmpty } ?? "no output"
        return .unreadable(exitCode: exitCode, detail: detail)
    }

    /// The lines TcrBar logs: a summary first, then one line per thing that
    /// moved, was left, or failed, each with its paths.
    public var logLines: [String] {
        switch self {
        case .unreadable(let code, let detail):
            return ["tcr migrate gave no report (exit \(code)): \(detail)"]
        case .report(let report):
            var lines: [String]
            switch report.outcome {
            case .moved: lines = ["tcr migrate moved the old-name files"]
            case .nothingToMove: lines = ["tcr migrate: nothing to move"]
            case .failed: lines = ["tcr migrate could not move everything; the old names are still used"]
            case .refused:
                lines = ["tcr migrate refused: \(report.refusal?.message ?? "a tcr server is running")"]
            }
            for entry in report.units + report.logs {
                let what = entry.unit ?? "log"
                switch entry.result {
                case "moved", "renamed":
                    lines.append("\(what): \(entry.from ?? "?") -> \(entry.to ?? "?")")
                case "both_exist":
                    lines.append(
                        "\(what): both \(entry.old ?? "?") and \(entry.new ?? "?") exist; the new one is used"
                    )
                case "target_exists":
                    lines.append("\(what): left \(entry.from ?? "?"), \(entry.to ?? "?") already exists")
                case "failed":
                    lines.append(
                        "\(what): could not move \(entry.from ?? "?") -> \(entry.to ?? "?"): \(entry.error ?? "?")"
                    )
                default:
                    continue
                }
            }
            return lines
        }
    }
}
