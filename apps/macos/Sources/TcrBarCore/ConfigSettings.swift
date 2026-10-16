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
/// default applied, as saved in the config file (`src/config_show.rs`,
/// `ConfigShow`), and under ``running`` the ones the proxy on the config's
/// port is using, when one answered. Keys this type does not name are
/// ignored, so a newer `tcr` that adds one does not break the decode.
public struct ConfigSettings: Decodable, Equatable, Sendable {
    public let configPath: String
    /// Fractions, as the file writes them: `0.95`, not `95`.
    public let switchThreshold: Double
    public let controlReserve: Double
    public let fableWeeklyThreshold: Double
    /// `true` when the file sets no Fable override, so ``fableWeeklyThreshold``
    /// is the switch threshold's. `false` from a `tcr` too old to say.
    public let fableWeeklyThresholdFollows: Bool
    public let resetUrgencyTierHours: Int
    public let sessionAffinity: Bool
    public let controlAccount: String?
    public let controlPooled: Bool
    public let accountThrottle: ConfigThrottle?
    public let fleetThrottle: ConfigThrottle?
    public let pacing: ConfigPacing?
    public let usageRetentionDays: Int
    public let http1Only: Bool
    /// Zero or one element: the `running` object. An array because a struct
    /// cannot hold an optional of itself directly, and an array's storage is
    /// out of line. Read it through ``running``.
    private let runningStorage: [ConfigSettings]

    /// The settings the proxy on the config's port is running, or `nil` when
    /// no proxy answered (or this `tcr` is too old to ask). `nil` is never
    /// "everything matches": only a reading here can clear a waiting mark.
    public var running: ConfigSettings? { runningStorage.first }

    public init(
        configPath: String, switchThreshold: Double, controlReserve: Double,
        fableWeeklyThreshold: Double, fableWeeklyThresholdFollows: Bool = false,
        resetUrgencyTierHours: Int, sessionAffinity: Bool,
        controlAccount: String?, controlPooled: Bool, accountThrottle: ConfigThrottle?,
        fleetThrottle: ConfigThrottle?, pacing: ConfigPacing?, usageRetentionDays: Int,
        http1Only: Bool, running: ConfigSettings? = nil
    ) {
        self.configPath = configPath
        self.switchThreshold = switchThreshold
        self.controlReserve = controlReserve
        self.fableWeeklyThreshold = fableWeeklyThreshold
        self.fableWeeklyThresholdFollows = fableWeeklyThresholdFollows
        self.resetUrgencyTierHours = resetUrgencyTierHours
        self.sessionAffinity = sessionAffinity
        self.controlAccount = controlAccount
        self.controlPooled = controlPooled
        self.accountThrottle = accountThrottle
        self.fleetThrottle = fleetThrottle
        self.pacing = pacing
        self.usageRetentionDays = usageRetentionDays
        self.http1Only = http1Only
        self.runningStorage = running.map { [$0] } ?? []
    }

    private enum CodingKeys: String, CodingKey {
        case configPath, switchThreshold, controlReserve, fableWeeklyThreshold
        case fableWeeklyThresholdFollows, resetUrgencyTierHours, sessionAffinity
        case controlAccount, controlPooled, accountThrottle, fleetThrottle, pacing
        case usageRetentionDays, http1Only, running
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        configPath = try c.decode(String.self, forKey: .configPath)
        switchThreshold = try c.decode(Double.self, forKey: .switchThreshold)
        controlReserve = try c.decode(Double.self, forKey: .controlReserve)
        fableWeeklyThreshold = try c.decode(Double.self, forKey: .fableWeeklyThreshold)
        fableWeeklyThresholdFollows =
            try c.decodeIfPresent(Bool.self, forKey: .fableWeeklyThresholdFollows) ?? false
        resetUrgencyTierHours = try c.decode(Int.self, forKey: .resetUrgencyTierHours)
        sessionAffinity = try c.decode(Bool.self, forKey: .sessionAffinity)
        controlAccount = try c.decodeIfPresent(String.self, forKey: .controlAccount)
        controlPooled = try c.decode(Bool.self, forKey: .controlPooled)
        accountThrottle = try c.decodeIfPresent(ConfigThrottle.self, forKey: .accountThrottle)
        fleetThrottle = try c.decodeIfPresent(ConfigThrottle.self, forKey: .fleetThrottle)
        pacing = try c.decodeIfPresent(ConfigPacing.self, forKey: .pacing)
        usageRetentionDays = try c.decode(Int.self, forKey: .usageRetentionDays)
        http1Only = try c.decode(Bool.self, forKey: .http1Only)
        runningStorage = try c.decodeIfPresent(ConfigSettings.self, forKey: .running).map { [$0] } ?? []
    }

