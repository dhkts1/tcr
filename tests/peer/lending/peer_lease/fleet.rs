// ---------------------------------------------------------------------------
// The fleet harness:a real `mitm::serve` in front of a real `Manager`, the
// same shape `tests/proxy/throttle_exempt.rs` uses. Nothing here reaches the network
// and nothing here touches the operator's config directory: every listener is `127.0.0.1:0` and
// every account is an obviously fake fixture.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::any;
use tcr::base::config::{Account, Config, ProxyConfig, ThrottleConfig};
use tcr::base::log_err::LogErr as _;
use tcr::clients::oauth::{OAuthError, RefreshFuture, TokenRefresher};
use tcr::clients::warmer::{AccountWarmer, WarmError, WarmFuture};
use tcr::manager::Manager;
use tcr::records::probe::{ProbeError, ProbeFuture, UsageProber};

/// The canned body the fake upstream answers with. A relayed request that
/// reached a real account comes back as exactly these bytes.
pub(crate) const CANNED: &[u8] = br#"{"type":"message","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;

/// The lender's one account. Its access token is the instrument: the fake
/// upstream records the Bearer it was called with, so "served on the
/// LENDER's account" is measured rather than assumed.
pub(crate) const LENDER_TOKEN: &str = "at-fake-lender";

/// A 200 carrying `body`, with the headers a healthy account answers with.
fn allowed_answer(body: Vec<u8>) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("anthropic-ratelimit-unified-status", "allowed")
        .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
        .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
        .body(Body::from(body))
        .expect("build the canned answer")
}

/// Drain a request's body, as the real API reads every byte it is sent.
async fn drain(req: axum::extract::Request) {
    axum::body::to_bytes(req.into_body(), 1024 * 1024)
        .await
        .log_debug("reading a test request body");
}

struct NeverRefreshes;
impl TokenRefresher for NeverRefreshes {
    fn refresh(
        &self,
        _provider: tcr::base::provider::Provider,
        _refresh_token: String,
    ) -> RefreshFuture {
        Box::pin(async { Err(OAuthError::Transient("no refresher in lease tests".into())) })
    }
}
struct NeverProbes;
impl UsageProber for NeverProbes {
    fn probe(&self, _credential: tcr::records::probe::ProbeCredential) -> ProbeFuture {
        Box::pin(async {
            Err(ProbeError {
                status: None,
                message: "no prober in lease tests".into(),
                retry_after_secs: None,
            })
        })
    }
}
struct NeverWarms;
impl AccountWarmer for NeverWarms {
    fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
        Box::pin(async {
            Err(WarmError {
                status: None,
                message: "no warmer in lease tests".into(),
            })
        })
    }
}

fn account() -> Account {
    Account {
        account_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
        // One org, so every borrowed request lands in ONE per-org GCRA
        // bucket: which is the bucket the pacing gate measures.
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        access_token: LENDER_TOKEN.to_string(),
        refresh_token: Some("rt-fake-lender".to_string()),
        ..Account::fixture("lender-fake")
    }
}

fn config(upstream: &str, accounts: Vec<Account>, account_throttle: ThrottleConfig) -> Config {
    Config {
        proxy: ProxyConfig {
            port: 0,
            ..ProxyConfig::default()
        },
        upstream: upstream.to_string(),
        switch_threshold: 0.98,
        account_throttle,
        ..Config::fixture(accounts)
    }
}

/// A manager with ONE account: the lender's fleet.
pub(crate) fn lending_manager(upstream: &str, account_throttle: ThrottleConfig) -> Arc<Manager> {
    Manager::new(
        config(upstream, vec![account()], account_throttle),
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// [`lending_manager`] whose one account is a CODEX account, with the Codex
/// origin pointed at `upstream` too: a lender that HAS an account a Codex
/// path could be served on, so a refusal of one is a decision and not an
/// accident of an Anthropic-only fleet.
pub(crate) fn codex_lending_manager(upstream: &str) -> Arc<Manager> {
    let mut config = config(
        upstream,
        vec![Account {
            provider: tcr::base::provider::Provider::Codex,
            ..account()
        }],
        ThrottleConfig::default(),
    );
    config.codex_upstream = upstream.to_string();
    Manager::new(
        config,
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// A fake upstream whose answer is `bytes` long: more than one relayed
/// frame carries, which is any long completion.
pub(crate) async fn spawn_huge_upstream(bytes: usize) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let counter = Arc::clone(&counter);
        async move {
            drain(req).await;
            counter.fetch_add(1, Ordering::SeqCst);
            allowed_answer(vec![b'x'; bytes])
        }
    }));
    (tcr::test_support::stub_upstream::http(app).url(), hits)
}

/// A fake upstream that holds every answer for `delay` before sending it,
/// so a borrow whose deadline is shorter runs out of time AFTER its body
/// has crossed to the lender and reached the origin.
pub(crate) async fn spawn_slow_upstream(delay: std::time::Duration) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let counter = Arc::clone(&counter);
        async move {
            drain(req).await;
            // Counted on ARRIVAL, before the delay: the question this
            // upstream answers is how many times the fleet sent one
            // request, and a request still in flight has already been paid
            // for.
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            allowed_answer(CANNED.to_vec())
        }
    }));
    (tcr::test_support::stub_upstream::http(app).url(), hits)
}

