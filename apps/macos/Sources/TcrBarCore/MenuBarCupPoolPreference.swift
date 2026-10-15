import Combine
import Foundation

/// Which pool the menu-bar cup follows in a fleet that holds both Claude and Codex
/// accounts. The count beside the cup always shows both pools; this picks only the one
/// whose fill and tint the cup draws.
///
/// Ignored in a fleet with one provider, which has one pool to show.
public enum MenuBarCupPool: String, CaseIterable, Sendable {
    /// The pool with less ready, so a dead pool never hides behind a healthy one. The
    /// default.
    case emptier
    /// Claude's pool, for an operator who keeps a Codex account at its weekly limit for
    /// days and does not want an empty cup for all of them.
    case claude
    /// Codex's pool.
    case codex

    /// The picker's word for this choice.
    public var title: String {
        switch self {
        case .emptier: return "Whichever pool is emptier"
        case .claude: return "Claude accounts"
        case .codex: return "Codex accounts"
        }
    }

    /// The Settings row's label.
    public static let settingsLabel = "Cup shows"

    /// The caption under that row.
    public static let settingsCaption =
        "With Claude and Codex accounts, the cup fills to the pool with less ready. "
        + "The count beside it shows both, Claude first."

    /// The counts toggle's caption in a fleet holding both providers, where the count it
    /// turns on has one part per pool. A fleet with one provider keeps the toggle's own
    /// caption.
    public static let mixedCountsCaption =
        "The cup's fill level already shows capacity; this spells it out as "
        + "\u{201c}9/13 \u{00b7} 2/3\u{201d}, Claude first, in tabular digits."
}

/// "Cup shows", persisted in `UserDefaults`. The same shape as
/// ``MenuBarCountsPreference``, for the same reason: there is no `App` or `View` to hang
/// an `@AppStorage` off, and a plain read would not publish the change back to the picker
/// that made it.
///
/// An absent key, or a value this build cannot name, reads as
/// ``MenuBarCupPool/emptier``: a hand edit or a word from a later build never strands the
/// cup on a pool, it falls back to the default.
@MainActor
public final class MenuBarCupPoolPreference: ObservableObject {

    /// The `UserDefaults` key. Do not change it, the same trap
    /// ``LaunchPreference/startServerAtLaunchKey`` documents: renaming it fails nothing
    /// and silently resets an operator's choice. `MenuBarCupPoolPreferenceTests` pins the
    /// literal.
    public static let cupPoolKey = "MenuBarCupPool"

    private let defaults: UserDefaults

    /// Written through to `UserDefaults` on every change, so the pool the menu bar follows
    /// and the pool the next launch reads cannot disagree.
    @Published public var pool: MenuBarCupPool {
        didSet { defaults.set(pool.rawValue, forKey: Self.cupPoolKey) }
    }

    /// - Parameter defaults: injected so a test, or a render, can use a scratch suite
    ///   rather than the operator's real preferences.
    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        // Property initialisation in `init` does not fire `didSet`, so reading the stored
        // value here cannot write it straight back.
        self.pool =
            defaults.string(forKey: Self.cupPoolKey).flatMap(MenuBarCupPool.init(rawValue:))
            ?? .emptier
    }
}
