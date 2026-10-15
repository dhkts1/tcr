import Foundation

/// What a Codex account's card, line and help say that a Claude account's do not.
///
/// A Codex account is a ChatGPT login, and four facts about it have no Claude
/// counterpart, so each gets words of its own here:
///
///  - **Its windows.** A ChatGPT plan says which quota windows it has and how long
///    each one is (``Account/quotaWindows``, ``Account/sevenDayWindowMinutes``). A Pro
///    plan has no 5-hour window at all, so its card draws one weekly row, and a plan
///    that reports none draws one line saying so, never a row of nothing.
///  - **Its spend.** A ChatGPT plan is not billed per request and this build holds no
///    price for an OpenAI model, so a Codex tail counts output tokens. A dollar figure
///    leads only when every request in that bucket was priced by the operator's own
///    config. A tail never reads `$0.00`, which would claim the traffic was free.
///  - **Its refusals.** ChatGPT refuses an account for a reason it names (a usage limit,
///    or the workspace's spend limit) or answers it 403. That refusal lifts on what tcr
///    sees next: a probe that reads the account as allowed, a usage window that rolls, or
///    the end of its hold. Anthropic's rejection lifts only on Anthropic's own verdict. A
///    reserved group strands a Codex account outright, because a Codex session cannot
///    ask for a group.
///  - **Its sign-in.** A Codex login is signed out when its refresh token dies, and the
///    refresh error says why, so the card can say which login took it.
///
/// Every sentence names ChatGPT, because a fleet can hold both providers and the same
/// state has a different cause and a different remedy on each. The Claude sentence a
/// refused row used to borrow ("Anthropic has rejected this account") sent an operator
/// looking for an Anthropic verdict that never happened.
///
/// Every function answers `nil`, or the Claude row's own text, on a Claude row, so no
/// Claude card, line or help changes. The canonical rows these words are tested on are
/// `apps/macos/Tests/Fixtures/codex-status.json`.
public enum CodexCardFacts {

    /// The minimum space the card reserves for quota rows; no extra window is drawn.
    public static func reservedQuotaRows(_ account: Account) -> Int {
        account.provider == .codex ? 2 : 0
    }

    // MARK: - The state pill's help

    /// The sentence behind a Codex row's state pill, or `nil` wherever the row keeps
    /// ``Account/stateHelp``, a Claude row included.
    ///
    /// - REFUSED: ChatGPT's reason, and when tcr tries the account again where that is
    ///   known. A 403 says so; a hold with a future end says until when; otherwise the
    ///   refusal stands until a probe clears it.
    /// - UNREACHABLE: the reserved groups that strand it, and the two ways out.
    /// - SPENT, held on a ChatGPT window, and only when ChatGPT named the limit or the
    ///   account holds credits: when it returns, and the credits tcr does not spend.
    ///   Without either, the pill's word and the red bar already say it all.
    /// - NEEDS RE-LOGIN with a known refresh error: the same sentence the row's note
    ///   says, so hover and the note agree.
    ///
    /// `timeZone` is the clock a hold's end is printed in; `home` shortens a path under
    /// it to `~`.
    public static func stateHelp(
        _ account: Account, now: Date, timeZone: TimeZone = .current,
        home: String = HomePath.current
    ) -> String? {
        guard account.provider == .codex else { return nil }
        switch FleetTally.Kind(account: account) {
        case .rejected:
            return refusedHelp(account, now: now, timeZone: timeZone)
        case .unreachable:
            return unreachableHelp(account)
        case .spent:
            return spentHelp(account, now: now)
        case .needsRelogin:
            guard let loginError = account.loginError, !loginError.isEmpty else { return nil }
            return signedOutSentence(loginError, codexHome: account.codexHome, home: home)
        case .ok, .near, .unknown, .unmeasured, .disabled:
            return nil
        }
    }

