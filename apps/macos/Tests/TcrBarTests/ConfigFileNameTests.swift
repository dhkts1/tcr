import XCTest

@testable import TcrBarCore

/// The settings hint names the file `tcr` actually reads: the four cases of
/// `src/paths.rs`'s rule, over a fake file system.
final class ConfigFileNameTests: XCTestCase {
    private let home = URL(fileURLWithPath: "/Users/alice")

    private func path(existing: Set<String>) -> String {
        ConfigFileName.displayPath(home: home) { existing.contains($0) }
    }

    private func file(_ name: String) -> String {
        home.appendingPathComponent(".config").appendingPathComponent(name).path
    }

    func testOldOnlyNamesTheOldFile() {
        XCTAssertEqual(
            path(existing: [file(ConfigFileName.legacy)]), "~/.config/teamclaude.json")
    }

    func testNewOnlyNamesTheNewFile() {
        XCTAssertEqual(path(existing: [file(ConfigFileName.current)]), "~/.config/tcr.json")
    }

    func testBothTheNewOneWins() {
        XCTAssertEqual(
            path(existing: [file(ConfigFileName.current), file(ConfigFileName.legacy)]),
            "~/.config/tcr.json")
    }

    func testNeitherNamesTheFileAFirstRunCreates() {
        XCTAssertEqual(path(existing: []), "~/.config/tcr.json")
    }
}
