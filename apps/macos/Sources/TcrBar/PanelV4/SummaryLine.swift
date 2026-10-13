import SwiftUI
import TcrBarCore

/// `.sum` — the sentence directly under the title. 15 pt, `dim`, numbers at 600,
/// and the role words in their own colours.
///
/// Concatenated `Text`, never an `HStack`: a stack cannot wrap, so it would
/// truncate the tail of the sentence instead of flowing it onto a second line,
/// and the tail is where the money is.
struct SummaryLine: View {
    /// One clause of the sentence, with its own colour and weight.
    ///
    /// The clause is the unit, not the number inside it: the mockup's markup is
    /// `<span class="ok">9 ready</span>`, and `.sum .ok` is the ONLY class in
    /// the sheet that sets a weight. Measured off the mockup's own render, per
    /// word: `9` and `ready` both draw a 5 px stem at 2x, while `3`, `near`,
    /// `limit`, `1` and `unmeasured` all draw 3–4. So `ok` is 600 whole and
    /// every other clause is 400 in its own colour — the sheet colours what the
    /// operator can act on and weights only the headline.
    struct Run {
        let text: String
        var tint: Color = Tok.dim
        var emphasised: Bool = false

        var spoken: String { text }
    }

    /// One or two lines. The Accounts tab draws two — the capacity breakdown,
    /// then spend and cache — because a single wrapped sentence of five clauses
    /// reads as one long number (the design's own `.sum` row).
    let lines: [[Run]]
    var accessibilityText: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(lines.enumerated()), id: \.offset) { _, runs in
                text(for: runs)
                    .lineSpacing(V4.lineSpacing(V4.summarySize))
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(minHeight: V4.lineHeight(V4.summarySize), alignment: .leading)
            }
        }
        .padding(.horizontal, V4.summaryPaddingSide)
        .padding(.bottom, V4.summaryPaddingBottom)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            accessibilityText
                ?? lines.map { $0.map(\.spoken).joined(separator: ", ") }.joined(separator: ". "))
    }

    private func text(for runs: [Run]) -> Text {
        var out = Text("")
        for (index, run) in runs.enumerated() {
            if index > 0 { out = out + fragment(" · ", Tok.mute, .regular) }
            out = out + fragment(run.text, run.tint, run.emphasised ? .semibold : .regular)
        }
        return out
    }

    private func fragment(_ text: String, _ tint: Color, _ weight: Font.Weight) -> Text {
        Text(text)
            .font(V4.font(V4.summarySize, weight))
            .foregroundColor(tint)
    }
}

extension SummaryLine {
    /// The Accounts tab's one line under the title: what it cost (`$41.80
    /// today · cache 95%`), or `nil` when nothing was measured.
    ///
    /// The tally line that led it (`4 ready · 1 near limit · 6 spent`) is gone
    /// (option H, unit H3): the strip at the top of the tab counts the fleet
    /// now ("5 of 13 can take work now"), and two counts of one fleet in two
    /// wordings, one over the other, is one too many.
    static func accounts(_ fleet: Fleet) -> SummaryLine? {
        var second: [Run] = []
        if fleet.hasUsage {
            // `dim` at 400: the mockup's money clause is `.nw`, which sets
            // wrapping and nothing else — `.sum b{color:#fff;font-weight:600}`
            // is a rule for the OTHER two tabs' summaries, where the figure is
            // the subject of the sentence. Here it is one clause of five.
            second.append(Run(text: "\(QuotaFormat.usd(fleet.todayCost)) today"))
            second.append(
                Run(text: "cache \(QuotaFormat.percent(fleet.todayCacheHitRatio))"))
            if let unpriced = fleet.todayUnpricedRequests, unpriced > 0 {
                // Kept even though the mockup has no such case: it says the
                // figure beside it is a FLOOR, and dropping it would make a
                // partially-priced day read as a measured one.
                second.append(Run(text: "\(unpriced) unpriced today", tint: Tok.mute))
            }
        }
        return second.isEmpty ? nil : SummaryLine(lines: [second])
    }
}
