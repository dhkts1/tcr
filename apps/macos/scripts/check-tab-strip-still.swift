// Proves the panel's tab strip does not move when a badge count changes.
//
// Reads the `70-tab-strip-*` PNGs that `TcrBar --render-states <dir>` writes:
// one strip per selection, appearance and set of counts (none, 1, 9, 10, 99,
// the 99+ cap, and a mixed set). For each selection and appearance it compares
// every PNG against the one with no badges and fails unless:
//
//   - the images are the same size, so the strip did not grow or shrink;
//   - every pixel that differs lies in at most three small boxes, one per
//     badged tab, no wider than the widest badge and no taller than one, so no
//     label, icon or pill edge moved;
//   - every such box lies inside a box the 99+ cap draws, the worst case;
//   - every badge lies inside its own tab's pill, so it neither crosses the
//     pill's edge nor reaches the neighbouring tab;
//   - no badge reaches the box around its tab's label ink in the no-badge
//     strip, so a badge never covers a letter.
//
// Gil, 2026-10-14: "avoid layout shifts. now whats going on with our tabs is
// horrible". A label that moves by one pixel makes a diff as wide as the strip
// and fails here.
//
// Usage: swift apps/macos/scripts/check-tab-strip-still.swift <render-dir>
// Exit 0 when the strip holds still, 1 on any finding, 2 on bad input.

import AppKit

struct Box: CustomStringConvertible {
    var minX: Int
    var maxX: Int
    var minY: Int
    var maxY: Int
    var width: Int { maxX - minX + 1 }
    var height: Int { maxY - minY + 1 }
    func inside(_ other: Box, slack: Int) -> Bool {
        minX >= other.minX - slack && maxX <= other.maxX + slack
            && minY >= other.minY - slack && maxY <= other.maxY + slack
    }
    var description: String { "x \(minX)...\(maxX) y \(minY)...\(maxY) (\(width)x\(height) px)" }
}

struct Pixels {
    let width: Int
    let height: Int
    let rgba: [UInt8]
}

func load(_ url: URL) -> Pixels? {
    guard let data = try? Data(contentsOf: url), let source = NSBitmapImageRep(data: data),
        let cg = source.cgImage
    else { return nil }
    let width = cg.width
    let height = cg.height
    var rgba = [UInt8](repeating: 0, count: width * height * 4)
    let drawn = rgba.withUnsafeMutableBytes { buffer -> Bool in
        guard
            let context = CGContext(
                data: buffer.baseAddress, width: width, height: height, bitsPerComponent: 8,
                bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return false }
        context.draw(cg, in: CGRect(x: 0, y: 0, width: width, height: height))
        return true
    }
    return drawn ? Pixels(width: width, height: height, rgba: rgba) : nil
}

/// The differing pixels of `a` against `b`, grouped into boxes by column: a
/// run of columns with any difference, split where `gap` clean columns pass.
///
/// A pixel differs when a channel moves by more than `noise`. The selected
/// pill's shadow renders a level apart from one scene to the next with nothing
/// moved (measured 2026-11-23: up to 62 pixels around the selected Accts pill,
/// every one off by exactly 1), and an exact comparison failed every Accounts
/// strip on that alone. A label or pill that moves changes pixels by far more.
func diffBoxes(_ a: Pixels, _ b: Pixels, gap: Int = 6, noise: Int = 1) -> [Box] {
    var columns: [Int: (Int, Int)] = [:]
    for y in 0..<a.height {
        for x in 0..<a.width {
            let i = (y * a.width + x) * 4
            if (0..<4).contains(where: { abs(Int(a.rgba[i + $0]) - Int(b.rgba[i + $0])) > noise }) {
                let old = columns[x] ?? (y, y)
                columns[x] = (min(old.0, y), max(old.1, y))
            }
        }
    }
    var boxes: [Box] = []
    for x in columns.keys.sorted() {
        guard let (top, bottom) = columns[x] else { continue }
        if var last = boxes.last, x - last.maxX <= gap {
            last.maxX = x
            last.minY = min(last.minY, top)
            last.maxY = max(last.maxY, bottom)
            boxes[boxes.count - 1] = last
        } else {
            boxes.append(Box(minX: x, maxX: x, minY: top, maxY: bottom))
        }
    }
    return boxes
}

