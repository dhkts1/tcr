import Foundation
import XCTest

@testable import TcrBarCore

/// The words of the Codex sign-in sheet, its help texts, the Re-import item, the notice
/// for a bundled `tcr` that cannot read a Codex config, and the Codex rows in Settings,
/// each pinned in full.
///
/// None of these is tcr's own reason: the sheet shows those verbatim, and no test here
/// pins one.
final class CodexSignInCopyTests: XCTestCase {

    private let bundledTcr = "/Applications/TcrBar.app/Contents/MacOS/tcr"

    /// The chooser that Add account… opens on a Mac that shows signs of codex.
    func testTheChooser() {
        XCTAssertEqual(CodexSignInCopy.chooserTitle, "Add an account")
        XCTAssertEqual(
            CodexSignInCopy.chooserDetail,
            "A Claude account serves Claude Code. A Codex account is a ChatGPT login that serves the codex CLI."
        )
        XCTAssertEqual(CodexSignInCopy.importLink, "Import a codex login instead…")
        XCTAssertEqual(CodexSignInCopy.cancelButton, "Cancel")
        XCTAssertEqual(CodexSignInCopy.codexAccountButton, "Codex account…")
        XCTAssertEqual(CodexSignInCopy.claudeAccountButton, "Claude account…")
    }

    /// The Codex sign-in while it starts, and while the browser has it. Copy link is
    /// mentioned only once a link exists to copy.
    func testTheOpeningAndWaitingDetails() {
        XCTAssertEqual(
            CodexSignInCopy.openingDetail, "Asking tcr to run `codex login` in a folder of its own.")
        let waiting =
            "Finish in the browser window that just opened. This ChatGPT login is tcr's own, so the codex CLI on this Mac keeps the login it has. Nothing is written until it comes back."
        XCTAssertEqual(CodexSignInCopy.waitingDetail(urlSeen: false), waiting)
        XCTAssertTrue(
            CodexSignInCopy.waitingDetail(urlSeen: false)
                .hasSuffix("Nothing is written until it comes back."))
        XCTAssertEqual(
            CodexSignInCopy.waitingDetail(urlSeen: true),
            waiting + " If no window appeared, copy the link and open it yourself.")
    }

    /// The saved sheet, for a new account and for a re-login, with and without a plan,
    /// and with the way to start codex when this Mac has no tcr but TcrBar's own.
    func testTheSavedSheet() {
        XCTAssertEqual(
            CodexSignInCopy.savedTitle(account: "bob@example.com", relogin: false),
            "Signed in as bob@example.com")
        XCTAssertEqual(
            CodexSignInCopy.savedTitle(account: "bob@example.com", relogin: true),
            "Signed in again as bob@example.com")
        let withPlan =
            "A Codex account (ChatGPT Pro). It serves Codex sessions started with `tcr run --codex`; the list here fills in on the next poll."
        XCTAssertEqual(CodexSignInCopy.savedDetail(plan: "ChatGPT Pro"), withPlan)
        XCTAssertEqual(
            CodexSignInCopy.savedDetail(plan: nil),
            "A Codex account. It serves Codex sessions started with `tcr run --codex`; the list here fills in on the next poll."
        )
        XCTAssertEqual(
            CodexSignInCopy.savedDetail(plan: "ChatGPT Pro", bundledTcrWithoutCLI: bundledTcr),
            withPlan + CodexBanner.noCLITail(bundledTcr: bundledTcr))
        XCTAssertEqual(
            CodexSignInCopy.savedDetail(plan: "ChatGPT Pro", bundledTcrWithoutCLI: nil), withPlan)
    }

    /// The import's confirmation, which explains guarded token sharing before anything runs,
    /// and the folder picker that leads to it.
    func testTheImportConfirmation() {
        XCTAssertEqual(
            CodexSignInCopy.importTitle(shortPath: "~/.codex"), "Import the codex login in ~/.codex?")
        XCTAssertEqual(
            CodexSignInCopy.reimportTitle(name: "erin@example.com/codex", shortPath: "~/.codex"),
            "Re-import erin@example.com/codex from ~/.codex?")
        XCTAssertEqual(
            CodexSignInCopy.importText,
            "This shares the codex CLI's login in that folder. tcr checks auth.json before writing refreshed tokens and can adopt replacement tokens for the same ChatGPT account. A simultaneous codex refresh can still race this update. Signing in again with `codex login` in that folder revokes this shared login; tcr may then need sign-in. Codex account… gives tcr a separate login."
        )
        XCTAssertEqual(CodexSignInCopy.importButton, "Import")
        XCTAssertEqual(
            CodexSignInCopy.importPanelMessage, "Choose the codex folder whose login tcr should share")
        XCTAssertEqual(CodexSignInCopy.panelPrompt, "Choose")
    }

