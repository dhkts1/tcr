//! The per-request usage and timing log lines, and the stream errors recorded beside them.

use super::*;

/// Bounded-poll the manager's decayed stream-error count for account 0 until
/// it matches `want`, or give up after ~2s. The tee task that records it runs
/// in a DETACHED `tokio::spawn`, so the client can observe body EOF before
/// that task has drained its channel and finished the parse loop — reading to
/// EOF then asserting immediately is racy. Returns the last-observed count and
/// how many times it polled, so a caller's failure message states what it saw
/// rather than reading as a hang or a bare timing artifact.
async fn poll_stream_error_count(manager: &Manager, want: usize) -> (usize, u32) {
    let mut polls = 0u32;
    let mut seen = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].stream_error_count;
    while seen != want && polls < 200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        seen = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].stream_error_count;
        polls += 1;
    }
    (seen, polls)
}

/// THE BUG: an Anthropic error envelope delivered inside a 200
/// `text/event-stream` body — the same shape this proxy itself synthesizes,
/// and the shape Anthropic's real upstream uses for a mid-stream failure —
/// must be OBSERVED, not silently booked as a clean serve. Before the fix,
/// `parse_sse_usage`'s match had no `error` arm, so the event fell into
/// `_ => {}` and nothing was recorded.
#[tokio::test]
async fn sse_error_event_is_observed_not_counted_as_clean_serve() {
    let body = concat!(
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    let up_addr = stub::canned(stub::reply("200 OK", "text/event-stream", body));

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

    let proxy = app(Arc::clone(&manager));

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    // Read the body to completion FIRST — the client can see EOF before the
    // detached tee task has finished parsing (see `poll_stream_error_count`).
    let _ = body_text(resp).await;

    let (seen, polls) = poll_stream_error_count(&manager, 1).await;
    assert_eq!(
        seen, 1,
        "polled {polls} times, stream-error count stayed {seen} — an in-band \
             SSE error event must be observed, not silently dropped as a clean serve"
    );

    // The terminal-outcome counter (`record_served`) is UNCHANGED by this —
    // it fires on the upstream's 200 status same as any clean serve, and this
    // fix is deliberately observability-only: it adds a SECOND signal, it
    // does not replace or gate the first.
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
        1,
        "the request-served counter is untouched by this fix"
    );
}

