//! `select_with_group`, the whole selection path, and the pin-load window its migration counts with.

use super::*;

/// How long a [`Manager::affinity`] pin keeps counting as **load** on its
/// account: **60 minutes** since its last touch. Past this a pin is still
/// honoured — it is not dropped, not expired, and a returning session gets its
/// account back — it simply stops making that account look busy to the
/// load-balancing migration in [`Manager::select_with_group`].
///
/// This is NOT [`CONN_AFFINITY_TTL_MS`](super::ledgers::CONN_AFFINITY_TTL_MS): that one guards a different map
/// (`conn_affinity`), bounds the life of ONE connection, and genuinely expires
/// its entries. It is also not an age TTL on the pin map. An expiring pin costs
/// a cold prompt prefix on the session's next request, the most expensive event
/// in this system; a pin excluded from a COUNT costs nothing, because the count
/// only ever decides whether some other session should be moved.
///
/// Why 60 minutes and not the 5 of [`CACHE_WARM_HOLD_SECS`] or
/// the 15 of [`crate::stores::affinity::PIN_TTL_MS`]: the count is a guess at how many
/// live sessions an account is carrying, and the two ways of guessing wrong are
/// not symmetric. Counting a dead pin makes the emptiest account on the fleet
/// read as one of the busiest and bounces every session that lands on it back
/// off again; NOT counting a session that is merely thinking under-reads a real
/// account and stacks a second session onto it, which is paid later in a
/// migration and a cold prefix. So take the widest window that still fixes the
/// first failure. 60 minutes is that: it is the ceiling of every cache window
/// this crate already believes in — [`crate::stores::affinity::EXTENDED_PIN_TTL_MS`] is
/// 55 minutes, derived from 52,951 real `request usage` records where a 15-60
/// minute gap still hit Anthropic's cache 73-88% of the time and only past 60
/// minutes fell to 12%. Past an hour no prefix is warm under any
/// `cache_control` setting, so such a pin is a preference, never a load.
///
/// Measured against the report this constant exists to fix: a live
/// `session-affinity.json` held 100 pins with exactly ONE touched inside the
/// last hour and individual pins 5 days 15 hours stale. 5, 15 and 60 minutes
/// all collapse that account's count from 8 to at most 2 (the requesting session
/// plus that one fresh pin), and migration needs the target to hold zero; 60
/// minutes is the one that does it while still counting every session that
/// could plausibly come back to a warm prefix.
pub(super) const PIN_LOAD_IDLE_MS: i64 = 60 * 60 * 1000;

/// Does a pin count as live load on its account? See [`PIN_LOAD_IDLE_MS`].
///
/// `is_self` is the requesting session's own pin: that session is demonstrably
/// alive — it is making this very request — so its own last-touch stamp (which
/// still names its PREVIOUS request) never disqualifies it.
pub(super) const fn pin_counts_as_load(is_self: bool, touched_ms: i64, now_ms: i64) -> bool {
    is_self || now_ms.saturating_sub(touched_ms) <= PIN_LOAD_IDLE_MS
}

