import XCTest

/// The wiring that carries Codex facts into the panel's views, read from the source.
///
/// The test target links `TcrBarCore` alone, so the views themselves cannot be built
/// here; the same technique `AccountCardExitPillWiringTests` uses. What this guards
/// reaches no picture the pixel gate compares: whether a card or line says which
/// provider it serves is spoken only, and a Codex row's note draws only in scenes that
/// hold a Codex account.
final class CodexPanelWiringTests: XCTestCase {

    /// Every host of a card or a line hands it whether the fleet holds Codex accounts,
    /// so each speaks `Claude account` in a mixed fleet: the kept-open card, the tall
    /// card and both line views in the Accounts tab, and the floating hover card.
    func testTheProviderReachesEveryCardAndLineHost() throws {
        let wiring = "namesProvider: fleet.holdsCodex"
        XCTAssertEqual(
            try occurrences(of: wiring, in: "apps/macos/Sources/TcrBar/PanelV4/AccountsTabV4.swift"),
            4,
            "the kept-open card, the tall card, the live line and the waiting line each pass it")
        XCTAssertEqual(
            try occurrences(of: wiring, in: "apps/macos/Sources/TcrBar/FleetView.swift"), 1,
            "the floating hover card is built from hoverCardBody, which passes it")
        XCTAssertGreaterThan(
            try occurrences(
                of: "namesProvider: namesProvider",
                in: "apps/macos/Sources/TcrBar/PanelV4/AccountHoverCard.swift"),
            0, "AccountHoverCardContent no longer hands it to its card")
        XCTAssertGreaterThan(
            try occurrences(
                of: "namesProvider: namesProvider",
                in: "apps/macos/Sources/TcrBar/PanelV4/AccountCard.swift"),
            0, "the card no longer speaks it")
    }

    /// A Codex row's note is one more line under the row, from the panel's own notice
    /// feed, and the notice view tells an alert from a failure in an exhaustive switch.
    func testTheCodexNoteRidesTheRowNotices() throws {
        let fleetView = try source("apps/macos/Sources/TcrBar/FleetView.swift")
        let feed = try slice(fleetView, from: "notices: { account in", to: "menu: { account in")
        XCTAssertTrue(
            feed.contains("codexNote: account.provider == .codex"),
            "the notice feed passes no Codex note, or passes one on a Claude row")
        XCTAssertTrue(
            feed.contains("CodexCardFacts.noteLine("),
            "the notice feed no longer builds the Codex note from the row")
        XCTAssertFalse(
            try source("apps/macos/Sources/TcrBar/PanelV4/AccountLineRow.swift")
                .contains("notice.tone == .failure ?"),
            "RowNotices picks its colour or its words with a two-way ternary again, so an "
                + "alert would read as a note")
    }

    // MARK: - Source helpers

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
    }

    private func source(_ relativePath: String) throws -> String {
        try String(contentsOf: repoRoot().appendingPathComponent(relativePath), encoding: .utf8)
    }

    /// How often `needle` occurs in the file, comment lines dropped.
    private func occurrences(of needle: String, in relativePath: String) throws -> Int {
        try source(relativePath)
            .split(separator: "\n")
            .filter { !$0.trimmingCharacters(in: .whitespaces).hasPrefix("//") }
            .map { $0.components(separatedBy: needle).count - 1 }
            .reduce(0, +)
    }

    private func slice(_ contents: String, from: String, to: String) throws -> String {
        let start = try XCTUnwrap(contents.range(of: from), "anchor not found: \(from)")
        let rest = contents[start.upperBound...]
        let end = try XCTUnwrap(rest.range(of: to), "anchor not found: \(to)")
        return String(rest[..<end.lowerBound])
    }
}
