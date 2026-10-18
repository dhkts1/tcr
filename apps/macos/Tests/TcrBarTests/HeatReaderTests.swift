import XCTest

@testable import TcrSMC

/// ``HeatReader`` against a fake SMC: what one bad key may and may not take
/// with it, what is retried and what is kept.
final class HeatReaderTests: XCTestCase {
    /// An SMC made of a table. A key with no entry answers "no such key", as
    /// a real SMC does for a key this Mac lacks.
    private final class FakeSMC: SMCKeySource {
        var values: [FourCC: Result<SMCValue, SMCError>] = [:]
        var keyTable: Result<[FourCC], SMCError> = .success([])
        private(set) var reads: [FourCC] = []
        private(set) var keysCalls = 0

        func read(_ key: FourCC) throws -> SMCValue {
            reads.append(key)
            guard let value = values[key] else { throw SMCError.keyNotFound(key) }
            return try value.get()
        }

        func keys() throws -> [FourCC] {
            keysCalls += 1
            return try keyTable.get()
        }

        func readCount(_ key: FourCC) -> Int { reads.filter { $0 == key }.count }
    }

    /// A `flt ` built from raw bits, so NaN and negatives can be served: the
    /// throwing ``SMCValue/flt(_:)`` refuses them, and an SMC need not.
    private func flt(_ value: Float) -> Result<SMCValue, SMCError> {
        let raw = value.bitPattern
        return .success(
            SMCValue(type: .flt, bytes: [0, 8, 16, 24].map { UInt8(truncatingIfNeeded: raw >> $0) }))
    }

    private func oneFan(_ smc: FakeSMC, actual: Float = 2000) {
        smc.values["FNum"] = .success(.ui8(1))
        smc.values["F0Ac"] = flt(actual)
        smc.values["F0Mn"] = flt(1000)
        smc.values["F0Mx"] = flt(5000)
    }

    private var clock: TimeInterval = 0
    private var reported: [String] = []

    private func reader(_ smc: FakeSMC) -> HeatReader {
        HeatReader(
            smc: smc, now: { [unowned self] in clock },
            report: { [unowned self] in reported.append($0) })
    }

    /// Finding 5: a CPU key that fails leaves the fans, and it is logged.
    func testAFailingCPUKeyLeavesTheFans() {
        let smc = FakeSMC()
        smc.values["TCMz"] = .failure(.smcRefused(key: "TCMz", result: 0x85))
        oneFan(smc)
        let heat = reader(smc).read()
        XCTAssertNil(heat.cpuCelsius)
        XCTAssertEqual(heat.fans.map(\.actualRPM), [2000])
        XCTAssertEqual(reported.count, 1)
        XCTAssertTrue(reported.first?.contains("TCMz") == true, "\(reported)")
    }

    /// Finding 5 and 3: a fan key that fails leaves the CPU, and is logged
    /// once however many polls it keeps failing.
    func testAFailingFanKeyLeavesTheCPUAndIsLoggedOnce() {
        let smc = FakeSMC()
        smc.values["TCMz"] = flt(61)
        oneFan(smc)
        smc.values["F0Ac"] = .failure(.kernel(code: 5))
        let reader = reader(smc)
        for _ in 0..<3 {
            let heat = reader.read()
            XCTAssertEqual(heat.cpuCelsius, 61)
            XCTAssertEqual(heat.fans, [])
        }
        XCTAssertEqual(reported.count, 1, "\(reported)")
    }

    /// Finding 5: a `TCMz` that is not a 4-byte `flt ` is not the CPU key;
    /// the hottest `Tp*` sensor is.
    func testATCMzOfAnotherTypeFallsBackToTheSensors() {
        let smc = FakeSMC()
        smc.values["TCMz"] = .success(.ui8(3))
        smc.keyTable = .success(["Tp01", "TA0P", "Tp02"])
        smc.values["Tp01"] = flt(50)
        smc.values["Tp02"] = flt(70)
        smc.values["TA0P"] = flt(90)
        XCTAssertEqual(reader(smc).read().cpuCelsius, 70)
        XCTAssertEqual(reported, [])
    }

    /// Finding 4: a failed enumeration is logged, not kept, and not retried
    /// before the delay is up.
    func testAFailedEnumerationIsRetriedAfterTheDelay() {
        let smc = FakeSMC()
        smc.keyTable = .failure(.kernel(code: 5))
        let reader = reader(smc)
        XCTAssertNil(reader.read().cpuCelsius)
        XCTAssertEqual(smc.keysCalls, 1)
        XCTAssertEqual(reported.count, 1)

        clock = HeatReader.enumerationRetryDelay - 1
        XCTAssertNil(reader.read().cpuCelsius)
        XCTAssertEqual(smc.keysCalls, 1, "retried before the delay")

        clock = HeatReader.enumerationRetryDelay
        smc.keyTable = .success(["Tp01"])
        smc.values["Tp01"] = flt(55)
        XCTAssertEqual(reader.read().cpuCelsius, 55)
        XCTAssertEqual(smc.keysCalls, 2)

        // Once it has worked, it is kept.
        clock += 1000
        _ = reader.read()
        XCTAssertEqual(smc.keysCalls, 2)
    }

    /// Finding 4: a `Tp*` key that fails once is read again next poll.
    func testATpKeyThatFailsOnceIsNotDropped() {
        let smc = FakeSMC()
        smc.keyTable = .success(["Tp01", "Tp02"])
        smc.values["Tp01"] = flt(50)
        smc.values["Tp02"] = .failure(.smcRefused(key: "Tp02", result: 0x85))
        let reader = reader(smc)
        XCTAssertEqual(reader.read().cpuCelsius, 50)
        smc.values["Tp02"] = flt(80)
        XCTAssertEqual(reader.read().cpuCelsius, 80)
    }

    /// Finding 6: NaN, infinities, negatives and out-of-band temperatures
    /// never leave the reader.
    func testNonFiniteAndOutOfBandReadingsAreDropped() {
        for bad: Float in [.nan, .infinity, 200, 0, -3] {
            let smc = FakeSMC()
            smc.values["TCMz"] = flt(bad)
            XCTAssertNil(reader(smc).read().cpuCelsius, "TCMz \(bad)")
        }
        let smc = FakeSMC()
        smc.keyTable = .success(["Tp01", "Tp02"])
        smc.values["Tp01"] = flt(.nan)
        smc.values["Tp02"] = flt(48)
        smc.values["FNum"] = .success(.ui8(3))
        for fan in 0..<3 {
            smc.values[HeatReader.fanKey(fan, "Mn")] = flt(1000)
            smc.values[HeatReader.fanKey(fan, "Mx")] = flt(5000)
        }
        smc.values["F0Ac"] = flt(-5)
        smc.values["F1Ac"] = flt(.infinity)
        smc.values["F2Ac"] = flt(1200)
        let heat = reader(smc).read()
        XCTAssertEqual(heat.cpuCelsius, 48)
        XCTAssertEqual(heat.fans.map(\.index), [2])
    }

    /// Finding 7: `Mn` and `Mx` are read once and kept; `Ac` every poll.
    func testFanLimitsAreReadOnce() {
        let smc = FakeSMC()
        oneFan(smc)
        let reader = reader(smc)
        for _ in 0..<3 { XCTAssertEqual(reader.read().fans.first?.maxRPM, 5000) }
        XCTAssertEqual(smc.readCount("F0Mn"), 1)
        XCTAssertEqual(smc.readCount("F0Mx"), 1)
        XCTAssertEqual(smc.readCount("F0Ac"), 3)
    }
}
