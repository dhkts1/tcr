//! The account-list walks a login, an import, `tcr mint` and `tcr doctor` make
//! over [`Config::accounts`], so none of them borrows the list to do it by hand.

use super::{Account, Config};
use crate::identity::Resolved;

impl Config {
    /// The one row `probe` names ([`Self::resolve_identity`]); `None` when no
    /// row does, and when two tie, which every caller refuses rather than guess.
    pub fn identity_row(&self, probe: &Account) -> Option<&Account> {
        match self.resolve_identity(probe) {
            Resolved::One(index) => self.accounts.get(index),
            Resolved::None | Resolved::Many => None,
        }
    }

    /// Each Codex row's name beside the reserved groups it carries, sorted, in
    /// config order: the rows `tcr doctor` lists when no proxy answered.
    pub fn codex_reserved_groups(&self) -> Vec<(&str, Vec<String>)> {
        let reserved = self.reserved_group_names();
        self.accounts
            .iter()
            .filter(|account| account.provider == crate::provider::Provider::Codex)
            .map(|account| {
                let mut reserved_groups: Vec<String> = account
                    .groups
                    .iter()
                    .flatten()
                    .filter(|group| reserved.contains(*group))
                    .cloned()
                    .collect();
                reserved_groups.sort();
                (account.name.as_str(), reserved_groups)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    /// Only Codex rows, in config order, each with just its reserved groups, sorted.
    #[test]
    fn codex_reserved_groups_lists_codex_rows_with_their_sorted_reserved_groups() {
        let config: Config = serde_json::from_str(
            r#"{ "groupSettings": { "zeta": { "reserved": true }, "alpha": { "reserved": true } },
                 "accounts": [
                   { "name": "alice@example.com", "accessToken": "at-a", "groups": ["alpha"] },
                   { "name": "bob@example.com", "provider": "codex", "accessToken": "at-b",
                     "groups": ["zeta", "plain", "alpha"] },
                   { "name": "carol@example.com", "provider": "codex", "accessToken": "at-c" } ] }"#,
        )
        .expect("fixture loads");
        assert_eq!(
            config.codex_reserved_groups(),
            vec![
                (
                    "bob@example.com",
                    vec!["alpha".to_string(), "zeta".to_string()]
                ),
                ("carol@example.com", Vec::new()),
            ]
        );
    }
}
