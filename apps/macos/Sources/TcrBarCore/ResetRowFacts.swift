import Foundation

/// The banked-reset row an account card draws under its quota bars, the badge its one-line
/// row draws, and the words of the dialog that spends one (`docs/design/banked-resets.md`,
/// option B of the reset-row mockup; Gil, 2026-11-16).
///
/// A row is drawn only for an account that holds at least one reset, so a card with none,
/// or one whose provider has not said, looks exactly as it did. The row is one line: each
/// kind held with its count and the earliest use-by among them, then a small Reset button
/// that opens the confirm alert.
///
/// Provider-neutral: every word here comes from `[Held]`, and only `held(_:)` knows where
/// an account's resets are read from. A Codex account's are ChatGPT's banked weekly resets
/// (`resetCredits`); a Claude account's are Anthropic's grants (`bankedResets`), each a
/// weekly, full, 5-hour or Fable reset, named the way Claude Code names them.
///
/// **Unknown is not zero.** A Claude account whose provider answered "not eligible" (the
/// reason `surface` is the one measured) holds an unknown number of resets: no row, no
/// badge, and `unknownNote(_:)` says so on the 7d bar's hover. It never reads "0".
public enum ResetRowFacts {

    /// One kind an account holds: how many, and when the first of them expires.
    public struct Held: Equatable, Sendable {
        public let kind: ResetKind
        public let count: UInt64
        public let earliestExpiryMs: Int64?

        public init(kind: ResetKind, count: UInt64, earliestExpiryMs: Int64? = nil) {
            self.kind = kind
            self.count = count
            self.earliestExpiryMs = earliestExpiryMs
        }
    }

    /// Every kind the account holds at least one of, in kind order, or `nil` when its
    /// provider has not said: unknown, never empty-meaning-zero.
    public static func held(_ account: Account) -> [Held]? {
        switch account.provider {
        case .codex:
            guard let credits = account.resetCredits else { return nil }
            guard credits.availableCount > 0 else { return [] }
            return [
                Held(
                    kind: .weekly, count: credits.availableCount,
                    earliestExpiryMs: credits.earliestExpiryMs)
            ]
        case .anthropic:
            guard let bank = account.bankedResets, bank.eligible else { return nil }
            var held: [Held] = []
            for kind in ResetKind.allCases {
                let grants = bank.grants.filter { $0.kind == kind && $0.resetsLeft > 0 }
                guard !grants.isEmpty else { continue }
                held.append(
                    Held(
                        kind: kind, count: grants.reduce(0) { $0 + UInt64($1.resetsLeft) },
                        earliestExpiryMs: grants.compactMap(\.endsAtMs).min()))
            }
            return held
        }
    }

    /// Whether the account's provider said what it holds, a count of zero included.
    public static func carriesBank(_ account: Account) -> Bool {
        held(account) != nil
    }

    /// Whether the card's Reset button and the menu's reset items are offered: the account
    /// holds at least one reset.
    public static func canRedeem(_ account: Account) -> Bool {
        !(held(account) ?? []).isEmpty
    }

    /// The kinds a click can spend, in kind order: the button opens the confirm alert at
    /// once for one, and a menu asking which for several.
    public static func choices(_ account: Account) -> [ResetKind] {
        (held(account) ?? []).map(\.kind)
    }

    /// The card's one line and its help, or `nil` when the account holds no reset or its
    /// provider has not said, which draws no row at all.
    ///
    /// `3 weekly · first expires Nov 30`, `1 weekly · 2 Fable · first expires Dec 10`, or the
    /// counts alone when no expiry was sent. `timeZone` is the clock dates are printed in.
    public static func row(_ account: Account, timeZone: TimeZone = .current)
        -> (text: String, help: String)?
    {
        guard let held = held(account), !held.isEmpty else { return nil }
        var text = held.map { "\($0.count) \($0.kind.short)" }.joined(separator: " · ")
        if let first = held.compactMap(\.earliestExpiryMs).min() {
            text += " · first expires \(day(first, timeZone: timeZone))"
        }
        let lines = held.map { held -> String in
            let noun = held.count == 1 ? held.kind.noun : held.kind.noun + "s"
            var line = "\(held.count) \(noun): \(refills(held.kind))."
            if let expiry = held.earliestExpiryMs {
                let which = held.count == 1 ? "Use by" : "The first, use by"
                line += " \(which) \(moment(expiry, timeZone: timeZone))."
            }
            return line
        }
        return (text, (lines + ["Reset spends one, after you confirm."]).joined(separator: "\n"))
    }

