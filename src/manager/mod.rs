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

mod bound_tokens;
mod capped_map;
use capped_map::{CappedMap, CappedSet};
mod groups;
use groups::GroupPolicy;
// The one TYPE that module owns and `proxy.rs` names: one minted token as the response path
// hands it over, so a kind can never be paired with another token's string.
pub use bound_tokens::MintedToken;
// The ONE line `src/manager/select.rs`'s zero-change tripwire explicitly
// permits: `Manager::lendable_fraction` has to live on a SIBLING of `select.rs`
// to reach its `pub(super) fn effective_threshold`, and a sibling module that
// is not declared does not exist. See `peer_lend.rs`'s own doc-comment.
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
mod pins;
mod probing;
pub use claude_reset::{ClaudeResetError, ClaudeResetOutcome, ClaudeResetResponse};
pub use codex_reset::{CodexResetError, CodexResetOutcome, CodexResetResponse, valid_reset_key};
mod refresh;
mod select;
mod snapshot;
mod state;
mod throttle;
mod usage;
mod warm;
mod wire_sessions;
mod write_through;

// Re-exported so unit F's replay harness (`tests/`) can call the SAME
// production predicate rather than a reimplementation — see
// the divert-budget design notes §6. Import path: `tcr::manager::{divert_verdict, DivertVerdict}`.
pub use select::{
    DivertVerdict, PoolPass, RoutingCandidates, divert_verdict, effective_fable_threshold,
};

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

/// Hard state of an account.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccountStatus {
    /// Healthy and in rotation.
    Active,
    /// Temporarily held out by a 429 (until `rate_limited_until_ms`).
    Throttled,
    /// Dead credential — sidelined until a config reload / re-login.
    Error,
}

impl AccountStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Throttled => "throttled",
            Self::Error => "error",
        }
    }
}

/// The mutable runtime state for one account (credentials + learned quota +
/// counters). Kept separate from the persisted [`config::Account`].
#[derive(Debug, Clone)]
pub struct AccountRuntime {
    pub name: String,
    pub account_type: config::AccountType,
    /// Which upstream this credential belongs to; copied from `config::Account::provider` at
    /// boot and never changed afterwards. Selection filters on it FIRST (`select::eligible`):
    /// a Claude request and a Codex account have nothing to say to each other.
    pub provider: crate::provider::Provider,
    /// The pooled account's own UUID, injected into the outbound request body's
    /// `metadata.user_id.account_uuid` so it matches the token we serve with
    /// (see [`crate::account_uuid`]). `None` leaves the body unchanged.
    pub account_uuid: Option<String>,
    /// The org this pooled account is scoped to (UUID and name). Part of the
    /// account's identity so token rotation persists to the RIGHT config entry
    /// when the same email is logged into two orgs (finding #9).
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub priority: i64,
    pub disabled: bool,
    /// The config file no longer holds this account: it was deleted (`tcr
    /// remove`, TcrBar's "Delete Account…", or a hand edit) while this process
    /// ran. Set and cleared ONLY by [`Manager::reload_groups_if_changed`], which
    /// compares the rotation with the file every time it re-reads it.
    ///
    /// A removed row keeps its slot, because a rotation index is held all over
    /// this process (pins, the control index, sessions, in-flight guards) and
    /// nothing may shift under them. It is parked (`disabled`), left out of
    /// every snapshot (so `tcr status` and the panel stop listing it), never
    /// the control or locked account, and invisible to name resolution. Before
    /// this existed the row stayed listed until a restart, so a delete of a
    /// row that was already parked changed nothing anyone could see.
    pub removed: bool,
    /// Mirrors [`config::Account::groups`], normalized to an owned `Vec` (empty
    /// when the config carried no `groups` key) so [`Manager::eligible`] can test
    /// membership without an `Option` at every call site.
    pub groups: Vec<String>,
    /// The account's plan, verbatim as the profile endpoint reported it, and
    /// the two fields that refine it — mirrors [`config::Account`]'s three, read
    /// from config at boot and BACKFILLED by the probe loop
    /// ([`Manager::probe_account`]) for a fleet that logged in before these
    /// fields existed. `None` means "never profiled", never a default plan.
    pub organization_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub seat_tier: Option<String>,
    /// Mirrors [`config::Account::plan_display_name`]: the provider's own plan name.
    pub plan_display_name: Option<String>,
    /// Runtime only: a profile read answered for this row in this process, so a row
    /// with a plan and no display name is not asked again (`backfill_plan_if_unknown`).
    pub plan_display_name_asked: bool,
    pub switch_threshold: Option<f64>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub status: AccountStatus,
    pub quota: Quota,
    /// Latched `true` the first time this account's 5h window was actually READ —
    /// "we have evidence about this account's window", which is the question
    /// [`Manager::warm_targets`]' boot gate is really asking. Never cleared: once
    /// we have read it we have read it, and a later probe FAILURE deliberately
    /// leaves the last-learned windows in place rather than blanking them.
    ///
    /// TWO sources latch it, because the evidence genuinely has two sources:
    ///  - [`Manager::apply_usage`], on a SUCCESSFUL probe of the usage endpoint —
    ///    including one that reports no 5h bucket at all, which is itself the
    ///    positive fact "this account has no live window";
    ///  - [`Manager::update_quota`], when a served response's rate-limit headers
    ///    carry the unified 5h window. Those headers are first-hand evidence about
    ///    that account, so an account carrying live traffic must not read as
    ///    "quota unknown" — it plainly is not.
    ///
    /// A response with NO 5h header latches nothing: that is silence, not the
    /// endpoint telling us there is no window.
    ///
    /// This, and NOT `probe_status`, is the gate. `record_probe` stamps
    /// `Error`/`Timeout`/`RateLimited` on a FAILED probe too, so
    /// `probe_status != Never` while `quota` is still `Quota::default()` — and a
    /// gate keyed on that lifts on blank quota, which is the boot burst coming
    /// straight back (a real fleet-wide false-error sweep is documented in
    /// `probing.rs`). It is equally NOT `quota.five_hour.is_some()`: `apply_bucket`
    /// early-returns when the endpoint omits the bucket, so an account whose
    /// responses never carry a 5h bucket would become permanently warm-INELIGIBLE —
    /// a dark feature that reads as enabled.
    pub quota_known: bool,
    /// Probes of THIS account that have failed CONSECUTIVELY without reading its
    /// quota. Bumped by [`Manager::record_probe`] on every terminal failure and
    /// reset to `0` by a success.
    ///
    /// It exists to bound the wait: `quota_known` alone can never latch when the
    /// probe fails forever, so gating on it unconditionally makes keep-warm
    /// structurally dark. See [`PROBE_FAILURES_BEFORE_WARMING_UNPROBED`].
    pub consecutive_probe_failures: u32,
    /// Consecutive 5xx probe reads, counted separately from
    /// `consecutive_probe_failures` above: a 429 self-clears and must reset
    /// this to `0` the same as a success does, so it is never conflated with a
    /// real outage. Bumped and read by [`Manager::probe_account`] via its
    /// private `note_5xx_probe` helper to escalate `ProbeStatus::RateLimited`
    /// to the visible `ProbeStatus::UpstreamDown` once a run crosses
    /// `SUSTAINED_5XX_THRESHOLD` (`manager/probing.rs`).
    pub consecutive_5xx_probes: u32,
    /// Consecutive keep-warm requests ([`Manager::warm_account`]) that succeeded
    /// but carried none of the unified 5h rate-limit headers — a 200 that latched
    /// no evidence. Reset to `0` only by a response (warm or served) whose
    /// headers DO carry the 5h window — see [`Manager::update_quota`].
    /// Deliberately NOT reset by a successful background probe
    /// ([`Manager::apply_usage`]): see that function's doc-comment for why a
    /// probe read is not the evidence this counter is bounding. Bounds
    /// [`Manager::warm_targets`]'s otherwise-unbounded repeat; see
    /// [`WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT`]. The exclusion this drives is
    /// recovered by [`Self::warm_evidence_retry_after_ms`] instead, once its
    /// cooldown elapses.
    pub consecutive_warms_without_evidence: u32,
    /// Set by [`Manager::record_warm_without_evidence`] once
    /// `consecutive_warms_without_evidence` reaches
    /// [`WARM_ATTEMPTS_WITHOUT_EVIDENCE_LIMIT`]: the wall-clock instant (epoch
    /// ms) at or after which [`Manager::warm_targets`] treats this account as
    /// eligible again despite the counter still being at or over the limit —
    /// one bounded retry per [`WARM_EVIDENCE_RETRY_COOLDOWN_MS`], not one per
    /// probe cycle. `None` when the account is not currently excluded, or once
    /// a header-bearing response clears it (see [`Manager::update_quota`]).
    pub warm_evidence_retry_after_ms: Option<i64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache-read input tokens (a SUBSET of `input_tokens`, not additional quota).
    /// Accumulated so the prompt-cache hit ratio (`cache_read / input_tokens`) is
    /// visible per account; `> 0` on post-first turns means the cache is warm.
    pub cache_read_tokens: u64,
    /// Cache-creation input tokens (also a subset of `input_tokens`).
    pub cache_creation_tokens: u64,
    pub requests: u64,
    pub last_used_ms: Option<i64>,
    /// Monotonic tick of the last time [`Manager::select`] *chose* this account
    /// (0 = never chosen). Ordering rotation by this — not by a wall-clock stamp —
    /// makes the spread clock-granularity-independent: a burst of selects in the
    /// same millisecond still fans out, because each pick takes the next tick.
    pub last_selected_seq: u64,
    /// Requests currently being served on this account. Incremented when the proxy
    /// picks this account to forward a request and decremented on completion via
    /// the RAII [`InFlightGuard`] (every drop path — success, rotate, error, panic).
    /// Mutated ONLY under the accounts write-lock (no second lock). Read by
    /// [`Manager::eligible`] to skip an account at/over the pacing concurrency cap.
    pub in_flight: u32,
    /// Wall-clock ms of the last time [`Manager::select`] chose this account to
    /// serve (0 = never). Read by [`Manager::eligible`] to enforce the pacing
    /// min-spacing between two selects of the SAME account.
    pub last_served_ms: i64,
    pub rate_limited_until_ms: Option<i64>,
    /// What armed `rate_limited_until_ms`; read only to name the gate.
    pub hold_cause: HoldCause,
    /// Why a Codex row's login is known dead: the OAuth code its last refresh was rejected with
    /// (`refresh_token_reused`, `invalid_grant`, …), else `http_<status>`. Set by a rejected
    /// refresh (`manager/refresh.rs`), cleared by a successful refresh or a re-login of the row
    /// ([`Manager::add_or_update_account`]). Always `None` on an Anthropic row. Not persisted:
    /// a restart forgets it, and the next refresh learns it again.
    pub login_error: Option<String>,
    /// [`config::Account::codex_home`]: the imported login's folder, for guarded token sharing
    /// and the status wire. A re-login replaces it with the submission's.
    pub codex_home: Option<String>,
    /// Latest shared tokens awaiting guarded auth.json publication; never part of status.
    pub(crate) pending_codex_writeback: Option<refresh::PendingCodexWriteback>,
    /// A short self-clearing cooldown after a *transient* refresh failure. A
    /// transient failure leaves the access token UNCHANGED, so the access-token
    /// guard in [`Manager::ensure_fresh_inner`] can't catch a follower already
    /// queued on the coalescing lock — it would re-POST the same single-use
    /// refresh token. This stamp (set to `now + REFRESH_RETRY_COOLDOWN_MS`)
    /// collapses the concurrent batch to one POST, yet self-clears by time so a
    /// later sequential retry still refreshes. Cleared on a successful refresh.
    pub refresh_retry_after_ms: Option<i64>,
    /// When an `Error` row may next attempt RECOVERY (a forced refresh). `Error`
    /// means "the refresh token was rejected", but a rejection can be transient (a
    /// token-war race, an upstream hiccup) — so it must not be a life sentence.
    /// `None` on a non-errored row. See [`Manager::grow_error_backoff`].
    pub error_retry_after_ms: Option<i64>,
    /// The current recovery backoff, doubled on each rejected recovery attempt and
    /// capped, so a genuinely dead credential costs ~2 refresh POSTs/hour rather
    /// than hammering the OAuth endpoint. `0` when the row is not errored.
    pub error_backoff_ms: i64,
    pub probe_status: ProbeStatus,
    pub last_probe_ms: Option<i64>,
    pub probe_error: Option<String>,
    /// The next permitted usage probe time, from an upstream `retry-after` header.
    pub probe_retry_after_ms: Option<i64>,
    /// Wall-clock ms of each stream failure observed on a stream this account
    /// served (see [`Manager::record_stream_error`]): an in-band SSE `error`
    /// event, or a stream that hit EOF without Anthropic's `message_stop`
    /// terminator — recorded with the fixed kind `"truncated"`, which covers a
    /// mid-stream transport death and the malformed/utf8 break alike. An
    /// `error` event takes precedence: EOF without `message_stop` is only
    /// counted as `"truncated"` when no more specific `error` was already
    /// recorded. Pruned to [`STREAM_ERROR_WINDOW_MS`] and hard-capped at
    /// [`STREAM_ERROR_CAP`] entries on BOTH insert and read, so it cannot grow
    /// unbounded. OBSERVABILITY ONLY — nothing in `select.rs` reads this
    /// field; see that module's gates for why.
    pub stream_error_times_ms: VecDeque<i64>,
    /// The most recent stream failure's kind: an Anthropic `error.type` (e.g.
    /// `"overloaded_error"`) from an in-band SSE `error` event, or the fixed
    /// string `"truncated"` when the stream hit EOF without `message_stop` and
    /// no more specific `error` was already recorded, alongside the decayed
    /// count above.
    pub last_stream_error: Option<String>,
    /// Coalescing gate for THIS account's OAuth refresh. Lives on the account rather
    /// than in a parallel Vec indexed by position, so an account added after startup
    /// cannot be missing one — the desync it replaces made `ensure_fresh` a silent
    /// no-op (no log, no status change) until the initial token expired.
    pub refresh_lock: Arc<AsyncMutex<()>>,
    /// This account's OWN upstream-forwarding client — never shared with any
    /// other account. `hyper-util`'s pool keys a connection on `(scheme,
    /// authority)` alone (`PoolKey`, `client/legacy/client.rs`), with nothing in
    /// that key touching the Bearer token or account identity — so a single
    /// `reqwest::Client` reused across the fleet collapses every account's
    /// traffic onto ONE pooled connection, and that connection's death (an h2
    /// `PROTOCOL_ERROR` reset, observed live spanning 2-3 distinct accounts at
    /// once) takes every account down with it, defeating rotation and failover
    /// at the connection layer no matter how healthy any individual account is.
    /// Built once per account by [`build_serving_client`] (never rebuilt on
    /// every request — that would throw away the warm pool this exists to
    /// keep) and wrapped in `Arc` so [`Manager::http_client`] can hand out
    /// cheap clones while still letting a test prove two accounts' clients are
    /// genuinely distinct instances via `Arc::ptr_eq`.
    ///
    /// REBUILT periodically, though — see [`MAX_SERVES_PER_CONNECTION`] and
    /// [`Manager::recycle_client`]. "Never on every request" is not
    /// "never": an h2 connection held open indefinitely ages into the reset
    /// zone this field's own history describes.
    pub http: Arc<reqwest::Client>,
    /// Upstream sends dispatched on the CURRENT [`Self::http`] since it was
    /// built, and the trigger for retiring it (see
    /// [`MAX_SERVES_PER_CONNECTION`]). Reset to 0 by the recycle, so it means
    /// "age of this client in serves", never a lifetime total —
    /// [`Self::requests`] is the lifetime counter and is deliberately NOT
    /// reused here: it is a display statistic incremented on a different path
    /// (`snapshot.rs`), and hanging connection lifetime off it would couple a
    /// transport decision to a stats counter that is free to change meaning.
    pub serves_since_client_build: u32,
}

// Test-only fault injection for `build_serving_client`: set via
// `fail_next_client_build`, consumed (and cleared) by the next call on this
// thread. Exists to prove — with a real panic, not a hypothetical one — two
// properties of `Manager::add_or_update_account`: its Added path builds the
// runtime with NEITHER `self.accounts` nor `config_write` held, so a panic
// here poisons neither lock — every other `.expect(...)` call site on either
// one in this module would otherwise go down with it — and its Updated path
// never reaches this function at all. See
// `add_or_update_account_added_path_does_not_poison_either_lock_on_a_client_build_panic`
// and `add_or_update_account_updated_path_never_builds_a_client`.
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_CLIENT_BUILD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm [`FAIL_NEXT_CLIENT_BUILD`] so the next call to `build_serving_client` on
/// this thread panics instead of building. Test-only.
#[cfg(test)]
pub(crate) fn fail_next_client_build() {
    FAIL_NEXT_CLIENT_BUILD.with(|f| f.set(true));
}

