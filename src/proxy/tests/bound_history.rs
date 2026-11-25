//! Account-bound history: what pins a conversation to the account that minted its tokens, and what lets it go.

use super::*;

// --- account-bound history pins the conversation, and never diverts ---

/// An upstream that answers every request with a Messages body carrying the bound state the
/// real API returns — a `server_tool_use` block, a signed `thinking` block, and the
/// message's own id — and records the `authorization` header of each request it saw, which
/// is how these tests read WHICH account served a turn. Every token is a placeholder: a real
/// signature is Anthropic's attestation and has no business in a public repo.
///
/// Each response mints its OWN tokens (`…_fake_<nth hit>`), as the real API does. One fixed
/// token across every response would let a LATER turn re-mint it onto whichever account
/// served it, which is exactly the routing this is meant to prevent and would make the test
/// agree with a broken implementation.
async fn spawn_bound_upstream() -> (
    String,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let counter = Arc::clone(&hits);
    let recorder = Arc::clone(&seen);
    let upstream = Router::new().fallback(move |req: Request| {
        let counter = Arc::clone(&counter);
        let recorder = Arc::clone(&recorder);
        async move {
            let nth = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let mint_id = format!("srvtoolu_fake_{nth}");
            let body = serde_json::json!({
                "id": format!("msg_fake_{nth}"),
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "step one",
                     "signature": format!("sig_fake_{nth}")},
                    {"type": "server_tool_use", "id": mint_id, "name": "advisor", "input": {}},
                    {"type": "advisor_tool_result", "tool_use_id": mint_id,
                     "content": {"type": "advisor_redacted_result", "encrypted_content": "AAAA"}},
                ],
                "usage": {"input_tokens": 5, "output_tokens": 1},
            })
            .to_string();
            let auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            recorder
                .lock()
                .expect("upstream recorder poisoned")
                .push(auth);
            ([(CONTENT_TYPE, "application/json")], body)
        }
    });
    (stub::http(upstream).url(), hits, seen)
}

/// Two accounts over `upstream`, `disabled` applied by position.
fn two_accounts_over(upstream: &str, disabled: &[bool]) -> Config {
    let mut config = Config {
        accounts: two_account_config(None).accounts,
        ..dummy_config(None, upstream)
    };
    for (account, disable) in config.accounts.iter_mut().zip(disabled.iter()) {
        account.disabled = disable.then_some(true);
    }
    config
}

/// The whole feature, end to end on a two-account fleet: a turn whose history carries a
/// signed `thinking` block is served by the account that minted the signature — even when
/// this session's own affinity pin names the other one — and the pin itself is left alone,
/// because the divert is for THIS request and the session is still warm where it was.
#[tokio::test]
async fn a_bound_token_pins_later_turns_to_the_minting_account() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);

    // Turn 1 mints the signature on whichever account rotation picked.
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-mint", &[])).await;
    assert_eq!(status, StatusCode::OK);
    let minting_auth = seen
        .lock()
        .expect("recorder")
        .first()
        .cloned()
        .expect("a hit");
    assert_eq!(
        manager
            .bound_token_account(&["sig_fake_1".to_string()])
            .map(|(_, kind)| kind),
        Some(BoundTokenKind::ThinkingSignature),
        "the response's thinking signature must have been recorded"
    );

    // A DIFFERENT session, pinned to the OTHER account. Rotation on its own would hand it
    // the same account as turn 1 (priority order, not round-robin), so the other one is
    // made the only choice for exactly this turn and released again straight after: what
    // this test needs is a session whose pin disagrees with the mint, by whatever route.
    manager.mark_rate_limited(0, 60);
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-other", &[])).await;
    assert_eq!(status, StatusCode::OK);
    manager.clear_rate_limited(0);
    let other_auth = seen.lock().expect("recorder")[1].clone();
    assert_ne!(
        other_auth, minting_auth,
        "the control this test rests on: that turn landed on the other account"
    );
    let pin_key = stable_hash("uid:", "sess-other");
    let pinned_before = manager.affinity_pin(pin_key);
    assert!(pinned_before.is_some(), "that turn must have left a pin");

    // The same session, now carrying the signature minted on the OTHER account. Its pin
    // says one thing, the minting account says another, and the minting account wins.
    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body("sess-other", &["sig_fake_1"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        seen.lock().expect("recorder")[2],
        minting_auth,
        "a request carrying the signature must reach the account that signed it"
    );
    assert_eq!(
        manager.affinity_pin(pin_key),
        pinned_before,
        "a divert serves one request; it must not rewrite the session's pin"
    );

    // And the pin is still doing its job for turns that carry no bound history.
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-other", &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        seen.lock().expect("recorder")[3],
        other_auth,
        "with no token to honour, the session goes back to its own pinned account"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 4);
}

