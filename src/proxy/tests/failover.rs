//! Upstream refusals and the failover after them: 401, 403, 529 and exhausted-429 verdicts, backoff and soft waits.

use super::*;

#[test]
fn classify_transient_429_absent_parks_short() {
    // THE BITING TEST: an absent retry-after must park the short
    // NO_GUIDANCE_HOLD_SECS base (15), not the old fabricated 60. The pre-fix
    // inline logic fabricated a 60s park for an absent retry-after
    // (unwrap_or(60), 60 > the 15s inline bound → Park(60)); this asserts the
    // new short base value instead — the oracle for the fleet-blackout fix.
    assert_eq!(
        classify_transient_429(None, 0, 0),
        Transient429::Park(NO_GUIDANCE_HOLD_SECS)
    );
    assert_eq!(classify_transient_429(None, 0, 0), Transient429::Park(15));
    // Jitter adds to the base hold so co-tripping accounts un-park staggered.
    assert_eq!(classify_transient_429(None, 0, 5), Transient429::Park(20));
}

#[test]
fn classify_transient_429_present_retry_after_unchanged() {
    // A non-zero jitter must be IGNORED on the present-retry-after path — it
    // only affects the no-guidance (None) hold. Pass jitter=3 throughout and
    // assert the results are the historical byte-identical values.
    // Short hint under the inline bound, retry budget available → inline-wait.
    assert_eq!(
        classify_transient_429(Some(3), 0, 3),
        Transient429::InlineWait(3)
    );
    // Retry cap reached → park the same short wait, rotate.
    assert_eq!(
        classify_transient_429(Some(3), MAX_SAME_ACCOUNT_429, 3),
        Transient429::Park(3)
    );
    // Upper clamp: anything over 300 parks at 300.
    assert_eq!(
        classify_transient_429(Some(500), 0, 3),
        Transient429::Park(300)
    );
    // Lower clamp: 0 becomes 1, still within the inline bound → inline-wait.
    assert_eq!(
        classify_transient_429(Some(0), 0, 3),
        Transient429::InlineWait(1)
    );
}

#[test]
fn backoff_529_ladder_escalates_then_clamps() {
    assert_eq!(
        backoff_529_secs(0, None),
        1,
        "first retry is near-immediate"
    );
    assert_eq!(backoff_529_secs(1, None), 2, "the ladder doubles");
    assert_eq!(
        backoff_529_secs(2, None),
        4,
        "third rung reaches the ceiling"
    );
    assert_eq!(
        backoff_529_secs(9, None),
        RETRY_529_MAX_BACKOFF_SECS as u64,
        "a rung past the ceiling clamps rather than overflowing the shift"
    );
}

#[test]
fn backoff_529_honours_retry_after_as_a_floor_but_clamps_it() {
    assert_eq!(
        backoff_529_secs(0, Some(3)),
        3,
        "a server hint longer than the rung wins — the upstream knows its own load"
    );
    assert_eq!(
        backoff_529_secs(1, Some(1)),
        2,
        "a hint SHORTER than the rung must not defeat the escalation"
    );
    assert_eq!(
        backoff_529_secs(0, Some(300)),
        RETRY_529_MAX_BACKOFF_SECS as u64,
        "an arbitrary server-supplied wait is clamped — it holds an in-flight slot"
    );
    assert_eq!(
        backoff_529_secs(0, Some(0)),
        1,
        "a zero/absurd hint still waits the rung, never busy-loops"
    );
    assert_eq!(backoff_529_secs(0, Some(-5)), 1, "negative hints too");
}

/// The in-flight guard is held across every 529 backoff, so the ladder's TOTAL
/// is a latency budget, not just a per-step one. This is the assertion that
/// fails if someone raises the retry count or the per-step ceiling without
/// re-reading the comment on the 529 arm.
///
/// Scope: this is the PER-ACCOUNT ladder. A request may now spend it on up to
/// `1 + MAX_529_FAILOVERS_PER_REQUEST` accounts, so the number a client actually
/// waits is a multiple of this one — see
/// `overloaded_529_failover_worst_case_latency_is_bounded`.
#[test]
fn backoff_529_worst_case_total_stays_single_digit() {
    let worst: u64 = (0..MAX_SAME_ACCOUNT_529_RETRIES)
        .map(|n| backoff_529_secs(n, Some(i64::MAX)))
        .sum();
    assert!(
        worst < 10,
        "529 retries hold an in-flight slot; worst-case added latency was {worst}s"
    );
}

/// The REQUEST-level 529 latency budget, and the loop headroom the failover needs.
/// Pure arithmetic over the constants, so it is the assertion that fails when
/// someone widens the failover budget without re-reading what it costs.
///
/// Two ladders, because they are two different promises. On the no-`retry-after`
/// ladder — the only shape the live captures actually carry — the whole request
/// stays single-digit at 9s. With a hostile server hint every rung clamps to
/// `RETRY_529_MAX_BACKOFF_SECS` and the ceiling is 24s: NOT single-digit, and
/// stated here honestly rather than assumed away, because the in-flight guard is
/// held across all of it. That ceiling is what bounds `MAX_529_FAILOVERS_PER_REQUEST`
/// at 2.
///
/// The headroom half is the one that would fail silently: the rotation loop stops
/// after `max_attempts_for(account_count)` iterations, so a failover ladder longer
/// than that budget would be truncated mid-walk — the request would stop early for
/// a reason nothing logs. The binding case is the SMALLEST fleet that can host the
/// full walk (`accounts_walked` accounts); bigger fleets only add headroom, and
/// smaller ones cannot reach the last hop at all because the failover is gated on
/// `select` offering another account.
#[test]
fn overloaded_529_failover_worst_case_latency_is_bounded() {
    let accounts_walked = 1 + u64::from(MAX_529_FAILOVERS_PER_REQUEST);
    let ladder = |hint: Option<i64>| -> u64 {
        (0..MAX_SAME_ACCOUNT_529_RETRIES)
            .map(|n| backoff_529_secs(n, hint))
            .sum()
    };

    let no_hint = ladder(None) * accounts_walked;
    assert!(
        no_hint < 10,
        "on the ladder the live upstream actually produces (no `retry-after`) the \
             whole request must stay single-digit; was {no_hint}s"
    );
    let hostile_hint = ladder(Some(i64::MAX)) * accounts_walked;
    assert!(
        hostile_hint <= 24,
        "even when every 529 asks for the maximum wait, a client connection and \
             the in-flight slots behind it are parked for at most 24s; was {hostile_hint}s"
    );

    let sends = accounts_walked as usize * (MAX_SAME_ACCOUNT_529_RETRIES as usize + 1);
    let budget = max_attempts_for(accounts_walked as usize);
    assert!(
        sends <= budget,
        "the full failover walk is {sends} sends but the rotation loop only allows \
             {budget} attempts on a {accounts_walked}-account fleet — the walk would be \
             silently truncated"
    );
}

