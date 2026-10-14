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
            headline
            segments
        }
        .padding(.horizontal, V4.stripPaddingSide)
        .padding(.bottom, V4.stripMarginBottom)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
    }

    private var spoken: String {
        guard let countdown = strip.nextBackCountdown else { return strip.headline }
        return "\(strip.headline), next back \(countdown)"
    }

    /// `.hl`: the count in green at 600, and the countdown in red inside a
    /// `dim` "next back". When nothing can take work the count is red too:
    /// "0 of 3 can take work now" in the ready green read as good news.
    private var headline: some View {
        HStack(alignment: .firstTextBaseline, spacing: V4.stripHeadlineGap) {
            Text(strip.headline)
                .font(V4.font(V4.stripHeadlineSize, .semibold))
                .foregroundStyle(strip.isOutOfCapacity ? Tok.spent : Tok.ok)
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

    private var segments: some View {
        HStack(spacing: V4.stripGap) {
            ForEach(strip.segments) { segment in
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
