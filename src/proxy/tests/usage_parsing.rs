//! Usage, tool-use and token parsing from streamed and non-streamed Anthropic and Codex responses.

use super::*;

/// Bug #1 + cache counting: a `message_start` split across TWO chunks is
/// still parsed whole, and `input` sums base + cache-creation + cache-read.
#[tokio::test]
async fn sse_usage_sums_cache_tokens_across_split_chunks() {
    let full = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{",
        "\"input_tokens\":10,\"cache_creation_input_tokens\":100,",
        "\"cache_read_input_tokens\":1000,\"output_tokens\":1}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    // Split mid-way through the first event's data line.
    let split = 60usize;
    let chunks = vec![
        Ok::<Bytes, Infallible>(Bytes::copy_from_slice(&full.as_bytes()[..split])),
        Ok::<Bytes, Infallible>(Bytes::copy_from_slice(&full.as_bytes()[split..])),
    ];
    let SseParse {
        usage: parsed,
        stream_error,
        ..
    } = parse_sse_usage(
        futures::stream::iter(chunks),
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(
        stream_error, None,
        "a stream that reaches message_stop has no error event"
    );
    // R1: the quota sum is byte-identical to before.
    assert_eq!(
        parsed.input_total, 1110,
        "10 + 100 (cache-creation) + 1000 (cache-read)"
    );
    assert_eq!(parsed.output, 42);
    // NEW: the cache components are now surfaced SEPARATELY (not summed away).
    assert_eq!(
        parsed.cache_read, 1000,
        "cache-read retained on the SSE path"
    );
    assert_eq!(parsed.cache_creation, 100, "cache-creation retained");
}

/// No double-count: multiple `message_delta`s yield the FINAL cumulative
/// output, not the sum, so applying the total once is correct.
#[tokio::test]
async fn sse_usage_takes_final_output_not_sum() {
    let full = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":37}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
    let SseParse {
        usage: parsed,
        stream_error,
        ..
    } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(
        stream_error, None,
        "a stream that reaches message_stop has no error event"
    );
    assert_eq!(parsed.input_total, 5);
    assert_eq!(parsed.output, 37, "final cumulative output, not 20 + 37");
    // No cache tokens in this fixture → both stay zero.
    assert_eq!(parsed.cache_read, 0);
    assert_eq!(parsed.cache_creation, 0);
}

/// FALSE-POSITIVE case #3 (contract-derived, not a path allowlist): a
/// stream that PRODUCED events but none of them `message_start` was
/// never confirmed to BE a Messages turn in the first place — a future
/// non-Messages endpoint that happens to share `text/event-stream` looks
/// exactly like this from inside `parse_sse_usage`. This is the only
/// shape that earns the exemption. A round-4 fix (see the sibling test
/// below) narrowed this from "no `message_start`" to "no `message_start`
/// AND at least one other event arrived" — a stream with ZERO events is
/// a different, indistinguishable-from-severed-early shape and must NOT
/// share this exemption; conflating the two was exactly the round-4 bug.
#[tokio::test]
async fn sse_stream_with_non_message_events_records_no_stream_error() {
    // A heartbeat-shaped event, never `message_start` — stands in for a
    // non-Messages endpoint that happens to emit `text/event-stream`.
    let full = concat!("event: heartbeat\n", "data: {\"type\":\"heartbeat\"}\n\n",);
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
    let SseParse {
        usage: parsed,
        stream_error,
        ..
    } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(parsed, ParsedUsage::default());
    assert_eq!(
        stream_error, None,
        "a stream that produced events but none of them message_start \
             was genuinely never a Messages turn — it must not be classified \
             as truncated just because it also has no message_stop"
    );
}

/// THE SEVEREST case this classifier exists to catch (round-4 fix): a
/// stream cut off BEFORE its first parseable event ever arrives. Before
/// this fix, `saw_message_start` gated the verdict on its own, so this
/// looked IDENTICAL to the sibling test above — zero events either way —
/// and a 2xx Messages turn severed before `message_start` recorded
/// NOTHING: zero content delivered to the client, `tcr status` showing a
/// perfectly healthy account. That is precisely "a truncated turn booked
/// as a clean serve," the bug this whole feature exists to catch. The
/// fix keys on `saw_any_event` instead of `saw_message_start` alone: zero
/// events is indistinguishable from a Messages turn severed early, so it
/// gets the SAME verdict as the confirmed-truncated case, not the
/// not-a-Messages-turn exemption above.
#[tokio::test]
async fn sse_stream_severed_before_any_event_records_truncation() {
    let stream = futures::stream::iter(Vec::<Result<Bytes, Infallible>>::new());
    let SseParse {
        usage: parsed,
        stream_error,
        ..
    } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(parsed, ParsedUsage::default());
    assert_eq!(
        stream_error.as_deref(),
        Some(TRUNCATED_STREAM_ERROR_KIND),
        "a stream that produced ZERO parseable events before ending must \
             be treated as truncated, not silently passed — it is \
             indistinguishable from a Messages turn severed before \
             message_start ever arrived"
    );
}

/// MUST-RECORD: "a malformed/utf8 frame that ends parsing mid-turn ⇒
/// truncation, not abstention." `eventsource-stream` 0.2.3's `Utf8Stream`
/// never surfaces a UTF-8 error MID-stream (it buffers an unresolved
/// tail hoping the next chunk completes it) — the error only surfaces
/// once the underlying byte stream ends with that tail still unresolved
/// (verified by reading `utf8_stream.rs`: mid-stream `Ok` is returned
/// unconditionally; `Err` is only constructed in the `Poll::Ready(None)`
/// arm). So "malformed frame" and "stream end" are the same moment here
/// — this fixture reproduces that: a confirmed `message_start`, then one
/// byte (`0xFF`) that can never resolve into valid UTF-8 no matter what
/// follows, and nothing follows. `parse_sse_usage`'s `break` on that `Err`
/// must still leave `saw_message_start` set, so the post-loop check
/// classifies it exactly like the ordinary "no `message_stop`" case.
#[tokio::test]
async fn sse_stream_with_malformed_utf8_after_message_start_records_truncation() {
    let head = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\
             \"input_tokens\":5,\"output_tokens\":1}}}\n\n",
    );
    let chunks = vec![
        Ok::<Bytes, Infallible>(Bytes::from(head)),
        Ok::<Bytes, Infallible>(Bytes::copy_from_slice(&[0xFFu8])),
    ];
    let SseParse {
        usage: parsed,
        stream_error,
        ..
    } = parse_sse_usage(
        futures::stream::iter(chunks),
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(
        parsed.input_total, 5,
        "usage from message_start is retained even though the stream ends malformed"
    );
    assert_eq!(
        stream_error.as_deref(),
        Some(TRUNCATED_STREAM_ERROR_KIND),
        "a confirmed Messages turn that ends on an unresolvable malformed \
             byte must be treated as truncated, not silently abstained"
    );
}

/// RUNNING NOW's source: a `tool_use` block streamed across three
/// `input_json_delta` fragments is reassembled and normalized exactly as the
/// request-side parse would have done it (`cd <dir> && timeout 15 rg ...`
/// reduces to `rg -n TODO`, class `Search`), because both go through
/// `session_wire::tool_use_event_from_block`.
#[tokio::test]
async fn sse_tool_use_is_reassembled_from_its_input_json_deltas() {
    let full = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"comm\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"and\\\": \\\"cd ~/src/example && \"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"timeout 15 rg -n TODO\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
    let SseParse {
        stream_error,
        tool_uses,
        ..
    } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(stream_error, None);
    assert_eq!(tool_uses.len(), 1, "one completed tool_use block");
    assert_eq!(tool_uses[0].id, "toolu_1");
    assert_eq!(tool_uses[0].name.as_deref(), Some("Bash"));
    assert_eq!(
        tool_uses[0].command_head.as_deref(),
        Some("rg -n TODO"),
        "the three fragments reassemble into one input object and go through \
             normalize_bash_command, cd-reduction and wrapper-stripping included"
    );
    assert_eq!(
        tool_uses[0].command_class,
        Some(crate::session_wire::CommandClass::Search)
    );
}

/// A turn severed before `content_block_stop` never produced a runnable tool
/// call — the client got no complete block, so nothing is "running".
#[tokio::test]
async fn an_unfinished_tool_use_block_is_not_reported_as_running() {
    let full = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\": \\\"ls\"}}\n\n",
    );
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
    let SseParse {
        stream_error,
        tool_uses,
        ..
    } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(
        stream_error.as_deref(),
        Some(TRUNCATED_STREAM_ERROR_KIND),
        "no message_stop — the existing truncation verdict is unchanged"
    );
    assert!(tool_uses.is_empty());
}

/// A text block interleaved with a tool block must not be collected, and two
/// tool blocks in one turn both are — the index keys each event to its block.
#[tokio::test]
async fn only_tool_use_blocks_are_collected_and_each_index_keeps_its_own() {
    let full = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"thinking\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_a\",\"name\":\"Read\",\"input\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_b\",\"name\":\"TaskOutput\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"task_id\\\": \\\"task_9\\\"}\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file_path\\\": \\\"/tmp/example.rs\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
    let SseParse { tool_uses, .. } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    let heads: Vec<(&str, Option<&str>)> = tool_uses
        .iter()
        .map(|t| (t.id.as_str(), t.command_head.as_deref()))
        .collect();
    assert_eq!(
        heads,
        vec![
            ("toolu_a", Some("Read /tmp/example.rs")),
            ("toolu_b", Some("TaskOutput · waiting on task task_9")),
        ],
        "the text block contributes nothing; interleaved deltas land on their own index"
    );
}

/// A SERVER-side tool block, in the shape captured off the wire on 2026-09-22: the id is
/// carried by `content_block_start` and the block is closed by the API itself, with the
/// encrypted result arriving as a block of its own. It must be collected as a server-tool
/// id (it decides routing) and must NOT become a client tool event (nothing runs it here).
#[tokio::test]
async fn a_server_tool_use_block_is_collected_by_id_and_is_not_a_client_tool() {
    let full = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srvtoolu_fake_1\",\"name\":\"advisor\",\"input\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"advisor_tool_result\",\"tool_use_id\":\"srvtoolu_fake_1\",\"content\":{\"type\":\"advisor_redacted_result\",\"encrypted_content\":\"AAAA\"}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_fake_1\",\"name\":\"Read\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file_path\\\": \\\"/tmp/example.rs\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
    let SseParse {
        tool_uses,
        bound_tokens,
        ..
    } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(
        bound_tokens,
        vec![(BoundTokenKind::ServerToolUse, "srvtoolu_fake_1".to_string())]
    );
    assert_eq!(
        tool_uses.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        vec!["toolu_fake_1"],
        "the server-side call is Anthropic's, not a tool this client runs"
    );
}

/// A signed reasoning block, streamed the way the API sends one: the signature arrives as
/// `signature_delta` fragments that concatenate, and is only whole at `content_block_stop`.
/// A half-assembled signature would hash to something no later turn ever presents.
#[tokio::test]
async fn a_thinking_signature_is_assembled_from_its_deltas() {
    let full = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fake_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step one\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_fake_\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"1\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"data_fake_1\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
    let SseParse {
        tool_uses,
        bound_tokens,
        ..
    } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(
        bound_tokens,
        vec![
            (BoundTokenKind::MessageId, "msg_fake_1".to_string()),
            (BoundTokenKind::ThinkingSignature, "sig_fake_1".to_string()),
            (BoundTokenKind::RedactedThinking, "data_fake_1".to_string()),
        ],
        "the signature is the CONCATENATION of its deltas, not the first fragment"
    );
    assert!(
        tool_uses.is_empty(),
        "reasoning is not a tool call the client runs"
    );
}

/// A stream cut off before `content_block_stop` has still named its minting account: the
/// message id and the server-tool id are whole the moment their block opens, so the retry
/// of that very turn can be sent back to the same account. The half-signature is NOT
/// reported — it would never match anything a later turn echoes.
#[tokio::test]
async fn a_truncated_stream_reports_the_whole_tokens_and_not_a_half_signature() {
    let full = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fake_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srvtoolu_fake_1\",\"name\":\"advisor\",\"input\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_fak\"}}\n\n",
    );
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(full))]);
    let SseParse {
        stream_error,
        bound_tokens,
        ..
    } = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(stream_error.as_deref(), Some(TRUNCATED_STREAM_ERROR_KIND));
    assert_eq!(
        bound_tokens,
        vec![
            (BoundTokenKind::MessageId, "msg_fake_1".to_string()),
            (BoundTokenKind::ServerToolUse, "srvtoolu_fake_1".to_string()),
        ]
    );
}

