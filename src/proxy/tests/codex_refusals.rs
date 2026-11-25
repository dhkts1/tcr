//! The refusals tcr sends a Codex client, and the holds and rotation behind them.

use super::*;

/// The 426 over a REAL socket, request written byte for byte: hyper parses the
/// upgrade request itself (an `Upgrade` + `Connection: Upgrade` pair is the one
/// request shape hyper treats specially), and `Host: chatgpt.com` takes the host
/// guard's allow-listed branch rather than the guard-skipped one `oneshot` gets.
/// The fleet has no Codex account at all, so a request that slipped past step 1c'
/// would be answered 400 by the empty pool, never forwarded anywhere.
#[tokio::test]
async fn codex_upgrade_over_a_real_socket_is_answered_426() {
    let addr = spawn_dead_upstream_proxy().await;
    let status = raw_request_status(
        addr,
        "GET /backend-api/codex/responses HTTP/1.1\r\n\
             Host: chatgpt.com\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n\r\n",
    )
    .await;
    assert_eq!(status, 426, "the upgrade is answered here, over the wire");
}

/// One Codex inference request on the client's own bearer, through the router, as codex
/// sends it.
async fn send_codex_turn(manager: &Arc<Manager>) -> Response {
    drive_full(
        Arc::clone(manager),
        Method::POST,
        crate::base::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await
}

/// The refused count and the latest refusal's type, as `tcr status` will read them.
fn refusals(manager: &Manager) -> (u64, Option<CodexRefusalKind>) {
    let traffic = manager.codex_traffic(crate::now_ms());
    (traffic.refused_10m, traffic.last_refusal)
}

#[tokio::test]
async fn codex_error_responses_keep_status_content_type_and_retry_after() {
    use crate::base::provider::Provider;

    let now = OffsetDateTime::now_utc();
    let mut config = codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9");
    config.accounts[1].disabled = Some(true);
    let manager = Manager::with_live_refresher(config, None);
    let responses = [
        (
            exhausted_response(&manager, now, 0, false, Provider::Codex, None, None),
            StatusCode::BAD_REQUEST,
            "no_codex_account",
            "no_codex_account",
            None,
        ),
        (
            codex_error_response(
                StatusCode::TOO_MANY_REQUESTS,
                codex_pool_unavailable_body(60, true),
                Some(60),
            ),
            StatusCode::TOO_MANY_REQUESTS,
            "pool_unavailable",
            "pool_unavailable",
            Some("60"),
        ),
        (
            codex_credential_refused_response(&[(0, Some("alice@example.com/codex".into()))]),
            StatusCode::FORBIDDEN,
            "proxy_error",
            "credential_refused",
            None,
        ),
    ];
    for (response, status, error_type, code, retry_after) in responses {
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .map(|value| value.to_str().unwrap()),
            retry_after,
        );
        let body = body_json(response).await;
        assert_eq!(body["error"]["type"], error_type);
        assert_eq!(body["error"]["code"], code);
        assert!(body["error"]["message"].as_str().is_some());
    }
}

/// **Every refusal tcr sends a Codex request itself is counted, under its body's type**, so
/// `tcr status` can say codex was refused while the panel looks healthy: no Codex account
/// at all, an unusable pool (disabled), a pool held temporarily by an upstream
/// rate limit, and a pool out on a quota. A Claude client's 429 is not codex's and is not counted.
///
/// Watched red: without the bump in the zero-account branch, the first case reads 0; with a
/// transient hold counted as a quota, the last case reads `usage_limit_reached`.
#[tokio::test]
async fn every_codex_refusal_tcr_sends_bumps_the_refused_ring() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let fleet = || codex_fleet(&anthropic_echo.url, &codex_echo.url);

    let mut config = fleet();
    config.accounts.remove(1);
    let manager = Manager::with_live_refresher(config, None);
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"]["type"],
        "no_codex_account"
    );
    assert_eq!(
        refusals(&manager),
        (1, Some(CodexRefusalKind::NoCodexAccount))
    );

    let mut config = fleet();
    config.accounts[1].disabled = Some(true);
    let manager = Manager::with_live_refresher(config, None);
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"]["type"],
        "no_codex_account"
    );
    assert_eq!(
        refusals(&manager),
        (1, Some(CodexRefusalKind::NoCodexAccount))
    );

    // A revalidation serve would try the full account once more; off, so the 429 is the
    // only answer.
    let mut config = fleet();
    config
        .extra
        .insert("revalidationServe".to_string(), serde_json::json!(false));
    let manager = Manager::with_live_refresher(config, None);
    let now = OffsetDateTime::now_utc();
    manager.edit_quota_for_test(1, |quota| {
        quota.seven_day = Some(crate::records::quota::QuotaWindow {
            utilization: 1.0,
            reset: Some(now + time::Duration::seconds(300)),
        });
    });
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body_json(response).await["error"]["type"],
        "usage_limit_reached"
    );
    assert_eq!(
        refusals(&manager),
        (1, Some(CodexRefusalKind::UsageLimitReached))
    );

    // A transient rate limit keeps its retryable pool-unavailable response.
    let manager = Manager::with_live_refresher(fleet(), None);
    manager.mark_rate_limited(1, 120);
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response.headers().contains_key("retry-after"),
        "a hold ends, so codex is told when to retry"
    );
    assert_eq!(
        body_json(response).await["error"]["type"],
        "pool_unavailable"
    );
    assert_eq!(
        refusals(&manager),
        (1, Some(CodexRefusalKind::PoolUnavailable))
    );
    assert_eq!(
        codex_echo.request_count(),
        0,
        "every 429 above is tcr's own"
    );

    // A Claude client's exhausted 429 is not a Codex refusal.
    let up_addr = spawn_scripted_upstream(vec![Some(raw_200())]).await;
    let manager = fleet_configured(up_addr, &["a", "b", "c"], |config| {
        config.accounts[2].provider = Provider::Codex;
        config
            .extra
            .insert("revalidationServe".to_string(), serde_json::json!(false));
    });
    manager.mark_rate_limited(0, 600);
    manager.mark_rate_limited(1, 600);
    let (status, _, body) = post_one_full(Arc::clone(&manager)).await;
    assert_eq!(status, 429, "{body}");
    assert!(body.contains("All 2 accounts exhausted"), "{body}");
    assert_eq!(refusals(&manager), (0, None));
}

/// **A Codex request relayed on its own credential because it named a routing of its own is
/// counted as unpooled**, and nothing else is: a plain inference request is pooled, and a
/// bookkeeping call is relayed for its path whatever routing it names.
///
/// Watched red: without the bump, the routed request reads 0.
#[tokio::test]
async fn a_routed_codex_relay_bumps_the_unpooled_ring() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let unpooled = |manager: &Manager| manager.codex_traffic(crate::now_ms()).unpooled_10m;

    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let (status, _) = drive(
        Arc::clone(&manager),
        Method::POST,
        crate::base::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("x-openai-account-routing-override", "us"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        codex_echo.last_request().header("authorization"),
        Some("Bearer client-own"),
        "relayed on the client's own credential"
    );
    assert_eq!(unpooled(&manager), 1);

    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(unpooled(&manager), 0, "a pooled request is not unpooled");

    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let (status, _) = drive(
        Arc::clone(&manager),
        Method::GET,
        crate::base::provider::CODEX_USAGE_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("x-openai-account-routing-override", "us"),
        ],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        unpooled(&manager),
        0,
        "a bookkeeping call is relayed for its path, routing or not"
    );
}