// --- per-request usage log line -----------------------------------------

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("shared buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// THE BITING TEST for the per-request usage log line: a real request
/// through the buffered (non-streaming) path, against a real fake
/// upstream returning real usage numbers, must produce a `tracing::info!`
/// line carrying those exact numbers — not zeros, not a per-chunk flood.
/// `#[tokio::test]` defaults to the current-thread flavor, so the
/// thread-local subscriber set below stays in effect across every await
/// point in the request, including the buffered-arm log call in `handle`.
#[tokio::test]
async fn buffered_request_logs_usage_line_with_real_token_counts() {
    let body = r#"{"usage":{"input_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":90,"output_tokens":11}}"#;
    let up_addr = stub::canned(stub::reply("200 OK", "application/json", body));

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

    let proxy = app(Arc::clone(&manager));

    let sink = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    assert_eq!(resp.status().as_u16(), 200);
    let _ = body_text(resp).await;

    let log = sink.contents();
    assert!(
        log.contains("cache_read_tokens=90")
                && log.contains("cache_creation_tokens=3")
                // `input_tokens` on the wire is 7, but `usage_from_json`'s
                // `input_total` is base + both cache components (7+3+90=100,
                // see `sum_input_tokens`) — the same total the streaming arm
                // and `status_endpoint_returns_live_counters` use.
                && log.contains("input_tokens=100")
                && log.contains("output_tokens=11"),
        "expected a usage log line carrying the real synthesized token \
             counts (cache_read=90, cache_creation=3, input_total=100, output=11), \
             got: {log:?}"
    );
}

/// THE BITING TEST for the upstream-error blind spot this bridge closes:
/// before this fix, `src/proxy.rs`'s only per-response log line was
/// "upstream response" (`account`, `path`, `status` — nothing else), so a
/// non-2xx status was opaque: 64 `status=400` lines on 2026-09-07 with no
/// way to tell one 400 from another. A real request against a real fake
/// upstream returning 400 with Anthropic's error envelope must now produce
/// a log line carrying the body's `error.type` and `error.message` as their
/// own greppable fields.
#[tokio::test]
async fn buffered_request_logs_upstream_error_type_and_message_on_non_2xx() {
    let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0.content: unexpected field wombat"}}"#;
    let up_addr = stub::canned(stub::reply("400 Bad Request", "application/json", body));

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

    let proxy = app(Arc::clone(&manager));

    let sink = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    assert_eq!(resp.status().as_u16(), 400);
    let _ = body_text(resp).await;

    let log = sink.contents();
    assert!(
        log.contains("error.type=\"invalid_request_error\"")
            && log.contains("unexpected field wombat"),
        "expected a log line carrying the upstream body's error.type \
             (invalid_request_error) and error.message text, got: {log:?}"
    );
}

// --- request timing -----------------------------------------------------

/// A fake upstream that reads the request, waits [`HEADER_DELAY_MS`], then
/// writes `response` (status line, headers and body) and closes.
async fn spawn_header_delayed_upstream(response: String) -> StubAddr {
    stub::delayed(response, Duration::from_millis(HEADER_DELAY_MS))
}

/// Sends one `method path` (a `{}` body on a POST) to `manager`'s router, as the usage-line
/// tests above do, in-process.
async fn send_one(manager: &Arc<Manager>, method: Method, path: &str) -> Response {
    let body = if method == Method::POST { "{}" } else { "" };
    send_to_proxy(app(Arc::clone(manager)), client_request(method, path, body)).await
}

fn info_sink() -> (SharedBuf, tracing::subscriber::DefaultGuard) {
    let sink = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (sink, guard)
}

/// THE BITING TEST for the streamed timing: one `request usage` line carries
/// every timing field, and the upstream's header delay lands in `upstream_ms`
/// and `first_byte_ms`, never in `wait_ms`. Captured at DEBUG, so a second
/// timing line for the same request (`request timing`) would be seen; and no
/// `slow request`, because tcr held nothing.
#[tokio::test]
async fn streamed_request_usage_line_carries_timing() {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let up_addr =
        spawn_header_delayed_upstream(stub::reply("200 OK", "text/event-stream", body)).await;
    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
    let proxy = app(Arc::clone(&manager));
    let (sink, _guard) = debug_sink();

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    assert_eq!(resp.status().as_u16(), 200);
    let _ = body_text(resp).await;

    let log = wait_for_line(&sink, "request usage").await;
    let usage = lines_with(&log, "request usage");
    assert_eq!(usage.len(), 1, "exactly one request usage line: {log:?}");
    let line = usage[0];
    // Shown under `--nocapture`: the line these assertions read.
    eprintln!("{line}");
    for name in [
        "wait_ms",
        "throttle_ms",
        "upstream_ms",
        "first_byte_ms",
        "total_ms",
        "attempts",
        "switched",
        "completed",
    ] {
        assert!(field(line, name).is_some(), "{name} missing on: {line}");
    }
    let (wait, upstream, first_byte, total) = (
        ms_field(line, "wait_ms"),
        ms_field(line, "upstream_ms"),
        ms_field(line, "first_byte_ms"),
        ms_field(line, "total_ms"),
    );
    assert!(
        upstream >= HEADER_DELAY_MS,
        "upstream_ms {upstream}: {line}"
    );
    assert!(
        wait < HEADER_DELAY_MS,
        "wait_ms {wait} holds the upstream's delay: {line}"
    );
    assert!(
        first_byte >= HEADER_DELAY_MS,
        "first_byte_ms {first_byte}: {line}"
    );
    assert!(
        total >= first_byte && total >= wait,
        "total_ms {total}: {line}"
    );
    assert_eq!(field(line, "attempts"), Some("1"), "{line}");
    assert_eq!(field(line, "switched"), Some("false"), "{line}");
    assert_eq!(field(line, "completed"), Some("true"), "{line}");
    assert!(lines_with(&log, "request timing").is_empty(), "{log:?}");
    assert!(lines_with(&log, "slow request").is_empty(), "{log:?}");
}

/// The buffered twin: `first_byte_ms` is the whole body in hand, and the
/// timing again rides on the one `request usage` line, and (captured at
/// DEBUG) on no second line.
#[tokio::test]
async fn buffered_request_usage_line_carries_timing() {
    let body = r#"{"usage":{"input_tokens":7,"output_tokens":11}}"#;
    let up_addr =
        spawn_header_delayed_upstream(stub::reply("200 OK", "application/json", body)).await;
    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
    let proxy = app(Arc::clone(&manager));
    let (sink, _guard) = debug_sink();

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    assert_eq!(resp.status().as_u16(), 200);
    let _ = body_text(resp).await;

    let log = sink.contents();
    let usage = lines_with(&log, "request usage");
    assert_eq!(usage.len(), 1, "exactly one request usage line: {log:?}");
    let line = usage[0];
    let (wait, upstream, first_byte, total) = (
        ms_field(line, "wait_ms"),
        ms_field(line, "upstream_ms"),
        ms_field(line, "first_byte_ms"),
        ms_field(line, "total_ms"),
    );
    assert!(
        upstream >= HEADER_DELAY_MS,
        "upstream_ms {upstream}: {line}"
    );
    assert!(
        wait < HEADER_DELAY_MS,
        "wait_ms {wait} holds the upstream's delay: {line}"
    );
    assert!(first_byte >= upstream, "first_byte_ms {first_byte}: {line}");
    assert!(
        total >= first_byte && total >= wait,
        "total_ms {total}: {line}"
    );
    assert_eq!(field(line, "completed"), Some("true"), "{line}");
    assert!(lines_with(&log, "request timing").is_empty(), "{log:?}");
}

/// Every line `log` holds at INFO.
fn info_lines(log: &str) -> Vec<&str> {
    log.lines().filter(|l| l.contains(" INFO ")).collect()
}

/// A served request writes ONE line at INFO, its `request usage` line, and that line
/// carries what the DEBUG `serving request` and `upstream response` lines name: the
/// account's index, the path and the status. Streamed and buffered, captured at INFO
/// as the default filter is.
#[tokio::test]
async fn a_served_request_is_one_info_line() {
    let stream_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let buffered_body = r#"{"usage":{"input_tokens":7,"output_tokens":11}}"#;
    for (content_type, body) in [
        ("text/event-stream", stream_body),
        ("application/json", buffered_body),
    ] {
        let up_addr =
            spawn_header_delayed_upstream(stub::reply("200 OK", content_type, body)).await;
        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
        let (sink, _guard) = info_sink();

        let resp = send_one(&manager, Method::POST, "/v1/messages").await;
        assert_eq!(resp.status().as_u16(), 200);
        let _ = body_text(resp).await;

        let log = wait_for_line(&sink, "request usage").await;
        let info = info_lines(&log);
        assert_eq!(info.len(), 1, "{content_type}: one INFO line: {log:?}");
        let line = info[0];
        assert!(line.contains("request usage"), "{line}");
        assert_eq!(field(line, "account_index"), Some("0"), "{line}");
        assert_eq!(field(line, "path"), Some("/v1/messages"), "{line}");
        assert_eq!(field(line, "status"), Some("200"), "{line}");
    }
}

/// The INFO exceptions to the rule above: an upstream answer that is not a success
/// keeps its `upstream response` line at INFO, and a relayed request is at INFO only
/// when its answer is not a success either.
#[tokio::test]
async fn only_an_unsuccessful_answer_logs_its_response_line_at_info() {
    let error = r#"{"type":"error","error":{"type":"invalid_request_error","message":"no"}}"#;
    for (path, status_line, body, message, want_info) in [
        (
            "/v1/messages",
            "400 Bad Request",
            error,
            "upstream response",
            true,
        ),
        (
            "/v1/code/sessions",
            "200 OK",
            "{}",
            "relayed response",
            false,
        ),
        (
            "/v1/code/sessions",
            "404 Not Found",
            "{}",
            "relayed response",
            true,
        ),
    ] {
        let up_addr =
            spawn_header_delayed_upstream(stub::reply(status_line, "application/json", body)).await;
        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
        let (sink, _guard) = info_sink();
        let method = if path == "/v1/messages" {
            Method::POST
        } else {
            Method::GET
        };
        let resp = send_one(&manager, method, path).await;
        let _ = body_text(resp).await;
        // The relayed body logs its timing on drop; give that a moment to land.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let log = sink.contents();
        let lines = lines_with(&log, message);
        if want_info {
            assert_eq!(lines.len(), 1, "{path} {status_line}: {log:?}");
            assert!(lines[0].contains(" INFO "), "{}", lines[0]);
        } else {
            assert!(info_lines(&log).is_empty(), "{path} {status_line}: {log:?}");
        }
    }
}

/// A request with no `request usage` line (a forwarded 400) gets its timing
/// on the DEBUG `request timing` line, exactly once, and nothing at INFO.
#[tokio::test]
async fn request_without_usage_logs_timing_at_debug_only() {
    let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"no"}}"#;
    let response = stub::reply("400 Bad Request", "application/json", body);
    for level in [tracing::Level::DEBUG, tracing::Level::INFO] {
        let up_addr = spawn_header_delayed_upstream(response.clone()).await;
        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
        let (sink, _guard) = if level == tracing::Level::DEBUG {
            debug_sink()
        } else {
            info_sink()
        };

        let resp = send_one(&manager, Method::POST, "/v1/messages").await;
        assert_eq!(resp.status().as_u16(), 400);
        let _ = body_text(resp).await;

        let log = sink.contents();
        let timing = lines_with(&log, "request timing");
        assert!(lines_with(&log, "request usage").is_empty(), "{log:?}");
        if level == tracing::Level::INFO {
            assert!(timing.is_empty(), "request timing must be DEBUG: {log:?}");
            continue;
        }
        assert_eq!(timing.len(), 1, "exactly one request timing line: {log:?}");
        let line = timing[0];
        eprintln!("{line}");
        assert!(line.contains("DEBUG"), "{line}");
        assert_eq!(field(line, "status"), Some("400"), "{line}");
        assert_eq!(field(line, "stream"), Some("false"), "{line}");
        assert!(ms_field(line, "upstream_ms") >= HEADER_DELAY_MS, "{line}");
        assert!(ms_field(line, "wait_ms") < HEADER_DELAY_MS, "{line}");
    }
}

