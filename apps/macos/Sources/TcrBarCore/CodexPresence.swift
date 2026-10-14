import Foundation

/// The evidence of codex that an empty or Claude-only panel needs before it mentions Codex.
///
/// Any one of the four sources is enough, and none of them reads a credential: a Codex row
/// in the fleet, a codex executable that resolves, a codex home folder that exists, or the
/// path a person stored by hand.
///
/// It is a value rather than a lookup, so the render harness can always pass ``absent``: a
/// render must never see this Mac's own `~/.codex`.
///
/// ``hasLoginFile`` is not a fifth source. It says whether the codex home holds a login, which
/// the folder alone does not, and it adds nothing to ``isPresent``: the file implies the folder.
public struct CodexPresence: Equatable, Sendable {
    /// Any Codex row in the fleet, in any state.
    public var fleetHoldsCodex: Bool
    /// The codex executable this Mac resolves to, or `nil` when none resolves.
    public var codexExecutable: String?
    /// `$CODEX_HOME`, or `~/.codex` when that folder exists.
    public var codexHome: String?
    /// The stored `CodexExecutablePath` preference.
    public var storedExecutable: String?
    /// `<codexHome>/auth.json` exists; existence only, never opened.
    public var hasLoginFile: Bool

    public init(
        fleetHoldsCodex: Bool = false, codexExecutable: String? = nil,
        codexHome: String? = nil, storedExecutable: String? = nil, hasLoginFile: Bool = false
    ) {
        self.fleetHoldsCodex = fleetHoldsCodex
        self.codexExecutable = codexExecutable
        self.codexHome = codexHome
        self.storedExecutable = storedExecutable
        self.hasLoginFile = hasLoginFile
    }

    /// Whether any one of the four sources says codex is on this Mac.
    public var isPresent: Bool {
        fleetHoldsCodex || codexExecutable != nil || codexHome != nil || storedExecutable != nil
    }

    /// No source at all. What every render passes.
    public static let absent = CodexPresence()
}
