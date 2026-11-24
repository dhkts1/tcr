//! The selector's entry points: `select`, the read-only `routing_candidates`, and the group spill and strict answers.

use super::*;

impl Manager {
    /// Pick the best eligible account not in `tried`, spreading load across the
    /// fleet, or `None` if all are exhausted/held/disabled.
    ///
    /// Within a priority tier we pick from the soonest **reset-urgency bucket**
    /// first ([`Self::reset_urgency_tier`] — the governing weekly reset floored
    /// into `resetUrgencyTierHours`-wide buckets, default 24h), because unused
    /// weekly quota is worth nothing once its window resets. Within a bucket we
    /// pick the **least-recently-selected** account (lowest `last_selected_seq`;
    /// a never-selected account sorts first) so consecutive requests fan out
    /// instead of hammering one account. Ordering by quota headroom was rejected
    /// deliberately: a single request barely moves a weekly bar, so "most
    /// headroom first" would deterministically pin one account until its bar
    /// caught up — the exact overload this fixes. Ranking on the RAW reset
    /// instant would pin it the same way, which is why the urgency term buckets
    /// instead of comparing directly. The winner is stamped with the next
    /// monotonic tick *before returning*, so even a burst of concurrent selects
    /// rotates within its bucket (each sees the previous stamp). The soonest
    /// weekly reset is the final cold-start tiebreak (all-unseen startup).
    ///
    /// This mutates rotation state (the stamp), so it takes the write lock.
    ///
    /// `model` is the request's target model (if known). When it names a Fable
    /// model, an account whose model-scoped weekly (`7d_oi`) bucket is exhausted
    /// is skipped — while that same account still serves every non-Fable model.
    ///
    /// `affinity` is the caller's session key (opt-in; `None` when the feature is
    /// off). With `None` this is byte-for-byte the pre-affinity behaviour. With
    /// `Some(key)`: if the session is already pinned to an account that is not in
    /// `tried` and still passes [`Self::eligible`], that pinned account is
    /// returned — still stamped with a fresh select tick so *other* sessions' LRU
    /// steers away from a busy pinned account. Otherwise a normal LRU/priority
    /// pick runs. Affinity never overrides priority — the pin is always a normal
    /// pick.
    ///
    /// **The invariant, and the whole point of a pin: a session's pin is re-keyed
    /// ONLY when the pinned ACCOUNT fails a HARD gate** ([`Self::account_hard_ok`] is
    /// the sole authority). Anthropic's prompt cache is per-account, so a re-key
    /// re-creates the entire conversation prefix on a cold account. Only disabled /
    /// [`AccountStatus::Error`] / `rejected` / a hold that OUTLIVES the prompt cache
    /// ([`CACHE_WARM_HOLD_SECS`]) re-key durably. This is self-healing: a durable
    /// failure arms a long hold or sets `rejected` on the very next attempt, and
    /// those fail `account_hard_ok`.
    ///
    /// The one exception is a pin with nothing left to protect: a session idle past its warm
    /// window (`WarmWindow`) has a cold cache on every account, so its pin is not honoured
    /// and the request takes the normal pick, which then re-pins wherever it lands. A cold
    /// pin that stood would only override the pick's order (priority, then the weekly-reset
    /// urgency bucket) for no gain.
    ///
    /// Everything else is a fact about ONE REQUEST, not about the account. Such a
    /// fact never re-keys, and — the second half of the same argument — mostly does
    /// not even DIVERT, because a divert costs that request the same cold prefix a
    /// re-key would. The five split by how much they actually know:
    ///  - **over the utilization threshold** → **SERVE the pin anyway, UNLESS the
    ///    session has also gone idle-cold** (no request in [`CACHE_WARM_HOLD_SECS`])
    ///    AND an eligible alternative account exists — then there is nothing left to
    ///    protect, so the pin durably re-keys instead (measured 2026-09-11: an
    ///    8-of-18-account morning burst left ten accounts idle for six hours while a
    ///    session that had gone cold anyway kept its pin on a saturated one; falling
    ///    through requires the same eligible-alternative check the normal pool pick
    ///    makes, so a fleet-wide soft threshold still gets a last-resort serve rather
    ///    than a spurious 429). Otherwise: that threshold is our own arithmetic over
    ///    headers that go stale by minutes, and Anthropic keeps answering 200s for
    ///    accounts it benches. Upstream is the oracle: serve, and let a real 429 arm a
    ///    real (HARD) hold. No config flag governs the idle-cold carve-out — it fires
    ///    only when the cache is already cold by the crate's own constant AND the
    ///    account is over the soft threshold, so there is nothing to opt out of.
    ///  - **paced out** (at the in-flight cap / inside min-spacing, see
    ///    [`Self::paced_out`]) → divert this ONE request, keep the pin. Our own
    ///    concurrency is measured exactly and never stale, and spreading a session's
    ///    burst is precisely what the cap exists to do.
    ///  - **model-class blocked** (a Fable request whose pin has an exhausted Fable
    ///    weekly, see [`Self::model_blocked`]) → divert this ONE request, keep the
    ///    pin. The account cannot answer THIS request and answers every other model
    ///    class perfectly, so treating it as death let a one-line title call drag a
    ///    200k-token Opus conversation onto a cold account.
    ///  - **already in `tried`** → divert this ONE request, keep the pin. It has
    ///    failed THIS request upstream, so re-serving it would spin — but one blip is
    ///    not proof the account is gone.
    ///  - **held by a rate-limit timer that clears while the cache is still warm**
    ///    (see [`Self::hold_clears_while_warm`]) → divert this ONE request, keep the
    ///    pin. A hold is a timer, not a death, and the ones we arm are mostly short:
    ///    a no-guidance park is 15s + jitter, and a `retry-after` park is clamped to
    ///    300s. Re-keying on one would discard a prefix that is still warm when the
    ///    account frees. Past [`CACHE_WARM_HOLD_SECS`] the trade inverts and the hold
    ///    goes back to being an ACCOUNT-level hard gate.
    ///
    /// Every divert routes to the fall-through pick while the pin stays put (see
    /// `keep_pin`); the utilization case returns from the fast-path.
    ///
    /// The one remaining voluntary mover — the load-balancing migration that
    /// re-pins a stacked session onto a less-loaded account — is therefore
    /// OPT-IN and ships OFF (top-level `loadBalanceMigration`, see
    /// [`Self::load_balance_migration_enabled`]). Disabled, the scan is skipped
    /// entirely and the pin is honoured; set to `true`, the balancing behaviour
    /// below is unchanged.
    ///
    /// **Control-account routing (part 2), applied ONLY when this call reaches
    /// this point with no existing pin honoured above** (`keep_pin.is_none()` —
    /// invariant: a pin, even a per-request-diverted one, is never touched by
    /// this overlay): `path` is classified by [`classify_request`] into the
    /// three-way split — `Inference` is pool-only and excluded from the control
    /// account even in the normal pick below; `Noise` prefers whichever account
    /// `conn_key` ([`crate::proxy::SessionKey`]'s per-connection value, distinct
    /// from `affinity`) already served on this connection
    /// ([`Self::conn_affinity`]); everything else prefers the control account
    /// via [`Self::control_eligible`] (which deliberately bypasses `disabled`).
    /// Inert whenever [`Self::control`] is `None`.
    ///
    /// Thin wrapper over [`Self::select_with_group`] with no group preference —
    /// kept as the stable signature the test suite (and every caller that has no
    /// `--group` to express) already calls by the hundred, so a per-request
    /// preference this narrow does not force every one of them to grow a new
    /// trailing `None`.
    pub fn select(
        &self,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        model: Option<&str>,
        affinity: Option<u64>,
        path: &str,
        conn_key: Option<u64>,
    ) -> Option<usize> {
        self.select_with_group(tried, now, model, affinity, path, conn_key, None)
    }

