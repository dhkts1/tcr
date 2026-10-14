import XCTest

@testable import TcrBarCore

/// The evidence of codex an empty or Claude-only panel needs before it may mention Codex.
final class CodexPresenceTests: XCTestCase {

    /// The value the render harness always passes: no source at all, so nothing is present.
    func testAbsentIsNotPresent() {
        XCTAssertFalse(CodexPresence.absent.isPresent)
        XCTAssertEqual(CodexPresence(), .absent)
        XCTAssertFalse(CodexPresence.absent.fleetHoldsCodex)
        XCTAssertNil(CodexPresence.absent.codexExecutable)
        XCTAssertNil(CodexPresence.absent.codexHome)
        XCTAssertNil(CodexPresence.absent.storedExecutable)
    }

    /// Any one of the four sources is enough on its own; none needs another beside it.
    func testAnyOneSourceMakesItPresent() {
        XCTAssertTrue(CodexPresence(fleetHoldsCodex: true).isPresent)
        XCTAssertTrue(CodexPresence(codexExecutable: "/opt/homebrew/bin/codex").isPresent)
        XCTAssertTrue(CodexPresence(codexHome: "/Users/example/.codex").isPresent)
        XCTAssertTrue(CodexPresence(storedExecutable: "/Users/example/.local/bin/codex").isPresent)
    }

    /// A codex folder is not a codex login: only `<codexHome>/auth.json` is, so whether that file
    /// exists is a fact of its own. It is false unless a caller found the file.
    func testALoginFileIsAFactOfItsOwn() {
        XCTAssertFalse(CodexPresence.absent.hasLoginFile)
        let folder = CodexPresence(codexHome: "/Users/example/.codex")
        XCTAssertFalse(folder.hasLoginFile)

        let login = CodexPresence(codexHome: "/Users/example/.codex", hasLoginFile: true)
        XCTAssertTrue(login.hasLoginFile)
        XCTAssertNotEqual(login, folder)
        XCTAssertTrue(login.isPresent)
    }

    // MARK: - The empty fleet's one Codex line

    /// A login on disk is the strongest evidence, and the line names where it is, shortened
    /// against the home it is given, whatever else is found.
    func testTheEmptyFleetHintNamesTheLogin() {
        let login = CodexPresence(codexHome: "/Users/example/.codex", hasLoginFile: true)
        let line =
            "Found a codex login at ~/.codex: Add account… can sign in a Codex account of tcr's own, or import that one."
        XCTAssertEqual(login.emptyFleetHint(home: "/Users/example"), line)

        var withBinary = login
        withBinary.codexExecutable = "/opt/homebrew/bin/codex"
        XCTAssertEqual(withBinary.emptyFleetHint(home: "/Users/example"), line)
    }

    /// A codex executable, found or stored, with no codex home: the line says codex is here
    /// and claims no login. With no evidence at all there is no line, and the empty fleet is
    /// the one it always was.
    func testTheEmptyFleetHintForABinaryOnly() {
        let line = "codex is on this Mac: Add account… can sign in a Codex (ChatGPT) account too."
        XCTAssertEqual(
            CodexPresence(codexExecutable: "/opt/homebrew/bin/codex").emptyFleetHint(home: "/Users/example"),
            line)
        XCTAssertEqual(
            CodexPresence(storedExecutable: "/Users/example/.local/bin/codex")
                .emptyFleetHint(home: "/Users/example"),
            line)
        XCTAssertNil(CodexPresence.absent.emptyFleetHint(home: "/Users/example"))
    }

