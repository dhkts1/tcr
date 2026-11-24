//! Token refresh: coalescing, cooldowns, forced refreshes, and the login errors a refusal leaves.

use super::*;

/// A [`CountingRefresher`] and the count it keeps.
fn refresh_counter() -> (Arc<CountingRefresher>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let refresher = Arc::new(CountingRefresher {
        calls: Arc::clone(&calls),
    });
    (refresher, calls)
}

/// A refresher whose FIRST call fails transiently (the leader — after a
/// short delay so the herd queues) and whose every later call succeeds.
/// Records the refresh token `POSTed` on each call so a test can assert the
/// single-use token was sent exactly once across a concurrent batch.
struct TransientThenOkRefresher {
    calls: Arc<AtomicUsize>,
    tokens_seen: Arc<Mutex<Vec<String>>>,
}

impl TokenRefresher for TransientThenOkRefresher {
    fn refresh(
        &self,
        _provider: crate::provider::Provider,
        refresh_token: String,
    ) -> RefreshFuture {
        let calls = Arc::clone(&self.calls);
        let tokens_seen = Arc::clone(&self.tokens_seen);
        Box::pin(async move {
            tokens_seen
                .lock()
                .expect("tokens_seen lock poisoned")
                .push(refresh_token);
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // Leader: hold the coalescing lock long enough for the herd
                // to queue, then fail transiently (access token unchanged).
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                Err::<Tokens, OAuthError>(OAuthError::Transient("simulated network blip".into()))
            } else {
                Ok(Tokens {
                    id_token: None,
                    access_token: "fresh-access".to_string(),
                    refresh_token: Some("fresh-refresh".to_string()),
                    expires_at_ms: crate::now_ms() + 3_600_000,
                })
            }
        })
    }
}

/// A prober that never hits the network — the default for selection/refresh
/// tests that do not exercise probing.
/// Gil, 2026-07-17: "are error accounts rechecked?" — they must be. An `Error`
/// row whose refresh now SUCCEEDS is recovered by the probe, because a successful
/// refresh is proof the credential was never dead (a dead one cannot produce one).
/// Before this, `Error` was a life sentence: nothing probed or selected an errored
/// row and only a refresh cleared it, so one transient rejection sidelined a
/// healthy account until restart — observed live with 7/7 tokens still probing 200.
#[tokio::test]
async fn errored_account_is_rechecked_and_recovers_when_its_refresh_succeeds() {
    let calls = Arc::new(AtomicUsize::new(0));
    let manager = build_manager_with_prober(
        config_with(vec![account("stuck", 0)]),
        Arc::new(CountingRefresher {
            calls: Arc::clone(&calls),
        }),
        Arc::new(NoProber),
    );

    manager.mark_error(0);
    assert_eq!(
        manager.account_status(0),
        Some(AccountStatus::Error),
        "precondition: the row is errored"
    );
    assert!(
        manager.probeable_indices().is_empty(),
        "while its recovery backoff is live an errored row stays sidelined"
    );

    // Its backoff elapses …
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].error_retry_after_ms = Some(crate::now_ms() - 1);
    }
    assert_eq!(
        manager.probeable_indices(),
        vec![0],
        "once the backoff elapses the row IS re-probed"
    );

    // … and the recovery attempt's refresh succeeds → back in rotation, no restart.
    manager.probe_all().await;
    assert_eq!(
        manager.account_status(0),
        Some(AccountStatus::Active),
        "an errored row whose refresh now succeeds must be RECOVERED — a successful \
         refresh proves it was never dead"
    );
}

