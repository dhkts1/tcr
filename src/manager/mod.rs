//! Account rotation, token freshness, and the live state the proxy/TUI share.
//!
//! Selection order:
//!   1. lowest `priority` value wins (operator-controlled; default 0);
//!   2. within a priority tier, the account in the soonest reset-urgency BUCKET
//!      goes next — the governing weekly reset floored into
//!      `resetUrgencyTierHours`-wide buckets (default 24h; `0` disables the
//!      term). Unused weekly quota is worth nothing once its window resets, so
//!      the fleet spends the account whose headroom expires soonest first. A
//!      window with no known reset sorts ahead of every bucket ("probe it").
//!   3. within a bucket, the least-recently-*selected* account goes next, so
//!      consecutive requests fan out across the fleet instead of hammering one
//!      account. (A single request barely moves a weekly bar, so ordering by
//!      quota headroom would pin one account until its bar caught up — and
//!      ordering by the RAW reset instant would do the same, which is why step 2
//!      buckets rather than compares directly.) A never-selected account sorts
//!      first; the raw weekly reset is the cold-start tiebreak below that.
//!
//! Eligibility = not disabled, not in an active rate-limit hold, not in a hard
//! `error` state, and under its threshold (per-account `switchThreshold` else the
//! global one), all evaluated **live** against `now`.
//!
//! Token refresh is **coalesced per account**: a `tokio::sync::Mutex` per account
//! plus a re-check under the guard means N concurrent requests on the same
//! hard-expired account trigger exactly one upstream refresh.
//!
//! Usage/request counters increment on the **actual serving index** passed by the
//! proxy, never a mutated "current" pointer — that is bug #3 designed out.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use time::OffsetDateTime;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::config::{self, Config, PacingConfig, ThrottleConfig};
use crate::oauth::{self, LiveRefresher, TokenRefresher, Tokens};
use crate::probe::{LiveUsageProber, PlanProber, ProbeStatus, Usage, UsageProber};
use crate::quota::Quota;
use crate::stats::{
    AccountSnapshot, GateReason, RequestLogEntry, SessionKind, SessionSnapshot, StatsSnapshot,
};
use crate::warmer::{AccountWarmer, LiveWarmer};

mod groups;
use groups::GroupPolicy;
// `Manager::lendable_fraction` lives in `peer_lend`, inside `crate::manager`, so it reaches
// `src/manager/select/mod.rs`'s `pub(super) fn effective_threshold` without widening it. This
// declaration is the one line that tripwire permits, because a module that is not declared does
// not exist. See `peer_lend.rs`'s own doc-comment.
mod peer_lend;
// The one TYPE that module owns and the lease layer names: what a hand grant
// hands over, chosen as one value so a bearer can never be paired with another
// account's expiry or another account's baseline. The module itself stays
// private, so this re-export is the whole of its surface beyond the methods on
// `Manager`.
pub use peer_lend::HandedCredential;
pub mod auto_reset;
mod claude_reset;
mod codex_reset;
mod select;
mod snapshot;
mod state;

// Split out of this file, one concern each, and grouped by concern into the directories
// below. The imports keep every moved name reachable here (siblings and `tests` read this
// module through `use super::*`, the grouped files through `use crate::manager::*`), and the
// `pub use` lines keep the `crate::manager::X` paths that `proxy.rs`, `tui.rs`, `demo.rs` and
// `tests/` name.
mod accounts;
mod construction;
mod freshness;
mod serving;
mod sessions;
pub(crate) use accounts::account_runtime::ResetTarget;
use accounts::account_runtime::match_listed;
pub use accounts::account_runtime::{AccountRuntime, AccountStatus};
pub use accounts::add_account::{AddAccountOutcome, AddPersist};
pub use accounts::control_account::{
    CODEX_HAS_NO_CONTROL_ACCOUNT, ControlPersist, SetControlOutcome, codex_control_refusal,
};
pub use accounts::disable_and_priority::{DisablePersist, SetDisabledOutcome, SetPriorityOutcome};
pub use claude_reset::{ClaudeResetError, ClaudeResetOutcome, ClaudeResetResponse};
pub use codex_reset::{CodexResetError, CodexResetOutcome, CodexResetResponse, valid_reset_key};
use serving::codex_traffic::{CodexRefusals, MinuteRing};
pub use serving::exhaustion_hint::ExhaustionHint;
pub use serving::in_flight::InFlightGuard;
use serving::in_flight::WarmInFlightGuard;
pub(crate) use serving::serving_client::build_serving_client;
#[cfg(test)]
pub(crate) use serving::serving_client::fail_next_client_build;
use serving::serving_client::{MAX_SERVES_PER_CONNECTION, RecycleReason};
use sessions::capped_map::{CappedMap, CappedSet};
// The one TYPE that module owns and `proxy.rs` names: one minted token as the response path
// hands it over, so a kind can never be paired with another token's string.
pub use sessions::bound_tokens::MintedToken;

// Re-exported so unit F's replay harness (`tests/`) can call the SAME
// production predicate rather than a reimplementation — see
// the divert-budget design notes §6. Import path: `tcr::manager::{divert_verdict, DivertVerdict}`.
pub use select::{DivertVerdict, PoolPass, RoutingCandidates, divert_verdict};

/// How long the session-affinity flusher waits after a change before it saves, so a burst of
/// changes is one write. It is also the most of a new or moved pin a SIGKILL can lose: a
/// clean shutdown writes the pins whatever happened before it.
pub const AFFINITY_SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest a pin's touch time waits to be written when nothing else about the pins moved.
/// Every request moves its pin's touch time, so writing on each one rewrote the whole file
/// every [`AFFINITY_SAVE_DELAY`] for as long as any session was busy. A touch time is read at
/// boot only to expire a pin older than `affinity::PIN_TTL_MS` (15 minutes), so one up to a
/// minute stale expires a pin at most a minute early, for a session that had already been
/// quiet for fourteen.
pub const AFFINITY_TOUCH_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);

/// As [`AFFINITY_SAVE_DELAY`], for the Sessions/Tools cache: how soon a new session reaches it.
pub const WIRE_SESSIONS_SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// As [`AFFINITY_TOUCH_REFRESH`], for the Sessions/Tools cache: how stale a session's
/// counters, times and tool calls can be on disk. The cache only fills the panel after a
/// restart, so what a SIGKILL loses is at most this much of its history.
pub const WIRE_SESSIONS_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);

