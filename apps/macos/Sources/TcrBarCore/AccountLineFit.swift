import AppKit

/// How wide the parts of a live account line are (option H), so that every
/// line in the panel draws its `5h` and `7d` labels and every bar starts at the
/// same x, in a group box or out of one (lead ruling on H1b, 2026-10-14).
///
/// H1 let each row drop its labels when it did not fit (`ViewThatFits`). Inside
/// a group box a line has 16 pt less, so whether a row kept its labels came down
/// to its percent digits, at a margin of zero: "44%" kept them and "31%" lost
/// them, and two rows of one group could disagree. Nothing here is decided per
/// row. The bar width and the name column are chosen once for the panel, at the
/// widest the digits can draw ("100%"), and the only switch left is the
/// panel-wide density, which changes the type sizes every line is measured at.
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
        /// The room held for the signal at the right ("ready", "in 3d 22h").
        public let signalWidth: CGFloat
        /// Option H's name column outside a group, kept when every name fits it.
        public let nameMinWidth: CGFloat
        /// Option H's bar, the widest a bar is drawn.
        public let barMaxWidth: CGFloat
        /// The label's type size.
        public let labelSize: CGFloat
        /// The percent's type size.
        public let percentSize: CGFloat

        public init(
            dot: CGFloat, gap: CGFloat, meterGap: CGFloat, labelMinWidth: CGFloat,
            percentMinWidth: CGFloat, signalWidth: CGFloat, nameMinWidth: CGFloat,
            barMaxWidth: CGFloat, labelSize: CGFloat, percentSize: CGFloat
        ) {
            self.dot = dot
            self.gap = gap
            self.meterGap = meterGap
            self.labelMinWidth = labelMinWidth
            self.percentMinWidth = percentMinWidth
            self.signalWidth = signalWidth
            self.nameMinWidth = nameMinWidth
            self.barMaxWidth = barMaxWidth
            self.labelSize = labelSize
            self.percentSize = percentSize
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
    /// The widest a percent draws: a full window.
    public static let widestPercent = "100%"

    /// The label column: the widest label, or the token's minimum.
    public static func labelColumn(_ m: Metrics) -> CGFloat {
        let widest =
            labels.map { NameRowFit.textWidth($0, size: m.labelSize, weight: .regular) }.max()
            ?? 0
        return max(m.labelMinWidth, widest.rounded(.up))
    }

    /// The percent column: wide enough for "100%", so no row's digits move the
    /// bar after it.
    public static func percentColumn(_ m: Metrics) -> CGFloat {
        let widest = NameRowFit.textWidth(widestPercent, size: m.percentSize, weight: .regular)
        return max(m.percentMinWidth, widest.rounded(.up))
    }

    /// One meter: label, bar, percent.
    public static func meterWidth(bar: CGFloat, _ m: Metrics) -> CGFloat {
        labelColumn(m) + m.meterGap + bar + m.meterGap + percentColumn(m)
    }

    /// A whole labelled line: dot, name, two meters, signal, and the gaps
    /// between them.
    public static func lineWidth(name: CGFloat, bar: CGFloat, _ m: Metrics) -> CGFloat {
        m.dot + m.gap + name + m.gap + meterWidth(bar: bar, m) + m.gap
            + meterWidth(bar: bar, m) + m.gap + m.signalWidth
    }

    /// The one bar width for every line in the panel: option H's bar, or less
    /// when a grouped line at the narrowest name column (option H's, less the
    /// group's inset) would not fit both meters at "100%". A whole point, so
    /// every bar draws on the same pixels.
    public static func barWidth(_ room: Room, _ m: Metrics) -> CGFloat {
        let name = max(0, m.nameMinWidth - room.groupInset)
        let spare = room.groupedWidth - lineWidth(name: name, bar: 0, m)
        return max(0, min(m.barMaxWidth, (spare / 2).rounded(.down)))
    }

    /// The name column of a line OUTSIDE a group. A line inside one draws
    /// `room.groupInset` less, so its bars start at the same x as the lines
    /// above and below it.
    ///
    /// - Parameters:
    ///   - widestLoose: the widest name drawn outside a group, `0` for none.
    ///   - widestGrouped: the widest name drawn inside a group, `nil` when no
    ///     live line is in a group.
    ///
    /// Option H's column at the least, grown to the widest name, and never past
    /// the room the rest of the line leaves: inside a group when any live line
    /// is in one, since that is the narrower box. Past that the name truncates
    /// and the labels stay.
    public static func nameColumn(
        widestLoose: CGFloat, widestGrouped: CGFloat?, bar: CGFloat, _ room: Room,
        _ m: Metrics
    ) -> CGFloat {
        let rest = lineWidth(name: 0, bar: bar, m)
        var widest = widestLoose
        var most = room.looseWidth - rest
        if let widestGrouped {
            widest = max(widest, widestGrouped + room.groupInset)
            most = min(most, room.groupedWidth - rest + room.groupInset)
        }
        return max(0, min(max(widest.rounded(.up), m.nameMinWidth), most.rounded(.down)))
    }
}
