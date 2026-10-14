import AppKit
import XCTest

@testable import TcrBarCore

/// Whether the segmented tab strip fits at FOUR tabs — `docs/design/panel-tabs.md`
/// §0: "the tab bar breaks at four, and that is measured". `feat/peers-panel`
/// declares a fourth `PanelTab.peers` case that has not landed on this branch,
/// so `PanelTab` itself is still three cases here — this test does not wait
/// for that merge to check the four-tab shape the bridge asked for; it reads
/// the three real labels straight out of `FleetView.swift`'s `PanelTab.title`
/// switch and adds the fourth ("Peers") as a literal, documented here as a
/// stand-in for the case this build does not declare yet.
///
/// `SegmentedTabs` is a SwiftUI view this bundle cannot instantiate (the test
/// target links `TcrBarCore` only — `Package.swift`), so the geometry is
/// measured the same way `QuotaTailWidthTests` measures a column it cannot
/// render: `V4` tokens read out of the app target's SOURCE, at the actual
/// system font, real string widths.
final class SegmentedTabsFitTests: XCTestCase {
    /// Every segment's badge count on the measured live fleet the bridge
    /// quotes: 18 accounts, 10 sessions, 3 tools running. Peers carries no
    /// badge — nothing on the wire counts peers yet.
    private let badges: [String: Int] = ["Accounts": 18, "Sessions": 10, "Tools": 3]

    /// Renders the bar at four tabs (three real, one "Peers" stand-in — see
    /// the class doc-comment) and fails while the Accounts segment is the
    /// widest of the four AND overflows its allotted width by a wide margin.
    /// Reads `PanelTab.title`'s REAL `case .accounts` string, so this is red
    /// before the label is shortened and green after — not two fixed-string
    /// fixtures that can never disagree with the code.
    ///
    /// It does NOT assert every segment fits its exact 84–85pt allotment:
    /// `SegmentedTabs` draws a 14pt icon per tab that the bridge's own CSS
    /// mockup does not (`docs/design/panel-tabs.md` §0 was measured against
    /// text + a count badge, no icon), so the real, icon-inclusive four-tab
    /// strip is tighter than the mockup implies — "Sessions", unchanged and
    /// out of scope here, still runs over on its own. What this gate owns is
    /// the ACCOUNTS segment: after the rename it must no longer be the worst
    /// offender, with only a small residual overflow left.
    func testAccountsSegmentIsNoLongerTheWidestOverflowOutlierAtFourTabs() throws {
        let accountsLabel = try panelTabTitle("accounts")
        let allotted = try allottedSegmentWidth(tabCount: 4)
        let widths = try idealWidths(accountsLabel: accountsLabel)
        let accountsWidth = try XCTUnwrap(widths[accountsLabel])
        let widest = try XCTUnwrap(widths.values.max())

        XCTAssertNotEqual(
            accountsWidth, widest,
            "\"\(accountsLabel)\" is still the widest of the four segments — shorten it "
                + "(PanelTab.title's `case .accounts`): \(widths)")
        XCTAssertLessThan(
            accountsWidth - allotted, 5,
            "\"\(accountsLabel)\" still overflows its segment by "
                + "\(String(format: "%.1f", accountsWidth - allotted)) pt — expected under 5pt "
                + "of residual overflow once shortened")
    }

    /// Why the badge is drawn over its tab's corner and not beside its label:
    /// a slot beside the label, reserved at its worst case so a count could
    /// never move the strip, does not fit three badged tabs. Even a slot for a
    /// single digit on Sessions, Tools and Peers overflows the strip once every
    /// tab keeps its inset. If a later change makes room, this goes red and
    /// the trade (``TabStripFit``'s doc-comment) can be revisited.
    func testAnInlineBadgeSlotOnEveryBadgedTabDoesNotFitTheStrip() throws {
        let inset = try token("tabInset")
        let tabGap = try token("tabGap")
        let badgePaddingH = try token("badgePaddingH")
        let labels = try stripTabs(badges: [:]).map(\.content).reduce(0, +)
        let digit = badgeWidth("9", size: try token("badgeSize"))
        let slot = tabGap + digit + 2 * badgePaddingH
        XCTAssertGreaterThan(
            labels + 8 * inset + 3 * slot, try stripContentWidth(),
            "three single-digit badge slots now fit beside the labels, so the badge could "
                + "go back inline at a reserved width")
    }

