//! The per-account gates: holds, rejections, the terminal and hard gates, eligibility, pacing and the binding gate reason.

use super::*;
use crate::config::effective_fable_threshold;

impl Manager {
    /// Milliseconds still to run on a LIVE rate-limit hold, or `None` when the
    /// account is not held right now — either no hold is armed, or its deadline has
    /// already passed (a past hold reads as expired live, no mutation needed).
    ///
    /// The single place `rate_limited_until_ms` is turned into a duration, so the
    /// three hold questions — held at all? clears warm? outlives the cache? — can
    /// never drift apart. Pure and lock-free.
    pub(in crate::manager) fn hold_remaining_ms(
        account: &AccountRuntime,
        now_ms: i64,
    ) -> Option<i64> {
        account
            .rate_limited_until_ms
            .map(|until| until - now_ms)
            .filter(|&remaining_ms| remaining_ms > 0)
    }

    /// Whether account `idx` has NO live rate-limit hold at `now_ms` — either none
    /// was ever armed, or the one that was has already run out.
    ///
    /// The locking wrapper around [`Self::hold_remaining_ms`] for callers OUTSIDE
    /// the manager (the proxy's rotation loop), so `rate_limited_until_ms` stays
    /// private and the "is it still held?" question keeps exactly one answer.
    ///
    /// An index that names no account answers `false` (treated as still held) —
    /// the safe direction, since the only caller uses a `true` to re-admit an
    /// account into a request's rotation.
    pub fn hold_expired(&self, idx: usize, now_ms: i64) -> bool {
        let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
        accounts
            .get(idx)
            .is_some_and(|account| Self::hold_remaining_ms(account, now_ms).is_none())
    }

