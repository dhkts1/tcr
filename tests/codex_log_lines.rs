//! The log lines the request path writes about Codex: the 426 that moves `codex` onto SSE is
//! logged at debug and never at warn, a Codex request is never offered a peer's carry, and a
//! Codex account in the fleet does not change when an Anthropic request is offered one.
//!
//! # Why this is its own test binary
//!
//! These tests read `tracing` output, and `tracing` caches, once per process, whether each log
//! callsite is enabled at all. `tracing-core` (0.1.36, `Rebuilder::JustOne` in `callsite.rs`)
//! works that out for a callsite reached for the first time while a single subscriber is
//! registered by asking only the reaching THREAD's subscriber, and holds no lock against a
//! subscriber being installed at that moment. In the lib test binary, a sibling test on a thread
//! with no subscriber reached the 426 line for the first time while this test was still
//! installing its own: the callsite was cached as never enabled, and the line was never written.
//! Measured over full lib runs: with timing added, the sibling's first hit landed inside
//! the install both times the capture came back empty (102 µs into 105, and 87 µs into 97); the
//! same request again was still silent, and a `rebuild_interest_cache()` made it log. A carry
//! test, whose capture was thread-local, lost its line the same way. No test in a shared binary
//! can order itself before every other test's first log line.
//!
//! Here every test calls [`capture`] before it builds a manager or sends anything, so the one
//! process-wide subscriber is installed before any of the crate's log lines is reached, and every
//! callsite's interest is worked out with it in place. A test added to this file must do the
//! same, first.
//!
//! # House rules
//!
//! No real account or token. Every origin is a closed port on loopback (`127.0.0.1:1` refuses
//! the connection), and the proxy is driven in-process through `proxy::app`, so no port is bound
//! at all, least of all 3456.

#![expect(
    clippy::expect_used,
    reason = "test helpers outside #[test] fail the test with a message; the ban is for production code"
)]

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tcr::config::Config;
use tcr::manager::Manager;
use tcr::proxy::ClientAddr;
use tower::ServiceExt as _;

/// The line the peer-carry seam writes first, once per offer, for an origin the mesh does not
/// carry (`127.0.0.1` here), before any peers file is read.
const OFFER: &str = "not an origin the mesh carries";

/// The name of the span [`captured`] drives a request inside, and the one span [`capture`]
/// writes lines for.
const CAPTURE_SPAN: &str = "tcr_test_capture";

/// What the subscriber writes, shared by every test in this binary.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("capture buffer poisoned")).into_owned()
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capture buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install, once, the process-wide subscriber every test here reads, and return its buffer.
/// Every test calls this FIRST (see the module doc). A concurrent caller waits in `get_or_init`
/// until the install is complete, so no test reaches a log line before it.
///
/// It writes every DEBUG-and-above line emitted inside a [`CAPTURE_SPAN`] span, from any
/// thread, and nothing else; each line names its span's marker, so a reader keeps its own lines
/// and no sibling's ([`captured`]).
fn capture() -> &'static SharedBuf {
    static SINK: OnceLock<SharedBuf> = OnceLock::new();
    SINK.get_or_init(|| {
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt as _;
        let sink = SharedBuf::default();
        let in_a_capture_span = tracing_subscriber::filter::dynamic_filter_fn(
            |meta: &tracing::Metadata<'_>,
             cx: &tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>| {
                *meta.level() <= tracing::Level::DEBUG
                    && ((meta.is_span() && meta.name() == CAPTURE_SPAN)
                        || cx.lookup_current().is_some_and(|span| {
                            span.scope().any(|outer| outer.name() == CAPTURE_SPAN)
                        }))
            },
        )
        .with_max_level_hint(tracing::level_filters::LevelFilter::DEBUG);
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_filter(in_a_capture_span);
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
            .expect("this test binary sets no other global subscriber");
        sink
    })
}

/// Drive `request` inside a fresh [`CAPTURE_SPAN`] span and return its output with every line
/// [`capture`] wrote inside THAT span. Each span carries a marker no other capture in this
/// process carries.
async fn captured<T>(request: impl Future<Output = T>) -> (T, String) {
    use tracing::Instrument as _;
    static NEXT_MARKER: AtomicU64 = AtomicU64::new(0);
    let sink = capture();
    let marker = NEXT_MARKER.fetch_add(1, Ordering::Relaxed);
    let output = request
        .instrument(tracing::debug_span!(CAPTURE_SPAN, marker))
        .await;
    let own = format!("{CAPTURE_SPAN}{{marker={marker}}}");
    let log = sink
        .contents()
        .lines()
        .filter(|line| line.contains(&own))
        .collect::<Vec<_>>()
        .join("\n");
    (output, log)
}

/// An Anthropic account row: fake tokens, not expired, so nothing is refreshed.
fn anthropic(name: &str) -> Value {
    json!({
        "name": name,
        "accessToken": "at-fake-anthropic",
        "refreshToken": "rt-fake-anthropic",
        "expiresAt": tcr::now_ms() + 3_600_000,
        "priority": 0,
    })
}

