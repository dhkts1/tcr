//! Keep-warm: warm targets, the warm loop, and its quota-evidence latch.

use super::*;

/// A prober that SUCCEEDS for every account and reports a weekly window only —
/// no 5h bucket at all. That is the shape that separates the two candidate boot
/// predicates: the quota was genuinely read (so `quota_known` latches and the
/// account is warm-eligible), while `quota.five_hour` stays `None` because
/// `apply_bucket` early-returns on an absent bucket. Gating on
/// `five_hour.is_some()` would leave such an account warm-INELIGIBLE forever.
struct ColdOkProber;
impl UsageProber for ColdOkProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        Box::pin(async {
            Ok(Usage {
                five_hour: crate::probe::WindowReading::Unread,
                seven_day: Some(UsageBucket {
                    utilization: Some(0.10),
                    reset_at_ms: Some(crate::now_ms() + 86_400_000),
                }),
                seven_day_oi: None,
                extra_usage_usd: None,
                codex: None,
                claude_resets: None,
            })
        })
    }
}

/// A warmer that records every access token it was asked to warm (so the warm
/// sweep's targeting can be asserted) and returns headers carrying a live 5h
/// window — exactly what a real warm response yields for the fold-back.
struct RecordingWarmer {
    warmed: Arc<Mutex<Vec<String>>>,
}
impl AccountWarmer for RecordingWarmer {
    fn warm(&self, access_token: String, _upstream: String) -> WarmFuture {
        let warmed = Arc::clone(&self.warmed);
        Box::pin(async move {
            warmed
                .lock()
                .expect("warmed lock poisoned")
                .push(access_token);
            let reset = (OffsetDateTime::now_utc() + Duration::hours(5)).unix_timestamp();
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.01".parse().unwrap(),
            );
            h.insert(
                "anthropic-ratelimit-unified-5h-reset",
                reset.to_string().parse().unwrap(),
            );
            Ok(h)
        })
    }
}

/// A warmer that succeeds (never a `WarmError`) but returns EMPTY headers — no
/// `anthropic-ratelimit-unified-5h-*` at all. Models a 200 response that
/// carries none of the unified rate-limit headers (an intermediary that
/// strips them, or a response shape the warm endpoint does not always emit):
/// the "silence" case [`Manager::update_quota`]'s doc-comment distinguishes
/// from evidence — a warm that latches nothing.
struct HeaderlessWarmer;
impl AccountWarmer for HeaderlessWarmer {
    fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
        Box::pin(async { Ok(reqwest::header::HeaderMap::new()) })
    }
}

fn build_manager_with_warmer(
    config: Config,
    refresher: Arc<dyn TokenRefresher>,
    warmer: Arc<dyn AccountWarmer>,
) -> Arc<Manager> {
    Manager::new(config, refresher, Arc::new(NoProber), warmer, None)
}

/// Drive account `idx`'s 5-hour window to `util` with a reset `hours_from_now`
/// (negative = past reset), via the real rate-limit headers the proxy learns.
fn set_5h(manager: &Manager, idx: usize, util: &str, hours_from_now: i64) {
    let now = OffsetDateTime::now_utc();
    let reset = (now + Duration::hours(hours_from_now)).unix_timestamp();
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        util.parse().unwrap(),
    );
    h.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.to_string().parse().unwrap(),
    );
    manager.update_quota(idx, &h, 200);
}

/// Build a config carrying an unmodelled top-level `warmupSeconds` value.
fn config_with_warmup(accounts: Vec<Account>, warmup_seconds: i64) -> Config {
    let mut config = config_with(accounts);
    config.extra.insert(
        "warmupSeconds".to_string(),
        serde_json::Value::from(warmup_seconds),
    );
    config
}

/// Build a config carrying an unmodelled top-level `quotaProbeSeconds` value.
/// `0` turns probing OFF, which is what makes the keep-warm boot gate fall
/// back to its pre-gate behaviour.
fn config_with_probe_seconds(accounts: Vec<Account>, probe_seconds: i64) -> Config {
    let mut config = config_with(accounts);
    config.extra.insert(
        "quotaProbeSeconds".to_string(),
        serde_json::Value::from(probe_seconds),
    );
    config
}

