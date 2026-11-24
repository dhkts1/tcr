//! Tests for the proxy handler, one file per subject; the helpers more than one file uses live here.

use super::*;
use crate::config::{Account, Config, ProxyConfig};
use crate::log_err::LogErr;
use crate::test_support::stub_upstream::{self as stub, StubAddr};

mod add_route;
mod bound_history;
mod client_request;
mod codex_refusals;
mod codex_usage_and_holds;
mod codex_wire;
mod control_routes;
mod failover;
mod relays;
mod request_logging;
mod serving_account;
mod status_routes;
mod transport_and_offline;
mod usage_parsing;

/// A request to the proxy as reqwest puts it on the wire: a loopback `host`, `accept: */*`,
/// and a `content-length` when there is a body. For a test that wants the proxy's answer
/// and not its listener, so it is driven in-process by [`send_to_proxy`]: no port, no
/// connection. A test whose subject IS the socket (hyper's request-line parser, the
/// production `mitm::serve` stack) keeps a real listener and says so.
fn client_request(method: Method, path: &str, body: impl Into<Bytes>) -> Request {
    let body: Bytes = body.into();
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(HOST, "127.0.0.1")
        .header(ACCEPT, "*/*");
    if !body.is_empty() {
        builder = builder.header(axum::http::header::CONTENT_LENGTH, body.len());
    }
    builder.body(Body::from(body)).expect("build request")
}

/// Answer `req` with `router` in-process.
async fn send_to_proxy(router: Router, req: Request) -> Response {
    use tower::ServiceExt as _;
    router.oneshot(req).await.expect("router response")
}

/// The whole body of `response`, as text.
async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    String::from_utf8_lossy(&bytes).into_owned()
}

const REQUEST_KEY: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn dummy_config(api_key: Option<&str>, upstream: &str) -> Config {
    Config {
        proxy: ProxyConfig {
            port: 0,
            api_key: api_key.map(str::to_string),
            request_key: None,
            extra: serde_json::Map::new(),
        },
        upstream: upstream.to_string(),
        codex_upstream: "http://127.0.0.1:0".to_string(),
        ..Config::fixture(vec![Account::fixture("dummy")])
    }
}

/// Drive the router directly with a `ClientAddr` extension we control.
///
/// This is the same extension `mitm::serve_http` injects from the real peer
/// socket, and driving the router by hand is the only way to present a
/// NON-loopback peer without binding a routable address on the test machine.
/// `peer = None` models a request that never went through the listener at all.
async fn status_request(
    manager: Arc<Manager>,
    method: Method,
    peer: Option<SocketAddr>,
    api_key: Option<&str>,
) -> (StatusCode, HeaderMap, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder().method(method).uri(STATUS_PATH);
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

fn loopback_peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 54_321))
}

/// A routable, definitely-not-loopback peer (TEST-NET-3, RFC 5737).
fn remote_peer() -> SocketAddr {
    SocketAddr::from(([203, 0, 113, 7], 44_444))
}

/// A two-account config with distinct orgs, written to `path` so the manager
/// that boots from it has a real durable half to write back to.
///
/// Obviously-fake identities only: this repository is public.
fn two_account_config(api_key: Option<&str>) -> Config {
    // The fixture's expiry is an hour out, so booting triggers no token refresh.
    let account = |name: &str, org: &str, uuid: &str, priority: i64| Account {
        account_uuid: Some(uuid.to_string()),
        org_uuid: Some(format!("11111111-1111-1111-1111-{org}")),
        org_name: Some(format!("Org {org}")),
        access_token: format!("at-{org}"),
        refresh_token: Some(format!("rt-{org}")),
        priority: Some(priority),
        ..Account::fixture(name)
    };
    Config {
        accounts: vec![
            account("alice@example.com", "aaaaaaaaaaaa", "22222222-a", 0),
            account("bob@example.com", "bbbbbbbbbbbb", "33333333-b", 1),
        ],
        ..dummy_config(api_key, "http://127.0.0.1:1")
    }
}

fn control_config_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "tcr-control-test-{tag}-{}-{}.json",
        std::process::id(),
        crate::now_ms()
    ))
}

/// A manager booted from a config that is ALSO on disk, so both halves of a
/// `disabled` change are observable: `manager.snapshot()` for the live rotation
/// and the file for the durable flag.
fn control_manager(tag: &str, api_key: Option<&str>) -> (Arc<Manager>, std::path::PathBuf) {
    let path = control_config_path(tag);
    let config = two_account_config(api_key);
    config::save(&path, &config).expect("write the test config");
    let manager = Manager::with_live_refresher(config, Some(path.clone()));
    (manager, path)
}

