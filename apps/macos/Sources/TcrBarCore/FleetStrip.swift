import Foundation

/// The strip at the top of the Accounts tab (option H, unit H3; Gil
/// 2026-10-14): a count headline, "next back in", and one segment per account
/// in the order the tab draws its rows, each filled by that account's own
/// 5-hour room. A Codex plan may report no 5-hour window, so a Codex segment
/// fills from its week instead, and from nothing at all when the plan reports
/// no window: it is then filled whole.
///
/// Pure, and in the library rather than in the view, for the reason
/// ``PanelHeight`` gives: the test target links `TcrBarCore` only, and the
/// order of the segments is exactly the rule that drifts when it lives in a
/// view. The tab's own section order (``Fleet/accountsTabSections(pinning:expandedGroups:)``)
/// and draw order (``Swift/Array/rowsInDrawOrder(expandedGroups:parkedCap:isCard:)``) live
/// here too, so the list and the strip read one order, not two copies of it.
///
/// What the option H frame draws besides (the week line, "login ends in",
/// "reading 40m old" and faded segments) waits for data `tcr` does not send
/// yet, and is absent here rather than zero.

/// The colour a filled segment wears: its row's dot, by the same rule.
public enum StripSegmentTone: Equatable, Sendable {
    case ok
    case near
    case bad
    case info
    case quiet

    /// The dot's colour for each ``FleetTally/Kind``, as `AccountLineRow`
    /// draws it.
    init(_ kind: FleetTally.Kind) {
        switch kind {
        case .ok: self = .ok
        case .near: self = .near
        case .spent, .needsRelogin, .rejected, .unreachable: self = .bad
        case .unmeasured: self = .info
        case .unknown, .disabled: self = .quiet
        }
    }
}

/// What one segment draws.
public enum StripSegmentKind: Equatable, Sendable {
    /// A live account with a 5-hour reading: the share of the window still
    /// left (0 to 1, from the bottom), in the row's colour.
    case room(Double, StripSegmentTone)
    /// Out of tokens: the whole segment red, no fill.
    case outOfTokens
    /// Parked, or never measured: the whole segment grey, no fill. A live
    /// account with no 5-hour reading is grey too, never an empty track that
    /// reads as a window used up.
    case off

    /// A live Codex account fills from the first window its plan reports: its
    /// 5-hour room, or its week's on a plan with no 5-hour window, such as Pro,
    /// whose 5-hour reading is absent for good rather than not yet taken. A plan
    /// that reports no window at all has nothing used, so it fills whole, in its
    /// row's colour. Every other account keeps the 5-hour rule.
    public init(_ account: Account) {
        switch FleetBand(account) {
        case .parked:
            self = .off
        case .outOfTokens:
            self = .outOfTokens
        case .live:
            if account.provider == .codex {
                self = Self.codexRoom(account)
                return
            }
            guard account.hasQuotaEvidence, let used = account.fiveHour else {
                self = .off
                return
            }
            self = .room(min(1, max(0, 1 - used)), StripSegmentTone(FleetTally.Kind(account: account)))
        }
    }

    /// A live Codex account's segment: grey with no evidence, whole for a plan with
    /// no window, else the room left in its 5-hour window or, without one, its week.
    private static func codexRoom(_ account: Account) -> StripSegmentKind {
        guard account.hasQuotaEvidence else { return .off }
        let tone = StripSegmentTone(FleetTally.Kind(account: account))
        if account.quotaWindows == [] { return .room(1, tone) }
        guard let used = account.fiveHour ?? account.sevenDay else { return .off }
        return .room(min(1, max(0, 1 - used)), tone)
    }
}

/// One account's segment. Identified by the ACCOUNT, not the row: an account
/// in two groups draws two lines and one segment.
public struct StripSegment: Identifiable, Equatable, Sendable {
    public let account: Account
    public let kind: StripSegmentKind
    public var id: String { account.id }

    public init(account: Account) {
        self.account = account
        self.kind = StripSegmentKind(account)
    }
}

