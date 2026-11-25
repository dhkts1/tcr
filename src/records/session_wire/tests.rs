//! Unit tests for session and tool-call parsing and for [`super::WireSessionTracker`].

use super::*;

/// `cd <dir> && <simple clause>` reduces to just the clause — the hook's own
/// measurement: 18% of its "compound" rows were this exact shape, a free `cd` hiding a
/// simple command that deserved a real class.
#[test]
fn cd_and_a_simple_clause_reduces_to_the_clause() {
    let (head, class) = normalize_bash_command("cd ~/src/example && cargo test -p example");
    assert_eq!(head, "cargo test -p example");
    assert_eq!(class, CommandClass::Build);
}

/// A wrapper sitting right after the `cd`'s `&&` — `cd <dir> && timeout 15 rg ...` is the
/// shape `prefer-fd-rg` itself emits, and the very reason the hook strips wrappers a
/// SECOND time after the `cd` reduction, not just once up front.
#[test]
fn a_wrapper_in_front_of_the_reduced_clause_is_stripped_too() {
    let (head, class) = normalize_bash_command("cd ~/src/example && timeout 15 rg -n TODO");
    assert_eq!(head, "rg -n TODO");
    assert_eq!(class, CommandClass::Search);
}

/// An env-var prefix in front of the real command is stripped, and the class is read off
/// what remains.
#[test]
fn an_env_prefix_is_stripped_before_classifying() {
    let (head, class) = normalize_bash_command("CI=1 FOO=bar cargo build --release");
    assert_eq!(head, "cargo build --release");
    assert_eq!(class, CommandClass::Build);
}

/// A genuine compound loses its `cd` too, and stays `compound`.
///
/// This assertion used to read the other way — the reduction bailed when the remainder
/// held another `;`, `&` or `|`, so the whole string including the `cd` was the head.
/// That is exactly what put `cd ~/src/example/st…` on three RUNNING NOW rows
/// (2026-09-13): a path where the row's one line should carry a verb. The class is read
/// off the remainder and is unchanged by the drop, which is why dropping it is safe.
///
/// The head assertion below was updated again for the identity reduction (`identity`
/// module): a compound's head used to be the WHOLE reduced text; now it is that text's
/// IDENTITY — the first non-trivial clause — which is `cargo build` here, not `cargo
/// build && cargo test`. `class` is untouched, since it still reads the `&&` off the
/// reduced text before identity ever runs.
#[test]
fn a_genuine_compound_loses_its_cd_and_stays_compound() {
    let (head, class) = normalize_bash_command("cd ~/src/example && cargo build && cargo test");
    assert_eq!(head, "cargo build");
    assert_eq!(class, CommandClass::Compound);
}

/// The live shape from finding 3: a `cd` in front of a PIPELINE. The row must say what
/// the pipeline starts with, not which directory it ran in.
///
/// The head is the pipeline's IDENTITY (its first clause, `rg -n TODO`) since the
/// identity reduction landed, not the whole pipeline text; `class` still reads `Compound`
/// off the pre-identity reduced text.
#[test]
fn a_cd_in_front_of_a_pipeline_is_reduced() {
    let (head, class) = normalize_bash_command("cd ~/src/example && rg -n TODO | head -20");
    assert_eq!(head, "rg -n TODO");
    assert_eq!(class, CommandClass::Compound);
}

/// `;` as the separator, with a compound remainder — the same rule as `&&`. Head is the
/// identity of the first clause (`cargo build`), not the whole `a; b` text, since the
/// identity reduction landed.
#[test]
fn a_cd_with_a_semicolon_separator_is_reduced() {
    let (head, _) = normalize_bash_command("cd ~/src/example; cargo build; cargo test");
    assert_eq!(head, "cargo build");
}

/// A bare `cd X` has no clause to reduce TO, so it is left alone — dropping it would
/// leave the row with an empty head. `cd` is a trivial verb, so identity falls back to
/// showing the bare `cd` clause itself rather than reducing further.
#[test]
fn a_bare_cd_is_left_alone() {
    let (head, _) = normalize_bash_command("cd ~/src/example");
    assert_eq!(head, "cd ~/src/example");
}

/// `cd` is matched as a whole word: `cdx` is somebody's own command, not a directory
/// change, and its first token is what the class is read off. The head is `cdx`'s
/// identity (its own clause, dropping `&& cargo build`) since the identity reduction
/// landed — `cdx` is not a trivial verb, so it is not itself reduced further.
#[test]
fn a_command_merely_starting_with_cd_is_left_alone() {
    let (head, _) = normalize_bash_command("cdx ~/src/example && cargo build");
    assert_eq!(head, "cdx ~/src/example");
}

/// A backslash-continued line is, to the identity reducer, an UNQUOTED newline: it splits
/// there just as it would on `;`, so the identity is just `cargo test`, dropping the
/// continuation entirely. `class` is still read off the whole reduced text (still
/// contains the raw newline, still `Build`) before identity ever runs.
///
/// This assertion used to read `"cargo test \\⏎  -p example"` — the WHOLE reduced text
/// with its newline swapped for `⏎` for display. That was the pre-identity behavior; see
/// `an_embedded_quoted_newline_still_becomes_the_display_marker` below for where `⏎` is
/// still reachable now (a newline INSIDE quotes, which the identity reducer never splits
/// on).
#[test]
fn a_backslash_continued_line_splits_at_the_unquoted_newline() {
    let raw = "cd ~/src/example && cargo test \\\n  -p example";
    let (head, class) = normalize_bash_command(raw);
    assert_eq!(head, "cargo test");
    assert_eq!(class, CommandClass::Build);
}

/// A real newline INSIDE quotes is never a split point (only unquoted separators are), so
/// it survives identity reduction — and [`normalize_bash_command`] still swaps it for `⏎`
/// before the display cap, exactly as it did for the whole reduced text before the
/// identity step existed.
#[test]
fn an_embedded_quoted_newline_still_becomes_the_display_marker() {
    let (head, _) = normalize_bash_command("printf 'a\nb'");
    assert_eq!(head, "printf 'a⏎b'");
}

/// One classification per class name — matches `hooks/log-slow-bash.sh`'s own class
/// list, so a wire reader and the harness hook agree on the word.
#[test]
fn one_classification_per_class_name() {
    let cases: &[(&str, CommandClass)] = &[
        ("wait-for-line.sh --until foo", CommandClass::Wait),
        ("scripts/merge-when-green.sh 123", CommandClass::Wait),
        (
            "until grep -q ready log.txt; do sleep 1; done",
            CommandClass::Wait,
        ),
        ("sleep 30", CommandClass::Wait),
        ("cd ~/src/example && a; b", CommandClass::Compound),
        ("rg -n TODO ~/src/example", CommandClass::Search),
        ("git fetch origin main", CommandClass::GitNet),
        ("git status", CommandClass::GitLocal),
        ("cargo test -p example", CommandClass::Build),
        ("echo hello", CommandClass::Other),
    ];
    for (raw, expected) in cases {
        let (_, class) = normalize_bash_command(raw);
        assert_eq!(class, *expected, "for command {raw:?}");
    }
}

fn tool_use(id: &str, name: &str, command: Option<&str>) -> Value {
    serde_json::json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": command.map_or(Value::Null, |c| serde_json::json!({"command": c})),
    })
}

fn agent_tool_use(id: &str, name: &str, description: &str, subagent_type: Option<&str>) -> Value {
    let mut input = serde_json::json!({"description": description});
    if let Some(st) = subagent_type {
        input["subagent_type"] = Value::String(st.to_string());
    }
    serde_json::json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input,
    })
}

fn tool_result(id: &str, is_error: bool, text: &str) -> Value {
    serde_json::json!({
        "type": "tool_result",
        "tool_use_id": id,
        "is_error": is_error,
        "content": text,
    })
}

fn messages_raw(msgs: Value) -> Box<RawValue> {
    RawValue::from_string(msgs.to_string()).expect("valid json")
}

#[test]
fn extract_session_id_reads_the_embedded_field() {
    let blob = r#"{"device_id":"d1","account_uuid":"a1","session_id":"sess-123"}"#;
    assert_eq!(extract_session_id(blob), Some("sess-123".to_string()));
}

