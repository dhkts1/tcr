//! The bound-history verdicts: whether the account a conversation's history is bound to can still
//! hold it, and serving a held request there. The binding itself (the token map and its file)
//! is `sessions::bound_tokens`, whose module doc says why a hold is worth a short wait and never
//! one that outlasts the cache.

use super::*;

impl Manager {
    /// Whether the account at `idx` can still hold a conversation whose history is bound to it:
    /// the one question that decides between holding the request there and releasing it to the
    /// fleet.
    ///
    /// Yes while the account is alive for every model class ([`Self::account_hard_ok`], which
    /// passes a hold that clears while the cache is warm) and for THIS request's class
    /// ([`Self::model_blocked`]). A short hold is waited out, or answered with that hold's own
    /// `retry-after`. The soft quota threshold is not consulted: an account over it still serves
    /// until the upstream says otherwise.
    ///
    /// No once the account is terminally gated (`disabled`, a dead login, a `rejected` unified
    /// status), parked, reserved away from this request, held for longer than the cache lives,
    /// or out of this model class's weekly bucket. None of those clears soon, and the client
    /// recovers from the one rejection a sibling gives (see the `sessions::bound_tokens` module
    /// doc). An index that names no account is a no too: there is nothing to hold the request to.
    pub fn bound_account_holds(
        &self,
        idx: usize,
        now: OffsetDateTime,
        is_fable: bool,
        group: Option<&str>,
        warm: WarmWindow,
    ) -> bool {
        let now_ms = odt_to_ms(now);
        let reserved = self.reserved_groups();
        let parked = self.parked_groups();
        let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
        accounts.get(idx).is_some_and(|account| {
            Self::account_hard_ok(account, now_ms, group, &reserved, &parked, warm)
                // "Holds" means this account answers the next request itself and every
                // other account is benched for it, so a rejection that merely clears
                // while the cache is warm (which `account_hard_ok` lets through to keep a
                // PIN) must still answer no here: holding a conversation on a rejected
                // account is the 429-every-turn incident the `sessions::bound_tokens` module
                // doc describes.
                && Self::account_terminal_gate(account).is_none()
                && !Self::model_blocked(
                    account,
                    self.global_threshold,
                    self.fable_weekly_threshold,
                    now,
                    is_fable,
                )
        })
    }

    /// Serve a held request on the account its history is bound to, over the soft threshold if
    /// need be: `Some(idx)` when that account can answer this request now.
    ///
    /// [`Self::bound_account_holds`] keeps a conversation on its account without consulting the
    /// soft threshold, so the serve side has to agree with it. It did not: a held request on an
    /// account over the threshold fell through to [`Self::select_revalidation`], whose fallback
    /// lets one request through per 2 s across the whole fleet, and the turn that lost that race
    /// got an exhausted 429 naming the account's 5-hour window. Claude Code renders that as
    /// "You've hit your session limit" (2026-09-24: 3 of 3 such 429s came 0.05-1.4 s after
    /// another turn on the same account, while four other accounts sat at 0%).
    ///
    /// Served the way `select_revalidation` serves a session's pin: the hard gates for THIS
    /// request ([`Self::hard_ok`] with the request's own group, so a live hold of any length
    /// still refuses), no revalidation window, and the session's pin left alone. `None` when the
    /// account already failed this request (`tried`), cannot serve it now, or sits outside the
    /// group this request asked for strictly: a strict group never serves from outside itself,
    /// held history included.
    #[expect(
        clippy::too_many_arguments,
        reason = "the per-request selection inputs `select_with_group` also takes"
    )]
    pub fn select_bound(
        &self,
        idx: usize,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        is_fable: bool,
        group: Option<&str>,
        strict_group: Option<&str>,
        warm: WarmWindow,
    ) -> Option<usize> {
        if tried.contains(&idx) {
            return None;
        }
        let provider = self
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(idx)?
            .provider;
        let unsupported_exit = match provider {
            crate::base::provider::Provider::Anthropic => false,
            crate::base::provider::Provider::Codex => {
                self.codex_accounts_with_unsupported_exit().contains(&idx)
            }
        };
        if unsupported_exit {
            return None;
        }
        let now_ms = odt_to_ms(now);
        let reserved = self.reserved_groups();
        let parked = self.parked_groups();
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let account = accounts.get_mut(idx)?;
        let outside_strict_group =
            strict_group.is_some_and(|g| !account.groups.iter().any(|carried| carried == g));
        if outside_strict_group
            || !Self::hard_ok(
                account,
                self.global_threshold,
                self.fable_weekly_threshold,
                now,
                now_ms,
                is_fable,
                group,
                &reserved,
                &parked,
                warm,
            )
        {
            return None;
        }
        let utilization = account.quota.max_utilization(now, is_fable);
        account.last_selected_seq = self.select_seq.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            account = %account.name,
            utilization,
            is_fable,
            "revalidation-serve (bound-honor): serving the account this conversation's history is bound to"
        );
        Some(idx)
    }
}
