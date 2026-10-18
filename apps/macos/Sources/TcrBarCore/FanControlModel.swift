import Combine
import Foundation

/// What the fan popover shows: one panel from the
/// proxy's `fanControl`, the helper's registration with macOS, and the mode
/// just picked.
public enum FanPanel: Equatable, Sendable {
    /// The three segments and the chosen mode's controls (panels 9 and 10, and
    /// Auto). `selected` is nil for a mode this build has no segment for.
    /// `note` is a sentence under the controls while the proxy is not simply
    /// doing what was asked.
    case controls(selected: FanModeChoice?, note: FanNote?)
    /// Panel 7: Fixed or Follow CPU asked for, the helper never installed.
    case install(selected: FanModeChoice)
    /// Panel 8: installed, waiting for the operator's OK in Login Items.
    case approval(selected: FanModeChoice)
    /// Approved, and the proxy has not reached the helper yet. TcrBar sends
    /// the mode once more (``FanControlModel``).
    case startingHelper(selected: FanModeChoice)
    /// Panel 11: another program holds the fans, by name when the proxy knows it.
    case anotherApp(holder: String)
    /// The tcr serving this Mac is not the signed one TcrBar ships.
    case notSigned
    /// The poll carried no `fanControl`: a proxy too old to control fans.
    case needsUpdate

    /// The segment lit, or nil when none is (panel 11, an old or unsigned tcr).
    public var selected: FanModeChoice? {
        switch self {
        case .controls(let selected, _): return selected
        case .install(let selected), .approval(let selected), .startingHelper(let selected):
            return selected
        case .anotherApp, .notSigned, .needsUpdate: return nil
        }
    }

    /// Whether the segments take a click.
    public var segmentsEnabled: Bool {
        switch self {
        case .controls, .install, .approval, .startingHelper: return true
        case .anotherApp, .notSigned, .needsUpdate: return false
        }
    }
}

/// The sentence under the controls when the proxy is not simply doing what
/// was asked. Each carries what it names, so a test compares the case and
/// the view reads ``text``.
public enum FanNote: Equatable, Sendable {
    /// The `tcr fan` command is still running.
    case sending
    /// Taking the fans from macOS.
    case entering
    /// A manual mode asked for and the controller not holding yet, nor saying why.
    case notHoldingYet
    case releasing
    /// A fan read failed; nothing is written until one succeeds.
    case unknownReading
    /// Handing back failed.
    case stuck(detail: String?)
    case unavailable(reason: FanControlReason?, detail: String?)
    /// A state this build does not know, by the proxy's word.
    case unrecognisedState(String)
    /// macOS reports a registration status this build does not know.
    case unrecognisedRegistration(String)

    public var text: String {
        switch self {
        case .sending: return "Asking tcr…"
        case .entering: return "Taking the fans from macOS…"
        case .notHoldingYet: return "tcr does not hold the fans yet."
        case .releasing: return "Handing the fans back to macOS…"
        case .unknownReading:
            return "A fan did not read, so nothing is written until it reads again."
        case .stuck(let detail):
            return Self.joined(
                "macOS did not take the fans back. They return at the next tcr start or a restart.",
                detail)
        case .unavailable(let reason, let detail):
            return Self.joined(Self.unavailableSentence(reason), detail)
        case .unrecognisedState(let raw):
            return "tcr reports the fans as “\(raw)”, which this TcrBar does not know."
        case .unrecognisedRegistration(let raw):
            return "macOS reports the fan helper as “\(raw)”, which this TcrBar does not know."
        }
    }

    private static func unavailableSentence(_ reason: FanControlReason?) -> String {
        switch reason {
        case .noSmc: return "This Mac has no fan controller tcr can reach."
        case .noFans: return "This Mac has no fans."
        case .helperUnreachable: return "tcr cannot reach the fan helper."
        case .helperNoReply: return "The fan helper did not answer."
        case .helperSignature: return "The fan helper is not signed as TcrBar's."
        case .helperVersion: return "The fan helper is a different version from this tcr."
        case .modeLocked: return "macOS would not let go of a fan."
        case .contended: return "Something else keeps taking the fans back."
        case .helperRefused: return "The fan helper refused."
        case .notSignedForFans: return "This tcr is not the one TcrBar ships; fan control needs it."
        case .macsFanControl, .anotherApp: return "Another app holds the fans."
        case .unrecognised(let raw): return "Fan control is off: “\(raw)”."
        case nil: return "Fan control is off."
        }
    }

