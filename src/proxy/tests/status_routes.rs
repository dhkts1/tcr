//! The `GET /_tcr/status` and `GET /_tcr/quota` routes and the `POST /_tcr/peer/dial` splice.

use super::*;

// --- GET /_tcr/status ---------------------------------------------------

// --- POST /_tcr/peer/dial -----------------------------------------------

/// A request to the peer dial route, with the upgrade and target headers,
/// from `peer`, through the router.
async fn peer_dial_request(
    manager: Arc<Manager>,
    peer: SocketAddr,
    api_key: Option<&str>,
) -> StatusCode {
    use tower::ServiceExt as _;
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(PEER_DIAL_PATH)
        .header("connection", "upgrade")
        .header("upgrade", PEER_DIAL_UPGRADE)
        .header(PEER_DIAL_TARGET_HEADER, "100.64.0.9:7755");
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    let mut req = builder.body(Body::empty()).expect("build request");
    req.extensions_mut().insert(ClientAddr(peer));
    app(manager)
        .oneshot(req)
        .await
        .expect("router response")
        .status()
}

/// **The peer dial route answers loopback callers holding the key, and
/// nobody else**: a caller off this Mac is 403, a keyless local one 401,
/// before the route reads anything else.
///
/// Watch it fail by deleting the `local_endpoint_gate` call in
/// `peer_dial_handler`: both refusals then become the route's own 400
/// (the router-driven request has no connection to upgrade).
#[tokio::test]
async fn peer_dial_refuses_a_caller_that_is_not_local() {
    let manager = Manager::with_live_refresher(
        dummy_config(Some("secret-proxy-key"), "http://127.0.0.1:1"),
        None,
    );
    assert_eq!(
        peer_dial_request(
            Arc::clone(&manager),
            remote_peer(),
            Some("secret-proxy-key")
        )
        .await,
        StatusCode::FORBIDDEN,
        "a caller off this Mac is refused even with the key"
    );
    assert_eq!(
        peer_dial_request(Arc::clone(&manager), loopback_peer(), None).await,
        StatusCode::UNAUTHORIZED,
        "a local caller without the key is refused"
    );
    assert_eq!(
        peer_dial_request(manager, loopback_peer(), Some("secret-proxy-key")).await,
        StatusCode::BAD_REQUEST,
        "a local caller with the key reaches the route, which wants a real connection"
    );
}

/// Clears the process's node slot when a test that set it ends, pass or
/// fail.
struct ClearSlot;

impl Drop for ClearSlot {
    fn drop(&mut self) {
        crate::peer::tailnet_node::server_slot().set(None);
    }
}

