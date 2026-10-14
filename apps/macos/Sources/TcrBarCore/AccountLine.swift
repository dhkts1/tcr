import Foundation

/// The words on an account's ONE line in the Accounts tab (option H, Gil
/// 2026-10-14): a live account's right-hand signal, and a waiting account's
/// middle and right-hand text, plus the band heading over the waiting ones.
///
/// Pure, and in the library rather than in the view, for the reason
/// ``PanelHeight`` gives: the test target links `TcrBarCore` only, and which
/// word a row says is exactly the rule that drifts when it lives in a view.
///
/// Every string here follows the house rule ``QuotaFormat/percent(_:)``
/// states: a figure nothing measured is ABSENT, never a zero and never a
/// placeholder. A row with no countdown on the wire draws no countdown.

/// What colour a line's words wear. A role rather than a colour, so this file
/// stays free of SwiftUI and the view maps each role to one palette token.
public enum AccountLineTone: Equatable, Sendable {
    /// `ready`, `parked`: the ordinary state, in the panel's quiet grey.
    case quiet
    /// A window near its limit, or a request waiting on a Mac that is down.
    case near
    /// Out of tokens, or a credential that cannot serve.
    case bad
    /// Nothing has measured this account yet.
    case info
}

/// How the Accounts tab draws one account: a one-line row, or its card.
public enum AccountRowForm: Equatable, Sendable {
    /// `.slim`: the dot, the name, both bars and one signal.
    case liveLine
    /// `.wline`: the dot, the name, the week used, and "back in" or "parked".
    case waitingLine
    /// The account's card, in the list itself.
    case card
    /// The account's hover card, drawn in the list in place of its line,
    /// because the operator chose "Keep Card Open" from its right-click menu
    /// (Gil, 2026-10-14). See ``KeptOpenCards``.
    case keptOpenCard
}

extension FleetSectionRow {
    /// Which form this row takes.
    ///
    /// A line follows the band: a live account gets the live line, an out of
    /// tokens or parked one the waiting line. The group does not enter into
    /// it: an account inside a group box gets the same line as one outside
    /// (lead ruling on H1b, 2026-10-14: "a fleet mixing cards and lines reads
    /// as unfinished").
    ///
    /// An account carrying a lease line (`carriesLease`) or an exit picker
    /// (`carriesExit`) keeps its card for good (lead ruling, 2026-10-14): the
    /// lease line is the notice that another Mac may read this account's
    /// traffic, which is not something to show only on hover, and the exit
    /// picker is a control, which cannot live in a hover card that vanishes
    /// when the pointer moves.
    ///
    /// An account in `keptOpen` draws its hover card in place of its line,
    /// live or waiting, in a group or out of one. The lease and exit rule
    /// still wins: that card is already open, and taller.
    public func form(
        carriesLease: Bool, carriesExit: Bool, keptOpen: KeptOpenCards = KeptOpenCards()
    ) -> AccountRowForm {
        if carriesLease || carriesExit { return .card }
        if keptOpen.contains(account.id) { return .keptOpenCard }
        return band == .live ? .liveLine : .waitingLine
    }
}

/// The accounts whose card the operator keeps open in the Accounts tab, by
/// ``Account/id`` (Gil, 2026-10-14: "The row's right-click menu gains 'Keep
/// card open' / 'Close card'").
///
/// Keyed by the account, never by ``FleetSectionRow/id``: an account in two
/// groups draws two rows, and keeping it open keeps it open in both. Stored by
/// TcrBar in its own `UserDefaults`, so the choice belongs to this Mac and
/// never reaches the `tcr` config file.
public struct KeptOpenCards: Equatable, Sendable {
    /// The account ids, sorted, the form `UserDefaults` stores.
    public private(set) var ids: [String]

    public init(_ ids: [String] = []) {
        self.ids = Array(Set(ids)).sorted()
    }

    public func contains(_ accountID: String) -> Bool {
        ids.contains(accountID)
    }

    /// Opens a closed card or closes an open one.
    public mutating func toggle(_ accountID: String) {
        if contains(accountID) {
            ids.removeAll { $0 == accountID }
        } else {
            ids = (ids + [accountID]).sorted()
        }
    }

    /// The set less every account no longer in `accounts`, so an account that
    /// left the fleet is dropped on the next poll rather than kept as a ghost
    /// that would reopen if a different account ever took its name.
    public func pruned(to accounts: [Account]) -> KeptOpenCards {
        let present = Set(accounts.map(\.id))
        return KeptOpenCards(ids.filter(present.contains))
    }
}

