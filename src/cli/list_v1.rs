//! `tcr list --json`: the schema-v1 account list CodexBar's claude-swap reader polls.

use std::path::Path;
use std::sync::Arc;

use super::load_config;
use super::render::{StatusSource, account_status_rows, snapshot_offline};
use crate::live_client::{LiveStatusError, fetch_live_status};
use crate::oauth::NoRefresh;
use crate::probe::{LiveUsageProber, UsageProber};
use crate::provider::Provider;

/// `tcr list --json`: every configured account in claude-swap's list schema v1,
/// for CodexBar's claude-swap reader (`tcr_status_wire::cswap_v1` has the
/// field-by-field contract).
///
/// Prints exactly one JSON document on stdout and returns the exit code: 0 with
/// the account list, 1 with the schema-v1 error envelope. The envelope goes to
/// stdout, not stderr, because CodexBar parses stdout whatever the exit code is.
pub async fn list_v1(config_path: &Path) -> i32 {
    let (document, code) = render_list_v1(config_path, Arc::new(LiveUsageProber::new())).await;
    println!("{document}");
    code
}

/// The document `tcr list --json` prints, and its exit code.
///
/// This NEVER probes. CodexBar runs it on every Claude refresh, and a read that
/// fell back to `status`'s offline path (which probes every account) would turn
/// each poll into one usage-endpoint call per account. So the live server is the
/// only source of a reading: with no server, or one that cannot be read, every
/// configured account is listed as `unavailable`. `prober` is a parameter only
/// so a test can prove this; the offline snapshot below is built with probing
/// off, and the prober it is handed must never be called.
///
/// A row's slot (`number`) is its position in the config FILE, derived here on
/// purpose rather than taken from the order the server returns rows in. The
/// server's list is its boot-time config plus accounts admitted since
/// (`Manager::reload_groups_if_changed` appends them), so once the file is
/// reordered the two orders differ. See [`overlay_live_rows`]. Only Claude rows
/// are listed ([`is_claude_row`]), so the position is among the file's Claude rows.
pub(super) async fn render_list_v1(
    config_path: &Path,
    prober: Arc<dyn UsageProber>,
) -> (String, i32) {
    use tcr_status_wire::cswap_v1::{self, ErrorKind, ErrorV1};

    // Read-only and polled, so no `import_claude_code_login_if_empty`: that
    // import is a write, and a menu-bar poll must not be what triggers it.
    let config = match load_config(config_path) {
        Ok(config) => config,
        Err(e) => {
            return (
                ErrorV1::line(ErrorKind::ConfigUnreadable, format!("{e:#}")),
                1,
            );
        }
    };
    let live = fetch_live_status(&config).await;
    let proxy_port = config.proxy.port;

    // Every account in the file, in file order, with nothing measured.
    let thresholds = config.switch_thresholds();
    let http1_only = config.http1_only;
    let control = config.control_account_name().map(str::to_string);
    let group_colors = config.group_colors();
    let snapshot = snapshot_offline(config, Arc::new(NoRefresh), prober, false).await;
    let file_rows = account_status_rows(
        &snapshot,
        &thresholds,
        StatusSource::Offline,
        None,
        http1_only,
        control.as_deref(),
        &group_colors,
    );

    let rows = match live {
        Ok(payload) => {
            let build = payload.build.clone();
            let http1_only = payload.http1_only;
            let control = payload.control.clone();
            let group_colors = payload.group_colors.clone();
            let (snapshot, thresholds) = payload.into_snapshot();
            let live_rows = account_status_rows(
                &snapshot,
                &thresholds,
                StatusSource::Live,
                Some(&build),
                http1_only,
                control.as_deref(),
                &group_colors,
            );
            overlay_live_rows(file_rows, live_rows)
        }
        // The ordinary state of a machine whose proxy is down: the rows say
        // `unavailable`, and that is the whole message.
        Err(LiveStatusError::NoServer) => file_rows,
        Err(reason) => {
            eprintln!(
                "[tcr] warning: could not read live status from the proxy on :{} ({}); listing every account as unavailable.",
                proxy_port,
                reason.why()
            );
            file_rows
        }
    };

    // claude-swap's list is CodexBar's list of CLAUDE accounts, and the schema has no
    // field to say a row is anything else, so a Codex account is left out: listed, it
    // would be drawn as a Claude account, under its email, with its Codex windows on
    // Claude bars. Dropped after the overlay, so a live row and a file row are judged
    // alike, and before `from_rows` numbers the slots.
    let rows: Vec<_> = rows.into_iter().filter(is_claude_row).collect();

    match serde_json::to_string_pretty(&cswap_v1::from_rows(&rows)) {
        Ok(document) => (document, 0),
        Err(e) => (ErrorV1::line(ErrorKind::RenderFailed, e.to_string()), 1),
    }
}

/// Whether a status row is a Claude account, the only kind claude-swap's list holds.
/// An absent `provider` is Anthropic, as on every wire this crate writes; a name this
/// build does not know is not an account it can vouch for as a Claude one. A `match`,
/// so a third provider decides here instead of being listed by default.
fn is_claude_row(row: &tcr_status_wire::AccountStatusRow) -> bool {
    match row.provider.as_deref() {
        None => true,
        Some(name) => match Provider::from_name(name) {
            Some(Provider::Anthropic) => true,
            Some(Provider::Codex) | None => false,
        },
    }
}

/// Put the live server's rows into the config file's order.
///
/// Each file row is replaced by the live row of the same name (names are unique
/// and are the whole address). A file row the server does not have (added to
/// the file since the server last read it) stays as the file's unmeasured row.
/// A live row the file no longer lists (removed since the server booted, and
/// still served until a restart) goes after the file's rows, in server order,
/// so every account the server is serving still appears, with a slot of its own.
pub(super) fn overlay_live_rows(
    file_rows: Vec<tcr_status_wire::AccountStatusRow>,
    mut live_rows: Vec<tcr_status_wire::AccountStatusRow>,
) -> Vec<tcr_status_wire::AccountStatusRow> {
    let mut rows: Vec<_> = file_rows
        .into_iter()
        .map(
            |file_row| match live_rows.iter().position(|l| l.name == file_row.name) {
                Some(at) => live_rows.remove(at),
                None => file_row,
            },
        )
        .collect();
    rows.extend(live_rows);
    rows
}
