import XCTest

@testable import TcrBarCore

/// The Codex keys of `tcr status --json`, read from the canonical fixture the Rust renderer is
/// proved against, and both directions of compatibility: an older decoder reading a new
/// server's rows, and this decoder reading an older server's.
final class CodexDecodeTests: XCTestCase {

    /// Ten rows, alice the one Claude row among them, and not one of them unreadable.
    func testEveryCanonicalRowDecodes() throws {
        let fleet = try canonicalFleet()

        XCTAssertEqual(fleet.unreadable, [])
        XCTAssertEqual(
            fleet.accounts.map(\.name),
            [
                "alice@example.com", "bob@example.com/codex", "carol@example.com",
                "dana@example.com", "erin@example.com/codex", "gil@example.com",
                "hal@example.com", "ivy@example.com", "jo@example.com", "kim@example.com",
            ])
        XCTAssertEqual(
            fleet.accounts.map(\.provider), [.anthropic] + Array(repeating: Provider.codex, count: 9))
    }

    /// Every Codex key reaches its field, each on the row that carries it, and stays absent
    /// on the rows that do not.
    func testEveryCodexKeyDecodesOnItsRow() throws {
        let fleet = try canonicalFleet()

        // The two fleet-wide facts ride on every Codex row, identical on each.
        let codexRows = fleet.accounts.filter { $0.provider == .codex }
        XCTAssertEqual(codexRows.count, 9)
        let traffic = CodexTraffic(
            refused10m: 12, lastRefusal: "usage_limit_reached", lastRefusalAtMs: 1_767_225_560_000,
            unpooled10m: 3)
        for account in codexRows {
            XCTAssertEqual(account.codexRoute, CodexRoute(pooled: true), account.name)
            XCTAssertEqual(account.codexTraffic, traffic, account.name)
        }

        let bob = try row("bob@example.com/codex", in: fleet)
        XCTAssertEqual(bob.quotaWindows, [.sevenDay])
        XCTAssertNil(bob.fiveHourWindowMinutes)
        XCTAssertEqual(bob.sevenDayWindowMinutes, 10_080)
        XCTAssertNil(bob.freeAtMs)
        XCTAssertNil(bob.credits)
        XCTAssertNil(bob.limitReachedType)
        XCTAssertNil(bob.loginError)
        XCTAssertNil(bob.codexHome)
        XCTAssertFalse(bob.spendControlReached)
        let bobWeek = try XCTUnwrap(bob.usage?.sevenDayWindow)
        XCTAssertEqual(bobWeek.outputTokens, 1_100_000)
        XCTAssertEqual(bobWeek.reasoningTokens, 420_000)
        XCTAssertEqual(bobWeek.since, 1_766_966_400_000)
        XCTAssertEqual(bob.usage?.today.reasoningTokens, 9_000)
        XCTAssertNil(bob.usage?.window)

        let carol = try row("carol@example.com", in: fleet)
        XCTAssertEqual(carol.gate, .unknown("seven-day"))
        XCTAssertEqual(carol.freeAtMs, 1_767_499_200_000)
        XCTAssertEqual(carol.limitReachedType, "rate_limit_reached")
        XCTAssertEqual(carol.credits, CodexCredits(hasCredits: true, unlimited: false, balance: "12.50"))

        let dana = try row("dana@example.com", in: fleet)
        XCTAssertEqual(dana.quotaWindows, [.fiveHour, .sevenDay])
        XCTAssertEqual(dana.fiveHourWindowMinutes, 300)
        XCTAssertEqual(dana.sevenDayWindowMinutes, 10_080)
        XCTAssertEqual(dana.usage?.window?.reasoningTokens, 55_000)
        let danaWeek = try XCTUnwrap(dana.usage?.sevenDayWindow)
        XCTAssertEqual(danaWeek.outputTokens, 640_000)
        XCTAssertEqual(danaWeek.reasoningTokens, 180_000)

        let erin = try row("erin@example.com/codex", in: fleet)
        XCTAssertEqual(erin.loginError, "refresh_token_reused")
        XCTAssertEqual(erin.codexHome, "/Users/example/.codex")
        XCTAssertEqual(erin.status, "error")
        // Her 5h window reset with nothing learned since: 0, and no reset.
        XCTAssertEqual(erin.fiveHour, 0.0)
        XCTAssertNil(erin.fiveHourResetAtMs)

        let gil = try row("gil@example.com", in: fleet)
        XCTAssertEqual(gil.gate, .rejected)
        XCTAssertTrue(gil.spendControlReached)
        XCTAssertEqual(gil.limitReachedType, "rate_limit_reached")

        let hal = try row("hal@example.com", in: fleet)
        XCTAssertEqual(hal.gate, .unknown("hold"))
        XCTAssertEqual(hal.limitReachedType, "rate_limit_reached")
        // A Codex hold is throttled, and never longer than an hour: 47 minutes.
        XCTAssertEqual(hal.status, "throttled")
        XCTAssertEqual(hal.freeAtMs, 1_767_228_420_000)

        let ivy = try row("ivy@example.com", in: fleet)
        XCTAssertEqual(ivy.gate, .unknown("five-hour"))
        XCTAssertEqual(ivy.freeAtMs, 1_767_228_720_000)
        XCTAssertEqual(ivy.usage?.window?.since, 1_767_210_720_000)

        let jo = try row("jo@example.com", in: fleet)
        XCTAssertEqual(jo.quotaWindows, [])
        XCTAssertEqual(jo.credits, CodexCredits(hasCredits: true, unlimited: true))
        XCTAssertNil(jo.usage?.sevenDayWindow)
        XCTAssertNil(jo.quota)

        let kim = try row("kim@example.com", in: fleet)
        XCTAssertEqual(kim.gate, .unknown("reserved"))
        XCTAssertEqual(kim.reservedGroups, ["research"])

        let alice = try row("alice@example.com", in: fleet)
        assertCarriesNoCodexFact(alice)
        XCTAssertNil(alice.freeAtMs)
    }

