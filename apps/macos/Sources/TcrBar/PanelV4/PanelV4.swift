import SwiftUI
import TcrBarCore

/// `.panel` — the panel itself: 372 pt wide, radius 18, padding 10 10 8, a 1 pt
/// `line` border with a brighter top edge, and tabular figures throughout.
///
/// This is the shell every tab draws inside. It owns the chrome and nothing else:
/// the header, the segmented control, the summary block, and the footer's rule.
///
/// The tabs come straight after the header, ABOVE the summary (2026-10-16). Each
/// tab answers a different question in the summary block, so its height differs
/// per tab: one line on Accounts and Peers, two on Sessions, three on Tools, and
/// the Tools line wraps by its own numbers. Drawn above the tabs, that block moved
/// the whole strip by up to 33 pt on every tab switch and on some polls. Under the
/// tabs it moves only what is below it.
/// What goes between the strip and the footer is the tab's own business, which is
/// why `content` is a closure rather than a switch in here — the Accounts tab is
/// transcribed from the mockup under `PanelV4/`, while Sessions and Tools keep
/// the views they already had (Gil, 2026-09-13: "i think just accounts need
/// fixing … the others i think ours look better").
///
/// The corner, the border and the top edge are the shared-chrome fix. Measured
/// against the approved mockup: its corner rounds over 18 pt, row y=0 is the
/// `rgba(255,255,255,.18)` top edge and a 1 pt `line` border runs the perimeter,
/// while every earlier round of this panel had pixel (0,0) already at panel fill
/// — radius 0, no border, no top edge. It is the single largest block of wrong
/// pixels on all three tabs, and it is drawn once, here, for all of them.
struct PanelV4<Summary: View, Content: View, Footer: View>: View {
    var title: String = "tcr fleet"
    /// "updated 4s ago", or `nil` before the first poll lands.
    let freshness: String?
    let tabs: [PanelTab]
    let selected: PanelTab
    let badges: [PanelTab: Int]
    let onSelect: (PanelTab) -> Void
    let onSettings: () -> Void
    /// How many account rows this panel is about to draw, or `nil` on a
    /// not-a-fleet state. Recorded — not drawn — so the density tokens can
    /// resolve `PanelDensity.auto` (Gil, 2026-09-13: "make compact the default
    /// please above 4 accounts").
    var accountCount: Int?
    /// Each tab's summary block, body and footer. Called for EVERY tab in
    /// `tabs`, not only the selected one: see ``tabLayers``.
    @ViewBuilder var summary: (PanelTab) -> Summary
    @ViewBuilder var content: (PanelTab) -> Content
    @ViewBuilder var footer: (PanelTab) -> Footer

    var body: some View {
        // Before any `V4` token in the tree below is read. A `static var`
        // density token is evaluated during layout by views with no fleet in
        // scope, so the count has to be current by the time the first of them
        // runs — which is why it is set here, in the shell, and not inside the
        // Accounts tab three levels down.
        PanelDensityPreference.setAccountCount(accountCount)
        return panelBody
    }

    private var panelBody: some View {
        VStack(alignment: .leading, spacing: 0) {
            PanelHeader(title: title, freshness: freshness, onSettings: onSettings)
            SegmentedTabs(
                tabs: tabs, selected: selected, badges: badges, onSelect: onSelect)
            tabLayers
        }
        .padding(.top, V4.panelPaddingTop)
        .padding(.horizontal, V4.panelPaddingSide)
        .padding(.bottom, V4.panelPaddingBottom)
        .frame(width: V4.panelWidth)
        .background(
            RoundedRectangle(cornerRadius: V4.panelRadius, style: .continuous)
                .fill(Tok.panel)
        )
        .overlay(
            RoundedRectangle(cornerRadius: V4.panelRadius, style: .continuous)
                .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
        )
        .overlay(alignment: .top) {
            // `border-top-color:rgba(255,255,255,.18)` — the lit edge that makes
            // the panel read as a raised surface rather than a painted rectangle.
            // Drawn as a clipped top segment rather than a second full border, so
            // the sides keep the `line` colour the sheet gives them.
            RoundedRectangle(cornerRadius: V4.panelRadius, style: .continuous)
                .strokeBorder(
                    Tok.panelTopEdge.opacity(V4.panelTopEdgeAlpha), lineWidth: V4.panelBorderWidth
                )
                .mask(
                    LinearGradient(
                        colors: [.black, .clear],
                        startPoint: .top, endPoint: .bottom)
                )
                .allowsHitTesting(false)
        }
        .clipShape(RoundedRectangle(cornerRadius: V4.panelRadius, style: .continuous))
    }

    /// Every tab, one over the other, at the tallest one's height, with only
    /// the selected one visible (Gil, 2026-10-16: "can you make also it to not
    /// change the height?").
    ///
    /// Each tab used to size the panel itself, so a switch resized the popover:
    /// 498 pt on Accounts and 411 pt on Sessions for one fleet, measured from
    /// the 1.2.4 renders. Now the height is the tallest tab's own, so nothing
    /// a switch does can change it. Not the cap: each tab's list is still
    /// clamped to the budget (`PanelHeight.listBudget`) and no further, so a
    /// two-account fleet stays a short panel (review finding 11).
    ///
    /// Why every tab is drawn rather than each one's height computed: what
    /// makes a tab tall is its summary lines, its list and its footer, and two
    /// of those wrap by their own text. Only a layout pass knows them, and a
    /// tab nobody draws gets no layout pass.
    ///
    /// The hidden tabs are drawn at zero opacity, take no clicks, leave the
    /// accessibility tree and are disabled, so neither the pointer, VoiceOver
    /// nor the Tab key can reach them. Their identity never changes with the
    /// selection, so a switch fires no `onAppear`: anything a tab does when it
    /// is looked at (the Peers tab's read, the Accounts tab's lent-to read)
    /// keys on `selected` instead.
    ///
    /// The footer sits at the bottom of the shared height on every tab, after
    /// a spacer, so Quit stays where it is too.
    private var tabLayers: some View {
        TallestTabLayout {
            ForEach(tabs, id: \.self) { tab in
                let shown = tab == selected
                VStack(alignment: .leading, spacing: 0) {
                    summary(tab)
                    content(tab)
                    Spacer(minLength: 0)
                    footer(tab)
                }
                .opacity(shown ? 1 : 0)
                .allowsHitTesting(shown)
                .accessibilityHidden(!shown)
                .disabled(!shown)
            }
        }
    }
}

/// Lays every child at the same origin and the same size: the width it is
/// offered and the tallest child's own height
/// (``PanelHeight/sharedTabHeight(_:)``).
///
/// A `ZStack` sizes to its tallest child too, but it does not hand that height
/// back to the shorter ones, so their footers would sit at their own height
/// instead of the panel's bottom. Here each child is measured at its ideal
/// height and then placed with the shared one, which its spacer fills.
struct TallestTabLayout: Layout {
    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let ideal = ProposedViewSize(width: proposal.width, height: nil)
        let sizes = subviews.map { $0.sizeThatFits(ideal) }
        return CGSize(
            width: proposal.width ?? sizes.map(\.width).max() ?? 0,
            height: PanelHeight.sharedTabHeight(sizes.map(\.height)))
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) {
        for subview in subviews {
            subview.place(
                at: bounds.origin, anchor: .topLeading,
                proposal: ProposedViewSize(width: bounds.width, height: bounds.height))
        }
    }
}
