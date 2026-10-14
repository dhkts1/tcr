import CoreGraphics

/// How wide each tab in the panel's segmented strip is drawn, so that every
/// tab keeps the same least room between its pill edge and its content.
///
/// The strip used to split into equal quarters. "Accts" is narrower than its
/// quarter and looked padded by the leftover; "Sessions" beside a badge is
/// wider than a quarter, took its own width, and drew its pill flush against
/// the icon and the badge (Gil, 2026-10-14: "the buttons for sessions has no
/// padding, only accts"). Three cases, in order:
///
/// 1. Every tab's content plus an inset on each side fits a quarter: four
///    equal quarters, the strip exactly as it was, so the Accts-selected
///    panel with no badges does not move.
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
}
