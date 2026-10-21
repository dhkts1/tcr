import AppKit

/// The whole menu-bar image: the ray mark, drawn by this app.
///
/// ## Why the app composes this instead of handing SwiftUI a view
///
/// It was a `MenuBarExtra` label, and a `MenuBarExtra` renders its label
/// **monochrome no matter what the image says**. Six label constructions were
/// each hosted in a real `MenuBarExtra` and rasterised off the real
/// `NSStatusBarButton` with `cacheDisplay(in:to:)`:
///
/// | label | opaque px | coloured px |
/// |---|---|---|
/// | `Image(nsImage:)`, `isTemplate = false` | 68 | 0 |
/// | `Image(nsImage:).renderingMode(.original)` | 68 | 0 |
/// | `Text("●").foregroundStyle(cyan)` | 158 | 0 |
/// | symbol pre-rasterised to a plain bitmap, `isTemplate = false` | 68 | 0 |
/// | `Text("☕")` (an emoji, i.e. a colour font) | 198 | 14 |
/// | `button.image = <tinted NSImage>` set on the button | 533 | 533 |
///
/// Only the last one carries an arbitrary colour, and it is not reachable from a
/// SwiftUI scene. So the app owns the `NSStatusItem`, and this type is what it
/// puts in `button.image`.
///
/// ## What it draws
///
/// The site's mark (`docs/site/mark.svg`) made into a gauge: eight rays round a
/// hub, each over a dim track. One glyph carries two facts:
///
///  - **Lit rays**: how many of the eight are drawn at full strength, clockwise
///    from 12 o'clock, is the fleet's capacity
///    (``StatusPoller/PollState/capacityFraction``; ``litRays(for:)`` maps it).
///    No ray is lit only when nothing is ready, and all eight only when
///    everything is, so a fleet one account short never reads as full.
///  - **Tint**: ``Tint/template`` (plain, system-coloured, exactly like every
///    other stock menu-bar icon) when keep-awake is off and nothing is wrong;
///    ``Tint/awake(_:)`` while keep-awake holds the Mac up; ``Tint/near(_:)``
///    when no account is ready and at least one is close; ``Tint/failed(_:)``
///    when the last poll did not answer at all, drawn as the tracks alone with
///    no hub, because there is no reading and no proxy to show. The caller
///    (``MenuBarShell/cupTint(for:awake:cupPool:)``) picks exactly one `Tint`
///    per poll, never blends two.
///
/// The canvas is a fixed ``size`` whatever the state, so the status item never
/// changes width when the reading does.
///
/// ## The catch, and how a coloured tint dodges it
///
/// A non-template image is drawn exactly as authored, so it stops getting the
/// system's automatic menu-bar tinting and would freeze at whatever colour was
/// baked in, right in one appearance and wrong in the other, UNLESS the
/// dynamic colour resolves inside the drawing handler, which runs at draw
/// time rather than at construction. ``draw(in:lit:color:failed:)`` runs inside
/// that handler and sets the colour there, and the dim tracks are the same
/// colour at a lower context alpha rather than a second, pre-resolved colour.
///
/// `Tint.template` needs none of that: it stays `isTemplate = true` end to
/// end, so the menu bar itself supplies the colour, in both appearances and
/// over a light wallpaper, for free. A template keeps its alpha, which is what
/// lets the dim tracks survive it. Do not regress it to a hand-tinted image to
/// make the branches look alike.
public enum MenuBarMark {

    /// The canvas, in points. The stock menu-bar glyph size.
    public static let size = NSSize(width: 18, height: 18)
    /// Rays round the hub. Eight, like the site mark.
    public static let rayCount = 8