/// How long the bound-token flusher waits after a change before it saves: a checkpoint, not a
/// debounce. The file holds up to `BOUND_TOKEN_CAP` tokens, several megabytes, and gains one on
/// nearly every assistant turn, so a minute's cadence rewrote all of it once or twice a minute
/// for as long as anyone was working: about 8 MB each time on a live proxy. A clean shutdown
/// writes the map whatever the cadence; what this bounds is a crash or SIGKILL, which loses at
/// most this long of newly minted tokens, whose conversations then route as they did before
/// this map existed.
pub const BOUND_TOKENS_SAVE_DELAY: std::time::Duration = std::time::Duration::from_mins(10);

const REQUEST_LOG_CAPACITY: usize = 200;

/// Upper bound on a single rate-limit hold. A 429 `retry-after` larger than this
/// is clamped so an account is always revalidated within the window rather than
/// pinned out for hours with no live request to clear the hold (finding #5).
const MAX_RATE_LIMIT_HOLD_SECONDS: i64 = 3600;

/// How long an upstream 403 holds the refused account out of rotation. Short on
/// purpose: a 403 can be about one model or one request, not the whole account,
/// so it must not bench a healthy account for long. Without any hold, though,
/// least-recently-selected rotation brings the account straight back and every
/// next request pays the same refused round trip. Longer than the exhaustion
/// soft-wait ceiling (under 60 s in proxy.rs), so a fleet that is refused
/// everywhere is not waited on as if it were a brief park. Kept clearly under
/// [`CACHE_WARM_HOLD_SECS`], so a session pinned to the account keeps its pin
/// and diverts request by request (`Manager::hold_clears_while_warm`) rather
/// than being re-keyed over what may be one refused request; a value at the
/// boundary would flip between the two a few milliseconds after arming.
pub(crate) const REFUSED_HOLD_SECONDS: i64 = 120;

/// Why [`AccountRuntime::rate_limited_until_ms`] is armed. Both causes hold the
/// account out of selection the same way; the cause only decides what the
/// account's gate reads ([`GateReason::Hold`] or [`GateReason::Refused`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum HoldCause {
    /// A 429 rate-limit hold ([`Manager::mark_rate_limited`]).
    #[default]
    RateLimit,
    /// An upstream 403 refusing our credential ([`Manager::mark_refused`]).
    Refused,
}

/// The dividing line between a rate-limit hold a session should WAIT OUT on its
/// pinned account and one long enough that re-keying is the cheaper trade:
/// **Anthropic's default ephemeral prompt-cache TTL, 5 minutes.**
///
/// A hold is a TIMER, not a death, and the timers we arm are mostly short — a
/// no-guidance transient 429 parks `NO_GUIDANCE_HOLD_SECS` (15s) plus jitter, and
/// a `retry-after` park is clamped to 300s ([`crate::proxy`]). Discarding a
/// per-account prompt cache that will still be warm 15 seconds later, and never
/// returning to it, is the same cache-loss defect the soft gates already fixed,
/// reached through a different signal.
///
/// So a hold with LESS than this remaining is SOFT — divert this ONE request and
/// keep the pin, and the session comes home warm. A hold with this much or MORE
/// remaining stays HARD: the old cache is dead by the time the account frees, and
/// re-keying settles the session on one account that then warms up, whereas
/// holding a long-dead pin would divert through the LRU pick on every turn and
/// scatter the conversation cold across the fleet.
///
/// 300 is the CONSERVATIVE choice, and the one a session gets when nothing is known
/// about its prefix. A session whose latest request asked for Anthropic's 1-hour
/// extended cache gets [`WarmWindow::EXTENDED`] instead, through
/// [`Manager::warm_window`]: until 2026-10-06 every session was weighed against
/// this constant, because a hold alone cannot tell which TTL a prefix was written
/// under, and a 1h session was moved off a 6-minute hold with 54 minutes of cache
/// left. Erring low costs at most one extra diverted request on a hold that would
/// in fact have come home warm; erring high costs a whole conversation prefix.
const CACHE_WARM_HOLD_SECS: i64 = 300;

/// How long one session's prompt cache stays warm after its last request: the one
/// value every hold-or-move decision reads (the hard gate's "does this hold outlive
/// the cache", the soft divert's "does it clear while warm", and the idle-cold
/// re-key). Built once per request by [`Manager::warm_window`] from what that
/// session's latest request asked of `cache_control` (see [`crate::cache_ttl`]),
/// and passed down rather than re-derived, so no two decisions can disagree about
/// the same session.
///
/// Two values, no third: [`Self::DEFAULT`] is [`CACHE_WARM_HOLD_SECS`], the 5-minute
/// window; [`Self::EXTENDED`] is [`crate::affinity::EXTENDED_PIN_TTL_MS`], the 55
/// minutes the pin store already trusts a 1h prefix for (derived there from 52,951
/// usage records). A caller with no session to ask passes `DEFAULT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WarmWindow {
    ms: i64,
}

impl WarmWindow {
    /// The 5-minute default window.
    pub const DEFAULT: Self = Self {
        ms: CACHE_WARM_HOLD_SECS * 1000,
    };
    /// The window of a session whose latest request asked for `"ttl":"1h"`.
    pub const EXTENDED: Self = Self {
        ms: crate::affinity::EXTENDED_PIN_TTL_MS,
    };

    /// The window in milliseconds.
    pub const fn ms(self) -> i64 {
        self.ms
    }

    /// The window in whole seconds, for log lines.
    pub const fn secs(self) -> i64 {
        self.ms / 1000
    }
}

/// How long a *transient* refresh failure holds off further refreshes of the
/// same account. A transient failure leaves the token unchanged, so the
/// access-token coalescing guard can't stop a follower queued on the lock from
/// re-POSTing the single-use refresh token; this short window collapses the
/// concurrent batch to one POST. Long enough to swallow a burst, short enough
/// that a later sequential retry (the account is not stuck) stays invisible.
const REFRESH_RETRY_COOLDOWN_MS: i64 = 2_000;
/// First recovery attempt for an `Error` row, and the floor the backoff starts at.
const ERROR_REPROBE_BASE_MS: i64 = 60_000;
/// Ceiling for the recovery backoff — a permanently dead credential is retried at
/// most ~twice an hour, so re-probing can never hammer the OAuth endpoint.
const ERROR_REPROBE_CAP_MS: i64 = 30 * 60_000;

