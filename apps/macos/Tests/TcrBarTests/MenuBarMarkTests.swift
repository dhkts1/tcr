import AppKit
import XCTest

@testable import TcrBarCore

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

/// Canonical alice under another name and her own org, in no group, with `overrides`: a
/// second Claude identity with alice's facts.
private func aliceCopy(
    _ name: String, org: String, _ overrides: [String: Any] = [:]
) throws -> [String: Any] {
    var all: [String: Any] = ["name": name, "orgUuid": org, "groups": [String]()]
    for (key, value) in overrides { all[key] = value }
    return try canonicalRow("alice@example.com", all)
}

/// A Claude pool of 3 of 4 ready: alice and two copies of her, ready, and frank, spent on
/// the week.
private func claudeThreeOfFour() throws -> [[String: Any]] {
    [
        try canonicalRow("alice@example.com", ["groups": [String]()]),
        try aliceCopy("nina@example.com", org: "33333333-3333-4333-8333-333333333333"),
        try aliceCopy("omar@example.com", org: "44444444-4444-4444-8444-444444444444"),
        try aliceCopy(
            "frank@example.com", org: "22222222-2222-4222-8222-222222222222",
            [
                "quotaState": "spent", "quota": 1.0, "sevenDay": 1.0, "sevenDayState": "spent",
                "held": [
                    ["window": "7d", "minutesUntilReset": 4320, "resetAtMs": 1_767_484_800_000]
                ],
            ]),
    ]
}

/// The menu-bar image, checked at the level a unit test can reach: the `NSImage`
/// this app hands to the status button.
///
/// What it cannot reach is whether the *button* honours what the image says —
/// that needs a real `NSStatusItem` and a rasterisation off it, which is what
/// `TcrBar --shell-probe` is for. These tests are the cheap half that runs in
/// `swift test`; the probe is the gate.
@MainActor
final class MenuBarMarkTests: XCTestCase {

    /// Pure saturated red rather than `Tok.awakeNSColor`: the assertions below
    /// are about colour surviving at all, so they must not be able to pass on a
    /// near-grey, and a palette token is free to change.
    private let loudTint = NSColor(srgbRed: 1, green: 0, blue: 0, alpha: 1)

    /// `.template` stays a template, which is what buys the menu bar's automatic
    /// tinting — correct in both appearances and over a light wallpaper, for
    /// free. Hand-tinting it to match the coloured branches would throw that away.
    func testTemplateTintIsATemplate() throws {
        let image = try XCTUnwrap(MenuBarMark.image(fraction: 0.5, tint: .template))
        XCTAssertTrue(image.isTemplate)
    }

    /// A template image is re-rendered in the menu bar's own colour, which strips
    /// the tint. `isTemplate = false` is the documented opt-out and the single
    /// property that makes the colour channel possible.
    func testEveryColouredTintIsNotATemplate() throws {
        for tint: MenuBarMark.Tint in [.awake(loudTint), .near(loudTint), .failed(loudTint)] {
            let image = try XCTUnwrap(MenuBarMark.image(fraction: 0.5, tint: tint))
            XCTAssertFalse(image.isTemplate, "\(tint) must not stay a template")
        }
    }

