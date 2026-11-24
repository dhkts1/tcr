//! `select_revalidation`, the over-threshold serve the proxy falls back to, its valve and its pinned-serve helpers.

use super::*;

impl Manager {
    /// The provider-local revalidation deadline, shared by its read and spend paths.
    pub(super) const fn revalidation_valve(
        &self,
        provider: crate::provider::Provider,
    ) -> &std::sync::atomic::AtomicI64 {
        match provider {
            crate::provider::Provider::Anthropic => &self.next_revalidation_at_ms,
            crate::provider::Provider::Codex => &self.next_codex_revalidation_at_ms,
        }
    }

    /// LAST-RESORT serve when normal [`Self::select`] found nothing (the whole
    /// fleet reads over the SOFT switch threshold). Serves an account that Anthropic
    /// still allows, ignoring the soft utilization/pacing gates but honoring the
    /// HARD blocks:
    ///   - `disabled` / [`AccountStatus::Error`] → skip;
    ///   - ANY live rate-limit hold (`rate_limited_until_ms` in the future) → skip.
    ///     A SERVE decision honours the whole hold; only the PIN decision softens the
    ///     short ones (see [`Self::hard_ok`] and [`CACHE_WARM_HOLD_SECS`]);
    ///   - `quota.status == Some("rejected")` → skip (genuinely blocked; would 429);
    ///   - a Fable request whose model-scoped weekly (`7d_oi`) is a hard reject → skip
    ///     (that same account still serves every non-Fable model).
    ///
    /// TWO paths, mirroring [`Self::select`]'s never-nested lock discipline (read the
    /// pin under the affinity lock, DROP it before taking the accounts lock):
    ///  - **(A) PIN-HONOR** (cache-warm): when `affinity` is `Some(key)` and the
    ///    session's pinned account is not in `tried` and passes the HARD gates, serve
    ///    THAT pin — even over the soft threshold — to keep the operator's prompt
    ///    cache warm. NO revalidation throttle here (the global egress throttle
    ///    already paces); the pin is left as-is. [`Self::select`] now runs this same
    ///    serve-the-pin rule in its own affinity fast-path (identical log line), so a
    ///    servable pin is honoured there and this path is the belt-and-braces copy for
    ///    any caller that reaches revalidation directly.
    ///  - **(B) FALLBACK** (no pin, or the pin is rejected/held/tried/model-blocked):
    ///    the least-utilized surviving account wins (lowest
    ///    [`crate::quota::Quota::max_utilization`]; ties: LRU `last_selected_seq`).
    ///    The anti-storm throttle applies HERE ONLY — at most one NEW-account
    ///    revalidation per [`REVALIDATION_MIN_SPACING_MS`]; inside that window return
    ///    `None` (the caller emits the honest 429). The window is spent only once a
    ///    servable candidate exists. On success the session is RE-PINNED to the
    ///    chosen account, mirroring `select()`'s re-pin — EXCEPT when the pin was
    ///    passed over merely because it cannot serve THIS request: its MODEL CLASS
    ///    ([`Self::model_blocked`]) or a hold that clears while its cache is still
    ///    warm ([`Self::hold_clears_while_warm`]). Both are per-request facts, so
    ///    they re-write the OLD index instead (`keep_pin`, exactly as `select()`
    ///    does).
    ///
    /// The winner's `last_selected_seq` is stamped and ONE `tracing::info!` names the
    /// path (pin-honor vs fallback), the account, and its utilization.
    ///
    /// `path` is the request's query-stripped path, the argument [`Self::select`]
    /// takes, and for the same reason: its provider decides which accounts this
    /// serve may land on, ahead of every other gate. A mixed fleet whose Anthropic
    /// half reads over the threshold has its idle accounts on the OTHER provider,
    /// and the least-utilized pick below would otherwise reach for exactly those.
    pub fn select_revalidation(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
        path: &str,
    ) -> Option<usize> {
        // Live-reload group membership/settings — same call `select_with_group`
        // makes, so a session revalidated through THIS path also sees a just-
        // reserved group and re-keys (bridge's pin-reclaim-via-reload case).
        self.reload_groups_if_changed();

        // Anti-storm spacing on the FALLBACK path only: the upstream 429→hold and the
        // global egress throttle are the real backstops; this just stops a
        // synchronized burst from slamming one over-threshold account when the fleet
        // saturates. The pin-honor path is never throttled here.
        const REVALIDATION_MIN_SPACING_MS: i64 = 2000;

        let groups = self.groups();
        let now_ms = odt_to_ms(now);
        // Derived exactly as `select_with_group` derives them.
        let provider = crate::provider::Provider::of_path(path);
        let filtered_tried = self.exclude_unsupported_codex_exits(tried, provider);
        let tried = filtered_tried.as_ref();
        // The Fable weekly bucket is an Anthropic fact; a Codex request never consults it.
        let is_fable =
            provider.has_fable_bucket() && model.is_some_and(crate::model::is_fable_model);

        // HARD-gate predicate shared by both paths: the blocks that even a
        // revalidation serve must honor (soft utilization/pacing deliberately absent).
        // The request's provider is asked first, as `eligible` asks it: an account of
        // the other provider is not an over-threshold candidate, it is no candidate.
        //
        // `group: None` — this path (unlike `select_with_group`) carries no
        // `--group` context at all, so it cannot tell a grouped session's real
        // ask from an ungrouped one. Passing `None` is the conservative choice
        // for a HARD reservation gate: it can only ever narrow who this path
        // revalidates onto a reserved account, never widen it, so a reserved
        // account is never leaked to traffic this path cannot identify.
        let warm = self.warm_window(affinity);
        let ask = PoolAsk {
            tried,
            now,
            now_ms,
            is_fable,
            provider,
            group: None,
            groups: &groups,
            // A locked provider never reaches this fallback: the proxy answers it first.
            locked: None,
            warm,
        };
        // `tried` gates SERVING and nothing else, so it is part of this predicate: the pin
        // already failed THIS request upstream when it is in `tried`, so it cannot answer it.
        let servable = |idx: usize, account: &AccountRuntime| -> bool {
            self.pool_candidate(PoolPass::Revalidation, idx, account, &ask)
        };

        // Mirrors `select()`'s `keep_pin`: set to the OLD pin index when the pin
        // cannot serve THIS request but the ACCOUNT is alive, so the fallback below
        // serves elsewhere while re-writing the OLD index. Without it a Fable request
        // that reaches this path re-keys the session and the next Opus turn pays a
        // cold prefix — the same defect this file fixed in `select()`, one path over.
        let mut keep_pin: Option<usize> = None;
        // The pin's `rate_limited_until_ms` at the moment `keep_pin` was set — the
        // other half of the divert episode's identity (`select()`'s
        // `divert_until_ms`, mirrored here). Only meaningful when `keep_pin` is
        // `Some`; `divert_verdict` is never consulted otherwise.
        let mut pin_until_ms: i64 = 0;

        // (A) PIN-HONOR — read the pin under the affinity lock, DROP it, then check
        // the pin's HARD gates under the accounts lock (never nested). No throttle.
        if let Some(key) = affinity {
            let pinned = {
                let pins = self.affinity.lock().unwrap_or_else(PoisonError::into_inner);
                pins.get(&key).map(|&(idx, _)| idx)
            };
            if let Some(idx) = pinned {
                let mut accounts = self
                    .accounts
                    .write()
                    .unwrap_or_else(PoisonError::into_inner);
                // Whether the PIN MOVES is decided below, on ACCOUNT-level evidence
                // alone — see the comment there.
                if accounts.get(idx).is_some_and(|a| servable(idx, a)) {
                    self.serve_pinned_over_threshold(&mut accounts, idx, now, is_fable);
                    return Some(idx);
                }
                // The pin cannot serve THIS request → fall through to the fallback
                // path (lock drops at the end of this scope, before the affinity
                // lock below). Whether that fall-through also MOVES the pin depends
                // on WHY: a model-class block leaves the account alive for every
                // other class, a short hold leaves it alive for every LATER request,
                // and membership in `tried` says only that ONE request failed on it
                // — none of the three is evidence the account is gone, so in all of
                // them the pin stays. Only an ACCOUNT-level HARD block is a durable
                // re-key.
                //
                // This test used to sit INSIDE `if !tried.contains(&idx)`, so a pin
                // that merely failed this one request left `keep_pin` at `None` and
                // the fallback's `pins.insert` re-keyed the session for good — a
                // transient blip throwing away a warm prompt cache. `select()` takes
                // the opposite decision on the identical fact and documents why
                // (see its affinity fast-path); the two now agree.
                if accounts.get(idx).is_some_and(|a| {
                    Self::account_hard_ok(a, now_ms, None, &groups.reserved, &groups.parked, warm)
                }) {
                    keep_pin = Some(idx);
                    pin_until_ms = accounts
                        .get(idx)
                        .and_then(|a| a.rate_limited_until_ms)
                        .unwrap_or(0);
                }
            }
        }

        // Sticky-destination reuse for the FALLBACK path (design doc §4.4,
        // do-not #7: "not a second copy of the rule" — same ledger, same
        // `divert_verdict` predicate `select()`'s sticky overlay uses, one call
        // site here). Read the episode BEFORE the fallback's accounts lock is
        // taken below — the accounts lock PIN-HONOR held has already dropped
        // (its scope ended above), so this ledger read nests under neither
        // (§4.2 rule 1).
        //
        // `budget = 0`: stickiness never consults the budget (unit E wires the
        // real `divert_budget()` in for the blocking half at this same site).
        // Sticky-before-Block decision (documented in this unit's FINAL-REPORT):
        // this call site always offers Sticky first — `divert_verdict` itself
        // only returns `Block` once a caller passes a nonzero budget, which
        // this phase never does, so the "try Sticky before honouring Block"
        // question is moot AT budget=0, but the shape here is the one E's
        // budget wiring extends without a second rewrite: E swaps the `0`
        // for `self.divert_budget()` and adds a branch on `DivertVerdict::Block`
        // immediately after this match — sticky is already tried first because
        // it's a separate, unconditional arm of the same predicate.
        let sticky_pick: Option<usize> = keep_pin.and_then(|pin_idx| {
            let episode = affinity.and_then(|key| {
                self.divert_ledger
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&key)
                    .copied()
            });
            match divert_verdict(episode, pin_idx, pin_until_ms, 0) {
                DivertVerdict::Sticky(sticky) if !tried.contains(&sticky) => Some(sticky),
                _ => None,
            }
        });

