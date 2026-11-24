//! `Manager` account-selection and eligibility methods, split verbatim from `mod.rs`.
//!
//! This file holds the selector's shared vocabulary: the request classes, the
//! divert verdict, the threshold resolvers and the pool-pass types. Each child
//! holds one concern: the entry points ([`routing`]), the whole selection path
//! ([`with_group`]), the revalidation serve ([`revalidation`]), the per-account
//! gates ([`gates`]), the Codex slot filters ([`codex`]), the pool picks
//! ([`pool`]) and the memory-only connection and divert maps ([`ledgers`]).

use super::*;

mod codex;
mod gates;
mod ledgers;
mod pool;
mod revalidation;
mod routing;
mod with_group;

#[cfg(test)]
use with_group::{PIN_LOAD_IDLE_MS, pin_counts_as_load};

/// Bound on [`Manager::affinity`] and on its side set
/// [`Manager::affinity_extended`]. Stable pins survive reconnects, so this size
/// cap, not a disconnect hook, is what keeps a long-lived proxy from growing
/// the map without limit.
pub(super) const AFFINITY_CAP: usize = 1024;
/// Bound on [`Manager::conn_affinity`] — see its doc-comment for why the map is
/// memory-only and never persisted.
pub(super) const CONN_AFFINITY_CAP: usize = 256;

/// The three-way split of an UNPINNED request (control account, part 2 — see
/// the module doc and `docs/plans/control-routing-bridge-coder.md`). Classified
/// from `path` alone (the caller's already query-stripped request path), never
/// from a session/affinity key — see [`classify_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestClass {
    /// `/v1/messages`, `/v1/messages/count_tokens` and Codex's
    /// `/backend-api/codex/responses` — pool only. **Never** the control
    /// account, even when `controlAccount` names an ENABLED (pooled) account —
    /// see [`Manager::select`]'s pool-pick exclusion.
    ///
    /// Two opt-ins lift that, both off by default, and both leave
    /// [`Manager::pool_pick_respects_control_reserve`]'s floor in force:
    /// `groupSettings.<g>.allowControlAccount` for one explicit `--group g` ask,
    /// and [`crate::config::Config::control_pooled`] for the whole pool.
    Inference,
    /// `/api/event_logging*`, `/mcp-registry*` — high-volume noise. Follows the
    /// requesting connection ([`Manager::conn_affinity`]) rather than the
    /// control account, so it never burns the one account being kept clean.
    Noise,
    /// Everything else — the identity/control plane. Prefers the control
    /// account (bypassing its `disabled` gate — see
    /// [`Manager::control_eligible`]) when it is genuinely usable; degrades to
    /// normal rotation otherwise. The classification **fails safe**: an unknown
    /// path defaults here rather than to the pool, the opposite of
    /// `CLIENT_CREDENTIAL_PREFIXES`'s fail-unsafe growth.
    ControlPreferred,
}

// Only a request WITHOUT a client bearer reaches this classifier on a `Noise`
// or `ControlPreferred` path: with one, `proxy::relay_mode` relays every
// non-inference path under the client's own credential first, and the
// `controlIdentity` check (`crate::control_identity`) has already confirmed
// that credential IS the control account. So the control-preferred pick below
// is the bearer-less fallback, not the identity plane's main road.

/// Why the group-respecting first pass of [`Manager::select_with_group`] came up
/// empty, so the soft fallback had to drop the group and serve from the whole
/// pool.
///
/// Exists because the log line at that fallback used to hardcode "under pacing"
/// for every group miss. Pacing ships OFF ([`crate::config::default_pacing`]), so
/// on a default install that sentence named the one constraint that was NOT in
/// play and hid the one that was. Measured on the live fleet 2026-08-23: a group
/// whose only member was the control account fell back on 3 of 3 probes (a
/// two-account group landed in-group 3 of 3), and every one of the nine log lines
/// produced while diagnosing it blamed pacing, which was unconfigured.
///
/// [`Self::OnlyControl`] is the arm that matters: it is not a transient miss. No
/// retry and no quota recovering fixes it, because for [`RequestClass::Inference`]
/// the exclusion holds for as long as it is configured to. It is not permanent
/// either — `allowControlAccount` on this group, or `controlPooled` fleet-wide,
/// clears the exclusion, and both are picked up without a restart. The arm simply
/// cannot fire once one of them is on: `select_with_group` then passes
/// `control_excluded: None`, and no member matches `Some(idx)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupMiss {
    /// No configured account carries this label at all — a typo, or a label
    /// removed from every account while a client still asks for it.
    Unknown,
    /// Every member is the control account, which this inference request was
    /// not allowed to select. Serves nothing until `allowControlAccount` is set
    /// on the group or `controlPooled` is set fleet-wide.
    OnlyControl,
    /// Accounts carry the label, but none of them belongs to this request's
    /// provider: a Claude request to a group of Codex accounts, or the reverse.
    /// No retry fixes it, because Claude and Codex accounts never serve each
    /// other's requests; only an account of the request's provider joining the
    /// group does.
    OtherProviderOnly,
    /// Members exist and are selectable in principle, but every one is held out
    /// right now by a hard gate: disabled, errored, rate-limited, over its
    /// switch threshold, model-blocked, or reserved away from this caller.
    AllGated,
    /// At least one member would serve but for the SOFT pacing gate — the only
    /// arm the old message was ever right about.
    Paced,
}

impl GroupMiss {
    /// One greppable token for the `reason=` log field, so an operator can count
    /// group misses by cause without parsing prose.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "no-such-group",
            Self::OnlyControl => "control-account-only",
            Self::OtherProviderOnly => "other-provider-only",
            Self::AllGated => "all-members-unavailable",
            Self::Paced => "all-members-paced",
        }
    }

    /// The sentence a human reads in the log. Says what to DO about it in the
    /// one case that has a remedy the operator must apply by hand.
    pub(super) const fn explain(self) -> &'static str {
        match self {
            Self::Unknown => "no configured account carries the requested group",
            Self::OnlyControl => {
                "the requested group's only member is the control account, which inference never selects — no request will ever route to this group until another account joins it, the control account is cleared, or the group opts in with `tcr group allow-control <g>`"
            }
            Self::OtherProviderOnly => {
                "the requested group has no account of this request's provider: Claude and Codex accounts never serve each other's requests"
            }
            Self::AllGated => "every account in the requested group is currently unavailable",
            Self::Paced => "every account in the requested group is paced out",
        }
    }
}

/// Classify an unpinned request's path into the three-way split. Inference is
/// whatever the path's own provider calls inference
/// ([`crate::provider::Provider::is_inference_path`]): exact matches only,
/// which keeps `stable_session_key`'s `/v1/messages/count_tokens must NOT
/// prefix-match /v1/messages` guard, read from the one list in
/// `crate::provider` rather than a copy here that could drift from it. Prefix
/// match for the two noise paths, matching the bridge's own examples
/// (`/api/event_logging/v2/batch`, and any `/mcp-registry...` route).
pub(super) fn classify_request(path: &str) -> RequestClass {
    let provider = crate::provider::Provider::of_path(path);
    if provider.is_inference_path(path) {
        RequestClass::Inference
    } else if path.starts_with("/api/event_logging") || path.starts_with("/mcp-registry") {
        RequestClass::Noise
    } else {
        RequestClass::ControlPreferred
    }
}

/// Bound on [`Manager::divert_ledger`] — the same figure as [`AFFINITY_CAP`].
/// [`DivertEpisode`] carries no
/// separate last-touch stamp (§4.1's struct is quoted verbatim from the
/// design doc), so `until_ms` is the eviction sort key here — the closest
/// available recency proxy, not a perfect one. A session that never diverts
/// again after arming a short hold ages out of this bound the same way a
/// long-idle `affinity` entry does.
pub(super) const DIVERT_LEDGER_CAP: usize = 1024;

/// The three-way verdict [`divert_verdict`] hands back for one divert
/// decision. `Fresh` and `Sticky` are consulted every divert regardless of
/// budget (§4.3 — "stickiness does not consult the budget at all"); `Block`
/// only becomes reachable once a caller passes a nonzero `budget` (unit E's
/// job, §4.4) — `select()`'s own call site always passes `0` in this phase,
/// so `Block` cannot be produced from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivertVerdict {
    /// No episode for this session matches `(pin, until_ms)` — either there
    /// is none yet, or the live `(pin, until_ms)` differs from the stored
    /// one (a new hold on the same pin, or a different pin entirely). This
    /// divert is free to land on any eligible account; whichever one it
    /// picks becomes this episode's sticky destination.
    Fresh,
    /// The episode matches and already has a known-warm destination — reuse
    /// it. Offered unconditionally (budget plays no part in this arm): a
    /// repeat divert to an already-touched account costs nothing new.
    Sticky(usize),
    /// The episode matches, `budget != 0`, and its distinct-destination
    /// count is already at or over that budget. Reserved for unit E's
    /// blocking half (§4.4) — never produced while every call site passes
    /// `budget = 0`.
    Block,
}

/// The chain-head predicate (§4.4): given this session's current divert
/// episode (or `None`), the pin and hold-deadline of the divert being
/// decided right now, and a distinct-destination `budget` (`0` = unlimited),
/// decide whether to reuse a known destination, allow a fresh pick, or (once
/// a nonzero budget is wired in) block. Pure function over its four
/// arguments — no lock, no I/O — so unit F's replay harness can run this
/// EXACT code over reconstructed log sequences (§6) rather than a
/// reimplementation.
///
/// Episode identity is `(pin, until_ms)`, not the session key alone: a
/// mismatch on either field means the stored episode is stale (a new hold,
/// or the pin itself changed) and is treated as no episode at all — the
/// caller is expected to overwrite the ledger entry from scratch on the next
/// `record_divert` (§4.1, "reset is structural, not timed").
///
/// `Block` fires only when `budget != 0` (the kill-switch: `0` never
/// blocks) and the episode's `destinations.count_ones() >= budget`. This
/// phase's own `select()` call site always passes `0`, so `Block` is
/// unreachable from production code today; unit E wires a real budget in at
/// its own call site(s) and is responsible for deciding, at THAT call site,
/// whether a `Sticky` destination should still be attempted before honouring
/// `Block` — this function does not know whether the caller has already
/// tried and rejected the sticky account.
pub fn divert_verdict(
    ep: Option<DivertEpisode>,
    pin: usize,
    until_ms: i64,
    budget: u32,
) -> DivertVerdict {
    let Some(episode) = ep.filter(|e| e.pin == pin && e.until_ms == until_ms) else {
        return DivertVerdict::Fresh;
    };
    if budget != 0 && episode.destinations.count_ones() >= budget {
        DivertVerdict::Block
    } else {
        DivertVerdict::Sticky(episode.sticky)
    }
}

/// `threshold − reserve` (floored at 0.0) when `allow_reserve` is true — the
/// effective switch threshold a GENERAL (non-control-preferred) pick applies
/// to the control account specifically, so ordinary pool traffic leaves it
/// some headroom instead of racing it to the same edge as every other
/// account. A control-preferred pick, or `allow_reserve = false`, uses the
/// full `threshold` unchanged. Pure; see [`Manager::control_reserve`]'s
/// doc-comment for why this is inert in the current (control-disabled)
/// configuration.
pub(super) fn effective_threshold(threshold: f64, reserve: f64, allow_reserve: bool) -> f64 {
    if allow_reserve {
        (threshold - reserve).max(0.0)
    } else {
        threshold
    }
}

/// The ONE resolver for the effective Fable-weekly (`7d_oi`) threshold, consumed
/// by both [`Manager::model_blocked`] and [`Manager::account_gate`]'s `FableWeekly`
/// push so they can never independently drift — see
/// [`crate::config::Config::fable_weekly_threshold`]'s doc for the operator-facing
/// contract. `today` is whatever threshold the caller would otherwise apply to
/// this bucket (the account's own `switch_threshold`, or `global_threshold`) —
/// `config_override.unwrap_or(today)` byte-for-byte reproduces pre-knob
/// behaviour when the knob is absent. Pure.
pub fn effective_fable_threshold(config_override: Option<f64>, today: f64) -> f64 {
    config_override.unwrap_or(today)
}

/// One pass of the pool pick, in the order the selector tries them for unpinned work:
/// the provider's hard account lock, [`Manager::select_with_group`]'s three, then
/// [`Manager::select_revalidation`]'s fallback once the proxy finds all three empty. Each
/// pass is one candidate filter, [`Manager::pool_candidate`]; the pass's own pick ranks
/// what that filter admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolPass {
    /// `lockAccount`: that account and no other, whatever its gates, ahead of every pass.
    Lock,
    /// [`Manager::pick_eligible`]: every gate, pacing and the request's group included.
    Paced,
    /// [`Manager::pick_least_loaded`]: the soft fallback, pacing and the group dropped.
    LeastLoaded,
    /// [`Manager::pick_fable_last_resort`]: a Fable request's predicted Fable-weekly gate
    /// relaxed to upstream's own rejection.
    FableLastResort,
    /// [`Manager::select_revalidation`]'s fallback: the soft threshold and pacing relaxed,
    /// every hard gate kept.
    Revalidation,
}

/// The request facts every pool pass's filter reads, gathered once per selection.
pub(super) struct PoolAsk<'a> {
    /// Slots this request may not be served from: failed attempts, unsupported exits, and
    /// for an inference pool pick the control account.
    pub(super) tried: &'a HashSet<usize>,
    pub(super) now: OffsetDateTime,
    pub(super) now_ms: i64,
    pub(super) is_fable: bool,
    pub(super) provider: crate::provider::Provider,
    /// The request's `--group`, which only [`PoolPass::Paced`] narrows on; every widening
    /// pass drops it.
    pub(super) group: Option<&'a str>,
    /// The one version of the group settings this selection reads.
    pub(super) groups: &'a GroupPolicy,
    /// The provider's live locked slot ([`Manager::locked_account_for`]), read before any
    /// accounts lock is taken: [`PoolPass::Lock`]'s only candidate.
    pub(super) locked: Option<usize>,
    /// The session's warm window, which [`PoolPass::Revalidation`]'s hard gate reads.
    pub(super) warm: WarmWindow,
}

/// `tried` plus the control account an inference pool pick may not select
/// ([`Manager::control_excluded`]), borrowed when that adds nothing.
fn exclude_control(
    tried: &HashSet<usize>,
    control_excluded: Option<usize>,
) -> std::borrow::Cow<'_, HashSet<usize>> {
    match control_excluded {
        Some(control_idx) if !tried.contains(&control_idx) => {
            let mut t = tried.clone();
            t.insert(control_idx);
            std::borrow::Cow::Owned(t)
        }
        _ => std::borrow::Cow::Borrowed(tried),
    }
}

