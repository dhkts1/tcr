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
    try Fleet.decode(JSONSerialization.data(withJSONObject: rows))
}

/// One Claude row of the scene-02 shape: every key the scene's builder writes that a
/// recorded value can read, and no `usage`.
private func claudeRow(
    _ name: String, quota: Any, state: String, disabled: Bool = false, probe: String = "ok",
    held: [[String: Any]] = [], extra: [String: Any] = [:]
) -> [String: Any] {
    var row: [String: Any] = [
        "name": name, "priority": 0, "status": "active", "disabled": disabled,
        "quota": quota, "quotaState": state, "fiveHour": quota, "sevenDay": quota,
        "held": held, "requests": 102, "inputTokens": 8_781_926, "outputTokens": 31_860,
        "cacheReadTokens": 7_407_414, "cacheHitRatio": 0.84, "probeStatus": probe,
        "probeError": NSNull(), "lastStreamError": NSNull(), "streamErrorCount": 0,
        "source": "live",
    ]
    for (key, value) in extra { row[key] = value }
    return row
}

/// Scene `02-mixed-thirteen`'s thirteen rows as main's builder writes them, against
/// `fixtureNow`, plus a gate-rejected and a group-parked Claude row: the two kinds of row
/// that today's readiness counts as ready and the state classifier does not.
private func claudeOnlyRows() -> [[String: Any]] {
    var rows = [0.12, 0.22, 0.32, 0.42].enumerated().map { index, quota in
        claudeRow("ok-\(index + 1)@example.com", quota: quota, state: "ok")
    }
    rows.append(
        claudeRow(
            "near@example.com", quota: 0.94, state: "near",
            held: [["window": "7d", "minutesUntilReset": 6498, "resetAtMs": 1_786_406_400_224]]))
    // Out of tokens the way main's scene draws them: the 5-hour window spent and
    // held, the week only partly used.
    let weeks = [0.6, 0.5, 0.7, 0.4, 0.6, 0.5]
    for n in 1...6 {
        let minutes = 12 + 30 * n
        let resetAtMs = Int64(1_767_225_600_000) + Int64(minutes) * 60_000
        rows.append(
            claudeRow(
                "spent-\(n)@example.com", quota: 1.0, state: "spent",
                held: [["window": "5h", "minutesUntilReset": minutes, "resetAtMs": resetAtMs]],
                extra: [
                    "fiveHour": 1.0, "fiveHourState": "spent",
                    "sevenDay": weeks[n - 1], "sevenDayState": "ok",
                ]))
    }
    rows.append(
        claudeRow(
            "never@example.com", quota: NSNull(), state: "ok", disabled: true, probe: "never"))
    rows.append(claudeRow("parked@example.com", quota: 0.2, state: "ok", disabled: true))
    rows.append(
        claudeRow("rejected-ok@example.com", quota: 0.3, state: "ok", extra: ["gate": "rejected"]))
    // `probeStatus "ok"` is load-bearing: with `"never"` this row stops disagreeing.
    rows.append(
        claudeRow(
            "group-parked@example.com", quota: 0.2, state: "ok",
            extra: [
                "gate": "parked", "groups": ["henry-team"], "parkedGroups": ["henry-team"],
            ]))
    return rows
}

/// A session with one running call, or none.
private func session(running: Bool) -> Session {
    Session(
        sessionId: "s-\(UUID().uuidString)", firstSeenMs: 0, lastSeenMs: 0,
        tools: running
            ? SessionTools(calls: 1, running: [ToolCall(tool: "Bash")]) : SessionTools())
}

/// A fleet counted per provider: each pool's ready count, the cup that follows the emptier
/// pool, the tooltip and the summary. Every Codex row comes from the canonical fixture.
final class PoolCapacityTests: XCTestCase {

    // MARK: - A fleet with no Codex account keeps today's counts

