import Foundation

/// Everything a row's menus and card may offer. One value decides them, so the gear,
/// the right-click menu and the card's own lines cannot disagree.
///
/// Not `CaseIterable`, on purpose: a later action is one more line here and one more
/// member of the sets in ``Account/availableActions``, and nothing enumerates the cases
/// to build a menu.
public enum AccountAction: Hashable, Sendable {
    case toggleEnabled, relogin, controlAccount, copyName, copyAccessToken, mintToken
    case groupStatement, copyRunGroup, mintGroupTokens, parkGroup, removeFromGroup, addToGroup
    case deleteAccount, exitPicker, exitStatement, lentToLine, lend
    /// Take the login the codex folder a row was imported from holds now.
    case reimport
}

extension Account {
    /// What this row may offer, by provider.
    ///
    /// A Claude row is offered everything it was offered before Codex accounts
    /// existed. A Codex row is not offered the four Claude concepts: a control
    /// account, a minted long-lived token, `tcr run --group` and a group mint. Each of
    /// them can only fail on a Codex row, and an item that can only fail is worse than
    /// none. A Codex request always leaves from this Mac, so a Codex card states that
    /// instead of offering an exit picker, and a Codex account is never inside a lease.
    ///
    /// Re-login is offered on both: one verb for both providers, so the state word and
    /// the action word match on every row. Re-import is offered only on a Codex row that
    /// names the codex folder it was imported from, because that folder is what it reads.
    public var availableActions: Set<AccountAction> {
        switch provider {
        case .anthropic: return AccountActions.claudeRow
        case .codex:
            guard codexHome != nil else { return AccountActions.codexRow }
            return AccountActions.codexRow.union([.reimport])
        }
    }

    /// The sign-in Re-login runs for this row: its own provider's, naming the row, so the
    /// sign-in can refuse a browser that hands back a different account.
    public var reloginRequest: LoginRequest {
        switch provider {
        case .anthropic: return .claude(account: name)
        case .codex: return .codex(account: name)
        }
    }

    /// The import Re-import runs for this row, from the codex folder it names, or `nil` on
    /// a row that names none.
    public var reimportRequest: LoginRequest? {
        guard provider == .codex, let codexHome else { return nil }
        return .codexImport(home: codexHome, account: name)
    }
}

/// One item of a group's submenu on an account row.
public enum GroupMenuItem: Hashable, Sendable {
    /// A disabled line that states a fact about the group, with nothing to click.
    case statement(String)
    /// Copy `tcr run --group <group>`.
    case copyRun
    case removeFromGroup
    case deleteGroup
    /// Mint a long-lived token for every member the mint can serve.
    case mintGroup(title: String)
    /// Park the group, or unpark it, with its state on the item.
    case park(title: String, parked: Bool)
}

/// One group the Add to group… submenu lists for a row.
public struct AddToGroupEntry: Hashable, Sendable {
    /// The group's name, which is what `tcr group add` takes.
    public let group: String
    /// What the item reads.
    public let title: String
    /// Whether choosing it does anything; a reserved group on a Codex row does not.
    public let isEnabled: Bool
    /// Whether a "Copy “tcr group add …”" entry follows it.
    public let offersCopy: Bool

    public init(group: String, title: String, isEnabled: Bool, offersCopy: Bool) {
        self.group = group
        self.title = title
        self.isEnabled = isEnabled
        self.offersCopy = offersCopy
    }
}

/// The words and items of a row's group menus, decided from ``Account/availableActions``.
public enum AccountActions {
    /// Every action, for a Claude row.
    static let claudeRow: Set<AccountAction> = [
        .toggleEnabled, .relogin, .controlAccount, .copyName, .copyAccessToken, .mintToken,
        .copyRunGroup, .mintGroupTokens, .parkGroup, .removeFromGroup, .addToGroup,
        .deleteAccount, .exitPicker, .lentToLine, .lend,
    ]

    /// What works on a Codex row, plus the two statements that say what does not.
    static let codexRow: Set<AccountAction> = [
        .toggleEnabled, .relogin, .copyName, .copyAccessToken, .groupStatement, .parkGroup,
        .removeFromGroup, .addToGroup, .deleteAccount, .exitStatement,
    ]

