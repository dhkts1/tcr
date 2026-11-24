//! Tests for the load and save seam in `cli/mod.rs`: quarantined accounts, a rotated
//! credential surviving an edit, and the empty-fleet hint.

use super::*;
const ONE_ACCOUNT_ONE_UNUSABLE: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a" },
        { "name": "carol@example.com", "type": "oauth", "importFrom": "/some/other/file.json" }
      ]
    }"#;

/// The server's own `config::load` stays lenient on this file — it drops
/// the unusable entry and keeps running. This is the control: proves the
/// fixture used by the refusal test below is exactly the kind of file the
/// fix was meant to keep the SERVER booting on.
#[test]
fn server_load_tolerates_a_quarantined_account() {
    let path = write_config("server-load-quarantine", ONE_ACCOUNT_ONE_UNUSABLE);
    let config = config::load(&path).expect("server load must not fail on this file");
    assert_eq!(config.accounts.len(), 1);
    assert_eq!(config.accounts[0].name, "alice@example.com");
    assert_eq!(
        config.quarantined_accounts,
        vec!["carol@example.com".to_string()]
    );
    fs::remove_file(&path).ok();
}

/// The CLI's load-edit-save round-trip cannot represent a quarantined
/// entry (`save` has nowhere to put it), so it refuses up front rather
/// than silently losing the entry's raw JSON on the next save.
#[test]
fn load_for_edit_refuses_a_config_with_a_quarantined_account() {
    let path = write_config("cli-load-quarantine", ONE_ACCOUNT_ONE_UNUSABLE);
    let err = load_for_edit(&path).expect_err("must refuse rather than silently drop an entry");
    let message = format!("{err:#}");
    assert!(
        message.contains("carol@example.com"),
        "error should name the quarantined account: {message}"
    );
    fs::remove_file(&path).ok();
}

/// A config with no quarantined accounts is unaffected: `load_for_edit`
/// still succeeds exactly as before this change.
#[test]
fn load_for_edit_succeeds_with_no_quarantined_accounts() {
    let mut fixture: serde_json::Value = serde_json::from_str(TWO_ACCOUNTS).unwrap();
    fixture["proxy"]["port"] = serde_json::json!(9);
    let path = write_config("cli-load-no-quarantine", &fixture.to_string());
    let config = load_for_edit(&path).expect("a clean config must still load for editing");
    assert_eq!(config.accounts.len(), 2);
    fs::remove_file(&path).ok();
}

// --- remove ------------------------------------------------------------
//
// Every one of these uses `config_on_a_dead_port`, never `TWO_ACCOUNTS`'
// literal 3456 directly — `remove_account` now dials the configured proxy
// port for real, and 3456 is where the live fleet proxy actually listens
// on this machine. Landing on the real port would mean this test suite
// silently disabling a real account.

/// `save_after_edit` must not revert a credential rotated since its config
/// was loaded.
///
/// THE WIRING TEST. `config`'s own
/// `a_settings_edit_does_not_revert_a_rotated_credential` calls
/// `save_settings` directly, so pointing `save_after_edit` back at
/// `config::save` leaves it green.
///
/// It calls `save_after_edit` rather than `edit_account` on purpose, and the
/// first attempt at this test got that wrong. `edit_account` runs
/// `load_for_edit` itself, so driving the whole chain reads the file AFTER
/// any rotation and the revert cannot happen; the test passed with the fix
/// removed. The exposure here is the window between `load_for_edit` and this
/// write, which is milliseconds rather than the seconds a stale server
/// snapshot holds. Narrow is not zero, a token refresh is exactly the thing
/// that lands in it, and the cost is an account dead until `tcr login`.
#[test]
fn save_after_edit_does_not_revert_a_rotated_credential() {
    let path = write_config(
        "save-after-edit-preserves",
        r#"{ "proxy": { "port": 9 }, "accounts": [
                   { "name": "acct-a", "accessToken": "at-a", "refreshToken": "rt-a", "expiresAt": 1 },
                   { "name": "acct-b", "accessToken": "at-b", "refreshToken": "rt-b", "expiresAt": 1 } ] }"#,
    );

    // What `load_for_edit` handed the verb.
    let mut cli = config::load(&path).expect("the fixture loads");

    // The server rotates acct-b inside the window, before the write below.
    let mut server = config::load(&path).expect("the fixture loads");
    server.accounts[1].access_token = "at-b-rotated".to_string();
    server.accounts[1].refresh_token = Some("rt-b-rotated".to_string());
    config::save_tokens_for(&path, &server, config::CredentialScope::Only(1))
        .expect("the rotation write");
    let rotated: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read back")).expect("valid json");
    assert_eq!(
        rotated["accounts"][1]["refreshToken"],
        serde_json::json!("rt-b-rotated"),
        "precondition: the rotation must land, or the assertion below proves nothing"
    );

    // The verb finishes and writes.
    cli.accounts[0].priority = Some(9);
    save_after_edit(&path, &cli).expect("the edit must save");

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read back")).expect("valid json");
    assert_eq!(
        value["accounts"][0]["priority"],
        serde_json::json!(9),
        "the verb's own edit must land"
    );
    assert_eq!(
        value["accounts"][1]["refreshToken"],
        serde_json::json!("rt-b-rotated"),
        "a priority edit on acct-a reverted acct-b's rotated refresh token; \
             that token is single-use and acct-b is now dead until `tcr login`"
    );

    fs::remove_file(&path).ok();
}

/// The empty-fleet line, as it always read, and nothing more without a codex login on
/// disk. A fleet with accounts prints no hint at all, codex login or not.
#[test]
fn warn_if_no_accounts_is_unchanged_without_codex_login() {
    assert_eq!(
        no_accounts_lines(&load_from(r#"{"accounts": []}"#), None),
        vec!["no accounts configured — run `tcr login` to add one"]
    );
    assert!(
        no_accounts_lines(
            &load_from(TWO_ACCOUNTS),
            Some(Path::new("/Users/example/.codex/auth.json"))
        )
        .is_empty()
    );
}

/// An empty fleet on a machine where codex holds a login gets one more line: the two
/// ways to pool codex, and the import's guarded token sharing. The login is named, never read.
#[test]
fn the_first_run_with_a_codex_login_prints_the_codex_hint() {
    assert_eq!(
        no_accounts_lines(
            &load_from(r#"{"accounts": []}"#),
            Some(Path::new("/Users/example/.codex/auth.json"))
        ),
        vec![
            "no accounts configured — run `tcr login` to add one".to_string(),
            "[tcr] found a codex login at /Users/example/.codex/auth.json: `tcr login \
                 --codex` adds a Codex account of tcr's own, and `tcr login --from-codex` \
                 imports that one with guarded token updates shared through auth.json"
                .to_string(),
        ]
    );
}
