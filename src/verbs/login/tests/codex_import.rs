use super::*;

// --- Codex import (`tcr login --from-codex`) ------------------------------

const CODEX_ACCOUNT_A: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_ACCOUNT_B: &str = "22222222-2222-4222-8222-222222222222";
/// The ChatGPT user of alice's login, in workspace [`CODEX_ACCOUNT_A`].
const CODEX_USER_A: &str = "55555555-5555-4555-8555-555555555555";

/// A config holding exactly these rows, each `(name, provider, account id)`.
fn config_with_rows(rows: &[(&str, crate::base::provider::Provider, Option<&str>)]) -> Config {
    let mut config: Config = serde_json::from_str("{}").unwrap();
    for (name, provider, account_id) in rows {
        config.accounts.push(crate::base::identity::probe(
            *provider,
            name,
            account_id.map(str::to_string),
            None,
            None,
        ));
    }
    config
}

#[test]
fn mint_codex_login_name_prefers_email_then_suffixes_codex_when_an_anthropic_row_holds_it() {
    use crate::base::provider::Provider;
    let empty = config_with_rows(&[]);
    assert_eq!(
        mint_codex_login_name(
            &empty,
            Some("alice@example.com"),
            Some(CODEX_ACCOUNT_A),
            None,
            None
        )
        .unwrap(),
        "alice@example.com"
    );
    let anthropic = config_with_rows(&[("alice@example.com", Provider::Anthropic, None)]);
    assert_eq!(
        mint_codex_login_name(
            &anthropic,
            Some("alice@example.com"),
            Some(CODEX_ACCOUNT_A),
            None,
            None
        )
        .unwrap(),
        "alice@example.com/codex"
    );
}

/// A re-import of the same Codex login updates its row in place, so it keeps
/// the name that row has: the bare email, or the suffixed one when an
/// Anthropic row holds the email.
#[test]
fn mint_codex_login_name_reuses_an_existing_codex_row_name() {
    use crate::base::provider::Provider;
    let codex = config_with_rows(&[("alice@example.com", Provider::Codex, Some(CODEX_ACCOUNT_A))]);
    assert_eq!(
        mint_codex_login_name(
            &codex,
            Some("alice@example.com"),
            Some(CODEX_ACCOUNT_A),
            None,
            None
        )
        .unwrap(),
        "alice@example.com"
    );
    let beside = config_with_rows(&[
        ("alice@example.com", Provider::Anthropic, None),
        (
            "alice@example.com/codex",
            Provider::Codex,
            Some(CODEX_ACCOUNT_A),
        ),
    ]);
    assert_eq!(
        mint_codex_login_name(
            &beside,
            Some("alice@example.com"),
            Some(CODEX_ACCOUNT_A),
            None,
            None
        )
        .unwrap(),
        "alice@example.com/codex"
    );
}

#[test]
fn mint_codex_login_name_requires_a_name_when_the_token_has_no_email() {
    let empty = config_with_rows(&[]);
    let err = mint_codex_login_name(&empty, None, Some(CODEX_ACCOUNT_A), None, None)
        .expect_err("no email and no --name is no name");
    assert!(err.to_string().contains("--name"), "{err}");
    assert_eq!(
        mint_codex_login_name(&empty, None, Some(CODEX_ACCOUNT_A), None, Some("work")).unwrap(),
        "work"
    );
}

/// A second ChatGPT account under the same email (a personal and a Team
/// workspace) is a second row, so it needs a name of its own: the write ADDS
/// it, and a row named like the first would be a duplicate name.
#[test]
fn mint_codex_login_name_gives_another_codex_account_of_the_same_email_its_own_name() {
    use crate::base::provider::Provider;
    let first = config_with_rows(&[("alice@example.com", Provider::Codex, Some(CODEX_ACCOUNT_A))]);
    assert_eq!(
        mint_codex_login_name(
            &first,
            Some("alice@example.com"),
            Some(CODEX_ACCOUNT_B),
            None,
            None
        )
        .unwrap(),
        "alice@example.com/codex"
    );
    let suffixed = config_with_rows(&[
        ("alice@example.com", Provider::Anthropic, None),
        (
            "alice@example.com/codex",
            Provider::Codex,
            Some(CODEX_ACCOUNT_A),
        ),
    ]);
    assert_eq!(
        mint_codex_login_name(
            &suffixed,
            Some("alice@example.com"),
            Some(CODEX_ACCOUNT_B),
            None,
            None
        )
        .unwrap(),
        "alice@example.com/codex-2"
    );
}

/// `--name` must be free: another account's name, of either provider, is
/// refused rather than shared. The name of this login's own row is not
/// another account's, so a re-import may name the row it updates.
#[test]
fn mint_codex_login_name_refuses_a_name_another_account_holds() {
    use crate::base::provider::Provider;
    let config = config_with_rows(&[
        ("work", Provider::Anthropic, None),
        ("team", Provider::Codex, Some(CODEX_ACCOUNT_B)),
    ]);
    for taken in ["work", "team"] {
        let err = mint_codex_login_name(
            &config,
            Some("alice@example.com"),
            Some(CODEX_ACCOUNT_A),
            None,
            Some(taken),
        )
        .expect_err("another account's name must be refused");
        assert!(
            err.to_string()
                .contains("already the name of another account"),
            "{taken}: {err}"
        );
    }
    let own = config_with_rows(&[("work", Provider::Codex, Some(CODEX_ACCOUNT_A))]);
    assert_eq!(
        mint_codex_login_name(
            &own,
            Some("alice@example.com"),
            Some(CODEX_ACCOUNT_A),
            None,
            Some("work")
        )
        .unwrap(),
        "work"
    );
}

