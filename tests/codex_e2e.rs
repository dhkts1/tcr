//! The real `tcr` binary, a scratch HOME, a free port, and two stub origins, one as `upstream` and
//! one as `codexUpstream`, that answer the Codex paths with canned SSE and `x-codex-*` headers.
//! This is the test that proves the hops the unit suites each take on faith: that a Codex turn
//! leaves for the Codex origin with the pooled bearer AND the swapped account id, that affinity
//! holds, that an upgrade never reaches an origin, that relays keep the client's bearer and add
//! none to a call that came without one, and that a Claude request never lands on a Codex
//! account.
//!
//! Never binds 3456 (`dead_port`). Never reads a real credential (`TCR_CODEX_AUTH` and
//! `TCR_CLAUDE_CODE_CREDENTIALS` point at files that do not exist).
//!
//! # Isolation
//!
//! Pinned on every spawned [`Command`] ([`isolate`]), never on this test process: `HOME` is a
//! fresh [`tempfile::TempDir`] with the XDG overrides removed, so the config, the MITM material,
//! the usage ledger and the affinity pins all land in it; every proxy variable (`HTTPS_PROXY`,
//! `HTTP_PROXY`, `ALL_PROXY`, either case) is removed so no ambient proxy can carry a loopback
//! call elsewhere; and the working directory is that HOME, so nothing a child runs looks at the
//! developer's checkout (`tcr status` reads the build of the checkout it is started in). The
//! config turns the quota probe off (`quotaProbeSeconds: 0`, as `tests/peer_e2e.rs` does): the
//! server's boot sweep would otherwise send carol's fixture bearer to `probe::USAGE_URL`, which
//! is api.anthropic.com whatever `upstream` says, and its Codex half would put
//! `/backend-api/wham/usage` calls in the ledger beside the ones these tests read, filling the
//! very bars step 3 says come from the response headers. The config gives carol no refresh token
//! for the same reason: should the server under test die or hang, `tcr status --json` falls back
//! to an offline sweep of every row that has one (`Manager::probeable_indices`), which ignores
//! `quotaProbeSeconds` and would send her bearer to that URL too; alice and bob keep theirs,
//! since their offline probe goes to the loopback `codexUpstream`. The server's stdout and stderr
//! go to a log in the scratch HOME, printed when a test fails.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

#[path = "tools/api_contract.rs"]
mod api_contract;

const ACCOUNT_ID_A: &str = "11111111-1111-4111-8111-111111111111";
const ACCOUNT_ID_B: &str = "22222222-2222-4222-8222-222222222222";

/// How long a debug-build `tcr server` gets to log that it is listening. Bounded so a server that
/// never comes up fails the test instead of hanging it.
const BOOT_TIMEOUT: Duration = Duration::from_secs(20);

/// The ceiling on any one request this file sends, so a wedged proxy fails a test rather than
/// stalling the suite.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The server's stdout and stderr, inside its scratch HOME.
const SERVER_LOG: &str = "tcr-server.log";

/// The origin a [`Seen`] reached: the stub serving as `upstream`, or the one serving as
/// `codexUpstream`.
const ANTHROPIC_ORIGIN: &str = "anthropic";
const CODEX_ORIGIN: &str = "codex";

/// One request as a stub origin received it.
#[derive(Clone, Debug)]
struct Seen {
    origin: &'static str,
    path: String,
    authorization: Option<String>,
    account_id: Option<String>,
    body: Vec<u8>,
}

/// Every request either stub origin received, in arrival order.
#[derive(Clone, Default)]
struct Ledger(Arc<Mutex<Vec<Seen>>>);

impl Ledger {
    fn all(&self) -> Vec<Seen> {
        self.0.lock().unwrap().clone()
    }

    fn last(&self) -> Seen {
        self.all().last().cloned().expect("the stub saw a request")
    }
}