/// **The status document carries the Codex counts, and names a Codex upstream that is not
/// the default**, which is what `tcr status` reads them from. The default upstream is not
/// named: the key is absent.
#[tokio::test]
async fn the_status_document_carries_codex_traffic_and_a_non_default_upstream() {
    let manager =
        Manager::with_live_refresher(codex_fleet("http://127.0.0.1:9", "http://codex.test"), None);
    manager.note_codex_refusal(CodexRefusalKind::PoolUnavailable, crate::now_ms());
    let (status, _, body) = status_request(manager, Method::GET, Some(loopback_peer()), None).await;
    assert_eq!(status, StatusCode::OK);
    let payload: crate::status::StatusPayload =
        serde_json::from_slice(&body).expect("a status payload");
    let traffic = payload.codex_traffic.expect("the counts are always sent");
    assert_eq!(traffic.refused_10m, 1);
    assert_eq!(
        traffic.last_refusal,
        Some(CodexRefusalKind::PoolUnavailable)
    );
    assert_eq!(payload.codex_upstream.as_deref(), Some("http://codex.test"));

    let manager = Manager::with_live_refresher(
        codex_fleet("http://127.0.0.1:9", crate::base::provider::CODEX_UPSTREAM),
        None,
    );
    let (status, _, body) = status_request(manager, Method::GET, Some(loopback_peer()), None).await;
    assert_eq!(status, StatusCode::OK);
    let raw: Value = serde_json::from_slice(&body).expect("JSON");
    assert!(raw.get("codex_upstream").is_none(), "{raw}");
    assert_eq!(
        raw["codex_traffic"],
        serde_json::json!({"refused10m": 0, "unpooled10m": 0})
    );
}

/// **A server holding a request key says so, and counts the plain request that lacked
/// it**, serving it anyway (log mode). Without a key the status carries none of the three
/// fields, which is how a launcher tells an older server from this one.
#[tokio::test]
async fn the_status_document_reports_log_mode_and_counts_an_unkeyed_request() {
    use tower::ServiceExt as _;
    let plain = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);
    let (_, _, body) = status_request(plain, Method::GET, Some(loopback_peer()), None).await;
    let raw: Value = serde_json::from_slice(&body).expect("JSON");
    for key in ["requestKeyMode", "unkeyedConnects", "unkeyedRequests"] {
        assert!(raw.get(key).is_none(), "{key} without a key: {raw}");
    }

    let mut config = dummy_config(None, "http://127.0.0.1:1");
    config.proxy.request_key = Some(REQUEST_KEY.to_string());
    let manager = Manager::with_live_refresher(config, None);
    // Refused locally by the path-shape guard, which sits after the count, so nothing
    // goes upstream; the keyed one is the control.
    for keyed in [false, true] {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri("/v1/../models")
            .header("user-agent", "curl/8.7.1");
        if keyed {
            builder = builder.header(crate::request_key::HEADER, REQUEST_KEY);
        }
        let mut req = builder.body(Body::empty()).expect("build request");
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        let response = app(Arc::clone(&manager))
            .oneshot(req)
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let (_, _, body) = status_request(manager, Method::GET, Some(loopback_peer()), None).await;
    let raw: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(raw["requestKeyMode"], "log", "{raw}");
    assert_eq!(raw["unkeyedConnects"], serde_json::json!([]), "{raw}");
    assert_eq!(
        raw["unkeyedRequests"],
        serde_json::json!([{"class": "v1-other", "userAgent": "curl", "count": 1}]),
        "{raw}"
    );
    assert!(
        !String::from_utf8_lossy(&body).contains(REQUEST_KEY),
        "the status never carries the key itself"
    );
}

/// **A Codex account deleted while the proxy runs stops counting on every Codex path at
/// once.** The status document no longer lists it, so `tcr status` words the route as a
/// pool with no Codex account; the proxy's own Codex count agrees, so its listener stops
/// intercepting chatgpt.com and a Codex request is told no Codex account is configured. The
/// deleted slot no longer counts for either provider.
///
/// Watched red: with deleted slots counted for Codex, the proxy's Codex count reads 1.
#[tokio::test]
async fn a_live_delete_of_the_last_codex_account_reads_as_no_codex_account() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    let config = codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9");
    let write = |accounts: &[Account]| {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({ "accounts": accounts }))
                .expect("serialize"),
        )
        .expect("write the config");
    };
    write(&config.accounts);
    let alice = config.accounts[0].clone();
    let manager = Manager::with_live_refresher(config, Some(path.clone()));
    manager.record_mitm_status(crate::status::MitmStatus {
        intercepted_hosts: vec![
            "api.anthropic.com".to_string(),
            "platform.claude.com".to_string(),
            "chatgpt.com".to_string(),
        ],
        leaf: crate::status::MitmStatus::LEAF_MINTED.to_string(),
    });
    let _ = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(manager.provider_account_count(Provider::Codex), 1);

    // bob is deleted from the file; the next snapshot reloads it and marks him removed.
    write(std::slice::from_ref(&alice));
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open the config")
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
        .expect("move the config's mtime");
    let (status, _, body) = status_request(
        Arc::clone(&manager),
        Method::GET,
        Some(loopback_peer()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(manager.provider_account_count(Provider::Codex), 0);
    assert_eq!(
        manager.provider_account_count(Provider::Anthropic),
        1,
        "the Anthropic count is the one it always was"
    );
    let payload: crate::status::StatusPayload =
        serde_json::from_slice(&body).expect("a status payload");
    let codex_rows = payload
        .accounts
        .iter()
        .filter(|row| row.provider == Provider::Codex)
        .count();
    assert_eq!(codex_rows, 0, "the document no longer lists bob");
    let route = crate::verbs::codex_route::codex_route(
        Ok(&payload),
        3999,
        std::path::Path::new("/tmp/tcr-run-test/tcr-ca.pem"),
        true,
        codex_rows,
    );
    assert_eq!(
        route.code.as_deref(),
        Some(crate::verbs::codex_route::NO_CODEX_ACCOUNT)
    );

    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"]["type"],
        "no_codex_account"
    );
}

