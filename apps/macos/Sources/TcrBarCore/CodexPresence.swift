import Foundation

/// The evidence of codex that an empty or Claude-only panel needs before it mentions Codex.
///
/// Any one of the four sources is enough, and none of them reads a credential: a Codex row
/// in the fleet, a codex executable that resolves, a codex home folder that exists, or the
/// path a person stored by hand.
///
/// It is a value rather than a lookup, so the render harness can always pass ``absent``: a
/// render must never see this Mac's own `~/.codex`.
///
/// ``hasLoginFile`` is not a fifth source. It says whether the codex home holds a login, which
/// the folder alone does not, and it adds nothing to ``isPresent``: the file implies the folder.
public struct CodexPresence: Equatable, Sendable {
    /// Any Codex row in the fleet, in any state.
    public var fleetHoldsCodex: Bool
    /// The codex executable this Mac resolves to, or `nil` when none resolves.
    public var codexExecutable: String?
    /// `$CODEX_HOME`, or `~/.codex` when that folder exists.
    public var codexHome: String?
    /// The stored `CodexExecutablePath` preference.
    public var storedExecutable: String?
    /// `<codexHome>/auth.json` exists; existence only, never opened.
    public var hasLoginFile: Bool

    public init(
        fleetHoldsCodex: Bool = false, codexExecutable: String? = nil,
        codexHome: String? = nil, storedExecutable: String? = nil, hasLoginFile: Bool = false
    ) {
        self.fleetHoldsCodex = fleetHoldsCodex
        self.codexExecutable = codexExecutable
        self.codexHome = codexHome
        self.storedExecutable = storedExecutable
        self.hasLoginFile = hasLoginFile
    }

    /// Whether any one of the four sources says codex is on this Mac.
    public var isPresent: Bool {
        fleetHoldsCodex || codexExecutable != nil || codexHome != nil || storedExecutable != nil
    }

    /// The one line an empty fleet's banner draws under its detail, or `nil` for none.
    ///
    /// The hint is drawn only on evidence of codex; with none, the empty fleet is the one it
    /// always was. The strongest evidence wins: a login file, then an executable, then a codex
    /// folder. Only a login file is worded as a login: a folder or a binary is not one, and the
    /// line for each claims no more than was found.
    ///
    /// `home` is a parameter, so a render names `/Users/example` and never the rendering Mac's
    /// home. A login file with no codex home to name falls through to the next line.
    public func emptyFleetHint(home: String) -> String? {
        if hasLoginFile, let codexHome {
            return
                "Found a codex login at \(HomePath.short(codexHome, home: home)): Add account… can sign in a Codex account of tcr's own, or import that one."
        }
        if codexExecutable != nil || storedExecutable != nil {
            return "codex is on this Mac: Add account… can sign in a Codex (ChatGPT) account too."
        }
        if let codexHome {
            return
                "Found a codex folder at \(HomePath.short(codexHome, home: home)): Add account… can sign in a Codex account of tcr's own."
        }
        return nil
    }

    /// No source at all. What every render passes.
    public static let absent = CodexPresence()

    // MARK: - The live resolver

    /// The four questions the resolver may ask about this Mac. None of them opens a file or
    /// runs a process: the resolver learns where codex is, never what it holds.
    public struct Probe: Sendable {
        public var isExecutableFile: @Sendable (String) -> Bool
        public var isDirectory: @Sendable (String) -> Bool
        /// A folder's entry names, or none when it cannot be listed.
        public var directoryEntries: @Sendable (String) -> [String]
        /// Whether a path exists. It never opens the file, so a login nobody may read
        /// still counts.
        public var fileExists: @Sendable (String) -> Bool

        public init(
            isExecutableFile: @escaping @Sendable (String) -> Bool,
            isDirectory: @escaping @Sendable (String) -> Bool,
            directoryEntries: @escaping @Sendable (String) -> [String],
            fileExists: @escaping @Sendable (String) -> Bool
        ) {
            self.isExecutableFile = isExecutableFile
            self.isDirectory = isDirectory
            self.directoryEntries = directoryEntries
            self.fileExists = fileExists
        }