    /// The point of the whole rebuild: a coloured mark really carries colour.
    func testAPixelInAColouredMarkCarriesTheTint() throws {
        let image = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(loudTint)))
        let scan = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        // A positive control on the probe itself: with nothing rasterised the
        // colour assertion below would be vacuous rather than failing.
        XCTAssertGreaterThan(scan.opaque, 0, "nothing was rasterised — the scan proves nothing")
        XCTAssertGreaterThan(scan.tinted, 0, "no red in the mark — the tint was dropped")
    }

    /// And `.template` does not, which is the negative control. Without it the
    /// test above passes just as happily on a mark that is red in every tint —
    /// i.e. on a menu bar that cannot tell keep-awake-on from off.
    func testTheTemplateTintCarriesNoTint() throws {
        let image = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .template))
        let scan = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        XCTAssertGreaterThan(scan.opaque, 0, "nothing was rasterised — the scan proves nothing")
        XCTAssertEqual(scan.tinted, 0, "the template tint is coloured, so it cannot follow the menu bar")
    }

    /// The capacity reading: an empty mark lights no ray, so its only opaque
    /// pixels are the hub (the dim tracks fall under the scan's alpha floor),
    /// while a full mark lights all eight.
    func testAHigherFractionOpaquesMorePixelsThanAnEmptyMark() throws {
        let empty = try XCTUnwrap(MenuBarMark.image(fraction: 0, tint: .awake(loudTint)))
        let full = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(loudTint)))
        let emptyScan = try XCTUnwrap(rasterise(empty, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
        let fullScan = try XCTUnwrap(rasterise(full, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        XCTAssertGreaterThan(
            fullScan.opaque, emptyScan.opaque,
            "a full mark must draw more ink than an empty one: the lit rays are not being drawn")
    }

    /// Which rays light for which reading. The two ends are exact: none only
    /// when nothing is ready, all eight only when everything is. In between
    /// the count rounds, but never to either end, so 1 of 13 ready still
    /// lights a ray and 12 of 13 still leaves one dark.
    func testLitRaysNeverRoundToEmptyOrFull() {
        XCTAssertEqual(MenuBarMark.litRays(for: nil), 0)
        XCTAssertEqual(MenuBarMark.litRays(for: .nan), 0)
        XCTAssertEqual(MenuBarMark.litRays(for: -0.5), 0)
        XCTAssertEqual(MenuBarMark.litRays(for: 0), 0)
        XCTAssertEqual(MenuBarMark.litRays(for: 1.0 / 13), 1)
        XCTAssertEqual(MenuBarMark.litRays(for: 0.5), 4)
        XCTAssertEqual(MenuBarMark.litRays(for: 8.0 / 13), 5)
        XCTAssertEqual(MenuBarMark.litRays(for: 12.0 / 13), 7)
        // Plain rounding would give 0 and 8 here: 1 of 20 ready, 19 of 20.
        XCTAssertEqual(MenuBarMark.litRays(for: 0.05), 1)
        XCTAssertEqual(MenuBarMark.litRays(for: 0.95), 7)
        XCTAssertEqual(MenuBarMark.litRays(for: 1), 8)
        XCTAssertEqual(MenuBarMark.litRays(for: 1.4), 8)
    }

    /// A failed poll has no reading and no proxy to show: tracks only, no hub.
    /// The centre pixel tells the two apart without relying on colour; the
    /// empty mark in the same colour is the positive control that the probe
    /// finds a hub when there is one.
    func testAFailedMarkDrawsNoHub() throws {
        let failed = MenuBarMark.image(fraction: nil, tint: .failed(loudTint))
        let empty = MenuBarMark.image(fraction: nil, tint: .near(loudTint))
        let dark = try XCTUnwrap(NSAppearance(named: .darkAqua))
        let failedScan = try XCTUnwrap(rasterise(failed, in: dark))
        let emptyScan = try XCTUnwrap(rasterise(empty, in: dark))
        XCTAssertGreaterThan(failedScan.opaque, 0, "nothing was rasterised: the scan proves nothing")
        XCTAssertGreaterThan(emptyScan.centreAlpha, 0.5, "the probe found no hub on a mark that has one")
        XCTAssertLessThan(failedScan.centreAlpha, 0.1, "a failed poll still draws the hub")
    }

    /// `nil` fraction is documented to draw exactly like `0`: an all-disabled
    /// fleet or a poll with nothing loaded yet must never draw a fault-coloured
    /// mark that also happens to be full.
    func testANilFractionDrawsLikeZero() throws {
        let nilFraction = try XCTUnwrap(MenuBarMark.image(fraction: nil, tint: .awake(loudTint)))
        let zeroFraction = try XCTUnwrap(MenuBarMark.image(fraction: 0, tint: .awake(loudTint)))
        let a = try XCTUnwrap(rasterise(nilFraction, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
        let b = try XCTUnwrap(rasterise(zeroFraction, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
        XCTAssertEqual(a.opaque, b.opaque)
    }

    /// A fraction outside `0...1` (a stale reading from a fleet that shrank
    /// mid-poll, or a negative value `Double` does not itself forbid) must
    /// never draw more ink than a full mark or less than an empty one: the
    /// caller's contract is a sane image for any `Double`, not a crash.
    func testAnOutOfRangeFractionStaysBoundedByEmptyAndFull() throws {
        let empty = try XCTUnwrap(MenuBarMark.image(fraction: 0, tint: .awake(loudTint)))
        let full = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(loudTint)))
        let emptyScan = try XCTUnwrap(rasterise(empty, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
        let fullScan = try XCTUnwrap(rasterise(full, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        for outOfRange: Double in [-0.5, 1.4] {
            let image = try XCTUnwrap(MenuBarMark.image(fraction: outOfRange, tint: .awake(loudTint)))
            let scan = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
            XCTAssertGreaterThanOrEqual(scan.opaque, emptyScan.opaque, "\(outOfRange) drew less than empty")
            XCTAssertLessThanOrEqual(scan.opaque, fullScan.opaque, "\(outOfRange) drew more than full")
        }
    }

    /// The claim a coloured branch rests on: a non-template image is drawn
    /// exactly as authored, so a colour baked in at composition time would be
    /// wrong in the other appearance — unless the dynamic colour is resolved
    /// inside the drawing handler, which runs at draw time.
    ///
    /// Asserted on one image drawn twice, not on two images: that is the
    /// stronger statement, and it is the one that fails if `NSImage` ever
    /// caches the first raster.
    func testADynamicTintReResolvesPerAppearance() throws {
        // `Tok.awakeNSColor`'s own shape, without depending on `TcrBar`: two
        // different fixed hues, chosen dynamically. `NSColor(name:dynamicProvider:)`
        // is the same primitive the real token is built on.
        let dynamic = NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                ? NSColor(srgbRed: 0, green: 1, blue: 0, alpha: 1)
                : NSColor(srgbRed: 1, green: 0, blue: 1, alpha: 1)
        }
        let image = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(dynamic)))
        let light = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .aqua))))
        let dark = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        XCTAssertGreaterThan(light.opaque, 0)
        XCTAssertGreaterThan(dark.opaque, 0)
        let distance =
            abs(light.meanColour.r - dark.meanColour.r) + abs(light.meanColour.g - dark.meanColour.g)
            + abs(light.meanColour.b - dark.meanColour.b)
        XCTAssertGreaterThan(
            distance, 0.3,
            "the same pixels came back the same colour in both appearances — the dynamic tint was baked in")
    }

    /// The menu bar has no room for a label, so this string is the only place the
    /// state is spoken. Both states have to say something, and they have to say
    /// different things.
    func testBothStatesAreSpokenAndTheyDiffer() throws {
        let off = MenuBarMark.accessibilityDescription(awake: false)
        let on = MenuBarMark.accessibilityDescription(awake: true)
        XCTAssertFalse(off.isEmpty)
        XCTAssertNotEqual(off, on)
        XCTAssertTrue(
            on.contains(KeepAwakeGlyph.accessibilityDescription),
            "the ON description must name the mode: \(on)")

        let image = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(loudTint)))
        XCTAssertEqual(image.accessibilityDescription, on)
    }

    // MARK: - A fleet holding Codex accounts

    /// The mark speaks each pool that has an enabled account, Claude first. A fleet with
    /// no Codex account keeps the sentence it always had.
    func testAMixedFleetSpeaksBothPools() throws {
        let fleet = try decodeFleet(
            try [
                "alice@example.com", "bob@example.com/codex", "carol@example.com",
                "dana@example.com", "erin@example.com/codex", "gil@example.com",
                "hal@example.com", "ivy@example.com", "jo@example.com", "kim@example.com",
            ].map { try canonicalRow($0) })
        let spoken = "tcr fleet capacity: Claude 1 of 1 ready, Codex 3 of 9 ready"
        XCTAssertEqual(MenuBarMark.accessibilityDescription(awake: false, fleet: fleet), spoken)

        let awake = MenuBarMark.accessibilityDescription(awake: true, fleet: fleet)
        XCTAssertTrue(awake.hasPrefix(spoken), awake)
        XCTAssertTrue(awake.hasSuffix(". Keeping this Mac awake."), awake)

        let knocked = MenuBarMark.accessibilityDescription(awake: false, knocks: 1, fleet: fleet)
        XCTAssertTrue(knocked.hasPrefix("1 Mac is asking to connect."), knocked)
        XCTAssertTrue(knocked.hasSuffix(spoken), knocked)

        let claudeOnly = try decodeFleet([canonicalRow("alice@example.com")])
        XCTAssertEqual(
            MenuBarMark.accessibilityDescription(awake: false, fleet: claudeOnly),
            "tcr fleet capacity")
        XCTAssertEqual(
            try XCTUnwrap(
                MenuBarMark.image(fraction: 1, tint: .template, fleet: fleet)
            ).accessibilityDescription, spoken)
    }

    /// A Codex-only fleet speaks its one pool.
    func testACodexOnlyFleetSpeaksItsPool() throws {
        let fleet = try decodeFleet(
            try [
                "bob@example.com/codex", "dana@example.com", "carol@example.com",
                "erin@example.com/codex",
            ].map { try canonicalRow($0) })
        XCTAssertEqual(
            MenuBarMark.accessibilityDescription(awake: false, fleet: fleet),
            "tcr fleet capacity: Codex 2 of 4 ready")
    }

    /// Each part of the count is amber only when its own pool is near: nothing ready and
    /// something close. The cup, following the emptier pool, is amber in both fleets.
    func testOnlyTheNearHalfIsAmber() throws {
        let codexNear = PollState.loaded(
            try decodeFleet(
                try claudeThreeOfFour() + [
                    canonicalRow("ivy@example.com"), canonicalRow("carol@example.com"),
                ]))
        XCTAssertEqual(
            codexNear.countsParts,
            [
                PollState.CountsPart(text: "3/4", isNear: false),
                PollState.CountsPart(text: "0/2", isNear: true),
            ])
        XCTAssertEqual(codexNear.countsNearHalves, [false, true])
        XCTAssertEqual(codexNear.capacityTintKind(awake: false, cupPool: .emptier), .near)

        let claudeNear = PollState.loaded(
            try decodeFleet([
                try aliceCopy(
                    "nina@example.com", org: "33333333-3333-4333-8333-333333333333",
                    ["quotaState": "near", "quota": 0.9]),
                try canonicalRow("bob@example.com/codex"), try canonicalRow("dana@example.com"),
            ]))
        XCTAssertEqual(
            claudeNear.countsParts,
            [
                PollState.CountsPart(text: "0/1", isNear: true),
                PollState.CountsPart(text: "2/2", isNear: false),
            ])
        XCTAssertEqual(claudeNear.countsNearHalves, [true, false])
        XCTAssertEqual(claudeNear.capacityTintKind(awake: false, cupPool: .emptier), .near)
    }

    // MARK: - Rasterising

    private struct Scan {
        var opaque = 0
        /// Pixels that carry the tint — i.e. any part of the mark drawn with a
        /// non-template colour.
        var tinted = 0
        /// Mean (r, g, b) of the opaque pixels, for telling one solid colour
        /// from another rather than merely "some colour, some other colour".
        var meanColour: (r: Double, g: Double, b: Double) = (0, 0, 0)
        /// Alpha of the pixel at the centre of the image, where the hub sits.
        var centreAlpha: Double = 0
    }

    /// Draw at 2x under a chosen appearance and count. `performAsCurrentDrawingAppearance`
    /// is what makes the dynamic colours inside the drawing handler resolve
    /// against that appearance rather than the process's default.
    private func rasterise(_ image: NSImage, in appearance: NSAppearance) -> Scan? {
        let scale = 2
        let width = Int(image.size.width.rounded()) * scale
        let height = Int(image.size.height.rounded()) * scale
        guard width > 0, height > 0,
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
        else { return nil }

        appearance.performAsCurrentDrawingAppearance {
            NSGraphicsContext.saveGraphicsState()
            let context = NSGraphicsContext(bitmapImageRep: rep)
            NSGraphicsContext.current = context
            context?.cgContext.scaleBy(x: CGFloat(scale), y: CGFloat(scale))
            image.draw(
                in: NSRect(origin: .zero, size: image.size), from: .zero,
                operation: .sourceOver, fraction: 1)
            NSGraphicsContext.restoreGraphicsState()
        }

        var scan = Scan()
        scan.centreAlpha = Double(
            rep.colorAt(x: rep.pixelsWide / 2, y: rep.pixelsHigh / 2)?.alphaComponent ?? 0)
        var rSum = 0.0
        var gSum = 0.0
        var bSum = 0.0
        for x in 0..<rep.pixelsWide {
            for y in 0..<rep.pixelsHigh {
                guard let colour = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB),
                    colour.alphaComponent > 0.35
                else { continue }
                scan.opaque += 1
                let r = colour.redComponent
                let g = colour.greenComponent
                let b = colour.blueComponent
                rSum += r
                gSum += g
                bSum += b
                // Not grey and not white/black — a template's own rendering —
                // counts as "carries a tint".
                let spread = max(r, g, b) - min(r, g, b)
                if spread > 0.15 {
                    scan.tinted += 1
                }
            }
        }
        if scan.opaque > 0 {
            scan.meanColour = (
                rSum / Double(scan.opaque), gSum / Double(scan.opaque), bSum / Double(scan.opaque)
            )
        }
        return scan
    }
}