/// What [`Manager::routing_candidates`] found for one new, unpinned request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingCandidates {
    /// The first pass, in the selector's order, with any candidate; `None` when none has one.
    pub pass: Option<PoolPass>,
    /// Every account that pass could choose now, by slot.
    pub accounts: Vec<usize>,
    /// Whether the request is a Fable request, the classification every pass read.
    pub is_fable: bool,
    /// Earliest known instant these same passes can serve, when none can now.
    pub free_at: Option<OffsetDateTime>,
    /// Gate-owner reasons in this request's scope, without account identities.
    pub gated: BTreeMap<GateReason, usize>,
}

#[cfg(test)]
mod select_test_support {
    //! The setup this file's test modules share: one way to build a manager,
    //! a healthy account, and the session-pin map read and written directly.
    use super::*;
    use crate::config::Account;
    use crate::oauth::NoRefresh;
    use crate::probe::LiveUsageProber;
    use crate::warmer::LiveWarmer;

    /// A healthy Anthropic account, the same shape every module in this file builds.
    pub(super) fn account(name: &str) -> Account {
        Account::fixture(name)
    }

    pub(super) fn config_with(accounts: Vec<Account>) -> Config {
        Config::fixture(accounts)
    }

    /// A manager over `config` with the live prober and warmer and no refresher or
    /// config file.
    pub(super) fn live_manager(config: Config) -> Arc<Manager> {
        live_manager_with(config, None)
    }

    /// [`live_manager`], with `path` as the config file behind it.
    pub(super) fn live_manager_on_file(config: Config, path: PathBuf) -> Arc<Manager> {
        live_manager_with(config, Some(path))
    }

    fn live_manager_with(config: Config, path: Option<PathBuf>) -> Arc<Manager> {
        Manager::new(
            config,
            Arc::new(NoRefresh),
            Arc::new(LiveUsageProber::new()),
            Arc::new(LiveWarmer::new()),
            path,
        )
    }

    /// Seed a live pin directly: session `key` on account `idx`, last touched at
    /// `touched_ms`.
    pub(super) fn pin(manager: &Manager, key: u64, idx: usize, touched_ms: i64) {
        manager
            .affinity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, (idx, touched_ms));
    }

    /// The account session `key` is pinned to, if any.
    pub(super) fn pin_of(manager: &Manager, key: u64) -> Option<usize> {
        manager
            .affinity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .map(|&(idx, _)| idx)
    }
}

#[cfg(test)]
mod divert_verdict_tests {
    //! Pure-function tests for [`divert_verdict`] (the divert-budget design notes
    //! §4.4) — all three arms, driven by explicit `budget` values, exactly as
    //! the brief requires. These are a SELF-authored oracle (this module wrote
    //! both the fix and the test), so they cap at σ2: sensitivity to the
    //! change is not independence. Unit F's replay over the retained logs
    //! (§6) is the independent oracle that can promote this past σ2.
    use super::*;

    fn ep(pin: usize, until_ms: i64, destinations: u64, sticky: usize) -> DivertEpisode {
        DivertEpisode {
            pin,
            until_ms,
            destinations,
            sticky,
        }
    }

    #[test]
    fn no_episode_is_fresh() {
        assert_eq!(divert_verdict(None, 0, 1_000, 1), DivertVerdict::Fresh);
    }

    #[test]
    fn mismatched_pin_is_fresh_even_with_a_recorded_episode() {
        let episode = ep(0, 1_000, 0b10, 1);
        // Same deadline, different pin: a different account is now held, so
        // this is not a continuation of the same episode.
        assert_eq!(
            divert_verdict(Some(episode), 2, 1_000, 1),
            DivertVerdict::Fresh
        );
    }

    #[test]
    fn mismatched_deadline_is_fresh_even_with_the_same_pin() {
        let episode = ep(0, 1_000, 0b10, 1);
        // Same pin, different deadline: a NEW hold on the same account — the
        // whole point of keying the episode on `(pin, until_ms)` rather than
        // the pin alone (§4.1).
        assert_eq!(
            divert_verdict(Some(episode), 0, 2_000, 1),
            DivertVerdict::Fresh
        );
    }

    #[test]
    fn matching_episode_under_unlimited_budget_offers_sticky() {
        let episode = ep(0, 1_000, 0b10, 1);
        // budget = 0 is the kill switch (§4.6): never Block, regardless of
        // how many distinct destinations are already recorded.
        assert_eq!(
            divert_verdict(Some(episode), 0, 1_000, 0),
            DivertVerdict::Sticky(1)
        );
    }

    #[test]
    fn matching_episode_under_a_nonzero_budget_offers_sticky() {
        let episode = ep(0, 1_000, 0b10, 1); // one destination recorded
        assert_eq!(
            divert_verdict(Some(episode), 0, 1_000, 2),
            DivertVerdict::Sticky(1)
        );
    }

    #[test]
    fn matching_episode_at_the_budget_cap_blocks() {
        let episode = ep(0, 1_000, 0b10, 1); // count_ones() == 1
        assert_eq!(
            divert_verdict(Some(episode), 0, 1_000, 1),
            DivertVerdict::Block
        );
    }

    #[test]
    fn matching_episode_over_the_budget_cap_blocks() {
        // Two distinct destinations already recorded (bits 1 and 3), budget 1.
        let episode = ep(0, 1_000, 0b1010, 1);
        assert_eq!(
            divert_verdict(Some(episode), 0, 1_000, 1),
            DivertVerdict::Block
        );
    }
}

/// Unit D's gate (the divert-budget design notes §4.4, do-not #7): does
/// `select_revalidation`'s FALLBACK arm (`:934-` in this file) consult the SAME
/// episode ledger and `divert_verdict` predicate B wired into `select()`, or
/// does it still spend a fresh least-utilised account every time?
///
/// These are full `Manager` integration tests, so the helpers below are
/// deliberately minimal, standalone re-implementations of `mod.rs`'s
/// `mod tests` fixtures (`account`/`config_with`/`build_manager`) rather than
/// imports — this unit's brief is `src/manager/select.rs` ONLY, and those
/// helpers are private to `crate::manager::tests`, a sibling module this file
/// cannot reach into. `LiveUsageProber`/`LiveWarmer`/`NoRefresh` are the real,
/// non-test, already-`pub` production types: `select_revalidation` never
/// calls `probe()`/`warm()`, so wiring the live (never-invoked) impls in is
/// safe and avoids yet another local test double.
///
/// Self-authored oracle — σ2. Sensitivity ("fails without the fix, passes
/// with it") is demonstrated in this unit's FINAL-REPORT via a break/restore
/// cycle, not by this file, per `CLAUDE.md` § "Verifying a change".
#[cfg(test)]
mod revalidation_sticky_tests {
    use super::select_test_support::{account, config_with, live_manager};
    use super::*;
    use std::collections::HashSet;

    /// The headline claim: after `select()` diverts and records this
    /// episode's sticky destination in `Manager::divert_ledger`, a request
    /// that reaches `select_revalidation`'s FALLBACK arm for the SAME
    /// episode must reuse that same destination — not the fleet's
    /// least-utilised account, which (three accounts, mirroring B's own
    /// anti-churn-defeating fixture) would otherwise be the OTHER alternate.
    #[test]
    fn revalidation_fallback_reuses_the_episode_sticky_destination() {
        let manager = live_manager(config_with(vec![
            account("home"),
            account("alt1"),
            account("alt2"),
        ]));
        let now = OffsetDateTime::now_utc();
        let key = 636_363u64;

        let home = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible");
        // A long hold: outlives the soft-wait ceiling in the real ladder
        // (§1.2), which is exactly the case this design's §1.2 "revalidation
        // leak" is about — but `select_revalidation` itself takes no ceiling,
        // so any hold that keeps `home` HARD-ok-but-rate-limited exercises
        // the same PIN-HONOR-falls-through-to-FALLBACK path.
        manager.mark_rate_limited(home, 120);

        // `select()` diverts and records the episode's sticky destination —
        // this is the ledger entry `select_revalidation` must now consult.
        let sticky_dest = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an un-held alternate serves this divert");
        assert_ne!(sticky_dest, home);

        // `select_revalidation`'s FALLBACK must reuse `sticky_dest`, not the
        // OTHER un-held alternate — the assertion three accounts (not two)
        // makes real, per B's own fixture comment.
        let served = manager
            .select_revalidation(&HashSet::new(), now, None, Some(key), "/v1/messages")
            .expect("an un-held alternate serves this revalidation request");
        assert_eq!(
            served, sticky_dest,
            "select_revalidation's FALLBACK must consult the same episode \
             ledger `select()` populated, reusing its sticky destination \
             instead of spending a fresh least-utilised account"
        );

        // The episode ledger must record the FALLBACK's own serve too (§4.2:
        // "every divert records, not just sticky ones"), same destination,
        // still one distinct destination.
        let episode = manager
            .divert_ledger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .copied()
            .expect("episode recorded");
        assert_eq!(episode.pin, home);
        assert_eq!(episode.sticky, sticky_dest);
        assert_eq!(
            episode.destinations.count_ones(),
            1,
            "select() and select_revalidation reusing the SAME destination \
             is one distinct destination, not two"
        );
    }
}

/// Replay of a divert trace observed in a live deployment: four consecutive
/// diverts for one session pinned to a rate-limited account, landing on three
/// distinct destinations. Session key and account names are fabricated here —
/// the shape is what carries over, and this repository is public. Drives the
/// REAL `select()` across repeated calls rather than `divert_verdict` in
/// isolation (that predicate is already covered by `divert_tests` above).
///
/// The bounce this replay exists to catch was observable because the build
/// carrying `select()`'s sticky overlay (`:688-716`) had not yet booted when
/// those diverts fired; no `divert-sticky` line had been emitted at all at
/// the time this test was written.
///
/// Self-authored oracle — σ2, same cap as `revalidation_sticky_tests` above
/// and for the same reason (the same change added both the overlay's test
/// coverage and this replay). Neutralizing the overlay turns this test red
/// with the destination-bounce it asserts against, which shows sensitivity to
/// the change but not independence from it. A genuinely independent oracle
/// would be unit F's replay harness reading retained log lines directly
/// (§6), which this test does not have access to.
#[cfg(test)]
mod sticky_divert_replay_tests {
    use super::select_test_support::{account, config_with, live_manager};
    use super::*;
    use std::collections::HashSet;

    /// Four `select()` calls in a row for one pinned, rate-limited session —
    /// the exact shape of the retained trace (four diverts, `pin-tried`
    /// every time because the pin is already in the request's own `tried`
    /// set after the first attempt upstream, matching the real log's
    /// `reason=pin-tried` on all four lines). Every call after the first
    /// must return the SAME alternate: that reuse is the entire point of
    /// PR #110, and its absence (a fresh least-utilised pick every time) is
    /// the bounce the production trace shows.
    #[test]
    fn four_diverts_in_one_episode_reuse_the_same_alternate() {
        let manager = live_manager(config_with(vec![
            account("alice"),
            account("bob"),
            account("carol"),
        ]));
        let now = OffsetDateTime::now_utc();
        let key = 42_u64;

        // Initial pin, mirroring the trace: the session pins to `alice`,
        // which then gets rate-limited.
        let pin = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible for the initial pin");
        manager.mark_rate_limited(pin, 120);

        // Every one of the four replayed diverts hits the SAME upstream
        // gate the log's `reason=pin-tried` names: the pin already failed
        // this request (it is in `tried`), so `select()` diverts and keeps
        // the pin rather than re-keying the session.
        let mut tried = HashSet::new();
        tried.insert(pin);

        let first = manager
            .select(&tried, now, None, Some(key), "/v1/messages", None)
            .expect("an alternate is eligible for the first divert");
        assert_ne!(
            first, pin,
            "the first divert must land on an alternate, not the held pin"
        );

        for n in 2..=4 {
            let dest = manager
                .select(&tried, now, None, Some(key), "/v1/messages", None)
                .unwrap_or_else(|| panic!("an alternate is eligible for divert #{n}"));
            assert_eq!(
                dest, first,
                "divert #{n} must reuse the episode's sticky destination {first}, \
                 not bounce to a different alternate — this is the exact shape \
                 of the production trace this test replays"
            );
        }

        // The episode ledger backs the reuse: one recorded episode, keyed
        // on this session, with exactly one distinct destination touched
        // across all four diverts.
        let episode = manager
            .divert_ledger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .copied()
            .expect("episode recorded");
        assert_eq!(episode.pin, pin);
        assert_eq!(episode.sticky, first);
        assert_eq!(
            episode.destinations.count_ones(),
            1,
            "four diverts reusing the same destination is one distinct \
             destination, not four"
        );

        // Episode reset is structural, not timed (§4.1): a NEW rate-limit
        // deadline on the same pin changes the episode's identity
        // `(pin, until_ms)`, so the next divert is `Fresh` again and is free
        // to land on any eligible alternate — including a different one.
        // With `first` now the most-recently-selected account (its
        // `last_selected_seq` is ahead of the untouched third account), the
        // LRU-ordered normal pick this fixture exercises actually lands on
        // the OTHER alternate, making the "allowed to change" claim
        // concrete rather than vacuous.
        manager.mark_rate_limited(pin, 200);
        let after_reset = manager
            .select(&tried, now, None, Some(key), "/v1/messages", None)
            .expect("an alternate is eligible after the episode resets");
        assert_ne!(
            after_reset, first,
            "a new rate-limit deadline is a new episode (§4.1) — the sticky \
             destination must be free to change, and with an untouched \
             third account in the fleet the LRU pick actually does change it"
        );
    }
}

/// Two gates on `select_with_group`'s strict-group arms, both of
/// which had ZERO coverage: reverting either hunk left the whole suite green.
///
/// A lease's scope is expressed as exactly this ask
/// ([`crate::peer::serve::ScopeRestriction::Group`]), so every door out of a
/// strict group is an account outside the scope serving a borrowed request on
/// somebody else's Mac.
#[cfg(test)]
mod strict_group_door_tests {
    use super::select_test_support::{live_manager, pin, pin_of};
    use super::*;
    use crate::config::Account;
    use std::collections::HashSet;

    fn account(name: &str, groups: &[&str]) -> Account {
        Account {
            groups: Some(groups.iter().map(|g| (*g).to_string()).collect()),
            ..Account::fixture(name)
        }
    }

