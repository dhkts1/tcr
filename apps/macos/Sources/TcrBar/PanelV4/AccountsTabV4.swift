import SwiftUI
import TcrBarCore

/// The Accounts tab, in the mockup's own DOM order: the loose cards first, then
/// one `.grp` box per named group, then the tab's action row.
///
/// The order is the one delta the sheet and the shipping panel disagreed about:
/// the mockup runs loose cards, then the parked group, then the collapsed one,
/// while the panel drew its groups first and the ungrouped pile between them. Loose cards lead here — they are the accounts
/// that answer "what can serve right now" with no label to read first.
///
/// Then the groups, in ``Fleet/sectionsInDisplayOrder(pinning:)``'s order
/// (unchanged, still unit-tested there) but with the ones that are drawn as a
/// single summary line LAST. That is the mockup's own order —
/// `HENRY-TOKEN · PARKED` with its member cards, then the collapsed
/// `ORCHARD · ACTIVE` — and it is a rule, not a transcription: a box holding
/// one line is a lid, and a lid between two open boxes reads as the end of the
/// list.
struct AccountsTabV4<Menu: View, Actions: View>: View {
    let fleet: Fleet
    let controlName: String?
    /// Group tokens the operator has opened. Same store the pre-v4 panel used,
    /// so a group left open stays open across this migration.
    let expandedGroups: Set<String>
    let onToggleGroup: (String) -> Void
    let now: Date
    /// Who is drawing on each account label, from `tcr peer ls --json`'s
    /// `lentTo` map. Empty when no account is inside a lease, and empty
    /// against a `tcr` that does not send the map yet, which draws no line on
    /// any card.
    var lentTo: [String: [PeerLentToEntry]] = [:]
    /// Where each account exits from, keyed by account label. Empty draws no
    /// row on any card.
    var exits: [String: AccountExit] = [:]
    /// The trusted Macs the exit picker may name.
    var exitPeers: [String] = []
    /// The rows behind those names, so a pinned Mac is drawn as the operator
    /// knows it rather than as its wire id. See ``AccountExitRow/peerRows``.
    var exitPeerRows: [PeerListDocument.PeerEntry] = []
    /// Still controls instead of menus and switches, for `--render-states`.
    var snapshotMode: Bool = false
    /// The write: which account, where it exits from, and whether that is a
    /// promise. One closure rather than two, because both halves are one
    /// `tcr peer account` call and a half-written pin is a state nobody chose.
    var onSetExit: (String, AccountExit.Route, Bool) -> Void = { _, _, _ in }
    /// Opens a lender's sheet in Settings > Peers.
    var onOpenLender: (String) -> Void = { _ in }
    /// The row whose hover card is showing (``FleetSectionRow/id``), whose
    /// line draws `.hov`'s fill. The card itself hangs from the panel root
    /// (`AccountHoverCard.swift`), not from here.
    var hoveredRow: String? = nil
    /// The account whose segment the pointer is on in the strip above the list
    /// (unit H3), by ``Account/id``: every line of that account draws `.hov`'s
    /// fill, as a hovered line does, and no hover card opens.
    var litAccount: String? = nil
    /// The accounts whose card the operator keeps open ("Keep Card Open" in
    /// the row's right-click menu): each draws its hover card's content in
    /// its own place in its box, and opens no floating card.
    var keptOpen = KeptOpenCards()
    /// The pointer entering (`true`) or leaving a line, by row identity.
    var onHoverLine: (String, Bool) -> Void = { _, _ in }
    /// What became of the last action on each account, drawn under its line
    /// or card (``RowNotices``). Empty for every account by default.
    var notices: (Account) -> [AccountRowNotice] = { _ in [] }
    /// The row's own actions, as a context menu — the SECOND route to them,
    /// from the one definition in ``AccountRow``.
    @ViewBuilder var menu: (Account) -> Menu
    /// The same actions as visible controls in the card's trailing slot: the
    /// actions menu, and `Re-login…` on a broken account.
    ///
    /// The mockup draws no per-card gear (`delta-list.md` #27) and the
    /// transcription took that literally, which left `.contextMenu` as the
    /// card's ONLY interaction: seven per-account actions, two of them
    /// destructive, reachable by right-click alone and by no keyboard or
    /// VoiceOver path at all.
    @ViewBuilder var actions: (Account) -> Actions