#[test]
fn soft_wait_secs_separates_transient_from_real_exhaustion() {
    // Transient burst-park (≤ ceiling), not yet soft-waited → wait that long.
    assert_eq!(soft_wait_secs(16, false), Some(16));
    // Exactly at the 20s ceiling → still a transient park, soft-wait.
    assert_eq!(
        soft_wait_secs(EXHAUSTION_SOFT_WAIT_MAX_SECS, false),
        Some(EXHAUSTION_SOFT_WAIT_MAX_SECS as u64)
    );
    // Just over the ceiling → real exhaustion, hard-fail (no wait).
    assert_eq!(
        soft_wait_secs(EXHAUSTION_SOFT_WAIT_MAX_SECS + 1, false),
        None
    );
    // A real quota window (hours) → far above ceiling → hard-fail.
    assert_eq!(soft_wait_secs(6000, false), None);
    // One-shot: already soft-waited this request → never wait again.
    assert_eq!(soft_wait_secs(16, true), None);
    // No promise of an imminent un-park (0 / negative hint) → hard-fail.
    assert_eq!(soft_wait_secs(0, false), None);
    assert_eq!(soft_wait_secs(-1, false), None);
}

/// U2e (storm fix): an upstream `401` whose forced refresh produces NO new token
/// (here: an always-transient refresher, the same `false` return a
/// cooldown-suppressed force gives) must leave the account **Active** and rotate
/// away — never `mark_error`. Before the fix a suppressed force retried the same
/// dead token, hit a 2nd 401, and sidelined a healthy account ~75s (cascade).
#[tokio::test]
async fn suppressed_force_401_keeps_account_active_and_rotates() {
    // A refresher that always fails transiently → apply_refresh never runs, so
    // ensure_fresh_force returns false (identical to a cooldown-suppressed force).
    struct AlwaysTransient;
    impl crate::oauth::TokenRefresher for AlwaysTransient {
        fn refresh(
            &self,
            _provider: Provider,
            _refresh_token: String,
        ) -> crate::oauth::RefreshFuture {
            Box::pin(async { Err(crate::oauth::OAuthError::Transient("simulated blip".into())) })
        }
    }

    // A canned upstream that answers every connection with a bare 401.
    let up_addr = stub::canned(raw_401());

    // No proxy api-key configured → the keyless test request is accepted.
    let manager = Manager::new(
        dummy_config(None, &format!("http://{up_addr}")),
        Arc::new(AlwaysTransient),
        Arc::new(crate::probe::LiveUsageProber::new()),
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );

    drop(
        send_to_proxy(
            app(Arc::clone(&manager)),
            client_request(Method::POST, "/v1/messages", "{}"),
        )
        .await,
    );

    assert_eq!(
        manager.account_status(0),
        Some(AccountStatus::Active),
        "a forced refresh that produced no new token must rotate, not sideline the account"
    );
}

/// The inverse of the two 401 tests around it: an account with NO refresh
/// token (`tcr login --token`) that 401s must be marked `Error` on the spot.
/// The churn argument that protects refreshable accounts does not apply —
/// nothing can have rotated its token, and nothing can ever renew it — and
/// before this arm existed such a row stayed `active` in `tcr status` for
/// as long as the config held it, 401ing every request it was handed.
#[tokio::test]
async fn single_401_on_account_without_refresh_token_marks_error() {
    struct NeverCalled;
    impl crate::oauth::TokenRefresher for NeverCalled {
        fn refresh(
            &self,
            _provider: Provider,
            _refresh_token: String,
        ) -> crate::oauth::RefreshFuture {
            Box::pin(async {
                panic!("a refresh must never be attempted for an account with no refresh token")
            })
        }
    }

    let up_addr = stub::canned(raw_401());

    let mut config = dummy_config(None, &format!("http://{up_addr}"));
    config.accounts[0].refresh_token = None;
    let manager = Manager::new(
        config,
        Arc::new(NeverCalled),
        Arc::new(crate::probe::LiveUsageProber::new()),
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );

    drop(
        send_to_proxy(
            app(Arc::clone(&manager)),
            client_request(Method::POST, "/v1/messages", "{}"),
        )
        .await,
    );

    assert_eq!(
        manager.account_status(0),
        Some(AccountStatus::Error),
        "a 401 on an account that cannot refresh is a dead credential, not churn"
    );
}

/// A SECOND 401 — the retry with a freshly-minted token also 401s — must NOT
/// condemn the account to the terminal `Error` status. That 2nd 401 is almost
/// always rotation churn (a concurrent force-refresh superseded the token we
/// retried with), not a dead credential. `Error` is unrecoverable BY DESIGN —
/// `probeable_indices`, `Manager::select` and warming all skip errored rows, and
/// only a successful refresh clears it, which can never run on a row nobody
/// probes or selects. So condemning a healthy account here sidelines it forever.
/// Observed live 2026-07-17: all 7 accounts walked to `error` one transient 401
/// at a time and the proxy went dark while every token still probed 200.
#[tokio::test]
async fn second_401_does_not_condemn_account_to_terminal_error() {
    // A refresher that SUCCEEDS: ensure_fresh_force returns true, so the request
    // retries the SAME account, the canned upstream 401s again, and we land on
    // the 2nd-401 path under test.
    struct AlwaysRefreshes;
    impl crate::oauth::TokenRefresher for AlwaysRefreshes {
        fn refresh(
            &self,
            _provider: Provider,
            _refresh_token: String,
        ) -> crate::oauth::RefreshFuture {
            Box::pin(async {
                Ok(crate::oauth::Tokens {
                    id_token: None,
                    access_token: "fresh-access".into(),
                    refresh_token: Some("fresh-refresh".into()),
                    expires_at_ms: crate::now_ms() + 3_600_000,
                })
            })
        }
    }

    // A canned upstream that answers every connection with a bare 401.
    let up_addr = stub::canned(raw_401());

    let manager = Manager::new(
        dummy_config(None, &format!("http://{up_addr}")),
        Arc::new(AlwaysRefreshes),
        Arc::new(crate::probe::LiveUsageProber::new()),
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );

    drop(
        send_to_proxy(
            app(Arc::clone(&manager)),
            client_request(Method::POST, "/v1/messages", "{}"),
        )
        .await,
    );

    assert_eq!(
        manager.account_status(0),
        Some(AccountStatus::Active),
        "a 2nd 401 is rotation churn, not a dead credential — it must leave the \
             account Active; `Error` is terminal and would sideline a healthy account \
             forever (nothing re-probes or re-selects an errored row)"
    );
}

