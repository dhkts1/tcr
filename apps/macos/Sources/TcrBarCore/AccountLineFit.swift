import AppKit

/// How wide the parts of a live account line are (option H), so that every
/// line in the panel draws its `5h` and `7d` labels and every bar starts at the
/// same x, in a group box or out of one (lead ruling on H1b, 2026-10-14).
///
/// H1 let each row drop its labels when it did not fit (`ViewThatFits`). Inside
/// a group box a line has 16 pt less, so whether a row kept its labels came down
/// to its percent digits, at a margin of zero: "44%" kept them and "31%" lost
/// them, and two rows of one group could disagree. Nothing here is decided per
/// row. The bar width and the name column are chosen once for the panel, the
/// percent column at the widest the digits draw ("100%"), and the only switch
/// left is the panel-wide density, which changes the type sizes every line is
/// measured at.
///
/// The signal at the right is sized to the widest signal the panel actually
/// draws, not to a reserve, and what that frees goes to the names (lead ruling,
/// 2026-10-14). A name that still does not fit is cut in the MIDDLE
/// (``fitName(_:width:_:)``), so `henry9` and `henry10` stay two names.
///
/// Pure and in the library for the reason ``NameRowFit`` is: the test target
/// links `TcrBarCore` only, and the view and the tests must measure with one
/// function. The view passes its own `V4` tokens in, so what is drawn is what
/// was measured.
public enum AccountLineFit {
    /// The line's tokens. The view builds this from `V4`; a test builds it from
    /// the same numbers.
    public struct Metrics: Equatable, Sendable {
        /// `.dot`'s diameter.
        public let dot: CGFloat
        /// The gap between the line's parts (`.slim{gap}`).
        public let gap: CGFloat
        /// The gap inside a meter, between its label, bar and percent.
        public let meterGap: CGFloat
        /// The least a meter's label column takes (`.sl{width}`).
        public let labelMinWidth: CGFloat
        /// The least a meter's percent column takes (`.mp{width}`).
        public let percentMinWidth: CGFloat
        /// Option H's room for the signal. Only the bar width is sized against
        /// it, so the bar does not change with what the signals say; the name
        /// column is sized against the signals actually drawn.
        public let signalWidth: CGFloat
        /// Option H's name column outside a group, kept when every name fits it.
        public let nameMinWidth: CGFloat
        /// Option H's bar, the widest a bar is drawn.
        public let barMaxWidth: CGFloat
        /// The name's type size (semibold).
        public let nameSize: CGFloat
        /// The label's type size.
        public let labelSize: CGFloat
        /// The percent's type size.
        public let percentSize: CGFloat
        /// The signal's type size.
        public let signalSize: CGFloat

        public init(
            dot: CGFloat, gap: CGFloat, meterGap: CGFloat, labelMinWidth: CGFloat,
            percentMinWidth: CGFloat, signalWidth: CGFloat, nameMinWidth: CGFloat,
            barMaxWidth: CGFloat, nameSize: CGFloat, labelSize: CGFloat,
            percentSize: CGFloat, signalSize: CGFloat
        ) {
            self.dot = dot
            self.gap = gap
            self.meterGap = meterGap
            self.labelMinWidth = labelMinWidth
            self.percentMinWidth = percentMinWidth
            self.signalWidth = signalWidth
            self.nameMinWidth = nameMinWidth
            self.barMaxWidth = barMaxWidth
            self.nameSize = nameSize
            self.labelSize = labelSize
            self.percentSize = percentSize
            self.signalSize = signalSize
        }
    }

    /// Where the lines sit: `looseWidth` is the width a line has in the tab's
    /// own box; inside a group box a line starts `groupInset` further in on
    /// each side, the group's own padding.
    public struct Room: Equatable, Sendable {
        public let looseWidth: CGFloat
        public let groupInset: CGFloat

        public init(looseWidth: CGFloat, groupInset: CGFloat) {
            self.looseWidth = looseWidth
            self.groupInset = groupInset
        }

        /// The width a line has inside a group box.
        public var groupedWidth: CGFloat { looseWidth - 2 * groupInset }
    }

    /// The two meters' labels, measured whole.
    public static let labels = ["5h", "7d"]
    /// The labels reserved by the panel's shared meter columns.
    public static func panelLabels(_ accounts: [Account]) -> [String] {
        var result = labels
        for account in accounts {
            for label in account.lineSlotLabels.compactMap({ $0 }) where !result.contains(label) {
                result.append(label)
            }
        }
        return result
    }

    /// The widest a percent draws: a full window.
    public static let widestPercent = "100%"
    /// What a cut name shows where its middle was.
    public static let ellipsis = "\u{2026}"

    /// The width `Text` draws `text` at in the panel's font: the system face
    /// with TABULAR digits, because `V4.font` adds `.monospacedDigit()`. A
    /// proportional measure reads "100%" two points narrower than it draws.
    public static func textWidth(_ text: String, size: CGFloat, weight: NSFont.Weight)
        -> CGFloat
    {
        (text as NSString)
            .size(withAttributes: [
                .font: NSFont.monospacedDigitSystemFont(ofSize: size, weight: weight)
            ]).width
    }

