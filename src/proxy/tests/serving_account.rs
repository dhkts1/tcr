//! What the serving account leaves on a response: its account headers stripped, its uuid patched in, and its 429s.

use super::*;

/// G1 (best-practices review, Apollo ch5.3 integration): `patch_account_uuid`
/// is thoroughly UNIT-tested, but the proxy wiring at the call site is not — every
/// other integration test posts a bare `{}`, so the rewrite branch never fires on
/// a real request. Drive one whose `metadata.user_id` blob carries a DIFFERENT
/// `account_uuid` and assert, from what the UPSTREAM actually received, that the
/// serving account's uuid was substituted, the client's is gone, and the body
/// length is unchanged (the whole point of the same-length patch — it keeps the
/// buffered request's Content-Length valid in flight).
#[tokio::test]
async fn account_uuid_is_rewritten_to_the_serving_account_on_a_real_request() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const CLIENT_UUID: &str = "11111111-1111-1111-1111-111111111111";
    const ACCT_UUID: &str = "22222222-2222-2222-2222-222222222222"; // both 36 chars

    struct NoRefresh;
    impl crate::oauth::TokenRefresher for NoRefresh {
        fn refresh(&self, _provider: Provider, _t: String) -> crate::oauth::RefreshFuture {
            Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
        }
    }

    // Echo upstream: capture the full forwarded request (headers + Content-Length
    // body), then answer 200.
    let captured = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
    let cap = Arc::clone(&captured);
    let up_addr = stub::raw(move |conn| {
        let cap = Arc::clone(&cap);
        async move {
            let mut sock = conn.stream;
            // What the shared listener already read to route the request.
            let mut buf = conn.head;
            let mut tmp = [0u8; 4096];
            loop {
                if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..h]).to_lowercase();
                    let cl = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + cl {
                        break;
                    }
                }
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            *cap.lock().await = buf;
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await
                .log_debug("writing a test response");
        }
    });

    let mut config = dummy_config(None, &format!("http://{up_addr}"));
    config.accounts[0].account_uuid = Some(ACCT_UUID.to_string());
    let manager = Manager::new(
        config,
        Arc::new(NoRefresh),
        Arc::new(crate::probe::LiveUsageProber::new()),
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );
    let inner = format!(r#"{{"account_uuid":"{CLIENT_UUID}","subscriptionType":"pro"}}"#);
    let body = serde_json::json!({
        "model": "claude-x",
        "metadata": { "user_id": inner },
        "messages": [],
    })
    .to_string();
    let sent_len = body.len();

    drop(
        send_to_proxy(
            app(Arc::clone(&manager)),
            client_request(Method::POST, "/v1/messages", body),
        )
        .await,
    );

    let received = captured.lock().await.clone();
    assert!(
        !received.is_empty(),
        "upstream received the forwarded request"
    );
    let body_at = received
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .expect("request has a header/body separator");
    let up_body = &received[body_at..];
    let up_body_str = String::from_utf8_lossy(up_body);
    assert!(
        up_body_str.contains(ACCT_UUID),
        "the serving account's uuid must be injected into the forwarded body"
    );
    assert!(
        !up_body_str.contains(CLIENT_UUID),
        "the client's account_uuid must be overwritten, not forwarded"
    );
    assert_eq!(
        up_body.len(),
        sent_len,
        "the same-length patch must preserve the body length so Content-Length stays valid"
    );
}

