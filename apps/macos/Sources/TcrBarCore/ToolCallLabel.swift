/// What the Tools tab says about one call's duration — in print and out loud.
///
/// Here rather than in `FleetView` because the test target links `TcrBarCore`
/// only (`Package.swift:39-43`): a string a view composes privately is a string
/// no assertion can read, and the whole point of this type is that the two
/// halves of it — what a sighted reader sees and what VoiceOver speaks — are
/// pinned to the same facts by a test.
///
/// The defect it exists for (review finding 4): a call KILLED at the Bash tool's 600 s timeout and one that
/// merely ran long drew the identical string in the identical pill, red against
/// grey the only difference. Measured on `17-tools-tab-dark.png`: `10M 0S` in
/// `#ea5a56`, `9M 43S` in `#bfbeb9`, same size, same shape. State carried by
/// colour alone, and the state in question is the most actionable fact on the
/// tab.
public enum ToolCallLabel {
    /// `"45s"`, `"4m 12s"`, `"3h 0m"`.
    ///
    /// The hour tier exists because the premise this function shipped with was
    /// wrong. It used to read "no day tier: the longest call this tab shows is
    /// the Bash tool's own timeout, six orders of magnitude under a day", and
    /// that is only true of a CAPPED call. RUNNING NOW also lists uncapped
    /// ones — an `Agent`, a `TaskOutput` — which have no deadline at all and
    /// routinely run for hours. Rendered by the old two-tier form, a three
    /// hour subagent printed `180m 3s`, which a reader has to divide to
    /// understand, and on the Sessions tab's narrower "oldest" column it did
    /// not even fit: it truncated to `180m…`, losing the unit.
    ///
    /// Nothing under an hour changes, which is every capped call this tab can
    /// show. Above it the seconds are dropped rather than carried, the same
    /// choice ``HeldWindow/duration(minutes:)`` already makes at its own scale
    /// (`4d 12h`, never `4d 12h 30m`): at three hours a second is not a fact
    /// anyone reads, and the column is 96pt.
    ///
    /// Still no DAY tier, and now for a reason that survives: a call running
    /// past 24 hours is older than ``SESSION_TTL_MS`` and older than the
    /// six-hour lost-result backstop, so the wire has dropped it long before
    /// it could reach this formatter.
    public static func duration(_ seconds: Double) -> String {
        let total = Int(seconds.rounded())
        if total >= 3600 {
            return "\(total / 3600)h \((total % 3600) / 60)m"
        }
        let minutes = total / 60
        let rest = total % 60
        return minutes > 0 ? "\(minutes)m \(rest)s" : "\(rest)s"
    }

    /// Was this call killed at the timeout, rather than merely slow? Never,
    /// for a call with no timeout (`nil`), however long it ran.
    ///
    /// `>=`, not `==`: the wire reports the call's own elapsed seconds and a
    /// kill lands a hair past the deadline it was measured against.
    public static func timedOut(seconds: Double, timeout: Double?) -> Bool {
        guard let timeout else { return false }
        return seconds >= timeout
    }

    /// The timeout SLOWEST TODAY judges a finished call against, or `nil` for a
    /// call that had none this build knows of, which no duration makes "timed out".
    ///
    /// Claude keeps the Bash timeout this list has always used. A completed Codex
    /// row does not carry its timeout on the wire, so no duration proves it timed out.
    public static func judgedTimeout(for entry: SessionToolEntry, bashTimeout: Double) -> Double? {
        switch entry.provider {
        case .anthropic: return bashTimeout
        case .codex: return nil
        }
    }

    /// The pill's printed text.
    ///
    /// A killed call spends the pill on the WORD rather than the number, and
    /// that is a deliberate trade of one fact for the other: the number is
    /// already known for this case (it is the timeout, which the summary line
    /// above states in full — "31 hit the 600s timeout"), while "this one was
    /// killed" is said nowhere else on the row. The exact elapsed time is not
    /// lost — ``spoken(seconds:timeout:)`` carries it to VoiceOver and to the
    /// row's tooltip.
    ///
    /// Measured before choosing it: the pill's glyph run is about 6.9 pt a
    /// character at this size, so `10m 0s · timed out` needs ~138 pt against a
    /// 60 pt column, and widening the column that far takes 78 pt from the
    /// command the row is about. `timed out` alone needs ~76 pt, which fits
    /// `V4.trailingColumnWidth` (96 pt), the width this tab already declares.
    public static func pill(seconds: Double, timeout: Double?) -> String {
        timedOut(seconds: seconds, timeout: timeout) ? "timed out" : duration(seconds)
    }

