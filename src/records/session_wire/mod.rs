//! F1: session + tool-call tracking for `tcr status --json`'s `sessions` array.
//!
//! Design and measurements: `docs/design/panel-tabs.md`.
//!
//! Two halves, deliberately separate:
//!
//! - Pure parsing (this module): given a request body already read into memory, pull out the
//!   `metadata.user_id`-embedded `session_id` and, from the TAIL of `messages`, any
//!   `tool_use` (assistant) and `tool_result` (user) blocks. The tail is not a fixed "last
//!   two" — Claude Code does not always end a request's history with exactly
//!   `[assistant(tool_use), user(tool_result)]`. Measured against this machine's own
//!   transcripts (`~/.claude/projects/*.jsonl`, walked via each entry's real `parentUuid`
//!   chain, so cross-thread/subagent interleaving in the same file cannot be mistaken for
//!   one thread's own history): a `tool_use` and its `tool_result` are always adjacent
//!   logical messages, but Claude Code can append further trailing `user` messages after
//!   the `tool_result` — a system-reminder is sent as its OWN message, not folded into the
//!   `tool_result`'s content array — before the next request actually goes out. So the tail
//!   walks backward from the newest message, gathering it into the scan window, until it
//!   reaches an `assistant` message (which is where the newest `tool_use` block, if any,
//!   lives) or [`TAIL_SCAN_CAP`] is hit. Never deserializes the whole conversation history
//!   into owned [`serde_json::Value`]s — `messages` is read borrowed as [`RawValue`] and
//!   only this bounded tail window is turned into owned values.
//! - [`WireSessionTracker`]: the bounded, in-memory table one request's parsed events get
//!   folded into. No I/O, no locking — [`crate::manager::Manager`] wraps one in a `Mutex`.
//!
//! A `tool_use` block reaches this table from two directions — a request's assistant message
//! ([`extract_tool_events`]) and the response the proxy just streamed (`proxy.rs`, which
//! builds its events through the shared [`tool_use_event_from_block`]) — because a Claude Code
//! request carries a `tool_use` and its `tool_result` together, so the request side alone can
//! never show a tool still running.
//!
//! A Codex session is keyed on codex's `session-id` header, not on `metadata.user_id`. Its
//! calls come from the response stream's finished output items
//! ([`codex_tool_call_from_item`]), run from the stream's end, and close when the session's
//! next Responses turn arrives ([`WireSessionTracker::record_codex_request`]). Auxiliary image,
//! memory and search calls count on the session but do not close its tools. None of its request
//! bodies is read.
//!
//! `command_head` (capped to 120 chars; the per-tool shape is on
//! [`ToolUseEvent::command_head`]) is held in this in-memory table and is never written to
//! `~/.cache/tcr/logs` or any other log file — the same
//! body-content-never-hits-disk rule `src/proxy/mod.rs` states for the request log.
//!
//! It DOES reach one file: the affinity-style snapshot in
//! [`crate::stores::session_wire_persist`], which round-trips [`WireSession`] to
//! `~/.cache/tcr/session-wire.json`. Between #299 and 2026-09-13 both
//! [`RunningTool::command_head`] and [`SlowTool::command_head`] carried
//! `#[serde(skip_serializing)]` and that file held no command at all. Gil reversed it the
//! same day, on a measurement: after a restart, 72 of 95 restored SLOWEST TODAY rows read
//! as a bare `Bash`, because the class survived and the command did not — a slowest-today
//! list with no commands in it is junk, not a reduced-detail version of the real thing. So
//! the head persists, bounded by what it already was: 120 characters of a NORMALIZED
//! command (env prefixes, wrappers and the leading `cd` clause are stripped before it is
//! stored), a file path, or a grep pattern — never a request or response body, which this
//! table has never held. The file is written `0600` by [`crate::base::config::write_atomic`],
//! which both creates its temp file with that mode and normalises the destination after the
//! rename, so an operator's umask cannot loosen it.

use serde_json::Value;
use serde_json::value::RawValue;

use crate::records::usage::UsageRecord;

/// A `tool_use` block found in an assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseEvent {
    pub id: String,
    pub name: Option<String>,
    /// What this call is DOING, for the panel row that would otherwise print the bare tool
    /// name. Built by [`tool_use_event_from_block`] for a Claude call and by
    /// [`codex_tool_call_from_item`] for a Codex one, always capped at [`COMMAND_HEAD_MAX`]
    /// characters:
    ///
    /// - `Bash` — the NORMALIZED first 120 characters of `input.command`, see
    ///   [`normalize_bash_command`].
    /// - `Agent` / `Task` (a running subagent) — `input.description` (Claude Code's 3-5 word
    ///   summary), prefixed with `input.subagent_type` when present
    ///   (`"reviewer: check the wire fixtures"`).
    /// - `TaskOutput` — `TaskOutput · waiting on task <input.task_id>`.
    /// - `Read` / `Edit` / `Write` / `NotebookEdit` — the tool name and `input.file_path`
    ///   (`input.notebook_path` for a notebook).
    /// - `Grep` / `Glob` — the tool name and `input.pattern`.
    /// - `WebFetch` — the HOST of `input.url`, never its path or query.
    /// - `WebSearch` — `input.query`.
    /// - `None` for any other tool, and for any of the above whose field is absent; the panel
    ///   falls back to the tool name.
    ///
    /// Never written to a log; it does persist in the session-wire snapshot — see the module
    /// doc's no-body-content-on-disk rule.
    pub command_head: Option<String>,
    /// A `Bash` tool call's coarse category — see [`CommandClass`]. `None` for any other
    /// tool, or when `command_head` is `None`.
    pub command_class: Option<CommandClass>,
}

/// A Bash command's coarse category, for the Tools tab's per-class rollup — mirrors
/// the operator harness's own slow-command classifier, so the wire and that harness
/// hook agree on both the rules and the names. Classified on the REDUCED command (after
/// env/wrapper/`cd`-clause stripping — see [`normalize_bash_command`]), never the raw one:
/// the hook's own measurement is that classifying a compound command by its first clause
/// hid a real defect (a slow `grep`) behind a `git` bucket for four days.
// `Serialize`/`Deserialize` because a classified command rides the session-wire
// snapshot (`src/stores/session_wire_persist.rs`, #289): a row restored at boot has to
// carry the same class it carried while live, or the Tools tab would reclassify
// half its rows as `other` after every restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CommandClass {
    /// `wait-for-line`, `merge-when-green`, or a command starting with `until`/`sleep` —
    /// never a defect, since taking long is the job.
    Wait,
    /// Any `;`, `&` or `|` left in the reduced command — a real compound, since a
    /// `cd <dir> && <clause>` already had its `cd` prefix reduced away before this check runs.
    Compound,
    /// `find`, `grep`, `fd`, `rg`, `bfs` or `du`.
    Search,
    /// `git fetch`/`push`/`pull`/`clone`/`ls-remote` — a network round trip.
    GitNet,
    /// Any other `git` subcommand.
    GitLocal,
    /// `cargo`, `pnpm`, `npm`, `yarn`, `uv`, `uvx`, `pytest`, `vitest`, `make`, `tsc`, `bun`
    /// or `cargo-q.sh`.
    Build,
    /// Everything else.
    Other,
}

impl CommandClass {
    /// The wire/panel name — kebab-case, matching `hooks/log-slow-bash.sh`'s own
    /// `class=` values verbatim, so a person reading both sees the same word.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Compound => "compound",
            Self::Search => "search",
            Self::GitNet => "git-net",
            Self::GitLocal => "git-local",
            Self::Build => "build",
            Self::Other => "other",
        }
    }
}

/// Take the first whitespace-delimited token of `s`, requiring at least one whitespace
/// character AFTER it (a bare trailing token with nothing following is never "wrapped" —
/// same as the hook's `\s+` after each stripped prefix). Returns `(token, rest trimmed of
/// its leading whitespace)`.
fn take_token(s: &str) -> Option<(&str, &str)> {
    let ws = s.find(char::is_whitespace)?;
    Some((&s[..ws], s[ws..].trim_start()))
}

