import XCTest

@testable import TcrBarCore

/// The two alerts that put a cost on restarting the proxy: taking over the port
/// from the panel, and Restart… in Settings.
///
/// Codex sessions lose their affinity pins on a restart too, so once the fleet holds
/// a Codex account each alert names them; Anthropic's per-account prompt cache is
/// still the cost it describes, and only Claude sessions pay it. Without a Codex
/// account each alert is today's, word for word.
final class RestartAlertCopyTests: XCTestCase {

    private let opening =
        "This kills the proxy currently holding the port and starts a new one in its place."
    private let closing =
        "Only do this if the running proxy is stuck or is a build you need to replace."

    func testTheTakeoverAlertIsTodaysWithoutCodex() {
        XCTAssertEqual(
            RestartAlertCopy.takeoverInformative(holdsCodex: false),
            opening + "\n\n"
                + "That wipes the session-to-account pin map. Anthropic's prompt cache is "
                + "per-account, so every live Claude session is re-pinned to a different account "
                + "and pays a full cold prompt-cache prefix on its next request. It is the most "
                + "expensive thing this app can do.\n\n" + closing)
    }

    func testTheTakeoverAlertNamesCodexSessions() {
        XCTAssertEqual(
            RestartAlertCopy.takeoverInformative(holdsCodex: true),
            opening + "\n\n"
                + "That wipes the session-to-account pin map. Anthropic's prompt cache is "
                + "per-account, so every live Claude session is re-pinned to a different account "
                + "and pays a full cold prompt-cache prefix on its next request; every live Codex "
                + "session is re-pinned too. It is the most expensive thing this app can do.\n\n"
                + closing)
    }

    func testTheRestartAlertIsTodaysWithoutCodex() {
        XCTAssertEqual(
            RestartAlertCopy.restartInformative(holdsCodex: false),
            "Every live session loses its prompt cache. Anthropic's cache is per-account, so a "
                + "session that comes back on a different account pays a full cold prefix — the "
                + "most expensive event in this system (CLAUDE.md). Session-affinity pins under "
                + "15 minutes old are restored.")
    }

    func testTheRestartAlertNamesBothProviders() {
        XCTAssertEqual(
            RestartAlertCopy.restartInformative(holdsCodex: true),
            "Every live Claude Code and Codex session can move to another account. Anthropic's "
                + "cache is per-account, so a Claude session that comes back on a different "
                + "account pays a full cold prefix — the most expensive event in this system "
                + "(CLAUDE.md). Session-affinity pins under 15 minutes old are restored.")
    }
}