/// Drive the proxy router with a peer we control, as `mitm::serve_http` would,
/// returning the WHOLE client-visible response.
///
/// Response headers are the surface the account strip acts on, so a helper
/// that folds them away cannot test it — [`drive`] is the status+body view
/// layered on top of this one.
async fn drive_full(
    manager: Arc<Manager>,
    method: Method,
    uri: &str,
    peer: Option<SocketAddr>,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    use tower::ServiceExt as _;
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder
        .body(Body::from(body.to_string()))
        .expect("build request");
    if let Some(addr) = peer {
        req.extensions_mut().insert(ClientAddr(addr));
    }
    app(manager).oneshot(req).await.expect("router response")
}

/// [`drive_full`] reduced to the status and body most tests assert on.
async fn drive(
    manager: Arc<Manager>,
    method: Method,
    uri: &str,
    peer: Option<SocketAddr>,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, Bytes) {
    let response = drive_full(manager, method, uri, peer, headers, body).await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, bytes)
}

/// One turn of a conversation: `user_id` is the session's stable identity (tier 2 of
/// `stable_session_key`), and each signature is echoed back inside an assistant `thinking`
/// block exactly as Claude Code echoes them.
fn thinking_turn_body(user_id: &str, signatures: &[&str]) -> String {
    thinking_turn_body_for("claude-sonnet-5", user_id, signatures)
}

/// [`thinking_turn_body`] for `model`, for a test whose model decides where thinking
/// survives ([`crate::model::thinking_is_org_bound`]).
fn thinking_turn_body_for(model: &str, user_id: &str, signatures: &[&str]) -> String {
    let mut messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
    for signature in signatures {
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "step one", "signature": signature},
            ],
        }));
    }
    serde_json::json!({
        "model": model,
        "metadata": {"user_id": user_id},
        "messages": messages,
    })
    .to_string()
}

/// Drive `/v1/messages` as a real client session would: a loopback peer and the
/// `SessionKey` extension `mitm::serve_http` attaches, without which `stable_session_key`
/// is never consulted and nothing is ever pinned.
async fn drive_conversation_full(manager: Arc<Manager>, body: &str) -> Response {
    use tower::ServiceExt as _;
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    req.extensions_mut().insert(ClientAddr(loopback_peer()));
    req.extensions_mut().insert(SessionKey(1));
    app(manager).oneshot(req).await.expect("router response")
}

/// [`drive_conversation_full`] reduced to the status and body most of these tests assert
/// on; the full form exists for the one that reads a `retry-after` header.
async fn drive_conversation(manager: Arc<Manager>, body: &str) -> (StatusCode, Bytes) {
    let response = drive_conversation_full(manager, body).await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, bytes)
}

/// A `MakeWriter` that keeps what was written, so a test can inspect
/// tracing output without capturing the process's real stdout. Same shape
/// as `main.rs`'s `SharedBuf` — duplicated locally because that one is
/// private to `main.rs`'s own test module.
#[derive(Clone, Default)]
struct SharedBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedBuf {
    fn contents(&self) -> String {
        let bytes = self.0.lock().expect("shared buffer poisoned").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// How long the fake upstreams below sit on a request before sending its
/// headers. Every positive control asserts this shows up in `upstream_ms`
/// (the send-to-headers span) and NOT in `wait_ms` (arrival to send).
const HEADER_DELAY_MS: u64 = 300;

/// Every line of `log` whose message is `message`.
fn lines_with<'a>(log: &'a str, message: &str) -> Vec<&'a str> {
    log.lines().filter(|l| l.contains(message)).collect()
}

/// The value of ` name=` on `line`, up to the next space.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!(" {name}=");
    let start = line.find(&marker)? + marker.len();
    Some(line[start..].split(' ').next().unwrap_or(""))
}

fn ms_field(line: &str, name: &str) -> u64 {
    field(line, name)
        .unwrap_or_else(|| panic!("no {name} on: {line}"))
        .parse()
        .unwrap_or_else(|e| panic!("{name} is not a number ({e}) on: {line}"))
}

