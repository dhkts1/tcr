use super::*;
use crate::config::{Account, Config, ProxyConfig};

/// An obviously fake proxy key: the gate is exercised, no real key is used.
const KEY: &str = "3333333333333333333333333333333333333333333333333333333333333333";

/// A keyed manager over one fake account whose upstreams are dead ports.
fn manager() -> Arc<Manager> {
    Manager::with_live_refresher(
        Config {
            proxy: ProxyConfig {
                port: 0,
                api_key: Some(KEY.to_string()),
                request_key: None,
                extra: serde_json::Map::new(),
            },
            upstream: "http://127.0.0.1:1".to_string(),
            codex_upstream: "http://127.0.0.1:1".to_string(),
            ..Config::fixture(vec![Account::fixture("alice@example.com")])
        },
        None,
    )
}

/// `GET path` from a loopback peer carrying the key, answered by the router
/// in-process: the [`ClientAddr`] is the extension the listener injects.
async fn get(manager: Arc<Manager>, path: &str) -> (StatusCode, HeaderMap, Bytes) {
    use tower::ServiceExt as _;
    let mut req = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header("x-api-key", KEY)
        .body(Body::empty())
        .expect("build request");
    req.extensions_mut()
        .insert(ClientAddr(SocketAddr::from(([127, 0, 0, 1], 54_321))));
    let response = app(manager).oneshot(req).await.expect("router response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, headers, body)
}

/// History and live whole-percent ticks reach the launcher's existing local surface.
/// All data is synthetic; neither a real ledger nor a quota endpoint is read.
#[tokio::test]
async fn history_and_ticks() {
    let manager = manager();
    let now = OffsetDateTime::now_utc();
    let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
    let ledger = fable_ledger(now_ms);
    manager.attach_usage_ledger(ledger.path().to_path_buf(), 29);
    let reset = now_ms + 3_600_000;
    let tick = |percent: u64| {
        let bucket = Some(crate::probe::UsageBucket {
            utilization: Some(percent as f64 / 100.0),
            reset_at_ms: Some(reset),
        });
        crate::probe::Usage {
            five_hour: bucket.into(),
            seven_day: bucket,
            seven_day_oi: bucket,
            ..Default::default()
        }
    };
    manager.apply_usage_at(0, &tick(10), now - time::Duration::seconds(60));
    for step in 1..=20 {
        let at = now - time::Duration::seconds(60 - step * 2);
        let (input, output) = if step % 2 == 0 { (100, 0) } else { (0, 25) };
        manager.record_usage(
            0,
            crate::usage::UsageRecord::from_quota_input(
                (at.unix_timestamp_nanos() / 1_000_000) as i64 - 1,
                Some("claude-fable-5".to_string()),
                None,
                input,
                0,
                0,
                0,
                output,
            ),
        );
        manager.apply_usage_at(0, &tick(10 + step as u64), at);
    }
    let value = document(Arc::clone(&manager)).await;
    let estimate = &value["forecast"]["providers"][0];
    let profile_and_scale_match =
        estimate["profile"].as_array().is_some_and(|p| {
            p.len() == 168 && (p.iter().filter_map(Value::as_f64).sum::<f64>() - 168.0).abs() < 1e-6
        }) && estimate["historyHours"].as_f64().is_some_and(|h| h >= 24.0)
            && estimate["level"]["fable"]["input"]
                .as_f64()
                .is_some_and(|x| x > 0.0)
            && estimate["calibrated"] == true;
    assert_eq!(
        manager.shutdown_usage_ledger(Duration::from_secs(2)),
        crate::usage::LedgerShutdown::Flushed
    );
    assert!(
        profile_and_scale_match,
        "history and ticks must yield estimates"
    );
    assert!(value["forecast"]["task"].is_null());
    assert!(value["forecast"]["bestProvider"].is_null());
}

/// A ledger of one synthetic Fable request an hour, from 2 to 48 hours before `now_ms`,
/// so the forecast has demand history to estimate from.
fn fable_ledger(now_ms: i64) -> tempfile::TempDir {
    use std::io::Write as _;
    let ledger = tempfile::tempdir().expect("owned ledger");
    for back in 2..=48 {
        let ts = now_ms - back * 3_600_000;
        let date = crate::usage::date_string(crate::usage::ledger_date(ts));
        let path = ledger.path().join(format!("{date}.jsonl"));
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap(),
            "{}",
            serde_json::json!({"t":ts,"a":"alice@example.com","m":"claude-fable-5",
                    "i":100,"c5":0,"c1":0,"r":900,"o":25})
        )
        .unwrap();
    }
    ledger
}

