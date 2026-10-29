import Darwin
import Foundation

/// Repoints a `tcr` on `PATH` that is older than the one inside this bundle.
///
/// # Why
///
/// The CLI and the app ship through two channels: Sparkle updates
/// `TcrBar.app`, and nothing updates the `tcr` a shell finds on `PATH`. The
/// shell's `claude` function runs bare `tcr run`, so an old CLI on `PATH`
/// launches Claude Code with the CA file IT knows about while the server inside
/// the updated app signs with another one. Every request then fails with
/// `UNABLE_TO_VERIFY_LEAF_SIGNATURE` (a 0.2.28 CLI beside a 1.2.18 app, which
/// handed out `teamclaude-ca.pem`; the CA loading for that file was removed in
/// 1.2.12).
///
/// So at every launch, and an update always relaunches, the app makes the
/// arrangement `apps/macos/README.md` § "Optionally pointing the CLI at the
/// bundle" describes, but only when it is plainly safe:
///
/// - the `PATH` copy is a regular file, not a symlink someone chose;
/// - both it and the bundled copy answer `--version`, and the `PATH` copy is
///   strictly older by major.minor.patch, so a dev build is never replaced;
/// - the bundle is an installed app under an `Applications` folder, never a
///   build folder or a translocated copy that can disappear under the link;
/// - the folder is writable by this user.
///
/// # How
///
/// A symlink to the bundled `tcr` is made at a temporary name in the SAME
/// folder and `rename(2)`d over the `PATH` file. Never `cp` and never unlink
/// then create: `scripts/install-cli.sh` explains the 25 processes a `cp` onto
/// a live executable killed, and an unlink leaves a window with no `tcr` at all.
/// rename(2) gives the path a new inode atomically, so a `tcr run` that is
/// already running keeps its own bytes.
///
/// ``decide(_:)`` is the whole policy and touches nothing. ``inspect(pathTcr:bundledTcr:isInstalledBundle:versionOf:fileManager:)``
/// gathers its inputs and ``run(pathTcr:bundledTcr:isInstalledBundle:versionOf:fileManager:)``
/// acts on the verdict.
public enum PathCliRepair {

    /// A `tcr --version` number, compared numerically part by part.
    public struct Version: Comparable, Equatable, Sendable, CustomStringConvertible {
        public let major: Int
        public let minor: Int
        public let patch: Int

        public init(major: Int, minor: Int, patch: Int) {
            self.major = major
            self.minor = minor
            self.patch = patch
        }

        /// `1.2.20` → 1, 2, 20. Anything after the patch digits (`1.2.21-dev`) is
        /// ignored, so a pre-release reads as its release and is never "older"
        /// than it. Fewer than three numeric parts is `nil`: unreadable, not zero.
        public init?(_ text: String) {
            let parts = text.split(separator: ".", maxSplits: 2, omittingEmptySubsequences: false)
            guard parts.count == 3 else { return nil }
            let numbers = parts.map { part in Int(part.prefix(while: { $0.isASCII && $0.isNumber })) }
            guard let major = numbers[0], let minor = numbers[1], let patch = numbers[2],
                parts[0].allSatisfy({ $0.isASCII && $0.isNumber }),
                parts[1].allSatisfy({ $0.isASCII && $0.isNumber })
            else { return nil }
            self.init(major: major, minor: minor, patch: patch)
        }

        public static func < (lhs: Version, rhs: Version) -> Bool {
            (lhs.major, lhs.minor, lhs.patch) < (rhs.major, rhs.minor, rhs.patch)
        }

        public var description: String { "\(major).\(minor).\(patch)" }
    }

    /// What the `PATH` entry itself is, read with `lstat`, so a symlink is seen as one.
    public enum EntryKind: Equatable, Sendable {
        case regularFile
        /// `resolved` is the fully resolved target; `intoBundle` compares it with the
        /// bundled `tcr`, also fully resolved.
        case symlink(resolved: String, intoBundle: Bool)
        /// A folder, a socket, or something `lstat` could not read.
        case other
    }

