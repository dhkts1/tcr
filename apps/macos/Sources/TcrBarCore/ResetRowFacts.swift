import Foundation

/// The banked-reset row an account card draws under its quota bars, and the badge its
/// one-line row draws (Gil, 2026-11-16: option B of
/// `docs/design/codex-weekly-reset-mockup.html`).
///
/// A row is drawn only for an account that holds at least one reset, so a card with
/// none, or one whose server has not said, looks exactly as it did. The row is one line:
/// how many resets the account holds and when the first expires, then a small Reset
/// button that opens the same confirm alert the card's menu item does.
///
/// Provider-neutral: every word here comes from a ``Bank``, and only ``bank(_:)`` knows
/// where an account's resets are read from. A Codex account's are ChatGPT's banked weekly
/// resets (`resetCredits`).
public enum ResetRowFacts {

    /// What one account holds: the resets it can spend now, and when the first of them
    /// expires (unix milliseconds), when the server said.
    public struct Bank: Equatable, Sendable {
        public let count: UInt64
        public let earliestExpiryMs: Int64?

        public init(count: UInt64, earliestExpiryMs: Int64? = nil) {
            self.count = count
            self.earliestExpiryMs = earliestExpiryMs
        }
    }

    /// The account's resets, or `nil` when its server has not said: unknown, never zero.
    public static func bank(_ account: Account) -> Bank? {
        switch account.provider {
        case .codex:
            guard let credits = account.resetCredits else { return nil }
            return Bank(count: credits.availableCount, earliestExpiryMs: credits.earliestExpiryMs)
        case .anthropic:
            return nil
        }
    }

    /// Whether the account's server said anything about its resets, a count of zero
    /// included: known, as opposed to unknown.
    public static func carriesBank(_ account: Account) -> Bool {
        bank(account) != nil
    }

    /// Whether the card's Reset button and the menu's "Redeem reset" are offered: the
    /// account holds at least one reset.
    public static func canRedeem(_ account: Account) -> Bool {
        (bank(account)?.count ?? 0) > 0
    }

    /// The card's one line and its help, or `nil` when the account holds no reset or its
    /// server has not said, which draws no row at all.
    ///
    /// `3 weekly · first expires Nov 30` (the line `docs/design/banked-resets.md`
    /// specifies), or the count alone when no expiry was sent. `timeZone` is the clock the date is
    /// printed in.
    public static func row(_ account: Account, timeZone: TimeZone = .current)
        -> (text: String, help: String)?
    {
        guard let bank = bank(account), bank.count > 0 else { return nil }
        var text = "\(bank.count) weekly"
        let noun = bank.count == 1 ? "weekly reset" : "weekly resets"
        var help = "\(bank.count) banked \(noun): each refills the 7-day window now."
        if let expiry = bank.earliestExpiryMs {
            text += " · first expires \(day(expiry, timeZone: timeZone))"
            let which = bank.count == 1 ? "It" : "The first"
            help += " \(which) can be used until \(moment(expiry, timeZone: timeZone))."
        }
        return (text, help + " Reset spends one, after you confirm.")
    }

    /// `↻3`: the badge at the end of a collapsed account's line, or `nil` when it holds
    /// no reset. No button: the button needs the card.
    public static func badge(_ account: Account) -> String? {
        guard let bank = bank(account), bank.count > 0 else { return nil }
        return "\(badgeGlyph)\(bank.count)"
    }

    /// The badge's glyph.
    public static let badgeGlyph = "↻"
    /// What separates the badge from the line's signal: `ready · ↻3`.
    public static let badgeSeparator = " · "

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