    /// Each row decoded twice, with its Codex keys and with them stripped. Stripped, it is a
    /// row from a server that predates Codex accounts, and it carries no Codex fact. With the
    /// keys, every field an older build reads is exactly what it was without them.
    func testAnOldDecoderIgnoresEveryNewKey() throws {
        let rows = try canonicalRows()
        XCTAssertEqual(rows.count, 10)
        let decoder = JSONDecoder()

        for row in rows {
            let name = try XCTUnwrap(row["name"] as? String)
            let withKeys = try decoder.decode(
                Account.self, from: JSONSerialization.data(withJSONObject: row))
            let without = try decoder.decode(
                Account.self, from: JSONSerialization.data(withJSONObject: withoutCodexKeys(row)))

            assertCarriesNoCodexFact(without)
            XCTAssertEqual(legacyAccount(withKeys), legacyAccount(without), name)
        }
    }

    /// Every row's provider before the key existed was Anthropic, so a row without it is one.
    func testAbsentProviderDecodesAsAnthropic() throws {
        let rows = try canonicalRows()
        XCTAssertNil(rows.first?["provider"], "precondition: alice's row has no provider key")
        let fleet = try canonicalFleet()
        let alice = fleet.accounts.first { $0.name == "alice@example.com" }
        XCTAssertEqual(alice?.provider, .anthropic, "unreadable: \(fleet.unreadable)")

        XCTAssertEqual(try Provider.decoding(nil), .anthropic)
        XCTAssertEqual(try Provider.decoding("anthropic"), .anthropic)
        XCTAssertEqual(try Provider.decoding("codex"), .codex)
        XCTAssertEqual(Provider.codex.token, "codex")
        XCTAssertEqual(Provider.anthropic.token, "anthropic")
    }