/// A Codex Responses stream's terminal event, in the shape `openai/codex` reads it
/// (`codex-api/src/sse/responses.rs:105-160,418-440`): `input_tokens` already counts the
/// cached part, and `output_tokens` already counts reasoning.
const CODEX_COMPLETED: &[u8] = br#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-6-astra","status":"completed","usage":{"input_tokens":1200,"input_tokens_details":{"cached_tokens":1000},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":10},"total_tokens":1240}}}"#;

/// Codex's usage lands in the ledger's own shape: the input total INCLUDES cached tokens
/// (the same total Anthropic's `message_start` sum produces), the cached part is surfaced as
/// cache read, and reasoning stays inside output. The model comes off the same event,
/// because a Codex request body is usually zstd and then cannot be peeked for it.
#[test]
fn codex_completed_event_yields_usage_and_model() {
    let parsed = codex_usage_from_event(CODEX_COMPLETED).expect("a completed event has usage");
    assert_eq!(
        parsed.usage.input_total, 1200,
        "input_tokens includes cached, like message_start"
    );
    assert_eq!(parsed.usage.cache_read, 1000);
    assert_eq!(
        parsed.usage.output, 40,
        "output includes reasoning tokens; they are billed output"
    );
    assert_eq!(parsed.usage.cache_creation, 0);
    assert_eq!(
        parsed.usage.cache_creation_1h,
        Some(0),
        "nothing a Codex turn writes is Anthropic's 1h cache: the split is known, not \
             missing, so usage_record never reads a request's cache_control ttl for it"
    );
    assert_eq!(parsed.model.as_deref(), Some("gpt-6-astra"));
}

