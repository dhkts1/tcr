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
        XCTAssertEqual(row.text, "3 weekly · Nov 30")
        XCTAssertEqual(
            row.help,
            "3 weekly resets: refills the 7-day window. The first, use by Nov 30, 22:37.\n"
                + "Reset spends one, after you confirm.")
        XCTAssertEqual(ResetRowFacts.badge(bob), "↻3")
        XCTAssertTrue(ResetRowFacts.canRedeem(bob))
    }

    func testOneResetIsSingularInItsHelp() throws {
        let bob = try codex(["availableCount": 1, "earliestExpiryMs": nov30])
        let row = try XCTUnwrap(ResetRowFacts.row(bob, timeZone: utc))
        XCTAssertEqual(row.text, "1 weekly · Nov 30")
        XCTAssertTrue(row.help.hasPrefix("1 weekly reset: refills the 7-day window. Use by "), row.help)
    }

    /// The date is printed in the Mac's own clock: 22:37 UTC on Nov 30 is Dec 1 in Tokyo.
    func testTheExpiryIsTheLocalDay() throws {
        let bob = try codex(["availableCount": 2, "earliestExpiryMs": nov30])
        let tokyo = try XCTUnwrap(TimeZone(identifier: "Asia/Tokyo"))
        XCTAssertEqual(
            ResetRowFacts.row(bob, timeZone: tokyo)?.text, "2 weekly · Dec 1")
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
        XCTAssertNil(ResetRowFacts.held(bob))
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

    // MARK: - Claude

    /// `1 weekly · 2 Fable · Nov 30`: each kind held, in kind order, a kind with no
    /// reset left omitted, and the earliest use-by among them.
    func testAClaudeLineCountsEachKindHeld() throws {
        let alice = try claude([
            ["kind": "fable", "resetsLeft": 2, "usableNow": true, "endsAtMs": nov30],
            ["kind": "weekly", "resetsLeft": 1, "usableNow": true, "endsAtMs": nov30 + 86_400_000],
            ["kind": "full", "resetsLeft": 0, "usableNow": true, "endsAtMs": nov30 - 86_400_000],
            ["kind": "something-new", "resetsLeft": 5],
        ])
        let row = try XCTUnwrap(ResetRowFacts.row(alice, timeZone: utc))
        XCTAssertEqual(row.text, "1 weekly · 2 Fable · Nov 30")
        XCTAssertEqual(row.kinds, "1 weekly · 2 Fable")
        XCTAssertEqual(row.date, "Nov 30", "the date is drawn apart, so the card never cuts it")
        XCTAssertTrue(row.help.contains("2 Fable resets: gives back what Fable used"), row.help)
        XCTAssertEqual(ResetRowFacts.badge(alice), "↻3")
        XCTAssertEqual(ResetRowFacts.choices(alice), [.weekly, .fable])
        XCTAssertNil(ResetRowFacts.disabledReason(alice))
        XCTAssertNil(ResetRowFacts.unknownNote(alice))
        XCTAssertFalse(CodexCardFacts.canRedeemReset(alice), "a Claude reset is not tcr codex reset")
    }

    /// Unknown is not zero: an ineligible answer draws no row and no badge, and the 7d bar's
    /// hover says the count is unknown. An eligible answer holding nothing says nothing.
    func testAnIneligibleClaudeBankIsUnknownNotZero() throws {
        let surface = try account(
            "alice@example.com",
            ["bankedResets": ["eligible": false, "reason": "surface", "grants": [Any]()]])
        XCTAssertNil(ResetRowFacts.held(surface))
        XCTAssertNil(ResetRowFacts.row(surface))
        XCTAssertNil(ResetRowFacts.badge(surface))
        XCTAssertFalse(ResetRowFacts.canRedeem(surface))
        XCTAssertEqual(ResetRowFacts.unknownNote(surface), "banked resets unknown")
        let empty = try claude([])
        XCTAssertEqual(ResetRowFacts.held(empty), [])
        XCTAssertNil(ResetRowFacts.row(empty))
        XCTAssertNil(ResetRowFacts.unknownNote(empty))
        XCTAssertNil(ResetRowFacts.unknownNote(try account("alice@example.com", [:])))
    }

    func testTheButtonSaysWhyNoResetIsUsable() throws {
        let paused = try claude([["kind": "weekly", "resetsLeft": 1, "paused": true]])
        XCTAssertEqual(
            ResetRowFacts.disabledReason(paused), "Paused: no reset can be used right now.")
        let waiting = try claude([
            ["kind": "weekly", "resetsLeft": 1, "usableNow": false, "requiresLimit": true]
        ])
        XCTAssertEqual(
            ResetRowFacts.disabledReason(waiting), "Usable once this account reaches its limit.")
        let later = try claude([["kind": "weekly", "resetsLeft": 1, "usableNow": false]])
        XCTAssertEqual(ResetRowFacts.disabledReason(later), "Not usable yet.")
    }

    /// The confirm alert, word for word, as the spec writes it.
    func testTheConfirmationNamesTheKindTheWindowAndTheUseBy() throws {
        // A Friday 10:00 UTC weekly reset.
        let friday: Int64 = 1_796_983_200_000
        let alice = try account(
            "alice@example.com",
            [
                "sevenDay": 0.46, "fiveHour": 0.12, "sevenDayResetAtMs": friday,
                "bankedResets": [
                    "eligible": true,
                    "grants": [["kind": "weekly", "resetsLeft": 1, "usableNow": true, "endsAtMs": nov30]],
                ],
            ])
        let weekly = ResetRowFacts.confirmation(alice, kind: .weekly, timeZone: utc)
        XCTAssertEqual(weekly.message, "Use a weekly reset on alice@example.com?")
        XCTAssertEqual(
            weekly.informative,
            "Refills your 7-day limit now (46% used). Your weekly reset day stays Friday.\n"
                + "1 weekly reset left · use by Nov 30.")
        let full = ResetRowFacts.confirmation(alice, kind: .full, timeZone: utc)
        XCTAssertEqual(
            full.informative,
            "Refills your 5-hour and 7-day limits now (5h 12% · 7d 46% used). Your weekly reset "
                + "day stays Friday.")
    }

    func testTheControllerRunsTcrResetWithTheKindForClaudeAndCodexResetOtherwise() {
        let ref = AccountRef(name: "alice@example.com")
        XCTAssertEqual(
            CodexResetController.arguments(account: ref, key: "k", kind: .fable),
            ["reset", "alice@example.com", "--kind", "fable", "--idempotency-key", "k", "--yes"])
        XCTAssertEqual(
            CodexResetController.arguments(account: ref, key: "k", kind: nil),
            ["codex", "reset", "alice@example.com", "--idempotency-key", "k", "--yes"])
    }

    // MARK: - Fixtures

    private func claude(_ grants: [[String: Any]]) throws -> Account {
        try account("alice@example.com", ["bankedResets": ["eligible": true, "grants": grants]])
    }

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
