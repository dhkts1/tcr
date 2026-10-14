import AppKit
import CoreText
import XCTest

@testable import TcrBarCore

/// A sentence that names commands between backticks, as the panel draws it: each command
/// without its backticks, never broken inside one of its words, and copied as written.
///
/// The sentences are the panel's own: the Codex line with the bundled tcr, the strict-exit
/// statement on a Codex card and the saved sign-in sheet's detail. Names are fake.
final class CommandProseTests: XCTestCase {

    private let bundledTcr = "/Applications/TcrBar.app/Contents/MacOS/tcr"

    /// The Codex line on a Mac with no command-line tcr names two commands, which split
    /// from the prose in order, each copied once and spoken without its backticks.
    func testASentenceSplitsIntoProseAndCommands() {
        let sentence = CodexBanner.noRequests(processes: 2, bundledTcrWithoutCLI: bundledTcr).text
        let first =
            "2 codex processes are running and no Codex request has reached tcr in 5 minutes "
            + "— start codex with "
        let second =
            ". TcrBar found no tcr besides its own: install the command-line tcr "
            + "(README › Install), or start codex with "
        let last = ", the tcr inside TcrBar, which keeps TcrBar from updating until that codex exits."
        let expected: [CommandProse.Run] = [
            .prose(first), .command("tcr run --codex"), .prose(second),
            .command("\(bundledTcr) run --codex"), .prose(last),
        ]
        XCTAssertEqual(CommandProse.runs(sentence), expected)
        XCTAssertEqual(
            CommandProse.commands(in: sentence), ["tcr run --codex", "\(bundledTcr) run --codex"])
        XCTAssertEqual(CommandProse.spoken(sentence), sentence.replacingOccurrences(of: "`", with: ""))

        let saved = CodexSignInCopy.savedDetail(plan: "ChatGPT Pro", bundledTcrWithoutCLI: bundledTcr)
        XCTAssertEqual(
            CommandProse.commands(in: saved), ["tcr run --codex", "\(bundledTcr) run --codex"],
            "a command named twice is copied once")
        XCTAssertEqual(
            CommandProse.commands(
                in: AccountExit.codexStrictStatement(name: "bob@example.com/codex")),
            ["tcr peer account bob@example.com/codex --exits-from local --no-must"])
    }

    /// A backtick with no partner, and an empty pair, stay prose as written. A sentence that
    /// names no command is one run of prose and copies nothing.
    func testAnUnpairedBacktickIsProse() {
        XCTAssertEqual(
            CommandProse.runs("run `tcr status` or ` alone"),
            [.prose("run "), .command("tcr status"), .prose(" or ` alone")])
        XCTAssertEqual(CommandProse.runs("an empty `` pair"), [.prose("an empty `` pair")])
        XCTAssertEqual(
            CommandProse.runs(AccountExit.codexStatement), [.prose(AccountExit.codexStatement)])
        XCTAssertEqual(CommandProse.commands(in: AccountExit.codexStatement), [])
        XCTAssertEqual(CommandProse.runs(""), [])
    }

    /// No line break is left inside a word of a command: between every two characters of a
    /// word sits a word joiner, so a path keeps its leading slash and a flag its hyphens.
    /// The spaces between its words stay breakable. A word that starts with a slash is led
    /// by a zero-width space, the break before it: a line may not start with a slash, even
    /// after a space, so without it the line would have to break inside the path. With
    /// both taken out the text is the command as written.
    func testACommandKeepsEachOfItsWordsWhole() {
        let joiner = Unicode.Scalar(0x2060)!
        for command in [
            "\(bundledTcr) run --codex",
            "tcr peer account bob@example.com/codex --exits-from local --no-must",
        ] {
            let drawn = CommandProse.unbreakable(command)
            let words = drawn.split(separator: " ", omittingEmptySubsequences: false)
            let written = command.split(separator: " ")
            XCTAssertEqual(words.count, written.count, command)
            for (word, writtenWord) in zip(words, written) {
                let ledByBreak = writtenWord.hasPrefix("/")
                XCTAssertEqual(
                    word.hasPrefix("\u{200B}"), ledByBreak,
                    "\(command): \(writtenWord) is led by a zero-width space only if it starts with /")
                let scalars = Array(word.unicodeScalars.dropFirst(ledByBreak ? 1 : 0))
                for (index, scalar) in scalars.enumerated() {
                    XCTAssertEqual(
                        scalar == joiner, index % 2 == 1,
                        "\(command): a break is left inside \(writtenWord)")
                }
            }
            XCTAssertEqual(stripped(drawn), command)
        }
    }

