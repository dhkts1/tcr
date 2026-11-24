//! Tests for the loopback client in `cli/live_client.rs`: the add-account route, the live
//! status read and the incumbent probe.

use super::*;
use crate::build_info::HANDOFF_SINCE;
#[test]
fn the_restart_hint_names_the_first_version_with_handoff() {
    assert!(RESTART_HINT.contains("stop it"));
    assert!(RESTART_HINT.contains(&format!("if it runs tcr {HANDOFF_SINCE} or newer")));
}
/// A minimal valid live server config: the port, no accounts, no api-key.
fn add_route_config(port: u16) -> Config {
    serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap()
}

/// A blank credential — the deliberately-invalid probe body — always
/// rejected by [`crate::proxy::add_account_handler`]'s own validation, so
/// this can never add a real account.
fn blank_account() -> Account {
    Account {
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        priority: None,
        ..Account::fixture("")
    }
}

#[tokio::test]
async fn post_add_account_reads_a_stamped_400_as_rejected() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let config = add_route_config(port);
    crate::test_support::live_proxy::serve(listener, config.clone(), None);

    let err = post_add_account(&config, &blank_account())
        .await
        .expect_err("a blank name/token must be refused by the route's own validation");
    std::assert_matches!(
        err,
        LiveControlError::Rejected(ref msg) if msg.contains("required"),
        "a stamped 400 is a structural Rejected, not a guess from status alone"
    );
}

/// The providers a STAMPED answer names come back beside it, read off the very probe
/// `oauth::probe_add_route` already sends. The same header on an UNSTAMPED answer — an older
/// tcr's catch-all, anything that is not this route — is no proof of anything and is not read.
#[tokio::test]
async fn post_add_account_with_providers_reads_them_off_a_stamped_answer_only() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let config = add_route_config(port);
    crate::test_support::live_proxy::serve(listener, config.clone(), None);

    let (answer, providers) = post_add_account_with_providers(&config, &blank_account()).await;
    std::assert_matches!(
        answer,
        Err(LiveControlError::Rejected(_)),
        "the blank probe is still the route's own stamped 400"
    );
    assert_eq!(providers, vec![Provider::Anthropic, Provider::Codex]);

    let (listener, port) = crate::test_support::loopback::listener().await;
    tokio::spawn(async move {
        let unstamped = axum::Router::new().fallback(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                [(crate::proxy::PROVIDERS_HEADER, "anthropic,codex")],
                "",
            )
        });
        axum::serve(listener, unstamped)
            .await
            .log_debug("serving the test stub");
    });
    let config = add_route_config(port);
    let (answer, providers) = post_add_account_with_providers(&config, &blank_account()).await;
    std::assert_matches!(answer, Err(LiveControlError::NoRoute));
    assert!(
        providers.is_empty(),
        "an unstamped answer names no providers, whatever its headers say: {providers:?}"
    );
}

#[tokio::test]
async fn post_add_account_unstamped_404_is_no_route() {
    // An "older tcr": something answers, but not on this route — no
    // `ENDPOINT_HEADER`, unlike every response this crate's own router
    // produces (even its 404s and 405s).
    let (listener, port) = crate::test_support::loopback::listener().await;
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new())
            .await
            .log_debug("serving the test stub");
    });
    let config = add_route_config(port);

    let err = post_add_account(&config, &blank_account())
        .await
        .expect_err("nothing is registered at this path");
    std::assert_matches!(
        err,
        LiveControlError::NoRoute,
        "an unstamped 404 must read as NoRoute, never Rejected"
    );
}

#[tokio::test]
async fn post_add_account_wrong_api_key_is_unauthorized() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let server_config: Config = serde_json::from_str(&format!(
        r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#
    ))
    .unwrap();
    crate::test_support::live_proxy::serve(listener, server_config, None);

    let client_config: Config = serde_json::from_str(&format!(
        r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
    ))
    .unwrap();
    let err = post_add_account(&client_config, &blank_account())
        .await
        .expect_err("a wrong api-key must be refused");
    std::assert_matches!(
        err,
        LiveControlError::Unauthorized,
        "must surface as Unauthorized, never NoRoute"
    );
}

