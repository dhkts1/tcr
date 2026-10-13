import SwiftUI
import TcrBarCore

/// Option H's hover card (Gil, 2026-10-14; unit H2): pointing at an account's
/// one line floats that account's full ``AccountCard`` over the list beside the
/// line, below it, or above it when the line is in the lower half of the list
/// (``HoverCardPlacement``). It carries what the line leaves out: the email
/// domain, the plan, the pills (`CONTROL` included), the gear, both windows
/// with their resets, the spend and the tokens. Live, out-of-tokens and parked
/// lines all get it.
///
/// ## Hung from the panel root, never expanded in place
///
/// Each line publishes its own bounds as an anchor (``hoverableLine(_:hovered:bleed:onHover:)``)
/// and the list publishes its viewport (``hoverCardList()``). The panel root
/// resolves both in its own space and draws the card in an `overlay`
/// (``accountHoverCard(shown:onHover:card:)``), which takes no part in the size
/// of the view it decorates. So the card is not clipped by the list's
/// `ScrollView`, and it cannot change the list's or the panel's height: a card
/// that grew a row in place would change the very content height that sizes
/// the popover, which reopens the layout cycle `ffe8a86` cut. `--render-states`
/// measures this: every hover scene is drawn with and without its card, and
/// the harness fails when the two sizes differ.
///
/// ## Decoration, not a route
///
/// Nothing is reachable only through this card. A click on the line opens the
/// same actions the card's gear does, from the one definition in `AccountRow`,
/// and a right-click opens them too, so a keyboard or VoiceOver user loses
/// nothing; the card is hidden from accessibility, because the line's spoken
/// label already says what it says. The gear works when the pointer reaches it:
/// the card stays while the pointer is on it, and the 4 pt gap between line
/// and card belongs to the card, so crossing it does not hover the next line.
///
/// ## Accounts that keep their in-list card
///
/// An account carrying a lease line or an exit picker is drawn with its full
/// card in the list, not as a line, and so never shows this card (lead ruling,
/// 2026-10-14). The exit picker is a control, and a control cannot live in a
/// card that vanishes when the pointer moves; the lease line is the notice that
/// another Mac may read this account's traffic, which is not something to show
/// only on hover. Grouped accounts still draw compact cards and have no line
/// either (their lines are unit H1b).
struct HoverableLine {
    let anchor: Anchor<CGRect>
    let row: FleetSectionRow
}

/// What the panel root needs to float a card: the list's visible bounds, and
/// every hoverable line's bounds, keyed by account name.
struct HoverCardAnchors {
    var list: Anchor<CGRect>?
    var lines: [String: HoverableLine] = [:]
}

struct HoverCardAnchorsKey: PreferenceKey {
    static let defaultValue = HoverCardAnchors()
    static func reduce(value: inout HoverCardAnchors, nextValue: () -> HoverCardAnchors) {
        let next = nextValue()
        if let list = next.list { value.list = list }
        value.lines.merge(next.lines) { _, newer in newer }
    }
}

extension View {
    /// A line the pointer can hover: `.hov`'s fill while its card shows, the
    /// pointer's enter and leave, and the line's bounds for the panel root.
    ///
    /// `bleed` widens the fill past the line to its box's edges, the
    /// `.wline.hov{margin:0 -10px;padding:0 10px}` of a waiting line; a live
    /// line's fill stays inside its box's padding, as `.slim.hov` does.
    func hoverableLine(
        _ row: FleetSectionRow, hovered: Bool, bleed: CGFloat,
        onHover: @escaping (Bool) -> Void
    ) -> some View {
        background(
            Tok.hover
                .opacity(hovered ? 1 : 0)
                .padding(.horizontal, -bleed)
        )
        .onHover(perform: onHover)
        .anchorPreference(key: HoverCardAnchorsKey.self, value: .bounds) {
            HoverCardAnchors(lines: [row.account.name: HoverableLine(anchor: $0, row: row)])
        }
    }

    /// The list's visible bounds, which decide whether a line is in its lower
    /// half. A transform rather than `anchorPreference`, which would replace
    /// the lines' anchors coming up from inside the list instead of adding to
    /// them.
    func hoverCardList() -> some View {
        transformAnchorPreference(key: HoverCardAnchorsKey.self, value: .bounds) {
            $0.list = $1
        }
    }

    /// Floats `card` for the account named `shown` beside its line. Draws
    /// nothing when no line by that name is on screen (another tab, a grouped
    /// account, a fleet that changed under the pointer).
    func accountHoverCard<Card: View>(
        shown: String?, onHover: @escaping (Bool) -> Void,
        @ViewBuilder card: @escaping (FleetSectionRow) -> Card
    ) -> some View {
        overlayPreferenceValue(HoverCardAnchorsKey.self) { anchors in
            GeometryReader { proxy in
                if let shown, let line = anchors.lines[shown], let list = anchors.list {
                    let row = proxy[line.anchor]
                    let listFrame = proxy[list]
                    let edge = HoverCardPlacement.edge(row: row, list: listFrame)
                    let region = HoverCardPlacement.region(
                        row: row, list: listFrame,
                        panel: CGRect(origin: .zero, size: proxy.size),
                        inset: V4.hoverCardInset)
                    FloatingCard { card(line.row) }
                        // The gap is the card's own, so the pointer crossing
                        // it is on the card and not on the next line.
                        .padding(edge == .below ? .top : .bottom, V4.hoverCardGap)
                        .contentShape(Rectangle())
                        .onHover(perform: onHover)
                        .frame(
                            width: region.width, height: region.height,
                            alignment: edge == .below ? .top : .bottom
                        )
                        .position(x: region.midX, y: region.midY)
                }
            }
        }
    }
}

/// `.hcard`: the card lifted off the list, a strong 1 pt ring and a deep shadow
/// so it reads as above the lines it covers rather than as one of them.
private struct FloatingCard<Content: View>: View {
    @ViewBuilder var content: () -> Content

    var body: some View {
        content()
            // Its own height, not the region's: the region runs to the panel's
            // edge, and a card proposed that height stretches its rows apart.
            .fixedSize(horizontal: false, vertical: true)
            // The shadow on a shape behind the card, never on the card: a
            // shadow on a stack is drawn under every glyph in it.
            .background(
                RoundedRectangle(cornerRadius: V4.cardRadius)
                    .fill(Tok.cardFill)
                    .shadow(
                        color: Color.black.opacity(V4.hoverCardShadowAlpha),
                        radius: V4.hoverCardShadowRadius, y: V4.hoverCardShadowY)
            )
            .overlay(
                RoundedRectangle(cornerRadius: V4.cardRadius)
                    .strokeBorder(Tok.hairlineStrong, lineWidth: V4.panelBorderWidth)
            )
            .accessibilityHidden(true)
    }
}