    /// Which accounts could serve a NEW, UNPINNED request for `model` on `path` now, asking
    /// for no group: the first pass, in the selector's own order, with any candidate, and
    /// every candidate it has. The candidates are [`Self::pool_candidate`]'s, the filter the
    /// picks themselves run, so routing and the picks cannot disagree about a gate.
    ///
    /// The passes, as a selection meets them: the provider's hard account lock, which serves
    /// alone when set; [`PoolPass::Paced`], [`PoolPass::LeastLoaded`], and
    /// [`PoolPass::FableLastResort`] for a Fable request; then [`PoolPass::Revalidation`],
    /// which the proxy reaches when those found nothing and `revalidationServe` is on. Its
    /// two-second valve is read without spending it, just as an unpinned serve requires.
    ///
    /// Read-only, unlike every selection: it stamps no account, pins and unpins no session
    /// and spends no revalidation window, so reading it never changes what the next
    /// selection does. The one write it shares with them is the group-settings reload, which
    /// only follows the config file.
    pub fn routing_candidates(
        &self,
        now: OffsetDateTime,
        model: Option<&str>,
        path: &str,
    ) -> RoutingCandidates {
        self.reload_groups_if_changed();
        let groups = self.groups();
        let provider = crate::provider::Provider::of_path(path);
        let is_fable =
            provider.has_fable_bucket() && model.is_some_and(crate::model::is_fable_model);
        let unsupported = self.unsupported_exits(provider);
        // Each read under a lock of its own, so before the accounts lock and never nested in
        // it.
        let locked = self.locked_account_for(provider);
        let revalidation = self.revalidation_serve_enabled();
        let warm = self.warm_window(None);
        let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
        let control_excluded =
            self.control_excluded(classify_request(path), None, &accounts, &groups);
        let pool_tried = exclude_control(&unsupported, control_excluded);
        // Selection spends the valve under the accounts write lock. Read it under this
        // read lock so every pass below sees the same provider-local deadline.
        let revalidation_at_ms = self.revalidation_valve(provider).load(Ordering::Relaxed);
        let candidates_at = |at| {
            let ask = |tried| PoolAsk {
                tried,
                now: at,
                now_ms: odt_to_ms(at),
                is_fable,
                provider,
                group: None,
                groups: &groups,
                locked,
                warm,
            };
            let pool = ask(&pool_tried);
            // The lock and revalidation do not see the control exclusion, matching
            // their actual selection paths.
            let whole = ask(&unsupported);
            let passes: Vec<(PoolPass, &PoolAsk<'_>)> = if locked.is_some() {
                vec![(PoolPass::Lock, &whole)]
            } else {
                [
                    Some((PoolPass::Paced, &pool)),
                    Some((PoolPass::LeastLoaded, &pool)),
                    is_fable.then_some((PoolPass::FableLastResort, &pool)),
                    (revalidation && odt_to_ms(at) >= revalidation_at_ms)
                        .then_some((PoolPass::Revalidation, &whole)),
                ]
                .into_iter()
                .flatten()
                .collect()
            };
            passes.into_iter().find_map(|(pass, ask)| {
                let found: Vec<usize> = accounts
                    .iter()
                    .enumerate()
                    .filter(|&(idx, account)| self.pool_candidate(pass, idx, account, ask))
                    .map(|(idx, _)| idx)
                    .collect();
                (!found.is_empty()).then_some((pass, found))
            })
        };
        if let Some((pass, accounts)) = candidates_at(now) {
            return RoutingCandidates {
                pass: Some(pass),
                accounts,
                is_fable,
                free_at: None,
                gated: BTreeMap::new(),
            };
        }

        // With no upstream update, these are the only instants at which an unpaced
        // pass can change. Ask the executable filters at each one, rather than
        // duplicating their hard/soft gates or timing against a provider-wide hint.
        // The first successful instant is min(account max(binding clear times));
        // a manual or unknown gate never yields a promise unless a pass can serve.
        let mut changes = Vec::new();
        changes.extend(ms_to_odt(revalidation_at_ms));
        let mut gated = BTreeMap::new();
        for (idx, account) in accounts.iter().enumerate() {
            if account.provider != provider
                || unsupported.contains(&idx)
                || (!revalidation && control_excluded == Some(idx))
            {
                continue;
            }
            changes.extend(account.rate_limited_until_ms.and_then(ms_to_odt));
            changes.extend(
                [
                    account.quota.five_hour,
                    account.quota.seven_day,
                    account.quota.seven_day_oi,
                ]
                .into_iter()
                .flatten()
                .filter_map(|window| window.live_reset(now)),
            );
            changes.extend(account.quota.standard_reset);
            let threshold = account.switch_threshold.unwrap_or(self.global_threshold);
            let (reason, _) = Self::account_gate(
                account,
                threshold,
                self.fable_weekly_threshold,
                now,
                odt_to_ms(now),
                is_fable,
                None,
                &groups.reserved,
                &groups.parked,
            );
            if reason != GateReason::Ok {
                *gated.entry(reason).or_insert(0) += 1;
            }
        }
        changes.sort_unstable();
        changes.dedup();
        let free_at = changes
            .into_iter()
            .filter(|&at| at > now)
            .find(|&at| candidates_at(at).is_some());
        RoutingCandidates {
            pass: None,
            accounts: Vec::new(),
            is_fable,
            free_at,
            gated,
        }
    }