/// A turn the backend ended early (`response.incomplete`) still spent its tokens, so it
/// counts exactly like a completed one. Every other event yields nothing — a delta, the
/// opening `response.created` (which names the model but carries no usage yet), anything
/// not JSON — so the tee never overwrites a terminal event's totals.
#[test]
fn codex_incomplete_event_counts_too_and_other_events_do_not() {
    let incomplete = CODEX_COMPLETED.to_vec();
    let incomplete = String::from_utf8(incomplete)
        .unwrap()
        .replace("response.completed", "response.incomplete");
    assert!(codex_usage_from_event(incomplete.as_bytes()).is_some());
    assert!(
        codex_usage_from_event(br#"{"type":"response.output_text.delta","delta":"ok"}"#).is_none()
    );
    assert!(
        codex_usage_from_event(
            br#"{"type":"response.created","response":{"id":"r","model":"gpt-6-astra"}}"#
        )
        .is_none()
    );
    assert!(codex_usage_from_event(b"not json").is_none());
}

/// Through the tee's own parser, end to end: the SSE framing is the one every Anthropic test
/// above exercises, and only the per-event dispatch differs. What comes out is the terminal
/// event's usage and model; the events before it contribute nothing.
#[tokio::test]
async fn a_codex_sse_stream_is_tee_d_into_the_ledger_shape() {
    let stream_text = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-opening\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-astra\",\"usage\":{\"input_tokens\":1200,\"input_tokens_details\":{\"cached_tokens\":1000},\"output_tokens\":40,\"total_tokens\":1240}}}\n\n",
    );
    // The same adapter every Anthropic SSE test above feeds the parser with.
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(stream_text))]);
    let parsed = parse_sse_usage(
        stream,
        Provider::Codex,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert_eq!(parsed.usage.input_total, 1200);
    assert_eq!(parsed.usage.cache_read, 1000);
    assert_eq!(parsed.usage.output, 40);
    assert_eq!(parsed.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(
        parsed.stream_error, None,
        "a Codex stream that reached its terminal event is not truncated"
    );
}

/// [`parse_sse_usage`] over one Codex stream, delivered in a single chunk.
async fn parse_codex_stream(text: impl Into<Bytes>) -> SseParse {
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(text.into())]);
    parse_sse_usage(
        stream,
        Provider::Codex,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await
}

/// A Codex turn's reasoning tokens are read off the same usage object as the rest, under
/// `output_tokens_details.reasoning_tokens`, from a stream's closing event and from a
/// non-streamed Responses object alike. They stay inside `output`, which is what the
/// model billed. An Anthropic body has none.
#[test]
fn codex_reasoning_tokens_parse_from_the_response() {
    let streamed = codex_usage_from_event(CODEX_COMPLETED).expect("a completed event has usage");
    assert_eq!((streamed.usage.reasoning, streamed.usage.output), (10, 40));
    let body = br#"{"id":"resp_1","object":"response","status":"completed","model":"gpt-6-astra","output":[],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40},"output_tokens":10,"output_tokens_details":{"reasoning_tokens":5},"total_tokens":110}}"#;
    let json = usage_from_json(body, Provider::Codex);
    assert_eq!((json.usage.reasoning, json.usage.output), (5, 10));
    let anthropic = usage_from_json(
        br#"{"usage":{"input_tokens":7,"output_tokens":3}}"#,
        Provider::Anthropic,
    );
    assert_eq!(anthropic.usage.reasoning, 0);
}

/// A finished `function_call` item in a Codex stream is a tool call the client is about
/// to run. The tee turns it into a running call on the request's session, headed by the
/// command it runs and carrying its own timeout (`timeout_ms` in whole seconds). The call
/// is a one-shot `exec_command`, the only schema whose arguments carry a timeout, which
/// codex offers when managed requirements turn unified exec off. Only the finished item
/// counts: `response.output_item.added`, which announces the same call, is not a second
/// call, and arguments are read from the finished item only.
#[tokio::test]
async fn a_codex_function_call_starts_a_running_tool_with_its_timeout() {
    const STREAM: &str = concat!(
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec_command","arguments":""}}"#,
        "\n\n",
        "event: response.output_item.done\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"cargo build\",\"timeout_ms\":120000}"}}"#,
        "\n\n",
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.1-codex","usage":{"input_tokens":12,"output_tokens":3}}}"#,
        "\n\n",
    );
    let parsed = parse_codex_stream(STREAM).await;
    assert_eq!(
        parsed.codex_tool_calls,
        vec![crate::session_wire::CodexToolCall {
            event: crate::session_wire::ToolUseEvent {
                id: "call_1".to_string(),
                name: Some("exec_command".to_string()),
                command_head: Some("cargo build".to_string()),
                command_class: Some(crate::session_wire::CommandClass::Build),
            },
            timeout_seconds: Some(120),
        }]
    );

    // The same stream through the handler starts the call on the request's session.
    let codex_url = spawn_fixed_origin("text/event-stream", STREAM).await;
    let anthropic_echo = CapturingUpstream::spawn().await;
    let manager = Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_url), None);
    let (status, body) = drive(
        Arc::clone(&manager),
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
            ("session-id", "sess-tool"),
        ],
        r#"{"input":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    // Started by the tee's detached task, which can finish after the client reads EOF.
    let running = || -> Vec<tcr_status_wire::RunningToolRow> {
        manager
            .snapshot(OffsetDateTime::now_utc())
            .wire_sessions
            .into_iter()
            .find(|row| row.session_id == "sess-tool")
            .map_or_default(|row| row.tools.running)
    };
    let mut polls = 0u32;
    while running().is_empty() && polls < 200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        polls += 1;
    }
    let running = running();
    assert_eq!(running.len(), 1, "polls: {polls}");
    assert_eq!(
        (
            running[0].tool.as_str(),
            running[0].command_head.as_deref(),
            running[0].command_class.as_deref(),
            running[0].timeout_seconds
        ),
        (
            "exec_command",
            Some("cargo build"),
            Some("build"),
            Some(120)
        )
    );
}

