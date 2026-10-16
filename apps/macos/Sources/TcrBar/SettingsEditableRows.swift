import SwiftUI
import TcrBarCore

/// The fixed sizes of the editable Rotation and Limits rows
/// (`docs/design/settings-editable-mockup.html`).
///
/// # Why the control column is 180 pt, not the mockup's 230
///
/// The mockup draws its 230 pt column in an 880 pt window, which leaves about
/// 350 pt for each row's title and description. This window opens at 660 pt
/// (``SettingsWindowController/shippedContentSize``), so a row is about 400 pt
/// wide, and 230 pt of controls would leave the descriptions about 150 pt and
/// wrap them to four lines. 180 pt still holds the widest control (the
/// throttle pair: "bursts of" plus a field plus a unit) and keeps the column
/// fixed, which is the mockup's rule: nothing moves when a value changes.
enum SettingsControlMetrics {
    static let columnWidth: CGFloat = 180
    static let fieldWidth: CGFloat = 52
    static let pairFieldWidth: CGFloat = 60
    static let unitWidth: CGFloat = 22
    static let menuWidth: CGFloat = 160
    /// One control's height, and the throttle pair's two lines. Fixed so a row
    /// keeps its height whatever its control holds, a dash included.
    static let controlHeight: CGFloat = 24
    static let pairHeight: CGFloat = 54
}

/// One row's label: the title with the amber waiting dot after it, and under
/// it the description, or in its place the sentence `tcr` refused the last
/// value with.
///
/// The dot is always laid out and only shown when the row waits, and the
/// caption reserves `lines` lines whether it holds the description or a
/// refusal, so neither a save nor a refusal moves the row.
struct SettingRowLabel: View {
    let title: String
    let detail: String?
    var refusal: String?
    var waiting = false
    var lines = 2
    var scope: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 5) {
                Text(title)
                Circle()
                    .fill(Tok.near)
                    .frame(width: 6, height: 6)
                    .opacity(waiting ? 1 : 0)
                    .accessibilityHidden(!waiting)
                    .accessibilityLabel("Saved, waiting for a proxy restart")
            }
            if let caption = refusal ?? detail {
                Text(caption)
                    .font(.caption)
                    .foregroundStyle(refusal == nil ? Tok.inkDim : Tok.spent)
                    .lineLimit(lines, reservesSpace: true)
                    .help(refusal ?? "")
            }
            if let scope {
                Text(scope).font(.caption).foregroundStyle(Tok.inkDim)
            }
        }
    }
}

/// A number field saved when it loses focus or on Return, with an optional
/// stepper whose every click saves at once.
///
/// It holds its own text while the operator types; `saved` puts it back
/// whenever the saved value changes, and a refused value puts it back to
/// `saved` too, since `commit` answers whether `tcr` took it.
struct SettingNumberField: View {
    let label: String
    let saved: String
    var placeholder = ""
    var width = SettingsControlMetrics.fieldWidth
    var step: Double?
    var bounds: ClosedRange<Double>?
    var unit: String?
    let commit: (String) async -> Bool

    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        HStack(spacing: 4) {
            TextField(label, text: $text, prompt: Text(placeholder))
                .labelsHidden()
                .textFieldStyle(.roundedBorder)
                .multilineTextAlignment(.trailing)
                .monospacedDigit()
                .frame(width: width)
                .focused($focused)
                .onSubmit { submit() }
            if let step {
                Stepper(label, onIncrement: { bump(step) }, onDecrement: { bump(-step) })
                    .labelsHidden()
            }
            if let unit {
                Text(unit)
                    .foregroundStyle(Tok.inkDim)
                    .frame(width: SettingsControlMetrics.unitWidth, alignment: .leading)
            }
        }
        .onAppear { text = saved }
        .onChange(of: saved) { newValue in text = newValue }
        .onChange(of: focused) { isFocused in
            if !isFocused { submit() }
        }
    }

    private func bump(_ delta: Double) {
        let base = Double(text) ?? Double(saved) ?? Double(placeholder) ?? 0
        var next = base + delta
        if let bounds { next = min(max(next, bounds.lowerBound), bounds.upperBound) }
        text = next.rounded() == next ? String(Int(next)) : String(format: "%.1f", next)
        submit()
    }

    private func submit() {
        let value = text.trimmingCharacters(in: .whitespaces)
        guard value != saved else { return }
        let fallback = saved
        Task {
            if !(await commit(value)) { text = fallback }
        }
    }
}

/// The two numbers of one throttle bucket, "one per … ms" over "bursts of …",
/// saved together when either field loses focus or on Return. Both empty is
/// the bucket turned off.
struct SettingThrottlePair: View {
    let label: String
    let saved: ConfigThrottle?
    let commit: ([String]) async -> Bool

    @State private var spacing = ""
    @State private var burst = ""
    @FocusState private var focus: Field?

    private enum Field { case spacing, burst }

    private var savedSpacing: String { saved.map { String($0.minSpacingMs) } ?? "" }
    private var savedBurst: String { saved.map { String($0.burst) } ?? "" }

    var body: some View {
        Grid(alignment: .trailing, horizontalSpacing: 6, verticalSpacing: 4) {
            GridRow {
                Text("one per").font(.caption).foregroundStyle(Tok.inkDim)
                field("\(label), milliseconds between sends", text: $spacing, .spacing)
                Text("ms").foregroundStyle(Tok.inkDim)
                    .frame(width: SettingsControlMetrics.unitWidth, alignment: .leading)
            }
            GridRow {
                Text("bursts of").font(.caption).foregroundStyle(Tok.inkDim)
                field("\(label), burst", text: $burst, .burst)
                Color.clear.frame(width: SettingsControlMetrics.unitWidth, height: 1)
            }
        }
        .onAppear(perform: reset)
        .onChange(of: saved) { _ in reset() }
        .onChange(of: focus) { now in
            if now == nil { submit() }
        }
    }

    private func field(_ label: String, text: Binding<String>, _ which: Field) -> some View {
        TextField(label, text: text, prompt: Text("off"))
            .labelsHidden()
            .textFieldStyle(.roundedBorder)
            .multilineTextAlignment(.trailing)
            .monospacedDigit()
            .frame(width: SettingsControlMetrics.pairFieldWidth)
            .focused($focus, equals: which)
            .onSubmit { submit() }
    }

    private func reset() {
        spacing = savedSpacing
        burst = savedBurst
    }

    private func submit() {
        let s = spacing.trimmingCharacters(in: .whitespaces)
        let b = burst.trimmingCharacters(in: .whitespaces)
        guard s != savedSpacing || b != savedBurst else { return }
        let values = s.isEmpty && b.isEmpty ? ["off"] : [s, b]
        Task {
            if !(await commit(values)) { reset() }
        }
    }
}