/// A client that walks away mid-stream is reported as `completed=false`, on
/// the same `request usage` line (the usage from `message_start` still
/// counts), with `total_ms` at the walk-away rather than never.
#[tokio::test]
async fn abandoned_stream_reports_completed_false() {
    let event = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
    );
    let up_addr = stub::stalled(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n",
        event.len()
    ));
    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
    let (sink, _guard) = info_sink();
    // Drop the actual axum response body at the client-abandonment seam. A
    // reqwest pool may retain its transport after its client body is dropped,
    // so that does not deterministically drop the server's passthrough body.
    let resp = drive_full(
        manager,
        Method::POST,
        "/v1/messages",
        Some(loopback_peer()),
        &[],
        "{}",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    // A transport chunk need not contain a complete SSE frame. Wait until
    // the usage-bearing event has actually reached the client before leaving.
    let mut received = Vec::new();
    while !received.windows(2).any(|pair| pair == b"\n\n") {
        let chunk = body.next().await.expect("expected message_start").unwrap();
        received.extend_from_slice(&chunk);
    }
    drop(body);

    let log = wait_for_line(&sink, "request usage").await;
    let usage = lines_with(&log, "request usage");
    assert_eq!(usage.len(), 1, "{log:?}");
    assert_eq!(field(usage[0], "completed"), Some("false"), "{}", usage[0]);
}

