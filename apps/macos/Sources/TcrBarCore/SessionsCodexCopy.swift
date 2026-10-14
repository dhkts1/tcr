import Foundation

/// What the Sessions and Tools tabs say once the fleet holds a Codex account.
///
/// Every line keys on the fleet's accounts (``Fleet/holdsCodex``, ``Fleet/holdsAnthropic``)
/// or on a session's ``Session/provider``. A Claude-only fleet gets back exactly the words the
/// two tabs printed before Codex accounts existed; those words live here beside the Codex ones,
/// so each line's two arms are written, and tested, side by side. A fleet with no account at
/// all is Claude-only too.
///
/// Lowercase codex names the program and the files it writes; capitalized Codex names the
/// provider, and what tcr counts by it: its accounts, its requests and its sessions.
///
/// Lives in `TcrBarCore`, not in the view, because the test target links this module only,
/// and a sentence a view composes privately is one no assertion can read.
public enum SessionsCodexCopy {
    // MARK: Sessions

    /// The Sessions tab's footer: where its rows come from. The proxy names every session; the
    /// project beside a Claude session comes from Claude Code's session file, and the project
    /// beside a Codex session from the first line of codex's rollout file.
    public static func sessionsFooter(_ fleet: Fleet) -> String {
        switch pools(fleet) {
        case .claudeOnly: return "proxy + Claude Code session files"
        case .mixed: return "proxy + Claude Code and codex session files"
        case .codexOnly: return "proxy + codex session files"
        }
    }

    /// The empty Sessions tab's detail, under its "No live sessions" title. It names what
    /// could have spoken to the proxy, so a Codex user is not told only about Claude Code.
    public static func emptyDetail(_ fleet: Fleet) -> String {
        switch pools(fleet) {
        case .claudeOnly: return "Nothing has spoken to the proxy in the last hour."
        case .mixed:
            return "No Claude Code or Codex session has spoken to the proxy in the last hour."
        case .codexOnly: return "No Codex session has spoken to the proxy in the last hour."
        }
    }

    /// The line for a proxy too old to record Codex sessions, or `nil` when there is nothing
    /// to explain.
    ///
    /// Such a proxy serves Codex and lists no Codex session at all, so the tab would read as
    /// if codex had been silent. The line shows only when that cannot be true: the fleet holds
    /// a Codex account, its Codex rows served requests in the last hour, and not one session
    /// carries the Codex provider. An empty list counts, because the old proxy records none.
    /// One Codex session on the list proves the proxy records them, and with no Codex traffic
    /// in the hour an empty list is simply the truth.
    public static func oldServerLine(_ fleet: Fleet) -> String? {
        guard fleet.holdsCodex, !fleet.sessions.contains(where: { $0.provider == .codex })
        else { return nil }
        let requests = fleet.accounts
            .filter { $0.provider == .codex }
            .reduce(0) { $0 + ($1.usage?.lastHour.requests ?? 0) }
        guard requests > 0 else { return nil }
        let noun = requests == 1 ? "request" : "requests"
        return "Codex served \(QuotaFormat.count(requests)) \(noun) in the last hour, "
            + "but this proxy predates Codex sessions — restart it on this build to see them."
    }

    /// What leads a session's subtitle: `"Codex"` for a Codex session in a fleet that holds
    /// both providers, where a row has to say which one it is. A Codex-only fleet needs no
    /// lead, and a Claude session never gets one, so its subtitle stays as it was.
    public static func subtitleLead(for session: Session, in fleet: Fleet) -> String? {
        guard session.provider == .codex, pools(fleet) == .mixed else { return nil }
        return "Codex"
    }

    /// A Codex session's trailing text while none of its calls runs: how long ago it was last
    /// seen, `"2m ago"`, and `"now"` under a minute. Never "idle" or "waiting on you": tcr
    /// cannot see what codex is doing between requests, and does not guess.
    public static func trailing(age: String) -> String {
        age == "now" ? age : "\(age) ago"
    }