/// `/backend-api/wham/usage`'s answer: the same two windows the inference headers report.
const WHAM_USAGE: &str = r#"{"plan_type":"pro","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_after_seconds":3600,"reset_at":1800003600},"secondary_window":{"used_percent":3,"limit_window_seconds":604800,"reset_after_seconds":86400,"reset_at":1800086400}}}"#;

/// `/v1/messages`'s answer.
const MESSAGE: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"output_tokens":2}}"#;

fn codex_sse_response() -> Response {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-astra\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-astra\",\"usage\":{\"input_tokens\":1200,\"input_tokens_details\":{\"cached_tokens\":1000},\"output_tokens\":40,\"total_tokens\":1240}}}\n\n",
    );
    (
        StatusCode::OK,
        [
            ("content-type", "text/event-stream".to_string()),
            ("x-codex-primary-used-percent", "12".to_string()),
            ("x-codex-primary-window-minutes", "300".to_string()),
            ("x-codex-primary-reset-at", (now + 3600).to_string()),
            ("x-codex-secondary-used-percent", "3".to_string()),
            ("x-codex-secondary-window-minutes", "10080".to_string()),
            ("x-codex-secondary-reset-at", (now + 86_400).to_string()),
        ],
        body,
    )
        .into_response()
}

/// One stub origin: the ledger it shares with the other, and which origin it is.
#[derive(Clone)]
struct Stub {
    ledger: Ledger,
    origin: &'static str,
}

/// A stub origin: records every request, tagged with the origin it reached, then answers by
/// path. Both origins answer every path, so a request sent to the wrong one is still answered,
/// and only its tag shows where it went.
async fn stub_handler(
    State(Stub { ledger, origin }): State<Stub>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap_or_else(|_| Bytes::new());
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    ledger.0.lock().unwrap().push(Seen {
        origin,
        path: path.clone(),
        authorization: header("authorization"),
        account_id: header("chatgpt-account-id"),
        body: body.to_vec(),
    });
    match path.as_str() {
        "/backend-api/codex/responses" => codex_sse_response(),
        "/backend-api/wham/usage" => (
            StatusCode::OK,
            [("content-type", "application/json")],
            WHAM_USAGE,
        )
            .into_response(),
        "/v1/messages" => {
            // The real origin refuses a message request without `anthropic-version` or
            // `content-type`: the one rule every fake upstream here reads,
            // tests/tools/api_contract.rs.
            if let Some(refusal) = api_contract::refuse_if_incomplete(&method, &headers) {
                return refusal;
            }
            (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("anthropic-ratelimit-unified-5h-utilization", "0.10"),
                ],
                MESSAGE,
            )
                .into_response()
        }
        _ => (StatusCode::NOT_FOUND, "stub: unknown path").into_response(),
    }
}

async fn spawn_stub(origin: &'static str, ledger: Ledger) -> String {
    let app = Router::new()
        .fallback(any(stub_handler))
        .with_state(Stub { ledger, origin });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// The two stub origins on one ledger, as `(upstream, codexUpstream, ledger)`.
async fn spawn_origins() -> (String, String, Ledger) {
    let ledger = Ledger::default();
    let anthropic = spawn_stub(ANTHROPIC_ORIGIN, ledger.clone()).await;
    let codex = spawn_stub(CODEX_ORIGIN, ledger.clone()).await;
    (anthropic, codex, ledger)
}

/// A port nothing is listening on: bound to learn the number, then released. Copied from
/// `tests/claude_code_import.rs`: each file under `tests/` is its own crate, so it cannot be
/// imported.
fn dead_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port must work");
    let port = listener
        .local_addr()
        .expect("a bound listener has an address")
        .port();
    drop(listener);
    port
}

/// A client that never sends a loopback request through an ambient proxy.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap()
}

/// Point a `tcr` child at the scratch `home` and nothing else: see the module doc.
fn isolate(command: &mut Command, home: &Path) {
    command
        .current_dir(home)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .env("TCR_CLAUDE_CODE_CREDENTIALS", home.join("no-claude-login"))
        .env("TCR_CODEX_AUTH", home.join("no-codex-login"));
}