let arguments = CommandLine.arguments
guard arguments.count == 2 else {
    FileHandle.standardError.write(Data("usage: check-tab-strip-still.swift <render-dir>\n".utf8))
    exit(2)
}
let directory = URL(fileURLWithPath: arguments[1])

// At the render's 2x scale. The 99+ badge is about 21 pt wide and 11 pt tall;
// a box past these is a label or a pill that moved, not a badge.
let maxBoxWidth = 2 * 40
let maxBoxHeight = 2 * 18
let badgedTabs = 3
let sets = ["1", "9", "10", "99", "cap", "mixed"]

// The pills, at 2x, from `V4`'s tokens as the render draws them (13 accounts, so
// Compact): the strip starts at `panelPaddingSide + segPadding` = 13 pt and
// `panelPaddingTop + segPadding` = 13 pt, each tab is a quarter of the 337 pt
// left after the padding and three `segGap`s (84.25 pt), 3 pt apart, and
// `tabMinHeight` (28 pt Compact) tall. The strip's render is 52 pt tall; a
// different height means a token moved and these numbers need reading again.
let stripHeight = 2 * 52
let pillTop = 2 * 13
let pillBottom = 2 * (13 + 28) - 1
func pillX(_ tab: Int) -> (Int, Int) {
    let x = 2 * (13 + CGFloat(tab) * (84.25 + 3))
    return (Int(x.rounded(.up)), Int((x + 2 * 84.25).rounded(.down)) - 1)
}

/// The box around tab `tab`'s LABEL ink in the no-badge strip `image`, or nil
/// when the tab draws no ink.
///
/// Ink is a pixel more than `threshold` off the pill's commonest colour on any
/// channel; the selected pill's fill and its rounded corner differ from the strip
/// by about 30, so they are not ink. Read column by column inside the pill, ink
/// falls into runs: the icon first, then the label, whose letters sit closer
/// together than the 6 pt (12 px) `tabGap` between icon and label. So every run
/// after the first is the label.
func labelBox(_ image: Pixels, tab: Int, threshold: Int = 48) -> Box? {
    let (left, right) = pillX(tab)
    func pixel(_ x: Int, _ y: Int) -> (Int, Int, Int) {
        let i = (y * image.width + x) * 4
        return (Int(image.rgba[i]), Int(image.rgba[i + 1]), Int(image.rgba[i + 2]))
    }
    var counts: [Int: Int] = [:]
    for y in pillTop...pillBottom {
        for x in left...right {
            let (r, g, b) = pixel(x, y)
            counts[r << 16 | g << 8 | b, default: 0] += 1
        }
    }
    guard let mode = counts.max(by: { $0.value < $1.value })?.key else { return nil }
    let (mr, mg, mb) = (mode >> 16 & 0xff, mode >> 8 & 0xff, mode & 0xff)
    var columns: [Int: (Int, Int)] = [:]
    for y in pillTop...pillBottom {
        for x in left...right {
            let (r, g, b) = pixel(x, y)
            if max(abs(r - mr), abs(g - mg), abs(b - mb)) > threshold {
                let old = columns[x] ?? (y, y)
                columns[x] = (min(old.0, y), max(old.1, y))
            }
        }
    }
    var runs: [Box] = []
    for x in columns.keys.sorted() {
        guard let (top, bottom) = columns[x] else { continue }
        if var last = runs.last, x - last.maxX <= 8 {
            last.maxX = x
            last.minY = min(last.minY, top)
            last.maxY = max(last.maxY, bottom)
            runs[runs.count - 1] = last
        } else {
            runs.append(Box(minX: x, maxX: x, minY: top, maxY: bottom))
        }
    }
    let label = runs.dropFirst()
    guard let first = label.first else { return nil }
    return label.reduce(first) {
        Box(
            minX: min($0.minX, $1.minX), maxX: max($0.maxX, $1.maxX),
            minY: min($0.minY, $1.minY), maxY: max($0.maxY, $1.maxY))
    }
}

