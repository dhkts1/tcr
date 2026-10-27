//! Workload scenario tests — real Claude Code traffic shapes driven through the
//! real selector, with **prompt-cache continuity** as the oracle.
//!
//! ## Why this file exists
//!
//! The inline unit suite passed in full while the live proxy destroyed 76.7% of
//! all prompt-cache creation: 13,033 mid-session cold starts and 39.4% of
//! requests switching account mid-conversation over 7 days. Unit tests of
//! `select()` cannot catch that class — each one asks "given this state, is the
//! next pick right?", while the damage is a property of a *sequence* of picks
//! under a *concurrent* arrival pattern. So these tests do not assert on one
//! call; they replay a workload and assert on the shape of the whole serve
//! history.
//!
//! ## The metric
//!
//! For one lineage (= one Claude Code conversation, keyed on the stable client
//! identity the proxy pins on), `switches` counts consecutive serves that landed
//! on *different* accounts, and
//!
//! ```text
//! continuity = 1.0 - switches / pairs      (pairs = requests - 1)
//! ```
//!
//! Anthropic's prompt cache is **per account**, so every switch re-creates the
//! whole conversation prefix upstream. `continuity == 1.0` means the whole
//! conversation was served warm.
//!
//! One deliberate refinement over raw `continuity`: a session that is *diverted*
//! for a single request by the soft pacing gate and then comes straight back
//! scores *worse* on `continuity` than one that is durably re-keyed and never
//! returns — the second serve back home counts as a second switch. That ranking
//! is backwards for cache economics (coming home is a cache HIT), so scenarios
//! that legitimately divert also assert on [`Fleet::home_share`] and on the pin
//! itself (the account the next select returns once the burst drains), which is
//! the invariant the soft-divert fix actually guarantees.
//!
//! ## Determinism
//!
//! Every scenario advances a fixed epoch by hand. Explicit-time delegates stamp
//! sends, fold quota and arm holds on that same clock; production callers retain
//! wall-clock wrappers. The replay below never sleeps or calls an upstream.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tcr::config::{Account, Config, PacingConfig};
use tcr::manager::{InFlightGuard, Manager};
use tcr::model::is_fable_model;
use tcr::oauth::{OAuthError, RefreshFuture, TokenRefresher};
use tcr::probe::{ProbeError, ProbeFuture, Usage, UsageBucket, UsageProber};
use tcr::warmer::{AccountWarmer, WarmError, WarmFuture};
use time::{Duration, OffsetDateTime};

// ---------------------------------------------------------------------------
// Part A — the harness
// ---------------------------------------------------------------------------

/// The in-flight cap these scenarios exercise: three requests may be in flight on
/// one account before it yields. This is the harness's own choice, NOT the
/// production default — `config::default_pacing` ships pacing OFF (no cap). The
/// scenarios build their `pacing(CAP)` explicitly so the capped path stays covered
/// on purpose; the burst scenarios are calibrated against this number, so it is
/// named once here.
const CAP: u32 = 3;

/// The default soft switch threshold (`config::default_switch_threshold`).
const SWITCH_THRESHOLD: f64 = 0.95;

/// Metric key under which identity-less (unpinned) traffic is recorded. Never
/// passed to `select` as an affinity — those calls pass `None`, which is the
/// whole point of scenario 6.
const ANON_KEY: u64 = 0;

/// The two model classes one Claude Code session interleaves: the conversation
/// turns, and the one-line title/summary calls that ride the same identity.
/// `is_fable_model` is a substring test, so these are the ids the proxy sees.
const OPUS: &str = "claude-opus-4-6";
const FABLE: &str = "claude-fable-5";

/// A fixed simulation epoch (2026-01-01T00:00:00Z). Every scenario advances
/// `now` from here by hand, so a run is identical on every machine and clock.
fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_225_600).expect("fixed epoch is valid")
}

const fn to_ms(at: OffsetDateTime) -> i64 {
    (at.unix_timestamp_nanos() / 1_000_000) as i64
}

/// Mirrors the inline unit helper of the same name (`src/manager/mod.rs`): one
/// config account with a priority and a token that is fresh far into the future.
fn account(name: &str, priority: i64) -> Account {
    Account {
        expires_at: Some(to_ms(t0()) + 3_600_000),
        priority: Some(priority),
        ..Account::fixture(name)
    }
}

/// Per-account pacing with the production cap and no min-spacing (see the module
/// header for why min-spacing stays off).
const fn pacing(cap: u32) -> PacingConfig {
    PacingConfig {
        max_in_flight_per_account: Some(cap),
        min_spacing_ms: None,
    }
}

/// None of these are ever invoked: `select` and `enter_in_flight` touch neither
/// the network nor the token/probe/warm paths. They exist because `Manager::new`
/// takes them.
struct NeverRefreshes;
impl TokenRefresher for NeverRefreshes {
    fn refresh(&self, _provider: tcr::provider::Provider, _refresh_token: String) -> RefreshFuture {
        Box::pin(async {
            Err(OAuthError::Transient(
                "no refresher in workload tests".into(),
            ))
        })
    }
}

struct NeverProbes;
impl UsageProber for NeverProbes {
    fn probe(&self, _credential: tcr::probe::ProbeCredential) -> ProbeFuture {
        Box::pin(async {
            Err(ProbeError {
                status: None,
                message: "no prober in workload tests".into(),
                retry_after_secs: None,
            })
        })
    }
}

struct NeverWarms;
impl AccountWarmer for NeverWarms {
    fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
        Box::pin(async {
            Err(WarmError {
                status: None,
                message: "no warmer in workload tests".into(),
            })
        })
    }
}

/// One Claude Code conversation. A lineage issues requests as
/// `select(&tried, now, model, Some(key))` — the same shape the proxy uses once
/// it has resolved a stable client identity (`x-api-key` / `metadata.user_id`).
///
/// Subagents and workflow workers inherit their parent's identity, so a fan-out
/// is modelled as *many concurrent requests on one lineage*, not many lineages.
#[derive(Clone, Copy)]
struct Lineage {
    key: u64,
    name: &'static str,
}

/// One recorded serve: the lineage that issued it, the account that answered it,
/// and whether it was a Fable-class request. The class is recorded because a real
/// session mixes classes, and only the NON-Fable subsequence carries the expensive
/// prompt cache — a diverted one-line title call costs nothing, while a diverted
/// conversation turn re-creates a 200k-token prefix.
#[derive(Clone, Copy)]
struct Serve {
    key: u64,
    account: usize,
    is_fable: bool,
}

/// A fleet of accounts plus the recorded serve history that the metric is
/// computed from.
struct Fleet {
    manager: Arc<Manager>,
    account_names: Vec<String>,
    /// Every serve, in issue order.
    serves: Vec<Serve>,
    /// Lineage display names in first-registration order (report ordering).
    lineages: Vec<(u64, &'static str)>,
    next_key: u64,
}

impl Fleet {
    /// Build a manager over `accounts` (name, priority) with `pacing` and the
    /// default soft switch threshold. `config_path: None` makes every persist a
    /// no-op, so nothing can reach disk.
    fn new(accounts: &[(&str, i64)], pacing: PacingConfig) -> Self {
        // The global egress throttle stays at its inert default: it is an async sleep on the
        // send path and has no bearing on selection.
        let config = Config {
            switch_threshold: SWITCH_THRESHOLD,
            pacing,
            ..Config::fixture(accounts.iter().map(|(n, p)| account(n, *p)).collect())
        };
        Self::from_config(config)
    }

    fn from_config(config: Config) -> Self {
        let account_names = config.accounts.iter().map(|a| a.name.clone()).collect();
        let manager = Manager::new(
            config,
            Arc::new(NeverRefreshes),
            Arc::new(NeverProbes),
            Arc::new(NeverWarms),
            None,
        );
        let mut fleet = Self {
            manager,
            account_names,
            serves: Vec::new(),
            lineages: Vec::new(),
            next_key: 1,
        };
        fleet.lineages.push((ANON_KEY, "anonymous"));
        fleet
    }

    /// A healthy same-priority fleet of `n` accounts at the production cap.
    fn healthy(n: usize) -> Self {
        const POOL: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
        let accounts: Vec<(&str, i64)> = POOL[..n].iter().map(|n| (*n, 0)).collect();
        Self::new(&accounts, pacing(CAP))
    }

    /// Register a new conversation. Keys are handed out densely from 1 so
    /// `ANON_KEY` (0) can never collide with a real lineage.
    fn lineage(&mut self, name: &'static str) -> Lineage {
        let key = self.next_key;
        self.next_key += 1;
        self.lineages.push((key, name));
        Lineage { key, name }
    }

