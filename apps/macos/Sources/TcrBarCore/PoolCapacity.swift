import Foundation

// Each provider's accounts, counted apart.
//
// A Claude account and a Codex account serve different clients: Claude Code cannot spend
// a ChatGPT plan, and codex cannot spend a Claude one. So a fleet holding both has two
// capacities, and one pooled count misstates both. "1 ready · 2 spent" over a part-full
// cup reads as "work can proceed" while every Claude request is refused, because the one
// ready account is a Codex account. Counting each pool on its own is what lets the menu
// bar, the strip and the tooltip say which client can work.
//
// A pool counts an account as ready when its state (`FleetTally.Kind`) is `.ok`. That is
// stricter than Claude's `Account.isReady`, which does not read the server's gate.
// Codex readiness does read its gate: refused, unreachable, group-parked and signed-out
// Codex accounts cannot serve.
//
// A fleet with no Codex account is not counted here at all. `Fleet.providerPools(now:)` is
// `nil` for it, the empty fleet included, and every caller keeps today's path on that
// `nil`: today's readiness, cup, tint, tooltip, strip and summary, unchanged. Today's
// readiness has known Claude-side gaps (a rejected or group-parked Claude row counts as
// ready). Closing them would change what a Claude-only fleet shows, so that is a separate,
// deliberate change, not a side effect of this one.

/// One provider's rows and what they can serve: the count, the cup's fill and tint, the
/// breakdown, and when the pool next frees.
public struct PoolCapacity: Equatable, Sendable {
    public let provider: Provider
    /// This provider's rows, in every state, parked included.
    public let accounts: [Account]
    /// The clock a return is counted down from.
    public let now: Date
    /// Each row's state, in ``accounts``' order. Computed once: every count below reads it.
    private let kinds: [FleetTally.Kind]

    public init(provider: Provider, accounts: [Account], now: Date) {
        self.provider = provider
        self.accounts = accounts
        self.now = now
        self.kinds = accounts.map(FleetTally.Kind.init(account:))
    }

    /// The pool's name in prose: `Claude` or `Codex`.
    public var name: String {
        switch provider {
        case .anthropic: return "Claude"
        case .codex: return "Codex"
        }
    }

    /// Rows whose state is `.disabled`: disabled by hand, or held back by a parked group.
    public var parkedCount: Int { count(.disabled) }

    /// The rows that could serve: every row that is not parked. A row in a parked group is
    /// parked here exactly as a disabled row is, because the operator faces the same fact:
    /// nothing lands on it.
    public var enabledCount: Int { accounts.count - parkedCount }

    /// Enabled rows whose state is ready.
    public var readyCount: Int { count(.ok) }

    /// Enabled rows near their limit.
    public var nearCount: Int { count(.near) }

    /// Enabled rows nothing has been measured about.
    public var unmeasuredCount: Int { count(.unmeasured) }

    /// Nothing ready, and something near its limit: the pool's amber case.
    public var isNear: Bool { readyCount == 0 && nearCount > 0 }

    /// `ready / enabled`, the cup's fill; `nil` when nothing is enabled.
    public var readyFraction: Double? {
        guard enabledCount > 0 else { return nil }
        return Double(readyCount) / Double(enabledCount)
    }

    /// The pool's state for the cup, tested in this order: nothing enabled
    /// (`unknown("empty")`), something ready (`ok`), something near (`near`), something
    /// unmeasured (`unknown("unmeasured")`), else `spent`. The ladder
    /// ``Fleet/capacityGlyphState`` walks for a whole fleet.
    public var glyphState: QuotaState {
        if enabledCount == 0 { return .unknown("empty") }
        if readyCount > 0 { return .ok }
        if nearCount > 0 { return .near }
        if unmeasuredCount > 0 { return .unknown("unmeasured") }
        return .spent
    }

    /// How bad ``glyphState`` is, for a tie between two pools: spent and unmeasured 2,
    /// near 1, ok 0. A pool nobody can vouch for is as bad as an empty one, and both are
    /// worse than one that is close.
    public var glyphRank: Int {
        switch glyphState {
        case .spent, .unknown("unmeasured"): return 2
        case .near: return 1
        case .ok, .unknown: return 0
        }
    }

    /// When the pool next frees, in unix milliseconds, or `nil` while it has a ready row or
    /// no known return.
    ///
    /// The soonest instant still ahead of ``now`` among the enabled rows. Each row offers
    /// one: when it is back (``Account/backAtMs``, the LATEST reset among the windows
    /// holding it, because a row held by two windows is still out when the first one
    /// rolls); or, on a Codex row that holds no window, the end of the server's hold
    /// (``Account/freeAtMs``). A refusal or a 403 hold carries no window, so its end is the
    /// only return it has. A Claude row's hold end is not read, so a Claude pool counts
    /// down to its windows alone.
    public var nextReturnAtMs: Int64? {
        guard readyCount == 0 else { return nil }
        let nowMs = Int64(now.timeIntervalSince1970 * 1000)
        return zip(accounts, kinds)
            .filter { _, kind in kind != .disabled && kind != .ok }
            .compactMap { account, _ in Self.returnAtMs(account) }
            .filter { $0 > nowMs }
            .min()
    }

