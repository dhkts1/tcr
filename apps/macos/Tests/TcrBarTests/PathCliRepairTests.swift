import Darwin
import XCTest

@testable import TcrBarCore

/// ``PathCliRepair`` against real files in a temp folder and real `tcr` stand-ins:
/// two-line shell scripts that print a version, so `--version` is a real process.
///
/// The bundle is laid out as `<tmp>/Applications/TcrBar.app/Contents/MacOS/tcr`, the
/// installed shape, and the `PATH` copy is `<tmp>/bin/tcr`. Nothing outside the temp
/// folder is read or written.
final class PathCliRepairTests: XCTestCase {
    private var root: URL!
    private var bin: URL!
    private var pathTcr: URL!
    private var bundledTcr: URL!

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory
            .appendingPathComponent("PathCliRepairTests-\(UUID().uuidString)", isDirectory: true)
        bin = root.appendingPathComponent("bin", isDirectory: true)
        let macOS = root.appendingPathComponent("Applications/TcrBar.app/Contents/MacOS", isDirectory: true)
        try FileManager.default.createDirectory(at: bin, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: macOS, withIntermediateDirectories: true)
        pathTcr = bin.appendingPathComponent("tcr")
        bundledTcr = macOS.appendingPathComponent("tcr")
        try writeScript(at: bundledTcr, printing: "tcr 1.2.20")
    }

    override func tearDownWithError() throws {
        chmod(bin.path, 0o755)
        try? FileManager.default.removeItem(at: root)
    }

    private func writeScript(at url: URL, printing line: String, exitCode: Int = 0) throws {
        try "#!/bin/sh\necho '\(line)'\nexit \(exitCode)\n".write(to: url, atomically: false, encoding: .utf8)
        chmod(url.path, 0o755)
    }

    private func repair() -> PathCliRepair.Outcome {
        PathCliRepair.run(pathTcr: pathTcr, bundledTcr: bundledTcr)
    }

    /// The `PATH` entry's inode, its kind and its bytes, read without following a symlink.
    private func snapshot() throws -> (inode: ino_t, isLink: Bool, bytes: Data?) {
        var info = stat()
        XCTAssertEqual(lstat(pathTcr.path, &info), 0)
        let isLink = info.st_mode & S_IFMT == S_IFLNK
        return (info.st_ino, isLink, isLink ? nil : try Data(contentsOf: pathTcr))
    }

    private func assertUntouched(
        _ before: (inode: ino_t, isLink: Bool, bytes: Data?), file: StaticString = #filePath,
        line: UInt = #line
    ) throws {
        let after = try snapshot()
        XCTAssertEqual(after.inode, before.inode, "the PATH entry was replaced", file: file, line: line)
        XCTAssertEqual(after.isLink, before.isLink, file: file, line: line)
        XCTAssertEqual(after.bytes, before.bytes, file: file, line: line)
    }

    private func onlyEntriesIn(_ directory: URL) throws -> [String] {
        try FileManager.default.contentsOfDirectory(atPath: directory.path).sorted()
    }

    // MARK: - The one case that acts

    func testPathCliRepairOlderRegularFileBecomesASymlinkToTheBundle() throws {
        try writeScript(at: pathTcr, printing: "tcr 0.2.28")
        let before = try snapshot()

        let outcome = repair()

        XCTAssertEqual(
            outcome,
            .repointed(
                pathTcr: pathTcr.path, bundledTcr: bundledTcr.path,
                from: PathCliRepair.Version(major: 0, minor: 2, patch: 28),
                to: PathCliRepair.Version(major: 1, minor: 2, patch: 20)))
        let after = try snapshot()
        XCTAssertTrue(after.isLink)
        XCTAssertNotEqual(after.inode, before.inode)
        XCTAssertEqual(
            try FileManager.default.destinationOfSymbolicLink(atPath: pathTcr.path), bundledTcr.path)
        // The shell's `tcr` now answers as the bundled one.
        XCTAssertEqual(PathCliRepair.versionByRunning(pathTcr), PathCliRepair.Version("1.2.20"))
        // No temporary link left beside it.
        XCTAssertEqual(try onlyEntriesIn(bin), ["tcr"])
        XCTAssertTrue(outcome.logLine.contains("now a symlink to"))
    }

    /// Numeric, not string: "1.10.0" sorts before "1.9.0" as text.
    func testPathCliRepairComparesVersionPartsAsNumbers() throws {
        try writeScript(at: bundledTcr, printing: "tcr 1.10.0")
        try writeScript(at: pathTcr, printing: "tcr 1.9.3")

        guard case .repointed = repair() else { return XCTFail("1.9.3 is older than 1.10.0") }
    }

    // MARK: - Every case that leaves it alone

    func testPathCliRepairNewerPathCopyIsUntouched() throws {
        try writeScript(at: pathTcr, printing: "tcr 1.3.0")
        let before = try snapshot()

        guard case .notOlder = repair() else { return XCTFail("a newer PATH tcr must be left alone") }
        try assertUntouched(before)
    }

    func testPathCliRepairEqualVersionIsUntouched() throws {
        try writeScript(at: pathTcr, printing: "tcr 1.2.20")
        let before = try snapshot()

        guard case .notOlder = repair() else { return XCTFail("an equal PATH tcr must be left alone") }
        try assertUntouched(before)
    }

    func testPathCliRepairUnreadableVersionIsUntouched() throws {
        try writeScript(at: pathTcr, printing: "usage: something else")
        let before = try snapshot()

        XCTAssertEqual(repair(), .pathVersionUnreadable(pathTcr: pathTcr.path))
        try assertUntouched(before)
    }

    func testPathCliRepairPathCopyThatFailsVersionIsUntouched() throws {
        try writeScript(at: pathTcr, printing: "tcr 0.2.28", exitCode: 2)
        let before = try snapshot()

        XCTAssertEqual(repair(), .pathVersionUnreadable(pathTcr: pathTcr.path))
        try assertUntouched(before)
    }

    func testPathCliRepairSymlinkIntoTheBundleIsUntouched() throws {
        XCTAssertEqual(symlink(bundledTcr.path, pathTcr.path), 0)
        let before = try snapshot()

        XCTAssertEqual(repair(), .alreadyBundled(pathTcr: pathTcr.path))
        try assertUntouched(before)
    }

    func testPathCliRepairSymlinkElsewhereIsUntouched() throws {
        let elsewhere = root.appendingPathComponent("cellar-tcr")
        try writeScript(at: elsewhere, printing: "tcr 0.2.28")
        XCTAssertEqual(symlink(elsewhere.path, pathTcr.path), 0)
        let before = try snapshot()

        guard case .symlinkElsewhere = repair() else { return XCTFail("someone chose that symlink") }
        try assertUntouched(before)
    }

    func testPathCliRepairReadOnlyFolderIsUntouchedAndNamesTheCommand() throws {
        try writeScript(at: pathTcr, printing: "tcr 0.2.28")
        let before = try snapshot()
        XCTAssertEqual(chmod(bin.path, 0o555), 0)

        let outcome = repair()

        guard case .directoryNotWritable = outcome else {
            return XCTFail("a folder this user cannot write must be left alone, got \(outcome)")
        }
        XCTAssertTrue(outcome.logLine.contains("ln -sf '\(bundledTcr.path)' '\(pathTcr.path)'"))
        try assertUntouched(before)
    }

    func testPathCliRepairBundledBinaryThatFailsVersionIsUntouched() throws {
        try writeScript(at: bundledTcr, printing: "tcr 1.2.20", exitCode: 1)
        try writeScript(at: pathTcr, printing: "tcr 0.2.28")
        let before = try snapshot()

        XCTAssertEqual(
            repair(), .bundledUnrunnable(pathTcr: pathTcr.path, bundledTcr: bundledTcr.path))
        try assertUntouched(before)
    }

    /// A build folder or a translocated copy is not something to point every shell at.
    func testPathCliRepairBundleOutsideApplicationsIsUntouched() throws {
        let buildMacOS = root.appendingPathComponent("build/TcrBar.app/Contents/MacOS", isDirectory: true)
        try FileManager.default.createDirectory(at: buildMacOS, withIntermediateDirectories: true)
        bundledTcr = buildMacOS.appendingPathComponent("tcr")
        try writeScript(at: bundledTcr, printing: "tcr 1.2.20")
        try writeScript(at: pathTcr, printing: "tcr 0.2.28")
        let before = try snapshot()

        guard case .bundleNotInstalled = repair() else { return XCTFail("not an installed app") }
        try assertUntouched(before)
    }

    func testPathCliRepairNoPathTcrDoesNothing() {
        XCTAssertEqual(PathCliRepair.run(pathTcr: nil, bundledTcr: bundledTcr), .noPathTcr)
    }

    func testPathCliRepairNoBundledTcrIsUntouched() throws {
        try writeScript(at: pathTcr, printing: "tcr 0.2.28")
        let before = try snapshot()

        XCTAssertEqual(
            PathCliRepair.run(pathTcr: pathTcr, bundledTcr: nil), .noBundledTcr(pathTcr: pathTcr.path))
        try assertUntouched(before)
    }

    // MARK: - Pieces

    func testPathCliRepairVersionParsing() {
        XCTAssertEqual(PathCliRepair.Version("1.2.20"), PathCliRepair.Version(major: 1, minor: 2, patch: 20))
        XCTAssertEqual(
            PathCliRepair.Version("1.2.21-dev"), PathCliRepair.Version(major: 1, minor: 2, patch: 21))
        XCTAssertNil(PathCliRepair.Version("1.2"))
        XCTAssertNil(PathCliRepair.Version("x.2.3"))
        XCTAssertNil(PathCliRepair.Version("1.2."))
        XCTAssertLessThan(PathCliRepair.Version("0.2.28")!, PathCliRepair.Version("1.2.20")!)
        XCTAssertLessThan(PathCliRepair.Version("1.9.9")!, PathCliRepair.Version("1.10.0")!)
    }

    func testPathCliRepairInstalledBundleShape() {
        XCTAssertTrue(
            PathCliRepair.isInstalledBundle(
                URL(fileURLWithPath: "/Applications/TcrBar.app/Contents/MacOS/tcr")))
        XCTAssertTrue(
            PathCliRepair.isInstalledBundle(
                URL(fileURLWithPath: "/Users/alice/Applications/TcrBar.app/Contents/MacOS/tcr")))
        XCTAssertFalse(
            PathCliRepair.isInstalledBundle(URL(fileURLWithPath: "/tmp/build/TcrBar.app/Contents/MacOS/tcr")))
        XCTAssertFalse(PathCliRepair.isInstalledBundle(URL(fileURLWithPath: "/Applications/tcr")))
    }
}
