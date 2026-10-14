import SwiftUI

/// `.sec` — a section's name: 11 pt / 700 / +0.1em, uppercase, `mute`, with a
/// 12 pt glyph and, inline beside it, `.unit`.
///
/// The subtitle is INLINE and lowercase on purpose (`.sec .unit{font-weight:400;
/// letter-spacing:.02em;text-transform:none}`). It carries the section's count,
/// "· 3", and nothing more: an explanation there ("· ring fills toward the 600s
/// timeout") is a caption over the rows, which Gil ruled out on 2026-10-14. A
/// denominator belongs on the mark it measures, as the ring's help does.
struct SectionHead: View {
    let title: String
    var systemImage: String?
    /// `.unit` — what the numbers below are measured against.
    var unit: String?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: V4.sectionHeadGap) {
            if let systemImage {
                Image(systemName: systemImage)
                    .font(.system(size: V4.sectionHeadGlyph))
                    .accessibilityHidden(true)
            }
            Text(title.uppercased())
                .font(V4.font(V4.sectionHeadSize, .bold))
                .tracking(V4.sectionHeadTracking)
            if let unit {
                Text(unit)
                    .font(V4.font(V4.sectionHeadSize))
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
        }
        .foregroundStyle(Tok.mute)
        .frame(minHeight: V4.lineHeight(V4.sectionHeadSize))
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }
}
