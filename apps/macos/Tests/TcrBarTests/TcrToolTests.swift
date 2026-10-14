import XCTest

@testable import TcrBarCore

/// What ``TcrTool/run(executable:arguments:stdin:)`` costs the thread that
/// calls it, against a REAL subprocess.
///
/// Real, because the thing under test is how the wait ends. A stub would
/// report the stub's own wait. `/bin/sh -c 'exit 3'` starts, exits with a code
/// nobody else uses, and touches nothing: not the proxy, not a file, not a
/// config.
///
/// The guard is the wait, not the child. A wait that runs the calling thread's
/// run loop returns on the next tick rather than when the child exits, which
/// on the main thread rounds every read up to about 62.5 milliseconds however
/// cheap the child was. Every panel read that happens on the main thread pays
/// that, so the cost is charged to the person waiting for the panel to draw.
final class TcrToolWaitTests: XCTestCase {

    /// A child that exits at once costs the main thread what the child costs.
    ///
    /// **The test asserts it is on the main thread first.** Off the main thread
    /// there is no run loop to spin and the old wait was already fast, so a
    /// timing assertion that ran anywhere else would pass on the broken code
    /// and guard nothing.
    ///
    /// Five runs and the MEDIAN, not one run: the run loop's step is paid on
    /// every single call, so it moves the median, while a machine that was busy
    /// for one moment moves only one sample. The ceiling is `waitCeilingMs`:
    /// above what the spawn itself costs (single digits here; a GitHub macOS
    /// runner read a median of 30.2 on 2026-10-06, run 37460997527, with the
    /// 30 ms ceiling this used to have) and below the step being guarded against
    /// (about 62.5, and measured at 64.8), so a slow shared runner passes and the
    /// run-loop tick still fails.
    func testAChildThatExitsAtOnceDoesNotCostTheMainThreadARunLoopTick() throws {
        XCTAssertTrue(
            Thread.isMainThread,
            "this test is only meaningful on the main thread, where the run loop is spun")

        let shell = URL(fileURLWithPath: "/bin/sh")
        let arguments = ["-c", "exit 3"]

        // One run before the timed ones. The first spawn in a process pays for
        // whatever the runtime sets up once, and that cost belongs to neither
        // the child nor the wait.
        _ = try TcrTool.run(executable: shell, arguments: arguments)

        var samples: [Double] = []
        for _ in 0..<5 {
            let started = DispatchTime.now().uptimeNanoseconds
            let output = try TcrTool.run(executable: shell, arguments: arguments)
            let took = Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000
            samples.append(took)

            // The shape of `Output` is a contract with every caller, so the
            // exit code and both streams are asserted on each run rather than
            // once: a faster wait that returned before the child was reaped
            // would show up here as a zero exit code.
            XCTAssertEqual(output.exitCode, 3)
            XCTAssertEqual(output.stdout, Data())
            XCTAssertEqual(output.stderr, "")
        }

        let median = samples.sorted()[samples.count / 2]
        XCTAssertLessThan(
            median, Self.waitCeilingMs,
            "median of \(samples.map { String(format: "%.1f", $0) }) ms on the main thread")
    }

    /// The most a spawn-and-wait may cost at the median: well under the 62.5 ms
    /// run-loop step this suite exists to catch, with room for a shared CI runner
    /// (see the doc on the main-thread test).
    static let waitCeilingMs = 45.0

    /// Off the main thread the same child costs the same, which is the control:
    /// it says the number above is about the wait and not about this machine
    /// having a slow `fork`.
    func testTheSameChildCostsTheSameOffTheMainThread() async throws {
        let samples = await Task.detached(priority: .userInitiated) { () -> [Double] in
            let shell = URL(fileURLWithPath: "/bin/sh")
            let arguments = ["-c", "exit 3"]
            _ = try? TcrTool.run(executable: shell, arguments: arguments)
            var out: [Double] = []
            for _ in 0..<5 {
                let started = DispatchTime.now().uptimeNanoseconds
                _ = try? TcrTool.run(executable: shell, arguments: arguments)
                out.append(Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000)
            }
            return out
        }.value

        let median = samples.sorted()[samples.count / 2]
        XCTAssertLessThan(
            median, Self.waitCeilingMs,
            "median of \(samples.map { String(format: "%.1f", $0) }) ms off the main thread")
    }
}

/// Which `tcr` a shell on this Mac would run, apart from the one inside TcrBar, and
/// whether that one predates Codex accounts; and which codex a person may pick.
///
/// Every directory is made fresh and passed in, so no answer depends on what this Mac
/// has in `/usr/local/bin` or `/opt/homebrew/bin`. The `tcr` files are empty scripts that
/// nothing runs.
final class TcrToolCodexTests: XCTestCase {

    /// The codex a person picks is stored beside the `tcr` override, under its own key.
    func testTheCodexExecutableKeySitsBesideTheTcrKey() {
        XCTAssertEqual(TcrTool.codexOverrideDefaultsKey, "CodexExecutablePath")
        XCTAssertEqual(TcrTool.overrideDefaultsKey, "TcrExecutablePath")
    }

