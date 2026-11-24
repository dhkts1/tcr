//! The local control routes (disabled, control account, priority) and the gate that keeps a browser off them.

use super::*;

// --- POST /_tcr/accounts/disabled ---------------------------------------

/// Drive the router with a `ClientAddr` we control, like [`status_request`].
/// `body` is sent verbatim so a malformed one is testable.
async fn control_request(
    manager: Arc<Manager>,
    method: Method,
    peer: Option<SocketAddr>,
    api_key: Option<&str>,
    body: &str,
) -> (StatusCode, HeaderMap, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder().method(method).uri(DISABLED_PATH);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    let mut req = builder
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    if let Some(addr) = peer {
        req.extensions_mut().insert(ClientAddr(addr));
    }
    let response = app(manager).oneshot(req).await.expect("router response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, headers, bytes)
}

fn disabled_in_file(path: &std::path::Path, index: usize) -> Option<Value> {
    let raw = std::fs::read_to_string(path).expect("read the test config");
    let doc: Value = serde_json::from_str(&raw).expect("valid json");
    doc["accounts"][index].get("disabled").cloned()
}

/// THE BITING TEST for the whole change: after the endpoint answers, the
/// account is parked **in the process that will serve the next request**, and
/// the config file carries it too.
///
/// Pre-change there was no route at all, and `tcr disable` wrote only the file
/// — so the running rotation kept the account. Asserting HTTP 200 alone would
/// re-create exactly that defect at a new altitude: the in-memory assertion is
/// the one that fails against a handler that answers politely and changes
/// nothing.
#[tokio::test]
async fn control_endpoint_parks_the_account_in_memory_and_on_disk() {
    let (manager, path) = control_manager("apply", None);
    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert!(!before.accounts[0].disabled, "alice starts in rotation");

    let (status, headers, body) = control_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com","org":null,"disabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(DISABLED_ENDPOINT),
        "the route stamps itself so a caller can tell it from a missing route"
    );

    let payload: SetDisabledResponse =
        serde_json::from_slice(&body).expect("an account-control payload");
    assert_eq!(payload.name, "alice@example.com", "the RESOLVED name");
    assert!(payload.disabled);
    assert!(payload.persisted, "the durable half succeeded");
    assert_eq!(payload.warning, None, "so there is nothing to warn about");

    // 1. THE LIVE ROTATION — the assertion that catches the original bug.
    let after = manager.snapshot(OffsetDateTime::now_utc());
    assert!(
        after.accounts[0].disabled,
        "alice is out of the LIVE rotation, not merely acknowledged"
    );
    assert!(
        !after.accounts[1].disabled,
        "and only the resolved account moved"
    );

    // 2. …AND the file, so the bench survives a restart.
    assert_eq!(disabled_in_file(&path, 0), Some(serde_json::json!(true)));
    assert_eq!(disabled_in_file(&path, 1), None, "bob's row is untouched");

    // Re-enabling drops the key rather than writing `false`, matching the CLI's
    // long-standing file contract.
    let (status, _headers, body) = control_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com","disabled":false}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(
        !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
        "re-enable reaches the live rotation"
    );
    assert_eq!(
        disabled_in_file(&path, 0),
        None,
        "re-enable DROPS the key, never writes false"
    );
    std::fs::remove_file(&path).ok();
}

/// SECURITY GUARD 1 — origin, on the MUTATING route. A non-loopback peer and an
/// absent `ClientAddr` (a request that never came through the listener, so its
/// origin is unprovable) are both refused, and neither may change rotation.
#[tokio::test]
async fn control_endpoint_rejects_a_non_loopback_client() {
    for (label, peer) in [
        ("a routable peer", Some(remote_peer())),
        ("no ClientAddr at all", None),
    ] {
        let (manager, path) = control_manager("origin", None);
        let (status, _headers, _body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            peer,
            None,
            r#"{"query":"alice@example.com","disabled":true}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} must be refused, got {status}"
        );
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "{label}: a refused caller changed the live rotation"
        );
        assert_eq!(
            disabled_in_file(&path, 0),
            None,
            "{label}: a refused caller wrote the config"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// SECURITY GUARD 2 — the key, on the MUTATING route. Every case is a LOOPBACK
/// peer, so what is asserted is precisely that loopback alone does not license
/// steering the rotation of a process holding every account's tokens.
#[tokio::test]
async fn control_endpoint_rejects_a_bad_api_key() {
    for (label, provided) in [
        ("no key at all", None),
        ("a wrong key", Some("wrong-key")),
        ("a prefix of the key", Some("secret")),
        ("an empty key", Some("")),
    ] {
        let (manager, path) = control_manager("key", Some("secret-key"));
        let (status, _headers, _body) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            provided,
            r#"{"query":"alice@example.com","disabled":true}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{label} must be refused, got {status}"
        );
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "{label}: an unauthenticated caller changed the live rotation"
        );
        std::fs::remove_file(&path).ok();
    }

    // …and the right key, from loopback, is served.
    let (manager, path) = control_manager("key-ok", Some("secret-key"));
    let (status, _headers, body) = control_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        Some("secret-key"),
        r#"{"query":"alice@example.com","disabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled);
    std::fs::remove_file(&path).ok();
}