    /// A refused row, by its cause: ChatGPT's 403, a hold with a known end, or a
    /// refusal that stands until a probe clears it.
    private static func refusedHelp(_ account: Account, now: Date, timeZone: TimeZone)
        -> String?
    {
        if account.isForbiddenHold {
            // The server always pairs this gate with the instant it lifts. Without one
            // there is no honest sentence to say, and the pill keeps the row's own help.
            guard let freeAtMs = account.freeAtMs else { return nil }
            var sentence =
                "ChatGPT answered this account's last request with 403 Forbidden. tcr holds "
                + "it until \(clock(freeAtMs, timeZone: timeZone))"
            // Under 30 seconds the countdown has no figure, and the clock says it all.
            if let caption = QuotaFormat.resetCaption(resetAtMs: freeAtMs, now: now) {
                sentence += ", \(caption)"
            }
            return sentence + ", then tries it again."
        }
        let why = refusalReason(account)
        if !account.held.isEmpty {
            return
                "ChatGPT refused this account's last request\(why). A usage window is also full. Check the reported limit or workspace access."
        }
        if let freeAtMs = account.freeAtMs, let minutes = wholeMinutes(until: freeAtMs, now: now) {
            return "ChatGPT refused this account's last request\(why) though no usage window is "
                + "full. tcr holds it until \(clock(freeAtMs, timeZone: timeZone)), in "
                + "\(HeldWindow.duration(minutes: minutes))."
        }
        return "ChatGPT refused this account's last request\(why), and no usage window is "
            + "full. tcr may still send it a request, and another refusal marks it again; a "
            + "probe that reads it as allowed, or sees one of its usage windows roll, clears "
            + "the refusal."
    }

    /// ` (the workspace's spend limit is reached)`, ` (limit: rate_limit_reached)`, or
    /// nothing: ChatGPT's own reason, verbatim, when it gave one.
    private static func refusalReason(_ account: Account) -> String {
        if account.spendControlReached { return " (the workspace's spend limit is reached)" }
        if let type = reachedType(account) { return " (limit: \(type))" }
        return ""
    }

    private static func unreachableHelp(_ account: Account) -> String? {
        let reserved = reservedGroups(account)
        guard !reserved.isEmpty else { return nil }
        let groups = spokenList(reserved)
        let head =
            "Reserved for \(groups), and a Codex session cannot ask for a group, so no Codex "
            + "request can reach this account."
        if reserved.count == 1 {
            return head + " Remove it from \(groups), or unreserve \(groups)."
        }
        return head + " Remove it from each of \(groups), or unreserve them."
    }

    /// The row's reserved groups, in the order the server lists them, naming only the
    /// groups the row is in.
    private static func reservedGroups(_ account: Account) -> [String] {
        let groups = account.groups ?? []
        var names: [String] = []
        for name in account.reservedGroups ?? [] where groups.contains(name) {
            if !names.contains(name) { names.append(name) }
        }
        return names
    }

    /// `research`, `research and orchard`, `a, b and c`.
    private static func spokenList(_ names: [String]) -> String {
        guard let last = names.last else { return "" }
        guard names.count > 1 else { return last }
        return names.dropLast().joined(separator: ", ") + " and " + last
    }

    private static func spentHelp(_ account: Account, now: Date) -> String? {
        let slot: Account.QuotaWindow
        switch account.gate {
        case .unknown("seven-day")?: slot = .sevenDay
        case .unknown("five-hour")?: slot = .fiveHour
        default: return nil
        }
        let credits = creditsSentence(account)
        let type = reachedType(account)
        guard credits != nil || type != nil else { return nil }
        let label = account.windowLabel(for: slot)
        let held = account.held.first { $0.window == label && $0.minutesUntilReset > 0 }
        guard
            let minutes = held?.minutesUntilReset
                ?? account.freeAtMs.flatMap({ wholeMinutes(until: $0, now: now) })
        else { return nil }
        var sentence = "ChatGPT's \(windowWord(label)) limit is reached"
        if let type { sentence += " (limit: \(type))" }
        sentence += "; it returns in \(HeldWindow.duration(minutes: minutes))."
        return sentence + (credits ?? "")
    }

    /// `weekly`, `5-hour`, or the label itself for a window that is neither.
    private static func windowWord(_ label: String) -> String {
        switch label {
        case "7d": return "weekly"
        case "5h": return "5-hour"
        default: return label
        }
    }

    /// The credits clause a spent row's help ends with, by ``creditsText(_:)``'s rule.
    private static func creditsSentence(_ account: Account) -> String? {
        guard let credits = account.credits, creditsText(account) != nil else { return nil }
        if credits.unlimited {
            return " This account also has unlimited credits, which tcr does not spend."
        }
        return " This account also holds \(credits.balance ?? "") credits, which tcr does not "
            + "spend."
    }