/// Reported 2026-11-07: a conversation resumed hours later went back to the account its
/// thinking was signed on, over an account the pick ranks first. For Opus 5.5 that hold
/// buys only a warm cache, and two hours idle there is none, so the pick decides. Two
/// controls fence it: Sonnet 5.5, whose thinking is dropped across organizations, and a
/// signature minted just now, whose cache is warm. Both stay held.
///
/// The pick is steered by priority alone (bob 0, alice 5), so a released request lands
/// on bob and a held one on alice, who signed every history here.
#[tokio::test]
async fn a_cold_opus_conversation_is_not_held_by_its_thinking() {
    let (upstream, _hits, seen) = spawn_bound_upstream().await;
    let mut config = two_accounts_over(&upstream, &[false, false]);
    config.accounts[0].priority = Some(5);
    config.accounts[1].priority = Some(0);
    let manager = Manager::with_live_refresher(config, None);
    let two_hours_ago = crate::now_ms() - 2 * 60 * 60 * 1000;
    let signed = |token: &str| vec![(BoundTokenKind::ThinkingSignature, token.to_string())];
    manager.record_bound_tokens(&signed("sig_cold_opus"), 0, two_hours_ago);
    manager.record_bound_tokens(&signed("sig_cold_sonnet"), 0, two_hours_ago);
    manager.record_bound_tokens(&signed("sig_warm_opus"), 0, crate::now_ms());
    let served_by = |n: usize| seen.lock().expect("recorder")[n].clone();

    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body_for("claude-opus-5-5", "sess-cold-opus", &["sig_cold_opus"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        served_by(0).contains("at-bbbbbbbbbbbb"),
        "a cold Opus conversation must take the pick, not its signer: got {}",
        served_by(0)
    );

    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body_for(
            "claude-sonnet-5-5",
            "sess-cold-sonnet",
            &["sig_cold_sonnet"],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        served_by(1).contains("at-aaaaaaaaaaaa"),
        "Sonnet 5.5 thinking is dropped off its org, so even cold it stays held: got {}",
        served_by(1)
    );

    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body_for("claude-opus-5-5", "sess-warm-opus", &["sig_warm_opus"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        served_by(2).contains("at-aaaaaaaaaaaa"),
        "a warm Opus conversation keeps its hold: got {}",
        served_by(2)
    );
}

/// A bound account that is OUT (here `disabled`, the same terminal gate a `rejected` unified
/// status or a dead login hits) releases the conversation to the fleet. The old answer was a
/// 429 on every turn until the account came back, which for a `rejected` account is days
/// (2026-09-22). The sibling rejects the bound state once and Claude Code recovers on its
/// own, by stripping it or replaying the thread; see `crate::manager::bound_tokens`.
///
/// The move is told first: the first request gets one plain 429 saying so, spending no
/// account, and the client's retry of it is the request that moves (`switch_warning`).
///
/// The bench still exists while the bound account is alive:
/// `a_bound_token_pins_later_turns_to_the_minting_account` is that control.
#[tokio::test]
async fn a_bound_account_that_is_out_releases_the_conversation_to_a_sibling() {
    let (upstream, hits, _seen) = spawn_bound_upstream().await;
    // Account 0 is out of rotation; the signature was minted on it before it went out.
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[true, false]), None);
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_fake_1".to_string())],
        0,
        crate::now_ms(),
    );
    assert_eq!(
        manager
            .bound_token_account(&["sig_fake_1".to_string()])
            .map(|(idx, _)| idx),
        Some(0),
        "precondition: the history IS bound, to the account that is out"
    );
    let turn = thinking_turn_body("sess-pinned", &["sig_fake_1"]);

    let (status, body) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the first request is told the conversation is moving: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        String::from_utf8_lossy(&body).contains("(disabled)"),
        "and why: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the warning spends no account"
    );

    let (status, body) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bound to an account that cannot serve, the retry goes to one that can: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the sibling served it");
}

/// A released conversation that ran on the sibling stays there when its old account comes
/// back. Its later turns mint nothing the history echoes, so without a re-bind the history's
/// newest known token still names the old account, and the next full-history request (a
/// replay after a thread 404, a side request) was held back there onto a cold cache:
/// 2026-10-15, 7 holds wrote 2.6M cache tokens that way (`Manager::rebind_served_history`).
#[tokio::test]
async fn a_released_conversation_stays_on_the_account_that_served_it() {
    let (upstream, _hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[true, false]), None);
    // Not `sig_fake_1`: the upstream mints `sig_fake_<nth hit>`, and its first answer would
    // record that name on the serving account by itself and pass this test without the fix.
    manager.record_bound_tokens(
        &[(
            BoundTokenKind::ThinkingSignature,
            "sig_fake_released".to_string(),
        )],
        0,
        crate::now_ms(),
    );
    let turn = thinking_turn_body("sess-moved", &["sig_fake_released"]);

    let (status, _) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "the switch warning");
    let (status, _) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(status, StatusCode::OK, "the retry moves");
    assert!(
        seen.lock().expect("recorder")[0].contains("at-bbbbbbbbbbbb"),
        "precondition: account 1 served the released conversation"
    );

    // Account 0 comes back, and the warning has run its life, as it had in the measured
    // case (the replay came 43 minutes after the conversation's last turn).
    assert_eq!(
        manager.set_disabled(0, false),
        DisablePersist::NoConfigFile,
        "re-enabled in memory, with no config file behind this manager to write"
    );
    manager.expire_switch_warnings();
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-fresh", &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        seen.lock().expect("recorder")[1].contains("at-aaaaaaaaaaaa"),
        "control: account 0 is back and serving, so only the history keeps a turn off it"
    );

    let (status, _) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        seen.lock().expect("recorder")[2].contains("at-bbbbbbbbbbbb"),
        "the same history must stay on the account that served it, warm, not go back to the \
             account it left: {}",
        seen.lock().expect("recorder")[2]
    );
}

