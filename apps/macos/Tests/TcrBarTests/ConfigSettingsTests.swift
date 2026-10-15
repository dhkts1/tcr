import Foundation
import XCTest

@testable import TcrBarCore

/// Settings > Groups & Rotation's values: what `tcr config show --json`
/// decodes to, the words each row shows, and the rule that a failed or
/// pending read shows no value rather than an example one.
final class ConfigSettingsTests: XCTestCase {
    /// Shaped like `tcr config show --json` on a file setting a control
    /// account, pacing, and the fleet throttle off (`tests/config_show.rs`
    /// asserts the same keys from the binary side). Values are fake.
    private let document = """
        {
          "configPath": "/Users/alice/.config/tcr.json",
          "switchThreshold": 0.95,
          "controlReserve": 0.05,
          "fableWeeklyThreshold": 0.95,
          "resetUrgencyTierHours": 24,
          "sessionAffinity": true,
          "controlAccount": "alice@example.com",
          "controlPooled": false,
          "accountThrottle": { "minSpacingMs": 350, "burst": 24 },
          "fleetThrottle": null,
          "pacing": { "maxInFlightPerAccount": 4, "minSpacingMs": null },
          "usageRetentionDays": 90,
          "http1Only": false
        }
        """

    private func decoded() throws -> ConfigSettings {
        let reading = ConfigSettingsCommand.classify(
            exitCode: 0, stdout: Data(document.utf8), stderr: "")
        guard case .settings(let settings) = reading else {
            XCTFail("expected settings, got \(reading)")
            throw CocoaError(.coderReadCorrupt)
        }
        return settings
    }

    func testTheShowDocumentDecodes() throws {
        let settings = try decoded()
        XCTAssertEqual(settings.controlAccount, "alice@example.com")
        XCTAssertEqual(settings.accountThrottle, ConfigThrottle(minSpacingMs: 350, burst: 24))
        XCTAssertNil(settings.fleetThrottle)
        XCTAssertEqual(settings.pacing, ConfigPacing(maxInFlightPerAccount: 4, minSpacingMs: nil))
        XCTAssertEqual(settings.usageRetentionDays, 90)
    }

    /// Every one of the twelve rows, in the words the pane shows.
    func testEachRowSaysItsValueInWords() throws {
        let settings = try decoded()
        let shown = Dictionary(
            uniqueKeysWithValues: ConfigSettingsFormat.keys.map {
                ($0, ConfigSettingsFormat.text(for: $0, in: settings))
            })
        XCTAssertEqual(
            shown,
            [
                SettingsRowBadge.switchThreshold: "95%",
                SettingsRowBadge.controlReserve: "5%",
                SettingsRowBadge.fableWeeklyThreshold: "95%",
                SettingsRowBadge.resetUrgencyTier: "24 h",
                SettingsRowBadge.sessionAffinity: "On",
                SettingsRowBadge.controlAccount: "alice@example.com",
                SettingsRowBadge.controlPooled: "Off",
                SettingsRowBadge.accountThrottle: "1 per 350 ms, bursts of 24",
                SettingsRowBadge.fleetThrottle: "Off",
                SettingsRowBadge.pacing: "4 in flight",
                SettingsRowBadge.usageRetentionDays: "90 days",
                SettingsRowBadge.http1Only: "Off",
            ])
    }

    /// The twelve keys are exactly the Rotation and Limits rows the badge
    /// table registers, so a row added there without a value fails here.
    func testTheKeysAreTheRotationAndLimitsRows() {
        let registered = Set(
            SettingsRowBadge.timing.keys.filter {
                $0.hasPrefix("rotation.") || $0.hasPrefix("limits.")
            })
        XCTAssertEqual(Set(ConfigSettingsFormat.keys), registered)
        XCTAssertEqual(ConfigSettingsFormat.keys.count, 12)
    }

    func testFormattersCoverTheEdges() {
        XCTAssertEqual(ConfigSettingsFormat.percent(1.0), "100%")
        XCTAssertEqual(ConfigSettingsFormat.percent(0.9), "90%")
        XCTAssertEqual(ConfigSettingsFormat.percent(0.955), "95.5%")
        XCTAssertEqual(ConfigSettingsFormat.percent(0), "0%")
        XCTAssertEqual(ConfigSettingsFormat.urgencyTier(hours: 0), "Off")
        XCTAssertEqual(ConfigSettingsFormat.days(1), "1 day")
        XCTAssertEqual(ConfigSettingsFormat.controlAccount(nil), "None")
        XCTAssertEqual(
            ConfigSettingsFormat.throttle(ConfigThrottle(minSpacingMs: 50, burst: 1)),
            "1 per 50 ms")
        XCTAssertEqual(
            ConfigSettingsFormat.pacing(ConfigPacing(maxInFlightPerAccount: nil, minSpacingMs: 200)),
            "1 per 200 ms")
        XCTAssertEqual(
            ConfigSettingsFormat.pacing(ConfigPacing(maxInFlightPerAccount: 2, minSpacingMs: 200)),
            "2 in flight, 1 per 200 ms")
        XCTAssertEqual(ConfigSettingsFormat.pacing(nil), "Off")
    }

    /// An older tcr with no `config` verb: no value, and the reason, never a
    /// default dressed as the user's own.
    func testAFailedReadShowsNoValueAndSaysWhy() {
        let reading = ConfigSettingsCommand.classify(
            exitCode: 2, stdout: Data(),
            stderr: "error: unrecognized subcommand 'config'\n")
        XCTAssertEqual(reading, .unavailable(reason: "error: unrecognized subcommand 'config'"))
        for key in ConfigSettingsFormat.keys {
            let row = ConfigSettingsFormat.row(for: key, reading: reading, hint: "Edit in x")
            XCTAssertEqual(row.value, "—", key)
            XCTAssertTrue(row.help.contains("unrecognized subcommand 'config'"), row.help)
        }
    }

    func testAPendingReadShowsNoValue() {
        for key in ConfigSettingsFormat.keys {
            XCTAssertEqual(
                ConfigSettingsFormat.row(for: key, reading: nil, hint: "Edit in x").value, "—")
        }
    }

    func testALoadedReadShowsTheValueWithTheHint() throws {
        let row = ConfigSettingsFormat.row(
            for: SettingsRowBadge.switchThreshold, reading: .settings(try decoded()),
            hint: "Edit in x")
        XCTAssertEqual(row.value, "95%")
        XCTAssertEqual(row.help, "Edit in x")
    }

    func testOutputThatDoesNotDecodeIsUnavailable() {
        let reading = ConfigSettingsCommand.classify(
            exitCode: 0, stdout: Data("{}".utf8), stderr: "")
        guard case .unavailable(let reason) = reading else {
            return XCTFail("expected unavailable, got \(reading)")
        }
        XCTAssertTrue(reason.hasPrefix("tcr config show --json did not decode"), reason)
    }

    func testTheArgvIsTheVerbTheBinaryDefines() {
        XCTAssertEqual(ConfigSettingsCommand.showArguments, ["config", "show", "--json"])
    }

    @MainActor
    func testAPinnedControllerNeverRunsTcr() async {
        let controller = ConfigSettingsController(pinned: nil)
        await controller.refresh()
        XCTAssertNil(controller.reading)
    }
}