/// THE ROUND TRIP: a real account, `POSTed` to a real live server, lands in
/// the SERVING rotation — not just in the response body.
#[tokio::test]
async fn post_add_account_adds_a_new_account_to_the_live_rotation() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let config = add_route_config(port);
    let manager = crate::test_support::live_proxy::serve(listener, config.clone(), None);

    let account = Account {
        access_token: "at-new".to_string(),
        refresh_token: Some("rt-new".to_string()),
        expires_at: Some(1_893_456_000_000),
        priority: None,
        ..Account::fixture("new@example.com")
    };
    let applied = post_add_account(&config, &account)
        .await
        .expect("a fresh identity must be added, not rejected");
    assert!(
        applied.added,
        "a nonexistent identity is ADDED: {applied:?}"
    );
    assert_eq!(applied.name, "new@example.com");

    let live = manager.snapshot(OffsetDateTime::now_utc());
    assert!(
        live.accounts.iter().any(|a| a.name == "new@example.com"),
        "the account must be in the SERVING rotation, not just the response"
    );
}

/// An ambiguous identity is refused by the route, structurally — never
/// guessed onto one of the candidates.
#[tokio::test]
async fn post_add_account_ambiguous_identity_is_rejected_not_guessed() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    // Two live accounts share a name and neither carries an org — the
    // legacy shape `same_identity`/`match_one` cannot break a tie on.
    let seed = format!(
        r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "dup@example.com", "type": "oauth", "accessToken": "at-1",
                  "refreshToken": "rt-1", "priority": 0 }},
                {{ "name": "dup@example.com", "type": "oauth", "accessToken": "at-2",
                  "refreshToken": "rt-2", "priority": 1 }}
            ] }}"#
    );
    let config: Config = serde_json::from_str(&seed).unwrap();
    crate::test_support::live_proxy::serve(listener, config.clone(), None);

    let account = Account {
        access_token: "at-new".to_string(),
        refresh_token: Some("rt-new".to_string()),
        expires_at: None,
        priority: None,
        ..Account::fixture("dup@example.com")
    };
    let err = post_add_account(&config, &account)
        .await
        .expect_err("an ambiguous identity must be refused, not guessed");
    std::assert_matches!(err, LiveControlError::Rejected(ref msg) if msg.contains("ambiguous"));
}

// --- resolve_account ---------------------------------------------------

/// The fallback half of the same contract: with nothing listening, the live
/// read reports `NoServer` (the ordinary case, which warns about nothing) and
/// `tcr status` keeps working exactly as before — just labelled `offline`.
///
/// # Why it retries
///
/// The port is reserved by binding and dropping, so between that drop and
/// the request it belongs to nobody, and under a full `cargo test`, where
/// other binaries are opening sockets at the same time, another process
/// takes it. This test then reaches something that ANSWERS, which is a
/// different error and a correct one, and the assertion fails for a reason
/// that has nothing to do with the classification it guards. Measured: it
/// went red exactly this way in one of three full-suite runs, with
/// `error sending request for url (http://127.0.0.1:64013/_tcr/status)`.
///
/// So a lost port is retried on a fresh one, five times at most. The ceiling
/// keeps the race visible: losing five in a row is a runner with no free
/// ports, which is a defect to see rather than to retry past. The same
/// shape, and the same reason, as `Mac::boot` in `tests/peer_e2e.rs`.
#[tokio::test]
async fn live_status_falls_back_when_no_server_answers() {
    /// How many times this may lose the port before it is a failure.
    const ATTEMPTS: usize = 5;

    let mut lost = 0;
    loop {
        // Bind and immediately drop, so the port is free and refuses.
        let port = crate::test_support::loopback::unused_port();
        let mut config = load_from(TWO_ACCOUNTS);
        config.proxy.port = port;

        match fetch_live_status(&config).await {
            Err(LiveStatusError::NoServer) => return,
            Err(err) => {
                lost += 1;
                assert!(
                    lost < ATTEMPTS,
                    "five loopback ports in a row were taken by something else between \
                         the reserve and the request, so this box has no free ports rather \
                         than a race: {}",
                    err.why()
                );
            }
            Ok(_) => panic!("something is listening on {port} and answered a status payload"),
        }
    }
}

// --- incumbent liveness -------------------------------------------------