/// The same move across organizations, for a model whose thinking survives only inside the
/// minting account's organization. The two fixture accounts sit in different organizations,
/// so the release is `cross_org` and the 200 on the new account drops the old thinking. The
/// signature used to keep naming the account it left, so once that account came back the
/// conversation was pulled back there: a cold cache, a 404 for thread state that now lives
/// on the new account, and a fresh switch warning when the old account ran out again.
#[tokio::test]
async fn a_sonnet_conversation_moved_across_organizations_is_not_pulled_back() {
    let model = "claude-sonnet-5-5";
    let (upstream, _hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);
    assert!(
        manager.outside_org_of(0).contains(&1),
        "precondition: the two accounts sit in different organizations"
    );

    // Turn 1 is served on account 0 and mints `sig_fake_1` there.
    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body_for(model, "sess-cross-org", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        seen.lock().expect("recorder")[0].contains("at-aaaaaaaaaaaa"),
        "precondition: account 0 served the conversation first"
    );
    let turn = thinking_turn_body_for(model, "sess-cross-org", &["sig_fake_1"]);

    // Account 0 goes out: one switch warning, then the retry moves to account 1.
    assert_eq!(
        manager.set_disabled(0, true),
        DisablePersist::NoConfigFile,
        "disabled in memory, with no config file behind this manager to write"
    );
    let (status, _) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "the switch warning");
    let (status, _) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(status, StatusCode::OK, "the retry moves");
    assert!(
        seen.lock().expect("recorder")[1].contains("at-bbbbbbbbbbbb"),
        "precondition: account 1 served the moved conversation"
    );

    // Account 0 comes back, and the warning has run its life.
    assert_eq!(
        manager.set_disabled(0, false),
        DisablePersist::NoConfigFile,
        "re-enabled in memory"
    );
    manager.expire_switch_warnings();
    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body_for(model, "sess-fresh-cross-org", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        seen.lock().expect("recorder")[2].contains("at-aaaaaaaaaaaa"),
        "control: account 0 is back and serving, so only the history keeps a turn off it"
    );

    for (n, nth) in [(1, 3), (2, 4)] {
        let (status, body) = drive_conversation(Arc::clone(&manager), &turn).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "turn {n} after the return is served, not warned: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            seen.lock().expect("recorder")[nth].contains("at-bbbbbbbbbbbb"),
            "turn {n} after the return must stay on the account the conversation moved to, \
                 not be pulled back to the one it left: {}",
            seen.lock().expect("recorder")[nth]
        );
    }
}

/// A bound account over the SOFT threshold still holds its conversation
/// (`Manager::bound_account_holds` ignores the threshold on purpose), so it must also SERVE
/// it, on every turn. It used to reach the fleet's last-resort revalidation serve, which lets
/// one request through per 2 s across the whole fleet: the turn that lost that race got an
/// exhausted 429 naming the 5-hour window, and Claude Code renders that claim as "session
/// limit". Measured 2026-09-24: 3 of 3 such 429s came 0.05-1.4 s after another turn on the
/// same account, while four other accounts sat at 0%.
///
/// The session is pinned to the OTHER account on purpose. When the pin IS the bound account,
/// the affinity fast-path already serves it over the threshold and this never happened.
#[tokio::test]
async fn a_bound_account_over_the_soft_threshold_serves_every_turn() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);

    // Pin the session to account 1, by the same route the pin test above uses.
    manager.mark_rate_limited(0, 60);
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-held", &[])).await;
    assert_eq!(status, StatusCode::OK);
    manager.clear_rate_limited(0);
    let pin_key = stable_hash("uid:", "sess-held");
    let pinned_before = manager.affinity_pin(pin_key);
    assert_eq!(
        pinned_before,
        Some(1),
        "precondition: the session is pinned to account 1"
    );
    let pinned_auth = seen.lock().expect("recorder")[0].clone();

    // The history is signed on account 0, and account 0 reads over the soft threshold (0.90
    // in `dummy_config`) on its 5-hour window, through the same headers a live reply carries.
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_held".to_string())],
        0,
        crate::now_ms(),
    );
    let mut over = HeaderMap::new();
    over.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        HeaderValue::from_static("0.96"),
    );
    over.insert(
        "anthropic-ratelimit-unified-5h-reset",
        HeaderValue::from_str(&(crate::now_ms() / 1000 + 660).to_string()).expect("reset header"),
    );
    over.insert(
        "anthropic-ratelimit-unified-status",
        HeaderValue::from_static("allowed_warning"),
    );
    manager.update_quota(0, &over, 200);

    // Three turns back to back, all inside one 2 s revalidation window.
    for turn in 1..=3 {
        let response = drive_conversation_full(
            Arc::clone(&manager),
            &thinking_turn_body("sess-held", &["sig_held"]),
        )
        .await;
        let status = response.status();
        let claim = response
            .headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("read body");
        assert_eq!(
            status,
            StatusCode::OK,
            "turn {turn}: a held conversation on an account over the soft threshold is served \
                 there, not refused (claim={claim:?}): {}",
            String::from_utf8_lossy(&body)
        );
    }

    let seen = seen.lock().expect("recorder");
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    assert!(
        seen[1..].iter().all(|auth| *auth != pinned_auth),
        "every held turn reached the signing account, never the pin: {seen:?}"
    );
    assert_eq!(
        manager.affinity_pin(pin_key),
        pinned_before,
        "serving the held account is for these requests; the session's pin stays"
    );
}

