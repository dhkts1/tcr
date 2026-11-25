//! Resolving a user's account query to one config row, and the message for a query that matches more than one.

use std::collections::BTreeMap;

use anyhow::bail;

use super::{Account, Config};
use crate::identity;

/// Resolve a user-supplied account `query` to exactly one index in `accounts`.
///
/// Delegates to [`identity::match_accounts`]: an EXACT match on the account's
/// name, and nothing else. Names are unique across a config by construction
/// (`config::load` renames duplicates; a login mints a free name), so an exact
/// name is a complete answer and there is nothing left to narrow. What a
/// duplicated name used to need — matching the email portion, then narrowing by
/// org — is gone with the duplicates.
///
/// A query matching more than one row therefore means the file was hand-edited
/// behind the loader's back. That is still an error, never a first-match guess.
pub fn resolve_account<T: identity::Queryable>(
    accounts: &[T],
    query: &str,
) -> anyhow::Result<usize> {
    match identity::match_one(accounts, query) {
        identity::Match::One(only) => Ok(only),
        identity::Match::None => bail!("no account matches '{query}'"),
        identity::Match::Ambiguous(names) => {
            bail!("{}", ambiguous_query_message(query, &names));
        }
    }
}

/// The one-line explanation of an ambiguous query, shared by the CLI's own
/// resolution and by the live control endpoint's 409 body, so a caller gets the
/// same actionable sentence — and the same candidate list — whichever answered.
pub fn ambiguous_query_message(query: &str, names: &[String]) -> String {
    format!(
        "'{query}' is ambiguous — {} accounts share that name: {}. Account names are unique by \
         construction, so this config was edited by hand; fix the duplicate and retry.",
        names.len(),
        names.join(", ")
    )
}

impl Config {
    /// The one row `query` names, by [`resolve_account`]'s rule and with its errors.
    pub fn resolve_account(&self, query: &str) -> anyhow::Result<&Account> {
        let idx = resolve_account(&self.accounts, query)?;
        Ok(&self.accounts[idx])
    }

    /// [`Config::resolve_account`], for a caller that edits the row it names.
    pub fn resolve_account_mut(&mut self, query: &str) -> anyhow::Result<&mut Account> {
        let idx = resolve_account(&self.accounts, query)?;
        Ok(&mut self.accounts[idx])
    }

