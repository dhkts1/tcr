//! The roster verbs keyed by identity: find the one row an identity probe
//! ([`crate::base::identity::probe`]) names, through [`crate::base::identity::resolve`],
//! and work on it. The name-keyed lookups (a user's query) are in `roster.rs`.

use std::collections::HashSet;

use serde_json::Value;

use super::edit::CODEX_SYNC_PENDING_KEY;
use super::{Account, Config, ControlAccountRef, EgressPin};
use crate::base::identity::Resolved;

impl Config {
    /// The one row of [`Self::accounts`] that `probe` names, by
    /// [`crate::base::identity::resolve`]: a tie it cannot break is
    /// [`Resolved::Many`], and every caller refuses on it rather than guess.
    fn resolve_identity(&self, probe: &Account) -> Resolved {
        crate::base::identity::resolve(self.accounts.iter().enumerate(), probe)
    }

    /// The one row `probe` names ([`Self::resolve_identity`]); `None` when no
    /// row does, and when two tie, which every caller refuses rather than guess.
    pub fn identity_row(&self, probe: &Account) -> Option<&Account> {
        match self.resolve_identity(probe) {
            Resolved::One(index) => self.accounts.get(index),
            Resolved::None | Resolved::Many => None,
        }
    }

    /// Apply `update` to the one row `probe` names ([`Self::resolve_identity`]),
    /// and say how it resolved: on [`Resolved::None`] and [`Resolved::Many`]
    /// nothing is touched, and the caller decides.
    pub fn update_identity_row(
        &mut self,
        probe: &Account,
        update: impl FnOnce(&mut Account),
    ) -> Resolved {
        let resolved = self.resolve_identity(probe);
        if let Resolved::One(index) = resolved
            && let Some(account) = self.accounts.get_mut(index)
        {
            update(account);
        }
        resolved
    }

    /// Bench or un-bench the one row `target` resolves to
    /// ([`Self::resolve_identity`]): `disabled: true` when `disabled`, the key
    /// dropped otherwise. An unbreakable tie writes nothing, as
    /// [`super::save_disabled`] refuses one too, so memory never drifts onto a
    /// record the file was not allowed to touch.
    pub fn set_disabled_by_identity(&mut self, target: &Account, disabled: bool) {
        self.update_identity_row(target, |account| {
            account.disabled = if disabled { Some(true) } else { None };
        });
    }

    /// Stamp a rotated credential onto the one row `probe` resolves to, and say
    /// where it went. Resolved, never first-match: `same_identity` treats an
    /// unknown org as a match, so with the legacy two-org shape in the file a
    /// first-match search lands THIS account's rotated credential on the OTHER
    /// account's record, overwriting a single-use refresh token that is then
    /// dead. A tie writes nothing.
    ///
    /// A Codex row also carries `sync_pending` as its publication marker
    /// ([`CODEX_SYNC_PENDING_KEY`]), or loses the marker when it is `None`; an
    /// Anthropic row's `extra` is left as it is.
    pub fn stamp_rotated_tokens(
        &mut self,
        probe: &Account,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_at_ms: i64,
        sync_pending: Option<Value>,
    ) -> Resolved {
        self.update_identity_row(probe, |account| {
            access_token.clone_into(&mut account.access_token);
            // THE TRAP: same invariant as `apply_refresh` in
            // `manager/freshness/refresh.rs` — `refresh_token` must be
            // `Some(...)` on every reachable refresh success.
            account.refresh_token = refresh_token.map(str::to_owned);
            account.expires_at = Some(expires_at_ms);
            match probe.provider {
                crate::base::provider::Provider::Anthropic => {}
                crate::base::provider::Provider::Codex => match sync_pending {
                    Some(value) => {
                        account
                            .extra
                            .insert(CODEX_SYNC_PENDING_KEY.to_string(), value);
                    }
                    None => {
                        account.extra.remove(CODEX_SYNC_PENDING_KEY);
                    }
                },
            }
        })
    }

    /// Set or clear the Codex publication marker ([`CODEX_SYNC_PENDING_KEY`])
    /// on the one row `target` resolves to, only while that row still holds
    /// `refresh`: a row whose token rotated since is not the record this marker
    /// describes.
    pub fn mark_codex_sync_pending(
        &mut self,
        target: &Account,
        refresh: &str,
        pending: Option<&Value>,
    ) {
        self.update_identity_row(target, |account| {
            if account.refresh_token.as_deref() != Some(refresh) {
                return;
            }
            match pending {
                Some(record) => {
                    account
                        .extra
                        .insert(CODEX_SYNC_PENDING_KEY.to_string(), record.clone());
                }
                None => {
                    account.extra.remove(CODEX_SYNC_PENDING_KEY);
                }
            }
        });
    }