/// **Served responses' own rate-limit headers calibrate the forecast.** Each response
/// reports the whole percent spent before its request, and each request spends one
/// more, alternating input and output. The proxy reads those headers while the
/// request's in-flight guard is held, so a reading that counted the request itself as
/// unfinished would never train, and the document would stay uncalibrated.
#[tokio::test]
async fn served_response_headers_calibrate_under_their_own_request() {
    let now_ms = crate::now_ms();
    let reset_secs = now_ms / 1000 + 3_600;
    let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter = Arc::clone(&served);
    let upstream = stub::http(Router::new().fallback(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        async move {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            for window in ["5h", "7d", "7d_oi"] {
                let utilization = format!("anthropic-ratelimit-unified-{window}-utilization");
                let reset = format!("anthropic-ratelimit-unified-{window}-reset");
                headers.insert(
                    HeaderName::from_bytes(utilization.as_bytes()).unwrap(),
                    HeaderValue::from_str(&format!("{}", (10 + n) as f64 / 100.0)).unwrap(),
                );
                headers.insert(
                    HeaderName::from_bytes(reset.as_bytes()).unwrap(),
                    HeaderValue::from_str(&reset_secs.to_string()).unwrap(),
                );
            }
            let (input, output) = if n.is_multiple_of(2) {
                (100, 0)
            } else {
                (0, 25)
            };
            let body = serde_json::json!({"id":"msg_1","type":"message","role":"assistant",
                    "model":"claude-fable-5","content":[],"stop_reason":"end_turn",
                    "usage":{"input_tokens":input,"output_tokens":output}});
            (headers, body.to_string())
        }
    }));
    let manager = keyed(Config {
        upstream: upstream.url(),
        codex_upstream: upstream.url(),
        ..Config::fixture(vec![Account::fixture("alice@example.com")])
    });
    let ledger = fable_ledger(now_ms);
    manager.attach_usage_ledger(ledger.path().to_path_buf(), 29);
    // One family needs 20 rising ticks; 40 leave room for readings that share a
    // millisecond, which re-anchor rather than add a sample.
    for _ in 0..40 {
        post(
            Arc::clone(&manager),
            "/v1/messages",
            r#"{"model":"claude-fable-5","max_tokens":2,"messages":[],"stream":false}"#,
        )
        .await;
    }
    assert_eq!(
        served.load(Ordering::SeqCst),
        40,
        "every request reached the stub"
    );
    let value = document(Arc::clone(&manager)).await;
    assert_eq!(
        manager.shutdown_usage_ledger(Duration::from_secs(2)),
        crate::usage::LedgerShutdown::Flushed
    );
    assert_eq!(
        value["forecast"]["providers"][0]["calibrated"], true,
        "{}",
        value["forecast"]
    );
}

/// **The running proxy serves the routing document locally, as schema 1.**
///
/// The status is asserted before the body is decoded, so a proxy without the
/// route fails here on the catch-all's local 404 for anything under `/_tcr`,
/// not on a JSON error.
#[tokio::test]
async fn schema_one() {
    let (status, _, body) = get(manager(), "/_tcr/routing").await;
    assert_eq!(status, 200, "routing document must be served locally");
    let document: Value = serde_json::from_slice(&body).expect("the routing document is JSON");
    assert_eq!(document["schema"], 1, "the document names schema 1");
}

use crate::routing::ErrorKind;
use crate::test_support::stub_upstream as stub;
use time::format_description::well_known::Rfc3339;

/// A loopback peer, as the listener injects one.
fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 54_321))
}

/// A keyed manager over `config`, whose own key is replaced by [`KEY`].
fn keyed(config: Config) -> Arc<Manager> {
    Manager::with_live_refresher(
        Config {
            proxy: ProxyConfig {
                port: 0,
                api_key: Some(KEY.to_string()),
                request_key: None,
                extra: serde_json::Map::new(),
            },
            ..config
        },
        None,
    )
}

/// The routing document `manager` serves now, parsed.
async fn document(manager: Arc<Manager>) -> Value {
    let (status, _, body) = get(manager, ROUTING_PATH).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&body).expect("the routing document is JSON")
}

/// One inference request through the router from a loopback client, drained.
async fn post(manager: Arc<Manager>, path: &str, body: &'static str) {
    use tower::ServiceExt as _;
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(HOST, "127.0.0.1")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("build request");
    req.extensions_mut().insert(ClientAddr(loopback()));
    let response = app(manager).oneshot(req).await.expect("router response");
    to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
}

