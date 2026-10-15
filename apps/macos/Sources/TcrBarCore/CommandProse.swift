import Foundation

/// A sentence that names commands between backticks, the way tcr and TcrBar both write
/// them, as the panel draws it.
///
/// Drawn as written, the backticks read as part of the command, and a line can break
/// wherever the command has a slash or a hyphen: `tcr run --` at the end of one line and
/// `codex` at the start of the next, or a lone `/` before `Applications/…`, which retyped
/// is a relative path. So the panel draws each command without its backticks, with no
/// break left inside any of its words (``unbreakable(_:)``), and keeps each path in the
/// prose around it whole the same way (``keepingPathsWhole(_:)``), such as the
/// `docs/troubleshooting.md` a reason from tcr names. A break between two words stays.
///
/// The sentence itself is unchanged: tests pin it, VoiceOver reads it without its
/// backticks (``spoken(_:)``), and the menu copies each command as written
/// (``commands(in:)``). Only the drawn text (``drawnRuns(_:)``) carries the joiners.
public enum CommandProse {
    /// One stretch of a sentence: words, or a command that was between backticks.
    public enum Run: Equatable, Sendable {
        case prose(String)
        case command(String)
    }

    /// The sentence split at its backticks into prose and commands, in order. A backtick
    /// with no partner after it stays prose, drawn as written, and so does an empty pair.
    public static func runs(_ sentence: String) -> [Run] {
        var runs: [Run] = []
        var prose = ""
        var rest = Substring(sentence)
        while let open = rest.firstIndex(of: "`") {
            let afterOpen = rest.index(after: open)
            guard let close = rest[afterOpen...].firstIndex(of: "`") else { break }
            prose += rest[..<open]
            let command = String(rest[afterOpen..<close])
            if command.isEmpty {
                prose += "``"
            } else {
                if !prose.isEmpty { runs.append(.prose(prose)) }
                prose = ""
                runs.append(.command(command))
            }
            rest = rest[rest.index(after: close)...]
        }
        prose += rest
        if !prose.isEmpty { runs.append(.prose(prose)) }
        return runs
    }

    /// The commands the sentence names, in order and each once: what its menu copies.
    public static func commands(in sentence: String) -> [String] {
        var commands: [String] = []
        for case .command(let command) in runs(sentence) where !commands.contains(command) {
            commands.append(command)
        }
        return commands
    }

    /// The sentence as it is read aloud: its words, without the backticks.
    public static func spoken(_ sentence: String) -> String {
        runs(sentence).map { run in
            switch run {
            case .prose(let text), .command(let text): return text
            }
        }.joined()
    }

    /// The sentence as the panel draws it: each command without its backticks and with no
    /// break inside a word (``unbreakable(_:)``), and the prose around them with every path
    /// kept whole (``keepingPathsWhole(_:)``). For drawing only, like both.
    public static func drawnRuns(_ sentence: String) -> [Run] {
        runs(sentence).map { run in
            switch run {
            case .prose(let prose): return .prose(keepingPathsWhole(prose))
            case .command(let command): return .command(unbreakable(command))
            }
        }
    }

    /// `prose` for drawing, with no line break left inside a word that has a slash in it,
    /// a path: `docs/troubleshooting.md` in a reason tcr sends, or each folder its codex
    /// search names. Such a word is drawn the way ``unbreakable(_:)`` draws a command's word.
    /// Every other word, and every space, is drawn as written.
    public static func keepingPathsWhole(_ prose: String) -> String {
        var drawn = ""
        var word = ""
        for character in prose {
            guard character.isWhitespace else {
                word.append(character)
                continue
            }
            drawn += word.contains("/") ? unbreakable(word) : word
            drawn.append(character)
            word = ""
        }
        return drawn + (word.contains("/") ? unbreakable(word) : word)
    }

    /// The longest command drawn whole, its spaces included: 24 characters of the mono face
    /// are at most 187 pt at the panel's largest text size, 13 pt, so such a command sits
    /// on a line of the narrowest column one is drawn in, a card's.
    static let wholeCommandLimit = 24

    /// `command` for drawing, with no line break left inside any of its words: a word
    /// joiner (U+2060) between every two characters of a word, and a no-break space for a
    /// space inside single quotes, which a shell reads as part of one word. The spaces
    /// between its words stay breakable, except in a command of ``wholeCommandLimit``
    /// characters or fewer, such as `tcr run --codex`, which is drawn whole rather than as
    /// `tcr run` at the end of one line and `--codex` at the start of the next.
    ///
    /// A word that starts with a slash is led by a zero-width space (U+200B), the one break
    /// before it. Unicode's line breaking never starts a line with a slash, even after a
    /// space, so with the breaks after its slashes joined too, such a path had no break left
    /// anywhere near it, and the line broke where it ran out of room: `…/MacOS/tc` above
    /// `r run --codex`.
    ///
    /// For drawing only. The joiners are real characters: this text pasted into a shell
    /// names a file that does not exist, so a view that draws it is never selectable, and
    /// copies ``commands(in:)`` instead.
    public static func unbreakable(_ command: String) -> String {
        let whole = command.count <= wholeCommandLimit
        var drawn = ""
        var quoted = false
        var inWord = false
        for character in command {
            if character == " ", !quoted, !whole {
                drawn.append(" ")
                inWord = false
                continue
            }
            if inWord {
                drawn.append("\u{2060}")
            } else if character == "/" {
                drawn.append("\u{200B}")
            }
            drawn.append(character == " " ? "\u{00A0}" : character)
            if character == "'" { quoted.toggle() }
            inWord = true
        }
        return drawn
    }
}