    fn config_with(accounts: Vec<Account>, lock_account: Option<String>) -> Config {
        Config {
            lock_account,
            ..Config::fixture(accounts)
        }
    }

    /// `config_with` plus a named control account, the door §H the
    /// control-preferred overlay below needs to reach.
    fn config_with_control(accounts: Vec<Account>, control_name: &str) -> Config {
        let mut config = config_with(accounts, None);
        config.control_account = Some(config::ControlAccountRef::Name(control_name.to_string()));
        config
    }

    /// **A HARD ACCOUNT LOCK DOES NOT BEAT A STRICT GROUP**, and this is the
    /// door that opened before the ask was even computed.
    ///
    /// `lockAccount` used to return above `strict_group`, so a lender with one
    /// set served every borrowed request on the locked account whatever the
    /// lease was scoped to. `None` and not a widening: a lock means "this
    /// account and no other", a strict ask means "inside this group and nowhere
    /// else", and when they disagree no account satisfies both.
    ///
    /// Watched red: move the `locked_idx` arm back above `strict_group` (or
    /// delete its `in_group` check) and the first assertion answers
    /// `Some(bob)`.
    #[test]
    fn a_hard_account_lock_is_refused_outside_a_strict_group() {
        let manager = live_manager(config_with(
            vec![account("alice", &["work"]), account("bob", &["home"])],
            Some("bob".to_string()),
        ));
        let now = OffsetDateTime::now_utc();
        let locked = manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
            .expect("the lock always answers when nothing asks for a group");

        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("work"),
            ),
            None,
            "the locked account is in `home`, so a STRICT ask for `work` has no \
             answer, and must not be served on the lock"
        );

        // The controls, without which the refusal above could be a lock that
        // simply stopped working.
        assert_eq!(
            manager.select_with_group(
                &HashSet::new(),
                now,
                None,
                None,
                "/v1/messages",
                None,
                Some("home"),
            ),
            Some(locked),
            "a strict ask the lock SATISFIES is still served on the lock"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(locked),
            "and a request that asks for no group is unaffected"
        );
    }

    /// **A STICKY DIVERT DESTINATION DOES NOT BEAT A STRICT GROUP**, the hunk
    /// added later and nobody covered: reverting it left the suite green.
    ///
    /// Stickiness exists so a diverted session keeps landing on ONE warm
    /// alternative, and for a spill (PREFER-only) group it still bypasses the
    /// narrowing exactly as before. A strict ask is not a preference to bypass.
    ///
    /// Watched red: pass `None` instead of `strict_group` to `Self::eligible`
    /// in `select_with_group`'s `sticky_pick` block and the strict ask below is
    /// answered `Some(sticky)`, an account outside the group, one divert
    /// later.
    #[test]
    fn a_sticky_divert_destination_is_refused_outside_a_strict_group() {
        let manager = live_manager(config_with(
            vec![account("alice", &["a-team"]), account("bob", &["b-team"])],
            None,
        ));
        let now = OffsetDateTime::now_utc();
        let key = 7_u64;

        // One pinned session, its pin then rate-limited, so the next request
        // for that session DIVERTS and keeps the pin.
        let pin = manager
            .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
            .expect("an account is eligible for the initial pin");
        manager.mark_rate_limited(pin, 120);
        let mut tried = HashSet::new();
        tried.insert(pin);

        // The first divert records the episode, and its destination becomes
        // the sticky one. This is the live sticky path, not a seeded ledger.
        let sticky = manager
            .select(&tried, now, None, Some(key), "/v1/messages", None)
            .expect("the other account serves the first divert");
        assert_ne!(sticky, pin, "a divert lands on an alternate");
        assert_eq!(
            manager
                .divert_ledger
                .lock()
                .expect("divert ledger lock poisoned")
                .get(&key)
                .copied()
                .expect("the episode is recorded")
                .sticky,
            sticky,
            "the sticky destination is the one the strict ask below must refuse"
        );

        // THE SUBJECT. The pin's OWN group is the strict ask: its only member
        // is the pin, which this request has already tried, so the honest
        // answer is `None`. The sticky destination is in the other group, and
        // serving it would be the out-of-scope serve.
        let pin_group = {
            let accounts = manager
                .accounts
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            accounts[pin].groups[0].clone()
        };
        let answer = manager.select_with_group(
            &tried,
            now,
            None,
            Some(key),
            "/v1/messages",
            None,
            Some(&pin_group),
        );
        assert_ne!(
            answer,
            Some(sticky),
            "the sticky destination is outside the strictly-asked group {pin_group} \
             and must never be the answer"
        );
        assert_eq!(
            answer, None,
            "and the honest answer is the refusal a strict ask is owed"
        );

        // The control: the SAME session, the same episode, the same sticky
        // destination, with no group asked for. Without this the assertion
        // above could be passing because stickiness stopped working.
        assert_eq!(
            manager.select(&tried, now, None, Some(key), "/v1/messages", None),
            Some(sticky),
            "with no group asked for, the sticky destination is still reused"
        );
    }

    /// **`is_group_strict` is `reserved || !spill`**, the one rule, so
    /// `src/proxy.rs`'s refusal terminals scope their answer to the same set
    /// the picker held the request inside.
    ///
    /// Watched red: make `is_group_strict` call `is_group_reserved` alone and
    /// the first assertion fails, which is exactly the state the two
    /// `proxy.rs` call sites were in.
    #[test]
    fn a_group_that_is_neither_reserved_nor_spill_is_strict() {
        let manager = live_manager(config_with(vec![account("alice", &["work"])], None));
        assert!(
            manager.is_group_strict("work"),
            "a plain group is strict: it is not a spill group, so the picker \
             holds its traffic inside it"
        );
        assert!(
            !manager.is_group_spill("work"),
            "and it is not a spill group, which is what makes it strict"
        );
        assert!(
            manager.is_group_strict("never-configured"),
            "a group nobody configured is strict too, for the same reason"
        );
    }

    /// `config_with` plus `loadBalanceMigration: true`, the only mode the
    /// migration scan below runs under (`Self::load_balance_migration_enabled`).
    fn config_with_migration(accounts: Vec<Account>) -> Config {
        let mut config = config_with(accounts, None);
        let mut extra = serde_json::Map::new();
        extra.insert(
            "loadBalanceMigration".to_string(),
            serde_json::Value::Bool(true),
        );
        config.extra = extra;
        config
    }

    // The tests below seed LIVE pins directly with `select_test_support::pin`,
    // bypassing `select`, so two sessions can be stacked on one account
    // deterministically: a fresh, unpinned `select` alternates its LRU pick across
    // every eligible account (`Self::pick_eligible`'s `last_selected_seq`
    // tie-break), so two back-to-back calls do NOT reliably land on the same index
    // the way the migration scan's precondition (`count_x >= 2`) needs.

    /// **THE FOURTH DOOR OUT OF A STRICT GROUP**, and the one that returns
    /// from inside the affinity fast path, so neither the hard-lock test above
    /// nor the sticky-destination test above ever reaches it.
    ///
    /// Watched red: the migration scan's `Self::eligible` call at this file's
    /// `:901` passes `None` for the group PREFERENCE instead of `strict_group`,
    /// so `at-outside` (no groups at all) is "eligible" for a strict `shared`
    /// ask and, being the only unpinned account, is the least-loaded migration
    /// target, `served` comes back `1`, the `assert_ne!` below fails, and the
    /// session is committed and re-pinned there (`:999-1004`). Green once that
    /// argument is `strict_group`.
    #[test]
    fn a_migration_target_is_refused_outside_a_strict_group() {
        let manager = live_manager(config_with_migration(vec![
            account("at-inside", &["shared"]),
            account("at-outside", &[]),
        ]));
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        // Two sessions already stacked on the in-group account: `count_x == 2`,
        // so the migration scan runs on the very next select for either one.
        pin(&manager, 10, 0, now_ms);
        pin(&manager, 11, 0, now_ms);

        let served = manager
            .select_with_group(
                &HashSet::new(),
                now,
                None,
                Some(10),
                "/v1/messages",
                None,
                Some("shared"),
            )
            .expect("the strict group still has a servable member");
        assert_ne!(
            served, 1,
            "`at-outside` carries no `shared` group; a migration target \
             outside the strict group is the out-of-scope serve the scope \
             exists to prevent"
        );
        assert_eq!(
            pin_of(&manager, 10),
            Some(served),
            "and whatever it chose is what the session is pinned to"
        );
    }

    /// The migration scan is the one candidate loop in this file with no
    /// `tried` guard.
    ///
    /// Watched red: `:891`'s `continue` fires only for `cand == idx`, so
    /// `at-b`, already in `tried`, either the account a borrowed request's
    /// scope benched (`src/proxy.rs:2249-2256`) or the account a local retry
    /// just failed on, is still the least-loaded candidate and the scan
    /// returns it, migrating the session onto the very account `tried`
    /// exists to rule out. Green once `:891` also `continue`s on
    /// `tried.contains(&cand)`.
    #[test]
    fn a_migration_target_is_never_an_account_the_request_already_benched() {
        let manager = live_manager(config_with_migration(vec![
            account("at-a", &[]),
            account("at-b", &[]),
        ]));
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        // Two sessions already stacked on `at-a`: `count_x == 2`, so the scan
        // runs and `at-b` (empty, and therefore the least-loaded candidate)
        // is the only place it could migrate to.
        pin(&manager, 10, 0, now_ms);
        pin(&manager, 11, 0, now_ms);

        // What a borrowed request under `x-tcr-accounts: at-a` looks like by
        // the time it reaches the picker (`src/proxy.rs:2249-2256`), and
        // equally what a local retry looks like after `at-b` failed it.
        let mut tried = HashSet::new();
        tried.insert(1);
        assert_ne!(
            manager.select(&tried, now, None, Some(10), "/v1/messages", None),
            Some(1),
            "`tried` is the set the picker may not return, and the migration \
             scan is inside the picker"
        );
    }

    /// **THE CONTROL-PREFERRED OVERLAY NEEDS NO CONFIG FLAG** to leave a
    /// strict group, only an unpinned session, a non-inference path, and a
    /// configured `controlAccount`.
    ///
    /// Watched red: `Self::control_eligible` deliberately bypasses every
    /// narrowing gate including group membership (see its own doc), and the
    /// overlay at this file's `:1080-1102` called it alone, so an unpinned
    /// `/v1/organizations`-class request asking strictly for `shared` was
    /// routed onto `at-control`, which carries no `shared` group. Green once
    /// the overlay also requires `strict_group` membership.
    #[test]
    fn control_preferred_overlay_is_refused_outside_a_strict_group() {
        let manager = live_manager(config_with_control(
            vec![
                account("at-inside", &["shared"]),
                account("at-control", &[]),
            ],
            "at-control",
        ));
        let now = OffsetDateTime::now_utc();

        let served = manager
            .select_with_group(
                &HashSet::new(),
                now,
                None,
                None, // unpinned, the overlay only ever runs on a fresh session
                "/v1/organizations",
                None,
                Some("shared"),
            )
            .expect("the strict group still has a servable member");
        assert_ne!(
            served, 1,
            "`at-control` carries no `shared` group; the control-preferred \
             overlay is a designated-identity route, not a licence to leave \
             a strict group with no config flag at all"
        );

        // The control: with no group asked for, the same request still
        // prefers the control account, the overlay itself is unaffected.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/organizations", None),
            Some(1),
            "with no group asked for, control-preferred routing is unchanged"
        );
    }

    /// **THE CONNECTION-AFFINITY (NOISE) OVERLAY NEEDS NO CONFIG FLAG
    /// EITHER**, the reachability turns only on the lender's HTTP client
    /// reusing one loopback connection across leases (unmeasured by the
    /// review), never on `loadBalanceMigration`.
    ///
    /// Watched red: the overlay at this file's `:1034-1075` called
    /// `Self::eligible` with `group: None`, so a `/mcp-registry`-class
    /// request whose connection was already following `at-outside` (no
    /// `shared` group) kept following it even under a strict `shared` ask.
    /// Green once that argument is `strict_group`.
    #[test]
    fn noise_connection_affinity_overlay_is_refused_outside_a_strict_group() {
        let manager = live_manager(config_with(
            vec![
                account("at-inside", &["shared"]),
                account("at-outside", &[]),
            ],
            None,
        ));
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);

        // The control FIRST, on its own connection key: with no group asked
        // for, a request follows its connection's already-affined account.
        // Checked before the strict ask below on a SEPARATE key, because a
        // strict ask that falls through to the normal pick re-records
        // `conn_affinity` for whichever key it used, sharing one key would
        // make the control assertion read the strict call's own side effect
        // instead of the overlay's un-narrowed behaviour.
        let control_conn_key = 100_u64;
        manager.conn_affinity_record(control_conn_key, 1, now_ms);
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                None,
                "/mcp-registry/v1/list",
                Some(control_conn_key),
            ),
            Some(1),
            "with no group asked for, connection affinity follows the account \
             an earlier request on this connection left it on"
        );

        // THE SUBJECT: the same connection-affinity fact, but the request now
        // asks strictly for `shared`, which `at-outside` does not carry.
        let strict_conn_key = 99_u64;
        manager.conn_affinity_record(strict_conn_key, 1, now_ms);
        let served = manager
            .select_with_group(
                &HashSet::new(),
                now,
                None,
                None, // unpinned, this overlay is also gated on `keep_pin.is_none()`
                "/mcp-registry/v1/list",
                Some(strict_conn_key),
                Some("shared"),
            )
            .expect("the strict group still has a servable member");
        assert_ne!(
            served, 1,
            "`at-outside` carries no `shared` group; a strict ask must not \
             keep following a connection's account out of the group"
        );
    }
}

/// Reserved-group semantics test #5:
/// `eligible` and `account_gate` must AGREE on the RESERVED gate over many
/// account/reserved-set/ask combinations — a property test over the two
/// predicates, not two example tests that can drift apart the way
/// `gate_and_hard_ok_agree_on_every_variant` (`manager/mod.rs`) already
/// guards the OTHER eight `GateReason` variants. Pure-predicate, so it needs
/// no `Manager`/config plumbing — just [`AccountRuntime`] and the same
/// `reserved_blocks` these two predicates both call.
#[cfg(test)]
mod reserved_gate_agreement_tests {
    use super::*;
    use crate::config::{Account, PacingConfig};

