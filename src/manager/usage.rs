//! `Manager` usage methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Fold a response's rate-limit headers into account `idx`'s quota. This runs
    /// on **every** upstream response (including 429s that trigger a rotation), so
    /// it deliberately does NOT touch the request counter — that would double-count
    /// a client request that was retried across accounts (bug #4). Request counting
    /// happens once, in [`Manager::record_served`].
    ///
    /// The account's PROVIDER picks the parser, never the headers: Anthropic's
    /// [`crate::quota::Quota::update_from_headers`] or Codex's
    /// [`crate::quota::Quota::update_from_codex_refusal_at`]. A `match`, so a third provider
    /// decides here instead of inheriting either. `status` is the response's HTTP status,
    /// which the Codex parser reads in place of Anthropic's `unified-status` header (a 2xx
    /// is `allowed`, a refusal 429 `rejected`); the Anthropic parser ignores it.
    ///
    /// A response that CARRIED the 5h window also latches
    /// [`AccountRuntime::quota_known`], on the same terms and for the same reason
    /// [`Self::apply_usage`] does: those headers are a first-hand read of this
    /// account's session window, so an account serving live traffic must not go on
    /// reading as "we have never seen this account's quota" — it plainly is not
    /// true, and keep-warm's gate is asking exactly that question. A response
    /// WITHOUT it latches nothing: an error page tells us nothing about the window,
    /// and silence must never be mistaken for evidence.
    ///
    /// `probe_status` is deliberately still untouched here — that field is about
    /// probe HEALTH, which a served response says nothing about.
    ///
    /// Returns whether THIS response's headers carried the 5h window — the same
    /// signal either parser returns, passed through
    /// so a caller like [`Manager::warm_account`] can tell "evidence read" apart
    /// from "silence" on this specific response, not just the account's
    /// once-ever `quota_known` latch. Deliberately NOT `#[must_use]`: most
    /// callers (every served-response path in `proxy.rs`, most existing tests)
    /// only care about the side effect of folding the headers in, and forcing
    /// every one of them to consume the bool would be pure churn unrelated to
    /// this fix.
    pub fn update_quota(
        &self,
        idx: usize,
        headers: &reqwest::header::HeaderMap,
        status: u16,
    ) -> bool {
        self.update_quota_at(idx, headers, status, OffsetDateTime::now_utc())
    }

    /// Fold a response and parse its refusal on the same explicit clock.
    pub fn update_quota_at(
        &self,
        idx: usize,
        headers: &reqwest::header::HeaderMap,
        status: u16,
        now: OffsetDateTime,
    ) -> bool {
        let refusal = (status == 429)
            .then(|| crate::quota::CodexQuotaRefusal::parse(headers, &[], now))
            .flatten();
        self.update_quota_with_codex_refusal_at(idx, headers, status, refusal.as_ref(), now)
    }

    pub(crate) fn update_quota_with_codex_refusal(
        &self,
        idx: usize,
        headers: &reqwest::header::HeaderMap,
        status: u16,
        refusal: Option<&crate::quota::CodexQuotaRefusal>,
    ) -> bool {
        self.update_quota_with_codex_refusal_at(
            idx,
            headers,
            status,
            refusal,
            OffsetDateTime::now_utc(),
        )
    }

    fn update_quota_with_codex_refusal_at(
        &self,
        idx: usize,
        headers: &reqwest::header::HeaderMap,
        status: u16,
        refusal: Option<&crate::quota::CodexQuotaRefusal>,
        now: OffsetDateTime,
    ) -> bool {
        let (newly_known, read_five_hour) = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            match accounts.get_mut(idx) {
                Some(account) => {
                    let read_five_hour = match account.provider {
                        crate::provider::Provider::Anthropic => {
                            account.quota.update_from_headers_at(headers, now)
                        }
                        crate::provider::Provider::Codex => account
                            .quota
                            .update_from_codex_refusal_at(headers, status, refusal, now),
                    };
                    let flipped = read_five_hour && !account.quota_known;
                    account.quota_known |= read_five_hour;
                    // Any response that DOES carry the 5h window is proof this
                    // account is not stuck returning header-less responses —
                    // resets keep-warm's miss counter regardless of whether this
                    // particular update was the warm path or a served request.
                    // Also clears the cooldown timestamp so no stale retry-after
                    // lingers on a row that just proved it does not need one.
                    if read_five_hour {
                        account.consecutive_warms_without_evidence = 0;
                        account.warm_evidence_retry_after_ms = None;
                    }
                    (flipped, read_five_hour)
                }
                None => (false, false),
            }
        };
        // Edge-triggered, with the accounts lock RELEASED — identical to
        // `apply_usage`, whose comment explains both halves.
        if newly_known {
            self.warm_wake.notify_one();
        }
        read_five_hour
    }

    /// Add token usage to account `idx` (the true serving account — bug #3).
    /// `input_tokens` already includes `cache_creation_input_tokens` +
    /// `cache_read_input_tokens` (summed by the caller — bug #4), and remains the
    /// quota counter. The `cache_read` / `cache_creation` components are ALSO
    /// tracked separately (they are a SUBSET of `input_tokens`, not additional
    /// quota) so the prompt-cache hit ratio is visible per account without
    /// changing what counts against quota.
    pub fn update_usage(
        &self,
        idx: usize,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
    ) {
        // The caller of this shape only has the FLAT cache-creation count, so
        // the whole of it is booked as a 5-minute write. `input_tokens` is kept
        // VERBATIM as the quota figure — `account.input_tokens` grows by
        // exactly what was passed, as it always has — and only the base input
        // used for PRICING is derived from it; see
        // [`crate::usage::UsageRecord::from_quota_input`] for what happens when
        // a caller's components exceed its own total.
        self.record_usage(
            idx,
            crate::usage::UsageRecord::from_quota_input(
                crate::now_ms(),
                None,
                None,
                input_tokens,
                cache_creation,
                0,
                cache_read,
                output_tokens,
            ),
        );
    }

    /// **The single entry point for a served request's usage.** Every token this
    /// fleet spends passes through here exactly once, and nothing else may
    /// increment the four cumulative counters.
    ///
    /// It does two separable things, in this order:
    ///
    /// 1. Adds to the cumulative per-account counters, whose meanings are
    ///    UNCHANGED by this function existing — `input_tokens` is still the
    ///    quota counter with cache creation and cache reads folded in
    ///    ([`crate::usage::UsageRecord::input_total`] reproduces it exactly),
    ///    and `cache_creation_tokens` is still both TTLs together. `status.rs`
    ///    explains why an existing wire field may never quietly change meaning.
    /// 2. Records the same request in the usage tracker, which is where the
    ///    model, the 5m/1h cache split, the cost and the time buckets live.
    ///
    /// The accounts write lock is RELEASED before step 2 — the two locks are
    /// never held together, the same discipline `update_quota` follows for the
    /// warm-wake notify.
    ///
    /// Deliberately does NOT touch the request counter: a client request retried
    /// across accounts reaches this once per upstream RESPONSE, and counting
    /// requests here is bug #4. That count happens in [`Manager::record_served`].
    pub fn record_usage(&self, idx: usize, mut record: crate::usage::UsageRecord) {
        let (name, (uuid, org)) = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            match accounts.get_mut(idx) {
                Some(account) => {
                    // The serving account's provider, which files the ledger line (`p`) and
                    // puts a Codex record in the weekly window. Read under the lock that
                    // already resolves its identity.
                    record.provider = account.provider;
                    account.input_tokens += record.input_total();
                    account.output_tokens += record.output;
                    account.cache_read_tokens += record.cache_read;
                    account.cache_creation_tokens += record.cache_creation();
                    // The identity fields are read by the LEDGER LINE and by
                    // nothing else, so they are allocated only when there is a
                    // ledger to read them — an embedder or a test with no usage
                    // directory pays nothing. Gated on `is_attached` rather than
                    // `is_persisting`: a ledger whose writes are failing right
                    // now is still queueing, and a line queued without its
                    // identity resolves onto the wrong account when it lands.
                    let identity = if self.usage.is_attached() {
                        (
                            crate::identity::uuid_key_of(account.account_uuid.as_deref())
                                .map(str::to_string),
                            crate::identity::org_key_of(
                                account.org_uuid.as_deref(),
                                account.org_name.as_deref(),
                            )
                            .map(str::to_string),
                        )
                    } else {
                        (None, None)
                    };
                    (account.name.clone(), identity)
                }
                // An index with no account is not this function's to complain
                // about — the caller's rotation loop already resolved it, and a
                // configuration reloaded underneath is not a reason to record
                // the tokens against somebody else.
                None => return,
            }
        };
        self.usage
            .record(idx, &record, &name, uuid.as_deref(), org.as_deref());
    }

    /// Observe the successful response's own model, independently of usage bookkeeping.
    /// A request-only model and a response that names no model add no catalogue id.
    pub(crate) fn record_response_model(
        &self,
        provider: crate::provider::Provider,
        model: Option<&str>,
    ) {
        if let Some(model) = model {
            self.routing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record_model(provider, model);
        }
    }

    /// Note an upstream error of `kind` on `provider` now, for the routing document's
    /// `lastError` ([`crate::routing`]). Observability only: nothing in selection reads it,
    /// and no retry, hold or gate follows from it.
    pub fn record_upstream_error(
        &self,
        provider: crate::provider::Provider,
        kind: crate::routing::ErrorKind,
    ) {
        self.record_upstream_error_at(provider, kind, OffsetDateTime::now_utc());
    }

    /// Record an upstream error on the caller's observation clock.
    pub fn record_upstream_error_at(
        &self,
        provider: crate::provider::Provider,
        kind: crate::routing::ErrorKind,
        now: OffsetDateTime,
    ) {
        self.routing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record_error(provider, kind, odt_to_ms(now));
    }

    /// What the routing document reports for `provider` as of `now_ms`: its last upstream
    /// error as `(kind, at_ms, count of that kind in the ten minutes up to now_ms)`, and every
    /// model id it served this boot.
    pub fn routing_observed(
        &self,
        provider: crate::provider::Provider,
        now_ms: i64,
    ) -> (
        Option<(crate::routing::ErrorKind, i64, u32)>,
        std::collections::BTreeSet<String>,
    ) {
        let seen = self.routing.lock().unwrap_or_else(PoisonError::into_inner);
        (seen.last_error(provider, now_ms), seen.model_ids(provider))
    }

    /// Attach `dir` as the usage ledger: replay today's and yesterday's files
    /// back through the recording path so a restart does not lose the day (and
    /// the week before them too when the fleet holds a Codex account, so its
    /// weekly window survives as well), prune day files past `retention_days`,
    /// then start appending. Each account's provider goes to the ledger with its
    /// identity: it decides whether the week is replayed.
    ///
    /// Only the SERVING process calls this. Returns what the replay found so the
    /// caller can log whether the day was restored or genuinely started at zero.
    pub fn attach_usage_ledger(
        &self,
        dir: PathBuf,
        retention_days: u32,
    ) -> crate::usage::ReplayReport {
        let accounts = self.ledger_accounts();
        self.usage
            .attach_ledger(dir, retention_days, &accounts, crate::now_ms())
    }

    /// Replace today's usage totals with a fresh replay of the ledger files —
    /// see [`crate::usage::UsageTracker::rebuild_from_ledger`]. Called once a
    /// listener handoff's predecessor has finished writing to them. Blocks on
    /// file I/O and holds up recording while it runs: call it off the async
    /// workers.
    pub fn rebuild_usage_from_ledger(
        &self,
    ) -> Result<crate::usage::ReplayReport, crate::usage::RebuildSkipped> {
        let accounts = self.ledger_accounts();
        self.usage.rebuild_from_ledger(&accounts, crate::now_ms())
    }

    /// Every account as the ledger resolves lines to it, in rotation order.
    fn ledger_accounts(&self) -> Vec<crate::usage::LedgerAccount> {
        // The whole identity, not the name: the same email in two orgs is two
        // accounts (`identity.rs`) and a name alone cannot tell them apart,
        // while the org key alone moves under a re-login's backfill and strands
        // every line written before it.
        self.accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|a| crate::usage::LedgerAccount {
                name: a.name.clone(),
                uuid: crate::identity::uuid_key_of(a.account_uuid.as_deref()).map(str::to_string),
                org: crate::identity::org_key_of(a.org_uuid.as_deref(), a.org_name.as_deref())
                    .map(str::to_string),
                org_name: a.org_name.clone().filter(|s| !s.is_empty()),
                provider: a.provider,
            })
            .collect()
    }

    /// This account's usage row as of `now_ms` (see [`crate::usage::UsageTracker::row`]), plus a
    /// Codex row's weekly window. `seven_day` is the weekly reset and the window's length, both
    /// in milliseconds, and `None` leaves `seven_day_window` absent.
    pub(super) fn usage_row(
        &self,
        idx: usize,
        now_ms: i64,
        five_hour_reset_ms: Option<i64>,
        seven_day: Option<(i64, i64)>,
    ) -> tcr_status_wire::UsageRow {
        let mut row = self.usage.row(idx, now_ms, five_hour_reset_ms);
        row.seven_day_window = seven_day.and_then(|(reset_ms, length_ms)| {
            self.usage
                .seven_day_window(idx, now_ms, reset_ms, length_ms)
        });
        row
    }

    /// The configured usage-ledger retention, in days
    /// ([`crate::config::Config::usage_retention_days`]). Read from the config
    /// this manager booted with, like every other boot-time knob.
    pub fn usage_retention_days(&self) -> u32 {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .usage_retention_days
    }

    /// Whether a usage ledger is attached AND writing — i.e. whether today's
    /// totals will survive a restart. False on every offline/test manager, and
    /// false again the moment the ledger's writes start failing.
    pub fn usage_is_persisting(&self) -> bool {
        self.usage.is_persisting()
    }

    /// Ledger lines dropped for a full writer queue — 0 unless the volume
    /// holding `~/.cache` stalled long enough to back the writer up.
    pub fn usage_dropped_lines(&self) -> u64 {
        self.usage.dropped_lines()
    }

    /// Drain the usage ledger and stop its writer, inside `budget`, so the day
    /// file holds every request this process served — see
    /// [`crate::usage::UsageTracker::shutdown_ledger`]. Called once, from
    /// `ServerHandle::shutdown_within`; a restart is the routine event here
    /// (every TcrBar update forces one), so the lines still in the queue at that
    /// moment are the ones a restart used to lose.
    pub fn shutdown_usage_ledger(
        &self,
        budget: std::time::Duration,
    ) -> crate::usage::LedgerShutdown {
        self.usage.shutdown_ledger(budget)
    }

    /// Test-only: stall the usage-ledger writer, so a shutdown through this
    /// manager takes its whole budget — see
    /// [`crate::usage::UsageTracker::wedge_writer_for_test`]. Dropping the
    /// returned receiver frees the writer.
    #[cfg(test)]
    pub(crate) fn wedge_usage_writer_for_test(&self) -> std::sync::mpsc::Receiver<()> {
        self.usage.wedge_writer_for_test()
    }

    /// Fold a background probe's usage into account `idx`'s quota windows and latch
    /// [`AccountRuntime::quota_known`].
    ///
    /// This is the ONLY place the latch is set, and that is the whole point: it runs
    /// exclusively on the `Ok` arm of [`Manager::probe_account`], so every probe
    /// FAILURE — `Error`, `Timeout`, `RateLimited` — leaves the latch (and therefore
    /// the account's keep-warm eligibility) untouched. Note it latches even when the
    /// endpoint reported no 5h bucket at all: "we have read this account's quota" is
    /// about the READ, not about which windows came back.
    ///
    /// Deliberately does NOT reset [`AccountRuntime::consecutive_warms_without_evidence`],
    /// even though this can latch [`AccountRuntime::quota_known`] just like a
    /// header-bearing served response does. A first cut of this fix reset it
    /// here unconditionally and reopened the exact loop the counter exists to
    /// close: `probeable_indices` (unlike [`Self::warm_targets`]) does not check
    /// this counter, so the background prober keeps visiting an excluded account
    /// on its OWN schedule — reset it on every successful probe and a
    /// persistently header-less account gets re-warmed once per probe cycle
    /// forever, worse than the original unbounded-warm bug this counter fixed.
    /// A probe finding no 5h bucket also says nothing about whether the next
    /// WARM response would carry one — it is a different, zero-spend endpoint,
    /// not a rehearsal of the warm request the counter is bounding. Recovery
    /// from the exclusion is [`Manager::record_warm_without_evidence`]'s
    /// [`AccountRuntime::warm_evidence_retry_after_ms`] cooldown instead — a
    /// flat wall-clock wait, decoupled from probe cadence, so the retry rate is
    /// bounded by policy rather than by how often the fleet happens to probe.
    pub fn apply_usage(&self, idx: usize, usage: &Usage) {
        self.apply_usage_at(idx, usage, OffsetDateTime::now_utc());
    }

    /// Fold a probe at an explicit observation time.
    pub fn apply_usage_at(&self, idx: usize, usage: &Usage, now: OffsetDateTime) {
        let newly_known = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            match accounts.get_mut(idx) {
                Some(account) => {
                    let was_codex_refused = match account.provider {
                        crate::provider::Provider::Anthropic => false,
                        crate::provider::Provider::Codex => {
                            account.quota.status.as_deref() == Some("rejected")
                        }
                    };
                    account.quota.apply_usage_at(usage, now);
                    if was_codex_refused
                        && account.quota.status.as_deref() != Some("rejected")
                        && account.hold_cause == HoldCause::RateLimit
                    {
                        account.rate_limited_until_ms = None;
                        if account.status == AccountStatus::Throttled {
                            account.status = AccountStatus::Active;
                        }
                    }
                    let flipped = !account.quota_known;
                    account.quota_known = true;
                    flipped
                }
                None => false,
            }
        };
        // Wake the keep-warm loop only on the false→true flip — at most once per
        // account per process, so the loop can never be spun by a steady-state
        // probe cadence. Signalled with the accounts lock RELEASED: the loop's
        // first act on waking is `warm_targets()`, which takes that same lock.
        if newly_known {
            self.warm_wake.notify_one();
        }
        // One greppable sample per successful probe, so a day of logs joined
        // with the usage ledger (`scripts/fit-quota-weights.py`) can fit how
        // each token kind weighs against the 5h window. Nothing else records
        // utilization over time: the headers and the usage endpoint both report
        // whole percents, so a single request cannot move the reading and a
        // controlled probe would cost several percent of a window; the fit
        // reads it off real traffic for free. `-1` is "not reported", never a
        // fabricated zero. Logged with the accounts lock released.
        let name = self.account_name(idx).unwrap_or_default();
        let util = |bucket: Option<crate::probe::UsageBucket>| {
            bucket.and_then(|b| b.utilization).unwrap_or(-1.0)
        };
        let reset = |bucket: Option<crate::probe::UsageBucket>| {
            bucket.and_then(|b| b.reset_at_ms).unwrap_or(-1)
        };
        tracing::info!(
            account = %name,
            five_hour = util(usage.five_hour),
            five_hour_reset_ms = reset(usage.five_hour),
            seven_day = util(usage.seven_day),
            seven_day_reset_ms = reset(usage.seven_day),
            seven_day_oi = util(usage.seven_day_oi),
            "quota-sample: usage probe read"
        );
    }

    /// Hold account `idx` out of rotation for `seconds` (a 429 quota rejection).
    /// The hold is clamped to at most [`MAX_RATE_LIMIT_HOLD_SECONDS`] so a huge
    /// `retry-after` (Anthropic weekly caps report hours) can never pin a healthy
    /// account out for that long with no revalidation path: it is re-selected
    /// after the bounded hold, and either serves or is re-held. Durable
    /// exhaustion is separately kept out of rotation by the
    /// learned quota utilization, not by this short-term hold.
    /// Record a MODEL-SCOPED quota rejection for account `idx` — upstream said
    /// `rejected` on the `7d_oi` scope while both shared scopes stayed allowed
    /// (issue #178). The deliberate contrast with [`Self::mark_rate_limited`] is
    /// that this is not an account-level hold at all: it writes the rejection
    /// into the learned `seven_day_oi` window, which is the durable-exhaustion
    /// channel the doc-comment above names, and which the selector already reads
    /// for exactly this model class ([`crate::quota::Quota::model_weekly_exhausted`],
    /// `Manager::model_blocked`). So the account keeps serving every other model,
    /// and the rejection frees at its own reset with no second deadline to
    /// reconcile. See [`crate::quota::Quota::reject_model_weekly`].
    pub fn mark_model_weekly_rejected(&self, idx: usize, headers: &reqwest::header::HeaderMap) {
        self.mark_model_weekly_rejected_at(idx, headers, OffsetDateTime::now_utc());
    }

    /// Record a scoped rejection on the caller's clock.
    pub fn mark_model_weekly_rejected_at(
        &self,
        idx: usize,
        headers: &reqwest::header::HeaderMap,
        now: OffsetDateTime,
    ) {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(account) = accounts.get_mut(idx) {
            account.quota.reject_model_weekly_at(headers, now);
            tracing::info!(
                account = %account.name,
                "model-scoped weekly quota rejection recorded"
            );
        }
    }

    pub fn mark_rate_limited(&self, idx: usize, seconds: i64) {
        self.mark_rate_limited_at(idx, seconds, OffsetDateTime::now_utc());
    }

    /// Arm the bounded rate-limit hold at an explicit time.
    pub fn mark_rate_limited_at(&self, idx: usize, seconds: i64, now: OffsetDateTime) {
        let hold = seconds.clamp(0, MAX_RATE_LIMIT_HOLD_SECONDS);
        let until = odt_to_ms(now) + hold * 1000;
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(account) = accounts.get_mut(idx) {
            account.status = AccountStatus::Throttled;
            account.rate_limited_until_ms = Some(until);
            account.hold_cause = HoldCause::RateLimit;
            tracing::info!(account = %account.name, hold_seconds = hold, "rate limited");
        }
    }

    /// A measured future quota deadline may be days away. Without one, cap the
    /// retry delay so a spend cap cannot bench an account until an unrelated reset.
    /// A successful probe also releases the hold when it reads the refusal as over.
    pub fn mark_codex_quota_limited(&self, idx: usize, seconds: i64) {
        self.mark_codex_quota_limited_at(idx, seconds, OffsetDateTime::now_utc());
    }

    /// Arm the quota hold using the caller's time and learned reset.
    pub fn mark_codex_quota_limited_at(&self, idx: usize, seconds: i64, now: OffsetDateTime) {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(account) = accounts.get_mut(idx) {
            let hold = account
                .quota
                .codex
                .refusal_reset
                .filter(|reset| *reset > now)
                .map_or_else(
                    || seconds.clamp(1, MAX_RATE_LIMIT_HOLD_SECONDS),
                    |reset| reset.unix_timestamp().saturating_sub(now.unix_timestamp()),
                );
            account.status = AccountStatus::Throttled;
            account.rate_limited_until_ms =
                Some(odt_to_ms(now).saturating_add(hold.saturating_mul(1000)));
            account.hold_cause = HoldCause::RateLimit;
        }
    }

    /// Hold account `idx` out of rotation for [`REFUSED_HOLD_SECONDS`] after an
    /// upstream 403 refused our credential. Leaves the status `Active`: the
    /// terminal `Error` is wrong for a 403 (see the 403 arm in proxy.rs), and
    /// `Throttled` would claim a rate limit. Selection skips any live
    /// `rate_limited_until_ms` whatever the status, so the hold binds the same.
    pub fn mark_refused(&self, idx: usize) {
        self.mark_refused_at(idx, OffsetDateTime::now_utc());
    }

    /// Arm a credential refusal hold on the caller's clock.
    pub fn mark_refused_at(&self, idx: usize, now: OffsetDateTime) {
        let until = odt_to_ms(now) + REFUSED_HOLD_SECONDS * 1000;
        let provider = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(account) = accounts.get_mut(idx) else {
                return;
            };
            account.rate_limited_until_ms = Some(until);
            account.hold_cause = HoldCause::Refused;
            tracing::info!(
                account = %account.name,
                hold_seconds = REFUSED_HOLD_SECONDS,
                "refused by upstream 403, held out of rotation"
            );
            account.provider
        };
        self.record_upstream_error_at(provider, crate::routing::ErrorKind::Auth, now);
    }

    /// Clear a hold after live proof it no longer binds (any non-429): a
    /// rate-limit hold, or a 403 refusal hold, since the account just answered.
    pub fn clear_rate_limited(&self, idx: usize) {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(account) = accounts.get_mut(idx) {
            if account.status == AccountStatus::Throttled {
                account.status = AccountStatus::Active;
                account.rate_limited_until_ms = None;
            } else if account.hold_cause == HoldCause::Refused {
                account.rate_limited_until_ms = None;
            }
            account.hold_cause = HoldCause::RateLimit;
        }
    }

    /// Record that account `idx` served a stream that failed to complete cleanly
    /// (`kind` is either an Anthropic `error.error.type`, e.g. `"overloaded_error"`,
    /// from an in-band SSE `error` event, or the fixed string `"truncated"` for a
    /// stream that hit EOF without Anthropic's `message_stop` terminator) — a
    /// turn the client saw as a 200 that must not read as a clean serve.
    /// OBSERVABILITY ONLY: this warns and updates a decayed counter and
    /// the account's last-error label; it deliberately does NOT call
    /// `mark_error` (that condemns terminally — see its doc comment on the 2026-07-17
    /// incident) or `mark_rate_limited` (an overloaded account is not over quota —
    /// see [`Self::mark_rate_limited`]'s doc comment), and nothing in
    /// `select.rs`'s `eligible`/`hard_ok`/`account_terminal_gate` reads the
    /// counter this writes — wiring it into routing is a separate, later,
    /// premortem'd change. The kind it names, if any
    /// ([`crate::routing::ErrorKind::of_stream`]), becomes the provider's last error in the
    /// routing document, which is observability too.
    pub fn record_stream_error(&self, idx: usize, kind: &str) {
        self.record_stream_error_at(idx, kind, OffsetDateTime::now_utc());
    }

    /// Record both stream observations on the same explicit clock.
    pub fn record_stream_error_at(&self, idx: usize, kind: &str, now: OffsetDateTime) {
        let now_ms = odt_to_ms(now);
        let provider = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(account) = accounts.get_mut(idx) else {
                return;
            };
            tracing::warn!(
                account = %account.name,
                index = idx,
                error_type = kind,
                "stream failed to complete cleanly — truncated turn, not a clean serve"
            );
            account.stream_error_times_ms.push_back(now_ms);
            prune_stream_errors(&mut account.stream_error_times_ms, now_ms);
            account.last_stream_error = Some(kind.to_string());
            account.provider
        };
        if let Some(error) = crate::routing::ErrorKind::of_stream(kind) {
            self.record_upstream_error_at(provider, error, now);
        }
    }
}