#[tokio::test]
async fn deleting_the_last_claude_account_returns_the_no_account_message() {
    let dir = tempfile::tempdir().expect("temporary config");
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    let config = codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9");
    let write = |accounts: &[Account]| {
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"accounts": accounts})).unwrap(),
        )
        .unwrap();
    };
    write(&config.accounts);
    let codex = config.accounts[1].clone();
    let manager = Manager::with_live_refresher(config, Some(path.clone()));
    let _ = manager.snapshot(OffsetDateTime::now_utc());
    write(&[codex]);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    let response = drive_full(
        Arc::clone(&manager),
        Method::POST,
        "/v1/messages",
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"model":"claude-x","messages":[]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = body_json(response).await;
    assert_eq!(
        body["error"]["message"],
        "No Claude account is configured in tcr; run `tcr login` to add one."
    );
    assert_eq!(manager.provider_account_count(Provider::Anthropic), 0);
    assert_eq!(manager.provider_account_count(Provider::Codex), 1);
}

/// **A pool that holds Codex accounts and no Claude account tells a Claude client so**,
/// and how to add one, where it said "All 0 accounts exhausted". Its status, type and
/// headers are the zero-account 429's as they were; a fleet with no Codex account, or with
/// a Claude account, keeps the count.
///
/// Watched red: without the zero-Claude-account arm, the message reads "All 0 accounts
/// exhausted".
#[tokio::test]
async fn the_zero_account_429_says_no_claude_account_when_only_codex_is_pooled() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let mut config = codex_fleet(&anthropic_echo.url, &codex_echo.url);
    config.accounts.remove(0);
    let manager = Manager::with_live_refresher(config, None);
    let response = drive_full(
        manager,
        Method::POST,
        "/v1/messages",
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"model":"claude-x","messages":[]}"#,
    )
    .await;
    let header = |response: &Response, name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let (retry_after, content_type, claim) = (
        header(&response, "retry-after"),
        header(&response, "content-type"),
        header(&response, "anthropic-ratelimit-unified-status"),
    );
    let body = body_json(response).await;
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "rate_limit_error", "{body}");
    assert_eq!(
        body["error"]["message"],
        "No Claude account is configured in tcr; run `tcr login` to add one."
    );
    assert_eq!(anthropic_echo.request_count(), 0);
    assert_eq!(codex_echo.request_count(), 0);

    // The controls: the same call on a fleet with no account at all, and on one that holds
    // a Claude account, keep their counts and their headers.
    let now = OffsetDateTime::now_utc();
    let mut empty = codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9");
    empty.accounts.clear();
    let empty = Manager::with_live_refresher(empty, None);
    let control = exhausted_response(&empty, now, 0, false, Provider::Anthropic, None, None);
    assert_eq!(control.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&control, "retry-after"), retry_after);
    assert_eq!(header(&control, "content-type"), content_type);
    assert_eq!(
        header(&control, "anthropic-ratelimit-unified-status"),
        claim
    );
    let message = body_json(control).await["error"]["message"]
        .as_str()
        .expect("a message")
        .to_string();
    assert!(message.starts_with("All 0 accounts exhausted"), "{message}");
    let mixed = Manager::with_live_refresher(
        codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9"),
        None,
    );
    let message = body_json(exhausted_response(
        &mixed,
        now,
        1,
        false,
        Provider::Anthropic,
        None,
        None,
    ))
    .await["error"]["message"]
        .as_str()
        .expect("a message")
        .to_string();
    assert!(message.starts_with("All 1 accounts exhausted"), "{message}");
}

/// **The no-Codex-account 429 names the command that adds one**: a login of tcr's own.
#[test]
fn the_no_codex_account_429_names_tcr_login_codex() {
    let body: Value = serde_json::from_str(&codex_no_account_body()).expect("a JSON body");
    assert_eq!(
        body["error"]["message"],
        "No Codex account is configured in tcr; run `tcr login --codex` to add one."
    );
}

/// **The exhausted-pool 429 says that no pooled account can take the request now**, and
/// when the first one frees, or that none reports when. It claims no account is "at its
/// limit": one held by an upstream 403 can be out beside one out on a window.
#[test]
fn codex_exhausted_body_says_no_pooled_account_can_take_the_request() {
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("a valid instant");
    let message = |measured: bool| {
        let body: Value =
            serde_json::from_str(&codex_exhausted_body(now, 300, measured)).expect("JSON");
        body["error"]["message"]
            .as_str()
            .expect("a message")
            .to_string()
    };
    assert_eq!(
        message(true),
        "No pooled Codex account can take this request now; the first one frees in 300 \
             seconds."
    );
    assert_eq!(
        message(false),
        "No pooled Codex account can take this request now, and none reports when it frees; \
             retry in 300 seconds."
    );
}

async fn assert_codex_block_is_forwarded(
    content_type: &'static str,
    challenge: bool,
    body: &'static str,
) {
    let origin = Router::new().fallback(move || async move {
        let mut response = Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(CONTENT_TYPE, content_type)
            .header("cf-ray", "test-ray");
        if challenge {
            response = response.header("cf-mitigated", "challenge");
        }
        response.body(Body::from(body)).expect("403 response")
    });
    let addr = stub::http(origin);
    let anthropic_echo = CapturingUpstream::spawn().await;
    let manager = Manager::with_live_refresher(
        codex_fleet(&anthropic_echo.url, &format!("http://{addr}")),
        None,
    );
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(response.headers()[CONTENT_TYPE], content_type);
    assert_eq!(response.headers()["cf-ray"], "test-ray");
    assert_eq!(
        response
            .headers()
            .get("cf-mitigated")
            .and_then(|value| value.to_str().ok()),
        challenge.then_some("challenge")
    );
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("403 body");
    assert_eq!(
        bytes.as_ref(),
        body.as_bytes(),
        "the upstream body survives"
    );
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let account = &snapshot.accounts[1];
    assert_eq!(
        account.gate,
        GateReason::Ok,
        "a block is not a login refusal"
    );
    assert_eq!(account.codex.login_error, None);
    assert_eq!(account.rate_limited_until, None);
    let again = send_codex_turn(&manager).await;
    assert_eq!(
        again.status(),
        StatusCode::FORBIDDEN,
        "a shared IP block must not exhaust the account pool"
    );
    assert_eq!(snapshot.accounts[0].gate, GateReason::Ok);
    assert_eq!(anthropic_echo.request_count(), 0);
}

#[tokio::test]
async fn a_codex_cloudflare_html_403_is_forwarded_without_blaming_the_login() {
    assert_codex_block_is_forwarded(
        "text/html",
        false,
        "<!DOCTYPE html><html><body>Cloudflare blocked this request</body></html>",
    )
    .await;
}

#[tokio::test]
async fn a_codex_region_403_is_forwarded_without_blaming_the_login() {
    assert_codex_block_is_forwarded(
            "application/json",
            false,
            r#"{"error":{"code":"unsupported_country_region_territory","message":"Country, region, or territory not supported"}}"#,
        )
        .await;
}

#[tokio::test]
async fn a_codex_challenge_header_overrides_an_auth_shaped_403_body() {
    assert_codex_block_is_forwarded(
        "application/json",
        true,
        r#"{"error":{"type":"authentication_error","code":"invalid_api_key"}}"#,
    )
    .await;
}