    /// A provider this build cannot name costs its row, and the row is listed as unreadable
    /// with that reason. The alternative is counting another provider's account as Claude
    /// capacity.
    func testAnUnknownProviderIsUnreadableAndNeverCapacity() throws {
        let contract = try Data(contentsOf: RealWorldDecodeTests.contractFixtureURL)
        let before = try Fleet.decode(contract)
        XCTAssertEqual(before.accounts.first?.name, "alice@example.com")
        XCTAssertEqual(before.readyCount, 1, "precondition: alice is the contract's one ready row")

        let after = try Fleet.decode(Self.setting("provider", to: "gemini", onRow: 0, of: contract))

        XCTAssertEqual(
            after.unreadable,
            [Fleet.UnreadableRow(index: 0, message: "provider 'gemini' is newer than this TcrBar")])
        XCTAssertFalse(after.accounts.contains { $0.name == "alice@example.com" })
        XCTAssertEqual(after.readyCount, 0)
        XCTAssertEqual(after.enabledCount, before.enabledCount - 1)
    }

    /// `[]` is the plan saying it has no window, and absent is not knowing: two facts, never
    /// collapsed into one.
    func testEmptyQuotaWindowsIsNotAbsent() throws {
        let fleet = try canonicalFleet()

        XCTAssertEqual(try row("jo@example.com", in: fleet).quotaWindows, [])
        XCTAssertNil(try row("alice@example.com", in: fleet).quotaWindows)
        XCTAssertEqual(try row("bob@example.com/codex", in: fleet).quotaWindows, [.sevenDay])
        XCTAssertEqual(
            try row("dana@example.com", in: fleet).quotaWindows, [.fiveHour, .sevenDay])
    }

    /// A window name a newer server invents is dropped, never fatal. A list of nothing but
    /// such names is unknown, not a plan with no window.
    func testAnUnknownWindowNameIsDroppedNotFatal() throws {
        let fixture = try CanonicalFixtures.data("codex-status.json")
        XCTAssertEqual(try canonicalRows()[3]["name"] as? String, "dana@example.com")

        let mixed = try Fleet.decode(
            Self.setting(
                "quotaWindows", to: ["fiveHour", "thirtyDay", "sevenDay"], onRow: 3, of: fixture))
        XCTAssertEqual(mixed.unreadable, [])
        XCTAssertEqual(try row("dana@example.com", in: mixed).quotaWindows, [.fiveHour, .sevenDay])

        let unknownOnly = try Fleet.decode(
            Self.setting("quotaWindows", to: ["thirtyDay"], onRow: 3, of: fixture))
        XCTAssertEqual(unknownOnly.unreadable, [])
        XCTAssertNil(try row("dana@example.com", in: unknownOnly).quotaWindows)

        XCTAssertNil(QuotaWindowKind.decoding(nil))
        XCTAssertEqual(QuotaWindowKind.decoding([]), [])
    }

    /// A Codex row in any state makes the fleet hold Codex. Disabled counts too: the
    /// contract's one Codex row is disabled.
    func testFleetHoldsCodexAndHoldsAnthropic() throws {
        let canonical = try canonicalFleet()
        XCTAssertTrue(canonical.holdsCodex)
        XCTAssertTrue(canonical.holdsAnthropic)

        let codexOnly = Fleet(accounts: canonical.accounts.filter { $0.provider == .codex })
        XCTAssertEqual(codexOnly.accounts.count, 9)
        XCTAssertTrue(codexOnly.holdsCodex)
        XCTAssertFalse(codexOnly.holdsAnthropic)

        let contract = try Fleet.decode(Data(contentsOf: RealWorldDecodeTests.contractFixtureURL))
        let contractCodex = try XCTUnwrap(contract.accounts.first { $0.provider == .codex })
        XCTAssertTrue(contractCodex.disabled, "precondition: the contract's Codex row is disabled")
        XCTAssertTrue(contract.holdsCodex)
        XCTAssertTrue(contract.holdsAnthropic)

        let anthropicOnly = Fleet(accounts: contract.accounts.filter { $0.provider == .anthropic })
        XCTAssertEqual(anthropicOnly.accounts.count, 4)
        XCTAssertFalse(anthropicOnly.holdsCodex)
        XCTAssertTrue(anthropicOnly.holdsAnthropic)

        let empty = Fleet(accounts: [])
        XCTAssertFalse(empty.holdsCodex)
        XCTAssertFalse(empty.holdsAnthropic)
    }