/// Mark every account as having had its quota successfully READ at least once.
///
/// `warm_targets` skips an account whose quota was never read while probing is
/// enabled (blank quota is unknown, not known-cold), and `config_with` leaves
/// `quotaProbeSeconds` at its default (nonzero) — so without this, every keep-warm test
/// below would be measuring the boot gate instead of the predicate it is
/// actually about.
///
/// `set_5h` now latches `quota_known` by itself (a response's 5h header is
/// evidence about the window, and `update_quota` treats it as such), so calling
/// this stays necessary only for the accounts a test does NOT drive with
/// `set_5h` — including every account driven by `set_7d`, which reports no 5h
/// window and so latches nothing. `probe_status` is still only ever set by a
/// probe.
fn mark_all_probed(manager: &Manager) {
    let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
    for account in accounts.iter_mut() {
        account.probe_status = ProbeStatus::Ok;
        account.quota_known = true;
    }
}

// ---- keep-warm (opt-in idle-account warming) ----

/// `warmupSeconds` reads OFF by default (absent → 0), reads the configured
/// value, and treats `<= 0` as disabled.
#[test]
fn warmup_interval_defaults_off_reads_value_and_disables_on_nonpositive() {
    let refresher = || counting_refresher();
    // Absent → 0 (dark default).
    let m = build_manager(config_with(vec![account("a", 0)]), refresher());
    assert_eq!(m.warmup_interval_seconds(), 0);
    // A positive value is read through.
    let m = build_manager(config_with_warmup(vec![account("a", 0)], 900), refresher());
    assert_eq!(m.warmup_interval_seconds(), 900);
    // A negative value disables (clamped to 0).
    let m = build_manager(config_with_warmup(vec![account("a", 0)], -5), refresher());
    assert_eq!(m.warmup_interval_seconds(), 0);
}

/// `warm_targets` includes only healthy idle OAuth accounts: a disabled,
/// errored, or throttled account is never a target.
#[test]
fn warm_targets_skips_disabled_errored_and_throttled() {
    let mut api = account("api", 0);
    api.account_type = config::AccountType::Unrecognized("api".to_string());
    let manager = manager_of(vec![
        account("idle", 0),
        account("off", 0),
        account("dead", 0),
        account("throttled", 0),
        api, // non-oauth
    ]);
    mark_all_probed(&manager); // isolate from the never-probed boot gate
    manager.set_disabled(1, true);
    manager.mark_error(2);
    manager.mark_rate_limited(3, 300); // → Throttled

    assert_eq!(
        manager.warm_targets(),
        vec![0],
        "only the healthy idle OAuth account is a warm target"
    );
}

/// Core contract: an account whose 5h window is LIVE (a future reset) is already
/// warm and skipped; a cold account (no 5h data) or one whose 5h reset has
/// passed IS a target.
#[test]
fn warm_targets_skips_live_5h_window_keeps_cold_and_past() {
    let manager = manager_of(vec![
        account("live", 0),
        account("cold", 0),
        account("past", 0),
    ]);
    mark_all_probed(&manager); // isolate from the never-probed boot gate
    set_5h(&manager, 0, "0.10", 3); // live: low util, reset 3h out → already warm
    // account 1 (cold): no 5h data at all
    set_5h(&manager, 2, "0.10", -1); // past: reset 1h ago → window elapsed

    assert_eq!(
        manager.warm_targets(),
        vec![1, 2],
        "skip the live-window account; warm the cold and past-reset ones"
    );
}

/// A near/over-threshold account is skipped — warming an exhausted account is
/// pointless spend. Uses the weekly window so the gate is isolated from the
/// live-5h check.
#[test]
fn warm_targets_skips_near_threshold_account() {
    let manager = manager_of(vec![account("full", 0), account("idle", 0)]);
    mark_all_probed(&manager); // isolate from the never-probed boot gate
    set_7d(&manager, 0, "0.95"); // over the 0.90 threshold

    assert_eq!(
        manager.warm_targets(),
        vec![1],
        "an at/over-threshold account is not a warm target"
    );
}