    /// Geometry in the 18-point design space: rays run from `inner` to `outer`
    /// out from the centre, `lineWidth` thick, round-capped, round a hub of
    /// `hubRadius`. Drawn at 18 px, the rays stay apart and the hub stays clear of them.
    private static let inner: CGFloat = 3.2
    private static let outer: CGFloat = 8.2
    private static let lineWidth: CGFloat = 1.7
    private static let hubRadius: CGFloat = 1.5
    /// How strongly an unlit track is drawn. Low enough to read as empty, high
    /// enough that eight tracks still read as the mark when nothing is lit.
    private static let trackAlpha: CGFloat = 0.32
    /// The failed mark has no lit rays and no hub, so its tracks carry the
    /// whole glyph and are drawn stronger.
    private static let failedTrackAlpha: CGFloat = 0.75

    /// How many rays to light for a capacity `fraction`. `nil`, NaN and
    /// anything at or below 0 light none; 1 and above light all eight; any
    /// reading in between lights at least one and at most seven, so a fleet
    /// with one account ready never looks empty and one with one account
    /// short never looks full.
    public static func litRays(for fraction: Double?) -> Int {
        guard let fraction, fraction.isFinite, fraction > 0 else { return 0 }
        if fraction >= 1 { return rayCount }
        let rounded = Int((fraction * Double(rayCount)).rounded())
        return min(rayCount - 1, max(1, rounded))
    }

    /// What VoiceOver says about the menu-bar item. It is the one surface with
    /// no room for a label, so this is the only place the state is spoken.
    ///
    /// A Mac waiting on an answer is spoken FIRST, in
    /// ``PeerAdmission/knockBarSentence(count:)``'s own words, because it is
    /// the one thing here that is waiting on a person rather than reporting on
    /// a fleet. Capacity follows it, unchanged, and at zero knocks the
    /// sentence is byte-identical to what it has always been.
    ///
    /// A fleet holding Codex accounts names each pool that has an enabled
    /// account, Claude first: `tcr fleet capacity: Claude 1 of 1 ready, Codex
    /// 3 of 9 ready`. The mark and its colour describe one pool, and a listener
    /// has neither, so the sentence carries both counts. With no `fleet`, or a
    /// fleet with no Codex account, it is the phrase it has always been.
    public static func accessibilityDescription(
        awake: Bool, knocks: Int = 0, fleet: Fleet? = nil
    ) -> String {
        let phrase = capacityPhrase(fleet)
        let capacity =
            awake
            ? "\(phrase). \(KeepAwakeGlyph.accessibilityDescription)."
            : phrase
        guard let asking = PeerAdmission.knockBarSentence(count: knocks) else { return capacity }
        return "\(asking) \(capacity)"
    }

    /// `tcr fleet capacity`, followed in a fleet holding Codex accounts by each
    /// pool's count, over the pools that have an enabled account.
    private static func capacityPhrase(_ fleet: Fleet?) -> String {
        let phrase = "tcr fleet capacity"
        guard let pools = fleet?.providerPools() else { return phrase }
        let counts = pools.filter { $0.enabledCount > 0 }.map {
            "\($0.name) \($0.readyCount) of \($0.enabledCount) ready"
        }
        guard !counts.isEmpty else { return phrase }
        return "\(phrase): " + counts.joined(separator: ", ")
    }

    /// Which colour the mark wears. Kept as data here, decided by the caller
    /// from `PollState` and the keep-awake toggle, so this type stays
    /// decoupled from `PollState`'s cases, and `Tok` stays the one place a
    /// colour is written down.
    public enum Tint: Equatable {
        /// Plain: `isTemplate = true`, no colour of its own. Do not hand-tint
        /// this to match the other branches: the whole point of staying
        /// template is the free system tinting it buys.
        case template
        /// Keep-awake is on: the whole mark in this colour
        /// (`Tok.awakeNSColor` at the call site).
        case awake(NSColor)
        /// No account ready, at least one near its limit
        /// (`Fleet.capacityGlyphState == .near`).
        case near(NSColor)
        /// The last poll did not answer at all. Callers pass `fraction: nil`
        /// alongside this case: there is no reading, so the mark is its
        /// tracks alone in this colour, with no hub.
        case failed(NSColor)