/// Is `token` an `IDENT=value` environment assignment? Generic — any valid shell identifier,
/// not restricted to one project's own prefix — mirroring the hook's
/// `^(FOO=bar\s+)+`.
fn is_env_assignment(token: &str) -> bool {
    let Some((ident, value)) = token.split_once('=') else {
        return false;
    };
    !ident.is_empty()
        && !value.is_empty()
        && ident
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Strip every leading `IDENT=value` token, in order — the hook's `^(FOO=bar\s+)+`.
fn strip_env_prefix(mut s: &str) -> &str {
    while let Some((token, rest)) = take_token(s) {
        if !is_env_assignment(token) {
            break;
        }
        s = rest;
    }
    s
}

/// Strip one leading wrapper (`timeout <n>`, `nice -n<n>` or `time`), or `None` if `s` does
/// not start with one. Mirrors the hook's `^(timeout\s+\d+\s+|nice\s+-n\d+\s+|time\s+)`.
fn strip_one_wrapper(s: &str) -> Option<&str> {
    let (first, rest) = take_token(s)?;
    match first {
        "timeout" => {
            let (arg, rest2) = take_token(rest)?;
            (!arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit())).then_some(rest2)
        }
        "time" => Some(rest),
        "nice" => {
            let (flag, rest2) = take_token(rest)?;
            let digits = flag.strip_prefix("-n")?;
            (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then_some(rest2)
        }
        _ => None,
    }
}

/// Strip every leading wrapper, in order — the hook's `(...)+` repetition.
fn strip_wrappers(mut s: &str) -> &str {
    while let Some(rest) = strip_one_wrapper(s) {
        s = rest;
    }
    s
}

/// Reduce `cd <dir> && <clause>` (or `cd <dir>; <clause>`) to just `<clause>` — the hook's
/// own measurement: 18% of its "compound" rows were exactly this shape, a free `cd` in front
/// of the real work.
///
/// The reduction runs whatever `<clause>` turns out to be, compound or not. It used to bail
/// when the remainder still held a `;`, `&` or `|`, and that bail is what put a path where a
/// verb belongs on the live panel (2026-09-13: three RUNNING NOW rows reading
/// `cd ~/src/example/st…`, every one of them a pipeline whose `cd` therefore survived). A `cd` prefix is never the interesting half of a row; [`classify_command`]
/// still reads `Compound` off the remainder, so the class is unchanged by dropping it.
///
/// `None` when `s` does not start with a standalone `cd`, or has no such separator.
fn reduce_cd_prefix(s: &str) -> Option<String> {
    let after_cd = s.strip_prefix("cd")?;
    if !after_cd.starts_with(char::is_whitespace) {
        return None;
    }
    let after_cd = after_cd.trim_start();
    let dir_end = after_cd.find(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|'))?;
    if dir_end == 0 {
        return None;
    }
    let after_dir = after_cd[dir_end..].trim_start();
    let after_sep = after_dir
        .strip_prefix("&&")
        .or_else(|| after_dir.strip_prefix(';'))?;
    let clause = after_sep.trim();
    if clause.is_empty() {
        return None;
    }
    Some(clause.to_string())
}

/// Does `s` start with `word` as a whole word (end of string, or followed by whitespace)?
fn starts_with_word(s: &str, word: &str) -> bool {
    s.strip_prefix(word)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Classify an already-REDUCED command (see [`normalize_bash_command`]) — the hook's own
/// `elif` chain, same order: `wait` first (never a defect), then `compound` (a genuine one,
/// since `cd` was already stripped), then by the first word's basename.
fn classify_command(reduced: &str) -> CommandClass {
    if reduced.contains("wait-for-line")
        || reduced.contains("merge-when-green")
        || starts_with_word(reduced, "until")
        || starts_with_word(reduced, "sleep")
    {
        return CommandClass::Wait;
    }
    if reduced.contains([';', '&', '|']) {
        return CommandClass::Compound;
    }
    let mut words = reduced.split_whitespace();
    let w0 = words.next().unwrap_or("");
    let w0_base = w0.rsplit('/').next().unwrap_or(w0);
    let w1 = words.next().unwrap_or("");
    if matches!(w0_base, "find" | "grep" | "fd" | "rg" | "bfs" | "du") {
        return CommandClass::Search;
    }
    if w0_base == "git" {
        return if matches!(w1, "fetch" | "push" | "pull" | "clone" | "ls-remote") {
            CommandClass::GitNet
        } else {
            CommandClass::GitLocal
        };
    }
    if matches!(
        w0_base,
        "cargo"
            | "pnpm"
            | "npm"
            | "yarn"
            | "uv"
            | "uvx"
            | "pytest"
            | "vitest"
            | "make"
            | "tsc"
            | "bun"
            | "cargo-q.sh"
    ) {
        return CommandClass::Build;
    }
    CommandClass::Other
}

/// Reduce a Bash command to its IDENTITY — the one clause that says what it is — so the
/// 120-char cap in [`normalize_bash_command`] truncates a short, meaningful string instead of
/// cutting the raw command mid-token. Ports the identity-reduction spec worked out and
/// validated against 40,589 real commands harvested from `~/.claude/projects/` (see
/// `docs/design/tools-tab.md`); the Python prototype's `identity()` is the line-by-line source
/// of truth this module mirrors.
///
/// In order: split on `;`, `&&`, `||`, `|` and newline, but ONLY where unquoted and at bracket
/// depth zero ([`shell_scan`] walks the string once, tracking quote/depth state, because a
/// regex cannot do this — `sed -i 's|a|b|'` carries a `|` inside its own quotes); drop a
/// segment that is only an env assignment and unwrap `T=$(...)`; drop `|| exit` / `|| true`
/// error handlers; strip env prefixes, wrappers and redirections per segment; skip trivial
/// verbs unless every segment is trivial; give an interpreter its script's basename; shorten
/// long absolute paths; and cut at a quote/bracket-aware boundary that is repaired FORWARD
/// (closing the quote, closing brackets, marking with `…`) rather than trimmed backward — a
/// backward trim is what produced a bare `rg -n` for 532 commands in the prototype's own
/// history, so [`balance_identity`] never does that.
mod identity;

/// Normalize a raw Bash `input.command` into a `(head, class)` pair for the Tools tab —
/// ports `hooks/log-slow-bash.sh`'s `jq` normalization verbatim, in order: strip leading
/// whitespace, strip leading env assignments, strip wrappers, reduce a
/// `cd <dir> && <clause>` prefix (only when `<clause>` is itself simple), strip wrappers
/// again (`cd <dir> && timeout 15 rg ...` is the shape `prefer-fd-rg` itself emits) — THEN reduce the
/// result to its [`identity::identity`] (the one clause that says what the command is) before
/// truncating to [`COMMAND_HEAD_MAX`], so the cap trims a short meaningful string instead of a
/// long one mid-token. The class is read off the REDUCED command, before either the identity
/// step or the truncation — display formatting must never change what a command classifies as
/// (#289's own rule, unchanged by #336's identity reduction).
pub fn normalize_bash_command(raw: &str) -> (String, CommandClass) {
    let s = strip_wrappers(strip_env_prefix(raw.trim_start()));
    let reduced = reduce_cd_prefix(s).unwrap_or_else(|| s.to_string());
    let reduced = strip_wrappers(&reduced).to_string();
    let class = classify_command(&reduced);
    let head = identity::identity(&reduced)
        .replace('\n', "⏎")
        .chars()
        .take(COMMAND_HEAD_MAX)
        .collect();
    (head, class)
}

/// A `tool_result` block found in a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultEvent {
    pub id: String,
    pub is_error: bool,
    /// `is_error` and the result's own content text contains "timed out" — the same string a
    /// Bash-tool timeout error carries.
    pub timed_out: bool,
}

/// Max characters kept from a Bash tool's `input.command` — see [`ToolUseEvent::command_head`].
const COMMAND_HEAD_MAX: usize = 120;

/// Upper bound on how many of `messages`' trailing entries [`extract_tool_events`] will ever
/// parse while walking backward for the assistant turn that owns the newest `tool_use` — see
/// the module doc. Comfortably covers the observed shape (one or two trailing non-assistant
/// messages, e.g a system-reminder sent as its own message) while keeping the parse cost
/// small and fixed even for a turn with no `tool_use` at all, where the walk never finds an
/// `assistant` message and runs all the way to the cap.
pub(crate) const TAIL_SCAN_CAP: usize = 20;

/// Parse `metadata.user_id`'s stringified JSON blob for its `session_id` — see
/// `src/proxy/mod.rs`'s `stable_session_key` doc-comment for the blob's shape and lineage
/// guarantees. `None` on absence or a parse failure; never a panic on a hand-shaped or
/// truncated blob.
pub fn extract_session_id(user_id_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct SessionIdPeek {
        #[serde(default)]
        session_id: Option<String>,
    }
    serde_json::from_str::<SessionIdPeek>(user_id_json)
        .ok()
        .and_then(|p| p.session_id)
}

/// The minimal shape read from `messages`: only `role` and `content`, and only for the
/// bounded tail window [`extract_tool_events`] walks — see the module doc for why the rest
/// of the history is never materialized.
#[derive(serde::Deserialize)]
struct MessagePeek {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: Option<Value>,
}

/// The minimal shape read from one content block.
#[derive(serde::Deserialize)]
struct BlockPeek {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    content: Option<Value>,
}

pub(crate) fn content_mentions_timeout(content: &Option<Value>) -> bool {
    match content {
        Some(Value::String(s)) => s.to_lowercase().contains("timed out"),
        Some(Value::Array(items)) => items.iter().any(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .is_some_and(|s| s.to_lowercase().contains("timed out"))
        }),
        _ => false,
    }
}

/// Extract `tool_use` / `tool_result` blocks from the LAST TWO elements of a request's
/// `messages` array, given as a borrowed [`RawValue`] (see `account_uuid.rs`'s `BodyPeek`
/// for the same borrowing technique). A body with no `messages`, a non-array
/// `messages`, or malformed elements yields two empty vectors rather than an error — this is
/// a best-effort observability peek, never a request-rejection path.
/// The request path reads this answer through [`crate::records::request_body::RequestBody`], which
/// finds it in its one walk; this is the definition that walk is tested against.
pub fn extract_tool_events(messages: &RawValue) -> (Vec<ToolUseEvent>, Vec<ToolResultEvent>) {
    let mut tool_uses = Vec::new();
    let mut tool_results = Vec::new();

    let Ok(all) = serde_json::from_str::<Vec<&RawValue>>(messages.get()) else {
        return (tool_uses, tool_results);
    };

    // Walk backward from the newest message, parsing each into the tail window, until an
    // `assistant` message is reached (where the newest `tool_use` block, if any, lives) or
    // `TAIL_SCAN_CAP` is hit — see the module doc for why a fixed "last two" undercounts.
    // `tail` ends up oldest-first, matching `all`'s own order, once reversed below.
    let mut tail: Vec<MessagePeek> = Vec::new();
    for raw in all.iter().rev().take(TAIL_SCAN_CAP) {
        let Ok(msg) = serde_json::from_str::<MessagePeek>(raw.get()) else {
            continue;
        };
        let is_assistant = msg.role == "assistant";
        tail.push(msg);
        if is_assistant {
            break;
        }
    }
    tail.reverse();

    for msg in tail {
        let Some(Value::Array(blocks)) = msg.content else {
            continue;
        };
        for block in blocks {
            let Ok(b) = serde_json::from_value::<BlockPeek>(block) else {
                continue;
            };
            match (msg.role.as_str(), b.kind.as_str()) {
                ("assistant", "tool_use") if let Some(id) = b.id => {
                    tool_uses.push(tool_use_event_from_block(id, b.name, b.input.as_ref()));
                }
                ("user", "tool_result") if let Some(id) = b.tool_use_id => {
                    let is_error = b.is_error.unwrap_or(false);
                    let timed_out = is_error && content_mentions_timeout(&b.content);
                    tool_results.push(ToolResultEvent {
                        id,
                        is_error,
                        timed_out,
                    });
                }
                _ => {}
            }
        }
    }
    (tool_uses, tool_results)
}

/// Most account-bound tokens one request's history is scanned for. Every token in a
/// conversation names the same account, so the first handful already resolves it — the cap
/// makes a long conversation cost a fixed amount here rather than a growing one.
pub const BOUND_TOKEN_SCAN_CAP: usize = 16;

/// What a request's history says about which account may serve it — see
/// [`extract_bound_tokens`].
///
/// A struct, not a tuple: the two values answer different questions and the second one drives a
/// bench all by itself, so a caller must not be able to bind them the wrong way round.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BoundHistory {
    /// The tokens themselves, deduplicated, at most [`BOUND_TOKEN_SCAN_CAP`] of them.
    pub tokens: Vec<String>,
    /// Whether the history carries ANY account-bound state at all — true even when the tokens
    /// were capped away, and true for a bound block whose token field is missing. This is what
    /// holds a conversation to its pin when no token happens to be known to this process.
    pub carries_bound_history: bool,
    /// Whether the history carries state another account cannot read at all: an advisor
    /// result (`server_tool_use`) or server-side thread state (`previous_message_id`, folded in
    /// by `src/proxy/mod.rs`). A history bound only by `thinking` and `redacted_thinking` blocks
    /// leaves this `false`, and for a model whose thinking survives a move
    /// (`crate::base::model::thinking_is_org_bound` false) its hold buys nothing but a warm cache.
    pub carries_account_only_state: bool,
}