/// **A Codex account is never a keep-warm target.** A warm is an Anthropic inference
/// request: `warm_account` POSTs `/v1/messages` to the Anthropic `upstream` with the
/// row's bearer, so warming a Codex row would hand its OpenAI bearer to Anthropic, and
/// on the right host would spend a real Codex turn. The Codex row here is otherwise an
/// ideal target (probed, cold, idle) and the Anthropic row in the same state is warmed,
/// so the exclusion is the provider and nothing else.
///
/// Watched red: drop the provider filter in `warm_targets` and the Codex row is listed
/// and its bearer warmed.
#[tokio::test]
async fn warm_targets_never_lists_a_codex_account() {
    let warmed = Arc::new(Mutex::new(Vec::new()));
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com", 0)
    };
    let manager = build_manager_with_warmer(
        config_with(vec![account("alice@example.com", 0), codex]),
        counting_refresher(),
        Arc::new(RecordingWarmer {
            warmed: Arc::clone(&warmed),
        }),
    );
    mark_all_probed(&manager); // isolate from the never-probed boot gate

    assert_eq!(
        manager.warm_targets(),
        vec![0],
        "only the Anthropic row is a keep-warm target"
    );
    manager.warm_all().await;
    assert_eq!(
        warmed.lock().expect("warmed lock poisoned").clone(),
        vec!["at-alice@example.com".to_string()],
        "no warm request ever carries the Codex row's bearer"
    );
}

/// `warm_all` warms ONLY the idle targets: the idle account's token is warmed,
/// while a live-window account, a disabled one, and an errored one are not.
#[tokio::test]
async fn warm_all_warms_only_idle_targets() {
    let warmed = Arc::new(Mutex::new(Vec::new()));
    let warmer = Arc::new(RecordingWarmer {
        warmed: Arc::clone(&warmed),
    });
    let manager = build_manager_with_warmer(
        config_with(vec![
            account("idle", 0),
            account("live", 0),
            account("off", 0),
            account("dead", 0),
        ]),
        counting_refresher(),
        warmer,
    );
    mark_all_probed(&manager); // isolate from the never-probed boot gate
    set_5h(&manager, 1, "0.10", 3); // live window → already warm
    manager.set_disabled(2, true);
    manager.mark_error(3);

    manager.warm_all().await;

    let warmed = warmed.lock().unwrap().clone();
    assert_eq!(
        warmed,
        vec!["at-idle".to_string()],
        "only the idle account's token is warmed"
    );
}

/// `warm_all` is a pure no-op — the warmer is never invoked — when there are
/// zero eligible targets.
#[tokio::test]
async fn warm_all_is_noop_when_no_targets() {
    let warmed = Arc::new(Mutex::new(Vec::new()));
    let warmer = Arc::new(RecordingWarmer {
        warmed: Arc::clone(&warmed),
    });
    let manager = build_manager_with_warmer(
        config_with(vec![account("off", 0)]),
        counting_refresher(),
        warmer,
    );
    manager.set_disabled(0, true);

    manager.warm_all().await;

    assert!(
        warmed.lock().unwrap().is_empty(),
        "the warmer must never be invoked when there are no targets"
    );
}

/// `warm_account` folds the warm response's rate-limit headers into the
/// account's quota, so its just-started 5h window is immediately visible
/// (populated `five_hour`, a live future reset) — which suppresses a re-warm.
#[tokio::test]
async fn warm_account_folds_response_headers_into_five_hour() {
    let warmer = Arc::new(RecordingWarmer {
        warmed: Arc::new(Mutex::new(Vec::new())),
    });
    let manager = build_manager_with_warmer(
        config_with(vec![account("cold", 0)]),
        counting_refresher(),
        warmer,
    );
    mark_all_probed(&manager); // isolate from the never-probed boot gate
    // Before: no 5h window, so this account is a target.
    assert_eq!(manager.warm_targets(), vec![0]);

    manager.warm_account(0).await;

    let now = OffsetDateTime::now_utc();
    let snap = manager.snapshot(now);
    assert!(
        snap.accounts[0].five_hour.is_some(),
        "the warm response's 5h window was folded into the account"
    );
    assert!(
        snap.accounts[0].five_hour_reset.is_some(),
        "the folded 5h window has a live future reset"
    );
    // And the fold suppresses a re-warm on the next sweep.
    assert!(
        manager.warm_targets().is_empty(),
        "a freshly-warmed account is no longer a target"
    );
}