    /// Everything ``decide(_:)`` needs, gathered once.
    public struct Facts: Equatable, Sendable {
        public let pathTcr: String?
        public let bundledTcr: String?
        public let entry: EntryKind
        public let bundleIsInstalled: Bool
        /// `nil` when `--version` failed or printed nothing readable.
        public let pathVersion: Version?
        /// `nil` when the bundled binary did not run `--version` with exit 0, which
        /// is the proof the README asks for before anything points at it.
        public let bundledVersion: Version?
        public let directoryWritable: Bool

        public init(
            pathTcr: String?, bundledTcr: String?, entry: EntryKind, bundleIsInstalled: Bool,
            pathVersion: Version?, bundledVersion: Version?, directoryWritable: Bool
        ) {
            self.pathTcr = pathTcr
            self.bundledTcr = bundledTcr
            self.entry = entry
            self.bundleIsInstalled = bundleIsInstalled
            self.pathVersion = pathVersion
            self.bundledVersion = bundledVersion
            self.directoryWritable = directoryWritable
        }
    }

    /// Every way a launch can end, one log line each.
    public enum Outcome: Equatable, Sendable {
        case noPathTcr
        case noBundledTcr(pathTcr: String)
        case bundleNotInstalled(pathTcr: String, bundledTcr: String)
        case alreadyBundled(pathTcr: String)
        case symlinkElsewhere(pathTcr: String, target: String)
        case notARegularFile(pathTcr: String)
        case bundledUnrunnable(pathTcr: String, bundledTcr: String)
        case pathVersionUnreadable(pathTcr: String)
        case notOlder(pathTcr: String, pathVersion: Version, bundledVersion: Version)
        case directoryNotWritable(
            pathTcr: String, bundledTcr: String, pathVersion: Version, bundledVersion: Version)
        /// ``decide(_:)``'s only verdict that acts. ``run(pathTcr:bundledTcr:isInstalledBundle:versionOf:fileManager:)``
        /// turns it into ``repointed(pathTcr:bundledTcr:from:to:)`` or ``failed(pathTcr:reason:)``.
        case shouldRepoint(pathTcr: String, bundledTcr: String, pathVersion: Version, bundledVersion: Version)
        case repointed(pathTcr: String, bundledTcr: String, from: Version, to: Version)
        case failed(pathTcr: String, reason: String)

        /// The one line logged for this outcome.
        public var logLine: String {
            switch self {
            case .noPathTcr:
                return "TcrBar: PATH tcr: none on PATH, nothing to repoint"
            case .noBundledTcr(let path):
                return "TcrBar: PATH tcr: \(path) left alone, this app has no bundled tcr"
            case .bundleNotInstalled(let path, let bundled):
                return "TcrBar: PATH tcr: \(path) left alone, \(bundled) is not in an installed app"
            case .alreadyBundled(let path):
                return "TcrBar: PATH tcr: \(path) already resolves into this app"
            case .symlinkElsewhere(let path, let target):
                return "TcrBar: PATH tcr: \(path) is a symlink to \(target), left alone"
            case .notARegularFile(let path):
                return "TcrBar: PATH tcr: \(path) is not a regular file, left alone"
            case .bundledUnrunnable(let path, let bundled):
                return "TcrBar: PATH tcr: \(path) left alone, \(bundled) --version did not run"
            case .pathVersionUnreadable(let path):
                return "TcrBar: PATH tcr: \(path) left alone, its --version is unreadable"
            case .notOlder(let path, let pathVersion, let bundledVersion):
                return
                    "TcrBar: PATH tcr: \(path) is \(pathVersion), not older than the bundled \(bundledVersion), left alone"
            case .directoryNotWritable(let path, let bundled, let pathVersion, let bundledVersion):
                return
                    "TcrBar: PATH tcr: \(path) is \(pathVersion), older than the bundled \(bundledVersion), and its folder is not writable; to fix by hand: ln -sf '\(bundled)' '\(path)'"
            case .shouldRepoint(let path, let bundled, let pathVersion, let bundledVersion):
                return "TcrBar: PATH tcr: \(path) \(pathVersion) should point at \(bundled) \(bundledVersion)"
            case .repointed(let path, let bundled, let from, let to):
                return "TcrBar: PATH tcr: \(path) was \(from), now a symlink to \(bundled) (\(to))"
            case .failed(let path, let reason):
                return "TcrBar: PATH tcr: repointing \(path) failed: \(reason)"
            }
        }
    }

