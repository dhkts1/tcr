//! Adding, updating and removing an `accounts` entry.

use std::path::Path;

use serde_json::{Map, Value};

use super::credential_version::stamp_credential_version;
use super::edit::CODEX_SYNC_PENDING_KEY;
use super::edit::{Edit, edit_document};
use super::edit::{EntryMatch, locate_account_entry};
use super::file::{read_document, write_atomic};
use super::tokens::Credentials;
use super::{Account, Config, ConfigError, ControlAccountRef};

/// The top-level key that tells the empty-fleet Claude Code import to leave
/// this config alone. Absent means the import may run; `false` means a person
/// emptied the fleet on purpose (see [`remove_account_entry`]). Read out of
/// [`Config::extra`] by [`Config::refuses_claude_code_import`].
pub const CLAUDE_CODE_IMPORT_KEY: &str = "claudeCodeImport";

impl Config {
    /// Whether this config said "do not import this machine's Claude Code
    /// login into me", which a delete of the last account writes. Without it,
    /// the next `tcr status` (the panel polls it every few seconds) finds an
    /// empty fleet and imports the login again, so a delete of the last
    /// account undid itself one poll later.
    pub fn refuses_claude_code_import(&self) -> bool {
        self.extra.get("claudeCodeImport") == Some(&Value::Bool(false))
    }
}

/// What [`remove_account_entry`] did to the on-disk document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountRemoval {
    /// The entry is gone and the file was rewritten.
    Removed {
        /// `controlAccount` named the removed entry and was dropped with it.
        control_cleared: bool,
        /// The removed entry was the last one, so the file now also carries
        /// [`CLAUDE_CODE_IMPORT_KEY`] `false`.
        fleet_emptied: bool,
    },
    /// Nothing on disk carries that identity. Nothing was written.
    NoEntry,
    /// More than one entry carries it. Nothing was written.
    Ambiguous,
}

/// Delete the ONE `accounts` entry carrying `target`'s identity from the file at
/// `path`, leaving every other key and every other entry exactly as written.
///
/// The raw-document shape of [`save_disabled`] and [`save_group_membership`],
/// never a `Config` round trip. A round trip cannot write back an entry the
/// loader quarantined (an API-key row, a `type` that is not a string, a foreign
/// `upstream`), which is why the CLI's load-edit-save path refuses any file
/// holding one; deleting through it refused EVERY delete on such a fleet. This
/// path never deserializes the other entries, so they survive byte for byte.
///
/// Two more keys are kept true to the removal in the same write, so the file
/// and the running proxy cannot disagree afterwards:
///
/// - `controlAccount`, when it names the removed entry, is dropped. The running
///   proxy stops treating a removed row as the control account at once; left
///   in the file, the name would quietly make a later login of the same name
///   the control account again.
/// - When the removed entry was the last one, [`CLAUDE_CODE_IMPORT_KEY`] is
///   written `false` (see [`Config::refuses_claude_code_import`]).
pub fn remove_account_entry(path: &Path, target: &Account) -> Result<AccountRemoval, ConfigError> {
    let mut doc = read_document(path)?;
    let index = match locate_account_entry(&doc, target) {
        EntryMatch::One(index) => index,
        EntryMatch::None => return Ok(AccountRemoval::NoEntry),
        EntryMatch::Many => return Ok(AccountRemoval::Ambiguous),
    };
    let Some(entries) = doc.get_mut("accounts").and_then(Value::as_array_mut) else {
        // `locate_account_entry` only answers `One` for a real array.
        return Ok(AccountRemoval::NoEntry);
    };
    entries.remove(index);
    let fleet_emptied = entries.is_empty();
    let names_target = doc
        .get("controlAccount")
        .and_then(|raw| serde_json::from_value::<ControlAccountRef>(raw.clone()).ok())
        .is_some_and(|control| control.name() == target.name);
    if names_target {
        doc.remove("controlAccount");
    }
    if fleet_emptied {
        doc.insert(CLAUDE_CODE_IMPORT_KEY.to_string(), Value::Bool(false));
    }
    write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    Ok(AccountRemoval::Removed {
        control_cleared: names_target,
        fleet_emptied,
    })
}