    /// One request on `lineage`, recorded. Panics (with the report) if the fleet
    /// refuses to serve — a dropped request is always a scenario bug here, since
    /// every scenario keeps at least one account servable.
    fn serve(&mut self, lineage: Lineage, now: OffsetDateTime) -> usize {
        self.serve_with_tried(lineage, &HashSet::new(), now)
    }

    /// One request on `lineage` that has already failed on the accounts in
    /// `tried` — the shape the proxy uses when it rotates after an upstream
    /// error.
    fn serve_with_tried(
        &mut self,
        lineage: Lineage,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
    ) -> usize {
        self.serve_inner(lineage, tried, None, now)
    }

    /// One request on `lineage` targeting `model` — the shape a mixed-class
    /// session issues, where the conversation turns and the title/summary calls
    /// share one identity but not one quota bucket.
    fn serve_model(&mut self, lineage: Lineage, model: &str, now: OffsetDateTime) -> usize {
        self.serve_inner(lineage, &HashSet::new(), Some(model), now)
    }

    fn serve_inner(
        &mut self,
        lineage: Lineage,
        tried: &HashSet<usize>,
        model: Option<&str>,
        now: OffsetDateTime,
    ) -> usize {
        match self
            .manager
            .select(tried, now, model, Some(lineage.key), "/v1/messages", None)
        {
            Some(account) => {
                self.serves.push(Serve {
                    key: lineage.key,
                    account,
                    is_fable: model.is_some_and(is_fable_model),
                });
                account
            }
            None => panic!(
                "fleet refused to serve lineage {}\n{}",
                lineage.name,
                self.report()
            ),
        }
    }

    /// One identity-less request: no affinity key, so the selector must route it
    /// without creating or touching a pin.
    fn serve_anon(&mut self, now: OffsetDateTime) -> usize {
        match self
            .manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
        {
            Some(account) => {
                self.serves.push(Serve {
                    key: ANON_KEY,
                    account,
                    is_fable: false,
                });
                account
            }
            None => panic!("fleet refused an identity-less request\n{}", self.report()),
        }
    }

    /// A request on `lineage` whose response is still streaming: the returned
    /// guard holds the account's `in_flight` slot until it is dropped. This is
    /// how a burst saturates an account — exactly as the proxy does when it
    /// moves the guard into an SSE body.
    fn stream(&mut self, lineage: Lineage, now: OffsetDateTime) -> InFlightGuard {
        let idx = self.serve(lineage, now);
        self.manager.enter_in_flight_at(idx, now)
    }

    /// As [`Self::stream`], for identity-less traffic.
    fn stream_anon(&mut self, now: OffsetDateTime) -> InFlightGuard {
        let idx = self.serve_anon(now);
        self.manager.enter_in_flight_at(idx, now)
    }

    /// Drive account `idx` over the soft switch threshold on its shared weekly
    /// (`7d`) window with a reset 48h out, through the same public path a
    /// background quota probe uses. Mirrors the inline unit helper
    /// `set_over_threshold` (`src/manager/mod.rs`); the reset is supplied
    /// explicitly so the fold is not clock-dependent.
    fn set_over_threshold(&self, idx: usize, util: f64, now: OffsetDateTime) {
        self.manager.apply_usage_at(
            idx,
            &Usage {
                seven_day: Some(UsageBucket {
                    utilization: Some(util),
                    reset_at_ms: Some(to_ms(now + Duration::hours(48))),
                }),
                ..Usage::default()
            },
            now,
        );
    }

    /// Drive account `idx` over the soft switch threshold on its MODEL-SCOPED
    /// weekly (`7d_oi`, the Fable bucket), through the same public path a
    /// background quota probe uses. Every shared dimension is left healthy, which
    /// is the real shape: the account is out of Fable and answers every other
    /// model class perfectly.
    fn set_fable_exhausted(&self, idx: usize, util: f64, now: OffsetDateTime) {
        self.manager.apply_usage_at(
            idx,
            &Usage {
                seven_day_oi: Some(UsageBucket {
                    utilization: Some(util),
                    reset_at_ms: Some(to_ms(now + Duration::hours(48))),
                }),
                ..Usage::default()
            },
            now,
        );
    }

    fn account_name(&self, idx: usize) -> &str {
        self.account_names.get(idx).map_or("?", String::as_str)
    }

    /// The accounts that served `key`, in order.
    fn serves_of(&self, key: u64) -> Vec<usize> {
        self.serves
            .iter()
            .filter(|s| s.key == key)
            .map(|s| s.account)
            .collect()
    }

    /// The accounts that served `key` with a request of one model class, in order.
    /// The NON-Fable subsequence is the conversation itself — the only one whose
    /// continuity costs real prompt-cache creation.
    fn serves_of_class(&self, key: u64, is_fable: bool) -> Vec<usize> {
        self.serves
            .iter()
            .filter(|s| s.key == key && s.is_fable == is_fable)
            .map(|s| s.account)
            .collect()
    }

    /// Consecutive serves of one lineage that landed on different accounts —
    /// each one is a prompt-cache prefix re-created upstream.
    fn switches(&self, key: u64) -> usize {
        self.serves_of(key)
            .windows(2)
            .filter(|w| w[0] != w[1])
            .count()
    }

    /// `1.0 - switches / pairs` over any serve path; `1.0` for fewer than 2
    /// serves. Split out so a single model class can be measured on its own.
    fn continuity_of(path: &[usize]) -> f64 {
        if path.len() < 2 {
            return 1.0;
        }
        let switches = path.windows(2).filter(|w| w[0] != w[1]).count();
        1.0 - (switches as f64 / (path.len() - 1) as f64)
    }

    /// `1.0 - switches / pairs`; `1.0` for a lineage with fewer than 2 requests.
    fn continuity(&self, key: u64) -> f64 {
        Self::continuity_of(&self.serves_of(key))
    }

    /// The account a lineage started on — the one holding its warm prefix.
    fn home(&self, key: u64) -> usize {
        *self
            .serves_of(key)
            .first()
            .unwrap_or_else(|| panic!("lineage {key} never issued a request"))
    }

    /// Fraction of a lineage's requests served by its home account. Unlike
    /// [`Self::continuity`] this does not punish a divert-and-return, so it is
    /// the right reading where the soft pacing gate is expected to fire.
    fn home_share(&self, key: u64) -> f64 {
        let serves = self.serves_of(key);
        if serves.is_empty() {
            return 1.0;
        }
        let home = serves[0];
        serves.iter().filter(|&&idx| idx == home).count() as f64 / serves.len() as f64
    }