    /// The policy. Pure: same facts, same verdict, nothing touched.
    public static func decide(_ facts: Facts) -> Outcome {
        guard let path = facts.pathTcr else { return .noPathTcr }
        switch facts.entry {
        case .symlink(_, intoBundle: true):
            return .alreadyBundled(pathTcr: path)
        case .symlink(let resolved, intoBundle: false):
            return .symlinkElsewhere(pathTcr: path, target: resolved)
        case .other:
            return .notARegularFile(pathTcr: path)
        case .regularFile:
            break
        }
        guard let bundled = facts.bundledTcr else { return .noBundledTcr(pathTcr: path) }
        guard facts.bundleIsInstalled else {
            return .bundleNotInstalled(pathTcr: path, bundledTcr: bundled)
        }
        guard let bundledVersion = facts.bundledVersion else {
            return .bundledUnrunnable(pathTcr: path, bundledTcr: bundled)
        }
        guard let pathVersion = facts.pathVersion else { return .pathVersionUnreadable(pathTcr: path) }
        guard pathVersion < bundledVersion else {
            return .notOlder(pathTcr: path, pathVersion: pathVersion, bundledVersion: bundledVersion)
        }
        guard facts.directoryWritable else {
            return .directoryNotWritable(
                pathTcr: path, bundledTcr: bundled, pathVersion: pathVersion, bundledVersion: bundledVersion)
        }
        return .shouldRepoint(
            pathTcr: path, bundledTcr: bundled, pathVersion: pathVersion, bundledVersion: bundledVersion)
    }

    /// Whether `bundledTcr` lives inside an installed app: `<…>/Applications/<name>.app/Contents/MacOS/tcr`.
    ///
    /// A symlink into a build folder or an App Translocation mount points at a
    /// file that goes away, and every shell `tcr` breaks with it.
    public static func isInstalledBundle(_ bundledTcr: URL) -> Bool {
        let parts = bundledTcr.standardizedFileURL.pathComponents
        guard parts.count >= 5 else { return false }
        let tail = Array(parts.suffix(4))
        return tail[0].hasSuffix(".app") && tail[1] == "Contents" && tail[2] == "MacOS"
            && parts[parts.count - 5] == "Applications"
    }

    /// `tcr --version` through ``TcrTool/run(executable:arguments:stdin:)``: the version
    /// when it exits 0 and prints one this app can read, else `nil`.
    ///
    /// Blocking: it runs a process.
    public static func versionByRunning(_ executable: URL) -> Version? {
        guard let output = try? TcrTool.run(executable: executable, arguments: ["--version"]),
            output.exitCode == 0,
            let text = TcrTool.version(fromVersionOutput: String(decoding: output.stdout, as: UTF8.self))
        else { return nil }
        return Version(text)
    }

    /// `realpath(3)`, or `nil` when some part of the chain does not exist.
    static func realPath(_ path: String) -> String? {
        guard let resolved = Darwin.realpath(path, nil) else { return nil }
        defer { free(resolved) }
        return String(cString: resolved)
    }

