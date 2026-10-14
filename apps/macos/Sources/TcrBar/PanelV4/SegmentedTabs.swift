import SwiftUI
import TcrBarCore

/// `.seg` / `.tab` / `.badge` — a real tablist of real buttons.
///
/// Three measured fixes over the pre-v4 strip. The selected item carries
/// `rgba(255,255,255,.12)` and a shadow, where the old one washed .045 over its
/// container — +10 per channel against the mockup's +26, faint enough to read as
/// unselected. The icons draw in a 14 pt box instead of the 23–34 pt of glyph
/// they had grown to. And the badges render from the same counts on EVERY tab,
/// including the one being looked at: the old strip drew zero badge pixels on
/// Accounts while showing both counts on the other two.
struct SegmentedTabs: View {
    let tabs: [PanelTab]
    let selected: PanelTab
    /// A count per tab, or no entry at all. A zero never draws a badge — the
    /// panel's standing rule for counts.
    let badges: [PanelTab: Int]
    let onSelect: (PanelTab) -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TabStripLayout(spacing: V4.segGap, inset: V4.tabInset) {
            ForEach(tabs, id: \.self) { tab in
                item(tab)
            }
        }
        .padding(V4.segPadding)
        .background(
            RoundedRectangle(cornerRadius: V4.segRadius).fill(Tok.ink.opacity(V4.segFillAlpha))
        )
        .padding(.bottom, V4.segMarginBottom)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("TcrBar panel")
    }

    private func item(_ tab: PanelTab) -> some View {
        let isOn = tab == selected
        // The switch itself is NOT wrapped in `withAnimation`: a broad
        // transaction here animated every layout change in the panel (the
        // content swap, the list height, each row sliding to its new place),
        // which read as "everything moves" (Gil, 2026-09-14). Only the
        // selected pill's fill and the label colour ease, scoped to `isOn`,
        // the same way `Tokens.swift` scopes every other animation in this
        // panel. The content swaps in one frame, like a native segmented control.
        return Button {
            onSelect(tab)
        } label: {
            HStack(spacing: V4.tabGap) {
                Image(systemName: tab.systemImage)
                    .font(.system(size: V4.tabLabelSize))
                    .frame(width: V4.tabIconBox, height: V4.tabIconBox)
                // ONE line, at its own width, never a share of the strip.
                //
                // Equal quarters of the strip are 84.25 pt. Measured at the
                // system font, "Sessions" beside a two-digit badge wants
                // 106.2 pt and so broke into `Sess` over `ions` on the
                // released panel. Nothing in the strip's own tokens can pay
                // for it: taking `segGap` to zero returns 2.25 pt, and a
                // smaller badge font is larger than the one already used.
                //
                // Their own widths fit with room to spare: the four together
                // want 303.8 pt of the strip's 337, even at `Sessions 120`
                // and `Tools 12`, which leaves enough for `V4.tabInset` on
                // each side of each tab (`TabStripLayout` below hands those
                // widths out). `fixedSize` is what reports that ideal width
                // to the layout instead of accepting a smaller one,
                // `lineLimit(1)` is what makes a label that somehow still
                // cannot fit ellipsise rather than grow the strip a second
                // line, and `SegmentedTabsFitTests` is what fails if a longer
                // label ever spends the slack.
                Text(tab.title)
                    .font(V4.font(V4.tabLabelSize, .semibold))
                    .tracking(V4.tabLabelTracking)
                    .lineLimit(1)
                    .fixedSize(horizontal: true, vertical: false)
                if let count = badges[tab], count > 0 {
                    // The same rule as the label, and it is not optional once
                    // the label has it. With only the label fixed, an item
                    // still holds its equal quarter and the compression moves
                    // to whatever can still give: rendered at `Sessions 120`
                    // and `Tools 12`, the three-digit badge was squeezed to a
                    // bare sliver and the two-digit one broke into `1` over
                    // `2`. A count that cannot be read is the one thing a
                    // badge is for.
                    Text("\(count)")
                        .font(V4.font(V4.badgeSize))
                        .foregroundStyle(Tok.ink)
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                        .contentTransition(.numericText())
                        .padding(.horizontal, V4.badgePaddingH)
                        .background(
                            RoundedRectangle(cornerRadius: V4.badgeRadius)
                                .fill(Tok.ink.opacity(V4.badgeFillAlpha))
                        )
                }
            }
            .foregroundStyle(isOn ? Tok.ink : Tok.dim)
            .frame(maxWidth: .infinity)
            .frame(minHeight: V4.tabMinHeight)
            .background(
                RoundedRectangle(cornerRadius: V4.tabRadius)
                    .fill(isOn ? Tok.ink.opacity(V4.tabSelectedAlpha) : Color.clear)
                    .shadow(
                        color: isOn ? Color.black.opacity(0.4) : .clear,
                        radius: 1, x: 0, y: 1)
            )
            .animation(reduceMotion ? nil : .easeOut(duration: V4.hoverDuration), value: isOn)
            .contentShape(Rectangle())
        }
        .buttonStyle(V4PressStyle(cornerRadius: V4.tabRadius))
        .accessibilityLabel(badges[tab].map { "\(tab.title), \($0)" } ?? tab.title)
        .accessibilityAddTraits(isOn ? [.isSelected] : [])
    }
}

/// Lays the tabs out side by side at ``TabStripFit``'s widths: equal quarters
/// when every tab's content and its insets fit one, otherwise each tab's own
/// content plus `inset` on both sides, with only the leftover shared.
///
/// An `HStack` of `.frame(maxWidth: .infinity)` items cannot do this: it
/// offers each item an equal share, and an item whose content is wider than
/// its share takes exactly its content width, so its pill ends at its badge
/// (Gil, 2026-10-14: "the buttons for sessions has no padding"). Each item
/// here still fills the width it is given and centres its content in it, so
/// the room on each side is half of what its width leaves over its content.
private struct TabStripLayout: Layout {
    let spacing: CGFloat
    let inset: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let widths = widths(in: proposal.width, subviews: subviews)
        let height =
            zip(subviews, widths)
            .map { subview, width in
                subview.sizeThatFits(ProposedViewSize(width: width, height: proposal.height)).height
            }
            .max() ?? 0
        return CGSize(width: widths.reduce(0, +) + gaps(subviews), height: height)
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) {
        var x = bounds.minX
        for (subview, width) in zip(subviews, widths(in: bounds.width, subviews: subviews)) {
            subview.place(
                at: CGPoint(x: x, y: bounds.midY), anchor: .leading,
                proposal: ProposedViewSize(width: width, height: bounds.height))
            x += width + spacing
        }
    }

    /// Each tab's width in a strip `width` wide. With no width to share (an
    /// ideal-size or unbounded proposal) each tab asks for its content and
    /// its two insets.
    private func widths(in width: CGFloat?, subviews: Subviews) -> [CGFloat] {
        let contents = subviews.map { $0.sizeThatFits(.unspecified).width }
        guard let width, width.isFinite else { return contents.map { $0 + inset * 2 } }
        return TabStripFit.widths(
            contentWidths: contents, inset: inset, available: width - gaps(subviews))
    }

    private func gaps(_ subviews: Subviews) -> CGFloat {
        spacing * CGFloat(max(subviews.count - 1, 0))
    }
}
