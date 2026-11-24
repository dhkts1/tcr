//! Account-management subcommands for the `tcr` CLI.
//!
//! These are the non-server verbs — `accounts`, `remove`, `priority`,
//! `enable`, `disable`, `status` — that read or mutate the config
//! (`~/.config/tcr.json`) directly, so friends can curate the fleet without
//! hand-editing JSON.
//!
//! The seam copies [`crate::oauth::login`]: pure functions here `load` → mutate
//! a `&mut Config` → [`config::save`]; `main.rs` is a thin parse-and-delegate
//! layer. Every load/save wrapper WARNS to stderr when a proxy is already
//! listening on the config's port — a running server refreshes tokens and
//! flushes the config on shutdown, so an out-of-band edit races that writer and
//! can be silently clobbered.
//!
//! The read verbs (`accounts`, `status`) build an OFFLINE [`Manager`] with
//! `config_path = None`, so a probe's token refresh is NEVER persisted to disk —
//! reading quota must never mutate the user's file out from under a live server.

use std::path::Path;

use anyhow::{Context as _, bail};

use crate::config::{self, Config};

// What `mod tests` names through `use super::*` and the code left here no longer uses; these
// move with the tests when they leave this file.
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use time::OffsetDateTime;

#[cfg(test)]
use crate::build_info::{self, BuildInfo};
#[cfg(test)]
use crate::config::Account;
#[cfg(test)]
use crate::log_err::LogErr as _;
#[cfg(test)]
use crate::manager::Manager;
#[cfg(test)]
use crate::oauth::{NoRefresh, TokenRefresher};
#[cfg(test)]
use crate::probe::{ProbeStatus, UsageProber};
#[cfg(test)]
use crate::provider::Provider;
#[cfg(test)]
use crate::stats::{AccountSnapshot, CodexFacts, QuotaState, StatsSnapshot};
#[cfg(test)]
use crate::status::{STATUS_KIND, StatusPayload};

mod account_query;
mod accounts;
mod fan;
mod groups;
mod list_v1;
mod live_client;
mod live_edit;
mod live_reads;
mod render;
mod reset;
mod settings;
mod status;

pub use account_query::{ambiguous_query_message, resolve_account};
pub use accounts::{
    PriorityArg, print_access_token, remove_account, request_key, set_account_egress, set_control,
    set_enabled, set_priority, show_control,
};
pub use fan::{FAN_SETTLE, fan};
pub use groups::{
    add_to_group, allow_control_group, disallow_control_group, list_groups, park_group,
    remove_from_group, reserve_group, reserve_group_with, set_group_color, unpark_group,
    unreserve_group, validate_group_label_chars,
};
pub use list_v1::list_v1;
pub use live_client::{
    IncumbentProbe, LiveStatusError, Liveness, RESTART_HINT, fetch_live_status, probe_incumbent,
};
pub(crate) use live_client::{LiveControlError, post_add_account, post_add_account_with_providers};
pub use live_reads::{live_peers, routing, sessions};
pub use render::{StatusSource, list_accounts, render_accounts, snapshot_offline};
pub use reset::{codex_reset, reset};
pub use settings::{config_show_document, set_config, show_config};
pub use status::{StatusCheck, doctor, status};

// The private items `mod tests` and `mod routing_tests` reach through `use super::*`.
#[cfg(test)]
use accounts::{priority_message, priority_value, write_control_account};
#[cfg(test)]
use fan::{FanSent, FanWait, accepted_command, fan_succeeded, fan_wait, fan_word, render_fan_text};
#[cfg(test)]
use groups::{
    ROUTE_BLOCK_CONTROL_ONLY, ReservePlan, codex_group_add_notice, control_account_group_notice,
    low_contrast_color_warning, plan_reserve, render_groups_json, render_groups_text,
};
#[cfg(test)]
use list_v1::{overlay_live_rows, render_list_v1};
#[cfg(test)]
use live_client::{Unreached, parse_live_status, send_loopback};
#[cfg(test)]
use live_edit::{
    CONFIG_SET_WORDS, CONTROL_WORDS, ENABLE_WORDS, Live, LiveSite, LiveWords, PRIORITY_WORDS,
    REMOVE_WORDS, Stale,
};
#[cfg(test)]
use live_reads::{
    RoutingReadError, fetch_routing_body, payload_carries_sessions, render_sessions_json,
    render_sessions_text,
};
#[cfg(test)]
use render::{
    CACHE_SIGNAL_FLOOR_REQUESTS, account_status_rows, account_status_rows_at, claude_resets_token,
    codex_tail, format_reset_clock, format_reset_countdown, held_suffix, render_accounts_json,
    resolve_thresholds, status_rows_json,
};
#[cfg(test)]
use status::{
    StatusText, build_fields, codex_traffic_notes, codex_wire_facts, eligible_token,
    holds_provider, relogin_hint_lines, render_status_text, skew_report, status_holds_provider,
};

