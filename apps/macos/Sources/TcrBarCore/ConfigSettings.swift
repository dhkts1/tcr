import Combine
import Foundation

/// One rate bucket as `tcr config show --json` reports it (`src/config_show.rs`,
/// `ThrottleSetting`): `burst` requests at once after an idle spell, then one
/// per `minSpacingMs`.
public struct ConfigThrottle: Decodable, Equatable, Sendable {
    public let minSpacingMs: Int
    public let burst: Int

    public init(minSpacingMs: Int, burst: Int) {
        self.minSpacingMs = minSpacingMs
        self.burst = burst
    }
}

/// Per-account pacing as `tcr config show --json` reports it. Either knob may
/// be absent; both absent never arrives here, because pacing off is `null`
/// one level up.
public struct ConfigPacing: Decodable, Equatable, Sendable {
    public let maxInFlightPerAccount: Int?
    public let minSpacingMs: Int?

    public init(maxInFlightPerAccount: Int?, minSpacingMs: Int?) {
        self.maxInFlightPerAccount = maxInFlightPerAccount
        self.minSpacingMs = minSpacingMs
    }
}

/// `tcr config show --json`: the effective rotation and limit settings, every
/// default applied, read from the config file without asking the proxy
/// (`src/config_show.rs`, `ConfigSettings`). Keys this type does not name are
/// ignored, so a newer `tcr` that adds one does not break the decode.
public struct ConfigSettings: Decodable, Equatable, Sendable {
    public let configPath: String
    /// Fractions, as the file writes them: `0.95`, not `95`.
    public let switchThreshold: Double
    public let controlReserve: Double
    public let fableWeeklyThreshold: Double
    public let resetUrgencyTierHours: Int
    public let sessionAffinity: Bool
    public let controlAccount: String?
    public let controlPooled: Bool
    public let accountThrottle: ConfigThrottle?
    public let fleetThrottle: ConfigThrottle?
    public let pacing: ConfigPacing?
    public let usageRetentionDays: Int
    public let http1Only: Bool

    public init(
        configPath: String, switchThreshold: Double, controlReserve: Double,
        fableWeeklyThreshold: Double, resetUrgencyTierHours: Int, sessionAffinity: Bool,
        controlAccount: String?, controlPooled: Bool, accountThrottle: ConfigThrottle?,
        fleetThrottle: ConfigThrottle?, pacing: ConfigPacing?, usageRetentionDays: Int,
        http1Only: Bool
    ) {
        self.configPath = configPath
        self.switchThreshold = switchThreshold
        self.controlReserve = controlReserve
        self.fableWeeklyThreshold = fableWeeklyThreshold
        self.resetUrgencyTierHours = resetUrgencyTierHours
        self.sessionAffinity = sessionAffinity
        self.controlAccount = controlAccount
        self.controlPooled = controlPooled
        self.accountThrottle = accountThrottle
        self.fleetThrottle = fleetThrottle
        self.pacing = pacing
        self.usageRetentionDays = usageRetentionDays
        self.http1Only = http1Only
    }
}

/// The words each Settings row shows for its value. Pure, so every case is a
/// unit test rather than a look at a render.
public enum ConfigSettingsFormat {
    /// `0.95` → `95%`; a value between whole percents keeps one decimal
    /// (`0.955` → `95.5%`) rather than rounding to a figure the file never said.
    public static func percent(_ fraction: Double) -> String {
        let tenths = (fraction * 1000).rounded()
        if tenths.truncatingRemainder(dividingBy: 10) == 0 {
            return "\(Int(tenths / 10))%"
        }
        return String(format: "%.1f%%", tenths / 10)
    }

    /// `24` → `24 h`. `0` turns the urgency term off, so it says so.
    public static func urgencyTier(hours: Int) -> String {
        hours == 0 ? "Off" : "\(hours) h"
    }

    public static func days(_ count: Int) -> String {
        count == 1 ? "1 day" : "\(count) days"
    }

    public static func onOff(_ value: Bool) -> String {
        value ? "On" : "Off"
    }

    public static func controlAccount(_ name: String?) -> String {
        name ?? "None"
    }

    /// `1 per 350 ms, bursts of 24`; a burst of 1 is strict spacing and says
    /// only the rate. `nil` is a bucket turned off.
    public static func throttle(_ value: ConfigThrottle?) -> String {
        guard let value else { return "Off" }
        let rate = "1 per \(value.minSpacingMs) ms"
        return value.burst > 1 ? "\(rate), bursts of \(value.burst)" : rate
    }

    /// `Off`, or each knob that is set: `4 in flight`, `1 per 200 ms`.
    public static func pacing(_ value: ConfigPacing?) -> String {
        guard let value else { return "Off" }
        var parts: [String] = []
        if let cap = value.maxInFlightPerAccount { parts.append("\(cap) in flight") }
        if let spacing = value.minSpacingMs { parts.append("1 per \(spacing) ms") }
        return parts.isEmpty ? "Off" : parts.joined(separator: ", ")
    }

    /// The value a row shows when there is nothing real to show: while the
    /// read runs, and when it failed. Never an example figure.
    public static let missing = "—"

