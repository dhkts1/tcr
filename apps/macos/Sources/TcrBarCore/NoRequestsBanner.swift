import Foundation

/// The Accounts tab's no-requests banner: a colleague installed tcr, added a
/// shared account, and Claude Code kept answering from its own keychain
/// account, and the added account sat at zero requests with no line on the
/// panel that said why. This is the pure half: given what the caller already
/// measured, whether to show the banner and what it says. Reading the live
/// facts (the fleet, the clock, the process table) is the view's job, not
/// this one's.
public enum NoRequestsBanner {
    /// How long the fleet must have read live, with every account at zero
    /// requests, before the banner appears. Long enough that a proxy's first
    /// request (DNS, TLS, the account picker) is not mistaken for silence;
    /// short enough that whoever is looking at the panel is still there.
    public static let quietWindow: TimeInterval = 300

    /// Every Claude account's requests served, summed, or `nil` when this read
    /// is not live. An offline read's `requests` is `nil` per account (a fresh
    /// `Manager` has nothing to report, `FleetStatus.swift`'s own
    /// doc-comment), and summing `nil` as `0` would draw this banner on an
    /// offline proxy, which has served nothing because it is not running, a
    /// different fact from "running and unreached".
    ///
    /// The banner is about where Claude Code is routed, so only Claude rows
    /// count: a Codex row's requests come from the codex CLI and say nothing
    /// about Claude. In a fleet holding a Codex account it is also `nil` unless
    /// an enabled Claude account exists, so a Mac that pools Codex alone, or
    /// keeps its Claude accounts switched off, is not nagged about Claude. A
    /// Claude-only fleet keeps today's banner: every row is a Claude row, so
    /// the sum is today's, and it never asked whether an account was enabled.
    /// So a live Claude-only fleet whose accounts are all disabled still draws
    /// it, and rightly: a claude routed here was refused, not served, and
    /// `requests` counts served requests only.
    public static func totalRequests(_ fleet: Fleet) -> Int? {
        guard fleet.source == .live else { return nil }
        let claude = fleet.accounts.filter { $0.provider == .anthropic }
        if fleet.holdsCodex, !claude.contains(where: { !$0.disabled }) { return nil }
        return claude.reduce(0) { $0 + ($1.requests ?? 0) }
    }

    /// The banner's one line, or `nil` when it must not show.
    ///
    /// `claudeCount` is the running-process count the caller read off
    /// `ProcessTable.read()`, the same probe the Tools tab already uses to
    /// match a running Bash call. `nil` when the caller has no such probe at
    /// all, which the gate treats as "unknown" and drops rather than as zero:
    /// a Mac with Claude idle for the moment must not read the same as a Mac
    /// with no Claude session anywhere.
    ///
    /// `namesClaude` is the caller's "this fleet holds a Codex account". The
    /// proxy then does serve requests, the codex CLI's, so "No request" would
    /// be false and the sentence says "No Claude request". Without it the
    /// sentence is the one a Claude-only fleet has always drawn.
    public static func text(
        liveFor: TimeInterval,
        claudeCount: Int?,
        route: ClaudeRouteRead.Route,
        namesClaude: Bool = false
    ) -> String? {
        guard liveFor >= quietWindow else { return nil }
        if let claudeCount, claudeCount == 0 { return nil }
        let noRequest = namesClaude ? "No Claude request" : "No request"
        return
            "\(noRequest) has reached this proxy since it started. Claude is routed to \(route.url) (\(route.source))."
    }
}
