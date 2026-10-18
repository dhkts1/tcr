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

    /// The dev bundle gets the dev label, so a dev build never registers the
    /// shipping helper; anything else, an unbundled build included, gets the
    /// shipping label.
    func testTheDevBundleGetsTheDevLabel() {
        XCTAssertEqual(
            FanHelperRegistration.label(forBundleIdentifier: "io.github.dhkts1.tcrbar.dev"),
            "io.github.dhkts1.tcrbar.fanhelper.dev")
        XCTAssertEqual(
            FanHelperRegistration.label(forBundleIdentifier: "io.github.dhkts1.tcrbar"),
            "io.github.dhkts1.tcrbar.fanhelper")
        XCTAssertEqual(
            FanHelperRegistration.label(forBundleIdentifier: nil),
            "io.github.dhkts1.tcrbar.fanhelper")
        XCTAssertEqual(
            FanHelperRegistration.plistName(forLabel: FanHelperRegistration.devLabel),
            "io.github.dhkts1.tcrbar.fanhelper.dev.plist")
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
}
