import TcrSMC
import XCTest

@testable import TcrBarCore

/// The machine line's two testable halves: the sentence it draws, and the
/// band it tints. ``MachineStats/read()`` itself is not tested — it reads this
/// machine, and a test that asserts anything about this machine's load average
/// asserts about the CI runner instead.
final class MachineStatsTests: XCTestCase {
    private func stats(
        load: Double = 1.0,
        cores: Int = 14,
        used: UInt64 = 48 * 1_073_741_824,
        total: UInt64 = 64 * 1_073_741_824,
        compiles: Int = 5,
        free: UInt64 = 210_000_000_000
    ) -> MachineStats {
        MachineStats(
            loadAverage: load, cores: cores, memoryUsedBytes: used, memoryTotalBytes: total,
            compiles: compiles, diskFreeBytes: free)
    }

    /// `docs/design/tools-tab.md`'s machine line, verbatim.
    func testLineMatchesTheMockup() {
        XCTAssertEqual(
            stats(load: 7.1).line,
            "load 7.1/14 · 48/64 GB · 5 compiles · 210 GB free")
    }

    /// The clause a view tints and the rest of the line join back into the
    /// whole line — the two halves exist so the tint can stop at the load,
    /// not so the sentence can differ from itself.
    func testClausesComposeTheLine() {
        let machine = stats(load: 7.1)
        XCTAssertEqual("\(machine.loadClause) · \(machine.restClause)", machine.line)
    }

    func testOneCompileIsSingular() {
        XCTAssertTrue(stats(compiles: 1).line.contains("· 1 compile ·"))
        XCTAssertTrue(stats(compiles: 0).line.contains("· 0 compiles ·"))
    }

    /// Memory reads in binary GB (a Mac's own "64 GB of RAM"), disk in decimal
    /// GB (the disk's own capacity, and Finder's). Both rounded to whole
    /// units: a tenth of a gigabyte changes no decision.
    func testUnitsAreTheOnesEachFigureIsQuotedIn() {
        let machine = stats(
            used: 17_179_869_184,  // 16 GiB
            total: 68_719_476_736,  // 64 GiB
            free: 210_400_000_000)  // 210 GB
        XCTAssertTrue(machine.line.contains("16/64 GB"), machine.line)
        XCTAssertTrue(machine.line.contains("210 GB free"), machine.line)
    }

    /// Gil's own dispatch rule: below one load unit per core says nothing,
    /// up to twice cores is amber, past twice cores is red and means one lane
    /// rather than three.
    func testLoadTintBands() {
        XCTAssertEqual(stats(load: 13.9, cores: 14).loadTint, .calm)
        XCTAssertEqual(stats(load: 14.0, cores: 14).loadTint, .busy)
        XCTAssertEqual(stats(load: 28.0, cores: 14).loadTint, .busy)
        XCTAssertEqual(stats(load: 28.1, cores: 14).loadTint, .overloaded)
    }

    /// A core count of zero is a failed read, not an infinitely overloaded
    /// box: dividing by it must not colour the line red.
    func testZeroCoresIsCalmNotRed() {
        XCTAssertEqual(stats(load: 7.1, cores: 0).loadTint, .calm)
    }

    // MARK: - heat (`docs/design/fan-readout-mockup.html`, panels 2 to 6)

    private func fan(_ index: Int, _ actual: Double, max: Double = 5_777, manual: Bool = false)
        -> FanReading
    {
        FanReading(index: index, actualRPM: actual, minRPM: 1_350, maxRPM: max, isManual: manual)
    }

    private func heated(_ heat: HeatReading?) -> MachineStats {
        MachineStats(
            loadAverage: 7.1, cores: 14, memoryUsedBytes: 48 * 1_073_741_824,
            memoryTotalBytes: 64 * 1_073_741_824, compiles: 5, diskFreeBytes: 210_000_000_000,
            heat: heat)
    }

    /// Whole degrees, and the BUSIER fan's share of its own maximum: 5,013 of
    /// 5,777 is 86.8 %, which reads 87. The quieter fan (4,976, 86.1 %) would
    /// read 86, so this also pins which fan the figure comes from.
    func testHeatClauseUsesTheBusierFan() {
        let machine = heated(HeatReading(cpuCelsius: 93.3, fans: [fan(0, 4_976), fan(1, 5_013)]))
        XCTAssertEqual(machine.heatClause, "CPU 93°C · fans 87%")
    }

    /// Two fans with different maximums: the share is each fan against its
    /// own, so a slower fan with a lower ceiling can be the busier one.
    func testBusierIsByShareNotByRPM() {
        let machine = heated(
            HeatReading(cpuCelsius: 50, fans: [fan(0, 3_000, max: 6_000), fan(1, 2_700, max: 3_000)]))
        XCTAssertEqual(machine.heatClause, "CPU 50°C · fans 90%")
    }