    /// Whether `group` has opted in to `spillToPool`, a PREFERENCE, whose own
    /// traffic may be served by a non-member when no member has capacity.
    ///
    /// A membership test rather than [`Self::spill_groups`] + `contains`, for
    /// the reason [`Self::is_group_reserved`] gives: the request path asks it
    /// per request and only ever needs the one answer. Reads the same
    /// hot-reloaded cache, so a `groupSettings` edit takes effect on the next
    /// request.
    pub fn is_group_spill(&self, group: &str) -> bool {
        self.groups().spill.contains(group)
    }

    /// **Whether a request asking for `group` is held STRICTLY inside it**,
    /// the one answer, for every reader.
    ///
    /// `reserved || !spill` ([`GroupPolicy::is_strict`]), the same call
    /// [`Self::select_with_group`] makes for its own `strict_group`. A
    /// measurement found two readers outside the picker asking
    /// [`Self::is_group_reserved`] ALONE, `src/proxy.rs`'s refusal arm and its
    /// mirror on the exhausted path, so for a group that is neither reserved
    /// nor a spill group the picker held the request strictly while those two
    /// scoped their answer to the whole fleet: the wait they sized, the
    /// revalidation they attempted and the 429 they finally returned were all
    /// about accounts the request was never allowed to touch. A lease's scope is
    /// exactly such a group, so a borrowed request's refusal named the lender's
    /// entire fleet.
    ///
    /// One function rather than the expression in three places: a strictness
    /// rule the picker and the refusal disagree about is a request held in a
    /// group and refused about a pool.
    pub fn is_group_strict(&self, group: &str) -> bool {
        self.groups().is_strict(group)
    }
}
