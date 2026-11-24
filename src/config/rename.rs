//! The migration that gives duplicated account names unique ones, in memory and on disk.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use super::file::{read_document, write_atomic};
use super::{Account, Config, ConfigError, ControlAccountRef};

/// One account [`load`] renamed to restore unique names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRename {
    /// The duplicated name the entry carried on disk.
    pub from: String,
    /// The unique name it now carries, minted by [`crate::identity::mint_name`].
    pub to: String,
    /// The org the new name was derived from, for the log line. `None` when the
    /// entry stored no org at all and the name fell through to a counter.
    pub org: Option<String>,
}

impl Config {
    /// Restore unique account names on a config that has duplicates, in memory AND
    /// on disk, once.
    ///
    /// Never refuses. A loader that rejected a duplicated config would take the
    /// whole fleet down the moment it upgraded — the config is what it is, and it
    /// was written by earlier `tcr` builds that permitted it. So: rename, log a line
    /// per rename, continue. The durable write failing is recorded on
    /// [`Config::rename_write_error`] rather than returned, because a server has to
    /// boot either way; the CLI's own load ([`crate::cli::load_for_edit`]) reads
    /// that field and refuses to run an edit verb on top of it, since a rename that
    /// never reached disk is one every future process re-derives.
    pub(super) fn migrate_duplicate_names(&mut self, path: &Path, file_index: &[usize]) {
        let renames = plan_renames(&self.accounts);
        if renames.is_empty() {
            return;
        }

        // The control account, if it names a duplicated row, currently resolves to
        // the FIRST row in file order carrying that name — that is what the running
        // server does with it today, so that is the row it must keep pointing at.
        let control_rename = self.control_account.as_ref().and_then(|control| {
            let first = self
                .accounts
                .iter()
                .position(|a| a.name == control.name())?;
            renames
                .iter()
                .find(|(index, _)| *index == first)
                .map(|(_, rename)| rename.to.clone())
        });

        // Both channels, deliberately, matching what `main.rs` already does for the
        // legacy-`throttle` migration a dozen lines from its own call to this.
        // `tracing` alone reaches nobody here: a CLI verb never installs a
        // subscriber at all, and `run_server` loads the config BEFORE
        // `init_tracing`, so the tracing line is dropped on the floor on every path
        // that can reach this code. Renaming a person's accounts under them is the
        // last thing that may happen silently.
        let say = |line: &str| {
            tracing::warn!("{line}");
            eprintln!("[tcr] {line}");
        };

        for (index, rename) in &renames {
            let where_from = match &rename.org {
                Some(org) => format!("org {org}"),
                None => "no org on file".to_string(),
            };
            say(&format!(
                "renamed account: {} -> {} ({where_from})",
                rename.from, rename.to
            ));
            self.accounts[*index].name.clone_from(&rename.to);
        }
        if let Some(name) = &control_rename {
            say(&format!(
                "control account followed the rename to '{name}' — it named the first row in file \
                 order, which is the row the server resolved it to"
            ));
            self.control_account = Some(ControlAccountRef::Name(name.clone()));
        }

        let writes: Vec<(usize, &str)> = renames
            .iter()
            .filter_map(|(index, rename)| Some((*file_index.get(*index)?, rename.to.as_str())))
            .collect();
        if writes.len() != renames.len() {
            self.rename_write_error = Some(
                "could not map every renamed account back to its entry in the file".to_string(),
            );
        } else if let Err(err) = save_renames(path, &writes, control_rename.as_deref()) {
            tracing::error!(error = %err, "could not persist the account renames");
            eprintln!("[tcr] could not persist the account renames: {err}");
            self.rename_write_error = Some(err.to_string());
        }

        self.renamed_accounts = renames.into_iter().map(|(_, rename)| rename).collect();
    }
}

