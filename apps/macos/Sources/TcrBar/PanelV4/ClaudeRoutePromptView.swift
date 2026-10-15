import AppKit
import SwiftUI
import TcrBarCore

/// The Accounts tab's one-time question, "Send `claude` through tcr?"
/// (``ClaudeRoutePrompt``).
///
/// The question and the outcome line that replaces it share one `ZStack`, so
/// the row's height is the question's whether or not it has been answered:
/// answering swaps what is drawn and moves nothing else on the panel (Gil,
/// 2026-10-14: no layout shifts from data changes). The hidden half stays in
/// the layout at zero opacity and out of hit testing and accessibility.
struct ClaudeRoutePromptView: View {
    let outcome: String?
    let pending: Bool
    let onAnswer: (ClaudeRoutePrompt.Answer) -> Void

    private var answered: Bool { outcome != nil }

    var body: some View {
        ZStack(alignment: .topLeading) {
            question
                .opacity(answered ? 0 : 1)
                .allowsHitTesting(!answered && !pending)
                .accessibilityHidden(answered)
            Text(outcome ?? " ")
                .font(V4.font(V4.muteSize))
                .foregroundStyle(Tok.inkDim)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
                .opacity(answered ? 1 : 0)
                .accessibilityHidden(!answered)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var question: some View {
        VStack(alignment: .leading, spacing: V4.sectionHeadGap) {
            Text("Send `claude` through tcr?")
                .font(V4.font(V4.muteSize, .semibold))
                .foregroundStyle(Tok.ink)
            HStack(spacing: V4.buttonGap) {
                V4Button(
                    title: "In my shell",
                    help: "Add a claude function to your shell's startup file (tcr setup shell)"
                ) { onAnswer(.shell) }
                V4Button(
                    title: "Every launch\u{2026}",
                    help: "Route every claude launch, from any app (tcr setup claude-settings)"
                ) { onAnswer(.everyLaunch) }
                V4Button(title: "Not now", help: "Never ask again; Settings > General can turn it on") {
                    onAnswer(.notNow)
                }
            }
        }
    }
}

/// The confirm behind "Every launch…", from the prompt and from Settings: the
/// one place the route's cost is put to the person choosing it.
enum ClaudeRouteConfirm {
    @MainActor
    static func everyLaunch() -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Send every claude launch through tcr?"
        alert.informativeText = ClaudeRoutePrompt.everyLaunchCost
        alert.addButton(withTitle: "Every Launch")
        alert.addButton(withTitle: "Cancel")
        return alert.runModal() == .alertFirstButtonReturn
    }
}