/// Is a proxy already listening on `port`? A cheap loopback connect probe —
/// the same gate `tcr run` uses to decide whether to route through us.
pub fn proxy_is_up(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// Warn (once) when a server is live on the config's port, since our write
/// races its shutdown flush. Best-effort — never blocks the edit.
fn warn_if_server_running(config: &Config) {
    if proxy_is_up(config.proxy.port) {
        eprintln!(
            "[tcr] warning: a proxy is already listening on :{} — it may overwrite this edit when it flushes the config on exit. Stop the server first, or expect to re-apply.",
            config.proxy.port
        );
    }
}

/// Every CLI verb's read of the user's config file: [`config::load_or_init`],
/// so a first run on a box with no `~/.config/tcr.json` creates the file
/// and continues instead of exiting 1 with `No such file or directory`. A
/// missing config is not an error condition for any verb here — `tcr status` on
/// a fresh install is the FIRST thing a new user runs, and it is what TcrBar's
/// panel polls.
///
/// One helper rather than the `load` + `with_context` pair repeated at a dozen
/// call sites: the next verb that needs the config gets the first-run behaviour
/// by construction, not by remembering.
fn load_config(config_path: &Path) -> anyhow::Result<Config> {
    load_config_with_creation(config_path).map(|(config, _)| config)
}

fn load_config_with_creation(config_path: &Path) -> anyhow::Result<(Config, bool)> {
    config::load_or_init(config_path)
        .with_context(|| format!("load config at {}", config_path.display()))
}

/// A first run's one chance to arrive with an account already in it: when the
/// fleet is empty, import the login this machine's `claude` CLI already holds.
///
/// Takes the just-loaded `config` and hands back the one the verb should go on
/// to render — re-read from disk after a successful import, so the verb shows
/// the account it just wrote rather than the empty fleet it loaded a moment
/// ago. Everything about "is there a login, did the write work" lives in
/// [`crate::oauth::auto_import_claude_code_login`]; what lives HERE is the one
/// rule that keeps it idempotent: a config with accounts in it is never
/// touched, so an account removed on purpose stays removed.
///
/// Best-effort in both directions: no login found, or an import that failed,
/// leaves `config` exactly as it came in and the caller prints the ordinary
/// empty-fleet hint.
async fn import_claude_code_login_if_empty(config_path: &Path, config: Config) -> Config {
    // A fleet a delete emptied stays empty: see `Config::refuses_claude_code_import`.
    if !config.accounts.is_empty() || config.refuses_claude_code_import() {
        return config;
    }
    if crate::oauth::auto_import_claude_code_login(config_path)
        .await
        .is_none()
    {
        return config;
    }
    // Re-read rather than reconstruct: the import may have gone through the
    // running proxy's add route, and the config on disk is the only place that
    // knows what actually landed.
    load_config(config_path).unwrap_or(config)
}

/// Print the first-run hint when the fleet is empty, on STDERR.
///
/// STDERR deliberately: `tcr status --json` and `tcr accounts --json` are piped
/// into `jq`, and TcrBar decodes the stdout of `tcr status --json` — an empty
/// fleet must stay a well-formed empty fleet there, with the human-facing
/// nudge on the other channel. The verb still exits 0: no accounts is a state,
/// not a failure.
///
/// Only an empty fleet asks whether codex holds a login of its own
/// ([`crate::codex_local::codex_login_exists`], a `metadata` call that never
/// opens the file), so a fleet with accounts costs nothing extra.
fn warn_if_no_accounts(config: &Config) {
    let codex_login = (config.accounts.is_empty() && crate::codex_local::codex_login_exists())
        .then(crate::codex_local::codex_login_path);
    for line in no_accounts_lines(config, codex_login.as_deref()) {
        eprintln!("{line}");
    }
}

/// The lines [`warn_if_no_accounts`] prints: none for a fleet with accounts, else the
/// empty-fleet sentence, then, when `codex_login` names the login codex keeps on this
/// machine, the two ways to pool codex.
///
/// The codex line fires on evidence only. A machine with no codex login is a Claude-only
/// first run and reads exactly as it always did; a machine where codex is signed in is one
/// whose person uses codex, and should hear that tcr can pool it. tcr never imports that login
/// on its own: sharing a login permits guarded token updates to its auth file, so it is the
/// person's choice. `claudeCodeImport: false` does not stop
/// the line either, since it stops only the Claude Code import.
fn no_accounts_lines(config: &Config, codex_login: Option<&Path>) -> Vec<String> {
    if !config.accounts.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["no accounts configured — run `tcr login` to add one".to_string()];
    if let Some(login) = codex_login {
        lines.push(format!(
            "[tcr] found a codex login at {}: `tcr login --codex` adds a Codex account of tcr's \
             own, and `tcr login --from-codex` imports that one with guarded token updates \
             shared through auth.json",
            login.display()
        ));
    }
    lines
}

/// Load the config for a management verb, warning if a server is live.
///
/// Refuses when [`Config::quarantined_accounts`] is non-empty. The server's
/// `load` is deliberately lenient — it drops an `accounts[]` entry with no
/// usable credential (e.g. an `importFrom`-shaped account) rather than take
/// the whole fleet down. A CLI edit verb cannot share that leniency: its
/// round-trip is `save_after_edit` → a bare `serde_json::to_string_pretty`
/// of the typed `Config`, which has nowhere to put an entry that was never
/// deserialized into it — so writing back after loading such a file would
/// silently and permanently erase the quarantined entry's raw JSON (its
/// `importFrom` pointer included) the first time any verb saved. Refusing
/// here, before any edit happens, keeps that erasure from ever becoming
/// reachable; the operator fixes or removes the entry by hand, then the CLI
/// works on it same as always.
///
/// Refuses on [`Config::rename_write_error`] for the analogous reason. `load`
/// renames duplicated account names in memory and persists the result; when only
/// the memory half happened, the names this process is about to edit exist
/// nowhere else, and the next process re-derives its own. An edit verb writing
/// under those names would attach the change to a row no one else can find.
/// The server has no such choice — it must boot — so it logs and continues.
fn load_for_edit(config_path: &Path) -> anyhow::Result<Config> {
    let config = load_config(config_path)?;
    if let Some(why) = &config.rename_write_error {
        bail!(
            "config at {} holds accounts sharing a name, and the rename that would fix it could \
             not be written back ({why}) — so nothing was changed. Fix the file's permissions, or \
             give each row its own name by hand, and retry.",
            config_path.display()
        );
    }
    if !config.quarantined_accounts.is_empty() {
        bail!(
            "config at {} has {} account(s) with no usable credential, so the CLI cannot safely \
             load-edit-save it: {}. Fix or remove the entry in the file by hand (the server \
             itself tolerates it and keeps running).",
            config_path.display(),
            config.quarantined_accounts.len(),
            config.quarantined_accounts.join(", ")
        );
    }
    warn_if_server_running(&config);
    Ok(config)
}

/// Save the config after a management verb, warning if a server is live.
fn save_after_edit(config_path: &Path, config: &Config) -> anyhow::Result<()> {
    warn_if_server_running(config);
    // `save_settings`, never `save`: no verb reaching here changes a credential,
    // and this config was loaded before the edit. Writing its credentials whole
    // reverts any rotation the server landed in between, and a single-use
    // refresh token does not survive that. The warning above covers losing YOUR
    // edit; it cannot cover losing somebody else's token.
    config::save_settings(config_path, config)
        .with_context(|| format!("save config at {}", config_path.display()))
}

/// Load → resolve `query` to one account → hand it to `mutate` → save.
///
/// Centralizes the load→resolve→save chain (incl. the nonexistent-account error
/// path) shared by every mutating verb. Resolution happens BEFORE `mutate`, so a
/// non-matching query returns an error with the file left byte-identical (no
/// partial write). `mutate` receives the resolved index and the whole config, so
/// a command that needs the fleet (e.g. relative priority) or must `remove` the
/// entry can still do so; it returns whatever the caller wants to report.
fn edit_account<T>(
    config_path: &Path,
    query: &str,
    mutate: impl FnOnce(&mut Config, usize) -> T,
) -> anyhow::Result<T> {
    let mut config = load_for_edit(config_path)?;
    let idx = resolve_account(&config.accounts, query)?;
    let out = mutate(&mut config, idx);
    save_after_edit(config_path, &config)?;
    Ok(out)
}

/// `tcr wrap [--days N] [--json]` — a weekly usage report read straight off
/// the usage ledger, offline and whether or not a proxy is running. All the
/// actual work — reading the ledger, aggregating, rendering — lives in
/// [`crate::wrap`]; this is the CLI's entry point into it.
pub fn wrap(config_path: &Path, days: u32, json: bool) -> anyhow::Result<()> {
    crate::wrap::print_report(&crate::usage::default_dir(), config_path, days, json)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_restart_hint_names_the_first_version_with_handoff() {
        assert!(RESTART_HINT.contains("stop it"));
        assert!(RESTART_HINT.contains(&format!(
            "if it runs tcr {} or newer",
            crate::restart::HANDOFF_SINCE
        )));
    }
    use super::*;
    use crate::oauth::{OAuthError, RefreshFuture};
    use crate::probe::{ProbeFuture, Usage, UsageBucket};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// `tcr request-key` prints the key the file holds, and with none it fails with a line
    /// that says the server writes one, and writes nothing itself.
    #[test]
    fn request_key_prints_the_saved_key_and_never_mints_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let keyed = dir.path().join("keyed.json");
        fs::write(
            &keyed,
            r#"{"proxy": {"requestKey": "4444444444444444444444444444444444444444444444444444444444444444"}, "accounts": []}"#,
        )
        .expect("writes");
        assert_eq!(
            request_key(&keyed).expect("a saved key prints"),
            "4444444444444444444444444444444444444444444444444444444444444444"
        );

        for body in [r#"{"accounts": []}"#, r#"{"proxy": {"requestKey": ""}}"#] {
            let bare = dir.path().join("bare.json");
            fs::write(&bare, body).expect("writes");
            let err = request_key(&bare).expect_err("no key is an error");
            assert!(
                err.to_string().contains("writes one there when it starts"),
                "{err}"
            );
            assert_eq!(
                fs::read_to_string(&bare).expect("reads"),
                body,
                "the CLI never writes a key"
            );
        }
    }

    /// `snapshot` as `tcr status --json` renders it from `source`, with no server build, no
    /// `http1Only`, no control account and no group colours.
    fn status_json(snapshot: &StatsSnapshot, thresholds: &[f64], source: StatusSource) -> String {
        render_accounts_json(
            snapshot,
            thresholds,
            source,
            None,
            false,
            None,
            &Default::default(),
        )
    }

    /// The row named `name` in the `tcr status --json` text `json`.
    fn status_row(json: &str, name: &str) -> serde_json::Value {
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(json).expect("json is a bare array");
        rows.into_iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no row named {name} in {json}"))
    }

    /// A unique temp config path per call so parallel tests never collide.
    fn temp_config_path(tag: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tcr-cli-test-{tag}-{}-{seq}.json",
            std::process::id()
        ))
    }

    /// Write `json` to a fresh temp path and return it.
    fn write_config(tag: &str, json: &str) -> std::path::PathBuf {
        let path = temp_config_path(tag);
        fs::write(&path, json).unwrap();
        path
    }

    const TWO_ACCOUNTS: &str = r#"{
      "proxy": { "port": 3456 },
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "orgName": "Org A",
          "orgUuid": "uuid-a", "accessToken": "at-a", "refreshToken": "rt-a",
          "expiresAt": 1893456000000, "priority": 0 },
        { "name": "bob@example.com", "type": "oauth", "orgName": "Org B",
          "orgUuid": "uuid-b", "accessToken": "at-b", "refreshToken": "rt-b",
          "expiresAt": 1893456000000, "priority": 1 }
      ]
    }"#;

    fn load(path: &Path) -> Config {
        config::load(path).unwrap()
    }

    // --- load_for_edit refuses a config with a quarantined account ---------

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

    #[tokio::test]
    async fn remove_deletes_named_account_leaving_siblings() {
        let path = config_on_a_dead_port("remove").await;
        remove_account(&path, "alice@example.com").await.unwrap();
        let config = load(&path);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "bob@example.com");
        fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn remove_preserves_unmodelled_extra_fields() {
        let path = config_on_a_dead_port("remove-extra").await;
        remove_account(&path, "alice@example.com").await.unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // The #1 silent-loss risk: unmodelled top-level keys survive the edit.
        assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
        assert!(value["routes"].is_array());
        fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn remove_nonexistent_errors_and_leaves_file_byte_identical() {
        let path = config_on_a_dead_port("remove-miss").await;
        let before = fs::read_to_string(&path).unwrap();
        let result = remove_account(&path, "nobody@example.com").await;
        assert!(result.is_err());
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a failed resolve must not write the config");
        fs::remove_file(&path).ok();
    }

    /// THE BITING TEST for remove, end to end through the PRODUCTION path: a
    /// real hybrid listener, a real socket, the real router. Pre-change
    /// `remove_account` only ever touched the file, so the running manager's
    /// rotation kept serving the deleted account's identity until restart —
    /// the same defect `set_enabled_parks_the_account_in_the_running_proxy`
    /// exists to catch, one level more destructive here because the record is
    /// also gone.
    #[tokio::test]
    async fn remove_parks_the_account_in_the_running_proxy_before_deleting() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let path = config_on_a_dead_port("live-remove").await;
        let mut config = load(&path);
        config.proxy.port = port;

        let manager =
            crate::test_support::live_proxy::serve(listener, config.clone(), Some(path.clone()));

        point_at_port(&path, port);

        remove_account(&path, "alice@example.com").await.unwrap();

        // 1. THE RUNNING ROTATION: out of it at once, not at next boot. It is
        // no longer listed (the snapshot re-reads the file first), and no pick
        // lands on it even though it has the better priority.
        let live = manager.snapshot(OffsetDateTime::now_utc());
        let listed: Vec<&str> = live.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            listed,
            vec!["bob@example.com"],
            "the serving process dropped it"
        );
        let picked = manager.select(
            &std::collections::HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            "/v1/messages",
            None,
        );
        assert_eq!(picked, Some(1), "the deleted account never serves again");

        // 2. …and the file no longer carries the record at all.
        let config = load(&path);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "bob@example.com");
        fs::remove_file(&path).ok();
    }

    /// **Deleting a Codex account takes out that row and no other**, in the file and in
    /// the running proxy, though an Anthropic row shares its identity. The two rows below
    /// carry the same `accountUuid` and no org, the shape in which identity alone calls
    /// them one account; `config::remove_account_entry` finds the entry it deletes, and
    /// the proxy's reload finds the runtime row the file lost, both by identity. Only the
    /// provider keeps either from landing on the Anthropic row.
    ///
    /// Watched red: drop the provider guard from `identity::same_identity`, and the
    /// delete is refused as ambiguous and changes nothing.
    #[tokio::test]
    async fn remove_takes_a_codex_account_out_and_leaves_its_anthropic_twin() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let path = write_config(
            "live-remove-codex",
            &format!(
                r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                     {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                        "refreshToken": "rt-a", "expiresAt": 4102444800000, "priority": 0,
                        "accountUuid": "11111111-1111-4111-8111-111111111111" }},
                     {{ "name": "alice@example.com/codex", "provider": "codex", "type": "oauth",
                        "accessToken": "at-c", "refreshToken": "rt-c", "expiresAt": 4102444800000,
                        "priority": 0, "accountUuid": "11111111-1111-4111-8111-111111111111",
                        "credentialVersion": 2 }}
                   ] }}"#
            ),
        );
        let manager =
            crate::test_support::live_proxy::serve(listener, load(&path), Some(path.clone()));

        remove_account(&path, "alice@example.com/codex")
            .await
            .expect("the Codex row is deleted");

        let live = manager.snapshot(OffsetDateTime::now_utc());
        let listed: Vec<(&str, Provider)> = live
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), a.provider))
            .collect();
        assert_eq!(
            listed,
            vec![("alice@example.com", Provider::Anthropic)],
            "the serving process dropped the Codex row and only it"
        );
        let pick = |path: &str| {
            manager.select(
                &std::collections::HashSet::new(),
                OffsetDateTime::now_utc(),
                None,
                None,
                path,
                None,
            )
        };
        assert_eq!(
            pick(crate::provider::CODEX_RESPONSES_PATH),
            None,
            "the deleted Codex account never serves again"
        );
        assert_eq!(
            pick("/v1/messages"),
            Some(0),
            "the Anthropic twin still serves"
        );

        let config = load(&path);
        let on_disk: Vec<(&str, Provider, &str)> = config
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), a.provider, a.access_token.as_str()))
            .collect();
        assert_eq!(
            on_disk,
            vec![("alice@example.com", Provider::Anthropic, "at-a")],
            "the file lost the Codex row and kept the Anthropic one, credential and all"
        );
        fs::remove_file(&path).ok();
    }

    /// The proxy rejected our api-key: the one arm that must NOT write. If
    /// this arm wrote instead, the config would say the account is gone while
    /// the running proxy — the one we couldn't even authenticate to, let
    /// alone confirm parked the account — keeps rotating it, permanently.
    #[tokio::test]
    async fn remove_refuses_on_unauthorized_without_writing() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let server_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }}, "accounts": [] }}"#
        ))
        .unwrap();
        crate::test_support::live_proxy::serve(listener, server_config, None);

        let path = config_on_a_dead_port("remove-unauth").await;
        let raw = fs::read_to_string(&path).unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        doc["proxy"]["port"] = serde_json::json!(port);
        doc["proxy"]["apiKey"] = serde_json::json!("wrong-key");
        fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        let err = remove_account(&path, "alice@example.com")
            .await
            .expect_err("a rejected api-key must refuse the removal");
        assert!(
            err.to_string().contains("NOT changed"),
            "the refusal must say nothing changed: {err}"
        );
        assert_eq!(
            before,
            fs::read_to_string(&path).unwrap(),
            "an Unauthorized removal must leave the config byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    /// An ambiguous query is refused by the SERVER and the CLI does not fall
    /// back to the file's own resolution — a removal applied to the wrong row
    /// is unrecoverable, unlike a mis-applied disable.
    #[tokio::test]
    async fn remove_refuses_an_ambiguous_query_without_writing() {
        let (path, listener) = config_on_a_scratch_port("remove-ambiguous").await;

        // The ambiguity is in the LIVE rotation only: the file keeps unique
        // names (the loader guarantees that), and the running fleet is broken by
        // hand afterwards. That is the split this test is about — the CLI must
        // not resolve against the file when the server refused.
        let before = serve_a_fleet_with_two_alices(&path, listener);

        let err = remove_account(&path, "alice@example.com")
            .await
            .expect_err("an ambiguous query must not be applied");
        let text = err.to_string();
        assert!(
            text.contains("ambiguous") && text.matches("alice@example.com").count() >= 2,
            "the refusal names the candidates: {text}"
        );
        assert_eq!(
            before,
            fs::read_to_string(&path).unwrap(),
            "a refused removal leaves the config byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    // --- priority ----------------------------------------------------------
    //
    // `config_on_a_dead_port`, never `TWO_ACCOUNTS`' literal 3456: `set_priority`
    // dials the configured port for real, and 3456 is the live fleet proxy.

    #[tokio::test]
    async fn set_priority_explicit_writes_int() {
        let path = config_on_a_dead_port("prio-n").await;
        set_priority(&path, "bob@example.com", PriorityArg::N(7))
            .await
            .unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[1].priority, Some(7));
        fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn set_priority_first_is_min0_minus_one() {
        // Existing priorities are 0 and 1 → min(0, {0,1}) - 1 = -1.
        let path = config_on_a_dead_port("prio-first").await;
        set_priority(&path, "bob@example.com", PriorityArg::First)
            .await
            .unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[1].priority, Some(-1));
        fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn set_priority_last_is_max0_plus_one() {
        // Existing priorities are 0 and 1 → max(0, {0,1}) + 1 = 2.
        let path = config_on_a_dead_port("prio-last").await;
        set_priority(&path, "alice@example.com", PriorityArg::Last)
            .await
            .unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[0].priority, Some(2));
        fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn set_priority_nonexistent_errors_and_leaves_file_byte_identical() {
        let path = config_on_a_dead_port("prio-miss").await;
        let before = fs::read_to_string(&path).unwrap();
        let result = set_priority(&path, "nobody@example.com", PriorityArg::N(3)).await;
        assert!(result.is_err());
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
        fs::remove_file(&path).ok();
    }

    /// `tcr priority bob --first` against a REAL proxy on an ephemeral port: the
    /// serving process routes the next fresh session to bob, and the file
    /// carries the new value too. Before the live route, only the file moved and
    /// the running rotation kept picking alice until a restart.
    #[tokio::test]
    async fn set_priority_reorders_the_running_proxy_and_the_file() {
        let (path, listener) = config_on_a_scratch_port("live-priority").await;

        let manager =
            crate::test_support::live_proxy::serve(listener, load(&path), Some(path.clone()));

        let fresh_pick = |m: &Manager| {
            m.select(
                &std::collections::HashSet::new(),
                OffsetDateTime::now_utc(),
                None,
                None,
                "/v1/messages",
                None,
            )
        };
        assert_eq!(fresh_pick(&manager), Some(0), "alice leads at boot");

        set_priority(&path, "bob@example.com", PriorityArg::First)
            .await
            .unwrap();

        assert_eq!(
            fresh_pick(&manager),
            Some(1),
            "the SERVING process now routes a fresh session to bob"
        );
        assert_eq!(
            load(&path).accounts[1].priority,
            Some(-1),
            "and the file agrees"
        );
        fs::remove_file(&path).ok();
    }

    /// A refused query (the live rotation holds two rows of one name) changes
    /// neither the file nor the running proxy's order.
    #[tokio::test]
    async fn set_priority_refuses_an_ambiguous_live_query_without_writing() {
        let (path, listener) = config_on_a_scratch_port("live-priority-ambiguous").await;

        let before = serve_a_fleet_with_two_alices(&path, listener);

        let err = set_priority(&path, "alice@example.com", PriorityArg::First)
            .await
            .expect_err("an ambiguous live query must not be applied");
        assert!(err.to_string().contains("ambiguous"), "{err}");
        assert_eq!(before, fs::read_to_string(&path).unwrap());
        fs::remove_file(&path).ok();
    }

    // --- config set / config show running -----------------------------------
    //
    // Every test dials a port it bound itself (a scratch proxy) or one nothing
    // listens on (`config_on_a_dead_port`): never 3456, the live proxy.

    /// A fixture whose port is a scratch listener's, bound here and handed back
    /// for the test to serve on.
    async fn config_on_a_scratch_port(tag: &str) -> (std::path::PathBuf, tokio::net::TcpListener) {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let path = config_on_a_dead_port(tag).await;
        point_at_port(&path, port);
        (path, listener)
    }

    /// Serve the config at `path` on `listener` with its second account renamed to
    /// `alice@example.com`, so the RUNNING fleet holds two alices while the file keeps unique
    /// names (the loader guarantees that). Answers the file's text from before the proxy started.
    fn serve_a_fleet_with_two_alices(path: &Path, listener: tokio::net::TcpListener) -> String {
        let mut config = load(path);
        config.accounts[1].name = "alice@example.com".to_string();
        let before = fs::read_to_string(path).unwrap();
        crate::test_support::live_proxy::serve(listener, config, Some(path.to_path_buf()));
        before
    }

    /// Rewrite the config file at `path` so its `proxy.port` is `port`, the way a CLI pointed at a
    /// test's live proxy finds it.
    fn point_at_port(path: &Path, port: u16) {
        let raw = fs::read_to_string(path).unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        doc["proxy"]["port"] = serde_json::json!(port);
        fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }

    fn file_value(path: &Path, key: &str) -> serde_json::Value {
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        doc.get(key).cloned().unwrap_or(serde_json::Value::Null)
    }

    /// With a proxy running, `config set` goes THROUGH it: the proxy writes the
    /// key into its own config file, the running value stays the boot one, and
    /// the proxy's next write of its own (a `tcr disable` through its route)
    /// keeps the key.
    ///
    /// The proxy serves a SECOND copy of the file (`served`), so a direct write
    /// by the CLI would land in `cli_path` and leave `served` alone: the value
    /// appearing in `served` and nowhere else is the proof the route did it.
    #[tokio::test]
    async fn config_set_goes_through_a_running_proxy_and_survives_its_next_write() {
        let (cli_path, listener) = config_on_a_scratch_port("config-set-live").await;
        let served = cli_path.with_extension("served.json");
        fs::copy(&cli_path, &served).unwrap();
        let cli_before = fs::read_to_string(&cli_path).unwrap();
        let manager =
            crate::test_support::live_proxy::serve(listener, load(&served), Some(served.clone()));

        set_config(&cli_path, "switchThreshold", "90%")
            .await
            .unwrap();

        assert_eq!(
            file_value(&served, "switchThreshold"),
            serde_json::json!(0.9)
        );
        assert_eq!(
            fs::read_to_string(&cli_path).unwrap(),
            cli_before,
            "the CLI wrote nothing itself"
        );
        assert_eq!(
            manager.running_settings().switch_threshold,
            0.95,
            "the running proxy keeps its boot value"
        );

        // The proxy's own next write: park bob through its route.
        set_enabled(&served, "bob@example.com", true).await.unwrap();
        assert_eq!(
            file_value(&served, "switchThreshold"),
            serde_json::json!(0.9),
            "the proxy's own write kept the saved setting"
        );
        assert_eq!(
            file_value(&served, "accounts")[1]["disabled"],
            serde_json::json!(true),
            "and that write did happen"
        );
        fs::remove_file(&cli_path).ok();
        fs::remove_file(&served).ok();
    }

    /// With nothing listening, `config set` writes the file itself.
    #[tokio::test]
    async fn config_set_without_a_proxy_writes_the_file() {
        let path = config_on_a_dead_port("config-set-offline").await;
        set_config(&path, "accountThrottle", "200 8").await.unwrap();
        set_config(&path, "pacing", "4").await.unwrap();
        assert_eq!(
            file_value(&path, "accountThrottle"),
            serde_json::json!({"minSpacingMs": 200, "burst": 8})
        );
        assert_eq!(
            file_value(&path, "pacing"),
            serde_json::json!({"maxInFlightPerAccount": 4})
        );
        assert_eq!(
            file_value(&path, "quotaProbeSeconds"),
            serde_json::json!(120)
        );
        fs::remove_file(&path).ok();
    }

    /// A refused value writes nothing, with or without a proxy, and a refusal
    /// from the proxy's route (here: a proxy with no config file to save to)
    /// does not fall back to writing the file.
    #[tokio::test]
    async fn each_refused_config_value_writes_nothing() {
        let offline = config_on_a_dead_port("config-set-refused").await;
        let (live, listener) = config_on_a_scratch_port("config-set-refused-live").await;
        // A proxy with no config file behind it: its route refuses every set.
        crate::test_support::live_proxy::serve(listener, load(&live), None);

        for path in [&offline, &live] {
            let before = fs::read_to_string(path).unwrap();
            for (key, value) in [
                ("switchThreshold", "0"),
                ("controlReserve", "0.6"),
                ("accountThrottle", "350 0"),
                ("pacing", "0"),
                ("http1Only", "maybe"),
                ("controlAccount", "alice@example.com"),
            ] {
                let err = set_config(path, key, value)
                    .await
                    .expect_err(&format!("{key} {value} is refused"));
                assert!(err.to_string().contains("not"), "{key} {value}: {err}");
                assert_eq!(before, fs::read_to_string(path).unwrap(), "{key} {value}");
            }
        }

        let before = fs::read_to_string(&live).unwrap();
        let err = set_config(&live, "switchThreshold", "0.9")
            .await
            .expect_err("the route refused it");
        assert!(err.to_string().contains("without a config file"), "{err}");
        assert_eq!(before, fs::read_to_string(&live).unwrap());
        fs::remove_file(&offline).ok();
        fs::remove_file(&live).ok();
    }

    /// `running` is absent with no proxy; with one, it is the proxy's boot
    /// value even after a set, and the set key is the one waiting.
    #[tokio::test]
    async fn config_show_reports_running_only_with_a_proxy() {
        let offline = config_on_a_dead_port("config-show-offline").await;
        let shown = config_show_document(&offline).await.unwrap();
        assert!(shown.running.is_none());
        assert!(shown.waiting().is_empty());

        let (path, listener) = config_on_a_scratch_port("config-show-live").await;
        crate::test_support::live_proxy::serve(listener, load(&path), Some(path.clone()));

        let shown = config_show_document(&path).await.unwrap();
        assert_eq!(shown.waiting(), Vec::<&str>::new(), "nothing waits at boot");

        set_config(&path, "switchThreshold", "0.9").await.unwrap();
        set_config(&path, "http1Only", "on").await.unwrap();
        let shown = config_show_document(&path).await.unwrap();
        assert_eq!(shown.saved.switch_threshold, 0.9);
        let running = shown.running.as_ref().expect("a proxy answered");
        assert_eq!(running.switch_threshold, 0.95, "the boot value");
        assert!(!running.http1_only);
        assert_eq!(shown.waiting(), ["switchThreshold", "http1Only"]);
        fs::remove_file(&offline).ok();
        fs::remove_file(&path).ok();
    }

    /// Two Claude and two Codex accounts with these priorities, parsed in memory:
    /// no file, and nothing to dial.
    fn priority_fleet(alice: i64, carol: i64, bob: i64, dana: i64) -> Config {
        serde_json::from_value(serde_json::json!({
            "accounts": [
                { "name": "alice@example.com", "accessToken": "at-a", "priority": alice },
                { "name": "carol@example.com", "accessToken": "at-c", "priority": carol },
                { "name": "bob@example.com/codex", "provider": "codex", "accessToken": "at-b",
                  "priority": bob },
                { "name": "dana@example.com/codex", "provider": "codex", "accessToken": "at-d",
                  "priority": dana }
            ]
        }))
        .unwrap()
    }

    /// `--first` and `--last` take the min and max over the row's OWN provider,
    /// with the same `0` seed: Claude and Codex accounts rotate apart. A Codex
    /// row's relative move names the end of the Codex rotation it now holds; an
    /// explicit value, and any Claude move, prints the line it always printed.
    #[test]
    fn priority_first_and_last_stay_within_the_rows_provider() {
        let fleet = priority_fleet(1, -1, 4, -4);
        assert_eq!(
            priority_value(&fleet, Provider::Anthropic, PriorityArg::First),
            -2,
            "a min over both providers would give -5"
        );
        assert_eq!(
            priority_value(&fleet, Provider::Anthropic, PriorityArg::Last),
            2,
            "a max over both providers would give 5"
        );

        let fleet = priority_fleet(6, -6, 2, -2);
        assert_eq!(
            priority_value(&fleet, Provider::Codex, PriorityArg::First),
            -3
        );
        assert_eq!(
            priority_value(&fleet, Provider::Codex, PriorityArg::Last),
            3
        );

        assert_eq!(
            priority_message(
                "bob@example.com/codex",
                -3,
                Provider::Codex,
                PriorityArg::First
            ),
            "Set priority of 'bob@example.com/codex' to -3, the front of the Codex rotation."
        );
        assert_eq!(
            priority_message(
                "bob@example.com/codex",
                3,
                Provider::Codex,
                PriorityArg::Last
            ),
            "Set priority of 'bob@example.com/codex' to 3, the back of the Codex rotation."
        );
        assert_eq!(
            priority_message(
                "bob@example.com/codex",
                5,
                Provider::Codex,
                PriorityArg::N(5)
            ),
            "Set priority of 'bob@example.com/codex' to 5."
        );
        assert_eq!(
            priority_message(
                "alice@example.com",
                -2,
                Provider::Anthropic,
                PriorityArg::First
            ),
            "Set priority of 'alice@example.com' to -2."
        );
    }

    /// `tcr priority <codex row> --first` writes the value worked out over the
    /// Codex rows: `set_priority` hands `priority_value` the row's provider, which
    /// the pure test above cannot see. On a port nothing serves, so the live half
    /// finds no proxy and the file is the whole truth.
    #[tokio::test]
    async fn priority_on_a_codex_row_writes_its_own_providers_value() {
        let port = crate::test_support::loopback::unused_port();
        let json = serde_json::json!({
            "proxy": { "port": port },
            "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "priority": 6 },
                { "name": "carol@example.com", "type": "oauth", "accessToken": "at-c",
                  "priority": -6 },
                { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
                  "accessToken": "at-b", "credentialVersion": 2, "priority": 2 },
                { "name": "dana@example.com/codex", "provider": "codex", "type": "oauth",
                  "accessToken": "at-d", "credentialVersion": 2, "priority": -2 }
            ]
        })
        .to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["proxy"]["port"], serde_json::json!(port));
        assert_ne!(
            parsed["proxy"]["port"],
            serde_json::json!(3456),
            "never the port a live proxy serves on"
        );
        let path = write_config("priority-codex", &json);

        set_priority(&path, "bob@example.com/codex", PriorityArg::First)
            .await
            .expect("no proxy answers, so the file is written");
        assert_eq!(
            load(&path).accounts[2].priority,
            Some(-3),
            "min(0, the Codex rows' 2 and -2) - 1; over every row it would be -7"
        );
        fs::remove_file(&path).ok();
    }

    // --- group ---------------------------------------------------------

    /// Three accounts: alice in two groups, bob in one (shared with alice),
    /// carol in none — enough to cover multi-group membership, a shared
    /// group, and an ungrouped account in one fixture. Carries `routes` (an
    /// unmodelled top-level key) and per-account `models` (an unmodelled
    /// per-account key) so a mutation's flatten round-trip can be checked
    /// without a second fixture.
    const GROUPED_ACCOUNTS: &str = r#"{
      "proxy": { "port": 3456 },
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "orgName": "Org A",
          "orgUuid": "uuid-a", "accessToken": "at-a", "refreshToken": "rt-a",
          "expiresAt": 1893456000000, "priority": 0,
          "groups": ["codereview", "dev"], "models": ["opus"] },
        { "name": "bob@example.com", "type": "oauth", "orgName": "Org B",
          "orgUuid": "uuid-b", "accessToken": "at-b", "refreshToken": "rt-b",
          "expiresAt": 1893456000000, "priority": 1,
          "groups": ["codereview"] },
        { "name": "carol@example.com", "type": "oauth", "orgName": "Org C",
          "orgUuid": "uuid-c", "accessToken": "at-c", "refreshToken": "rt-c",
          "expiresAt": 1893456000000, "priority": 2 }
      ]
    }"#;

    /// The fleet's real shape: one email logged into two orgs. `tcr group add`
    /// used to take the FIRST match and report success, so the panel labelled a
    /// row nobody asked for and had no way to notice.
    const DUPLICATED_EMAIL: &str = r#"{
      "accounts": [
        { "name": "henry@example.com", "type": "oauth", "orgName": "Personal",
          "orgUuid": "11111111-1111-1111-1111-111111111111",
          "accountUuid": "uuid-person", "accessToken": "at-p",
          "refreshToken": "rt-p", "expiresAt": 1893456000000, "priority": 0 },
        { "name": "henry@example.com", "type": "oauth", "orgName": "Example Team",
          "orgUuid": "22222222-2222-2222-2222-222222222222",
          "accountUuid": "uuid-person", "accessToken": "at-t",
          "refreshToken": "rt-t", "expiresAt": 1893456000000, "priority": 1 }
      ]
    }"#;

    /// The bare email names ONE row after the loader's migration — the Personal
    /// one, which keeps it — and the Team row is addressed by the name that was
    /// minted for it. The assertion is on the OTHER row every time: labelling
    /// the right account is only half of it, and a resolver that labelled both
    /// would pass a check that only looked at the one it was asked for.
    #[test]
    fn group_add_labels_exactly_the_named_row_of_a_migrated_duplicate() {
        let path = write_config("group-add-migrated", DUPLICATED_EMAIL);
        add_to_group(&path, "codereview", "henry@example.com/example-team").unwrap();

        let config = load(&path);
        assert_eq!(config.accounts[0].name, "henry@example.com");
        assert_eq!(config.accounts[1].name, "henry@example.com/example-team");
        assert_eq!(
            config.accounts[0].groups, None,
            "the Personal row was not asked for and must not be labelled"
        );
        assert_eq!(
            config.accounts[1].groups,
            Some(vec!["codereview".to_string()]),
            "the Team row is the one the name pointed at"
        );

        add_to_group(&path, "burst", "henry@example.com").unwrap();
        let config = load(&path);
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["burst".to_string()]),
            "the bare email is the Personal row and nothing else"
        );
        assert_eq!(
            config.accounts[1].groups,
            Some(vec!["codereview".to_string()]),
            "the Team row keeps only its own label"
        );
    }

    /// A duplicate put back by hand is MIGRATED AGAIN on the next load, not
    /// refused and not guessed at. That is what makes uniqueness a property of
    /// the system rather than of one upgrade: every verb goes through
    /// `config::load`, so nothing downstream ever sees two rows of one name.
    ///
    /// The original regression this replaces: an exact-name scan silently took
    /// `accounts[0]`, so the operator saw "Added ... to group" about the wrong
    /// account. Here the label lands on the row the name resolves to, and the
    /// re-minted name is on disk for the next reader.
    #[test]
    fn a_hand_edited_duplicate_is_migrated_again_on_the_next_load() {
        let path = write_config("group-add-rebroken", DUPLICATED_EMAIL);
        assert_eq!(
            load(&path).accounts[1].name,
            "henry@example.com/example-team"
        );
        rename_account_in_file(&path, 1, "henry@example.com");

        add_to_group(&path, "codereview", "henry@example.com").unwrap();

        let config = load(&path);
        assert_eq!(config.accounts[0].name, "henry@example.com");
        assert_eq!(
            config.accounts[1].name, "henry@example.com/example-team",
            "the re-broken row is renamed again"
        );
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["codereview".to_string()]),
            "the label landed on the row the bare email names"
        );
        assert_eq!(config.accounts[1].groups, None);
    }

    /// `rm` resolves the same way — the panel's "remove from group" is the same
    /// menu as "add to group" and must not be able to strip the wrong row.
    #[test]
    fn group_rm_strips_exactly_the_named_row_of_a_migrated_duplicate() {
        let path = write_config("group-rm-migrated", DUPLICATED_EMAIL);
        let team = "henry@example.com/example-team";
        add_to_group(&path, "codereview", team).unwrap();
        add_to_group(&path, "codereview", "henry@example.com").unwrap();

        remove_from_group(&path, "codereview", Some("henry@example.com"), false).unwrap();
        let config = load(&path);
        // Removing the last label DROPS the `groups` key rather than leaving an
        // empty array — `save_group_membership`'s contract, same shape as
        // `disabled == false` removing its key.
        assert_eq!(config.accounts[0].groups, None);
        assert_eq!(
            config.accounts[1].groups,
            Some(vec!["codereview".to_string()]),
            "the row nobody named keeps its label"
        );

        remove_from_group(&path, "codereview", Some(team), false).unwrap();
        assert_eq!(load(&path).accounts[1].groups, None);
    }

    /// Rewrite one `accounts[]` entry's `name` in the file directly, bypassing
    /// `config::load`. The only way to reach a duplicate name now, and therefore
    /// the only way to test the refusals that guard one.
    fn rename_account_in_file(path: &Path, index: usize, name: &str) {
        let mut doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).expect("readable"))
                .expect("the fixture is JSON");
        doc["accounts"][index]["name"] = serde_json::Value::String(name.to_string());
        fs::write(
            path,
            serde_json::to_string_pretty(&doc).expect("serializes"),
        )
        .expect("writable");
    }

    /// An unknown name still comes back with the whole roster, which is what
    /// the panel needs to act on — it shells out blind. A plain "no account
    /// matches" would be a worse error in exactly the place it is read.
    #[test]
    fn group_add_names_every_configured_account_when_the_name_is_unknown() {
        let path = write_config("group-add-unknown", TWO_ACCOUNTS);
        let err = add_to_group(&path, "codereview", "nobody@example.com")
            .expect_err("an unknown account is an error");
        let text = err.to_string();
        assert!(text.contains("alice@example.com"), "{text}");
        assert!(text.contains("bob@example.com"), "{text}");
    }

    #[test]
    fn group_add_labels_named_account_and_leaves_siblings_byte_identical() {
        let path = write_config("group-add", TWO_ACCOUNTS);
        add_to_group(&path, "codereview", "alice@example.com").unwrap();
        let config = load(&path);
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["codereview".to_string()])
        );
        // bob is untouched — same identity fields, still no `groups` key.
        assert_eq!(config.accounts[1].name, "bob@example.com");
        assert_eq!(config.accounts[1].groups, None);
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            value["accounts"][1].get("groups").is_none(),
            "bob's raw JSON entry must not gain a groups key: {value}"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_add_twice_is_idempotent() {
        let path = write_config("group-add-twice", TWO_ACCOUNTS);
        add_to_group(&path, "codereview", "alice@example.com").unwrap();
        add_to_group(&path, "codereview", "alice@example.com").unwrap();
        let config = load(&path);
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["codereview".to_string()]),
            "adding the same label twice must not duplicate it"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_rm_removes_only_named_label_leaving_others_intact() {
        let path = write_config("group-rm-one", GROUPED_ACCOUNTS);
        remove_from_group(&path, "codereview", Some("alice@example.com"), false).unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[0].groups, Some(vec!["dev".to_string()]));
        // bob still carries codereview — only alice's membership changed.
        assert_eq!(
            config.accounts[1].groups,
            Some(vec!["codereview".to_string()])
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_rm_all_clears_label_from_every_member_and_nothing_else() {
        let path = write_config("group-rm-all", GROUPED_ACCOUNTS);
        remove_from_group(&path, "codereview", None, true).unwrap();
        let config = load(&path);
        // alice keeps `dev`, loses `codereview`.
        assert_eq!(config.accounts[0].groups, Some(vec!["dev".to_string()]));
        // bob had only `codereview` — the key drops entirely (not `[]`).
        assert_eq!(config.accounts[1].groups, None);
        // carol was never in it and stays untouched.
        assert_eq!(config.accounts[2].groups, None);
        // No other field moved — priorities and identities intact.
        assert_eq!(config.accounts[0].priority, Some(0));
        assert_eq!(config.accounts[1].priority, Some(1));
        assert_eq!(config.accounts[2].priority, Some(2));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_rm_all_on_a_group_with_no_members_is_a_success_no_op() {
        let path = write_config("group-rm-all-empty", GROUPED_ACCOUNTS);
        let before = fs::read_to_string(&path).unwrap();
        remove_from_group(&path, "nonexistent-group", None, true).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an --all on an empty group must write nothing"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_add_unknown_account_errors_and_names_the_configured_accounts() {
        let path = write_config("group-add-unknown", GROUPED_ACCOUNTS);
        let err = add_to_group(&path, "codereview", "nobody@example.com").unwrap_err();
        let message = err.to_string();
        for name in ["alice@example.com", "bob@example.com", "carol@example.com"] {
            assert!(
                message.contains(name),
                "unknown-account error must name every configured account, missing {name}: {message}"
            );
        }
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_rm_unknown_account_errors_and_names_the_configured_accounts() {
        let path = write_config("group-rm-unknown", GROUPED_ACCOUNTS);
        let err =
            remove_from_group(&path, "codereview", Some("nobody@example.com"), false).unwrap_err();
        let message = err.to_string();
        for name in ["alice@example.com", "bob@example.com", "carol@example.com"] {
            assert!(message.contains(name), "missing {name}: {message}");
        }
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_add_rejects_bad_label_but_rm_still_accepts_it() {
        // A control character somehow already in the config (hand-edited, or
        // written by an older/buggy version) — `rm` must be able to strip it.
        let with_bad_label = r#"{
          "proxy": { "port": 3456 },
          "accounts": [
            { "name": "alice@example.com", "type": "oauth",
              "accessToken": "at-a", "priority": 0,
              "groups": ["good", "bad\u0000label"] }
          ]
        }"#;
        let bad_label = "bad\u{0}label";

        let path = write_config("group-add-bad-label", with_bad_label);
        let err = add_to_group(&path, bad_label, "alice@example.com").unwrap_err();
        assert!(
            err.to_string().contains("control character"),
            "add must name the character class at fault: {err}"
        );
        // Nothing was written by the rejected `add`.
        let config = load(&path);
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["good".to_string(), bad_label.to_string()])
        );

        // `rm` does NOT run the validator — it must still remove the bad label.
        remove_from_group(&path, bad_label, Some("alice@example.com"), false).unwrap();
        let config = load(&path);
        assert_eq!(config.accounts[0].groups, Some(vec!["good".to_string()]));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_mutation_preserves_unmodelled_extra_keys() {
        let path = write_config("group-extra", GROUPED_ACCOUNTS);
        add_to_group(&path, "burst", "carol@example.com").unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Top-level unmodelled key.
        assert!(
            value["routes"].is_array(),
            "top-level `routes` must survive: {value}"
        );
        assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
        // Per-account unmodelled key on an entry NOT even touched by this edit.
        assert_eq!(value["accounts"][0]["models"], serde_json::json!(["opus"]));
        fs::remove_file(&path).ok();
    }

    // --- group reserve / unreserve (the safety rail) -----------------------

    /// Every account belongs to `codereview` and none are disabled — reserving
    /// it would leave 0 unreserved enabled accounts, so this fixture is the
    /// one the safety rail's hard refusal must fire on.
    const ALL_IN_ONE_GROUP: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
          "priority": 0, "groups": ["codereview"] },
        { "name": "bob@example.com", "type": "oauth", "accessToken": "at-b",
          "priority": 1, "groups": ["codereview"] }
      ]
    }"#;

    /// Bridge test #6: `reserve` refuses when it would leave zero unreserved
    /// enabled accounts, and the config is BYTE-IDENTICAL after the refusal —
    /// the safety rail runs its count-before-write, not a write-then-rollback.
    #[test]
    fn reserve_refuses_when_it_would_leave_zero_unreserved_enabled_accounts() {
        let path = write_config("reserve-zero", ALL_IN_ONE_GROUP);
        let before = fs::read_to_string(&path).unwrap();
        let result = reserve_group(&path, "codereview");
        assert!(
            result.is_err(),
            "reserving the fleet's only group must be refused"
        );
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "a refused reservation must not touch the file at all"
        );
        let config = load(&path);
        assert!(
            !config.is_group_reserved("codereview"),
            "config in memory must also show the group as still unreserved"
        );
        fs::remove_file(&path).ok();
    }

    /// A disabled account does not count toward the pool either direction —
    /// reserving a group that leaves only a DISABLED account unreserved is
    /// refused exactly like leaving none at all, and a group with a healthy
    /// unreserved sibling succeeds and reports the right remaining count.
    #[test]
    fn reserve_counts_only_enabled_accounts_in_the_remaining_pool() {
        let path = write_config(
            "reserve-disabled-sibling",
            r#"{
              "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "priority": 0, "groups": ["codereview"] },
                { "name": "bob@example.com", "type": "oauth", "accessToken": "at-b",
                  "priority": 1, "disabled": true }
              ]
            }"#,
        );
        let result = reserve_group(&path, "codereview");
        assert!(
            result.is_err(),
            "the only OTHER account is disabled — reserving must still refuse"
        );
        fs::remove_file(&path).ok();
    }

    /// The success path: a group with headroom reserves cleanly, persists,
    /// and survives a restart-style reload; unreserve clears it again. Idempotent
    /// both ways.
    #[test]
    fn reserve_then_unreserve_round_trips_and_is_idempotent() {
        let path = write_config("reserve-roundtrip", GROUPED_ACCOUNTS);
        reserve_group(&path, "codereview").unwrap();
        let config = load(&path);
        assert!(config.is_group_reserved("codereview"));
        assert!(
            !config.is_group_reserved("dev"),
            "only the named group changes"
        );

        // Idempotent: reserving an already-reserved group succeeds.
        reserve_group(&path, "codereview").unwrap();
        assert!(load(&path).is_group_reserved("codereview"));

        unreserve_group(&path, "codereview").unwrap();
        let config = load(&path);
        assert!(!config.is_group_reserved("codereview"));

        // Idempotent the other way too.
        unreserve_group(&path, "codereview").unwrap();
        assert!(!load(&path).is_group_reserved("codereview"));

        fs::remove_file(&path).ok();
    }

    /// `tcr group reserve` rejects the same bad-character labels `tcr group
    /// add` already does — reuses [`validate_group_label_chars`], no second
    /// validator.
    #[test]
    fn reserve_rejects_bad_label() {
        let path = write_config("reserve-bad-label", TWO_ACCOUNTS);
        let result = reserve_group(&path, "bad\nlabel");
        assert!(result.is_err(), "a newline in the label must be rejected");
        fs::remove_file(&path).ok();
    }

    // --- group reserve in a fleet holding Codex accounts ----------------------

    /// alice is the only Claude account, and she is in `orchard`; bob and dana
    /// are enabled Codex accounts in no group.
    const ONE_CLAUDE_TWO_CODEX: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
          "groups": ["orchard"] },
        { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
          "accessToken": "at-b", "credentialVersion": 2 },
        { "name": "dana@example.com/codex", "provider": "codex", "type": "oauth",
          "accessToken": "at-d", "credentialVersion": 2 }
      ]
    }"#;

    /// `orchard` holds alice (Claude) and bob (Codex); carol (Claude) and dana
    /// (Codex) are in no group.
    const ORCHARD_HOLDS_CODEX: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
          "groups": ["orchard"] },
        { "name": "carol@example.com", "type": "oauth", "accessToken": "at-c" },
        { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
          "accessToken": "at-b", "credentialVersion": 2, "groups": ["orchard"] },
        { "name": "dana@example.com/codex", "provider": "codex", "type": "oauth",
          "accessToken": "at-d", "credentialVersion": 2 }
      ]
    }"#;

    /// The rail counts Claude accounts: reserving the group that holds the only
    /// Claude account leaves two Codex accounts unreserved, and they cannot serve
    /// a Claude session, so it is refused. `--force` does not lift it, and the
    /// refusal leaves the file byte-identical.
    #[test]
    fn reserving_the_last_claude_account_counts_claude_only() {
        let refusal = "reserving 'orchard' would leave 0 unreserved enabled Claude accounts — Claude could serve no unrequested traffic, and the 2 Codex account(s) left cannot serve it. every enabled Claude account belongs to this group. Nothing changed.";
        let path = write_config("reserve-last-claude", ONE_CLAUDE_TWO_CODEX);
        let config = load(&path);
        assert_eq!(
            plan_reserve(&config, "orchard", false),
            Err(refusal.to_string())
        );
        assert_eq!(
            plan_reserve(&config, "orchard", true),
            Err(refusal.to_string()),
            "--force lifts only the Codex-members refusal, never the Claude rail"
        );

        let before = fs::read(&path).unwrap();
        let err = reserve_group_with(&path, "orchard", true).expect_err("the rail refuses");
        assert_eq!(err.to_string(), refusal);
        assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");
        fs::remove_file(&path).ok();
    }

    /// A group holding a Codex account is refused without `--force`, because a
    /// Codex session cannot ask for a group: while it is reserved, its Codex
    /// members serve nothing. The remedy names the first of them.
    #[test]
    fn reserving_a_group_with_codex_members_needs_force() {
        let refusal = "reserving 'orchard' strands its Codex account(s) bob@example.com/codex: a Codex session cannot ask for a group, so while 'orchard' is reserved they serve nothing. Remove them first (`tcr group rm orchard bob@example.com/codex`), or pass --force to reserve it anyway. Nothing changed.";
        let path = write_config("reserve-codex-members", ORCHARD_HOLDS_CODEX);
        assert_eq!(
            plan_reserve(&load(&path), "orchard", false),
            Err(refusal.to_string())
        );

        let before = fs::read(&path).unwrap();
        let err = reserve_group_with(&path, "orchard", false).expect_err("refused without --force");
        assert_eq!(err.to_string(), refusal);
        assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");
        fs::remove_file(&path).ok();
    }

    /// `--force` reserves it and says, per provider, what is left: one Claude
    /// account and one Codex account stay unreserved, and one Claude account is
    /// few enough to warn about. The already-reserved line ends with the same
    /// clause, and the Codex-members refusal still applies to a group that is
    /// already reserved.
    #[test]
    fn force_reserve_warns_and_counts_per_provider() {
        let path = write_config("reserve-force", ORCHARD_HOLDS_CODEX);
        let plan = plan_reserve(&load(&path), "orchard", true).expect("--force reserves it");
        assert_eq!(
            plan.force_warning,
            Some("warning: 'orchard' holds the Codex account(s) bob@example.com/codex. A Codex session cannot ask for a group, so while 'orchard' is reserved they serve nothing — `tcr group rm orchard bob@example.com/codex` returns one to the pool.".to_string())
        );
        assert_eq!(
            plan.reserved_line,
            "group 'orchard' reserved. 1 unreserved enabled Claude account(s) and 1 Codex account(s) remain."
        );
        assert_eq!(
            plan.already_line,
            "group 'orchard' is already reserved; nothing changed. 1 unreserved enabled Claude account(s) and 1 Codex account(s) remain."
        );
        assert_eq!(
            plan.low_count_warning,
            Some("warning: only 1 unreserved enabled Claude account(s) remain — ordinary (unrequested) Claude traffic may starve.".to_string())
        );

        reserve_group_with(&path, "orchard", true).expect("--force writes the reservation");
        assert!(load(&path).is_group_reserved("orchard"));

        let before = fs::read(&path).unwrap();
        assert!(
            reserve_group_with(&path, "orchard", false).is_err(),
            "an already-reserved group holding a Codex account still needs --force"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");
        reserve_group_with(&path, "orchard", true).expect("and with it, it is already reserved");
        fs::remove_file(&path).ok();
    }

    /// A fleet with no Codex account keeps every word the reserve verb printed
    /// before Codex accounts existed, and `--force` changes nothing in it.
    #[test]
    fn a_claude_only_fleets_reserve_sentence_is_unchanged() {
        let zero_path = write_config("reserve-claude-only-zero", ALL_IN_ONE_GROUP);
        let all_in_one = load(&zero_path);
        let refusal = "reserving 'codereview' would leave 0 unreserved enabled accounts — the fleet could serve no unrequested traffic. every enabled account belongs to this group. Nothing changed.";
        assert_eq!(
            plan_reserve(&all_in_one, "codereview", false),
            Err(refusal.to_string())
        );
        assert_eq!(
            plan_reserve(&all_in_one, "codereview", true),
            Err(refusal.to_string())
        );

        let grouped_path = write_config("reserve-claude-only", GROUPED_ACCOUNTS);
        let grouped = load(&grouped_path);
        let plan = plan_reserve(&grouped, "codereview", false).expect("carol stays unreserved");
        assert_eq!(
            plan,
            ReservePlan {
                force_warning: None,
                reserved_line: "group 'codereview' reserved. 1 unreserved enabled account(s) remain."
                    .to_string(),
                already_line: "group 'codereview' is already reserved; nothing changed. 1 unreserved enabled account(s) remain."
                    .to_string(),
                low_count_warning: Some(
                    "warning: only 1 unreserved enabled account(s) remain — ordinary (unrequested) traffic may starve."
                        .to_string()
                ),
            }
        );
        assert_eq!(plan_reserve(&grouped, "codereview", true), Ok(plan));
        fs::remove_file(&zero_path).ok();
        fs::remove_file(&grouped_path).ok();
    }

    // --- group park / unpark ------------------------------------------------

    /// Mirrors [`reserve_then_unreserve_round_trips_and_is_idempotent`]:
    /// parking persists, is scoped to the named group, survives a reload, and
    /// both directions are idempotent. Unlike `reserve` there is no
    /// pool-floor rail to trip, so the success path is the whole story.
    #[test]
    fn park_then_unpark_round_trips_and_is_idempotent() {
        let path = write_config("park-roundtrip", GROUPED_ACCOUNTS);
        park_group(&path, "codereview").unwrap();
        let config = load(&path);
        assert!(config.is_group_parked("codereview"));
        assert!(
            !config.is_group_parked("dev"),
            "only the named group changes"
        );

        // Idempotent: parking an already-parked group succeeds.
        park_group(&path, "codereview").unwrap();
        assert!(load(&path).is_group_parked("codereview"));

        unpark_group(&path, "codereview").unwrap();
        assert!(!load(&path).is_group_parked("codereview"));

        // Idempotent the other way too.
        unpark_group(&path, "codereview").unwrap();
        assert!(!load(&path).is_group_parked("codereview"));

        fs::remove_file(&path).ok();
    }

    /// The two flags COMPOSE and neither write disturbs the other: a group can
    /// be parked and reserved at once, and clearing one leaves the other
    /// standing. Both were decided up front as independent, so this is the test
    /// that keeps them that way.
    #[test]
    fn park_and_reserve_are_independent_flags() {
        let path = write_config("park-and-reserve", GROUPED_ACCOUNTS);
        reserve_group(&path, "codereview").unwrap();
        park_group(&path, "codereview").unwrap();
        let config = load(&path);
        assert!(config.is_group_reserved("codereview"));
        assert!(config.is_group_parked("codereview"));

        unpark_group(&path, "codereview").unwrap();
        let config = load(&path);
        assert!(
            config.is_group_reserved("codereview"),
            "unparking must not clear the reservation"
        );
        assert!(!config.is_group_parked("codereview"));

        park_group(&path, "codereview").unwrap();
        unreserve_group(&path, "codereview").unwrap();
        let config = load(&path);
        assert!(
            config.is_group_parked("codereview"),
            "unreserving must not clear the park"
        );
        assert!(!config.is_group_reserved("codereview"));

        fs::remove_file(&path).ok();
    }

    /// Unparking is not a re-enable: a member the operator disabled by hand is
    /// still disabled afterwards. The two facts live in different places in the
    /// file (an account's `disabled`, a group's `parked`), and this pins that
    /// `unpark`'s targeted write never reaches across.
    #[test]
    fn unparking_leaves_a_members_own_disabled_alone() {
        let path = write_config("park-keeps-disabled", GROUPED_ACCOUNTS);
        let alice = load(&path)
            .accounts
            .into_iter()
            .find(|a| a.name == "alice@example.com")
            .expect("alice is in the fixture");
        config::save_disabled(&path, &alice, true).expect("bench alice by hand");
        park_group(&path, "codereview").unwrap();
        unpark_group(&path, "codereview").unwrap();

        let config = load(&path);
        let alice = config
            .accounts
            .iter()
            .find(|a| a.name == "alice@example.com")
            .expect("alice survives the round trip");
        assert_eq!(
            alice.disabled,
            Some(true),
            "a member disabled on its own stays disabled after unpark"
        );
        fs::remove_file(&path).ok();
    }

    /// `tcr group park` rejects the same bad-character labels `tcr group
    /// reserve`/`add` already do — reuses [`validate_group_label_chars`], no
    /// second validator. `unpark` deliberately does NOT validate, so a label
    /// that somehow reached the file can always be cleared.
    #[test]
    fn park_rejects_bad_label_and_unpark_does_not() {
        let path = write_config("park-bad-label", TWO_ACCOUNTS);
        assert!(
            park_group(&path, "bad\nlabel").is_err(),
            "a newline in the label must be rejected"
        );
        assert!(
            unpark_group(&path, "bad\nlabel").is_ok(),
            "unpark must be able to clear a label park would refuse to write"
        );
        fs::remove_file(&path).ok();
    }

    /// Both `ls` surfaces carry the flag, and they agree.
    #[test]
    fn ls_reports_a_parked_group_on_both_surfaces() {
        let path = write_config("ls-parked", GROUPED_ACCOUNTS);
        park_group(&path, "codereview").unwrap();
        let config = load(&path);

        let codereview = group_line(&render_groups_text(&config), "codereview");
        assert!(
            codereview.contains("parked=yes"),
            "text ls must mark the parked group: {codereview}"
        );
        let dev = group_line(&render_groups_text(&config), "dev");
        assert!(
            dev.contains("parked=no"),
            "an ordinary group must not be flagged parked: {dev}"
        );

        let json: serde_json::Value =
            serde_json::from_str(&render_groups_json(&config).unwrap()).unwrap();
        let rows = json["groups"].as_array().expect("groups array");
        let row = rows
            .iter()
            .find(|r| r["group"] == "codereview")
            .expect("codereview row");
        assert_eq!(row["parked"], serde_json::json!(true));
        let plain = rows.iter().find(|r| r["group"] == "dev").expect("dev row");
        assert_eq!(plain["parked"], serde_json::json!(false));

        fs::remove_file(&path).ok();
    }

    // --- allow-control / disallow-control -----------------------------------

    /// Mirrors [`reserve_then_unreserve_round_trips_and_is_idempotent`]: opting
    /// in persists, is scoped to the named group, survives a reload, and both
    /// directions are idempotent.
    #[test]
    fn allow_then_disallow_control_round_trips_and_is_idempotent() {
        let path = write_config("allow-control-roundtrip", GROUPED_ACCOUNTS);
        allow_control_group(&path, "codereview").unwrap();
        let config = load(&path);
        assert!(config.group_allows_control("codereview"));
        assert!(
            !config.group_allows_control("dev"),
            "only the named group changes"
        );

        // Idempotent: opting in an already-opted-in group succeeds.
        allow_control_group(&path, "codereview").unwrap();
        assert!(load(&path).group_allows_control("codereview"));

        disallow_control_group(&path, "codereview").unwrap();
        let config = load(&path);
        assert!(!config.group_allows_control("codereview"));

        // Idempotent the other way too.
        disallow_control_group(&path, "codereview").unwrap();
        assert!(!load(&path).group_allows_control("codereview"));

        fs::remove_file(&path).ok();
    }

    /// `tcr group allow-control` rejects the same bad-character labels `tcr
    /// group reserve`/`add` already do — reuses [`validate_group_label_chars`].
    #[test]
    fn allow_control_rejects_bad_label() {
        let path = write_config("allow-control-bad-label", TWO_ACCOUNTS);
        let result = allow_control_group(&path, "bad\nlabel");
        assert!(result.is_err(), "a newline in the label must be rejected");
        fs::remove_file(&path).ok();
    }

    /// `ls` must show a control-only group as routable once it has opted in —
    /// the whole point of the flag, on the surface an operator actually reads.
    #[test]
    fn ls_marks_an_opted_in_control_only_group_as_routing() {
        let path = write_config("routes-control-allowed", CONTROL_ONLY_GROUP);
        allow_control_group(&path, "research").unwrap();
        let config = load(&path);
        let research = group_line(&render_groups_text(&config), "research");
        assert!(
            research.contains("allows_control=yes")
                && research.contains("routes=yes")
                && !research.contains("route_block="),
            "an opted-in control-only group must report routable: {research}"
        );
        let dev = group_line(&render_groups_text(&config), "dev");
        assert!(
            dev.contains("allows_control=no"),
            "an ordinary group must not be flagged as opted in: {dev}"
        );
        fs::remove_file(&path).ok();
    }

    /// Same fact on the JSON surface.
    #[test]
    fn ls_json_carries_allows_control() {
        let path = write_config("routes-control-allowed-json", CONTROL_ONLY_GROUP);
        allow_control_group(&path, "research").unwrap();
        let config = load(&path);
        let value: serde_json::Value =
            serde_json::from_str(&render_groups_json(&config).unwrap()).unwrap();
        let row = value["groups"]
            .as_array()
            .expect("groups array")
            .iter()
            .find(|r| r["group"] == "research")
            .expect("research row");
        assert_eq!(row["allowsControl"], serde_json::json!(true));
        assert_eq!(row["routes"], serde_json::json!(true));
        assert_eq!(row["routeBlock"], serde_json::Value::Null);
        fs::remove_file(&path).ok();
    }

    // --- unroutable groups ---------------------------------------------------

    /// A fleet where the control account is the ONLY member of a group. This is
    /// the shape that shipped on the live fleet and served nothing: inference
    /// never picks the control account, so `research` could not route, while
    /// `tcr group ls` reported a healthy one-member group.
    const CONTROL_ONLY_GROUP: &str = r#"{
      "proxy": { "port": 3456 },
      "controlAccount": "gil@example.com",
      "accounts": [
        { "name": "gil@example.com", "type": "oauth", "accessToken": "at-g",
          "expiresAt": 1893456000000, "priority": 0, "groups": ["research"] },
        { "name": "worker@example.com", "type": "oauth", "accessToken": "at-w",
          "expiresAt": 1893456000000, "priority": 1, "groups": ["dev"] },
        { "name": "worker2@example.com", "type": "oauth", "accessToken": "at-w2",
          "expiresAt": 1893456000000, "priority": 2, "groups": ["dev"] }
      ]
    }"#;

    fn group_line(text: &str, group: &str) -> String {
        text.lines()
            .find(|l| {
                l.starts_with(&format!("group {group} "))
                    || l.starts_with(&format!("group {group}"))
            })
            .unwrap_or_else(|| panic!("no line for group {group} in:\n{text}"))
            .to_string()
    }

    /// `ls` must distinguish a group that cannot route from one that can — the
    /// whole point of the field. `dev` (two ordinary accounts) routes; `research`
    /// (control account only) does not, and says why.
    #[test]
    fn ls_marks_a_control_only_group_as_not_routing() {
        let path = write_config("routes-control-only", CONTROL_ONLY_GROUP);
        let config = load(&path);
        let research = group_line(&render_groups_text(&config), "research");
        assert!(
            research.contains("routes=no") && research.contains("route_block=control-account-only"),
            "a control-account-only group must say it routes nothing: {research}"
        );
        let dev = group_line(&render_groups_text(&config), "dev");
        assert!(
            dev.contains("routes=yes") && !dev.contains("route_block="),
            "an ordinary group must not be flagged: {dev}"
        );
        fs::remove_file(&path).ok();
    }

    /// The remedy the warning tells an operator to apply must actually clear the
    /// flag — otherwise the message sends them somewhere that does not help.
    #[test]
    fn a_second_member_makes_the_group_route_again() {
        let path = write_config("routes-second-member", CONTROL_ONLY_GROUP);
        add_to_group(&path, "research", "worker@example.com").unwrap();
        let config = load(&path);
        let research = group_line(&render_groups_text(&config), "research");
        assert!(
            research.contains("routes=yes") && !research.contains("route_block="),
            "adding a non-control account must make the group routable: {research}"
        );
        fs::remove_file(&path).ok();
    }

    /// The row for group `name` in `tcr group ls --json` over `config`.
    fn group_row(config: &Config, name: &str) -> serde_json::Value {
        let value: serde_json::Value =
            serde_json::from_str(&render_groups_json(config).unwrap()).unwrap();
        value["groups"]
            .as_array()
            .expect("groups array")
            .iter()
            .find(|r| r["group"] == name)
            .expect("group row")
            .clone()
    }

    /// Same fact on the JSON surface, which the panel and scripts read.
    #[test]
    fn ls_json_carries_routes_and_route_block() {
        let path = write_config("routes-json", CONTROL_ONLY_GROUP);
        let config = load(&path);
        let row = |name: &str| group_row(&config, name);
        assert_eq!(row("research")["routes"], serde_json::json!(false));
        assert_eq!(
            row("research")["routeBlock"],
            serde_json::json!(ROUTE_BLOCK_CONTROL_ONLY),
            "the JSON reason must be the same token the text surface prints"
        );
        assert_eq!(row("dev")["routes"], serde_json::json!(true));
        assert_eq!(row("dev")["routeBlock"], serde_json::Value::Null);
        fs::remove_file(&path).ok();
    }

    /// The warning `group add` prints. It fires only for the control account,
    /// and it names both remedies — otherwise it is just an alarm with no exit.
    #[test]
    fn labelling_the_control_account_warns_and_names_a_remedy() {
        let path = write_config("routes-notice", CONTROL_ONLY_GROUP);
        let config = load(&path);
        let notice = control_account_group_notice(&config, "gil@example.com", "research")
            .expect("labelling the control account must produce a notice");
        assert!(notice.contains("control account"), "{notice}");
        assert!(
            notice.contains("research"),
            "the notice names the group: {notice}"
        );
        assert!(
            notice.contains("tcr control --clear"),
            "the notice names a runnable remedy: {notice}"
        );
        assert!(
            control_account_group_notice(&config, "worker@example.com", "dev").is_none(),
            "an ordinary account must not be warned about"
        );
        fs::remove_file(&path).ok();
    }

    /// The offline half of `tcr control` (no server, or one too old for the live route)
    /// refuses a Codex account the way the live route does, so `tcr control` cannot
    /// persist a `controlAccount` the next boot would ignore. The file is left as it was.
    ///
    /// The config names a port nothing listens on (`test_support::loopback::unused_port`): an
    /// edit asks whether a
    /// server is running on the config's port, and without one that is 3456, where a live
    /// tcr may be serving.
    ///
    /// Watched red: drop the provider check from `write_control_account` and the file
    /// names the Codex row.
    #[tokio::test]
    async fn write_control_account_refuses_a_codex_account_and_writes_nothing() {
        let port = crate::test_support::loopback::unused_port();
        let path = write_config(
            "control-codex",
            &serde_json::json!({
                "proxy": { "port": port },
                "accounts": [
                    { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                      "expiresAt": 1_893_456_000_000_i64, "priority": 0 },
                    { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
                      "accountUuid": "11111111-1111-4111-8111-111111111111", "accessToken": "at-b",
                      "refreshToken": "rt-b", "expiresAt": 1_893_456_000_000_i64, "priority": 0 }
                ]
            })
            .to_string(),
        );
        let before = fs::read_to_string(&path).unwrap();
        let err = write_control_account(&path, Some("bob@example.com/codex"))
            .expect_err("a Codex account cannot be the control account");
        let message = format!("{err:#}");
        assert!(
            message.contains("bob@example.com/codex")
                && message.contains(crate::manager::CODEX_HAS_NO_CONTROL_ACCOUNT),
            "{message}"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            before,
            "nothing was written"
        );
        assert_eq!(
            write_control_account(&path, Some("alice@example.com")).unwrap(),
            Some("alice@example.com".to_string()),
            "control: the Anthropic row is set"
        );
        fs::remove_file(&path).ok();
    }

    // --- the exit lock on a Codex account -------------------------------------

    /// alice (Claude) and bob (Codex) on a port nothing serves: an edit asks
    /// whether a server is running on the config's port, and without one that is
    /// 3456, where a live tcr may be serving. `bob_extra` is merged into bob's row.
    async fn alice_and_codex_bob(tag: &str, bob_extra: serde_json::Value) -> std::path::PathBuf {
        let port = crate::test_support::loopback::unused_port();
        let mut bob = serde_json::json!({
            "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
            "accessToken": "at-b", "refreshToken": "rt-b", "credentialVersion": 2
        });
        if let (Some(row), Some(extra)) = (bob.as_object_mut(), bob_extra.as_object()) {
            row.extend(extra.clone());
        }
        let json = serde_json::json!({
            "proxy": { "port": port },
            "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "refreshToken": "rt-a" },
                bob
            ]
        });
        assert_ne!(json["proxy"]["port"], serde_json::json!(3456));
        write_config(tag, &json.to_string())
    }

    /// `tcr peer account <codex> --exits-from <peer>` is refused, and so is
    /// `--must` on its own, which would make an old pin strict: a Codex request
    /// always leaves from this Mac. Each refusal leaves the file byte-identical,
    /// and the same pin on a Claude row is written as it always was.
    #[tokio::test]
    async fn exits_from_a_peer_is_refused_on_a_codex_row() {
        let refusal = "'bob@example.com/codex' is a Codex account, and a Codex request always leaves from this Mac, so it cannot exit from another Mac. Nothing was changed.";
        let path = alice_and_codex_bob("egress-codex", serde_json::json!({})).await;
        let peer = tcr_peer_wire::PeerId([7_u8; 32]);
        let before = fs::read(&path).unwrap();

        let err = set_account_egress(
            &path,
            "bob@example.com/codex",
            Some(config::Egress::Via(peer)),
            None,
        )
        .expect_err("a Codex account cannot be pinned to another Mac");
        assert_eq!(err.to_string(), refusal);
        assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");

        let err = set_account_egress(&path, "bob@example.com/codex", None, Some(true))
            .expect_err("--must alone would make an old pin strict");
        assert_eq!(err.to_string(), refusal);
        assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");

        let (name, pin) = set_account_egress(
            &path,
            "alice@example.com",
            Some(config::Egress::Via(peer)),
            None,
        )
        .expect("control: a Claude row is pinned as before");
        assert_eq!(name, "alice@example.com");
        assert_eq!(pin.egress, config::Egress::Via(peer));
        fs::remove_file(&path).ok();
    }

    /// `--exits-from local --no-must` stays allowed on a Codex row, so a pin
    /// written before the refusal existed can still be cleared.
    #[tokio::test]
    async fn exits_from_local_still_clears_an_old_codex_pin() {
        let peer = tcr_peer_wire::PeerId([7_u8; 32]);
        let path = alice_and_codex_bob(
            "egress-codex-clear",
            serde_json::json!({ "egress": format!("via {}", peer.to_wire()), "egressStrict": true }),
        )
        .await;
        assert_eq!(
            load(&path).accounts[1].egress_pin(),
            config::EgressPin {
                egress: config::Egress::Via(peer),
                strict: true
            },
            "the fixture carries the old strict pin"
        );

        set_account_egress(
            &path,
            "bob@example.com/codex",
            Some(config::Egress::Local),
            Some(false),
        )
        .expect("local and --no-must clear the old pin");
        assert_eq!(
            load(&path).accounts[1].egress_pin(),
            config::EgressPin {
                egress: config::Egress::Local,
                strict: false
            }
        );
        fs::remove_file(&path).ok();
    }

    // --- groups and Codex accounts --------------------------------------------

    /// `tcr group add` on a Codex account says on stderr what the label does and
    /// does not do, and still writes it: the label parks the account, and the
    /// command's idempotent exit-0 contract with TcrBar is unchanged.
    #[tokio::test]
    async fn group_add_on_a_codex_row_warns_on_stderr() {
        let path = alice_and_codex_bob("group-add-codex", serde_json::json!({})).await;
        let config = load(&path);
        assert_eq!(
            codex_group_add_notice(&config.accounts[1], "orchard"),
            Some("warning: 'bob@example.com/codex' is a Codex account. A Codex session cannot ask for a group, so 'orchard' labels it and can park it, but never routes to it; while 'orchard' is reserved, it serves nothing.".to_string())
        );
        assert_eq!(
            codex_group_add_notice(&config.accounts[0], "orchard"),
            None,
            "a Claude row is an ordinary add"
        );

        add_to_group(&path, "orchard", "bob@example.com/codex")
            .expect("the label is written all the same");
        assert!(load(&path).accounts[1].in_group("orchard"));
        fs::remove_file(&path).ok();
    }

    /// `tcr group ls` counts a group's Codex members apart, and `routes=` keeps
    /// meaning "a Claude session asking for this group can be served": it counts
    /// Claude members only, because no Codex request carries a group. A group of
    /// Codex members alone says why it routes nothing, and a group with no Codex
    /// member reads exactly as it always did.
    #[test]
    fn group_ls_counts_codex_members_and_routes_counts_anthropic_only() {
        let path = write_config(
            "group-ls-codex",
            r#"{
              "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "groups": ["mixed", "solo"] },
                { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
                  "accessToken": "at-b", "credentialVersion": 2, "groups": ["mixed", "lab"] }
              ]
            }"#,
        );
        let config = load(&path);

        let text = render_groups_text(&config);
        let line = |group: &str| -> String {
            text.lines()
                .find(|line| line.starts_with(&format!("group {group} ")))
                .unwrap_or_else(|| panic!("a line for {group}: {text}"))
                .to_string()
        };
        let mixed = line("mixed");
        assert!(
            mixed.contains(" members=alice@example.com,bob@example.com/codex codex_members=1 "),
            "{mixed}"
        );
        assert!(mixed.contains(" routes=yes "), "{mixed}");
        let lab = line("lab");
        assert!(lab.contains(" codex_members=1 "), "{lab}");
        assert!(lab.contains(" routes=no route_block=codex-only "), "{lab}");
        let solo = line("solo");
        assert!(!solo.contains("codex_members"), "{solo}");
        assert!(solo.contains(" routes=yes "), "{solo}");

        let row = |name: &str| group_row(&config, name);
        assert_eq!(row("mixed")["codexMembers"], serde_json::json!(1));
        assert_eq!(row("mixed")["routes"], serde_json::json!(true));
        assert_eq!(row("mixed")["routeBlock"], serde_json::Value::Null);
        assert_eq!(row("lab")["codexMembers"], serde_json::json!(1));
        assert_eq!(row("lab")["routes"], serde_json::json!(false));
        assert_eq!(row("lab")["routeBlock"], serde_json::json!("codex-only"));
        assert!(
            row("solo").get("codexMembers").is_none(),
            "a group with no Codex member carries no new key: {}",
            row("solo")
        );
        fs::remove_file(&path).ok();
    }

    // --- group color --------------------------------------------------------

    /// A configured color round-trips through config and onto both `ls`
    /// renderers, tagged `color_source=set`/`colorSource: "set"`.
    #[test]
    fn group_color_set_round_trips_through_config_and_ls() {
        let path = write_config("color-set", GROUPED_ACCOUNTS);
        set_group_color(&path, "codereview", Some("#32D74B")).unwrap();
        let config = load(&path);
        assert_eq!(
            config.group_color("codereview"),
            ("#32d74b".to_string(), config::ColorSource::Set),
            "hex normalizes to lowercase on write"
        );
        let text = render_groups_text(&config);
        let line = text
            .lines()
            .find(|l| l.starts_with("group codereview"))
            .expect("codereview line");
        assert!(
            line.contains("color=#32d74b") && line.contains("color_source=set"),
            "text ls carries the set color: {line}"
        );
        let json_value: serde_json::Value =
            serde_json::from_str(&render_groups_json(&config).unwrap()).unwrap();
        let row = json_value["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["group"] == "codereview")
            .expect("codereview row");
        assert_eq!(row["color"], serde_json::json!("#32d74b"));
        assert_eq!(row["colorSource"], serde_json::json!("set"));
        fs::remove_file(&path).ok();
    }

    /// THE test for problem 3's trigger condition: a known low-contrast pick
    /// (`#d69451`, `config::best_readable_apca` measures ~54.1, below the 60
    /// floor) produces a warning naming the measured value and a derived
    /// alternative that itself clears the floor — driving
    /// `low_contrast_color_warning`'s actual branch, not just checking it
    /// compiles.
    #[test]
    fn low_contrast_color_warning_fires_and_names_a_good_alternative() {
        let low_contrast = "#d69451";
        assert!(
            config::best_readable_apca(low_contrast) < 60.0,
            "test fixture assumption broken: {low_contrast} must score below the APCA floor"
        );
        let warning = low_contrast_color_warning(low_contrast, "codereview").expect("must warn");
        assert!(warning.contains(low_contrast), "names the color: {warning}");
        assert!(warning.contains("codereview"), "names the group: {warning}");
        let alternative = config::derive_group_color("codereview");
        assert!(
            warning.contains(&alternative),
            "names a derived alternative ({alternative}): {warning}"
        );
        assert!(
            config::best_readable_apca(&alternative) >= 60.0,
            "the alternative named in the warning must itself be readable"
        );
    }

    /// Control for the test above: a color that DOES clear the APCA floor
    /// gets no warning at all — the low-contrast test is exercising the
    /// warning branch specifically, not a function that always returns `Some`.
    #[test]
    fn low_contrast_color_warning_is_silent_for_a_readable_pick() {
        let readable = "#32d74b";
        assert!(
            config::best_readable_apca(readable) >= 60.0,
            "test fixture assumption broken: {readable} must clear the APCA floor"
        );
        assert_eq!(low_contrast_color_warning(readable, "codereview"), None);
    }

    /// `set_group_color` itself must still SUCCEED (never refuse) and
    /// actually persist a low-contrast pick to disk — the warning is
    /// advisory, not a gate. Exercises the real CLI entry point end to end,
    /// distinct from the two pure-function tests above.
    #[test]
    fn set_group_color_accepts_a_low_contrast_pick_instead_of_refusing() {
        let low_contrast = "#d69451";
        let path = write_config("color-low-contrast", GROUPED_ACCOUNTS);
        let result = set_group_color(&path, "codereview", Some(low_contrast));
        assert!(
            result.is_ok(),
            "a low-contrast color must warn, not be refused: {result:?}"
        );
        let config = load(&path);
        assert_eq!(
            config.group_color("codereview"),
            (low_contrast.to_string(), config::ColorSource::Set),
            "the low-contrast color must actually be set, not silently dropped"
        );
        fs::remove_file(&path).ok();
    }

    /// An unconfigured group gets a derived color that is stable across
    /// independent reads and differs from another name's — the bridge's
    /// "stable across runs, differs by name" requirement, checked directly
    /// against the config layer rather than the CLI (no write happens here).
    #[test]
    fn unconfigured_group_color_is_derived_stable_and_distinguishes_names() {
        let path = write_config("color-derived", GROUPED_ACCOUNTS);
        let config = load(&path);
        let (first, source) = config.group_color("codereview");
        assert_eq!(source, config::ColorSource::Derived);
        let (second, _) = config.group_color("codereview");
        assert_eq!(first, second, "same name, same process, same color");
        // Independent load — a fresh process reading the same file — must
        // derive the identical color: stability cannot depend on anything
        // but the group name.
        let reloaded = load(&path);
        let (third, _) = reloaded.group_color("codereview");
        assert_eq!(first, third, "stable across independent loads");

        let (dev_color, _) = config.group_color("dev");
        assert_ne!(
            first, dev_color,
            "two different group names must not collide onto the same color"
        );
        fs::remove_file(&path).ok();
    }

    /// `--clear` reverts a configured color to the derived default — the
    /// SAME derived value `unconfigured_group_color_is_derived_...` above
    /// pins, not a different fallback.
    #[test]
    fn group_color_clear_reverts_to_derived() {
        let path = write_config("color-clear", GROUPED_ACCOUNTS);
        let (derived_before, _) = load(&path).group_color("codereview");

        set_group_color(&path, "codereview", Some("#123456")).unwrap();
        assert_eq!(
            load(&path).group_color("codereview"),
            ("#123456".to_string(), config::ColorSource::Set)
        );

        set_group_color(&path, "codereview", None).unwrap();
        let (after_clear, source) = load(&path).group_color("codereview");
        assert_eq!(source, config::ColorSource::Derived);
        assert_eq!(
            after_clear, derived_before,
            "clearing must land on exactly the value that was always derived, \
             not some other fallback"
        );
        fs::remove_file(&path).ok();
    }

    /// Strictly invalid hex is rejected, and — like `reserve`'s safety
    /// rail — the config is byte-identical after the refusal: a rejected
    /// write must never touch the file.
    #[test]
    fn group_color_rejects_invalid_hex_and_leaves_file_untouched() {
        let path = write_config("color-bad-hex", GROUPED_ACCOUNTS);
        let before = fs::read_to_string(&path).unwrap();
        for bad in ["32d74b", "#32d74", "#zzzzzz", "#12345g", ""] {
            let result = set_group_color(&path, "codereview", Some(bad));
            assert!(result.is_err(), "{bad:?} must be rejected");
        }
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a rejected hex must not touch the file");
        fs::remove_file(&path).ok();
    }

    /// `#RGB` shorthand expands to the doubled `#RRGGBB` form.
    #[test]
    fn group_color_accepts_short_hex_form() {
        let path = write_config("color-short-hex", GROUPED_ACCOUNTS);
        set_group_color(&path, "codereview", Some("#abc")).unwrap();
        assert_eq!(
            load(&path).group_color("codereview"),
            ("#aabbcc".to_string(), config::ColorSource::Set)
        );
        fs::remove_file(&path).ok();
    }

    /// Setting a color on a group no account carries is an error listing the
    /// configured groups — matching `add_to_group`'s unknown-account message
    /// shape (via `find_account_by_name`).
    #[test]
    fn group_color_on_unknown_group_lists_configured_groups() {
        let path = write_config("color-unknown-group", GROUPED_ACCOUNTS);
        let before = fs::read_to_string(&path).unwrap();
        let err = set_group_color(&path, "typo-group", Some("#32d74b"))
            .expect_err("an unconfigured group name must be refused");
        let msg = err.to_string();
        assert!(msg.contains("codereview") && msg.contains("dev"), "{msg}");
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "a refused group name must not touch the file"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn group_ls_json_lists_an_account_in_two_groups_under_both() {
        let config = config::load(&write_config("group-ls-json", GROUPED_ACCOUNTS)).unwrap();
        let rendered = render_groups_json(&config).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let groups = value["groups"].as_array().unwrap();
        let codereview = groups
            .iter()
            .find(|g| g["group"] == "codereview")
            .expect("codereview group must be present");
        let members: Vec<&str> = codereview["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert!(
            members.contains(&"alice@example.com"),
            "alice is in codereview AND dev — she must appear under codereview too: {members:?}"
        );
        assert!(members.contains(&"bob@example.com"));
        let dev = groups
            .iter()
            .find(|g| g["group"] == "dev")
            .expect("dev group must be present");
        let dev_members: Vec<&str> = dev["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert!(
            dev_members.contains(&"alice@example.com"),
            "alice must ALSO appear under dev: {dev_members:?}"
        );
        assert_eq!(value["ungrouped"], serde_json::json!(1)); // carol only
    }

    #[test]
    fn group_ls_text_reports_ungrouped_count() {
        let config = config::load(&write_config("group-ls-text", GROUPED_ACCOUNTS)).unwrap();
        let rendered = render_groups_text(&config);
        assert!(rendered.contains("ungrouped accounts=1"), "{rendered}");
        assert!(rendered.contains("group codereview"), "{rendered}");
        assert!(rendered.contains("group dev"), "{rendered}");
    }

    // --- enable / disable --------------------------------------------------

    /// `TWO_ACCOUNTS` pointed at a port nothing serves, written to a temp file.
    ///
    /// The port is `test_support::loopback::unused_port`, which never draws 3456: `TWO_ACCOUNTS`
    /// names 3456 and this substitutes it away precisely so a test can never POST an
    /// account-control command at the REAL proxy, and a draw of 3456 would make the
    /// substitution a no-op.
    ///
    /// Load-bearing safety, not tidiness: `set_enabled` is live-first now, and
    /// `TWO_ACCOUNTS` names port **3456** — the port a REAL tcr serves Gil's fleet
    /// on. A test left on that port would POST an account-control command at the
    /// live proxy. The substitution is asserted below because a silent no-op here
    /// would aim every one of these tests at the running server.
    async fn config_on_a_dead_port(tag: &str) -> std::path::PathBuf {
        let port = crate::test_support::loopback::unused_port();
        let json = TWO_ACCOUNTS.replace("\"port\": 3456 }", &format!("\"port\": {port} }}"));
        // Compare the PARSED value, not the text: a substring check for
        // `"port": 3456` also matched a drawn port of 34560–34569 and tripped
        // this guard on a healthy substitution (about one Linux run in 3,000).
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["proxy"]["port"],
            serde_json::json!(port),
            "the port substitution must apply, or this test talks to the live proxy"
        );
        write_config(tag, &json)
    }

    #[tokio::test]
    async fn set_enabled_true_writes_disabled_true() {
        let path = config_on_a_dead_port("disable").await;
        set_enabled(&path, "alice@example.com", true).await.unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["accounts"][0]["disabled"], serde_json::json!(true));
        fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn set_enabled_false_drops_the_disabled_key() {
        let path = config_on_a_dead_port("enable").await;
        // First disable, then re-enable — the key must vanish entirely.
        set_enabled(&path, "alice@example.com", true).await.unwrap();
        set_enabled(&path, "alice@example.com", false)
            .await
            .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            value["accounts"][0].get("disabled").is_none(),
            "re-enable must DROP the disabled key, not write false"
        );
        fs::remove_file(&path).ok();
    }

    /// THE BITING TEST for the CLI half, end to end through the PRODUCTION path:
    /// a real hybrid listener (the thing that injects `ClientAddr`), a real socket,
    /// the real router, and `set_enabled` as `tcr disable` calls it.
    ///
    /// The two assertions after the call are the whole bug. Pre-change `set_enabled`
    /// wrote the file and stopped, so the RUNNING manager's `disabled` stayed
    /// `false` and the account it named kept being handed traffic — measured on the
    /// live fleet, two accounts deep. A file-only assertion passes against that
    /// defect; the in-memory one does not.
    #[tokio::test]
    async fn set_enabled_parks_the_account_in_the_running_proxy() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let path = config_on_a_dead_port("live-disable").await;
        let mut config = load(&path);
        config.proxy.port = port;

        // The server owns the SAME config file the CLI is pointed at, exactly as
        // the real proxy does — so the durable half lands where `tcr` looks.
        let manager =
            crate::test_support::live_proxy::serve(listener, config.clone(), Some(path.clone()));

        // Point the CLI at the live port by rewriting the file it will load.
        point_at_port(&path, port);

        set_enabled(&path, "alice@example.com", true).await.unwrap();

        // 1. THE RUNNING ROTATION. Not a fresh Manager, not the file — the process
        //    that would serve the next request.
        let live = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(live.accounts[0].name, "alice@example.com");
        assert!(
            live.accounts[0].disabled,
            "the account is parked IN THE SERVING PROCESS, not just on disk"
        );
        assert!(
            !live.accounts[1].disabled,
            "and only the resolved account was touched"
        );

        // 2. …and the file carries it, so the bench survives a restart.
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after["accounts"][0]["disabled"],
            serde_json::json!(true),
            "the durable half landed too: {after}"
        );

        // 3. Re-enable, live, and the key is DROPPED from the document — the same
        //    contract the file-only path has always had.
        set_enabled(&path, "alice@example.com", false)
            .await
            .unwrap();
        assert!(
            !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "re-enable reaches the live rotation too"
        );
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "re-enable DROPS the key rather than writing false: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// An ambiguous query is refused by the SERVER and the CLI does not fall back:
    /// the file's own resolution could land on a different row than the one the
    /// server was talking about, and a disable applied to the wrong account is
    /// worse than none. The live rotation is broken by hand to hold two rows of
    /// the same name; the file keeps unique names throughout, which is what
    /// makes the byte-identical assertion below mean what it says.
    #[tokio::test]
    async fn set_enabled_refuses_an_ambiguous_query_without_writing() {
        let (path, listener) = config_on_a_scratch_port("live-ambiguous").await;

        let before = serve_a_fleet_with_two_alices(&path, listener);

        let err = set_enabled(&path, "alice@example.com", true)
            .await
            .expect_err("an ambiguous query must not be applied");
        let text = err.to_string();
        assert!(
            text.contains("ambiguous") && text.matches("alice@example.com").count() >= 2,
            "the refusal names the candidates: {text}"
        );
        assert_eq!(
            before,
            fs::read_to_string(&path).unwrap(),
            "a refused command leaves the config byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    // --- post_add_account ---------------------------------------------------

    /// A minimal valid live server config: the port, no accounts, no api-key.
    fn add_route_config(port: u16) -> Config {
        serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap()
    }

    /// A blank credential — the deliberately-invalid probe body — always
    /// rejected by [`crate::proxy::add_account_handler`]'s own validation, so
    /// this can never add a real account.
    fn blank_account() -> Account {
        Account {
            access_token: String::new(),
            refresh_token: None,
            expires_at: None,
            priority: None,
            ..Account::fixture("")
        }
    }

    #[tokio::test]
    async fn post_add_account_reads_a_stamped_400_as_rejected() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let config = add_route_config(port);
        crate::test_support::live_proxy::serve(listener, config.clone(), None);

        let err = post_add_account(&config, &blank_account())
            .await
            .expect_err("a blank name/token must be refused by the route's own validation");
        std::assert_matches!(
            err,
            LiveControlError::Rejected(ref msg) if msg.contains("required"),
            "a stamped 400 is a structural Rejected, not a guess from status alone"
        );
    }

    /// The providers a STAMPED answer names come back beside it, read off the very probe
    /// `oauth::probe_add_route` already sends. The same header on an UNSTAMPED answer — an older
    /// tcr's catch-all, anything that is not this route — is no proof of anything and is not read.
    #[tokio::test]
    async fn post_add_account_with_providers_reads_them_off_a_stamped_answer_only() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let config = add_route_config(port);
        crate::test_support::live_proxy::serve(listener, config.clone(), None);

        let (answer, providers) = post_add_account_with_providers(&config, &blank_account()).await;
        std::assert_matches!(
            answer,
            Err(LiveControlError::Rejected(_)),
            "the blank probe is still the route's own stamped 400"
        );
        assert_eq!(providers, vec![Provider::Anthropic, Provider::Codex]);

        let (listener, port) = crate::test_support::loopback::listener().await;
        tokio::spawn(async move {
            let unstamped = axum::Router::new().fallback(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    [(crate::proxy::PROVIDERS_HEADER, "anthropic,codex")],
                    "",
                )
            });
            axum::serve(listener, unstamped)
                .await
                .log_debug("serving the test stub");
        });
        let config = add_route_config(port);
        let (answer, providers) = post_add_account_with_providers(&config, &blank_account()).await;
        std::assert_matches!(answer, Err(LiveControlError::NoRoute));
        assert!(
            providers.is_empty(),
            "an unstamped answer names no providers, whatever its headers say: {providers:?}"
        );
    }

    #[tokio::test]
    async fn post_add_account_unstamped_404_is_no_route() {
        // An "older tcr": something answers, but not on this route — no
        // `ENDPOINT_HEADER`, unlike every response this crate's own router
        // produces (even its 404s and 405s).
        let (listener, port) = crate::test_support::loopback::listener().await;
        tokio::spawn(async move {
            axum::serve(listener, axum::Router::new())
                .await
                .log_debug("serving the test stub");
        });
        let config = add_route_config(port);

        let err = post_add_account(&config, &blank_account())
            .await
            .expect_err("nothing is registered at this path");
        std::assert_matches!(
            err,
            LiveControlError::NoRoute,
            "an unstamped 404 must read as NoRoute, never Rejected"
        );
    }

    #[tokio::test]
    async fn post_add_account_wrong_api_key_is_unauthorized() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let server_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#
        ))
        .unwrap();
        crate::test_support::live_proxy::serve(listener, server_config, None);

        let client_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
        ))
        .unwrap();
        let err = post_add_account(&client_config, &blank_account())
            .await
            .expect_err("a wrong api-key must be refused");
        std::assert_matches!(
            err,
            LiveControlError::Unauthorized,
            "must surface as Unauthorized, never NoRoute"
        );
    }

    /// THE ROUND TRIP: a real account, `POSTed` to a real live server, lands in
    /// the SERVING rotation — not just in the response body.
    #[tokio::test]
    async fn post_add_account_adds_a_new_account_to_the_live_rotation() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let config = add_route_config(port);
        let manager = crate::test_support::live_proxy::serve(listener, config.clone(), None);

        let account = Account {
            access_token: "at-new".to_string(),
            refresh_token: Some("rt-new".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            ..Account::fixture("new@example.com")
        };
        let applied = post_add_account(&config, &account)
            .await
            .expect("a fresh identity must be added, not rejected");
        assert!(
            applied.added,
            "a nonexistent identity is ADDED: {applied:?}"
        );
        assert_eq!(applied.name, "new@example.com");

        let live = manager.snapshot(OffsetDateTime::now_utc());
        assert!(
            live.accounts.iter().any(|a| a.name == "new@example.com"),
            "the account must be in the SERVING rotation, not just the response"
        );
    }

    /// An ambiguous identity is refused by the route, structurally — never
    /// guessed onto one of the candidates.
    #[tokio::test]
    async fn post_add_account_ambiguous_identity_is_rejected_not_guessed() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        // Two live accounts share a name and neither carries an org — the
        // legacy shape `same_identity`/`match_one` cannot break a tie on.
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "dup@example.com", "type": "oauth", "accessToken": "at-1",
                  "refreshToken": "rt-1", "priority": 0 }},
                {{ "name": "dup@example.com", "type": "oauth", "accessToken": "at-2",
                  "refreshToken": "rt-2", "priority": 1 }}
            ] }}"#
        );
        let config: Config = serde_json::from_str(&seed).unwrap();
        crate::test_support::live_proxy::serve(listener, config.clone(), None);

        let account = Account {
            access_token: "at-new".to_string(),
            refresh_token: Some("rt-new".to_string()),
            expires_at: None,
            priority: None,
            ..Account::fixture("dup@example.com")
        };
        let err = post_add_account(&config, &account)
            .await
            .expect_err("an ambiguous identity must be refused, not guessed");
        std::assert_matches!(err, LiveControlError::Rejected(ref msg) if msg.contains("ambiguous"));
    }

    // --- resolve_account ---------------------------------------------------

    fn load_from(json: &str) -> Config {
        serde_json::from_str(json).unwrap()
    }

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

    /// A prober that returns a fixed 5-hour bar for any token — lets a snapshot
    /// carry a known quota% without touching the network.
    struct FixedProber {
        util: f64,
    }
    impl UsageProber for FixedProber {
        fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
            let util = self.util;
            Box::pin(async move {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Read(UsageBucket {
                        utilization: Some(util),
                        reset_at_ms: Some(crate::now_ms() + 3_600_000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            })
        }
    }

    // --- tcr list --json ----------------------------------------------------

    /// `tcr list --json` over the config at `path` with a [`CountingProber`], which file is then
    /// removed: the document, the exit code, and how many probes reached the prober.
    async fn list_v1_counting_probes(path: &Path) -> (String, i32, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let (document, code) = render_list_v1(
            path,
            Arc::new(CountingProber {
                calls: Arc::clone(&calls),
            }),
        )
        .await;
        fs::remove_file(path).ok();
        (document, code, calls)
    }

    /// A prober that only counts: `tcr list` must never reach it.
    struct CountingProber {
        calls: Arc<AtomicU64>,
    }
    impl UsageProber for CountingProber {
        fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Read(UsageBucket {
                        utilization: Some(0.5),
                        reset_at_ms: Some(crate::now_ms() + 3_600_000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            })
        }
    }

    /// With no server, `tcr list --json` lists every configured account as
    /// `unavailable`, in file order, and the prober sees ZERO calls. Both
    /// accounts carry an unexpired token, so a path that probed would reach
    /// the prober twice.
    #[tokio::test]
    async fn list_v1_with_no_server_never_probes_and_lists_every_account_unavailable() {
        let json = TWO_ACCOUNTS.replace(
            "3456",
            &crate::test_support::loopback::unused_port().to_string(),
        );
        let path = write_config("list-v1-no-server", &json);
        let (document, code, calls) = list_v1_counting_probes(&path).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "tcr list must never probe; a CodexBar poll would become a usage call per account"
        );
        assert_eq!(code, 0);
        let doc: serde_json::Value = serde_json::from_str(&document).expect("one JSON document");
        assert_eq!(doc["schemaVersion"], 1);
        assert!(doc["activeAccountNumber"].is_null());
        assert_eq!(doc["supportsAccountSwitching"], false);
        let accounts = doc["accounts"].as_array().expect("accounts");
        let got: Vec<(i64, &str, &str)> = accounts
            .iter()
            .map(|a| {
                (
                    a["number"].as_i64().expect("number"),
                    a["email"].as_str().expect("email"),
                    a["usageStatus"].as_str().expect("usageStatus"),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (1, "alice@example.com", "unavailable"),
                (2, "bob@example.com", "unavailable"),
            ]
        );
        assert!(
            accounts.iter().all(|a| a["usage"] == serde_json::json!({})),
            "nothing was measured, so no window is claimed"
        );
    }

    /// A config that cannot be parsed is the schema-v1 error envelope on
    /// stdout with exit 1, because CodexBar parses stdout on any exit code.
    #[tokio::test]
    async fn list_v1_reports_an_unreadable_config_in_the_error_envelope() {
        let path = write_config("list-v1-bad", "{ not json");
        let (document, code, calls) = list_v1_counting_probes(&path).await;

        assert_eq!(code, 1);
        let doc: serde_json::Value = serde_json::from_str(&document).expect("one JSON document");
        assert_eq!(doc["schemaVersion"], 1);
        assert_eq!(doc["error"]["type"], "config_unreadable");
        assert!(
            doc["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// **`tcr list --json` never hands CodexBar a Codex account as one of its Claude
    /// accounts.** The document is claude-swap's list, which CodexBar reads as Claude
    /// accounts and nothing else, and it has no field that could say otherwise: listed,
    /// a ChatGPT login would be drawn as a Claude account, under its email, with its
    /// Codex windows on Claude bars. The Claude rows keep their file order and number
    /// from 1 among themselves.
    ///
    /// Watched red: list every row, and the Codex account takes slot 2 as a second
    /// `alice@example.com`.
    #[tokio::test]
    async fn list_v1_never_lists_a_codex_account() {
        let json = format!(
            r#"{{ "proxy": {{ "port": {} }}, "accounts": [
                 {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                    "refreshToken": "rt-a", "expiresAt": 4102444800000 }},
                 {{ "name": "alice@example.com/codex", "provider": "codex", "type": "oauth",
                    "accessToken": "at-c", "refreshToken": "rt-c", "expiresAt": 4102444800000,
                    "accountUuid": "11111111-1111-4111-8111-111111111111", "credentialVersion": 2 }},
                 {{ "name": "bob@example.com", "type": "oauth", "accessToken": "at-b",
                    "refreshToken": "rt-b", "expiresAt": 4102444800000 }}
               ] }}"#,
            crate::test_support::loopback::unused_port()
        );
        let path = write_config("list-v1-codex", &json);
        let (document, code, calls) = list_v1_counting_probes(&path).await;

        assert_eq!(code, 0, "{document}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "it still never probes");
        let doc: serde_json::Value = serde_json::from_str(&document).expect("one JSON document");
        let got: Vec<(i64, &str)> = doc["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .map(|a| {
                (
                    a["number"].as_i64().expect("number"),
                    a["email"].as_str().expect("email"),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![(1, "alice@example.com"), (2, "bob@example.com")],
            "{document}"
        );
    }

    /// Slots follow the FILE, whatever order the server returns rows in: a row
    /// the server lacks keeps the file's place, and one the file lost goes last.
    #[tokio::test]
    async fn overlay_live_rows_keeps_the_file_order() {
        let config = load_from(TWO_ACCOUNTS);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            false,
        )
        .await;
        let colors = std::collections::BTreeMap::new();
        let file_rows = account_status_rows(
            &snapshot,
            &[],
            StatusSource::Offline,
            None,
            false,
            None,
            &colors,
        );
        // The server knows bob (first, as if the file was reordered after it
        // booted) and carol (since removed from the file), not alice.
        let mut bob = file_rows[1].clone();
        bob.source = "live".into();
        let mut carol = file_rows[0].clone();
        carol.name = "carol@example.com".into();
        carol.source = "live".into();

        let rows = overlay_live_rows(file_rows, vec![bob, carol]);
        let got: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.source.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("alice@example.com", "offline"),
                ("bob@example.com", "live"),
                ("carol@example.com", "live"),
            ]
        );
    }

    #[tokio::test]
    async fn render_accounts_emits_one_greppable_line_per_account() {
        let config = load_from(TWO_ACCOUNTS);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;
        // Offline is the honest source for a snapshot built by `snapshot_offline`,
        // and it is the exact line shape `tcr accounts` emits — that verb is
        // offline by construction, so this is the greppable line a human reads
        // most often.
        let text = render_accounts(&snapshot, StatusSource::Offline);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per account");
        // Each line is greppable: name + priority + per-window utilization.
        assert!(lines[0].contains("alice@example.com"));
        assert!(lines[0].contains("priority=0"));
        // FixedProber sets five_hour=0.25, seven_day/seven_day_oi=None.
        assert!(lines[0].contains("5h=25%"), "5h window: {}", lines[0]);
        assert!(lines[0].contains("wk=n/a"), "wk unknown: {}", lines[0]);
        // seven_day_oi is None → the fable token is omitted entirely.
        assert!(!lines[0].contains("fable="), "no fable token: {}", lines[0]);
        // Well under the 0.9 default threshold → an honest `ok` state.
        assert!(lines[0].contains("state=ok"), "healthy state: {}", lines[0]);
        assert!(lines[1].contains("bob@example.com"));
        assert!(lines[1].contains("priority=1"));
        // At 25% util no window is a binding hold, but it still has a live
        // reset — and a window's reset is a property of the window, not of
        // whether it is pinning the account, so the inline countdown still
        // shows. The legacy `held=`/`quota=`/`quota_state=` fields are gone
        // for good.
        assert!(
            text.contains("5h=25%(+"),
            "under-threshold accounts still carry their window's countdown: {text}"
        );
        assert!(
            !text.contains("held=") && !text.contains("quota="),
            "legacy quota/held fields are gone: {text}"
        );
        // Every token stays `key=value`, so the new stream-error token is greppable
        // wherever it sits in the line and does not displace its neighbours.
        for line in &lines {
            assert!(
                line.contains("probe=") && line.contains("stream_errors=n/a"),
                "the offline line keeps probe= and carries an unmeasured \
                 stream-error token: {line}"
            );
        }
    }

    /// THE HONESTY TEST. An offline snapshot's serving counters live in the
    /// SERVER's process, so `input_tokens` here is structurally zero — there is
    /// nothing to divide by and no measurement was taken. It must therefore emit
    /// `null` (JSON) / `n/a` (human), never a `0.0` that reads as a measured "0%
    /// cache hits".
    ///
    /// That false zero is the entire reason this bug hid: `tcr status --json`
    /// reported `"cacheHitRatio": 0.0` for every account forever, so the one
    /// surface that should have shown a prompt-cache catastrophe reported
    /// "cache fine" straight through it. The `source` field is the other half —
    /// it names the process the counters would have come from.
    #[tokio::test]
    async fn offline_status_reports_null_not_zero_hit_ratio() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;

        let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        assert_eq!(rows.len(), 2, "one row per account");
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                snapshot.accounts[i].input_tokens, 0,
                "the premise: an offline snapshot has counted nothing"
            );
            // The four serving counters read `null` on the offline path, not
            // a real `0` — same false-zero fix as `cacheHitRatio` below, one
            // field family over: these live in the SERVING process, so a
            // fresh offline `Manager` has structurally never measured them.
            for key in ["requests", "inputTokens", "outputTokens", "cacheReadTokens"] {
                assert!(
                    row[key].is_null(),
                    "{key} is unmeasured on the offline path, never a false 0: {row}"
                );
                assert_ne!(
                    row[key],
                    serde_json::json!(0),
                    "the literal 0 that would masquerade as a measurement is gone: {row}"
                );
            }
            assert!(
                row["cacheHitRatio"].is_null(),
                "an uncounted ratio is null, not a number: {row}"
            );
            assert_ne!(
                row["cacheHitRatio"],
                serde_json::json!(0.0),
                "the literal 0.0 that masqueraded as a measurement is gone: {row}"
            );
            assert_eq!(
                row["source"], "offline",
                "every row names the process its counters came from: {row}"
            );
        }

        // A ratio that IS measured still renders as a number — the null is about
        // absence of data, not a blanket suppression. `requests` must clear
        // `CACHE_SIGNAL_FLOOR_REQUESTS` too, or the floor added below would
        // null this out regardless of the token counts.
        let mut counted = snapshot.clone();
        counted.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS;
        counted.accounts[0].input_tokens = 1_000;
        counted.accounts[0].cache_read_tokens = 750;
        let live = status_json(&counted, &thresholds, StatusSource::Live);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["cacheHitRatio"], serde_json::json!(0.75));
        assert_eq!(rows[0]["source"], "live");
        assert!(
            rows[1]["cacheHitRatio"].is_null(),
            "the still-uncounted account stays null: {}",
            rows[1]
        );

        // The human view uses the same `n/a` idiom the unknown quota windows use.
        let text = render_accounts(&snapshot, StatusSource::Offline);
        for line in text.lines() {
            assert!(
                line.contains("cache=n/a"),
                "uncounted cache reads as n/a, never 0%: {line}"
            );
            assert!(!line.contains("cache=0%"), "no false measured zero: {line}");
        }
        assert!(
            render_accounts(&counted, StatusSource::Live).contains("cache=75%"),
            "a measured ratio still renders as a percentage"
        );
    }

    /// THE SIGNAL-FLOOR TEST. An account whose only real traffic is a
    /// handful of cold diverts has genuinely nonzero, genuinely LIVE token
    /// counters — this is not the `input_tokens == 0` case above — but too
    /// few served requests to trust the ratio it would produce. Before this
    /// fix, `tcr status` reported that account's near-empty-denominator ratio
    /// as a confident measured number (as low as single digits of percent),
    /// which read exactly like "this account's cache is broken" and sent a
    /// lead chasing a regression that was never there
    /// This pins the fix: under `CACHE_SIGNAL_FLOOR_REQUESTS`, the ratio is
    /// `null`/`n/a` — no-signal — same as the honest-zero case, never a
    /// number.
    #[tokio::test]
    async fn an_account_under_the_signal_floor_reports_no_signal_not_a_number() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;

        // Real, live, nonzero tokens — a genuine measurement, not an unset
        // counter — but ONE below the floor's request count. A single cold
        // divert can plausibly write tens of thousands of cache_creation
        // tokens in one request, so a small `requests` count with a large
        // token count is exactly the shape this guards, not a fixture
        // artefact.
        let mut sparse = snapshot;
        sparse.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS - 1;
        sparse.accounts[0].input_tokens = 42_000;
        sparse.accounts[0].cache_read_tokens = 100;

        let live = status_json(&sparse, &thresholds, StatusSource::Live);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert!(
            rows[0]["cacheHitRatio"].is_null(),
            "under the floor, a real nonzero ratio still reads as no-signal, \
             not a misleadingly precise number: {}",
            rows[0]
        );
        assert_ne!(
            rows[0]["cacheHitRatio"],
            serde_json::json!(0.0),
            "no-signal must never collapse to the false '0% cache hits' this \
             module already fixed once: {}",
            rows[0]
        );

        let text = render_accounts(&sparse, StatusSource::Live);
        let line = text
            .lines()
            .find(|l| l.contains(&sparse.accounts[0].name))
            .expect("the sparse account's line");
        assert!(
            line.contains("cache=n/a"),
            "the human view reports the same no-signal state: {line}"
        );

        // One request OVER the floor, same tokens: the ratio is measured and
        // renders as the real number — this is the boundary, not a blanket
        // suppression of light accounts.
        let mut at_floor = sparse.clone();
        at_floor.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS;
        let live = status_json(&at_floor, &thresholds, StatusSource::Live);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(
            rows[0]["cacheHitRatio"],
            serde_json::json!(100.0 / 42_000.0),
            "at the floor the same tokens now render as a real measurement: {}",
            rows[0]
        );
    }

    /// The stream-error counter obeys the same rule as the cache ratio above, and
    /// for a sharper reason: `0` on an ERROR counter is an affirmative all-clear.
    /// Offline it is structurally zero — the count lives in the serving process —
    /// so publishing it as `0` would claim "no truncated streams" about a process
    /// this one never spoke to.
    #[tokio::test]
    async fn offline_status_reports_null_not_zero_stream_errors() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;

        let offline = status_json(&snapshot, &thresholds, StatusSource::Offline);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&offline).expect("valid json");
        for row in &rows {
            // `get`, not `row[...]`: indexing a MISSING key also yields Null, so
            // `row["streamErrorCount"].is_null()` would pass against the very bug
            // this test exists for — the field never being rendered at all.
            assert_eq!(
                row.get("streamErrorCount"),
                Some(&serde_json::Value::Null),
                "the key is present AND null — unmeasured, never a 0 all-clear: {row}"
            );
            assert_eq!(
                row.get("lastStreamError"),
                Some(&serde_json::Value::Null),
                "nothing measured: {row}"
            );
        }

        // Measured and genuinely clean is a DIFFERENT state, and renders as 0.
        let live = status_json(&snapshot, &thresholds, StatusSource::Live);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["streamErrorCount"], serde_json::json!(0));

        // Measured and dirty carries both the count and the latest error type.
        let mut errored = snapshot.clone();
        errored.accounts[0].stream_error_count = 3;
        errored.accounts[0].last_stream_error = Some("overloaded_error".to_string());
        let live = status_json(&errored, &thresholds, StatusSource::Live);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
        assert_eq!(rows[0]["streamErrorCount"], serde_json::json!(3));
        assert_eq!(
            rows[0]["lastStreamError"],
            serde_json::json!("overloaded_error")
        );

        // The text view carries the same three states as greppable tokens.
        let text = render_accounts(&snapshot, StatusSource::Offline);
        for line in text.lines() {
            assert!(
                line.contains("stream_errors=n/a"),
                "unmeasured reads n/a, never 0: {line}"
            );
            assert!(
                !line.contains("stream_errors=0"),
                "no false all-clear: {line}"
            );
        }
        assert!(
            render_accounts(&snapshot, StatusSource::Live).contains("stream_errors=0"),
            "a measured clean fleet still renders a real 0"
        );
        let text = render_accounts(&errored, StatusSource::Live);
        assert!(
            text.contains("stream_errors=3"),
            "the count is greppable: {text}"
        );
        assert!(
            text.contains("last_stream_error=overloaded_error"),
            "the latest error type is greppable: {text}"
        );
    }

    /// END-TO-END through the PRODUCTION path, which is the claim that actually
    /// matters: a real hybrid listener (the thing that injects `ClientAddr`), a
    /// real HTTP round trip on a real socket, and the real client parser.
    ///
    /// The counters below exist ONLY in the served manager's process — exactly the
    /// situation that made `tcr status` lie, since the offline path builds a fresh
    /// `Manager` that can never see them. Binds port 0 (a free ephemeral port) and
    /// overrides the config's 3456, so this never touches a real running proxy.
    #[tokio::test]
    async fn live_status_reads_the_running_servers_counters() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let mut config = load_from(TWO_ACCOUNTS);
        config.proxy.port = port;

        let manager = crate::test_support::live_proxy::serve(listener, config.clone(), None);
        // 750 of 1000 input tokens were prompt-cache reads: a real 75% hit ratio,
        // held in this process only. `record_served` clears
        // `CACHE_SIGNAL_FLOOR_REQUESTS` so the ratio below is a reported number,
        // not the new floor's no-signal null — this test is about the ratio
        // crossing the wire from the live server, not about the floor itself.
        manager.update_usage(0, 1_000, 200, 750, 50);
        let now = OffsetDateTime::now_utc();
        for _ in 0..CACHE_SIGNAL_FLOOR_REQUESTS {
            manager.record_served(0, now, None, crate::stats::SessionKind::Fallback);
        }

        let payload = match fetch_live_status(&config).await {
            Ok(p) => p,
            Err(LiveStatusError::NoServer) => panic!("the spawned server did not answer"),
            Err(err) => panic!("live status unusable: {}", err.why()),
        };
        let build = payload.build.clone();
        let (snapshot, thresholds) = payload.into_snapshot();

        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Live,
            Some(&build),
            false,
            None,
            &Default::default(),
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            rows[0]["cacheHitRatio"],
            serde_json::json!(0.75),
            "the ratio is the SERVER's real measurement, not a structural zero: {}",
            rows[0]
        );
        assert_eq!(rows[0]["inputTokens"], 1_000);
        assert_eq!(rows[0]["cacheReadTokens"], 750);
        assert_eq!(rows[0]["source"], "live");
        // The account that served nothing still reports an honest null, not a 0.0.
        assert!(rows[1]["cacheHitRatio"].is_null(), "{}", rows[1]);
        // Thresholds came from the SERVER, not from re-reading the config file.
        assert_eq!(thresholds.len(), 2);

        // END-TO-END on the build stamp too: the sha crossed a real socket from
        // the serving process, and reaches every rendered row.
        assert_eq!(
            build,
            BuildInfo::current(),
            "the served payload names the build that served it"
        );
        for row in &rows {
            assert_eq!(row["serverSha"], serde_json::json!(build_info::SHA));
        }
    }

    /// A Codex account reads as one on both `tcr status` surfaces, through the LIVE
    /// path the TcrBar panel polls: its JSON row carries `provider: "codex"` and the
    /// ChatGPT plan label, and its text line names the same plan. The Anthropic row
    /// beside it carries no `provider` key at all — what keeps a panel that predates
    /// the key decoding it unchanged — and keeps its Anthropic label.
    ///
    /// Through a real socket from a running server, not from a hand-built snapshot:
    /// the provider has to survive `Manager::snapshot`, the server's `StatusPayload`
    /// and the client's `into_snapshot` before a renderer sees it, and the contract
    /// fixture renders a snapshot directly, so it sees none of those three hops.
    /// Binds port 0, never the config's 3456.
    #[tokio::test]
    async fn live_status_shows_a_codex_accounts_provider_and_chatgpt_plan() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let mut config = load_from(
            r#"{
              "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "refreshToken": "rt-a", "expiresAt": 1893456000000, "priority": 0,
                  "organizationType": "claude_max", "rateLimitTier": "default_claude_max_20x" },
                { "name": "alice@example.com/codex", "type": "oauth", "provider": "codex",
                  "accountUuid": "11111111-1111-4111-8111-111111111111",
                  "organizationType": "pro", "accessToken": "at-c", "refreshToken": "rt-c",
                  "expiresAt": 1893456000000, "priority": 1 }
              ]
            }"#,
        );
        config.proxy.port = port;
        crate::test_support::live_proxy::serve(listener, config.clone(), None);

        let payload = match fetch_live_status(&config).await {
            Ok(p) => p,
            Err(LiveStatusError::NoServer) => panic!("the spawned server did not answer"),
            Err(err) => panic!("live status unusable: {}", err.why()),
        };
        let (snapshot, thresholds) = payload.into_snapshot();

        let json = status_json(&snapshot, &thresholds, StatusSource::Live);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        let (claude, codex) = (&rows[0], &rows[1]);
        assert_eq!(codex["provider"], "codex", "{codex}");
        assert_eq!(codex["plan"], "ChatGPT Pro", "{codex}");
        assert_eq!(
            codex["organizationType"], "pro",
            "the raw claim still rides beside the label: {codex}"
        );
        assert!(
            claude.get("provider").is_none(),
            "an Anthropic row carries no provider key, not even null: {claude}"
        );
        assert_eq!(claude["plan"], "Max 20x", "{claude}");

        let text = render_accounts(&snapshot, StatusSource::Live);
        let line_for = |name: &str| {
            text.lines()
                .find(|l| l.starts_with(&format!("account {name} ")))
                .unwrap_or_else(|| panic!("no line for {name}:\n{text}"))
        };
        let codex_line = line_for("alice@example.com/codex");
        assert!(codex_line.contains("plan=ChatGPT Pro"), "{codex_line}");
        let claude_line = line_for("alice@example.com");
        assert!(claude_line.contains("plan=Max 20x"), "{claude_line}");
    }

    /// The offline path has no serving process, so `serverSha` is `null` — the
    /// same "not measured" idiom as `cacheHitRatio`, and never this CLI's own sha
    /// standing in for a server's.
    #[tokio::test]
    async fn offline_rows_report_a_null_server_sha() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            false,
        )
        .await;

        let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        for row in &rows {
            assert!(
                row["serverSha"].is_null(),
                "no server answered, so no server sha: {row}"
            );
            assert!(row["serverDirty"].is_null(), "{row}");
        }
    }

    /// The text line's provenance tail. With a server it names both builds; with
    /// none it still names the CLI's own, so "which binary am I running" is
    /// always answerable from the first line of output.
    #[test]
    fn build_fields_name_the_server_and_the_client() {
        let server = BuildInfo {
            sha: "cd146ce".to_string(),
            dirty: Some(false),
            built_at: "2026-07-26T00:00:00Z".to_string(),
        };
        let live = build_fields(Some(&server), &BuildInfo::current());
        assert!(live.contains("server_sha=cd146ce"), "{live}");
        assert!(live.contains("server_dirty=false"), "{live}");
        assert!(
            live.contains("server_built_at=2026-07-26T00:00:00Z"),
            "{live}"
        );
        assert!(
            live.contains(&format!("client_sha={}", build_info::SHA)),
            "{live}"
        );

        let offline = build_fields(None, &BuildInfo::current());
        assert!(
            !offline.contains("server_sha="),
            "no server, no server fields: {offline}"
        );
        assert!(
            offline.contains(&format!("client_sha={}", build_info::SHA)),
            "{offline}"
        );

        // An unknown dirty flag reads as `unknown`, never as `false`.
        let murky = build_fields(Some(&BuildInfo::default()), &BuildInfo::current());
        assert!(murky.contains("server_dirty=unknown"), "{murky}");
    }

    /// With no live server there is nothing whose code could be stale, so the
    /// skew check stays silent regardless of what the checkout looks like.
    #[test]
    fn skew_report_is_silent_without_a_server() {
        assert_eq!(skew_report(None), None);
    }

    // ---------------------------------------------------------------------
    // `tcr sessions --json`
    // ---------------------------------------------------------------------

    /// A minimal but REAL status body — it must survive `parse_live_status`,
    /// so `kind` has to be the live constant rather than a placeholder.
    fn status_body(extra: &str) -> String {
        format!(r#"{{"kind":"{STATUS_KIND}","accounts":[]{extra}}}"#)
    }

    /// The whole reason `supported` is read off the raw body: an EMPTY
    /// `sessions` array is still a server that reported the key. The typed
    /// payload cannot tell this case from the one below — both decode to an
    /// empty vec — so a test that went through `StatusPayload` would pass
    /// while the panel drew the wrong sentence.
    #[test]
    fn an_empty_sessions_array_is_still_a_supported_server() {
        let body = status_body(r#","sessions":[]"#);
        assert!(payload_carries_sessions(&body));
        let Ok(payload) = parse_live_status(&body) else {
            panic!("a well-formed status body must parse");
        };
        assert!(payload.sessions.is_empty());
    }

    /// The pre-F1 server: no such key at all.
    #[test]
    fn a_payload_without_the_sessions_key_is_unsupported() {
        assert!(!payload_carries_sessions(&status_body("")));
    }

    #[test]
    fn a_payload_with_rows_is_supported() {
        let body = status_body(
            r#","sessions":[{"sessionId":"11111111-1111-1111-1111-111111111111","firstSeenMs":1,"lastSeenMs":2}]"#,
        );
        assert!(payload_carries_sessions(&body));
        let Ok(payload) = parse_live_status(&body) else {
            panic!("a well-formed status body must parse");
        };
        assert_eq!(payload.sessions.len(), 1);
    }

    /// Not-JSON is not a channel. `false`, never a panic and never `true`.
    #[test]
    fn a_non_json_body_carries_no_sessions() {
        assert!(!payload_carries_sessions("not json at all"));
    }

    /// What `tcr sessions` prints for a Claude-only fleet, pinned before the sessions wire
    /// learned about Codex: every later change must leave these bytes exactly as they are. The
    /// rows are built from JSON holding only the keys a Claude session has always carried, so
    /// the golden cannot lean on a field added after it. The second row's model has no price in
    /// this build and prints `costUsd=0.0000`. That zero is a known defect, kept on purpose:
    /// fixing it would change what a Claude-only fleet prints.
    #[test]
    fn claude_only_sessions_text_is_unchanged() {
        let rows: Vec<tcr_status_wire::SessionRow> = serde_json::from_value(serde_json::json!([
            {
                "sessionId": "a1f3c2d4-0000-4000-8000-000000000001",
                "account": "alice@example.com",
                "model": "claude-opus-5",
                "firstSeenMs": 1_767_223_200_000_i64,
                "lastSeenMs": 1_767_225_597_000_i64,
                "requests": 24,
                "inputTokens": 180_000,
                "outputTokens": 9_200,
                "cacheReadTokens": 160_000,
                "cacheCreationTokens": 12_000,
                "tools": { "calls": 12 },
                "costUsd": 1.2034,
                "cacheTtlSecs": 3600
            },
            {
                "sessionId": "22222222-2222-4222-8222-222222222222",
                "account": "carol@example.com",
                "model": "claude-opus-4-5",
                "firstSeenMs": 1_767_225_000_000_i64,
                "lastSeenMs": 1_767_225_060_000_i64,
                "requests": 1,
                "inputTokens": 10,
                "outputTokens": 2
            },
            {
                "sessionId": "33333333-3333-4333-8333-333333333333",
                "firstSeenMs": 1_767_225_000_000_i64,
                "lastSeenMs": 1_767_225_000_000_i64,
                "requests": 1
            }
        ]))
        .expect("session rows in today's shape");
        assert_eq!(
            render_sessions_text(true, &rows),
            concat!(
                "supported=true sessions=3\n",
                "session=a1f3c2d4-0000-4000-8000-000000000001 account=alice@example.com model=claude-opus-5 requests=24 inputTokens=180000 outputTokens=9200 cacheReadTokens=160000 toolCalls=12 costUsd=1.2034\n",
                "session=22222222-2222-4222-8222-222222222222 account=carol@example.com model=claude-opus-4-5 requests=1 inputTokens=10 outputTokens=2 cacheReadTokens=0 toolCalls=0 costUsd=0.0000\n",
                "session=33333333-3333-4333-8333-333333333333 account=- model=- requests=1 inputTokens=0 outputTokens=0 cacheReadTokens=0 toolCalls=0 costUsd=0.0000\n",
            )
        );
        assert_eq!(
            render_sessions_text(false, &[]),
            "supported=false sessions=0\n"
        );
    }

    /// A Codex session's line names its provider after the account and never prints a cost
    /// nobody measured: `costUsd=n/a` while no request was priced, including before its first
    /// response finished; the priced part and the count it leaves out when only some were; the
    /// plain figure when all were. Reasoning tokens print when there are any.
    #[test]
    fn a_codex_session_line_carries_provider_and_its_unpriced_figures() {
        let codex =
            |id: &str, cost: f64, unpriced: u64, reasoning: u64| -> tcr_status_wire::SessionRow {
                serde_json::from_value(serde_json::json!({
                    "sessionId": id,
                    "account": "bob@example.com/codex",
                    "provider": "codex",
                    "model": "gpt-5.1-codex",
                    "firstSeenMs": 1_767_224_880_000_i64,
                    "lastSeenMs": 1_767_225_599_000_i64,
                    "requests": 7,
                    "inputTokens": 95_000,
                    "outputTokens": 4_100,
                    "cacheReadTokens": 80_000,
                    "tools": { "calls": 3 },
                    "costUsd": cost,
                    "unpricedRequests": unpriced,
                    "reasoningTokens": reasoning
                }))
                .expect("a Codex session row")
            };
        let rows = [
            codex("019a3c5e-0000-7000-8000-000000000002", 0.0, 7, 1_200),
            codex("44444444-4444-4444-8444-444444444444", 0.42, 3, 0),
            codex("55555555-5555-4555-8555-555555555555", 1.5, 0, 0),
            codex("66666666-6666-4666-8666-666666666666", 0.0, 0, 0),
        ];
        let tail = "account=bob@example.com/codex provider=codex model=gpt-5.1-codex requests=7 inputTokens=95000 outputTokens=4100 cacheReadTokens=80000 toolCalls=3";
        assert_eq!(
            render_sessions_text(true, &rows),
            format!(
                "supported=true sessions=4\n\
                 session=019a3c5e-0000-7000-8000-000000000002 {tail} costUsd=n/a reasoningTokens=1200\n\
                 session=44444444-4444-4444-8444-444444444444 {tail} costUsd=0.4200 unpricedRequests=3\n\
                 session=55555555-5555-4555-8555-555555555555 {tail} costUsd=1.5000\n\
                 session=66666666-6666-4666-8666-666666666666 {tail} costUsd=n/a\n"
            )
        );
    }

    /// The document shape the Swift poller decodes. Written as literal text
    /// rather than round-tripped through the same `json!` macro that produced
    /// it — an assertion built from the code under test proves only that the
    /// code equals itself.
    #[test]
    fn sessions_json_is_an_object_with_supported_and_rows() {
        let row = tcr_status_wire::SessionRow {
            session_id: "11111111-1111-1111-1111-111111111111".to_string(),
            account: Some("alice@example.com".to_string()),
            model: Some("claude-sonnet-5".to_string()),
            first_seen_ms: 1_700_000_000_000,
            last_seen_ms: 1_700_000_060_000,
            requests: 3,
            input_tokens: 400,
            output_tokens: 50,
            cache_read_tokens: 900,
            cache_creation_tokens: 25,
            tools: tcr_status_wire::SessionToolsRow::default(),
            req_per_minute: vec![0, 1, 2],
            cost_usd: 0.5,
            cache_ttl_secs: Some(3600),
            provider: None,
            unpriced_requests: 0,
            reasoning_tokens: 0,
        };
        let rendered = render_sessions_json(true, std::slice::from_ref(&row), None, None);
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(value["supported"], serde_json::json!(true));
        // camelCase keys, exactly as the Swift `Session` decoder names them
        // (`FleetStatus.swift`'s `CodingKeys`) — a `rename_all` regression on
        // the wire type fails here rather than on a screen.
        assert_eq!(
            value["sessions"][0]["sessionId"],
            serde_json::json!("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            value["sessions"][0]["cacheReadTokens"],
            serde_json::json!(900)
        );
        assert_eq!(
            value["sessions"][0]["reqPerMinute"],
            serde_json::json!([0, 1, 2])
        );
        assert_eq!(value["sessions"][0]["costUsd"], serde_json::json!(0.5));
        assert_eq!(
            value["sessions"][0]["cacheTtlSecs"],
            serde_json::json!(3600)
        );

        // The unsupported document still carries both keys, so the client
        // never has to tell "absent" from "false".
        let empty = render_sessions_json(false, &[], None, None);
        let value: serde_json::Value = serde_json::from_str(&empty).expect("valid JSON");
        assert_eq!(value["supported"], serde_json::json!(false));
        assert_eq!(value["sessions"], serde_json::json!([]));
        assert!(
            value.get("heat").is_none(),
            "no heat key without a reading: {value}"
        );
    }

    /// The server's heat reading crosses `tcr sessions --json` whole, under the
    /// camelCase keys `FleetStatus.swift`'s `HeatReading` decodes.
    #[test]
    fn sessions_json_carries_the_heat_reading() {
        let heat = tcr_status_wire::HeatRow {
            cpu_celsius: Some(61.0),
            fans: vec![tcr_status_wire::FanRow {
                index: 1,
                actual_rpm: 5013.0,
                min_rpm: 1350.0,
                max_rpm: 5777.0,
                manual: false,
                target_rpm: None,
            }],
            ftst: None,
        };
        let rendered = render_sessions_json(true, &[], Some(&heat), None);
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            value["heat"],
            serde_json::json!({
                "cpuCelsius": 61.0,
                "fans": [{"index": 1, "actualRpm": 5013.0, "minRpm": 1350.0,
                          "maxRpm": 5777.0, "manual": false}],
            })
        );
        assert_eq!(value["sessions"], serde_json::json!([]));
        assert!(
            value.get("fanControl").is_none(),
            "no key without a controller"
        );
    }

    /// `fanControl` crosses `tcr sessions --json` whole, beside `heat`, exactly as the
    /// committed contract sample spells it: the object TcrBar's popover decodes.
    #[test]
    fn fan_status_crosses_sessions_json_as_the_contract_says() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fan-control-contract.json");
        let samples: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let sample = samples["holdingFixed5000"].clone();
        let row: tcr_status_wire::FanControlRow = serde_json::from_value(sample.clone()).unwrap();
        let rendered = render_sessions_json(true, &[], None, Some(&row));
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(value["fanControl"], sample);
    }

    /// `tcr fan`'s text names the wish, the state, each fan's mode key and target, and
    /// `Ftst`: what Gil's hardware checks read.
    #[test]
    fn fan_status_text_names_every_fact_the_hardware_checks_read() {
        let row = tcr_status_wire::FanControlRow {
            desired: tcr_status_wire::FanDesiredRow::Fixed { rpm: 5000 },
            state: tcr_status_wire::FanStateRow::Foreign,
            reason: Some(tcr_status_wire::FanReasonRow::MacsFanControl),
            hot_guard: false,
            targets: vec![],
            detail: None,
            command: 0,
            since_ms: None,
        };
        let heat = tcr_status_wire::HeatRow {
            cpu_celsius: Some(61.0),
            fans: vec![tcr_status_wire::FanRow {
                index: 0,
                actual_rpm: 5013.0,
                min_rpm: 1350.0,
                max_rpm: 5777.0,
                manual: true,
                target_rpm: Some(5000.0),
            }],
            ftst: Some(false),
        };
        assert_eq!(
            render_fan_text(&row, Some(&heat)),
            "desired=fixed 5000 rpm\nstate=foreign\nreason=macsFanControl\nhotGuard=off\n\
             cpu=61.0°C\nfan=0 actual=5013 min=1350 max=5777 mode=manual target=5000\nftst=0\n"
        );
    }

    /// A reason's word is its wire name without its data, for the ones that carry data too:
    /// what `reason=` and the failure line print.
    #[test]
    fn fan_reason_word_is_the_wire_name_without_its_data() {
        use tcr_status_wire::FanReasonRow;
        for (reason, word) in [
            (FanReasonRow::MacsFanControl, "macsFanControl"),
            (FanReasonRow::AnotherApp, "anotherApp"),
            (FanReasonRow::NotSignedForFans, "notSignedForFans"),
            (FanReasonRow::NoSmc, "noSmc"),
            (FanReasonRow::NoFans, "noFans"),
            (FanReasonRow::HelperUnreachable, "helperUnreachable"),
            (FanReasonRow::HelperNoReply, "helperNoReply"),
            (FanReasonRow::HelperSignature, "helperSignature"),
            (
                FanReasonRow::HelperVersion { installed: None },
                "helperVersion",
            ),
            (
                FanReasonRow::HelperVersion { installed: Some(2) },
                "helperVersion",
            ),
            (FanReasonRow::ModeLocked(1), "modeLocked"),
            (FanReasonRow::Contended, "contended"),
            (FanReasonRow::HelperRefused, "helperRefused"),
            (FanReasonRow::ControllerStopped, "controllerStopped"),
        ] {
            assert_eq!(fan_word(&reason), word, "{reason:?}");
        }
    }

    /// A status row for the `tcr fan` tests, under `command`, the newest command acted on.
    fn fan_row(
        desired: tcr_status_wire::FanDesiredRow,
        state: tcr_status_wire::FanStateRow,
        command: u64,
    ) -> tcr_status_wire::FanControlRow {
        tcr_status_wire::FanControlRow {
            desired,
            state,
            reason: None,
            hot_guard: false,
            targets: vec![],
            detail: None,
            command,
            since_ms: None,
        }
    }

    /// `tcr fan fixed 5000` succeeds only for fixed 5000 itself: holding another mode, even
    /// one newer than the command, is not its success.
    #[test]
    fn fan_cli_succeeds_only_for_the_mode_it_sent() {
        use crate::fan::FanRequest;
        use tcr_status_wire::{FanDesiredRow, FanStateRow};
        let sent = FanSent {
            request: FanRequest::Fixed { rpm: 5000 },
            command: 1_000,
        };
        let ours = FanDesiredRow::Fixed { rpm: 5000 };
        let other = FanDesiredRow::Fixed { rpm: 3000 };

        let settled = fan_row(ours, FanStateRow::Holding, 1_200);
        assert_eq!(fan_wait(&sent, &settled), FanWait::Settled);
        assert!(fan_succeeded(&sent, &settled));

        let replaced = fan_row(other, FanStateRow::Holding, 1_200);
        assert_eq!(fan_wait(&sent, &replaced), FanWait::Replaced);
        assert!(!fan_succeeded(&sent, &replaced));

        // Not acted on yet, or still entering: wait.
        let before = fan_row(ours, FanStateRow::Holding, 900);
        assert_eq!(fan_wait(&sent, &before), FanWait::Waiting);
        assert!(!fan_succeeded(&sent, &before));
        let entering = fan_row(ours, FanStateRow::Entering, 1_200);
        assert_eq!(fan_wait(&sent, &entering), FanWait::Waiting);

        // Settled, but not holding.
        let foreign = fan_row(ours, FanStateRow::Foreign, 1_200);
        assert_eq!(fan_wait(&sent, &foreign), FanWait::Settled);
        assert!(!fan_succeeded(&sent, &foreign));

        // `auto --force` is Auto.
        let forced = FanSent {
            request: FanRequest::Auto { force: true },
            command: 1_000,
        };
        let off = fan_row(FanDesiredRow::Auto, FanStateRow::Off, 1_200);
        assert!(fan_succeeded(&forced, &off));
    }

    /// The case the command counter exists for (Gil, 2026-10-19): a later command that asks
    /// for the very wish from before this one is a replacement, said at once, never a wait for
    /// this command that runs out at 49 s.
    #[test]
    fn fan_cli_a_return_to_the_wish_from_before_is_replaced_at_once() {
        use crate::fan::FanRequest;
        use tcr_status_wire::{FanDesiredRow, FanStateRow};
        let sent = FanSent {
            request: FanRequest::Fixed { rpm: 5000 },
            command: 1_000,
        };
        // Before: Auto, as acted on under 999. Then `auto` again, number 1_001, acted on.
        let before = fan_row(FanDesiredRow::Auto, FanStateRow::Off, 999);
        assert_eq!(fan_wait(&sent, &before), FanWait::Waiting);
        let auto_again = fan_row(FanDesiredRow::Auto, FanStateRow::Off, 1_001);
        assert_eq!(fan_wait(&sent, &auto_again), FanWait::Replaced);
        assert!(!fan_succeeded(&sent, &auto_again));
    }

    /// Two commands drained in one step are acted on under the later number: the earlier one
    /// is judged by the wish shown, its own (settled by the state) or the later one's
    /// (replaced).
    #[test]
    fn fan_cli_a_drained_pair_settles_the_earlier_command_by_its_wish() {
        use crate::fan::FanRequest;
        use tcr_status_wire::{FanDesiredRow, FanStateRow};
        let earlier = FanSent {
            request: FanRequest::Fixed { rpm: 5000 },
            command: 1_000,
        };
        let same = fan_row(
            FanDesiredRow::Fixed { rpm: 5000 },
            FanStateRow::Holding,
            1_001,
        );
        assert_eq!(fan_wait(&earlier, &same), FanWait::Settled);
        assert!(fan_succeeded(&earlier, &same));
        let other = fan_row(
            FanDesiredRow::Fixed { rpm: 3000 },
            FanStateRow::Holding,
            1_001,
        );
        assert_eq!(fan_wait(&earlier, &other), FanWait::Replaced);
    }

    /// A server restarted after the command went in numbers from its own start, above the
    /// command's number, so the command reads as acted on; the wish the restart reapplied (the
    /// one the command saved before the restart) decides.
    #[test]
    fn fan_cli_a_restarted_server_settles() {
        use crate::fan::FanRequest;
        use tcr_status_wire::{FanDesiredRow, FanStateRow};
        let sent = FanSent {
            request: FanRequest::Fixed { rpm: 5000 },
            command: 1_760_000_000_001,
        };
        let restarted = 1_760_000_090_000;
        let held = fan_row(
            FanDesiredRow::Fixed { rpm: 5000 },
            FanStateRow::Holding,
            restarted,
        );
        assert_eq!(fan_wait(&sent, &held), FanWait::Settled);
        assert!(fan_succeeded(&sent, &held));
        let reentering = fan_row(
            FanDesiredRow::Fixed { rpm: 5000 },
            FanStateRow::Entering,
            restarted,
        );
        assert_eq!(fan_wait(&sent, &reentering), FanWait::Waiting);
    }

    /// A server older than the counter answers the POST with its whole `fanControl` object and
    /// writes no `command` in its status. `tcr fan` says so and fails, and reads no other
    /// field in its place.
    #[test]
    fn fan_cli_a_server_without_the_counter_says_so() {
        let old_answer = r#"{"desired":{"mode":"auto"},"state":"off","reason":null,
            "hotGuard":false,"targets":[],"detail":null,"sinceMs":1760000000000}"#;
        let accepted: crate::fan::FanAccepted = serde_json::from_str(old_answer).unwrap();
        let refused = accepted_command(accepted, 3456).unwrap_err().to_string();
        assert!(
            refused.contains("older than this tcr's fan command counter"),
            "{refused}"
        );
        assert!(refused.contains("127.0.0.1:3456"), "{refused}");

        let ours: crate::fan::FanAccepted = serde_json::from_str(r#"{"command":7}"#).unwrap();
        assert_eq!(accepted_command(ours, 3456).unwrap(), 7);
        // This build's 202: the whole object from before the command, with its number.
        let full = r#"{"desired":{"mode":"auto"},"state":"off","reason":null,
            "hotGuard":false,"targets":[],"detail":null,"command":8,"sinceMs":1760000000000}"#;
        let ours: crate::fan::FanAccepted = serde_json::from_str(full).unwrap();
        assert_eq!(accepted_command(ours, 3456).unwrap(), 8);

        // Its status object does not read as one, so nothing is judged from it.
        assert!(serde_json::from_str::<tcr_status_wire::FanControlRow>(old_answer).is_err());
    }

    /// `tcr fan auto` with the fans held at 5000. The status keeps the old wish and the old
    /// number until the controller acts, so the snapshot from taking the command, and a state
    /// the old wish reached after the send (the SMC not reading: unknown, still under the old
    /// number), both wait; releasing waits; `off` succeeds. A third wish is Replaced.
    #[test]
    fn fan_cli_auto_from_holding_waits_through_the_release() {
        use crate::fan::FanRequest;
        use tcr_status_wire::{FanDesiredRow, FanStateRow};
        let held = FanDesiredRow::Fixed { rpm: 5000 };
        let sent = FanSent {
            request: FanRequest::Auto { force: false },
            command: 1_000,
        };
        let taken = fan_row(held, FanStateRow::Holding, 999);
        assert_eq!(fan_wait(&sent, &taken), FanWait::Waiting);
        assert!(!fan_succeeded(&sent, &taken));
        let old_wish_moved = fan_row(held, FanStateRow::Unknown, 999);
        assert_eq!(fan_wait(&sent, &old_wish_moved), FanWait::Waiting);
        let releasing = fan_row(FanDesiredRow::Auto, FanStateRow::Releasing, 1_000);
        assert_eq!(fan_wait(&sent, &releasing), FanWait::Waiting);
        let off = fan_row(FanDesiredRow::Auto, FanStateRow::Off, 1_000);
        assert_eq!(fan_wait(&sent, &off), FanWait::Settled);
        assert!(fan_succeeded(&sent, &off));

        let replaced = fan_row(
            FanDesiredRow::Fixed { rpm: 3000 },
            FanStateRow::Holding,
            1_200,
        );
        assert_eq!(fan_wait(&sent, &replaced), FanWait::Replaced);
        assert!(!fan_succeeded(&sent, &replaced));
    }

    /// A controller that stopped on a panic keeps the command number it had, below this
    /// command's, and never acts again: `tcr fan` ends its wait at the first status that says
    /// so, whatever the wish and number shown, and fails with that reason.
    #[test]
    fn fan_cli_a_stopped_controller_ends_the_wait_at_once() {
        use crate::fan::FanRequest;
        use tcr_status_wire::{FanDesiredRow, FanReasonRow, FanStateRow};
        for request in [
            FanRequest::Fixed { rpm: 5000 },
            FanRequest::Auto { force: false },
        ] {
            let sent = FanSent {
                request,
                command: 1_000,
            };
            for (desired, command) in [
                (FanDesiredRow::Auto, 999),
                (FanDesiredRow::Fixed { rpm: 3000 }, 999),
                (request.desired_row(), 1_000),
            ] {
                let mut stopped = fan_row(desired, FanStateRow::Unavailable, command);
                stopped.reason = Some(FanReasonRow::ControllerStopped);
                assert_eq!(
                    fan_wait(&sent, &stopped),
                    FanWait::Stopped,
                    "{request:?} {desired:?} {command}"
                );
                assert!(!fan_succeeded(&sent, &stopped));
            }
        }
        // Another reason under an old number is still a wait: the controller may yet act.
        let sent = FanSent {
            request: FanRequest::Fixed { rpm: 5000 },
            command: 1_000,
        };
        let mut silent = fan_row(FanDesiredRow::Auto, FanStateRow::Unavailable, 999);
        silent.reason = Some(FanReasonRow::HelperNoReply);
        assert_eq!(fan_wait(&sent, &silent), FanWait::Waiting);
    }

    /// The wait covers the version call (which waits out launchd starting the helper late),
    /// two worst-case set-manuals and two set-targets.
    #[test]
    fn fan_cli_waits_through_two_worst_case_set_manuals() {
        use tcr_fan::controller::{SET_MANUAL_DEADLINE, SET_TARGET_DEADLINE, VERSION_DEADLINE};
        assert!(FAN_SETTLE >= VERSION_DEADLINE + 2 * SET_MANUAL_DEADLINE + 2 * SET_TARGET_DEADLINE);
        assert_eq!(FAN_SETTLE, Duration::from_secs(49));
    }

    /// The fallback half of the same contract: with nothing listening, the live
    /// read reports `NoServer` (the ordinary case, which warns about nothing) and
    /// `tcr status` keeps working exactly as before — just labelled `offline`.
    ///
    /// # Why it retries
    ///
    /// The port is reserved by binding and dropping, so between that drop and
    /// the request it belongs to nobody, and under a full `cargo test`, where
    /// other binaries are opening sockets at the same time, another process
    /// takes it. This test then reaches something that ANSWERS, which is a
    /// different error and a correct one, and the assertion fails for a reason
    /// that has nothing to do with the classification it guards. Measured: it
    /// went red exactly this way in one of three full-suite runs, with
    /// `error sending request for url (http://127.0.0.1:64013/_tcr/status)`.
    ///
    /// So a lost port is retried on a fresh one, five times at most. The ceiling
    /// keeps the race visible: losing five in a row is a runner with no free
    /// ports, which is a defect to see rather than to retry past. The same
    /// shape, and the same reason, as `Mac::boot` in `tests/peer_e2e.rs`.
    #[tokio::test]
    async fn live_status_falls_back_when_no_server_answers() {
        /// How many times this may lose the port before it is a failure.
        const ATTEMPTS: usize = 5;

        let mut lost = 0;
        loop {
            // Bind and immediately drop, so the port is free and refuses.
            let port = crate::test_support::loopback::unused_port();
            let mut config = load_from(TWO_ACCOUNTS);
            config.proxy.port = port;

            match fetch_live_status(&config).await {
                Err(LiveStatusError::NoServer) => return,
                Err(err) => {
                    lost += 1;
                    assert!(
                        lost < ATTEMPTS,
                        "five loopback ports in a row were taken by something else between \
                         the reserve and the request, so this box has no free ports rather \
                         than a race: {}",
                        err.why()
                    );
                }
                Ok(_) => panic!("something is listening on {port} and answered a status payload"),
            }
        }
    }

    // --- incumbent liveness -------------------------------------------------

    /// THE CASE THAT MUST NOT BE A TAKEOVER. A healthy proxy whose api-key we do
    /// not have answers `401` — it accepted the connection, routed the request
    /// and wrote a response, which is the opposite of wedged. `build` is `None`
    /// here exactly as it is for a wedged proxy, so a stand-down that decided on
    /// `build.is_none()` would SIGKILL a server that is serving every request
    /// fine. The liveness verdict is what separates them.
    #[tokio::test]
    async fn a_rejected_api_key_is_an_answering_incumbent_not_a_wedged_one() {
        let (listener, port) = crate::test_support::loopback::listener().await;
        let mut served = load_from(TWO_ACCOUNTS);
        served.proxy.port = port;
        served.proxy.api_key = Some("the-real-key".to_string());

        crate::test_support::live_proxy::serve(listener, served.clone(), None);

        // Same port, WRONG key — what an operator with a stale config has.
        let mut probing = served.clone();
        probing.proxy.api_key = Some("the-wrong-key".to_string());
        let probe = probe_incumbent(&probing).await;

        assert!(
            probe.build.is_none(),
            "the build read fails, which is precisely why it cannot be the liveness signal"
        );
        assert_eq!(
            probe.liveness,
            Liveness::Answering,
            "a 401 is a serving process; taking its port would be a takeover of a healthy proxy"
        );
    }

    /// The WEDGED shape: something holds the listening socket, the connect even
    /// succeeds off the kernel backlog, and no response is ever written. This is
    /// the state a deadlocked proxy leaves the port in, and the one the startup
    /// stand-down has to be able to name — before this, `tcr` reported it as a
    /// healthy incumbent and exited 0, and the port was unrecoverable.
    ///
    /// Costs the probe's own 5s deadline by construction: the assertion IS that
    /// we wait for it and then call it silent.
    #[tokio::test]
    async fn a_listener_that_never_answers_is_silent_not_answering() {
        // Bound and never accepted: connections queue in the backlog forever.
        let (listener, port) = crate::test_support::loopback::listener().await;
        let mut config = load_from(TWO_ACCOUNTS);
        config.proxy.port = port;

        let probe = probe_incumbent(&config).await;
        assert!(probe.build.is_none());
        match probe.liveness {
            Liveness::Silent { why } => assert!(
                !why.is_empty(),
                "the operator-facing line needs the probe's own account of what happened"
            ),
            Liveness::Answering => {
                panic!("a socket that never wrote a byte is not an answering proxy")
            }
        }
        drop(listener);
    }

    /// The other WEDGED shape: the connect itself runs into its 2s deadline,
    /// because the SYN is dropped rather than refused. reqwest reports that
    /// error as BOTH `is_connect()` and `is_timeout()`, so a sort that asks
    /// `is_connect()` first files a wedged proxy as "nothing is listening", and
    /// `tcr status` falls back offline without a word. The status read had that
    /// order until it shared [`send_loopback`] with the control routes.
    ///
    /// A loopback port cannot stage it: a full accept queue on macOS answers
    /// with a reset, not silence. So the request goes to a TEST-NET-3 address
    /// (RFC 5737), which nobody answers, and costs the 2s connect deadline.
    #[tokio::test]
    async fn a_connect_timeout_is_no_answer_not_no_server() {
        let config = load_from(TWO_ACCOUNTS);
        let started = std::time::Instant::now();
        let sent = send_loopback(&config, crate::proxy::STATUS_PATH, |client, _| {
            client.get("http://203.0.113.10:41000/_tcr/status")
        })
        .await;
        let waited = started.elapsed();
        match sent.map_err(LiveStatusError::from) {
            Err(LiveStatusError::NoAnswer(_)) => {}
            Err(other) => panic!(
                "a connect that ran into its deadline must be NoAnswer, got {other:?} after \
                 {waited:?} (well under 2s means this box has no route to TEST-NET-3, which \
                 refuses instead of blackholing)"
            ),
            Ok(response) => panic!("TEST-NET-3 answered: HTTP {}", response.status()),
        }
    }

    /// Every verb that asks the running proxy first says what it said when each
    /// verb spelled out its own refusals and warnings, word for word. The one
    /// planned difference is `tcr priority`'s "too old" warning, which named a
    /// `tcr restart` subcommand that does not exist and now ends in
    /// [`RESTART_HINT`] like the rest.
    #[test]
    fn each_live_verb_keeps_its_own_refusal_and_warning_words() {
        let mut config = load_from(TWO_ACCOUNTS);
        config.proxy.port = 3456;
        let path = Path::new("/x/config.json");
        let refusal = |words: &'static LiveWords, error: LiveControlError| {
            let site = LiveSite {
                config: &config,
                config_path: path,
                words,
            };
            match site.decide::<()>(Err(error)) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("a rejected key or a refused query must not reach the file"),
            }
        };
        let warning = |words: &'static LiveWords, stale: Stale, keeps: Option<&str>| {
            LiveSite {
                config: &config,
                config_path: path,
                words,
            }
            .warning(&stale, keeps)
        };
        let message = "no account matches 'zed'.";
        let why = "HTTP 500 Internal Server Error";

        struct Case {
            words: &'static LiveWords,
            keeps: Option<&'static str>,
            unauthorized: &'static str,
            rejected: String,
            too_old: String,
            failed: String,
        }
        let cases = [
            Case {
                words: &REMOVE_WORDS,
                keeps: Some("routing to 'alice'"),
                unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json — the config was NOT changed, because deleting it would leave the running proxy still rotating this account. Fix `proxy.apiKey` and retry.",
                rejected: format!(
                    "the proxy running on :3456 refused this: {message} Nothing was changed."
                ),
                too_old: format!(
                    "[tcr] WARNING: the proxy running on :3456 is too old to accept live account control (no /_tcr/accounts/disabled route), so only the config file was changed. It will KEEP routing to 'alice' until it restarts. {RESTART_HINT}"
                ),
                failed: format!(
                    "[tcr] WARNING: could not apply this to the proxy running on :3456 ({why}), so only the config file was changed. It may KEEP routing to 'alice' until it restarts."
                ),
            },
            Case {
                words: &PRIORITY_WORDS,
                keeps: Some("the old order"),
                unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json, so the config was NOT changed, because writing it would leave the running proxy on the old order. Fix `proxy.apiKey` and retry.",
                rejected: format!(
                    "the proxy running on :3456 refused this: {message} Nothing was changed."
                ),
                too_old: format!(
                    "[tcr] WARNING: the proxy running on :3456 is too old to accept a live priority change (no /_tcr/accounts/priority route), so only the config file was changed. It will KEEP the old order until it restarts. {RESTART_HINT}"
                ),
                failed: format!(
                    "[tcr] WARNING: could not apply this to the proxy running on :3456 ({why}), so only the config file was changed. It may KEEP the old order until it restarts."
                ),
            },
            Case {
                words: &CONFIG_SET_WORDS,
                keeps: None,
                unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json, so nothing was changed: writing the file beside a running proxy can lose the change to its own next write. Fix `proxy.apiKey` and retry.",
                rejected: format!("the proxy running on :3456 refused this: {message}"),
                too_old: format!(
                    "[tcr] WARNING: the proxy running on :3456 is too old to save settings itself (no /_tcr/config route), so this process wrote the file beside it. {RESTART_HINT}"
                ),
                failed: format!(
                    "[tcr] WARNING: could not hand this to the proxy running on :3456 ({why}), so this process wrote the file beside it."
                ),
            },
            Case {
                words: &ENABLE_WORDS,
                keeps: Some("benching 'alice'"),
                unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json — the config was NOT changed, because writing it would leave the running proxy still rotating this account. Fix `proxy.apiKey` and retry.",
                rejected: format!(
                    "the proxy running on :3456 refused this: {message} Nothing was changed."
                ),
                too_old: format!(
                    "[tcr] WARNING: the proxy running on :3456 is too old to accept live account control (no /_tcr/accounts/disabled route), so only the config file was changed. It will KEEP benching 'alice' until it restarts. {RESTART_HINT}"
                ),
                failed: format!(
                    "[tcr] WARNING: could not apply this to the proxy running on :3456 ({why}), so only the config file was changed. It may KEEP benching 'alice' until it restarts."
                ),
            },
            Case {
                words: &CONTROL_WORDS,
                keeps: Some("its OLD control account"),
                unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json — the config was NOT changed, because writing it would leave the running proxy still resolving identity traffic against its old control account. Fix `proxy.apiKey` and retry.",
                rejected: format!(
                    "the proxy running on :3456 refused this: {message} Nothing was changed."
                ),
                too_old: format!(
                    "[tcr] WARNING: the proxy running on :3456 is too old to accept live account control (no /_tcr/accounts/control route), so only the config file was changed. It will KEEP its OLD control account until it restarts. {RESTART_HINT}"
                ),
                failed: format!(
                    "[tcr] WARNING: could not apply this to the proxy running on :3456 ({why}), so only the config file was changed. It may KEEP its OLD control account until it restarts."
                ),
            },
        ];
        for case in cases {
            let route = case.words.route_path;
            assert_eq!(
                refusal(case.words, LiveControlError::Unauthorized),
                case.unauthorized,
                "{route}"
            );
            assert_eq!(
                refusal(case.words, LiveControlError::Rejected(message.to_string())),
                case.rejected,
                "{route}"
            );
            assert_eq!(
                warning(case.words, Stale::NoRoute, case.keeps),
                case.too_old,
                "{route}"
            );
            assert_eq!(
                warning(case.words, Stale::Failed(why.to_string()), case.keeps),
                case.failed,
                "{route}"
            );
        }
    }

    /// The decision itself: nothing listening is a quiet file write, a missing
    /// route and an unusable answer are a warned one, and an answer is applied.
    #[test]
    fn the_live_decision_writes_the_file_only_where_it_may() {
        let config = load_from(TWO_ACCOUNTS);
        let site = LiveSite {
            config: &config,
            config_path: Path::new("/x/config.json"),
            words: &ENABLE_WORDS,
        };
        assert!(matches!(site.decide(Ok(7)), Ok(Live::Applied(7))));
        assert!(matches!(
            site.decide::<()>(Err(LiveControlError::NoServer)),
            Ok(Live::FileOnly(None))
        ));
        assert!(matches!(
            site.decide::<()>(Err(LiveControlError::NoRoute)),
            Ok(Live::FileOnly(Some(Stale::NoRoute)))
        ));
        assert!(matches!(
            site.decide::<()>(Err(LiveControlError::NoAnswer("late".into()))),
            Ok(Live::FileOnly(Some(Stale::Failed(why)))) if why == "late"
        ));
        assert!(matches!(
            site.decide::<()>(Err(LiveControlError::Unusable("HTTP 500".into()))),
            Ok(Live::FileOnly(Some(Stale::Failed(why)))) if why == "HTTP 500"
        ));
        assert!(
            site.decide::<()>(Err(LiveControlError::Unauthorized))
                .is_err()
        );
        assert!(
            site.decide::<()>(Err(LiveControlError::Rejected("no".into())))
                .is_err()
        );
    }

    /// The classification the two tests above exercise, pinned directly so a
    /// future variant cannot be added on the wrong side of it by accident.
    #[test]
    fn only_an_http_response_counts_as_answered() {
        assert!(
            !LiveStatusError::NoServer.answered(),
            "nothing listening answered nothing"
        );
        assert!(
            !LiveStatusError::NoAnswer("the server did not answer within 5s".into()).answered(),
            "a deadline with no bytes back is the wedged shape"
        );
        assert!(
            LiveStatusError::Unusable("HTTP 401 Unauthorized".into()).answered(),
            "an HTTP response — any HTTP response — means the process is serving"
        );
    }

    // --- no-persist guarantee ----------------------------------------------

    #[tokio::test]
    async fn status_offline_does_not_persist_the_config() {
        // A scripted prober returns usage; snapshot_offline builds a manager with
        // config_path = None, so nothing is written back — the temp config must
        // be byte-unchanged after a full probe sweep.
        let path = write_config("no-persist", TWO_ACCOUNTS);
        let before = fs::read_to_string(&path).unwrap();
        let config = load(&path);
        let _snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.5 }),
            true,
        )
        .await;
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an offline status probe must never persist to the config file"
        );
        fs::remove_file(&path).ok();
    }

    // --- `tcr status --check` ----------------------------------------------

    /// The `--check` rule, one probe outcome at a time, on a row produced by
    /// the real offline snapshot (a 401 from the scripted prober reads `error`)
    /// and then re-labelled per case. The positive control comes first: the
    /// unmodified row must fail the check, or every `Clean` below is vacuous.
    #[tokio::test]
    async fn status_check_fails_only_on_an_enabled_account_whose_probe_failed() {
        use crate::probe::ProbeStatus;
        let path = write_config("status-check", EXPIRED_ACCOUNT);
        let snapshot = snapshot_offline(
            load(&path),
            Arc::new(NoRefresh),
            Arc::new(RejectingProber),
            true,
        )
        .await;
        fs::remove_file(&path).ok();
        assert_eq!(snapshot.accounts[0].probe_status, ProbeStatus::Error);
        assert_eq!(StatusCheck::of(&snapshot), StatusCheck::ProbeFailed);
        assert_eq!(StatusCheck::ProbeFailed.exit_code(), 4);
        assert_eq!(StatusCheck::Clean.exit_code(), 0);

        let relabel = |probe: ProbeStatus, disabled: bool| {
            let mut s = snapshot.clone();
            s.accounts[0].probe_status = probe;
            s.accounts[0].disabled = disabled;
            StatusCheck::of(&s)
        };
        for failed in [
            ProbeStatus::Error,
            ProbeStatus::Timeout,
            ProbeStatus::UpstreamDown,
        ] {
            assert_eq!(
                relabel(failed, false),
                StatusCheck::ProbeFailed,
                "{failed:?}"
            );
            assert_eq!(
                relabel(failed, true),
                StatusCheck::Clean,
                "a disabled account never fails the check ({failed:?})"
            );
        }
        for benign in [
            ProbeStatus::Never,
            ProbeStatus::Ok,
            ProbeStatus::RateLimited,
        ] {
            assert_eq!(relabel(benign, false), StatusCheck::Clean, "{benign:?}");
        }
    }

    /// **`tcr status --check` counts a Codex row's failed probe like any other.** The
    /// rule is "an enabled account's last probe failed", and a Codex account is probed
    /// at its own provider's usage endpoint, so its failure is a probe failure of the
    /// fleet: exit 4. The same row disabled is out of the rule, as an Anthropic one is.
    /// Built through the real offline snapshot over a prober that refuses only Codex
    /// credentials, so the row that fails is failed by its OWN provider's probe, and the
    /// Anthropic row's healthy probe is the control.
    ///
    /// Watched red: exempt a Codex row in `probe_failed`, and the enabled case reads
    /// clean.
    #[tokio::test]
    async fn status_check_counts_an_enabled_codex_rows_failed_probe() {
        struct CodexRefusingProber {
            seen: Arc<std::sync::Mutex<Vec<Provider>>>,
        }
        impl UsageProber for CodexRefusingProber {
            fn probe(&self, credential: crate::probe::ProbeCredential) -> ProbeFuture {
                let provider = credential.provider;
                self.seen.lock().expect("seen lock poisoned").push(provider);
                Box::pin(async move {
                    match provider {
                        Provider::Anthropic => Ok(Usage {
                            five_hour: crate::probe::WindowReading::Read(UsageBucket {
                                utilization: Some(0.2),
                                reset_at_ms: None,
                            }),
                            seven_day: None,
                            seven_day_oi: None,
                            extra_usage_usd: None,
                            codex: None,
                            claude_resets: None,
                        }),
                        Provider::Codex => Err(crate::probe::ProbeError {
                            status: Some(401),
                            message: "HTTP 401: token rejected".to_string(),
                            retry_after_secs: None,
                        }),
                    }
                })
            }
        }

        for (disabled, want) in [
            (false, StatusCheck::ProbeFailed),
            (true, StatusCheck::Clean),
        ] {
            let config: Config = serde_json::from_str(&format!(
                r#"{{ "accounts": [
                     {{ "name": "alice@example.com", "accessToken": "at-a", "refreshToken": "rt-a",
                        "expiresAt": 4102444800000 }},
                     {{ "name": "bob@example.com/codex", "provider": "codex", "accessToken": "at-b",
                        "refreshToken": "rt-b", "expiresAt": 4102444800000,
                        "accountUuid": "22222222-2222-4222-8222-222222222222", "disabled": {disabled} }}
                   ] }}"#
            ))
            .expect("a test config parses");
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let snapshot = snapshot_offline(
                config,
                Arc::new(NoRefresh),
                Arc::new(CodexRefusingProber {
                    seen: Arc::clone(&seen),
                }),
                true,
            )
            .await;
            assert_eq!(
                snapshot.accounts[0].probe_status,
                ProbeStatus::Ok,
                "control: the Anthropic row's own probe is healthy"
            );
            assert_eq!(StatusCheck::of(&snapshot), want, "disabled: {disabled}");
            if !disabled {
                assert_eq!(snapshot.accounts[1].provider, Provider::Codex);
                assert_eq!(snapshot.accounts[1].probe_status, ProbeStatus::Error);
                assert!(
                    seen.lock()
                        .expect("seen lock poisoned")
                        .contains(&Provider::Codex),
                    "the Codex row was probed with a Codex credential"
                );
            }
        }
    }

    // --- no-refresh guarantee (Fix 1) --------------------------------------

    /// A prober that always fails as if the usage endpoint rejected the token
    /// (HTTP 401) — the shape an EXPIRED account's stale access token gets once
    /// [`NoRefresh`] has declined to refresh it.
    struct RejectingProber;
    impl UsageProber for RejectingProber {
        fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
            Box::pin(async {
                Err(crate::probe::ProbeError {
                    status: Some(401),
                    message: "HTTP 401: token expired".to_string(),
                    retry_after_secs: None,
                })
            })
        }
    }

    /// An INSTRUMENTED refresher: records every refresh attempt and, like
    /// [`NoRefresh`], never contacts the network. Injected into `snapshot_offline`
    /// to prove the offline snapshot routes an expired account's refresh through the
    /// caller's refresher — never a hidden [`crate::oauth::LiveRefresher`] that would
    /// hit the OAuth endpoint and revoke the live server's single-use token.
    struct RecordingRefresher {
        calls: Arc<AtomicU64>,
    }
    impl TokenRefresher for RecordingRefresher {
        fn refresh(&self, _provider: Provider, _refresh_token: String) -> RefreshFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(OAuthError::Transient(
                    "recorded — never networked".to_string(),
                ))
            })
        }
    }

    /// One OAuth account whose token expired in 1970 — probing it would, before the
    /// fix, trigger a real OAuth refresh that revokes the running server's copy.
    const EXPIRED_ACCOUNT: &str = r#"{
      "proxy": { "port": 3456 },
      "accounts": [
        { "name": "stale@example.com", "type": "oauth", "orgName": "Org X",
          "orgUuid": "uuid-x", "accessToken": "at-stale", "refreshToken": "rt-stale",
          "expiresAt": 1000, "priority": 0 }
      ]
    }"#;

    #[tokio::test]
    async fn offline_snapshot_over_expired_account_routes_through_injected_refresher() {
        // Biting test for Fix 1: over an EXPIRED account, the offline snapshot must
        // hand the refresh to the INJECTED refresher (production wires NoRefresh,
        // which never networks) rather than a hidden LiveRefresher. A recording
        // refresher proves the routing — if snapshot_offline ignored its refresher
        // param and hardcoded LiveRefresher, `calls` would stay 0 and this fails.
        // The row must still surface a clear, greppable probe=error, and the config
        // stays byte-identical.
        let path = write_config("expired", EXPIRED_ACCOUNT);
        let before = fs::read_to_string(&path).unwrap();
        let config = load(&path);
        let calls = Arc::new(AtomicU64::new(0));
        let refresher = Arc::new(RecordingRefresher {
            calls: Arc::clone(&calls),
        });
        let snapshot = snapshot_offline(config, refresher, Arc::new(RejectingProber), true).await;
        let text = render_accounts(&snapshot, StatusSource::Offline);
        assert!(
            text.contains("stale@example.com"),
            "renders the account: {text}"
        );
        assert!(
            text.contains("probe=error"),
            "surfaces a clear probe status for the expired account: {text}"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "the expired account's refresh was routed through the injected refresher, \
             never a hidden LiveRefresher (which would have hit the OAuth endpoint)"
        );
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an offline probe of an expired account must never persist"
        );
        fs::remove_file(&path).ok();
    }

    // --- reset countdowns (3a) ---------------------------------------------

    #[test]
    fn reset_countdown_tiers_minutes_then_hours_then_days() {
        let now = OffsetDateTime::now_utc();
        use time::Duration;
        // Minutes under two hours (matches the spec example `+92m`).
        assert_eq!(
            format_reset_countdown(now + Duration::minutes(5), now),
            "+5m"
        );
        assert_eq!(
            format_reset_countdown(now + Duration::minutes(92), now),
            "+92m"
        );
        // Hours from two hours up to two days.
        assert_eq!(format_reset_countdown(now + Duration::hours(5), now), "+5h");
        // Days beyond two days.
        assert_eq!(format_reset_countdown(now + Duration::days(3), now), "+3d");
        assert_eq!(format_reset_countdown(now + Duration::days(7), now), "+7d");
        // A reset that already elapsed clamps to +0m, never negative.
        assert_eq!(
            format_reset_countdown(now - Duration::minutes(5), now),
            "+0m"
        );
    }

    /// `free_in=` answers "when does it come back" — the question
    /// `status=throttled` cannot. It lives on THIS surface because the TUI's
    /// Status column is Length(9) and "throttled" is exactly 9 characters, so a
    /// countdown there is clipped at every terminal width.
    ///
    /// Asserts the RENDERED LINE and the emitted JSON, never a formatting helper.
    /// A unit test on the helper is what let a clipped countdown ship green.
    #[tokio::test]
    async fn free_in_names_the_instant_and_is_omitted_when_none_can_be_promised() {
        // WindowProber gates alice on 5h (~+92m) and bob on 7d (+3d) — real gates
        // with known resets, so account_gate yields a free_at for both.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let gated =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
        let text = render_accounts(&gated, StatusSource::Live);
        for line in text.lines() {
            assert!(
                line.contains("free_in="),
                "a gated account names its instant: {line}"
            );
            let secs: i64 = line
                .split("free_in=")
                .nth(1)
                .and_then(|t| t.split('s').next())
                .and_then(|n| n.parse().ok())
                .expect("free_in carries whole seconds");
            assert!(secs > 0, "a promised instant is in the future: {line}");
        }
        // JSON mirrors it for machine consumers, in ms like every other instant.
        let json = status_json(&gated, &thresholds, StatusSource::Live);
        assert!(
            json.contains("\"freeAtMs\""),
            "json carries freeAtMs: {json}"
        );
        assert!(
            json.contains("\"secondsUntilFree\""),
            "and the countdown: {json}"
        );

        // UNGATED: nothing binds, so there is no instant to promise. The token is
        // OMITTED, never rendered as zero — `free_in=0s` would read as "returns
        // now", which is the same false promise `free_at = None` exists to avoid.
        let config = load_from(TWO_ACCOUNTS);
        let open = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(FixedProber { util: 0.25 }),
            true,
        )
        .await;
        let text = render_accounts(&open, StatusSource::Live);
        assert!(
            !text.contains("free_in="),
            "an account in rotation promises nothing: {text}"
        );
    }

    #[test]
    fn reset_clock_is_bare_hh_mm_utc() {
        // 18:50:00 UTC → "18:50Z" regardless of seconds.
        let reset = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
            + time::Duration::seconds(0);
        let clock = format_reset_clock(reset);
        assert!(clock.ends_with('Z'), "explicit UTC marker: {clock}");
        assert_eq!(clock.len(), 6, "HH:MMZ shape: {clock}");
        assert!(clock.contains(':'), "clock shape: {clock}");
    }

    /// A prober that drives one account over its 5h threshold and another over its
    /// 7d threshold, each with a future reset — so both windows read as held.
    struct WindowProber;
    impl UsageProber for WindowProber {
        fn probe(&self, credential: crate::probe::ProbeCredential) -> ProbeFuture {
            let now = crate::now_ms();
            Box::pin(async move {
                if credential.access_token == "at-a" {
                    Ok(Usage {
                        five_hour: crate::probe::WindowReading::Read(UsageBucket {
                            utilization: Some(0.95),
                            reset_at_ms: Some(now + 92 * 60 * 1000),
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                        extra_usage_usd: None,
                        codex: None,
                        claude_resets: None,
                    })
                } else {
                    Ok(Usage {
                        five_hour: crate::probe::WindowReading::Unread,
                        seven_day: Some(UsageBucket {
                            utilization: Some(0.97),
                            reset_at_ms: Some(now + 3 * 24 * 60 * 60 * 1000),
                        }),
                        seven_day_oi: None,
                        extra_usage_usd: None,
                        codex: None,
                        claude_resets: None,
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn render_accounts_shows_held_window_and_reset_for_over_threshold_accounts() {
        // 3a: a held account's binding window carries an inline `(+countdown)`,
        // gated on the same per-account threshold `eligible` uses.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
        let text = render_accounts(&snapshot, StatusSource::Live);
        let alice = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        let bob = text
            .lines()
            .find(|l| l.contains("bob@example.com"))
            .expect("bob line");
        // alice (at-a) is 5h-held at 0.95 with a ~+92m reset; her 5h window carries
        // the inline countdown while her (unlearned) weekly reads n/a. The exact
        // minute drifts with wall-clock between probe and render, so match the `(+`.
        assert!(alice.contains("5h=95%(+"), "5h hold + countdown: {alice}");
        assert!(alice.contains("wk=n/a"), "alice weekly unknown: {alice}");
        // bob (at-b) is 7d-held at 0.97 with a +3d reset; his weekly carries the
        // countdown while his (unlearned) 5h reads n/a.
        assert!(bob.contains("wk=97%(+"), "wk hold + countdown: {bob}");
        assert!(bob.contains("5h=n/a"), "bob 5h unknown: {bob}");
        // Neither is over its 1.0 exhaustion line, so both read `near`, not `spent`.
        assert!(alice.contains("state=near"), "alice near-limit: {alice}");
        assert!(bob.contains("state=near"), "bob near-limit: {bob}");
        // The legacy suffix format is gone from the text line.
        assert!(!text.contains("held="), "no legacy held= suffix: {text}");
        // Greppable one-line-per-account contract survives the new fields.
        assert_eq!(
            text.lines().count(),
            2,
            "still one line per account: {text}"
        );

        // JSON mirrors the held fields for machine consumers.
        let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
        assert!(json.contains("\"held\""), "json carries held array: {json}");
        assert!(
            json.contains("\"window\": \"5h\""),
            "json names the 5h window: {json}"
        );
        assert!(
            json.contains("\"window\": \"7d\""),
            "json names the 7d window: {json}"
        );
        assert!(
            json.contains("minutesUntilReset"),
            "json carries the countdown: {json}"
        );
    }

    /// A prober that puts alice's 5h window comfortably BELOW her threshold (a
    /// live reset that was previously invisible on the wire), and leaves bob's
    /// 5h window entirely unlearned (no live reset at all — the other
    /// invisible case).
    struct BelowThresholdWindowProber;
    impl UsageProber for BelowThresholdWindowProber {
        fn probe(&self, credential: crate::probe::ProbeCredential) -> ProbeFuture {
            let now = crate::now_ms();
            Box::pin(async move {
                if credential.access_token == "at-a" {
                    Ok(Usage {
                        five_hour: crate::probe::WindowReading::Read(UsageBucket {
                            utilization: Some(0.3),
                            reset_at_ms: Some(now + 42 * 60 * 1000),
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                        extra_usage_usd: None,
                        codex: None,
                        claude_resets: None,
                    })
                } else {
                    Ok(Usage {
                        five_hour: crate::probe::WindowReading::Unread,
                        seven_day: None,
                        seven_day_oi: None,
                        extra_usage_usd: None,
                        codex: None,
                        claude_resets: None,
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn render_accounts_json_carries_reset_for_a_below_threshold_window() {
        // This is the exact case the fix restores: a window well under its
        // threshold (so it never appears in `held[]`) still has a live reset,
        // and that reset must now reach the JSON wire unconditionally.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;

        let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
        let alice_row = status_row(&json, "alice@example.com");
        // 0.3 is far under the 0.9 default threshold, so alice's 5h window is
        // never a binding hold, yet its reset must still reach the wire.
        assert!(
            alice_row["fiveHourResetAtMs"].is_i64(),
            "below-threshold window's reset reaches the wire: {alice_row}"
        );
        // The below-threshold window never binds, so it is absent from `held[]`
        // even though its reset is now unconditionally on the wire.
        assert!(
            alice_row["held"]
                .as_array()
                .expect("held is an array")
                .is_empty(),
            "held[] stays empty for a non-binding window: {alice_row}"
        );
    }

    /// `tcr status --json` carries `quotaUpdatedAtMs`, the time of the newest
    /// quota reading, on both the offline path (its own probe) and the live
    /// path (the server's payload, rebuilt by `into_snapshot`), and the two
    /// agree.
    #[tokio::test]
    async fn render_accounts_json_carries_quota_updated_at() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let before = crate::now_ms();
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;
        let render = |snapshot: &StatsSnapshot, source: StatusSource| {
            let json = status_json(snapshot, &thresholds, source);
            status_row(&json, "alice@example.com")
        };

        let offline = render(&snapshot, StatusSource::Offline);
        let at = offline["quotaUpdatedAtMs"]
            .as_i64()
            .unwrap_or_else(|| panic!("a probed row carries quotaUpdatedAtMs: {offline}"));
        assert!(
            at >= before && at <= crate::now_ms(),
            "a fresh probe reads as recent: {at}"
        );

        let payload =
            StatusPayload::from_snapshot(&snapshot, &thresholds, false, None, Default::default());
        let wire = serde_json::to_string(&payload).expect("serialize");
        let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
        let (rebuilt, rebuilt_thresholds) = back.into_snapshot();
        let live_json = status_json(&rebuilt, &rebuilt_thresholds, StatusSource::Live);
        let live: Vec<serde_json::Value> = serde_json::from_str(&live_json).expect("json");
        let live = live
            .iter()
            .find(|r| r["name"] == "alice@example.com")
            .expect("alice row");
        assert_eq!(
            live["quotaUpdatedAtMs"], offline["quotaUpdatedAtMs"],
            "the live path carries the same instant the server read"
        );
    }

    #[tokio::test]
    async fn render_accounts_json_omits_reset_when_no_live_reset_exists() {
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;

        let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
        let bob_row = status_row(&json, "bob@example.com");
        assert_eq!(
            bob_row["fiveHourResetAtMs"],
            serde_json::Value::Null,
            "no live reset renders as null, never 0 or a fabricated instant: {bob_row}"
        );
        assert_eq!(
            bob_row["sevenDayResetAtMs"],
            serde_json::Value::Null,
            "no live reset renders as null, never 0 or a fabricated instant: {bob_row}"
        );
    }

    #[tokio::test]
    async fn render_accounts_json_held_array_is_unchanged_for_over_threshold_account() {
        // The additive claim, asserted rather than assumed: adding the two new
        // reset fields must not change a single byte of the existing `held[]`
        // shape for an account whose window WAS already binding.
        let config = load_from(TWO_ACCOUNTS);
        let thresholds = resolve_thresholds(&config);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
        let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
        let alice_row = status_row(&json, "alice@example.com");
        let held = alice_row["held"].as_array().expect("held is an array");
        assert_eq!(
            held.len(),
            1,
            "alice's 5h window is the sole hold: {held:?}"
        );
        let entry = &held[0];
        assert_eq!(entry["window"], "5h");
        assert!(
            entry["resetAtMs"].is_i64(),
            "held entry keeps resetAtMs: {entry}"
        );
        assert!(
            entry["minutesUntilReset"].is_i64(),
            "held entry keeps minutesUntilReset: {entry}"
        );
        // held[]'s own two keys are unaffected by the new top-level fields.
        assert_eq!(
            entry.as_object().expect("held entry is an object").len(),
            3,
            "held entry shape (window/resetAtMs/minutesUntilReset) is unchanged: {entry}"
        );
    }

    #[tokio::test]
    async fn render_accounts_text_carries_countdown_for_a_below_threshold_window() {
        // The text-line half of the same restored invariant: a window well
        // under its threshold still has a live reset, so the greppable status
        // line must carry its `(+countdown)` even though the window never
        // binds a hold.
        let config = load_from(TWO_ACCOUNTS);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;
        let text = render_accounts(&snapshot, StatusSource::Live);
        let alice = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        assert!(
            alice.contains("5h=30%(+"),
            "below-threshold window still carries a countdown: {alice}"
        );
    }

    #[tokio::test]
    async fn render_accounts_text_omits_countdown_when_no_live_reset_exists() {
        let config = load_from(TWO_ACCOUNTS);
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(BelowThresholdWindowProber),
            true,
        )
        .await;
        let text = render_accounts(&snapshot, StatusSource::Live);
        let bob = text
            .lines()
            .find(|l| l.contains("bob@example.com"))
            .expect("bob line");
        // bob's 5h window was never learned at all: n/a, no `(+…)` token.
        assert!(bob.contains("5h=n/a"), "unlearned window reads n/a: {bob}");
        assert!(
            !bob.contains("5h=n/a("),
            "no countdown token on an unlearned window: {bob}"
        );
    }

    /// A prober that drives every account's 5h window fully over the 1.0
    /// exhaustion line, with a future reset — so the window binds and reads `spent`.
    struct ExhaustedProber;
    impl UsageProber for ExhaustedProber {
        fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
            let now = crate::now_ms();
            Box::pin(async move {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Read(UsageBucket {
                        utilization: Some(1.0),
                        reset_at_ms: Some(now + 45 * 60 * 1000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn render_accounts_marks_fully_exhausted_account_as_spent_with_countdown() {
        let config = load_from(TWO_ACCOUNTS);
        let snapshot =
            snapshot_offline(config, Arc::new(NoRefresh), Arc::new(ExhaustedProber), true).await;
        let text = render_accounts(&snapshot, StatusSource::Live);
        let line = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        // At 1.0 util the account is truly spent (never masquerading as `full`),
        // and its binding 5h window carries an inline `(+…)` countdown. The exact
        // minute drifts with wall-clock between probe and render, so match the `(+`.
        assert!(line.contains("state=spent"), "exhausted → spent: {line}");
        assert!(
            line.contains("5h=100%(+"),
            "5h exhausted + countdown: {line}"
        );
    }

    // ---------------------------------------------------------------------
    // The golden `tcr status --json` contract fixture.
    // ---------------------------------------------------------------------

    /// Path of the fixture BOTH sides read. One file, never a copy: the whole
    /// point is that the Swift decoder and this renderer cannot drift apart, and
    /// two files that must stay equal are the drift this exists to prevent.
    /// `apps/macos/Tests/TcrBarTests/RealWorldDecodeTests.swift` reaches the same
    /// path from `#filePath`.
    fn status_contract_fixture_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/status-contract.json")
    }

    /// One fully-specified account row for the fixture.
    ///
    /// Every field is passed explicitly rather than defaulted, so a new field on
    /// [`AccountSnapshot`] breaks this call site and forces a decision about what
    /// the contract sample should say about it.
    #[expect(
        clippy::too_many_arguments,
        reason = "a test builder whose arguments are the row's fields"
    )]
    fn contract_account(
        name: &str,
        priority: i64,
        status: &str,
        disabled: bool,
        five_hour: Option<f64>,
        five_hour_reset: Option<OffsetDateTime>,
        seven_day: Option<f64>,
        seven_day_reset: Option<OffsetDateTime>,
        seven_day_oi: Option<f64>,
        seven_day_oi_reset: Option<OffsetDateTime>,
        probe_status: ProbeStatus,
        probe_error: Option<&str>,
        quota_state: QuotaState,
        groups: &[&str],
        usage: Option<tcr_status_wire::UsageRow>,
        // `plan` is `(organizationType, rateLimitTier, seatTier)` — the three
        // raw strings, exactly as the profile endpoint spells them. `org` is
        // `(orgUuid, orgName)`, the org a client shows beside this row.
        // Grouped rather than five more
        // positional `Option<&str>` arguments, which at this arity is a call
        // site nobody can read.
        plan: (Option<&str>, Option<&str>, Option<&str>),
        org: (Option<&str>, Option<&str>),
    ) -> AccountSnapshot {
        AccountSnapshot {
            slot: 0,
            name: name.to_string(),
            // Anthropic unless the sample says otherwise: its one Codex row
            // overrides this after construction, as row 2 does `free_at`.
            provider: Provider::Anthropic,
            organization_type: plan.0.map(str::to_string),
            rate_limit_tier: plan.1.map(str::to_string),
            seat_tier: plan.2.map(str::to_string),
            plan_display_name: None,
            org_uuid: org.0.map(str::to_string),
            org_name: org.1.map(str::to_string),
            priority,
            status: status.to_string(),
            disabled,
            five_hour,
            five_hour_reset,
            five_hour_absent_on_plan: false,
            seven_day,
            seven_day_reset,
            seven_day_oi,
            seven_day_oi_reset,
            requests: if five_hour.is_some() { 102 } else { 0 },
            input_tokens: if five_hour.is_some() { 8_000_000 } else { 0 },
            output_tokens: if five_hour.is_some() { 31_860 } else { 0 },
            cache_read_tokens: if five_hour.is_some() { 6_000_000 } else { 0 },
            // Chosen so the four input dimensions reconcile: `inputTokens` is
            // the quota counter and equals base + cache creation + cache reads,
            // so 8,000,000 = 400,000 + 1,600,000 + 6,000,000. A fixture whose
            // numbers do not add up teaches every reader the wrong arithmetic.
            cache_creation_tokens: if five_hour.is_some() { 1_600_000 } else { 0 },
            last_used: None,
            rate_limited_until: None,
            probe_status,
            last_probe: None,
            quota_updated_at: None,
            probe_error: probe_error.map(str::to_string),
            quota_state,
            gate: crate::stats::GateReason::Ok,
            free_at: None,
            stream_error_count: if five_hour.is_some() { 2 } else { 0 },
            last_stream_error: if five_hour.is_some() {
                Some("overloaded_error".to_string())
            } else {
                None
            },
            groups: groups.iter().map(ToString::to_string).collect(),
            reserved_groups: Vec::new(),
            parked_groups: Vec::new(),
            control_allowed_groups: Vec::new(),
            usage,
            codex: Default::default(),
        }
    }

    /// One model's slice of a fixture row's usage, priced by the SAME function
    /// production prices with.
    ///
    /// A cost typed into a fixture is a claim about the pricing table that
    /// nothing checks: change a rate and the fixture goes on asserting the old
    /// dollar figure is what this proxy produces. Running the real
    /// [`crate::pricing`] path means the committed bytes are what the code
    /// actually computes, and a rate change shows up as a fixture diff — which
    /// is most of the point of committing one.
    fn contract_totals(
        model: &str,
        requests: u64,
        input: u64,
        cache_5m: u64,
        cache_1h: u64,
        cache_read: u64,
        output: u64,
    ) -> tcr_status_wire::UsageTotals {
        let price = crate::pricing::PricingTable::default().lookup(model);
        tcr_status_wire::UsageTotals {
            requests,
            input_tokens: input,
            // BOTH TTLs, exactly as `Totals::to_wire` emits it — the same
            // quantity the row-level `cacheCreationTokens` carries, with the
            // 1-hour part broken out beside it.
            cache_creation_tokens: cache_5m + cache_1h,
            cache_creation_1h_tokens: cache_1h,
            cache_read_tokens: cache_read,
            output_tokens: output,
            cost_usd: price.map(|price| {
                crate::pricing::cost_nanos(&price, input, cache_5m, cache_1h, cache_read, output)
                    as f64
                    / 1e9
            }),
            unpriced_requests: if price.is_some() { 0 } else { requests },
            reasoning_tokens: 0,
        }
    }

    /// Add `other` into `totals`, the way the tracker's own buckets sum.
    /// `costUsd` is `None` only when NOTHING in the sum could be priced.
    fn contract_add(
        totals: tcr_status_wire::UsageTotals,
        other: tcr_status_wire::UsageTotals,
    ) -> tcr_status_wire::UsageTotals {
        let requests = totals.requests + other.requests;
        let unpriced_requests = totals.unpriced_requests + other.unpriced_requests;
        tcr_status_wire::UsageTotals {
            requests,
            input_tokens: totals.input_tokens + other.input_tokens,
            cache_creation_tokens: totals.cache_creation_tokens + other.cache_creation_tokens,
            cache_creation_1h_tokens: totals.cache_creation_1h_tokens
                + other.cache_creation_1h_tokens,
            cache_read_tokens: totals.cache_read_tokens + other.cache_read_tokens,
            output_tokens: totals.output_tokens + other.output_tokens,
            // Summed in NANODOLLARS, then converted once — exactly what the
            // tracker does. Adding the two dollar figures as `f64` instead
            // yields 14.165700000000001 for a total the production path renders
            // as 14.1657, and a fixture that disagrees with production in its
            // last bit is a fixture that will be "fixed" by someone rounding.
            cost_usd: if requests > 0 && unpriced_requests >= requests {
                None
            } else {
                let nanos = |usd: Option<f64>| (usd.unwrap_or(0.0) * 1e9).round() as u64;
                Some((nanos(totals.cost_usd) + nanos(other.cost_usd)) as f64 / 1e9)
            },
            unpriced_requests,
            reasoning_tokens: totals.reasoning_tokens + other.reasoning_tokens,
        }
    }

    /// A fixture row's `usage`, built from a mix of two models.
    ///
    /// The token counts reconcile with the row-level counters
    /// [`contract_account`] sets: 102 requests, 400,000 base input, 1,600,000
    /// cache creation (1,200,000 at 5 minutes plus 400,000 at an hour, and the
    /// nested `cacheCreationTokens` carries that same 1,600,000 the row-level
    /// key does), 6,000,000 cache reads and 31,860 output. `lastHour` is a subset of
    /// `window`, which is a subset of `today` — the containment any reader will
    /// assume, so the sample had better honour it.
    ///
    /// `second_model` is what makes the two priced rows differ from the third:
    /// pass a model the table has no published rate for and `costUsd` becomes a
    /// PARTIAL total with `unpricedRequests` saying how much is missing, which
    /// is the case a client renderer is most likely to get wrong.
    fn contract_usage(second_model: &str) -> tcr_status_wire::UsageRow {
        let opus = |requests: u64| {
            contract_totals(
                "claude-opus-5",
                requests,
                4_000 * requests,
                12_000 * requests,
                4_000 * requests,
                60_000 * requests,
                302 * requests,
            )
        };
        let other = |requests: u64| {
            contract_totals(
                second_model,
                requests,
                3_750 * requests,
                11_250 * requests,
                3_750 * requests,
                56_250 * requests,
                335 * requests,
            )
        };
        tcr_status_wire::UsageRow {
            today: contract_add(opus(70), other(32)),
            window: Some(tcr_status_wire::UsageWindow {
                // The 5-hour reset every fixture row carries, minus five hours.
                since: 1_767_225_600_000 - crate::usage::FIVE_HOURS_MS,
                totals: contract_add(opus(28), other(12)),
            }),
            last_hour: contract_add(opus(8), other(4)),
            today_by_model: [
                ("claude-opus-5".to_string(), opus(70)),
                (second_model.to_string(), other(32)),
            ]
            .into_iter()
            .collect(),
            seven_day_window: None,
        }
    }

    /// A fixture row's `usage` for an account that has served nothing: what the
    /// usage tracker reports for one, every bucket a MEASURED zero (`costUsd:
    /// 0.0`, never the `null` that means "could not be priced") and no `window`,
    /// because no five-hour reset was ever learned to name the window's start.
    fn contract_idle_usage() -> tcr_status_wire::UsageRow {
        let idle = tcr_status_wire::UsageTotals {
            cost_usd: Some(0.0),
            ..Default::default()
        };
        tcr_status_wire::UsageRow {
            today: idle,
            window: None,
            last_hour: idle,
            today_by_model: std::collections::BTreeMap::new(),
            seven_day_window: None,
        }
    }

    /// A fixed instant in the PAST, so the two clock-derived fields render
    /// deterministically.
    ///
    /// `minutesUntilReset` is `(reset - now).whole_minutes().max(0)` and
    /// `secondsUntilFree` is dropped entirely once `free_at` has elapsed, so
    /// every reset instant here is historical and both render as their clamped /
    /// absent value. That buys a byte-for-byte comparison against raw renderer
    /// output with **no normalisation layer** — and a normalisation layer is
    /// exactly where a contract pin rots, because it is the one part of the
    /// comparison nothing checks.
    fn fixture_instant(unix_ms: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_ms) * 1_000_000)
            .expect("fixture instant is a valid timestamp")
    }

    /// The snapshot the fixture is rendered from. Obviously-fake data only —
    /// this repository is public.
    fn status_contract_snapshot() -> (StatsSnapshot, Vec<f64>) {
        // 2026-01-01T00:00:00Z and 2026-01-02T00:00:00Z, both long past.
        let five_hour_reset = fixture_instant(1_767_225_600_000);
        let seven_day_reset = fixture_instant(1_767_312_000_000);

        let accounts = vec![
            // `ok`: probed, measured, in rotation — the ordinary row.
            contract_account(
                "alice@example.com",
                0,
                "active",
                false,
                Some(0.04),
                Some(five_hour_reset),
                Some(0.01),
                Some(seven_day_reset),
                Some(0.0),
                Some(seven_day_reset),
                ProbeStatus::Ok,
                None,
                QuotaState::Normal,
                // Non-empty and multi-membership — the only row the Swift decode
                // test has real group data to read, and the one the greppable
                // `groups=codereview,dev` text-line assertion targets.
                &["codereview", "dev"],
                // Every request priced: `costUsd` is a complete figure and
                // `unpricedRequests` is 0.
                Some(contract_usage("claude-sonnet-5")),
                // `Max 20x` — the multiplier suffix, and the plan the fleet's
                // personal rows carry.
                (Some("claude_max"), Some("default_claude_max_20x"), None),
                (
                    Some("11111111-1111-1111-1111-111111111111"),
                    Some("Example Personal"),
                ),
            ),
            // `near`: at threshold, so `held` carries a window — the only row
            // that exercises the nested object.
            contract_account(
                "bob@example.com",
                1,
                "active",
                false,
                Some(0.95),
                Some(five_hour_reset),
                Some(0.91),
                Some(seven_day_reset),
                Some(0.10),
                Some(seven_day_reset),
                ProbeStatus::RateLimited,
                None,
                QuotaState::NearLimit,
                &[],
                // A model the built-in table has no published rate for, so a
                // THIRD of this row's requests are unpriced: `costUsd` is a
                // partial total and `unpricedRequests` says so. The case a
                // client renderer is most likely to get wrong — silently
                // presenting a partial sum as the whole spend.
                Some(contract_usage("claude-sonnet-4-5-20250929")),
                // `Team 5x` — a premium Team seat, where the multiplier
                // OUTRANKS the seat (`team_tier_1` is not what shows).
                (
                    Some("claude_team"),
                    Some("default_claude_max_5x"),
                    Some("team_tier_1"),
                ),
                (
                    Some("22222222-2222-2222-2222-222222222222"),
                    Some("Example Team"),
                ),
            ),
            // `spent`: fully consumed, and carrying a probe error string.
            contract_account(
                "carol@example.com",
                2,
                "throttled",
                false,
                Some(1.0),
                Some(five_hour_reset),
                Some(1.0),
                Some(seven_day_reset),
                Some(0.5),
                Some(seven_day_reset),
                ProbeStatus::Error,
                Some("probe failed: connection reset"),
                QuotaState::Exhausted,
                &[],
                Some(contract_usage("claude-sonnet-5")),
                // `Team Standard` — a standard seat, whose tier names no
                // multiplier, so the SEAT is the suffix. Same org as bob above,
                // which is the pairing that makes `orgUuid` worth carrying.
                (
                    Some("claude_team"),
                    Some("default_raven"),
                    Some("team_standard"),
                ),
                (
                    Some("22222222-2222-2222-2222-222222222222"),
                    Some("Example Team"),
                ),
            ),
            // Never probed AND disabled: the four quota fractions and
            // `cacheHitRatio` are all null. This row is the one that shipped a
            // decode crash — `valueNotFound … Path: [2].quota`. Also the
            // unlabelled row: `groups` must render `[]`, never `null`.
            contract_account(
                "dave@example.com",
                3,
                "active",
                true,
                None,
                None,
                None,
                None,
                None,
                None,
                ProbeStatus::Never,
                None,
                QuotaState::Normal,
                &[],
                // NO usage object at all — the shape a client gets from a
                // server built before usage existed. The key is absent, not
                // null-valued and not zero-filled, so both decoders are pinned
                // against the skew that actually happens here: the binary on
                // disk is rebuilt on merge while the live process keeps serving
                // until someone restarts it.
                None,
                // NEVER PROFILED: all three plan keys null and `plan` null with
                // them. The row both decoders must render as "plan unknown"
                // rather than inventing one — and the shape every account has
                // until its first probe backfills it.
                (None, None, None),
                (None, None),
            ),
            // A Codex account, under the name the import gives a ChatGPT login
            // whose email an Anthropic row already holds (`<email>/codex`). Its
            // plan is the raw `chatgpt_plan_type` claim, labelled `ChatGPT
            // Pro`, with no rate-limit tier, no seat and no org: a ChatGPT
            // workspace is named by its account id alone. Probed, with a
            // WEEKLY reading only, which is all the Codex backend reports, and
            // idle, so its usage is the measured zero. Disabled, so it adds
            // nothing to the capacity counts a panel derives from this sample:
            // a panel that predates `provider` would count an enabled Codex row
            // as Claude capacity, and the Swift suite pins those counts.
            contract_account(
                "bob@example.com/codex",
                4,
                "active",
                true,
                None,
                None,
                Some(0.10),
                Some(seven_day_reset),
                None,
                None,
                ProbeStatus::Ok,
                None,
                QuotaState::Normal,
                &[],
                Some(contract_idle_usage()),
                (Some("pro"), None, None),
                (None, None),
            ),
        ];
        let mut accounts = accounts;
        // `freeAtMs` and `rateLimitedUntilMs` must be pinned as *numbers*
        // somewhere in the sample, or the fixture would only ever prove they can
        // be null — which a deleted key also looks like. Both instants are past,
        // so `secondsUntilFree` (the one clock-derived companion) stays absent
        // and the render stays deterministic.
        accounts[2].free_at = Some(fixture_instant(1_767_312_000_000));
        accounts[2].rate_limited_until = Some(fixture_instant(1_767_225_600_000));
        // Same reason for `quotaUpdatedAtMs`: a number on the ordinary row, null
        // on the others, so both shapes are pinned.
        accounts[0].quota_updated_at = Some(fixture_instant(1_767_225_600_000));
        // The sample's one non-Anthropic row, so it pins both shapes of the
        // `provider` key: absent on the four rows above, `"codex"` here.
        accounts[4].provider = Provider::Codex;
        accounts[4].codex.reset_credits = Some(tcr_status_wire::ResetCredits {
            available_count: 3,
            earliest_expiry_ms: Some(1_800_000_000_000),
        });

        let thresholds = vec![0.9; accounts.len()];
        (
            StatsSnapshot {
                accounts,
                current: Some(0),
                current_by_provider: crate::stats::CurrentByProvider::default(),
                recent: Vec::new(),
                sessions: Vec::new(),
                wire_sessions: Vec::new(),
                wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
            },
            thresholds,
        )
    }

    /// The exact bytes the fixture file must hold.
    fn status_contract_rendered() -> String {
        let (snapshot, thresholds) = status_contract_snapshot();
        let build = BuildInfo {
            sha: "abc1234".to_string(),
            dirty: Some(false),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        };
        // Real content, not an empty map: `groupColors` must be exercised by
        // the fixture the same way `groups`/`reservedGroups` already are, or a
        // future renderer bug that stops emitting real colors would pass this
        // pin silently. Matches alice's `["codereview", "dev"]` membership
        // above so a reader can see the shape it feeds without cross-checking
        // another test.
        let group_colors: std::collections::BTreeMap<String, String> = [
            (
                "codereview".to_string(),
                config::derive_group_color("codereview"),
            ),
            ("dev".to_string(), "#0a84ff".to_string()),
        ]
        .into_iter()
        .collect();

        // Trailing newline so the file is a well-formed text file; the renderer
        // itself emits none.
        format!(
            "{}\n",
            render_accounts_json(
                &snapshot,
                &thresholds,
                StatusSource::Live,
                Some(&build),
                false,
                None,
                &group_colors,
            )
        )
    }

    /// THE CROSS-LANGUAGE CONTRACT PIN.
    ///
    /// `tcr status --json` is decoded by TcrBar (`FleetStatus.swift`), and no
    /// compiler checks that seam — the key names cross as strings. This test
    /// asserts the renderer still produces the committed fixture byte-for-byte,
    /// and `RealWorldDecodeTests.testCommittedContractFixtureDecodes` decodes
    /// **that same file** through `Fleet.decode`.
    ///
    /// So a renamed key cannot land quietly: this test goes red first, and
    /// regenerating the fixture to satisfy it (`TCR_UPDATE_FIXTURES=1 cargo test
    /// status_contract_fixture_matches_committed`) turns the Swift side red in
    /// the same breath. Both reds were observed before this was believed.
    #[test]
    fn status_contract_fixture_matches_committed() {
        let path = status_contract_fixture_path();
        let rendered = status_contract_rendered();

        if std::env::var_os("TCR_UPDATE_FIXTURES").is_some() {
            fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
            fs::write(&path, &rendered).expect("write fixture");
            eprintln!("regenerated {}", path.display());
            return;
        }

        let committed = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read the committed contract fixture at {}: {e}. \
                 Regenerate with TCR_UPDATE_FIXTURES=1 cargo test \
                 status_contract_fixture_matches_committed",
                path.display()
            )
        });

        assert_eq!(
            committed,
            rendered,
            "`tcr status --json` no longer renders the committed contract \
             fixture at {}. If the change is intended, regenerate it with \
             TCR_UPDATE_FIXTURES=1 and then run the Swift decode suite \
             (apps/macos: swift test) — that is the half of the contract this \
             process cannot check.",
            path.display()
        );
    }

    /// `tcr status`'s text form over the contract sample's Anthropic rows, as a live
    /// server at `abc1234` answers a client at `def5678`, with `claude_verdict` above the
    /// header when there is one.
    fn claude_only_status_text(claude_verdict: Option<&str>) -> String {
        let (mut snapshot, _) = status_contract_snapshot();
        snapshot
            .accounts
            .retain(|row| row.provider == Provider::Anthropic);
        let server = BuildInfo {
            sha: "abc1234".to_string(),
            dirty: Some(false),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let client = BuildInfo {
            sha: "def5678".to_string(),
            dirty: Some(false),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        };
        // The group colors `status_contract_rendered` builds.
        let group_colors: std::collections::BTreeMap<String, String> = [
            (
                "codereview".to_string(),
                config::derive_group_color("codereview"),
            ),
            ("dev".to_string(), "#0a84ff".to_string()),
        ]
        .into_iter()
        .collect();
        render_status_text(&StatusText {
            snapshot: &snapshot,
            source: StatusSource::Live,
            server_build: Some(&server),
            client_build: &client,
            http1_only: false,
            group_colors: &group_colors,
            claude_verdict: claude_verdict.map(str::to_string),
            // Not pooled, and a Claude-only fleet still prints no Codex line.
            codex_route: Some(&mitm_off_route()),
            holds_codex: holds_provider(&snapshot, Provider::Codex),
        })
    }

    /// The bytes `tcr status` prints for a fleet with no Codex account, pinned before the
    /// status text learned about Codex accounts: every line a Codex fleet gains must leave a
    /// Claude-only fleet's text byte for byte as it was, the route verdict above the header
    /// included.
    #[test]
    fn claude_only_status_text_is_unchanged() {
        /// The pinned bytes, below any verdict line.
        const FLEET: &str = "status source=live server_sha=abc1234 server_dirty=false \
             server_built_at=2026-01-01T00:00:00Z client_sha=def5678 http1Only=false \
             groupColors=codereview:#63cbfe,dev:#0a84ff\n\
             account alice@example.com priority=0 plan=Max 20x 5h=4% wk=1% fable=0% cache=75% \
             state=ok status=active probe=ok stream_errors=2 last_stream_error=overloaded_error \
             groups=codereview,dev\n\
             account bob@example.com priority=1 plan=Team 5x 5h=95% wk=91% fable=10% cache=75% \
             state=near status=active probe=rate-limited stream_errors=2 \
             last_stream_error=overloaded_error\n\
             account carol@example.com priority=2 plan=Team Standard 5h=100% wk=100% fable=50% \
             cache=75% state=spent status=throttled probe=error stream_errors=2 \
             last_stream_error=overloaded_error\n\
             account dave@example.com priority=3 5h=n/a wk=n/a cache=n/a state=ok status=active \
             probe=never stream_errors=0 disabled\n";
        assert_eq!(claude_only_status_text(None), FLEET);
        assert_eq!(
            claude_only_status_text(Some(
                "verdict: this proxy is not on Claude's route: \
                 /Users/example/.claude/settings.json sets https://gateway.example.com"
            )),
            format!(
                "verdict: this proxy is not on Claude's route: \
                 /Users/example/.claude/settings.json sets https://gateway.example.com\n{FLEET}"
            )
        );
    }

    /// The Claude verdict the text-form tests print above the header.
    const CLAUDE_VERDICT: &str = "verdict: this proxy is not on Claude's route: \
                                  /Users/example/.claude/settings.json sets \
                                  https://gateway.example.com";

    /// The MITM-off route's reason, as `tcr run --codex` words it for a proxy on :3999.
    const MITM_OFF_REASON: &str = "the proxy on :3999 is running without MITM: its TLS material \
                                   failed to load at boot — see its log";

    /// The status read of a proxy on :3999, as `fetch_live_status` hands it over: parsed from
    /// the wire, with `mitm` as given, `codex_rows` Codex accounts and `traffic` as its counts.
    fn live_payload(
        mitm: serde_json::Value,
        codex_rows: usize,
        traffic: Option<serde_json::Value>,
    ) -> StatusPayload {
        let mut wire = crate::test_support::status_wire::codex_status_wire(Some(mitm), codex_rows);
        if let Some(traffic) = traffic {
            wire["codex_traffic"] = traffic;
        }
        serde_json::from_value(wire).expect("a status payload")
    }

    fn route_test_ca() -> &'static Path {
        Path::new("/tmp/tcr-run-test/tcr-ca.pem")
    }

    /// The route of a proxy on :3999 running without MITM: not pooled.
    fn mitm_off_route() -> tcr_status_wire::CodexRoute {
        let payload = live_payload(
            serde_json::json!({"interceptedHosts": [], "leaf": "none"}),
            1,
            None,
        );
        crate::codex_route::codex_route(Ok(&payload), 3999, route_test_ca(), true, 1)
    }

    fn pooled_route() -> tcr_status_wire::CodexRoute {
        tcr_status_wire::CodexRoute {
            pooled: true,
            ..Default::default()
        }
    }

    #[test]
    fn an_older_proxy_uses_configured_codex_provider_presence() {
        let old = live_payload(serde_json::Value::Null, 0, None);
        let config: Config = serde_json::from_value(serde_json::json!({
            "accounts": [{"name":"alice@example.com/codex", "provider":"codex", "accessToken":"at-fake"}]
        })).unwrap();
        assert!(status_holds_provider(&Ok(old), &config, Provider::Codex));
        let current = live_payload(
            serde_json::json!({"interceptedHosts": [], "leaf":"minted"}),
            0,
            None,
        );
        assert!(
            !status_holds_provider(&Ok(current), &config, Provider::Codex),
            "a current server's live deletion overrides stale configured rows"
        );
    }

    /// `tcr status`'s text form over `snapshot`, as the golden renders it, with the Claude
    /// verdict and the Codex route given.
    fn status_text_of(
        snapshot: &StatsSnapshot,
        claude_verdict: Option<&str>,
        codex_route: &tcr_status_wire::CodexRoute,
    ) -> String {
        let build = BuildInfo {
            sha: "abc1234".to_string(),
            dirty: Some(false),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        };
        render_status_text(&StatusText {
            snapshot,
            source: StatusSource::Live,
            server_build: Some(&build),
            client_build: &build,
            http1_only: false,
            group_colors: &std::collections::BTreeMap::new(),
            claude_verdict: claude_verdict.map(str::to_string),
            codex_route: Some(codex_route),
            holds_codex: holds_provider(snapshot, Provider::Codex),
        })
    }

    /// **A fleet that holds a Codex account is told, above the header, when codex is not
    /// pooled**, keyed `codexVerdict:` beside Claude's own verdict, which keeps the first line.
    /// A pooled route prints nothing.
    #[test]
    fn the_codex_verdict_prints_only_with_a_codex_account_not_pooled() {
        // The contract sample: four Anthropic rows and a disabled Codex row.
        let (snapshot, _) = status_contract_snapshot();
        let off = mitm_off_route();
        let text = status_text_of(&snapshot, None, &off);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            format!("codexVerdict: codex is not pooled: {MITM_OFF_REASON}")
        );
        assert!(lines[1].starts_with("status source="), "{text}");

        let text = status_text_of(&snapshot, Some(CLAUDE_VERDICT), &off);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], CLAUDE_VERDICT, "Claude's verdict stays first");
        assert_eq!(
            lines[1],
            format!("codexVerdict: codex is not pooled: {MITM_OFF_REASON}")
        );
        assert!(lines[2].starts_with("status source="), "{text}");

        let text = status_text_of(&snapshot, None, &pooled_route());
        assert!(text.starts_with("status source="), "{text}");
        assert!(!text.contains("codex is not pooled"), "{text}");
    }

    /// **A Codex-only fleet is not told about Claude's route**, which none of its accounts
    /// uses: its one verdict is codex's, keyed `verdict:`. A pooled route leaves the header
    /// first, and an empty fleet keeps Claude's verdict as it always had it.
    ///
    /// Watched red: with Claude's verdict printed whatever the fleet holds, the Codex-only
    /// fleet opens with Claude's route.
    #[test]
    fn the_claude_verdict_is_skipped_in_a_codex_only_fleet() {
        let (mut snapshot, _) = status_contract_snapshot();
        snapshot
            .accounts
            .retain(|row| row.provider == Provider::Codex);
        let off = mitm_off_route();
        let text = status_text_of(&snapshot, Some(CLAUDE_VERDICT), &off);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            format!("verdict: codex is not pooled: {MITM_OFF_REASON}")
        );
        assert!(lines[1].starts_with("status source="), "{text}");
        assert!(
            !text.contains("this proxy is not on Claude's route"),
            "{text}"
        );

        let text = status_text_of(&snapshot, Some(CLAUDE_VERDICT), &pooled_route());
        assert!(text.starts_with("status source="), "{text}");

        snapshot.accounts.clear();
        let text = status_text_of(&snapshot, Some(CLAUDE_VERDICT), &off);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], CLAUDE_VERDICT, "an empty fleet keeps it");
        assert!(lines[1].starts_with("status source="), "{text}");
        assert!(!text.contains("codex is not pooled"), "{text}");
    }

    /// **The stderr notes say what tcr did with codex in the last ten minutes**, each only
    /// when its count is above zero.
    #[test]
    fn status_prints_the_codex_traffic_notes_on_stderr() {
        let traffic = tcr_status_wire::CodexTraffic {
            refused_10m: 12,
            last_refusal: Some(tcr_status_wire::CodexRefusalKind::UsageLimitReached),
            last_refusal_at_ms: Some(1),
            unpooled_10m: 3,
        };
        assert_eq!(
            codex_traffic_notes(&traffic),
            [
                "[tcr] note codex-refusals: refused_10m=12 last_type=usage_limit_reached — tcr \
                 refused codex because no Codex account could serve; see the Codex \
                 rows above",
                "[tcr] note codex-unpooled: relayed_10m=3 — requests from a FedRAMP or \
                 residency-routed ChatGPT login ran on their own credential, unpooled",
            ]
        );
        assert!(
            codex_traffic_notes(&tcr_status_wire::CodexTraffic::default()).is_empty(),
            "nothing to say"
        );
        let refused_only = tcr_status_wire::CodexTraffic {
            unpooled_10m: 0,
            ..traffic
        };
        let notes = codex_traffic_notes(&refused_only);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with("[tcr] note codex-refusals:"),
            "{notes:?}"
        );
    }

    /// **`--json` carries a Codex route only from a live read.** Nothing listening, no answer,
    /// or one this build cannot use: none of them is a route, so no row claims one.
    #[test]
    fn the_route_is_absent_offline() {
        for live in [
            Err(LiveStatusError::NoServer),
            Err(LiveStatusError::NoAnswer(
                "the server did not answer within 5s".to_string(),
            )),
            Err(LiveStatusError::Unusable(
                "HTTP 401 Unauthorized".to_string(),
            )),
        ] {
            let facts = codex_wire_facts(&live, 3999, route_test_ca(), true);
            assert_eq!(facts, (None, None), "{live:?}");
        }
    }

    /// **The route and the traffic ride the Codex rows only**: the same on every Codex row, and
    /// on no Anthropic row, whose bytes stay what they were.
    #[test]
    fn route_and_traffic_ride_only_codex_rows() {
        let live = Ok(live_payload(
            serde_json::json!({
                "interceptedHosts": ["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
                "leaf": "minted",
            }),
            1,
            Some(serde_json::json!({
                "refused10m": 2,
                "lastRefusal": "pool_unavailable",
                "lastRefusalAtMs": 1_767_225_000_000_i64,
                "unpooled10m": 0,
            })),
        ));
        let (route, traffic) = codex_wire_facts(&live, 3999, route_test_ca(), true);
        assert_eq!(route, Some(pooled_route()));
        assert_eq!(traffic.as_ref().map(|t| t.refused_10m), Some(2));

        let (snapshot, thresholds) = status_contract_snapshot();
        let build = BuildInfo {
            sha: "abc1234".to_string(),
            dirty: Some(false),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let rendered = status_rows_json(account_status_rows_at(
            OffsetDateTime::from_unix_timestamp(1_767_225_600).expect("a valid instant"),
            &snapshot,
            &thresholds,
            StatusSource::Live,
            Some(&build),
            false,
            None,
            &std::collections::BTreeMap::new(),
            route.as_ref(),
            traffic.as_ref(),
        ));
        let rows: Vec<serde_json::Value> = serde_json::from_str(&rendered).expect("an array");
        assert_eq!(rows.len(), 5);
        let mut anthropic = 0;
        for row in &rows {
            if row["name"] == "bob@example.com/codex" {
                assert_eq!(
                    row["codexRoute"],
                    serde_json::to_value(&route).expect("a route")
                );
                assert_eq!(
                    row["codexTraffic"],
                    serde_json::to_value(&traffic).expect("the traffic")
                );
            } else {
                anthropic += 1;
                assert!(row.get("codexRoute").is_none(), "{row}");
                assert!(row.get("codexTraffic").is_none(), "{row}");
            }
        }
        assert_eq!(anthropic, 4);
    }

    /// The fixture is only worth pinning if it is *representative*: a sample
    /// covering one shape would pin one shape. Asserted against the committed
    /// bytes rather than the renderer, so a fixture regenerated from a
    /// narrowed snapshot fails here too.
    #[test]
    fn status_contract_fixture_covers_every_rendered_shape() {
        let committed = fs::read_to_string(status_contract_fixture_path())
            .expect("committed contract fixture is readable");
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&committed).expect("fixture is a bare JSON array");
        assert_eq!(rows.len(), 5, "one row per covered shape");

        let states: Vec<&str> = rows
            .iter()
            .map(|r| r["quotaState"].as_str().expect("quotaState is a string"))
            .collect();
        assert!(states.contains(&"ok"), "{states:?}");
        assert!(states.contains(&"near"), "{states:?}");
        assert!(states.contains(&"spent"), "{states:?}");

        let probes: Vec<&str> = rows
            .iter()
            .map(|r| r["probeStatus"].as_str().expect("probeStatus is a string"))
            .collect();
        assert!(probes.contains(&"never"), "{probes:?}");
        assert!(probes.contains(&"ok"), "{probes:?}");
        assert!(probes.contains(&"rate-limited"), "{probes:?}");
        assert!(probes.contains(&"error"), "{probes:?}");

        // The never-probed row: every optional the Swift model types as
        // optional really is null somewhere in this sample. `get`, not
        // indexing — indexing a MISSING key also yields Null, which would let a
        // deleted key pass as a null.
        let never = rows
            .iter()
            .find(|r| r["probeStatus"] == "never")
            .expect("a never-probed row");
        for key in [
            "quota",
            "fiveHour",
            "sevenDay",
            "sevenDayOi",
            "cacheHitRatio",
        ] {
            assert_eq!(
                never.get(key),
                Some(&serde_json::Value::Null),
                "{key} is present AND null on the never-probed row: {never}"
            );
        }
        assert_eq!(never["disabled"], serde_json::json!(true));

        // At least one row carries a `held` window, one carries a probe error,
        // and one carries a stream error — the three nested/optional shapes the
        // panel renders differently.
        assert!(
            rows.iter()
                .any(|r| !r["held"].as_array().expect("held is an array").is_empty()),
            "some row is held: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["probeError"].is_string()),
            "some row carries a probe error: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["lastStreamError"].is_string()),
            "some row carries a stream error: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["freeAtMs"].is_i64()),
            "some row pins freeAtMs as a number: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["rateLimitedUntilMs"].is_i64()),
            "some row pins rateLimitedUntilMs as a number: {committed}"
        );
        assert!(
            rows.iter().any(|r| !r["groups"]
                .as_array()
                .expect("groups is an array")
                .is_empty()),
            "some row carries non-empty groups: {committed}"
        );
        assert!(
            rows.iter().any(|r| r["groups"]
                .as_array()
                .expect("groups is an array")
                .is_empty()),
            "some row carries no groups, rendered as [] not null: {committed}"
        );
        // `groupColors` is server-wide (like `http1Only`), so it is identical
        // on every row — checking row 0 checks all of them. Real entries, not
        // an empty object: the fixture must exercise both a configured color
        // ("dev") and a derived one ("codereview").
        let group_colors = rows[0]["groupColors"]
            .as_object()
            .expect("groupColors is an object");
        assert_eq!(
            group_colors.get("dev"),
            Some(&serde_json::json!("#0a84ff")),
            "configured color round-trips onto the wire: {committed}"
        );
        assert!(
            group_colors
                .get("codereview")
                .and_then(|v| v.as_str())
                .is_some_and(|hex| hex.starts_with('#') && hex.len() == 7),
            "derived color is still a real hex value on the wire: {committed}"
        );

        // Public repo: the fixture carries no real account data. A positive
        // control on the same probe — the fake domain really is present — so an
        // empty match cannot read as "clean".
        assert!(
            committed.contains("@example.com"),
            "positive control: the fixture uses example.com addresses"
        );
        for row in &rows {
            let name = row["name"].as_str().expect("name is a string");
            let email = name.strip_suffix("/codex").unwrap_or(name);
            assert!(
                email.ends_with("@example.com"),
                "every fixture account is obviously fake: {name}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // The canonical Codex fixtures TcrBar decodes.
    // ---------------------------------------------------------------------

    /// One of the canonical Codex fixtures, read from where TcrBar's decode tests read it
    /// (`apps/macos/Tests/Fixtures/`). The same file, never a copy: one set of fixtures
    /// with two proofs, the panel's decode and this renderer's output, is what keeps the
    /// producer and the consumer from drifting apart.
    fn canonical_fixture(name: &str) -> serde_json::Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("apps/macos/Tests/Fixtures")
            .join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("cannot read the canonical fixture {}: {e}", path.display())
        });
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("the canonical fixture {} is not JSON: {e}", path.display()))
    }

    /// The canonical fixtures' clock, 2026-01-01T00:00:00Z. Every time in them is an
    /// offset from it.
    const CANONICAL_NOW_MS: i64 = 1_767_225_600_000;
    const SECOND_MS: i64 = 1_000;
    const MINUTE_MS: i64 = 60 * SECOND_MS;
    const HOUR_MS: i64 = 60 * MINUTE_MS;
    const DAY_MS: i64 = 24 * HOUR_MS;
    /// A weekly window's length, 10,080 minutes.
    const WEEK_MS: i64 = 7 * DAY_MS;

    /// The keys a Codex row may carry and an Anthropic row never does.
    const CODEX_ROW_KEYS: [&str; 12] = [
        "quotaWindows",
        "fiveHourWindowMinutes",
        "sevenDayWindowMinutes",
        "limitReachedType",
        "spendControlReached",
        "credits",
        "loginError",
        "codexHome",
        "tokenSyncPending",
        "resetCredits",
        "codexRoute",
        "codexTraffic",
    ];

    /// A Claude row's banked resets on `tcr status`: per kind with the earliest use-by,
    /// `unknown` for an ineligible answer (never `0`), `0` for eligible with none held, and
    /// nothing when never read. The JSON carries the grants and never a grant id.
    #[test]
    fn claude_banked_resets_reach_text_and_json() {
        use tcr_status_wire::{BankedReset, BankedResets, ResetKind};
        let grant = |kind, left, ends| BankedReset {
            kind,
            resets_left: left,
            usable_now: true,
            paused: false,
            requires_limit: false,
            ends_at_ms: ends,
            unavailable_reason: None,
        };
        let held = BankedResets {
            eligible: true,
            reason: None,
            grants: vec![
                grant(ResetKind::Fable, 2, Some(1_891_036_800_000)),
                grant(ResetKind::Weekly, 1, Some(1_891_123_200_000)),
                grant(ResetKind::Weekly, 0, Some(1_800_000_000_000)),
            ],
        };
        assert_eq!(
            claude_resets_token(Some(&held)),
            " resets=weekly:1,fable:2(Dec 4)"
        );
        let surface = BankedResets {
            eligible: false,
            reason: Some("surface".to_owned()),
            grants: Vec::new(),
        };
        assert_eq!(claude_resets_token(Some(&surface)), " resets=unknown");
        let none = BankedResets {
            eligible: true,
            ..surface
        };
        assert_eq!(claude_resets_token(Some(&none)), " resets=0");
        assert_eq!(claude_resets_token(None), "");

        let (mut snapshot, thresholds) = status_contract_snapshot();
        assert_eq!(snapshot.accounts[0].provider, Provider::Anthropic);
        snapshot.accounts[0].codex.banked_resets = Some(held);
        let text = status_text_of(&snapshot, None, &pooled_route());
        assert!(text.contains(" resets=weekly:1,fable:2(Dec 4)"), "{text}");
        let wire: serde_json::Value =
            serde_json::from_str(&status_json(&snapshot, &thresholds, StatusSource::Live)).unwrap();
        assert_eq!(wire[0]["bankedResets"]["eligible"], true);
        assert_eq!(wire[0]["bankedResets"]["grants"][0]["kind"], "fable");
        assert_eq!(wire[0]["bankedResets"]["grants"][0]["resetsLeft"], 2);
        assert!(wire[4].get("bankedResets").is_none());
    }

    #[test]
    fn reset_credits_reach_text_and_json_without_identities() {
        let (mut snapshot, thresholds) = status_contract_snapshot();
        let text = status_text_of(&snapshot, None, &pooled_route());
        assert!(text.contains(" resets=3 reset_expires_at_ms=1800000000000"));
        let wire: serde_json::Value =
            serde_json::from_str(&status_json(&snapshot, &thresholds, StatusSource::Live)).unwrap();
        assert_eq!(
            wire[4]["resetCredits"],
            serde_json::json!({
                "availableCount": 3, "earliestExpiryMs": 1_800_000_000_000i64
            })
        );
        assert!(wire[0].get("resetCredits").is_none());
        snapshot.accounts[4].codex.reset_credits = Some(tcr_status_wire::ResetCredits {
            available_count: 0,
            earliest_expiry_ms: None,
        });
        assert!(codex_tail(&snapshot.accounts[4]).contains(" resets=0"));
        assert!(!codex_tail(&snapshot.accounts[4]).contains("reset_expires_at_ms"));
        snapshot.accounts[4].codex.reset_credits = None;
        assert!(!codex_tail(&snapshot.accounts[4]).contains("resets="));
        snapshot.accounts[0].codex.reset_credits = Some(tcr_status_wire::ResetCredits {
            available_count: 9,
            earliest_expiry_ms: None,
        });
        assert!(!codex_tail(&snapshot.accounts[0]).contains("resets="));
        let wire: serde_json::Value =
            serde_json::from_str(&status_json(&snapshot, &thresholds, StatusSource::Live)).unwrap();
        assert!(wire[0].get("resetCredits").is_none());
        assert!(wire[4].get("resetCredits").is_none());
    }

    #[test]
    fn codex_pending_token_sync_has_a_text_status_hint() {
        let (mut snapshot, _) = status_contract_snapshot();
        let account = &mut snapshot.accounts[0];
        account.provider = Provider::Codex;
        account.codex.token_sync_pending = true;
        assert!(codex_tail(account).contains("token_sync=pending"));
        account.provider = Provider::Anthropic;
        assert!(codex_tail(account).is_empty());
    }

    /// The instant `offset_ms` from the canonical clock.
    fn canonical_at(offset_ms: i64) -> OffsetDateTime {
        fixture_instant(CANONICAL_NOW_MS + offset_ms)
    }

    /// The serving build both fixtures name.
    fn fixture_build() -> BuildInfo {
        BuildInfo {
            sha: "abc1234".to_string(),
            dirty: Some(false),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    /// The fleet-wide Codex facts every canonical Codex row repeats: codex is pooled, and
    /// in the last ten minutes tcr refused twelve requests itself and relayed three on the
    /// client's own credential.
    fn canonical_codex_route_and_traffic()
    -> (tcr_status_wire::CodexRoute, tcr_status_wire::CodexTraffic) {
        (
            tcr_status_wire::CodexRoute {
                pooled: true,
                ..Default::default()
            },
            tcr_status_wire::CodexTraffic {
                refused_10m: 12,
                last_refusal: Some(tcr_status_wire::CodexRefusalKind::UsageLimitReached),
                last_refusal_at_ms: Some(1_767_225_560_000),
                unpooled_10m: 3,
            },
        )
    }

    /// The keys whose values differ between two JSON objects, from the union of both
    /// objects' keys, so a key only one side carries is named too.
    fn differing_keys(got: &serde_json::Value, want: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = got
            .as_object()
            .into_iter()
            .chain(want.as_object())
            .flat_map(|object| object.keys().cloned())
            .collect();
        keys.sort();
        keys.dedup();
        keys.retain(|key| got.get(key) != want.get(key));
        keys
    }

    /// One bucket of a canonical Codex row's usage, as `Totals::to_wire` reports traffic no
    /// configured rate prices, which is all Codex traffic: every request unpriced, so
    /// `costUsd` is `null` once the bucket served anything, and the measured zero `0.0`
    /// while it served nothing. `input` is BASE input, the row's quota counter less cache
    /// reads, as `Totals::to_wire` reports it.
    fn codex_bucket(
        requests: u64,
        input: u64,
        cache_read: u64,
        output: u64,
        reasoning: u64,
    ) -> tcr_status_wire::UsageTotals {
        tcr_status_wire::UsageTotals {
            requests,
            input_tokens: input,
            cache_read_tokens: cache_read,
            output_tokens: output,
            reasoning_tokens: reasoning,
            unpriced_requests: requests,
            cost_usd: if requests == 0 { Some(0.0) } else { None },
            ..Default::default()
        }
    }

    /// A canonical Codex row's usage. Each window opens where the usage tracker opens it,
    /// read off the row's own resets so the two cannot disagree: the five-hour window at
    /// the five-hour reset minus five hours, the weekly window at the weekly reset minus a
    /// week. `todayByModel` holds today's one model once today served anything.
    fn canonical_codex_usage(
        row: &AccountSnapshot,
        today: tcr_status_wire::UsageTotals,
        last_hour: tcr_status_wire::UsageTotals,
        window: Option<tcr_status_wire::UsageTotals>,
        seven_day_window: Option<tcr_status_wire::UsageTotals>,
    ) -> tcr_status_wire::UsageRow {
        let opened = |reset: Option<OffsetDateTime>, length_ms: i64| {
            let reset = reset.expect("a window's usage needs the window's reset");
            (reset.unix_timestamp_nanos() / 1_000_000) as i64 - length_ms
        };
        tcr_status_wire::UsageRow {
            today,
            window: window.map(|totals| tcr_status_wire::UsageWindow {
                since: opened(row.five_hour_reset, crate::usage::FIVE_HOURS_MS),
                totals,
            }),
            last_hour,
            today_by_model: if today.requests > 0 {
                std::iter::once(("gpt-5.1-codex".to_string(), today)).collect()
            } else {
                std::collections::BTreeMap::new()
            },
            seven_day_window: seven_day_window.map(|totals| tcr_status_wire::UsageWindow {
                since: opened(row.seven_day_reset, WEEK_MS),
                totals,
            }),
        }
    }

    /// A canonical Codex row before the facts its state adds: active, probed `ok`, in no
    /// group, nothing served, every Codex fact unreported. Built by [`contract_account`],
    /// so a new [`AccountSnapshot`] field breaks this sample as it breaks the contract one.
    fn canonical_codex_row(
        name: &str,
        priority: i64,
        plan: &str,
        org_uuid: &str,
    ) -> AccountSnapshot {
        AccountSnapshot {
            provider: Provider::Codex,
            ..contract_account(
                name,
                priority,
                "active",
                false,
                None,
                None,
                None,
                None,
                None,
                None,
                ProbeStatus::Ok,
                None,
                QuotaState::Normal,
                &[],
                None,
                (Some(plan), None, None),
                (Some(org_uuid), None),
            )
        }
    }

    /// The snapshot behind `codex-status.json`: its ten rows in fixture order, alice's
    /// Claude row and one Codex row per state, each with a threshold of 0.85.
    ///
    /// Only facts are set here. Every derived key, `quota`, the per-window states, `held`,
    /// `secondsUntilFree`, `cacheHitRatio` and `plan`, is the renderer's to compute, which
    /// is what makes the comparison a proof of the renderer rather than of this function.
    fn canonical_codex_snapshot() -> (StatsSnapshot, Vec<f64>) {
        use crate::stats::GateReason;
        use tcr_status_wire::CodexCredits;

        let at = canonical_at;
        let idle = codex_bucket(0, 0, 0, 0, 0);
        let weekly_only = || CodexFacts {
            quota_windows: Some(vec!["sevenDay".to_string()]),
            seven_day_window_minutes: Some(10_080),
            ..CodexFacts::default()
        };
        let both_windows = || CodexFacts {
            quota_windows: Some(vec!["fiveHour".to_string(), "sevenDay".to_string()]),
            five_hour_window_minutes: Some(300),
            seven_day_window_minutes: Some(10_080),
            ..CodexFacts::default()
        };

        // Claude, OK, in ORCHARD: contract row 1 with its windows moved into the future.
        let mut alice_usage = contract_usage("claude-sonnet-5");
        alice_usage.window = alice_usage.window.map(|w| tcr_status_wire::UsageWindow {
            since: 1_767_217_140_000,
            ..w
        });
        let alice = contract_account(
            "alice@example.com",
            0,
            "active",
            false,
            Some(0.47),
            Some(at(2 * HOUR_MS + 39 * MINUTE_MS)),
            Some(0.20),
            Some(at(4 * DAY_MS)),
            Some(0.62),
            Some(at(4 * DAY_MS)),
            ProbeStatus::Ok,
            None,
            QuotaState::Normal,
            &["orchard"],
            Some(alice_usage),
            (Some("claude_max"), Some("default_claude_max_20x"), None),
            (
                Some("11111111-1111-1111-1111-111111111111"),
                Some("Example Personal"),
            ),
        );

        // OK, one weekly window.
        let mut bob = AccountSnapshot {
            seven_day: Some(0.34),
            seven_day_reset: Some(at(4 * DAY_MS)),
            last_probe: Some(at(-30 * SECOND_MS)),
            requests: 42,
            input_tokens: 1_100_000,
            cache_read_tokens: 902_000,
            output_tokens: 22_000,
            codex: weekly_only(),
            ..canonical_codex_row(
                "bob@example.com/codex",
                1,
                "pro",
                "55555555-5555-4555-8555-555555555555",
            )
        };
        bob.usage = Some(canonical_codex_usage(
            &bob,
            codex_bucket(42, 198_000, 902_000, 22_000, 9_000),
            codex_bucket(7, 30_000, 160_000, 4_100, 1_200),
            None,
            Some(codex_bucket(
                1_480, 11_000_000, 50_000_000, 1_100_000, 420_000,
            )),
        ));

        // SPENT on its weekly window, with credits left.
        let mut carol = AccountSnapshot {
            seven_day: Some(1.0),
            seven_day_reset: Some(at(3 * DAY_MS + 4 * HOUR_MS)),
            last_probe: Some(at(-30 * SECOND_MS)),
            quota_state: QuotaState::Exhausted,
            gate: GateReason::SevenDay,
            free_at: Some(at(3 * DAY_MS + 4 * HOUR_MS)),
            requests: 310,
            input_tokens: 5_400_000,
            cache_read_tokens: 4_806_000,
            output_tokens: 61_000,
            codex: CodexFacts {
                limit_reached_type: Some("rate_limit_reached".to_string()),
                credits: Some(CodexCredits {
                    has_credits: true,
                    unlimited: false,
                    balance: Some("12.50".to_string()),
                }),
                ..weekly_only()
            },
            ..canonical_codex_row(
                "carol@example.com",
                2,
                "pro",
                "66666666-6666-4666-8666-666666666666",
            )
        };
        carol.usage = Some(canonical_codex_usage(
            &carol,
            codex_bucket(310, 594_000, 4_806_000, 61_000, 21_000),
            idle,
            None,
            Some(codex_bucket(
                2_900,
                20_000_000,
                140_000_000,
                5_400_000,
                2_100_000,
            )),
        ));

        // OK, both windows.
        let mut dana = AccountSnapshot {
            five_hour: Some(0.18),
            five_hour_reset: Some(at(3 * HOUR_MS + 2 * MINUTE_MS)),
            seven_day: Some(0.25),
            seven_day_reset: Some(at(5 * DAY_MS)),
            last_probe: Some(at(-30 * SECOND_MS)),
            requests: 96,
            input_tokens: 2_000_000,
            cache_read_tokens: 1_700_000,
            output_tokens: 260_000,
            codex: both_windows(),
            ..canonical_codex_row(
                "dana@example.com",
                3,
                "plus",
                "77777777-7777-4777-8777-777777777777",
            )
        };
        dana.usage = Some(canonical_codex_usage(
            &dana,
            codex_bucket(96, 300_000, 1_700_000, 260_000, 70_000),
            codex_bucket(9, 30_000, 150_000, 24_000, 6_000),
            Some(codex_bucket(31, 100_000, 540_000, 210_000, 55_000)),
            Some(codex_bucket(400, 1_400_000, 7_600_000, 640_000, 180_000)),
        ));

        // NEEDS RE-LOGIN, imported from a folder.
        let mut erin = AccountSnapshot {
            status: "error".to_string(),
            five_hour: Some(0.0),
            five_hour_reset: None,
            seven_day: Some(0.40),
            seven_day_reset: Some(at(2 * DAY_MS)),
            probe_status: ProbeStatus::Error,
            last_probe: Some(at(-MINUTE_MS)),
            probe_error: Some("refresh_token_reused".to_string()),
            gate: GateReason::Login,
            codex: CodexFacts {
                login_error: Some("refresh_token_reused".to_string()),
                codex_home: Some("/Users/example/.codex".to_string()),
                ..both_windows()
            },
            ..canonical_codex_row(
                "erin@example.com/codex",
                4,
                "team",
                "88888888-8888-4888-8888-888888888888",
            )
        };
        erin.usage = Some(canonical_codex_usage(
            &erin,
            idle,
            idle,
            None,
            Some(codex_bucket(120, 1_000_000, 4_000_000, 140_000, 50_000)),
        ));

        // REFUSED with no window full: the spend control.
        let mut gil = AccountSnapshot {
            five_hour: Some(0.15),
            five_hour_reset: Some(at(3 * HOUR_MS + 2 * MINUTE_MS)),
            seven_day: Some(0.10),
            seven_day_reset: Some(at(6 * DAY_MS)),
            last_probe: Some(at(-MINUTE_MS)),
            gate: GateReason::Rejected,
            requests: 24,
            input_tokens: 400_000,
            cache_read_tokens: 284_000,
            output_tokens: 52_000,
            codex: CodexFacts {
                limit_reached_type: Some("rate_limit_reached".to_string()),
                spend_control_reached: true,
                ..both_windows()
            },
            ..canonical_codex_row(
                "gil@example.com",
                5,
                "team",
                "99999999-9999-4999-8999-999999999999",
            )
        };
        gil.usage = Some(canonical_codex_usage(
            &gil,
            codex_bucket(24, 116_000, 284_000, 52_000, 14_000),
            codex_bucket(2, 9_000, 21_000, 3_000, 800),
            Some(codex_bucket(20, 96_000, 234_000, 48_000, 13_000)),
            Some(codex_bucket(140, 900_000, 2_100_000, 310_000, 90_000)),
        ));

        // REFUSED, still held: throttled for the 47 minutes its hold has left, since a Codex
        // hold never outlasts an hour.
        let mut hal = AccountSnapshot {
            status: "throttled".to_string(),
            seven_day: Some(0.60),
            seven_day_reset: Some(at(5 * DAY_MS)),
            last_probe: Some(at(-2 * MINUTE_MS)),
            gate: GateReason::Hold,
            free_at: Some(at(47 * MINUTE_MS)),
            rate_limited_until: Some(at(47 * MINUTE_MS)),
            requests: 77,
            input_tokens: 1_900_000,
            cache_read_tokens: 1_520_000,
            output_tokens: 40_000,
            codex: CodexFacts {
                limit_reached_type: Some("rate_limit_reached".to_string()),
                ..weekly_only()
            },
            ..canonical_codex_row(
                "hal@example.com",
                6,
                "pro",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            )
        };
        hal.usage = Some(canonical_codex_usage(
            &hal,
            codex_bucket(77, 380_000, 1_520_000, 40_000, 15_000),
            idle,
            None,
            Some(codex_bucket(900, 8_000_000, 32_000_000, 1_900_000, 700_000)),
        ));

        // NEAR on its five-hour window, in ORCHARD.
        let mut ivy = AccountSnapshot {
            five_hour: Some(0.88),
            five_hour_reset: Some(at(52 * MINUTE_MS)),
            seven_day: Some(0.17),
            seven_day_reset: Some(at(6 * DAY_MS)),
            last_probe: Some(at(-40 * SECOND_MS)),
            quota_state: QuotaState::NearLimit,
            gate: GateReason::FiveHour,
            free_at: Some(at(52 * MINUTE_MS)),
            groups: vec!["orchard".to_string()],
            requests: 60,
            input_tokens: 1_300_000,
            cache_read_tokens: 1_014_000,
            output_tokens: 260_000,
            codex: both_windows(),
            ..canonical_codex_row(
                "ivy@example.com",
                7,
                "plus",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            )
        };
        ivy.usage = Some(canonical_codex_usage(
            &ivy,
            codex_bucket(60, 286_000, 1_014_000, 260_000, 70_000),
            codex_bucket(20, 90_000, 330_000, 80_000, 22_000),
            Some(codex_bucket(52, 240_000, 860_000, 210_000, 60_000)),
            Some(codex_bucket(300, 1_600_000, 5_400_000, 900_000, 250_000)),
        ));

        // OK with no usage window at all, and unlimited credits.
        let mut jo = AccountSnapshot {
            last_probe: Some(at(-50 * SECOND_MS)),
            requests: 30,
            input_tokens: 300_000,
            cache_read_tokens: 225_000,
            output_tokens: 6_000,
            codex: CodexFacts {
                quota_windows: Some(Vec::new()),
                credits: Some(CodexCredits {
                    has_credits: true,
                    unlimited: true,
                    balance: None,
                }),
                ..CodexFacts::default()
            },
            ..canonical_codex_row(
                "jo@example.com",
                8,
                "business",
                "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            )
        };
        jo.usage = Some(canonical_codex_usage(
            &jo,
            codex_bucket(30, 75_000, 225_000, 6_000, 1_000),
            idle,
            None,
            None,
        ));

        // UNREACHABLE: its one group, RESEARCH, is reserved.
        let mut kim = AccountSnapshot {
            seven_day: Some(0.05),
            seven_day_reset: Some(at(6 * DAY_MS)),
            last_probe: Some(at(-3 * MINUTE_MS)),
            gate: GateReason::Reserved,
            groups: vec!["research".to_string()],
            reserved_groups: vec!["research".to_string()],
            codex: weekly_only(),
            ..canonical_codex_row(
                "kim@example.com",
                9,
                "pro",
                "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
            )
        };
        kim.usage = Some(canonical_codex_usage(
            &kim,
            idle,
            idle,
            None,
            Some(codex_bucket(40, 200_000, 700_000, 30_000, 9_000)),
        ));

        let accounts = vec![alice, bob, carol, dana, erin, gil, hal, ivy, jo, kim];
        let thresholds = vec![0.85; accounts.len()];
        (
            StatsSnapshot {
                accounts,
                current: None,
                current_by_provider: crate::stats::CurrentByProvider::default(),
                recent: Vec::new(),
                sessions: Vec::new(),
                wire_sessions: Vec::new(),
                wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
            },
            thresholds,
        )
    }

    /// The two sessions behind `codex-sessions.json`: a Claude session with a running
    /// `Bash`, and a Codex session with a running `exec_command`. That is codex's default,
    /// interactive tool, which has no timeout argument, so the call carries none.
    fn canonical_codex_sessions() -> Vec<tcr_status_wire::SessionRow> {
        let ms = |offset_ms: i64| CANONICAL_NOW_MS + offset_ms;
        let ramp = |zeros: usize, tail: &[u16]| -> Vec<u16> {
            std::iter::repeat_n(0, zeros)
                .chain(tail.iter().copied())
                .collect()
        };
        let bucket = |tool: &str, calls: u64, seconds_p50: f64| tcr_status_wire::ToolBucketRow {
            tool: tool.to_string(),
            calls,
            seconds_p50,
            ..Default::default()
        };
        let slow = |tool: &str, seconds: f64, head: &str, class: Option<&str>, ended_ms: i64| {
            tcr_status_wire::SlowToolRow {
                tool: tool.to_string(),
                seconds,
                command_head: Some(head.to_string()),
                command_class: class.map(str::to_string),
                ended_ms,
            }
        };
        let claude = tcr_status_wire::SessionRow {
            session_id: "a1f3c2d4-0000-4000-8000-000000000001".to_string(),
            account: Some("alice@example.com".to_string()),
            model: Some("claude-opus-5".to_string()),
            first_seen_ms: ms(-40 * MINUTE_MS),
            last_seen_ms: ms(-3 * SECOND_MS),
            requests: 24,
            input_tokens: 180_000,
            output_tokens: 9_200,
            cache_read_tokens: 160_000,
            cache_creation_tokens: 12_000,
            tools: tcr_status_wire::SessionToolsRow {
                calls: 12,
                running: vec![tcr_status_wire::RunningToolRow {
                    tool: "Bash".to_string(),
                    started_ms: ms(-20 * SECOND_MS),
                    command_head: Some("cargo test".to_string()),
                    command_class: Some("build".to_string()),
                    timeout_seconds: None,
                }],
                by_tool: vec![bucket("Bash", 8, 2.5), bucket("Read", 4, 0.1)],
                // The ten slowest of its twelve calls.
                slowest: vec![
                    slow(
                        "Bash",
                        12.0,
                        "cargo build --release",
                        Some("build"),
                        ms(-295 * SECOND_MS),
                    ),
                    slow(
                        "Bash",
                        6.0,
                        "cargo build",
                        Some("build"),
                        ms(-230 * SECOND_MS),
                    ),
                    slow(
                        "Bash",
                        3.0,
                        "rg -n TODO",
                        Some("search"),
                        ms(-175 * SECOND_MS),
                    ),
                    slow(
                        "Bash",
                        2.5,
                        "cargo test -p example",
                        Some("build"),
                        ms(-250 * SECOND_MS),
                    ),
                    slow(
                        "Bash",
                        2.5,
                        "git status",
                        Some("git-local"),
                        ms(-160 * SECOND_MS),
                    ),
                    slow(
                        "Bash",
                        2.0,
                        "git status",
                        Some("git-local"),
                        ms(-320 * SECOND_MS),
                    ),
                    slow(
                        "Bash",
                        2.0,
                        "cargo test",
                        Some("build"),
                        ms(-110 * SECOND_MS),
                    ),
                    slow(
                        "Bash",
                        1.5,
                        "git status",
                        Some("git-local"),
                        ms(-125 * SECOND_MS),
                    ),
                    slow(
                        "Read",
                        0.4,
                        "Read /Users/example/git/tcr/src/session_wire.rs",
                        None,
                        ms(-280 * SECOND_MS),
                    ),
                    slow(
                        "Read",
                        0.1,
                        "Read /Users/example/git/tcr/src/proxy.rs",
                        None,
                        ms(-350 * SECOND_MS),
                    ),
                ],
                ..Default::default()
            },
            // Twenty of its 24 requests fall in the ring; the other four came before it.
            req_per_minute: ramp(22, &[1, 2, 3, 2, 4, 3, 5, 0]),
            cost_usd: 1.2034,
            cache_ttl_secs: Some(3600),
            provider: None,
            unpriced_requests: 0,
            reasoning_tokens: 0,
        };
        let codex = tcr_status_wire::SessionRow {
            session_id: "019a3c5e-0000-7000-8000-000000000002".to_string(),
            account: Some("bob@example.com/codex".to_string()),
            model: Some("gpt-5.1-codex".to_string()),
            first_seen_ms: ms(-175 * SECOND_MS),
            last_seen_ms: ms(-50 * SECOND_MS),
            requests: 7,
            input_tokens: 95_000,
            output_tokens: 4_100,
            cache_read_tokens: 80_000,
            cache_creation_tokens: 0,
            tools: tcr_status_wire::SessionToolsRow {
                calls: 3,
                running: vec![tcr_status_wire::RunningToolRow {
                    tool: "exec_command".to_string(),
                    started_ms: ms(-45 * SECOND_MS),
                    command_head: Some("cargo build".to_string()),
                    command_class: Some("build".to_string()),
                    timeout_seconds: None,
                }],
                by_tool: vec![
                    bucket("exec_command", 2, 4.0),
                    bucket("apply_patch", 1, 0.2),
                ],
                slowest: vec![
                    slow(
                        "exec_command",
                        4.0,
                        "cargo test",
                        Some("build"),
                        ms(-115 * SECOND_MS),
                    ),
                    slow(
                        "exec_command",
                        4.0,
                        "cargo check",
                        Some("build"),
                        ms(-55_500),
                    ),
                    slow("apply_patch", 0.2, "apply_patch", None, ms(-94_800)),
                ],
                ..Default::default()
            },
            req_per_minute: ramp(26, &[2, 3, 2, 0]),
            cost_usd: 0.0,
            cache_ttl_secs: None,
            provider: Some("codex".to_string()),
            unpriced_requests: 7,
            reasoning_tokens: 1_200,
        };
        vec![claude, codex]
    }

    /// `status_rows_json(account_status_rows_at(..))`, given the facts behind `codex-status.json`, emits exactly
    /// the rows TcrBar decodes from that file: every key and every derived value, compared
    /// as `serde_json::Value`s with no normalisation and no rounding.
    ///
    /// The fixture belongs to the panel, whose decode tests read the same file; this is
    /// the producer's half of that contract. A key renamed, dropped or computed differently
    /// fails here before it reaches a screen. The render runs at the fixture's own clock,
    /// so `held` and `secondsUntilFree` come out as committed with nothing to normalise.
    #[test]
    fn canonical_codex_status_matches_the_renderer() {
        let (snapshot, thresholds) = canonical_codex_snapshot();
        let (route, traffic) = canonical_codex_route_and_traffic();
        let group_colors: std::collections::BTreeMap<String, String> = [
            ("orchard".to_string(), "#30d158".to_string()),
            ("research".to_string(), "#bf5af2".to_string()),
        ]
        .into_iter()
        .collect();
        let rendered = status_rows_json(account_status_rows_at(
            fixture_instant(CANONICAL_NOW_MS),
            &snapshot,
            &thresholds,
            StatusSource::Live,
            Some(&fixture_build()),
            false,
            None,
            &group_colors,
            Some(&route),
            Some(&traffic),
        ));
        let rendered: Vec<serde_json::Value> =
            serde_json::from_str(&rendered).expect("the renderer emits a JSON array");
        let serde_json::Value::Array(fixture) = canonical_fixture("codex-status.json") else {
            panic!("codex-status.json is a bare array");
        };

        assert_eq!(fixture.len(), 10, "codex-status.json carries ten rows");
        assert_eq!(rendered.len(), 10, "the renderer emits one row per account");
        let names = |rows: &[serde_json::Value]| -> Vec<String> {
            rows.iter()
                .map(|r| r["name"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        assert_eq!(
            names(&rendered),
            names(&fixture),
            "the renderer emits the fixture's rows in the fixture's order"
        );
        // Row by row, naming the keys: one assertion over the whole array prints over a
        // thousand lines and points at none of them.
        for (got, want) in rendered.iter().zip(&fixture) {
            assert!(
                got == want,
                "{} differs from the canonical fixture on {:?}\nrendered: {got:#}\nfixture: {want:#}",
                want["name"],
                differing_keys(got, want)
            );
        }
    }

    /// `render_sessions_json` emits exactly `codex-sessions.json` for the two sessions
    /// behind it.
    ///
    /// The session shape the panel decodes, pinned the way the status rows are: the Codex
    /// keys present on the Codex session, and absent, never `null` or `0`, on the Claude one,
    /// whose bytes are the ones it had before Codex existed.
    #[test]
    fn canonical_codex_sessions_match_the_renderer() {
        let rendered: serde_json::Value = serde_json::from_str(&render_sessions_json(
            true,
            &canonical_codex_sessions(),
            None,
            None,
        ))
        .expect("valid JSON");
        let fixture = canonical_fixture("codex-sessions.json");
        let (Some(got), Some(want)) = (
            rendered["sessions"].as_array(),
            fixture["sessions"].as_array(),
        ) else {
            panic!("both documents carry a sessions array: {rendered} / {fixture}");
        };
        for (got, want) in got.iter().zip(want) {
            assert!(
                got == want,
                "session {} differs from the canonical fixture on {:?}\nrendered: {got:#}\nfixture: {want:#}",
                want["sessionId"],
                differing_keys(got, want)
            );
        }
        assert_eq!(
            rendered, fixture,
            "render_sessions_json no longer emits codex-sessions.json"
        );
    }

    /// **A Codex row's held window is labelled by its length**, in the JSON `held` array and in
    /// the text `held=` token alike: a 30-day weekly window reads `30d`, where the fixed label
    /// would say `7d`, the length the plan does not have. Without a reported length the label
    /// is the fixed one, and an Anthropic row keeps `7d` whatever its facts hold.
    ///
    /// Watched red: label every window by its slot, as before, and the 30-day row reads `7d`.
    #[test]
    fn a_codex_held_window_is_labelled_by_its_length() {
        let now = fixture_instant(CANONICAL_NOW_MS);
        let full_weekly = |provider: Provider, minutes: Option<u32>| AccountSnapshot {
            provider,
            seven_day: Some(1.0),
            seven_day_reset: Some(canonical_at(3 * DAY_MS)),
            codex: CodexFacts {
                seven_day_window_minutes: minutes,
                ..CodexFacts::default()
            },
            ..canonical_codex_row(
                "bob@example.com/codex",
                0,
                "pro",
                "55555555-5555-4555-8555-555555555555",
            )
        };
        let held_labels = |row: AccountSnapshot| -> (Vec<String>, String) {
            let snapshot = StatsSnapshot {
                accounts: vec![row.clone()],
                current: None,
                current_by_provider: crate::stats::CurrentByProvider::default(),
                recent: Vec::new(),
                sessions: Vec::new(),
                wire_sessions: Vec::new(),
                wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
            };
            let json = status_rows_json(account_status_rows_at(
                now,
                &snapshot,
                &[0.85],
                StatusSource::Live,
                None,
                false,
                None,
                &std::collections::BTreeMap::new(),
                None,
                None,
            ));
            let rows: Vec<serde_json::Value> =
                serde_json::from_str(&json).expect("the renderer emits a JSON array");
            let held = rows[0]["held"]
                .as_array()
                .expect("every row carries held")
                .iter()
                .map(|h| h["window"].as_str().unwrap_or_default().to_string())
                .collect();
            (held, held_suffix(&row, 0.85, now))
        };

        let (json, text) = held_labels(full_weekly(Provider::Codex, Some(43_200)));
        assert_eq!(
            json,
            vec!["30d".to_string()],
            "a 30-day window, on the wire"
        );
        assert!(
            text.starts_with(" held=30d "),
            "and in the text token: {text}"
        );

        let (json, text) = held_labels(full_weekly(Provider::Codex, None));
        assert_eq!(
            json,
            vec!["7d".to_string()],
            "no reported length: the fixed label"
        );
        assert!(text.starts_with(" held=7d "), "{text}");

        let (json, text) = held_labels(full_weekly(Provider::Anthropic, Some(43_200)));
        assert_eq!(
            json,
            vec!["7d".to_string()],
            "an Anthropic row is unchanged"
        );
        assert!(text.starts_with(" held=7d "), "{text}");
    }

    /// An Anthropic row renders exactly as the committed contract fixture holds it, with no
    /// Codex key: with no Codex fact, and again with every Codex fact set and a Codex route
    /// and traffic passed in.
    ///
    /// This is the compatibility promise for a Claude-only fleet: `tcr status --json` prints
    /// the bytes it always did. The renderer writes the Codex keys on Codex rows only, so
    /// neither a snapshot that wrongly carries a Codex fact nor a fleet-wide route can reach
    /// an Anthropic row. Compared with the committed file, never a regenerated copy.
    #[test]
    fn anthropic_rows_serialize_with_no_new_key() {
        let (mut snapshot, mut thresholds) = status_contract_snapshot();
        snapshot.accounts.truncate(4);
        thresholds.truncate(4);
        assert!(
            snapshot
                .accounts
                .iter()
                .all(|a| a.provider == Provider::Anthropic),
            "the contract's first four rows are its Anthropic rows"
        );
        assert_eq!(thresholds, vec![0.9; 4]);

        let committed: Vec<serde_json::Value> = serde_json::from_str(
            &fs::read_to_string(status_contract_fixture_path())
                .expect("committed contract fixture is readable"),
        )
        .expect("the contract fixture is a bare JSON array");
        let group_colors: std::collections::BTreeMap<String, String> = [
            (
                "codereview".to_string(),
                config::derive_group_color("codereview"),
            ),
            ("dev".to_string(), "#0a84ff".to_string()),
        ]
        .into_iter()
        .collect();
        let (route, traffic) = canonical_codex_route_and_traffic();
        let render = |snapshot: &StatsSnapshot| -> Vec<serde_json::Value> {
            let json = status_rows_json(account_status_rows_at(
                // After every instant in the contract sample, so each clock-derived field
                // clamps exactly as it does in the committed file.
                fixture_instant(1_767_398_400_000),
                snapshot,
                &thresholds,
                StatusSource::Live,
                Some(&fixture_build()),
                false,
                None,
                &group_colors,
                Some(&route),
                Some(&traffic),
            ));
            serde_json::from_str(&json).expect("the renderer emits a JSON array")
        };
        let check = |pass: &str, rows: Vec<serde_json::Value>| {
            assert_eq!(rows.len(), 4, "{pass}: one row per account");
            for (row, committed) in rows.iter().zip(&committed) {
                let name = row["name"].as_str().expect("every row has a name");
                for key in CODEX_ROW_KEYS {
                    assert!(row.get(key).is_none(), "{pass}: {name} carries {key}");
                }
                if let Some(usage) = row.get("usage") {
                    assert!(
                        usage.get("sevenDayWindow").is_none(),
                        "{pass}: {name}'s usage carries sevenDayWindow"
                    );
                    let mut buckets = vec![&usage["today"], &usage["lastHour"]];
                    buckets.extend(usage.get("window"));
                    buckets.extend(
                        usage["todayByModel"]
                            .as_object()
                            .expect("todayByModel is an object")
                            .values(),
                    );
                    for bucket in buckets {
                        assert!(
                            bucket.get("reasoningTokens").is_none(),
                            "{pass}: a bucket of {name}'s usage carries reasoningTokens: {bucket}"
                        );
                    }
                }
                assert_eq!(
                    row, committed,
                    "{pass}: {name} is not the committed contract row"
                );
            }
        };

        check("with no Codex fact", render(&snapshot));

        for account in &mut snapshot.accounts {
            account.codex = CodexFacts {
                quota_windows: Some(vec!["fiveHour".to_string(), "sevenDay".to_string()]),
                five_hour_window_minutes: Some(300),
                seven_day_window_minutes: Some(10_080),
                limit_reached_type: Some("rate_limit_reached".to_string()),
                spend_control_reached: true,
                credits: Some(tcr_status_wire::CodexCredits {
                    has_credits: true,
                    unlimited: false,
                    balance: Some("12.50".to_string()),
                }),
                login_error: Some("refresh_token_reused".to_string()),
                codex_home: Some("/Users/example/.codex".to_string()),
                token_sync_pending: true,
                banked_resets: None,
                auto_reset: None,
                reset_credits: Some(tcr_status_wire::ResetCredits {
                    available_count: 3,
                    earliest_expiry_ms: Some(1_800_000_000_000),
                }),
            };
        }
        check("with every Codex fact set", render(&snapshot));
    }

    /// `groups` on the JSON path: an array for a labelled account (including
    /// multi-membership), and `[]` — never `null` — for an unlabelled one. This
    /// is config, not a serving counter, so it does not get the
    /// null-on-offline treatment other fields get.
    #[test]
    fn groups_render_as_array_never_null() {
        let (snapshot, thresholds) = status_contract_snapshot();
        let json = status_json(&snapshot, &thresholds, StatusSource::Live);
        let alice = status_row(&json, "alice@example.com");
        assert_eq!(
            alice["groups"],
            serde_json::json!(["codereview", "dev"]),
            "labelled account carries its groups: {alice}"
        );
        let dave = status_row(&json, "dave@example.com");
        assert_eq!(
            dave["groups"],
            serde_json::json!([]),
            "unlabelled account renders [], never null: {dave}"
        );
    }

    /// `groupColors` is FLEET-wide, repeated on every row exactly like
    /// `http1Only` — not filtered down to the groups the row itself belongs
    /// to. Dave's row carries no groups but must still see `codereview`'s
    /// color, and a group nobody on this fleet is even labelled with (`ops`)
    /// still appears: the map is whatever the caller passes, not derived
    /// per-row from `groups`.
    #[test]
    fn group_colors_is_fleet_wide_not_filtered_to_the_rows_own_groups() {
        let (snapshot, thresholds) = status_contract_snapshot();
        let group_colors: std::collections::BTreeMap<String, String> = [
            ("codereview".to_string(), "#5163d6".to_string()),
            ("dev".to_string(), "#0a84ff".to_string()),
            ("ops".to_string(), "#ff9500".to_string()),
        ]
        .into_iter()
        .collect();
        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Live,
            None,
            false,
            None,
            &group_colors,
        );
        let dave = status_row(&json, "dave@example.com");
        assert_eq!(
            dave["groups"],
            serde_json::json!([]),
            "dave carries no groups of his own"
        );
        assert_eq!(
            dave["groupColors"],
            serde_json::json!({
                "codereview": "#5163d6",
                "dev": "#0a84ff",
                "ops": "#ff9500",
            }),
            "dave's row still carries the full fleet map, including groups \
             he does not belong to: {dave}"
        );
    }

    /// `groupColors` on a fleet with no groups at all: `{}`, never `null` —
    /// same never-`null` contract `groups`/`reservedGroups` already carry.
    #[test]
    fn group_colors_is_empty_object_never_null_when_no_groups() {
        let (snapshot, thresholds) = status_contract_snapshot();
        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Live,
            None,
            false,
            None,
            &std::collections::BTreeMap::new(),
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
        for row in &rows {
            assert_eq!(
                row["groupColors"],
                serde_json::json!({}),
                "empty fleet groups render as {{}}, never null: {row}"
            );
        }
    }

    /// `groups=` on the text path: a greppable `groups=codereview,dev` token for
    /// a labelled account, and the token omitted entirely (not `groups=`) for an
    /// unlabelled one — the same "nothing to say" idiom as `fable=` and
    /// `last_stream_error=`.
    #[test]
    fn groups_text_line_greppable_and_omitted_when_empty() {
        let (snapshot, _thresholds) = status_contract_snapshot();
        let text = render_accounts(&snapshot, StatusSource::Live);
        let alice_line = text
            .lines()
            .find(|l| l.contains("alice@example.com"))
            .expect("alice line");
        assert!(
            alice_line.contains("groups=codereview,dev"),
            "labelled account's groups are greppable: {alice_line}"
        );
        let dave_line = text
            .lines()
            .find(|l| l.contains("dave@example.com"))
            .expect("dave line");
        assert!(
            !dave_line.contains("groups="),
            "unlabelled account omits the token entirely: {dave_line}"
        );
    }

    /// `plan=` on the text path: the derived label, greppable, on every account
    /// that has been profiled — and the token omitted ENTIRELY, never
    /// `plan=unknown`, for one that has not. This is the only token that tells
    /// two rows sharing an email apart, so a fabricated value is worse than
    /// silence.
    #[test]
    fn plan_text_line_greppable_and_omitted_when_never_profiled() {
        let (snapshot, _thresholds) = status_contract_snapshot();
        let text = render_accounts(&snapshot, StatusSource::Live);
        let line_for = |name: &str| {
            text.lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("{name} line"))
                .to_string()
        };

        let alice = line_for("alice@example.com");
        assert!(
            alice.contains("plan=Max 20x"),
            "the multiplier suffix is what a Max row shows: {alice}"
        );
        let bob = line_for("bob@example.com");
        assert!(
            bob.contains("plan=Team 5x"),
            "a premium Team seat shows its multiplier, not `team_tier_1`: {bob}"
        );
        let carol = line_for("carol@example.com");
        assert!(
            carol.contains("plan=Team Standard"),
            "a standard seat's tier names no multiplier, so the seat shows: {carol}"
        );
        let dave = line_for("dave@example.com");
        assert!(
            !dave.contains("plan="),
            "a never-profiled account omits the token entirely: {dave}"
        );
    }

    /// The same three facts on the JSON path, plus the raw provider strings a
    /// script keys on and the `orgUuid` it narrows an ambiguous name with.
    #[test]
    fn plan_and_org_render_on_the_json_path() {
        let (snapshot, thresholds) = status_contract_snapshot();
        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Live,
            None,
            false,
            None,
            &std::collections::BTreeMap::new(),
        );
        let rows: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        assert_eq!(rows[0]["plan"], "Max 20x");
        assert_eq!(rows[0]["organizationType"], "claude_max");
        assert_eq!(rows[0]["rateLimitTier"], "default_claude_max_20x");
        assert!(
            rows[0]["seatTier"].is_null(),
            "a Max row has no seat — null, not an empty string"
        );
        assert_eq!(rows[0]["orgUuid"], "11111111-1111-1111-1111-111111111111");

        assert_eq!(rows[1]["plan"], "Team 5x");
        assert_eq!(rows[1]["seatTier"], "team_tier_1");
        assert_eq!(rows[2]["plan"], "Team Standard");
        assert_eq!(
            rows[1]["orgUuid"], rows[2]["orgUuid"],
            "two rows in one org report the same orgUuid — what a client narrows by"
        );

        assert!(
            rows[3]["plan"].is_null() && rows[3]["organizationType"].is_null(),
            "a never-profiled row publishes null, never a fabricated default"
        );
        assert!(
            rows[3]["orgUuid"].is_null(),
            "and no org to narrow by either"
        );
    }

    /// The plan is a CONFIG fact, not a serving counter, so it is real on the
    /// offline path too — unlike `requests`/`streamErrorCount`, which are null
    /// there. A `plan` that vanished whenever the proxy was down would be
    /// useless in exactly the moment an operator is reading `tcr status`.
    #[test]
    fn plan_is_reported_on_the_offline_path_too() {
        let (snapshot, thresholds) = status_contract_snapshot();
        let json = render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Offline,
            None,
            false,
            None,
            &std::collections::BTreeMap::new(),
        );
        let rows: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(rows[0]["plan"], "Max 20x");
        assert!(
            rows[0]["requests"].is_null(),
            "the control: a real serving counter IS null offline, so this test is \
             not simply reading a path where nothing is nulled"
        );
    }

    /// `sevenDayOiState` and `sevenDayOiResetAtMs` render from a snapshot that
    /// carries the Fable weekly pair, and both keys are `null` — never absent,
    /// never a fabricated `0`/`"ok"` — for an account whose window was never
    /// learned. Mirrors `sevenDayState`/`sevenDayResetAtMs`'s own contract.
    #[test]
    fn fable_weekly_state_and_reset_render_and_null_when_unlearned() {
        let (snapshot, thresholds) = reserved_groups_snapshot();
        let mut snapshot = snapshot;
        let learned_reset = fixture_instant(1_767_312_000_000);
        // alice: learned, above threshold → "near". dave: never learned → null.
        snapshot.accounts[0].seven_day_oi = Some(0.95);
        snapshot.accounts[0].seven_day_oi_reset = Some(learned_reset);
        snapshot.accounts[1].seven_day_oi = None;
        snapshot.accounts[1].seven_day_oi_reset = None;

        let json = status_json(&snapshot, &thresholds, StatusSource::Live);
        let alice = status_row(&json, "alice@example.com");
        assert_eq!(
            alice["sevenDayOiState"],
            serde_json::json!("near"),
            "learned, above-threshold window renders its state: {alice}"
        );
        assert_eq!(
            alice["sevenDayOiResetAtMs"],
            serde_json::json!(learned_reset.unix_timestamp_nanos() as i64 / 1_000_000),
            "learned window's reset reaches the wire: {alice}"
        );
        let dave = status_row(&json, "dave@example.com");
        assert_eq!(
            dave["sevenDayOiState"],
            serde_json::Value::Null,
            "never-learned window's state is null, not absent: {dave}"
        );
        assert_eq!(
            dave["sevenDayOiResetAtMs"],
            serde_json::Value::Null,
            "never-learned window's reset is null, not absent: {dave}"
        );
    }

    /// `reservedGroups` on the JSON wire: always an array, `[]` — never
    /// `null` — when nothing is reserved, mirroring `groups`'s own contract.
    /// Bridge test #7.
    fn reserved_groups_snapshot() -> (StatsSnapshot, Vec<f64>) {
        let base = AccountSnapshot {
            slot: 0,
            name: String::new(),
            provider: Provider::Anthropic,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            plan_display_name: None,
            org_uuid: None,
            org_name: None,
            priority: 0,
            status: "active".to_string(),
            disabled: false,
            five_hour: None,
            five_hour_reset: None,
            five_hour_absent_on_plan: false,
            seven_day: None,
            seven_day_reset: None,
            seven_day_oi: None,
            seven_day_oi_reset: None,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            last_used: None,
            rate_limited_until: None,
            probe_status: ProbeStatus::Never,
            last_probe: None,
            quota_updated_at: None,
            probe_error: None,
            quota_state: QuotaState::Normal,
            gate: crate::stats::GateReason::Ok,
            free_at: None,
            stream_error_count: 0,
            last_stream_error: None,
            groups: Vec::new(),
            reserved_groups: Vec::new(),
            parked_groups: Vec::new(),
            control_allowed_groups: Vec::new(),
            usage: None,
            codex: Default::default(),
        };
        let accounts = vec![
            AccountSnapshot {
                name: "alice@example.com".to_string(),
                gate: crate::stats::GateReason::Reserved,
                groups: vec!["dev".to_string(), "codereview".to_string()],
                reserved_groups: vec!["codereview".to_string()],
                parked_groups: Vec::new(),
                control_allowed_groups: Vec::new(),
                usage: None,
                ..base.clone()
            },
            AccountSnapshot {
                name: "dave@example.com".to_string(),
                ..base
            },
        ];
        (
            StatsSnapshot {
                accounts,
                current: None,
                current_by_provider: crate::stats::CurrentByProvider::default(),
                recent: Vec::new(),
                sessions: Vec::new(),
                wire_sessions: Vec::new(),
                wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
            },
            vec![0.90, 0.90],
        )
    }

    #[test]
    fn reserved_groups_render_as_array_never_null() {
        let (snapshot, thresholds) = reserved_groups_snapshot();
        let json = status_json(&snapshot, &thresholds, StatusSource::Live);
        let alice = status_row(&json, "alice@example.com");
        assert_eq!(
            alice["reservedGroups"],
            serde_json::json!(["codereview"]),
            "reserved subset of alice's groups: {alice}"
        );
        assert_eq!(alice["gate"], serde_json::json!("reserved"));
        let dave = status_row(&json, "dave@example.com");
        assert_eq!(
            dave["reservedGroups"],
            serde_json::json!([]),
            "nothing reserved renders [], never null: {dave}"
        );
    }

    /// `reservedGroups` round-trips through the `status.rs` wire (the
    /// full server->client payload, not just this file's JSON renderer) —
    /// bridge test #7's other half.
    #[test]
    fn reserved_groups_round_trip_through_status_payload() {
        let (snapshot, thresholds) = reserved_groups_snapshot();
        let payload =
            StatusPayload::from_snapshot(&snapshot, &thresholds, false, None, Default::default());
        let wire = serde_json::to_string(&payload).expect("serialize");
        assert!(
            wire.contains("\"reservedGroups\":[\"codereview\"]"),
            "reservedGroups is camelCase on the wire: {wire}"
        );
        let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
        let (rebuilt, _) = back.into_snapshot();
        assert_eq!(
            rebuilt.accounts[0].reserved_groups,
            vec!["codereview".to_string()]
        );
        assert_eq!(rebuilt.accounts[1].reserved_groups, Vec::<String>::new());
    }

    /// The same two surfaces for `parkedGroups`: an array on the JSON wire —
    /// `[]`, never `null`, when nothing is parked — the `"parked"` gate token
    /// beside it, and a full round trip through the server→client payload.
    /// Both halves in one test because they are one contract: the panel reads
    /// the field to name the group, and the token to know the row is out.
    #[test]
    fn parked_groups_render_as_an_array_and_round_trip_with_their_gate() {
        let (mut snapshot, thresholds) = reserved_groups_snapshot();
        snapshot.accounts[0].gate = crate::stats::GateReason::Parked;
        snapshot.accounts[0].reserved_groups = Vec::new();
        snapshot.accounts[0].parked_groups = vec!["codereview".to_string()];

        let json = status_json(&snapshot, &thresholds, StatusSource::Live);
        let alice = status_row(&json, "alice@example.com");
        assert_eq!(
            alice["parkedGroups"],
            serde_json::json!(["codereview"]),
            "parked subset of alice's groups: {alice}"
        );
        assert_eq!(alice["gate"], serde_json::json!("parked"));
        let dave = status_row(&json, "dave@example.com");
        assert_eq!(
            dave["parkedGroups"],
            serde_json::json!([]),
            "nothing parked renders [], never null: {dave}"
        );

        let payload =
            StatusPayload::from_snapshot(&snapshot, &thresholds, false, None, Default::default());
        let wire = serde_json::to_string(&payload).expect("serialize");
        assert!(
            wire.contains("\"parkedGroups\":[\"codereview\"]"),
            "parkedGroups is camelCase on the wire: {wire}"
        );
        let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
        let (rebuilt, _) = back.into_snapshot();
        assert_eq!(
            rebuilt.accounts[0].parked_groups,
            vec!["codereview".to_string()]
        );
        assert_eq!(rebuilt.accounts[0].gate, crate::stats::GateReason::Parked);
        assert_eq!(rebuilt.accounts[1].parked_groups, Vec::<String>::new());
    }

    /// A server that predates the field sends no `parkedGroups` key at all, and
    /// its rows must still decode — `#[serde(default)]`'s whole job. Same
    /// forward-compat guard `reservedGroups` carries.
    #[test]
    fn a_payload_without_parked_groups_still_deserializes() {
        let (snapshot, thresholds) = reserved_groups_snapshot();
        let payload =
            StatusPayload::from_snapshot(&snapshot, &thresholds, false, None, Default::default());
        let mut wire: serde_json::Value =
            serde_json::to_value(&payload).expect("payload serializes");
        for account in wire["accounts"]
            .as_array_mut()
            .expect("accounts is an array")
        {
            account
                .as_object_mut()
                .expect("account row is an object")
                .remove("parkedGroups")
                .expect("the field was there to remove — the positive control");
        }
        let back: StatusPayload =
            serde_json::from_value(wire).expect("a row with no parkedGroups key still decodes");
        let (rebuilt, _) = back.into_snapshot();
        assert_eq!(rebuilt.accounts[0].parked_groups, Vec::<String>::new());
    }

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
            serde_json::from_str(&fs::read_to_string(&path).expect("read back"))
                .expect("valid json");
        assert_eq!(
            rotated["accounts"][1]["refreshToken"],
            serde_json::json!("rt-b-rotated"),
            "precondition: the rotation must land, or the assertion below proves nothing"
        );

        // The verb finishes and writes.
        cli.accounts[0].priority = Some(9);
        save_after_edit(&path, &cli).expect("the edit must save");

        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read back"))
                .expect("valid json");
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

    /// A snapshot holding `accounts` and nothing else: no account serving, no session, no log.
    fn snapshot_of(accounts: Vec<AccountSnapshot>) -> StatsSnapshot {
        StatsSnapshot {
            accounts,
            current: None,
            current_by_provider: crate::stats::CurrentByProvider::default(),
            recent: Vec::new(),
            sessions: Vec::new(),
            wire_sessions: Vec::new(),
            wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
        }
    }

    /// A Codex row says it is one right after its name, and ends with what only a Codex row can
    /// say, after `parked_groups=` and in a fixed order, each token only when it has something
    /// to say. A Claude row never prints any of it, whatever its Codex facts hold.
    #[test]
    fn codex_account_line_tokens_follow_parked_groups_in_order() {
        use crate::stats::GateReason;
        use tcr_status_wire::CodexCredits;

        let mut kim = canonical_codex_row(
            "kim@example.com",
            9,
            "pro",
            "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        );
        kim.groups = vec!["orchard".to_string(), "research".to_string()];
        kim.reserved_groups = vec!["research".to_string()];
        kim.parked_groups = vec!["orchard".to_string()];
        kim.gate = GateReason::Reserved;
        kim.codex.login_error = Some("refresh_token_reused".to_string());
        kim.codex.limit_reached_type = Some("rate_limit_reached".to_string());
        kim.codex.spend_control_reached = true;
        kim.codex.credits = Some(CodexCredits {
            has_credits: true,
            unlimited: false,
            balance: Some("12.50".to_string()),
        });
        let line = |row: &AccountSnapshot| {
            render_accounts(&snapshot_of(vec![row.clone()]), StatusSource::Live)
        };

        assert_eq!(
            line(&kim),
            "account kim@example.com provider=codex priority=9 plan=ChatGPT Pro 5h=n/a wk=n/a \
             cache=n/a state=ok status=active probe=ok stream_errors=0 \
             groups=orchard,research reserved_groups=research parked_groups=orchard \
             login_error=refresh_token_reused limit_reached=rate_limit_reached \
             spend_control=reached credits=12.50 reachable=no workspace=dddddddd\n"
        );

        let no_workspace = AccountSnapshot {
            org_uuid: None,
            ..kim.clone()
        };
        assert_eq!(
            line(&no_workspace),
            line(&kim).replace(" workspace=dddddddd", ""),
            "no workspace id, no workspace token"
        );

        let mut unlimited = kim.clone();
        unlimited.codex.credits = Some(CodexCredits {
            has_credits: false,
            unlimited: true,
            balance: None,
        });
        assert_eq!(
            line(&unlimited),
            line(&kim).replace(" credits=12.50", " credits=unlimited")
        );

        let mut spent = kim.clone();
        spent.codex.credits = Some(CodexCredits {
            has_credits: false,
            unlimited: false,
            balance: Some("0.00".to_string()),
        });
        assert_eq!(
            line(&spent),
            line(&kim).replace(" credits=12.50", ""),
            "a balance with no credits behind it says nothing"
        );

        let reachable = AccountSnapshot {
            gate: GateReason::Ok,
            ..kim.clone()
        };
        assert_eq!(
            line(&reachable),
            line(&kim).replace(" reachable=no", ""),
            "only a reserved Codex row is unreachable"
        );

        let claude = AccountSnapshot {
            provider: Provider::Anthropic,
            ..kim
        };
        let claude_without_codex_facts = AccountSnapshot {
            codex: CodexFacts::default(),
            ..claude.clone()
        };
        assert_eq!(
            line(&claude),
            line(&claude_without_codex_facts),
            "a Claude row's line never reads its Codex facts"
        );
        for token in [
            " provider=",
            "=none",
            " login_error=",
            " limit_reached=",
            " spend_control=",
            " credits=",
            " reachable=",
            " workspace=",
        ] {
            assert!(
                !line(&claude).contains(token),
                "a Claude row printed {token:?}: {}",
                line(&claude)
            );
        }
    }

    /// `none` is a window the plan does not report, from the row's own list of windows, and
    /// `n/a` keeps meaning a window expected and not yet measured: an unknown list, or a listed
    /// window with no reading. A reading left over for a window the plan dropped does not bring
    /// it back, and a Claude row never reads the list.
    #[test]
    fn five_hour_none_and_weekly_none_on_codex_rows() {
        let windows = |list: &[&str]| Some(list.iter().map(ToString::to_string).collect());
        let codex = |name: &str, quota_windows: Option<Vec<String>>| {
            let mut row =
                canonical_codex_row(name, 0, "pro", "55555555-5555-4555-8555-555555555555");
            row.codex.quota_windows = quota_windows;
            row
        };
        let mut ivy = codex("ivy@example.com", windows(&["sevenDay"]));
        ivy.five_hour = Some(0.5);
        let mut alice = contract_account(
            "alice@example.com",
            0,
            "active",
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            ProbeStatus::Ok,
            None,
            QuotaState::Normal,
            &[],
            None,
            (Some("claude_max"), Some("default_claude_max_20x"), None),
            (None, None),
        );
        alice.codex.quota_windows = windows(&[]);
        let rows = vec![
            codex("bob@example.com/codex", windows(&["sevenDay"])),
            codex("jo@example.com", windows(&[])),
            codex("dana@example.com", windows(&["fiveHour", "sevenDay"])),
            codex("hal@example.com", None),
            ivy,
            alice,
        ];
        let text = render_accounts(&snapshot_of(rows), StatusSource::Live);
        let lines: Vec<&str> = text.lines().collect();
        let expected = [
            ("bob@example.com/codex", " 5h=none wk=n/a cache="),
            ("jo@example.com", " 5h=none wk=none cache="),
            ("dana@example.com", " 5h=n/a wk=n/a cache="),
            ("hal@example.com", " 5h=n/a wk=n/a cache="),
            ("ivy@example.com", " 5h=none "),
            ("alice@example.com", " 5h=n/a wk=n/a cache="),
        ];
        assert_eq!(lines.len(), expected.len(), "{text}");
        for (line, (name, holds)) in lines.iter().zip(expected) {
            assert!(
                line.starts_with(&format!("account {name} ")),
                "rows keep their order: {line}"
            );
            assert!(line.contains(holds), "{name} must hold {holds:?}: {line}");
        }
    }

    /// An Ultra row as measured 2026-11-01: a Max-20x org whose profile names its plan
    /// "Ultra" and whose usage read said `"five_hour": null`. Through the server-to-CLI
    /// wire (the live `tcr status` path), its line says `plan=Ultra` and `5h=none`, while
    /// `5h=n/a` stays for a 5h window that is merely unknown.
    #[test]
    fn an_ultra_row_reads_plan_ultra_and_five_hour_none_through_the_wire() {
        let row = |name: &str| {
            contract_account(
                name,
                0,
                "active",
                false,
                None,
                None,
                None,
                None,
                None,
                None,
                ProbeStatus::Ok,
                None,
                QuotaState::Normal,
                &[],
                None,
                (Some("claude_max"), Some("default_claude_max_20x"), None),
                (None, None),
            )
        };
        let ultra = AccountSnapshot {
            plan_display_name: Some("Ultra".to_string()),
            five_hour_absent_on_plan: true,
            ..row("alice@example.com")
        };
        let unknown = row("bob@example.com");
        let snapshot = snapshot_of(vec![ultra, unknown]);
        let payload =
            StatusPayload::from_snapshot(&snapshot, &[0.95, 0.95], false, None, Default::default());
        let wire = serde_json::to_string(&payload).expect("serialize");
        let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
        let (rebuilt, _) = back.into_snapshot();
        let text = render_accounts(&rebuilt, StatusSource::Live);
        let line_for = |name: &str| {
            text.lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("{name} line in {text}"))
                .to_string()
        };
        let alice = line_for("alice@example.com");
        assert!(
            alice.contains(" plan=Ultra 5h=none wk=n/a "),
            "the provider's plan name and the plan's missing 5h window: {alice}"
        );
        let bob = line_for("bob@example.com");
        assert!(
            bob.contains(" plan=Max 20x 5h=n/a "),
            "no display name and no null: today's label and `n/a`: {bob}"
        );
    }

    /// `tcr status --json` tells a panel which windows a Claude plan has the way it tells it
    /// for a Codex plan: an Ultra row (no 5h window) carries `quotaWindows: ["sevenDay"]`,
    /// so TcrBar drops its 5h meter instead of drawing it as unmeasured. A Claude row
    /// without the flag carries no `quotaWindows` key at all, the bytes it always was.
    #[test]
    fn status_json_lists_only_the_week_for_a_claude_plan_with_no_five_hour_window() {
        let row = |name: &str| {
            contract_account(
                name,
                0,
                "active",
                false,
                None,
                None,
                None,
                None,
                None,
                None,
                ProbeStatus::Ok,
                None,
                QuotaState::Normal,
                &[],
                None,
                (Some("claude_max"), Some("default_claude_max_20x"), None),
                (None, None),
            )
        };
        let ultra = AccountSnapshot {
            plan_display_name: Some("Ultra".to_string()),
            five_hour_absent_on_plan: true,
            ..row("alice@example.com")
        };
        let snapshot = snapshot_of(vec![ultra, row("bob@example.com")]);
        let json = status_json(&snapshot, &[0.95, 0.95], StatusSource::Live);
        let alice = status_row(&json, "alice@example.com");
        assert_eq!(
            alice["quotaWindows"],
            serde_json::json!(["sevenDay"]),
            "a plan with no 5h window lists only the week: {alice}"
        );
        assert!(
            alice.get("provider").is_none(),
            "still an Anthropic row: {alice}"
        );
        let bob = status_row(&json, "bob@example.com");
        assert!(
            bob.get("quotaWindows").is_none(),
            "an unflagged Claude row carries no quotaWindows key: {bob}"
        );
    }

    /// `tcr accounts` prints `tcr status`'s account line, the Codex tokens included, from its
    /// offline snapshot. The Claude row is the line it always was: its workspace id prints no
    /// `workspace=`.
    #[test]
    fn accounts_text_carries_the_codex_tokens() {
        let alice = contract_account(
            "alice@example.com",
            0,
            "active",
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            ProbeStatus::Never,
            None,
            QuotaState::Normal,
            &[],
            None,
            (Some("claude_max"), Some("default_claude_max_20x"), None),
            (Some("11111111-1111-1111-1111-111111111111"), None),
        );
        let bob = AccountSnapshot {
            probe_status: ProbeStatus::Never,
            ..canonical_codex_row(
                "bob@example.com/codex",
                1,
                "pro",
                "55555555-5555-4555-8555-555555555555",
            )
        };
        assert_eq!(
            render_accounts(&snapshot_of(vec![alice, bob]), StatusSource::Offline),
            "account alice@example.com priority=0 plan=Max 20x 5h=n/a wk=n/a cache=n/a state=ok \
             status=active probe=never stream_errors=n/a\n\
             account bob@example.com/codex provider=codex priority=1 plan=ChatGPT Pro 5h=n/a \
             wk=n/a cache=n/a state=ok status=active probe=never stream_errors=n/a \
             workspace=55555555\n"
        );
    }

    /// A Claude row for the header and re-login tests: probed, in no group, its plan known,
    /// and gated `gate`.
    fn claude_row(name: &str, gate: crate::stats::GateReason) -> AccountSnapshot {
        AccountSnapshot {
            gate,
            ..contract_account(
                name,
                0,
                "active",
                false,
                None,
                None,
                None,
                None,
                None,
                None,
                ProbeStatus::Ok,
                None,
                QuotaState::Normal,
                &[],
                None,
                (Some("claude_max"), Some("default_claude_max_20x"), None),
                (None, None),
            )
        }
    }

    /// A Codex row for the header and re-login tests, gated `gate`.
    fn codex_row(name: &str, gate: crate::stats::GateReason) -> AccountSnapshot {
        AccountSnapshot {
            gate,
            ..canonical_codex_row(name, 0, "pro", "55555555-5555-4555-8555-555555555555")
        }
    }

    /// A mixed fleet's header counts each pool's eligible accounts: gate `ok` and not
    /// disabled, over every row of that provider. A fleet of one provider, or of none, keeps
    /// the header it always had.
    #[test]
    fn the_eligible_header_prints_only_in_mixed_fleets() {
        use crate::stats::GateReason;

        let alice = claude_row("alice@example.com", GateReason::Ok);
        let frank = claude_row("frank@example.com", GateReason::FiveHour);
        let bob = codex_row("bob@example.com/codex", GateReason::Ok);
        let dana = AccountSnapshot {
            disabled: true,
            ..codex_row("dana@example.com", GateReason::Ok)
        };
        let erin = codex_row("erin@example.com/codex", GateReason::Login);
        let all = vec![
            alice.clone(),
            frank.clone(),
            bob.clone(),
            dana.clone(),
            erin.clone(),
        ];

        assert_eq!(
            eligible_token(&snapshot_of(all.clone())),
            " eligible=claude:1/2,codex:1/3"
        );
        assert_eq!(eligible_token(&snapshot_of(vec![alice.clone(), frank])), "");
        assert_eq!(eligible_token(&snapshot_of(vec![bob, dana, erin])), "");
        assert_eq!(eligible_token(&snapshot_of(Vec::new())), "");

        // A Codex row held by an upstream 403 is out of rotation until its hold ends.
        let pat = codex_row("pat@example.com/codex", GateReason::Refused);
        assert_eq!(
            eligible_token(&snapshot_of(vec![alice, pat])),
            " eligible=claude:1/1,codex:0/1"
        );

        let build = |sha: &str| BuildInfo {
            sha: sha.to_string(),
            dirty: Some(false),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let group_colors = std::collections::BTreeMap::new();
        let pooled = tcr_status_wire::CodexRoute {
            pooled: true,
            code: None,
            reason: None,
            upstream: None,
        };
        let text = render_status_text(&StatusText {
            snapshot: &snapshot_of(all),
            source: StatusSource::Live,
            server_build: Some(&build("abc1234")),
            client_build: &build("def5678"),
            http1_only: false,
            group_colors: &group_colors,
            claude_verdict: None,
            codex_route: Some(&pooled),
            holds_codex: true,
        });
        let header = text
            .lines()
            .find(|line| line.starts_with("status source="))
            .unwrap_or_else(|| panic!("no header:\n{text}"));
        assert!(
            header.ends_with(" eligible=claude:1/2,codex:1/3"),
            "the count ends the header: {header}"
        );
    }

    /// Each Codex row in LOGIN gets one stderr line naming the command that signs it in again,
    /// and why its login died when that is known, in the words the dashboard uses. A Claude row
    /// never does, and neither does a Codex row whose gate is not LOGIN.
    #[test]
    fn status_prints_one_relogin_hint_per_signed_out_codex_row() {
        use crate::stats::GateReason;

        let mut erin = codex_row("erin@example.com/codex", GateReason::Login);
        erin.codex.login_error = Some("refresh_token_reused".to_string());
        erin.codex.codex_home = Some("/Users/example/.codex".to_string());
        let ivy = codex_row("ivy@example.com/codex", GateReason::Login);
        let frank = claude_row("frank@example.com", GateReason::Login);
        let mut bob = codex_row("bob@example.com/codex", GateReason::Ok);
        bob.codex.login_error = Some("refresh_token_reused".to_string());

        assert_eq!(
            relogin_hint_lines(
                &snapshot_of(vec![erin, ivy, frank, bob]),
                Some(Path::new("/Users/example"))
            ),
            [
                "[tcr] erin@example.com/codex needs re-login: the codex CLI in ~/.codex \
                 refreshed this shared login first — tcr login --codex --account \
                 erin@example.com/codex",
                "[tcr] ivy@example.com/codex needs re-login — tcr login --codex --account \
                 ivy@example.com/codex",
            ]
        );
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
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    /// **`tcr routing --json` exits 3 only when nothing listens**: a refused connection is
    /// the one way no proxy answered. Silence, and a client that could not be built, are 4,
    /// with every answer that is not a readable schema-1 document.
    #[test]
    fn routing_exits_3_only_when_nothing_listens() {
        assert_eq!(RoutingReadError::from(Unreached::Refused).exit_code(), 3);
        for unreached in [
            Unreached::Silent("the server did not answer within 5s".to_string()),
            Unreached::Unbuilt("http client: no TLS".to_string()),
        ] {
            assert_eq!(RoutingReadError::from(unreached).exit_code(), 4);
        }
        assert_eq!(
            RoutingReadError::Unreadable("HTTP 500".to_string()).exit_code(),
            4
        );
    }
    #[tokio::test]
    async fn reviewer_routing_rejects_an_http_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        let origin_task = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/unrelated-service\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let target_task = tokio::spawn(async move {
            if let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_secs(2), target.accept()).await
            {
                let mut request = [0u8; 4096];
                let n = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let body = r#"{"schema":1,"generatedAt":"2026-10-26T00:00:00Z","build":{"sha":"fake","dirty":false,"builtAt":"2026-10-26T00:00:00Z"},"providers":[],"forecast":null}"#;
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                Some(request)
            } else {
                None
            }
        });
        let mut config = Config::fixture(vec![]);
        config.proxy.port = origin_port;
        config.proxy.api_key = Some("fake-routing-key".to_string());
        let result = fetch_routing_body(&config).await;
        origin_task.await.unwrap();
        let forwarded = target_task.await.unwrap();
        assert!(
            result.is_err(),
            "HTTP 302 must be unreadable, redirected answer was accepted; request at second service: {forwarded:?}"
        );
        let error = result.unwrap_err();
        assert_eq!(error.exit_code(), 4);
        assert!(
            forwarded.is_none(),
            "a redirect target must receive no request or API key"
        );
    }
}
