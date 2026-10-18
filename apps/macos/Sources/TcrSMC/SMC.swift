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

public enum SMCError: Error, Hashable, Sendable {
    /// No AppleSMC service: normal in a virtual machine, including CI's macOS runner.
    case unavailable
    /// `kIOReturnNotPrivileged` (0xe00002c1): the kernel wants root. Its own
    /// case because the root helper's whole reason to exist is this one error.
    case notPrivileged
    /// The SMC answered 0x84: this Mac has no such key.
    case keyNotFound(FourCC)
    /// IOKit succeeded but the SMC refused, with its own result byte.
    case smcRefused(key: FourCC, result: UInt8)
    /// The SMC refused to name the key at this index of its key table.
    case keyIndexRefused(index: UInt32, result: UInt8)
    /// Any other IOKit error, by its `kern_return_t`.
    case kernel(code: Int32)
    /// A typed decode asked of a value of another type, or a write of a value
    /// whose type is not the key's own.
    case wrongType(expected: FourCC, actual: FourCC)
    /// A value whose byte count does not fit its type, a write whose byte
    /// count is not the key's own, or a key the SMC says is wider than the 32
    /// bytes its protocol carries.
    case wrongSize(type: FourCC, expected: Int, actual: Int)
    /// An encode asked for a number its type cannot hold, or one no fan or
    /// sensor can mean: NaN, an infinity, a negative `flt `.
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

    /// The same for a key-table index call, which names no key: its refusal
    /// is ``keyIndexRefused(index:result:)``, never a "no such key".
    static func checkIndex(_ kr: Int32, smcResult: UInt8, index: UInt32) -> SMCError? {
        if kr == notPrivilegedCode { return .notPrivileged }
        if kr != 0 { return .kernel(code: kr) }
        if smcResult != 0 { return .keyIndexRefused(index: index, result: smcResult) }
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

    /// Throws for NaN, either infinity and any negative number: every `flt `
    /// this library writes is a fan speed, and none of those is one. The root
    /// helper sends what this returns straight to the SMC.
    public static func flt(_ value: Float) throws -> SMCValue {
        guard value.isFinite, value >= 0 else {
            throw SMCError.outOfRange(type: .flt, value: Double(value))
        }
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

    /// The most bytes one SMC call carries: the protocol struct's
    /// `uint8_t bytes[32]`.
    public static let maxValueSize = 32

    /// Throws ``SMCError/wrongSize(type:expected:actual:)`` when the SMC says
    /// the key is wider than ``maxValueSize``, rather than returning the first
    /// 32 bytes of a longer value as if they were all of it.
    public func read(_ key: FourCC) throws -> SMCValue {
        var type: UInt32 = 0
        var size: UInt32 = 0
        var smcResult: UInt8 = 0
        var buffer = [UInt8](repeating: 0, count: Self.maxValueSize)
        let kr = buffer.withUnsafeMutableBufferPointer { raw in
            csmc_read_key(handle, key.rawValue, &type, &size, raw.baseAddress, &smcResult)
        }
        if let error = SMCError.check(kr, smcResult: smcResult, key: key) { throw error }
        guard Int(size) <= Self.maxValueSize else {
            throw SMCError.wrongSize(
                type: FourCC(rawValue: type), expected: Self.maxValueSize, actual: Int(size))
        }
        return SMCValue(type: FourCC(rawValue: type), bytes: Array(buffer.prefix(Int(size))))
    }

    /// The key's own type and size, as the SMC declares them, without its value.
    public func keyInfo(_ key: FourCC) throws -> SMCKeyInfo {
        var type: UInt32 = 0
        var size: UInt32 = 0
        var smcResult: UInt8 = 0
        let kr = csmc_key_info(handle, key.rawValue, &type, &size, &smcResult)
        if let error = SMCError.check(kr, smcResult: smcResult, key: key) { throw error }
        return SMCKeyInfo(type: FourCC(rawValue: type), size: Int(size))
    }

    /// Needs root: a non-root caller gets ``SMCError/notPrivileged``.
    ///
    /// Reads the key's own type and size first and sends nothing unless
    /// `value` matches both (``checkWrite(_:fits:)``): the SMC takes whatever
    /// bytes it is handed, and a `ui8` written to a `flt ` key is a fan target
    /// of whatever those bytes happen to mean.
    public func write(_ key: FourCC, _ value: SMCValue) throws {
        try Self.checkWrite(value, fits: try keyInfo(key))
        var smcResult: UInt8 = 0
        let kr = value.bytes.withUnsafeBufferPointer { raw in
            csmc_write_key(
                handle, key.rawValue, value.type.rawValue, UInt32(raw.count), raw.baseAddress,
                &smcResult)
        }
        if let error = SMCError.check(kr, smcResult: smcResult, key: key) { throw error }
    }

    /// The check ``write(_:_:)`` makes before it sends anything: the type is
    /// the key's own and the byte count is the key's own. An empty value is a
    /// size mismatch whatever the key says.
    static func checkWrite(_ value: SMCValue, fits info: SMCKeyInfo) throws {
        guard value.type == info.type else {
            throw SMCError.wrongType(expected: info.type, actual: value.type)
        }
        guard !value.bytes.isEmpty, value.bytes.count == info.size,
            value.bytes.count <= maxValueSize
        else {
            throw SMCError.wrongSize(type: info.type, expected: info.size, actual: value.bytes.count)
        }
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
            if let error = SMCError.checkIndex(kr, smcResult: smcResult, index: index) { throw error }
            keys.append(FourCC(rawValue: raw))
        }
        return keys
    }
}

/// A key's declared type and size.
public struct SMCKeyInfo: Equatable, Sendable {
    public let type: FourCC
    public let size: Int