/// A fake upstream that rejects every request with a long 429, so the one
/// account that tried it is put on a hold the picker will not come back to
/// inside this test.
pub(crate) async fn spawn_rejecting_upstream() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let counter = Arc::clone(&counter);
        async move {
            drain(req).await;
            counter.fetch_add(1, Ordering::SeqCst);
            Response::builder()
                .status(429)
                .header("content-type", "application/json")
                .header("retry-after", "3600")
                .header("anthropic-ratelimit-unified-status", "rejected")
                .header("anthropic-ratelimit-unified-5h-status", "rejected")
                .body(Body::from(
                    br#"{"type":"error","error":{"type":"rate_limit_error"}}"#.to_vec(),
                ))
                .expect("build the canned 429")
        }
    }));
    (tcr::test_support::stub_upstream::http(app).url(), hits)
}

/// A fleet of ONE account, LOCKED to that account by name.
///
/// `lock_account` is the operator saying "this account or nothing", so the
/// picker answers `None` and the request reaches the same terminal a dry
/// fleet does. The difference is what may happen there: a dry fleet may
/// borrow, and a locked one may not, because a peer's credential is not
/// the account the operator pinned this fleet to.
pub(crate) fn locked_manager(upstream: &str) -> Arc<Manager> {
    let locked = account();
    let name = locked.name.clone();
    let mut config = config(upstream, vec![locked], ThrottleConfig::default());
    config.lock_account = Some(name);
    Manager::new(
        config,
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// A manager with NO accounts at all: the dry fleet, which is the only
/// condition under which the fallback seam is consulted.
pub(crate) fn dry_manager() -> Arc<Manager> {
    Manager::new(
        config("http://127.0.0.1:1", Vec::new(), ThrottleConfig::default()),
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// One request the fake upstream answered, as the LENDER's own proxy
/// actually sent it.
///
/// Grew from a bare Bearer string: the header ALLOWLIST gate
/// asks what the lender's outbound request carried, and a recorder that
/// keeps one header cannot answer a question about the others.
#[derive(Debug, Clone)]
pub(crate) struct SeenRequest {
    /// The Bearer, with `Bearer ` stripped, or `<none>`.
    pub token: String,
    /// Every header name and value, lower-cased names.
    pub headers: Vec<(String, String)>,
    /// The request target as it arrived, path and query together. A
    /// recorder that kept only the headers could not answer whether a
    /// borrowed request still carried the parameters its client sent.
    pub uri: String,
}

impl SeenRequest {
    /// The first value under `name`, or `None`.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(seen, _)| seen == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every header value, for an assertion about a SECRET rather than
    /// about a name: a borrower's cookie arriving under a name nobody
    /// thought to check is invisible to a per-name assertion.
    pub(crate) fn values(&self) -> Vec<&str> {
        self.headers
            .iter()
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// Every request the fake upstream was called with, in order.
pub(crate) type ServedBy = Arc<Mutex<Vec<SeenRequest>>>;

/// A fake upstream that answers [`CANNED`] and records who asked, served
/// as a route on the binary's shared stub listener.
pub(crate) async fn spawn_upstream() -> (String, Arc<AtomicUsize>, ServedBy) {
    let (app, hits, served) = recording_app();
    (
        tcr::test_support::stub_upstream::http(app).url(),
        hits,
        served,
    )
}

/// [`spawn_upstream`] on a kernel port of its own, for an ORIGIN a hand-mode
/// borrower sends to: `serve_on_handed_bearer` sets the request's path on
/// the origin URL rather than appending to it, so a shared stub's
/// `/stub/<id>` prefix would be dropped and the request would miss it.
pub(crate) async fn spawn_origin() -> (String, Arc<AtomicUsize>, ServedBy) {
    let (app, hits, served) = recording_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake origin");
    let addr = listener.local_addr().expect("origin addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .log_debug("serving a test server");
    });
    (format!("http://{addr}"), hits, served)
}

fn recording_app() -> (Router, Arc<AtomicUsize>, ServedBy) {
    let hits = Arc::new(AtomicUsize::new(0));
    let served: ServedBy = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::clone(&hits);
    let recorder = Arc::clone(&served);
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let counter = Arc::clone(&counter);
        let recorder = Arc::clone(&recorder);
        async move {
            let token = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .trim_start_matches("Bearer ")
                .to_string();
            // The origin's own refusal. Decided BEFORE the body is drained
            // and answered after the request has been recorded: the
            // request really did arrive, so an instrument that counts
            // arrivals must see it, and a request the real API answers 400
            // to is not one this fake upstream may answer 200 to. See
            // `tests/tools/api_contract.rs`.
            let refusal = super::api_contract::refuse_if_incomplete(req.method(), req.headers());
            let uri = req.uri().to_string();
            let headers = req
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        String::from_utf8_lossy(value.as_bytes()).to_string(),
                    )
                })
                .collect();
            drain(req).await;
            counter.fetch_add(1, Ordering::SeqCst);
            recorder.lock().expect("served-by lock").push(SeenRequest {
                token,
                headers,
                uri,
            });
            if let Some(refusal) = refusal {
                return refusal;
            }
            allowed_answer(CANNED.to_vec())
        }
    }));
    (app, hits, served)
}

/// A real proxy on a kernel port in front of `manager`.
pub(crate) async fn spawn_proxy(manager: Arc<Manager>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        tcr::proxy::mitm::serve(listener, manager, None).await;
    });
    format!("http://{addr}")
}

pub(crate) const fn tight_throttle() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(350),
        burst: Some(1),
    }
}