/// A `429` with NO `unified-status`, i.e. a TRANSIENT rate limit: the arm that
/// inline-waits `retry_after` seconds on the same account and, once
/// [`MAX_SAME_ACCOUNT_429`] inline retries are spent, PARKS it for that long and
/// rotates — the `Transient429::Park` that puts an account into BOTH
/// `mark_rate_limited` and `tried`. `retry_after: 1` is what keeps a test that
/// must reach the park down to two one-second inline waits.
fn raw_429_transient(retry_after: u32) -> String {
    format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\
             retry-after: {retry_after}\r\n\r\n"
    )
}

/// A bare `401`. With the `fleet` refresher (every refresh a transient error)
/// the force-refresh yields no new token, so the account is benched in `tried`
/// and the request rotates — an entry no timer ever clears.
fn raw_401() -> String {
    "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
}

/// A bare `403 Forbidden` — upstream refusing the pooled credential itself,
/// distinct from `local_endpoint_gate`'s own 403 (never sent upstream) and from
/// a `RelayMode::ClientCredential` 403 (a different code path entirely; see
/// `CLIENT_CREDENTIAL_PREFIXES`).
fn raw_403() -> String {
    "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
}

/// [`post_one`] with session affinity LIVE, so a test can assert what happened to
/// the session's PIN and not just to the client's status.
///
/// Two things have to be true for a request to be pinned, and `app()` alone
/// provides neither: the [`SessionKey`] extension must be present (injected here
/// by a layer, exactly as the hybrid CONNECT server does per connection) and the
/// request must carry a stable identity for [`stable_session_key`] to hash —
/// hence `metadata.user_id` in the body. The pin lives in the MANAGER, so two
/// calls with the same `user_id` are two requests of ONE session even though each
/// call drives a fresh router.
async fn post_one_pinned(manager: Arc<Manager>, user_id: &str) -> u16 {
    let affinity_app = app(manager).layer(axum::middleware::from_fn(
        move |mut req: Request, next: axum::middleware::Next| async move {
            // The value is a per-connection ordinal upstream and is deliberately
            // NOT the routing key (`stable_session_key` never falls back to it),
            // so a constant is as faithful here as a counter.
            req.extensions_mut().insert(SessionKey(1));
            next.run(req).await
        },
    ));
    let body = serde_json::json!({
        "model": "claude-x",
        "metadata": { "user_id": user_id },
        "messages": [],
    })
    .to_string();
    send_to_proxy(
        affinity_app,
        client_request(Method::POST, "/v1/messages", body),
    )
    .await
    .status()
    .as_u16()
}

/// A 529 is transient upstream overload, so it is retried IN PLACE — the client
/// sees the eventual 200 instead of an error it has to re-send by hand.
///
/// The fleet has two accounts and selection is LRU, so a fresh fleet picks `a`
/// first and would pick `b` on any rotation. Scripting `529` then `200` makes
/// the two behaviours distinguishable in the stats: retry-in-place credits `a`
/// with the 200, a rotation credits `b`. Before this arm existed the client got
/// the 529 straight through and neither account served anything.
#[tokio::test]
async fn overloaded_529_retries_the_same_account() {
    let up_addr = spawn_scripted_upstream(vec![
        Some(raw_529()), // attempt 1 — upstream reports itself overloaded
        Some(raw_200()), // attempt 2 — the SAME account, after the backoff
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b"]);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(
        status, 200,
        "a 529 is transient — the retry must serve the client the 200 rather \
             than surfacing an error it has to re-send by hand"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let served: Vec<&str> = snap
        .accounts
        .iter()
        .filter(|a| a.requests > 0)
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(
        served,
        ["a"],
        "the retry must reuse the account that 529'd — landing on `b` means the \
             529 rotated and threw away a warm prompt cache for nothing"
    );
    let a = snap.accounts.iter().find(|a| a.name == "a").unwrap();
    assert_eq!(
        a.requests, 1,
        "only the terminal outcome counts: a retried 529 is not a served request"
    );
}

/// The ladder is bounded, and on exhaustion the 529 is forwarded VERBATIM —
/// the pre-fix behaviour, just later. Both halves run concurrently because each
/// pays the real 1s + 2s backoff.
///
/// Two scripts pin the attempt count from both sides: with the budget exactly
/// spent the third send lands on a scripted 200 (proving both retries ran), and
/// with one 529 too many the fourth send never happens, so the 200 behind it is
/// unreachable and the client gets the 529.
///
/// The SINGLE-account fleet is load-bearing, not incidental: it is what keeps this
/// the IN-PLACE budget's test after the failover stage was added. With a second
/// account the spent budget would fail over instead of giving up, and this would
/// silently become a (worse) copy of `overloaded_529_fails_over_to_another_account`.
/// The failover's own bound is `overloaded_529_failover_is_bounded`.
#[tokio::test]
async fn overloaded_529_gives_up_after_the_budget() {
    let exact = async {
        let up_addr = spawn_scripted_upstream(vec![
            Some(raw_529()),
            Some(raw_529()),
            Some(raw_200()), // reachable only if BOTH retries fire
        ])
        .await;
        post_one(fleet(up_addr, &["a"])).await
    };
    let over = async {
        let (up_addr, attempts) = spawn_counted_upstream(vec![
            Some(raw_529()),
            Some(raw_529()),
            Some(raw_529()),
            Some(raw_200()), // must stay unreachable — the budget is spent
        ])
        .await;
        let (status, body) = post_one_with_body(fleet(up_addr, &["a"])).await;
        (status, body, attempts.load(Ordering::SeqCst))
    };
    let (exact_status, (over_status, over_body, over_attempts)) = tokio::join!(exact, over);

    assert_eq!(
        exact_status, 200,
        "the budget is {MAX_SAME_ACCOUNT_529_RETRIES} retries — the third send \
             must still happen and serve the client"
    );
    assert_eq!(
        over_status, 529,
        "one 529 past the budget must terminate, not retry forever — the \
             scripted 200 behind it proves the send never happened"
    );
    assert_eq!(
        over_attempts,
        MAX_SAME_ACCOUNT_529_RETRIES as usize + 1,
        "the ladder is exactly one send plus {MAX_SAME_ACCOUNT_529_RETRIES} retries"
    );
    assert!(
        over_body.contains("overloaded_error"),
        "give-up forwards the upstream 529 verbatim, exactly as before this arm \
             existed; a synthesized body would break clients that parse it. Got: {over_body}"
    );
}

/// The constraint that survives the failover stage, and the one no client ever
/// sees: a 529 must not cost the account its ELIGIBILITY. It means "this send was
/// refused", never "this account is over quota", so `mark_rate_limited` is the one
/// call this arm may never make — a hold would bench a healthy account for every
/// OTHER request in flight, and only a later request can observe that.
///
/// A single-account fleet, so the failover has nowhere to go and the give-up path
/// runs: request 1 spends the in-place budget and gets the 529 forwarded. The bite
/// is request 2 on the SAME manager — it must be served a 200 by the SAME account,
/// which is only possible if the 529 left behind neither a hold nor a durable
/// bench (`tried` is per-request and dies with the request).
///
/// This was `overloaded_529_does_not_rotate_or_hold` and asserted a no-rotation
/// half too. That half is now false BY DESIGN; what bounds the rotation instead is
/// `overloaded_529_failover_is_bounded`.
#[tokio::test]
async fn overloaded_529_never_arms_a_hold_or_benches_the_account() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        Some(raw_529()),
        Some(raw_529()),
        Some(raw_529()),
        Some(raw_200()), // request 2 — reachable only if `a` is still eligible
    ])
    .await;
    let manager = fleet(up_addr, &["a"]);

    assert_eq!(
        post_one(Arc::clone(&manager)).await,
        529,
        "one account means nothing to fail over to, so the in-place ladder is the \
             whole budget and the upstream's 529 is forwarded"
    );
    // Without this the test passes vacuously on a proxy that never retries at
    // all: one 529 forwarded straight through leaves the account unheld too.
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        MAX_SAME_ACCOUNT_529_RETRIES as usize + 1,
        "exactly one initial send plus {MAX_SAME_ACCOUNT_529_RETRIES} retries"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
    assert!(
        a.rate_limited_until.is_none(),
        "a 529 is upstream overload, not a quota rejection: it must never arm a \
             rate-limit hold that would bench the account for later requests"
    );
    assert_eq!(
        a.gate,
        GateReason::Ok,
        "no hard gate of any kind may be left behind — the account stays in rotation"
    );
    assert_eq!(
        a.requests, 1,
        "the forwarded 529 counts once — the retries must not each count as served"
    );

    // The durable half of the claim, which only a second request can make.
    assert_eq!(
        post_one(Arc::clone(&manager)).await,
        200,
        "the 529'd account must be fully eligible for the NEXT request"
    );
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
    assert_eq!(a.requests, 2, "both requests were served by `a`");
}