/// A local shell call and an `exec_command` both run a command line, so both are headed
/// and classified by the normalizer a Claude `Bash` call goes through: `git push` reads as
/// a network round trip whichever client ran it. The argv a local shell call wraps its
/// script in (`bash -lc <script>`) is unwrapped first; codex parses that item, though it
/// never offers the tool. The default interactive `exec_command` names no timeout: its
/// `yield_time_ms` is a yield interval, so that call carries none. An output item that is
/// no tool call is skipped.
#[tokio::test]
async fn a_codex_local_shell_call_is_a_shell_command() {
    let parsed = parse_codex_stream(concat!(
            "event: response.output_item.done\n",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"local_shell_call","id":"lsh_1","call_id":"call_2","status":"completed","action":{"type":"exec","command":["bash","-lc","git push origin main"],"timeout_ms":30000}}}"#,
            "\n\n",
            "event: response.output_item.done\n",
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_3","call_id":"call_3","name":"exec_command","arguments":"{\"cmd\":\"cargo test -p example\",\"yield_time_ms\":10000}"}}"#,
            "\n\n",
            "event: response.output_item.done\n",
            r#"data: {"type":"response.output_item.done","output_index":2,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}"#,
            "\n\n",
            "event: response.completed\n",
            r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.1-codex","usage":{"input_tokens":12,"output_tokens":3}}}"#,
            "\n\n",
        ))
        .await;
    use crate::session_wire::CommandClass;
    // (call id, tool, head, class, timeout)
    type Call<'a> = (
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<CommandClass>,
        Option<u64>,
    );
    let calls: Vec<Call> = parsed
        .codex_tool_calls
        .iter()
        .map(|c| {
            (
                c.event.id.as_str(),
                c.event.name.as_deref(),
                c.event.command_head.as_deref(),
                c.event.command_class,
                c.timeout_seconds,
            )
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            (
                "call_2",
                Some("local_shell_call"),
                Some("git push origin main"),
                Some(CommandClass::GitNet),
                Some(30)
            ),
            (
                "call_3",
                Some("exec_command"),
                Some("cargo test -p example"),
                Some(CommandClass::Build),
                None
            ),
        ]
    );
}

/// The Codex item read lives in the Codex branch alone. An Anthropic stream's `tool_use`
/// blocks come out exactly as before, with no Codex call beside them, and a Codex-shaped
/// event on an Anthropic stream is no call of either kind.
#[tokio::test]
async fn anthropic_tool_parsing_is_unchanged() {
    let stream_text = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":7,"output_tokens":1}}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"cargo build\"}"}}"#,
        "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        "event: response.output_item.done\n",
        r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let stream = futures::stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(stream_text))]);
    let parsed = parse_sse_usage(
        stream,
        Provider::Anthropic,
        crate::provider::CODEX_RESPONSES_PATH,
    )
    .await;
    assert!(
        parsed.codex_tool_calls.is_empty(),
        "an Anthropic stream carries no Codex call"
    );
    let heads: Vec<(&str, Option<&str>)> = parsed
        .tool_uses
        .iter()
        .map(|t| (t.id.as_str(), t.command_head.as_deref()))
        .collect();
    assert_eq!(heads, vec![("toolu_1", Some("cargo build"))]);
}

/// A Codex request's session is the one its `session-id` header names, trimmed: the
/// header session affinity pins on. A Codex body is usually zstd, so the header is the
/// one place a Codex turn says which conversation it belongs to. The row is a Codex one,
/// attributed to the account that served it. On an Anthropic request the header names
/// nothing, because its session comes from `metadata.user_id` as before, and a blank
/// header names no session at all.
#[tokio::test]
async fn the_codex_session_id_header_names_the_wire_session() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let codex = |sid: &'static str| {
        [
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
            ("session-id", sid),
        ]
    };
    for (path, headers, body) in [
        (
            crate::provider::CODEX_RESPONSES_PATH,
            codex("  s-1  "),
            r#"{"input":[]}"#,
        ),
        (
            crate::provider::CODEX_RESPONSES_PATH,
            codex("   "),
            r#"{"input":[]}"#,
        ),
        (
            "/v1/messages",
            [
                ("content-type", "application/json"),
                ("anthropic-version", "2023-06-01"),
                ("session-id", "s-2"),
            ],
            r#"{"model":"claude-opus-5","messages":[]}"#,
        ),
    ] {
        let (status, bytes) = drive(
            Arc::clone(&manager),
            Method::POST,
            path,
            Some(loopback_peer()),
            &headers,
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    // Recorded at the terminal outcome, before the response is built: nothing to wait for.
    let rows = manager.snapshot(OffsetDateTime::now_utc()).wire_sessions;
    let ids: Vec<&str> = rows.iter().map(|row| row.session_id.as_str()).collect();
    assert_eq!(
        ids,
        ["s-1"],
        "the blank header and the Anthropic request name no session"
    );
    assert_eq!(rows[0].provider.as_deref(), Some("codex"));
    assert_eq!(rows[0].account.as_deref(), Some("bob@example.com/codex"));
    assert_eq!(rows[0].requests, 1);
}

/// A Codex request tcr answers itself is no request on its session. tcr answers a request
/// itself when every upstream answer was a 403, and again when a request meets the hold
/// that 403 arms (`REFUSED_HOLD_SECONDS`) while no other Codex account can serve, unless
/// the hold is about to end and the request waits it out instead. Such a request never
/// reaches the terminal outcome: it is not counted on its session and closes none of the
/// calls the session's last response started, as a Claude `tool_result` in such a request
/// closes nothing. The calls close on the next request tcr serves.
#[tokio::test]
async fn a_codex_request_tcr_answers_itself_closes_no_call() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let auth_body = r#"{"error":{"type":"authentication_error","code":"invalid_api_key"}}"#;
    let auth_response = stub::reply("403 Forbidden", "application/json", auth_body);
    let (codex_origin, attempts) = spawn_counted_upstream(vec![Some(auth_response)]).await;
    let manager = Manager::with_live_refresher(
        codex_fleet(&anthropic_echo.url, &format!("http://{codex_origin}")),
        None,
    );
    let started = OffsetDateTime::now_utc();
    manager.record_codex_wire_session(
        Some("sess-held"),
        Some("bob@example.com/codex".to_string()),
        started,
    );
    manager.record_codex_wire_session_calls(
        Some("sess-held"),
        started,
        &[crate::session_wire::CodexToolCall {
            event: crate::session_wire::ToolUseEvent {
                id: "call_1".to_string(),
                name: Some("exec_command".to_string()),
                command_head: Some("cargo test".to_string()),
                command_class: Some(crate::session_wire::CommandClass::Build),
            },
            timeout_seconds: None,
        }],
    );
    // The first meets the upstream 403, which holds bob; the second meets the hold.
    for attempt in ["the upstream 403", "the hold it arms"] {
        let (status, body) = drive(
            Arc::clone(&manager),
            Method::POST,
            crate::provider::CODEX_RESPONSES_PATH,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("content-type", "application/json"),
                ("session-id", "sess-held"),
            ],
            r#"{"input":[]}"#,
        )
        .await;
        assert_ne!(
            status,
            StatusCode::OK,
            "{attempt}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "the hold answered the second request: it never reached the origin"
    );
    let row = manager
        .snapshot(OffsetDateTime::now_utc())
        .wire_sessions
        .into_iter()
        .find(|row| row.session_id == "sess-held")
        .expect("the session the test seeded");
    assert_eq!(
        row.requests, 1,
        "a request tcr answers itself is not counted"
    );
    assert_eq!(
        (row.tools.calls, row.tools.running.len()),
        (0, 1),
        "call_1 still runs: nothing tcr answered itself closed it"
    );
}

