import XCTest

@testable import TcrBarCore

/// A Codex quota row's label follows its window's length, and a Claude row's
/// never moves.
///
/// A ChatGPT plan reports how long each of its windows is, and the length is not
/// always five hours and seven days: a plan with a 30-day window labelled `7d`
/// would tell the operator the wrong reset horizon on every row. When the length
/// is not on the wire, the label stays the fixed one the panel has always drawn.
///
/// The canonical rows come from `apps/macos/Tests/Fixtures/codex-status.json`.
/// Account names are obviously fake: this repository is public.
final class QuotaWindowLabelTests: XCTestCase {

    func testTheLabelFollowsTheWindowLength() {
        XCTAssertEqual(QuotaFormat.windowLabel(minutes: 300), "5h")
        XCTAssertEqual(QuotaFormat.windowLabel(minutes: 10080), "7d")
        XCTAssertEqual(QuotaFormat.windowLabel(minutes: 43200), "30d")
        XCTAssertEqual(QuotaFormat.windowLabel(minutes: 90), "90m")
        XCTAssertEqual(QuotaFormat.windowLabel(minutes: 60), "1h")
        XCTAssertEqual(QuotaFormat.windowLabel(minutes: 1440), "1d")
    }

    func testAnAbsentLengthKeepsTheFixedLabel() throws {
        let bob = try canonical("bob@example.com/codex")
        XCTAssertNil(bob.fiveHourWindowMinutes, "the fixture's bob carries no 5-hour length")
        XCTAssertEqual(bob.windowLabel(for: .fiveHour), "5h")
        XCTAssertEqual(bob.windowLabel(for: .sevenDay), "7d")

        let monthly = try canonical("bob@example.com/codex", ["sevenDayWindowMinutes": 43200])
        XCTAssertEqual(monthly.windowLabel(for: .sevenDay), "30d")

        let alice = try canonical("alice@example.com", ["sevenDayWindowMinutes": 43200])
        XCTAssertEqual(alice.provider, .anthropic)
        XCTAssertEqual(
            alice.windowLabel(for: .sevenDay), "7d",
            "a Claude row keeps its fixed label whatever length its row carries")
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

/// The canonical row, decoded, and failing the test if it does not decode.
private func canonical(_ name: String, _ overrides: [String: Any] = [:]) throws -> Account {
    let fleet = try decodeFleet([try canonicalRow(name, overrides)])
    XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
    return try XCTUnwrap(fleet.accounts.first)
}
