import Foundation

/// The words of the two alerts that put a cost on restarting the proxy: taking over
/// the port from the panel, and Restart… in Settings.
///
/// A restart wipes the session-to-account pin map, and Codex sessions are pinned
/// too. So once the fleet holds a Codex account each alert names Codex sessions as
/// well; Anthropic's per-account prompt cache is still the cost it describes, and only
/// a Claude session pays it. Without a Codex account each alert is today's sentence,
/// word for word.
public enum RestartAlertCopy {
    /// The take-over alert's informative text.
    public static func takeoverInformative(holdsCodex: Bool) -> String {
        let pinMap =
            holdsCodex
            ? "That wipes the session-to-account pin map. Anthropic's prompt cache is "
                + "per-account, so every live Claude session is re-pinned to a different "
                + "account and pays a full cold prompt-cache prefix on its next request; every "
                + "live Codex session is re-pinned too. It is the most expensive thing this app "
                + "can do."
            : "That wipes the session-to-account pin map. Anthropic's prompt cache is "
                + "per-account, so every live Claude session is re-pinned to a different "
                + "account and pays a full cold prompt-cache prefix on its next request. It is "
                + "the most expensive thing this app can do."
        return "This kills the proxy currently holding the port and starts a new one in its "
            + "place.\n\n" + pinMap + "\n\n"
            + "Only do this if the running proxy is stuck or is a build you need to replace."
    }

    /// The restart alert's informative text.
    public static func restartInformative(holdsCodex: Bool) -> String {
        holdsCodex
            ? "Every live Claude Code and Codex session can move to another account. "
                + "Anthropic's cache is per-account, so a Claude session that comes back on a "
                + "different account pays a full cold prefix — the most expensive event in this "
                + "system (CLAUDE.md). Session-affinity pins under 15 minutes old are restored."
            : "Every live session loses its prompt cache. Anthropic's cache is per-account, so "
                + "a session that comes back on a different account pays a full cold prefix — "
                + "the most expensive event in this system (CLAUDE.md). Session-affinity pins "
                + "under 15 minutes old are restored."
    }
}