/// One cold account, probed, that is first a warm target and then gets fifty warms
/// whose 200s carry no rate-limit headers. Fifty is a generous number of cadence-driven
/// warms standing in for "forever": if the account is STILL a target after this many,
/// the loop has no bound at all.
async fn one_cold_account_warmed_fifty_times_without_evidence() -> Arc<Manager> {
    let manager = build_manager_with_warmer(
        config_with(vec![account("cold", 0)]),
        counting_refresher(),
        Arc::new(HeaderlessWarmer),
    );
    mark_all_probed(&manager); // isolate from the never-probed boot gate
    assert_eq!(manager.warm_targets(), vec![0]);
    for _ in 0..50 {
        manager.warm_account(0).await;
    }
    manager
}

/// **THE truncation guard.** `warm_targets`' own doc-comment calls a warm that
/// latches no evidence "self-limiting", on the strength of `warm_account`
/// folding the response's rate-limit headers back into the account's quota. A
/// 200 that carries none of the `anthropic-ratelimit-unified-5h-*` headers
/// folds NOTHING (`Quota::update_from_headers`'s own doc-comment: "a response
/// without the header latches nothing") — so the account's 5h window stays
/// blank, `live_reset` stays `None`, and it stays a `warm_targets` member
/// forever, warmed again every cadence at real upstream cost. This asserts the
/// promise in `warm_targets`' doc-comment actually holds: a warm that never
/// latches evidence must not repeat without limit.
#[tokio::test]
async fn warm_account_without_evidence_headers_does_not_repeat_forever() {
    let manager = one_cold_account_warmed_fifty_times_without_evidence().await;
    assert!(
        !manager.warm_targets().contains(&0),
        "a warm that never latches evidence must not remain an eligible target forever"
    );
}

/// A plan with no 5h window (the usage endpoint's `"five_hour": null`, an Ultra
/// org) has no window for a warm to start, so it is never a warm target, even
/// with its quota read and no live 5h reset. Without the gate it would be warmed
/// every cadence up to the repeat cap, spending weekly quota for nothing.
#[test]
fn a_plan_with_no_five_hour_window_is_not_a_warm_target() {
    let manager = manager_of(vec![account("ultra", 0)]);
    mark_all_probed(&manager);
    assert_eq!(manager.warm_targets(), vec![0], "control: a cold row warms");

    manager.apply_usage(
        0,
        &Usage {
            five_hour: crate::probe::WindowReading::PlanHasNone,
            ..Usage::default()
        },
    );
    assert!(
        manager.warm_targets().is_empty(),
        "an account whose plan has no 5h window has nothing to warm"
    );
}

/// A follow-up regression on the bound above: it must not be defeatable by
/// the background prober. A first cut of the recovery fix reset
/// `consecutive_warms_without_evidence` on every successful `apply_usage`
/// call — but `probeable_indices` (unlike `warm_targets`) does not check
/// that counter, so the prober keeps visiting an excluded account on its
/// OWN schedule regardless of exclusion. That reset would reopen the gate
/// every probe cycle and hand back a full burst of
/// `WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT` header-less warms each time —
/// MORE spend than the original unbounded-warm bug, not less. Drives an
/// account past the limit, then feeds it several successful
/// no-5h-bucket probes (several "probe cycles") and asserts it stays
/// excluded throughout — recovery must come from the cooldown, not from
/// probe traffic.
#[tokio::test]
async fn repeated_probe_cycles_do_not_reopen_a_warm_exclusion() {
    let manager = one_cold_account_warmed_fifty_times_without_evidence().await;
    assert!(
        !manager.warm_targets().contains(&0),
        "setup: the account must be excluded before the probe-cycle check runs"
    );

    let headerless_probe = Usage {
        five_hour: crate::probe::WindowReading::Unread,
        seven_day: None,
        seven_day_oi: None,
        extra_usage_usd: None,
        codex: None,
        claude_resets: None,
    };
    for cycle in 0..5 {
        manager.apply_usage(0, &headerless_probe);
        assert!(
            !manager.warm_targets().contains(&0),
            "probe cycle {cycle}: a successful probe that reads no 5h bucket \
             must not, by itself, reopen a warm exclusion — recovery is the \
             cooldown, not probe traffic"
        );
    }
}