    /// The row decoder is written by hand now, and this is what stops it dropping a key it
    /// always read: each of alice's fields, one by one, then three rows for what alice lacks.
    func testEveryPreExistingKeyStillReachesItsField() throws {
        let fleet = try canonicalFleet()
        let alice = try row("alice@example.com", in: fleet)

        XCTAssertEqual(alice.name, "alice@example.com")
        XCTAssertEqual(alice.priority, 0)
        XCTAssertEqual(alice.status, "active")
        XCTAssertFalse(alice.disabled)
        XCTAssertEqual(alice.quota, 0.62)
        XCTAssertEqual(alice.quotaState, .ok)
        XCTAssertEqual(alice.fiveHour, 0.47)
        XCTAssertEqual(alice.sevenDay, 0.2)
        XCTAssertEqual(alice.sevenDayOi, 0.62)
        XCTAssertEqual(alice.fiveHourState, .ok)
        XCTAssertEqual(alice.sevenDayState, .ok)
        XCTAssertEqual(alice.sevenDayOiState, .ok)
        XCTAssertEqual(alice.fiveHourResetAtMs, 1_767_235_140_000)
        XCTAssertEqual(alice.sevenDayResetAtMs, 1_767_571_200_000)
        XCTAssertEqual(alice.sevenDayOiResetAtMs, 1_767_571_200_000)
        XCTAssertEqual(alice.held, [])
        XCTAssertEqual(alice.requests, 102)
        XCTAssertEqual(alice.inputTokens, 8_000_000)
        XCTAssertEqual(alice.outputTokens, 31_860)
        XCTAssertEqual(alice.cacheReadTokens, 6_000_000)
        XCTAssertEqual(alice.cacheHitRatio, 0.75)
        XCTAssertEqual(alice.usage?.today.costUsd, 14.1657)
        XCTAssertEqual(alice.usage?.window?.since, 1_767_217_140_000)
        XCTAssertEqual(alice.probeStatus, .ok)
        XCTAssertNil(alice.probeError)
        XCTAssertEqual(alice.lastStreamError, "overloaded_error")
        XCTAssertEqual(alice.streamErrorCount, 2)
        XCTAssertEqual(alice.source, .live)
        XCTAssertEqual(alice.serverSha, "abc1234")
        XCTAssertEqual(alice.serverDirty, false)
        XCTAssertEqual(alice.groups, ["orchard"])
        XCTAssertEqual(alice.reservedGroups, [])
        XCTAssertEqual(alice.parkedGroups, [])
        XCTAssertEqual(alice.controlAllowedGroups, [])
        XCTAssertEqual(alice.groupColors, ["orchard": "#30d158", "research": "#bf5af2"])
        XCTAssertEqual(alice.plan, "Max 20x")
        XCTAssertEqual(alice.organizationType, "claude_max")
        XCTAssertEqual(alice.rateLimitTier, "default_claude_max_20x")
        XCTAssertNil(alice.seatTier)
        XCTAssertEqual(alice.orgUuid, "11111111-1111-1111-1111-111111111111")
        XCTAssertEqual(alice.orgName, "Example Personal")
        XCTAssertEqual(alice.gate, .ok)

        XCTAssertEqual(try row("erin@example.com/codex", in: fleet).probeError, "refresh_token_reused")
        XCTAssertEqual(
            try row("carol@example.com", in: fleet).held,
            [HeldWindow(window: "7d", minutesUntilReset: 4_560, resetAtMs: 1_767_499_200_000)])
        let contract = try Fleet.decode(Data(contentsOf: RealWorldDecodeTests.contractFixtureURL))
        XCTAssertEqual(try row("bob@example.com", in: contract).seatTier, "team_tier_1")
    }

