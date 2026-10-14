import Foundation

/// The one Codex line the Accounts tab may draw under the summary, beside the Claude one.
///
/// This is the pure half, as ``NoRequestsBanner`` is for Claude: given what the panel already
/// measured (the fleet, the codex processes running, how long the Codex accounts have been
/// quiet, whether this Mac shares its accounts), which line shows and what it says. Reading
/// those facts is the view's job.
///
/// Only one Codex line shows, so the panel never stacks four amber sentences above its tabs,
/// and the precedence is the order an operator can act on: a route that is not pooled explains
/// everything below it, then refusals tcr answered itself, then requests that ran on their own
/// login, then codex running with nothing reaching tcr. The Claude line is independent: each
/// provider can show one.
///
/// A line needs a live read and a Codex account that can serve (``codexRequests(_:)``): an
/// offline proxy has refused nothing, and a pool with every account switched off or parked
/// is not in use.
public enum CodexBanner: Equatable, Sendable {
    /// The proxy cannot pool codex. The reason is the route's own words, or its code when it
    /// gives no words.
    case notPooled(reason: String)
    /// tcr itself answered Codex requests with a refusal in the last ten minutes.
    ///
    /// The ten-minute ring lives in the proxy and starts empty when it restarts, so the line
    /// clears then while codex may still be failing; it returns with the next refusal.
    case refused(count: Int, last: LastRefusal, peerSharing: Bool)
    /// Codex requests in the last ten minutes carried a routing header, so tcr relayed them on
    /// the client's own login rather than pooling them.
    case unpooled(count: Int)
    /// codex is running and no Codex request has reached tcr for ``quietWindow``.
    ///
    /// Every input must be measured: the process count, how long the Codex accounts have been
    /// at zero requests, a pooled route and an empty unpooled count. A reading that is missing
    /// draws nothing; a miss means no line, never a false one.
    case noRequests(processes: Int, bundledTcrWithoutCLI: String?)

    /// What the last refusal told codex, from its error type.
    public enum LastRefusal: Equatable, Sendable {
        /// `usage_limit_reached`: codex was told its usage limit is reached.
        case usageLimit
        /// `no_codex_account`: the pool held no Codex account at all.
        case noAccount
        /// Any other type, or none: codex was asked to retry.
        case other

        init(_ type: String?) {
            switch type {
            case "usage_limit_reached": self = .usageLimit
            case "no_codex_account": self = .noAccount
            default: self = .other
            }
        }
    }

    /// The line's colour: amber for a line that asks for action, dim for the one that only
    /// reports.
    public enum Tint: Equatable, Sendable {
        case near
        case dim
    }

    /// How long the Codex accounts must have been at zero requests: the Claude line's window.
    public static let quietWindow = NoRequestsBanner.quietWindow

    public var tint: Tint {
        if case .unpooled = self { return .dim }
        return .near
    }

    /// The sentence, in its own form for one.
    public var text: String {
        switch self {
        case .notPooled(let reason):
            return "Codex is not pooled: \(reason)"
        case .refused(let count, let last, let peerSharing):
            let sentence = Self.refusedSentence(count: count, last: last)
            return peerSharing ? sentence + " Trusted Macs cannot serve Codex requests." : sentence
        case .unpooled(let count):
            return count == 1
                ? "1 Codex request in the last 10 min ran on its own login: its ChatGPT workspace is FedRAMP or residency-routed, which tcr does not pool."
                : "\(count) Codex requests in the last 10 min ran on their own login: their ChatGPT workspace is FedRAMP or residency-routed, which tcr does not pool."
        case .noRequests(let processes, let bundledTcrWithoutCLI):
            let sentence =
                processes == 1
                ? "1 codex process is running and no Codex request has reached tcr in 5 minutes — start codex with `tcr run --codex`."
                : "\(processes) codex processes are running and no Codex request has reached tcr in 5 minutes — start codex with `tcr run --codex`."
            guard let bundledTcrWithoutCLI else { return sentence }
            return sentence + Self.noCLITail(bundledTcr: bundledTcrWithoutCLI)
        }
    }