/// The relay path's twin: one DEBUG `relay timing` line per relayed request,
/// streamed here (`/v1/code/` is a client-credential relay), with the
/// header delay in `upstream_ms`.
#[tokio::test]
async fn relayed_request_logs_relay_timing_at_debug() {
    let body = "{}";
    let up_addr =
        spawn_header_delayed_upstream(stub::reply("200 OK", "application/json", body)).await;
    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
    let (sink, _guard) = debug_sink();

    let resp = send_one(&manager, Method::GET, "/v1/code/sessions").await;
    assert_eq!(resp.status().as_u16(), 200);
    let _ = body_text(resp).await;

    let log = wait_for_line(&sink, "relay timing").await;
    let relay = lines_with(&log, "relay timing");
    assert_eq!(relay.len(), 1, "exactly one relay timing line: {log:?}");
    let line = relay[0];
    eprintln!("{line}");
    assert!(line.contains("DEBUG"), "{line}");
    assert_eq!(field(line, "path"), Some("/v1/code/sessions"), "{line}");
    assert_eq!(field(line, "stream"), Some("true"), "{line}");
    assert_eq!(field(line, "completed"), Some("true"), "{line}");
    let upstream = ms_field(line, "upstream_ms");
    assert!(
        upstream >= HEADER_DELAY_MS,
        "upstream_ms {upstream}: {line}"
    );
    assert!(ms_field(line, "total_ms") >= upstream, "{line}");
    assert!(
        lines_with(&log, "request timing").is_empty()
            && lines_with(&log, "request usage").is_empty(),
        "a relayed request is not a pooled one: {log:?}"
    );
}

