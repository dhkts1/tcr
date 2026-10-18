import CSMC
import Foundation

// The Mac's System Management Controller, read through AppleSMC without root:
// fan speeds, fan modes and temperature sensors.
//
// Reading needs no privilege. Writing does: a non-root write came back
// `kIOReturnNotPrivileged` (0xe00002c1) on 2026-10-18, so ``SMCConnection/write(_:_:)``
// exists for the root helper built on this library, and nothing in TcrBar
// itself writes a fan.
//
// Reference: Stats (exelban/stats, MIT, `SMC/smc.swift`). This is written fresh
// from the protocol; the places that follow Stats say so.

/// A four-character SMC code: a key name (`F0Ac`) or a data type (`flt `).
public struct FourCC: Hashable, Sendable, CustomStringConvertible, ExpressibleByStringLiteral {
    public let rawValue: UInt32

    public init(rawValue: UInt32) {
        self.rawValue = rawValue
    }

    /// Nil unless `text` is exactly four bytes of UTF-8.
    public init?(_ text: String) {
        let bytes = Array(text.utf8)
        guard bytes.count == 4 else { return nil }
        rawValue = bytes.reduce(0) { $0 << 8 | UInt32($1) }
    }

    /// For literals in source, where a wrong length is a typo to stop on.
    public init(stringLiteral value: String) {
        guard let code = FourCC(value) else {
            preconditionFailure("an SMC code is four bytes, got \"\(value)\"")
        }
        self = code
    }

    public var description: String {
        let bytes = [24, 16, 8, 0].map { UInt8(truncatingIfNeeded: rawValue >> $0) }
        return String(decoding: bytes.map { (32...126).contains($0) ? $0 : UInt8(ascii: "?") }, as: UTF8.self)
    }

    public static let flt: FourCC = "flt "
    public static let ui8: FourCC = "ui8 "
    public static let ui16: FourCC = "ui16"
    public static let ui32: FourCC = "ui32"
    public static let fpe2: FourCC = "fpe2"
}

public enum SMCError: Error, Equatable, Sendable {
    /// No AppleSMC service: normal in a virtual machine, including CI's macOS runner.
    case unavailable
    /// `kIOReturnNotPrivileged` (0xe00002c1): the kernel wants root. Its own
    /// case because the root helper's whole reason to exist is this one error.
    case notPrivileged
    /// The SMC answered 0x84: this Mac has no such key.
    case keyNotFound(FourCC)
    /// IOKit succeeded but the SMC refused, with its own result byte.
    case smcRefused(key: FourCC, result: UInt8)
    /// Any other IOKit error, by its `kern_return_t`.
    case kernel(code: Int32)
    /// A typed decode asked of a value of another type.
    case wrongType(expected: FourCC, actual: FourCC)
    /// A value whose byte count does not fit its type.
    case wrongSize(type: FourCC, expected: Int, actual: Int)
    /// An encode asked for a number its type cannot hold.
    case outOfRange(type: FourCC, value: Double)
    /// A numeric decode of a type this library does not read.
    case unsupportedType(FourCC)

    static let notPrivilegedCode = Int32(bitPattern: 0xe000_02c1)
    static let notFoundCode = Int32(bitPattern: 0xe000_02f0)
    static let smcKeyNotFound: UInt8 = 0x84

    /// One IOKit return plus the SMC's own result byte, as a typed error, or
    /// nil when both say success.
    static func check(_ kr: Int32, smcResult: UInt8, key: FourCC) -> SMCError? {
        if kr == notPrivilegedCode { return .notPrivileged }
        if kr != 0 { return .kernel(code: kr) }
        if smcResult == smcKeyNotFound { return .keyNotFound(key) }
        if smcResult != 0 { return .smcRefused(key: key, result: smcResult) }
        return nil
    }
}

/// One key's value as the SMC holds it: its type and raw bytes. The typed
/// accessors decode; the static constructors encode.
public struct SMCValue: Equatable, Sendable {
    public let type: FourCC
    public let bytes: [UInt8]

    public init(type: FourCC, bytes: [UInt8]) {
        self.type = type
        self.bytes = bytes
    }

