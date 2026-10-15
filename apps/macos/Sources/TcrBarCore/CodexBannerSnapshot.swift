import Foundation

/// The Codex line for one opening of the persistent popover. The shell decides
/// before showing it; polling records the candidate for the next opening.
@MainActor
public final class CodexBannerSnapshot: ObservableObject {
    @Published public private(set) var shown: CodexBanner?
    public private(set) var hasOpened = false
    private var candidate: CodexBanner?

    public init() {}

    public func observe(_ candidate: CodexBanner?) {
        self.candidate = candidate
    }

    public func decideAtOpen() {
        hasOpened = true
        shown = candidate
    }
}