/// End-to-end affinity + cache surfacing over a REAL 2-account fleet: two
/// requests carrying the SAME `metadata.user_id` must pin to ONE account
/// (requests 2 / 0 — the seam no unit test spans), and that account's snapshot
/// must expose the upstream's `cache_read_input_tokens` separately (not summed
/// away into the quota total). The `SessionKey` extension is injected by a
/// layer here exactly as the hybrid CONNECT server (`mitm.rs`) does per
/// connection — `app()` alone injects none, so without it affinity is inert.
#[tokio::test]
async fn same_user_id_pins_one_account_and_surfaces_cache_read() {
    struct NoRefresh;
    impl crate::oauth::TokenRefresher for NoRefresh {
        fn refresh(&self, _provider: Provider, _t: String) -> crate::oauth::RefreshFuture {
            Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
        }
    }

    // Fake upstream: every connection gets a 200 whose JSON `usage` carries a
    // non-zero `cache_read_input_tokens` (a warm-cache turn). Loops so BOTH
    // forwarded requests are answered.
    let body = r#"{"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":500,"output_tokens":20}}"#;
    let up_addr = stub::canned(stub::reply("200 OK", "application/json", body));

    // A 2-account fleet pointed at the one fake upstream.
    let mut config = dummy_config(None, &format!("http://{up_addr}"));
    let mut second = config.accounts[0].clone();
    second.name = "dummy2".to_string();
    config.accounts.push(second);
    let manager = Manager::new(
        config,
        Arc::new(NoRefresh),
        Arc::new(crate::probe::LiveUsageProber::new()),
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );

    // Inject a DISTINCT per-request SessionKey the way mitm.rs does (its
    // `next_session_key()` = `session_seq.fetch_add(1)` hands every connection
    // a unique key). Distinct fallback keys are load-bearing to the proof: if
    // `metadata.user_id` extraction were dead, key resolution would fall back
    // to these distinct per-connection keys and the two requests would SPREAD
    // to 2 accounts (test fails). Only the SHARED `user_id` "cache-affinity-user"
    // resolving to one stable key can pin them 2/0 — so a green test proves the
    // body's user_id is what pins, not merely affinity-on.
    use std::sync::atomic::{AtomicU64, Ordering};
    let served = Arc::clone(&manager);
    let seq = Arc::new(AtomicU64::new(1));
    let affinity_app = app(served).layer(axum::middleware::from_fn(
        move |mut req: Request, next: axum::middleware::Next| {
            let seq = Arc::clone(&seq);
            async move {
                req.extensions_mut()
                    .insert(SessionKey(seq.fetch_add(1, Ordering::Relaxed)));
                next.run(req).await
            }
        },
    ));
    let body = serde_json::json!({
        "model": "claude-x",
        "metadata": { "user_id": "cache-affinity-user" },
        "messages": [],
    })
    .to_string();

    for _ in 0..2 {
        drop(
            send_to_proxy(
                affinity_app.clone(),
                client_request(Method::POST, "/v1/messages", body.clone()),
            )
            .await,
        );
    }

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let reqs: Vec<u64> = snap.accounts.iter().map(|a| a.requests).collect();
    // Affinity end-to-end: one account served BOTH, the other served NONE.
    assert!(
        reqs.contains(&2),
        "one account must serve both same-user_id requests, got {reqs:?}"
    );
    assert!(
        reqs.contains(&0),
        "the other account must serve none (affinity pinned the session), got {reqs:?}"
    );
    // Cache surfacing: the serving account exposes the retained cache-read
    // tokens (500 per request x2), NOT summed away into the quota total.
    let serving = snap
        .accounts
        .iter()
        .find(|a| a.requests == 2)
        .expect("a serving account exists");
    assert!(
        serving.cache_read_tokens > 0,
        "cache-read must be surfaced on the serving account"
    );
    assert_eq!(
        serving.cache_read_tokens, 1000,
        "500 cache-read per request, twice"
    );
}

/// A real model-scoped Fable rejection. The overall header is rejected, but
/// both shared windows remain allowed, so only the Fable weekly scope binds.
fn raw_429_fable_rejected(retry_after: u32) -> String {
    format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\
             retry-after: {retry_after}\r\n\
             anthropic-ratelimit-unified-status: rejected\r\n\
             anthropic-ratelimit-unified-5h-status: allowed\r\n\
             anthropic-ratelimit-unified-7d-status: allowed_warning\r\n\
             anthropic-ratelimit-unified-7d_oi-status: rejected\r\n\r\n"
    )
}

/// A successful response that supplies live allowed evidence for every scope.
fn raw_200_all_scopes_allowed() -> String {
    let body = br#"{"usage":{"input_tokens":1,"output_tokens":1}}"#;
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\
             anthropic-ratelimit-unified-status: allowed\r\n\
             anthropic-ratelimit-unified-5h-status: allowed\r\n\
             anthropic-ratelimit-unified-7d-status: allowed\r\n\
             anthropic-ratelimit-unified-7d_oi-status: allowed\r\n\r\n{}",
        body.len(),
        String::from_utf8_lossy(body)
    )
}