/// A request the proxy served for `model` on account `idx`.
fn served(manager: &Manager, idx: usize, model: &str, session: Option<u64>) {
    let provider = manager.snapshot(OffsetDateTime::now_utc()).accounts[idx].provider;
    manager.record_response_model(provider, Some(model));
    manager.record_usage(
        idx,
        crate::usage::UsageRecord::from_quota_input(
            crate::now_ms(),
            Some(model.to_string()),
            session,
            10,
            0,
            0,
            0,
            5,
        ),
    );
}

/// An object's keys, sorted.
fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .map_or_default(|object| object.keys().map(String::as_str).collect());
    keys.sort_unstable();
    keys
}

/// `value` is an RFC 3339 time in UTC, as the schema has every time.
fn assert_utc(value: &Value) -> OffsetDateTime {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("a time string: {value}"));
    assert!(text.ends_with('Z'), "UTC: {text}");
    let at = OffsetDateTime::parse(text, &Rfc3339).unwrap_or_else(|e| panic!("{text}: {e}"));
    assert_eq!(at.offset(), time::UtcOffset::UTC, "{text}");
    at
}

/// **Every field of schema 1, and its type**: the top level, the build, both providers in
/// order, a last error, a model, a null forecast; served as JSON and never stored.
#[tokio::test]
async fn every_field_and_its_type() {
    let manager = manager();
    served(&manager, 0, "claude-opus-5-5", None);
    manager.record_upstream_error(Provider::Anthropic, ErrorKind::Quota);
    let (status, headers, body) = get(manager, ROUTING_PATH).await;
    assert_eq!(status, StatusCode::OK);
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    assert_eq!(header("content-type"), Some("application/json"));
    assert_eq!(header("cache-control"), Some("no-store"));
    let document: Value = serde_json::from_slice(&body).expect("JSON");

    assert_eq!(
        keys(&document),
        ["build", "forecast", "generatedAt", "providers", "schema"]
    );
    assert_eq!(document["schema"], 1);
    assert_utc(&document["generatedAt"]);
    assert_eq!(keys(&document["build"]), ["builtAt", "dirty", "sha"]);
    assert!(document["build"]["sha"].is_string());
    assert!(document["build"]["builtAt"].is_string());
    assert!(document["build"]["dirty"].is_boolean() || document["build"]["dirty"].is_null());
    assert!(
        document["forecast"].is_null(),
        "no forecast before the forecast rounds"
    );

    let providers = document["providers"].as_array().expect("providers array");
    let names: Vec<_> = providers.iter().map(|p| p["provider"].as_str()).collect();
    assert_eq!(names, [Some("anthropic"), Some("codex")]);
    for provider in providers {
        if provider["provider"] == "codex" {
            assert_eq!(
                keys(provider),
                [
                    "lastError",
                    "models",
                    "provider",
                    "resetCreditsAvailable",
                    "state"
                ],
                "the `codex` provider must carry resetCreditsAvailable"
            );
            assert!(provider["resetCreditsAvailable"].as_u64().is_some());
            assert_eq!(provider["resetCreditsAvailable"], 0);
        } else {
            assert_eq!(keys(provider), ["lastError", "models", "provider", "state"]);
        }
        assert!(
            matches!(provider["state"].as_str(), Some("ok" | "degraded" | "out")),
            "{provider}"
        );
        assert!(provider["models"].is_array(), "{provider}");
    }

    let anthropic = &providers[0];
    let last_error = &anthropic["lastError"];
    assert_eq!(keys(last_error), ["at", "count10m", "kind"]);
    assert_eq!(last_error["kind"], "quota");
    assert_utc(&last_error["at"]);
    assert!(
        last_error["count10m"].as_u64().is_some_and(|n| n >= 1),
        "{last_error}"
    );
    assert_eq!(
        anthropic["state"], "degraded",
        "a pool that can serve, with an error in ten minutes"
    );
    let models = anthropic["models"].as_array().expect("models array");
    assert_eq!(models.len(), 1, "{models:?}");
    let opus = &models[0];
    assert_eq!(
        keys(opus),
        [
            "accountsRoutable",
            "freesAt",
            "ids",
            "model",
            "reason",
            "routable"
        ]
    );
    assert_eq!(opus["model"], "opus");
    assert_eq!(opus["ids"], serde_json::json!(["claude-opus-5-5"]));
    assert_eq!(opus["routable"], true);
    assert!(
        opus["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "{opus}"
    );
    assert!(
        opus["freesAt"].is_null(),
        "a routable model frees at no later time"
    );
    assert_eq!(opus["accountsRoutable"], 1);

    let codex = &providers[1];
    assert!(codex["lastError"].is_null());
    assert_eq!(codex["models"], serde_json::json!([]));
    assert_eq!(codex["state"], "out", "no `codex` account can serve");
}

/// **The document names no account, organization, session or credential**, even for a
/// fleet whose account carries every one of them and a served request with a session.
#[tokio::test]
async fn names_no_account_organization_session_or_credential() {
    const ACCOUNT_UUID: &str = "11111111-1111-4111-8111-111111111111";
    const ORG_UUID: &str = "22222222-2222-4222-8222-222222222222";
    const SESSION: u64 = 987_654_321_987;
    let alice = Account {
        account_uuid: Some(ACCOUNT_UUID.to_string()),
        org_uuid: Some(ORG_UUID.to_string()),
        org_name: Some("Example Org".to_string()),
        ..Account::fixture("alice@example.com")
    };
    let manager = keyed(Config {
        upstream: "http://127.0.0.1:1".to_string(),
        codex_upstream: "http://127.0.0.1:1".to_string(),
        ..Config::fixture(vec![alice])
    });
    served(&manager, 0, "claude-opus-5-5", Some(SESSION));
    manager.mark_refused(0);
    let (status, _, body) = get(manager, ROUTING_PATH).await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8_lossy(&body);
    for private in [
        "alice",
        "example.com",
        ACCOUNT_UUID,
        ORG_UUID,
        "Example Org",
        "at-alice",
        "rt-alice",
        KEY,
        &SESSION.to_string(),
    ] {
        assert!(!body.contains(private), "{private} in {body}");
    }
}

/// **An unroutable model says no available account can serve it now**, the meaning of
/// `capacity`, never measured overload, and names when the account holding it frees.
#[tokio::test]
async fn an_unroutable_model_says_no_available_account_can_serve_it_now() {
    let manager = manager();
    served(&manager, 0, "claude-opus-5-5", None);
    manager.mark_rate_limited(0, 600);
    let before = OffsetDateTime::now_utc();
    let document = document(manager).await;
    let anthropic = &document["providers"][0];
    assert_eq!(anthropic["state"], "out");
    let opus = &anthropic["models"][0];
    assert_eq!(opus["routable"], false, "{opus}");
    assert_eq!(opus["accountsRoutable"], 0, "{opus}");
    let reason = opus["reason"].as_str().expect("a reason");
    assert!(
        reason.starts_with("no available account can serve this model now"),
        "{reason}"
    );
    assert!(!reason.to_lowercase().contains("overload"), "{reason}");
    let frees = assert_utc(&opus["freesAt"]);
    assert!(
        frees > before && frees <= before + time::Duration::seconds(601),
        "the hold's end: {frees}"
    );
}

/// **The route reads, and only reads**: every other method is a local 405, never
/// forwarded.
#[tokio::test]
async fn only_get_is_served() {
    use tower::ServiceExt as _;
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let mut req = Request::builder()
            .method(method.clone())
            .uri(ROUTING_PATH)
            .header("x-api-key", KEY)
            .body(Body::empty())
            .expect("build request");
        req.extensions_mut().insert(ClientAddr(loopback()));
        let response = app(manager()).oneshot(req).await.expect("router response");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
    }
}