/// The bad-request table. Each row is a body a caller can realistically send,
/// and none of them may half-apply anything.
#[tokio::test]
async fn control_endpoint_rejects_malformed_bodies() {
    for (label, body) in [
        ("an empty body", ""),
        ("not json", "disable alice"),
        ("a json array", r#"["alice@example.com", true]"#),
        ("no disabled field", r#"{"query":"alice@example.com"}"#),
        ("no query field", r#"{"disabled":true}"#),
        ("an empty query", r#"{"query":"   ","disabled":true}"#),
        (
            "disabled as a string",
            r#"{"query":"alice@example.com","disabled":"true"}"#,
        ),
    ] {
        let (manager, path) = control_manager("badbody", None);
        let (status, headers, response) = control_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label} must be a 400, got {status}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(DISABLED_ENDPOINT),
            "{label}: a 400 still identifies the route that produced it"
        );
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "{label}: a rejected body still changed the rotation"
        );
        assert_eq!(
            disabled_in_file(&path, 0),
            None,
            "{label}: and wrote the file"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// A query naming no live account is a 404 **from the route** (stamped), which
/// is what lets the CLI tell it from the local 404 an older tcr returns for a
/// path it does not serve. Same status, opposite reactions.
#[tokio::test]
async fn control_endpoint_404s_an_unknown_account_and_names_itself() {
    let (manager, path) = control_manager("nomatch", None);
    let (status, headers, body) = control_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"nobody@example.com","disabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(DISABLED_ENDPOINT),
        "without this stamp the CLI cannot tell this from a missing route"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("nobody@example.com"), "{text}");
    assert!(
        !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled
            && !manager.snapshot(OffsetDateTime::now_utc()).accounts[1].disabled,
        "a miss parks nobody"
    );
    std::fs::remove_file(&path).ok();
}

/// An ambiguous query is a 409 that NAMES the candidates, so the operator
/// knows which rows to go fix. Refusing is not pedantry: guessing would
/// bench an account nobody asked about.
///
/// Only the LIVE rotation is broken here — `config::load` would have renamed
/// the duplicate — which is the only way this arm is still reachable.
#[tokio::test]
async fn control_endpoint_409s_an_ambiguous_query_naming_the_candidates() {
    let path = control_config_path("ambiguous");
    let mut config = two_account_config(None);
    config.accounts[1].name = "alice@example.com".to_string();
    config::save(&path, &config).expect("write the test config");
    let manager = Manager::with_live_refresher(config, Some(path.clone()));

    let (status, headers, body) = control_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com","disabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(DISABLED_ENDPOINT)
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("ambiguous") && text.matches("alice@example.com").count() >= 2,
        "the 409 names the candidates: {text}"
    );
    let live = manager.snapshot(OffsetDateTime::now_utc());
    assert!(
        !live.accounts[0].disabled && !live.accounts[1].disabled,
        "an unbreakable tie parks NEITHER row"
    );
    assert_eq!(disabled_in_file(&path, 0), None);
    assert_eq!(disabled_in_file(&path, 1), None);
    std::fs::remove_file(&path).ok();
}

/// The same two-org fleet with the names a migration would have given it:
/// the qualified name parks exactly its own row, live AND on disk. The
/// resolution rule is the CLI's own [`crate::identity::match_one`], run
/// against the LIVE rotation slots.
#[tokio::test]
async fn control_endpoint_parks_the_row_a_qualified_name_points_at() {
    let path = control_config_path("qualified-name");
    let mut config = two_account_config(None);
    config.accounts[0].name = "alice@example.com".to_string();
    config.accounts[1].name = "alice@example.com/team".to_string();
    config::save(&path, &config).expect("write the test config");
    let manager = Manager::with_live_refresher(config, Some(path.clone()));

    let (status, _headers, body) = control_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com/team","disabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let live = manager.snapshot(OffsetDateTime::now_utc());
    assert!(
        !live.accounts[0].disabled && live.accounts[1].disabled,
        "the SECOND row — the one the name points at — is the one parked"
    );
    assert_eq!(disabled_in_file(&path, 0), None);
    assert_eq!(disabled_in_file(&path, 1), Some(serde_json::json!(true)));
    std::fs::remove_file(&path).ok();
}

/// A manager with no `config_path` (`tcr demo`, `tcr status --probe`, tests)
/// applies the change LIVE and has nothing to persist to. That is reported
/// honestly — `persisted: false` — and, per
/// [`crate::manager::DisablePersist::warning`], it needs no warning: there is
/// no file that was supposed to carry it.
#[tokio::test]
async fn control_endpoint_reports_an_unpersisted_change_honestly() {
    let manager = Manager::with_live_refresher(two_account_config(None), None);
    let (status, _headers, body) = control_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com","disabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let payload: SetDisabledResponse = serde_json::from_slice(&body).expect("payload");
    assert!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
        "the live change still happened"
    );
    assert!(
        !payload.persisted,
        "…and the answer does not claim it is durable"
    );
    assert_eq!(
        payload.warning, None,
        "memory-only by design, not a failure"
    );
}

/// A `DisablePersist` arm that changed memory but NOT the file is surfaced, not
/// swallowed: the change is in force now and dies on restart, which is the exact
/// state the old file-only code left operators in silently. Here the config file
/// does not exist at all, so the write fails.
#[tokio::test]
async fn control_endpoint_surfaces_a_failed_persist() {
    let path = control_config_path("nofile");
    std::fs::remove_file(&path).ok();
    let manager = Manager::with_live_refresher(two_account_config(None), Some(path.clone()));

    let (status, _headers, body) = control_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com","disabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the LIVE half succeeded");
    let payload: SetDisabledResponse = serde_json::from_slice(&body).expect("payload");
    assert!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
        "the account is parked in the live rotation"
    );
    assert!(!payload.persisted, "but the file does not carry it");
    let warning = payload.warning.expect("a not-saved warning, never silence");
    assert!(
        warning.contains("NOT SAVED") && warning.contains("returns to rotation on restart"),
        "the warning is DisablePersist::warning verbatim, direction included: {warning}"
    );
}

