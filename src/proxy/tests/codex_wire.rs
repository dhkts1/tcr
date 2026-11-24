//! Codex on the wire: the 426 for a websocket, the account-id and bearer swap, and which host serves which provider.

use super::*;

// --- Codex: the 426, the identity swap, the exhausted body --------------

//
// Every value below is a fixture: `11111111-…` is the serving account's id,
// `99999999-…` the CLIENT's, and no token is real. This repository is public.

/// `upgrade: websocket` is how Codex asks for its default transport
/// (`openai/codex` f82e365, `model-provider-info/src/lib.rs:561`); `h2c` is an
/// upgrade too and must not be read as one.
#[test]
fn wants_websocket_upgrade_reads_the_upgrade_header_case_insensitively() {
    let mut h = HeaderMap::new();
    assert!(!wants_websocket_upgrade(&h));
    h.insert("upgrade", HeaderValue::from_static("WebSocket"));
    assert!(wants_websocket_upgrade(&h));
    h.insert("upgrade", HeaderValue::from_static("h2c"));
    assert!(!wants_websocket_upgrade(&h));
}

/// The pooled bearer travels with ITS account's id, never with the id of the
/// account the client is logged in as; everything else the client sent goes
/// through as it came, compressed body framing included.
#[test]
fn codex_upstream_headers_swap_the_account_id_and_the_bearer_and_keep_the_rest() {
    let mut req = HeaderMap::new();
    req.insert(AUTHORIZATION, HeaderValue::from_static("Bearer client-own"));
    req.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("99999999-9999-4999-8999-999999999999"),
    );
    req.insert("session-id", HeaderValue::from_static("s-1"));
    req.insert("x-codex-turn-state", HeaderValue::from_static("opaque"));
    req.insert("content-encoding", HeaderValue::from_static("zstd"));
    req.insert("accept-encoding", HeaderValue::from_static("gzip"));
    let out = build_codex_upstream_headers(
        &req,
        "pooled-token",
        Some("11111111-1111-4111-8111-111111111111"),
    );
    assert_eq!(out.get(AUTHORIZATION).unwrap(), "Bearer pooled-token");
    assert_eq!(
        out.get("chatgpt-account-id").unwrap(),
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(out.get("session-id").unwrap(), "s-1");
    assert_eq!(
        out.get("x-codex-turn-state").unwrap(),
        "opaque",
        "the client's own sticky-routing token is forwarded verbatim"
    );
    assert_eq!(
        out.get("content-encoding").unwrap(),
        "zstd",
        "a compressed body keeps its encoding"
    );
    assert!(
        out.get("accept-encoding").is_none(),
        "we negotiate identity responses, as for Anthropic"
    );
}

/// A client that sent no account id still gets the serving account's.
#[test]
fn codex_upstream_headers_insert_the_account_id_when_the_client_sent_none() {
    let mut req = HeaderMap::new();
    req.insert(AUTHORIZATION, HeaderValue::from_static("Bearer client-own"));
    let out = build_codex_upstream_headers(
        &req,
        "pooled-token",
        Some("11111111-1111-4111-8111-111111111111"),
    );
    assert_eq!(
        out.get("chatgpt-account-id").unwrap(),
        "11111111-1111-4111-8111-111111111111"
    );
}

/// No id to offer is no id at all: the client's own may not ride along with
/// a bearer that belongs to someone else.
#[test]
fn codex_upstream_headers_drop_the_clients_account_id_when_we_have_none_to_offer() {
    let mut req = HeaderMap::new();
    req.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("99999999-9999-4999-8999-999999999999"),
    );
    let out = build_codex_upstream_headers(&req, "pooled-token", None);
    assert!(
        out.get("chatgpt-account-id").is_none(),
        "never forward another account's identity with our bearer"
    );
}

/// The one error shape Codex renders as a usage limit (`map_api_error`,
/// `codex-rs/codex-api/src/api_bridge.rs` at openai/codex f82e365).
#[test]
fn codex_exhausted_body_is_the_shape_codex_renders() {
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("a valid instant");
    let body: Value = serde_json::from_str(&codex_exhausted_body(now, 1234, true)).unwrap();
    assert_eq!(body["error"]["type"], "usage_limit_reached");
    assert_eq!(body["error"]["code"], "usage_limit_reached");
    assert_eq!(body["error"]["resets_in_seconds"], 1234);
    assert!(body["error"]["message"].as_str().unwrap().contains("1234"));
}

