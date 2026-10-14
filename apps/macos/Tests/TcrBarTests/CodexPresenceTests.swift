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
}