/// A Codex import over the LIVE route reaches the running proxy as a Codex
/// account, and lands beside the Anthropic row that holds its email, never on
/// it. That Anthropic row even carries the Codex login's own account id, the
/// strongest pull toward a match there is, so only the provider keeps the two
/// apart: this fails if the provider is lost anywhere on the way, in the row
/// the import builds, in the request body or at the route.
///
/// Watched red: let `proxy::AddAccountRequest` drop `provider` and the server
/// files the import as an Anthropic account, resolves it onto alice's row by
/// the shared id and overwrites alice's credential with the Codex one.
#[tokio::test]
async fn a_codex_import_over_the_live_route_lands_as_codex_beside_the_anthropic_row() {
    use crate::base::provider::Provider;
    let (listener, port) = crate::test_support::loopback::listener().await;
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-codex-live-{}-{port}.json",
        std::process::id()
    ));
    let seed = format!(
        r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accountUuid": "{CODEX_USER_A}",
                  "accessToken": "at-alice", "refreshToken": "rt-alice",
                  "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
    );
    std::fs::write(&path, &seed).unwrap();
    let manager = serve_live_proxy(listener, &seed, Some(path.clone()));

    let login = crate::clients::codex_login::CodexLogin {
        access_token: "at-codex".to_string(),
        refresh_token: "rt-codex".to_string(),
        account_id: Some(CODEX_ACCOUNT_A.to_string()),
        user_id: Some(CODEX_USER_A.to_string()),
        email: Some("alice@example.com".to_string()),
        plan_type: Some("pro".to_string()),
        expires_at_ms: Some(1_800_000_000_000),
        is_fedramp: false,
    };
    let config = load_or_default(&path).unwrap();
    let name = mint_codex_login_name(
        &config,
        login.email.as_deref(),
        login.user_id.as_deref(),
        login.account_id.as_deref(),
        None,
    )
    .unwrap();
    assert_eq!(name, "alice@example.com/codex");
    let saved = finish_login_account(
        &path,
        WriteRoute::Live,
        &config,
        codex_account_row(name, login, None),
    )
    .await
    .unwrap();
    assert_eq!(saved, "alice@example.com/codex");

    // The running proxy: the import is a Codex account of its own, and alice
    // is still an Anthropic account holding her own credential.
    assert_eq!(manager.account_count(), 2, "added beside, not onto, alice");
    let index_of = |name: &str| {
        (0..manager.account_count())
            .find(|&i| manager.account_name(i).as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no live account named {name}"))
    };
    let codex = index_of("alice@example.com/codex");
    assert_eq!(manager.provider(codex), Some(Provider::Codex));
    assert_eq!(manager.access_token(codex).as_deref(), Some("at-codex"));
    let alice = index_of("alice@example.com");
    assert_eq!(manager.provider(alice), Some(Provider::Anthropic));
    assert_eq!(manager.access_token(alice).as_deref(), Some("at-alice"));

    // The file the server persisted: the same two rows.
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let rows = doc["accounts"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{doc}");
    assert_eq!(rows[0]["name"], "alice@example.com");
    assert_eq!(rows[0]["accessToken"], "at-alice");
    assert!(
        rows[0].get("provider").is_none(),
        "alice stays Anthropic: {doc}"
    );
    assert_eq!(rows[1]["name"], "alice@example.com/codex");
    assert_eq!(rows[1]["provider"], "codex");
    assert_eq!(rows[1]["accessToken"], "at-codex");
    assert_eq!(rows[1]["accountUuid"], CODEX_USER_A, "the ChatGPT user");
    assert_eq!(rows[1]["orgUuid"], CODEX_ACCOUNT_A, "the workspace");

    std::fs::remove_file(&path).ok();
}

// --- import_codex_login: the provider gate, the echo, the re-import rules --

/// Import alice's Codex login (bearer `at-codex`) into the config at `path` with no flags:
/// no `--force`, no name, no `--account`, no `CODEX_HOME`.
async fn import_alices_login(path: &Path) -> anyhow::Result<String> {
    import_codex_login(
        path,
        false,
        None,
        None,
        alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
        None,
    )
    .await
}

/// alice's Codex login for account A, expiring at `expires_at_ms`, with these
/// fake tokens.
fn alice_codex_login(
    expires_at_ms: i64,
    access_token: &str,
    refresh_token: &str,
) -> crate::clients::codex_login::CodexLogin {
    crate::clients::codex_login::CodexLogin {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        account_id: Some(CODEX_ACCOUNT_A.to_string()),
        user_id: Some(CODEX_USER_A.to_string()),
        email: Some("alice@example.com".to_string()),
        plan_type: Some("pro".to_string()),
        expires_at_ms: Some(expires_at_ms),
        is_fedramp: false,
    }
}

/// A config file in a fresh tempdir whose proxy is on `port`, whose `codexUpstream` is
/// `codex_upstream` and whose `accounts` array is `accounts` (JSON).
///
/// An import that gets past its local checks asks `codexUpstream` where the login's
/// workspace is served before it writes, so every test names a loopback stub here
/// ([`accounts_check_stub`]) and none reaches chatgpt.com.
fn import_config(
    port: u16,
    codex_upstream: &str,
    accounts: &str,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    std::fs::write(
        &path,
        format!(
            r#"{{ "proxy": {{ "port": {port} }}, "codexUpstream": "{codex_upstream}", "accounts": {accounts} }}"#
        ),
    )
    .unwrap();
    (dir, path)
}

/// One request [`accounts_check_stub`] received: its path, and the bearer and ChatGPT
/// account id it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenCheck {
    path: String,
    bearer: Option<String>,
    account_id: Option<String>,
}