/// The per-account header block a real Anthropic response carries, as raw
/// header lines. Includes `requests-remaining` and `tokens-limit` alongside the
/// `unified-*` family precisely because the proxy itself only reads `unified-*`:
/// a strip that enumerated the names it knows would forward these two.
const ACCOUNT_HEADER_LINES: &str = "anthropic-ratelimit-unified-status: allowed\r\n\
         anthropic-ratelimit-unified-5h-utilization: 0.42\r\n\
         anthropic-ratelimit-unified-7d-utilization: 1.0\r\n\
         anthropic-ratelimit-unified-7d-reset: 1800000000\r\n\
         anthropic-ratelimit-requests-remaining: 7\r\n\
         anthropic-ratelimit-tokens-limit: 40000\r\n\
         anthropic-organization-id: org-01234567\r\n";

/// A `200 OK` carrying the full per-account header set, plus the two headers
/// that must SURVIVE: `request-id` (a request id, not an account id) and an
/// ordinary `anthropic-version` standing in for every unrelated header.
fn raw_200_with_account_headers() -> String {
    let body = br#"{"usage":{"input_tokens":1,"output_tokens":1}}"#;
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n{ACCOUNT_HEADER_LINES}\
             request-id: req_011CabcdEFGH\r\nanthropic-version: 2023-06-01\r\n\r\n{}",
        body.len(),
        String::from_utf8_lossy(body)
    )
}

/// [`raw_429_rejected`] carrying the per-account headers a live rejection does.
/// The quota model is built from exactly these, so this is the reply that shows
/// whether the client-boundary strip blinded it. `unified-status` is `rejected`
/// here, so it cannot share [`ACCOUNT_HEADER_LINES`]'s `allowed`.
fn raw_429_rejected_with_account_headers(retry_after: u32) -> String {
    format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\
             retry-after: {retry_after}\r\n\
             anthropic-ratelimit-unified-status: rejected\r\n\
             anthropic-ratelimit-unified-5h-utilization: 0.42\r\n\
             anthropic-ratelimit-unified-7d-utilization: 1.0\r\n\
             anthropic-ratelimit-unified-7d-reset: 1800000000\r\n\
             anthropic-ratelimit-requests-remaining: 7\r\n\
             anthropic-ratelimit-tokens-limit: 40000\r\n\
             anthropic-organization-id: org-01234567\r\n\r\n"
    )
}

/// The every-account-scoped header block the upstream in these tests sends, by
/// name. Spelled out here — rather than derived from [`is_account_scoped`] — so
/// the assertions describe the WIRE, not the predicate under test.
const LEAKED_HEADER_NAMES: &[&str] = &[
    "anthropic-ratelimit-unified-status",
    "anthropic-ratelimit-unified-5h-utilization",
    "anthropic-ratelimit-unified-7d-utilization",
    "anthropic-ratelimit-unified-7d-reset",
    "anthropic-ratelimit-requests-remaining",
    "anthropic-ratelimit-tokens-limit",
    "anthropic-organization-id",
];

/// The classifier alone. The `request-id` and `anthropic-version` negatives are
/// the point: a prefix loose enough to swallow either would strip the one id
/// that makes a failed call traceable.
#[test]
fn is_account_scoped_matches_the_family_and_nothing_else() {
    for name in LEAKED_HEADER_NAMES {
        assert!(is_account_scoped(name), "{name} belongs to the account");
    }
    // Not yet invented, but the prefix must already cover it.
    assert!(is_account_scoped("anthropic-ratelimit-something-new"));
    for name in [
        "request-id",
        "anthropic-version",
        "content-type",
        "retry-after",
        "anthropic-organization",  // not the id
        "x-anthropic-ratelimit-a", // the family is not a substring match
    ] {
        assert!(!is_account_scoped(name), "{name} must survive");
    }
}

/// THE REPORTED BUG: a rotated response must reach the client carrying NONE of
/// the serving account's quota or org headers.
///
/// Claude Code renders its usage banner from `anthropic-ratelimit-unified-*`,
/// and with 13 accounts in rotation those headers describe a different account
/// on every request — the upstream here reports `7d-utilization: 1.0`, i.e. the
/// exact shape that made a freshly-opened session show a usage-limit banner.
#[tokio::test]
async fn rotated_response_strips_every_account_header() {
    let up_addr = spawn_scripted_upstream(vec![Some(raw_200_with_account_headers())]).await;
    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
    let response = drive_messages(manager).await;

    assert_eq!(response.status(), StatusCode::OK);
    for name in LEAKED_HEADER_NAMES {
        assert!(
            response.headers().get(*name).is_none(),
            "{name} describes the serving account, not the caller"
        );
    }
    let leaked: Vec<&str> = response
        .headers()
        .keys()
        .map(HeaderName::as_str)
        .filter(|k| k.starts_with("anthropic-ratelimit"))
        .collect();
    assert!(
        leaked.is_empty(),
        "no rate-limit header may survive, by prefix: {leaked:?}"
    );
}

