import XCTest

@testable import TcrBarCore

/// The Tailscale row's structure, as assertions about the source that draws
/// it.
///
/// Source-reading for the reason `PeerInternetRowLayoutTests` gives: the test
/// target links `TcrBarCore` alone, and `PeerTailnetRow` and the pane are views
/// in the executable. The drawn result is checked with `--render-states --only
/// tn-row` and `--render-settings`; these stop the source going back.
final class PeerTailnetRowWiringTests: XCTestCase {

    /// The row sits in This Mac, directly under the internet switch (the
    /// tailnet mockup's scene 1).
    func testTheRowIsDrawnUnderTheInternetSwitch() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let thisMac = try slice(
            pane, from: "private var thisMac: some View {", to: "} header: {")
        guard let internet = thisMac.range(of: "PeerInternetRow("),
            let tailnet = thisMac.range(of: "PeerTailnetRow(")
        else {
            return XCTFail("This Mac no longer draws both the internet row and the Tailscale row")
        }
        XCTAssertLessThan(
            internet.lowerBound, tailnet.lowerBound,
            "the Tailscale row is drawn above the internet switch, not under it")
    }

    /// The row's two flexible columns have an IDEAL width of zero. Without it
    /// the grouped `Form` widened its column to the row's one-line width and
    /// the pairing request's title further down drew as one truncated line
    /// (`--render-settings`, 2026-10-20).
    func testTheRowDoesNotWidenTheFormsColumn() throws {
        let row = try source("apps/macos/Sources/TcrBar/PeerTailnetRow.swift")
        let body = try slice(row, from: "var body: some View {", to: "// MARK: The picker")
        XCTAssertEqual(
            body.components(
                separatedBy:
                    ".frame(minWidth: 0, idealWidth: 0, maxWidth: .infinity, alignment: .leading)"
            ).count - 1, 2,
            "the label column and the state line must both report an ideal width of zero")
    }

    /// One line, reserved: the state line never wraps and never collapses.
    func testTheStateLineIsOneReservedLine() throws {
        let row = try source("apps/macos/Sources/TcrBar/PeerTailnetRow.swift")
        let line = try slice(row, from: "private var stateLine: some View {", to: "\n    }\n}")
        XCTAssertTrue(
            line.contains(#"Text("Ag").font(.caption).hidden()"#),
            "the hidden line that reserves the state line's height is gone, so a state with "
                + "no words would collapse the row")
        XCTAssertTrue(
            line.contains(".lineLimit(1)"),
            "the state line can wrap, so a long state would grow the row")
    }

    /// The pane polls `tcr peer tailnet --json` while it is on screen, and a
    /// render run polls nothing.
    func testThePanePollsAndARenderDoesNot() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(pane.contains(".task { await pollTailnet() }"))
        let poll = try slice(pane, from: "private func pollTailnet() async {", to: "\n    }\n")
        XCTAssertTrue(
            poll.contains("guard RenderSettings.requestedDirectory() == nil else { return }"),
            "a render run would shell out to tcr")
        XCTAssertTrue(pane.contains("controller.capture(PeerCommand.tailnetRead)"))
    }

    // MARK: - Source helpers (same technique as PeerInternetRowLayoutTests)

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
    }

    private func source(_ relativePath: String) throws -> String {
        try String(
            contentsOf: repoRoot().appendingPathComponent(relativePath), encoding: .utf8)
    }

    private func slice(_ contents: String, from: String, to: String) throws -> String {
        guard let start = contents.range(of: from) else {
            throw Anchor.missing(from)
        }
        let rest = contents[start.upperBound...]
        guard let end = rest.range(of: to) else {
            throw Anchor.missing(to)
        }
        return String(rest[..<end.lowerBound])
    }

    private enum Anchor: Error, CustomStringConvertible {
        case missing(String)
        var description: String {
            switch self {
            case .missing(let anchor): return "anchor not found: \(anchor)"
            }
        }
    }
}