/// How many probe sweeps in a row may fail to read an account's quota before
/// keep-warm stops waiting for evidence and proceeds without it — the escape valve
/// on [`Manager::warm_targets`]'s boot gate.
///
/// The gate waits because a blank quota is *unknown*, not *known-cold*, and
/// warming an account whose 5h window is already live is wasted spend. But absence
/// of evidence is not evidence of a live window, and if the probe fails
/// persistently the wait never ends: keep-warm goes silently dark while config and
/// TUI both read as enabled. That is the worse failure — an unbounded wait is a
/// kill switch, a bounded one costs a few minutes.
///
/// Three, from the probe cadence. One failed probe proves nothing: the fleet-wide
/// false error `probing.rs` documents (a bursted sweep 429ing itself) is exactly a
/// one-sweep event, and lifting on it would hand the boot burst straight back.
///
/// **What the three count, since the probe went per-account.** The counter always
/// lived on the account and was always bumped per PROBE by
/// [`Manager::record_probe`]; when the background refresh was a fleet sweep, one
/// sweep was one probe per account, so "three sweeps" and "three of this
/// account's probes" were the same number. Only the first phrasing has stopped
/// being true. The semantics are deliberately unchanged: three consecutive failed
/// probes *of this account*, which is still exactly the "a probe failing once is a
/// hiccup, three times is a condition" test it was chosen for — and it is now
/// genuinely per-account, so one account with a dead credential no longer has its
/// failures interleaved with a fleet-wide notion of a sweep.
///
/// What DID change is the wall clock, and only because the cadence moved. Three
/// failures are three of this account's own randomly drawn intervals, so at the
/// default [`crate::probe::DEFAULT_PROBE_SECONDS`] (300s, drawn `+/-30%`) the
/// bound is 10.5-19.5 minutes rather than the old ~2.5. That is longer but still
/// bounded, still a fraction of any sane `warmupSeconds` (3600s is the usual
/// value), and it is only ever paid when the probe is genuinely broken — a
/// working probe latches `quota_known` on its first success and never reaches
/// here at all.
const PROBE_FAILURES_BEFORE_WARMING_UNPROBED: u32 = 3;

/// Bound on consecutive keep-warm requests that succeeded (200) but carried NONE
/// of the `anthropic-ratelimit-unified-5h-*` headers — a response that latches no
/// evidence (see [`Manager::update_quota`]'s doc-comment: "a response WITHOUT the
/// header latches nothing"). Left unbounded, such an account would stay a
/// [`Manager::warm_targets`] member forever — its 5h window never gets folded, so
/// `live_reset` never fires — and get re-warmed every cadence at real upstream
/// cost for nothing.
///
/// Mirrors [`PROBE_FAILURES_BEFORE_WARMING_UNPROBED`] in shape (three consecutive
/// misses is a hiccup, not a verdict) but bounds the opposite direction: that one
/// bounds a WAIT before warming starts; this one bounds a REPEAT once warming
/// itself stops producing evidence.
///
/// Crossing this excludes the account from [`Manager::warm_targets`] — but that
/// exclusion must itself be recoverable without a restart, or a transient
/// upstream condition that strips the 5h headers for a few minutes sidelines the
/// account for the life of the process. [`AccountRuntime::warm_evidence_retry_after_ms`]
/// is that recovery: a flat cooldown, not a per-response reset — an EARLIER
/// version of this fix reset the counter on every successful background probe,
/// which reopened exactly the loop this constant bounds (`probeable_indices`
/// probes an excluded account regardless of `warm_targets` membership, so a
/// probe-triggered reset re-armed a full burst of [`WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT`]
/// warms every probe cycle — worse than the original unbounded-warm bug).
const WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT: u32 = 3;

/// Flat cooldown before a keep-warm-excluded account (see
/// [`WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT`]) is retried. An hour: two orders of
/// magnitude cheaper than retrying every probe cycle (probe cadence defaults to
/// minutes, not hours — see [`crate::probe::DEFAULT_PROBE_SECONDS`]) while still
/// recovering well inside a restart's cost. Deliberately a flat wait, not
/// exponential backoff like [`AccountRuntime::error_backoff_ms`]: a header-less
/// warm response is not evidence the account is unhealthy (unlike a rejected
/// refresh token), so there is no reason to believe the NEXT retry is less
/// likely to work than this one, and a flat interval keeps the recovery time
/// bound simple to state.
const WARM_EVIDENCE_RETRY_COOLDOWN_MS: i64 = 3_600_000;

/// Decay window for [`AccountRuntime::stream_error_times_ms`] — how far back a
/// stream error still counts toward the operator-facing decayed count. An hour:
/// long enough to show a persistent pattern, short enough that a stale blip from
/// hours ago does not linger in the TUI forever.
const STREAM_ERROR_WINDOW_MS: i64 = 3_600_000;

/// Hard cap on [`AccountRuntime::stream_error_times_ms`] so a pathological
/// account (erroring on every request, indefinitely) cannot grow the queue
/// without bound even inside the decay window.
const STREAM_ERROR_CAP: usize = 64;

/// Why [`Manager::persist_setting`] saved nothing. Both arms mean the setting
/// is nowhere: unlike a disable, there is no in-memory half that took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingPersistError {
    /// No config file behind this manager (`tcr demo`, tests).
    NoConfigFile,
    /// Reading or writing the file failed; the error, in words.
    WriteFailed(String),
}

/// Per-session serving stats, keyed on the stable session key. Independent of
/// `affinity` (which holds only the current pin) so routing is unaffected.
struct SessionStat {
    account_idx: usize,
    requests: u64,
    last_seen_ms: i64,
    /// [`SessionKind::Stable`] when this session was keyed on a stable client identity
    /// (x-api-key / `metadata.user_id`); [`SessionKind::Prefix`] when there was none
    /// but a `system`/`tools` hash pinned it anyway; [`SessionKind::Fallback`] when
    /// there was neither and the request served unpinned. DISPLAY provenance
    /// only — never routing.
    kind: SessionKind,
}