    /// Whether the fleet is empty: no usable account row, so a quarantined row does not count.
    pub fn holds_no_accounts(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Whether any account of `provider` is configured, in any state
    /// (disabled, grouped, pinned, or not).
    ///
    /// With [`Provider::Codex`](crate::provider::Provider::Codex) it is the
    /// gate every Codex-aware CLI sentence sits behind: a fleet for which it is
    /// false is Claude-only, and prints exactly what it printed before Codex
    /// accounts existed. Reads [`Self::accounts`], so a row the loader set
    /// aside never counts, and neither does one already deleted from the file.
    pub fn holds_provider(&self, provider: crate::provider::Provider) -> bool {
        self.accounts.iter().any(|a| a.provider == provider)
    }

    /// Resolve `name` to exactly one configured account by EXACT match on
    /// `Account.name`.
    ///
    /// [`resolve_account`]'s rule, with the group verbs' own not-found message.
    ///
    /// The one thing it does not borrow from `resolve_account` is the not-found
    /// text. The panel shells out blind, so an unknown name has to come back with
    /// the full roster to act on rather than a bare "no account matches".
    pub fn find_account_by_name(&self, name: &str) -> anyhow::Result<&Account> {
        match identity::match_one(&self.accounts, name) {
            identity::Match::One(idx) => Ok(&self.accounts[idx]),
            identity::Match::Ambiguous(names) => {
                bail!("{}", ambiguous_query_message(name, &names))
            }
            identity::Match::None => {
                let configured: Vec<&str> = self.accounts.iter().map(|a| a.name.as_str()).collect();
                bail!(
                    "no account named '{name}' in the config. Configured accounts: {}",
                    configured.join(", ")
                );
            }
        }
    }

    /// The provider of the account named `name`, by exact name (names are unique
    /// across a config). `None` for a name no row carries.
    pub fn provider_of(&self, name: &str) -> Option<crate::provider::Provider> {
        self.accounts
            .iter()
            .find(|account| account.name == name)
            .map(|account| account.provider)
    }

    /// Every account carrying the `group` label, in config order.
    pub fn group_members<'a>(&'a self, group: &'a str) -> impl Iterator<Item = &'a Account> {
        self.accounts.iter().filter(move |a| a.in_group(group))
    }

    /// Whether every enabled account of `provider` carries the `group` label.
    pub fn group_holds_every_enabled(
        &self,
        group: &str,
        provider: crate::provider::Provider,
    ) -> bool {
        self.accounts
            .iter()
            .filter(|a| a.provider == provider && !a.disabled.unwrap_or(false))
            .all(|a| a.in_group(group))
    }

    /// Every group label on every account, in config order, a label once per
    /// account that carries it.
    pub fn group_labels(&self) -> impl Iterator<Item = &str> {
        self.accounts
            .iter()
            .filter_map(|a| a.groups.as_ref())
            .flatten()
            .map(String::as_str)
    }

    /// Group name -> the (in-order) names of every account carrying that label,
    /// plus the count of accounts carrying NO label at all. An account in several
    /// groups appears once per group it is in — this is the shared aggregation
    /// both `tcr group ls` renderings read, so the two outputs can never disagree
    /// about which account is in which group.
    pub fn group_membership(&self) -> (BTreeMap<&str, Vec<&str>>, usize) {
        let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut ungrouped = 0usize;
        for account in &self.accounts {
            match account.groups.as_deref() {
                Some(labels) if !labels.is_empty() => {
                    for label in labels {
                        groups
                            .entry(label.as_str())
                            .or_default()
                            .push(account.name.as_str());
                    }
                }
                _ => ungrouped += 1,
            }
        }
        (groups, ungrouped)
    }

    /// The priorities `provider`'s accounts carry, in config order; a row with none is skipped.
    pub fn priorities_of(&self, provider: crate::provider::Provider) -> impl Iterator<Item = i64> {
        self.accounts
            .iter()
            .filter(move |a| a.provider == provider)
            .filter_map(|a| a.priority)
    }

    /// Per-account gating threshold — the account's `switchThreshold`, else the global
    /// one — collected in config order (which is the snapshot's account order), so the
    /// held-window display gates on exactly the threshold `Manager::eligible` routes on.
    pub fn switch_thresholds(&self) -> Vec<f64> {
        self.accounts
            .iter()
            .map(|a| a.switch_threshold.unwrap_or(self.switch_threshold))
            .collect()
    }

    /// Whether the loader set any account aside for holding no usable credential.
    pub fn holds_quarantined_accounts(&self) -> bool {
        !self.quarantined_accounts.is_empty()
    }

    /// Refuse a CLI load-edit-save of the file at `config_path` while any account
    /// is quarantined: a whole-config save would drop the row the loader set aside.
    pub fn refuse_edit_if_quarantined(&self, config_path: &std::path::Path) -> anyhow::Result<()> {
        if self.holds_quarantined_accounts() {
            bail!(
                "config at {} has {} account(s) with no usable credential, so the CLI cannot safely \
                 load-edit-save it: {}. Fix or remove the entry in the file by hand (the server \
                 itself tolerates it and keeps running).",
                config_path.display(),
                self.quarantined_accounts.len(),
                self.quarantined_accounts.join(", ")
            );
        }
        Ok(())
    }

    /// The `proxy.requestKey` a launcher may present to this config's server:
    /// `None` when the file holds none, or an empty one.
    pub fn usable_request_key(&self) -> Option<&str> {
        self.proxy
            .request_key
            .as_deref()
            .filter(|key| !key.is_empty())
    }

    /// Every account whose peer label (its name through
    /// [`tcr_peer_wire::sanitize_label`]) is one of `labels`, in config order.
    pub fn accounts_labelled<'a>(
        &'a self,
        labels: &'a [String],
    ) -> impl Iterator<Item = &'a Account> {
        self.accounts.iter().filter(move |account| {
            tcr_peer_wire::sanitize_label(&account.name).is_ok_and(|label| labels.contains(&label))
        })
    }

    /// The name `controlAccount` names, in either of its shapes; `None` when unset.
    pub fn control_account_name(&self) -> Option<&str> {
        self.control_account
            .as_ref()
            .map(super::ControlAccountRef::name)
    }
}