/// **The route passes the local gate**: a caller off this host is refused even with the
/// key, and a local caller without it is refused.
#[tokio::test]
async fn the_gate_refuses_a_remote_or_keyless_caller() {
    use tower::ServiceExt as _;
    let ask = |peer: SocketAddr, key: Option<&'static str>| async move {
        let mut builder = Request::builder().method(Method::GET).uri(ROUTING_PATH);
        if let Some(key) = key {
            builder = builder.header("x-api-key", key);
        }
        let mut req = builder.body(Body::empty()).expect("build request");
        req.extensions_mut().insert(ClientAddr(peer));
        app(manager())
            .oneshot(req)
            .await
            .expect("router response")
            .status()
    };
    let remote = SocketAddr::from(([203, 0, 113, 7], 44_444));
    assert_eq!(ask(remote, Some(KEY)).await, StatusCode::FORBIDDEN);
    assert_eq!(ask(loopback(), None).await, StatusCode::UNAUTHORIZED);
}

/// The last error and its count, as the document reports `provider`'s.
async fn last_error(manager: Arc<Manager>, provider: usize) -> (Value, u64) {
    let document = document(manager).await;
    let error = &document["providers"][provider]["lastError"];
    (
        error["kind"].clone(),
        error["count10m"].as_u64().unwrap_or(0),
    )
}