/// **Only a quota is a Codex usage limit.** codex ends its turn on `usage_limit_reached`
/// and retries any other 429 after its `retry-after`, so the claim is made when the gate
/// that times the wait is a five-hour or weekly window or the backend's own refusal, or,
/// with no gate timing it, when every account out is out on one of those. A hold, a dead
/// login, an operator's disable, park or reservation, and an empty `gated` (which an
/// "every reason" test would pass vacuously) are not.
#[test]
fn only_a_quota_gate_is_a_codex_usage_limit() {
    let hint = |binding: Option<GateReason>, gated: &[(GateReason, usize)]| ExhaustionHint {
        retry_after: 60,
        free_at: None,
        binding,
        gated: gated.iter().copied().collect(),
    };
    for quota in [
        GateReason::FiveHour,
        GateReason::SevenDay,
        GateReason::Rejected,
    ] {
        assert!(
            codex_limit_is_quota(&hint(Some(quota), &[(quota, 1), (GateReason::Hold, 1)])),
            "{quota:?} times the wait"
        );
        assert!(
            codex_limit_is_quota(&hint(None, &[(quota, 2)])),
            "every account is out on {quota:?}"
        );
    }
    assert!(
        codex_limit_is_quota(&hint(
            None,
            &[(GateReason::SevenDay, 1), (GateReason::Rejected, 1)]
        )),
        "every account is out on a quota, two different ones"
    );
    for other in [
        GateReason::Ok,
        GateReason::Hold,
        GateReason::Standard,
        GateReason::Login,
        GateReason::Disabled,
        GateReason::Reserved,
        GateReason::Parked,
        GateReason::FableWeekly,
    ] {
        assert!(
            !codex_limit_is_quota(&hint(Some(other), &[(other, 1), (GateReason::SevenDay, 1)])),
            "{other:?} times the wait"
        );
        assert!(
            !codex_limit_is_quota(&hint(None, &[(GateReason::SevenDay, 1), (other, 1)])),
            "one account is out on {other:?}"
        );
    }
    assert!(
        !codex_limit_is_quota(&hint(None, &[])),
        "no gate at all is no evidence of a quota"
    );
}

/// The 429 body for a Codex pool that is out for any reason but a quota: the envelope
/// codex parses, under a type that is none of the three it reads as a limit
/// (`usage_limit_reached`, `usage_not_included`, `insufficient_quota`), with no reset for
/// it to print. The wait is in `resets_in_seconds` and the message, for a person; a
/// 60-second fallback is not printed as if it had been measured.
#[test]
fn codex_pool_unavailable_body_claims_no_limit() {
    for (wait_secs, measured) in [(45, true), (60, false)] {
        let body: Value = serde_json::from_str(&codex_pool_unavailable_body(wait_secs, measured))
            .expect("a JSON body");
        assert_eq!(body["error"]["type"], "pool_unavailable", "{body}");
        assert_eq!(body["error"]["code"], "pool_unavailable", "{body}");
        for field in ["resets_at", "plan_type"] {
            assert!(body["error"].get(field).is_none(), "{field}: {body}");
        }
        assert_eq!(body["error"]["resets_in_seconds"], wait_secs, "{body}");
        let message = body["error"]["message"].as_str().expect("a message");
        assert!(
            message.contains(&format!("{wait_secs} seconds")),
            "{message}"
        );
        assert_eq!(
            message.contains("none reports when it frees"),
            !measured,
            "{message}"
        );
    }
}

// --- Codex through the handler: two origins, one mixed fleet ------------

const CODEX_CLIENT_ID: &str = "99999999-9999-4999-8999-999999999999";