    /// The imported sheet, with and without a plan, and with the way to start codex.
    func testTheImportedSheet() {
        XCTAssertEqual(
            CodexSignInCopy.importedTitle(account: "bob@example.com"), "Imported bob@example.com")
        let withPlan =
            "A Codex account (ChatGPT Pro), sharing the codex CLI's login in ~/.codex. It serves Codex sessions started with `tcr run --codex`."
        XCTAssertEqual(
            CodexSignInCopy.importedDetail(plan: "ChatGPT Pro", shortPath: "~/.codex"), withPlan)
        XCTAssertEqual(
            CodexSignInCopy.importedDetail(plan: nil, shortPath: "~/.codex"),
            "A Codex account, sharing the codex CLI's login in ~/.codex. It serves Codex sessions started with `tcr run --codex`."
        )
        XCTAssertEqual(
            CodexSignInCopy.importedDetail(
                plan: "ChatGPT Pro", shortPath: "~/.codex", bundledTcrWithoutCLI: bundledTcr),
            withPlan + CodexBanner.noCLITail(bundledTcr: bundledTcr))
        XCTAssertEqual(
            CodexSignInCopy.importedDetail(
                plan: "ChatGPT Pro", shortPath: "~/.codex", bundledTcrWithoutCLI: nil),
            withPlan)
    }

    /// A refused sign-in is titled by the code tcr sent, never by its words; a code this
    /// TcrBar does not know, or none, keeps the title the sheet always had.
    func testEveryFailedTitle() {
        let titles: [String: String] = [
            "codex-fedramp": "tcr can't pool a FedRAMP workspace",
            "codex-routed": "This ChatGPT workspace can't be pooled",
            "codex-unconfirmed": "Couldn't confirm this workspace's routing",
            "codex-missing": "TcrBar could not find the codex CLI",
            "codex-identity": "That browser signed in a different ChatGPT account or workspace",
            "codex-login-failed": "codex login did not finish",
            "account-is-codex": "This is a Codex account",
            "account-is-claude": "This is a Claude account",
        ]
        for (code, title) in titles {
            XCTAssertEqual(CodexSignInCopy.failedTitle(code: code), title, code)
        }
        XCTAssertEqual(CodexSignInCopy.failedTitle(code: nil), "Sign-in failed")
        XCTAssertEqual(CodexSignInCopy.failedTitle(code: "something-new"), "Sign-in failed")
    }

    /// The way out when tcr cannot find codex, and what a refused choice says.
    func testTheMissingCodexChooser() {
        XCTAssertEqual(CodexSignInCopy.chooseCodexButton, "Choose codex…")
        XCTAssertEqual(CodexSignInCopy.codexPanelMessage, "Choose the codex executable")
        XCTAssertEqual(
            CodexSignInCopy.refusedCodexChoice(shortPath: "~/Downloads/codex"),
            "~/Downloads/codex is not an executable file.")
    }

    /// The help on the chooser's items and on a Codex row's Re-login.
    func testTheHelpTexts() {
        XCTAssertEqual(
            CodexSignInCopy.oldTcrHelp,
            "This tcr predates Codex sign-in, so this opens Terminal with the two commands. The panel lists the account on the next poll after they finish."
        )
        XCTAssertEqual(
            CodexSignInCopy.codexAccountHelp,
            "Signs in to ChatGPT through codex's own sign-in, run by tcr in a folder of its own: the codex CLI's login is not touched. The account is written only when the browser comes back."
        )
        XCTAssertEqual(
            CodexSignInCopy.importLinkHelp,
            "Shares a codex login through guarded auth.json token updates. A simultaneous codex refresh can still race the update. Signing in again with `codex login` in that folder revokes this shared login; tcr may then need sign-in."
        )
        XCTAssertEqual(CodexSignInCopy.reloginHint, "Signs in to ChatGPT")
        XCTAssertEqual(
            CodexSignInCopy.reloginHelp,
            "Signs in to ChatGPT in your browser, through codex's own sign-in run in a folder of tcr's own, requesting this exact account. tcr refuses to save if the browser hands back a different account or workspace. The codex CLI's own login is not touched."
        )
    }