    public init(type: FourCC, size: Int) {
        self.type = type
        self.size = size
    }
}

/// What ``HeatReader`` needs from an SMC: one key's value, and the key table.
/// ``SMCConnection`` is the real one; tests hand in a fake.
protocol SMCKeySource: AnyObject {
    func read(_ key: FourCC) throws -> SMCValue
    func keys() throws -> [FourCC]
}

extension SMCConnection: SMCKeySource {}

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
    /// The hottest CPU sensor, °C, always within ``HeatReader/plausibleCelsius``.
    /// Nil when this Mac has neither `TCMz` nor any readable `Tp*` sensor, or
    /// when none of them read this time.
    public let cpuCelsius: Double?
    /// Empty on a Mac with no fans (`FNum` 0, a MacBook Air), and missing any
    /// fan whose speed did not read.
    public let fans: [FanReading]

    public init(cpuCelsius: Double?, fans: [FanReading]) {
        self.cpuCelsius = cpuCelsius
        self.fans = fans
    }

    /// Read now. Nil means one thing: this Mac has no AppleSMC
    /// (``SMCError/unavailable``, normal in a VM).
    ///
    /// Every other error is logged, once per distinct error, and the reading
    /// carries whatever did read: a bad fan key never removes the CPU, and the
    /// reverse. An SMC that opened and read nothing is an empty reading, not nil.
    public static func read() -> HeatReading? {
        switch HeatReader.shared {
        case .unavailable: return nil
        case .failed: return HeatReading(cpuCelsius: nil, fans: [])
        case .ready(let reader): return reader.read()
        }
    }
}

/// The process-wide reader behind ``HeatReading/read()``: one connection, and
/// the per-Mac facts (where the CPU temperature lives, which spelling the fan
/// mode key uses, each fan's limits) found once and kept.
final class HeatReader: @unchecked Sendable {
    enum Shared {
        case ready(HeatReader)
        /// No AppleSMC: a VM. Nothing about that changes between polls.
        case unavailable
        /// AppleSMC is there and would not open. Logged once, here.
        case failed(Error)
    }

    static let shared: Shared = {
        do {
            return .ready(HeatReader(smc: try SMCConnection()))
        } catch SMCError.unavailable {
            return .unavailable
        } catch {
            HeatReader.log("SMC: AppleSMC would not open: \(error)")
            return .failed(error)
        }
    }()

    static let cpuMaxKey: FourCC = "TCMz"