/// The rename each duplicated-name account needs, as `(index into `accounts`,
/// rename)`, in fleet order. Empty when every name is already unique.
///
/// One row per duplicated name keeps the bare email: the one whose
/// `organizationType` is a personal plan (`claude_max` / `claude_pro`), else the
/// lowest priority number, else the first in file order. That is the row a
/// person means when they type their own email — the personal account is the one
/// they had before their company gave them a seat.
fn plan_renames(accounts: &[Account]) -> Vec<(usize, AccountRename)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for account in accounts {
        *counts.entry(account.name.as_str()).or_default() += 1;
    }
    let duplicated: HashSet<&str> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(name, _)| *name)
        .collect();
    if duplicated.is_empty() {
        return Vec::new();
    }

    // Every name in the fleet is off-limits as a minted name, and so is every
    // name minted earlier in this same pass.
    let mut taken: HashSet<String> = accounts.iter().map(|a| a.name.clone()).collect();
    let mut renames = Vec::new();
    for name in {
        // Deterministic order, so two runs over one file plan the same renames.
        let mut names: Vec<&str> = duplicated.into_iter().collect();
        names.sort_unstable();
        names
    } {
        let rows: Vec<usize> = accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.name == name)
            .map(|(i, _)| i)
            .collect();
        let Some(keeper) = rows
            .iter()
            .copied()
            .find(|&i| is_personal_plan(accounts[i].organization_type.as_deref()))
            .or_else(|| {
                // `min_by_key` keeps the FIRST row on a tie, which is the file
                // order this falls back to when no priority distinguishes them.
                rows.iter()
                    .copied()
                    .min_by_key(|&i| accounts[i].priority.unwrap_or(0))
            })
        else {
            // A duplicated name has at least two rows, so this is unreachable.
            continue;
        };
        for index in rows {
            if index == keeper {
                continue;
            }
            let account = &accounts[index];
            let minted = crate::identity::mint_name(
                crate::identity::email_of(&account.name),
                account.org_name.as_deref(),
                account.org_uuid.as_deref(),
                &taken,
            );
            taken.insert(minted.clone());
            renames.push((
                index,
                AccountRename {
                    from: account.name.clone(),
                    to: minted,
                    org: account
                        .org_name
                        .clone()
                        .or_else(|| account.org_uuid.clone()),
                },
            ));
        }
    }
    renames.sort_by_key(|(index, _)| *index);
    renames
}

/// Whether an `organizationType` names a plan a person holds in their own right
/// rather than through a seat someone granted them.
fn is_personal_plan(organization_type: Option<&str>) -> bool {
    matches!(organization_type, Some("claude_max" | "claude_pro"))
}