    /// A Claude-only fleet draws its counts, cup, tint, tooltip and spoken mark from today's
    /// code, against values recorded on that code before the per-pool counts existed.
    ///
    /// The headline counts 6 ready while the breakdown says 4 ready, 1 rejected and 3
    /// parked. That disagreement is today's Claude-only behaviour, and it is pinned here on
    /// purpose: today's readiness reads the quota and not the server's gate, so the
    /// gate-rejected row and the group-parked row both count as ready in the headline, while
    /// the state classifier behind the breakdown files them as rejected and parked. Counting
    /// them the other way would change what a Claude-only fleet shows, which is a separate,
    /// deliberate change.
    func testClaudeOnlyFleetUsesTodaysReadiness() throws {
        let fleet = try decodeFleet(claudeOnlyRows())
        XCTAssertEqual(fleet.accounts.count, 15)
        XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
        let sentence = "6 of 13 accounts ready · 1 near their limit · 3 parked"

        XCTAssertEqual(fleet.readyCount, 6)
        XCTAssertEqual(fleet.enabledCount, 13)
        XCTAssertEqual(fleet.capacitySummary, "6 of 13 ready")
        XCTAssertEqual(fleet.countsSentence, sentence)
        XCTAssertEqual(
            fleet.sentenceBreakdown.map(\.sentenceLabel),
            ["4 ready", "1 near limit", "6 spent", "1 rejected by Anthropic", "3 parked"])

        let state = PollState.loaded(fleet)
        XCTAssertEqual(state.countsLabel, "6/13")
        XCTAssertEqual(state.capacityFraction, 6.0 / 13.0)
        XCTAssertEqual(state.capacityTintKind(awake: false), .template)
        XCTAssertEqual(state.tooltipSentence, sentence)
        XCTAssertEqual(MenuBarMark.accessibilityDescription(awake: false), "tcr fleet capacity")

        let withTools = Fleet(
            accounts: fleet.accounts,
            sessions: [session(running: true), session(running: true), session(running: false)],
            sessionsSupported: true)
        XCTAssertEqual(PollState.loaded(withTools).runningToolsCount(showRunningTools: true), 2)
        XCTAssertEqual(PollState.loaded(withTools).tooltipSentence, sentence)

        // The per-pool API keeps every one of these for a fleet with no Codex account,
        // whatever the cup's pin says.
        for pin in MenuBarCupPool.allCases {
            XCTAssertEqual(state.capacityFraction(cupPool: pin), 6.0 / 13.0, "\(pin)")
            XCTAssertEqual(state.capacityTintKind(awake: false, cupPool: pin), .template, "\(pin)")
            XCTAssertEqual(state.countsParts, [PollState.CountsPart(text: "6/13", isNear: false)])
            XCTAssertEqual(state.tooltipSentence(cupPool: pin), sentence, "\(pin)")
            XCTAssertEqual(
                state.menuBarToolTip(
                    awake: true, showRunningTools: false, knocks: 0, cupPool: pin),
                sentence + " · Keeping this Mac awake", "\(pin)")
            XCTAssertEqual(
                MenuBarMark.accessibilityDescription(awake: false, fleet: fleet),
                "tcr fleet capacity")
        }
    }

    /// The tooltip a fleet with no Codex account shows, composed the way the menu bar has
    /// always composed it: the capacity sentence, then the running tools, then keep-awake,
    /// and a waiting Mac in front of all of it. Every expected value is written out by
    /// hand, so a change to any piece of the composition shows here.
    func testClaudeOnlyTooltipKeepsTodaysComposition() throws {
        let accounts = try decodeFleet(claudeOnlyRows()).accounts
        let s = "6 of 13 accounts ready · 1 near their limit · 3 parked"
        let withTools = PollState.loaded(
            Fleet(
                accounts: accounts,
                sessions: [session(running: true), session(running: true), session(running: false)],
                sessionsSupported: true))
        let oneTool = PollState.loaded(
            Fleet(accounts: accounts, sessions: [session(running: true)], sessionsSupported: true))
        let noTools = PollState.loaded(
            Fleet(accounts: accounts, sessions: [], sessionsSupported: true))
        let plain = PollState.loaded(Fleet(accounts: accounts))
        let failed = PollState.commandFailed(exitCode: 1, message: "x")

        for pin in MenuBarCupPool.allCases {
            XCTAssertEqual(
                withTools.menuBarToolTip(
                    awake: true, showRunningTools: true, knocks: 2, cupPool: pin),
                "2 Macs are asking to connect. Open the Peers tab to answer them. "
                    + "6 of 13 accounts ready · 1 near their limit · 3 parked"
                    + " · 2 tools running · Keeping this Mac awake", "\(pin)")
            XCTAssertEqual(
                oneTool.menuBarToolTip(
                    awake: false, showRunningTools: true, knocks: 1, cupPool: pin),
                "1 Mac is asking to connect. Open the Peers tab to answer it. "
                    + "6 of 13 accounts ready · 1 near their limit · 3 parked · 1 tool running",
                "\(pin)")
            XCTAssertEqual(
                noTools.menuBarToolTip(
                    awake: false, showRunningTools: true, knocks: 0, cupPool: pin),
                "6 of 13 accounts ready · 1 near their limit · 3 parked · 0 tools running",
                "\(pin)")
            XCTAssertEqual(
                plain.menuBarToolTip(
                    awake: false, showRunningTools: false, knocks: 0, cupPool: pin),
                s, "\(pin)")
            XCTAssertEqual(
                failed.menuBarToolTip(
                    awake: true, showRunningTools: true, knocks: 1, cupPool: pin),
                "1 Mac is asking to connect. Open the Peers tab to answer it. "
                    + "tcr status failed (exit 1): x · Keeping this Mac awake", "\(pin)")
        }
    }