    fn account_runtime(groups: &[&str]) -> AccountRuntime {
        AccountRuntime::from_config(
            &Account {
                refresh_token: None,
                groups: Some(groups.iter().map(ToString::to_string).collect()),
                ..Account::fixture("probe")
            },
            false,
        )
    }

    /// Unrequested traffic (`group: None`, both `eligible`'s narrowing AND
    /// reserved-check args): `eligible` and `account_gate` must agree with
    /// `reserved_blocks` for every combination of an account's groups against
    /// every reserved set — the account is otherwise perfectly healthy, so
    /// RESERVED is the only thing that can hold it out.
    #[test]
    fn agree_on_unrequested_traffic_across_group_and_reserved_combinations() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let pacing = PacingConfig::default();

        let group_combos: [&[&str]; 4] = [&[], &["codereview"], &["dev"], &["codereview", "dev"]];
        let reserved_combos: [&[&str]; 4] = [
            &[],
            &["codereview"],
            &["dev"],
            &["codereview", "dev", "burst"],
        ];

        for groups in group_combos {
            let account = account_runtime(groups);
            for reserved_list in reserved_combos {
                let reserved: HashSet<String> =
                    reserved_list.iter().map(ToString::to_string).collect();
                let blocked = Manager::reserved_blocks(&account, None, &reserved);

                let elig = Manager::eligible(
                    &account,
                    0.90,
                    None,
                    &pacing,
                    false,
                    now,
                    now_ms,
                    false,
                    crate::provider::Provider::Anthropic,
                    None,
                    None,
                    &reserved,
                    &HashSet::new(),
                );
                assert_eq!(
                    elig, !blocked,
                    "groups={groups:?} reserved={reserved_list:?}: eligible \
                     disagrees with reserved_blocks on an otherwise-healthy account"
                );

                let (gate, _) = Manager::account_gate(
                    &account,
                    0.90,
                    None,
                    now,
                    now_ms,
                    false,
                    None,
                    &reserved,
                    &HashSet::new(),
                );
                assert_eq!(
                    gate == GateReason::Reserved,
                    blocked,
                    "groups={groups:?} reserved={reserved_list:?}: account_gate \
                     disagrees with reserved_blocks"
                );

                let hard_ok = Manager::account_hard_ok(
                    &account,
                    now_ms,
                    None,
                    &reserved,
                    &HashSet::new(),
                    WarmWindow::DEFAULT,
                );
                assert_eq!(
                    hard_ok, !blocked,
                    "groups={groups:?} reserved={reserved_list:?}: account_hard_ok \
                     disagrees with reserved_blocks — a reserved pin would survive \
                     the hard-gate re-test"
                );
            }
        }
    }

    /// An explicit ask for one of the account's own groups always clears the
    /// reserved gate in `eligible`, regardless of what else is reserved — the
    /// spec's "session asked for group g -> eligible iff g in groups(A))"
    /// branch, which never adds a second requirement on top of the match.
    #[test]
    fn an_explicit_matching_ask_is_never_reserved_blocked() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let pacing = PacingConfig::default();
        let account = account_runtime(&["codereview", "dev"]);
        let reserved: HashSet<String> = ["codereview".to_string(), "dev".to_string()].into();

        for ask in ["codereview", "dev"] {
            assert!(
                Manager::eligible(
                    &account,
                    0.90,
                    None,
                    &pacing,
                    false,
                    now,
                    now_ms,
                    false,
                    crate::provider::Provider::Anthropic,
                    Some(ask),
                    Some(ask),
                    &reserved,
                    &HashSet::new(),
                ),
                "ask={ask}: an explicit ask for the account's own (reserved) group \
                 must clear the reserved gate"
            );
            let (gate, _) = Manager::account_gate(
                &account,
                0.90,
                None,
                now,
                now_ms,
                false,
                Some(ask),
                &reserved,
                &HashSet::new(),
            );
            assert_ne!(gate, GateReason::Reserved, "ask={ask}: account_gate agrees");
        }
    }
}

/// Group-miss classification (`Manager::classify_group_miss`): the log line at
/// the soft fallback must name the constraint that was actually in play.
///
/// The arm that earns the module is [`GroupMiss::OnlyControl`]. Before this
/// existed, a group whose only member was the control account fell back on every
/// single request and the log said "under pacing" — on a fleet where pacing was
/// unconfigured. These tests pin each cause to its own answer so the message can
/// never again describe a constraint that was switched off.
#[cfg(test)]
mod group_miss_tests {
    use super::*;
    use crate::config::{Account, PacingConfig};

    fn account(name: &str, groups: &[&str], disabled: bool) -> AccountRuntime {
        AccountRuntime::from_config(
            &Account {
                refresh_token: None,
                disabled: disabled.then_some(true),
                groups: Some(groups.iter().map(ToString::to_string).collect()),
                ..Account::fixture(name)
            },
            false,
        )
    }

    fn classify(
        accounts: &[AccountRuntime],
        control_excluded: Option<usize>,
        pacing: &PacingConfig,
        group: &str,
    ) -> GroupMiss {
        let now = OffsetDateTime::now_utc();
        Manager::classify_group_miss(
            accounts,
            &HashSet::new(),
            control_excluded,
            0.95,
            None,
            pacing,
            now,
            odt_to_ms(now),
            false,
            crate::provider::Provider::Anthropic,
            group,
            &HashSet::new(),
            &HashSet::new(),
        )
    }

    /// The live defect, in one assertion: `research` holds exactly one account
    /// and that account is the control one, which inference never picks. The
    /// group is not busy and not paced — it is permanently unroutable, and the
    /// reason must say so.
    #[test]
    fn a_group_holding_only_the_control_account_is_named_control_account_only() {
        let accounts = [
            account("gil@example.com", &["research"], false),
            account("worker@example.com", &[], false),
        ];
        let miss = classify(&accounts, Some(0), &PacingConfig::default(), "research");
        assert_eq!(miss, GroupMiss::OnlyControl, "{miss:?}");
        assert_eq!(miss.as_str(), "control-account-only");
        assert!(
            miss.explain().contains("control account"),
            "the sentence an operator reads must name the control account: {}",
            miss.explain()
        );
    }

    /// Same fleet, same group — but the request is NOT inference, so nothing
    /// excluded the control account and the group is perfectly routable. Guards
    /// against reporting `OnlyControl` from group shape alone.
    #[test]
    fn the_same_group_is_not_control_blocked_when_nothing_was_excluded() {
        let accounts = [
            account("gil@example.com", &["research"], false),
            account("worker@example.com", &[], false),
        ];
        assert_ne!(
            classify(&accounts, None, &PacingConfig::default(), "research"),
            GroupMiss::OnlyControl
        );
    }

    /// A label no account carries is its own cause — a typo or a label removed
    /// from every account, not a busy group.
    #[test]
    fn a_label_no_account_carries_is_unknown() {
        let accounts = [account("worker@example.com", &["dev"], false)];
        assert_eq!(
            classify(&accounts, None, &PacingConfig::default(), "resarch"),
            GroupMiss::Unknown
        );
    }

    /// A second, non-control member exists, so the group is not control-blocked
    /// — but it is disabled, which is a hard gate and not pacing.
    #[test]
    fn a_hard_gated_member_is_unavailable_not_paced() {
        let accounts = [
            account("gil@example.com", &["research"], false),
            account("worker@example.com", &["research"], true),
        ];
        assert_eq!(
            classify(&accounts, Some(0), &PacingConfig::default(), "research"),
            GroupMiss::AllGated
        );
    }

    /// The one case the old hardcoded message was right about: pacing is
    /// configured, the member is healthy, and it is inside its min-spacing
    /// window. Pacing keeps its name — the fix is not to stop saying "paced",
    /// it is to stop saying it when pacing is off.
    #[test]
    fn a_healthy_member_inside_the_spacing_window_is_paced() {
        let pacing = PacingConfig {
            max_in_flight_per_account: None,
            min_spacing_ms: Some(60_000),
        };
        let mut accounts = [account("worker@example.com", &["research"], false)];
        accounts[0].last_served_ms = crate::now_ms();
        assert_eq!(
            classify(&accounts, None, &pacing, "research"),
            GroupMiss::Paced
        );
    }

    /// `account`, as a Codex account.
    fn codex_account(name: &str, groups: &[&str]) -> AccountRuntime {
        let mut runtime = account(name, groups, false);
        runtime.provider = crate::provider::Provider::Codex;
        runtime
    }

    /// A Claude request to a group whose only member is a Codex account is not
    /// a busy group: no Claude request will ever be served there, because Claude
    /// and Codex accounts never serve each other's requests. The reason says
    /// that, rather than "every account in the requested group is currently
    /// unavailable", which reads as something time will fix.
    #[test]
    fn a_claude_request_to_a_codex_only_group_is_other_provider_only() {
        let accounts = [
            account("worker@example.com", &["dev"], false),
            codex_account("bob@example.com/codex", &["lab"]),
        ];
        let miss = classify(&accounts, None, &PacingConfig::default(), "lab");
        assert_eq!(miss, GroupMiss::OtherProviderOnly, "{miss:?}");
        assert_eq!(miss.as_str(), "other-provider-only");
        assert_eq!(
            miss.explain(),
            "the requested group has no account of this request's provider: Claude and Codex accounts never serve each other's requests"
        );
    }

    /// A group holding the excluded control account and a Codex account is,
    /// to a Claude request, a group holding only the control account: the Codex
    /// member is not a candidate, so the reason is the control exclusion.
    #[test]
    fn a_control_plus_codex_group_is_only_control() {
        let accounts = [
            account("gil@example.com", &["research"], false),
            codex_account("bob@example.com/codex", &["research"]),
            account("worker@example.com", &[], false),
        ];
        assert_eq!(
            classify(&accounts, Some(0), &PacingConfig::default(), "research"),
            GroupMiss::OnlyControl
        );
    }

    /// A row deleted from the file keeps its rotation slot and its old groups
    /// until a restart. The narrowing is by provider only, so such a slot still
    /// counts as a member of its own provider: a fleet with no Codex account,
    /// whose group's only member was just deleted, keeps answering "unavailable"
    /// and never names a provider it does not have.
    #[test]
    fn a_group_holding_only_a_removed_claude_slot_is_all_gated() {
        let mut accounts = [
            account("gone@example.com", &["lab"], true),
            account("worker@example.com", &[], false),
        ];
        accounts[0].removed = true;
        assert_eq!(
            classify(&accounts, None, &PacingConfig::default(), "lab"),
            GroupMiss::AllGated
        );
    }
}

/// Tests for the reset-urgency bucket ([`Manager::reset_urgency_tier`]) — the
/// second ranking key inside a priority tier.
///
/// The headline claim every test here circles: **an account whose weekly
/// headroom expires sooner is spent ahead of one that was selected less
/// recently**, because unused weekly quota is worth nothing after its reset.
/// That is a deliberate inversion of the pre-tier LRU order, so the discriminating
/// test below is built to FAIL on the pre-tier code — it stamps the urgent
/// account as the most-recently-selected one, which is exactly the account plain
/// LRU sorts last.
#[cfg(test)]
mod reset_urgency_tests {
    use super::select_test_support::{account, live_manager, pin};
    use super::*;
    use crate::config::Account;
    use crate::quota::QuotaWindow;
    use std::collections::HashSet;

    fn config_with(accounts: Vec<Account>, tier_hours: u32) -> Config {
        Config {
            reset_urgency_tier_hours: tier_hours,
            ..Config::fixture(accounts)
        }
    }

    /// Give account `idx` a weekly window resetting `hours` from `now` at a
    /// utilization well under the fixture's 0.90 threshold, so the account is
    /// ranked but never gated — this suite is about ORDER, and an account held
    /// out by quota would prove nothing about it.
    fn set_weekly(manager: &Manager, idx: usize, hours: i64, now: OffsetDateTime) {
        let mut accounts = manager
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        accounts[idx].quota.seven_day = Some(QuotaWindow {
            utilization: 0.10,
            reset: Some(now + time::Duration::hours(hours)),
        });
    }

    fn set_seq(manager: &Manager, idx: usize, seq: u64) {
        let mut accounts = manager
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        accounts[idx].last_selected_seq = seq;
    }

    fn pick(manager: &Manager, now: OffsetDateTime) -> usize {
        manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
            .expect("an account is eligible")
    }

    /// **The discriminating test.** Two accounts in one priority tier: `urgent`
    /// resets in 2h (bucket 0 at the default 24h width), `roomy` in 100h
    /// (bucket 4). `urgent` is stamped as the MOST recently selected account and
    /// `roomy` as never-selected, so pre-tier LRU would pick `roomy` — this test
    /// fails on the old ordering, which is the point of it.
    ///
    /// Mirrors the live fleet that motivated the change (measured 2026-08-26):
    /// 13 accounts all at priority 0, resets spread 2.5h–152.5h, and the
    /// soonest-resetting account sitting on 44% of a weekly bucket that was
    /// worth nothing a few hours later.
    #[test]
    fn a_sooner_resetting_account_outranks_a_less_recently_selected_one() {
        let manager = live_manager(config_with(vec![account("urgent"), account("roomy")], 24));
        let now = OffsetDateTime::now_utc();

        set_weekly(&manager, 0, 2, now);
        set_weekly(&manager, 1, 100, now);
        // Bias plain LRU as hard as possible TOWARD `roomy`: `urgent` looks
        // most-recently-selected, `roomy` never-selected.
        set_seq(&manager, 0, 999);
        set_seq(&manager, 1, 0);

        assert_eq!(
            pick(&manager, now),
            0,
            "the account whose weekly window resets in 2h must be spent before \
             the one with 100h left, even though it is the most recently \
             selected of the two — its unused headroom is the only headroom \
             about to expire"
        );
    }

    /// The shape reported 2026-11-07: a session pinned to `roomy` (reset in 86h)
    /// comes back after its cache went cold, and `urgent` (reset in 11h) sits
    /// unused. Both are under the threshold, so before this change the pin was
    /// honoured and `urgent`'s week expired unspent. A cold pin protects nothing,
    /// so the request takes the reset order like any new one, and the session is
    /// re-pinned where it lands.
    #[test]
    fn a_cold_pin_takes_the_reset_order_again() {
        let manager = live_manager(config_with(vec![account("roomy"), account("urgent")], 24));
        let now = OffsetDateTime::now_utc();
        set_weekly(&manager, 0, 86, now);
        set_weekly(&manager, 1, 11, now);
        let key = 4242;
        let cold_ms = odt_to_ms(now) - WarmWindow::DEFAULT.ms() - 1_000;
        pin(&manager, key, 0, cold_ms);

        let served = manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
        assert_eq!(
            served,
            Some(1),
            "a session idle past its warm window has no cache left on `roomy`, so it \
             must take the reset order and land on the account resetting in 11h"
        );
        assert_eq!(
            manager.affinity_pin(key),
            Some(1),
            "the session is re-pinned where the pick sent it"
        );
    }