/// A stub `codexUpstream` on 127.0.0.1:0 that answers every request `status` with `body`
/// and records it. Returns its base URL and the record.
async fn accounts_check_stub(
    status: u16,
    body: &str,
) -> (String, Arc<std::sync::Mutex<Vec<SeenCheck>>>) {
    recording_stub(
        axum::http::StatusCode::from_u16(status).unwrap(),
        axum::http::header::CONTENT_TYPE,
        "application/json",
        body,
    )
}

/// A stub that answers every request `status` with the one header `name: value` and
/// `body`, and records each request as a [`SeenCheck`]. Returns its base URL and the record.
fn recording_stub(
    status: axum::http::StatusCode,
    name: axum::http::HeaderName,
    value: &str,
    body: &str,
) -> (String, Arc<std::sync::Mutex<Vec<SeenCheck>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let (value, body) = (value.to_string(), body.to_string());
    let origin = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let recorded = Arc::clone(&recorded);
        let (name, value, body) = (name.clone(), value.clone(), body.clone());
        async move {
            let header = |name: &str| {
                req.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            recorded.lock().unwrap().push(SeenCheck {
                path: req.uri().path().to_string(),
                bearer: header("authorization"),
                account_id: header(crate::base::provider::CODEX_ACCOUNT_HEADER),
            });
            (status, [(name, value)], body)
        }
    });
    let addr = crate::test_support::stub_upstream::http(origin);
    (format!("http://{addr}"), seen)
}

/// An accounts/check answer naming account A's workspace with these two routing values.
fn workspace_answer(origin: &str, routing: &str) -> String {
    format!(
        r#"{{"accounts":{{"{CODEX_ACCOUNT_A}":{{"workspace_backend_origin":"{origin}","account_routing_override":"{routing}","plan_type":"pro"}}}},"account_ordering":["{CODEX_ACCOUNT_A}"]}}"#
    )
}

/// A stub `codexUpstream` that clears account A's workspace for the pool.
async fn unconstrained_upstream() -> String {
    accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT"))
        .await
        .0
}

/// The one request an import sends `codexUpstream`: the workspace check, with the login's
/// own bearer and account id.
fn workspace_check_of(bearer: &str) -> SeenCheck {
    SeenCheck {
        path: crate::clients::codex_login::ACCOUNTS_CHECK_PATH.to_string(),
        bearer: Some(format!("Bearer {bearer}")),
        account_id: Some(CODEX_ACCOUNT_A.to_string()),
    }
}

/// A stub of the account-add route on 127.0.0.1:0, recording every request
/// body it receives. Every answer is stamped with the route's
/// `ENDPOINT_HEADER`, as every tcr with the route stamps them; `advertise`
/// decides whether it also names its providers (a current server) or not (a
/// server that predates them). The blank probe gets the route's 400; a real
/// add gets a 200 whose `provider` is `echo` (absent when `None`, as an older
/// server's body is).
async fn stub_add_route(
    advertise: bool,
    echo: Option<crate::base::provider::Provider>,
) -> (u16, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
    use axum::response::IntoResponse as _;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let route = axum::Router::new().route(
        crate::proxy::ADD_ACCOUNT_PATH,
        axum::routing::post(move |body: axum::body::Bytes| {
            let recorded = Arc::clone(&recorded);
            async move {
                let doc: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                recorded.lock().unwrap().push(doc.clone());
                let mut headers = axum::http::HeaderMap::new();
                headers.insert(
                    crate::proxy::ENDPOINT_HEADER,
                    axum::http::HeaderValue::from_static(crate::proxy::ADD_ACCOUNT_ENDPOINT),
                );
                if advertise {
                    headers.insert(
                        crate::proxy::PROVIDERS_HEADER,
                        axum::http::HeaderValue::from_static("anthropic,codex"),
                    );
                }
                let name = doc["name"].as_str().unwrap_or_default().to_string();
                if name.is_empty() {
                    let refusal = r#"{"type":"error","error":{"type":"invalid_request_error","message":"\"name\" (string) is required and must not be empty."}}"#;
                    return (axum::http::StatusCode::BAD_REQUEST, headers, refusal)
                        .into_response();
                }
                let mut applied = serde_json::json!({
                    "name": name, "added": true, "index": 0, "persisted": true, "warning": null
                });
                if let Some(echo) = echo {
                    applied["provider"] = serde_json::json!(echo);
                }
                (axum::http::StatusCode::OK, headers, applied.to_string()).into_response()
            }
        }),
    );
    (serve_router(route).await.port(), seen)
}

