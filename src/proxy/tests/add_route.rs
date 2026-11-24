//! The `POST /_tcr/accounts` route that adds or updates an account.

use super::*;

// --- POST /_tcr/accounts (add) -------------------------------------------

/// Drive the router at [`ADD_ACCOUNT_PATH`], mirroring [`control_request`].
async fn add_request(
    manager: Arc<Manager>,
    peer: Option<SocketAddr>,
    api_key: Option<&str>,
    body: &str,
) -> (StatusCode, HeaderMap, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(ADD_ACCOUNT_PATH);
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

fn account_in_file(path: &std::path::Path, index: usize) -> Option<Value> {
    let raw = std::fs::read_to_string(path).expect("read the test config");
    let doc: Value = serde_json::from_str(&raw).expect("valid json");
    doc["accounts"].get(index).cloned()
}

fn account_count_in_file(path: &std::path::Path) -> usize {
    let raw = std::fs::read_to_string(path).expect("read the test config");
    let doc: Value = serde_json::from_str(&raw).expect("valid json");
    doc["accounts"].as_array().map_or(0, Vec::len)
}

/// THE BITING TEST for the append half: a brand-new identity lands at the END
/// of the live rotation and is **immediately servable** — not merely
/// acknowledged. Every OTHER account is put in `tried`, so `select` can only
/// return the new one by actually finding it eligible.
#[tokio::test]
async fn add_endpoint_appends_a_new_account_and_it_is_immediately_servable() {
    let (manager, path) = control_manager("append", None);
    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(before.accounts.len(), 2, "starts with alice and bob");

    let (status, headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"carol@example.com","accessToken":"at-carol","refreshToken":"rt-carol","expiresAt":9999999999999}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(ADD_ACCOUNT_ENDPOINT),
        "the route stamps itself so a caller can tell it from a missing route"
    );

    let payload: AddAccountResponse =
        serde_json::from_slice(&body).expect("an account-add payload");
    assert_eq!(payload.name, "carol@example.com");
    assert!(
        payload.added,
        "a brand new identity is an APPEND, not an update"
    );
    assert_eq!(payload.index, 2, "appended at the END");
    assert!(payload.persisted, "the durable half succeeded");
    assert_eq!(payload.warning, None, "a refresh token was supplied");

    // 1. THE LIVE ROTATION — present AND selectable, not merely acknowledged.
    let after = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(after.accounts.len(), 3);
    assert_eq!(after.accounts[2].name, "carol@example.com");
    let mut tried: HashSet<usize> = HashSet::new();
    tried.insert(0);
    tried.insert(1);
    assert_eq!(
        manager.select(
            &tried,
            OffsetDateTime::now_utc(),
            None,
            None,
            "/v1/messages",
            None
        ),
        Some(2),
        "the new account is eligible and selectable immediately, with no restart"
    );

    // 2. …and the file, so it survives one.
    assert_eq!(account_count_in_file(&path), 3);
    let on_disk = account_in_file(&path, 2).expect("the appended entry");
    assert_eq!(on_disk["name"], "carol@example.com");
    assert_eq!(on_disk["accessToken"], "at-carol");
    std::fs::remove_file(&path).ok();
}