/// The recovery half of the bound above. `consecutive_warms_without_evidence`
/// is a ONE-WAY latch with a single writer (`record_warm_without_evidence`)
/// and only one reset (`update_quota`, on a header-bearing served or warmed
/// response) — but an account excluded from `warm_targets` is never picked
/// to SERVE either, so it can never earn one of those on its own. Left that
/// way, a transient upstream condition that strips the 5h headers for a few
/// minutes would exclude the account from keep-warm for the life of the
/// process, recoverable only by a restart (this repo's most expensive event
/// — a full cold prompt-cache prefix for every live session).
///
/// The fix is `warm_evidence_retry_after_ms`'s flat cooldown, set by
/// `record_warm_without_evidence` once the account crosses the limit. This
/// drives an account past the limit, confirms it is excluded, fast-forwards
/// the cooldown by writing a past deadline directly (the same pattern this
/// module's other `_until_ms`/`_after_ms` cooldown tests use — a real sleep
/// of `WARM_EVIDENCE_RETRY_COOLDOWN_MS` is not a reasonable thing to do in a
/// unit test), and asserts the account is a warm target again.
#[tokio::test]
async fn warm_evidence_retry_cooldown_recovers_an_excluded_account() {
    let manager = one_cold_account_warmed_fifty_times_without_evidence().await;
    assert!(
        !manager.warm_targets().contains(&0),
        "setup: the account must be excluded before recovery is exercised"
    );
    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert!(
            accounts[0].warm_evidence_retry_after_ms.is_some(),
            "setup: crossing the limit must arm a retry cooldown"
        );
    }

    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].warm_evidence_retry_after_ms = Some(crate::now_ms() - 1);
    }

    assert!(
        manager.warm_targets().contains(&0),
        "an elapsed retry cooldown must recover an account the warm latch \
         excluded, without a restart"
    );
}

/// The failing half of the recovery above. `record_warm_without_evidence`
/// only re-arms `warm_evidence_retry_after_ms` from the Ok-but-header-less
/// branch of `warm_account` — a warm that FAILS outright takes the `Err`
/// arm instead, which (before this fix) never touched the deadline. Once
/// the cooldown had elapsed, a FAILING retry left the now-past deadline in
/// place, and `warm_targets`' `cooldown_elapsed` check re-admitted the
/// account on every subsequent pass rather than once per cooldown — the
/// "one bounded retry burst per hour" property collapsing back to "every
/// tick" on the failure path specifically, which is the unbounded-repeat
/// bug this whole latch exists to bound. Drives an account past the limit
/// and elapses its cooldown directly (this test is about the `Err` arm's
/// re-arm, not about repeating `warm_account` 50 times to get there), then
/// feeds it ONE failing warm (`NoWarmer` always returns `Err`) and asserts
/// the deadline moved back into the future — and the account is excluded
/// again immediately, not just eventually.
#[tokio::test]
async fn a_failing_retry_does_not_reset_the_warm_evidence_cooldown() {
    let manager = build_manager_with_warmer(
        config_with(vec![account("cold", 0)]),
        counting_refresher(),
        Arc::new(NoWarmer),
    );
    mark_all_probed(&manager); // isolate from the never-probed boot gate

    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].consecutive_warms_without_evidence = WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT;
        accounts[0].warm_evidence_retry_after_ms = Some(crate::now_ms() - 1);
    }
    assert!(
        manager.warm_targets().contains(&0),
        "setup: an elapsed cooldown must make the account a target again"
    );

    manager.warm_account(0).await; // NoWarmer always returns Err

    let deadline = {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        accounts[0].warm_evidence_retry_after_ms
    };
    assert!(
        deadline.is_some_and(|until| until > crate::now_ms()),
        "a FAILING retry must re-arm the cooldown into the future too, not \
         just an Ok-but-header-less one — otherwise the account is \
         re-admitted on every subsequent tick instead of once per cooldown"
    );
    assert!(
        !manager.warm_targets().contains(&0),
        "immediately after a failing retry the account must be excluded \
         again, not left eligible on a stale past deadline"
    );
}