/// THE GATE. A live proxy that answers the route but names no providers predates
/// Codex accounts: it would drop the row's `provider`, store this login as a Claude
/// account and send its tokens to Anthropic. The import refuses — `--force` included
/// — writes nothing, and posts nothing but the blank probe.
#[tokio::test]
async fn a_codex_import_is_refused_by_a_live_proxy_that_names_no_providers() {
    let (port, seen) = stub_add_route(false, None).await;
    let (_dir, path) = import_config(
        port,
        &unconstrained_upstream().await,
        r#"[{ "name": "alice@example.com", "accessToken": "at-alice" }]"#,
    );
    let before = std::fs::read(&path).unwrap();

    for force in [false, true] {
        let err = import_codex_login(
            &path,
            force,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .expect_err("a proxy that predates Codex accounts must refuse the import");
        let text = err.to_string();
        assert!(
            text.contains(&format!(":{port}"))
                && text.contains("predates Codex accounts")
                && text.contains("store this login as a Claude account")
                && text.contains("send its token to Anthropic")
                && text.contains("Restart the proxy on this build"),
            "force={force}: {text}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "force={force}: nothing may be written"
        );
    }
    let bodies = seen.lock().unwrap();
    assert_eq!(
        bodies.len(),
        2,
        "one probe per attempt and nothing else: {bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .all(|body| body["name"] == "" && body["accessToken"] == ""),
        "only the blank probe may reach it, never the login: {bodies:?}"
    );
}

/// The gate's other side, against the real server: it names `codex`, so the import
/// takes the live route and lands as a Codex account beside alice's Anthropic row,
/// in the running proxy and in the file it persists — at the credential version an
/// older proxy refuses.
#[tokio::test]
async fn a_codex_import_over_a_proxy_that_names_codex_lands_live_as_codex() {
    use crate::base::provider::Provider;
    let (listener, port) = crate::test_support::loopback::listener().await;
    let (_dir, path) = import_config(
        port,
        &unconstrained_upstream().await,
        &format!(
            r#"[{{ "name": "alice@example.com", "type": "oauth", "accountUuid": "{CODEX_ACCOUNT_A}",
                      "accessToken": "at-alice", "refreshToken": "rt-alice",
                      "expiresAt": 1893456000000, "priority": 0 }}]"#
        ),
    );
    let manager = serve_live_proxy(
        listener,
        &std::fs::read_to_string(&path).unwrap(),
        Some(path.clone()),
    );

    let saved = import_alices_login(&path)
        .await
        .expect("a server that names codex takes the import live");
    assert_eq!(saved, "alice@example.com/codex");

    let codex = (0..manager.account_count())
        .find(|&i| manager.account_name(i).as_deref() == Some("alice@example.com/codex"))
        .expect("the import is in the live rotation");
    assert_eq!(manager.provider(codex), Some(Provider::Codex));
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let rows = doc["accounts"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{doc}");
    assert!(rows[0].get("provider").is_none(), "alice stays Anthropic");
    assert_eq!(rows[1]["provider"], "codex");
    assert_eq!(rows[1]["credentialVersion"], 2, "{doc}");
}

/// Behind the gate, the echo: a server that names `codex` but answers the add with
/// another provider, or none, stored the row as a Claude account. That is a hard
/// error telling the user to remove the row it names, never a success line.
#[tokio::test]
async fn a_live_add_not_echoed_as_codex_is_a_hard_error_naming_the_row_to_remove() {
    for echo in [None, Some(crate::base::provider::Provider::Anthropic)] {
        let (port, seen) = stub_add_route(true, echo).await;
        let (_dir, path) = import_config(port, &unconstrained_upstream().await, "[]");

        let err = import_alices_login(&path)
            .await
            .expect_err("an add not confirmed as Codex must fail loudly");
        let text = err.to_string();
        assert!(
            text.contains("live, and on disk, as a Claude account")
                && text.contains("may already have reached Anthropic")
                && text.contains("tcr remove 'alice@example.com'")
                && text.contains("`codex login` again"),
            "echo {echo:?}: {text}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "echo {echo:?}: the add was posted, and the answer is what failed"
        );
    }
}