/// Upstream sends after which an account's HTTP client is retired and rebuilt,
/// dropping its pooled h2 connection so the next send opens a fresh one.
///
/// Anthropic's edge eventually drains a long-lived h2 connection, and the
/// settings in [`build_serving_client`] — `http2_keep_alive_while_idle` plus a
/// 300s pool idle timeout — are deliberately chosen to keep ONE connection warm
/// forever, so nothing retired it. Measured over one day's live log (2026-08-27,
/// n=2136 `Reset(_, PROTOCOL_ERROR, Remote)` transport failures), the resets are
/// concentrated by connection age, not by concurrency:
///
/// | stream id at reset | share |
/// |---|---|
/// | 1-99 (fresh connection) | 6.8% |
/// | 100-999 | 21.5% |
/// | 1000-4999 | 5.9% |
/// | 5000-9999 | 21.9% |
/// | 10000+ | 43.9% |
///
/// Median 9699, max 17449 — roughly 8700 requests multiplexed onto one socket.
/// h2 client-initiated stream ids advance by 2, so N serves reaches stream id
/// ~2N: 500 keeps a connection under ~1000, below the p25 of the observed reset
/// distribution, while still amortising the handshake over 500 requests.
///
/// Retiring a connection is CHEAP and must not be confused with rotating an
/// account. It costs one TCP+TLS handshake (~100-300ms, the cost
/// [`build_serving_client`]'s keep-alive settings exist to avoid); it does NOT
/// cost a prompt-cache miss, because Anthropic's prompt cache is keyed on the
/// account and prefix server-side and has nothing to do with which socket the
/// bytes arrived on. Account rotation is the expensive one (measured 5-10x the
/// quota of a warm serve), and it is exactly what this constant exists to make
/// rarer.
const MAX_SERVES_PER_CONNECTION: u32 = 500;

/// Why [`Manager::recycle_client`] is retiring one account's client, so its
/// log line names the cause instead of always claiming a spent serve budget.
/// A sleep retires every account at once and logs its own line
/// (`Manager::retire_every_client_after_sleep`), so it is not a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecycleReason {
    /// The client carried [`MAX_SERVES_PER_CONNECTION`] sends.
    ServeBudget,
    /// An HTTP/2 stream reset left the pooled connection open and broken.
    StreamReset,
}

/// Build one upstream-forwarding HTTP client. Called once per account (see
/// [`AccountRuntime::http`]) so every account's client is configured
/// identically and differs from every other account's only in the connection
/// pool it privately owns.
///
/// `http1_only` mirrors [`config::Config::http1_only`] — see its doc-comment
/// for why this exists and why it defaults to `false`. When `true`, every
/// stream this client opens is capped at HTTP/1.1, so the
/// `http2_keep_alive_*` settings below become inert (h1 has no PING frame to
/// send); they are left in place because they are still correct for the
/// default h2 path.
pub(crate) fn build_serving_client(http1_only: bool) -> Arc<reqwest::Client> {
    #[cfg(test)]
    assert!(
        !FAIL_NEXT_CLIENT_BUILD.with(|f| f.replace(false)),
        "build reqwest client (test-injected failure)"
    );
    // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
    // proxy — routing our upstream through an ambient proxy (e.g. one on
    // :3456) loops us back through a proxy and every request dies as
    // "upstream unreachable". Always reach Anthropic directly.
    let mut builder = crate::http_client::builder()
        .no_proxy()
        // Cap only the CONNECT phase. A blackholed route (no RST, no reply)
        // otherwise stalls the attempt until the OS TCP timeout, and with a
        // retry budget of `account_count * 2 + 4` that is many minutes of a
        // hung request. `oauth.rs` and `probe.rs` both already set one.
        //
        // DELIBERATELY NOT a total `.timeout(...)`, and do not add one: these
        // responses are long-lived SSE streams that legitimately run longer
        // than any bound worth setting, and a total timeout would truncate
        // them mid-stream. `connect_timeout` cannot — it applies only before
        // the response headers arrive, so once a stream is flowing it is out
        // of the picture.
        .connect_timeout(std::time::Duration::from_secs(10))
        // Keep the single HTTP/2 connection to Anthropic warm across
        // interactive think-time pauses. reqwest reaps idle connections
        // after 90s by default, but a coding session routinely pauses
        // longer — so the next request would pay a fresh TCP+TLS handshake
        // (~100-300ms). h2 keep-alive PINGs + a 5-min idle timeout hold the
        // connection open so a post-pause request skips the reconnect.
        .http2_keep_alive_interval(std::time::Duration::from_secs(30))
        .http2_keep_alive_while_idle(true)
        .pool_idle_timeout(std::time::Duration::from_secs(300));
    if http1_only {
        // Structural blast-radius cap: h1 does not multiplex, so a
        // connection-level fault (GOAWAY / framing error) kills at most the
        // one request on that connection instead of every stream sharing it.
        builder = builder.http1_only();
    }
    // Test builds only: a test whose upstream is a stub sends over the stubs' Unix socket instead
    // of opening a loopback port per request (`test_support::stub_upstream`, § The Unix socket).
    #[cfg(any(test, feature = "test-support"))]
    if let Some(socket) = crate::test_support::stub_upstream::client_socket() {
        builder = builder.unix_socket(socket);
    }
    // A client that cannot be built (the TLS backend failed to initialise) leaves
    // the process unable to make any upstream request, and this function has no
    // error channel: its callers are the constructor, `from_runtimes` and the
    // post-sleep rebuild.
    #[expect(
        clippy::expect_used,
        reason = "no upstream request is possible without a client, and the signature has no error channel"
    )]
    Arc::new(builder.build().expect("build reqwest client"))
}

/// A rotation slot answers a user's account query by the same field a config
/// record does, so [`Manager::set_disabled_by_query`] can run the CLI's own
/// resolution rule over the LIVE fleet.
impl crate::identity::Queryable for AccountRuntime {
    fn query_name(&self) -> &str {
        &self.name
    }
}

/// [`crate::identity::match_one`] over the rotation, blind to rows deleted
/// from the config ([`AccountRuntime::removed`]). A deleted row keeps its slot,
/// so without this a query could still park, prioritise or promote it, and a
/// re-login under the same name would read as ambiguous.
fn match_listed(accounts: &[AccountRuntime], query: &str) -> crate::identity::Match {
    let hits: Vec<usize> = crate::identity::match_accounts(accounts, query)
        .into_iter()
        .filter(|&idx| !accounts[idx].removed)
        .collect();
    match hits.as_slice() {
        [] => crate::identity::Match::None,
        [only] => crate::identity::Match::One(*only),
        many => crate::identity::Match::Ambiguous(
            many.iter().map(|&idx| accounts[idx].name.clone()).collect(),
        ),
    }
}

/// Which account a banked-reset spend names: a user's query, resolved by
/// [`match_listed`]'s rule, or a rotation slot the server chose itself
/// ([`auto_reset`]), which is never ambiguous and never a deleted row.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ResetTarget<'a> {
    Name(&'a str),
    Slot(usize),
}

impl ResetTarget<'_> {
    fn resolve(self, accounts: &[AccountRuntime]) -> crate::identity::Match {
        match self {
            Self::Name(query) => match_listed(accounts, query),
            Self::Slot(idx) if accounts.get(idx).is_some_and(|row| !row.removed) => {
                crate::identity::Match::One(idx)
            }
            Self::Slot(_) => crate::identity::Match::None,
        }
    }
}

/// What [`Manager::set_disabled_by_query`] did.
///
/// `Applied` carries the RESOLVED name plus the durable half's fate, which the
/// caller must surface — see [`DisablePersist::warning`].
///
/// The name is resolved, not echoed, so the answer says what was actually
/// parked. It is NOT a substring or case-insensitive match:
/// [`crate::identity::match_accounts`] is the account's exact name,
/// byte-for-byte and untrimmed. Do not "fix" resolution to match a looser
/// description; a widened rule is how a query silently parks an account nobody
/// named, which is exactly what the `Ambiguous` arm below exists to refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetDisabledOutcome {
    Applied {
        name: String,
        persist: DisablePersist,
    },
    /// The query named no account in the live rotation.
    NoMatch,
    /// The query named more than one — only reachable on a config hand-edited
    /// past the loader's rename migration. Listed here so the caller can tell
    /// the user which rows to go fix.
    Ambiguous(Vec<String>),
}

/// What [`Manager::set_priority_by_query`] did. Same resolution arms as
/// [`SetDisabledOutcome`], with no durable half: the CLI writes the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetPriorityOutcome {
    /// The RESOLVED name of the account whose priority is now in force.
    Applied { name: String },
    /// The query named no account in the live rotation.
    NoMatch,
    /// The query named more than one.
    Ambiguous(Vec<String>),
}

/// What [`Manager::add_or_update_account`] did.
///
/// Same shape as [`SetDisabledOutcome`] — resolve by identity against the LIVE
/// rotation, then report what happened plus the durable half's fate — but with
/// one difference `SetDisabledOutcome` never needs: a disable/enable query only
/// ever names an account that must already exist, so a miss is failure
/// (`NoMatch`). An account-add legitimately names one nobody has seen before, so
/// `Match::None` here is the SUCCESS arm (`Added`), not a rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAccountOutcome {
    /// No account in the live rotation carried this identity — appended at the
    /// END via [`Manager::add_account`]. Pins are in-memory `(session_key,
    /// account_index)`, so appending is the only insertion that cannot re-key a
    /// live session onto the wrong account. An account with no explicit
    /// `priority` is assigned `max(existing priorities) + 1` — deliberate,
    /// not incidental: it joins the back of the fleet rather than reading as
    /// 0 (the primary tier) — see the doc-comment on the Added arm inside
    /// [`Manager::add_or_update_account`].
    Added {
        idx: usize,
        name: String,
        persist: AddPersist,
    },
    /// Exactly one account already carried this identity — its credentials were
    /// replaced IN PLACE at `idx` (see [`Manager::add_or_update_account`]). Its
    /// `account_type` is always stamped to the submitted value and any identity
    /// field (`account_uuid`/`org_uuid`/`org_name`) it was missing is backfilled
    /// — never a value it already carried. Routing state — priority, disabled
    /// flag, switch threshold, learned quota, counters — is untouched, and
    /// `idx` never moves.
    Updated {
        idx: usize,
        name: String,
        persist: AddPersist,
    },
    /// The submitted identity matches more than one account in the live
    /// rotation; these are their names, so the caller can tell the operator
    /// what to disambiguate with. Never guessed.
    Ambiguous(Vec<String>),
}

impl AccountRuntime {
    fn from_config(account: &config::Account, http1_only: bool) -> Self {
        Self {
            name: account.name.clone(),
            account_type: account.account_type.clone(),
            provider: account.provider,
            account_uuid: account.account_uuid.clone(),
            org_uuid: account.org_uuid.clone(),
            org_name: account.org_name.clone(),
            priority: account.priority.unwrap_or(0),
            disabled: account.disabled.unwrap_or(false),
            removed: false,
            groups: account.groups.clone().unwrap_or_default(),
            organization_type: account.organization_type.clone(),
            rate_limit_tier: account.rate_limit_tier.clone(),
            seat_tier: account.seat_tier.clone(),
            plan_display_name: account.plan_display_name.clone(),
            plan_display_name_asked: false,
            switch_threshold: account.switch_threshold,
            access_token: account.access_token.clone(),
            refresh_token: account.refresh_token.clone(),
            // Defensive normalize: a stored value in seconds becomes ms.
            expires_at_ms: account.expires_at.map(oauth::normalize_expires_at),
            status: AccountStatus::Active,
            quota: Quota::default(),
            // Nothing restores the last known windows across a restart, so at boot
            // every account's quota is genuinely unread.
            quota_known: false,
            consecutive_probe_failures: 0,
            consecutive_5xx_probes: 0,
            consecutive_warms_without_evidence: 0,
            warm_evidence_retry_after_ms: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            requests: 0,
            last_used_ms: None,
            last_selected_seq: 0,
            in_flight: 0,
            last_served_ms: 0,
            rate_limited_until_ms: None,
            hold_cause: HoldCause::RateLimit,
            login_error: None,
            codex_home: account.codex_home.clone(),
            pending_codex_writeback: refresh::PendingCodexWriteback::from_config(account),
            refresh_retry_after_ms: None,
            error_retry_after_ms: None,
            error_backoff_ms: 0,
            probe_status: ProbeStatus::Never,
            last_probe_ms: None,
            probe_error: None,
            probe_retry_after_ms: None,
            stream_error_times_ms: VecDeque::new(),
            last_stream_error: None,
            refresh_lock: Arc::new(AsyncMutex::new(())),
            http: build_serving_client(http1_only),
            serves_since_client_build: 0,
        }
    }
}

/// What [`Manager::set_disabled`] achieved about DURABILITY — whether the bench
/// will still be there after a restart.
///
/// Returned rather than only logged because `tracing` is redirected to a log file
/// in TUI mode, so a failed write was invisible to the one person who could act on
/// it: the TUI kept rendering the account as disabled while the flag had provably
/// not reached disk. Every non-`None` [`Self::warning`] is a state where pressing
/// `d` looked like it worked and did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisablePersist {
    /// The config file carries the flag — written just now, or already correct.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`, the
    /// tests). Memory-only BY DESIGN: nothing to persist to, nothing wrong.
    NoConfigFile,
    /// The index named no account, so nothing changed in memory either.
    NoSuchAccount,
    /// Nothing on disk carries this account's identity — the entry was deleted or
    /// renamed while the proxy ran.
    NoEntry,
    /// More than one entry carries this identity; the write was refused rather
    /// than landed on a guess. See [`config::DisabledWrite::Ambiguous`].
    Ambiguous,
    /// The write itself failed (unreadable, malformed, or unwritable file).
    WriteFailed,
}

impl DisablePersist {
    /// The one line to put in front of the user, or `None` when the outcome needs
    /// no explanation. Short enough for a single terminal row and phrased so the
    /// headline (`NOT SAVED`) survives truncation on a narrow pane.
    ///
    /// `disabled` is the DIRECTION the keypress asked for, and it is required
    /// because the consequence of a failed write is its exact opposite in the two
    /// directions. Nothing was written either way, so the file still says whatever
    /// it said: after a failed **disable** the account comes back IN ROTATION,
    /// after a failed **enable** it comes back BENCHED. A single direction-blind
    /// line told the user "it returns to rotation on restart" for both — so on a
    /// failed enable they restarted on that advice and got the opposite.
    pub const fn warning(self, disabled: bool) -> Option<&'static str> {
        match self {
            // The flag is durable, or was never meant to be (demo/status have no
            // config file at all) — nothing to say.
            Self::Persisted | Self::NoConfigFile => None,
            Self::NoSuchAccount => Some("NOT SAVED: that account row no longer exists"),
            Self::NoEntry => Some(if disabled {
                "NOT SAVED: no config entry matches this account — it returns to rotation on restart"
            } else {
                "NOT SAVED: no config entry matches this account — it stays benched after a restart"
            }),
            // Says nothing about a restart, so it needs no direction — only an
            // action that actually helps.
            Self::Ambiguous => Some(
                "NOT SAVED: two config entries share this account's identity — give each its own orgUuid",
            ),
            Self::WriteFailed => Some(if disabled {
                "NOT SAVED: writing the config failed — it returns to rotation on restart"
            } else {
                "NOT SAVED: writing the config failed — it stays benched after a restart"
            }),
        }
    }

    /// [`Self::warning`] for a row of `provider`. Only the duplicate notice differs: two
    /// entries that share a Codex login are told apart by neither of them holding it twice,
    /// so a Codex row names that remedy (remove one, or re-import it under a name of its own),
    /// where a Claude row keeps its `orgUuid` remedy. Every other outcome reads the same for
    /// both providers.
    pub const fn warning_for(
        self,
        disabled: bool,
        provider: crate::provider::Provider,
    ) -> Option<&'static str> {
        match provider {
            crate::provider::Provider::Anthropic => self.warning(disabled),
            crate::provider::Provider::Codex => match self {
                Self::Ambiguous => Some(
                    "NOT SAVED: two config entries share this Codex login — remove the duplicate \
                     (tcr remove <name>), or re-import it with --name",
                ),
                _ => self.warning(disabled),
            },
        }
    }
}

/// What [`Manager::set_control_by_query`] did. Same shape as
/// [`SetDisabledOutcome`], with one difference: a `None` query is a legitimate
/// request (clear the control account), not a missing argument, so `Applied`'s
/// `name` is itself an `Option` rather than `Applied` only ever meaning "matched
/// something".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetControlOutcome {
    /// `name` is the resolved account name, or `None` when this call cleared
    /// the control account.
    Applied {
        name: Option<String>,
        persist: ControlPersist,
    },
    /// The query named no account in the live rotation.
    NoMatch,
    /// The query named more than one — only reachable on a config hand-edited
    /// past the loader's rename migration. Listed so the caller can tell the
    /// user which rows to go fix.
    Ambiguous(Vec<String>),
    /// The query named a Codex account, which cannot be the control account
    /// ([`codex_control_refusal`]); carries its name. Nothing was changed: the
    /// boot path ignores such a `controlAccount`, so accepting it here would
    /// persist a setting the next start throws away.
    CodexAccount(String),
}

/// Why a Codex account cannot be the control account, in the words both places
/// that meet one use: boot, which ignores a `controlAccount` naming one, and
/// `tcr control`, which refuses to set one. The control account is an Anthropic
/// identity (its bootstrap calls, its probe, its `controlIdentity` check), and
/// Codex has no counterpart yet.
pub const CODEX_HAS_NO_CONTROL_ACCOUNT: &str = "Codex has no control-account semantics yet";

/// `tcr control`'s refusal of the Codex account `name`, live
/// ([`SetControlOutcome::CodexAccount`]) and offline alike.
pub fn codex_control_refusal(name: &str) -> String {
    format!(
        "'{name}' is a Codex account, and {CODEX_HAS_NO_CONTROL_ACCOUNT}, so it cannot be the \
         control account."
    )
}