/// An instant under a day away reads as a time; one a day or more away carries its date, so
/// a weekly window three days out is not read as later today.
#[test]
fn a_reset_a_day_or_more_away_carries_its_date() {
    let now = time::macros::datetime!(2026-09-24 12:43:11 UTC);
    assert_eq!(
        at_utc(now + time::Duration::minutes(17), now),
        "13:00:11 UTC"
    );
    assert_eq!(
        at_utc(now + time::Duration::seconds(260_209), now),
        "2026-09-27 13:00:00 UTC"
    );
}

/// A 429 for held history never claims a quota window, even when its one account is over
/// the threshold on its 5-hour window. The claim makes Claude Code print "You've hit your
/// session limit" and stop (measured 2026-09-24 against Claude Code 2.1.281), and it is
/// false for held history, whose account releases the conversation to the fleet once it is
/// really out. A plain 429 is retried by the client on its own.
///
/// The control is the same fleet with no history held: every account is over, so the
/// fleet's 429 still names the window.
#[tokio::test]
async fn a_held_429_never_claims_a_quota_window() {
    let manager = Manager::with_live_refresher(
        two_accounts_over("http://127.0.0.1:9", &[false, false]),
        None,
    );
    let mut over = HeaderMap::new();
    over.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        HeaderValue::from_static("0.96"),
    );
    over.insert(
        "anthropic-ratelimit-unified-5h-reset",
        HeaderValue::from_str(&(crate::now_ms() / 1000 + 660).to_string()).expect("reset header"),
    );
    over.insert(
        "anthropic-ratelimit-unified-status",
        HeaderValue::from_static("allowed_warning"),
    );
    manager.update_quota(0, &over, 200);
    manager.update_quota(1, &over, 200);
    let now = OffsetDateTime::now_utc();
    let claim = |response: &Response| {
        response
            .headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };

    let fleet = exhausted_response(&manager, now, 2, false, Provider::Anthropic, None, None);
    assert_eq!(
        claim(&fleet).as_deref(),
        Some("five_hour"),
        "control: with no history held, the fleet's 429 names the window"
    );

    let held = exhausted_response(&manager, now, 2, false, Provider::Anthropic, None, Some(0));
    assert_eq!(held.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(claim(&held), None, "a held 429 names no window");
    assert!(
        held.headers()
            .get("anthropic-ratelimit-unified-status")
            .is_none(),
        "and carries no unified status, so the client retries it"
    );
}

/// An upstream on which the FIRST test account (`at-aaaaaaaaaaaa`) is out, answering every
/// request with a `rejected` 429 on its 5-hour window, and every other account serves a
/// 200. Counts each side's hits, which is how these tests read who was asked.
async fn spawn_first_account_out_upstream() -> (
    String,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits_first = Arc::new(AtomicUsize::new(0));
    let hits_other = Arc::new(AtomicUsize::new(0));
    let (first, other) = (Arc::clone(&hits_first), Arc::clone(&hits_other));
    let reset = (crate::now_ms() / 1000 + 3600).to_string();
    let upstream = Router::new().fallback(move |req: Request| {
            let (first, other, reset) = (Arc::clone(&first), Arc::clone(&other), reset.clone());
            async move {
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                if auth.contains("at-aaaaaaaaaaaa") {
                    first.fetch_add(1, Ordering::SeqCst);
                    return Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header(CONTENT_TYPE, "application/json")
                        .header("retry-after", "3600")
                        .header("anthropic-ratelimit-unified-status", "rejected")
                        .header("anthropic-ratelimit-unified-5h-status", "rejected")
                        .header("anthropic-ratelimit-unified-5h-utilization", "1.0")
                        .header("anthropic-ratelimit-unified-5h-reset", reset)
                        .body(Body::from(
                            r#"{"type":"error","error":{"type":"rate_limit_error","message":"out"}}"#,
                        ))
                        .expect("429 response");
                }
                other.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"id":"msg_fake_ok","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#,
                    ))
                    .expect("200 response")
            }
        });
    (stub::http(upstream).url(), hits_first, hits_other)
}