#[tokio::test]
async fn codex_model_failover_times_the_first_byte_the_client_receives() {
    let origin = Router::new().fallback(|headers: HeaderMap| async move {
            if headers["authorization"] == "Bearer at-bob" {
                return Response::builder().status(StatusCode::BAD_REQUEST)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"detail":"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."}"#)).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(HEADER_DELAY_MS)).await;
            Response::builder().header(CONTENT_TYPE, "text/event-stream")
                .body(Body::from("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-6.1-sol\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n")).unwrap()
        });
    let upstream = stub::http(origin).url();
    let mut config = codex_fleet_with_carol("http://127.0.0.1:0", &upstream);
    config.accounts[1].priority = Some(0);
    config.accounts[2].priority = Some(1);
    let manager = Manager::with_live_refresher(config, None);
    let (sink, _guard) = debug_sink();
    let response = drive_full(
        manager,
        Method::POST,
        crate::base::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"model":"gpt-6.1-sol","input":[]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap();
    let log = wait_for_line(&sink, "request usage").await;
    let lines = lines_with(&log, "request usage");
    assert_eq!(lines.len(), 1, "{log}");
    let line = lines[0];
    assert_eq!(field(line, "attempts"), Some("2"), "{line}");
    assert_eq!(field(line, "switched"), Some("true"), "{line}");
    assert!(
        ms_field(line, "first_byte_ms") >= HEADER_DELAY_MS,
        "first byte must come from the served stream, not the rejected body: {line}"
    );
}

#[tokio::test]
async fn codex_model_refusals_try_a_supporting_sibling_without_benching_free() {
    for shape in ["detail", "error", "message"] {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let captured = Arc::clone(&seen);
        let origin = Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
                let captured = Arc::clone(&captured);
                async move {
                    let bearer = headers["authorization"].to_str().unwrap().to_owned();
                    captured.lock().unwrap().push(bearer.clone());
                    let model: Value = serde_json::from_slice(&body).unwrap();
                    if bearer == "Bearer at-bob" && model["model"] == "gpt-6.1-sol" {
                        let detail = "The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.";
                        let error = match shape {
                            "detail" => serde_json::json!({"detail": detail}),
                            "error" => serde_json::json!({"error": {"message": detail}}),
                            _ => serde_json::json!({"message": detail}),
                        };
                        Response::builder().status(StatusCode::BAD_REQUEST).header(CONTENT_TYPE, "application/json")
                            .body(Body::from(error.to_string())).unwrap()
                    } else { Response::new(Body::from("{}")) }
                }
            });
        let upstream = stub::http(origin).url();
        let mut config = codex_fleet_with_carol("http://127.0.0.1:0", &upstream);
        config.accounts[1].priority = Some(0);
        config.accounts[2].priority = Some(1);
        let manager = Manager::with_live_refresher(config, None);
        for model in ["gpt-6.1-sol", "gpt-6-luna"] {
            let body = serde_json::json!({"model": model, "input": []}).to_string();
            let response = drive_full(
                Arc::clone(&manager),
                Method::POST,
                crate::base::provider::CODEX_RESPONSES_PATH,
                Some(loopback_peer()),
                &[
                    ("authorization", "Bearer client-own"),
                    ("content-type", "application/json"),
                ],
                &body,
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{shape}: a supporting account serves {model}"
            );
        }
        assert_eq!(
            *seen.lock().unwrap(),
            ["Bearer at-bob", "Bearer at-carol", "Bearer at-bob"]
        );
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(snapshot.accounts[1].gate, GateReason::Ok);
        assert_eq!(snapshot.accounts[1].codex.login_error, None);
        assert_eq!(snapshot.accounts[1].rate_limited_until, None);
    }
}

#[tokio::test]
async fn codex_model_refusals_explain_an_unavailable_supporting_account() {
    for blocked in ["disabled", "held", "quota", "credential"] {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = Arc::clone(&requests);
        let origin = Router::new().fallback(move |headers: HeaderMap| {
                let captured = Arc::clone(&captured);
                async move {
                    captured.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = if headers["authorization"] == "Bearer at-carol" {
                        (StatusCode::FORBIDDEN, r#"{"error":{"code":"invalid_api_key","type":"authentication_error"}}"#)
                    } else {
                        (StatusCode::BAD_REQUEST, r#"{"detail":"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."}"#)
                    };
                    Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(Body::from(body)).unwrap()
                }
            });
        let upstream = stub::http(origin).url();
        let mut config = codex_fleet_with_carol("http://127.0.0.1:0", &upstream);
        config.accounts[1].priority = Some(0);
        config.accounts[2].priority = Some(1);
        config.accounts[2].disabled = Some(blocked == "disabled");
        let manager = Manager::with_live_refresher(config, None);
        match blocked {
            "held" => manager.mark_rate_limited(2, 300),
            "quota" => manager.mark_codex_quota_limited(2, 300),
            _ => {}
        }
        let response = drive_full(
            Arc::clone(&manager),
            Method::POST,
            crate::base::provider::CODEX_RESPONSES_PATH,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("content-type", "application/json"),
            ],
            r#"{"model":"gpt-6.1-sol","input":[]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{blocked}");
        assert!(!response.headers().contains_key("retry-after"));
        let body = body_json(response).await;
        assert_eq!(
            body["error"]["code"], "model_unavailable",
            "{blocked}: {body}"
        );
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("gpt-6.1-sol") && message.contains("pooled"));
        assert!(message.contains("Choose another model"));
        if blocked == "credential" {
            assert!(message.contains("tcr login --codex --account carol@example.com"));
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            if blocked == "credential" { 2 } else { 1 }
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[1].gate,
            GateReason::Ok
        );
    }
}

#[tokio::test]
async fn codex_other_bad_requests_keep_the_upstream_response() {
    let body =
        r#"{"error":{"message":"The temperature parameter is not supported for this model."}}"#;
    let origin = Router::new().fallback(move || async move {
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    });
    let upstream = stub::http(origin).url();
    let manager = Manager::with_live_refresher(
        codex_fleet_with_carol("http://127.0.0.1:0", &upstream),
        None,
    );
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap()
            .as_ref(),
        body.as_bytes()
    );
    assert!(
        manager
            .snapshot(OffsetDateTime::now_utc())
            .accounts
            .iter()
            .all(|row| row.gate == GateReason::Ok)
    );
}

