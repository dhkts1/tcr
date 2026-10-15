import XCTest

@testable import TcrBarCore

final class CodexMeterReservationTests: XCTestCase {
    func testMeterLabelsReserveReportedWindowsEvenWhenTheirRowsAreHidden() throws {
        let fleet = try fixture()
        let all = AccountLineFit.panelLabels(fleet.accounts)
        // Visibility (spent, collapsed or kept open) is not an input to this reservation.
        XCTAssertEqual(all, ["5h", "7d", "30d"])
    }

    private func fixture() throws -> Fleet {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Fixtures/codex-status.json")
        let rows = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
        let claude = try XCTUnwrap(rows.first { $0["provider"] as? String != "codex" })
        var codex = try XCTUnwrap(rows.first { $0["name"] as? String == "bob@example.com/codex" })
        codex["sevenDayWindowMinutes"] = 43_200
        return try Fleet.decode(JSONSerialization.data(withJSONObject: [claude, codex]))
    }
}