    // MARK: - The money line

    /// A fleet holding Codex accounts keeps the one money line, with Claude's own figures and
    /// a Codex clause. A fleet without one has no such line here: it keeps today's. The
    /// cache figure is cache reads over input-side tokens (base input, cache writes and
    /// cache reads), summed over today's buckets of the rows named and no others.
    func testTheSummaryLinesFollowTheFleetsShape() throws {
        func text(_ line: [SummaryRun]) -> String { line.map(\.text).joined(separator: " · ") }

        XCTAssertNil(AccountsSummary.lines(try decodeFleet([canonicalRow("alice@example.com")])))

        // alice and frank are two copies of canonical alice's day: $14.1657 each, and
        // 12,000,000 cache reads of 16,000,000 Claude input-side tokens. Codex's 83k is
        // bob's 22,000 and carol's 61,000 output tokens.
        let mixed = try decodeFleet(sceneTwentyFourRows())
        let line = try XCTUnwrap(AccountsSummary.lines(mixed))
        XCTAssertEqual(line.count, 2)
        XCTAssertEqual(text(line[0]), "$28.3 today · cache 75%")
        XCTAssertEqual(text(line[1]), "Codex 83k tokens")
        XCTAssertEqual(line.flatMap { $0 }.map(\.role), [.dim, .dim, .dim])
        XCTAssertEqual(
            AccountsSummary.usageHelp(mixed),
            "Codex: 83k output tokens today, unpriced: a ChatGPT plan is not billed per request.")

        let priced = try decodeFleet([
            try alice(spentOn5h), try frank(),
            try pricedToday(canonicalRow("bob@example.com/codex"), cost: 1.2),
            try pricedToday(canonicalRow("carol@example.com"), cost: 3.4),
        ])
        let pricedLine = try XCTUnwrap(AccountsSummary.lines(priced))
        XCTAssertEqual(text(pricedLine[0]), "$28.3 today · cache 75%")
        XCTAssertEqual(text(pricedLine[1]), "Codex $4.60")
        XCTAssertEqual(
            AccountsSummary.usageHelp(priced), "Codex: $4.60 today, at the prices in your config.")

        // 7,408,000 of the four Codex rows' 8,500,000 input-side tokens today were cache reads.
        let codexOnly = try decodeFleet(codexOnlyRows())
        let codexLine = try XCTUnwrap(AccountsSummary.lines(codexOnly))
        XCTAssertEqual(codexLine.count, 1)
        XCTAssertEqual(text(codexLine[0]), "343k tokens today · cache 87%")
        XCTAssertEqual(codexLine[0].map(\.role), [.dim, .dim])
        XCTAssertNil(AccountsSummary.usageHelp(codexOnly))

        let unmeasured = try decodeFleet(
            try ["alice@example.com", "bob@example.com/codex", "carol@example.com"].map {
                try canonicalRow($0, ["usage": NSNull()])
            })
        XCTAssertNil(AccountsSummary.lines(unmeasured))
    }

    /// In a fleet holding both providers the money line keeps its two lines whichever
    /// clauses a poll adds or drops: Claude's first unpriced request, Codex's first request
    /// of the day, or midnight clearing either. On one line, the unpriced clause landing
    /// beside the Codex one wrapped it onto a second and moved everything under it.
    func testTheMixedMoneyLineKeepsItsLinesWhenAClauseComesOrGoes() throws {
        func unpriced(_ row: [String: Any], _ count: Int) throws -> [String: Any] {
            var row = row
            var usage = try XCTUnwrap(row["usage"] as? [String: Any])
            var today = try XCTUnwrap(usage["today"] as? [String: Any])
            today["unpricedRequests"] = count
            usage["today"] = today
            row["usage"] = usage
            return row
        }
        let codexIdle: [String: Any] = ["usage": NSNull()]
        let states: [(String, [[String: Any]])] = [
            ("both clauses", try sceneTwentyFourRows()),
            (
                "unpriced beside Codex",
                [
                    try unpriced(try alice(spentOn5h), 3), try frank(),
                    try canonicalRow("bob@example.com/codex"), try canonicalRow("carol@example.com"),
                ]
            ),
            (
                "no Codex request today",
                [
                    try alice(spentOn5h), try frank(),
                    try canonicalRow("bob@example.com/codex", codexIdle),
                    try canonicalRow("carol@example.com", codexIdle),
                ]
            ),
            (
                "no Claude request today",
                [
                    try alice(["usage": NSNull()]),
                    try aliceCopy("frank@example.com", 0, ["usage": NSNull()]),
                    try canonicalRow("bob@example.com/codex"), try canonicalRow("carol@example.com"),
                ]
            ),
        ]
        for (name, rows) in states {
            let lines = try XCTUnwrap(AccountsSummary.lines(try decodeFleet(rows)), name)
            XCTAssertEqual(lines.count, 2, name)
            XCTAssertFalse(
                lines[0].contains { $0.text.hasPrefix("Codex") }, "\(name): a Codex clause on Claude's line")
        }
        let withUnpriced = try XCTUnwrap(AccountsSummary.lines(try decodeFleet(states[1].1)))
        XCTAssertEqual(
            withUnpriced[0].map(\.text), ["$28.3 today", "cache 75%", "3 unpriced today"],
            "positive control: the unpriced clause is drawn")
    }