    /// How many distinct accounts a lineage was served by — the number of
    /// upstream caches its conversation prefix was created in.
    fn distinct_accounts(&self, key: u64) -> usize {
        let mut seen: Vec<usize> = self.serves_of(key);
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    /// Combined continuity over every real (non-anonymous) lineage: total
    /// switches over total pairs.
    fn overall_continuity(&self) -> f64 {
        let (switches, pairs) = self.lineages.iter().filter(|(k, _)| *k != ANON_KEY).fold(
            (0usize, 0usize),
            |(s, p), (key, _)| {
                let n = self.serves_of(*key).len();
                (s + self.switches(*key), p + n.saturating_sub(1))
            },
        );
        if pairs == 0 {
            return 1.0;
        }
        1.0 - (switches as f64 / pairs as f64)
    }

    /// One greppable line per lineage that issued anything:
    /// `lineage=<name> requests=<n> switches=<n> continuity=<0.00-1.00> home=<account> path=<a>b(fable)>a>`
    /// — a Fable-class serve is marked in the path, so a mixed-model run shows at a
    /// glance which hops were cheap title calls and which were conversation turns.
    fn report(&self) -> String {
        let mut counts: HashMap<u64, usize> = HashMap::new();
        for serve in &self.serves {
            *counts.entry(serve.key).or_insert(0) += 1;
        }
        let mut out = String::new();
        for (key, name) in &self.lineages {
            let serves = self.serves_of(*key);
            if serves.is_empty() {
                continue;
            }
            let path: Vec<String> = self
                .serves
                .iter()
                .filter(|s| s.key == *key)
                .map(|s| {
                    let name = self.account_name(s.account);
                    if s.is_fable {
                        format!("{name}(fable)")
                    } else {
                        name.to_string()
                    }
                })
                .collect();
            out.push_str(&format!(
                "lineage={} requests={} switches={} continuity={:.2} home={} path={}\n",
                name,
                serves.len(),
                self.switches(*key),
                self.continuity(*key),
                self.account_name(serves[0]),
                path.join(">"),
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Part B — the scenarios
// ---------------------------------------------------------------------------

/// **1. One conversation, 50 turns, a healthy fleet.** The baseline every other
/// scenario degrades from: a lone session's warm cache is never moved, so it is
/// served by exactly one account for its whole life.
#[test]
fn solo_session_stays_on_one_account() {
    let mut fleet = Fleet::healthy(3);
    let session = fleet.lineage("solo");
    let mut now = t0();

    for _ in 0..50 {
        fleet.serve(session, now);
        now += Duration::seconds(1);
    }

    assert_eq!(
        fleet.distinct_accounts(session.key),
        1,
        "a lone session must never be moved\n{}",
        fleet.report()
    );
    assert_eq!(fleet.switches(session.key), 0, "\n{}", fleet.report());
    assert!(
        (fleet.continuity(session.key) - 1.0).abs() < f64::EPSILON,
        "\n{}",
        fleet.report()
    );
}

/// **2. A subagent fan-out.** Five workers run concurrently under the PARENT's
/// lineage key — real behaviour, because subagents inherit `metadata.user_id`
/// — each holding a streaming response. With the cap at 3 some workers MUST be
/// diverted; that is pacing working. What must NOT happen is the parent's pin
/// moving: once the burst drains, the next turn has to land back on the account
/// holding the conversation's warm prefix.
///
/// This is the load-bearing one. A soft gate may divert a request; it may never
/// re-key a session.
#[test]
fn subagent_fanout_does_not_rekey_the_parent() {
    let mut fleet = Fleet::healthy(3);
    let parent = fleet.lineage("parent");
    let mut now = t0();

    // The parent's first turn establishes the pin (and the warm cache).
    let home = fleet.serve(parent, now);
    now += Duration::seconds(1);

    // Five subagents fan out concurrently on the parent's identity.
    let mut streams = Vec::new();
    for _ in 0..5 {
        streams.push(fleet.stream(parent, now));
        now += Duration::milliseconds(50);
    }

    let fanout: Vec<usize> = fleet.serves_of(parent.key)[1..].to_vec();
    assert_eq!(
        fanout.iter().filter(|&&idx| idx == home).count(),
        CAP as usize,
        "exactly the cap's worth of workers should fit on the pinned account\n{}",
        fleet.report()
    );
    assert!(
        fanout.iter().any(|&idx| idx != home),
        "over-cap workers must be diverted off the pin — that is pacing working\n{}",
        fleet.report()
    );

    // The streams complete.
    drop(streams);
    now += Duration::seconds(1);

    assert_eq!(
        fleet.serve(parent, now),
        home,
        "the parent's next turn must land on its ORIGINAL account: a soft pacing \
         divert must not have re-keyed the session\n{}",
        fleet.report()
    );
}

/// **3. A workflow fan-out at 16 concurrent**, same inherited identity. Asserts
/// the pin survives well past the cap, not merely at cap+1 — past the point
/// where every account is saturated and selection falls through to the
/// least-loaded soft fallback.
#[test]
fn workflow_fanout_16_concurrent_does_not_rekey() {
    let mut fleet = Fleet::healthy(4);
    let workflow = fleet.lineage("workflow");
    let mut now = t0();

    let home = fleet.serve(workflow, now);
    now += Duration::seconds(1);

    let mut streams = Vec::new();
    for _ in 0..16 {
        streams.push(fleet.stream(workflow, now));
        now += Duration::milliseconds(25);
    }

    // 4 accounts x cap 3 = 12 paced slots; the last 4 are served by the soft
    // fallback (pacing may spread a request, never drop one).
    assert_eq!(
        fleet.serves_of(workflow.key).len(),
        17,
        "every fan-out request must be served\n{}",
        fleet.report()
    );

    drop(streams);
    now += Duration::seconds(1);

    assert_eq!(
        fleet.serve(workflow, now),
        home,
        "a 16-wide burst must not move the pin\n{}",
        fleet.report()
    );
    assert_eq!(
        fleet.serve(workflow, now + Duration::seconds(1)),
        home,
        "and it must stay put on the turn after that\n{}",
        fleet.report()
    );
}

/// **4. Two sessions stacked on one account.** Both start on the same account
/// (the second is offline when they arrive), then the fleet recovers. The
/// load-balancing migration is *allowed* to move one of them once — that is the
/// balance it exists for — but it must then converge, not ping-pong the pair
/// back and forth for the rest of their lives.
#[test]
fn two_sessions_on_one_account_do_not_pingpong() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0)], pacing(CAP));
    let first = fleet.lineage("session-a");
    let second = fleet.lineage("session-b");
    let mut now = t0();

    // Both sessions arrive while only one account is up, so both pin to it.
    fleet.manager.set_disabled(1, true);
    assert_eq!(fleet.serve(first, now), 0);
    assert_eq!(fleet.serve(second, now), 0);
    fleet.manager.set_disabled(1, false);
    now += Duration::seconds(1);

    for _ in 0..40 {
        fleet.serve(first, now);
        now += Duration::seconds(1);
        fleet.serve(second, now);
        now += Duration::seconds(1);
    }

    for session in [first, second] {
        assert!(
            fleet.switches(session.key) <= 1,
            "{} moved {} times — a rebalance is one move, not a ping-pong\n{}",
            session.name,
            fleet.switches(session.key),
            fleet.report()
        );
    }
    assert!(
        fleet.overall_continuity() >= 0.9,
        "combined continuity {:.3} fell below 0.9\n{}",
        fleet.overall_continuity(),
        fleet.report()
    );
    // Load-balancing migration ships OFF, so the pair deliberately does NOT
    // rebalance onto one account each: a session moves only at start or on a HARD
    // failure, and stacking two warm sessions on one healthy account costs nothing
    // while moving either one costs a full prompt-cache re-creation. Both keeping
    // their original pin is the CORRECT outcome, and it is strictly stronger than
    // the convergence this test originally asserted.
    for session in [first, second] {
        assert_eq!(
            fleet.switches(session.key),
            0,
            "{} left its warm pin while load-balancing migration is disabled\n{}",
            session.name,
            fleet.report()
        );
    }
    // The converge-one-per-account behaviour still exists behind
    // `loadBalanceMigration: true`; it is covered by
    // `migration_is_off_by_default_and_pin_is_honoured` in src/manager/mod.rs.
}

/// **5. An account crosses its quota threshold mid-run.** Two crossings, and only
/// one of them is a reason to move:
///
/// * **the SOFT switch threshold** is our own utilization arithmetic, computed from
///   headers that can be stale by minutes. Anthropic keeps answering 200s for
///   accounts it benches (one observed reading 100% weekly while still serving), so
///   it is a rotation hint for UNPINNED picks — the pinned sessions keep serving
///   from it and keep their prompt caches warm: `continuity == 1.0`.
/// * **a live 429 hold** is upstream's own verdict. That is HARD: the pinned
///   sessions move, exactly once, and stay moved.
///
/// The session on the healthy account must not be perturbed by either — a quota
/// event on a neighbour is never a reason to throw away a warm cache.
#[test]
fn quota_crossing_moves_only_the_affected_sessions() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0)], pacing(CAP));
    let doomed_a = fleet.lineage("on-alpha-1");
    let healthy = fleet.lineage("on-bravo");
    let doomed_b = fleet.lineage("on-alpha-2");
    let mut now = t0();

    // LRU rotation lands these as alpha / bravo / alpha.
    assert_eq!(fleet.serve(doomed_a, now), 0);
    assert_eq!(fleet.serve(healthy, now), 1);
    assert_eq!(fleet.serve(doomed_b, now), 0);
    now += Duration::seconds(1);

    // Warm turns while everything is healthy — nobody moves.
    for _ in 0..5 {
        for session in [doomed_a, healthy, doomed_b] {
            fleet.serve(session, now);
            now += Duration::seconds(1);
        }
    }
    for session in [doomed_a, healthy, doomed_b] {
        assert_eq!(
            fleet.switches(session.key),
            0,
            "{} moved while the fleet was healthy\n{}",
            session.name,
            fleet.report()
        );
    }

    // Phase A — alpha crosses the SOFT switch threshold. Nobody moves: the pinned
    // sessions are served by alpha anyway, and upstream gets to be the oracle.
    fleet.set_over_threshold(0, 0.99, now);

    for _ in 0..5 {
        for session in [doomed_a, healthy, doomed_b] {
            fleet.serve(session, now);
            now += Duration::seconds(1);
        }
    }

    for session in [doomed_a, doomed_b] {
        assert_eq!(
            fleet.serves_of(session.key).last(),
            Some(&0),
            "{} must still be served by its over-threshold pin — the SOFT \
             threshold is a rotation hint, not proof the account is gone\n{}",
            session.name,
            fleet.report()
        );
        assert!(
            (fleet.continuity(session.key) - 1.0).abs() < f64::EPSILON,
            "{} lost cache continuity to a SOFT gate\n{}",
            session.name,
            fleet.report()
        );
    }
    // A session arriving cold still avoids the over-threshold account: the
    // threshold keeps steering the picks that have no warm cache to lose.
    assert_eq!(
        fleet.serve_anon(now),
        1,
        "unpinned traffic must still route away from the over-threshold \
         account\n{}",
        fleet.report()
    );
    now += Duration::seconds(1);

    // Phase B — alpha 429s for real, which arms a live hold. THAT is hard.
    fleet.manager.mark_rate_limited_at(0, 60, now);

    for _ in 0..5 {
        for session in [doomed_a, healthy, doomed_b] {
            fleet.serve(session, now);
            now += Duration::seconds(1);
        }
    }

    for session in [doomed_a, doomed_b] {
        assert_eq!(
            fleet.switches(session.key),
            1,
            "{} must re-pin exactly once off the held account — and only on the \
             HARD gate, never on the soft crossing before it\n{}",
            session.name,
            fleet.report()
        );
        assert_eq!(
            fleet.serves_of(session.key).last(),
            Some(&1),
            "{} must end up on the healthy account\n{}",
            session.name,
            fleet.report()
        );
    }
    assert_eq!(
        fleet.switches(healthy.key),
        0,
        "the session on the healthy account must not be perturbed by a \
         neighbour's quota event\n{}",
        fleet.report()
    );
    assert!(
        (fleet.continuity(healthy.key) - 1.0).abs() < f64::EPSILON,
        "\n{}",
        fleet.report()
    );
}