        /// This Mac's file system.
        public static let live = Probe(
            isExecutableFile: { CodexExecutableChoice.isAcceptable(path: $0) },
            isDirectory: { path in
                var isDirectory: ObjCBool = false
                return FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory)
                    && isDirectory.boolValue
            },
            directoryEntries: { (try? FileManager.default.contentsOfDirectory(atPath: $0)) ?? [] },
            fileExists: { FileManager.default.fileExists(atPath: $0) })
    }

    /// The evidence of codex on this Mac, found by asking where it lives and never by
    /// running it.
    ///
    /// The executable is the first codex that runs, asked in this order, each path once:
    /// 1. `TCR_CODEX_BIN`;
    /// 2. `PATH`, then the folders tcr itself is looked for in
    ///    (``TcrTool/searchDirectories(environment:home:)``);
    /// 3. npm's global `bin`: `$NPM_CONFIG_PREFIX` (or `npm_config_prefix`), else npm's
    ///    documented user prefix `~/.npm-global`. No npm file is read to find it, so a
    ///    `~/.npmrc` that holds a registry token is never opened;
    /// 4. `~/.nvm/versions/node/*/bin`, the newest version first;
    /// 5. `~/.volta/bin`;
    /// 6. `~/.asdf/shims`.
    ///
    /// A Finder-launched app inherits none of the shell's `PATH`, which is why the installers'
    /// own folders are asked by name. nvm's folder is listed only when nothing before it runs.
    ///
    /// The codex home is `$CODEX_HOME` when it is a folder, else `~/.codex` when that is one.
    /// Inside it the resolver asks one question, whether `auth.json` exists, and only once the
    /// home is known: it checks existence only, never opens a file inside the codex home, and
    /// never runs codex. Blocking, though cheap; the panel runs it off the main thread.
    ///
    /// ``fleetHoldsCodex`` is left `false`: the panel sets it from the fleet it has loaded.
    public static func resolve(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        storedExecutable: String? = UserDefaults.standard.string(forKey: TcrTool.codexOverrideDefaultsKey),
        home: String = FileManager.default.homeDirectoryForCurrentUser.path,
        probe: Probe = .live
    ) -> CodexPresence {
        let home = trimmingTrailingSlashes(home)
        var asked = Set<String>()
        func firstRunnable(_ candidates: [String]) -> String? {
            candidates.first {
                asked.insert($0).inserted && probe.isExecutableFile($0) && !probe.isDirectory($0)
            }
        }

        let homeURL = URL(fileURLWithPath: home, isDirectory: true)
        let npmPrefix =
            [environment["NPM_CONFIG_PREFIX"], environment["npm_config_prefix"]]
            .compactMap { $0 }.first { !$0.isEmpty }.map(trimmingTrailingSlashes)
            ?? "\(home)/.npm-global"
        let nvm = "\(home)/.nvm/versions/node"
        let steps: [() -> [String]] = [
            { [environment["TCR_CODEX_BIN"]].compactMap { $0 }.filter { !$0.isEmpty } },
            {
                TcrTool.searchDirectories(environment: environment, home: homeURL)
                    .map { $0.appendingPathComponent("codex").path }
            },
            { ["\(npmPrefix)/bin/codex"] },
            {
                probe.directoryEntries(nvm)
                    .sorted { $0.localizedStandardCompare($1) == .orderedDescending }
                    .map { "\(nvm)/\($0)/bin/codex" }
            },
            { ["\(home)/.volta/bin/codex"] },
            { ["\(home)/.asdf/shims/codex"] },
        ]
        var executable: String?
        for step in steps {
            executable = firstRunnable(step())
            if executable != nil { break }
        }

        var codexHome: String?
        if let named = environment["CODEX_HOME"].map(trimmingTrailingSlashes), !named.isEmpty,
            probe.isDirectory(named)
        {
            codexHome = named
        } else if probe.isDirectory("\(home)/.codex") {
            codexHome = "\(home)/.codex"
        }
        let hasLoginFile = codexHome.map { probe.fileExists("\($0)/auth.json") } ?? false

        return CodexPresence(
            fleetHoldsCodex: false, codexExecutable: executable, codexHome: codexHome,
            storedExecutable: storedExecutable, hasLoginFile: hasLoginFile)
    }

    private static func trimmingTrailingSlashes(_ path: String) -> String {
        var path = path
        while path.count > 1, path.hasSuffix("/") { path.removeLast() }
        return path
    }
}

/// This Mac's codex evidence for the panel, resolved off the main thread each time the
/// panel opens, and the `tcr` inside TcrBar when this Mac has no other.
///
/// The render harness never builds one: a render must not see this Mac's codex, so it
/// passes ``CodexPresence/absent`` instead.
@MainActor
public final class CodexPresenceController: ObservableObject {
    @Published public private(set) var presence: CodexPresence = .absent
    /// Set only when codex is in evidence or the fleet holds a Codex account: it words how
    /// to start codex through the pool, which a Mac without codex does not need.
    @Published public private(set) var bundledTcrWithoutCLI: String?

    public init() {}

    /// Resolve again, off the main thread, and publish what changed.
    public func refresh(fleetHoldsCodex: Bool) async {
        let (resolved, bundled) = await Task.detached(priority: .utility) {
            (CodexPresence.resolve(), TcrTool.bundledTcrWithoutCLI())
        }.value
        if presence != resolved { presence = resolved }
        let tail = resolved.isPresent || fleetHoldsCodex ? bundled : nil
        if bundledTcrWithoutCLI != tail { bundledTcrWithoutCLI = tail }
    }
}