/// One provider's headline row in the strip, for a fleet holding Codex accounts.
///
/// A Claude account and a Codex account serve different clients, so a fleet holding both
/// has two capacities, and the one folded "can take work now" over every row would hide
/// a pool with nothing left: three ready Codex accounts read as work proceeding while
/// every Claude request is refused. Each pool therefore gets its own row, with the count
/// the menu bar shows for it, worded "ready" because it is that count and not the
/// band-based one the folded headline makes.
///
/// The row counts down by the strip's own rule, over this pool's out-of-tokens rows
/// alone, so it agrees with that pool's OUT OF TOKENS heading. When that rule has no
/// figure and nothing in the pool is ready, the pool's own return fills in: a row held
/// only by a hold that carries no window still says when it frees.
public struct StripPoolLine: Equatable, Sendable {
    public let provider: Provider
    /// `Claude` or `Codex`.
    public let name: String
    /// Rows whose state is ready.
    public let ready: Int
    /// Rows that are not parked.
    public let enabled: Int
    /// Rows parked by hand or by a parked group.
    public let parked: Int
    /// The count's colour: grey with nothing enabled, green with something ready, amber
    /// with nothing ready and something near its limit, red otherwise.
    public let tone: SummaryRun.Role
    /// `"in 2h 39m"`: the soonest this pool's out-of-tokens rows are back, by the
    /// rule its OUT OF TOKENS heading uses. `nil` when none is waiting on a reset.
    public let nextBackCountdown: String?
    /// `"in 47m"`: the pool's own return, only while nothing in it is ready and
    /// ``nextBackCountdown`` has no figure. `nil` otherwise.
    public let nextInCountdown: String?
    /// The breakdown the row does not draw: `"Codex: 3 ready · 1 spent"`.
    public let help: String
    /// The breakdown's clauses past the ready one, for the spoken sentence.
    private let spokenClauses: [String]

    public init(pool: PoolCapacity, nextBackCountdown: String?) {
        provider = pool.provider
        name = pool.name
        ready = pool.readyCount
        enabled = pool.enabledCount
        parked = pool.parkedCount
        if pool.enabledCount == 0 {
            tone = .dim
        } else if pool.readyCount > 0 {
            tone = .ok
        } else if pool.isNear {
            tone = .near
        } else {
            tone = .bad
        }
        self.nextBackCountdown = nextBackCountdown
        nextInCountdown =
            nextBackCountdown == nil
            ? QuotaFormat.resetCaption(resetAtMs: pool.nextReturnAtMs, now: pool.now) : nil
        help = "\(pool.name): " + pool.breakdownLabels.joined(separator: " · ")
        spokenClauses = pool.breakdown.filter { $0.kind != .ok }.map {
            "\($0.count) \($0.kind.phrase(for: pool.provider))"
        }
    }

    /// `"Claude · "`, drawn ahead of the count.
    public var lead: String { "\(name) · " }

    /// `"0 of 2 ready"`, or `"2 parked"` when nothing in the pool is enabled.
    public var count: String {
        enabled == 0 ? "\(parked) parked" : "\(ready) of \(enabled) ready"
    }

    /// `"Claude, 0 of 2 ready, 2 spent, next back in 2h 39m."`: the row and its
    /// breakdown, saying the ready count once.
    public var spoken: String {
        guard enabled > 0 else { return "\(name), \(parked) parked." }
        var clauses = ["\(name), \(ready) of \(enabled) ready"] + spokenClauses
        if let nextBackCountdown {
            clauses.append("next back \(nextBackCountdown)")
        } else if let nextInCountdown {
            clauses.append("next \(nextInCountdown)")
        }
        return clauses.joined(separator: ", ") + "."
    }
}

public struct FleetStrip: Equatable, Sendable {
    /// Left to right, the tab's rows top to bottom.
    public let segments: [StripSegment]
    /// N: the accounts in the live band. A count; no plan weighting.
    public let canTakeWork: Int
    /// M: every account.
    public let total: Int
    /// `"in 42m"`: the soonest an out-of-tokens account is back, the same
    /// figure as the out-of-tokens heading's "first back in". `nil` when no
    /// account is waiting on a reset.
    public let nextBackCountdown: String?
    /// One headline row per pool, Claude first, drawn in place of ``headline``
    /// in a fleet holding Codex accounts. `nil` for every other fleet, which
    /// draws ``headline`` as it always has.
    public let pools: [StripPoolLine]?

    /// "Segments are about 25 pt wide at 13 accounts and about 12 pt at 25;
    /// past that the strip stops being pointable" (option H's own note). The
    /// segments are still drawn; pointing at one does nothing.
    public static let pointableLimit = 25

    /// `drawnRows`: every row the tab draws (and the rows of a group it has
    /// closed), in its order; ``Swift/Array/rowsInDrawOrder(expandedGroups:parkedCap:isCard:)``
    /// builds it. An account's segment sits at its FIRST row.
    ///
    /// `pools`: ``Fleet/providerPools(now:)``, `nil` for a fleet with no Codex
    /// account. Each pool's row counts down by the rule ``nextBackCountdown``
    /// uses, over that pool's own out-of-tokens segments.
    public init(drawnRows: [FleetSectionRow], now: Date, pools: [PoolCapacity]? = nil) {
        var seen: Set<String> = []
        var segments: [StripSegment] = []
        for row in drawnRows where seen.insert(row.account.id).inserted {
            segments.append(StripSegment(account: row.account))
        }
        self.segments = segments
        self.total = segments.count
        self.canTakeWork = segments.filter { FleetBand($0.account) == .live }.count
        self.nextBackCountdown = FleetSection.firstBackCountdown(
            segments.map(\.account).filter { FleetBand($0) == .outOfTokens }, now: now)
        self.pools = pools?.map { pool in
            StripPoolLine(
                pool: pool,
                nextBackCountdown: FleetSection.firstBackCountdown(
                    segments.map(\.account).filter {
                        $0.provider == pool.provider && FleetBand($0) == .outOfTokens
                    }, now: now))
        }
    }