impl Manager {
    /// [`Self::select`], with an optional PREFER-semantics group (`tcr run
    /// --group <name>`, Phase 1 — see `docs/plans/account-groups-bridge-phase1.md`).
    /// `group` narrows only the pacing-respecting first pass of the normal pick
    /// (mirroring how that pass already narrows on `respect_pacing`); the soft
    /// fallback pass always passes `None`, so a group with no current capacity
    /// degrades to the whole pool rather than a 429.
    ///
    /// # Which checks see the group, and which see `None`
    ///
    /// This paragraph was stale and said the opposite of the code: "every
    /// OTHER eligibility check … passes `None` unconditionally". Two of them
    /// were moved, because a PREFERENCE and a STRICT ask are not the same
    /// thing and only the first may be bypassed to keep a session warm.
    ///
    /// - the **affinity fast-path** and the **sticky divert destination** are
    ///   judged against `strict_group`, not the raw `group`. A warm pin is
    ///   still never re-litigated against a mere preference (a spill group
    ///   passes as `None` there, exactly as before), but an operator's
    ///   `--group` on a non-spill group, and a lease's scope, hold: an
    ///   out-of-group pin is DIVERTED, never served, and the session keeps the
    ///   pin its other traffic is warm on.
    /// - the **hard account lock** is refused outright when it falls outside
    ///   `strict_group`, which is why this function now computes the ask before
    ///   it reads the lock.
    /// - the **reservation** argument (`Self::reserved_blocks`) still takes the
    ///   raw `group` everywhere: a reservation is not a preference, it is a real
    ///   ask about who owns an account.
    ///
    /// An already-warm session is never re-KEYED by any of it: every arm above
    /// diverts and keeps the pin.
    #[expect(
        clippy::too_many_arguments,
        reason = "the per-request selection inputs, each read by a different gate"
    )]
    pub fn select_with_group(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
        path: &str,
        conn_key: Option<u64>,
        group: Option<&str>,
    ) -> Option<usize> {
        // Live-reload group membership/settings before anything below reads them —
        // see `Self::reload_groups_if_changed`'s doc. Cheap no-op when the config
        // file's mtime has not moved since the last check.
        self.reload_groups_if_changed();

        // One version of the group settings for the whole call — see
        // `Self::groups`'s doc for why this clones an `Arc` rather than holding
        // the lock across the accounts/affinity locks taken below.
        let groups = self.groups();
        // THE GROUP ASK THIS CALL WILL HOLD STRICTLY, computed once here rather
        // than at the pool pick alone, because the affinity fast-path below has
        // to ask the same question and used to have no way to. See the pool
        // pick's own comment for what strictness means and for the two
        // carve-outs.
        let strict_group = group.filter(|g| groups.is_strict(g));
        // THE PROVIDER THIS REQUEST BELONGS TO, decided by its path (see
        // `crate::base::provider`'s module doc for why path and not host). Computed
        // here, above the hard lock, for the same reason `strict_group` is: the
        // lock returns before `eligible` is ever asked, so it has to ask this
        // itself. Every account this call returns is of this provider:
        // `eligible` refuses any other first, and each door below that does
        // not go through `eligible` checks it on its own.
        let provider = crate::base::provider::Provider::of_path(path);

        // A strict Codex peer exit is a configuration block, not an upstream failure.
        // Exclude it before locks, affinity and pool picks can select that account.
        let filtered_tried = self.exclude_unsupported_codex_exits(tried, provider);
        let tried = filtered_tried.as_ref();

        // Hard account lock: pin its provider's traffic to the configured account, bypassing
        // rotation/affinity/migration. `tried` still ends the rotation loop, once the
        // locked account has failed this request, return None (no failover to the pool).
        //
        // # A STRICT GROUP OUTRANKS THE LOCK, and once it did not
        //
        // This arm used to sit ABOVE `strict_group`, returning before the ask
        // had even been computed, a measurement proved it, and it is the
        // third door out of a strict group after the affinity fast-path and
        // stickiness. For `tcr run --group` a lock is arguably the operator's
        // own louder instruction; for a LEASE it is not the same operator at
        // all. The lender expresses the scope as exactly this ask
        // (`crate::peer::lease::ScopeRestriction::Group`), so a lender with
        // `lockAccount` set served every borrowed request on the locked account
        // whatever the lease was scoped to, a scoped lease drawing on an
        // account outside its scope, on somebody else's Mac, which is the one
        // outcome scoping exists to prevent.
        //
        // The refusal is `None` and not a widening to the group: a lock means
        // "this account and no other" and a strict ask means "inside this group
        // and nowhere else", so when they disagree there is no account that
        // satisfies both, and the honest answer is the one a strict ask is
        // always owed. The caller's next rung is the 429 it already has.
        //
        // # A LOCK ONLY BINDS ITS OWN PROVIDER
        //
        // The other upstream keeps its own independent pool; it must never be
        // disabled by this lock or receive the locked account's credential.
        //
        // A deleted lock no longer restricts either provider.
        if let Some(li) = self.locked_account_for(provider) {
            if let Some(g) = strict_group {
                let in_group = {
                    let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                    accounts
                        .get(li)
                        .is_some_and(|a| a.groups.iter().any(|carried| carried == g))
                };
                if !in_group {
                    return None;
                }
            }
            return if tried.contains(&li) { None } else { Some(li) };
        }

        let now_ms = odt_to_ms(now);
        // Compute the Fable classification ONCE, not per-account.
        // The Fable weekly bucket is an Anthropic fact; a Codex request never consults it.
        let is_fable =
            provider.has_fable_bucket() && model.is_some_and(crate::base::model::is_fable_model);
        // Compute the control-account three-way split ONCE (part 2). Consulted
        // twice below: the routing overlay (control-preferred / noise) after the
        // affinity fast-path, and the pool-pick exclusion (inference must never
        // land on the control account) inside the normal pick.
        let request_class = classify_request(path);

        // Set (to the OLD pin index) for the four per-REQUEST failures that DIVERT —
        // the pin is paced out, it cannot serve this request's model class, it is
        // already in `tried`, or it is parked on a hold that clears while its prompt
        // cache is still warm — while still clearing every ACCOUNT-level HARD gate
        // (`Self::account_hard_ok`). A per-request fact may divert a request; it may
        // never RE-KEY a session: Anthropic's prompt cache is per-account, so a re-pin
        // re-creates the whole conversation prefix on the next request. The
        // fall-through re-pin at the bottom honours this and keeps the old index.
        // The fourth — over the utilization threshold — does not divert at all; the
        // fast-path below serves the pin and returns.
        let mut keep_pin: Option<usize> = None;
        // WHY the pin was kept while another account serves, carried down to the
        // re-pin at the bottom because only there are BOTH the kept pin and the
        // account actually serving known. `None` means no divert happened. Purely
        // for the log line — nothing branches on it.
        let mut divert_reason: Option<&'static str> = None;
        // The pin's `rate_limited_until_ms` at the moment `keep_pin` was set —
        // the divert episode's identity half (§4.1), captured once here under
        // whichever accounts guard was already held at that site rather than
        // taking a second lock later. `0` when the pin has no live hold (a
        // paced/model-class/pin-tried divert): a stable sentinel, not a real
        // deadline, so those diverts still key one shared episode per pin.
        let mut divert_until_ms: i64 = 0;
        // Set when the session's pin was given up because its cache is cold, to the idle
        // time that made it so. Carried down to the re-pin line, which otherwise names a
        // HARD gate as the reason for every move. Purely for the log.
        let mut cold_pin_idle_ms: Option<i64> = None;
        // Built once here, read by every hold-or-move decision below.
        let warm = self.warm_window(affinity);

        // Affinity fast-path: honour an existing pin when it is still usable. Read
        // the pin under the affinity lock, then DROP that lock before taking the
        // accounts lock (never nest the two — that is the documented deadlock).
        if let Some(key) = affinity {
            // (1) UNDER THE AFFINITY LOCK ONLY: read this session's pin `X` and, in
            // the same critical section, tally the per-account pinned-session counts
            // that the load-balancing decision needs. Drop this lock before taking
            // the accounts lock — the two are NEVER held simultaneously (the
            // documented deadlock).
            let (pinned, counts) = {
                let pins = self.affinity.lock().unwrap_or_else(PoisonError::into_inner);
                let pinned = pins.get(&key).map(|&(idx, touched_ms)| (idx, touched_ms));
                // Only pins that are plausibly LIVE count as load — a days-dead pin
                // must not make its account read as busy. See `PIN_LOAD_IDLE_MS`.
                let mut counts: HashMap<usize, usize> = HashMap::new();
                for (&pin_key, &(idx, touched_ms)) in pins.iter() {
                    if pin_counts_as_load(pin_key == key, touched_ms, now_ms) {
                        *counts.entry(idx).or_insert(0) += 1;
                    }
                }
                (pinned, counts)
            };
            if let Some((idx, pin_touched_ms)) = pinned {
                // A PIN NEVER CROSSES PROVIDERS.
                //
                // `eligible` refuses an account of the other provider, but this
                // fast path does not end at `eligible`: the over-threshold
                // pin-honor serve and the `tried` divert below both ask
                // `Self::account_hard_ok`, which judges the ACCOUNT and knows
                // nothing about the request, so a pin on the other provider's
                // account would be served a request its upstream cannot answer.
                // No request path pins a session there on purpose (it takes two
                // providers' session keys colliding); this makes it harmless if
                // one ever does.
                //
                // A DIVERT and not a re-key, the same family as the strict-group
                // divert below: the mismatch is a fact about THIS request, so it
                // is served by its own provider's pool and the pin stays where
                // the session it belongs to is warm.
                let other_provider = {
                    let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                    accounts.get(idx).is_some_and(|a| a.provider != provider)
                };
                // A STRICT GROUP ASK OUTRANKS AN EXISTING PIN, and once it
                // did not.
                //
                // Every eligibility call in this fast-path passes `group: None`
                // on purpose, a warm pin is not re-litigated against a per-request
                // PREFERENCE, and `Self::reserved_blocks` answers `false` for
                // any `Some(g)`, so no gate below looks at group MEMBERSHIP at
                // all. A session that had pinned account `X` therefore kept
                // being served on `X` even when the request asked strictly for
                // a group `X` is not in. For `tcr run --group` that is a leak
                // out of the group an operator typed; for a LEASE it is worse,
                // because the lender's serving leg expresses a lease's
                // scope as exactly this header
                // ([`crate::peer::lease::ScopeRestriction::Group`]), so a
                // borrowed request whose session key collided with a local
                // session's pin was served on an account OUTSIDE the scope,
                // on somebody else's Mac, which is the one outcome the scope
                // exists to prevent.
                //
                // A DIVERT and not a re-key: the group ask is a fact about
                // THIS request, so it joins the paced / model-class /
                // short-hold family, the request is served inside the group
                // and the session keeps the pin its other traffic is warm on.
                // When the group has no free member the strict arm below
                // returns `None`, which is the honest refusal a strict ask is
                // owed, never a serve on the out-of-group pin.
                let out_of_strict_group = strict_group.is_some_and(|g| {
                    let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                    !accounts
                        .get(idx)
                        .is_some_and(|a| a.groups.iter().any(|carried| carried == g))
                });
                if other_provider {
                    let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                    keep_pin = Some(idx);
                    divert_reason = Some("other-provider");
                    divert_until_ms = accounts
                        .get(idx)
                        .and_then(|a| a.rate_limited_until_ms)
                        .unwrap_or(0);
                } else if out_of_strict_group {
                    let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                    keep_pin = Some(idx);
                    divert_reason = Some("group-strict");
                    divert_until_ms = accounts
                        .get(idx)
                        .and_then(|a| a.rate_limited_until_ms)
                        .unwrap_or(0);
                } else if tried.contains(&idx) {
                    // The pin already failed THIS request upstream. That is NOT proof
                    // the account is gone — a transient blip (a dropped connection, a
                    // 5xx) leaves every ACCOUNT-level gate clear. So divert this
                    // request and keep the pin. Self-healing: a DURABLE failure arms a
                    // hold or sets `rejected`, both of which fail `account_hard_ok`, so
                    // the re-key still happens — one request later, on evidence.
                    //
                    // `account_hard_ok`, not `hard_ok`: the model-class gate is about
                    // THIS request, so a Fable-exhausted pin that a Fable request
                    // already tried still keeps its pin for the session's Opus turns.
                    let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                    if accounts.get(idx).is_some_and(|a| {
                        Self::account_hard_ok(
                            a,
                            now_ms,
                            group,
                            &groups.reserved,
                            &groups.parked,
                            warm,
                        )
                    }) {
                        keep_pin = Some(idx);
                        divert_reason = Some("pin-tried");
                        divert_until_ms = accounts
                            .get(idx)
                            .and_then(|a| a.rate_limited_until_ms)
                            .unwrap_or(0);
                    }
                } else {
                    let count_x = counts.get(&idx).copied().unwrap_or(0);
                    // Load-balancing migration ships OFF (see
                    // [`Self::load_balance_migration_enabled`]): a session that HAS a
                    // pin is by definition already warm, so moving it to even out
                    // counts is a guaranteed prompt-cache loss. Read the flag HERE,
                    // while NO other lock is held — the affinity lock dropped above
                    // and the accounts lock is taken below, so the config lock is
                    // never nested under either. Short-circuited on `count_x`, so the
                    // common lone-session path does not touch the config lock at all.
                    let migration_ok = count_x >= 2 && self.load_balance_migration_enabled();
                    // (2) UNDER THE ACCOUNTS LOCK ONLY: confirm the pin `X` is still
                    // usable, then — and only when migration is ENABLED and >=2
                    // sessions stack on `X` — look for the least-loaded ELIGIBLE
                    // account `Y` that strictly improves balance
                    // (`count(Y)+1 < count(X)`). Stamp whichever we settle on.
                    // `None` means `X` is ineligible → fall through to the normal
                    // pick/re-pin path (which already handles a dead pin).
                    let decision: Option<(usize, Option<(String, String)>)> = {
                        let mut accounts = self
                            .accounts
                            .write()
                            .unwrap_or_else(PoisonError::into_inner);
                        // The full (soft-inclusive) gate. Failing it does NOT bench the
                        // pin — it only means we drop into the hard-gate re-test below,
                        // which decides serve-anyway vs durable re-key.
                        let x_usable = accounts.get(idx).is_some_and(|a| {
                            Self::eligible(
                                a,
                                self.global_threshold,
                                self.fable_weekly_threshold,
                                &self.pacing,
                                true,
                                now,
                                now_ms,
                                is_fable,
                                provider,
                                None,  // no group PREFERENCE — honouring an existing pin
                                group, // but reservation is not a preference — real ask
                                &groups.reserved,
                                &groups.parked,
                            )
                        });
                        let pin_idle_ms = now_ms - pin_touched_ms;
                        if x_usable && pin_idle_ms >= warm.ms() {
                            // A COLD PIN PROTECTS NOTHING. The pin exists for the prompt
                            // cache, and a session idle past `warm` has none left on any
                            // account, so honouring it only overrides the normal pick's
                            // order (priority, then the weekly-reset urgency bucket) for no
                            // gain. Fall through to that pick, which may well choose the
                            // same account, and re-pin wherever it lands. This is the rule
                            // `affinity::PIN_TTL_MS` already applies to a pin restored at
                            // boot, now applied to the live map too. Reported 2026-11-07: a
                            // session resumed after a break went back to the account
                            // resetting in 3d 14h while the one resetting in 11h idled.
                            cold_pin_idle_ms = Some(pin_idle_ms);
                            None
                        } else if x_usable {
                            // Default: honour the pin. With migration disabled (the
                            // default), or when `count_x < 2` (a LONE session),
                            // `target` stays `idx` and nothing below runs, so this
                            // path is byte-identical to the pre-migration behaviour —
                            // a warm session's cache is never moved.
                            let mut target = idx;
                            let mut migrate_names: Option<(String, String)> = None;
                            if migration_ok {
                                // Least-loaded eligible target, ordered by
                                // (pinned-session-count asc, in_flight asc,
                                // last_selected_seq asc / LRU). A candidate qualifies
                                // ONLY if it strictly improves balance — this guard
                                // prevents thrash and equal-swaps.
                                let mut best: Option<usize> = None;
                                let mut best_key: Option<(usize, u32, u64)> = None;
                                for (cand, account) in accounts.iter().enumerate() {
                                    if cand == idx || tried.contains(&cand) {
                                        continue;
                                    }
                                    let count_y = counts.get(&cand).copied().unwrap_or(0);
                                    if count_y + 1 >= count_x {
                                        continue;
                                    }
                                    // Only migrate onto an ELIGIBLE account — the same
                                    // gate the pin honours (disabled/error/rate-limit/
                                    // quota/pacing), so a throttled `Y` is never chosen.
                                    if !Self::eligible(
                                        account,
                                        self.global_threshold,
                                        self.fable_weekly_threshold,
                                        &self.pacing,
                                        true,
                                        now,
                                        now_ms,
                                        is_fable,
                                        provider,
                                        // `strict_group`, not `None`: the fourth door out of a
                                        // strict group, and the one that returns from inside the
                                        // affinity fast path. A migration target is a NEW pin, not
                                        // a re-litigation of the existing one, so it is held to the
                                        // same strict ask every other candidate loop in this
                                        // function already is.
                                        strict_group,
                                        group, // reservation is not a preference, real ask
                                        &groups.reserved,
                                        &groups.parked,
                                    ) {
                                        continue;
                                    }
                                    let cand_key =
                                        (count_y, account.in_flight, account.last_selected_seq);
                                    if best_key.is_none_or(|b| cand_key < b) {
                                        best = Some(cand);
                                        best_key = Some(cand_key);
                                    }
                                }
                                if let Some(y) = best {
                                    let x_name =
                                        accounts.get(idx).map_or_default(|a| a.name.clone());
                                    let y_name = accounts.get(y).map_or_default(|a| a.name.clone());
                                    migrate_names = Some((x_name, y_name));
                                    target = y;
                                }
                            }
                            // Stamp the chosen account so a second session's LRU steers
                            // away from an account already busy under a pin.
                            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                            if let Some(account) = accounts.get_mut(target) {
                                account.last_selected_seq = tick;
                            }
                            Some((target, migrate_names))
                        } else {
                            // Re-test the SAME account against the ACCOUNT-level HARD
                            // gates ALONE to separate a per-REQUEST yield from a real
                            // block. Passing there means the pin cleared
                            // disabled/error/hold/rejected and only tripped a gate that
                            // describes this one request — the utilization threshold
                            // (`quota.is_near`), the concurrency-cap / min-spacing
                            // pacing gate, or the model-class gate. None of those is
                            // proof this account is gone. Failing it IS a genuine HARD
                            // block → fall through to the normal pick, which durably
                            // re-keys (the session's account really is gone).
                            //
                            // The four then part ways, because they are not the same
                            // kind of claim (see `Self::paced_out`, `Self::model_blocked`,
                            // `Self::hold_clears_while_warm`):
                            //
                            //  - PACED OUT → yield THIS request and keep the pin, as
                            //    before. Our own concurrency is a fact we measure
                            //    exactly, and spreading a burst is what the cap is for.
                            //
                            //  - HELD, BUT THE HOLD CLEARS WHILE THE CACHE IS WARM →
                            //    same shape. A hold is a TIMER, not a death, and the
                            //    timers we arm are mostly short (a no-guidance park is
                            //    15s + jitter). Diverting the one request costs one cold
                            //    prefix; re-keying throws away a cache that would still
                            //    have been warm when the account came back, and the
                            //    session never returns to it. A hold that OUTLIVES the
                            //    cache is a different claim and fails `account_alive`
                            //    above — see `CACHE_WARM_HOLD_SECS`.
                            //
                            //  - MODEL-CLASS BLOCKED (a Fable request, Fable weekly
                            //    exhausted) → same shape: this account cannot answer
                            //    THIS request, and answers every other model class
                            //    fine. Divert the one request, keep the pin — otherwise
                            //    a one-line title call re-keys a 200k-token Opus
                            //    conversation onto a cold account.
                            //
                            //  - OVER THE UTILIZATION THRESHOLD → SERVE THE PIN, UNLESS
                            //    the session has also gone IDLE-COLD (no request in
                            //    [`CACHE_WARM_HOLD_SECS`]), in which case there is
                            //    nothing left to protect and we fall through to a
                            //    durable re-key instead (see below). That threshold is
                            //    our own arithmetic over headers stale by minutes, and
                            //    Anthropic keeps answering 200s for accounts it
                            //    benches. Keeping the pin while still diverting the
                            //    request — the earlier half-fix — bought nothing: the
                            //    pin survived and the request paid the cold prefix
                            //    anyway (measured live: a 44.4% account-switch rate on
                            //    SUCCESSFUL serves, with zero pacing events and zero
                            //    hard failures). This is the behaviour
                            //    `select_revalidation`'s PIN-HONOR path already had,
                            //    hoisted to where it is reachable; both log the
                            //    identical line so they grep together.
                            //
                            //    Measured 2026-09-11 (four retained log days): a
                            //    morning burst of 23,745 requests in four hours ran on
                            //    8 of 18 accounts because affinity pins held every
                            //    session where it started; ten accounts sat idle while
                            //    those eight drained, and the whole pool went empty for
                            //    six hours (525 requests returned 429 to clients). A
                            //    session idle long enough for its own prompt cache to
                            //    be cold anyway has nothing to lose by moving — this is
                            //    the one rule that would have spread that burst, and it
                            //    needs no config flag: it fires only when the cache is
                            //    already cold by the crate's own constant AND the
                            //    account is over the soft threshold, so there is
                            //    nothing to opt out of.
                            let account_alive = accounts.get(idx).is_some_and(|a| {
                                Self::account_hard_ok(
                                    a,
                                    now_ms,
                                    group,
                                    &groups.reserved,
                                    &groups.parked,
                                    warm,
                                )
                            });
                            let model_blocked = accounts.get(idx).is_some_and(|a| {
                                Self::model_blocked(
                                    a,
                                    self.global_threshold,
                                    self.fable_weekly_threshold,
                                    now,
                                    is_fable,
                                )
                            });
                            let paced_out = accounts
                                .get(idx)
                                .is_some_and(|a| Self::paced_out(a, &self.pacing, now_ms));
                            // Reached only when `account_alive` holds, so any live hold
                            // left here is by construction a short one — named anyway so
                            // the branch does not depend on the ordering above.
                            let held_briefly = accounts
                                .get(idx)
                                .is_some_and(|a| Self::hold_clears_while_warm(a, now_ms, warm));
                            // The same shape for a 5h rejection that rolls while the
                            // cache is warm: `account_alive` already let it through,
                            // and it must divert the request rather than serve it.
                            let rejected_briefly = accounts.get(idx).is_some_and(|a| {
                                Self::rejection_clears_while_warm(a, now_ms, warm)
                            });
                            if !account_alive {
                                // Genuinely gone: fall through and durably re-key.
                                None
                            } else if held_briefly || rejected_briefly || model_blocked || paced_out
                            {
                                // Yield this ONE request to the fall-through pick; the
                                // re-pin at the bottom re-inserts the OLD index and now
                                // logs WHICH of the per-request gates diverted it.
                                keep_pin = Some(idx);
                                divert_reason = Some(if held_briefly {
                                    "short-hold"
                                } else if rejected_briefly {
                                    "rejection-clears-warm"
                                } else if model_blocked {
                                    "model-class"
                                } else {
                                    "paced"
                                });
                                divert_until_ms = accounts
                                    .get(idx)
                                    .and_then(|a| {
                                        a.rate_limited_until_ms.or_else(|| {
                                            a.quota.five_hour_rejection_reset().map(odt_to_ms)
                                        })
                                    })
                                    .unwrap_or(0);
                                None
                            } else {
                                let idle_ms = pin_idle_ms;
                                // An eligible alternative must exist before we give up
                                // this pin — otherwise falling through would turn a soft
                                // threshold into a hard 429 the account itself never
                                // produced. Same gate the normal pool pick applies (the
                                // soft-inclusive `eligible`, no group narrowing), just
                                // asked here first so a genuinely fleet-wide soft
                                // threshold still gets a last-resort serve.
                                let has_eligible_alternative =
                                    accounts.iter().enumerate().any(|(cand, account)| {
                                        cand != idx
                                            && Self::eligible(
                                                account,
                                                self.global_threshold,
                                                self.fable_weekly_threshold,
                                                &self.pacing,
                                                true,
                                                now,
                                                now_ms,
                                                is_fable,
                                                provider,
                                                None,
                                                group,
                                                &groups.reserved,
                                                &groups.parked,
                                            )
                                    });
                                if idle_ms >= warm.ms() && has_eligible_alternative {
                                    // Idle-cold, and there is somewhere better to go: the
                                    // prompt cache is already dead by our own clock, so
                                    // serving the pin protects nothing — fall through to
                                    // the normal pick, which durably re-keys onto the
                                    // least-loaded ELIGIBLE account (the same path a
                                    // genuinely dead pin takes).
                                    let util = accounts
                                        .get(idx)
                                        .map_or_default(|a| a.quota.max_utilization(now, is_fable));
                                    let name = accounts.get(idx).map_or_default(|a| a.name.clone());
                                    tracing::info!(
                                        idle_s = idle_ms / 1000,
                                        cache_warm_hold_secs = warm.secs(),
                                        account = %name,
                                        utilization = util,
                                        is_fable,
                                        "revalidation-rekey (idle-cold): session idle {}s >= {}s on over-threshold account {} utilization {}; re-keying",
                                        idle_ms / 1000,
                                        warm.secs(),
                                        name,
                                        util
                                    );
                                    cold_pin_idle_ms = Some(idle_ms);
                                    None
                                } else {
                                    self.serve_pinned_over_threshold(
                                        &mut accounts,
                                        idx,
                                        now,
                                        is_fable,
                                    );
                                    // `target == idx`, so the commit section below is a
                                    // plain pin refresh: OLD index, fresh `now_ms`.
                                    Some((idx, None))
                                }
                            }
                        }
                    };
                    if let Some((target, migrate_names)) = decision {
                        // (3) UNDER THE AFFINITY LOCK ONLY: commit the pin. Accounts
                        // lock already dropped — never nest.
                        //
                        // TOCTOU close: the `counts` driving the migration decision
                        // were read in section (1) and the lock was then dropped, so a
                        // concurrent select on another session stacked on the same X
                        // could have decided the SAME idle Y in parallel. Committing
                        // both blindly would OVER-migrate (Y over-stacks, X empties —
                        // the inverse of the goal, and it can oscillate). So for a
                        // MIGRATION we RE-VALIDATE against FRESH counts re-tallied from
                        // the live map under this same lock that mutates it: the pin
                        // must still be X AND `count(target)+1 < count(X)` must still
                        // hold. If not, ABORT the move and keep the existing pin. No
                        // accounts lock is taken here (the re-check needs only the
                        // affinity-map counts + the already-chosen target; the next
                        // select re-checks eligibility anyway).
                        let mut pins = self.affinity.lock().unwrap_or_else(PoisonError::into_inner);
                        let mut committed = target;
                        if target != idx {
                            let still_pinned_x = pins.get(&key).map(|&(i, _)| i) == Some(idx);
                            let mut count_x_now = 0usize;
                            let mut count_t_now = 0usize;
                            // Same live-load filter as section (1), so the re-check
                            // cannot disagree with the decision it re-validates.
                            for (&pin_key, &(i, touched_ms)) in pins.iter() {
                                if !pin_counts_as_load(pin_key == key, touched_ms, now_ms) {
                                    continue;
                                }
                                if i == idx {
                                    count_x_now += 1;
                                }
                                if i == target {
                                    count_t_now += 1;
                                }
                            }
                            // Strictly-improves-balance guard, re-checked on fresh state.
                            if still_pinned_x && count_t_now + 1 < count_x_now {
                                if let Some((x_name, y_name)) = migrate_names {
                                    tracing::info!(
                                        "affinity: migrate session off {} (live_pins={}) -> {} (live_pins={})",
                                        x_name,
                                        count_x_now,
                                        y_name,
                                        count_t_now
                                    );
                                }
                            } else {
                                // The decision went stale between sections (a sibling
                                // select already rebalanced): keep the current pin.
                                committed = idx;
                            }
                        }
                        // Re-pin the session (migration target, or the honoured/kept X)
                        // and refresh its last-touch for LRU eviction.
                        pins.insert(key, (committed, now_ms));
                        // The map changed, so the on-disk copy is now behind. A
                        // relaxed store under the lock: no allocation, no second
                        // lock, nothing that can block the request path.
                        self.mark_affinity_dirty();
                        return Some(committed);
                    }
                }
            }
        }

        // Control-account routing overlay (part 2 — three-way split of an
        // UNPINNED request, see `classify_request`). Gated on `keep_pin.is_none()`
        // so it NEVER touches a session that already has a pin — even one only
        // diverted for this one request (invariant 1: a pin is re-keyed ONLY by
        // an ACCOUNT-level hard gate, never by this preference).
        if keep_pin.is_none() {
            match request_class {
                RequestClass::Inference => {
                    // Pool-only — no preference here; the exclusion lives in the
                    // normal pick below so it also covers the case where control
                    // points at an ENABLED (pooled) account.
                }
                RequestClass::Noise => {
                    // Follow the connection: reuse whichever account this
                    // connection already served, while it is still eligible
                    // (the ordinary gate — no disabled-bypass here, unlike
                    // control preference). `tried` still excludes an account
                    // that already failed THIS request.
                    if let Some(k) = conn_key
                        && let Some(idx) = self.conn_affinity_get(k, now_ms)
                        && !tried.contains(&idx)
                    {
                        let usable = {
                            let accounts =
                                self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                            accounts.get(idx).is_some_and(|a| {
                                Self::eligible(
                                    a,
                                    self.global_threshold,
                                    self.fable_weekly_threshold,
                                    &self.pacing,
                                    true,
                                    now,
                                    now_ms,
                                    is_fable,
                                    provider,
                                    // `strict_group`, not `None`: a GROUP-scoped lease
                                    // carries no `tried` seeding the way an account-set
                                    // scope does, so for it `strict_group` is the whole
                                    // enforcement, a connection-affinity account outside
                                    // it must never be followed onto.
                                    strict_group,
                                    group, // reservation is not a preference, real ask
                                    &groups.reserved,
                                    &groups.parked,
                                )
                            })
                        };
                        if usable {
                            let mut accounts = self
                                .accounts
                                .write()
                                .unwrap_or_else(PoisonError::into_inner);
                            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                            if let Some(account) = accounts.get_mut(idx) {
                                account.last_selected_seq = tick;
                                tracing::info!(
                                    account = %account.name,
                                    conn = k,
                                    "control: noise request follows its connection's account"
                                );
                            }
                            drop(accounts);
                            self.conn_affinity_record(k, idx, now_ms);
                            return Some(idx);
                        }
                    }
                    // No usable connection-pinned account — fall through to the
                    // normal pick below, which re-records `conn_affinity` once
                    // it settles on a winner.
                }
                RequestClass::ControlPreferred => {
                    if let Some(control_idx) = self.control()
                        && !tried.contains(&control_idx)
                    {
                        let usable = {
                            let accounts =
                                self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                            accounts.get(control_idx).is_some_and(|a| {
                                // `control_eligible` deliberately bypasses `disabled` and
                                // every other narrowing gate (see its own doc), it is NOT
                                // a substitute for the group scope. A STRICT ask (an
                                // operator's `--group`, or a lease's scope) is not a
                                // preference this designated-identity route may ignore: a
                                // GROUP-scoped lease carries no `tried` seeding, so this is
                                // the whole enforcement for it, same reasoning as the
                                // connection-affinity overlay just above.
                                //
                                // Nor is it a substitute for the provider, asked first:
                                // the control account is an Anthropic identity
                                // (`assemble` ignores a Codex one), and every
                                // `/backend-api/` path but the inference one classifies
                                // as control-preferred.
                                a.provider == provider
                                    && Self::control_eligible(a, now_ms)
                                    && strict_group
                                        .is_none_or(|g| a.groups.iter().any(|carried| carried == g))
                            })
                        };
                        if usable {
                            let mut accounts = self
                                .accounts
                                .write()
                                .unwrap_or_else(PoisonError::into_inner);
                            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                            if let Some(account) = accounts.get_mut(control_idx) {
                                account.last_selected_seq = tick;
                                tracing::info!(
                                    account = %account.name,
                                    "control: routing unpinned identity-plane request to the control account"
                                );
                            }
                            return Some(control_idx);
                        }
                    }
                    // Control unset, held, errored, rejected, or already tried —
                    // degrade to normal rotation (no second designated identity).
                }
            }
        }

        // Sticky-destination overlay (§4.3): when this request diverted
        // (`keep_pin` is `Some` — the exact mirror of the control-routing
        // overlay above, which is gated on `keep_pin.is_none()` and is
        // therefore already skipped for every divert), prefer this episode's
        // already-warmed destination over spending a fresh one. Shape copied
        // from the NOISE overlay (`:506-548` pre-this-change): read the
        // remembered index, check `!tried.contains` and `eligible`; otherwise
        // fall through to the normal pick below, which records the winner as
        // this episode's destination on the way out (§4.2).
        //
        // The ledger is read here in its OWN critical section — no accounts
        // or affinity lock is held at this point (both dropped above) — and
        // `budget = 0` is hardcoded: stickiness never consults the budget
        // (see this file's `divert_verdict` doc-comment); the real
        // `divert_budget()` read is unit E's, at its own (blocking) call
        // site.
        let sticky_pick: Option<usize> = if let Some(pin_idx) = keep_pin {
            let episode = affinity.and_then(|key| {
                self.divert_ledger
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&key)
                    .copied()
            });
            match divert_verdict(episode, pin_idx, divert_until_ms, 0) {
                DivertVerdict::Sticky(sticky) if !tried.contains(&sticky) => {
                    let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                    let usable = accounts.get(sticky).is_some_and(|a| {
                        Self::eligible(
                            a,
                            self.global_threshold,
                            self.fable_weekly_threshold,
                            &self.pacing,
                            true,
                            now,
                            now_ms,
                            is_fable,
                            provider,
                            // `strict_group`, not `None`: the second door out
                            // of a strict group, and the one the fast-path fix
                            // above would otherwise leave open. Stickiness
                            // exists so a diverted session keeps landing on ONE
                            // warm alternative, and for a PREFER-only (spill)
                            // group it still bypasses the narrowing exactly as
                            // before. But a STRICT ask, an operator's
                            // `--group`, or a lease's scope, is not a
                            // preference to bypass: a sticky destination
                            // outside it is the same out-of-scope serve, one
                            // divert later.
                            strict_group,
                            group, // reservation is not a preference, real ask
                            &groups.reserved,
                            &groups.parked,
                        )
                    });
                    usable.then_some(sticky)
                }
                _ => None,
            }
        } else {
            None
        };
        let via_sticky = sticky_pick.is_some();

        // Normal LRU/priority pick (identical to the pre-affinity path when no
        // control account is set). The accounts lock is scoped so it is released
        // before we touch the affinity lock again for the re-pin below.
        let best = if let Some(sticky) = sticky_pick {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
            if let Some(account) = accounts.get_mut(sticky) {
                account.last_selected_seq = tick;
            }
            Some(sticky)
        } else {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);

            // By default inference NEVER selects the control account, so that
            // inference load can never burn the one identity the control plane
            // resolves to (`control-account-bridge-coder.md` §"the fix"). The
            // default-disabled control account is already excluded by
            // `eligible`'s own disabled check, so this only does real work when
            // `controlAccount` names a non-disabled account.
            // Modeled as an extra `tried` member rather than a new parameter
            // threaded through `pick_eligible`/`pick_least_loaded`, so both
            // passes (and the pacing/least-loaded fallback) honour it for free.
            //
            // Hoisted into its own binding (it used to be inlined in the `match`
            // below) because the group-miss classifier on the fallback path needs
            // the SAME answer: an account held out only by this exclusion is the
            // one case where a `--group` ask can never be satisfied by any retry,
            // and naming it requires knowing which index was excluded and why.
            //
            // Opt-in carve-out: an explicit `--group g` ask is allowed to select
            // the control account when the control account itself carries `g`
            // AND `g` has opted in via `groupSettings.g.allowControlAccount`
            // (`tcr group allow-control g`). Both conditions are checked here
            // rather than deferred to `eligible`, so this stays the single place
            // that decides whether the control account is even a CANDIDATE for
            // inference — `pool_pick_respects_control_reserve`'s reserve-floor
            // guard already applies unconditionally once the account becomes
            // selectable, so this carve-out stays safe without further changes.
            //
            // Fleet-wide carve-out: `controlPooled` drops the exclusion for
            // EVERY inference pick, leaving `pool_pick_respects_control_reserve`
            // as the only thing holding the control account back — the same
            // reserve floor the group carve-out above already relies on, applied
            // to the whole pool instead of one group. It is checked here, in the
            // one place that decides candidacy, so the group carve-out, the
            // group-miss classifier and both pick passes all agree for free.
            //
            // This is a deliberate trade, not a tightening: see
            // `Config::control_pooled` for why the reserve is a floor over
            // LAGGING headers and what it costs when upstream wins the race.
            let control_excluded = self.control_excluded(request_class, group, &accounts, &groups);
            let pool_tried = exclude_control(tried, control_excluded);
            let ask = PoolAsk {
                tried: &pool_tried,
                now,
                now_ms,
                is_fable,
                provider,
                group,
                groups: &groups,
                // A lock returned above, before the pool.
                locked: None,
                warm,
            };

            // First pass: honour pacing (skip accounts at the concurrency cap or
            // inside the min-spacing window) AND, when `--group` was requested,
            // prefer that group. With pacing OFF and no group this is byte-identical
            // to the pre-pacing pick.
            let mut best = self.pick_eligible(&accounts, &ask);

            // An explicit `--group` ask is STRICT BY DEFAULT (2026-09-13), and
            // strictness is checked BEFORE the soft fallback below can widen the
            // pick: when the asked-for group has no free member, the request
            // refuses rather than spilling into the whole pool. This used to be
            // conditional on `reserved`; it no longer is. A group ask an
            // operator typed is being served by that group or not at all,
            // independent of whether the group is also `reserved` (which today
            // governs only the INBOUND direction — keeping other traffic OUT —
            // see `GroupSettings::reserved`'s doc).
            //
            // Two carve-outs from "strict by default":
            //   - `groupSettings.<g>.spillToPool` is the escape hatch back to the
            //     pre-2026-09-13 spill behaviour, for an operator who wants it.
            //     Every existing prefer-and-fall-back test now sets this so it
            //     keeps exercising the OLD rule under its new name.
            //   - A RESERVED group ignores `spillToPool` and is always strict:
            //     spilling a reserved group's own traffic into the pool would
            //     hand it to an account other traffic cannot reach any other
            //     way, which contradicts what `reserved` promises inbound.
            //
            // Measured on the live fleet 2026-09-01: pool traffic diverted onto a
            // reserved-group member 127 times in a day (116 requests, 43 of them
            // Opus), rate-limiting it (`hold_seconds=17`); three seconds later that
            // group's OWN `--group` ask found its only member gated and spilled to
            // the pool — 33 times, every one `reason="all-members-unavailable"`.
            // That measurement was the inbound leak causing an outbound one on a
            // RESERVED group specifically; it is also the reasoning for making
            // strict-by-default apply to every group, not only reserved ones — an
            // operator who explicitly asks for a group has the same expectation
            // whether or not that group also happens to be reserved.
            //
            // Returning `None` is not dropping the request: it hands the caller's
            // exhaustion ladder (`proxy.rs`) the same `None` a truly exhausted fleet
            // produces, which soft-waits once for a transient park (the 17s hold
            // above) and answers an honest 429 only for real exhaustion.
            // `strict_group` is computed once at the top of this function, the
            // affinity fast-path needs the same value, and one expression
            // evaluated twice is one expression that drifts.

            // Soft fallback (CRITICAL — pacing must never DROP a servable request,
            // and a group with no current capacity must never either): if the first
            // pass found nothing, retry ignoring BOTH pacing and the group, serving
            // the least-loaded (lowest in_flight, then the normal LRU key) account in
            // the WHOLE pool. With pacing OFF and no group, the first pass and this
            // pass use identical eligibility, so a None first pass ⟹ None here too —
            // default-OFF stays byte-identical (no spurious fallback, no log).
            if best.is_none() {
                if let Some(g) = strict_group {
                    // Same classifier the fallback arm uses, so an operator counting
                    // `reason=` tokens sees one vocabulary across both outcomes.
                    let miss = Self::classify_group_miss(
                        &accounts,
                        tried,
                        control_excluded,
                        self.global_threshold,
                        self.fable_weekly_threshold,
                        &self.pacing,
                        now,
                        now_ms,
                        is_fable,
                        provider,
                        g,
                        &groups.reserved,
                        &groups.parked,
                    );
                    tracing::info!(
                        group = g,
                        reason = miss.as_str(),
                        "group: strict — refusing to fall back to the whole pool, returning no account — {}",
                        miss.explain(),
                    );
                } else if let Some(idx) = self.pick_least_loaded(&accounts, &ask) {
                    if let Some(account) = accounts.get(idx) {
                        // This fallback fires whenever the first (group- and
                        // pacing-respecting) pass came up empty — which can be pacing
                        // alone, the group alone, or both together. Name whichever
                        // constraints were actually IN PLAY on the first pass rather
                        // than always blaming pacing: an operator debugging why
                        // `--group` did not land where expected must not read a log
                        // line about pacing when pacing was never the reason.
                        if let Some(g) = group {
                            let miss = Self::classify_group_miss(
                                &accounts,
                                tried,
                                control_excluded,
                                self.global_threshold,
                                self.fable_weekly_threshold,
                                &self.pacing,
                                now,
                                now_ms,
                                is_fable,
                                provider,
                                g,
                                &groups.reserved,
                                &groups.parked,
                            );
                            tracing::info!(
                                account = %account.name,
                                in_flight = account.in_flight,
                                group = g,
                                reason = miss.as_str(),
                                "group: falling back to the whole pool (group ignored), serving least-loaded — {}",
                                miss.explain(),
                            );
                        } else {
                            tracing::info!(
                                account = %account.name,
                                in_flight = account.in_flight,
                                "pacing: all accounts paced, serving least-loaded"
                            );
                        }
                    }
                    best = Some(idx);
                }
            }

            // Fable last resort (three-tier gate, replacing the old hard two-tier
            // one): reached only when BOTH passes above already came up empty AND
            // a strict group did not just intentionally return `None`
            // (`strict_group` is a per-group PRIVACY/EXCLUSIVITY decision, never a
            // candidate pool to widen). The threshold applied by `model_blocked` above is a
            // PREDICTION (`switchThreshold` against `7d_oi`, observed sitting at
            // 0.95-0.98 on a real fleet); upstream — `Quota::reject_model_weekly`,
            // the only writer of a genuine `1.0` — is the AUTHORITY. A prediction
            // must never itself produce the user-facing 429; only relax the gate
            // once every other option is exhausted, so on a healthy fleet this
            // path costs nothing (identical to today) and on a mispredicted one it
            // spends one real request to convert the guess into a fact.
            if best.is_none()
                && is_fable
                && strict_group.is_none()
                && let Some(idx) = self.pick_fable_last_resort(&accounts, &ask)
            {
                if let Some(account) = accounts.get(idx) {
                    let seven_day_oi = account
                        .quota
                        .effective(crate::records::quota::QuotaBucket::SevenDayOi, now)
                        .unwrap_or(0.0);
                    tracing::info!(
                        account = %account.name,
                        seven_day_oi,
                        "fable: last resort — every account is over the predicted \
                         Fable-weekly threshold and none was rejected upstream; \
                         trying the freshest"
                    );
                }
                best = Some(idx);
            }

            // Stamp the chosen account so the next select prefers a different one.
            if let Some(idx) = best {
                let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
                if let Some(account) = accounts.get_mut(idx) {
                    account.last_selected_seq = tick;
                }
            }
            best
        };

        // Record this divert's destination in the episode ledger (§4.2 rule
        // 3: written only after the affinity guard below has dropped, in its
        // own critical section — never nested under it). Every divert
        // records, not just sticky ones, so a fresh divert's destination
        // becomes the NEXT divert's sticky offer.
        if let (Some(key), Some(pin_idx), Some(dest)) = (affinity, keep_pin, best) {
            self.record_divert(key, pin_idx, divert_until_ms, dest, now_ms);
        }

        // Record the pick as this session's pin (initial pin, or re-pin on
        // migration). Skipped entirely when affinity is off, so the map stays
        // empty on the disabled path.
        if let (Some(key), Some(idx)) = (affinity, best) {
            // A per-REQUEST-gated pin (paced, model-class blocked, or transiently in
            // `tried`) diverted THIS request only: re-insert the OLD index, not the
            // account we are about to serve. The insert still has to happen — this
            // `now_ms` is one of only two writers of the last-touch stamp that the
            // AFFINITY_CAP eviction sorts on, so skipping it would make a
            // heavily-diverted session the eviction victim.
            let pin_idx = keep_pin.unwrap_or(idx);
            let moved_off = {
                let mut pins = self.affinity.lock().unwrap_or_else(PoisonError::into_inner);
                let previous = pins.get(&key).map(|&(i, _)| i);
                // Capped at AFFINITY_CAP: an insert over it evicts the oldest-last-touch
                // entry other than this one.
                pins.insert(key, (pin_idx, now_ms));
                self.mark_affinity_dirty();
                // Only a real change is worth a line: `None` on an initial pin, and on
                // a soft divert (where `pin_idx` is the unchanged old index).
                previous.filter(|&p| p != pin_idx)
            };
            // The only durable re-key that happens here, and until now it logged
            // nothing — which is why a 39.4% live account-switch rate went unnoticed.
            // Reaching this line now means the pin failed an ACCOUNT-level HARD gate,
            // so the line doubles as the audit trail for the invariant.
            // The accounts lock is taken ONLY after the affinity lock has dropped
            // above; the two are never held simultaneously.
            if let Some(previous) = moved_off {
                let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                let old_name = accounts.get(previous).map_or("?", |a| a.name.as_str());
                let new_name = accounts.get(pin_idx).map_or("?", |a| a.name.as_str());
                if let Some(idle_ms) = cold_pin_idle_ms {
                    tracing::info!(
                        "affinity: re-pin session {} off {} -> {} (cache cold: idle {}s)",
                        short_session_id(key),
                        old_name,
                        new_name,
                        idle_ms / 1000
                    );
                } else {
                    tracing::info!(
                        "affinity: re-pin session {} off {} -> {} (pin failed an ACCOUNT HARD gate)",
                        short_session_id(key),
                        old_name,
                        new_name
                    );
                }
            }
            // The SOFT counterpart, and until now it emitted nothing at all: the pin
            // survived, a DIFFERENT account served, and no line said so — which is why
            // off-pin serves were unattributable in the live logs. Normally exclusive
            // with `moved_off` (a divert re-inserts the OLD index, so `previous ==
            // pin_idx` and `moved_off` is `None`); a concurrent select that re-pinned
            // this session between the two reads can make both fire, which is two
            // honest lines about one request, not a defect. The accounts lock is taken
            // only after the affinity lock dropped above — never nested.
            if let Some(reason) = divert_reason {
                let (pin_name, serve_name) = self.pin_and_serve_names(pin_idx, idx);
                tracing::info!(
                    "affinity: divert session {} pin {} -> serving {} (reason={}, pin kept)",
                    short_session_id(key),
                    pin_name,
                    serve_name,
                    reason
                );
                // A SIBLING line, not a replacement (§4.8): the line above is a
                // parsed interface (`divert-history.py:33-36`) and stays exactly
                // as it was. This one exists only to prove fan-out was avoided —
                // the same regex cannot match it (different second word).
                if via_sticky {
                    tracing::info!(
                        "affinity: divert-sticky session {} pin {} -> reusing {} (reason={}, warm=true)",
                        short_session_id(key),
                        pin_name,
                        serve_name,
                        reason
                    );
                }
            }
        }

        // Re-record the connection's account for NOISE traffic that fell
        // through to the normal pick above (no usable conn-pinned account, or
        // affinity itself is off). A direct hit already returned early via
        // `conn_affinity_record` in the overlay; this is the "otherwise normal
        // rotation, and re-record" half of §2. Never touches `self.affinity`.
        if request_class == RequestClass::Noise
            && let (Some(k), Some(idx)) = (conn_key, best)
        {
            self.conn_affinity_record(k, idx, now_ms);
        }
        best
    }
}
