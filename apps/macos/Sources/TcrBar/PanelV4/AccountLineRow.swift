import SwiftUI
import TcrBarCore

/// Option H's account rows (Gil, 2026-10-14): ONE line per account, so a
/// thirteen-account fleet fits the list's 520 pt cap instead of six cards of it.
///
/// What a line leaves out is on purpose and is not lost: the email domain, the
/// plan, the pills, the spend tail and the per-row gear are the account card's,
/// which option H shows on hover (unit H2). Every word that IS drawn comes from
/// `TcrBarCore/AccountLine.swift`, where it is tested.
///
/// No column headings and no caption over the rows (Gil, 2026-10-13: "this is
/// bad and not needed"). Every line draws its `5h` and `7d` labels and every
/// bar starts at the same x, in a group box or out of one (lead ruling on H1b,
/// 2026-10-14): the bar width and the name column are chosen once for the
/// panel by ``AccountLineFit``, never per row, and a name too long for the
/// room truncates rather than taking the labels with it.
///
/// ## The actions, without a gear
///
/// The whole line is the actions menu: a click opens the same items the
/// right-click menu has, from the one definition in `AccountRow`. A `Menu`
/// styled `.button` with a `.plain` button style draws its label as written;
/// `.borderlessButton` would flatten the row to its name alone (measured on a
/// throwaway AppKit render, 2026-10-14). In `snapshotMode` the label is drawn
/// alone, because `ImageRenderer` draws any `Menu` as the "prohibited" glyph.
struct AccountLineRow<MenuItems: View>: View {
    let account: Account
    let now: Date
    var isControl: Bool = false
    var snapshotMode: Bool = false
    /// The name column's width: one panel-wide column, less a group's padding
    /// inside a group box, so every bar starts at the same x
    /// (``AccountLineFit/nameColumn(looseNames:groupedNames:signals:bar:_:_:)``).
    var nameWidth: CGFloat = V4.liveNameWidth
    /// The fleet also holds Codex accounts, so a Claude line's spoken label says
    /// which provider it serves (``Account/cardSummaryLabel(now:isControl:namesProvider:)``).
    var namesProvider: Bool = false
    /// The meters' label column, ONE for the panel, as wide as the widest label
    /// any line draws: a Codex window labelled by its length (`30d`) widens it
    /// for every line, so every bar still starts at one x. The default is the
    /// column for `5h` and `7d` alone.
    var labelWidth: CGFloat = AccountLineFit.labelColumn(V4.lineMetrics)
    /// The one bar width, measured with those same labels (``V4/lineBarWidth(labels:)``).
    var barWidth: CGFloat = V4.lineBarWidth
    @ViewBuilder var menu: () -> MenuItems

    var body: some View {
        Group {
            if snapshotMode {
                content
            } else {
                Menu {
                    menu()
                } label: {
                    content
                }
                .menuStyle(.button)
                .buttonStyle(.plain)
                .menuIndicator(.hidden)
            }
        }
        .contextMenu { menu() }
        .help(CodexCardFacts.nameHelp(account))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            account.cardSummaryLabel(
                now: now, isControl: isControl, namesProvider: namesProvider)
        )
        .modifier(LineValue(value: CodexCardFacts.lineValue(account, now: now)))
        .accessibilityHint("Opens this account's actions.")
    }

    private var content: some View {
        HStack(spacing: V4.liveLineGap) {
            LineDot(color: LineColor.dot(account))
            // Cut in the MIDDLE by ``AccountLineFit/fitName(_:width:_:)``,
            // measured in this font, so `henry9` and `henry10` stay two names
            // (lead ruling, 2026-10-14). `.middle` is the backstop should the
            // measure and the draw ever differ by a fraction of a point; the
            // full name is the line's help and spoken label.
            Text(
                AccountLineFit.fitName(
                    AccountName.localPart(account.name), width: nameWidth, V4.lineMetrics)
            )
            .font(V4.font(V4.nameSize, .semibold))
            .foregroundStyle(Tok.ink)
            .lineLimit(1)
            .truncationMode(.middle)
            .frame(width: nameWidth, alignment: .leading)
            if let noWindows = account.lineNoWindowsText {
                // A plan that reports no window: one mute phrase across both
                // meter slots, which keep their width so the signal stays put.
                HStack(spacing: V4.liveLineGap) {
                    meter(AccountLineFit.labels[0], nil, .unmeasured).hidden()
                    meter(AccountLineFit.labels[1], nil, .unmeasured).hidden()
                }
                .overlay(alignment: .leading) {
                    Text(noWindows)
                        .font(V4.font(V4.muteSize))
                        .foregroundStyle(Tok.mute)
                        .lineLimit(1)
                }
            } else {
                let slots = account.lineSlotLabels
                slot(0, slots[0], account.fiveHour, account.quotaBarTintSource(for: .fiveHour))
                slot(1, slots[1], account.sevenDay, account.quotaBarTintSource(for: .sevenDay))
            }
            let signal = account.liveSignal(now: now)
            Text(signal.text)
                .font(V4.font(V4.lineSignalSize))
                .foregroundStyle(LineColor.tone(signal.tone))
                .lineLimit(1)
                .truncationMode(.tail)
                // Whatever the row has left after the name and the meters,
                // which ``AccountLineFit`` keeps at least as wide as the
                // widest signal the panel draws.
                .frame(maxWidth: .infinity, alignment: .trailing)
        }
        .frame(minHeight: V4.lineRowHeight)
        .contentShape(Rectangle())
    }

    /// One meter slot: the window's meter under its label, or, for a window the
    /// plan does not report (`label` is `nil`), the same meter hidden, so the
    /// slot keeps its width and the bars after it keep their x.
    @ViewBuilder
    private func slot(
        _ index: Int, _ label: String?, _ value: Double?, _ tint: QuotaBarTintSource
    ) -> some View {
        if let label {
            meter(label, value, tint)
        } else {
            meter(AccountLineFit.labels[index], value, tint).hidden()
        }
    }

    /// `.sc`: the window's label, its small bar, its percentage.
    /// Both columns are fixed widths measured for the widest they draw
    /// (``AccountLineFit/labelColumn(_:labels:)``, ``AccountLineFit/percentColumn(_:)``),
    /// so a row's own digits never move the bar after them.
    private func meter(
        _ label: String, _ value: Double?, _ tint: QuotaBarTintSource
    ) -> some View {
        HStack(spacing: V4.lineMeterGap) {
            // Natural width, never wrapped, inside a column measured for it:
            // at Comfortable's 12 pt the label is wider than the sheet's 14 pt
            // and wrapped to "5" over "h" inside a frame of that width.
            Text(label)
                .font(V4.font(V4.muteSize))
                .foregroundStyle(Tok.mute)
                .lineLimit(1)
                .fixedSize()
                .frame(width: labelWidth, alignment: .leading)
            LineBar(value: value, tint: tint, width: barWidth)
            Text(QuotaFormat.percent(value))
                .font(V4.font(V4.dimSize))
                .foregroundStyle(Tok.dim)
                .lineLimit(1)
                .fixedSize()
                .frame(width: AccountLineFit.percentColumn(V4.lineMetrics), alignment: .trailing)
        }
        .fixedSize()
    }
}