    /// The label column: the widest label, or the token's minimum.
    ///
    /// `labels` is every label the panel's lines draw. A Codex plan's window can
    /// be labelled by its length (`30d`), and the column holds the widest of them
    /// so every bar still starts at one x. A fleet whose lines draw only `5h` and
    /// `7d` passes nothing and gets the column it always had.
    public static func labelColumn(_ m: Metrics, labels: [String] = AccountLineFit.labels)
        -> CGFloat
    {
        let widest =
            labels.map { textWidth($0, size: m.labelSize, weight: .regular) }.max() ?? 0
        return max(m.labelMinWidth, widest.rounded(.up))
    }

    /// The percent column: wide enough for "100%" in tabular digits, so no
    /// row's digits move the bar after it.
    public static func percentColumn(_ m: Metrics) -> CGFloat {
        let widest = textWidth(widestPercent, size: m.percentSize, weight: .regular)
        return max(m.percentMinWidth, widest.rounded(.up))
    }

    /// One meter: label, bar, percent.
    public static func meterWidth(
        bar: CGFloat, _ m: Metrics, labels: [String] = AccountLineFit.labels
    ) -> CGFloat {
        labelColumn(m, labels: labels) + m.meterGap + bar + m.meterGap + percentColumn(m)
    }

    /// A whole labelled line: dot, name, two meters, signal, and the gaps
    /// between them.
    public static func lineWidth(
        name: CGFloat, bar: CGFloat, signal: CGFloat, _ m: Metrics,
        labels: [String] = AccountLineFit.labels
    ) -> CGFloat {
        m.dot + m.gap + name + m.gap + meterWidth(bar: bar, m, labels: labels) + m.gap
            + meterWidth(bar: bar, m, labels: labels) + m.gap + signal
    }

    /// The signal column: the widest of `signals` as drawn.
    public static func signalColumn(_ signals: [String], _ m: Metrics) -> CGFloat {
        let widest = signals.map { textWidth($0, size: m.signalSize, weight: .regular) }.max()
        return (widest ?? 0).rounded(.up)
    }

    /// The one bar width for every line in the panel: option H's bar, or less
    /// when a grouped line at the narrowest name column (option H's, less the
    /// group's inset) and option H's signal room would not fit both meters at
    /// "100%". A whole point, so every bar draws on the same pixels.
    public static func barWidth(
        _ room: Room, _ m: Metrics, labels: [String] = AccountLineFit.labels
    ) -> CGFloat {
        let name = max(0, m.nameMinWidth - room.groupInset)
        let spare =
            room.groupedWidth
            - lineWidth(name: name, bar: 0, signal: m.signalWidth, m, labels: labels)
        return max(0, min(m.barMaxWidth, (spare / 2).rounded(.down)))
    }

    /// The name column of a line OUTSIDE a group. A line inside one draws
    /// `room.groupInset` less, so its bars start at the same x as the lines
    /// above and below it.
    ///
    /// - Parameters:
    ///   - looseNames: the names drawn on live lines outside a group.
    ///   - groupedNames: the names drawn on live lines inside a group, `nil`
    ///     when no live line is in one.
    ///   - signals: every live line's signal as drawn, which sizes the column
    ///     at the right (``signalColumn(_:_:)``).
    ///
    /// Option H's column at the least, grown to the widest name, and never past
    /// the room the rest of the line leaves: inside a group when any live line
    /// is in one, since that is the narrower box. Past that the name is cut
    /// (``fitName(_:width:_:)``) and the labels stay.
    public static func nameColumn(
        looseNames: [String], groupedNames: [String]?, signals: [String], bar: CGFloat,
        _ room: Room, _ m: Metrics, labels: [String] = AccountLineFit.labels
    ) -> CGFloat {
        let rest = lineWidth(
            name: 0, bar: bar, signal: signalColumn(signals, m), m, labels: labels)
        var widest = widestName(looseNames, m)
        var most = room.looseWidth - rest
        if let groupedNames {
            widest = max(widest, widestName(groupedNames, m) + room.groupInset)
            most = min(most, room.groupedWidth - rest + room.groupInset)
        }
        return max(0, min(max(widest.rounded(.up), m.nameMinWidth), most.rounded(.down)))
    }

    private static func widestName(_ names: [String], _ m: Metrics) -> CGFloat {
        names.map { textWidth($0, size: m.nameSize, weight: .semibold) }.max() ?? 0
    }

    /// `name` as it is drawn in a column `width` wide: whole when it fits, or
    /// else cut in the MIDDLE, keeping as much of both ends as fits, the end
    /// never less than the start (lead ruling, 2026-10-14).
    ///
    /// The end is what tells `henry9` from `henry10`; cut at the tail both
    /// read `henr…`, which is two rows saying the same name. Measured in the
    /// font the line draws (``textWidth(_:size:weight:)``), and the result is
    /// the text drawn, so the view never cuts it again.
    public static func fitName(_ name: String, width: CGFloat, _ m: Metrics) -> String {
        func fits(_ text: String) -> Bool {
            textWidth(text, size: m.nameSize, weight: .semibold) <= width
        }
        if fits(name) { return name }
        let characters = Array(name)
        var keep = characters.count - 1
        while keep > 0 {
            let tail = (keep + 1) / 2
            let head = keep - tail
            let candidate =
                String(characters.prefix(head)) + ellipsis + String(characters.suffix(tail))
            if fits(candidate) { return candidate }
            keep -= 1
        }
        return ellipsis
    }
}