/// The OTHER half of the discriminator, and the half that is easy to get wrong:
/// a path this proxy does not serve answers 404 **without** the stamp.
///
/// This is what `tcr` reads as "the running proxy is too old to accept live
/// account control", which makes it write the file and warn loudly. If the
/// catch-all ever started stamping, that arm would silently become the
/// route-refused arm and the CLI would report a bad query instead of a stale
/// proxy — the original invisible-failure shape, restored.
#[tokio::test]
async fn an_unrouted_local_path_is_not_stamped_as_the_control_route() {
    use tower::ServiceExt as _;
    for uri in [
        // NOT `/_tcr/accounts` — that is now [`ADD_ACCOUNT_PATH`], a real
        // registered route (see the account-add tests below).
        "/_tcr/accounts/add",
        "/_tcr/accounts/disable",
        "/_tcr/accounts/disabled/extra",
    ] {
        let (manager, path) = control_manager("unstamped", None);
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(Body::from(
                r#"{"query":"alice@example.com","disabled":true}"#,
            ))
            .expect("build request");
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        let response = app(Arc::clone(&manager))
            .oneshot(req)
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert!(
            response.headers().get(ENDPOINT_HEADER).is_none(),
            "{uri} must not claim to be the account-control route"
        );
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "{uri} changed nothing"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// The route is `POST`-only, and a wrong method is answered LOCALLY — never
/// forwarded, and never able to mutate. The stamp distinguishes this 405 (route
/// exists) from an older tcr's 404 (route absent).
#[tokio::test]
async fn control_endpoint_refuses_other_methods_locally() {
    for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
        let (manager, path) = control_manager("method", None);
        let (status, headers, _body) = control_request(
            Arc::clone(&manager),
            method.clone(),
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com","disabled":true}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on the control path is refused locally"
        );
        assert_ne!(
            status,
            StatusCode::BAD_GATEWAY,
            "{method} must never fall through to the upstream forwarder"
        );
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(DISABLED_ENDPOINT),
            "{method}: the 405 identifies the route, so a caller does not read \
                 it as a proxy too old to have one"
        );
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "{method} changed the rotation"
        );
        std::fs::remove_file(&path).ok();
    }
}

// --- POST /_tcr/accounts/control ----------------------------------------

fn control_account_in_file(path: &std::path::Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).expect("read the test config");
    let doc: Value = serde_json::from_str(&raw).expect("valid json");
    doc.get("controlAccount").cloned()
}

/// Drive the control-ACCOUNT route with a `ClientAddr` we control, the same
/// shape as [`control_request`] but against [`CONTROL_PATH`].
async fn control_account_request(
    manager: Arc<Manager>,
    method: Method,
    peer: Option<SocketAddr>,
    api_key: Option<&str>,
    body: &str,
) -> (StatusCode, HeaderMap, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder().method(method).uri(CONTROL_PATH);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    let mut req = builder
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    if let Some(addr) = peer {
        req.extensions_mut().insert(ClientAddr(addr));
    }
    let response = app(manager).oneshot(req).await.expect("router response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, headers, bytes)
}

/// THE BITING TEST for the control-account route: after the endpoint
/// answers, the LIVE manager resolves it (`control_name()`), not merely the
/// response body, and the file's top-level `controlAccount` carries it too.
#[tokio::test]
async fn control_account_endpoint_sets_it_in_memory_and_on_disk() {
    let (manager, path) = control_manager("control-apply", None);
    assert_eq!(manager.control(), None, "starts with no control account");

    let (status, headers, body) = control_account_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com","org":null}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(CONTROL_ENDPOINT),
        "the route stamps itself so a caller can tell it from a missing route"
    );

    let payload: SetControlResponse =
        serde_json::from_slice(&body).expect("a control-account payload");
    assert_eq!(payload.name, Some("alice@example.com".to_string()));
    assert!(!payload.cleared);
    assert!(payload.persisted, "the durable half succeeded");
    assert_eq!(payload.warning, None);

    // 1. THE LIVE MANAGER resolves the control account — not merely an
    // acknowledgement in the response body.
    assert_eq!(
        manager.control_name(),
        Some("alice@example.com".to_string()),
        "the live manager must resolve the new control account"
    );

    // 2. …and the file's top-level key carries it too — the account's NAME,
    // which is unique across the config and so is the whole key.
    assert_eq!(
        control_account_in_file(&path),
        Some(serde_json::json!("alice@example.com"))
    );

    // Clearing (`query: null`) removes it from both halves.
    let (status, _headers, body) = control_account_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":null}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let payload: SetControlResponse = serde_json::from_slice(&body).expect("payload");
    assert_eq!(payload.name, None);
    assert!(payload.cleared);
    assert!(payload.persisted);
    assert_eq!(manager.control(), None, "cleared in memory");
    assert_eq!(control_account_in_file(&path), None, "cleared on disk");

    std::fs::remove_file(&path).ok();
}

