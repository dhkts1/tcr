//! One account's runtime row, its hard status, and how a query names a row. The row's upstream
//! HTTP client is built in `manager::upstream_client`.

use crate::manager::*;

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
    /// `SUSTAINED_5XX_THRESHOLD` (`manager/freshness/probing.rs`).
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
    /// refresh (`manager/freshness/refresh.rs`), cleared by a successful refresh or a re-login of
    /// the row ([`Manager::add_or_update_account`]). Always `None` on an Anthropic row. Not persisted:
    /// a restart forgets it, and the next refresh learns it again.
    pub login_error: Option<String>,
    /// [`config::Account::codex_home`]: the imported login's folder, for guarded token sharing
    /// and the status wire. A re-login replaces it with the submission's.
    pub codex_home: Option<String>,
    /// Latest shared tokens awaiting guarded auth.json publication; never part of status.
    pub(crate) pending_codex_writeback: Option<PendingCodexWriteback>,
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
pub(in crate::manager) fn match_listed(
    accounts: &[AccountRuntime],
    query: &str,
) -> crate::identity::Match {
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
    pub(in crate::manager) fn resolve(self, accounts: &[AccountRuntime]) -> crate::identity::Match {
        match self {
            Self::Name(query) => match_listed(accounts, query),
            Self::Slot(idx) if accounts.get(idx).is_some_and(|row| !row.removed) => {
                crate::identity::Match::One(idx)
            }
            Self::Slot(_) => crate::identity::Match::None,
        }
    }
}

impl AccountRuntime {
    pub(in crate::manager) fn from_config(account: &config::Account, http1_only: bool) -> Self {
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
            pending_codex_writeback: PendingCodexWriteback::from_config(account),
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