/// What a targeted [`save_account`] upsert did to the on-disk document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountWrite {
    /// No entry on disk carried this identity — a new one was appended, holding
    /// every field `account` carries (identity, credentials, and routing
    /// knobs) — with one deliberate exception: an ABSENT `priority` is filled
    /// in as `max(existing priorities) + 1`, joining the back of the fleet,
    /// rather than left absent (which reads as 0 at runtime and silently
    /// promotes the new account to the primary tier). An explicit `priority`
    /// the caller submitted is never overridden. See [`merge_account`]'s
    /// doc-comment on the Added arm for the full rationale.
    Added,
    /// Exactly one entry carried this identity — its CREDENTIAL fields
    /// (`accessToken` / `refreshToken` / `expiresAt`) were updated in place,
    /// `type` was stamped to `account.account_type` unconditionally (and
    /// `provider` with it, Anthropic written as the absent key), and any
    /// of `accountUuid` / `orgUuid` / `orgName` the entry was MISSING got
    /// backfilled from `account` (never overwriting a value already present —
    /// this is what permanently stops a legacy no-org entry from loosely
    /// matching every org variant of that person forever). `priority`,
    /// `disabled`, `switchThreshold`, and any unmodelled `extra` key are
    /// untouched, exactly as [`Config::merge_tokens`] leaves them.
    Updated,
    /// More than one on-disk entry carries this identity; nothing was written.
    /// Same refusal as [`DisabledWrite::Ambiguous`], for the same reason: a
    /// write here would have to guess which entry to overwrite.
    Ambiguous,
    /// The document has an `accounts` key that exists but is not a JSON array —
    /// too corrupt a shape to append into blindly. Nothing was written.
    Unwritable,
}

impl std::fmt::Display for AccountWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Ambiguous => "more than one entry on disk carries this identity",
            Self::Unwritable => "the accounts key is not an array",
        })
    }
}

/// Insert-or-replace, by identity, the ONE `accounts` entry matching `account` —
/// the durable half of live account-add (`POST /_tcr/accounts`,
/// [`crate::manager::Manager::add_or_update_account`]).
///
/// Sibling of [`save_disabled`], and it shares its whole shape: read the raw
/// document, mutate ONLY what needs to change, write back atomically only if
/// something did. It differs from `save_disabled` in exactly one place — a MISS
/// is not a refusal here. `save_disabled` refuses on [`DisabledWrite::NoEntry`]
/// because its `target` is an identity-only probe with no real credential to
/// write ([`crate::identity::probe`] leaves `access_token` empty); appending it
/// would create a useless entry. `account` here is always a COMPLETE record —
/// either a brand-new login or an existing account's own identity with fresh
/// credentials merged in (`Manager::persist_replaced`) — so a miss is filled in
/// rather than refused. That is also why `find_account_entry` (which returns
/// a refusal on a miss) is not reused directly here; this
/// calls the lower-level [`locate_account_entry`] it is itself built on, so both
/// functions still run the identical resolution scan.
///
/// An identity matching MORE than one on-disk entry still refuses: guessing
/// which one to overwrite risks stamping fresh credentials over a DIFFERENT
/// account's single-use refresh token — exactly the failure `find_account_entry`
/// exists to rule out.
pub fn save_account(path: &Path, account: &Account) -> Result<AccountWrite, ConfigError> {
    edit_document(path, |doc| merge_account(doc, account))
}

impl Edit for AccountWrite {
    fn wrote(&self) -> bool {
        matches!(self, Self::Added | Self::Updated)
    }
    fn landed(&self) -> bool {
        self.wrote()
    }
}