/// What [`Manager::set_control`] achieved about DURABILITY — whether the
/// control account will still be set (or cleared) after a restart. Same shape
/// as [`DisablePersist`] now that `controlAccount` is resolved by IDENTITY, not
/// by a bare name — see [`config::ControlWrite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPersist {
    /// The config file carries the control account (or its absence) — written
    /// just now, or already correct.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`,
    /// tests). Memory-only BY DESIGN.
    NoConfigFile,
    /// More than one on-disk entry carries this identity; the write was
    /// refused rather than landed on a guess. See
    /// [`config::ControlWrite::Ambiguous`].
    Ambiguous,
    /// The write itself failed (unreadable, malformed, or unwritable file).
    WriteFailed,
}

impl ControlPersist {
    /// The one line to put in front of the user, or `None` when the outcome
    /// needs no explanation. Mirrors [`DisablePersist::warning`]; there is no
    /// direction-dependent wording here because clearing and setting fail the
    /// same way — a failed write just leaves the file saying whatever it said.
    pub const fn warning(self) -> Option<&'static str> {
        match self {
            Self::Persisted | Self::NoConfigFile => None,
            Self::Ambiguous => Some(
                "NOT SAVED: two config entries share this account's identity — give each its own orgUuid",
            ),
            Self::WriteFailed => {
                Some("NOT SAVED: writing the config failed — this will not survive a restart")
            }
        }
    }
}

/// Why [`Manager::persist_setting`] saved nothing. Both arms mean the setting
/// is nowhere: unlike a disable, there is no in-memory half that took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingPersistError {
    /// No config file behind this manager (`tcr demo`, tests).
    NoConfigFile,
    /// Reading or writing the file failed; the error, in words.
    WriteFailed(String),
}

/// What [`Manager::add_or_update_account`] achieved about DURABILITY. Same role
/// as [`DisablePersist`], deliberately simpler: an upsert either lands or it does
/// not, and there is no "no such account" / "no config entry" arm here — a MISS
/// on the durable side is FILLED IN by [`config::save_account`], never refused
/// (see that function's doc-comment for why it differs from
/// [`config::save_disabled`] on exactly this point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddPersist {
    /// The config file carries the new or replaced credentials — written just
    /// now.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`,
    /// tests). Memory-only BY DESIGN.
    NoConfigFile,
    /// More than one entry on disk carries this identity; the write was refused
    /// rather than landed on a guess. See [`config::AccountWrite::Ambiguous`].
    Ambiguous,
    /// The write itself failed: unreadable, malformed or unwritable file, or a
    /// document whose `accounts` key exists but is not a JSON array.
    WriteFailed,
}

impl AddPersist {
    /// The one line to put in front of the operator, or `None` when the outcome
    /// needs no explanation.
    pub const fn warning(self) -> Option<&'static str> {
        match self {
            Self::Persisted | Self::NoConfigFile => None,
            Self::Ambiguous => Some(
                "NOT SAVED: another config entry shares this account's identity — give it its own orgUuid",
            ),
            Self::WriteFailed => {
                Some("NOT SAVED: writing the config failed — this will not survive a restart")
            }
        }
    }
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
    /// [`crate::manager::select::effective_fable_threshold`] at the two call
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
    /// inference rotation on purpose — see the module's `probing.rs` doc).
    /// Selection itself does NOT consult this field (part 1's invariant);
    /// routing is added on top in part 2.
    ///
    /// Lock order: taken ALONE, never nested under `accounts` or `affinity` —
    /// same discipline as `select.rs:253-256`.
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
    /// [`crate::manager::pins::affinity_pin_snapshot`] knows which persisted
    /// pins get [`crate::affinity::EXTENDED_PIN_TTL_MS`] instead of the
    /// default at the next restart. Deliberately a side map rather than a third
    /// tuple field on `affinity` itself: nothing on the selection path
    /// (`select.rs`) needs to read it, only the snapshot/restore bridge does, so
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
    /// set of independently locked parts ([`bound_tokens::ShardedBoundTokens`]), none of them **ever**
    /// held while the accounts lock is taken. Each part keeps its own mint order
    /// ([`bound_tokens::BoundTokenMap`]), so holding it full costs nothing per record.
    bound_tokens: bound_tokens::ShardedBoundTokens,
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
    routing: Mutex<crate::routing::Observations>,
}

/// Resets the keep-warm in-flight flag on drop, so a sweep that unwinds early
/// still clears the guard.
struct WarmInFlightGuard<'a>(&'a AtomicBool);

impl Drop for WarmInFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// RAII counter for a request currently being served on one account. Taken via
/// [`Manager::enter_in_flight`] when the proxy picks an account to forward to, it
/// decrements that account's `in_flight` on Drop — so EVERY exit path of a serve
/// attempt (success, 429/401 rotate, transport error, panic-unwind) releases the
/// slot and a counter can never leak and strand an account out of rotation.
/// Modeled on [`WarmInFlightGuard`]; the count lives under the accounts write-lock
/// (no second lock — preserves the no-lock-nesting discipline).
///
/// Holds an owned `Arc` of the manager's account table (not a borrow) so the guard is
/// `'static` and can be MOVED into a streamed response body — its Drop then fires at
/// stream completion rather than at handler return. The `Arc` shares the manager's own
/// table and its one lock; no second lock and no detached counter are introduced.
pub struct InFlightGuard {
    accounts: Arc<RwLock<Vec<AccountRuntime>>>,
    idx: usize,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(account) = accounts.get_mut(self.idx) {
            // saturating_sub so a double-drop / underflow can never wrap to u32::MAX
            // and pin the account out forever.
            account.in_flight = account.in_flight.saturating_sub(1);
        }
    }
}

