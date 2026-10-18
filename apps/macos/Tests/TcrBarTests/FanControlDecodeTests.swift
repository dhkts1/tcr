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
           "sinceMs": 0
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
                #"{"desired": {"mode": "auto"}, "state": "off", "reason": null, "hotGuard": false, "targets": [], "detail": null, "sinceMs": 0}"#
            )?
            .desired, .auto)
        XCTAssertEqual(
            try decode(
                #"{"desired": {"mode": "fixed", "rpm": 5000}, "state": "holding", "reason": null, "hotGuard": false, "targets": [{"fan": 0, "rpm": 5000.0}, {"fan": 1, "rpm": 5000.0}], "detail": null, "sinceMs": 0}"#
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
            #"{"desired": {"mode": "fixed", "rpm": 5000}, "state": "unavailable", "reason": "helperUnreachable", "hotGuard": false, "targets": [], "detail": null, "sinceMs": 0}"#
        )
        XCTAssertEqual(unreachable?.state, .unavailable)
        XCTAssertEqual(unreachable?.reason, .helperUnreachable)

        let macs = try decode(
            #"{"desired": {"mode": "auto"}, "state": "foreign", "reason": "macsFanControl", "hotGuard": false, "targets": [], "detail": "fans go back to automatic when Macs Fan Control quits", "sinceMs": 0}"#
        )
        XCTAssertEqual(macs?.state, .foreign)
        XCTAssertEqual(macs?.reason, .macsFanControl)
        XCTAssertEqual(macs?.detail, "fans go back to automatic when Macs Fan Control quits")

        XCTAssertEqual(
            try decode(
                #"{"desired": {"mode": "auto"}, "state": "unavailable", "reason": "notSignedForFans"}"#)?
                .reason, .notSignedForFans)
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

    func testCommandsAreTheCliVerbs() {
        XCTAssertEqual(FanCommand.auto.arguments, ["fan", "auto"])
        XCTAssertEqual(FanCommand.fixed(rpm: 5_000).arguments, ["fan", "fixed", "5000"])
        XCTAssertEqual(
            FanCommand.follow(FanCurve(quietBelowC: 60, fullAtC: 90)).arguments,
            ["fan", "follow", "60", "90"])
    }
}
