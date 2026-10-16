// Proves the panel keeps one height on every tab.
//
// Reads the `27-one-height-*` PNGs that
// `TcrBar --render-states <dir> --only one-height` writes: one fleet drawn on the
// Accounts, Sessions, Tools and Peers tabs, in both appearances. For each
// appearance it fails unless:
//
//   - the four images are the same size, so switching tabs moves nothing below
//     the strip and the window does not resize;
//   - against the Accounts image, everything from the top edge down to the end
//     of the tab strip is identical outside at most two boxes, the pill that
//     lost the selection and the pill that gained it, each no bigger than one
//     tab. A title or a strip moved by one pixel makes a change as wide as the
//     panel and fails here;
//   - each of the other three images differs from Accounts in the strip at all.
//     A render where the selection did not move would pass the two checks above
//     for the wrong reason.
//
// Gil, 2026-10-16: "can you make also it to not change the height?"
//
// Usage: swift apps/macos/scripts/check-tabs-one-height.swift <render-dir>
// Exit 0 when the tabs share one height, 1 on any finding, 2 on bad input.

import AppKit

struct Box: CustomStringConvertible {
    var minX: Int
    var maxX: Int
    var minY: Int
    var maxY: Int
    var width: Int { maxX - minX + 1 }
    var height: Int { maxY - minY + 1 }
    var description: String { "x \(minX)...\(maxX) y \(minY)...\(maxY) (\(width)x\(height) px)" }
}

/// How far apart two channels may be and still count as the same pixel.
///
/// Not zero, because the tabs' title text is not drawn bit-for-bit alike: on
/// 2026-10-16 the title rows differed by 1 to 4 levels in 255 between tabs (at
/// most 4, on 2 pixels), while two renders of one tab matched exactly, so it is
/// the compositing of different content, not noise between runs. A title or a
/// strip moved by one point changes its edges by far more than this, which is
/// what this check exists to see, and that was checked by moving the title.
let channelTolerance = 6

struct Pixels {
    let width: Int
    let height: Int
    let rgba: [UInt8]

    func same(_ other: Pixels, x: Int, y: Int) -> Bool {
        let i = (y * width + x) * 4
        return (0..<4).allSatisfy { abs(Int(rgba[i + $0]) - Int(other.rgba[i + $0])) <= channelTolerance }
    }

    func rowMatches(_ other: Pixels, y: Int) -> Bool {
        (0..<width).allSatisfy { same(other, x: $0, y: y) }
    }
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

/// The differing pixels in rows `0..<rows`, grouped into boxes by column: a run
/// of columns with any difference, split where `gap` clean columns pass.
func diffBoxes(_ a: Pixels, _ b: Pixels, rows: Int, gap: Int = 6) -> [Box] {
    var columns: [Int: (Int, Int)] = [:]
    for y in 0..<rows {
        for x in 0..<a.width where !a.same(b, x: x, y: y) {
            let old = columns[x] ?? (y, y)
            columns[x] = (min(old.0, y), max(old.1, y))
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
    FileHandle.standardError.write(Data("usage: check-tabs-one-height.swift <render-dir>\n".utf8))
    exit(2)
}
let directory = URL(fileURLWithPath: arguments[1])

// At the render's 2x scale. A tab is about a quarter of the 352 pt strip and
// 28 to 32 pt tall plus its badge's 5 pt lift; the strip ends well inside the
// top 100 pt (header 44 pt, strip 38 pt).
let maxBoxWidth = 2 * 110
let maxBoxHeight = 2 * 44
let stripEndsBy = 2 * 100
let others = ["sessions", "tools", "peers"]

var findings: [String] = []
var compared = 0
for appearance in ["dark", "light"] {
    func url(_ tab: String) -> URL {
        directory.appendingPathComponent("27-one-height-\(tab)-auto-\(appearance).png")
    }
    guard let accounts = load(url("accounts")) else {
        FileHandle.standardError.write(
            Data("missing \(url("accounts").path): run --render-states <dir> --only one-height\n".utf8))
        exit(2)
    }
    for tab in others {
        guard let image = load(url(tab)) else {
            FileHandle.standardError.write(Data("missing \(url(tab).path)\n".utf8))
            exit(2)
        }
        compared += 1
        guard image.width == accounts.width, image.height == accounts.height else {
            findings.append(
                "\(tab) \(appearance): \(image.width)x\(image.height) px, "
                    + "Accounts is \(accounts.width)x\(accounts.height) px")
            continue
        }
        // The first row that differs is the top of the two pills; the first
        // clean row after it is the bottom of the strip.
        guard let first = (0..<image.height).first(where: { !image.rowMatches(accounts, y: $0) })
        else {
            findings.append("\(tab) \(appearance): identical to Accounts, so the selection never moved")
            continue
        }
        let end =
            (first..<image.height).first(where: { image.rowMatches(accounts, y: $0) }) ?? image.height
        if end > stripEndsBy {
            findings.append(
                "\(tab) \(appearance): the rows from \(first) to \(end) px all differ from Accounts, "
                    + "past where the strip ends, so something above the content moved")
            continue
        }
        let boxes = diffBoxes(image, accounts, rows: end)
        print(
            "\(tab) \(appearance): \(image.width)x\(image.height) px, strip rows \(first)..<\(end) "
                + "differ from Accounts in \(boxes)")
        if boxes.count > 2 {
            findings.append(
                "\(tab) \(appearance): \(boxes.count) changed regions in the strip, want at most two "
                    + "(the pill that lost the selection and the one that gained it): \(boxes)")
        }
        for box in boxes where box.width > maxBoxWidth || box.height > maxBoxHeight {
            findings.append("\(tab) \(appearance): a change in the strip too big to be one tab, \(box)")
        }
    }
}

for finding in findings { print("FAIL \(finding)") }
print("compared \(compared) tabs against Accounts: \(findings.count) findings")
exit(findings.isEmpty ? 0 : 1)