/// SECURITY GUARD 1 — origin, on the control-account route: same posture as
/// [`control_endpoint_rejects_a_non_loopback_client`].
#[tokio::test]
async fn control_account_endpoint_rejects_a_non_loopback_client() {
    for (label, peer) in [
        ("a routable peer", Some(remote_peer())),
        ("no ClientAddr at all", None),
    ] {
        let (manager, path) = control_manager("control-origin", None);
        let (status, _headers, _body) = control_account_request(
            Arc::clone(&manager),
            Method::POST,
            peer,
            None,
            r#"{"query":"alice@example.com"}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} must be refused, got {status}"
        );
        assert_eq!(
            manager.control(),
            None,
            "{label}: a refused caller changed the control account"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// SECURITY GUARD 2 — the key, on the control-account route: same posture as
/// [`control_endpoint_rejects_a_bad_api_key`].
#[tokio::test]
async fn control_account_endpoint_rejects_a_bad_api_key() {
    for (label, provided) in [("no key at all", None), ("a wrong key", Some("wrong-key"))] {
        let (manager, path) = control_manager("control-key", Some("secret-key"));
        let (status, _headers, _body) = control_account_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            provided,
            r#"{"query":"alice@example.com"}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{label} must be refused, got {status}"
        );
        assert_eq!(manager.control(), None);
        std::fs::remove_file(&path).ok();
    }

    let (manager, path) = control_manager("control-key-ok", Some("secret-key"));
    let (status, _headers, body) = control_account_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        Some("secret-key"),
        r#"{"query":"alice@example.com"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(manager.control(), Some(0));
    std::fs::remove_file(&path).ok();
}

/// A missing/wrong `content-type` is refused (415), stamped, and changes
/// nothing — same posture [`is_json_content_type`] enforces on the other two
/// mutating routes.
#[tokio::test]
async fn control_account_endpoint_requires_json_content_type() {
    use tower::ServiceExt as _;
    let (manager, path) = control_manager("control-ctype", None);
    let req = Request::builder()
        .method(Method::POST)
        .uri(CONTROL_PATH)
        .body(Body::from(r#"{"query":"alice@example.com"}"#.to_string()))
        .expect("build request");
    let mut req = req;
    req.extensions_mut().insert(ClientAddr(loopback_peer()));
    let response = app(Arc::clone(&manager))
        .oneshot(req)
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        response
            .headers()
            .get(ENDPOINT_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(CONTROL_ENDPOINT),
        "a 415 still identifies the route that produced it"
    );
    assert_eq!(manager.control(), None);
    std::fs::remove_file(&path).ok();
}

/// The route is `POST`-only; a wrong method is answered LOCALLY (405),
/// never forwarded, and never able to mutate — same posture as
/// [`control_endpoint_refuses_other_methods_locally`].
#[tokio::test]
async fn control_account_endpoint_refuses_other_methods_locally() {
    for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
        let (manager, path) = control_manager("control-method", None);
        let (status, headers, _body) = control_account_request(
            Arc::clone(&manager),
            method.clone(),
            Some(loopback_peer()),
            None,
            r#"{"query":"alice@example.com"}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on the control-account path is refused locally"
        );
        assert_ne!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(CONTROL_ENDPOINT)
        );
        assert_eq!(
            manager.control(),
            None,
            "{method} changed the control account"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// A query naming no live account is a 404 from the route (stamped), and an
/// ambiguous query is a 409 naming the candidates — same posture as the
/// disabled-flag route's equivalents.
#[tokio::test]
async fn control_account_endpoint_404s_and_409s() {
    let (manager, path) = control_manager("control-nomatch", None);
    let (status, headers, body) = control_account_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"nobody@example.com"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(CONTROL_ENDPOINT)
    );
    assert!(String::from_utf8_lossy(&body).contains("nobody@example.com"));
    assert_eq!(manager.control(), None);
    std::fs::remove_file(&path).ok();

    let path = control_config_path("control-ambiguous");
    let mut config = two_account_config(None);
    config.accounts[1].name = "alice@example.com".to_string();
    config::save(&path, &config).expect("write the test config");
    let manager = Manager::with_live_refresher(config, Some(path.clone()));
    let (status, headers, body) = control_account_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(CONTROL_ENDPOINT)
    );
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("ambiguous") && text.matches("alice@example.com").count() >= 2);
    assert_eq!(manager.control(), None, "an unbreakable tie sets nothing");
    std::fs::remove_file(&path).ok();
}

/// **`tcr control` naming a Codex account is refused on the live route, and changes
/// nothing.** Boot ignores a `controlAccount` that names a Codex account, so accepting
/// one here would persist a setting the next start throws away. 409 and stamped, so
/// every `tcr control` client reports the refusal and writes nothing; the message
/// carries the boot warning's reason.
///
/// Watched red: drop the provider check from `Manager::set_control_by_query` and the
/// route answers 200 and writes the Codex row's name to the file.
#[tokio::test]
async fn control_account_endpoint_refuses_a_codex_account_and_changes_nothing() {
    let path = control_config_path("control-codex");
    let mut config = two_account_config(None);
    config.accounts[1].provider = Provider::Codex;
    let codex = config.accounts[1].name.clone();
    config::save(&path, &config).expect("write the test config");
    let manager = Manager::with_live_refresher(config, Some(path.clone()));

    let (status, headers, body) = control_account_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        &format!(r#"{{"query":"{codex}"}}"#),
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(CONTROL_ENDPOINT)
    );
    assert!(
        text.contains(&codex) && text.contains(crate::manager::CODEX_HAS_NO_CONTROL_ACCOUNT),
        "the refusal names the account and the reason boot gives: {text}"
    );
    assert_eq!(manager.control(), None, "nothing is set in memory");
    assert_eq!(control_account_in_file(&path), None, "nor on disk");
    std::fs::remove_file(&path).ok();
}

/// Drive the priority route with a `ClientAddr` we control, the same shape
/// as [`control_request`] but against [`PRIORITY_PATH`].
async fn priority_request(
    manager: Arc<Manager>,
    method: Method,
    peer: Option<SocketAddr>,
    api_key: Option<&str>,
    body: &str,
) -> (StatusCode, HeaderMap, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder().method(method).uri(PRIORITY_PATH);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    let mut req = builder
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    if let Some(addr) = peer {
        req.extensions_mut().insert(ClientAddr(addr));
    }
    let response = app(manager).oneshot(req).await.expect("router response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, headers, bytes)
}

/// What a fresh session (no affinity pin, nothing tried) is routed to.
fn fresh_pick(manager: &Manager) -> Option<usize> {
    manager.select(
        &HashSet::new(),
        OffsetDateTime::now_utc(),
        None,
        None,
        "/v1/messages",
        None,
    )
}

/// THE BITING TEST for the priority route: after the endpoint answers, the
/// NEXT fresh session is routed by the new priority in the running process.
///
/// Before the route existed `tcr priority` wrote only the file, so the live
/// rotation kept picking alice (priority 0) over bob (priority 1) until a
/// restart. A 200 alone would not catch a handler that answers and changes
/// nothing; the selection is the assertion that does.
#[tokio::test]
async fn priority_endpoint_reorders_the_live_rotation() {
    let (manager, path) = control_manager("priority-apply", None);
    assert_eq!(fresh_pick(&manager), Some(0), "alice leads at priority 0");

    let (status, headers, body) = priority_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"bob@example.com","priority":-1}"#,
    )
    .await;
    let picked = fresh_pick(&manager);
    assert_eq!(
        (status, picked),
        (StatusCode::OK, Some(1)),
        "the route must answer 200 and the next fresh session must go to bob; body: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(PRIORITY_ENDPOINT),
        "the route stamps itself so a caller can tell it from a missing route"
    );
    let payload: SetPriorityResponse = serde_json::from_slice(&body).expect("a priority payload");
    assert_eq!(payload.name, "bob@example.com", "the RESOLVED name");
    assert_eq!(payload.priority, -1);
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(snap.accounts[1].priority, -1, "status shows the live value");
    assert_eq!(
        snap.accounts[0].priority, 0,
        "only the resolved account moved"
    );
    std::fs::remove_file(&path).ok();
}

/// The live priority of rotation slot `idx`.
fn live_priority(manager: &Manager, idx: usize) -> i64 {
    manager.snapshot(OffsetDateTime::now_utc()).accounts[idx].priority
}

/// The priority route passes the same two gates as the other mutating
/// routes: a non-loopback or unprovable origin is 403, a missing or wrong
/// api-key is 401, and neither changes the live rotation.
#[tokio::test]
async fn priority_endpoint_refuses_bad_origin_and_bad_key() {
    let body = r#"{"query":"bob@example.com","priority":-1}"#;
    for (label, peer) in [
        ("a routable peer", Some(remote_peer())),
        ("no ClientAddr at all", None),
    ] {
        let (manager, path) = control_manager("priority-origin", None);
        let (status, _h, _b) =
            priority_request(Arc::clone(&manager), Method::POST, peer, None, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label} must be refused");
        assert_eq!(live_priority(&manager, 1), 1, "{label} changed priority");
        std::fs::remove_file(&path).ok();
    }
    for (label, provided) in [("no key at all", None), ("a wrong key", Some("wrong-key"))] {
        let (manager, path) = control_manager("priority-key", Some("secret-key"));
        let (status, _h, _b) = priority_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            provided,
            body,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{label} must be refused");
        assert_eq!(live_priority(&manager, 1), 1, "{label} changed priority");
        std::fs::remove_file(&path).ok();
    }
    let (manager, path) = control_manager("priority-key-ok", Some("secret-key"));
    let (status, _h, b) = priority_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        Some("secret-key"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(live_priority(&manager, 1), -1);
    std::fs::remove_file(&path).ok();
}

/// Request-shape refusals are local, stamped, and change nothing: a missing
/// content type (415), a wrong method (405), malformed bodies (400), an
/// unknown account (404) and an ambiguous one (409).
#[tokio::test]
async fn priority_endpoint_refuses_bad_requests_locally() {
    use tower::ServiceExt as _;
    let (manager, path) = control_manager("priority-shape", None);
    let stamped = |h: &HeaderMap| {
        h.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()) == Some(PRIORITY_ENDPOINT)
    };

    let mut req = Request::builder()
        .method(Method::POST)
        .uri(PRIORITY_PATH)
        .body(Body::from(r#"{"query":"bob@example.com","priority":-1}"#))
        .expect("build request");
    req.extensions_mut().insert(ClientAddr(loopback_peer()));
    let response = app(Arc::clone(&manager))
        .oneshot(req)
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(stamped(response.headers()), "a 415 names its route");

    for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
        let (status, headers, _b) = priority_request(
            Arc::clone(&manager),
            method.clone(),
            Some(loopback_peer()),
            None,
            r#"{"query":"bob@example.com","priority":-1}"#,
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert!(stamped(&headers), "{method}: a 405 names its route");
    }

    for bad in [
        "not json",
        r#"["bob@example.com",-1]"#,
        r#"{"query":"bob@example.com"}"#,
        r#"{"priority":-1}"#,
        r#"{"query":"bob@example.com","priority":"first"}"#,
        r#"{"query":"  ","priority":-1}"#,
    ] {
        let (status, headers, _b) = priority_request(
            Arc::clone(&manager),
            Method::POST,
            Some(loopback_peer()),
            None,
            bad,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert!(stamped(&headers), "{bad}: a 400 names its route");
    }

    let (status, headers, body) = priority_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"nobody@example.com","priority":-1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(stamped(&headers));
    assert!(String::from_utf8_lossy(&body).contains("nobody@example.com"));
    assert_eq!(live_priority(&manager, 0), 0, "nothing moved");
    assert_eq!(live_priority(&manager, 1), 1, "nothing moved");
    std::fs::remove_file(&path).ok();

    let path = control_config_path("priority-ambiguous");
    let mut config = two_account_config(None);
    config.accounts[1].name = "alice@example.com".to_string();
    config::save(&path, &config).expect("write the test config");
    let manager = Manager::with_live_refresher(config, Some(path.clone()));
    let (status, headers, _b) = priority_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"alice@example.com","priority":-1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(stamped(&headers));
    assert_eq!(live_priority(&manager, 0), 0, "a tie sets nothing");
    assert_eq!(live_priority(&manager, 1), 1, "a tie sets nothing");
    std::fs::remove_file(&path).ok();
}

/// **`tcr priority` on a Codex account moves it within the Codex pool, live, and
/// moves nothing else.** The route resolves by exact name, like every live route,
/// so a Codex row is reached by its own name and, unlike `tcr control` (which
/// refuses a Codex account), takes the priority: the next fresh Codex request goes
/// to it, while a fresh Claude request still goes to the Anthropic account.
///
/// Watched red: refuse a Codex row in `set_priority_by_query` as the control route
/// does, and the route answers 404 and the Codex pick never moves.
#[tokio::test]
async fn priority_endpoint_moves_a_codex_account_within_its_own_provider() {
    let mut config = codex_fleet_with_carol("http://127.0.0.1:1", "http://127.0.0.1:1");
    config.accounts[1].priority = Some(0);
    config.accounts[2].priority = Some(1);
    let manager = Manager::with_live_refresher(config, None);
    let fresh = |path: &str| {
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            path,
            None,
        )
    };
    assert_eq!(
        fresh(crate::provider::CODEX_RESPONSES_PATH),
        Some(1),
        "bob leads the Codex pool"
    );
    assert_eq!(fresh("/v1/messages"), Some(0));

    let (status, headers, body) = priority_request(
        Arc::clone(&manager),
        Method::POST,
        Some(loopback_peer()),
        None,
        r#"{"query":"carol@example.com/codex","priority":-1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(PRIORITY_ENDPOINT)
    );
    let payload: SetPriorityResponse = serde_json::from_slice(&body).expect("a priority payload");
    assert_eq!(payload.name, "carol@example.com/codex");
    assert_eq!(
        fresh(crate::provider::CODEX_RESPONSES_PATH),
        Some(2),
        "the next fresh Codex request goes to carol"
    );
    assert_eq!(
        fresh("/v1/messages"),
        Some(0),
        "a Claude request never reaches a Codex account, whatever its priority"
    );
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        (snap.accounts[2].priority, snap.accounts[2].provider),
        (-1, Provider::Codex)
    );
}

/// Drive the control route with FULL control over the request line and every
/// header, so a BROWSER-shaped or forward-proxy-shaped request is testable as
/// it would actually arrive. [`control_request`] always sends
/// `content-type: application/json` on an origin-form target — which is
/// precisely the one shape that was never the attack.
///
/// The peer is always loopback and no proxy api-key is configured, because that
/// is the population this class applies to: [`crate::config`] defaults
/// `api_key: None` and nothing generates one, so on a fresh install loopback is
/// the whole gate. With a key configured the route is closed by the key check
/// (see [`control_endpoint_rejects_a_bad_api_key`]) — requiring a header also
/// makes the request non-simple, so a browser never sends it at all.
async fn control_request_shaped(
    manager: Arc<Manager>,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, HeaderMap, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder().method(Method::POST).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder
        .body(Body::from(body.to_string()))
        .expect("build request");
    req.extensions_mut().insert(ClientAddr(loopback_peer()));
    let response = app(manager).oneshot(req).await.expect("router response");
    let status = response.status();
    let response_headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, response_headers, bytes)
}

/// One row of a request-SHAPE table: what it is, the request target, the
/// headers, and the status it must come back with.
type ShapeCase<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>, StatusCode);

