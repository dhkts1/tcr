import XCTest

@testable import TcrBarCore

/// `fanControl` on the `tcr sessions --json` object, as the proxy writes it
/// (`crates/tcr-status-wire`, beside `heat`), and the per-fan
/// `targetRpm` on `heat`. The JSON here is the contract with the Rust side:
/// change it only with the wire.
final class FanControlDecodeTests: XCTestCase {

    /// The wire's own sample, byte for byte inside the sessions document.
    static let designSample = """
        {"supported": true, "sessions": [],
         "fanControl": {
           "desired": { "mode": "followCpu", "quietBelowC": 60, "fullAtC": 90 },
           "state": "holding",
           "reason": null,
           "hotGuard": false,
           "targets": [ { "fan": 0, "rpm": 5777.0 } ],
           "detail": null,
           "command": 0
         }}
        """

    private func decode(_ fanControl: String, log: @escaping (String) -> Void = { _ in }) throws
        -> FanControl?
    {
        let object = try JSONSerialization.jsonObject(with: Data(fanControl.utf8))
        return Fleet.decodeFanControl(object, log: log)
    }

    func testTheDesignSampleDecodes() throws {
        let read = try Fleet.decodeSessions(Data(Self.designSample.utf8))
        XCTAssertEqual(
            read.fanControl,
            FanControl(
                desired: .followCPU(FanCurve(quietBelowC: 60, fullAtC: 90)), state: .holding,
                reason: nil, hotGuard: false, targets: [FanTarget(fan: 0, rpm: 5_777)], detail: nil))
        let fleet = Fleet(accounts: []).withSessions(read)
        XCTAssertEqual(fleet.fanControl, read.fanControl)
    }

    /// `tcr fan`'s three modes are also the three `desired` shapes.
    func testEveryDesiredModeDecodes() throws {
        XCTAssertEqual(
            try decode(
                #"{"desired": {"mode": "auto"}, "state": "off", "reason": null, "hotGuard": false, "targets": [], "detail": null, "command": 0}"#
            )?
            .desired, .auto)
        XCTAssertEqual(
            try decode(
                #"{"desired": {"mode": "fixed", "rpm": 5000}, "state": "holding", "reason": null, "hotGuard": false, "targets": [{"fan": 0, "rpm": 5000.0}, {"fan": 1, "rpm": 5000.0}], "detail": null, "command": 0}"#
            )?
            .desired, .fixed(rpm: 5_000))
        XCTAssertEqual(
            try decode(
                #"{"desired": {"mode": "followCpu", "quietBelowC": 60.0, "fullAtC": 90.0}, "state": "holding"}"#
            )?
            .desired, .followCPU(FanCurve(quietBelowC: 60, fullAtC: 90)))
    }

