//! Codex usage booking, and the holds a Codex 429 or 403 puts on its account.

use super::*;

/// **A streamed turn is filed under the model its REQUEST named, and a Codex turn whose
/// request named none under the one its stream's terminal event named**
/// (`request_model_for_usage.or(response_model)` in `handle`), as each account's own usage
/// row reads it back. A Codex body with no model in it (a zstd body cannot be peeked for
/// one) lands under `response.completed`'s model: not under `"unknown"`, and not under the
/// model `response.created` named. A Codex body naming one keeps it, whatever the stream
/// says, and so does an Anthropic turn, whatever its `message_start` says.
#[tokio::test]
async fn a_streamed_codex_turn_with_no_request_model_is_filed_under_the_streams() {
    let codex_url = spawn_fixed_origin(
            "text/event-stream",
            concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-opening\"}}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-astra\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}}\n\n",
            ),
        )
        .await;
    let anthropic_url = spawn_fixed_origin(
            "text/event-stream",
            concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-served-5\",\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ),
        )
        .await;
    let manager = Manager::with_live_refresher(codex_fleet(&anthropic_url, &codex_url), None);
    let codex = [
        ("authorization", "Bearer client-own"),
        ("content-type", "application/json"),
    ];
    let anthropic = [("content-type", "application/json")];
    for (path, headers, body) in [
        (
            crate::provider::CODEX_RESPONSES_PATH,
            &codex[..],
            r#"{"input":[]}"#,
        ),
        (
            crate::provider::CODEX_RESPONSES_PATH,
            &codex[..],
            r#"{"model":"gpt-6-requested","input":[]}"#,
        ),
        (
            "/v1/messages",
            &anthropic[..],
            r#"{"model":"claude-opus-5","messages":[]}"#,
        ),
    ] {
        let (status, bytes) = drive(
            Arc::clone(&manager),
            Method::POST,
            path,
            Some(loopback_peer()),
            headers,
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
    // Booked by the tee's detached task, which can finish after the client reads EOF.
    let models = |idx: usize| -> Vec<String> {
        manager.snapshot(OffsetDateTime::now_utc()).accounts[idx]
            .usage
            .as_ref()
            .map_or_default(|row| row.today_by_model.keys().cloned().collect())
    };
    let mut polls = 0u32;
    while (models(0).is_empty() || models(1).len() < 2) && polls < 200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        polls += 1;
    }
    assert_eq!(
        models(1),
        ["gpt-6-astra", "gpt-6-requested"],
        "bob's two Codex turns (polls: {polls})"
    );
    assert_eq!(
        models(0),
        ["claude-opus-5"],
        "alice's Anthropic turn (polls: {polls})"
    );
}

/// A Codex turn's reasoning tokens reach its account's usage buckets, still inside output.
/// An Anthropic turn's buckets carry none.
#[tokio::test]
async fn codex_usage_records_its_reasoning_tokens() {
    let codex_url = spawn_fixed_origin(
            "text/event-stream",
            concat!(
                "event: response.completed\n",
                r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.1-codex","usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":100},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":10}}}}"#,
                "\n\n",
            ),
        )
        .await;
    let anthropic_url = spawn_fixed_origin(
            "text/event-stream",
            concat!(
                "event: message_start\n",
                r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":7,"output_tokens":1}}}"#,
                "\n\n",
                "event: message_stop\n",
                r#"data: {"type":"message_stop"}"#,
                "\n\n",
            ),
        )
        .await;
    let manager = Manager::with_live_refresher(codex_fleet(&anthropic_url, &codex_url), None);
    let codex = [
        ("authorization", "Bearer client-own"),
        ("content-type", "application/json"),
    ];
    let anthropic = [("content-type", "application/json")];
    for (path, headers, body) in [
        (
            crate::provider::CODEX_RESPONSES_PATH,
            &codex[..],
            r#"{"input":[]}"#,
        ),
        (
            "/v1/messages",
            &anthropic[..],
            r#"{"model":"claude-opus-5","messages":[]}"#,
        ),
    ] {
        let (status, bytes) = drive(
            Arc::clone(&manager),
            Method::POST,
            path,
            Some(loopback_peer()),
            headers,
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
    // Booked by the tee's detached task, which can finish after the client reads EOF.
    let last_hour = |idx: usize| {
        manager.snapshot(OffsetDateTime::now_utc()).accounts[idx]
            .usage
            .as_ref()
            .map_or_default(|usage| usage.last_hour)
    };
    let mut polls = 0u32;
    while (last_hour(1).requests == 0 || last_hour(0).requests == 0) && polls < 200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        polls += 1;
    }
    assert_eq!(
        (last_hour(1).reasoning_tokens, last_hour(1).output_tokens),
        (10, 40),
        "bob's Codex turn (polls: {polls})"
    );
    assert_eq!(last_hour(0).reasoning_tokens, 0, "alice's Anthropic turn");
}