fn timing_fields_waiting(wait_ms: u64) -> TimingFields {
    TimingFields {
        wait_ms: Some(wait_ms),
        throttle_ms: 0,
        upstream_ms: Some(1),
        first_byte_ms: Some(wait_ms + 1),
        total_ms: wait_ms + 2,
        attempts: 1,
        switched: false,
        status: 200,
        stream: true,
        completed: true,
        account: Some("alice@example.com".to_string()),
        model: None,
        session: None,
    }
}

/// The one INFO exception: tcr held the request for `SLOW_WAIT_MS` or more.
/// Both sides of the boundary, so a threshold off by one fails here.
#[test]
fn slow_request_line_fires_only_when_tcr_held_the_request() {
    let (sink, _guard) = info_sink();
    timing_fields_waiting(SLOW_WAIT_MS - 1).log_if_held();
    assert!(sink.contents().is_empty(), "{:?}", sink.contents());
    timing_fields_waiting(SLOW_WAIT_MS).log_if_held();
    let log = sink.contents();
    let slow = lines_with(&log, "slow request");
    assert_eq!(slow.len(), 1, "{log:?}");
    assert!(slow[0].contains(" INFO "), "{}", slow[0]);
    assert_eq!(ms_field(slow[0], "wait_ms"), SLOW_WAIT_MS, "{}", slow[0]);
}

/// The SECOND blind spot this bridge closes: a proxy-GENERATED refusal
/// (never touched an upstream at all) previously left no trace carrying
/// its own message — `grep -cE 'quota exhausted on all|accounts exhausted'`
/// returned `0` for a day the fleet-exhausted message almost certainly
/// fired. `error_response` is the single constructor every such refusal
/// goes through, so a direct call must now log a line carrying its
/// `error.type` and `error.message`.
#[test]
fn error_response_logs_its_own_type_and_message() {
    let sink = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let _ = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_error",
        "All 13 accounts exhausted. Retry in 42s.",
        Some(42),
    );

    let log = sink.contents();
    assert!(
        log.contains("error.type=\"rate_limit_error\"") && log.contains("accounts exhausted"),
        "expected error_response's own construction-site log line to \
             carry error.type and the message text, got: {log:?}"
    );
}

/// THE OTHER BUG this fix closes: a stream that ends after `message_start` /
/// `message_delta` with no `message_stop` is not a clean serve — it is
/// indistinguishable, from inside `parse_sse_usage`, from a connection that
/// was severed mid-turn. Anthropic's ONLY marker that a turn actually
/// finished is `message_stop`; a complete turn carries one, and anything
/// that reaches EOF without one — this fixture included — is truncated.
/// Before this fix this test asserted `stream_error_count == 0` under the
/// docstring "a clean 200 SSE stream" for exactly this fixture — the
/// assertion WAS the bug, encoded as the contract. It is inverted here to
/// assert the truncation IS detected.
#[tokio::test]
async fn sse_stream_without_message_stop_is_recorded_as_truncated() {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
    );
    let up_addr = stub::canned(stub::reply("200 OK", "text/event-stream", body));

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

    let proxy = app(Arc::clone(&manager));

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    let _ = body_text(resp).await;

    let (seen, polls) = poll_stream_error_count(&manager, 1).await;
    assert_eq!(
        seen, 1,
        "polled {polls} times, stream-error count stayed {seen} — a stream that \
             ends without message_stop must be recorded as truncated, not booked as \
             a clean serve"
    );

    // The tee task updates usage BEFORE it records the stream error (see the
    // spawned task above `parse_sse_usage` is awaited in), so by the time the
    // poll above observes the error count, usage from `message_start` has
    // already landed too — truncation detection does not cost the quota
    // accounting anything.
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens,
        5,
        "usage already parsed before the stream was found truncated must still count"
    );
}