    /// The four labels, icons and insets fit an equal quarter each, so the
    /// strip is four equal quarters, which is the panel people look at most.
    /// With the badge taking no room that holds at every count.
    func testTheStripIsFourEqualQuartersAtEveryCount() throws {
        let quarter = try allottedSegmentWidth(tabCount: 4)
        for count in [nil, 1, 10, 120] as [Int?] {
            let frames = try stripFrames(
                badges: ["sessions": count, "tools": count, "peers": count])
            for frame in frames {
                XCTAssertEqual(
                    frame.width, quarter, accuracy: 0.01,
                    "a tab is \(frame.width) pt wide, not its \(quarter) pt quarter, at \(String(describing: count))"
                )
            }
        }
    }

    /// Without `lineLimit(1)` and `fixedSize` a label offered a narrower share
    /// than it needs breaks in half rather than asking for its width: the
    /// released panel once drew `Sess` over
    /// `ions`. Pinned at the source, because a wrap is invisible to every
    /// assertion that measures a string instead of a rendered line.
    func testTheTabLabelAsksForItsOwnWidthAndNeverWraps() throws {
        let source = try panelSource("PanelV4/SegmentedTabs.swift")
        let squashed = source.components(separatedBy: .whitespacesAndNewlines).joined()
        let label = try XCTUnwrap(
            squashed.range(of: "Text(tab.title)").map { String(squashed[$0.upperBound...]) },
            "`Text(tab.title)` is no longer in SegmentedTabs in the shape this test reads")
        let modifiers = String(label.prefix(300))
        XCTAssertTrue(
            modifiers.contains(".lineLimit(1)"),
            "the tab label may wrap again: no `.lineLimit(1)` on it")
        XCTAssertTrue(
            modifiers.contains(".fixedSize(horizontal:true,vertical:false)"),
            "the tab label takes an equal quarter of the strip again rather than its own "
                + "width, which is what made \"Sessions\" break in two")
    }

    /// Every tab keeps at least `V4.tabInset` between its pill edge and its
    /// content, on both sides (#480: "the buttons for sessions has no padding,
    /// only accts"), at the counts Gil saw that at and at the widest.
    func testEveryTabKeepsTheInsetAtEveryCount() throws {
        let inset = try token("tabInset")
        let leading = try token("tabIconBox") + (try token("tabGap"))
        for count in [nil, 3, 5, 12, 120] as [Int?] {
            let badges = ["sessions": count, "tools": count, "peers": count]
            let frames = try stripFrames(badges: badges)
            XCTAssertLessThanOrEqual(
                (frames.last.map { $0.x + $0.width } ?? 0),
                try stripContentWidth() + 3 * (try token("segGap")) + 0.01,
                "the strip runs past its width at \(String(describing: count))")
            for frame in frames {
                let before = frame.labelX - leading - frame.x
                let after = frame.x + frame.width - frame.labelX - frame.labelWidth
                XCTAssertGreaterThanOrEqual(
                    min(before, after), inset - 0.01,
                    "a tab's content runs to \(String(format: "%.1f", min(before, after))) pt "
                        + "from its pill edge at \(String(describing: count)), under the "
                        + "\(inset) pt inset")
            }
        }
    }