/// THE boot gate. At startup every account's quota is blank (`from_config`
/// seeds `Quota::default()` and nothing restores it), so before the gate every
/// probeable account looked cold and the warm loop's immediate first tick
/// would have fired a real quota-spending request at all of them — including
/// accounts whose 5h window is genuinely live. A never-probed account's blank
/// quota is unknown, not known-cold, so with probing on there are no targets.
#[test]
fn warm_targets_is_empty_at_boot_while_probing_is_enabled() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    // Boot state, untouched: probing on by default (75s), nothing probed yet.
    assert!(manager.probe_interval_seconds() > 0);

    assert!(
        manager.warm_targets().is_empty(),
        "an unread account's blank quota is unknown, not known-cold"
    );
}

/// **THE spec-error guard.** The boot gate's predicate must be "a probe actually
/// READ this account's quota", never "a probe reported *something*":
/// `probe_account` calls `apply_usage` only on `Ok`, while `record_probe` stamps
/// `Error`/`Timeout`/`RateLimited` on failure — so after a failed first sweep
/// `probe_status != Never` with the quota still blank, and a `probe_status`-keyed
/// gate lifts on blank quota and hands the boot burst straight back. A FAILED
/// probe must leave the account a NON-target.
#[tokio::test]
async fn warm_targets_stays_empty_after_a_failed_probe() {
    // `ScriptedProber` errors (500) on every token but `ok_token`; no account
    // here carries that token, so the whole sweep fails — the fleet-wide false
    // error `probing.rs` documents.
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0), account("b", 0)]),
        counting_refresher(),
        Arc::new(ScriptedProber {
            ok_token: "at-nobody".to_string(),
        }),
    );
    assert!(manager.probe_interval_seconds() > 0);

    manager.probe_all().await;

    {
        let accounts = manager.accounts.read().unwrap();
        // The probe SPOKE — this is what makes `probe_status` the wrong gate.
        assert!(
            accounts
                .iter()
                .all(|a| a.probe_status != ProbeStatus::Never),
            "the failed sweep must still stamp a terminal probe status on every row"
        );
        // …but it read nothing.
        assert!(
            accounts.iter().all(|a| !a.quota_known),
            "a FAILED probe must never latch quota_known"
        );
    }
    assert!(
        manager.warm_targets().is_empty(),
        "a failed probe leaves the quota unread, so nothing may be a warm target"
    );
}

/// FIX 2's guard, and the other half of FIX 1. A SUCCESSFUL probe latches the
/// gate open and must WAKE the warm loop: its ticker fired its only immediate
/// tick before any quota existed and `MissedTickBehavior::Skip` puts the next one
/// a full `warmupSeconds` away, so without the wake a proxy restarted more often
/// than its warm interval warms nothing, ever.
///
/// The prober reports a weekly bucket and NO 5h bucket on purpose: it proves the
/// latch is about the read, not about `five_hour.is_some()` — the account is a
/// target with `five_hour` still `None`.
#[tokio::test]
async fn a_successful_probe_opens_the_gate_and_wakes_the_warm_loop() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(ColdOkProber),
    );
    let wake_window = std::time::Duration::from_millis(50);
    assert!(manager.warm_targets().is_empty(), "boot: nothing read yet");
    assert!(
        tokio::time::timeout(wake_window, manager.warm_wake().notified())
            .await
            .is_err(),
        "nothing may wake the warm loop before any quota has been read"
    );

    manager.probe_all().await;

    assert!(
        manager.accounts.read().unwrap()[0]
            .quota
            .five_hour
            .is_none(),
        "the fixture must leave the 5h window absent, or it proves nothing about the predicate"
    );
    assert_eq!(
        manager.warm_targets(),
        vec![0],
        "a read quota opens the gate even with no 5h bucket in the response"
    );
    tokio::time::timeout(wake_window, manager.warm_wake().notified())
        .await
        .expect(
            "the first successful probe must wake the warm loop, not leave it a full interval away",
        );
}