    private static func joined(_ sentence: String, _ detail: String?) -> String {
        guard let detail, !detail.isEmpty else { return sentence }
        return sentence + " " + detail
    }
}

/// The result of one `tcr fan ...` run.
public enum FanCommandResult: Equatable, Sendable {
    case done
    /// `tcr`'s own words, or why it could not run.
    case failed(String)
}

/// The fan popover's state and actions.
///
/// Built from three facts, each from its owner: the proxy's `fanControl` and
/// `heat` from the poll, and the helper's registration from macOS
/// (``FanHelperRegistration/status(label:)``). TcrBar reads no SMC and holds no
/// helper connection: every change is `tcr fan auto|fixed <RPM>|follow <Q> <F>`,
/// run off the main thread. The proxy keeps the fans and the wish.
///
/// Three rules this type exists for:
/// - **Send on release**: dragging the slider moves the figure only;
///   letting go sends one command. Each stepper click is one command.
/// - **Fixed starts at the held speed** (Gil): picking Fixed asks for the speed
///   the fans are already held at, so switching from another app is silent
///   (``heldSpeed(heat:)``).
/// - **Re-send on approval**: when the helper is approved while the proxy
///   still reports it unreachable, the mode is sent once more, so the proxy
///   retries now rather than at its next backoff.
@MainActor
public final class FanControlModel: ObservableObject {
    /// The latest poll's facts.
    @Published public private(set) var fanControl: FanControl?
    @Published public private(set) var heat: HeatReading?
    /// macOS's answer for the helper, read fresh only where it decides the
    /// panel (state `unavailable/helperUnreachable`) and when the popover opens.
    @Published public private(set) var registration: FanHelperRegistration.Status

    /// The mode just picked, shown until a poll reports it as desired.
    @Published public private(set) var picked: FanModeChoice?
    /// The slider's figure: the drag's while dragging, else what was last sent
    /// or what the proxy holds.
    @Published public private(set) var fixedRPM: Double?
    /// The Follow CPU curve the steppers show.
    @Published public private(set) var curve: FanCurve = .starting
    /// The last command's failure, or an action that could not run. Cleared
    /// by the next action.
    @Published public private(set) var failure: String?
    /// A `tcr fan` command is running.
    @Published public private(set) var sending = false

    public typealias Runner = @Sendable ([String]) async -> FanCommandResult

    private let runner: Runner
    private let readRegistration: () -> FanHelperRegistration.Status
    private let registerHelper: () throws -> Void
    private let openApprovalPage: () -> Void

    private var dragging = false
    /// The latest command not yet run: a newer one replaces it, so a burst of
    /// stepper clicks runs at most two commands.
    private var queued: FanCommand?
    private var worker: Task<Void, Never>?
    /// The re-send for ``FanPanel/startingHelper(selected:)`` happened, so it is
    /// not repeated on every poll; cleared when the panel moves on.
    private var resentForStartingHelper = false

    public init(
        runner: @escaping Runner = FanControlModel.runTcr,
        readRegistration: @escaping () -> FanHelperRegistration.Status = {
            FanHelperRegistration.status()
        },
        registerHelper: @escaping () throws -> Void = { try FanHelperRegistration.register() },
        openApprovalPage: @escaping () -> Void = { FanHelperRegistration.openApproval() }
    ) {
        self.runner = runner
        self.readRegistration = readRegistration
        self.registerHelper = registerHelper
        self.openApprovalPage = openApprovalPage
        self.registration = .notRegistered
    }

    /// A model pinned to one set of facts, for the render harness: no command,
    /// no registration read, no registration write.
    public static func pinned(
        fanControl: FanControl?, heat: HeatReading?, registration: FanHelperRegistration.Status
    ) -> FanControlModel {
        let model = FanControlModel(
            runner: { _ in .failed("a pinned model runs nothing") },
            readRegistration: { registration },
            registerHelper: {},
            openApprovalPage: {})
        model.apply(fanControl: fanControl, heat: heat, registration: registration)
        return model
    }

    // MARK: Facts in

    /// One poll's facts. Reads the registration only where it decides the panel.
    public func update(fanControl: FanControl?, heat: HeatReading?) {
        let registration = Self.registrationMatters(fanControl) ? readRegistration() : self.registration
        apply(fanControl: fanControl, heat: heat, registration: registration)
    }

