import AppKit

/// What VoiceOver says while keep-awake holds the Mac up: the one fact about
/// keep-awake's mark that outlives the mark itself.
///
/// ## History
///
/// This type used to build a tinted coffee cup, first as a second glyph beside
/// a capacity gauge and then as the silhouette of the one-cup menu-bar mark.
/// The menu-bar glyph is now the ray mark (``MenuBarMark``), which carries
/// keep-awake as its teal tint, so the cup and its image builder are gone and
/// only the spoken string is left.
public enum KeepAwakeGlyph {
    /// What VoiceOver says. The menu bar is the one surface with no room for a
    /// label, so this is the only place the state is spoken.
    public static let accessibilityDescription = "Keeping this Mac awake"
}