/// The failover, end to end, together with the pin invariant that makes it safe.
///
/// `a` is overloaded for its whole in-place budget while `b` is healthy, so the
/// client must see `b`'s 200 — before this stage existed it saw `a`'s 529 and
/// re-sent by hand. The SESSION, meanwhile, must still be pinned to `a`: a 529 is
/// a fact about one request, not about the account. `tried.insert` buys both at
/// once, because it sends `select` down its "pin-tried" branch, which diverts THIS
/// REQUEST and keeps the pin.
///
/// `sessions[0].account` is read from the affinity map (the sole authority on the
/// pin) and `last_served_account` from the serve, so the two DIFFERING is the
/// assertion: one request moved, the session did not. The fifth scripted reply
/// proves it behaviourally — the session's next request comes home to `a`.
#[tokio::test]
async fn overloaded_529_fails_over_to_another_account() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        Some(raw_529()), // `a` — send 1
        Some(raw_529()), // `a` — in-place retry 1
        Some(raw_529()), // `a` — in-place retry 2, budget now spent
        Some(raw_200()), // the failover target serves the client
        Some(raw_200()), // the session's NEXT request, back home on `a`
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b"]);
    const SESSION: &str = "529-failover-session";

    assert_eq!(
        post_one_pinned(Arc::clone(&manager), SESSION).await,
        200,
        "the overload is account-scoped, so a sibling can serve: the client must \
             get the 200 rather than a 529 it has to re-send by hand"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        MAX_SAME_ACCOUNT_529_RETRIES as usize + 2,
        "the in-place ladder still runs in FULL on `a` first — the failover is the \
             send AFTER it, not a replacement for it"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
    let b = snap.accounts.iter().find(|x| x.name == "b").unwrap();
    assert_eq!(
        (a.requests, b.requests),
        (0, 1),
        "only the terminal outcome counts, and it happened on `b`"
    );
    assert!(
        a.rate_limited_until.is_none(),
        "the failover benches `a` for THIS REQUEST only — no hold may be armed"
    );
    let session = snap
        .sessions
        .first()
        .expect("the pinned session is visible in the snapshot");
    assert_eq!(
        session.account, "a",
        "the pin must NOT move: a 529 diverts one request, and re-keying the \
             session would cold-start its whole prompt-cache prefix on `b`"
    );
    assert_eq!(
        session.last_served_account, "b",
        "…while the account that actually served it is the failover target"
    );

    assert_eq!(
        post_one_pinned(Arc::clone(&manager), SESSION).await,
        200,
        "the session's next request must be servable"
    );
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
    assert_eq!(
        a.requests, 1,
        "and it comes HOME to the pinned account — the divert really was for one \
             request only"
    );
}

/// A 403 refusing OUR pooled credential on account `a` must fail over to `b`,
/// never reach the client verbatim: Claude Code reads a bare 403 as its OWN
/// session being dead and drops the user's login over an account problem that
/// is entirely ours.
#[tokio::test]
async fn forbidden_403_fails_over_to_another_account() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![Some(raw_403()), Some(raw_200())]).await;
    let manager = fleet(up_addr, &["a", "b"]);

    assert_eq!(
        post_one(Arc::clone(&manager)).await,
        200,
        "the 403 must never reach the client — it names a credential the client \
             never saw and cannot act on"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "exactly one rotation: refused on `a`, served by `b`"
    );
    assert_eq!(
        manager.account_status(0),
        Some(AccountStatus::Active),
        "a 403 is evidence about THIS request's access, not proof the refresh \
             token is dead — the account must stay eligible for the next request, \
             the same rule the 401 branch already follows"
    );
}

