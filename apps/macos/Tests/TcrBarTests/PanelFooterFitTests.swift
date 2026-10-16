import AppKit
import XCTest

@testable import TcrBarCore

/// Every arm of the Sessions and Tools footers fits its row at the panel's width.
///
/// The polish pass shortened both Claude-only lines because they drew truncated beside their
/// trailing fact at 372 pt, and the Tools line is the panel's one privacy claim. A fleet holding
/// a Codex account draws the same footer in its own words, so each of those is held to the same
/// row. The row is `PanelFooter`'s: the panel's width inside its side padding, less the trailing
/// fact (fixed size, it never gives way) and the footer gap on each side of the spacer between
/// them, and on Sessions less the glyph and its gap. The tokens are read from `V4.swift`, the
/// way `SegmentedTabsFitTests` reads them, because this bundle does not link the app target.
///
/// Account names are obviously fake. This repository is public.
final class PanelFooterFitTests: XCTestCase {

    func testEverySessionsFooterFitsBesideTheSparklineNote() throws {
        let trailing = try XCTUnwrap(
            firstCapture(
                of: "if tab == \\.sessions \\{ return \"([^\"]+)\" \\}",
                in: try panelSource("FleetView.swift")),
            "the Sessions footer's trailing note is no longer a literal in v4FooterTrailing")
        let glyph = try XCTUnwrap(
            NSImage(systemSymbolName: "shippingbox", accessibilityDescription: nil)?
                .withSymbolConfiguration(.init(pointSize: try token("footerGlyph"), weight: .regular))
        ).size.width
        let room = try row(trailing: trailing) - glyph - (try token("sectionHeadGap"))
        for (pool, fleet) in try fleets() {
            let line = SessionsCodexCopy.sessionsFooter(fleet)
            XCTAssertLessThanOrEqual(
                try width(line), room,
                "the \(pool) Sessions footer \"\(line)\" truncates beside \"\(trailing)\"")
        }
    }

    func testEveryToolsFooterFitsItsRow() throws {
        // A Claude-only fleet's line sits beside the build id, `server abc1234` in the render
        // fixtures. A fleet holding a Codex account gives the id up to its longer line
        // (`v4FooterTrailing`), so that line has the whole row.
        let claude = SessionsCodexCopy.toolsFooter(holdsCodex: false)
        XCTAssertLessThanOrEqual(
            try width(claude), try row(trailing: "server abc1234"),
            "the Claude-only Tools footer \"\(claude)\" truncates beside the build id")
        let codex = SessionsCodexCopy.toolsFooter(holdsCodex: true)
        XCTAssertLessThanOrEqual(
            try width(codex), try row(trailing: nil),
            "the Codex Tools footer \"\(codex)\" truncates")
    }

    // MARK: - The footer's row

    /// The room the leading text has: the panel inside its side padding, less the trailing
    /// fact and a footer gap on each side of the spacer, or one gap when there is no fact.
    private func row(trailing: String?) throws -> CGFloat {
        let inner = try token("panelWidth") - 2 * (try token("panelPaddingSide"))
        let gap = try token("footerGap")
        guard let trailing else { return inner - gap }
        return inner - 2 * gap - (try width(trailing))
    }

    /// `V4.font(V4.footerSize)`: the system font with tabular digits.
    private func width(_ text: String) throws -> CGFloat {
        let font = NSFont.monospacedDigitSystemFont(ofSize: try token("footerSize"), weight: .regular)
        return (text as NSString).size(withAttributes: [.font: font]).width
    }

    // MARK: - Fleets

    private func fleets() throws -> [(String, Fleet)] {
        let claude = account(name: "alice@example.com")
        let codex = "{\"provider\":\"codex\"," + account(name: "bob@example.com/codex").dropFirst()
        return [
            ("Claude-only", try decodeFleet([claude])),
            ("mixed", try decodeFleet([claude, codex])),
            ("Codex-only", try decodeFleet([codex])),
        ]
    }

    private func decodeFleet(_ rows: [String]) throws -> Fleet {
        try Fleet.decode(Data("[\(rows.joined(separator: ","))]".utf8))
    }

    private func account(name: String) -> String {
        """
        {"name":"\(name)","priority":0,"status":"active",
         "disabled":false,"quota":0.5,"quotaState":"ok",
         "fiveHour":0.1,"sevenDay":0.1,
         "sevenDayOi":0.1,"sevenDayOiState":"ok","sevenDayOiResetAtMs":null,
         "held":[],
         "requests":0,"inputTokens":0,"outputTokens":0,
         "cacheReadTokens":0,"cacheHitRatio":0.0,"probeStatus":"ok",
         "probeError":null,
         "lastStreamError":null,"streamErrorCount":0,"source":"live",
         "serverSha":"abc1234","serverDirty":false}
        """
    }

    // MARK: - Reading the app target's source

    private func token(_ name: String) throws -> CGFloat {
        let value = try XCTUnwrap(
            firstCapture(
                of: "static let \(name): CGFloat = ([0-9.]+)", in: try panelSource("PanelV4/V4.swift")),
            "V4.\(name) is no longer a plain `static let … = <number>`")
        return CGFloat(try XCTUnwrap(Double(value)))
    }

    private func firstCapture(of pattern: String, in source: String) -> String? {
        guard let regex = try? NSRegularExpression(pattern: pattern),
            let match = regex.firstMatch(
                in: source, range: NSRange(source.startIndex..., in: source)),
            let range = Range(match.range(at: 1), in: source)
        else { return nil }
        return String(source[range])
    }

    private func panelSource(_ relative: String) throws -> String {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // -> TcrBarTests
            .deletingLastPathComponent()  // -> Tests
            .deletingLastPathComponent()  // -> apps/macos
            .deletingLastPathComponent()  // -> apps
            .deletingLastPathComponent()  // -> repo root
        let file = repoRoot.appendingPathComponent("apps/macos/Sources/TcrBar/\(relative)")
        return try String(contentsOf: file, encoding: .utf8)
    }
}
