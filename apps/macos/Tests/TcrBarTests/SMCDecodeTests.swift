import XCTest

@testable import TcrSMC

/// The SMC value codecs, against bytes read off a real Mac (a Mac16,7 on
/// 2026-10-18), and the one test that touches hardware, which skips by name
/// where there is no AppleSMC to read: CI's macOS runner is a VM.
final class SMCDecodeTests: XCTestCase {
    private func hex(_ text: String) -> [UInt8] {
        stride(from: 0, to: text.count, by: 2).map { offset in
            let start = text.index(text.startIndex, offsetBy: offset)
            return UInt8(text[start..<text.index(start, offsetBy: 2)], radix: 16) ?? 0
        }
    }

    /// `F0Tg`, `F0Mx` and `F0Ac` as read: `flt ` is little-endian.
    func testFloatDecodesTheMeasuredBytes() throws {
        XCTAssertEqual(try SMCValue(type: .flt, bytes: hex("00409c45")).floatValue(), 5000.0)
        XCTAssertEqual(try SMCValue(type: .flt, bytes: hex("0088b445")).floatValue(), 5777.0)
        XCTAssertEqual(
            try SMCValue(type: .flt, bytes: hex("73319d45")).floatValue(), 5030.2, accuracy: 0.05)
    }

    func testFloatEncodeIsTheInverse() throws {
        XCTAssertEqual(SMCValue.flt(5000).bytes, hex("00409c45"))
        XCTAssertEqual(SMCValue.flt(5777).bytes, hex("0088b445"))
        XCTAssertEqual(try SMCValue.flt(5030.2).floatValue(), 5030.2)
    }

    func testIntegerTypesAreBigEndian() throws {
        XCTAssertEqual(try SMCValue(type: .ui8, bytes: [2]).uint8Value(), 2)
        XCTAssertEqual(try SMCValue(type: .ui16, bytes: [0x01, 0x02]).uint16Value(), 0x0102)
        XCTAssertEqual(SMCValue.ui16(0x0102).bytes, [0x01, 0x02])
        XCTAssertEqual(SMCValue.ui8(1).bytes, [1])
        // `#KEY` on the measured Mac: 3,462 keys.
        XCTAssertEqual(try SMCValue(type: .ui32, bytes: [0, 0, 0x0d, 0x86]).uint32Value(), 3462)
    }

    /// `fpe2`, Intel's fan-speed type: 14.2 fixed point, big-endian. 5000 rpm
    /// is 20000 quarters, 0x4e20.
    func testFpe2RoundTrips() throws {
        XCTAssertEqual(try SMCValue(type: .fpe2, bytes: [0x4e, 0x20]).fpe2Value(), 5000)
        XCTAssertEqual(try SMCValue.fpe2(5000).bytes, [0x4e, 0x20])
        XCTAssertEqual(try SMCValue.fpe2(1350.25).fpe2Value(), 1350.25)
        XCTAssertThrowsError(try SMCValue.fpe2(16_384)) { error in
            XCTAssertEqual(error as? SMCError, .outOfRange(type: .fpe2, value: 16_384))
        }
        XCTAssertThrowsError(try SMCValue.fpe2(-1))
    }

    /// A fan speed is `flt ` on Apple Silicon and `fpe2` on Intel; the number
    /// is the same either way.
    func testNumericValueReadsEitherFanType() throws {
        XCTAssertEqual(try SMCValue(type: .flt, bytes: hex("00409c45")).numericValue(), 5000)
        XCTAssertEqual(try SMCValue(type: .fpe2, bytes: [0x4e, 0x20]).numericValue(), 5000)
        XCTAssertThrowsError(try SMCValue(type: "sp78", bytes: [0x40, 0]).numericValue()) { error in
            XCTAssertEqual(error as? SMCError, .unsupportedType("sp78"))
        }
    }

    /// A decode of the wrong type or size is an error, never a number read
    /// from bytes that mean something else.
    func testWrongTypeAndSizeThrow() {
        XCTAssertThrowsError(try SMCValue(type: .ui8, bytes: [1]).floatValue()) { error in
            XCTAssertEqual(error as? SMCError, .wrongType(expected: .flt, actual: .ui8))
        }
        XCTAssertThrowsError(try SMCValue(type: .flt, bytes: [0, 0]).floatValue()) { error in
            XCTAssertEqual(error as? SMCError, .wrongSize(type: .flt, expected: 4, actual: 2))
        }
    }

    /// Through a `String` parameter on purpose: `FourCC("flt")` with a
    /// literal argument picks the literal initializer, which stops the process
    /// on a wrong length instead of returning nil.
    private func parse(_ text: String) -> FourCC? { FourCC(text) }

    func testFourCCRoundTripsAndRejectsWrongLengths() {
        XCTAssertEqual(parse("TCMz")?.description, "TCMz")
        XCTAssertEqual(parse("flt ")?.rawValue, 0x666c_7420)
        XCTAssertNil(parse("flt"))
        XCTAssertNil(parse("F10Ac"))
        XCTAssertEqual(HeatReader.fanKey(1, "md"), "F1md")
    }

    /// 0xe00002c1, the non-root write on 2026-10-18, is its own case and never
    /// a generic kernel error: the root helper exists because of this one.
    func testNotPrivilegedIsItsOwnCase() {
        XCTAssertEqual(
            SMCError.check(Int32(bitPattern: 0xe000_02c1), smcResult: 0, key: "F0Tg"), .notPrivileged)
        XCTAssertEqual(SMCError.check(0, smcResult: 0x84, key: "TCMz"), .keyNotFound("TCMz"))
        XCTAssertEqual(
            SMCError.check(0, smcResult: 0x85, key: "F0Tg"), .smcRefused(key: "F0Tg", result: 0x85))
        XCTAssertEqual(SMCError.check(5, smcResult: 0, key: "F0Tg"), .kernel(code: 5))
        XCTAssertNil(SMCError.check(0, smcResult: 0, key: "F0Tg"))
    }

    /// The one hardware read. Skips, by name, where there is no AppleSMC.
    func testHardwareReadIsPlausibleWhereThereIsAnSMC() throws {
        guard let heat = HeatReading.read() else {
            throw XCTSkip("HeatReading.read() is nil: no readable AppleSMC here (a VM)")
        }
        if let cpu = heat.cpuCelsius {
            XCTAssertTrue(HeatReader.plausibleCelsius.contains(cpu), "\(cpu)")
        }
        for fan in heat.fans {
            XCTAssertGreaterThan(fan.maxRPM, 0, "fan \(fan.index)")
            XCTAssertGreaterThanOrEqual(fan.actualRPM, 0, "fan \(fan.index)")
        }
    }
}