/// A GENUINELY dead credential must never reach `Active`. Its recovery refresh keeps
/// being rejected, so it stays `Error` and the backoff doubles — which is why
/// re-probing cannot "silently re-insert a dead account into rotation".
#[tokio::test]
async fn permanently_rejected_account_never_recovers_and_its_backoff_grows() {
    struct AlwaysRejects;
    impl TokenRefresher for AlwaysRejects {
        fn refresh(
            &self,
            _provider: crate::provider::Provider,
            _refresh_token: String,
        ) -> RefreshFuture {
            Box::pin(async {
                Err(OAuthError::AuthRejected {
                    status: 400,
                    code: None,
                })
            })
        }
    }
    let manager = build_manager_with_prober(
        config_with(vec![account("dead", 0)]),
        Arc::new(AlwaysRejects),
        Arc::new(NoProber),
    );

    manager.mark_error(0);
    let first_backoff = manager.accounts.read().expect("lock")[0].error_backoff_ms;
    assert_eq!(
        first_backoff, ERROR_REPROBE_BASE_MS,
        "the first rejection arms the base backoff"
    );

    for _ in 0..3 {
        {
            let mut a = manager.accounts.write().expect("accounts lock poisoned");
            a[0].error_retry_after_ms = Some(crate::now_ms() - 1);
        }
        manager.probe_all().await;
        assert_eq!(
            manager.account_status(0),
            Some(AccountStatus::Error),
            "a dead credential must NEVER be revived by a re-probe"
        );
    }

    let grown = manager.accounts.read().expect("lock")[0].error_backoff_ms;
    assert!(
        grown > first_backoff,
        "each rejected recovery must back off harder (was {first_backoff}, now {grown}) \
         so a dead row can never hammer the OAuth endpoint"
    );
    assert!(
        grown <= ERROR_REPROBE_CAP_MS,
        "the backoff is capped at {ERROR_REPROBE_CAP_MS}, got {grown}"
    );
}

/// THE test for the handoff's safety property. A refresh token is
/// single-use, so once this process has given its listening socket to a
/// successor, the successor holds the same tokens and is the only one
/// entitled to rotate them. A draining request that reaches `ensure_fresh`
/// here would invalidate the successor's copy.
///
/// Two managers, identical but for the release, because the obvious
/// one-manager shape does not test anything. Calling `ensure_fresh_force`
/// once as a precondition and again after releasing looks like a control,
/// but a FORCED SUCCESS re-arms the refresh cooldown
/// (`refresh.rs`, `cooldown_after = force.then(..)`), and `refresh_plan`
/// then declines the second call on cooldown grounds before it ever reaches
/// the gate. That version passes with the gate deleted. This one was watched
/// failing without it.
///
/// The force path is used deliberately: plain `ensure_fresh` would decline
/// on expiry grounds, the fixture's token having an hour left, and pass for
/// the wrong reason again.
#[tokio::test]
async fn a_released_manager_refuses_to_refresh_even_when_forced() {
    // Control: identical manager, ownership NOT released.
    let control_calls = Arc::new(AtomicUsize::new(0));
    let control = build_manager(
        config_with(vec![account("owning", 0)]),
        Arc::new(CountingRefresher {
            calls: Arc::clone(&control_calls),
        }),
    );
    assert!(
        control.ensure_fresh_force(0).await,
        "control: a force refresh must succeed while ownership is held"
    );
    assert_eq!(
        control_calls.load(Ordering::SeqCst),
        1,
        "control: the force must actually reach the refresher, or the \
         subject's zero below proves nothing"
    );

    // Subject: same fixture, same call, ownership released first.
    let calls = Arc::new(AtomicUsize::new(0));
    let manager = build_manager(
        config_with(vec![account("draining", 0)]),
        Arc::new(CountingRefresher {
            calls: Arc::clone(&calls),
        }),
    );
    manager.release_mutation_ownership();

    let applied = manager.ensure_fresh_force(0).await;

    assert!(
        !applied,
        "a released manager reported that it applied a refresh"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a released manager sent a SINGLE-USE refresh token upstream; the \
         successor holds that same token and its copy is now dead"
    );
}

/// A refresher that rejects its first refresh as a reused token, and succeeds after.
struct RejectsOnce {
    calls: Arc<AtomicUsize>,
}
impl TokenRefresher for RejectsOnce {
    fn refresh(
        &self,
        _provider: crate::provider::Provider,
        _refresh_token: String,
    ) -> RefreshFuture {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                Err(OAuthError::AuthRejected {
                    status: 400,
                    code: Some("refresh_token_reused"),
                })
            } else {
                Ok(Tokens {
                    id_token: None,
                    access_token: "at-fresh".to_string(),
                    refresh_token: Some("rt-fresh".to_string()),
                    expires_at_ms: crate::now_ms() + 3_600_000,
                })
            }
        })
    }
}