/// Polls the captured log until `message` appears, for lines written by a
/// spawned task (the streamed arm's parser) after the client already has
/// its whole body.
async fn wait_for_line(sink: &SharedBuf, message: &str) -> String {
    for _ in 0..300 {
        let log = sink.contents();
        if log.contains(message) {
            return log;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no {message:?} line within 3s; log: {:?}", sink.contents());
}

/// A dispatcher registered for the life of the test process, so the sink below is never the
/// only one. While exactly one is registered, tracing-core 0.1.36 (`callsite.rs`,
/// `has_just_one`) computes a newly hit callsite's interest from the hitting thread's
/// default alone: a sibling test with no subscriber that reaches `request usage` first
/// caches it as `never`, and this thread's sink then stays empty. With two registered, every
/// callsite combines all of them and each event asks its own thread's subscriber.
/// `codex_model_failover_times_the_first_byte_the_client_receives` beside
/// `codex_failed_stream_limits_move_the_next_pinned_turn` lost its line that way, 3 of 3.
static SECOND_DISPATCHER: std::sync::LazyLock<tracing::Dispatch> =
    std::sync::LazyLock::new(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));

fn debug_sink() -> (SharedBuf, tracing::subscriber::DefaultGuard) {
    std::sync::LazyLock::force(&SECOND_DISPATCHER);
    let sink = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (sink, guard)
}

/// A fake upstream that answers by CONNECTION ORDINAL, so one test can script the
/// exact per-attempt sequence the rotation loop sees. `None` = read the request
/// and hang up without replying, which the client surfaces as a TRANSPORT failure
/// (the shape the 502 decision turns on); `Some(raw)` = write `raw` verbatim.
/// Connections past the end of the script reuse the last entry. Every scripted
/// reply must send `connection: close` so the client pool cannot reuse a socket
/// and attempt N is always connection N.
///
/// A route on the binary's shared raw listener ([`stub::scripted`]), not a port of its own.
async fn spawn_scripted_upstream(script: Vec<Option<String>>) -> StubAddr {
    stub::scripted(script).0
}

/// [`spawn_scripted_upstream`] plus a live count of the connections it was handed —
/// i.e. of upstream ATTEMPTS. Retries are invisible in the account stats (only
/// the terminal outcome is recorded), so a test that must pin the exact number
/// of sends reads it here instead of inferring it from the client's status.
async fn spawn_counted_upstream(
    script: Vec<Option<String>>,
) -> (StubAddr, Arc<std::sync::atomic::AtomicUsize>) {
    stub::scripted(script)
}

/// A `200 OK` with a minimal JSON usage body.
fn raw_200() -> String {
    stub::reply(
        "200 OK",
        "application/json",
        r#"{"usage":{"input_tokens":1,"output_tokens":1}}"#,
    )
}

/// A `429` reporting DURABLE quota rejection. `is_quota_rejected` matches on
/// `unified-status: rejected`, which is the ONE 429 arm that parks the account
/// and rotates with no inline sleep — so a test using it stays fast and has no
/// timing dependency.
fn raw_429_rejected(retry_after: u32) -> String {
    format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\
             retry-after: {retry_after}\r\nanthropic-ratelimit-unified-status: rejected\r\n\r\n"
    )
}

/// A `529 Overloaded` shaped like Anthropic's — the status the retry ladder
/// exists for. Carries NO `retry-after` (the live captures do not), so the
/// backoff comes from the ladder alone and the test's timing is deterministic.
fn raw_529() -> String {
    stub::reply(
        "529 ",
        "application/json",
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    )
}

/// Boot an N-account fleet (all cloned from `dummy`) pointed at `upstream`.
fn fleet(upstream: impl std::fmt::Display, names: &[&str]) -> Arc<Manager> {
    fleet_configured(upstream, names, |_| {})
}

/// [`fleet`] with a hook to edit the config before the manager boots — for a
/// test that needs an unmodelled top-level key (`revalidationServe`) set.
fn fleet_configured(
    upstream: impl std::fmt::Display,
    names: &[&str],
    configure: impl FnOnce(&mut Config),
) -> Arc<Manager> {
    struct NoRefresh;
    impl crate::oauth::TokenRefresher for NoRefresh {
        fn refresh(&self, _provider: Provider, _t: String) -> crate::oauth::RefreshFuture {
            Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
        }
    }
    let mut config = dummy_config(None, &format!("http://{upstream}"));
    for name in names {
        let mut extra = config.accounts[0].clone();
        extra.name = (*name).to_string();
        config.accounts.push(extra);
    }
    config.accounts.remove(0); // keep exactly `names`, in order
    configure(&mut config);
    Manager::new(
        config,
        Arc::new(NoRefresh),
        Arc::new(crate::probe::LiveUsageProber::new()),
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    )
}

/// [`post_one`] plus the client-visible BODY — for asserting that a status the
/// proxy gives up on is forwarded verbatim, not replaced by a synthesized one.
async fn post_one_with_body(manager: Arc<Manager>) -> (u16, String) {
    let (status, _, body) = post_one_full(manager).await;
    (status, body)
}

