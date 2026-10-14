import Foundation

/// Why a "Start server" control should be disabled rather than merely
/// retried, derived from ``ServerController/State``, the same check the app
/// already runs before starting. A colleague's "Start server" was disabled
/// with no reason at all; this names one.
///
/// `takeover_port` (`src/singleton.rs`) prints one extra stderr line whenever
/// a NON-proxy process holds the port, ahead of the bind failure that
/// follows it either way: `":{port} is held by a non-proxy process (pid
/// {pid}): {cmd}"`. `ServerController.classifyExit` folds that stderr,
/// verbatim, into `.incumbentHoldsPort`'s `message`, so a safe start's own
/// failed attempt is where this line comes from, not a separate port probe.
extension ServerController.State {
    /// The plain sentence for a disabled "Start server" control, or `nil` when
    /// starting again is not futile: every state but a non-proxy holder,
    /// including a `tcr` proxy already on the port (the common, benign case,
    /// starting again just reports "already running") and a holder that
    /// answered nothing at all (there is no name to give it).
    public var startDisabledReason: String? {
        guard case .incumbentHoldsPort(let message) = self else { return nil }
        return Self.nonProxyHolderReason(inMessage: message)
    }

    /// Pure parse of `takeover_port`'s non-proxy-holder line. `nil` when the
    /// line is not present. Most `.incumbentHoldsPort` messages never printed
    /// it, because most incumbents are another `tcr`.
    static func nonProxyHolderReason(inMessage message: String) -> String? {
        guard
            let regex = try? NSRegularExpression(
                pattern: #":(\d+) is held by a non-proxy process \(pid \d+\): ([^\p{Pd}\n]+)"#)
        else { return nil }
        let range = NSRange(message.startIndex..., in: message)
        guard let match = regex.firstMatch(in: message, range: range),
            let portRange = Range(match.range(at: 1), in: message),
            let nameRange = Range(match.range(at: 2), in: message)
        else { return nil }
        let port = message[portRange]
        let name = message[nameRange].trimmingCharacters(in: .whitespaces)
        return "Port \(port) is held by \(name). Take over stops it."
    }
}

/// The refusal a `tcr` built before Codex accounts gives when the config it loads holds
/// one, and the sentence TcrBar says in its place.
///
/// The producer is tcr's config loader (`src/config.rs`): an account whose
/// `credentialVersion` is newer than the build understands is refused, with the whole
/// load, rather than read in a shape it does not know. A Codex account is written at a
/// newer version than any `tcr` from before Codex accounts understands, so every
/// command that loads the config, the server's own start included, fails with this
/// refusal, and tcr's words name a version, not the fix. TcrBar matches it to say which
/// tcr is too old, and what to do.
///
/// The match skips the account's name and both numbers: the name is a person's, and
/// the numbers move each time a credential shape does. The words between them are
/// pinned against the loader's source by a test.
public enum PredatesCodexRefusal {
    /// Whether `text` carries the config loader's refusal of a newer credential shape.
    public static func matches(_ text: String) -> Bool {
        guard
            let regex = try? NSRegularExpression(
                pattern: #"carries credentialVersion \d+, which this build only understands up to \d+"#)
        else { return false }
        return regex.firstMatch(in: text, range: NSRange(text.startIndex..., in: text)) != nil
    }

    /// `text` as a single line shows it: the sentence naming the tcr inside TcrBar when
    /// `text` carries the refusal, and otherwise `text` unchanged.
    public static func displayed(_ text: String) -> String {
        matches(text) ? CodexSignInCopy.predatesCodexSentence : text
    }
}
