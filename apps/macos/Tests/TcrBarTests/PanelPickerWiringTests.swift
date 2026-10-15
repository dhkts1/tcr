import XCTest

/// The sign-in sheet's two pickers, the codex folder to import and the codex executable,
/// run through the shell's runner, which keeps the panel up while they are open.
///
/// The panel is a transient popover: a picker taking focus would close it, and the
/// sheet's next step, the import's confirmation or the re-run with the chosen codex,
/// would be drawn into a panel nobody can see. Read from the source, because both files
/// are in the app target, which this test target does not link.
final class PanelPickerWiringTests: XCTestCase {

    func testEveryPickerRunsThroughTheShellsRunner() throws {
        let shell = try source("MenuBarShell.swift")
        let panels = shell.components(separatedBy: "FleetPanel(").count - 1
        XCTAssertEqual(panels, 2, "the shell builds the panel in its init and in openPanel(on:)")
        XCTAssertEqual(
            shell.components(separatedBy: "runOpenPanel: {").count - 1, panels,
            "a panel built without the runner opens no picker at all")
        XCTAssertTrue(
            try body(of: "func runOpenPanel(", in: shell).contains(".applicationDefined"),
            "the runner no longer holds the popover open while a picker runs")

        let fleetView = try source("FleetView.swift")
        for picker in ["private func chooseCodexFolder(", "private func chooseCodexExecutable("] {
            let body = try body(of: picker, in: fleetView)
            XCTAssertTrue(
                body.contains("runOpenPanel(panel)"),
                "\(picker) no longer goes through the shell's runner")
            XCTAssertFalse(
                body.contains("runModal()"),
                "\(picker) runs its picker itself, which closes the panel under the sheet")
        }
    }

    /// A function's text, from its declaration to its closing brace at the indentation
    /// these files use for a member.
    private func body(of declaration: String, in source: String) throws -> String {
        let start = try XCTUnwrap(source.range(of: declaration), "no \(declaration)")
        let rest = source[start.upperBound...]
        let end = try XCTUnwrap(rest.range(of: "\n    }\n"), "\(declaration) never closes")
        return String(rest[..<end.lowerBound])
    }

    private func source(_ name: String) throws -> String {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .appendingPathComponent("Sources/TcrBar/\(name)")
        return try String(contentsOf: url, encoding: .utf8)
    }
}