/// A live Codex add that names the folder its login came from lands it on the running row
/// and in the file, as the file route does: the route mirrors `config::Account`'s wire
/// shape field by field, so a field missing here is silently dropped.
///
/// Watched red: without the request field, the row and the file carry no folder.
#[tokio::test]
async fn add_account_request_carries_the_codex_home() {
    let (manager, path) = control_manager("codex-home", None);
    let (status, _, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"carol@example.com","provider":"codex","accountUuid":"55555555-5555-4555-8555-555555555555","orgUuid":"66666666-6666-4666-8666-666666666666","accessToken":"at-carol","refreshToken":"rt-carol","expiresAt":9999999999999,"codexHome":"/Users/example/.codex"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let carol = snapshot
        .accounts
        .iter()
        .find(|a| a.name == "carol@example.com")
        .expect("carol is in the running proxy");
    assert_eq!(
        carol.codex.codex_home.as_deref(),
        Some("/Users/example/.codex")
    );
    let on_disk = account_in_file(&path, 2).expect("the appended entry");
    assert_eq!(on_disk["name"], "carol@example.com");
    assert_eq!(on_disk["codexHome"], "/Users/example/.codex");
    std::fs::remove_file(&path).ok();
}

/// An `apikey` account is refused here with a stamped 400, by the same rule
/// `config::load` quarantines a file row by: tcr does not serve API-key
/// accounts. Neither the live rotation nor the file changes.
#[tokio::test]
async fn add_endpoint_refuses_an_api_key_account() {
    let (manager, path) = control_manager("refuse-apikey", None);
    let (status, headers, body) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        None,
        r#"{"name":"carol@example.com","type":"apikey","accessToken":"sk-ant-api-fake"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(ADD_ACCOUNT_ENDPOINT)
    );
    assert!(
        String::from_utf8_lossy(&body).contains("does not serve API-key accounts"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let after = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(after.accounts.len(), 2, "nothing joined the rotation");
    assert_eq!(account_count_in_file(&path), 2, "nothing reached the file");
    std::fs::remove_file(&path).ok();
}

/// THE BITING TEST for the update half: re-adding an EXISTING identity
/// replaces its credentials in place — same index, same account count — and
/// its routing state (priority) survives untouched.
#[tokio::test]
async fn add_endpoint_updates_an_existing_account_in_place_and_keeps_the_index() {
    let (manager, path) = control_manager("update", None);
    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(before.accounts.len(), 2);
    assert_eq!(before.accounts[0].name, "alice@example.com");
    assert_eq!(before.accounts[0].priority, 0);

    let (status, _headers, body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            None,
            r#"{"name":"alice@example.com","accessToken":"at-alice-fresh","refreshToken":"rt-alice-fresh","expiresAt":9999999999999}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let payload: AddAccountResponse =
        serde_json::from_slice(&body).expect("an account-add payload");
    assert_eq!(payload.name, "alice@example.com");
    assert!(
        !payload.added,
        "an existing identity is an UPDATE, not an append"
    );
    assert_eq!(
        payload.index, 0,
        "the SAME index — never moved, never duplicated"
    );
    assert!(payload.persisted, "the durable half succeeded");

    // 1. THE LIVE ROTATION: same count, same index, NEW credentials, and
    // routing state (priority) untouched.
    let after = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(after.accounts.len(), 2, "no duplicate appended");
    assert_eq!(after.accounts[0].name, "alice@example.com");
    assert_eq!(after.accounts[0].priority, 0, "routing state preserved");
    assert_eq!(
        manager.access_token(0).as_deref(),
        Some("at-alice-fresh"),
        "the credential actually changed"
    );

    // 2. …and the file: same entry, same position, new token.
    assert_eq!(account_count_in_file(&path), 2, "no duplicate row on disk");
    let on_disk = account_in_file(&path, 0).expect("alice's entry");
    assert_eq!(on_disk["accessToken"], "at-alice-fresh");
    std::fs::remove_file(&path).ok();
}

/// SECURITY GUARD 1 — origin, on the account-ADD route. [`local_endpoint_gate`]
/// is the ONE implementation the disable and add routes both use, so this is
/// largely a re-proof — but on a write that carries a fresh OAuth token in its
/// body, a drift here would be worse.
#[tokio::test]
async fn add_endpoint_rejects_a_non_loopback_client() {
    for (label, peer) in [
        ("a routable peer", Some(remote_peer())),
        ("no ClientAddr at all", None),
    ] {
        let (manager, path) = control_manager("add-origin", None);
        let (status, _headers, _body) = add_request(
            Arc::clone(&manager),
            peer,
            None,
            r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} must be refused, got {status}"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            2,
            "{label}: a refused caller appended an account"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// SECURITY GUARD 2 — the key, on the account-ADD route.
#[tokio::test]
async fn add_endpoint_rejects_a_bad_api_key() {
    for (label, provided) in [("no key at all", None), ("a wrong key", Some("wrong-key"))] {
        let (manager, path) = control_manager("add-key", Some("secret-key"));
        let (status, _headers, _body) = add_request(
            Arc::clone(&manager),
            Some(loopback_peer()),
            provided,
            r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{label} must be refused, got {status}"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            2,
            "{label}: an unauthenticated caller appended an account"
        );
        std::fs::remove_file(&path).ok();
    }

    // …and the right key, from loopback, is served.
    let (manager, path) = control_manager("add-key-ok", Some("secret-key"));
    let (status, _headers, body) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        Some("secret-key"),
        r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
        3
    );
    std::fs::remove_file(&path).ok();
}

/// A non-JSON content-type is refused — this route carries an OAuth token in
/// its body and is a write, so the browser/CSRF defence must be no weaker
/// than the disable route's.
#[tokio::test]
async fn add_endpoint_requires_json_content_type() {
    use tower::ServiceExt as _;
    let (manager, path) = control_manager("add-ctype", None);
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(ADD_ACCOUNT_PATH)
        .header(CONTENT_TYPE, "text/plain")
        .body(Body::from(
            r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
        ))
        .expect("build request");
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
        Some(ADD_ACCOUNT_ENDPOINT)
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
        2
    );
    std::fs::remove_file(&path).ok();
}

/// The bad-request table. None of these may half-apply anything.
#[tokio::test]
async fn add_endpoint_rejects_malformed_bodies() {
    for (label, body) in [
        ("an empty body", ""),
        ("not json", "add carol"),
        ("a json array", r#"["carol@example.com"]"#),
        ("no accessToken field", r#"{"name":"carol@example.com"}"#),
        ("no name field", r#"{"accessToken":"at-carol"}"#),
        (
            "an empty name",
            r#"{"name":"   ","accessToken":"at-carol"}"#,
        ),
        (
            "an empty accessToken",
            r#"{"name":"carol@example.com","accessToken":"  "}"#,
        ),
    ] {
        let (manager, path) = control_manager("add-badbody", None);
        let (status, headers, response) =
            add_request(Arc::clone(&manager), Some(loopback_peer()), None, body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label} must be a 400, got {status}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(ADD_ACCOUNT_ENDPOINT),
            "{label}: a 400 still identifies the route that produced it"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            2,
            "{label}: a rejected body still appended an account"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// An ambiguous identity is a 409 naming the candidates, never guessed. Only
/// the LIVE rotation is broken here — `config::load` would have renamed the
/// duplicate — which is the only way this arm is still reachable.
#[tokio::test]
async fn add_endpoint_409s_an_ambiguous_identity_naming_the_candidates() {
    let path = control_config_path("add-ambiguous");
    let mut config = two_account_config(None);
    config.accounts[1].name = "alice@example.com".to_string();
    config::save(&path, &config).expect("write the test config");
    let manager = Manager::with_live_refresher(config, Some(path.clone()));

    let (status, headers, body) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        None,
        r#"{"name":"alice@example.com","accessToken":"at-new"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
        Some(ADD_ACCOUNT_ENDPOINT)
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("ambiguous") && text.matches("alice@example.com").count() >= 2,
        "the 409 names the candidates: {text}"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
        2,
        "an unbreakable tie changes nothing"
    );
    std::fs::remove_file(&path).ok();
}

/// A manager with no `config_path` (`tcr demo`, `tcr status --probe`, tests)
/// applies the change LIVE and has nothing to persist to — reported honestly
/// (`persisted: false`, no warning), exactly as the disable route's
/// equivalent case.
#[tokio::test]
async fn add_endpoint_reports_an_unpersisted_change_honestly() {
    let manager = Manager::with_live_refresher(two_account_config(None), None);
    let (status, _headers, body) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        None,
        r#"{"name":"carol@example.com","accessToken":"at-carol","refreshToken":"rt-carol"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let payload: AddAccountResponse = serde_json::from_slice(&body).expect("payload");
    assert!(payload.added);
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
        3,
        "the live append still happened"
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

/// A failed durable write still returns 200 — the live change stands — with
/// `persisted: false` and a warning, never swallowed. Here the config file
/// does not exist at all, so the write fails.
#[tokio::test]
async fn add_endpoint_surfaces_a_failed_persist() {
    let path = control_config_path("add-nofile");
    std::fs::remove_file(&path).ok();
    let manager = Manager::with_live_refresher(two_account_config(None), Some(path.clone()));

    let (status, _headers, body) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        None,
        r#"{"name":"carol@example.com","accessToken":"at-carol","refreshToken":"rt-carol"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the LIVE half succeeded");
    let payload: AddAccountResponse = serde_json::from_slice(&body).expect("payload");
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
        3,
        "the account is appended in the live rotation"
    );
    assert!(!payload.persisted, "but the file does not carry it");
    let warning = payload.warning.expect("a NOT SAVED warning, never silence");
    assert!(
        warning.contains("NOT SAVED"),
        "the warning is AddPersist::warning verbatim: {warning}"
    );
}

/// The submitted account carries no refresh token — a fact the operator
/// needs at add time, not a hard error: it will serve now and go dead once
/// the access token expires.
#[tokio::test]
async fn add_endpoint_warns_when_no_refresh_token_is_supplied() {
    let (manager, path) = control_manager("add-norefresh", None);
    let (status, _headers, body) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        None,
        r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let payload: AddAccountResponse = serde_json::from_slice(&body).expect("payload");
    assert!(payload.persisted);
    let warning = payload.warning.expect("a no-refresh-token warning");
    assert!(warning.contains("no refresh token"), "{warning}");
    std::fs::remove_file(&path).ok();
}

/// F7 — the add route is `POST`-only, same as the disable route
/// ([`control_endpoint_refuses_other_methods_locally`]), and had no test of
/// its own: a wrong method must be answered LOCALLY (never forwarded, never
/// able to mutate), and the 405 must still identify the route so a caller
/// can tell it from an older tcr that has no route at all.
#[tokio::test]
async fn add_endpoint_refuses_other_methods_locally() {
    use tower::ServiceExt as _;
    for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
        let (manager, path) = control_manager("add-method", None);
        let mut req = Request::builder()
            .method(method.clone())
            .uri(ADD_ACCOUNT_PATH)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
            ))
            .expect("build request");
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        let response = app(Arc::clone(&manager))
            .oneshot(req)
            .await
            .expect("router response");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on the add-account path is refused locally"
        );
        assert_ne!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "{method} must never fall through to the upstream forwarder"
        );
        assert_eq!(
            response
                .headers()
                .get(ENDPOINT_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(ADD_ACCOUNT_ENDPOINT),
            "{method}: the 405 identifies the route, so a caller does not read \
                 it as a proxy too old to have one"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
            2,
            "{method} appended or changed an account"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// Every answer the account-add route STAMPS also names the providers this
/// server can store — the blank-probe 400 `oauth::probe_add_route` reads it
/// from, a 405, a 409 and a 200 — and the gate's unstamped refusal names
/// neither, so a caller that failed the gate learns nothing about the route.
/// The stamp's own value is pinned literally: every older CLI recognises the
/// route by that exact string, so the providers had to be a second header.
#[tokio::test]
async fn the_add_route_names_its_providers_on_every_stamped_answer() {
    assert_eq!(
        ADD_ACCOUNT_ENDPOINT, "accounts-add",
        "older CLIs (TcrBar's bundled one included) match this exact value"
    );
    let names_providers = |label: &str, headers: &HeaderMap| {
        assert_eq!(
            headers.get(ENDPOINT_HEADER).and_then(|v| v.to_str().ok()),
            Some(ADD_ACCOUNT_ENDPOINT),
            "{label}: stamped"
        );
        assert_eq!(
            headers.get(PROVIDERS_HEADER).and_then(|v| v.to_str().ok()),
            Some("anthropic,codex"),
            "{label}: the providers ride on the stamped answer"
        );
    };

    // The blank probe's 400.
    let (manager, path) = control_manager("add-providers-400", None);
    let (status, headers, _) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        None,
        r#"{"name":"","accessToken":""}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    names_providers("the blank probe's 400", &headers);
    std::fs::remove_file(&path).ok();

    // A 200.
    let (manager, path) = control_manager("add-providers-200", None);
    let (status, headers, _) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        None,
        r#"{"name":"carol@example.com","accessToken":"at-carol","refreshToken":"rt-carol"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    names_providers("a 200", &headers);
    std::fs::remove_file(&path).ok();

    // A 409.
    let path = control_config_path("add-providers-409");
    let mut config = two_account_config(None);
    config.accounts[1].name = "alice@example.com".to_string();
    config::save(&path, &config).expect("write the test config");
    let manager = Manager::with_live_refresher(config, Some(path.clone()));
    let (status, headers, _) = add_request(
        Arc::clone(&manager),
        Some(loopback_peer()),
        None,
        r#"{"name":"alice@example.com","accessToken":"at-new"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    names_providers("a 409", &headers);
    std::fs::remove_file(&path).ok();

    // A 405.
    {
        use tower::ServiceExt as _;
        let (manager, path) = control_manager("add-providers-405", None);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(ADD_ACCOUNT_PATH)
            .body(Body::empty())
            .expect("build request");
        req.extensions_mut().insert(ClientAddr(loopback_peer()));
        let response = app(manager).oneshot(req).await.expect("router response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        names_providers("a 405", response.headers());
        std::fs::remove_file(&path).ok();
    }

    // The gate's refusal is unstamped, and names no providers either.
    let (manager, path) = control_manager("add-providers-gate", None);
    let (status, headers, _) = add_request(
        Arc::clone(&manager),
        Some(remote_peer()),
        None,
        r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        headers.get(ENDPOINT_HEADER).is_none() && headers.get(PROVIDERS_HEADER).is_none(),
        "a refusal must not confirm the route exists: {headers:?}"
    );
    std::fs::remove_file(&path).ok();
}

/// The 200's `provider` is the stored row's, read back from the live rotation, on
/// both of the route's arms: a Codex add comes back `codex`, an Anthropic one
/// `anthropic`, and so does an update of an existing row. This is what
/// `oauth::finish_login_account` checks a Codex import against.
#[tokio::test]
async fn the_add_route_echoes_the_provider_it_stored() {
    let (manager, path) = control_manager("add-echo", None);
    for (body, want, added) in [
        (
            r#"{"name":"bob@example.com/codex","provider":"codex","accessToken":"at-codex",
                    "accountUuid":"22222222-2222-4222-8222-222222222222"}"#,
            Provider::Codex,
            true,
        ),
        (
            r#"{"name":"carol@example.com","accessToken":"at-carol"}"#,
            Provider::Anthropic,
            true,
        ),
        (
            r#"{"name":"bob@example.com/codex","provider":"codex","accessToken":"at-codex-2",
                    "accountUuid":"22222222-2222-4222-8222-222222222222"}"#,
            Provider::Codex,
            false,
        ),
    ] {
        let (status, _headers, bytes) =
            add_request(Arc::clone(&manager), Some(loopback_peer()), None, body).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let payload: AddAccountResponse =
            serde_json::from_slice(&bytes).expect("an account-add payload");
        assert_eq!(payload.added, added, "{}: which arm answered", payload.name);
        assert_eq!(payload.provider, Some(want), "{}", payload.name);
        assert_eq!(
            manager.provider(payload.index),
            Some(want),
            "the echo is the stored row's provider"
        );
    }
    std::fs::remove_file(&path).ok();
}

/// SEAM TEST. `oauth::probe_add_route` (unit 3)
/// decides whether `tcr login` takes the LIVE route by `POSTing` this exact
/// deliberately-blank body to `ADD_ACCOUNT_PATH` via
/// `cli::post_add_account` and reading whether the reply is a STAMPED
/// 400. An unstamped answer, or a different (non-400) status, reads as
/// `AddCapability::Unusable` — `login_route` now REFUSES outright rather
/// than falling back to the whole-file `config::save` path (bridge item
/// D): a needless refusal beside a server that has the route, not the
/// single-use-refresh-token clobber this whole feature exists to remove.
/// Worst of all would be a 200 that actually appended a blank-named
/// account to the live fleet: that reads as `AddCapability::Present`
/// instead — a probe that is supposed to always fail silently succeeding
/// and mutating the live server.
///
/// Driven through the REAL production stack — a real `TcpListener` and
/// `crate::mitm::serve`, exactly how `main()` invokes it — not a
/// hand-built router, so a change to route registration or middleware
/// ordering is covered here too, not only by the in-process `oneshot`
/// tests above. Complements (does not replace) unit 3's own
/// `probe_add_capability_reads_a_stamped_400_as_present` (oauth.rs) and
/// `post_add_account_reads_a_stamped_400_as_rejected` (cli.rs), which
/// this repo's cross-file boundary keeps this route from editing.
#[tokio::test]
async fn probe_add_capabilitys_blank_body_is_a_stamped_400_and_mutates_nothing() {
    let (manager, path) = control_manager("seam-probe", None);
    let before = manager.snapshot(OffsetDateTime::now_utc()).accounts.len();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind seam-probe listener");
    let addr = listener.local_addr().expect("listener addr");
    let serving = Arc::clone(&manager);
    tokio::spawn(async move { crate::mitm::serve(listener, serving, None).await });

    let client = crate::test_support::loopback::client();
    // The EXACT `Account` `oauth::probe_add_route` builds:
    // a blank name, a blank access token, everything else absent.
    // DERIVED, not a restated literal — a future edit to the probe's body
    // (oauth.rs, out of this route's reach) changes this test's request
    // too, instead of leaving a stale literal green while it tests a body
    // nobody sends. Sent via `.json()` exactly like `post_add_account`
    // sends it (`cli.rs:478`, `.json(account)`).
    let probe_body = Account {
        name: String::new(),
        account_type: config::AccountType::OAuth,
        provider: Default::default(),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        priority: None,
        switch_threshold: None,
        disabled: None,
        groups: None,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
        egress: config::Egress::Local,
        egress_strict: false,
        codex_home: None,
        extra: serde_json::Map::new(),
    };
    let resp = client
        .post(format!("http://{addr}{ADD_ACCOUNT_PATH}"))
        .json(&probe_body)
        .send()
        .await
        .expect("send the probe's blank body");

    assert_eq!(
        resp.status().as_u16(),
        400,
        "the probe body must answer a local 400 — never forwarded, never a \
             soft-fail status the probe would misclassify"
    );
    assert_eq!(
        resp.headers()
            .get(ENDPOINT_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(ADD_ACCOUNT_ENDPOINT),
        "unstamped and `probe_add_capability` reads this as Unusable — `login_route` \
             now refuses outright instead of falling back to the whole-file config::save \
             path with a live server running"
    );

    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts.len(),
        before,
        "the probe's deliberately-blank body must never reach add_or_update_account"
    );
    assert_eq!(account_count_in_file(&path), before, "…nor the disk");

    std::fs::remove_file(&path).ok();
}