/// **A send that reaches no upstream is a transport error.**
#[tokio::test]
async fn a_send_that_reaches_no_upstream_is_a_transport_error() {
    let manager = manager();
    post(
        Arc::clone(&manager),
        "/v1/messages",
        r#"{"model":"claude-opus-5-5","max_tokens":1,"messages":[]}"#,
    )
    .await;
    let (kind, count) = last_error(manager, 0).await;
    assert_eq!(kind, "transport");
    assert!(count >= 1);
}

/// **An upstream 503 is overloaded**, read at the status every attempt passes.
#[tokio::test]
async fn an_upstream_503_is_overloaded() {
    let upstream = stub::canned(stub::reply(
        "503 Service Unavailable",
        "application/json",
        "{}",
    ));
    let manager = keyed(Config {
        upstream: upstream.url(),
        codex_upstream: "http://127.0.0.1:1".to_string(),
        ..Config::fixture(vec![Account::fixture("alice@example.com")])
    });
    post(
        Arc::clone(&manager),
        "/v1/messages",
        r#"{"model":"claude-opus-5-5","max_tokens":1,"messages":[]}"#,
    )
    .await;
    let (kind, count) = last_error(manager, 0).await;
    assert_eq!(kind, "overloaded");
    assert!(count >= 1);
}

/// **A `codex` account refusing the model for its plan is capacity**, on the `codex`
/// provider only.
#[tokio::test]
async fn a_codex_model_refusal_is_capacity() {
    let origin = Router::new().fallback(|| async {
            (
                StatusCode::BAD_REQUEST,
                [(CONTENT_TYPE, "application/json")],
                r#"{"detail":"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."}"#,
            )
        });
    let upstream = stub::http(origin);
    let bob = Account {
        provider: Provider::Codex,
        account_uuid: Some("33333333-3333-4333-8333-333333333333".to_string()),
        ..Account::fixture("bob@example.com/codex")
    };
    let manager = keyed(Config {
        upstream: "http://127.0.0.1:1".to_string(),
        codex_upstream: upstream.url(),
        ..Config::fixture(vec![bob])
    });
    post(
        Arc::clone(&manager),
        crate::provider::CODEX_RESPONSES_PATH,
        r#"{"model":"gpt-6.1-sol","input":[]}"#,
    )
    .await;
    let document = document(manager).await;
    assert_eq!(document["providers"][1]["lastError"]["kind"], "capacity");
    assert!(document["providers"][0]["lastError"].is_null());
}
#[tokio::test]
async fn reviewer_head_must_be_405() {
    use tower::ServiceExt as _;
    let mut req = Request::builder()
        .method(Method::HEAD)
        .uri(ROUTING_PATH)
        .header("x-api-key", KEY)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ClientAddr(loopback()));
    let response = app(manager()).oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "every method other than GET must be 405"
    );
}

#[tokio::test]
async fn reviewer_all_boot_model_ids_are_reported() {
    let manager = manager();
    for n in 0..65 {
        served(&manager, 0, &format!("claude-opus-{n}"), None);
    }
    served(&manager, 0, "claude-sonnet-5-5", None);
    let document = document(manager).await;
    let ids = document["providers"][0]["models"][0]["ids"]
        .as_array()
        .unwrap();
    assert_eq!(
        ids.len(),
        65,
        "schema ids include every full id seen this boot"
    );
    assert!(ids.iter().any(|id| id == "claude-opus-64"));
    assert_eq!(document["providers"][0]["models"][1]["model"], "sonnet");
    assert_eq!(
        document["providers"][0]["models"][1]["ids"][0],
        "claude-sonnet-5-5"
    );
}

#[tokio::test]
async fn reviewer_spent_revalidation_window_is_not_routable_now() {
    let manager = manager();
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        HeaderValue::from_static("0.95"),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        HeaderValue::from_str(&(OffsetDateTime::now_utc().unix_timestamp() + 3600).to_string())
            .unwrap(),
    );
    assert!(manager.update_quota(0, &headers, 200));
    served(&manager, 0, "claude-opus-5-5", None);
    let now = OffsetDateTime::now_utc();
    let tried = HashSet::new();
    assert_eq!(
        manager.select(
            &tried,
            now,
            Some("claude-opus-5-5"),
            None,
            "/v1/messages",
            None
        ),
        None
    );
    assert_eq!(
        manager.select_revalidation(&tried, now, Some("claude-opus-5-5"), None, "/v1/messages"),
        Some(0)
    );
    assert_eq!(
        manager.select_revalidation(&tried, now, Some("claude-opus-5-5"), None, "/v1/messages"),
        None,
        "positive control: the spent window actually refuses this request"
    );
    let document = crate::routing::document(&manager, now).unwrap();
    let document = serde_json::to_value(document).unwrap();
    assert_eq!(
        document["providers"][0]["models"][0]["routable"], false,
        "routing must agree with the request the selector can serve now"
    );
    let frees = assert_utc(&document["providers"][0]["models"][0]["freesAt"]);
    assert!(
        frees > now && frees <= now + time::Duration::seconds(2),
        "the spent valve, rather than the soft quota reset, sets freesAt"
    );
    assert!(
        manager
            .routing_candidates(
                frees - time::Duration::milliseconds(1),
                Some("claude-opus-5-5"),
                "/v1/messages"
            )
            .accounts
            .is_empty()
    );
    let ready = manager.routing_candidates(frees, Some("claude-opus-5-5"), "/v1/messages");
    assert_eq!(ready.accounts, vec![0], "the valve opens at its expiry");
    assert_eq!(
        manager.select_revalidation(&tried, frees, Some("claude-opus-5-5"), None, "/v1/messages"),
        Some(0),
        "a routing read must not spend the opened valve"
    );
}