    private func expect(_ wanted: FourCC, size: Int) throws {
        guard type == wanted else { throw SMCError.wrongType(expected: wanted, actual: type) }
        guard bytes.count == size else {
            throw SMCError.wrongSize(type: type, expected: size, actual: bytes.count)
        }
    }

    /// `flt `: an IEEE single, LITTLE-endian (`00409c45` is 5000.0, measured).
    /// Every other numeric SMC type is big-endian.
    public func floatValue() throws -> Float {
        try expect(.flt, size: 4)
        let raw = bytes.reversed().reduce(UInt32(0)) { $0 << 8 | UInt32($1) }
        return Float(bitPattern: raw)
    }

    public func uint8Value() throws -> UInt8 {
        try expect(.ui8, size: 1)
        return bytes[0]
    }

    /// `ui16`, big-endian.
    public func uint16Value() throws -> UInt16 {
        try expect(.ui16, size: 2)
        return UInt16(bytes[0]) << 8 | UInt16(bytes[1])
    }

    /// `ui32`, big-endian. The type of `#KEY`, the key count.
    public func uint32Value() throws -> UInt32 {
        try expect(.ui32, size: 4)
        return bytes.reduce(UInt32(0)) { $0 << 8 | UInt32($1) }
    }

    /// `fpe2`: unsigned big-endian fixed point, 14 integer bits and 2
    /// fraction bits, the fan-speed type on Intel Macs.
    public func fpe2Value() throws -> Double {
        try expect(.fpe2, size: 2)
        return Double(UInt16(bytes[0]) << 8 | UInt16(bytes[1])) / 4
    }

    /// Whichever of the four numeric types this value is, as a Double. A fan
    /// speed is `flt ` on Apple Silicon and `fpe2` on Intel; callers that only
    /// want the number read it here.
    public func numericValue() throws -> Double {
        switch type {
        case .flt: return Double(try floatValue())
        case .ui8: return Double(try uint8Value())
        case .ui16: return Double(try uint16Value())
        case .fpe2: return try fpe2Value()
        default: throw SMCError.unsupportedType(type)
        }
    }

    public static func flt(_ value: Float) -> SMCValue {
        let raw = value.bitPattern
        return SMCValue(type: .flt, bytes: [0, 8, 16, 24].map { UInt8(truncatingIfNeeded: raw >> $0) })
    }

    public static func ui8(_ value: UInt8) -> SMCValue {
        SMCValue(type: .ui8, bytes: [value])
    }

    public static func ui16(_ value: UInt16) -> SMCValue {
        SMCValue(type: .ui16, bytes: [UInt8(value >> 8), UInt8(truncatingIfNeeded: value)])
    }

    /// Rounded to the nearest quarter; throws outside 0 to 16,383.75.
    public static func fpe2(_ value: Double) throws -> SMCValue {
        let quarters = (value * 4).rounded()
        guard quarters.isFinite, quarters >= 0, quarters <= Double(UInt16.max) else {
            throw SMCError.outOfRange(type: .fpe2, value: value)
        }
        let raw = UInt16(quarters)
        return SMCValue(type: .fpe2, bytes: [UInt8(raw >> 8), UInt8(truncatingIfNeeded: raw)])
    }
}

/// One open AppleSMC connection. Open once and keep it: the connection is
/// closed when this object goes away.
///
/// Not safe to share across threads by itself; ``HeatReading/read()`` guards
/// its own shared one with a lock.
public final class SMCConnection {
    private let handle: UInt32

    /// Throws ``SMCError/unavailable`` when the Mac has no AppleSMC (a VM).
    public init() throws {
        var handle: UInt32 = 0
        let kr = csmc_open(&handle)
        if kr == SMCError.notFoundCode { throw SMCError.unavailable }
        if kr == SMCError.notPrivilegedCode { throw SMCError.notPrivileged }
        guard kr == 0 else { throw SMCError.kernel(code: kr) }
        self.handle = handle
    }

    deinit {
        // A close that fails leaves nothing to undo: the port dies with the
        // process either way, and deinit has nowhere to report to.
        csmc_close(handle)
    }

