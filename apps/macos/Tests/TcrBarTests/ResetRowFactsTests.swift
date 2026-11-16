import XCTest

@testable import TcrBarCore

/// Option B's reset row and badge (Gil, 2026-11-16,
/// `docs/design/codex-weekly-reset-mockup.html`): the words a card's row and a
/// collapsed line's badge draw, from what the server says an account holds.
///
/// Rows are the canonical ones in `apps/macos/Tests/Fixtures/codex-status.json`, with
/// `resetCredits` laid over them. Names are fake: this repository is public.
final class ResetRowFactsTests: XCTestCase {
    private let utc = TimeZone(identifier: "UTC")!
    /// 2026-11-30 22:37 UTC.
    private let nov30: Int64 = 1_796_078_220_000

    func testThreeResetsNameTheFirstExpiry() throws {
        let bob = try codex(["availableCount": 3, "earliestExpiryMs": nov30])
        let row = try XCTUnwrap(ResetRowFacts.row(bob, timeZone: utc))
        XCTAssertEqual(row.text, "3 weekly · first expires Nov 30")
        XCTAssertEqual(
            row.help,
            "3 banked weekly resets: each refills the 7-day window now. The first can be used "
                + "until Nov 30, 22:37. Reset spends one, after you confirm.")
        XCTAssertEqual(ResetRowFacts.badge(bob), "↻3")
        XCTAssertTrue(ResetRowFacts.canRedeem(bob))
    }

    func testOneResetIsSingularInItsHelp() throws {
        let bob = try codex(["availableCount": 1, "earliestExpiryMs": nov30])
        let row = try XCTUnwrap(ResetRowFacts.row(bob, timeZone: utc))
        XCTAssertEqual(row.text, "1 weekly · first expires Nov 30")
        XCTAssertTrue(row.help.hasPrefix("1 banked weekly reset: "), row.help)
    }

    /// The date is printed in the Mac's own clock: 22:37 UTC on Nov 30 is Dec 1 in Tokyo.
    func testTheExpiryIsTheLocalDay() throws {
        let bob = try codex(["availableCount": 2, "earliestExpiryMs": nov30])
        let tokyo = try XCTUnwrap(TimeZone(identifier: "Asia/Tokyo"))
        XCTAssertEqual(
            ResetRowFacts.row(bob, timeZone: tokyo)?.text, "2 weekly · first expires Dec 1")
    }

    func testNoExpirySaysTheCountAlone() throws {
        let bob = try codex(["availableCount": 2])
        XCTAssertEqual(ResetRowFacts.row(bob, timeZone: utc)?.text, "2 weekly")
    }

    /// No reset draws no row and no badge, so the card keeps its height; but the bank is
    /// still known, which is what reserves the badge's room on every line.
    func testZeroDrawsNothingButIsKnown() throws {
        let bob = try codex(["availableCount": 0])
        XCTAssertNil(ResetRowFacts.row(bob))
        XCTAssertNil(ResetRowFacts.badge(bob))
        XCTAssertFalse(ResetRowFacts.canRedeem(bob))
        XCTAssertTrue(ResetRowFacts.carriesBank(bob))
    }

    /// A server that has not said is unknown, never zero: no row, no badge, no reserve.
    func testAnAbsentBankIsUnknown() throws {
        let bob = try account("bob@example.com/codex", [:])
        XCTAssertNil(ResetRowFacts.bank(bob))
        XCTAssertNil(ResetRowFacts.row(bob))
        XCTAssertNil(ResetRowFacts.badge(bob))
        XCTAssertFalse(ResetRowFacts.carriesBank(bob))
    }

    /// A Claude row carrying Codex's key draws nothing: its resets are not ChatGPT's.
    func testAClaudeRowDoesNotReadCodexResets() throws {
        let alice = try account(
            "alice@example.com", ["resetCredits": ["availableCount": 3, "earliestExpiryMs": nov30]])
        XCTAssertNil(ResetRowFacts.row(alice))
        XCTAssertNil(ResetRowFacts.badge(alice))
        XCTAssertFalse(ResetRowFacts.carriesBank(alice))
    }

    // MARK: - Fixtures

    private func codex(_ credits: [String: Any]) throws -> Account {
        try account("bob@example.com/codex", ["resetCredits": credits])
    }

    private func account(_ name: String, _ overrides: [String: Any]) throws -> Account {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // TcrBarTests
            .deletingLastPathComponent()  // Tests
            .appendingPathComponent("Fixtures/codex-status.json")
        let rows = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
        var row = try XCTUnwrap(rows.first { $0["name"] as? String == name }, "no \(name)")
        for (key, value) in overrides { row[key] = value }
        let fleet = try Fleet.decode(JSONSerialization.data(withJSONObject: [row]))
        XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
        return try XCTUnwrap(fleet.accounts.first)
    }
}