/// **6. Identity-less traffic must never take a pin.** Requests with no stable
/// client identity route by rotation and leave the affinity map untouched.
///
/// The guard is behavioural, because the pin map is private. A ghost pin — the
/// `proxy.rs` bug, where a request with no identity still got a synthetic key —
/// makes identity-less traffic STICK to one account instead of rotating, and the
/// phantom pins inflate the per-account pinned-session counts that drive the
/// load-balancing migration, dragging real warm sessions around for "balance"
/// against sessions that do not exist. Rotation is the observable end of that.
///
/// Three accounts, not two: the pin fast-path re-stamps the pinned account on
/// every select (deliberately — so other sessions' LRU steers away from it),
/// which in a two-account fleet would deterministically funnel all identity-less
/// traffic to the one remaining account and make rotation unobservable.
#[test]
fn identityless_requests_never_take_a_pin() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0), ("charlie", 0)], pacing(CAP));
    let real = fleet.lineage("real-session");
    let mut now = t0();

    fleet.serve(real, now);
    now += Duration::seconds(1);

    for _ in 0..20 {
        fleet.serve_anon(now);
        now += Duration::seconds(1);
        fleet.serve(real, now);
        now += Duration::seconds(1);
    }

    assert!(
        fleet.distinct_accounts(ANON_KEY) >= 2,
        "identity-less requests must rotate across the fleet — sticking to one \
         account is the signature of a pin\n{}",
        fleet.report()
    );
    assert_eq!(
        fleet.switches(real.key),
        0,
        "identity-less traffic must not perturb a pinned session\n{}",
        fleet.report()
    );
    assert!(
        (fleet.continuity(real.key) - 1.0).abs() < f64::EPSILON,
        "\n{}",
        fleet.report()
    );
    assert_eq!(
        fleet.serves_of(real.key).last(),
        Some(&fleet.home(real.key)),
        "the pinned session must end where it started\n{}",
        fleet.report()
    );
}

/// **7. The production trigger: a busy neighbour must not re-key a quiet
/// session.** `in_flight` is an ACCOUNT-wide aggregate, so a neighbour's
/// concurrency — here identity-less traffic, which takes no pin and so cannot
/// trip the load-balancing migration — is what the quiet session's pin is
/// measured against.
///
/// Two phases:
/// * **busy, under the cap** — the pin is still usable, so the quiet session is
///   served warm every turn: `continuity == 1.0`.
/// * **saturated, at the cap** — the pin soft-fails, so that ONE request is
///   diverted (pacing working), and then the very next turn must come home.
///   Before the soft-divert fix the divert re-keyed the session durably: it
///   never came home, and every later turn paid a cold cache.
///
/// Pacing is the soft gate that still diverts. The other one — our own utilization
/// threshold — does not: scenario 5 covers a pin served straight through it.
#[test]
fn busy_neighbour_does_not_rekey_a_quiet_session() {
    // `hot` sorts first on priority, so both the quiet session and the
    // identity-less neighbour land there while it has room.
    let mut fleet = Fleet::new(&[("hot", 0), ("cold", 10)], pacing(CAP));
    let quiet = fleet.lineage("quiet");
    let mut now = t0();

    let home = fleet.serve(quiet, now);
    assert_eq!(home, 0, "the quiet session pins to the priority-0 account");
    now += Duration::seconds(1);

    // Phase 1 — the neighbour is busy but leaves a slot.
    for _ in 0..4 {
        let neighbour: Vec<InFlightGuard> = (0..CAP - 1)
            .map(|_| {
                let guard = fleet.stream_anon(now);
                now += Duration::milliseconds(10);
                guard
            })
            .collect();
        assert_eq!(
            fleet.serve(quiet, now),
            home,
            "a busy-but-not-saturated neighbour must not move a quiet session\n{}",
            fleet.report()
        );
        drop(neighbour);
        now += Duration::seconds(1);
    }
    assert!(
        (fleet.continuity(quiet.key) - 1.0).abs() < f64::EPSILON,
        "phase 1 continuity must be 1.0\n{}",
        fleet.report()
    );

    // Phase 2 — the neighbour saturates the account.
    const SATURATED_ROUNDS: usize = 4;
    for _ in 0..SATURATED_ROUNDS {
        let neighbour: Vec<InFlightGuard> = (0..CAP)
            .map(|_| {
                let guard = fleet.stream_anon(now);
                now += Duration::milliseconds(10);
                guard
            })
            .collect();
        assert_ne!(
            fleet.serve(quiet, now),
            home,
            "a saturated pin must DIVERT the request — that is the soft gate \
             doing its job\n{}",
            fleet.report()
        );
        drop(neighbour);
        now += Duration::seconds(1);
        assert_eq!(
            fleet.serve(quiet, now),
            home,
            "and the very next turn must come HOME: a soft gate may divert a \
             request, it may never re-key a session\n{}",
            fleet.report()
        );
        now += Duration::seconds(1);
    }

    assert_eq!(
        fleet.distinct_accounts(quiet.key),
        2,
        "the quiet session should only ever have touched its home and the \
         divert target\n{}",
        fleet.report()
    );
    let served = fleet.serves_of(quiet.key).len();
    let expected_home_share = (served - SATURATED_ROUNDS) as f64 / served as f64;
    assert!(
        (fleet.home_share(quiet.key) - expected_home_share).abs() < 1e-9,
        "every turn except the {SATURATED_ROUNDS} paced diverts must be served \
         by the home account (home_share {:.3}, expected {expected_home_share:.3})\n{}",
        fleet.home_share(quiet.key),
        fleet.report()
    );
}

// ---------------------------------------------------------------------------
// Part C — one invariant per cache-losing path, in the order the fixes landed.
// Each was written as an executable spec while the scenario still failed.
// ---------------------------------------------------------------------------

/// **8. A transient upstream failure must divert, never re-key.** When a 429 or
/// a transport blip fails a request on the pinned account, the proxy retries
/// with that account in `tried`. `select` then falls through to the normal pick
/// and writes the fall-through account in as the session's new pin — durably.
/// The account was never gone; one request failed on it.
///
/// This is the same invariant the soft-pacing fix established (`keep_pin` in
/// `src/manager/select.rs`), reached through a different trigger: `tried` is a
/// per-REQUEST fact, so like a soft gate it may divert a request, but it may not
/// re-key a session.
#[test]
fn transient_429_does_not_rekey_a_session() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0)], pacing(CAP));
    let session = fleet.lineage("blipped");
    let mut now = t0();

    let home = fleet.serve(session, now);
    now += Duration::seconds(1);

    // The turn that blips: upstream fails on the pin, the proxy retries with it
    // in `tried`. Serving elsewhere is correct — this ONE request must go
    // somewhere.
    let mut tried = HashSet::new();
    tried.insert(home);
    let diverted = fleet.serve_with_tried(session, &tried, now);
    assert_ne!(
        diverted, home,
        "the retry must not re-use the failed account"
    );
    now += Duration::seconds(1);

    // The blip is over — the account was never actually gone.
    for _ in 0..5 {
        assert_eq!(
            fleet.serve(session, now),
            home,
            "a transient failure must not have moved the pin\n{}",
            fleet.report()
        );
        now += Duration::seconds(1);
    }
}