    /// The pill's accessibility value, and the same sentence as its tooltip —
    /// `nil` for a call that simply ran, whose pill already speaks its duration.
    public static func spoken(seconds: Double, timeout: Double?) -> String? {
        guard let timeout, timedOut(seconds: seconds, timeout: timeout) else { return nil }
        return "ran \(duration(seconds)), killed at the \(Int(timeout)) second timeout"
    }

    /// What a RUNNING NOW row prints beside its ring: `"4m 12s"`, or
    /// `"9m 40s · 20s left"` once the call is inside the warning band.
    ///
    /// `remaining` is `nil` for a call with no known cap — an Agent, a Read —
    /// and such a call can never print a "left" clause, because there is
    /// nothing this build knows it is running out of. That is the same refusal
    /// the ring makes by not being drawn at all for those tools; stating it
    /// once, here, is what stops the two from disagreeing (they did: the first
    /// render of this layout printed "9m 43s · 17s left" beside an Agent call
    /// with no ring).
    ///
    /// The clause says "left" rather than "to timeout" because only a row with
    /// a ring prints it, and the ring already names what it fills toward, on
    /// its help and in its spoken value: the Bash tool's 600s, or the timeout a
    /// codex call named. The section head carries a count alone. The shorter
    /// word also fits beside the elapsed time in the trailing column.
    public static func running(elapsed: Double?, remaining: Double?, warnWithin: Double) -> String {
        let age = elapsed.map(duration) ?? ""
        guard let remaining, remaining <= warnWithin else { return age }
        return "\(age) · \(Int(max(0, remaining.rounded())))s left"
    }

    /// The word a row names its call's tool by, after the session's name: a Claude call's
    /// own tool name, and a codex call's kind, `shell` or `edit`, the word its BY TOOL
    /// bucket (``category(forTool:provider:)``) names it by.
    ///
    /// codex's own names are long: beside a running call's ring, its time and `no Kill`,
    /// `exec_command` was cut to `exec_c…`, which names nothing. Any other codex tool keeps
    /// its own name, which is the only word there is for it.
    public static func rowWord(forTool tool: String, provider: Provider) -> String {
        guard provider == .codex else { return tool }
        switch tool {
        case "shell", "exec_command", "local_shell_call": return "shell"
        case "apply_patch": return "edit"
        default: return tool
        }
    }

    /// The BY TOOL buckets, in the order the line lists them: Claude's three, then codex's.
    public static let categoryOrder = [
        "Bash", "Agent", "Read/Grep/Edit", "Codex shell", "Codex edit", "Codex other",
    ]

    /// The BY TOOL bucket a call files under, by the provider of the session that made it.
    ///
    /// A Claude call keeps the buckets it always had: `Bash`, `Agent` for `Agent` and `Task`,
    /// and `Read/Grep/Edit` for everything else. A Codex call never shares one of them, even
    /// when its name matches, because a codex call is not a Claude Code tool.
    ///
    /// The Codex names are codex's own tool names, as tcr reads them off the response stream:
    /// `exec_command` is codex's shell tool, the one it offers by default; `shell` and a
    /// `local_shell_call` item are the older shell forms, kept so a call that arrives under
    /// one of them still reads as a shell call; `apply_patch` is codex's file edit. Anything
    /// else codex calls, `update_plan` for one, is `Codex other`.
    public static func category(forTool tool: String, provider: Provider) -> String {
        switch provider {
        case .anthropic:
            switch tool {
            case "Bash": return "Bash"
            case "Agent", "Task": return "Agent"
            default: return "Read/Grep/Edit"
            }
        case .codex:
            switch tool {
            case "shell", "exec_command", "local_shell_call": return "Codex shell"
            case "apply_patch": return "Codex edit"
            default: return "Codex other"
            }
        }
    }
}