    /// Any fan whose mode key says manual: another app holds the fans.
    func testManualSuffix() {
        let machine = heated(
            HeatReading(
                cpuCelsius: 93.3, fans: [fan(0, 5_013, manual: true), fan(1, 4_976, manual: false)]))
        XCTAssertEqual(machine.heatClause, "CPU 93°C · fans 87% manual")
    }

    func testFansOffWhenEveryFanReadsZero() {
        let machine = heated(HeatReading(cpuCelsius: 46.2, fans: [fan(0, 0), fan(1, 0)]))
        XCTAssertEqual(machine.heatClause, "CPU 46°C · fans off")
    }

    /// One fan spinning is not "off".
    func testOneFanSpinningIsNotOff() {
        let machine = heated(HeatReading(cpuCelsius: 46, fans: [fan(0, 0), fan(1, 1_444)]))
        XCTAssertEqual(machine.heatClause, "CPU 46°C · fans 25%")
    }

    /// A MacBook Air reports zero fans: the fan part goes, heat stays.
    func testNoFansDropsTheFanPart() {
        XCTAssertEqual(heated(HeatReading(cpuCelsius: 61, fans: [])).heatClause, "CPU 61°C")
    }

    /// No reading at all: no clause, and the line is exactly today's line.
    func testNoReadingIsTodaysLine() {
        let machine = heated(nil)
        XCTAssertNil(machine.heatClause)
        XCTAssertNil(machine.heatDetail)
        XCTAssertEqual(machine.line, "load 7.1/14 · 48/64 GB · 5 compiles · 210 GB free")
    }

    /// The clause sits right after the load in the whole line.
    func testHeatSitsAfterTheLoad() {
        let machine = heated(HeatReading(cpuCelsius: 93.3, fans: [fan(0, 5_013)]))
        XCTAssertEqual(
            machine.line, "load 7.1/14 · CPU 93°C · fans 87% · 48/64 GB · 5 compiles · 210 GB free")
    }

    /// The tooltip of mockup panel 3: one decimal on the CPU, each fan's own
    /// figures, and what manual means.
    func testHeatDetailIsPanelThreesTooltip() {
        let machine = heated(
            HeatReading(
                cpuCelsius: 93.3, fans: [fan(0, 5_013, manual: true), fan(1, 4_976, manual: true)]))
        XCTAssertEqual(
            machine.heatDetail,
            """
            Hottest CPU sensor 93.3 °C
            Fan 1: 5,013 of 5,777 rpm
            Fan 2: 4,976 of 5,777 rpm
            Manual: another app set these speeds, so they do not follow the heat.
            """)
    }

    func testGroupedRPM() {
        XCTAssertEqual(MachineStats.grouped(0), "0")
        XCTAssertEqual(MachineStats.grouped(999), "999")
        XCTAssertEqual(MachineStats.grouped(5_013.4), "5,013")
        XCTAssertEqual(MachineStats.grouped(1_234_567), "1,234,567")
        XCTAssertNil(MachineStats.grouped(.nan))
    }

    /// A `HeatReading` anyone builds can carry NaN or an infinity; the clause,
    /// the phrases and the tooltip leave that figure out and never trap.
    func testNonFiniteFiguresAreLeftOutNeverTrap() {
        let machine = heated(
            HeatReading(
                cpuCelsius: .nan,
                fans: [fan(0, .infinity), fan(1, 2_000, max: .nan), fan(2, .nan, manual: true)]))
        XCTAssertNil(machine.heatClause)
        XCTAssertEqual(machine.heatPhrases, [])
        XCTAssertNil(machine.heatDetail)

        let cpuOnly = heated(HeatReading(cpuCelsius: .infinity, fans: [fan(0, 2_888)]))
        XCTAssertEqual(cpuOnly.heatClause, "fans 50%")
        XCTAssertEqual(cpuOnly.heatDetail, "Fan 1: 2,888 of 5,777 rpm")
    }

    /// The CPU and the fans are two phrases, so the panel can wrap between them.
    func testHeatPhrasesAreSeparate() {
        let machine = heated(HeatReading(cpuCelsius: 93.3, fans: [fan(0, 5_013, manual: true)]))
        XCTAssertEqual(machine.heatPhrases, ["CPU 93°C", "fans 87% manual"])
        XCTAssertEqual(heated(HeatReading(cpuCelsius: 61, fans: [])).heatPhrases, ["CPU 61°C"])
    }

    /// Manual is the news even when no fan has a maximum to share against.
    func testManualSurvivesWithNoShare() {
        let machine = heated(
            HeatReading(cpuCelsius: 50, fans: [fan(0, 2_000, max: 0, manual: true)]))
        XCTAssertEqual(machine.heatClause, "CPU 50°C · fans manual")
        XCTAssertEqual(
            machine.heatDetail,
            """
            Hottest CPU sensor 50.0 °C
            Fan 1: 2,000 rpm
            Manual: another app set these speeds, so they do not follow the heat.
            """)
        // Without manual there is nothing to say about the fans.
        XCTAssertEqual(
            heated(HeatReading(cpuCelsius: 50, fans: [fan(0, 2_000, max: 0)])).heatClause, "CPU 50°C")
    }
}