/// Every account-bound token carried anywhere in a request's `messages`.
///
/// Three block shapes bind a conversation to the account that produced them, and all three ride
/// in the history for its whole life:
///
/// - `server_tool_use.id` — the advisor call, whose encrypted result decrypts on one org only;
/// - `thinking.signature` — Anthropic's attestation over a reasoning block;
/// - `redacted_thinking.data` — the same thing, already encrypted.
///
/// (`previous_message_id` binds a conversation too, but it is a top-level body field rather
/// than a content block, so `src/proxy/mod.rs` reads it in the same peek and folds it in here.)
///
/// Deliberately NOT the bounded tail [`extract_tool_events`] walks: it is the OLD blocks that
/// still bind the request, so a tail scan would find them on turn two and miss them forever
/// after — the same as not having this at all.
///
/// Cheap by construction: each block is read as `type` plus one string field, so the
/// megabyte-scale `encrypted_content`, `thinking` text and tool inputs beside them are skipped
/// rather than materialized.
///
/// Role is not checked. These blocks are only ever emitted by the model, so they only appear in
/// an `assistant` message; requiring the role would add a way for a body shaped slightly
/// differently to lose its pin, and a client gains nothing by claiming one — the worst a forged
/// token does is bench the fleet down to the account that minted it.
/// The request path reads this answer through [`crate::records::request_body::RequestBody`], which
/// finds it in its one walk; this is the definition that walk is tested against.
pub fn extract_bound_tokens(messages: &RawValue) -> BoundHistory {
    /// One content block, read for the type and the four fields any bound shape uses.
    #[derive(serde::Deserialize)]
    struct BoundBlockPeek {
        #[serde(rename = "type", default)]
        kind: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        signature: Option<String>,
        #[serde(default)]
        data: Option<String>,
    }
    /// One message, whose `content` is kept borrowed so a message with no blocks at all costs
    /// nothing beyond finding its end.
    #[derive(serde::Deserialize)]
    struct ContentPeek<'a> {
        #[serde(borrow, default)]
        content: Option<&'a RawValue>,
    }

    let mut found = BoundHistory::default();
    let Ok(all) = serde_json::from_str::<Vec<&RawValue>>(messages.get()) else {
        return found;
    };
    for raw in all {
        let Ok(msg) = serde_json::from_str::<ContentPeek>(raw.get()) else {
            continue;
        };
        // A string `content` (the common plain-text message) is not an array of blocks and
        // cannot carry one of these — parsing it as blocks fails, which is the skip.
        let Some(Ok(blocks)) = msg
            .content
            .map(|c| serde_json::from_str::<Vec<BoundBlockPeek>>(c.get()))
        else {
            continue;
        };
        for block in blocks {
            // The token field differs per shape; an ordinary `tool_use` (client-side, `toolu_…`)
            // is not bound state at all and must not appear here — its result travels in the
            // request body in plain text and any account can read it.
            let token = match block.kind.as_str() {
                "server_tool_use" => block.id,
                "thinking" => block.signature,
                "redacted_thinking" => block.data,
                _ => continue,
            };
            // Reached only for a bound shape, so the history is bound even when the token field
            // itself is absent and even once the cap has been hit.
            found.carries_bound_history = true;
            if block.kind == "server_tool_use" {
                found.carries_account_only_state = true;
            }
            let Some(token) = token else {
                continue;
            };
            if found.tokens.len() >= BOUND_TOKEN_SCAN_CAP {
                continue;
            }
            if !found.tokens.contains(&token) {
                found.tokens.push(token);
            }
        }
    }
    found
}