    func testReasonsDecodeToTheirCases() throws {
        let unreachable = try decode(
            #"{"desired": {"mode": "fixed", "rpm": 5000}, "state": "unavailable", "reason": "helperUnreachable", "hotGuard": false, "targets": [], "detail": null, "command": 0}"#
        )
        XCTAssertEqual(unreachable?.state, .unavailable)
        XCTAssertEqual(unreachable?.reason, .helperUnreachable)

        let macs = try decode(
            #"{"desired": {"mode": "auto"}, "state": "foreign", "reason": "macsFanControl", "hotGuard": false, "targets": [], "detail": "fans go back to automatic when Macs Fan Control quits", "command": 0}"#
        )
        XCTAssertEqual(macs?.state, .foreign)
        XCTAssertEqual(macs?.reason, .macsFanControl)
        XCTAssertEqual(macs?.detail, "fans go back to automatic when Macs Fan Control quits")

        XCTAssertEqual(
            try decode(
                #"{"desired": {"mode": "auto"}, "state": "unavailable", "reason": "notSignedForFans"}"#)?
                .reason, .notSignedForFans)

        XCTAssertEqual(
            try decode(
                #"{"desired": {"mode": "fixed", "rpm": 5000}, "state": "unavailable", "reason": "controllerStopped"}"#
            )?
            .reason, .controllerStopped)
    }

    /// A value this build does not know is kept by its own word, never mapped
    /// onto a known one.
    func testUnknownValuesAreKeptNotGuessed() throws {
        let control = try decode(
            #"{"desired": {"mode": "turbo"}, "state": "wobbling", "reason": "solarFlare"}"#)
        XCTAssertEqual(control?.desired, .unrecognised("turbo"))
        XCTAssertEqual(control?.state, .unrecognised("wobbling"))
        XCTAssertEqual(control?.reason, .unrecognised("solarFlare"))
    }

    /// serde's externally tagged form for a reason that carries a value.
    func testATaggedReasonDecodesByItsTag() throws {
        let version = try decode(
            #"{"desired": {"mode": "fixed", "rpm": 5000}, "state": "unavailable", "reason": {"helperVersion": {"installed": 2}}}"#
        )
        XCTAssertEqual(version?.reason, .helperVersion)
        let locked = try decode(
            #"{"desired": {"mode": "fixed", "rpm": 5000}, "state": "unavailable", "reason": {"modeLocked": 1}}"#
        )
        XCTAssertEqual(locked?.reason, .modeLocked)
    }

    /// An object missing `state` is left out and logged once, and the session
    /// rows and heat beside it still decode.
    func testABadObjectIsLeftOutAndLoggedOnce() throws {
        var lines: [String] = []
        XCTAssertNil(try decode(#"{"desired": {"mode": "auto"}}"#, log: { lines.append($0) }))
        XCTAssertEqual(lines.count, 1)
        XCTAssertTrue(lines[0].contains("fan control"), lines[0])

        let json = """
            {"supported": true, "sessions": [{"sessionId": "11111111-1111-1111-1111-111111111111"}],
             "heat": {"cpuCelsius": 50, "fans": []},
             "fanControl": {"desired": {"mode": "fixed"}, "state": "holding"}}
            """
        let read = try Fleet.decodeSessions(Data(json.utf8))
        XCTAssertNil(read.fanControl)
        XCTAssertEqual(read.channel, .live)
        XCTAssertEqual(read.sessions.count + read.unreadable.count, 1)
        XCTAssertEqual(read.heat, HeatReading(cpuCelsius: 50, fans: []))
    }

    /// A curve in fractions is refused rather than rounded.
    func testAFractionalCurveIsRefused() throws {
        XCTAssertNil(
            try decode(
                #"{"desired": {"mode": "followCpu", "quietBelowC": 60.5, "fullAtC": 90}, "state": "holding"}"#
            ))
    }

    func testNoKeyIsNoFanControl() throws {
        let read = try Fleet.decodeSessions(Data(#"{"supported": true, "sessions": []}"#.utf8))
        XCTAssertNil(read.fanControl)
    }

    /// Stage 1's per-fan heat gains an optional `targetRpm`.
    func testTargetRpmDecodesOnAFan() throws {
        let json = """
            {"supported": true, "sessions": [],
             "heat": {"cpuCelsius": 93.3, "fans": [
                {"index": 0, "actualRpm": 5013.0, "minRpm": 1350.0, "maxRpm": 5777.0, "manual": true, "targetRpm": 5000.0},
                {"index": 1, "actualRpm": 4976.0, "minRpm": 1350.0, "maxRpm": 5777.0, "manual": true}]}}
            """
        let fans = try Fleet.decodeSessions(Data(json.utf8)).heat?.fans
        XCTAssertEqual(fans?.first?.targetRPM, 5_000)
        XCTAssertNil(fans?.last?.targetRPM)
    }

    // MARK: The committed cross-language fixture

    /// `<repo>/tests/fixtures/fan-control-contract.json`: the SAME file the
    /// Rust side renders and compares against (`src/fan.rs`), located from
    /// `#filePath` as ``RealWorldDecodeTests/contractFixtureURL`` locates
    /// `status-contract.json`.
    static var fanContractURL: URL {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { url = url.deletingLastPathComponent() }
        return url.appendingPathComponent("tests/fixtures/fan-control-contract.json")
    }

    private func fanContract() throws -> [String: Any] {
        let data = try Data(contentsOf: Self.fanContractURL)
        guard let samples = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw XCTSkip("unreachable: the fixture's top level is not an object")
        }
        return samples
    }

    /// Every sample decodes, and nothing in the fixture is left unread: a
    /// sample the Rust side adds and this list does not name fails here.
    func testEverySampleInTheSharedFixtureDecodes() throws {
        let samples = try fanContract()
        let expected: [String: FanControl] = [
            "off": FanControl(desired: .auto, state: .off),
            "holdingFixed5000": FanControl(
                desired: .fixed(rpm: 5_000), state: .holding,
                targets: [FanTarget(fan: 0, rpm: 5_000), FanTarget(fan: 1, rpm: 5_000)]),
            "holdingFollow": FanControl(
                desired: .followCPU(FanCurve(quietBelowC: 60, fullAtC: 90)), state: .holding,
                targets: [FanTarget(fan: 0, rpm: 5_777), FanTarget(fan: 1, rpm: 5_777)]),
            "foreignMacsFanControl": FanControl(
                desired: .fixed(rpm: 5_000), state: .foreign, reason: .macsFanControl),
            "foreignMacsFanControlWaiting": FanControl(
                desired: .auto, state: .foreign, reason: .macsFanControl,
                targets: [FanTarget(fan: 0, rpm: 5_000), FanTarget(fan: 1, rpm: 5_000)],
                detail: "fans go back to automatic when Macs Fan Control quits"),
            "unavailableModeLocked": FanControl(
                desired: .fixed(rpm: 5_000), state: .unavailable, reason: .modeLocked,
                detail: "fan 1 would not leave automatic"),
            "unavailableHelperVersion": FanControl(
                desired: .fixed(rpm: 5_000), state: .unavailable, reason: .helperVersion,
                detail: "the installed helper speaks protocol 2; this tcr speaks 1"),
            "unavailableHelperUnreachable": FanControl(
                desired: .fixed(rpm: 5_000), state: .unavailable, reason: .helperUnreachable),
        ]
        XCTAssertEqual(Set(samples.keys), Set(expected.keys), "the fixture's samples changed")
        for (name, sample) in samples.sorted(by: { $0.key < $1.key }) {
            var lines: [String] = []
            let control = Fleet.decodeFanControl(sample, log: { lines.append($0) })
            XCTAssertEqual(lines, [], "\(name) logged a decode failure")
            XCTAssertEqual(control, expected[name], name)
        }
        // Inside a whole sessions document too, as the poll reads it.
        let document: [String: Any] = [
            "supported": true, "sessions": [], "fanControl": samples["holdingFixed5000"] as Any,
        ]
        let read = try Fleet.decodeSessions(JSONSerialization.data(withJSONObject: document))
        XCTAssertEqual(read.fanControl, expected["holdingFixed5000"])
    }

    /// The controller's reason when the helper's reply was not the contract's
    /// shape at all: `{"helperVersion": {}}`, no `installed`.
    func testAHelperVersionWithNoInstalledDecodes() throws {
        let control = try decode(
            #"{"desired": {"mode": "fixed", "rpm": 5000}, "state": "unavailable", "reason": {"helperVersion": {}}, "hotGuard": false, "targets": [], "detail": null, "command": 0}"#
        )
        XCTAssertEqual(control?.state, .unavailable)
        XCTAssertEqual(control?.reason, .helperVersion)
    }

    /// The heat object as a fan-control proxy writes it: `ftst` beside the
    /// fans, and each fan with `manual` and `targetRpm`. The rows read, and a
    /// key this build does not use (`ftst`, on the heat object or a fan) fails
    /// nothing.
    func testAHeatRowWithFtstAndTargetRpmDecodes() throws {
        let json = """
            {"supported": true, "sessions": [],
             "heat": {"cpuCelsius": 71.5, "ftst": true, "fans": [
                {"index": 0, "actualRpm": 5013.0, "minRpm": 1350.0, "maxRpm": 5777.0, "manual": true, "targetRpm": 5000.0, "ftst": true},
                {"index": 1, "actualRpm": 1400.0, "minRpm": 1350.0, "maxRpm": 5777.0, "manual": false, "targetRpm": 0.0}]}}
            """
        let heat = try Fleet.decodeSessions(Data(json.utf8)).heat
        XCTAssertEqual(
            heat,
            HeatReading(
                cpuCelsius: 71.5,
                fans: [
                    FanReading(
                        index: 0, actualRPM: 5_013, minRPM: 1_350, maxRPM: 5_777, isManual: true,
                        targetRPM: 5_000),
                    FanReading(
                        index: 1, actualRPM: 1_400, minRPM: 1_350, maxRPM: 5_777, isManual: false,
                        targetRPM: 0),
                ]))
    }

    func testCommandsAreTheCliVerbs() {
        XCTAssertEqual(FanCommand.auto.arguments, ["fan", "auto"])
        XCTAssertEqual(FanCommand.fixed(rpm: 5_000).arguments, ["fan", "fixed", "5000"])
        XCTAssertEqual(
            FanCommand.follow(FanCurve(quietBelowC: 60, fullAtC: 90)).arguments,
            ["fan", "follow", "60", "90"])
    }
}