/// One stretch of an account box: a run of one-line rows that share a box, or
/// one kept-open card that sits between two such runs at the box's width.
public enum AccountLineRun: Identifiable, Equatable, Sendable {
    case lines([FleetSectionRow])
    case keptOpen(FleetSectionRow)

    public var id: String {
        switch self {
        case .lines(let rows): return "lines:" + (rows.first?.id ?? "")
        case .keptOpen(let row): return "card:" + row.id
        }
    }
}

extension Array where Element == FleetSectionRow {
    /// These rows, in order, cut into runs at every kept-open one, so each
    /// kept-open card is drawn in its own place in the list. Rows with no
    /// kept-open card among them come back as ONE run, the box as before.
    public func lineRuns(isKeptOpen: (FleetSectionRow) -> Bool) -> [AccountLineRun] {
        var runs: [AccountLineRun] = []
        var pending: [FleetSectionRow] = []
        for row in self {
            if isKeptOpen(row) {
                if !pending.isEmpty { runs.append(.lines(pending)) }
                pending = []
                runs.append(.keptOpen(row))
            } else {
                pending.append(row)
            }
        }
        if !pending.isEmpty { runs.append(.lines(pending)) }
        return runs
    }
}

/// One run of words and the role it is drawn in.
public struct AccountLineText: Equatable, Sendable {
    public let text: String
    public let tone: AccountLineTone

    public init(_ text: String, _ tone: AccountLineTone) {
        self.text = text
        self.tone = tone
    }
}

extension Account {
    /// The one word or countdown at the right of a LIVE account's line.
    ///
    /// Read off ``FleetTally/Kind``, the classifier the group tally and the
    /// hover card's state pill already use, so the row cannot say `ready`
    /// beside a tally that counts it `near`.
    ///
    ///  - `ok`: `ready`.
    ///  - `near`: the countdown to the reset of the window that is near
    ///    (`in 47m`), so the reader learns when the pressure lifts. The 5-hour
    ///    window is checked first because it is the one that resets within
    ///    the hour; a near 7-day window with a calm 5-hour one counts down to
    ///    the WEEK, since the 5-hour reset would relieve nothing. With no
    ///    reset on the wire, the words `near limit` instead of a made-up time.
    ///  - every other kind: the state word today's card pill says, so a
    ///    broken credential still reads as broken on a line with no pill.
    ///    `needs re-login` is shortened to `re-login`, the action, because the
    ///    whole phrase does not fit the column a line leaves it; the red dot
    ///    and the spoken label (``cardSummaryLabel(now:isControl:namesProvider:)``)
    ///    carry the rest. A refused Codex row reads `refused`, ChatGPT's word, and
    ///    carries no countdown: the column would widen for every line in the
    ///    panel, and the hold's end is in the row's help.
    public func liveSignal(now: Date) -> AccountLineText {
        switch FleetTally.Kind(account: self) {
        case .ok:
            return AccountLineText("ready", .quiet)
        case .near:
            guard let caption = nearResetCaption(now: now) else {
                return AccountLineText("near limit", .near)
            }
            return AccountLineText(caption, .near)
        case .spent:
            return AccountLineText(backInCaption(now: now) ?? "out of tokens", .bad)
        case .unknown:
            return AccountLineText("unknown", .quiet)
        case .needsRelogin:
            return AccountLineText("re-login", .bad)
        case .rejected:
            // ChatGPT refuses a Codex account; only Anthropic rejects one.
            return AccountLineText(provider == .codex ? "refused" : "rejected", .bad)
        case .unreachable:
            return AccountLineText("unreachable", .bad)
        case .unmeasured:
            return AccountLineText("unmeasured", .info)
        case .disabled:
            return AccountLineText("parked", .quiet)
        }
    }

    /// `"in 47m"` for the first window whose OWN state is near or spent and
    /// that carries a future reset; `nil` when none does.
    private func nearResetCaption(now: Date) -> String? {
        let windows: [(QuotaWindow, Int64?)] = [
            (.fiveHour, fiveHourResetAtMs), (.sevenDay, sevenDayResetAtMs),
        ]
        for (window, reset) in windows {
            guard case .state(let state) = quotaBarTintSource(for: window) else { continue }
            guard state == .near || state == .spent else { continue }
            if let caption = QuotaFormat.resetCaption(resetAtMs: reset, now: now) {
                return caption
            }
        }
        return nil
    }

    /// When this account can serve again: the LATEST reset among the windows
    /// holding it (``held``), in unix milliseconds. `nil` when nothing holds it.
    ///
    /// The latest, not ``soonestHold``: an account held by both its 5-hour and
    /// its 7-day window is still out when the 5-hour one resets, so the soonest
    /// reset would print "back in 42m" over an account that stays out for days.
    public var backAtMs: Int64? {
        held.map(\.resetAtMs).max()
    }