    /// The row keys (``SettingsRowBadge``) whose saved value the running proxy
    /// is not using yet, in the order the pane draws them. Empty when no proxy
    /// answered. The control account never waits: `tcr control` applies it to
    /// the running proxy at once. Fable waits only when its own override
    /// changed, the same rule as `ConfigShow::waiting` in `src/config_show.rs`.
    public var waitingKeys: [String] {
        guard let running else { return [] }
        let fableWaits =
            fableWeeklyThresholdFollows != running.fableWeeklyThresholdFollows
            || (!fableWeeklyThresholdFollows
                && fableWeeklyThreshold != running.fableWeeklyThreshold)
        let differs: [(String, Bool)] = [
            (SettingsRowBadge.switchThreshold, switchThreshold != running.switchThreshold),
            (SettingsRowBadge.controlReserve, controlReserve != running.controlReserve),
            (SettingsRowBadge.fableWeeklyThreshold, fableWaits),
            (SettingsRowBadge.resetUrgencyTier, resetUrgencyTierHours != running.resetUrgencyTierHours),
            (SettingsRowBadge.sessionAffinity, sessionAffinity != running.sessionAffinity),
            (SettingsRowBadge.controlPooled, controlPooled != running.controlPooled),
            (SettingsRowBadge.accountThrottle, accountThrottle != running.accountThrottle),
            (SettingsRowBadge.fleetThrottle, fleetThrottle != running.fleetThrottle),
            (SettingsRowBadge.pacing, pacing != running.pacing),
            (SettingsRowBadge.usageRetentionDays, usageRetentionDays != running.usageRetentionDays),
            (SettingsRowBadge.http1Only, http1Only != running.http1Only),
        ]
        return differs.filter(\.1).map(\.0)
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

    /// A fraction as the number a percent field holds: `0.9` → `90`,
    /// `0.955` → `95.5`. The field draws the `%` as its unit.
    public static func percentField(_ fraction: Double) -> String {
        let tenths = (fraction * 1000).rounded()
        if tenths.truncatingRemainder(dividingBy: 10) == 0 {
            return "\(Int(tenths / 10))"
        }
        return String(format: "%.1f", tenths / 10)
    }

    /// The fixed line under Limits. `waiting` is ``ConfigSettings/waitingKeys``'
    /// count; `proxyAnswered` is whether the reading carries a `running`
    /// object at all, since with no proxy nothing can be waiting on it.
    public static func waitingLine(waiting: Int, proxyAnswered: Bool) -> String {
        guard proxyAnswered else {
            return "No proxy reported the values it runs, so a saved change takes effect "
                + "when one starts."
        }
        switch waiting {
        case 0:
            return "Nothing is waiting: the proxy runs every value saved here."
        case 1:
            return "1 change is saved and takes effect when the proxy restarts. "
                + restartCost
        default:
            return "\(waiting) changes are saved and take effect when the proxy restarts. "
                + restartCost
        }
    }

    /// What a restart costs, in the mockup's words: affinity pins younger than
    /// `affinity::PIN_TTL_MS` (15 minutes) are restored, older ones are not.
    public static let restartCost =
        "A restart costs warm prompt caches older than 15 minutes."

    /// The hour widths the urgency menu offers, `0` being off. The saved value
    /// is added when it is not one of them, so a hand-edited width still shows.
    public static func urgencyTierChoices(including saved: Int) -> [Int] {
        let base = [0, 6, 12, 24, 48, 72, 168]
        return base.contains(saved) ? base : (base + [saved]).sorted()
    }

    /// The ledger lengths the retention menu offers, plus the saved one.
    public static func retentionChoices(including saved: Int) -> [Int] {
        let base = [7, 14, 30, 90, 180, 365]
        return base.contains(saved) ? base : (base + [saved]).sorted()
    }

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

/// The argv this app hands `tcr` to save one setting, and how to read what
/// came back. `tcr config set` validates the value, and goes through the
/// running proxy when there is one (`src/cli.rs`, `set_config`).
public enum ConfigSetCommand {
    /// `tcr config set <key> <value>...`. `key` is the config file's own name
    /// (``fileKey(for:)``); a throttle passes two values.
    public static func arguments(key: String, values: [String]) -> [String] {
        ["config", "set", key] + values
    }

    /// The config-file key behind one row, or `nil` for a row this verb does
    /// not write (the control account has `tcr control`).
    public static func fileKey(for rowKey: String) -> String? {
        switch rowKey {
        case SettingsRowBadge.switchThreshold: return "switchThreshold"
        case SettingsRowBadge.controlReserve: return "controlReserve"
        case SettingsRowBadge.fableWeeklyThreshold: return "fableWeeklyThreshold"
        case SettingsRowBadge.resetUrgencyTier: return "resetUrgencyTierHours"
        case SettingsRowBadge.sessionAffinity: return "sessionAffinity"
        case SettingsRowBadge.controlPooled: return "controlPooled"
        case SettingsRowBadge.accountThrottle: return "accountThrottle"
        case SettingsRowBadge.fleetThrottle: return "fleetThrottle"
        case SettingsRowBadge.pacing: return "pacing"
        case SettingsRowBadge.usageRetentionDays: return "usageRetentionDays"
        case SettingsRowBadge.http1Only: return "http1Only"
        default: return nil
        }
    }

    /// What one save did. `refused` carries `tcr`'s own sentence, without its
    /// `Error: ` prefix, for the row to show.
    public enum Outcome: Equatable, Sendable {
        case saved
        case refused(String)
    }

    public static func classify(exitCode: Int32, stderr: String) -> Outcome {
        guard exitCode != 0 else { return .saved }
        var text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        if text.hasPrefix("Error: ") { text.removeFirst("Error: ".count) }
        return .refused(text.isEmpty ? "tcr exited \(exitCode)" : text)
    }

    /// Blocking save, always off the main actor.
    nonisolated static func performSet(key: String, values: [String]) -> Outcome {
        run(arguments(key: key, values: values))
    }

    /// Blocking `tcr control <name>` / `tcr control --clear`, read the same way.
    nonisolated static func performControl(name: String?) -> Outcome {
        run(ControlAccountCommand.setArguments(name: name))
    }

    nonisolated private static func run(_ arguments: [String]) -> Outcome {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .refused("tcr not found (searched \(notFound.searched.count) locations)")
        case .success(let executable):
            do {
                let output = try TcrTool.run(executable: executable, arguments: arguments)
                return classify(exitCode: output.exitCode, stderr: output.stderr)
            } catch {
                return .refused(error.localizedDescription)
            }
        }
    }
}

/// Settings > Groups & Rotation's read of `tcr config show --json`, and its
/// saves through `tcr config set` and `tcr control`.
@MainActor
public final class ConfigSettingsController: ObservableObject {
    /// `nil` until the first read lands.
    @Published public private(set) var reading: ConfigSettingsCommand.Reading?
    /// The sentence `tcr` refused a row's last value with, by row key
    /// (``SettingsRowBadge``). Cleared by that row's next save.
    @Published public private(set) var refusals: [String: String] = [:]
    /// Row keys with a save in flight.
    @Published public private(set) var saving: Set<String> = []

