import Foundation
import XCTest

@testable import TcrBarCore

/// ``FanControlModel``: every panel, row by row; send on release;
/// the re-send once the helper is approved; and Fixed starting at the speed
/// the fans are already held at (Gil, 2026-10-18). No `tcr` runs: the runner
/// is a recorder.
@MainActor
final class FanControlModelTests: XCTestCase {

    /// Every `tcr fan` argument list the model asked for, in order.
    private final class Recorder: @unchecked Sendable {
        private let lock = NSLock()
        private var calls: [[String]] = []
        private var offMain: [Bool] = []

        func record(_ arguments: [String], offMainThread: Bool) {
            lock.lock()
            calls.append(arguments)
            offMain.append(offMainThread)
            lock.unlock()
        }

        var all: [[String]] {
            lock.lock()
            defer { lock.unlock() }
            return calls
        }

        var allOffMain: Bool {
            lock.lock()
            defer { lock.unlock() }
            return offMain.allSatisfy { $0 }
        }
    }

    private var recorder = Recorder()
    private var registration: FanHelperRegistration.Status = .notRegistered

    private func model() -> FanControlModel {
        let recorder = recorder
        return FanControlModel(
            runner: { arguments in
                recorder.record(arguments, offMainThread: !Thread.isMainThread)
                return .done
            },
            readRegistration: { [unowned self] in self.registration },
            registerHelper: {},
            openApprovalPage: {})
    }

    private func fan(
        _ index: Int, _ actual: Double, manual: Bool = false, target: Double? = nil, min: Double = 1_350
    ) -> FanReading {
        FanReading(
            index: index, actualRPM: actual, minRPM: min, maxRPM: 5_777, isManual: manual,
            targetRPM: target)
    }

    private let fixed = FanDesired.fixed(rpm: 5_000)
    private let follow = FanDesired.followCPU(FanCurve(quietBelowC: 60, fullAtC: 90))
    private let allRegistrations: [FanHelperRegistration.Status] = [
        .enabled, .notRegistered, .requiresApproval, .notFound, .unrecognised(rawValue: 9),
    ]

    private func panel(
        _ desired: FanDesired, _ state: FanControlState, _ reason: FanControlReason? = nil,
        _ registration: FanHelperRegistration.Status
    ) -> FanPanel {
        FanControlModel.panel(
            fanControl: FanControl(desired: desired, state: state, reason: reason),
            registration: registration)
    }

    // MARK: - Every panel, row by row

    /// auto | any | off: controls, Auto selected.
    func testAutoOffIsTheControlsWithAutoSelected() {
        for registration in allRegistrations {
            XCTAssertEqual(
                panel(.auto, .off, nil, registration), .controls(selected: .auto, note: nil),
                "\(registration)")
        }
    }

    /// fixed / followCpu | notRegistered | unavailable/helperUnreachable: panel 7.
    /// `notFound` too: what macOS answers before a first register.
    func testNoHelperIsPanelSeven() {
        for registration in [FanHelperRegistration.Status.notRegistered, .notFound] {
            XCTAssertEqual(
                panel(fixed, .unavailable, .helperUnreachable, registration), .install(selected: .fixed))
            XCTAssertEqual(
                panel(follow, .unavailable, .helperUnreachable, registration),
                .install(selected: .followCPU))
        }
    }

    /// fixed / followCpu | requiresApproval | unavailable/helperUnreachable: panel 8.
    func testAwaitingApprovalIsPanelEight() {
        XCTAssertEqual(
            panel(fixed, .unavailable, .helperUnreachable, .requiresApproval), .approval(selected: .fixed))
        XCTAssertEqual(
            panel(follow, .unavailable, .helperUnreachable, .requiresApproval),
            .approval(selected: .followCPU))
    }

    /// fixed / followCpu | enabled | unavailable/helperUnreachable: "Starting the helper...".
    func testApprovedButUnreachableIsStartingTheHelper() {
        XCTAssertEqual(
            panel(fixed, .unavailable, .helperUnreachable, .enabled), .startingHelper(selected: .fixed))
        XCTAssertEqual(
            panel(follow, .unavailable, .helperUnreachable, .enabled),
            .startingHelper(selected: .followCPU))
    }

    /// A registration status macOS added later is shown by its word.
    func testAnUnknownRegistrationIsShownNotGuessed() {
        XCTAssertEqual(
            panel(fixed, .unavailable, .helperUnreachable, .unrecognised(rawValue: 9)),
            .controls(selected: .fixed, note: .unrecognisedRegistration("unrecognised(9)")))
    }

    /// fixed | enabled | holding: panel 9. followCpu | enabled | holding: panel 10.
    func testHoldingIsPanelsNineAndTen() {
        XCTAssertEqual(panel(fixed, .holding, nil, .enabled), .controls(selected: .fixed, note: nil))
        XCTAssertEqual(panel(follow, .holding, nil, .enabled), .controls(selected: .followCPU, note: nil))
    }