    // MARK: - What a Codex row's state is

    /// A Codex session cannot ask for a group, so a Codex row in a reserved group takes no
    /// request at all, however healthy its quota reads.
    func testAReservedCodexRowIsUnreachable() throws {
        let kim = try account(canonicalRow("kim@example.com"))
        XCTAssertTrue(kim.isUnreachable)
        XCTAssertEqual(FleetTally.Kind(account: kim), .unreachable)
        XCTAssertEqual(FleetTally.Kind.unreachable.token, "unreachable")
        XCTAssertEqual(FleetTally.Kind.unreachable.phrase(for: .codex), "unreachable")
        XCTAssertFalse(kim.isReady, "a Codex row with a blocking gate is not ready")
    }

    /// A hold that names the limit ChatGPT reached is ChatGPT's refusal, in ChatGPT's word.
    func testAHoldWithAReachedTypeIsRefused() throws {
        let hal = try account(canonicalRow("hal@example.com"))
        XCTAssertTrue(hal.isRefusedHold)
        XCTAssertEqual(FleetTally.Kind(account: hal), .rejected)
        XCTAssertEqual(FleetTally.Kind.rejected.phrase(for: .codex), "refused by ChatGPT")
        XCTAssertEqual(FleetTally.Kind.rejected.phrase(for: .anthropic), "rejected by Anthropic")
        XCTAssertEqual(hal.freeAtMs, 1_767_228_420_000)

    }

    /// The server arms the same `hold` gate for a plain 429 burst. Nothing names a limit, so
    /// the row is held, not refused: its pill and the pool's count read its quota, as a held
    /// Claude row's do, so a burst does not flap the card between OK and Refused.
    func testAPlainRateLimitHoldIsNotRefused() throws {
        let burst = try account(
            canonicalRow(
                "hal@example.com",
                ["limitReachedType": NSNull(), "secondsUntilFree": 17, "sevenDay": 0.3]))
        XCTAssertEqual(burst.gate, .unknown("hold"))
        XCTAssertFalse(burst.isRefusedHold)
        XCTAssertEqual(FleetTally.Kind(account: burst), .ok)
        XCTAssertFalse(burst.isReady, "a held Codex row takes no request while the hold runs")
        XCTAssertEqual(Fleet(accounts: [burst]).readyCount, 0)
        XCTAssertEqual(PoolCapacity(provider: .codex, accounts: [burst], now: Date()).readyCount, 1)
    }

    /// A plan that reports no usage window has no figure to carry, so once a probe has run
    /// it is measured. A failed probe keeps it measured, as it keeps a Claude row's bar.
    func testNoWindowsAfterAProbeCountsAsMeasured() throws {
        let jo = try account(canonicalRow("jo@example.com"))
        XCTAssertTrue(jo.hasQuotaEvidence)
        XCTAssertEqual(FleetTally.Kind(account: jo), .ok)

        for probe in ["error", "timeout", "rate-limited"] {
            let failed = try account(canonicalRow("jo@example.com", ["probeStatus": probe]))
            XCTAssertTrue(failed.hasQuotaEvidence, probe)
            XCTAssertEqual(FleetTally.Kind(account: failed), .ok, probe)
        }
        let alice = try account(canonicalRow("alice@example.com", ["probeStatus": "error"]))
        XCTAssertTrue(alice.hasQuotaEvidence)
        XCTAssertEqual(FleetTally.Kind(account: alice), .ok)

        let never = try account(canonicalRow("jo@example.com", ["probeStatus": "never"]))
        XCTAssertEqual(FleetTally.Kind(account: never), .unmeasured)
        let unknown = try account(canonicalRow("jo@example.com", ["quotaWindows": NSNull()]))
        XCTAssertEqual(FleetTally.Kind(account: unknown), .unmeasured)
    }

