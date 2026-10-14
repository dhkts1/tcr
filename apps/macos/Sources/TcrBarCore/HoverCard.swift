import CoreGraphics

/// Option H's hover card (Gil, 2026-10-14): pointing at an account's one line
/// floats that account's full card over the list beside it. These are the two
/// rules the view must not get wrong, kept pure so a test can run them: which
/// side of the row the card goes on, and when it goes away.
///
/// The card is laid out in a box the panel already has, never in one it grows:
/// it hangs from an overlay on the panel root, so it takes no part in the
/// list's or the panel's size. A card expanded in place would change the
/// height of the content that sizes the popover, which is the layout cycle
/// `ffe8a86` cut (see `FleetView`'s note on tabs and fixed height budgets).

/// Which side of its row the floating card sits on.
public enum HoverCardEdge: Equatable, Sendable {
    case below
    case above
}

public enum HoverCardPlacement {
    /// Below the row, or above it when the row's middle is in the lower half
    /// of the visible list, so the card always opens toward the larger room.
    public static func edge(row: CGRect, list: CGRect) -> HoverCardEdge {
        row.midY > list.midY ? .above : .below
    }

    /// The box the card is laid out in, in the panel's own coordinates: the
    /// panel's width less `inset` on each side, from the row's bottom edge to
    /// the panel's bottom (``HoverCardEdge/below``), or from the panel's top
    /// to the row's top edge (``HoverCardEdge/above``). The view pins the card
    /// to the edge that touches the row.
    ///
    /// Always inside `panel`, so the card can cover the rows and the footer
    /// but never sit past the panel's edge, which the popover would clip.
    public static func region(
        row: CGRect, list: CGRect, panel: CGRect, inset: CGFloat
    ) -> CGRect {
        let x = panel.minX + inset
        let width = max(0, panel.width - 2 * inset)
        switch edge(row: row, list: list) {
        case .below:
            let top = min(max(row.maxY, panel.minY), panel.maxY)
            return CGRect(x: x, y: top, width: width, height: panel.maxY - top)
        case .above:
            let bottom = min(max(row.minY, panel.minY), panel.maxY)
            return CGRect(x: x, y: panel.minY, width: width, height: bottom - panel.minY)
        }
    }
}

/// When the hover card shows, and for which account.
///
/// The pointer can leave the row and enter the card itself, which floats
/// against it: the card's gear is the same actions menu a click on the row
/// opens, so it should still work when the pointer reaches it. A leave
/// therefore does not hide the card at once; it waits for ``settle()``, which
/// the view runs a short grace after any leave, and which hides the card only
/// when the pointer is on neither a row nor the card.
public struct HoverCardTracker: Equatable, Sendable {
    /// The account whose line the pointer is on, if any.
    public private(set) var row: String?
    /// Whether the pointer is on the card.
    public private(set) var onCard: Bool = false
    /// The account whose card is showing, if any.
    public private(set) var shown: String?

    /// `shown` seeds a card with no pointer at all, for the render harness,
    /// which cannot hover.
    public init(shown: String? = nil) {
        self.shown = shown
    }

    public mutating func rowEntered(_ account: String) {
        row = account
        shown = account
    }

    /// A leave from a row the pointer has already left for another one is
    /// stale and changes nothing: the newer row's card stays.
    public mutating func rowLeft(_ account: String) {
        if row == account { row = nil }
    }

    public mutating func cardEntered() {
        onCard = true
    }

    public mutating func cardLeft() {
        onCard = false
    }

    /// Hides the card when the pointer is on neither a row nor the card.
    public mutating func settle() {
        if row == nil && !onCard { shown = nil }
    }
}