/// `/v1/messages` with NO `SessionKey` extension, so the request has no session key: the
/// one kind a switch warning cannot be remembered for, and so the one that moves silently.
async fn drive_unkeyed(
    manager: Arc<Manager>,
    body: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Bytes) {
    use tower::ServiceExt as _;
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder
        .body(Body::from(body.to_string()))
        .expect("build request");
    req.extensions_mut().insert(ClientAddr(loopback_peer()));
    let response = app(manager).oneshot(req).await.expect("router response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    (status, bytes)
}

/// The held account runs out DURING a request: its upstream answers a `rejected` 429. The
/// client is told once, in a plain 429 whose own text says the conversation is moving, and
/// the client's retry of that 429 is served by the sibling. It used to get the exhausted
/// 429 with the window's claim ("You've hit your session limit") and move only on the next
/// turn the user typed.
#[tokio::test]
async fn a_held_account_running_out_mid_request_warns_once_then_moves() {
    use std::sync::atomic::Ordering;
    let (upstream, hits_first, hits_other) = spawn_first_account_out_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_held".to_string())],
        0,
        crate::now_ms(),
    );
    let turn = thinking_turn_body("sess-moving", &["sig_held"]);

    let warned = drive_conversation_full(Arc::clone(&manager), &turn).await;
    assert_eq!(warned.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        warned
            .headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .is_none()
            && warned
                .headers()
                .get("anthropic-ratelimit-unified-status")
                .is_none(),
        "the warning is a plain 429, the only kind whose text the client shows"
    );
    assert_eq!(
        warned
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some(SWITCH_WARNING_RETRY_AFTER_SECS.to_string().as_str()),
    );
    let body = to_bytes(warned.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("moves it to another account")
            && text.contains("(rejected upstream)")
            && !text.contains("example.com"),
        "the warning says what is happening and why, and names no account: {text}"
    );
    assert_eq!(
        (
            hits_first.load(Ordering::SeqCst),
            hits_other.load(Ordering::SeqCst)
        ),
        (1, 0),
        "only the held account was asked"
    );

    // The client's retry of the warning: the same request, and it moves.
    let (status, body) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the retry is served by the sibling: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        (
            hits_first.load(Ordering::SeqCst),
            hits_other.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
}

/// With no session key there is nothing to remember a warning by, so a held account that
/// runs out mid-request releases the request IN that request, and the sibling serves it.
#[tokio::test]
async fn a_held_account_running_out_mid_request_moves_within_it_when_no_warning_can_stick() {
    use std::sync::atomic::Ordering;
    let (upstream, hits_first, hits_other) = spawn_first_account_out_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_held".to_string())],
        0,
        crate::now_ms(),
    );

    let (status, body) = drive_unkeyed(
        Arc::clone(&manager),
        &thinking_turn_body("sess-unkeyed", &["sig_held"]),
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "released in the same request: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        (
            hits_first.load(Ordering::SeqCst),
            hits_other.load(Ordering::SeqCst)
        ),
        (1, 1),
        "the held account refused it, then the sibling served it"
    );
}

/// The same mid-request release, on a BORROWED request whose lease names only the held
/// account (`x-tcr-accounts`). The release re-admits only what the hold benched, so the
/// lease's scope still keeps the other account out: the borrower gets a 429, and the account
/// outside the lease is never asked. Clearing `tried` wholesale served it there (found in
/// review, 2026-09-24: status 200 with the other account hit once).
#[tokio::test]
async fn a_mid_request_release_keeps_a_leases_account_set_scope() {
    use std::sync::atomic::Ordering;
    let (upstream, hits_first, hits_other) = spawn_first_account_out_upstream().await;
    let mut config = two_accounts_over(&upstream, &[false, false]);
    config.accounts[0].name = "alice".to_string();
    config.accounts[1].name = "bob".to_string();
    let manager = Manager::with_live_refresher(config, None);
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_held".to_string())],
        0,
        crate::now_ms(),
    );
    let label = tcr_peer_wire::sanitize_label("alice").expect("label");

    let (status, body) = drive_unkeyed(
        Arc::clone(&manager),
        &thinking_turn_body("sess-scoped", &["sig_held"]),
        &[
            (RELAYED_HEADER_NAME, "1"),
            (ACCOUNTS_HEADER_NAME, label.as_str()),
        ],
    )
    .await;
    assert_eq!(
        hits_other.load(Ordering::SeqCst),
        0,
        "the account outside the lease is never asked (status {status}): {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(hits_first.load(Ordering::SeqCst), 1);
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

/// A warning is a commitment. Once this session was told its conversation is moving off
/// account 0, the next request moves even though account 0 would hold it again: here its
/// hold is 200 s, under the cache's life, which on its own keeps the conversation there and
/// answers the retry with a 429 of its own (found in review, 2026-09-24).
#[tokio::test]
async fn a_told_session_moves_even_if_its_account_would_hold_it_again() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_held".to_string())],
        0,
        crate::now_ms(),
    );
    manager.mark_rate_limited(0, 200);
    let key = stable_hash("uid:", "sess-told");
    assert!(
        manager.first_switch_warning(key, 0, crate::now_ms()),
        "precondition: the session is told once"
    );

    let (status, body) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body("sess-told", &["sig_held"]),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the told session moves: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(
        seen.lock().expect("recorder")[0].contains("at-bbbbbbbbbbbb"),
        "served by the sibling"
    );
}

/// A Fable request on a held account that is out only of its Fable weekly window diverts
/// that one request, as before, and the conversation stays where it is. So there is no move
/// to announce: no warning, and the session's one warning is left for a real move.
#[tokio::test]
async fn a_fable_only_block_diverts_without_a_warning() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_held".to_string())],
        0,
        crate::now_ms(),
    );
    let mut fable_out = HeaderMap::new();
    fable_out.insert(
        "anthropic-ratelimit-unified-7d_oi-utilization",
        HeaderValue::from_static("1.0"),
    );
    fable_out.insert(
        "anthropic-ratelimit-unified-7d_oi-reset",
        HeaderValue::from_str(&(crate::now_ms() / 1000 + 259_200).to_string())
            .expect("reset header"),
    );
    manager.update_quota(0, &fable_out, 200);
    let body = with_body_field(
        &thinking_turn_body("sess-fable", &["sig_held"]),
        "model",
        serde_json::json!("claude-fable-5"),
    );

    let (status, response) = drive_conversation(Arc::clone(&manager), &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the Fable request diverts, no warning first: {}",
        String::from_utf8_lossy(&response)
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(
        seen.lock().expect("recorder")[0].contains("at-bbbbbbbbbbbb"),
        "served by the sibling"
    );
    assert!(
        !manager.switch_warned(stable_hash("uid:", "sess-fable"), 0, crate::now_ms()),
        "the session's warning is still unspent"
    );
}