/// The over-stripping guard. `request-id` identifies a REQUEST — it is what
/// makes one failed call traceable to Anthropic — and every unrelated upstream
/// header is still the client's to see. Their presence is also what proves the
/// test above can fail: these arrived over the same wire.
#[tokio::test]
async fn rotated_response_keeps_request_id_and_unrelated_headers() {
    let up_addr = spawn_scripted_upstream(vec![Some(raw_200_with_account_headers())]).await;
    let manager =
        Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
    let response = drive_messages(manager).await;

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    assert_eq!(
        header("request-id").as_deref(),
        Some("req_011CabcdEFGH"),
        "request-id identifies a request, not an account"
    );
    assert_eq!(
        header("anthropic-version").as_deref(),
        Some("2023-06-01"),
        "an unrelated upstream header is untouched"
    );
    assert_eq!(header("content-type").as_deref(), Some("application/json"));
}

/// THE REGRESSION GUARD THAT MATTERS: the strip is at the CLIENT boundary, so
/// everything upstream of it still sees the untouched headers.
///
/// A durable `429` must still (a) set the account's quota status to `rejected`,
/// which is the red REJECTED state in the TUI and the hard gate that re-keys a
/// session pin, (b) arm the rate-limit hold, and (c) fold the reported weekly
/// utilization into the quota model. Strip at ingest instead and all three go
/// silently dark while the client-facing bug still looks fixed.
#[tokio::test]
async fn durable_429_still_rejects_the_account_after_the_strip() {
    let up_addr =
        spawn_scripted_upstream(vec![Some(raw_429_rejected_with_account_headers(120))]).await;
    let manager = fleet(up_addr, &["a"]);
    let response = drive_messages(Arc::clone(&manager)).await;

    // The only account is held out, so the client gets the synthesized
    // fleet-exhausted 429 — which carries no upstream headers by construction.
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    for name in LEAKED_HEADER_NAMES {
        assert!(
            response.headers().get(*name).is_none(),
            "{name} is stripped"
        );
    }

    let account = &manager.snapshot(OffsetDateTime::now_utc()).accounts[0];
    assert_eq!(
        account.gate,
        GateReason::Rejected,
        "`update_quota` must still have seen `unified-status: rejected`"
    );
    assert!(
        account.rate_limited_until.is_some(),
        "the durable-rejection hold must still be armed"
    );
    assert_eq!(
        account.seven_day,
        Some(1.0),
        "the reported weekly utilization must still reach the quota model"
    );
}

/// Reproduce the live mixed-model sequence through both HTTP hops. A Fable
/// rejection must not park Opus, and later allowed evidence must restore Fable.
#[tokio::test]
async fn fable_rejection_is_scoped_and_live_allowed_evidence_clears_it() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        Some(raw_429_fable_rejected(120)),
        Some(raw_200_all_scopes_allowed()),
        Some(raw_200()),
    ])
    .await;
    let manager = fleet(up_addr, &["a"]);

    assert_eq!(
        post_model(Arc::clone(&manager), "claude-fable-5").await,
        429,
        "the active Fable rejection must block a Fable request"
    );
    assert_eq!(
        post_model(Arc::clone(&manager), "claude-opus-4-6").await,
        200,
        "the Fable-only rejection must not block Opus"
    );
    assert_eq!(
        post_model(Arc::clone(&manager), "claude-fable-5").await,
        200,
        "the allowed response must clear the stale Fable rejection"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "each client request must make exactly one upstream attempt"
    );
}

/// A locked request cannot rotate to a different account after rejection.
#[tokio::test]
async fn locked_account_stops_after_its_quota_rejection() {
    let (up_addr, attempts) =
        spawn_counted_upstream(vec![Some(raw_429_rejected(120)), Some(raw_200())]).await;
    let manager = fleet_configured(up_addr, &["a", "b"], |config| {
        config.lock_account = Some("a".to_string());
    });

    assert_eq!(post_one(manager).await, 429);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "the lock must prevent a retry on account b"
    );
}

