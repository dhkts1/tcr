//! Resolving a user's account query to one config row, and the message for a query that matches more than one.

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

    /// Whether any account of `provider` is configured, in any state.
    pub fn holds_provider(&self, provider: crate::provider::Provider) -> bool {
        self.accounts.iter().any(|a| a.provider == provider)
    }
}