    /// The row text for one Rotation or Limits key (``SettingsRowBadge``),
    /// or `nil` for a key that is not one of those twelve.
    public static func text(for key: String, in settings: ConfigSettings) -> String? {
        switch key {
        case SettingsRowBadge.switchThreshold: return percent(settings.switchThreshold)
        case SettingsRowBadge.controlReserve: return percent(settings.controlReserve)
        case SettingsRowBadge.fableWeeklyThreshold: return percent(settings.fableWeeklyThreshold)
        case SettingsRowBadge.resetUrgencyTier:
            return urgencyTier(hours: settings.resetUrgencyTierHours)
        case SettingsRowBadge.sessionAffinity: return onOff(settings.sessionAffinity)
        case SettingsRowBadge.controlAccount: return controlAccount(settings.controlAccount)
        case SettingsRowBadge.controlPooled: return onOff(settings.controlPooled)
        case SettingsRowBadge.accountThrottle: return throttle(settings.accountThrottle)
        case SettingsRowBadge.fleetThrottle: return throttle(settings.fleetThrottle)
        case SettingsRowBadge.pacing: return pacing(settings.pacing)
        case SettingsRowBadge.usageRetentionDays: return days(settings.usageRetentionDays)
        case SettingsRowBadge.http1Only: return onOff(settings.http1Only)
        default: return nil
        }
    }

    /// The twelve keys ``text(for:in:)`` answers, in the order the pane draws
    /// them.
    public static let keys: [String] = [
        SettingsRowBadge.switchThreshold, SettingsRowBadge.controlReserve,
        SettingsRowBadge.fableWeeklyThreshold, SettingsRowBadge.resetUrgencyTier,
        SettingsRowBadge.sessionAffinity, SettingsRowBadge.controlAccount,
        SettingsRowBadge.controlPooled, SettingsRowBadge.accountThrottle,
        SettingsRowBadge.fleetThrottle, SettingsRowBadge.pacing,
        SettingsRowBadge.usageRetentionDays, SettingsRowBadge.http1Only,
    ]

    /// What one row draws for `reading`: the value, and the tooltip. A loaded
    /// reading shows the value with `hint` as the tooltip; no reading yet, or
    /// a failed one, shows ``missing`` and says why in the tooltip.
    public static func row(
        for key: String, reading: ConfigSettingsCommand.Reading?, hint: String
    ) -> (value: String, help: String) {
        switch reading {
        case .none:
            return (missing, "Reading the config file…")
        case .unavailable(let reason):
            return (missing, "Could not read the settings: \(reason)")
        case .settings(let settings):
            guard let text = text(for: key, in: settings) else {
                return (missing, "No value for \(key)")
            }
            return (text, hint)
        }
    }
}

/// The argv this app hands `tcr` to read the settings, and the one rule for
/// reading what came back. This app never reads the config file itself: it
/// holds credentials (`CLAUDE.md`), and `tcr` is the one parser of it.
public enum ConfigSettingsCommand {
    public static let showArguments = ["config", "show", "--json"]

    /// What one read established. "Could not ask" is kept apart from every
    /// answer: an older `tcr` with no `config` verb must show no value, never
    /// a default dressed as the user's own.
    public enum Reading: Equatable, Sendable {
        case settings(ConfigSettings)
        case unavailable(reason: String)
    }

    public static func classify(exitCode: Int32, stdout: Data, stderr: String) -> Reading {
        guard exitCode == 0 else {
            var text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            if text.hasPrefix("Error: ") { text.removeFirst("Error: ".count) }
            return .unavailable(reason: text.isEmpty ? "tcr exited \(exitCode)" : text)
        }
        do {
            return .settings(try JSONDecoder().decode(ConfigSettings.self, from: stdout))
        } catch {
            return .unavailable(reason: "tcr config show --json did not decode: \(error)")
        }
    }

    /// Blocking read, always off the main actor.
    nonisolated static func performShow() -> Reading {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .unavailable(
                reason: "tcr not found (searched \(notFound.searched.count) locations)")
        case .success(let executable):
            do {
                let output = try TcrTool.run(executable: executable, arguments: showArguments)
                return classify(
                    exitCode: output.exitCode, stdout: output.stdout, stderr: output.stderr)
            } catch {
                return .unavailable(reason: error.localizedDescription)
            }
        }
    }
}

/// Settings > Groups & Rotation's read of `tcr config show --json`.
@MainActor
public final class ConfigSettingsController: ObservableObject {
    /// `nil` until the first read lands.
    @Published public private(set) var reading: ConfigSettingsCommand.Reading?

    /// The render harness never spawns `tcr`.
    private let inert: Bool

    public init() {
        self.inert = false
    }

    /// A controller pinned to one reading, for the render harness and tests.
    /// ``refresh()`` does nothing on it.
    public init(pinned reading: ConfigSettingsCommand.Reading?) {
        self.inert = true
        self.reading = reading
    }

    /// Re-read the settings off the main thread. The previous reading stays
    /// on screen until the new one lands, so a re-read never blanks a value.
    public func refresh() async {
        guard !inert else { return }
        reading = await Task.detached(priority: .utility) {
            ConfigSettingsCommand.performShow()
        }.value
    }
}