/// What [`Manager::exhaustion_hint`] learned about an exhausted fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExhaustionHint {
    /// Whole seconds until the soonest account frees, at least 1; the `60`
    /// sentinel when `free_at` is `None`.
    pub retry_after: i64,
    /// The instant that account frees, when any account advertises one.
    pub free_at: Option<OffsetDateTime>,
    /// The gate holding that account — what `retry_after` is timed against.
    pub binding: Option<GateReason>,
    /// Every account out of rotation, counted by the gate holding it.
    pub gated: BTreeMap<GateReason, usize>,
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
    /// Build a manager over `config`, using `refresher` for token refresh,
    /// `prober` for background usage reads, and persisting refreshes to
    /// `config_path` (if given).
    pub fn new(
        config: Config,
        refresher: Arc<dyn TokenRefresher>,
        prober: Arc<dyn UsageProber>,
        warmer: Arc<dyn AccountWarmer>,
        config_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let accounts: Vec<AccountRuntime> = config
            .accounts
            .iter()
            .map(|a| AccountRuntime::from_config(a, config.http1_only))
            .collect();
        Self::assemble(config, refresher, prober, warmer, config_path, accounts)
    }

    /// Assemble the `Arc<Manager>` from an already-built runtime vec. Shared by
    /// [`Self::new`] (which derives the runtimes from `config`) and
    /// [`Self::from_runtimes`] (which is handed pre-seeded runtimes for the demo).
    fn assemble(
        config: Config,
        refresher: Arc<dyn TokenRefresher>,
        prober: Arc<dyn UsageProber>,
        warmer: Arc<dyn AccountWarmer>,
        config_path: Option<PathBuf>,
        accounts: Vec<AccountRuntime>,
    ) -> Arc<Self> {
        let upstream = config.upstream.clone();
        let codex_upstream = config.codex_upstream.clone();
        let proxy_api_key = config.proxy.api_key.clone();
        let request_key = config
            .proxy
            .request_key
            .clone()
            .filter(|key| !key.is_empty());
        let global_threshold = config.switch_threshold;
        let fable_weekly_threshold = config.fable_weekly_threshold;
        let groups = Arc::new(GroupPolicy::from_config(&config));
        let pacing = config.pacing.clone();
        let account_throttle = config.account_throttle.clone();
        let fleet_throttle = config.fleet_throttle.clone();

        let locked_idx = config.lock_account.as_ref().and_then(|name| {
            let idx = accounts.iter().position(|a| a.name == *name);
            if idx.is_none() {
                let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
                tracing::error!(
                    lock_account = %name, available = ?names,
                    "lockAccount name did not match any account — running UNLOCKED (normal routing)"
                );
            }
            idx
        });
        // Capture the locked account name BEFORE `accounts` is moved into the struct.
        let locked_name = locked_idx.and_then(|i| accounts.get(i).map(|a| a.name.clone()));

        // Clamp defensively even though `config::default_control_reserve` and
        // the field's own doc already promise `[0.0, 0.5]` — a hand-edited
        // config file is not bound by either.
        let control_reserve = config.effective_control_reserve();
        // Read before `config` is moved into the struct below, same reason as
        // `locked_name` above.
        let control_pooled = config.control_pooled;
        let control_identity = config.control_identity;

        // Hours → ms once, at construction. `i64::from` then a checked multiply:
        // a hand-edited `resetUrgencyTierHours` of, say, 100_000_000 would
        // overflow the product and wrap to a negative width, which would invert
        // the bucket ordering rather than fail loudly. Saturating at i64::MAX
        // instead degrades to "one bucket for everyone" — the same shape as the
        // `0` disable, which is the safe direction to fall.
        let reset_urgency_tier_ms =
            i64::from(config.reset_urgency_tier_hours).saturating_mul(3_600_000);

        let control_idx = config.control_account.as_ref().and_then(|identity| {
            match crate::identity::match_one(&accounts[..], identity.name()) {
                crate::identity::Match::One(idx) => Some(idx),
                crate::identity::Match::None => {
                    let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
                    tracing::error!(
                        control_account = %identity.name(), available = ?names,
                        "controlAccount did not match any account — running with NO control account"
                    );
                    None
                }
                // Two or more accounts share this name — only reachable on a
                // config hand-edited past `config::load`'s rename migration.
                // Picking the first one silently, as the old `position(...)`
                // scan did, risks binding control to a disabled Team-seat row
                // while the account that resolved it live stays unpinned.
                crate::identity::Match::Ambiguous(candidates) => {
                    tracing::error!(
                        control_account = %identity.name(), candidates = ?candidates,
                        "controlAccount matches more than one account — refusing to guess, running with NO control account"
                    );
                    None
                }
            }
        });
        // The control account is an Anthropic identity: the one Claude Code's
        // bootstrap calls resolve to, probed and `controlIdentity`-checked as one.
        // Codex has no counterpart in this version, so a Codex row named here is
        // warned about and ignored rather than half-honoured. A `match`,
        // so a third provider decides this here instead of inheriting an answer.
        let control_idx = control_idx.filter(|&idx| {
            accounts.get(idx).is_none_or(|account| match account.provider {
                crate::provider::Provider::Anthropic => true,
                crate::provider::Provider::Codex => {
                    tracing::warn!(
                        account = %account.name,
                        "controlAccount names a Codex account; {CODEX_HAS_NO_CONTROL_ACCOUNT}, ignoring it"
                    );
                    false
                }
            })
        });

        let usage = crate::usage::UsageTracker::new(
            accounts.len(),
            crate::pricing::PricingTable::new(config.pricing.clone().into_iter().collect()),
        );

        let manager = Arc::new(Self {
            me: std::sync::OnceLock::new(),
            forecast: Mutex::new(
                (0..accounts.len())
                    .map(|_| crate::forecast::Calibration::default())
                    .collect(),
            ),
            accounts: Arc::new(RwLock::new(accounts)),
            usage,
            refresher,
            prober,
            plan_prober: RwLock::new(Arc::new(crate::probe::LivePlanProber)),
            warmer,
            warm_in_flight: AtomicBool::new(false),
            auto_resets: Mutex::new(auto_reset::Ledger::default()),
            warm_wake: Notify::new(),
            config: Mutex::new(config),
            config_write: Arc::new(Mutex::new(())),
            config_path,
            mutation_released: AtomicBool::new(false),
            usage_gates: Mutex::new(HashMap::new()),
            wake: crate::wake::WakeDetector::new(),
            upstream,
            codex_upstream,
            proxy_api_key,
            request_key,
            unkeyed: crate::request_key::UnkeyedCounters::default(),
            global_threshold,
            fable_weekly_threshold,
            groups: RwLock::new(groups),
            mitm: std::sync::OnceLock::new(),
            fan: std::sync::OnceLock::new(),
            codex_refused: Mutex::new(CodexRefusals::default()),
            codex_unpooled: Mutex::new(MinuteRing::default()),
            groups_reload_mtime: Mutex::new(None),
            pacing,
            account_throttle,
            fleet_throttle,
            locked_idx,
            control_idx: RwLock::new(control_idx),
            org_tat_ms: AsyncMutex::new(HashMap::new()),
            fleet_tat_ms: AsyncMutex::new(HashMap::new()),
            log: Mutex::new(VecDeque::with_capacity(REQUEST_LOG_CAPACITY)),
            current: Mutex::new(None),
            current_by_provider: Mutex::new(crate::stats::CurrentByProvider::default()),
            select_seq: AtomicU64::new(1),
            affinity: Mutex::new(CappedMap::new(select::AFFINITY_CAP, |&(_, touch)| touch)),
            affinity_extended: Mutex::new(CappedSet::new(select::AFFINITY_CAP)),
            affinity_file: crate::cache_file::SavedFile::new(AFFINITY_TOUCH_REFRESH),
            bound_tokens: bound_tokens::ShardedBoundTokens::default(),
            bound_tokens_file: crate::cache_file::SavedFile::new(std::time::Duration::ZERO),
            bound_history_logged: Mutex::new(HashSet::new()),
            bound_switch_warned: Mutex::new(HashMap::new()),
            sessions: Mutex::new(CappedMap::new(snapshot::SESSION_CAP, |s| s.last_seen_ms)),
            wire_sessions: Mutex::new(crate::session_wire::WireSessionTracker::new()),
            wire_sessions_file: crate::cache_file::SavedFile::new(WIRE_SESSIONS_REFRESH),
            session_seq: AtomicU64::new(1),
            next_revalidation_at_ms: std::sync::atomic::AtomicI64::new(0),
            next_codex_revalidation_at_ms: std::sync::atomic::AtomicI64::new(0),
            conn_affinity: Mutex::new(CappedMap::new(select::CONN_AFFINITY_CAP, |&(_, touch)| {
                touch
            })),
            divert_ledger: Mutex::new(CappedMap::new(select::DIVERT_LEDGER_CAP, |e| e.until_ms)),
            control_reserve,
            control_pooled,
            control_identity,
            client_identities: crate::control_identity::ClientIdentityCache::new(),
            reset_urgency_tier_ms,
            routing: Mutex::new(crate::routing::Observations::default()),
        });
        // Hand the manager its own `Arc` (weakly) now that one exists — see the
        // `me` field. Infallible: this is the only write, on a just-built value.
        if manager.me.set(Arc::downgrade(&manager)).is_err() {
            tracing::warn!("manager self-reference was already set");
        }

        if let (Some(i), Some(name)) = (locked_idx, locked_name) {
            tracing::warn!(
                account = %name, idx = i,
                "ACCOUNT LOCK ACTIVE — this provider's traffic pinned to one account; rotation/affinity/migration bypassed, no failover"
            );
        }

        manager
    }

    /// Convenience constructor for the real binary (live OAuth refresher + live
    /// usage prober).
    pub fn with_live_refresher(config: Config, config_path: Option<PathBuf>) -> Arc<Self> {
        let prober = LiveUsageProber::new().with_codex_base(config.codex_upstream.clone());
        Self::new(
            config,
            Arc::new(LiveRefresher::new()),
            Arc::new(prober),
            Arc::new(LiveWarmer::new()),
            config_path,
        )
    }

    /// Build a manager from pre-seeded runtime rows, for the `tcr demo` dashboard
    /// (`src/demo.rs`). The live refresher/prober/warmer are held but NEVER invoked
    /// — the demo only ever calls [`Self::snapshot`] and [`Self::set_disabled`], so
    /// no token, probe, or network I/O ever happens. `config_path = None` makes
    /// every persist a no-op, so nothing can be written to disk.
    pub fn from_runtimes(accounts: Vec<AccountRuntime>) -> Arc<Self> {
        #[expect(
            clippy::expect_used,
            reason = "Config has no Default; an empty JSON object parses by construction and the signature has no error channel"
        )]
        let config: Config = serde_json::from_str("{}")
            .expect("an empty JSON object is always a valid default config");
        let prober = LiveUsageProber::new().with_codex_base(config.codex_upstream.clone());
        Self::assemble(
            config,
            Arc::new(LiveRefresher::new()),
            Arc::new(prober),
            Arc::new(LiveWarmer::new()),
            None,
            accounts,
        )
    }

    /// Seed one fake live session into the sessions map, for the `tcr demo`
    /// dashboard alone — never touched by the real serving path (which upserts
    /// sessions in [`Self::record_served`]). Lets the demo paint a populated
    /// sessions pane without routing any real traffic.
    pub fn seed_session(
        &self,
        key: u64,
        account_idx: usize,
        requests: u64,
        last_seen_ms: i64,
        kind: SessionKind,
    ) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.insert(
            key,
            SessionStat {
                account_idx,
                requests,
                last_seen_ms,
                kind,
            },
        );
    }

    /// Hard state of account `idx` — lets the proxy skip an account that a
    /// refresh just proved dead (`Error`) without wasting an upstream round-trip.
    pub fn account_status(&self, idx: usize) -> Option<AccountStatus> {
        self.account(idx, |a| a.status)
    }

    /// A `retry-after` hint (seconds) for a synthetic 429 when every account is
    /// exhausted: the soonest instant at which SOME account genuinely re-enters
    /// rotation, clamped to at least 1s, defaulting to 60s when nothing is known.
    /// The seconds half of [`Self::exhaustion_hint`]; see it for the derivation, and for why
    /// it folds only `provider`'s accounts.
    pub fn retry_after_hint(
        &self,
        now: OffsetDateTime,
        is_fable: bool,
        provider: crate::provider::Provider,
    ) -> i64 {
        self.exhaustion_hint(now, is_fable, provider, None)
            .retry_after
    }

    /// [`Self::retry_after_hint`], scoped to the members of one group.
    ///
    /// Exists for STRICT (reserved) groups. A strict `--group g` request can only
    /// ever be served by a member of `g`, so sizing its wait with the fleet-wide
    /// hint above times it against accounts the request will never be allowed to
    /// use: any unrelated account un-gating sooner wins that `min`, the caller's
    /// one-shot soft-wait is spent waking early, and the request answers a 429
    /// while its own member is still seconds from free. The fleet-wide hint stays
    /// exactly as it is — ungrouped traffic genuinely can use any account, and
    /// that promise must keep holding for it.
    ///
    /// Returns the same `60` default when no member advertises a recovery instant,
    /// so a strict group whose members are all `Error`/disabled degrades to the
    /// same honest "try again in a minute" as an exhausted fleet.
    pub fn retry_after_hint_for_group(
        &self,
        now: OffsetDateTime,
        is_fable: bool,
        provider: crate::provider::Provider,
        group: &str,
    ) -> i64 {
        self.exhaustion_hint(now, is_fable, provider, Some(group))
            .retry_after
    }

    /// Everything a synthetic exhausted 429 can honestly tell the client, derived
    /// once from the same per-account gate view selection uses.
    ///
    /// `retry_after` is the soonest instant at which SOME account genuinely
    /// re-enters rotation, in whole seconds, clamped to at least 1. Honest by
    /// construction: it minimises over each account's [`Self::account_gate`]
    /// `free_at` — the instant ALL of *that* account's active gates clear —
    /// instead of the raw min over every window's reset. The raw-min was
    /// bug-shaped: it counted a 5-hour reset of an account that stays gated on
    /// its weekly bucket, and the reset of an `Error`/disabled account that never
    /// self-frees at all, so it promised a recovery that would not happen.
    /// Accounts that contribute nothing (`Ok`, `Login`, `Disabled`, or a gating
    /// window with no known reset) are skipped.
    ///
    /// When NO account advertises a recovery instant, `retry_after` is the `60`
    /// sentinel and `free_at` is `None`. The two are reported separately on
    /// purpose: a caller sizing a wait keeps reading the number, and a caller
    /// writing a message can say "no reset time is known" instead of printing a
    /// fixed 60 that a reader would take for a measurement. `proxy.rs` relies on
    /// the sentinel staying above its soft-wait cap — see the compile-time
    /// assertion beside `EXHAUSTION_SOFT_WAIT_MAX_SECS` there.
    ///
    /// `binding` is the gate that holds the soonest-freeing account — the one
    /// whose clearing `retry_after` is timed against — so the 429 can carry the
    /// matching `anthropic-ratelimit-unified-*` claim when that gate is a quota
    /// window, and stay a plain 429 when it is not. `gated` counts every account
    /// out of rotation by its gate, for the message.
    ///
    /// `is_fable` scopes the evaluation exactly as selection does: only a Fable
    /// request is gated by the model-scoped weekly (`7d_oi`) bucket, so an
    /// all-Fable-exhausted fleet reports that bucket's reset while non-Fable
    /// traffic ignores it.
    ///
    /// `group` narrows the fleet to one group's members and, reaching
    /// [`Self::account_gate`] as `Some(group)` on purpose, reports each member's
    /// REAL gate rather than the reservation: an explicit ask is never
    /// reserved-blocked by its own group ([`Self::reserved_blocks`]). `None` is
    /// the "unrequested traffic" view [`Self::snapshot`] reports — a promise this
    /// hint makes must hold for the traffic that would actually hit the 429.
    ///
    /// `provider` narrows the fold to the request's own provider's accounts, before
    /// anything else is asked of them. An account of the other provider can never
    /// serve this request (`eligible` refuses it first), so its reset says nothing
    /// about when this one can be served: folding it in let a Codex account freeing
    /// in two seconds time a Claude client's wait, and its gate inflate the count of
    /// accounts "out", while every Anthropic account stayed out for an hour.
    ///
    /// **An all-rejected Codex fleet** is the one place a terminal gate contributes.
    /// `rejected` has no clear-instant (`Self::account_terminal_gate`), so a Codex
    /// fleet whose every account answered a quota refusal used to report the 60-second
    /// sentinel, which the Codex client renders as "resets in 60 seconds" for a window
    /// days away. When EVERY account folded is `rejected`, each Codex one contributes
    /// when it can be expected back (`Self::rejected_codex_free_at_ms`): the refusal body's
    /// measured reset, then the latest live reset among its windows at or over its threshold,
    /// and only with neither its rate-limit hold's expiry. Measured Codex quota holds preserve
    /// that full deadline instead of using the transient hold's one-hour cap. An
    /// Anthropic `rejected` still contributes nothing, so Anthropic's hint is unchanged:
    /// it is upstream's verdict, which need not belong to any window this account stores
    /// (see `rejected_account_reports_a_rejected_gate`).
    pub fn exhaustion_hint(
        &self,
        now: OffsetDateTime,
        is_fable: bool,
        provider: crate::provider::Provider,
        group: Option<&str>,
    ) -> ExhaustionHint {
        self.exhaustion_hint_over(now, is_fable, Some(provider), group, None)
    }

    /// [`Self::exhaustion_hint`] for a request that ONE account alone may serve.
    ///
    /// Exists for a request whose history is account-bound (see [`crate::bound_tokens`]): the
    /// proxy benches every other account, so the fleet-wide hint times the wait against
    /// accounts this request will never be allowed to use. Any of them freeing sooner wins the
    /// fleet's `min`, spends the request's one-shot soft-wait waking early, and answers a 429
    /// whose `retry-after` has nothing to do with when the bound account frees.
    ///
    /// Same sentinel as the fleet form: `60` with `free_at: None` when that account advertises
    /// no recovery instant.
    pub fn exhaustion_hint_for_account(
        &self,
        now: OffsetDateTime,
        is_fable: bool,
        idx: usize,
    ) -> ExhaustionHint {
        self.exhaustion_hint_over(now, is_fable, None, None, Some(idx))
    }

    /// The one derivation behind both hint forms: `provider` narrows to one provider's
    /// accounts, `group` to a group's members, `only` to a single account index. The
    /// single-account form passes no provider: the one account it names is the request's
    /// bound account, which `proxy::handle` only ever takes from the request's own provider.
    fn exhaustion_hint_over(
        &self,
        now: OffsetDateTime,
        is_fable: bool,
        provider: Option<crate::provider::Provider>,
        group: Option<&str>,
        only: Option<usize>,
    ) -> ExhaustionHint {
        let now_ms = odt_to_ms(now);
        let codex_candidates = match provider.or_else(|| only.and_then(|idx| self.provider(idx))) {
            Some(crate::provider::Provider::Codex) => {
                Some(self.codex_candidate_indices(only, group))
            }
            Some(crate::provider::Provider::Anthropic) | None => None,
        };
        let groups = self.groups();
        let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
        let mut gated: BTreeMap<GateReason, usize> = BTreeMap::new();
        let mut soonest: Option<(i64, GateReason)> = None;
        // The all-rejected Codex fold (see `exhaustion_hint`): whether every account
        // folded is `rejected`, and the soonest instant a rejected Codex one is expected
        // back. Used only when the first is still true after the loop.
        let mut every_one_rejected = true;
        let mut rejected_codex_soonest: Option<i64> = None;
        for (_, account) in accounts
            .iter()
            .enumerate()
            .filter(|&(idx, _)| only.is_none_or(|only| only == idx))
            .filter(|&(idx, _)| {
                codex_candidates
                    .as_ref()
                    .is_none_or(|candidates| candidates.contains(&idx))
            })
            .filter(|(_, account)| provider.is_none_or(|p| account.provider == p))
            .filter(|(_, account)| group.is_none_or(|g| account.groups.iter().any(|m| m == g)))
        {
            let threshold = account.switch_threshold.unwrap_or(self.global_threshold);
            let (reason, free_at) = Self::account_gate(
                account,
                threshold,
                self.fable_weekly_threshold,
                now,
                now_ms,
                is_fable,
                group,
                &groups.reserved,
                &groups.parked,
            );
            if reason == GateReason::Rejected {
                match account.provider {
                    crate::provider::Provider::Anthropic => {}
                    crate::provider::Provider::Codex => {
                        if let Some(at) = Self::rejected_codex_free_at_ms(account, threshold, now) {
                            rejected_codex_soonest =
                                Some(rejected_codex_soonest.map_or(at, |best| best.min(at)));
                        }
                    }
                }
            } else {
                every_one_rejected = false;
            }
            if reason != GateReason::Ok {
                *gated.entry(reason).or_insert(0) += 1;
            }
            let Some(at) = free_at.map(odt_to_ms).filter(|&at| at > now_ms) else {
                continue;
            };
            if soonest.is_none_or(|(best, _)| at < best) {
                soonest = Some((at, reason));
            }
        }
        if every_one_rejected && let Some(at) = rejected_codex_soonest {
            soonest = Some((at, GateReason::Rejected));
        }
        match soonest {
            Some((at, reason)) => ExhaustionHint {
                retry_after: ((at - now_ms + 999) / 1000).max(1),
                free_at: ms_to_odt(at),
                binding: Some(reason),
                gated,
            },
            None => ExhaustionHint {
                retry_after: 60,
                free_at: None,
                binding: None,
                gated,
            },
        }
    }

    /// When a `rejected` CODEX account can be expected back, for the exhaustion hint
    /// alone (see [`Self::exhaustion_hint`]), in epoch ms and only while still ahead of
    /// `now`: the refusal body's measured reset, then the latest live reset among its
    /// windows at or over `threshold` (every one has to roll before it can serve), else its
    /// rate-limit hold's expiry. `None` when none is known.
    ///
    /// A measured Codex quota deadline keeps its full duration
    /// ([`Self::mark_codex_quota_limited`]); only transient rate limits use the bounded hold.
    /// The account stays rejected until a usage probe sees the window it stored roll, reads
    /// the account allowed ([`crate::quota::Quota::apply_usage`]), or a served 2xx carrying a
    /// window reads it allowed.
    fn rejected_codex_free_at_ms(
        account: &AccountRuntime,
        threshold: f64,
        now: OffsetDateTime,
    ) -> Option<i64> {
        let now_ms = odt_to_ms(now);
        account
            .quota
            .codex
            .refusal_reset
            .map(odt_to_ms)
            .or_else(|| {
                Self::rejected_codex_window(account, threshold, now)
                    .map(|(_, reset)| odt_to_ms(reset))
            })
            .or(account.rate_limited_until_ms)
            .filter(|&at| at > now_ms)
    }

    /// The window that is holding a `rejected` CODEX account out, and when it rolls: among its
    /// five-hour and weekly windows at or over `threshold` with a live reset, the one whose
    /// reset is latest, since every one of them has to roll before the account serves. `None`
    /// when no window explains the refusal.
    ///
    /// One answer for both of its readers, [`Self::rejected_codex_free_at_ms`] (when the
    /// exhaustion hint expects the account back) and [`Self::codex_refusal_gate`] (what the
    /// snapshot reports), so the two cannot name different windows.
    fn rejected_codex_window(
        account: &AccountRuntime,
        threshold: f64,
        now: OffsetDateTime,
    ) -> Option<(GateReason, OffsetDateTime)> {
        [
            (GateReason::FiveHour, account.quota.five_hour),
            (GateReason::SevenDay, account.quota.seven_day),
        ]
        .into_iter()
        .filter_map(|(gate, window)| {
            let window = window.filter(|w| w.effective(now) >= threshold)?;
            Some((gate, window.live_reset(now)?))
        })
        .max_by_key(|&(_, reset)| reset)
    }

    /// The window that refused a `rejected` CODEX account and has reached its reset since, with
    /// no probe yet to read the new window: among its five-hour and weekly windows whose STORED
    /// utilization is at or over `threshold` and whose reset is at or before `now`, the one
    /// whose reset is latest, as in [`Self::rejected_codex_window`]. `None` when no such window
    /// is stored.
    ///
    /// The stored utilization, not [`crate::quota::QuotaWindow::effective`], which reads a
    /// window past its reset as `0.0`: that the window rolled is the clock's inference, and the
    /// refusal stands until a usage probe reads the new window
    /// ([`crate::quota::Quota::apply_usage`]), or a served 2xx that carries a window reads the
    /// account allowed.
    fn rolled_codex_window(
        account: &AccountRuntime,
        threshold: f64,
        now: OffsetDateTime,
    ) -> Option<GateReason> {
        [
            (GateReason::FiveHour, account.quota.five_hour),
            (GateReason::SevenDay, account.quota.seven_day),
        ]
        .into_iter()
        .filter_map(|(gate, window)| {
            let window = window.filter(|w| w.utilization >= threshold)?;
            Some((gate, window.reset.filter(|&reset| reset <= now)?))
        })
        .max_by_key(|&(_, reset)| reset)
        .map(|(gate, _)| gate)
    }

    /// The gate a CODEX account at its usage limit REPORTS: the window that refused it, with
    /// that window's reset as the time it frees, in place of a `Rejected` that names no time.
    /// Only a Codex row whose `account_gate` is `Rejected` is changed:
    ///
    /// 1. a full window with a live reset ([`Self::rejected_codex_window`]) reports its own gate,
    ///    `FiveHour` or `SevenDay`, freeing at that reset;
    /// 2. else a full window whose reset has passed, with no probe yet to confirm the roll
    ///    ([`Self::rolled_codex_window`]), keeps its own gate, with no time;
    /// 3. else a hold still in the future reports itself, freeing at the hold's end: `Hold`, or
    ///    `Refused` when an upstream 403 armed it, the name `account_gate` gives that hold;
    /// 4. else it stays `Rejected`, with no time.
    ///
    /// Why: ChatGPT's usage-limit refusal names the window it refused on, and that window's
    /// headers carry its reset, so the account's return is known. Anthropic's `rejected` is its
    /// own verdict and names no window, so an Anthropic row is returned unchanged.
    ///
    /// Why rule 2: past the reset the window has rolled as far as the clock knows, but tcr has
    /// not read the new window, and the refusal stands until a usage probe reads it, or until a
    /// served response from the account answers 2xx with a window
    /// (`Quota::update_from_codex_refusal_at`) or refuses again. The row keeps naming that window,
    /// with no time, so it does not read as a refusal no window explains. The hold the refusal
    /// armed has lapsed by then, so the account is back in the pool and may be sent a request in
    /// those minutes.
    ///
    /// Why rule 3 holds even for a refusal that carried no reset: the proxy arms a hold for every
    /// Codex quota refusal, to its body's measured reset, then its windows' reset
    /// (`quota::codex_hold_seconds`), else to the 429's `retry-after`, else for 60 seconds.
    /// Measured quota deadlines are preserved in full. While that hold is live the
    /// account really is held, with a known end, so `Hold` and its countdown are the truth; once
    /// the hold lapses, the refusal that still stands reads `Rejected`. The live hold can also be
    /// an upstream 403's ([`Self::mark_refused`]): once the refusal's own hold lapses the account
    /// is back in the pool, which never reads `rejected`, and a 403 then holds it for
    /// [`REFUSED_HOLD_SECONDS`]. That hold reads `Refused` here, as it does on any other row.
    ///
    /// Display only, and only [`Self::snapshot`] calls it. Selection still reads the refusal
    /// through `account_terminal_gate`, and the exhaustion hint through `account_gate`, so this
    /// remap changes neither whether the account is held out nor when the hint expects it back.
    /// The probe cadence does read the snapshot row, so `schedule::ProbeUrgency::of` takes a
    /// standing refusal from the row's `limit_reached_type` as well as from `Rejected`, and the
    /// remap cannot slow the probe that ends the refusal either.
    /// The tokens are ones every client already parses: they are variants of the closed
    /// `GateReason` enum, so no older CLI fails to read a payload that carries them.
    pub(super) fn codex_refusal_gate(
        account: &AccountRuntime,
        threshold: f64,
        now: OffsetDateTime,
        gate: GateReason,
        free_at: Option<OffsetDateTime>,
    ) -> (GateReason, Option<OffsetDateTime>) {
        match account.provider {
            crate::provider::Provider::Anthropic => return (gate, free_at),
            crate::provider::Provider::Codex => {}
        }
        if gate != GateReason::Rejected {
            return (gate, free_at);
        }
        if let Some((window, reset)) = Self::rejected_codex_window(account, threshold, now) {
            return (window, Some(reset));
        }
        if let Some(window) = Self::rolled_codex_window(account, threshold, now) {
            return (window, None);
        }
        match account
            .rate_limited_until_ms
            .filter(|&until| until > odt_to_ms(now))
        {
            // Named by its cause, as `account_gate` names a hold on a row with no refusal.
            Some(until) => (
                match account.hold_cause {
                    HoldCause::RateLimit => GateReason::Hold,
                    HoldCause::Refused => GateReason::Refused,
                },
                ms_to_odt(until),
            ),
            None => (GateReason::Rejected, None),
        }
    }

    /// Mark account `idx` as serving one more in-flight request: bump its
    /// `in_flight` count and stamp `last_served_ms`, returning an [`InFlightGuard`]
    /// that decrements the count on Drop. The proxy takes this the moment it picks
    /// an account to forward to, so the guard's lifetime spans the whole serve
    /// attempt and releases the slot on every exit path (success, rotate, error,
    /// panic). For a terminal SSE (`text/event-stream`) serve the proxy MOVES the
    /// owned guard into the streamed body, so the decrement fires at stream
    /// completion — not at handler return, when axum has yet to poll the body.
    ///
    /// Also ages the account's HTTP client: this is the one call taken exactly
    /// once per upstream send, so it is where [`MAX_SERVES_PER_CONNECTION`] is
    /// counted and where a due recycle is claimed. The accounts write-lock is
    /// taken here and then AGAIN by [`Self::recycle_client`] — never nested, and
    /// never held across the client build. It used to be a single uncontested
    /// section; that is why the claim resets the counter rather than letting
    /// `recycle_client` re-read it, which would race between the two sections.
    ///
    /// It is also where a sleep is noticed, for the same reason: it runs once
    /// per send and BEFORE the proxy checks the send's client out with
    /// [`Self::http_client`], so the send that first observes the wake already
    /// goes out on a fresh pool. [`Self::http_client`] is a read accessor that
    /// tests call to compare `Arc`s; making it rebuild clients would turn a
    /// getter into a writer.
    pub fn enter_in_flight(self: &Arc<Self>, idx: usize) -> InFlightGuard {
        if let Some(slept) = self.wake.observe() {
            self.retire_every_client_after_sleep(slept);
        }
        self.enter_in_flight_at(idx, OffsetDateTime::now_utc())
    }

    /// Claim a send slot and stamp it on the caller's clock. Sleep observation
    /// belongs to the production wrapper; RAII and client recycling are shared.
    /// Replay-only entry; production sends use [`Self::enter_in_flight`] to observe sleep.
    #[doc(hidden)]
    pub fn enter_in_flight_at(self: &Arc<Self>, idx: usize, now: OffsetDateTime) -> InFlightGuard {
        let claimed_recycle = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            match accounts.get_mut(idx) {
                Some(account) => {
                    account.in_flight = account.in_flight.saturating_add(1);
                    account.last_served_ms = odt_to_ms(now);
                    account.serves_since_client_build =
                        account.serves_since_client_build.saturating_add(1);
                    // Claim the recycle under the SAME write lock that observes the
                    // threshold, and reset the counter as the act of claiming it:
                    // that is what makes the claim exclusive, so two sends crossing
                    // the threshold concurrently cannot both rebuild — the loser
                    // reads 0 and returns false.
                    if account.serves_since_client_build >= MAX_SERVES_PER_CONNECTION {
                        account.serves_since_client_build = 0;
                        true
                    } else {
                        false
                    }
                }
                None => false,
            }
        };
        if claimed_recycle {
            self.recycle_client(idx, RecycleReason::ServeBudget);
        }
        InFlightGuard {
            accounts: Arc::clone(&self.accounts),
            idx,
        }
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

    /// Give EVERY account a fresh HTTP client after the machine slept for
    /// `slept`, not just the one about to send: the sleep outlasted every
    /// pooled connection's idle life on the server side, and none of the pools
    /// can tell (see [`crate::wake`]). Each account's next send then opens a
    /// new connection instead of failing once on a dead one and retrying.
    ///
    /// Same rules as [`Self::recycle_client`]: in-flight requests keep their
    /// own `Arc` of the old client, and the replacements are built with no lock
    /// held. An account appended between the count and the swap already came
    /// with a fresh client of its own, so it is left alone. Each swapped
    /// account's serve-budget counter restarts, since it counts serves on the
    /// CURRENT client.
    ///
    /// The one INFO line per wake is here; nothing logs per request.
    fn retire_every_client_after_sleep(&self, slept: std::time::Duration) {
        let count = self
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        let http1_only = self.http1_only();
        let fresh: Vec<_> = (0..count)
            .map(|_| build_serving_client(http1_only))
            .collect();
        let retired = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let mut retired = 0usize;
            for (account, http) in accounts.iter_mut().zip(fresh) {
                account.http = http;
                account.serves_since_client_build = 0;
                retired += 1;
            }
            retired
        };
        tracing::info!(
            slept_secs = slept.as_secs(),
            clients_retired = retired,
            "the machine slept; retiring every account's pooled upstream connection, \
             which the sleep most likely closed while the pool's idle timers stood still"
        );
    }

    /// Recycle account `idx`'s client after an HTTP/2 stream reset, before the
    /// proxy's same-account retry sends on it again.
    ///
    /// An h2 stream reset (`RST_STREAM`) fails one stream; the connection
    /// itself stays open and pooled, unlike a connection-level death, which
    /// reqwest evicts from the pool as part of raising the error. Left alone,
    /// every retry and every rotation would keep checking out that same
    /// broken connection, the live incident this exists to fix: 477
    /// stream-reset failures across every account over 11 minutes, cured only
    /// by a full process restart, because nothing in the ladder ever swapped
    /// the connection out. This is [`Self::recycle_client`] with
    /// [`RecycleReason::StreamReset`], so its one log line names the reset
    /// rather than a serve budget that was never spent.
    pub fn recycle_client_after_reset(&self, idx: usize) {
        self.recycle_client(idx, RecycleReason::StreamReset);
    }

    /// Retire account `idx`'s HTTP client and give it a fresh one, dropping the
    /// pooled h2 connection that has now carried [`MAX_SERVES_PER_CONNECTION`]
    /// sends before Anthropic's edge drains it out from under us.
    ///
    /// Requests already in flight are untouched. Each holds its own
    /// `Arc<reqwest::Client>` clone, taken by [`Self::http_client`] before the
    /// send, so the OLD client — and the connection carrying their streams —
    /// stays alive until the last of them finishes and drops the final `Arc`.
    /// Only sends selected after this point get the new pool. That is the whole
    /// reason this swaps an `Arc` instead of reaching into the existing client.
    ///
    /// The replacement is built with NEITHER lock held, matching
    /// [`Self::add_or_update_account`]'s Added path and for the same reason: a
    /// panic in [`build_serving_client`] (which tests inject) must not poison
    /// `self.accounts`.
    ///
    /// `reason` is the caller's, and picks the one log line this writes: the
    /// line used to claim a spent serve budget for every recycle, including
    /// the stream-reset one.
    fn recycle_client(&self, idx: usize, reason: RecycleReason) {
        let fresh = build_serving_client(self.http1_only());
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(account) = accounts.get_mut(idx) else {
            // Stale index: accounts are appended, never removed, so this is not
            // reachable on the live path — same rationale as `http_client`.
            return;
        };
        account.http = fresh;
        match reason {
            RecycleReason::ServeBudget => tracing::info!(
                account_index = idx,
                account = %account.name,
                serves = MAX_SERVES_PER_CONNECTION,
                "retiring the upstream connection after its serve budget — \
                 a fresh h2 connection costs a handshake, not a prompt-cache miss"
            ),
            RecycleReason::StreamReset => tracing::info!(
                account_index = idx,
                account = %account.name,
                "retiring the upstream connection after an HTTP/2 stream reset, \
                 the connection survives the reset, so the retry needs a fresh one"
            ),
        }
    }

    /// Flush the refreshed TOKENS to disk. Token refreshes already persist
    /// incrementally via [`Self::persist_tokens`], so this is the
    /// belt-and-suspenders final flush on shutdown (DESIGN §main). A missing
    /// `config_path` (tests, corrupt-source boot) is a silent no-op so a corrupt
    /// user file is never clobbered with defaults.
    ///
    /// Writes via [`config::save_tokens`], NOT [`config::save`]: the in-memory
    /// `Config` is a boot-time snapshot, so flushing it whole would revert every
    /// setting the user edited while the proxy was running.
    /// Give up the right to mutate account state, permanently.
    ///
    /// Called by the predecessor in a socket handoff, AFTER its final flush and
    /// BEFORE the successor starts serving. See [`Self::mutation_released`] for
    /// what each refusal prevents. The release sits between the predecessor's
    /// last legitimate write and the successor's first: any earlier and the
    /// final flush is gated off by this very flag, any later and the
    /// predecessor's own shutdown persist lands on the successor's fresh
    /// tokens.
    pub fn release_mutation_ownership(&self) {
        // `swap` rather than `store` so the log line fires once even if a
        // caller releases twice.
        if !self.mutation_released.swap(true, Ordering::SeqCst) {
            tracing::info!(
                "mutation ownership released; this process will not refresh a token or write the config again"
            );
        }
    }

    /// Whether [`Self::release_mutation_ownership`] has been called.
    pub fn mutation_is_released(&self) -> bool {
        self.mutation_released.load(Ordering::SeqCst)
    }

    pub fn persist_now(&self) {
        // After a handoff this write would land our stale in-memory tokens on
        // top of the successor's fresh ones. `shutdown_within` calls this on
        // the way out, so the gate has to be here and not at the call site.
        if self.mutation_is_released() {
            tracing::debug!("skipping config persist: mutation ownership was released");
            return;
        }
        let Some(path) = &self.config_path else {
            return;
        };
        // INV1 — the write lock, held across the whole read-modify-write below, is
        // what stops a shutdown flush racing a concurrent persist_tokens and
        // clobbering a just-rotated token. It replaces the config lock in that role;
        // dropping it here while persist_tokens no longer holds `config` across its
        // save would leave BOTH writers unserialized.
        let _writing = self
            .config_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // INV2 — clone inside a short critical section so the config lock is
        // released on this line, before any file I/O below runs.
        let snapshot = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Err(err) = config::save_tokens(path, &snapshot) {
            tracing::error!(error = %err, "failed to flush config on shutdown");
        }
    }

    fn persist_tokens(
        &self,
        name: &str,
        account_uuid: Option<String>,
        org_uuid: Option<String>,
        org_name: Option<String>,
        provider: crate::provider::Provider,
        tokens: &Tokens,
    ) {
        // Defence in depth: `refresh_plan` already refuses to produce a refresh
        // once ownership is released, so nothing should reach here. Gated anyway
        // because this is the function that actually touches the credential file.
        if self.mutation_is_released() {
            tracing::debug!("skipping token persist: mutation ownership was released");
            return;
        }
        let Some(path) = &self.config_path else {
            return;
        };
        // Match the config entry by full identity (account_uuid + org), not name
        // alone, so a rotated token lands on the RIGHT entry when the same email
        // is logged into two orgs (finding #9). With all identity fields None
        // (today's config) same_identity falls back to name equality — unchanged.
        //
        // The provider rides on the probe, as on every probe, and here it decides:
        // `identity::same_identity` never matches a row of another provider, so only
        // rows of this one are candidates, both in the `resolve` below and in the
        // pairing `save_tokens_for`'s merge makes. An Anthropic row and a Codex row
        // may share a name, and when either lacks an account UUID the name fallback
        // alone calls them one account — a tie that would refuse to persist this
        // rotation, or a pairing that would stamp it onto the other provider's row.
        let probe = crate::identity::probe(provider, name, account_uuid, org_uuid, org_name);
        let sync_pending = match provider {
            crate::provider::Provider::Anthropic => None,
            crate::provider::Provider::Codex => {
                let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                accounts
                    .iter()
                    .find(|a| {
                        a.provider == provider
                            && a.name == name
                            && a.account_uuid == probe.account_uuid
                            && a.org_uuid == probe.org_uuid
                    })
                    .and_then(|a| a.pending_codex_writeback.as_ref())
                    .map(refresh::PendingCodexWriteback::record)
            }
        };

        // INV1 — the modify and the save must be ONE atomic step against the file.
        // Without that, two concurrent refreshes race: a stale save clobbers the
        // other account's just-rotated refresh token, which then 400s
        // ("invalid_grant") on its next refresh and is dead until re-authed by hand.
        // This lock — not the config lock — is what provides that serialization now,
        // and it covers save_tokens' whole read-modify-write of the file.
        let _writing = self
            .config_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // INV2 — the config lock is taken ONLY for this block: apply the rotation to
        // the snapshot and clone it out. The guard drops at the closing brace, so
        // `save_tokens` below runs with `self.config` free and a per-connection
        // `session_affinity_enabled()` never waits on an fsync. Cloning is safe here
        // in a way it was NOT before: INV1 already serializes the writers, so no
        // second writer can slip a stale document in between.
        //
        // Deliberately NOT INV3 (mutate only after a successful write): the mutation
        // IS the write's content — it has to precede it — and memory is the recovery
        // buffer for a single-use token. If this save fails, the freshly rotated
        // token still sits in the snapshot for `persist_now` to flush at shutdown;
        // rolling memory back on failure would lose it for good.
        let (snapshot, placement) = {
            let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            // Resolved, never first-match: `same_identity` treats an unknown org as
            // a match, so with the legacy two-org shape in the file a first-match
            // search lands THIS account's rotated credential on the OTHER account's
            // record — overwriting a single-use refresh token that is then dead.
            // An unbreakable tie writes nothing and says so.
            let placement = crate::identity::resolve(config.accounts.iter().enumerate(), &probe);
            if let crate::identity::Resolved::One(position) = placement
                && let Some(account) = config.accounts.get_mut(position)
            {
                account.access_token.clone_from(&tokens.access_token);
                // THE TRAP: same invariant as `apply_refresh` in
                // `manager/refresh.rs` — `tokens.refresh_token` must be
                // `Some(...)` on every reachable refresh success.
                account.refresh_token.clone_from(&tokens.refresh_token);
                account.expires_at = Some(tokens.expires_at_ms);
                match provider {
                    crate::provider::Provider::Anthropic => {}
                    crate::provider::Provider::Codex => match sync_pending {
                        Some(value) => {
                            account
                                .extra
                                .insert(config::CODEX_SYNC_PENDING_KEY.to_string(), value);
                        }
                        None => {
                            account.extra.remove(config::CODEX_SYNC_PENDING_KEY);
                        }
                    },
                }
            }
            (config.clone(), placement)
        };
        // Logged with the config lock released (INV2) — a `warn!` is cheap but the
        // critical section above is on the per-connection path's lock.
        match placement {
            crate::identity::Resolved::One(_) => {}
            crate::identity::Resolved::None => tracing::warn!(
                account = %name,
                "no loaded config account carries this identity; the rotated token is not in the snapshot and will not reach disk"
            ),
            crate::identity::Resolved::Many => tracing::warn!(
                account = %name,
                "more than one loaded config account carries this identity; refusing to guess which one just rotated, so the rotated token will not reach disk and this account may need `tcr login`"
            ),
        }
        // Tokens only, and only THIS account's: the in-memory config is a
        // boot-time snapshot, so writing it whole would stamp stale settings over
        // the user's live file. Its CREDENTIALS for the other accounts are stale
        // in the same way and cost more to lose, because another process that
        // refreshed or re-authed one of them in the meantime gets its single-use
        // refresh token reverted and that account dies until `tcr login`. See
        // `config::CredentialScope`.
        //
        // A placement that is not `One` means the rotated credential never
        // reached the snapshot at all (both branches above say exactly that), so
        // there is nothing of ours to persist and the only thing a write could do
        // is stamp stale copies over whatever is on disk. Write nothing.
        let crate::identity::Resolved::One(position) = placement else {
            return;
        };
        if let Err(err) =
            config::save_tokens_for(path, &snapshot, config::CredentialScope::Only(position))
        {
            tracing::error!(error = %err, "failed to persist refreshed token to config");
        }
    }

    /// The current group settings, one version of the config file. Clones the
    /// `Arc` rather than returning a guard so a caller never holds this lock
    /// across a second lock acquisition (`accounts`, `affinity`). A caller that
    /// asks more than one group question reads them all from one `groups()`,
    /// so a reload between two questions cannot hand it a mix of two versions.
    fn groups(&self) -> Arc<GroupPolicy> {
        Arc::clone(&self.groups.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The currently-reserved group names, for a caller that needs the set
    /// itself. Two calls to this and [`Self::parked_groups`] may read two
    /// versions; a caller that needs both reads [`Self::groups`] once.
    fn reserved_groups(&self) -> HashSet<String> {
        self.groups().reserved.clone()
    }

    /// The currently-parked group names. See [`Self::reserved_groups`].
    pub(super) fn parked_groups(&self) -> HashSet<String> {
        self.groups().parked.clone()
    }

    /// Whether `group` is currently reserved — and therefore STRICT: its own
    /// traffic never spills to a non-member (see
    /// [`Self::select_with_group`]'s strict arm).
    ///
    /// A membership test rather than [`Self::reserved_groups`] + `contains`,
    /// because the request path asks this per request and only ever needs the
    /// one answer; cloning the set to look at a single key is waste on a hot
    /// path. Reads the same hot-reloaded cache, so `tcr group unreserve` takes
    /// effect on the next request with no restart.
    pub fn is_group_reserved(&self, group: &str) -> bool {
        self.groups().reserved.contains(group)
    }

    /// The group names currently opted in to `spillToPool`. See
    /// [`Self::reserved_groups`].
    pub(super) fn spill_groups(&self) -> HashSet<String> {
        self.groups().spill.clone()
    }

    /// Re-read [`config::Account::groups`] and `groupSettings` (`reserved`,
    /// `parked`, `allowControlAccount`, `color`) from `self.config_path` when the file's mtime has moved since
    /// the last check — the fix for group edits appearing to do nothing. The
    /// same pass also follows the file's account LIST: an account it gained is
    /// admitted, and one it lost is marked [`AccountRuntime::removed`], which is
    /// what makes a delete leave the running proxy and the panel without a
    /// restart. Called from a natural
    /// cadence point ([`Self::select_with_group`], [`Self::select_revalidation`],
    /// [`Self::snapshot`]) rather than a dedicated watcher thread — one more
    /// background task is one more thing to keep alive, and every one of
    /// those call sites already runs on every request or every TUI tick.
    ///
    /// Touches ONLY the fields named above. Every other field of `self.config`
    /// — accounts, credentials, tokens, every unmodelled top-level key — is
    /// left exactly as booted: the in-memory snapshot stays authoritative for
    /// those, or this would fight [`Self::persist_now`] / [`Self::persist_tokens`]
    /// / [`Self::persist_disabled`]'s read-modify-write of the SAME file.
    ///
    /// Checks the cached mtime before touching `config_write`. When a file change
    /// needs reloading, takes that lock without waiting and checks mtime again under
    /// the guard, keeping the same read/parse serialization as config writers.
    /// Requests use the last completed snapshot during an in-flight write or fsync;
    /// the next cadence check after the writer releases applies its completed edit.
    ///
    /// A file that cannot be stat'd, read, or parsed **keeps every current
    /// in-memory value** and logs a warning — never blanks the fleet's groups
    /// — and, unlike a successful reload, does NOT advance the remembered
    /// mtime, so a transient or malformed edit self-heals on the very next
    /// natural-cadence check rather than waiting for the file to change
    /// again.
    pub(super) fn reload_groups_if_changed(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        let read_mtime = || match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(mtime) => Some(mtime),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %path.display(),
                    "group reload: could not stat config file, keeping current groups"
                );
                None
            }
        };
        let Some(mtime) = read_mtime() else {
            return;
        };
        if *self
            .groups_reload_mtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            == Some(mtime)
        {
            return;
        }
        let _writing = match self.config_write.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return,
            Err(std::sync::TryLockError::Poisoned(_)) => panic!("config write lock poisoned"),
        };
        let Some(mtime) = read_mtime() else {
            return;
        };
        if *self
            .groups_reload_mtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            == Some(mtime)
        {
            return;
        }
        match config::load(path) {
            Ok(fresh) => {
                self.apply_group_reload(&fresh);
                *self
                    .groups_reload_mtime
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(mtime);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %path.display(),
                    "group reload: config is unreadable/malformed, keeping current groups"
                );
            }
        }
    }

    /// Apply `fresh`'s group membership/settings onto the running server and
    /// log exactly what changed — the line an operator greps for after
    /// editing groups to ask "did it pick up my edit". Silent when nothing
    /// actually differs (the common case: most cadence ticks see an unchanged
    /// file, or a mtime bump from an unrelated write). Called only from
    /// [`Self::reload_groups_if_changed`], under `config_write`.
    ///
    /// The group settings are swapped as one [`GroupPolicy`], and only when
    /// one of them changed. They are applied BEFORE per-account `groups`, so
    /// that by the time this returns and the caller re-tests a pinned
    /// session's hard gate, a session on an account whose group just became
    /// reserved is judged against the fresh reservation — the existing
    /// pin-reclaim-on-reservation test (`pin_reclaim_when_the_pinned_accounts_group_is_reserved`)
    /// extends to the reload path for free, because [`Self::select_with_group`]
    /// re-tests [`Self::account_hard_ok`] against a fresh [`Self::groups`]
    /// snapshot on every call — reservation was never cached PAST one call, only
    /// ACROSS them until this reload existed.
    ///
    /// Matches each running [`AccountRuntime`] to `fresh.accounts` by
    /// [`crate::identity::same_identity`] — the SAME identity rule
    /// [`Self::persist_tokens`] uses — rather than by name, so a config
    /// mid-way through a login churn (a display name changed, an org backfilled)
    /// still reloads the right row instead of silently dropping its groups.
    fn apply_group_reload(&self, fresh: &Config) {
        let fresh_groups = GroupPolicy::from_config(fresh);
        let mut changed: Vec<String> = {
            let mut groups = self.groups.write().unwrap_or_else(PoisonError::into_inner);
            let changed = groups.changes_to(&fresh_groups);
            if !changed.is_empty() {
                *groups = Arc::new(fresh_groups);
            }
            changed
        };
        {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let fresh_control = fresh.control_account.as_ref().map(|c| c.name().to_string());
            for (idx, account) in accounts.iter_mut().enumerate() {
                let probe = crate::identity::probe(
                    account.provider,
                    &account.name,
                    account.account_uuid.clone(),
                    account.org_uuid.clone(),
                    account.org_name.clone(),
                );
                let Some(matched) = fresh
                    .accounts
                    .iter()
                    .find(|a| crate::identity::same_identity(&probe, a))
                else {
                    // The file no longer holds this account: it was deleted.
                    // Parked in memory only (there is no entry left to write
                    // the flag into) and dropped from every snapshot; see
                    // `AccountRuntime::removed`.
                    if !account.removed {
                        account.removed = true;
                        account.disabled = true;
                        changed.push(format!("{}: removed from the config", account.name));
                        // A deleted account is not the control account: its
                        // routing bypasses `disabled` on purpose, so leaving the
                        // index would keep control traffic on the deleted row.
                        // Same lock order as `probeable_indices`: accounts, then
                        // control.
                        let mut control = self
                            .control_idx
                            .write()
                            .unwrap_or_else(PoisonError::into_inner);
                        if *control == Some(idx) {
                            *control = None;
                            changed
                                .push(format!("{}: no longer the control account", account.name));
                        }
                    }
                    continue;
                };
                if account.removed {
                    // Back in the file (an undone hand edit, a login that wrote
                    // the file): the file's row is the truth again, credentials
                    // and bench flag included.
                    account.removed = false;
                    account.disabled = matched.disabled.unwrap_or(false);
                    account.access_token.clone_from(&matched.access_token);
                    account.refresh_token.clone_from(&matched.refresh_token);
                    account.expires_at_ms = matched.expires_at.map(oauth::normalize_expires_at);
                    // The folder too. `login_error` is left as it is, as `status` is: a restored
                    // entry holds whatever tokens it held, and the next refresh decides.
                    account.codex_home.clone_from(&matched.codex_home);
                    changed.push(format!("{}: back in the config", account.name));
                    if fresh_control.as_deref() == Some(account.name.as_str()) {
                        let mut control = self
                            .control_idx
                            .write()
                            .unwrap_or_else(PoisonError::into_inner);
                        if control.is_none() {
                            *control = Some(idx);
                        }
                    }
                }
                let new_groups = matched.groups.clone().unwrap_or_default();
                if new_groups != account.groups {
                    changed.push(format!(
                        "{}: groups {:?} -> {:?}",
                        account.name, account.groups, new_groups
                    ));
                    account.groups = new_groups;
                }
            }

            // Admit an account the file gained since boot (or the last reload)
            // — APP-35. Never touches an account already in `accounts`: one
            // the file LOST was marked `removed` by the loop above, in place,
            // and one that came back was revived there too, so neither is
            // admitted a second time here. The identity match is the same probe the loop above uses, just
            // run in the opposite direction: does any LOADED account already
            // own this fresh entry?
            //
            // Pushed onto `self.config.accounts` in the SAME iteration as
            // `self.accounts` (never one without the other): `Self::account_egress`
            // and every other by-index reader of `self.config` assume the two
            // vectors are index-aligned, because accounts are appended, never
            // removed, from either. Before this fix only the runtime vector grew
            // here, so after one admission `self.accounts[idx]` and
            // `self.config.accounts.get(idx)` named two different accounts (or
            // `idx` was simply out of bounds on the config side) for every index
            // from the admitted account onward: a strict pin on any of them
            // silently failed open (`account_egress` returns `None` on an
            // out-of-range `idx`, and callers read `None` as "no pin").
            let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            let http1_only = config.http1_only;
            for candidate in &fresh.accounts {
                let already_known = accounts.iter().any(|existing| {
                    let probe = crate::identity::probe(
                        existing.provider,
                        &existing.name,
                        existing.account_uuid.clone(),
                        existing.org_uuid.clone(),
                        existing.org_name.clone(),
                    );
                    crate::identity::same_identity(&probe, candidate)
                });
                if already_known {
                    continue;
                }
                changed.push(format!("{}: admitted by reload", candidate.name));
                accounts.push(AccountRuntime::from_config(candidate, http1_only));
                config.accounts.push(candidate.clone());
            }
        }

        if !changed.is_empty() {
            tracing::info!(changes = ?changed, "config reload: picked up a group edit");
        }
    }

    /// Flush account `idx`'s `disabled` flag to the config file, so an account
    /// deliberately benched from the TUI is still benched after a restart.
    ///
    /// Upholds the same three invariants as [`Self::persist_tokens`], with the line
    /// carrying each marked below:
    ///  - **INV1** — the `config_write` guard spans the whole read-modify-write, so
    ///    this write and a concurrent token rotation never interleave. That matters
    ///    more here than the flag does: an interleaved write can clobber a refresh
    ///    token that rotated in between, and a refresh token is single-use, so the
    ///    clobbered account 400s (`invalid_grant`) on its next refresh and is dead
    ///    until re-authed by hand.
    ///  - **INV2** — `save_disabled` runs with `self.config` NOT held. That lock is
    ///    on the per-connection path (`session_affinity_enabled`), so holding it
    ///    across a read + `sync_all` + rename let one keypress stall connection
    ///    setup.
    ///  - **INV3** — the in-memory snapshot is mutated only once the file is known
    ///    to carry the flag. Mutating first (and regardless of the outcome) left
    ///    memory and disk permanently diverged on `NoEntry`/`Ambiguous`/`Err`, with
    ///    nothing to reconcile or retry them — the exact opposite of the guarantee.
    ///
    /// Writes the flag ONLY, via [`config::save_disabled`], never the whole config:
    /// the in-memory `Config` is a boot-time snapshot, so flushing it whole would
    /// revert every setting the user edited while the proxy was running.
    ///
    /// A missing `config_path` (tests, `tcr demo`, `tcr status --probe`) is a
    /// SILENT no-op — those managers must never touch a real config file.
    fn persist_disabled(
        &self,
        idx: usize,
        target: &config::Account,
        disabled: bool,
    ) -> DisablePersist {
        let Some(path) = &self.config_path else {
            return DisablePersist::NoConfigFile;
        };
        // INV1 to INV3 live in `write_through`.
        let outcome = self.write_through(
            path,
            |path| config::save_disabled(path, target, disabled),
            |config| {
                // Resolved by identity, through the one `identity::resolve` that
                // `persist_tokens` and `save_disabled` resolve through, with the
                // provider riding on `target` (`set_disabled` builds it from the row),
                // so all three land on the same record and never on a row of another
                // provider. An unbreakable tie writes nothing: `save_disabled` refuses
                // one too, and memory must not drift onto a record the file was not
                // allowed to touch.
                if let crate::identity::Resolved::One(position) =
                    crate::identity::resolve(config.accounts.iter().enumerate(), target)
                    && let Some(account) = config.accounts.get_mut(position)
                {
                    account.disabled = if disabled { Some(true) } else { None };
                }
            },
        );
        match outcome {
            Ok(config::DisabledWrite::Updated) => {
                tracing::info!(
                    account = %target.name,
                    index = idx,
                    disabled,
                    "persisted account disabled flag to config"
                );
                DisablePersist::Persisted
            }
            Ok(config::DisabledWrite::Unchanged) => {
                tracing::debug!(
                    account = %target.name,
                    index = idx,
                    disabled,
                    "config already carries this disabled state; nothing written"
                );
                DisablePersist::Persisted
            }
            Ok(config::DisabledWrite::NoEntry) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "no config entry carries this account's identity; the disabled flag will NOT survive a restart"
                );
                DisablePersist::NoEntry
            }
            Ok(config::DisabledWrite::Ambiguous) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess which one to flag, so the disabled flag will NOT survive a restart"
                );
                DisablePersist::Ambiguous
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "failed to persist the disabled flag to config"
                );
                DisablePersist::WriteFailed
            }
        }
    }

    /// Sideline account `idx` on a proven-dead credential. Arms the recovery backoff
    /// with it, so even a hand-sidelined row is re-probed rather than stuck forever.
    pub fn mark_error(&self, idx: usize) {
        {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(account) = accounts.get_mut(idx) {
                account.status = AccountStatus::Error;
            }
        }
        self.grow_error_backoff(idx);
    }

    /// Enable/disable account `idx`. Re-enabling clears a stuck error/hold.
    ///
    /// The flag is also PERSISTED (see [`Self::persist_disabled`]) — memory-only
    /// was the bug: a restart silently returned a deliberately benched account to
    /// rotation, because the server writes nothing but credentials back.
    ///
    /// Returns what became of the DURABLE half, because in TUI mode `tracing` is
    /// redirected to a log file: a failed write reported only there leaves the TUI
    /// rendering the account as benched while the bench provably will not survive a
    /// restart. The caller is expected to put [`DisablePersist::warning`] in front of
    /// the person who pressed the key.
    pub fn set_disabled(&self, idx: usize, disabled: bool) -> DisablePersist {
        // Take the identity out under the accounts lock and RELEASE that lock
        // before persisting. The persist path takes the config lock, and holding
        // both at once here would invert the order `warm_targets` reads them in
        // (config, then accounts) — the shape a deadlock is made of.
        let target = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            // A row deleted from the config keeps its slot, parked and unlisted, and
            // `disabled` is all that keeps it out of rotation (`eligible` never reads
            // `removed`), so it answers as a row that no longer exists: enabling it would
            // serve an account the operator deleted, on its in-memory credentials, until a
            // restart. Checked under this write lock, so no reload can mark or revive the
            // row between the check and the write. Reached by a dashboard key drawn before
            // the delete, and by `set_disabled_by_query`, which resolves under a read lock
            // it releases before calling here.
            let Some(account) = accounts.get_mut(idx).filter(|account| !account.removed) else {
                return DisablePersist::NoSuchAccount;
            };
            account.disabled = disabled;
            if !disabled && account.status == AccountStatus::Error {
                account.status = AccountStatus::Active;
                account.rate_limited_until_ms = None;
            }
            // `save_disabled` locates the row by this probe, and
            // `identity::same_identity` never matches a row of the other provider:
            // a probe built with the wrong provider (Anthropic) addresses no Codex
            // row, so a Codex account's bench would never reach disk.
            crate::identity::probe(
                account.provider,
                &account.name,
                account.account_uuid.clone(),
                account.org_uuid.clone(),
                account.org_name.clone(),
            )
        };
        self.persist_disabled(idx, &target, disabled)
    }

    /// Resolve a user-supplied `(query, org)` against the LIVE rotation and apply
    /// `disabled` to whatever it names — the whole operation the control endpoint
    /// performs, kept here so no caller outside this module has to know that a
    /// rotation index is what [`Self::set_disabled`] takes.
    ///
    /// Resolution runs over `self.accounts` — the rotation slots themselves — and
    /// NOT over the boot-time config snapshot, so the index handed to
    /// `set_disabled` is a slot by construction. Matching the config's vector and
    /// then indexing the runtime one would be correct only while the two stay the
    /// same length in the same order forever, which is not a property anything
    /// here enforces.
    ///
    /// The read guard is released before `set_disabled` takes its write guard —
    /// `RwLock` is not reentrant, so holding it across the call would deadlock.
    pub fn set_disabled_by_query(&self, query: &str, disabled: bool) -> SetDisabledOutcome {
        let (idx, name) = {
            let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
            match match_listed(&accounts[..], query) {
                crate::identity::Match::One(idx) => (idx, accounts[idx].name.clone()),
                crate::identity::Match::None => return SetDisabledOutcome::NoMatch,
                crate::identity::Match::Ambiguous(names) => {
                    return SetDisabledOutcome::Ambiguous(names);
                }
            }
        };
        SetDisabledOutcome::Applied {
            name,
            persist: self.set_disabled(idx, disabled),
        }
    }

    /// Resolve `query` against the LIVE rotation and set that account's
    /// `priority` in memory, so the next selection ranks by it. For the
    /// [`crate::proxy::PRIORITY_PATH`] route behind `tcr priority`.
    ///
    /// Memory only, deliberately unlike [`Self::set_disabled`]: `tcr priority`
    /// computes the value from the config file (its `--first`/`--last` are
    /// relative to the file's priorities) and writes the file itself, then
    /// sends the resolved integer here. Resolution is the same
    /// [`crate::identity::match_one`] over the rotation slots that
    /// [`Self::set_disabled_by_query`] uses, for the same reason.
    pub fn set_priority_by_query(&self, query: &str, priority: i64) -> SetPriorityOutcome {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let idx = match match_listed(&accounts[..], query) {
            crate::identity::Match::One(idx) => idx,
            crate::identity::Match::None => return SetPriorityOutcome::NoMatch,
            crate::identity::Match::Ambiguous(names) => {
                return SetPriorityOutcome::Ambiguous(names);
            }
        };
        let account = &mut accounts[idx];
        account.priority = priority;
        SetPriorityOutcome::Applied {
            name: account.name.clone(),
        }
    }

    /// The current control account's rotation index, or `None` when unset.
    pub fn control(&self) -> Option<usize> {
        *self
            .control_idx
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The current control account's name, or `None` when unset / the index no
    /// longer resolves (an account row cannot disappear — `add_account` is
    /// append-only — so the only way this reads `None` with `control()` some
    /// is a bug, not a live scenario; resolving defensively here rather than
    /// indexing avoids a panic either way).
    pub fn control_name(&self) -> Option<String> {
        let idx = self.control()?;
        self.account(idx, |a| a.name.clone())
    }

    /// Whether the control account also takes general inference traffic — the
    /// boot-time `controlPooled` snapshot. Surfaced on the `server started` line
    /// because it is the one routing flag whose cost is an OUTAGE rather than a
    /// slowdown, and an operator reading that line after a restart needs to see
    /// it without opening the config.
    pub const fn control_pooled(&self) -> bool {
        self.control_pooled
    }

    /// The boot-time `controlIdentity` snapshot — see [`crate::control_identity`].
    pub const fn control_identity(&self) -> config::ControlIdentityMode {
        self.control_identity
    }

    /// The identity the control account must be checked against: its `name`
    /// and `accountUuid`. `None` when no control account is set.
    pub fn control_identity_target(&self) -> Option<crate::control_identity::ControlIdentity> {
        let idx = self.control()?;
        self.account(idx, |a| crate::control_identity::ControlIdentity {
            name: a.name.clone(),
            account_uuid: a.account_uuid.clone(),
        })
    }

    /// The resolved-client-identity cache the check reads and fills.
    pub const fn client_identities(&self) -> &crate::control_identity::ClientIdentityCache {
        &self.client_identities
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

    /// Persist ONLY the top-level `controlAccount` key, via
    /// [`config::save_control_account`] — never the whole config (see
    /// [`Self::persist_disabled`]'s doc-comment for why: the in-memory
    /// `Config` is a boot-time snapshot, and flushing it whole would revert
    /// every setting the user edited while the proxy ran — the exact clobber
    /// fixed in `1d978ce`).
    ///
    /// Same three invariants as [`Self::persist_disabled`]:
    ///  - **INV1** — `config_write` spans the whole read-modify-rename, so this
    ///    write and a concurrent token rotation or disabled-flag write can
    ///    never interleave.
    ///  - **INV2** — `save_control_account` runs with `self.config` NOT held,
    ///    so the file I/O never blocks the per-connection config read.
    ///  - **INV3** — the in-memory snapshot (`config.control_account`) is
    ///    mutated only once the file is known to carry the desired state
    ///    (`Updated` or `Unchanged`); a failed write leaves memory and disk in
    ///    the same (divergent-from-the-request) state rather than only disk.
    ///
    /// A missing `config_path` (tests, `tcr demo`, `tcr status --probe`) is a
    /// SILENT no-op — those managers must never touch a real config file.
    fn persist_control(&self, target: Option<config::Account>) -> ControlPersist {
        let Some(path) = &self.config_path else {
            return ControlPersist::NoConfigFile;
        };
        // INV1 to INV3 live in `write_through`.
        let outcome = self.write_through(
            path,
            |path| config::save_control_account(path, target.as_ref()),
            |config| {
                config.control_account = target.as_ref().map(config::ControlAccountRef::from);
            },
        );
        let name = target.as_ref().map(|a| a.name.as_str());
        match outcome {
            Ok(config::ControlWrite::Updated) => {
                tracing::info!(
                    control_account = ?name,
                    "persisted control account to config"
                );
                ControlPersist::Persisted
            }
            Ok(config::ControlWrite::Unchanged) => {
                tracing::debug!(
                    control_account = ?name,
                    "config already names this control account; nothing written"
                );
                ControlPersist::Persisted
            }
            Ok(config::ControlWrite::Ambiguous) => {
                tracing::error!(
                    control_account = ?name,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess, so the control account will NOT survive a restart"
                );
                ControlPersist::Ambiguous
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    control_account = ?name,
                    path = %path.display(),
                    "failed to persist the control account to config"
                );
                ControlPersist::WriteFailed
            }
        }
    }

    /// Set (`idx = Some(_)`) or clear (`idx = None`) the control account by
    /// rotation index, and persist it. Mirrors [`Self::set_disabled`]'s
    /// resolve-identity-then-persist shape: the identity is read out under a
    /// released `accounts` READ lock before `control_idx` is taken, so the two
    /// never nest (same discipline `set_disabled` documents for
    /// `accounts`/`config`).
    fn set_control(&self, idx: Option<usize>) -> ControlPersist {
        let target = idx.and_then(|i| {
            self.account(i, |a| {
                // Carries the account's provider for the reason
                // `set_disabled`'s probe does: `save_control_account`
                // checks the row through `config::locate_account_entry`.
                crate::identity::probe(
                    a.provider,
                    &a.name,
                    a.account_uuid.clone(),
                    a.org_uuid.clone(),
                    a.org_name.clone(),
                )
            })
        });
        {
            let mut control = self
                .control_idx
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            *control = idx;
        }
        self.persist_control(target)
    }

    /// Resolve a user-supplied `query` against the LIVE rotation and set
    /// the control account to whatever it names — the whole operation the
    /// control endpoint performs. `query = None` CLEARS the control account
    /// (there is nothing to resolve, so this never reaches `match_one`).
    ///
    /// Resolution runs over `self.accounts` — never the boot-time config
    /// snapshot — for the same reason [`Self::set_disabled_by_query`] does.
    pub fn set_control_by_query(&self, query: Option<&str>) -> SetControlOutcome {
        let Some(query) = query else {
            return SetControlOutcome::Applied {
                name: None,
                persist: self.set_control(None),
            };
        };
        let (idx, name) = {
            let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
            match match_listed(&accounts[..], query) {
                // Refused here as boot ignores it (see `assemble`): a `match`, so a third
                // provider decides this too instead of inheriting an answer.
                crate::identity::Match::One(idx) => match accounts[idx].provider {
                    crate::provider::Provider::Anthropic => (idx, accounts[idx].name.clone()),
                    crate::provider::Provider::Codex => {
                        return SetControlOutcome::CodexAccount(accounts[idx].name.clone());
                    }
                },
                crate::identity::Match::None => return SetControlOutcome::NoMatch,
                crate::identity::Match::Ambiguous(names) => {
                    return SetControlOutcome::Ambiguous(names);
                }
            }
        };
        SetControlOutcome::Applied {
            name: Some(name),
            persist: self.set_control(Some(idx)),
        }
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

    /// Append a new account to the live rotation and return its index.
    ///
    /// APPEND-ONLY — this is a hard requirement, not a style choice. Pins are
    /// in-memory `(session_key, account_index)`; appending keeps every existing
    /// index valid, whereas inserting or reordering would re-key live sessions
    /// and cold-start their prompt cache, which is the exact cost this whole
    /// feature exists to avoid. Never insert at a position other than the end,
    /// and never reorder either vec.
    ///
    /// Updates both `self.accounts` (the live rotation `AccountRuntime`s) and
    /// `self.config`'s accounts vec (so later identity-resolution in
    /// `persist_tokens` / `persist_disabled` finds the row instead of logging
    /// the "no loaded config account carries this identity" WARN). The two
    /// locks are taken SEQUENTIALLY, never nested — the same discipline
    /// `set_disabled` documents: holding both at once risks inverting the lock
    /// order `warm_targets` reads them in.
    ///
    /// Derives the runtime row via [`AccountRuntime::from_config`], which is
    /// exactly how every other account's runtime state is built at startup, so
    /// an appended account gets its own `refresh_lock` for free — there is no
    /// longer a parallel structure to keep in sync.
    pub fn add_account(&self, account: config::Account) -> usize {
        let http1_only = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .http1_only;
        let runtime = AccountRuntime::from_config(&account, http1_only);
        let idx = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            accounts.push(runtime);
            accounts.len() - 1
        };
        {
            let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            config.accounts.push(account);
        }
        idx
    }

    /// Resolve `account` against the LIVE rotation by identity and either append
    /// it (nothing matched) or replace an existing account's credentials in
    /// place (exactly one matched) — the whole operation `POST /_tcr/accounts`
    /// performs, kept here for the same reason [`Self::set_disabled_by_query`]
    /// is: no caller outside this module needs to know that a rotation index is
    /// what the mutating primitives take.
    ///
    /// Resolution uses [`crate::identity::resolve`]/[`crate::identity::same_identity`]
    /// — the SAME rule the durable half ([`config::save_account`]) and `tcr
    /// login` (`oauth::upsert_account`) already run — never
    /// [`crate::identity::match_one`]. `match_one` goes through
    /// [`crate::identity::Queryable`], which exposes only the name: it
    /// cannot compare `account_uuid`, so it was structurally unable to tell two
    /// different people sharing a display name apart. Unifying on `resolve`
    /// fixes that (a differing `account_uuid` never matches, so a same-named
    /// different person is APPENDED, never overwritten) and it fixes the
    /// live/durable split brain the old rule caused: `resolve` tolerates an
    /// unknown org on either side exactly as the durable write does, so a
    /// legacy no-org row and a submission carrying its own org now agree on
    /// which one account that is, on both halves, every time — not only when
    /// there happen to be two orgs in play.
    ///
    /// A submission carrying no identity fields at all (a bare name) still
    /// finds its row: `same_identity` falls back to name equality when either
    /// side has no uuid, and names are unique, so `resolve` answers that case
    /// on its own. It used to need a `match_one` retry on top, because a bare
    /// email had to find a stored `email (Org)` display name — a shape that
    /// existed only while names were not unique.
    ///
    /// Resolve AND mutate under ONE write-lock acquisition of `self.accounts` —
    /// closing a TOCTOU the old two-lock version had: it resolved under a READ
    /// lock, released it, then took a separate WRITE lock to append. Two
    /// concurrent submissions of the same brand-new identity could both resolve
    /// "no match" before either had appended, and both would append —
    /// duplicating the live row. Holding one write lock across resolve-then-
    /// mutate means the second caller's resolve runs after the first caller's
    /// append is already visible, so it finds the row and updates it instead.
    ///
    /// `config_write` is held across the WHOLE function — resolve, mutate, and
    /// persist — not just the file I/O at the end. The `self.accounts` fix
    /// above only serializes the LIVE half: it guarantees the second of two
    /// concurrent same-identity submissions sees the first one's row and takes
    /// the Updated path instead of duplicating it. It said nothing about the
    /// DURABLE half, which used to take its own `config_write` lock only
    /// inside `persist_added`/`persist_replaced`, well after `self.accounts`
    /// was released — so two callers' persists could still race onto
    /// `config_write` in the OPPOSITE order from the one their live resolves
    /// agreed on, each writing its OWN submitted credentials. The loser's
    /// `persist_replaced` (seeing nothing on disk yet) would append a fresh
    /// entry with ITS tokens; the winner's `persist_added` would then find
    /// that entry and merge ITS OWN tokens over it — leaving the file holding
    /// whichever call happened to reach `config_write` first, independent of
    /// which one actually won the live race. Measured 2/200 over 200 rounds of
    /// two concurrent adds of one identity. Holding this lock across both
    /// halves makes resolve-mutate-persist one atomic unit, so the durable
    /// write order always agrees with the live resolve order.
    ///
    /// This is the one place in `Manager` that holds `config_write` and
    /// `self.accounts` at once — everywhere else they are taken sequentially,
    /// never nested (see `set_disabled`'s and `warm_targets`'s comments on
    /// that discipline). It is safe: `self.accounts` is held only for the
    /// brief in-memory resolve-and-mutate inside this span, never across the
    /// file I/O below, and no other code path holds `self.accounts` and then
    /// reaches for `config_write` or `config` — so this adds one new,
    /// one-directional edge (`config_write` → `self.accounts`) to the lock
    /// order, not a cycle. `persist_added`/`persist_replaced` no longer take
    /// `config_write` themselves — they assume the caller already holds it.
    ///
    /// A resolution that lands on Added needs a freshly-built
    /// [`AccountRuntime`] (a `reqwest::Client`) to push, and building one must
    /// never happen while EITHER lock is held — a panic mid-build would
    /// poison it for every other `.expect(...)` site on that lock,
    /// `config_write` included (`persist_tokens`, `persist_now`'s shutdown
    /// flush, this function's own next call). So `config_write` is held
    /// across each ATTEMPT's resolve/mutate/persist, not necessarily across
    /// the whole call: an attempt that resolves Added with no runtime ready
    /// yet mutates NOTHING, drops both locks, builds, and retries — see the
    /// loop inside the function body for why at most one retry is ever
    /// needed and why it cannot reopen the TOCTOU above.
    pub fn add_or_update_account(&self, mut account: config::Account) -> AddAccountOutcome {
        // The submission's provider rides on the probe, so a Codex submission never
        // resolves to an Anthropic row that shares its name, live or on disk
        // (`identity::same_identity`): it is added beside it, never stamped onto it.
        let target = crate::identity::probe(
            account.provider,
            &account.name,
            account.account_uuid.clone(),
            account.org_uuid.clone(),
            account.org_name.clone(),
        );

        /// What the locked resolve-and-mutate section below produced, so the
        /// durable write can run after `self.accounts`'s lock is released —
        /// `persist_added`/`persist_replaced` do their own file I/O and must
        /// never run under `self.accounts`'s lock (though both still run
        /// under `config_write`, held since this attempt's start — see the N2
        /// doc-comment on `add_or_update_account`).
        enum Resolution {
            Added {
                idx: usize,
            },
            Updated {
                idx: usize,
                name: String,
                /// The row's OWN identity (post-backfill) plus its REAL routing
                /// state (`priority/switch_threshold/disabled`) — never
                /// `identity::probe`'s placeholders, see [`Self::persist_replaced`]
                /// and the F5 note there. Boxed: `Added` is a bare `usize`, and
                /// clippy flags the size gap between variants otherwise.
                target: Box<config::Account>,
            },
            /// The resolve below landed on Added, but there is no
            /// already-built [`AccountRuntime`] to push yet — see the loop
            /// below, which built nothing here on purpose.
            NeedsBuild,
        }

        // Built on demand, never while a lock is held — see the doc-comment
        // above `add_or_update_account`. `None` until the first attempt
        // below discovers it actually needs one.
        let mut speculative_runtime: Option<AccountRuntime> = None;

        loop {
            // N2 — see above: held across THIS ATTEMPT's resolve, mutate and
            // persist so it never interleaves with another concurrent call's.
            // Dropped early, before any mutation, on an attempt that turns out
            // to need a client it doesn't have yet (`NeedsBuild` below).
            let _writing = self
                .config_write
                .lock()
                .unwrap_or_else(PoisonError::into_inner);

            let resolution = {
                let mut accounts = self
                    .accounts
                    .write()
                    .unwrap_or_else(PoisonError::into_inner);

                let probes: Vec<(usize, config::Account)> = accounts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        (
                            i,
                            crate::identity::probe(
                                a.provider,
                                &a.name,
                                a.account_uuid.clone(),
                                a.org_uuid.clone(),
                                a.org_name.clone(),
                            ),
                        )
                    })
                    .collect();

                let matched =
                    match crate::identity::resolve(probes.iter().map(|(i, p)| (*i, p)), &target) {
                        crate::identity::Resolved::One(idx) => Some(idx),
                        crate::identity::Resolved::Many => {
                            // Recompute the loose set for its names: `Resolved::Many`
                            // itself carries none, and this is exactly the set
                            // `resolve` drew from (same predicate, same candidates).
                            let names = accounts
                                .iter()
                                .filter(|a| {
                                    crate::identity::same_identity(
                                        &target,
                                        &crate::identity::probe(
                                            a.provider,
                                            &a.name,
                                            a.account_uuid.clone(),
                                            a.org_uuid.clone(),
                                            a.org_name.clone(),
                                        ),
                                    )
                                })
                                .map(|a| a.name.clone())
                                .collect();
                            return AddAccountOutcome::Ambiguous(names);
                        }
                        crate::identity::Resolved::None => None,
                    };

                match matched.and_then(|idx| accounts.get_mut(idx).map(|row| (idx, row))) {
                    Some((idx, row)) => {
                        row.access_token.clone_from(&account.access_token);
                        row.refresh_token.clone_from(&account.refresh_token);
                        row.expires_at_ms = account.expires_at.map(oauth::normalize_expires_at);
                        // F4: fresh credentials clear a stuck error — `eligible`
                        // hard-gates on `status == Error` — but do NOT clear a
                        // rate-limit hold. That hold expires on its own
                        // (`MAX_RATE_LIMIT_HOLD_SECONDS`) and the next 429 re-arms
                        // it; clearing it here bought nothing but the ability to
                        // race a genuinely-still-limited account back into
                        // rotation early.
                        if row.status == AccountStatus::Error {
                            row.status = AccountStatus::Active;
                        }
                        // A login of an account deleted while this process ran
                        // lands on the deleted row's slot (same identity) and
                        // brings it back as the login asked for it, not parked
                        // the way the delete left it. The durable write below
                        // puts the row back in the file, so the next reload
                        // agrees.
                        if row.removed {
                            row.removed = false;
                            row.disabled = account.disabled.unwrap_or(false);
                        }
                        // A Codex re-login brings a live login, so whatever killed the last one
                        // no longer holds, and the folder it came from (or none, for a login
                        // of tcr's own) is the submission's. Whatever the row's status was, so
                        // neither sits inside the two conditions above.
                        match account.provider {
                            crate::provider::Provider::Anthropic => {}
                            crate::provider::Provider::Codex => {
                                row.login_error = None;
                                row.codex_home.clone_from(&account.codex_home);
                                row.pending_codex_writeback = None;
                            }
                        }
                        // F6: the submitted type always wins — this route only
                        // ever carries a real credential, so a row whose stored
                        // type was stale (e.g. `"api"`) is corrected, or
                        // `refresh_plan` silently never refreshes it again.
                        // Identity fields are BACKFILLED — filled in only where
                        // the row does not already carry a value — mirroring
                        // `oauth::upsert_account`. Backfilling (rather than
                        // ignoring) is what permanently closes the split-brain
                        // trigger: once a legacy no-org row's org is filled in,
                        // it stops being loosely matched by every org variant of
                        // that person and starts requiring an exact org match,
                        // like any other fully-known account.
                        row.account_type = account.account_type.clone();
                        if row.account_uuid.is_none() {
                            row.account_uuid.clone_from(&account.account_uuid);
                        }
                        if row.org_uuid.is_none() {
                            row.org_uuid.clone_from(&account.org_uuid);
                        }
                        if row.org_name.is_none() {
                            row.org_name.clone_from(&account.org_name);
                        }
                        // Plan fields are OVERWRITTEN where the submission has
                        // one, not backfilled like the identity fields above —
                        // a plan changes (an org upgrades, an account joins a
                        // Team) while an identity does not. A `None` still
                        // writes nothing, so a re-add that carried no profile
                        // never blanks a plan we already know. Same rule as
                        // `config::merge_account`'s durable half; see
                        // `config::Account::organization_type`.
                        if account.organization_type.is_some() {
                            row.organization_type.clone_from(&account.organization_type);
                        }
                        if account.rate_limit_tier.is_some() {
                            row.rate_limit_tier.clone_from(&account.rate_limit_tier);
                        }
                        if account.seat_tier.is_some() {
                            row.seat_tier.clone_from(&account.seat_tier);
                        }
                        if account.plan_display_name.is_some() {
                            row.plan_display_name.clone_from(&account.plan_display_name);
                        }
                        // F5: carry the row's REAL routing state, not
                        // `identity::probe`'s None placeholders — see
                        // `persist_replaced`'s doc-comment for the restart-un-bench
                        // this replaces.
                        //
                        // `egress`/`egress_strict` are NOT on `AccountRuntime` (see
                        // `Self::account_egress`'s doc-comment: a pin is operator
                        // intent that only the config file states), so the only
                        // place to read the row's current pin from is
                        // `self.config`, resolved by identity, the same way
                        // `persist_replaced` resolves its own write target, never
                        // by `idx`, because `idx` here is a `self.accounts`
                        // (runtime) index and is not guaranteed to line up with
                        // `self.config.accounts`.
                        let existing_pin = {
                            let config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                            match crate::identity::resolve(
                                config.accounts.iter().enumerate(),
                                &target,
                            ) {
                                crate::identity::Resolved::One(position) => config
                                    .accounts
                                    .get(position)
                                    .map_or_default(config::Account::egress_pin),
                                crate::identity::Resolved::Many
                                | crate::identity::Resolved::None => config::EgressPin::default(),
                            }
                        };
                        let target = config::Account {
                            name: row.name.clone(),
                            account_type: row.account_type.clone(),
                            // The SUBMISSION's provider: `persist_replaced`
                            // writes through `config::locate_account_entry`,
                            // which never matches a row of the other provider,
                            // so a default (Anthropic) here would append an
                            // Anthropic row carrying a Codex credential instead
                            // of updating the Codex row in place.
                            provider: account.provider,
                            account_uuid: row.account_uuid.clone(),
                            org_uuid: row.org_uuid.clone(),
                            org_name: row.org_name.clone(),
                            access_token: String::new(),
                            refresh_token: None,
                            expires_at: None,
                            priority: Some(row.priority),
                            switch_threshold: row.switch_threshold,
                            disabled: row.disabled.then_some(true),
                            // Same "carry the row's REAL routing state" rule as
                            // priority/switch_threshold/disabled above: group labels
                            // are configured routing state, not identity, so a
                            // credential re-add/refresh must not silently clear them.
                            groups: (!row.groups.is_empty()).then(|| row.groups.clone()),
                            // Carried for the same reason as `groups` above,
                            // with one difference: these were just merged from
                            // the submission a few lines up, so `row` already
                            // holds the newest reading and the durable write
                            // gets it rather than the pre-merge value.
                            organization_type: row.organization_type.clone(),
                            rate_limit_tier: row.rate_limit_tier.clone(),
                            seat_tier: row.seat_tier.clone(),
                            plan_display_name: row.plan_display_name.clone(),
                            // Carried from the config row this account already
                            // resolves to, never hardcoded to `Local`/unpinned;
                            // see the `existing_pin` note above. A re-add that
                            // supplied no egress preference of its own must not
                            // silently drop an operator's exit-node pin.
                            egress: existing_pin.egress,
                            egress_strict: existing_pin.strict,
                            // The row's folder, just set from the submission: a row revived
                            // from a delete has no file entry left, so the write appends this
                            // whole `target`, and the folder reaches the file only through it.
                            codex_home: row.codex_home.clone(),
                            extra: serde_json::Map::new(),
                        };
                        Resolution::Updated {
                            idx,
                            name: row.name.clone(),
                            target: Box::new(target),
                        }
                    }
                    None => match speculative_runtime.take() {
                        // No runtime ready yet — mutate nothing, report it, and
                        // let the loop below build one with no lock held before
                        // retrying. Safe to abandon this attempt outright: it
                        // has not touched `accounts`, `self.config`, or disk.
                        None => Resolution::NeedsBuild,
                        Some(mut runtime) => {
                            // A follow-up regression on this arm (distinct from the
                            // TOCTOU fix this function is named for above): an
                            // appended account with no explicit priority must join
                            // the BACK of the fleet — `max(existing priorities) + 1`
                            // — not the 0 that `AccountRuntime::from_config`'s
                            // `unwrap_or(0)` reads from an absent priority, which
                            // would silently promote it to the PRIMARY tier ahead of
                            // the established fleet. Computed from `accounts` (the
                            // LIVE rotation, not the config snapshot) because this
                            // section already holds its write lock and it is
                            // authoritative for what is routing right now. Skipped
                            // when the caller submitted an explicit priority (the
                            // documented new-account case — see `AddAccountRequest`'s
                            // doc-comment in `proxy.rs`), which is never overridden.
                            // Mirrors the identical fix in `config::merge_account`'s
                            // Added arm.
                            if account.priority.is_none() {
                                let next_priority = accounts
                                    .iter()
                                    .map(|a| a.priority)
                                    .max()
                                    .map_or(0, |max| max + 1);
                                account.priority = Some(next_priority);
                                // Built on the PRIOR attempt, before `next_priority`
                                // was known — patch it in rather than rebuilding
                                // (which would call `build_serving_client()` again,
                                // under this same lock, resurrecting exactly the bug
                                // the loop above exists to avoid).
                                runtime.priority = next_priority;
                            }
                            accounts.push(runtime);
                            Resolution::Added {
                                idx: accounts.len() - 1,
                            }
                        }
                    },
                }
            };

            match resolution {
                Resolution::NeedsBuild => {
                    // Both locks are released here (self.accounts already went
                    // out of scope above; config_write is dropped explicitly)
                    // before the one call that can panic — see the doc-comment
                    // above `add_or_update_account`. Nothing was mutated on this
                    // attempt, so retrying from scratch is safe; the next
                    // attempt is guaranteed to have a runtime ready, so it
                    // cannot hit this arm again.
                    drop(_writing);
                    let http1_only = self
                        .config
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .http1_only;
                    speculative_runtime = Some(AccountRuntime::from_config(&account, http1_only));
                    continue;
                }
                Resolution::Added { idx } => {
                    // Still sequential, never nested, with `self.accounts` (which
                    // is already released by this point) — the `self.config` lock
                    // here is a separate, short critical section, same discipline
                    // `add_account` documents. It runs nested inside
                    // `config_write` (held since this attempt's start, see the N2
                    // doc-comment above), which is what makes this whole arm
                    // atomic with respect to another concurrent call.
                    {
                        let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                        config.accounts.push(account.clone());
                    }
                    let name = account.name.clone();
                    let persist = self.persist_added(&account);
                    return AddAccountOutcome::Added { idx, name, persist };
                }
                Resolution::Updated { idx, name, target } => {
                    let persist = self.persist_replaced(idx, &target, &account);
                    return AddAccountOutcome::Updated { idx, name, persist };
                }
            }
        }
    }

    /// Durably persist a newly-appended account (see [`Self::add_account`]) to
    /// the config file. Same INV1 discipline as [`Self::persist_disabled`]: the
    /// whole `config::save_account` read-modify-write runs under
    /// `config_write`, never `self.config` — nothing here needs the in-memory
    /// snapshot, because [`Self::add_account`] already pushed `account` onto it.
    ///
    /// PRECONDITION: unlike every other `config_write` writer in this module,
    /// this one does NOT take the lock itself — its only caller,
    /// [`Self::add_or_update_account`], already holds it across the whole
    /// resolve-mutate-persist sequence (see that function's N2 doc-comment),
    /// and `Mutex` is not reentrant, so locking again here would deadlock.
    /// Never call this without `config_write` already held.
    fn persist_added(&self, account: &config::Account) -> AddPersist {
        let Some(path) = &self.config_path else {
            return AddPersist::NoConfigFile;
        };
        match config::save_account(path, account) {
            Ok(config::AccountWrite::Added) => {
                tracing::info!(account = %account.name, "persisted newly added account to config");
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Updated) => {
                // The on-disk document already carried this identity even
                // though the live rotation did not — an entry added to the
                // file by hand while the proxy ran, or a stale boot snapshot.
                // The fresh credentials still landed, just as an update to
                // that row rather than a new one.
                tracing::info!(
                    account = %account.name,
                    "an on-disk entry already carried this identity; its credentials were updated instead of appending a duplicate"
                );
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Ambiguous) => {
                tracing::warn!(
                    account = %account.name,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess, so the new account will NOT survive a restart"
                );
                AddPersist::Ambiguous
            }
            Ok(config::AccountWrite::Unwritable) => {
                tracing::error!(
                    account = %account.name,
                    path = %path.display(),
                    "the config document's accounts key is not a JSON array; refusing to touch it"
                );
                AddPersist::WriteFailed
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %account.name,
                    path = %path.display(),
                    "failed to persist the newly added account to config"
                );
                AddPersist::WriteFailed
            }
        }
    }

    /// Durably persist replaced credentials for the account at live index `idx`,
    /// identified by `target` — its OWN stored identity (post-backfill), never
    /// the submitted request's (see [`Self::add_or_update_account`] for why).
    ///
    /// `target` also carries the row's REAL `priority`/`switch_threshold`/
    /// `disabled`, not `identity::probe`'s `None` placeholders. That matters
    /// only on the rare path below where no in-memory or on-disk entry carries
    /// this identity and one gets APPENDED from `target`: a placeholder-routing
    /// `target` would append a fresh, un-benched entry for an account that was
    /// deliberately disabled, so it would come back into rotation on the very
    /// next restart — the exact bug `persist_disabled` exists to prevent.
    ///
    /// PRECONDITION: same as [`Self::persist_added`] — does NOT take
    /// `config_write` itself; its only caller, [`Self::add_or_update_account`],
    /// already holds it. Never call this without `config_write` already held.
    fn persist_replaced(
        &self,
        idx: usize,
        target: &config::Account,
        fresh: &config::Account,
    ) -> AddPersist {
        let Some(path) = &self.config_path else {
            return AddPersist::NoConfigFile;
        };
        let mut for_disk = target.clone();
        for_disk.access_token.clone_from(&fresh.access_token);
        for_disk.refresh_token.clone_from(&fresh.refresh_token);
        for_disk.expires_at = fresh.expires_at;
        match fresh.provider {
            crate::provider::Provider::Anthropic => {}
            crate::provider::Provider::Codex => {
                for_disk.extra.remove(config::CODEX_SYNC_PENDING_KEY);
            }
        }

        let outcome = config::save_account(path, &for_disk);
        // INV3, mirroring `persist_disabled`: touch the in-memory snapshot only
        // once the FILE actually carries the new credentials. Without this, the
        // snapshot `persist_tokens`/`persist_now` later clone and write back on
        // shutdown would still hold the OLD (just-replaced) token and stamp it
        // back over the file this call just fixed.
        if matches!(
            outcome,
            Ok(config::AccountWrite::Updated | config::AccountWrite::Added)
        ) {
            let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            match crate::identity::resolve(config.accounts.iter().enumerate(), target) {
                crate::identity::Resolved::One(position) => {
                    if let Some(entry) = config.accounts.get_mut(position) {
                        entry.access_token.clone_from(&for_disk.access_token);
                        entry.refresh_token.clone_from(&for_disk.refresh_token);
                        entry.expires_at = for_disk.expires_at;
                        entry.codex_home.clone_from(&for_disk.codex_home);
                        entry.extra.remove(config::CODEX_SYNC_PENDING_KEY);
                    }
                }
                // No in-memory entry carries this identity either — the disk
                // row `save_account` just appended has no counterpart here.
                // Mirror the append so a later persist can still find it.
                crate::identity::Resolved::None => config.accounts.push(for_disk.clone()),
                // A tie the in-memory snapshot cannot break either; leave it
                // alone rather than guess which entry to overwrite.
                crate::identity::Resolved::Many => {}
            }
        }

        match outcome {
            Ok(config::AccountWrite::Updated) => {
                tracing::info!(account = %target.name, index = idx, "persisted replaced credentials to config");
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Added) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "no config entry carried this account's identity; a fresh entry was appended"
                );
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Ambiguous) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess, so the replaced credentials will NOT survive a restart"
                );
                AddPersist::Ambiguous
            }
            Ok(config::AccountWrite::Unwritable) => {
                tracing::error!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "the config document's accounts key is not a JSON array; refusing to touch it"
                );
                AddPersist::WriteFailed
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "failed to persist replaced credentials to config"
                );
                AddPersist::WriteFailed
            }
        }
    }
}

