import Foundation

/// The words TcrBar itself says about adding and repairing a Codex account: the sign-in
/// sheet's titles and details, the help on the chooser's items and on a Codex row's
/// Re-login and Re-import, the notice for a bundled `tcr` that cannot read a Codex config,
/// and the Codex rows in Settings.
///
/// They live in this one place so a test can read every one of them. None of them words
/// one of tcr's own reasons: the sheet shows tcr's `reason` and every `notice` verbatim,
/// and titles a refusal by its `code` alone, so tcr can reword a reason without breaking
/// TcrBar. Short paths come from ``HomePath/short(_:home:)``.
public enum CodexSignInCopy {
    // MARK: - The chooser

    public static let chooserTitle = "Add an account"
    public static let chooserDetail =
        "A Claude account serves Claude Code. A Codex account is a ChatGPT login that serves the codex CLI."
    public static let importLink = "Import a codex login instead…"
    public static let cancelButton = "Cancel"
    public static let codexAccountButton = "Codex account…"
    public static let claudeAccountButton = "Claude account…"

    // MARK: - Signing in

    /// Under `Starting sign-in…`, for a Codex sign-in. An import's opening has no detail.
    public static let openingDetail = "Asking tcr to run `codex login` in a folder of its own."

    /// Under the waiting title. Copy link is mentioned only once a `browser` event has
    /// sent a URL, because only then is there a link to copy.
    public static func waitingDetail(urlSeen: Bool) -> String {
        let detail =
            "Finish in the browser window that just opened. This ChatGPT login is tcr's own, so the codex CLI on this Mac keeps the login it has. Nothing is written until it comes back."
        return urlSeen ? detail + " If no window appeared, copy the link and open it yourself." : detail
    }

    public static func savedTitle(account: String, relogin: Bool) -> String {
        relogin ? "Signed in again as \(account)" : "Signed in as \(account)"
    }

    /// The saved sheet's detail. Without a plan the parenthesis goes, and the rest stays.
    /// When this Mac has no tcr but TcrBar's own, the way to start codex follows.
    public static func savedDetail(plan: String?, bundledTcrWithoutCLI: String? = nil) -> String {
        let account = plan.map { "A Codex account (\($0))." } ?? "A Codex account."
        let detail =
            account
            + " It serves Codex sessions started with `tcr run --codex`; the list here fills in on the next poll."
        return detail + tail(bundledTcrWithoutCLI)
    }

    // MARK: - Importing

    public static func importTitle(shortPath: String) -> String {
        "Import the codex login in \(shortPath)?"
    }

    public static func reimportTitle(name: String, shortPath: String) -> String {
        "Re-import \(name) from \(shortPath)?"
    }

    /// What sharing a codex login permits, said before anything runs.
    public static let importText =
        "This shares the codex CLI's login in that folder. tcr writes refreshed tokens back only while auth.json still holds the refresh token it loaded, and adopts newer tokens for the same ChatGPT account. A changed login is left untouched. Codex account… gives tcr a separate login."
    public static let importButton = "Import"
    public static let importPanelMessage = "Choose the codex folder whose login tcr should share"
    /// The prompt of both pickers, the folder for an import and the codex executable.
    public static let panelPrompt = "Choose"

    public static func importedTitle(account: String) -> String {
        "Imported \(account)"
    }

    /// The imported sheet's detail, with the same no-plan rule and tail as
    /// ``savedDetail(plan:bundledTcrWithoutCLI:)``.
    public static func importedDetail(
        plan: String?, shortPath: String, bundledTcrWithoutCLI: String? = nil
    ) -> String {
        let account = plan.map { "A Codex account (\($0))" } ?? "A Codex account"
        let detail =
            account
            + ", sharing the codex CLI's login in \(shortPath). It serves Codex sessions started with `tcr run --codex`."
        return detail + tail(bundledTcrWithoutCLI)
    }

    // MARK: - Refusals

    /// The failed sheet's title, keyed on the `code` of tcr's `error` event. An unknown
    /// code, or none, keeps the title the sheet has always had.
    public static func failedTitle(code: String?) -> String {
        switch code {
        case "codex-fedramp": return "tcr can't pool a FedRAMP workspace"
        case "codex-routed": return "This ChatGPT workspace can't be pooled"
        case "codex-unconfirmed": return "Couldn't confirm this workspace's routing"
        case "codex-missing": return "TcrBar could not find the codex CLI"
        case "codex-identity": return "That browser signed in a different ChatGPT account or workspace"
        case "codex-login-failed": return "codex login did not finish"
        case "account-is-codex": return "This is a Codex account"
        case "account-is-claude": return "This is a Claude account"
        default: return "Sign-in failed"
        }
    }