/// The wake is edge-triggered on the false→true flip, so a steady-state probe
/// cadence cannot spin the warm loop: the SECOND successful sweep over the same
/// accounts signals nothing.
#[tokio::test]
async fn a_repeat_probe_does_not_re_wake_the_warm_loop() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(ColdOkProber),
    );
    let wake_window = std::time::Duration::from_millis(50);

    manager.probe_all().await;
    // Consume the one wake the first read is entitled to.
    tokio::time::timeout(wake_window, manager.warm_wake().notified())
        .await
        .expect("first read wakes the loop");

    manager.probe_all().await;

    assert!(
        tokio::time::timeout(wake_window, manager.warm_wake().notified())
            .await
            .is_err(),
        "quota_known is already latched, so a repeat probe must not re-wake the loop"
    );
}

/// **State 1 — evidence from the OTHER source.** A served response's
/// rate-limit headers are a first-hand read of that account's 5h window, so
/// they open the gate exactly as a probe does. Without this an account carrying
/// live traffic read as "quota unknown", which is false on its face: we have
/// its window, we just did not get it from the probe.
///
/// The header path must also WAKE the loop, for the same reason the probe path
/// does — an eligible account may not sit out a whole `warmupSeconds`.
#[tokio::test]
async fn a_served_response_header_is_quota_evidence_and_opens_the_gate() {
    let manager = manager_of(vec![account("a", 0)]);
    let wake_window = std::time::Duration::from_millis(50);
    assert!(manager.probe_interval_seconds() > 0);
    assert!(manager.warm_targets().is_empty(), "boot: nothing read yet");

    // A response carrying the unified 5h window, with a reset already past —
    // so the window it reports is EXPIRED and the account is genuinely cold.
    set_5h(&manager, 0, "0.10", -1);

    assert!(
        manager.accounts.read().unwrap()[0].quota_known,
        "a response that reported the 5h window is evidence about that window"
    );
    assert_eq!(
        manager.warm_targets(),
        vec![0],
        "with the window read and not live, the account is a target"
    );
    tokio::time::timeout(wake_window, manager.warm_wake().notified())
        .await
        .expect("the first read wakes the warm loop whichever source it came from");
}

/// …but only a response that actually SAID something about the 5h window. A
/// response without the header — an error page, a non-Anthropic upstream — is
/// silence, and silence is not evidence. `set_7d` reports the weekly window and
/// no 5h window, which is exactly that shape.
#[test]
fn a_response_without_the_five_hour_header_latches_nothing() {
    let manager = manager_of(vec![account("a", 0)]);

    set_7d(&manager, 0, "0.10");

    assert!(
        !manager.accounts.read().unwrap()[0].quota_known,
        "a weekly-only response says nothing about the 5h window"
    );
    assert!(
        manager.warm_targets().is_empty(),
        "so the gate must still be waiting"
    );
}