/// A 403 must hold the refused account for a short while, so the next requests
/// stop paying the same refused round trip, and the row must read `refused`
/// rather than `ok` while the hold lasts.
#[tokio::test]
async fn forbidden_403_holds_the_account_and_shows_refused() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![Some(raw_403()), Some(raw_200())]).await;
    let manager = fleet(up_addr, &["a", "b"]);

    for _ in 0..4 {
        assert_eq!(post_one(Arc::clone(&manager)).await, 200);
    }
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        5,
        "one wasted attempt on `a`, then `a` is held and never retried"
    );
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
    assert_eq!(
        a.requests, 0,
        "a held `a` is never selected again; `attempts` alone cannot show this, \
             because the scripted upstream answers by connection order, not by account"
    );
    assert_eq!(
        serde_json::to_value(a.gate).unwrap(),
        serde_json::json!("refused"),
        "a held 403 account must not read `gate: ok`"
    );
    assert_eq!(
        manager.account_status(0),
        Some(AccountStatus::Active),
        "the hold is bounded, never the terminal `Error`"
    );
}

/// When 403s cover the WHOLE eligible fleet, the client must get a 502
/// `proxy_error` naming the refused account(s) — never the bare upstream 403
/// (unusable: names a credential the client cannot act on) and never a
/// synthesized 429 (a lie: waiting will not fix a refused credential).
#[tokio::test]
async fn forbidden_403_fast_fails_when_the_whole_fleet_is_refused() {
    // One script entry, reused for every connection: 403, anywhere.
    let (up_addr, attempts) = spawn_counted_upstream(vec![Some(raw_403())]).await;
    let manager = fleet(up_addr, &["a", "b"]);

    let (status, body) = post_one_with_body(Arc::clone(&manager)).await;
    assert_eq!(
        status, 502,
        "both accounts refused — refusals are the WHOLE story, so this must \
             fast-fail rather than fall through to the generic exhaustion 429"
    );
    assert!(
        body.contains("proxy_error"),
        "must use the proxy_error envelope, not forward the 403: {body}"
    );
    assert!(
        body.contains("a (index 0)") && body.contains("b (index 1)"),
        "must name the refused accounts so the reader knows which credential(s) \
             need attention: {body}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "every account in the fleet was tried exactly once, then the fast-fail \
             fired — no retry loop on a refusal"
    );
}

/// A MIX of a 403 refusal and an ordinary quota rejection must NOT fast-fail:
/// the refusal does not account for the whole story, and the account that was
/// merely rate-limited can still recover on its own hold timer. This is the
/// upstream follow-up fix's whole point — a naive short-circuit at the top of
/// the no-account branch turns one dead credential into a hard error for every
/// recoverable exhaustion too.
#[tokio::test]
async fn forbidden_403_mixed_with_quota_exhaustion_falls_through_to_existing_handling() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        Some(raw_403()),               // `a` — refused
        Some(raw_429_rejected(3_600)), // `b` — durable quota rejection
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b"]);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(
        status, 429,
        "one refusal among a mixed fleet must fall through to the EXISTING \
             exhaustion handling (a 429), not the 403 fast-fail's 502 — the \
             quota-rejected account might still recover on its own hold"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "both accounts were tried once each before the fleet-exhausted 429"
    );
}

/// A fleet spent on its 5-hour windows answers a 429 the client can READ as a
/// usage limit: `anthropic-ratelimit-unified-status: rejected`, the claim the
/// client maps to its five-hour type, and a `reset` equal to the window's real
/// reset — not a constant. Without those headers the client labels the 429
/// "not your usage limit", which is the opposite of what happened. The body
/// names the cause and the instant too, for a reader without a header view.
#[tokio::test]
async fn exhausted_429_carries_the_unified_headers_when_a_quota_window_binds() {
    let up_addr = spawn_scripted_upstream(vec![Some(raw_200())]).await;
    // Revalidation-serve (default ON) would probe an over-threshold account
    // with a real send instead of failing the request; this test is about the
    // 429 the client gets once nothing is servable, so it is off here.
    let manager = fleet_configured(up_addr, &["a", "b"], |config| {
        config
            .extra
            .insert("revalidationServe".to_string(), serde_json::json!(false));
    });
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 900;
    let mut spent = HeaderMap::new();
    spent.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        HeaderValue::from_static("0.99"),
    );
    spent.insert(
        "anthropic-ratelimit-unified-5h-reset",
        HeaderValue::from_str(&reset.to_string()).unwrap(),
    );
    for idx in 0..2 {
        assert!(
            manager.update_quota(idx, &spent, 200),
            "fixture must land a 5h window"
        );
    }

    let (status, headers, body) = post_one_full(manager).await;
    assert_eq!(
        status, 429,
        "no eligible account → exhausted 429; body: {body}"
    );
    let h = |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_string());
    assert_eq!(
        h("anthropic-ratelimit-unified-status").as_deref(),
        Some("rejected")
    );
    assert_eq!(
        h("anthropic-ratelimit-unified-representative-claim").as_deref(),
        Some("five_hour")
    );
    assert_eq!(
        h("anthropic-ratelimit-unified-reset").as_deref(),
        Some(reset.to_string().as_str()),
        "the reset is the window's own instant, not a synthesized one"
    );
    // The window is 900s out, which the client would refuse outright (it gives
    // up on anything above 60), so the HEADER is capped. This assertion used to
    // pin the raw 895..=900; that was the bug, not the contract.
    let retry_after: i64 = h("retry-after").unwrap().parse().unwrap();
    assert_eq!(
        retry_after, CLIENT_RETRY_AFTER_MAX_SECS,
        "a 900s wait is advertised at the client's accepted cap, got {retry_after}"
    );
    assert!(
        body.contains("All 2 accounts exhausted: 2 in the 5-hour window.")
            && body.contains("soonest account frees at"),
        "the body names the cause and the instant: {body}"
    );
    let body_secs: i64 = body
        .split("Retry in ")
        .nth(1)
        .and_then(|rest| rest.split('s').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("body carries a `Retry in Ns` figure: {body}"));
    assert!(
        (895..=900).contains(&body_secs),
        "the body keeps the REAL, uncapped distance to the reset, got {body_secs}"
    );
}