    /// How many accounts a parked group shows before its "Show N more accounts"
    /// button — round 2's approved mockup draws two of HENRY-TOKEN's five
    /// ("Show 3 more accounts"), one fewer than round 1's three. Each of the
    /// two is one line now, saying its own week used.
    static var parkedVisibleRows: Int { 2 }

    /// ``Fleet/accountsTabSections(pinning:expandedGroups:)``, the order the
    /// strip's segments read too.
    private var sections: [FleetSection] {
        fleet.accountsTabSections(pinning: controlName, expandedGroups: expandedGroups)
    }

    /// A group the operator has not opened and that the fleet says collapses by
    /// default: one line, its tally, and a button.
    private func isSummarised(_ section: FleetSection) -> Bool {
        section.isSummarised(expandedGroups: expandedGroups)
    }

    /// The strip over this tab (unit H3): one segment per account, in the
    /// order this view draws its rows, from the same sections, caps and forms.
    /// A fleet holding Codex accounts also gets one headline row per pool.
    static func strip(
        fleet: Fleet, controlName: String?, expandedGroups: Set<String>,
        lentTo: [String: [PeerLentToEntry]], exits: [String: AccountExit],
        keptOpen: KeptOpenCards, now: Date
    ) -> FleetStrip {
        // A kept-open card is drawn in its line's own place, so only the tall
        // card moves a row to the end of its section.
        let rows = fleet.accountsTabSections(pinning: controlName, expandedGroups: expandedGroups)
            .rowsInDrawOrder(expandedGroups: expandedGroups, parkedCap: parkedVisibleRows) {
                form($0, lentTo: lentTo, exits: exits, keptOpen: keptOpen) == .card
            }
        return FleetStrip(drawnRows: rows, now: now, pools: fleet.providerPools(now: now))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(sections.enumerated()), id: \.element.id) { index, section in
                if section.group == .ungrouped {
                    looseLines(section, first: index == 0)
                } else {
                    group(section, first: index == 0)
                }
            }
        }
    }

    /// The ungrouped accounts of one band, one line each (option H, Gil
    /// 2026-10-14): live ones in a box with no heading, out-of-tokens and
    /// parked ones under a band heading that counts them.
    ///
    /// The heading is drawn here although ``Array/soleGroupInBand(at:)``
    /// suppresses it in the legacy panel. That rule exists so a band heading
    /// never stacks over a group legend that already names the band; an
    /// ungrouped section has no legend, so without this heading a waiting
    /// line would sit under nothing that says why it is waiting.
    ///
    /// An account carrying a lease line or an exit picker keeps today's full
    /// card instead of a line (``FleetSectionRow/form(carriesLease:carriesExit:)``
    /// has the ruling and its reason).
    @ViewBuilder
    private func looseLines(_ section: FleetSection, first: Bool) -> some View {
        let lines = section.rows.filter { form($0) != .card }
        let cards = section.rows.filter { form($0) == .card }
        VStack(alignment: .leading, spacing: 0) {
            if section.band != .live {
                BandHeading(section: section, now: now)
            }
            if !lines.isEmpty {
                lineRuns(
                    lines, band: section.band, grouped: false,
                    marginBottom: V4.lineBoxMarginBottom)
            }
            ForEach(cards) { row in
                VStack(alignment: .leading, spacing: 0) {
                    card(row, shape: .full)
                    RowNotices(notices: notices(row.account))
                }
                .padding(.bottom, V4.lineBoxMarginBottom)
            }
        }
        // The first block under the tab strip collapses its margin into the
        // strip's, as a card there always has.
        .padding(.top, first ? V4.marginAfterStrip(V4.cardGap) : .zero)
    }

    /// The one-line rows of a section, with each kept-open account's card in
    /// its own place among them (``Swift/Array/lineRuns(isKeptOpen:)``): the
    /// lines before it close their box, the card follows at the box's width,
    /// and the lines after it open a new box. The stretches stand apart by the
    /// gap the boxes and cards already keep where they sit, and the last one
    /// takes `marginBottom`. With nothing kept open this is the one box, as
    /// before.
    private func lineRuns(
        _ rows: [FleetSectionRow], band: FleetBand, grouped: Bool, marginBottom: CGFloat
    ) -> some View {
        let runs = rows.lineRuns { form($0) == .keptOpenCard }
        let gap = grouped ? V4.groupCardGap : V4.lineBoxMarginBottom
        return VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(runs.enumerated()), id: \.element.id) { index, run in
                let bottom = index == runs.count - 1 ? marginBottom : gap
                switch run {
                case .lines(let lines):
                    lineBox(lines, band: band, grouped: grouped, marginBottom: bottom)
                case .keptOpen(let row):
                    keptOpenCard(row, grouped: grouped)
                        .padding(.bottom, bottom)
                }
            }
        }
    }

    /// A kept-open account: its hover card's content, from the same view the
    /// floating card draws (``AccountHoverCardContent``), so the two cannot
    /// drift. It publishes no hover anchor, so pointing at it opens no
    /// floating copy of itself; pointing at its strip segment lights it the
    /// way a line is lit, in `.hov`'s fill.
    private func keptOpenCard(_ row: FleetSectionRow, grouped: Bool) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            AccountHoverCardContent(
                row: row, shape: grouped ? .compact : .full, now: now,
                snapshotMode: snapshotMode,
                lit: litAccount == row.account.id,
                namesProvider: fleet.holdsCodex
            ) {
                actions(row.account)
            } menu: {
                menu(row.account)
            }
            RowNotices(notices: notices(row.account))
        }
    }

    /// One box of one-line rows, the same box on the tab and inside a group:
    /// the live line or the waiting line, each with its hover card (unit H2).
    ///
    /// A line is hovered and anchored by its ROW's identity, not by its
    /// account's name: an account in two groups draws two lines, and keyed by
    /// name the card would hang from whichever line published last and both
    /// lines would draw the hover fill.
    ///
    /// A live line's name column is the panel's one column
    /// (``liveLineFit``), less the group's padding inside a group box, so
    /// its bars start at the same x as the lines above and below it.
    private func lineBox(
        _ rows: [FleetSectionRow], band: FleetBand, grouped: Bool, marginBottom: CGFloat
    ) -> some View {
        let fit = liveLineFit
        let nameWidth = fit.nameColumn - (grouped ? V4.lineRoom.groupInset : 0)
        return LineBox(
            items: rows,
            paddingH: band == .live ? V4.liveBoxPaddingH : V4.waitBoxPaddingH,
            marginBottom: marginBottom
        ) { row in
            switch form(row) {
            // The notices sit inside the hovered span, so a hover card hangs
            // below them rather than over the reason a click did not work.
            case .liveLine:
                VStack(alignment: .leading, spacing: 0) {
                    AccountLineRow(
                        account: row.account, now: now, isControl: row.isControl,
                        snapshotMode: snapshotMode, nameWidth: nameWidth,
                        namesProvider: fleet.holdsCodex,
                        labelWidth: fit.labelWidth, barWidth: fit.barWidth
                    ) { menu(row.account) }
                    RowNotices(notices: notices(row.account))
                }
                .hoverableLine(row, hovered: lit(row), bleed: .zero) {
                    onHoverLine(row.id, $0)
                }
            case .waitingLine:
                VStack(alignment: .leading, spacing: 0) {
                    WaitingLineRow(
                        account: row.account, band: row.band, now: now,
                        isControl: row.isControl, snapshotMode: snapshotMode,
                        namesProvider: fleet.holdsCodex
                    ) { menu(row.account) }
                    RowNotices(notices: notices(row.account))
                }
                .hoverableLine(
                    row, hovered: lit(row), bleed: V4.waitBoxPaddingH
                ) { onHoverLine(row.id, $0) }
            case .card, .keptOpenCard:
                // Never reached: the callers pass only rows whose form is a
                // line, and draw the cards themselves (``lineRuns``).
                EmptyView()
            }
        }
    }

    /// The live lines' widths, ONE set for the whole panel.
    private struct LiveLineFit {
        /// The name column of a live line outside a group.
        let nameColumn: CGFloat
        /// The meters' label column.
        let labelWidth: CGFloat
        /// The bar.
        let barWidth: CGFloat
    }

    /// The name column of a live line outside a group, ONE for the whole panel
    /// (lead ruling on H1b, 2026-10-14). Measured over the live lines that are
    /// drawn: a group closed to its summary line has none, and counting its
    /// members would truncate the names on screen to make room for lines that
    /// are not.
    ///
    /// The signals of the same lines size the column at the right, and what a
    /// short one leaves goes to the names (lead ruling, 2026-10-14).
    ///
    /// The labels of the same lines size the meters' label column and the bar:
    /// a Codex window labelled by its length (`30d`) widens the label column for
    /// every line, and the bar gives it the room, so every bar still starts at
    /// one x. A panel whose lines draw only `5h` and `7d` gets main's widths.
    private var liveLineFit: LiveLineFit {
        var loose: [String] = []
        var grouped: [String]?
        var signals: [String] = []
        var labels = AccountLineFit.labels
        for section in sections where !isSummarised(section) {
            for row in section.rows where form(row) == .liveLine {
                let name = AccountName.localPart(row.account.name)
                if section.group == .ungrouped {
                    loose.append(name)
                } else {
                    grouped = (grouped ?? []) + [name]
                }
                signals.append(row.account.liveSignal(now: now).text)
                for label in row.account.lineSlotLabels.compactMap({ $0 })
                where !labels.contains(label) {
                    labels.append(label)
                }
            }
        }
        let bar = V4.lineBarWidth(labels: labels)
        return LiveLineFit(
            nameColumn: AccountLineFit.nameColumn(
                looseNames: loose, groupedNames: grouped, signals: signals, bar: bar,
                V4.lineRoom, V4.lineMetrics, labels: labels),
            labelWidth: AccountLineFit.labelColumn(V4.lineMetrics, labels: labels),
            barWidth: bar)
    }

    /// Which form this row takes, from what only this view knows: whether the
    /// account carries a lease line or an exit row.
    private func form(_ row: FleetSectionRow) -> AccountRowForm {
        Self.form(row, lentTo: lentTo, exits: exits, keptOpen: keptOpen)
    }

    private static func form(
        _ row: FleetSectionRow, lentTo: [String: [PeerLentToEntry]],
        exits: [String: AccountExit], keptOpen: KeptOpenCards
    ) -> AccountRowForm {
        row.form(
            carriesLease: !PeerLease.leases(forAccountLabel: row.account.name, in: lentTo)
                .isEmpty,
            carriesExit: PeerLease.exit(forAccountLabel: row.account.name, in: exits) != nil,
            keptOpen: keptOpen)
    }

    /// Whether `account` draws the tall card whatever the operator chooses
    /// (a lease line or an exit row), so its menu offers no "Keep Card Open":
    /// that card is open already.
    static func keepsTallCard(
        _ account: Account, lentTo: [String: [PeerLentToEntry]], exits: [String: AccountExit]
    ) -> Bool {
        !PeerLease.leases(forAccountLabel: account.name, in: lentTo).isEmpty
            || PeerLease.exit(forAccountLabel: account.name, in: exits) != nil
    }

    /// Whether a line draws `.hov`'s fill: its own hover card is showing, or
    /// the pointer is on its account's segment in the strip.
    private func lit(_ row: FleetSectionRow) -> Bool {
        hoveredRow == row.id || litAccount == row.account.id
    }

    private func card(_ row: FleetSectionRow, shape: AccountCard<Actions>.Shape) -> some View {
        AccountCard(
            account: row.account, shape: shape, now: now, isControl: row.isControl,
            namesProvider: fleet.holdsCodex,
            // Looked up through ``PeerLease/leases(forAccountLabel:in:)``, not
            // with a subscript: the CLI masks any label its sanitizer refuses,
            // so an email-labelled account arrives under one shared `[masked]`
            // key that names several accounts and identifies none. That helper
            // refuses to hand it to anybody, rather than drawing one account's
            // lease on another's card.
            lentTo: PeerLease.leases(forAccountLabel: row.account.name, in: lentTo),
            onOpenLender: onOpenLender,
            exit: PeerLease.exit(forAccountLabel: row.account.name, in: exits),
            exitPeers: exitPeers,
            exitPeerRows: exitPeerRows,
            snapshotMode: snapshotMode,
            onChooseExit: { route in
                onSetExit(
                    row.account.name, route,
                    PeerLease.exit(forAccountLabel: row.account.name, in: exits)?.strict
                        ?? false)
            },
            onToggleExitMust: { must in
                guard
                    let current = PeerLease.exit(
                        forAccountLabel: row.account.name, in: exits)
                else { return }
                onSetExit(row.account.name, current.route, must)
            }
        ) {
            actions(row.account)
        }
        .contextMenu { menu(row.account) }
    }

    @ViewBuilder
    private func group(_ section: FleetSection, first: Bool) -> some View {
        let expanded = expandedGroups.contains(section.group.token)
        let summarised = isSummarised(section)
        let capped = section.isWhollyParked && !expanded
        let visible =
            capped ? Array(section.rows.prefix(Self.parkedVisibleRows)) : Array(section.rows)
        let hidden = section.rows.count - visible.count

        GroupBox(
            legend: section.legendText,
            color: section.outlineColor.map(V4.groupColor),
            collapsed: summarised
        ) {
            if summarised {
                // The whole of a collapsed live group: one line and its tally.
                // Every string here is a ``FleetSection`` property, so what is
                // drawn cannot drift from what the group holds.
                V4Row {
                    DimText(text: section.collapsedSummaryLine)
                } trailing: {
                    HStack(spacing: V4.pillGap) {
                        ForEach(section.breakdown, id: \.kind.token) { tally in
                            V4Pill(text: tally.label, role: pillRole(tally.kind))
                        }
                        // The control account can be a member of a group
                        // small enough to collapse. Its own card is gone —
                        // the whole point of collapsing — so the one line
                        // left is where the fact has to live, after the
                        // tallies it did not change.
                        if section.containsControl {
                            V4Pill(
                                text: "Control",
                                help: "The control account for this fleet is inside this group.")
                        }
                    }
                }
            } else {
                // No summary line over a parked group's lines any more (lead
                // ruling on H1b, 2026-10-14): it said "worst 5h N% · 7d N%"
                // over lines that each show their own week, and Gil called
                // explanatory lines over rows "not needed". The legend still
                // says PARKED and how many.
                //
                // One line per account, in the box the tab's own lines use
                // (option H, lead ruling on H1b 2026-10-14); an account with a
                // lease line or an exit row keeps its card, after the lines,
                // as on the tab.
                //
                // `.grp .card{margin:6px 0}` — EVERY item, the first included,
                // and the line box counts as one. Its top margin does not
                // collapse into the group's own 12 pt padding, so the first
                // item sits 18 pt under the stroke; giving it nothing put it
                // at 11 pt. The LAST item carries the same margin on its OWN
                // bottom, but only when nothing else follows it: `GroupBox`'s
                // own bottom padding (4 pt) plus this 6 pt closes the box at
                // the mockup's 10, and a disclosure row right below gets the
                // 6 pt instead, so the two never stack into 12.
                let lines = visible.filter { form($0) != .card }
                let cards = visible.filter { form($0) == .card }
                let closes = !isCollapsible(section)
                if !lines.isEmpty {
                    lineRuns(
                        lines, band: section.band, grouped: true,
                        marginBottom: cards.isEmpty && closes ? V4.groupCardGap : 0
                    )
                    .padding(.top, V4.groupCardGap)
                }
                ForEach(Array(cards.enumerated()), id: \.element.id) { index, row in
                    VStack(alignment: .leading, spacing: 0) {
                        card(row, shape: .compact)
                        RowNotices(notices: notices(row.account))
                    }
                    .padding(.top, V4.groupCardGap)
                    .padding(
                        .bottom, index == cards.count - 1 && closes ? V4.groupCardGap : 0)
                }
            }
            if isCollapsible(section) {
                V4Disclosure(
                    title: disclosureTitle(section, expanded: expanded, hidden: hidden),
                    expanded: expanded,
                    help: expanded
                        ? "Collapses this group again."
                        : "Shows every account in this group."
                ) { onToggleGroup(section.group.token) }
                .padding(.bottom, V4.groupCardGap)
            }
        }
        // A group that leads the tab collapses its own 20 pt top margin with the
        // strip's 12 the same way a card does. ``GroupBox`` has already applied
        // the full margin, so the collapse is the difference — negative, and it
        // is a subtraction rather than a parameter because the margin belongs to
        // the group, not to whoever happens to draw it first.
        .padding(.top, first ? V4.marginAfterStrip(V4.groupMarginTop) - V4.groupMarginTop : 0)
    }

    /// Whether this group has a disclosure at all — whether it has ever hidden
    /// anything, in either direction.
    ///
    /// Both call sites used to sit behind `if summarised` / `if hidden > 0`, so
    /// once a group was open BOTH were false and no control rendered. Expanding
    /// was a one-way door, and the state persists to `UserDefaults` across
    /// launches: the thing that vanished was the control the user had just
    /// pressed, which also orphans focus under assistive tech. The pre-v4 panel
    /// kept the reverse path on the legend button with a "Collapses this group."
    /// hint.
    ///
    /// A wholly-parked group of three or fewer is NOT collapsible: it caps at
    /// ``parkedVisibleRows`` and so has nothing to hide, and a control that does
    /// nothing is worse than none.
    private func isCollapsible(_ section: FleetSection) -> Bool {
        section.collapsesByDefault
            || (section.isWhollyParked && section.rows.count > Self.parkedVisibleRows)
    }

    /// One direction each. ``FleetSection/expandButtonLabel`` says the whole
    /// count ("Show 6 accounts in this group") because a collapsed group shows
    /// no cards at all; a capped parked group says the REMAINDER, because some
    /// are already on screen. Closing says neither number — what it removes is
    /// whatever is currently open.
    private func disclosureTitle(
        _ section: FleetSection, expanded: Bool, hidden: Int
    ) -> String {
        if expanded { return "Show fewer accounts" }
        if isSummarised(section) { return section.expandButtonLabel }
        return "Show \(hidden) more \(hidden == 1 ? "account" : "accounts")"
    }

    private func pillRole(_ kind: FleetTally.Kind) -> V4Pill.Role {
        switch kind {
        case .ok: return .ok
        case .near: return .warn
        case .spent, .needsRelogin, .rejected, .unreachable: return .bad
        case .unmeasured: return .info
        case .unknown, .disabled: return .neutral
        }
    }
}