#[test]
fn extract_session_id_none_on_absence_or_garbage() {
    assert_eq!(extract_session_id(r#"{"device_id":"d1"}"#), None);
    assert_eq!(extract_session_id("not json"), None);
}

#[test]
fn extract_tool_events_pairs_use_and_result_in_the_last_two_messages() {
    let messages = serde_json::json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [tool_use("tu_1", "Bash", Some("ls -la"))]},
        {"role": "user", "content": [tool_result("tu_1", false, "ok")]},
    ]);
    let raw = messages_raw(messages);
    let (uses, results) = extract_tool_events(&raw);
    // Only the LAST TWO messages are read, so the assistant tool_use message is seen
    // (it's second-to-last) and the tool_result message (last) is seen too.
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].id, "tu_1");
    assert_eq!(uses[0].command_head.as_deref(), Some("ls -la"));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "tu_1");
    assert!(!results[0].is_error);
}

/// The block pair the API sends back for a server-side tool call, in the shape captured off
/// the wire on 2026-09-22: the `server_tool_use` and its encrypted result both sit in ONE
/// assistant message, beside the ordinary text and `tool_use` blocks of that turn. The
/// `encrypted_content` here is a placeholder — a real one decodes only on the org that
/// minted it and has no business in a public repo.
fn advisor_pair(id: &str) -> Value {
    serde_json::json!([
        {"type": "text", "text": "thinking"},
        {"type": "server_tool_use", "id": id, "name": "advisor", "input": {}},
        {"type": "advisor_tool_result", "tool_use_id": id,
         "content": {"type": "advisor_redacted_result", "encrypted_content": "AAAA"}},
    ])
}

/// A signed reasoning block, in the shape the API returns it and Claude Code echoes it.
fn thinking_block(signature: &str) -> Value {
    serde_json::json!({"type": "thinking", "thinking": "step one", "signature": signature})
}

/// The whole point of scanning ALL messages: the block that binds the conversation was
/// minted on turn one and is still echoed on turn ten, far outside the tail window
/// `extract_tool_events` walks. Every bound shape is found there.
#[test]
fn bound_tokens_are_found_anywhere_in_the_history() {
    let messages = serde_json::json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": advisor_pair("srvtoolu_fake_1")},
        {"role": "user", "content": [tool_result("tu_1", false, "ok")]},
        {"role": "assistant", "content": [thinking_block("sig_fake_1")]},
        {"role": "user", "content": "another turn"},
        {"role": "assistant", "content": [
            {"type": "redacted_thinking", "data": "data_fake_1"},
            tool_use("tu_2", "Bash", Some("ls")),
        ]},
        {"role": "user", "content": [tool_result("tu_2", false, "ok")]},
    ]);
    let found = extract_bound_tokens(&messages_raw(messages));
    assert_eq!(
        found.tokens,
        vec![
            "srvtoolu_fake_1".to_string(),
            "sig_fake_1".to_string(),
            "data_fake_1".to_string(),
        ]
    );
    assert!(found.carries_bound_history);
}

/// A client-side `tool_use` is not bound state: its result travels in the request body in
/// plain text and any account can read it. Benching the fleet for one would be a pure loss,
/// and — more to the point — claiming it as bound history would hold every ordinary
/// tool-using conversation to its pin.
#[test]
fn a_client_side_tool_use_is_not_bound_state() {
    let messages = serde_json::json!([
        {"role": "assistant", "content": [tool_use("toolu_fake_1", "Bash", Some("ls"))]},
        {"role": "user", "content": [tool_result("toolu_fake_1", false, "ok")]},
        {"role": "user", "content": "plain text carries nothing"},
    ]);
    let found = extract_bound_tokens(&messages_raw(messages));
    assert!(found.tokens.is_empty());
    assert!(!found.carries_bound_history);
}

/// A bound block whose token field is missing still means the history is bound — the
/// request must be held to its pin even though there is nothing to look up.
#[test]
fn a_bound_block_with_no_token_still_reports_bound_history() {
    let messages = serde_json::json!([
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "step one"}]},
    ]);
    let found = extract_bound_tokens(&messages_raw(messages));
    assert!(found.tokens.is_empty());
    assert!(found.carries_bound_history);
}

#[test]
fn repeated_and_multiple_bound_tokens_are_collected_once_each_and_capped() {
    let mut history = vec![serde_json::json!({"role": "user", "content": "hi"})];
    for n in 0..(BOUND_TOKEN_SCAN_CAP + 5) {
        history.push(serde_json::json!({
            "role": "assistant",
            "content": [thinking_block(&format!("sig_fake_{n}"))],
        }));
        // The same signature echoed again, as a real conversation does on every later turn.
        history.push(serde_json::json!({
            "role": "assistant",
            "content": [thinking_block("sig_fake_0")],
        }));
    }
    let found = extract_bound_tokens(&messages_raw(Value::Array(history)));
    assert_eq!(found.tokens.len(), BOUND_TOKEN_SCAN_CAP);
    assert_eq!(found.tokens[0], "sig_fake_0");
    assert_eq!(
        found.tokens.iter().filter(|t| *t == "sig_fake_0").count(),
        1,
        "a signature echoed on every turn is still one token"
    );
    assert!(found.carries_bound_history);
}

#[test]
fn a_body_with_no_bound_blocks_yields_nothing() {
    let raw = messages_raw(serde_json::json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
    ]));
    assert_eq!(extract_bound_tokens(&raw), BoundHistory::default());
    let malformed = RawValue::from_string("\"not an array\"".to_string()).expect("raw value");
    assert_eq!(
        extract_bound_tokens(&malformed),
        BoundHistory::default(),
        "a body that does not parse binds nothing, and never panics"
    );
}

#[test]
fn extract_tool_events_fills_command_head_for_an_agent_tool_with_subagent_type() {
    let messages = serde_json::json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [agent_tool_use(
            "tu_agent",
            "Agent",
            "check the wire fixtures",
            Some("reviewer"),
        )]},
    ]);
    let raw = messages_raw(messages);
    let (uses, _results) = extract_tool_events(&raw);
    assert_eq!(uses.len(), 1);
    assert_eq!(
        uses[0].command_head.as_deref(),
        Some("reviewer: check the wire fixtures")
    );
}

#[test]
fn extract_tool_events_fills_command_head_for_a_task_tool_without_subagent_type() {
    let messages = serde_json::json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [agent_tool_use(
            "tu_task",
            "Task",
            "review the diff",
            None,
        )]},
    ]);
    let raw = messages_raw(messages);
    let (uses, _results) = extract_tool_events(&raw);
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].command_head.as_deref(), Some("review the diff"));
}

#[test]
fn extract_tool_events_ignores_history_before_the_last_two() {
    let messages = serde_json::json!([
        {"role": "assistant", "content": [tool_use("stale", "Bash", Some("rm -rf /"))]},
        {"role": "user", "content": [tool_result("stale", false, "ok")]},
        {"role": "assistant", "content": "just text"},
        {"role": "user", "content": "another turn"},
    ]);
    let raw = messages_raw(messages);
    let (uses, results) = extract_tool_events(&raw);
    assert!(
        uses.is_empty(),
        "the stale tool_use is outside the last two messages"
    );
    assert!(results.is_empty());
}

#[test]
fn a_trailing_reminder_message_after_the_tool_result_must_not_hide_the_pair() {
    // Real shape, confirmed against this machine's own Claude Code transcripts
    // (~/.claude/projects/*.jsonl, parentUuid-chained — NOT a cross-thread artifact):
    // Claude Code appends a system-reminder as its OWN trailing `user` message,
    // separate from the `tool_result` message that precedes it, rather than folding
    // the reminder text into the tool_result's own content array. So by the time the
    // client actually fires the next request, the array's last THREE elements are
    // [assistant(tool_use), user(tool_result), user(reminder-only text)] — the
    // matching pair sits at [-3, -2], one step outside "the last two messages".
    let messages = serde_json::json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [tool_use("tu_reminded", "Bash", Some("ls -la"))]},
        {"role": "user", "content": [tool_result("tu_reminded", false, "ok")]},
        {"role": "user", "content": [{"type": "text", "text": "<system-reminder>...</system-reminder>"}]},
    ]);
    let raw = messages_raw(messages);
    let (uses, results) = extract_tool_events(&raw);

    let mut tracker = WireSessionTracker::new();
    tracker.record_request(
        "sess-reminded",
        Some("alice@example.com".into()),
        Some("claude-x".into()),
        1_000,
        &uses,
        &results,
    );

    let snap = tracker.snapshot(1_000);
    let (_, session) = &snap[0];
    assert_eq!(
        session.tools.calls, 1,
        "the tool call must still be counted even though a trailing reminder message \
             pushed the real pair one slot outside the last two messages"
    );
}