/// The pin rung releases on the same rule, and this is where the boundary sits: a hold past
/// the cache's life (`CACHE_WARM_HOLD_SECS`, 300s) releases the conversation, while a
/// shorter one still answers 429 with that hold's own `retry-after`
/// (`a_bound_request_on_a_long_hold_answers_429_with_that_holds_retry_after`, 45s).
#[tokio::test]
async fn bound_history_on_a_hold_past_the_cache_goes_to_a_sibling() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);

    // Pin the session to account 1 by making it the only choice for one turn.
    manager.mark_rate_limited(0, 60);
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-far", &[])).await;
    assert_eq!(status, StatusCode::OK);
    manager.clear_rate_limited(0);
    let pinned_auth = seen.lock().expect("recorder")[0].clone();
    assert_eq!(
        manager.affinity_pin(stable_hash("uid:", "sess-far")),
        Some(1)
    );

    // Bound history this process never minted, so the pin rung names account 1, which is
    // now held for ten minutes.
    manager.mark_rate_limited(1, 600);
    let turn = thinking_turn_body("sess-far", &["sig_fake_minted_before_this_build"]);
    // Told first (`switch_warning`), and the client's retry is the request that moves.
    let (status, body) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the first request is told the conversation is moving: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        String::from_utf8_lossy(&body).contains("(on an upstream hold until "),
        "and why: {}",
        String::from_utf8_lossy(&body)
    );
    let (status, body) = drive_conversation(Arc::clone(&manager), &turn).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a hold past the cache's life releases the conversation: {}",
        String::from_utf8_lossy(&body)
    );
    assert_ne!(
        seen.lock().expect("recorder")[1],
        pinned_auth,
        "served by the sibling, not the held pin"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

/// Three accounts over `upstream` in two organizations: alice (index 0, `at-aaaaaaaaaaaa`)
/// and carol (index 2, `at-cccccccccccc`) share one, bob (index 1, `at-bbbbbbbbbbbb`) is in
/// the other. Priority order is alice, bob, carol, so with alice out the ordinary pick
/// chooses bob, the OTHER organization, and only a same-organization preference picks carol.
fn two_orgs_over(upstream: &str, carol_disabled: bool) -> Config {
    let mut config = two_accounts_over(upstream, &[false, false]);
    let mut carol = config.accounts[0].clone();
    carol.name = "carol@example.com".to_string();
    carol.account_uuid = Some("44444444-c".to_string());
    carol.access_token = "at-cccccccccccc".to_string();
    carol.refresh_token = Some("rt-cccccccccccc".to_string());
    carol.priority = Some(2);
    carol.disabled = carol_disabled.then_some(true);
    config.accounts.push(carol);
    config
}

/// The `authorization` that served one turn of a conversation bound to alice by a thinking
/// signature she minted, with alice on a hold past the cache's life, so the turn is released
/// before the loop (the pre-loop release site). No session key, so no switch warning: the
/// release happens in this one request.
async fn released_turn_served_by(model: &str, carol_disabled: bool) -> String {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_orgs_over(&upstream, carol_disabled), None);
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_held".to_string())],
        0,
        crate::now_ms(),
    );
    manager.mark_rate_limited(0, 600);
    let body = with_body_field(
        &thinking_turn_body("sess-org", &["sig_held"]),
        "model",
        serde_json::json!(model),
    );
    let (status, response) = drive_unkeyed(Arc::clone(&manager), &body, &[]).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "released and served: {}",
        String::from_utf8_lossy(&response)
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    seen.lock().expect("recorder")[0].clone()
}

/// Sonnet 5.5 keeps a thinking block only inside the organization that minted it
/// (`crate::base::model::thinking_is_org_bound`), so a released conversation goes to the
/// same-organization seat even though the ordinary pick would choose the other organization.
#[tokio::test]
async fn a_released_sonnet_5_5_conversation_goes_to_a_same_org_seat() {
    let served = released_turn_served_by("claude-sonnet-5-5", false).await;
    assert!(
        served.contains("at-cccccccccccc"),
        "served by carol, alice's organization: {served}"
    );
}

/// Opus 5.5 keeps a thinking block on any account, so its release is not steered: the
/// ordinary pick serves it, on the other organization.
#[tokio::test]
async fn a_released_opus_5_5_conversation_takes_the_ordinary_pick() {
    let served = released_turn_served_by("claude-opus-5-5", false).await;
    assert!(
        served.contains("at-bbbbbbbbbbbb"),
        "served by bob, the ordinary pick: {served}"
    );
}

/// With no usable seat in the bound account's organization, a Sonnet 5.5 release falls back
/// to the ordinary pick rather than failing.
#[tokio::test]
async fn a_sonnet_5_5_release_with_no_same_org_seat_takes_the_ordinary_pick() {
    let served = released_turn_served_by("claude-sonnet-5-5", true).await;
    assert!(
        served.contains("at-bbbbbbbbbbbb"),
        "served by bob, the only account left: {served}"
    );
}