    /// How long a failed `Tp*` enumeration waits before the next try. A
    /// starting value, not a measured one: long enough that a key table that
    /// keeps failing does not cost 3,462 kernel calls every poll.
    static let enumerationRetryDelay: TimeInterval = 60

    /// A sensor that reads 0 or below, or past 150 °C, is one not fitted or
    /// not powered, not a hot CPU; the reading drops it, whichever key it came
    /// from. NaN is outside every range.
    static let plausibleCelsius: ClosedRange<Double> = 0.5...150

    private let smc: any SMCKeySource
    private let now: () -> TimeInterval
    private let report: (String) -> Void
    private let lock = NSLock()

    /// `TCMz` is a 4-byte `flt ` on this Mac (true), is not or is absent
    /// (false), or has not answered yet (nil).
    private var useMaxKey: Bool?
    /// The `Tp*` sensors, once an enumeration has succeeded.
    private var processorKeys: [FourCC]?
    private var enumerationFailedAt: TimeInterval?
    private var modeKeyIsLower: Bool?
    /// Each fan's `Mn` and `Mx`, kept once both read: they do not change.
    private var fanLimits: [Int: (min: Double, max: Double)] = [:]
    /// Every line already reported, so a key that fails on every poll is
    /// one log line, not one a poll.
    private var reported: Set<String> = []

    init(
        smc: any SMCKeySource,
        now: @escaping () -> TimeInterval = { ProcessInfo.processInfo.systemUptime },
        report: @escaping (String) -> Void = HeatReader.log
    ) {
        self.smc = smc
        self.now = now
        self.report = report
    }

    /// The app logs through `NSLog` throughout (`PanelSizeDelta.marker`'s
    /// doc-comment says why), and a menu-bar app's stderr goes nowhere.
    static func log(_ line: String) {
        NSLog("TcrBar: %@", line)
    }

    func read() -> HeatReading {
        lock.lock()
        defer { lock.unlock() }
        return HeatReading(cpuCelsius: readCPU(), fans: readFans())
    }

    private func note(_ what: String, _ error: Error) {
        let line = "SMC: \(what): \(error)"
        if reported.insert(line).inserted { report(line) }
    }

    // MARK: CPU

    /// `TCMz` when this Mac has it as a 4-byte `flt `; otherwise the hottest
    /// `Tp*` sensor.
    private func readCPU() -> Double? {
        if useMaxKey != false {
            do {
                let value = try smc.read(Self.cpuMaxKey)
                if value.type == .flt && value.bytes.count == 4 {
                    useMaxKey = true
                    return Self.plausible(Double(try value.floatValue()))
                }
                // Some other type or size: not the key this reader knows.
                useMaxKey = false
            } catch SMCError.keyNotFound {
                useMaxKey = false
            } catch {
                note("\(Self.cpuMaxKey) read", error)
                // A key that has read before failed this once: no figure this
                // poll. One never yet read: try the sensors, ask again next poll.
                if useMaxKey == true { return nil }
            }
        }
        guard let keys = resolvedProcessorKeys() else { return nil }
        var hottest: Double?
        for key in keys {
            let celsius: Double
            do {
                celsius = Double(try smc.read(key).floatValue())
            } catch {
                note("\(key) read", error)
                continue
            }
            guard let plausible = Self.plausible(celsius) else { continue }
            hottest = max(hottest ?? plausible, plausible)
        }
        return hottest
    }

    static func plausible(_ celsius: Double) -> Double? {
        plausibleCelsius.contains(celsius) ? celsius : nil
    }

    /// The `Tp*` sensors, enumerated once: 3,462 keys on every poll is not a
    /// price to pay for one number. A failed enumeration is NOT kept: it is
    /// logged and tried again no sooner than ``enumerationRetryDelay`` later.
    private func resolvedProcessorKeys() -> [FourCC]? {
        if let processorKeys { return processorKeys }
        if let failedAt = enumerationFailedAt, now() - failedAt < Self.enumerationRetryDelay {
            return nil
        }
        do {
            let keys = try processorSensorKeys()
            processorKeys = keys
            enumerationFailedAt = nil
            return keys
        } catch {
            note("key table enumeration (retrying in \(Int(Self.enumerationRetryDelay)) s)", error)
            enumerationFailedAt = now()
            return nil
        }
    }