#[test]
fn a_tool_use_then_its_tool_result_yields_one_call_with_the_right_seconds() {
    let mut tracker = WireSessionTracker::new();
    let uses = vec![ToolUseEvent {
        id: "tu_1".into(),
        name: Some("Bash".into()),
        command_head: Some("sleep 5".into()),
        command_class: None,
    }];
    // First request: the tool_use appears, no result yet.
    tracker.record_request(
        "sess-a",
        Some("alice@example.com".into()),
        Some("claude-x".into()),
        1_000,
        &uses,
        &[],
    );
    // Second request, 5500ms later: the tool_result arrives.
    let results = vec![ToolResultEvent {
        id: "tu_1".into(),
        is_error: false,
        timed_out: false,
    }];
    tracker.record_request(
        "sess-a",
        Some("alice@example.com".into()),
        Some("claude-x".into()),
        6_500,
        &[],
        &results,
    );

    let snap = tracker.snapshot(6_500);
    assert_eq!(snap.len(), 1);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.calls, 1);
    assert_eq!(session.tools.errors, 0);
    assert!(session.tools.running.is_empty());
    assert_eq!(session.tools.slowest.len(), 1);
    // Started at the PREVIOUS request's time (1_000, this session's first-ever request,
    // so "previous response end" is that request's own arrival), resolved at 6_500.
    assert_eq!(session.tools.slowest[0].seconds, 5.5);
    assert_eq!(session.tools.slowest[0].tool, "Bash");
}

/// The RUNNING NOW half: a `tool_use` parsed out of the RESPONSE is running from stream
/// end until the NEXT request's `tool_result` closes it — and that next request replays
/// the same block without resetting its clock.
#[test]
fn a_response_side_tool_use_runs_until_the_next_requests_tool_result() {
    let mut tracker = WireSessionTracker::new();
    // The request that produced the turn — its own body carried no tool events.
    tracker.record_request("sess-r", None, None, 1_000, &[], &[]);

    // The response finished streaming at 2_000 carrying one tool_use.
    let uses = vec![ToolUseEvent {
        id: "tu_1".into(),
        name: Some("Bash".into()),
        command_head: Some("cargo test --release".into()),
        command_class: None,
    }];
    tracker.record_response_tool_uses("sess-r", 2_000, &uses);

    let mid = tracker.snapshot(2_500);
    let (_, session) = &mid[0];
    assert_eq!(
        session.tools.running.len(),
        1,
        "RUNNING NOW is non-empty between the response and the next request"
    );
    assert_eq!(session.tools.running["tu_1"].started_ms, 2_000);
    assert_eq!(
        session.requests, 1,
        "a response-side insert is the same request's second half, not another request"
    );

    // The next request, 7 seconds after the stream ended, replays the same tool_use AND
    // carries its tool_result.
    tracker.record_request(
        "sess-r",
        None,
        None,
        9_000,
        &uses,
        &[ToolResultEvent {
            id: "tu_1".into(),
            is_error: false,
            timed_out: false,
        }],
    );

    let snap = tracker.snapshot(9_000);
    let (_, session) = &snap[0];
    assert!(session.tools.running.is_empty(), "the result closed it");
    assert_eq!(session.tools.calls, 1);
    assert_eq!(
        session.tools.slowest[0].seconds, 7.0,
        "measured from the response-side start (2_000) — the replayed tool_use must not \
             reset it to this request's previous-response instant (1_000, which would read 8.0)"
    );
}

/// A response can only add to a session the request half already created: the request is
/// recorded first, at the same call site, and nothing else may conjure a session.
#[test]
fn a_response_side_tool_use_for_an_unknown_session_is_ignored() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_response_tool_uses(
        "sess-never-seen",
        2_000,
        &[ToolUseEvent {
            id: "tu_1".into(),
            name: Some("Bash".into()),
            command_head: None,
            command_class: None,
        }],
    );
    assert!(tracker.snapshot(2_000).is_empty());
}

/// Cause 2: every row said `TaskOutput` and nothing else because `command_head` was `None`
/// for all but Bash/Agent/Task.
#[test]
fn command_head_says_what_a_non_bash_tool_is_doing() {
    let cases: [(&str, Value, Option<&str>); 8] = [
        (
            "TaskOutput",
            serde_json::json!({"task_id": "task_9"}),
            Some("TaskOutput · waiting on task task_9"),
        ),
        (
            "Read",
            serde_json::json!({"file_path": "/tmp/example.rs"}),
            Some("Read /tmp/example.rs"),
        ),
        (
            "NotebookEdit",
            serde_json::json!({"notebook_path": "/tmp/example.ipynb"}),
            Some("NotebookEdit /tmp/example.ipynb"),
        ),
        (
            "Grep",
            serde_json::json!({"pattern": "fn main"}),
            Some("Grep fn main"),
        ),
        (
            "Glob",
            serde_json::json!({"pattern": "**/*.rs"}),
            Some("Glob **/*.rs"),
        ),
        (
            // The host only — a path and query carry the search term or record id.
            "WebFetch",
            serde_json::json!({"url": "https://example.com/orders/42?token=abc"}),
            Some("WebFetch example.com"),
        ),
        (
            "WebSearch",
            serde_json::json!({"query": "rust sse parser"}),
            Some("WebSearch rust sse parser"),
        ),
        // A tool with no dedicated rule falls back to its own name as the head — see
        // `a_tool_with_no_dedicated_head_arm_gets_its_own_name_as_the_head` for why `None`
        // here was the #299→#302 bug, not a deliberate fallback.
        (
            "SomeFutureTool",
            serde_json::json!({"whatever": 1}),
            Some("SomeFutureTool"),
        ),
    ];

    for (name, input, expected) in cases {
        let messages = messages_raw(serde_json::json!([
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "tu_1", "name": name, "input": input}],
            }
        ]));
        let (uses, _) = extract_tool_events(&messages);
        assert_eq!(uses.len(), 1, "for {name}");
        assert_eq!(uses[0].command_head.as_deref(), expected, "head for {name}");
        assert!(
            uses[0].command_class.is_none(),
            "class is Bash-only ({name})"
        );
    }
}

/// The field the head reads can be absent (a malformed or future input shape): that is a
/// missing head, never a fabricated one.
#[test]
fn a_missing_head_field_leaves_the_head_none() {
    let messages = messages_raw(serde_json::json!([
        {
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "tu_1", "name": "Read", "input": {}}],
        }
    ]));
    let (uses, _) = extract_tool_events(&messages);
    assert_eq!(uses[0].command_head, None);
}

/// A head is capped like a Bash command's — a long path cannot widen a panel row.
#[test]
fn a_long_head_is_capped_like_a_bash_command() {
    let long_path = format!("/tmp/{}", "a".repeat(400));
    let messages = messages_raw(serde_json::json!([
        {
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "tu_1",
                "name": "Read",
                "input": {"file_path": long_path},
            }],
        }
    ]));
    let (uses, _) = extract_tool_events(&messages);
    let head = uses[0].command_head.as_deref().expect("a head");
    assert_eq!(head.chars().count(), COMMAND_HEAD_MAX);
    assert!(head.starts_with("Read /tmp/aaa"));
}