/// Build one [`ToolUseEvent`] from a `tool_use` block's `id`, `name` and `input`.
///
/// Factored out of [`extract_tool_events`] because a `tool_use` block now reaches this table
/// from TWO directions and both must produce the identical event: a REQUEST body's assistant
/// message (here) and the RESPONSE the proxy has just finished streaming (`proxy.rs`'s SSE and
/// non-streamed parse paths). The response side is what makes `running` non-empty at all — a
/// Claude Code request carries a `tool_use` and its matching `tool_result` in the SAME body, so
/// a request-only table opens and closes every call within one call and shows nothing in
/// flight (measured 2026-09-13 against the live proxy: 11 sessions, `running: 0` in every one,
/// while at least three were mid-tool).
///
/// Every head produced here is display context for a panel row, capped at
/// [`COMMAND_HEAD_MAX`] characters — see [`ToolUseEvent::command_head`] for the per-tool shape
/// and the module doc for where it may and may not be held.
pub fn tool_use_event_from_block(
    id: String,
    name: Option<String>,
    input: Option<&Value>,
) -> ToolUseEvent {
    let mut command_class = None;
    let field = |key: &str| input.and_then(|v| v.get(key)).and_then(Value::as_str);
    let capped = |s: String| -> String { s.chars().take(COMMAND_HEAD_MAX).collect() };
    let command_head = match name.as_deref() {
        Some("Bash") => field("command").map(|s| {
            let (head, class) = normalize_bash_command(s);
            command_class = Some(class);
            head
        }),
        Some("Agent" | "Task") => field("description").map(|description| {
            let head = match field("subagent_type") {
                Some(subagent_type) => format!("{subagent_type}: {description}"),
                None => description.to_string(),
            };
            capped(head)
        }),
        // A running `TaskOutput` is a session WAITING on a subagent, not working — without
        // this it renders as the bare word `TaskOutput`, which is what four of the live
        // slowest-six rows said on 2026-09-13.
        Some("TaskOutput") => field("task_id")
            .map(|task_id| capped(format!("TaskOutput · waiting on task {task_id}"))),
        Some(tool @ ("Read" | "Edit" | "Write" | "NotebookEdit")) => field("file_path")
            .or_else(|| field("notebook_path"))
            .map(|path| capped(format!("{tool} {path}"))),
        Some(tool @ ("Grep" | "Glob")) => {
            field("pattern").map(|pattern| capped(format!("{tool} {pattern}")))
        }
        // The HOST only: a fetch URL's path and query carry the search term or record id,
        // and the row only needs to say where the call went.
        Some("WebFetch") => field("url").map(|url| capped(format!("WebFetch {}", url_host(url)))),
        Some("WebSearch") => field("query").map(|query| capped(format!("WebSearch {query}"))),
        // Every tool without a dedicated arm above (`Monitor` and anything added after this
        // list) still gets a head: the bare tool name, so the row says what ran instead of
        // rendering blank. `None` here is what produced the head-less `slowest`/`timed_out`
        // rows this module's doc-comment and `WireSessionTracker::restore` both describe.
        _ => name.clone().map(capped),
    };
    ToolUseEvent {
        id,
        name,
        command_head,
        command_class,
    }
}

/// One tool call a Codex response asked for: the event every tool row is built from, plus
/// the call's own timeout, which a Claude `tool_use` never carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexToolCall {
    pub event: ToolUseEvent,
    /// The call's `timeout_ms` in whole seconds, rounded up; `None` when its arguments
    /// named no timeout, or a zero one.
    pub timeout_seconds: Option<u64>,
}

/// Most bytes of a function call's `arguments` string that [`codex_tool_call_from_item`]
/// decodes. Only a 120-character head is ever displayed, so this is display slack, as the
/// proxy's own cap is for a streamed Claude `tool_use` input. A larger call still counts,
/// under its bare tool name and with no timeout.
const CODEX_ARGUMENTS_CAP: usize = 64 * 1024;

/// The tool call one finished Codex output item asked for, or `None` for an item that is
/// no tool call (a message, a reasoning item) or names no call id. Item and argument names
/// are `openai/codex`'s at f82e365:
/// - the finished item under `item` of a `response.output_item.done` event
///   (`codex-api/src/sse/responses.rs:159-168,344-360,366-376`);
/// - the `function_call`, `custom_tool_call` and `local_shell_call` shapes
///   (`protocol/src/models.rs:1024-1027,1082-1123,1167-1187`; `core/src/tools/router.rs:257-309`);
/// - the interactive `exec_command`'s `cmd` and `yield_time_ms`
///   (`core/src/tools/handlers/shell_spec.rs:24-115`), and the one-shot `exec_command`'s
///   `timeout_ms` (`core/src/tools/spec_plan.rs:1276-1284`;
///   `core/src/tools/handlers/unified_exec/exec_command.rs:103-108,126-138`);
/// - `wait_agent`'s `timeout_ms` (`core/src/tools/handlers/multi_agents_spec.rs:287-313`), and
///   `apply_patch` as a freeform tool (`core/src/tools/handlers/apply_patch_spec.rs:9-28`).
///
/// - A `function_call` names its tool and carries its arguments as a JSON object encoded
///   in a string; a `custom_tool_call` (codex's freeform tools, `apply_patch` among them)
///   carries raw text instead; a `local_shell_call` is a command line in `action` (codex
///   parses that item, though at f82e365 it never offers the tool).
/// - A shell command (`exec_command`'s `cmd`, or a local shell call's argv) is headed and
///   classified by [`normalize_bash_command`], the normalizer a Claude `Bash` call goes
///   through, so one command reads the same whichever client ran it. Every other tool is
///   headed by its bare name, as an unlisted Claude tool is. A freeform call's `input` (for
///   `apply_patch`, the whole patch) is never read.
/// - `timeout_ms` from an `exec_command`'s arguments or a local shell call's action becomes
///   whole seconds, rounded up. Without it, or with a zero one, the call carries no
///   timeout, never a guessed one. Only the one-shot `exec_command`, which codex offers
///   when managed requirements turn unified exec off, carries one: the default interactive
///   `exec_command` has none, and its `yield_time_ms` is a yield interval, never read here.
///   Another tool's `timeout_ms` is not read either: `wait_agent`'s bounds a wait on
///   sub-agents, not a command.
pub fn codex_tool_call_from_item(item: &Value) -> Option<CodexToolCall> {
    // A fn, not a closure: a closure cannot tie its returned borrow to its argument's.
    fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
        value.get(key).and_then(Value::as_str)
    }
    let (id, name, arguments) = match text(item, "type")? {
        "function_call" => (
            text(item, "call_id")?,
            text(item, "name")?.to_string(),
            text(item, "arguments")
                .filter(|raw| raw.len() <= CODEX_ARGUMENTS_CAP)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok()),
        ),
        "custom_tool_call" => (
            text(item, "call_id")?,
            text(item, "name")?.to_string(),
            None,
        ),
        "local_shell_call" => (
            text(item, "call_id").or_else(|| text(item, "id"))?,
            "local_shell_call".to_string(),
            item.get("action").cloned(),
        ),
        _ => return None,
    };
    let command = match name.as_str() {
        "exec_command" => arguments
            .as_ref()
            .and_then(|args| text(args, "cmd"))
            .map(str::to_string),
        "local_shell_call" => arguments
            .as_ref()
            .and_then(|args| args.get("command"))
            .and_then(shell_script),
        _ => None,
    };
    let (command_head, command_class) = match command {
        Some(command) => {
            let (head, class) = normalize_bash_command(&command);
            (Some(head), Some(class))
        }
        None => (Some(name.chars().take(COMMAND_HEAD_MAX).collect()), None),
    };
    // A command's own deadline only: `wait_agent` also names a `timeout_ms`, and it is no
    // command timeout.
    let timeout_seconds = arguments
        .as_ref()
        .filter(|_| matches!(name.as_str(), "exec_command" | "local_shell_call"))
        .and_then(|args| args.get("timeout_ms"))
        .and_then(Value::as_u64)
        .map(|ms| ms.div_ceil(1000))
        .filter(|s| *s > 0);
    Some(CodexToolCall {
        event: ToolUseEvent {
            id: id.to_string(),
            name: Some(name),
            command_head,
            command_class,
        },
        timeout_seconds,
    })
}

/// The script an argv runs: the last word of `<shell> -c <script>` or
/// `<shell> -lc <script>` for `bash`, `sh` or `zsh` (by base name), which is how a shell
/// call wraps one, and the argv joined by spaces otherwise. A word that is not a string is
/// skipped, and `None` is for an argv that is not an array or holds no string.
fn shell_script(argv: &Value) -> Option<String> {
    let words: Vec<&str> = argv.as_array()?.iter().filter_map(Value::as_str).collect();
    match words.as_slice() {
        [] => None,
        [shell, "-c" | "-lc", script]
            if matches!(shell.rsplit('/').next(), Some("bash" | "sh" | "zsh")) =>
        {
            Some((*script).to_string())
        }
        _ => Some(words.join(" ")),
    }
}

