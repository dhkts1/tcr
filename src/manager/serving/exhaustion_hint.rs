//! What an exhausted fleet's synthetic 429 tells the client, and a refused Codex row's gate.

use crate::manager::*;

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

impl Manager {
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
    pub(in crate::manager) fn codex_refusal_gate(
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
}