/// What `session-wire.json` keeps of a Codex call stays inside what docs/architecture.md's
/// Files on disk table promises, "up to 120 characters of each slow or running tool call's
/// command": an `apply_patch` (a freeform `custom_tool_call` whose `input` is the raw patch)
/// is headed by its tool's name, never by the patch, and a long shell command is cut by the
/// same normalizer, to the same length, as a Claude `Bash` command.
#[test]
fn an_apply_patch_call_keeps_its_name_never_its_patch() {
    let patch = codex_tool_call_from_item(&serde_json::json!({
            "type": "custom_tool_call",
            "call_id": "call_1",
            "name": "apply_patch",
            "input": "*** Begin Patch\n*** Update File: /Users/example/src/lib.rs\n@@\n-old\n+new\n*** End Patch\n"
        }))
        .expect("a freeform tool call is a call");
    assert_eq!(
        (
            patch.event.command_head.as_deref(),
            patch.event.command_class,
            patch.timeout_seconds
        ),
        (Some("apply_patch"), None, None)
    );
    let long = codex_tool_call_from_item(&serde_json::json!({
            "type": "function_call",
            "call_id": "call_2",
            "name": "exec_command",
            "arguments": serde_json::json!({ "cmd": format!("cargo test {}", "x".repeat(400)) }).to_string()
        }))
        .expect("an exec_command call");
    let head = long.event.command_head.expect("a shell command has a head");
    assert!(head.chars().count() <= COMMAND_HEAD_MAX, "{head}");
}

/// A Codex call's `timeout_ms` becomes whole seconds, rounded up, so a sub-second deadline
/// never reads as no deadline at all; a zero one is no timeout. Only the one-shot
/// `exec_command` names a command timeout: the interactive one's `yield_time_ms` is a yield
/// interval, and `wait_agent`'s `timeout_ms` bounds a wait on sub-agents, so neither call
/// carries one.
#[test]
fn a_codex_timeout_is_whole_seconds_rounded_up() {
    let timeout_of = |name: &str, arguments: Value| {
        codex_tool_call_from_item(&serde_json::json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": name,
            "arguments": arguments.to_string()
        }))
        .expect("a function call is a call")
        .timeout_seconds
    };
    let one_shot = |ms: u64| {
        timeout_of(
            "exec_command",
            serde_json::json!({ "cmd": "cargo build", "timeout_ms": ms }),
        )
    };
    assert_eq!(one_shot(500), Some(1));
    assert_eq!(one_shot(0), None);
    assert_eq!(one_shot(120_000), Some(120));
    assert_eq!(
        timeout_of(
            "exec_command",
            serde_json::json!({ "cmd": "cargo build", "yield_time_ms": 10_000 })
        ),
        None,
        "a yield interval is no timeout"
    );
    assert_eq!(
        timeout_of(
            "wait_agent",
            serde_json::json!({ "targets": ["agent_1"], "timeout_ms": 30_000 })
        ),
        None,
        "a wait on sub-agents is no command timeout"
    );
}

/// A Codex tool call as the stream parse hands it over.
fn codex_call(id: &str, tool: &str, head: &str, timeout_seconds: Option<u64>) -> CodexToolCall {
    CodexToolCall {
        event: ToolUseEvent {
            id: id.to_string(),
            name: Some(tool.to_string()),
            command_head: Some(head.to_string()),
            command_class: Some(CommandClass::Build),
        },
        timeout_seconds,
    }
}

#[test]
fn auxiliary_codex_model_calls_leave_the_sessions_tools_running() {
    for path in [
        "/backend-api/codex/images/generations",
        "/backend-api/codex/images/edits",
        "/backend-api/codex/memories/trace_summarize",
        "/backend-api/codex/alpha/search",
    ] {
        let mut tracker = WireSessionTracker::new();
        tracker.record_codex_request("sess-c", Some("alice@example.com/codex".into()), 1_000);
        tracker.record_codex_tool_calls(
            "sess-c",
            2_000,
            &[codex_call("call_1", "exec_command", "cargo build", None)],
        );
        tracker.record_codex_model_request("sess-c", None, path, 3_000);
        let auxiliary = tracker.snapshot(3_000).pop().unwrap().1;
        assert_eq!(auxiliary.requests, 2, "{path} still counts as a request");
        assert_eq!(
            auxiliary.tools.running.len(),
            1,
            "{path} is not the next Responses turn"
        );
        assert_eq!(auxiliary.tools.calls, 0, "{path} carries no shell result");

        tracker.record_codex_model_request(
            "sess-c",
            None,
            crate::base::provider::CODEX_RESPONSES_PATH,
            4_000,
        );
        let next_turn = tracker.snapshot(4_000).pop().unwrap().1;
        assert_eq!(next_turn.requests, 3);
        assert!(next_turn.tools.running.is_empty());
        assert_eq!(next_turn.tools.calls, 1);
    }
}

/// A Codex call closes when the NEXT Responses turn on its session arrives. That request carries
/// the call's output, in a body tcr does not read, so its arrival is the close. The call
/// lands in the counts, its tool's bucket and the slowest list as a Claude call does, and
/// as neither an error nor a timeout, which tcr never saw.
#[test]
fn the_next_responses_turn_on_the_session_closes_its_codex_tools() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_codex_request("sess-c", Some("bob@example.com/codex".to_string()), 1_000);
    tracker.record_codex_tool_calls(
        "sess-c",
        2_000,
        &[codex_call(
            "call_1",
            "exec_command",
            "cargo test",
            Some(120),
        )],
    );
    let (_, open) = &tracker.snapshot(2_000)[0];
    assert_eq!(open.provider, crate::base::provider::Provider::Codex);
    assert_eq!(open.tools.running["call_1"].timeout_seconds, Some(120));

    tracker.record_codex_request("sess-c", None, 6_000);
    let (_, closed) = &tracker.snapshot(6_000)[0];
    assert!(
        closed.tools.running.is_empty(),
        "the next request closed the call"
    );
    assert_eq!(
        (
            closed.tools.calls,
            closed.tools.errors,
            closed.tools.timeouts
        ),
        (1, 0, 0)
    );
    assert_eq!(closed.tools.by_tool["exec_command"].calls, 1);
    assert_eq!(closed.tools.slowest.len(), 1);
    assert_eq!(closed.tools.slowest[0].seconds, 4.0);
    assert_eq!(closed.tools.slowest[0].ended_ms, 6_000);
    assert_eq!(
        closed.tools.slowest[0].command_head.as_deref(),
        Some("cargo test")
    );
    assert_eq!(closed.requests, 2);
    assert_eq!(
        closed.account.as_deref(),
        Some("bob@example.com/codex"),
        "a request that names no account keeps the one already known"
    );
}

/// The close is per session: a request on one `session-id` never closes another
/// session's calls, however many codex runs share the proxy. A call whose arguments
/// named no timeout carries none.
#[test]
fn a_request_on_another_session_closes_nothing() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_codex_request("sess-a", None, 1_000);
    tracker.record_codex_tool_calls(
        "sess-a",
        2_000,
        &[codex_call("call_1", "exec_command", "cargo build", None)],
    );
    tracker.record_codex_request("sess-b", None, 3_000);
    let sessions: std::collections::HashMap<String, WireSession> =
        tracker.snapshot(3_000).into_iter().collect();
    assert_eq!(
        sessions["sess-a"].tools.running.len(),
        1,
        "sess-b's request left it running"
    );
    assert_eq!(sessions["sess-a"].tools.calls, 0);
    assert_eq!(
        sessions["sess-a"].tools.running["call_1"].timeout_seconds,
        None
    );
}

/// A Codex session never gets a cache TTL: the 300 s and 3600 s tiers are Anthropic's
/// prompt cache, and a Codex cache write is neither, though it is still counted. The
/// usage's model names the session, its reasoning tokens are kept, and its model tally
/// counts the request. The same write on a Claude session still stamps its tier.
#[test]
fn codex_sessions_get_no_cache_ttl() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_codex_request("sess-codex", None, 1_000);
    tracker.record_codex_usage(
        "sess-codex",
        &UsageRecord {
            reasoning: 12,
            ts_ms: 1_000,
            ..wire_usage(Some("gpt-5.1-codex"), 1_000, 500, 0, 2_000, 40)
        },
    );
    tracker.record_request("sess-claude", None, None, 1_000, &[], &[]);
    tracker.record_usage(
        "sess-claude",
        &wire_usage(Some("claude-opus-5"), 1_000, 500, 0, 2_000, 40),
    );
    let sessions: std::collections::HashMap<String, WireSession> =
        tracker.snapshot(1_000).into_iter().collect();
    let codex = &sessions["sess-codex"];
    assert_eq!(codex.cache_ttl_secs, None);
    assert_eq!(
        codex.cache_creation_tokens, 500,
        "the write is still counted"
    );
    assert_eq!(codex.reasoning_tokens, 12);
    assert_eq!(codex.model.as_deref(), Some("gpt-5.1-codex"));
    assert_eq!(codex.by_model["gpt-5.1-codex"].requests, 1);
    assert_eq!(sessions["sess-claude"].cache_ttl_secs, Some(300), "control");
    assert_eq!(
        sessions["sess-claude"].provider,
        crate::base::provider::Provider::Anthropic
    );
}