    /// The popover opened: read the registration now, whatever the state, so
    /// an approval given while the panel was closed shows at once.
    public func popoverOpened() {
        apply(fanControl: fanControl, heat: heat, registration: readRegistration())
    }

    private func apply(
        fanControl: FanControl?, heat: HeatReading?, registration: FanHelperRegistration.Status
    ) {
        self.fanControl = fanControl
        self.heat = heat
        self.registration = registration
        if !sending, worker == nil {
            // A finished command's effect is in this poll: the poll is the truth.
            picked = nil
        }
        if !dragging, picked == nil {
            if case .fixed(let rpm)? = fanControl?.desired { fixedRPM = rpm }
        }
        if picked == nil, case .followCPU(let wanted)? = fanControl?.desired { curve = wanted }
        let base = Self.panel(fanControl: fanControl, registration: registration)
        if case .startingHelper = base {
            if !resentForStartingHelper, let desired = fanControl?.desired,
                let command = FanCommand(desired)
            {
                resentForStartingHelper = true
                send(command)
            }
        } else {
            resentForStartingHelper = false
        }
    }

    /// Only `unavailable/helperUnreachable` with a manual mode asked for
    /// depends on the registration.
    static func registrationMatters(_ control: FanControl?) -> Bool {
        guard let control, control.state == .unavailable, control.reason == .helperUnreachable
        else { return false }
        switch control.desired {
        case .fixed, .followCPU: return true
        case .auto, .unrecognised: return false
        }
    }

    // MARK: Panel

    /// Every combination of mode, registration and state, row by row.
    public static func panel(
        fanControl: FanControl?, registration: FanHelperRegistration.Status
    ) -> FanPanel {
        guard let control = fanControl else { return .needsUpdate }
        if control.state == .foreign {
            return .anotherApp(holder: holderName(control.reason))
        }
        if control.state == .unavailable, control.reason == .notSignedForFans {
            return .notSigned
        }
        let selected = control.desired.choice
        if registrationMatters(control), let selected {
            switch registration {
            case .notRegistered, .notFound: return .install(selected: selected)
            case .requiresApproval: return .approval(selected: selected)
            case .enabled: return .startingHelper(selected: selected)
            case .unrecognised:
                return .controls(
                    selected: selected, note: .unrecognisedRegistration(registration.word))
            }
        }
        return .controls(selected: selected, note: note(control))
    }

    static func holderName(_ reason: FanControlReason?) -> String {
        switch reason {
        case .macsFanControl: return "Macs Fan Control"
        case .unrecognised(let raw): return "Another app (\(raw))"
        default: return "Another app"
        }
    }

    /// The note under the controls for a state that is not plainly the wish.
    static func note(_ control: FanControl) -> FanNote? {
        let manual = control.desired.choice.map { $0 != .auto } ?? false
        switch control.state {
        case .off: return manual ? .notHoldingYet : nil
        case .holding: return manual ? nil : .releasing
        case .entering: return .entering
        case .releasing: return .releasing
        case .unknown: return .unknownReading
        case .stuck: return .stuck(detail: control.detail)
        case .unavailable: return .unavailable(reason: control.reason, detail: control.detail)
        case .foreign: return nil
        case .unrecognised(let raw): return .unrecognisedState(raw)
        }
    }

    /// What the popover shows now: the poll's panel, or the mode just picked
    /// while its command runs and no poll has reported it yet.
    public var panel: FanPanel {
        let base = Self.panel(fanControl: fanControl, registration: registration)
        guard let picked, base.segmentsEnabled, base.selected != picked else { return base }
        return .controls(selected: picked, note: .sending)
    }

    // MARK: Actions

    /// A segment clicked. Fixed asks for ``heldSpeed(heat:)``; Follow CPU for
    /// the curve on show.
    public func choose(_ mode: FanModeChoice) {
        guard panel.segmentsEnabled, panel.selected != mode else { return }
        switch mode {
        case .auto:
            send(.auto)
        case .fixed:
            guard let rpm = Self.heldSpeed(heat: heat), let whole = MachineStats.whole(rpm), whole > 0
            else {
                failure = "No fan speed to start from: the last reading carried none."
                return
            }
            fixedRPM = Double(whole)
            send(.fixed(rpm: whole))
        case .followCPU:
            send(.follow(curve))
        }
        picked = mode
    }

    /// The slider moved: the figure follows, nothing is sent.
    public func sliderMoved(to rpm: Double) {
        dragging = true
        fixedRPM = rpm
    }