    /// Add account… keeps its help on a Mac without signs of codex, and names both
    /// providers on one with them.
    func testTheAddAccountHelpIsTodaysWithoutPresence() {
        XCTAssertNil(CodexSignInCopy.addAccountHelp(isPresent: false))
        XCTAssertEqual(
            CodexSignInCopy.addAccountHelp(isPresent: true),
            "Adds a Claude account, or a Codex (ChatGPT) account for the codex CLI, signing in through your browser in this app. The account is written only when the browser comes back; an older tcr falls back to a Terminal window."
        )
    }

    /// The Re-import item on a Codex row imported from a folder, and its help.
    func testTheReimportItem() {
        XCTAssertEqual(CodexSignInCopy.reimportItem(shortPath: "~/.codex"), "Re-import from ~/.codex…")
        XCTAssertEqual(
            CodexSignInCopy.reimportHelp(shortPath: "~/.codex"),
            "Take the login the codex CLI in ~/.codex holds now. Refreshed tokens are shared through guarded auth.json updates; a simultaneous codex refresh can still race the update. Signing in again with `codex login` in that folder revokes this shared login; tcr may then need sign-in."
        )
    }

    /// What the panel says when the tcr inside TcrBar cannot read a config that holds a
    /// Codex account, as a title and detail, and as one sentence where one line is all
    /// there is.
    func testThePredatesCodexNotice() {
        XCTAssertEqual(
            CodexSignInCopy.predatesCodexTitle, "The selected tcr predates Codex accounts")
        XCTAssertEqual(
            CodexSignInCopy.predatesCodexDetail,
            "Your config holds a Codex account, which this tcr cannot read. Update the selected tcr, or clear its TCR_BIN or TcrExecutablePath override."
        )
        XCTAssertEqual(
            CodexSignInCopy.predatesCodexSentence,
            "The selected tcr predates Codex accounts. Your config holds a Codex account, which this tcr cannot read. Update the selected tcr, or clear its TCR_BIN or TcrExecutablePath override."
        )
        XCTAssertEqual(
            CodexSignInCopy.predatesCodexSentence,
            CodexSignInCopy.predatesCodexTitle + ". " + CodexSignInCopy.predatesCodexDetail)
    }

    /// The Codex row in Settings › General › Server, for every route the proxy can
    /// report, and for none yet.
    func testTheSettingsCodexRow() throws {
        XCTAssertEqual(CodexSignInCopy.settingsCodexLabel, "Codex")
        XCTAssertEqual(
            CodexSignInCopy.settingsCodexValue(route: nil), "Unknown until the server answers")
        XCTAssertEqual(
            CodexSignInCopy.settingsCodexValue(route: try route(#"{"pooled":true}"#)),
            "Pooled: tcr marks Codex connections to chatgpt.com")
        XCTAssertEqual(
            CodexSignInCopy.settingsCodexValue(
                route: try route(#"{"pooled":true,"upstream":"https://chatgpt.example.test"}"#)),
            "Pooled through https://chatgpt.example.test")
        let reason =
            "the proxy on :3456 is running without MITM: its TLS material failed to load at boot — see its log"
        XCTAssertEqual(
            CodexSignInCopy.settingsCodexValue(
                route: try route(#"{"pooled":false,"code":"mitm-off","reason":"\#(reason)"}"#)),
            "Not pooled: " + reason)
    }

    /// The flag under Settings › Updates' command-line tcr, and the caption on the four
    /// rotation rows that only Claude accounts follow.
    func testThePathTcrFlagAndTheRotationCaption() {
        XCTAssertEqual(
            CodexSignInCopy.pathTcrFlag(path: "/usr/local/bin/tcr", version: "0.2.44"),
            "/usr/local/bin/tcr · 0.2.44 — predates Codex accounts; update it before it next runs")
        XCTAssertEqual(
            CodexSignInCopy.pathTcrFlag(path: "/usr/local/bin/tcr", version: nil),
            "/usr/local/bin/tcr — predates Codex accounts; update it before it next runs")
        XCTAssertEqual(CodexSignInCopy.rotationScope, "Claude accounts only")
    }

    /// A group with a Codex member counts each provider, and its help says what reserving
    /// it does to those members.
    func testTheGroupsMembersLineAndHelp() {
        XCTAssertEqual(CodexSignInCopy.groupMembers(claude: 2, codex: 1), "Members: 2 Claude, 1 Codex")
        XCTAssertEqual(
            CodexSignInCopy.groupReservedCodexHelp,
            "A Codex session cannot ask for a group, so while this group is reserved its Codex members serve nothing."
        )
    }

    /// A route decoded from the status wire's own JSON, so this test does not depend on
    /// how the type is built in code.
    private func route(_ json: String) throws -> CodexRoute {
        try JSONDecoder().decode(CodexRoute.self, from: Data(json.utf8))
    }
}