    private static func refusedSentence(count: Int, last: LastRefusal) -> String {
        switch (count == 1, last) {
        case (true, .usageLimit):
            return "tcr refused 1 Codex request in the last 10 min and told codex its usage limit is reached."
        case (true, .noAccount):
            return "tcr refused 1 Codex request in the last 10 min: no Codex account was in the pool."
        case (true, .other):
            return
                "tcr refused 1 Codex request in the last 10 min and asked codex to retry: no Codex account could serve it."
        case (false, .usageLimit):
            return
                "tcr refused \(count) Codex requests in the last 10 min; the last one told codex its usage limit is reached."
        case (false, .noAccount):
            return
                "tcr refused \(count) Codex requests in the last 10 min; the last one found no Codex account in the pool."
        case (false, .other):
            return
                "tcr refused \(count) Codex requests in the last 10 min; the last one asked codex to retry: no Codex account could serve it."
        }
    }

    /// The sentence that follows an instruction to run `tcr run --codex` when this Mac has no
    /// command-line tcr, only the one inside TcrBar, with its leading space. `bundledTcr` is that
    /// tcr's path, quoted for a shell where it needs to be. The one place the sentence lives:
    /// the sign-in sheet's details append it too.
    public static func noCLITail(bundledTcr: String) -> String {
        " TcrBar found no tcr besides its own: install the command-line tcr (README › Install), or start codex with `\(GroupCommand.shellQuote(bundledTcr)) run --codex`, the tcr inside TcrBar, which keeps TcrBar from updating until that codex exits."
    }

    /// The requests served by every Codex account that can serve (one whose
    /// ``FleetTally/Kind`` is not `.disabled`), summed: the count the quiet clock watches.
    /// `nil` unless the read is live and at least one such account exists, which is also
    /// when no Codex line may show at all.
    public static func codexRequests(_ fleet: Fleet) -> Int? {
        guard fleet.source == .live else { return nil }
        let serving = fleet.accounts.filter {
            $0.provider == .codex && FleetTally.Kind(account: $0) != .disabled
        }
        guard !serving.isEmpty else { return nil }
        return serving.reduce(0) { $0 + ($1.requests ?? 0) }
    }

    /// The line to draw, or `nil` for none: the first of the four that holds, in precedence.
    ///
    /// Every Codex row carries the same route and traffic (the server sends one reading,
    /// copied onto each), so the first Codex row's reading speaks for all. `codexProcesses` is
    /// the running codex sessions, `codexQuietFor` how long the serving Codex accounts have
    /// been at zero requests, and `bundledTcrWithoutCLI` the tcr inside TcrBar when this Mac
    /// has no other; each is `nil` while unknown.
    public static func current(
        fleet: Fleet, codexProcesses: Int?, codexQuietFor: TimeInterval?, peerSharing: Bool,
        bundledTcrWithoutCLI: String? = nil
    ) -> CodexBanner? {
        guard let requests = codexRequests(fleet) else { return nil }
        let codex = fleet.accounts.filter { $0.provider == .codex }

        // A route with neither words nor a code says nothing an operator can act on, so the
        // line is skipped and precedence moves on.
        if let route = codex.first(where: { $0.codexRoute?.pooled == false })?.codexRoute,
            let reason = [route.reason, route.code].compactMap({ $0 }).first(where: { !$0.isEmpty })
        {
            return .notPooled(reason: reason)
        }

        let traffic = codex.first?.codexTraffic
        if let traffic, traffic.refused10m > 0 {
            return .refused(
                count: traffic.refused10m, last: LastRefusal(traffic.lastRefusal),
                peerSharing: peerSharing)
        }
        if let traffic, traffic.unpooled10m > 0 {
            return .unpooled(count: traffic.unpooled10m)
        }

        if let processes = codexProcesses, processes > 0,
            let quietFor = codexQuietFor, quietFor >= quietWindow,
            requests == 0,
            codex.first?.codexRoute?.pooled == true,
            traffic?.unpooled10m == 0
        {
            return .noRequests(processes: processes, bundledTcrWithoutCLI: bundledTcrWithoutCLI)
        }
        return nil
    }
}