    private static func reachedType(_ account: Account) -> String? {
        guard let type = account.limitReachedType, !type.isEmpty else { return nil }
        return type
    }

    /// `00:47`: the hold's end on the clock, in `timeZone`.
    private static func clock(_ ms: Int64, timeZone: TimeZone) -> String {
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm"
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = timeZone
        return formatter.string(from: Date(timeIntervalSince1970: Double(ms) / 1000))
    }

    /// The whole minutes until `ms`, at least one, or `nil` when it is not ahead. At
    /// least one, so a hold in its last seconds reads `in 1m` and never `in now`.
    private static func wholeMinutes(until ms: Int64, now: Date) -> Int? {
        let seconds = Double(ms) / 1000 - now.timeIntervalSince1970
        guard seconds > 0 else { return nil }
        return max(1, Int(seconds / 60))
    }

    /// What a Codex account's one-line row speaks as its value: the sentence its
    /// card's pill would show on hover, since a line draws no pill. `nil` on a
    /// signed-out row, whose sentence is the notice right under its line, and on
    /// every row ``stateHelp(_:now:timeZone:home:)`` has nothing for.
    public static func lineValue(
        _ account: Account, now: Date, timeZone: TimeZone = .current,
        home: String = HomePath.current
    ) -> String? {
        guard account.health != .needsRelogin else { return nil }
        return stateHelp(account, now: now, timeZone: timeZone, home: home)
    }

    // MARK: - Credits, the no-windows line and the name's help

    /// `credits unlimited`, `credits 12.50`, or `nil` when the account holds none, or
    /// is not a Codex account.
    public static func creditsText(_ account: Account) -> String? {
        guard account.provider == .codex, let credits = account.credits else { return nil }
        if credits.unlimited { return "credits unlimited" }
        if credits.hasCredits, let balance = credits.balance, !balance.isEmpty {
            return "credits \(balance)"
        }
        return nil
    }

    /// The one mute line a Codex card draws in place of its rows when the plan reports
    /// no usage window at all, with its credits, and its help. `nil` on every other row.
    public static func noWindowsLine(_ account: Account) -> (text: String, help: String)? {
        guard account.provider == .codex, account.quotaWindows == [] else { return nil }
        var text = "No usage windows reported"
        if let credits = creditsText(account) { text += " · \(credits)" }
        var help = "This plan reported no usage window on the last successful probe."
        if let usage = account.usage {
            help += " \(QuotaFormat.tokens(usage.today.outputTokens)) output tokens today."
        }
        return (text, help)
    }

    /// `bob@example.com/codex · ChatGPT workspace 55555555`: a Codex row's name, with the
    /// ChatGPT workspace it signs in to, because two logins of one email can serve two
    /// workspaces. The name alone on a Claude row, and on a Codex row with no workspace.
    public static func nameHelp(_ account: Account) -> String {
        guard account.provider == .codex, let org = account.orgUuid else { return account.name }
        let workspace = String(org.filter(\.isHexDigit).prefix(8))
        guard !workspace.isEmpty else { return account.name }
        return "\(account.name) · ChatGPT workspace \(workspace)"
    }

    // MARK: - The tails

    /// A Codex row's tail on one window's row, and its help: that window's output
    /// tokens, or today's when the server did not send the window's own bucket, with a
    /// help that says which.
    ///
    /// `${cost} · ` leads only when the operator's prices covered every request in the
    /// bucket. Otherwise the cost is not a measurement of anything: a bucket nobody
    /// could price has no cost, and an idle one is not "free". `nil` without `usage`,
    /// and on a Claude row.
    public static func tail(_ account: Account, window: Account.QuotaWindow)
        -> (text: String, help: String)?
    {
        guard account.provider == .codex, let usage = account.usage else { return nil }
        let own = window == .fiveHour ? usage.window : usage.sevenDayWindow
        let bucket = own ?? usage.today
        let span = own == nil ? "today" : spanPhrase(account.windowLabel(for: window))
        let tokens = QuotaFormat.tokens(bucket.outputTokens)
        let reasoning =
            bucket.reasoningTokens > 0
            ? " (\(QuotaFormat.tokens(bucket.reasoningTokens)) reasoning)" : ""
        let lead = pricedLead(bucket)
        return (lead + tokens, lead + "\(tokens) output tokens \(span)\(reasoning)")
    }

