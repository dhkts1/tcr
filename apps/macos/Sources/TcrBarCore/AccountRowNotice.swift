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
        /// A state the account is in, drawn red like a failure but spoken as
        /// written: a signed-out Codex login, say. It is not an action that
        /// failed, which is why it is never spoken as "Failed: …".
        case alert
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
    ///
    /// `codexNote` is a Codex row's own note (``CodexCardFacts/noteLine(reloginRefusal:loginError:codexHome:groupAddWarning:home:)``),
    /// after the failures and before the note on a success: a state the account
    /// is in outranks a remark on something that worked. `nil` on every Claude
    /// row, which therefore gets exactly the lines it always did.
    ///
    /// A failure that carries the refusal of a bundled tcr too old to read a
    /// config holding a Codex account reads as the sentence naming that tcr
    /// (``PredatesCodexRefusal/displayed(_:)``), since tcr's own words name a
    /// version and not the fix. Such a tcr refuses every command with the same
    /// words, so a line whose text an earlier line already has is dropped: the
    /// row says it once. Lines of different texts are all kept, as before, and
    /// two failures of tcr's own words never share a text, because each starts
    /// with its own prefix.
    public static func collect(
        deleteFailure: RemoveAccountCommand.Failure?,
        deleteNotice: String?,
        groupFailure: (group: String, failure: GroupCommand.Failure)?,
        toggleFailure: AccountCommand.Failure?,
        codexNote: AccountRowNotice? = nil
    ) -> [AccountRowNotice] {
        var lines: [AccountRowNotice] = []
        if let deleteFailure {
            lines.append(failure(deleteFailure.summary))
        }
        if let groupFailure {
            lines.append(failure("group “\(groupFailure.group)”: \(groupFailure.failure.summary)"))
        }
        if let toggleFailure {
            lines.append(failure(toggleFailure.summary))
        }
        if let codexNote {
            lines.append(codexNote)
        }
        if let deleteNotice {
            lines.append(AccountRowNotice(text: deleteNotice, tone: .note))
        }
        var seen = Set<String>()
        return lines.filter { seen.insert($0.text).inserted }
    }

    private static func failure(_ text: String) -> AccountRowNotice {
        AccountRowNotice(text: PredatesCodexRefusal.displayed(text), tone: .failure)
    }
}