    public func read(_ key: FourCC) throws -> SMCValue {
        var type: UInt32 = 0
        var size: UInt32 = 0
        var smcResult: UInt8 = 0
        var buffer = [UInt8](repeating: 0, count: 32)
        let kr = buffer.withUnsafeMutableBufferPointer { raw in
            csmc_read_key(handle, key.rawValue, &type, &size, raw.baseAddress, &smcResult)
        }
        if let error = SMCError.check(kr, smcResult: smcResult, key: key) { throw error }
        return SMCValue(type: FourCC(rawValue: type), bytes: Array(buffer.prefix(min(Int(size), 32))))
    }

    /// Needs root: a non-root caller gets ``SMCError/notPrivileged``.
    public func write(_ key: FourCC, _ value: SMCValue) throws {
        guard value.bytes.count <= 32 else {
            throw SMCError.wrongSize(type: value.type, expected: 32, actual: value.bytes.count)
        }
        var smcResult: UInt8 = 0
        let kr = value.bytes.withUnsafeBufferPointer { raw in
            csmc_write_key(
                handle, key.rawValue, value.type.rawValue, UInt32(raw.count), raw.baseAddress,
                &smcResult)
        }
        if let error = SMCError.check(kr, smcResult: smcResult, key: key) { throw error }
    }

    /// Every key name the SMC reports, in its own order. 3,462 on a Mac16,7:
    /// thousands of kernel calls, so callers enumerate once and keep the list.
    public func keys() throws -> [FourCC] {
        let count = try read("#KEY").uint32Value()
        var keys: [FourCC] = []
        keys.reserveCapacity(Int(count))
        for index in 0..<count {
            var raw: UInt32 = 0
            var smcResult: UInt8 = 0
            let kr = csmc_key_at_index(handle, index, &raw, &smcResult)
            if let error = SMCError.check(kr, smcResult: smcResult, key: "#KEY") { throw error }
            keys.append(FourCC(rawValue: raw))
        }
        return keys
    }
}

/// One fan, as read.
public struct FanReading: Equatable, Sendable {
    /// 0-based, the `n` in `F{n}Ac`.
    public let index: Int
    public let actualRPM: Double
    public let minRPM: Double
    public let maxRPM: Double
    /// The fan's mode key reads 1 (forced): a program fixed its speed, so it
    /// no longer follows heat. 0 and 3 are both automatic (Stats' `FanMode`).
    public let isManual: Bool

    public init(index: Int, actualRPM: Double, minRPM: Double, maxRPM: Double, isManual: Bool) {
        self.index = index
        self.actualRPM = actualRPM
        self.minRPM = minRPM
        self.maxRPM = maxRPM
        self.isManual = isManual
    }
}

/// How hot the CPU is and how hard the fans work.
public struct HeatReading: Equatable, Sendable {
    /// The hottest CPU sensor, °C. Nil when this Mac has neither `TCMz` nor
    /// any readable `Tp*` sensor.
    public let cpuCelsius: Double?
    /// Empty on a Mac with no fans (`FNum` 0, a MacBook Air).
    public let fans: [FanReading]

    public init(cpuCelsius: Double?, fans: [FanReading]) {
        self.cpuCelsius = cpuCelsius
        self.fans = fans
    }

    /// Read now. Nil when AppleSMC is absent or unreadable, which is normal in
    /// a VM, or when it yields neither a temperature nor a fan.
    public static func read() -> HeatReading? {
        guard let reader = HeatReader.shared else { return nil }
        return try? reader.read()
    }
}

/// The process-wide reader behind ``HeatReading/read()``: one connection, and
/// the two per-Mac facts (where the CPU temperature lives, which spelling the
/// fan mode key uses) found once and kept.
final class HeatReader: @unchecked Sendable {
    /// Where the CPU temperature comes from on this Mac.
    enum CPUSource: Equatable {
        /// One key that already holds the CPU maximum (`TCMz`).
        case key(FourCC)
        /// The hottest of these `Tp*` sensors, enumerated once.
        case hottest([FourCC])
    }

    static let cpuMaxKey: FourCC = "TCMz"

    /// Nil when AppleSMC cannot be opened; then it stays nil for the life of
    /// the process, because nothing about a VM changes between polls.
    static let shared: HeatReader? = {
        guard let connection = try? SMCConnection() else { return nil }
        return HeatReader(connection: connection)
    }()

    private let connection: SMCConnection
    private let lock = NSLock()
    private var cpuSource: CPUSource?
    private var modeKeyIsLower: Bool?