#[test]
fn claude_session_cache_keeps_its_pre_codex_shape() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_request("sess-claude", None, None, 1_000, &[], &[]);
    tracker.record_usage(
        "sess-claude",
        &wire_usage(Some("claude-opus-5"), 100, 0, 0, 25, 10),
    );
    let (_, session) = tracker.snapshot(1_000).pop().unwrap();
    let wire = serde_json::to_value(&session).unwrap();
    assert!(wire.get("provider").is_none());
    assert!(wire.get("reasoning_tokens").is_none(), "{wire}");
    assert!(
        wire["by_model"]["claude-opus-5"].get("requests").is_none(),
        "{wire}"
    );
    assert_eq!(
        wire["requests"], 1,
        "the existing session request count stays"
    );
    assert_eq!(wire["by_model"]["claude-opus-5"]["input"], 100);

    tracker.record_codex_request("sess-codex", None, 1_000);
    tracker.record_codex_usage(
        "sess-codex",
        &UsageRecord {
            reasoning: 4,
            ts_ms: 1_000,
            ..wire_usage(Some("gpt-5.1-codex"), 100, 0, 0, 25, 10)
        },
    );
    let (_, codex) = tracker
        .snapshot(1_000)
        .into_iter()
        .find(|(id, _)| id == "sess-codex")
        .unwrap();
    let wire = serde_json::to_value(codex).unwrap();
    assert_eq!(wire["reasoning_tokens"], 4);
    assert_eq!(wire["by_model"]["gpt-5.1-codex"]["requests"], 1);
}

#[test]
fn an_unmatched_tool_use_id_stays_in_running() {
    let mut tracker = WireSessionTracker::new();
    let uses = vec![ToolUseEvent {
        id: "tu_orphan".into(),
        name: Some("Read".into()),
        command_head: None,
        command_class: None,
    }];
    tracker.record_request("sess-b", None, None, 1_000, &uses, &[]);
    let snap = tracker.snapshot(2_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.calls, 0);
    assert_eq!(session.tools.running.len(), 1);
    assert!(session.tools.running.contains_key("tu_orphan"));
}

/// A terminal tool's `tool_use` (parsed from a response) must never enter `running` — it
/// will never get a `tool_result`, so an entry here is a ghost that only the six-hour
/// backstop would ever clear.
#[test]
fn a_terminal_tool_use_never_enters_running() {
    let mut tracker = WireSessionTracker::new();
    let uses = vec![ToolUseEvent {
        id: "tu_structured".into(),
        name: Some("StructuredOutput".into()),
        command_head: None,
        command_class: None,
    }];
    // `record_response_tool_uses` is the path the live ghosts actually took (a response's
    // own tool_use, inserted at stream-end) — exercise the gate through it directly, on a
    // session `record_request` has already created.
    tracker.record_request("sess-terminal", None, None, 500, &[], &[]);
    tracker.record_response_tool_uses("sess-terminal", 1_000, &uses);
    let snap = tracker.snapshot(2_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.running.len(), 0);
}

/// The positive control: a real long-running tool (`Agent`) and an ordinary `Bash` call in
/// the SAME batch as a terminal tool must still land in `running` — proving the gate is
/// selective to [`TERMINAL_TOOLS`], not a blanket off-switch on `insert_running`.
#[test]
fn a_bash_and_agent_tool_use_still_land_in_running_beside_a_terminal_tool() {
    let mut tracker = WireSessionTracker::new();
    let uses = vec![
        ToolUseEvent {
            id: "tu_bash".into(),
            name: Some("Bash".into()),
            command_head: None,
            command_class: None,
        },
        ToolUseEvent {
            id: "tu_agent".into(),
            name: Some("Agent".into()),
            command_head: None,
            command_class: None,
        },
        ToolUseEvent {
            id: "tu_structured".into(),
            name: Some("StructuredOutput".into()),
            command_head: None,
            command_class: None,
        },
    ];
    tracker.record_request("sess-mixed", None, None, 1_000, &uses, &[]);
    let snap = tracker.snapshot(2_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.running.len(), 2);
    assert!(session.tools.running.contains_key("tu_bash"));
    assert!(session.tools.running.contains_key("tu_agent"));
    assert!(!session.tools.running.contains_key("tu_structured"));
}

#[test]
fn a_65th_pending_tool_use_evicts_the_oldest() {
    let mut tracker = WireSessionTracker::new();
    // Establish the session with a plain touch first (no tool_use), at a time distinctly
    // earlier than every subsequent one — a brand-new session's OWN first tool_use starts
    // at its own arrival time (see `record_request`'s doc), which would otherwise tie with
    // the second tool_use's start (the first request's own arrival time) and make "the
    // oldest" ambiguous between them.
    tracker.record_request("sess-c", None, None, 0, &[], &[]);
    for i in 0..PENDING_TOOL_CAP {
        let uses = vec![ToolUseEvent {
            id: format!("tu_{i}"),
            name: Some("Bash".into()),
            command_head: None,
            command_class: None,
        }];
        tracker.record_request("sess-c", None, None, 1_000 + i as i64, &uses, &[]);
    }
    let snap = tracker.snapshot(2_000);
    let (_, session) = &snap[0];
    // Exactly at the cap (64 pending ids): nothing evicted yet.
    assert_eq!(session.tools.running.len(), PENDING_TOOL_CAP);
    assert!(session.tools.running.contains_key("tu_0"));

    // The 65th distinct pending id.
    let uses = vec![ToolUseEvent {
        id: "tu_overflow".into(),
        name: Some("Bash".into()),
        command_head: None,
        command_class: None,
    }];
    tracker.record_request("sess-c", None, None, 2_000, &uses, &[]);
    let snap = tracker.snapshot(3_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.running.len(), PENDING_TOOL_CAP);
    assert!(session.tools.running.contains_key("tu_overflow"));
    assert!(
        !session.tools.running.contains_key("tu_0"),
        "the oldest pending id (tu_0, started at 0) must have been evicted to make room"
    );
    assert!(
        session.tools.running.contains_key("tu_1"),
        "the next-oldest survives"
    );
}

#[test]
fn a_session_unseen_for_an_hour_disappears() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_request(
        "sess-d",
        Some("alice@example.com".into()),
        None,
        0,
        &[],
        &[],
    );
    assert_eq!(
        tracker.snapshot(SESSION_TTL_MS).len(),
        1,
        "exactly at the TTL boundary it is still live"
    );
    assert_eq!(
        tracker.snapshot(SESSION_TTL_MS + 1).len(),
        0,
        "one ms past the TTL it is gone"
    );
}

