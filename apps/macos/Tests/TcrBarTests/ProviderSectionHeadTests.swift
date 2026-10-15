import XCTest

@testable import TcrBarCore

/// The head over each provider's list on the Accounts tab, in a fleet holding both: the
/// provider's name and how many of its accounts lie beneath.
///
/// The head counts accounts and says nothing else. The strip above the list already counts
/// what is ready in each pool and when the next one is back, and each band heading says when
/// its first account returns; a third reading of one pool is the repetition the panel's
/// summary was cut down to avoid. Every account is a canonical row of
/// `apps/macos/Tests/Fixtures/codex-status.json`.
final class ProviderSectionHeadTests: XCTestCase {

    /// The title `SectionHead` uppercases, and the unit beside it: the provider's accounts,
    /// singular at one.
    func testAHeadCountsItsProvidersAccounts() throws {
        let claude = ProviderSectionHead(provider: .anthropic, accounts: 4)
        XCTAssertEqual(claude.title, "Claude")
        XCTAssertEqual(claude.unit, "· 4 accounts")
        let codex = ProviderSectionHead(provider: .codex, accounts: 1)
        XCTAssertEqual(codex.title, "Codex")
        XCTAssertEqual(codex.unit, "· 1 account")

        let fleet = try canonicalFleet()
        XCTAssertEqual(fleet.sectionHead(for: .anthropic)?.unit, "· 1 account")
        XCTAssertEqual(fleet.sectionHead(for: .codex)?.unit, "· 9 accounts")
    }

    /// An account in two groups draws a line under each, and is still one account: the head
    /// agrees with the strip, which gives it one segment.
    func testAnAccountInTwoGroupsIsCountedOnce() throws {
        let fleet = try decodeFleet([
            try canonicalRow("alice@example.com"),
            try canonicalRow("bob@example.com/codex"),
            try canonicalRow("ivy@example.com", ["groups": ["orchard", "research"]]),
        ])
        let codexLines = try XCTUnwrap(
            fleet.accountsTabSectionsByProvider(expandedGroups: [])?.last?.sections.flatMap(\.rows))
        XCTAssertEqual(
            codexLines.filter { $0.account.name == "ivy@example.com" }.count, 2,
            "positive control: ivy draws a line in each of her groups")

        XCTAssertEqual(fleet.sectionHead(for: .codex)?.unit, "· 2 accounts")
    }

    /// A pool with nothing enabled still has its accounts beneath the head; the strip is
    /// where it says they are parked.
    func testAPoolWithNothingEnabledStillCountsItsAccounts() throws {
        let disabled = try decodeFleet([
            try canonicalRow("alice@example.com"),
            try canonicalRow("bob@example.com/codex", ["disabled": true]),
            try canonicalRow("carol@example.com", ["disabled": true]),
        ])
        XCTAssertEqual(disabled.sectionHead(for: .codex)?.unit, "· 2 accounts")

        let groupParked: [String: Any] = [
            "groups": ["night"], "parkedGroups": ["night"], "gate": "parked", "freeAtMs": NSNull(),
        ]
        let parked = try decodeFleet([
            try canonicalRow("alice@example.com"),
            try canonicalRow("bob@example.com/codex", groupParked),
            try canonicalRow("carol@example.com", groupParked),
        ])
        XCTAssertEqual(parked.sectionHead(for: .codex)?.unit, "· 2 accounts")
        XCTAssertEqual(parked.sectionHead(for: .codex)?.spoken, "CODEX, 2 accounts")
    }

    /// What VoiceOver reads for the head: the uppercased title and the count, without the
    /// middle dot the unit draws.
    func testTheSpokenHead() throws {
        XCTAssertEqual(
            ProviderSectionHead(provider: .anthropic, accounts: 4).spoken, "CLAUDE, 4 accounts")
        XCTAssertEqual(ProviderSectionHead(provider: .codex, accounts: 1).spoken, "CODEX, 1 account")
        XCTAssertEqual(try canonicalFleet().sectionHead(for: .codex)?.spoken, "CODEX, 9 accounts")
    }
}

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

/// All ten canonical rows: alice (Claude, in orchard) and nine Codex rows.
private func canonicalFleet() throws -> Fleet {
    try decodeFleet(
        [
            "alice@example.com", "bob@example.com/codex", "carol@example.com", "dana@example.com",
            "erin@example.com/codex", "gil@example.com", "hal@example.com", "ivy@example.com",
            "jo@example.com", "kim@example.com",
        ].map { try canonicalRow($0) })
}