#[tokio::test]
async fn an_unreadable_codex_403_is_held_briefly_and_rotates_without_relogin() {
    for stalled in [false, true] {
        let first = Arc::new(std::sync::Mutex::new(None::<String>));
        let observed = Arc::clone(&first);
        let origin = Router::new().fallback(move |headers: HeaderMap| {
            let observed = Arc::clone(&observed);
            async move {
                let bearer = headers["authorization"].to_str().unwrap().to_owned();
                let original = observed
                    .lock()
                    .unwrap()
                    .get_or_insert(bearer.clone())
                    .clone();
                if bearer != original {
                    return Response::new(Body::from("{}"));
                }
                let stream = futures::stream::unfold(0, move |step| async move {
                    match step {
                        0 => Some((Ok::<_, std::io::Error>(Bytes::from_static(b"{")), 1)),
                        1 => {
                            if stalled {
                                std::future::pending::<()>().await;
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            Some((Err(std::io::Error::other("fixture body disconnected")), 2))
                        }
                        _ => None,
                    }
                });
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        });
        let upstream = stub::http(origin).url();
        let manager = Manager::with_live_refresher(
            codex_fleet_with_carol("http://127.0.0.1:0", &upstream),
            None,
        );
        let response = tokio::time::timeout(Duration::from_secs(8), send_codex_turn(&manager))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "stalled={stalled}: a healthy sibling serves this turn"
        );
        let failed = codex_index_of(first.lock().unwrap().as_deref().unwrap());
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        let row = &snapshot.accounts[failed];
        assert!(
            row.rate_limited_until.is_some(),
            "an unreadable access response must not repeat immediately"
        );
        assert_ne!(row.gate, GateReason::Refused);
        assert_eq!(row.codex.login_error, None);
    }
}

#[tokio::test]
async fn mixed_codex_workspace_and_credential_refusals_keep_both_remedies() {
    let origin = Router::new().fallback(|headers: HeaderMap| async move {
            let body = if headers["authorization"] == "Bearer at-bob" {
                r#"{"error":{"type":"authentication_error","code":"invalid_api_key","message":"Login rejected"}}"#
            } else {
                r#"{"error":{"code":"workspace_deactivated","message":"Workspace deactivated"}}"#
            };
            Response::builder().status(StatusCode::FORBIDDEN).header(CONTENT_TYPE, "application/json")
                .body(Body::from(body)).unwrap()
        });
    let upstream = stub::http(origin).url();
    let manager = Manager::with_live_refresher(
        codex_fleet_with_carol("http://127.0.0.1:0", &upstream),
        None,
    );
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.headers().contains_key("retry-after"));
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "workspace_deactivated");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("Workspace deactivated"));
    assert!(
        message.contains("tcr login --codex --account bob@example.com"),
        "{message}"
    );
}

#[tokio::test]
async fn codex_account_access_403_rotates_without_requesting_relogin() {
    let first_bearer = Arc::new(std::sync::Mutex::new(None::<String>));
    let captured = Arc::clone(&first_bearer);
    let origin = Router::new().fallback(move |req: Request| {
            let captured = Arc::clone(&captured);
            async move {
                let bearer = req.headers()["authorization"].to_str().unwrap().to_owned();
                let first = captured.lock().unwrap().get_or_insert(bearer.clone()).clone();
                if first == bearer {
                    Response::builder().status(StatusCode::FORBIDDEN).header("content-type", "application/json")
                        .body(Body::from(r#"{"error":{"code":"workspace_deactivated","message":"Workspace deactivated"}}"#)).unwrap()
                } else { Response::new(Body::from("{}")) }
            }
        });
    let url = stub::http(origin).url();
    let manager =
        Manager::with_live_refresher(codex_fleet_with_carol("http://127.0.0.1:9", &url), None);
    assert_eq!(send_codex_turn(&manager).await.status(), StatusCode::OK);
    let blocked = codex_index_of(first_bearer.lock().unwrap().as_deref().unwrap());
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(snap.accounts[blocked].codex.login_error, None);
    assert_eq!(snap.accounts[blocked].gate, GateReason::Refused);
}

#[test]
fn codex_workspace_refusal_codes_ignore_case_and_whitespace() {
    for code in [
        "workspace_deactivated",
        "deactivated_workspace",
        "workspace_disabled",
        "account_deactivated",
        "account_disabled",
        "organization_deactivated",
    ] {
        let body = serde_json::json!({"error":{"code":format!(" {} ", code.to_uppercase())}});
        assert!(
            codex_forbidden_is_account_error(&HeaderMap::new(), body.to_string().as_bytes()),
            "{code}"
        );
    }
}

#[tokio::test]
async fn codex_workspace_refusals_keep_answering_403_during_the_hold() {
    let origin = Router::new().fallback(|| async {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":{"code":"workspace_deactivated","message":"Workspace deactivated"}}"#,
            ))
            .unwrap()
    });
    let url = stub::http(origin).url();
    let manager =
        Manager::with_live_refresher(codex_fleet_with_carol("http://127.0.0.1:9", &url), None);
    for attempt in 0..2 {
        let response = send_codex_turn(&manager).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "attempt {attempt}"
        );
        assert!(!response.headers().contains_key("retry-after"));
        let body = to_bytes(response.into_body(), 65536).await.unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(!text.contains("pool_unavailable"), "{text}");
        assert!(!text.contains("tcr login"), "{text}");
    }
    for account in manager
        .snapshot(OffsetDateTime::now_utc())
        .accounts
        .iter()
        .filter(|a| a.provider == Provider::Codex)
    {
        assert_eq!(account.gate, GateReason::Refused);
        assert_eq!(account.codex.login_error, None);
    }
}

#[tokio::test]
async fn codex_workspace_refusal_stays_403_when_a_quota_gate_outlasts_its_hold() {
    for (window, gate) in [
        ("primary", GateReason::FiveHour),
        ("secondary", GateReason::SevenDay),
    ] {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&requests);
        let origin = Router::new().fallback(move || {
            let seen = Arc::clone(&seen);
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"error":{"code":"workspace_deactivated"}}"#))
                    .unwrap()
            }
        });
        let url = stub::http(origin).url();
        let mut config = codex_fleet_with_carol("http://127.0.0.1:9", &url);
        config.accounts[2].disabled = Some(true);
        let manager = Manager::with_live_refresher(config, None);
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_bytes(format!("x-codex-{window}-used-percent").as_bytes()).unwrap(),
            HeaderValue::from_static("95"),
        );
        headers.insert(
            HeaderName::from_bytes(format!("x-codex-{window}-reset-at").as_bytes()).unwrap(),
            HeaderValue::from_str(&(OffsetDateTime::now_utc().unix_timestamp() + 7200).to_string())
                .unwrap(),
        );
        manager.update_quota(1, &headers, 200);
        let hint = manager.exhaustion_hint(OffsetDateTime::now_utc(), false, Provider::Codex, None);
        assert_eq!(hint.gated.get(&gate), Some(&1));
        assert_eq!(
            send_codex_turn(&manager).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let hint = manager.exhaustion_hint(OffsetDateTime::now_utc(), false, Provider::Codex, None);
        assert_eq!(hint.gated.get(&gate), Some(&1));
        assert!(
            !hint.gated.contains_key(&GateReason::Refused),
            "the longer quota gate hides the active refusal hold"
        );
        let response = send_codex_turn(&manager).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{window}");
        assert!(!response.headers().contains_key("retry-after"));
        assert_eq!(
            body_json(response).await["error"]["code"],
            "credential_refused"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "requests during the refusal hold never reach the origin"
        );
    }
}

