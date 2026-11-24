use super::*;
use crate::config::{Account, PacingConfig};
use crate::oauth::{OAuthError, RefreshFuture};
use crate::probe::{ProbeError, ProbeFuture, Usage, UsageBucket};
use crate::warmer::{AccountWarmer, WarmError, WarmFuture};
use std::sync::atomic::{AtomicUsize, Ordering};
use time::Duration;

/// A rate-limit hold that clears while the pinned account's prompt cache is
/// still warm — the SOFT case. The live value of a no-guidance transient park
/// (`proxy::NO_GUIDANCE_HOLD_SECS`), which is the hold this fleet arms most.
const SHORT_HOLD_SECS: i64 = 15;

/// A hold that OUTLIVES the prompt cache — the only kind that is ACCOUNT-level
/// death and may re-key a session. Derived from the threshold rather than
/// written as a literal so that raising [`CACHE_WARM_HOLD_SECS`] can never
/// silently turn a re-key test into a divert test.
const LONG_HOLD_SECS: i64 = CACHE_WARM_HOLD_SECS + 60;

#[test]
fn enter_in_flight_stamps_the_wall_clock() {
    let manager = Manager::new(
        Config::fixture(vec![Account::fixture("alpha")]),
        Arc::new(oauth::NoRefresh),
        Arc::new(LiveUsageProber::new()),
        Arc::new(LiveWarmer::new()),
        None,
    );
    let before = crate::now_ms();
    manager.accounts.write().unwrap()[0].last_served_ms = before + 86_400_000;
    drop(manager.enter_in_flight(0));
    let after = crate::now_ms();
    let stamp = manager.accounts.read().unwrap()[0].last_served_ms;
    assert!(
        (before..=after).contains(&stamp),
        "enter_in_flight must stamp the wall clock directly: {before} <= {stamp} <= {after}"
    );
}

/// `build_serving_client` must not panic on either branch of `http1_only` —
/// h1 support is part of the reqwest/rustls TLS backend this crate already
/// pulls in, but a builder method rejecting a combination it doesn't support
/// is exactly the kind of thing that only shows up at `.build()`.
///
/// This cannot assert the built client actually NEGOTIATES h1 on the wire:
/// reqwest exposes no introspection on a built `Client` for its ALPN/HTTP
/// version policy, and asserting anything less than that would only prove
/// the bool reached this function's own argument — not that it reaches
/// reqwest's TLS layer. See the PR report for the explicit statement of
/// that gap.
#[test]
fn build_serving_client_builds_under_both_http1_only_settings() {
    let _off = build_serving_client(false);
    let _on = build_serving_client(true);
}

#[test]
fn throttle_slot_burst1_is_strict_spacing() {
    // B=1 (tau=0): threading the TAT across 3 calls at a fixed `now` yields
    // allow_at points spaced by exactly `spacing_ms`.
    let now = 1000;
    let spacing = 100;
    let (tat1, allow1) = throttle_slot(0, now, spacing, 1);
    assert_eq!(allow1, now); // first send: instant (allow_at == now)
    let (tat2, allow2) = throttle_slot(tat1, now, spacing, 1);
    assert_eq!(allow2, allow1 + spacing);
    let (_tat3, allow3) = throttle_slot(tat2, now, spacing, 1);
    assert_eq!(allow3, allow2 + spacing);
}

#[test]
fn throttle_slot_burst3_admits_then_paces() {
    // B=3, now=1000, T=100 (tau=200): first 3 fire instantly (allow_at <= now),
    // the 4th is paced to now + spacing_ms.
    let now = 1000;
    let spacing = 100;
    let burst = 3;
    let (tat1, allow1) = throttle_slot(0, now, spacing, burst);
    assert_eq!((tat1, allow1), (1100, 800));
    assert!(allow1 <= now);
    let (tat2, allow2) = throttle_slot(tat1, now, spacing, burst);
    assert_eq!((tat2, allow2), (1200, 900));
    assert!(allow2 <= now);
    let (tat3, allow3) = throttle_slot(tat2, now, spacing, burst);
    assert_eq!((tat3, allow3), (1300, 1000));
    assert!(allow3 <= now);
    let (tat4, allow4) = throttle_slot(tat3, now, spacing, burst);
    assert_eq!((tat4, allow4), (1400, 1100));
    assert_eq!(allow4, now + spacing); // 4th is paced
}

fn account(name: &str, priority: i64) -> Account {
    Account {
        priority: Some(priority),
        ..Account::fixture(name)
    }
}

/// Like [`account`], carrying group labels — for the `--group` prefer-routing
/// tests.
fn account_in_groups(name: &str, priority: i64, groups: &[&str]) -> Account {
    Account {
        groups: Some(groups.iter().map(ToString::to_string).collect()),
        ..account(name, priority)
    }
}

fn config_with(accounts: Vec<Account>) -> Config {
    Config::fixture(accounts)
}

/// Like [`config_with`] but with request pacing configured (for the pacing tests).
fn config_with_pacing(accounts: Vec<Account>, pacing: PacingConfig) -> Config {
    let mut config = config_with(accounts);
    config.pacing = pacing;
    config
}

/// A refresher that counts how many times it is invoked and mints a token
/// valid far into the future (so a coalesced follower sees it as fresh).
struct CountingRefresher {
    calls: Arc<AtomicUsize>,
}

/// A [`CountingRefresher`] and the count it keeps.
fn refresh_counter() -> (Arc<CountingRefresher>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let refresher = Arc::new(CountingRefresher {
        calls: Arc::clone(&calls),
    });
    (refresher, calls)
}

/// A [`CountingRefresher`] whose count no test reads.
fn counting_refresher() -> Arc<CountingRefresher> {
    Arc::new(CountingRefresher {
        calls: Arc::new(AtomicUsize::new(0)),
    })
}

/// A manager over `accounts` with the defaults [`build_manager`] gives:
/// a [`counting_refresher`], no prober, no warmer, no config file.
fn manager_of(accounts: Vec<Account>) -> Arc<Manager> {
    build_manager(config_with(accounts), counting_refresher())
}

impl TokenRefresher for CountingRefresher {
    fn refresh(
        &self,
        _provider: crate::provider::Provider,
        _refresh_token: String,
    ) -> RefreshFuture {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            // Simulate a little upstream latency so concurrent callers pile
            // up on the coalescing lock.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok::<Tokens, OAuthError>(Tokens {
                id_token: None,
                access_token: "fresh-access".to_string(),
                refresh_token: Some("fresh-refresh".to_string()),
                expires_at_ms: crate::now_ms() + 3_600_000,
            })
        })
    }
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

struct NoProber;
impl UsageProber for NoProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        Box::pin(async {
            Err(ProbeError {
                status: None,
                message: "no prober configured".into(),
                retry_after_secs: None,
            })
        })
    }
}

/// A prober returning a canned success for one account and an error for
/// another, keyed on the access token so probe-health can be asserted.
struct ScriptedProber {
    ok_token: String,
}
impl UsageProber for ScriptedProber {
    fn probe(&self, credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let ok = credential.access_token == self.ok_token;
        Box::pin(async move {
            if ok {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Read(UsageBucket {
                        utilization: Some(0.25),
                        reset_at_ms: Some(crate::now_ms() + 3_600_000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            } else {
                Err(ProbeError {
                    status: Some(500),
                    message: "upstream boom".into(),
                    retry_after_secs: None,
                })
            }
        })
    }
}

/// A prober that succeeds on its first call (learning a 5-hour bar) and then
/// returns a 429 on every call after — for asserting a probe-429 keeps the
/// last-good utilization and reads as `RateLimited`, not `Error`.
struct FirstOkThen429Prober {
    calls: Arc<AtomicUsize>,
}

struct RetryAfterProber {
    calls: Arc<AtomicUsize>,
}

impl UsageProber for RetryAfterProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ProbeError {
                status: Some(429),
                message: "Too Many Requests".into(),
                retry_after_secs: Some(3600),
            })
        })
    }
}
impl UsageProber for FirstOkThen429Prober {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if n == 0 {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Read(UsageBucket {
                        utilization: Some(0.25),
                        reset_at_ms: Some(crate::now_ms() + 3_600_000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            } else {
                Err(ProbeError {
                    status: Some(429),
                    message: "Too Many Requests".into(),
                    retry_after_secs: None,
                })
            }
        })
    }
}

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

/// A warmer that never hits the network — the default for tests that do not
/// exercise keep-warm. If invoked it records nothing and returns empty headers.
struct NoWarmer;
impl AccountWarmer for NoWarmer {
    fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
        Box::pin(async {
            Err(WarmError {
                status: None,
                message: "no warmer configured".into(),
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

fn build_manager(config: Config, refresher: Arc<dyn TokenRefresher>) -> Arc<Manager> {
    Manager::new(
        config,
        refresher,
        Arc::new(NoProber),
        Arc::new(NoWarmer),
        None,
    )
}

fn build_manager_with_prober(
    config: Config,
    refresher: Arc<dyn TokenRefresher>,
    prober: Arc<dyn UsageProber>,
) -> Arc<Manager> {
    Manager::new(config, refresher, prober, Arc::new(NoWarmer), None)
}

fn build_manager_with_warmer(
    config: Config,
    refresher: Arc<dyn TokenRefresher>,
    warmer: Arc<dyn AccountWarmer>,
) -> Arc<Manager> {
    Manager::new(config, refresher, Arc::new(NoProber), warmer, None)
}

/// A manager that DOES have a config file behind it — the live server's shape.
/// Every other builder here passes `config_path = None`, which makes each
/// persist a silent no-op.
fn build_manager_with_path(config: Config, path: PathBuf) -> Arc<Manager> {
    Manager::new(
        config,
        counting_refresher(),
        Arc::new(NoProber),
        Arc::new(NoWarmer),
        Some(path),
    )
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

/// The other half, and the one that needs no request at all to fire:
/// `ServerHandle::shutdown_within` ends by calling `persist_now`, so an
/// ungated predecessor writes its stale in-memory tokens over the
/// successor's fresh ones as its very last act.
#[tokio::test]
async fn a_released_manager_does_not_write_the_config() {
    let path = tmp_config_path("released-no-persist");
    let manager = build_manager_with_path(config_with(vec![account("a", 0)]), path.clone());

    // A persist while ownership is held establishes the file and proves the
    // write path is live -- without this, an unchanged file below could just
    // mean nothing ever writes here.
    manager.persist_now();
    let owned = std::fs::read(&path).expect("the owned persist must have written the file");
    assert!(
        !owned.is_empty(),
        "precondition: the owned persist wrote bytes"
    );

    // Stand in for the successor having rewritten the file after the handoff.
    std::fs::write(&path, b"{\"successor\":true}").expect("writing the successor's file");

    manager.release_mutation_ownership();
    manager.persist_now();

    assert_eq!(
        std::fs::read(&path).expect("reading the file back"),
        b"{\"successor\":true}",
        "a released manager overwrote the successor's config file"
    );
}

/// `persist_tokens` must write ONLY the account it rotated.
///
/// `config::CredentialScope` makes that expressible; this is the test that
/// it is USED. Every config-level test passes with `persist_tokens` handing
/// over `CredentialScope::All`, so without this the fix could be reverted
/// and nothing would go red.
///
/// The story: the manager holds a boot-time snapshot. Another `tcr` process
/// re-auths `acct-b`, putting a fresh single-use refresh token on disk. The
/// manager then rotates `acct-a` and persists. Its snapshot still carries
/// b's OLD credential, and writing it would leave b dead until re-authed by
/// hand. Strictly ordered, no concurrency, which is why a file lock is not
/// the fix here.
#[test]
fn a_rotation_persists_only_the_rotated_account() {
    let path = tmp_config_path("scoped-persist");
    let config = config_with(vec![account("a", 0), account("b", 1)]);
    let manager = build_manager_with_path(config, path.clone());

    // Establish the file from the manager's own snapshot, so an unchanged
    // value below cannot just mean nothing ever writes here.
    manager.persist_now();
    let before = std::fs::read_to_string(&path).expect("the persist must write the file");
    assert!(
        before.contains("rt-b"),
        "precondition: acct-b's original credential is on disk: {before}"
    );

    // Another process re-auths acct-b. Completes before the rotation below.
    let mut relogin = config::load(&path).expect("the file we just wrote loads");
    relogin.accounts[1].refresh_token = Some("rt-b-relogin".to_string());
    config::save_tokens(&path, &relogin).expect("the re-auth write");

    // The manager rotates acct-a only.
    manager.persist_tokens(
        "a",
        None,
        None,
        None,
        crate::provider::Provider::Anthropic,
        &Tokens {
            id_token: None,
            access_token: "at-a-rotated".to_string(),
            refresh_token: Some("rt-a-rotated".to_string()),
            expires_at_ms: crate::now_ms() + 3_600_000,
        },
    );

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
            .expect("the config stays valid JSON");
    assert_eq!(
        value["accounts"][0]["refreshToken"],
        serde_json::json!("rt-a-rotated"),
        "the rotation this persist exists for must land"
    );
    assert_eq!(
        value["accounts"][1]["refreshToken"],
        serde_json::json!("rt-b-relogin"),
        "rotating acct-a reverted acct-b's re-auth; that token is single-use \
         and acct-b is now dead until `tcr login`"
    );

    std::fs::remove_file(&path).ok();
}

/// A rotated Codex credential is persisted onto the Codex row, and the Anthropic
/// row that shares its name keeps its own.
///
/// Neither row carries an account UUID, so identity alone falls back to the name
/// and calls the two rows one account; only the provider tells them apart. Both
/// halves of the write depend on it: provider-blind, `persist_tokens`' own
/// placement ties and refuses, and so does the merge that pairs on-disk entries
/// with loaded accounts — either way the rotated single-use refresh token never
/// reaches the file, and the Codex account is dead after the next restart.
#[test]
fn a_rotated_codex_token_persists_onto_the_codex_row_beside_a_same_named_anthropic_row() {
    let path = tmp_config_path("codex-beside-anthropic");
    let mut anthropic = account("alice@example.com", 0);
    anthropic.access_token = "at-anthropic".to_string();
    anthropic.refresh_token = Some("rt-anthropic".to_string());
    let mut codex = account("alice@example.com", 1);
    codex.provider = crate::provider::Provider::Codex;
    codex.access_token = "at-codex".to_string();
    codex.refresh_token = Some("rt-codex".to_string());
    let manager = build_manager_with_path(config_with(vec![anthropic, codex]), path.clone());

    // Establish the file from the manager's own snapshot, so an unchanged row
    // below cannot just mean nothing ever writes here.
    manager.persist_now();

    manager.persist_tokens(
        "alice@example.com",
        None,
        None,
        None,
        crate::provider::Provider::Codex,
        &Tokens {
            id_token: None,
            access_token: "at-codex-rotated".to_string(),
            refresh_token: Some("rt-codex-rotated".to_string()),
            expires_at_ms: crate::now_ms() + 3_600_000,
        },
    );

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
            .expect("the config stays valid JSON");
    let row_of = |provider: Option<&str>| {
        value["accounts"]
            .as_array()
            .expect("an accounts array")
            .iter()
            .find(|row| row.get("provider").and_then(serde_json::Value::as_str) == provider)
            .unwrap_or_else(|| panic!("no row with provider {provider:?}: {value}"))
            .clone()
    };
    let anthropic_row = row_of(None);
    assert_eq!(
        anthropic_row["refreshToken"],
        serde_json::json!("rt-anthropic"),
        "a Codex rotation must leave the Anthropic row's own credential alone: {value}"
    );
    assert_eq!(
        anthropic_row["accessToken"],
        serde_json::json!("at-anthropic")
    );
    let codex_row = row_of(Some("codex"));
    assert_eq!(
        codex_row["refreshToken"],
        serde_json::json!("rt-codex-rotated"),
        "the rotated Codex token must reach the Codex row: {value}"
    );
    assert_eq!(
        codex_row["accessToken"],
        serde_json::json!("at-codex-rotated")
    );

    std::fs::remove_file(&path).ok();
}

/// Released is terminal. Nothing clears it, and releasing twice is safe --
/// both `shutdown` and `Drop` can reach the same path.
#[test]
fn releasing_mutation_ownership_is_terminal_and_idempotent() {
    let manager = manager_of(vec![account("a", 0)]);
    assert!(!manager.mutation_is_released(), "starts owning mutation");
    manager.release_mutation_ownership();
    assert!(manager.mutation_is_released());
    manager.release_mutation_ownership();
    assert!(
        manager.mutation_is_released(),
        "a second release must not toggle it back"
    );
}

/// A plan prober that answers one canned reading and counts its calls — the
/// counter is the point, not decoration: the backfill's whole contract is
/// that it runs ONCE per account and never again.
struct CountingPlanProber {
    plan: crate::probe::Plan,
    calls: Arc<AtomicUsize>,
}

impl PlanProber for CountingPlanProber {
    fn plan(&self, _access_token: String) -> crate::probe::PlanFuture {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let plan = self.plan.clone();
        Box::pin(async move { plan })
    }
}

/// Give `manager` a [`CountingPlanProber`] that answers `plan`, and return its call count.
fn count_plan_fetches(manager: &Manager, plan: crate::probe::Plan) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    manager.set_plan_prober(Arc::new(CountingPlanProber {
        plan,
        calls: Arc::clone(&calls),
    }));
    calls
}

/// One Anthropic OAuth row, `alice@example.com`, that has no plan yet.
const ALICE_WITHOUT_A_PLAN: &str = r#"{ "accounts": [
    { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
      "refreshToken": "rt-a", "priority": 0 }
] }"#;

/// One Codex row, `bob@example.com/codex`, on the `plus` plan.
const BOB_CODEX_ON_PLUS: &str = r#"{ "accounts": [
    { "name": "bob@example.com/codex", "type": "oauth", "provider": "codex",
      "credentialVersion": 2,
      "accountUuid": "22222222-2222-4222-8222-222222222222",
      "organizationType": "plus", "groups": ["orchard"], "priority": 0,
      "accessToken": "at-bob", "refreshToken": "rt-bob",
      "expiresAt": 4102444800000 }
] }"#;

/// `json` written to a fresh temp config path tagged `tag`.
fn config_file(tag: &str, json: &str) -> PathBuf {
    let path = tmp_config_path(tag);
    std::fs::write(&path, json).unwrap();
    path
}

/// A manager over the config file at `path` that probes usage with `prober`.
fn manager_on_file(path: &std::path::Path, prober: Arc<dyn UsageProber>) -> Arc<Manager> {
    Manager::new(
        config::load(path).unwrap(),
        counting_refresher(),
        prober,
        Arc::new(NoWarmer),
        Some(path.to_path_buf()),
    )
}

/// The reason this feature has a backfill at all: fifteen accounts logged in
/// before the plan fields existed, and re-authenticating each in a browser is
/// not an answer. One probe learns the plan, writes it to the runtime row AND
/// to disk, and never asks again.
#[tokio::test]
async fn probing_backfills_a_missing_plan_once_and_persists_it() {
    let path = config_file("plan-backfill", ALICE_WITHOUT_A_PLAN);
    let manager = manager_on_file(&path, Arc::new(NoProber));

    let calls = count_plan_fetches(
        &manager,
        crate::probe::Plan {
            organization_type: Some("claude_team".to_string()),
            rate_limit_tier: Some("default_raven".to_string()),
            seat_tier: Some("team_standard".to_string()),
            plan_display_name: None,
        },
    );

    manager.probe_account(0).await;

    {
        let accounts = manager.accounts.read().expect("lock");
        assert_eq!(
            accounts[0].organization_type.as_deref(),
            Some("claude_team")
        );
        assert_eq!(accounts[0].seat_tier.as_deref(), Some("team_standard"));
    }
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        on_disk["accounts"][0]["organizationType"], "claude_team",
        "a plan learned only in memory is re-fetched on every restart forever"
    );
    assert_eq!(on_disk["accounts"][0]["seatTier"], "team_standard");
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // The steady state: an account that HAS a plan is never asked again, so
    // this costs zero extra HTTP calls per cadence for the fleet's whole life.
    manager.probe_account(0).await;
    manager.probe_account(0).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "the backfill must run once per account, not once per probe"
    );
    std::fs::remove_file(&path).ok();
}

/// A row profiled before `plan_display_name` existed (2026-11-01) has a plan and no
/// display name. It is asked once more in this process, learns "Ultra" on the
/// runtime row and on disk, and is not asked again.
#[tokio::test]
async fn probing_asks_a_profiled_row_once_for_its_display_name() {
    let path = config_file(
        "plan-display-name",
        r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
              "refreshToken": "rt-a", "priority": 0,
              "organizationType": "claude_max", "rateLimitTier": "default_claude_max_20x" }
        ] }"#,
    );
    let manager = manager_on_file(&path, Arc::new(NoProber));
    let calls = count_plan_fetches(
        &manager,
        crate::probe::Plan {
            organization_type: Some("claude_max".to_string()),
            rate_limit_tier: Some("default_claude_max_20x".to_string()),
            seat_tier: None,
            plan_display_name: Some("Ultra".to_string()),
        },
    );

    manager.probe_account(0).await;
    manager.probe_account(0).await;

    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "asked once, not per probe"
    );
    assert_eq!(
        manager.accounts.read().expect("lock")[0]
            .plan_display_name
            .as_deref(),
        Some("Ultra")
    );
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(on_disk["accounts"][0]["planDisplayName"], "Ultra");
    std::fs::remove_file(&path).ok();
}

/// A Codex usage prober whose every answer names one ChatGPT plan and nothing else.
struct CodexPlanProber {
    plan: &'static str,
}

impl UsageProber for CodexPlanProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let plan = self.plan;
        Box::pin(async move {
            Ok(Usage {
                codex: Some(crate::codex_probe::CodexProbeFacts {
                    plan_type: Some(plan.into()),
                    ..Default::default()
                }),
                ..Usage::default()
            })
        })
    }
}

/// **A Codex account's plan follows its usage probe**, and the change is written to that
/// row's `organizationType` and to no other key. A refresh carries no new plan, so before
/// this a plan changed at ChatGPT (Plus to Pro here) stayed what the login said at import.
/// The write is the surgical `save_plan`, and an unchanged plan writes nothing at all.
///
/// Watched red, one break at a time: pass `Some("")` as the rate-limit tier to `save_plan`
/// and the document gains a `rateLimitTier`; drop the call in `probe_account` and the
/// runtime plan stays `plus`.
#[tokio::test]
async fn a_plus_to_pro_probe_updates_the_plan_and_saves_only_that_key() {
    let path = config_file("codex-plan", BOB_CODEX_ON_PLUS);
    let original = read_config_json(&path);
    let manager = manager_on_file(&path, Arc::new(CodexPlanProber { plan: "pro" }));
    // The Anthropic profile endpoint is never this plan's source: a Codex row skips the
    // backfill, and the counting fake keeps the live endpoint out should that ever break.
    let profile_calls = count_plan_fetches(&manager, crate::probe::Plan::default());

    manager.probe_account(0).await;

    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0]
            .organization_type
            .as_deref(),
        Some("pro"),
        "the runtime row follows the probe's plan"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
            .plan_label()
            .as_deref(),
        Some("ChatGPT Pro")
    );
    let mut expected = original.clone();
    expected["accounts"][0]["organizationType"] = serde_json::json!("pro");
    assert_eq!(
        read_config_json(&path),
        expected,
        "the plan is saved, and no other key moves"
    );
    assert_eq!(profile_calls.load(Ordering::Relaxed), 0);

    let saved = std::fs::read(&path).unwrap();
    manager.probe_account(0).await;
    assert_eq!(
        std::fs::read(&path).unwrap(),
        saved,
        "an unchanged plan writes nothing"
    );
    std::fs::remove_file(&path).ok();
}

/// Run `probe_account(0)` on its own thread while this one holds `config_write`, and answer
/// the config file's bytes as they were while the lock was held, then as they are once it
/// was released and the probe finished. The bytes are read before the release, and asserted
/// on only after the join, so a writer that ignores the lock fails the test, not hangs it.
fn config_while_and_after_the_write_lock(
    manager: &Manager,
    path: &std::path::Path,
) -> (String, String) {
    let writing = manager
        .config_write
        .lock()
        .expect("config write lock poisoned");
    let held = std::thread::scope(|scope| {
        let probe = scope.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the probe")
                .block_on(manager.probe_account(0));
        });
        // Long enough for the probe to reach its write, which must wait.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let held = std::fs::read_to_string(path).unwrap();
        drop(writing);
        probe.join().expect("the probe finished");
        held
    });
    (held, std::fs::read_to_string(path).unwrap())
}

/// Nothing reaches the config file at `path` while another writer holds `config_write`,
/// and once it is released `probe_account(0)` has written `plan` to the row.
fn assert_the_plan_write_waits_for_the_write_lock(
    manager: &Manager,
    path: &std::path::Path,
    plan: &str,
) {
    let before = std::fs::read_to_string(path).unwrap();
    let (held, after) = config_while_and_after_the_write_lock(manager, path);
    assert_eq!(
        held, before,
        "nothing reaches the file while another writer holds the lock"
    );
    let after: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        after["accounts"][0]["organizationType"], plan,
        "once the lock is released, the plan is written"
    );
}

/// **The Codex plan write holds the config writers' lock** (INV1). `save_plan` reads the
/// whole document, changes one key and writes the document back, so without the lock a
/// read taken before another writer's rename and written after it puts back what that
/// writer replaced: a refresh token it had just rotated, which is single-use and already
/// spent, so that account is dead after its next restart. While another writer holds the
/// lock, nothing reaches the file; once it is released, the plan does.
///
/// Watched red: without the lock, the new plan is on disk while the lock is still held.
#[test]
fn a_codex_plan_write_waits_for_the_config_write_lock() {
    let path = tmp_config_path("codex-plan-lock");
    std::fs::write(
        &path,
        r#"{ "accounts": [
            { "name": "bob@example.com/codex", "type": "oauth", "provider": "codex",
              "credentialVersion": 2,
              "accountUuid": "22222222-2222-4222-8222-222222222222",
              "organizationType": "plus", "priority": 0,
              "accessToken": "at-bob", "refreshToken": "rt-bob",
              "expiresAt": 4102444800000 }
        ] }"#,
    )
    .unwrap();
    let manager = manager_on_file(&path, Arc::new(CodexPlanProber { plan: "pro" }));
    count_plan_fetches(&manager, crate::probe::Plan::default());

    assert_the_plan_write_waits_for_the_write_lock(&manager, &path, "pro");
    std::fs::remove_file(&path).ok();
}

/// The plan the profile backfill learns is written under the same lock, for the same
/// reason: it is the other writer `save_plan` has.
///
/// Watched red: without the lock, the learned plan is on disk while the lock is still held.
#[test]
fn a_backfilled_plan_write_waits_for_the_config_write_lock() {
    let path = config_file("plan-backfill-lock", ALICE_WITHOUT_A_PLAN);
    let manager = manager_on_file(&path, Arc::new(NoProber));
    count_plan_fetches(
        &manager,
        crate::probe::Plan {
            organization_type: Some("claude_team".to_string()),
            rate_limit_tier: None,
            seat_tier: None,
            plan_display_name: None,
        },
    );

    assert_the_plan_write_waits_for_the_write_lock(&manager, &path, "claude_team");
    std::fs::remove_file(&path).ok();
}

/// A Codex usage prober that answers with what the real parser makes of one payload.
struct CodexPayloadProber {
    payload: serde_json::Value,
}

impl UsageProber for CodexPayloadProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let usage = crate::codex_probe::usage_from_payload(&self.payload);
        Box::pin(async move { Ok(usage) })
    }
}

/// **An empty `plan_type` names no plan.** A probe that answers `""`, or only whitespace,
/// leaves the account's plan as it was, in memory and on disk, so the plan label never goes
/// blank. A plan the probe does name is still taken verbatim.
///
/// Watched red: take `plan_type` with no emptiness check, and the row's plan becomes `""`.
#[tokio::test]
async fn a_probe_with_an_empty_plan_type_leaves_the_plan_as_it_was() {
    for plan_type in ["", "   "] {
        let path = config_file("codex-empty-plan", BOB_CODEX_ON_PLUS);
        let before = std::fs::read(&path).unwrap();
        let manager = manager_on_file(
            &path,
            Arc::new(CodexPayloadProber {
                payload: serde_json::json!({ "plan_type": plan_type, "rate_limit": null }),
            }),
        );
        count_plan_fetches(&manager, crate::probe::Plan::default());

        manager.probe_account(0).await;

        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0]
                .organization_type
                .as_deref(),
            Some("plus"),
            "plan_type {plan_type:?} names no plan"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
                .plan_label()
                .as_deref(),
            Some("ChatGPT Plus"),
            "plan_type {plan_type:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "plan_type {plan_type:?} writes nothing"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// A profile fetch that failed says nothing about the account's quota or its
/// credential. Recording a probe failure over a cosmetic label would bench a
/// healthy account, so this path must be entirely silent on failure — and
/// must ask again next time rather than latching.
#[tokio::test]
async fn a_failed_plan_fetch_records_nothing_and_never_fails_the_probe() {
    let path = config_file("plan-fetch-fails", ALICE_WITHOUT_A_PLAN);
    let manager = manager_on_file(&path, Arc::new(NoProber));
    let before = std::fs::read_to_string(&path).unwrap();

    // An all-`None` reading is exactly what `fetch_profile` returns on
    // any failure — network, non-2xx, or a malformed body.
    let calls = count_plan_fetches(&manager, crate::probe::Plan::default());

    manager.probe_account(0).await;

    assert_eq!(
        manager.accounts.read().expect("lock")[0].organization_type,
        None,
        "nothing was learned, so nothing is recorded"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "a failed profile fetch must not rewrite the credential file"
    );

    manager.probe_account(0).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "a failure must not latch — the account still has no plan, so the \
         next probe asks again"
    );
    std::fs::remove_file(&path).ok();
}

/// An account whose plan came from config at boot is already answered, so
/// the probe loop never spends a request on it.
#[tokio::test]
async fn an_account_that_already_has_a_plan_is_never_re_fetched() {
    let path = config_file(
        "plan-already-known",
        r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
              "refreshToken": "rt-a", "priority": 0,
              "organizationType": "claude_max",
              "rateLimitTier": "default_claude_max_20x",
              "planDisplayName": "Ultra" }
        ] }"#,
    );
    let manager = manager_on_file(&path, Arc::new(NoProber));
    assert_eq!(
        manager.accounts.read().expect("lock")[0]
            .rate_limit_tier
            .as_deref(),
        Some("default_claude_max_20x"),
        "the plan is read from config at boot, not only learned by probing"
    );

    let calls = count_plan_fetches(&manager, crate::probe::Plan::default());

    manager.probe_account(0).await;
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    std::fs::remove_file(&path).ok();
}

/// A prober that records the credential of every probe and answers an empty
/// success, so a test reads exactly what [`Manager::probe_account`] built.
struct RecordingProber {
    seen: Arc<Mutex<Vec<crate::probe::ProbeCredential>>>,
}

impl UsageProber for RecordingProber {
    fn probe(&self, credential: crate::probe::ProbeCredential) -> ProbeFuture {
        self.seen
            .lock()
            .expect("seen lock poisoned")
            .push(credential);
        Box::pin(async { Ok(Usage::default()) })
    }
}

/// Row `idx`'s `quotaUpdatedAtMs` in the `tcr.status.v1` payload the server would send now.
fn quota_updated_at_on_the_wire(manager: &Manager, idx: usize) -> Option<i64> {
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let payload =
        crate::status::StatusPayload::from_snapshot(&snap, &[], false, None, Default::default());
    let json = serde_json::to_value(&payload).expect("payload serializes");
    json["accounts"][idx]["quotaUpdatedAtMs"].as_i64()
}

/// An Anthropic row, `alice@example.com`, whose account uuid is all ones.
fn alice_with_an_account_uuid() -> Account {
    Account {
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        ..account("alice@example.com", 0)
    }
}

/// Probe each of `accounts` once, in order, and answer the credentials the probes carried.
/// The plan backfill gets a counting fake, so no row asks the live profile endpoint.
async fn probe_credentials_of(accounts: Vec<Account>) -> Vec<crate::probe::ProbeCredential> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let count = accounts.len();
    let manager = build_manager_with_prober(
        config_with(accounts),
        counting_refresher(),
        Arc::new(RecordingProber {
            seen: Arc::clone(&seen),
        }),
    );
    count_plan_fetches(&manager, crate::probe::Plan::default());
    for idx in 0..count {
        manager.probe_account(idx).await;
    }
    seen.lock().expect("seen lock poisoned").clone()
}

/// **A probe names the account it is for, not just the bearer it pays with.** The
/// provider picks the usage endpoint, and a Codex probe must also send its ChatGPT
/// account id in `chatgpt-account-id`, which is the row's `account_uuid`. An Anthropic
/// row and a Codex row are probed, so each provider's credential is pinned beside the
/// other's.
///
/// Watched red: build the credential with `account_id: None` and the Codex row's
/// account id is missing.
#[tokio::test]
async fn a_probe_credential_names_the_accounts_provider_and_account_uuid() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        ..account("bob@example.com", 1)
    };
    let seen: Vec<_> = probe_credentials_of(vec![alice_with_an_account_uuid(), codex])
        .await
        .iter()
        .map(|c| (c.provider, c.access_token.clone(), c.account_id.clone()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                crate::provider::Provider::Anthropic,
                "at-alice@example.com".to_string(),
                Some("11111111-1111-4111-8111-111111111111".to_string()),
            ),
            (
                crate::provider::Provider::Codex,
                "at-bob@example.com".to_string(),
                Some("22222222-2222-4222-8222-222222222222".to_string()),
            ),
        ],
        "each probe carries its own row's provider, bearer and account id"
    );
}

/// **A Codex probe names the workspace.** A Codex row's `chatgpt-account-id` is its
/// workspace, `org_uuid`, since `account_uuid` became the ChatGPT user; a row saved before
/// that split carries the workspace in `account_uuid` and no `org_uuid`, and keeps sending
/// it. An Anthropic row's credential still names its `account_uuid`.
///
/// Watched red: with the Codex arm reading `account_uuid`, bob's probe names his user.
#[tokio::test]
async fn a_codex_probe_names_the_workspace_and_falls_back_to_the_account_uuid() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("55555555-5555-4555-8555-555555555555".to_string()),
        org_uuid: Some("66666666-6666-4666-8666-666666666666".to_string()),
        ..account("bob@example.com", 1)
    };
    let older = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("77777777-7777-4777-8777-777777777777".to_string()),
        ..account("carol@example.com", 2)
    };
    let seen: Vec<_> = probe_credentials_of(vec![alice_with_an_account_uuid(), codex, older])
        .await
        .iter()
        .map(|c| (c.provider, c.account_id.clone()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                crate::provider::Provider::Anthropic,
                Some("11111111-1111-4111-8111-111111111111".to_string()),
            ),
            (
                crate::provider::Provider::Codex,
                Some("66666666-6666-4666-8666-666666666666".to_string()),
            ),
            (
                crate::provider::Provider::Codex,
                Some("77777777-7777-4777-8777-777777777777".to_string()),
            ),
        ]
    );
}

/// A refresher that rejects every refresh as `status` with `code`, the way a dead login is
/// rejected.
struct RejectsWith {
    status: u16,
    code: Option<&'static str>,
}
impl TokenRefresher for RejectsWith {
    fn refresh(
        &self,
        _provider: crate::provider::Provider,
        _refresh_token: String,
    ) -> RefreshFuture {
        let (status, code) = (self.status, self.code);
        Box::pin(async move { Err(OAuthError::AuthRejected { status, code }) })
    }
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

fn codex_account(name: &str, priority: i64) -> Account {
    Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("55555555-5555-4555-8555-555555555555".to_string()),
        org_uuid: Some("66666666-6666-4666-8666-666666666666".to_string()),
        ..account(name, priority)
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

/// **The folder rides the wire on Codex rows only.** A Codex row imported from a folder
/// echoes it; an Anthropic row has none to echo.
///
/// Watched red: without the copy in `from_config`, the Codex row echoes nothing.
#[test]
fn the_snapshot_echoes_codex_home_on_codex_rows_only() {
    let codex = Account {
        codex_home: Some("/Users/example/.codex".to_string()),
        ..codex_account("bob@example.com", 1)
    };
    let manager = manager_of(vec![account("alice@example.com", 0), codex]);
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(snap.accounts[0].codex.codex_home, None);
    assert_eq!(
        snap.accounts[1].codex.codex_home.as_deref(),
        Some("/Users/example/.codex")
    );
}

/// **A login revives a deleted Codex row with its new folder and no stale error.** A row
/// deleted while the proxy ran keeps its slot, parked; a login of the same identity brings
/// it back through `add_or_update_account`'s Updated arm, and the durable write appends the
/// whole row again, because the file has no entry left to update. Both of the row's Codex
/// facts must come from that login: no `login_error` (the login is live), and the
/// submission's folder, in the running proxy and in the file.
///
/// Watched red twice: without the `login_error` clear, the revived row still reads
/// `refresh_token_reused`; without `codex_home` in the persisted row, the file's entry has
/// no `codexHome`.
#[tokio::test]
async fn a_relogin_of_a_deleted_codex_row_revives_it_with_its_folder_and_no_login_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(crate::paths::CONFIG_FILE);
    let alice = account("alice@example.com", 0);
    let bob = Account {
        codex_home: Some("/Users/example/.codex".to_string()),
        ..codex_account("bob@example.com", 1)
    };
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({ "accounts": [alice.clone()] }))
            .expect("serialize"),
    )
    .expect("write the file");
    let manager = Manager::new(
        config_with(vec![alice, bob.clone()]),
        Arc::new(RejectsWith {
            status: 400,
            code: Some("refresh_token_reused"),
        }),
        Arc::new(NoProber),
        Arc::new(NoWarmer),
        Some(path.clone()),
    );

    manager.ensure_fresh_force(1).await;
    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert!(
        before.accounts.iter().all(|a| a.name != "bob@example.com"),
        "the file lacks bob, so the reload took him out"
    );

    let relogin = Account {
        access_token: "at-bob-fresh".to_string(),
        refresh_token: Some("rt-bob-fresh".to_string()),
        codex_home: Some("/Users/example/.codex-b".to_string()),
        ..bob
    };
    assert!(matches!(
        manager.add_or_update_account(relogin),
        AddAccountOutcome::Updated { idx: 1, .. }
    ));

    let after = manager.snapshot(OffsetDateTime::now_utc());
    let bobs: Vec<_> = after
        .accounts
        .iter()
        .filter(|a| a.name == "bob@example.com")
        .collect();
    assert_eq!(bobs.len(), 1, "bob is listed once");
    assert!(!bobs[0].disabled, "a fresh login serves");
    assert_eq!(bobs[0].codex.login_error, None);
    assert_eq!(
        bobs[0].codex.codex_home.as_deref(),
        Some("/Users/example/.codex-b")
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    let entry = doc["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .find(|a| a["name"] == "bob@example.com")
        .expect("bob is back in the file");
    assert_eq!(entry["codexHome"], "/Users/example/.codex-b");
}

/// **A row that is gone yields no credential, never an Anthropic one.** The provider
/// picks the usage endpoint, so a credential built on the default provider would send the
/// bearer to Anthropic's, whichever provider issued it. No row can vanish today, so this
/// pins the fallback rather than a path: an index past the end builds nothing, and
/// `probe_account` stands down on that (read, not driven: its token read already returns
/// early for such an index). The row that is there is the control.
///
/// Watched red: fall back to `Provider::default()` and the index past the end gets an
/// Anthropic credential.
#[test]
fn a_row_that_is_gone_yields_no_probe_credential() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        ..account("bob@example.com", 0)
    };
    let manager = manager_of(vec![codex]);
    let gone = manager.probe_credential(1, "at-bob@example.com".to_string());
    assert!(
        gone.is_none(),
        "an index past the end has no provider to pick an endpoint by: {gone:?}"
    );
    let control = manager
        .probe_credential(0, "at-bob@example.com".to_string())
        .expect("control: the row that is there yields its credential");
    assert_eq!(control.provider, crate::provider::Provider::Codex);
    assert_eq!(
        control.account_id.as_deref(),
        Some("22222222-2222-4222-8222-222222222222")
    );
}

/// **A Codex account is never asked the Anthropic profile endpoint for its plan.** That
/// endpoint is the backfill's only source, so asking it for a Codex row hands an OpenAI
/// bearer to the wrong provider; and since no plan ever comes back, the once-per-account
/// gate never closes and it would be asked again on every probe. The Anthropic row in
/// the same fleet is still asked, so the skip is the provider and not a backfill that
/// stopped running.
///
/// Watched red: drop the Codex early return in `backfill_plan_if_unknown` and the plan
/// prober is asked for the Codex row.
#[tokio::test]
async fn a_codex_account_is_never_asked_the_anthropic_profile_for_its_plan() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com", 1)
    };
    let manager = build_manager_with_prober(
        config_with(vec![account("alice@example.com", 0), codex]),
        counting_refresher(),
        Arc::new(NoProber),
    );
    let calls = count_plan_fetches(&manager, crate::probe::Plan::default());

    manager.probe_account(1).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "the Codex row must never reach the Anthropic profile endpoint"
    );

    manager.probe_account(0).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "control: the Anthropic row is still asked"
    );
}

/// **A Codex `rejected` is lifted by a probe that sees its window roll**, the path an
/// Anthropic rejection already takes ([`crate::quota::Quota::apply_usage`]). A Codex
/// quota refusal, a 429 carrying `x-codex-rate-limit-reached-type`, marks the account
/// `rejected`; `select()` treats that as terminal, so no served response comes back to
/// clear it, and before the Codex probe it stood until the process restarted.
///
/// End to end from the production constructor: `with_live_refresher` points the live
/// prober at the configured `codexUpstream` (a local stub here), the Codex arm sends the
/// row's bearer and `chatgpt-account-id`, and the measured weekly-only payload (the
/// weekly window in the PRIMARY slot, at 2% once it has rolled) lands in `seven_day`,
/// the window the refusal left at 100%.
///
/// The prober's Codex base is checked before anything probes, so a regressed wiring
/// fails here, offline, instead of sending the fixture bearer to the real backend.
///
/// Watched red, one break at a time: drop the `with_codex_base` chain from
/// `with_live_refresher` and the prober's base is the real backend's, caught before any
/// probe; map the payload by position and the 2% lands in `five_hour`, the stored weekly
/// 100% never falls, and the account stays rejected.
#[tokio::test]
async fn a_codex_rejection_is_lifted_when_a_probe_sees_its_weekly_window_roll() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let usage_endpoint = axum::Router::new().route(
        crate::provider::CODEX_USAGE_PATH,
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let recorder = Arc::clone(&recorder);
            async move {
                let header = |name: &str| {
                    headers
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                recorder
                    .lock()
                    .expect("recorder lock poisoned")
                    .push((header("authorization"), header("chatgpt-account-id")));
                axum::Json(serde_json::json!({
                    "plan_type": "pro",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {
                            "used_percent": 2,
                            "limit_window_seconds": 604_800,
                            "reset_after_seconds": 604_000,
                            "reset_at": now + 604_000,
                        },
                        "secondary_window": null,
                    },
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the usage stub");
    let codex_upstream = format!("http://{}", listener.local_addr().expect("stub address"));
    tokio::spawn(async move {
        crate::log_err::LogErr::log_debug(
            axum::serve(listener, usage_endpoint).await,
            "serving the usage stub",
        );
    });

    let mut config = config_with(vec![Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        ..account("bob@example.com", 0)
    }]);
    config.codex_upstream = codex_upstream.clone();
    let manager = Manager::with_live_refresher(config, None);
    assert_eq!(
        manager.prober.codex_base(),
        Some(codex_upstream.as_str()),
        "the live prober must probe Codex at the configured codexUpstream"
    );
    // The backfill skips a Codex row; should that ever break, this keeps the live
    // profile endpoint out of the test.
    count_plan_fetches(&manager, crate::probe::Plan::default());
    let codex_request = || {
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            crate::provider::CODEX_RESPONSES_PATH,
            None,
        )
    };

    // The refusal: the weekly window spent, reported in the primary slot.
    let mut refusal = reqwest::header::HeaderMap::new();
    refusal.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    refusal.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    refusal.insert(
        "x-codex-primary-reset-at",
        (now + 3600).to_string().parse().unwrap(),
    );
    refusal.insert(
        crate::quota::CODEX_REACHED_TYPE_HEADER,
        "primary".parse().unwrap(),
    );
    manager.update_quota(0, &refusal, 429);
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0]
            .quota
            .status
            .as_deref(),
        Some("rejected"),
        "precondition: the refusal marked the account rejected"
    );
    assert_eq!(
        codex_request(),
        None,
        "precondition: a rejected account serves nothing"
    );

    manager.probe_account(0).await;

    assert_eq!(
        seen.lock().expect("recorder lock poisoned").clone(),
        vec![(
            Some("Bearer at-bob@example.com".to_string()),
            Some("22222222-2222-4222-8222-222222222222".to_string()),
        )],
        "one probe, at the configured upstream, with the row's bearer and account id"
    );
    let account = manager.accounts.read().expect("accounts lock poisoned")[0].clone();
    assert_eq!(
        account.quota.status, None,
        "the weekly window rolled, so the rejection recorded against it is stale"
    );
    assert_eq!(
        codex_request(),
        Some(0),
        "and the account is back in rotation"
    );
    let weekly = account.quota.seven_day.expect("the probe's weekly reading");
    assert!(
        (weekly.utilization - 0.02).abs() < 1e-9,
        "the probe's 2% replaced the refusal's 100%, got {}",
        weekly.utilization
    );
    assert!(
        account.quota.five_hour.is_none(),
        "the payload carried no five-hour window, so none may be invented"
    );
}

/// A manager over `alice@example.com` at priority 0, then one Codex row
/// `<name>@example.com/codex` per name, at priorities 1, 2, and on.
fn alice_then_codex_rows(names: &[&str]) -> Arc<Manager> {
    let codex = names.iter().zip(1..).map(|(name, priority)| Account {
        provider: crate::provider::Provider::Codex,
        ..account(&format!("{name}@example.com/codex"), priority)
    });
    manager_of(
        std::iter::once(account("alice@example.com", 0))
            .chain(codex)
            .collect(),
    )
}

/// Codex quota headers saying the weekly window (the only one the measured plan
/// reports) is `used` percent spent, resetting a day out.
fn codex_weekly(used: &str) -> reqwest::header::HeaderMap {
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 86_400;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-codex-primary-used-percent", used.parse().unwrap());
    headers.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    headers.insert(
        "x-codex-primary-reset-at",
        reset.to_string().parse().unwrap(),
    );
    headers
}

/// **A Codex row's `quotaUpdatedAtMs` comes from that row's own readings**, both
/// kinds: a served response's Codex quota headers, which `update_quota` routes to
/// the Codex parser by the row's provider, and a successful Codex probe. The
/// Anthropic row beside it read nothing and stays absent, and a Codex response
/// that carried no quota header leaves the time where it was. Walked to the
/// `tcr.status.v1` payload, the hop `tcr status --json` reads.
///
/// Watched red: without the stamp in `Quota::update_from_codex_refusal_at`, the
/// header reading leaves the Codex row absent.
#[tokio::test]
async fn a_codex_rows_quota_updated_at_comes_from_its_own_readings() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        ..account("bob@example.com/codex", 1)
    };
    let manager = build_manager_with_prober(
        config_with(vec![account("alice@example.com", 0), codex]),
        counting_refresher(),
        Arc::new(RecordingProber {
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let wire_value = quota_updated_at_on_the_wire;
    let stale = crate::now_ms() - 40 * 60 * 1000;
    let rewind = |manager: &Manager| {
        accounts_mut(manager)[1].quota.updated_at = Some(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(stale) * 1_000_000)
                .expect("valid instant"),
        );
    };
    assert_eq!(wire_value(&manager, 1), None, "nothing read yet");

    let before = crate::now_ms();
    manager.update_quota(1, &codex_weekly("40"), 200);
    let read = wire_value(&manager, 1).expect("a Codex header reading carries quotaUpdatedAtMs");
    assert!(
        read >= before,
        "a fresh Codex reading reads as recent: {read} < {before}"
    );
    assert_eq!(
        wire_value(&manager, 0),
        None,
        "the Anthropic row read nothing"
    );

    rewind(&manager);
    manager.update_quota(1, &reqwest::header::HeaderMap::new(), 502);
    assert_eq!(
        wire_value(&manager, 1),
        Some(stale),
        "silence is not a reading"
    );

    rewind(&manager);
    let before = crate::now_ms();
    manager.probe_account(1).await;
    let probed = wire_value(&manager, 1).expect("still present");
    assert!(
        probed >= before,
        "a successful Codex probe moves it: {probed} < {before}"
    );
}

/// **#466's shorter probe cadence reads a Codex row by its own readings.** A Codex
/// account whose weekly window is at its threshold, or that a Codex quota refusal
/// marked rejected below it, is out of rotation, so no served response refreshes it
/// and only a probe sees its window roll: it gets the near-limit cadence. An idle
/// Codex account keeps the routine one, as does the Anthropic account that read
/// nothing. The probe that cadence fires is the row's own provider's
/// (`a_probe_credential_names_the_accounts_provider_and_account_uuid` pins the
/// credential), and the hourly cap counts each account apart, so a faster Codex
/// cadence sends Codex bearers to `wham/usage` and nothing to Anthropic.
///
/// Watched red: read every Codex row as routine in `ProbeUrgency::of`, and bob and
/// carol lose the shorter cadence.
#[test]
fn a_held_codex_account_gets_the_near_limit_probe_cadence() {
    use crate::schedule::ProbeUrgency;
    let manager = alice_then_codex_rows(&["bob", "carol", "dave"]);
    // bob: at its weekly limit, read from a served 200.
    manager.update_quota(1, &codex_weekly("97"), 200);
    // carol: a Codex quota refusal at half its weekly window.
    let mut refusal = codex_weekly("50");
    refusal.insert(
        crate::quota::CODEX_REACHED_TYPE_HEADER,
        "primary".parse().unwrap(),
    );
    manager.update_quota(2, &refusal, 429);
    // dave: idle.
    manager.update_quota(3, &codex_weekly("10"), 200);

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let urgency: Vec<(&str, ProbeUrgency)> = snap
        .accounts
        .iter()
        .map(|a| (a.name.as_str(), ProbeUrgency::of(a)))
        .collect();
    assert_eq!(
        urgency,
        vec![
            ("alice@example.com", ProbeUrgency::Routine),
            ("bob@example.com/codex", ProbeUrgency::NearLimit),
            ("carol@example.com/codex", ProbeUrgency::NearLimit),
            ("dave@example.com/codex", ProbeUrgency::Routine),
        ]
    );
    assert_eq!(
        (snap.accounts[2].gate, snap.accounts[2].quota_state),
        (GateReason::Rejected, crate::stats::QuotaState::Normal),
        "control: carol is held by its refusal, not by its threshold"
    );
}

/// **A Codex refusal keeps the near-limit probe cadence whatever gate its row reports.**
/// The snapshot reports a standing refusal as the window or the hold that explains it
/// (`Manager::codex_refusal_gate`), not as `Rejected`, and `ProbeUrgency::of` reads that
/// row. The refusal still stands until a probe ends it, so the account keeps the cadence a
/// `rejected` one gets: while the hold the proxy armed for the refusal lasts (`Hold`),
/// while an upstream 403's hold lasts (`Refused`), and past the refusing window's reset
/// before a probe confirms the roll (that window, with no time). Every one of them reads
/// below its threshold at that moment, so only the refusal can set the cadence. A hold no
/// refusal set stays routine, as does the Anthropic account, and so does a refused account
/// whose login then died: `Login` outranks `Rejected`, so the refusal never was what held
/// it out, and before the remap it read routine too.
///
/// Watched red: read only `GateReason::Rejected` in `ProbeUrgency::of`, and bob, carol and
/// erin read routine; read any standing refusal whatever the gate, and frank reads
/// near-limit.
#[test]
fn a_codex_refusal_keeps_the_near_limit_cadence_whatever_gate_its_row_reports() {
    use crate::schedule::ProbeUrgency;
    use crate::stats::{GateReason, QuotaState};
    let manager = alice_then_codex_rows(&["bob", "carol", "dave", "erin", "frank"]);
    let refusal = |mut headers: reqwest::header::HeaderMap| {
        headers.insert(
            crate::quota::CODEX_REACHED_TYPE_HEADER,
            "rate_limit_reached".parse().unwrap(),
        );
        headers
    };
    // bob: refused at half its weekly window, held as the proxy holds every refusal.
    manager.update_quota(1, &refusal(codex_weekly("50")), 429);
    manager.mark_rate_limited(1, 1800);
    // carol: refused on a full weekly window that rolls in half a minute, read a minute on,
    // before any probe.
    let soon = OffsetDateTime::now_utc().unix_timestamp() + 30;
    let mut full = reqwest::header::HeaderMap::new();
    full.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    full.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    full.insert(
        "x-codex-primary-reset-at",
        soon.to_string().parse().unwrap(),
    );
    manager.update_quota(2, &refusal(full), 429);
    // dave: a hold no refusal set, over an idle weekly window.
    manager.update_quota(3, &codex_weekly("10"), 200);
    manager.mark_rate_limited(3, 1800);
    // erin: refused at half its weekly window, then held by an upstream 403.
    manager.update_quota(4, &refusal(codex_weekly("50")), 429);
    manager.mark_refused(4);
    // frank: refused at half its weekly window, then its login died.
    manager.update_quota(5, &refusal(codex_weekly("50")), 429);
    accounts_mut(&manager)[5].status = AccountStatus::Error;

    // Past carol's reset, well inside erin's two-minute hold.
    let later = OffsetDateTime::now_utc() + Duration::minutes(1);
    let snap = manager.snapshot(later);
    let rows: Vec<(&str, GateReason, QuotaState, ProbeUrgency)> = snap
        .accounts
        .iter()
        .map(|a| (a.name.as_str(), a.gate, a.quota_state, ProbeUrgency::of(a)))
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "alice@example.com",
                GateReason::Ok,
                QuotaState::Normal,
                ProbeUrgency::Routine
            ),
            (
                "bob@example.com/codex",
                GateReason::Hold,
                QuotaState::Normal,
                ProbeUrgency::NearLimit
            ),
            (
                "carol@example.com/codex",
                GateReason::SevenDay,
                QuotaState::Normal,
                ProbeUrgency::NearLimit
            ),
            (
                "dave@example.com/codex",
                GateReason::Hold,
                QuotaState::Normal,
                ProbeUrgency::Routine
            ),
            (
                "erin@example.com/codex",
                GateReason::Refused,
                QuotaState::Normal,
                ProbeUrgency::NearLimit
            ),
            (
                "frank@example.com/codex",
                GateReason::Login,
                QuotaState::Normal,
                ProbeUrgency::Routine
            ),
        ]
    );
}

/// **A Codex row's snapshot reports its quota facts, and the refusal's kind exactly while
/// that refusal stands.** The refusal sets the kind, the credits and which windows the plan
/// has; a probe that sees the window roll ends the refusal and its kind; a new refusal
/// brings a new kind, which a probe refreshes while it stands, and that probe, carrying no
/// window, reports `[]`: the plan has none, which is a fact, not an unknown; a 2xx that
/// carried a window ends it, and a probe then never brings the kind back. An Anthropic row
/// reports none of it, even with a stray fact on its quota.
///
/// Watched red, one break at a time: fill the facts for every row and the Anthropic row
/// reports the stray credits; let the accessor ignore the status and drop the clear on a
/// 2xx, and the kind outlives the refusal; filter an empty list out in
/// `codex_snapshot_facts`, and the `[]` reads `None`.
#[test]
fn the_reached_type_is_present_exactly_while_the_refusal_stands() {
    use crate::codex_probe::CodexProbeFacts;
    use tcr_status_wire::CodexCredits;
    let manager = manager_of(vec![
        Account {
            provider: crate::provider::Provider::Codex,
            account_uuid: Some("22222222-2222-4222-8222-222222222222".into()),
            ..account("bob@example.com", 0)
        },
        account("alice@example.com", 1),
    ]);
    let snap = |idx: usize| manager.snapshot(OffsetDateTime::now_utc()).accounts[idx].clone();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let now_ms = crate::now_ms();
    let headers = |pairs: &[(&str, String)]| {
        let mut h = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("a name"),
                value.parse().expect("a header value"),
            );
        }
        h
    };
    let credits = Some(CodexCredits {
        has_credits: true,
        unlimited: false,
        balance: Some("12.50".into()),
    });

    // 1. The weekly refusal.
    let refusal = headers(&[
        ("x-codex-primary-used-percent", "100".into()),
        ("x-codex-primary-window-minutes", "10080".into()),
        ("x-codex-primary-reset-at", (now + 3 * 86_400).to_string()),
        (
            crate::quota::CODEX_REACHED_TYPE_HEADER,
            "rate_limit_reached".into(),
        ),
        ("x-codex-credits-has-credits", "true".into()),
        ("x-codex-credits-unlimited", "false".into()),
        ("x-codex-credits-balance", "12.50".into()),
    ]);
    manager.update_quota(0, &refusal, 429);
    let bob = snap(0).codex;
    assert_eq!(
        bob.limit_reached_type.as_deref(),
        Some("rate_limit_reached")
    );
    assert_eq!(bob.quota_windows, Some(vec!["sevenDay".to_string()]));
    assert_eq!(bob.seven_day_window_minutes, Some(10_080));
    assert_eq!(bob.five_hour_window_minutes, None);
    assert_eq!(bob.credits, credits);
    assert!(!bob.spend_control_reached);

    // 2. A probe sees the weekly window roll: the refusal ends, and its kind with it.
    manager.apply_usage(
        0,
        &Usage {
            seven_day: Some(UsageBucket {
                utilization: Some(0.02),
                reset_at_ms: Some(now_ms + 604_000_000),
            }),
            ..Usage::default()
        },
    );
    assert_eq!(snap(0).codex.limit_reached_type, None, "the window rolled");

    // 3. A refusal no full window explains brings its own kind.
    let refusal_no_full_window = headers(&[
        ("x-codex-primary-used-percent", "40".into()),
        ("x-codex-primary-window-minutes", "10080".into()),
        ("x-codex-primary-reset-at", (now + 3 * 86_400).to_string()),
        (
            crate::quota::CODEX_REACHED_TYPE_HEADER,
            "workspace_credits".into(),
        ),
    ]);
    manager.update_quota(0, &refusal_no_full_window, 429);
    assert_eq!(
        snap(0).codex.limit_reached_type.as_deref(),
        Some("workspace_credits")
    );

    // 4. While it stands, a probe's kind refreshes it.
    let probe_kind = Usage {
        codex: Some(CodexProbeFacts {
            limit_reached_type: Some("rate_limit_reached".into()),
            ..Default::default()
        }),
        ..Usage::default()
    };
    manager.apply_usage(0, &probe_kind);
    assert_eq!(
        snap(0).codex.limit_reached_type.as_deref(),
        Some("rate_limit_reached"),
        "the probe refreshes the kind while the refusal stands"
    );
    assert_eq!(
        snap(0).codex.quota_windows,
        Some(Vec::new()),
        "a probe that carried no window says the plan reports none: [], not unknown"
    );

    // 5. A 2xx that carried a window ends the refusal; a probe never brings its kind back.
    let ok_with_window = headers(&[
        ("x-codex-primary-used-percent", "41".into()),
        ("x-codex-primary-window-minutes", "10080".into()),
        ("x-codex-primary-reset-at", (now + 3 * 86_400).to_string()),
    ]);
    manager.update_quota(0, &ok_with_window, 200);
    assert_eq!(snap(0).codex.limit_reached_type, None, "the 2xx ended it");
    manager.apply_usage(0, &probe_kind);
    assert_eq!(
        snap(0).codex.limit_reached_type,
        None,
        "a probe never brings the kind back without a refusal"
    );

    // 6. Control: the Anthropic row reports none of it, even with a stray fact.
    let anthropic_ok = headers(&[("anthropic-ratelimit-unified-5h-utilization", "0.20".into())]);
    manager.update_quota(1, &anthropic_ok, 200);
    assert_eq!(snap(1).codex, crate::stats::CodexFacts::default());
    manager.accounts.write().expect("accounts lock poisoned")[1]
        .quota
        .codex
        .credits = Some(CodexCredits::default());
    assert_eq!(
        snap(1).codex,
        crate::stats::CodexFacts::default(),
        "only a Codex row ever reports these facts"
    );
}

/// **A Codex refusal ends when a usage probe reads the account allowed**: allowed, not at
/// its limit, its spend control not reached, and no refusal kind. That is the backend's own
/// verdict, so a probe ends a refusal no window explains (a spend cap, credits) once ChatGPT
/// ends it. Each condition is needed: a probe that reads any one of them otherwise, or that
/// carries no `rate_limit` at all, leaves the refusal and its kind standing. The window-roll
/// lift still ends a refusal on a probe that says not allowed, and an Anthropic rejection is
/// untouched: its probe carries no verdict.
///
/// Driven through the real payload parser (`codex_probe::usage_from_payload`), each probe at
/// the refusal's own 60 percent, so no window rolls.
///
/// Watched red, one condition dropped at a time: without `allowed`, the probe carrying no
/// `rate_limit` lifts the refusal; without `limit_reached`, the probe allowed but at its
/// limit does; without the spend control, the spend-control probe does; without the kind,
/// the probe carrying one does.
#[test]
fn a_codex_refusal_clears_when_a_probe_reads_it_allowed() {
    use crate::codex_probe::usage_from_payload;
    use serde_json::{Value, json};
    let manager = manager_of(vec![
        Account {
            provider: crate::provider::Provider::Codex,
            account_uuid: Some("22222222-2222-4222-8222-222222222222".into()),
            ..account("bob@example.com", 0)
        },
        account("alice@example.com", 1),
    ]);
    let gate = |idx: usize| {
        let row = manager.snapshot(OffsetDateTime::now_utc()).accounts[idx].clone();
        (row.gate, row.codex.limit_reached_type)
    };
    let codex_request = || {
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            crate::provider::CODEX_RESPONSES_PATH,
            None,
        )
    };
    let reset_at = (OffsetDateTime::now_utc() + Duration::days(5)).unix_timestamp();
    // The Pro refusal: its one window, the weekly one, at 60 percent in the primary slot.
    let refuse = || {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-codex-primary-used-percent", "60".parse().unwrap());
        h.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
        h.insert(
            "x-codex-primary-reset-at",
            reset_at.to_string().parse().unwrap(),
        );
        h.insert(
            crate::quota::CODEX_REACHED_TYPE_HEADER,
            "rate_limit_reached".parse().unwrap(),
        );
        manager.update_quota(0, &h, 429);
    };
    let payload = |allowed: bool, limit_reached: bool, spend: bool, kind: Value, used: u32| {
        json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": allowed,
                "limit_reached": limit_reached,
                "primary_window": {
                    "used_percent": used,
                    "limit_window_seconds": 604_800,
                    "reset_at": reset_at,
                },
                "secondary_window": null,
            },
            "spend_control": { "reached": spend },
            "rate_limit_reached_type": kind,
        })
    };
    let refused = (GateReason::Rejected, Some("rate_limit_reached".to_string()));

    refuse();
    assert_eq!(gate(0), refused, "precondition: refused, with its kind");

    let kind = json!({ "type": "rate_limit_reached", "details": "fixture" });
    for (case, probe) in [
        (
            "not allowed, at its limit",
            payload(false, true, false, Value::Null, 60),
        ),
        (
            "allowed but at its limit",
            payload(true, true, false, Value::Null, 60),
        ),
        (
            "spend control reached",
            payload(true, false, true, Value::Null, 60),
        ),
        (
            "carrying a refusal kind",
            payload(true, false, false, kind, 60),
        ),
        (
            "no rate_limit",
            json!({
                "plan_type": "pro",
                "rate_limit": null,
                "spend_control": { "reached": false },
            }),
        ),
    ] {
        manager.apply_usage(0, &usage_from_payload(&probe));
        assert_eq!(gate(0), refused, "{case}: the refusal and its kind stand");
    }

    manager.apply_usage(
        0,
        &usage_from_payload(&payload(true, false, false, Value::Null, 60)),
    );
    assert_eq!(
        gate(0),
        (GateReason::Ok, None),
        "read allowed: the refusal and its kind end"
    );
    // The pool pick (`Manager::eligible`) reads the hold and the windows, never `rejected`,
    // so this holds before the lift too: what the lift ends is the refusal itself, its kind
    // and the gate every reader is shown, and with them the pin and control gates that read
    // `rejected` (`account_terminal_gate`).
    assert_eq!(codex_request(), Some(0), "and the account serves");

    // The window-roll lift, on a probe that says not allowed: the weekly reading fell.
    refuse();
    assert_eq!(gate(0), refused, "a fresh refusal");
    manager.apply_usage(
        0,
        &usage_from_payload(&payload(false, false, false, Value::Null, 2)),
    );
    assert_eq!(gate(0), (GateReason::Ok, None), "the window rolled");

    // Control: an Anthropic rejection, probed at the same reading, stands.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].quota.status = Some("rejected".to_string());
        accounts[1].quota.seven_day = Some(crate::quota::QuotaWindow {
            utilization: 0.6,
            reset: Some(OffsetDateTime::now_utc() + Duration::days(5)),
        });
    }
    manager.apply_usage(
        1,
        &Usage {
            seven_day: Some(UsageBucket {
                utilization: Some(0.6),
                reset_at_ms: Some(reset_at * 1000),
            }),
            ..Usage::default()
        },
    );
    assert_eq!(
        gate(1).0,
        GateReason::Rejected,
        "an Anthropic probe lifts nothing"
    );
}

/// A unique temp path per test — the suite runs tests in parallel threads of
/// ONE process, so a pid-only name collides.
fn tmp_config_path(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("tcr-mgr-{tag}-{}-{seq}.json", std::process::id()))
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

/// Set account `idx`'s weekly (`7d`) window near/over threshold with a future
/// reset — isolates the near-threshold gate from the live-5h gate.
fn set_7d(manager: &Manager, idx: usize, util: &str) {
    let now = OffsetDateTime::now_utc();
    let reset = (now + Duration::hours(2)).unix_timestamp();
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        "anthropic-ratelimit-unified-7d-utilization",
        util.parse().unwrap(),
    );
    h.insert(
        "anthropic-ratelimit-unified-7d-reset",
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

// ---- `--group` prefer-routing (Phase 1: PREFER, never `--only`) -----------

/// The prefer half of the feature: with exactly one account carrying the
/// requested group, an unpinned select with that group MUST land on it —
/// even though the OTHER account would win the ordinary (no-preference)
/// LRU/priority pick. Then, group-out the grouped account (via `disabled`,
/// the simplest hard gate) and confirm the SAME select call REFUSES
/// (`None`) rather than spilling into the pool — an explicit `--group` ask
/// is strict by default (2026-09-13): served by that group or not at all.
///
/// This used to assert a pool fallback here; that was the pre-2026-09-13
/// rule, encoded now by
/// [`group_preference_falls_back_to_the_pool_when_spill_to_pool_is_set`]
/// with the `spillToPool` escape hatch explicitly on.
#[test]
fn group_preference_prefers_then_refuses_when_exhausted() {
    let grouped = account_in_groups("grouped", 5, &["codereview"]);
    let plain = account("plain", 0); // lower priority number sorts FIRST ordinarily
    let manager = manager_of(vec![grouped, plain]);
    let now = OffsetDateTime::now_utc();

    // Sanity control: with NO group requested, priority ordering picks the
    // UNGROUPED account (index 1) — proves the group assertion below is
    // actually exercising the preference, not just priority order.
    assert_eq!(
        manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "no group requested: ordinary priority order picks the ungrouped account"
    );

    // The whole feature: `--group codereview` prefers index 0 despite its
    // worse priority.
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(0),
        "a group with capacity must be preferred over a better-priority ungrouped account"
    );

    // Gate the grouped account out entirely (disabled = hard-ineligible).
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
    }

    // POSITIVE CONTROL: the pool itself is still perfectly servable. Without
    // this, the `None` below would also be satisfied by a fleet where
    // nothing at all could be picked — proving the assertion about
    // strictness rather than about an empty pool.
    assert_eq!(
        manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "control: the ungrouped pool is servable, so a None below is strictness, not exhaustion"
    );

    // Strict-by-default: the group has no capacity, so an explicit ask for
    // it refuses rather than spilling into the whole pool.
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "group exhausted, no spillToPool set: must refuse, not spill into the pool"
    );
}

/// The escape hatch: with `groupSettings.<g>.spillToPool` set, the SAME
/// exhausted-group scenario as
/// [`group_preference_prefers_then_refuses_when_exhausted`] falls back to
/// the whole pool instead of refusing — the pre-2026-09-13 behaviour,
/// restored per group on request.
#[test]
fn group_preference_falls_back_to_the_pool_when_spill_to_pool_is_set() {
    let grouped = account_in_groups("grouped", 5, &["codereview"]);
    let plain = account("plain", 0);
    let mut config = config_with(vec![grouped, plain]);
    config.group_settings.insert(
        "codereview".to_string(),
        config::GroupSettings {
            reserved: false,
            parked: false,
            allow_control_account: false,
            spill_to_pool: true,
            color: None,
            extra: serde_json::Map::new(),
        },
    );
    let manager = build_manager(config, counting_refresher());
    let now = OffsetDateTime::now_utc();

    // Confirm the preference still applies before exhausting the group —
    // `spillToPool` only changes what happens on exhaustion.
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(0),
        "spillToPool does not change the prefer half of the feature"
    );

    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
    }

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(1),
        "spillToPool restores the pre-2026-09-13 rule: group exhausted, fall back to the pool"
    );
}

// ---- reserved groups (a reservation is not a preference) ------------------

/// Like [`config_with`] but with the named groups marked `reserved` in
/// `groupSettings` — mirrors [`config_with_lock`]/[`config_with_control`].
fn config_with_reserved(accounts: Vec<Account>, reserved: &[&str]) -> Config {
    let mut config = config_with(accounts);
    for group in reserved {
        config.group_settings.insert(
            (*group).to_string(),
            config::GroupSettings {
                reserved: true,
                parked: false,
                allow_control_account: false,
                spill_to_pool: false,
                color: None,
                extra: serde_json::Map::new(),
            },
        );
    }
    config
}

/// Semantics test #1 (both directions): unrequested traffic cannot select
/// an account in a reserved group, even when its priority would otherwise
/// win — and the SAME account is reachable by unrequested traffic again
/// once its group is not reserved.
#[test]
fn reserved_group_blocks_unrequested_traffic_both_directions() {
    let reserved_acct = account_in_groups("reserved-acct", 0, &["codereview"]);
    let plain = account("plain", 5); // worse priority — must lose ordinarily
    let manager = build_manager(
        config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "reserved account excluded from unrequested traffic despite better priority"
    );

    // Direction 2: same accounts, group NOT reserved — priority wins again.
    let unreserved = manager_of(vec![
        account_in_groups("reserved-acct", 0, &["codereview"]),
        account("plain", 5),
    ]);
    assert_eq!(
        unreserved.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0),
        "same account, group unreserved: unrequested traffic can select it again"
    );
}

/// Semantics test #2: `--group codereview` can still select a reserved
/// `codereview` account — reservation narrows UNREQUESTED traffic, never
/// the group's own members.
#[test]
fn reserved_group_still_selectable_by_its_own_group_ask() {
    let reserved_acct = account_in_groups("reserved-acct", 0, &["codereview"]);
    let plain = account("plain", 5);
    let manager = build_manager(
        config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(0),
        "--group codereview still selects its own reserved account"
    );
}

/// Semantics test #2b: a RESERVED group stays strict — when its members
/// are all gated, the ask returns `None` instead of spilling into the
/// pool, and unlike an unreserved group this cannot be reopened with
/// `spillToPool` (see [`GroupSettings::spill_to_pool`]'s doc-comment).
///
/// Strict-by-default (2026-09-13) now covers every group, reserved or
/// not — see
/// [`group_preference_prefers_then_refuses_when_exhausted`] for the
/// unreserved case, which behaves identically here. What this test still
/// proves that the unreserved one does not: a reserved group's strictness
/// is NOT an opt-out surface. Without it, a reserved group's request could
/// silently land on an account the operator deliberately walled off from
/// it — measured on the live fleet 2026-09-01, 33 times in one day, every
/// one `reason="all-members-unavailable"`, after pool traffic had
/// rate-limited the group's only member.
#[test]
fn a_reserved_group_never_falls_back_to_the_pool() {
    let reserved_acct = account_in_groups("reserved-acct", 5, &["codereview"]);
    let plain = account("plain", 0); // better priority — would win any pool pick
    let manager = build_manager(
        config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();

    // Gate the group's only member out entirely (disabled = hard-ineligible),
    // exactly as `group_preference_prefers_then_refuses_when_exhausted` does.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
    }

    // POSITIVE CONTROL: the pool itself is still perfectly servable. Without
    // this, the `None` below would also be satisfied by a fleet where nothing
    // at all could be picked — proving the assertion about strictness rather
    // than about an empty pool.
    assert_eq!(
        manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "control: the ungrouped pool is servable, so a None below is strictness, not exhaustion"
    );

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "a reserved group must never spill into the pool — refuse and let the \
         caller's exhaustion ladder soft-wait or answer an honest 429"
    );
}

/// The escape hatch does not reopen a RESERVED group: `spillToPool: true`
/// on a reserved group's `groupSettings` entry is ignored, because
/// spilling its own traffic into the pool would hand it to an account
/// other traffic cannot reach any other way — the opposite of what
/// `reserved` promises inbound. Same scenario as
/// [`a_reserved_group_never_falls_back_to_the_pool`], with `spillToPool`
/// deliberately set to prove it has no effect here.
#[test]
fn a_reserved_group_ignores_spill_to_pool() {
    let reserved_acct = account_in_groups("reserved-acct", 5, &["codereview"]);
    let plain = account("plain", 0);
    let mut config = config_with(vec![reserved_acct, plain]);
    config.group_settings.insert(
        "codereview".to_string(),
        config::GroupSettings {
            reserved: true,
            parked: false,
            allow_control_account: false,
            spill_to_pool: true,
            color: None,
            extra: serde_json::Map::new(),
        },
    );
    let manager = build_manager(config, counting_refresher());
    let now = OffsetDateTime::now_utc();

    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
    }

    assert_eq!(
        manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "control: the ungrouped pool is servable"
    );

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "reserved + spillToPool: still refuses — spillToPool cannot reopen a reserved group"
    );
}

/// A strict group's retry hint is sized by when one of ITS members frees,
/// not by whichever account in the fleet un-gates first.
///
/// The fleet-wide [`Manager::retry_after_hint`] minimises over every
/// account, so an unrelated account recovering sooner wins. For a strict
/// `--group` request that number is a promise about capacity the request is
/// not allowed to use: the caller spends its one-shot soft-wait, wakes to
/// find its own member still held, and answers a 429 that was avoidable.
/// The member here is held 10x longer than the non-member precisely so the
/// two hints cannot coincide by accident.
#[test]
fn a_strict_groups_retry_hint_is_sized_by_its_own_member() {
    let member = account_in_groups("member", 0, &["codereview"]);
    let outsider = account("outsider", 1);
    let manager = build_manager(
        config_with_reserved(vec![member, outsider], &["codereview"]),
        counting_refresher(),
    );
    manager.mark_rate_limited(0, 600); // the group's member
    manager.mark_rate_limited(1, 60); // an unrelated account, free much sooner
    // AFTER the marks: each `mark_rate_limited` reads its own clock, so a
    // `now` taken before them is a few microseconds earlier than the
    // holds' anchor and the 60 s hint rounds up to 61 — which failed the
    // `<= 60` control on CI (#189) with nothing wrong in the code.
    let now = OffsetDateTime::now_utc();

    let fleet = manager.retry_after_hint(now, false, crate::provider::Provider::Anthropic);
    let scoped = manager.retry_after_hint_for_group(
        now,
        false,
        crate::provider::Provider::Anthropic,
        "codereview",
    );

    // Control: the fleet-wide hint really is won by the outsider, so the
    // assertion below is measuring the group scoping and not a tautology.
    assert!(
        fleet <= 60,
        "control: fleet-wide hint should follow the sooner outsider, got {fleet}s"
    );
    assert!(
        scoped > 500,
        "a strict group's hint must follow its own held member (~600s), got {scoped}s"
    );
}

/// Semantics test #3: an account in reserved `codereview` + plain `dev` is
/// reachable by BOTH `--group` values and NOT by unrequested traffic.
#[test]
fn reserved_plus_plain_group_reachable_by_either_ask_not_unrequested() {
    let multi = account_in_groups("multi", 0, &["codereview", "dev"]);
    let plain = account("plain", 5);
    let manager = build_manager(
        config_with_reserved(vec![multi, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(0),
        "reachable via its reserved group"
    );
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("dev"),
        ),
        Some(0),
        "reachable via its plain group too"
    );
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "unrequested traffic still excluded — one reserved group is enough"
    );
}

// ---- parked groups (a whole group out of rotation) ------------------------

/// Like [`config_with_reserved`] but sets `parked` instead — the two flags
/// are independent, so nothing here touches `reserved`.
fn config_with_parked(accounts: Vec<Account>, parked: &[&str]) -> Config {
    let mut config = config_with(accounts);
    for group in parked {
        config.group_settings.insert(
            (*group).to_string(),
            config::GroupSettings {
                reserved: false,
                parked: true,
                allow_control_account: false,
                spill_to_pool: false,
                color: None,
                extra: serde_json::Map::new(),
            },
        );
    }
    config
}

/// THE HARD GATE. A member of a parked group is never selected — not by
/// unrequested traffic, and not by an explicit `--group` ask for the parked
/// group itself, which is where parking differs from reservation: an
/// explicit ask lifts a reservation and does not lift a park.
///
/// The last assertion is the one that makes the ask honest rather than
/// silently rerouted: with the ONLY member of the asked-for group parked,
/// the request refuses (`None`) instead of spilling onto an unrelated
/// account, exactly as a reserved group's own ask does when no member can
/// serve.
#[test]
fn parked_group_member_is_never_selected() {
    let parked_acct = account_in_groups("parked-acct", 0, &["codereview"]);
    let plain = account("plain", 5); // worse priority — must lose ordinarily
    let manager = build_manager(
        config_with_parked(vec![parked_acct, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "a parked account must be excluded from unrequested traffic despite better priority"
    );
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "a --group ask for a parked group must not select the parked member, and \
         strict-by-default means it refuses rather than falling back to the pool"
    );

    // Same fixture with the parked group as the fleet's ONLY group member
    // pool: the ask has nowhere honest to go, so it refuses.
    let only_parked = build_manager(
        config_with_parked(
            vec![account_in_groups("parked-acct", 0, &["codereview"])],
            &["codereview"],
        ),
        counting_refresher(),
    );
    assert_eq!(
        only_parked.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "with every member parked the ask must refuse, not serve from somewhere else"
    );
}

/// A pin does not survive its group being parked: the pin is RE-KEYED, not
/// merely diverted for one request — the same treatment a newly-reserved
/// group's pin gets, because `parked_blocks` sits inside `account_hard_ok`.
#[test]
fn pin_reclaim_when_the_pinned_accounts_group_is_parked() {
    let manager = build_manager(
        config_with_parked(codereview_acct_and_other(), &["codereview"]),
        counting_refresher(),
    );
    let key = 4242u64;
    let served = select_with_a_fresh_pin_on_the_first_account(&manager, key)
        .expect("the unparked account is still eligible");
    assert_eq!(served, 1, "a parked account must not keep serving its pin");
    let pinned_now = pin_of(&manager, key);
    assert_eq!(
        pinned_now,
        Some(1),
        "the pin itself must move off the parked account"
    );
}

/// The LIVE half: nothing is parked at boot, a config write parks the
/// group, and the very next request — with no restart — sees the gate. This
/// is the claim `tcr group park`'s own help makes to the operator, so it is
/// measured rather than assumed.
#[test]
fn reload_picks_up_a_group_being_parked() {
    let (path, manager) = acct_and_other_on_disk("reload-group-park");

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0),
        "at boot nothing is parked, so the better-priority account serves"
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    let parked_config = config_with_parked(codereview_acct_and_other(), &["codereview"]);
    config::save(&path, &parked_config).expect("write parked reload config");

    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "a group parked on disk must take effect on the next request, with no restart"
    );
    let snapshot = manager.snapshot(now);
    assert_eq!(
        snapshot.accounts[0].gate,
        GateReason::Parked,
        "and the fleet view names the gate"
    );
    assert_eq!(
        snapshot.accounts[0].parked_groups,
        vec!["codereview".to_string()],
        "naming WHICH group parked the row"
    );

    std::fs::remove_file(&path).ok();
}

/// The two flags are independent and the row's own `disabled` wins the
/// REASON: a hand-disabled member of a parked group reads `disabled`, so
/// the panel never tells an operator a group parked a row they benched
/// themselves.
#[test]
fn a_disabled_member_of_a_parked_group_still_reads_disabled() {
    let mut disabled_member = account_in_groups("benched", 0, &["codereview"]);
    disabled_member.disabled = Some(true);
    let manager = build_manager(
        config_with_parked(
            vec![
                disabled_member,
                account_in_groups("live", 1, &["codereview"]),
                account("other", 5),
            ],
            &["codereview"],
        ),
        counting_refresher(),
    );
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snapshot.accounts[0].gate,
        GateReason::Disabled,
        "the row's own choice outranks its group's"
    );
    assert_eq!(
        snapshot.accounts[1].gate,
        GateReason::Parked,
        "its sibling, benched only by the group, says so"
    );
    assert_eq!(
        snapshot.accounts[0].parked_groups,
        vec!["codereview".to_string()],
        "the parked group is still REPORTED on the disabled row — only the gate differs"
    );
}

// ---- control-account opt-in groups (`allowControlAccount`) ----------------

/// Like [`config_with_reserved`] but sets `allowControlAccount` instead —
/// combined with [`config_with_control`]'s `control_account` set so the
/// fixture is actually control-only, not just labelled.
fn config_with_control_allowed(accounts: Vec<Account>, control: &str, groups: &[&str]) -> Config {
    let mut config = config_with_control(accounts, control);
    for group in groups {
        config.group_settings.insert(
            (*group).to_string(),
            config::GroupSettings {
                reserved: false,
                parked: false,
                allow_control_account: true,
                spill_to_pool: false,
                color: None,
                extra: serde_json::Map::new(),
            },
        );
    }
    config
}

/// The whole feature, both directions: a group whose only member is the
/// control account cannot serve `--group research` inference by default —
/// strict-by-default (2026-09-13) means that ask now refuses rather than
/// spilling to the pool, since an explicit `--group` ask is served by that
/// group or not at all — but the SAME fixture, with `allowControlAccount`
/// set on `research`, serves it.
#[test]
fn control_only_group_serves_inference_only_once_opted_in() {
    let control = account_in_groups("control-acct", 0, &["research"]);
    let other = account("other", 5);

    let blocked = build_manager(
        config_with_control(vec![control.clone(), other.clone()], "control-acct"),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        blocked.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "control: with no group requested, the pool is servable"
    );
    assert_eq!(
        blocked.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("research"),
        ),
        None,
        "not opted in: strict by default, refuses rather than falling back to the pool"
    );

    let mut opted_in_config = config_with_control(vec![control, other], "control-acct");
    opted_in_config.group_settings.insert(
        "research".to_string(),
        config::GroupSettings {
            reserved: false,
            parked: false,
            allow_control_account: true,
            spill_to_pool: false,
            color: None,
            extra: serde_json::Map::new(),
        },
    );
    let opted_in = build_manager(opted_in_config, counting_refresher());
    assert_eq!(
        opted_in.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("research"),
        ),
        Some(0),
        "opted in: the group's own `--group` ask may now select the control account"
    );
}

/// The opt-in is scoped to the GROUP, not to the control account globally:
/// an unrequested (no `--group`) inference request must still never select
/// the control account, even when its only group has opted in.
#[test]
fn control_opt_in_does_not_affect_unrequested_traffic() {
    let control = account_in_groups("control-acct", 0, &["research"]);
    let other = account("other", 5);
    let manager = build_manager(
        config_with_control_allowed(vec![control, other], "control-acct", &["research"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "unrequested traffic must still never select the control account"
    );
}

/// Semantics test #4 — THE test that proves the feature is real: a
/// session already pinned to an account whose group becomes reserved
/// (the on-disk-pin-predates-the-reservation case the bridge names) is
/// re-pinned away on its next UNREQUESTED request, not left serving the
/// reserved account forever. Seeds the pin directly into `manager.affinity`
/// — the shape a pin restored from disk at boot takes, exactly the
/// scenario the bridge's "reachable by a proxy restart or by a pin that
/// predates the reservation" sentence describes.
#[test]
fn pin_reclaim_when_the_pinned_accounts_group_is_reserved() {
    let manager = build_manager(
        config_with_reserved(codereview_acct_and_other(), &["codereview"]),
        counting_refresher(),
    );
    let key = 4242u64;
    let served = select_with_a_fresh_pin_on_the_first_account(&manager, key)
        .expect("the plain account is still eligible");
    assert_eq!(
        served, 1,
        "a reserved account must not keep serving an unrequested pinned session"
    );

    // The pin itself must have MOVED — "reservation is not a preference":
    // an ordinary per-request divert would leave the OLD index pinned and
    // only serve elsewhere for this one request; that is NOT enough here.
    let pinned_now = pin_of(&manager, key);
    assert_eq!(
        pinned_now,
        Some(1),
        "the pin itself must move off the reserved account, not merely divert one request"
    );
}

/// Pin session `key` to account 0 as of now, then select for it with nothing tried.
fn select_with_a_fresh_pin_on_the_first_account(manager: &Manager, key: u64) -> Option<usize> {
    let now = OffsetDateTime::now_utc();
    seed_pin(manager, key, 0, odt_to_ms(now));
    manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
}

// ---- live group reload (problem 1: group edits appear to do nothing) ------

/// `acct` at priority 0 in group `codereview`, then `other` at priority 5 in no group.
fn codereview_acct_and_other() -> Vec<Account> {
    vec![
        account_in_groups("acct", 0, &["codereview"]),
        account("other", 5),
    ]
}

/// A manager booted from a config file tagged `tag` that holds
/// [`codereview_acct_and_other`] and nothing parked or reserved, and that file's path.
fn acct_and_other_on_disk(tag: &str) -> (PathBuf, Arc<Manager>) {
    let path = tmp_config_path(tag);
    let boot = config_with(codereview_acct_and_other());
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());
    (path, manager)
}

/// A `Config` with one account per `(name, priority, groups)` tuple —
/// `groups = &[]` means no `groups` key, matching [`account`]/
/// [`account_in_groups`]'s own split. Shared setup for every reload test
/// below: called once to build the manager's BOOT config, and called
/// again (saved straight to `path`, never through the manager) to
/// simulate an out-of-band edit — a hand edit, or a sibling `tcr group`
/// invocation — while the manager keeps running.
fn reload_config(accounts: &[(&str, i64, &[&str])]) -> Config {
    let built: Vec<Account> = accounts
        .iter()
        .map(|&(name, priority, groups)| {
            if groups.is_empty() {
                account(name, priority)
            } else {
                account_in_groups(name, priority, groups)
            }
        })
        .collect();
    config_with(built)
}

/// THE test for problem 1: editing a temp config's group membership and
/// touching its mtime makes a SUBSEQUENT snapshot report the new groups —
/// without rebuilding the `Manager`. If only one test in this section
/// survives, this is it.
#[test]
fn snapshot_picks_up_a_group_edit_without_rebuilding_the_manager() {
    let path = tmp_config_path("reload-feature");
    let boot = reload_config(&[("acct", 0, &["dev"])]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(before.accounts[0].groups, vec!["dev".to_string()]);

    // A fast filesystem can produce two writes with the SAME mtime at
    // whole-second resolution; sleep past that so the edit is actually
    // observable via mtime, the same concern any mtime-triggered reload
    // design has.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let edited = reload_config(&[("acct", 0, &["ops", "burst"])]);
    config::save(&path, &edited).expect("write edited reload config");

    let after = manager.snapshot(OffsetDateTime::now_utc());
    let mut groups = after.accounts[0].groups.clone();
    groups.sort();
    assert_eq!(
        groups,
        vec!["burst".to_string(), "ops".to_string()],
        "a snapshot after the file changed must report the NEW groups, without \
         rebuilding the Manager"
    );

    std::fs::remove_file(&path).ok();
}

/// A reload does NOT disturb accounts, credentials or tokens — only
/// `groups`/`groupSettings` move. Rewrites the file with the SAME account
/// identity (name match), a DIFFERENT access token, AND a group change in
/// one write, then asserts the in-memory access token is untouched while
/// the group DID move — proving the scope, not just the happy path.
#[test]
fn reload_never_touches_accounts_credentials_or_tokens() {
    let path = tmp_config_path("reload-scope");
    let boot = reload_config(&[("acct", 0, &["dev"])]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    let original_token = {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        accounts[0].access_token.clone()
    };

    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut tampered = reload_config(&[("acct", 0, &["ops"])]);
    tampered.accounts[0].access_token = "at-DIFFERENT-token-must-not-apply".to_string();
    config::save(&path, &tampered).expect("write tampered reload config");

    manager.reload_groups_if_changed();

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(
        accounts[0].access_token, original_token,
        "reload must never touch the access token — only group fields"
    );
    assert_eq!(
        accounts[0].groups,
        vec!["ops".to_string()],
        "the group change in the SAME file write must still apply"
    );
    drop(accounts);
    std::fs::remove_file(&path).ok();
}

/// A malformed config at reload time keeps the previous in-memory groups
/// (never blanks the fleet) and does not advance the remembered mtime, so
/// a subsequent FIX self-heals without the file needing to change again
/// after that.
#[test]
fn reload_keeps_current_groups_on_a_malformed_config() {
    let path = tmp_config_path("reload-malformed");
    let boot = reload_config(&[("acct", 0, &["dev"])]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, b"{ this is not valid json").expect("write malformed config");
    manager.reload_groups_if_changed();

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(
        accounts[0].groups,
        vec!["dev".to_string()],
        "a malformed config at reload time must keep the previous groups, not blank them"
    );
    drop(accounts);

    // Self-heal: fixing the file (mtime necessarily advances again, since
    // this write follows the malformed one) makes the NEXT reload apply.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let fixed = reload_config(&[("acct", 0, &["fixed"])]);
    config::save(&path, &fixed).expect("write fixed reload config");
    manager.reload_groups_if_changed();
    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(accounts[0].groups, vec!["fixed".to_string()]);
    drop(accounts);
    std::fs::remove_file(&path).ok();
}

/// APP-35: an account APPENDED to the file after boot must be admitted by
/// the same reload path that already picks up a group edit — not just
/// counted, but genuinely selectable, which the disabled-control below
/// proves. Never touches or removes the two accounts that were already
/// there (`reload_never_touches_accounts_credentials_or_tokens` already
/// pins that half; this is additive).
#[test]
fn reload_admits_an_account_added_to_the_file_after_boot() {
    let path = tmp_config_path("reload-hot-add");
    let boot = reload_config(&[("acct-a", 0, &[]), ("acct-b", 5, &[])]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());
    assert_eq!(
        manager
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .len(),
        2,
        "control: boots with exactly the two accounts on disk"
    );

    // The operator appends a third account to the file by hand while the
    // proxy runs.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let appended = reload_config(&[("acct-a", 0, &[]), ("acct-b", 5, &[]), ("acct-c", 9, &[])]);
    config::save(&path, &appended).expect("write appended reload config");
    manager.reload_groups_if_changed();

    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts.len(),
            3,
            "the account added to the file must appear in the running manager"
        );
    }

    // Gate the two original accounts out so a successful select can only
    // mean the new one is genuinely selectable, not merely present in a
    // list nothing reads.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
        accounts[1].disabled = true;
    }
    let now = OffsetDateTime::now_utc();
    let idx = manager.select(&HashSet::new(), now, None, None, "/v1/messages", None);
    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(
        idx.and_then(|i| accounts.get(i)).map(|a| a.name.as_str()),
        Some("acct-c"),
        "the newly admitted account must be selectable, not just counted: got {idx:?}"
    );
    drop(accounts);
    std::fs::remove_file(&path).ok();
}

/// A Codex row the file names while the proxy runs is ADMITTED by the reload
/// even when the running fleet holds an Anthropic row of the same name with no
/// uuid, and that Anthropic row keeps its own groups. Provider-blind, the name
/// fallback called the Codex row already known through the Anthropic row, so it
/// was never admitted, and the group carry handed the Codex row's groups to the
/// Anthropic one. The Codex row carries a uuid, as a real one does (the ChatGPT
/// account id): it does not help, because the Anthropic side has none.
///
/// Watched red: drop the provider guard from `identity::same_identity` and the
/// Codex row is never admitted, while the Anthropic row takes its groups.
#[test]
fn reload_admits_a_codex_row_named_like_a_running_anthropic_row() {
    let path = tmp_config_path("reload-codex-beside-anthropic");
    let boot = config_with(vec![account_in_groups(
        "alice@example.com",
        0,
        &["anthropic-team"],
    )]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    std::thread::sleep(std::time::Duration::from_millis(20));
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        ..account_in_groups("alice@example.com", 1, &["codex-team"])
    };
    config::save(&path, &config_with(vec![codex])).expect("write reload config");
    manager.reload_groups_if_changed();

    let rows: Vec<(String, crate::provider::Provider, Vec<String>)> = manager
        .accounts
        .read()
        .expect("accounts lock poisoned")
        .iter()
        .map(|a| (a.name.clone(), a.provider, a.groups.clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "alice@example.com".to_string(),
                crate::provider::Provider::Anthropic,
                vec!["anthropic-team".to_string()]
            ),
            (
                "alice@example.com".to_string(),
                crate::provider::Provider::Codex,
                vec!["codex-team".to_string()]
            ),
        ],
        "the Codex row is admitted beside the Anthropic row, each with its own groups"
    );
    std::fs::remove_file(&path).ok();
}

/// C-4 regression: `Self::account_egress` indexes `self.config.accounts`
/// with a RUNTIME index (`self.accounts`). Before this fix, the APP-35
/// admission arm above (`reload_admits_an_account_added_to_the_file_after_boot`)
/// pushed a newly discovered account onto the runtime vector ONLY, never
/// onto `self.config.accounts`, so after one hot-add, `self.accounts` has
/// N+1 rows and `self.config.accounts` still has N. `account_egress(N)` then
/// reads `config.accounts.get(N)`, which is out of bounds and `None`, so a
/// STRICT pin on the newly admitted account is silently read as no pin at
/// all.
#[test]
fn reload_keeps_account_egress_index_aligned_with_a_hot_added_strict_pin() {
    let path = tmp_config_path("reload-egress-index");
    let pinned_peer = tcr_peer_wire::PeerId([9u8; 32]);
    let boot = config_with(vec![account("acct-a", 0)]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());
    assert_eq!(
        manager
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .len(),
        1,
        "control: boots with exactly the one account on disk"
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    let appended = config_with(vec![
        account("acct-a", 0),
        Account {
            egress: config::Egress::Via(pinned_peer),
            egress_strict: true,
            ..account("acct-b", 5)
        },
    ]);
    config::save(&path, &appended).expect("write appended reload config");
    manager.reload_groups_if_changed();

    let idx = {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts.len(),
            2,
            "the newly admitted account must appear in the runtime rotation"
        );
        accounts
            .iter()
            .position(|a| a.name == "acct-b")
            .expect("acct-b was admitted by reload")
    };

    let pin = manager.account_egress(idx);
    assert_eq!(
        pin,
        Some(config::EgressPin {
            egress: config::Egress::Via(pinned_peer),
            strict: true,
        }),
        "a strict pin on a hot-admitted account must be readable by its runtime \
         index, not silently None (fail open) because self.config.accounts \
         lagged self.accounts: got {pin:?}"
    );

    std::fs::remove_file(&path).ok();
}

/// A session pinned to an account that becomes reserved VIA RELOAD (not
/// construction-time, unlike
/// [`pin_reclaim_when_the_pinned_accounts_group_is_reserved`]) is
/// re-keyed on its next unrequested `select`.
#[test]
fn reload_re_keys_a_pin_when_its_group_becomes_reserved() {
    let (path, manager) = acct_and_other_on_disk("reload-pin-reclaim");

    let now = OffsetDateTime::now_utc();
    let key = 9191u64;
    seed_pin(&manager, key, 0, odt_to_ms(now));

    // Reserve 'codereview' via a fresh file write — no construction-time
    // reservation at all, unlike the sibling test above.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let reserved_config = config_with_reserved(codereview_acct_and_other(), &["codereview"]);
    config::save(&path, &reserved_config).expect("write reserved reload config");

    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the plain account is still eligible");
    assert_eq!(
        served, 1,
        "a group reserved via RELOAD must not keep serving an unrequested pinned session"
    );
    let pinned_now = pin_of(&manager, key);
    assert_eq!(
        pinned_now,
        Some(1),
        "the pin must move off the newly-reserved account, not merely divert one request"
    );

    std::fs::remove_file(&path).ok();
}

/// Same live-reload cadence as [`reload_re_keys_a_pin_when_its_group_becomes_reserved`],
/// applied to `allowControlAccount`: a control-only group cannot serve its
/// own `--group` ask at boot, and a config write that opts it in — with NO
/// restart — makes the very same request land on the control account on
/// its next call.
#[test]
fn reload_picks_up_a_group_opting_in_to_the_control_account() {
    let path = tmp_config_path("reload-control-allowed");
    let boot = config_with_control(
        vec![
            account_in_groups("control-acct", 0, &["research"]),
            account("other", 5),
        ],
        "control-acct",
    );
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("research"),
        ),
        None,
        "not opted in at boot: strict by default, refuses rather than falling back to the pool"
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut opted_in = config_with_control(
        vec![
            account_in_groups("control-acct", 0, &["research"]),
            account("other", 5),
        ],
        "control-acct",
    );
    opted_in.group_settings.insert(
        "research".to_string(),
        config::GroupSettings {
            reserved: false,
            parked: false,
            allow_control_account: true,
            spill_to_pool: false,
            color: None,
            extra: serde_json::Map::new(),
        },
    );
    config::save(&path, &opted_in).expect("write opted-in reload config");

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("research"),
        ),
        Some(0),
        "a live opt-in write must be picked up with no restart"
    );

    std::fs::remove_file(&path).ok();
}

// ---- hard account lock (`lockAccount`; no failover) -----------------------

fn config_with_lock(accounts: Vec<Account>, lock: &str) -> Config {
    let mut config = config_with(accounts);
    config.lock_account = Some(lock.to_string());
    config
}

fn config_with_control(accounts: Vec<Account>, control: &str) -> Config {
    let mut config = config_with(accounts);
    config.control_account = Some(config::ControlAccountRef::Name(control.to_string()));
    config
}

/// A hard lock pins EVERY select to the locked index, ignoring the LRU
/// preference and any session-affinity key that points elsewhere.
#[test]
fn select_lock_always_returns_locked_idx() {
    let manager = build_manager(
        config_with_lock(
            vec![account("zero", 0), account("one", 0), account("two", 0)],
            "one",
        ),
        counting_refresher(),
    );
    assert_eq!(manager.locked_idx, Some(1));
    let now = OffsetDateTime::now_utc();
    // No affinity, empty tried → locked account regardless of LRU (index 0
    // would be the natural first pick here).
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
    // An affinity key returns the SAME locked account (lock ignores affinity).
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(42), "/v1/messages", None),
        Some(1)
    );
    // Bias the LRU toward index 0 by pinning affinity elsewhere first — lock
    // still wins.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(7), "/v1/messages", None),
        Some(1)
    );
}

/// The lock has NO failover: once the locked account is in `tried`, select
/// returns None rather than rotating to the pool.
#[test]
fn select_lock_returns_none_when_locked_tried() {
    let manager = build_manager(
        config_with_lock(vec![account("zero", 0), account("one", 0)], "one"),
        counting_refresher(),
    );
    assert_eq!(manager.locked_idx, Some(1));
    let now = OffsetDateTime::now_utc();
    let mut tried = HashSet::new();
    tried.insert(1usize);
    assert_eq!(
        manager.select(&tried, now, None, None, "/v1/messages", None),
        None
    );
}

/// `assemble` resolves the configured name to its account index; a name that
/// matches no account resolves to None (runs unlocked).
#[test]
fn assemble_resolves_lock_name() {
    let matched = build_manager(
        config_with_lock(vec![account("a", 0), account("b", 0), account("c", 0)], "c"),
        counting_refresher(),
    );
    assert_eq!(matched.locked_idx, Some(2));

    let unmatched = build_manager(
        config_with_lock(vec![account("a", 0), account("b", 0)], "ghost"),
        counting_refresher(),
    );
    assert_eq!(unmatched.locked_idx, None);
}

/// Absent `lockAccount` → `locked_idx == None` → select is unchanged.
#[test]
fn unlocked_default_leaves_locked_idx_none() {
    let manager = manager_of(vec![account("solo", 0)]);
    assert_eq!(manager.locked_idx, None);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0)
    );
}

/// `assemble` resolves the configured `controlAccount` name to its account
/// index, mirroring `assemble_resolves_lock_name`; a name that matches no
/// account resolves to `None` (runs with no control account, not a panic).
#[test]
fn assemble_resolves_control_name() {
    let matched = build_manager(
        config_with_control(vec![account("a", 0), account("b", 0), account("c", 0)], "c"),
        counting_refresher(),
    );
    assert_eq!(matched.control(), Some(2));

    let unmatched = build_manager(
        config_with_control(vec![account("a", 0), account("b", 0)], "ghost"),
        counting_refresher(),
    );
    assert_eq!(unmatched.control(), None);
}

/// A `controlAccount` naming a Codex account is warned about and ignored:
/// the control account is an Anthropic identity (its bootstrap calls, its
/// probe, its `controlIdentity` check), and Codex has no counterpart in this
/// version. The same name resolves when the row is Anthropic,
/// so the `None` below is the provider and not a name that never matched.
///
/// Watched red: drop the provider filter after `assemble`'s control
/// resolution and the Codex row resolves (`Some(1)`).
#[test]
fn assemble_ignores_a_codex_control_account() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com", 0)
    };
    let manager = build_manager(
        config_with_control(
            vec![account("alice@example.com", 0), codex],
            "bob@example.com",
        ),
        counting_refresher(),
    );
    assert_eq!(
        manager.control(),
        None,
        "a Codex control account must be ignored at boot"
    );

    let anthropic = build_manager(
        config_with_control(
            vec![
                account("alice@example.com", 0),
                account("bob@example.com", 0),
            ],
            "bob@example.com",
        ),
        counting_refresher(),
    );
    assert_eq!(
        anthropic.control(),
        Some(1),
        "the control: an Anthropic row resolves"
    );
}

#[test]
fn upstream_for_routes_each_provider_to_its_own_host() {
    // Built the way the other tests here build a manager, from a Config whose `upstream` and
    // `codex_upstream` name two different hosts.
    let mut config = config_with(vec![account("alice@example.com", 0)]);
    config.upstream = "http://anthropic.test".to_string();
    config.codex_upstream = "http://codex.test".to_string();
    let manager = build_manager(config, counting_refresher());
    assert_eq!(
        manager.upstream_for(crate::provider::Provider::Anthropic),
        "http://anthropic.test"
    );
    assert_eq!(
        manager.upstream_for(crate::provider::Provider::Codex),
        "http://codex.test"
    );
    assert_eq!(
        manager.upstream(),
        "http://anthropic.test",
        "the Anthropic-only accessor is unchanged"
    );
}

/// `Manager::provider` reads each runtime row's own provider, which
/// `AccountRuntime::from_config` copied from the config row, and `None`
/// only past the end.
///
/// Watched red: have the accessor answer the default for every row and the
/// Codex row reads as Anthropic.
#[test]
fn provider_reads_each_rows_own_provider() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com", 0)
    };
    let manager = manager_of(vec![account("alice@example.com", 0), codex]);
    assert_eq!(
        manager.provider(0),
        Some(crate::provider::Provider::Anthropic)
    );
    assert_eq!(manager.provider(1), Some(crate::provider::Provider::Codex));
    assert_eq!(manager.provider(2), None, "an index past the end");
}

/// The exact bug this unit fixes: a legacy bare-string `controlAccount`
/// that names a DUPLICATED email must not silently bind to whichever row
/// sorts first (the old `position(...)` behaviour) — it must refuse and run
/// with no control account, same as an unmatched name.
#[test]
fn assemble_refuses_legacy_control_name_on_duplicated_email() {
    let dup_a = Account {
        org_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
        ..account("dup@example.com", 0)
    };
    let dup_b = Account {
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        disabled: Some(true),
        ..account("dup@example.com", 0)
    };
    let config = config_with_control(vec![dup_a, dup_b], "dup@example.com");
    let manager = build_manager(config, counting_refresher());
    assert_eq!(
        manager.control(),
        None,
        "an ambiguous legacy name must run with NO control account, never a guess"
    );
}

/// A `controlAccount` written by an OLDER tcr — the identity object, with
/// its org fields — still resolves at boot, by its `name` alone. Nothing
/// writes that shape now, and a config carrying one must still come up on
/// the right row rather than with no control account at all.
///
/// The two rows have distinct names here because `config::load` guarantees
/// that; the object's `orgUuid` is the field this deliberately no longer
/// reads, and the row it names is the one that must win.
#[test]
fn assemble_resolves_a_legacy_identity_control_account_by_name() {
    let personal = account("dup@example.com", 0);
    let team = Account {
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        ..account("dup@example.com/team", 0)
    };
    let mut config = config_with(vec![personal, team]);
    config.control_account = Some(config::ControlAccountRef::Identity {
        name: "dup@example.com/team".to_string(),
        account_uuid: None,
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        org_name: None,
    });
    let manager = build_manager(config, counting_refresher());
    assert_eq!(
        manager.control(),
        Some(1),
        "the object's name must pick the second row, not the first"
    );
}

/// Full round trip through the durable half, on the fleet shape that used
/// to make this impossible: one person, two orgs, one email. The loader
/// gives the second row its own name; setting control by that name persists
/// the NAME (nothing else is needed — a name is unique), and a fresh
/// `Manager` built from the reloaded file resolves to the SAME row, not
/// just "a" row.
///
/// The duplicated-email file is written first and read back through
/// `config::load` deliberately: the migration is the step under test as much
/// as the control write is.
#[test]
fn set_control_on_a_migrated_duplicate_survives_a_reload() {
    let path = tmp_config_path("control-survives-reload");
    let dup_a = Account {
        account_uuid: Some("33333333-3333-3333-3333-333333333333".to_string()),
        org_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
        refresh_token: Some("rt-first".to_string()),
        ..account("dup@example.com", 0)
    };
    let dup_b = Account {
        account_uuid: Some("33333333-3333-3333-3333-333333333333".to_string()),
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        refresh_token: Some("rt-second".to_string()),
        ..account("dup@example.com", 0)
    };
    config::save(&path, &config_with(vec![dup_a, dup_b])).expect("write test config");

    // The loader renames the second row — no org name on file, so the name
    // falls back to the first eight characters of its org uuid.
    let migrated = config::load(&path).expect("load migrates the duplicate");
    assert_eq!(migrated.accounts[0].name, "dup@example.com");
    assert_eq!(migrated.accounts[1].name, "dup@example.com/22222222");
    let manager = build_manager_with_path(migrated, path.clone());

    let outcome = manager.set_control_by_query(Some("dup@example.com/22222222"));
    assert_eq!(
        outcome,
        SetControlOutcome::Applied {
            name: Some("dup@example.com/22222222".to_string()),
            persist: ControlPersist::Persisted,
        }
    );
    assert_eq!(manager.control(), Some(1), "resolved the SECOND row live");

    // The bare name is the whole key on disk now.
    let on_disk = config::load(&path).expect("reload persisted config");
    assert_eq!(
        on_disk.control_account,
        Some(config::ControlAccountRef::Name(
            "dup@example.com/22222222".to_string()
        ))
    );
    assert!(
        on_disk.renamed_accounts.is_empty(),
        "the second load has nothing left to rename"
    );

    // A brand-new Manager built from the reloaded file resolves to the
    // SAME row (index 1, `rt-second`) — not just "a" row.
    let reloaded_manager = build_manager_with_path(on_disk, path.clone());
    assert_eq!(reloaded_manager.control(), Some(1));

    std::fs::remove_file(&path).ok();
}

/// Absent `controlAccount` → `control() == None`.
#[test]
fn control_absent_default_leaves_control_none() {
    let manager = manager_of(vec![account("solo", 0)]);
    assert_eq!(manager.control(), None);
}

/// PART 1's invariant, NARROWED by part 2 (deliberately — see the module
/// doc for the three-way split): setting a control account must not change
/// which account `select` returns **for inference traffic on the realistic
/// deployment shape, where the control account stays `disabled`** (the
/// documented default — see `Manager::control_idx`'s doc). Part 1 asserted
/// this unconditionally, because part 1 shipped no routing at all and the
/// control account was inert everywhere.
///
/// That is no longer strictly true in general: part 2 permanently excludes
/// the control account from inference candidacy REGARDLESS of `disabled`
/// (see [`inference_never_goes_to_control_even_when_unpinned`], which
/// deliberately leaves control ENABLED and asserts inference still skips
/// it) — so on a tiny fleet where the enabled control account would
/// otherwise have been the natural LRU pick, inference selection DOES
/// change the moment `disabled` is lifted. This test is narrowed to the
/// disabled case (`set_disabled(0, true)` below) where the exclusion is
/// provably redundant with `eligible`'s own disabled check, so the
/// original "byte-identical" claim keeps holding exactly where it matters
/// operationally. [`control_preference_prefers_control_for_non_inference_paths`]
/// covers the (expected) non-inference side changing.
#[test]
fn setting_a_control_account_does_not_change_selection() {
    let without_control = manager_of(vec![account("a", 0), account("b", 0)]);
    let with_control = build_manager(
        config_with_control(vec![account("a", 0), account("b", 0)], "a"),
        counting_refresher(),
    );
    // Realistic deployment shape: the control account stays `disabled` —
    // see the doc-comment above for why this is what keeps the invariant
    // honestly true rather than coincidentally true. Disabled on BOTH
    // managers, isolating the effect of `controlAccount` itself: the
    // baseline already excludes account 0 the ordinary way (a plain
    // `disabled` account, unrelated to control), so any DIFFERENCE
    // between the two would be attributable to the control preference,
    // not to `disabled` doing double duty.
    without_control.set_disabled(0, true);
    with_control.set_disabled(0, true);
    assert_eq!(with_control.control(), Some(0));

    let now = OffsetDateTime::now_utc();
    for _ in 0..20 {
        let picked_without =
            without_control.select(&HashSet::new(), now, None, None, "/v1/messages", None);
        let picked_with =
            with_control.select(&HashSet::new(), now, None, None, "/v1/messages", None);
        assert_eq!(
            picked_without, picked_with,
            "a control account must not change which account select() returns for inference traffic"
        );
        if let Some(idx) = picked_without {
            without_control.set_current(idx);
            with_control.set_current(idx);
        }
    }
}

/// The other half of the narrowing above: an UNPINNED, non-inference,
/// non-noise request (the identity/control plane) DOES change the pick —
/// it now prefers the control account, byte-different from `without_control`.
#[test]
fn control_preference_prefers_control_for_non_inference_paths() {
    let without_control = manager_of(vec![account("a", 0), account("b", 0)]);
    let with_control = build_manager(
        config_with_control(vec![account("a", 0), account("b", 0)], "b"),
        counting_refresher(),
    );
    assert_eq!(with_control.control(), Some(1));

    let now = OffsetDateTime::now_utc();
    let picked_without =
        without_control.select(&HashSet::new(), now, None, None, "/api/organizations", None);
    let picked_with =
        with_control.select(&HashSet::new(), now, None, None, "/api/organizations", None);
    assert_eq!(
        picked_without,
        Some(0),
        "no control account: ordinary LRU picks index 0"
    );
    assert_eq!(
        picked_with,
        Some(1),
        "a control account IS preferred for an unpinned identity-plane request"
    );
}

// ---- control-account routing (part 2 — see docs/plans/control-routing-bridge-coder.md) ----

/// Invariant 1, the one that matters: an EXISTING pin is never re-keyed by
/// the control preference, even after control is set at runtime.
#[test]
fn control_preference_never_moves_an_existing_pin() {
    let manager = manager_of(vec![account("pool", 0), account("ctrl", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 555u64;
    // Pin session `key` to account 0 (pool) BEFORE any control account
    // exists — an ordinary identity-plane select, no preference in play.
    let pinned = manager
        .select(
            &HashSet::new(),
            now,
            None,
            Some(key),
            "/api/organizations",
            None,
        )
        .expect("an account is eligible");
    assert_eq!(pinned, 0);

    // Set the control account to `ctrl` (index 1) at runtime.
    manager.set_control_by_query(Some("ctrl"));
    assert_eq!(manager.control(), Some(1));

    // The SAME session's next identity-plane request must still return its
    // EXISTING pin (0), never the newly-preferred control account (1).
    for _ in 0..5 {
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                Some(key),
                "/api/organizations",
                None
            ),
            Some(0),
            "an existing pin must never be re-keyed by the control preference"
        );
    }
}

/// The `keep_pin` half of invariant 1, distinct from the test above: a pin
/// that is only DIVERTED for one request (it already failed upstream —
/// `tried` — but the ACCOUNT itself is still alive) must not be re-keyed to
/// the control account either. The test above never actually reaches the
/// control overlay (a clean, servable pin returns from the affinity
/// fast-path first); this one forces a divert so `keep_pin` really is
/// `Some(_)` when the overlay's `keep_pin.is_none()` gate is checked.
#[test]
fn control_preference_never_moves_a_diverted_pin() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 777u64;
    let pinned = manager
        .select(
            &HashSet::new(),
            now,
            None,
            Some(key),
            "/api/organizations",
            None,
        )
        .expect("an account is eligible");
    assert_eq!(pinned, 0, "the first pick, tie-broken by index, is \"a\"");

    // Set control to "c" (index 2) — a DIFFERENT account — after the pin
    // already exists.
    manager.set_control_by_query(Some("c"));
    assert_eq!(manager.control(), Some(2));

    // Bias ordinary LRU firmly toward "b" (index 1): stamp "c" (control)
    // as ALREADY selected, so its LRU key sorts strictly AFTER "b"'s
    // never-selected key. If the divert incorrectly routed straight to
    // control (the hoisting bug this test exists to catch — the overlay
    // reached ahead of the `keep_pin` gate), it would land on "c"
    // regardless of this bias, because it never consults ordinary LRU at
    // all for a control-preferred pick.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[2].last_selected_seq = 999;
    }

    // This ONE request diverts — the pinned account already failed it.
    let mut tried = HashSet::new();
    tried.insert(pinned);
    let served = manager
        .select(&tried, now, None, Some(key), "/api/organizations", None)
        .expect("a divert target is eligible");
    assert_eq!(
        served, 1,
        "a diverted pinned request must go through ORDINARY rotation (picking \"b\"), \
         not straight to the control account"
    );

    // A clean retry must still return the ORIGINAL pin, never re-keyed to
    // the control account by the divert.
    let rechecked = manager
        .select(
            &HashSet::new(),
            now,
            None,
            Some(key),
            "/api/organizations",
            None,
        )
        .expect("an account is eligible");
    assert_eq!(
        rechecked, pinned,
        "a per-request divert must never re-key the session's pin to the control account"
    );
}

/// §1 no-op guard: the control account ships `disabled` (out of the
/// inference rotation, by design — see the module doc), and the control
/// preference must BYPASS that gate. If `select` used `eligible` or
/// `account_hard_ok` here instead of `Self::control_eligible`, this test
/// fails silently useful: it would still compile and the feature would
/// simply never fire (the single most likely way to get this wrong).
#[test]
fn control_is_picked_even_though_disabled() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    assert_eq!(manager.control(), Some(1));
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(1),
        "a disabled control account must still be picked for an identity-plane request"
    );
}

#[test]
fn control_is_not_picked_when_errored() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].status = AccountStatus::Error;
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "an ERRORED control account must degrade to normal rotation"
    );
}

#[test]
fn control_is_not_picked_when_rejected() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].quota.status = Some("rejected".to_string());
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "a REJECTED control account must degrade to normal rotation"
    );
}

#[test]
fn control_is_not_picked_when_rate_limited() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].rate_limited_until_ms = Some(now_ms + 60_000);
    }
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "a LIVE-held control account must degrade to normal rotation"
    );
}

/// Inference must NEVER select the control account — including UNPINNED
/// and with control left ENABLED (pooled), where `eligible` alone would
/// happily pick it. Only the dedicated pool-pick exclusion stops it.
#[test]
fn inference_never_goes_to_control_even_when_unpinned() {
    let manager = build_manager(
        config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl"),
        counting_refresher(),
    );
    assert_eq!(manager.control(), Some(0));
    let now = OffsetDateTime::now_utc();
    for _ in 0..10 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1),
            "inference must route to the pool, never the control account"
        );
    }
}

/// `controlPooled` lifts the exclusion the test above pins down: the same
/// unpinned inference now reaches the control account, because
/// `select_with_group` stops force-adding the control index to `tried`.
/// Asserts REACHABILITY, not a fixed index — LRU decides the order, and
/// pinning the order here would be testing the tiebreak, not the flag.
#[test]
fn pooled_control_account_takes_inference() {
    let mut config = config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl");
    config.control_pooled = true;
    let manager = build_manager(config, counting_refresher());
    assert_eq!(manager.control(), Some(0));
    let now = OffsetDateTime::now_utc();
    let reached_control = (0..10)
        .any(|_| manager.select(&HashSet::new(), now, None, None, "/v1/messages", None) == Some(0));
    assert!(
        reached_control,
        "controlPooled must let an inference pick reach the control account"
    );
}

/// The guard the whole opt-in rests on, and the reason `controlPooled` is
/// not simply "delete the exclusion": a POOLED control account is held out
/// at `switchThreshold - controlReserve` (0.90 - 0.05 = 0.85), NOT at the
/// full threshold every other account gets.
///
/// 0.86 is chosen to sit BELOW the full 0.90 on purpose — `eligible` still
/// passes this account, so the only thing that can hold it back is
/// [`super::select::Manager::pool_pick_respects_control_reserve`]. Raise it
/// to 0.90 and the test would pass for the wrong reason.
#[test]
fn pooled_control_account_is_held_out_by_the_reserve() {
    let mut config = config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl");
    config.control_pooled = true;
    let manager = build_manager(config, counting_refresher());
    let now = OffsetDateTime::now_utc();
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].quota.five_hour = Some(crate::quota::QuotaWindow {
            utilization: 0.86,
            reset: Some(now + Duration::seconds(300)),
        });
    }
    for _ in 0..10 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1),
            "a pooled control account past threshold-reserve must be held out, \
             leaving the identity plane the headroom the reserve exists to keep"
        );
    }
}

/// §2: a NOISE request reuses whichever account this connection already
/// served.
#[test]
fn noise_path_follows_its_connection() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let now = OffsetDateTime::now_utc();
    let conn = 42u64;
    let first = manager
        .select(
            &HashSet::new(),
            now,
            None,
            None,
            "/api/event_logging/v2/batch",
            Some(conn),
        )
        .expect("an account is eligible");
    for _ in 0..5 {
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                None,
                "/api/event_logging/v2/batch",
                Some(conn)
            ),
            Some(first),
            "noise traffic on the same connection must stay on the same account"
        );
    }
}

/// §2 fallback: once the connection's account is no longer eligible,
/// normal rotation takes over AND `conn_affinity` is re-recorded to the
/// new winner (not left stale on the held account).
#[test]
fn noise_path_falls_back_when_its_account_is_held() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let conn = 7u64;
    let first = manager
        .select(
            &HashSet::new(),
            now,
            None,
            None,
            "/mcp-registry/list",
            Some(conn),
        )
        .expect("an account is eligible");
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[first].rate_limited_until_ms = Some(now_ms + 60_000);
    }
    let second = manager
        .select(
            &HashSet::new(),
            now,
            None,
            None,
            "/mcp-registry/list",
            Some(conn),
        )
        .expect("the other account is still eligible");
    assert_ne!(
        second, first,
        "a held connection-pinned account must fall back to rotation"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/mcp-registry/list",
            Some(conn)
        ),
        Some(second),
        "conn_affinity must be RE-RECORDED to the new winner, not left stale"
    );
}

/// Proves the split splits: noise and identity traffic on the SAME
/// connection route independently — a noise pin to the pool account must
/// not short-circuit the identity plane's control preference.
#[test]
fn identity_path_still_goes_to_control_on_the_same_connection() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    let now = OffsetDateTime::now_utc();
    let conn = 99u64;
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/api/event_logging/v2/batch",
            Some(conn)
        ),
        Some(0),
        "noise pins this connection to the pool account"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/api/organizations",
            Some(conn)
        ),
        Some(1),
        "control preference must not be short-circuited by the connection's noise pin"
    );
}

/// `conn_affinity` is a SEPARATE, memory-only map (invariant 3): noise
/// traffic must never mark the persisted affinity map dirty or write into
/// it, mirroring `Self::affinity`'s own persistence contract.
#[test]
fn conn_affinity_is_never_persisted() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    assert!(!manager.affinity_file.is_dirty());
    let now = OffsetDateTime::now_utc();
    for i in 0..5u64 {
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/api/event_logging/v2/batch",
            Some(i),
        );
    }
    assert!(
        !manager.affinity_file.is_dirty(),
        "noise traffic must never mark the persisted affinity map dirty"
    );
    assert!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .is_empty(),
        "noise traffic must never write into the persisted affinity map"
    );
}

/// Invariant 4: inert with no control account set — path classification
/// changes nothing about ordinary LRU.
#[test]
fn control_preference_is_inert_when_unset() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    assert_eq!(manager.control(), None);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "no control account set -> ordinary LRU, unaffected by path classification"
    );
}

/// The hard account lock still short-circuits BEFORE path classification
/// is even computed — control preference can never outrank it.
#[test]
fn control_preference_never_outranks_lock_account() {
    let mut config = config_with_control(vec![account("locked", 0), account("ctrl", 0)], "ctrl");
    config.lock_account = Some("locked".to_string());
    let manager = build_manager(config, counting_refresher());
    assert_eq!(manager.locked_idx, Some(0));
    assert_eq!(manager.control(), Some(1));
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "the hard lock must win over control preference regardless of path"
    );
}

/// `set_control_by_query` resolves against the LIVE rotation (mirroring
/// `set_disabled_by_query`), reports the resolved name, sets `control()`,
/// and a `None` query clears it.
#[test]
fn set_control_by_query_resolves_sets_and_clears() {
    let manager = manager_of(vec![account("gil", 0), account("other", 0)]);

    let outcome = manager.set_control_by_query(Some("gil"));
    assert_eq!(
        outcome,
        SetControlOutcome::Applied {
            name: Some("gil".to_string()),
            persist: ControlPersist::NoConfigFile,
        }
    );
    assert_eq!(manager.control(), Some(0));
    assert_eq!(manager.control_name(), Some("gil".to_string()));

    let cleared = manager.set_control_by_query(None);
    assert_eq!(
        cleared,
        SetControlOutcome::Applied {
            name: None,
            persist: ControlPersist::NoConfigFile,
        }
    );
    assert_eq!(manager.control(), None);
}

/// `tcr control` naming a Codex account is refused at runtime, as boot ignores one
/// (`assemble_ignores_a_codex_control_account`): nothing is set, so nothing is
/// persisted, and a control account already in place stays. The Anthropic row of
/// the same fleet is set, so the refusal is the provider's and not a name that
/// never matched.
///
/// Watched red: drop the provider check from `set_control_by_query` and the Codex
/// row is applied.
#[test]
fn set_control_by_query_refuses_a_codex_account() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com/codex", 0)
    };
    let manager = manager_of(vec![account("alice@example.com", 0), codex]);
    let refused = SetControlOutcome::CodexAccount("bob@example.com/codex".to_string());
    assert_eq!(
        manager.set_control_by_query(Some("bob@example.com/codex")),
        refused
    );
    assert_eq!(manager.control(), None, "nothing is set");

    manager.set_control_by_query(Some("alice@example.com"));
    assert_eq!(
        manager.control(),
        Some(0),
        "control: the Anthropic row is set"
    );
    assert_eq!(
        manager.set_control_by_query(Some("bob@example.com/codex")),
        refused
    );
    assert_eq!(
        manager.control(),
        Some(0),
        "the control account already in place stays"
    );
}

#[test]
fn set_control_by_query_no_match_and_ambiguous() {
    let manager = manager_of(vec![account("dup", 0), account("dup", 0)]);
    assert_eq!(
        manager.set_control_by_query(Some("ghost")),
        SetControlOutcome::NoMatch
    );
    assert_eq!(
        manager.set_control_by_query(Some("dup")),
        SetControlOutcome::Ambiguous(vec!["dup".to_string(), "dup".to_string()])
    );
    assert_eq!(
        manager.control(),
        None,
        "an ambiguous query must not set anything"
    );
}

#[test]
fn account_uuid_accessor_returns_configured_value() {
    let mut with_uuid = account("has-uuid", 0);
    with_uuid.account_uuid = Some("abcdef01-2345-6789-abcd-ef0123456789".to_string());
    let without_uuid = account("no-uuid", 0);
    let manager = manager_of(vec![with_uuid, without_uuid]);

    assert_eq!(
        manager.account_uuid(0).as_deref(),
        Some("abcdef01-2345-6789-abcd-ef0123456789"),
        "configured account_uuid is returned"
    );
    assert_eq!(manager.account_uuid(1), None, "absent config → None");
    assert_eq!(manager.account_uuid(99), None, "out-of-range idx → None");
}

// ---- per-account request pacing (default-OFF; soft; RAII in-flight) --------

/// #1 Default-OFF is inert: an account that WOULD be paced (saturated
/// `in_flight` + just served) is still selected exactly as before, so an
/// unconfigured proxy is byte-identical to the pre-pacing build.
#[test]
fn pacing_off_is_inert() {
    let manager = manager_of(vec![account("solo", 0)]);
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].in_flight = 5; // would exceed any cap …
        a[0].last_served_ms = crate::now_ms(); // … and be inside any spacing window.
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0),
        "with PacingConfig::default() the account is selected regardless of load"
    );
}

/// #2 An account at the in-flight cap is skipped and a second, un-capped
/// account is chosen instead.
#[test]
fn in_flight_cap_makes_account_ineligible() {
    let pacing = PacingConfig {
        max_in_flight_per_account: Some(1),
        min_spacing_ms: None,
    };
    let manager = build_manager(
        config_with_pacing(vec![account("busy", 0), account("free", 0)], pacing),
        counting_refresher(),
    );
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].in_flight = 1; // at cap=1 → ineligible under pacing.
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "the capped account is skipped and the free one is chosen"
    );
}

/// #3 An account selected less than `min_spacing_ms` ago is skipped; once the
/// window elapses it is eligible again. Tested at `eligible()` so the soft
/// `select()` fallback can't mask the gate.
#[test]
fn min_spacing_skips_recently_served() {
    let pacing = PacingConfig {
        max_in_flight_per_account: None,
        min_spacing_ms: Some(1000),
    };
    let manager = build_manager(
        config_with_pacing(vec![account("a", 0)], pacing.clone()),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].last_served_ms = now_ms; // served 0ms ago.
    }
    let a = manager.accounts.read().expect("accounts lock poisoned");
    assert!(
        !Manager::eligible(
            &a[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            &pacing,
            true,
            now,
            now_ms,
            false,
            crate::provider::Provider::Anthropic,
            None,
            None,
            &HashSet::new(),
            &HashSet::new(),
        ),
        "served 0ms ago (< 1000ms) → skipped"
    );
    let later_ms = now_ms + 1000;
    let later = ms_to_odt(later_ms).expect("valid timestamp");
    assert!(
        Manager::eligible(
            &a[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            &pacing,
            true,
            later,
            later_ms,
            false,
            crate::provider::Provider::Anthropic,
            None,
            None,
            &HashSet::new(),
            &HashSet::new(),
        ),
        "after the spacing window → eligible again"
    );
}

/// #4 SOFT invariant: when EVERY account is paced out but otherwise healthy,
/// `select()` still returns Some (the least-loaded) — pacing never turns a
/// servable request into a failure.
#[test]
fn all_paced_falls_back_to_serve() {
    let pacing = PacingConfig {
        max_in_flight_per_account: Some(1),
        min_spacing_ms: None,
    };
    let manager = build_manager(
        config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
        counting_refresher(),
    );
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].in_flight = 1;
        a[1].in_flight = 1; // both over cap=1.
    }
    let now = OffsetDateTime::now_utc();
    assert!(
        manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
            .is_some(),
        "all-paced but healthy → soft fallback serves least-loaded, never None"
    );
}

/// #5 The RAII in-flight guard decrements the count on Drop, so no serve-path
/// exit can leak a counter and strand an account.
#[test]
fn in_flight_guard_decrements_on_drop() {
    let manager = manager_of(vec![account("a", 0)]);
    {
        let _guard = manager.enter_in_flight(0);
        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
            1,
            "entering bumps the in-flight count"
        );
    }
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
        0,
        "dropping the guard decrements back to 0"
    );
}

/// An account's upstream client is RETIRED once it has carried
/// [`MAX_SERVES_PER_CONNECTION`] sends, so its pooled h2 connection is
/// replaced before Anthropic's edge drains it (the reset distribution behind
/// that number is on the constant).
///
/// The control half matters as much as the assertion: one send short of the
/// budget the client must be the SAME `Arc`. Without it this test would pass
/// against a build that recycled on EVERY send — which would throw away the
/// warm pool on every request, the exact thing `AccountRuntime::http`'s
/// doc-comment forbids.
#[test]
fn the_upstream_client_is_recycled_once_its_serve_budget_is_spent() {
    let manager = manager_of(vec![account("a", 0)]);
    let original = manager.http_client(0).expect("account 0 exists");

    for _ in 0..(MAX_SERVES_PER_CONNECTION - 1) {
        drop(manager.enter_in_flight(0));
    }
    assert!(
        Arc::ptr_eq(
            &original,
            &manager.http_client(0).expect("account 0 exists")
        ),
        "the pool must survive right up to the budget — rebuilding earlier pays a \
         TCP+TLS handshake the keep-alive settings exist to avoid"
    );

    drop(manager.enter_in_flight(0));
    assert!(
        !Arc::ptr_eq(
            &original,
            &manager.http_client(0).expect("account 0 exists")
        ),
        "crossing the serve budget must hand out a NEW client, so the next send \
         opens a fresh h2 connection instead of one about to be drained"
    );
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0].serves_since_client_build,
        0,
        "the recycle resets the age counter, so the next budget starts clean"
    );
}

/// After the machine slept, the first send retires EVERY account's client,
/// not only the sending one: every pooled connection outlived the sleep
/// with idle timers that stood still (see `crate::wake`).
///
/// The control half: sends with no sleep between them keep the SAME `Arc`,
/// so this cannot pass against a build that rebuilt on every send; and a
/// second send after the same sleep keeps the client the first one built.
#[test]
fn every_upstream_client_is_retired_on_the_first_send_after_a_sleep() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let clients = |m: &Manager| -> Vec<Arc<reqwest::Client>> {
        (0..3)
            .map(|i| m.http_client(i).expect("account exists"))
            .collect()
    };
    let original = clients(&manager);

    drop(manager.enter_in_flight(0));
    let awake = clients(&manager);
    for (i, (before, after)) in original.iter().zip(&awake).enumerate() {
        assert!(
            Arc::ptr_eq(before, after),
            "account {i}: a send with no sleep before it must keep the warm pool"
        );
    }

    manager
        .wake
        .inject_sleep(std::time::Duration::from_mins(15));
    drop(manager.enter_in_flight(0));
    let woken = clients(&manager);
    for (i, (before, after)) in original.iter().zip(&woken).enumerate() {
        assert!(
            !Arc::ptr_eq(before, after),
            "account {i}: the first send after a sleep must retire this account's \
             client too, not only the sending account's"
        );
    }

    drop(manager.enter_in_flight(1));
    for (i, (before, after)) in woken.iter().zip(&clients(&manager)).enumerate() {
        assert!(
            Arc::ptr_eq(before, after),
            "account {i}: one sleep retires the clients once, not on every later send"
        );
    }
}

/// #6 The SSE fix: once the owned guard is MOVED into a streamed body, the
/// account's `in_flight` stays incremented for the life of the STREAM — not the
/// life of the handler scope — and only decrements when the stream is dropped.
/// A handler-local guard (the bug) would have decremented at handler return,
/// before axum ever polls the body; this asserts the lifetime moved.
#[test]
fn in_flight_guard_lifetime_follows_stream_not_scope() {
    use futures::StreamExt;

    let manager = manager_of(vec![account("a", 0)]);

    // Build the guarded stream exactly as the SSE passthrough does: move the owned
    // guard into a `move` map closure so the stream body OWNS it. The `&guard`
    // anchor mirrors the proxy's forced edition-2021 capture.
    let stream = {
        let guard = manager.enter_in_flight(0);
        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
            1,
            "entering bumps the in-flight count"
        );
        futures::stream::iter(0..3u8).map(move |item| {
            let _anchor = &guard;
            item
        })
        // `guard`'s binding scope ENDS here. Were it handler-local, its Drop would
        // fire now and in_flight would fall to 0. It must NOT — the stream owns it.
    };
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
        1,
        "guard moved into the stream: count stays incremented after its binding scope ends"
    );

    drop(stream);
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
        0,
        "dropping the stream drops the guard → the account's in_flight returns to 0"
    );
}

/// #7 A configured in-flight cap of `0` is treated as DISABLED (identical to
/// unset), never as "hold out every account". A literal `Some(0)` would make
/// `in_flight >= 0` true for the whole fleet — pacing out every request, flooding
/// the "skip in selection" log, and collapsing onto the least-loaded fallback.
#[test]
fn in_flight_cap_zero_is_disabled_not_dark_pool() {
    let pacing = PacingConfig {
        max_in_flight_per_account: Some(0),
        min_spacing_ms: None,
    };
    assert_eq!(
        pacing.effective_max_in_flight(),
        None,
        "cap=0 normalises to None (disabled)"
    );
    assert!(
        !pacing.is_active(),
        "cap=0 as the only knob leaves pacing fully inert"
    );

    let manager = build_manager(
        config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
        counting_refresher(),
    );
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].in_flight = 7; // would exceed any positive cap …
        a[1].in_flight = 9;
    }
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let a = manager.accounts.read().expect("accounts lock poisoned");
    assert!(
        Manager::eligible(
            &a[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            &manager.pacing,
            true,
            now,
            now_ms,
            false,
            crate::provider::Provider::Anthropic,
            None,
            None,
            &HashSet::new(),
            &HashSet::new(),
        ),
        "cap=0 → account stays eligible regardless of in_flight (no dark pool)"
    );
    // And selection still serves normally rather than flooding the fallback.
    drop(a);
    assert!(
        manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
            .is_some(),
        "cap=0 selects a servable account without pacing anyone out"
    );
}

#[test]
fn select_prefers_lower_priority_value() {
    let manager = manager_of(vec![account("low-pri", 5), account("high-pri", 0)]);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
}

#[test]
fn select_skips_disabled_account() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    manager.set_disabled(0, true);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
}

#[test]
fn select_skips_rate_limited_account() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    manager.mark_rate_limited(0, 300);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
}

#[test]
fn select_skips_account_over_threshold() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    // Drive account 0 over the 0.90 threshold via real headers.
    let reset = (now + Duration::hours(1)).unix_timestamp();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.95".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.to_string().parse().unwrap(),
    );
    manager.update_quota(0, &headers, 200);
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
}

#[test]
fn select_returns_none_when_all_tried() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();
    let tried: HashSet<usize> = std::iter::once(0).collect();
    assert_eq!(
        manager.select(&tried, now, None, None, "/v1/messages", None),
        None
    );
}

/// Load-balancing: within a priority tier, consecutive selects fan out across
/// every eligible account (least-recently-selected first) instead of pinning
/// the same one. Six sequential picks over three equal-priority accounts must
/// land exactly twice on each. A FIXED `now` (no clock advance) proves the
/// spread is driven by the monotonic select tick, not the wall clock — so a
/// same-millisecond burst still rotates.
#[test]
fn select_spreads_load_across_a_priority_tier() {
    assert_eq!(
        six_sessionless_selects_over_one_tier_of_three(),
        [2, 2, 2],
        "load must spread evenly across the tier, not pin one account"
    );
}

/// How many of six selects with no session key, at one fixed `now`, landed on each of
/// three accounts that share priority 0.
fn six_sessionless_selects_over_one_tier_of_three() -> [usize; 3] {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let now = OffsetDateTime::now_utc();
    let mut counts = [0usize; 3];
    for _ in 0..6 {
        let idx = manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
            .expect("an account is eligible");
        counts[idx] += 1;
    }
    counts
}

/// Priority still dominates the spread: a lower-priority-value account is
/// picked every time while it is eligible, even as its select tick advances —
/// load-balancing is *within* a tier, never across tiers.
#[test]
fn select_spread_respects_priority_tiers() {
    let manager = manager_of(vec![account("primary", 0), account("pillow", 10)]);
    let now = OffsetDateTime::now_utc();
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(0),
            "the primary tier is never abandoned for a higher-priority-value account"
        );
    }
}

/// Regression: `persist_tokens` must save the config UNDER the lock, or two
/// concurrent token refreshes race on the file and one account's just-rotated
/// refresh token is lost — that account then 400s on its next refresh. Fire N
/// concurrent persists for distinct accounts and assert the reloaded file has
/// EVERY account's new refresh token.
#[test]
fn concurrent_token_persists_do_not_lose_updates() {
    use std::thread;
    let tmp = std::env::temp_dir().join(format!(
        "tcr-persist-race-{}-{}.json",
        std::process::id(),
        crate::now_ms()
    ));
    let n = 8usize;
    let accts: Vec<Account> = (0..n).map(|i| account(&format!("a{i}"), 0)).collect();

    let manager = Manager::new(
        config_with(accts),
        counting_refresher(),
        Arc::new(NoProber),
        Arc::new(NoWarmer),
        Some(tmp.clone()),
    );

    let handles: Vec<_> = (0..n)
        .map(|i| {
            let m = Arc::clone(&manager);
            thread::spawn(move || {
                m.persist_tokens(
                    &format!("a{i}"),
                    None,
                    None,
                    None,
                    crate::provider::Provider::Anthropic,
                    &Tokens {
                        id_token: None,
                        access_token: format!("new-at-{i}"),
                        refresh_token: Some(format!("new-rt-{i}")),
                        expires_at_ms: crate::now_ms() + 3_600_000,
                    },
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let reloaded = config::load(&tmp).expect("reload persisted config");
    for i in 0..n {
        let a = reloaded
            .accounts
            .iter()
            .find(|a| a.name == format!("a{i}"))
            .expect("account present after concurrent persist");
        assert_eq!(
            a.refresh_token.as_deref(),
            Some(format!("new-rt-{i}").as_str()),
            "account a{i} lost its rotated refresh token under concurrent persist"
        );
    }
    std::fs::remove_file(&tmp).ok();
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

/// Finding #1: the background probe must SKIP disabled and errored accounts —
/// never spend upstream traffic on a sidelined account nor let a probe's
/// refresh silently flip an errored account back into rotation.
///
/// UPDATED contract (control account, part 1): "disabled → skipped" now
/// has exactly one exception, the control account, covered separately by
/// [`probeable_indices_still_includes_a_disabled_control_account`] and
/// [`probeable_indices_excludes_a_disabled_non_control_account_even_with_a_control_set`]
/// below. This test has no control account configured at all, so it keeps
/// asserting the base case: an ordinary disabled account is skipped.
#[tokio::test]
async fn probe_all_skips_disabled_and_errored_accounts() {
    let prober = Arc::new(ScriptedProber {
        ok_token: "at-ok".to_string(),
    });
    let manager = build_manager_with_prober(
        config_with(vec![
            account("ok", 0),
            account("off", 0),
            account("dead", 0),
        ]),
        counting_refresher(),
        prober,
    );
    manager.set_disabled(1, true);
    manager.mark_error(2);

    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());

    // Only the healthy account was probed.
    assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
    assert_eq!(
        snap.accounts[1].probe_status,
        ProbeStatus::Never,
        "a disabled non-control account must not be probed"
    );
    assert!(snap.accounts[1].last_probe.is_none());
    assert_eq!(
        snap.accounts[2].probe_status,
        ProbeStatus::Never,
        "an errored account must not be probed (nor reactivated)"
    );
    // And the errored account was NOT flipped back to active by a probe refresh.
    assert_eq!(manager.account_status(2), Some(AccountStatus::Error));
}

/// The narrow exception itself, asserted directly on `probeable_indices()`
/// — the unit `probe_all` merely iterates: a disabled account that IS the
/// control account stays probeable, so its usage keeps getting tracked
/// even though it is deliberately out of the inference rotation.
#[test]
fn probeable_indices_still_includes_a_disabled_control_account() {
    let manager = manager_of(vec![account("ok", 0), account("gil", 0)]);
    manager.set_control_by_query(Some("gil"));
    manager.set_disabled(1, true);

    assert_eq!(
        manager.probeable_indices(),
        vec![0, 1],
        "the disabled CONTROL account (idx 1) must still be probeable"
    );
}

/// The exception must not silently widen: with a control account set,
/// every OTHER disabled account is still excluded from
/// `probeable_indices()`.
#[test]
fn probeable_indices_excludes_a_disabled_non_control_account_even_with_a_control_set() {
    let manager = manager_of(vec![account("gil", 0), account("ok", 0), account("off", 0)]);
    manager.set_control_by_query(Some("gil"));
    manager.set_disabled(0, true); // the control account itself, disabled
    manager.set_disabled(2, true); // an unrelated account, disabled

    assert_eq!(
        manager.probeable_indices(),
        vec![0, 1],
        "idx 0 (disabled control) stays probeable, idx 1 (enabled) stays \
         probeable, idx 2 (disabled, NOT control) is excluded — the \
         exception must not widen"
    );
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

/// The background probe refreshes EVERY OAuth row (not just the serving one)
/// and a FAILING probe surfaces as visible probe health — a message and a
/// timestamp — rather than a silently-frozen quota bar. A transient upstream
/// 5xx is a *soft* `RateLimited`, not a red error (the endpoint, not the
/// credential, is unhappy), and it must not fabricate a fresh bar.
#[tokio::test]
async fn probe_all_updates_every_account_and_records_failures() {
    // Account 0's token ("at-ok") probes OK; account 1's does not.
    let prober = Arc::new(ScriptedProber {
        ok_token: "at-ok".to_string(),
    });
    let manager = build_manager_with_prober(
        config_with(vec![account("ok", 0), account("bad", 0)]),
        counting_refresher(),
        prober,
    );

    manager.probe_all().await;
    let now = OffsetDateTime::now_utc();
    let snap = manager.snapshot(now);

    // Row 0: probe succeeded and its quota window was learned live.
    assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
    assert!(snap.accounts[0].probe_error.is_none());
    assert!(snap.accounts[0].last_probe.is_some());
    assert_eq!(snap.accounts[0].five_hour, Some(0.25));

    // Row 1: the 5xx probe failed and the failure is visible, not hidden —
    // but as a soft RateLimited (transient upstream), never a red error.
    assert_eq!(snap.accounts[1].probe_status, ProbeStatus::RateLimited);
    assert!(snap.accounts[1].probe_error.is_some());
    assert!(snap.accounts[1].last_probe.is_some());
    // A failed probe must NOT fabricate a fresh bar for the bad account.
    assert_eq!(snap.accounts[1].five_hour, None);
}

/// `quotaUpdatedAtMs` is the time of the newest quota reading from EITHER
/// source, so a consumer can tell a reading too old to trust from a fresh
/// one. `lastProbeMs` alone cannot: a busy account's bars move from response
/// headers between probes, and an idle account with a wedged probe shows
/// frozen bars that `lastProbeMs` (stamped on failures too) reads as recent.
///
/// Walks the hops a consumer sees: the runtime `Quota`, the manager's
/// snapshot, and the `tcr.status.v1` payload the server sends the CLI.
#[tokio::test]
async fn quota_updated_at_is_the_newest_reading_from_probe_or_headers() {
    let manager = build_manager_with_prober(
        config_with(vec![account("ok", 0), account("bad", 0)]),
        counting_refresher(),
        Arc::new(ScriptedProber {
            ok_token: "at-ok".to_string(),
        }),
    );
    let wire_value = quota_updated_at_on_the_wire;

    // Nothing read yet: absent, never a fabricated "just now".
    assert_eq!(wire_value(&manager, 0), None);

    // A fresh account: a successful probe stamps a recent reading.
    let before = crate::now_ms();
    manager.probe_all().await;
    let fresh = wire_value(&manager, 0).expect("a probed account carries quotaUpdatedAtMs");
    assert!(
        fresh >= before && fresh <= crate::now_ms(),
        "a fresh probe reads as recent: {fresh} not in [{before}, now]"
    );
    // A FAILED probe is not a reading: `lastProbeMs` moves, this does not.
    assert!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[1]
            .last_probe
            .is_some()
    );
    assert_eq!(
        wire_value(&manager, 1),
        None,
        "a failed probe read no quota and must not claim one"
    );

    // A stale account: the last reading was 40 minutes ago, past any sane
    // freshness limit, and the wire says exactly when.
    let stale = crate::now_ms() - 40 * 60 * 1000;
    {
        let mut accounts = accounts_mut(&manager);
        accounts[0].quota.updated_at = Some(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(stale) * 1_000_000)
                .expect("valid instant"),
        );
    }
    assert_eq!(
        wire_value(&manager, 0),
        Some(stale),
        "the status row carries the exact time of the stale reading"
    );

    // A response with no rate-limit headers (an error page) is silence, not
    // a reading: the stale time stands.
    manager.update_quota(0, &reqwest::header::HeaderMap::new(), 502);
    assert_eq!(wire_value(&manager, 0), Some(stale));

    // A header update after the stale probe moves it forward.
    let before = crate::now_ms();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.30".parse().expect("header value"),
    );
    manager.update_quota(0, &headers, 200);
    let moved = wire_value(&manager, 0).expect("still present");
    assert!(
        moved >= before,
        "a 5h header reading moves quotaUpdatedAtMs forward: {moved} < {before}"
    );

    // A header update that returns `false` (no 5h window) but reads the 7d
    // window is still a reading, and also moves it forward.
    {
        let mut accounts = accounts_mut(&manager);
        accounts[0].quota.updated_at = Some(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(stale) * 1_000_000)
                .expect("valid instant"),
        );
    }
    let before = crate::now_ms();
    let mut weekly_only = reqwest::header::HeaderMap::new();
    weekly_only.insert(
        "anthropic-ratelimit-unified-7d-utilization",
        "0.40".parse().expect("header value"),
    );
    assert!(
        !manager.update_quota(0, &weekly_only, 200),
        "control: this response carries no 5h window"
    );
    let moved = wire_value(&manager, 0).expect("still present");
    assert!(
        moved >= before,
        "a 7d-only header reading moves quotaUpdatedAtMs forward: {moved} < {before}"
    );
}

/// Write access to the runtime rows, for tests that rewind a timestamp.
fn accounts_mut(manager: &Manager) -> std::sync::RwLockWriteGuard<'_, Vec<AccountRuntime>> {
    manager.accounts.write().expect("accounts lock poisoned")
}

/// Probe 429-tolerance: the usage endpoint throttling the *probe* is benign.
/// After a good probe learns a bar, a following 429 probe must (a) read as
/// `RateLimited`, never `Error`, and (b) leave the last-good utilization
/// intact — so a throttled sweep never blanks the bars nor cries false error.
#[tokio::test]
async fn probe_429_is_rate_limited_and_keeps_last_utilization() {
    let prober = Arc::new(FirstOkThen429Prober {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        prober,
    );

    // First sweep: probe succeeds, the 5-hour bar is learned.
    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
    assert_eq!(snap.accounts[0].five_hour, Some(0.25));

    // Second sweep: the probe is 429'd. Soft state, and the bar is untouched.
    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::RateLimited,
        "a probe 429 is soft, never a red error"
    );
    assert_eq!(
        snap.accounts[0].five_hour,
        Some(0.25),
        "a probe 429 must preserve the last-good utilization, not blank it"
    );
    assert!(snap.accounts[0].probe_error.is_some());
}

/// A prober that always fails with a fixed HTTP status — for pinning exactly
/// how many consecutive reads of a given status it takes (or does not take)
/// to move `probe_status` past the benign `RateLimited`.
struct AlwaysStatusProber {
    status: u16,
}
impl UsageProber for AlwaysStatusProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let status = self.status;
        Box::pin(async move {
            Err(ProbeError {
                status: Some(status),
                message: "upstream boom".into(),
                retry_after_secs: None,
            })
        })
    }
}

/// 1b, the half of the fix `probe_429_is_rate_limited_and_keeps_last_utilization`
/// does not cover: ONE 5xx read is still the same benign, self-clearing
/// `RateLimited` a 429 gets — it must NOT jump straight to the visible
/// escalated state on the first hiccup.
#[tokio::test]
async fn a_single_5xx_probe_stays_rate_limited() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(AlwaysStatusProber { status: 503 }),
    );

    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::RateLimited,
        "a lone 5xx is a hiccup, same as a 429 — not yet the sustained-outage state"
    );
}

/// 1b, the other half: a SUSTAINED run of 5xx reads — as opposed to the
/// single hiccup above — is first-hand evidence the usage endpoint itself is
/// down, and must escalate to the visible `UpstreamDown` rather than keep
/// hiding behind `RateLimited` forever.
#[tokio::test]
async fn sustained_5xx_probes_escalate_to_upstream_down() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(AlwaysStatusProber { status: 502 }),
    );

    for sweep in 1..probing::SUSTAINED_5XX_THRESHOLD {
        manager.probe_all().await;
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            snap.accounts[0].probe_status,
            ProbeStatus::RateLimited,
            "sweep {sweep}: below the threshold, still the benign label"
        );
    }

    // The crossing sweep.
    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::UpstreamDown,
        "a sustained run of 5xx must surface as its own visible status"
    );
}

/// The counters driving the two statuses above must be genuinely
/// independent: a 429 never contributes to the sustained-5xx count, so an
/// account that is merely self-throttled — however many times in a row —
/// must never be mistaken for a real outage.
#[tokio::test]
async fn repeated_429s_never_escalate_to_upstream_down() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(AlwaysStatusProber { status: 429 }),
    );

    for _ in 0..(probing::SUSTAINED_5XX_THRESHOLD * 3) {
        manager.probe_all().await;
    }
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::RateLimited,
        "a self-throttle stays benign no matter how many times it repeats"
    );
}

/// A success between two 5xx runs must reset the sustained-5xx count, the
/// same way it resets `consecutive_probe_failures` — a brief recovery in the
/// middle is not "the endpoint has been down the whole time".
#[tokio::test]
async fn a_success_between_5xx_runs_resets_the_sustained_count() {
    let state = Arc::new(Mutex::new(0usize));

    struct FlakyProber {
        state: Arc<Mutex<usize>>,
    }
    impl UsageProber for FlakyProber {
        fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
            let mut n = self.state.lock().expect("lock poisoned");
            *n += 1;
            // 5xx, 5xx, OK, 5xx, 5xx — never THREE in a row.
            let fail = matches!(*n, 1 | 2 | 4 | 5);
            let call = *n;
            drop(n);
            Box::pin(async move {
                if fail {
                    Err(ProbeError {
                        status: Some(500),
                        message: format!("upstream boom #{call}"),
                        retry_after_secs: None,
                    })
                } else {
                    Ok(Usage {
                        five_hour: crate::probe::WindowReading::Read(UsageBucket {
                            utilization: Some(0.1),
                            reset_at_ms: Some(crate::now_ms() + 3_600_000),
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                        extra_usage_usd: None,
                        codex: None,
                        claude_resets: None,
                    })
                }
            })
        }
    }

    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(FlakyProber { state }),
    );

    for _ in 0..5 {
        manager.probe_all().await;
    }
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::RateLimited,
        "two 5xx, a success, then two more 5xx is never THREE IN A ROW — \
         the intervening success must have reset the count, or this reads \
         as UpstreamDown for a run that was never actually sustained"
    );
}

#[tokio::test]
async fn probe_retry_after_blocks_the_next_probe_until_it_expires() {
    let calls = Arc::new(AtomicUsize::new(0));
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(RetryAfterProber {
            calls: Arc::clone(&calls),
        }),
    );

    manager.probe_all().await;
    manager.probe_all().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    manager.accounts.write().unwrap()[0].probe_retry_after_ms = Some(crate::now_ms() - 1);
    manager.probe_all().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn probe_retry_after_has_a_seven_day_safety_cap() {
    let manager = manager_of(vec![account("a", 0)]);
    let before = crate::now_ms();

    manager.record_probe(
        0,
        ProbeStatus::RateLimited,
        Some("rate limited".to_string()),
        Some(u64::MAX),
    );

    let deadline = manager.accounts.read().unwrap()[0]
        .probe_retry_after_ms
        .unwrap();
    assert!(deadline >= before + 7 * 24 * 60 * 60 * 1000);
    assert!(deadline <= crate::now_ms() + 7 * 24 * 60 * 60 * 1000);
}

/// A network/transport probe failure (no HTTP status, not a timeout) stays a
/// VISIBLE `Error` — a persistent connectivity problem (upstream down, DNS/TLS,
/// a proxy-env regression) must never hide behind a benign "busy". Only
/// endpoint-side 429/5xx soften; a transport failure is the credential's whole
/// path being down, exactly the signal probe-health exists to surface.
#[tokio::test]
async fn probe_transport_failure_is_visible_error() {
    struct NetFailProber;
    impl UsageProber for NetFailProber {
        fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
            Box::pin(async {
                Err(ProbeError {
                    status: None,
                    message: "error sending request: connection refused".into(),
                    retry_after_secs: None,
                })
            })
        }
    }

    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(NetFailProber),
    );
    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::Error,
        "a transport failure must stay a visible error, not a soft busy"
    );
}

/// Change 3: an account near/over its weekly quota is held out of rotation but
/// stays operationally ACTIVE — never a red "error". The snapshot exposes this
/// as a `quota_state` (`NearLimit` / Exhausted) while `status` remains "active".
#[test]
fn near_and_over_quota_render_active_with_honest_quota_state() {
    use crate::stats::QuotaState;

    let manager = manager_of(vec![account("near", 0), account("full", 0)]);
    let now = OffsetDateTime::now_utc();
    set_7d(&manager, 0, "0.95"); // over the 0.90 threshold, under 100% → NearLimit
    set_7d(&manager, 1, "1.20"); // over 100% → Exhausted

    let snap = manager.snapshot(now);
    // Both are held out of rotation …
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        None,
        "over-threshold accounts are held out of rotation"
    );
    // … yet neither is an "error": status stays active, quota_state is honest.
    assert_eq!(snap.accounts[0].status, "active");
    assert_eq!(snap.accounts[1].status, "active");
    assert_eq!(snap.accounts[0].quota_state, QuotaState::NearLimit);
    assert_eq!(snap.accounts[1].quota_state, QuotaState::Exhausted);
}

/// Biting test for the 0.90 → 0.95 default-threshold raise. Keyed off the
/// SHIPPED config default (parsed from a minimal config), not a hardcoded
/// literal, so it tracks the constant: an account at 0.92 weekly utilization
/// has real headroom under the 0.95 default → stays Normal and eligible. On
/// the pre-change tree (default 0.90) that same 0.92 tripped the line → held
/// `NearLimit` and `select()` would refuse it, so this fails before the raise.
/// 0.96 → `NearLimit` (held, credential fine); 1.00 → Exhausted (fully spent).
#[test]
fn default_threshold_raise_gives_92pct_headroom_normal_near_full() {
    use crate::stats::QuotaState;

    // The default the shipped config applies when `switchThreshold` is absent.
    let default_threshold = serde_json::from_str::<Config>(r#"{ "accounts": [] }"#)
        .expect("minimal config parses")
        .switch_threshold;
    let mut config = config_with(vec![
        account("headroom", 0),
        account("near", 0),
        account("full", 0),
    ]);
    config.switch_threshold = default_threshold;
    let manager = build_manager(config, counting_refresher());
    let now = OffsetDateTime::now_utc();
    set_7d(&manager, 0, "0.92"); // under the 0.95 default → real headroom, Normal
    set_7d(&manager, 1, "0.96"); // over 0.95, under 100% → NearLimit
    set_7d(&manager, 2, "1.00"); // at 100% → Exhausted

    let snap = manager.snapshot(now);
    assert_eq!(
        snap.accounts[0].quota_state,
        QuotaState::Normal,
        "0.92 has headroom under the 0.95 default (pre-change 0.90 held it NearLimit)"
    );
    assert_eq!(snap.accounts[1].quota_state, QuotaState::NearLimit);
    assert_eq!(snap.accounts[2].quota_state, QuotaState::Exhausted);
    // The headroom account is not merely labelled Normal — it is actually
    // servable (pre-change it was held out and select() skipped it).
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0),
        "the 0.92 account is eligible under the raised default"
    );
}

/// Best-practices review (Apollo ch7 — no stale shadow of a live value): the
/// `Throttled` enum is cleared only when the account next serves a non-429, so a
/// naturally-expired hold would show a STALE "throttled" in the snapshot while
/// routing already treats the account as eligible (routing reads the live
/// `rate_limited_until_ms`, not the enum). The snapshot must derive the displayed
/// status from the live deadline, like the quota bars and `rate_limited_until`.
#[test]
fn snapshot_status_is_active_once_the_throttle_hold_expires() {
    let manager = manager_of(vec![account("live-hold", 0), account("expired-hold", 0)]);
    let now = OffsetDateTime::now_utc();

    manager.mark_rate_limited(0, 300); // a LIVE hold → still "throttled"
    manager.mark_rate_limited(1, 300); // mark, then rewind the deadline into the past:
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].rate_limited_until_ms = Some(crate::now_ms() - 1_000);
    }

    let snap = manager.snapshot(now);
    assert_eq!(
        snap.accounts[0].status, "throttled",
        "a live hold still displays throttled"
    );
    assert_eq!(
        snap.accounts[1].status, "active",
        "an expired hold must display active, not a stale throttled — routing already \
         treats it as eligible"
    );
    assert!(
        snap.accounts[1].rate_limited_until.is_none(),
        "the live rate_limited_until is already filtered to future-only"
    );
}

/// Bug #4: the request counter increments ONCE per client request, not once
/// per upstream response. A request that folds quota headers from several
/// upstream responses (a retry that rotated accounts) still counts one.
#[test]
fn request_counter_counts_once_across_retries() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();

    // Simulate two upstream responses for a single client request (e.g. a
    // 429 that recorded a spent bucket, then the retry's 200).
    let headers = reqwest::header::HeaderMap::new();
    manager.update_quota(0, &headers, 429);
    manager.update_quota(0, &headers, 200);
    // Exactly one terminal serve.
    manager.record_served(0, now, None, SessionKind::Fallback);

    let snap = manager.snapshot(now);
    assert_eq!(snap.accounts[0].requests, 1);
    assert_eq!(snap.current, Some(0));
}

/// `record_served`'s `kind` threads through the per-session stat into the
/// snapshot, so the TUI can fold fallback sessions without touching routing.
/// A stable serve persists `SessionKind::Stable`; a fallback serve `Fallback`.
#[test]
fn record_served_threads_stable_flag_into_snapshot() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();

    // Two distinct session keys: one stable-identity serve, one fallback.
    manager.record_served(0, now, Some(0xABCD), SessionKind::Stable);
    manager.record_served(0, now, Some(0x1234), SessionKind::Fallback);

    let snap = manager.snapshot(now);
    let stable = snap
        .sessions
        .iter()
        .find(|s| s.id == short_session_id(0xABCD))
        .expect("stable session present in snapshot");
    let fallback = snap
        .sessions
        .iter()
        .find(|s| s.id == short_session_id(0x1234))
        .expect("fallback session present in snapshot");
    assert_eq!(
        stable.kind,
        SessionKind::Stable,
        "stable serve must persist SessionKind::Stable"
    );
    assert_eq!(
        fallback.kind,
        SessionKind::Fallback,
        "fallback serve must persist SessionKind::Fallback"
    );
}

/// Session `key`'s row in the manager's snapshot at `now`.
fn session_in_the_snapshot(manager: &Manager, now: OffsetDateTime, key: u64) -> SessionSnapshot {
    manager
        .snapshot(now)
        .sessions
        .into_iter()
        .find(|s| s.id == short_session_id(key))
        .expect("the session is present in the snapshot")
}

/// The sessions pane must report the PIN, never whoever happened to serve the
/// last request. A hold that clears while the pinned account's prompt cache is
/// still warm DIVERTS one request and deliberately keeps the pin (see
/// [`CACHE_WARM_HOLD_SECS`]) — but the snapshot used to take its account from the
/// SERVING index, so every divert made the session visibly jump accounts in the
/// TUI although the pin never moved. A display that misreports state is worse
/// than no display: it made a fleet measured at a 1.70% switch rate read as
/// "sessions keep jumping".
#[test]
fn snapshot_reports_the_pin_not_the_last_serve() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 0xFEEDu64;

    // Pin the session to `a`, and serve one request there.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(0),
        "precondition: the first select pins the session to `a`"
    );
    manager.record_served(0, now, Some(key), SessionKind::Stable);

    // `a` picks up a SHORT hold — one that clears while its cache is still warm.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].rate_limited_until_ms = Some(odt_to_ms(now) + SHORT_HOLD_SECS * 1000);
    }
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(1),
        "precondition: the short hold DIVERTS this one request to `b`"
    );
    manager.record_served(1, now, Some(key), SessionKind::Stable);

    let session = session_in_the_snapshot(&manager, now, key);
    assert_eq!(
        session.account, "a",
        "the snapshot must report the PIN, which a divert never moves"
    );
    assert_eq!(
        session.last_served_account, "b",
        "the divert must stay OBSERVABLE in its own field — the goal is honesty, \
         not concealment"
    );
}

/// The other half of the same contract, so the fix cannot swing too far and start
/// hiding genuine re-keys behind a stale pin: when the pin DURABLY moves, the
/// snapshot follows it. Only an ACCOUNT-level hard gate re-keys, so sideline `a`
/// and let the next select re-pin the session to `b`.
#[test]
fn snapshot_account_follows_a_real_rekey() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 0xBEEFu64;

    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(0),
        "precondition: the session starts pinned to `a`"
    );
    manager.record_served(0, now, Some(key), SessionKind::Stable);

    // A dead credential is ACCOUNT-level death → the pin is durably re-keyed.
    manager.mark_error(0);
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(1),
        "precondition: an errored pin re-keys the session to `b`"
    );
    manager.record_served(1, now, Some(key), SessionKind::Stable);

    let session = session_in_the_snapshot(&manager, now, key);
    assert_eq!(
        session.account, "b",
        "a REAL re-key must move the reported account — the pin is the authority \
         in both directions"
    );
    assert_eq!(session.last_served_account, "b");
}

/// Serving a request must not re-order the sessions pane. Rows used to sort
/// most-recent-first, so every single request threw its session to the top and
/// the pane churned under the operator's eyes — the other half of why a stable
/// fleet looked like it was thrashing.
#[test]
fn session_rows_do_not_reorder_on_a_serve() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();
    let keys = [0x11u64, 0x22, 0x33];

    // Three sessions pinned to the one account, last seen 3s/2s/1s ago — so a
    // recency order and a stable order are genuinely different orders here.
    for key in keys {
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
    }
    for (offset, key) in [3i64, 2, 1].into_iter().zip(keys) {
        manager.record_served(
            0,
            now - Duration::seconds(offset),
            Some(key),
            SessionKind::Stable,
        );
    }

    let ids = |snap: StatsSnapshot| -> Vec<String> {
        snap.sessions.iter().map(|s| s.id.clone()).collect()
    };
    let before = ids(manager.snapshot(now));
    assert_eq!(
        before.len(),
        3,
        "precondition: all three sessions are shown"
    );

    // The OLDEST session — not the first row — serves one request.
    manager.record_served(0, now, Some(keys[0]), SessionKind::Stable);
    let after = ids(manager.snapshot(now));

    assert_eq!(
        before, after,
        "a serve must never move a session's row; the age belongs in the `Last` \
         column, not in the row order"
    );
}

/// Cache tokens count: `update_usage` accumulates whatever the caller sums,
/// which for the proxy includes cache-creation + cache-read input tokens.
#[test]
fn update_usage_accumulates_input_and_output() {
    let manager = manager_of(vec![account("a", 0)]);
    // e.g. input_tokens=10 + cache_creation=100 + cache_read=1000 = 1110.
    // The last two args are the cache-read / cache-creation SUBSETS of that sum.
    manager.update_usage(0, 1110, 42, 1000, 100);
    manager.update_usage(0, 0, 8, 0, 0);
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    // R1: input_tokens is the SUM, byte-identical to the pre-cache behaviour.
    assert_eq!(snap.accounts[0].input_tokens, 1110);
    assert_eq!(snap.accounts[0].output_tokens, 50);
    // NEW: the cache components accumulate separately (a subset of the sum).
    assert_eq!(snap.accounts[0].cache_read_tokens, 1000);
    assert_eq!(snap.accounts[0].cache_creation_tokens, 100);
}

/// FINDING 7. `input_tokens` is the QUOTA counter, and `update_usage` is
/// `pub` on a library crate — so it must add the caller's number, verbatim,
/// whatever relationship that number has to the components passed beside
/// it. Decomposing with a saturating subtraction and re-summing the pieces
/// grew the counter by 800 for an `input_tokens` of 100, which is both the
/// quota consumption and the denominator of `cacheHitRatio`.
#[test]
fn update_usage_adds_the_callers_input_verbatim() {
    let manager = manager_of(vec![account("a", 0)]);
    // An inconsistent caller: 100 of input against 800 of components.
    manager.update_usage(0, 100, 5, 750, 50);
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].input_tokens, 100,
        "the quota counter grows by exactly what the caller passed"
    );
    assert_eq!(snap.accounts[0].output_tokens, 5);
    assert_eq!(snap.accounts[0].cache_read_tokens, 750);
    assert_eq!(snap.accounts[0].cache_creation_tokens, 50);
}

/// Drive an account's model-scoped weekly (`7d_oi`, Fable) bucket over
/// threshold via the real rate-limit headers the proxy learns from.
fn exhaust_oi_bucket(manager: &Manager, idx: usize, now: OffsetDateTime) {
    let reset = (now + Duration::hours(2)).unix_timestamp();
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        "anthropic-ratelimit-unified-7d_oi-utilization",
        "0.95".parse().unwrap(),
    );
    h.insert(
        "anthropic-ratelimit-unified-7d_oi-reset",
        reset.to_string().parse().unwrap(),
    );
    manager.update_quota(idx, &h, 200);
}

/// Accounts `a` and `b` at one priority with `a`'s Fable bucket exhausted, and the
/// `now` it was exhausted at.
fn a_and_b_with_a_oi_exhausted() -> (Arc<Manager>, OffsetDateTime) {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    exhaust_oi_bucket(&manager, 0, now);
    (manager, now)
}

/// HEADLINE per-model routing: a Fable request SKIPS an account whose `7d_oi`
/// (Fable weekly) bucket is exhausted, and rotates to the next eligible one.
#[test]
fn fable_request_skips_oi_exhausted_account() {
    let (manager, now) = a_and_b_with_a_oi_exhausted();
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            None,
            "/v1/messages",
            None
        ),
        Some(1),
        "a Fable request must skip the OI-exhausted account 0"
    );
}

/// The SAME OI-exhausted account still serves every non-Fable model: with
/// account 1 already tried, a non-Fable request lands right back on account 0.
#[test]
fn fable_exhausted_account_still_serves_non_fable() {
    let (manager, now) = a_and_b_with_a_oi_exhausted();
    let tried: HashSet<usize> = std::iter::once(1).collect();
    assert_eq!(
        manager.select(
            &tried,
            now,
            Some("claude-opus-4-6"),
            None,
            "/v1/messages",
            None
        ),
        Some(0),
        "a non-Fable model must still serve from the OI-exhausted account 0"
    );
}

/// A request with no known model never consults the `7d_oi` bucket: account 0
/// (OI-exhausted) is still picked when it is the only untried account.
#[test]
fn no_model_ignores_oi_bucket() {
    let (manager, now) = a_and_b_with_a_oi_exhausted();
    let tried: HashSet<usize> = std::iter::once(1).collect();
    assert_eq!(
        manager.select(&tried, now, None, None, "/v1/messages", None),
        Some(0),
        "no-model traffic must ignore the model-scoped OI bucket"
    );
}

/// Session affinity, headline: a session key pins to ONE account. Six selects
/// with the SAME key over three equal-priority accounts all land on the same
/// idx — the exact inverse of `select_spreads_load_across_a_priority_tier`.
#[test]
fn affinity_pins_session_to_one_account() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let now = OffsetDateTime::now_utc();
    let first = manager
        .select(&HashSet::new(), now, None, Some(7), "/v1/messages", None)
        .expect("an account is eligible");
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(7), "/v1/messages", None),
            Some(first),
            "the same session key must stay pinned to one account"
        );
    }
}

/// Regression guard: the disabled path (`affinity = None`) is byte-unchanged
/// behaviour — six selects over three accounts still fan out evenly [2,2,2].
#[test]
fn affinity_none_still_spreads() {
    assert_eq!(
        six_sessionless_selects_over_one_tier_of_three(),
        [2, 2, 2],
        "the disabled affinity path must spread exactly like today"
    );
}

/// Migration: when a session's pinned account becomes ineligible (rate
/// limited long enough to outlive its prompt cache), the next same-key select
/// re-pins to a DIFFERENT eligible account and then sticks to that one.
#[test]
fn affinity_repins_when_pinned_account_ineligible() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(42), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(pinned, LONG_HOLD_SECS);
    let repinned = manager
        .select(&HashSet::new(), now, None, Some(42), "/v1/messages", None)
        .expect("the other account is eligible");
    assert_ne!(repinned, pinned, "must migrate off the ineligible pin");
    // And it sticks to the new pin.
    for _ in 0..3 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(42), "/v1/messages", None),
            Some(repinned),
            "the re-pin must be durable"
        );
    }
}

/// Accounts `a` and `b` at one priority, paced to one request in flight each, and the
/// account session `key`'s first select at `now` pinned it to.
fn one_in_flight_pair_with_a_pin(key: u64, now: OffsetDateTime) -> (Arc<Manager>, usize) {
    let pacing = PacingConfig {
        max_in_flight_per_account: Some(1),
        min_spacing_ms: None,
    };
    let manager = build_manager(
        config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
        counting_refresher(),
    );
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    (manager, pinned)
}

/// PACING gates DIVERT, they never RE-KEY: a pinned account that is merely paced
/// (at the in-flight cap, every hard gate clear) yields THIS request to another
/// account while the pin stays put, and the session returns to it the moment the
/// guards drop. Before the fix the fall-through re-pin at the bottom of `select`
/// rewrote the pin to the diverted account, permanently cold-starting that
/// session's per-account prompt cache.
///
/// Contrast `soft_gated_pin_is_served_not_diverted`, where the OTHER soft gate —
/// our own utilization threshold — does not even divert. The difference is what
/// each gate knows: `in_flight` is a fact we measure exactly and continuously
/// about our own concurrency, and spreading a burst is the whole reason the cap
/// exists; utilization is arithmetic over headers that go stale by minutes.
#[test]
fn soft_paced_pin_diverts_without_repinning() {
    let now = OffsetDateTime::now_utc();
    let (manager, pinned) = one_in_flight_pair_with_a_pin(77, now);
    // Saturate ONLY the pinned account: at cap=1 it is soft-paced while every
    // hard gate (disabled/error/hold/quota) stays clear.
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[pinned].in_flight = 1;
    }
    let diverted = manager
        .select(&HashSet::new(), now, None, Some(77), "/v1/messages", None)
        .expect("the un-paced account serves this request");
    assert_ne!(
        diverted, pinned,
        "a soft-paced pin must yield THIS request to the cooler account"
    );
    assert_eq!(
        pin_of(&manager, 77),
        Some(pinned),
        "a PACING divert must not move the pin"
    );
    // The pacing guard clears → the session snaps back to its warm account.
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[pinned].in_flight = 0;
    }
    for _ in 0..3 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(77), "/v1/messages", None),
            Some(pinned),
            "once un-paced the session must return to its original account"
        );
    }
}

/// The last-touch stamp is still refreshed on a pacing divert, so a heavily
/// diverted session can never become the `AFFINITY_CAP` eviction victim (the
/// eviction sorts on exactly this field).
#[test]
fn soft_paced_divert_refreshes_pin_last_touch() {
    let now = OffsetDateTime::now_utc();
    let (manager, pinned) = one_in_flight_pair_with_a_pin(88, now);
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[pinned].in_flight = 1;
    }
    // A later `now` is what the divert must stamp onto the surviving pin.
    let later = now + Duration::seconds(30);
    let later_ms = odt_to_ms(later);
    manager
        .select(&HashSet::new(), later, None, Some(88), "/v1/messages", None)
        .expect("the un-paced account serves this request");
    let pins = manager.affinity.lock().expect("affinity lock poisoned");
    assert_eq!(
        pins.get(&88).copied(),
        Some((pinned, later_ms)),
        "the divert must re-insert the OLD index with a FRESH last-touch"
    );
}

/// If the pinned account is in `tried` AND fails a HARD gate, affinity falls
/// through to a normal pick and re-pins to the new account. Being in `tried`
/// is on its own only a SOFT signal (see
/// `transient_tried_failure_keeps_the_pin`) — it takes a hard gate, here a hold
/// that outlives the prompt cache, to make the fall-through durable. A SHORT
/// hold would not: it diverts and keeps the pin (see
/// `short_hold_diverts_but_keeps_the_pin`), so the fall-through would serve the
/// other account while leaving the session pinned where it was.
#[test]
fn affinity_falls_through_when_pinned_in_tried_and_hard_gated() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let a = manager
        .select(&HashSet::new(), now, None, Some(9), "/v1/messages", None)
        .expect("an account is eligible");
    // The failover that put A in `tried` was durable: it armed a hold that
    // outlives A's prompt cache, so there is nothing left to come home to.
    manager.mark_rate_limited(a, LONG_HOLD_SECS);
    let tried: HashSet<usize> = std::iter::once(a).collect();
    let b = manager
        .select(&tried, now, None, Some(9), "/v1/messages", None)
        .expect("the untried account is eligible");
    assert_ne!(b, a, "must fall through the tried pin to the other account");
    // The pin updated to B: a fresh same-key select with nothing tried sticks to B.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(9), "/v1/messages", None),
        Some(b),
        "the pin must have migrated to the fallen-through account"
    );
}

// ---- only a HARD gate may re-key a session ---------------------------------

/// THE cache-loss fix: an account crossing the SOFT utilization threshold used to
/// dump every session pinned to it at once, because `eligible()` folds
/// `quota.is_near` into the same bool as the hard gates. Keeping the pin while
/// still diverting the request — the first half of the fix — bought nothing: the
/// diverted request paid the cold prefix anyway (measured live: a 44.4%
/// account-switch rate on SUCCESSFUL serves, with zero hard failures). The
/// threshold is our own arithmetic, and it routinely benches an account Anthropic
/// is answering 200s for, so it may not bench a warm pin at all: the pinned
/// account SERVES. With eleven of thirteen live accounts sitting at 98-100%, this
/// is the single largest remaining cause of per-account prompt-cache loss.
#[test]
fn soft_gated_pin_is_served_not_diverted() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(1234), "/v1/messages", None)
        .expect("an account is eligible");
    // Over the soft threshold, but Anthropic still says `allowed_warning`:
    // every HARD gate is clear.
    set_over_threshold(&manager, pinned, 0.995, "allowed_warning");

    for _ in 0..3 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(1234), "/v1/messages", None),
            Some(pinned),
            "an over-threshold pin that clears every HARD gate still serves its \
             own session — upstream, not our utilization arithmetic, is the oracle"
        );
    }
    assert_eq!(
        pin_of(&manager, 1234),
        Some(pinned),
        "crossing the SOFT utilization threshold must not re-key the session"
    );

    // The threshold is not dead — it still steers selection with no pin to
    // protect, so a session arriving cold lands on the cooler account.
    let unpinned = manager
        .select(&HashSet::new(), now, None, None, "/v1/messages", None)
        .expect("the under-threshold account is eligible");
    assert_ne!(
        unpinned, pinned,
        "the soft threshold still gates UNPINNED selection"
    );
}

/// APP-16 (measured 2026-09-11): an idle-cold pin on an over-threshold account
/// has nothing left to lose by moving — its prompt cache is already cold by the
/// crate's own [`CACHE_WARM_HOLD_SECS`] clock, so serving the pin only keeps a
/// dead-weight session off the accounts sitting idle. `select` must fall through
/// to the normal pick, which durably re-keys onto the least-loaded ELIGIBLE
/// account, exactly as a genuinely dead pin does.
#[test]
fn idle_cold_session_on_over_threshold_account_rekeys_to_an_eligible_account() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    set_over_threshold(&manager, 0, 0.95, "allowed_warning");
    let key = 9001u64;
    seed_pin(
        &manager,
        key,
        0,
        odt_to_ms(now) - (CACHE_WARM_HOLD_SECS + 1) * 1000,
    );

    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the eligible account b serves this request");
    assert_eq!(
        served, 1,
        "an idle-cold pin on an over-threshold account must fall through to \
         the eligible account, not serve the dead-weight pin"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(1),
        "the session's pin must durably move to the account that served it"
    );
}

/// The complement, pinned so the change above cannot widen: a session touched
/// well inside [`CACHE_WARM_HOLD_SECS`] still has a warm cache to protect, so an
/// over-threshold pin is served exactly as `soft_gated_pin_is_served_not_diverted`
/// already established — this is today's behaviour, unchanged.
#[test]
fn warm_session_on_over_threshold_account_keeps_its_pin() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    set_over_threshold(&manager, 0, 0.95, "allowed_warning");
    let key = 9002u64;
    seed_pin(&manager, key, 0, odt_to_ms(now) - 10_000);

    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the pinned account is still eligible on hard gates");
    assert_eq!(
        served, 0,
        "a warm (well within CACHE_WARM_HOLD_SECS) over-threshold pin still \
         serves its own session"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(0),
        "the pin must not move while the cache is still warm"
    );
}

/// Falling through must not turn a soft threshold into a hard 429: with no
/// eligible alternative (B disabled), the idle-cold pin still serves its own
/// over-threshold account — the same last-resort the fall-through pick already
/// gives a genuinely dead pin.
#[test]
fn idle_cold_session_stays_when_no_eligible_alternative() {
    let manager = manager_of(vec![
        account("a", 0),
        Account {
            disabled: Some(true),
            ..account("b", 0)
        },
    ]);
    let now = OffsetDateTime::now_utc();
    set_over_threshold(&manager, 0, 0.95, "allowed_warning");
    let key = 9003u64;
    seed_pin(
        &manager,
        key,
        0,
        odt_to_ms(now) - (CACHE_WARM_HOLD_SECS + 1) * 1000,
    );

    let served = manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
    assert_eq!(
        served,
        Some(0),
        "with no eligible alternative, an idle-cold pin must still serve its \
         over-threshold account rather than 429 the request"
    );
}

/// The guard against over-correcting: a HARD gate still re-keys durably. A
/// rate-limit hold that outlives the prompt cache means the account is gone for
/// longer than the session's prefix survives, so the session moves and STAYS
/// moved. (A SHORTER hold is the SOFT case — see
/// `short_hold_diverts_but_keeps_the_pin`.)
#[test]
fn pin_with_live_hold_is_rekeyed() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(2345), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(pinned, LONG_HOLD_SECS);

    let served = manager
        .select(&HashSet::new(), now, None, Some(2345), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(served, pinned, "a held pin cannot serve");
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&2345)
            .map(|&(idx, _)| idx),
        Some(served),
        "a hold outliving the prompt cache is a HARD gate — it must re-key the \
         session"
    );
}

/// THE guard against over-correcting serve-the-pin into serve-anything: an
/// account that is over the soft threshold AND holds a live 429 must still
/// rotate. This is the exact combination the serve-the-pin path could swallow —
/// the soft gate that now serves, stacked on the hard gate that must still win —
/// and it is also the self-healing loop that makes serving over the threshold
/// safe: a genuinely rejected account answers with a 429, that 429 arms
/// `rate_limited_until_ms`, and this select is the one that moves the session off
/// it. `hard_ok` stays the sole authority in both directions.
#[test]
fn pin_that_fails_a_hard_gate_still_rotates() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None)
        .expect("an account is eligible");
    set_over_threshold(&manager, pinned, 0.995, "allowed_warning");
    // The serve-over-threshold hit a real 429, which armed a real hold — and a
    // long one, past the point where waiting it out could still hit a warm cache.
    manager.mark_rate_limited(pinned, LONG_HOLD_SECS);

    let served = manager
        .select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(
        served, pinned,
        "a live hold is HARD — it outranks the serve-the-pin path"
    );
    assert_eq!(
        pin_of(&manager, 5678),
        Some(served),
        "and it re-keys the session DURABLY, not just for this request"
    );
    // Durable: a fresh select with nothing tried stays on the new account.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None),
        Some(served),
        "the session must not snap back to the held account"
    );
}

/// `quota.status == "rejected"` is Anthropic's own verdict, so it is HARD even
/// though the account looks identical to the `allowed_warning` case above in
/// every other field. This pair is the whole soft/hard distinction in two tests.
#[test]
fn pin_rejected_by_upstream_is_rekeyed() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(3456), "/v1/messages", None)
        .expect("an account is eligible");
    set_over_threshold(&manager, pinned, 0.995, "rejected");

    let served = manager
        .select(&HashSet::new(), now, None, Some(3456), "/v1/messages", None)
        .expect("the allowed account serves this request");
    assert_ne!(served, pinned, "a rejected pin cannot serve");
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&3456)
            .map(|&(idx, _)| idx),
        Some(served),
        "an upstream `rejected` is a HARD gate — it must re-key the session"
    );
}

/// A pin landing in `tried` means this ONE request failed over it — a dropped
/// connection or a 5xx, not proof the account is gone. Divert, keep the pin.
/// Self-healing: when the failure is durable it arms a hold, and the very next
/// select re-keys on that evidence instead of on a single blip.
#[test]
fn transient_tried_failure_keeps_the_pin() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(4567), "/v1/messages", None)
        .expect("an account is eligible");
    let tried: HashSet<usize> = std::iter::once(pinned).collect();

    let served = manager
        .select(&tried, now, None, Some(4567), "/v1/messages", None)
        .expect("the untried account serves this request");
    assert_ne!(served, pinned, "the tried pin cannot serve THIS request");
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&4567)
            .map(|&(idx, _)| idx),
        Some(pinned),
        "a transient failover must not re-key the session"
    );
    // Nothing tried → straight back to the warm pin.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(4567), "/v1/messages", None),
        Some(pinned),
        "the session must return to its original account"
    );

    // Now the failure proves durable (a 429 armed a hold long enough to outlive
    // the account's prompt cache) — that IS hard, so the same tried-pin select
    // re-keys.
    manager.mark_rate_limited(pinned, LONG_HOLD_SECS);
    let moved = manager
        .select(&tried, now, None, Some(4567), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&4567)
            .map(|&(idx, _)| idx),
        Some(moved),
        "a durable failure still re-keys — the divert-and-keep is self-healing"
    );
}

/// Drive account `idx` over the soft threshold on its MODEL-SCOPED weekly
/// (`7d_oi`, the Fable bucket) with a reset 48h out, leaving every shared
/// dimension healthy: the account is out of Fable and serves everything else
/// perfectly. The companion to [`set_over_threshold`], which moves the SHARED
/// weekly instead.
fn set_fable_exhausted(manager: &Manager, idx: usize, util: f64) {
    let now = OffsetDateTime::now_utc();
    let mut a = manager.accounts.write().expect("accounts lock poisoned");
    a[idx].quota.seven_day_oi = Some(crate::quota::QuotaWindow {
        utilization: util,
        reset: Some(now + Duration::hours(48)),
    });
}

/// Model-scoped exhaustion is a property of the REQUEST CLASS, not of the
/// account: an account out of its Fable weekly still answers Opus perfectly. So a
/// Fable request whose pin is Fable-exhausted diverts THAT ONE request and keeps
/// the pin — the session's next Opus turn must still land on the account holding
/// its warm prefix.
///
/// This is the live shape, not a corner case: every Claude Code session mixes
/// classes (Opus for the conversation, a one-line Fable call for titles and
/// summaries) while a real fleet reads 95-99% on the Fable weekly across nearly
/// every account. Counted as account death, one cheap title request re-keyed a
/// 200k-token conversation onto a cold account.
#[test]
fn fable_exhaustion_diverts_the_request_but_keeps_the_pin() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 909_090u64;
    let home = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("an account is eligible");
    set_fable_exhausted(&manager, home, 0.999);

    let served = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("the other account can still serve Fable");
    assert_ne!(
        served, home,
        "a Fable request must not be served from an exhausted Fable weekly"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(home),
        "model-scoped exhaustion is a per-REQUEST fact — it may divert a \
         request, it may never re-key the session"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None
        ),
        Some(home),
        "the next non-Fable request must come home to the warm prefix"
    );
}

/// THE over-correction guard for the test above: the model gate softened, and
/// nothing else did. Same Fable-exhausted pin, but now the account is also under a
/// 429 hold that outlives its prompt cache — ACCOUNT-level death, which must still
/// re-key durably.
#[test]
fn account_death_still_rekeys_a_fable_session() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 808_080u64;
    let home = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("an account is eligible");
    set_fable_exhausted(&manager, home, 0.999);
    // The account is not merely out of Fable — it is gone for every model class,
    // and for longer than its prompt cache survives.
    manager.mark_rate_limited(home, LONG_HOLD_SECS);

    let served = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("the un-held account serves this request");
    assert_ne!(served, home, "a held pin cannot serve");
    assert_eq!(
        pin_of(&manager, key),
        Some(served),
        "a hold outliving the prompt cache is ACCOUNT-level death — it must \
         still re-key the session"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None
        ),
        Some(served),
        "and durably: the Opus turn stays on the failover, it does not snap back"
    );
}

/// The same invariant one path over. When the whole fleet reads over the SOFT
/// threshold, `select` returns `None` and the request lands in
/// `select_revalidation` — which also re-pins. A Fable request whose pin is
/// Fable-exhausted must be SERVED elsewhere there too while the pin stays put.
///
/// Live-reachable, not theoretical: a fleet at 95-99% Fable has no Fable-eligible
/// account for `select` to pick, so this is the path a title request actually
/// takes.
#[test]
fn revalidation_fable_divert_keeps_the_pin() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 707_070u64;
    let home = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("an account is eligible");
    let other = 1 - home;
    // The whole fleet crosses the SOFT threshold, and the pin is out of Fable.
    set_over_threshold(&manager, home, 0.99, "allowed_warning");
    set_over_threshold(&manager, other, 0.96, "allowed_warning");
    set_fable_exhausted(&manager, home, 0.999);

    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            Some(key),
            "/v1/messages",
            None
        ),
        None,
        "every account is over the soft threshold → normal select benches all"
    );
    assert_eq!(
        manager.select_revalidation(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            Some(key),
            "/v1/messages"
        ),
        Some(other),
        "the revalidation serve routes the Fable request to an account that can \
         answer it"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(home),
        "and it must NOT have re-keyed the session — the pin is model-blocked \
         for this request only"
    );
    // A non-Fable request comes straight home: `select`'s pin-honor path serves
    // the over-threshold pin rather than diverting it.
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None
        ),
        Some(home),
        "the Opus turn is served by the warm pin"
    );
}

/// A rate-limit hold is a TIMER, not a death — and the timers this proxy arms
/// are mostly SHORT: a no-guidance transient 429 parks 15s + jitter, and a
/// `retry-after` park is clamped to 300s (`src/proxy.rs`). Anthropic's default
/// ephemeral prompt cache lives 5 minutes, so re-keying on a 15-second park
/// throws away a prefix that would still be warm 15 seconds later — and the
/// session never returns to it, because a re-key is durable.
///
/// So a hold that clears inside [`CACHE_WARM_HOLD_SECS`] is SOFT: it diverts
/// THIS one request (the account really is parked and would only answer another
/// 429) and leaves the pin alone, and the session comes home warm when the timer
/// runs out. The over-correction guard is `long_hold_rekeys_the_session`, which
/// is this same fixture with only the hold duration changed.
#[test]
fn short_hold_diverts_but_keeps_the_pin() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 515_151u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(home, SHORT_HOLD_SECS);

    let diverted = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(
        diverted, home,
        "a parked pin cannot serve THIS request — serving it only buys a 429"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(home),
        "a hold that clears while the cache is still warm may divert a request, \
         it may never re-key the session"
    );

    // The timer runs out. A past hold reads as expired live, no mutation needed.
    let after = now + Duration::seconds(SHORT_HOLD_SECS + 5);
    for _ in 0..3 {
        assert_eq!(
            manager.select(
                &HashSet::new(),
                after,
                None,
                Some(key),
                "/v1/messages",
                None
            ),
            Some(home),
            "once the hold clears the session must come home to the account \
             still holding its warm prefix"
        );
    }
}

/// THE over-correction guard for the test above: only the SHORT holds softened.
/// The same fixture with a hold that outlives the prompt cache must still re-key
/// durably — by the time that account frees, the prefix it was holding is gone,
/// so keeping the pin would divert through the LRU pick on every turn and
/// scatter the conversation cold across the fleet instead of settling it on one
/// account that can warm up.
#[test]
fn long_hold_rekeys_the_session() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 525_252u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(home, LONG_HOLD_SECS);

    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(served, home, "a parked pin cannot serve");
    assert_eq!(
        pin_of(&manager, key),
        Some(served),
        "a hold that outlives the prompt cache is ACCOUNT-level death — it must \
         re-key the session"
    );
    // Durable: a fresh select with nothing tried stays on the new account.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(served),
        "the session must not snap back to the held account"
    );
}

/// Read the divert episode currently recorded for a session key, if any.
fn divert_episode_of(manager: &Manager, key: u64) -> Option<DivertEpisode> {
    manager
        .divert_ledger
        .lock()
        .expect("divert ledger lock poisoned")
        .get(&key)
        .copied()
}

/// Phase 1's headline claim (the divert-budget design notes §4.3, Phase 1
/// test list): a SECOND divert inside the SAME hold episode reuses the
/// FIRST divert's destination, rather than the normal LRU spread's own
/// anti-churn steering it onto a *different* alternate. Three accounts
/// (not two) so this is a real assertion: with only one alternate
/// available, "the second divert lands on the same account" would be true
/// with or without stickiness.
#[test]
fn sticky_divert_reuses_first_destination() {
    let manager = manager_of(vec![
        account("home", 0),
        account("alt1", 0),
        account("alt2", 0),
    ]);
    let now = OffsetDateTime::now_utc();
    let key = 616_161u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(home, SHORT_HOLD_SECS);

    let first_dest = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an un-held alternate serves this request");
    assert_ne!(first_dest, home, "the held pin cannot serve THIS request");

    // Without stickiness, the normal LRU/priority pick's own anti-churn
    // (`select()`'s doc-comment: "consecutive requests fan out instead of
    // hammering one account") would steer the SECOND divert onto the
    // account that was NOT just selected. With the sticky overlay it must
    // land back on `first_dest` instead.
    let second_dest = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an un-held alternate serves this request");
    assert_eq!(
        second_dest, first_dest,
        "a second divert in the same hold episode must reuse the first \
         divert's destination, not spend a second fresh account"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(home),
        "sticky reuse is a divert, not a re-key — the pin stays home"
    );

    let episode = divert_episode_of(&manager, key).expect("episode recorded");
    assert_eq!(episode.pin, home);
    assert_eq!(episode.sticky, first_dest);
    assert_eq!(
        episode.destinations.count_ones(),
        1,
        "two diverts reusing the SAME destination is one distinct \
         destination, not two"
    );
}

/// Episode boundary (the divert-budget design notes §4.1: "Reset is
/// structural, not timed"): a session that diverts, recovers, and is then
/// held AGAIN on a later, distinct hold must start the second episode
/// with a clean mask — the earlier hold's destination must not leak in as
/// a stale sticky pick for a hold it never happened during.
#[test]
fn new_hold_deadline_resets_the_episode() {
    let manager = manager_of(vec![
        account("home", 0),
        account("alt1", 0),
        account("alt2", 0),
    ]);
    // A single wall-clock read for the whole test — both hold deadlines
    // below are EXPLICIT, fixed offsets from this one `now_ms`, never
    // from `mark_rate_limited` (which takes its own, separate real-clock
    // read via `crate::now_ms()`). Mixing two independent clock reads is
    // exactly what made an earlier version of this test flaky: two reads
    // microseconds apart are USUALLY distinguishable at millisecond
    // resolution, but "usually" is not a gate. Deterministic by
    // construction now, not by luck.
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let key = 626_262u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[home].rate_limited_until_ms = Some(now_ms + SHORT_HOLD_SECS * 1000);
    }
    manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an un-held alternate serves this request");
    let episode_one = divert_episode_of(&manager, key).expect("episode recorded");

    // Let the first hold clear and the session come home.
    let after = now + Duration::seconds(SHORT_HOLD_SECS + 5);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            after,
            None,
            Some(key),
            "/v1/messages",
            None
        ),
        Some(home),
        "the hold cleared — the session returns to its warm pin"
    );

    // A SECOND, later hold on the SAME account: a different
    // `rate_limited_until_ms`, therefore a different episode identity.
    // Still derived from the SAME `now_ms` capture above, at a
    // deliberately distinct explicit offset — see
    // `DivertEpisode::until_ms`'s doc comment for why an ACCIDENTAL
    // collision between two real holds would be benign; this test simply
    // does not rely on one.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[home].rate_limited_until_ms =
            Some(now_ms + (SHORT_HOLD_SECS + 5) * 1000 + SHORT_HOLD_SECS * 1000);
    }
    manager
        .select(
            &HashSet::new(),
            after,
            None,
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("an un-held alternate serves this request");
    let episode_two = divert_episode_of(&manager, key).expect("episode recorded");

    assert_ne!(
        episode_one.until_ms, episode_two.until_ms,
        "two separate holds on the same account carry two different deadlines"
    );
    assert_eq!(
        episode_two.destinations.count_ones(),
        1,
        "the new episode starts with a FULL budget — a clean, single-bit \
         mask, not the first episode's carried-over distinct-destination count"
    );
}

/// Pin a session, park its account with EXACTLY `remaining_ms` left to run, and
/// report whether the next select RE-KEYED the session (`true`) or merely
/// diverted the request while keeping the pin (`false`).
///
/// Writes `rate_limited_until_ms` directly instead of going through
/// `mark_rate_limited`, which anchors the deadline to the process wall clock and
/// therefore cannot express an exact remaining duration relative to the `now`
/// the select is given — the whole point of a boundary test.
fn rekeys_with_hold_remaining(remaining_ms: i64) -> bool {
    rekeys_with_hold_remaining_for(remaining_ms, false)
}

/// [`rekeys_with_hold_remaining`] for a session whose latest request asked
/// for the 1h cache (`extended`), recorded the way the request path records
/// it, through `note_affinity_ttl`, before the hold is weighed.
fn rekeys_with_hold_remaining_for(remaining_ms: i64, extended: bool) -> bool {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let key = 424_242u64;
    manager.note_affinity_ttl(key, extended);
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[home].status = AccountStatus::Throttled;
        accounts[home].rate_limited_until_ms = Some(now_ms + remaining_ms);
    }
    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(
        served, home,
        "a parked pin cannot serve, at any hold length"
    );
    pin_of(&manager, key) != Some(home)
}

/// Pins the dividing line itself, from BOTH sides, so a refactor cannot drift it
/// silently: a hold with exactly [`CACHE_WARM_HOLD_SECS`] left is LONG (the cache
/// dies at the same instant the account frees, so there is nothing to come home
/// to), and one millisecond less is SHORT.
///
/// Asserted through `select`, not against the predicate, so it is the ROUTING
/// decision that is nailed down rather than an implementation detail.
#[test]
fn hold_exactly_at_the_boundary_is_treated_as_long() {
    let boundary_ms = CACHE_WARM_HOLD_SECS * 1000;
    assert!(
        rekeys_with_hold_remaining(boundary_ms),
        "a hold with exactly CACHE_WARM_HOLD_SECS left must re-key — the prefix \
         is gone at the same instant the account frees"
    );
    assert!(
        !rekeys_with_hold_remaining(boundary_ms - 1),
        "one millisecond under the line must keep the pin — the boundary is \
         `>=`, and this is the assertion that stops it drifting"
    );
}

/// A session whose latest request asked for the 1h cache is weighed against
/// its own window ([`WarmWindow::EXTENDED`], 55 minutes), not the 5-minute
/// default: a 20-minute hold re-keys a default session and only diverts a 1h
/// one, whose prefix is still warm when the account frees. The boundary is
/// pinned from both sides the same way the default one is above.
///
/// Watched red on the tree before `warm_window`, where every session got the
/// default: the second assertion failed, the 1h session re-keyed.
#[test]
fn an_extended_ttl_session_keeps_its_pin_through_a_hold_the_default_rekeys_on() {
    let twenty_minutes_ms = 20 * 60 * 1000;
    assert!(
        rekeys_with_hold_remaining_for(twenty_minutes_ms, false),
        "a default session re-keys: 20 minutes outlives a 5-minute cache"
    );
    assert!(
        !rekeys_with_hold_remaining_for(twenty_minutes_ms, true),
        "a 1h session keeps its pin: 20 minutes clears while its cache is warm"
    );
    let boundary_ms = WarmWindow::EXTENDED.ms();
    assert!(
        rekeys_with_hold_remaining_for(boundary_ms, true),
        "a hold exactly as long as the extended window re-keys even a 1h session"
    );
    assert!(
        !rekeys_with_hold_remaining_for(boundary_ms - 1, true),
        "one millisecond under the extended line keeps the pin"
    );
}

/// [`rekeys_with_hold_remaining_for`] for the other timer: the pinned account
/// carries upstream's `rejected` verdict on its 5h window, with that window's
/// reset `remaining_ms` away. No `rate_limited_until_ms` is armed, so the only
/// clock the router can read is the quota's own reset.
fn rekeys_with_rejection_rolling_in(remaining_ms: i64, extended: bool) -> bool {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 535_353u64;
    manager.note_affinity_ttl(key, extended);
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[home].quota.status = Some("rejected".to_string());
        accounts[home].quota.five_hour = Some(crate::quota::QuotaWindow {
            utilization: 1.0,
            reset: Some(now + Duration::milliseconds(remaining_ms)),
        });
    }
    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-rejected account serves this request");
    assert_ne!(
        served, home,
        "a rejected account never serves, whatever its reset"
    );
    pin_of(&manager, key) != Some(home)
}

/// A `rejected` 5h status is a timer, not a death, when its window rolls while
/// the session's cache is still warm: the pin survives and the request diverts,
/// under the session's own window (5 minutes by default, 55 for a 1h session).
/// One that rolls after the cache is cold re-keys, as before.
///
/// Watched red on the tree before `rejection_clears_while_warm`, where
/// `rejected` was terminal under every window: the first and third assertions
/// failed, the session re-keyed.
#[test]
fn a_5h_rejection_rolling_inside_the_warm_window_keeps_the_pin() {
    let four_minutes_ms = 4 * 60 * 1000;
    let twenty_minutes_ms = 20 * 60 * 1000;
    assert!(
        !rekeys_with_rejection_rolling_in(four_minutes_ms, false),
        "default session: a rejection rolling in 4 minutes keeps the pin"
    );
    assert!(
        rekeys_with_rejection_rolling_in(twenty_minutes_ms, false),
        "default session: one rolling in 20 minutes outlives the cache and re-keys"
    );
    assert!(
        !rekeys_with_rejection_rolling_in(twenty_minutes_ms, true),
        "1h session: 20 minutes is inside its 55-minute window, pin kept"
    );
    assert!(
        rekeys_with_rejection_rolling_in(WarmWindow::EXTENDED.ms(), true),
        "1h session: a reset exactly at the window's edge re-keys"
    );
}

/// The 1h rewrite's default follows the fleet: on when every account is
/// OAuth (the window counts a 1h write like a 5m one), off as soon as one
/// row of an unrecognized type is present (a static key that may be billed
/// 2x against 1.25x on every write), and an explicit `cacheTtlRewrite` wins
/// either way. An API-key row cannot be the trigger: it never loads.
///
/// Watched red before the default read the account types: the second
/// assertion failed, a fleet with an API-key account still rewrote.
#[test]
fn cache_ttl_rewrite_defaults_to_the_fleets_account_types() {
    let all_oauth = manager_of(vec![account("a", 0), account("b", 0)]);
    assert!(all_oauth.cache_ttl_rewrite_enabled(), "all OAuth: on");

    let mut api_key = account("k", 0);
    api_key.account_type = config::AccountType::Unrecognized("static".to_string());
    let mixed = manager_of(vec![account("a", 0), api_key.clone()]);
    assert!(
        !mixed.cache_ttl_rewrite_enabled(),
        "one API-key account: off, its writes are billed"
    );

    let mut forced = config_with(vec![api_key]);
    forced
        .extra
        .insert("cacheTtlRewrite".to_string(), serde_json::Value::Bool(true));
    assert!(
        build_manager(forced, counting_refresher()).cache_ttl_rewrite_enabled(),
        "an explicit true overrides the default"
    );

    let mut off = config_with(vec![account("a", 0)]);
    off.extra.insert(
        "cacheTtlRewrite".to_string(),
        serde_json::Value::Bool(false),
    );
    assert!(
        !build_manager(off, counting_refresher()).cache_ttl_rewrite_enabled(),
        "an explicit false overrides the default"
    );
}

/// Once a hold LONG enough to have re-keyed a session has moved it onto a
/// failover account, the expiry of that hold does NOT bring the session home:
/// the pin is simply the failover from then on.
///
/// That is the whole justification for re-keying on a long hold at all. By the
/// time the original account frees, the prefix it was holding is gone
/// ([`CACHE_WARM_HOLD_SECS`]), so coming home would buy a cold start on the way
/// back and no cache hit at the other end — strictly worse than staying put.
///
/// The complement is `short_hold_diverts_but_keeps_the_pin`: a hold that clears
/// while the cache is still warm never re-keys in the first place, so there is
/// nothing to come home FROM. The two together are the whole rule.
#[test]
fn hold_expiry_leaves_the_session_on_its_failover() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 606_060u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(home, LONG_HOLD_SECS);

    let failover = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(
        failover, home,
        "a hold outliving the prompt cache re-keys the session"
    );
    assert_eq!(pin_of(&manager, key), Some(failover));

    // The session keeps working on its failover while the hold runs out, so its
    // cache there stays warm: one more turn a minute before the hold ends. A
    // session idle past its warm window would take the normal pick instead, and
    // that is `a_cold_pin_takes_the_reset_order_again`'s rule, not this one's.
    let last_turn = now + Duration::seconds(LONG_HOLD_SECS - 60);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            last_turn,
            None,
            Some(key),
            "/v1/messages",
            None
        ),
        Some(failover),
        "while the hold is live the session is served on its failover"
    );

    // The hold expires. A past hold reads as expired live, no mutation needed.
    let after = now + Duration::seconds(LONG_HOLD_SECS + 40);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            after,
            None,
            Some(key),
            "/v1/messages",
            None
        ),
        Some(failover),
        "the session stays on its failover once the hold clears — the original \
         account's prompt cache is long gone, so coming home would only buy a \
         second cold start"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(failover),
        "and the pin is not walked back either"
    );
}

/// Distinct session keys fan out to different accounts (each initial pin is a
/// normal LRU pick) and then each key stays on its own account.
#[test]
fn affinity_distinct_keys_fan_out_then_stick() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let x = manager
        .select(&HashSet::new(), now, None, Some(1), "/v1/messages", None)
        .expect("an account is eligible");
    let y = manager
        .select(&HashSet::new(), now, None, Some(2), "/v1/messages", None)
        .expect("an account is eligible");
    assert_ne!(x, y, "distinct keys' initial pins fan out across the tier");
    // Each key repeats onto its own account.
    for _ in 0..3 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(1), "/v1/messages", None),
            Some(x)
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(2), "/v1/messages", None),
            Some(y)
        );
    }
}

/// The affinity map is bounded by `AFFINITY_CAP` + LRU-by-last-touch: filling it
/// past the cap evicts the single oldest-last-touch pin, and a recently-touched
/// pin survives even when it was the oldest by insertion order.
#[test]
fn affinity_map_is_bounded_by_cap_with_lru_eviction() {
    // Mirror of the private `AFFINITY_CAP` in `select`.
    const AFFINITY_CAP: usize = 1024;

    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let base = OffsetDateTime::now_utc();

    // Fill to exactly CAP distinct keys, each stamped at a strictly later
    // last-touch than the previous (key i+1 touched at base + i seconds).
    for i in 0..AFFINITY_CAP {
        let key = (i + 1) as u64;
        manager
            .select(
                &HashSet::new(),
                base + Duration::seconds(i as i64),
                None,
                Some(key),
                "/v1/messages",
                None,
            )
            .expect("an account is eligible");
    }
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .len(),
        AFFINITY_CAP,
        "map fills to exactly CAP without evicting yet (len == CAP, not > CAP)"
    );

    // Refresh the oldest-by-insertion pin (key 1) at the latest time, so its
    // last-touch becomes the newest and it must survive the next eviction. Key 2
    // is now the oldest-last-touch entry.
    manager.select(
        &HashSet::new(),
        base + Duration::seconds((AFFINITY_CAP + 10) as i64),
        None,
        Some(1),
        "/v1/messages",
        None,
    );

    // One more distinct key pushes len over CAP → evict the single oldest.
    manager.select(
        &HashSet::new(),
        base + Duration::seconds((AFFINITY_CAP + 11) as i64),
        None,
        Some((AFFINITY_CAP + 1) as u64),
        "/v1/messages",
        None,
    );

    let pins = manager.affinity.lock().expect("affinity lock poisoned");
    assert_eq!(pins.len(), AFFINITY_CAP, "eviction holds the map at CAP");
    assert!(pins.contains_key(&1), "the recently-touched pin survives");
    assert!(
        !pins.contains_key(&2),
        "the oldest-last-touch pin is evicted"
    );
    assert!(
        pins.contains_key(&((AFFINITY_CAP + 1) as u64)),
        "the just-inserted pin is present"
    );
}

/// Pin session `key` to account `idx` as of `at_ms`, the shape a pin restored from disk
/// at boot takes.
fn seed_pin(manager: &Manager, key: u64, idx: usize, at_ms: i64) {
    manager
        .affinity
        .lock()
        .expect("affinity lock poisoned")
        .insert(key, (idx, at_ms));
}

/// Directly pin `key` to account `idx` in the affinity map (last-touch = now),
/// bypassing a `select()` call — lets a migration test set up an arbitrary stacked
/// starting layout without depending on how the initial pins were formed.
fn pin_session(manager: &Manager, key: u64, idx: usize) {
    seed_pin(manager, key, idx, crate::now_ms());
}

/// Like [`config_with`] but with the (default-OFF) load-balancing migration
/// explicitly enabled — every test that exercises the migration scan itself
/// must opt in, exactly as an operator would via `~/.config/tcr.json`.
fn config_with_migration(accounts: Vec<Account>) -> Config {
    let mut config = config_with(accounts);
    config.extra.insert(
        "loadBalanceMigration".to_string(),
        serde_json::Value::Bool(true),
    );
    config
}

/// Read the account a session key is currently pinned to (None if unpinned).
fn pin_of(manager: &Manager, key: u64) -> Option<usize> {
    manager
        .affinity
        .lock()
        .expect("affinity lock poisoned")
        .get(&key)
        .map(|&(idx, _)| idx)
}

/// Migration #1 — a LONE session is NEVER migrated: its warm cache is preserved
/// even when an idle, eligible account sits right next to it. `count(X) < 2` must
/// be byte-identical to honouring the pin.
#[test]
fn lone_session_never_migrates() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    // One session pinned to acct 0; acct 1 is idle and eligible.
    pin_session(&manager, 100, 0);
    for _ in 0..6 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(100), "/v1/messages", None),
            Some(0),
            "a lone session must stay on its warm account (no migration)"
        );
    }
    assert_eq!(pin_of(&manager, 100), Some(0), "the pin never moved");
}

/// Migration #2 — two sessions stacked on acct 0 with acct 1 idle+eligible and
/// `loadBalanceMigration` ENABLED: selecting for one of them migrates it to
/// acct 1, and the affinity map records the new pin.
#[test]
fn stacked_session_migrates_to_idle() {
    let manager = build_manager(
        config_with_migration(vec![account("a", 0), account("b", 0)]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    pin_session(&manager, 10, 0);
    pin_session(&manager, 11, 0);
    // count(0)=2, count(1)=0 → 0+1 < 2, so migrate this session onto acct 1.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
        Some(1),
        "a stacked session migrates to the idle eligible account"
    );
    assert_eq!(
        pin_of(&manager, 10),
        Some(1),
        "the affinity map now pins the migrated session to acct 1"
    );
}

/// Migration #3 — convergence, no thrash (with `loadBalanceMigration` ENABLED):
/// after #2's migration the layout is balanced (one session each), so further
/// selects for BOTH sessions are stable — neither bounces back.
#[test]
fn migration_converges_no_thrash() {
    let manager = build_manager(
        config_with_migration(vec![account("a", 0), account("b", 0)]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    pin_session(&manager, 10, 0);
    pin_session(&manager, 11, 0);
    // The one migration from #2.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
        Some(1)
    );
    // Now 1-and-1: every further select is a lone session on its account.
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
            Some(1),
            "the migrated session stays put (no bounce back)"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(11), "/v1/messages", None),
            Some(0),
            "the remaining session stays put"
        );
    }
}

/// Migration #4 — no eligible emptier account, no migration: two sessions stack on
/// acct 0 and the only other account is errored, so the stacked session honours
/// its pin rather than migrating onto a throttled/dead account.
#[test]
fn no_migration_when_no_emptier_eligible() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    pin_session(&manager, 20, 0);
    pin_session(&manager, 21, 0);
    manager.mark_error(1); // the only alternative is ineligible
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(20), "/v1/messages", None),
            Some(0),
            "no eligible emptier account → honour the pin, never migrate onto a dead one"
        );
    }
    assert_eq!(pin_of(&manager, 20), Some(0), "the pin never moved");
}

/// Migration #5 — with `loadBalanceMigration` ENABLED, three sessions stacked on
/// acct 0 with accts 1,2 idle spread to one-each after a round of selects
/// (convergence across three accounts).
#[test]
fn three_sessions_spread_across_three_idle() {
    let manager = build_manager(
        config_with_migration(vec![account("a", 0), account("b", 0), account("c", 0)]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    pin_session(&manager, 1, 0);
    pin_session(&manager, 2, 0);
    pin_session(&manager, 3, 0);
    // A round of selects, one per session.
    for key in [1u64, 2, 3] {
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
    }
    let mut homes = [
        pin_of(&manager, 1),
        pin_of(&manager, 2),
        pin_of(&manager, 3),
    ]
    .map(|p| p.expect("each session stays pinned"));
    homes.sort_unstable();
    assert_eq!(
        homes,
        [0, 1, 2],
        "three stacked sessions spread to one-per-account after a round of selects"
    );
}

/// Migration #6 — the DEFAULT is no migration at all. Identical stacked layout to
/// #2 (two sessions on acct 0, acct 1 idle and eligible): with a plain config
/// NEITHER session moves, because a session that already has a pin is by
/// definition warm and Anthropic's prompt cache is per-account, so re-pinning it
/// merely to even out counts throws that cache away. The SAME layout under
/// `"loadBalanceMigration": true` still migrates — proof the behaviour is gated,
/// not deleted.
#[test]
fn migration_is_off_by_default_and_pin_is_honoured() {
    let now = OffsetDateTime::now_utc();

    // --- default config: the migration scan never runs ---
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    assert!(
        !manager.load_balance_migration_enabled(),
        "loadBalanceMigration must default to OFF"
    );
    pin_session(&manager, 30, 0);
    pin_session(&manager, 31, 0);
    // count(0)=2, count(1)=0 — exactly the condition that used to migrate.
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(30), "/v1/messages", None),
            Some(0),
            "a stacked session keeps its warm account when migration is off"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(31), "/v1/messages", None),
            Some(0),
            "its stack-mate keeps the same account too"
        );
    }
    assert_eq!(pin_of(&manager, 30), Some(0), "pin 30 never moved");
    assert_eq!(pin_of(&manager, 31), Some(0), "pin 31 never moved");

    // --- same layout, key explicitly true: the old behaviour is intact ---
    let enabled = build_manager(
        config_with_migration(vec![account("a", 0), account("b", 0)]),
        counting_refresher(),
    );
    assert!(
        enabled.load_balance_migration_enabled(),
        "the explicit key must read back as ON"
    );
    pin_session(&enabled, 30, 0);
    pin_session(&enabled, 31, 0);
    assert_eq!(
        enabled.select(&HashSet::new(), now, None, Some(30), "/v1/messages", None),
        Some(1),
        "with loadBalanceMigration=true the stacked session still migrates"
    );
    assert_eq!(
        pin_of(&enabled, 30),
        Some(1),
        "and the affinity map records the migrated pin"
    );
}

/// When the only future reset across the fleet is the model-scoped weekly
/// (`seven_day_oi`) window — the bucket Fable requests gate on —
/// `retry_after_hint` advertises that reset, not the 60s fallthrough default.
#[test]
fn retry_after_hint_uses_seven_day_oi_window() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();
    // Only the 7d_oi window has a future reset (~2h out); five_hour / seven_day /
    // rate-limit hold are all absent, so before the fix the hint fell through to 60s.
    let reset_ms = odt_to_ms(now) + 2 * 3_600_000;
    manager.apply_usage(
        0,
        &crate::probe::Usage {
            five_hour: crate::probe::WindowReading::Unread,
            seven_day: None,
            seven_day_oi: Some(UsageBucket {
                utilization: Some(0.99),
                reset_at_ms: Some(reset_ms),
            }),
            extra_usage_usd: None,
            codex: None,
            claude_resets: None,
        },
    );
    // A FABLE request (`is_fable = true`) is the only one gated by the 7d_oi
    // bucket, so it is what makes that window drive the hint. A non-Fable
    // request ignores 7d_oi entirely and falls through to the 60s default.
    let hint = manager.retry_after_hint(now, true, crate::provider::Provider::Anthropic);
    assert!(
        hint > 60,
        "the 7d_oi reset must drive the hint past the 60s default, got {hint}"
    );
    let expected = ((reset_ms - odt_to_ms(now) + 999) / 1000).max(1);
    assert_eq!(
        hint, expected,
        "hint equals the 7d_oi reset delta in seconds"
    );
    // The general (non-Fable) view does NOT see the Fable-only weekly, so with
    // no other gate the hint falls back to the 60s default — proof the
    // `is_fable` scoping is honoured, not hard-coded.
    assert_eq!(
        manager.retry_after_hint(now, false, crate::provider::Provider::Anthropic),
        60,
        "a non-Fable request ignores the 7d_oi bucket"
    );
}

/// A quota window at `util`, resetting at `reset` (`None` = unknown reset).
fn window(util: f64, reset: Option<OffsetDateTime>) -> crate::quota::QuotaWindow {
    crate::quota::QuotaWindow {
        utilization: util,
        reset,
    }
}

/// A fresh active runtime with empty quota, for the [`Manager::account_gate`] tests.
fn gate_runtime() -> AccountRuntime {
    AccountRuntime::from_config(&account("gate", 0), false)
}

/// One fixture for [`gate_and_hard_ok_agree_on_every_variant`]: a runtime
/// that exhibits one [`GateReason`], whether that block is ACCOUNT-level
/// (`account_hard_ok == false`) or request-scoped, and the two group sets
/// the request is judged against.
struct GateCase {
    label: &'static str,
    runtime: AccountRuntime,
    account_level: bool,
    reserved: HashSet<String>,
    parked: HashSet<String>,
}

impl GateCase {
    /// The common shape: the runtime alone exhibits the gate, with nothing
    /// reserved and nothing parked. Only `Reserved` and `Parked` — the two
    /// gates that depend on more than the runtime — build one by hand.
    fn plain(label: &'static str, runtime: AccountRuntime, account_level: bool) -> Self {
        Self {
            label,
            runtime,
            account_level,
            reserved: HashSet::new(),
            parked: HashSet::new(),
        }
    }
}

#[test]
fn account_gate_ok_when_healthy() {
    let now = OffsetDateTime::now_utc();
    let a = gate_runtime();
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Ok, None)
    );
}

#[test]
fn account_gate_disabled_and_error_never_self_free() {
    let now = OffsetDateTime::now_utc();
    // Even with a gating window present, the terminal states dominate and carry
    // NO clear-instant — they never self-free.
    let mut disabled = gate_runtime();
    disabled.disabled = true;
    disabled.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
    assert_eq!(
        Manager::account_gate(
            &disabled,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Disabled, None)
    );

    let mut errored = gate_runtime();
    errored.status = AccountStatus::Error;
    errored.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
    assert_eq!(
        Manager::account_gate(
            &errored,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Login, None)
    );
}

#[test]
fn account_gate_binds_on_latest_clearing_window() {
    // 5h clears SOON, 7d clears LATER — the account frees only when BOTH clear,
    // so the reason is the later 7d gate and free_at is its (max) reset. A naive
    // "soonest reset" would wrongly report the 5h instant.
    let now = OffsetDateTime::now_utc();
    let soon = now + Duration::seconds(300);
    let later = now + Duration::seconds(5_000);
    let mut a = gate_runtime();
    a.quota.five_hour = Some(window(0.99, Some(soon)));
    a.quota.seven_day = Some(window(0.99, Some(later)));
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::SevenDay, Some(later))
    );
}

#[test]
fn account_gate_unknown_reset_sorts_latest_and_hides_time() {
    // A window over threshold whose reset is unknown (None) is the longest
    // possible constraint: it becomes the reason and free_at is None (no promise
    // of a time), even beside a window with a known soon reset.
    let now = OffsetDateTime::now_utc();
    let mut a = gate_runtime();
    a.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
    a.quota.seven_day = Some(window(0.99, None));
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::SevenDay, None)
    );
}

#[test]
fn account_gate_fable_weekly_only_gates_fable() {
    // The 7d_oi bucket gates a Fable evaluation and is invisible to the general
    // one — the exact `is_fable` split `eligible` makes.
    let now = OffsetDateTime::now_utc();
    let reset = now + Duration::seconds(4_000);
    let mut a = gate_runtime();
    a.quota.seven_day_oi = Some(window(0.99, Some(reset)));
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Ok, None),
        "the non-Fable view ignores the model-scoped weekly"
    );
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            true,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::FableWeekly, Some(reset)),
        "a Fable evaluation gates on it"
    );
}

/// A near standard (API-key) token limit with a future `standard_reset` and no
/// other gate is reported as `(Standard, Some(reset))` — `account_gate` now sees
/// the same dimension `eligible`/`is_near` enforces.
#[test]
fn account_gate_gates_a_near_standard_limit() {
    let now = OffsetDateTime::now_utc();
    let reset = now + Duration::seconds(600);
    let mut a = gate_runtime();
    a.quota.tokens_limit = Some(1_000);
    a.quota.tokens_remaining = Some(50); // 95% spent, over the 0.90 threshold
    a.quota.standard_reset = Some(reset);
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Standard, Some(reset))
    );
}

/// The exact G2 scenario: an account with BOTH a short transient Hold (+8s) AND
/// a near standard limit (reset +600s) must bind on the later-clearing Standard
/// gate, so `retry_after_hint` reports the true 600s recovery — not the 8s Hold
/// that would trigger a soft-wait then hard-fail anyway.
#[test]
fn account_gate_standard_reset_outlasts_a_short_hold() {
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let reset = now + Duration::seconds(600);
    let mut a = AccountRuntime::from_config(&account("std-hold", 0), false);
    a.switch_threshold = Some(0.90);
    a.rate_limited_until_ms = Some(now_ms + 8_000); // short Hold, +8s
    a.quota.requests_limit = Some(200);
    a.quota.requests_remaining = Some(5); // 97.5% spent
    a.quota.standard_reset = Some(reset);

    // The Standard gate (later reset) wins max_by_key over the +8s Hold.
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            now_ms,
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Standard, Some(reset)),
        "the standard reset outlasts the short hold"
    );

    // retry_after_hint must now reflect the real 600s, not the 8s Hold.
    let manager = Manager::from_runtimes(vec![a]);
    let hint = manager.retry_after_hint(now, false, crate::provider::Provider::Anthropic);
    let expected = ((odt_to_ms(reset) - now_ms + 999) / 1000).max(1);
    assert_eq!(hint, expected, "hint must be the 600s reset, got {hint}");
    assert!(hint > 500, "must not report the 8s hold, got {hint}");
}

/// G-a: an OAuth account (all standard fields `None`) produces NO Standard gate,
/// so `account_gate` is byte-identical to its pre-fix behavior for OAuth.
#[test]
fn account_gate_ignores_standard_for_oauth() {
    let now = OffsetDateTime::now_utc();
    let a = gate_runtime(); // all standard fields None by default
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Ok, None),
        "OAuth accounts never gate on the standard dimension"
    );
}

/// An expired `standard_reset` (in the PAST) means the upstream limit has
/// refreshed, so a spent count must NOT keep gating — mirrors `is_near`'s expiry
/// rule so a spent standard window never pins an API-key account out forever.
#[test]
fn account_gate_ignores_expired_standard() {
    let now = OffsetDateTime::now_utc();
    let mut a = gate_runtime();
    a.quota.tokens_limit = Some(1_000);
    a.quota.tokens_remaining = Some(10); // 99% spent, over threshold
    a.quota.standard_reset = Some(now - Duration::seconds(1)); // already refreshed
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Ok, None),
        "an expired standard window no longer gates"
    );
}

#[test]
fn rejected_account_reports_a_rejected_gate() {
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let mut a = gate_runtime();
    a.quota.status = Some("rejected".to_string());

    // Was `Ok`: `account_hard_ok` held the account out on `rejected` while
    // `account_gate` had no arm for it, so the TUI showed a rejected account as
    // healthy and in rotation.
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            now_ms,
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Rejected, None)
    );
    assert!(
        !Manager::account_hard_ok(
            &a,
            now_ms,
            None,
            &HashSet::new(),
            &HashSet::new(),
            WarmWindow::DEFAULT
        ),
        "and it stays hard-gated, exactly as before"
    );

    // Terminal, so it dominates a live window and carries NO clear-instant —
    // `retry_after_hint` reads `free_at`, and a rejected account was never going
    // to come back at its 5h reset.
    a.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            now_ms,
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Rejected, None),
        "a rejected account must not advertise a window reset as its recovery"
    );
}

/// A refused account as the snapshot sees it: `provider`, `rejected`, and the two windows
/// as (utilization, reset from `now`). Thresholds are the default 0.95.
fn refused_runtime(
    provider: crate::provider::Provider,
    now: OffsetDateTime,
    five_hour: Option<(f64, Duration)>,
    seven_day: Option<(f64, Duration)>,
) -> AccountRuntime {
    let window = |(utilization, reset_in): (f64, Duration)| crate::quota::QuotaWindow {
        utilization,
        reset: Some(now + reset_in),
    };
    let mut account = AccountRuntime::from_config(&account("refused", 0), false);
    account.provider = provider;
    account.quota.status = Some("rejected".into());
    account.quota.five_hour = five_hour.map(window);
    account.quota.seven_day = seven_day.map(window);
    account
}

/// A manager over one refused Codex account, as [`refused_runtime`] builds it.
fn refused_codex_manager(
    now: OffsetDateTime,
    five_hour: Option<(f64, Duration)>,
    seven_day: Option<(f64, Duration)>,
) -> Arc<Manager> {
    Manager::from_runtimes(vec![refused_runtime(
        crate::provider::Provider::Codex,
        now,
        five_hour,
        seven_day,
    )])
}

/// A manager over one refused Codex account whose windows are both half spent, so no
/// window explains the refusal.
fn codex_refused_with_neither_window_full(now: OffsetDateTime) -> Arc<Manager> {
    refused_codex_manager(
        now,
        Some((0.5, Duration::hours(2))),
        Some((0.5, Duration::days(3))),
    )
}

/// The (gate, free-at in Unix seconds) the snapshot reports for account 0.
fn reported_gate(manager: &Manager, now: OffsetDateTime) -> (GateReason, Option<i64>) {
    let row = &manager.snapshot(now).accounts[0];
    (row.gate, row.free_at.map(OffsetDateTime::unix_timestamp))
}

/// **A Codex account at its weekly limit reports `SevenDay` and the weekly reset**, not a
/// `Rejected` with no time: ChatGPT's usage-limit refusal names the window, and the window
/// outranks the hour-long hold the refusal also armed.
///
/// Watched red: return the input unchanged from `codex_refusal_gate` and this reads
/// `Rejected`.
#[test]
fn a_weekly_refusal_reports_seven_day_and_its_reset() {
    let now = OffsetDateTime::now_utc();
    let manager = refused_codex_manager(
        now,
        Some((0.3, Duration::hours(2))),
        Some((1.0, Duration::days(3))),
    );
    manager.mark_rate_limited(0, 3600);
    assert_eq!(
        reported_gate(&manager, now),
        (
            GateReason::SevenDay,
            Some((now + Duration::days(3)).unix_timestamp())
        )
    );
}

/// The five-hour window that refused an account is its gate, with that window's reset. With
/// both windows full, both have to roll before it serves, so the later reset, the weekly
/// one, is the gate.
///
/// Watched red: take the earliest reset instead of the latest and the both-full case reads
/// `FiveHour`.
#[test]
fn a_five_hour_refusal_reports_five_hour() {
    let now = OffsetDateTime::now_utc();
    let manager = refused_codex_manager(
        now,
        Some((1.0, Duration::minutes(52))),
        Some((0.17, Duration::days(5))),
    );
    assert_eq!(
        reported_gate(&manager, now),
        (
            GateReason::FiveHour,
            Some((now + Duration::minutes(52)).unix_timestamp())
        )
    );

    let both_full = refused_codex_manager(
        now,
        Some((1.0, Duration::hours(1))),
        Some((1.0, Duration::days(3))),
    );
    assert_eq!(
        reported_gate(&both_full, now),
        (
            GateReason::SevenDay,
            Some((now + Duration::days(3)).unix_timestamp())
        ),
        "both full: the later reset is when the account serves again"
    );
}

/// A refusal no full window explains (credits, a spend cap) but that armed a hold reports
/// the hold, until the hold's end.
#[test]
fn a_refusal_with_no_full_window_holds_until_its_reset() {
    let now = OffsetDateTime::now_utc();
    let manager = codex_refused_with_neither_window_full(now);
    manager.mark_rate_limited(0, 1800);
    let row = manager.snapshot(now).accounts[0].clone();
    assert!(row.rate_limited_until.is_some(), "the hold is armed");
    assert_eq!(row.gate, GateReason::Hold);
    assert_eq!(row.free_at, row.rate_limited_until);
}

/// **A standing refusal whose live hold an upstream 403 armed reports `Refused`**, the gate
/// a 403's hold reads on every other row, until the hold's end. A refused Codex account is
/// back in the pool once the refusal's own hold lapses, since the pick reads the hold and
/// the windows and never `rejected`, so the live hold can be a 403's
/// (`Manager::mark_refused`). The remap reports a live hold under the name `account_gate`
/// gives it.
///
/// Watched red: report every live hold as `Hold`, and this reads `Hold`.
#[test]
fn a_refusal_held_by_an_upstream_403_reports_refused() {
    let now = OffsetDateTime::now_utc();
    let manager = codex_refused_with_neither_window_full(now);
    manager.mark_refused(0);
    let row = manager.snapshot(now).accounts[0].clone();
    assert!(row.rate_limited_until.is_some(), "the 403's hold is armed");
    assert_eq!(row.gate, GateReason::Refused);
    assert_eq!(row.free_at, row.rate_limited_until);
}

/// **The window that refused an account keeps its gate past its reset, with no time, until a
/// probe confirms the roll.** The clock says the window has rolled; a usage probe, or a
/// served 2xx that carries a window, ends the refusal. Reading `Rejected` in between would
/// turn a spent row into a refused one for the minutes until the next probe. The hold the
/// refusal armed, to that same reset, has lapsed by then too. Weekly and five-hour alike.
///
/// Watched red: without the rolled-window rule, past the reset this reads
/// `(Rejected, None)`.
#[test]
fn a_refusing_window_past_its_reset_keeps_its_gate_until_a_probe_confirms_the_roll() {
    let now = OffsetDateTime::now_utc();
    let weekly = refused_codex_manager(now, None, Some((1.0, Duration::minutes(1))));
    weekly.mark_rate_limited(0, 60);
    assert_eq!(
        reported_gate(&weekly, now),
        (
            GateReason::SevenDay,
            Some((now + Duration::minutes(1)).unix_timestamp())
        ),
        "before the reset: the window, and its reset"
    );
    assert_eq!(
        reported_gate(&weekly, now + Duration::minutes(2)),
        (GateReason::SevenDay, None),
        "past the reset, no probe yet: the same window, with no time"
    );

    let five_hour = refused_codex_manager(
        now,
        Some((1.0, Duration::minutes(1))),
        Some((0.3, Duration::days(5))),
    );
    assert_eq!(
        reported_gate(&five_hour, now + Duration::minutes(2)),
        (GateReason::FiveHour, None)
    );
}

/// With neither a full window nor a hold, nothing names a time: still `Rejected`.
#[test]
fn a_refusal_with_neither_stays_rejected() {
    let now = OffsetDateTime::now_utc();
    let manager = codex_refused_with_neither_window_full(now);
    assert_eq!(reported_gate(&manager, now), (GateReason::Rejected, None));
}

/// Anthropic's `rejected` is its own verdict and names no time, so its row is exactly what
/// `account_gate` says, a full weekly window or not.
#[test]
fn an_anthropic_rejected_row_is_unchanged() {
    let now = OffsetDateTime::now_utc();
    let manager = Manager::from_runtimes(vec![refused_runtime(
        crate::provider::Provider::Anthropic,
        now,
        None,
        Some((1.0, Duration::days(3))),
    )]);
    assert_eq!(reported_gate(&manager, now), (GateReason::Rejected, None));
}

/// The drift guard, and the reason R7 exists. [`Manager::account_gate`] (what
/// the TUI and `retry_after_hint` read) and [`Manager::account_hard_ok`] (whether
/// a session may lose its pin) once kept two hand-maintained gate lists, and they
/// drifted: `rejected` was on one and missing from the other.
///
/// The two are deliberately NOT equal — a quota window or a short hold gates the
/// display while leaving the pin alone — so this pins the RELATIONSHIP per
/// variant instead. The classification below is exhaustive over [`GateReason`],
/// so adding a variant stops compiling until someone decides which side of the
/// account/request line it falls on.
#[test]
fn gate_and_hard_ok_agree_on_every_variant() {
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let reset = now + Duration::seconds(5_000);

    const ALL: [GateReason; 12] = [
        GateReason::Ok,
        GateReason::Hold,
        GateReason::Refused,
        GateReason::FiveHour,
        GateReason::SevenDay,
        GateReason::FableWeekly,
        GateReason::Standard,
        GateReason::Login,
        GateReason::Disabled,
        GateReason::Rejected,
        GateReason::Reserved,
        GateReason::Parked,
    ];

    for reason in ALL {
        // Only a Fable-scoped evaluation can ever surface the model-scoped gate.
        let is_fable = reason == GateReason::FableWeekly;

        // Per case: a label, a runtime that actually exhibits `reason`, whether
        // that block is ACCOUNT-level (`account_hard_ok == false`) or
        // request-scoped (`account_hard_ok` stays true, the pin survives), and
        // and the reserved / parked group sets the request evaluates
        // against (both empty for every reason but `Reserved` and `Parked`
        // themselves, the two whose gate depends on more than the runtime
        // alone).
        let cases: Vec<GateCase> = match reason {
            GateReason::Ok => vec![GateCase::plain("healthy", gate_runtime(), true)],

            // Terminal: a fact about the credential, for every model class.
            GateReason::Disabled => {
                let mut a = gate_runtime();
                a.disabled = true;
                vec![GateCase::plain("operator-disabled", a, false)]
            }
            GateReason::Login => {
                let mut a = gate_runtime();
                a.status = AccountStatus::Error;
                vec![GateCase::plain("dead credential", a, false)]
            }
            GateReason::Rejected => {
                let mut a = gate_runtime();
                a.quota.status = Some("rejected".to_string());
                vec![GateCase::plain("upstream rejected", a, false)]
            }
            // Reservation is not a preference: unrequested traffic (`group:
            // None`, matching every other case in this loop) against an
            // account in a reserved group is ACCOUNT-level-blocked, exactly
            // like the terminal gates above — see `Self::reserved_blocks`.
            GateReason::Reserved => {
                let mut a = gate_runtime();
                a.groups = vec!["codereview".to_string()];
                vec![GateCase {
                    label: "reserved group, unrequested traffic",
                    runtime: a,
                    account_level: false,
                    reserved: ["codereview".to_string()].into(),
                    parked: HashSet::new(),
                }]
            }
            // Parking is not a preference either, and unlike reservation it
            // does not even depend on the request's ask: a member of a
            // parked group is ACCOUNT-level-blocked for every request —
            // see `Self::parked_blocks`.
            GateReason::Parked => {
                let mut a = gate_runtime();
                a.groups = vec!["codereview".to_string()];
                vec![GateCase {
                    label: "parked group",
                    runtime: a,
                    account_level: false,
                    reserved: HashSet::new(),
                    parked: ["codereview".to_string()].into(),
                }]
            }

            // The one reason that splits on DURATION: past the cache TTL a hold
            // is account death, under it a timer worth keeping the pin for.
            GateReason::Hold => {
                let mut long = gate_runtime();
                long.rate_limited_until_ms = Some(now_ms + (CACHE_WARM_HOLD_SECS + 60) * 1_000);
                let mut short = gate_runtime();
                short.rate_limited_until_ms = Some(now_ms + 30_000);
                vec![
                    GateCase::plain("hold outliving the cache", long, false),
                    GateCase::plain("hold clearing while warm", short, true),
                ]
            }
            // A 403 refusal hold rides the same deadline as a rate-limit hold,
            // so it splits on duration exactly the same way; only its name
            // differs.
            GateReason::Refused => {
                let mut long = gate_runtime();
                long.rate_limited_until_ms = Some(now_ms + (CACHE_WARM_HOLD_SECS + 60) * 1_000);
                long.hold_cause = HoldCause::Refused;
                let mut short = gate_runtime();
                short.rate_limited_until_ms = Some(now_ms + 30_000);
                short.hold_cause = HoldCause::Refused;
                vec![
                    GateCase::plain("refusal hold outliving the cache", long, false),
                    GateCase::plain("refusal hold clearing while warm", short, true),
                ]
            }

            // Windows are per-request facts: they gate the display and every
            // serve decision, but must never move a session's pin.
            GateReason::FiveHour => {
                let mut a = gate_runtime();
                a.quota.five_hour = Some(window(0.99, Some(reset)));
                vec![GateCase::plain("5h over threshold", a, true)]
            }
            GateReason::SevenDay => {
                let mut a = gate_runtime();
                a.quota.seven_day = Some(window(0.99, Some(reset)));
                vec![GateCase::plain("7d over threshold", a, true)]
            }
            GateReason::FableWeekly => {
                let mut a = gate_runtime();
                a.quota.seven_day_oi = Some(window(0.99, Some(reset)));
                vec![GateCase::plain("7d_oi over threshold", a, true)]
            }
            GateReason::Standard => {
                let mut a = gate_runtime();
                a.quota.tokens_limit = Some(1_000);
                a.quota.tokens_remaining = Some(10); // 99% spent
                a.quota.standard_reset = Some(reset);
                vec![GateCase::plain("standard limit spent", a, true)]
            }
        };

        for case in cases {
            let GateCase {
                label,
                runtime,
                account_level,
                reserved,
                parked,
            } = case;
            let (gate, _) = Manager::account_gate(
                &runtime, 0.90, None, now, now_ms, is_fable, None, &reserved, &parked,
            );
            assert_eq!(gate, reason, "fixture `{label}` must exhibit {reason:?}");

            let hard_ok = Manager::account_hard_ok(
                &runtime,
                now_ms,
                None,
                &reserved,
                &parked,
                WarmWindow::DEFAULT,
            );
            assert_eq!(
                hard_ok, account_level,
                "`{label}`: account_hard_ok disagrees with {reason:?}'s classification"
            );

            // The invariant that actually broke: whatever `account_hard_ok` holds
            // out MUST have a reason to show for it. `rejected` violated this.
            if !hard_ok {
                assert_ne!(
                    gate,
                    GateReason::Ok,
                    "`{label}` is hard-gated but renders Ok — the gate lists have drifted"
                );
            }
        }
    }
}

/// The serving client must never carry a TOTAL request timeout: these responses
/// are long-lived SSE streams, and a total timeout truncates them mid-stream.
/// (The `connect_timeout` it does set is not observable here — `reqwest::Client`
/// exposes no getters and its `Debug` omits it, since the value lives inside the
/// connector. Only the total timeout is surfaced.)
#[test]
fn serving_client_has_no_total_timeout() {
    let manager = manager_of(vec![account("a", 0)]);

    // reqwest prints the total timeout as a `TotalTimeout` field, and only when
    // one is set. Prove the marker on a control client first, so a future reqwest
    // that renames it fails HERE loudly instead of making the guard below pass
    // vacuously.
    let control = crate::http_client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("build control client");
    assert!(
        format!("{control:?}").contains("TotalTimeout"),
        "reqwest no longer reports a total timeout in Debug — this guard needs rewriting"
    );

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert!(
        !format!("{:?}", accounts[0].http).contains("TotalTimeout"),
        "the serving client grew a total timeout; it will truncate SSE streams"
    );
}

/// The connection-pool isolation this whole module exists for: two accounts
/// must never be handed the SAME client. `hyper-util` keys its pool on
/// `(scheme, authority)` alone — nothing about the Bearer token or account
/// identity — so a client shared across accounts collapses every account
/// onto one pooled connection, and that connection's death takes every
/// account down with it regardless of how healthy any individual account is
/// (see [`AccountRuntime::http`]'s doc comment for the measured incident).
///
/// This is an IN-PROCESS identity check, not a live network measurement —
/// `Arc::ptr_eq` proves two accounts never share the same client instance
/// (and thus never share its connection pool), and that repeated lookups of
/// the SAME account keep returning the SAME instance rather than a fresh
/// one that would throw away its warm pool. It does not, and cannot, prove
/// two real TCP connections were opened — that would need an actual
/// upstream to observe.
#[test]
fn different_accounts_never_share_a_serving_client() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);

    let client_a = manager.http_client(0).expect("account 0 exists");
    let client_b = manager.http_client(1).expect("account 1 exists");
    assert!(
        !Arc::ptr_eq(&client_a, &client_b),
        "two different accounts were handed the same client — a dead \
         connection on one account's pool would take every other account \
         down with it"
    );

    // The SAME account, looked up again, must return the SAME instance —
    // a fresh client per lookup would defeat the whole point (a cold pool
    // on every single request), even though it would still pass the
    // distinctness assertion above.
    let client_a_again = manager.http_client(0).expect("account 0 exists");
    assert!(
        Arc::ptr_eq(&client_a, &client_a_again),
        "the same account's client must be stable across lookups, not \
         rebuilt per call"
    );
}

/// Three accounts out of rotation at `now`, each with a 5-hour reset that misleads
/// differently:
///
/// - A, weekly-gated: its 5h resets SOON (200s) but its 7d stays over threshold until
///   5000s, so A does NOT actually return until 5000s.
/// - B, a dead credential (Error) that holds the SOONEST raw reset (100s). It never
///   self-frees, so it must contribute nothing to the hint.
/// - C, the TRUE first recovery: 5h-gated with a later reset (900s) and a healthy 7d,
///   so it genuinely returns at 900s.
fn weekly_gated_dead_and_five_hour_gated(now: OffsetDateTime) -> Arc<Manager> {
    let at = |secs: i64| now + Duration::seconds(secs);
    let mut a = AccountRuntime::from_config(&account("a", 0), false);
    a.switch_threshold = Some(0.90);
    a.quota.five_hour = Some(window(0.99, Some(at(200))));
    a.quota.seven_day = Some(window(0.99, Some(at(5_000))));
    let mut b = AccountRuntime::from_config(&account("b", 0), false);
    b.switch_threshold = Some(0.90);
    b.status = AccountStatus::Error;
    b.quota.five_hour = Some(window(0.99, Some(at(100))));
    let mut c = AccountRuntime::from_config(&account("c", 0), false);
    c.switch_threshold = Some(0.90);
    c.quota.five_hour = Some(window(0.99, Some(at(900))));
    Manager::from_runtimes(vec![a, b, c])
}

/// The live-incident shape: a fleet-exhausted 429's `retry-after` must name the
/// TRUE soonest recovery, never an earlier reset belonging to an account that
/// stays gated for another reason or never self-frees at all.
#[test]
fn retry_after_hint_skips_error_and_still_gated_accounts() {
    let now = OffsetDateTime::now_utc();
    let at = |secs: i64| now + Duration::seconds(secs);
    let manager = weekly_gated_dead_and_five_hour_gated(now);
    let hint = manager.retry_after_hint(now, false, crate::provider::Provider::Anthropic);

    // Correct = C's 900s reset. The pre-fix raw-min over every window's reset
    // returned ~100s: it counted B's reset (an Error account that never returns)
    // and A's 200s 5h reset (A stays gated on its weekly until 5000s). Both are
    // now skipped, so the min of the real free_at instants is C's 900s.
    let expected = ((odt_to_ms(at(900)) - odt_to_ms(now) + 999) / 1000).max(1);
    assert_eq!(hint, expected, "hint must be C's 900s reset, got {hint}");
    assert!(
        hint > 800,
        "must not report B's 100s or A's 200s reset, got {hint}"
    );
}

/// The full hint: the gate that TIMES the wait, and a count of every account
/// out of rotation by its gate. Same fleet as the test above, so the binding
/// gate is C's 5-hour window, while A's weekly and B's dead credential are
/// counted without being allowed to drive the time.
#[test]
fn exhaustion_hint_names_the_binding_gate_and_counts_every_gated_account() {
    let now = OffsetDateTime::now_utc();
    let at = |secs: i64| now + Duration::seconds(secs);
    let manager = weekly_gated_dead_and_five_hour_gated(now);
    let hint = manager.exhaustion_hint(now, false, crate::provider::Provider::Anthropic, None);
    assert_eq!(
        hint.binding,
        Some(GateReason::FiveHour),
        "C's 5h window times the wait"
    );
    assert_eq!(
        hint.free_at.map(odt_to_ms),
        Some(odt_to_ms(at(900))),
        "free_at is C's reset instant"
    );
    assert_eq!(
        hint.retry_after,
        manager.retry_after_hint(now, false, crate::provider::Provider::Anthropic),
        "the seconds half is byte-identical to retry_after_hint"
    );
    let counts: Vec<(GateReason, usize)> = hint.gated.iter().map(|(r, n)| (*r, *n)).collect();
    assert_eq!(
        counts,
        vec![
            (GateReason::FiveHour, 1),
            (GateReason::SevenDay, 1),
            (GateReason::Login, 1)
        ],
        "every gated account is counted under the gate that binds IT"
    );
}

/// When no account advertises a reset, the number stays the 60 sentinel the
/// proxy's soft-wait relies on, but `free_at`/`binding` are `None` so a
/// message can say "no reset known" instead of printing 60 as a measurement.
#[test]
fn exhaustion_hint_reports_an_unknown_reset_as_unknown_not_as_sixty_seconds() {
    let now = OffsetDateTime::now_utc();
    let mut a = AccountRuntime::from_config(&account("a", 0), false);
    a.switch_threshold = Some(0.90);
    a.quota.seven_day = Some(window(0.99, None));
    let manager = Manager::from_runtimes(vec![a]);
    let hint = manager.exhaustion_hint(now, false, crate::provider::Provider::Anthropic, None);
    assert_eq!(hint.retry_after, 60);
    assert_eq!(hint.free_at, None);
    assert_eq!(hint.binding, None);
    assert_eq!(hint.gated.get(&GateReason::SevenDay), Some(&1));
}

/// A request that only account 1 may serve (bound history, see `crate::bound_tokens`) is
/// told when account 1 frees — 7 seconds — not when the SOONER account 0 does. The
/// fleet-wide hint over the same fleet is the control: it answers 2.
#[test]
fn a_single_account_hint_times_the_wait_on_that_account_alone() {
    let manager = Manager::from_runtimes(vec![
        AccountRuntime::from_config(&account("a", 0), false),
        AccountRuntime::from_config(&account("b", 0), false),
    ]);
    manager.mark_rate_limited(0, 2);
    manager.mark_rate_limited(1, 7);
    // Read AFTER the marks, so each hold has at most its own length left and rounds up to
    // exactly that many whole seconds.
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager
            .exhaustion_hint(now, false, crate::provider::Provider::Anthropic, None)
            .retry_after,
        2,
        "the control: fleet-wide, the sooner account wins"
    );
    let hint = manager.exhaustion_hint_for_account(now, false, 1);
    assert_eq!(
        hint.retry_after, 7,
        "bound to account 1, the wait is ITS hold: {hint:?}"
    );
    assert_eq!(hint.binding, Some(GateReason::Hold));
    assert_eq!(
        hint.gated.get(&GateReason::Hold),
        Some(&1),
        "only the one account is counted: {hint:?}"
    );
}

/// **A 429's wait is timed on the request's own provider.** An account of the
/// other provider can never serve the request, so its reset says nothing about when this
/// one can be served: a Claude fleet out for seven seconds is not "back in two" because
/// a Codex account is, nor the reverse. Both orders, every entry point (the fleet hint,
/// its seconds half, and the group form a strict request uses), and the gate count names
/// only the provider's own account.
///
/// Watched red: drop the provider filter in `exhaustion_hint_over` and the sooner account
/// wins for both providers.
#[test]
fn codex_exhaustion_hint_folds_only_the_requests_own_provider() {
    use crate::provider::Provider;
    for (anthropic_hold, codex_hold) in [(7, 2), (2, 7)] {
        let mut codex = AccountRuntime::from_config(&account_in_groups("b", 0, &["g"]), false);
        codex.provider = Provider::Codex;
        let manager = Manager::from_runtimes(vec![
            AccountRuntime::from_config(&account_in_groups("a", 0, &["g"]), false),
            codex,
        ]);
        manager.mark_rate_limited(0, anthropic_hold);
        manager.mark_rate_limited(1, codex_hold);
        // Read AFTER the marks, so each hold rounds up to exactly its own length.
        let now = OffsetDateTime::now_utc();
        for (provider, hold) in [
            (Provider::Anthropic, anthropic_hold),
            (Provider::Codex, codex_hold),
        ] {
            let hint = manager.exhaustion_hint(now, false, provider, None);
            assert_eq!(
                hint.retry_after,
                hold,
                "{} is timed on its own account: {hint:?}",
                provider.as_str()
            );
            assert_eq!(
                hint.gated.get(&GateReason::Hold),
                Some(&1),
                "only {}'s own account is counted: {hint:?}",
                provider.as_str()
            );
            assert_eq!(manager.retry_after_hint(now, false, provider), hold);
            assert_eq!(
                manager.retry_after_hint_for_group(now, false, provider, "g"),
                hold,
                "the group form folds the provider too"
            );
        }
    }
}

/// **A Codex 429's wait is the earliest Codex window reset**, not the 60-second
/// placeholder: once `x-codex-*` headers fill a Codex account's weekly window, the
/// provider-scoped hint folds it like any other window over threshold. Driven through
/// `update_quota` with the shape the real backend sends (only the weekly window, in the
/// primary slot), so it is the dispatch that fills the window: an Anthropic account
/// handed the SAME headers learns nothing from them and times nothing.
///
/// Watched red, one arm of the dispatch in `update_quota` at a time: the Codex arm given
/// the Anthropic parser leaves the Codex hint at the 60-second placeholder, and the
/// Anthropic arm given the Codex parser times the Anthropic hint on a Codex window.
#[test]
fn codex_headers_time_the_codex_hint_and_never_the_anthropic_one() {
    use crate::provider::Provider;
    let mut codex = AccountRuntime::from_config(&account("b", 0), false);
    codex.provider = Provider::Codex;
    let manager = Manager::from_runtimes(vec![
        AccountRuntime::from_config(&account("a", 0), false),
        codex,
    ]);
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 1800;
    let mut weekly_full = reqwest::header::HeaderMap::new();
    weekly_full.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    weekly_full.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    weekly_full.insert(
        "x-codex-primary-reset-at",
        reset.to_string().parse().unwrap(),
    );
    for idx in [0, 1] {
        assert!(
            !manager.update_quota(idx, &weekly_full, 200),
            "a weekly window alone is no five-hour read, for either parser"
        );
    }
    let now = OffsetDateTime::now_utc();

    let codex_hint = manager.exhaustion_hint(now, false, Provider::Codex, None);
    assert_eq!(
        codex_hint.binding,
        Some(GateReason::SevenDay),
        "the Codex account is out on its weekly window: {codex_hint:?}"
    );
    assert!(
        (1795..=1800).contains(&codex_hint.retry_after),
        "the Codex wait is the window's reset, not the 60s placeholder: {codex_hint:?}"
    );
    assert_eq!(
        codex_hint.free_at.map(OffsetDateTime::unix_timestamp),
        Some(reset)
    );

    let anthropic_hint = manager.exhaustion_hint(now, false, Provider::Anthropic, None);
    assert_eq!(
        (anthropic_hint.binding, anthropic_hint.free_at),
        (None, None),
        "the Anthropic account read nothing from Codex headers: {anthropic_hint:?}"
    );
}

/// **An all-rejected Codex fleet is timed on the windows it stored, then on its
/// holds**, never on the 60-second sentinel. `rejected` is a terminal gate with no
/// clear-instant, so with every Codex account refused the hint used to say "60" for
/// a weekly window days away. Each refused account here is out on its weekly window
/// and held at the one-hour ceiling: the hint names the SOONER stored weekly reset,
/// not the hour-long hold. With no window stored, the hold's expiry answers.
///
/// The two controls pin the fold's edges: a fleet that is not ALL rejected keeps the
/// ordinary fold (its held account frees first), and an all-rejected ANTHROPIC fleet
/// is untouched, still the sentinel.
///
/// Watched red: drop the fold after the loop and the first two answer 60; prefer the
/// hold over the stored window and the first answers the hour.
#[test]
fn an_all_rejected_codex_fleet_is_timed_on_its_stored_windows_then_its_holds() {
    use crate::provider::Provider;
    let now = OffsetDateTime::now_utc();
    let rejected = |name: &str, provider: Provider, weekly_reset_in: Option<i64>| {
        let mut account = AccountRuntime::from_config(&account(name, 0), false);
        account.provider = provider;
        account.quota.status = Some("rejected".to_string());
        account.quota.seven_day = weekly_reset_in.map(|secs| crate::quota::QuotaWindow {
            utilization: 1.0,
            reset: Some(now + Duration::seconds(secs)),
        });
        account
    };

    let fleet = Manager::from_runtimes(vec![
        rejected("a", Provider::Codex, Some(3 * 86_400)),
        rejected("b", Provider::Codex, Some(2 * 86_400)),
    ]);
    fleet.mark_rate_limited(0, 3600);
    fleet.mark_rate_limited(1, 3600);
    let hint = fleet.exhaustion_hint(now, false, Provider::Codex, None);
    assert_eq!(
        hint.free_at.map(OffsetDateTime::unix_timestamp),
        Some((now + Duration::seconds(2 * 86_400)).unix_timestamp()),
        "the sooner stored weekly reset, not the 60s sentinel nor the hour-long hold: {hint:?}"
    );
    assert_eq!(hint.binding, Some(GateReason::Rejected));
    assert_eq!(hint.gated.get(&GateReason::Rejected), Some(&2));

    let no_window = Manager::from_runtimes(vec![rejected("a", Provider::Codex, None)]);
    no_window.mark_rate_limited(0, 300);
    let hint = no_window.exhaustion_hint(OffsetDateTime::now_utc(), false, Provider::Codex, None);
    assert!(
        (299..=300).contains(&hint.retry_after) && hint.free_at.is_some(),
        "with no window stored, the hold's expiry: {hint:?}"
    );

    let mixed = Manager::from_runtimes(vec![rejected("a", Provider::Codex, Some(2 * 86_400)), {
        let mut held = AccountRuntime::from_config(&account("b", 0), false);
        held.provider = Provider::Codex;
        held
    }]);
    mixed.mark_rate_limited(1, 30);
    let hint = mixed.exhaustion_hint(OffsetDateTime::now_utc(), false, Provider::Codex, None);
    assert_eq!(
        hint.binding,
        Some(GateReason::Hold),
        "control: not every account is rejected, so the ordinary fold answers: {hint:?}"
    );
    assert!((29..=30).contains(&hint.retry_after), "{hint:?}");

    let anthropic =
        Manager::from_runtimes(vec![rejected("a", Provider::Anthropic, Some(2 * 86_400))]);
    anthropic.mark_rate_limited(0, 300);
    let hint = anthropic.exhaustion_hint(now, false, Provider::Anthropic, None);
    assert_eq!(
        (hint.retry_after, hint.free_at, hint.binding),
        (60, None, None),
        "control: an all-rejected Anthropic fleet keeps the sentinel: {hint:?}"
    );
}

/// `next_session_key` hands out strictly-increasing, unique u64s starting at 1.
#[test]
fn session_keys_are_monotonic_and_unique() {
    let manager = manager_of(vec![account("a", 0)]);
    let mut prev = 0u64;
    let mut seen = HashSet::new();
    for i in 0..5 {
        let key = manager.next_session_key();
        if i == 0 {
            assert_eq!(key, 1, "the first session key is 1");
        }
        assert!(key > prev, "session keys must strictly increase");
        assert!(seen.insert(key), "session keys must be unique");
        prev = key;
    }
}

// ---- durable disable (the TUI's `d`/`e` survives a restart) ----

/// A config file for `accounts`, in the on-disk shape, carrying an unmodelled
/// top-level key so collateral damage is visible.
fn write_account_file(path: &std::path::Path, names: &[&str]) {
    let entries: Vec<String> = names
        .iter()
        .map(|n| {
            format!(
                r#"{{ "name": "{n}", "type": "oauth", "accessToken": "at-{n}", "refreshToken": "rt-{n}" }}"#
            )
        })
        .collect();
    std::fs::write(
        path,
        format!(
            r#"{{ "warmupSeconds": 900, "accounts": [ {} ] }}"#,
            entries.join(", ")
        ),
    )
    .expect("write test config");
}

fn read_config_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read test config"))
        .expect("test config is valid JSON")
}

/// THE regression guard for this fix. Benching an account from the TUI wrote
/// memory only, so a restart silently returned it to rotation. `set_disabled`
/// must reach the file — and `e` must remove the key again, not write `false`.
#[test]
fn set_disabled_persists_through_to_the_config_file() {
    let path = tmp_config_path("durable-disable");
    write_account_file(&path, &["acct-a", "acct-b"]);
    let manager = build_manager_with_path(
        config_with(vec![account("acct-a", 0), account("acct-b", 0)]),
        path.clone(),
    );

    manager.set_disabled(0, true);

    let after = read_config_json(&path);
    assert_eq!(after["accounts"][0]["disabled"], serde_json::json!(true));
    assert!(
        after["accounts"][1].get("disabled").is_none(),
        "the unrelated account was flagged too: {after}"
    );
    assert_eq!(
        after["warmupSeconds"],
        serde_json::json!(900),
        "an unmodelled key was dropped by the flag write"
    );
    // The in-memory config must agree with the file, so the two views of the
    // flag cannot diverge.
    assert_eq!(
        manager.config.lock().unwrap().accounts[0].disabled,
        Some(true)
    );

    // Re-enabling DROPS the key (the CLI contract), never writes false.
    manager.set_disabled(0, false);
    let after = read_config_json(&path);
    assert!(
        after["accounts"][0].get("disabled").is_none(),
        "re-enable must drop the key, not write false: {after}"
    );
    assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
    std::fs::remove_file(&path).ok();
}

/// A Codex account's bench reaches ITS row. `config::locate_account_entry`
/// never matches a row of the other provider, so the probe `set_disabled`
/// locates the row by must carry the runtime account's provider: built as
/// Anthropic it addresses nothing, and the bench silently does not survive a
/// restart.
///
/// Watched red: build `set_disabled`'s probe with `Provider::Anthropic` in
/// place of `account.provider` and the outcome is `NoEntry`, with the Codex
/// row unflagged.
#[test]
fn set_disabled_on_a_codex_account_persists_to_its_codex_row() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0), codex]),
        "codex-disable",
    );

    assert_eq!(
        manager.set_disabled(1, true),
        DisablePersist::Persisted,
        "the Codex account's bench must reach its own row"
    );
    let after = read_config_json(&path);
    assert_eq!(after["accounts"][1]["disabled"], serde_json::json!(true));
    assert_eq!(after["accounts"][1]["provider"], serde_json::json!("codex"));
    assert!(
        after["accounts"][0].get("disabled").is_none(),
        "the Anthropic row must be untouched: {after}"
    );
    std::fs::remove_file(&path).ok();
}

/// `account` as a Codex account, told from the others by its name alone (no workspace or
/// user id, unlike [`codex_account`], whose rows all share one login).
fn codex_account_named(name: &str) -> Account {
    Account {
        provider: crate::provider::Provider::Codex,
        ..account(name, 0)
    }
}

/// Delete every account but `kept` from the config at `path` while `manager` runs, the way
/// `tcr remove` or a hand edit does, and take the snapshot that notices. The sleep moves the
/// file's mtime, which is what the reload watches.
fn delete_from_disk(
    manager: &Manager,
    path: &std::path::Path,
    kept: Vec<Account>,
) -> StatsSnapshot {
    std::thread::sleep(std::time::Duration::from_millis(20));
    config::save(path, &config_with(kept)).expect("rewrite the test config");
    manager.snapshot(OffsetDateTime::now_utc())
}

fn names(snapshot: &StatsSnapshot) -> Vec<&str> {
    snapshot.accounts.iter().map(|a| a.name.as_str()).collect()
}

fn slots(snapshot: &StatsSnapshot) -> Vec<usize> {
    snapshot.accounts.iter().map(|a| a.slot).collect()
}

/// The dashboard marks the account serving EACH provider, so the manager remembers one per
/// provider: a Codex request served after a Claude one must not erase which account is
/// serving Claude. `current` keeps its meaning, the account that served last of either.
#[test]
fn the_serving_account_is_tracked_per_provider() {
    let (manager, path) = build_manager_with_disk(
        config_with(vec![
            account("alice@example.com", 0),
            codex_account_named("bob@example.com/codex"),
        ]),
        "serving-per-provider",
    );
    manager.set_current(0);
    manager.set_current(1);

    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snapshot.current_by_provider,
        crate::stats::CurrentByProvider {
            anthropic: Some(0),
            codex: Some(1),
        }
    );
    assert_eq!(snapshot.current, Some(1));
    std::fs::remove_file(&path).ok();
}

/// The serving accounts are positions in the snapshot's list, as `current` is, so a row
/// deleted above one moves it up by one, and a deleted serving row reads as nothing serving.
/// A slot is never what the list is indexed by: after the first delete, carol is slot 2 and
/// position 1.
#[test]
fn the_serving_account_survives_a_deleted_row_above_it() {
    let alice = account("alice@example.com", 0);
    let carol = codex_account_named("carol@example.com/codex");
    let (manager, path) = build_manager_with_disk(
        config_with(vec![
            alice.clone(),
            codex_account_named("bob@example.com/codex"),
            carol.clone(),
        ]),
        "serving-past-a-delete",
    );
    manager.set_current(0);
    manager.set_current(2);
    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(slots(&before), [0, 1, 2]);
    assert_eq!(
        before.current_by_provider,
        crate::stats::CurrentByProvider {
            anthropic: Some(0),
            codex: Some(2),
        }
    );

    let without_bob = delete_from_disk(&manager, &path, vec![alice.clone(), carol]);
    // First, that the reload happened at all: a file it could not parse would keep every
    // row, and the assertions below would pass or fail for that reason instead.
    assert_eq!(
        names(&without_bob),
        ["alice@example.com", "carol@example.com/codex"]
    );
    assert_eq!(slots(&without_bob), [0, 2]);
    assert_eq!(
        without_bob.current_by_provider,
        crate::stats::CurrentByProvider {
            anthropic: Some(0),
            codex: Some(1),
        }
    );
    assert_eq!(without_bob.current, Some(1));

    let without_carol = delete_from_disk(&manager, &path, vec![alice]);
    assert_eq!(names(&without_carol), ["alice@example.com"]);
    assert_eq!(
        without_carol.current_by_provider,
        crate::stats::CurrentByProvider {
            anthropic: Some(0),
            codex: None,
        }
    );
    assert_eq!(without_carol.current, None);
    std::fs::remove_file(&path).ok();
}

/// A session carries the provider of the account it is pinned to, and a log line the
/// provider it was pushed with. A session pinned to a row deleted from the config keeps
/// that row's provider, as it keeps that row's name: the pin still points at the slot.
#[test]
fn sessions_and_log_entries_carry_their_provider() {
    let alice = account("alice@example.com", 0);
    let (manager, path) = build_manager_with_disk(
        config_with(vec![
            alice.clone(),
            codex_account_named("bob@example.com/codex"),
        ]),
        "session-provider",
    );
    let now_ms = crate::now_ms();
    manager.seed_session(0xA11CE, 0, 3, now_ms, SessionKind::Stable);
    pin_session(&manager, 0xA11CE, 0);
    manager.seed_session(0xB0B, 1, 5, now_ms, SessionKind::Stable);
    pin_session(&manager, 0xB0B, 1);
    for (path, provider) in [
        ("/v1/messages", crate::provider::Provider::Anthropic),
        (
            crate::provider::CODEX_RESPONSES_PATH,
            crate::provider::Provider::Codex,
        ),
    ] {
        manager.push_log(RequestLogEntry {
            time: OffsetDateTime::now_utc(),
            method: "POST".to_string(),
            path: path.to_string(),
            status: 200,
            account: String::new(),
            provider,
        });
    }

    let provider_of = |snapshot: &StatsSnapshot, account: &str| {
        snapshot
            .sessions
            .iter()
            .find(|s| s.account == account)
            .unwrap_or_else(|| panic!("no session pinned to {account}: {:?}", snapshot.sessions))
            .provider
    };
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        provider_of(&snapshot, "alice@example.com"),
        crate::provider::Provider::Anthropic
    );
    assert_eq!(
        provider_of(&snapshot, "bob@example.com/codex"),
        crate::provider::Provider::Codex
    );
    let logged: Vec<(&str, crate::provider::Provider)> = snapshot
        .recent
        .iter()
        .map(|e| (e.path.as_str(), e.provider))
        .collect();
    assert_eq!(
        logged,
        [
            (
                crate::provider::CODEX_RESPONSES_PATH,
                crate::provider::Provider::Codex
            ),
            ("/v1/messages", crate::provider::Provider::Anthropic),
        ]
    );

    let without_bob = delete_from_disk(&manager, &path, vec![alice]);
    assert_eq!(names(&without_bob), ["alice@example.com"]);
    assert_eq!(
        provider_of(&without_bob, "bob@example.com/codex"),
        crate::provider::Provider::Codex,
        "a session pinned to a deleted row keeps its provider"
    );
    std::fs::remove_file(&path).ok();
}

/// A row deleted from the config keeps its slot, parked and unlisted, and only `disabled`
/// keeps it out of rotation: `eligible` never reads `removed`. So no key may reach it. An
/// enable would put an account the operator deleted back into rotation on its in-memory
/// credentials until a restart, and a bench would report a save for a row nobody can see.
#[test]
fn a_deleted_row_is_never_benched_or_enabled() {
    let bob = account("bob@example.com", 0);
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0), bob.clone()]),
        "deleted-row-keys",
    );
    manager.snapshot(OffsetDateTime::now_utc());
    let without_alice = delete_from_disk(&manager, &path, vec![bob]);
    assert_eq!(names(&without_alice), ["bob@example.com"]);
    let on_disk = std::fs::read(&path).expect("read the test config");

    assert_eq!(
        manager.set_disabled(0, false),
        DisablePersist::NoSuchAccount,
        "an enable must not reach a deleted row"
    );
    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert!(accounts[0].removed, "the deleted row is still marked");
        assert!(
            accounts[0].disabled,
            "and still parked: an enable of it would put it back in rotation"
        );
    }
    assert_eq!(
        manager.set_disabled(0, true),
        DisablePersist::NoSuchAccount,
        "a bench of a deleted row is refused too"
    );
    assert_eq!(
        std::fs::read(&path).expect("read the test config"),
        on_disk,
        "nothing was written for a row the file no longer holds"
    );
    std::fs::remove_file(&path).ok();
}

/// The duplicate notice a Codex row gets names the Codex remedy: remove the duplicate or
/// re-import it under its own name. Giving each entry its own `orgUuid`, the Claude remedy,
/// does nothing for two entries that share one ChatGPT login. Every other outcome, and every
/// Claude row, reads exactly as `warning` does.
#[test]
fn the_codex_duplicate_notice_names_the_codex_remedy() {
    const CODEX_DUPLICATE: &str = "NOT SAVED: two config entries share this Codex login \
        — remove the duplicate (tcr remove <name>), or re-import it with --name";
    let outcomes = [
        DisablePersist::Persisted,
        DisablePersist::NoConfigFile,
        DisablePersist::NoSuchAccount,
        DisablePersist::NoEntry,
        DisablePersist::Ambiguous,
        DisablePersist::WriteFailed,
    ];
    for disabled in [true, false] {
        assert_eq!(
            DisablePersist::Ambiguous.warning_for(disabled, crate::provider::Provider::Codex),
            Some(CODEX_DUPLICATE)
        );
        for outcome in outcomes {
            assert_eq!(
                outcome.warning_for(disabled, crate::provider::Provider::Anthropic),
                outcome.warning(disabled),
                "a Claude row reads {outcome:?} as it always has"
            );
            if outcome != DisablePersist::Ambiguous {
                assert_eq!(
                    outcome.warning_for(disabled, crate::provider::Provider::Codex),
                    outcome.warning(disabled),
                    "{outcome:?} says the same about either provider"
                );
            }
        }
    }
    assert!(
        DisablePersist::Ambiguous
            .warning_for(true, crate::provider::Provider::Anthropic)
            .is_some_and(|text| text.ends_with("give each its own orgUuid")),
        "the Claude duplicate notice keeps its remedy"
    );
}

/// A manager with NO `config_path` — every test here, `tcr demo`, and
/// `tcr status --probe` — must write nothing at all. The demo drives
/// `set_disabled` on rows that have no config file behind them.
///
/// Differential against the test above: same fixture, same call, and the only
/// difference is the absent path. Without the pair, "the file did not change"
/// would prove nothing.
#[test]
fn set_disabled_writes_nothing_without_a_config_path() {
    let path = tmp_config_path("durable-disable-none");
    write_account_file(&path, &["acct-a"]);
    let before = std::fs::read_to_string(&path).unwrap();

    let manager = manager_of(vec![account("acct-a", 0)]);
    assert!(manager.config_path.is_none());

    manager.set_disabled(0, true);

    // The runtime row still flips — only the disk write is suppressed.
    assert!(manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "a manager with no config_path must never write to disk"
    );
    std::fs::remove_file(&path).ok();
}

/// **INV3 / FIX 4.** A write that did not land must leave the in-memory snapshot
/// alone. Mutating memory first and regardless of the outcome left the two views
/// permanently diverged — memory saying benched, disk saying nothing — with
/// nothing to reconcile or retry them, which is the opposite of the guarantee the
/// comment above `persist_disabled` claimed.
///
/// All three failure shapes, because each takes a different arm:
/// `NoEntry` (identity absent from disk), `Ambiguous` (two entries share it), and
/// `Err` (the file cannot even be parsed).
#[test]
fn a_failed_persist_leaves_the_in_memory_snapshot_unchanged() {
    // NoEntry — the runtime row exists, the disk entry does not.
    let path = tmp_config_path("persist-noentry");
    write_account_file(&path, &["someone-else"]);
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
    assert_eq!(
        manager.set_disabled(0, true),
        DisablePersist::NoEntry,
        "no disk entry carries this identity"
    );
    assert_eq!(
        manager.config.lock().unwrap().accounts[0].disabled,
        None,
        "a write that never landed must not be reflected in memory"
    );
    // The runtime row still flips — the bench takes effect NOW, it just is not
    // durable, which is exactly what the caller is told. Read off the row
    // itself, not a snapshot: this file never held `acct-a`, so a snapshot's
    // reload marks the row removed and lists nothing.
    assert!(manager.accounts.read().unwrap()[0].disabled);
    std::fs::remove_file(&path).ok();

    // Ambiguous — two disk entries share the identity, so none may be chosen.
    let path = tmp_config_path("persist-ambiguous");
    write_account_file(&path, &["acct-a", "acct-a"]);
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
    let before = std::fs::read_to_string(&path).unwrap();
    assert_eq!(manager.set_disabled(0, true), DisablePersist::Ambiguous);
    assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "an ambiguous identity must leave the file byte-identical"
    );
    std::fs::remove_file(&path).ok();

    // Err — the file cannot be parsed, so nothing can be merged into it.
    let path = tmp_config_path("persist-malformed");
    std::fs::write(&path, "{ not json").unwrap();
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
    assert_eq!(manager.set_disabled(0, true), DisablePersist::WriteFailed);
    assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ not json",
        "a malformed config must be left exactly as found"
    );
    std::fs::remove_file(&path).ok();
}

/// Every outcome the user could mistake for success must SAY so, and the two
/// that are fine must stay silent. This is the mapping the TUI renders, so a
/// silent failure arm here is a lie on screen.
#[test]
fn every_non_durable_persist_outcome_warns() {
    for disabled in [true, false] {
        for outcome in [DisablePersist::Persisted, DisablePersist::NoConfigFile] {
            assert_eq!(
                outcome.warning(disabled),
                None,
                "{outcome:?} is not a failure — warning about it would be noise"
            );
        }
        for outcome in [
            DisablePersist::NoSuchAccount,
            DisablePersist::NoEntry,
            DisablePersist::Ambiguous,
            DisablePersist::WriteFailed,
        ] {
            let warning = outcome
                .warning(disabled)
                .unwrap_or_else(|| panic!("{outcome:?} must warn — it did not persist"));
            assert!(
                warning.starts_with("NOT SAVED"),
                "{outcome:?} must lead with the headline so it survives truncation: {warning}"
            );
        }
    }
}

/// **THE direction guard.** Nothing was written, so the file still says what it
/// said — and that means the consequence of a failed write is the OPPOSITE in
/// the two directions. A failed `d` (disable) leaves the account in rotation; a
/// failed `e` (enable) leaves it BENCHED. One direction-blind line used to
/// claim "it returns to rotation on restart" for both, so a user whose enable
/// failed restarted on that advice and got the account benched anyway.
#[test]
fn a_failed_persist_states_the_consequence_of_its_own_direction() {
    for outcome in [DisablePersist::NoEntry, DisablePersist::WriteFailed] {
        let disabling = outcome.warning(true).expect("a failed disable must warn");
        assert!(
            disabling.ends_with("it returns to rotation on restart"),
            "a failed DISABLE leaves the account in rotation: {disabling}"
        );

        let enabling = outcome.warning(false).expect("a failed enable must warn");
        assert!(
            enabling.ends_with("it stays benched after a restart"),
            "a failed ENABLE leaves the account benched: {enabling}"
        );
        assert!(
            !enabling.contains("returns to rotation"),
            "a failed enable must never promise the opposite of what happens: {enabling}"
        );
    }
}

/// The ambiguity refusal must name an action that actually works. It used to
/// say "rename one to fix", which is provably wrong whenever both entries carry
/// an `accountUuid`: the match never consults names on that path, so renaming
/// changes nothing and the next `d` fails identically.
#[test]
fn the_ambiguity_warning_names_a_remedy_that_works() {
    let warning = DisablePersist::Ambiguous
        .warning(true)
        .expect("an ambiguous refusal must warn");
    assert!(
        warning.contains("orgUuid"),
        "the remedy is giving the entries distinct org keys: {warning}"
    );
    assert!(
        !warning.contains("rename"),
        "renaming does not break a UUID-matched tie: {warning}"
    );
}

/// A manager with no config file behind it reports that distinctly and does NOT
/// warn: `tcr demo` and `tcr status --probe` are memory-only by design, and a
/// "not saved" banner on every keypress there would be noise, not honesty.
#[test]
fn set_disabled_without_a_config_file_is_not_a_failure() {
    let manager = manager_of(vec![account("acct-a", 0)]);
    assert_eq!(manager.set_disabled(0, true), DisablePersist::NoConfigFile);
    assert_eq!(manager.set_disabled(9, true), DisablePersist::NoSuchAccount);
}

/// **INV2 / FIX 6.** The config file write must not run under the config mutex.
/// That mutex is taken on the PER-CONNECTION path — `state.rs`'s
/// `session_affinity_enabled()`, called from `mitm.rs`'s serve path — so holding
/// it across `save_disabled`'s read + `sync_all` + rename let one TUI keypress
/// stall connection setup on an fsync.
///
/// Measured by holding the config mutex here and requiring the write to complete
/// anyway. Under a persist that holds `config` across the save, the write cannot
/// start until this guard drops, so the poll below never sees the flag and the
/// test fails on its deadline.
#[test]
fn the_config_file_write_does_not_run_under_the_config_mutex() {
    let path = tmp_config_path("persist-inv2");
    write_account_file(&path, &["acct-a"]);
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());

    // Stand in for a request-path `session_affinity_enabled()` holding the lock.
    let held = manager.config.lock().expect("config lock poisoned");

    let writer = {
        let m = Arc::clone(&manager);
        std::thread::spawn(move || m.set_disabled(0, true))
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if read_config_json(&path)["accounts"][0]
            .get("disabled")
            .is_some()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the config file write blocked on the config mutex — INV2 violated"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // INV3 still applies: the memory update is what waits for this guard.
    drop(held);
    assert_eq!(
        writer.join().expect("persist thread panicked"),
        DisablePersist::Persisted
    );
    assert_eq!(
        manager.config.lock().unwrap().accounts[0].disabled,
        Some(true)
    );
    std::fs::remove_file(&path).ok();
}

/// **INV1 / FIX 6.** Concurrent persists must not interleave their
/// read-modify-write of the file. Each of these threads benches a DIFFERENT
/// account, so every flag must be present at the end; an interleaved
/// read-modify-write drops the flags written between another thread's read and
/// its rename, which shows up here as a missing flag.
#[test]
fn concurrent_persists_do_not_lose_each_others_writes() {
    const ACCOUNTS: usize = 8;
    let names: Vec<String> = (0..ACCOUNTS).map(|i| format!("acct-{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let path = tmp_config_path("persist-inv1");
    write_account_file(&path, &refs);
    let manager = build_manager_with_path(
        config_with(names.iter().map(|n| account(n, 0)).collect()),
        path.clone(),
    );

    let threads: Vec<_> = (0..ACCOUNTS)
        .map(|idx| {
            let m = Arc::clone(&manager);
            std::thread::spawn(move || m.set_disabled(idx, true))
        })
        .collect();
    for thread in threads {
        assert_eq!(
            thread.join().expect("persist thread panicked"),
            DisablePersist::Persisted
        );
    }

    let after = read_config_json(&path);
    for idx in 0..ACCOUNTS {
        assert_eq!(
            after["accounts"][idx]["disabled"],
            serde_json::json!(true),
            "account {idx}'s flag was lost to an interleaved write: {after}"
        );
    }
    assert_eq!(
        after["warmupSeconds"],
        serde_json::json!(900),
        "an unmodelled key was dropped by the concurrent writes"
    );
    std::fs::remove_file(&path).ok();
}

/// An out-of-range index is a no-op on both halves: nothing flips in memory
/// and nothing is written, rather than persisting a flag for an account that
/// does not exist.
#[test]
fn set_disabled_out_of_range_writes_nothing() {
    let path = tmp_config_path("durable-disable-oob");
    write_account_file(&path, &["acct-a"]);
    let before = std::fs::read_to_string(&path).unwrap();
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());

    manager.set_disabled(9, true);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    std::fs::remove_file(&path).ok();
}

// ---- live account-add identity resolution (`POST /_tcr/accounts`) ----

/// Submit `submission` and assert it replaced the credential on row 0.
fn assert_it_updates_the_first_row(manager: &Manager, submission: Account) {
    let outcome = manager.add_or_update_account(submission);
    std::assert_matches!(outcome, AddAccountOutcome::Updated { idx: 0, .. });
}

/// A manager whose in-memory config AND on-disk file both start out equal
/// to `config` — so both halves of `add_or_update_account` are observable:
/// `manager.accounts`/`manager.config` for the live view, and reloading
/// `path` for the durable one. Mirrors `proxy.rs`'s `control_manager`.
fn build_manager_with_disk(config: Config, tag: &str) -> (Arc<Manager>, PathBuf) {
    let path = tmp_config_path(tag);
    config::save(&path, &config).expect("write test config");
    (build_manager_with_path(config, path.clone()), path)
}

/// F1 — CRITICAL regression. `{alice@example.com, uuid 1111…, rt-CORP}` is
/// on record; a submission of `{alice@example.com, uuid 2222…, rt-PERSONAL}`
/// is a DIFFERENT PERSON who happens to share a display name. The old rule
/// (`identity::match_one`, which goes through `Queryable` and cannot compare
/// `account_uuid` at all) matched it onto Corp's row and overwrote its
/// single-use refresh token — unrecoverable without a hand re-auth. It must
/// APPEND instead, leaving Corp's row untouched, in memory and on disk.
#[test]
fn add_or_update_account_never_overwrites_a_different_persons_refresh_token() {
    let corp = Account {
        account_uuid: Some("uuid-1111".to_string()),
        access_token: "at-CORP".to_string(),
        refresh_token: Some("rt-CORP".to_string()),
        ..account("alice@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![corp]), "f1-uuid-mismatch");

    let submission = Account {
        account_uuid: Some("uuid-2222".to_string()),
        access_token: "at-PERSONAL".to_string(),
        refresh_token: Some("rt-PERSONAL".to_string()),
        ..account("alice@example.com", 0)
    };
    let outcome = manager.add_or_update_account(submission);

    let (idx, persist) = match outcome {
        AddAccountOutcome::Added { idx, persist, .. } => (idx, persist),
        other => panic!(
            "a different account_uuid under the same name must APPEND, not \
             update or refuse: {other:?}"
        ),
    };
    assert_eq!(idx, 1, "appended at the end, Corp's row never moves");
    assert_eq!(persist, AddPersist::Persisted);

    // In memory: Corp's row is byte-identical.
    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(accounts.len(), 2);
        assert_eq!(
            accounts[0].refresh_token.as_deref(),
            Some("rt-CORP"),
            "Corp's single-use refresh token was overwritten in memory"
        );
        assert_eq!(accounts[0].access_token, "at-CORP");
        assert_eq!(accounts[1].refresh_token.as_deref(), Some("rt-PERSONAL"));
    }

    // On disk: same.
    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(reloaded.accounts.len(), 2);
    assert_eq!(
        reloaded.accounts[0].refresh_token.as_deref(),
        Some("rt-CORP"),
        "Corp's single-use refresh token was overwritten on disk"
    );
    assert_eq!(
        reloaded.accounts[1].refresh_token.as_deref(),
        Some("rt-PERSONAL")
    );

    std::fs::remove_file(&path).ok();
}

/// F5 follow-up: the re-add target `add_or_update_account`'s `Updated`
/// arm hands to `persist_replaced` must carry the account's EXISTING
/// exit-node pin, never hardcode the unpinned default. The bug is
/// unobservable on the common path (`persist_replaced`'s `Resolved::One`
/// arm reads only `access_token`/`refresh_token`/`expires_at` off the
/// target, never touching `egress` when a matching config/disk row is
/// found, so egress survives untouched either way there). It bites in
/// `persist_replaced`'s APPEND arm, reached when the in-memory
/// `self.accounts` runtime row still exists but the on-disk document has
/// lost its counterpart entry (an operator hand-editing `tcr-peers`-style
/// config, or a partial write): `merge_account`'s `EntryMatch::None` arm
/// then serializes the WHOLE target object as a fresh disk row, `egress`
/// included, so with the hardcoded default, the pin the in-memory config
/// still remembers never reaches the newly recreated disk row.
#[test]
fn add_or_update_account_carries_an_existing_strict_egress_pin_into_a_recreated_disk_row() {
    let pinned_peer = tcr_peer_wire::PeerId([7u8; 32]);
    let pinned = Account {
        account_uuid: Some("uuid-pinned".to_string()),
        access_token: "at-old".to_string(),
        refresh_token: Some("rt-old".to_string()),
        egress: config::Egress::Via(pinned_peer),
        egress_strict: true,
        ..account("pinned@example.com", 0)
    };
    let (manager, path) =
        build_manager_with_disk(config_with(vec![pinned]), "f5-egress-pin-carried");

    // Simulate the disk document losing its entry for this account while
    // `self.config` in memory (the manager never re-reads accounts from
    // disk, only groups reload) still carries the full row, pin
    // included: a raw JSON edit, never through `config::save`, which
    // would go through `self.config` and defeat the scenario.
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["accounts"].as_array_mut().unwrap().clear();
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    // Same identity, fresh credentials, and, like a real re-add through
    // `POST /_tcr/accounts`, no opinion of its own about egress: the
    // submission carries only what `account(..)` builds, i.e. Local/false.
    let submission = Account {
        account_uuid: Some("uuid-pinned".to_string()),
        access_token: "at-new".to_string(),
        refresh_token: Some("rt-new".to_string()),
        ..account("pinned@example.com", 0)
    };
    let outcome = manager.add_or_update_account(submission);
    match outcome {
        AddAccountOutcome::Updated { persist, .. } => {
            assert_eq!(persist, AddPersist::Persisted);
        }
        other => panic!("same identity, fresh credentials must UPDATE in place: {other:?}"),
    }

    // DISK: the row `persist_replaced` recreates must carry the pin
    // forward, not reset it to Local.
    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(reloaded.accounts.len(), 1);
    assert_eq!(
        reloaded.accounts[0].egress,
        config::Egress::Via(pinned_peer),
        "the recreated disk row must carry the account's existing egress pin forward, \
         not reset it to Local"
    );
    assert!(
        reloaded.accounts[0].egress_strict,
        "the recreated disk row must carry the account's existing strict flag forward"
    );

    std::fs::remove_file(&path).ok();
}

/// F2 — CRITICAL regression. A legacy row with NO stored org, met by a
/// submission carrying the row's OWN org (even just profiled for the first
/// time), must resolve to the SAME row on both the live and durable halves
/// — never split into a second live row backed by the same one disk entry.
#[test]
fn add_or_update_account_backfills_a_legacy_no_org_row_instead_of_splitting_the_fleet() {
    let legacy = Account {
        account_uuid: Some("uuid-legacy".to_string()),
        refresh_token: Some("rt-legacy".to_string()),
        ..account("alice@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![legacy]), "f2-legacy-org");

    let submission = Account {
        account_uuid: Some("uuid-legacy".to_string()),
        org_uuid: Some("org-corp-uuid".to_string()),
        org_name: Some("Corp".to_string()),
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com", 0)
    };
    let outcome = manager.add_or_update_account(submission);

    let (idx, persist) = match outcome {
        AddAccountOutcome::Updated { idx, persist, .. } => (idx, persist),
        other => panic!(
            "a legacy no-org row meeting its own org must UPDATE in place, \
             not split the fleet: {other:?}"
        ),
    };
    assert_eq!(idx, 0);
    assert_eq!(persist, AddPersist::Persisted);

    // LIVE: one row, now carrying the org and the fresh credentials.
    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts.len(),
            1,
            "a second live row appeared — the split brain this fixes"
        );
        assert_eq!(accounts[0].org_uuid.as_deref(), Some("org-corp-uuid"));
        assert_eq!(accounts[0].org_name.as_deref(), Some("Corp"));
        assert_eq!(accounts[0].refresh_token.as_deref(), Some("rt-fresh"));
    }

    // DISK: agrees — one entry, same identity, now carrying the org too, so
    // the NEXT submission (a genuinely different org) requires an exact
    // match instead of tolerating a still-unknown org key forever.
    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(reloaded.accounts.len(), 1, "disk split into a second entry");
    assert_eq!(
        reloaded.accounts[0].org_uuid.as_deref(),
        Some("org-corp-uuid")
    );
    assert_eq!(
        reloaded.accounts[0].refresh_token.as_deref(),
        Some("rt-fresh")
    );

    std::fs::remove_file(&path).ok();
}

/// F3 — HIGH regression. The old two-lock version resolved under a READ
/// lock, released it, then took a separate WRITE lock to append — so N
/// concurrent submissions of the same brand-new identity could all resolve
/// "no match" before any of them had appended (measured 73 duplicate rows
/// in 200 concurrent rounds). Fire N concurrent adds of ONE new identity and
/// assert exactly one caller appended, on both halves.
#[test]
fn concurrent_adds_of_one_new_identity_never_duplicate_the_live_row() {
    use std::thread;
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0)]),
        "f3-toctou",
    );

    let n = 16usize;
    let submission = Account {
        account_uuid: Some("uuid-new".to_string()),
        access_token: "at-new".to_string(),
        refresh_token: Some("rt-new".to_string()),
        ..account("carol@example.com", 0)
    };

    let handles: Vec<_> = (0..n)
        .map(|_| {
            let m = Arc::clone(&manager);
            let acct = submission.clone();
            thread::spawn(move || m.add_or_update_account(acct))
        })
        .collect();
    let outcomes: Vec<AddAccountOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("add_or_update_account thread panicked"))
        .collect();

    let added = outcomes
        .iter()
        .filter(|o| matches!(o, AddAccountOutcome::Added { .. }))
        .count();
    let updated = outcomes
        .iter()
        .filter(|o| matches!(o, AddAccountOutcome::Updated { .. }))
        .count();
    assert_eq!(
        added, 1,
        "exactly one concurrent caller may append a brand-new identity; \
         the rest must see it as already-added (Updated): added={added} updated={updated}"
    );
    assert_eq!(updated, n - 1);

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(
        accounts.len(),
        2,
        "alice's row plus exactly one new row for carol — not a duplicate"
    );
    drop(accounts);

    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(
        reloaded.accounts.len(),
        2,
        "disk duplicated the new identity"
    );

    std::fs::remove_file(&path).ok();
}

/// A follow-up regression on the Added path (distinct from the F3 TOCTOU
/// finding above, and from `config::save_account`'s identical fix on the
/// durable side): appending an account with no explicit priority must
/// join the BACK of the LIVE fleet — `max(existing priorities) + 1` — not
/// the 0 that `AccountRuntime::from_config`'s `unwrap_or(0)` reads from an
/// absent `priority`, which would silently promote a freshly added
/// account to the PRIMARY tier ahead of the established fleet.
#[test]
fn add_or_update_account_added_path_assigns_max_plus_one_priority_when_none_submitted() {
    let (manager, path) = build_manager_with_disk(
        config_with(vec![
            account("alice@example.com", 0),
            account("bob@example.com", 1),
        ]),
        "live-default-priority",
    );

    let submission = Account {
        priority: None,
        ..account("carol@example.com", 0)
    };
    let idx = match manager.add_or_update_account(submission) {
        AddAccountOutcome::Added { idx, .. } => idx,
        other => panic!("expected Added: {other:?}"),
    };

    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts[idx].priority, 2,
            "an added account with no explicit priority must join the back \
             of the live fleet, not the default-derived 0"
        );
    }

    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(
        reloaded.accounts[idx].priority,
        Some(2),
        "the durable half must agree with the live rotation on the assigned priority"
    );

    std::fs::remove_file(&path).ok();
}

/// An explicit priority submitted on the Added path is never overridden by
/// the max+1 default — mirrors the durable-side test of the same name in
/// `config.rs`.
#[test]
fn add_or_update_account_added_path_keeps_an_explicit_priority() {
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0)]),
        "live-explicit-priority",
    );

    let submission = Account {
        priority: Some(99),
        ..account("carol@example.com", 0)
    };
    let idx = match manager.add_or_update_account(submission) {
        AddAccountOutcome::Added { idx, .. } => idx,
        other => panic!("expected Added: {other:?}"),
    };

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(accounts[idx].priority, 99);
    drop(accounts);

    std::fs::remove_file(&path).ok();
}

/// Before the first fix, the Added path's `AccountRuntime::from_config(&account)`
/// ran INSIDE the `self.accounts.write()` block — meaning
/// `build_serving_client`'s `.expect("build reqwest client")` executed while
/// the write guard was held. A panic there poisons the `RwLock` for every
/// other caller in this module: `http_client`, `select`, `snapshot`,
/// `record_stream_error` and ~93 more all `.expect("accounts lock
/// poisoned")`, so one recoverable client-build failure would take down the
/// whole proxy.
///
/// A second round moved the build OUTSIDE `self.accounts`'s lock but left
/// it inside `config_write` (held from function entry in that version) —
/// so the SAME panic instead poisoned `config_write`, which is arguably
/// worse: `persist_tokens` and `persist_now`'s shutdown flush both
/// `.expect()` it, so a poisoned `config_write` stops rotated refresh
/// tokens from ever reaching disk again, on top of every future call to
/// this very function panicking at its own `config_write.lock()`.
///
/// Uses the `#[cfg(test)]` fault-injection seam on `build_serving_client`
/// to make that failure real rather than hypothetical, on a submission
/// guaranteed to resolve Added (an empty fleet, so nothing can match),
/// and asserts BOTH locks survive.
#[test]
fn add_or_update_account_added_path_does_not_poison_either_lock_on_a_client_build_panic() {
    let manager = manager_of(vec![]);
    let submission = account("brand-new@example.com", 0);

    fail_next_client_build();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        manager.add_or_update_account(submission)
    }));
    assert!(
        result.is_err(),
        "setup: the injected client-build failure must actually panic"
    );

    assert!(
        manager.accounts.read().is_ok(),
        "a client-build panic in add_or_update_account must not poison the \
         accounts lock — every other .expect(\"accounts lock poisoned\") \
         call site would panic in turn"
    );
    assert!(
        manager.config_write.lock().is_ok(),
        "a client-build panic in add_or_update_account must not poison the \
         config_write lock either — persist_tokens and persist_now's \
         shutdown flush both .expect() it, and this function itself takes \
         it again on its very next call"
    );
}

/// The Added-only half of the fix above: an ordinary re-auth of an
/// EXISTING account (the common case this route serves — every token
/// refresh's re-login goes through `POST /_tcr/accounts`, not just a
/// first-time add) must resolve Updated on its FIRST locked attempt and
/// never reach `build_serving_client` at all. A version that built the
/// runtime unconditionally before resolving (fixing the poisoning bug
/// above but not this one) would waste a client build on every re-auth
/// and widen the panic surface to a call that never needed one. Arms the
/// SAME fault-injection seam on a submission guaranteed to resolve
/// Updated (an existing row with a matching bare identity) — if the
/// build were ever reached the injected panic would fire, so a normal
/// return proves it was not.
#[test]
fn add_or_update_account_updated_path_never_builds_a_client() {
    let existing = account("alice@example.com", 0);
    let manager = manager_of(vec![existing.clone()]);
    let mut resubmission = existing;
    resubmission.access_token = "at-alice-rotated".to_string();

    fail_next_client_build();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        manager.add_or_update_account(resubmission)
    }));

    let outcome = result.unwrap_or_else(|_| {
        panic!(
            "add_or_update_account must never reach build_serving_client on \
             the Updated path — the injected client-build failure should \
             not have fired at all"
        )
    });
    assert!(
        matches!(outcome, AddAccountOutcome::Updated { .. }),
        "setup: the resubmission must resolve against the existing row, \
         not append a new one"
    );
}

/// The live half of a brand-new-identity race was already serialized by
/// the single `accounts` write-lock spanning resolve+mutate (see the
/// TOCTOU fix above) — but the two callers' DURABLE writes still queued on
/// `config_write` independently, in whatever order the OS scheduled them,
/// with no guarantee that order agreed with which caller actually won the
/// live race. Two real logins of the SAME identity racing each other (a
/// double-submit, a retried `tcr login`) carry genuinely DIFFERENT tokens,
/// so a disagreement is observable: the file could end up holding the
/// LOSING submission's token while the live rotation served the winner's.
/// Measured 2/200 over 200 rounds of two simultaneous adds of one
/// identity before `config_write` was widened to span the whole
/// resolve-mutate-persist sequence. Runs the same 200 rounds to catch it.
#[test]
fn concurrent_adds_of_one_new_identity_never_disagree_with_disk_on_the_token() {
    use std::thread;
    for round in 0..200 {
        let (manager, path) = build_manager_with_disk(
            config_with(vec![account("alice@example.com", 0)]),
            &format!("n2-token-race-{round}"),
        );

        let base = Account {
            account_uuid: Some("uuid-new".to_string()),
            ..account("carol@example.com", 0)
        };
        let submission_a = Account {
            access_token: "at-A".to_string(),
            refresh_token: Some("rt-A".to_string()),
            ..base.clone()
        };
        let submission_b = Account {
            access_token: "at-B".to_string(),
            refresh_token: Some("rt-B".to_string()),
            ..base
        };

        let m1 = Arc::clone(&manager);
        let m2 = Arc::clone(&manager);
        let h1 = thread::spawn(move || m1.add_or_update_account(submission_a));
        let h2 = thread::spawn(move || m2.add_or_update_account(submission_b));
        h1.join().expect("adder A panicked");
        h2.join().expect("adder B panicked");

        let live_token = {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            accounts
                .iter()
                .find(|a| a.name == "carol@example.com")
                .expect("carol's row exists live")
                .access_token
                .clone()
        };
        let reloaded = config::load(&path).expect("reload persisted config");
        let disk_token = reloaded
            .accounts
            .iter()
            .find(|a| a.name == "carol@example.com")
            .expect("carol's row exists on disk")
            .access_token
            .clone();

        assert_eq!(
            live_token, disk_token,
            "round {round}: live token ({live_token}) and disk token \
             ({disk_token}) disagree for the same identity"
        );

        std::fs::remove_file(&path).ok();
    }
}

/// F4 — an `Error` row given fresh credentials must clear back to `Active`
/// (`eligible` hard-gates on `status == Error`) but its rate-limit hold must
/// SURVIVE the credential replace — clearing it bought nothing but the
/// ability to race a genuinely-still-limited account back into rotation
/// early. An EXPIRED hold proves both facts at once without contradiction:
/// the field keeps its stamped value, and the account is selectable because
/// that value is already in the past.
#[test]
fn add_or_update_account_clears_error_status_but_keeps_the_rate_limit_hold() {
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0)]),
        "f4-hold-survives",
    );
    let hold_until = crate::now_ms() - 1_000;
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].status = AccountStatus::Error;
        accounts[0].rate_limited_until_ms = Some(hold_until);
    }

    let submission = Account {
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com", 0)
    };
    assert_it_updates_the_first_row(&manager, submission);

    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts[0].status,
            AccountStatus::Active,
            "fresh credentials must clear a stuck error"
        );
        assert_eq!(
            accounts[0].rate_limited_until_ms,
            Some(hold_until),
            "the rate-limit hold must survive a credential replace"
        );
    }

    assert_eq!(
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            "/v1/messages",
            None
        ),
        Some(0),
        "active status + an EXPIRED hold ⇒ immediately selectable"
    );

    std::fs::remove_file(&path).ok();
}

/// F5 hardening. When the durable write must APPEND on `persist_replaced`'s
/// path (no on-disk entry carries this identity — e.g. it was deleted while
/// the proxy ran), the appended record must carry the row's REAL
/// `priority`/`disabled`, never `identity::probe`'s `None` placeholders —
/// otherwise a deliberately-benched account comes back into rotation on the
/// very next restart, the exact bug `persist_disabled` exists to prevent.
#[test]
fn add_or_update_account_persists_real_routing_state_when_appending_via_replace() {
    let alice = Account {
        account_uuid: Some("uuid-alice".to_string()),
        priority: Some(5),
        disabled: Some(true),
        ..account("alice@example.com", 5)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![alice]), "f5-routing-state");

    // The disk entry vanishes out from under the live process — the durable
    // write must now APPEND.
    std::fs::write(&path, r#"{"accounts": []}"#).expect("rewrite disk config");

    let submission = Account {
        account_uuid: Some("uuid-alice".to_string()),
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com", 0)
    };
    assert_it_updates_the_first_row(&manager, submission);

    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(reloaded.accounts.len(), 1);
    assert_eq!(
        reloaded.accounts[0].priority,
        Some(5),
        "a benched account's priority must survive an append-via-replace"
    );
    assert_eq!(
        reloaded.accounts[0].disabled,
        Some(true),
        "…and its disabled flag — the exact restart-un-bench persist_disabled exists to prevent"
    );

    std::fs::remove_file(&path).ok();
}

/// F6 — an update backfills every identity field the row is missing
/// (`account_uuid`/`org_uuid`/`org_name`) and always corrects `account_type`
/// to the submitted value: a row left at a stale non-`"oauth"` type is a
/// silent death, because `refresh_plan` skips any account whose type is not
/// `"oauth"`. On both the live row and the disk entry.
#[test]
fn add_or_update_account_backfills_identity_and_corrects_a_stale_account_type() {
    let legacy_api_row = Account {
        account_type: config::AccountType::Unrecognized("api".to_string()),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        access_token: "sk-old".to_string(),
        refresh_token: None,
        ..account("alice@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![legacy_api_row]), "f6-backfill");

    let submission = Account {
        account_type: config::AccountType::OAuth,
        account_uuid: Some("uuid-alice".to_string()),
        org_uuid: Some("org-corp-uuid".to_string()),
        org_name: Some("Corp".to_string()),
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com", 0)
    };
    assert_it_updates_the_first_row(&manager, submission);

    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts[0].account_type,
            config::AccountType::OAuth,
            "a stale non-oauth type must be corrected, or refresh_plan silently \
             never refreshes this account again"
        );
        assert_eq!(accounts[0].account_uuid.as_deref(), Some("uuid-alice"));
        assert_eq!(accounts[0].org_uuid.as_deref(), Some("org-corp-uuid"));
        assert_eq!(accounts[0].org_name.as_deref(), Some("Corp"));
    }

    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(
        reloaded.accounts[0].account_type,
        config::AccountType::OAuth
    );
    assert_eq!(
        reloaded.accounts[0].account_uuid.as_deref(),
        Some("uuid-alice")
    );
    assert_eq!(
        reloaded.accounts[0].org_uuid.as_deref(),
        Some("org-corp-uuid")
    );
    assert_eq!(reloaded.accounts[0].org_name.as_deref(), Some("Corp"));

    std::fs::remove_file(&path).ok();
}

/// Re-adding a Codex account replaces the credential on ITS row. The
/// Updated arm hands `persist_replaced` a target it builds itself, and
/// `config::locate_account_entry` never matches a row of the other
/// provider: a target that defaulted to Anthropic found no row and APPENDED
/// one, an Anthropic row carrying the Codex credential, which the next boot
/// would send to the Anthropic upstream.
///
/// Watched red: give the Updated target `provider: Default::default()` and
/// the file grows a second row with no `provider` key.
#[test]
fn re_adding_a_codex_account_updates_its_row_and_appends_no_anthropic_duplicate() {
    let stored = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        access_token: "at-old".to_string(),
        refresh_token: Some("rt-old".to_string()),
        ..account("bob@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![stored.clone()]), "codex-readd");

    let submission = Account {
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..stored
    };
    let outcome = manager.add_or_update_account(submission);
    std::assert_matches!(
        outcome,
        AddAccountOutcome::Updated {
            idx: 0,
            persist: AddPersist::Persisted,
            ..
        }
    );

    let after = read_config_json(&path);
    let rows = after["accounts"].as_array().expect("an accounts array");
    assert_eq!(
        rows.len(),
        1,
        "a Codex re-add must update its own row, never append a duplicate: {after}"
    );
    assert_eq!(rows[0]["provider"], serde_json::json!("codex"));
    assert_eq!(rows[0]["refreshToken"], serde_json::json!("rt-fresh"));
    std::fs::remove_file(&path).ok();
}

/// Preservation guard: a submission with NO identity fields at all (a bare
/// name) resolves on NAME EQUALITY alone — `same_identity`'s fallback when
/// either side has no uuid — and a name that matches nothing is APPENDED,
/// never merged onto a row whose name merely looks similar.
///
/// This used to need a `match_one` retry on top, so a bare email could find
/// a stored `alice@example.com (Corp)`. That display-name shape existed only
/// because names were not unique; a submission naming a row that is not
/// there is now a new account, which is the honest reading.
#[test]
fn add_or_update_account_with_a_bare_name_matches_only_that_exact_name() {
    let stored = Account {
        refresh_token: Some("rt-old".to_string()),
        ..account("alice@example.com/corp", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![stored]), "f-bare-name-exact");

    // The exact stored name updates in place.
    let same_row = Account {
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com/corp", 0)
    };
    match manager.add_or_update_account(same_row) {
        AddAccountOutcome::Updated { idx, .. } => assert_eq!(idx, 0),
        other => panic!("an exact name must update its own row: {other:?}"),
    }

    // A different name is a different account, appended.
    let other_row = Account {
        access_token: "at-other".to_string(),
        refresh_token: Some("rt-other".to_string()),
        ..account("alice@example.com", 0)
    };
    match manager.add_or_update_account(other_row) {
        AddAccountOutcome::Added { idx, .. } => assert_eq!(idx, 1),
        other => panic!("an unmatched name must be appended: {other:?}"),
    }
    assert_eq!(
        manager
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .len(),
        2
    );

    std::fs::remove_file(&path).ok();
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
        &crate::probe::Usage {
            five_hour: crate::probe::WindowReading::PlanHasNone,
            ..crate::probe::Usage::default()
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

    let headerless_probe = crate::probe::Usage {
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

/// Multi-thread stress: RACE `select()` / `enter_in_flight()` / guard-drop
/// against the other account-lock writers (`mark_rate_limited` /
/// `record_served`) across 8 threads on one shared `Arc<Manager>` with pacing
/// ON. This is the race surface ThreadSanitizer watches in CI (the `tsan` job
/// filters on this test name); TSan is unrunnable on arm64-macOS, so under
/// normal `cargo test` this proves two things TSan does not: the harness runs
/// without panicking, and the shared `in_flight` counter is *balanced* — every
/// account returns to `in_flight == 0` once all guards drop. A leak, a
/// double-decrement, or a lost increment across the check-then-act-across-locks
/// path shows up here as a nonzero residual even without instrumentation.
#[test]
fn concurrent_pacing_stress() {
    const THREADS: usize = 8;
    const ITERS: usize = 2000;

    let pacing = PacingConfig {
        max_in_flight_per_account: Some(2),
        min_spacing_ms: None,
    };
    let manager = build_manager(
        config_with_pacing(
            vec![
                account("a0", 0),
                account("a1", 0),
                account("a2", 0),
                account("a3", 0),
            ],
            pacing,
        ),
        counting_refresher(),
    );

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let manager = Arc::clone(&manager);
            std::thread::spawn(move || {
                let empty: HashSet<usize> = HashSet::new();
                for i in 0..ITERS {
                    let now = OffsetDateTime::now_utc();
                    // Vary the session key per thread AND per iteration so
                    // affinity pins spread across accounts and the pin map
                    // churns — maximising concurrent affinity-lock traffic.
                    let session_key = Some((t as u64) << 32 | (i as u64 % 16));
                    if let Some(idx) =
                        manager.select(&empty, now, None, session_key, "/v1/messages", None)
                    {
                        // Take the in-flight slot, hold it briefly to widen the
                        // window where a concurrent select/writer observes a
                        // nonzero count, then drop the guard (decrement).
                        let guard = manager.enter_in_flight(idx);
                        for _ in 0..8 {
                            std::hint::spin_loop();
                        }
                        // Occasionally fire the other account-lock writers so
                        // TSan sees select/enter racing real mutation, not just
                        // the in_flight path.
                        if i % 17 == 0 {
                            manager.record_served(idx, now, session_key, SessionKind::Fallback);
                        }
                        if i % 53 == 0 {
                            // Short hold so accounts recover and stay selectable
                            // for the rest of the run.
                            manager.mark_rate_limited(idx, 1);
                        }
                        drop(guard);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        // A panic in any thread (e.g. a poisoned lock from an unwind under the
        // write-lock) propagates here and fails the test.
        h.join().expect("stress thread panicked");
    }

    // Every guard has dropped, so the counter MUST be balanced back to zero on
    // every account. A nonzero residual is a real concurrency bug (leaked or
    // double-counted in_flight) — surface it, never silence it.
    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    for (idx, account) in accounts.iter().enumerate() {
        assert_eq!(
            account.in_flight, 0,
            "account {idx} ({}) leaked in_flight={} after all guards dropped",
            account.name, account.in_flight
        );
    }
}

/// TOCTOU regression (concurrency) — the migration feature's OWN target workload,
/// with `loadBalanceMigration` explicitly ENABLED (it ships OFF).
/// Several sessions start STACKED on one account with idle, eligible accounts
/// alongside. `N` threads select for those sessions in lockstep: a `Barrier`
/// forces every round to fire simultaneously, which is the exact interleaving the
/// adversarial review flagged — two concurrent selects both reading `count(Y)=0`
/// for the same idle `Y` and BOTH migrating onto it, over-stacking `Y` to 2 while
/// `X` empties to 0 (the inverse of the goal) and oscillating 0→1→0.
///
/// The fix — re-validating `count(target)+1 < count(X)` against FRESH counts under
/// the affinity lock in section 3, aborting the move if it no longer holds — makes
/// each committed migration strictly reduce the load's sum-of-squares, so the
/// system converges to exactly one session per account and STAYS there. Under the
/// pre-fix code the stale-count commit over-migrates and the final distribution is
/// NOT balanced. (Asserted on a balanced fleet — no pacing / rate-limit churn — so
/// the only force acting on the distribution is the migration logic under test.)
#[test]
fn concurrent_stacked_sessions_do_not_over_migrate() {
    const N: usize = 4; // sessions == accounts → balanced fixed point is one-each
    const ROUNDS: usize = 100;

    let manager = build_manager(
        config_with_migration(vec![
            account("a0", 0),
            account("a1", 0),
            account("a2", 0),
            account("a3", 0),
        ]),
        counting_refresher(),
    );
    // Pathological start: ALL N sessions pinned to account 0.
    for key in 0..N as u64 {
        pin_session(&manager, key, 0);
    }

    let barrier = Arc::new(std::sync::Barrier::new(N));
    let handles: Vec<_> = (0..N as u64)
        .map(|key| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let empty: HashSet<usize> = HashSet::new();
                for _ in 0..ROUNDS {
                    // Synchronise so all N selects contend on the same stacked
                    // account in the same instant — maximising the TOCTOU window.
                    barrier.wait();
                    let now = OffsetDateTime::now_utc();
                    manager.select(&empty, now, None, Some(key), "/v1/messages", None);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("migration stress thread panicked");
    }

    // Every session is still pinned (none lost), and the distribution converged
    // to exactly one-per-account. An account holding >=2 while another sits at 0
    // is the over-migration / oscillation the re-validation exists to prevent.
    let pins = manager.affinity.lock().expect("affinity lock poisoned");
    assert_eq!(pins.len(), N, "every session must remain pinned");
    let mut per_account = [0usize; N];
    for &(idx, _) in pins.values() {
        per_account[idx] += 1;
    }
    assert_eq!(
        per_account,
        [1, 1, 1, 1],
        "stacked sessions must converge to one-per-account without over-migrating \
         (got {per_account:?}) — a stale-count commit would over-stack one account"
    );
}

// ---- over-threshold revalidation-serve (last-resort; default ON) ----------

/// Drive account `idx` OVER the soft switch threshold on its shared weekly
/// (`7d`) window (future reset) and stamp `unifiedStatus`, so normal `select`
/// benches it but `select_revalidation` can still consider it. `util` is the
/// weekly utilization; `status` the reported unified status.
fn set_over_threshold(manager: &Manager, idx: usize, util: f64, status: &str) {
    let now = OffsetDateTime::now_utc();
    let mut a = manager.accounts.write().expect("accounts lock poisoned");
    let account = &mut a[idx];
    account.quota.seven_day = Some(crate::quota::QuotaWindow {
        utilization: util,
        reset: Some(now + Duration::hours(48)),
    });
    account.quota.status = Some(status.to_string());
}

/// #1 Whole fleet over the soft threshold, all `allowed_warning`: normal
/// `select` is `None` but `select_revalidation` serves the LOWEST-utilization
/// account.
#[test]
fn revalidation_serves_least_utilized_when_all_over_threshold() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    set_over_threshold(&manager, 0, 0.995, "allowed_warning");
    set_over_threshold(&manager, 1, 0.970, "allowed_warning"); // least utilized
    set_over_threshold(&manager, 2, 0.999, "allowed_warning");

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        None,
        "every account is over the soft threshold → normal select benches all"
    );
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(1),
        "revalidation serves the least-utilized allowed account"
    );
}

/// #1b THE cache-warmth test: a session pinned (affinity) to an over-threshold
/// but `allowed_warning` account X — with OTHER accounts LESS utilized — must be
/// served X (its warm pin), NOT the least-utilized other, and must NOT consume
/// the fallback anti-storm window. If X becomes `rejected`, it falls back to the
/// least-utilized survivor and RE-PINS the session there.
#[test]
fn revalidation_honors_pin_through_threshold() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    // The pinned account (idx 2) is the MOST utilized; the others are cooler.
    set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&manager, 1, 0.970, "allowed_warning");
    set_over_threshold(&manager, 2, 0.999, "allowed_warning"); // the warm pin
    let key = 424_242u64;
    let now = OffsetDateTime::now_utc();
    seed_pin(&manager, key, 2, odt_to_ms(now));

    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, Some(key), "/v1/messages"),
        Some(2),
        "pin-honor serves the session's warm pinned account, NOT the least-utilized other"
    );
    // The pin-honor path must NOT have burned the fallback throttle window: an
    // unaffiliated (no-pin) fallback serve still succeeds immediately.
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(0),
        "pin-honor does not consume the fallback anti-storm window"
    );

    // When the pin becomes HARD-blocked (rejected), fall back to least-utilized
    // and RE-PIN the session there. Use a fresh manager (throttle valve reset).
    let manager2 = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    set_over_threshold(&manager2, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&manager2, 1, 0.970, "allowed_warning");
    set_over_threshold(&manager2, 2, 0.999, "rejected"); // pin now blocked
    seed_pin(&manager2, key, 2, odt_to_ms(now));
    assert_eq!(
        manager2.select_revalidation(&HashSet::new(), now, None, Some(key), "/v1/messages"),
        Some(0),
        "a rejected pin falls back to the least-utilized survivor"
    );
    let repinned = pin_of(&manager2, key);
    assert_eq!(
        repinned,
        Some(0),
        "the fallback serve re-pins the session to the account it chose"
    );
}

/// #1c `tried` governs whether an account may SERVE this request; it must never
/// decide whether that session's PIN may MOVE. A pin that merely failed THIS
/// request — a transport blip, a 5xx — while still clearing every ACCOUNT-level
/// HARD gate stays put: the fallback serves elsewhere for this one request and
/// the session comes home on the next.
///
/// Before the fix the `account_hard_ok → keep_pin` test sat INSIDE
/// `if !tried.contains(&idx)`, so a tried pin left `keep_pin` at `None` and the
/// fallback's `pins.insert` durably re-keyed the session — a warm prompt cache
/// discarded on the strength of one failed request. `select()` takes the opposite
/// decision on the identical fact and documents why; this is the two agreeing.
///
/// Also the first coverage of a NON-EMPTY `tried` on this path at all — every
/// other `select_revalidation` test passes `&HashSet::new()`.
#[test]
fn revalidation_keeps_the_pin_when_it_is_merely_tried() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&manager, 1, 0.970, "allowed_warning");
    set_over_threshold(&manager, 2, 0.999, "allowed_warning"); // the warm pin
    let key = 515_151u64;
    let now = OffsetDateTime::now_utc();
    seed_pin(&manager, key, 2, odt_to_ms(now));

    // The pin failed THIS request and nothing more: not disabled, not errored,
    // not rejected, no hold. It clears every ACCOUNT-level hard gate.
    let tried = HashSet::from([2usize]);
    assert_eq!(
        manager.select_revalidation(&tried, now, None, Some(key), "/v1/messages"),
        Some(0),
        "a tried pin cannot serve THIS request → the least-utilized survivor does"
    );
    let pin_after = pin_of(&manager, key);
    assert_eq!(
        pin_after,
        Some(2),
        "the pin must NOT move: membership in `tried` is a per-request fact, not \
         evidence the account is gone"
    );
}

/// #1d The over-correction guard for #1c. Hoisting the `keep_pin` test out of the
/// `!tried` guard must not make a pin STICKY: a pin that fails an ACCOUNT-level
/// HARD gate still re-keys, and being in `tried` as well changes nothing. Both
/// hard shapes are exercised — `quota.status == "rejected"`, and a hold that
/// outlives the prompt cache (>= `CACHE_WARM_HOLD_SECS`).
#[test]
fn revalidation_rekeys_when_the_pin_fails_a_hard_gate() {
    let key = 626_262u64;
    let now = OffsetDateTime::now_utc();
    let tried = HashSet::from([2usize]);
    let pin_session = |manager: &Manager, idx: usize| seed_pin(manager, key, idx, odt_to_ms(now));
    let pin_of = |manager: &Manager| pin_of(manager, key);

    // (a) REJECTED and tried — a durable block, so the session must move off it.
    let rejected = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    set_over_threshold(&rejected, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&rejected, 1, 0.970, "allowed_warning");
    set_over_threshold(&rejected, 2, 0.999, "rejected");
    pin_session(&rejected, 2);
    assert_eq!(
        rejected.select_revalidation(&tried, now, None, Some(key), "/v1/messages"),
        Some(0),
        "a rejected pin is skipped; the least-utilized survivor serves"
    );
    assert_eq!(
        pin_of(&rejected),
        Some(0),
        "a rejected pin RE-KEYS — `tried` must not shield it from the hard gate"
    );

    // (b) HELD PAST THE CACHE and tried — also durable (a fresh manager, so the
    // anti-storm valve the serve above armed does not swallow this call).
    let held = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    set_over_threshold(&held, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&held, 1, 0.970, "allowed_warning");
    set_over_threshold(&held, 2, 0.999, "allowed_warning");
    {
        let mut a = held.accounts.write().expect("accounts lock poisoned");
        a[2].rate_limited_until_ms = Some(crate::now_ms() + 3_600_000);
    }
    pin_session(&held, 2);
    assert_eq!(
        held.select_revalidation(&tried, now, None, Some(key), "/v1/messages"),
        Some(0),
        "a pin held past its cache lifetime is skipped; the survivor serves"
    );
    assert_eq!(
        pin_of(&held),
        Some(0),
        "a hold that outlives the cache RE-KEYS — `tried` must not shield it"
    );
}

/// #2 A `rejected` account is skipped even when least-utilized; a higher-util
/// `allowed` one is served. If ALL are `rejected` → `None`.
#[test]
fn revalidation_skips_rejected_accounts() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    set_over_threshold(&manager, 0, 0.960, "rejected"); // least util but blocked
    set_over_threshold(&manager, 1, 0.990, "allowed_warning");

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(1),
        "the least-utilized account is rejected → skip it, serve the allowed one"
    );

    // Now reject BOTH → nothing servable → None.
    set_over_threshold(&manager, 1, 0.990, "rejected");
    // A fresh manager to reset the anti-storm valve (previous serve armed it).
    let manager2 = manager_of(vec![account("a", 0), account("b", 0)]);
    set_over_threshold(&manager2, 0, 0.960, "rejected");
    set_over_threshold(&manager2, 1, 0.990, "rejected");
    assert_eq!(
        manager2.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        None,
        "all accounts rejected → no revalidation target, honest 429"
    );
}

/// #3 An account under a live rate-limit hold is skipped even if least-utilized.
#[test]
fn revalidation_skips_hard_held_account() {
    let manager = manager_of(vec![account("held", 0), account("free", 0)]);
    set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least util …
    set_over_threshold(&manager, 1, 0.990, "allowed_warning");
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].rate_limited_until_ms = Some(crate::now_ms() + 60_000); // … but held.
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(1),
        "a live-held account is a HARD block — skipped even as least-utilized"
    );
}

/// #4 Anti-storm: two back-to-back calls — the first serves, the second (inside
/// `REVALIDATION_MIN_SPACING_MS`) returns `None`.
#[test]
fn revalidation_throttle_spacing_respected() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    set_over_threshold(&manager, 0, 0.970, "allowed_warning");
    set_over_threshold(&manager, 1, 0.990, "allowed_warning");
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(0),
        "first call serves"
    );
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        None,
        "a second call within the spacing window is throttled to None"
    );
}

/// #5 When one account is UNDER threshold, normal `select` returns it and the
/// revalidation path is never consulted — the normal path stays unchanged.
#[test]
fn revalidation_not_consulted_when_an_account_is_eligible() {
    let manager = manager_of(vec![account("hot", 0), account("cool", 0)]);
    set_over_threshold(&manager, 0, 0.999, "allowed_warning");
    // account 1 stays healthy (no quota set) → under threshold → eligible.
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "an under-threshold account is served by the normal path; revalidation \
         is only consulted after select() returns None"
    );
}

/// #6 A Fable request skips an account whose Fable weekly (`7d_oi`) is a hard
/// reject, but a non-Fable request serves that same account.
#[test]
fn revalidation_fable_skips_fable_exhausted() {
    let build = || {
        let manager = manager_of(vec![account("fable-full", 0), account("other", 0)]);
        let now = OffsetDateTime::now_utc();
        // account 0: least-utilized on shared dims but Fable weekly exhausted.
        set_over_threshold(&manager, 0, 0.950, "allowed_warning");
        set_over_threshold(&manager, 1, 0.990, "allowed_warning");
        set_fable_exhausted(&manager, 0, 0.999);
        (manager, now)
    };

    // Fable request: account 0's Fable weekly is a hard reject → skip → serve 1.
    let (manager, now) = build();
    assert_eq!(
        manager.select_revalidation(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            None,
            "/v1/messages"
        ),
        Some(1),
        "a Fable request skips the Fable-exhausted account even when least-utilized"
    );

    // Non-Fable request on a fresh manager: account 0 IS served (Fable weekly
    // gates Fable traffic only; on shared dims account 0 is least-utilized).
    let (manager, now) = build();
    assert_eq!(
        manager.select_revalidation(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            None,
            "/v1/messages"
        ),
        Some(0),
        "a non-Fable request still serves the Fable-exhausted account"
    );
}

/// A manager with no account, for the Codex traffic counts, which no account owns.
fn traffic_manager() -> Arc<Manager> {
    manager_of(Vec::new())
}

/// The refused count covers the last ten minutes, by the minute: a 429 drops out once ten
/// whole minutes have passed since the minute it was sent in. The latest refusal's type and
/// time ride with the count, and go when it reaches zero, so the wire never pairs a zero
/// count with a stale type.
///
/// Watched red: summing all ten buckets whatever their minute keeps the first refusal in
/// the count at the tenth minute.
#[test]
fn the_refused_ring_counts_ten_minutes_of_codex_429s() {
    // 2026-01-01T00:00:00Z, on a minute boundary.
    let t0: i64 = 1_767_225_600_000;
    let manager = traffic_manager();
    manager.note_codex_refusal(tcr_status_wire::CodexRefusalKind::UsageLimitReached, t0);
    manager.note_codex_refusal(
        tcr_status_wire::CodexRefusalKind::PoolUnavailable,
        t0 + 300_000,
    );
    assert_eq!(
        manager.codex_traffic(t0 + 599_000),
        tcr_status_wire::CodexTraffic {
            refused_10m: 2,
            last_refusal: Some(tcr_status_wire::CodexRefusalKind::PoolUnavailable),
            last_refusal_at_ms: Some(t0 + 300_000),
            unpooled_10m: 0,
        }
    );
    assert_eq!(manager.codex_traffic(t0 + 600_000).refused_10m, 1);
    let emptied = manager.codex_traffic(t0 + 900_000);
    assert_eq!(emptied.refused_10m, 0);
    assert_eq!(emptied.last_refusal, None);
    assert_eq!(emptied.last_refusal_at_ms, None);
}

/// The unpooled count covers the same ten minutes, by the same minute buckets.
#[test]
fn the_unpooled_ring_counts_routed_relays() {
    let t0: i64 = 1_767_225_600_000;
    let manager = traffic_manager();
    for _ in 0..3 {
        manager.note_codex_unpooled(t0);
    }
    assert_eq!(manager.codex_traffic(t0 + 599_000).unpooled_10m, 3);
    assert_eq!(manager.codex_traffic(t0 + 600_000).unpooled_10m, 0);
    assert_eq!(manager.codex_traffic(t0 + 599_000).refused_10m, 0);
}

/// The extended-cache set stays bounded with no pin flush to prune it: with affinity off
/// (no file, no flusher) every keyed request that asked for the 1h cache used to add a
/// key for good. Over the cap the least recently noted key goes, and the newest stays.
#[test]
fn the_extended_cache_set_is_capped_without_a_pin_flush() {
    let manager = manager_of(vec![account("a", 0)]);
    let noted = select::AFFINITY_CAP as u64 * 2;
    for key in 0..noted {
        manager.note_affinity_ttl(key, true);
    }
    let held = manager
        .affinity_extended
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    assert_eq!(held, select::AFFINITY_CAP);
    assert_eq!(manager.warm_window(Some(noted - 1)), WarmWindow::EXTENDED);
    assert_eq!(manager.warm_window(Some(0)), WarmWindow::DEFAULT);
}

/// `group` settings as `(reserved, spill_to_pool)` for the one group `g`.
fn config_with_g_settings(accounts: Vec<Account>, reserved: bool, spill: bool) -> Config {
    let mut config = config_with(accounts);
    config.group_settings.insert(
        "g".to_string(),
        config::GroupSettings {
            reserved,
            spill_to_pool: spill,
            ..Default::default()
        },
    );
    config
}

/// A `--group g` select reads every group setting from ONE version of the config.
///
/// Two versions, both strict for `g`: (reserved false, spill false) and (reserved true,
/// spill true). The only member of `g` is disabled, so a strict ask refuses and a spill
/// ask lands on the outsider `o`. A select that read `reserved` from the first version and
/// `spill` from the second would see (false, true), the one mix that is not strict, and
/// route the request to `o`. A thread flips between the two versions while this one
/// selects; any `Some` is a torn read.
#[test]
fn a_group_reload_mid_select_never_routes_on_a_mix_of_two_versions() {
    let accounts = || {
        vec![
            Account {
                disabled: Some(true),
                ..account_in_groups("m", 0, &["g"])
            },
            account("o", 0),
        ]
    };
    let neither = config_with_g_settings(accounts(), false, false);
    let both = config_with_g_settings(accounts(), true, true);
    let refresher = || counting_refresher();
    let select_g = |manager: &Manager| {
        manager.select_with_group(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            "/v1/messages",
            None,
            Some("g"),
        )
    };

    // Positive control: the mixed version itself does route to `o`.
    let mixed = build_manager(config_with_g_settings(accounts(), false, true), refresher());
    assert_eq!(select_g(&mixed), Some(1), "the torn mix must route to `o`");
    let manager = build_manager(neither.clone(), refresher());
    assert_eq!(select_g(&manager), None, "(false, false) is strict");
    manager.apply_group_reload(&both);
    assert_eq!(select_g(&manager), None, "(true, true) is strict");

    let stop = Arc::new(AtomicBool::new(false));
    let flipper = {
        let manager = Arc::clone(&manager);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                manager.apply_group_reload(&neither);
                manager.apply_group_reload(&both);
            }
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut torn = 0usize;
    let mut selects = 0usize;
    while std::time::Instant::now() < deadline {
        selects += 1;
        if select_g(&manager).is_some() {
            torn += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    assert!(flipper.join().is_ok(), "the reload thread panicked");
    assert_eq!(torn, 0, "{torn} of {selects} selects routed on a torn read");
}