/// Write the migration's renames — and the `controlAccount` that followed one —
/// into the file at `path`, touching NOTHING else.
///
/// Same targeted read-modify-write shape as [`save_disabled`], and for the same
/// reason it spells out: a whole-`Config` [`save`] would reformat and re-emit
/// every key in the document, including ones this process never modelled, at the
/// worst possible moment — the boot of a build the operator just upgraded to.
///
/// Entries are addressed by their INDEX in the file's own `accounts` array, not
/// by identity. Identity is exactly what cannot distinguish these rows: the
/// migration exists for duplicates, and a pair written before org UUIDs were
/// stored is two rows `same_identity` calls one.
fn save_renames(
    path: &Path,
    renames: &[(usize, &str)],
    control_account: Option<&str>,
) -> Result<(), ConfigError> {
    let mut doc = read_document(path)?;
    let entries = doc
        .get_mut("accounts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            ConfigError::Parse(<serde_json::Error as serde::de::Error>::custom(
                "the accounts key is not an array",
            ))
        })?;
    for (index, name) in renames {
        let entry = entries
            .get_mut(*index)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                ConfigError::Parse(<serde_json::Error as serde::de::Error>::custom(format!(
                    "accounts[{index}] is not an object"
                )))
            })?;
        entry.insert("name".to_string(), Value::String((*name).to_string()));
    }
    if let Some(name) = control_account {
        doc.insert(
            "controlAccount".to_string(),
            Value::String(name.to_string()),
        );
    }
    write_atomic(path, &serde_json::to_string_pretty(&doc)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load;
    use crate::config::tests::{DISABLE_SAMPLE, read_json, tmp_path};
    use serde_json::json;
    use std::fs;

    // ---- the duplicate-name migration -------------------------------------

    /// The fleet's actual shape: one person, a personal Max org and a company
    /// Team org, one email between them, and `controlAccount` naming it.
    const DUPLICATE_NAME_FILE: &str = r#"{
      "upstream": "https://api.anthropic.com",
      "controlAccount": "henry@example.com",
      "routes": { "hand-edited": "must survive" },
      "accounts": [
        { "name": "henry@example.com", "type": "oauth", "accessToken": "at-personal",
          "refreshToken": "rt-personal", "expiresAt": 1893456000000, "priority": 1,
          "accountUuid": "aaaaaaaa-1111-1111-1111-111111111111",
          "orgUuid": "11111111-1111-1111-1111-111111111111",
          "orgName": "Henry Personal", "organizationType": "claude_max",
          "groups": ["gil"] },
        { "name": "henry@example.com", "type": "oauth", "accessToken": "at-team",
          "refreshToken": "rt-team", "expiresAt": 1893456000000, "priority": 0,
          "accountUuid": "aaaaaaaa-1111-1111-1111-111111111111",
          "orgUuid": "22222222-2222-2222-2222-222222222222",
          "orgName": "Henry Token", "organizationType": "claude_team" },
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
          "expiresAt": 1893456000000, "priority": 2, "sx": "unmodelled" }
      ]
    }"#;

    /// The whole contract of the migration, in one pass: the personal row keeps
    /// the bare email, the Team row takes its org's slug, `controlAccount`
    /// still names the personal row, and NOTHING else in the document moves.
    ///
    /// Note the priorities are deliberately inverted (`claude_max` carries the
    /// HIGHER number). A keeper chosen by priority would pick the Team row, so
    /// this only passes if the personal-plan rule is what actually ran.
    #[test]
    fn load_renames_a_duplicated_name_and_touches_nothing_else() {
        let path = tmp_path("rename-migration");
        fs::write(&path, DUPLICATE_NAME_FILE).unwrap();
        let before = read_json(&path);

        let config = load(&path).expect("a duplicated name loads, it never refuses");

        assert_eq!(config.accounts[0].name, "henry@example.com");
        assert_eq!(config.accounts[1].name, "henry@example.com/henry-token");
        assert_eq!(config.accounts[2].name, "alice@example.com");
        assert_eq!(
            config.control_account,
            Some(ControlAccountRef::Name("henry@example.com".to_string())),
            "control stays on the personal row, which kept the bare email"
        );
        assert_eq!(config.renamed_accounts.len(), 1);
        assert_eq!(config.renamed_accounts[0].from, "henry@example.com");
        assert_eq!(
            config.renamed_accounts[0].to,
            "henry@example.com/henry-token"
        );
        assert_eq!(
            config.renamed_accounts[0].org.as_deref(),
            Some("Henry Token")
        );
        assert_eq!(config.rename_write_error, None);

        // The rename is DURABLE, and it is the only thing that changed. Diffing
        // the whole document — not just re-reading the field we wrote — is what
        // catches a targeted edit that quietly dropped an unmodelled key.
        let mut after = read_json(&path);
        assert_eq!(
            after["accounts"][1]["name"],
            json!("henry@example.com/henry-token")
        );
        after["accounts"][1]["name"] = json!("henry@example.com");
        assert_eq!(
            after, before,
            "the write touched something other than the renamed row's name"
        );

        fs::remove_file(&path).ok();
    }

    /// Loading the migrated file again renames nothing and rewrites nothing.
    /// Without this the migration could be "working" by renaming on every boot,
    /// handing out a new name each time.
    #[test]
    fn a_second_load_of_a_migrated_file_changes_nothing() {
        let path = tmp_path("rename-idempotent");
        fs::write(&path, DUPLICATE_NAME_FILE).unwrap();
        load(&path).expect("first load migrates");
        let after_first = fs::read_to_string(&path).unwrap();

        let second = load(&path).expect("second load");
        assert!(second.renamed_accounts.is_empty(), "nothing left to rename");
        assert_eq!(second.accounts[1].name, "henry@example.com/henry-token");
        assert_eq!(
            after_first,
            fs::read_to_string(&path).unwrap(),
            "the second load must leave the file byte-identical"
        );

        fs::remove_file(&path).ok();
    }

    /// When the row that gets renamed is the FIRST one in file order — the row
    /// a running server resolves `controlAccount` to today — the key follows it,
    /// so control lands on the same account it was on before the upgrade.
    #[test]
    fn control_follows_the_rename_when_it_named_the_renamed_row() {
        let path = tmp_path("rename-control-follows");
        // No personal plan on either row, so the keeper is decided by priority:
        // the SECOND row (priority 0) keeps the bare email, and the first — the
        // one `controlAccount` resolves to today — is the one renamed.
        fs::write(
            &path,
            r#"{
              "controlAccount": "henry@example.com",
              "accounts": [
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-a",
                  "priority": 1, "accountUuid": "aaaa-1", "orgUuid": "org-a",
                  "orgName": "Henry Token" },
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-b",
                  "priority": 0, "accountUuid": "aaaa-1", "orgUuid": "org-b",
                  "orgName": "Henry Personal" }
              ]
            }"#,
        )
        .unwrap();

        let config = load(&path).expect("loads");
        assert_eq!(config.accounts[0].name, "henry@example.com/henry-token");
        assert_eq!(config.accounts[1].name, "henry@example.com");
        assert_eq!(
            config.control_account,
            Some(ControlAccountRef::Name(
                "henry@example.com/henry-token".to_string()
            )),
            "control must follow the FIRST row, which is the one the server resolved it to"
        );
        assert_eq!(
            read_json(&path)["controlAccount"],
            json!("henry@example.com/henry-token"),
            "and durably, not only in memory"
        );

        fs::remove_file(&path).ok();
    }

    /// With no `orgName` to slug, the name falls back to the first eight
    /// characters of the org uuid — a name is still needed, and a stable one.
    #[test]
    fn a_row_with_no_org_name_is_named_from_its_org_uuid() {
        let path = tmp_path("rename-uuid-fallback");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-a",
                  "priority": 0, "accountUuid": "aaaa-1", "orgUuid": "abcdefgh-1111" },
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-b",
                  "priority": 1, "accountUuid": "aaaa-1", "orgUuid": "zyxwvuts-2222" }
            ] }"#,
        )
        .unwrap();

        let config = load(&path).expect("loads");
        assert_eq!(config.accounts[0].name, "henry@example.com");
        assert_eq!(config.accounts[1].name, "henry@example.com/zyxwvuts");

        fs::remove_file(&path).ok();
    }

    /// A quarantined entry sits BEFORE the duplicated pair in the file, so the
    /// in-memory index and the on-disk index differ. The rename must land on the
    /// on-disk row — writing by the in-memory index would rename the wrong
    /// account, and the two indices agree in every other fixture here, so
    /// nothing else in this file could catch it.
    #[test]
    fn the_rename_writes_by_the_files_own_index_not_the_in_memory_one() {
        let path = tmp_path("rename-after-quarantine");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "credential-less@example.com", "type": "oauth", "importFrom": "elsewhere" },
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-a",
                  "priority": 0, "accountUuid": "aaaa-1", "orgUuid": "org-a",
                  "orgName": "Henry Personal" },
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-b",
                  "priority": 1, "accountUuid": "aaaa-1", "orgUuid": "org-b",
                  "orgName": "Henry Token" }
            ] }"#,
        )
        .unwrap();

        let config = load(&path).expect("loads");
        assert_eq!(
            config.quarantined_accounts,
            vec!["credential-less@example.com"]
        );
        assert_eq!(config.accounts[0].name, "henry@example.com");
        assert_eq!(config.accounts[1].name, "henry@example.com/henry-token");

        // On disk the rows are at 1 and 2, and the quarantined row is still
        // there untouched — `load` drops it from memory, never from the file.
        let after = read_json(&path);
        assert_eq!(
            after["accounts"][0]["name"],
            json!("credential-less@example.com")
        );
        assert_eq!(after["accounts"][0]["importFrom"], json!("elsewhere"));
        assert_eq!(after["accounts"][1]["name"], json!("henry@example.com"));
        assert_eq!(
            after["accounts"][2]["name"],
            json!("henry@example.com/henry-token")
        );

        fs::remove_file(&path).ok();
    }

    /// Three rows on one email: the personal row keeps it and the other two get
    /// distinct names. A minting rule that only ever considered the ORIGINAL
    /// names would hand the same name to both.
    #[test]
    fn three_rows_sharing_a_name_all_end_up_distinct() {
        let path = tmp_path("rename-three-way");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-a",
                  "priority": 0, "orgName": "Acme" },
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-b",
                  "priority": 1, "orgName": "Acme", "organizationType": "claude_max" },
                { "name": "henry@example.com", "type": "oauth", "accessToken": "at-c",
                  "priority": 2, "orgName": "Acme" }
            ] }"#,
        )
        .unwrap();

        let config = load(&path).expect("loads");
        let names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "henry@example.com/acme",
                "henry@example.com",
                "henry@example.com/acme-2"
            ],
            "the Max row keeps the email; the other two get distinct names"
        );
        assert_eq!(
            names.iter().collect::<HashSet<_>>().len(),
            3,
            "the whole point: three rows, three names"
        );

        fs::remove_file(&path).ok();
    }

    /// A config that never had a duplicate is not rewritten at all — no rename
    /// pass, no write, and `renamed_accounts` empty. The migration must be
    /// invisible to the fleets it does not concern.
    #[test]
    fn a_config_with_unique_names_is_left_byte_identical() {
        let path = tmp_path("rename-noop");
        fs::write(&path, DISABLE_SAMPLE).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        let config = load(&path).expect("loads");
        assert!(config.renamed_accounts.is_empty());
        assert_eq!(config.rename_write_error, None);
        assert_eq!(before, fs::read_to_string(&path).unwrap());

        fs::remove_file(&path).ok();
    }
}
