import Foundation

/// The strip at the top of the Accounts tab (option H, unit H3; Gil
/// 2026-10-14): a count headline, "next back in", and one segment per account
/// in the order the tab draws its rows, each filled by that account's own
/// 5-hour room.
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
        case .spent, .needsRelogin, .rejected: self = .bad
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

    public init(_ account: Account) {
        switch FleetBand(account) {
        case .parked:
            self = .off
        case .outOfTokens:
            self = .outOfTokens
        case .live:
            guard account.hasQuotaEvidence, let used = account.fiveHour else {
                self = .off
                return
            }
            self = .room(min(1, max(0, 1 - used)), StripSegmentTone(FleetTally.Kind(account: account)))
        }
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

    /// "Segments are about 25 pt wide at 13 accounts and about 12 pt at 25;
    /// past that the strip stops being pointable" (option H's own note). The
    /// segments are still drawn; pointing at one does nothing.
    public static let pointableLimit = 25

    /// `drawnRows`: every row the tab draws (and the rows of a group it has
    /// closed), in its order; ``Swift/Array/rowsInDrawOrder(expandedGroups:parkedCap:isCard:)``
    /// builds it. An account's segment sits at its FIRST row.
    public init(drawnRows: [FleetSectionRow], now: Date) {
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
    }

    /// "5 of 13 can take work now".
    public var headline: String { "\(canTakeWork) of \(total) can take work now" }

    public var isPointable: Bool { segments.count <= Self.pointableLimit }
}

extension FleetSection {
    /// A group the operator has not opened and that collapses by default: the
    /// tab draws it as one summary line, after every open group.
    public func isSummarised(expandedGroups: Set<String>) -> Bool {
        collapsesByDefault && !expandedGroups.contains(group.token)
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
        let all = sectionsInDisplayOrder(pinning: controlName)
        let loose = all.filter { $0.group == .ungrouped }
        let named = all.filter { $0.group != .ungrouped }
        return loose
            + named.filter { !$0.isSummarised(expandedGroups: expandedGroups) }
            + named.filter { $0.isSummarised(expandedGroups: expandedGroups) }
    }
}

extension Array where Element == FleetSection {
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
            } else if section.isWhollyParked && !expandedGroups.contains(section.group.token) {
                shown = [FleetSectionRow](section.rows.prefix(parkedCap))
            } else {
                shown = section.rows
            }
            let hidden = [FleetSectionRow](section.rows.dropFirst(shown.count))
            return shown.filter { !isCard($0) } + shown.filter(isCard) + hidden
        }
    }
}