    /// A command short enough to sit on any line the panel draws one on, `tcr run --codex` or
    /// `codex login`, is drawn whole: its spaces do not break either, so no line ends on
    /// `tcr run` above a line that starts `--codex`. A longer one, such as the bundled tcr's
    /// or the strict-exit one, still breaks between its words, which it needs to fit.
    func testAShortCommandIsKeptWhole() {
        for command in ["tcr run --codex", "codex login"] {
            let drawn = CommandProse.unbreakable(command)
            XCTAssertFalse(drawn.contains(" "), "\(command) can break between its words")
            XCTAssertEqual(stripped(drawn).replacingOccurrences(of: "\u{00A0}", with: " "), command)
        }
        for command in [
            "\(bundledTcr) run --codex",
            "tcr peer account bob@example.com/codex --exits-from local --no-must",
        ] {
            XCTAssertTrue(CommandProse.unbreakable(command).contains(" "), command)
        }
    }

    /// In the prose around the commands, a word with a slash in it, a path, is kept whole the
    /// same way: `docs/troubleshooting.md` in a reason tcr sends, or each folder its codex
    /// search names. Every other word is drawn as written, a hyphenated one included.
    func testAPathInProseIsKeptWhole() {
        let prose = "see docs/troubleshooting.md, or /Users/example/.local/bin and re-minted"
        let drawn = CommandProse.keepingPathsWhole(prose)
        XCTAssertEqual(
            drawn,
            "see " + CommandProse.unbreakable("docs/troubleshooting.md,") + " or "
                + CommandProse.unbreakable("/Users/example/.local/bin") + " and re-minted")
        XCTAssertEqual(stripped(drawn), prose)
        XCTAssertEqual(
            CommandProse.drawnRuns("see docs/x.md or run `tcr status`"),
            [
                .prose(CommandProse.keepingPathsWhole("see docs/x.md or run ")),
                .command(CommandProse.unbreakable("tcr status")),
            ])
    }

    /// No line ends inside a word of a command, or inside a path in the prose around one, at
    /// any width that word fits in, up to the panel's. Laid out by CoreText, which sets the
    /// panel's text, in the faces and sizes these sentences are drawn in: the system face
    /// for the prose and the mono face for the commands, at the mute and dim sizes of both
    /// densities. The sentences are the panel's own and tcr's reasons, with fake paths.
    ///
    /// The bundled tcr's path is the case this exists for. Its leading slash may not start a
    /// line, so with no break left inside the path, CoreText broke it wherever the line ran
    /// out: `…/MacOS/tc` and `r run --codex`.
    func testNoLineEndsInsideACommandOrAPath() {
        let sentences = [
            CodexBanner.noRequests(processes: 2, bundledTcrWithoutCLI: bundledTcr).text,
            CodexSignInCopy.savedDetail(plan: "ChatGPT Pro", bundledTcrWithoutCLI: bundledTcr),
            AccountExit.codexStrictStatement(name: "bob@example.com/codex"),
            "Codex is not pooled: the proxy on :3456 is serving the JS teamclaude leaf, which "
                + "covers api.anthropic.com only and is never re-minted — a restart will not "
                + "change this; see docs/troubleshooting.md",
            "could not find the codex CLI (searched TCR_CODEX_BIN, CODEX_BIN, PATH, "
                + "/Users/example/.local/bin, /Users/example/.cargo/bin, /opt/homebrew/bin, "
                + "/usr/local/bin, /Users/example/.npm-global/bin). Install it, or set "
                + "TCR_CODEX_BIN to its path. Nothing was written.",
            "codex needs a login of its own to start, even when pooled, and none is at "
                + "/Users/example/.codex/auth.json: unless codex keeps its login in the keyring, "
                + "run `codex login` once",
        ]
        for size in [11.0, 12.0, 13.0] {
            for sentence in sentences {
                let (drawn, whole) = laidOut(sentence, size: size)
                let widest = whole.map { width(of: drawn.attributedSubstring(from: $0)) }.max() ?? 0
                breaking: for width in stride(from: max(200, widest.rounded(.up) + 1), through: 372, by: 1) {
                    for end in lineEnds(drawn, width: width) {
                        guard let word = whole.first(where: { $0.location < end && end < NSMaxRange($0) })
                        else { continue }
                        XCTFail(
                            "at \(size) pt in \(width) pt a line ends inside "
                                + stripped((drawn.string as NSString).substring(with: word))
                                + ": \(sentence)")
                        break breaking
                    }
                }
            }
        }
    }

    /// A shell word in single quotes holds its spaces: they are drawn as no-break spaces,
    /// so a quoted path with a space in it is never broken either.
    func testAQuotedWordKeepsItsSpaces() {
        let command = "'/Applications/Tools Folder/TcrBar.app/Contents/MacOS/tcr' run --codex"
        let drawn = CommandProse.unbreakable(command)
        XCTAssertEqual(drawn.split(separator: " ").count, 3, drawn)
        XCTAssertEqual(stripped(drawn).replacingOccurrences(of: "\u{00A0}", with: " "), command)
    }

    // MARK: - The panel