    /// "5 of 13 can take work now".
    public var headline: String { "\(canTakeWork) of \(total) can take work now" }

    public var isPointable: Bool { segments.count <= Self.pointableLimit }

    /// The index of the first segment of the second pool, in a fleet holding both providers: the
    /// tab draws Claude's list first, so it is the first Codex segment. `nil` when every segment is
    /// one provider's.
    public var poolBreak: Int? {
        guard let lead = segments.first?.account.provider else { return nil }
        return segments.firstIndex { $0.account.provider != lead }
    }
}

extension FleetSection {
    /// A group the operator has not opened and that collapses by default: the
    /// tab draws it as one summary line, after every open group. Opened under
    /// its own ``expansionKey``, so a Codex box and a Claude box of one group
    /// open apart.
    public func isSummarised(expandedGroups: Set<String>) -> Bool {
        collapsesByDefault && !expandedGroups.contains(expansionKey)
    }
}

extension Fleet {
    /// The Accounts tab's own section order: the loose sections first, then
    /// the open groups, then the groups closed to one summary line, each run
    /// in ``sectionsInDisplayOrder(pinning:)``'s order. `AccountsTabV4`'s doc
    /// comment has why loose lines lead and closed groups come last.
    public func accountsTabSections(
        pinning controlName: String? = nil, expandedGroups: Set<String>
    ) -> [FleetSection] {
        sectionsInDisplayOrder(pinning: controlName).inTabOrder(expandedGroups: expandedGroups)
    }

    /// The Accounts tab's lists in a fleet holding Codex, Claude first, one per provider that
    /// has rows; `nil` in a fleet with no Codex row, which draws `accountsTabSections` unchanged.
    ///
    /// In a mixed fleet the provider is the outer level, because the operator asks two
    /// questions: what can serve Claude Code, and what can serve codex. A group with members of
    /// both is drawn once in each list, so each box's count, tally and spend cover one
    /// provider. Each list is in the tab's own order, which the strip reads too.
    public func accountsTabSectionsByProvider(
        pinning controlName: String? = nil, expandedGroups: Set<String>
    ) -> [ProviderSections]? {
        guard holdsCodex else { return nil }
        return [Provider.anthropic, .codex].compactMap { provider in
            let rows = accounts.filter { $0.provider == provider }
            guard !rows.isEmpty else { return nil }
            return ProviderSections(
                provider: provider,
                sections: FleetSection.sections(of: rows, provider: provider, pinning: controlName)
                    .inTabOrder(expandedGroups: expandedGroups))
        }
    }
}

extension Array where Element == FleetSection {
    /// The tab's order over one list: the loose sections, then the open groups, then the groups
    /// closed to one summary line, each run in the list's own order.
    public func inTabOrder(expandedGroups: Set<String>) -> [FleetSection] {
        let loose = filter { $0.group == .ungrouped }
        let named = filter { $0.group != .ungrouped }
        return loose
            + named.filter { !$0.isSummarised(expandedGroups: expandedGroups) }
            + named.filter { $0.isSummarised(expandedGroups: expandedGroups) }
    }
    /// Every row of these sections, in the order the tab draws them: in each
    /// section its lines, then its cards (`isCard`: an account carrying a
    /// lease line or an exit row). A wholly-parked group the tab caps at
    /// `parkedCap` rows draws those first; its hidden rows, and every row of a
    /// group closed to one line, follow in the section's own order, where they
    /// would be drawn if it were opened.
    public func rowsInDrawOrder(
        expandedGroups: Set<String>, parkedCap: Int, isCard: (FleetSectionRow) -> Bool
    ) -> [FleetSectionRow] {
        flatMap { section -> [FleetSectionRow] in
            let shown: [FleetSectionRow]
            if section.isSummarised(expandedGroups: expandedGroups) {
                shown = []
            } else if section.isWhollyParked && !expandedGroups.contains(section.expansionKey) {
                shown = [FleetSectionRow](section.rows.prefix(parkedCap))
            } else {
                shown = section.rows
            }
            let hidden = [FleetSectionRow](section.rows.dropFirst(shown.count))
            return shown.filter { !isCard($0) } + shown.filter(isCard) + hidden
        }
    }
}