/// **The terminal-type gate.** A `response.failed` event is no usage, whatever it carries:
/// this one is [`CODEX_COMPLETED`] with nothing changed but its type, usage block and all,
/// so only the gate in [`codex_terminal_usage`] keeps it out of the ledger.
#[test]
fn a_failed_codex_event_yields_no_usage_even_with_a_usage_block() {
    let failed = String::from_utf8(CODEX_COMPLETED.to_vec())
        .unwrap()
        .replace("response.completed", "response.failed");
    assert!(codex_usage_from_event(failed.as_bytes()).is_none());
}

/// Upstream's own fixture (`parses_cache_write_token_usage`): the tokens written to the
/// cache (`cache_write_tokens`) are cache CREATION and the ones read from it
/// (`cached_tokens`) cache READ, both inside `input_tokens`. So the ledger record's base
/// input is what is left of the 100, zero, with nothing floored, and a request asking for
/// Anthropic's 1h cache cannot move a Codex write there.
#[test]
fn codex_cache_writes_are_cache_creation_and_cache_hits_are_cache_reads() {
    let event = br#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-6-astra","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40,"cache_write_tokens":60},"output_tokens":10,"output_tokens_details":{"reasoning_tokens":5},"total_tokens":110}}}"#;
    let parsed = codex_usage_from_event(event)
        .expect("a completed event has usage")
        .usage;
    assert_eq!(
        (
            parsed.input_total,
            parsed.cache_read,
            parsed.cache_creation,
            parsed.output
        ),
        (100, 40, 60, 10),
        "(input total, cache read, cache creation, output)"
    );
    let record = usage_record(&parsed, Some("gpt-6-astra".to_string()), None, || true);
    assert_eq!(
        (
            record.input,
            record.cache_5m,
            record.cache_1h,
            record.cache_read
        ),
        (0, 60, 0, 40),
        "(base input, 5m write, 1h write, cache read)"
    );
}

/// A terminal event whose `usage` is null reports no usage at all, rather than a turn that
/// spent zero: [`parse_sse_usage`] then keeps the totals it had.
#[test]
fn a_codex_terminal_event_with_a_null_usage_yields_none() {
    assert!(
        codex_usage_from_event(
            br#"{"type":"response.completed","response":{"id":"resp_1","usage":null}}"#
        )
        .is_none()
    );
}

/// **A failed Codex turn is a stream error, recorded under its code**, as a failed
/// Anthropic turn is under its `error.type`. `response.failed` names the failure in
/// `response.error.code`, next to a null `usage` (upstream's own fixture,
/// `codex-api/src/sse/responses.rs:1047`); an `error` event names it in a nested `error`
/// object. The first one wins: a `response.failed` after an `error` event still ends the
/// turn, but does not rename its failure.
#[tokio::test]
async fn a_failed_codex_turn_is_a_stream_error_named_by_its_code() {
    let failed = parse_codex_stream(concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-astra\"}}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"status\":\"failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\"},\"usage\":null}}\n\n",
        ))
        .await;
    assert_eq!(failed.stream_error.as_deref(), Some("rate_limit_exceeded"));
    assert_eq!(
        failed.usage,
        ParsedUsage::default(),
        "a failed turn reports no usage"
    );

    let error_first = parse_codex_stream(concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"code\":\"flex_unavailable\",\"message\":\"Flex is unavailable\"}}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"status\":\"failed\",\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"Overloaded\"},\"usage\":null}}\n\n",
        ))
        .await;
    assert_eq!(
        error_first.stream_error.as_deref(),
        Some("flex_unavailable"),
        "the first failure names the turn's"
    );
}

/// **A Codex stream that ends before its terminal event is truncated**, exactly as an
/// Anthropic one that ends before `message_stop` is: events arrived, and the turn never
/// closed. A `response.incomplete` closes it (its spent tokens still count), and so does a
/// `response.completed` that reports no usage.
#[tokio::test]
async fn a_codex_stream_cut_off_before_its_terminal_event_is_truncated() {
    const OPENING: &str = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-astra\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
    );
    let cut = parse_codex_stream(OPENING).await;
    assert_eq!(
        cut.stream_error.as_deref(),
        Some(TRUNCATED_STREAM_ERROR_KIND)
    );

    let incomplete = parse_codex_stream(format!(
            "{OPENING}event: response.incomplete\ndata: {}\n\n",
            r#"{"type":"response.incomplete","response":{"id":"resp_1","model":"gpt-6-astra","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1200,"output_tokens":40}}}"#
        ))
        .await;
    assert_eq!(incomplete.stream_error, None);
    assert_eq!(
        incomplete.usage.input_total, 1200,
        "an incomplete turn's tokens were spent"
    );

    let completed_without_usage = parse_codex_stream(format!(
        "{OPENING}event: response.completed\ndata: {}\n\n",
        r#"{"type":"response.completed","response":{"id":"resp_1","usage":null}}"#
    ))
    .await;
    assert_eq!(completed_without_usage.stream_error, None);
}

#[tokio::test]
async fn codex_auxiliary_sse_does_not_require_a_responses_terminator() {
    for path in [
        "/backend-api/codex/images/generations",
        "/backend-api/codex/images/edits",
        "/backend-api/codex/memories/trace_summarize",
        "/backend-api/codex/alpha/search",
    ] {
        for body in ["", "event: done\ndata: {\"type\":\"done\"}\n\n"] {
            let stream = futures::stream::iter(vec![Ok::<_, Infallible>(body)]);
            let parsed = parse_sse_usage(stream, Provider::Codex, path).await;
            assert_eq!(parsed.stream_error, None, "{path}: {body:?}");
        }
        let body =
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"server_error\"}}\n\n";
        let stream = futures::stream::iter(vec![Ok::<_, Infallible>(body)]);
        let parsed = parse_sse_usage(stream, Provider::Codex, path).await;
        assert_eq!(
            parsed.stream_error.as_deref(),
            Some("server_error"),
            "{path}"
        );
    }
}

