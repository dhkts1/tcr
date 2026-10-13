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
    public func form(carriesLease: Bool, carriesExit: Bool) -> AccountRowForm {
        if carriesLease || carriesExit { return .card }
        return band == .live ? .liveLine : .waitingLine
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
    ///    and the spoken label (``cardSummaryLabel(now:isControl:)``) carry
    ///    the rest.
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
            return AccountLineText("rejected", .bad)
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
    public var waitingLineText: String? {
        guard hasQuotaEvidence else { return "never measured" }
        guard let sevenDay else { return nil }
        return "week \(QuotaFormat.percent(sevenDay)) used"
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
        let nowMs = Int64(now.timeIntervalSince1970 * 1000)
        let soonest = rows.compactMap(\.account.backAtMs).filter { $0 > nowMs }.min()
        return QuotaFormat.resetCaption(resetAtMs: soonest, now: now)
    }
}