var findings: [String] = []
var compared = 0
for selection in ["accounts", "sessions"] {
    for appearance in ["dark", "light"] {
        func url(_ set: String) -> URL {
            directory.appendingPathComponent("70-tab-strip-\(selection)-\(set)-\(appearance).png")
        }
        guard let base = load(url("none")), let cap = load(url("cap")) else {
            FileHandle.standardError.write(
                Data("missing \(url("none").path) or its cap twin: run --render-states\n".utf8))
            exit(2)
        }
        guard base.width == cap.width, base.height == cap.height else {
            findings.append("\(selection) \(appearance): the cap render is a different size")
            continue
        }
        let capBoxes = diffBoxes(cap, base)
        if capBoxes.count != badgedTabs {
            findings.append(
                "\(selection) \(appearance) cap: \(capBoxes.count) changed regions, want "
                    + "\(badgedTabs), one per badged tab: \(capBoxes)")
        }
        guard base.height == stripHeight else {
            findings.append(
                "\(selection) \(appearance): the strip is \(base.height) px tall, not "
                    + "\(stripHeight); a tab token moved, so the pill bounds here are stale")
            continue
        }
        // The cap draws the widest badge every badged tab can show, so a cap box
        // that stays in its pill and off the letters covers every smaller one.
        for box in capBoxes {
            let centre = (box.minX + box.maxX) / 2
            guard let tab = (0..<4).first(where: { pillX($0).0 <= centre && centre <= pillX($0).1 })
            else {
                findings.append("\(selection) \(appearance) cap: a badge between tabs, \(box)")
                continue
            }
            let (left, right) = pillX(tab)
            if box.minX < left || box.maxX > right || box.minY < pillTop || box.maxY > pillBottom {
                findings.append(
                    "\(selection) \(appearance) cap: tab \(tab)'s badge leaves its pill "
                        + "(x \(left)...\(right) y \(pillTop)...\(pillBottom)), \(box)")
            }
            guard let label = labelBox(base, tab: tab) else {
                findings.append("\(selection) \(appearance): tab \(tab) draws no label ink")
                continue
            }
            if box.minX <= label.maxX && label.minX <= box.maxX && box.minY <= label.maxY
                && label.minY <= box.maxY
            {
                findings.append(
                    "\(selection) \(appearance) cap: tab \(tab)'s badge \(box) covers its "
                        + "label \(label)")
            }
        }
        for set in sets {
            guard let image = load(url(set)) else {
                FileHandle.standardError.write(Data("missing \(url(set).path)\n".utf8))
                exit(2)
            }
            compared += 1
            guard image.width == base.width, image.height == base.height else {
                findings.append(
                    "\(selection) \(appearance) \(set): \(image.width)x\(image.height) px, "
                        + "the no-badge strip is \(base.width)x\(base.height)")
                continue
            }
            for box in diffBoxes(image, base) {
                if box.width > maxBoxWidth || box.height > maxBoxHeight {
                    findings.append(
                        "\(selection) \(appearance) \(set): a change too big to be a badge, \(box)")
                } else if !capBoxes.contains(where: { box.inside($0, slack: 2) }) {
                    findings.append(
                        "\(selection) \(appearance) \(set): a change outside every 99+ badge, \(box)")
                }
            }
        }
    }
}

for finding in findings { print("FAIL \(finding)") }
print("compared \(compared) renders against their no-badge strip: \(findings.count) findings")
exit(findings.isEmpty ? 0 : 1)