    /// Read the facts. Blocking: it may run both binaries' `--version`.
    ///
    /// The versions are only asked for when the entry is a regular file, so a
    /// symlink that is already right costs no process at all.
    public static func inspect(
        pathTcr: URL?,
        bundledTcr: URL?,
        isInstalledBundle: (URL) -> Bool = PathCliRepair.isInstalledBundle,
        versionOf: (URL) -> Version? = PathCliRepair.versionByRunning,
        fileManager: FileManager = .default
    ) -> Facts {
        guard let pathTcr else {
            return Facts(
                pathTcr: nil, bundledTcr: bundledTcr?.path, entry: .other, bundleIsInstalled: false,
                pathVersion: nil, bundledVersion: nil, directoryWritable: false)
        }
        let bundled = bundledTcr.flatMap { fileManager.isExecutableFile(atPath: $0.path) ? $0 : nil }
        var info = stat()
        let entry: EntryKind
        if lstat(pathTcr.path, &info) != 0 {
            entry = .other
        } else if info.st_mode & S_IFMT == S_IFLNK {
            let resolved = realPath(pathTcr.path)
            let bundledResolved = bundledTcr.flatMap { realPath($0.path) }
            entry = .symlink(
                resolved: resolved ?? "(dangling)",
                intoBundle: resolved != nil && resolved == bundledResolved)
        } else if info.st_mode & S_IFMT == S_IFREG {
            entry = .regularFile
        } else {
            entry = .other
        }
        let readVersions = entry == .regularFile
        let directory = pathTcr.deletingLastPathComponent().path
        return Facts(
            pathTcr: pathTcr.path,
            bundledTcr: bundled?.path,
            entry: entry,
            bundleIsInstalled: bundled.map(isInstalledBundle) ?? false,
            pathVersion: readVersions ? versionOf(pathTcr) : nil,
            bundledVersion: readVersions ? bundled.flatMap(versionOf) : nil,
            directoryWritable: access(directory, W_OK) == 0)
    }

    /// Inspect, decide, and act on a ``Outcome/shouldRepoint(pathTcr:bundledTcr:pathVersion:bundledVersion:)``.
    ///
    /// Blocking: run it off the main thread.
    public static func run(
        pathTcr: URL?,
        bundledTcr: URL?,
        isInstalledBundle: (URL) -> Bool = PathCliRepair.isInstalledBundle,
        versionOf: (URL) -> Version? = PathCliRepair.versionByRunning,
        fileManager: FileManager = .default
    ) -> Outcome {
        let facts = inspect(
            pathTcr: pathTcr, bundledTcr: bundledTcr, isInstalledBundle: isInstalledBundle,
            versionOf: versionOf, fileManager: fileManager)
        let verdict = decide(facts)
        guard case .shouldRepoint(let path, let bundled, let from, let to) = verdict else { return verdict }
        if let reason = replaceWithSymlink(at: path, pointingAt: bundled) {
            return .failed(pathTcr: path, reason: reason)
        }
        return .repointed(pathTcr: path, bundledTcr: bundled, from: from, to: to)
    }

    /// A symlink to `target` at a temporary name beside `path`, then `rename(2)` over
    /// `path`. `nil` on success, else what failed and its `errno` text.
    ///
    /// The temporary link is removed when the rename fails, so a failure leaves the
    /// folder as it was.
    static func replaceWithSymlink(at path: String, pointingAt target: String) -> String? {
        let directory = (path as NSString).deletingLastPathComponent
        let temporary = (directory as NSString).appendingPathComponent(
            ".tcr.tcrbar-\(getpid())-\(UUID().uuidString)")
        guard symlink(target, temporary) == 0 else {
            return "symlink \(temporary): \(String(cString: strerror(errno)))"
        }
        guard rename(temporary, path) == 0 else {
            let reason = "rename \(temporary) -> \(path): \(String(cString: strerror(errno)))"
            unlink(temporary)
            return reason
        }
        return nil
    }
}