/// **A Codex turn streamed under some other Content-Type still reaches the ledger.** The
/// stream arm used to be chosen by the `Content-Type` header alone, so an SSE body labelled
/// anything but `text/event-stream` was buffered and read as JSON, failed at its first byte,
/// and booked the request with zero tokens (seen live on 2026-10-17: a 127 KB Codex body,
/// `expected value at line 1 column 1`). The body's first bytes decide now.
#[tokio::test]
async fn a_codex_stream_under_another_content_type_is_still_counted() {
    for content_type in [
        "application/json",
        "text/plain; charset=utf-8",
        "application/octet-stream",
    ] {
        let codex_url = spawn_fixed_origin(
                content_type,
                concat!(
                    "event: response.created\n",
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-astra\"}}\n\n",
                    "event: response.completed\n",
                    r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-6-astra","usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":100},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":10}}}}"#,
                    "\n\n",
                ),
            )
            .await;
        let anthropic_url = spawn_fixed_origin("application/json", "{}").await;
        let manager = Manager::with_live_refresher(codex_fleet(&anthropic_url, &codex_url), None);
        let (status, bytes) = drive(
            Arc::clone(&manager),
            Method::POST,
            crate::provider::CODEX_RESPONSES_PATH,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("content-type", "application/json"),
            ],
            r#"{"input":[]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{content_type}");
        assert!(
            bytes.starts_with(b"event: response.created\n"),
            "{content_type}: the client gets the stream byte for byte"
        );
        // Booked by the tee's detached task, which can finish after the client reads EOF.
        let last_hour = || {
            manager.snapshot(OffsetDateTime::now_utc()).accounts[1]
                .usage
                .as_ref()
                .map_or_default(|usage| usage.last_hour)
        };
        let mut polls = 0u32;
        while last_hour().output_tokens == 0 && polls < 200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            polls += 1;
        }
        let row = last_hour();
        assert_eq!(
            (row.output_tokens, row.reasoning_tokens),
            (40, 10),
            "{content_type}: the turn's tokens (polls: {polls})"
        );
    }
}

/// **A non-streamed Codex turn is booked with Codex's own field names.** Its body is a bare
/// Responses object, with no `response.completed` wrapper, and it used to be read with the
/// Anthropic names, which a Responses `usage` does not carry, so its cache reads were booked
/// as zero. The numbers are upstream's own fixture (`parses_cache_write_token_usage`): 40 of
/// the 100 input tokens read from the cache and 60 written to it.
#[tokio::test]
async fn a_non_streamed_codex_turn_books_its_cache_reads_and_writes() {
    let codex_url = spawn_fixed_origin(
            "application/json",
            r#"{"id":"resp_1","object":"response","status":"completed","model":"gpt-6-astra","output":[],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40,"cache_write_tokens":60},"output_tokens":10,"output_tokens_details":{"reasoning_tokens":5},"total_tokens":110}}"#,
        )
        .await;
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
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    // Booked inline on this path, before the response is built: nothing to wait for.
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let bob = &snapshot.accounts[1];
    assert_eq!(
        (
            bob.input_tokens,
            bob.cache_read_tokens,
            bob.cache_creation_tokens,
            bob.output_tokens
        ),
        (100, 40, 60, 10),
        "(input total, cache read, cache creation, output)"
    );
}

/// **A non-streamed Codex turn whose request named no model is filed under the model its
/// Responses object names, and it mints no bound token.** The non-streamed twin of
/// `a_streamed_codex_turn_with_no_request_model_is_filed_under_the_streams`: a zstd request
/// body cannot be peeked for a model, so the response is the one place it is named. The
/// body's `resp_…` id is not an Anthropic message id, and no later request looks one up.
///
/// Watched red, twice: file the turn under the request's model alone and it lands under
/// `"unknown"`; record the body's bound tokens for every provider again and `resp_1` is
/// recorded as a message id.
#[tokio::test]
async fn a_non_streamed_codex_turn_is_filed_under_its_responses_model_and_binds_nothing() {
    let codex_url = spawn_fixed_origin(
            "application/json",
            r#"{"id":"resp_1","object":"response","status":"completed","model":"gpt-6-astra","output":[],"usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}}"#,
        )
        .await;
    let anthropic_echo = CapturingUpstream::spawn().await;
    let manager = Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_url), None);
    // zstd magic + junk: not JSON, so the request names no model tcr can read
    let body: Vec<u8> = vec![
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x58, 0x41, 0x00, 0x00, 0x7b, 0x7d,
    ];
    let (status, answer) = drive_bytes(
        Arc::clone(&manager),
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
            ("content-encoding", "zstd"),
        ],
        body,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&answer)
    );
    // Booked inline on this path, before the response is built: nothing to wait for.
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let models: Vec<String> = snapshot.accounts[1]
        .usage
        .as_ref()
        .map_or_default(|row| row.today_by_model.keys().cloned().collect());
    assert_eq!(
        models,
        ["gpt-6-astra"],
        "bob's Codex turn is filed under the model its Responses object names"
    );
    let bound = manager.bound_token_snapshot();
    let output = tempfile::tempdir().unwrap();
    assert_eq!(
        crate::bound_tokens::save(&output.path().join("bound.json"), bound, crate::now_ms())
            .unwrap(),
        0,
        "a Codex body's resp_ id is not a bound token"
    );
}

/// A Codex origin that refuses the FIRST request it sees, whoever's bearer it carries,
/// with the refusal shape of `codex-api/src/rate_limits.rs` (both windows, the
/// five-hour one listed first at 30% and the weekly one full) plus, when given,
/// `x-codex-rate-limit-reached-type` and a `retry-after`. Every later request is served
/// `200 {}`. Returns the origin, the bearer it refused, and the bearer of every request
/// it served, in order, so a test reads who served the retry off the origin itself.
async fn spawn_codex_origin_refusing_the_first(
    reached_type: Option<&'static str>,
    retry_after: Option<&'static str>,
    five_hour_reset: i64,
    weekly_reset: i64,
) -> (
    String,
    Arc<std::sync::Mutex<Option<String>>>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let refused: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let served: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let (refused_by, served_by) = (Arc::clone(&refused), Arc::clone(&served));
    let origin = Router::new().fallback(move |req: Request| {
        let (refused_by, served_by) = (Arc::clone(&refused_by), Arc::clone(&served_by));
        async move {
            let bearer = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            {
                let mut first = refused_by.lock().expect("refusal record poisoned");
                if first.is_none() {
                    *first = Some(bearer);
                    let mut refusal = Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header(CONTENT_TYPE, "application/json")
                        .header("x-codex-primary-used-percent", "30")
                        .header("x-codex-primary-window-minutes", "300")
                        .header("x-codex-primary-reset-at", five_hour_reset.to_string())
                        .header("x-codex-secondary-used-percent", "100")
                        .header("x-codex-secondary-window-minutes", "10080")
                        .header("x-codex-secondary-reset-at", weekly_reset.to_string());
                    if let Some(kind) = reached_type {
                        refusal = refusal.header("x-codex-rate-limit-reached-type", kind);
                    }
                    if let Some(secs) = retry_after {
                        refusal = refusal.header("retry-after", secs);
                    }
                    return refusal
                        .body(Body::from(if reached_type.is_some() {
                            r#"{"error":{"type":"usage_limit_reached","message":"out"}}"#
                        } else {
                            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#
                        }))
                        .expect("429 response");
                }
            }
            served_by
                .lock()
                .expect("served record poisoned")
                .push(bearer);
            Response::builder()
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .expect("200 response")
        }
    });
    let addr = stub::http(origin);
    (format!("http://{addr}"), refused, served)
}