    /// `"back in 42m"` at the right of an out-of-tokens line, or `nil` when no
    /// window holds this account or its reset has already passed.
    public func backInCaption(now: Date) -> String? {
        guard let caption = QuotaFormat.resetCaption(resetAtMs: backAtMs, now: now) else {
            return nil
        }
        return "back \(caption)"
    }

    /// The middle of a waiting line (out of tokens, or parked): how much of
    /// the WEEK is used, which is what decides whether waiting is worth it.
    ///
    /// `never measured` for an account with no quota evidence at all
    /// (``hasQuotaEvidence``), which is a different fact from 0 % and must not
    /// look like one. `nil` for a measured account whose 7-day window has no
    /// reading: there is nothing to say, so the line says nothing.
    ///
    /// A Codex plan that reports no window says so (``lineNoWindowsText``). A
    /// Codex plan whose weekly slot is not seven days long names its own length
    /// (`30d 100% used`), because `week` would misstate the horizon the figure
    /// is measured over; a 7-day or unknown length keeps `week`, as on every
    /// Claude line.
    public var waitingLineText: String? {
        guard hasQuotaEvidence else { return "never measured" }
        if let lineNoWindowsText { return lineNoWindowsText }
        guard let sevenDay else { return nil }
        let label = windowLabel(for: .sevenDay)
        let span = label == "7d" ? "week" : label
        return "\(span) \(QuotaFormat.percent(sevenDay)) used"
    }

    /// The labels of a live line's two meter slots, 5-hour then weekly: the label
    /// a window's length gives it (``windowLabel(for:)``), or `nil` for a window
    /// the plan does not report, whose slot the line leaves blank so every bar
    /// after it still starts at one x. `["5h", "7d"]` on every Claude line.
    public var lineSlotLabels: [String?] {
        let reported = reportedWindows
        return [QuotaWindow.fiveHour, .sevenDay].map {
            reported.contains($0) ? windowLabel(for: $0) : nil
        }
    }

    /// `no usage windows`, across both meter slots of a Codex line whose plan has
    /// said it reports no window at all; `nil` on every other line. Not before a
    /// probe has said so: until then the row is unmeasured, like any other.
    public var lineNoWindowsText: String? {
        guard provider == .codex, quotaWindows == [], hasQuotaEvidence else { return nil }
        return "no usage windows"
    }

    /// The right of a waiting line, for the band it sits in. A live band has
    /// no waiting line, so it answers `nil`.
    public func waitingLineSignal(in band: FleetBand, now: Date) -> AccountLineText? {
        switch band {
        case .live:
            return nil
        case .outOfTokens:
            return backInCaption(now: now).map { AccountLineText($0, .bad) }
        case .parked:
            return AccountLineText("parked", .quiet)
        }
    }
}

extension FleetSection {
    /// `"Out of tokens · 6"`, the heading over a band of waiting lines. The
    /// view uppercases it, the way every section heading on this panel is
    /// drawn. The count is THIS section's rows, which are the lines drawn
    /// under it.
    public var bandHeadingText: String {
        "\(band.title) · \(rows.count)"
    }

    /// `"first back in 42m"` at the right of an out-of-tokens heading: the
    /// soonest ``Account/backAtMs`` among its rows that is still ahead. `nil`
    /// on any other band, and when no row carries a countdown.
    ///
    /// A reset already behind `now` is dropped before taking the minimum:
    /// left in, one stale row would hide every other row's real countdown.
    public func firstBackCaption(now: Date) -> String? {
        firstBackCountdown(now: now).map { "first back \($0)" }
    }

    /// The countdown half of ``firstBackCaption(now:)`` alone (`"in 42m"`), for
    /// a view that draws it in a colour of its own.
    public func firstBackCountdown(now: Date) -> String? {
        guard band == .outOfTokens else { return nil }
        return Self.firstBackCountdown(rows.map(\.account), now: now)
    }

    /// The soonest ``Account/backAtMs`` among `accounts` that is still ahead,
    /// as a countdown. One rule for this heading and for the strip's "next
    /// back in" (unit H3), so the two can never show different figures.
    public static func firstBackCountdown(_ accounts: [Account], now: Date) -> String? {
        let nowMs = Int64(now.timeIntervalSince1970 * 1000)
        let soonest = accounts.compactMap(\.backAtMs).filter { $0 > nowMs }.min()
        return QuotaFormat.resetCaption(resetAtMs: soonest, now: now)
    }
}
