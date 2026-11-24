//! Codex slot filters: strict peer exits this build cannot honour, and the Codex candidate counts.

use super::*;

impl Manager {
    /// Unsupported strict Codex exits at their exact runtime slots. The manager's
    /// cached config and runtime vectors are append-only and stay index-aligned;
    /// deleting a file row marks its runtime slot removed rather than shifting it.
    pub(crate) fn codex_accounts_with_unsupported_exit(&self) -> HashSet<usize> {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .unsupported_strict_exit_slots()
    }

    /// The slots a request of `provider` may never be served from: strict Codex peer exits
    /// this build cannot honour. Empty for `anthropic`.
    pub(super) fn unsupported_exits(&self, provider: crate::provider::Provider) -> HashSet<usize> {
        match provider {
            crate::provider::Provider::Anthropic => HashSet::new(),
            crate::provider::Provider::Codex => self.codex_accounts_with_unsupported_exit(),
        }
    }

    pub(super) fn exclude_unsupported_codex_exits<'a>(
        &self,
        tried: &'a HashSet<usize>,
        provider: crate::provider::Provider,
    ) -> std::borrow::Cow<'a, HashSet<usize>> {
        let unsupported = self.unsupported_exits(provider);
        if unsupported.is_empty() {
            return std::borrow::Cow::Borrowed(tried);
        }
        self.affinity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, (idx, _)| !unsupported.contains(idx));
        let mut excluded = tried.clone();
        excluded.extend(unsupported);
        std::borrow::Cow::Owned(excluded)
    }

    /// Whether waiting could leave a Codex account usable. Operator-disabled, parked,
    /// removed, reserved, dead-login and unsupported strict peer-exit rows need a
    /// configuration change; quota/refusal holds are timers and still count here.
    /// A bound conversation or hard account lock cannot recover on another row.
    pub(crate) fn codex_account_can_serve_eventually(&self, bound_to: Option<usize>) -> bool {
        self.codex_candidate_counts(bound_to, None, |_| false).0 > 0
    }

    /// Whether every otherwise usable Codex account has a live access-refusal hold.
    /// Inspect the hold directly: a later quota window can hide it in the reported gate.
    pub(crate) fn codex_pool_is_refused(
        &self,
        now: OffsetDateTime,
        bound_to: Option<usize>,
        group: Option<&str>,
    ) -> bool {
        let now_ms = odt_to_ms(now);
        let (candidates, refused) = self.codex_candidate_counts(bound_to, group, |account| {
            account.hold_cause == HoldCause::Refused
                && account
                    .rate_limited_until_ms
                    .is_some_and(|until| until > now_ms)
        });
        candidates > 0 && candidates == refused
    }

    /// Count candidate slots and the subset satisfying `matches`, under one accounts read.
    /// Config/group snapshots are released before that read, so no request-side guard
    /// waits on config persistence or nests config and accounts locks.
    fn codex_candidate_counts(
        &self,
        bound_to: Option<usize>,
        group: Option<&str>,
        matches: impl Fn(&AccountRuntime) -> bool,
    ) -> (usize, usize) {
        self.fold_codex_candidates(bound_to, group, (0, 0), |(total, matching), _, account| {
            (total + 1, matching + usize::from(matches(account)))
        })
    }

    /// The exhaustion deadline must fold the same slots that can recover by
    /// waiting. Disabled or unsupported siblings must not hide a real reset.
    pub(in crate::manager) fn codex_candidate_indices(
        &self,
        bound_to: Option<usize>,
        group: Option<&str>,
    ) -> HashSet<usize> {
        self.fold_codex_candidates(bound_to, group, HashSet::new(), |mut indices, idx, _| {
            indices.insert(idx);
            indices
        })
    }

    fn fold_codex_candidates<T>(
        &self,
        bound_to: Option<usize>,
        group: Option<&str>,
        initial: T,
        mut fold: impl FnMut(T, usize, &AccountRuntime) -> T,
    ) -> T {
        self.reload_groups_if_changed();
        let unsupported = self.codex_accounts_with_unsupported_exit();
        let groups = self.groups();
        let locked = self.locked_account_for(crate::provider::Provider::Codex);
        self.accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .enumerate()
            .filter(|(idx, account)| match account.provider {
                crate::provider::Provider::Anthropic => false,
                crate::provider::Provider::Codex => {
                    !account.removed
                        && bound_to.is_none_or(|bound| bound == *idx)
                        && locked.is_none_or(|locked_idx| locked_idx == *idx)
                        && group.is_none_or(|group| account.groups.iter().any(|g| g == group))
                        && !account.disabled
                        && account.status != AccountStatus::Error
                        && !Self::parked_blocks(account, &groups.parked)
                        && !Self::reserved_blocks(account, group, &groups.reserved)
                        && !unsupported.contains(idx)
                }
            })
            .fold(initial, |value, (idx, account)| fold(value, idx, account))
    }
}
