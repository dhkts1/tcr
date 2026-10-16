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
        let saved = await controller.save(rowKey: SettingsRowBadge.switchThreshold, values: ["90%"])
        XCTAssertFalse(saved, "a pinned controller saves nothing")
        XCTAssertTrue(controller.refusals.isEmpty)
    }

    // MARK: - running, and what waits

    /// The document with a `running` object, as `tcr config show --json`
    /// prints it beside a proxy: two edits saved (switch 0.95 → 0.9, pacing on
    /// at 4), the proxy still on its boot values, Fable following on both sides.
    private let documentWithRunning = """
        {
          "configPath": "/Users/alice/.config/tcr.json",
          "switchThreshold": 0.9, "controlReserve": 0.05,
          "fableWeeklyThreshold": 0.9, "fableWeeklyThresholdFollows": true,
          "resetUrgencyTierHours": 24, "sessionAffinity": true,
          "controlAccount": "alice@example.com", "controlPooled": false,
          "accountThrottle": { "minSpacingMs": 350, "burst": 24 },
          "fleetThrottle": { "minSpacingMs": 100, "burst": 32 },
          "pacing": { "maxInFlightPerAccount": 4, "minSpacingMs": null },
          "usageRetentionDays": 90, "http1Only": false,
          "running": {
            "configPath": "/Users/alice/.config/tcr.json",
            "switchThreshold": 0.95, "controlReserve": 0.05,
            "fableWeeklyThreshold": 0.95, "fableWeeklyThresholdFollows": true,
            "resetUrgencyTierHours": 24, "sessionAffinity": true,
            "controlAccount": "alice@example.com", "controlPooled": false,
            "accountThrottle": { "minSpacingMs": 350, "burst": 24 },
            "fleetThrottle": { "minSpacingMs": 100, "burst": 32 },
            "pacing": null,
            "usageRetentionDays": 90, "http1Only": false
          }
        }
        """

    private func decode(_ json: String) throws -> ConfigSettings {
        try JSONDecoder().decode(ConfigSettings.self, from: Data(json.utf8))
    }

    /// Two edits wait; Fable, which follows the switch threshold on both sides,
    /// is not counted a second time for the switch edit.
    func testTheRunningObjectDecodesAndNamesWhatWaits() throws {
        let settings = try decode(documentWithRunning)
        XCTAssertEqual(settings.running?.switchThreshold, 0.95)
        XCTAssertNil(settings.running?.pacing)
        XCTAssertTrue(settings.fableWeeklyThresholdFollows)
        XCTAssertEqual(settings.waitingKeys, [SettingsRowBadge.switchThreshold, SettingsRowBadge.pacing])
    }

    /// No `running` (no proxy, or an older tcr) is never "everything matches":
    /// nothing waits, and the line says no proxy reported. An older tcr's
    /// missing `fableWeeklyThresholdFollows` reads as not following.
    func testNoRunningObjectMeansNothingWaitsAndSaysSo() throws {
        let settings = try decoded()
        XCTAssertNil(settings.running)
        XCTAssertFalse(settings.fableWeeklyThresholdFollows)
        XCTAssertEqual(settings.waitingKeys, [])
        XCTAssertTrue(
            ConfigSettingsFormat.waitingLine(waiting: 0, proxyAnswered: false)
                .hasPrefix("No proxy reported"))
    }

    /// A Fable override waits on its own once it stops following, even at the
    /// number it used to follow.
    func testAFableOverrideWaitsWhenItStopsFollowing() throws {
        let json =
            documentWithRunning
            .replacingOccurrences(
                of: "\"fableWeeklyThreshold\": 0.9, \"fableWeeklyThresholdFollows\": true",
                with: "\"fableWeeklyThreshold\": 0.95, \"fableWeeklyThresholdFollows\": false")
        let settings = try decode(json)
        XCTAssertTrue(settings.waitingKeys.contains(SettingsRowBadge.fableWeeklyThreshold))
    }

    func testTheWaitingLineCountsInWords() {
        XCTAssertEqual(
            ConfigSettingsFormat.waitingLine(waiting: 0, proxyAnswered: true),
            "Nothing is waiting: the proxy runs every value saved here.")
        XCTAssertEqual(
            ConfigSettingsFormat.waitingLine(waiting: 1, proxyAnswered: true),
            "1 change is saved and takes effect when the proxy restarts. "
                + "A restart costs warm prompt caches older than 15 minutes.")
        XCTAssertEqual(
            ConfigSettingsFormat.waitingLine(waiting: 2, proxyAnswered: true),
            "2 changes are saved and take effect when the proxy restarts. "
                + "A restart costs warm prompt caches older than 15 minutes.")
    }

    // MARK: - saving

    /// Every editable row maps to the config-file key `tcr config set` takes,
    /// and the control account to none (it saves through `tcr control`).
    func testEveryRowButTheControlAccountHasASetKey() {
        let keys = ConfigSettingsFormat.keys.map { ($0, ConfigSetCommand.fileKey(for: $0)) }
        for (row, fileKey) in keys where row != SettingsRowBadge.controlAccount {
            XCTAssertNotNil(fileKey, row)
        }
        XCTAssertNil(ConfigSetCommand.fileKey(for: SettingsRowBadge.controlAccount))
        XCTAssertEqual(
            ConfigSetCommand.fileKey(for: SettingsRowBadge.resetUrgencyTier), "resetUrgencyTierHours")
        XCTAssertEqual(
            Set(keys.compactMap(\.1)).count, 11, "eleven distinct keys, as `src/config_set.rs` has")
    }

    func testTheSetArgvIsTheVerbTheBinaryDefines() {
        XCTAssertEqual(
            ConfigSetCommand.arguments(key: "accountThrottle", values: ["350", "24"]),
            ["config", "set", "accountThrottle", "350", "24"])
    }

    /// A refusal carries tcr's own sentence without its `Error: ` prefix.
    func testARefusalCarriesTheSentence() {
        XCTAssertEqual(ConfigSetCommand.classify(exitCode: 0, stderr: "[tcr] WARNING: x"), .saved)
        XCTAssertEqual(
            ConfigSetCommand.classify(
                exitCode: 1,
                stderr: "Error: controlReserve takes a share of quota from 0 to 50%, as 5% or 0.05, "
                    + "not '0.6'. Nothing was changed.\n"),
            .refused(
                "controlReserve takes a share of quota from 0 to 50%, as 5% or 0.05, "
                    + "not '0.6'. Nothing was changed."))
        XCTAssertEqual(ConfigSetCommand.classify(exitCode: 2, stderr: ""), .refused("tcr exited 2"))
    }

    func testAPercentFieldHoldsTheNumberWithoutItsSign() {
        XCTAssertEqual(ConfigSettingsFormat.percentField(0.9), "90")
        XCTAssertEqual(ConfigSettingsFormat.percentField(0.05), "5")
        XCTAssertEqual(ConfigSettingsFormat.percentField(0.955), "95.5")
    }

    /// A hand-edited value outside the menu's list is still offered, so the
    /// menu shows what is saved rather than a neighbour.
    func testAMenuOffersTheSavedValueEvenOffTheList() {
        XCTAssertEqual(ConfigSettingsFormat.urgencyTierChoices(including: 24), [0, 6, 12, 24, 48, 72, 168])
        XCTAssertTrue(ConfigSettingsFormat.urgencyTierChoices(including: 36).contains(36))
        XCTAssertTrue(ConfigSettingsFormat.retentionChoices(including: 45).contains(45))
    }
}