/// The host part of a URL. Hand-rolled rather than adding a URL crate for one display field:
/// drop the scheme, cut at the first `/`, `?` or `#`, then drop any `user@` credentials prefix
/// (which is exactly the part that must never reach a panel row). A string carrying none of
/// those separators is its own host.
fn url_host(raw: &str) -> &str {
    let after_scheme = raw.split_once("://").map_or(raw, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

/// One tool call still awaiting its `tool_result`.
///
/// `Serialize`/`Deserialize` (and on every struct below it, down to
/// [`WireSession`]) exist for exactly one reader: [`crate::stores::session_wire_persist`],
/// which round-trips a session's live state to disk verbatim so a restored row
/// is the SAME struct the tracker already knows how to project, not a second
/// parallel shape that could drift from it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunningTool {
    pub tool: String,
    pub started_ms: i64,
    /// `#[serde(default)]` — NOT `skip_serializing`: the head persists (Gil, 2026-09-13,
    /// reversing #299; the module doc has the why). `default` stays so a cache file written
    /// by a build between #299 and that ruling, which omits the field entirely, still loads
    /// rather than taking every session in the file down with it.
    #[serde(default)]
    pub command_head: Option<String>,
    pub command_class: Option<CommandClass>,
    /// The call's own deadline in whole seconds, when its arguments named one (a Codex call's
    /// `timeout_ms`, which only codex's one-shot `exec_command` carries). `None` for every
    /// Claude call: its deadline belongs to the tool, not to the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

/// One completed tool call, for the "ten slowest" list — and, with the same fields, for
/// [`ToolStats::timed_out`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SlowTool {
    pub tool: String,
    pub seconds: f64,
    /// Same `#[serde(default)]` reasoning as [`RunningTool::command_head`].
    #[serde(default)]
    pub command_head: Option<String>,
    pub command_class: Option<CommandClass>,
    pub ended_ms: i64,
}

/// How many of a tool's most recent call durations are kept for [`ToolBucket::seconds_p50`] —
/// see the bridge ("a bounded reservoir per tool for the median").
pub const TOOL_DURATION_RESERVOIR_CAP: usize = 256;

/// One tool's aggregate stats within a session — the source for `SessionToolsRow::by_tool`
/// (`crates/tcr-status-wire`). Keyed on `tool_use.name` verbatim; `Read`, `Grep`, `Glob` and
/// `Edit` are deliberately NOT merged here (the panel groups them) — see the bridge.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolBucket {
    pub calls: u64,
    pub errors: u64,
    /// Completed calls of this tool whose duration was 60 seconds or more.
    pub over_one_minute: u64,
    /// Bounded reservoir of the last [`TOOL_DURATION_RESERVOIR_CAP`] completed calls'
    /// durations (seconds), for a wall-clock-cheap running median.
    durations: std::collections::VecDeque<f64>,
}

impl ToolBucket {
    fn record(&mut self, seconds: f64, is_error: bool) {
        self.calls += 1;
        if is_error {
            self.errors += 1;
        }
        if seconds >= 60.0 {
            self.over_one_minute += 1;
        }
        self.durations.push_back(seconds);
        if self.durations.len() > TOOL_DURATION_RESERVOIR_CAP {
            self.durations.pop_front();
        }
    }

    /// The median over the retained reservoir — not a true all-time median once the
    /// reservoir has evicted older samples, which is the bounded-memory tradeoff the bridge
    /// accepts. `0.0` on a bucket with no completed calls yet.
    pub fn seconds_p50(&self) -> f64 {
        if self.durations.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f64> = self.durations.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        if sorted.len().is_multiple_of(2) {
            f64::midpoint(sorted[mid - 1], sorted[mid])
        } else {
            sorted[mid]
        }
    }
}

/// Per-session tool aggregates.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolStats {
    pub calls: u64,
    pub errors: u64,
    pub timeouts: u64,
    /// [`Self::timeouts`] split by what the command WAS, keyed on
    /// [`CommandClass::as_str`] for a `Bash` call that carried one and on the tool's own
    /// name (`"Agent"`) for everything else — so the map always sums to `timeouts` and the
    /// panel's "TIMED OUT TODAY" section and its headline count read one number.
    ///
    /// `BTreeMap` rather than `HashMap` because this one is DISPLAYED: a stable key order
    /// keeps the section from reshuffling between two snapshots that say the same thing.
    /// `#[serde(default)]` so a cache file written before this field existed still loads.
    #[serde(default)]
    pub timeouts_by_class: std::collections::BTreeMap<String, u64>,
    /// The commands that timed out, newest first, capped at [`TIMED_OUT_CAP`].
    /// `#[serde(default)]` for the same restore reason as [`Self::timeouts_by_class`].
    #[serde(default)]
    pub timed_out: Vec<SlowTool>,
    /// Completed calls (any tool) whose duration was 60 seconds or more — the session-wide
    /// total; [`ToolBucket::over_one_minute`] carries the same count per tool.
    pub over_one_minute: u64,
    /// Keyed by `tool_use_id`. Capped at [`PENDING_TOOL_CAP`] per session.
    pub running: std::collections::HashMap<String, RunningTool>,
    /// Sorted by `seconds` descending, capped at [`SLOWEST_CAP`].
    pub slowest: Vec<SlowTool>,
    /// Per-tool aggregates keyed on `tool_use.name`.
    pub by_tool: std::collections::HashMap<String, ToolBucket>,
}

/// How many wall-clock minutes [`ReqPerMinuteRing`] retains.
pub const REQ_PER_MINUTE_LEN: usize = 30;

/// Requests seen in each of the last [`REQ_PER_MINUTE_LEN`] wall-clock minutes, oldest first,
/// always exactly that many entries — the source for `SessionRow::req_per_minute`
/// (`crates/tcr-status-wire`). Advanced on [`Self::record`] (a new request bumps the current
/// minute's bucket) and again, read-only, by [`Self::projected`] (`snapshot` calls this so an
/// idle session decays toward zeros instead of freezing on its last-seen minute).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReqPerMinuteRing {
    buckets: std::collections::VecDeque<u16>,
    /// The wall-clock minute number (`ms / 60_000`) the newest (last) bucket represents.
    head_minute: i64,
}

impl Default for ReqPerMinuteRing {
    fn default() -> Self {
        Self {
            buckets: std::iter::repeat_n(0, REQ_PER_MINUTE_LEN).collect(),
            head_minute: 0,
        }
    }
}

impl ReqPerMinuteRing {
    /// Shift the ring forward to `minute`, pushing a zero bucket per elapsed minute (capped
    /// at [`REQ_PER_MINUTE_LEN`] shifts, since anything beyond that clears the whole ring
    /// anyway). A `minute` at or before the current head is a no-op — this ring never runs
    /// backward.
    fn advance_to(&mut self, minute: i64) {
        let diff = minute - self.head_minute;
        if diff <= 0 {
            return;
        }
        let shift = diff.min(REQ_PER_MINUTE_LEN as i64) as usize;
        for _ in 0..shift {
            self.buckets.pop_front();
            self.buckets.push_back(0);
        }
        self.head_minute = minute;
    }

    fn record(&mut self, now_ms: i64) {
        self.advance_to(now_ms.div_euclid(60_000));
        if let Some(last) = self.buckets.back_mut() {
            *last = last.saturating_add(1);
        }
    }

    /// A read-only projection to `now_ms`, oldest first — never mutates stored state, so
    /// `snapshot` (which takes `&self`) can decay an idle session's ring for display without
    /// needing a write lock.
    pub fn projected(&self, now_ms: i64) -> Vec<u16> {
        let mut copy = self.clone();
        copy.advance_to(now_ms.div_euclid(60_000));
        copy.buckets.into_iter().collect()
    }
}

/// Model this session has never been attributed a usage record under.
const UNKNOWN_MODEL: &str = "unknown";

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// One model's raw token tally within a session — the input to pricing, kept apart from
/// [`WireSession::input_tokens`] and friends (the QUOTA counters, unchanged in meaning) so a
/// session that spans two models can be priced per-model and summed, rather than priced once
/// against whichever model happened to be current. `input` here is BASE input only (excludes
/// both cache dimensions), matching [`crate::records::usage::UsageRecord::input`] — never re-derive it
/// from `cache_5m + cache_1h + cache_read` here, since that is what pricing itself does.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelTokenTally {
    pub input: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub output: u64,
    /// Codex usage records in this tally, so its session can count unpriced requests.
    /// Zero and omitted for Claude; older cache files still load with that default.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub requests: u64,
}