        /// `nil` for `.template`, the dynamic colour otherwise.
        var color: NSColor? {
            switch self {
            case .template: return nil
            case .awake(let c), .near(let c), .failed(let c): return c
            }
        }
    }

    /// The image for `NSStatusBarButton.image`.
    ///
    /// - Parameter fraction: the fleet's capacity, `0...1`, turned into lit
    ///   rays by ``litRays(for:)``. `nil` (an all-disabled fleet, or no fleet
    ///   read yet) draws exactly like `0`, never a fault-coloured mark; the
    ///   fault, if there is one, is `tint`'s job.
    /// - Parameter tint: see ``Tint``.
    /// - Parameter knocks: how many Macs are waiting on an answer, for the
    ///   spoken description alone. The knock MARK itself is a separate glyph
    ///   in the status item's title (`MenuBarShell.updateMark`), not part of
    ///   this image: it has to be drawn whether or not the counts label is on,
    ///   and this mark is a reading of the fleet and nothing else.
    /// - Parameter fleet: the fleet the mark reads, for the spoken description
    ///   alone (``accessibilityDescription(awake:knocks:fleet:)``). It changes
    ///   no pixel: the rays and the tint are `fraction` and `tint`.
    public static func image(
        fraction: Double?, tint: Tint, knocks: Int = 0, fleet: Fleet? = nil
    ) -> NSImage {
        let lit = litRays(for: fraction)
        let color = tint.color
        let failed: Bool
        if case .failed = tint { failed = true } else { failed = false }
        let composed = NSImage(size: size, flipped: false) { rect in
            Self.draw(in: rect, lit: lit, color: color, failed: failed)
            return true
        }
        // Only `.template` stays a template end to end: every coloured
        // branch draws an arbitrary hue the menu bar must not re-tint.
        composed.isTemplate = tint == .template
        let awake: Bool
        if case .awake = tint { awake = true } else { awake = false }
        composed.accessibilityDescription = accessibilityDescription(
            awake: awake, knocks: knocks, fleet: fleet)
        return composed
    }

    /// Tracks first, lit rays over them, hub last. `color == nil` (the
    /// `.template` branch) draws in black, which the menu bar replaces with its
    /// own colour while keeping each pixel's alpha. A non-`nil` color is set
    /// here, inside the image's drawing handler, so a dynamic `NSColor` resolves
    /// against the appearance current at draw time.
    private static func draw(in rect: NSRect, lit: Int, color: NSColor?, failed: Bool) {
        let ink = color ?? .black
        let scale = min(rect.width / size.width, rect.height / size.height)
        let centre = NSPoint(x: rect.midX, y: rect.midY)

        func ray(_ index: Int) -> NSBezierPath {
            // 0 is 12 o'clock, counting clockwise; y grows upwards here.
            let angle = 2 * Double.pi * Double(index) / Double(rayCount)
            let dx = CGFloat(sin(angle))
            let dy = CGFloat(cos(angle))
            let path = NSBezierPath()
            path.move(to: NSPoint(x: centre.x + dx * inner * scale, y: centre.y + dy * inner * scale))
            path.line(to: NSPoint(x: centre.x + dx * outer * scale, y: centre.y + dy * outer * scale))
            path.lineWidth = lineWidth * scale
            path.lineCapStyle = .round
            return path
        }

        ink.setStroke()
        ink.setFill()
        guard let context = NSGraphicsContext.current?.cgContext else { return }

        context.saveGState()
        context.setAlpha(failed ? failedTrackAlpha : trackAlpha)
        for index in lit..<rayCount { ray(index).stroke() }
        context.restoreGState()

        for index in 0..<lit { ray(index).stroke() }

        if !failed {
            let r = hubRadius * scale
            NSBezierPath(
                ovalIn: NSRect(x: centre.x - r, y: centre.y - r, width: r * 2, height: r * 2)
            ).fill()
        }
    }
}
