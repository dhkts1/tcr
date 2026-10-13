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
/// bad and not needed"). The `5h` and `7d` labels sit beside their bars while
/// they fit and drop when they do not, which is what larger text does to them.
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
    /// The name column's width, shared by every row in one box so the bars
    /// line up: ``nameColumnWidth(for:)``.
    var nameWidth: CGFloat = V4.liveNameWidth
    @ViewBuilder var menu: () -> MenuItems

    /// The widest of `accounts`' names at the row's name font, held between
    /// ``V4/liveNameWidth`` (option H's column) and ``V4/liveNameMaxWidth``.
    static func nameColumnWidth(for accounts: [Account]) -> CGFloat {
        let widest =
            accounts.map {
                NameRowFit.textWidth(
                    AccountName.localPart($0.name), size: V4.nameSize, weight: .semibold)
            }.max() ?? 0
        return min(max(widest.rounded(.up), V4.liveNameWidth), V4.liveNameMaxWidth)
    }

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
        .help(account.name)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(account.cardSummaryLabel(now: now, isControl: isControl))
        .accessibilityHint("Opens this account's actions.")
    }

    private var content: some View {
        ViewThatFits(in: .horizontal) {
            line(labelled: true)
            line(labelled: false)
        }
        .frame(minHeight: V4.lineRowHeight)
        .contentShape(Rectangle())
    }

    private func line(labelled: Bool) -> some View {
        HStack(spacing: V4.liveLineGap) {
            LineDot(color: LineColor.dot(account))
            Text(AccountName.localPart(account.name))
                .font(V4.font(V4.nameSize, .semibold))
                .foregroundStyle(Tok.ink)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(width: nameWidth, alignment: .leading)
            meter("5h", account.fiveHour, account.quotaBarTintSource(for: .fiveHour), labelled)
            meter("7d", account.sevenDay, account.quotaBarTintSource(for: .sevenDay), labelled)
            let signal = account.liveSignal(now: now)
            Text(signal.text)
                .font(V4.font(V4.lineSignalSize))
                .foregroundStyle(LineColor.tone(signal.tone))
                .lineLimit(1)
                .truncationMode(.tail)
                // One ideal width for EVERY row's signal, whatever it says, so
                // `ViewThatFits` makes the same choice on every line of a box
                // and the bars stay in one column. Measured per row, a long
                // signal dropped one row's labels and left its neighbours'.
                .frame(
                    minWidth: 0, idealWidth: V4.lineSignalIdealWidth, maxWidth: .infinity,
                    alignment: .trailing)
        }
    }

    /// `.sc`: the window's label, its small bar, its percentage.
    private func meter(
        _ label: String, _ value: Double?, _ tint: QuotaBarTintSource, _ labelled: Bool
    ) -> some View {
        HStack(spacing: V4.lineMeterGap) {
            if labelled {
                // Natural width, never wrapped: at Comfortable's 12 pt the
                // label is wider than the sheet's 14 pt column and wrapped to
                // "5" over "h" inside a fixed frame.
                Text(label)
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.mute)
                    .lineLimit(1)
                    .fixedSize()
                    .frame(minWidth: V4.lineLabelWidth, alignment: .leading)
            }
            LineBar(value: value, tint: tint)
            Text(QuotaFormat.percent(value))
                .font(V4.font(V4.dimSize))
                .foregroundStyle(Tok.dim)
                .lineLimit(1)
                .fixedSize()
                .frame(minWidth: V4.linePercentWidth, alignment: .trailing)
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
        .help(account.name)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(account.cardSummaryLabel(now: now, isControl: isControl))
        .accessibilityHint("Opens this account's actions.")
    }

    private var content: some View {
        HStack(spacing: V4.waitLineGap) {
            LineDot(color: band == .parked ? Tok.mute : LineColor.dot(account))
            Text(AccountName.localPart(account.name))
                .font(V4.font(V4.nameSize, .semibold))
                .foregroundStyle(Tok.ink)
                .lineLimit(1)
                .truncationMode(.tail)
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
                .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth))
        .padding(.bottom, marginBottom)
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
                    .frame(width: max(V4.barMinWidth, V4.lineBarWidth * fraction))
            }
        }
        .frame(width: V4.lineBarWidth, height: V4.lineBarHeight)
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
        case .spent, .needsRelogin, .rejected: return Tok.spent
        case .unmeasured: return Tok.unmeasured
        case .unknown, .disabled: return Tok.mute
        }
    }
}