/// **A body's first bytes say whether it is SSE**, across a field name split between
/// chunks, and every chunk read to decide is handed back in order.
#[tokio::test]
async fn sniff_sse_reads_the_first_non_blank_line() {
    assert_eq!(sse_verdict(b"event: response.created\n"), Some(true));
    assert_eq!(sse_verdict(b"\n\ndata: {}\n"), Some(true));
    assert_eq!(sse_verdict(b": keep-alive\n"), Some(true));
    assert_eq!(sse_verdict(b"{\"usage\":{}}"), Some(false));
    assert_eq!(sse_verdict(b"  [1]"), Some(false));
    assert_eq!(sse_verdict(b"eve"), None, "half a field name");
    assert_eq!(sse_verdict(b" \n"), None, "nothing but blanks yet");

    let chunks = ["ev", "ent: response.created\n", "data: {}\n\n", "tail"];
    let mut body = futures::stream::iter(
        chunks.map(|c| Ok::<_, Infallible>(Bytes::from_static(c.as_bytes()))),
    );
    let (is_sse, read) = sniff_sse(&mut body).await;
    assert!(is_sse);
    let read: Vec<Bytes> = read.into_iter().map(Result::unwrap).collect();
    assert_eq!(read, [&b"ev"[..], &b"ent: response.created\n"[..]]);
    let rest: Vec<Bytes> = body.map(Result::unwrap).collect().await;
    assert_eq!(rest, [&b"data: {}\n\n"[..], &b"tail"[..]]);

    let mut json = futures::stream::iter([Ok::<_, Infallible>(Bytes::from_static(b"{}"))]);
    assert!(!sniff_sse(&mut json).await.0);
    let mut empty = futures::stream::iter(Vec::<Result<Bytes, Infallible>>::new());
    assert_eq!(sniff_sse(&mut empty).await, (false, Vec::new()));
}

// The three non-streamed readers as they shipped before the one-parse change, each parsing
// the body on its own, kept verbatim as the reference `parse_non_streamed_response` must
// agree with.
fn reference_usage_from_json(bytes: &[u8]) -> ParsedUsage {
    let value = match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(
                error = %err,
                body_len = bytes.len(),
                "a non-streamed 200 body did not parse as JSON, so this turn's tokens are \
                 NOT in the usage ledger; totals for this window are an undercount"
            );
            return ParsedUsage::default();
        }
    };
    let Some(usage) = value.get("usage") else {
        // Ordinary, and deliberately silent: plenty of 200 bodies carry no usage.
        return ParsedUsage::default();
    };
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let (cache_read, cache_creation, cache_creation_1h) = cache_breakdown(usage);
    ParsedUsage {
        input_total: sum_input_tokens(usage),
        output,
        cache_read,
        cache_creation,
        cache_creation_1h,
        reasoning: 0,
    }
}

fn reference_response_tool_uses_from_json(bytes: &[u8]) -> Vec<crate::session_wire::ToolUseEvent> {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return Vec::new();
    };
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| {
            b.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t == "tool_use")
        })
        .take(crate::session_wire::PENDING_TOOL_CAP)
        .filter_map(|b| {
            let id = b.get("id").and_then(Value::as_str)?.to_string();
            Some(crate::session_wire::tool_use_event_from_block(
                id,
                b.get("name").and_then(Value::as_str).map(str::to_string),
                b.get("input"),
            ))
        })
        .collect()
}

fn reference_response_bound_tokens_from_json(bytes: &[u8]) -> Vec<MintedToken> {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return Vec::new();
    };
    let mut tokens: Vec<MintedToken> = Vec::new();
    if let Some(id) = value.get("id").and_then(Value::as_str) {
        tokens.push((BoundTokenKind::MessageId, id.to_string()));
    }
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return tokens;
    };
    for block in blocks {
        let field = |name: &str| block.get(name).and_then(Value::as_str).map(str::to_string);
        let minted = match block.get("type").and_then(Value::as_str) {
            Some(SERVER_TOOL_USE_BLOCK_TYPE) => {
                field("id").map(|t| (BoundTokenKind::ServerToolUse, t))
            }
            Some(THINKING_BLOCK_TYPE) => {
                field("signature").map(|t| (BoundTokenKind::ThinkingSignature, t))
            }
            Some(REDACTED_THINKING_BLOCK_TYPE) => {
                field("data").map(|t| (BoundTokenKind::RedactedThinking, t))
            }
            _ => None,
        };
        if let Some(minted) = minted {
            tokens.push(minted);
        }
        if tokens.len() >= crate::session_wire::BOUND_TOKEN_SCAN_CAP {
            break;
        }
    }
    tokens
}

/// One parse of a non-streamed body reads the same usage, tool uses and bound tokens as
/// the three separate parses it replaced, on bodies with and without each part.
#[test]
fn one_parse_of_a_non_streamed_body_matches_the_three_it_replaced() {
    let fixtures: &[&str] = &[
        // Text only, full usage with the 5m/1h split.
        r#"{"id":"msg_fake_1","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":7,"cache_creation_input_tokens":30,"cache_read_input_tokens":90,"output_tokens":4,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}}}"#,
        // tool_use blocks, a thinking block, a redacted one, a server tool.
        r#"{"id":"msg_fake_2","type":"message","content":[{"type":"thinking","thinking":"plan","signature":"sig_fake_2"},{"type":"redacted_thinking","data":"data_fake_2"},{"type":"server_tool_use","id":"srvtoolu_fake_2","name":"web_search","input":{"query":"q"}},{"type":"tool_use","id":"toolu_fake_2","name":"Bash","input":{"command":"ls -la"}},{"type":"tool_use","id":"toolu_fake_3","name":"Read","input":{"file_path":"/tmp/x"}}],"usage":{"input_tokens":5,"output_tokens":2}}"#,
        // No usage at all, and no id.
        r#"{"type":"message","content":[{"type":"tool_use","id":"toolu_fake_4","name":"Grep","input":{"pattern":"fn"}}]}"#,
        // usage present, content missing; content not an array.
        r#"{"id":"msg_fake_5","usage":{"input_tokens":1,"output_tokens":1}}"#,
        r#"{"id":"msg_fake_6","content":"text","usage":{"output_tokens":3}}"#,
        // A tool_use with no id, a thinking block with no signature.
        r#"{"id":"msg_fake_7","content":[{"type":"tool_use","name":"Grep"},{"type":"thinking","thinking":"x"}],"usage":{"input_tokens":2}}"#,
        // Not an object, and not JSON.
        "[1,2,3]",
        "not json at all",
        "",
    ];
    for fixture in fixtures {
        let bytes = fixture.as_bytes();
        let response = parse_non_streamed_response(bytes, Provider::Anthropic);
        assert_eq!(
            response.usage,
            reference_usage_from_json(bytes),
            "usage differs for {fixture}"
        );
        assert_eq!(
            response.tool_uses,
            reference_response_tool_uses_from_json(bytes),
            "tool uses differ for {fixture}"
        );
        assert_eq!(
            response.bound_tokens,
            reference_response_bound_tokens_from_json(bytes),
            "bound tokens differ for {fixture}"
        );
    }
    // A positive control: the fixtures do carry every kind of result, so the equality
    // above is not three empty lists agreeing.
    let rich = parse_non_streamed_response(fixtures[1].as_bytes(), Provider::Anthropic);
    assert_eq!(rich.tool_uses.len(), 2);
    assert_eq!(rich.bound_tokens.len(), 4);
    assert_eq!(
        parse_non_streamed_response(fixtures[0].as_bytes(), Provider::Anthropic)
            .usage
            .cache_creation_1h,
        Some(20)
    );
}