/// **A Codex 429 is the ACCOUNT's, never the request's.** The account that answered it
/// is held until the reset of its FULL window (the weekly one here: not the five-hour
/// window listed first, and not the 429's own `retry-after`), its quota reads rejected,
/// and the rotation moves on, so a second Codex account serves the same request. With
/// and without a `retry-after`, because each fails differently without the Codex arm:
/// `is_request_scoped_429` reads Anthropic's guidance (`?beta=true`, `unified-status`),
/// which no Codex request carries, so a guidance-free Codex 429 was forwarded with no
/// account held, and one with `retry-after: 60` parked its account for 60 seconds. The
/// account that served the retry is the one the origin RECORDED serving, never inferred
/// from which one is held.
///
/// Watched red: drop `provider == Anthropic` from the request-scoped gate, then put
/// `retry-after` ahead of the window in the Codex hold.
#[tokio::test]
async fn a_codex_429_holds_its_account_to_the_full_windows_reset_and_rotates() {
    for retry_after in [None, Some("60")] {
        let anthropic_echo = CapturingUpstream::spawn().await;
        let secs = OffsetDateTime::now_utc().unix_timestamp();
        let (five_hour_reset, weekly_reset) = (secs + 600, secs + 1800);
        let (codex_url, refused, served) = spawn_codex_origin_refusing_the_first(
            Some("secondary"),
            retry_after,
            five_hour_reset,
            weekly_reset,
        )
        .await;
        let manager = Manager::with_live_refresher(
            codex_fleet_with_carol(&anthropic_echo.url, &codex_url),
            None,
        );

        let (status, answer) = drive(
            Arc::clone(&manager),
            Method::POST,
            crate::provider::CODEX_RESPONSES_PATH,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("content-type", "application/json"),
            ],
            "{}",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "retry-after {retry_after:?}: the other Codex account serves the request: {}",
            String::from_utf8_lossy(&answer)
        );
        let served_bearers = served.lock().expect("served record poisoned").clone();
        assert_eq!(
            served_bearers.len(),
            1,
            "served exactly once: {served_bearers:?}"
        );
        assert_eq!(
            anthropic_echo.request_count(),
            0,
            "rotation never leaves Codex"
        );

        let refused_bearer = refused
            .lock()
            .expect("refusal record poisoned")
            .clone()
            .expect("one account was refused");
        let held_idx = codex_index_of(&refused_bearer);
        let served_idx = codex_index_of(&served_bearers[0]);
        assert_ne!(
            served_idx, held_idx,
            "retry-after {retry_after:?}: the retry was served by the OTHER Codex account"
        );
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        let held = &snapshot.accounts[held_idx];
        let until = held
            .rate_limited_until
            .unwrap_or_else(|| panic!("retry-after {retry_after:?}: the refused account is held"))
            .unix_timestamp();
        assert!(
            (until - weekly_reset).abs() <= 1,
            "retry-after {retry_after:?}: held until the full weekly window's reset \
                 ({weekly_reset}), not the five-hour one ({five_hour_reset}) nor retry-after; \
                 held until {until}"
        );
        assert_eq!(
            held.gate,
            GateReason::SevenDay,
            "a refusal carrying x-codex-rate-limit-reached-type reports the full window that refused it"
        );
        assert_eq!(held.seven_day, Some(1.0), "the weekly window was learned");
        let other = &snapshot.accounts[served_idx];
        assert_eq!(
            other.rate_limited_until, None,
            "the serving account is not held"
        );
        assert_eq!(other.gate, GateReason::Ok);
    }
}

