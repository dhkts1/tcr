import SwiftUI
import TcrBarCore

/// The fan control popover, opened from the heat clause on the Tools tab's
/// numbers line: `docs/design/fan-readout-mockup.html` panels 7 to 11.
///
/// It draws ``FanControlModel/panel`` and nothing else decides what shows. The
/// proxy holds the fans; every control here becomes one `tcr fan ...` command.
///
/// Every control is drawn in SwiftUI rather than an AppKit-backed `Picker` or
/// `Slider`, because `ImageRenderer` draws neither, and the render harness must
/// show this view as it is (`RenderStates`, the `17h` to `17l` popover scenes).
struct FanPopover: View {
    @ObservedObject var fans: FanControlModel

    /// The mockup's `.pop{width:310px}`.
    static let width: CGFloat = 310
    /// The link the heat clause carries; ``FleetView`` opens this popover on it.
    static let linkURL = URL(string: "tcrbar-fans://open")

    private var heat: HeatReading? { fans.heat }
    private var panel: FanPanel { fans.panel }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            segments
            content
            if let note = shownNote {
                Text(note)
                    .font(V4.font(V4.muteSize))
                    .foregroundColor(Tok.dim)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 6)
            }
            if let failure = shownFailure {
                Text(failure)
                    .font(V4.font(V4.muteSize))
                    .foregroundColor(Tok.spent)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 6)
            }
            Divider().padding(.top, 10)
            Text(footer)
                .font(V4.font(V4.muteSize))
                .foregroundColor(Tok.mute)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, 8)
        }
        .padding(12)
        .frame(width: Self.width, alignment: .leading)
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            Text("Fans").font(V4.font(13.5, .semibold)).foregroundColor(Tok.ink)
            Spacer()
            if let celsius = heat?.cpuCelsius, let degrees = MachineStats.whole(celsius) {
                Text("CPU \(degrees)°C").font(V4.font(V4.muteSize, .medium))
                    .foregroundColor(Tok.dim)
            }
        }
        .padding(.bottom, 8)
    }

    /// Three segments, the mockup's `.segc`. Off (dimmed, not tappable) while
    /// another app holds the fans or this tcr cannot control them.
    private var segments: some View {
        let enabled = panel.segmentsEnabled
        return HStack(spacing: 2) {
            ForEach(FanModeChoice.allCases, id: \.self) { mode in
                let on = panel.selected == mode
                Button {
                    fans.choose(mode)
                } label: {
                    Text(mode.label)
                        .font(V4.font(V4.muteSize, .semibold))
                        .foregroundColor(on ? Tok.ink : Tok.dim)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 4)
                        .background(
                            RoundedRectangle(cornerRadius: 6).fill(on ? Tok.ink.opacity(0.15) : .clear))
                }
                .buttonStyle(.plain)
                .disabled(!enabled)
                .accessibilityAddTraits(on ? .isSelected : [])
            }
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 8).fill(Tok.ink.opacity(0.06)))
        .opacity(enabled ? 1 : 0.4)
        .padding(.bottom, 10)
    }

    @ViewBuilder private var content: some View {
        switch panel {
        case .controls(let selected, _):
            switch selected {
            case .fixed: fixed
            case .followCPU: followCPU
            case .auto, nil: fanRows(ofMax: true)
            }
        case .install:
            message(
                "Fixed speeds need a small helper that runs as root. It accepts fan speeds and nothing else, and it puts the fans back on automatic when the tcr server stops or crashes.",
                button: "Install helper…", action: fans.installHelper)
        case .approval:
            message(
                "macOS needs your OK once: System Settings › General › Login Items, under “Allow in the Background”, turn on TcrBar.",
                button: "Open Login Items", action: fans.openApproval)
        case .startingHelper:
            message("Starting the helper…", button: nil, action: {})
        case .anotherApp(let holder):
            VStack(alignment: .leading, spacing: 6) {
                (Text("\(holder) is holding the fans.").fontWeight(.semibold).foregroundColor(Tok.ink)
                    + Text(" Quit it to control them here.").foregroundColor(Tok.dim))
                    .font(V4.font(12.5))
                    .fixedSize(horizontal: false, vertical: true)
                fanRows(ofMax: true)
            }
        case .notSigned:
            message(
                "This tcr is not the one TcrBar ships; fan control needs it.", button: nil, action: {})
        case .needsUpdate:
            message("Update tcr to control fans here.", button: nil, action: {})
        }
    }

    /// The note only under the controls: the other panels already say what is
    /// going on in their own sentence.
    private var shownNote: String? {
        guard case .controls(_, let note?) = panel else { return nil }
        return note.text
    }

    /// A failed command's words, except where the panel itself is the answer:
    /// with no helper, `tcr fan fixed` fails with the reason panel 7 explains.
    private var shownFailure: String? {
        switch panel {
        case .controls, .install, .approval: return fans.failure
        case .startingHelper, .anotherApp, .notSigned, .needsUpdate: return nil
        }
    }

    private func message(_ text: String, button: String?, action: @escaping () -> Void) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(text)
                .font(V4.font(12.5))
                .foregroundColor(Tok.dim)
                .fixedSize(horizontal: false, vertical: true)
            if let button {
                Button(action: action) {
                    Text(button)
                        .font(V4.font(12.5, .semibold))
                        .foregroundColor(Tok.ink)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 4)
                        .background(RoundedRectangle(cornerRadius: 6).fill(Tok.ink.opacity(0.13)))
                }
                .buttonStyle(.plain)
            }
        }
    }

    // MARK: Panel 9, Fixed

    private var fixed: some View {
        VStack(alignment: .leading, spacing: 0) {
            keyValue("Speed", speedText, bold: true)
            if let range = fans.sliderRange, let rpm = fans.fixedRPM {
                RPMSlider(
                    value: min(max(rpm, range.lowerBound), range.upperBound), range: range,
                    onMove: fans.sliderMoved(to:), onRelease: fans.sliderReleased(at:)
                )
                .padding(.vertical, 8)
            }
            fanRows(ofMax: false)
        }
    }

    /// "5,000 rpm · 87% of max": the share of the busier fan's maximum, the
    /// figure the numbers line's `fans 87%` is.
    private var speedText: String {
        guard let rpm = fans.fixedRPM, let figure = MachineStats.grouped(rpm) else { return "–" }
        let maximum = (heat?.fans ?? []).map(\.maxRPM).filter { $0.isFinite && $0 > 0 }.max()
        guard let maximum, let percent = MachineStats.whole(rpm / maximum * 100) else {
            return "\(figure) rpm"
        }
        return "\(figure) rpm · \(percent)% of max"
    }

    // MARK: Panel 10, Follow CPU

    private var followCPU: some View {
        let curve = fans.curve
        let celsius = heat?.cpuCelsius
        return VStack(alignment: .leading, spacing: 2) {
            temperatureRow("Quiet below", curve.quietBelowC, quiet: true)
            temperatureRow("Full speed at", curve.fullAtC, quiet: false)
            CurveView(curve: curve, celsius: celsius).padding(.vertical, 4)
            if let now = nowText(curve: curve, celsius: celsius) {
                keyValue("Now", now, bold: true)
            }
        }
    }

    /// "93°C → full, 5,777 rpm": where the CPU sits on the line and the speed
    /// the proxy last wrote. Nil without a CPU reading or a written target.
    private func nowText(curve: FanCurve, celsius: Double?) -> String? {
        guard let celsius, let degrees = MachineStats.whole(celsius),
            let share = curve.share(atCelsius: celsius),
            let target = fans.fanControl?.targets.map(\.rpm).filter({ $0.isFinite }).max(),
            let rpm = MachineStats.grouped(target)
        else { return nil }
        let hot = fans.fanControl?.hotGuard == true
        let word: String
        if hot || share >= 1 {
            word = "full"
        } else if share <= 0 {
            word = "minimum"
        } else {
            word = MachineStats.whole(share * 100).map { "\($0)%" } ?? "–"
        }
        return "\(degrees)°C → \(word), \(rpm) rpm"
    }

    private func temperatureRow(_ label: String, _ value: Int, quiet: Bool) -> some View {
        HStack {
            Text(label).foregroundColor(Tok.dim)
            Spacer()
            stepButton("−", enabled: fans.canStep(quiet: quiet, by: -5)) {
                fans.step(quiet: quiet, by: -5)
            }
            Text("\(value)°C")
                .font(V4.font(12.5, .semibold))
                .foregroundColor(Tok.ink)
                .monospacedDigit()
                .frame(minWidth: 40)
                .padding(.vertical, 1)
                .background(
                    RoundedRectangle(cornerRadius: 5).fill(Tok.ink.opacity(0.08))
                        .overlay(RoundedRectangle(cornerRadius: 5).stroke(Tok.ink.opacity(0.09))))
            stepButton("+", enabled: fans.canStep(quiet: quiet, by: 5)) {
                fans.step(quiet: quiet, by: 5)
            }
        }
        .font(V4.font(12.5))
    }

    private func stepButton(_ glyph: String, enabled: Bool, _ action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(glyph).font(V4.font(12.5, .semibold)).foregroundColor(Tok.dim).frame(width: 16)
        }
        .buttonStyle(.plain)
        .disabled(!enabled)
        .opacity(enabled ? 1 : 0.35)
    }

    // MARK: Shared parts

    /// One row per fan: `5,013 rpm`, or `5,013 of 5,777 rpm`, or `off`.
    private func fanRows(ofMax: Bool) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(heat?.fans ?? [], id: \.index) { fan in
                keyValue("Fan \(fan.index + 1)", rpmText(fan, ofMax: ofMax), bold: false)
            }
        }
    }

    private func rpmText(_ fan: FanReading, ofMax: Bool) -> String {
        guard let actual = MachineStats.grouped(fan.actualRPM) else { return "–" }
        if fan.actualRPM <= 0 { return "off" }
        guard ofMax, fan.maxRPM > 0, let maximum = MachineStats.grouped(fan.maxRPM) else {
            return "\(actual) rpm"
        }
        return "\(actual) of \(maximum) rpm"
    }

    private func keyValue(_ key: String, _ value: String, bold: Bool) -> some View {
        HStack {
            Text(key).foregroundColor(Tok.dim)
            Spacer()
            Text(value).fontWeight(bold ? .semibold : .regular).foregroundColor(bold ? Tok.ink : Tok.dim)
        }
        .font(V4.font(12.5))
        .padding(.vertical, 1)
    }

    /// The mockup's `.pfoot`, except that the mockup says "when TcrBar quits":
    /// the proxy holds the fans, so they go back when the tcr server stops.
    private var footer: String {
        let back = "Back to automatic when the tcr server stops, crashes or updates."
        switch panel {
        case .anotherApp:
            return "TcrBar did not set these speeds and will not change them."
        case .controls(.fixed, _) where fans.fanControl?.state == .holding:
            return back + " At 100°C the fans go to full speed, whatever is set."
        case .controls(.followCPU, _) where fans.fanControl?.state == .holding:
            return back
        default:
            let figures = (heat?.fans ?? []).map { "Fan \($0.index + 1) \(rpmText($0, ofMax: false))" }
            let now = fans.fanControl?.state == .holding ? "Now held by tcr" : "Now automatic"
            return ([now] + figures).joined(separator: " · ")
        }
    }
}