/// One session's row.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WireSession {
    pub account: Option<String>,
    pub model: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub requests: u64,
    /// BASE input only — never cache reads or cache creation, both counted separately below.
    /// The 2026-09-14 incident: this field used to hold `UsageRecord::input_total()`, which
    /// already folds `cache_read_tokens` in, so the panel's hit-ratio (`cache_read /
    /// (input_tokens + cache_creation_tokens + cache_read_tokens)`) double-counted cache reads
    /// and every session rendered ~50% no matter its real hit rate.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    /// Cache-write tokens (5-minute plus 1-hour TTL creation), kept apart from
    /// `input_tokens` for the same reason as `cache_read_tokens` — see its doc-comment.
    /// `#[serde(default)]` so a cache file written before this field existed still loads.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    pub tools: ToolStats,
    /// See [`ReqPerMinuteRing`].
    pub req_per_minute: ReqPerMinuteRing,
    /// Per-model raw token tallies, keyed on the model id a usage record carried (or
    /// [`UNKNOWN_MODEL`] when it carried none) — the source `crate::manager::wire_sessions`
    /// prices into `SessionRow::cost_usd`, one model's price at a time, then sums.
    pub by_model: std::collections::HashMap<String, ModelTokenTally>,
    /// The prompt-cache tier of this session's most recent cache write, in seconds (3600 for
    /// the 1-hour tier, 300 for the 5-minute one); `None` before its first write. A response
    /// with no cache write leaves it alone, because a cache read refreshes the entry at the
    /// tier it was written with. `#[serde(default)]` so a cache file written before this
    /// field existed still loads.
    #[serde(default)]
    pub cache_ttl_secs: Option<u32>,
    /// Which provider's client this session is: Codex for a session named by codex's
    /// `session-id` header, Anthropic for every other. Persisted with a default, so a cache
    /// file written before this field existed loads as the Anthropic sessions it holds, and
    /// skipped when Anthropic, so a Claude-only fleet's file keeps its shape.
    #[serde(
        default,
        skip_serializing_if = "crate::base::provider::Provider::is_anthropic"
    )]
    pub provider: crate::base::provider::Provider,
    /// A Codex session's reasoning tokens: the part of `output_tokens` its model spent
    /// reasoning, already counted inside it. Zero and omitted on Anthropic sessions.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
    /// When a Codex session's latest usage was recorded: the stamp of the usage record its
    /// last finished turn produced, which lands when the stream ends, after `last_seen_ms`
    /// was set by the turn's response headers. `None` on every Anthropic session, and on a
    /// Codex session before its first usage; skipped when `None`, so a Claude-only fleet's
    /// file keeps its shape. See [`WireSession::is_live_at`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage_ms: Option<i64>,
}

impl WireSession {
    /// Whether this session is still live at `now_ms`, for a table that drops a session
    /// `ttl_ms` after its last request: within `ttl_ms` of `last_seen_ms`, or, for a session
    /// whose last usage was recorded later, until that usage has left a trailing `ttl_ms`
    /// window counted by whole minutes, plus one minute.
    ///
    /// Why the usage: a Codex request is counted when its response's headers arrive, and its
    /// usage only when the stream ends. The accounts' `usage.lastHour` counts that usage, and
    /// keeps its whole minute (`usage::range`), so on the request's clock alone a Codex row
    /// reports requests in the last hour for up to the stream's length plus a minute after its
    /// session left the list. TcrBar reads exactly that as a proxy too old to record Codex
    /// sessions. The extra minute is slack for the accounts and the sessions being read in two
    /// calls. A session with no usage time, every Claude session, is dropped by its last request
    /// alone, as before.
    pub fn is_live_at(&self, now_ms: i64, ttl_ms: i64) -> bool {
        now_ms.saturating_sub(self.last_seen_ms) <= ttl_ms
            || self.last_usage_ms.is_some_and(|usage_ms| {
                now_ms < (usage_ms.div_euclid(60_000) + 2) * 60_000 + ttl_ms
            })
    }
}

/// Tools whose `tool_use` block is the LAST content block of a turn, so the client never sends
/// another API request off the back of it: the proxy sees the `tool_use`, but nothing after it
/// carries the matching `tool_result`, and it sits in `running` until [`RUNNING_UNBOUNDED_LOST_MS`]
/// calls it lost. Measured against the live proxy, 2026-09-17: 51 of 57 `running` entries were
/// `StructuredOutput` (the Agent SDK's terminal tool), ages 2.6 to 246 minutes.
///
/// A `tool_result` for one of these DOES arrive occasionally (the live `sessions_summary.byTool`
/// row that day: `{'tool': 'StructuredOutput', 'calls': 15, 'errors': 15}`, 15 of 66 seen) — the
/// close path itself is not broken, the tool is simply terminal most of the time. Since
/// [`WireSessionTracker::insert_running`] never puts these in `running`, that rare late
/// `tool_result` has nothing to remove and is silently NOT counted into `calls`/`errors`/`by_tool`
/// either: the whole completion pipeline is gated on a `running` entry existing, so skipping the
/// insert also skips the count. That is the trade this constant makes — no ghost row, at the cost
/// of the already-rare (23%) completion count for exactly these tools.
///
/// A set, not a single name, so the next terminal tool is one line here.
///
/// A name list, not a shortened backstop for every uncapped tool: measured live across the
/// whole fleet the same day, stuck-vs-completed by tool was `StructuredOutput` 59/(59+15) =
/// 79.7%, against `Bash` 5/4846 = 0.1%, `Edit` 1/176 = 0.6%, `Write` 1/225 = 0.4%, and
/// `AskUserQuestion` 1/8 = 12.5% (n=8, and that tool genuinely blocks on a human, so its one
/// stuck entry is most likely real work, not a ghost). `StructuredOutput` sits two orders of
/// magnitude above every ordinary tool — categorically different, not a straggler — which is
/// what makes a name list proportionate here; shortening [`RUNNING_UNBOUNDED_LOST_MS`] for
/// every uncapped tool would misclassify the `Bash`/`Edit`/`Write`/`AskUserQuestion` rows above,
/// which are ordinary in-flight calls, exactly what `running` exists to show. The ghost count
/// was still climbing on the unpatched proxy while this was measured (51 at 19:31, 59 by
/// 20:0x, one unchanged process) — roughly one new ghost per four minutes of fleet activity,
/// capped only by [`PENDING_TOOL_CAP`] (64) per session.
pub const TERMINAL_TOOLS: &[&str] = &["StructuredOutput"];

/// Cap on pending (running) tool-use ids per session — see the bridge.
pub const PENDING_TOOL_CAP: usize = 64;
/// Cap on tracked sessions — see the bridge.
pub const SESSION_CAP: usize = 512;
/// How many of the slowest completed tool calls each session keeps.
pub const SLOWEST_CAP: usize = 10;
/// How many timed-out commands each session keeps, newest first
/// ([`ToolStats::timed_out`]). Twice [`SLOWEST_CAP`]: a timeout is rarer than a slow call
/// but its list is the one a person reads to spot a REPEATING command, and a pattern needs
/// more than ten rows to be visible. Still bounded — this rides the session-wire snapshot
/// to disk, once per session.
pub const TIMED_OUT_CAP: usize = 20;
/// A session unseen for this long is evicted — see the bridge ("an hour").
pub const SESSION_TTL_MS: i64 = 60 * 60 * 1000;

/// How long a `Bash` entry may sit in `running` before the panel calls it LOST rather than
/// running: the Bash tool's own 600 s timeout plus a 60 s grace for the round trip that
/// carries the `tool_result` back to the proxy.
///
/// Past that line the call cannot still be running — the client killed it at the timeout —
/// so an entry still here is one whose `tool_result` never reached this process (a request
/// served by a previous proxy, or a client that never sent the closing turn). Showing it as
/// running is the ghost Gil saw on 2026-09-13: two `Bash · 0s to timeout` rows at 17 minutes,
/// with a red ring, that nothing would ever close.
pub const RUNNING_BASH_LOST_MS: i64 = (600 + 60) * 1000;

/// The same line for every tool the Bash timeout does not govern — `Agent`, `Task`,
/// `TaskOutput` and the file tools have no known deadline, so this is a liveness backstop
/// rather than a deduction: six hours is longer than any real subagent this fleet has run
/// and shorter than a stale entry's useful life. Anything still `running` past it is a lost
/// result, not work in flight.
pub const RUNNING_UNBOUNDED_LOST_MS: i64 = 6 * 60 * 60 * 1000;