/// A running `tcr server` on its own scratch HOME. Dropping it kills the process, prints its log
/// if the test is failing, and removes the HOME.
struct Server {
    child: Child,
    port: u16,
    home: tempfile::TempDir,
    config: PathBuf,
}

impl Server {
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.path().join(SERVER_LOG)).unwrap_or_default()
    }

    /// Run `tcr <args…> --config <this server's config>` under the server's isolation, so the
    /// verb reads this server's port from the file and talks to this server alone.
    async fn tcr(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
        command.args(args).arg("--config").arg(&self.config);
        isolate(&mut command, self.home.path());
        tokio::task::spawn_blocking(move || command.output())
            .await
            .expect("the blocking task ran")
            .expect("spawning the built tcr binary")
    }

    /// `tcr disable <name>`, which the running server applies and writes to the config.
    async fn disable(&self, name: &str) {
        let out = self.tcr(&["disable", name]).await;
        assert!(
            out.status.success(),
            "tcr disable {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The fleet as `tcr status --json` prints it: a bare array, one row per account.
    async fn status_rows(&self) -> Vec<serde_json::Value> {
        let out = self.tcr(&["status", "--json"]).await;
        assert!(
            out.status.success(),
            "tcr status --json: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let status: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("status --json is JSON");
        status
            .as_array()
            .cloned()
            .expect("status --json is an array of account rows")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!("--- tcr server log (:{}) ---\n{}", self.port, self.log());
        }
    }
}

/// Three accounts, carol on Anthropic and alice and bob on Codex, and an origin for each
/// provider.
///
/// The two Codex rows share the default priority on purpose. Fresh, they tie on every selection
/// key and the first in the file, alice, takes the first turn; once she has served, the unpinned
/// pick is bob. So a second turn that stays on alice is the `session-id` pin and nothing else.
/// With bob a priority behind, alice wins every pick and step 2 passes with the pin deleted.
fn write_config(path: &Path, port: u16, upstream: &str, codex_upstream: &str) {
    std::fs::write(path, format!(r#"{{
  "proxy": {{ "port": {port} }},
  "upstream": "{upstream}",
  "codexUpstream": "{codex_upstream}",
  "sessionAffinity": true,
  "quotaProbeSeconds": 0,
  "accounts": [
    {{ "name": "carol@example.com", "accessToken": "at-carol", "expiresAt": 4102444800000 }},
    {{ "name": "alice@example.com/codex", "provider": "codex", "accountUuid": "{ACCOUNT_ID_A}", "accessToken": "at-alice", "refreshToken": "rt-alice", "expiresAt": 4102444800000 }},
    {{ "name": "bob@example.com/codex", "provider": "codex", "accountUuid": "{ACCOUNT_ID_B}", "accessToken": "at-bob", "refreshToken": "rt-bob", "expiresAt": 4102444800000 }}
  ]
}}"#)).unwrap();
}

/// Boot `tcr server --headless` on a scratch HOME whose config sends Anthropic requests to
/// `upstream` and Codex requests to `codex_upstream`, and wait until THIS child has bound its
/// port.
///
/// The child logs `listening on http://127.0.0.1:<port> (headless)` only after its own bind
/// succeeded (`src/main.rs`, once `server::serve` has started), so that line is the readiness
/// signal. An answer on the port proves less: `dead_port` releases the number before the child
/// binds it, and a stranger that took it in that window would answer in the child's place, which
/// for a sibling `codex_e2e` server, serving the same fleet, could pass a test it never ran.
/// `RUST_LOG` is pinned because the line is logged at info.
async fn start_server(upstream: &str, codex_upstream: &str) -> Server {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config").join("teamclaude.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let port = dead_port();
    write_config(&config, port, upstream, codex_upstream);
    let log = std::fs::File::create(home.path().join(SERVER_LOG)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    command
        .args(["server", "--headless", "--config"])
        .arg(&config)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    isolate(&mut command, home.path());
    let child = command.spawn().unwrap();
    let mut server = Server {
        child,
        port,
        home,
        config,
    };
    let listening = format!("listening on http://127.0.0.1:{port} (headless)");
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if server.log().contains(&listening) {
            return server;
        }
        if let Some(exit) = server.child.try_wait().unwrap() {
            panic!("tcr server exited ({exit}) before it listened on :{port}");
        }
        assert!(
            Instant::now() < deadline,
            "tcr server did not come up on :{port}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Every request either origin saw reached its own provider's origin: a `/backend-api/` path the
/// Codex one, anything else the Anthropic one.
fn assert_each_reached_its_own_origin(ledger: &Ledger) {
    for seen in ledger.all() {
        let own = if seen.path.starts_with("/backend-api/") {
            CODEX_ORIGIN
        } else {
            ANTHROPIC_ORIGIN
        };
        assert_eq!(seen.origin, own, "{} reached the wrong origin", seen.path);
    }
}

/// One Codex session through the real binary: its turns leave for the Codex origin on the pooled
/// account's bearer and id, stay on that account, fill its bars from the response headers, have
/// their WebSocket upgrade answered here, and have a non-inference call relayed on the client's
/// own bearer, or on none when it came without one.
///
/// Watched red, each break restored after: drop the account-id swap in
/// `proxy::build_codex_upstream_headers` and step 1 sees the client's own id; send Codex requests
/// to `upstream` in `Manager::upstream_for` and step 1 reaches the Anthropic origin; drop the
/// provider check in `manager::select::eligible` and step 1 is served on carol; skip the
/// `session-id` tier of `proxy::stable_session_key` and step 2 lands on bob; read the header's
/// percent as the fraction in `Quota::update_from_codex_headers` (`used` for `used / 100.0`) and
/// step 3 reads 12.0; skip the local 426 in `proxy::handle` and step 4 gets the origin's 200;
/// skip the relay for Codex paths in `proxy::handle` and step 5 is served on bob's pooled bearer;
/// drop the Codex arm of `proxy::relay_mode` and step 6, which carries no bearer, is pooled.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_turn_is_pooled_pinned_measured_and_relayed_correctly() {
    let (upstream, codex_upstream, ledger) = spawn_origins().await;
    let server = start_server(&upstream, &codex_upstream).await;
    let base = server.base();
    let client = client();

    // 1. Inference: pooled bearer + swapped account id; nothing reaches an Anthropic path.
    let resp = client
        .post(format!("{base}/backend-api/codex/responses"))
        .header("authorization", "Bearer client-own")
        .header("chatgpt-account-id", "99999999-9999-4999-8999-999999999999")
        .header("session-id", "sess-1")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-6-astra","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap(); // drain the SSE so the tee finishes
    let first = ledger.last();
    assert_eq!(first.path, "/backend-api/codex/responses");
    assert_eq!(
        first.origin, CODEX_ORIGIN,
        "a Codex turn goes to codexUpstream"
    );
    assert_eq!(
        first.authorization.as_deref(),
        Some("Bearer at-alice"),
        "alice, first of two tied Codex accounts, serves first"
    );
    assert_eq!(
        first.account_id.as_deref(),
        Some(ACCOUNT_ID_A),
        "the pooled account's id replaces the client's"
    );

    // 2. Affinity: same session-id, same account, even with a different body. Unpinned, this turn
    //    would go to bob now that alice has served (see `write_config`).
    let resp = client
        .post(format!("{base}/backend-api/codex/responses"))
        .header("authorization", "Bearer client-own")
        .header("session-id", "sess-1")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-6-astra","input":[1]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();
    let second = ledger.last();
    assert_eq!(
        String::from_utf8_lossy(&second.body),
        r#"{"model":"gpt-6-astra","input":[1]}"#,
        "the origin saw the second turn"
    );
    assert_eq!(second.authorization.as_deref(), Some("Bearer at-alice"));

    // 3. Quota bars populated from the response headers.
    let rows = server.status_rows().await;
    let alice = rows
        .iter()
        .find(|a| a["name"] == "alice@example.com/codex")
        .expect("alice row");
    // An offline snapshot would probe the stub's usage endpoint, which reports the same
    // numbers: only the live server's row says what the response headers did.
    assert_eq!(alice["source"], "live", "{alice}");
    let bar = |key: &str| {
        alice[key]
            .as_f64()
            .unwrap_or_else(|| panic!("no {key} reading: {alice}"))
    };
    assert!((bar("fiveHour") - 0.12).abs() < 1e-9, "{alice}");
    assert!((bar("sevenDay") - 0.03).abs() < 1e-9, "{alice}");
    assert_eq!(alice["provider"], "codex");

    // 4. A WebSocket upgrade is answered 426 and never forwarded.
    let before = ledger.all().len();
    let resp = client
        .get(format!("{base}/backend-api/codex/responses"))
        .header("authorization", "Bearer client-own")
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        // 16 zero bytes: a valid key shape with no entropy, so the pre-commit gitleaks gate
        // ignores it (the RFC 6455 sample nonce trips its generic-api-key rule).
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "AAAAAAAAAAAAAAAAAAAAAA==")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 426);
    assert_eq!(
        ledger.all().len(),
        before,
        "the origin must see no upgrade attempt"
    );

    // 5. A non-inference Codex path relays the CLIENT's bearer.
    let resp = client
        .get(format!("{base}/backend-api/wham/usage"))
        .header("authorization", "Bearer client-own")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let relayed = ledger.last();
    assert_eq!(relayed.path, "/backend-api/wham/usage");
    assert_eq!(relayed.authorization.as_deref(), Some("Bearer client-own"));
    assert_eq!(
        relayed.account_id, None,
        "we add no identity to a relayed call"
    );

    // 6. One with no bearer at all is relayed as it came. Only Codex's own arm of the relay
    //    decision relays it: the generic arm relays a call only when it carries the client's
    //    bearer, which is why step 5 cannot tell the two arms apart.
    let resp = client
        .get(format!("{base}/backend-api/wham/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let bare = ledger.last();
    assert_eq!(bare.path, "/backend-api/wham/usage");
    assert_eq!(
        bare.authorization, None,
        "a call that came without a bearer leaves without one, never with a pooled one"
    );
    assert_eq!(bare.account_id, None, "and with no account id");

    assert_each_reached_its_own_origin(&ledger);
}

/// Watched red: drop the provider check in `manager::select::eligible` and the second
/// `/v1/messages` is served on `Bearer at-alice`, the unpinned pick once carol has served; with
/// that in-loop assertion lifted, the request after the disable gets 200, not 429.
#[tokio::test(flavor = "multi_thread")]
async fn an_anthropic_request_never_lands_on_a_codex_account() {
    let (upstream, codex_upstream, ledger) = spawn_origins().await;
    let server = start_server(&upstream, &codex_upstream).await;
    let client = client();
    let messages = || {
        client
            .post(format!("{}/v1/messages", server.base()))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-opus-5","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}"#)
    };
    for _ in 0..5 {
        let resp = messages().send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            ledger.last().authorization.as_deref(),
            Some("Bearer at-carol"),
            "only the Anthropic account may serve /v1/messages"
        );
    }
    // Disable carol via the live endpoint; the next Claude request must be refused, not spilled.
    server.disable("carol@example.com").await;
    let resp = messages().send().await.unwrap();
    assert_eq!(
        resp.status(),
        429,
        "an exhausted Anthropic fleet answers 429, never a Codex account"
    );
    assert!(
        !ledger
            .all()
            .iter()
            .any(|s| s.path == "/v1/messages"
                && s.authorization.as_deref() != Some("Bearer at-carol")),
        "no /v1/messages may reach the origin on a Codex bearer"
    );
    assert_each_reached_its_own_origin(&ledger);
}

/// The other direction of the spill, and the error shape a Codex client reads. Every Codex
/// account is disabled, which is no usage limit, so the 429 is the plain one in codex's own
/// envelope (`pool_unavailable`), which codex retries rather than ending its turn.
///
/// Watched red: set the `type` in `proxy::codex_pool_unavailable_body` to `rate_limit_error`,
/// or drop the Codex arm of `proxy::exhausted_response` so the Anthropic envelope goes out, and
/// the type assertion fails; send `proxy::codex_exhausted_body` whatever gates the pool and it
/// reads `usage_limit_reached`; drop the provider check in `manager::select::eligible` and
/// carol serves the request (200).
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_request_with_the_codex_pool_exhausted_gets_the_codex_error_shape() {
    let (upstream, codex_upstream, _ledger) = spawn_origins().await;
    let server = start_server(&upstream, &codex_upstream).await;
    for name in ["alice@example.com/codex", "bob@example.com/codex"] {
        server.disable(name).await;
    }
    let resp = client()
        .post(format!("{}/backend-api/codex/responses", server.base()))
        .header("authorization", "Bearer client-own")
        .header("session-id", "s")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "pool_unavailable", "{body}");
}