/// One session's divert fan-out inside ONE hold episode. Keyed on the EPISODE,
/// not on wall-clock: a session that diverts three times across three separate
/// holds hours apart is healthy and must start each one with a full budget.
/// See the divert-budget design notes §4.1.
/// `pub` (not `pub(super)`) and every field `pub`, deliberately: unit F's
/// replay harness (the divert-budget design notes §6) reconstructs episodes
/// from retained logs and must be able to build a `DivertEpisode` directly to
/// feed [`select::divert_verdict`], the SAME predicate production calls —
/// "not a reimplementation" is the whole point of the gate. See that
/// function's re-export at the bottom of this file for the exact import path.
#[derive(Debug, Clone, Copy)]
pub struct DivertEpisode {
    /// The pinned account the episode is about.
    pub pin: usize,
    /// The pin's `rate_limited_until_ms` at the time of the first divert. This
    /// value IS the episode identity — every divert inside one hold observes the
    /// same deadline, and a new hold necessarily carries a different one. A
    /// mismatch against the live value on a later divert (different pin, or the
    /// same pin with a different `until_ms`) means the ledger entry is stale and
    /// is overwritten from scratch — no timer, no sweeper.
    ///
    /// **Considered and accepted: two genuinely distinct holds on the same pin
    /// CAN collide on this value** (a second 429 arriving with the identical
    /// millisecond deadline as the first, or two holds both clamped to the
    /// same `MAX_RATE_LIMIT_HOLD_SECONDS` ceiling). When that happens the mask
    /// does NOT reset — the two holds are treated as one episode. This is
    /// deliberately left as-is rather than defended against: a same-deadline
    /// collision means the session is still inside a hold window with the
    /// identical clear-instant in every way that matters to the sticky
    /// policy, so reusing the prior destination is still the right call, not
    /// a bug. No counter or nonce was added to disambiguate same-`until_ms`
    /// holds — that would widen a persisted-adjacent structure to defend
    /// against a hypothetical with no observed real case.
    pub until_ms: i64,
    /// Bitmask of destination account indices already served for this session in
    /// this episode. `count_ones()` is the distinct-destination count; capped at
    /// 64 accounts, which the fleet is nowhere near.
    pub destinations: u64,
    /// The first destination, so the sticky overlay has a single preferred index
    /// without scanning the mask.
    pub sticky: usize,
}