/// A horizontal track with a knob, the mockup's `.slider`, in SwiftUI so the
/// render harness can draw it. Dragging reports each new rpm, rounded to 50,
/// to `onMove`; letting go reports the last one to `onRelease`, which is the
/// only one that becomes a command.
private struct RPMSlider: View {
    let value: Double
    let range: ClosedRange<Double>
    let onMove: (Double) -> Void
    let onRelease: (Double) -> Void

    /// The rpm under `x` on a track `width` wide, rounded to 50.
    private func rpm(at x: CGFloat, width: CGFloat) -> Double {
        let share = Double(min(max(x / max(width, 1), 0), 1))
        let span = range.upperBound - range.lowerBound
        return ((range.lowerBound + share * span) / 50).rounded() * 50
    }

    var body: some View {
        GeometryReader { geometry in
            let width = geometry.size.width
            let span = range.upperBound - range.lowerBound
            let fraction = span > 0 ? (value - range.lowerBound) / span : 0
            ZStack(alignment: .leading) {
                Capsule().fill(Tok.ink.opacity(0.14)).frame(height: 4)
                Capsule().fill(Tok.dim).frame(width: width * fraction, height: 4)
                Circle().fill(Tok.ink).frame(width: 16, height: 16)
                    .shadow(color: .black.opacity(0.5), radius: 1.5, y: 1)
                    .offset(x: width * fraction - 8)
            }
            .frame(height: 16)
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { drag in onMove(rpm(at: drag.location.x, width: width)) }
                    .onEnded { drag in onRelease(rpm(at: drag.location.x, width: width)) })
        }
        .frame(height: 16)
        .accessibilityElement()
        .accessibilityLabel("Fan speed")
        .accessibilityValue("\(MachineStats.grouped(value) ?? "–") rpm")
        .accessibilityAdjustableAction { direction in
            let step = direction == .increment ? 100.0 : -100.0
            onRelease(min(max(value + step, range.lowerBound), range.upperBound))
        }
    }
}