/// A hold whose soonest free is hours away still ships a `retry-after` the
/// client will honour. Before the cap this header was 3600 and the client gave
/// up on the request in a second (or, in watchdog mode, slept an hour).
#[tokio::test]
async fn exhausted_429_caps_an_hours_long_retry_after_at_the_client_ceiling() {
    let up_addr = spawn_scripted_upstream(vec![Some(raw_200())]).await;
    let manager = fleet(up_addr, &["a", "b"]);
    for idx in 0..2 {
        manager.mark_rate_limited(idx, 3_600);
    }
    let hint = manager.exhaustion_hint(OffsetDateTime::now_utc(), false, Provider::Anthropic, None);
    assert!(
        hint.retry_after > 3_500 && hint.free_at.is_some(),
        "fixture must be a MEASURED wait far above the cap: {hint:?}"
    );
    let (status, headers, body) = post_one_full(manager).await;
    assert_eq!(status, 429, "body: {body}");
    let retry_after: i64 = headers
        .get("retry-after")
        .expect("retry-after ships")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        retry_after <= 60,
        "the client refuses a retry-after above 60, got {retry_after}"
    );
    assert_ne!(
        retry_after, 60,
        "60 is the unknown-reset sentinel; a measured wait must not wear it"
    );
}

/// A measured wait already inside the cap is advertised as-is: the cap only
/// ever shortens what the client could not use.
#[tokio::test]
async fn exhausted_429_passes_a_retry_after_inside_the_cap_through_unchanged() {
    let up_addr = spawn_scripted_upstream(vec![Some(raw_200())]).await;
    let manager = fleet(up_addr, &["a", "b"]);
    // 40s: above the 20s soft-wait ceiling (so the request hard-fails instead
    // of sleeping) and below the cap.
    for idx in 0..2 {
        manager.mark_rate_limited(idx, 40);
    }
    let (status, headers, body) = post_one_full(manager).await;
    assert_eq!(status, 429, "body: {body}");
    let retry_after: i64 = headers
        .get("retry-after")
        .expect("retry-after ships")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (38..=40).contains(&retry_after),
        "a 40s wait is inside the cap and must ship unchanged, got {retry_after}"
    );
}

/// The pure rule, at its three edges. A measured wait above the cap is capped,
/// a measured wait of EXACTLY 60 is capped too (so a `60` on the wire can only
/// ever be the sentinel), and the unknown-reset sentinel passes through as the
/// 60 `exhaustion_hint_reports_an_unknown_reset_as_unknown_not_as_sixty_seconds`
/// pins in the manager.
#[test]
fn advertised_retry_after_caps_measured_waits_and_keeps_the_sentinel_distinct() {
    let now = OffsetDateTime::now_utc();
    let hint = |retry_after: i64, free_at: Option<OffsetDateTime>| ExhaustionHint {
        retry_after,
        free_at,
        binding: None,
        gated: std::collections::BTreeMap::new(),
    };
    let measured = |secs: i64| hint(secs, Some(now + time::Duration::seconds(secs)));

    assert_eq!(advertised_retry_after(&measured(57_950)), 59);
    assert_eq!(advertised_retry_after(&measured(61)), 59);
    assert_eq!(
        advertised_retry_after(&measured(60)),
        59,
        "a measured 60 must not collide with the sentinel on the wire"
    );
    assert_eq!(advertised_retry_after(&measured(59)), 59);
    assert_eq!(advertised_retry_after(&measured(30)), 30);
    assert_eq!(advertised_retry_after(&measured(1)), 1);
    assert_eq!(
        advertised_retry_after(&hint(60, None)),
        60,
        "the unknown-reset sentinel reaches the client unchanged — and 60 is \
             exactly the ceiling it honours"
    );
}

/// An upstream `retry-after` hold is NOT the user's usage limit, so a fleet
/// held that way stays a plain 429: no unified headers, and a body that says
/// "hold" rather than claiming a window. Both accounts carry the hold a
/// transient upstream 429 arms (`mark_rate_limited`), 600s out.
#[tokio::test]
async fn exhausted_429_stays_plain_when_an_upstream_hold_binds() {
    let up_addr = spawn_scripted_upstream(vec![Some(raw_200())]).await;
    let manager = fleet(up_addr, &["a", "b"]);
    for idx in 0..2 {
        manager.mark_rate_limited(idx, 600);
    }
    let (status, headers, body) = post_one_full(manager).await;
    assert_eq!(status, 429, "body: {body}");
    assert!(
        headers.get("anthropic-ratelimit-unified-status").is_none(),
        "a hold must not be dressed up as a usage limit"
    );
    assert!(
        headers.get("retry-after").is_some(),
        "the live retry-after still ships"
    );
    assert!(
        body.contains("2 on an upstream hold") && body.contains("Retry in "),
        "the body names the hold: {body}"
    );
}

/// The bound. Every account in a five-account fleet is overloaded, so the request
/// walks accounts until its failover budget runs out — and then STOPS, instead of
/// cascading across the fleet and paying every account's in-place ladder on the
/// way (which, with every concurrent request doing the same, is how a partial
/// overload becomes a total one).
///
/// The fleet is deliberately LARGER than the budget allows, so the attempt count
/// separates a bounded walk (`1 + MAX_529_FAILOVERS_PER_REQUEST` accounts, each
/// getting exactly its in-place ladder) from a cascade over all five. The client
/// still receives the upstream's own 529 verbatim: the give-up SHAPE is unchanged,
/// it just arrives later.
#[tokio::test]
async fn overloaded_529_failover_is_bounded() {
    // One script entry, reused for every connection: nothing but 529s, anywhere.
    let (up_addr, attempts) = spawn_counted_upstream(vec![Some(raw_529())]).await;
    let manager = fleet(up_addr, &["a", "b", "c", "d", "e"]);

    let (status, body) = post_one_with_body(Arc::clone(&manager)).await;
    assert_eq!(
        status, 529,
        "with the whole fleet overloaded the failover cannot help, so the client \
             gets the 529"
    );
    assert!(
        body.contains("overloaded_error"),
        "give-up still forwards the upstream 529 VERBATIM, exactly as it did \
             before failover existed; a synthesized body would break clients that \
             parse it. Got: {body}"
    );

    let accounts_walked = 1 + MAX_529_FAILOVERS_PER_REQUEST as usize;
    let sends_per_account = MAX_SAME_ACCOUNT_529_RETRIES as usize + 1;
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        accounts_walked * sends_per_account,
        "{accounts_walked} accounts at {sends_per_account} sends each — two idle \
             accounts must be left untouched, or one overloaded request walks the fleet"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let held: Vec<&str> = snap
        .accounts
        .iter()
        .filter(|a| a.rate_limited_until.is_some())
        .map(|a| a.name.as_str())
        .collect();
    assert!(
        held.is_empty(),
        "however many accounts a 529 walks, it arms a hold on none of them: {held:?}"
    );
    assert_eq!(
        snap.accounts.iter().map(|a| a.requests).sum::<u64>(),
        1,
        "one client request is one terminal outcome, no matter how many accounts \
             it was sent to"
    );
}