/// A Codex origin that refuses every inference request the way a ChatGPT Pro login is refused at
/// its weekly limit: `429`, its one window (the weekly one, in the primary slot) full until
/// `weekly_reset` (Unix seconds), the refusal's kind, and the account's credits. Any other path
/// is a 404.
async fn spawn_weekly_refusing_codex_origin(weekly_reset: u64) -> String {
    let app = Router::new().fallback(move |req: axum::extract::Request| async move {
        if req.uri().path() != "/backend-api/codex/responses" {
            return (StatusCode::NOT_FOUND, "stub: unknown path").into_response();
        }
        (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("content-type", "application/json".to_string()),
                ("x-codex-primary-used-percent", "100".to_string()),
                ("x-codex-primary-window-minutes", "10080".to_string()),
                ("x-codex-primary-reset-at", weekly_reset.to_string()),
                (
                    "x-codex-rate-limit-reached-type",
                    "rate_limit_reached".to_string(),
                ),
                ("x-codex-credits-has-credits", "true".to_string()),
                ("x-codex-credits-unlimited", "false".to_string()),
                ("x-codex-credits-balance", "12.50".to_string()),
            ],
            r#"{"error":{"type":"usage_limit_reached","message":"out"}}"#,
        )
            .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Both Codex accounts at their weekly limit, through the real binary: `tcr status --json`
/// reports each as `seven-day` with the time the window rolls, not as `rejected` with no time,
/// though the hour-long hold the refusal armed is still there. Each row carries what the refusal
/// said: its kind, which windows the plan has (the weekly one alone) and how long it is, and the
/// credits, all read off headers the client never saw. The Anthropic row carries none of it.
///
/// Watched red: drop the `codex_refusal_gate` call in `Manager::snapshot` and both rows read
/// `rejected`.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_usage_limit_reports_the_refusing_window_on_status_json() {
    let upstream = spawn_stub(ANTHROPIC_ORIGIN, Ledger::default()).await;
    let weekly_reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3 * 86_400;
    let refusing_codex = spawn_weekly_refusing_codex_origin(weekly_reset).await;
    let server = start_server(&upstream, &refusing_codex).await;

    // alice is refused, the rotation tries bob, he is refused too, and tcr answers.
    let resp = client()
        .post(format!("{}/backend-api/codex/responses", server.base()))
        .header("authorization", "Bearer client-own")
        .header("session-id", "s")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-6-astra","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);

    let rows = server.status_rows().await;
    for name in ["alice@example.com/codex", "bob@example.com/codex"] {
        let row = rows
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("no {name} row"));
        assert_eq!(row["source"], "live", "{row}");
        assert_eq!(row["gate"], "seven-day", "{row}");
        assert_eq!(row["freeAtMs"], weekly_reset * 1000, "{row}");
        assert!(
            row["secondsUntilFree"].as_i64().is_some_and(|s| s > 0),
            "{row}"
        );
        assert!(
            row["rateLimitedUntilMs"].is_number(),
            "the hold is armed, and the window outranks it: {row}"
        );
        assert_eq!(row["limitReachedType"], "rate_limit_reached", "{row}");
        assert_eq!(
            row["quotaWindows"],
            serde_json::json!(["sevenDay"]),
            "{row}"
        );
        assert_eq!(row["sevenDayWindowMinutes"], 10_080, "{row}");
        assert!(row.get("fiveHourWindowMinutes").is_none(), "{row}");
        assert_eq!(
            row["credits"],
            serde_json::json!({"balance": "12.50", "hasCredits": true, "unlimited": false}),
            "{row}"
        );
        assert!(row.get("spendControlReached").is_none(), "{row}");
    }

    let carol = rows
        .iter()
        .find(|row| row["name"] == "carol@example.com")
        .expect("carol row");
    for key in [
        "quotaWindows",
        "fiveHourWindowMinutes",
        "sevenDayWindowMinutes",
        "limitReachedType",
        "spendControlReached",
        "credits",
    ] {
        assert!(
            carol.get(key).is_none(),
            "the Anthropic row carries no {key}: {carol}"
        );
    }
}