    /// A real rejected gate stays a refusal even when a quota window is full too.
    func testARejectedCodexLoginIsNotReclassifiedAsASpentWindow() throws {
        let row = try account(canonicalRow("carol@example.com", rejectedFullWindow))
        XCTAssertTrue(row.isRejected)
        XCTAssertEqual(FleetTally.Kind(account: row), .rejected)
        XCTAssertNotNil(row.stateHelp)
    }

    /// The short hold the server arms after ChatGPT answered 403 is a refusal on a Codex
    /// row. A Claude row held the same way reads as it always has.
    func testA403HoldOnACodexRowIsRefused() throws {
        let bob = try account(canonicalRow("bob@example.com/codex", forbiddenHold))
        XCTAssertTrue(bob.isForbiddenHold)
        XCTAssertFalse(bob.isRefusedHold)
        XCTAssertFalse(bob.isRejected)
        XCTAssertEqual(FleetTally.Kind(account: bob), .rejected)

        // No held window, so the hold's own end is the pool's return: 90 s rounds to 2m.
        let pool = PoolCapacity(provider: .codex, accounts: [bob], now: fixtureNow)
        XCTAssertEqual(pool.readyCount, 0)
        XCTAssertEqual(pool.nextReturnAtMs, 1_767_225_690_000)
        XCTAssertEqual(pool.nextIn, "next in 2m")

        let alice = try account(canonicalRow("alice@example.com", ["gate": "refused"]))
        XCTAssertFalse(alice.isForbiddenHold)
        XCTAssertEqual(FleetTally.Kind(account: alice), .ok)
        XCTAssertTrue(alice.isReady)
    }

    /// The card's pill says `Refused` on a refused Codex row, and every other word is the
    /// one register main already has.
    func testTheCodexPillWord() {
        let kinds: [FleetTally.Kind] = [
            .ok, .near, .spent, .unknown, .needsRelogin, .rejected, .unmeasured, .disabled,
            .unreachable,
        ]
        for kind in kinds {
            XCTAssertEqual(kind.pillText(for: .anthropic), kind.pillText, kind.token)
            XCTAssertEqual(
                kind.pillText(for: .codex), kind == .rejected ? "Refused" : kind.pillText,
                kind.token)
        }
        XCTAssertEqual(FleetTally.Kind.unreachable.pillText, "Unreachable")
    }

    // MARK: - Each pool, counted by state

    /// A pool's ready rows are its enabled rows whose state is ready. jo counts, by the
    /// no-window evidence; gil, hal and kim do not, though today's readiness would take them.
    func testAMixedFleetCountsEachPoolByKind() throws {
        let pools = try XCTUnwrap(try canonicalFleet().providerPools(now: fixtureNow))
        XCTAssertEqual(pools.map(\.provider), [.anthropic, .codex])
        XCTAssertEqual(pools[0].readyCount, 1)
        XCTAssertEqual(pools[0].enabledCount, 1)
        XCTAssertEqual(pools[1].readyCount, 3)
        XCTAssertEqual(pools[1].enabledCount, 9)
        XCTAssertEqual(
            pools[1].breakdownLabels,
            [
                "3 ready", "1 near limit", "1 spent", "1 need re-login", "2 refused by ChatGPT",
                "1 unreachable",
            ])
    }

    /// A Codex-only fleet has one pool. A fleet with no Codex account has none: it keeps
    /// today's counts, and so does the empty fleet.
    func testACodexOnlyFleetCountsByKind() throws {
        let pools = try XCTUnwrap(try decodeFleet(codexOnlyRows()).providerPools(now: fixtureNow))
        XCTAssertEqual(pools.map(\.provider), [.codex])
        XCTAssertEqual(pools[0].readyCount, 2)
        XCTAssertEqual(pools[0].enabledCount, 4)
        XCTAssertEqual(pools[0].breakdownLabels, ["2 ready", "1 spent", "1 need re-login"])

        XCTAssertNil(
            try decodeFleet([canonicalRow("alice@example.com")]).providerPools(now: fixtureNow))
        XCTAssertNil(Fleet(accounts: []).providerPools(now: fixtureNow))
    }

