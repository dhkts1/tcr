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
}