/// A row of a table every case of which must be SERVED, so there is no status
/// column to get wrong.
type ServedCase<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>);

/// A row that varies only in its headers — the target is fixed.
type HeaderCase<'a> = (&'a str, Vec<(&'a str, &'a str)>, StatusCode);

/// THE CSRF TABLE — the biting test for this change. Every row is a request a
/// WEB PAGE open in a browser on this host can actually cause, and each of the
/// first four **returned 200 and parked a live account** before these checks
/// existed (measured on the route as merged in #71).
///
/// Why a browser can send them at all: `text/plain`,
/// `application/x-www-form-urlencoded` and `multipart/form-data` are the three
/// CORS **simple** content types, so a cross-origin `fetch`/form POST carrying
/// one is sent with NO preflight. The page cannot read the reply — this proxy
/// emits no `Access-Control-*` header — which is irrelevant: the entire payoff
/// of this route IS the side effect, so write-only is the whole attack. The
/// read route next door was immune by being a read; this class arrives with the
/// FIRST mutating route, and the loopback gate's own doc-comment already argues
/// that bind scope is not authorization. The browser is the caller it failed to
/// enumerate.
///
/// The rebound `Host` row is the DNS-rebinding shape and the reason a
/// content-type check alone is not enough: a page served from a name that
/// resolves to 127.0.0.1 is SAME-ORIGIN with the proxy, so it sends no `Origin`,
/// triggers no preflight, and may use any content type it likes. The only thing
/// that distinguishes it from a real local caller is the name it addressed us
/// by.
///
/// The assertion that bites is the in-memory one: a handler that answers 415
/// politely and parks the account anyway passes a status-only check.
#[tokio::test]
async fn control_endpoint_refuses_every_browser_reachable_shape() {
    let json = r#"{"query":"alice@example.com","disabled":true}"#;
    let absolute_form = format!("http://api.anthropic.com{DISABLED_PATH}");
    let cases: Vec<ShapeCase> = vec![
        (
            "a text/plain body — a CORS simple request, so no preflight",
            DISABLED_PATH,
            vec![("content-type", "text/plain;charset=UTF-8")],
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "an x-www-form-urlencoded body — simple too, and what a bare \
                 <form> posts",
            DISABLED_PATH,
            vec![("content-type", "application/x-www-form-urlencoded")],
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "a multipart/form-data body — the third simple type",
            DISABLED_PATH,
            vec![("content-type", "multipart/form-data; boundary=x")],
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "no content-type at all — absent is simple as well",
            DISABLED_PATH,
            vec![],
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "a rebound Host — a name that resolves to 127.0.0.1, so the page \
                 is same-origin and sends nothing that marks it cross-site",
            DISABLED_PATH,
            vec![
                ("content-type", "application/json"),
                ("host", "rebound.example.com"),
            ],
            StatusCode::MISDIRECTED_REQUEST,
        ),
        (
            "an absolute-form request line naming anthropic — a forward-proxy \
                 request, never something addressed to this route",
            &absolute_form,
            vec![("content-type", "application/json")],
            StatusCode::MISDIRECTED_REQUEST,
        ),
        (
            "a cross-origin fetch that DID preflight (a future route may not \
                 be JSON, so the Origin is refused on its own merits)",
            DISABLED_PATH,
            vec![
                ("content-type", "application/json"),
                ("origin", "https://evil.example.com"),
            ],
            StatusCode::FORBIDDEN,
        ),
        (
            "Sec-Fetch-Site: cross-site — the browser saying so itself",
            DISABLED_PATH,
            vec![
                ("content-type", "application/json"),
                ("sec-fetch-site", "cross-site"),
            ],
            StatusCode::FORBIDDEN,
        ),
        (
            "Sec-Fetch-Site: same-site — a sibling subdomain is not us",
            DISABLED_PATH,
            vec![
                ("content-type", "application/json"),
                ("sec-fetch-site", "same-site"),
            ],
            StatusCode::FORBIDDEN,
        ),
    ];

    for (label, uri, headers, expected) in cases {
        let (manager, path) = control_manager("csrf", None);
        let (status, _headers, body) =
            control_request_shaped(Arc::clone(&manager), uri, &headers, json).await;
        assert_eq!(
            status,
            expected,
            "{label}: expected {expected}, got {status}: {}",
            String::from_utf8_lossy(&body)
        );
        // THE assertion. A refusal that still mutates is the same defect one
        // altitude up, and a status-only table would not see it.
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "{label}: a refused request changed the LIVE rotation"
        );
        assert_eq!(
            disabled_in_file(&path, 0),
            None,
            "{label}: a refused request wrote the config"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// The other direction, so the checks above cannot be satisfied by refusing
/// everything: every shape a LEGITIMATE local caller emits is still served and
/// still mutates. `tcr disable` builds its request with reqwest's `.json()`
/// against `http://127.0.0.1:<port>` (`cli.rs`), which is row 1.
#[tokio::test]
async fn control_endpoint_still_serves_every_legitimate_local_shape() {
    let json = r#"{"query":"alice@example.com","disabled":true}"#;
    let loopback_form = format!("http://127.0.0.1:3456{DISABLED_PATH}");
    let cases: Vec<ServedCase> = vec![
        (
            "what `tcr disable` sends: reqwest .json() to a loopback literal",
            DISABLED_PATH,
            vec![
                ("content-type", "application/json"),
                ("host", "127.0.0.1:3456"),
            ],
        ),
        (
            "a charset parameter on the media type — RFC 9110 legitimate",
            DISABLED_PATH,
            vec![("content-type", "application/json; charset=utf-8")],
        ),
        (
            "an upper-case media type — the type is case-insensitive",
            DISABLED_PATH,
            vec![("content-type", "Application/JSON")],
        ),
        (
            "Host: localhost — loopback by name, not by literal",
            DISABLED_PATH,
            vec![
                ("content-type", "application/json"),
                ("host", "localhost:3456"),
            ],
        ),
        (
            "Host: [::1] — the v6 loopback literal, port stripped like the \
                 forwarding path's own host guard does",
            DISABLED_PATH,
            vec![("content-type", "application/json"), ("host", "[::1]:3456")],
        ),
        (
            "no Host header at all — an origin-form base-URL request, which is \
                 also every direct-axum caller",
            DISABLED_PATH,
            vec![("content-type", "application/json")],
        ),
        (
            "an absolute-form loopback target — the authority is still ours",
            &loopback_form,
            vec![("content-type", "application/json")],
        ),
        (
            "Sec-Fetch-Site: same-origin",
            DISABLED_PATH,
            vec![
                ("content-type", "application/json"),
                ("sec-fetch-site", "same-origin"),
            ],
        ),
        (
            "Sec-Fetch-Site: none — a user-initiated request, not a site's",
            DISABLED_PATH,
            vec![
                ("content-type", "application/json"),
                ("sec-fetch-site", "none"),
            ],
        ),
    ];

    for (label, uri, headers) in cases {
        let (manager, path) = control_manager("legit", None);
        let (status, headers, body) =
            control_request_shaped(Arc::clone(&manager), uri, &headers, json).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{label}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(DISABLED_ENDPOINT),
            "{label}: the route still stamps itself"
        );
        assert!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "{label}: a served request must still reach the LIVE rotation"
        );
        assert_eq!(
            disabled_in_file(&path, 0),
            Some(serde_json::json!(true)),
            "{label}: and the durable half"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// The READ route is deliberately NOT content-type gated (a GET has no body to
/// type), and this pins that: the shapes the mutating route now refuses with a
/// 415 are served here, unchanged. Changing it would buy nothing — the route is
/// side-effect-free and a page cannot read its reply — and would break `tcr
/// status`, which sends no content-type on a GET.
///
/// The cross-site and host checks DO apply, because they live in the shared
/// [`local_endpoint_gate`]: one implementation for both routes, per that
/// function's own reasoning about which copy of a duplicated gate drifts.
#[tokio::test]
async fn status_endpoint_is_not_content_type_gated_but_is_cross_site_gated() {
    use tower::ServiceExt as _;
    let served = [
        ("no content-type — what `tcr status` actually sends", vec![]),
        (
            "a text/plain content-type on a GET",
            vec![("content-type", "text/plain")],
        ),
    ];
    let refused: [HeaderCase; 3] = [
        (
            "a cross-origin Origin",
            vec![("origin", "https://evil.example.com")],
            StatusCode::FORBIDDEN,
        ),
        (
            "Sec-Fetch-Site: cross-site",
            vec![("sec-fetch-site", "cross-site")],
            StatusCode::FORBIDDEN,
        ),
        (
            "a rebound Host",
            vec![("host", "rebound.example.com")],
            StatusCode::MISDIRECTED_REQUEST,
        ),
    ];

    let probe_status = |headers: Vec<(&'static str, &'static str)>| async move {
        let manager = Manager::with_live_refresher(two_account_config(None), None);
        let mut builder = Request::builder().method(Method::GET).uri(STATUS_PATH);
        for (name, value) in &headers {
            builder = builder.header(*name, *value);
        }
        let mut req = builder.body(Body::empty()).expect("build request");
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        app(manager)
            .oneshot(req)
            .await
            .expect("router response")
            .status()
    };

    for (label, headers) in served {
        assert_eq!(probe_status(headers).await, StatusCode::OK, "{label}");
    }
    for (label, headers, expected) in refused {
        assert_eq!(probe_status(headers).await, expected, "{label}");
    }
}

/// The three new pure helpers, in isolation — the negatives are the point.
#[test]
fn local_gate_helpers_classify_their_edges() {
    // A loopback name is any 127/8 address, the v6 loopback bracketed or bare,
    // and `localhost` in any case. Nothing else, including the addresses a
    // rebound page would be served from.
    for host in [
        "127.0.0.1",
        "127.0.0.53",
        "::1",
        "[::1]",
        "localhost",
        "LocalHost",
    ] {
        assert!(host_is_loopback(host), "{host} is loopback");
    }
    for host in [
        "rebound.example.com",
        "api.anthropic.com",
        "169.254.169.254",
        "0.0.0.0",
        "192.168.1.10",
        "localhost.evil.example.com",
        "",
    ] {
        assert!(!host_is_loopback(host), "{host} is NOT loopback");
    }

    let with = |name: &str, value: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
        headers
    };

    // Only `application/json`, parameters and case allowed; ABSENT is a no.
    for value in [
        "application/json",
        "Application/JSON",
        "application/json; charset=utf-8",
        "application/json;charset=UTF-8",
    ] {
        assert!(
            is_json_content_type(&with("content-type", value)),
            "{value} is json"
        );
    }
    for value in [
        "text/plain",
        "text/plain;charset=UTF-8",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "application/json-patch+json",
        "application/jsonx",
        "",
    ] {
        assert!(
            !is_json_content_type(&with("content-type", value)),
            "{value} is NOT json"
        );
    }
    assert!(
        !is_json_content_type(&HeaderMap::new()),
        "no content-type at all is a CORS simple request too"
    );

    // Cross-site: any Origin, or a Sec-Fetch-Site that is not ours.
    assert!(is_cross_site_request(&with(
        "origin",
        "https://evil.example.com"
    )));
    assert!(is_cross_site_request(&with("origin", "null")));
    assert!(is_cross_site_request(&with("sec-fetch-site", "cross-site")));
    assert!(is_cross_site_request(&with("sec-fetch-site", "same-site")));
    assert!(!is_cross_site_request(&with(
        "sec-fetch-site",
        "same-origin"
    )));
    assert!(!is_cross_site_request(&with("sec-fetch-site", "none")));
    assert!(
        !is_cross_site_request(&HeaderMap::new()),
        "a non-browser caller sends neither header, and must not be refused"
    );

    // The authority in an absolute-form target WINS over the Host header, and
    // an origin-form request with no Host names no host at all.
    let absolute: axum::http::Uri = "http://api.anthropic.com/_tcr/status".parse().expect("uri");
    assert_eq!(
        target_host(&absolute, &with("host", "127.0.0.1:3456")),
        Some("api.anthropic.com"),
        "the request line is what the client asked for"
    );
    let origin_form: axum::http::Uri = "/_tcr/status".parse().expect("uri");
    assert_eq!(
        target_host(&origin_form, &with("host", "localhost:3456")),
        Some("localhost"),
        "the port is stripped"
    );
    assert_eq!(
        target_host(&origin_form, &with("host", "[::1]:3456")),
        Some("[::1]"),
        "…and a bracketed v6 literal survives it, brackets included"
    );
    assert_eq!(
        target_host(&origin_form, &with("host", "[::1]")),
        Some("[::1]"),
        "a v6 literal with NO port keeps every colon: splitting on the last \
             one yields '[:' and misroutes an IPv6 loopback client"
    );
    assert_eq!(target_host(&origin_form, &HeaderMap::new()), None);
}
