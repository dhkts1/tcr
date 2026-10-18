import Foundation

/// What the proxy's fan controller is doing: the `fanControl` object `tcr
/// sessions --json` carries beside `heat` (the status wire in
/// `crates/tcr-status-wire`), and what `tcr fan --json` prints.
///
/// The proxy holds the fans; TcrBar only shows this and asks for a mode with
/// `tcr fan ...` (``FanControlModel``). `state` and `reason` are enums on both
/// sides: a value this build does not know decodes to `.unrecognised(raw)` and
/// is shown as the proxy's own word, never mapped onto a known one.
public struct FanControl: Equatable, Sendable, Decodable {
    /// The mode the proxy was last asked for. It stays what was asked even
    /// while the fans cannot follow it (no helper yet, another app holding
    /// them), which is how the popover knows which segment to light.
    public let desired: FanDesired
    public let state: FanControlState
    /// Why the state is what it is. Nil while nothing is wrong.
    public let reason: FanControlReason?
    /// The CPU passed the hot guard, so every fan is at full speed whatever
    /// the mode says.
    public let hotGuard: Bool
    /// The speed the controller last wrote to each fan. Empty while it holds
    /// none.
    public let targets: [FanTarget]
    /// The proxy's own sentence about the state, shown under the controls.
    public let detail: String?

    public init(
        desired: FanDesired,
        state: FanControlState,
        reason: FanControlReason? = nil,
        hotGuard: Bool = false,
        targets: [FanTarget] = [],
        detail: String? = nil
    ) {
        self.desired = desired
        self.state = state
        self.reason = reason
        self.hotGuard = hotGuard
        self.targets = targets
        self.detail = detail
    }

    private enum CodingKeys: String, CodingKey {
        case desired, state, reason, hotGuard, targets, detail
    }

    /// `desired` and `state` are required: an object without them says nothing
    /// the popover could act on, so the whole object is refused (and
    /// ``Fleet/decodeFanControl(_:log:)`` logs it once). Every other key may be
    /// missing or null, and a key this build does not know is ignored.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        desired = try c.decode(FanDesired.self, forKey: .desired)
        state = try c.decode(FanControlState.self, forKey: .state)
        reason = try c.decodeIfPresent(FanControlReason.self, forKey: .reason)
        hotGuard = try c.decodeIfPresent(Bool.self, forKey: .hotGuard) ?? false
        targets = try c.decodeIfPresent([FanTarget].self, forKey: .targets) ?? []
        detail = try c.decodeIfPresent(String.self, forKey: .detail)
    }
}

/// One fan's written target, `{"fan": 0, "rpm": 5777.0}`.
public struct FanTarget: Equatable, Sendable, Decodable {
    public let fan: Int
    public let rpm: Double

    public init(fan: Int, rpm: Double) {
        self.fan = fan
        self.rpm = rpm
    }
}

/// Follow CPU's two temperatures: fans at their minimum below `quietBelowC`,
/// at full speed from `fullAtC`, a straight line between.
public struct FanCurve: Equatable, Sendable {
    public let quietBelowC: Int
    public let fullAtC: Int

    public init(quietBelowC: Int, fullAtC: Int) {
        self.quietBelowC = quietBelowC
        self.fullAtC = fullAtC
    }

    /// The mockup's panel 10, and what Follow CPU starts at before the proxy
    /// has been asked for a curve.
    public static let starting = FanCurve(quietBelowC: 60, fullAtC: 90)

    /// The proxy's own limits (`FollowCurve` in `crates/tcr-fan`): the
    /// lower temperature at least 20 °C, the upper at least 5 °C above it and
    /// at most the hot guard's 100 °C. The steppers stop at these rather than
    /// send a curve the proxy would refuse.
    public static let lowestQuiet = 20
    public static let smallestGap = 5
    public static let highestFull = 100

    public var isAccepted: Bool {
        quietBelowC >= Self.lowestQuiet && fullAtC - quietBelowC >= Self.smallestGap
            && fullAtC <= Self.highestFull
    }

    /// How far along the line `celsius` sits: 0 at or below the quiet
    /// temperature, 1 at or above full, linear between. Nil for a reading that
    /// is not a number.
    public func share(atCelsius celsius: Double) -> Double? {
        guard celsius.isFinite else { return nil }
        let low = Double(quietBelowC)
        let high = Double(fullAtC)
        if celsius <= low { return 0 }
        if celsius >= high { return 1 }
        return (celsius - low) / (high - low)
    }
}

/// `fanControl.desired`: `{"mode":"auto"}`, `{"mode":"fixed","rpm":5000}`,
/// `{"mode":"followCpu","quietBelowC":60,"fullAtC":90}`.
public enum FanDesired: Equatable, Sendable, Decodable {
    case auto
    case fixed(rpm: Double)
    case followCPU(FanCurve)
    /// A mode this build does not know, by the proxy's own name for it.
    case unrecognised(String)

    /// The segment this mode lights, or nil for a mode the popover has no
    /// segment for.
    public var choice: FanModeChoice? {
        switch self {
        case .auto: return .auto
        case .fixed: return .fixed
        case .followCPU: return .followCPU
        case .unrecognised: return nil
        }
    }

    private enum CodingKeys: String, CodingKey {
        case mode, rpm, quietBelowC, fullAtC
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let mode = try c.decode(String.self, forKey: .mode)
        switch mode {
        case "auto":
            self = .auto
        case "fixed":
            self = .fixed(rpm: try c.decode(Double.self, forKey: .rpm))
        case "followCpu":
            self = .followCPU(
                FanCurve(
                    quietBelowC: try Self.wholeDegrees(c, .quietBelowC),
                    fullAtC: try Self.wholeDegrees(c, .fullAtC)))
        default:
            self = .unrecognised(mode)
        }
    }

