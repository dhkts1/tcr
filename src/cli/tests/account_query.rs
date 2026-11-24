//! Tests for resolving an account query to one row, in `config/roster.rs`.

use super::*;
use crate::config::resolve_account;
#[test]
fn resolve_exact_name_wins() {
    // An exact display-name match resolves outright; a distinct name is not
    // matched by the email fallback.
    let config = load_from(
        r#"{ "accounts": [
                { "name": "user", "type": "oauth", "accessToken": "at1" },
                { "name": "user-two", "type": "oauth", "accessToken": "at2" }
            ] }"#,
    );
    let idx = resolve_account(&config.accounts, "user").unwrap();
    assert_eq!(idx, 0, "exact name resolves to that account");
}

/// Two accounts sharing the SAME email in different orgs — the shape the
/// whole unique-names contract exists for. `load_from` parses without
/// migrating (there is no file to write back to), so this is the one place
/// the pre-migration fleet can be looked at directly.
const DUP_EMAIL_TWO_ORGS: &str = r#"{
      "accounts": [
        { "name": "me@example.com", "type": "oauth", "accountUuid": "uuid-person",
          "orgName": "Org A", "orgUuid": "uuid-a", "accessToken": "at-a", "priority": 0 },
        { "name": "me@example.com", "type": "oauth", "accountUuid": "uuid-person",
          "orgName": "Org B", "orgUuid": "uuid-b", "accessToken": "at-b", "priority": 1 }
      ]
    }"#;

#[test]
fn resolve_a_duplicated_name_errors_listing_candidates() {
    let accounts = load_from(DUP_EMAIL_TWO_ORGS).accounts;
    let err = resolve_account(&accounts, "me@example.com").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ambiguous"), "ambiguous error: {msg}");
    assert!(
        msg.matches("me@example.com").count() >= 2,
        "error lists both candidates: {msg}"
    );
}

/// The migrated names each resolve to their own row, and nothing resolves
/// by the email PORTION of a qualified name — that fallback is what let one
/// query name two accounts.
#[test]
fn resolve_after_migration_gives_each_row_its_own_name() {
    let path = write_config("resolve-migrated", DUP_EMAIL_TWO_ORGS);
    let accounts = load(&path).accounts;
    assert_eq!(accounts[0].name, "me@example.com");
    assert_eq!(accounts[1].name, "me@example.com/org-b");

    assert_eq!(resolve_account(&accounts, "me@example.com").unwrap(), 0);
    assert_eq!(
        resolve_account(&accounts, "me@example.com/org-b").unwrap(),
        1
    );
    assert!(resolve_account(&accounts, "org-b").is_err());
}

// --- render_accounts ---------------------------------------------------