/// **Why a Codex login is dead reaches the wire.** A refresh rejected with an OAuth code
/// records that code; one rejected with none records its status. The row goes to `error`
/// as any rejected row does. An Anthropic row rejected the same way records nothing, so
/// its wire row gains no key.
///
/// Watched red: without the store in the rejection arm, the Codex row's error is `None`.
#[tokio::test]
async fn a_reused_refresh_token_surfaces_as_a_login_error() {
    let login_error = |status: u16, code: Option<&'static str>, codex: bool| {
        let row = if codex {
            codex_account("bob@example.com", 0)
        } else {
            account("alice@example.com", 0)
        };
        let manager = build_manager(
            config_with(vec![row]),
            Arc::new(RejectsWith { status, code }),
        );
        async move {
            manager.ensure_fresh_force(0).await;
            assert_eq!(manager.account_status(0), Some(AccountStatus::Error));
            let snap = manager.snapshot(OffsetDateTime::now_utc());
            assert_eq!(snap.accounts[0].status, "error");
            snap.accounts[0].codex.login_error.clone()
        }
    };
    assert_eq!(
        login_error(400, Some("refresh_token_reused"), true).await,
        Some("refresh_token_reused".to_string())
    );
    assert_eq!(
        login_error(401, None, true).await,
        Some("http_401".to_string())
    );
    assert_eq!(
        login_error(400, Some("refresh_token_reused"), false).await,
        None,
        "an Anthropic row records no login error"
    );
}

/// **A live login clears it.** The next successful refresh clears the recorded error, and
/// so does a re-login of the row through `add_or_update_account`.
///
/// Watched red: without the clear in `apply_refresh`, the reused-token error outlives the
/// refresh that proved the login alive.
#[tokio::test]
async fn a_successful_refresh_clears_the_login_error() {
    let manager = build_manager(
        config_with(vec![codex_account("bob@example.com", 0)]),
        Arc::new(RejectsOnce {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    );
    let error_of = |manager: &Manager| {
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
            .codex
            .login_error
            .clone()
    };
    manager.ensure_fresh_force(0).await;
    assert_eq!(error_of(&manager), Some("refresh_token_reused".to_string()));
    assert!(
        manager.ensure_fresh_force(0).await,
        "the second refresh succeeds"
    );
    assert_eq!(error_of(&manager), None, "a refresh that worked clears it");

    let relogin = build_manager(
        config_with(vec![codex_account("bob@example.com", 0)]),
        Arc::new(RejectsWith {
            status: 400,
            code: Some("refresh_token_reused"),
        }),
    );
    relogin.ensure_fresh_force(0).await;
    assert_eq!(error_of(&relogin), Some("refresh_token_reused".to_string()));
    let fresh = Account {
        access_token: "at-relogin".to_string(),
        refresh_token: Some("rt-relogin".to_string()),
        ..codex_account("bob@example.com", 0)
    };
    assert!(matches!(
        relogin.add_or_update_account(fresh),
        AddAccountOutcome::Updated { idx: 0, .. }
    ));
    assert_eq!(error_of(&relogin), None, "a re-login clears it");
}

/// Refresh coalescing: N concurrent `ensure_fresh` calls on the SAME
/// hard-expired account trigger exactly ONE upstream refresh.
/// A refresher that reports when the upstream POST has begun and then waits to
/// be released — the shape of a real refresh that is in flight when the client
/// hangs up.
struct BlockingRefresher {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl TokenRefresher for BlockingRefresher {
    fn refresh(
        &self,
        _provider: crate::provider::Provider,
        _refresh_token: String,
    ) -> RefreshFuture {
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            Ok::<Tokens, OAuthError>(Tokens {
                id_token: None,
                access_token: "fresh-access".to_string(),
                refresh_token: Some("fresh-refresh".to_string()),
                expires_at_ms: crate::now_ms() + 3_600_000,
            })
        })
    }
}

/// A refresh token is single-use: the moment the POST leaves this box, upstream
/// may rotate it whether or not we are still listening. So the refresh must not
/// live inside the request future that asked for it — a client that hangs up
/// mid-refresh would drop it, and the next attempt would re-POST a token
/// upstream had already spent, get `400`, and mark a HEALTHY account `Error`,
/// which is terminal.
///
/// Live proof this is not hypothetical (2026-09-16 proxy log): an account logged
/// `refreshing OAuth token` at 19:07:00.191 and never logged an outcome; the next
/// attempt, 57s later, was rejected, and the account was condemned.
#[tokio::test]
async fn a_cancelled_caller_does_not_abandon_its_refresh() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let refresher = Arc::new(BlockingRefresher {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });
    let mut acct = account("expired", 0);
    acct.expires_at = Some(crate::now_ms() - 60_000);
    let manager = build_manager(config_with(vec![acct]), refresher);

    let m = Arc::clone(&manager);
    let caller = tokio::spawn(async move { m.ensure_fresh_force(0).await });
    started.notified().await; // the POST is in flight upstream
    caller.abort(); // the client hung up
    crate::log_err::LogErr::log_debug(caller.await, "joining the aborted caller");
    release.notify_one(); // upstream answers, as it would have anyway

    // The answer must still be applied. Polled, because the refresh now runs on
    // its own task and finishes a moment after the release.
    let mut applied = None;
    for _ in 0..200 {
        if manager.access_token(0).as_deref() == Some("fresh-access") {
            applied = manager.access_token(0);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        applied.as_deref(),
        Some("fresh-access"),
        "a refresh whose caller was cancelled must still apply its answer — \
         otherwise the rotated token is lost and the next attempt burns the account"
    );
}

#[tokio::test]
async fn concurrent_ensure_fresh_coalesces_to_single_refresh() {
    let (refresher, calls) = refresh_counter();
    let mut acct = account("expired", 0);
    acct.expires_at = Some(crate::now_ms() - 60_000); // already expired
    let manager = build_manager(config_with(vec![acct]), refresher);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move { m.ensure_fresh(0).await }));
    }
    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // And the token is now the refreshed one.
    assert_eq!(manager.access_token(0).as_deref(), Some("fresh-access"));
}