/// **A Codex JSON auth 403 holds its account briefly, as an Anthropic one does, and reads
/// `refused`, never a quota verdict.** The 403 arm is reached only for an account
/// `select` picked within the request's provider, so the hold lands on the Codex
/// account that answered, and `Manager::mark_refused` arms the flat
/// `REFUSED_HOLD_SECONDS` without reading a header. So the next three requests never
/// try the refused account again, its status stays `Active`, its gate reads `refused`
/// (not `rejected`: only a 429 carrying the reached-type header is a Codex quota
/// refusal), its hold is the short one, and the Anthropic account beside it is
/// neither held nor asked.
///
/// Watched red: let only an Anthropic 403 arm the hold, and the refused Codex
/// account is tried again and reads `ok`.
#[tokio::test]
async fn a_codex_403_holds_its_account_briefly_and_reads_refused() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let (codex_url, refused, served) = spawn_codex_origin_forbidding_the_first().await;
    let manager = Manager::with_live_refresher(
        codex_fleet_with_carol(&anthropic_echo.url, &codex_url),
        None,
    );

    for n in 0..4 {
        let (status, answer) = drive(
            Arc::clone(&manager),
            Method::POST,
            crate::provider::CODEX_RESPONSES_PATH,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("content-type", "application/json"),
            ],
            "{}",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "request {n} is served by the other Codex account: {}",
            String::from_utf8_lossy(&answer)
        );
    }
    let refused_idx = codex_index_of(
        &refused
            .lock()
            .expect("refusal record poisoned")
            .clone()
            .expect("one account was refused"),
    );
    let served_bearers = served.lock().expect("served record poisoned").clone();
    assert_eq!(served_bearers.len(), 4, "{served_bearers:?}");
    assert!(
        served_bearers
            .iter()
            .all(|bearer| codex_index_of(bearer) != refused_idx),
        "the refused account is not tried again while held: {served_bearers:?}"
    );
    assert_eq!(
        anthropic_echo.request_count(),
        0,
        "rotation never leaves Codex"
    );

    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let held = &snapshot.accounts[refused_idx];
    assert_eq!(
        held.gate,
        GateReason::Refused,
        "a Codex 403 reads refused, never rejected"
    );
    assert_eq!(
        manager.account_status(refused_idx),
        Some(AccountStatus::Active),
        "the hold is bounded, never the terminal `Error`"
    );
    let until = held
        .rate_limited_until
        .expect("the refused account is held")
        .unix_timestamp();
    let now_s = OffsetDateTime::now_utc().unix_timestamp();
    assert!(
        until <= now_s + crate::manager::REFUSED_HOLD_SECONDS
            && until > now_s + crate::manager::REFUSED_HOLD_SECONDS - 30,
        "the short refusal hold: held until {until}, {now_s} now"
    );
    let alice = &snapshot.accounts[0];
    assert_eq!(
        (alice.gate, alice.rate_limited_until),
        (GateReason::Ok, None),
        "the Anthropic account is untouched"
    );
}

/// **A Codex 429 that is NOT a quota refusal holds on `retry-after`, never on a
/// window.** It carries no `x-codex-rate-limit-reached-type`, so its window headers
/// (the weekly one full, half an hour out) say how full the account is, not why this
/// request was refused: the account is held for the 429's own `retry-after`, or the
/// 60-second default without one, its quota is NOT marked rejected, and the rotation
/// still moves on to the other Codex account.
///
/// Watched red: let `codex_hold_seconds` answer every Codex 429 again and both runs
/// hold the account to the weekly reset instead.
#[tokio::test]
async fn a_codex_429_without_a_reached_type_holds_on_retry_after_not_the_window() {
    for (retry_after, expected_hold) in [(Some("30"), 30_i64), (None, 15)] {
        let anthropic_echo = CapturingUpstream::spawn().await;
        let secs = OffsetDateTime::now_utc().unix_timestamp();
        let weekly_reset = secs + 1800;
        let (codex_url, refused, served) =
            spawn_codex_origin_refusing_the_first(None, retry_after, secs + 600, weekly_reset)
                .await;
        let manager = Manager::with_live_refresher(
            codex_fleet_with_carol(&anthropic_echo.url, &codex_url),
            None,
        );

        let before = OffsetDateTime::now_utc().unix_timestamp();
        let (status, answer) = drive(
            Arc::clone(&manager),
            Method::POST,
            crate::provider::CODEX_RESPONSES_PATH,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("content-type", "application/json"),
            ],
            "{}",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "retry-after {retry_after:?}: the other Codex account serves the request: {}",
            String::from_utf8_lossy(&answer)
        );
        let served_bearers = served.lock().expect("served record poisoned").clone();
        assert_eq!(
            served_bearers.len(),
            1,
            "served exactly once: {served_bearers:?}"
        );

        let refused_bearer = refused
            .lock()
            .expect("refusal record poisoned")
            .clone()
            .expect("one account was refused");
        let held_idx = codex_index_of(&refused_bearer);
        assert_ne!(codex_index_of(&served_bearers[0]), held_idx);
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        let held = &snapshot.accounts[held_idx];
        let until = held
            .rate_limited_until
            .unwrap_or_else(|| panic!("retry-after {retry_after:?}: the refused account is held"))
            .unix_timestamp();
        assert!(
            if retry_after.is_some() {
                (until - (before + expected_hold)).abs() <= 2
            } else {
                (13..=22).contains(&(until - before))
            },
            "retry-after {retry_after:?}: held {expected_hold}s plus optional jitter from {before}, not to the \
                 weekly reset ({weekly_reset}); held until {until}"
        );
        assert_ne!(
            held.gate,
            GateReason::Rejected,
            "a 429 without x-codex-rate-limit-reached-type is not a quota refusal"
        );
    }
}

/// **A Codex pool held only by a short 429 that is not a quota refusal is waited out,
/// not refused.** The hold such a 429 arms is a timer that expires on its own, as an
/// Anthropic no-guidance park is, so the soft-wait must be able to take the account back
/// (`parked_transient`). The one Codex account answers a 429 with `retry-after: 1` and no
/// `x-codex-rate-limit-reached-type` or window headers, then 200: the client gets the
/// 200, after exactly two sends. The origin is its own: the refusing one above reports a
/// full weekly window, which keeps the soft-wait from firing at all.
///
/// Watched red: drop the `parked_transient` insert from the Codex arm, and the soft-wait
/// sleeps out the hold, re-admits nothing and answers 429 after one send.
#[test]
fn codex_stream_burst_limits_have_short_jittered_holds() {
    for code in ["rate_limit_exceeded", "slow_down"] {
        let limit = CodexStreamLimit::from_error(&serde_json::json!({"code": code})).unwrap();
        assert!(limit.refusal.is_none());
        assert!(
            (15..=20).contains(&limit.hold_seconds),
            "{code}: {}",
            limit.hold_seconds
        );
    }
    let quota =
        CodexStreamLimit::from_error(&serde_json::json!({"type": "usage_limit_reached"})).unwrap();
    assert!(quota.refusal.is_some());
    assert_eq!(
        quota.hold_seconds, 60,
        "quota refusals retain their documented fallback"
    );
}

