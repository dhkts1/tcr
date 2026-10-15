import AppKit

/// Whether an account card's header line has room to draw the plan.
///
/// **The rule, in one sentence: the plan is drawn whole or not at all**, so it
/// is drawn only when the name, a handle shrunk to its floor and the plan all
/// fit the width the pills and the card's own controls leave.
///
/// A plan that truncates says less than one that is absent. Cut to its first
/// letter it reads as a stray glyph beside the name rather than as a plan, and
/// the reader cannot tell `Team 5x` from `Team Standard` by its `T`; absent, the
/// card says nothing about the plan, which is true and looks it. The handle is
/// the piece that still truncates, because half a domain is still a domain and
/// the half that survives is the half that identifies it.
///
/// Lives here rather than in the card so the card and the tests that grade it
/// measure with ONE function: a second copy of this arithmetic in a test would
/// agree with the card only until either was edited.
public enum NameRowFit {

    /// The width `Text(_:)` draws a string at, for the system font at a size
    /// and weight.
    ///
    /// `NSFont.systemFont(ofSize:weight:)` is the same face SwiftUI's
    /// `.system(size:weight:)` resolves to, so this measures what the panel
    /// actually draws rather than something close to it.
    public static func textWidth(_ text: String, size: CGFloat, weight: NSFont.Weight) -> CGFloat {
        (text as NSString)
            .size(withAttributes: [.font: NSFont.systemFont(ofSize: size, weight: weight)]).width
    }

    /// The width the card's trailing controls take: the gearshape glyph the
    /// actions menu draws, at the body size `AccountRow` gives it.
    ///
    /// The one term in the header with no token behind it, because it is an
    /// image rather than a layout constant, so it is measured as the symbol
    /// itself. The 13 pt is that menu label's own body font; it lives here
    /// beside the measurement rather than in the card, where a bare number in
    /// a panel file is what `scripts/check-panel-v4.sh` exists to refuse.
    public static let actionsSlotWidth: CGFloat = {
        let configuration = NSImage.SymbolConfiguration(pointSize: 13, weight: .regular)
        guard
            let image = NSImage(systemSymbolName: "gearshape", accessibilityDescription: nil)?
                .withSymbolConfiguration(configuration)
        else {
            // Not a silent fallback: the symbol is a system one and resolves on
            // every macOS this app runs on. If it ever does not, reserving a
            // square of the glyph's own point size over-reserves rather than
            // letting a plan draw into the gear.
            return 13
        }
        return image.size.width
    }()

    /// The width of the `Re-login…` button a card draws in its header beside the
    /// actions menu, on an account that needs signing in again.
    ///
    /// Measured the way ``actionsSlotWidth`` measures its glyph: as the control
    /// itself. The card's button is a bordered SwiftUI `Button` at the small
    /// control size, which draws its label at the small control font, 11 pt,
    /// whatever font its view asks for, and fits exactly the width this push
    /// button at the same size and font reports. Read on the main thread, where
    /// the card is drawn.
    public static let reloginButtonWidth: CGFloat = {
        let button = NSButton(title: "Re-login\u{2026}", target: nil, action: nil)
        button.bezelStyle = .push
        button.controlSize = .small
        button.font = .systemFont(ofSize: NSFont.systemFontSize(for: .small))
        return button.fittingSize.width
    }()

    /// The width the header's trailing controls take: the actions menu, and, when
    /// the card reserves it, the `Re-login…` button `gap` ahead of it.
    ///
    /// Only a Codex card reserves the button (``Account/reservesReloginInNameRow``).
    /// A Claude card drawing the same button still subtracts the menu alone, as it
    /// always has, so every Claude card keeps its name room to the point.
    public static func actionsWidth(reservesRelogin: Bool, gap: CGFloat) -> CGFloat {
        reservesRelogin ? actionsSlotWidth + reloginButtonWidth + gap : actionsSlotWidth
    }

    /// The narrowest a truncated handle is still drawn at: `@` and the
    /// ellipsis the truncation puts after it.
    ///
    /// This is what the handle is assumed to shrink TO while deciding about the
    /// plan. Assuming it keeps its full width instead would drop the plan off
    /// cards that have room for both once the handle gives way, which is the
    /// order the row lays out in.
    public static func handleFloorWidth(size: CGFloat) -> CGFloat {
        textWidth("@\u{2026}", size: size, weight: .medium)
    }

    /// Whether the plan label may be drawn.
    ///
    /// - Parameters:
    ///   - localPart: everything before the first `@`, which never truncates.
    ///   - domain: `@example.com`, or `nil` for a name with no `@` at all.
    ///   - plan: the label itself, measured whole because whole is the only way
    ///     it is ever drawn.
    ///   - available: the header's width less the pills, the controls and every
    ///     gap between them. The caller owns those tokens.
    ///   - nameSize: the name row's font size at the density being drawn.
    ///   - planSize: the plan's own, smaller, size.
    ///   - gap: the spacing between the row's pieces, counted once per gap.
    ///
    /// Letter tracking is left out. The name's is negative, so the sum here
    /// over-reserves by about a point on a long name, and over-reserving errs
    /// toward dropping a plan that would just have fitted rather than drawing
    /// one that then truncates, which is the direction this rule exists to
    /// fail in.
    public static func drawsPlan(
        localPart: String, domain: String?, plan: String, available: CGFloat,
        nameSize: CGFloat, planSize: CGFloat, gap: CGFloat
    ) -> Bool {
        guard !plan.isEmpty else { return false }
        var needed = textWidth(localPart, size: nameSize, weight: .semibold)
        if domain != nil {
            needed += gap + handleFloorWidth(size: nameSize)
        }
        needed += gap + textWidth(plan, size: planSize, weight: .regular)
        return needed <= available
    }
}