/// Invariant 1 on the FAILOVER path specifically: benching an account in the
/// per-request `tried` set must not shade into benching it for everyone else.
/// After a completed failover, no account carries a rate-limit hold and none is
/// held out of rotation for any other reason either (an upstream `rejected` mark
/// included) — [`crate::stats::GateReason::Ok`] is exactly that claim, read from
/// the same `account_gate` the selector's hard gates come from.
///
/// The sibling of `overloaded_529_never_arms_a_hold_or_benches_the_account`, which
/// asserts the same thing on the give-up path where no failover happens at all.
#[tokio::test]
async fn overloaded_529_failover_arms_no_hold() {
    use crate::stats::GateReason;
    let up_addr = spawn_scripted_upstream(vec![
        Some(raw_529()),
        Some(raw_529()),
        Some(raw_529()),
        Some(raw_200()), // the failover target serves
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b", "c"]);

    assert_eq!(
        post_one(Arc::clone(&manager)).await,
        200,
        "precondition: the failover happened and served the client"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let held: Vec<&str> = snap
        .accounts
        .iter()
        .filter(|a| a.rate_limited_until.is_some())
        .map(|a| a.name.as_str())
        .collect();
    assert!(
        held.is_empty(),
        "a 529 is not a quota rejection — `mark_rate_limited` must never be \
             reached from this arm. Held: {held:?}"
    );
    let gated: Vec<(&str, GateReason)> = snap
        .accounts
        .iter()
        .filter(|a| a.gate != GateReason::Ok)
        .map(|a| (a.name.as_str(), a.gate))
        .collect();
    assert!(
        gated.is_empty(),
        "every account, the overloaded one included, must still be in rotation \
             for the next request. Gated: {gated:?}"
    );
}

/// THE BITING TEST for the soft-wait: the account the sleep was TIMED FOR must
/// actually be selectable when the sleep ends.
///
/// `Transient429::Park` benches an account twice — a hold via
/// `mark_rate_limited` AND an entry in `tried` — and `pick_eligible` /
/// `pick_least_loaded` both test `tried` BEFORE they evaluate eligibility. So
/// waiting out the hold alone changed nothing: `select()` returned `None` a
/// second time, the one-soft-wait guard blocked another wait, and the request
/// fell through to a revalidation-serve that skips `tried` too — an honest-looking
/// 429 handed to the client after a sleep that bought nothing.
///
/// One account, so "some account served" and "the account we waited for served"
/// are the same claim. Three transient 429s spend the two inline retries and then
/// park it for 1s; the fourth connection is the 200 that only exists if the
/// re-admission works. Pre-fix this test reads 429.
#[tokio::test]
async fn soft_wait_readmits_the_unparked_account() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        Some(raw_429_transient(1)), // attempt 1 — inline-wait 1s, same account
        Some(raw_429_transient(1)), // attempt 2 — inline budget spent
        Some(raw_429_transient(1)), // attempt 3 — park 1s + `tried`, fleet empty
        Some(raw_200()),            // attempt 4 — only reachable after re-admission
    ])
    .await;
    let manager = fleet(up_addr, &["a"]);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(
        status, 200,
        "the soft-wait slept for THIS account's un-park — it must be back in \
             rotation when the sleep ends, not still excluded by `tried`"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        4,
        "three 429s then one serve — no extra attempt, so the re-admission did \
             not turn the rotation loop into a retry spin"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let a = snap.accounts.iter().find(|x| x.name == "a").unwrap();
    assert_eq!(
        a.requests, 1,
        "the re-admitted account is the one that served"
    );
    assert!(
        a.rate_limited_until.is_none(),
        "the 200 proves the hold no longer binds, so it is cleared"
    );
}

/// The precision guard on the re-admission: ONLY a transient park is a timer.
///
/// `tried` carries accounts benched for reasons a clock never undoes — a 401
/// whose force-refresh produced no token, a transport blip, a dead token, a
/// durable quota rejection. Re-admitting those after the sleep would re-send this
/// request to an account that already failed it, which is how a recovery path
/// becomes a retry loop. So the sweep is keyed on WHY the entry is in `tried`,
/// not merely on "has no live hold" — `a` and `b` have no hold at all and must
/// still stay out.
///
/// LRU makes the failure loud rather than silent: `a` was stamped first, so a
/// sweep that cleared every hold-free entry would re-admit `a` and serve the 200
/// from it. The assertion is on WHICH account served.
#[tokio::test]
async fn soft_wait_does_not_readmit_a_401_or_transport_failure() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        Some(raw_401()),            // attempt 1 — `a`: refresh fails, benched in `tried`
        None,                       // attempt 2 — `b`: transport blip, benched in `tried`
        Some(raw_429_transient(1)), // attempt 3 — `c`: inline-wait
        Some(raw_429_transient(1)), // attempt 4 — `c`: inline budget spent
        Some(raw_429_transient(1)), // attempt 5 — `c`: park 1s + `tried`, fleet empty
        Some(raw_200()),            // attempt 6 — must be `c`, never `a` or `b`
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b", "c"]);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(status, 200, "the transiently parked `c` still recovers");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        6,
        "one attempt each on `a` and `b`, three on `c`, then `c`'s serve — a \
             re-admitted `a` or `b` would add attempts here"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let served: Vec<&str> = snap
        .accounts
        .iter()
        .filter(|x| x.requests > 0)
        .map(|x| x.name.as_str())
        .collect();
    assert_eq!(
        served,
        ["c"],
        "only the account parked by a HOLD may come back; the 401 and the \
             transport failure are not timers and stay benched for this request"
    );
}

