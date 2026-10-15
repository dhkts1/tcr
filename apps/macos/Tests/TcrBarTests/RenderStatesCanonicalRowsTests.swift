import XCTest

@testable import TcrBarCore

/// The render harness's canonical Codex rows.
///
/// A render must not need the test bundle, so the render harness keeps its own copy of the
/// canonical Codex status fixture. The first test holds that copy equal to the one the decode
/// tests read and the Rust renderer is proved against. It reads the copy as text, because the
/// harness is in the app target, which no test links.
///
/// The harness builds a scene's rows through `RenderFixtureRow`, in TcrBarCore, so the rest test
/// that builder directly, and pin as text that the harness calls it.
final class RenderStatesCanonicalRowsTests: XCTestCase {

    /// Compares text, line for line, as the acceptance check's `sed … | diff` does. Parsed, the
    /// two would compare `false` and `0` as equal numbers, and a row whose `false` became `0`
    /// is a row `JSONDecoder` refuses.
    func testTheRenderHarnessCarriesTheCanonicalRows() throws {
        let harness = try Self.harnessText()
        guard
            let open = harness.range(of: "canonicalCodexStatusJSON = #\"\"\""),
            let close = harness.range(of: "\"\"\"#", range: open.upperBound..<harness.endIndex)
        else {
            XCTFail(
                "the canonical rows have moved or been renamed in RenderStates.swift; "
                    + "update this test's anchors")
            return
        }

        let indent = "        "  // what seats each line of the copy inside the literal
        let lines = harness[open.upperBound..<close.lowerBound]
            .split(separator: "\n", omittingEmptySubsequences: false)
            .dropFirst()  // the rest of the opening line, after the anchor
            .dropLast()  // the closing line's indent, before `"""#`
        let embedded =
            lines.map { $0.hasPrefix(indent) ? $0.dropFirst(indent.count) : $0 }
            .joined(separator: "\n") + "\n"
        let fixture = try Self.fixtureText()
        XCTAssertEqual(embedded, fixture, Self.firstDifference(embedded, fixture))
    }

    /// What a Codex scene reads off a canonical row holds whenever the render runs: every
    /// absolute instant, at any depth of the row, sits as far from the render's `now` as it sits
    /// from the fixture's own clock, and a relative figure stays as the fixture wrote it.
    func testACanonicalRowKeepsEachInstantAsFarFromNowAsTheFixtureDoes() throws {
        let nowMs: Int64 = 1_800_000_000_000  // far from the fixture's own clock, so a missed shift shows
        let now = Date(timeIntervalSince1970: TimeInterval(nowMs) / 1000)
        let minute: Int64 = 60_000

        let carol = try Self.row("carol@example.com", now: now)  // frees in 3d 4h
        XCTAssertEqual(Self.instant(carol["freeAtMs"]), nowMs + 4_560 * minute)
        let held = try XCTUnwrap((carol["held"] as? [[String: Any]])?.first)
        XCTAssertEqual(Self.instant(held["resetAtMs"]), nowMs + 4_560 * minute)
        XCTAssertEqual(held["minutesUntilReset"] as? Int, 4_560)

        let hal = try Self.row("hal@example.com", now: now)  // held for 47m
        XCTAssertEqual(Self.instant(hal["freeAtMs"]), nowMs + 47 * minute)
        XCTAssertEqual(Self.instant(hal["rateLimitedUntilMs"]), nowMs + 47 * minute)

        let alice = try Self.row("alice@example.com", now: now)  // the 5h window resets in 2h 39m
        XCTAssertEqual(Self.instant(alice["fiveHourResetAtMs"]), nowMs + 159 * minute)
        XCTAssertTrue(alice["freeAtMs"] is NSNull, "a null instant must stay null")
    }

    /// A scene varies one fact by laying it over the row last, so it wins, and the rest of the
    /// row stays the fixture's.
    func testExtraIsLaidOverTheCanonicalRowLast() throws {
        let bob = try Self.row(
            "bob@example.com/codex", now: Date(),
            extra: ["probeStatus": "error", "loginError": "refresh_token_reused"])
        XCTAssertEqual(bob["probeStatus"] as? String, "error")
        XCTAssertEqual(bob["loginError"] as? String, "refresh_token_reused")
        XCTAssertEqual(bob["provider"] as? String, "codex")
        XCTAssertEqual(bob["plan"] as? String, "ChatGPT Pro")
    }

    func testANameTheFixtureDoesNotHoldBuildsNoRow() throws {
        XCTAssertNil(
            RenderFixtureRow.json(named: "nobody@example.com", in: try Self.fixtureText(), now: Date()))
    }

    /// `codexRow` and `account(...)` are in the app target, so this pins as text that both build
    /// through the tested builder, and that `codexRow` builds over the harness's own copy.
    func testTheHarnessBuildsItsRowsThroughTheTestedBuilder() throws {
        let harness = try Self.harnessText().filter { !$0.isWhitespace }
        for call in [
            "RenderFixtureRow.json(named:row.rawValue,in:canonicalCodexStatusJSON,now:now,extra:extra)",
            "RenderFixtureRow.jsonText(object,laying:extra)",
        ] {
            XCTAssertTrue(
                harness.contains(call),
                "RenderStates.swift no longer calls \(call), so a row it builds is untested")
        }
    }

    private static func harnessText() throws -> String {
        try String(
            contentsOf: CanonicalFixtures.repoRoot.appendingPathComponent(
                "apps/macos/Sources/TcrBar/RenderStates.swift"),
            encoding: .utf8)
    }

    private static func fixtureText() throws -> String {
        String(decoding: try CanonicalFixtures.data("codex-status.json"), as: UTF8.self)
    }

    /// The canonical row `name`, built for `now` as a scene builds it, parsed back.
    private static func row(_ name: String, now: Date, extra: [String: Any] = [:]) throws
        -> [String: Any]
    {
        let text = try XCTUnwrap(
            RenderFixtureRow.json(named: name, in: try fixtureText(), now: now, extra: extra))
        return try XCTUnwrap(try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any])
    }

    private static func instant(_ value: Any?) -> Int64? { (value as? NSNumber)?.int64Value }

    /// Where the copy first parts from the fixture, so a failure names the line instead of
    /// leaving it to be found in two printed copies of the whole file.
    private static func firstDifference(_ copy: String, _ fixture: String) -> String {
        let copyLines = copy.split(separator: "\n", omittingEmptySubsequences: false)
        let fixtureLines = fixture.split(separator: "\n", omittingEmptySubsequences: false)
        for (index, pair) in zip(copyLines, fixtureLines).enumerated() where pair.0 != pair.1 {
            return "the copy first differs at line \(index + 1) of the fixture: "
                + "\(pair.0.trimmingCharacters(in: .whitespaces)) against "
                + "\(pair.1.trimmingCharacters(in: .whitespaces))"
        }
        return "the copy has \(copyLines.count) lines, and the fixture \(fixtureLines.count)"
    }
}