/// Finding #10: the FORCE path coalesces too. N concurrent `ensure_fresh_force`
/// on the same (not-yet-expired) account trigger exactly ONE upstream refresh,
/// so a burst of 401s never rotates the refresh token N times back-to-back.
#[tokio::test]
async fn concurrent_ensure_fresh_force_coalesces_to_single_refresh() {
    let (refresher, calls) = refresh_counter();
    // A perfectly valid (unexpired) token — only `force` drives the refresh.
    let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move { m.ensure_fresh_force(0).await }));
    }
    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(manager.access_token(0).as_deref(), Some("fresh-access"));
}

/// A refresher that records the provider each refresh was asked for, then mints a
/// fresh token so the refresh applies.
struct ProviderRecordingRefresher {
    providers: Arc<Mutex<Vec<crate::provider::Provider>>>,
}

impl TokenRefresher for ProviderRecordingRefresher {
    fn refresh(
        &self,
        provider: crate::provider::Provider,
        _refresh_token: String,
    ) -> RefreshFuture {
        self.providers
            .lock()
            .expect("providers lock poisoned")
            .push(provider);
        Box::pin(async {
            Ok::<Tokens, OAuthError>(Tokens {
                id_token: None,
                access_token: "fresh-access".to_string(),
                refresh_token: Some("fresh-refresh".to_string()),
                expires_at_ms: crate::now_ms() + 3_600_000,
            })
        })
    }
}

/// The manager asks for each account's refresh under that account's own provider,
/// because the live refresher sends the token to that provider's endpoint: a Codex
/// refresh token posted to Anthropic is rejected, and a rejection marks a healthy
/// account `Error`.
#[tokio::test]
async fn a_refresh_is_requested_for_the_accounts_own_provider() {
    let providers = Arc::new(Mutex::new(Vec::new()));
    let mut codex = account("codex-row", 1);
    codex.provider = crate::provider::Provider::Codex;
    let manager = build_manager(
        config_with(vec![account("anthropic-row", 0), codex]),
        Arc::new(ProviderRecordingRefresher {
            providers: Arc::clone(&providers),
        }),
    );

    assert!(
        manager.ensure_fresh_force(1).await,
        "precondition: the Codex row's refresh must apply"
    );
    assert!(
        manager.ensure_fresh_force(0).await,
        "precondition: the Anthropic row's refresh must apply"
    );

    assert_eq!(
        *providers.lock().expect("providers lock poisoned"),
        vec![
            crate::provider::Provider::Codex,
            crate::provider::Provider::Anthropic
        ],
        "each refresh must be requested under its own account's provider"
    );
}