/// **A WebSocket upgrade on the Codex inference path is answered 426 HERE**,
/// with a pooled Codex account free to serve it, and neither origin sees a
/// byte. A 426 is the status that moves Codex to SSE at once, for the rest of
/// the session; the real backend answers the upgrade 101 (measured
/// 2026-10-12), so forwarding it can never be the right move. No account is
/// spent on it either: none is even SELECTED, which is what the last
/// assertion reads (`last_selected_seq` is stamped by every pick, whereas
/// `requests` counts only serves and stays 0 for a pick answered 426).
///
/// Watched red: comment out step 1c' in `handle` and the upgrade goes out as
/// a bare GET on bob's credential: 200, and the Codex origin counts 1. Move
/// the 426 below the pick instead and only the selection assertion fails.
#[tokio::test]
async fn codex_upgrade_is_answered_426_and_never_forwarded() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let resp = drive_full(
        Arc::clone(&manager),
        Method::GET,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "AAAAAAAAAAAAAAAAAAAAAA=="),
            ("authorization", "Bearer client-own"),
        ],
        "",
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UPGRADE_REQUIRED,
        "the Codex origin saw {} request(s)",
        codex_echo.request_count()
    );
    assert_eq!(
        codex_echo.request_count(),
        0,
        "the upstream must see nothing"
    );
    assert_eq!(anthropic_echo.request_count(), 0);
    for idx in 0..manager.account_count() {
        assert_eq!(
            manager.last_selected_seq(idx),
            Some(0),
            "an upgrade is answered before any account is selected (account {idx})"
        );
    }
}