/// [`post_one_with_body`] plus the response HEADERS — for asserting what a
/// synthesized 429 tells a client that reads headers, not only its body.
async fn post_one_full(manager: Arc<Manager>) -> (u16, HeaderMap, String) {
    let body = serde_json::json!({ "model": "claude-x", "messages": [] }).to_string();
    let resp = send_to_proxy(
        app(manager),
        client_request(Method::POST, "/v1/messages", body),
    )
    .await;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, body_text(resp).await)
}

/// POST one `/v1/messages` naming `model` to `manager`'s router, returning the
/// client-visible status.
async fn post_model(manager: Arc<Manager>, model: &str) -> u16 {
    let body = serde_json::json!({ "model": model, "messages": [] }).to_string();
    send_to_proxy(
        app(manager),
        client_request(Method::POST, "/v1/messages", body),
    )
    .await
    .status()
    .as_u16()
}

async fn post_one(manager: Arc<Manager>) -> u16 {
    post_model(manager, "claude-x").await
}

fn retry_after(resp: &Response) -> Option<&str> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
}

/// A manager over a DEAD upstream (127.0.0.1:1, reliably connection-refused) and
/// no proxy key: what [`spawn_dead_upstream_proxy`] serves.
fn dead_upstream_manager() -> Arc<Manager> {
    let config = dummy_config(None, "http://127.0.0.1:1");
    assert_eq!(
        config.codex_upstream, "http://127.0.0.1:0",
        "an unspecified test origin must fail closed"
    );
    Manager::with_live_refresher(config, None)
}

/// Boot the proxy over a fresh loopback listener with a DEAD upstream
/// (127.0.0.1:1, reliably connection-refused) and no proxy key, returning its
/// address. A request the host guard lets THROUGH reaches the rotation loop
/// and fails transport → 503 (connection-refused is a pure connect-phase
/// failure — see [`bad_gateway`] — so it is the safe-to-retry shape, not a
/// 502); a request the guard REJECTS returns its local status (421) with no
/// egress. The 421-vs-503 split is what proves whether the guard fired.
///
/// Keeps a real listener on purpose: these tests write the request line byte for
/// byte, and what they prove is that hyper's own request-line parser hands the
/// guard the shapes they name.
async fn spawn_dead_upstream_proxy() -> SocketAddr {
    let manager = dead_upstream_manager();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app(manager))
            .await
            .log_debug("serving a test server");
    });
    addr
}

/// Send a raw HTTP/1.1 request over a fresh TCP connection and return the
/// response status code. Lets a test control the exact request-target form
/// (absolute vs origin) and `Host` header, which reqwest derives from the URI
/// and will not let a test spoof — required to exercise both guard branches.
async fn raw_request_status(addr: SocketAddr, raw: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = sock.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // The status line ends at the first CRLF — enough to read the code.
        if buf.windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    // "HTTP/1.1 421 Misdirected Request" → 421
    String::from_utf8_lossy(&buf)
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

/// The serving Codex account's ChatGPT account id, and the one the CLIENT
/// says it is. Both are fixtures.
const CODEX_SERVING_ID: &str = "11111111-1111-4111-8111-111111111111";

/// One request a [`CapturingUpstream`] received, as it arrived on the wire.
/// The body is kept as BYTES: a Codex body is usually zstd, and a lossy string would
/// make "forwarded byte for byte" unfalsifiable.
#[derive(Clone, Debug)]
struct SeenRequest {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl SeenRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// How many times `name` arrived. One identity header, never two.
    fn header_count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(n, _)| n == name).count()
    }
}

/// A fake origin that RECORDS every request it receives and answers `200 {}`.
///
/// [`spawn_echo_upstream`] reports what it saw inside its own response, which
/// works only for the one origin a request reached. The Codex tests run two
/// origins side by side, Anthropic's and Codex's, and the assertion that
/// matters is as often "this one saw NOTHING" as "this one saw X", so each
/// origin keeps its own record and its own count.
struct CapturingUpstream {
    url: String,
    seen: Arc<std::sync::Mutex<Vec<SeenRequest>>>,
}