    /// The view lays the strip out with ``TabStripFit`` and `V4.tabInset`, and
    /// draws the badge as an overlay on the tab's top-trailing corner, so the
    /// arithmetic the tests here check is the arithmetic drawn. An overlay
    /// takes no room; a badge back inside the `HStack` would.
    func testTheStripIsLaidOutByTabStripFit() throws {
        let source = try panelSource("PanelV4/SegmentedTabs.swift")
        XCTAssertTrue(
            source.contains("TabStripFit.widths("),
            "SegmentedTabs no longer sizes its tabs with TabStripFit.widths")
        XCTAssertTrue(
            source.contains("inset: V4.tabInset"),
            "SegmentedTabs no longer passes V4.tabInset to TabStripFit")
        XCTAssertTrue(
            source.contains(".overlay(alignment: .topTrailing)"),
            "the badge is no longer an overlay on the tab's corner")
        XCTAssertTrue(
            source.contains("TabStripFit.badgeText("),
            "the badge no longer draws TabStripFit.badgeText, so its cap is not the one tested")
        let row = try XCTUnwrap(
            source.range(of: "HStack(spacing: V4.tabGap) {").map { String(source[$0.upperBound...]) },
            "the tab's content HStack is not in the shape this test reads")
        let rowBody = try XCTUnwrap(row.components(separatedBy: "\n            }").first)
        XCTAssertFalse(
            rowBody.lowercased().contains("badge"),
            "the badge is back inside the tab's content row, where it takes room and "
                + "moves the strip when a count changes")
    }

    /// Nothing in the strip moves because data changed (Gil, 2026-10-14: "avoid
    /// layout shifts ... whats going on with our tabs is horrible"). Every tab's
    /// frame and every label's frame are the same whether a badge is absent,
    /// shows one digit, two, or the cap, on each tab that can carry one, alone
    /// and all at once.
    func testNoTabOrLabelMovesWhenABadgeAppearsVanishesOrChangesDigits() throws {
        let counts: [Int?] = [nil, 0, 1, 9, 10, 99, 100, 1000, 120_000]
        let baseline = try stripFrames(badges: [:])
        for tab in ["sessions", "tools", "peers"] {
            for count in counts {
                let frames = try stripFrames(badges: [tab: count])
                XCTAssertEqual(
                    frames, baseline,
                    "the strip moved with \(tab) at \(count.map(String.init) ?? "no badge")")
            }
        }
        for count in counts {
            let frames = try stripFrames(
                badges: ["sessions": count, "tools": count, "peers": count])
            XCTAssertEqual(
                frames, baseline,
                "the strip moved with every badge at \(count.map(String.init) ?? "none")")
        }
    }

    /// A count past the cap reads `99+`, so the widest badge is known before
    /// any count arrives.
    func testTheBadgeCapsAtNinetyNinePlus() {
        XCTAssertNil(TabStripFit.badgeText(nil))
        XCTAssertNil(TabStripFit.badgeText(0))
        XCTAssertEqual(TabStripFit.badgeText(1), "1")
        XCTAssertEqual(TabStripFit.badgeText(99), "99")
        XCTAssertEqual(TabStripFit.badgeText(100), "99+")
        XCTAssertEqual(TabStripFit.badgeText(120_000), "99+")
    }

    /// The selection only repaints: `isOn` may choose a colour, a fill, a
    /// shadow, an animation and an accessibility trait, and nothing that takes
    /// room. A selected label in a heavier weight, or a selected pill with its
    /// own padding, would move the strip on every click.
    func testTheSelectionNeverTouchesGeometry() throws {
        let source = try panelSource("PanelV4/SegmentedTabs.swift")
        let lines = source.components(separatedBy: .newlines)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { $0.contains("isOn") && !$0.hasPrefix("//") }
        let allowed = [
            "let isOn = tab == selected", ".foregroundStyle(isOn", ".fill(isOn",
            "color: isOn", ".animation(", ".accessibilityAddTraits(isOn",
        ]
        XCTAssertFalse(lines.isEmpty, "no `isOn` in SegmentedTabs: this test reads nothing")
        for line in lines {
            XCTAssertTrue(
                allowed.contains { line.hasPrefix($0) },
                "the selection reaches something other than paint: `\(line)`")
        }
    }