/// POSITIVE CONTROL for the truncation detector above: the identical
/// fixture, this time WITH `message_stop`, must record NO stream error.
/// Without this, the detector could not be told apart from one that fires
/// unconditionally — this is what proves it discriminates on the
/// terminator rather than on the mere absence of an in-band `error` event.
#[tokio::test]
async fn sse_stream_with_message_stop_records_no_stream_error() {
    let up_addr = stub::canned(stub::reply(
        "200 OK",
        "text/event-stream",
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        ),
    ));

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

    let proxy = app(Arc::clone(&manager));

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    let _ = body_text(resp).await;

    // Poll for the USAGE to land (a positive, waitable signal that the tee
    // task has finished), then assert the error count is absent — never a
    // zero-wait check, which would pass vacuously before the tee even runs.
    let mut polls = 0u32;
    let mut input_tokens = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens;
    while input_tokens == 0 && polls < 200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        input_tokens = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens;
        polls += 1;
    }
    assert_eq!(
        input_tokens, 5,
        "polled {polls} times waiting for usage to land"
    );

    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].stream_error_count,
        0,
        "a stream that reaches message_stop must record no stream error"
    );
}

/// FALSE-POSITIVE case #1 (the worst of the set): a CLIENT abandoning the
/// connection mid-turn — exactly what happens when a user hits Esc in
/// Claude Code — must record NOTHING, not `"truncated"`. Before the
/// `ended`/`evidence_dropped` machinery, `tx` was owned by the `.map()`
/// closure over `resp.bytes_stream()`; axum dropping the response body on
/// client disconnect dropped that closure, hence `tx`, and `rx.recv()`
/// returning `None` was INDISTINGUISHABLE from the upstream finishing the
/// stream itself. The fake upstream here sends `message_start` and then
/// hangs forever (never sends `message_stop`, never closes) — the ONLY
/// way this stream ever ends is the test client dropping it, so any
/// `stream_error` this test observes was fabricated from an interrupt,
/// not from anything upstream actually did.
///
/// Also covers the overwrite half of the same bug: a genuine PRIOR
/// `last_stream_error` is seeded before the interrupted request, and must
/// survive it untouched — `record_stream_error` always overwrites the
/// label (see its doc comment), so a false "truncated" here would have
/// erased real evidence of the account being `overloaded_error`.
#[tokio::test]
async fn client_abort_mid_stream_records_no_stream_error() {
    let event = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
    );
    // Chunked, not content-length: a real Messages turn's
    // length isn't known up front. One chunk, then HANG —
    // never a terminating "0\r\n\r\n" chunk, never a close.
    // If the client does not abandon this connection, this
    // handler simply blocks forever and the test times out —
    // which is the point: nothing here ever produces
    // `message_stop` or EOF on its own.
    let up_addr = stub::stalled(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n",
        event.len()
    ));

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
    // Seed a genuine prior error BEFORE the interrupted request — the
    // overwrite-on-abandonment bug would clobber this.
    manager.record_stream_error(0, "overloaded_error");

    let proxy = app(Arc::clone(&manager));

    tokio::time::timeout(Duration::from_secs(10), async {
        let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
        let mut body = resp.into_body().into_data_stream();
        // Read the ONE chunk the fake upstream ever sends, proving the
        // tee actually started, THEN abandon — mirrors a client that
        // received partial output before the user hit Esc.
        let first = body.next().await;
        assert!(first.is_some(), "expected the message_start chunk");
        drop(body);
    })
    .await
    .expect("client-abort scenario must not hang the server");

    // Positive, waitable signal that the tee's parse loop drained the one
    // chunk it got: usage from `message_start` lands regardless of how
    // the stream ends (see `sse_stream_without_message_stop_...` above).
    let (input_tokens, polls) = {
        let mut polls = 0u32;
        let mut seen = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens;
        while seen == 0 && polls < 200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            seen = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].input_tokens;
            polls += 1;
        }
        (seen, polls)
    };
    assert_eq!(
        input_tokens, 5,
        "polled {polls} times waiting for usage to land"
    );

    // No positive signal exists for "the classifier decided to abstain" —
    // silence IS the pass condition — so flat-wait comfortably past the
    // `ended`/`evidence_dropped` decision (a local oneshot + atomic load,
    // never network-bound) before asserting nothing changed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snapshot.accounts[0].stream_error_count, 1,
        "count must stay at the ONE seeded error — the client abandoning \
             the connection must add nothing"
    );
    assert_eq!(
        snapshot.accounts[0].last_stream_error.as_deref(),
        Some("overloaded_error"),
        "the genuine prior error label must survive a client abandoning \
             a later stream — record_stream_error unconditionally overwrites \
             this field, so a fabricated \"truncated\" here would erase real \
             evidence"
    );
}