/// Owns all rotation state and the machinery to refresh tokens and reach upstream.
pub struct Manager {
    /// This manager's own `Arc`, so a method holding `&self` can hand work to a
    /// task that OUTLIVES its caller. Exactly one such user today: a token refresh
    /// must not die with the request that asked for it (see
    /// [`Self::ensure_fresh_inner`]) — the refresh token is already spent upstream
    /// by then, and losing the answer condemns a healthy account.
    ///
    /// `Weak`, so the manager's own field cannot keep it alive; `OnceLock`, set by
    /// [`Self::assemble`] the moment the `Arc` exists. Unset means "built some other
    /// way": the refresh then runs inline, exactly as it did before.
    me: std::sync::OnceLock<std::sync::Weak<Self>>,
    /// An `Arc` so an [`InFlightGuard`] can hold the account table it decrements
    /// without holding the whole manager.
    accounts: Arc<RwLock<Vec<AccountRuntime>>>,
    /// Lazily allocated gates for append-only account slots. The registry lock
    /// is released before awaiting a gate; unrelated accounts never queue together.
    usage_gates: Mutex<HashMap<usize, Arc<AsyncMutex<()>>>>,
    refresher: Arc<dyn TokenRefresher>,
    /// Reads each account's quota from the zero-spend usage endpoint on a timer.
    prober: Arc<dyn UsageProber>,
    /// Reads an account's PLAN from the profile endpoint, once, the first time
    /// the probe loop meets a row that has none — see
    /// [`Self::backfill_plan_if_unknown`].
    ///
    /// Swappable rather than a constructor argument: every one of the fifteen
    /// `Manager::new` call sites would otherwise grow a parameter for a seam
    /// only the plan backfill's own tests use. `RwLock` because the swap
    /// happens once, before the manager serves anything, and every later access
    /// is a read.
    plan_prober: RwLock<Arc<dyn PlanProber>>,
    /// Warms idle accounts to keep their 5h window live (opt-in; see [`Self::warm_all`]).
    warmer: Arc<dyn AccountWarmer>,
    /// Ensures two keep-warm sweeps never overlap.
    warm_in_flight: AtomicBool,
    /// What [`auto_reset`] tried: every reset it attempted, so none is tried twice, and
    /// each account's last attempt, for the status wire.
    auto_resets: Mutex<auto_reset::Ledger>,
    /// Wakes the keep-warm loop the moment an account's quota is first READ, so the
    /// boot gate on [`Self::warm_targets`] costs a probe cycle rather than a full
    /// `warmupSeconds` of dark time.
    ///
    /// Without it the gate is a silent kill switch: the warm loop's ticker fires its
    /// first tick immediately, that sweep necessarily finds no targets (no quota has
    /// been read yet), and `MissedTickBehavior::Skip` puts the next tick a whole
    /// interval away — so at `warmupSeconds: 3600` a proxy restarted more often than
    /// hourly warms NOTHING, ever, while reading as enabled in config and TUI.
    ///
    /// `Notify` rather than a channel because a permit stored by `notify_one` before
    /// the loop is waiting is consumed by its next `notified()` — the wake survives
    /// the race between the first probe and the loop reaching its `select!`, and a
    /// `Notified` dropped by a losing `select!` branch hands its permit back. Fired
    /// ONLY on the false→true `quota_known` flip, which happens at most once per
    /// account per process, so it can never become a self-feeding loop of sweeps.
    warm_wake: Notify,
    /// The persisted config, kept so token refreshes can be written back with
    /// every unmodelled field intact.
    config: Mutex<Config>,
    /// Serializes the whole read-modify-write of the CONFIG FILE, and nothing else
    /// (**INV1**). Every writer — [`Self::persist_now`], [`Self::persist_tokens`],
    /// [`Self::persist_disabled`], and the two plan writers, the profile backfill
    /// (`backfill_plan_if_unknown`) and a Codex plan change (`record_codex_plan`) —
    /// holds this across its whole read, modify, `sync_all` and rename, so two
    /// persists can never interleave and a stale document can never clobber a
    /// just-rotated single-use refresh token.
    ///
    /// It exists so `self.config` no longer has to do that job (**INV2**). The
    /// config mutex is taken on the PER-CONNECTION path —
    /// [`Self::session_affinity_enabled`], from `mitm.rs`'s serve path — so holding
    /// it across an fsync let one TUI keypress stall connection setup. Splitting the
    /// two gives serialization without putting file I/O under a lock that request
    /// handling waits on.
    ///
    /// Lock order is `config_write` → `config`, never the reverse: nothing that
    /// holds `config` ever reaches for this. A `()` payload — it guards an external
    /// resource (the file), not data. Shared ownership lets async callers move the
    /// complete write, including its lock wait, onto a blocking worker.
    config_write: Arc<Mutex<()>>,
    config_path: Option<PathBuf>,
    /// Set once, by [`Self::release_mutation_ownership`], when this process has
    /// handed the listening socket to a successor and is only draining.
    ///
    /// While it is set, this `Manager` refuses to rotate a token or write the
    /// config file. Both refusals are load-bearing and for different reasons.
    ///
    /// A refresh token is SINGLE-USE, so a draining request that reaches
    /// `ensure_fresh` would rotate a token the successor is also holding, and
    /// invalidate the successor's copy — the token war `crate::singleton`
    /// exists to prevent, arriving from inside one lineage rather than between
    /// two rival proxies.
    ///
    /// A config write is worse, because it needs no request at all:
    /// `ServerHandle::shutdown_within` ends by calling [`Self::persist_now`],
    /// so an ungated predecessor writes its now-stale in-memory tokens over the
    /// successor's fresh ones as its LAST act. That is the failure
    /// `crate::singleton`'s module doc describes — boot-time tokens written
    /// back over fresh ones — through a new door.
    ///
    /// Terminal on purpose: nothing clears it. A process that has given away
    /// its socket never gets ownership back, and a flag that could be unset is
    /// a flag a future caller can unset by mistake.
    mutation_released: AtomicBool,
    /// Notices that the machine slept between two upstream sends, so
    /// [`Self::enter_in_flight`] can retire every account's pooled connection
    /// before the first post-wake send checks out one the sleep killed. See
    /// [`crate::wake`] for why the pool's own idle timers cannot.
    wake: crate::wake::WakeDetector,
    upstream: String,
    /// Where a Codex account's requests go ([`config::Config::codex_upstream`]). Read
    /// through [`Self::upstream_for`]; `upstream` above stays the Anthropic origin.
    codex_upstream: String,
    proxy_api_key: Option<String>,
    /// The request key ([`crate::request_key`]), a boot-time snapshot of
    /// `proxy.requestKey` like `proxy_api_key` above. `None` holds no key, and then
    /// nothing is counted and `requestKeyMode` is absent from the status.
    request_key: Option<String>,
    /// Callers that arrived without [`Self::request_key`]. Log mode only: counted,
    /// never refused.
    unkeyed: crate::request_key::UnkeyedCounters,
    global_threshold: f64,
    /// Override for the Fable weekly bucket alone — see
    /// [`crate::config::Config::fable_weekly_threshold`]. `None` means "use
    /// whatever threshold the site would apply anyway", resolved by
    /// [`crate::config::effective_fable_threshold`] at the two call
    /// sites ([`Self::model_blocked`], [`Self::account_gate`]'s `FableWeekly`
    /// push). Snapshotted at construction, same restart-to-take-effect contract
    /// as `global_threshold` above.
    fable_weekly_threshold: Option<f64>,
    /// The group settings (`reserved`, `parked`, `allowControlAccount`,
    /// `spillToPool`, colors) as one value. Seeded from the config at
    /// construction and **hot-reloaded** from the config file thereafter; see
    /// [`Self::reload_groups_if_changed`] for when and how. A reload swaps the
    /// whole `Arc`, and a reader clones one ([`Self::groups`]) and never holds
    /// this lock across the accounts/affinity locks, so every read in one call
    /// sees one version of the file.
    groups: RwLock<Arc<GroupPolicy>>,
    /// What this process's MITM listener can intercept by its policy and leaf,
    /// recorded once by `server::serve` from its boot-time TLS load
    /// ([`Self::record_mitm_status`]). `OnceLock` because the listener's leaf
    /// is loaded once and never reloaded (whether it terminates `chatgpt.com`
    /// also depends on the Codex accounts, counted on every CONNECT); unset
    /// means this process loaded no TLS material, which [`Self::mitm_status`]
    /// reports as MITM off.
    mitm: std::sync::OnceLock<crate::status::MitmStatus>,
    /// The fan controller's handle ([`crate::fan`]), installed once by the
    /// binary after the bind ([`Self::install_fan`]). Unset in a library
    /// caller or a test that did not ask for one: such a process runs no fan
    /// control, its status carries no `fanControl`, and `/_tcr/fan` says so.
    fan: std::sync::OnceLock<Arc<crate::fan::FanHandle>>,
    /// The Codex 429s this process sent itself in the last ten minutes, and the latest one
    /// ([`Self::note_codex_refusal`], [`Self::codex_traffic`]). A leaf lock, never held with
    /// another. In memory only: a restart empties it.
    codex_refused: Mutex<CodexRefusals>,
    /// The Codex requests relayed unpooled for their own routing in the last ten minutes
    /// ([`Self::note_codex_unpooled`]). A leaf lock, never held with another. In memory only.
    codex_unpooled: Mutex<MinuteRing>,
    /// The config file's mtime as of the last successful (or unmodified-since)
    /// [`Self::reload_groups_if_changed`] check. `None` before the first check.
    /// Read under its own mutex for the unchanged-file fast path; advanced only
    /// under `config_write` after applying a complete file snapshot. Reloads also
    /// recheck it under that write guard, so racing checks cannot advance past an
    /// edit another reload has not applied.
    groups_reload_mtime: Mutex<Option<std::time::SystemTime>>,
    /// Per-account request pacing knobs, snapshotted from the config at
    /// construction. Default (all `None`) → inert → selection is byte-identical to
    /// the no-pacing build. See [`config::PacingConfig`].
    pacing: PacingConfig,
    /// Per-ORGANIZATION outbound throttle knobs, snapshotted from config at
    /// construction (all-`None` → inert). This is the primary limiter: Anthropic
    /// sets limits per organization, so this is the bucket that matches the thing
    /// actually being limited.
    account_throttle: ThrottleConfig,
    /// Fleet-wide ceiling knobs, snapshotted from config at construction
    /// (all-`None` → inert). Insurance against a shared-identity limit nobody has
    /// measured; set far looser than [`Self::account_throttle`] so it does not bind
    /// in normal use.
    fleet_throttle: ThrottleConfig,
    /// Per-account usage buckets, pricing and the append-only ledger — the one
    /// place a served request's tokens are aggregated for display. Present on
    /// every `Manager`, but only the SERVING process attaches a ledger to it
    /// (`Self::attach_usage_ledger`), so an offline manager's tracker is empty
    /// and its rows read as "not measured" rather than as zero traffic.
    ///
    /// Lock order: this is a **leaf** lock. Nothing taken while it is held, and
    /// it is always innermost — `Self::record_usage` takes the accounts lock,
    /// drops it, and only then records here; `Self::snapshot` holds the accounts
    /// READ lock across its `usage_row` calls, which is safe precisely because
    /// no path ever runs the other way round.
    usage: crate::usage::UsageTracker,
    /// Runtime-only, cold quota fits indexed by the same stable slots as accounts.
    forecast: Mutex<Vec<crate::forecast::Calibration>>,
    /// Resolved hard-lock target (index of the account named by `config.lockAccount`),
    /// or `None` when unlocked / the name did not match. For its own provider,
    /// [`Self::select`] returns `i` (bypassing rotation/affinity/migration) with
    /// no failover; the other provider's pool remains independent.
    locked_idx: Option<usize>,
    /// The resolved identity-bound control account (index of the account named
    /// by `config.controlAccount`), or `None` when unset / the name did not
    /// match. Deliberately **not** `locked_idx`'s immutable `Option<usize>` —
    /// this one is set at runtime via `tcr control` / `POST
    /// /_tcr/accounts/control`, mirroring `set_disabled_by_query`, so it needs
    /// interior mutability. Read by [`Self::probeable_indices`] to keep usage
    /// tracking on this account even while it stays `disabled` (out of the
    /// inference rotation on purpose — see the module's `freshness/probing.rs` doc).
    /// Selection itself does NOT consult this field (part 1's invariant);
    /// routing is added on top in part 2.
    ///
    /// Lock order: taken ALONE, never nested under `accounts` or `affinity` —
    /// same discipline as `select/routing.rs:159`'s reads ahead of the accounts lock.
    control_idx: RwLock<Option<usize>>,
    /// GCRA theoretical-arrival-times (epoch ms) for the PER-ORGANIZATION throttle,
    /// keyed by [`Self::throttle_bucket_key`].
    ///
    /// One mutex over the whole map rather than a mutex per key: the guard is held
    /// only for an O(1) lookup-and-update and never across the sleep, so contention
    /// is identical to the single scalar this replaced. A map (not a `Vec` indexed
    /// by account) because the key is an ORG, and several accounts may share one.
    ///
    /// Entries are created on demand, so an account added at runtime needs no
    /// coordination at the `accounts.push` sites.
    ///
    /// Lock order: taken ALONE. [`Self::throttle_send`] releases this guard before
    /// touching [`Self::fleet_tat_ms`] — the two are never held together.
    org_tat_ms: AsyncMutex<HashMap<String, i64>>,
    /// GCRA theoretical-arrival-times (epoch ms) for each provider's fleet ceiling.
    /// Independent upstreams never spend one another's burst budget. Same discipline
    /// as [`Self::org_tat_ms`]: held only for the O(1) update, released before sleep.
    fleet_tat_ms: AsyncMutex<HashMap<crate::provider::Provider, i64>>,
    log: Mutex<VecDeque<RequestLogEntry>>,
    current: Mutex<Option<usize>>,
    /// The slot that most recently served each provider, as [`Self::current`] is for either:
    /// the dashboard marks the account serving Claude and the one serving codex at once.
    /// [`Self::snapshot`] re-counts each into a position among the rows it lists.
    ///
    /// Lock order: taken alone, after [`Self::current`] is released. `snapshot` reads it while
    /// it holds the accounts lock, so [`Self::set_current`] reads the slot's provider (the
    /// accounts lock, released) BEFORE it takes this or `current`.
    current_by_provider: Mutex<crate::stats::CurrentByProvider>,
    /// Monotonic counter handed out one tick at a time by [`Manager::select`] to
    /// stamp the account it picks, so the next select prefers a different one
    /// (load spread). Starts at 1 so 0 reads unambiguously as "never selected".
    select_seq: AtomicU64,
    /// Session affinity (default ON, opt-out via `"sessionAffinity": false`): a
    /// stable identity hash → `(account index it is pinned to, last-touch ms)`.
    /// Populated only when `sessionAffinity` is enabled and a `SessionKey`
    /// extension flows in; empty (and never consulted) otherwise, so the disabled
    /// path is provably inert. Bounded by a size cap
    /// (`AFFINITY_CAP`) + LRU-by-last-touch eviction in [`Manager::select`] — stable
    /// pins intentionally SURVIVE reconnects (that is the point of a stable key), so
    /// there is no disconnect-release. Kept a plain `std::sync::Mutex` and **never**
    /// held while the accounts lock is taken (read the pin, drop this lock, then do
    /// eligibility) so the two can never deadlock.
    affinity: Mutex<CappedMap<u64, (usize, i64)>>,
    /// Session keys whose MOST RECENT request carried Anthropic's extended
    /// `"cache_control": {"ttl": "1h"}` (see [`crate::cache_ttl`]), so
    /// [`Manager::affinity_pin_snapshot`] knows which persisted
    /// pins get [`crate::affinity::EXTENDED_PIN_TTL_MS`] instead of the
    /// default at the next restart. Deliberately a side map rather than a third
    /// tuple field on `affinity` itself: nothing on the selection path
    /// (`select/`) needs to read it, only the snapshot/restore bridge does, so
    /// widening the hot map's value type for every read site there would be
    /// blast radius this doesn't need. Written directly from the request path
    /// (`crate::proxy`, alongside where `session_key` is computed) rather than
    /// through `select()`, guarded the same way `affinity` is — never held
    /// while the accounts lock is taken. Capped at `AFFINITY_CAP` on its own,
    /// because the pin flush that prunes it to the live pins never runs when
    /// affinity is off or there is no affinity file.
    affinity_extended: Mutex<CappedSet<u64>>,
    /// The pin file's dirty flag, flusher wake-up and last write (see
    /// [`Manager::mark_affinity_dirty`] and [`crate::affinity`]). Shape: which session is pinned
    /// to which account; the touch times are written at most once per
    /// [`AFFINITY_TOUCH_REFRESH`].
    affinity_file: crate::cache_file::SavedFile,
    /// Hashed account-bound token → (account index, kind, mint ms): who minted each advisor id,
    /// thinking signature, redacted-thinking payload and message id the fleet has served. See
    /// [`crate::bound_tokens`] for the rule — a conversation carrying one of these is served by
    /// its minting account or by nobody, because no other account can read it back.
    ///
    /// Keyed by HASH, never the token itself: a thinking signature is several hundred bytes and
    /// arrives on nearly every assistant turn, so hashing at the door bounds this map at 64
    /// bytes a key and keeps Anthropic's attestations out of this process's memory.
    ///
    /// Positional like [`Self::affinity`], and for the same reason: the response path learns an
    /// index, and the request path needs an index to bench the rest of the fleet. The file half
    /// stores identities. Written from the response path, read from the request path; a plain
    /// set of independently locked parts ([`sessions::bound_tokens::ShardedBoundTokens`]), none of
    /// them **ever** held while the accounts lock is taken. Each part keeps its own mint order
    /// ([`sessions::bound_tokens::BoundTokenMap`]), so holding it full costs nothing per record.
    bound_tokens: sessions::bound_tokens::ShardedBoundTokens,
    /// The bound-token file's dirty flag, flusher wake-up and last write. Every change is shape
    /// here (a new token is what a restart needs), so the gate only skips a save whose map is
    /// already on disk.
    bound_tokens_file: crate::cache_file::SavedFile,
    /// Session keys already logged as held to their pin by bound history, so a long
    /// conversation costs one line and not one per turn. Diagnostic only: nothing routes on it.
    bound_history_logged: Mutex<HashSet<u64>>,
    /// `(session key, account index)` → when that session was told its held conversation is
    /// moving off that account, so the warning is one 429 and the client's retry moves it.
    /// Unlike [`Self::bound_history_logged`] this one routes: see
    /// [`Self::first_switch_warning`].
    bound_switch_warned: Mutex<HashMap<(u64, usize), i64>>,
    /// Per-session serving stats (session key → account/count/last-seen), for
    /// live per-session visibility in the TUI. Separate from `affinity` so the
    /// routing pin stays byte-for-byte unchanged; bounded in `record_served`.
    sessions: Mutex<CappedMap<u64, SessionStat>>,
    /// The Claude Code `session_id`-keyed table with tool-call timing (F1,
    /// `docs/design/panel-tabs.md`), on a different grain from [`Self::sessions`] above — see
    /// [`crate::session_wire`] for the tracker itself and [`crate::stats::StatsSnapshot::wire_sessions`]
    /// for where it surfaces. A separate lock from every other field here: nothing on the
    /// routing path reads it, so it is never held together with `accounts` or `affinity`.
    wire_sessions: Mutex<crate::session_wire::WireSessionTracker>,
    /// The Sessions/Tools cache's dirty flag, flusher wake-up and last write
    /// ([`crate::session_wire_persist`]). Shape: which sessions the table holds; their counters
    /// and times are written at most once per [`WIRE_SESSIONS_REFRESH`].
    wire_sessions_file: crate::cache_file::SavedFile,
    /// Monotonic session-key source handed out by [`Manager::next_session_key`],
    /// one per connection. Starts at 1 so the first key is a nonzero, unique u64.
    session_seq: AtomicU64,
    /// Anti-storm valve for over-threshold revalidation-serve: epoch-ms before which
    /// no new revalidation serve is issued. See [`Manager::select_revalidation`].
    next_revalidation_at_ms: std::sync::atomic::AtomicI64,
    /// Codex has its own revalidation budget; a Claude fallback never spends it.
    next_codex_revalidation_at_ms: std::sync::atomic::AtomicI64,
    /// Connection-scoped affinity for NOISE traffic (`/api/event_logging`,
    /// `/mcp-registry` — see [`select::classify_request`]): connection key
    /// ([`crate::proxy::SessionKey`]'s wrapped `u64`, minted per-connection by
    /// [`Manager::next_session_key`]) → `(account index, last-touch ms)`.
    ///
    /// Deliberately **separate** from [`Self::affinity`] and **never** written to
    /// `session-affinity.json` — the whole reason [`crate::proxy::SessionKey`]'s
    /// own doc-comment objects to using it as a routing key ("a pin keyed on it
    /// dies with the connection and leaves a ghost entry") does not apply here:
    /// that objection is about the PERSISTED map surviving past the connection
    /// it was minted for. This map is memory-only, LRU-capped
    /// (`CONN_AFFINITY_CAP`) with a short idle TTL (`CONN_AFFINITY_TTL_MS`), so a
    /// dead connection's entry ages out rather than persisting as a ghost.
    /// Guarded the same way as `affinity` — never held while the accounts lock
    /// is taken.
    conn_affinity: Mutex<CappedMap<u64, (usize, i64)>>,
    /// One [`DivertEpisode`] per session currently mid-hold, session key ->
    /// episode. See the divert-budget design notes §4.1/§4.2.
    ///
    /// Guarded the same way as `affinity` and `conn_affinity` — **read before any
    /// accounts lock is taken, written only after the affinity guard has
    /// dropped, never nested inside either.** Not persisted (deliberately: after
    /// a restart the pins are restored but no hold is, so every session
    /// legitimately starts a fresh episode) and bounded the same way `affinity`
    /// is — a size cap + LRU-by-last-touch eviction, keyed on `until_ms` as the
    /// episode's own last-touch.
    divert_ledger: Mutex<CappedMap<u64, DivertEpisode>>,
    /// Per-account quota headroom reserved for the control account (§3, part 2
    /// of the control-account feature) — `switchThreshold`/`global_threshold`
    /// minus this, applied ONLY when a general (non-control-preferred) pick
    /// evaluates the control account as a candidate. See
    /// [`select::effective_threshold`]. Resolved from `config.control_reserve`,
    /// clamped to `[0.0, 0.5]`.
    ///
    /// Consulted only once the control account is a CANDIDATE for a general
    /// pick, which is [`Self::control_pooled`] or a `--group` ask that opted in
    /// via `allowControlAccount`. Otherwise `select_with_group` force-adds the
    /// control index to `tried` and this is never reached — and it is inert in
    /// every case while the control account is `disabled`, because
    /// [`Self::eligible`]'s terminal gate drops it first.
    control_reserve: f64,
    /// Whether the control account takes GENERAL inference traffic, guarded by
    /// [`Self::control_reserve`] alone. Boot-time snapshot of
    /// `config.control_pooled`; default `false`, which preserves the rule that
    /// an inference request never selects the control account.
    ///
    /// Read [`crate::config::Config::control_pooled`] before changing this: the
    /// reserve is a floor over LAGGING headers, so `true` accepts that inference
    /// can burn the control account into `rejected` and cost the identity plane
    /// its anchor until the weekly window resets.
    control_pooled: bool,
    /// Boot-time snapshot of `config.control_identity` — see
    /// [`crate::control_identity`].
    control_identity: config::ControlIdentityMode,
    /// Client bearers the proxy has resolved to an identity, for the check
    /// [`Self::control_identity`] governs.
    client_identities: crate::control_identity::ClientIdentityCache,
    /// Width in MILLISECONDS of the reset-urgency bucket that rotation ranks by
    /// within a priority tier, resolved once from
    /// [`crate::config::Config::reset_urgency_tier_hours`] at construction
    /// (boot-time snapshot — an edit needs a restart). `0` disables the term,
    /// restoring the pre-tier `(priority, last_selected_seq, reset)` ordering.
    ///
    /// Held in ms rather than hours so [`select::Manager::reset_urgency_tier`]
    /// divides in the same unit the resets arrive in, with no repeated
    /// hour-conversion at the top of every selection loop.
    reset_urgency_tier_ms: i64,
    /// The upstream errors and served model ids this boot has seen, per provider, for the
    /// routing document ([`crate::routing`]). Observability only: no selection reads it.
    /// A leaf lock, never held while another is taken.
    routing: Mutex<crate::observations::Observations>,
}