    /// The slider let go: one command.
    public func sliderReleased(at rpm: Double) {
        dragging = false
        fixedRPM = rpm
        guard let whole = MachineStats.whole(rpm), whole > 0 else {
            failure = "That speed is not a number of rpm."
            return
        }
        send(.fixed(rpm: whole))
        picked = .fixed
    }

    /// A Follow CPU stepper clicked: one command, if the proxy would take the
    /// curve. The view disables a stepper at its limit (``canStep(quiet:by:)``).
    public func step(quiet: Bool, by degrees: Int) {
        guard canStep(quiet: quiet, by: degrees) else { return }
        curve = stepped(quiet: quiet, by: degrees)
        send(.follow(curve))
        picked = .followCPU
    }

    public func canStep(quiet: Bool, by degrees: Int) -> Bool {
        stepped(quiet: quiet, by: degrees).isAccepted
    }

    private func stepped(quiet: Bool, by degrees: Int) -> FanCurve {
        quiet
            ? FanCurve(quietBelowC: curve.quietBelowC + degrees, fullAtC: curve.fullAtC)
            : FanCurve(quietBelowC: curve.quietBelowC, fullAtC: curve.fullAtC + degrees)
    }

    /// Panel 7's button. macOS usually answers "requires approval", which the
    /// next read shows as panel 8.
    public func installHelper() {
        failure = nil
        do {
            try registerHelper()
        } catch {
            failure = "The helper did not install: \(error.localizedDescription)"
        }
        popoverOpened()
    }

    /// Panel 8's button.
    public func openApproval() {
        openApprovalPage()
    }

    // MARK: Fixed starts at the held speed

    /// The speed Fixed starts at (Gil, 2026-10-18): the target of a fan a
    /// program already holds, the highest when they differ, so switching from
    /// Macs Fan Control changes no noise. Otherwise the busier fan's actual
    /// speed; for fans at rest, the highest fan minimum, the slowest speed every
    /// fan will spin at. Nil when the reading has none of these.
    public static func heldSpeed(heat: HeatReading?) -> Double? {
        let fans = heat?.fans ?? []
        let held = fans.filter(\.isManual).compactMap(\.targetRPM).filter { $0.isFinite && $0 > 0 }
        if let target = held.max() { return target }
        if let actual = fans.map(\.actualRPM).filter({ $0.isFinite && $0 > 0 }).max() {
            return actual
        }
        return fans.map(\.minRPM).filter { $0.isFinite && $0 > 0 }.max()
    }

    /// The slider's range: the lowest fan minimum to the highest maximum. Nil
    /// when they did not read, and the popover then shows the speed without a
    /// slider rather than a range nobody measured.
    public var sliderRange: ClosedRange<Double>? {
        let fans = heat?.fans ?? []
        guard let low = fans.map(\.minRPM).filter({ $0.isFinite && $0 > 0 }).min(),
            let high = fans.map(\.maxRPM).filter({ $0.isFinite && $0 > 0 }).max(), low < high
        else { return nil }
        return low...high
    }

    // MARK: Running `tcr fan`

    private func send(_ command: FanCommand) {
        failure = nil
        queued = command
        guard worker == nil else { return }
        sending = true
        worker = Task { [weak self] in await self?.drain() }
    }

    private func drain() async {
        while let next = queued {
            queued = nil
            if case .failed(let message) = await runner(next.arguments) {
                failure = message
            }
        }
        sending = false
        worker = nil
    }

    /// Wait for the commands in flight. For tests.
    public func settle() async {
        while let worker {
            await worker.value
        }
    }

    /// `tcr fan ...` through ``TcrTool``, on a detached task: the command polls
    /// the proxy for up to 20 s, and the main thread never waits on it.
    public nonisolated static let runTcr: Runner = { arguments in
        await Task.detached(priority: .userInitiated) {
            switch TcrTool.resolve() {
            case .failure(let notFound):
                return .failed("tcr not found (searched \(notFound.searched.count) locations)")
            case .success(let executable):
                do {
                    let output = try TcrTool.run(executable: executable, arguments: arguments)
                    guard output.exitCode == 0 else {
                        let words = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                        return .failed(words.isEmpty ? "tcr fan exited \(output.exitCode)" : words)
                    }
                    return .done
                } catch {
                    return .failed("tcr fan did not run: \(error.localizedDescription)")
                }
            }
        }.value
    }
}