/// A Codex account row, `bob@example.com/codex`, with a fixture ChatGPT account id.
fn codex() -> Value {
    json!({
        "name": "bob@example.com/codex",
        "provider": "codex",
        "accountUuid": "11111111-1111-4111-8111-111111111111",
        "accessToken": "at-fake-codex",
        "refreshToken": "rt-fake-codex",
        "expiresAt": tcr::now_ms() + 3_600_000,
        "priority": 0,
    })
}

/// A manager over `accounts`, with both upstreams on a port that refuses every connection.
fn fleet(accounts: Vec<Value>) -> Arc<Manager> {
    let config: Config = serde_json::from_value(json!({
        "proxy": { "port": 0 },
        "upstream": "http://127.0.0.1:1",
        "codexUpstream": "http://127.0.0.1:1",
        "accounts": accounts,
    }))
    .expect("a test fleet's config parses");
    Manager::with_live_refresher(config, None)
}

/// One request through the real router, from a loopback client; the status it was answered.
async fn send(
    manager: Arc<Manager>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &'static str,
) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::from(body)).expect("build the request");
    request
        .extensions_mut()
        .insert(ClientAddr(SocketAddr::from(([127, 0, 0, 1], 50_000))));
    tcr::proxy::app(manager)
        .oneshot(request)
        .await
        .expect("the router always answers")
        .status()
}

/// **The 426 is logged at debug, never warn.** Every Codex session opens with a WebSocket
/// upgrade, so one 426 per session is the normal cost of the SSE fallback, and a warn line for it
/// would read as a fault per session. A pooled Codex account is free to serve, so nothing but the
/// upgrade decides the answer.
///
/// Watched red: build the 426 through `error_response` again and its warn line
/// ("proxy-generated error response") is logged.
#[tokio::test]
async fn codex_upgrade_426_is_logged_at_debug_not_warn() {
    capture();
    let manager = fleet(vec![anthropic("alice@example.com"), codex()]);
    let (status, log) = captured(send(
        manager,
        "GET",
        tcr::provider::CODEX_RESPONSES_PATH,
        &[("upgrade", "websocket"), ("connection", "Upgrade")],
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
    assert!(
        log.contains("answered a Codex WebSocket upgrade with 426"),
        "control: the 426 is logged: {log}"
    );
    assert!(
        !log.contains(" WARN "),
        "a 426 must log nothing at warn: {log}"
    );
}

/// **A Codex request is never offered to a peer's carry.** Codex stays off the mesh in this
/// version, and a blind carry is an offer to a peer as much as a lease is. The seam's own first
/// line for a host the mesh does not carry is the probe ([`OFFER`]), as in
/// `tests/peer_egress.rs`: it fires once per offer. The Codex fleet has one account, so the
/// ladder is out of moves on its first connect failure, which is exactly when an Anthropic
/// request IS offered (the control).
///
/// Watched red: drop `provider == Anthropic` from the carry's guard and the Codex request logs
/// the offer.
#[tokio::test]
async fn codex_request_is_never_offered_to_a_peer_carry() {
    capture();
    let codex_only = fleet(vec![codex()]);
    let anthropic_only = fleet(vec![anthropic("alice@example.com")]);

    let (status, log) = captured(send(
        codex_only,
        "POST",
        tcr::provider::CODEX_RESPONSES_PATH,
        &[("content-type", "application/json")],
        "{}",
    ))
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a connect-phase failure"
    );
    assert!(
        !log.contains(OFFER),
        "a Codex request was offered to a peer's carry: {log}"
    );

    let (status, log) = captured(send(
        anthropic_only,
        "POST",
        "/v1/messages",
        &[("content-type", "application/json")],
        "{}",
    ))
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        log.contains(OFFER),
        "control: an Anthropic request on the same dead origin IS offered: {log}"
    );
}

/// **A Codex account does not change when an Anthropic request is offered a peer's carry.**
/// `rotation_is_spent` asks whether the ladder has an untried account left for THIS request, and
/// an account of the other provider never is one: two Anthropic accounts plus a Codex one reach
/// the offer on exactly the attempt two Anthropic accounts alone reach it, with the same answer.
///
/// Watched red: compare against the fleet-wide `account_count` again and the mixed fleet is never
/// offered the carry (its ladder reads one account short of spent).
#[tokio::test]
async fn a_codex_account_does_not_delay_an_anthropic_requests_carry_offer() {
    capture();
    let mut outcomes = Vec::new();
    for with_codex in [false, true] {
        let mut accounts = vec![
            anthropic("alice@example.com"),
            anthropic("carol@example.com"),
        ];
        if with_codex {
            accounts.push(codex());
        }
        let (status, log) = captured(send(
            fleet(accounts),
            "POST",
            "/v1/messages",
            &[("content-type", "application/json")],
            "{}",
        ))
        .await;
        outcomes.push((status, log.matches(OFFER).count()));
    }
    assert_eq!(
        outcomes[0],
        (StatusCode::SERVICE_UNAVAILABLE, 1),
        "control: two Anthropic accounts are offered the carry once both refused the connect"
    );
    assert_eq!(
        outcomes[1], outcomes[0],
        "a Codex account must not change what an Anthropic request gets"
    );
}
