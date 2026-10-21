import XCTest

@testable import TcrBarCore

/// Settings › Menu Bar › "Icon shows": which pool the menu-bar cup follows in a fleet that
/// holds both Claude and Codex accounts. The same storage shape as
/// `MenuBarCountsPreferenceTests`, with the default the cup needs: the emptier pool.
@MainActor
final class MenuBarCupPoolPreferenceTests: XCTestCase {

    private var suiteName = ""
    private var defaults = UserDefaults.standard

    override func setUp() {
        super.setUp()
        // A scratch suite, never `.standard`: a test that writes the real key would
        // silently move the operator's own cup.
        suiteName = "tcrbar.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suiteName) ?? .standard
    }

    override func tearDown() {
        UserDefaults.standard.removePersistentDomain(forName: suiteName)
        super.tearDown()
    }

    /// An absent key, or a word this build cannot name, is the emptier pool: the default
    /// that never lets a dead pool hide behind a healthy one.
    func testTheDefaultIsEmptier() {
        XCTAssertNil(defaults.object(forKey: MenuBarCupPoolPreference.cupPoolKey))
        XCTAssertEqual(MenuBarCupPoolPreference(defaults: defaults).pool, .emptier)

        defaults.set("bogus", forKey: MenuBarCupPoolPreference.cupPoolKey)
        XCTAssertEqual(MenuBarCupPoolPreference(defaults: defaults).pool, .emptier)
    }

    /// The key and the three stored words are pinned: renaming either one silently resets
    /// an operator's choice.
    func testTheKeyAndValuesAreLiteral() {
        XCTAssertEqual(MenuBarCupPoolPreference.cupPoolKey, "MenuBarCupPool")
        XCTAssertEqual(MenuBarCupPool.allCases.map(\.rawValue), ["emptier", "claude", "codex"])

        let preference = MenuBarCupPoolPreference(defaults: defaults)
        preference.pool = .codex
        XCTAssertEqual(defaults.string(forKey: MenuBarCupPoolPreference.cupPoolKey), "codex")
        XCTAssertEqual(MenuBarCupPoolPreference(defaults: defaults).pool, .codex)
    }

    /// The row's words, verbatim.
    func testTheSettingsWordsAreVerbatim() {
        XCTAssertEqual(MenuBarCupPool.settingsLabel, "Icon shows")
        XCTAssertEqual(
            MenuBarCupPool.allCases.map(\.title),
            ["Whichever pool is emptier", "Claude accounts", "Codex accounts"])
        XCTAssertEqual(
            MenuBarCupPool.settingsCaption,
            "With Claude and Codex accounts, the icon lights its rays for the pool with less ready. "
                + "The count beside it shows both, Claude first.")
        XCTAssertEqual(
            MenuBarCupPool.mixedCountsCaption,
            "The icon's lit rays already show capacity; this spells it out as "
                + "\u{201c}9/13 \u{00b7} 2/3\u{201d}, Claude first, in tabular digits.")
    }
}