    /// The render harness never spawns `tcr`.
    private let inert: Bool

    public init() {
        self.inert = false
    }

    /// A controller pinned to one reading, for the render harness and tests.
    /// ``refresh()`` and every save do nothing on it; `refusals` lets a render
    /// draw a refused row.
    public init(pinned reading: ConfigSettingsCommand.Reading?, refusals: [String: String] = [:]) {
        self.inert = true
        self.reading = reading
        self.refusals = refusals
    }

    /// Save one row's value through `tcr config set`, off the main thread,
    /// then re-read. Returns whether `tcr` took it; on a refusal the row's
    /// sentence is in ``refusals`` and the caller puts its field back to the
    /// saved value.
    @discardableResult
    public func save(rowKey: String, values: [String]) async -> Bool {
        guard let fileKey = ConfigSetCommand.fileKey(for: rowKey) else {
            refusals[rowKey] = "This row is not saved through tcr config set."
            return false
        }
        return await perform(rowKey: rowKey) {
            ConfigSetCommand.performSet(key: fileKey, values: values)
        }
    }

    /// Set the control account, or clear it with `nil`, through `tcr control`.
    @discardableResult
    public func saveControlAccount(_ name: String?) async -> Bool {
        await perform(rowKey: SettingsRowBadge.controlAccount) {
            ConfigSetCommand.performControl(name: name)
        }
    }

    private func perform(
        rowKey: String, _ work: @escaping @Sendable () -> ConfigSetCommand.Outcome
    ) async -> Bool {
        guard !inert, !saving.contains(rowKey) else { return false }
        saving.insert(rowKey)
        defer { saving.remove(rowKey) }
        let outcome = await Task.detached(priority: .userInitiated) { work() }.value
        await refresh()
        switch outcome {
        case .saved:
            refusals[rowKey] = nil
            return true
        case .refused(let sentence):
            refusals[rowKey] = sentence
            return false
        }
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
