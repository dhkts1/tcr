import XCTest

@testable import TcrBarCore

/// The Tools tab's numbers block: two fixed rows, the machine on the first and
/// heat, errors and timeouts on the second, whatever is or is not known.
final class ToolsNumbersTests: XCTestCase {
    private func machine(heat: HeatReading? = nil) -> MachineStats {
        MachineStats(
            loadAverage: 7.1, cores: 14, memoryUsedBytes: 48 * 1_073_741_824,
            memoryTotalBytes: 64 * 1_073_741_824, compiles: 5, diskFreeBytes: 210_000_000_000,
            heat: heat)
    }

    private let hot = HeatReading(
        cpuCelsius: 93.3,
        fans: [FanReading(index: 0, actualRPM: 5_013, minRPM: 1_350, maxRPM: 5_777, isManual: true)])

    /// The mockup Gil picked on 2026-10-21: load and the rest on row 1, the
    /// heat phrases then errors then timeouts on row 2.
    func testTheUsualStateFillsBothRows() {
        let numbers = ToolsNumbers(machine: machine(heat: hot), errors: 3, timeouts: 44)
        XCTAssertEqual(
            numbers.rows,
            [
                [.load("load 7.1/14"), .plain("48/64 GB · 5 compiles · 210 GB free")],
                [.heat("CPU 93°C"), .heat("fans 87% manual"), .plain("3 err today"), .plain("44 timed out")],
            ])
    }

    /// Always two rows, so the cards below never move: a missing machine
    /// reading leaves row 1 EMPTY rather than pulling row 2 up into it.
    func testRowsNeverChangePlaces() {
        XCTAssertEqual(
            ToolsNumbers(machine: nil, errors: 0, timeouts: 0).rows,
            [[], [.plain("0 timed out")]])
        XCTAssertEqual(
            ToolsNumbers(machine: machine(), errors: 0, timeouts: 2).rows,
            [[.load("load 7.1/14"), .plain("48/64 GB · 5 compiles · 210 GB free")], [.plain("2 timed out")]])
        for numbers in [
            ToolsNumbers(machine: nil, errors: 0, timeouts: 0),
            ToolsNumbers(machine: machine(), errors: 9, timeouts: 0),
            ToolsNumbers(machine: machine(heat: hot), errors: 99, timeouts: 9_999),
        ] {
            XCTAssertEqual(numbers.rows.count, ToolsNumbers.rowCount)
        }
        XCTAssertEqual(ToolsNumbers.rowCount, 2)
    }

    /// Each row is cut to one line at the tail, so the whole block, and the
    /// fan detail behind the heat phrases, is the hover text.
    func testHelpCarriesBothRowsInFullThenTheFanDetail() {
        let numbers = ToolsNumbers(machine: machine(heat: hot), errors: 3, timeouts: 44)
        XCTAssertEqual(
            numbers.help,
            """
            load 7.1/14 · 48/64 GB · 5 compiles · 210 GB free
            CPU 93°C · fans 87% manual · 3 err today · 44 timed out

            Hottest CPU sensor 93.3 °C
            Fan 1: 5,013 of 5,777 rpm
            Manual: another app set these speeds, so they do not follow the heat.
            """)
        XCTAssertEqual(ToolsNumbers(machine: nil, errors: 1, timeouts: 0).help, "1 err today · 0 timed out")
    }
}