    /// The copy inside TcrBar is never the one a shell runs, even when a search
    /// directory names the bundle's own folder.
    func testThePathTcrIsFoundApartFromTheBundledOne() throws {
        let layout = try makeLayout()
        XCTAssertEqual(
            TcrTool.pathExecutable(
                searchDirectories: [layout.bundle, layout.pathDirectory], bundle: layout.bundle,
                fileManager: .default)?.path,
            layout.pathDirectory.appendingPathComponent("tcr").path)
        XCTAssertNil(
            TcrTool.pathExecutable(
                searchDirectories: [layout.bundle], bundle: layout.bundle, fileManager: .default))
        XCTAssertNil(
            TcrTool.pathExecutable(
                searchDirectories: [layout.empty], bundle: layout.bundle, fileManager: .default))
    }

    /// A `tcr` without `--from-codex` predates Codex accounts; the last one before the
    /// in-app sign-in has it, so it does not.
    func testThePredatesCodexJudgement() {
        XCTAssertTrue(
            TcrTool.predatesCodexAccounts(
                loginHelp: "Usage: tcr login [OPTIONS]\n      --non-interactive  Drive the login\n"))
        XCTAssertFalse(
            TcrTool.predatesCodexAccounts(loginHelp: LoginCapabilityTests.helpBeforeCodexSignIn))
    }

    /// `tcr --version` prints the name, then the version.
    func testTheVersionIsReadFromTheVersionLine() {
        XCTAssertEqual(TcrTool.version(fromVersionOutput: "tcr 0.2.44\n"), "0.2.44")
        XCTAssertNil(TcrTool.version(fromVersionOutput: "garbage"))
    }

    /// TcrBar's own `tcr` is named only when a shell would find no other.
    func testTheBundledTcrIsNamedOnlyWhenNoOtherIsFound() throws {
        let layout = try makeLayout()
        XCTAssertNil(
            TcrTool.bundledTcrWithoutCLI(
                searchDirectories: [layout.pathDirectory], bundle: layout.bundle,
                fileManager: .default))
        XCTAssertEqual(
            TcrTool.bundledTcrWithoutCLI(
                searchDirectories: [layout.empty], bundle: layout.bundle, fileManager: .default),
            layout.bundle.appendingPathComponent("tcr").path)
        XCTAssertNil(
            TcrTool.bundledTcrWithoutCLI(
                searchDirectories: [layout.empty], bundle: layout.empty, fileManager: .default))
    }

    /// A picked codex must be a file this Mac can run, through a symlink or not. A
    /// `Codex.app` folder is executable to `access(2)` and is still not a program.
    func testACodexChoiceMustBeAnExecutableFile() throws {
        let root = try temporaryFolder()
        let fileManager = FileManager.default
        let runnable = root.appendingPathComponent("codex")
        try writeScript(at: runnable, mode: 0o755)
        let notRunnable = root.appendingPathComponent("codex-0644")
        try writeScript(at: notRunnable, mode: 0o644)
        let linkToRunnable = root.appendingPathComponent("codex-link")
        try fileManager.createSymbolicLink(at: linkToRunnable, withDestinationURL: runnable)
        let linkToNotRunnable = root.appendingPathComponent("codex-0644-link")
        try fileManager.createSymbolicLink(at: linkToNotRunnable, withDestinationURL: notRunnable)
        let app = root.appendingPathComponent("Codex.app", isDirectory: true)
        try fileManager.createDirectory(at: app, withIntermediateDirectories: true)
        try fileManager.setAttributes([.posixPermissions: 0o755], ofItemAtPath: app.path)

        XCTAssertTrue(CodexExecutableChoice.isAcceptable(path: runnable.path))
        XCTAssertTrue(CodexExecutableChoice.isAcceptable(path: linkToRunnable.path))
        XCTAssertFalse(CodexExecutableChoice.isAcceptable(path: app.path), "a folder was accepted")
        XCTAssertFalse(CodexExecutableChoice.isAcceptable(path: notRunnable.path))
        XCTAssertFalse(CodexExecutableChoice.isAcceptable(path: linkToNotRunnable.path))
        XCTAssertFalse(
            CodexExecutableChoice.isAcceptable(path: root.appendingPathComponent("missing").path))
    }

    // MARK: - Helpers

    /// `bundle/tcr` and `pathdir/tcr`, both executable, and an empty folder.
    private func makeLayout() throws -> (bundle: URL, pathDirectory: URL, empty: URL) {
        let root = try temporaryFolder()
        let bundle = root.appendingPathComponent("bundle", isDirectory: true)
        let pathDirectory = root.appendingPathComponent("pathdir", isDirectory: true)
        let empty = root.appendingPathComponent("empty", isDirectory: true)
        for directory in [bundle, pathDirectory, empty] {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        }
        try writeScript(at: bundle.appendingPathComponent("tcr"), mode: 0o755)
        try writeScript(at: pathDirectory.appendingPathComponent("tcr"), mode: 0o755)
        return (bundle, pathDirectory, empty)
    }

    private func writeScript(at url: URL, mode: Int) throws {
        try "#!/bin/sh\n".write(to: url, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: mode], ofItemAtPath: url.path)
    }

    /// A fresh folder, removed when the test ends.
    private func temporaryFolder() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-tcrtool-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}