/// A Codex session stays listed while its last usage still counts in the accounts'
/// `usage.lastHour`. The session's clock is its last request, counted when the response's
/// headers arrive; the usage is recorded when the stream ends, and `lastHour` keeps the
/// record's whole minute. So for up to the stream's length plus a minute after the hour, a
/// Codex row reported requests in the last hour while no Codex session was listed, which is
/// what TcrBar reads as a proxy too old to record Codex sessions, and it told a current
/// proxy to restart. Here the request is at T and the stream ends 40 s later; at every
/// second around the line, a request `lastHour` counts has its session listed, and both are
/// gone two minutes past the hour.
///
/// Watched red: on the request's clock alone, the session is gone at T + 1 h + 1 ms while
/// `lastHour` still counts its request.
#[test]
fn a_codex_session_stays_listed_while_its_usage_counts_in_the_last_hour() {
    const T: i64 = 1_767_225_600_000;
    let usage_at = T + 40_000;
    let mut tracker = WireSessionTracker::new();
    tracker.record_codex_request("sess-codex", Some("bob@example.com/codex".into()), T);
    tracker.record_codex_usage(
        "sess-codex",
        &UsageRecord {
            reasoning: 0,
            ts_ms: usage_at,
            ..wire_usage(Some("gpt-5.1-codex"), 1_000, 0, 0, 0, 10)
        },
    );
    let usage =
        crate::records::usage::UsageTracker::new(1, crate::base::pricing::PricingTable::default());
    usage.record(
        0,
        &UsageRecord::from_quota_input(
            usage_at,
            Some("gpt-5.1-codex".to_string()),
            None,
            1_000,
            0,
            0,
            0,
            10,
        ),
        "bob@example.com/codex",
        None,
        None,
    );
    let listed = |now: i64| {
        tracker
            .snapshot(now)
            .iter()
            .any(|(id, _)| id == "sess-codex")
    };
    let last_hour = |now: i64| usage.row(0, now, None).last_hour.requests;

    let past_the_request = T + SESSION_TTL_MS + 1;
    assert_eq!(
        last_hour(past_the_request),
        1,
        "control: the usage still counts in lastHour"
    );
    assert!(
        listed(past_the_request),
        "the session is listed while lastHour counts its request"
    );
    for now in (T + SESSION_TTL_MS - 5_000..=T + SESSION_TTL_MS + 3 * 60_000).step_by(1_000) {
        assert!(
            last_hour(now) == 0 || listed(now),
            "at {now} lastHour counts a Codex request and no Codex session is listed"
        );
    }
    let gone = T + SESSION_TTL_MS + 2 * 60_000;
    assert_eq!(last_hour(gone), 0);
    assert!(!listed(gone), "both are gone two minutes past the hour");
}

/// A restored session comes back with its finished-call facts and an EMPTY running list.
///
/// The ghost this pins (2026-09-13): the `tool_result` that would close a running entry
/// was addressed to the process that died, so a restored entry never closes and the panel
/// draws it as live forever. Counts and `slowest` describe calls that already finished,
/// so they survive the restart untouched.
#[test]
fn restore_drops_running_and_keeps_the_finished_facts() {
    let mut source = WireSessionTracker::new();
    source.record_request("sess-ghost", None, None, 1_000, &[], &[]);
    let uses = vec![
        ToolUseEvent {
            id: "tu_done".into(),
            name: Some("Bash".into()),
            command_head: Some("cargo test --release".into()),
            command_class: None,
        },
        ToolUseEvent {
            id: "tu_ghost".into(),
            name: Some("Bash".into()),
            command_head: Some("sleep 900".into()),
            command_class: None,
        },
    ];
    source.record_response_tool_uses("sess-ghost", 2_000, &uses);
    source.record_request(
        "sess-ghost",
        None,
        None,
        5_000,
        &[],
        &[ToolResultEvent {
            id: "tu_done".into(),
            is_error: false,
            timed_out: false,
        }],
    );
    let saved: std::collections::HashMap<String, WireSession> =
        source.snapshot(5_000).into_iter().collect();
    assert_eq!(
        saved["sess-ghost"].tools.running.len(),
        1,
        "the source table still holds the unclosed entry"
    );

    let mut restored = WireSessionTracker::new();
    restored.restore(saved);
    let snap = restored.snapshot(5_000);
    let (_, session) = &snap[0];
    assert!(
        session.tools.running.is_empty(),
        "no restored entry may be shown as running"
    );
    assert_eq!(session.tools.calls, 1, "the finished call survives");
    assert_eq!(session.tools.slowest.len(), 1);
    assert_eq!(session.requests, 2);
}

/// The #299→#302 window wrote `slowest`/`timed_out` entries with `command_head: None` to
/// `session-wire.json`; restoring one gave a row with no command that sat there until the
/// session aged out (2026-09-14, `slowest total 180, headless 80` on the live proxy). A
/// restored session must drop those entries but keep the headed ones, and `calls` — a
/// fact about calls that happened, head or no head — must not move.
#[test]
fn restore_drops_headless_slowest_and_timed_out_entries_but_keeps_calls() {
    let mut source = WireSessionTracker::new();
    source.record_request("sess-headless", None, None, 1_000, &[], &[]);
    let uses = vec![
        ToolUseEvent {
            id: "tu_headless".into(),
            name: Some("Bash".into()),
            command_head: None,
            command_class: None,
        },
        ToolUseEvent {
            id: "tu_headed".into(),
            name: Some("Bash".into()),
            command_head: Some("cargo test --release".into()),
            command_class: None,
        },
    ];
    source.record_response_tool_uses("sess-headless", 2_000, &uses);
    source.record_request(
        "sess-headless",
        None,
        None,
        5_000,
        &[],
        &[
            ToolResultEvent {
                id: "tu_headless".into(),
                is_error: false,
                timed_out: true,
            },
            ToolResultEvent {
                id: "tu_headed".into(),
                is_error: false,
                timed_out: false,
            },
        ],
    );
    let saved: std::collections::HashMap<String, WireSession> =
        source.snapshot(5_000).into_iter().collect();
    assert_eq!(saved["sess-headless"].tools.slowest.len(), 2);
    assert_eq!(saved["sess-headless"].tools.timed_out.len(), 1);
    assert_eq!(saved["sess-headless"].tools.calls, 2);

    let mut restored = WireSessionTracker::new();
    restored.restore(saved);
    let snap = restored.snapshot(5_000);
    let (_, session) = &snap[0];
    assert_eq!(
        session.tools.slowest.len(),
        1,
        "the head-less entry is dropped"
    );
    assert_eq!(
        session.tools.slowest[0].command_head.as_deref(),
        Some("cargo test --release"),
        "the headed entry survives"
    );
    assert!(
        session.tools.timed_out.is_empty(),
        "the head-less timed-out entry is dropped too"
    );
    assert_eq!(
        session.tools.calls, 2,
        "the count describes calls that happened, unaffected by whether a head survived"
    );
}

/// The default arm of [`tool_use_event_from_block`] — every tool without a dedicated
/// arm, `Monitor` included — must fall back to the tool's own name rather than `None`.
/// This is the other half of the #299→#302 fix: after it, no NEW entry can ever be
/// head-less, so [`restore`]'s filter above is a no-op on a healthy file.
#[test]
fn a_tool_with_no_dedicated_head_arm_gets_its_own_name_as_the_head() {
    let event = tool_use_event_from_block("tu_1".into(), Some("Monitor".into()), None);
    assert_eq!(event.command_head.as_deref(), Some("Monitor"));
}

/// A running entry too old to still be running is a LOST result — the projection's own
/// check, at both limits and on both sides of each.
#[test]
fn a_running_entry_past_its_limit_reads_as_lost() {
    assert!(
        !running_tool_is_lost("Bash", 0, RUNNING_BASH_LOST_MS),
        "exactly at the timeout-plus-grace line it may still be running"
    );
    assert!(
        running_tool_is_lost("Bash", 0, RUNNING_BASH_LOST_MS + 1),
        "one ms past it, the result was lost"
    );
    assert!(
        !running_tool_is_lost("Agent", 0, RUNNING_BASH_LOST_MS + 1),
        "a subagent has no Bash timeout — it may legitimately run for hours"
    );
    assert!(
        !running_tool_is_lost("Agent", 0, RUNNING_UNBOUNDED_LOST_MS),
        "exactly at the backstop it is still live"
    );
    assert!(
        running_tool_is_lost("TaskOutput", 0, RUNNING_UNBOUNDED_LOST_MS + 1),
        "one ms past the backstop, any tool is lost"
    );
}

#[test]
fn a_body_with_no_messages_changes_nothing() {
    // Modeled as: the caller found no `messages` field at all and so never calls
    // `extract_tool_events`; `record_request` with empty event slices must still update
    // request/account/model bookkeeping and touch nothing tool-related.
    let mut tracker = WireSessionTracker::new();
    tracker.record_request(
        "sess-e",
        Some("alice@example.com".into()),
        Some("claude-x".into()),
        1_000,
        &[],
        &[],
    );
    let snap = tracker.snapshot(1_000);
    let (_, session) = &snap[0];
    assert_eq!(session.requests, 1);
    assert_eq!(session.tools.calls, 0);
    assert!(session.tools.running.is_empty());
}