    /// Whether a live hold is still running when this account's prompt cache dies
    /// ([`CACHE_WARM_HOLD_SECS`] or more remaining) — the only hold long enough to
    /// be worth re-keying a session for, and so the only one
    /// [`Self::account_hard_ok`] counts as account death.
    ///
    /// The boundary is `>=`: a hold that clears at exactly the cache TTL is treated
    /// as LONG, because the prefix is gone at the same instant the account frees.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(in crate::manager) fn hold_outlives_cache(
        account: &AccountRuntime,
        now_ms: i64,
        warm: WarmWindow,
    ) -> bool {
        Self::hold_remaining_ms(account, now_ms)
            .is_some_and(|remaining_ms| remaining_ms >= warm.ms())
    }

    /// Whether a live hold clears while this account's prompt cache is still warm
    /// (under [`CACHE_WARM_HOLD_SECS`] remaining). The exact complement of
    /// [`Self::hold_outlives_cache`] over live holds, and the SOFT case: this
    /// account cannot answer THIS request, but it is a timer and not a death, so
    /// divert the one request and leave the pin where it is.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(in crate::manager) fn hold_clears_while_warm(
        account: &AccountRuntime,
        now_ms: i64,
        warm: WarmWindow,
    ) -> bool {
        Self::hold_remaining_ms(account, now_ms)
            .is_some_and(|remaining_ms| remaining_ms < warm.ms())
    }

    /// Whether a `rejected` status clears while this session's prompt cache is still
    /// warm: the 5h window carries it and its reset falls inside `warm`. The same
    /// shape as [`Self::hold_clears_while_warm`], for the one terminal gate that is
    /// really a timer: the pin survives and each request diverts, so the session
    /// comes home to a warm prefix when the window rolls instead of paying a cold
    /// one on a failover account it then never leaves. Measured 2026-10-06 over
    /// five days of logs: 855 of 886 held-conversation moves fired on a terminal
    /// gate, and `rejected` is the only one of the three that self-clears.
    ///
    /// Never a SERVE verdict: a rejected account cannot answer a request until the
    /// window rolls, so [`Self::hard_ok`] and [`Self::bound_account_holds`] still
    /// refuse it outright. Pure and lock-free.
    pub(in crate::manager) fn rejection_clears_while_warm(
        account: &AccountRuntime,
        now_ms: i64,
        warm: WarmWindow,
    ) -> bool {
        account
            .quota
            .five_hour_rejection_reset()
            .map(odt_to_ms)
            .is_some_and(|reset_ms| {
                let remaining_ms = reset_ms - now_ms;
                remaining_ms > 0 && remaining_ms < warm.ms()
            })
    }

    /// The TERMINAL account-level gates — the blocks that are a fact about the
    /// CREDENTIAL rather than about one request or one window, and that never
    /// self-free (so they carry no clear-instant): `disabled`,
    /// [`AccountStatus::Error`], and `quota.status == Some("rejected")` —
    /// Anthropic's own verdict, which unlike a window has no reset to wait on.
    /// `None` when no terminal gate is active.
    ///
    /// **This is the one list both [`Self::account_hard_ok`] and
    /// [`Self::account_gate`] read**, and it exists because they used to keep two
    /// hand-maintained copies of it that drifted: `account_hard_ok` blocked on
    /// `rejected` while `account_gate` had no `Rejected` arm at all, so an account
    /// Anthropic had explicitly rejected was held out of rotation while rendering
    /// `OK` in the TUI and feeding [`Manager::retry_after_hint`] as though it were
    /// healthy. A terminal gate added HERE reaches both callers at once; one added
    /// to either caller alone is exactly the bug this prevents.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(in crate::manager) fn account_terminal_gate(
        account: &AccountRuntime,
    ) -> Option<GateReason> {
        if account.disabled {
            return Some(GateReason::Disabled);
        }
        if account.status == AccountStatus::Error {
            return Some(GateReason::Login);
        }
        if account.quota.status.as_deref() == Some("rejected") {
            return Some(GateReason::Rejected);
        }
        None
    }

    /// The ACCOUNT-level HARD gates alone — the blocks that mean *this account is
    /// gone for every model class*, with every SOFT gate deliberately absent:
    ///   - every terminal gate ([`Self::account_terminal_gate`]: `disabled`,
    ///     [`AccountStatus::Error`], `rejected`) → hard fail;
    ///   - a rate-limit hold that OUTLIVES the prompt cache
    ///     ([`Self::hold_outlives_cache`]) — a SHORTER hold is a timer, not a death,
    ///     and deliberately passes here.
    ///
    /// **This — not [`Self::hard_ok`] — is the SOLE authority on whether a session's
    /// pin may be re-keyed**, because a re-key re-creates the conversation prefix on
    /// a cold account. Everything absent from it is a fact about ONE REQUEST rather
    /// than about the account, and a per-request fact may divert a request but must
    /// never move a pin. That covers the soft utilization threshold
    /// ([`crate::quota::Quota::is_near`]), pacing, an entry in `tried`, model-scoped
    /// exhaustion ([`Self::model_blocked`]), and — the newest — a rate-limit hold
    /// that clears while the cache is still warm ([`Self::hold_clears_while_warm`]).
    ///
    /// The hold split is why this predicate is NOT the right one for a SERVE
    /// decision: a short-held account passes here (its pin survives) while it still
    /// cannot answer a request until the timer runs out. [`Self::hard_ok`] adds that
    /// back.
    ///
    /// Known staleness caveat: `quota.status` is written ONLY from live response
    /// headers ([`crate::quota`]), never by the background probe, so for a benched
    /// account it is stale-or-`None` and `None` passes here. That is acceptable
    /// precisely because a genuine rejection answers the next attempt with a 429,
    /// which arms a real hold — upstream stays the oracle instead of our own
    /// possibly-stale arithmetic.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    ///
    /// `group` is the SESSION's real requested group (`tcr run --group`, or
    /// `None`) — never the PREFER-narrowing override a pin-honoring call site
    /// passes to [`Self::eligible`]. A reservation is not a preference: this is
    /// the predicate that decides whether a warm pin survives, so it must see
    /// what the session actually asked for, not the bypass value the four
    /// pin/connection/sticky call sites in [`Manager::select_with_group`] pass
    /// to `eligible` to avoid re-litigating group PREFERENCE against an
    /// established pin. See [`Self::reserved_blocks`].
    pub(in crate::manager) fn account_hard_ok(
        account: &AccountRuntime,
        now_ms: i64,
        group: Option<&str>,
        reserved: &HashSet<String>,
        parked: &HashSet<String>,
        warm: WarmWindow,
    ) -> bool {
        match Self::account_terminal_gate(account) {
            // The one terminal gate that is a timer in disguise: a 5h rejection
            // whose window rolls while this session's cache is still warm keeps
            // the pin, exactly as a short hold does. See
            // [`Self::rejection_clears_while_warm`].
            Some(GateReason::Rejected)
                if Self::rejection_clears_while_warm(account, now_ms, warm) => {}
            Some(_) => return false,
            None => {}
        }
        if Self::parked_blocks(account, parked) {
            return false;
        }
        if Self::hold_outlives_cache(account, now_ms, warm) {
            return false;
        }
        if Self::reserved_blocks(account, group, reserved) {
            return false;
        }
        true
    }

    /// Whether a PARKED group holds `account` out of rotation altogether.
    ///
    /// Deliberately takes no request group: unlike [`Self::reserved_blocks`],
    /// parking is a fact about the ACCOUNT, exactly like `disabled`. An
    /// explicit `--group <parked>` ask does not lift it — that ask gets the
    /// same honest refusal (soft-wait, then 429) a reserved group's ask gets
    /// when no member can serve, which is the point: an operator parks a group
    /// to stop its accounts serving anything at all until they unpark it.
    ///
    /// Because this sits in [`Self::account_hard_ok`], a parked account is also
    /// never PINNED and an existing pin on it is re-keyed away on the next
    /// request — same treatment a group that has just become reserved gets.
    pub(in crate::manager) fn parked_blocks(
        account: &AccountRuntime,
        parked: &HashSet<String>,
    ) -> bool {
        account.groups.iter().any(|g| parked.contains(g))
    }

    /// Whether a RESERVATION (not a preference — see the bridge's "Semantics"
    /// section) holds `account` out for a request that asked for `group`.
    ///
    /// `A eligible iff g ∈ groups(A)` when the session asked for a specific `g`
    /// — reservation never adds a SECOND requirement on top of that match, so
    /// `Some(_)` always returns `false` here (the ordinary group-preference
    /// check already governs, wherever the caller applies it). Only an
    /// unrequested ask (`None`) — which includes the soft-fallback pass, which
    /// deliberately treats itself as unrequested for this purpose exactly as it
    /// does for group preference — is ever blocked, and then only if `account`
    /// carries ANY reserved group: an account in reserved `codereview` and
    /// plain `dev` stays reachable by `--group dev`, and is blocked for traffic
    /// that asked for nothing.
    pub(in crate::manager) fn reserved_blocks(
        account: &AccountRuntime,
        group: Option<&str>,
        reserved: &HashSet<String>,
    ) -> bool {
        if group.is_some() {
            return false;
        }
        account.groups.iter().any(|g| reserved.contains(g))
    }

    /// Whether this account cannot serve **this request's model class** while still
    /// serving every other one: a Fable request against an exhausted model-scoped
    /// weekly (`7d_oi`) bucket.
    ///
    /// Model-scoped exhaustion is a property of the REQUEST CLASS, never of the
    /// account — an account out of its Fable weekly answers Opus perfectly — so it is
    /// deliberately NOT part of [`Self::account_hard_ok`]. At a pin it belongs with
    /// the soft-divert family: divert THAT ONE request and keep the pin, so the
    /// session's next Opus turn still lands on the account holding its warm prefix.
    /// It remains a hard skip for every SERVE decision ([`Self::hard_ok`],
    /// [`Self::eligible`]) — serving a Fable request from an exhausted bucket only
    /// buys a 429.
    ///
    /// The live shape that makes the distinction load-bearing: every Claude Code
    /// session mixes classes — Opus for the conversation plus a one-line Fable call
    /// for titles and summaries — while a real fleet reads 95-99% on the Fable weekly
    /// across nearly every account. Counted as account death, one cheap title request
    /// re-keyed the session and dragged a 200k-token conversation onto a cold
    /// account.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(in crate::manager) fn model_blocked(
        account: &AccountRuntime,
        global_threshold: f64,
        fable_weekly_threshold: Option<f64>,
        now: OffsetDateTime,
        is_fable: bool,
    ) -> bool {
        let threshold = account.switch_threshold.unwrap_or(global_threshold);
        let threshold = effective_fable_threshold(fable_weekly_threshold, threshold);
        is_fable && account.quota.model_weekly_exhausted(threshold, now)
    }

    /// Can this account SERVE this request at all, soft gates aside: the
    /// account-level blocks ([`Self::account_hard_ok`]), **any** live rate-limit
    /// hold, and this request's own model-class block ([`Self::model_blocked`]).
    ///
    /// The short-hold term is the one that is not implied by `account_hard_ok`, and
    /// it is load-bearing in exactly one direction. A hold under
    /// [`CACHE_WARM_HOLD_SECS`] deliberately passes `account_hard_ok` so the session
    /// keeps its pin — but the account is still parked, so serving it only buys
    /// another 429. Every SERVE decision therefore re-adds the full hold here; only
    /// the PIN decision gets the softened one.
    ///
    /// The right predicate for a serve decision (may this account answer *this*
    /// request?) and the wrong one for a pin decision (is this session's account
    /// gone?) — see [`Self::account_hard_ok`], which owns the latter.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    #[expect(
        clippy::too_many_arguments,
        reason = "the per-request selection inputs, each read by a different gate"
    )]
    pub(in crate::manager) fn hard_ok(
        account: &AccountRuntime,
        global_threshold: f64,
        fable_weekly_threshold: Option<f64>,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        group: Option<&str>,
        reserved: &HashSet<String>,
        parked: &HashSet<String>,
        warm: WarmWindow,
    ) -> bool {
        Self::account_hard_ok(account, now_ms, group, reserved, parked, warm)
            // A SERVE needs the account itself answering: a rejection that merely
            // clears while the cache is warm keeps the pin above but cannot answer
            // this request, so the terminal gate is asked again here, unsoftened.
            && Self::account_terminal_gate(account).is_none()
            && !Self::hold_clears_while_warm(account, now_ms, warm)
            && !Self::model_blocked(
                account,
                global_threshold,
                fable_weekly_threshold,
                now,
                is_fable,
            )
    }

    /// Whether the CONTROL account may be preferred for an unpinned
    /// identity-plane request (control-account part 2, §1). Deliberately
    /// **bypasses the `disabled` gate**: the control account ships `disabled`
    /// on purpose (out of the inference rotation) while remaining the admin
    /// identity — see [`Manager::control_idx`]'s doc-comment. Still honours
    /// every gate that means genuinely broken:
    ///   - [`AccountStatus::Error`] → skip (a dead login is a dead login);
    ///   - `quota.status == Some("rejected")` → skip (Anthropic's own verdict);
    ///   - a LIVE rate-limit hold (`rate_limited_until_ms` in the future) → skip.
    ///
    /// **Deliberately NOT [`Self::eligible`] and NOT [`Self::account_hard_ok`]**
    /// — both fold `disabled` into their terminal gate
    /// ([`Self::account_terminal_gate`]), so calling either here would make the
    /// whole control-routing feature a silent no-op: it compiles, its tests
    /// pass, and it never fires because the one account it is meant to route to
    /// always reads as ineligible. This predicate is the bypass; it does NOT
    /// loosen `eligible` or `account_hard_ok` themselves, which every other
    /// caller still depends on.
    ///
    /// No utilization/pacing/model-class check: those exist to spread LOAD
    /// across a fleet, which does not apply to a single designated identity —
    /// the reserve for a POOLED control account is [`effective_threshold`]'s
    /// job, applied on the GENERAL pick's side, not here.
    ///
    /// Pure and lock-free; the caller holds whichever accounts lock it needs.
    pub(in crate::manager) fn control_eligible(account: &AccountRuntime, now_ms: i64) -> bool {
        if account.status == AccountStatus::Error {
            return false;
        }
        if account.quota.status.as_deref() == Some("rejected") {
            return false;
        }
        if let Some(until) = account.rate_limited_until_ms
            && now_ms < until
        {
            return false;
        }
        true
    }

    /// Why the group-respecting pass found nothing — see [`GroupMiss`].
    ///
    /// Called ONLY from the soft fallback, which runs after that pass already
    /// returned `None`, so this scan costs nothing on a request that routes
    /// normally. Pure: takes the accounts slice the caller already holds a guard
    /// on, and reads no lock of its own.
    ///
    /// `control_excluded` is the index [`Self::select_with_group`] forced into
    /// its pool-`tried` set for THIS request — `Some` only for an inference
    /// request with a control account configured. Passed in rather than
    /// re-derived from `self.control()` so the answer here can never disagree
    /// with the exclusion that actually ran: the classifier's whole job is to
    /// name that exclusion when it is the reason.
    ///
    /// `tried` is the caller's ORIGINAL set, deliberately not the
    /// control-injected `pool_tried` — a member that is only in the set because
    /// of the injection must be attributed to [`GroupMiss::OnlyControl`], not
    /// silently counted as "already tried".
    ///
    /// Every arm after [`GroupMiss::Unknown`] reads only the members of the
    /// request's own `provider`: the others could never have served it, so a
    /// group holding none of them is [`GroupMiss::OtherProviderOnly`], and a
    /// Codex member beside the excluded control account does not hide that the
    /// control exclusion is the reason.
    #[expect(
        clippy::too_many_arguments,
        reason = "the per-request selection inputs, each read by a different gate"
    )]
    pub(in crate::manager) fn classify_group_miss(
        accounts: &[AccountRuntime],
        tried: &HashSet<usize>,
        control_excluded: Option<usize>,
        global_threshold: f64,
        fable_weekly_threshold: Option<f64>,
        pacing: &PacingConfig,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        provider: crate::provider::Provider,
        group: &str,
        reserved: &HashSet<String>,
        parked: &HashSet<String>,
    ) -> GroupMiss {
        let members: Vec<usize> = accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| account.groups.iter().any(|label| label == group))
            .map(|(idx, _)| idx)
            .collect();
        if members.is_empty() {
            return GroupMiss::Unknown;
        }
        // Narrowed to the request's provider: a Claude request is never served
        // by a Codex account, nor the reverse, so a member of the other provider
        // is no candidate and must not be what the reason describes. By provider
        // ONLY: a row deleted from the file keeps its slot (`removed`) and its old
        // groups until a restart, and dropping it here would have a fleet with no
        // Codex account name a provider it does not have.
        let members: Vec<usize> = members
            .into_iter()
            .filter(|idx| {
                accounts
                    .get(*idx)
                    .is_some_and(|account| account.provider == provider)
            })
            .collect();
        if members.is_empty() {
            return GroupMiss::OtherProviderOnly;
        }
        if members.iter().all(|idx| Some(*idx) == control_excluded) {
            return GroupMiss::OnlyControl;
        }
        // Would any member serve with the soft pacing gate lifted? If yes, pacing
        // really was the reason and the old message was right by accident. If no,
        // the members are hard-gated, and saying "paced" would be the same lie in
        // the other direction — which is exactly the defect this classifier exists
        // to end, so it must not be reintroduced here.
        let servable_unpaced = members.iter().any(|idx| {
            Some(*idx) != control_excluded
                && !tried.contains(idx)
                && accounts.get(*idx).is_some_and(|account| {
                    Self::eligible(
                        account,
                        global_threshold,
                        fable_weekly_threshold,
                        pacing,
                        false,
                        now,
                        now_ms,
                        is_fable,
                        provider,
                        Some(group),
                        Some(group),
                        reserved,
                        parked,
                    )
                })
        });
        if servable_unpaced {
            GroupMiss::Paced
        } else {
            GroupMiss::AllGated
        }
    }

    /// `reserved_group` is the SESSION's real requested group, used ONLY for
    /// the reservation gate below — see [`Self::reserved_blocks`] and
    /// [`Self::account_hard_ok`]'s doc-comment for why this is a separate
    /// value from `group` (the PREFER-narrowing argument): the four
    /// pin/connection/sticky call sites in [`Manager::select_with_group`] pass
    /// `group: None` to bypass PREFER re-narrowing against an established pin,
    /// but must still pass the session's ACTUAL ask here, because a
    /// reservation is not a preference. Every other caller passes the same
    /// value for both.
    ///
    /// `provider` is the REQUEST's provider, and it is checked before anything
    /// else: an account of the other provider is not a weaker candidate, it is
    /// not a candidate at all, since its credential means nothing to the
    /// request's upstream. An exhausted Anthropic fleet answers 429 rather than
    /// spilling onto a Codex account, and the reverse.
    #[expect(
        clippy::too_many_arguments,
        reason = "the per-request selection inputs, each read by a different gate"
    )]
    pub(in crate::manager) fn eligible(
        account: &AccountRuntime,
        global_threshold: f64,
        fable_weekly_threshold: Option<f64>,
        pacing: &PacingConfig,
        respect_pacing: bool,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        provider: crate::provider::Provider,
        group: Option<&str>,
        reserved_group: Option<&str>,
        reserved: &HashSet<String>,
        parked: &HashSet<String>,
    ) -> bool {
        if account.provider != provider {
            return false;
        }
        if account.disabled || account.status == AccountStatus::Error {
            return false;
        }
        // Parking is an account-level block, not a group preference — see
        // `Self::parked_blocks`. Placed with `disabled` above rather than with
        // the reservation gate below because it is the same KIND of fact.
        if Self::parked_blocks(account, parked) {
            return false;
        }
        // A rate-limit hold blocks the account only while it is still in the
        // future — a past hold is treated as expired (live), no mutation needed.
        if let Some(until) = account.rate_limited_until_ms
            && now_ms < until
        {
            return false;
        }
        let threshold = account.switch_threshold.unwrap_or(global_threshold);
        if account.quota.is_near(threshold, now) {
            return false;
        }
        // Per-model routing: only a Fable request gates on the model-scoped weekly
        // (`7d_oi`) bucket — every non-Fable model still serves from this account.
        // Kept here on purpose: for an UNPINNED pick a Fable-exhausted account
        // genuinely cannot serve this request, so it must be skipped. What changed
        // is only that it is no longer grounds to MOVE AN EXISTING PIN — that
        // decision reads [`Self::account_hard_ok`], which excludes this gate.
        if Self::model_blocked(
            account,
            global_threshold,
            fable_weekly_threshold,
            now,
            is_fable,
        ) {
            return false;
        }
        // Reservation (not a preference — see this fn's `reserved_group` doc
        // above and [`Self::reserved_blocks`]): a hard block, independent of
        // the PREFER narrowing below, so it applies even at the four call
        // sites that pass `group: None` to skip that narrowing.
        if Self::reserved_blocks(account, reserved_group, reserved) {
            return false;
        }
        // `--group` PREFER routing (Phase 1: no `--only`, so this never hard-blocks
        // an account on its own — it only narrows the CALLER's chosen pass, since
        // the fallback pass always calls this with `group: None`). A HARD-looking
        // early return by design, positioned after the model-block gate and before
        // the SOFT pacing gate below, matching this account's true selectivity: it
        // filters exactly like a hard gate WITHIN one pass. `account_gate` /
        // `account_hard_ok` are untouched — Phase 1 never reports a group as the
        // reason an account is hard-blocked.
        if let Some(g) = group
            && !account.groups.iter().any(|acct_group| acct_group == g)
        {
            return false;
        }
        // SOFT pacing gate, evaluated LAST so it only ever narrows an already-healthy
        // account. When `respect_pacing` is false (the fallback pass) or pacing is
        // unconfigured, this is inert — so a default-OFF build is byte-identical here
        // and the fallback pass can always still find a servable account.
        !(respect_pacing && Self::paced_out(account, pacing, now_ms))
    }

    /// Whether the SOFT pacing gate ALONE holds this account out: it is at the
    /// per-account in-flight cap, or inside the min-spacing window since its last
    /// serve. Inert (always `false`) when pacing is unconfigured, which is how it
    /// ships.
    ///
    /// Split out of [`Self::eligible`] because [`Self::select`] needs to tell the two
    /// soft gates apart. They are not the same kind of signal:
    ///   - the **utilization threshold** is OUR arithmetic over headers that go stale
    ///     by minutes, and it routinely benches an account Anthropic is still
    ///     answering 200s for — so it may not bench a warm pin;
    ///   - **pacing** is a fact we measure exactly and continuously about our OWN
    ///     concurrency, and spreading a session's burst is the entire reason the cap
    ///     exists. It stays a per-REQUEST yield: divert this one, keep the pin.
    pub(in crate::manager) const fn paced_out(
        account: &AccountRuntime,
        pacing: &PacingConfig,
        now_ms: i64,
    ) -> bool {
        if !pacing.is_active() {
            return false;
        }
        if let Some(cap) = pacing.effective_max_in_flight()
            && account.in_flight >= cap
        {
            return true;
        }
        if let Some(gap) = pacing.min_spacing_ms
            && now_ms.saturating_sub(account.last_served_ms) < gap as i64
        {
            return true;
        }
        false
    }

    /// Why this account is out of rotation and when it clears — the display and
    /// hint-side companion to [`Self::eligible`], mirroring its HARD gates exactly
    /// (disabled/error/rejected → hold → 5-hour → weekly → Fable weekly) while deliberately
    /// omitting soft pacing, which only ever narrows an already-healthy account and
    /// never holds one out. Pure and lock-free so both [`Manager::snapshot`] and
    /// [`Manager::retry_after_hint`] can call it as the single source of truth.
    ///
    /// The terminal gates ([`Self::account_terminal_gate`]: `Disabled`, `Login`,
    /// `Rejected`) never self-free, so their instant is `None` — a rejected account
    /// therefore stops feeding [`Manager::retry_after_hint`] a clear-instant it was
    /// never going to honour. Otherwise every ACTIVE gate contributes the instant it clears:
    /// a future hold its deadline, and a window at/over `threshold` its
    /// [`crate::quota::QuotaWindow::live_reset`]. An account frees only once ALL of
    /// its gates clear, so the binding gate is the LATEST-clearing one and
    /// `free_at` is the MAX of the clear-instants. An active gate with no known
    /// reset (a window over threshold whose reset is unknown) is the
    /// longest-possible constraint, so it sorts as "latest": it becomes the reason
    /// and `free_at` is `None` (we cannot promise a time). No active gate → `Ok`.
    /// `group` is the SESSION's real requested group — same argument
    /// [`Self::account_hard_ok`] takes, for the same reason: this display-side
    /// mirror must agree with the routing predicate on which accounts a
    /// RESERVED gate actually blocks. Every current caller ([`Manager::snapshot`],
    /// [`Manager::retry_after_hint`]) reports the fleet as unrequested traffic
    /// sees it, so both pass `None`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the per-request selection inputs, each read by a different gate"
    )]
    pub(in crate::manager) fn account_gate(
        account: &AccountRuntime,
        threshold: f64,
        fable_weekly_threshold: Option<f64>,
        now: OffsetDateTime,
        now_ms: i64,
        is_fable: bool,
        group: Option<&str>,
        reserved: &HashSet<String>,
        parked: &HashSet<String>,
    ) -> (GateReason, Option<OffsetDateTime>) {
        // Terminal states that never self-free — reported with no clear-instant.
        // Shared with `account_hard_ok` so the two can no longer disagree about
        // which blocks are account-level (see `account_terminal_gate`).
        if let Some(reason) = Self::account_terminal_gate(account) {
            return (reason, None);
        }
        // Parked by a group. Reported AFTER the terminal gates above and as an
        // early return of its own, which is the ordering the panel depends on:
        // a row disabled by hand inside a parked group reads `disabled`, its
        // own state, not `parked` — see `GateReason::Parked`. Never self-frees
        // (only `tcr group unpark`), so no clear-instant, same as `Disabled`.
        if Self::parked_blocks(account, parked) {
            return (GateReason::Parked, None);
        }

        // Every ACTIVE hard gate paired with the instant it clears (`None` = active
        // but no known reset). Collected in eligibility order; soft pacing is
        // intentionally excluded (see the doc comment).
        let mut gates: Vec<(GateReason, Option<OffsetDateTime>)> = Vec::new();
        // Reservation never self-frees on its own (only an operator's `tcr
        // group unreserve` clears it) — `None` clear-instant, mirroring the
        // terminal gates above; kept as an ordinary `gates` entry rather than
        // an early return because it is a fact about THIS request's group ask,
        // not about the credential — see `Self::reserved_blocks`.
        if Self::reserved_blocks(account, group, reserved) {
            gates.push((GateReason::Reserved, None));
        }
        if let Some(until) = account.rate_limited_until_ms
            && now_ms < until
        {
            let reason = match account.hold_cause {
                HoldCause::RateLimit => GateReason::Hold,
                HoldCause::Refused => GateReason::Refused,
            };
            gates.push((reason, ms_to_odt(until)));
        }
        if let Some(window) = account.quota.five_hour
            && window.effective(now) >= threshold
        {
            gates.push((GateReason::FiveHour, window.live_reset(now)));
        }
        if let Some(window) = account.quota.seven_day
            && window.effective(now) >= threshold
        {
            gates.push((GateReason::SevenDay, window.live_reset(now)));
        }
        // The model-scoped weekly gates Fable requests ONLY — mirror `eligible`.
        if is_fable && let Some(window) = account.quota.seven_day_oi {
            let fable_threshold = effective_fable_threshold(fable_weekly_threshold, threshold);
            if window.effective(now) >= fable_threshold {
                gates.push((GateReason::FableWeekly, window.live_reset(now)));
            }
        }
        // Standard (API-key) token/request limits: the same check `Quota::is_near`
        // makes, so account_gate agrees with eligible(): a standard-limited account
        // is gated, not "soon-servable". free_at = standard_reset (the upstream
        // refresh instant); None when unknown ("cannot promise a time").
        if account.quota.standard_limited(threshold, now) {
            gates.push((GateReason::Standard, account.quota.standard_reset));
        }

        // The latest-clearing gate binds. `gate_clear_key` sorts an unknown reset
        // (`None`) after every known instant, so if any active gate has no reset it
        // wins here and carries its `None` out as `free_at` — exactly the
        // "cannot promise a time" case. `max_by_key` breaks ties toward the
        // last-collected gate, a deterministic order.
        gates
            .into_iter()
            .max_by_key(|&(_, at)| gate_clear_key(at))
            .unwrap_or((GateReason::Ok, None))
    }
}

/// Sort key for a gate's clear-instant that ranks an unknown reset (`None`) after
/// every known instant: an active gate with no known reset is the
/// longest-possible constraint, so it must sort as the LATEST-clearing gate.
/// Among known instants the natural chronological order applies. Used by
/// [`Manager::account_gate`] to pick the binding (latest-clearing) gate.
const fn gate_clear_key(at: Option<OffsetDateTime>) -> (u8, OffsetDateTime) {
    match at {
        Some(t) => (0, t),
        None => (1, OffsetDateTime::UNIX_EPOCH),
    }
}