/// **Through the real listener, a dial carries bytes both ways and ends
/// both ways**: the CLI's half (`dial_through_server`) gets a `101` and a
/// byte stream; what it writes reaches the far end the node dialled, what
/// the far end writes comes back; and when the CLI hangs up, the far end
/// sees the stream end rather than waiting on a half-close the node cannot
/// carry.
///
/// Watch it fail by serving the hybrid listener without upgrades
/// (`serve_connection` in `mitm::serve_http`): the dial then never gets
/// its stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_dial_splices_bytes_both_ways_through_the_listener() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (far_tx, mut far_rx) =
        tokio::sync::mpsc::unbounded_channel::<(SocketAddr, tokio::io::DuplexStream)>();
    let dial: crate::peer::tailnet_node::DialFn = Arc::new(move |addr| {
        let far_tx = far_tx.clone();
        Box::pin(async move {
            let (ours, theirs) = tokio::io::duplex(64 * 1024);
            far_tx
                .send((addr, theirs))
                .map_err(|_| anyhow::anyhow!("the test stopped listening"))?;
            Ok(Box::new(ours) as crate::peer::serve::PeerStream)
        })
    });
    let _clear = ClearSlot;
    let peers_dir = tempfile::tempdir().expect("tempdir");
    crate::peer::tailnet_node::server_slot().set(Some(crate::peer::tailnet_node::Published {
        dial,
        listen: "100.64.0.7:7755".parse().expect("addr"),
        peers: peers_dir.path().join("tcr-peers.json"),
    }));

    let manager = Manager::with_live_refresher(
        dummy_config(Some("secret-proxy-key"), "http://127.0.0.1:1"),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

    let target: SocketAddr = "100.64.0.9:7755".parse().expect("addr");
    let mut cli = crate::peer::tailnet_node::dial_through_server(
        port,
        Some("secret-proxy-key"),
        target,
        Duration::from_secs(5),
    )
    .await
    .expect("the running tcr dials and answers 101");
    let (dialled, mut far) = tokio::time::timeout(Duration::from_secs(5), far_rx.recv())
        .await
        .expect("the node was asked to dial")
        .expect("a far end");
    assert_eq!(dialled, target);

    cli.write_all(b"ping").await.expect("write");
    let mut got = [0_u8; 4];
    far.read_exact(&mut got).await.expect("far read");
    assert_eq!(&got, b"ping");
    far.write_all(b"pong").await.expect("far write");
    cli.read_exact(&mut got).await.expect("cli read");
    assert_eq!(&got, b"pong");

    drop(cli);
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), far.read_to_end(&mut rest))
        .await
        .expect("the far end sees the CLI hang up")
        .expect("read");
    assert!(rest.is_empty());

    // Gil, 2026-10-20: a port that is not a peer port is refused, as an
    // answer the CLI reads, and the node is never asked to dial it.
    let refused = crate::peer::tailnet_node::dial_through_server(
        port,
        Some("secret-proxy-key"),
        "100.64.0.9:22".parse().expect("addr"),
        Duration::from_secs(5),
    )
    .await
    .err()
    .map(|err| format!("{err:#}"))
    .expect("port 22 is refused");
    assert!(
        refused.contains("403") && refused.contains("port 22"),
        "{refused}"
    );
    assert!(
        far_rx.try_recv().is_err(),
        "the node was not asked to dial port 22"
    );
}