    /// Every `Tp*` key holding a 4-byte `flt ` (102 on a Mac16,7). A key that
    /// will not read now is kept, logged, and read again on the next poll:
    /// one failure is not a reason to drop a sensor for the life of the process.
    private func processorSensorKeys() throws -> [FourCC] {
        let tp = UInt32(UInt8(ascii: "T")) << 8 | UInt32(UInt8(ascii: "p"))
        var kept: [FourCC] = []
        for key in try smc.keys() where key.rawValue >> 16 == tp {
            do {
                let value = try smc.read(key)
                if value.type == .flt && value.bytes.count == 4 { kept.append(key) }
            } catch {
                note("\(key) read", error)
                kept.append(key)
            }
        }
        return kept
    }

    // MARK: Fans

    /// `F{n}{suffix}`. Four bytes for every `fan` in 0...9, which is the only
    /// range ``readFans()`` asks for; anything else is a bug and stops here.
    static func fanKey(_ fan: Int, _ suffix: String) -> FourCC {
        FourCC(stringLiteral: "F\(fan)\(suffix)")
    }

    private func readFans() -> [FanReading] {
        let count: Int
        do {
            count = Int(try smc.read("FNum").uint8Value())
        } catch SMCError.keyNotFound {
            return []
        } catch {
            note("FNum read", error)
            return []
        }
        // Ten fans is the most a fan key's single digit can name.
        return (0..<min(count, 10)).compactMap(readFan)
    }

    /// Nil when the fan's speed does not read, or reads as no speed at all
    /// (not finite, or negative).
    private func readFan(_ fan: Int) -> FanReading? {
        guard let actual = rpm(fan, "Ac") else { return nil }
        let limits = self.limits(fan)
        return FanReading(
            index: fan, actualRPM: actual, minRPM: limits.min, maxRPM: limits.max,
            isManual: isManual(fan))
    }

    /// `Mn` and `Mx`, read once both read and then kept. One that does not
    /// read is 0 this poll (``FanReading/maxRPM`` 0 is "no maximum") and is
    /// asked again next poll.
    private func limits(_ fan: Int) -> (min: Double, max: Double) {
        if let known = fanLimits[fan] { return known }
        let low = rpm(fan, "Mn")
        let high = rpm(fan, "Mx")
        guard let low, let high else { return (low ?? 0, high ?? 0) }
        fanLimits[fan] = (low, high)
        return (low, high)
    }

    private func rpm(_ fan: Int, _ suffix: String) -> Double? {
        let key = Self.fanKey(fan, suffix)
        let value: Double
        do {
            value = try smc.read(key).numericValue()
        } catch {
            note("\(key) read", error)
            return nil
        }
        return value.isFinite && value >= 0 ? value : nil
    }

    /// The fan's mode key reads 1. No mode key at all (older Intel Macs keep
    /// the bit in `FS! `) means nothing says another app holds this fan.
    private func isManual(_ fan: Int) -> Bool {
        guard let suffix = modeKeySuffix() else { return false }
        let key = Self.fanKey(fan, suffix)
        do {
            return try smc.read(key).uint8Value() == 1
        } catch SMCError.keyNotFound {
            return false
        } catch {
            note("\(key) read", error)
            return false
        }
    }

    /// `md` where `F0md` exists, else `Md`: some Apple Silicon models spell
    /// the mode key in lower case. Stats probes `F0md` the same way. Nil, and
    /// asked again next poll, when the probe itself fails.
    private func modeKeySuffix() -> String? {
        if modeKeyIsLower == nil {
            do {
                _ = try smc.read("F0md")
                modeKeyIsLower = true
            } catch SMCError.keyNotFound {
                modeKeyIsLower = false
            } catch {
                note("F0md read", error)
                return nil
            }
        }
        return modeKeyIsLower == true ? "md" : "Md"
    }
}