#[tokio::test]
async fn reviewer_frees_at_excludes_nonpooled_control_account() {
    let mut config = Config::fixture(vec![
        Account::fixture("alice@example.com"),
        Account::fixture("bob@example.com"),
    ]);
    config.control_account = Some(config::ControlAccountRef::Name(
        "alice@example.com".to_string(),
    ));
    config
        .extra
        .insert("revalidationServe".to_string(), serde_json::json!(false));
    let manager = keyed(config);
    served(&manager, 1, "claude-opus-5-5", None);
    manager.mark_rate_limited(0, 60);
    manager.mark_rate_limited(1, 600);
    let now = OffsetDateTime::now_utc();
    let future = now + time::Duration::seconds(61);
    assert!(
        manager
            .routing_candidates(future, Some("claude-opus-5-5"), "/v1/messages")
            .accounts
            .is_empty(),
        "positive control: the freed control account cannot serve new pool work"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            future,
            Some("claude-opus-5-5"),
            None,
            "/v1/messages",
            None
        ),
        None,
        "independent public selection still cannot serve after the control account clears"
    );
    let document = document(manager).await;
    let opus = &document["providers"][0]["models"][0];
    assert_eq!(opus["routable"], false);
    let frees = assert_utc(&opus["freesAt"]);
    assert!(
        frees > now + time::Duration::seconds(599),
        "freesAt must describe the pool account's 600 second hold, got {frees}, now {now}"
    );
}
#[tokio::test]
async fn independent_response_named_model_catalogue() {
    use tower::ServiceExt as _;
    let upstream = stub::canned(stub::reply(
        "200 OK",
        "application/json",
        r#"{"id":"fake-message","type":"message","role":"assistant","model":"claude-sonnet-5-5","content":[],"usage":{"input_tokens":10,"output_tokens":2}}"#,
    ));
    let manager = keyed(Config {
        upstream: upstream.url(),
        codex_upstream: "http://127.0.0.1:1".to_string(),
        ..Config::fixture(vec![Account::fixture("alice@example.com")])
    });
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header(HOST, "127.0.0.1")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"model":"claude-opus-request-only","max_tokens":2,"messages":[]}"#,
        ))
        .unwrap();
    req.extensions_mut().insert(ClientAddr(loopback()));
    let response = app(Arc::clone(&manager)).oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "fixture served successfully"
    );
    let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap();
    let served: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        served["model"], "claude-sonnet-5-5",
        "positive control: response names the served model"
    );
    let observed = document(manager).await;
    let models = observed["providers"][0]["models"].as_array().unwrap();
    assert!(
        models.iter().all(|model| model["ids"]
            .as_array()
            .unwrap()
            .iter()
            .all(|id| id != "claude-opus-request-only")),
        "routing must not catalogue a request-only id absent from the served response: {models:?}"
    );
    assert!(
        models.iter().any(|model| model["ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "claude-sonnet-5-5")),
        "routing must catalogue the id the served response actually named: {models:?}"
    );
}
/// Response identity is independent of request-preferred usage, including a turn
/// with no usage at all. Each case drives inference and the public routing read.
async fn response_catalogue_case(provider: Provider, streaming: bool, named: bool, usage: bool) {
    use tower::ServiceExt as _;
    let request_model = match provider {
        Provider::Anthropic => "claude-opus-request-only",
        Provider::Codex => "gpt-request-only",
    };
    let response_model = match provider {
        Provider::Anthropic => "claude-sonnet-5-5",
        Provider::Codex => "gpt-response-only",
    };
    let mut value = serde_json::json!({"id":"fake-response", "type":"message", "role":"assistant", "content":[]});
    if named {
        value["model"] = response_model.into();
    }
    if usage {
        value["usage"] = serde_json::json!({"input_tokens":10,"output_tokens":2});
    }
    let body = if streaming {
        match provider {
            Provider::Anthropic => format!(
                "event: message_start\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
                serde_json::json!({"type":"message_start", "message":value})
            ),
            Provider::Codex => format!(
                "event: response.completed\ndata: {}\n\n",
                serde_json::json!({"type":"response.completed", "response":value})
            ),
        }
    } else {
        value.to_string()
    };
    let upstream = stub::http(Router::new().fallback(move || {
        let body = body.clone();
        async move {
            (
                [(
                    CONTENT_TYPE,
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )],
                body,
            )
        }
    }));
    let account = Account {
        provider,
        ..Account::fixture("alice@example.com")
    };
    let manager = keyed(Config {
        upstream: upstream.url(),
        codex_upstream: upstream.url(),
        ..Config::fixture(vec![account])
    });
    let path = match provider {
        Provider::Anthropic => "/v1/messages",
        Provider::Codex => crate::provider::CODEX_RESPONSES_PATH,
    };
    let mut req = Request::builder().method(Method::POST).uri(path)
            .header(HOST,"127.0.0.1").header(CONTENT_TYPE,"application/json")
            .body(Body::from(serde_json::json!({"model":request_model,"max_tokens":2,"messages":[],"input":[],"stream":streaming}).to_string())).unwrap();
    req.extensions_mut().insert(ClientAddr(loopback()));
    let response = app(Arc::clone(&manager)).oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "fixture served successfully"
    );
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap();
    let served: Value = if streaming {
        let text = std::str::from_utf8(&bytes).unwrap();
        let event: Value =
            serde_json::from_str(text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap())
                .unwrap();
        match provider {
            Provider::Anthropic => event["message"].clone(),
            Provider::Codex => event["response"].clone(),
        }
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    assert_eq!(
        served.get("model").and_then(Value::as_str),
        named.then_some(response_model),
        "positive control: served response identity"
    );
    // The streamed parser is a detached tee. For usage cases, its ledger row is
    // the completion barrier; no-usage cases wait for the expected observation.
    let ledger_models = || {
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
            .usage
            .as_ref()
            .map_or_default(|row| row.today_by_model.keys().cloned().collect::<Vec<_>>())
    };
    for _ in 0..200 {
        let ready = if usage {
            !ledger_models().is_empty()
        } else {
            manager
                .routing_observed(provider, crate::now_ms())
                .1
                .contains(response_model)
        };
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if usage {
        assert_eq!(
            ledger_models(),
            [request_model],
            "usage keeps the requested model"
        );
    } else {
        assert!(ledger_models().is_empty(), "no usage creates no ledger row");
    }
    let doc = document(manager).await;
    let slot = match provider {
        Provider::Anthropic => 0,
        Provider::Codex => 1,
    };
    let ids: Vec<_> = doc["providers"][slot]["models"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["ids"].as_array().unwrap())
        .map(|id| id.as_str().unwrap())
        .collect();
    let expected: Vec<_> = named.then_some(response_model).into_iter().collect();
    assert_eq!(
        ids, expected,
        "catalogue contains only response-named ids, independently of usage"
    );
}

#[tokio::test]
async fn response_catalogue_anthropic_buffered_mismatch() {
    response_catalogue_case(Provider::Anthropic, false, true, true).await;
}

#[tokio::test]
async fn response_catalogue_anthropic_buffered_missing_model() {
    response_catalogue_case(Provider::Anthropic, false, false, true).await;
}

#[tokio::test]
async fn response_catalogue_anthropic_buffered_without_usage() {
    response_catalogue_case(Provider::Anthropic, false, true, false).await;
}

#[tokio::test]
async fn response_catalogue_anthropic_streamed_mismatch() {
    response_catalogue_case(Provider::Anthropic, true, true, true).await;
}

#[tokio::test]
async fn response_catalogue_anthropic_streamed_missing_model() {
    response_catalogue_case(Provider::Anthropic, true, false, true).await;
}

#[tokio::test]
async fn response_catalogue_anthropic_streamed_without_usage() {
    response_catalogue_case(Provider::Anthropic, true, true, false).await;
}

#[tokio::test]
async fn response_catalogue_codex_buffered_mismatch() {
    response_catalogue_case(Provider::Codex, false, true, true).await;
}

#[tokio::test]
async fn response_catalogue_codex_buffered_missing_model() {
    response_catalogue_case(Provider::Codex, false, false, true).await;
}

#[tokio::test]
async fn response_catalogue_codex_buffered_without_usage() {
    response_catalogue_case(Provider::Codex, false, true, false).await;
}

#[tokio::test]
async fn response_catalogue_codex_streamed_mismatch() {
    response_catalogue_case(Provider::Codex, true, true, true).await;
}

#[tokio::test]
async fn response_catalogue_codex_streamed_missing_model() {
    response_catalogue_case(Provider::Codex, true, false, true).await;
}

#[tokio::test]
async fn response_catalogue_codex_streamed_without_usage() {
    response_catalogue_case(Provider::Codex, true, true, false).await;
}

#[tokio::test]
async fn reviewer_unmetered_terminal_keeps_metered_ledger_model() {
    use tower::ServiceExt as _;
    let body = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fake-response\",\"model\":\"gpt-metered\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fake-response\",\"usage\":null}}\n\n",
    );
    let upstream = stub::http(
        Router::new()
            .fallback(move || async move { ([(CONTENT_TYPE, "text/event-stream")], body) }),
    );
    let account = Account {
        provider: Provider::Codex,
        ..Account::fixture("alice@example.com")
    };
    let manager = keyed(Config {
        upstream: "http://127.0.0.1:1".into(),
        codex_upstream: upstream.url(),
        ..Config::fixture(vec![account])
    });
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(crate::provider::CODEX_RESPONSES_PATH)
        .header(HOST, "127.0.0.1")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"input":[]}"#))
        .unwrap();
    req.extensions_mut().insert(ClientAddr(loopback()));
    let response = app(Arc::clone(&manager)).oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "positive control: served response"
    );
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap();
    assert_eq!(
        bytes.as_ref(),
        body.as_bytes(),
        "positive control: complete replayed stream forwarded"
    );
    for _ in 0..200 {
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        if snapshot.accounts[0]
            .usage
            .as_ref()
            .is_some_and(|u| u.today.requests > 0)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let usage = snapshot.accounts[0]
        .usage
        .as_ref()
        .expect("metered turn has usage");
    assert_eq!(
        (
            usage.today.requests,
            usage.today.input_tokens,
            usage.today.output_tokens
        ),
        (1, 12, 3),
        "positive control: prior terminal's metered totals survive"
    );
    let models: Vec<_> = usage.today_by_model.keys().map(String::as_str).collect();
    assert_eq!(
        models,
        ["gpt-metered"],
        "an unmetered terminal must not erase the model attached to retained usage"
    );
}