    /// `in this 5-hour window`, `this week`, or `in this 30d window`.
    private static func spanPhrase(_ label: String) -> String {
        switch label {
        case "5h": return "in this 5-hour window"
        case "7d": return "this week"
        default: return "in this \(label) window"
        }
    }

    /// The dollar figure a fully priced bucket leads with, or nothing. A priced total
    /// under half a cent prints as `$0.00`, which no Codex surface draws (the rule
    /// ``FleetSection/todaySpend`` follows for a Codex box), so that tail keeps its
    /// tokens alone.
    private static func pricedLead(_ bucket: UsageTotals) -> String {
        guard bucket.requests > 0, bucket.unpricedRequests == 0, let cost = bucket.costUsd
        else { return "" }
        let dollars = QuotaFormat.usd(cost)
        guard dollars != "$0.00" else { return "" }
        return dollars + " · "
    }

    // MARK: - The note under a row

    /// The one note a Codex row carries, by priority: a re-login this app run tried and
    /// tcr refused, then why the account is signed out, then a warning `tcr group add`
    /// gave this app run. `nil` when there is nothing to say: an absent reason is never
    /// drawn, so a signed-out row from a server that sends no refresh error has no note.
    ///
    /// The warning arrives without its `warning: ` prefix.
    public static func noteLine(
        reloginRefusal: String?, loginError: String?, codexHome: String?,
        groupAddWarning: String?, home: String = HomePath.current
    ) -> CodexNoteLine? {
        if let reloginRefusal, !reloginRefusal.isEmpty {
            return CodexNoteLine(text: "Re-login refused: " + reloginRefusal, tone: .spent)
        }
        if let loginError, !loginError.isEmpty {
            return CodexNoteLine(
                text: signedOutSentence(loginError, codexHome: codexHome, home: home),
                tone: .spent)
        }
        if let groupAddWarning, !groupAddWarning.isEmpty {
            return CodexNoteLine(text: groupAddWarning, tone: .caution)
        }
        return nil
    }

    /// Why a Codex login is signed out, from the refresh error that signed it out.
    ///
    /// A login imported from the codex CLI shares its refresh token with that CLI, and
    /// whichever refreshes first leaves the other's token dead. That is the commonest
    /// cause, and the one whose remedy differs: a re-login gives tcr a login of its own.
    private static func signedOutSentence(_ loginError: String, codexHome: String?, home: String)
        -> String
    {
        switch loginError {
        case "refresh_token_reused":
            if let codexHome, !codexHome.isEmpty {
                return "Signed out: the codex CLI in \(HomePath.short(codexHome, home: home)) "
                    + "refreshed this shared login first. Re-login gives tcr its own."
            }
            return "Signed out: another copy of this login refreshed first. Re-login to sign it "
                + "in again."
        case "refresh_token_expired":
            return "Signed out: the login expired. Re-login to sign it in again."
        case "refresh_token_invalidated":
            return "Signed out: the login was revoked. Re-login to sign it in again."
        case "invalid_grant":
            return "Signed out: ChatGPT rejected the refresh (invalid_grant). Re-login to sign it "
                + "in again."
        case "http_401":
            return "Signed out: ChatGPT answered the refresh with 401. Re-login to sign it in "
                + "again."
        default:
            return "Signed out: ChatGPT refused the refresh (\(loginError)). Re-login to sign it "
                + "in again."
        }
    }
}

/// The note under a Codex row: its sentence, and whether it reports a state the account
/// is in (`spent`, drawn red) or a warning about a group (`near`).
public struct CodexNoteLine: Equatable, Sendable {
    public enum Tone: Equatable, Sendable {
        /// A state the account is in: signed out, or a re-login tcr refused.
        case spent
        /// A caution about how the account is set up: a group no Codex request can reach.
        case caution
    }

    public let text: String
    public let tone: Tone

    public init(text: String, tone: Tone) {
        self.text = text
        self.tone = tone
    }
}

extension AccountRowNotice {
    /// A Codex row's note as one more line under the row. A signed-out or refused
    /// re-login is a state the account is in, so it is an alert, red and spoken as
    /// written; a group warning is a caution, which is not a quota state.
    public init(codexNote: CodexNoteLine) {
        switch codexNote.tone {
        case .spent: self.init(text: codexNote.text, tone: .alert)
        case .caution: self.init(text: codexNote.text, tone: .caution)
        }
    }
}