/// The 426 is the Codex inference path's alone: a `/v1/messages` request
/// carrying a stray `upgrade: websocket` still reaches the rotation and is
/// served.
///
/// Watched red: drop the provider and path conjuncts from step 1c' and this
/// request is answered 426.
#[tokio::test]
async fn an_anthropic_request_with_an_upgrade_header_still_reaches_the_rotation() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, "http://127.0.0.1:9"), None);
    let resp = drive_full(
        manager,
        Method::POST,
        "/v1/messages",
        Some(loopback_peer()),
        &[
            ("upgrade", "websocket"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(anthropic_echo.request_count(), 1, "served by the rotation");
}

/// The inference path carries the POOLED bearer and the serving account's
/// own id, exactly one of each, never the client's.
///
/// Watched red: build the Codex request's headers with `build_upstream_headers`
/// alone (no swap) and the client's id reaches the origin with our bearer.
/// Dropping `out.remove(...)` does NOT turn this red: `HeaderMap::insert`
/// already replaces every earlier value, so that line guards only the case
/// with no id to offer (`codex_upstream_headers_drop_the_clients_account_id_…`).
#[tokio::test]
async fn codex_inference_swaps_account_header_and_bearer() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let (status, body) = drive(
        manager,
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("chatgpt-account-id", CODEX_CLIENT_ID),
            ("session-id", "s-1"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let seen = codex_echo.last_request();
    assert_eq!(seen.path, crate::provider::CODEX_RESPONSES_PATH);
    assert_eq!(seen.header("authorization"), Some("Bearer at-bob"));
    assert_eq!(seen.header("chatgpt-account-id"), Some(CODEX_SERVING_ID));
    assert_eq!(
        seen.header_count("chatgpt-account-id"),
        1,
        "one identity travels with the bearer, never the client's beside it"
    );
    assert_eq!(seen.header("session-id"), Some("s-1"));
    assert_eq!(
        anthropic_echo.request_count(),
        0,
        "a Codex request never reaches the Anthropic origin"
    );
}

/// Each line of the recent-request log carries the provider its request was routed on,
/// because the dashboard draws a Codex path by its tail and must not guess the provider back
/// from a path it shortens.
#[tokio::test]
async fn codex_and_claude_requests_log_their_provider() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let (status, body) = drive(
        Arc::clone(&manager),
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("chatgpt-account-id", CODEX_CLIENT_ID),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let (status, body) = drive(
        Arc::clone(&manager),
        Method::POST,
        "/v1/messages",
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let logged: Vec<(String, String, Provider)> = manager
        .snapshot(OffsetDateTime::now_utc())
        .recent
        .into_iter()
        .map(|entry| (entry.path, entry.account, entry.provider))
        .collect();
    assert_eq!(
        logged,
        [
            (
                "/v1/messages".to_string(),
                "alice@example.com".to_string(),
                Provider::Anthropic
            ),
            (
                crate::provider::CODEX_RESPONSES_PATH.to_string(),
                "bob@example.com/codex".to_string(),
                Provider::Codex
            ),
        ],
        "most recent first"
    );
}

/// A Codex inference request that names a routing of its own, a data-residency override or
/// FedRAMP (`openai/codex` f82e365, `model-provider/src/workspace_routing.rs:12,39-77`,
/// `model-provider/src/bearer_auth_provider.rs:43-45`), comes from a workspace that is not
/// served as a pooled account is. It is relayed on the client's OWN credential, with its own
/// account id and its routing intact, and never pooled: no account is selected, and no pooled
/// bearer goes anywhere.
///
/// Watched red: without the routing relay, the request is pooled and carries `Bearer at-bob`.
#[tokio::test]
async fn a_codex_inference_request_naming_its_routing_keeps_the_client_credential() {
    for (name, value) in [
        ("x-openai-account-routing-override", "us"),
        ("x-openai-fedramp", "true"),
    ] {
        let anthropic_echo = CapturingUpstream::spawn().await;
        let codex_echo = CapturingUpstream::spawn().await;
        let manager =
            Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
        let (status, body) = drive(
            Arc::clone(&manager),
            Method::POST,
            crate::provider::CODEX_RESPONSES_PATH,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("chatgpt-account-id", CODEX_CLIENT_ID),
                (name, value),
                ("content-type", "application/json"),
            ],
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let seen = codex_echo.last_request();
        assert_eq!(
            seen.header("authorization"),
            Some("Bearer client-own"),
            "{name}: the client's own bearer, never a pooled one"
        );
        assert_eq!(seen.header("chatgpt-account-id"), Some(CODEX_CLIENT_ID));
        assert_eq!(
            seen.header(name),
            Some(value),
            "its routing travels with it"
        );
        for idx in 0..manager.account_count() {
            assert_eq!(
                manager.last_selected_seq(idx),
                Some(0),
                "{name}: no account is selected (account {idx})"
            );
        }
        assert_eq!(anthropic_echo.request_count(), 0);
    }
}

/// No pooled request carries the client's routing: [`build_codex_upstream_headers`] drops
/// both routing headers whatever the client sent, so a pooled bearer is never routed by the
/// client's workspace. A backstop to the relay above.
///
/// Watched red: drop the strip and both headers reach the origin beside the pooled bearer.
#[test]
fn pooled_codex_headers_never_carry_the_clients_routing() {
    let mut client = HeaderMap::new();
    client.insert(AUTHORIZATION, HeaderValue::from_static("Bearer client-own"));
    client.insert(
        "x-openai-account-routing-override",
        HeaderValue::from_static("us"),
    );
    client.insert("x-openai-fedramp", HeaderValue::from_static("true"));
    client.insert("session-id", HeaderValue::from_static("s-1"));
    let out = build_codex_upstream_headers(&client, "at-bob", Some(CODEX_SERVING_ID));
    assert_eq!(out.get("x-openai-account-routing-override"), None);
    assert_eq!(out.get("x-openai-fedramp"), None);
    assert_eq!(
        out.get(AUTHORIZATION).and_then(|v| v.to_str().ok()),
        Some("Bearer at-bob")
    );
    assert_eq!(
        out.get("session-id").and_then(|v| v.to_str().ok()),
        Some("s-1"),
        "every other header still rides along"
    );
}

/// A Codex row's `chatgpt-account-id` is its workspace: `orgUuid`, now that `accountUuid`
/// holds the ChatGPT user. A row saved before that split carries the workspace in
/// `accountUuid` and no `orgUuid`, and still sends it.
///
/// Watched red: with the header read from `account_uuid`, the split row sends its user.
#[tokio::test]
async fn the_account_header_comes_from_org_uuid_and_falls_back_to_account_uuid() {
    const USER: &str = "55555555-5555-4555-8555-555555555555";
    const WORKSPACE: &str = "66666666-6666-4666-8666-666666666666";
    for (org_uuid, sent) in [(Some(WORKSPACE), WORKSPACE), (None, USER)] {
        let anthropic_echo = CapturingUpstream::spawn().await;
        let codex_echo = CapturingUpstream::spawn().await;
        let mut config = codex_fleet(&anthropic_echo.url, &codex_echo.url);
        config.accounts[1].account_uuid = Some(USER.to_string());
        config.accounts[1].org_uuid = org_uuid.map(str::to_string);
        let manager = Manager::with_live_refresher(config, None);
        let (status, body) = drive(
            manager,
            Method::POST,
            crate::provider::CODEX_RESPONSES_PATH,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own"),
                ("chatgpt-account-id", CODEX_CLIENT_ID),
                ("content-type", "application/json"),
            ],
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let seen = codex_echo.last_request();
        assert_eq!(seen.header("authorization"), Some("Bearer at-bob"));
        assert_eq!(
            seen.header("chatgpt-account-id"),
            Some(sent),
            "org_uuid {org_uuid:?}"
        );
    }
}

/// A client that names no account still reaches the origin naming ours.
#[tokio::test]
async fn codex_inference_inserts_account_header_when_client_sent_none() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let (status, body) = drive(
        manager,
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("session-id", "s-1"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let seen = codex_echo.last_request();
    assert_eq!(seen.header("authorization"), Some("Bearer at-bob"));
    assert_eq!(seen.header("chatgpt-account-id"), Some(CODEX_SERVING_ID));
    assert_eq!(anthropic_echo.request_count(), 0);
}

/// The zstd body Codex usually sends is forwarded as it came, encoding header
/// and all: nothing here can read it, so nothing here may touch it.
#[tokio::test]
async fn codex_body_is_forwarded_byte_identical() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    // zstd magic + junk: not JSON
    let body: Vec<u8> = vec![
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x58, 0x41, 0x00, 0x00, 0x7b, 0x7d,
    ];
    let (status, answer) = drive_bytes(
        manager,
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
            ("content-encoding", "zstd"),
        ],
        body.clone(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&answer)
    );
    let seen = codex_echo.last_request();
    assert_eq!(seen.body, body);
    assert_eq!(seen.header("content-encoding"), Some("zstd"));
    assert_eq!(anthropic_echo.request_count(), 0);
}

/// An UNCOMPRESSED JSON body is the case where a rewrite could actually
/// change bytes, so it is the one that gives "never parsed, never spliced"
/// its teeth: this body carries both things the Anthropic path rewrites, a
/// `metadata.user_id` with an embedded account uuid (the splice) and a
/// `cache_control` breakpoint in `tools` (the 1-hour rewrite, on by default
/// for an all-OAuth fleet). Both are Anthropic facts; the Codex body goes out
/// byte for byte.
///
/// Watched red, twice: give the Codex arm the Anthropic splice and the
/// client's uuid is replaced by bob's; drop `provider == Anthropic` from the
/// cache-ttl rewrite's gate and the body is re-serialized with `"ttl":"1h"`.
#[tokio::test]
async fn codex_json_body_is_neither_spliced_nor_rewritten() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    assert!(
        manager.cache_ttl_rewrite_enabled(),
        "precondition: the 1-hour rewrite is on for this fleet"
    );
    let body = format!(
        r#"{{"model":"gpt-5","metadata":{{"user_id":"{{\"account_uuid\":\"{CODEX_CLIENT_ID}\",\"session_id\":\"s-1\"}}"}},"tools":[{{"type":"function","name":"f","cache_control":{{"type":"ephemeral"}}}}],"input":[]}}"#
    );
    assert!(
        matches!(
            crate::account_uuid::patch_account_uuid(body.as_bytes(), CODEX_SERVING_ID),
            std::borrow::Cow::Owned(_)
        ),
        "precondition: the Anthropic splice WOULD rewrite this body"
    );
    assert!(
        crate::cache_ttl::extend_all_ttls(body.as_bytes()).is_some(),
        "precondition: the 1-hour rewrite WOULD rewrite this body"
    );
    let (status, answer) = drive(
        manager,
        Method::POST,
        crate::provider::CODEX_RESPONSES_PATH,
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own"),
            ("content-type", "application/json"),
        ],
        &body,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&answer)
    );
    assert_eq!(
        String::from_utf8_lossy(&codex_echo.last_request().body),
        body,
        "a Codex body is forwarded byte for byte"
    );
}