/// **State 3 — the bounded wait.** `quota_known` can never latch while the
/// probe fails, so gating on it *unconditionally* makes keep-warm structurally
/// dark: silently doing nothing while config and TUI both read as enabled.
/// After `PROBE_FAILURES_BEFORE_WARMING_UNPROBED` consecutive failed sweeps the
/// gate stops waiting — absence of evidence is not evidence of a live window —
/// and wakes the loop it kept parked.
///
/// One failure must NOT be enough: the fleet-wide false error `probing.rs`
/// documents is a one-sweep event, and lifting on it hands the boot burst back.
#[tokio::test]
async fn a_persistently_failing_probe_stops_blocking_keep_warm() {
    // No account carries `ok_token`, so every sweep fails on every row.
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(ScriptedProber {
            ok_token: "at-nobody".to_string(),
        }),
    );
    let wake_window = std::time::Duration::from_millis(50);
    assert!(manager.probe_interval_seconds() > 0);

    for sweep in 1..PROBE_FAILURES_BEFORE_WARMING_UNPROBED {
        manager.probe_all().await;
        assert!(
            manager.warm_targets().is_empty(),
            "sweep {sweep} of {PROBE_FAILURES_BEFORE_WARMING_UNPROBED}: one or two failures are a hiccup, not a verdict"
        );
        assert!(
            tokio::time::timeout(wake_window, manager.warm_wake().notified())
                .await
                .is_err(),
            "sweep {sweep}: nothing may wake the loop while the gate is still waiting"
        );
    }

    manager.probe_all().await;

    {
        let accounts = manager.accounts.read().unwrap();
        assert!(
            !accounts[0].quota_known,
            "the escape valve must NOT fake evidence — nothing was ever read"
        );
        assert_eq!(
            accounts[0].consecutive_probe_failures,
            PROBE_FAILURES_BEFORE_WARMING_UNPROBED
        );
    }
    assert_eq!(
        manager.warm_targets(),
        vec![0],
        "a probe that has failed every sweep is not going to answer; waiting forever is a kill switch"
    );
    tokio::time::timeout(wake_window, manager.warm_wake().notified())
        .await
        .expect("giving up on the wait must wake the loop, not leave it a full interval away");
}

/// The escape valve is armed by a RUN of failures, not a total: one success in
/// between resets it. Otherwise an account that fails, recovers, and fails
/// again drifts into "stop waiting" on evidence it actually has.
#[tokio::test]
async fn a_successful_probe_resets_the_failure_run() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(ScriptedProber {
            ok_token: "at-nobody".to_string(),
        }),
    );
    manager.probe_all().await;
    assert_eq!(
        manager.accounts.read().unwrap()[0].consecutive_probe_failures,
        1
    );

    manager.record_probe(0, ProbeStatus::Ok, None, None);

    assert_eq!(
        manager.accounts.read().unwrap()[0].consecutive_probe_failures,
        0,
        "a successful read ends the run"
    );
}

/// Once the probe has actually spoken, a genuinely cold or expired window IS
/// warmed again — the gate defers the first sweep, it does not disable
/// keep-warm. A probed account with a LIVE window stays skipped.
#[test]
fn warm_targets_resumes_once_the_probe_has_reported() {
    let manager = manager_of(vec![
        account("cold", 0),
        account("live", 0),
        account("dark", 0),
    ]);
    // Only the first two have had their quota read; the third never has.
    {
        let mut accounts = manager.accounts.write().unwrap();
        accounts[0].probe_status = ProbeStatus::Ok;
        accounts[0].quota_known = true;
        accounts[1].probe_status = ProbeStatus::Ok;
        accounts[1].quota_known = true;
    }
    set_5h(&manager, 0, "0.10", -1); // probed, window expired → genuinely cold
    set_5h(&manager, 1, "0.10", 3); // probed, window live → already warm

    assert_eq!(
        manager.warm_targets(),
        vec![0],
        "a probed account with an expired window is a target; a live one and an unprobed one are not"
    );
}

/// The dark-feature guard. With `quotaProbeSeconds: 0` no probe task is ever
/// spawned, so `quota_known` stays `false` forever. Gating on it
/// unconditionally would make keep-warm structurally unable to fire while
/// still reading as enabled — so with probing off, the gate does not apply.
#[test]
fn warm_targets_ignores_the_boot_gate_when_probing_is_disabled() {
    let manager = build_manager(
        config_with_probe_seconds(vec![account("a", 0), account("b", 0)], 0),
        counting_refresher(),
    );
    assert_eq!(manager.probe_interval_seconds(), 0);
    // Nothing has been (or ever will be) probed, so no quota is known.
    assert!(
        manager
            .accounts
            .read()
            .unwrap()
            .iter()
            .all(|a| !a.quota_known)
    );

    assert_eq!(
        manager.warm_targets(),
        vec![0, 1],
        "with the probe off there is nothing to wait for; keep-warm must not go dark"
    );
}
