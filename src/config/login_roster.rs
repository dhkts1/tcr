//! The account-list walks a login, an import, `tcr mint` and `tcr doctor` make
//! over [`Config::accounts`], so none of them borrows the list to do it by hand.

use std::collections::HashSet;

use super::{Account, Config};

impl Config {
    /// Append `account` behind the whole fleet: at one past the highest
    /// priority any row carries, or 0 when none carries one.
    pub fn append_at_next_priority(&mut self, account: Account) {
        let next_priority = self
            .accounts
            .iter()
            .filter_map(|a| a.priority)
            .max()
            .map_or(0, |max| max + 1);
        self.accounts.push(Account {
            priority: Some(next_priority),
            ..account
        });
    }

    /// The one row `query` names by exact name ([`crate::identity::match_one`]),
    /// `None` when no row does, and [`Config::resolve_account`]'s error when
    /// more than one does.
    pub fn resolve_account_or_none(&self, query: &str) -> anyhow::Result<Option<&Account>> {
        match crate::identity::match_one(&self.accounts, query) {
            crate::identity::Match::One(idx) => Ok(self.accounts.get(idx)),
            crate::identity::Match::None => Ok(None),
            crate::identity::Match::Ambiguous(names) => {
                anyhow::bail!("{}", super::ambiguous_query_message(query, &names))
            }
        }
    }

    /// The Claude row a stored login identity already is: by
    /// [`crate::identity::same_identity`] with the Anthropic `probe` when both
    /// it and the row carry an account UUID, else by `email` against the email
    /// part of the row's name ([`crate::identity::email_of`]), ignoring case.
    /// The first such row, in config order.
    pub fn stored_claude_row(&self, probe: &Account, email: Option<&str>) -> Option<&Account> {
        self.accounts
            .iter()
            .filter(|row| row.provider == crate::provider::Provider::Anthropic)
            .find(|row| {
                if probe.account_uuid.is_some() && row.account_uuid.is_some() {
                    crate::identity::same_identity(row, probe)
                } else {
                    email.is_some_and(|email| {
                        crate::identity::email_of(&row.name).eq_ignore_ascii_case(email)
                    })
                }
            })
    }

    /// Whether `probe`'s name is free for it: every row carrying that name is
    /// the same account as `probe` ([`crate::identity::same_identity`]), which
    /// holds when no row carries it.
    pub fn name_free_for(&self, probe: &Account) -> bool {
        self.accounts
            .iter()
            .filter(|a| a.name == probe.name)
            .all(|a| crate::identity::same_identity(a, probe))
    }

    /// Every row's name: the names a new login must not take.
    pub fn taken_names(&self) -> HashSet<String> {
        self.accounts.iter().map(|a| a.name.clone()).collect()
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