/// An upstream `429` is logged with upstream's own words and with the client
/// that asked. Before this, the 429 arm rotated without reading the body, so
/// a 429 carrying no quota headers left nothing in the log to say why, and
/// nothing said which client's requests kept drawing it.
///
/// Runs the request up to three times for the reason written out on
/// `src/server.rs`'s `boot_line_carries_every_new_knob_with_its_configured_value`:
/// other 429 tests in this binary reach these log callsites on threads with
/// no subscriber, `tracing` caches `Interest::never` for them process-wide,
/// and this sink then stays empty however long anything waits (measured
/// here 2026-09-24: passes alone, empty sink under the parallel suite, still
/// empty with one rebuild). A rebuild before each attempt covers every
/// callsite registered by the attempt before it. Only a MISSING line is
/// retried; a line that is present with the wrong content fails at once.
#[tokio::test(flavor = "current_thread")]
async fn upstream_429_logs_its_body_and_the_asking_client() {
    const ATTEMPTS: usize = 3;
    let body =
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"probe-429-message"}}"#;
    let raw = format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\nretry-after: 120\r\n\
             anthropic-ratelimit-unified-status: rejected\r\n\r\n{body}",
        body.len()
    );

    let mut log = String::new();
    for _ in 0..ATTEMPTS {
        let (up_addr, _) = spawn_counted_upstream(vec![Some(raw.clone())]).await;
        let manager = fleet_configured(up_addr, &["a"], |config| {
            config.lock_account = Some("a".to_string());
        });
        let sink = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        let response = drive_full(
            manager,
            Method::POST,
            "/v1/messages",
            Some(loopback_peer()),
            &[
                ("content-type", "application/json"),
                ("user-agent", "probe-client/9.9"),
            ],
            r#"{"model":"claude-probe-model"}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        log = sink.contents();
        let error_line = log.lines().find(|l| l.contains("upstream error response"));
        let diagnostic = log.lines().find(|l| l.contains("429 diagnostic"));
        let (Some(error_line), Some(diagnostic)) = (error_line, diagnostic) else {
            continue;
        };
        assert!(
            error_line.contains("status=429")
                && error_line.contains(r#"error.message="probe-429-message""#),
            "the 429 body must be logged as an upstream error response, got: {log:?}"
        );
        assert!(
            diagnostic.contains(r#"user_agent="probe-client/9.9""#)
                && diagnostic.contains(r#"model="claude-probe-model""#),
            "the 429 diagnostic must name the asking client and model, got: {log:?}"
        );
        return;
    }
    panic!(
        "no attempt of {ATTEMPTS} logged both an `upstream error response` and a \
             `429 diagnostic` line for the upstream 429; last log: {log:?}"
    );
}

/// Refusal diagnostics keep the allowlisted upstream evidence and distinguish a full
/// body from a capped prefix, without logging request credentials, cookies or prompts.
#[tokio::test(flavor = "current_thread")]
async fn codex_refusal_logs_only_allowlisted_evidence() {
    for (status, padding) in [
        (StatusCode::TOO_MANY_REQUESTS, 0),
        (StatusCode::TOO_MANY_REQUESTS, ERROR_BODY_LOG_CAP),
        (StatusCode::FORBIDDEN, 0),
    ] {
        let body = if status == StatusCode::FORBIDDEN {
            "<html>evidence-403-challenge</html>".to_string()
        } else {
            serde_json::json!({
                "error": {
                    "type": "usage_limit_reached",
                    "message": "evidence-429-message",
                    "resets_at": 4_102_444_800_i64,
                },
                "padding": "x".repeat(padding),
            })
            .to_string()
        };
        let raw = format!(
            "HTTP/1.1 {} evidence\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\nretry-after: 120\r\n\
                 x-codex-primary-used-percent: 100\r\n\
                 x-codex-primary-window-minutes: 300\r\n\
                 x-codex-primary-reset-at: 4102444800\r\n\
                 x-codex-secondary-used-percent: 35\r\n\
                 x-codex-secondary-window-minutes: 10080\r\n\
                 x-codex-secondary-reset-at: 4102531200\r\n\
                 x-codex-rate-limit-reached-type: credits\r\n\
                 x-codex-credits-has-credits: false\r\n\
                 x-codex-credits-unlimited: false\r\n\
                 x-codex-credits-balance: 0.00\r\n\
                 cf-mitigated: challenge\r\ncf-ray: fixture-ray\r\nserver: cloudflare\r\n\
                 request-id: fixture-request\r\nx-request-id: fixture-x-request\r\n\
                 authorization: Bearer fixture-response-secret\r\n\
                 cookie: fixture-response-cookie\r\nset-cookie: fixture-set-cookie\r\n\r\n{body}",
            status.as_u16(),
            body.len(),
        );
        let mut log = String::new();
        let mut captured = false;
        // Match the existing upstream log test's protection against tracing's shared
        // callsite cache when other tests first visit a site without a subscriber.
        for _ in 0..3 {
            let (up_addr, _) = spawn_counted_upstream(vec![Some(raw.clone())]).await;
            let mut config = codex_fleet("http://127.0.0.1:9", &format!("http://{up_addr}"));
            config.lock_account = Some("bob@example.com/codex".into());
            config
                .extra
                .insert("revalidationServe".into(), serde_json::json!(false));
            let manager = Manager::with_live_refresher(config, None);
            let sink = SharedBuf::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(sink.clone())
                .with_ansi(false)
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            tracing::callsite::rebuild_interest_cache();
            let response = drive_full(
                manager,
                Method::POST,
                crate::provider::CODEX_RESPONSES_PATH,
                Some(loopback_peer()),
                &[
                    ("content-type", "application/json"),
                    ("authorization", "Bearer fixture-client-secret"),
                    ("cookie", "fixture-client-cookie"),
                ],
                r#"{"model":"codex-evidence-model","input":"fixture-private-prompt"}"#,
            )
            .await;
            assert_eq!(response.status(), status);
            log = sink.contents();
            let error_line = log
                .lines()
                .find(|line| line.contains("upstream error response"));
            let marker = format!("{} diagnostic", status.as_u16());
            let diagnostic = log.lines().find(|line| line.contains(&marker));
            let (Some(error_line), Some(diagnostic)) = (error_line, diagnostic) else {
                continue;
            };
            if status == StatusCode::TOO_MANY_REQUESTS {
                for evidence in [
                    r#"codex_primary_used_percent="100""#,
                    r#"codex_primary_window_minutes="300""#,
                    r#"codex_primary_reset_at="4102444800""#,
                    r#"codex_secondary_used_percent="35""#,
                    r#"codex_secondary_window_minutes="10080""#,
                    r#"codex_secondary_reset_at="4102531200""#,
                    r#"codex_rate_limit_reached_type="credits""#,
                    r#"codex_credits_has_credits="false""#,
                    r#"codex_credits_unlimited="false""#,
                    r#"codex_credits_balance="0.00""#,
                ] {
                    assert!(diagnostic.contains(evidence), "missing {evidence}: {log}");
                }
            }
            for evidence in [
                r#"content_type="application/json""#,
                r#"cf_mitigated="challenge""#,
                r#"cf_ray="fixture-ray""#,
                r#"server="cloudflare""#,
                r#"request_id="fixture-request""#,
                r#"x_request_id="fixture-x-request""#,
            ] {
                assert!(diagnostic.contains(evidence), "missing {evidence}: {log}");
            }
            assert!(
                error_line.contains(&format!("body_length={}", body.len())),
                "{log}"
            );
            assert!(
                error_line.contains(&format!(
                    "body_truncated={}",
                    body.len() > ERROR_BODY_LOG_CAP
                )),
                "{log}"
            );
            for private in [
                "fixture-client-secret",
                "fixture-response-secret",
                "at-bob",
                "fixture-client-cookie",
                "fixture-response-cookie",
                "fixture-set-cookie",
                "fixture-private-prompt",
            ] {
                assert!(
                    !log.contains(private),
                    "private request/header data reached the log: {private}"
                );
            }
            captured = true;
            break;
        }
        assert!(captured, "refusal evidence was never logged: {log}");
    }
}

/// A `429` with no quota guidance at all (no `retry-after`, no `unified-*`
/// header) and upstream's own error body. A direct API client draws this shape
/// on `/v1/messages` without `?beta=true`.
fn raw_429_no_guidance() -> String {
    stub::reply(
        "429 Too Many Requests",
        "application/json",
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"upstream-own-words"}}"#,
    )
}

/// The request-scoped arm: a no-guidance 429 on a request WITHOUT `?beta=true`
/// reaches the client with upstream's own body after ONE upstream attempt, and
/// no account is parked. Before the fix the same reply parked the account and
/// rotated, so account `b` answered and one direct client could walk the whole
/// fleet into "exhausted" for everyone else.
#[tokio::test]
async fn a_no_guidance_429_without_the_beta_flag_is_forwarded_and_parks_nothing() {
    let (up_addr, attempts) =
        spawn_counted_upstream(vec![Some(raw_429_no_guidance()), Some(raw_200())]).await;
    let manager = fleet_configured(up_addr, &["a", "b"], |_| {});

    let response = drive_full(
        Arc::clone(&manager),
        Method::POST,
        "/v1/messages",
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"model":"claude-opus-5-5","messages":[]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the forwarded body");
    assert!(
        String::from_utf8_lossy(&body).contains("upstream-own-words"),
        "the client must get upstream's own 429 body, not a synthesized one: {body:?}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "a request-scoped 429 must not rotate to another account"
    );
    let parked: Vec<_> = manager
        .snapshot(OffsetDateTime::now_utc())
        .accounts
        .into_iter()
        .filter(|a| a.rate_limited_until.is_some())
        .map(|a| a.name)
        .collect();
    assert!(
        parked.is_empty(),
        "no account may be parked, got {parked:?}"
    );
}

/// The other side of the line: the SAME no-guidance 429 on a Claude Code
/// request (`?beta=true`) keeps the park-and-rotate handling, because nothing
/// measured says those are about the request.
#[tokio::test]
async fn a_no_guidance_429_with_the_beta_flag_still_parks_and_rotates() {
    let (up_addr, attempts) =
        spawn_counted_upstream(vec![Some(raw_429_no_guidance()), Some(raw_200())]).await;
    let manager = fleet_configured(up_addr, &["a", "b"], |_| {});

    let response = drive_full(
        Arc::clone(&manager),
        Method::POST,
        "/v1/messages?beta=true",
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"model":"claude-opus-5-5","messages":[]}"#,
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the second account answers"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let parked = manager
        .snapshot(OffsetDateTime::now_utc())
        .accounts
        .into_iter()
        .filter(|a| a.rate_limited_until.is_some())
        .count();
    assert_eq!(
        parked, 1,
        "the account that answered 429 is parked, as before"
    );
}

#[test]
fn request_scoped_429_needs_no_guidance_and_no_beta_flag() {
    let none = HeaderMap::new();
    let mut retry = HeaderMap::new();
    retry.insert("retry-after", HeaderValue::from_static("5"));
    let mut unified = HeaderMap::new();
    unified.insert(
        "anthropic-ratelimit-unified-status",
        HeaderValue::from_static("allowed"),
    );

    assert!(is_request_scoped_429("/v1/messages", &none, false));
    assert!(is_request_scoped_429("/v1/messages?foo=1", &none, false));
    assert!(is_request_scoped_429(
        "/v1/messages?beta=false",
        &none,
        false
    ));
    assert!(!is_request_scoped_429(
        "/v1/messages?beta=true",
        &none,
        false
    ));
    assert!(!is_request_scoped_429(
        "/v1/messages?x=1&beta=true",
        &none,
        false
    ));
    assert!(!is_request_scoped_429("/v1/messages", &retry, false));
    assert!(!is_request_scoped_429("/v1/messages", &unified, false));
    assert!(!is_request_scoped_429("/v1/messages", &none, true));
}

/// The relay paths are served with the CALLER's own credential, so their
/// rate-limit and org headers describe the caller and are coherent. Stripping
/// them would hide the caller's real quota from them — the inverse of the bug.
#[tokio::test]
async fn relay_response_keeps_its_account_headers() {
    for path in [
        "/v1/code/session/abc",
        "/api/oauth/files/file_0123",
        "/api/oauth/file_upload",
    ] {
        let up_addr = spawn_scripted_upstream(vec![Some(raw_200_with_account_headers())]).await;
        let manager =
            Manager::with_live_refresher(dummy_config(None, &format!("http://{up_addr}")), None);
        let response = drive_full(
            manager,
            Method::POST,
            path,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own-token"),
                ("content-type", "application/json"),
            ],
            "{}",
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK, "{path} was relayed");
        for name in LEAKED_HEADER_NAMES {
            assert!(
                response.headers().get(*name).is_some(),
                "{name} is the CALLER's own on {path} and must pass through"
            );
        }
    }
}

/// One rotated `/v1/messages` through the router, returning the whole response.
async fn drive_messages(manager: Arc<Manager>) -> Response {
    drive_full(
        manager,
        Method::POST,
        "/v1/messages",
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"model":"claude-sonnet-5","messages":[]}"#,
    )
    .await
}
