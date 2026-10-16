import Foundation

/// The Codex notices for one opening of the persistent popover. The shell decides
/// before showing it; polling records the candidate for the next opening.
///
/// The shell feeds it, not the panel. The panel's view is first drawn by the first
/// opening, after the shell has already decided, so a candidate the view recorded
/// could never reach the opening that drew it: the first opening after launch showed
/// no Codex line at all. The shell advances the quiet clock on every poll, the panel
/// open or not (``observePoll(_:now:)``), and builds both lines from the fleet at the
/// opening itself (``decideAtOpen(fleet:bundledTcrWithoutCLI:now:codexProcesses:)``).
@MainActor
public final class CodexBannerSnapshot: ObservableObject {
    @Published public private(set) var shown: CodexBanner?
    @Published public private(set) var sessionsLine: String?
    private var sessionsCandidate: String?
    public private(set) var hasOpened = false
    private var candidate: CodexBanner?
    /// How long the Codex request count has stood still, off every poll.
    public private(set) var quiet = CodexQuietClock()
    /// Whether this Mac shares its accounts with trusted Macs, as the panel's Peers
    /// reader last saw it: only the refusal line's tail reads it.
    public var peerSharing = false

    public init() {}

    /// Advance the quiet clock off the poll that just landed; `nil` when it loaded no fleet.
    public func observePoll(_ fleet: Fleet?, now: Date = Date()) {
        quiet.advance(fleet, now: now)
    }

    /// Decide this opening's two lines from `fleet`, the read the opening draws.
    /// `codexProcesses` walks the process table, and runs only when the last line's every
    /// other input holds (``CodexBanner/current(fleet:codexProcesses:codexQuietFor:peerSharing:bundledTcrWithoutCLI:)``).
    public func decideAtOpen(
        fleet: Fleet?, bundledTcrWithoutCLI: String?, now: Date = Date(),
        codexProcesses: () -> Int?
    ) {
        if let fleet {
            candidate = CodexBanner.current(
                fleet: fleet, codexProcesses: codexProcesses(),
                codexQuietFor: quiet.quietFor(now: now), peerSharing: peerSharing,
                bundledTcrWithoutCLI: bundledTcrWithoutCLI)
            sessionsCandidate = SessionsCodexCopy.trafficWithoutSessionsLine(fleet)
        } else {
            candidate = nil
            sessionsCandidate = nil
        }
        decideAtOpen()
    }

    /// Record a candidate for the next opening, as the render harness does.
    public func observe(_ candidate: CodexBanner?) {
        self.candidate = candidate
    }

    /// Record a sessions line for the next opening, as the render harness does.
    public func observeSessionsLine(_ line: String?) {
        sessionsCandidate = line
    }

    /// Show the recorded candidates; nothing a poll records reaches the panel until the next call.
    public func decideAtOpen() {
        hasOpened = true
        shown = candidate
        sessionsLine = sessionsCandidate
    }
}
