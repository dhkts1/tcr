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

    /// The card's one line: each kind held with its count, then the earliest use-by.
    public struct Row: Equatable, Sendable {
        /// `1 weekly · 2 Fable`: may be cut at its end when the card is narrow.
        public let kinds: String
        /// `Fable expires Dec 10`: the earliest use-by among the kinds held, named with
        /// the kind it belongs to when the account holds more than one kind (Gil,
        /// 2026-11-17: "show which one is the expiring"), or `nil` when none was sent.
        /// Within `soonMs` of it the day becomes the time: `Fable expires today 18:00`.
        /// Never cut (Gil, 2026-11-16): the card cuts the kinds instead.
        public let date: String?
        /// The first use-by is less than `soonMs` away: the card draws the date, and the
        /// collapsed line its badge, in the warning ink.
        public let soon: Bool
        /// The date slot says a reset tcr spent by itself (`Fable auto-used`), drawn in the
        /// ready ink instead of the date, for the day the server keeps the record.
        public let autoUsed: Bool
        /// Every kind in full, what it refills and its use-by.
        public let help: String

        /// `1 weekly · 2 Fable · Dec 10`: the whole line, as it reads uncut.
        public var text: String { date.map { "\(kinds) · \($0)" } ?? kinds }
    }

    /// How close a use-by must be for the row and badge to warn: one day.
    public static let soonMs: Int64 = 86_400_000

    /// The kind whose reset expires first, or `nil` when no use-by was sent. A tie goes to
    /// the first in kind order.
    public static func expiringFirst(_ held: [Held]) -> Held? {
        held.filter { $0.earliestExpiryMs != nil }
            .min { ($0.earliestExpiryMs ?? .max) < ($1.earliestExpiryMs ?? .max) }
    }

    /// Whether `expiry` is still ahead and less than `soonMs` away.
    static func isSoon(_ expiry: Int64, now: Date) -> Bool {
        let left = expiry - Int64(now.timeIntervalSince1970 * 1000)
        return left > 0 && left < soonMs
    }

    /// The card's one line and its help, or `nil` when the account holds no reset or its
    /// provider has not said, which draws no row at all. `timeZone` is the clock dates
    /// are printed in.
    public static func row(
        _ account: Account, now: Date = Date(), timeZone: TimeZone = .current
    ) -> Row? {
        guard let held = held(account), !held.isEmpty else { return nil }
        let kinds = held.map { "\($0.count) \($0.kind.short)" }.joined(separator: " · ")
        let first = expiringFirst(held)
        var date: String?
        var soon = false
        if let first, let expiry = first.earliestExpiryMs {
            soon = isSoon(expiry, now: now)
            let subject = held.count > 1 ? "\(first.kind.short) expires" : "expires"
            let when = soon ? near(expiry, now: now, timeZone: timeZone) : day(expiry, timeZone: timeZone)
            date = "\(subject) \(when)"
        }
        // A reset tcr spent by itself takes the date's place for the day, unless another
        // reset is itself about to expire: that warning matters more.
        let auto = account.autoReset
        let autoUsed = auto?.outcome == .applied && !soon
        if let auto, autoUsed { date = "\(auto.kind.short) auto-used" }
        let lines = held.map { held -> String in
            let noun = held.count == 1 ? held.kind.noun : held.kind.noun + "s"
            var line = "\(held.count) \(noun): \(refills(held.kind))."
            if let expiry = held.earliestExpiryMs {
                let which = held.count == 1 ? "Use by" : "The first, use by"
                line += " \(which) \(moment(expiry, timeZone: timeZone))."
                if first?.kind == held.kind && kinds.contains(" · ") { line += " Expires first." }
            }
            return line
        }
        var tail = ["Reset spends one, after you confirm."]
        if let auto {
            tail.append(autoNote(auto, timeZone: timeZone))
        } else if held.contains(where: { $0.earliestExpiryMs != nil }) {
            tail.append("tcr uses a reset itself 2 hours before it would expire.")
        }
        return Row(
            kinds: kinds, date: date, soon: soon, autoUsed: autoUsed,
            help: (lines + tail).joined(separator: "\n"))
    }

    /// The hover's sentence on what tcr did by itself, in fixed words.
    static func autoNote(_ auto: AutoReset, timeZone: TimeZone) -> String {
        let at = format(auto.atMs, "HH:mm", timeZone: timeZone)
        let noun = auto.kind.noun
        switch auto.outcome {
        case .applied:
            return "tcr used a \(noun) at \(at), before it expired."
        case .needsLimit:
            return
                "tcr could not use the \(noun) at \(at): it works only once this account reaches its limit."
        case .notUsableYet:
            return "tcr could not use the \(noun) at \(at): it was not usable yet."
        case .alreadyUsed:
            return "tcr found the \(noun) already used at \(at)."
        case .noneLeft:
            return "tcr found no \(noun) left at \(at)."
        case .failed:
            return "tcr could not use the \(noun) at \(at); the tcr log has why."
        }
    }

    /// Whether the collapsed line's badge warns: a reset expires within `soonMs`.
    public static func badgeSoon(_ account: Account, now: Date = Date()) -> Bool {
        guard let held = held(account), let expiry = expiringFirst(held)?.earliestExpiryMs else {
            return false
        }
        return isSoon(expiry, now: now)
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

    /// `today 22:37` or `tomorrow 09:00`, in `timeZone`: a use-by under a day away.
    private static func near(_ ms: Int64, now: Date, timeZone: TimeZone) -> String {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = timeZone
        let date = Date(timeIntervalSince1970: Double(ms) / 1000)
        let word = calendar.isDate(date, inSameDayAs: now) ? "today" : "tomorrow"
        return "\(word) \(format(ms, "HH:mm", timeZone: timeZone))"
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