    /// The cup follows the pool with the lower ready fraction. A tie goes to the worse
    /// state, spent or unmeasured before near, near before ok, and then to Claude.
    func testTheEmptierPoolGovernsWithItsTieBreaks() throws {
        let cases: [(label: String, rows: [[String: Any]], governs: Provider)] = [
            (
                "Claude 3/4, Codex 0/2 spent",
                try claudeThreeOfFour() + [
                    canonicalRow("bob@example.com/codex", ["quotaState": "spent"]),
                    canonicalRow("carol@example.com"),
                ],
                .codex
            ),
            (
                "Claude 3/4, Codex 2/2",
                try claudeThreeOfFour() + [
                    canonicalRow("bob@example.com/codex"), canonicalRow("dana@example.com"),
                ],
                .anthropic
            ),
            (
                "both 0/2, Claude spent and Codex near: spent outranks near",
                try claudeNoneOfTwo() + [
                    canonicalRow("ivy@example.com"), canonicalRow("carol@example.com"),
                ],
                .anthropic
            ),
            (
                "both 0/2 and spent: a tie goes to Claude",
                try claudeNoneOfTwo() + [
                    canonicalRow("carol@example.com"),
                    canonicalRow("dana@example.com", ["quotaState": "spent"]),
                ],
                .anthropic
            ),
            (
                "both 1/2 and ok: a tie goes to Claude",
                try claudeOneOfTwo() + [
                    canonicalRow("bob@example.com/codex"), canonicalRow("carol@example.com"),
                ],
                .anthropic
            ),
        ]
        for (label, rows, governs) in cases {
            let fleet = try decodeFleet(rows)
            XCTAssertEqual(fleet.governingPool(.emptier, now: fixtureNow)?.provider, governs, label)
        }
    }

    /// The tie the worse state decides with Codex the worse pool: both at 0 of 2, Claude
    /// near and Codex spent. Codex governs, so the cup is not amber, and the tooltip says
    /// the cup shows Codex. The first pool, Claude's, would win the tie on its own.
    func testASpentCodexPoolOutranksANearClaudePoolAtTheSameFraction() throws {
        let fleet = try decodeFleet(
            try [alice(["quotaState": "near", "quota": 0.9]), frank()] + [
                canonicalRow("carol@example.com"),
                canonicalRow("dana@example.com", ["quotaState": "spent"]),
            ])
        XCTAssertEqual(fleet.governingPool(.emptier, now: fixtureNow)?.provider, .codex)
        let state = PollState.loaded(fleet)
        XCTAssertEqual(state.capacityTintKind(awake: false, cupPool: .emptier), .template)
        XCTAssertEqual(
            state.tooltipSentence(cupPool: .emptier, now: fixtureNow),
            "Claude · 0 of 2 accounts ready · 1 near their limit · next in 3d\n"
                + "Codex · 0 of 2 accounts ready · next in 3d 4h · shown on the icon")
    }

    /// A pool with no enabled account is out of the count and out of the cup, and a pin to
    /// it falls through to the pool that has one.
    func testAPoolWithNoEnabledAccountDropsOut() throws {
        let fleet = try decodeFleet(
            try claudeThreeOfFour() + [
                canonicalRow("bob@example.com/codex", ["disabled": true]),
                canonicalRow("carol@example.com", ["disabled": true]),
            ])
        let codex = try XCTUnwrap(fleet.providerPools(now: fixtureNow)?.last)
        XCTAssertEqual(codex.provider, .codex)
        XCTAssertEqual(codex.enabledCount, 0)
        XCTAssertEqual(fleet.governingPool(.codex, now: fixtureNow)?.provider, .anthropic)

        // The menu-bar count shows the one pool left. A panel scene cannot draw it, so this
        // is where it is pinned.
        let state = PollState.loaded(fleet)
        XCTAssertEqual(state.countsLabel, "3/4")
        XCTAssertEqual(state.countsParts, [PollState.CountsPart(text: "3/4", isNear: false)])
    }

    /// Rows held back by a parked group are parked, exactly as disabled rows are: a pool of
    /// them has nothing enabled, whatever each row's own `disabled` says.
    func testAPoolWhoseRowsAreAllGroupParkedDropsOut() throws {
        let fleet = try decodeFleet(try claudeThreeOfFour() + nightParkedCodexRows())
        let codex = try XCTUnwrap(fleet.providerPools(now: fixtureNow)?.last)
        XCTAssertEqual(codex.provider, .codex)
        XCTAssertEqual(codex.enabledCount, 0)
        XCTAssertEqual(codex.parkedCount, 2)
        XCTAssertNil(codex.readyFraction)
        XCTAssertNil(codex.countsText)
        XCTAssertNil(codex.nextIn)
        XCTAssertEqual(fleet.governingPool(.emptier, now: fixtureNow)?.provider, .anthropic)
        XCTAssertEqual(fleet.governingPool(.codex, now: fixtureNow)?.provider, .anthropic)

        let state = PollState.loaded(fleet)
        XCTAssertEqual(state.countsLabel, "3/4")
        XCTAssertEqual(state.countsNearHalves, [false])
        for pin in MenuBarCupPool.allCases {
            XCTAssertEqual(state.capacityFraction(cupPool: pin), 0.75, "\(pin)")
        }
        XCTAssertEqual(
            state.tooltipSentence(cupPool: .emptier, now: fixtureNow),
            "Claude · 3 of 4 accounts ready · shown on the icon\nCodex · 2 parked")
    }

