import XCTest

/// The panel's Codex sign-in and repair wiring, read from the source.
///
/// The test target links `TcrBarCore` alone, so `FleetView` and the menu-bar shell cannot
/// be built here; this reads them as text, the technique `AccountCardExitPillWiringTests`
/// uses. What it guards is the hop from a click to the right request: the requests, the
/// copy and the note store are tested on their own in `TcrBarCore`.
final class FleetViewCodexWiringTests: XCTestCase {

    private let fleetView = "apps/macos/Sources/TcrBar/FleetView.swift"
    private let shell = "apps/macos/Sources/TcrBar/MenuBarShell.swift"

    /// Re-login on any row runs that row's own request, so a Codex row signs in to
    /// ChatGPT for itself and a Claude row runs the sign-in it always did. A Codex
    /// request runs in the sheet, or in Terminal for an older tcr.
    func testTheReloginEntryPointsRunTheRowsOwnRequest() throws {
        let source = try source(fleetView)
        XCTAssertTrue(
            source.contains("beginLogin(account.reloginRequest)"),
            "Re-login no longer runs the row's own request, so a Codex row would start a "
                + "Claude sign-in")
        for gone in ["beginLogin(reloggingIn:", "LoginSession(account:"] {
            XCTAssertFalse(
                source.contains(gone),
                "FleetView.swift still holds \(gone), a sign-in that can only be Claude's")
        }

        let begin = try slice(
            source, from: "private func beginLogin(_ request: LoginRequest) {",
            to: "private func endLogin(cancelling: Bool) {")
        for call in [
            ".begin(request)", "LoginCapability.readCodexPath(executable:",
            "LoginLauncher.launchCodexLogin(reloggingIn:", "LoginLauncher.launchCodexImport(codexHome:",
            "LoginCapability.probe(executable:", "LoginLauncher.launch(reloggingIn:",
        ] {
            XCTAssertTrue(
                begin.contains(call),
                "beginLogin lost \(call): a sign-in would no longer reach the sheet, or an "
                    + "older tcr its Terminal window")
        }
    }

    /// Add account… opens the chooser only on evidence of codex, and otherwise starts the
    /// Claude sign-in exactly as it did. The chooser's links and the missing-codex picker
    /// are wired.
    func testAddAccountOffersCodexOnlyOnEvidence() throws {
        let source = try source(fleetView)
        let add = try slice(
            source, from: "private func addAccount() {", to: "private func reloginAccount(")
        for call in [
            "effectivePresence.isPresent", "phase: .choosing", "LoginCapability.probeCodex(executable:",
            "beginLogin(.claude(account: nil))",
        ] {
            XCTAssertTrue(
                add.contains(call),
                "addAccount lost \(call): a Mac with codex would not be offered a Codex "
                    + "account, or one without it would be")
        }
        for wiring in [
            "onChoose:", "onImportLink:", "onConfirmImport:", "onChooseCodex:", ".confirmImport(home:",
            "forKey: TcrTool.codexOverrideDefaultsKey",
        ] {
            XCTAssertTrue(
                source.contains(wiring),
                "FleetView.swift lost \(wiring): a button in the sign-in sheet would do nothing")
        }
    }

    /// A Codex row's Re-login says it signs in to ChatGPT, in its help and to VoiceOver;
    /// a Claude row keeps the words it had.
    func testTheReloginHelpAndHintNameChatGPTOnACodexRow() throws {
        let source = try source(fleetView)
        for text in [
            "private var codexReloginHelp: String", "CodexSignInCopy.reloginHelp",
            "CodexSignInCopy.oldTcrHelp", "LoginCapability.knownCodexSupport(executable:",
        ] {
            XCTAssertTrue(
                source.contains(text),
                "FleetView.swift lost \(text): a Codex row's Re-login would describe a Claude "
                    + "sign-in")
        }

        let menu = try slice(
            source, from: "private var contextMenuItems: some View {",
            to: "private var groupMenuItems: some View {")
        XCTAssertTrue(menu.contains("codexReloginHelp"), "the menu item's help names no ChatGPT")
        XCTAssertTrue(
            menu.contains("one. The login it starts now mints a credential good for a "),
            "a Claude row's Re-login lost the help it had")

        let button = try slice(
            source, from: "private var reloginButton: some View {",
            to: "private var toggleButton: some View {")
        for text in [
            "codexReloginHelp", ".accessibilityHint(CodexSignInCopy.reloginHint)",
            #".accessibilityLabel("Re-login \(account.name)")"#,
            #"refuses to save if the browser hands back a different one.""#,
        ] {
            XCTAssertTrue(
                button.contains(text),
                "the Re-login button lost \(text): VoiceOver or the help would misname what "
                    + "it signs in to")
        }
    }