/// `.wline`: an account that is waiting, out of tokens or parked. The dot, the
/// name, how much of the week is used, and when it is back.
struct WaitingLineRow<MenuItems: View>: View {
    let account: Account
    let band: FleetBand
    let now: Date
    var isControl: Bool = false
    var snapshotMode: Bool = false
    /// See ``AccountLineRow/namesProvider``.
    var namesProvider: Bool = false
    @ViewBuilder var menu: () -> MenuItems

    var body: some View {
        Group {
            if snapshotMode {
                content
            } else {
                Menu {
                    menu()
                } label: {
                    content
                }
                .menuStyle(.button)
                .buttonStyle(.plain)
                .menuIndicator(.hidden)
            }
        }
        .contextMenu { menu() }
        .help(CodexCardFacts.nameHelp(account))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            account.cardSummaryLabel(
                now: now, isControl: isControl, namesProvider: namesProvider)
        )
        .modifier(LineValue(value: CodexCardFacts.lineValue(account, now: now)))
        .accessibilityHint("Opens this account's actions.")
    }

    private var content: some View {
        HStack(spacing: V4.waitLineGap) {
            LineDot(color: band == .parked ? Tok.mute : LineColor.dot(account))
            Text(AccountName.localPart(account.name))
                .font(V4.font(V4.nameSize, .semibold))
                .foregroundStyle(Tok.ink)
                .lineLimit(1)
                // The middle, for the reason the live line's name is cut
                // there: the end of a name is what tells two apart.
                .truncationMode(.middle)
                .frame(minWidth: V4.waitNameMinWidth, alignment: .leading)
            if let middle = account.waitingLineText {
                Text(middle)
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.mute)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            if let signal = account.waitingLineSignal(in: band, now: now) {
                Text(signal.text)
                    .font(V4.font(V4.lineSignalSize))
                    .foregroundStyle(LineColor.tone(signal.tone))
                    .lineLimit(1)
                    .fixedSize()
            }
        }
        .frame(minHeight: V4.waitRowHeight)
        .contentShape(Rectangle())
    }
}

/// `.bandhead`: "OUT OF TOKENS · 6", with "first back in 42m" at its right.
struct BandHeading: View {
    let section: FleetSection
    let now: Date

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: V4.bandHeadGap) {
            Text(section.bandHeadingText.uppercased())
                .font(V4.font(V4.bandHeadSize, .bold))
                .tracking(V4.bandHeadTracking)
                .foregroundStyle(Tok.dim)
                .lineLimit(1)
            Spacer(minLength: 0)
            if let countdown = section.firstBackCountdown(now: now) {
                (Text("first back ").foregroundColor(Tok.mute)
                    + Text(countdown).foregroundColor(Tok.spent))
                    .font(V4.font(V4.bandHeadSize, .medium))
                    .tracking(V4.bandHeadTailTracking)
                    .lineLimit(1)
                    .fixedSize()
            }
        }
        .padding(.horizontal, V4.bandHeadPaddingH)
        .padding(.bottom, V4.bandHeadMarginBottom)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }
}

