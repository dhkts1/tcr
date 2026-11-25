//! `Manager` account-selection and eligibility methods, split verbatim from `mod.rs`.
//!
//! This file holds the selector's shared vocabulary: the divert verdict, the
//! threshold resolvers and the pool-pass types. The request classes are the
//! parent's ([`RequestClass`]), because the throttle reads them too. Each child
//! holds one concern: the entry points ([`routing`]), the whole selection path
//! ([`with_group`]), the revalidation serve ([`revalidation`]), the per-account
//! gates ([`gates`]), the Codex slot filters ([`codex`]), the pool picks
//! ([`pool`]), the memory-only connection and divert maps ([`ledgers`]) and the verdicts on a
//! conversation's bound account ([`bound`]).

use super::*;

mod bound;
mod codex;
mod gates;
mod ledgers;
mod pool;
mod revalidation;
mod routing;
mod with_group;

/// Bound on [`Manager::affinity`] and on its side set
/// [`Manager::affinity_extended`]. Stable pins survive reconnects, so this size
/// cap, not a disconnect hook, is what keeps a long-lived proxy from growing
/// the map without limit.
pub(super) const AFFINITY_CAP: usize = 1024;
/// Bound on [`Manager::conn_affinity`] — see its doc-comment for why the map is
/// memory-only and never persisted.
pub(super) const CONN_AFFINITY_CAP: usize = 256;

/// Why the group-respecting first pass of [`Manager::select_with_group`] came up
/// empty, so the soft fallback had to drop the group and serve from the whole
/// pool.
///
/// Exists because the log line at that fallback used to hardcode "under pacing"
/// for every group miss. Pacing ships OFF ([`crate::base::config::default_pacing`]), so
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
    pub(super) provider: crate::base::provider::Provider,
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
mod tests;