#[tokio::test]
async fn codex_short_burst_retry_keeps_the_same_account_when_a_sibling_is_healthy() {
    let anthropic = CapturingUpstream::spawn().await;
    let bearers = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&bearers);
    let origin = Router::new().fallback(move |headers: HeaderMap| {
        let seen = Arc::clone(&seen);
        async move {
            let first = {
                let mut seen = seen.lock().unwrap();
                seen.push(
                    headers
                        .get("authorization")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
                seen.len() == 1
            };
            if first {
                Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header(CONTENT_TYPE, "application/json")
                    .header("retry-after", "1")
                    .body(Body::from(r#"{"error":{"message":"slow down"}}"#))
                    .unwrap()
            } else {
                Response::builder()
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap()
            }
        }
    });
    let base = stub::http(origin).url();
    let manager = Manager::with_live_refresher(codex_fleet_with_carol(&anthropic.url, &base), None);
    let started = tokio::time::Instant::now();
    let (status, _) = drive(
        manager,
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
            ("session-id", "short-codex-burst"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = bearers.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0], seen[1],
        "a one-second burst must keep the warm account"
    );
    assert!(started.elapsed() >= Duration::from_millis(850));
    assert_eq!(anthropic.request_count(), 0);
}

#[tokio::test]
async fn a_codex_account_held_by_a_short_non_quota_429_serves_the_retry() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&sends);
    let origin = Router::new().fallback(move || {
        let counted = Arc::clone(&counted);
        async move {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                return Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header(CONTENT_TYPE, "application/json")
                    .header("retry-after", "1")
                    .body(Body::from(r#"{"error":{"message":"slow down"}}"#))
                    .expect("429 response");
            }
            Response::builder()
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .expect("200 response")
        }
    });
    let codex_url = stub::http(origin).url();
    let manager = Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_url), None);

    let (status, answer) = drive(
        Arc::clone(&manager),
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    let sent = sends.load(Ordering::SeqCst);
    assert_eq!(
        status,
        StatusCode::OK,
        "the soft-wait takes the held account back ({sent} send(s)): {}",
        String::from_utf8_lossy(&answer)
    );
    assert_eq!(sent, 2, "one refused send, then the one that served");
    assert_eq!(anthropic_echo.request_count(), 0);
}

/// **The eleven per-account Codex headers are stripped from a POOLED response and kept
/// on a RELAYED one**, by name, while `x-codex-turn-state`, which shares their prefix,
/// reaches the client either way. A pooled response describes whichever account served
/// it, so its quota picture and credit balance would change from request to request; a
/// relayed one describes the client's own account.
///
/// The fixture names all eleven itself, so each is pinned by what reaches the client,
/// not by the list's contents: the precondition asks only that the fixture cover every
/// name in [`crate::quota::CODEX_QUOTA_HEADERS`].
///
/// Watched red: drop the Codex names from `is_account_scoped` and the pooled response
/// carries all seven quota headers; strip the whole `x-codex-` prefix instead and it
/// loses the turn state; remove `x-codex-credits-balance` from the list and the pooled
/// response carries the serving account's balance.
#[tokio::test]
async fn codex_quota_headers_are_stripped_from_a_pooled_response_and_kept_on_a_relayed_one() {
    let secs = OffsetDateTime::now_utc().unix_timestamp();
    let quota_headers: Vec<(&'static str, String)> = vec![
        ("x-codex-primary-used-percent", "25".to_string()),
        ("x-codex-primary-window-minutes", "300".to_string()),
        ("x-codex-primary-reset-at", (secs + 3600).to_string()),
        ("x-codex-secondary-used-percent", "3".to_string()),
        ("x-codex-secondary-window-minutes", "10080".to_string()),
        ("x-codex-secondary-reset-at", (secs + 86_400).to_string()),
        ("x-codex-rate-limit-reached-type", "primary".to_string()),
        ("x-codex-credits-has-credits", "true".to_string()),
        ("x-codex-credits-unlimited", "false".to_string()),
        ("x-codex-credits-balance", "12.50".to_string()),
        ("x-codex-promo-message", "fixture promo".to_string()),
    ];
    for name in crate::quota::CODEX_QUOTA_HEADERS {
        assert!(
            quota_headers.iter().any(|(sent, _)| *sent == name),
            "precondition: the fixture sends every name in the strip list, {name} included"
        );
    }
    let answer_with = quota_headers.clone();
    let origin = Router::new().fallback(move || {
        let answer_with = answer_with.clone();
        async move {
            let mut response = Response::builder().header(CONTENT_TYPE, "application/json");
            for (name, value) in &answer_with {
                response = response.header(*name, value.as_str());
            }
            response
                .header("x-codex-turn-state", "opaque")
                .body(Body::from("{}"))
                .expect("Codex response")
        }
    });
    let codex_url = stub::http(origin).url();
    let manager = Manager::with_live_refresher(codex_fleet("http://127.0.0.1:9", &codex_url), None);
    let header = |response: &Response, name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };

    let pooled = drive_full(
        Arc::clone(&manager),
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(pooled.status(), StatusCode::OK);
    for (name, _) in &quota_headers {
        assert_eq!(
            header(&pooled, name),
            None,
            "{name} leaked from a pooled serve"
        );
    }
    assert_eq!(
        header(&pooled, "x-codex-turn-state").as_deref(),
        Some("opaque"),
        "the turn state is the client's own and must reach it"
    );

    let relayed = drive_full(
        manager,
        Method::GET,
        "/backend-api/wham/usage",
        Some(loopback_peer()),
        &[("authorization", "Bearer client-own")],
        "",
    )
    .await;
    assert_eq!(relayed.status(), StatusCode::OK);
    for (name, value) in &quota_headers {
        assert_eq!(
            header(&relayed, name).as_deref(),
            Some(value.as_str()),
            "{name} describes the client's own account on a relay and must reach it"
        );
    }
    assert_eq!(
        header(&relayed, "x-codex-turn-state").as_deref(),
        Some("opaque")
    );
}