/// The mid-request release site: alice holds the conversation, her upstream answers a
/// `rejected` 429 during the request, and the request (no session key, so no warning) is
/// released inside it. Sonnet 5.5 goes to carol, alice's organization, not to bob.
#[tokio::test]
async fn a_sonnet_5_5_conversation_released_mid_request_goes_to_a_same_org_seat() {
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let reset = (crate::now_ms() / 1000 + 3600).to_string();
    let upstream = Router::new().fallback(move |req: Request| {
            let (recorder, reset) = (Arc::clone(&recorder), reset.clone());
            async move {
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                recorder
                    .lock()
                    .expect("upstream recorder poisoned")
                    .push(auth.clone());
                if auth.contains("at-aaaaaaaaaaaa") {
                    return Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header(CONTENT_TYPE, "application/json")
                        .header("retry-after", "3600")
                        .header("anthropic-ratelimit-unified-status", "rejected")
                        .header("anthropic-ratelimit-unified-5h-status", "rejected")
                        .header("anthropic-ratelimit-unified-5h-utilization", "1.0")
                        .header("anthropic-ratelimit-unified-5h-reset", reset)
                        .body(Body::from(
                            r#"{"type":"error","error":{"type":"rate_limit_error","message":"out"}}"#,
                        ))
                        .expect("429 response");
                }
                Response::builder()
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"id":"msg_fake_ok","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#,
                    ))
                    .expect("200 response")
            }
        });
    let addr = stub::http(upstream);
    let manager =
        Manager::with_live_refresher(two_orgs_over(&format!("http://{addr}"), false), None);
    manager.record_bound_tokens(
        &[(BoundTokenKind::ThinkingSignature, "sig_held".to_string())],
        0,
        crate::now_ms(),
    );
    let body = with_body_field(
        &thinking_turn_body("sess-org-mid", &["sig_held"]),
        "model",
        serde_json::json!("claude-sonnet-5-5"),
    );

    let (status, response) = drive_unkeyed(Arc::clone(&manager), &body, &[]).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "released in the same request: {}",
        String::from_utf8_lossy(&response)
    );
    let seen = seen.lock().expect("recorder").clone();
    assert_eq!(
        seen.len(),
        2,
        "alice refused it, then one account served it"
    );
    assert!(seen[0].contains("at-aaaaaaaaaaaa"), "alice was asked first");
    assert!(
        seen[1].contains("at-cccccccccccc"),
        "served by carol, alice's organization: {}",
        seen[1]
    );
}

/// `body` with one top-level field set, for the two places a request names thread state.
fn with_body_field(body: &str, key: &str, value: Value) -> String {
    let mut parsed: Value = serde_json::from_str(body).expect("test body is JSON");
    parsed[key] = value;
    parsed.to_string()
}

/// Of the two places Claude Code sends a `previous_message_id`, only `thread` names state
/// the request depends on. `diagnostics` rides on the full-history replay the client sends
/// AFTER a thread was lost; binding on it pinned the recovery request to the account that
/// had just failed it (2026-09-22). The `thread` half is the control: the same id there
/// still holds the request to the account that minted it.
#[tokio::test]
async fn only_the_thread_previous_message_id_binds() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);

    // Turn 1 on its own session mints `msg_fake_1` on account 0 (priority order).
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-mint", &[])).await;
    assert_eq!(status, StatusCode::OK);
    let minting_auth = seen.lock().expect("recorder")[0].clone();
    assert_eq!(
        manager.bound_token_account(&["msg_fake_1".to_string()]),
        Some((0, BoundTokenKind::MessageId)),
        "precondition: the response's message id was recorded against account 0"
    );

    // A second session pinned to account 1.
    manager.mark_rate_limited(0, 60);
    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body("sess-replay", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    manager.clear_rate_limited(0);
    let pinned_auth = seen.lock().expect("recorder")[1].clone();
    assert_ne!(
        pinned_auth, minting_auth,
        "control: the pin is the OTHER account"
    );

    let diagnostics = with_body_field(
        &thinking_turn_body("sess-replay", &[]),
        "diagnostics",
        serde_json::json!({"previous_message_id": "msg_fake_1"}),
    );
    let (status, _) = drive_conversation(Arc::clone(&manager), &diagnostics).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        seen.lock().expect("recorder")[2],
        pinned_auth,
        "a diagnostics id binds nothing: the session's own pin decides"
    );

    let thread = with_body_field(
        &thinking_turn_body("sess-replay", &[]),
        "thread",
        serde_json::json!({"type": "continue", "previous_message_id": "msg_fake_1"}),
    );
    let (status, _) = drive_conversation(Arc::clone(&manager), &thread).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        seen.lock().expect("recorder")[3],
        minting_auth,
        "a thread id holds the request to the account that holds the thread"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 4);
}