/// Is this `running` entry a call still in flight, or a result that was lost?
///
/// See [`RUNNING_BASH_LOST_MS`] and [`RUNNING_UNBOUNDED_LOST_MS`]. Read at PROJECTION time
/// (`crate::manager::wire_sessions::wire_sessions_snapshot`) rather than folded into the
/// table, so a `tool_result` that does arrive late still closes its entry and lands in
/// `slowest` with a real duration.
pub fn running_tool_is_lost(tool: &str, started_ms: i64, now_ms: i64) -> bool {
    let age_ms = now_ms.saturating_sub(started_ms);
    let limit = if tool == "Bash" {
        RUNNING_BASH_LOST_MS
    } else {
        RUNNING_UNBOUNDED_LOST_MS
    };
    age_ms > limit
}

/// [`running_tool_is_lost`] for one `running` entry. A call that named its own timeout (a Codex
/// call's `timeout_ms`, [`RunningTool::timeout_seconds`]) is lost once that timeout and the same
/// 60 s grace [`RUNNING_BASH_LOST_MS`] gives `Bash` have passed: codex ended the command at its
/// timeout, so a call still here is one whose closing request never reached this process. Every
/// other call keeps its tool's line, which is every Claude call, since none carries a timeout.
pub fn running_call_is_lost(call: &RunningTool, now_ms: i64) -> bool {
    match call.timeout_seconds {
        Some(seconds) => {
            let limit_ms = i64::try_from(seconds)
                .unwrap_or(i64::MAX)
                .saturating_add(60)
                .saturating_mul(1_000);
            now_ms.saturating_sub(call.started_ms) > limit_ms
        }
        None => running_tool_is_lost(&call.tool, call.started_ms, now_ms),
    }
}

/// The bounded, in-memory table `tcr status --json`'s `sessions` array is read from. No I/O; a
/// caller (`Manager`) is responsible for locking.
#[derive(Debug, Default)]
pub struct WireSessionTracker {
    sessions: std::collections::HashMap<String, WireSession>,
}