    /// In a fleet holding both providers, Settings can pin the cup to one pool. The count
    /// beside it shows both pools under every pin.
    func testAPinnedPoolGovernsTheCup() throws {
        let fleet = try decodeFleet(
            try claudeThreeOfFour() + [
                canonicalRow("bob@example.com/codex", ["quotaState": "spent"]),
                canonicalRow("carol@example.com"),
            ])
        let state = PollState.loaded(fleet)
        XCTAssertEqual(state.capacityFraction(cupPool: .emptier), 0.0)
        XCTAssertEqual(state.capacityFraction(cupPool: .codex), 0.0)
        XCTAssertEqual(state.capacityFraction(cupPool: .claude), 0.75)
        // The count reads no pin at all: it is the same under every one.
        XCTAssertEqual(state.countsLabel, "3/4 · 0/2")
    }

    /// The tooltip gives each pool its own line, says which one the cup shows, and puts what
    /// the Mac is doing on a line of its own.
    func testTheTooltipHasOneLinePerPool() throws {
        let state = PollState.loaded(try decodeFleet(sceneTwentyFourRows()))
        XCTAssertEqual(
            state.menuBarToolTip(
                awake: true, showRunningTools: false, knocks: 0, cupPool: .emptier, now: fixtureNow),
            "Claude · 0 of 2 accounts ready · next in 2h 39m · shown on the icon\n"
                + "Codex · 1 of 2 accounts ready\nKeeping this Mac awake")
        XCTAssertEqual(
            state.menuBarToolTip(
                awake: true, showRunningTools: false, knocks: 0, cupPool: .codex, now: fixtureNow),
            "Claude · 0 of 2 accounts ready · next in 2h 39m\n"
                + "Codex · 1 of 2 accounts ready · shown on the icon\nKeeping this Mac awake")
        XCTAssertEqual(
            state.menuBarToolTip(
                awake: false, showRunningTools: false, knocks: 0, cupPool: .emptier,
                now: fixtureNow),
            "Claude · 0 of 2 accounts ready · next in 2h 39m · shown on the icon\n"
                + "Codex · 1 of 2 accounts ready")
        let knocked = state.menuBarToolTip(
            awake: false, showRunningTools: false, knocks: 1, cupPool: .emptier, now: fixtureNow)
        XCTAssertEqual(
            knocked.components(separatedBy: "\n").first,
            try XCTUnwrap(PeerAdmission.knockBarSentence(count: 1)))

        let codexOnly = PollState.loaded(try decodeFleet(codexOnlyRows()))
        XCTAssertEqual(
            codexOnly.menuBarToolTip(
                awake: false, showRunningTools: false, knocks: 0, cupPool: .emptier,
                now: fixtureNow),
            "2 of 4 Codex accounts ready")
    }

    /// A mixed fleet's last line is what the Mac is doing: the running tools, then
    /// keep-awake, under the waiting Mac's sentence and the pool lines.
    func testAMixedFleetsActivityLineCountsRunningToolsBeforeKeepAwake() throws {
        let accounts = try decodeFleet(sceneTwentyFourRows()).accounts
        let state = PollState.loaded(
            Fleet(
                accounts: accounts,
                sessions: [session(running: true), session(running: true), session(running: false)],
                sessionsSupported: true))
        XCTAssertEqual(
            state.menuBarToolTip(
                awake: true, showRunningTools: true, knocks: 1, cupPool: .emptier, now: fixtureNow),
            "1 Mac is asking to connect. Open the Peers tab to answer it.\n"
                + "Claude · 0 of 2 accounts ready · next in 2h 39m · shown on the icon\n"
                + "Codex · 1 of 2 accounts ready\n"
                + "2 tools running · Keeping this Mac awake")
    }