    /// `"next in 2h 39m"`, the pool's return as every countdown on the panel prints one
    /// (``QuotaFormat/resetCaption(resetAtMs:now:)``): rounded to the minute, and `nil`
    /// for a return under 30 seconds away, which would otherwise read `in now`.
    public var nextIn: String? {
        QuotaFormat.resetCaption(resetAtMs: nextReturnAtMs, now: now).map { "next " + $0 }
    }

    /// Every state with at least one row, in the order the summary has always used, with
    /// the unreachable bucket after the refused one: ready, near limit, spent, unknown,
    /// unmeasured, need re-login, rejected or refused, unreachable, parked.
    public var breakdown: [FleetTally] {
        let order: [FleetTally.Kind] = [
            .ok, .near, .spent, .unknown, .unmeasured, .needsRelogin, .rejected, .unreachable,
            .disabled,
        ]
        return order.compactMap { kind in
            let n = count(kind)
            return n > 0 ? FleetTally(kind: kind, count: n) : nil
        }
    }

    /// ``breakdown`` in words, each worded for this provider: `2 refused by ChatGPT` on
    /// Codex, `1 rejected by Anthropic` on Claude.
    public var breakdownLabels: [String] {
        breakdown.map { "\($0.count) \($0.kind.phrase(for: provider))" }
    }

    /// `"3/4"`, this pool's part of the menu-bar count; `nil` when nothing is enabled, so a
    /// parked pool drops out of the count.
    public var countsText: String? {
        guard enabledCount > 0 else { return nil }
        return "\(readyCount)/\(enabledCount)"
    }

    /// `"3 of 4 accounts ready · 1 near their limit · 1 parked"`, then ` · next in 2h 39m`
    /// when the pool is waiting: ``Fleet/countsSentence``'s clauses over this pool, with
    /// readiness by state. `nounPrefix` names the accounts (`"Codex "` gives
    /// `"2 of 4 Codex accounts ready"`). A clause with a zero count is dropped. `nil` when
    /// nothing is enabled.
    public func countsSentence(nounPrefix: String = "") -> String? {
        guard enabledCount > 0 else { return nil }
        let noun = enabledCount == 1 ? "account" : "accounts"
        var clauses = ["\(readyCount) of \(enabledCount) \(nounPrefix)\(noun) ready"]
        if nearCount > 0 { clauses.append("\(nearCount) near their limit") }
        if unmeasuredCount > 0 { clauses.append("\(unmeasuredCount) unmeasured") }
        if parkedCount > 0 { clauses.append("\(parkedCount) parked") }
        if let nextIn { clauses.append(nextIn) }
        return clauses.joined(separator: " · ")
    }

    /// One row's return: a Codex row that holds no window returns when the server's hold
    /// ends, every other row when the last window holding it resets.
    private static func returnAtMs(_ account: Account) -> Int64? {
        if account.provider == .codex && account.held.isEmpty { return account.freeAtMs }
        return account.backAtMs
    }

    private func count(_ kind: FleetTally.Kind) -> Int {
        kinds.reduce(0) { $0 + ($1 == kind ? 1 : 0) }
    }
}

extension MenuBarCupPool {
    /// The provider a pin names, or `nil` for the emptier pool.
    var pinnedProvider: Provider? {
        switch self {
        case .emptier: return nil
        case .claude: return .anthropic
        case .codex: return .codex
        }
    }
}

/// One clause of a summary, with the role its colour comes from. A role rather than a
/// colour, so this file stays free of SwiftUI and the view maps each role to one palette
/// token.
public struct SummaryRun: Equatable, Sendable {
    public enum Role: Equatable, Sendable {
        /// A pool's name, leading its row.
        case lead
        /// Ready accounts.
        case ok
        /// Accounts near their limit.
        case near
        /// Accounts that serve nothing right now: spent, signed out, refused, unreachable.
        case bad
        /// A figure, and the states nobody has to act on.
        case dim
        /// A caveat on the figure beside it.
        case mute
    }

    public let text: String
    public let role: Role

    public init(text: String, role: Role) {
        self.text = text
        self.role = role
    }
}

