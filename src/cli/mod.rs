//! Account-management subcommands for the `tcr` CLI.
//!
//! These are the non-server verbs — `accounts`, `remove`, `priority`,
//! `enable`, `disable`, `status` — that read or mutate the config
//! (`~/.config/tcr.json`) directly, so friends can curate the fleet without
//! hand-editing JSON.
//!
//! The seam copies [`crate::login::login`]: pure functions here `load` → mutate
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
/// [`crate::login::auto_import_claude_code_login`]; what lives HERE is the one
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
    if crate::login::auto_import_claude_code_login(config_path)
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
mod tests;