/// **9. Migration must never drag a session down a priority tier.** The
/// load-balancing migration in `src/manager/select.rs` ranks candidates on
/// `cand_key = (pinned_count, in_flight, last_selected_seq)` — `priority` is
/// absent — while `pick_eligible` sorts priority FIRST. So an account that
/// normal selection would never choose while a priority-0 account is healthy can
/// still win the migration and take a warm session with it (observed 544x in one
/// day).
///
/// Balance is a tiebreak WITHIN a tier, never a reason to cross one.
#[test]
fn migration_never_moves_a_session_to_a_lower_priority_tier() {
    let mut fleet = Fleet::new(&[("primary", 0), ("overflow", 10)], pacing(CAP));
    let first = fleet.lineage("tier-a");
    let second = fleet.lineage("tier-b");
    let mut now = t0();

    // Both sessions arrive while the overflow account is offline, so both pin to
    // the primary (priority-0) account.
    fleet.manager.set_disabled(1, true);
    assert_eq!(fleet.serve(first, now), 0);
    assert_eq!(fleet.serve(second, now), 0);
    fleet.manager.set_disabled(1, false);
    now += Duration::seconds(1);

    for _ in 0..10 {
        for session in [first, second] {
            fleet.serve(session, now);
            now += Duration::seconds(1);
        }
    }

    for session in [first, second] {
        assert_eq!(
            fleet.distinct_accounts(session.key),
            1,
            "{} was migrated off the healthy priority-0 account onto a lower \
             tier purely for balance\n{}",
            session.name,
            fleet.report()
        );
    }
}

/// **10. A mixed-model session keeps ONE home.** A Claude Code conversation is not
/// one model class: the turns are Opus, and riding on the same identity are
/// one-line Fable calls for titles and summaries. A live fleet reads 95-99% on the
/// Fable weekly across nearly every account, so a session's pinned account is
/// routinely out of Fable while answering Opus perfectly.
///
/// Model-scoped exhaustion is therefore a property of the REQUEST CLASS, not of the
/// account. Counted as account death it re-keyed the whole session — one cheap
/// title request dragging a 200k-token conversation onto a cold account, which is
/// the most expensive possible trade. The Fable calls may land wherever they can be
/// served; every Opus turn must land on ONE account.
#[test]
fn mixed_model_session_keeps_one_home() {
    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0)], pacing(CAP));
    let session = fleet.lineage("mixed");
    let mut now = t0();

    // The first turn establishes the pin (and the warm prefix).
    let home = fleet.serve_model(session, OPUS, now);
    now += Duration::seconds(1);
    // That account is now out of its Fable weekly. Every shared dimension stays
    // healthy: it answers Opus exactly as before.
    fleet.set_fable_exhausted(home, 0.99, now);

    // Ten conversation turns, each followed by the title/summary call the client
    // fires on the same session.
    for _ in 0..10 {
        fleet.serve_model(session, OPUS, now);
        now += Duration::seconds(1);
        fleet.serve_model(session, FABLE, now);
        now += Duration::seconds(1);
    }

    let opus_path = fleet.serves_of_class(session.key, false);
    assert!(
        opus_path.iter().all(|&idx| idx == home),
        "every Opus turn must be served by the session's home account — a Fable \
         gate may divert a Fable request, it may never move the pin\n{}",
        fleet.report()
    );
    assert!(
        (Fleet::continuity_of(&opus_path) - 1.0).abs() < f64::EPSILON,
        "continuity over the conversation turns must be 1.0, got {:.3}\n{}",
        Fleet::continuity_of(&opus_path),
        fleet.report()
    );

    let fable_path = fleet.serves_of_class(session.key, true);
    assert_eq!(
        fable_path.len(),
        10,
        "every Fable call must be served\n{}",
        fleet.report()
    );
    assert!(
        fable_path.iter().all(|&idx| idx != home),
        "a Fable call must be routed off the Fable-exhausted account — serving it \
         there only buys a 429\n{}",
        fleet.report()
    );
}

/// **11. A short hold is a DETOUR, not a move.** A 429 park is a TIMER on an
/// account, and the timers this proxy arms are mostly short: a transient 429 with
/// no `retry-after` parks 15s + jitter, and one with a hint is clamped to 300s
/// (`src/proxy.rs`). Anthropic's default ephemeral prompt cache lives 5 minutes.
///
/// Counting any live hold as account death therefore threw away a prefix that
/// would still have been warm 15 seconds later — and permanently, because a re-key
/// is durable, so the session never went back. Nothing about the account had
/// changed by the time it un-parked; the conversation simply never returned to it.
///
/// The shape here is the live one: a conversation runs warm, its account trips a
/// burst 429 mid-run, the parked turns go elsewhere because the account genuinely
/// cannot answer them, and the moment the timer expires the conversation comes
/// home to its warm prefix. Two switches over the whole run — out and back — and
/// nothing that survives the detour.
#[test]
fn transient_hold_returns_the_session_home() {
    /// The hold the fleet arms most: `proxy::NO_GUIDANCE_HOLD_SECS`, the park a
    /// transient 429 with no `retry-after` gets. Well under the 5-minute cache TTL.
    const HOLD_SECS: i64 = 15;
    /// Turns before the 429 and after the hold clears. Sized so the two-switch
    /// detour cannot by itself drag continuity under the 0.9 floor.
    const WARM_TURNS: i64 = 12;
    /// Turns issued while the account is parked. Each is spaced 2s apart and the
    /// first is at +1s, so all of them land inside the hold.
    const HELD_TURNS: i64 = 5;

    let start = t0();

    let mut fleet = Fleet::new(&[("alpha", 0), ("bravo", 0)], pacing(CAP));
    let session = fleet.lineage("held-mid-run");

    // Phase 1 — a healthy conversation. Kept inside the first second so the hold
    // armed just below still has its full duration ahead of the simulated clock.
    let home = fleet.serve_model(session, OPUS, start);
    for turn in 1..WARM_TURNS {
        fleet.serve_model(session, OPUS, start + Duration::milliseconds(turn * 50));
    }
    assert_eq!(
        fleet.switches(session.key),
        0,
        "the warm phase must not move the session\n{}",
        fleet.report()
    );

    // Phase 2 — a burst 429 with no guidance parks the home account for 15s.
    fleet.manager.mark_rate_limited_at(home, HOLD_SECS, start);

    for turn in 0..HELD_TURNS {
        let at = start + Duration::seconds(1 + turn * 2);
        assert_ne!(
            fleet.serve_model(session, OPUS, at),
            home,
            "a parked account cannot answer this turn — serving it buys another \
             429\n{}",
            fleet.report()
        );
    }

    // Phase 3 — the timer runs out. The prefix on `home` was never 5 minutes old,
    // so the conversation must come back to it and stay.
    for turn in 0..WARM_TURNS {
        let at = start + Duration::seconds(HOLD_SECS + 5 + turn);
        assert_eq!(
            fleet.serve_model(session, OPUS, at),
            home,
            "once the hold cleared the session must come home to its warm \
             prefix — the hold was a timer, not a death\n{}",
            fleet.report()
        );
    }

    assert_eq!(
        fleet.switches(session.key),
        2,
        "a detour is exactly two switches: off the parked account and back \
         again\n{}",
        fleet.report()
    );
    let conversation = fleet.serves_of_class(session.key, false);
    let continuity = Fleet::continuity_of(&conversation);
    assert!(
        continuity >= 0.9,
        "continuity over the conversation turns must survive a short park, got \
         {continuity:.3}\n{}",
        fleet.report()
    );
}

#[test]
fn fixed_epoch_spacing() {
    let mut fleet = Fleet::new(
        &[("alpha", 0), ("bravo", 1)],
        PacingConfig {
            max_in_flight_per_account: None,
            min_spacing_ms: Some(1_000),
        },
    );
    drop(fleet.stream_anon(t0()));
    let pick = fleet.manager.select(
        &HashSet::new(),
        t0() + Duration::milliseconds(1_001),
        None,
        None,
        "/v1/messages",
        None,
    );
    assert_eq!(pick, Some(0), "spacing must expire on the replay clock");
}