    /// The four tabs, in the strip's order, with the badge each would draw for
    /// `badges` (tab case name to count).
    private func stripTabs(badges: [String: Int?]) throws -> [TabStripFit.Tab] {
        let leading = try token("tabIconBox") + (try token("tabGap"))
        let labelSize = try token("tabLabelSize_comfortable")
        let tracking = try token("tabLabelTracking")
        let badgeSize = try token("badgeSize")
        let badgePaddingH = try token("badgePaddingH")
        return try ["accounts", "sessions", "tools", "peers"].map { name in
            let label = try panelTabTitle(name)
            let text = TabStripFit.badgeText(badges[name] ?? nil)
            return TabStripFit.Tab(
                leading: leading,
                label: measure(label, size: labelSize, weight: .semibold) + tracking,
                badge: text.map { badgeWidth($0, size: badgeSize) + 2 * badgePaddingH })
        }
    }

    /// The strip's frames for `badges`, from the same ``TabStripFit`` the view
    /// lays out with.
    private func stripFrames(badges: [String: Int?]) throws -> [TabStripFit.Frame] {
        TabStripFit.frames(
            try stripTabs(badges: badges), inset: try token("tabInset"),
            spacing: try token("segGap"), available: try stripContentWidth())
    }

    /// The badge's text width: `V4.font` is the system font with tabular digits.
    private func badgeWidth(_ text: String, size: CGFloat) -> CGFloat {
        (text as NSString).size(withAttributes: [
            .font: NSFont.monospacedDigitSystemFont(ofSize: size, weight: .regular)
        ]).width
    }

    /// The strip's width for its four tabs, after its padding and three gaps.
    private func stripContentWidth() throws -> CGFloat {
        try allottedSegmentWidth(tabCount: 4) * 4
    }

    /// `[label: idealWidth]` for the four segments — the three real tabs plus
    /// the literal "Peers" stand-in (see the class doc-comment).
    private func idealWidths(accountsLabel: String) throws -> [String: CGFloat] {
        let sessions = try panelTabTitle("sessions")
        let tools = try panelTabTitle("tools")
        var out: [String: CGFloat] = [:]
        out[accountsLabel] = try idealSegmentWidth(
            label: accountsLabel, badge: badges["Accounts"])
        out[sessions] = try idealSegmentWidth(label: sessions, badge: badges[sessions])
        out[tools] = try idealSegmentWidth(label: tools, badge: badges[tools])
        out["Peers"] = try idealSegmentWidth(label: "Peers", badge: nil)
        return out
    }

    // MARK: - Geometry, read from V4's own source

    /// `(panelWidth - 2*panelPaddingSide - 2*segPadding - segGap*(n-1)) / n` —
    /// `SegmentedTabs`' own layout: an `HStack` of `n` equal-width buttons
    /// inside `.padding(segPadding)`, inside the panel's own side padding.
    private func allottedSegmentWidth(tabCount: Int) throws -> CGFloat {
        let panelWidth = try token("panelWidth")
        let panelPaddingSide = try token("panelPaddingSide")
        let segPadding = try token("segPadding")
        let segGap = try token("segGap")
        let available =
            panelWidth - 2 * panelPaddingSide - 2 * segPadding - segGap * CGFloat(tabCount - 1)
        return available / CGFloat(tabCount)
    }

    /// icon box + gap + label text + (badge gap + badge pill, if any) — the
    /// exact `HStack` `SegmentedTabs.item(_:)` lays out.
    private func idealSegmentWidth(label: String, badge: Int?) throws -> CGFloat {
        let iconBox = try token("tabIconBox")
        let tabGap = try token("tabGap")
        let labelSize = try token("tabLabelSize_comfortable")
        let tracking = try token("tabLabelTracking")
        let labelWidth = measure(label, size: labelSize, weight: .semibold) + tracking
        var width = iconBox + tabGap + labelWidth
        if let badge {
            let badgeSize = try token("badgeSize")
            let badgePaddingH = try token("badgePaddingH")
            let badgeWidth = measure("\(badge)", size: badgeSize, weight: .regular) + 2 * badgePaddingH
            width += tabGap + badgeWidth
        }
        return width
    }

