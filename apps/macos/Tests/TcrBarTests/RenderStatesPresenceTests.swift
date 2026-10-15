import Foundation
import XCTest

/// No render harness looks at this Mac's codex.
///
/// The panel's live codex evidence comes from ``CodexPresence/resolve(environment:storedExecutable:home:probe:)``,
/// run by a ``CodexPresenceController`` that the menu-bar shell owns, and the tcr checks
/// beside it look at this Mac's `PATH`. A render that reached any of them would draw this
/// Mac's `~/.codex`, its codex, or its command-line tcr into a picture the pixel gate
/// compares, and that picture would differ from Mac to Mac. The harnesses pass
/// ``CodexPresence/absent`` and fixed values instead; this reads their sources to keep it so.
final class RenderStatesPresenceTests: XCTestCase {

    func testNoRenderHarnessConstructsTheLiveResolver() throws {
        let live = [
            "CodexPresence.resolve", "CodexPresenceController(", "TcrTool.bundledTcrWithoutCLI(",
            "pathExecutable(", "TcrTool.pathTcr()",
        ]
        for file in ["RenderStates.swift", "RenderMark.swift", "RenderSettings.swift"] {
            let text = try source(file)
            for call in live {
                XCTAssertFalse(
                    text.contains(call),
                    "\(file) calls \(call): a render would see this Mac's ~/.codex, its codex "
                        + "or its tcr, and draw a different picture on every Mac")
            }
        }

        XCTAssertTrue(
            try source("MenuBarShell.swift").contains("CodexPresenceController("),
            "the menu-bar shell no longer builds the live resolver, so this test reads the "
                + "wrong file and proves nothing")
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