/// One Codex session through the real binary: two turns naming the same `session-id` are one
/// session on `tcr sessions`, attributed to the Codex account that served them, filed under the
/// model the stream named, unpriced instead of priced at zero, with no Anthropic cache TTL. The
/// text line names the provider and prints `costUsd=n/a`.
///
/// Watched red: write `provider: None` for every row in `Manager::wire_sessions_snapshot`'s
/// projection and the JSON row's `provider` reads null.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_turn_records_one_codex_session_live() {
    let (upstream, codex_upstream, _ledger) = spawn_origins().await;
    let server = start_server(&upstream, &codex_upstream).await;
    let base = server.base();
    let client = client();
    for turn in 1..=2 {
        let resp = client
            .post(format!("{base}/backend-api/codex/responses"))
            .header("authorization", "Bearer client-own")
            .header("session-id", "sess-live")
            .header("content-type", "application/json")
            .body(r#"{"input":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "turn {turn}");
        // Read the stream to its end, as codex does, so the tee sees the closing event.
        resp.bytes().await.unwrap();
    }
    // The usage half is booked by the tee's detached task after the stream ends: poll.
    let deadline = Instant::now() + Duration::from_secs(10);
    let session = loop {
        let out = server.tcr(&["sessions", "--json"]).await;
        assert!(
            out.status.success(),
            "tcr sessions --json: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let doc: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("sessions --json is JSON");
        let row = doc["sessions"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["sessionId"] == "sess-live"))
            .cloned();
        if let Some(row) = row.filter(|row| row["outputTokens"] == 80) {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "the session never showed both turns' usage: {doc}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(session["provider"], "codex");
    assert_eq!(
        session["account"], "alice@example.com/codex",
        "both turns pinned to the first pick"
    );
    assert_eq!(
        session["model"], "gpt-6-astra",
        "the stream named it; the body named none"
    );
    assert_eq!(session["requests"], 2);
    assert_eq!(session["unpricedRequests"], 2);
    assert_eq!(session["costUsd"], 0.0);
    assert_eq!(
        session["inputTokens"], 400,
        "base input: 1200 less the 1000 cached, twice"
    );
    assert_eq!(session["cacheReadTokens"], 2000);
    assert!(
        session.get("cacheTtlSecs").is_none(),
        "no Anthropic cache TTL on a Codex session"
    );
    assert!(
        session.get("reasoningTokens").is_none(),
        "the stub's turns carry no reasoning"
    );
    let text = server.tcr(&["sessions"]).await;
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        stdout.contains(
            "session=sess-live account=alice@example.com/codex provider=codex model=gpt-6-astra requests=2 inputTokens=400 outputTokens=80 cacheReadTokens=2000 toolCalls=0 costUsd=n/a\n"
        ),
        "{stdout}"
    );
}