/// `.sbox` / `.wbox`: the box a run of lines sits in, with a 60 % hairline
/// between rows and none above the first.
struct LineBox<Item: Identifiable, Row: View>: View {
    let items: [Item]
    let paddingH: CGFloat
    /// `.sbox,.wbox{margin:0 0 8px}` on the tab itself. Inside a group box
    /// the box keeps the spacing the group's cards had instead
    /// (``V4/groupCardGap``), so the group closes at the same height.
    var marginBottom: CGFloat = V4.lineBoxMarginBottom
    @ViewBuilder var row: (Item) -> Row

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(items.enumerated()), id: \.element.id) { index, item in
                if index > 0 {
                    Rectangle()
                        .fill(Tok.cardLine.opacity(V4.lineDividerAlpha))
                        .frame(height: V4.panelBorderWidth)
                }
                row(item)
            }
        }
        .padding(.vertical, V4.lineBoxPaddingV)
        .padding(.horizontal, paddingH)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.cardFill))
        .overlay(
            RoundedRectangle(cornerRadius: V4.cardRadius)
                .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
        )
        .padding(.bottom, marginBottom)
    }
}

/// What became of the last thing done to an account, under its line or card:
/// a refused delete or group edit in `tcr`'s own words, or what `tcr` said
/// while a delete succeeded (``AccountRowNotice``). Draws nothing when there is
/// nothing to say, so a row with no notice keeps its height.
///
/// `.mute`'s size, wrapped rather than cut (a reason is useless with its end
/// missing), in `Tok.spent` for a failure and `Tok.near` for a note, the two
/// colours the rest of the panel already uses for those.
///
/// An alert, a state the account is in such as a signed-out Codex login, is red
/// like a failure but spoken as written: no action failed to produce it. Both
/// choices are exhaustive switches, so a new tone cannot fall silently into the
/// amber branch.
struct RowNotices: View {
    let notices: [AccountRowNotice]

    var body: some View {
        if !notices.isEmpty {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(notices) { notice in
                    Text(notice.text)
                        .font(V4.font(V4.muteSize))
                        .foregroundStyle(Self.color(notice.tone))
                        .lineLimit(nil)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .help(notice.text)
                        .accessibilityLabel(Self.spoken(notice))
                }
            }
        }
    }

    private static func color(_ tone: AccountRowNotice.Tone) -> Color {
        switch tone {
        case .failure, .alert: return Tok.spent
        case .note: return Tok.near
        }
    }

    private static func spoken(_ notice: AccountRowNotice) -> String {
        switch notice.tone {
        case .failure: return "Failed: \(notice.text)"
        case .alert, .note: return notice.text
        }
    }
}

/// A line's accessibility value, set only when there is one: a Codex line's
/// state help (``CodexCardFacts/lineValue(_:now:timeZone:home:)``), which its
/// card's pill would show on hover. Every other line gets no value, as before.
private struct LineValue: ViewModifier {
    let value: String?

    @ViewBuilder
    func body(content: Content) -> some View {
        if let value {
            content.accessibilityValue(value)
        } else {
            content
        }
    }
}

/// `.dot`, never the only carrier of its meaning: the words at the right of
/// the same line say the state.
private struct LineDot: View {
    let color: Color

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: V4.lineDotSize, height: V4.lineDotSize)
            .accessibilityHidden(true)
    }
}

/// `.mbar`: a 32 pt track and its fill. An unmeasured window draws the empty
/// track and no fill, the rule ``QuotaRow`` records for the card's bar.
private struct LineBar: View {
    let value: Double?
    let tint: QuotaBarTintSource
    /// The panel's one bar width (``AccountLineRow/barWidth``).
    let width: CGFloat

    private var fraction: Double {
        switch QuotaFormat.barFill(value) {
        case .measured(let v): return v
        case .unmeasured: return 0
        }
    }

    var body: some View {
        ZStack(alignment: .leading) {
            RoundedRectangle(cornerRadius: V4.barRadius)
                .fill(Tok.ink.opacity(V4.barTrackAlpha))
            if let fill = tint.fillColor {
                RoundedRectangle(cornerRadius: V4.barRadius)
                    .fill(fill)
                    .frame(width: max(V4.barMinWidth, width * fraction))
            }
        }
        .frame(width: width, height: V4.lineBarHeight)
    }
}

/// The one mapping from a line's state to a palette colour.
private enum LineColor {
    static func tone(_ tone: AccountLineTone) -> Color {
        switch tone {
        case .quiet: return Tok.mute
        case .near: return Tok.near
        case .bad: return Tok.spent
        case .info: return Tok.unmeasured
        }
    }

    /// The same kinds ``AccountsTabV4``'s tally dots colour, by the same rule.
    static func dot(_ account: Account) -> Color {
        switch FleetTally.Kind(account: account) {
        case .ok: return Tok.ok
        case .near: return Tok.near
        case .spent, .needsRelogin, .rejected, .unreachable: return Tok.spent
        case .unmeasured: return Tok.unmeasured
        case .unknown, .disabled: return Tok.mute
        }
    }
}