    init(connection: SMCConnection, cpuSource: CPUSource? = nil) {
        self.connection = connection
        self.cpuSource = cpuSource
    }

    func read() throws -> HeatReading? {
        lock.lock()
        defer { lock.unlock() }
        let cpu = try readCPU(source: try resolvedCPUSource())
        let fans = try readFans()
        if cpu == nil && fans.isEmpty { return nil }
        return HeatReading(cpuCelsius: cpu, fans: fans)
    }

    /// `TCMz` when this Mac has it; otherwise the `Tp*` sensors, enumerated
    /// once per process. 3,462 keys on every poll is not a price to pay for
    /// one number. An enumeration that fails is kept as an empty list rather
    /// than retried each poll, for the same reason.
    private func resolvedCPUSource() throws -> CPUSource {
        if let cpuSource { return cpuSource }
        let source: CPUSource
        do {
            _ = try connection.read(Self.cpuMaxKey).floatValue()
            source = .key(Self.cpuMaxKey)
        } catch SMCError.keyNotFound {
            source = .hottest((try? Self.processorSensorKeys(connection)) ?? [])
        }
        cpuSource = source
        return source
    }

    /// Every `Tp*` key holding a 4-byte `flt `: the processor sensors (102 on
    /// a Mac16,7).
    static func processorSensorKeys(_ connection: SMCConnection) throws -> [FourCC] {
        try connection.keys().filter { key in
            guard key.rawValue >> 16 == UInt32(UInt8(ascii: "T")) << 8 | UInt32(UInt8(ascii: "p"))
            else { return false }
            guard let value = try? connection.read(key) else { return false }
            return value.type == .flt && value.bytes.count == 4
        }
    }

    /// A sensor that reads 0 or below, or past 150 °C, is one not fitted or
    /// not powered, not a hot CPU; the maximum skips it.
    static let plausibleCelsius: ClosedRange<Double> = 0.5...150

    func readCPU(source: CPUSource) throws -> Double? {
        switch source {
        case .key(let key):
            return Double(try connection.read(key).floatValue())
        case .hottest(let keys):
            var hottest: Double?
            for key in keys {
                guard let value = try? connection.read(key).floatValue() else { continue }
                let celsius = Double(value)
                guard Self.plausibleCelsius.contains(celsius) else { continue }
                hottest = max(hottest ?? celsius, celsius)
            }
            return hottest
        }
    }

    /// `F{n}md` where it exists, else `F{n}Md`: some Apple Silicon models
    /// spell the mode key in lower case. Stats probes `F0md` the same way.
    private func modeKey(_ fan: Int) throws -> FourCC {
        if modeKeyIsLower == nil {
            do {
                _ = try connection.read("F0md")
                modeKeyIsLower = true
            } catch SMCError.keyNotFound {
                modeKeyIsLower = false
            }
        }
        return Self.fanKey(fan, modeKeyIsLower == true ? "md" : "Md")
    }

    /// `F{n}{suffix}`. Four bytes for every `fan` in 0...9, which is the only
    /// range ``readFans()`` asks for; anything else is a bug and stops here.
    static func fanKey(_ fan: Int, _ suffix: String) -> FourCC {
        FourCC(stringLiteral: "F\(fan)\(suffix)")
    }

    private func readFans() throws -> [FanReading] {
        let count: Int
        do {
            count = Int(try connection.read("FNum").uint8Value())
        } catch SMCError.keyNotFound {
            return []
        }
        // Ten fans is the most a fan key's single digit can name.
        return try (0..<min(count, 10)).map { fan in
            func number(_ suffix: String) throws -> Double {
                try connection.read(Self.fanKey(fan, suffix)).numericValue()
            }
            let isManual: Bool
            do {
                isManual = try connection.read(try modeKey(fan)).uint8Value() == 1
            } catch SMCError.keyNotFound {
                // No mode key at all (older Intel Macs keep the bit in `FS! `):
                // nothing says another app holds this fan.
                isManual = false
            }
            return FanReading(
                index: fan, actualRPM: try number("Ac"), minRPM: try number("Mn"),
                maxRPM: try number("Mx"), isManual: isManual)
        }
    }
}