#[tokio::test]
async fn reviewer_unmetered_terminal_keeps_priced_usage() {
    use tower::ServiceExt as _;
    let body = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fake-response\",\"model\":\"gpt-metered\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fake-response\",\"usage\":null}}\n\n",
    );
    let upstream = stub::http(
        Router::new()
            .fallback(move || async move { ([(CONTENT_TYPE, "text/event-stream")], body) }),
    );
    let account = Account {
        provider: Provider::Codex,
        ..Account::fixture("alice@example.com")
    };
    let manager = keyed(Config {
        pricing: std::collections::HashMap::from([(
            "gpt-metered".into(),
            crate::pricing::PricingOverride {
                input: 1.0,
                output: 1.0,
                cache_write_5m: None,
                cache_write_1h: None,
                cache_read: None,
            },
        )]),
        upstream: "http://127.0.0.1:1".into(),
        codex_upstream: upstream.url(),
        ..Config::fixture(vec![account])
    });
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(crate::provider::CODEX_RESPONSES_PATH)
        .header(HOST, "127.0.0.1")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"input":[]}"#))
        .unwrap();
    req.extensions_mut().insert(ClientAddr(loopback()));
    let response = app(Arc::clone(&manager)).oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "positive control: served response"
    );
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap();
    assert_eq!(
        bytes.as_ref(),
        body.as_bytes(),
        "positive control: complete replayed stream forwarded"
    );
    for _ in 0..200 {
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        if snapshot.accounts[0]
            .usage
            .as_ref()
            .is_some_and(|u| u.today.requests > 0)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let usage = snapshot.accounts[0]
        .usage
        .as_ref()
        .expect("metered turn has usage");
    assert_eq!(
        (
            usage.today.requests,
            usage.today.input_tokens,
            usage.today.output_tokens
        ),
        (1, 12, 3),
        "positive control: prior terminal's metered totals survive"
    );
    assert_eq!(
        usage.today.cost_usd,
        Some(0.000_015),
        "an unmetered terminal must not turn retained priced usage into an unpriced total"
    );
    let models: Vec<_> = usage.today_by_model.keys().map(String::as_str).collect();
    assert_eq!(
        models,
        ["gpt-metered"],
        "an unmetered terminal must not erase the model attached to retained usage"
    );
}