/// A re-import of a login already saved under another name than the one the
/// mint would pick (its email is free again) keeps the stored name — the write
/// updates that row in place — and a `--name` that is not that name is refused,
/// since the write would rename nothing.
#[tokio::test]
async fn a_re_import_keeps_the_name_its_row_is_saved_under() {
    let port = 0;
    let (_dir, path) = import_config(
        port,
        &unconstrained_upstream().await,
        &format!(
            r#"[{{ "name": "alice@example.com/codex", "provider": "codex",
                      "accountUuid": "{CODEX_USER_A}", "orgUuid": "{CODEX_ACCOUNT_A}", "accessToken": "at-old",
                      "refreshToken": "rt-old", "expiresAt": 1700000000000 }}]"#
        ),
    );

    let saved = import_codex_login(
        &path,
        false,
        None,
        None,
        alice_codex_login(1_800_000_000_000, "at-new", "rt-new"),
        None,
    )
    .await
    .expect("a re-import updates its row");
    assert_eq!(
        saved, "alice@example.com/codex",
        "the stored name, not the email"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let rows = doc["accounts"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{doc}");
    assert_eq!(rows[0]["name"], "alice@example.com/codex");
    assert_eq!(rows[0]["accessToken"], "at-new");

    let before = std::fs::read(&path).unwrap();
    let err = import_codex_login(
        &path,
        false,
        Some("work"),
        None,
        alice_codex_login(1_900_000_000_000, "at-newer", "rt-newer"),
        None,
    )
    .await
    .expect_err("a --name that is not the row's name renames nothing and is refused");
    assert!(
        err.to_string()
            .contains("this login is already saved as 'alice@example.com/codex'"),
        "{err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
}

/// Re-importing an older credential, such as one left after a skipped write-back, over tcr's newer one
/// would kill the account at its next refresh: refused, nothing written. A login
/// NEWER than tcr's (a fresh `codex login`) still replaces it.
#[tokio::test]
async fn a_re_import_of_an_older_credential_is_refused_and_writes_nothing() {
    let port = 0;
    let (_dir, path) = import_config(
        port,
        &unconstrained_upstream().await,
        &format!(
            r#"[{{ "name": "alice@example.com", "provider": "codex",
                      "accountUuid": "{CODEX_USER_A}", "orgUuid": "{CODEX_ACCOUNT_A}", "accessToken": "at-live",
                      "refreshToken": "rt-old", "expiresAt": 1900000000000 }}]"#
        ),
    );
    let before = std::fs::read(&path).unwrap();

    let err = import_codex_login(
        &path,
        false,
        None,
        None,
        alice_codex_login(1_800_000_000_000, "at-stale", "rt-stale"),
        None,
    )
    .await
    .expect_err("an older credential must not overwrite tcr's newer one");
    assert!(
        err.to_string()
            .contains("tcr already holds a newer credential for this account"),
        "{err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");

    import_codex_login(
        &path,
        false,
        None,
        None,
        alice_codex_login(2_000_000_000_000, "at-fresh", "rt-fresh"),
        None,
    )
    .await
    .expect("a newer login still replaces tcr's");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["accounts"][0]["refreshToken"], "rt-fresh");
}

// --- import_codex_login: `--account` ----------------------------------------

/// What a recording stub has been asked, one entry per request.
type Asked<T> = Arc<std::sync::Mutex<Vec<T>>>;

/// A live add route that advertises Codex and a `codexUpstream` whose workspace check
/// clears account A, both recording what they are asked: the route's port and record, then
/// the upstream's base URL and record.
async fn recording_route_and_clearing_upstream()
-> (u16, Asked<serde_json::Value>, String, Asked<SeenCheck>) {
    let (port, probes) = stub_add_route(true, Some(crate::base::provider::Provider::Codex)).await;
    let (upstream, checks) =
        accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT")).await;
    (port, probes, upstream, checks)
}

/// Neither the add route nor `codexUpstream` was asked anything.
fn assert_neither_stub_was_asked(probes: &Asked<serde_json::Value>, checks: &Asked<SeenCheck>) {
    assert!(
        probes.lock().unwrap().is_empty(),
        "not even the route probe: {:?}",
        probes.lock().unwrap()
    );
    assert!(
        checks.lock().unwrap().is_empty(),
        "no workspace check: {:?}",
        checks.lock().unwrap()
    );
}

/// `--account` names the login the user means to import, so a `CODEX_HOME` that holds
/// another is refused before anything else is asked, naming the account, and nothing is
/// written. The add route and `codexUpstream` are stubs that record what they are asked, and
/// neither is asked anything.
///
/// Watched red: without the guard, alice's login is imported over the live route.
#[tokio::test]
async fn an_import_for_another_account_is_refused_and_writes_nothing() {
    let (port, probes, upstream, checks) = recording_route_and_clearing_upstream().await;
    let (_dir, path) = import_config(port, &upstream, "[]");
    let before = std::fs::read(&path).unwrap();

    let err = import_codex_login(
        &path,
        false,
        None,
        Some("bob@example.com"),
        alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
        None,
    )
    .await
    .expect_err("alice's login must not be imported as bob");
    let text = err.to_string();
    assert!(
        text.contains("is not 'bob@example.com'") && text.contains("nothing was written"),
        "{text}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    assert_neither_stub_was_asked(&probes, &checks);
}

/// The control: `--account` naming the login's own email imports it, as any import does.
///
/// Watched red: with the guard refusing the account it should accept, nothing is written.
#[tokio::test]
async fn an_import_for_its_own_account_is_written() {
    let (_dir, path) = import_config(0, &unconstrained_upstream().await, "[]");

    let saved = import_codex_login(
        &path,
        false,
        None,
        Some("alice@example.com"),
        alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
        None,
    )
    .await
    .expect("alice's own login is imported");
    assert_eq!(saved, "alice@example.com");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["accounts"][0]["name"], "alice@example.com", "{doc}");
    assert_eq!(doc["accounts"][0]["provider"], "codex", "{doc}");
}

