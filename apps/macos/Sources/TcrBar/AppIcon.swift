import AppKit
import Foundation

/// The app mark, drawn in code.
///
/// ## Why code and not an asset
///
/// A committed `.icns` is a binary that drifts silently: change the mark and the
/// icon keeps the old one, and nothing fails. Drawing it here means the whole
/// icon set regenerates from one command.
///
/// ## What it means
///
/// The site's mark (`docs/site/mark.svg`): a radial quota chart. Eight rays round
/// a hub, one per account, each as long as that account's quota left, in the
/// panel's ready green, near amber and spent red, over dim full-length tracks.
/// The white hub is the proxy. The tracks and the hub are what keep it from
/// reading as a spark or a spinner. The lengths are a fixed picture of a healthy
/// pool with one account near its limit and one nearly spent, the same as the
/// site, not a live reading.
///
/// ## Small sizes
///
/// At 32 px and below (the Finder list and the Login Items row, which is where
/// the icon is mostly seen) the tracks blur into the rays, so those slots drop
/// them and draw thicker rays and a bigger hub.
enum AppIcon {

    /// Each ray's length as a fraction of its track, clockwise from 12 o'clock,
    /// and its colour: the site mark's eight rays, measured off `mark.svg`.
    private static let rays: [(length: CGFloat, color: NSColor)] = [
        (1.00, ready), (0.75, ready), (0.94, ready), (0.57, ready),
        (0.81, ready), (0.35, near), (0.69, ready), (0.15, spent),
    ]
    private static let ready = srgb(0x66, 0xd0, 0x81)
    private static let near = srgb(0xff, 0xd1, 0x6b)
    private static let spent = srgb(0xea, 0x5a, 0x56)
    private static let track = srgb(0x2f, 0x33, 0x39)
    private static let hub = srgb(0xf2, 0xf2, 0xef)

    private static func srgb(_ r: Int, _ g: Int, _ b: Int) -> NSColor {
        NSColor(
            srgbRed: CGFloat(r) / 255, green: CGFloat(g) / 255, blue: CGFloat(b) / 255, alpha: 1)
    }

    /// Draw the mark at `size` points, square.
    static func image(size: CGFloat) -> NSImage {
        NSImage(size: NSSize(width: size, height: size), flipped: false) { rect in
            let s = size / 1024.0  // design space is 1024, the largest icns slot
            let small = size <= 32

            // macOS icons do not fill their square: the grid leaves a margin so
            // adjacent icons breathe. ~10% inset matches the system look.
            let ground = rect.insetBy(dx: 100 * s, dy: 100 * s)

            // The site's near-black rather than pure black, which reads as a
            // hole next to real macOS icons.
            let squircle = NSBezierPath(
                roundedRect: ground, xRadius: 185 * s, yRadius: 185 * s)
            srgb(0x09, 0x0c, 0x10).setFill()
            squircle.fill()

            // A hairline lip so the mark still has an edge on a dark wallpaper.
            srgb(0x53, 0x59, 0x60).setStroke()
            squircle.lineWidth = 6 * s
            squircle.stroke()

            let centre = NSPoint(x: rect.midX, y: rect.midY)
            let inner = (small ? 120 : 130) * s
            let outer = (small ? 330 : 340) * s
            let width = (small ? 78 : 62) * s
            let hubRadius = (small ? 70 : 64) * s

            func ray(_ index: Int, length: CGFloat) -> NSBezierPath {
                // 0 is 12 o'clock, counting clockwise; y grows upwards here.
                let angle = 2 * Double.pi * Double(index) / Double(rays.count)
                let dx = CGFloat(sin(angle))
                let dy = CGFloat(cos(angle))
                let end = inner + max(length, 0.02) * (outer - inner)
                let path = NSBezierPath()
                path.move(to: NSPoint(x: centre.x + dx * inner, y: centre.y + dy * inner))
                path.line(to: NSPoint(x: centre.x + dx * end, y: centre.y + dy * end))
                path.lineWidth = width
                path.lineCapStyle = .round
                return path
            }

            if !small {
                track.setStroke()
                for index in rays.indices { ray(index, length: 1).stroke() }
            }
            for (index, value) in rays.enumerated() {
                value.color.setStroke()
                ray(index, length: value.length).stroke()
            }

            hub.setFill()
            NSBezierPath(
                ovalIn: NSRect(
                    x: centre.x - hubRadius, y: centre.y - hubRadius,
                    width: hubRadius * 2, height: hubRadius * 2)
            ).fill()

            return true
        }
    }

    /// Every slot `iconutil` expects. Missing one does not fail the build — it just
    /// makes macOS scale a neighbour, which is where a soft, slightly wrong icon in
    /// the Dock comes from.
    private static let slots: [(name: String, points: CGFloat, scale: CGFloat)] = [
        ("icon_16x16", 16, 1), ("icon_16x16@2x", 16, 2),
        ("icon_32x32", 32, 1), ("icon_32x32@2x", 32, 2),
        ("icon_128x128", 128, 1), ("icon_128x128@2x", 128, 2),
        ("icon_256x256", 256, 1), ("icon_256x256@2x", 256, 2),
        ("icon_512x512", 512, 1), ("icon_512x512@2x", 512, 2),
    ]

    static let flag = "--render-icon"

    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    /// Write a full `.iconset` directory, which `iconutil -c icns` turns into the
    /// bundle's icon.
    static func writeIconSet(to directory: URL) -> Never {
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
        } catch {
            FileHandle.standardError.write(Data("cannot create \(directory.path)\n".utf8))
            exit(1)
        }

        var written = 0
        for slot in slots {
            let pixels = slot.points * slot.scale
            let image = image(size: pixels)
            guard let tiff = image.tiffRepresentation,
                let rep = NSBitmapImageRep(data: tiff),
                let png = rep.representation(using: .png, properties: [:])
            else {
                FileHandle.standardError.write(Data("render failed: \(slot.name)\n".utf8))
                continue
            }
            let url = directory.appendingPathComponent("\(slot.name).png")
            do {
                try png.write(to: url)
                written += 1
            } catch {
                FileHandle.standardError.write(Data("write failed: \(slot.name)\n".utf8))
            }
        }
        print("wrote \(written)/\(slots.count) icon slots into \(directory.path)")
        exit(written == slots.count ? 0 : 1)
    }
}
