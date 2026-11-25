//! Requests relayed past rotation: which paths relay, whose credential they carry, and the local routes that never reach upstream.

use super::*;

// --- rotation-bypassing relays -----------------------------------------

/// What a fake upstream saw — one of these per request it received.
#[derive(serde::Serialize, serde::Deserialize)]
struct Echo {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Echo {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Spawn a fake upstream that echoes every request back as JSON and counts hits.
///
/// A real upstream is what makes these tests measure the thing that matters:
/// what the proxy PUT ON THE WIRE. Asserting on the header-building helpers
/// alone would prove only that the helpers work, not that the relay path calls
/// them — the exact gap the bug lived in. The hit counter is the second half:
/// it proves a locally-answered path reached NO upstream at all.
async fn spawn_echo_upstream() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let upstream = Router::new().fallback(move |req: Request| {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let (parts, body) = req.into_parts();
            let bytes = to_bytes(body, MAX_BODY_BYTES).await.unwrap_or_default();
            axum::Json(Echo {
                method: parts.method.to_string(),
                path: parts
                    .uri
                    .path_and_query()
                    .map_or_default(|pq| pq.as_str().to_string()),
                headers: parts
                    .headers
                    .iter()
                    .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect(),
                body: String::from_utf8_lossy(&bytes).to_string(),
            })
        }
    });
    (stub::http(upstream).url(), hits)
}

#[tokio::test]
async fn reset_route_is_local_and_applies() {
    let origin =
        stub::http(Router::new().fallback(|| async {
            axum::Json(serde_json::json!({"code":"reset","windows_reset":1}))
        }));
    let manager = Manager::with_live_refresher(codex_fleet(&origin.url(), &origin.url()), None);
    let response = drive_full(
        manager,
        Method::POST,
        "/_tcr/codex/reset",
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"query":"bob@example.com/codex","idempotencyKey":"synthetic-attempt"}"#,
    )
    .await;
    assert_eq!(
        response
            .headers()
            .get(ENDPOINT_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("codex-reset"),
        "reset must be answered by its local route"
    );
}

#[tokio::test]
async fn reset_route_refuses_every_gate_method_and_body_violation() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let origin = stub::http(Router::new().fallback(move || {
        let seen = Arc::clone(&seen);
        async move {
            seen.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({"code":"reset","windows_reset":1}))
        }
    }));
    let mut config = codex_fleet(&origin.url(), &origin.url());
    config.proxy.api_key = Some("fake-local-key".into());
    let manager = Manager::with_live_refresher(config, None);
    let good = r#"{"query":"bob@example.com/codex","idempotencyKey":"synthetic-attempt"}"#;
    let json = [
        ("content-type", "application/json"),
        ("x-api-key", "fake-local-key"),
    ];
    for (peer, headers, status) in [
        (None, json.to_vec(), StatusCode::FORBIDDEN),
        (
            Some("192.0.2.1:1234".parse().unwrap()),
            json.to_vec(),
            StatusCode::FORBIDDEN,
        ),
        (
            Some(loopback_peer()),
            vec![("content-type", "application/json")],
            StatusCode::UNAUTHORIZED,
        ),
        (
            Some(loopback_peer()),
            vec![("content-type", "application/json"), ("x-api-key", "wrong")],
            StatusCode::UNAUTHORIZED,
        ),
        (
            Some(loopback_peer()),
            vec![
                ("content-type", "application/json"),
                ("x-api-key", "fake-local-key"),
                ("host", "evil.example"),
            ],
            StatusCode::MISDIRECTED_REQUEST,
        ),
        (
            Some(loopback_peer()),
            vec![
                ("content-type", "application/json"),
                ("x-api-key", "fake-local-key"),
                ("origin", "https://evil.example"),
            ],
            StatusCode::FORBIDDEN,
        ),
        (
            Some(loopback_peer()),
            vec![
                ("content-type", "application/json"),
                ("x-api-key", "fake-local-key"),
                ("sec-fetch-site", "cross-site"),
            ],
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = drive_full(
            Arc::clone(&manager),
            Method::POST,
            CODEX_RESET_PATH,
            peer,
            &headers,
            good,
        )
        .await;
        assert_eq!(
            response.status(),
            status,
            "reset must retain every local endpoint gate"
        );
    }
    for method in [
        Method::GET,
        Method::HEAD,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ] {
        let response = drive_full(
            Arc::clone(&manager),
            method,
            CODEX_RESET_PATH,
            Some(loopback_peer()),
            &json,
            good,
        )
        .await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response.headers().get(ENDPOINT_HEADER).unwrap(),
            "codex-reset"
        );
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    }
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let mut headers = vec![("x-api-key", "fake-local-key")];
        if let Some(value) = content_type {
            headers.push(("content-type", value));
        }
        let response = drive_full(
            Arc::clone(&manager),
            Method::POST,
            CODEX_RESET_PATH,
            Some(loopback_peer()),
            &headers,
            good,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    for body in ["null".to_string(),"[]".into(),"{}".into(),"false".into(),"not json".into(),
            r#"["bob@example.com/codex","synthetic-attempt"]"#.into(),
            r#"{"query":1,"idempotencyKey":"synthetic-attempt"}"#.into(),
            r#"{"query":"bob@example.com/codex","idempotencyKey":null}"#.into(),
            r#"{"query":"bob@example.com/codex","idempotencyKey":"","creditId":"fake-secret-sentinel"}"#.into(),
            r#"{"query":"bob@example.com/codex","idempotencyKey":"synthetic-attempt","extra":true}"#.into(),
            serde_json::json!({"query":"bob@example.com/codex","idempotencyKey":"a".repeat(129)}).to_string(),
            serde_json::json!({"query":"bob@example.com/codex","idempotencyKey":"bad key"}).to_string(),
            serde_json::json!({"query":"bob@example.com/codex","idempotencyKey":"ñ"}).to_string(),
            serde_json::json!({"query":"bob@example.com/codex","idempotencyKey":"a".repeat(9000)}).to_string(),
        ] {
            let response=drive_full(Arc::clone(&manager),Method::POST,CODEX_RESET_PATH,Some(loopback_peer()),&json,&body).await;
            assert_eq!(response.status(),StatusCode::BAD_REQUEST);
            let body=to_bytes(response.into_body(),MAX_BODY_BYTES).await.unwrap();
            assert!(!String::from_utf8_lossy(&body).contains("fake-secret-sentinel"));
        }
    for invalid in [
        Value::Null,
        serde_json::json!(true),
        serde_json::json!(12),
        serde_json::json!([]),
        serde_json::json!({}),
        serde_json::json!(""),
    ] {
        for field in ["query", "idempotencyKey"] {
            let mut body: Value = serde_json::from_str(good).unwrap();
            body[field] = invalid.clone();
            assert_eq!(
                drive_full(
                    Arc::clone(&manager),
                    Method::POST,
                    CODEX_RESET_PATH,
                    Some(loopback_peer()),
                    &json,
                    &body.to_string()
                )
                .await
                .status(),
                StatusCode::BAD_REQUEST
            );
        }
    }
    for (query, status) in [
        ("bob", StatusCode::NOT_FOUND),
        ("alice@example.com", StatusCode::BAD_REQUEST),
    ] {
        let body =
            serde_json::json!({"query":query,"idempotencyKey":"synthetic-attempt"}).to_string();
        assert_eq!(
            drive_full(
                Arc::clone(&manager),
                Method::POST,
                CODEX_RESET_PATH,
                Some(loopback_peer()),
                &json,
                &body
            )
            .await
            .status(),
            status
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "refusals must not reach any backend"
    );
    let response = drive_full(
        manager,
        Method::POST,
        CODEX_RESET_PATH,
        Some(loopback_peer()),
        &json,
        good,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(ENDPOINT_HEADER).unwrap(),
        "codex-reset"
    );
    let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        serde_json::json!({"outcome":"applied","usageRefreshed":true})
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "positive control: consume then usage"
    );
}