    /// The complement, so the rule above cannot widen: inside the warm window the
    /// cache on `roomy` is real, and the pin still outranks the reset order.
    #[test]
    fn a_warm_pin_keeps_its_account_over_the_reset_order() {
        let manager = live_manager(config_with(vec![account("roomy"), account("urgent")], 24));
        let now = OffsetDateTime::now_utc();
        set_weekly(&manager, 0, 86, now);
        set_weekly(&manager, 1, 11, now);
        let key = 4243;
        pin(&manager, key, 0, odt_to_ms(now) - 10_000);

        let served = manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
        assert_eq!(
            served,
            Some(0),
            "a warm pin keeps its cache: it is honoured over the reset order"
        );
        assert_eq!(
            manager.affinity_pin(key),
            Some(0),
            "the warm pin does not move"
        );
    }

    /// The other half of the design: bucketing must NOT collapse into a
    /// deterministic pin. Two accounts resetting 2h apart share bucket 0 at the
    /// default 24h width, so LRU still decides between them and consecutive
    /// picks alternate — the fan-out that ranking on the raw reset instant would
    /// have destroyed.
    #[test]
    fn accounts_in_one_bucket_still_fan_out_by_lru() {
        let manager = live_manager(config_with(vec![account("first"), account("second")], 24));
        let now = OffsetDateTime::now_utc();

        set_weekly(&manager, 0, 2, now);
        set_weekly(&manager, 1, 4, now);

        let picks: Vec<usize> = (0..4).map(|_| pick(&manager, now)).collect();
        assert!(
            picks.contains(&0) && picks.contains(&1),
            "two accounts sharing one urgency bucket must both be served — \
             got {picks:?}, which is the deterministic pin the bucket exists \
             to prevent"
        );
    }

    /// `resetUrgencyTierHours: 0` is the config-only rollback: the urgency term
    /// ties for every account and the pre-tier LRU order returns. Same fixture
    /// as the discriminating test, opposite expected winner.
    #[test]
    fn tier_hours_zero_restores_the_pre_tier_lru_order() {
        let manager = live_manager(config_with(vec![account("urgent"), account("roomy")], 0));
        let now = OffsetDateTime::now_utc();

        set_weekly(&manager, 0, 2, now);
        set_weekly(&manager, 1, 100, now);
        set_seq(&manager, 0, 999);
        set_seq(&manager, 1, 0);

        assert_eq!(
            pick(&manager, now),
            1,
            "with the tier term disabled the never-selected account wins on \
             plain LRU, exactly as it did before this feature existed"
        );
    }

    /// An account with no known live weekly reset keeps sorting FIRST — the
    /// pre-tier "unknown quota → probe it" contract, which the bucket must
    /// carry through rather than quietly re-rank. `roomy` here is both
    /// never-selected AND resetting soon; the unmeasured account still beats it.
    #[test]
    fn an_unknown_weekly_reset_still_sorts_ahead_of_every_bucket() {
        let manager = live_manager(config_with(
            vec![account("unmeasured"), account("soon")],
            24,
        ));
        let now = OffsetDateTime::now_utc();

        // `unmeasured` deliberately gets NO weekly window at all.
        set_weekly(&manager, 1, 1, now);
        set_seq(&manager, 0, 999);
        set_seq(&manager, 1, 0);

        assert_eq!(
            pick(&manager, now),
            0,
            "an account whose weekly window was never measured must be picked \
             (and so probed) ahead of every bucketed account"
        );
    }

    /// Unit-level check of the bucket arithmetic itself, independent of
    /// selection: the floor division, the disabled arm, and the unknown-reset
    /// sentinel. Widths are checked at the boundary (24h lands in bucket 1, not
    /// 0) because an off-by-one here silently re-ranks the whole fleet.
    #[test]
    fn bucket_arithmetic_floors_and_handles_both_sentinels() {
        let manager = live_manager(config_with(vec![account("a")], 24));
        let now = OffsetDateTime::now_utc();

        let tier = |hours: i64| {
            set_weekly(&manager, 0, hours, now);
            let accounts = manager
                .accounts
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            manager.reset_urgency_tier(&accounts[0], now)
        };

        assert_eq!(tier(2), 0, "2h into a 24h width is bucket 0");
        assert_eq!(tier(23), 0, "23h is still bucket 0");
        assert_eq!(tier(25), 1, "25h crosses into bucket 1");
        assert_eq!(tier(100), 4, "100h floors to bucket 4");
        assert_eq!(tier(152), 6, "the live fleet's furthest account, bucket 6");

        // Unknown reset → the minimum, ahead of every bucket.
        {
            let mut accounts = manager
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            accounts[0].quota.seven_day = None;
        }
        {
            let accounts = manager
                .accounts
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            assert_eq!(
                manager.reset_urgency_tier(&accounts[0], now),
                i64::MIN,
                "an unmeasured weekly window sorts ahead of every bucket"
            );
        }

        // Disabled → a constant tie for everyone, including the unknown arm.
        let off = live_manager(config_with(vec![account("a")], 0));
        {
            let accounts = off.accounts.read().unwrap_or_else(PoisonError::into_inner);
            assert_eq!(
                off.reset_urgency_tier(&accounts[0], now),
                0,
                "a disabled tier term must tie, not rank"
            );
        }
    }
}

/// The Fable weekly last-resort gate (`docs/plans/fable-gate-bridge-coder.md`):
/// on a real fleet the model-scoped weekly bucket (`7d_oi`) rests at 0.95-0.98
/// against a `switchThreshold` of 0.95, so every pooled account hard-failed
/// `model_blocked` and a Fable request 429'd even though upstream had never
/// once rejected on that bucket. These drive the REAL `select()` (not the
/// pure predicates) across the three tiers the fix adds: predicted-near is
/// still eligible normally, predicted-over-but-not-rejected is skipped on the
/// first two passes and usable as a last resort, and genuinely-rejected
/// (`>= 1.0`, the only value [`crate::quota::Quota::reject_model_weekly`]
/// writes) is never picked.
#[cfg(test)]
mod fable_last_resort_tests {
    use super::select_test_support::{account, live_manager};
    use super::*;
    use crate::config::Account;
    use crate::quota::QuotaWindow;
    use std::collections::HashSet;
    use time::Duration;

    const FABLE_MODEL: &str = "claude-fable-5";

    fn config_with(accounts: Vec<Account>, switch_threshold: f64) -> Config {
        Config {
            switch_threshold,
            ..Config::fixture(accounts)
        }
    }

    /// Set account `idx`'s `7d_oi` utilization, resetting two hours out (safely
    /// in the future for the lifetime of a unit test).
    fn set_seven_day_oi(manager: &Manager, idx: usize, utilization: f64) {
        let now = OffsetDateTime::now_utc();
        let mut accounts = manager
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        accounts[idx].quota.seven_day_oi = Some(QuotaWindow {
            utilization,
            reset: Some(now + Duration::hours(2)),
        });
    }

    /// Absent `fableWeeklyThreshold` must byte-for-byte match today's
    /// behaviour: `switchThreshold` alone decides the Fable-weekly gate. Guards
    /// the default so shipping the knob does not silently change an existing
    /// fleet's routing — see the bridge's evidence (ten of thirteen accounts
    /// sitting at 0.95-0.98 on `7d_oi`, none of them upstream-rejected).
    #[test]
    fn absent_fable_weekly_threshold_matches_todays_switch_threshold_gate() {
        let manager = live_manager(config_with(vec![account("acct-a")], 0.95));
        set_seven_day_oi(&manager, 0, 0.96);
        let now = OffsetDateTime::now_utc();
        let accounts = manager
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            Manager::model_blocked(
                &accounts[0],
                manager.global_threshold,
                manager.fable_weekly_threshold,
                now,
                true,
            ),
            "absent fableWeeklyThreshold must gate a Fable request exactly as \
             switchThreshold did before this knob existed"
        );
    }

    /// `fableWeeklyThreshold: 1.0` frees an account tcr merely PREDICTS is over
    /// (0.96 against a 0.95 `switchThreshold`) but still gates one upstream has
    /// GENUINELY rejected (utilization `1.0`, the only value
    /// `reject_model_weekly` writes) — the whole point of the bridge: let the
    /// Fable bucket run to a real rejection instead of tcr's own prediction.
    #[test]
    fn fable_weekly_threshold_at_one_frees_prediction_but_not_rejection() {
        let mut config = config_with(vec![account("acct-a"), account("acct-b")], 0.95);
        config.fable_weekly_threshold = Some(1.0);
        let manager = live_manager(config);
        set_seven_day_oi(&manager, 0, 0.96); // predicted-over, never rejected
        set_seven_day_oi(&manager, 1, 1.0); // genuinely rejected upstream
        let now = OffsetDateTime::now_utc();
        let accounts = manager
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            !Manager::model_blocked(
                &accounts[0],
                manager.global_threshold,
                manager.fable_weekly_threshold,
                now,
                true,
            ),
            "fableWeeklyThreshold=1.0 must not gate an account tcr merely \
             predicts is over — only upstream's own rejection may"
        );
        assert!(
            Manager::model_blocked(
                &accounts[1],
                manager.global_threshold,
                manager.fable_weekly_threshold,
                now,
                true,
            ),
            "fableWeeklyThreshold=1.0 must still gate a genuine upstream rejection"
        );
    }

    /// `account_gate`'s `FableWeekly` push must agree with `model_blocked` on the
    /// same account under both an absent and a set `fableWeeklyThreshold` — two
    /// independently-resolved copies is exactly how the panel and routing can
    /// drift; see [`effective_fable_threshold`], the one resolver both consume.
    #[test]
    fn account_gate_and_model_blocked_agree_on_fable_weekly_under_both_settings() {
        for fable_weekly_threshold in [None, Some(1.0)] {
            let mut config = config_with(vec![account("acct-a")], 0.95);
            config.fable_weekly_threshold = fable_weekly_threshold;
            let manager = live_manager(config);
            set_seven_day_oi(&manager, 0, 0.96);
            let now = OffsetDateTime::now_utc();
            let now_ms = odt_to_ms(now);
            let accounts = manager
                .accounts
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            let blocked = Manager::model_blocked(
                &accounts[0],
                manager.global_threshold,
                manager.fable_weekly_threshold,
                now,
                true,
            );
            let threshold = accounts[0]
                .switch_threshold
                .unwrap_or(manager.global_threshold);
            let (gate, _) = Manager::account_gate(
                &accounts[0],
                threshold,
                manager.fable_weekly_threshold,
                now,
                now_ms,
                true,
                None,
                &HashSet::new(),
                &HashSet::new(),
            );
            assert_eq!(
                gate == GateReason::FableWeekly,
                blocked,
                "fableWeeklyThreshold={fable_weekly_threshold:?}: account_gate \
                 and model_blocked must agree on whether this account is \
                 Fable-gated"
            );
        }
    }

    /// The regression this unit exists to fix: every pooled account sits at
    /// 0.96 against a 0.95 threshold (the live-fleet shape from the bridge's
    /// evidence) — the OLD two-tier gate hard-skipped every one of them and
    /// `select()` returned `None`, which the proxy turns into a synthesized
    /// 429. The new three-tier gate must still serve the request.
    #[test]
    fn fleet_all_over_predicted_threshold_is_still_served() {
        let manager = live_manager(config_with(
            vec![account("acct-a"), account("acct-b"), account("acct-c")],
            0.95,
        ));
        for idx in 0..3 {
            set_seven_day_oi(&manager, idx, 0.96);
        }
        let now = OffsetDateTime::now_utc();
        let served = manager.select(
            &HashSet::new(),
            now,
            Some(FABLE_MODEL),
            None,
            "/v1/messages",
            None,
        );
        assert!(
            served.is_some(),
            "a fleet predicted-over-threshold but never rejected upstream must \
             still serve a Fable request via the last resort, not synthesize a 429"
        );
    }

    /// An account upstream has ACTUALLY rejected on `7d_oi` (utilization
    /// `1.0`, the only value `reject_model_weekly` writes) must never be the
    /// last-resort pick, even though it would otherwise sort first by having
    /// the lowest priority/seq tie. The other two accounts are merely
    /// predicted-over, so the pick must land on one of THEM.
    #[test]
    fn a_genuinely_rejected_account_is_never_the_last_resort_pick() {
        let manager = live_manager(config_with(
            vec![account("acct-a"), account("acct-b"), account("acct-c")],
            0.95,
        ));
        set_seven_day_oi(&manager, 0, 1.0); // genuinely rejected upstream
        set_seven_day_oi(&manager, 1, 0.96); // predicted-over, not rejected
        set_seven_day_oi(&manager, 2, 0.97); // predicted-over, not rejected

        let now = OffsetDateTime::now_utc();
        let served = manager
            .select(
                &HashSet::new(),
                now,
                Some(FABLE_MODEL),
                None,
                "/v1/messages",
                None,
            )
            .expect("two accounts remain usable as a last resort");
        assert_ne!(
            served, 0,
            "the account genuinely rejected upstream (utilization 1.0) must \
             never be the last-resort pick"
        );
    }

    /// Every account genuinely rejected upstream (`>= 1.0` on all three) — the
    /// fleet really is exhausted, so `select()` must still answer `None` and
    /// let the proxy's real 429 fire. The last resort must not paper over an
    /// authority-confirmed exhaustion.
    #[test]
    fn fleet_fully_rejected_upstream_still_returns_none() {
        let manager = live_manager(config_with(
            vec![account("acct-a"), account("acct-b")],
            0.95,
        ));
        set_seven_day_oi(&manager, 0, 1.0);
        set_seven_day_oi(&manager, 1, 1.0);

        let now = OffsetDateTime::now_utc();
        let served = manager.select(
            &HashSet::new(),
            now,
            Some(FABLE_MODEL),
            None,
            "/v1/messages",
            None,
        );
        assert_eq!(
            served, None,
            "genuine exhaustion (every account rejected upstream) must still 429"
        );
    }

    /// A non-Fable request against the same over-predicted-threshold fleet must
    /// be byte-identical to today: `is_near` already ignores `7d_oi` for
    /// non-Fable traffic, so this request was never gated on it and the new
    /// last-resort path (gated on `is_fable`) must never even be consulted.
    #[test]
    fn a_non_fable_request_is_unaffected() {
        let manager = live_manager(config_with(
            vec![account("acct-a"), account("acct-b")],
            0.95,
        ));
        set_seven_day_oi(&manager, 0, 0.96);
        set_seven_day_oi(&manager, 1, 0.97);

        let now = OffsetDateTime::now_utc();
        let served = manager.select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            None,
            "/v1/messages",
            None,
        );
        assert!(
            served.is_some(),
            "a non-Fable request must never be gated by the Fable-weekly bucket, \
             last resort or otherwise"
        );
    }

    /// An account still UNDER the predicted threshold is preferred over one
    /// already over it — the last resort only ever widens the pool once the
    /// normal picks already found nothing, it never changes the ORDER a
    /// healthy fleet picks in.
    #[test]
    fn an_account_under_threshold_is_preferred_over_one_above_it() {
        let manager = live_manager(config_with(vec![account("fresh"), account("stale")], 0.95));
        set_seven_day_oi(&manager, 0, 0.50); // fresh: well under threshold
        set_seven_day_oi(&manager, 1, 0.96); // stale: over threshold

        let now = OffsetDateTime::now_utc();
        let served = manager
            .select(
                &HashSet::new(),
                now,
                Some(FABLE_MODEL),
                None,
                "/v1/messages",
                None,
            )
            .expect("the fresh account is eligible normally");
        assert_eq!(
            served, 0,
            "an account under the predicted threshold must be picked over one \
             above it, without ever touching the last resort"
        );
    }

    /// Hard constraint from the bridge: a RESERVED strict group must still
    /// return `None` when its own member cannot serve, even though a last-resort
    /// candidate exists elsewhere in the pool — the last resort widens ONLY the
    /// whole-pool soft fallback, never a reservation, which is a privacy
    /// decision, not a load-balancing one.
    #[test]
    fn a_reserved_strict_group_still_returns_none_with_a_last_resort_candidate_elsewhere() {
        let mut priv_account = account("gil-private");
        priv_account.groups = Some(vec!["priv".to_string()]);
        let mut config = config_with(vec![priv_account, account("pool-account")], 0.95);
        config.group_settings.insert(
            "priv".to_string(),
            config::GroupSettings {
                reserved: true,
                ..Default::default()
            },
        );
        let manager = live_manager(config);
        set_seven_day_oi(&manager, 0, 0.96); // the reserved group's only member
        set_seven_day_oi(&manager, 1, 0.50); // pool-only, would serve as a last resort

        let now = OffsetDateTime::now_utc();
        let served = manager.select_with_group(
            &HashSet::new(),
            now,
            Some(FABLE_MODEL),
            None,
            "/v1/messages",
            None,
            Some("priv"),
        );
        assert_eq!(
            served, None,
            "a reserved group's own request must refuse rather than spill to \
             the whole pool's last resort"
        );
    }
}