/// Prune `times` to [`STREAM_ERROR_WINDOW_MS`] and hard-cap at
/// [`STREAM_ERROR_CAP`] entries, oldest first. The mutating half of pruning —
/// called on INSERT, where the caller already holds the accounts write lock.
fn prune_stream_errors(times: &mut VecDeque<i64>, now_ms: i64) {
    while times
        .front()
        .is_some_and(|&t| now_ms - t > STREAM_ERROR_WINDOW_MS)
    {
        times.pop_front();
    }
    while times.len() > STREAM_ERROR_CAP {
        times.pop_front();
    }
}

/// The account's DECAYED stream-error count as of `now_ms`: entries within
/// [`STREAM_ERROR_WINDOW_MS`]. The read-side half of pruning — [`Manager::snapshot`]
/// holds only the accounts READ lock, so it cannot pop stale entries the way
/// [`prune_stream_errors`] does on insert; this counts without mutating instead,
/// which is equivalent for display purposes (the next insert physically prunes).
pub(super) fn stream_error_count(times: &VecDeque<i64>, now_ms: i64) -> usize {
    times
        .iter()
        .filter(|&&t| now_ms - t <= STREAM_ERROR_WINDOW_MS)
        .count()
}

#[cfg(test)]
mod routing_observation_tests {
    //! What the routing document reports per provider is written where a served request,
    //! a failed stream and a refused credential are already recorded.
    use super::*;
    use crate::config::Account;
    use crate::provider::Provider;
    use crate::routing::ErrorKind;
    use std::collections::BTreeSet;