/// The refresh coalescing lock now lives on `AccountRuntime` itself rather than
/// in a parallel `Vec` sized at construction — an account appended after
/// startup via [`Manager::add_account`] must still refresh. Before this fix,
/// the appended account's index was beyond `refresh_locks.len()` and
/// `ensure_fresh_inner` returned `false` SILENTLY (no log, no error) — this
/// test proves refresh actually fires for it, not just that the call returns.
#[tokio::test]
async fn appended_account_refreshes() {
    let (refresher, calls) = refresh_counter();
    // Seed with one pre-existing account so the appended one is NOT index 0 —
    // the desync this guards against only bites an index beyond the
    // construction-time Vec's length.
    let manager = build_manager(config_with(vec![account("seed", 0)]), refresher);

    let mut appended = account("appended", 0);
    appended.expires_at = Some(crate::now_ms() - 60_000); // already expired
    let idx = manager.add_account(appended);
    assert_eq!(idx, 1, "appended account must land at the next index");

    manager.ensure_fresh(idx).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "refresh must actually fire for an account appended after construction"
    );
    assert_eq!(
        manager.access_token(idx).as_deref(),
        Some("fresh-access"),
        "the appended account's access token must be the refreshed one"
    );
}

/// Transient-leader coalescing (the bug this fixes): when the leader's
/// refresh fails TRANSIENTLY the access token is UNCHANGED, so the
/// access-token guard alone can't stop a follower already queued on the lock
/// from re-POSTing the SAME single-use refresh token. The self-clearing
/// cooldown must collapse the concurrent batch to exactly ONE upstream POST —
/// and still permit a later sequential retry (no suppress-forever).
#[tokio::test]
async fn transient_leader_coalesces_refresh_and_cooldown_self_clears() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tokens_seen = Arc::new(Mutex::new(Vec::new()));
    let refresher = Arc::new(TransientThenOkRefresher {
        calls: Arc::clone(&calls),
        tokens_seen: Arc::clone(&tokens_seen),
    });
    let mut acct = account("expired", 0);
    acct.expires_at = Some(crate::now_ms() - 60_000); // hard-expired → refresh planned
    let manager = build_manager(config_with(vec![acct]), refresher);

    // A burst of concurrent refreshers. The leader fails transiently; every
    // follower must be coalesced by the cooldown, NOT re-POST the token.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move { m.ensure_fresh(0).await }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // Exactly ONE upstream POST across the whole batch...
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a transient-leader failure must still coalesce the batch to ONE refresh POST"
    );
    // ...and the token POSTed was the single-use refresh token, sent once.
    assert_eq!(
        *tokens_seen.lock().unwrap(),
        vec!["rt-expired".to_string()],
        "the single-use refresh token must be sent exactly once across the batch"
    );
    // The transient failure left the access token unchanged.
    assert_eq!(manager.access_token(0).as_deref(), Some("at-expired"));

    // Self-clearing: once the cooldown window elapses, a later call refreshes
    // again. Rewind the stamp into the past to simulate the elapsed window
    // deterministically (crate::now_ms is wall-clock, so a real wait would
    // cost the full cooldown); this exercises the same time-based guard.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].refresh_retry_after_ms = Some(crate::now_ms() - 1);
    }
    manager.ensure_fresh(0).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "after the cooldown self-clears, a later sequential refresh must be allowed"
    );
    assert_eq!(manager.access_token(0).as_deref(), Some("fresh-access"));
}

#[tokio::test]
async fn ensure_fresh_skips_when_token_still_valid() {
    let (refresher, calls) = refresh_counter();
    // Default account expiry is 1h out — not hard-expired.
    let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);
    manager.ensure_fresh(0).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// U1 (storm fix): a token within the proactive `EXPIRING_SOON` buffer refreshes
/// on a NON-force `ensure_fresh` (before it hard-expires), while a token
/// comfortably outside the buffer is still left alone.
#[tokio::test]
async fn ensure_fresh_refreshes_expiring_soon_but_not_fresh() {
    // now+60s is inside the 5-min buffer → proactive non-force refresh fires.
    let soon_calls = Arc::new(AtomicUsize::new(0));
    let mut soon = account("soon", 0);
    soon.expires_at = Some(crate::now_ms() + 60_000);
    let soon_mgr = build_manager(
        config_with(vec![soon]),
        Arc::new(CountingRefresher {
            calls: Arc::clone(&soon_calls),
        }),
    );
    soon_mgr.ensure_fresh(0).await;
    assert_eq!(
        soon_calls.load(Ordering::SeqCst),
        1,
        "a token expiring within the 5-min buffer must refresh proactively"
    );
    assert_eq!(soon_mgr.access_token(0).as_deref(), Some("fresh-access"));

    // now+10min is outside the buffer → no refresh.
    let fresh_calls = Arc::new(AtomicUsize::new(0));
    let mut fresh = account("fresh", 0);
    fresh.expires_at = Some(crate::now_ms() + 600_000);
    let fresh_mgr = build_manager(
        config_with(vec![fresh]),
        Arc::new(CountingRefresher {
            calls: Arc::clone(&fresh_calls),
        }),
    );
    fresh_mgr.ensure_fresh(0).await;
    assert_eq!(
        fresh_calls.load(Ordering::SeqCst),
        0,
        "a token outside the 5-min buffer must not refresh"
    );
}