#[test]
fn extract_tool_events_empty_on_absent_or_malformed_messages() {
    let raw = RawValue::from_string("null".to_string()).unwrap();
    let (uses, results) = extract_tool_events(&raw);
    assert!(uses.is_empty());
    assert!(results.is_empty());

    let raw = RawValue::from_string("\"not an array\"".to_string()).unwrap();
    let (uses, results) = extract_tool_events(&raw);
    assert!(uses.is_empty());
    assert!(results.is_empty());
}

#[test]
fn a_timeout_error_is_counted_as_both_error_and_timeout() {
    let mut tracker = WireSessionTracker::new();
    let uses = vec![ToolUseEvent {
        id: "tu_timeout".into(),
        name: Some("Bash".into()),
        command_head: Some("sleep 700".into()),
        command_class: None,
    }];
    tracker.record_request("sess-f", None, None, 1_000, &uses, &[]);
    let results = vec![ToolResultEvent {
        id: "tu_timeout".into(),
        is_error: true,
        timed_out: true,
    }];
    tracker.record_request("sess-f", None, None, 601_000, &[], &results);
    let snap = tracker.snapshot(601_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.calls, 1);
    assert_eq!(session.tools.errors, 1);
    assert_eq!(session.tools.timeouts, 1);
}

/// Two timeouts of different command classes split into `timeouts_by_class` (which sums
/// to `timeouts`) and land in `timed_out` newest first, carrying the head the call had
/// while it was running. The classes come from the real classifier via
/// [`tool_use_event_from_block`], not hand-set, so this test fails if the classifier's
/// names and the wire's keys ever stop agreeing.
#[test]
fn timeouts_split_by_command_class_and_keep_the_commands() {
    let mut tracker = WireSessionTracker::new();
    let wait = tool_use_event_from_block(
        "tu_wait".into(),
        Some("Bash".into()),
        Some(&serde_json::json!({
            "command": "until grep -q ready build.log; do sleep 5; done"
        })),
    );
    let push = tool_use_event_from_block(
        "tu_push".into(),
        Some("Bash".into()),
        Some(&serde_json::json!({ "command": "git push origin main" })),
    );
    assert_eq!(wait.command_class, Some(CommandClass::Wait));
    assert_eq!(push.command_class, Some(CommandClass::GitNet));

    tracker.record_request("sess-t", None, None, 1_000, &[wait, push], &[]);
    // The `until` loop times out first, the `git push` a minute later — so the push is
    // the newer of the two and must lead `timed_out`.
    tracker.record_request(
        "sess-t",
        None,
        None,
        601_000,
        &[],
        &[ToolResultEvent {
            id: "tu_wait".into(),
            is_error: true,
            timed_out: true,
        }],
    );
    tracker.record_request(
        "sess-t",
        None,
        None,
        661_000,
        &[],
        &[ToolResultEvent {
            id: "tu_push".into(),
            is_error: true,
            timed_out: true,
        }],
    );

    let snap = tracker.snapshot(661_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.timeouts, 2);
    assert_eq!(
        session.tools.timeouts_by_class,
        [("wait".to_string(), 1u64), ("git-net".to_string(), 1u64)]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
    );
    assert_eq!(
        session.tools.timeouts_by_class.values().sum::<u64>(),
        session.tools.timeouts,
        "the per-class split must sum to the headline count"
    );
    assert_eq!(session.tools.timed_out.len(), 2);
    assert_eq!(
        session.tools.timed_out[0].command_head.as_deref(),
        Some("git push origin main"),
        "newest first"
    );
    assert_eq!(
        session.tools.timed_out[0].command_class,
        Some(CommandClass::GitNet)
    );
    assert_eq!(session.tools.timed_out[0].ended_ms, 661_000);
    // `until` is a control-flow keyword, so identity reduction keeps only its first
    // clause (up to the `;`), dropping `do sleep 5; done` — the loop's OWN condition is
    // what says what it's waiting on, not its body.
    assert_eq!(
        session.tools.timed_out[1].command_head.as_deref(),
        Some("until grep -q ready build.log")
    );
}

/// A timed-out call that is not a classified `Bash` command counts under its TOOL name,
/// so the map still sums to `timeouts` rather than dropping the row on the floor.
#[test]
fn a_non_bash_timeout_counts_under_its_tool_name() {
    let mut tracker = WireSessionTracker::new();
    let agent = tool_use_event_from_block(
        "tu_agent".into(),
        Some("Agent".into()),
        Some(&serde_json::json!({ "description": "check the wire fixtures" })),
    );
    assert_eq!(agent.command_class, None);
    tracker.record_request("sess-a", None, None, 1_000, &[agent], &[]);
    tracker.record_request(
        "sess-a",
        None,
        None,
        2_000,
        &[],
        &[ToolResultEvent {
            id: "tu_agent".into(),
            is_error: true,
            timed_out: true,
        }],
    );
    let snap = tracker.snapshot(2_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.timeouts, 1);
    assert_eq!(session.tools.timeouts_by_class.get("Agent"), Some(&1));
}

/// `timed_out` is bounded like every other per-session list here, and drops the OLDEST
/// rows when it overflows — the newest timeout is the one worth reading.
#[test]
fn timed_out_is_capped_and_keeps_the_newest() {
    let mut tracker = WireSessionTracker::new();
    for i in 0..(TIMED_OUT_CAP + 5) {
        let id = format!("tu_{i}");
        let use_event = tool_use_event_from_block(
            id.clone(),
            Some("Bash".into()),
            Some(&serde_json::json!({ "command": format!("sleep {i}") })),
        );
        let clock = 1_000 + i as i64 * 1_000;
        tracker.record_request("sess-c", None, None, clock, &[use_event], &[]);
        tracker.record_request(
            "sess-c",
            None,
            None,
            clock + 500,
            &[],
            &[ToolResultEvent {
                id,
                is_error: true,
                timed_out: true,
            }],
        );
    }
    let snap = tracker.snapshot(1_000 + (TIMED_OUT_CAP + 5) as i64 * 1_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.timeouts, (TIMED_OUT_CAP + 5) as u64);
    assert_eq!(session.tools.timed_out.len(), TIMED_OUT_CAP);
    assert_eq!(
        session.tools.timed_out[0].command_head.as_deref(),
        Some(format!("sleep {}", TIMED_OUT_CAP + 4).as_str())
    );
}

/// `by_tool` keeps separate buckets per tool NAME (three tools, two of which get more
/// than one completed call), tracks each bucket's own calls/errors, and reports the
/// median of its own reservoir — not a global median across every tool.
#[test]
fn by_tool_tracks_calls_errors_and_a_per_tool_median() {
    let mut tracker = WireSessionTracker::new();
    // Bash: three completed calls back-to-back on a strictly increasing clock, each
    // one's start pinned to the PREVIOUS call's own end (see `record_request`'s doc on
    // `previous_response_end_ms`) — durations 1s, 3s, 5s (median 3s), the second an error.
    let mut clock_ms = 0i64;
    for (i, (secs, is_error)) in [(1.0, false), (3.0, true), (5.0, false)]
        .into_iter()
        .enumerate()
    {
        let id = format!("bash_{i}");
        tracker.record_request(
            "sess-tools",
            None,
            None,
            clock_ms,
            &[ToolUseEvent {
                id: id.clone(),
                name: Some("Bash".into()),
                command_head: None,
                command_class: None,
            }],
            &[],
        );
        clock_ms += (secs * 1000.0) as i64;
        tracker.record_request(
            "sess-tools",
            None,
            None,
            clock_ms,
            &[],
            &[ToolResultEvent {
                id,
                is_error,
                timed_out: false,
            }],
        );
    }
    // Read: one completed call, 2s, no error — same rule: starts where the clock left off.
    tracker.record_request(
        "sess-tools",
        None,
        None,
        clock_ms,
        &[ToolUseEvent {
            id: "read_0".into(),
            name: Some("Read".into()),
            command_head: None,
            command_class: None,
        }],
        &[],
    );
    clock_ms += 2_000;
    tracker.record_request(
        "sess-tools",
        None,
        None,
        clock_ms,
        &[],
        &[ToolResultEvent {
            id: "read_0".into(),
            is_error: false,
            timed_out: false,
        }],
    );

    let snap = tracker.snapshot(clock_ms);
    let (_, session) = &snap[0];
    assert_eq!(
        session.tools.by_tool.len(),
        2,
        "Bash and Read stay separate buckets"
    );
    let bash = &session.tools.by_tool["Bash"];
    assert_eq!(bash.calls, 3);
    assert_eq!(bash.errors, 1);
    assert_eq!(bash.seconds_p50(), 3.0, "the middle of [1, 3, 5]");
    let read = &session.tools.by_tool["Read"];
    assert_eq!(read.calls, 1);
    assert_eq!(read.errors, 0);
    assert_eq!(read.seconds_p50(), 2.0);
}