/// The endpoint ALWAYS says what this process's MITM listener can intercept,
/// MITM off included, because on the wire the key's absence is what marks a
/// server older than it (`StatusPayload::mitm`), and `tcr run --codex` tells
/// only that server to restart. What it reports is what `serve` recorded at
/// its TLS load, never a re-read of the leaf on disk.
#[tokio::test]
async fn status_endpoint_always_reports_what_the_listener_intercepts() {
    let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
    let (status, _, body) = status_request(
        Arc::clone(&manager),
        Method::GET,
        Some(loopback_peer()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let wire: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(
        wire["mitm"],
        serde_json::json!({"interceptedHosts": [], "leaf": "none"}),
        "nothing recorded: MITM off, reported rather than left out"
    );

    let recorded = crate::status::MitmStatus {
        intercepted_hosts: vec!["api.anthropic.com".to_string(), "chatgpt.com".to_string()],
        leaf: crate::status::MitmStatus::LEAF_MINTED.to_string(),
    };
    manager.record_mitm_status(recorded.clone());
    let (_, _, body) = status_request(manager, Method::GET, Some(loopback_peer()), None).await;
    let payload: crate::status::StatusPayload =
        serde_json::from_slice(&body).expect("a tcr status payload");
    assert_eq!(payload.mitm, Some(recorded));
}

/// The endpoint carries the machine's heat exactly where there is an
/// AppleSMC to read: a reading with a CPU figure on a Mac that has one, no
/// key at all in a VM or on Linux.
///
/// The process's heat cache is shared with every other status test in this
/// binary, and a request that finds a read in flight answers without one
/// (`crate::heat`), so the first request may carry none. It asks for up to
/// two seconds before concluding there is no heat.
#[tokio::test]
async fn status_endpoint_carries_heat_where_there_is_an_smc() {
    let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
    let mut smc = tcr_smc::HeatSource::open(Box::new(|_| {}));
    let has_smc = smc
        .read()
        .is_some_and(|reading| reading.cpu_celsius.is_some());
    let mut wire = Value::Null;
    for _ in 0..40 {
        let (status, _, body) = status_request(
            Arc::clone(&manager),
            Method::GET,
            Some(loopback_peer()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        wire = serde_json::from_slice(&body).expect("json");
        if wire.get("heat").is_some() || !has_smc {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if has_smc {
        let cpu = wire["heat"]["cpuCelsius"].as_f64();
        assert!(
            cpu.is_some_and(|c| tcr_smc::PLAUSIBLE_CELSIUS.contains(&c)),
            "{wire}"
        );
    } else {
        assert!(wire.get("heat").is_none(), "no SMC, no key: {wire}");
    }
}

/// THE BITING TEST for the whole change: the endpoint reports the counters of
/// the process that actually served, so a cache hit ratio computed from them
/// is a real measurement.
///
/// Pre-fix there was no route at all — `tcr status` built a FRESH `Manager`
/// whose `input_tokens` is structurally 0, so its ratio was the literal `0.0`
/// fallback for every account, always. Here 750 of 1000 input tokens were
/// cache reads and the payload must carry exactly that, giving 0.75.
#[tokio::test]
async fn status_endpoint_returns_live_counters() {
    let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
    // Serve a request through the manager so the counters are non-zero, the
    // same two calls `handle` makes at a terminal outcome.
    manager.update_usage(0, 1_000, 200, 750, 50);
    manager.record_served(0, OffsetDateTime::now_utc(), None, SessionKind::Fallback);

    let (status, headers, body) =
        status_request(manager, Method::GET, Some(loopback_peer()), None).await;
    assert_eq!(status, StatusCode::OK, "loopback GET is served");
    assert_eq!(
        headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "live state from a credential-holding process is never stored"
    );

    let payload: crate::status::StatusPayload =
        serde_json::from_slice(&body).expect("a tcr status payload");
    assert_eq!(payload.kind, crate::status::STATUS_KIND);
    let account = &payload.accounts[0];
    assert_eq!(account.name, "dummy");
    assert_eq!(account.requests, 1, "the serve was counted");
    assert_eq!(account.input_tokens, 1_000);
    assert_eq!(account.cache_read_tokens, 750);
    assert_eq!(account.cache_creation_tokens, 50);
    assert_eq!(account.output_tokens, 200);
    // The number `tcr status` derives from this row — a real 0.75, never the
    // structural 0.0 an offline snapshot could only ever produce.
    let ratio = account.cache_read_tokens as f64 / account.input_tokens as f64;
    assert!(
        (ratio - 0.75).abs() < f64::EPSILON,
        "hit ratio reflects the real numbers: {ratio}"
    );
}

/// SECURITY GUARD 1 — origin. Binding loopback is not authorization, so the
/// endpoint proves the peer instead: a non-loopback `ClientAddr` is refused,
/// and so is an ABSENT one (a request that did not arrive through the hybrid
/// listener has no provable origin, so it fails CLOSED). Neither answer may
/// carry any fleet state.
#[tokio::test]
async fn status_endpoint_rejects_a_non_loopback_client() {
    let config = || dummy_config(None, "http://127.0.0.1:1");

    for (label, peer) in [
        ("a routable peer", Some(remote_peer())),
        ("no ClientAddr at all", None),
    ] {
        let manager = Manager::with_live_refresher(config(), None);
        let (status, _headers, body) = status_request(manager, Method::GET, peer, None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} must be refused, got {status}"
        );
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("dummy") && !text.contains("accounts"),
            "a refused caller learns nothing about the fleet: {text}"
        );
    }
}

/// SECURITY GUARD 2 — the key. When a proxy api-key is configured this
/// endpoint requires it with NO loopback exemption, deliberately stricter than
/// the forwarding path (which exempts loopback because `claude` sends its own
/// OAuth and never the proxy key). Every case below is a LOOPBACK peer, so
/// what is being asserted is precisely that loopback alone is not enough.
#[tokio::test]
async fn status_endpoint_rejects_a_bad_api_key() {
    let config = || dummy_config(Some("sk-proxy-secret"), "http://127.0.0.1:1");

    for (label, key) in [
        ("no key", None),
        ("wrong key", Some("sk-wrong-secret!")),
        ("prefix of the key", Some("sk-proxy")),
        ("key with a suffix", Some("sk-proxy-secret-extra")),
    ] {
        let manager = Manager::with_live_refresher(config(), None);
        let (status, _headers, body) =
            status_request(manager, Method::GET, Some(loopback_peer()), key).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{label} must be 401 even from loopback, got {status}"
        );
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("dummy"),
            "a rejected caller learns nothing about the fleet: {text}"
        );
    }

    // ...and the correct key from loopback is served.
    let manager = Manager::with_live_refresher(config(), None);
    let (status, _headers, _body) = status_request(
        manager,
        Method::GET,
        Some(loopback_peer()),
        Some("sk-proxy-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "loopback + correct key is served");
}

/// SECURITY GUARD 3 — no secrets on the wire. Asserted on the RESPONSE BYTES,
/// not on a struct: a struct assertion only proves the fields you thought to
/// name are clean, while the bytes are what actually leaves the process. The
/// proxy holds every account's OAuth access and refresh token plus the proxy
/// key; none of the three may appear, in any field, under any name.
#[tokio::test]
async fn status_endpoint_leaks_no_secrets() {
    let manager = Manager::with_live_refresher(
        dummy_config(Some("sk-proxy-secret"), "http://127.0.0.1:1"),
        None,
    );
    manager.update_usage(0, 1_000, 200, 750, 50);
    let (status, headers, body) = status_request(
        manager,
        Method::GET,
        Some(loopback_peer()),
        Some("sk-proxy-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let text = String::from_utf8_lossy(&body);
    for secret in [
        "at-dummy",        // the account's OAuth ACCESS token
        "rt-dummy",        // the account's OAuth REFRESH token
        "sk-proxy-secret", // the proxy api-key (must not be echoed back)
        "Bearer",          // no upstream Authorization header, in any form
    ] {
        assert!(
            !text.contains(secret),
            "the response body leaked {secret:?}: {text}"
        );
    }
    // Sanity: the assertion above is meaningful only if a real payload WAS
    // rendered — an empty body would pass it vacuously.
    assert!(
        text.contains("\"name\":\"dummy\"") && text.contains("\"cacheReadTokens\":750"),
        "the body really is a populated status payload: {text}"
    );
    // Headers are part of the response too — nothing credential-shaped there.
    for (name, value) in &headers {
        let rendered = format!("{name}: {}", value.to_str().unwrap_or_default());
        assert!(
            !rendered.contains("at-dummy")
                && !rendered.contains("rt-dummy")
                && !rendered.contains("sk-proxy-secret"),
            "a response header leaked a secret: {rendered}"
        );
    }
}

/// SECURITY GUARD 4 — GET only, and never a fall-through. A non-GET on the
/// status path is answered LOCALLY with 405; it must not mutate anything and
/// must not reach `handle`.
///
/// The 405 is what proves the no-fall-through: `handle` would rewrite the
/// request to the configured upstream — a dead port here — and return 502. So
/// 405 (and not 502) is the assertion that a POST to this path was never sent
/// to Anthropic with a pooled OAuth Bearer attached.
///
/// Sensitivity, measured rather than assumed: this test still passes with the
/// method router's explicit `.fallback` removed, because axum 0.8 already
/// answers 405 instead of inheriting the outer catch-all. So it is a
/// REGRESSION guard on the routing contract — it would catch the path being
/// re-registered under `any(...)`, moved behind the fallback, or an axum
/// upgrade that starts propagating the outer fallback — not proof that the
/// explicit `.fallback` is currently load-bearing.
#[tokio::test]
async fn status_endpoint_is_get_only() {
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
        let before = manager.snapshot(OffsetDateTime::now_utc());
        let (status, _headers, body) = status_request(
            Arc::clone(&manager),
            method.clone(),
            Some(loopback_peer()),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on the status path is refused locally, got {status}"
        );
        assert_ne!(
            status,
            StatusCode::BAD_GATEWAY,
            "{method} must never fall through to the upstream forwarder"
        );
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("cacheReadTokens"),
            "a refused method returns no snapshot: {text}"
        );

        // Nothing moved: no serve counted, no token usage, no probe.
        let after = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(after.accounts[0].requests, before.accounts[0].requests);
        assert_eq!(
            after.accounts[0].input_tokens,
            before.accounts[0].input_tokens
        );
        assert_eq!(
            after.accounts[0].probe_status, before.accounts[0].probe_status,
            "{method} triggered no probe"
        );
        assert_eq!(after.current, before.current, "{method} moved no cursor");
    }
}

// --- GET /_tcr/quota ----------------------------------------------------

/// The public spelling of the quota route, written out rather than read from
/// the constant so a rename of the path a status line calls fails here.
const QUOTA_URI: &str = "/_tcr/quota";

/// [`status_request`]'s shape for the quota route: drive the router directly
/// with a `ClientAddr` we control.
async fn quota_request(
    manager: Arc<Manager>,
    method: Method,
    peer: Option<SocketAddr>,
    api_key: Option<&str>,
) -> (StatusCode, HeaderMap, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder().method(method).uri(QUOTA_URI);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    let mut req = builder.body(Body::empty()).expect("build request");
    if let Some(addr) = peer {
        req.extensions_mut().insert(ClientAddr(addr));
    }
    let response = app(manager).oneshot(req).await.expect("router response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, headers, body)
}

/// Feed account `idx` a 5-hour and a weekly reading through the same headers
/// a live reply carries. Resets are whole seconds, as Anthropic sends them.
fn feed_quota(manager: &Manager, idx: usize, five_hour: &str, seven_day: &str, now_s: i64) {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        (
            "anthropic-ratelimit-unified-5h-utilization",
            five_hour.to_string(),
        ),
        (
            "anthropic-ratelimit-unified-5h-reset",
            (now_s + 3_600).to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-utilization",
            seven_day.to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-reset",
            (now_s + 86_400).to_string(),
        ),
    ] {
        headers.insert(name, HeaderValue::from_str(&value).expect("header value"));
    }
    manager.update_quota(idx, &headers, 200);
}

/// THE GATE for the quota route: a status line ticks every turn, so the answer
/// must come from memory. It returns the small per-account shape, makes no
/// upstream attempt, and answers well inside one status-line tick.
///
/// The upstream counter is checked live at the end: one real `/v1/messages`
/// through the same manager must move it to 1, so the 0 before it is a
/// measurement and not a harness that cannot count.
#[tokio::test]
async fn quota_endpoint_answers_from_memory_without_upstream() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![Some(raw_200())]).await;
    let manager = fleet_configured(up_addr, &["alice", "bob"], |_| {});
    let now_s = crate::now_ms() / 1000;
    feed_quota(&manager, 0, "0.5", "0.2", now_s);
    // Over `dummy_config`'s 0.90 threshold, so bob's gate names the window.
    feed_quota(&manager, 1, "0.96", "0.4", now_s);

    let started = Instant::now();
    let (status, headers, body) = quota_request(
        Arc::clone(&manager),
        Method::GET,
        Some(loopback_peer()),
        None,
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(
        status,
        StatusCode::OK,
        "loopback GET on the quota route is served, got {status}: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "the quota route answers from memory, took {elapsed:?}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "the quota route never calls upstream"
    );
    assert_eq!(
        headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store")
    );

    let payload: Value = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(payload["kind"], "tcr.quota.v1");
    let accounts = payload["accounts"].as_array().expect("accounts array");
    assert_eq!(accounts.len(), 2, "one row per account: {payload}");

    // The row is the small shape and nothing more: a status line has no use
    // for token counters, groups or plan fields.
    let mut keys: Vec<&str> = accounts[0]
        .as_object()
        .expect("account row is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "fiveHour",
            "fiveHourResetMs",
            "gate",
            "name",
            "sevenDay",
            "sevenDayResetMs"
        ]
    );

    assert_eq!(accounts[0]["name"], "alice");
    assert_eq!(accounts[0]["gate"], "ok");
    assert_eq!(accounts[0]["fiveHour"], 0.5);
    assert_eq!(accounts[0]["fiveHourResetMs"], (now_s + 3_600) * 1000);
    assert_eq!(accounts[0]["sevenDay"], 0.2);
    assert_eq!(accounts[0]["sevenDayResetMs"], (now_s + 86_400) * 1000);
    assert_eq!(accounts[1]["name"], "bob");
    assert_eq!(accounts[1]["gate"], "five-hour");
    assert_eq!(accounts[1]["fiveHour"], 0.96);

    // Positive control for the zero above: a real request through the same
    // manager reaches the counted upstream.
    assert_eq!(post_one(Arc::clone(&manager)).await, 200);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "the upstream counter is live"
    );
}

/// The documented v1 quota endpoint reports Claude capacity only. Codex's
/// provider and weekly window remain available on the provider-aware status route.
#[tokio::test]
async fn quota_v1_excludes_codex_rows_while_status_keeps_their_capacity() {
    let manager = Manager::with_live_refresher(
        codex_fleet("http://127.0.0.1:1", "http://127.0.0.1:1"),
        None,
    );
    let now_s = crate::now_ms() / 1000;
    feed_quota(&manager, 0, "0.5", "0.2", now_s);
    let mut weekly = HeaderMap::new();
    weekly.insert(
        "x-codex-primary-used-percent",
        HeaderValue::from_static("40"),
    );
    weekly.insert(
        "x-codex-primary-window-minutes",
        HeaderValue::from_static("10080"),
    );
    weekly.insert(
        "x-codex-primary-reset-at",
        HeaderValue::from_str(&(now_s + 86_400).to_string()).expect("header value"),
    );
    manager.update_quota(1, &weekly, 200);

    let (status, _headers, body) = quota_request(
        Arc::clone(&manager),
        Method::GET,
        Some(loopback_peer()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_slice(&body).expect("JSON body");
    let rows = payload["accounts"].as_array().expect("accounts array");
    assert_eq!(rows[0]["name"], "alice@example.com");
    assert!(
        rows[0].get("provider").is_none(),
        "an Anthropic row carries no provider key: {}",
        rows[0]
    );
    assert_eq!(payload["kind"], "tcr.quota.v1");
    assert_eq!(rows.len(), 1, "Codex is absent from Claude's v1 capacity");

    let (status, _, body) = status_request(manager, Method::GET, Some(loopback_peer()), None).await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_slice(&body).expect("status JSON");
    let rows = payload["accounts"].as_array().expect("status accounts");
    assert_eq!(rows[1]["name"], "bob@example.com/codex");
    assert_eq!(rows[1]["provider"], "codex", "{}", rows[1]);
    assert!(
        rows[1]["sevenDay"]
            .as_f64()
            .is_some_and(|v| (v - 0.4).abs() < 1e-9),
        "the provider-aware status route keeps Codex's weekly reading: {}",
        rows[1]
    );
}

/// The quota route passes the same [`local_endpoint_gate`] as the status
/// route: a non-loopback or unproven peer is refused, and a configured proxy
/// key is required with no loopback exemption. A refusal carries no fleet
/// state.
#[tokio::test]
async fn quota_endpoint_has_the_status_gate() {
    for (label, peer) in [
        ("a routable peer", Some(remote_peer())),
        ("no ClientAddr at all", None),
    ] {
        let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
        let (status, _headers, body) = quota_request(manager, Method::GET, peer, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label} must be refused");
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("dummy") && !text.contains("accounts"),
            "a refused caller learns nothing about the fleet: {text}"
        );
    }

    let keyed = || {
        Manager::with_live_refresher(
            dummy_config(Some("sk-proxy-secret"), "http://127.0.0.1:1"),
            None,
        )
    };
    let (status, _h, _b) = quota_request(keyed(), Method::GET, Some(loopback_peer()), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a keyless loopback caller is refused when a key is set"
    );
    let (status, _h, body) = quota_request(
        keyed(),
        Method::GET,
        Some(loopback_peer()),
        Some("sk-proxy-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the right key is served");
    assert!(
        String::from_utf8_lossy(&body).contains("\"name\":\"dummy\""),
        "the keyed answer is a real payload"
    );
}

/// GET only, answered locally: any other verb is a local 405, never a
/// fall-through to the forwarder (which would answer 502 off the dead port).
#[tokio::test]
async fn quota_endpoint_is_get_only() {
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
        let (status, _headers, body) =
            quota_request(manager, method.clone(), Some(loopback_peer()), None).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on the quota path is refused locally, got {status}"
        );
        assert!(
            !String::from_utf8_lossy(&body).contains("fiveHour"),
            "a refused method returns no quota"
        );
    }
}