#[tokio::test]
async fn codex_workspace_refusal_with_a_disabled_sibling_is_still_403() {
    let origin = Router::new().fallback(|| async {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":{"code":"workspace_deactivated","message":"Workspace deactivated"}}"#,
            ))
            .unwrap()
    });
    let url = stub::http(origin).url();
    let mut config = codex_fleet_with_carol("http://127.0.0.1:9", &url);
    config.accounts[2].disabled = Some(true);
    let manager = Manager::with_live_refresher(config, None);
    for attempt in 0..2 {
        let response = send_codex_turn(&manager).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "attempt {attempt}"
        );
        assert!(!response.headers().contains_key("retry-after"));
        let body = to_bytes(response.into_body(), 65536).await.unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(!text.contains("pool_unavailable"), "{text}");
        assert!(!text.contains("tcr login"), "{text}");
    }
    for account in manager
        .snapshot(OffsetDateTime::now_utc())
        .accounts
        .iter()
        .filter(|a| a.provider == Provider::Codex && a.gate != GateReason::Disabled)
    {
        assert_eq!(account.gate, GateReason::Refused);
        assert_eq!(account.codex.login_error, None);
    }
}

/// A Codex origin that refuses EVERY request with a 403, recording each bearer in order.
async fn spawn_codex_origin_forbidding_all() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let refused: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let refused_by = Arc::clone(&refused);
    let origin = Router::new().fallback(move |req: Request| {
            let refused_by = Arc::clone(&refused_by);
            async move {
                let bearer = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                refused_by
                    .lock()
                    .expect("refusal record poisoned")
                    .push(bearer);
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"error":{"type":"authentication_error","code":"invalid_api_key","message":"login refused"}}"#,
                    ))
                    .expect("403 response")
            }
        });
    let addr = stub::http(origin);
    (format!("http://{addr}"), refused)
}

/// One sentence of the Codex credential refusal, for the account `name`.
fn credential_refused_sentence(name: &str) -> String {
    format!(
        "ChatGPT refused {name}'s login — sign it in again with `tcr login --codex --account \
             {name}` or TcrBar's Re-login\u{2026}"
    )
}

/// **A Codex credential refused upstream gets a 403 in codex's own envelope**, naming the
/// command that signs the account in again: the status that leaves codex's own login alone
/// (a 401 would have codex reload and refresh it), with no `retry-after`, because waiting
/// does not fix a refused login. Two refused accounts give one sentence each, in the order
/// they were refused.
///
/// Watched red: answering codex with the Anthropic 502 fails the status and the body.
#[tokio::test]
async fn a_refused_codex_credential_gets_codexs_envelope() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let (codex_url, _refused) = spawn_codex_origin_forbidding_all().await;
    let manager = Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_url), None);
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert!(!response.headers().contains_key("retry-after"));
    assert_eq!(
        body_json(response).await,
        serde_json::json!({"error": {
            "type": "proxy_error",
            "code": "credential_refused",
            "message": credential_refused_sentence("bob@example.com/codex"),
        }})
    );

    // A fresh manager: a 403 holds its account, and a second request would meet the hold.
    let (codex_url, refused) = spawn_codex_origin_forbidding_all().await;
    let manager = Manager::with_live_refresher(
        codex_fleet_with_carol(&anthropic_echo.url, &codex_url),
        None,
    );
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let order: Vec<&str> = refused
        .lock()
        .expect("refusal record poisoned")
        .iter()
        .map(|bearer| match codex_index_of(bearer) {
            1 => "bob@example.com/codex",
            _ => "carol@example.com/codex",
        })
        .collect();
    assert_eq!(order.len(), 2, "{order:?}");
    assert_eq!(
        body_json(response).await["error"]["message"],
        format!(
            "{} {}",
            credential_refused_sentence(order[0]),
            credential_refused_sentence(order[1])
        )
    );
    assert_eq!(anthropic_echo.request_count(), 0);
}

/// A refused Codex login stays held and subsequent requests preserve access-denied status.
/// The next request never reaches the origin and receives no misleading rate-limit retry.
/// The locally generated refusal increments the refusal counter.
///
/// Watched red: without the hold, the second request reaches the origin and is served.
#[tokio::test]
async fn a_refused_codex_login_is_held_and_keeps_its_access_denied_status() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let (codex_url, refused, served) = spawn_codex_origin_forbidding_the_first().await;
    let manager = Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_url), None);

    let first = send_codex_turn(&manager).await;
    assert_eq!(first.status(), StatusCode::FORBIDDEN);
    assert!(!first.headers().contains_key("retry-after"));
    assert_eq!(
        body_json(first).await["error"]["message"],
        credential_refused_sentence("bob@example.com/codex")
    );

    let second = send_codex_turn(&manager).await;
    assert_eq!(second.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        second
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert!(!second.headers().contains_key("retry-after"));
    let second_body = body_json(second).await;
    assert_eq!(second_body["error"]["type"], "proxy_error");
    assert_eq!(second_body["error"]["code"], "credential_refused");

    assert!(refused.lock().expect("poisoned").is_some(), "one refusal");
    assert!(
        served.lock().expect("poisoned").is_empty(),
        "the origin saw exactly one request"
    );
    assert_eq!(
        refusals(&manager),
        (1, Some(CodexRefusalKind::CredentialRefused))
    );
}