/// How many one-minute buckets a [`MinuteRing`] holds: the minutes it counts back.
const TRAFFIC_RING_MINUTES: i64 = 10;

/// A count over the last ten minutes, in ten one-minute buckets keyed by the minute
/// (`now_ms / 60_000`). A bucket counts only while its minute is one of the last ten, so a
/// quiet stretch empties the count with no sweep; a bump that lands on a slot holding an
/// older minute resets it first.
#[derive(Debug, Default)]
struct MinuteRing {
    buckets: [(i64, u64); TRAFFIC_RING_MINUTES as usize],
}

impl MinuteRing {
    const fn bump(&mut self, now_ms: i64) {
        let minute = now_ms.div_euclid(60_000);
        let slot = &mut self.buckets[minute.rem_euclid(TRAFFIC_RING_MINUTES) as usize];
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        slot.1 += 1;
    }

    fn count(&self, now_ms: i64) -> u64 {
        let minute = now_ms.div_euclid(60_000);
        self.buckets
            .iter()
            .filter(|(at, _)| *at <= minute && minute - *at < TRAFFIC_RING_MINUTES)
            .map(|(_, n)| n)
            .sum()
    }
}

/// The Codex 429s this process sent itself: their ten-minute count, and the latest one's
/// error `type` and when it was sent.
#[derive(Debug, Default)]
struct CodexRefusals {
    ring: MinuteRing,
    last: Option<(tcr_status_wire::CodexRefusalKind, i64)>,
}

