import SwiftUI
import TcrBarCore

/// `.hsum.htop`: the top of the Accounts tab, under the tab bar and over the
/// scrolling list (option H, unit H3; Gil, 2026-10-14: "can we put tab ontop
/// of the drawing?"). It does not scroll, and no other tab draws it.
///
/// Two rows: the count headline with "next back in" at its right, then one
/// segment per account in the order the list draws its rows
/// (``FleetStrip``). Pointing at a segment rings it and lights every line of
/// that account below, the way a hovered line lights; it opens no hover card.
/// The ring is an overlay and the light is a line's own background, so
/// pointing changes no height (`--render-states` checks it with and without).
///
/// A fleet holding Codex accounts draws one headline row per pool in place of
/// the one headline (``StripPoolLine``), Claude first: a Claude account and a
/// Codex account serve different clients, and one folded count would hide a
/// pool with nothing left. Each extra row grows the panel by one headline
/// line; the list keeps its own budget. A fleet holding both providers also
/// leaves a wider gap between Claude's segments and Codex's
/// (``FleetStrip/poolBreak``), where the tab's two lists meet.
///
/// The frame's week line, "login ends in", "reading 40m old" and faded
/// segments wait for data `tcr` does not send yet, and are absent.
struct FleetStripHeader: View {
    let strip: FleetStrip
    /// The account whose segment is ringed, by ``Account/id``.
    let lit: String?
    /// The pointer entering (`true`) or leaving a segment, by account id.
    /// Never called past ``FleetStrip/pointableLimit``.
    let onPoint: (String, Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let pools = strip.pools {
                ForEach(pools, id: \.name) { line in
                    poolRow(line)
                }
            } else {
                headline
            }
            segments
        }
        .padding(.horizontal, V4.stripPaddingSide)
        .padding(.bottom, V4.stripMarginBottom)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
    }

    /// One sentence per pool, each with its breakdown, in a fleet holding Codex
    /// accounts; the headline and its countdown otherwise.
    private var spoken: String {
        if let pools = strip.pools { return pools.map(\.spoken).joined(separator: " ") }
        guard let countdown = strip.nextBackCountdown else { return strip.headline }
        return "\(strip.headline), next back \(countdown)"
    }

    /// `.hl`: the count in green at 600, and the countdown in red inside a
    /// `dim` "next back".
    private var headline: some View {
        HStack(alignment: .firstTextBaseline, spacing: V4.stripHeadlineGap) {
            Text(strip.headline)
                .font(V4.font(V4.stripHeadlineSize, .semibold))
                .foregroundStyle(Tok.ok)
                .lineLimit(1)
            Spacer(minLength: 0)
            if let countdown = strip.nextBackCountdown {
                (Text("next back ").foregroundColor(Tok.dim)
                    + Text(countdown).foregroundColor(Tok.spent))
                    .font(V4.font(V4.stripHeadlineSize))
                    .lineLimit(1)
                    .fixedSize()
            }
        }
        .frame(minHeight: V4.lineHeight(V4.stripHeadlineSize))
    }

    /// One pool's row: `Claude · ` in ink and the count at 600 in its tone,
    /// then the countdown at the right in the headline's two colours. A pool
    /// with nothing enabled is grey throughout and counts nothing down. The
    /// breakdown it does not draw is its tooltip.
    private func poolRow(_ line: StripPoolLine) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: V4.stripHeadlineGap) {
            (Text(line.lead)
                .font(V4.font(V4.stripHeadlineSize))
                .foregroundColor(line.enabled == 0 ? Tok.dim : Tok.ink)
                + Text(line.count)
                .font(V4.font(V4.stripHeadlineSize, .semibold))
                .foregroundColor(Self.toneColor(line.tone)))
                .lineLimit(1)
            Spacer(minLength: 0)
            if let countdown = line.nextBackCountdown {
                countdownText(lead: "next back ", countdown)
            } else if let countdown = line.nextInCountdown {
                countdownText(lead: "next ", countdown)
            }
        }
        .frame(minHeight: V4.lineHeight(V4.stripHeadlineSize))
        .help(line.help)
    }

    /// `next back in 2h 39m`: the phrase in `dim`, the countdown in red.
    private func countdownText(lead: String, _ countdown: String) -> some View {
        (Text(lead).foregroundColor(Tok.dim) + Text(countdown).foregroundColor(Tok.spent))
            .font(V4.font(V4.stripHeadlineSize))
            .lineLimit(1)
            .fixedSize()
    }

    /// A pool count's colour, by its role.
    private static func toneColor(_ role: SummaryRun.Role) -> Color {
        switch role {
        case .ok: return Tok.ok
        case .near: return Tok.near
        case .bad: return Tok.spent
        case .dim: return Tok.dim
        case .lead: return Tok.ink
        case .mute: return Tok.mute
        }
    }

    private var segments: some View {
        HStack(spacing: V4.stripGap) {
            ForEach(Array(strip.segments.enumerated()), id: \.element.id) { index, segment in
                if index == strip.poolBreak {
                    Color.clear.frame(width: V4.stripPoolSpacer)
                }
                StripSegmentView(kind: segment.kind, ringed: lit == segment.id)
                    .contentShape(Rectangle())
                    .onHover { inside in
                        if strip.isPointable { onPoint(segment.id, inside) }
                    }
            }
        }
        .frame(height: V4.stripHeight)
        .padding(.vertical, V4.stripMarginV)
    }
}

/// `.hseg`: a track filled from the bottom by the account's 5-hour room, or
/// washed whole in red (out of tokens) or grey (parked, never measured).
private struct StripSegmentView: View {
    let kind: StripSegmentKind
    let ringed: Bool

    var body: some View {
        ZStack(alignment: .bottom) {
            Rectangle().fill(background)
            if case .room(let room, let tone) = kind {
                Rectangle()
                    .fill(Self.color(tone))
                    .frame(height: V4.stripHeight * room)
            }
        }
        .frame(maxWidth: .infinity)
        .clipShape(RoundedRectangle(cornerRadius: V4.stripSegmentRadius))
        .overlay(
            RoundedRectangle(cornerRadius: V4.stripSegmentRadius + V4.stripRingWidth)
                .strokeBorder(Tok.ink, lineWidth: V4.stripRingWidth)
                .padding(-V4.stripRingWidth)
                .opacity(ringed ? 1 : 0)
                .allowsHitTesting(false)
        )
    }

    private var background: Color {
        switch kind {
        case .room: return Tok.ink.opacity(V4.barTrackAlpha)
        case .outOfTokens: return Tok.spent.opacity(V4.stripSpentAlpha)
        case .off: return Tok.mute.opacity(V4.stripOffAlpha)
        }
    }

    /// The same colours a line's dot wears for the same state.
    private static func color(_ tone: StripSegmentTone) -> Color {
        switch tone {
        case .ok: return Tok.ok
        case .near: return Tok.near
        case .bad: return Tok.spent
        case .info: return Tok.unmeasured
        case .quiet: return Tok.mute
        }
    }
}