fn parse_echo(bytes: &Bytes) -> Echo {
    serde_json::from_slice(bytes).unwrap_or_else(|err| {
        panic!(
            "an echo payload ({err}): {}",
            String::from_utf8_lossy(bytes)
        )
    })
}

/// The classifier, in isolation. The negatives matter as much as the positives:
/// `/v1/code/` is a prefix WITH its slash so `/v1/codex` is not a Remote Control
/// path, and the token refresh is POST-only so a GET on it is ordinary traffic.
#[test]
fn every_codex_model_call_uses_the_pool() {
    for path in [
        "/backend-api/codex/responses",
        "/backend-api/codex/images/generations",
        "/backend-api/codex/images/edits",
        "/backend-api/codex/memories/trace_summarize",
        "/backend-api/codex/alpha/search",
    ] {
        for bearer in [false, true] {
            assert_eq!(relay_mode(&Method::POST, path, bearer), None, "{path}");
        }
        assert!(Provider::Codex.is_inference_path(path), "{path}");
        let descendant = format!("{path}/other");
        assert_eq!(
            relay_mode(&Method::POST, &descendant, true),
            Some(RelayMode::ClientCredential)
        );
    }
    for path in [
        "/backend-api/wham/usage",
        "/backend-api/codex/models",
        "/backend-api/codex/memories",
    ] {
        assert_eq!(
            relay_mode(&Method::GET, path, true),
            Some(RelayMode::ClientCredential)
        );
    }
}

#[test]
fn relay_mode_classifies_only_the_bypass_paths() {
    for path in [
        "/v1/code/",
        "/v1/code/session/abc",
        "/api/oauth/files/file_0123",
        "/api/oauth/file_upload",
        "/v1/mcp_servers",
        "/v1/sessions/session_abc/archive",
    ] {
        assert_eq!(
            relay_mode(&Method::POST, path, false),
            Some(RelayMode::ClientCredential),
            "{path} is client-credential"
        );
        assert_eq!(
            relay_mode(&Method::GET, path, false),
            Some(RelayMode::ClientCredential),
            "{path} is client-credential on GET too"
        );
    }
    assert_eq!(
        relay_mode(&Method::POST, CLIENT_TOKEN_REFRESH_PATH, false),
        Some(RelayMode::Raw)
    );
    for (method, path) in [
        (Method::GET, "/v1/oauth/token"), // refresh is POST-only
        (Method::POST, "/v1/messages"),
        (Method::GET, "/v1/models"),
        (Method::GET, "/v1/codex"), // NOT under /v1/code/
        (Method::GET, "/"),
    ] {
        assert_eq!(
            relay_mode(&method, path, false),
            None,
            "{method} {path} must take the normal rotation path"
        );
    }
}

/// Claude Code's org-scoped calls name the CLIENT's own organization in the path, so
/// only the client's own credential can answer them. Rotated onto a pooled account
/// from another org they 401/403, and every 401 force-refreshes that account — the
/// loop that condemned six healthy accounts in 15 minutes (see
/// [`CLIENT_CREDENTIAL_PREFIXES`]). The uuid is fake.
#[test]
fn org_scoped_oauth_paths_relay_with_the_client_credential() {
    const ORG: &str = "11111111-2222-3333-4444-555555555555";
    for path in [
        format!("/api/oauth/organizations/{ORG}/marketplaces"),
        format!("/api/oauth/organizations/{ORG}/plugins/list-plugins"),
        format!("/api/oauth/organizations/{ORG}/skills/skill_0123/download"),
        "/api/oauth/organizations".to_string(),
    ] {
        assert_eq!(
            relay_mode(&Method::GET, &path, false),
            Some(RelayMode::ClientCredential),
            "{path} is bound to the client's own org"
        );
    }
    assert_eq!(
        relay_mode(&Method::GET, "/api/oauth/organizations_v2", false),
        None,
        "a longer identifier sharing the prefix is a different route"
    );
}