    /// A sum of two buckets keeps their reasoning; otherwise a summed bucket would read none.
    func testAddingTwoBucketsSumsTheirReasoningTokens() throws {
        let fleet = try canonicalFleet()
        let bob = try XCTUnwrap(try row("bob@example.com/codex", in: fleet).usage?.today)
        let dana = try XCTUnwrap(try row("dana@example.com", in: fleet).usage?.today)

        let sum = bob.adding(dana)

        XCTAssertEqual(sum.reasoningTokens, 79_000)
        XCTAssertEqual(sum.outputTokens, 282_000)
    }

    // MARK: - Helpers

    private func canonicalFleet() throws -> Fleet {
        try Fleet.decode(CanonicalFixtures.data("codex-status.json"))
    }

    private func canonicalRows() throws -> [[String: Any]] {
        try XCTUnwrap(
            JSONSerialization.jsonObject(with: CanonicalFixtures.data("codex-status.json"))
                as? [[String: Any]])
    }

    private func row(_ name: String, in fleet: Fleet) throws -> Account {
        try XCTUnwrap(fleet.accounts.first { $0.name == name }, "no row named \(name)")
    }

    /// `fixture` with one key of one row replaced: the committed bytes, varied in one fact,
    /// rather than a row typed out again.
    private static func setting(
        _ key: String, to value: Any, onRow index: Int, of fixture: Data
    ) throws -> Data {
        var rows = try XCTUnwrap(JSONSerialization.jsonObject(with: fixture) as? [[String: Any]])
        rows[index][key] = value
        return try JSONSerialization.data(withJSONObject: rows)
    }

    /// No Codex fact at all: what every row of a server without Codex accounts reads as.
    private func assertCarriesNoCodexFact(
        _ a: Account, file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertEqual(a.provider, .anthropic, a.name, file: file, line: line)
        XCTAssertNil(a.quotaWindows, a.name, file: file, line: line)
        XCTAssertNil(a.fiveHourWindowMinutes, a.name, file: file, line: line)
        XCTAssertNil(a.sevenDayWindowMinutes, a.name, file: file, line: line)
        XCTAssertNil(a.limitReachedType, a.name, file: file, line: line)
        XCTAssertNil(a.credits, a.name, file: file, line: line)
        XCTAssertNil(a.loginError, a.name, file: file, line: line)
        XCTAssertNil(a.codexHome, a.name, file: file, line: line)
        XCTAssertNil(a.codexRoute, a.name, file: file, line: line)
        XCTAssertNil(a.codexTraffic, a.name, file: file, line: line)
        XCTAssertNil(a.usage?.sevenDayWindow, a.name, file: file, line: line)
        XCTAssertFalse(a.spendControlReached, a.name, file: file, line: line)
        guard let usage = a.usage else { return }
        var buckets = [usage.today, usage.lastHour] + Array(usage.todayByModel.values)
        if let window = usage.window { buckets.append(window) }
        for bucket in buckets {
            XCTAssertEqual(bucket.reasoningTokens, 0, a.name, file: file, line: line)
        }
    }

    /// The eleven row keys a Codex account adds.
    private static let codexRowKeys = [
        "provider", "quotaWindows", "fiveHourWindowMinutes", "sevenDayWindowMinutes",
        "limitReachedType", "spendControlReached", "credits", "loginError", "codexHome",
        "codexRoute", "codexTraffic",
    ]