/// FALSE-POSITIVE case #3: a forwarded non-2xx response never had a
/// `message_stop` contract — it is an error response, not a truncated
/// turn. The fixture is deliberately the SAME shape that would have been
/// misclassified as truncated before this fix (a `message_start` with no
/// `message_stop`), on a 400 instead of a 200, to prove the status gate
/// — not the `message_start` gate above — is what suppresses it here.
#[tokio::test]
async fn non_2xx_sse_body_records_no_stream_error() {
    let up_addr = stub::canned(stub::reply(
        "400 Bad Request",
        "text/event-stream",
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        ),
    ));

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

    let proxy = app(Arc::clone(&manager));

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let _ = body_text(resp).await;

    // record_served fires on the terminal outcome regardless of status
    // (same as the in-band-error test above) — a waitable positive
    // signal that the request was actually processed before we check for
    // the absence of a stream error.
    let mut polls = 0u32;
    let mut requests = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests;
    while requests == 0 && polls < 200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        requests = manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests;
        polls += 1;
    }
    assert_eq!(
        requests, 1,
        "polled {polls} times waiting for the request to be booked"
    );

    // Flat-wait past where the tee task would have recorded a stream
    // error had the status gate not suppressed it — same rationale as
    // the client-abort test above (silence is the pass condition).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].stream_error_count,
        0,
        "a forwarded 400 body missing message_stop is an error response, \
             not a truncated Messages turn — it must not be classified"
    );
}

/// End-to-end version of `sse_stream_severed_before_any_event_records_truncation`
/// above: a 2xx `text/event-stream` response that closes with an EMPTY
/// body — no bytes at all, so `parse_sse_usage` sees zero events — must
/// be recorded as truncated through the FULL pipeline, including the
/// `status_is_success` / `ended_naturally` / `evidence_dropped` gates the
/// unit-level test above never exercises. This is the severest live
/// regression named in the round-4 review: before the fix, this exact
/// shape recorded NOTHING — zero content delivered to the client, and
/// the account looked perfectly healthy.
#[tokio::test]
async fn sse_stream_severed_before_message_start_is_recorded_as_truncated_e2e() {
    // Headers claim an SSE body, then the connection closes
    // having sent NOT ONE byte of it — severed before the
    // stream could ever produce its first parseable event.
    let up_addr = stub::canned(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    );

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

    let proxy = app(Arc::clone(&manager));

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = body_text(resp).await;

    let (seen, polls) = poll_stream_error_count(&manager, 1).await;
    assert_eq!(
        seen, 1,
        "polled {polls} times, stream-error count stayed {seen} — a 2xx \
             SSE stream severed before its first event must be recorded as \
             truncated, not booked as a clean serve"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
            .last_stream_error
            .as_deref(),
        Some(TRUNCATED_STREAM_ERROR_KIND)
    );
}

/// MUST-RECORD: "an in-band `error` event, at ANY status, always." This is
/// the live regression from `status_is_success` wrapping BOTH the
/// synthesized-truncation branch and the positive-observation branch: a
/// genuine `error` event forwarded inside a NON-2xx body used to be
/// silently dropped, a coverage regression versus before the truncation
/// feature existed at all. Same fixture shape as
/// `sse_error_event_is_observed_not_counted_as_clean_serve` (a 200), this
/// time on a 400 — proving the status gate no longer reaches the positive
/// case.
#[tokio::test]
async fn in_band_error_on_non_2xx_is_recorded_regardless_of_status() {
    let up_addr = stub::canned(stub::reply(
        "400 Bad Request",
        "text/event-stream",
        concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad\"}}\n\n",
        ),
    ));

    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);

    let proxy = app(Arc::clone(&manager));

    let resp = send_to_proxy(proxy, client_request(Method::POST, "/v1/messages", "{}")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let _ = body_text(resp).await;

    let (seen, polls) = poll_stream_error_count(&manager, 1).await;
    assert_eq!(
        seen, 1,
        "polled {polls} times, stream-error count stayed {seen} — an \
             in-band SSE error event must be observed on a forwarded non-2xx \
             body too, not just on 200"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
            .last_stream_error
            .as_deref(),
        Some("invalid_request_error"),
        "the recorded kind must be the POSITIVE observation read off the \
             wire, never the synthesized \"truncated\" kind — this path must \
             not go anywhere near the status_is_success gate"
    );
}