#[cfg(test)]
mod stale_pin_load_tests {
    //! A pin's last-touch decides whether it counts as LOAD on its account
    //! ([`PIN_LOAD_IDLE_MS`]), never whether it is honoured. Field report: 100
    //! pins with one touched in the last hour made the emptiest account read as
    //! carrying eight sessions, and every session that landed on it was
    //! migrated straight back off.
    use super::select_test_support::{account, live_manager, pin, pin_of};
    use super::*;
    use std::collections::HashSet;

    /// Two accounts, `loadBalanceMigration` ON (the only mode that counts pins).
    fn build_manager() -> Arc<Manager> {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "loadBalanceMigration".to_string(),
            serde_json::Value::Bool(true),
        );
        let config = Config {
            extra,
            ..Config::fixture(vec![
                account("alice@example.com"),
                account("bob@example.com"),
            ])
        };
        live_manager(config)
    }

    /// Five days stale, the shape of the field report.
    const DAYS_STALE_MS: i64 = 5 * 24 * 60 * 60 * 1000;

    #[test]
    fn the_window_boundary_and_the_requesting_session() {
        let now_ms = 10 * PIN_LOAD_IDLE_MS;
        assert!(pin_counts_as_load(false, now_ms, now_ms), "just touched");
        assert!(
            pin_counts_as_load(false, now_ms - PIN_LOAD_IDLE_MS, now_ms),
            "exactly at the window still counts"
        );
        assert!(
            !pin_counts_as_load(false, now_ms - PIN_LOAD_IDLE_MS - 1, now_ms),
            "one millisecond past the window does not"
        );
        assert!(
            pin_counts_as_load(true, now_ms - DAYS_STALE_MS, now_ms),
            "the requesting session is live however old its previous touch"
        );
    }

    /// Brief test 1. Account 0 carries six days-dead pins plus the requesting
    /// session; account 1 carries one fresh pin. Counting raw pins reads 7 vs 1
    /// and migrates; counting live load reads 1 vs 1 and must not.
    #[test]
    fn stale_pins_do_not_make_an_account_read_as_busy() {
        let manager = build_manager();
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        for key in 1..=6 {
            pin(&manager, key, 0, now_ms - DAYS_STALE_MS);
        }
        pin(&manager, 100, 1, now_ms - 60_000);
        pin(&manager, 50, 0, now_ms - 60_000);

        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(50), "/v1/messages", None),
            Some(0),
            "days-dead pins must not make account 0 look busier than account 1"
        );
        assert_eq!(
            pin_of(&manager, 50),
            Some(0),
            "the session was not migrated"
        );
    }

    /// Brief test 2. The same shape with the other pins FRESH is real load:
    /// 3 live on account 0 vs 0 on account 1 still migrates.
    #[test]
    fn fresh_pins_still_count_as_load() {
        let manager = build_manager();
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        pin(&manager, 1, 0, now_ms - 60_000);
        pin(&manager, 2, 0, now_ms - (PIN_LOAD_IDLE_MS - 60_000));
        pin(&manager, 50, 0, now_ms - 60_000);

        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(50), "/v1/messages", None),
            Some(1),
            "pins inside the window are live load, so the stacked session moves"
        );
        assert_eq!(pin_of(&manager, 50), Some(1));
    }

    /// The re-check under the commit lock must use the same filter as the
    /// decision. Account 1 carries five days-dead pins: the decision reads it
    /// empty and picks it, and an unfiltered re-check (5+1 < 3 is false) would
    /// silently abort the move the decision correctly made.
    #[test]
    fn stale_pins_on_the_target_do_not_abort_a_migration() {
        let manager = build_manager();
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        pin(&manager, 1, 0, now_ms - 60_000);
        pin(&manager, 2, 0, now_ms - 60_000);
        pin(&manager, 50, 0, now_ms - 60_000);
        for key in 11..=15 {
            pin(&manager, key, 1, now_ms - DAYS_STALE_MS);
        }

        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(50), "/v1/messages", None),
            Some(1),
            "the commit-time re-check must not count the target's dead pins"
        );
        assert_eq!(pin_of(&manager, 50), Some(1));
    }

    /// A count filter, not a TTL: no stale pin is removed from the map. The
    /// session returning after days is a different question: its cache is cold
    /// on every account, so its pin is not honoured and it takes the normal pick
    /// (`a_cold_pin_takes_the_reset_order_again`), here the emptier account 0,
    /// and is re-pinned there. Every OTHER stale pin is left exactly as it was.
    #[test]
    fn a_stale_pin_takes_the_pick_and_no_other_pin_is_dropped() {
        let manager = build_manager();
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        for key in 1..=6 {
            pin(&manager, key, 1, now_ms - DAYS_STALE_MS);
        }
        pin(&manager, 50, 1, now_ms - DAYS_STALE_MS);

        let served = manager.select(&HashSet::new(), now, None, Some(50), "/v1/messages", None);
        assert!(served.is_some(), "the returning session is served");
        let pins = manager
            .affinity
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(pins.len(), 7, "no stale pin was evicted by the filter");
        assert_eq!(
            pins.get(&50).map(|&(i, _)| i),
            served,
            "the returning session is re-pinned where the pick served it"
        );
        assert!((1..=6).all(|k| pins.get(&k).map(|&(i, _)| i) == Some(1)));
    }
}

/// **Selection filters by provider FIRST**: a Claude request and a Codex
/// account have nothing to say to each other, in either direction (see
/// `docs/architecture.md`, "Codex requests").
///
/// Every way out of `select_with_group` is a door, and an exhausted Anthropic
/// fleet must answer 429 rather than spill onto a Codex account through any of
/// them: the pool pick ([`Manager::eligible`]), the hard account lock, a
/// session's pin, the control-preferred overlay, the Fable last resort, and the
/// revalidation serve the proxy falls back to when the pick comes back empty.
/// Each test below names the door it guards and the line that closes it.
#[cfg(test)]
mod provider_filter_tests {
    use super::select_test_support::{
        account, config_with, live_manager, live_manager_on_file, pin, pin_of,
    };
    use super::*;
    use crate::config::Account;
    use crate::provider::Provider;
    use crate::quota::QuotaWindow;
    use std::collections::HashSet;
    use time::Duration;

    const ANTHROPIC_PATH: &str = "/v1/messages";
    const CODEX_PATH: &str = "/backend-api/codex/responses";

    /// The same account, on the Codex upstream.
    fn codex(name: &str) -> Account {
        Account {
            provider: Provider::Codex,
            ..account(name)
        }
    }

    /// [`account`] as a runtime row, built by the constructor `assemble` uses,
    /// for the pure [`Manager::eligible`] predicate.
    fn runtime(name: &str) -> AccountRuntime {
        AccountRuntime::from_config(&account(name), false)
    }

    #[test]
    fn a_loaded_strict_codex_peer_exit_is_skipped_by_every_selection_path() {
        let peer = tcr_peer_wire::PeerId([0x55; 32]);
        let mut failures = Vec::new();
        for door in ["pool", "affinity", "locked", "revalidation", "bound"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(crate::paths::CONFIG_FILE);
            let mut config = config_with(vec![
                Account {
                    egress: config::Egress::Via(peer),
                    egress_strict: true,
                    ..codex("alice@example.com/codex")
                },
                codex("bob@example.com/codex"),
            ]);
            config.proxy.port = 9;
            if door == "locked" {
                config.lock_account = Some("alice@example.com/codex".into());
            }
            config::save(&path, &config).unwrap();
            let config = config::load(&path).unwrap();
            assert_eq!(
                config.accounts.len(),
                2,
                "the row remains available for repair"
            );
            assert!(config.accounts[0].egress_strict);
            let manager = live_manager(config);
            let now = OffsetDateTime::now_utc();
            pin(&manager, 77, 0, odt_to_ms(now));
            let got = match door {
                "pool" | "locked" => {
                    manager.select(&HashSet::new(), now, None, None, CODEX_PATH, None)
                }
                "affinity" => {
                    manager.select(&HashSet::new(), now, None, Some(77), CODEX_PATH, None)
                }
                "revalidation" => {
                    manager.select_revalidation(&HashSet::new(), now, None, Some(77), CODEX_PATH)
                }
                "bound" => manager.select_bound(
                    0,
                    &HashSet::new(),
                    now,
                    false,
                    None,
                    None,
                    WarmWindow::DEFAULT,
                ),
                _ => unreachable!(),
            };
            let expected = if matches!(door, "locked" | "bound") {
                None
            } else {
                Some(1)
            };
            if got != expected {
                failures.push(format!("{door}: selected {got:?}, expected {expected:?}"));
            }
            if matches!(door, "affinity" | "revalidation") && pin_of(&manager, 77) != Some(1) {
                failures.push(format!(
                    "{door}: retained the unusable account's affinity pin"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn supported_peer_exit_settings_keep_their_selection_behavior() {
        let peer = tcr_peer_wire::PeerId([0x55; 32]);
        for (provider, egress, strict) in [
            (Provider::Anthropic, config::Egress::Via(peer), true),
            (Provider::Codex, config::Egress::Via(peer), false),
            (Provider::Codex, config::Egress::Local, true),
        ] {
            let manager = live_manager(config_with(vec![Account {
                provider,
                egress,
                egress_strict: strict,
                ..account("alice@example.com")
            }]));
            let path = match provider {
                Provider::Anthropic => ANTHROPIC_PATH,
                Provider::Codex => CODEX_PATH,
            };
            assert_eq!(
                manager.select(
                    &HashSet::new(),
                    OffsetDateTime::now_utc(),
                    None,
                    None,
                    path,
                    None
                ),
                Some(0),
                "{provider:?} {egress:?}, strict={strict} remains selectable"
            );
        }
    }

    /// Set account `idx`'s window to `utilization`, resetting two hours out.
    fn set_window(
        manager: &Manager,
        idx: usize,
        window: fn(&mut Quota) -> &mut Option<QuotaWindow>,
        utilization: f64,
    ) {
        let now = OffsetDateTime::now_utc();
        let mut accounts = manager
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *window(&mut accounts[idx].quota) = Some(QuotaWindow {
            utilization,
            reset: Some(now + Duration::hours(2)),
        });
    }

    #[test]
    fn classify_request_treats_the_codex_responses_path_as_inference() {
        assert_eq!(
            classify_request("/backend-api/codex/responses"),
            RequestClass::Inference
        );
        assert_eq!(classify_request("/v1/messages"), RequestClass::Inference);
        // Every other Codex path is relayed by the proxy before classification matters; it must at
        // least not be mistaken for inference.
        assert_ne!(
            classify_request("/backend-api/wham/usage"),
            RequestClass::Inference
        );
    }

    /// The pool pick's door: `eligible` refuses a provider mismatch before any
    /// other check, in both directions.
    ///
    /// Watched red: delete `eligible`'s `if account.provider != provider`
    /// guard and the first assertion passes a Codex account for an Anthropic
    /// request.
    #[test]
    fn eligible_refuses_an_account_of_the_other_provider() {
        let mut codex = runtime("alice@example.com");
        codex.provider = Provider::Codex;
        let anthropic = runtime("bob@example.com");
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let pacing = PacingConfig::default();
        let empty = HashSet::new();
        // Same call shape as the existing `eligible` tests in this file; only `provider` is new.
        assert!(
            !Manager::eligible(
                &codex,
                0.9,
                None,
                &pacing,
                false,
                now,
                now_ms,
                false,
                Provider::Anthropic,
                None,
                None,
                &empty,
                &empty
            ),
            "a Codex account must never serve an Anthropic request"
        );
        assert!(
            !Manager::eligible(
                &anthropic,
                0.9,
                None,
                &pacing,
                false,
                now,
                now_ms,
                false,
                Provider::Codex,
                None,
                None,
                &empty,
                &empty
            ),
            "an Anthropic account must never serve a Codex request"
        );
        assert!(Manager::eligible(
            &codex,
            0.9,
            None,
            &pacing,
            false,
            now,
            now_ms,
            false,
            Provider::Codex,
            None,
            None,
            &empty,
            &empty
        ));
    }

    /// One Codex account free and the only Anthropic account disabled: the
    /// Anthropic request gets `None`, which the proxy turns into an honest
    /// 429, and the Codex request is served.
    ///
    /// Watched red: the same `eligible` guard; without it the Anthropic request
    /// lands on the Codex account.
    #[test]
    fn select_with_only_codex_accounts_free_refuses_an_anthropic_request() {
        let disabled_anthropic = Account {
            disabled: Some(true),
            ..account("bob@example.com")
        };
        let manager = live_manager(config_with(vec![
            codex("alice@example.com"),
            disabled_anthropic,
        ]));
        let picked = manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            Some("claude-opus-5"),
            None,
            "/v1/messages",
            None,
        );
        assert_eq!(
            picked, None,
            "an exhausted Anthropic fleet must 429, never spill to Codex"
        );
        let picked = manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            Some("gpt-6-astra"),
            None,
            "/backend-api/codex/responses",
            None,
        );
        assert_eq!(
            picked,
            Some(0),
            "the Codex account serves the Codex request"
        );
    }

    /// A hard lock constrains its own provider only. The other provider still
    /// selects from its own pool and never receives the locked credential.
    #[test]
    fn a_hard_account_lock_never_serves_the_other_provider() {
        let mut config = config_with(vec![codex("alice@example.com"), account("bob@example.com")]);
        config.lock_account = Some("alice@example.com".to_string());
        let manager = live_manager(config);
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, ANTHROPIC_PATH, None),
            Some(1),
            "a Codex lock must leave the Anthropic pool available"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, CODEX_PATH, None),
            Some(0),
            "the lock still holds for its own provider's requests"
        );
    }