impl Manager {
    /// Count a 429 this process answered a Codex request with itself, under its body's kind.
    /// Fed by `proxy::exhausted_response`'s Codex arms.
    pub fn note_codex_refusal(&self, kind: tcr_status_wire::CodexRefusalKind, now_ms: i64) {
        let mut refused = self
            .codex_refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        refused.ring.bump(now_ms);
        refused.last = Some((kind, now_ms));
    }

    /// Count a Codex request relayed on the client's own credential, unpooled, because it named
    /// a routing of its own (`x-openai-account-routing-override` or `x-openai-fedramp`).
    pub fn note_codex_unpooled(&self, now_ms: i64) {
        self.codex_unpooled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bump(now_ms);
    }

    /// What this process did with Codex requests in the ten minutes before `now_ms`: the 429s
    /// it sent itself, and the requests it relayed unpooled. The latest refusal's type and time
    /// are reported only while the refused count is above zero, so the wire never pairs a zero
    /// count with a stale type.
    ///
    /// Both counts live in memory and a restart empties them: until the next refusal, a
    /// restarted proxy reports none, though codex may still be failing.
    pub fn codex_traffic(&self, now_ms: i64) -> tcr_status_wire::CodexTraffic {
        let (refused_10m, last) = {
            let refused = self
                .codex_refused
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let count = refused.ring.count(now_ms);
            (count, refused.last.filter(|_| count > 0))
        };
        let unpooled_10m = self
            .codex_unpooled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .count(now_ms);
        let (last_refusal, last_refusal_at_ms) = match last {
            Some((error_type, at_ms)) => (Some(error_type), Some(at_ms)),
            None => (None, None),
        };
        tcr_status_wire::CodexTraffic {
            refused_10m,
            last_refusal,
            last_refusal_at_ms,
            unpooled_10m,
        }
    }
}

#[cfg(test)]
mod tests;