    private func measure(_ string: String, size: CGFloat, weight: NSFont.Weight) -> CGFloat {
        (string as NSString)
            .size(withAttributes: [.font: NSFont.systemFont(ofSize: size, weight: weight)]).width
    }

    // MARK: - Reading tokens and labels out of the app target's source

    /// `PanelTab.title`'s own switch, one case at a time — pinned at the
    /// source for the same reason ``QuotaTailWidthTests`` reads `V4` there:
    /// this bundle does not link `TcrBar`.
    ///
    /// Scoped to the `enum PanelTab { … }` block specifically, not the whole
    /// file: `FleetView.swift` has at least one OTHER switch over the same
    /// cases with completely different strings (`v4FooterLeading`'s `case
    /// .sessions: return "proxy + Claude Code session files"`), and a
    /// file-wide `firstMatch` grabbed that one instead the first time this
    /// test was written.
    private func panelTabTitle(_ caseName: String) throws -> String {
        let source = try panelSource("FleetView.swift")
        let enumBody = try XCTUnwrap(
            firstCapture(of: "enum PanelTab[^{]*\\{([\\s\\S]*?)\\n\\}", in: source),
            "no `enum PanelTab { ... }` block found in FleetView.swift")
        let pattern = "case \\.\(caseName): return \"([^\"]+)\""
        return try XCTUnwrap(
            firstCapture(of: pattern, in: enumBody),
            "PanelTab.title no longer has a `case .\(caseName): return \"...\"` line in the "
                + "shape this test expects")
    }

    private func token(_ name: String) throws -> CGFloat {
        let source = try panelSource("PanelV4/V4.swift")
        // `tabLabelSize` is a density-computed `static var { compact ? 12 :
        // 12.5 }`, not a plain `static let`. The comfortable (12.5) branch is
        // read here, the larger of the two — a fit proven at the larger size
        // holds at the smaller one too, the same convention
        // `QuotaTailWidthTests.muteSize()` uses.
        if name == "tabLabelSize_comfortable" {
            let squashed = source.components(separatedBy: .whitespacesAndNewlines).joined()
            let value = try XCTUnwrap(
                firstCapture(of: "tabLabelSize:CGFloat\\{compact\\?[0-9.]+:([0-9.]+)\\}", in: squashed),
                "V4.tabLabelSize is no longer `compact ? <n> : <n>`")
            return CGFloat(try XCTUnwrap(Double(value)))
        }
        let pattern = "static let \(name): CGFloat = ([0-9.]+)"
        let value = try XCTUnwrap(
            firstCapture(of: pattern, in: source),
            "V4.\(name) is no longer a plain `static let … = <number>`")
        return CGFloat(try XCTUnwrap(Double(value)))
    }

    private func firstCapture(of pattern: String, in source: String) -> String? {
        guard let regex = try? NSRegularExpression(pattern: pattern),
            let match = regex.firstMatch(
                in: source, range: NSRange(source.startIndex..., in: source)),
            let range = Range(match.range(at: 1), in: source)
        else { return nil }
        return String(source[range])
    }

    private func panelSource(_ relative: String) throws -> String {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // -> TcrBarTests
            .deletingLastPathComponent()  // -> Tests
            .deletingLastPathComponent()  // -> apps/macos
            .deletingLastPathComponent()  // -> apps
            .deletingLastPathComponent()  // -> repo root
        let file = repoRoot.appendingPathComponent("apps/macos/Sources/TcrBar/\(relative)")
        return try String(contentsOf: file, encoding: .utf8)
    }
}
