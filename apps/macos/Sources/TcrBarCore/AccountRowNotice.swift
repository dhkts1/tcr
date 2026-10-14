import Foundation

/// One line the panel draws under an account's row to say what became of the
/// last thing done to it: a refusal in `tcr`'s own words, or something `tcr`
/// said while succeeding.
///
/// It exists because the v4 panel drew none of these. ``RemoveAccountController``,
/// ``GroupController`` and ``AccountController`` each recorded their failures
/// and the legacy row drew them, but option H's one-line rows and cards never
/// read them, so a delete `tcr` refused looked exactly like a delete that had
/// not been clicked (Gil, 2026-10-14: "remove delete account doesnt work
/// sometimes").
public struct AccountRowNotice: Equatable, Sendable, Identifiable {
    /// Whether the line reports a failure or a note on a success. The two wear
    /// different colours: a success that `tcr` commented on must not look
    /// like an error.
    public enum Tone: Equatable, Sendable {
        case failure
        case note
    }

    public let text: String
    public let tone: Tone

    public init(text: String, tone: Tone) {
        self.text = text
        self.tone = tone
    }

    public var id: String { text }

    /// Every notice for one row, failures first, in a fixed order so the lines
    /// do not swap places between two polls.
    public static func collect(
        deleteFailure: RemoveAccountCommand.Failure?,
        deleteNotice: String?,
        groupFailure: (group: String, failure: GroupCommand.Failure)?,
        toggleFailure: AccountCommand.Failure?
    ) -> [AccountRowNotice] {
        var lines: [AccountRowNotice] = []
        if let deleteFailure {
            lines.append(AccountRowNotice(text: deleteFailure.summary, tone: .failure))
        }
        if let groupFailure {
            lines.append(
                AccountRowNotice(
                    text: "group “\(groupFailure.group)”: \(groupFailure.failure.summary)",
                    tone: .failure))
        }
        if let toggleFailure {
            lines.append(AccountRowNotice(text: toggleFailure.summary, tone: .failure))
        }
        if let deleteNotice {
            lines.append(AccountRowNotice(text: deleteNotice, tone: .note))
        }
        return lines
    }
}