#[test]
fn old_spacing_adapter_reproduces_red() {
    let fleet = Fleet::new(
        &[("alpha", 0), ("bravo", 1)],
        PacingConfig {
            max_in_flight_per_account: None,
            min_spacing_ms: Some(1_000),
        },
    );
    drop(fleet.manager.enter_in_flight(0));
    assert_eq!(
        fleet.manager.select(
            &HashSet::new(),
            t0() + Duration::milliseconds(1_001),
            None,
            None,
            "/v1/messages",
            None,
        ),
        Some(1),
        "the old wall-clock adapter must still demonstrate the missing seam"
    );
}

#[test]
fn replay_clock_controls_quota_and_holds() {
    let now = t0();
    let mut quota = tcr::quota::Quota::default();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.5".parse().unwrap(),
    );
    assert!(quota.update_from_headers_at(&headers, now));
    assert_eq!(quota.updated_at, Some(now));
    assert_eq!(
        quota.five_hour.unwrap().reset,
        Some(now + Duration::hours(5))
    );
    quota.apply_usage_at(
        &Usage {
            seven_day: Some(UsageBucket {
                utilization: Some(0.6),
                reset_at_ms: None,
            }),
            ..Usage::default()
        },
        now,
    );
    assert_eq!(
        quota.seven_day.unwrap().reset,
        Some(now + Duration::days(7))
    );
    quota.reject_model_weekly_at(&reqwest::header::HeaderMap::new(), now);
    assert_eq!(
        quota.seven_day_oi.unwrap().reset,
        Some(now + Duration::days(7))
    );

    let fleet = Fleet::new(&[("alpha", 0), ("bravo", 1)], pacing(CAP));
    assert!(fleet.manager.update_quota_at(0, &headers, 200, now));
    fleet.manager.mark_rate_limited_at(0, 5, now);
    let pick = |at| {
        fleet
            .manager
            .select(&HashSet::new(), at, None, None, "/v1/messages", None)
    };
    assert_eq!(pick(now + Duration::seconds(4)), Some(1));
    assert_eq!(pick(now + Duration::seconds(6)), Some(0));
    fleet.manager.mark_refused_at(0, now);
    assert_eq!(pick(now), Some(1));
    assert_eq!(pick(now + Duration::days(1)), Some(0));
    fleet
        .manager
        .mark_model_weekly_rejected_at(0, &reqwest::header::HeaderMap::new(), now);
    assert_eq!(
        fleet.manager.select(
            &HashSet::new(),
            now + Duration::days(1),
            Some(FABLE),
            None,
            "/v1/messages",
            None
        ),
        Some(1)
    );
}