/// U2a (storm fix): a SUCCESSFUL forced refresh must LEAVE the refresh cooldown
/// armed — `apply_refresh` installs the token pair and the re-arm deadline in a
/// single write (the forced deadline is passed in as `cooldown_after`), so the
/// throttle can never be clobbered by a success-path clear.
#[tokio::test]
async fn forced_success_re_arms_refresh_cooldown() {
    let manager = manager_of(vec![account("valid", 0)]);
    let before = crate::now_ms();
    assert!(
        manager.ensure_fresh_force(0).await,
        "a forced refresh that rotated the token returns true"
    );
    let until = {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        accounts[0].refresh_retry_after_ms
    };
    assert!(
        until.is_some_and(|u| u >= before),
        "a successful forced refresh must leave the cooldown armed"
    );
}

/// U2b (storm fix): two sequential `ensure_fresh_force` on a non-expired account
/// within the cooldown window collapse to exactly ONE upstream POST.
#[tokio::test]
async fn sequential_forced_refresh_within_cooldown_posts_once() {
    let (refresher, calls) = refresh_counter();
    let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);
    assert!(
        manager.ensure_fresh_force(0).await,
        "the first forced refresh rotates the token"
    );
    assert!(
        !manager.ensure_fresh_force(0).await,
        "a second forced refresh inside the cooldown is suppressed (no new token)"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the cooldown collapses back-to-back forced refreshes to ONE POST"
    );
}

/// U2c (storm fix): the hard-expired force override in `refresh_plan` is intact.
/// A hard-expired token under `force` is PLANNED for refresh even during a live
/// cooldown (the `!(force && is_expired)` gate), so a dead-in-hand token is never
/// pinned un-refreshable by the throttle. The inner coalescing guard still
/// collapses a *concurrent* batch to ONE POST — that guard must NOT get the same
/// override, or a follower would re-POST the single-use refresh token
/// (`transient_leader_coalesces_refresh_and_cooldown_self_clears` is the gate);
/// the throttle instead self-clears by time (`..._after_cooldown_elapses`).
#[test]
fn hard_expired_force_override_intact_in_plan() {
    let mut acct = account("expired", 0);
    acct.expires_at = Some(crate::now_ms() - 60_000);
    let manager = manager_of(vec![acct]);
    let now = crate::now_ms();
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].refresh_retry_after_ms = Some(now + REFRESH_RETRY_COOLDOWN_MS);
    }
    assert!(
        manager.refresh_plan(0, true, now).is_some(),
        "a hard-expired token under force must be planned for refresh despite a live cooldown"
    );

    // The contrasting half: a NON-force expiring-soon plan during the same live
    // cooldown STAYS suppressed (the U1 proactive path never overrides).
    let mut soon = account("soon", 0);
    soon.expires_at = Some(now + 60_000);
    let soon_mgr = manager_of(vec![soon]);
    {
        let mut accounts = soon_mgr.accounts.write().expect("accounts lock poisoned");
        accounts[0].refresh_retry_after_ms = Some(now + REFRESH_RETRY_COOLDOWN_MS);
    }
    assert!(
        soon_mgr.refresh_plan(0, false, now).is_none(),
        "a non-force expiring-soon plan during a live cooldown stays suppressed"
    );
}

/// U2d (storm fix): once the re-armed cooldown elapses, a later forced refresh
/// is allowed again — the throttle self-clears by time, never suppress-forever.
#[tokio::test]
async fn forced_refresh_allowed_after_cooldown_elapses() {
    let (refresher, calls) = refresh_counter();
    let manager = build_manager(config_with(vec![account("valid", 0)]), refresher);
    assert!(manager.ensure_fresh_force(0).await);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Rewind the re-armed cooldown into the past to simulate elapse
    // deterministically (crate::now_ms is wall-clock).
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].refresh_retry_after_ms = Some(crate::now_ms() - 1);
    }
    assert!(
        manager.ensure_fresh_force(0).await,
        "once the cooldown elapses a forced refresh is allowed again"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