    /// The first line of a group's submenu on a Codex row: why it offers no
    /// `tcr run --group` and no group mint.
    public static let codexGroupStatement = "Codex sessions can't ask for a group"

    /// The submenu of one group this row belongs to, in order.
    ///
    /// 1. On a Codex row, ``codexGroupStatement``.
    /// 2. The routing line, only when the group has a Claude member: Claude routing is
    ///    judged over the members that can serve a Claude request
    ///    (``GroupRouting/routes(group:accounts:controlName:)``), and a group with none
    ///    has nothing for the line to be about.
    /// 3. Copy `tcr run --group`, and the group mint, named for Claude in a group with
    ///    Codex members, because only its Claude accounts can be minted.
    /// 4. Park or Unpark, with both providers counted in a group that spans them:
    ///    parking holds every member back, Claude and Codex alike.
    /// 5. Remove from the group, and delete the group for everyone.
    public static func groupItems(
        for account: Account, group: String, accounts: [Account], controlName: String?
    ) -> [GroupMenuItem] {
        let actions = account.availableActions
        let members = accounts.filter { ($0.groups ?? []).contains(group) }
        var items: [GroupMenuItem] = []
        if actions.contains(.groupStatement) {
            items.append(.statement(codexGroupStatement))
        }
        if members.contains(where: { $0.provider == .anthropic }) {
            if !GroupRouting.routes(group: group, accounts: accounts, controlName: controlName) {
                items.append(.statement("⚠︎ “\(group)” routes nothing — control account only"))
            } else if GroupRouting.allowsControlAccount(group: group, accounts: accounts) {
                items.append(.statement("“\(group)” may use the control account"))
            }
        }
        if actions.contains(.copyRunGroup) {
            items.append(.copyRun)
        }
        if actions.contains(.mintGroupTokens) {
            let title =
                members.contains(where: { $0.provider == .codex })
                ? "Mint Long-Lived Tokens for “\(group)”’s Claude Accounts…"
                : "Mint Long-Lived Tokens for “\(group)”…"
            items.append(.mintGroup(title: title))
        }
        if actions.contains(.parkGroup) {
            // Read from the fleet, not this row: a group is parked or not for
            // everyone, and the wire may omit the label on one member.
            let parked = accounts.contains { ($0.parkedGroups ?? []).contains(group) }
            var title = parked ? "Unpark group “\(group)”" : "Park group “\(group)”"
            if let summary = GroupSummary.summarize(accounts).first(where: { $0.name == group }),
                summary.spansBothProviders
            {
                title += " (\(summary.claudeMemberCount) Claude, \(summary.codexMemberCount) Codex)"
            }
            items.append(.park(title: title, parked: parked))
        }
        if actions.contains(.removeFromGroup) {
            items.append(.removeFromGroup)
        }
        items.append(.deleteGroup)
        return items
    }

    /// Every group this account is not already a member of, fleet-wide, sorted: the
    /// population the "Add to group…" submenu offers. Its "New group…" entry is
    /// always there above them, so a fleet with no group at all yet still offers one,
    /// and the submenu never reads as "this row cannot be added to a group".
    ///
    /// On a Codex row, a group any account reserves is listed disabled, with no copy
    /// entry: a reserved group serves only requests that ask for it, and no Codex
    /// request can ask for a group. A Claude row is offered every group as it always
    /// was, a reserved one included.
    public static func addToGroupEntries(for account: Account, accounts: [Account])
        -> [AddToGroupEntry]
    {
        let existing = Set(account.groups ?? [])
        let everyGroup = Set(accounts.flatMap { $0.groups ?? [] })
        let reserved = Set(accounts.flatMap { $0.reservedGroups ?? [] })
        let isCodex = account.provider == .codex
        return everyGroup.subtracting(existing).sorted().map { group in
            guard isCodex, reserved.contains(group) else {
                return AddToGroupEntry(group: group, title: group, isEnabled: true, offersCopy: true)
            }
            return AddToGroupEntry(
                group: group, title: "\(group) (reserved: no Codex request can reach it)",
                isEnabled: false, offersCopy: false)
        }
    }
}