    /// The Sessions summary's numbers line, one clause group per pool, or `nil` for a fleet
    /// with no Codex account, which keeps the line it always had.
    ///
    /// The Claude group is figured over the Claude accounts and sessions alone, so no Codex
    /// request is ever counted as an unpriced Claude one: its burn rate, today's spend, the
    /// live sessions' spend, each when present, and the account ratio, always. The Codex group
    /// is told in tokens, today's output tokens over the Codex rows (left out when no Codex
    /// row was measured), then its account ratio, left out while no session carries the Codex
    /// provider: a proxy too old to record Codex sessions cannot say how many accounts are in
    /// use, and absent is not zero. Each group's first clause carries its pool's name. The
    /// fleet's tool errors close the line when there are any.
    public static func summaryClauses(_ fleet: Fleet) -> [String]? {
        guard fleet.holdsCodex else { return nil }
        var clauses: [String] = []
        if fleet.holdsAnthropic {
            let claude = pool(.anthropic, of: fleet)
            var group: [String] = []
            if let hourly = claude.burnRateSegment { group.append(hourly) }
            if let today = claude.todayCost { group.append("\(QuotaFormat.usd(today)) today") }
            if let live = claude.sessionsLiveCost { group.append("\(QuotaFormat.usd(live)) live") }
            group.append(accountRatio(claude))
            clauses += named("Claude", group)
        }
        let codex = pool(.codex, of: fleet)
        var group: [String] = []
        if let tokens = fleet.todayOutputTokens(of: .codex) {
            group.append("\(QuotaFormat.tokens(tokens)) tokens")
        }
        if !codex.sessions.isEmpty { group.append(accountRatio(codex)) }
        clauses += named("Codex", group)
        if fleet.toolsTotalErrors > 0 { clauses.append("\(fleet.toolsTotalErrors) err") }
        return clauses
    }

    // MARK: Tools

    /// The RUNNING NOW head's unit. While any running call is codex's, a ring may fill toward
    /// that call's own timeout rather than the Bash tool's 600 s, so the head stops naming one
    /// figure for every ring.
    public static func runningUnit(count: Int, anyCodex: Bool) -> String {
        if anyCodex { return "\(count) · ring fills toward each call's timeout" }
        return "\(count) · ring fills toward the \(Int(ToolCall.bashTimeoutSeconds))s timeout"
    }

    /// The Tools tab's footer. tcr reads a Claude call from the request that carries it, and a
    /// codex call from the response that announces it, so with a Codex account the tab is
    /// built from both.
    public static func toolsFooter(holdsCodex: Bool) -> String {
        holdsCodex
            ? "from request and response bodies · nothing logged"
            : "from request bodies only · nothing logged"
    }

    /// What a running Codex call's row shows where a Claude call's ✕ goes. tcr cannot match a
    /// codex call to a process, so it never offers to kill one; the words say so in the slot
    /// rather than leave a gap that reads as a missing button.
    public static let noKill = "no Kill"

    /// The help and the VoiceOver label of ``noKill``.
    public static let noKillHelp =
        "tcr cannot match this call to a codex process, so it offers no Kill."

    // MARK: Pools

    private enum Pools {
        case claudeOnly, mixed, codexOnly
    }

    private static func pools(_ fleet: Fleet) -> Pools {
        guard fleet.holdsCodex else { return .claudeOnly }
        return fleet.holdsAnthropic ? .mixed : .codexOnly
    }

    /// One provider's accounts and sessions, as a fleet of their own, on the same sessions
    /// channel, so every whole-fleet figure reads over that pool alone.
    private static func pool(_ provider: Provider, of fleet: Fleet) -> Fleet {
        Fleet(
            accounts: fleet.accounts.filter { $0.provider == provider },
            unreadable: [],
            sessions: fleet.sessions.filter { $0.provider == provider },
            sessionsChannel: fleet.sessionsChannel)
    }

    /// `"1/2 accounts"`: the accounts the pool's live sessions run on, over the pool's
    /// accounts.
    private static func accountRatio(_ pool: Fleet) -> String {
        "\(pool.sessionsDistinctAccountCount)/\(pool.accounts.count) accounts"
    }

    /// `group` with the pool's name before its first clause, `"Claude $3.10/hr"`, or nothing
    /// for an empty group.
    private static func named(_ name: String, _ group: [String]) -> [String] {
        guard let first = group.first else { return [] }
        return ["\(name) \(first)"] + group.dropFirst()
    }
}