/// `--account` naming a Codex row refuses a login that does not name both its ChatGPT
/// user and its workspace: such a login cannot be shown to be that row's account. The
/// identity check fell back to comparing names, and its probe row carried the target row's
/// own name, so the check passed for any such login and the import wrote a row for whoever
/// it was. It is refused like any other login that is not the row's, before the route probe
/// and the workspace check, with nothing written. `--codex` refuses the same login.
///
/// Watched red: without the check, carol's login with no user id is imported beside bob's
/// row, as `carol@example.com`.
#[tokio::test]
async fn an_account_import_refuses_a_login_that_names_no_user_or_workspace() {
    let (port, probes, upstream, checks) = recording_route_and_clearing_upstream().await;
    let (_dir, path) = import_config(
        port,
        &upstream,
        &format!(
            r#"[{{ "name": "bob@example.com/codex", "provider": "codex",
                      "accountUuid": "{CODEX_USER_A}", "orgUuid": "{CODEX_ACCOUNT_A}",
                      "accessToken": "at-bob", "refreshToken": "rt-bob",
                      "expiresAt": 1700000000000 }}]"#
        ),
    );
    let before = std::fs::read(&path).unwrap();
    let carol = || crate::clients::codex_login::CodexLogin {
        email: Some("carol@example.com".to_string()),
        ..alice_codex_login(1_800_000_000_000, "at-carol", "rt-carol")
    };
    for (what, login) in [
        (
            "no user",
            crate::clients::codex_login::CodexLogin {
                user_id: None,
                ..carol()
            },
        ),
        (
            "no workspace",
            crate::clients::codex_login::CodexLogin {
                account_id: None,
                ..carol()
            },
        ),
    ] {
        let err = import_codex_login(
            &path,
            false,
            None,
            Some("bob@example.com/codex"),
            login,
            None,
        )
        .await
        .expect_err(what);
        assert_eq!(
            err.to_string(),
            "the Codex login on this machine is not 'bob@example.com/codex'; nothing was \
             written.",
            "{what}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{what}: nothing was written"
        );
    }
    assert_neither_stub_was_asked(&probes, &checks);
}

// --- import_codex_login: logins tcr must not pool -----------------------------