impl CapturingUpstream {
    async fn spawn() -> Self {
        let seen: Arc<std::sync::Mutex<Vec<SeenRequest>>> = Arc::default();
        let recorder = Arc::clone(&seen);
        let upstream = Router::new().fallback(move |req: Request| {
            let recorder = Arc::clone(&recorder);
            async move {
                let (parts, body) = req.into_parts();
                let bytes = to_bytes(body, MAX_BODY_BYTES).await.unwrap_or_default();
                recorder
                    .lock()
                    .expect("upstream recorder poisoned")
                    .push(SeenRequest {
                        path: parts
                            .uri
                            .path_and_query()
                            .map_or_default(|pq| pq.as_str().to_string()),
                        headers: parts
                            .headers
                            .iter()
                            .map(|(n, v)| {
                                (n.as_str().to_string(), v.to_str().unwrap_or("").to_string())
                            })
                            .collect(),
                        body: bytes.to_vec(),
                    });
                ([(CONTENT_TYPE, "application/json")], "{}")
            }
        });
        Self {
            url: stub::http(upstream).url(),
            seen,
        }
    }

    fn request_count(&self) -> usize {
        self.seen.lock().expect("upstream recorder poisoned").len()
    }

    fn last_request(&self) -> SeenRequest {
        self.seen
            .lock()
            .expect("upstream recorder poisoned")
            .last()
            .cloned()
            .expect("this origin saw no request at all")
    }
}

/// A mixed fleet: `alice@example.com` on Anthropic (index 0) and
/// `bob@example.com/codex` on Codex (index 1), each provider on its own
/// origin. alice carries no account uuid, so nothing is spliced into an
/// Anthropic body in these tests.
fn codex_fleet(anthropic: &str, codex: &str) -> Config {
    let base = dummy_config(None, anthropic);
    let alice = Account {
        name: "alice@example.com".to_string(),
        access_token: "at-alice".to_string(),
        refresh_token: Some("rt-alice".to_string()),
        ..base.accounts[0].clone()
    };
    let bob = Account {
        name: "bob@example.com/codex".to_string(),
        provider: Provider::Codex,
        account_uuid: Some(CODEX_SERVING_ID.to_string()),
        access_token: "at-bob".to_string(),
        refresh_token: Some("rt-bob".to_string()),
        ..base.accounts[0].clone()
    };
    Config {
        accounts: vec![alice, bob],
        codex_upstream: codex.to_string(),
        ..base
    }
}

/// [`drive_full`] with a raw byte body, for a body that is not UTF-8.
async fn drive_bytes(
    manager: Arc<Manager>,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder.body(Body::from(body)).expect("build request");
    req.extensions_mut().insert(ClientAddr(loopback_peer()));
    let response = app(manager).oneshot(req).await.expect("router response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, bytes)
}

/// A fake origin that answers every request `200`, with `body` as `content_type`.
async fn spawn_fixed_origin(content_type: &'static str, body: &'static str) -> String {
    let origin =
        Router::new().fallback(move || async move { ([(CONTENT_TYPE, content_type)], body) });
    stub::http(origin).url()
}

/// [`codex_fleet`] plus a second Codex account: alice on Anthropic (0), bob (1) and
/// carol (2) on Codex, so a Codex 429 has somewhere to rotate to.
fn codex_fleet_with_carol(anthropic: &str, codex: &str) -> Config {
    let mut config = codex_fleet(anthropic, codex);
    let carol = Account {
        name: "carol@example.com/codex".to_string(),
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        access_token: "at-carol".to_string(),
        refresh_token: Some("rt-carol".to_string()),
        ..config.accounts[1].clone()
    };
    config.accounts.push(carol);
    config
}

/// The fleet index of a bearer [`codex_fleet_with_carol`] issued, as an origin saw it.
fn codex_index_of(bearer: &str) -> usize {
    match bearer {
        "Bearer at-bob" => 1,
        "Bearer at-carol" => 2,
        other => panic!("a Codex origin saw an unexpected bearer: {other}"),
    }
}

/// A Codex origin that answers the FIRST request 403 and every later one 200,
/// recording each bearer. The 403 carries `x-codex-rate-limit-reached-type`, which
/// must not turn a 403 into a quota verdict, and no window: a window would hand the
/// other account the better headroom, and selection would avoid the refused one
/// with no hold at all.
async fn spawn_codex_origin_forbidding_the_first() -> (
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
                        return Response::builder()
                            .status(StatusCode::FORBIDDEN)
                            .header(CONTENT_TYPE, "application/json")
                            .header("x-codex-rate-limit-reached-type", "primary")
                            .body(Body::from(
                                r#"{"error":{"type":"authentication_error","code":"invalid_api_key","message":"login refused"}}"#,
                            ))
                            .expect("403 response");
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

/// A response's body, parsed as JSON.
async fn body_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("a JSON body ({err}): {}", String::from_utf8_lossy(&bytes)))
}