const fn odt_to_ms(now: OffsetDateTime) -> i64 {
    (now.unix_timestamp_nanos() / 1_000_000) as i64
}

/// Pure GCRA slot computation for the global outbound throttle. Given the current
/// theoretical-arrival-time `tat_ms`, the arrival `now_ms`, the emission interval
/// `spacing_ms` (T) and bucket capacity `burst` (B), returns
/// `(new_tat_ms, allow_at_ms)`. The caller advances the stored TAT to `new_tat_ms`
/// and sleeps until `allow_at_ms` if it is in the future. `burst` requests admit
/// instantly after idle (`allow_at` <= now), then one per T.
fn throttle_slot(tat_ms: i64, now_ms: i64, spacing_ms: i64, burst: u32) -> (i64, i64) {
    let tau = spacing_ms * (i64::from(burst.max(1)) - 1); // burst tolerance (B-1)*T
    let base = tat_ms.max(now_ms); // can't schedule in the past
    (base + spacing_ms, base - tau) // (new TAT, earliest allowed)
}

fn ms_to_odt(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

/// Fold a u64 session key to a short, stable hex id for display (24 bits).
fn short_session_id(key: u64) -> String {
    format!("{:06x}", ((key ^ (key >> 40)) & 0xFF_FFFF))
}

/// Whether a wall-clock cooldown deadline (epoch ms) has elapsed as of `now_ms`.
/// `None` (no cooldown armed) reads as "not held back by this gate" — the same
/// shape [`Manager::probeable_indices`]'s `error_retry_after_ms` check and
/// [`Manager::warm_targets`]'s `warm_evidence_retry_after_ms` check both need,
/// factored out so the two don't drift on the `>=` vs `>` boundary.
fn cooldown_elapsed(deadline_ms: Option<i64>, now_ms: i64) -> bool {
    deadline_ms.is_some_and(|until| now_ms >= until)
}

impl Manager {
    /// Hard state of account `idx` — lets the proxy skip an account that a
    /// refresh just proved dead (`Error`) without wasting an upstream round-trip.
    pub fn account_status(&self, idx: usize) -> Option<AccountStatus> {
        self.account(idx, |a| a.status)
    }

    /// Replay-only read of the selector's hard serve gates, without spending its valve.
    #[doc(hidden)]
    pub fn replay_can_revalidate(&self, idx: usize, now: OffsetDateTime, is_fable: bool) -> bool {
        if !self.revalidation_serve_enabled() {
            return false;
        }
        let groups = self.groups();
        let warm = self.warm_window(None);
        let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
        accounts.get(idx).is_some_and(|account| {
            Self::hard_ok(
                account,
                self.global_threshold,
                self.fable_weekly_threshold,
                now,
                odt_to_ms(now),
                is_fable,
                None,
                &groups.reserved,
                &groups.parked,
                warm,
            )
        })
    }

    /// The resolved `lockAccount` name, or `None` when unlocked / the name did
    /// not match any account. `locked_idx` is fixed at construction (see
    /// [`Self::assemble`]), so this is a plain resolve against the live
    /// `accounts` vec — no lock ordering concerns beyond the read itself.
    /// Read by the boot-line log in `server.rs` so a hard lock is visible from
    /// outside the process, the same reasoning as `http1_only` on that line.
    pub fn locked_account_name(&self) -> Option<String> {
        let idx = self.locked_idx?;
        self.account(idx, |a| (!a.removed).then(|| a.name.clone()))
            .flatten()
    }

    /// Whether per-account request pacing does anything (see
    /// [`config::PacingConfig::is_active`]). Read by the boot-line log in
    /// `server.rs`, not the request path — [`Self::eligible`] reads
    /// `self.pacing` directly.
    pub const fn pacing_active(&self) -> bool {
        self.pacing.is_active()
    }

    /// Whether EITHER outbound throttle does anything (see
    /// [`config::ThrottleConfig::is_active`]). Read by the boot-line log in
    /// `server.rs`, not the request path — [`Self::throttle_send`] reads
    /// `self.account_throttle` / `self.fleet_throttle` directly.
    ///
    /// `true` when either bucket is live, because "throttling is on" is what the
    /// boot line is telling the operator; a build with only the fleet ceiling armed
    /// is still throttled.
    pub const fn throttle_active(&self) -> bool {
        self.account_throttle.is_active() || self.fleet_throttle.is_active()
    }

    /// Persist one `tcr config set` setting to the config file, for the
    /// [`crate::proxy::CONFIG_PATH`] route.
    ///
    /// The FILE only, never `self.config`: see [`crate::config_set`] for why
    /// changing the in-memory copy would switch three of these keys on halfway.
    /// What this process runs keeps being what it booted with, which is what
    /// [`Self::running_settings`] reports, and the difference is what TcrBar
    /// counts as waiting for a restart.
    ///
    /// Under `config_write`, the lock every other writer of this file in this
    /// process holds across its whole read-modify-rename (INV1 on
    /// [`Self::persist_control`]), so a token rotation landing at the same
    /// moment cannot drop this key, nor this write drop its token.
    pub fn persist_setting(
        &self,
        setting: &crate::config_set::Setting,
    ) -> Result<crate::config_set::SettingWrite, SettingPersistError> {
        let Some(path) = &self.config_path else {
            return Err(SettingPersistError::NoConfigFile);
        };
        let _writing = self
            .config_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        crate::config_set::save_setting(path, setting).map_err(|err| {
            tracing::error!(
                error = %err,
                key = %setting.key(),
                path = %path.display(),
                "failed to persist a config setting"
            );
            SettingPersistError::WriteFailed(err.to_string())
        })
    }

    /// The rotation and limit settings this process is running: its boot-time
    /// snapshot, read through the same [`crate::config_show::ConfigSettings`]
    /// constructor `tcr config show` reads the file with, so a key compares
    /// equal on both sides exactly when the running value is the saved one.
    pub fn running_settings(&self) -> crate::config_show::ConfigSettings {
        let path = self.config_path.clone().unwrap_or_default();
        let config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
        crate::config_show::ConfigSettings::from_config(&config, &path)
    }

    /// Record which account actually served the most recent request, and that it is now the
    /// account serving its provider.
    ///
    /// The slot's provider is read FIRST, which takes and releases the accounts lock, and only
    /// then are `current` and `current_by_provider` taken, one at a time. [`Self::snapshot`]
    /// holds the accounts lock while it takes both, so holding either of them here while
    /// taking the accounts lock would be the two orders a deadlock is made of.
    pub fn set_current(&self, idx: usize) {
        let provider = self.provider(idx);
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(idx);
        if let Some(provider) = provider {
            self.current_by_provider
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .set(provider, idx);
        }
    }
}

#[cfg(test)]
mod tests;