    /// A temperature the proxy may print as `60` or `60.0`. A fraction is
    /// refused rather than rounded: the steppers move in whole degrees, and a
    /// curve they cannot show exactly is not one to guess at.
    private static func wholeDegrees(
        _ c: KeyedDecodingContainer<CodingKeys>, _ key: CodingKeys
    ) throws -> Int {
        let value = try c.decode(Double.self, forKey: key)
        guard value.isFinite, value.rounded() == value, abs(value) < 1_000 else {
            throw DecodingError.dataCorruptedError(
                forKey: key, in: c, debugDescription: "\(key.rawValue) is not a whole degree: \(value)")
        }
        return Int(value)
    }
}

/// `fanControl.state`: the proxy's fan controller's state.
public enum FanControlState: Equatable, Sendable, Decodable {
    /// Automatic, and nothing held.
    case off
    /// Taking the fans from macOS (up to about 5 s on Apple Silicon).
    case entering
    case holding
    /// Handing the fans back to macOS.
    case releasing
    /// Another program holds the fans; ``FanControl/reason`` says which.
    case foreign
    /// A fan read failed, so nothing is written until one succeeds.
    case unknown
    /// Fan control cannot run; ``FanControl/reason`` says why.
    case unavailable
    /// Handing back failed after its retries.
    case stuck
    case unrecognised(String)

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        switch raw {
        case "off": self = .off
        case "entering": self = .entering
        case "holding": self = .holding
        case "releasing": self = .releasing
        case "foreign": self = .foreign
        case "unknown": self = .unknown
        case "unavailable": self = .unavailable
        case "stuck": self = .stuck
        default: self = .unrecognised(raw)
        }
    }
}

/// `fanControl.reason`: who else holds the fans, or why control cannot run.
public enum FanControlReason: Equatable, Sendable, Decodable {
    case macsFanControl
    case anotherApp
    case notSignedForFans
    case noSmc
    case noFans
    case helperUnreachable
    case helperNoReply
    case helperSignature
    case helperVersion
    case modeLocked
    case contended
    case helperRefused
    case unrecognised(String)

    /// The reason's wire name: what `tcr fan` prints, and what the popover
    /// shows for a reason it has no sentence for.
    public var word: String {
        switch self {
        case .macsFanControl: return "macsFanControl"
        case .anotherApp: return "anotherApp"
        case .notSignedForFans: return "notSignedForFans"
        case .noSmc: return "noSmc"
        case .noFans: return "noFans"
        case .helperUnreachable: return "helperUnreachable"
        case .helperNoReply: return "helperNoReply"
        case .helperSignature: return "helperSignature"
        case .helperVersion: return "helperVersion"
        case .modeLocked: return "modeLocked"
        case .contended: return "contended"
        case .helperRefused: return "helperRefused"
        case .unrecognised(let raw): return raw
        }
    }

    init(word: String) {
        switch word {
        case "macsFanControl": self = .macsFanControl
        case "anotherApp": self = .anotherApp
        case "notSignedForFans": self = .notSignedForFans
        case "noSmc": self = .noSmc
        case "noFans": self = .noFans
        case "helperUnreachable": self = .helperUnreachable
        case "helperNoReply": self = .helperNoReply
        case "helperSignature": self = .helperSignature
        case "helperVersion": self = .helperVersion
        case "modeLocked": self = .modeLocked
        case "contended": self = .contended
        case "helperRefused": self = .helperRefused
        default: self = .unrecognised(word)
        }
    }

    /// A bare string, `"helperUnreachable"`. Also serde's externally tagged
    /// form for a variant that carries a value, `{"helperVersion": {...}}`
    /// or `{"modeLocked": 1}`: an object with exactly one key is that key's
    /// reason, and the value is left to ``FanControl/detail``. Anything else
    /// fails the decode.
    public init(from decoder: Decoder) throws {
        if let word = try? decoder.singleValueContainer().decode(String.self) {
            self.init(word: word)
            return
        }
        let tagged = try decoder.container(keyedBy: AnyKey.self)
        guard tagged.allKeys.count == 1, let key = tagged.allKeys.first else {
            throw DecodingError.dataCorrupted(
                DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription:
                        "reason is neither a string nor an object with one key"))
        }
        self.init(word: key.stringValue)
    }

    private struct AnyKey: CodingKey {
        let stringValue: String
        var intValue: Int? { nil }
        init(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { nil }
    }
}

/// The popover's three segments.
public enum FanModeChoice: CaseIterable, Equatable, Sendable {
    case auto, fixed, followCPU

    public var label: String {
        switch self {
        case .auto: return "Auto"
        case .fixed: return "Fixed"
        case .followCPU: return "Follow CPU"
        }
    }
}

/// What `tcr fan ...` TcrBar runs.
public enum FanCommand: Equatable, Sendable {
    case auto
    case fixed(rpm: Int)
    case follow(FanCurve)

    public var arguments: [String] {
        switch self {
        case .auto: return ["fan", "auto"]
        case .fixed(let rpm): return ["fan", "fixed", String(rpm)]
        case .follow(let curve):
            return ["fan", "follow", String(curve.quietBelowC), String(curve.fullAtC)]
        }
    }

    /// The command that asks for `desired` again, or nil for a mode this
    /// build cannot name.
    public init?(_ desired: FanDesired) {
        switch desired {
        case .auto: self = .auto
        case .fixed(let rpm):
            guard let whole = MachineStats.whole(rpm), whole > 0 else { return nil }
            self = .fixed(rpm: whole)
        case .followCPU(let curve): self = .follow(curve)
        case .unrecognised: return nil
        }
    }
}