/// A token this process never minted — from before this shipped, or aged out of the map —
/// on a session with no pin benches nothing. The rule must never block a conversation it
/// knows nothing about and has no better guess for.
#[tokio::test]
async fn an_unknown_bound_token_on_an_unpinned_session_changes_nothing() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);

    for _ in 0..2 {
        let (status, _) = drive_conversation(
            Arc::clone(&manager),
            &thinking_turn_body("sess-unknown", &["sig_fake_never_minted"]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let seen = seen.lock().expect("recorder").clone();
    assert_eq!(
        seen[0], seen[1],
        "unchanged routing here means the session's own pin decided, not a bench"
    );
}

/// The case that covers every conversation older than this build: the history IS bound —
/// it carries a `thinking` signature — but this process never minted it, so nothing
/// resolves. It must be held to the session's PIN rather than diverted, because the pin is
/// where that history was made, and a divert is a guaranteed rejection.
///
/// The control is the same request on a short hold: without the bench it diverts to the
/// sibling and is served (`short-hold`, 2,773 of 4,420 diverts measured 2026-09-20..22).
#[tokio::test]
async fn bound_history_with_no_known_token_holds_the_session_to_its_pin() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);

    // Pin the session to account 1 by making it the only choice for one turn.
    manager.mark_rate_limited(0, 60);
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-old", &[])).await;
    assert_eq!(status, StatusCode::OK);
    manager.clear_rate_limited(0);
    let pinned_auth = seen.lock().expect("recorder")[0].clone();
    let pin_key = stable_hash("uid:", "sess-old");
    assert_eq!(manager.affinity_pin(pin_key), Some(1));

    // The control FIRST, so this test cannot pass on a fleet that never diverts: an
    // ORDINARY turn on the pinned account's short hold goes to the sibling.
    manager.mark_rate_limited(1, 7);
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-old", &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(
        seen.lock().expect("recorder")[1],
        pinned_auth,
        "with no bound history, a short hold on the pin DIVERTS — that is the behaviour \
             this rule has to override"
    );
    manager.clear_rate_limited(1);

    // Same session, same hold, but now the history is bound by a signature this process
    // never saw. There is nothing to resolve, so the pin decides — and the request WAITS
    // that hold out on the pinned account rather than diverting. A hold inside
    // `INLINE_WAIT_MAX_SECS` is waited inline, so the turn is served, on the one account
    // that can serve it: strictly better than the 429 the same bench produces for a longer
    // hold (the test below), and never the sibling.
    manager.mark_rate_limited(1, 1);
    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body("sess-old", &["sig_fake_minted_before_this_build"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        seen.lock().expect("recorder")[2],
        pinned_auth,
        "a bound conversation is held to its pin and waits, never diverted"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

/// The other half of the hold: a bound conversation whose only account is parked for LONGER
/// than the soft-wait gets a 429 advertising that account's own `retry-after`, so the
/// client comes back when the account is actually free — not a divert onto a sibling that
/// would reject the history, and not an instant 429 with an invented number.
#[tokio::test]
async fn a_bound_request_on_a_long_hold_answers_429_with_that_holds_retry_after() {
    let (upstream, hits, seen) = spawn_bound_upstream().await;
    let manager = Manager::with_live_refresher(two_accounts_over(&upstream, &[false, false]), None);

    // Pin the session to account 1, as above.
    manager.mark_rate_limited(0, 600);
    let (status, _) =
        drive_conversation(Arc::clone(&manager), &thinking_turn_body("sess-held", &[])).await;
    assert_eq!(status, StatusCode::OK);
    manager.clear_rate_limited(0);
    assert_eq!(
        manager.affinity_pin(stable_hash("uid:", "sess-held")),
        Some(1)
    );
    let pinned_auth = seen.lock().expect("recorder")[0].clone();

    // A hold past the soft-wait ceiling (`EXHAUSTION_SOFT_WAIT_MAX_SECS`), so waiting it out
    // inside the request is not an option, and under the client's `retry-after` ceiling
    // (`CLIENT_RETRY_AFTER_MAX_SECS`), so the header carries the figure itself. The SIBLING
    // frees sooner: timed fleet-wide, the answer would be the sibling's 3s — a soft-wait
    // spent on an account this request may not use, then a 429 whose number means nothing.
    const BOUND_HOLD: i64 = 45;
    const _: () = assert!(
        BOUND_HOLD > EXHAUSTION_SOFT_WAIT_MAX_SECS && BOUND_HOLD <= CLIENT_RETRY_AFTER_MAX_SECS
    );
    manager.mark_rate_limited(0, 3);
    manager.mark_rate_limited(1, BOUND_HOLD);
    let asked_at = Instant::now();
    let response = drive_conversation_full(
        Arc::clone(&manager),
        &thinking_turn_body("sess-held", &["sig_fake_minted_before_this_build"]),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "held to a parked pin, the honest answer is a 429"
    );
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .expect("a 429 from this path must carry a retry-after");
    assert!(
        (BOUND_HOLD - 1..=BOUND_HOLD).contains(&retry_after),
        "the hint must be the pinned account's OWN hold ({BOUND_HOLD}s, less the moment \
             spent getting here), not the sibling's and not an invented number: got \
             {retry_after}"
    );
    assert!(
        asked_at.elapsed() < Duration::from_secs(2),
        "answered at once, not after a soft-wait timed on the sibling: {:?}",
        asked_at.elapsed()
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the held request reached no upstream at all"
    );
    manager.clear_rate_limited(0);
    manager.clear_rate_limited(1);

    // Released, the same bound request is served by the pinned account.
    let (status, _) = drive_conversation(
        Arc::clone(&manager),
        &thinking_turn_body("sess-held", &["sig_fake_minted_before_this_build"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen.lock().expect("recorder")[1], pinned_auth);
}