    /// Every Add account… button names both providers on a Mac with codex.
    func testEveryAddAccountButtonTakesTheCodexHelp() throws {
        let source = try source(fleetView)
        XCTAssertEqual(
            source.components(
                separatedBy:
                    "CodexSignInCopy.addAccountHelp(isPresent: effectivePresence.isPresent) ?? Self.addAccountHelp"
            ).count - 1, 3,
            "the v4 action row, the empty fleet and the legacy action row each take the Codex help")
        for gone in ["help: Self.addAccountHelp", ".help(Self.addAccountHelp)"] {
            XCTAssertFalse(
                source.contains(gone),
                "an Add account… button still says only Claude: \(gone)")
        }
    }

    /// A refused re-login and a group add's warning are kept under the account's line,
    /// and each poll drops a note the fleet has moved past.
    func testTheCardNotesAreRecordedAndReconciled() throws {
        let source = try source(fleetView)
        XCTAssertTrue(
            try slice(
                source, from: "private func endLogin(cancelling: Bool) {",
                to: "private func mintAccountToken("
            ).contains("codexNotes.recordReloginRefusal("),
            "a refused Codex re-login is forgotten when the sheet closes, so the line reads "
                + "re-login with no reason")
        for (from, to) in [
            ("private var addToGroupMenu: some View {", "private var everyGroupFleetWide: Set<String> {"),
            ("private func presentNewGroupPrompt() {", "private func confirmDeleteGroup("),
        ] {
            XCTAssertTrue(
                try slice(source, from: from, to: to).contains("codexNotes.recordGroupAddNotice("),
                "a group add from \(from) drops tcr's warning that no Codex request reaches it")
        }

        let notices = try slice(source, from: "notices: { account in", to: "menu: { account in")
        for note in [
            "codexNotes.notes.reloginRefusal(for: account.ref)",
            "codexNotes.notes.groupAddWarning(for: account.ref)",
        ] {
            XCTAssertTrue(
                notices.contains(note), "the line's notices no longer read \(note)")
        }
        for gone in ["reloginRefusal: nil", "groupAddWarning: nil", #"\.codexNotes"#] {
            XCTAssertFalse(
                source.contains(gone),
                "FleetView.swift holds \(gone): a recorded note would never reach the line")
        }

        let reconcile = try slice(
            source, from: "private func reconcileCodexNotes() {", to: "private func toggleKeptOpen(")
        for text in [
            "guard case .loaded(let fleet) = poller.state else { return }",
            "codexNotes.reconcile(with: fleet.accounts)",
        ] {
            XCTAssertTrue(
                reconcile.contains(text),
                "reconcileCodexNotes lost \(text): a failed poll would clear every note, or "
                    + "no poll would")
        }
        let poll = try slice(
            source, from: ".onChange(of: poller.lastPollAt)", to: ".confirmationDialog(")
        XCTAssertTrue(poll.contains("reconcileCodexNotes()"), "no poll reconciles the notes")
        XCTAssertTrue(poll.contains("pruneKeptOpenCards()"))
    }

    /// A bundled tcr too old for a config holding a Codex account is named, in the poll's
    /// banner and in every failure line, instead of its refusal's version numbers.
    func testAPreCodexTcrIsNamedInThePanel() throws {
        let source = try source(fleetView)
        for call in ["PredatesCodexRefusal.matches(", "PredatesCodexRefusal.displayed("] {
            XCTAssertTrue(
                source.contains(call),
                "FleetView.swift lost \(call): a too-old tcr would show its raw refusal")
        }
    }

    /// The shell resolves this Mac's codex for the panel and owns the one note store, so
    /// a note survives the panel closing.
    func testTheShellGivesThePanelThisMacsPresenceAndOneNoteStore() throws {
        let source = try source(shell)
        XCTAssertTrue(
            source.contains("codexPresence.refresh("),
            "the shell never resolves this Mac's codex, so Add account… never offers it")
        XCTAssertTrue(
            source.contains("codexNotes: codexNotes"),
            "the panel is not handed the shell's note store, so notes vanish with the panel")
        XCTAssertFalse(
            source.contains("codexPresence: .absent"),
            "the live panel still passes no codex evidence")
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

    private func slice(_ contents: String, from: String, to: String) throws -> String {
        guard let start = contents.range(of: from) else { throw Anchor.missing(from) }
        let rest = contents[start.upperBound...]
        guard let end = rest.range(of: to) else { throw Anchor.missing(to) }
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