    /// A codex folder holds a login only when its auth.json exists, so a folder alone is
    /// worded as a folder, and an executable beside it says codex is here instead.
    func testAFolderWithoutALoginFileClaimsNoLogin() {
        let folder = CodexPresence(codexHome: "/Users/example/.codex")
        let folderLine = folder.emptyFleetHint(home: "/Users/example")
        XCTAssertEqual(
            folderLine,
            "Found a codex folder at ~/.codex: Add account… can sign in a Codex account of tcr's own.")

        var withBinary = folder
        withBinary.codexExecutable = "/opt/homebrew/bin/codex"
        let binaryLine = withBinary.emptyFleetHint(home: "/Users/example")
        XCTAssertEqual(
            binaryLine, "codex is on this Mac: Add account… can sign in a Codex (ChatGPT) account too.")

        for line in [folderLine, binaryLine] {
            XCTAssertFalse(line?.contains("codex login") ?? true, "\(line ?? "nil") claims a login")
        }
    }

    // MARK: - The live resolver

    /// The resolver asks about codex where each installer puts it, in a fixed order, and
    /// stops at the first one that runs: the path a person or a launcher named, then
    /// `PATH` and the folders tcr itself is searched in, then npm's, nvm's newest version
    /// first, volta's and asdf's. A Finder-launched app has none of them on its `PATH`.
    func testTheResolverSearchesInOrder() {
        let environment = [
            "TCR_CODEX_BIN": "/opt/example/codex", "PATH": "/custom/bin:/usr/bin",
            "NPM_CONFIG_PREFIX": "/Users/example/.npm-custom",
        ]
        let nvm = "/Users/example/.nvm/versions/node"
        let nothing = CodexProbeRecorder(entries: [nvm: ["v20.18.0", "v22.11.0"]])
        let found = CodexPresence.resolve(
            environment: environment, storedExecutable: nil, home: "/Users/example",
            probe: nothing.probe)
        XCTAssertEqual(
            nothing.executables,
            [
                "/opt/example/codex", "/custom/bin/codex", "/usr/bin/codex",
                "/Users/example/.local/bin/codex", "/Users/example/.cargo/bin/codex",
                "/opt/homebrew/bin/codex", "/usr/local/bin/codex", "/Users/example/.npm-custom/bin/codex",
                "\(nvm)/v22.11.0/bin/codex", "\(nvm)/v20.18.0/bin/codex", "/Users/example/.volta/bin/codex",
                "/Users/example/.asdf/shims/codex",
            ])
        XCTAssertNil(found.codexExecutable)

        let homebrew = CodexProbeRecorder(
            executables: ["/usr/local/bin/codex"], entries: [nvm: ["v20.18.0", "v22.11.0"]])
        let stopped = CodexPresence.resolve(
            environment: environment, storedExecutable: nil, home: "/Users/example",
            probe: homebrew.probe)
        XCTAssertEqual(stopped.codexExecutable, "/usr/local/bin/codex")
        XCTAssertEqual(homebrew.executables.last, "/usr/local/bin/codex", "asked on past the one that runs")
        XCTAssertEqual(homebrew.listed, [], "listed nvm's versions after codex was found")

        for (key, prefix) in [
            (nil, "/Users/example/.npm-global"), ("npm_config_prefix", "/Users/example/.npm-lower"),
        ] as [(String?, String)] {
            var npm = ["PATH": "/usr/bin"]
            if let key { npm[key] = prefix }
            let recorder = CodexProbeRecorder()
            _ = CodexPresence.resolve(
                environment: npm, storedExecutable: nil, home: "/Users/example", probe: recorder.probe)
            XCTAssertTrue(recorder.executables.contains("\(prefix)/bin/codex"), "\(recorder.executables)")
        }
    }