/// A request carrying the client's own bearer is relayed under it on every
/// path that is not inference — the bookkeeping calls are questions about the
/// CLIENT's identity, and a pooled account answers them for someone else. The
/// two inference paths stay pooled whatever credential arrives: they are the
/// quota the proxy exists to rotate.
#[test]
fn a_client_bearer_relays_every_non_inference_path() {
    for (method, path) in [
        (Method::GET, "/api/claude_cli/bootstrap"),
        (Method::GET, "/api/oauth/profile"),
        (Method::GET, "/api/oauth/account/settings"),
        (
            Method::GET,
            "/api/organizations/11111111-2222-3333-4444-555555555555/model_selector/cc",
        ),
        (Method::GET, "/v1/ultrareview/quota"),
        (Method::GET, "/mcp-registry/v0/servers"),
        (Method::POST, "/api/event_logging/v2/batch"),
        (Method::GET, "/v1/models"),
        (Method::GET, "/"),
    ] {
        assert_eq!(
            relay_mode(&method, path, true),
            Some(RelayMode::ClientCredential),
            "{method} {path} with a client bearer is the client's own call"
        );
        assert_eq!(
            relay_mode(&method, path, false),
            None,
            "{method} {path} without a bearer still takes the pooled path"
        );
    }
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        assert_eq!(
            relay_mode(&Method::POST, path, true),
            None,
            "{path} spends quota and is pooled even with a client bearer"
        );
    }
    assert_eq!(
        relay_mode(&Method::POST, CLIENT_TOKEN_REFRESH_PATH, true),
        Some(RelayMode::Raw),
        "a bearer does not turn the token refresh into a credentialled relay"
    );
}

#[test]
fn client_bearer_of_reads_the_scheme_case_insensitively_and_rejects_empty() {
    let mut h = HeaderMap::new();
    assert_eq!(client_bearer_of(&h), None);
    h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer tok-1"));
    assert_eq!(client_bearer_of(&h), Some("tok-1"));
    h.insert(AUTHORIZATION, HeaderValue::from_static("bearer   tok-2  "));
    assert_eq!(client_bearer_of(&h), Some("tok-2"));
    h.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Basic dXNlcjpwdw=="),
    );
    assert_eq!(client_bearer_of(&h), None, "not a bearer");
    h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
    assert_eq!(client_bearer_of(&h), None, "an empty token is no bearer");
}