/// The non-streamed twin of the token scan, including the body's own message id.
#[test]
fn non_streamed_bound_tokens_come_off_the_body() {
    let body = br#"{
            "id": "msg_fake_1",
            "type": "message",
            "content": [
                {"type": "text", "text": "on it"},
                {"type": "thinking", "thinking": "step one", "signature": "sig_fake_1"},
                {"type": "redacted_thinking", "data": "data_fake_1"},
                {"type": "server_tool_use", "id": "srvtoolu_fake_1", "name": "advisor", "input": {}},
                {"type": "advisor_tool_result", "tool_use_id": "srvtoolu_fake_1",
                 "content": {"type": "advisor_redacted_result", "encrypted_content": "AAAA"}},
                {"type": "tool_use", "id": "toolu_fake_1", "name": "Grep", "input": {"pattern": "fn main"}}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 2}
        }"#;
    assert_eq!(
        response_bound_tokens_from_json(body),
        vec![
            (BoundTokenKind::MessageId, "msg_fake_1".to_string()),
            (BoundTokenKind::ThinkingSignature, "sig_fake_1".to_string()),
            (BoundTokenKind::RedactedThinking, "data_fake_1".to_string()),
            (BoundTokenKind::ServerToolUse, "srvtoolu_fake_1".to_string()),
        ],
        "the client-side tool_use is not bound state and must not appear"
    );
    assert!(response_bound_tokens_from_json(b"not json at all").is_empty());
}

/// The non-streamed twin: `tool_use` blocks come off the response body's
/// `content` array, through the same event builder.
#[test]
fn non_streamed_response_tool_uses_come_off_the_content_array() {
    let body = br#"{
            "id": "msg_1",
            "type": "message",
            "content": [
                {"type": "text", "text": "on it"},
                {"type": "tool_use", "id": "toolu_1", "name": "Grep", "input": {"pattern": "fn main"}}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 2}
        }"#;
    let uses = response_tool_uses_from_json(body);
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].id, "toolu_1");
    assert_eq!(uses[0].command_head.as_deref(), Some("Grep fn main"));
    assert!(
        response_tool_uses_from_json(b"not json at all").is_empty(),
        "a body that does not parse yields no events, never a panic"
    );
}

/// The other half of the malformed-frame property: `evidence_dropped`
/// must latch on `Full` (a real dropped chunk — genuine evidence loss)
/// and NOT on `Closed` (the consumer went away — never evidence about
/// upstream data). See [`is_genuine_evidence_loss`]'s doc comment for why
/// that distinction holds independent of the malformed-frame path
/// specifically. Exercised directly against the extracted predicate
/// rather than a network-level reproduction of the race, which the
/// malformed-frame path alone cannot trigger.
/// A malformed non-streamed body must SAY so, and an ordinary body without a
/// `usage` key must stay quiet.
///
/// Before this test both cases returned a default [`ParsedUsage`], the caller
/// skips recording when the totals are zero, and nothing anywhere named the
/// difference: a body the proxy could not parse silently cost the ledger a
/// turn. Asserting the log rather than the return value is deliberate, because
/// the return value is identical in both cases and that is exactly the point.
#[test]
fn a_malformed_non_streamed_body_is_logged_and_a_usage_less_one_is_not() {
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<(tracing::Level, String)>>>);
    struct Msg(Option<String>);
    impl Visit for Msg {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut m = Msg(None);
            event.record(&mut m);
            if let Some(msg) = m.0 {
                // The LEVEL is captured too. Asserting on the message text alone
                // let a mutant survive: downgrading this very warning to `trace!`
                // kept the word and the test stayed green (measured 2026-09-12,
                // before this line existed).
                self.0
                    .lock()
                    .expect("capture lock")
                    .push((*event.metadata().level(), msg));
            }
        }
    }

    use tracing_subscriber::layer::SubscriberExt;
    let cap = Capture::default();
    let subscriber = tracing_subscriber::registry().with(cap.clone());

    let parsed_bad = tracing::subscriber::with_default(subscriber, || {
        usage_from_json(b"this is not json at all", Provider::Anthropic).usage
    });
    assert_eq!(
        parsed_bad,
        ParsedUsage::default(),
        "a malformed body still yields no usage — the return value is unchanged"
    );
    let lines = cap.0.lock().expect("capture lock").clone();
    let undercounts: Vec<_> = lines
        .iter()
        .filter(|(level, msg)| *level == tracing::Level::WARN && msg.contains("undercount"))
        .collect();
    assert_eq!(
        undercounts.len(),
        1,
        "exactly one WARN should name the undercount, got {lines:?}"
    );

    let cap2 = Capture::default();
    let subscriber2 = tracing_subscriber::registry().with(cap2.clone());
    let parsed_no_usage = tracing::subscriber::with_default(subscriber2, || {
        usage_from_json(br#"{"id":"msg_1","type":"message"}"#, Provider::Anthropic).usage
    });
    assert_eq!(
        parsed_no_usage,
        ParsedUsage::default(),
        "a body without a usage key also yields no usage"
    );
    assert!(
        cap2.0.lock().expect("capture lock").is_empty(),
        "an ordinary body with no usage key must not warn: {:?}",
        cap2.0.lock().expect("capture lock")
    );
}

#[test]
fn evidence_loss_predicate_distinguishes_full_from_closed() {
    use tokio::sync::mpsc::error::TrySendError;
    assert!(
        is_genuine_evidence_loss(&TrySendError::Full(())),
        "a full channel is genuine evidence loss — the parser never saw those bytes"
    );
    assert!(
        !is_genuine_evidence_loss(&TrySendError::Closed(())),
        "a closed receiver is the parser's own exit, not evidence loss"
    );
}