    /// A Claude lock leaves Codex selection independent, while its own
    /// provider continues to use the exact locked slot.
    #[test]
    fn a_hard_account_lock_on_an_anthropic_account_never_serves_a_codex_request() {
        let mut config = config_with(vec![account("alice@example.com"), codex("bob@example.com")]);
        config.lock_account = Some("alice@example.com".to_string());
        let manager = live_manager(config);
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, CODEX_PATH, None),
            Some(1),
            "a Claude lock must leave the Codex pool available"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, ANTHROPIC_PATH, None),
            Some(0),
            "the lock still holds for its own provider's requests"
        );
    }

    #[test]
    fn provider_isolation_claude_lock_does_not_hide_codex_capacity() {
        let mut config = config_with(vec![account("alice@example.com"), codex("bob@example.com")]);
        config.lock_account = Some("alice@example.com".into());
        let manager = live_manager(config);
        assert!(manager.codex_account_can_serve_eventually(None));
        assert!(manager.codex_account_can_serve_eventually(Some(1)));
        assert!(!manager.codex_account_can_serve_eventually(Some(0)));
        assert_eq!(
            manager.select(
                &HashSet::from([0]),
                OffsetDateTime::now_utc(),
                None,
                None,
                ANTHROPIC_PATH,
                None
            ),
            None,
            "the lock still prevents failover within its own provider"
        );
    }

    #[test]
    fn provider_isolation_codex_lock_uses_its_slot_not_a_shared_name() {
        let mut config = config_with(vec![
            Account {
                disabled: Some(true),
                account_uuid: Some("11111111-1111-1111-1111-111111111111".into()),
                ..codex("alice@example.com")
            },
            Account {
                account_uuid: Some("22222222-2222-2222-2222-222222222222".into()),
                ..codex("alice@example.com")
            },
        ]);
        config.lock_account = Some("alice@example.com".into());
        let manager = live_manager(config);
        assert!(
            !manager.codex_account_can_serve_eventually(None),
            "a healthy same-named row is not the locked slot"
        );
        assert!(!manager.codex_account_can_serve_eventually(Some(1)));
        assert_eq!(
            manager.select(
                &HashSet::from([0]),
                OffsetDateTime::now_utc(),
                None,
                None,
                CODEX_PATH,
                None
            ),
            None,
            "a failed locked slot never falls through to its same-named sibling"
        );
    }

    #[test]
    fn provider_isolation_unsupported_exit_excludes_only_its_exact_slot() {
        let manager = live_manager(config_with(vec![
            Account {
                account_uuid: Some("11111111-1111-1111-1111-111111111111".into()),
                egress: config::Egress::Via(tcr_peer_wire::PeerId([0x55; 32])),
                egress_strict: true,
                ..codex("alice@example.com")
            },
            Account {
                account_uuid: Some("22222222-2222-2222-2222-222222222222".into()),
                ..codex("alice@example.com")
            },
        ]));
        let now = OffsetDateTime::now_utc();
        pin(&manager, 77, 1, odt_to_ms(now));
        assert_eq!(
            manager.codex_accounts_with_unsupported_exit(),
            HashSet::from([0])
        );
        assert!(manager.codex_account_can_serve_eventually(Some(1)));
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(77), CODEX_PATH, None),
            Some(1)
        );
        assert_eq!(
            pin_of(&manager, 77),
            Some(1),
            "the healthy same-named account keeps its pin"
        );
    }

    #[test]
    fn provider_isolation_request_reads_do_not_wait_for_a_config_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::paths::CONFIG_FILE);
        let mut config = config_with(vec![account("alice@example.com"), codex("bob@example.com")]);
        config.proxy.port = 9;
        config::save(&path, &config).unwrap();
        let manager = live_manager_on_file(config.clone(), path.clone());
        assert_eq!(manager.provider_account_count(Provider::Anthropic), 1);
        let writing = manager.config_write.lock().unwrap();
        config.accounts.remove(0);
        config::save(&path, &config).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_manager = Arc::clone(&manager);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let count = worker_manager.provider_account_count(Provider::Anthropic);
            let selected = worker_manager.select(
                &HashSet::new(),
                OffsetDateTime::now_utc(),
                None,
                None,
                ANTHROPIC_PATH,
                None,
            );
            let codex_available = worker_manager.codex_account_can_serve_eventually(None);
            done_tx.send((count, selected, codex_available)).unwrap();
        });
        started_rx.recv().unwrap();
        let result = done_rx.recv_timeout(std::time::Duration::from_millis(500));
        drop(writing);
        worker.join().unwrap();
        assert_eq!(
            result.expect("request reads must use cached state during an in-flight config write"),
            (1, Some(0), true)
        );
        assert_eq!(
            manager.provider_account_count(Provider::Anthropic),
            0,
            "the completed delete is reloaded on the next request"
        );
        assert_eq!(
            manager.select(
                &HashSet::new(),
                OffsetDateTime::now_utc(),
                None,
                None,
                ANTHROPIC_PATH,
                None
            ),
            None
        );
    }

    /// Send once on account `first_idx` (0 is Anthropic, 1 is Codex), then expect the
    /// other provider's account to send within 250 ms, failing with `why` if it is paced.
    /// Answers the other account's index and the path each index sends on.
    async fn send_on_one_provider_then_the_other(
        manager: &Manager,
        first_idx: usize,
        why: &str,
    ) -> (usize, [&'static str; 2]) {
        let second_idx = 1 - first_idx;
        let paths = [ANTHROPIC_PATH, CODEX_PATH];
        manager.throttle_send(paths[first_idx], first_idx).await;
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            manager.throttle_send(paths[second_idx], second_idx),
        )
        .await
        .expect(why);
        (second_idx, paths)
    }

    #[tokio::test]
    async fn provider_isolation_fleet_throttle_does_not_pace_the_other_upstream() {
        for first in [Provider::Anthropic, Provider::Codex] {
            let mut config =
                config_with(vec![account("alice@example.com"), codex("bob@example.com")]);
            config.fleet_throttle = ThrottleConfig {
                min_spacing_ms: Some(60_000),
                burst: Some(1),
            };
            let manager = live_manager(config);
            let first_idx = match first {
                Provider::Anthropic => 0,
                Provider::Codex => 1,
            };
            let (second_idx, paths) = send_on_one_provider_then_the_other(
                &manager,
                first_idx,
                "the other provider has its own fleet burst",
            )
            .await;
            for idx in [first_idx, second_idx] {
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(10),
                        manager.throttle_send(paths[idx], idx)
                    )
                    .await
                    .is_err(),
                    "same-provider requests still consume the fleet throttle"
                );
            }
        }
    }

    async fn assert_org_throttle_is_provider_scoped(org_uuid: Option<&str>) {
        for first_idx in [0, 1] {
            let mut accounts = vec![
                account("alice@example.com"),
                codex("bob@example.com"),
                account("carol@example.com"),
                codex("dave@example.com"),
            ];
            for account in &mut accounts {
                account.org_uuid = org_uuid.map(str::to_string);
                account.org_name = Some("Example Org".into());
            }
            let mut config = config_with(accounts);
            config.account_throttle = ThrottleConfig {
                min_spacing_ms: Some(60_000),
                burst: Some(1),
            };
            let manager = live_manager(config);
            let (second_idx, paths) = send_on_one_provider_then_the_other(
                &manager,
                first_idx,
                "equal organization identities on different providers have independent bursts",
            )
            .await;
            for idx in [first_idx, second_idx] {
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(10),
                        manager.throttle_send(paths[idx], idx + 2)
                    )
                    .await
                    .is_err(),
                    "accounts in one organization on the same provider still share its throttle"
                );
            }
        }
    }

    #[tokio::test]
    async fn provider_isolation_org_throttle_uuid_does_not_pace_the_other_upstream() {
        assert_org_throttle_is_provider_scoped(Some("11111111-1111-1111-1111-111111111111")).await;
    }

    #[tokio::test]
    async fn provider_isolation_org_throttle_name_does_not_pace_the_other_upstream() {
        assert_org_throttle_is_provider_scoped(None).await;
    }

    /// **A SESSION'S PIN NEVER CROSSES PROVIDERS.** The affinity fast path
    /// serves a pin that fails `eligible` whenever the account is still alive
    /// (`Manager::account_hard_ok`) and the session is warm: the over-threshold
    /// pin-honor serve. A pin on the other provider's account passes
    /// `account_hard_ok`, which knows nothing about the request, so without its
    /// own check that serve hands an Anthropic request to a Codex account.
    ///
    /// A DIVERT and not a re-key: the mismatch is a fact about this request,
    /// like the model-class and strict-group diverts, so the pin stays where
    /// the session's other traffic is warm.
    ///
    /// Watched red: delete the fast path's provider divert and the request is
    /// served on the pinned Codex account (`Some(0)`) by the pin-honor serve.
    #[test]
    fn a_pin_on_the_other_providers_account_is_diverted_never_served() {
        let manager = live_manager(config_with(vec![
            codex("alice@example.com"),
            account("bob@example.com"),
        ]));
        let now = OffsetDateTime::now_utc();
        // Touched just now, so the session is warm and the pin-honor serve is live.
        pin(&manager, 7, 0, odt_to_ms(now));
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(7), ANTHROPIC_PATH, None),
            Some(1),
            "the Anthropic request is served by the Anthropic account"
        );
        assert_eq!(
            pin_of(&manager, 7),
            Some(0),
            "a divert, not a re-key: the session keeps its pin"
        );
    }

    /// **THE CONTROL-PREFERRED OVERLAY'S DOOR.** Only the Codex responses path
    /// is inference; every other `/backend-api/` path classifies as
    /// control-preferred, and the overlay routes it through
    /// `Manager::control_eligible`, which deliberately bypasses `eligible`. The
    /// control account is an Anthropic identity (`assemble` refuses a Codex
    /// one), so without a provider check a Codex control-plane request is
    /// routed onto it. The proxy relays those paths under the client's own
    /// bearer before selection; this is the picker's own guarantee.
    ///
    /// Watched red: drop the overlay's provider check and the Codex request is
    /// served on the Anthropic control account (`Some(1)`).
    #[test]
    fn the_control_overlay_never_routes_the_other_providers_request() {
        let mut config = config_with(vec![codex("alice@example.com"), account("bob@example.com")]);
        config.control_account = Some(config::ControlAccountRef::Name(
            "bob@example.com".to_string(),
        ));
        let manager = live_manager(config);
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                None,
                "/backend-api/wham/usage",
                None
            ),
            Some(0),
            "a Codex control-plane request degrades to its own provider's pool, never the Anthropic control account"
        );
        // The control: an Anthropic control-plane request still prefers it.
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/organizations", None),
            Some(1),
            "control-preferred routing is unchanged for its own provider"
        );
    }

    /// **THE FABLE LAST RESORT'S DOOR.** `pick_fable_last_resort` mirrors
    /// `eligible`'s gates by hand rather than calling it, and a Codex account
    /// carries no `7d_oi` window at all (tcr reads no model-scoped window from
    /// Codex), so it reads as the FRESHEST candidate and would win the last
    /// resort outright.
    ///
    /// Watched red: drop the last resort's provider check and the Fable request
    /// is served on the Codex account (`Some(1)`).
    #[test]
    fn the_fable_last_resort_never_reaches_the_other_provider() {
        let manager = live_manager(config_with(vec![
            account("alice@example.com"),
            codex("bob@example.com"),
        ]));
        // Over the predicted Fable threshold (0.90), not rejected upstream: the
        // shape that sends a Fable request to the last resort.
        set_window(&manager, 0, |q| &mut q.seven_day_oi, 0.97);
        assert_eq!(
            manager.select(
                &HashSet::new(),
                OffsetDateTime::now_utc(),
                Some("claude-fable-5"),
                None,
                ANTHROPIC_PATH,
                None
            ),
            Some(0),
            "the last resort spends its one request on the Anthropic account, never the Codex one"
        );
    }

    /// **THE REVALIDATION SERVE'S DOOR**, the one the proxy opens when the pick
    /// comes back empty (`revalidationServe` ships ON). Its fallback serves the
    /// least-utilized account that passes the hard gates alone, and an idle
    /// Codex account is the least utilized account on any mixed fleet whose
    /// Anthropic half is over the soft threshold.
    ///
    /// Watched red: drop the provider check from `select_revalidation`'s
    /// `hard_ok` and the Anthropic request is revalidated onto the Codex
    /// account (`Some(1)`).
    #[test]
    fn codex_and_claude_revalidation_valves_are_independent() {
        for order in [
            [(CODEX_PATH, 1), (ANTHROPIC_PATH, 0)],
            [(ANTHROPIC_PATH, 0), (CODEX_PATH, 1)],
        ] {
            let manager = live_manager(config_with(vec![
                account("alice@example.com"),
                codex("bob@example.com"),
            ]));
            set_window(&manager, 0, |quota| &mut quota.five_hour, 0.99);
            set_window(&manager, 1, |quota| &mut quota.five_hour, 0.99);
            let now = OffsetDateTime::now_utc();
            for (path, index) in order {
                assert_eq!(
                    manager.select(&HashSet::new(), now, None, None, path, None),
                    None
                );
                assert_eq!(
                    manager.select_revalidation(&HashSet::new(), now, None, None, path),
                    Some(index),
                    "one provider must not spend the other's revalidation valve"
                );
                assert_eq!(
                    manager.select_revalidation(&HashSet::new(), now, None, None, path),
                    None,
                    "same-provider bursts remain bounded"
                );
            }
        }
    }

    #[test]
    fn revalidation_never_serves_the_other_provider() {
        let manager = live_manager(config_with(vec![
            account("alice@example.com"),
            codex("bob@example.com"),
        ]));
        set_window(&manager, 0, |q| &mut q.five_hour, 0.95);
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, ANTHROPIC_PATH, None),
            None,
            "precondition: the Anthropic pool reads exhausted to the normal pick"
        );
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, None, ANTHROPIC_PATH),
            Some(0),
            "revalidation serves the over-threshold Anthropic account, never the idle Codex one"
        );
    }

    /// **THE REVALIDATION SERVE'S PIN-HONOR DOOR**, the pin half of the test
    /// above. A session pinned on the Codex account sends an Anthropic request
    /// into `select_revalidation`: the pin-honor serve must refuse the pin (its
    /// `hard_ok` asks the provider first) and the fallback serves the Anthropic
    /// account. The pin itself is KEPT: the Codex account is alive
    /// (`Manager::account_hard_ok` judges the account, not the request), so this
    /// is a divert like `select`'s other-provider divert, never a re-key.
    ///
    /// Watched red twice: drop the provider check from `select_revalidation`'s
    /// `hard_ok` and the pin-honor serve hands the request to the Codex account
    /// (`Some(0)`); make the keep-pin test ask the provider too (a re-key) and
    /// the session's pin moves to the Anthropic account.
    #[test]
    fn revalidation_diverts_a_pin_on_the_other_providers_account_and_keeps_it() {
        let manager = live_manager(config_with(vec![
            codex("alice@example.com"),
            account("bob@example.com"),
        ]));
        let now = OffsetDateTime::now_utc();
        // Touched just now: a warm session, so nothing but the provider stands
        // between the pin and the pin-honor serve.
        pin(&manager, 7, 0, odt_to_ms(now));
        assert_eq!(
            manager.select_revalidation(&HashSet::new(), now, None, Some(7), ANTHROPIC_PATH),
            Some(1),
            "the Anthropic request is revalidated onto the Anthropic account"
        );
        assert_eq!(
            pin_of(&manager, 7),
            Some(0),
            "a divert, not a re-key: the session keeps its pin"
        );
    }

    /// The Fable weekly bucket is an Anthropic fact; a Codex request never
    /// consults it, at either place a request's class is computed
    /// (`select_with_group` and `select_revalidation`).
    ///
    /// The model id is a fixture that merely CONTAINS "fable", because that is
    /// all `crate::model::is_fable_model` asks. No Codex account carries a
    /// `7d_oi` window (tcr reads no model-scoped window from Codex), so one is
    /// set by hand here: it is the only way to observe the gate, and the test
    /// pins the rule rather than a failure seen live.
    ///
    /// Watched red: drop the `provider == Anthropic` term from either `is_fable`
    /// and that call answers `None` (the window reads as a hard rejection).
    #[test]
    fn a_codex_request_never_consults_the_fable_weekly_bucket() {
        let now = OffsetDateTime::now_utc();
        let manager = live_manager(config_with(vec![codex("alice@example.com")]));
        set_window(&manager, 0, |q| &mut q.seven_day_oi, 1.0);
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                Some("gpt-6-fable"),
                None,
                CODEX_PATH,
                None
            ),
            Some(0),
            "select must not gate a Codex request on the Fable bucket"
        );
        // A second manager: the revalidation serve spends a fleet-wide 2 s
        // window per serve, and this assertion must not depend on that.
        let manager = live_manager(config_with(vec![codex("alice@example.com")]));
        set_window(&manager, 0, |q| &mut q.seven_day_oi, 1.0);
        assert_eq!(
            manager.select_revalidation(
                &HashSet::new(),
                now,
                Some("gpt-6-fable"),
                None,
                CODEX_PATH
            ),
            Some(0),
            "select_revalidation must not gate a Codex request on the Fable bucket"
        );
    }
}

