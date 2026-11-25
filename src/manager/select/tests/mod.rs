//! The selector's tests, one file per test module. A module's doc, where it has one
//! outside its file, sits on its `mod` line here.

use super::with_group::{PIN_LOAD_IDLE_MS, pin_counts_as_load};
use super::*;

#[cfg(test)]
mod select_test_support;

#[cfg(test)]
mod divert_verdict_tests;

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
mod revalidation_sticky_tests;

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
mod sticky_divert_replay_tests;

/// Two gates on `select_with_group`'s strict-group arms, both of
/// which had ZERO coverage: reverting either hunk left the whole suite green.
///
/// A lease's scope is expressed as exactly this ask
/// ([`crate::peer::lease::ScopeRestriction::Group`]), so every door out of a
/// strict group is an account outside the scope serving a borrowed request on
/// somebody else's Mac.
#[cfg(test)]
mod strict_group_door_tests;

/// Reserved-group semantics test #5:
/// `eligible` and `account_gate` must AGREE on the RESERVED gate over many
/// account/reserved-set/ask combinations — a property test over the two
/// predicates, not two example tests that can drift apart the way
/// `gate_and_hard_ok_agree_on_every_variant` (`manager/mod.rs`) already
/// guards the OTHER eight `GateReason` variants. Pure-predicate, so it needs
/// no `Manager`/config plumbing — just [`AccountRuntime`] and the same
/// `reserved_blocks` these two predicates both call.
#[cfg(test)]
mod reserved_gate_agreement_tests;

/// Group-miss classification (`Manager::classify_group_miss`): the log line at
/// the soft fallback must name the constraint that was actually in play.
///
/// The arm that earns the module is [`GroupMiss::OnlyControl`]. Before this
/// existed, a group whose only member was the control account fell back on every
/// single request and the log said "under pacing" — on a fleet where pacing was
/// unconfigured. These tests pin each cause to its own answer so the message can
/// never again describe a constraint that was switched off.
#[cfg(test)]
mod group_miss_tests;

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
mod reset_urgency_tests;

/// The Fable weekly last-resort gate (`docs/plans/fable-gate-bridge-coder.md`):
/// on a real fleet the model-scoped weekly bucket (`7d_oi`) rests at 0.95-0.98
/// against a `switchThreshold` of 0.95, so every pooled account hard-failed
/// `model_blocked` and a Fable request 429'd even though upstream had never
/// once rejected on that bucket. These drive the REAL `select()` (not the
/// pure predicates) across the three tiers the fix adds: predicted-near is
/// still eligible normally, predicted-over-but-not-rejected is skipped on the
/// first two passes and usable as a last resort, and genuinely-rejected
/// (`>= 1.0`, the only value [`crate::records::quota::Quota::reject_model_weekly`]
/// writes) is never picked.
#[cfg(test)]
mod fable_last_resort_tests;

#[cfg(test)]
mod stale_pin_load_tests;

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
mod provider_filter_tests;

#[cfg(test)]
mod routing_candidates_tests;
