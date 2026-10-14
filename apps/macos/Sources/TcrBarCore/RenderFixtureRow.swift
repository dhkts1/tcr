import Foundation

/// Builds the rows the render harness draws: one row of a status fixture, moved to the render's
/// clock, with a scene's own facts laid over it. It serves the render harness alone
/// (`RenderStates.codexRow` and `RenderStates.account`), and is public only because the harness
/// is in the app target. It lives here, and not beside its callers, because no test compiles the
/// app target, and every Codex scene's countdown rests on this arithmetic.
///
/// A fixture writes each absolute instant against its own clock, ``fixtureNowMs``. A scene
/// renders against the real clock, so every instant moves by the same amount, `now` minus that
/// clock, and sits as far from `now` as it sat from the fixture's: carol frees in 3d 4h, and
/// hal is held for 47m, whenever the render runs. A relative figure (`minutesUntilReset`,
/// `secondsUntilFree`) is measured from whenever the row was read, and stays as it is.
public enum RenderFixtureRow {
    /// 2026-01-01T00:00:00Z, the instant every absolute time in
    /// `apps/macos/Tests/Fixtures/codex-status.json` is written against.
    static let fixtureNowMs: Int64 = 1_767_225_600_000

    /// The keys that hold an absolute instant, at any depth of a row.
    static let instantKeys: Set<String> = [
        "fiveHourResetAtMs", "sevenDayResetAtMs", "sevenDayOiResetAtMs", "freeAtMs",
        "rateLimitedUntilMs", "lastProbeMs", "resetAtMs", "since", "lastRefusalAtMs",
    ]

    /// The row named `name` in `fixture`, a JSON array of row objects, as JSON text: every
    /// absolute instant moved to sit as far from `now` as it sits from ``fixtureNowMs``, then
    /// `extra` laid over it as ``jsonText(_:laying:)`` lays it. `nil` when `fixture` is not an
    /// array of objects, or holds no row of that name.
    public static func json(
        named name: String, in fixture: String, now: Date, extra: [String: Any] = [:]
    ) -> String? {
        let delta = Int64((now.timeIntervalSince1970 * 1000).rounded()) - fixtureNowMs
        guard
            let rows = (try? JSONSerialization.jsonObject(with: Data(fixture.utf8)))
                as? [[String: Any]],
            let row = rows.first(where: { $0["name"] as? String == name }),
            let moved = shiftingInstants(row, by: delta) as? [String: Any]
        else { return nil }
        return jsonText(moved, laying: extra)
    }

    /// `object` with `extra` laid over it (`extra` wins), as JSON text with sorted keys. A value
    /// is anything `JSONSerialization` writes: a string, a number, a `Bool`, `NSNull()` for
    /// `null`, or an array or dictionary of those. `nil` when the merged object holds anything
    /// else.
    public static func jsonText(_ object: [String: Any], laying extra: [String: Any]) -> String? {
        let merged = object.merging(extra) { _, new in new }
        guard JSONSerialization.isValidJSONObject(merged),
            let data = try? JSONSerialization.data(withJSONObject: merged, options: [.sortedKeys])
        else { return nil }
        return String(decoding: data, as: UTF8.self)
    }

    /// `value` with the number under every instant key, at any depth, moved by `delta`
    /// milliseconds.
    static func shiftingInstants(_ value: Any, by delta: Int64) -> Any {
        if let object = value as? [String: Any] {
            var moved: [String: Any] = [:]
            for (key, item) in object {
                if instantKeys.contains(key), let instant = item as? NSNumber {
                    moved[key] = instant.int64Value + delta
                } else {
                    moved[key] = shiftingInstants(item, by: delta)
                }
            }
            return moved
        }
        if let array = value as? [Any] { return array.map { shiftingInstants($0, by: delta) } }
        return value  // a string, a number, a Bool, or NSNull: a null instant stays null
    }
}
