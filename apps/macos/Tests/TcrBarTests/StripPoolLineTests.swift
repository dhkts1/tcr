import AppKit
import XCTest

@testable import TcrBarCore

/// 2026-01-01T00:00:00Z: the clock every canonical timestamp is written against.
private let fixtureNow = Date(timeIntervalSince1970: 1_767_225_600)

/// One canonical row from apps/macos/Tests/Fixtures/codex-status.json, with `overrides`
/// written over its JSON object (`NSNull()` writes a JSON null).
private func canonicalRow(_ name: String, _ overrides: [String: Any] = [:]) throws -> [String: Any] {
    let url = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TcrBarTests
        .deletingLastPathComponent()  // Tests
        .appendingPathComponent("Fixtures/codex-status.json")
    let rows = try XCTUnwrap(
        JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
    var row = try XCTUnwrap(rows.first { $0["name"] as? String == name }, "no canonical \(name)")
    for (key, value) in overrides { row[key] = value }
    return row
}

private func decodeFleet(_ rows: [[String: Any]]) throws -> Fleet {
    let fleet = try Fleet.decode(JSONSerialization.data(withJSONObject: rows))
    XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
    return fleet
}

/// The strip's pool rows: one headline row per provider in a fleet holding Codex
/// accounts, counted the way the menu bar counts, each counting down by the strip's own
/// rule over its own rows. A fleet with no Codex account keeps the one headline it has
/// always drawn.
///
/// Every row comes from the canonical fixture or is a renamed copy of canonical alice.
/// Account names are obviously fake: this repository is public.
final class StripPoolLineTests: XCTestCase {

    // MARK: - A fleet with no Codex account

    /// No pool rows at all: the strip is exactly the one built without them.
    func testAClaudeOnlyFleetKeepsMainsStrip() throws {
        for rows in [[try alice()], [try alice(), try frank()]] {
            let fleet = try decodeFleet(rows)
            XCTAssertNil(fleet.providerPools(now: fixtureNow))
            let strip = strip(fleet)
            XCTAssertNil(strip.pools)
            XCTAssertEqual(strip, mainStrip(fleet))
        }
    }

    // MARK: - The counts

    /// Each row's figures are its pool's half of the menu-bar count, in the same order.
    func testThePoolRowsCountWhatTheMenuBarCounts() throws {
        let scene = try decodeFleet(sceneTwentyFourRows())
        let pools = try XCTUnwrap(strip(scene).pools)
        XCTAssertEqual(pools.map(\.provider), [.anthropic, .codex])
        XCTAssertEqual(pools.map(\.lead), ["Claude · ", "Codex · "])
        XCTAssertEqual(pools.map(\.count), ["0 of 2 ready", "1 of 2 ready"])
        XCTAssertEqual(menuBarHalves(pools), "0/2 · 1/2")
        XCTAssertEqual(PollState.loaded(scene).countsLabel, menuBarHalves(pools))

        // jo counts as ready by the no-window evidence; carol is the one Codex row spent.
        let canonical = try canonicalFleet()
        let canonicalPools = try XCTUnwrap(strip(canonical).pools)
        XCTAssertEqual(canonicalPools.map(\.count), ["1 of 1 ready", "3 of 9 ready"])
        XCTAssertEqual(menuBarHalves(canonicalPools), "1/1 · 3/9")
        XCTAssertEqual(PollState.loaded(canonical).countsLabel, menuBarHalves(canonicalPools))
    }

    /// The count's colour: green with something ready, amber with nothing ready and
    /// something near, red otherwise.
    func testThePoolRowTones() throws {
        let scene = try XCTUnwrap(strip(try decodeFleet(sceneTwentyFourRows())).pools)
        XCTAssertEqual(scene.map(\.tone), [.bad, .ok])

        let nearCodex = try decodeFleet(
            try claudeThreeOfFour() + [
                canonicalRow("ivy@example.com"), canonicalRow("carol@example.com"),
            ])
        XCTAssertEqual(try XCTUnwrap(strip(nearCodex).pools).map(\.tone), [.ok, .near])
    }

    /// A pool whose every row is parked, here by a parked group, says so in grey, counts
    /// nothing down, and drops out of the menu-bar count.
    func testAPoolWithNoEnabledAccountReadsParked() throws {
        let fleet = try decodeFleet(try claudeThreeOfFour() + nightParkedCodexRows())
        let pools = try XCTUnwrap(strip(fleet).pools)
        let codex = try XCTUnwrap(pools.last)
        XCTAssertEqual(codex.provider, .codex)
        XCTAssertEqual(codex.count, "2 parked")
        XCTAssertEqual(codex.tone, .dim)
        XCTAssertNil(codex.nextBackCountdown)
        XCTAssertNil(codex.nextInCountdown)
        XCTAssertEqual(codex.help, "Codex: 2 parked")
        XCTAssertEqual(codex.spoken, "Codex, 2 parked.")
        XCTAssertEqual(PollState.loaded(fleet).countsLabel, "3/4")
    }

    // MARK: - The countdowns

    /// Each pool row counts down by the strip's rule over its own out-of-tokens rows, so it
    /// shows the figure that pool's OUT OF TOKENS heading shows. At zero ready with no such
    /// figure, the pool's own return fills in.
    func testEachPoolCountsDownByMainsRule() throws {
        let scene = try decodeFleet(sceneTwentyFourRows())
        let pools = try XCTUnwrap(strip(scene).pools)
        XCTAssertEqual(pools.map(\.nextBackCountdown), ["in 2h 39m", "in 3d 4h"])
        for line in pools {
            XCTAssertEqual(
                line.nextBackCountdown,
                try outOfTokensCountdown(
                    Fleet(accounts: scene.accounts.filter { $0.provider == line.provider })),
                "\(line.name)'s row and its own OUT OF TOKENS heading disagree")
        }

        let codexOnly = try decodeFleet(codexOnlyRows())
        let codexLine = try XCTUnwrap(strip(codexOnly).pools?.first)
        XCTAssertEqual(codexLine.nextBackCountdown, "in 3d 4h")
        XCTAssertEqual(codexLine.nextBackCountdown, try outOfTokensCountdown(codexOnly))

        // hal's hold carries no window and keeps him in the live band, so carol's week is
        // the row's figure, while the pool's return, which the tooltip prints, is hal's.
        let carolAndHal = try decodeFleet([
            canonicalRow("carol@example.com"), canonicalRow("hal@example.com"),
        ])
        let both = try XCTUnwrap(strip(carolAndHal).pools?.first)
        XCTAssertEqual(both.nextBackCountdown, "in 3d 4h")
        XCTAssertNil(both.nextInCountdown)
        XCTAssertEqual(
            try XCTUnwrap(carolAndHal.providerPools(now: fixtureNow)?.first).nextIn,
            "next in 47m")

        let halAlone = try decodeFleet([canonicalRow("hal@example.com")])
        let hal = try XCTUnwrap(strip(halAlone).pools?.first)
        XCTAssertNil(hal.nextBackCountdown)
        XCTAssertEqual(hal.nextInCountdown, "in 47m")
    }

    // MARK: - Words

    /// The breakdown the row does not draw is its tooltip, and the strip speaks every pool
    /// in one sentence each, saying the ready count once.
    func testTheHelpAndTheSpokenSentence() throws {
        let canonical = try XCTUnwrap(strip(try canonicalFleet()).pools)
        XCTAssertEqual(
            canonical.map(\.help),
            [
                "Claude: 1 ready",
                "Codex: 3 ready · 1 near limit · 1 spent · 1 need re-login · 2 refused by ChatGPT"
                    + " · 1 unreachable",
            ])

        let scene = try XCTUnwrap(strip(try decodeFleet(sceneTwentyFourRows())).pools)
        XCTAssertEqual(
            scene.map(\.spoken).joined(separator: " "),
            "Claude, 0 of 2 ready, 2 spent, next back in 2h 39m. "
                + "Codex, 1 of 2 ready, 1 spent, next back in 3d 4h.")
    }

    // MARK: - Room

    /// The widest pool row fits the strip at both densities without truncating: the lead,
    /// a two-digit count, and the longest countdown under the week window, with the
    /// headline gap on both sides of the spacer.
    func testTheWidestPoolRowFitsTheStrip() throws {
        let room =
            try token("panelWidth") - 2 * (try token("panelPaddingSide"))
            - 2 * (try token("stripPaddingSide"))
        let gap = try token("stripHeadlineGap")
        let step = try token("stripHeadlineStep")
        let comfortable = try densityToken("summarySize")
        let compact = try XCTUnwrap(
            firstCapture(
                of: "staticvarsummarySize:CGFloat\\{compact\\?([0-9.]+):[0-9.]+\\}",
                in: try squashedV4Source()
            ).flatMap(Double.init),
            "V4.summarySize is no longer `compact ? <n> : <n>`")
        XCTAssertEqual(room, 344)

        var totals: [String] = []
        var overflows = false
        for size in [CGFloat(compact) + step, comfortable + step] {
            for lead in ["Claude · ", "Codex · "] {
                for countdown in ["next back in 23h 59m", "next back in 6d 23h"] {
                    let total =
                        AccountLineFit.textWidth(lead, size: size, weight: .regular)
                        + AccountLineFit.textWidth("99 of 99 ready", size: size, weight: .semibold)
                        + 2 * gap
                        + AccountLineFit.textWidth(countdown, size: size, weight: .regular)
                    totals.append(
                        "\(lead)99 of 99 ready | \(countdown) at \(size) pt: "
                            + String(format: "%.1f", total))
                    if total > room { overflows = true }
                }
            }
        }
        XCTAssertFalse(
            overflows,
            "a pool row overflows the strip's \(room) pt:\n" + totals.joined(separator: "\n"))
    }

    // MARK: - The app target

    /// The shim never compiles the app target, so these anchors are its only guard there:
    /// the tab hands the strip its pools, the strip header draws them, and the money line
    /// reads per-provider figures in a fleet holding Codex.
    func testTheStripAndTheMoneyLineAreWired() throws {
        let anchors: [(file: String, text: String)] = [
            ("PanelV4/AccountsTabV4.swift", "pools: fleet.providerPools(now: now)"),
            ("PanelV4/FleetStripHeader.swift", "strip.pools"),
            ("PanelV4/SummaryLine.swift", "fleet.holdsCodex"),
            ("PanelV4/SummaryLine.swift", "AccountsSummary.lines(fleet)"),
        ]
        for anchor in anchors {
            XCTAssertTrue(
                try panelSource(anchor.file).contains(anchor.text),
                "\(anchor.file) no longer contains `\(anchor.text)`")
        }
    }

    // MARK: - Strips

    /// The strip as the Accounts tab builds it, every group closed, with the pools.
    private func strip(_ fleet: Fleet) -> FleetStrip {
        FleetStrip(
            drawnRows: drawnRows(fleet), now: fixtureNow, pools: fleet.providerPools(now: fixtureNow))
    }

    /// The same strip built with no pools at all, as every fleet's strip was built before.
    private func mainStrip(_ fleet: Fleet) -> FleetStrip {
        FleetStrip(drawnRows: drawnRows(fleet), now: fixtureNow)
    }

    private func drawnRows(_ fleet: Fleet) -> [FleetSectionRow] {
        fleet.accountsTabSections(expandedGroups: [])
            .rowsInDrawOrder(expandedGroups: [], parkedCap: 2) { _ in false }
    }

    /// The pool rows' figures written the way the menu bar's count writes them.
    private func menuBarHalves(_ pools: [StripPoolLine]) -> String {
        pools.filter { $0.enabled > 0 }.map { "\($0.ready)/\($0.enabled)" }
            .joined(separator: " · ")
    }

    /// `fleet`'s first OUT OF TOKENS heading's countdown.
    private func outOfTokensCountdown(_ fleet: Fleet) throws -> String? {
        try XCTUnwrap(fleet.sectionsInDisplayOrder().first { $0.band == .outOfTokens })
            .firstBackCountdown(now: fixtureNow)
    }

    // MARK: - Rows

    /// alice's 5-hour window full and held, back at 1767235140000: 2h 39m after `fixtureNow`.
    private let spentOn5h: [String: Any] = [
        "quotaState": "spent", "quota": 1.0, "fiveHour": 1.0, "fiveHourState": "spent",
        "held": [["window": "5h", "minutesUntilReset": 159, "resetAtMs": 1_767_235_140_000]],
    ]

    /// The week full and held, back at 1767484800000: 3d after `fixtureNow`.
    private let spentOn7d: [String: Any] = [
        "quotaState": "spent", "quota": 1.0, "sevenDay": 1.0, "sevenDayState": "spent",
        "held": [["window": "7d", "minutesUntilReset": 4320, "resetAtMs": 1_767_484_800_000]],
    ]

    /// Canonical alice under another name and her own org, in no group: a second Claude
    /// identity with alice's facts. `index` picks the org, so two copies never share one.
    private func aliceCopy(
        _ name: String, _ index: Int, _ overrides: [String: Any] = [:]
    ) throws -> [String: Any] {
        let digit = String(Array("234ef")[index])
        func repeated(_ count: Int) -> String { String(repeating: digit, count: count) }
        var all: [String: Any] = [
            "name": name, "groups": [String](),
            "orgUuid":
                "\(repeated(8))-\(repeated(4))-4\(repeated(3))-8\(repeated(3))-\(repeated(12))",
        ]
        for (key, value) in overrides { all[key] = value }
        return try canonicalRow("alice@example.com", all)
    }

    /// alice in no group, with `overrides`.
    private func alice(_ overrides: [String: Any] = [:]) throws -> [String: Any] {
        var all: [String: Any] = ["groups": [String]()]
        for (key, value) in overrides { all[key] = value }
        return try canonicalRow("alice@example.com", all)
    }

    /// frank: alice's copy, spent on the week.
    private func frank() throws -> [String: Any] {
        try aliceCopy("frank@example.com", 0, spentOn7d)
    }

    /// A Claude pool of 3 of 4 ready: alice and two copies, ready, and frank, spent.
    private func claudeThreeOfFour() throws -> [[String: Any]] {
        [
            try alice(), try aliceCopy("nina@example.com", 1), try aliceCopy("omar@example.com", 2),
            try frank(),
        ]
    }

    /// Scene 24's four rows: alice and frank spent, bob ready, carol spent.
    private func sceneTwentyFourRows() throws -> [[String: Any]] {
        [
            try alice(spentOn5h), try frank(), try canonicalRow("bob@example.com/codex"),
            try canonicalRow("carol@example.com"),
        ]
    }

    /// The Codex-only fleet: bob and dana ready, carol spent, erin signed out.
    private func codexOnlyRows() throws -> [[String: Any]] {
        try ["bob@example.com/codex", "dana@example.com", "carol@example.com", "erin@example.com/codex"]
            .map { try canonicalRow($0) }
    }

    /// bob and carol in `night`, a parked group no Claude row is in, the way the server sends
    /// a parked group's members. carol keeps her held week.
    private func nightParkedCodexRows() throws -> [[String: Any]] {
        let parked: [String: Any] = [
            "groups": ["night"], "parkedGroups": ["night"], "gate": "parked", "freeAtMs": NSNull(),
        ]
        return [
            try canonicalRow("bob@example.com/codex", parked),
            try canonicalRow("carol@example.com", parked),
        ]
    }

    /// All ten canonical rows: alice, and nine Codex accounts in every state.
    private func canonicalFleet() throws -> Fleet {
        try decodeFleet(
            try [
                "alice@example.com", "bob@example.com/codex", "carol@example.com",
                "dana@example.com", "erin@example.com/codex", "gil@example.com", "hal@example.com",
                "ivy@example.com", "jo@example.com", "kim@example.com",
            ].map { try canonicalRow($0) })
    }

    // MARK: - Reading the tokens out of the app target's source

    /// `static let <name>: CGFloat = <number>`.
    private func token(_ name: String) throws -> CGFloat {
        let value = try XCTUnwrap(
            firstCapture(of: "static let \(name): CGFloat = ([0-9.]+)\\b", in: try v4Source()),
            "V4.\(name) is no longer a plain `static let … = <number>`; teach this test the "
                + "new shape rather than measuring against a guess")
        return CGFloat(try XCTUnwrap(Double(value)))
    }

    /// `static var <name>: CGFloat { compact ? <a> : <b> }`: the comfortable
    /// branch, the larger of the two, so a fit proven here holds at Compact's
    /// tighter size as well.
    private func densityToken(_ name: String) throws -> CGFloat {
        let value = try XCTUnwrap(
            firstCapture(
                of: "staticvar\(name):CGFloat\\{compact\\?[0-9.]+:([0-9.]+)\\}",
                in: try squashedV4Source()),
            "V4.\(name) is no longer `compact ? <n> : <n>`")
        return CGFloat(try XCTUnwrap(Double(value)))
    }

    private func firstCapture(of pattern: String, in source: String) -> String? {
        guard let regex = try? NSRegularExpression(pattern: pattern),
            let match = regex.firstMatch(
                in: source, range: NSRange(source.startIndex..., in: source)),
            let range = Range(match.range(at: 1), in: source)
        else { return nil }
        return String(source[range])
    }

    private func squashedV4Source() throws -> String {
        try v4Source().components(separatedBy: .whitespacesAndNewlines).joined()
    }

    private func v4Source() throws -> String {
        try panelSource("PanelV4/V4.swift")
    }

    private func panelSource(_ relative: String) throws -> String {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // -> TcrBarTests
            .deletingLastPathComponent()  // -> Tests
            .deletingLastPathComponent()  // -> apps/macos
            .deletingLastPathComponent()  // -> apps
            .deletingLastPathComponent()  // -> repo root
        return try String(
            contentsOf: repoRoot.appendingPathComponent("apps/macos/Sources/TcrBar/\(relative)"),
            encoding: .utf8)
    }
}