    public static let chooseCodexButton = "Choose codex…"
    public static let codexPanelMessage = "Choose the codex executable"

    /// Under tcr's reason, when the codex a person picked is not something tcr can run.
    public static func refusedCodexChoice(shortPath: String) -> String {
        "\(shortPath) is not an executable file."
    }

    // MARK: - Help

    /// On `Codex account…` and on a Codex row's Re-login when this tcr has no
    /// `tcr login --codex`: those two run the Terminal script instead.
    public static let oldTcrHelp =
        "This tcr predates Codex sign-in, so this opens Terminal with the two commands. The panel lists the account on the next poll after they finish."
    public static let codexAccountHelp =
        "Signs in to ChatGPT through codex's own sign-in, run by tcr in a folder of its own: the codex CLI's login is not touched. The account is written only when the browser comes back."
    public static let importLinkHelp =
        "Shares a codex login through guarded auth.json token updates. A changed login is left untouched."
    /// The VoiceOver hint on a Codex row's Re-login button.
    public static let reloginHint = "Signs in to ChatGPT"
    public static let reloginHelp =
        "Signs in to ChatGPT in your browser, through codex's own sign-in run in a folder of tcr's own, requesting this exact account. tcr refuses to save if the browser hands back a different account or workspace. The codex CLI's own login is not touched."

    /// Add account…'s help on a Mac that shows signs of codex. `nil` without them, so
    /// the caller keeps the help it always had.
    public static func addAccountHelp(isPresent: Bool) -> String? {
        guard isPresent else { return nil }
        return
            "Adds a Claude account, or a Codex (ChatGPT) account for the codex CLI, signing in through your browser in this app. The account is written only when the browser comes back; an older tcr falls back to a Terminal window."
    }

    public static func reimportItem(shortPath: String) -> String {
        "Re-import from \(shortPath)…"
    }

    public static func reimportHelp(shortPath: String) -> String {
        "Take the login the codex CLI in \(shortPath) holds now. Refreshed tokens are shared through guarded auth.json updates; a changed login is left untouched."
    }

    // MARK: - A bundled tcr that cannot read a Codex config

    public static let predatesCodexTitle = "The tcr inside TcrBar predates Codex accounts"
    public static let predatesCodexDetail =
        "Your config holds a Codex account, which this tcr cannot read. Update TcrBar (Check for Updates…)."
    /// The title and detail as one sentence, for the places that show a single line: the
    /// server's summary, the footer, a row's failure line and what VoiceOver reads.
    public static let predatesCodexSentence = predatesCodexTitle + ". " + predatesCodexDetail

    // MARK: - Settings

    public static let settingsCodexLabel = "Codex"

    /// Whether the running proxy pools Codex, as it reported. Without a report yet, says
    /// so. A route that is not pooled gives tcr's reason, or its code when it sent no
    /// reason, and says only that it is not pooled when it sent neither.
    public static func settingsCodexValue(route: CodexRoute?) -> String {
        guard let route else { return "Unknown until the server answers" }
        guard route.pooled else {
            let why = [route.reason, route.code].compactMap { $0 }.first { !$0.isEmpty }
            return why.map { "Not pooled: \($0)" } ?? "Not pooled"
        }
        if let upstream = route.upstream, !upstream.isEmpty { return "Pooled through \(upstream)" }
        return "Pooled: tcr marks Codex connections to chatgpt.com"
    }

    /// Under Settings › Updates' command-line tcr, when that tcr cannot read a config
    /// holding a Codex account.
    public static func pathTcrFlag(path: String, version: String?) -> String {
        let named = version.map { "\(path) · \($0)" } ?? path
        return named + " — predates Codex accounts; update it before it next runs"
    }

    /// The caption on the four rotation rows that only Claude accounts follow.
    public static let rotationScope = "Claude accounts only"

    public static func groupMembers(claude: Int, codex: Int) -> String {
        "Members: \(claude) Claude, \(codex) Codex"
    }

    public static let groupReservedCodexHelp =
        "A Codex session cannot ask for a group, so while this group is reserved its Codex members serve nothing."

    private static func tail(_ bundledTcrWithoutCLI: String?) -> String {
        bundledTcrWithoutCLI.map { CodexBanner.noCLITail(bundledTcr: $0) } ?? ""
    }
}
