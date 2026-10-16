import Foundation

// MARK: - When

/// Whether this launch runs `tcr migrate --json`, the step that moves an
/// install's `teamclaude` config file and cache directory to the `tcr` names.
///
/// It runs at launch, before TcrBar starts its server, whenever an old name is
/// still on disk. Nothing is remembered between launches: a refused move (a
/// server already holding the port) or a failed one is tried again at the next
/// launch, and once both old names are gone the check finds nothing and the
/// CLI is never started. `tcr migrate` refuses beside a running server itself,
/// so trying costs nothing.
public enum NameMigrationGate {
    public enum Decision: Equatable, Sendable {
        case run
        case skip
    }

    /// The config file every install before the rename has, in `~/.config`.
    public static let legacyConfigFile = ConfigFileName.legacy
    /// The cache directory every install before the rename has, under the cache base.
    public static let legacyCacheDir = "teamclaude"

    /// - Parameters:
    ///   - buildNumber: `AppBuild.buildNumber`. `nil` (an unbundled `swift build`
    ///     binary, a test host) never runs: it has no bundled `tcr` of its own.
    ///   - oldNamesPresent: ``oldNamesPresent(home:environment:fileExists:)``.
    public static func decide(buildNumber: String?, oldNamesPresent: Bool) -> Decision {
        guard let buildNumber, !buildNumber.isEmpty else { return .skip }
        return oldNamesPresent ? .run : .skip
    }

    /// Whether `~/.config/teamclaude.json` or `<cache base>/teamclaude` exists.
    ///
    /// The cache base is `src/paths.rs`'s `cache_base` rule: `$XDG_CACHE_HOME`
    /// when set and non-empty, else `$HOME/.cache`. Only asks whether a path
    /// exists; never opens one, because the config holds live credentials.
    public static func oldNamesPresent(
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        environment: [String: String] = ProcessInfo.processInfo.environment,
        fileExists: (String) -> Bool = { FileManager.default.fileExists(atPath: $0) }
    ) -> Bool {
        let config = home.appendingPathComponent(".config").appendingPathComponent(legacyConfigFile)
        let cacheBase: URL
        if let xdg = environment["XDG_CACHE_HOME"], !xdg.isEmpty {
            cacheBase = URL(fileURLWithPath: xdg)
        } else {
            cacheBase = home.appendingPathComponent(".cache")
        }
        let cache = cacheBase.appendingPathComponent(legacyCacheDir)
        return fileExists(config.path) || fileExists(cache.path)
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