/// The money line at the top of the Accounts tab, in a fleet holding Codex accounts.
///
/// The line keeps the grammar it has always had (today's spend, the cache, a caveat),
/// with each figure taken from the rows it is about. Beside Claude's spend, the cache is
/// Claude's prompt cache and "unpriced today" counts Claude requests; the fleet-wide
/// figures would fold Codex in, and a ChatGPT plan is not priced per request at all. Codex
/// joins as one clause of its own: its output tokens today, or its dollars when the
/// operator's prices cover every Codex request.
public enum AccountsSummary {
    /// The money line's runs, or `nil`.
    ///
    /// `nil` for a fleet with no Codex account, which keeps today's line, and for a fleet
    /// holding Codex accounts with nothing measured to say.
    ///
    /// - Both providers: `$28.3 today · cache 75%`, then `· N unpriced today` in mute when
    ///   Claude has unpriced requests, then `· Codex 83k tokens`, or `· Codex $4.60` when
    ///   the Codex day is fully priced. Each clause is there only when its rows were
    ///   measured.
    /// - Codex only: `343k tokens today · cache 87%`, or `$4.60 today · cache 87%` when
    ///   fully priced.
    public static func lines(_ fleet: Fleet) -> [[SummaryRun]]? {
        guard fleet.holdsCodex else { return nil }
        var runs: [SummaryRun] = []
        if fleet.holdsAnthropic {
            if fleet.hasUsage(of: .anthropic) {
                runs.append(
                    SummaryRun(
                        text: "\(QuotaFormat.usd(fleet.todayCost(of: .anthropic))) today",
                        role: .dim))
                runs.append(
                    SummaryRun(
                        text:
                            "cache \(QuotaFormat.percent(fleet.todayCacheHitRatio(of: .anthropic)))",
                        role: .dim))
                if let unpriced = fleet.todayUnpricedRequests(of: .anthropic), unpriced > 0 {
                    runs.append(SummaryRun(text: "\(unpriced) unpriced today", role: .mute))
                }
            }
            if fleet.hasUsage(of: .codex) {
                runs.append(SummaryRun(text: "Codex \(codexFigure(fleet))", role: .dim))
            }
        } else if fleet.hasUsage(of: .codex) {
            runs.append(SummaryRun(text: "\(codexFigure(fleet)) today", role: .dim))
            runs.append(
                SummaryRun(
                    text: "cache \(QuotaFormat.percent(fleet.todayCacheHitRatio(of: .codex)))",
                    role: .dim))
        }
        return runs.isEmpty ? nil : [runs]
    }

    /// What the Codex clause means, for the money line's help: in a fleet holding both
    /// providers whose line carries the clause, and `nil` everywhere else.
    public static func usageHelp(_ fleet: Fleet) -> String? {
        guard fleet.holdsBothProviders, fleet.hasUsage(of: .codex) else { return nil }
        if fleet.todayIsFullyPriced(of: .codex) {
            return "Codex: \(QuotaFormat.usd(fleet.todayCost(of: .codex))) today, "
                + "at the prices in your config."
        }
        return "Codex: \(QuotaFormat.tokens(fleet.todayOutputTokens(of: .codex))) output tokens "
            + "today, unpriced: a ChatGPT plan is not billed per request."
    }

    /// `$4.60` when the Codex day is fully priced, else `83k tokens`.
    private static func codexFigure(_ fleet: Fleet) -> String {
        if fleet.todayIsFullyPriced(of: .codex) {
            return QuotaFormat.usd(fleet.todayCost(of: .codex))
        }
        return "\(QuotaFormat.tokens(fleet.todayOutputTokens(of: .codex))) tokens"
    }
}

extension Fleet {
    /// Whether the fleet holds a Claude account and a Codex account, in any state: the
    /// fleet whose count has two parts and whose Settings offer the cup's pin.
    public var holdsBothProviders: Bool { holdsCodex && holdsAnthropic }

    /// One pool per provider the fleet holds, Claude first. `nil` for a fleet holding no
    /// Codex account, the empty fleet included, which keeps today's counts. A Codex-only
    /// fleet has one pool.
    public func providerPools(now: Date = Date()) -> [PoolCapacity]? {
        guard holdsCodex else { return nil }
        var pools: [PoolCapacity] = []
        if holdsAnthropic {
            pools.append(
                PoolCapacity(
                    provider: .anthropic, accounts: accounts.filter { $0.provider == .anthropic },
                    now: now))
        }
        pools.append(
            PoolCapacity(
                provider: .codex, accounts: accounts.filter { $0.provider == .codex }, now: now))
        return pools
    }

    /// The pool the cup follows, or `nil` for a fleet holding no Codex account, and for a
    /// fleet with no enabled account in any pool.
    ///
    /// In a fleet holding both providers a pin wins when its pool has an enabled account;
    /// a fleet with one provider has no choice to pin. Otherwise the pool with the lower
    /// ready fraction governs, among the pools with an enabled account, so a dead pool
    /// never hides behind a healthy one. A tie goes to the worse ``PoolCapacity/glyphRank``,
    /// then to the first pool, which is Claude's. The fractions are compared as cross
    /// products, so a tie is exact.
    public func governingPool(_ pin: MenuBarCupPool, now: Date = Date()) -> PoolCapacity? {
        guard let pools = providerPools(now: now) else { return nil }
        let candidates = pools.filter { $0.enabledCount > 0 }
        if holdsBothProviders, let pinned = pin.pinnedProvider,
            let pool = candidates.first(where: { $0.provider == pinned })
        {
            return pool
        }
        var governing: PoolCapacity?
        for pool in candidates {
            guard let current = governing else {
                governing = pool
                continue
            }
            let lower = pool.readyCount * current.enabledCount
            let higher = current.readyCount * pool.enabledCount
            if lower < higher || (lower == higher && pool.glyphRank > current.glyphRank) {
                governing = pool
            }
        }
        return governing
    }
}