/// Insert-or-replace `account` by identity in `doc`. See [`save_account`].
fn merge_account(
    doc: &mut Map<String, Value>,
    account: &Account,
) -> Result<AccountWrite, ConfigError> {
    match locate_account_entry(doc, account) {
        EntryMatch::Many => Ok(AccountWrite::Ambiguous),
        EntryMatch::One(index) => {
            // Proven present by the immutable scan `locate_account_entry` just
            // ran; this chain cannot miss in practice, but a save must never
            // silently drop a credential if it somehow does.
            if let Some(entry) = doc
                .get_mut("accounts")
                .and_then(Value::as_array_mut)
                .and_then(|entries| entries.get_mut(index))
                .and_then(Value::as_object_mut)
            {
                let credentials = Credentials {
                    access_token: &account.access_token,
                    refresh_token: account.refresh_token.as_deref(),
                    expires_at: account.expires_at,
                };
                if let Value::Object(fields) = serde_json::to_value(&credentials)? {
                    entry.extend(fields);
                }
                // Same stamp as `merge_tokens`: this entry just received a real
                // credential, so it carries its provider's version from here on
                // (`locate_account_entry` matched it within `account`'s provider).
                stamp_credential_version(entry, account.provider);
                // `type` always wins — `save_account` is only ever called with a
                // real credential, so a stale stored type (e.g. an API-key row
                // re-added as OAuth) is corrected here; leaving it wrong is a
                // silent death (`refresh_plan` skips any account whose type is
                // not `"oauth"`).
                //
                // Identity fields are BACKFILLED — written only where the entry
                // does not already carry a value, never overwriting one that is
                // present. `locate_account_entry` may have matched loosely (the
                // org-unknown tolerance), so this is what turns a legacy no-org
                // entry into a fully-known one: the NEXT submission for a
                // genuinely different org of the same person then requires an
                // exact match instead of tolerating the still-unknown org key,
                // closing the split-brain trigger for good rather than only
                // for this one write.
                entry.insert(
                    "type".to_string(),
                    serde_json::to_value(&account.account_type)?,
                );
                // `provider` is stamped beside `type`, with Anthropic spelled as
                // the ABSENT key — the same rule `skip_serializing_if` gives the
                // Added arm below, so this rewrite never writes
                // `"provider": "anthropic"` onto a row.
                match account.provider {
                    crate::provider::Provider::Anthropic => {
                        entry.remove("provider");
                    }
                    crate::provider::Provider::Codex => {
                        entry.insert(
                            "provider".to_string(),
                            Value::String(account.provider.as_str().to_string()),
                        );
                    }
                }
                // A Codex row's folder is OVERWRITTEN, never backfilled: a re-import from
                // another folder moves it, and a re-sign with a login of tcr's own leaves no
                // folder to point at, so `None` removes the key. A backfill would keep the
                // first folder forever, and TcrBar would go on offering to re-import from it.
                // An Anthropic submission leaves the entry's key alone.
                match account.provider {
                    crate::provider::Provider::Anthropic => {}
                    crate::provider::Provider::Codex => match &account.codex_home {
                        Some(folder) => {
                            entry.insert("codexHome".to_string(), Value::String(folder.clone()));
                        }
                        None => {
                            entry.remove("codexHome");
                        }
                    },
                }
                match account.provider {
                    crate::provider::Provider::Anthropic => {}
                    crate::provider::Provider::Codex => {
                        entry.remove(CODEX_SYNC_PENDING_KEY);
                    }
                }
                let present = |entry: &Map<String, Value>, key: &str| matches!(entry.get(key), Some(Value::String(s)) if !s.is_empty());
                if !present(entry, "accountUuid")
                    && let Some(uuid) = &account.account_uuid
                {
                    entry.insert("accountUuid".to_string(), Value::String(uuid.clone()));
                }
                if !present(entry, "orgUuid")
                    && let Some(uuid) = &account.org_uuid
                {
                    entry.insert("orgUuid".to_string(), Value::String(uuid.clone()));
                }
                if !present(entry, "orgName")
                    && let Some(name) = &account.org_name
                {
                    entry.insert("orgName".to_string(), Value::String(name.clone()));
                }
                // Plan fields are OVERWRITTEN, not backfilled — the opposite of
                // the identity fields above, deliberately. An identity does not
                // change (that is what makes it an identity), so a stored value
                // is authoritative and a fresh one that disagrees is a reason to
                // refuse. A PLAN changes: an org upgrades from Max 5x to Max 20x,
                // a personal account joins a Team. Backfilling those would pin
                // the fleet's first reading forever and quietly show a stale
                // plan on every surface. A `None` still writes nothing, so a
                // caller with no reading never blanks a value we do have.
                if let Some(org_type) = &account.organization_type {
                    entry.insert(
                        "organizationType".to_string(),
                        Value::String(org_type.clone()),
                    );
                }
                if let Some(tier) = &account.rate_limit_tier {
                    entry.insert("rateLimitTier".to_string(), Value::String(tier.clone()));
                }
                if let Some(seat) = &account.seat_tier {
                    entry.insert("seatTier".to_string(), Value::String(seat.clone()));
                }
                if let Some(display) = &account.plan_display_name {
                    entry.insert(
                        "planDisplayName".to_string(),
                        Value::String(display.clone()),
                    );
                }
            }
            Ok(AccountWrite::Updated)
        }
        EntryMatch::None => {
            // Distinguished from "no `accounts` key at all" (fine — create it)
            // so a document where `accounts` is present but the wrong JSON type
            // is never silently overwritten with a fresh single-element array.
            if matches!(doc.get("accounts"), Some(v) if !v.is_array()) {
                return Ok(AccountWrite::Unwritable);
            }
            let entries = doc
                .entry("accounts".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            let Value::Array(entries) = entries else {
                // Unreachable given the check above, but never index into a
                // shape that was not actually verified to be an array.
                return Ok(AccountWrite::Unwritable);
            };
            let mut new_entry = serde_json::to_value(account)?;
            // An appended account with no explicit priority joins the BACK
            // of the fleet — `max(existing priorities) + 1` — rather than
            // being left absent. `priority` is deliberate-default here, not
            // incidental: `skip_serializing_if` means a `None` would otherwise
            // serialize to no key at all, and an absent `priority` reads as 0
            // at runtime (`AccountRuntime::from_config`'s `unwrap_or(0)`),
            // which silently promotes a freshly added account to the PRIMARY
            // tier ahead of the established fleet. Mirrors
            // `login::upsert_account`'s historical default. Skipped when the
            // caller submitted an explicit priority (the documented
            // new-account case — see `AddAccountRequest`'s doc-comment in
            // `proxy.rs`), which is never overridden.
            if account.priority.is_none() {
                let next_priority = entries
                    .iter()
                    .filter_map(|e| e.get("priority").and_then(Value::as_i64))
                    .max()
                    .map_or(0, |max| max + 1);
                if let Value::Object(fields) = &mut new_entry {
                    fields.insert("priority".to_string(), Value::from(next_priority));
                }
            }
            // `Account` deliberately carries no `credentialVersion` field (see
            // [`CREDENTIAL_VERSION`]'s doc-comment), so a freshly serialized
            // entry has the key only when its `extra` carried one — stamp it
            // here, at its provider's version, rather than leave a brand-new
            // account the one entry the read path has to backfill. A new Codex
            // row is therefore at [`CODEX_CREDENTIAL_VERSION`] from its first
            // write, which is what keeps an older proxy from ever serving it.
            if let Value::Object(fields) = &mut new_entry {
                stamp_credential_version(fields, account.provider);
            }
            entries.push(new_entry);
            Ok(AccountWrite::Added)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{DISABLE_SAMPLE, read_json, tmp_path};
    use serde_json::json;
    use std::fs;

    /// `save_account`'s update path treats the plan the opposite way to the
    /// identity fields beside it: overwrite, because a plan changes.
    #[test]
    fn save_account_overwrites_the_plan_while_backfilling_identity() {
        let path = tmp_path("plan-on-relogin");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "alice@example.com", "accessToken": "at-old", "orgName": "Kept",
                  "organizationType": "claude_max", "rateLimitTier": "default_claude_max_5x" }
            ] }"#,
        )
        .unwrap();

        let mut account = full_account("alice@example.com", "at-new");
        account.org_name = Some("Ignored".to_string());
        account.organization_type = Some("claude_team".to_string());
        account.rate_limit_tier = Some("default_raven".to_string());
        account.seat_tier = Some("team_standard".to_string());
        assert_eq!(
            save_account(&path, &account).unwrap(),
            AccountWrite::Updated
        );

        let after = read_json(&path);
        assert_eq!(
            after["accounts"][0]["orgName"], "Kept",
            "identity stays backfill-only — a stored org name is never overwritten"
        );
        assert_eq!(after["accounts"][0]["organizationType"], "claude_team");
        assert_eq!(after["accounts"][0]["rateLimitTier"], "default_raven");
        assert_eq!(after["accounts"][0]["seatTier"], "team_standard");
        fs::remove_file(&path).ok();
    }

    // --- save_account (the durable half of live account-add) ----------------

    /// A full, valid account record — the shape `Manager::add_or_update_account`
    /// always hands `save_account`, whether it is a brand-new login or an
    /// existing account's own identity with fresh credentials merged in.
    fn full_account(name: &str, access_token: &str) -> Account {
        Account {
            access_token: access_token.to_string(),
            refresh_token: Some(format!("rt-{access_token}")),
            expires_at: Some(1_800_000_000_000),
            priority: Some(2),
            ..Account::fixture(name)
        }
    }

    /// A miss appends a brand-new entry carrying every field, and leaves the
    /// rest of the document untouched.
    #[test]
    fn save_account_appends_a_new_identity() {
        let path = tmp_path("add-append");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_account(&path, &full_account("carol@example.com", "at-carol")).unwrap(),
            AccountWrite::Added
        );

        let after = read_json(&path);
        let accounts = after["accounts"].as_array().expect("accounts array");
        assert_eq!(accounts.len(), 3, "appended, not replacing anything");
        assert_eq!(accounts[2]["name"], json!("carol@example.com"));
        assert_eq!(accounts[2]["accessToken"], json!("at-carol"));
        assert_eq!(accounts[2]["refreshToken"], json!("rt-at-carol"));
        assert_eq!(accounts[2]["priority"], json!(2));
        // The pre-existing rows and unmodelled top-level keys are untouched.
        assert_eq!(accounts[0]["name"], json!("acct-a"));
        assert_eq!(accounts[1]["name"], json!("acct-b"));
        assert_eq!(after["warmupSeconds"], json!(900));
        fs::remove_file(&path).ok();
    }

    /// Two pre-existing entries with real, distinct priorities — the shape
    /// needed to prove the Added path computes MAX+1 rather than reading an
    /// absent priority as 0.
    const PRIORITY_SAMPLE: &str = r#"{
      "accounts": [
        { "name": "acct-a", "type": "oauth", "accessToken": "at-a", "priority": 0 },
        { "name": "acct-b", "type": "oauth", "accessToken": "at-b", "priority": 1 }
      ]
    }"#;

    /// An appended account with no explicit priority joins the BACK of the
    /// fleet — `max(existing priorities) + 1` — never left absent. An absent
    /// `priority` reads as 0 at runtime (`AccountRuntime::from_config`'s
    /// `unwrap_or(0)`), which silently promotes a freshly added account to the
    /// PRIMARY tier ahead of the established fleet. Mirrors
    /// `login::upsert_account`'s historical default, which this route did not
    /// share before this fix.
    #[test]
    fn save_account_added_path_assigns_max_plus_one_priority_when_none_submitted() {
        let path = tmp_path("add-default-priority");
        fs::write(&path, PRIORITY_SAMPLE).unwrap();

        let new_account = Account {
            priority: None,
            ..full_account("carol@example.com", "at-carol")
        };
        assert_eq!(
            save_account(&path, &new_account).unwrap(),
            AccountWrite::Added
        );

        let after = read_json(&path);
        let accounts = after["accounts"].as_array().expect("accounts array");
        assert_eq!(accounts.len(), 3);
        assert_eq!(
            accounts[2]["priority"],
            json!(2),
            "an added account with no explicit priority must join the back of \
             the fleet, not read as 0: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// The max+1 default only fills an ABSENT priority — a caller that submits
    /// one explicitly (the documented new-account case, see
    /// `AddAccountRequest`'s doc-comment in `proxy.rs`) is never overridden.
    #[test]
    fn save_account_added_path_keeps_an_explicit_priority() {
        let path = tmp_path("add-explicit-priority");
        fs::write(&path, PRIORITY_SAMPLE).unwrap();

        let new_account = Account {
            priority: Some(99),
            ..full_account("carol@example.com", "at-carol")
        };
        assert_eq!(
            save_account(&path, &new_account).unwrap(),
            AccountWrite::Added
        );

        let after = read_json(&path);
        assert_eq!(after["accounts"][2]["priority"], json!(99));
        fs::remove_file(&path).ok();
    }

    /// A hit replaces ONLY the credential triple on the matching entry — name,
    /// type, and every unmodelled key (here `models`) survive untouched, exactly
    /// as `merge_tokens` leaves them.
    #[test]
    fn save_account_updates_an_existing_identity_in_place() {
        let path = tmp_path("add-update");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        let fresh = Account {
            expires_at: Some(999),
            priority: Some(99), // deliberately different — must NOT land on disk
            ..full_account("acct-a", "at-a-fresh")
        };
        assert_eq!(save_account(&path, &fresh).unwrap(), AccountWrite::Updated);

        let after = read_json(&path);
        let accounts = after["accounts"].as_array().expect("accounts array");
        assert_eq!(accounts.len(), 2, "no duplicate row");
        assert_eq!(accounts[0]["name"], json!("acct-a"), "identity untouched");
        assert_eq!(accounts[0]["accessToken"], json!("at-a-fresh"));
        assert_eq!(accounts[0]["refreshToken"], json!("rt-at-a-fresh"));
        assert_eq!(accounts[0]["expiresAt"], json!(999));
        assert!(
            accounts[0].get("priority").is_none(),
            "priority was never on this entry and the update must not add it: {after}"
        );
        assert_eq!(
            accounts[0]["models"],
            json!(["claude-fable-5"]),
            "an unmodelled per-account key survives the credential-only merge"
        );
        // The account we did NOT name keeps its own credentials.
        assert_eq!(accounts[1]["accessToken"], json!("at-b"));
        fs::remove_file(&path).ok();
    }

    /// An identity matching more than one on-disk entry refuses rather than
    /// guesses which one to overwrite — same posture as `save_disabled`.
    #[test]
    fn save_account_refuses_an_ambiguous_identity() {
        const TWINS: &str = r#"{
          "accounts": [
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-1" },
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-2" }
          ]
        }"#;
        let path = tmp_path("add-ambiguous");
        fs::write(&path, TWINS).unwrap();

        assert_eq!(
            save_account(&path, &full_account("dup@example.com", "at-new")).unwrap(),
            AccountWrite::Ambiguous
        );

        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["accessToken"], json!("at-1"));
        assert_eq!(after["accounts"][1]["accessToken"], json!("at-2"));
        fs::remove_file(&path).ok();
    }

    /// A miss on a document with NO `accounts` key at all still succeeds — the
    /// key is created — because (unlike `save_disabled`'s target) `account` here
    /// is always a complete, real record worth creating a home for.
    #[test]
    fn save_account_creates_the_accounts_array_when_absent() {
        let path = tmp_path("add-no-accounts-key");
        fs::write(&path, r#"{"warmupSeconds": 900}"#).unwrap();

        assert_eq!(
            save_account(&path, &full_account("carol@example.com", "at-carol")).unwrap(),
            AccountWrite::Added
        );

        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["name"], json!("carol@example.com"));
        assert_eq!(after["warmupSeconds"], json!(900));
        fs::remove_file(&path).ok();
    }

    /// An `accounts` key that is present but NOT an array is too corrupt a shape
    /// to append into blindly — refused, and the document is left byte-identical.
    #[test]
    fn save_account_refuses_when_accounts_key_is_not_an_array() {
        const MALFORMED: &str = r#"{"accounts": "not-an-array"}"#;
        let path = tmp_path("add-not-array");
        fs::write(&path, MALFORMED).unwrap();

        assert_eq!(
            save_account(&path, &full_account("carol@example.com", "at-carol")).unwrap(),
            AccountWrite::Unwritable
        );

        let after: Value = serde_json::from_str(MALFORMED).unwrap();
        assert_eq!(
            read_json(&path),
            after,
            "an unwritable shape is left untouched"
        );
        fs::remove_file(&path).ok();
    }
}