        // (B) FALLBACK — least-utilized surviving account, throttled, then re-pinned.
        let via_sticky;
        let idx = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);

            let sticky_usable =
                sticky_pick.filter(|&s| accounts.get(s).is_some_and(|a| servable(s, a)));
            via_sticky = sticky_usable.is_some();

            let idx = if let Some(sticky) = sticky_usable {
                sticky
            } else {
                let mut best: Option<usize> = None;
                // (util as ordered bits, last_selected_seq) — utilizations are finite and
                // non-negative here (NaN/inf headers filtered at parse), so the raw bit
                // pattern orders correctly for ascending "least utilized first".
                let mut best_key: Option<(u64, u64)> = None;
                for (i, account) in accounts.iter().enumerate() {
                    if !servable(i, account) {
                        continue;
                    }
                    let util = account.quota.max_utilization(now, is_fable);
                    let key = (util.to_bits(), account.last_selected_seq);
                    if best_key.is_none_or(|b| key < b) {
                        best = Some(i);
                        best_key = Some(key);
                    }
                }
                best?
            };

            // A servable candidate exists — spend the anti-storm window now (after
            // selection, so a fleet with nothing to serve never burns the valve).
            let valve = self.revalidation_valve(provider);
            let next_at = valve.load(Ordering::Relaxed);
            if now_ms < next_at {
                return None;
            }
            valve.store(now_ms + REVALIDATION_MIN_SPACING_MS, Ordering::Relaxed);

            let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
            let util = accounts
                .get(idx)
                .map_or_default(|a| a.quota.max_utilization(now, is_fable));
            if let Some(account) = accounts.get_mut(idx) {
                account.last_selected_seq = tick;
                if via_sticky {
                    tracing::info!(
                        account = %account.name,
                        utilization = util,
                        is_fable,
                        "revalidation-serve (fallback, sticky): reusing this episode's already-warmed destination"
                    );
                } else {
                    tracing::info!(
                        account = %account.name,
                        utilization = util,
                        is_fable,
                        "revalidation-serve (fallback): whole fleet over soft threshold — serving least-utilized allowed account"
                    );
                }
            }
            idx
        };

        // Re-pin the session (accounts lock already dropped — never nest the two).
        // Mirrors `select()`'s re-pin, `keep_pin` included: a model-class divert
        // re-writes the OLD index with a fresh `now_ms`, so the session comes home on
        // its next request of another class. No size-cap eviction here because a
        // revalidation serve only ever touches an EXISTING session's pin.
        if let Some(key) = affinity {
            let pin_idx = keep_pin.unwrap_or(idx);
            let previous = {
                let mut pins = self.affinity.lock().unwrap_or_else(PoisonError::into_inner);
                let previous = pins.get(&key).map(|&(i, _)| i);
                pins.insert(key, (pin_idx, now_ms));
                self.mark_affinity_dirty();
                previous
            };
            // Record this fallback serve in the episode ledger (§4.2 rule 3: written
            // only after the affinity guard above has dropped, its own critical
            // section, never nested under it or the accounts lock below). Only when
            // `keep_pin` was set — i.e. the pin stayed and this is genuinely a
            // divert, not a hard re-key onto a fresh home with no live hold to key
            // an episode on. Mirrors `select()`'s own record-divert guard.
            if let Some(pin_idx) = keep_pin {
                self.record_divert(key, pin_idx, pin_until_ms, idx, now_ms);
            }
            // Until now this arm emitted nothing, so a request served off-pin by the
            // fallback was unattributable: the logs showed a serve on an account the
            // session was not pinned to and no line explaining it. One greppable line
            // carrying the same `reason=` field as `select()`'s divert log, so all
            // four reasons grep together. It says KEPT or RE-KEYED rather than
            // assuming a divert, because this arm does both: `keep_pin` set means the
            // pin stayed and another account served; `keep_pin` unset means the
            // fallback account became the new pin. The accounts lock is taken only
            // after the affinity lock has dropped above — never simultaneously.
            let (pin_name, serve_name) = self.pin_and_serve_names(pin_idx, idx);
            tracing::info!(
                "affinity: revalidation session {} pin {} -> serving {} (reason=revalidation-fallback, pin {})",
                short_session_id(key),
                pin_name,
                serve_name,
                if previous.is_some_and(|p| p != pin_idx) {
                    "re-keyed"
                } else {
                    "kept"
                }
            );
        }
        Some(idx)
    }

    /// Serve the session's pinned account `idx` even though it is over the soft threshold: bump
    /// its LRU tick and log the `revalidation-serve (pin-honor)` line. The caller holds the
    /// accounts write lock.
    pub(super) fn serve_pinned_over_threshold(
        &self,
        accounts: &mut [AccountRuntime],
        idx: usize,
        now: OffsetDateTime,
        is_fable: bool,
    ) {
        let tick = self.select_seq.fetch_add(1, Ordering::Relaxed);
        let util = accounts
            .get(idx)
            .map_or_default(|a| a.quota.max_utilization(now, is_fable));
        if let Some(account) = accounts.get_mut(idx) {
            account.last_selected_seq = tick;
            tracing::info!(
                account = %account.name,
                utilization = util,
                is_fable,
                "revalidation-serve (pin-honor): serving session's pinned account over soft threshold to keep its cache warm"
            );
        }
    }

    /// The names of the pinned and the serving account for an `affinity:` log line, `?` for an
    /// index that no longer exists. Takes the accounts read lock itself and drops it before
    /// returning, so the caller must not hold the affinity lock.
    pub(super) fn pin_and_serve_names(&self, pin_idx: usize, idx: usize) -> (String, String) {
        let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
        let name = |i: usize| accounts.get(i).map_or("?", |a| a.name.as_str()).to_owned();
        (name(pin_idx), name(idx))
    }
}
