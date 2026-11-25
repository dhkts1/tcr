//! The pool picks: each pass's candidate filter, its ranking, and the control-account exclusion.

use super::*;

impl Manager {
    /// The best pacing-respecting eligible account not in `tried`, by ascending
    /// `(priority, reset-urgency bucket, last_selected_seq, soonest weekly reset)`
    /// — the pre-pacing LRU order, now bucketed by reset urgency
    /// ([`Self::reset_urgency_tier`]) and additionally skipping any account the
    /// soft pacing gate holds out.
    /// Read-only (no stamp); the caller stamps the winner. Emits one INFO line per
    /// account skipped *specifically because of pacing* (healthy but capped/spaced)
    /// so the knobs are tunable live.
    /// The GENERAL pick's extra narrowing of the control account (§3): `true`
    /// for every account that is not the control account (a no-op), and for the
    /// control account itself, `true` only while its utilization stays under
    /// `effective_threshold(threshold, control_reserve, true)` rather than the
    /// full threshold [`Self::eligible`] already checked. Applied AFTER
    /// `eligible` returns true, so it only ever narrows further — never widens.
    /// **Inert while the control account is `disabled`**: `eligible` excludes a
    /// disabled account before this is ever reached (see
    /// [`Manager::control_reserve`]'s doc-comment).
    pub(super) fn pool_pick_respects_control_reserve(
        &self,
        idx: usize,
        account: &AccountRuntime,
        now: OffsetDateTime,
    ) -> bool {
        if self.control() != Some(idx) {
            return true;
        }
        let threshold = account.switch_threshold.unwrap_or(self.global_threshold);
        let reserved = effective_threshold(threshold, self.control_reserve, true);
        !account.quota.is_near(reserved, now)
    }

    /// The reset-urgency bucket `account` falls into — its governing weekly
    /// reset, floored into [`Manager::reset_urgency_tier_ms`]-wide buckets
    /// measured forward from `now`. **Ascending: a lower bucket is spent
    /// first**, because unused weekly quota is worth nothing once its window
    /// resets.
    ///
    /// Why a bucket and not the reset instant itself: a reset is a
    /// millisecond-precision instant that essentially never ties, so ranking on
    /// it directly would retire the `last_selected_seq` tiebreak entirely and
    /// park every unpinned request on the single soonest-resetting account until
    /// it gated. That is the same deterministic pin [`Manager::select`]'s
    /// doc-comment rejects quota-headroom ordering for, and this pool pick
    /// carries no `in_flight` term to dilute it (only
    /// [`Manager::pick_least_loaded`] does), so a burst would land undiluted.
    /// Bucketing re-creates the tie: at the default 24h, the live fleet's four
    /// soonest-resetting accounts share one bucket and still fan out inside it.
    ///
    /// Two arms deliberately return a value that is NOT a bucket index:
    ///  - **no known live reset → [`i64::MIN`]**, sorting the account ahead of
    ///    every bucket. This preserves the pre-tier "unknown quota sorts first,
    ///    probe it" contract byte-for-byte — the same reason the raw-`reset`
    ///    term below maps `None` to [`i128::MIN`]. It covers both a genuinely
    ///    unmeasured window and one whose reset has already elapsed;
    ///    [`crate::records::quota::QuotaWindow::live_reset`] collapses those two into
    ///    `None` and this does not try to tell them apart.
    ///  - **feature disabled (`reset_urgency_tier_ms == 0`) → a constant `0`**,
    ///    which ties for every account and hands the decision straight back to
    ///    the LRU tick. That restores the pre-tier ordering exactly, including
    ///    the unknown-reset-first behaviour, which survives on the raw-`reset`
    ///    term the key still carries.
    ///
    /// Self-rotating by construction: spending an account does not move its
    /// reset, but crossing that reset starts a fresh window ~7 days out, which
    /// drops the account to the back of the bucket order. No account starves —
    /// each one's window comes due in turn.
    pub(super) fn reset_urgency_tier(&self, account: &AccountRuntime, now: OffsetDateTime) -> i64 {
        if self.reset_urgency_tier_ms == 0 {
            return 0;
        }
        let Some(reset) = account.quota.governing_weekly_reset(now) else {
            return i64::MIN;
        };
        // `live_reset` only ever hands back a FUTURE instant, so this difference
        // is positive in practice; `max(0)` keeps a clock that stepped backwards
        // between the two reads from producing a negative bucket that would
        // outrank a genuinely urgent account. `try_from` saturates rather than
        // wrapping for the same reason.
        let remaining_ms = i64::try_from((reset - now).whole_milliseconds())
            .unwrap_or(i64::MAX)
            .max(0);
        remaining_ms / self.reset_urgency_tier_ms
    }