    /// One `anthropic` account (slot 0) and one `codex` account (slot 1).
    fn manager() -> Arc<Manager> {
        Manager::new(
            Config::fixture(vec![
                Account::fixture("alice@example.com"),
                Account {
                    provider: Provider::Codex,
                    ..Account::fixture("bob@example.com/codex")
                },
            ]),
            Arc::new(oauth::NoRefresh),
            Arc::new(LiveUsageProber::new()),
            Arc::new(LiveWarmer::new()),
            None,
        )
    }

    fn last_kind(manager: &Manager, provider: Provider) -> Option<ErrorKind> {
        manager
            .routing_observed(provider, crate::now_ms())
            .0
            .map(|(kind, _, _)| kind)
    }

    /// **Response observations are filed under their provider**, independently of usage.
    /// A usage slot with no account still records nothing.
    #[test]
    fn a_served_model_is_seen_under_its_serving_accounts_provider() {
        let manager = manager();
        let record = |model: &str| {
            crate::usage::UsageRecord::from_quota_input(
                crate::now_ms(),
                Some(model.to_string()),
                None,
                10,
                0,
                0,
                0,
                5,
            )
        };
        manager.record_response_model(Provider::Anthropic, Some("claude-opus-5-5"));
        manager.record_usage(0, record("claude-opus-5-5"));
        manager.record_response_model(Provider::Codex, Some("gpt-5.1-codex"));
        manager.record_usage(1, record("gpt-5.1-codex"));
        manager.record_usage(9, record("served-by-no-account"));
        let now = crate::now_ms();
        assert_eq!(
            manager.routing_observed(Provider::Anthropic, now).1,
            BTreeSet::from(["claude-opus-5-5".to_string()])
        );
        assert_eq!(
            manager.routing_observed(Provider::Codex, now).1,
            BTreeSet::from(["gpt-5.1-codex".to_string()])
        );
    }