#[cfg(test)]
mod routing_candidates_tests {
    //! [`Manager::routing_candidates`] is the selector's own answer for a new, unpinned
    //! request: the pass that would serve it and every account that pass could choose,
    //! read without changing anything a selection reads.
    use super::select_test_support::{account, config_with, live_manager, pin};
    use super::*;
    use crate::quota::QuotaWindow;
    use time::Duration;

    const MESSAGES: &str = "/v1/messages";
    const MODELS: [Option<&str>; 3] = [None, Some("claude-opus-5-5"), Some("claude-fable-5-1")];

    /// Three healthy Anthropic accounts, `tune` applied to their config.
    fn fleet(tune: fn(&mut Config)) -> Arc<Manager> {
        let mut config = config_with(vec![
            account("a@example.com"),
            account("b@example.com"),
            account("c@example.com"),
        ]);
        tune(&mut config);
        live_manager(config)
    }

    /// Change every listed account's runtime, as upstream answers and holds would.
    fn each(manager: &Manager, slots: &[usize], change: impl Fn(&mut AccountRuntime)) {
        let mut accounts = manager
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for &slot in slots {
            change(&mut accounts[slot]);
        }
    }

    fn window(utilization: f64, now: OffsetDateTime) -> Option<QuotaWindow> {
        Some(QuotaWindow {
            utilization,
            reset: Some(now + Duration::hours(2)),
        })
    }

    /// What a new, unpinned request for `model` is served from, through the calls the proxy
    /// makes: `select`, then the revalidation serve when it is on, no lock is set and `select`
    /// found nothing.
    fn served(manager: &Manager, now: OffsetDateTime, model: Option<&str>) -> Option<usize> {
        let none = HashSet::new();
        manager
            .select(&none, now, model, None, MESSAGES, None)
            .or_else(|| {
                (manager.revalidation_serve_enabled()
                    && manager
                        .locked_account_for(crate::provider::Provider::Anthropic)
                        .is_none())
                .then(|| manager.select_revalidation(&none, now, model, None, MESSAGES))
                .flatten()
            })
    }

    struct Case {
        name: &'static str,
        tune: fn(&mut Config),
        setup: fn(&Manager, OffsetDateTime),
        /// The pass and its candidates, for no model, an Opus model and a Fable model.
        expect: [(Option<PoolPass>, &'static [usize]); 3],
    }

    const ALL: &[usize] = &[0, 1, 2];

    fn cases() -> Vec<Case> {
        vec![
            Case {
                name: "a healthy fleet",
                tune: |_| {},
                setup: |_, _| {},
                expect: [(Some(PoolPass::Paced), ALL); 3],
            },
            Case {
                name: "one held, one disabled",
                tune: |_| {},
                setup: |manager, now| {
                    let until = odt_to_ms(now) + 600_000;
                    each(manager, &[0], |a| a.rate_limited_until_ms = Some(until));
                    each(manager, &[1], |a| a.disabled = true);
                },
                expect: [(Some(PoolPass::Paced), &[2]); 3],
            },
            Case {
                name: "Fable predicted out on two",
                tune: |_| {},
                setup: |manager, now| {
                    each(manager, &[0, 1], |a| {
                        a.quota.seven_day_oi = window(0.97, now);
                    });
                },
                expect: [
                    (Some(PoolPass::Paced), ALL),
                    (Some(PoolPass::Paced), ALL),
                    (Some(PoolPass::Paced), &[2]),
                ],
            },
            Case {
                name: "Fable predicted out everywhere",
                tune: |_| {},
                setup: |manager, now| {
                    each(manager, ALL, |a| a.quota.seven_day_oi = window(0.97, now));
                },
                expect: [
                    (Some(PoolPass::Paced), ALL),
                    (Some(PoolPass::Paced), ALL),
                    (Some(PoolPass::FableLastResort), ALL),
                ],
            },
            Case {
                name: "Fable rejected everywhere",
                tune: |_| {},
                setup: |manager, now| {
                    each(manager, ALL, |a| a.quota.seven_day_oi = window(1.0, now));
                },
                expect: [
                    (Some(PoolPass::Paced), ALL),
                    (Some(PoolPass::Paced), ALL),
                    (None, &[]),
                ],
            },
            Case {
                name: "every account paced",
                tune: |config| {
                    config.pacing.max_in_flight_per_account = Some(1);
                },
                setup: |manager, _| each(manager, ALL, |a| a.in_flight = 1),
                expect: [(Some(PoolPass::LeastLoaded), ALL); 3],
            },
            Case {
                name: "every account over the switch threshold",
                tune: |_| {},
                setup: |manager, now| {
                    each(manager, ALL, |a| a.quota.five_hour = window(0.95, now));
                },
                expect: [(Some(PoolPass::Revalidation), ALL); 3],
            },
            Case {
                name: "every account over the switch threshold, revalidation off",
                tune: |config| {
                    config
                        .extra
                        .insert("revalidationServe".to_string(), serde_json::json!(false));
                },
                setup: |manager, now| {
                    each(manager, ALL, |a| a.quota.five_hour = window(0.95, now));
                },
                expect: [(None, &[]); 3],
            },
            Case {
                name: "every account held",
                tune: |_| {},
                setup: |manager, now| {
                    let until = odt_to_ms(now) + 600_000;
                    each(manager, ALL, |a| a.rate_limited_until_ms = Some(until));
                },
                expect: [(None, &[]); 3],
            },
            Case {
                name: "the control account is not the pool's",
                tune: |config| {
                    config.control_account =
                        Some(config::ControlAccountRef::Name("a@example.com".to_string()));
                },
                setup: |_, _| {},
                expect: [(Some(PoolPass::Paced), &[1, 2]); 3],
            },
            Case {
                name: "a lock serves its account whatever its gates",
                tune: |config| config.lock_account = Some("b@example.com".to_string()),
                setup: |manager, _| each(manager, &[1], |a| a.disabled = true),
                expect: [(Some(PoolPass::Lock), &[1]); 3],
            },
        ]
    }

    /// **Routing reads what the picks serve.** For each fleet and model, the pass routing
    /// names is the one that serves, a model is routable exactly when a new request for it
    /// is served, and the account served is one routing counted.
    ///
    /// Each model gets a fresh fleet: a pick stamps its account, and a revalidation serve
    /// spends the window the next one would need.
    #[test]
    fn routing_reads_what_the_picks_serve() {
        for case in cases() {
            for (model, (pass, accounts)) in MODELS.into_iter().zip(case.expect) {
                let manager = fleet(case.tune);
                let now = OffsetDateTime::now_utc();
                (case.setup)(&manager, now);
                let found = manager.routing_candidates(now, model, MESSAGES);
                let name = case.name;
                assert_eq!(
                    (found.pass, found.accounts.as_slice()),
                    (pass, accounts),
                    "{name}, {model:?}"
                );
                let pick = served(&manager, now, model);
                assert_eq!(
                    pick.is_some(),
                    found.pass.is_some(),
                    "{name}, {model:?}: routable must be exactly what a new request is served"
                );
                if let Some(idx) = pick {
                    assert!(
                        found.accounts.contains(&idx),
                        "{name}, {model:?}: served {idx}, routing counted {:?}",
                        found.accounts
                    );
                }
            }
        }
    }

    /// Everything a selection reads and writes besides the gates themselves.
    #[derive(Debug, PartialEq)]
    struct SelectorState {
        stamps: Vec<u64>,
        select_seq: u64,
        pins: Vec<(u64, (usize, i64))>,
        revalidation_at: (i64, i64),
        connections: usize,
        diverts: usize,
    }

    fn selector_state(manager: &Manager) -> SelectorState {
        let mut pins: Vec<(u64, (usize, i64))> = manager
            .affinity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(&key, &pin)| (key, pin))
            .collect();
        pins.sort_unstable();
        SelectorState {
            stamps: manager
                .accounts
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|a| a.last_selected_seq)
                .collect(),
            select_seq: manager.select_seq.load(Ordering::Relaxed),
            pins,
            revalidation_at: (
                manager.next_revalidation_at_ms.load(Ordering::Relaxed),
                manager
                    .next_codex_revalidation_at_ms
                    .load(Ordering::Relaxed),
            ),
            connections: manager
                .conn_affinity
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
            diverts: manager
                .divert_ledger
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
        }
    }

    /// **A routing read changes nothing the selector reads**: no stamp, no pin, no
    /// revalidation window spent. A fleet that was read selects exactly as its unread twin
    /// does, on a healthy fleet and on one only revalidation can serve.
    #[test]
    fn a_routing_read_changes_no_selector_state() {
        for over_threshold in [false, true] {
            let now = OffsetDateTime::now_utc();
            let setup = |manager: &Manager| {
                pin(manager, 7, 0, odt_to_ms(now));
                if over_threshold {
                    each(manager, ALL, |a| a.quota.five_hour = window(0.95, now));
                }
            };
            let read = fleet(|_| {});
            let twin = fleet(|_| {});
            setup(&read);
            setup(&twin);
            let before = selector_state(&read);
            for model in MODELS {
                assert_eq!(
                    read.routing_candidates(now, model, MESSAGES),
                    read.routing_candidates(now, model, MESSAGES),
                    "over_threshold={over_threshold}, {model:?}: a read does not change the next read"
                );
            }
            assert_eq!(
                selector_state(&read),
                before,
                "over_threshold={over_threshold}"
            );
            for turn in 0..3 {
                assert_eq!(
                    served(&read, now, MODELS[1]),
                    served(&twin, now, MODELS[1]),
                    "over_threshold={over_threshold}, turn {turn}: the read fleet selects as its twin"
                );
            }
            assert_eq!(selector_state(&read), selector_state(&twin));
        }
    }
}