    pub(super) fn pick_eligible(
        &self,
        accounts: &[AccountRuntime],
        ask: &PoolAsk<'_>,
    ) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut best_key: Option<(i64, i64, u64, i128)> = None;
        for (idx, account) in accounts.iter().enumerate() {
            if !self.pool_candidate(PoolPass::Paced, idx, account, ask) {
                // Distinguish a pacing skip (healthy but capped/spaced) from a real
                // ineligibility (disabled/error/quota) so the log names only the former:
                // eligible once pacing is ignored, and held out by pacing alone.
                if !ask.tried.contains(&idx)
                    && Self::paced_out(account, &self.pacing, ask.now_ms)
                    && Self::eligible(
                        account,
                        self.global_threshold,
                        self.fable_weekly_threshold,
                        &self.pacing,
                        false,
                        ask.now,
                        ask.now_ms,
                        ask.is_fable,
                        ask.provider,
                        ask.group,
                        ask.group,
                        &ask.groups.reserved,
                        &ask.groups.parked,
                    )
                {
                    tracing::info!(
                        account = %account.name,
                        in_flight = account.in_flight,
                        "pacing: skip in selection"
                    );
                }
                continue;
            }
            let now = ask.now;
            // Unknown weekly reset sorts FIRST (probe it) — treat as the minimum.
            let reset = account
                .quota
                .governing_weekly_reset(now)
                .map_or(i128::MIN, |r| i128::from(r.unix_timestamp()));
            let key = (
                account.priority,
                self.reset_urgency_tier(account, now),
                account.last_selected_seq,
                reset,
            );
            if best_key.is_none_or(|b| key < b) {
                best = Some(idx);
                best_key = Some(key);
            }
        }
        best
    }

    /// The least-loaded servable account not in `tried`, IGNORING pacing (the soft
    /// fallback pass). Sort key ascending: `(in_flight, priority, reset-urgency
    /// bucket, last_selected_seq, weekly reset)` — least concurrent load first,
    /// then the normal bucketed LRU order. `in_flight` stays ahead of the urgency
    /// bucket deliberately: this pass exists to find the COOLEST account when the
    /// fleet is all-paced, and letting reset urgency outrank live load would send
    /// the overflow straight back onto the busy account the fallback is trying to
    /// relieve. All
    /// non-pacing eligibility (disabled/error/rate-limit/quota) still applies, so a
    /// genuinely exhausted fleet still yields `None` (a real 429), while a merely
    /// all-paced fleet always yields the coolest account. Read-only.
    pub(super) fn pick_least_loaded(
        &self,
        accounts: &[AccountRuntime],
        ask: &PoolAsk<'_>,
    ) -> Option<usize> {
        let now = ask.now;
        let mut best: Option<usize> = None;
        let mut best_key: Option<(u32, i64, i64, u64, i128)> = None;
        for (idx, account) in accounts.iter().enumerate() {
            if !self.pool_candidate(PoolPass::LeastLoaded, idx, account, ask) {
                continue;
            }
            let reset = account
                .quota
                .governing_weekly_reset(now)
                .map_or(i128::MIN, |r| i128::from(r.unix_timestamp()));
            let key = (
                account.in_flight,
                account.priority,
                self.reset_urgency_tier(account, now),
                account.last_selected_seq,
                reset,
            );
            if best_key.is_none_or(|b| key < b) {
                best = Some(idx);
                best_key = Some(key);
            }
        }
        best
    }

    /// The Fable last-resort pick (three-tier gate): reached only when
    /// [`Self::pick_eligible`] and [`Self::pick_least_loaded`] both already
    /// returned `None` for a Fable request. Mirrors [`Self::eligible`]'s
    /// non-model gates (the request's provider/disabled/error/live hold/shared
    /// five-hour and seven-day windows/standard limits/group) but — unlike
    /// `eligible` and `pick_least_loaded` — does **not** hard-skip on
    /// [`Self::model_blocked`]'s prediction; it excludes only an account
    /// upstream has genuinely rejected on `7d_oi`
    /// ([`crate::records::quota::Quota::model_weekly_rejected`]). Ignores `group`
    /// (always `None`, matching the soft fallback's own widening) — this pass
    /// only runs once the whole pool is already being considered.
    ///
    /// Sort key ascending, freshest first: `(7d_oi utilization, priority,
    /// reset-urgency bucket, last_selected_seq, weekly reset)`. Preferring the
    /// LOWEST live `7d_oi` spends the one probe request on the account most
    /// likely to still have real headroom, rather than the one already
    /// closest to a genuine reject.
    ///
    /// Read-only.
    pub(super) fn pick_fable_last_resort(
        &self,
        accounts: &[AccountRuntime],
        ask: &PoolAsk<'_>,
    ) -> Option<usize> {
        let now = ask.now;
        let mut best: Option<usize> = None;
        let mut best_key: Option<(u64, i64, i64, u64, i128)> = None;
        for (idx, account) in accounts.iter().enumerate() {
            if !self.pool_candidate(PoolPass::FableLastResort, idx, account, ask) {
                continue;
            }
            let seven_day_oi = account
                .quota
                .effective(crate::records::quota::QuotaBucket::SevenDayOi, now)
                .unwrap_or(0.0);
            let oi_bucket = (seven_day_oi.max(0.0) * 1_000_000.0) as u64;
            let reset = account
                .quota
                .governing_weekly_reset(now)
                .map_or(i128::MIN, |r| i128::from(r.unix_timestamp()));
            let key = (
                oi_bucket,
                account.priority,
                self.reset_urgency_tier(account, now),
                account.last_selected_seq,
                reset,
            );
            if best_key.is_none_or(|b| key < b) {
                best = Some(idx);
                best_key = Some(key);
            }
        }
        best
    }

    /// Whether `account`, at slot `idx`, is a candidate of `pass` for the request `ask`
    /// describes: the filter half of every pool pass, and the one place each pass's
    /// filter is written. The gates themselves stay with their owners
    /// ([`Self::eligible`], [`Self::hard_ok`], [`Self::model_blocked`] through both, and
    /// [`Self::pool_pick_respects_control_reserve`]); this says only which owners each
    /// pass asks. Pure; the caller holds whichever accounts lock it needs.
    pub(in crate::manager) fn pool_candidate(
        &self,
        pass: PoolPass,
        idx: usize,
        account: &AccountRuntime,
        ask: &PoolAsk<'_>,
    ) -> bool {
        if ask.tried.contains(&idx) {
            return false;
        }
        let (now, now_ms) = (ask.now, ask.now_ms);
        match pass {
            // The lock bypasses every gate: `select_with_group` returns it before the pool.
            PoolPass::Lock => ask.locked == Some(idx),
            PoolPass::Paced | PoolPass::LeastLoaded => {
                // Only the first pass narrows on the request's group; the soft fallback
                // widens to the whole pool.
                let group = match pass {
                    PoolPass::Paced => ask.group,
                    _ => None,
                };
                Self::eligible(
                    account,
                    self.global_threshold,
                    self.fable_weekly_threshold,
                    &self.pacing,
                    pass == PoolPass::Paced,
                    now,
                    now_ms,
                    ask.is_fable,
                    ask.provider,
                    group,
                    group, // this pass's own real ask — see `eligible`'s `reserved_group` doc
                    &ask.groups.reserved,
                    &ask.groups.parked,
                ) && self.pool_pick_respects_control_reserve(idx, account, now)
            }
            PoolPass::FableLastResort => {
                // `eligible`'s first gate, asked by hand because this pass does not
                // call it. Not hypothetical: an account of the other provider has no
                // `7d_oi` window at all, so it reads as the FRESHEST candidate below
                // and would win this pick outright.
                if account.provider != ask.provider {
                    return false;
                }
                if account.disabled || account.status == AccountStatus::Error {
                    return false;
                }
                // Parked is an account-level block like `disabled` above, and this
                // last-resort pass relaxes only the Fable `7d_oi` prediction — never
                // an operator's own decision to hold a group out.
                if Self::parked_blocks(account, &ask.groups.parked) {
                    return false;
                }
                if let Some(until) = account.rate_limited_until_ms
                    && now_ms < until
                {
                    return false;
                }
                let threshold = account.switch_threshold.unwrap_or(self.global_threshold);
                // The shared (non-Fable) gates still apply in full — this pass
                // relaxes ONLY the Fable-specific `7d_oi` prediction, never the
                // account's other windows.
                if account.quota.is_near(threshold, now) {
                    return false;
                }
                // The one gate this pass relaxes from a prediction to the
                // authority: skip only a `7d_oi` upstream has actually rejected.
                if account.quota.model_weekly_rejected(now) {
                    return false;
                }
                if Self::reserved_blocks(account, None, &ask.groups.reserved) {
                    return false;
                }
                self.pool_pick_respects_control_reserve(idx, account, now)
            }
            // The request's provider is asked first, as `eligible` asks it: an account of
            // the other provider is not an over-threshold candidate, it is no candidate.
            // `group: None` for the reason `select_revalidation` gives at its call.
            PoolPass::Revalidation => {
                account.provider == ask.provider
                    && Self::hard_ok(
                        account,
                        self.global_threshold,
                        self.fable_weekly_threshold,
                        now,
                        now_ms,
                        ask.is_fable,
                        None,
                        &ask.groups.reserved,
                        &ask.groups.parked,
                        ask.warm,
                    )
            }
        }
    }

    /// The control account, when an inference pool pick must not select it: unless
    /// `controlPooled` is set, or the request's `--group` opted the control account in
    /// (`allowControlAccount`) and the control account carries that group. Why this
    /// exclusion exists, and why it is an extra `tried` member: the comment at its use in
    /// [`Self::select_with_group`].
    pub(super) fn control_excluded(
        &self,
        request_class: RequestClass,
        group: Option<&str>,
        accounts: &[AccountRuntime],
        groups: &GroupPolicy,
    ) -> Option<usize> {
        if request_class != RequestClass::Inference || self.control_pooled {
            return None;
        }
        self.control().filter(|&idx| {
            let opted_in = group.is_some_and(|g| {
                groups.control_allowed.contains(g)
                    && accounts
                        .get(idx)
                        .is_some_and(|a| a.groups.iter().any(|gr| gr == g))
            });
            !opted_in
        })
    }
}