impl WireSessionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one request's parsed events into the table. `account`/`model` overwrite the
    /// session's last-known value when `Some`, and are left untouched when `None` (a request
    /// that could not be attributed to an account, or carried no `model` field, must not
    /// blank out a value a previous request already established).
    pub fn record_request(
        &mut self,
        session_id: &str,
        account: Option<String>,
        model: Option<String>,
        now_ms: i64,
        tool_uses: &[ToolUseEvent],
        tool_results: &[ToolResultEvent],
    ) {
        self.evict_stale(now_ms);

        let is_new = !self.sessions.contains_key(session_id);
        let entry = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| WireSession {
                first_seen_ms: now_ms,
                ..Default::default()
            });
        // The instant this session's previous response finished streaming — the "running"
        // start time for any tool_use first seen THIS request (see the bridge: "the moment
        // the proxy finished streaming the response that produced it"). A brand-new session
        // has no such instant, so its first tool_use starts "now" rather than at a fabricated
        // past time.
        let previous_response_end_ms = if is_new { now_ms } else { entry.last_seen_ms };

        entry.requests += 1;
        entry.last_seen_ms = now_ms;
        entry.req_per_minute.record(now_ms);
        if account.is_some() {
            entry.account = account;
        }
        if model.is_some() {
            entry.model = model;
        }

        for tu in tool_uses {
            Self::insert_running(&mut entry.tools, tu, previous_response_end_ms, None);
        }

        for tr in tool_results {
            if let Some(running) = entry.tools.running.remove(&tr.id) {
                Self::complete_call(&mut entry.tools, running, now_ms, tr.is_error, tr.timed_out);
            }
        }

        self.evict_over_cap();
    }

    /// Fold one finished call into its session's aggregates: the counts, the tool's bucket,
    /// the timeout tallies and the slowest list. Shared by a Claude `tool_result` and a Codex
    /// session's next request, so the two can never count a call differently.
    fn complete_call(
        tools: &mut ToolStats,
        running: RunningTool,
        now_ms: i64,
        is_error: bool,
        timed_out: bool,
    ) {
        let seconds = (now_ms - running.started_ms).max(0) as f64 / 1000.0;
        tools.calls += 1;
        if is_error {
            tools.errors += 1;
        }
        if seconds >= 60.0 {
            tools.over_one_minute += 1;
        }
        tools
            .by_tool
            .entry(running.tool.clone())
            .or_default()
            .record(seconds, is_error);
        let completed = SlowTool {
            tool: running.tool,
            seconds,
            command_head: running.command_head,
            command_class: running.command_class,
            ended_ms: now_ms,
        };
        if timed_out {
            tools.timeouts += 1;
            // The class if the classifier gave this call one, the tool's own name
            // otherwise — so the map sums to `timeouts` for every tool, not just
            // the `Bash` calls that have a class.
            let key = completed
                .command_class
                .map_or_else(|| completed.tool.clone(), |c| c.as_str().to_string());
            *tools.timeouts_by_class.entry(key).or_insert(0) += 1;
            tools.timed_out.insert(0, completed.clone());
            tools.timed_out.truncate(TIMED_OUT_CAP);
        }
        Self::insert_slowest(&mut tools.slowest, completed);
    }

    /// Add token counts learned from a response's usage — a separate call because usage is
    /// resolved later than the request (after the upstream response, sometimes after an SSE
    /// stream finishes), on a session that [`Self::record_request`] has already created. A
    /// session not (yet, or no longer) present is silently ignored — there is nothing to
    /// attribute the tokens to.
    ///
    /// The record's BASE input (`UsageRecord::input`) accumulates into `entry.input_tokens`,
    /// never the quota figure (`UsageRecord::input_total()`), which already folds cache reads
    /// and cache creation in; folding the quota figure in here is the 2026-09-14 double-count
    /// incident (see [`WireSession::input_tokens`]). Its model and both cache-creation tiers
    /// fold into [`WireSession::by_model`] so a session that spans two models can be priced
    /// per-model and summed, rather than averaged. The record is passed whole, never as its
    /// adjacent `u64`s, so a swapped `cache_5m` and `cache_1h` cannot compile.
    pub fn record_usage(&mut self, session_id: &str, record: &UsageRecord) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            Self::add_usage(entry, record);
            if record.cache_1h > 0 {
                entry.cache_ttl_secs = Some(3600);
            } else if record.cache_5m > 0 {
                entry.cache_ttl_secs = Some(300);
            }
        }
    }

    /// The token half of a response's usage, shared by [`Self::record_usage`] and
    /// [`Self::record_codex_usage`]: the session's totals and its per-model tally, which also
    /// counts Codex usage records so its session can report unpriced requests.
    fn add_usage(entry: &mut WireSession, record: &UsageRecord) {
        entry.input_tokens += record.input;
        entry.cache_creation_tokens += record.cache_5m + record.cache_1h;
        entry.output_tokens += record.output;
        entry.cache_read_tokens += record.cache_read;
        let tally = entry
            .by_model
            .entry(record.model.as_deref().unwrap_or(UNKNOWN_MODEL).to_string())
            .or_default();
        tally.input += record.input;
        tally.cache_5m += record.cache_5m;
        tally.cache_1h += record.cache_1h;
        tally.cache_read += record.cache_read;
        tally.output += record.output;
        match entry.provider {
            crate::base::provider::Provider::Anthropic => {}
            crate::base::provider::Provider::Codex => tally.requests += 1,
        }
    }

    /// Insert tool calls parsed from the RESPONSE this session's last request produced, as
    /// running from `now_ms` — the instant the stream ended, which is when the client actually
    /// started the tool. This is the half that makes `running` non-empty: see
    /// [`tool_use_event_from_block`] for why the request side alone never can.
    ///
    /// Deliberately NOT [`Self::record_request`]: this is the second half of a request already
    /// recorded, so it must not count another request, move `last_seen_ms`, or feed the
    /// per-minute ring. A session that is absent is ignored rather than created — the request
    /// half runs first and creates it, and a response alone names nothing to attribute to.
    ///
    /// The matching `tool_result` arrives in the NEXT request and closes the entry through
    /// [`Self::record_request`]'s result loop, which measures the duration from the
    /// `started_ms` set here; that same request also replays the `tool_use`, and the
    /// already-running skip in [`Self::insert_running`] is what keeps this earlier, truer
    /// start instant instead of overwriting it.
    pub fn record_response_tool_uses(
        &mut self,
        session_id: &str,
        now_ms: i64,
        tool_uses: &[ToolUseEvent],
    ) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            for tu in tool_uses {
                Self::insert_running(&mut entry.tools, tu, now_ms, None);
            }
        }
    }

    /// Count one Codex Responses turn on `session_id`, creating it as a Codex session, and close
    /// every call its previous response started.
    ///
    /// The close is the request's arrival. The request carries those calls' outputs in a body
    /// that is usually zstd and is never read here, so its arrival is all there is to measure
    /// by. Each call completes as a Claude `tool_result` completes one ([`Self::complete_call`]),
    /// timed to `now_ms`, as neither an error nor a timeout, which tcr cannot see. Calls close
    /// oldest first, so two that end together always sort the same way. Only this session's
    /// calls close: the request names this session and no other.
    pub fn record_codex_request(&mut self, session_id: &str, account: Option<String>, now_ms: i64) {
        self.record_request(session_id, account, None, now_ms, &[], &[]);
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.provider = crate::base::provider::Provider::Codex;
            let mut open: Vec<(String, RunningTool)> = entry.tools.running.drain().collect();
            open.sort_by(|a, b| {
                a.1.started_ms
                    .cmp(&b.1.started_ms)
                    .then_with(|| a.0.cmp(&b.0))
            });
            for (_, running) in open {
                Self::complete_call(&mut entry.tools, running, now_ms, false, false);
            }
        }
    }

    /// Count a pooled model request on its Codex session, with the endpoint available
    /// to distinguish the next conversation turn from auxiliary model work.
    pub fn record_codex_model_request(
        &mut self,
        session_id: &str,
        account: Option<String>,
        path: &str,
        now_ms: i64,
    ) {
        if path == crate::base::provider::CODEX_RESPONSES_PATH {
            self.record_codex_request(session_id, account, now_ms);
        } else {
            self.record_request(session_id, account, None, now_ms, &[], &[]);
            if let Some(entry) = self.sessions.get_mut(session_id) {
                entry.provider = crate::base::provider::Provider::Codex;
            }
        }
    }

    /// Start the tool calls a Codex response asked for, as running from `now_ms` (the instant
    /// its stream ended), each with its own timeout. The Codex twin of
    /// [`Self::record_response_tool_uses`]: it never counts a request, moves `last_seen_ms`,
    /// or creates a session.
    pub fn record_codex_tool_calls(
        &mut self,
        session_id: &str,
        now_ms: i64,
        calls: &[CodexToolCall],
    ) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            for call in calls {
                Self::insert_running(&mut entry.tools, &call.event, now_ms, call.timeout_seconds);
            }
        }
    }

    /// Add one Codex response's usage, recorded at the record's own stamp (`ts_ms`). It
    /// keeps the same totals and per-model tally as [`Self::record_usage`], with four
    /// differences. It stamps no cache TTL, because the 300 s and 3600 s tiers are Anthropic's
    /// prompt cache. It keeps the reasoning tokens, which are already inside `output`. The
    /// usage's model names the session, because a Codex request body is usually zstd, so the
    /// request named no model. And it keeps `ts_ms` as [`WireSession::last_usage_ms`], which
    /// keeps the session listed while the usage still counts in the last hour.
    pub fn record_codex_usage(&mut self, session_id: &str, record: &UsageRecord) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            Self::add_usage(entry, record);
            entry.reasoning_tokens += record.reasoning;
            if let Some(model) = &record.model {
                entry.model = Some(model.clone());
            }
            let ts_ms = record.ts_ms;
            entry.last_usage_ms = Some(entry.last_usage_ms.map_or(ts_ms, |at| at.max(ts_ms)));
        }
    }

    /// Put one `tool_use` into `running`, evicting the oldest entry when [`PENDING_TOOL_CAP`]
    /// is reached. An id already running is left ALONE — its recorded start is the earlier and
    /// therefore truer one (the response-side insert), and a later request replaying the same
    /// block must not reset the clock.
    fn insert_running(
        tools: &mut ToolStats,
        tu: &ToolUseEvent,
        started_ms: i64,
        timeout_seconds: Option<u64>,
    ) {
        if tools.running.contains_key(&tu.id) {
            return;
        }
        // A terminal tool (see [`TERMINAL_TOOLS`]) never gets a `tool_result`, so it must never
        // enter `running` in the first place — every caller of this function crosses this one
        // gate, rather than each of them filtering before calling in.
        if tu
            .name
            .as_deref()
            .is_some_and(|name| TERMINAL_TOOLS.contains(&name))
        {
            return;
        }
        if tools.running.len() >= PENDING_TOOL_CAP
            && let Some(oldest) = tools
                .running
                .iter()
                .min_by_key(|(_, r)| r.started_ms)
                .map(|(k, _)| k.clone())
        {
            tools.running.remove(&oldest);
        }
        tools.running.insert(
            tu.id.clone(),
            RunningTool {
                tool: tu.name.clone().unwrap_or_default(),
                started_ms,
                command_head: tu.command_head.clone(),
                command_class: tu.command_class,
                timeout_seconds,
            },
        );
    }

    fn insert_slowest(slowest: &mut Vec<SlowTool>, tool: SlowTool) {
        slowest.push(tool);
        slowest.sort_by(|a, b| {
            b.seconds
                .partial_cmp(&a.seconds)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        slowest.truncate(SLOWEST_CAP);
    }

    fn evict_stale(&mut self, now_ms: i64) {
        self.sessions
            .retain(|_, s| s.is_live_at(now_ms, SESSION_TTL_MS));
    }

    fn evict_over_cap(&mut self) {
        if self.sessions.len() > SESSION_CAP
            && let Some(oldest) = self
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.last_seen_ms)
                .map(|(k, _)| k.clone())
        {
            self.sessions.remove(&oldest);
        }
    }

    /// Every currently-live session, unordered. `now_ms` evicts stale entries first, so a
    /// session that crossed the one-hour idle line since its last write never surfaces; a
    /// Codex session's line is the later one [`WireSession::is_live_at`] gives it.
    pub fn snapshot(&self, now_ms: i64) -> Vec<(String, WireSession)> {
        self.sessions
            .iter()
            .filter(|(_, s)| s.is_live_at(now_ms, SESSION_TTL_MS))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Fold restored sessions (`crate::stores::session_wire_persist::load`'s output) into this
    /// table. Existing entries win — a session already tracked by a request served
    /// between boot and this call is fresher than anything on disk, same rule
    /// `Manager::restore_affinity` follows for pins — so this is meant to run once, at
    /// boot, before the listener binds.
    ///
    /// Every restored session's `tools.running` is DROPPED. A running list cannot be
    /// restored truthfully: the `tool_result` that closes one of these entries was addressed
    /// to the process that died, so nothing this process ever sees will close it, and the
    /// panel would draw it as a live call forever (measured 2026-09-13: two `Bash` rows at 17
    /// minutes against a 600 s timeout, restored across a restart). Counts, `slowest` and the
    /// per-tool buckets are all facts about calls that already FINISHED, so they restore
    /// unchanged — except `slowest` and `timed_out`, which each drop any entry whose
    /// `command_head` is `None`. Between #299 (heads stripped before save) and #302 (heads
    /// persisted again), every entry written to `session-wire.json` lost its head; restoring
    /// one gives a row with no command, and it sits there — Gil's screenshot, 2026-09-14 —
    /// until the session ages out (`SESSION_TTL_MS`). Since #298 every tool gets a head
    /// (`tool_use_event_from_block`'s default arm now falls back to the tool's own name), so
    /// a file written by the current build never has a head-less entry and this filter is a
    /// no-op on it; `calls`, `timeouts` and the other counts describe calls that happened
    /// regardless of whether a head survived, so they are untouched.
    pub fn restore(&mut self, sessions: std::collections::HashMap<String, WireSession>) {
        for (session_id, mut session) in sessions {
            session.tools.running.clear();
            session.tools.slowest.retain(|t| t.command_head.is_some());
            session.tools.timed_out.retain(|t| t.command_head.is_some());
            self.sessions.entry(session_id).or_insert(session);
        }
    }
}

/// A usage record carrying exactly these components, stamped at 0: what a test hands
/// [`WireSessionTracker::record_usage`] and its Codex twin, here and in
/// `manager::wire_sessions`. The quota figure is the sum of the parts, so the record's base
/// input is `base_input` exactly.
#[cfg(test)]
pub(crate) fn wire_usage(
    model: Option<&str>,
    base_input: u64,
    cache_5m: u64,
    cache_1h: u64,
    cache_read: u64,
    output: u64,
) -> UsageRecord {
    UsageRecord::from_quota_input(
        0,
        model.map(str::to_string),
        None,
        base_input + cache_5m + cache_1h + cache_read,
        cache_5m,
        cache_1h,
        cache_read,
        output,
    )
}

#[cfg(test)]
mod tests;
