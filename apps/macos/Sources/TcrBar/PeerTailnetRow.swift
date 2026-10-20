import SwiftUI
import TcrBarCore

/// The "Tailscale" row on Settings > Peers > This Mac: a title, the sub-line,
/// a fixed-width picker (Off / Tailscale app / tcr's own node…), and one
/// state line.
///
/// A plain `VStack` rather than rows written inline in the pane's `Form`, for
/// the reason ``PeerInternetRow`` gives: `ImageRenderer` cannot draw a
/// `Form`, and as its own view this row rasterises under `--render-states`,
/// one PNG per state.
///
/// # Nothing moves when the state changes (Gil, 2026-10-14)
///
/// Every size is decided once, from the worst case. The picker has a fixed
/// width whatever its label says. The sub-line is the same sentence in every
/// state. The state line is ONE line in every state, its height reserved by a
/// hidden line of the same font, truncating rather than wrapping, and it
/// spans the row's full width under the picker so the longest working line
/// ("tcr's own node · … · 1 paired Mac connected") is not cut in the narrow
/// label column.
///
/// It owns no state and reads no clock: the state arrives decided by
/// ``PeerTailnetState/state(_:now:)``.
struct PeerTailnetRow: View {
    let read: PeerTailnetRead
    let state: PeerTailnetState
    /// A pick is running: the picker is disabled until it lands.
    var pending = false
    /// The pick, with the mode it moves TO.
    var onPick: (PeerTailnetMode) -> Void = { _ in }
    /// The state line's press, per link. A link with no handler is not
    /// drawn: the join and details sheets are not built yet, and a press
    /// that does nothing is not drawn in the meantime.
    var onOpenApp: (() -> Void)? = nil
    var onJoin: (() -> Void)? = nil
    var onDetails: (() -> Void)? = nil
    /// `ImageRenderer` draws a `Menu` as the "prohibited" glyph, so the
    /// render harness draws a look-alike of the closed picker instead.
    var snapshotMode = false

    /// The mockup's `.pop`: 186 pt, whatever the label says.
    static let pickerWidth: CGFloat = 186

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center, spacing: 12) {
                VStack(alignment: .leading, spacing: 1) {
                    Text("Tailscale")
                    Text(PeerTailnetState.rowDetail)
                        .font(.caption)
                        .foregroundStyle(Tok.inkFaint)
                        .fixedSize(horizontal: false, vertical: true)
                }
                // An IDEAL width of zero, and the reason is measured: the
                // grouped `Form` sizes its table column from each row's ideal
                // width, and the sub-line's one-line width plus the picker is
                // wider than the pane. The column then grew, every other row
                // was measured at that width, and the pairing request's title
                // below drew as one truncated line instead of wrapping to two
                // (`--render-settings`, 2026-10-20).
                .frame(minWidth: 0, idealWidth: 0, maxWidth: .infinity, alignment: .leading)
                picker
                    .frame(width: Self.pickerWidth, alignment: .trailing)
            }
            stateLine
                .padding(.top, 4)
                .frame(minWidth: 0, idealWidth: 0, maxWidth: .infinity, alignment: .leading)
        }
    }

    // MARK: The picker

    @ViewBuilder private var picker: some View {
        if snapshotMode {
            HStack(spacing: 4) {
                Text(read.pickerTitle)
                    .lineLimit(1)
                Spacer(minLength: 0)
                Image(systemName: "chevron.up.chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Tok.inkFaint)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .frame(width: Self.pickerWidth)
            .overlay(
                RoundedRectangle(cornerRadius: 6).strokeBorder(Tok.hairlineStrong, lineWidth: 1)
            )
            .opacity(read.canPick ? 1 : 0.5)
        } else {
            // A `Picker`, not a `Menu`: in a grouped `Form` it draws as the
            // same pop-up (value and up/down chevrons) every other choice in
            // Settings uses, which is what the mockup's `.pop` stands for.
            Picker("Tailscale", selection: selection) {
                ForEach(PeerTailnetMode.allCases, id: \.self) { mode in
                    if mode == .app { Divider() }
                    VStack(alignment: .leading) {
                        Text(mode.title)
                        Text(mode.menuDetail(nodeSupported: read.nodeSupported))
                    }
                    .tag(Optional(mode))
                        .disabled(!mode.isOffered(nodeSupported: read.nodeSupported))
                }
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .frame(width: Self.pickerWidth, alignment: .trailing)
            .disabled(!read.canPick || pending)
        }
    }

    /// The picker's selection: what `tcr` last said, and a pick runs the
    /// verb with the mode it moves TO. `nil` before the first answer.
    private var selection: Binding<PeerTailnetMode?> {
        Binding(
            get: { read.mode },
            set: { picked in
                guard let picked, picked != read.mode else { return }
                onPick(picked)
            })
    }

    // MARK: The state line

    private var tint: Color {
        switch state.tone {
        case .plain: return Tok.inkFaint
        case .ok: return Tok.ok
        case .warn: return Tok.near
        }
    }

    private var linkAction: (title: String, action: () -> Void)? {
        guard let link = state.link else { return nil }
        let handler: (() -> Void)?
        switch link {
        case .openApp: handler = onOpenApp
        case .join: handler = onJoin
        case .details: handler = onDetails
        }
        return handler.map { (link.title, $0) }
    }

    private var stateLine: some View {
        ZStack(alignment: .leading) {
            // One caption line, reserved in every state.
            Text("Ag").font(.caption).hidden().accessibilityHidden(true)
            HStack(spacing: 6) {
                Text(state.line)
                    .font(.caption)
                    .foregroundStyle(tint)
                    .lineLimit(1)
                    .truncationMode(.tail)
                if let link = linkAction {
                    if snapshotMode {
                        // `ImageRenderer` draws a link button as the
                        // "prohibited" glyph too; the word, in link ink.
                        Text(link.title)
                            .font(.caption)
                            .foregroundStyle(Tok.link)
                            .lineLimit(1)
                            .fixedSize()
                    } else {
                        Button(link.title, action: link.action)
                            .buttonStyle(.link)
                            .font(.caption)
                            .lineLimit(1)
                            .fixedSize()
                    }
                }
                Spacer(minLength: 0)
            }
        }
    }
}