/// THE CASE THAT MUST NOT BE A TAKEOVER. A healthy proxy whose api-key we do
/// not have answers `401` — it accepted the connection, routed the request
/// and wrote a response, which is the opposite of wedged. `build` is `None`
/// here exactly as it is for a wedged proxy, so a stand-down that decided on
/// `build.is_none()` would SIGKILL a server that is serving every request
/// fine. The liveness verdict is what separates them.
#[tokio::test]
async fn a_rejected_api_key_is_an_answering_incumbent_not_a_wedged_one() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let mut served = load_from(TWO_ACCOUNTS);
    served.proxy.port = port;
    served.proxy.api_key = Some("the-real-key".to_string());

    crate::test_support::live_proxy::serve(listener, served.clone(), None);

    // Same port, WRONG key — what an operator with a stale config has.
    let mut probing = served.clone();
    probing.proxy.api_key = Some("the-wrong-key".to_string());
    let probe = probe_incumbent(&probing).await;

    assert!(
        probe.build.is_none(),
        "the build read fails, which is precisely why it cannot be the liveness signal"
    );
    assert_eq!(
        probe.liveness,
        Liveness::Answering,
        "a 401 is a serving process; taking its port would be a takeover of a healthy proxy"
    );
}

/// The WEDGED shape: something holds the listening socket, the connect even
/// succeeds off the kernel backlog, and no response is ever written. This is
/// the state a deadlocked proxy leaves the port in, and the one the startup
/// stand-down has to be able to name — before this, `tcr` reported it as a
/// healthy incumbent and exited 0, and the port was unrecoverable.
///
/// Costs the probe's own 5s deadline by construction: the assertion IS that
/// we wait for it and then call it silent.
#[tokio::test]
async fn a_listener_that_never_answers_is_silent_not_answering() {
    // Bound and never accepted: connections queue in the backlog forever.
    let (listener, port) = crate::test_support::loopback::listener().await;
    let mut config = load_from(TWO_ACCOUNTS);
    config.proxy.port = port;

    let probe = probe_incumbent(&config).await;
    assert!(probe.build.is_none());
    match probe.liveness {
        Liveness::Silent { why } => assert!(
            !why.is_empty(),
            "the operator-facing line needs the probe's own account of what happened"
        ),
        Liveness::Answering => {
            panic!("a socket that never wrote a byte is not an answering proxy")
        }
    }
    drop(listener);
}

/// The other WEDGED shape: the connect itself runs into its 2s deadline,
/// because the SYN is dropped rather than refused. reqwest reports that
/// error as BOTH `is_connect()` and `is_timeout()`, so a sort that asks
/// `is_connect()` first files a wedged proxy as "nothing is listening", and
/// `tcr status` falls back offline without a word. The status read had that
/// order until it shared [`send_loopback`] with the control routes.
///
/// A loopback port cannot stage it: a full accept queue on macOS answers
/// with a reset, not silence. So the request goes to a TEST-NET-3 address
/// (RFC 5737), which nobody answers, and costs the 2s connect deadline.
#[tokio::test]
async fn a_connect_timeout_is_no_answer_not_no_server() {
    let config = load_from(TWO_ACCOUNTS);
    let started = std::time::Instant::now();
    let sent = send_loopback(&config, crate::proxy::STATUS_PATH, |client, _| {
        client.get("http://203.0.113.10:41000/_tcr/status")
    })
    .await;
    let waited = started.elapsed();
    match sent.map_err(LiveStatusError::from) {
        Err(LiveStatusError::NoAnswer(_)) => {}
        Err(other) => panic!(
            "a connect that ran into its deadline must be NoAnswer, got {other:?} after \
                 {waited:?} (well under 2s means this box has no route to TEST-NET-3, which \
                 refuses instead of blackholing)"
        ),
        Ok(response) => panic!("TEST-NET-3 answered: HTTP {}", response.status()),
    }
}

/// The classification the two tests above exercise, pinned directly so a
/// future variant cannot be added on the wrong side of it by accident.
#[test]
fn only_an_http_response_counts_as_answered() {
    assert!(
        !LiveStatusError::NoServer.answered(),
        "nothing listening answered nothing"
    );
    assert!(
        !LiveStatusError::NoAnswer("the server did not answer within 5s".into()).answered(),
        "a deadline with no bytes back is the wedged shape"
    );
    assert!(
        LiveStatusError::Unusable("HTTP 401 Unauthorized".into()).answered(),
        "an HTTP response — any HTTP response — means the process is serving"
    );
}

// --- no-persist guarantee ----------------------------------------------
