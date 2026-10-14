import SwiftUI
import TcrBarCore

/// Settings > General: which way a typed `claude` goes through tcr, with the
/// prompt's two choices plus Off, so the answer can be changed or undone.
///
/// Every change is `tcr setup …` run as a subprocess
/// (``ClaudeRouteController/choose(_:)``); this app never edits a startup
/// file or `settings.json` itself. The caption line is always drawn, two
/// lines tall at most, so a status read or a refusal landing changes its
/// words and not the row's height.
struct ClaudeRouteSettingsRow: View {
    @ObservedObject var controller: ClaudeRouteController
    /// `ImageRenderer` draws a `Picker` as the "prohibited" glyph, so the
    /// render harness draws the current choice as plain text instead.
    var snapshotMode = false

    /// The choices offered: the prompt's two plus Off, and Both only when
    /// both are already on, so the row can show it rather than misname it.
    private var options: [ClaudeRoute] {
        controller.route == .both ? [.off, .shell, .everyLaunch, .both] : [.off, .shell, .everyLaunch]
    }

    private var caption: String {
        if let failure = controller.failure { return "Not changed: \(failure)" }
        switch controller.reading {
        case .none:
            return "Reading tcr setup status\u{2026}"
        case .unavailable(let reason)?:
            return "This tcr cannot say: \(reason)"
        case .status(let status)?:
            if let conflict = status.conflicts.first {
                return "In the way: \(conflict.summary())"
            }
            switch status.route {
            case .off:
                return "claude talks to Anthropic directly unless started with tcr run."
            case .shell:
                return "claude typed in a terminal runs tcr run. Apps that start claude are not covered."
            case .everyLaunch, .both:
                return
                    "Every claude launch goes through tcr. While tcr is stopped, claude cannot reach Anthropic."
            }
        }
    }

    private var selection: Binding<ClaudeRoute> {
        Binding(
            get: { controller.route ?? .off },
            set: { target in
                guard target != controller.route else { return }
                if target == .everyLaunch, !ClaudeRouteConfirm.everyLaunch() { return }
                Task { await controller.choose(target) }
            })
    }

    var body: some View {
        LabeledContent {
            if snapshotMode {
                Text((controller.route ?? .off).label)
                    .foregroundStyle(Tok.inkDim)
                    .lineLimit(1)
                    .fixedSize()
            } else {
                Picker("Send claude through tcr", selection: selection) {
                    ForEach(options, id: \.self) { route in
                        Text(route.label).tag(route)
                    }
                }
                .labelsHidden()
                .pickerStyle(.menu)
                .fixedSize()
                .disabled(controller.pending || controller.route == nil)
            }
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text("Send claude through tcr")
                ZStack(alignment: .topLeading) {
                    // Two lines reserved whatever the caption says.
                    Text("\n").font(.caption).hidden()
                    Text(caption)
                        .font(.caption).foregroundStyle(Tok.inkDim)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }
}