    /// The resolver never runs codex and never opens a file inside the codex home: it asks
    /// whether `auth.json` exists there, and nothing else. A login is a fact of its own,
    /// and the four questions a probe can ask are all of what it can do.
    func testTheResolverNeverRunsCodexOrReadsInsideCodexHome() {
        let home = "/Users/example"
        let codexB = "\(home)/.codex-b"
        let signedIn = CodexProbeRecorder(directories: [codexB], files: ["\(codexB)/auth.json"])
        let fromEnvironment = CodexPresence.resolve(
            environment: ["CODEX_HOME": codexB, "PATH": "/usr/bin"],
            storedExecutable: "/Users/example/bin/codex", home: home, probe: signedIn.probe)
        XCTAssertEqual(fromEnvironment.codexHome, codexB)
        XCTAssertTrue(fromEnvironment.hasLoginFile)
        XCTAssertEqual(fromEnvironment.storedExecutable, "/Users/example/bin/codex")
        XCTAssertFalse(fromEnvironment.fleetHoldsCodex, "the panel, not the resolver, reads the fleet")
        XCTAssertEqual(signedIn.existence, ["\(codexB)/auth.json"])
        XCTAssertEqual(
            signedIn.everything.filter { $0.hasPrefix(codexB + "/") }, ["\(codexB)/auth.json"],
            "the resolver asked about a path inside the codex home")
        XCTAssertEqual(signedIn.listed, ["\(home)/.nvm/versions/node"])

        let signedOut = CodexProbeRecorder(directories: [codexB])
        XCTAssertFalse(
            CodexPresence.resolve(
                environment: ["CODEX_HOME": codexB], storedExecutable: nil, home: home,
                probe: signedOut.probe
            ).hasLoginFile)

        let dotCodex = CodexProbeRecorder(directories: ["\(home)/.codex"])
        XCTAssertEqual(
            CodexPresence.resolve(
                environment: [:], storedExecutable: nil, home: home, probe: dotCodex.probe
            ).codexHome,
            "\(home)/.codex")

        let none = CodexProbeRecorder()
        let absent = CodexPresence.resolve(
            environment: [:], storedExecutable: nil, home: home, probe: none.probe)
        XCTAssertNil(absent.codexHome)
        XCTAssertFalse(absent.hasLoginFile)
        XCTAssertEqual(none.existence, [], "asked about a login with no codex home")

        XCTAssertEqual(
            Mirror(reflecting: CodexPresence.Probe.live).children.count, 4,
            "a probe asks four questions, none of which opens a file or runs a process")
    }

    /// The live check is existence only: a login file nobody may read still counts, as it
    /// does for the CLI's own check.
    func testTheLiveLoginCheckIsExistenceOnly() throws {
        let folder = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-codex-presence-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: folder) }
        let login = folder.appendingPathComponent("auth.json")
        try Data("{}".utf8).write(to: login)
        try FileManager.default.setAttributes([.posixPermissions: 0o000], ofItemAtPath: login.path)
        addTeardownBlock {
            try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: login.path)
        }

        XCTAssertTrue(CodexPresence.Probe.live.fileExists(login.path))
        XCTAssertFalse(CodexPresence.Probe.live.fileExists(folder.appendingPathComponent("none").path))
    }
}

/// A probe that answers from sets and records every question, each kind on its own list,
/// so a test can read the order the executables were asked in.
final class CodexProbeRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private let runnable: Set<String>
    private let folders: Set<String>
    private let present: Set<String>
    private let entries: [String: [String]]
    private var asked: [(kind: String, path: String)] = []

    init(
        executables: Set<String> = [], directories: Set<String> = [], files: Set<String> = [],
        entries: [String: [String]] = [:]
    ) {
        self.runnable = executables
        self.folders = directories
        self.present = files
        self.entries = entries
    }

    var probe: CodexPresence.Probe {
        CodexPresence.Probe(
            isExecutableFile: { self.note("x", $0) && self.runnable.contains($0) },
            isDirectory: { self.note("d", $0) && self.folders.contains($0) },
            directoryEntries: { self.note("l", $0) ? self.entries[$0] ?? [] : [] },
            fileExists: { self.note("f", $0) && self.present.contains($0) })
    }

    var executables: [String] { paths("x") }
    var listed: [String] { paths("l") }
    var existence: [String] { paths("f") }
    var everything: [String] { lock.withLock { asked.map(\.path) } }

    private func paths(_ kind: String) -> [String] {
        lock.withLock { asked.filter { $0.kind == kind }.map(\.path) }
    }

    /// Records the question, and answers `true` so it can lead an `&&`.
    private func note(_ kind: String, _ path: String) -> Bool {
        lock.withLock { asked.append((kind, path)) }
        return true
    }
}