/// A completed call of 60 seconds or more counts toward both the per-tool
/// `over_one_minute` and the session-wide total — independent of `errors`/`timeouts`, so a
/// slow-but-successful call still counts.
#[test]
fn over_one_minute_counts_slow_completed_calls_regardless_of_error_status() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_request(
        "sess-slow",
        None,
        None,
        0,
        &[ToolUseEvent {
            id: "tu_slow".into(),
            name: Some("Bash".into()),
            command_head: Some("sleep 90".into()),
            command_class: None,
        }],
        &[],
    );
    tracker.record_request(
        "sess-slow",
        None,
        None,
        90_000,
        &[],
        &[ToolResultEvent {
            id: "tu_slow".into(),
            is_error: false,
            timed_out: false,
        }],
    );
    let snap = tracker.snapshot(90_000);
    let (_, session) = &snap[0];
    assert_eq!(session.tools.over_one_minute, 1);
    assert_eq!(session.tools.by_tool["Bash"].over_one_minute, 1);
    assert_eq!(session.tools.errors, 0, "a slow call need not be an error");
}

/// After 31 simulated minutes of one request per minute, the ring has dropped the very
/// first minute's count and kept exactly the last 30, oldest first.
#[test]
fn req_per_minute_ring_drops_the_first_minute_after_31_minutes() {
    let mut tracker = WireSessionTracker::new();
    for minute in 0..31 {
        tracker.record_request("sess-ring", None, None, minute * 60_000, &[], &[]);
    }
    let snap = tracker.snapshot(30 * 60_000);
    let (_, session) = &snap[0];
    let ring = session.req_per_minute.projected(30 * 60_000);
    assert_eq!(ring.len(), REQ_PER_MINUTE_LEN);
    assert_eq!(
        ring,
        vec![1u16; REQ_PER_MINUTE_LEN],
        "minutes 1..=30 each got exactly one request; minute 0 was pushed out"
    );
}

/// A session with no NEW requests for a while decays toward zeros when projected forward
/// — the ring must not freeze on whatever it last recorded.
#[test]
fn req_per_minute_decays_to_zero_when_projected_past_the_last_request() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_request("sess-idle", None, None, 0, &[], &[]);
    let snap = tracker.snapshot(0);
    let (_, session) = &snap[0];
    // Immediately: minute 0 shows one request.
    assert_eq!(session.req_per_minute.projected(0).last(), Some(&1));
    // 35 minutes later, with no new request, minute 0 has scrolled off entirely.
    let projected = session.req_per_minute.projected(35 * 60_000);
    assert_eq!(
        projected,
        vec![0u16; REQ_PER_MINUTE_LEN],
        "35 idle minutes is more than the 30-minute window, so every bucket is zero"
    );
}

/// Every shape the identity reducer has to survive, one row each, with the reason it is
/// here. Each case is drawn from a corpus of 40,589 real Bash commands, rewritten with
/// `alice`-style paths because this repository is public.
///
/// This replaces an `#[ignore]`d check that read a 40,589-row fixture out of `/tmp`. That
/// one could not fail for the right reason and could fail for a wrong one: CI never ran
/// it, it returned early (passing) whenever the file was absent, and it asserted an EXACT
/// mismatch count, so handing it a cleaner fixture made it go red. A table that always
/// runs beats a corpus that never does.
#[test]
fn identity_reduces_every_shape_the_corpus_contains() {
    // (raw command, expected identity, why this row exists)
    let cases: &[(&str, &str, &str)] = &[
        (
            "W=/Users/alice/wt/job-1612; git -C $W status --short | head",
            "git -C $W status --short",
            "a bare assignment is not a command; taking its name yielded `W` 1,177 times",
        ),
        (
            "/opt/homebrew/bin/bash /tmp/hand.sh JOB-1 2>&1 | tail -6",
            "bash hand.sh JOB-1",
            "slicing by verb length cut into the path and yielded `bash bash` 413 times",
        ),
        (
            "cd /Users/alice/git/x || exit; rg -n foo src/",
            "rg -n foo src/",
            "the right side of || is an error handler, never the identity",
        ),
        (
            "sed -i 's|aaa|bbb|' file.py",
            "sed -i 's|aaa|bbb|' file.py",
            "separators INSIDE quotes must not split; a regex split yielded `sed -i 's`",
        ),
        (
            "cd X && (make build > /tmp/a.log 2>&1; echo done)",
            "make build ; echo done",
            "a subshell's closing paren must not survive as an orphan",
        ),
        (
            "python3 -c \"import json\" 2>/dev/null",
            "python3 -c \u{2039}script\u{203a}",
            "an inline script is not a filename",
        ),
        (
            "CARGO_TARGET_DIR=/x/y timeout 590 cargo check --manifest-path Cargo.toml",
            "cargo check --manifest-path Cargo.toml",
            "env prefix and wrapper both strip before the verb is read",
        ),
        (
            "cat > /tmp/out.txt <<EOF\nhi\nEOF",
            "cat > /tmp/out.txt",
            "for a heredoc that writes a file, the file IS the point",
        ),
        (
            "for i in 1 2 3; do echo $i; done",
            "for i in 1 2 3",
            "a control structure keeps its own opening clause",
        ),
        (
            "printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
            "printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\u{2026}'",
            "one long quoted token must be cut and REPAIRED, not trimmed back to the bare verb",
        ),
    ];
    for (raw, expected, why) in cases {
        assert_eq!(&identity::identity(raw), expected, "{why}\n  raw: {raw:?}");
    }
}

/// A session's cache tier is the tier of its MOST RECENT cache write: a 1-hour write sets
/// 3600, a response that wrote nothing keeps it (a read refreshes the entry at the tier it
/// was written with), and a later 5-minute write sets 300. Absent before the first write.
#[test]
fn cache_ttl_follows_the_most_recent_cache_write() {
    let mut tracker = WireSessionTracker::new();
    tracker.record_request("sess-ttl", None, None, 1_000, &[], &[]);
    assert_eq!(tracker.snapshot(1_000)[0].1.cache_ttl_secs, None);

    // Reads and base input alone do not name a tier.
    tracker.record_usage("sess-ttl", &wire_usage(None, 10, 0, 0, 500, 5));
    assert_eq!(tracker.snapshot(1_000)[0].1.cache_ttl_secs, None);

    tracker.record_usage("sess-ttl", &wire_usage(None, 10, 0, 40, 0, 5));
    assert_eq!(tracker.snapshot(1_000)[0].1.cache_ttl_secs, Some(3600));

    tracker.record_usage("sess-ttl", &wire_usage(None, 10, 0, 0, 900, 5));
    assert_eq!(
        tracker.snapshot(1_000)[0].1.cache_ttl_secs,
        Some(3600),
        "a response with no cache write keeps the last tier"
    );

    tracker.record_usage("sess-ttl", &wire_usage(None, 10, 20, 0, 0, 5));
    assert_eq!(tracker.snapshot(1_000)[0].1.cache_ttl_secs, Some(300));
}

/// The repair is forward, never backward. Trimming back to the last balanced position is
/// what produced a bare `rg -n` for 532 commands: the argument was thrown away to keep the
/// result well-formed. Closing the quote keeps both.
#[test]
fn an_unterminated_cut_is_closed_rather_than_discarded() {
    let raw = "rg -n \"enum PanelTab|case sessions|case tools|case accounts|case activity\" src/";
    let got = identity::identity(raw);
    assert!(
        got.len() > 20,
        "a repair that keeps only the flags has discarded the argument: {got:?}"
    );
    assert_eq!(
        got.matches('"').count() % 2,
        0,
        "the identity must not end inside a quote: {got:?}"
    );
}
