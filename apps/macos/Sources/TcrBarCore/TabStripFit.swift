import CoreGraphics

/// How wide each tab in the panel's segmented strip is drawn, so that every
/// tab keeps the same least room between its pill edge and its content, and
/// where each tab and its label sit.
///
/// A tab's content is its icon and its label, and nothing that changes with
/// the data. The badge (a count) is drawn over the tab's top-trailing corner
/// and takes no room, so the strip is laid out from the labels alone and
/// never moves when a count appears, vanishes or gains a digit (Gil,
/// 2026-10-14, of 1.1.20, where the badge sat inline and every count change
/// re-laid the strip: "avoid layout shifts. now whats going on with our tabs
/// is horrible").
///
/// An inline badge slot reserved at its worst case does not fit. The strip has
/// 337 pt; the four labels with their icons and an inset on each side want
/// about 269, which leaves about 68 for three badged tabs (Sessions, Tools,
/// Peers), and one slot for a single digit already costs 25 (the gap, the
/// pill's padding and the digit). So the room a badge can never take is
/// room it is not given.
///
/// The width rule, for labels that might one day not fit, in order:
///
/// 1. Every tab's content plus an inset on each side fits a quarter: four
///    equal quarters. Today's four labels always land here.
/// 2. It does not, but the contents and their insets fit the strip: each tab
///    gets its content plus both insets, and only what is left over is shared,
///    equally. Every tab then has the same room on each side, inset included.
/// 3. They do not fit at all: each tab is scaled down from its content plus
///    insets in proportion, so the strip still fits one line and the labels,
///    which are `lineLimit(1)`, cut rather than wrap.
///
/// Pure and in the library for the reason ``AccountLineFit`` is: the test
/// target links `TcrBarCore` only, and the view and the tests must lay the
/// strip out with one function.
public enum TabStripFit {
    /// One width per tab, in order, for tabs whose content wants
    /// `contentWidths`, keeping `inset` on each side of every tab's content,
    /// in a strip with `available` points left after its gaps.
    public static func widths(
        contentWidths: [CGFloat], inset: CGFloat, available: CGFloat
    ) -> [CGFloat] {
        guard !contentWidths.isEmpty, available > 0 else {
            return contentWidths.map { _ in 0 }
        }
        let count = CGFloat(contentWidths.count)
        let share = available / count
        let wanted = contentWidths.map { $0 + 2 * inset }
        if wanted.allSatisfy({ $0 <= share }) {
            return contentWidths.map { _ in share }
        }
        let total = wanted.reduce(0, +)
        if total <= available {
            let slack = (available - total) / count
            return wanted.map { $0 + slack }
        }
        let scale = available / total
        return wanted.map { $0 * scale }
    }

    /// The largest count a badge spells out. Above it the badge reads
    /// ``badgeOverflow``, so the widest badge the strip can ever draw is known
    /// before any count arrives and stays inside its tab's corner.
    public static let badgeCap = 99
    public static let badgeOverflow = "99+"

    /// The text a badge draws for `count`, or nil when it draws none. A zero
    /// never draws a badge, the panel's standing rule for counts.
    public static func badgeText(_ count: Int?) -> String? {
        guard let count, count > 0 else { return nil }
        return count > badgeCap ? badgeOverflow : "\(count)"
    }

    /// One tab's parts, in points: the icon box and the gap after it, the
    /// label, and the badge pill when the tab draws one.
    ///
    /// `badge` is here so a caller says what the tab shows, and so the gate
    /// can vary it: ``frames(_:inset:spacing:available:)`` must give the same
    /// answer whatever it holds.
    public struct Tab: Equatable {
        public var leading: CGFloat
        public var label: CGFloat
        public var badge: CGFloat?

        public init(leading: CGFloat, label: CGFloat, badge: CGFloat?) {
            self.leading = leading
            self.label = label
            self.badge = badge
        }

        /// What the tab lays out: icon, gap, label. Never the badge.
        public var content: CGFloat { leading + label }
    }

    /// Where one tab and its label are drawn, from the strip's leading edge.
    public struct Frame: Equatable {
        public var x: CGFloat
        public var width: CGFloat
        public var labelX: CGFloat
        public var labelWidth: CGFloat
    }

    /// Every tab's frame and its label's, as `SegmentedTabs` draws them: each
    /// tab at ``widths(contentWidths:inset:available:)``, `spacing` apart,
    /// its content centred in it.
    public static func frames(
        _ tabs: [Tab], inset: CGFloat, spacing: CGFloat, available: CGFloat
    ) -> [Frame] {
        let widths = widths(contentWidths: tabs.map(\.content), inset: inset, available: available)
        var x: CGFloat = 0
        var out: [Frame] = []
        for (tab, width) in zip(tabs, widths) {
            let contentX = x + (width - tab.content) / 2
            out.append(
                Frame(x: x, width: width, labelX: contentX + tab.leading, labelWidth: tab.label))
            x += width + spacing
        }
        return out
    }
}
