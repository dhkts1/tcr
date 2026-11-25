use super::*;
use std::sync::Arc;

// --- loopback helpers: the only places this module's tests open a socket -------------

/// A port nothing listens on, so a connect to it is refused outright. Port 1 (tcpmux) is
/// never bound on a dev machine or CI, so no socket is opened to find one; this replaces a
/// bind-read-drop of a port-0 listener in each test that wanted a refusal.
const DEAD_PORT: u16 = 1;

/// Serve `app` on its own ephemeral loopback port for the rest of the test, and return the
/// address. Only for code that dials a bare PORT (a live proxy's `proxy.port`); a stub reached
/// by a base URL is a route on `test_support::stub_upstream` instead, with no port of its own.
async fn serve_router(app: axum::Router) -> std::net::SocketAddr {
    let (listener, port) = crate::test_support::loopback::listener().await;
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .log_debug("serving a test server");
    });
    std::net::SocketAddr::from(([127, 0, 0, 1], port))
}

/// [`crate::test_support::live_proxy::serve`] for the config the JSON `seed` holds.
fn serve_live_proxy(
    listener: TcpListener,
    seed: &str,
    config_path: Option<std::path::PathBuf>,
) -> Arc<crate::manager::Manager> {
    crate::test_support::live_proxy::serve(
        listener,
        serde_json::from_str(seed).unwrap(),
        config_path,
    )
}

/// A live proxy, with no config file, that admits only requests carrying `api_key`.
/// Answers its port.
async fn live_proxy_requiring_the_api_key(api_key: &str) -> u16 {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let seed = format!(r#"{{ "proxy": {{ "port": {port}, "apiKey": "{api_key}" }} }}"#);
    serve_live_proxy(listener, &seed, None);
    port
}

/// A live proxy, and a temp config file tagged `tag` that names its port and nothing else.
/// The proxy itself runs with no config file behind it. Answers the file's path and the port.
async fn a_live_proxy_and_its_config_file(tag: &str) -> (std::path::PathBuf, u16) {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-route-{tag}-{}-{port}.json",
        std::process::id()
    ));
    let seed = format!(r#"{{ "proxy": {{ "port": {port} }} }}"#);
    std::fs::write(&path, &seed).unwrap();
    serve_live_proxy(listener, &seed, None);
    (path, port)
}

/// Serve `app` on a loopback port for the life of the test binary, from a thread and runtime
/// of its own (each `#[tokio::test]` has a runtime that dies with it, so a server spawned
/// from one test would die under the others). For a stub whose answer never varies, one
/// listener then does for every test that wants it.
fn serve_for_the_binary(app: axum::Router) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app)
                .await
                .log_debug("serving a shared test server");
        });
    });
    port
}

/// The port of one shared server whose router has no routes: it answers every path with a
/// 404 carrying no `ENDPOINT_HEADER`, the shape of an older tcr.
fn empty_router_port() -> u16 {
    static PORT: std::sync::LazyLock<u16> =
        std::sync::LazyLock::new(|| serve_for_the_binary(axum::Router::new()));
    *PORT
}

/// The port of one shared listener that accepts nothing: a connect succeeds off the kernel
/// backlog and nothing is ever written back, the wedged shape. Held in a static, so it
/// outlives every test and a connect to it never closes under one.
fn wedged_port() -> u16 {
    static WEDGED: std::sync::LazyLock<(std::net::TcpListener, u16)> =
        std::sync::LazyLock::new(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            (listener, port)
        });
    WEDGED.1
}

// --- finish_login ---------------------------------------------------------

/// A [`Profile`] carrying only an email — the common case, no org info.
fn profile_named(email: &str) -> Profile {
    Profile {
        email: Some(email.to_string()),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
    }
}

/// A [`Profile`] carrying an email and a plan — the shape a real profile
/// fetch returns for a seated org.
fn profile_with_plan(
    email: &str,
    organization_type: &str,
    rate_limit_tier: Option<&str>,
    seat_tier: Option<&str>,
) -> Profile {
    Profile {
        email: Some(email.to_string()),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        organization_type: Some(organization_type.to_string()),
        rate_limit_tier: rate_limit_tier.map(str::to_string),
        seat_tier: seat_tier.map(str::to_string),
        plan_display_name: None,
    }
}

/// A live proxy of the given kind, as `singleton::live_proxy_server` reports
/// it — pid 4242 throughout, so a message assertion naming it is unambiguous.
fn incumbent(kind: singleton::ProxyKind) -> Option<singleton::Incumbent> {
    Some(singleton::Incumbent { pid: 4242, kind })
}

/// Unwrap a [`LoginRoute::Refuse`]'s message, or panic naming what came back
/// instead — every existing refusal test still wants the bare message
/// string, and this is the one place that unwrap lives now that the guard
/// returns a three-way enum instead of `Option<String>`.
fn expect_refuse(route: LoginRoute) -> String {
    match route {
        LoginRoute::Refuse(msg) => msg,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn tokens(access: &str, refresh: &str, expires: i64) -> Tokens {
    Tokens {
        id_token: None,
        access_token: access.to_string(),
        refresh_token: Some(refresh.to_string()),
        expires_at_ms: expires,
    }
}

fn two_account_seed() -> String {
    r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
              "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 },
            { "name": "bob@example.com", "type": "oauth", "accessToken": "at-bob",
              "refreshToken": "rt-bob", "expiresAt": 1893456000000, "priority": 1 }
        ] }"#
        .to_string()
}

/// `seed` written to a fresh temp config file tagged `tag`, and its path.
fn login_file(tag: &str, seed: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("tcr-oauth-{tag}-{}-{seq}.json", std::process::id()));
    std::fs::write(&path, seed).unwrap();
    path
}

/// A [`Profile`] carrying an account uuid and org — the branch the plain
/// email-only [`profile_named`] can never exercise, and the one the review
/// found untested (`identity::resolve`'s uuid+org comparison, not merely
/// its name-equality fallback).
fn profile_with_identity(
    email: &str,
    account_uuid: Option<&str>,
    org_uuid: Option<&str>,
    org_name: Option<&str>,
) -> Profile {
    Profile {
        email: Some(email.to_string()),
        account_uuid: account_uuid.map(str::to_string),
        org_uuid: org_uuid.map(str::to_string),
        org_name: org_name.map(str::to_string),
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
    }
}

/// An all-`None` [`Profile`] — the expected shape for a `claude
/// setup-token` credential, whose scope is inference-only and so its
/// `/api/oauth/profile` fetch will very likely fail.
fn all_none_profile() -> Profile {
    Profile {
        email: None,
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
    }
}

mod account;
mod admission;
mod browser;
mod claude_import;
mod codex_import;
mod login_ui;
mod persist;
mod remedies;
mod setup_token;
