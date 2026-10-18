import Foundation

/// One fan, as the proxy read it (`FanRow` in `crates/tcr-status-wire`).
public struct FanReading: Equatable, Sendable, Decodable {
    /// 0-based, the `n` in the SMC's `F{n}Ac`.
    public let index: Int
    public let actualRPM: Double
    /// 0 when it did not read.
    public let minRPM: Double
    /// 0 when it did not read: "no maximum", not a ceiling of 0.
    public let maxRPM: Double
    /// The fan's mode key says a program fixed its speed, so it no longer
    /// follows heat.
    public let isManual: Bool

    public init(index: Int, actualRPM: Double, minRPM: Double, maxRPM: Double, isManual: Bool) {
        self.index = index
        self.actualRPM = actualRPM
        self.minRPM = minRPM
        self.maxRPM = maxRPM
        self.isManual = isManual
    }

    /// The wire's camelCase names, `actualRpm` rather than this type's
    /// `actualRPM`.
    private enum CodingKeys: String, CodingKey {
        case index, actualRpm, minRpm, maxRpm, manual
    }

    /// Every key optional, as the Rust side declares them: a later field
    /// must not fail this decode, and a missing one reads as its zero.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        index = try c.decodeIfPresent(Int.self, forKey: .index) ?? 0
        actualRPM = try c.decodeIfPresent(Double.self, forKey: .actualRpm) ?? 0
        minRPM = try c.decodeIfPresent(Double.self, forKey: .minRpm) ?? 0
        maxRPM = try c.decodeIfPresent(Double.self, forKey: .maxRpm) ?? 0
        isManual = try c.decodeIfPresent(Bool.self, forKey: .manual) ?? false
    }
}

/// How hot the CPU runs and how hard the fans work: the `heat` object the
/// proxy reads through AppleSMC (`crates/tcr-smc`) and `tcr sessions --json`
/// carries (`HeatRow` in `crates/tcr-status-wire`).
///
/// The app reads no SMC itself. The proxy does, off its runtime, and drops
/// non-finite and out-of-band figures there, but this is a value anyone can
/// build, so ``MachineStats/heatPhrases`` still never trusts a figure to be
/// a number.
public struct HeatReading: Equatable, Sendable, Decodable {
    /// The hottest CPU sensor, °C. Nil when no CPU sensor read.
    public let cpuCelsius: Double?
    /// Empty on a Mac with no fans, and missing any fan whose speed did not
    /// read.
    public let fans: [FanReading]

    public init(cpuCelsius: Double?, fans: [FanReading]) {
        self.cpuCelsius = cpuCelsius
        self.fans = fans
    }

    private enum CodingKeys: String, CodingKey {
        case cpuCelsius, fans
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        cpuCelsius = try c.decodeIfPresent(Double.self, forKey: .cpuCelsius)
        fans = try c.decodeIfPresent([FanReading].self, forKey: .fans) ?? []
    }
}

/// Where ``Fleet/decodeHeat(_:log:)`` reports a heat object it could not
/// read: `NSLog`, as the rest of the app logs, once per distinct message.
public final class HeatDecodeLog: @unchecked Sendable {
    public static let shared = HeatDecodeLog()

    /// Guards ``logged``: polls decode off the main thread.
    private let lock = NSLock()
    private var logged: Set<String> = []

    /// `NSLog("%@", …)`, never the message as the format: a decoder's words
    /// can hold a `%`.
    public func once(_ line: String) {
        lock.lock()
        let fresh = logged.insert(line).inserted
        lock.unlock()
        if fresh { NSLog("%@", line) }
    }
}