/// The `controlIdentity` check end to end, through `app()` against a fake
/// upstream: the client's bearer is resolved via `/api/oauth/profile` and
/// compared to the control account. A foreign bearer is refused on a
/// bookkeeping call AND on inference, before anything reaches upstream; the
/// control account's own bearer passes, and its bookkeeping call goes out
/// under THAT bearer (the relay rule), not a pooled one. `warn` serves,
/// `off` never asks, no bearer never asks, and a profile fetch that fails is
/// fail-open. Identities are fake.
#[tokio::test]
async fn control_identity_refuses_a_client_signed_in_as_another_account() {
    use axum::extract::State;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt as _;

    const ALICE_UUID: &str = "11111111-1111-1111-1111-111111111111";
    const BOB_UUID: &str = "22222222-2222-2222-2222-222222222222";

    struct NoRefresh;
    impl crate::clients::oauth::TokenRefresher for NoRefresh {
        fn refresh(&self, _provider: Provider, _t: String) -> crate::clients::oauth::RefreshFuture {
            Box::pin(async {
                Err(crate::clients::oauth::OAuthError::Transient(
                    "unused".into(),
                ))
            })
        }
    }

    #[derive(Clone, Default)]
    struct Seen {
        profile_hits: Arc<AtomicUsize>,
        messages_hits: Arc<AtomicUsize>,
        bootstrap_auth: Arc<std::sync::Mutex<Vec<String>>>,
    }
    async fn profile(State(s): State<Seen>, headers: HeaderMap) -> Response {
        use axum::response::IntoResponse as _;
        s.profile_hits.fetch_add(1, Ordering::SeqCst);
        let (email, uuid) = match client_bearer_of(&headers) {
            Some("tok-alice") => ("alice@example.com", ALICE_UUID),
            Some("tok-bob") => ("bob@example.com", BOB_UUID),
            Some("tok-dead") => return StatusCode::UNAUTHORIZED.into_response(),
            _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        Json(serde_json::json!({
            "account": {"uuid": uuid, "email": email},
            "organization": {"uuid": "33333333-3333-3333-3333-333333333333", "name": "acme-corp"}
        }))
        .into_response()
    }
    async fn messages(State(s): State<Seen>) -> Json<Value> {
        s.messages_hits.fetch_add(1, Ordering::SeqCst);
        Json(
            serde_json::json!({"usage": {"input_tokens": 1, "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0, "output_tokens": 1}}),
        )
    }
    async fn bootstrap(State(s): State<Seen>, headers: HeaderMap) -> Json<Value> {
        let auth = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>")
            .to_string();
        s.bootstrap_auth.lock().unwrap().push(auth);
        Json(serde_json::json!({}))
    }

    let seen = Seen::default();
    let up_addr = stub::http(
        Router::new()
            .route("/api/oauth/profile", get(profile))
            .route("/v1/messages", post(messages))
            .route("/api/claude_cli/bootstrap", get(bootstrap))
            .with_state(seen.clone()),
    );

    let manager_with = |mode: config::ControlIdentityMode| {
        let mut config = dummy_config(None, &format!("http://{up_addr}"));
        config.accounts[0].name = "alice@example.com".to_string();
        config.accounts[0].account_uuid = Some(ALICE_UUID.to_string());
        config.control_account = Some(config::ControlAccountRef::Name(
            "alice@example.com".to_string(),
        ));
        config.control_pooled = true;
        config.control_identity = mode;
        Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(crate::records::probe::LiveUsageProber::new()),
            Arc::new(crate::clients::warmer::LiveWarmer::new()),
            None,
        )
    };
    let request = |method: Method, uri: &str, bearer: Option<&str>| {
        let mut builder = Request::builder()
            .method(method.clone())
            .uri(uri)
            .header(CONTENT_TYPE, "application/json");
        if let Some(token) = bearer {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = if method == Method::POST {
            Body::from(r#"{"system":"hi","messages":[]}"#)
        } else {
            Body::empty()
        };
        let mut req = builder.body(body).expect("build request");
        req.extensions_mut().insert(SessionKey(0xF00D));
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        req
    };
    async fn body_text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    // refuse: bob is refused on a bookkeeping call and on inference, and
    // neither reaches upstream.
    let manager = manager_with(config::ControlIdentityMode::Refuse);
    let response = app(Arc::clone(&manager))
        .oneshot(request(
            Method::GET,
            "/api/claude_cli/bootstrap",
            Some("tok-bob"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let text = body_text(response).await;
    assert!(
        text.contains("bob@example.com") && text.contains("alice@example.com"),
        "the refusal names both identities: {text}"
    );
    assert!(
        text.contains("/logout"),
        "the refusal names the fix: {text}"
    );
    let response = app(Arc::clone(&manager))
        .oneshot(request(
            Method::POST,
            "/v1/messages?beta=true",
            Some("tok-bob"),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "inference is refused too"
    );
    assert_eq!(seen.messages_hits.load(Ordering::SeqCst), 0);
    assert!(seen.bootstrap_auth.lock().unwrap().is_empty());
    assert_eq!(
        seen.profile_hits.load(Ordering::SeqCst),
        1,
        "one profile fetch for two requests on one bearer"
    );

    // The control account's own bearer passes, and its bootstrap call is
    // relayed under that bearer rather than the pooled token.
    let response = app(Arc::clone(&manager))
        .oneshot(request(
            Method::GET,
            "/api/claude_cli/bootstrap",
            Some("tok-alice"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        seen.bootstrap_auth.lock().unwrap().as_slice(),
        ["Bearer tok-alice".to_string()],
        "a bookkeeping call carries the CLIENT's credential upstream"
    );
    let response = app(Arc::clone(&manager))
        .oneshot(request(
            Method::POST,
            "/v1/messages?beta=true",
            Some("tok-alice"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(seen.messages_hits.load(Ordering::SeqCst), 1);
    assert_eq!(seen.profile_hits.load(Ordering::SeqCst), 2);

    // A bearer the profile endpoint cannot resolve is fail-open, and is
    // asked again next time because a failure is never cached.
    for _ in 0..2 {
        let response = app(Arc::clone(&manager))
            .oneshot(request(
                Method::POST,
                "/v1/messages?beta=true",
                Some("tok-broken"),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "an unresolved identity is served"
        );
    }
    assert_eq!(seen.profile_hits.load(Ordering::SeqCst), 4);

    // A bearer upstream REFUSES (expired or revoked) is served too, but is
    // remembered: two requests on it cost one profile fetch, not two.
    for _ in 0..2 {
        let response = app(Arc::clone(&manager))
            .oneshot(request(
                Method::POST,
                "/v1/messages?beta=true",
                Some("tok-dead"),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a refused bearer is fail-open"
        );
    }
    assert_eq!(
        seen.profile_hits.load(Ordering::SeqCst),
        5,
        "a 401 on the profile endpoint is cached, a 500 is not"
    );

    // No bearer: nothing to check, nothing asked.
    let response = app(Arc::clone(&manager))
        .oneshot(request(Method::POST, "/v1/messages?beta=true", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(seen.profile_hits.load(Ordering::SeqCst), 5);

    // warn: bob is served.
    let manager = manager_with(config::ControlIdentityMode::Warn);
    let response = app(manager)
        .oneshot(request(
            Method::POST,
            "/v1/messages?beta=true",
            Some("tok-bob"),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "warn serves a foreign client"
    );
    assert_eq!(seen.profile_hits.load(Ordering::SeqCst), 6);

    // off: bob is served and the profile endpoint is never asked.
    let manager = manager_with(config::ControlIdentityMode::Off);
    let response = app(manager)
        .oneshot(request(
            Method::POST,
            "/v1/messages?beta=true",
            Some("tok-bob"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        seen.profile_hits.load(Ordering::SeqCst),
        6,
        "off never resolves"
    );
}

/// The segment-boundary rule, at both edges. Every entry that reads as a prefix
/// in this module is really "this path, or something under it" — a longer
/// IDENTIFIER sharing the same leading characters is a different route.
#[test]
fn relay_mode_matches_whole_segments_at_both_edges() {
    // Bare, with no trailing slash: a real route, previously missed by `/v1/code/`.
    for path in [
        "/v1/code",
        "/api/oauth/files",
        "/api/oauth/file_upload",
        "/v1/mcp_servers",
        "/v1/sessions",
    ] {
        assert_eq!(
            relay_mode(&Method::POST, path, false),
            Some(RelayMode::ClientCredential),
            "the bare {path} is the same route as {path}/"
        );
    }
    assert_eq!(
        relay_mode(&Method::POST, "/v1/sessions/session_abc/archive", false),
        Some(RelayMode::ClientCredential),
        "a session sub-path is still client-credential"
    );
    // Longer identifiers: NOT the route, previously swallowed by the entry that
    // carried no terminator.
    for path in [
        "/v1/codex",
        "/api/oauth/file_upload_v2",
        "/api/oauth/file_uploadX",
        "/api/oauth/filesystem",
        "/v1/sessions_v2",
    ] {
        assert_eq!(
            relay_mode(&Method::POST, path, false),
            None,
            "{path} only shares a prefix — it is not the route"
        );
    }
    // The token refresh takes the same rule. `/v1/oauth/token/` used to miss the
    // exact compare and fall through to the POOLED path, which is the one outcome
    // a client's own credential exchange must never have.
    for path in ["/v1/oauth/token", "/v1/oauth/token/"] {
        assert_eq!(
            relay_mode(&Method::POST, path, false),
            Some(RelayMode::Raw),
            "{path} is the client's own token exchange"
        );
    }
    assert_eq!(
        relay_mode(&Method::POST, "/v1/oauth/tokens", false),
        None,
        "a longer identifier is not the token endpoint"
    );
}

/// Pins the claude.ai connector list onto the client's own credential.
#[test]
fn the_connector_list_never_takes_a_pooled_token() {
    for method in [Method::GET, Method::POST] {
        assert_eq!(
            relay_mode(&method, "/v1/mcp_servers", false),
            Some(RelayMode::ClientCredential),
            "{method} /v1/mcp_servers must not be re-credentialled"
        );
    }
    assert_eq!(
        relay_mode(&Method::GET, "/v1/mcp_servers/srv_0123", false),
        Some(RelayMode::ClientCredential),
        "one connector is the same route as the collection"
    );
    for path in ["/v1/mcp_servers_v2", "/v1/mcp_serversX"] {
        assert_eq!(
            relay_mode(&Method::GET, path, false),
            None,
            "{path} only shares a prefix — it is not the route"
        );
    }
}

/// `path_is_under` in isolation — the single rule every prefix decision uses.
#[test]
fn path_is_under_matches_only_whole_segments() {
    for (path, base, want) in [
        ("/_tcr", "/_tcr", true),
        ("/_tcr/", "/_tcr", true),
        ("/_tcr/status", "/_tcr", true),
        ("/_tcrx", "/_tcr", false),
        ("/_tc", "/_tcr", false),
        ("/v1/code/session/abc", "/v1/code", true),
        ("", "/_tcr", false),
    ] {
        assert_eq!(
            path_is_under(path, base),
            want,
            "path_is_under({path:?}, {base:?})"
        );
    }
}

/// Dot-segment recognition, in every spelling the upstream URL parser folds —
/// and, as importantly, NOT in the spellings it leaves alone. Over-rejecting a
/// legitimate path is a real failure mode, not a safe default.
#[test]
fn dot_segments_are_recognised_in_every_spelling() {
    // Single-dot (`.`, `%2e`) and double-dot (`..` and its three mixed
    // spellings) segments — the parser folds `%2e`/`%2E` before classifying.
    for segment in [".", "%2e", "%2E", "..", "%2e%2e", "%2E%2E", ".%2e", "%2e."] {
        assert!(is_dot_segment(segment), "{segment} is a dot segment");
    }
    for segment in [
        "",
        "...",       // three dots is an ordinary name
        "%2e%2e%2e", // …in any spelling
        "a.json",    // a literal dot INSIDE a name
        "..a",
        "a..",
        "..%2f..", // `%2f` is not decoded, so this is one opaque segment
        "%2f",
        "%25 2e",
        "file_upload",
    ] {
        assert!(!is_dot_segment(segment), "{segment} is NOT a dot segment");
    }
}

/// The whole-path guard: which request targets disagree with what reqwest will
/// put on the wire. The negatives are the half that keeps the guard honest.
#[test]
fn path_is_ambiguous_flags_traversal_and_leaves_ordinary_paths_alone() {
    for path in [
        "/v1/code/../../v1/messages",
        "/v1/code/%2e%2e/%2e%2e/v1/messages",
        "/v1/code/../../_tcr/status",
        "/x/../_tcr/status",
        "/v1/./messages",
        "/v1/code/.%2e/../v1/messages",
        "/..",
        "/v1/code/..\\../v1/messages", // `\` is a WHATWG path separator
        "/v1/code\\foo",               // …so this lands on /v1/code/foo upstream
    ] {
        assert!(path_is_ambiguous(path), "{path} must be refused");
    }
    for path in [
        "/",
        "/v1/messages",
        "/v1/models",
        "/api/oauth/files/a.json",       // a literal dot in a filename
        "/api/oauth/files/...",          // three dots
        "/api/oauth/files/..%2f..",      // `%2f` is not decoded by the parser
        "/api/oauth/files/%5c..",        // nor is `%5c`
        "/v1/organizations/x.y.z/usage", // dots inside segments
        "/_tcr/status",
    ] {
        assert!(!path_is_ambiguous(path), "{path} is legitimate");
    }
}

/// Every client-credential prefix reaches upstream carrying the CLIENT's own
/// `authorization` — and never the pooled account Bearer, which is what 403'd
/// the Remote Control stream and silently dropped claude.ai attachments.
#[tokio::test]
async fn client_credential_paths_forward_the_clients_own_credential() {
    let (upstream, hits) = spawn_echo_upstream().await;
    let paths = [
        "/v1/code/session/abc",
        "/api/oauth/files/file_0123",
        "/api/oauth/file_upload",
        "/v1/mcp_servers",
    ];
    for path in paths {
        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let (status, body) = drive(
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
        assert_eq!(status, StatusCode::OK, "{path} was relayed");
        let echo = parse_echo(&body);
        assert_eq!(echo.path, path, "the path is forwarded verbatim");
        assert_eq!(
            echo.header("authorization"),
            Some("Bearer client-own-token"),
            "{path} must carry the client's OWN credential"
        );
        assert_ne!(
            echo.header("authorization"),
            Some("Bearer at-dummy"),
            "{path} must never carry the pooled account token"
        );
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        paths.len(),
        "one upstream hit per relayed path"
    );
}

/// The proxy's own gate credential never leaves this process. An `x-api-key`
/// equal to the configured proxy key is OUR secret and is stripped; a different
/// one is the client's own and passes through untouched.
#[tokio::test]
async fn client_credential_strips_only_our_own_proxy_key() {
    let (upstream, _hits) = spawn_echo_upstream().await;
    let config = || dummy_config(Some("sk-proxy-secret"), &upstream);

    let manager = Manager::with_live_refresher(config(), None);
    let (status, body) = drive(
        manager,
        Method::GET,
        "/api/oauth/files/file_0123",
        Some(loopback_peer()),
        &[("x-api-key", "sk-proxy-secret")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let echo = parse_echo(&body);
    assert_eq!(
        echo.header("x-api-key"),
        None,
        "our own proxy key must never reach Anthropic"
    );

    let manager = Manager::with_live_refresher(config(), None);
    let (status, body) = drive(
        manager,
        Method::GET,
        "/api/oauth/files/file_0123",
        Some(loopback_peer()),
        &[("x-api-key", "sk-the-clients-own")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let echo = parse_echo(&body);
    assert_eq!(
        echo.header("x-api-key"),
        Some("sk-the-clients-own"),
        "a key that is not ours is the client's credential and passes through"
    );
}

/// `POST /v1/oauth/token` is a credential exchange that is not ours: it goes up
/// with NO auth header in any form (its credentials are in the body, which is
/// forwarded byte-for-byte) and no header we did not explicitly choose.
#[tokio::test]
async fn token_refresh_relays_raw_with_no_auth_header() {
    let (upstream, _hits) = spawn_echo_upstream().await;
    let manager =
        Manager::with_live_refresher(dummy_config(Some("sk-proxy-secret"), &upstream), None);
    let payload = r#"{"grant_type":"refresh_token","refresh_token":"rt-client"}"#;
    let (status, body) = drive(
        manager,
        Method::POST,
        "/v1/oauth/token",
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own-token"),
            ("x-api-key", "sk-proxy-secret"),
            ("content-type", "application/json"),
            ("x-stainless-lang", "js"),
        ],
        payload,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let echo = parse_echo(&body);
    assert_eq!(echo.method, "POST");
    assert_eq!(echo.body, payload, "the refresh body is forwarded verbatim");
    assert_eq!(
        echo.header("authorization"),
        None,
        "a raw relay sends no auth header at all"
    );
    assert_eq!(echo.header("x-api-key"), None, "nor our gate credential");
    assert_eq!(echo.header("x-stainless-lang"), None, "nor anything else");
    assert_eq!(echo.header("content-type"), Some("application/json"));
}

/// THE BITING TEST: a relayed request spends none of the rotation's state, and
/// the `/v1/messages` control in the same test proves the assertion can fail —
/// the identical counters DO move when a request goes the pooled-credential way,
/// which is also the regression guard that this change left that path alone.
#[tokio::test]
async fn rotation_state_moves_for_messages_and_never_for_a_relay() {
    let (upstream, _hits) = spawn_echo_upstream().await;

    let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
    let before = manager.snapshot(OffsetDateTime::now_utc());
    for (method, path) in [
        (Method::POST, "/v1/code/session/abc"),
        (Method::GET, "/api/oauth/files/file_0123"),
        (Method::POST, "/api/oauth/file_upload"),
        (Method::POST, "/v1/oauth/token"),
    ] {
        let (status, _body) = drive(
            Arc::clone(&manager),
            method.clone(),
            path,
            Some(loopback_peer()),
            &[("authorization", "Bearer client-own-token")],
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{method} {path} was relayed");
    }
    let after = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        after.accounts[0].requests, before.accounts[0].requests,
        "a relayed request must not be counted against any account"
    );
    assert_eq!(
        after.accounts[0].last_used, before.accounts[0].last_used,
        "nor touch its LRU key"
    );
    assert_eq!(after.current, before.current, "nor move the cursor");
    assert_eq!(
        after.sessions.len(),
        before.sessions.len(),
        "nor write a pin"
    );

    // The control: the same counters on the same manager, for a request that
    // DOES go through rotation. It also re-asserts the pooled-credential
    // contract on `/v1/messages` — the client's authorization is replaced, not
    // forwarded.
    let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
    let (status, body) = drive(
        Arc::clone(&manager),
        Method::POST,
        "/v1/messages",
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own-token"),
            ("content-type", "application/json"),
        ],
        r#"{"model":"claude-sonnet-5"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let echo = parse_echo(&body);
    assert_eq!(
        echo.header("authorization"),
        Some("Bearer at-dummy"),
        "/v1/messages still carries the POOLED token"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
        1,
        "…and is counted against the serving account"
    );
}

/// Anything under the proxy's own prefix that is not a registered route is
/// answered LOCALLY. The hit counter is the assertion that matters: a fall-through
/// would have sent a proxy-private path to Anthropic with a pooled Bearer on it.
#[tokio::test]
async fn unknown_local_paths_are_answered_locally() {
    let (upstream, hits) = spawn_echo_upstream().await;
    for (method, uri) in [
        (Method::GET, "/_tcr/status-typo"),
        (Method::GET, "/_tcr/status/extra"),
        (Method::POST, "/_tcr/anything-else"),
        (Method::GET, "/_tcr/"),
        (Method::GET, "/_tcr"),
        (Method::GET, "/_tcr/status?x=1&y=2/../nope"),
        // Near-misses of the two account-control routes. The mutating verb
        // makes these the rows that matter most: a `POST` that fell through
        // the catch-all would be rewritten onto api.anthropic.com carrying a
        // pooled OAuth Bearer, which is the shape that burned an account.
        (Method::POST, "/_tcr/accounts/"),
        (Method::POST, "/_tcr/accounts/disable"),
        (Method::POST, "/_tcr/accounts/disabled/extra"),
        // `/_tcr/accounts/priority` itself is a route now (PRIORITY_PATH);
        // its near-misses stay here.
        (Method::POST, "/_tcr/accounts/priorities"),
        (Method::POST, "/_tcr/accounts/priority/extra"),
        // `/_tcr/config` is a route (CONFIG_PATH); its near-misses stay here.
        (Method::POST, "/_tcr/configs"),
        (Method::POST, "/_tcr/config/extra"),
        // The bare path IS registered (ADD_ACCOUNT_PATH) — a `GET` on it
        // gets a local 405, not a 404 like every other row here — but it
        // belongs in this table anyway for the assertion this table makes
        // and `add_endpoint_refuses_other_methods_locally` does not: the
        // hit counter, proving a wrong-verb request STILL never reaches
        // upstream.
        (Method::GET, "/_tcr/accounts"),
    ] {
        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let (status, body) = drive(
            Arc::clone(&manager),
            method.clone(),
            uri,
            Some(loopback_peer()),
            &[],
            "",
        )
        .await;
        let text = String::from_utf8_lossy(&body);
        // `/_tcr/status?…` IS a registered route (the query does not change the
        // path), so it is served; the bare add-account path is registered too,
        // wrong verb, so it is a local 405; every other shape is a local 404.
        // None of the three is ever forwarded, which is the single claim this
        // test makes.
        if uri.starts_with("/_tcr/status?") {
            assert_eq!(status, StatusCode::OK, "{uri} is the real status route");
        } else if uri == "/_tcr/accounts" {
            assert_eq!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {uri} → the route exists, wrong verb"
            );
        } else {
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri} → local 404");
            assert!(
                text.contains("not_found_error"),
                "in the standard error envelope: {text}"
            );
        }
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
            0,
            "{uri} spent no account"
        );
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "no `/_tcr/…` request may ever reach an upstream"
    );
}

/// `/_tcr/config` answers locally on every verb: a `GET` with the settings
/// this process runs, stamped as the route; a refused value with a stamped
/// 400 naming the key; a `PUT` with a stamped 405. None of them reaches the
/// upstream, and a refused value writes nothing (there is no file here).
#[tokio::test]
async fn the_config_route_answers_locally_and_stamps_every_answer() {
    let (upstream, hits) = spawn_echo_upstream().await;
    let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);

    let response = drive_full(
        Arc::clone(&manager),
        Method::GET,
        CONFIG_PATH,
        Some(loopback_peer()),
        &[],
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(ENDPOINT_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(CONFIG_ENDPOINT)
    );
    let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    let running: crate::base::config_show::ConfigSettings =
        serde_json::from_slice(&body).expect("the running settings");
    assert_eq!(running, manager.running_settings());

    let response = drive_full(
        Arc::clone(&manager),
        Method::POST,
        CONFIG_PATH,
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"key": "controlReserve", "value": "0.9"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get(ENDPOINT_HEADER).is_some());
    let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("controlReserve takes"), "{text}");

    let (status, _) = drive(
        Arc::clone(&manager),
        Method::PUT,
        CONFIG_PATH,
        Some(loopback_peer()),
        &[],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

fn stamp_of(response: &Response) -> Option<String> {
    response
        .headers()
        .get(ENDPOINT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// `/_tcr/fan`: POST only, the exact bodies of the contract, a stamped 400
/// in one sentence for a refused value, a stamped 202 carrying the object
/// from before the command under the command's number, and the new wish on
/// the status route, under that
/// number, once the controller acted on it. The controller behind it
/// reaches no SMC and no helper, so nothing here can touch a fan.
#[tokio::test]
async fn fan_route_takes_the_contract_bodies_and_answers_locally() {
    let (upstream, hits) = spawn_echo_upstream().await;
    let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
    let json = [("content-type", "application/json")];

    // No controller in this process: a stamped 503, never a forward.
    let response = drive_full(
        Arc::clone(&manager),
        Method::POST,
        FAN_PATH,
        Some(loopback_peer()),
        &json,
        r#"{"mode":"auto"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(stamp_of(&response).as_deref(), Some(FAN_ENDPOINT));

    manager.install_fan(crate::stores::fan::start_inert());

    let response = drive_full(
        Arc::clone(&manager),
        Method::POST,
        FAN_PATH,
        Some(loopback_peer()),
        &json,
        r#"{"mode":"fixed","rpm":5000}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(stamp_of(&response).as_deref(), Some(FAN_ENDPOINT));
    let accepted = body_json(response).await;
    let mine = accepted["command"]
        .as_u64()
        .unwrap_or_else(|| panic!("the 202 names the command's number: {accepted}"));
    // The rest is the object from before the command, as 1.2.12 answered:
    // the controller starts on Auto.
    assert_eq!(
        accepted["desired"],
        serde_json::json!({"mode": "auto"}),
        "{accepted}"
    );
    assert!(accepted["sinceMs"].is_i64(), "{accepted}");

    let response = drive_full(
        Arc::clone(&manager),
        Method::POST,
        FAN_PATH,
        Some(loopback_peer()),
        &json,
        r#"{"mode":"followCpu","quietBelowC":60,"fullAtC":101}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(stamp_of(&response).as_deref(), Some(FAN_ENDPOINT));
    let refused = body_json(response).await.to_string();
    assert!(refused.contains("above the hot guard"), "{refused}");

    for body in [r#"{"mode":"fixed","rpm":5000,"x":1}"#, r#"["fixed",5000]"#] {
        let response = drive_full(
            Arc::clone(&manager),
            Method::POST,
            FAN_PATH,
            Some(loopback_peer()),
            &json,
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }

    let (status, _) = drive(
        Arc::clone(&manager),
        Method::GET,
        FAN_PATH,
        Some(loopback_peer()),
        &[],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // Not loopback: refused by the shared gate, unstamped.
    let response = drive_full(
        Arc::clone(&manager),
        Method::POST,
        FAN_PATH,
        Some(remote_peer()),
        &json,
        r#"{"mode":"auto"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(stamp_of(&response), None);

    // The status route carries the same object. With no SMC the
    // controller reports `unavailable / noSmc`.
    let response = drive_full(
        Arc::clone(&manager),
        Method::GET,
        STATUS_PATH,
        Some(loopback_peer()),
        &[],
        "",
    )
    .await;
    let status = body_json(response).await;
    assert!(status["fanControl"].is_object(), "{status}");
    // The wish shows once the controller has acted on it, under the
    // number the 202 gave.
    let want = serde_json::json!({
        "desired": {"mode": "fixed", "rpm": 5000}, "reason": "noSmc", "command": mine
    });
    let mut seen = serde_json::json!({
        "desired": status["fanControl"]["desired"],
        "reason": status["fanControl"]["reason"],
        "command": status["fanControl"]["command"],
    });
    for _ in 0..50 {
        if seen == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        seen = manager.fan().map_or_default(|f| {
            let row = f.row();
            serde_json::json!({
                "desired": row.desired, "reason": row.reason, "command": row.command
            })
        });
    }
    assert_eq!(seen, want);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    if let Some(fan) = manager.fan() {
        let fan = Arc::clone(fan);
        tokio::task::spawn_blocking(move || fan.shutdown(Duration::from_secs(1)))
            .await
            .expect("the controller stops");
    }
}

/// The traversal table, end to end, against the fake upstream that reports what
/// it actually RECEIVED. Every row here was measured reaching an upstream before
/// the guard existed, and the received path is the proof that the classification
/// and the wire disagreed:
///
/// | raw request target                  | classified as | upstream received |
/// |-------------------------------------|---------------|-------------------|
/// | `/v1/code/../../v1/messages`        | RELAY         | `/v1/messages`    |
/// | `/v1/code/%2e%2e/%2e%2e/v1/messages`| RELAY         | `/v1/messages`    |
/// | `/v1/code/../../_tcr/status`        | RELAY         | `/_tcr/status`    |
/// | `/x/../_tcr/status`                 | pooled        | `/_tcr/status`    |
/// | `/v1/code\..\../v1/messages`        | RELAY         | `/v1/messages`    |
/// | `/v1/code\foo`                      | pooled        | `/v1/code/foo`    |
///
/// Rows 1-2 and 5 are a real `/v1/messages` routed as a relay: no `select`, no
/// in-flight slot, no throttle, no `record_served`, no pin, no retry ladder, and
/// the CLIENT's credential instead of a pooled one. Rows 3-4 defeat the `/_tcr/`
/// guard, row 4 putting a POOLED BEARER on `/_tcr/status` — the shape that burned
/// an account. Row 6 is the inverse: a Remote Control path routed as pooled.
///
/// The hit counter is the assertion that matters — 400, and nothing on the wire.
#[tokio::test]
async fn traversal_paths_are_refused_before_any_routing_decision() {
    let (upstream, hits) = spawn_echo_upstream().await;
    for uri in [
        "/v1/code/../../v1/messages",
        "/v1/code/%2e%2e/%2e%2e/v1/messages",
        "/v1/code/%2E%2E/%2E%2E/v1/messages",
        "/v1/code/../../_tcr/status",
        "/x/../_tcr/status",
        "/v1/code/.%2e/../v1/messages",
        "/v1/messages/./",
        "/v1/code/..\\../v1/messages",
        "/v1/code\\foo",
    ] {
        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let (status, body) = drive(
            Arc::clone(&manager),
            Method::POST,
            uri,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own-token"),
                ("content-type", "application/json"),
            ],
            r#"{"model":"claude-sonnet-5"}"#,
        )
        .await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} → 400, got {text}");
        assert!(
            text.contains("invalid_request_error"),
            "in the standard error envelope: {text}"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
            0,
            "{uri} spent no account"
        );
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "not one ambiguous path may reach an upstream, by either route"
    );
}

/// The other half of the guard: a dot that is not a dot SEGMENT is ordinary path
/// text and must still be forwarded. A guard that rejected `a.json` would break
/// attachment fetches to fix a traversal — trading one silent failure for another.
#[tokio::test]
async fn legitimate_paths_containing_dots_are_still_forwarded() {
    let (upstream, hits) = spawn_echo_upstream().await;
    for uri in [
        "/api/oauth/files/a.json",
        "/api/oauth/files/....",
        "/api/oauth/files/..%2f..",
        "/api/oauth/files/%5c..",
        "/v1/messages",
    ] {
        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let (status, body) = drive(
            manager,
            Method::POST,
            uri,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own-token"),
                ("content-type", "application/json"),
            ],
            r#"{"model":"claude-sonnet-5"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri} must not be rejected");
        assert_eq!(
            parse_echo(&body).path,
            uri,
            "{uri} reaches the upstream verbatim"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 5);
}

/// The segment-boundary fix on the wire, in both directions. The bare `/v1/code`
/// is the Remote Control route and must carry the CLIENT's token; the longer
/// identifier `/api/oauth/file_upload_v2` is NOT that route and takes rotation.
///
/// The second half is observable only WITHOUT a client bearer: with one, every
/// non-inference path is relayed under it (the bearer rule in [`relay_mode`]),
/// so the boundary decides nothing a client with its own login can see. The
/// last leg pins that too — the same longer identifier, sent WITH a bearer,
/// goes out under the client's credential and spends no account.
#[tokio::test]
async fn segment_boundaries_decide_which_credential_goes_on_the_wire() {
    let (upstream, _hits) = spawn_echo_upstream().await;

    let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
    let (status, body) = drive(
        Arc::clone(&manager),
        Method::POST,
        "/v1/code",
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own-token"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        parse_echo(&body).header("authorization"),
        Some("Bearer client-own-token"),
        "the bare /v1/code is Remote Control and keeps the client's credential"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
        0,
        "…and spends no account"
    );

    let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
    let (status, body) = drive(
        Arc::clone(&manager),
        Method::POST,
        "/api/oauth/file_upload_v2",
        Some(loopback_peer()),
        &[("content-type", "application/json")],
        r#"{"model":"claude-sonnet-5"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        parse_echo(&body).header("authorization"),
        Some("Bearer at-dummy"),
        "a longer identifier is not the upload route — without a client bearer it takes the pooled token"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
        1,
        "…and is counted against the serving account"
    );

    let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
    let (status, body) = drive(
        Arc::clone(&manager),
        Method::POST,
        "/api/oauth/file_upload_v2",
        Some(loopback_peer()),
        &[
            ("authorization", "Bearer client-own-token"),
            ("content-type", "application/json"),
        ],
        r#"{"model":"claude-sonnet-5"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        parse_echo(&body).header("authorization"),
        Some("Bearer client-own-token"),
        "with a client bearer the same non-inference path is the client's own call"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
        0,
        "…and spends no account"
    );
}

/// `POST /v1/oauth/token/` — the trailing-slash spelling of the client's own
/// credential exchange. It used to miss the exact compare and take the POOLED
/// path, attaching our Bearer to an exchange that is not ours. A raw relay sends
/// NO authorization at all, which is the whole point of [`RelayMode::Raw`].
#[tokio::test]
async fn token_refresh_never_takes_a_pooled_bearer_on_any_spelling() {
    let (upstream, _hits) = spawn_echo_upstream().await;
    for uri in ["/v1/oauth/token", "/v1/oauth/token/"] {
        let manager = Manager::with_live_refresher(dummy_config(None, &upstream), None);
        let (status, body) = drive(
            Arc::clone(&manager),
            Method::POST,
            uri,
            Some(loopback_peer()),
            &[
                ("authorization", "Bearer client-own-token"),
                ("content-type", "application/json"),
            ],
            r#"{"grant_type":"refresh_token"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let echo = parse_echo(&body);
        assert_ne!(
            echo.header("authorization"),
            Some("Bearer at-dummy"),
            "{uri} must NEVER carry the pooled account token"
        );
        assert_eq!(
            echo.header("authorization"),
            None,
            "{uri} is a raw relay — it carries no authorization in any form"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].requests,
            0,
            "{uri} spends no account"
        );
    }
}

/// The relays bypass ROTATION, not the api-key gate. A non-loopback caller with
/// no key is refused before anything is forwarded — relaying for an
/// unauthenticated caller would be a new hole, not a fix.
#[tokio::test]
async fn relay_paths_stay_behind_the_api_key_gate() {
    let (upstream, hits) = spawn_echo_upstream().await;
    for (method, path) in [
        (Method::POST, "/v1/code/session/abc"),
        (Method::GET, "/api/oauth/files/file_0123"),
        (Method::POST, "/v1/oauth/token"),
    ] {
        let manager =
            Manager::with_live_refresher(dummy_config(Some("sk-proxy-secret"), &upstream), None);
        let (status, _body) = drive(
            manager,
            method.clone(),
            path,
            Some(remote_peer()),
            &[("authorization", "Bearer client-own-token")],
            "{}",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "keyless remote {method} {path} must be refused"
        );
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "a refused caller's request never reached upstream"
    );
}
