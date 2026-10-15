import AppKit
import SwiftUI
import TcrBarCore

/// The panel's four text roles.
///
/// ## Truncation is allowed; losing the content is not
///
/// All four end in `.lineLimit(1)` + `.truncationMode(.tail)`, and until this
/// none of them offered the full value anywhere: `rg '\.help\(' PanelV4/`
/// returned three hits, all on Buttons, and `rg textSelection PanelV4/` returned
/// nothing. The hidden text is the IDENTITY of the thing and no second place on
/// the panel says it — 8 of 8 mono command rows on the Tools tab end in an
/// ellipsis, and `dave@example.com` renders as `dave@exam…`. The approved
/// mockup carries the full command as a `title=` on 7 of 7 mono spans; the
/// transcription dropped the whole hover layer.
///
/// So every role carries `.help(text)` — the pointer route, the mockup's own
/// `title=` — and `.accessibilityValue(text)`, the spoken one. ``MonoText``
/// adds `.textSelection(.enabled)`: a command you cannot read is one thing, a
/// command you cannot copy is another.

/// `.name` — an account or session name. 15 pt / 600 / -0.005em, `ink`.
struct NameText: View {
    let text: String
    /// One line, as every other role here defaults to. Opt-in to two for a
    /// name line that is a SENTENCE rather than an identifier: the request to
    /// pair reads `<name> wants to pair`, and a Mac with a long name lost the
    /// verb to the ellipsis, which left the one line that says a stranger is
    /// asking for something saying only that a stranger exists.
    var lineLimit: Int? = 1
    var body: some View {
        Text(text)
            .font(V4.font(V4.nameSize, .semibold))
            .tracking(V4.nameTracking)
            .foregroundStyle(Tok.ink)
            .lineLimit(lineLimit)
            .truncationMode(.tail)
            .frame(minHeight: V4.lineHeight(V4.nameSize), alignment: .leading)
            .help(text)
            .accessibilityValue(text)
    }
}

/// `.dim` — the secondary line: `repo · model`, the quota grid, a metric line.
struct DimText: View {
    let text: String
    var lineLimit: Int? = 1
    var body: some View {
        Text(text)
            .font(V4.font(V4.dimSize))
            .foregroundStyle(Tok.dim)
            .lineLimit(lineLimit)
            .truncationMode(.tail)
            .frame(minHeight: V4.lineHeight(V4.dimSize), alignment: .leading)
            .help(text)
            .accessibilityValue(text)
    }
}

/// `.mute` — the tertiary line: the plan line, a tool's owner, the footer.
///
/// ## No line limit means WRAP, never truncate
///
/// A `lineLimit(nil)` with no `fixedSize` truncates under vertical pressure
/// instead of wrapping, and the panel is under vertical pressure by
/// construction. The knock card's help line rendered `nothing is share…` in the
/// running panel while the block beside it, which carries `fixedSize`, wrapped
/// whole: one sentence cut and one sentence not, on one card, from this one
/// missing modifier. `emptyCard` had the same latent defect.
///
/// So the modifier is applied when and ONLY when `lineLimit == nil`. A call
/// site that asked for one line still gets one line and still ellipsises, which
/// is what a bounded identifier on a shared row needs.
struct MuteText: View {
    let text: String
    var lineLimit: Int? = 1
    var body: some View {
        Text(text)
            .font(V4.font(V4.muteSize))
            .foregroundStyle(Tok.mute)
            .lineLimit(lineLimit)
            .truncationMode(.tail)
            .fixedSize(horizontal: false, vertical: lineLimit == nil)
            .frame(minHeight: V4.lineHeight(V4.muteSize), alignment: .leading)
            .help(text)
            .accessibilityValue(text)
    }
}

/// `.tool .stat` — the cpu/memory figure on a Tools-tab running row.
///
/// `MuteText` in every respect but the size, which is the mockup's own 11 px
/// for this one element (``V4/toolRowStatSize``). Its own type rather than a
/// size parameter on `MuteText`: the mockup gives this element its own rule,
/// and a shared view with a size knob is how one call site quietly restyles a
/// token for everyone.
struct StatText: View {
    let text: String
    var body: some View {
        Text(text)
            .font(V4.font(V4.toolRowStatSize))
            .foregroundStyle(Tok.mute)
            .lineLimit(1)
            .truncationMode(.tail)
            .frame(minHeight: V4.lineHeight(V4.muteSize), alignment: .leading)
    }
}

/// `.mono` — a tool call's command head. Ellipsised at the tail, and never
/// allowed to push the thing beside it off the row.
///
/// `lineLimit` is 1 where the command shares a row with something else, and
/// ``V4/commandLineLimit`` where it has a full-width line of its own on the
/// Tools tab.
struct MonoText: View {
    let text: String
    var lineLimit: Int = 1
    var body: some View {
        Text(text)
            .font(V4.mono(V4.monoSize))
            .foregroundStyle(Tok.ink)
            .lineLimit(lineLimit)
            .truncationMode(.tail)
            .textSelection(.enabled)
            .help(text)
            .accessibilityValue(text)
    }
}

/// A sentence that names commands between backticks (``CommandProse``), in the font and
/// colour its call site sets: each command in the mono face at the sentence's size,
/// without its backticks and never broken inside one of its words, and a menu that copies
/// each one as written, `Copy “tcr run --codex”`. A path in the prose, such as the one tcr's
/// not-pooled reason names, is never broken either. A sentence that names no command is
/// one `Text`, with no menu.
///
/// Not selectable, unlike ``MonoText``. The drawn text carries word joiners, and a
/// drag-select would copy them: the pasted command would name a file that does not exist,
/// with nothing on screen to say why, which is the rule `FleetView`'s email break hint
/// keeps for the same reason. The menu copies the sentence's own commands instead, and
/// VoiceOver reads the sentence as written, without its backticks.
struct CommandProseText: View {
    let text: String
    /// The sentence's point size, at which its commands are drawn in the mono face.
    let size: CGFloat

    var body: some View {
        let commands = CommandProse.commands(in: text)
        if commands.isEmpty {
            Text(verbatim: CommandProse.keepingPathsWhole(text))
                .accessibilityLabel(text)
        } else {
            drawn
                .contextMenu {
                    ForEach(commands, id: \.self) { command in
                        Button("Copy “\(command)”") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(command, forType: .string)
                        }
                    }
                }
                .accessibilityLabel(CommandProse.spoken(text))
        }
    }

    /// The sentence as ``CommandProse/drawnRuns(_:)`` draws it: the prose with its paths
    /// kept whole, and each command, in the mono face, with no break inside its words.
    private var drawn: Text {
        CommandProse.drawnRuns(text).reduce(Text(verbatim: "")) { sentence, run in
            switch run {
            case .prose(let prose):
                return sentence + Text(verbatim: prose)
            case .command(let command):
                return sentence + Text(verbatim: command).font(V4.mono(size))
            }
        }
    }
}