/// Follow CPU's line, the mockup's `.curve` SVG: minimum, a slope between the
/// two temperatures, full; and a dot at the CPU now.
private struct CurveView: View {
    let curve: FanCurve
    let celsius: Double?

    /// Wide enough that both temperatures always sit inside it.
    private var axis: ClosedRange<Double> {
        let low = min(Double(curve.quietBelowC) - 25, 35)
        let high = max(Double(curve.fullAtC) + 15, 105)
        return low...high
    }

    var body: some View {
        Canvas { context, size in
            let left: CGFloat = 4
            let right = size.width - 4
            let top: CGFloat = 8
            let bottom: CGFloat = 57
            let baseline: CGFloat = 72
            func x(_ c: Double) -> CGFloat {
                left + (right - left) * CGFloat((c - axis.lowerBound) / (axis.upperBound - axis.lowerBound))
            }
            func y(_ share: Double) -> CGFloat { bottom - (bottom - top) * CGFloat(share) }
            var axes = Path()
            axes.move(to: CGPoint(x: left, y: baseline))
            axes.addLine(to: CGPoint(x: right, y: baseline))
            axes.move(to: CGPoint(x: left, y: 6))
            axes.addLine(to: CGPoint(x: left, y: baseline))
            context.stroke(axes, with: .color(Tok.ink.opacity(0.18)), lineWidth: 1)
            var line = Path()
            line.move(to: CGPoint(x: left, y: y(0)))
            line.addLine(to: CGPoint(x: x(Double(curve.quietBelowC)), y: y(0)))
            line.addLine(to: CGPoint(x: x(Double(curve.fullAtC)), y: y(1)))
            line.addLine(to: CGPoint(x: right, y: y(1)))
            context.stroke(line, with: .color(Tok.dim), lineWidth: 2)
            if let celsius, let share = curve.share(atCelsius: celsius) {
                let clamped = min(max(celsius, axis.lowerBound), axis.upperBound)
                let dot = CGRect(x: x(clamped) - 4, y: y(share) - 4, width: 8, height: 8)
                context.fill(Path(ellipseIn: dot), with: .color(Tok.ink))
            }
            let font = V4.font(11)
            context.draw(
                Text("\(curve.quietBelowC)°C").font(font).foregroundColor(Tok.mute),
                at: CGPoint(x: x(Double(curve.quietBelowC)), y: 79))
            context.draw(
                Text("\(curve.fullAtC)°C").font(font).foregroundColor(Tok.mute),
                at: CGPoint(x: x(Double(curve.fullAtC)), y: 79))
            context.draw(
                Text("min").font(font).foregroundColor(Tok.mute), at: CGPoint(x: 10, y: bottom - 10),
                anchor: .leading)
            context.draw(
                Text("full").font(font).foregroundColor(Tok.mute), at: CGPoint(x: 10, y: top + 7),
                anchor: .leading)
        }
        .frame(height: 86)
        .accessibilityElement()
        .accessibilityLabel(
            "Fan speed against CPU temperature: minimum up to \(curve.quietBelowC) degrees, full at \(curve.fullAtC)"
        )
    }
}