    /// The exit lock of the one row `target` resolves to, or no lock when no
    /// row or more than one does.
    pub fn egress_pin_of(&self, target: &Account) -> EgressPin {
        self.identity_row(target)
            .map_or_default(Account::egress_pin)
    }

    /// Carry replaced credentials (`for_disk`, already written to the file) onto
    /// the in-memory row `target` resolves to: tokens, expiry, Codex folder, and
    /// the Codex publication marker dropped.
    pub fn adopt_replaced_credentials(&mut self, target: &Account, for_disk: &Account) {
        let resolved = self.update_identity_row(target, |entry| {
            entry.access_token.clone_from(&for_disk.access_token);
            entry.refresh_token.clone_from(&for_disk.refresh_token);
            entry.expires_at = for_disk.expires_at;
            entry.codex_home.clone_from(&for_disk.codex_home);
            entry.extra.remove(CODEX_SYNC_PENDING_KEY);
        });
        match resolved {
            Resolved::One(_) => {}
            // No in-memory entry carries this identity either — the disk
            // row `save_account` just appended has no counterpart here.
            // Mirror the append so a later persist can still find it.
            Resolved::None => self.accounts.push(for_disk.clone()),
            // A tie the in-memory snapshot cannot break either; leave it
            // alone rather than guess which entry to overwrite.
            Resolved::Many => {}
        }
    }

    /// Append `account` as the last row. Rows are appended, never removed or
    /// reordered, which is what keeps them index-aligned with the manager's
    /// runtime rows.
    pub fn append_account(&mut self, account: Account) {
        self.accounts.push(account);
    }

    /// [`Self::append_account`], behind the whole fleet: at one past the
    /// highest priority any row carries, or 0 when none carries one.
    pub fn append_at_next_priority(&mut self, account: Account) {
        let next_priority = self
            .accounts
            .iter()
            .filter_map(|a| a.priority)
            .max()
            .map_or(0, |max| max + 1);
        self.append_account(Account {
            priority: Some(next_priority),
            ..account
        });
    }

    /// One value per row, in row order, each built from the row and this
    /// config's `http1_only`: how the manager derives its index-aligned runtime
    /// rows.
    pub fn build_per_account<T>(&self, mut build: impl FnMut(&Account, bool) -> T) -> Vec<T> {
        self.accounts
            .iter()
            .map(|a| build(a, self.http1_only))
            .collect()
    }

    /// Every row [`crate::base::identity::same_identity`] calls the same account as
    /// `probe`, in config order: the candidates a [`Resolved::Many`] could not
    /// choose between. Not [`Self::resolve_identity`]: the group reload takes
    /// the first of them to match its running rows.
    pub fn same_identity_rows<'a>(&'a self, probe: &Account) -> impl Iterator<Item = &'a Account> {
        self.accounts
            .iter()
            .filter(|a| crate::base::identity::same_identity(probe, a))
    }

    /// Append every row of `fresh` for which `admit` returns `true`, after
    /// `admit` has seen it; `admit` decides whether the row is new and does the
    /// caller's half of admitting it.
    pub fn admit_rows_from(&mut self, fresh: &Self, mut admit: impl FnMut(&Account) -> bool) {
        for candidate in &fresh.accounts {
            if admit(candidate) {
                self.accounts.push(candidate.clone());
            }
        }
    }

    /// Each row's exit lock, in row order ([`Account::egress_pin`]).
    pub fn egress_pins(&self) -> Vec<EgressPin> {
        self.accounts.iter().map(Account::egress_pin).collect()
    }

    /// Row `idx`'s exit lock, `None` for an index past the last row.
    pub fn egress_pin_at(&self, idx: usize) -> Option<EgressPin> {
        self.accounts.get(idx).map(Account::egress_pin)
    }

    /// The indices of rows whose strict peer exit this build cannot honour
    /// ([`Account::has_unsupported_strict_peer_exit`]).
    pub fn unsupported_strict_exit_slots(&self) -> HashSet<usize> {
        self.accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| account.has_unsupported_strict_peer_exit())
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Name `target` as the control account, or none.
    pub fn name_control_account(&mut self, target: Option<&Account>) {
        self.control_account = target.map(ControlAccountRef::from);
    }
}