    /// `row` as a server from before Codex accounts writes it: no Codex row key, no weekly
    /// usage window, and no reasoning figure in any bucket. `freeAtMs` is an older key than
    /// all of these, and stays.
    private func withoutCodexKeys(_ row: [String: Any]) -> [String: Any] {
        var row = row
        for key in Self.codexRowKeys { row[key] = nil }
        guard var usage = row["usage"] as? [String: Any] else { return row }
        usage["sevenDayWindow"] = nil
        for name in ["today", "window", "lastHour"] {
            guard var bucket = usage[name] as? [String: Any] else { continue }
            bucket["reasoningTokens"] = nil
            usage[name] = bucket
        }
        if let byModel = usage["todayByModel"] as? [String: [String: Any]] {
            usage["todayByModel"] = byModel.mapValues { bucket -> [String: Any] in
                var bucket = bucket
                bucket["reasoningTokens"] = nil
                return bucket
            }
        }
        row["usage"] = usage
        return row
    }

    /// `a` rebuilt from the fields that existed before Codex accounts, in the initializer's
    /// order. Every field added since takes its default, so two rows that differ only in what
    /// an older build cannot read compare equal.
    private func legacyAccount(_ a: Account) -> Account {
        Account(
            name: a.name, priority: a.priority, status: a.status, disabled: a.disabled,
            quota: a.quota, quotaState: a.quotaState, fiveHour: a.fiveHour,
            sevenDay: a.sevenDay, sevenDayOi: a.sevenDayOi, fiveHourState: a.fiveHourState,
            sevenDayState: a.sevenDayState, sevenDayOiState: a.sevenDayOiState,
            fiveHourResetAtMs: a.fiveHourResetAtMs, sevenDayResetAtMs: a.sevenDayResetAtMs,
            sevenDayOiResetAtMs: a.sevenDayOiResetAtMs, held: a.held, requests: a.requests,
            inputTokens: a.inputTokens, outputTokens: a.outputTokens,
            cacheReadTokens: a.cacheReadTokens, cacheHitRatio: a.cacheHitRatio,
            probeStatus: a.probeStatus, probeError: a.probeError,
            lastStreamError: a.lastStreamError, streamErrorCount: a.streamErrorCount,
            source: a.source, serverSha: a.serverSha, serverDirty: a.serverDirty,
            groups: a.groups, reservedGroups: a.reservedGroups, parkedGroups: a.parkedGroups,
            controlAllowedGroups: a.controlAllowedGroups, groupColors: a.groupColors,
            usage: a.usage.map(legacyUsage), plan: a.plan, organizationType: a.organizationType,
            rateLimitTier: a.rateLimitTier, seatTier: a.seatTier, orgUuid: a.orgUuid,
            orgName: a.orgName, gate: a.gate)
    }

    /// The four buckets that existed before the weekly window, each through ``legacyBucket``.
    private func legacyUsage(_ u: UsageRow) -> UsageRow {
        UsageRow(
            today: legacyBucket(u.today), window: u.window.map(legacyBucket),
            lastHour: legacyBucket(u.lastHour), todayByModel: u.todayByModel.mapValues(legacyBucket))
    }

    /// `t` without its reasoning figure, which no older build reads.
    private func legacyBucket(_ t: UsageTotals) -> UsageTotals {
        UsageTotals(
            requests: t.requests, inputTokens: t.inputTokens,
            cacheCreationTokens: t.cacheCreationTokens,
            cacheCreation1hTokens: t.cacheCreation1hTokens, cacheReadTokens: t.cacheReadTokens,
            outputTokens: t.outputTokens, costUsd: t.costUsd, unpricedRequests: t.unpricedRequests,
            since: t.since)
    }
}

/// The canonical Codex fixtures in `apps/macos/Tests/Fixtures`. They sit outside the test
/// target, and are read from this file's own path because the runner's working directory is
/// not the package.
enum CanonicalFixtures {
    static var repoRoot: URL {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { url = url.deletingLastPathComponent() }  // file, TcrBarTests, Tests, macos, apps
        return url
    }
    static func url(_ name: String) -> URL {
        repoRoot.appendingPathComponent("apps/macos/Tests/Fixtures/\(name)")
    }
    static func data(_ name: String) throws -> Data { try Data(contentsOf: url(name)) }
}
