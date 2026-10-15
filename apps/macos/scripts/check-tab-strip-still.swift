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
//   - every such box lies inside a box the 99+ cap draws, the worst case.
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
func diffBoxes(_ a: Pixels, _ b: Pixels, gap: Int = 6) -> [Box] {
    var columns: [Int: (Int, Int)] = [:]
    for y in 0..<a.height {
        for x in 0..<a.width {
            let i = (y * a.width + x) * 4
            if a.rgba[i] != b.rgba[i] || a.rgba[i + 1] != b.rgba[i + 1]
                || a.rgba[i + 2] != b.rgba[i + 2] || a.rgba[i + 3] != b.rgba[i + 3]
            {
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

// At the render's 2x scale. The 99+ badge is about 33 pt wide and 13 pt tall;
// a box past these is a label or a pill that moved, not a badge.
let maxBoxWidth = 2 * 40
let maxBoxHeight = 2 * 18
let badgedTabs = 3
let sets = ["1", "9", "10", "99", "cap", "mixed"]

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