    /// any | any | foreign: panel 11, named or "Another app".
    func testForeignIsPanelElevenWhateverWasAsked() {
        for desired in [FanDesired.auto, fixed, follow] {
            for registration in allRegistrations {
                XCTAssertEqual(
                    panel(desired, .foreign, .macsFanControl, registration),
                    .anotherApp(holder: "Macs Fan Control"))
                XCTAssertEqual(
                    panel(desired, .foreign, .anotherApp, registration), .anotherApp(holder: "Another app"))
            }
        }
    }

    /// any | any | unavailable/notSignedForFans: the unsigned-tcr sentence.
    func testAnUnsignedTcrSaysSo() {
        for desired in [FanDesired.auto, fixed, follow] {
            XCTAssertEqual(panel(desired, .unavailable, .notSignedForFans, .enabled), .notSigned)
        }
        XCTAssertEqual(
            FanNote.unavailable(reason: .notSignedForFans, detail: nil).text,
            "This tcr is not the one TcrBar ships; fan control needs it.")
    }

    /// field absent | any: controls off, "Update tcr to control fans here."
    func testNoFieldIsUpdateTcr() {
        for registration in allRegistrations {
            let panel = FanControlModel.panel(fanControl: nil, registration: registration)
            XCTAssertEqual(panel, .needsUpdate)
            XCTAssertFalse(panel.segmentsEnabled)
        }
    }

    /// Between the rows: a state that is not plainly the wish carries a note,
    /// and an unknown state is shown by its word.
    func testOtherStatesCarryANote() {
        XCTAssertEqual(panel(fixed, .entering, nil, .enabled), .controls(selected: .fixed, note: .entering))
        XCTAssertEqual(panel(.auto, .releasing, nil, .enabled), .controls(selected: .auto, note: .releasing))
        XCTAssertEqual(
            panel(fixed, .unknown, nil, .enabled), .controls(selected: .fixed, note: .unknownReading))
        XCTAssertEqual(
            panel(fixed, .unrecognised("wobbling"), nil, .enabled),
            .controls(selected: .fixed, note: .unrecognisedState("wobbling")))
        XCTAssertEqual(
            panel(fixed, .unavailable, .modeLocked, .enabled),
            .controls(selected: .fixed, note: .unavailable(reason: .modeLocked, detail: nil)))
    }

    // MARK: - Send on release

    func testTheSliderSendsOnceOnRelease() async {
        let fans = model()
        fans.update(
            fanControl: FanControl(desired: fixed, state: .holding),
            heat: HeatReading(cpuCelsius: 60, fans: [fan(0, 5_000, manual: true, target: 5_000)]))
        for rpm in [4_900.0, 4_700, 4_500, 4_200, 4_000] {
            fans.sliderMoved(to: rpm)
        }
        await fans.settle()
        XCTAssertEqual(recorder.all, [], "a drag sent before it was let go")
        XCTAssertEqual(fans.fixedRPM, 4_000, "the figure did not follow the drag")
        fans.sliderReleased(at: 4_000)
        await fans.settle()
        XCTAssertEqual(recorder.all, [["fan", "fixed", "4000"]])
        XCTAssertTrue(recorder.allOffMain, "tcr fan ran on the main thread")
    }

    /// A stepper click is a release: one command each, with the new curve.
    func testAStepperClickSendsOnce() async {
        let fans = model()
        fans.update(fanControl: FanControl(desired: follow, state: .holding), heat: nil)
        fans.step(quiet: true, by: 5)
        await fans.settle()
        XCTAssertEqual(recorder.all, [["fan", "follow", "65", "90"]])
        XCTAssertFalse(fans.canStep(quiet: false, by: -25), "a curve the proxy refuses is offered")
        XCTAssertFalse(fans.canStep(quiet: false, by: 15), "full past the 100 °C guard is offered")
    }

    // MARK: - Re-send once the helper is approved

    func testApprovalResendsTheModeOnce() async {
        let fans = model()
        let waiting = FanControl(desired: fixed, state: .unavailable, reason: .helperUnreachable)
        registration = .requiresApproval
        fans.update(fanControl: waiting, heat: nil)
        await fans.settle()
        XCTAssertEqual(fans.panel, .approval(selected: .fixed))
        XCTAssertEqual(recorder.all, [])

        registration = .enabled
        fans.update(fanControl: waiting, heat: nil)
        await fans.settle()
        XCTAssertEqual(recorder.all, [["fan", "fixed", "5000"]])

        // Polls that still say unreachable do not send again.
        fans.update(fanControl: waiting, heat: nil)
        fans.update(fanControl: waiting, heat: nil)
        await fans.settle()
        XCTAssertEqual(recorder.all.count, 1)
    }

    /// No re-send for a state that does not need one.
    func testNoResendWhileHolding() async {
        let fans = model()
        registration = .enabled
        fans.update(fanControl: FanControl(desired: fixed, state: .holding), heat: nil)
        await fans.settle()
        XCTAssertEqual(recorder.all, [])
    }

    // MARK: - Fixed starts at the held speed

