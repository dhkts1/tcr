import Foundation

/// What TcrBar says under a Codex account's line about the last repair tried on it: the
/// reason tcr refused a re-login, and the warning a group add printed.
///
/// Both are kept for this app run only, in memory, and are never written anywhere. A
/// refused re-login writes nothing, so nothing that tcr reports would say why it failed:
/// once the sheet closed, the account's line would read `re-login` with no reason. A group
/// add's warning is stderr from a command that succeeded, which nothing else keeps either.
/// Each note clears once the row it was about has moved on, which each poll reconciles.
public struct CodexNotes: Equatable, Sendable {
    /// A refusal, with the row's state when it came back.
    private struct Refusal: Equatable, Sendable {
        let reason: String
        let status: String
        let loginError: String?
    }

    /// A warning, with the group whose add printed it.
    private struct Warning: Equatable, Sendable {
        let text: String
        let group: String
    }

    private var refusals: [AccountRef: Refusal] = [:]
    private var warnings: [AccountRef: Warning] = [:]

    public init() {}

    /// Keep tcr's reason for refusing this row's re-login, with the row's `status` and
    /// `loginError` as they are now: a change to either means the refusal is out of date.
    public mutating func recordReloginRefusal(_ reason: String, for account: Account) {
        refusals[account.ref] = Refusal(
            reason: reason, status: account.status, loginError: account.loginError)
    }

    /// Keep the warning a successful `tcr group add` printed for this row, without its
    /// `warning: ` prefix. A notice that warned of nothing keeps nothing.
    public mutating func recordGroupAddNotice(_ notice: String, group: String, for account: AccountRef) {
        guard let text = GroupCommand.warning(inNotice: notice) else { return }
        warnings[account] = Warning(text: text, group: group)
    }

    /// Drop every note the fleet has moved past: a refusal once its row's `status` or
    /// `loginError` differs from when it came back, a warning once its row is no longer in
    /// that group, and either once its row has left the fleet.
    public mutating func reconcile(with accounts: [Account]) {
        let rows = Dictionary(accounts.map { ($0.ref, $0) }, uniquingKeysWith: { first, _ in first })
        refusals = refusals.filter { ref, refusal in
            guard let row = rows[ref] else { return false }
            return row.status == refusal.status && row.loginError == refusal.loginError
        }
        warnings = warnings.filter { ref, warning in
            rows[ref].map { ($0.groups ?? []).contains(warning.group) } ?? false
        }
    }

    public func reloginRefusal(for account: AccountRef) -> String? { refusals[account]?.reason }

    public func groupAddWarning(for account: AccountRef) -> String? { warnings[account]?.text }
}

/// The panel's one set of ``CodexNotes``, observed so that recording a note redraws the
/// lines. The menu-bar shell owns one for the app run; a render hands one its scene's notes.
@MainActor
public final class CodexNoteStore: ObservableObject {
    @Published public private(set) var notes: CodexNotes

    public init(notes: CodexNotes = CodexNotes()) {
        self.notes = notes
    }

    public func recordReloginRefusal(_ reason: String, for account: Account) {
        notes.recordReloginRefusal(reason, for: account)
    }

    public func recordGroupAddNotice(_ notice: String, group: String, for account: AccountRef) {
        notes.recordGroupAddNotice(notice, group: group, for: account)
    }

    /// Reconciled on a copy, and published only when a note went: every poll calls this.
    public func reconcile(with accounts: [Account]) {
        var next = notes
        next.reconcile(with: accounts)
        if next != notes { notes = next }
    }
}
