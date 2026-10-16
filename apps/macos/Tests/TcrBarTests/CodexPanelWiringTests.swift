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
        // Why a row is signed out comes from a poll, so the note reads what the opening
        // read, never the row each poll brings: a poll inserted a wrapping note under an
        // open panel's line and moved everything below it.
        XCTAssertTrue(
            feed.contains("loginError: codexNotes.notes.signedOutError(for: account.ref)"),
            "the Codex note no longer reads the sign-out reason this opening read")
        XCTAssertFalse(
            feed.contains("account.loginError"), "the Codex note reads each poll's sign-out reason")
        let onPoll = try slice(
            fleetView, from: ".onChange(of: poller.lastPollAt) { _ in", to: "syncPeersProvider()")
        XCTAssertFalse(
            onPoll.contains("codexNotes") || onPoll.contains("reconcileCodexNotes"),
            "a poll reconciles the Codex notes, which removes one under an open panel")
        let shell = try source("apps/macos/Sources/TcrBar/MenuBarShell.swift")
        let opened = try XCTUnwrap(shell.range(of: "codexNotes.openedOn(fleet,"))
        let show = try XCTUnwrap(shell.range(of: "popover.show(relativeTo: button.bounds"))
        XCTAssertLessThan(opened.lowerBound, show.lowerBound, "the shell decides the notes after the show")
        XCTAssertFalse(
            try source("apps/macos/Sources/TcrBar/PanelV4/AccountLineRow.swift")
                .contains("notice.tone == .failure ?"),
            "RowNotices picks its colour or its words with a two-way ternary again, so an "
                + "alert would read as a note")
    }

    /// A group add's warning on a Codex row is a caution about how the row is set up, not
    /// a quota state, so it is drawn in the panel's dim ink: in the near-limit amber it sat
    /// beside the row's own amber bar and read as part of it. A note on a Claude row keeps
    /// its amber.
    func testAGroupAddWarningIsDrawnInTheDimInkNotTheNearLimitAmber() throws {
        let notices = try slice(
            try source("apps/macos/Sources/TcrBar/PanelV4/AccountLineRow.swift"),
            from: "private static func color(_ tone: AccountRowNotice.Tone) -> Color {",
            to: "private static func spoken(")
        XCTAssertTrue(
            notices.contains("case .caution: return Tok.dim"),
            "a Codex group-add warning is no longer drawn in the dim ink")
        XCTAssertTrue(
            notices.contains("case .note: return Tok.near"), "a note on a success lost its amber")
    }

    /// The chooser's import link is drawn in the system's link colour, which stays a link's
    /// blue whatever accent colour the Mac is set to: in the accent colour, a Mac set to
    /// graphite drew it as plain grey text.
    func testTheImportLinkIsDrawnInTheLinkColour() throws {
        let sheet = try slice(
            try source("apps/macos/Sources/TcrBar/LoginSheet.swift"),
            from: "Button(CodexSignInCopy.importLink)", to: ".help(CodexSignInCopy.importLinkHelp)")
        XCTAssertTrue(sheet.contains(".foregroundStyle(Tok.link)"), "the import link lost its colour")
        XCTAssertTrue(
            try source("apps/macos/Sources/TcrBar/Tokens.swift")
                .contains("public static let link = Color(nsColor: .linkColor)"),
            "Tok.link is no longer the system's link colour")
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