    /// `↻3`: the badge at the end of a collapsed account's line, every kind counted, or
    /// `nil` when it holds no reset. No button: the button needs the card.
    public static func badge(_ account: Account) -> String? {
        guard let held = held(account), !held.isEmpty else { return nil }
        return "\(badgeGlyph)\(held.reduce(0) { $0 + $1.count })"
    }

    /// The badge's glyph.
    public static let badgeGlyph = "↻"
    /// What separates the badge from the line's signal: `ready · ↻3`.
    public static let badgeSeparator = " · "

    /// `banked resets unknown`, for the 7d bar's hover on a Claude account whose provider
    /// did not say what it holds; `nil` everywhere else.
    public static func unknownNote(_ account: Account) -> String? {
        guard account.provider == .anthropic, let bank = account.bankedResets, !bank.eligible
        else { return nil }
        return "banked resets unknown"
    }

    /// Why the Reset button is disabled although the account holds resets, or `nil` when one
    /// is usable now: paused, needing the limit first, or not usable yet. A Codex reset is
    /// always usable.
    public static func disabledReason(_ account: Account) -> String? {
        guard account.provider == .anthropic, let bank = account.bankedResets, bank.eligible
        else { return nil }
        let held = bank.grants.filter { $0.resetsLeft > 0 }
        guard !held.isEmpty, !held.contains(where: { $0.usableNow && !$0.paused }) else {
            return nil
        }
        if held.allSatisfy(\.paused) { return "Paused: no reset can be used right now." }
        if held.contains(where: \.requiresLimit) {
            return "Usable once this account reaches its limit."
        }
        return "Not usable yet."
    }

    /// What a kind refills, for the hover and the confirm alert.
    public static func refills(_ kind: ResetKind) -> String {
        switch kind {
        case .session: return "refills the 5-hour window"
        case .weekly: return "refills the 7-day window"
        case .full: return "refills the 5-hour and 7-day windows"
        case .fable: return "gives back what Fable used from the weekly budget"
        }
    }

    /// The confirm alert for spending one `kind` reset on a Claude account:
    ///
    ///     Use a weekly reset on alice@example.com?
    ///     Refills your 7-day limit now (46% used). Your weekly reset day stays Friday.
    ///     1 weekly reset left · use by Dec 10.
    ///
    /// A figure the row does not carry is left out rather than guessed.
    public static func confirmation(
        _ account: Account, kind: ResetKind, timeZone: TimeZone = .current
    ) -> (message: String, informative: String) {
        let message = "Use a \(kind.noun) on \(account.name)?"
        let used = { (value: Double?) in value.map { " (\(QuotaFormat.percent($0)) used)" } ?? "" }
        let stays: String =
            account.sevenDayResetAtMs.map {
                " Your weekly reset day stays \(format($0, "EEEE", timeZone: timeZone))."
            } ?? ""
        let what: String
        switch kind {
        case .session:
            what = "Refills your 5-hour limit now\(used(account.fiveHour))."
        case .weekly:
            what = "Refills your 7-day limit now\(used(account.sevenDay)).\(stays)"
        case .full:
            let both: String
            if let five = account.fiveHour, let seven = account.sevenDay {
                both =
                    " (5h \(QuotaFormat.percent(five)) · 7d \(QuotaFormat.percent(seven)) used)"
            } else {
                both = ""
            }
            what = "Refills your 5-hour and 7-day limits now\(both).\(stays)"
        case .fable:
            what = "Gives back what Fable used from your weekly limit\(used(account.sevenDayOi))."
        }
        var lines = [what]
        if let held = held(account)?.first(where: { $0.kind == kind }) {
            let noun = held.count == 1 ? kind.noun : kind.noun + "s"
            var left = "\(held.count) \(noun) left"
            if let expiry = held.earliestExpiryMs {
                left += " · use by \(day(expiry, timeZone: timeZone))"
            }
            lines.append(left + ".")
        }
        return (message, lines.joined(separator: "\n"))
    }

    /// `Nov 30`, in `timeZone`.
    private static func day(_ ms: Int64, timeZone: TimeZone) -> String {
        format(ms, "MMM d", timeZone: timeZone)
    }

    /// `Nov 30, 22:37`, in `timeZone`: the help's fuller instant.
    private static func moment(_ ms: Int64, timeZone: TimeZone) -> String {
        format(ms, "MMM d, HH:mm", timeZone: timeZone)
    }

    private static func format(_ ms: Int64, _ pattern: String, timeZone: TimeZone) -> String {
        let formatter = DateFormatter()
        formatter.dateFormat = pattern
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = timeZone
        return formatter.string(from: Date(timeIntervalSince1970: Double(ms) / 1000))
    }
}