    #[test]
    fn explicit_errors_share_the_replay_clock() {
        let manager = manager();
        let now = OffsetDateTime::from_unix_timestamp(1_767_225_600).expect("fixed epoch");
        manager.record_upstream_error_at(Provider::Anthropic, ErrorKind::Quota, now);
        assert_eq!(
            manager
                .routing_observed(Provider::Anthropic, odt_to_ms(now))
                .0,
            Some((ErrorKind::Quota, odt_to_ms(now), 1))
        );
        let stream_at = now + time::Duration::seconds(1);
        manager.record_stream_error_at(0, "overloaded_error", stream_at);
        assert_eq!(
            manager
                .routing_observed(Provider::Anthropic, odt_to_ms(stream_at))
                .0,
            Some((ErrorKind::Overloaded, odt_to_ms(stream_at), 1))
        );
        assert_eq!(
            manager.accounts.read().expect("accounts lock")[0]
                .stream_error_times_ms
                .back(),
            Some(&odt_to_ms(stream_at))
        );
        let refused_at = now + time::Duration::seconds(2);
        manager.mark_refused_at(0, refused_at);
        assert_eq!(
            manager
                .routing_observed(Provider::Anthropic, odt_to_ms(refused_at))
                .0,
            Some((ErrorKind::Auth, odt_to_ms(refused_at), 1))
        );
        manager.record_stream_error_at(usize::MAX, "truncated", refused_at);
        assert_eq!(
            manager
                .routing_observed(Provider::Anthropic, odt_to_ms(refused_at))
                .0,
            Some((ErrorKind::Auth, odt_to_ms(refused_at), 1))
        );
    }

    /// **A failed stream and a refused credential are the provider's last error**; an
    /// in-band error that names no kind leaves the last one in place.
    #[test]
    fn a_failed_stream_and_a_refused_credential_are_the_last_error() {
        let manager = manager();
        assert_eq!(last_kind(&manager, Provider::Anthropic), None);
        manager.record_stream_error(0, "overloaded_error");
        assert_eq!(
            last_kind(&manager, Provider::Anthropic),
            Some(ErrorKind::Overloaded)
        );
        manager.record_stream_error(0, "cyber_policy");
        assert_eq!(
            last_kind(&manager, Provider::Anthropic),
            Some(ErrorKind::Overloaded)
        );
        manager.record_stream_error(1, "truncated");
        assert_eq!(
            last_kind(&manager, Provider::Codex),
            Some(ErrorKind::Transport)
        );
        manager.mark_refused(0);
        assert_eq!(
            last_kind(&manager, Provider::Anthropic),
            Some(ErrorKind::Auth)
        );
        assert_eq!(
            last_kind(&manager, Provider::Codex),
            Some(ErrorKind::Transport)
        );
    }
}