    /// A pool with nothing ready says when it next frees: the soonest still-ahead return of
    /// its enabled rows, each row's latest held reset, or a Codex row's own hold end when
    /// nothing is held. It reads the way every other countdown on the panel reads.
    func testNextInIsTheSoonestHoldOrCodexReturn() throws {
        func codex(_ rows: [[String: Any]]) throws -> PoolCapacity {
            PoolCapacity(provider: .codex, accounts: try decodeFleet(rows).accounts, now: fixtureNow)
        }
        func claude(_ rows: [[String: Any]]) throws -> PoolCapacity {
            PoolCapacity(
                provider: .anthropic, accounts: try decodeFleet(rows).accounts, now: fixtureNow)
        }
        let carol = try canonicalRow("carol@example.com")
        let hal = try canonicalRow("hal@example.com")

        let both = try codex([carol, hal])
        XCTAssertEqual(both.nextReturnAtMs, 1_767_228_420_000)
        XCTAssertEqual(both.nextIn, "next in 47m")

        let carolAlone = try codex([carol])
        XCTAssertEqual(carolAlone.nextReturnAtMs, 1_767_499_200_000)
        XCTAssertEqual(carolAlone.nextIn, "next in 3d 4h")

        let withBob = try codex([carol, hal, try canonicalRow("bob@example.com/codex")])
        XCTAssertNil(withBob.nextReturnAtMs)
        XCTAssertNil(withBob.nextIn)

        let aliceSpent = try claude([canonicalRow("alice@example.com", spentOn5h)])
        XCTAssertEqual(aliceSpent.nextReturnAtMs, 1_767_235_140_000)
        XCTAssertEqual(aliceSpent.nextIn, "next in 2h 39m")

        // A hold's end counts on a Codex row only.
        var freeNoHold = spentOn5h
        freeNoHold["held"] = [Any]()
        freeNoHold["freeAtMs"] = 1_767_235_140_000
        let claudeFree = try claude([canonicalRow("alice@example.com", freeNoHold)])
        XCTAssertNil(claudeFree.nextReturnAtMs)
        XCTAssertNil(claudeFree.nextIn)

        // Held by both windows, the row is back at the LATER reset.
        var bothWindows = spentOn5h
        bothWindows["held"] = [
            ["window": "5h", "minutesUntilReset": 159, "resetAtMs": 1_767_235_140_000],
            ["window": "7d", "minutesUntilReset": 4320, "resetAtMs": 1_767_484_800_000],
        ]
        let later = try claude([canonicalRow("alice@example.com", bothWindows)])
        XCTAssertEqual(later.nextReturnAtMs, 1_767_484_800_000)
        XCTAssertEqual(later.nextIn, "next in 3d")

        // Under 30 seconds the return is known and no countdown prints.
        let almost = try codex([canonicalRow("hal@example.com", ["freeAtMs": 1_767_225_620_000])])
        XCTAssertEqual(almost.nextReturnAtMs, 1_767_225_620_000)
        XCTAssertNil(almost.nextIn)
    }

    // MARK: - Builders

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

    /// A Claude pool of 0 of 2: alice spent on 5h, frank spent on the week.
    private func claudeNoneOfTwo() throws -> [[String: Any]] {
        [try alice(spentOn5h), try frank()]
    }

    /// A Claude pool of 1 of 2: alice ready, frank spent.
    private func claudeOneOfTwo() throws -> [[String: Any]] {
        [try alice(), try frank()]
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

    /// `row` with its `usage.today` priced at `cost`, every request of it priced.
    private func pricedToday(_ row: [String: Any], cost: Double) throws -> [String: Any] {
        var row = row
        var usage = try XCTUnwrap(row["usage"] as? [String: Any])
        var today = try XCTUnwrap(usage["today"] as? [String: Any])
        today["costUsd"] = cost
        today["unpricedRequests"] = 0
        usage["today"] = today
        row["usage"] = usage
        return row
    }

    /// All ten canonical rows: alice, and nine Codex accounts in every state.
    private func canonicalFleet() throws -> Fleet {
        let rows = try [
            "alice@example.com", "bob@example.com/codex", "carol@example.com", "dana@example.com",
            "erin@example.com/codex", "gil@example.com", "hal@example.com", "ivy@example.com",
            "jo@example.com", "kim@example.com",
        ].map { try canonicalRow($0) }
        let fleet = try decodeFleet(rows)
        XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
        return fleet
    }

    /// The keys an older server's Codex window refusal carries over canonical carol: gate
    /// `rejected`, nothing that a newer server adds, and her held week left in place.
    private let rejectedFullWindow: [String: Any] = [
        "gate": "rejected", "freeAtMs": NSNull(), "secondsUntilFree": NSNull(),
        "limitReachedType": NSNull(), "credits": NSNull(), "quotaWindows": NSNull(),
        "sevenDayWindowMinutes": NSNull(),
    ]

    /// The 403 hold over canonical bob: held for 90 seconds from `fixtureNow`.
    private let forbiddenHold: [String: Any] = [
        "gate": "refused", "freeAtMs": 1_767_225_690_000,
        "rateLimitedUntilMs": 1_767_225_690_000, "secondsUntilFree": 90,
    ]

    private func account(_ row: [String: Any]) throws -> Account {
        let fleet = try decodeFleet([row])
        XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
        return try XCTUnwrap(fleet.accounts.first)
    }
}
