//! The roster verbs keyed by identity: find the one row an identity probe
//! ([`crate::identity::probe`]) names, through [`crate::identity::resolve`],
//! and work on it. The name-keyed lookups (a user's query) are in `roster.rs`.

use super::{Account, Config};
use crate::identity::Resolved;

impl Config {
    /// The one row of [`Self::accounts`] that `probe` names, by
    /// [`crate::identity::resolve`]: a tie it cannot break is
    /// [`Resolved::Many`], and every caller refuses on it rather than guess.
    pub fn resolve_identity(&self, probe: &Account) -> Resolved {
        crate::identity::resolve(self.accounts.iter().enumerate(), probe)
    }
}