/// Every Codex path but the inference one is the CLIENT's business (usage
/// display, memories, threads, plugins) and goes up under the client's own
/// bearer, to the Codex origin. With no bearer it goes up with none: a
/// pooled one is never put on it.
#[tokio::test]
async fn codex_non_inference_paths_relay_the_clients_own_bearer() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    let (status, _) = drive(
        Arc::clone(&manager),
        Method::GET,
        crate::provider::CODEX_USAGE_PATH,
        Some(loopback_peer()),
        &[("authorization", "Bearer client-own")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = codex_echo.last_request();
    assert_eq!(seen.path, crate::provider::CODEX_USAGE_PATH);
    assert_eq!(seen.header("authorization"), Some("Bearer client-own"));
    assert_ne!(seen.header("authorization"), Some("Bearer at-bob"));

    let (status, _) = drive(
        Arc::clone(&manager),
        Method::GET,
        crate::provider::CODEX_USAGE_PATH,
        Some(loopback_peer()),
        &[],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        codex_echo.last_request().header("authorization"),
        None,
        "no bearer in, no bearer out: a pooled one is never put on a Codex relay"
    );
    assert_eq!(codex_echo.request_count(), 2);
    assert_eq!(
        anthropic_echo.request_count(),
        0,
        "a Codex relay goes to the Codex origin"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[1].requests,
        0,
        "a relay spends nothing the rotation owns"
    );
}

/// Every host tcr intercepts names the provider whose requests it carries, and no
/// other host does. A host added to [`crate::mitm::ALLOWED_HOSTS`] without a provider
/// here would be refused outright by `handle`'s host-and-path check (it fails closed),
/// so this fails first, at the table. Loopback names no provider: base-URL mode keeps
/// the path's.
///
/// Watched red: drop the `platform.claude.com` row and the first loop names it.
#[test]
fn every_intercepted_host_names_its_provider() {
    use crate::provider::Provider;
    for host in crate::mitm::ALLOWED_HOSTS {
        assert!(
            intercepted_host_provider(host).is_some(),
            "{host} is intercepted but names no provider"
        );
    }
    for (host, _) in INTERCEPTED_HOST_PROVIDERS {
        assert!(
            crate::mitm::host_allowed(host),
            "{host} names a provider but is not intercepted"
        );
    }
    assert_eq!(
        intercepted_host_provider("chatgpt.com"),
        Some(Provider::Codex)
    );
    assert_eq!(
        intercepted_host_provider("api.anthropic.com"),
        Some(Provider::Anthropic)
    );
    assert_eq!(
        intercepted_host_provider("platform.claude.com"),
        Some(Provider::Anthropic)
    );
    assert_eq!(
        intercepted_host_provider("ChatGPT.COM"),
        Some(Provider::Codex),
        "case-insensitive, like the allowlist"
    );
    for other in [
        "127.0.0.1",
        "localhost",
        "auth.openai.com",
        "chatgpt.com.evil",
    ] {
        assert_eq!(intercepted_host_provider(other), None, "{other}");
    }
}

/// **An intercepted `chatgpt.com` request outside `/backend-api/` is refused, and
/// reaches no origin.** Its path alone would classify it Anthropic, and the host guard
/// lets chatgpt.com through, so before this refusal it was rewritten to Anthropic's
/// origin: measured on the code without it, `GET /api/oauth/profile` with `Host:
/// chatgpt.com` and a ChatGPT bearer was relayed to the Anthropic origin carrying that
/// bearer (200), and `POST /v1/messages` was served by the Anthropic pool on alice's
/// token. Now both are answered 421, naming the host and the path; neither origin sees
/// a byte and no account is selected.
///
/// Watched red: drop the host-and-path check from `handle` and the first request is
/// relayed (200, not 421).
#[tokio::test]
async fn a_chatgpt_com_request_outside_backend_api_is_refused_and_reaches_no_origin() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    // The second spelling carries a port and capitals: the host is compared as the
    // allowlist compares it, port stripped and case ignored.
    for (method, host, path, headers) in [
        (
            Method::GET,
            "chatgpt.com",
            "/api/oauth/profile",
            vec![("authorization", "Bearer chatgpt-client-token")],
        ),
        (
            Method::POST,
            "CHATGPT.com:443",
            "/v1/messages",
            vec![("content-type", "application/json")],
        ),
    ] {
        let mut headers = headers;
        headers.push(("host", host));
        let (status, bytes) = drive(
            Arc::clone(&manager),
            method,
            path,
            Some(loopback_peer()),
            &headers,
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{host}{path}");
        let body: Value = serde_json::from_slice(&bytes).expect("a JSON error");
        let message = body["error"]["message"].as_str().expect("a message");
        assert!(
            message.contains(&format!("{}{path}", strip_port(host))),
            "the refusal names the host and the path: {message}"
        );
    }
    assert_eq!(
        anthropic_echo.request_count(),
        0,
        "nothing reached Anthropic"
    );
    assert_eq!(codex_echo.request_count(), 0, "nor Codex");
    for idx in 0..manager.account_count() {
        assert_eq!(
            manager.last_selected_seq(idx),
            Some(0),
            "refused before any account is selected (account {idx})"
        );
    }
}

/// **The mirror: an Anthropic host with a `/backend-api/` path is refused too.** The path
/// alone would classify it Codex, so before this refusal `GET /backend-api/wham/usage`
/// with `Host: api.anthropic.com` and a Claude bearer was relayed to the Codex origin
/// carrying that bearer (measured on the code without it), and a
/// `platform.claude.com` inference call was served by the Codex pool. Both are answered
/// 421 now, and neither origin sees a byte.
///
/// Watched red: drop the host-and-path check from `handle` and the first request is
/// relayed to Codex (200, not 421).
#[tokio::test]
async fn an_anthropic_host_with_a_backend_api_path_is_refused_and_reaches_no_origin() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    for (method, host, path, headers) in [
        (
            Method::GET,
            "api.anthropic.com",
            crate::provider::CODEX_USAGE_PATH,
            vec![("authorization", "Bearer claude-client-token")],
        ),
        (
            Method::POST,
            "platform.claude.com",
            crate::provider::CODEX_RESPONSES_PATH,
            vec![("content-type", "application/json")],
        ),
    ] {
        let mut headers = headers;
        headers.push(("host", host));
        let (status, bytes) = drive(
            Arc::clone(&manager),
            method,
            path,
            Some(loopback_peer()),
            &headers,
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{host}{path}");
        let body: Value = serde_json::from_slice(&bytes).expect("a JSON error");
        let message = body["error"]["message"].as_str().expect("a message");
        assert!(
            message.contains(&format!("{host}{path}")),
            "the refusal names the host and the path: {message}"
        );
    }
    assert_eq!(codex_echo.request_count(), 0, "nothing reached Codex");
    assert_eq!(anthropic_echo.request_count(), 0, "nor Anthropic");
    for idx in 0..manager.account_count() {
        assert_eq!(
            manager.last_selected_seq(idx),
            Some(0),
            "refused before any account is selected (account {idx})"
        );
    }
}

/// A request with no intercepted host keeps the path's provider: a loopback `Host`
/// (base-URL mode), or none at all. `/backend-api/wham/usage` still reaches the Codex
/// origin and `/v1/messages` the Anthropic one, so the refusal above is about
/// intercepted hosts only.
///
/// Watched red: drop `!host_is_loopback(host)` from the host-and-path check and the
/// first loopback request is answered 421.
#[tokio::test]
async fn a_loopback_or_hostless_request_keeps_its_paths_provider() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let manager =
        Manager::with_live_refresher(codex_fleet(&anthropic_echo.url, &codex_echo.url), None);
    for host in [
        Some("127.0.0.1:3456"),
        Some("localhost"),
        Some("[::1]:3456"),
        None,
    ] {
        let mut headers = vec![("authorization", "Bearer client-own")];
        headers.extend(host.map(|host| ("host", host)));
        let (status, _) = drive(
            Arc::clone(&manager),
            Method::GET,
            crate::provider::CODEX_USAGE_PATH,
            Some(loopback_peer()),
            &headers,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{host:?}");
        let mut headers = vec![("content-type", "application/json")];
        headers.extend(host.map(|host| ("host", host)));
        let (status, _) = drive(
            Arc::clone(&manager),
            Method::POST,
            "/v1/messages",
            Some(loopback_peer()),
            &headers,
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{host:?}");
    }
    assert_eq!(
        codex_echo.request_count(),
        4,
        "every usage call reached Codex"
    );
    assert_eq!(
        anthropic_echo.request_count(),
        4,
        "every /v1/messages reached Anthropic"
    );
}

/// No pooled Codex account can serve, the backend having refused the only one
/// at its limit and tcr holding it out: the client gets the 429 shape Codex
/// renders as a usage limit, and no origin is asked.
#[tokio::test]
async fn codex_exhausted_pool_answers_a_codex_shaped_429() {
    let anthropic_echo = CapturingUpstream::spawn().await;
    let codex_echo = CapturingUpstream::spawn().await;
    let config = codex_fleet(&anthropic_echo.url, &codex_echo.url);
    let manager = Manager::with_live_refresher(config, None);
    manager.edit_quota_for_test(1, |quota| quota.status = Some("rejected".to_string()));
    manager.mark_rate_limited(1, 3_600);
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
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        resp.headers().contains_key("retry-after"),
        "the retry-after logic is kept"
    );
    let bytes = to_bytes(resp.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "usage_limit_reached");
    assert_eq!(body["error"]["code"], "usage_limit_reached");
    assert_eq!(codex_echo.request_count(), 0);
    assert_eq!(anthropic_echo.request_count(), 0);
}