/// The home folder a path is shortened against, so a note says `~/.codex` rather than
/// a full path that names this Mac's user.
///
/// It never reads a file: it is the home folder's path, and nothing under it is opened,
/// `~/.codex` and every credential included. The render harness sets ``current`` to the
/// fixtures' `/Users/example`, so a scene reads the same on every Mac.
public enum HomePath {
    public nonisolated(unsafe) static var current: String =
        FileManager.default.homeDirectoryForCurrentUser.path

    /// `/Users/example/.codex` → `~/.codex`; the home itself → `~`. A path outside the
    /// home comes back unchanged, and so does one that only shares its first letters
    /// (`/Users/example2`): the prefix stops at a path boundary.
    public static func short(_ path: String, home: String = current) -> String {
        guard !home.isEmpty else { return path }
        if path == home { return "~" }
        let prefix = home.hasSuffix("/") ? home : home + "/"
        guard path.hasPrefix(prefix) else { return path }
        return "~/" + path.dropFirst(prefix.count)
    }
}

/// The legacy panel's account strings (`TCRBAR_LEGACY_PANEL=1`), for a Claude row and
/// for a Codex row.
///
/// The legacy row wrote these as Anthropic's: the plan "as Anthropic reports it", a
/// pill reading `rejected` with Anthropic's verdict behind it, a spoken "rejected by
/// Anthropic". On a Codex row each of those is false, and a refused ChatGPT login would
/// send an operator looking for an Anthropic verdict that never happened. Each function
/// returns the legacy row's own words on a Claude row, unchanged, and ChatGPT's words
/// on a Codex row, the same ones the v4 card uses.
public enum LegacyRowCopy {
    /// The plan label's help. On a Codex row, the name with its ChatGPT workspace.
    public static func planHelp(_ account: Account) -> String {
        guard account.provider == .codex else {
            return "This account's plan, as Anthropic reports it for its organization."
        }
        return CodexCardFacts.nameHelp(account)
    }

    /// The organization tag's help. On a Codex row, the name with its ChatGPT workspace.
    public static func orgTagHelp(_ account: Account) -> String {
        guard account.provider == .codex else {
            return "The organization half of this account's name — the full name is "
                + "\(account.name), which is what every `tcr` command takes."
        }
        return CodexCardFacts.nameHelp(account)
    }

    /// The `rotating` pill's help. A Codex row keeps the first sentence only: the
    /// second is about Anthropic's own gate, which a Codex row never meets.
    public static func rotatingHelp(_ account: Account) -> String {
        let rotating =
            "In the rotation — this account can be picked for traffic, as far as this build "
            + "can tell."
        guard account.provider == .codex else {
            return rotating
                + " A quota rejection from Anthropic can also exclude an account without "
                + "changing its status; TcrBar cannot see that gate yet."
        }
        return rotating
    }

    /// The pill a rejected or refused row draws: `rejected` on a Claude row, `refused`
    /// on a Codex row.
    public static func rejectedPillWord(_ account: Account) -> String {
        account.provider == .codex ? "refused" : "rejected"
    }

    /// That pill's help: Anthropic's verdict on a Claude row; ChatGPT's reason and the
    /// hold's end on a Codex row (``CodexCardFacts/stateHelp(_:now:timeZone:home:)``),
    /// or `nil` where there is no honest sentence to say.
    public static func rejectedPillHelp(_ account: Account, now: Date) -> String? {
        guard account.provider == .codex else {
            return "Anthropic has rejected this account, so `tcr` sends it no traffic whatever "
                + "its quota reads. It returns on their verdict, not on a reset."
        }
        return CodexCardFacts.stateHelp(account, now: now)
    }

    /// The spoken phrase for that state. A Codex row held after a 403 says how long
    /// the hold has left, which is when it serves again, and says no figure in the
    /// hold's last seconds, when the countdown has none.
    public static func rejectedSpoken(_ account: Account, now: Date) -> String {
        guard account.provider == .codex else { return "rejected by Anthropic, not in rotation" }
        if account.isForbiddenHold {
            guard let caption = QuotaFormat.resetCaption(resetAtMs: account.freeAtMs, now: now)
            else { return "refused by ChatGPT, held" }
            return "refused by ChatGPT, held \(caption)"
        }
        return "refused by ChatGPT, not in rotation"
    }
}
