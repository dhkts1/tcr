//! Shared fixtures for the manager tests: account and config builders, and the fake
//! refreshers, probers and warmers more than one subject file uses.

use super::*;
use crate::base::config::{Account, PacingConfig};
use crate::clients::oauth::{OAuthError, RefreshFuture};
use crate::clients::warmer::{AccountWarmer, WarmError, WarmFuture};
use crate::records::probe::{ProbeError, ProbeFuture, Usage, UsageBucket};
use std::sync::atomic::{AtomicUsize, Ordering};
use time::Duration;

mod add_account;
mod affinity;
mod codex;
mod control;
mod gates;
mod group_policy;
mod keep_warm;
mod migration;
mod pacing;
mod persistence;
mod probes;
mod selection;
mod stats_snapshot;
mod token_refresh;

/// A rate-limit hold that clears while the pinned account's prompt cache is
/// still warm — the SOFT case. The live value of a no-guidance transient park
/// (`proxy::NO_GUIDANCE_HOLD_SECS`), which is the hold this fleet arms most.
const SHORT_HOLD_SECS: i64 = 15;

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
        _provider: crate::base::provider::Provider,
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

struct NoProber;
impl UsageProber for NoProber {
    fn probe(&self, _credential: crate::records::probe::ProbeCredential) -> ProbeFuture {
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
    fn probe(&self, credential: crate::records::probe::ProbeCredential) -> ProbeFuture {
        let ok = credential.access_token == self.ok_token;
        Box::pin(async move {
            if ok {
                Ok(Usage {
                    five_hour: crate::records::probe::WindowReading::Read(UsageBucket {
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

/// A plan prober that answers one canned reading and counts its calls — the
/// counter is the point, not decoration: the backfill's whole contract is
/// that it runs ONCE per account and never again.
struct CountingPlanProber {
    plan: crate::records::probe::Plan,
    calls: Arc<AtomicUsize>,
}

impl PlanProber for CountingPlanProber {
    fn plan(&self, _access_token: String) -> crate::records::probe::PlanFuture {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let plan = self.plan.clone();
        Box::pin(async move { plan })
    }
}

/// Give `manager` a [`CountingPlanProber`] that answers `plan`, and return its call count.
fn count_plan_fetches(manager: &Manager, plan: crate::records::probe::Plan) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    manager.set_plan_prober(Arc::new(CountingPlanProber {
        plan,
        calls: Arc::clone(&calls),
    }));
    calls
}

/// A prober that records the credential of every probe and answers an empty
/// success, so a test reads exactly what [`Manager::probe_account`] built.
struct RecordingProber {
    seen: Arc<Mutex<Vec<crate::records::probe::ProbeCredential>>>,
}

impl UsageProber for RecordingProber {
    fn probe(&self, credential: crate::records::probe::ProbeCredential) -> ProbeFuture {
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

/// A refresher that rejects every refresh as `status` with `code`, the way a dead login is
/// rejected.
struct RejectsWith {
    status: u16,
    code: Option<&'static str>,
}
impl TokenRefresher for RejectsWith {
    fn refresh(
        &self,
        _provider: crate::base::provider::Provider,
        _refresh_token: String,
    ) -> RefreshFuture {
        let (status, code) = (self.status, self.code);
        Box::pin(async move { Err(OAuthError::AuthRejected { status, code }) })
    }
}

fn codex_account(name: &str, priority: i64) -> Account {
    Account {
        provider: crate::base::provider::Provider::Codex,
        account_uuid: Some("55555555-5555-4555-8555-555555555555".to_string()),
        org_uuid: Some("66666666-6666-4666-8666-666666666666".to_string()),
        ..account(name, priority)
    }
}

/// A unique temp path per test — the suite runs tests in parallel threads of
/// ONE process, so a pid-only name collides.
fn tmp_config_path(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("tcr-mgr-{tag}-{}-{seq}.json", std::process::id()))
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

fn config_with_control(accounts: Vec<Account>, control: &str) -> Config {
    let mut config = config_with(accounts);
    config.control_account = Some(config::ControlAccountRef::Name(control.to_string()));
    config
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

/// Write access to the runtime rows, for tests that rewind a timestamp.
fn accounts_mut(manager: &Manager) -> std::sync::RwLockWriteGuard<'_, Vec<AccountRuntime>> {
    manager.accounts.write().expect("accounts lock poisoned")
}

/// Drive account `idx` over the soft threshold on its MODEL-SCOPED weekly
/// (`7d_oi`, the Fable bucket) with a reset 48h out, leaving every shared
/// dimension healthy: the account is out of Fable and serves everything else
/// perfectly. The companion to [`set_over_threshold`], which moves the SHARED
/// weekly instead.
fn set_fable_exhausted(manager: &Manager, idx: usize, util: f64) {
    let now = OffsetDateTime::now_utc();
    let mut a = manager.accounts.write().expect("accounts lock poisoned");
    a[idx].quota.seven_day_oi = Some(crate::records::quota::QuotaWindow {
        utilization: util,
        reset: Some(now + Duration::hours(48)),
    });
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

/// Read the account a session key is currently pinned to (None if unpinned).
fn pin_of(manager: &Manager, key: u64) -> Option<usize> {
    manager
        .affinity
        .lock()
        .expect("affinity lock poisoned")
        .get(&key)
        .map(|&(idx, _)| idx)
}

fn read_config_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read test config"))
        .expect("test config is valid JSON")
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

// ---- over-threshold revalidation-serve (last-resort; default ON) ----------

/// Drive account `idx` OVER the soft switch threshold on its shared weekly
/// (`7d`) window (future reset) and stamp `unifiedStatus`, so normal `select`
/// benches it but `select_revalidation` can still consider it. `util` is the
/// weekly utilization; `status` the reported unified status.
fn set_over_threshold(manager: &Manager, idx: usize, util: f64, status: &str) {
    let now = OffsetDateTime::now_utc();
    let mut a = manager.accounts.write().expect("accounts lock poisoned");
    let account = &mut a[idx];
    account.quota.seven_day = Some(crate::records::quota::QuotaWindow {
        utilization: util,
        reset: Some(now + Duration::hours(48)),
    });
    account.quota.status = Some(status.to_string());
}
