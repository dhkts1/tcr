import ServiceManagement
import XCTest

@testable import TcrBarCore

/// The fan helper's registration: the status mapping, which label a bundle
/// gets, and the `--fan-helper` command line. Nothing here registers anything;
/// the one live read is `status`, which is read-only.
final class FanHelperRegistrationTests: XCTestCase {

    func testEveryKnownStatusMapsToItsOwnCase() {
        XCTAssertEqual(FanHelperRegistration.classify(.enabled), .enabled)
        XCTAssertEqual(FanHelperRegistration.classify(.notRegistered), .notRegistered)
        XCTAssertEqual(FanHelperRegistration.classify(.requiresApproval), .requiresApproval)
        XCTAssertEqual(FanHelperRegistration.classify(.notFound), .notFound)
    }

    func testAnUnknownStatusIsSurfacedNotGuessed() {
        guard let future = SMAppService.Status(rawValue: 99) else {
            return XCTFail("SMAppService.Status rejected a raw value; the @unknown arm is untestable")
        }
        XCTAssertEqual(FanHelperRegistration.classify(future), .unrecognised(rawValue: 99))
        XCTAssertEqual(FanHelperRegistration.Status.unrecognised(rawValue: 99).word, "unrecognised(99)")
    }

    /// The label is whatever the bundle's Info.plist names, never derived
    /// from the bundle id: an Apple Development build of the shipping bundle
    /// id ships the `.dev` helper, and reads `.dev` here.
    func testTheLabelIsWhatInfoPlistNames() {
        let key = FanHelperRegistration.infoPlistKey
        XCTAssertEqual(
            FanHelperRegistration.label(fromInfo: [
                "CFBundleIdentifier": "io.github.dhkts1.tcrbar",
                key: "io.github.dhkts1.tcrbar.fanhelper.dev",
            ]),
            "io.github.dhkts1.tcrbar.fanhelper.dev")
        XCTAssertEqual(
            FanHelperRegistration.label(fromInfo: [key: "io.github.dhkts1.tcrbar.fanhelper"]),
            "io.github.dhkts1.tcrbar.fanhelper")
        XCTAssertEqual(
            FanHelperRegistration.plistName(forLabel: FanHelperRegistration.devLabel),
            "io.github.dhkts1.tcrbar.fanhelper.dev.plist")
    }

    /// No key, or a value that is not one of the two labels: no label, and
    /// `status` says so without asking macOS. A dev bundle id alone does not
    /// produce a label.
    func testNoKeyIsItsOwnStatusNotAGuess() {
        let key = FanHelperRegistration.infoPlistKey
        XCTAssertNil(FanHelperRegistration.label(fromInfo: nil))
        XCTAssertNil(
            FanHelperRegistration.label(fromInfo: ["CFBundleIdentifier": "io.github.dhkts1.tcrbar.dev"]))
        XCTAssertNil(FanHelperRegistration.label(fromInfo: [key: ""]))
        XCTAssertNil(FanHelperRegistration.label(fromInfo: [key: "io.github.dhkts1.tcrbar.fanhelper.beta"]))
        XCTAssertNil(FanHelperRegistration.label(fromInfo: [key: 1]))
        XCTAssertEqual(FanHelperRegistration.status(label: nil), .noLabel)
        XCTAssertEqual(FanHelperRegistration.Status.noLabel.word, "noLabel")
        XCTAssertThrowsError(try FanHelperRegistration.register(label: nil))
        XCTAssertThrowsError(try FanHelperRegistration.unregister(label: nil))
    }

    /// The test runner is not a bundle the build script wrote, so it reads as
    /// no label: the `swift run` and render-harness case.
    func testThisProcessHasNoLabel() {
        XCTAssertNil(FanHelperRegistration.label)
        XCTAssertEqual(FanHelperRegistration.status(), .noLabel)
    }

    /// The build script writes the key the app reads, by the same name.
    func testTheBuildScriptWritesTheKeyTheAppReads() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root = root.deletingLastPathComponent() }
        let script = try String(
            contentsOf: root.appendingPathComponent("apps/macos/scripts/build-tcrbar.sh"),
            encoding: .utf8)
        XCTAssertTrue(
            script.contains(
                "plutil -insert \(FanHelperRegistration.infoPlistKey) -string \"$fan_helper_label\""),
            "build-tcrbar.sh no longer writes \(FanHelperRegistration.infoPlistKey) from the label it shipped"
        )
    }

    /// The label is one string in three places: here, the Rust identity and
    /// the plist in the repository. Read the plist from the source tree, so a
    /// rename in one place breaks this test.
    func testTheReleaseLabelMatchesTheCommittedPlist() throws {
        let plist = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // TcrBarTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // apps/macos
            .appendingPathComponent("LaunchDaemons")
            .appendingPathComponent(
                FanHelperRegistration.plistName(forLabel: FanHelperRegistration.releaseLabel))
        let data = try Data(contentsOf: plist)
        let object = try PropertyListSerialization.propertyList(from: data, format: nil)
        guard let dict = object as? [String: Any] else {
            return XCTFail("\(plist.path) is not a dictionary")
        }
        XCTAssertEqual(dict["Label"] as? String, FanHelperRegistration.releaseLabel)
        XCTAssertEqual(
            dict["MachServices"] as? [String: Bool], [FanHelperRegistration.releaseLabel: true])
        XCTAssertEqual(dict["BundleProgram"] as? String, "Contents/MacOS/tcr-fan-helper")
        XCTAssertNil(dict["RunAtLoad"])
        XCTAssertNil(dict["KeepAlive"])
    }

    func testTheCommandLineIsParsedStrictly() {
        XCTAssertNil(FanHelperRegistration.request(["TcrBar"]))
        XCTAssertEqual(FanHelperRegistration.request(["TcrBar", "--fan-helper", "status"]), .status)
        XCTAssertEqual(FanHelperRegistration.request(["TcrBar", "--fan-helper", "register"]), .register)
        XCTAssertEqual(
            FanHelperRegistration.request(["TcrBar", "--fan-helper", "unregister"]), .unregister)
        guard case .usage = FanHelperRegistration.request(["TcrBar", "--fan-helper"]) else {
            return XCTFail("a bare --fan-helper must be a usage error, not a start")
        }
        guard case .usage = FanHelperRegistration.request(["TcrBar", "--fan-helper", "install"]) else {
            return XCTFail("an unknown verb must be a usage error, not a start")
        }
    }

    // MARK: register() that throws

    private struct Refused: Error {}

    /// 2026-10-18, Gil's Mac: `register()` threw "Operation not permitted"
    /// and the status right after read `requiresApproval`. That is a
    /// registration; only another status keeps the failure.
    func testAThrowIsJudgedByTheStatusAfterIt() {
        typealias R = FanHelperRegistration
        for after in [R.Status.requiresApproval, .enabled] {
            XCTAssertEqual(R.registerOutcome(thrown: Refused(), statusAfter: after), .registered(after))
        }
        for after in [R.Status.notRegistered, .notFound, .unrecognised(rawValue: 9), .noLabel] {
            XCTAssertEqual(R.registerOutcome(thrown: Refused(), statusAfter: after), .failed)
        }
        // No throw: whatever the status says, the call did register.
        XCTAssertEqual(
            R.registerOutcome(thrown: nil, statusAfter: .requiresApproval), .registered(.requiresApproval))
        XCTAssertEqual(
            R.registerOutcome(thrown: nil, statusAfter: .notRegistered), .registered(.notRegistered))
    }
}