#[tokio::test]
async fn codex_failed_stream_limits_move_the_next_pinned_turn() {
    for code in [
        "insufficient_quota",
        "usage_not_included",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded",
        "rate_limit_exceeded",
        "slow_down",
        "context_length_exceeded",
        "invalid_prompt",
        "server_is_overloaded",
    ] {
        for event in ["response.failed", "error"] {
            let reset = OffsetDateTime::now_utc().unix_timestamp() + 7200;
            let error = serde_json::json!({
                "code": code, "resets_at": reset,
                "headers": {"retry-after": "25"}
            });
            let failure = if event == "response.failed" {
                serde_json::json!({"type": event, "response": {"error": error}})
            } else {
                serde_json::json!({"type": event, "error": error})
            };
            let failed_bytes = format!("data: {failure}\n\n");
            let seen = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
            let origin_seen = Arc::clone(&seen);
            let origin_failure = failed_bytes.clone();
            let origin = Router::new().fallback(move |req: Request| {
                    let seen = Arc::clone(&origin_seen);
                    let failed_bytes = origin_failure.clone();
                    async move {
                        let idx = codex_index_of(req.headers()["authorization"].to_str().unwrap());
                        let mut seen = seen.lock().unwrap();
                        seen.push(idx);
                        let body = if seen.len() == 1 {
                            failed_bytes
                        } else {
                            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n".into()
                        };
                        Response::builder().header(CONTENT_TYPE, "text/event-stream")
                            .body(Body::from(body)).unwrap()
                    }
                });
            let url = stub::http(origin).url();
            let manager = Manager::with_live_refresher(
                codex_fleet_with_carol("http://127.0.0.1:9", &url),
                None,
            );
            let send = || async {
                use tower::ServiceExt as _;
                let mut request = Request::builder()
                    .method(Method::POST)
                    .uri(crate::base::provider::CODEX_RESPONSES_PATH)
                    .header("authorization", "Bearer client-own")
                    .header(CONTENT_TYPE, "application/json")
                    .header("session-id", "test-stream-quota-session")
                    .body(Body::from("{}"))
                    .unwrap();
                request.extensions_mut().insert(ClientAddr(loopback_peer()));
                request.extensions_mut().insert(SessionKey(1));
                app(Arc::clone(&manager)).oneshot(request).await.unwrap()
            };
            let first = send().await;
            assert_eq!(first.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(first.into_body(), MAX_BODY_BYTES).await.unwrap(),
                failed_bytes
            );
            let first_idx = seen.lock().unwrap()[0];
            assert_eq!(
                manager.affinity_pin(stable_hash("sid:", "test-stream-quota-session")),
                Some(first_idx),
                "the test must exercise a real conversation pin"
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if manager.snapshot(OffsetDateTime::now_utc()).accounts[first_idx]
                        .last_stream_error
                        .as_deref()
                        == Some(code)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("stream error was recorded");
            let snapshot = manager.snapshot(OffsetDateTime::now_utc());
            let request_error = matches!(
                code,
                "context_length_exceeded" | "invalid_prompt" | "server_is_overloaded"
            );
            if request_error {
                assert!(
                    snapshot.accounts[first_idx].rate_limited_until.is_none(),
                    "{event}/{code} must not hold a healthy account"
                );
            } else {
                let held = snapshot.accounts[first_idx]
                    .rate_limited_until
                    .unwrap_or_else(|| panic!("{event}/{code} must hold its serving account"));
                if matches!(code, "rate_limit_exceeded" | "slow_down") {
                    assert!(
                        (20..=25).contains(&(held - OffsetDateTime::now_utc()).whole_seconds())
                    );
                } else {
                    assert!((held.unix_timestamp() - reset).abs() <= 1, "{event}/{code}");
                }
            }
            let second = send().await;
            assert_eq!(second.status(), StatusCode::OK);
            to_bytes(second.into_body(), MAX_BODY_BYTES).await.unwrap();
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            if request_error {
                assert_eq!(
                    seen[0], seen[1],
                    "{event}/{code} must preserve the healthy pin"
                );
            } else {
                assert_ne!(
                    seen[0], seen[1],
                    "{event}/{code} must move a pinned conversation"
                );
            }
        }
    }
}

#[tokio::test]
async fn unusable_codex_siblings_do_not_hide_a_spent_pools_reset() {
    for state in [
        "disabled",
        "parked",
        "reserved",
        "login",
        "unsupported",
        "locked",
    ] {
        let mut config = codex_fleet_with_carol("http://127.0.0.1:9", "http://127.0.0.1:9");
        match state {
            "disabled" => config.accounts[2].disabled = Some(true),
            "parked" | "reserved" => {
                config.accounts[2].groups = Some(vec!["test-group".into()]);
                config.group_settings.insert(
                    "test-group".into(),
                    config::GroupSettings {
                        parked: state == "parked",
                        reserved: state == "reserved",
                        ..Default::default()
                    },
                );
            }
            "unsupported" => {
                config.accounts[2].egress = config::Egress::Via(tcr_peer_wire::PeerId([0x55; 32]));
                config.accounts[2].egress_strict = true;
            }
            "locked" => config.lock_account = Some(config.accounts[1].name.clone()),
            "login" => {}
            _ => unreachable!(),
        }
        let manager = Manager::with_live_refresher(config, None);
        if state == "login" {
            manager.mark_error(2);
        }
        let now = OffsetDateTime::now_utc();
        let reset = now.unix_timestamp() + 3 * 86400;
        let headers = HeaderMap::new();
        let body = serde_json::json!({"error":{"type":"usage_limit_reached", "resets_at":reset}})
            .to_string();
        let refusal =
            crate::records::quota::CodexQuotaRefusal::parse(&headers, body.as_bytes(), now)
                .unwrap();
        manager.update_quota_with_codex_refusal(1, &headers, 429, Some(&refusal));
        manager.mark_codex_quota_limited(1, 3 * 86400);
        let response = exhausted_response(&manager, now, 2, false, Provider::Codex, None, None);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = body_json(response).await;
        assert_eq!(
            body["error"]["type"], "usage_limit_reached",
            "{state}: {body}"
        );
        assert!(
            (body["error"]["resets_at"].as_i64().unwrap() - reset).abs() <= 1,
            "{state}: {body}"
        );
    }
}

#[tokio::test]
async fn codex_refusal_probe_releases_the_account_hold() {
    let manager = Manager::with_live_refresher(
        codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9"),
        None,
    );
    let headers = HeaderMap::new();
    let refusal = crate::records::quota::CodexQuotaRefusal::parse(
        &headers,
        br#"{"error":{"type":"insufficient_quota"}}"#,
        OffsetDateTime::now_utc(),
    )
    .expect("body quota refusal");
    manager.update_quota_with_codex_refusal(1, &headers, 429, Some(&refusal));
    manager.mark_codex_quota_limited(1, 6 * 86400);
    manager.apply_usage(
        1,
        &crate::records::probe::Usage {
            codex: Some(crate::records::codex_probe::CodexProbeFacts {
                allowed: Some(true),
                limit_reached: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(snapshot.accounts[1].rate_limited_until, None);
    assert_eq!(snapshot.accounts[1].gate, GateReason::Ok);
}

#[tokio::test]
async fn codex_refusal_without_a_full_window_caps_the_retry_hold() {
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 6 * 86400;
    let origin = Router::new().fallback(move || async move {
        Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("content-type", "application/json")
            .header("retry-after", "999999")
            .header("x-codex-primary-used-percent", "35")
            .header("x-codex-primary-window-minutes", "10080")
            .header("x-codex-primary-reset-at", reset.to_string())
            .body(Body::from(r#"{"error":{"type":"insufficient_quota"}}"#))
            .unwrap()
    });
    let url = stub::http(origin).url();
    let manager = Manager::with_live_refresher(codex_fleet("http://127.0.0.1:9", &url), None);
    assert_eq!(
        send_codex_turn(&manager).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let remaining = manager.snapshot(OffsetDateTime::now_utc()).accounts[1]
        .rate_limited_until
        .unwrap()
        - OffsetDateTime::now_utc();
    assert!(
        (1..=3600).contains(&remaining.whole_seconds()),
        "hold is {remaining}"
    );
}

#[tokio::test]
async fn codex_accounts_that_cannot_recover_by_waiting_fail_immediately() {
    for state in ["disabled", "parked", "login", "reserved"] {
        let mut config = codex_fleet("http://127.0.0.1:9", "http://127.0.0.1:9");
        if state == "disabled" {
            config.accounts[1].disabled = Some(true);
        }
        if state == "parked" {
            config.accounts[1].groups = Some(vec!["test-group".into()]);
            config.group_settings.insert(
                "test-group".into(),
                config::GroupSettings {
                    parked: true,
                    ..Default::default()
                },
            );
        }
        if state == "reserved" {
            config.accounts[1].groups = Some(vec!["test-group".into()]);
            config.group_settings.insert(
                "test-group".into(),
                config::GroupSettings {
                    reserved: true,
                    ..Default::default()
                },
            );
        }
        let manager = Manager::with_live_refresher(config, None);
        if state == "login" {
            manager.mark_error(1);
        }
        let response = send_codex_turn(&manager).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{state} must not trigger Codex's 429 retry loop"
        );
        assert!(!response.headers().contains_key("retry-after"));
        assert_eq!(
            body_json(response).await["error"]["code"],
            "no_codex_account"
        );
    }
}

#[tokio::test]
async fn codex_locked_unsupported_exit_cannot_wait_for_an_unreachable_sibling() {
    let mut config = codex_fleet_with_carol("http://127.0.0.1:9", "http://127.0.0.1:9");
    config.lock_account = Some(config.accounts[1].name.clone());
    config.accounts[1].egress = config::Egress::Via(tcr_peer_wire::PeerId([0x55; 32]));
    config.accounts[1].egress_strict = true;
    let manager = Manager::with_live_refresher(config, None);
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.headers().contains_key("retry-after"));
}

async fn check_codex_quota_body_case(
    body: String,
    reached_header: bool,
    quota: bool,
    body_reset: bool,
) {
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 7200;
    let reply = body.replace("RESET", &reset.to_string());
    let origin = Router::new().fallback(move || {
        let reply = reply.clone();
        async move {
            let mut response = Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header(CONTENT_TYPE, "application/json")
                .header("retry-after", "30")
                .header(
                    "x-codex-primary-used-percent",
                    if quota && !body_reset { "100" } else { "10" },
                )
                .header("x-codex-primary-reset-at", (reset + 3600).to_string());
            if reached_header {
                response =
                    response.header(crate::records::quota::CODEX_REACHED_TYPE_HEADER, "primary");
            }
            response.body(Body::from(reply)).unwrap()
        }
    });
    let url = stub::http(origin).url();
    let mut config = codex_fleet("http://127.0.0.1:9", &url);
    config
        .extra
        .insert("revalidationServe".into(), serde_json::json!(false));
    let manager = Manager::with_live_refresher(config, None);
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = body_json(response).await;
    assert_eq!(
        body["error"]["type"],
        if quota {
            "usage_limit_reached"
        } else {
            "pool_unavailable"
        }
    );
    let held_until = manager.snapshot(OffsetDateTime::now_utc()).accounts[1]
        .rate_limited_until
        .unwrap()
        .unix_timestamp();
    let expected = if quota {
        reset + if body_reset { 0 } else { 3600 }
    } else {
        reset - 7200 + 30
    };
    assert!(
        (held_until - expected).abs() <= 2,
        "held until {held_until}, expected {expected}"
    );
    if quota {
        assert!(
            (body["error"]["resets_at"].as_i64().unwrap() - expected).abs() <= 2,
            "{body}"
        );
    }
}

#[tokio::test]
async fn codex_quota_body_without_headers_holds_to_its_reset() {
    check_codex_quota_body_case(
        r#"{"error":{"type":"usage_limit_reached","resets_at":RESET}}"#.into(),
        false,
        true,
        true,
    )
    .await;
}

#[tokio::test]
async fn codex_quota_header_without_body_holds_to_its_window() {
    check_codex_quota_body_case("{}".into(), true, true, false).await;
}

#[tokio::test]
async fn codex_quota_body_reset_outranks_the_header_window() {
    check_codex_quota_body_case(
        r#"{"error":{"type":"usage_limit_reached","resets_at":RESET}}"#.into(),
        true,
        true,
        true,
    )
    .await;
}

#[tokio::test]
async fn codex_relative_quota_reset_is_in_seconds() {
    check_codex_quota_body_case(
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":7200}}"#.into(),
        false,
        true,
        true,
    )
    .await;
}

#[tokio::test]
async fn codex_plain_rate_limit_uses_retry_after() {
    check_codex_quota_body_case("{}".into(), false, false, false).await;
}

#[tokio::test]
async fn codex_malformed_quota_body_uses_retry_after() {
    check_codex_quota_body_case("{broken".into(), false, false, false).await;
}

#[tokio::test]
async fn codex_quota_types_and_codes_all_stop_client_retries() {
    for error in [
        r#"{"type":"usage_not_included"}"#,
        r#"{"type":"insufficient_quota"}"#,
        r#"{"code":"insufficient_quota"}"#,
        r#"{"code":"credit_balance_exhausted"}"#,
        r#"{"code":"organization_spend_limit_exceeded"}"#,
        r#"{"code":"project_spend_limit_exceeded"}"#,
        r#"{"code":"organization_usage_limit_exceeded"}"#,
    ] {
        let mut error: Value = serde_json::from_str(error).unwrap();
        error["resets_at"] = serde_json::json!("RESET");
        let body = serde_json::json!({"error":error})
            .to_string()
            .replace("\"RESET\"", "RESET");
        check_codex_quota_body_case(body, false, true, true).await;
    }
}

#[tokio::test]
async fn codex_exhausted_pool_reports_the_earliest_body_reset() {
    let earliest = OffsetDateTime::now_utc().unix_timestamp() + 7200;
    let later = earliest + 3600;
    let refused = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&refused);
    let origin = Router::new().fallback(move |headers: HeaderMap| {
        let bearer = headers.get("authorization").unwrap().to_str().unwrap();
        let reset = match bearer {
            "Bearer at-bob" => later,
            "Bearer at-carol" => earliest,
            other => panic!("unexpected Codex bearer: {other}"),
        };
        seen.lock().unwrap().push((bearer.to_string(), reset));
        async move {
            Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"error":{"type":"usage_limit_reached","resets_at":reset}})
                        .to_string(),
                ))
                .unwrap()
        }
    });
    let url = stub::http(origin).url();
    let mut config = codex_fleet_with_carol("http://127.0.0.1:9", &url);
    // Bob is refused first; Carol, refused second, has the earlier reset.
    config.accounts[1].priority = Some(0);
    config.accounts[2].priority = Some(1);
    config
        .extra
        .insert("revalidationServe".into(), serde_json::json!(false));
    let manager = Manager::with_live_refresher(config, None);
    let response = send_codex_turn(&manager).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = body_json(response).await;
    assert_eq!(
        *refused.lock().unwrap(),
        [
            ("Bearer at-bob".to_string(), later),
            ("Bearer at-carol".to_string(), earliest),
        ],
        "the account refused second must have the earliest reset"
    );
    assert_eq!(body["error"]["type"], "usage_limit_reached");
    assert!(
        (body["error"]["resets_at"].as_i64().unwrap() - earliest).abs() <= 2,
        "{body}"
    );
}