/// Termination: re-admitting accounts must not turn the rotation loop into an
/// unbounded retry. Two things bound it and this asserts both.
///
/// The one-soft-wait-per-request guard means the sweep can run AT MOST ONCE, and
/// every iteration — the soft-waiting one included — still spends one unit of the
/// `max_attempts` budget. So when the re-admitted account immediately re-parks,
/// the second `select()` miss cannot wait again and control reaches the existing
/// exhausted-429 path.
///
/// `a` is durably quota-rejected (a 3600s hold, and never in the transient set);
/// `b` parks transiently, is re-admitted, and 429s straight back. Five upstream
/// attempts, then a 429 — the connection count is what proves it did not spin.
#[tokio::test]
async fn soft_wait_still_terminates_when_nothing_recovers() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        Some(raw_429_rejected(3600)), // attempt 1 — `a`: durable, no re-admission
        Some(raw_429_transient(1)),   // attempt 2 — `b`: inline-wait
        Some(raw_429_transient(1)),   // attempt 3 — `b`: inline budget spent
        Some(raw_429_transient(1)),   // attempt 4 — `b`: park 1s + `tried`
        Some(raw_429_transient(1)),   // attempt 5 — `b` re-admitted, 429s again
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b"]);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(
        status, 429,
        "nothing recovered, so the honest exhausted-429 is still the verdict"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        5,
        "the second select() miss cannot soft-wait again, so the re-admission \
             buys exactly ONE extra attempt and the loop ends"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let total: u64 = snap.accounts.iter().map(|x| x.requests).sum();
    assert_eq!(total, 0, "no account ever served this request");
}

/// The 502 predicate itself, over the four states the two counters can be in.
/// The bug was that the second row returned `true`.
/// The measured failure this exists to prevent: 42 of 52 quota rejections on
/// 2026-08-04 armed an IDENTICAL 3600s hold, eight accounts inside four
/// seconds, so the whole fleet would have un-parked as one wave.
#[test]
fn a_ceiling_clamped_hold_is_spread_but_a_genuine_one_is_verbatim() {
    // AT THE CEILING: upstream asked for >= 3600 and we were already going to
    // return early, so spreading inside [3600-N, 3600] costs nothing and
    // desyncs the herd. Distinct nanos MUST yield distinct holds.
    let spread: HashSet<i64> = (0..8)
        .map(|k| jittered_quota_hold(7200, k * 111_111_111))
        .collect();
    assert!(
        spread.len() > 1,
        "eight accounts rejected together must not share one hold: {spread:?}"
    );
    for h in &spread {
        assert!(
            (MAX_QUOTA_HOLD_SECS - QUOTA_HOLD_JITTER_MAX_SECS..=MAX_QUOTA_HOLD_SECS).contains(h),
            "stays inside the envelope we already chose: {h}"
        );
    }

    // BELOW THE CEILING: armed verbatim. Returning an account before upstream
    // said it may come back is the one thing this path must never do — the
    // 829s hold observed live must stay 829s.
    assert_eq!(jittered_quota_hold(829, 999_999_999), 829);
    assert_eq!(jittered_quota_hold(1, 999_999_999), 1);
    // And a nonsense/negative header can never produce a zero-length hold.
    assert_eq!(jittered_quota_hold(0, 12_345), 1);
    assert_eq!(jittered_quota_hold(-5, 12_345), 1);
}

#[test]
fn bad_gateway_only_when_nothing_reached_an_upstream() {
    assert!(
        every_attempt_transport_failed(2, 0),
        "blips only, nothing answered → 502"
    );
    assert!(
        !every_attempt_transport_failed(1, 1),
        "an upstream answered → the blip is not the whole story (the live bug)"
    );
    assert!(
        !every_attempt_transport_failed(0, 3),
        "no transport failure at all → never a 502"
    );
    assert!(
        !every_attempt_transport_failed(0, 0),
        "no attempt made → exhausted, not unreachable"
    );
}

/// Both terminals (select finding nothing, and the attempt budget running out) read one
/// verdict off the tallies. The budget terminal used to answer a transport-only walk with a
/// 502 whatever the resolver said, and an all-refused walk with the exhausted 429.
#[tokio::test]
async fn the_terminal_verdict_reads_every_arm_off_the_tallies() {
    use super::ladder::Tally;

    struct NoRefresh;
    impl crate::oauth::TokenRefresher for NoRefresh {
        fn refresh(&self, _provider: Provider, _t: String) -> crate::oauth::RefreshFuture {
            Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
        }
    }
    let manager = Manager::new(
        dummy_config(None, "http://127.0.0.1:9"),
        Arc::new(NoRefresh),
        Arc::new(crate::probe::LiveUsageProber::new()),
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );
    let now = OffsetDateTime::now_utc();
    // The status and `x-should-retry`, which tell the four shapes apart: the offline 503
    // carries none, `bad_gateway`'s 503 says `true`, its 502 says `false`, and the
    // credential-refused 502 carries none.
    let verdict = |mut tally: Tally| {
        tally
            .verdict(&manager, Provider::Anthropic, None, now, None, None)
            .map(|response| {
                let should_retry = response
                    .headers()
                    .get("x-should-retry")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                (response.status(), should_retry)
            })
    };

    let offline = Tally {
        transport_failures: 2,
        offline_failures: 1,
        ..Tally::default()
    };
    assert_eq!(
        verdict(offline),
        Some((StatusCode::SERVICE_UNAVAILABLE, None)),
        "a resolver failure in a transport-only walk is the offline 503"
    );
    let connect_only = Tally {
        transport_failures: 2,
        ..Tally::default()
    };
    assert_eq!(
        verdict(connect_only),
        Some((StatusCode::SERVICE_UNAVAILABLE, Some("true".into())))
    );
    let unknown_outcome = Tally {
        transport_failures: 1,
        unknown_outcome_transport_failure: true,
        ..Tally::default()
    };
    assert_eq!(
        verdict(unknown_outcome),
        Some((StatusCode::BAD_GATEWAY, Some("false".into())))
    );
    let all_refused = Tally {
        upstream_responses: 2,
        refused_403: vec![(0, Some("a".into())), (1, Some("b".into()))],
        ..Tally::default()
    };
    assert_eq!(
        verdict(all_refused),
        Some((StatusCode::BAD_GATEWAY, None)),
        "every answer a refused 403 is the credential-refused 502"
    );
    let answered = Tally {
        transport_failures: 1,
        upstream_responses: 2,
        refused_403: vec![(0, None)],
        ..Tally::default()
    };
    assert_eq!(
        verdict(answered),
        None,
        "an upstream that answered other than with a refusal leaves the caller's last resort"
    );
}