/// Synthetic quota replay. Only the event scheduler, upstream budget ledger and
/// measurements live here: every routing decision calls the real Manager. The
/// ledger is a fake upstream, not an alternative implementation of ranking.
mod quota_replay {
    use super::*;
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeMap, VecDeque};
    use tcr::provider::Provider;
    use tcr::usage::UsageRecord;

    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const TRAINING: i64 = DAY;
    const END: i64 = TRAINING + 6 * DAY;
    const MODELS: [&str; 5] = [
        OPUS,
        "claude-haiku-4-5",
        FABLE,
        "claude-sonnet-4-6",
        "gpt-5.1-codex",
    ];
    const A_HOURS: [u64; 24] = [
        18, 26, 17, 27, 25, 21, 15, 21, 58, 63, 74, 64, 57, 84, 46, 50, 59, 66, 34, 41, 51, 33, 30,
        20,
    ];
    const C_HOURS: [u64; 24] = [
        41, 34, 36, 50, 30, 14, 22, 17, 30, 36, 50, 29, 50, 76, 50, 27, 37, 80, 46, 54, 67, 58, 42,
        23,
    ];

    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn shuffle<T>(&mut self, values: &mut [T]) {
            for i in (1..values.len()).rev() {
                values.swap(i, self.next() as usize % (i + 1));
            }
        }
    }

    /// Exact integer apportionment. Largest remainder wins; index breaks ties.
    fn apportioned(total: u64, weights: &[u64]) -> Vec<u64> {
        let sum: u128 = weights.iter().map(|&w| u128::from(w)).sum();
        assert!(sum > 0);
        let mut values: Vec<u64> = weights
            .iter()
            .map(|&w| (u128::from(total) * u128::from(w) / sum) as u64)
            .collect();
        let mut order: Vec<usize> = (0..weights.len()).collect();
        order.sort_by_key(|&i| {
            (
                std::cmp::Reverse(u128::from(total) * u128::from(weights[i]) % sum),
                i,
            )
        });
        let missing = total - values.iter().sum::<u64>();
        for &i in order.iter().take(missing as usize) {
            values[i] += 1;
        }
        values
    }

    /// Integer quantile knots. The long flat shoulders are a declared synthetic
    /// joint-shape assumption; only the supplied quantiles are observations.
    /// The unobserved top tail absorbs the exact total without changing a knot.
    fn quantiles(n: usize, total: u64, p50: u64, p90: u64, p99: u64) -> Vec<u64> {
        let knots = [
            (0, 1),
            (n / 2, p50),
            (n * 89 / 100, p50),
            (n * 90 / 100, p90),
            (n * 98 / 100, p90),
            (n * 99 / 100, p99),
            (n - 1, p99),
        ];
        let mut values = vec![0; n];
        for pair in knots.windows(2) {
            let [(lo, a), (hi, b)] = pair else {
                unreachable!()
            };
            for (i, value) in values.iter_mut().enumerate().take(hi + 1).skip(*lo) {
                *value = a + (b - a) * (i - lo) as u64 / (hi - lo).max(1) as u64;
            }
        }
        let used = values.iter().sum::<u64>();
        assert!(
            used <= total,
            "quantiles cannot represent this total: {used} > {total}"
        );
        let tail = n * 99 / 100 + 1;
        let add = apportioned(total - used, &vec![1; n - tail]);
        for (v, extra) in values[tail..].iter_mut().zip(add) {
            *v += extra;
        }
        values[tail..].sort_unstable();
        values
    }

    #[derive(Clone, Debug, Serialize)]
    struct Request {
        at: i64,
        session: u64,
        family: usize,
        input: u64,
        output: u64,
        long_cache: bool,
    }
    impl Request {
        fn provider(&self) -> usize {
            usize::from(self.family == 4)
        }
        const fn path(&self) -> &'static str {
            if self.family == 4 {
                "/backend-api/codex/responses"
            } else {
                "/v1/messages"
            }
        }
    }

    /// Counts and span ranks are correlated; token sizes are shuffled. Session
    /// starts are assigned by remaining hourly token demand, with seeded jitter.
    /// Hour shares are normalized (as are the 1.01 family shares). A session can
    /// straddle an hour: no measured within-session joint distribution is known.
    fn trace(mixed: bool) -> Vec<Request> {
        let mut rng = SplitMix64(17);
        let mut requests = Vec::new();
        let mut next_session = 1;
        for day in 0..7 {
            for provider in 0..=usize::from(mixed) {
                let (
                    count,
                    sessions,
                    input,
                    output,
                    p50,
                    p90,
                    p99,
                    count50,
                    count90,
                    span50,
                    span90,
                    hours,
                ) = if provider == 0 {
                    (
                        8_514, 142, 46_100_000, 9_860_000, 1_341, 7_784, 111_866, 3, 114, 1, 48,
                        &A_HOURS,
                    )
                } else {
                    (
                        6_566, 150, 17_600_000, 3_740_000, 1_088, 7_145, 16_738, 37, 95, 9, 39,
                        &C_HOURS,
                    )
                };
                let mut inputs = quantiles(count, input, p50, p90, p99);
                rng.shuffle(&mut inputs);
                let outputs = apportioned(output, &inputs);
                let counts = quantiles(sessions, count as u64, count50, count90, count90);
                let mut families = Vec::new();
                for (family, n) in apportioned(
                    count as u64,
                    if provider == 0 {
                        &[37, 32, 31, 1]
                    } else {
                        &[1]
                    },
                )
                .into_iter()
                .enumerate()
                {
                    families.extend(std::iter::repeat_n(
                        if provider == 0 { family } else { 4 },
                        n as usize,
                    ));
                }
                rng.shuffle(&mut families);
                let targets = apportioned(input, hours);
                let mut assigned = [0u64; 24];
                let mut offset = 0;
                for (rank, n) in counts.into_iter().enumerate() {
                    let n = n as usize;
                    let hour = (0..24)
                        .max_by_key(|&h| {
                            (
                                i128::from(targets[h]) - i128::from(assigned[h]),
                                std::cmp::Reverse(h),
                            )
                        })
                        .unwrap();
                    let start =
                        day * DAY + hour as i64 * HOUR + (rng.next() % (HOUR as u64 / 4)) as i64;
                    let span = if rank <= sessions / 2 {
                        60_000 * span50 * rank as i64 / (sessions / 2) as i64
                    } else {
                        60_000
                            * (span50
                                + (span90 - span50) * (rank - sessions / 2) as i64
                                    / (sessions * 4 / 10) as i64)
                    };
                    for turn in 0..n {
                        let i = offset + turn;
                        requests.push(Request {
                            at: (start + span * turn as i64 / n.saturating_sub(1).max(1) as i64)
                                .min((day + 1) * DAY - 1),
                            session: next_session,
                            family: families[i],
                            input: inputs[i],
                            output: outputs[i],
                            long_cache: next_session % 4 == 0,
                        });
                        assigned[hour] += inputs[i];
                    }
                    offset += n;
                    next_session += 1;
                }
                assert_eq!(offset, count);
            }
        }
        requests.sort_by_key(|r| r.at);
        requests
    }

    #[derive(Clone, Copy)]
    struct Budget {
        capacity: f64,
        used: f64,
        reset: i64,
        period: i64,
    }
    struct Slot {
        provider: usize,
        windows: [Budget; 3],
    }
    impl Slot {
        fn usage(&self) -> Usage {
            let bucket = |w: Budget| {
                Some(UsageBucket {
                    utilization: Some(w.used / w.capacity),
                    reset_at_ms: Some(to_ms(t0()) + w.reset),
                })
            };
            Usage {
                five_hour: bucket(self.windows[0]),
                seven_day: bucket(self.windows[1]),
                seven_day_oi: if self.provider == 0 {
                    bucket(self.windows[2])
                } else {
                    None
                },
                ..Usage::default()
            }
        }
    }

    impl Slot {
        /// Probe and response describe the same independent budgets. A reset
        /// cannot accidentally clear the rejection of another exhausted window.
        fn report(&self, manager: &Manager, idx: usize, now: OffsetDateTime) {
            manager.apply_usage_at(idx, &self.usage(), now);
            let exhausted = self.windows[..2].iter().any(|w| w.used >= w.capacity);
            let mut headers = reqwest::header::HeaderMap::new();
            if self.provider == 0 {
                headers.insert(
                    "anthropic-ratelimit-unified-status",
                    if exhausted { "rejected" } else { "allowed" }
                        .parse()
                        .unwrap(),
                );
            } else {
                for (prefix, w) in ["x-codex-primary", "x-codex-secondary"]
                    .into_iter()
                    .zip(self.windows)
                {
                    headers.insert(
                        format!("{prefix}-used-percent")
                            .parse::<reqwest::header::HeaderName>()
                            .unwrap(),
                        (100.0 * w.used / w.capacity).to_string().parse().unwrap(),
                    );
                    headers.insert(
                        format!("{prefix}-window-minutes")
                            .parse::<reqwest::header::HeaderName>()
                            .unwrap(),
                        (w.period / 60_000).to_string().parse().unwrap(),
                    );
                    headers.insert(
                        format!("{prefix}-reset-at")
                            .parse::<reqwest::header::HeaderName>()
                            .unwrap(),
                        ((to_ms(t0()) + w.reset) / 1_000)
                            .to_string()
                            .parse()
                            .unwrap(),
                    );
                }
                if exhausted {
                    headers.insert("x-codex-rate-limit-reached-type", "quota".parse().unwrap());
                }
            }
            manager.update_quota_at(idx, &headers, if exhausted { 429 } else { 200 }, now);
            if self.provider == 0 && self.windows[2].used >= self.windows[2].capacity {
                headers.insert(
                    "anthropic-ratelimit-unified-7d_oi-reset",
                    ((to_ms(t0()) + self.windows[2].reset) / 1_000)
                        .to_string()
                        .parse()
                        .unwrap(),
                );
                manager.mark_model_weekly_rejected_at(idx, &headers, now);
            }
        }
    }

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Score {
        lost_five_hour: f64,
        lost_weekly: f64,
        lost_scoped: f64,
        lost_distinct_budget_sum: f64,
        lockout_minutes: [f64; 2],
        cold_moves_5m: u64,
        cold_moves_1h: u64,
        arrivals: u64,
        served: u64,
        backlog: u64,
        input_arrived: u64,
        output_arrived: u64,
        input_served: u64,
        output_served: u64,
        input_backlog: u64,
        output_backlog: u64,
        reasoning_served: u64,
        revalidations: u64,
    }

    enum Event {
        Reset(usize, usize),
        Complete(usize, usize, InFlightGuard),
        Hold(usize),
        Release,
        Arrival(usize),
    }

    /// A discrete-event fake upstream. Completion charges tokens and reports
    /// live usage, then RAII releases the in-flight slot. A spent budget causes
    /// a real hold/model-scoped refusal. No unserved request is discarded.
    fn replay(requests: &[Request], mixed: bool, weight: u64, load: f64) -> Score {
        let mut accounts = Vec::new();
        let mut slots = Vec::new();
        let resets = [
            2.5, 11.5, 13.5, 18.5, 27.5, 33.5, 49.5, 52.5, 79.5, 97.75, 116.0, 134.25, 152.5,
        ];
        for provider in 0..=usize::from(mixed) {
            let n = if mixed {
                if provider == 0 { 5 } else { 2 }
            } else {
                13
            };
            let demand = if provider == 0 {
                46_100_000 + 4 * 9_860_000
            } else {
                17_600_000 + 4 * 3_740_000
            } as f64;
            for i in 0..n {
                let mut a = account(&format!("replay-{provider}-{i}"), 0);
                a.provider = if provider == 0 {
                    Provider::Anthropic
                } else {
                    Provider::Codex
                };
                accounts.push(a);
                let weekly = 7.0 * demand / (0.9 * n as f64 * load);
                let five = 5.0 * demand / (24.0 * 0.9 * n as f64 * load);
                let headroom = 0.44 + 0.48 * i as f64 / (n - 1) as f64;
                let weekly_reset = if mixed {
                    (i + 1) as i64 * WEEK / n as i64
                } else {
                    (resets[i] * HOUR as f64) as i64
                };
                slots.push(Slot {
                    provider,
                    windows: [
                        Budget {
                            capacity: five,
                            used: five * (1.0 - headroom),
                            reset: (i + 1) as i64 * 5 * HOUR / n as i64,
                            period: 5 * HOUR,
                        },
                        Budget {
                            capacity: weekly,
                            used: weekly * (1.0 - headroom),
                            reset: weekly_reset,
                            period: WEEK,
                        },
                        Budget {
                            capacity: weekly,
                            used: weekly * (1.0 - headroom),
                            reset: weekly_reset,
                            period: WEEK,
                        },
                    ],
                });
            }
        }
        let fleet = Fleet::from_config(Config {
            pacing: PacingConfig {
                max_in_flight_per_account: Some(CAP),
                min_spacing_ms: Some(1_000),
            },
            ..Config::fixture(accounts)
        });
        let mut events = BTreeMap::new();
        let mut seq = 0u64;
        let mut schedule = |events: &mut BTreeMap<(i64, u8, u64), Event>, at, kind, event| {
            seq += 1;
            events.insert((at, kind, seq), event);
        };
        for (i, slot) in slots.iter().enumerate() {
            slot.report(&fleet.manager, i, t0());
            for w in 0..if slot.provider == 0 { 3 } else { 2 } {
                schedule(&mut events, slot.windows[w].reset, 0, Event::Reset(i, w));
            }
            for day in 0..7 {
                schedule(
                    &mut events,
                    day * DAY + 9 * HOUR + i as i64 * 1_000,
                    2,
                    Event::Hold(i),
                );
            }
        }
        for (i, request) in requests.iter().enumerate() {
            schedule(&mut events, request.at, 3, Event::Arrival(i));
        }
        schedule(&mut events, TRAINING, 0, Event::Release);
        schedule(&mut events, END, 0, Event::Release);
        let mut pending: [VecDeque<usize>; 5] = std::array::from_fn(|_| VecDeque::new());
        let mut prefixes: HashMap<(u64, usize), i64> = HashMap::new();
        let mut previous: HashMap<u64, usize> = HashMap::new();
        let mut score = Score::default();
        let mut last_at = 0;
        while let Some(((at, _, _), event)) = events.pop_first() {
            if at > END {
                events.insert((at, 4, 0), event);
                break;
            }
            let elapsed = (at - last_at.max(TRAINING)).max(0);
            for provider in 0..2 {
                if pending
                    .iter()
                    .enumerate()
                    .any(|(f, q)| usize::from(f == 4) == provider && !q.is_empty())
                {
                    score.lockout_minutes[provider] += elapsed as f64 / 60_000.0;
                }
            }
            last_at = at;
            let now = t0() + Duration::milliseconds(at);
            match event {
                Event::Reset(i, w) => {
                    let budget = &mut slots[i].windows[w];
                    if (TRAINING..END).contains(&at) {
                        let loss = (budget.capacity - budget.used).max(0.0);
                        match w {
                            0 => score.lost_five_hour += loss,
                            1 => score.lost_weekly += loss,
                            _ => score.lost_scoped += loss,
                        }
                    }
                    budget.used = 0.0;
                    budget.reset += budget.period;
                    schedule(&mut events, budget.reset, 0, Event::Reset(i, w));
                    slots[i].report(&fleet.manager, i, now);
                }
                Event::Complete(i, r, guard) => {
                    let request = &requests[r];
                    let scored = request.at >= TRAINING;
                    if scored {
                        score.served += 1;
                        score.input_served += request.input;
                        score.output_served += request.output;
                        if request.provider() == 1 {
                            score.reasoning_served += request.output / 4;
                        }
                    }
                    if let Some(&old) = previous.get(&request.session) {
                        if old != i {
                            for (ttl, count) in [
                                (300_000, &mut score.cold_moves_5m),
                                (HOUR, &mut score.cold_moves_1h),
                            ] {
                                let warm = |idx| {
                                    prefixes
                                        .get(&(request.session, idx))
                                        .is_some_and(|&last| at - last < ttl)
                                };
                                if scored && warm(old) && !warm(i) {
                                    *count += 1;
                                }
                            }
                        }
                    }
                    previous.insert(request.session, i);
                    prefixes.insert((request.session, i), at);
                    let charge = (request.input + weight * request.output) as f64;
                    for w in 0..if request.family == 2 { 3 } else { 2 } {
                        slots[i].windows[w].used += charge;
                    }
                    let mut record = UsageRecord::from_quota_input(
                        to_ms(now),
                        Some(MODELS[request.family].into()),
                        Some(request.session),
                        request.input + request.input / 2,
                        if request.long_cache {
                            0
                        } else {
                            request.input / 2
                        },
                        if request.long_cache {
                            request.input / 2
                        } else {
                            0
                        },
                        request.input / 2,
                        request.output,
                    );
                    if request.provider() == 1 {
                        record.reasoning = request.output / 4;
                    }
                    fleet.manager.record_usage(i, record);
                    slots[i].report(&fleet.manager, i, now);
                    drop(guard);
                }
                Event::Hold(i) => {
                    fleet.manager.mark_rate_limited_at(i, 15, now);
                    schedule(&mut events, at + 15_000, 2, Event::Release);
                }
                Event::Release => {}
                Event::Arrival(r) => {
                    pending[requests[r].family].push_back(r);
                    if at >= TRAINING {
                        score.arrivals += 1;
                        score.input_arrived += requests[r].input;
                        score.output_arrived += requests[r].output;
                    }
                }
            }
            for queue in &mut pending {
                while let Some(&r) = queue.front() {
                    let request = &requests[r];
                    let tried = HashSet::new();
                    let picked = fleet
                        .manager
                        .select(
                            &tried,
                            now,
                            Some(MODELS[request.family]),
                            Some(request.session),
                            request.path(),
                            None,
                        )
                        .or_else(|| {
                            let pick = fleet.manager.select_revalidation(
                                &tried,
                                now,
                                Some(MODELS[request.family]),
                                Some(request.session),
                                request.path(),
                            );
                            if at >= TRAINING && pick.is_some() {
                                score.revalidations += 1;
                            }
                            pick
                        });
                    let Some(i) = picked else {
                        break;
                    };
                    assert_eq!(slots[i].provider, request.provider());
                    queue.pop_front();
                    let guard = fleet.manager.enter_in_flight_at(i, now);
                    schedule(
                        &mut events,
                        at + 1_000 + (request.input + request.output) as i64 / 100,
                        1,
                        Event::Complete(i, r, guard),
                    );
                }
            }
        }
        let in_flight = events.values().filter_map(|event| match event {
            Event::Complete(_, r, _) => Some(*r),
            _ => None,
        });
        for r in pending
            .iter()
            .flatten()
            .copied()
            .chain(in_flight)
            .filter(|&r| requests[r].at >= TRAINING)
        {
            score.backlog += 1;
            score.input_backlog += requests[r].input;
            score.output_backlog += requests[r].output;
        }
        assert_eq!(score.arrivals, score.served + score.backlog);
        assert_eq!(
            score.input_arrived,
            score.input_served + score.input_backlog
        );
        assert_eq!(
            score.output_arrived,
            score.output_served + score.output_backlog
        );
        score.lost_distinct_budget_sum =
            score.lost_five_hour + score.lost_weekly + score.lost_scoped;
        score
    }

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn baseline() -> serde_json::Value {
        let mut cases = serde_json::Map::new();
        for mixed in [false, true] {
            let requests = trace(mixed);
            let trace_digest = digest(&serde_json::to_vec(&requests).unwrap());
            for (name, weight, load) in [
                ("main", 4, 1.0),
                ("weight1", 1, 1.0),
                ("weight8", 8, 1.0),
                ("load06", 4, 0.6),
                ("load11", 4, 1.1),
            ] {
                let score = replay(&requests, mixed, weight, load);
                let score_digest = digest(&serde_json::to_vec(&score).unwrap());
                cases.insert(format!("{}-{name}", if mixed { "B" } else { "A" }), serde_json::json!({ "trace_sha256": trace_digest, "score_sha256": score_digest, "score": score }));
            }
        }
        serde_json::json!({"schema":1, "seed":17, "epoch_ms":to_ms(t0()), "training_ms":TRAINING, "scored_ms":6*DAY, "assumptions":"Synthetic joint shape; capacities use W=4 and load sensitivities scale demand/capacity. No measured output price. Prefix warmth reported independently at 5m and 1h. Weekly includes scoped only once as a distinct budget.", "cases":cases})
    }

    #[test]
    fn seeded_trace_preserves_aggregates() {
        let requests = trace(true);
        assert_eq!(
            digest(&serde_json::to_vec(&requests).unwrap()),
            digest(&serde_json::to_vec(&trace(true)).unwrap())
        );
        for day in 0..7 {
            for provider in 0..2 {
                let day_requests: Vec<_> = requests
                    .iter()
                    .filter(|r| r.at / DAY == day && r.provider() == provider)
                    .collect();
                let (n, input, output, sessions, qs) = if provider == 0 {
                    (8_514, 46_100_000, 9_860_000, 142, [1_341, 7_784, 111_866])
                } else {
                    (6_566, 17_600_000, 3_740_000, 150, [1_088, 7_145, 16_738])
                };
                assert_eq!(day_requests.len(), n);
                assert_eq!(day_requests.iter().map(|r| r.input).sum::<u64>(), input);
                assert_eq!(day_requests.iter().map(|r| r.output).sum::<u64>(), output);
                assert_eq!(
                    day_requests
                        .iter()
                        .map(|r| r.session)
                        .collect::<HashSet<_>>()
                        .len(),
                    sessions
                );
                let mut tokens: Vec<_> = day_requests.iter().map(|r| r.input).collect();
                tokens.sort_unstable();
                for (percent, expected) in [50, 90, 99].into_iter().zip(qs) {
                    assert!(tokens[n * percent / 100].abs_diff(expected) <= expected / 20);
                }
            }
        }
    }

    #[test]
    fn frozen_baseline() {
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/quota-replay-baseline.json")).unwrap();
        assert_eq!(
            baseline(),
            expected,
            "the trace and old ranking baseline are frozen"
        );
    }

    #[test]
    #[ignore = "one-time baseline capture; prints scores for the fixture"]
    fn capture_baseline() {
        let start = std::time::Instant::now();
        println!(
            "REPLAY_BASELINE={}",
            serde_json::to_string(&baseline()).unwrap()
        );
        println!(
            "REPLAY_RUNTIME_SECONDS={:.6}",
            start.elapsed().as_secs_f64()
        );
    }
}