/// **The Codex quota facts reach the snapshot from a pooled serve, though the client never
/// sees the headers that carry them.** The proxy folds the upstream headers into the serving
/// account's quota BEFORE `build_response` strips them from the pooled answer, so the
/// credits, the windows and their lengths are on bob's row and on nobody else's, and none of
/// the eleven headers reaches the client. The reached-type header rides on a 200 here, which
/// is no refusal, so no kind is reported.
///
/// Watched red: strip the eleven names from the upstream headers before `update_quota`, and
/// the credits are never read.
#[tokio::test]
async fn codex_quota_facts_reach_the_snapshot_though_the_client_never_sees_their_headers() {
    let secs = OffsetDateTime::now_utc().unix_timestamp();
    let quota_headers: Vec<(&'static str, String)> = vec![
        ("x-codex-primary-used-percent", "25".to_string()),
        ("x-codex-primary-window-minutes", "300".to_string()),
        ("x-codex-primary-reset-at", (secs + 3600).to_string()),
        ("x-codex-secondary-used-percent", "3".to_string()),
        ("x-codex-secondary-window-minutes", "10080".to_string()),
        ("x-codex-secondary-reset-at", (secs + 86_400).to_string()),
        ("x-codex-rate-limit-reached-type", "primary".to_string()),
        ("x-codex-credits-has-credits", "true".to_string()),
        ("x-codex-credits-unlimited", "false".to_string()),
        ("x-codex-credits-balance", "12.50".to_string()),
        ("x-codex-promo-message", "fixture promo".to_string()),
    ];
    for name in crate::quota::CODEX_QUOTA_HEADERS {
        assert!(
            quota_headers.iter().any(|(sent, _)| *sent == name),
            "precondition: the fixture sends every name in the strip list, {name} included"
        );
    }
    let answer_with = quota_headers.clone();
    let origin = Router::new().fallback(move || {
        let answer_with = answer_with.clone();
        async move {
            let mut response = Response::builder().header(CONTENT_TYPE, "application/json");
            for (name, value) in &answer_with {
                response = response.header(*name, value.as_str());
            }
            response.body(Body::from("{}")).expect("Codex response")
        }
    });
    let codex_url = stub::http(origin).url();
    let manager = Manager::with_live_refresher(codex_fleet("http://127.0.0.1:9", &codex_url), None);

    let pooled = drive_full(
        Arc::clone(&manager),
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(pooled.status(), StatusCode::OK);
    for (name, _) in &quota_headers {
        assert!(
            pooled.headers().get(*name).is_none(),
            "{name} leaked from a pooled serve"
        );
    }

    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let bob = &snapshot.accounts[1].codex;
    assert_eq!(
        bob.credits,
        Some(tcr_status_wire::CodexCredits {
            has_credits: true,
            unlimited: false,
            balance: Some("12.50".into()),
        }),
        "the credits were read before the strip"
    );
    assert_eq!(
        bob.quota_windows,
        Some(vec!["fiveHour".to_string(), "sevenDay".to_string()])
    );
    assert_eq!(bob.five_hour_window_minutes, Some(300));
    assert_eq!(bob.seven_day_window_minutes, Some(10_080));
    assert_eq!(
        bob.limit_reached_type, None,
        "a 200 carrying the reached-type header is no refusal"
    );
    assert_eq!(
        snapshot.accounts[0].codex,
        crate::stats::CodexFacts::default(),
        "alice, the Anthropic account, reports none of it"
    );
}

/// **History bound to an account of the OTHER provider never holds a
/// request there.** The hold benches every other account and
/// `Manager::select_bound` checks no provider, so a Claude conversation whose
/// signature was minted on a Codex account was served on the Codex account's
/// bearer, against the Anthropic origin. Dropped, the request takes the
/// ordinary pick. Both directions.
///
/// Watched red: drop the provider filter on `bound_candidate` and the
/// Anthropic origin sees `Bearer at-bob`.
#[tokio::test]
async fn codex_bound_history_never_holds_a_request_on_the_other_providers_account() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    // One token minted on each account, each naming the OTHER provider's.
    manager.record_bound_tokens(
        &[(
            BoundTokenKind::ThinkingSignature,
            "sig_fake_bob".to_string(),
        )],
        1,
        crate::now_ms(),
    );
    manager.record_bound_tokens(
        &[(
            BoundTokenKind::ThinkingSignature,
            "sig_fake_alice".to_string(),
        )],
        0,
        crate::now_ms(),
    );

    // A Claude conversation carrying the Codex account's token.
    let (status, body) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body("sess-cross", &["sig_fake_bob"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        anthropic_echo.last_request().header("authorization"),
        Some("Bearer at-alice"),
        "a Claude request is served by the Anthropic account, never on a Codex bearer"
    );
    assert_eq!(codex_echo.request_count(), 0);

    // A Codex request carrying the Anthropic account's token.
    let (status, body) = drive(
        Arc::clone(&manager),
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
        ],
        &thinking_turn_body("sess-cross-codex", &["sig_fake_alice"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        codex_echo.last_request().header("authorization"),
        Some("Bearer at-bob"),
        "a Codex request is served by the Codex account, never on an Anthropic bearer"
    );
    assert_eq!(anthropic_echo.request_count(), 1);
}

/// **The Fable weekly bucket is an Anthropic fact** (this is the third
/// `is_fable`, after `select.rs`'s two): a Codex request whose model id
/// merely contains "fable" is never timed against it. Here the Codex account
/// is out for ten minutes AND carries a Fable-weekly rejection set by hand (no
/// Codex account has one in production, since tcr reads no model-scoped
/// window from Codex; it is the only way to observe the gate). The 429 says
/// ten minutes, not the week.
///
/// Watched red: drop `provider == Anthropic` from `request_is_fable` and
/// `resets_in_seconds` reads about 604800.
#[tokio::test]
async fn codex_request_never_reads_the_fable_weekly_bucket() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    manager.mark_model_weekly_rejected(1, &HeaderMap::new());
    manager.mark_rate_limited(1, 600);
    let resp = drive_full(
        manager,
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
        ],
        r#"{"model":"gpt-6-fable"}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let bytes = to_bytes(resp.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let resets = body["error"]["resets_in_seconds"]
        .as_u64()
        .expect("a number of seconds");
    assert!(
        (590..=600).contains(&resets),
        "the hold times this Codex 429, not the Fable weekly window: {resets}"
    );
    assert_eq!(codex_echo.request_count(), 0);
}

/// **A Codex client's bearer is never sent to Anthropic.** The
/// `controlIdentity` check resolves a client bearer through
/// `/api/oauth/profile` on the ANTHROPIC upstream; on a Codex request that
/// bearer is a ChatGPT token, so the check is an Anthropic-only question and
/// a Codex request never asks it. The control is an Anthropic bookkeeping
/// call on the same manager, which does ask: without it, the zero below
/// would pass on a manager whose check never runs at all.
///
/// Watched red: drop the `provider == Anthropic` gate on the check and the
/// Anthropic origin receives `/api/oauth/profile` with `Bearer client-own`.
#[tokio::test]
async fn codex_bearer_never_reaches_the_anthropic_identity_check() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let mut config = codex_fleet(&anthropic_echo.url, &codex_echo.url);
    config.control_account = Some(config::ControlAccountRef::Name(
        "alice@example.com".to_string(),
    ));
    config.control_pooled = true;
    let manager = Manager::with_live_refresher(config, None);
    for (method, path, body) in [
        (Method::GET, crate::provider::CODEX_USAGE_PATH, ""),
        (Method::POST, crate::provider::CODEX_RESPONSES_PATH, "{}"),
    ] {
        let (status, answer) = drive(
            Arc::clone(&manager),
            method,
            path,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("content-type", "application/json"),
            ],
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&answer)
        );
    }
    assert_eq!(
        anthropic_echo.request_count(),
        0,
        "a ChatGPT bearer must never reach the Anthropic profile endpoint"
    );
    assert_eq!(codex_echo.request_count(), 2);

    // The control: an Anthropic bookkeeping call resolves the bearer first.
    let (status, _) = drive(
        Arc::clone(&manager),
        Method::GET,
        "/api/claude_cli/bootstrap",
        Some(loopback_peer()),
        &[("authorization", "Bearer client-own")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let paths: Vec<String> = anthropic_echo
        .seen
        .lock()
        .expect("upstream recorder poisoned")
        .iter()
        .map(|seen| seen.path.clone())
        .collect();
    assert!(
        paths.iter().any(|path| path == "/api/oauth/profile"),
        "control: the check is live for an Anthropic request: {paths:?}"
    );
}

/// **Each provider's exhausted 429 is timed on that provider's accounts
/// alone**, and only a Codex client gets the Codex body. Both orders:
/// whichever provider's account frees sooner, the other provider's client is
/// never told to come back then.
///
/// Watched red: hand `exhausted_response`'s fleet hint every account again and
/// the sooner hold answers both clients.
#[tokio::test]
async fn codex_exhausted_429s_are_timed_on_the_requests_own_provider() {
    use crate::provider::Provider;
    let retry_after = |response: &Response| {
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
    };
    for (alice_hold, bob_hold) in [(7_i64, 2_i64), (2, 7)] {
        let manager = Manager::with_live_refresher(
            codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9"),
            None,
        );
        manager.mark_rate_limited(0, alice_hold);
        manager.mark_rate_limited(1, bob_hold);
        // Read AFTER the marks, so each hold rounds up to exactly its own length.
        let now = OffsetDateTime::now_utc();

        let anthropic =
            exhausted_response(&manager, now, 2, false, Provider::Anthropic, None, None);
        assert_eq!(anthropic.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            retry_after(&anthropic),
            Some(alice_hold),
            "a Claude client waits for the Anthropic account"
        );
        let bytes = to_bytes(anthropic.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["error"]["type"], "rate_limit_error",
            "the Anthropic body is the one it always was"
        );

        let codex = exhausted_response(&manager, now, 2, false, Provider::Codex, None, None);
        assert_eq!(codex.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            retry_after(&codex),
            Some(bob_hold),
            "a Codex client waits for the Codex account"
        );
        let bytes = to_bytes(codex.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["error"]["type"], "pool_unavailable",
            "a pool that is only held is no usage limit: {body}"
        );
        assert_eq!(body["error"]["resets_in_seconds"], bob_hold);
    }
}

/// **The exhausted-pool 429 says WHEN in the field codex reads, when a reset was
/// measured.** codex parses an HTTP 429 in `map_api_error` (openai/codex f82e365,
/// `codex-rs/codex-api/src/api_bridge.rs`): a `usage_limit_reached` body becomes its
/// usage-limit error, whose reset time is `error.resets_at` in absolute Unix seconds,
/// printed as "Try again at <time>"; without it codex prints "Try again later". Nothing in
/// codex reads `resets_in_seconds` at that commit. For a full weekly window both fields
/// come from one instant, its reset, so `resets_at` is the 429's `now` plus
/// `resets_in_seconds`. The 60-second fallback, when no account reports a reset (one the
/// backend refused, with no window to wait on), is a wait and not a measured time: it
/// carries no `resets_at`, and its message says no account reports one. A pool out on
/// anything but a quota, a disabled account here, claims no limit at all.
///
/// Watched red: leave `resets_at` out of [`codex_exhausted_body`] and the first
/// assertion reads null; write it for the fallback too and the fallback's reads a time.
#[tokio::test]
async fn codex_exhausted_429_carries_resets_at_and_it_agrees_with_resets_in_seconds() {
    use crate::provider::Provider;
    let body_of = |response: Response| async move {
        let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        serde_json::from_slice::<Value>(&bytes).expect("a JSON body")
    };

    // The one Codex account's weekly window is full, and resets in 300 seconds.
    let manager = Manager::with_live_refresher(
        codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9"),
        None,
    );
    let now = OffsetDateTime::now_utc();
    manager.edit_quota_for_test(1, |quota| {
        quota.seven_day = Some(crate::quota::QuotaWindow {
            utilization: 1.0,
            reset: Some(now + time::Duration::seconds(300)),
        });
    });
    let body = body_of(exhausted_response(
        &manager,
        now,
        1,
        false,
        Provider::Codex,
        None,
        None,
    ))
    .await;
    assert_eq!(body["error"]["type"], "usage_limit_reached", "{body}");
    let resets_at = body["error"]["resets_at"].as_i64();
    assert_eq!(
        resets_at,
        Some(now.unix_timestamp() + 300),
        "resets_at is the instant the window resets, in Unix seconds: {body}"
    );
    assert_eq!(body["error"]["resets_in_seconds"], 300, "{body}");
    assert!(
        body["error"]["plan_type"].is_null(),
        "a pool can mix plans, so no plan is claimed: {body}"
    );

    // The backend refused the one Codex account, which has no window to wait on: the
    // 60-second fallback is a wait, so no time is claimed.
    let manager = Manager::with_live_refresher(
        codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9"),
        None,
    );
    manager.edit_quota_for_test(1, |quota| quota.status = Some("rejected".to_string()));
    let now = OffsetDateTime::now_utc();
    let body = body_of(exhausted_response(
        &manager,
        now,
        1,
        false,
        Provider::Codex,
        None,
        None,
    ))
    .await;
    assert_eq!(body["error"]["type"], "usage_limit_reached", "{body}");
    assert_eq!(body["error"]["resets_in_seconds"], 60, "{body}");
    assert!(
        body["error"].get("resets_at").is_none(),
        "no reset was measured, so codex is given no time to print: {body}"
    );
    let message = body["error"]["message"].as_str().expect("a message");
    assert!(
        message.contains("none reports when it frees"),
        "the message does not print the 60 as if it were measured: {message}"
    );

    // Disabled accounts require an operator action, not a timed retry.
    let mut config = codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9");
    config.accounts[1].disabled = Some(true);
    let manager = Manager::with_live_refresher(config, None);
    let response = exhausted_response(
        &manager,
        OffsetDateTime::now_utc(),
        1,
        false,
        Provider::Codex,
        None,
        None,
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.headers().contains_key("retry-after"));
    let body = body_of(response).await;
    assert_eq!(body["error"]["type"], "no_codex_account");
    assert!(body["error"].get("resets_in_seconds").is_none());
    assert!(body["error"].get("resets_at").is_none());
}

/// **A fleet with no Codex account answers a Codex request by saying so**,
/// and names the fix, instead of a `pool_unavailable` 429 whose 60-second
/// retry no wait will satisfy. It answers 400 in the `{"error": {...}}`
/// envelope, with no `retry-after`: waiting changes nothing. Neither origin
/// is asked.
///
/// Watched red: drop the zero-account branch from `exhausted_response` and
/// the body is `pool_unavailable` with `resets_in_seconds: 60` and a
/// `retry-after: 60`, which tells codex to wait for an account no wait will
/// add.
#[tokio::test]
async fn a_fleet_with_no_codex_account_says_so_to_a_codex_client() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let mut config = codex_fleet(&anthropic_echo.url, &codex_echo.url);
    config.accounts.remove(1);
    let manager = Manager::with_live_refresher(config, None);
    let resp = drive_full(
        manager,
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let retry_after = resp.headers().get("retry-after").cloned();
    let bytes = to_bytes(resp.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "no_codex_account", "{body}");
    assert_eq!(body["error"]["code"], "no_codex_account", "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("No Codex account is configured") && message.contains("tcr login --codex"),
        "{message}"
    );
    assert!(body["error"].get("resets_in_seconds").is_none(), "{body}");
    assert_eq!(retry_after, None, "no wait fixes a missing account");
    assert_eq!(codex_echo.request_count(), 0);
    assert_eq!(anthropic_echo.request_count(), 0);
}

/// **The Anthropic 429 counts the Anthropic accounts.** "All N accounts
/// exhausted" names the accounts that could have served the request; a Codex
/// account in the same fleet is never one of the N.
///
/// Watched red: hand the exhausted terminals the fleet-wide `account_count`
/// again and the message reads "All 3 accounts exhausted".
#[tokio::test]
async fn the_anthropic_exhausted_429_counts_only_anthropic_accounts() {
    let up_addr = spawn_scripted_upstream(vec![Some(raw_200())]).await;
    let manager = fleet_configured(up_addr, &["a", "b", "c"], |config| {
        config.accounts[2].provider = Provider::Codex;
        // A revalidation serve has nothing to serve on held accounts anyway;
        // off so the 429 below is the only possible answer.
        config
            .extra
            .insert("revalidationServe".to_string(), serde_json::json!(false));
    });
    manager.mark_rate_limited(0, 600);
    manager.mark_rate_limited(1, 600);
    let (status, _, body) = post_one_full(manager).await;
    assert_eq!(status, 429, "{body}");
    assert!(
        body.contains("All 2 accounts exhausted"),
        "only the two Anthropic accounts are counted: {body}"
    );
}
