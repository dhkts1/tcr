import Foundation

/// The one Codex line the Accounts tab may draw under the summary, beside the Claude one.
///
/// This is the pure half, as ``NoRequestsBanner`` is for Claude: given what was measured (the
/// fleet, the codex processes running, how long the Codex request count has stood still,
/// whether this Mac shares its accounts), which line shows and what it says. The shell reads
/// those facts at each opening (``CodexBannerSnapshot``).
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
    /// Every input must be measured: the process count, how long the Codex accounts' request
    /// count has stood still (``CodexQuietClock``), a pooled route and an empty unpooled
    /// count. A reading that is missing draws nothing; a miss means no line, never a false
    /// one.
    case noRequests(processes: Int, bundledTcrWithoutCLI: String?)

    /// What the last refusal told codex, from its error type.
    public enum LastRefusal: Equatable, Sendable {
        /// `usage_limit_reached`: codex was told its usage limit is reached.
        case usageLimit
        /// `no_codex_account`: the pool held no usable Codex account.
        case noAccount
        /// The login or workspace was refused; waiting cannot repair it.
        case credential
        /// No available pooled account could serve the requested model.
        case model
        /// Any other type, or none: codex was asked to retry.
        case other

        init(_ type: String?) {
            switch type {
            case "usage_limit_reached": self = .usageLimit
            case "no_codex_account": self = .noAccount
            case "credential_refused": self = .credential
            case "model_unavailable": self = .model
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

    /// How long the Codex accounts' request count must have stood still: the Claude line's
    /// window.
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
        case (_, .model):
            let noun = count == 1 ? "request" : "requests"
            return
                "tcr refused \(count) Codex \(noun) in the last 10 min: no available pooled account could serve the requested model. Choose another model or enable an account that supports it."
        case (_, .credential):
            let noun = count == 1 ? "request" : "requests"
            return
                "tcr refused \(count) Codex \(noun) in the last 10 min: a login or workspace was refused. Check the workspace or sign in again."
        case (true, .usageLimit):
            return "tcr refused 1 Codex request in the last 10 min and told codex its usage limit is reached."
        case (true, .noAccount):
            return "tcr refused 1 Codex request in the last 10 min: no usable Codex account was in the pool."
        case (true, .other):
            return
                "tcr refused 1 Codex request in the last 10 min and asked codex to retry: no Codex account could serve it."
        case (false, .usageLimit):
            return
                "tcr refused \(count) Codex requests in the last 10 min; the last one told codex its usage limit is reached."
        case (false, .noAccount):
            return
                "tcr refused \(count) Codex requests in the last 10 min; the last one found no usable Codex account in the pool."
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
    /// the running codex sessions, `codexQuietFor` how long the serving Codex accounts' request
    /// count has stood still (``CodexQuietClock``), and `bundledTcrWithoutCLI` the tcr inside
    /// TcrBar when this Mac has no other; each is `nil` while unknown.
    ///
    /// `codexProcesses` is read only once every other input holds for the last line, because
    /// it is a walk of the process table: an opening that draws another line, or no line for
    /// another reason, never walks it.
    public static func current(
        fleet: Fleet, codexProcesses: @autoclosure () -> Int?, codexQuietFor: TimeInterval?,
        peerSharing: Bool, bundledTcrWithoutCLI: String? = nil
    ) -> CodexBanner? {
        guard codexRequests(fleet) != nil else { return nil }
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

        if let quietFor = codexQuietFor, quietFor >= quietWindow,
            codex.first?.codexRoute?.pooled == true,
            traffic?.unpooled10m == 0,
            let processes = codexProcesses(), processes > 0
        {
            return .noRequests(processes: processes, bundledTcrWithoutCLI: bundledTcrWithoutCLI)
        }
        return nil
    }
}

/// How long the Codex accounts' request count has stood still, for the line that says no
/// Codex request has reached tcr in five minutes.
///
/// Each row's `requests` counts from the proxy's start, so "the count is 0" means "none since
/// the proxy started": after one pooled turn in the morning it is never 0 again, and a codex
/// started later on its own login would never be named. The line claims the last five
/// minutes, so the clock restarts whenever the summed count moves (a proxy restart's drop
/// included) and stops on a read that is not live or holds no Codex account that can serve.
public struct CodexQuietClock: Equatable, Sendable {
    /// The summed count the clock last saw, `nil` while stopped.
    public private(set) var requests: Int?
    /// When that count was first seen, `nil` while stopped.
    public private(set) var since: Date?

    public init() {}

    /// Advance the clock off the poll that just landed; `nil` is a poll that loaded no fleet.
    public mutating func advance(_ fleet: Fleet?, now: Date) {
        guard let fleet, let total = CodexBanner.codexRequests(fleet) else {
            self = CodexQuietClock()
            return
        }
        guard total != requests else { return }
        requests = total
        since = now
    }

    /// How long the count has stood still at `now`, or `nil` while stopped.
    public func quietFor(now: Date) -> TimeInterval? {
        since.map { now.timeIntervalSince($0) }
    }
}