    /// The sentences TcrBar writes with a command in them are drawn as command prose: the
    /// Codex line, the strict-exit statement and the Codex sign-in sheet. The test target
    /// links TcrBarCore alone, so the views are read as text. The view copies the commands
    /// as written, and its drawn text, which carries the joiners, is never selectable.
    func testTheSentencesThatNameACommandAreDrawnAsCommandProse() throws {
        XCTAssertTrue(
            try slice(
                try appSource("FleetView.swift"),
                from: "private func codexBanner(_ fleet: Fleet) -> some View {",
                to: "private var hasFleet: Bool {"
            ).contains("CommandProseText(text: banner.text"),
            "the Codex line no longer draws its commands as command prose")
        XCTAssertTrue(
            try slice(
                try appSource("PanelV4/AccountExitRow.swift"), from: "struct AccountExitStatement",
                to: "struct MustSwitch"
            ).contains("CommandProseText(text: text"),
            "the strict-exit statement no longer draws its command as command prose")
        let sheet = try appSource("LoginSheet.swift")
        XCTAssertTrue(
            try slice(sheet, from: "if let detail {", to: "if case .choosing = phase {")
                .contains("CommandProseText(text: detail"),
            "the Codex sheet's detail no longer draws its commands as command prose")
        XCTAssertTrue(
            try slice(sheet, from: "ForEach(Array(facts.notices.enumerated())", to: "buttons")
                .contains("CommandProseText(text: notice"),
            "the Codex sheet's notices no longer draw their commands as command prose")

        let view = try slice(
            try appSource("PanelV4/V4Text.swift"), from: "struct CommandProseText", to: nil)
        XCTAssertTrue(view.contains("CommandProse.commands(in: text)"), "the menu copies nothing")
        XCTAssertTrue(
            view.contains("CommandProse.drawnRuns(text)"), "a command or a path may break again")
        XCTAssertTrue(
            view.contains("Text(verbatim: CommandProse.keepingPathsWhole(text))"),
            "a sentence with no command, such as tcr's not-pooled reason, may break inside a path")
        XCTAssertFalse(
            view.contains(".textSelection("),
            "the drawn text carries word joiners, which a selection would copy")
    }

    // MARK: - Helpers

    /// The text as written: without the word joiners and zero-width spaces drawing adds.
    private func stripped(_ drawn: String) -> String {
        drawn.replacingOccurrences(of: "\u{2060}", with: "").replacingOccurrences(of: "\u{200B}", with: "")
    }

    /// `sentence` as the panel draws it (``CommandProse/drawnRuns(_:)``), prose in the system
    /// face and commands in the mono face, both with the panel's monospaced digits, and the
    /// range of every word no line may end inside: each word of a command, and each word of
    /// the prose with a slash in it. A zero-width space before a word is the break before it,
    /// so it is not part of the word.
    private func laidOut(_ sentence: String, size: Double) -> (NSAttributedString, [NSRange]) {
        let proseFont = NSFont.monospacedDigitSystemFont(ofSize: size, weight: .regular)
        let commandFont = NSFont.monospacedSystemFont(ofSize: size, weight: .regular)
        let drawn = NSMutableAttributedString()
        var whole: [NSRange] = []
        for run in CommandProse.drawnRuns(sentence) {
            let text: String
            let font: NSFont
            let everyWord: Bool
            switch run {
            case .prose(let prose): (text, font, everyWord) = (prose, proseFont, false)
            case .command(let command): (text, font, everyWord) = (command, commandFont, true)
            }
            var location = drawn.length
            for word in text.components(separatedBy: " ") {
                let length = (word as NSString).length
                let lead = word.hasPrefix("\u{200B}") ? 1 : 0
                if everyWord || word.contains("/"), length > lead {
                    whole.append(NSRange(location: location + lead, length: length - lead))
                }
                location += length + 1
            }
            drawn.append(NSAttributedString(string: text, attributes: [.font: font]))
        }
        return (drawn, whole)
    }

    /// The width CoreText sets `text` in on one line.
    private func width(of text: NSAttributedString) -> Double {
        Double(CTLineGetTypographicBounds(CTLineCreateWithAttributedString(text), nil, nil, nil))
    }

    /// Where each line but the last ends when CoreText sets `text` in lines `width` wide, as
    /// offsets into it.
    private func lineEnds(_ text: NSAttributedString, width: Double) -> [Int] {
        let typesetter = CTTypesetterCreateWithAttributedString(text)
        var ends: [Int] = []
        var start = 0
        while start < text.length {
            let count = CTTypesetterSuggestLineBreak(typesetter, start, width)
            guard count > 0 else { break }
            start += count
            if start < text.length { ends.append(start) }
        }
        return ends
    }

    private func appSource(_ relative: String) throws -> String {
        let file = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // TcrBarTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // apps/macos
            .appendingPathComponent("Sources/TcrBar/\(relative)")
        return try String(contentsOf: file, encoding: .utf8)
    }

    /// The text from `start` to the first `end` after it, or to the end of the file.
    private func slice(_ source: String, from start: String, to end: String?) throws -> String {
        let from = try XCTUnwrap(source.range(of: start), "no \(start) — update this test")
        guard let end else { return String(source[from.lowerBound...]) }
        let to = try XCTUnwrap(
            source.range(of: end, range: from.upperBound..<source.endIndex),
            "no \(end) after \(start) — update this test")
        return String(source[from.lowerBound..<to.lowerBound])
    }
}