/// Non-stream JSON path also sums the cache tokens into `input`.
#[test]
fn json_usage_sums_cache_tokens() {
    let body = br#"{"usage":{"input_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":90,"output_tokens":11}}"#;
    let parsed = usage_from_json(body, Provider::Anthropic).usage;
    // R1: 7 + 3 + 90 = 100, the quota sum, unchanged.
    assert_eq!(parsed.input_total, 100);
    assert_eq!(parsed.output, 11);
    // R2: the JSON path surfaces the SAME cache fields the SSE path does.
    assert_eq!(
        parsed.cache_read, 90,
        "cache-read retained on the JSON path"
    );
    assert_eq!(parsed.cache_creation, 3, "cache-creation retained");
    assert_eq!(
        parsed.cache_creation_1h, None,
        "this response reported no 1h split at all — absent, not zero"
    );
}

/// FINDING 2. A reported `ephemeral_1h_input_tokens: 0` is a first-hand
/// statement that nothing was written at the 1-hour TTL, and must never be
/// read as "no breakdown" and handed to the request's `cache_control` flag.
///
/// This is the shape every Claude Code session here produces: the system
/// prompt is pinned at 1h, the conversation at 5m, so a turn that writes
/// 200k tokens of 5-minute cache and nothing at 1h answers exactly this
/// while `requests_extended_ttl` is true. Booking those 200k at 1h prices
/// them at 2x base input instead of 1.25x — on Opus 5, $10/MTok against
/// $6.25/MTok — and both `costUsd` and `cacheCreation1hTokens` read high
/// while looking measured.
#[test]
fn an_explicitly_zero_1h_split_is_not_mistaken_for_a_missing_one() {
    let body = br#"{"usage":{"input_tokens":50,"cache_creation_input_tokens":200000,
            "cache_creation":{"ephemeral_5m_input_tokens":200000,"ephemeral_1h_input_tokens":0},
            "cache_read_input_tokens":0,"output_tokens":11}}"#;
    let parsed = usage_from_json(body, Provider::Anthropic).usage;
    assert_eq!(
        parsed.cache_creation_1h,
        Some(0),
        "reported zero survives as a REPORTED zero"
    );

    let record = usage_record(&parsed, Some("claude-opus-5".to_string()), None, || true);
    assert_eq!(
        record.cache_1h, 0,
        "the response said nothing was written at 1h, and the request's ttl flag \
             does not overrule a first-hand report"
    );
    assert_eq!(
        record.cache_5m, 200_000,
        "the whole write is a 5-minute write, priced at 1.25x rather than 2x"
    );

    // The genuinely absent case still falls back, which is the behaviour
    // this must not break: no nested object, so the request's ttl decides.
    let no_split = br#"{"usage":{"input_tokens":50,"cache_creation_input_tokens":200000,
            "cache_read_input_tokens":0,"output_tokens":11}}"#;
    let parsed = usage_from_json(no_split, Provider::Anthropic).usage;
    assert_eq!(parsed.cache_creation_1h, None, "control: nothing reported");
    let record = usage_record(&parsed, Some("claude-opus-5".to_string()), None, || true);
    assert_eq!(
        record.cache_1h, 200_000,
        "with no report at all, the request's own 1h cache_control is the only evidence"
    );
}

/// FINDING 4 (round 2). The `cache_creation` OBJECT is the report. A
/// response that sends it with only `ephemeral_5m_input_tokens` has stated
/// where its cache creation went; reading the missing 1h key as "no split
/// reported" handed the whole flat count to the request's `cache_control`
/// flag and priced a 5-minute write at 2x base input instead of 1.25x.
#[test]
fn a_cache_creation_object_with_no_1h_key_reports_zero_not_silence() {
    let five_minute_only = br#"{"usage":{"input_tokens":50,"cache_creation_input_tokens":200000,
            "cache_creation":{"ephemeral_5m_input_tokens":200000},
            "cache_read_input_tokens":0,"output_tokens":11}}"#;
    let parsed = usage_from_json(five_minute_only, Provider::Anthropic).usage;
    assert_eq!(
        parsed.cache_creation_1h,
        Some(0),
        "the object is present, so its silence on 1h is a reported zero"
    );
    let record = usage_record(&parsed, Some("claude-opus-5".to_string()), None, || true);
    assert_eq!(
        record.cache_1h, 0,
        "a request pinned at 1h does not overrule the response's own report"
    );
    assert_eq!(record.cache_5m, 200_000, "priced at 1.25x, not 2x");

    // The other shape: no object at all is genuine silence, and the
    // request's own ttl is then the only evidence there is.
    let no_object = br#"{"usage":{"input_tokens":50,"cache_creation_input_tokens":200000,
            "cache_read_input_tokens":0,"output_tokens":11}}"#;
    let parsed = usage_from_json(no_object, Provider::Anthropic).usage;
    assert_eq!(parsed.cache_creation_1h, None, "absent is absent");
    let record = usage_record(&parsed, Some("claude-opus-5".to_string()), None, || true);
    assert_eq!(record.cache_1h, 200_000);
}

/// FINDING 6 (round 2). With nothing written to the cache, both arms of the
/// split produce 0 — so asking the request's `cache_control` question
/// cannot change the record, and asking it costs a full body deserialize
/// inline on the non-streamed request path.
#[test]
fn the_ttl_closure_is_not_called_when_the_response_cached_nothing() {
    let mut asked = 0u32;
    let nothing_cached = ParsedUsage {
        input_total: 1_000,
        output: 10,
        cache_read: 500,
        cache_creation: 0,
        cache_creation_1h: None,
        reasoning: 0,
    };
    let record = usage_record(&nothing_cached, None, None, || {
        asked += 1;
        true
    });
    assert_eq!(asked, 0, "an uncached turn must not parse its own body");
    assert_eq!((record.cache_1h, record.cache_5m), (0, 0));

    // And it IS asked when the answer can still change the record.
    let mut asked = 0u32;
    let cached = ParsedUsage {
        cache_creation: 900,
        ..nothing_cached
    };
    let record = usage_record(&cached, None, None, || {
        asked += 1;
        true
    });
    assert_eq!(asked, 1, "control: the fallback still runs when it matters");
    assert_eq!(record.cache_1h, 900);
}

/// FINDING 10. The request side of the split is evidence of what was ASKED
/// for, not what was written. On the path that matters, a response that
/// reported its own 1h split, it must never be consulted. The closure PANICS
/// here, so a fallback that runs eagerly again says so.
#[test]
fn the_request_body_is_not_reparsed_when_the_response_reported_its_split() {
    let parsed = ParsedUsage {
        input_total: 1_000,
        output: 10,
        cache_read: 0,
        cache_creation: 900,
        cache_creation_1h: Some(400),
        reasoning: 0,
    };
    let record = usage_record(&parsed, Some("claude-opus-5".to_string()), None, || {
        panic!("the request body must not be parsed when the response reported its split")
    });
    assert_eq!(record.cache_1h, 400);
    assert_eq!(record.cache_5m, 500);
}