    /// Macs Fan Control held both fans at 5,000: Fixed asks for 5,000, not
    /// what the fans happen to read this second.
    func testFixedStartsAtTheHeldTarget() async {
        let fans = model()
        fans.update(
            fanControl: FanControl(desired: .auto, state: .off),
            heat: HeatReading(
                cpuCelsius: 80,
                fans: [
                    fan(0, 5_013, manual: true, target: 5_000), fan(1, 4_976, manual: true, target: 5_000),
                ]
            ))
        fans.choose(.fixed)
        await fans.settle()
        XCTAssertEqual(recorder.all, [["fan", "fixed", "5000"]])
        XCTAssertEqual(fans.fixedRPM, 5_000)
    }

    /// Nobody holds them: the busier fan's actual speed. At rest: the slowest
    /// speed every fan spins at.
    func testFixedStartsAtTheBusierFanWhenNothingHoldsThem() {
        XCTAssertEqual(
            FanControlModel.heldSpeed(
                heat: HeatReading(cpuCelsius: 70, fans: [fan(0, 2_100), fan(1, 2_480)])),
            2_480)
        XCTAssertEqual(
            FanControlModel.heldSpeed(
                heat: HeatReading(cpuCelsius: 46, fans: [fan(0, 0), fan(1, 0, min: 1_400)])), 1_400)
        XCTAssertNil(FanControlModel.heldSpeed(heat: HeatReading(cpuCelsius: 46, fans: [])))
        // A target on a fan in automatic is not a hold.
        XCTAssertEqual(
            FanControlModel.heldSpeed(
                heat: HeatReading(cpuCelsius: 70, fans: [fan(0, 2_100, target: 4_000)])),
            2_100)
    }

    /// No speed to start from is said, not replaced by a made-up one.
    func testFixedWithNoReadingSaysSo() async {
        let fans = model()
        fans.update(fanControl: FanControl(desired: .auto, state: .off), heat: nil)
        fans.choose(.fixed)
        await fans.settle()
        XCTAssertEqual(recorder.all, [])
        XCTAssertNotNil(fans.failure)
    }

    // MARK: - The mode just picked

    /// Picking shows the new segment at once, and the next poll is the truth.
    func testAPickShowsUntilThePollReportsIt() async {
        let fans = model()
        registration = .notRegistered
        fans.update(fanControl: FanControl(desired: .auto, state: .off), heat: nil)
        fans.choose(.followCPU)
        XCTAssertEqual(fans.panel, .controls(selected: .followCPU, note: .sending))
        await fans.settle()
        XCTAssertEqual(recorder.all, [["fan", "follow", "60", "90"]])
        // The proxy stored the wish and cannot reach a helper: panel 7.
        fans.update(
            fanControl: FanControl(desired: follow, state: .unavailable, reason: .helperUnreachable),
            heat: nil)
        XCTAssertEqual(fans.panel, .install(selected: .followCPU))
    }

    /// The segments are off while another app holds the fans: a click sends nothing.
    func testNoCommandWhileAnotherAppHoldsTheFans() async {
        let fans = model()
        fans.update(
            fanControl: FanControl(desired: .auto, state: .foreign, reason: .macsFanControl),
            heat: HeatReading(cpuCelsius: 80, fans: [fan(0, 5_013, manual: true, target: 5_000)]))
        fans.choose(.fixed)
        await fans.settle()
        XCTAssertEqual(recorder.all, [])
    }

    // MARK: - The numbers line's word

    func testTheLineSuffixFollowsTheState() {
        XCTAssertEqual(MachineStats.FanSuffix.of(FanControl(desired: fixed, state: .holding)), .fixed)
        XCTAssertEqual(MachineStats.FanSuffix.of(FanControl(desired: follow, state: .holding)), .none)
        XCTAssertEqual(
            MachineStats.FanSuffix.of(
                FanControl(desired: .auto, state: .foreign, reason: .macsFanControl)), .manual)
        XCTAssertEqual(MachineStats.FanSuffix.of(FanControl(desired: .auto, state: .off)), .fromModeKeys)
        XCTAssertEqual(MachineStats.FanSuffix.of(nil), .fromModeKeys)

        func line(_ control: FanControl?) -> String? {
            MachineStats(
                loadAverage: 1, cores: 14, memoryUsedBytes: 0, memoryTotalBytes: 0, compiles: 0,
                diskFreeBytes: 0,
                heat: HeatReading(
                    cpuCelsius: 93.3,
                    fans: [fan(0, 5_013, manual: true, target: 5_000), fan(1, 4_976, manual: true)]),
                fanControl: control
            ).heatClause
        }
        XCTAssertEqual(line(FanControl(desired: fixed, state: .holding)), "CPU 93°C · fans 87% fixed")
        XCTAssertEqual(line(FanControl(desired: follow, state: .holding)), "CPU 93°C · fans 87%")
        XCTAssertEqual(
            line(FanControl(desired: .auto, state: .foreign, reason: .macsFanControl)),
            "CPU 93°C · fans 87% manual")
        XCTAssertEqual(line(nil), "CPU 93°C · fans 87% manual")
    }
}