/// A FedRAMP login is refused first: offline, before the route probe and the workspace
/// check, with nothing written, and `--force` does not get past it. The add route and
/// `codexUpstream` are both stubs that record what they are asked, and neither is asked
/// anything.
///
/// Watched red twice: without the refusal the import succeeds, and with it moved after the
/// route probe the route stub records the blank probe.
#[tokio::test]
async fn a_fedramp_login_is_refused_before_anything_else() {
    let (port, probes, upstream, checks) = recording_route_and_clearing_upstream().await;
    let (_dir, path) = import_config(port, &upstream, "[]");
    let before = std::fs::read(&path).unwrap();
    let mut login = alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex");
    login.is_fedramp = true;

    let err = import_codex_login(&path, true, None, None, login, None)
        .await
        .expect_err("a FedRAMP login must not be imported");
    let text = err.to_string();
    assert!(
        text.contains("FedRAMP")
            && text.contains("chatgpt_account_is_fedramp")
            && text.contains("Nothing was written"),
        "{text}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    assert_neither_stub_was_asked(&probes, &checks);
}

/// A login whose workspace is served anywhere but chatgpt.com, by another backend origin or
/// a residency override, is refused naming what the workspace check said, after one check
/// carrying the imported bearer and account id, and nothing is written.
///
/// Watched red: without the check, the import writes the row.
#[tokio::test]
async fn a_routed_workspace_is_refused_naming_its_routing_and_writes_nothing() {
    for (origin, routing, named) in [
        (
            "https://gov.chatgpt.com",
            "NO_CONSTRAINT",
            "https://gov.chatgpt.com",
        ),
        ("NO_CONSTRAINT", "us", "\"us\""),
    ] {
        let (upstream, checks) = accounts_check_stub(200, &workspace_answer(origin, routing)).await;
        let (_dir, path) = import_config(0, &upstream, "[]");
        let before = std::fs::read(&path).unwrap();

        let err = import_alices_login(&path)
            .await
            .expect_err("a routed workspace must not be imported");
        let text = err.to_string();
        assert!(
            text.contains(named)
                && text.contains("not served at https://chatgpt.com")
                && text.contains("Nothing was written"),
            "{routing}: {text}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
        assert_eq!(
            *checks.lock().unwrap(),
            [workspace_check_of("at-codex")],
            "one check, with the imported bearer"
        );
    }
}

/// The same refusal over the live route, where an add would put the login in the running
/// proxy's pool at once: the workspace check runs there too, before the add. The route stub
/// sees only the blank probe, the check stub sees one check with the imported bearer, and
/// nothing is written.
///
/// Watched red: with the check run only on the file route, the login is added live.
#[tokio::test]
async fn a_routed_workspace_is_refused_over_the_live_route_and_never_added() {
    let (port, probes) = stub_add_route(true, Some(crate::base::provider::Provider::Codex)).await;
    let (upstream, checks) = accounts_check_stub(
        200,
        &workspace_answer("https://gov.chatgpt.com", "NO_CONSTRAINT"),
    )
    .await;
    let (_dir, path) = import_config(port, &upstream, "[]");
    let before = std::fs::read(&path).unwrap();

    let err = import_alices_login(&path)
        .await
        .expect_err("a routed workspace must not be imported over the live route");
    let text = err.to_string();
    assert!(
        text.contains("https://gov.chatgpt.com")
            && text.contains("not served at https://chatgpt.com")
            && text.contains("Nothing was written"),
        "{text}"
    );
    let bodies = probes.lock().unwrap().clone();
    assert!(
        bodies.len() == 1 && bodies[0]["name"] == "" && bodies[0]["accessToken"] == "",
        "the blank probe and no add: {bodies:?}"
    );
    assert_eq!(
        *checks.lock().unwrap(),
        [workspace_check_of("at-codex")],
        "one check, with the imported bearer"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
}

/// A workspace check that cannot complete refuses the import, naming why, with nothing
/// written: an error status, an answer that is not JSON or names no routing, and an
/// upstream nobody listens on.
///
/// Watched red: without the check, every one of these imports writes the row.
#[tokio::test]
async fn a_workspace_check_that_cannot_complete_refuses_the_import() {
    let dead = "http://127.0.0.1:0".to_string();
    let mut upstreams = vec![(dead, "could not connect".to_string())];
    for (status, body, why) in [
        (500, "{}", "answered HTTP 500"),
        (401, "{}", "answered HTTP 401"),
        (200, "not json", "not JSON"),
        (
            200,
            r#"{"accounts":{}}"#,
            "names no workspace_backend_origin",
        ),
    ] {
        let (upstream, _checks) = accounts_check_stub(status, body).await;
        upstreams.push((upstream, why.to_string()));
    }
    for (upstream, why) in upstreams {
        let (_dir, path) = import_config(0, &upstream, "[]");
        let before = std::fs::read(&path).unwrap();

        let err = import_alices_login(&path)
            .await
            .expect_err("an unconfirmed workspace must not be imported");
        let text = err.to_string();
        assert!(
            text.contains("could not confirm")
                && text.contains(&why)
                && text.contains("Nothing was written"),
            "{why}: {text}"
        );
        assert!(
            !text.contains("at-codex"),
            "the bearer never appears: {text}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    }
}

/// A stub `codexUpstream` on 127.0.0.1:0 that answers every request `302 Found` with
/// `Location: location`, and records it as [`accounts_check_stub`] does.
async fn redirecting_stub(location: &str) -> (String, Arc<std::sync::Mutex<Vec<SeenCheck>>>) {
    recording_stub(
        axum::http::StatusCode::FOUND,
        axum::http::header::LOCATION,
        location,
        "",
    )
}

/// A workspace check answered with a redirect refuses the import, and the redirect is never
/// followed: the check carries the imported bearer and account id, so it goes to
/// `codexUpstream` and nowhere else. Here `codexUpstream` answers 302 to a second loopback
/// origin whose answer would clear the login; that origin is asked nothing, and nothing is
/// written.
///
/// Watched red: on a client that follows redirects, the second origin is asked, with the
/// account id though without the bearer, and its answer imports the login.
#[tokio::test]
async fn a_redirected_workspace_check_refuses_the_import_and_is_never_followed() {
    let (elsewhere, redirected) =
        accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT")).await;
    let (upstream, checks) = redirecting_stub(&format!("{elsewhere}/elsewhere")).await;
    let (_dir, path) = import_config(0, &upstream, "[]");
    let before = std::fs::read(&path).unwrap();

    let err = import_alices_login(&path)
        .await
        .expect_err("a redirected workspace check must not clear the login");
    let text = err.to_string();
    assert!(
        text.contains("answered HTTP 302") && text.contains("Nothing was written"),
        "{text}"
    );
    assert!(
        redirected.lock().unwrap().is_empty(),
        "the redirect's target is asked nothing: {:?}",
        redirected.lock().unwrap()
    );
    assert_eq!(
        *checks.lock().unwrap(),
        [workspace_check_of("at-codex")],
        "one check, to codexUpstream"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
}

/// A workspace served at chatgpt.com is imported, after exactly one check that carried the
/// imported bearer and account id to `codexUpstream`.
///
/// Watched red: without the check, the stub is asked nothing.
#[tokio::test]
async fn an_unconstrained_workspace_is_imported_after_one_check_with_the_imported_bearer() {
    let (upstream, checks) = accounts_check_stub(
        200,
        &workspace_answer("https://chatgpt.com", "NO_CONSTRAINT"),
    )
    .await;
    let (_dir, path) = import_config(0, &upstream, "[]");

    let saved = import_alices_login(&path)
        .await
        .expect("a workspace served at chatgpt.com is imported");
    assert_eq!(saved, "alice@example.com");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["accounts"][0]["provider"], "codex", "{doc}");
    assert_eq!(*checks.lock().unwrap(), [workspace_check_of("at-codex")]);
}

/// The first-run race the macOS e2e caught: this process saw an empty
/// fleet when its verb started, and by the time the import reads the
/// config, another `tcr` process (the server booting) has written its
/// import there. The automatic import must stop on that read, not mint
/// `unnamed-2` beside the other process's `unnamed`. Returns before the
/// profile fetch, so no network.
#[tokio::test]
async fn first_run_import_stops_when_another_process_imported_first() {
    let dead_port = DEAD_PORT;
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-import-race-{}-{dead_port}.json",
        std::process::id()
    ));
    std::fs::write(
        &path,
        format!(
            r#"{{ "proxy": {{ "port": {dead_port} }}, "accounts": [
                     {{ "name": "unnamed", "accessToken": "at-fake-first", "refreshToken": "rt-fake-first" }}
                   ] }}"#
        ),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let login = crate::clients::claude_code_login::ClaudeCodeLogin {
        access_token: "at-fake-second".to_string(),
        refresh_token: Some("rt-fake-second".to_string()),
        expires_at: Some(4_102_444_800_000),
        subscription_type: Some("max".to_string()),
    };

    let outcome = import_claude_code_login(
        &path,
        ImportInto::EmptyFleetOnly,
        false,
        None,
        None,
        login,
        LoginUi::Machine,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome, None,
        "the automatic import must skip a fleet that is no longer empty"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the config must be left byte-for-byte alone"
    );
    std::fs::remove_file(&path).ok();
}

// --- refusals a GUI titles by their code -----------------------------------------

/// The three import refusals carry their codes, read back through the error a caller holds,
/// and read exactly as they always did: the routed, unconfirmed and FedRAMP tests above
/// still pass on `err.to_string()`.
///
/// Watched red: with the refusals back to plain `bail!`s, no `LoginRefusal` is in the chain.
#[tokio::test]
async fn the_codex_import_refusals_carry_their_codes() {
    let code_of = |err: &anyhow::Error| match err.downcast_ref::<LoginRefusal>() {
        Some(refusal) => refusal.code,
        None => panic!("no coded refusal in: {err:#}"),
    };

    let (_dir, path) = import_config(0, &unconstrained_upstream().await, "[]");
    let mut login = alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex");
    login.is_fedramp = true;
    let err = import_codex_login(&path, false, None, None, login, None)
        .await
        .unwrap_err();
    assert_eq!(code_of(&err), "codex-fedramp");
    assert!(err.to_string().contains("FedRAMP"), "{err}");

    let (upstream, _) = accounts_check_stub(
        200,
        &workspace_answer("https://gov.chatgpt.com", "NO_CONSTRAINT"),
    )
    .await;
    let (_dir, path) = import_config(0, &upstream, "[]");
    let err = import_alices_login(&path).await.unwrap_err();
    assert_eq!(code_of(&err), "codex-routed");
    assert!(
        err.to_string()
            .contains("not served at https://chatgpt.com"),
        "{err}"
    );

    let (upstream, _) = accounts_check_stub(500, "{}").await;
    let (_dir, path) = import_config(0, &upstream, "[]");
    let err = import_alices_login(&path).await.unwrap_err();
    assert_eq!(code_of(&err), "codex-unconfirmed");
    assert!(err.to_string().contains("could not confirm"), "{err}");
}

/// `--from-codex --account` takes the row name `tcr accounts` prints, as every other verb's
/// `--account` does, and still takes the login's email. A name that is a Claude row is
/// refused before the route probe and any write: no Codex import re-signs it. A name that is
/// no row keeps the email check and its words.
///
/// Watched red: comparing the email alone refuses the row name as "not
/// 'alice@example.com/codex'".
#[tokio::test]
async fn from_codex_account_accepts_the_row_name() {
    let (_dir, path) = import_config(
        0,
        &unconstrained_upstream().await,
        &format!(
            r#"[{{ "name": "alice@example.com/codex", "provider": "codex",
                      "accountUuid": "{CODEX_USER_A}", "orgUuid": "{CODEX_ACCOUNT_A}",
                      "accessToken": "at-old", "refreshToken": "rt-old",
                      "expiresAt": 1700000000000 }}]"#
        ),
    );
    let saved = import_codex_login(
        &path,
        false,
        None,
        Some("alice@example.com/codex"),
        alice_codex_login(1_800_000_000_000, "at-new", "rt-new"),
        None,
    )
    .await
    .expect("the row name is accepted");
    assert_eq!(saved, "alice@example.com/codex");
    let rows = |path: &Path| -> Vec<serde_json::Value> {
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        doc["accounts"].as_array().unwrap().clone()
    };
    let written = rows(&path);
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(written[0]["accessToken"], "at-new");

    let saved = import_codex_login(
        &path,
        false,
        None,
        Some("alice@example.com"),
        alice_codex_login(1_900_000_000_000, "at-newer", "rt-newer"),
        None,
    )
    .await
    .expect("the login's email is still accepted");
    assert_eq!(saved, "alice@example.com/codex");
    assert_eq!(rows(&path)[0]["accessToken"], "at-newer");

    let before = std::fs::read(&path).unwrap();
    let err = import_codex_login(
        &path,
        false,
        None,
        Some("erin@example.com"),
        alice_codex_login(2_000_000_000_000, "at-x", "rt-x"),
        None,
    )
    .await
    .expect_err("a name that is no row and not the login's email");
    assert_eq!(
        err.to_string(),
        "the Codex login on this machine is not 'erin@example.com'; nothing was written."
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");

    let (port, probes, upstream, checks) = recording_route_and_clearing_upstream().await;
    let (_dir, claude) = import_config(
        port,
        &upstream,
        r#"[{ "name": "dana@example.com", "accessToken": "at-dana" }]"#,
    );
    let before = std::fs::read(&claude).unwrap();
    let err = import_codex_login(
        &claude,
        false,
        None,
        Some("dana@example.com"),
        alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
        None,
    )
    .await
    .expect_err("a Claude row is never re-signed by a Codex import");
    let refusal = err.downcast_ref::<LoginRefusal>().expect("a coded refusal");
    assert_eq!(refusal.code, "account-is-claude");
    assert_eq!(
        refusal.reason,
        "'dana@example.com' is a Claude account; `tcr login --from-codex` imports Codex \
         accounts only. Sign it in again with `tcr login --account dana@example.com`. \
         Nothing was written."
    );
    assert_eq!(
        std::fs::read(&claude).unwrap(),
        before,
        "nothing was written"
    );
    assert!(
        probes.lock().unwrap().is_empty(),
        "not even the route probe"
    );
    assert!(checks.lock().unwrap().is_empty(), "no workspace check");
}
