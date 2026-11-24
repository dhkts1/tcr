//! `tcr status` and `tcr doctor`: the fleet read live-first with an offline fallback, its
//! `--check` verdict, the text rendering, and the build-skew line.

use std::path::Path;
use std::sync::Arc;

use time::OffsetDateTime;

use super::live_client::{LiveStatusError, fetch_live_status};
use super::render::{
    StatusSource, account_status_rows_at, render_accounts, resolve_thresholds, snapshot_offline,
    status_rows_json,
};
use super::{
    import_claude_code_login_if_empty, load_config, load_config_with_creation, warn_if_no_accounts,
};
use crate::build_info::{self, BuildInfo};
use crate::config::Config;
use crate::oauth::NoRefresh;
use crate::probe::{LiveUsageProber, ProbeStatus};
use crate::provider::Provider;
use crate::stats::{AccountSnapshot, StatsSnapshot};
use crate::status::StatusPayload;

/// What `tcr status --check` concluded about the fleet it just printed.
///
/// # The rule: which probe outcomes count as failed
///
/// A row fails the check when its account is NOT disabled and its last probe
/// ended in [`ProbeStatus::Error`], [`ProbeStatus::Timeout`] or
/// [`ProbeStatus::UpstreamDown`]: the probe ran and did not get a reading.
///
/// These do not fail it:
/// - [`ProbeStatus::Never`]: no probe has run (a fresh server, or an account
///   the probe loop does not cover), so there is no outcome to judge.
/// - [`ProbeStatus::RateLimited`]: the usage endpoint throttled the probe
///   itself. Its own doc-comment calls this benign: the account's serving quota
///   is untouched and its last bar is kept.
/// - a disabled account, whatever its probe says: the operator took it out of
///   rotation, so its health is not what a script polling the fleet asks about.
///
/// The match on [`ProbeStatus`] has no wildcard on purpose: a new variant does
/// not compile until someone decides which side of this rule it is on.
///
/// # The exit code
///
/// [`StatusCheck::ProbeFailed`] exits 4, not 1, following `tcr doctor`'s
/// convention ([`crate::doctor::Report::exit_code`]): 1 stays what every verb's
/// `anyhow` error exits with (an unreadable config, say), and each condition a
/// check reports gets a code of its own. 2 and 3 are taken by doctor's route
/// verdicts, which `tcr status` also prints, so the probe failure takes 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCheck {
    /// No enabled account's last probe failed.
    Clean,
    /// At least one enabled account's last probe failed.
    ProbeFailed,
}

impl StatusCheck {
    /// Apply the rule above to every row of `snapshot`.
    pub fn of(snapshot: &StatsSnapshot) -> Self {
        if snapshot.accounts.iter().any(probe_failed) {
            Self::ProbeFailed
        } else {
            Self::Clean
        }
    }

    /// The process exit code this verdict maps to.
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Clean => 0,
            Self::ProbeFailed => 4,
        }
    }
}

/// One row's half of [`StatusCheck`]'s rule.
const fn probe_failed(a: &AccountSnapshot) -> bool {
    if a.disabled {
        return false;
    }
    match a.probe_status {
        ProbeStatus::Error | ProbeStatus::Timeout | ProbeStatus::UpstreamDown => true,
        ProbeStatus::Never | ProbeStatus::Ok | ProbeStatus::RateLimited => false,
    }
}

/// `tcr status [--json] [--check]`. Returns the process exit code: always 0
/// without `check`, whatever the probes said, because TcrBar runs
/// `tcr status --json` and reads any non-zero exit as a failed poll
/// (`StatusPoller.classify`), which would blank the panel. With `check`, the
/// code is [`StatusCheck::exit_code`]. Either way stdout is the same bytes.
pub async fn status(config_path: &Path, json: bool, check: bool) -> anyhow::Result<i32> {
    // Read-only verb: plain load, no clobber-warning (we never save).
    let (config, created) = load_config_with_creation(config_path)?;
    let config = import_claude_code_login_if_empty(config_path, config).await;
    let first_empty_config = created && config.holds_no_accounts();
    warn_if_no_accounts(&config);

    // Read before the match: the offline arm CONSUMES `config`, and the route
    // check below needs the port whichever arm ran.
    let proxy_port = config.proxy.port;
    // How many requests this proxy served recently, or `None` when nothing
    // answered. The two are different facts, and `crate::doctor` keeps them
    // apart (a proxy that is up and idle is healthy; one that never answered is
    // not on the route at all).
    let mut served_recently: Option<u64> = None;

    // ONE read of the running proxy, and the Codex route and traffic are taken from it before
    // the snapshot consumes it. The text form words the route whatever the read gave (offline,
    // that no proxy answered); `--json` carries the route and the traffic only from a live read.
    // A newly created empty file configures no existing fleet. Its first status
    // read is offline; do not probe an unrelated process on the default port.
    let live_read = if first_empty_config {
        Err(LiveStatusError::NoServer)
    } else {
        fetch_live_status(&config).await
    };
    let holds_codex = status_holds_provider(&live_read, &config, Provider::Codex);
    let ca = crate::mitm::minted_ca_path();
    let ca_readable = ca.is_file();
    let codex_route = crate::codex_route::codex_route(
        live_read.as_ref(),
        proxy_port,
        &ca,
        ca_readable,
        live_read
            .as_ref()
            .map_or(0, crate::codex_route::listed_codex_accounts),
    );
    let (wire_route, wire_traffic) = codex_wire_facts(&live_read, proxy_port, &ca, ca_readable);

    let (source, server_build, snapshot, thresholds, http1_only, control, group_colors) =
        match live_read {
            Ok(payload) => {
                served_recently = Some(crate::doctor::requests_in_window(
                    &payload,
                    crate::doctor::REQUEST_WINDOW_MINUTES,
                ));
                let build = payload.build.clone();
                let http1_only = payload.http1_only;
                let control = payload.control.clone();
                let group_colors = payload.group_colors.clone();
                let (snapshot, thresholds) = payload.into_snapshot();
                (
                    StatusSource::Live,
                    Some(build),
                    snapshot,
                    thresholds,
                    http1_only,
                    control,
                    group_colors,
                )
            }
            Err(reason) => {
                // Both "it answered something unusable" and "it answered nothing"
                // warn; only the ordinary no-server case stays quiet.
                if !matches!(reason, LiveStatusError::NoServer) {
                    let why = reason.why();
                    eprintln!(
                        "[tcr] warning: could not read live status from the proxy on :{} ({why}) — falling back to an offline snapshot, whose serving counters are all zero.",
                        config.proxy.port
                    );
                }
                let thresholds = resolve_thresholds(&config);
                // `config` is consumed by `snapshot_offline` below, so read
                // `http1_only`/`control_account`/`group_colors` off it first —
                // this is the config FILE's value, not a running server's
                // (there is none in this branch), which is the honest answer
                // for an offline snapshot. `control` is a config fact either
                // way, so this is the same reading a live server would derive
                // at boot.
                let http1_only = config.http1_only;
                let control = config.control_account_name().map(str::to_string);
                let group_colors = config.group_colors();
                let prober = LiveUsageProber::new().with_codex_base(config.codex_upstream.clone());
                let snapshot =
                    snapshot_offline(config, Arc::new(NoRefresh), Arc::new(prober), true).await;
                (
                    StatusSource::Offline,
                    None,
                    snapshot,
                    thresholds,
                    http1_only,
                    control,
                    group_colors,
                )
            }
        };

    // Build skew goes to STDERR in both modes: `--json` output is a bare array
    // that scripts pipe into jq, and a diagnostic on stdout would corrupt it.
    // One channel for the warning also means one place to look for it.
    if let Some(line) = skew_report(server_build.as_ref()) {
        eprintln!("{line}");
    }

    if json {
        println!(
            "{}",
            status_rows_json(account_status_rows_at(
                OffsetDateTime::now_utc(),
                &snapshot,
                &thresholds,
                source,
                server_build.as_ref(),
                http1_only,
                control.as_deref(),
                &group_colors,
                wire_route.as_ref(),
                wire_traffic.as_ref(),
            ))
        );
    } else {
        // The route problem goes ABOVE the fleet, because it outranks it: a
        // glance at `tcr status` that shows healthy accounts while Claude Code
        // is pointed at another base URL is the exact reading that cost a
        // colleague an afternoon. Only when there IS a problem (exit 2, routed
        // elsewhere; exit 3, routed here with nothing answering), and only in
        // the text form. `--json` is a bare array a script pipes into jq, and
        // a line above it would corrupt that contract.
        let route = crate::doctor::inspect(proxy_port, served_recently);
        let claude_verdict = (route.exit_code() != 0).then(|| route.verdict());
        print!(
            "{}",
            render_status_text(&StatusText {
                snapshot: &snapshot,
                source,
                server_build: server_build.as_ref(),
                client_build: &BuildInfo::current(),
                http1_only,
                group_colors: &group_colors,
                claude_verdict,
                codex_route: Some(&codex_route),
                holds_codex,
            })
        );
        // A signed-out Codex row's remedy goes to stderr, below the account lines that name
        // it. It is not a probe verdict, so it never changes `--check`'s exit code.
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(std::path::PathBuf::from);
        for line in relogin_hint_lines(&snapshot, home.as_deref()) {
            eprintln!("{line}");
        }
        // What tcr did with codex lately goes to stderr, below the account lines it points at,
        // and only for a fleet that holds a Codex account.
        if holds_provider(&snapshot, Provider::Codex)
            && let Some(traffic) = &wire_traffic
        {
            for line in codex_traffic_notes(traffic) {
                eprintln!("{line}");
            }
        }
    }
    if !check {
        return Ok(0);
    }
    Ok(StatusCheck::of(&snapshot).exit_code())
}

pub(super) fn status_holds_provider(
    live: &Result<StatusPayload, LiveStatusError>,
    config: &Config,
    provider: Provider,
) -> bool {
    match live {
        Ok(payload) if payload.mitm.is_some() => {
            payload.accounts.iter().any(|row| row.provider == provider)
        }
        Ok(_) | Err(_) => config.holds_provider(provider),
    }
}

/// Does the fleet hold an account of `provider`, in any state?
pub(super) fn holds_provider(snapshot: &StatsSnapshot, provider: Provider) -> bool {
    snapshot.accounts.iter().any(|a| a.provider == provider)
}

/// The fleet's Codex route and traffic, as `tcr status --json` repeats them on every Codex row:
/// from a live read only. A read that was not live (nothing listening, no answer, an answer
/// this build cannot use) gives neither, so `--json` never claims a route it did not read, nor
/// counts a server it could not ask.
pub(super) fn codex_wire_facts(
    live: &Result<StatusPayload, LiveStatusError>,
    port: u16,
    ca: &Path,
    ca_readable: bool,
) -> (
    Option<tcr_status_wire::CodexRoute>,
    Option<tcr_status_wire::CodexTraffic>,
) {
    match live {
        Ok(payload) => (
            Some(crate::codex_route::codex_route(
                Ok(payload),
                port,
                ca,
                ca_readable,
                crate::codex_route::listed_codex_accounts(payload),
            )),
            payload.codex_traffic.clone(),
        ),
        Err(_) => (None, None),
    }
}

/// The stderr notes `tcr status` prints about what the proxy did with codex in the last ten
/// minutes, each only when its count is above zero: the pool refusals tcr answered codex with itself,
/// and the requests it relayed unpooled because they named a routing of their own.
pub(crate) fn codex_traffic_notes(traffic: &tcr_status_wire::CodexTraffic) -> Vec<String> {
    let mut notes = Vec::new();
    if traffic.refused_10m > 0 {
        notes.push(format!(
            "[tcr] note codex-refusals: refused_10m={} last_type={} — tcr refused codex \
             because no Codex account could serve; see the Codex rows above",
            traffic.refused_10m,
            traffic.last_refusal.map_or("unknown", |kind| kind.as_str()),
        ));
    }
    if traffic.unpooled_10m > 0 {
        notes.push(format!(
            "[tcr] note codex-unpooled: relayed_10m={} — requests from a FedRAMP or \
             residency-routed ChatGPT login ran on their own credential, unpooled",
            traffic.unpooled_10m,
        ));
    }
    notes
}

/// What `tcr status` prints on stdout in its text form, in order: a route verdict when there is
/// one to give, the `status source=` header, and one line per account. Pure, so the bytes a
/// Claude-only fleet sees can be pinned.
pub(crate) struct StatusText<'a> {
    pub snapshot: &'a StatsSnapshot,
    pub source: StatusSource,
    pub server_build: Option<&'a BuildInfo>,
    pub client_build: &'a BuildInfo,
    pub http1_only: bool,
    pub group_colors: &'a std::collections::BTreeMap<String, String>,
    /// `crate::doctor::Report::verdict()` when Claude's route has a problem (`exit_code() != 0`).
    pub claude_verdict: Option<String>,
    /// Whether the running proxy pools codex ([`crate::codex_route::codex_route`]), worded above
    /// the header when the fleet holds a Codex account and codex is not pooled.
    pub codex_route: Option<&'a tcr_status_wire::CodexRoute>,
    /// Provider presence from live rows, or configured rows when an older proxy
    /// cannot report providers yet.
    pub holds_codex: bool,
}

/// The bytes of `tcr status`'s text form, [`StatusText`]'s lines in order, each ending in a
/// newline.
pub(crate) fn render_status_text(text: &StatusText<'_>) -> String {
    let mut out = String::new();
    let holds_claude = holds_provider(text.snapshot, Provider::Anthropic);
    // Claude's route is this fleet's question only while it holds a Claude account, or none
    // at all (the empty fleet keeps the verdict it always had). A Codex-only fleet is not
    // told about a route none of its accounts use.
    if (holds_claude || text.snapshot.accounts.is_empty())
        && let Some(verdict) = &text.claude_verdict
    {
        out.push_str(verdict);
        out.push('\n');
    }
    // Codex's route second, so Claude's verdict keeps the first line. Keyed `verdict:` in a
    // Codex-only fleet, where it is the only verdict there is.
    let unpooled = text
        .codex_route
        .filter(|route| !route.pooled)
        .and_then(|route| route.reason.as_deref());
    if let (Some(reason), true) = (unpooled, text.holds_codex) {
        let key = if holds_claude {
            "codexVerdict"
        } else {
            "verdict"
        };
        out.push_str(&format!("{key}: codex is not pooled: {reason}\n"));
    }
    // One greppable `source=` line above the account lines, in the same
    // key=value idiom, so the provenance is visible without --json.
    // `http1Only` rides here rather than per-account: it is a server-wide
    // fact, like `source` itself, not a per-account gating detail — see
    // `Manager::http1_only`'s doc-comment for why this must NOT be
    // re-derived from the config file when `source=live`. `groupColors`
    // rides the same way, one `name:hex` token per group.
    let group_colors_field = if text.group_colors.is_empty() {
        String::new()
    } else {
        format!(
            " groupColors={}",
            text.group_colors
                .iter()
                .map(|(g, hex)| format!("{g}:{hex}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    out.push_str(&format!(
        "status source={}{}{} http1Only={}{}{}\n",
        text.source.as_str(),
        match text.source {
            StatusSource::Live => String::new(),
            StatusSource::Offline =>
                " note=serving-counters-unavailable-no-server-answered".to_string(),
        },
        build_fields(text.server_build, text.client_build),
        text.http1_only,
        group_colors_field,
        eligible_token(text.snapshot),
    ));
    out.push_str(&render_accounts(text.snapshot, text.source));
    out
}

/// ` eligible=claude:R/N,codex:R/N`, the header's count of each pool's eligible accounts, in a
/// fleet that holds both providers; empty otherwise, so a Claude-only or Codex-only header is
/// the one it always was. N is every row of that provider, and R the rows whose gate is `ok`
/// and that are not disabled: the dashboard's "eligible", which a row held by an upstream 403
/// or 429 is not.
pub(super) fn eligible_token(snapshot: &StatsSnapshot) -> String {
    let tally = |provider: Provider| {
        let rows: Vec<&AccountSnapshot> = snapshot
            .accounts
            .iter()
            .filter(|a| a.provider == provider)
            .collect();
        let eligible = rows
            .iter()
            .filter(|a| a.gate == crate::stats::GateReason::Ok && !a.disabled)
            .count();
        (eligible, rows.len())
    };
    let (claude, claude_rows) = tally(Provider::Anthropic);
    let (codex, codex_rows) = tally(Provider::Codex);
    if claude_rows == 0 || codex_rows == 0 {
        return String::new();
    }
    format!(" eligible=claude:{claude}/{claude_rows},codex:{codex}/{codex_rows}")
}

/// The stderr line `tcr status` prints for each Codex row whose login is dead (gate LOGIN), in
/// row order: the command that signs it in again, after why it died when that is known
/// ([`AccountSnapshot::relogin_why`], the one place the why is worded, so this line and the
/// dashboard's re-login hint cannot disagree). With no recorded why, the clause and its colon
/// are dropped and the line still prints. `home` shows a codex folder under it as `~`.
pub(super) fn relogin_hint_lines(snapshot: &StatsSnapshot, home: Option<&Path>) -> Vec<String> {
    snapshot
        .accounts
        .iter()
        .filter(|a| a.provider == Provider::Codex && a.gate == crate::stats::GateReason::Login)
        .map(|a| {
            let name = &a.name;
            let why = a.relogin_why(home).map_or_default(|why| format!(": {why}"));
            format!("[tcr] {name} needs re-login{why} — tcr login --codex --account {name}")
        })
        .collect()
}

/// `tcr doctor [--json] [--codex]`: is Claude Code reaching this proxy, and if
/// not, what decided otherwise; and, for a fleet that holds a Codex account or
/// with `--codex`, would codex be pooled from this shell. Returns the exit code
/// the caller hands to the process, with the meanings
/// [`crate::doctor::Report::exit_code`] gives them: 0 routed here and serving,
/// 2 routed somewhere else, 3 routed here with nothing answering. Whose code it
/// is follows the fleet ([`crate::doctor::Diagnosis`]): Claude's while the
/// fleet holds a Claude account, so a script asking about Claude Code gets the
/// answer it always got; Codex's for a fleet of Codex accounts alone, and with
/// `--codex`. A config key this build does not recognise is named and changes
/// no exit code.
///
/// The live read happens here rather than in `crate::doctor` so there stays
/// exactly ONE client speaking to the status endpoint, with one set of
/// timeouts and one api-key rule. Everything this prints is decided by
/// [`crate::doctor::inspect`] and [`crate::doctor::inspect_codex`], over that
/// one read. The fleet is the running proxy's when it answered, else the
/// config's, and only a fleet holding a Codex account, or `--codex`, pays for
/// the Codex half: a Claude-only fleet's doctor never searches for a codex
/// binary.
///
/// `LiveStatusError::NoServer` is the only failure that reads as "no proxy
/// answered" without a word on stderr: it is the ordinary state of a machine
/// whose proxy is down, and it is already the headline of the verdict line.
/// The other two mean something IS on the port and could not be read, which the
/// operator has to be told, since the request count then reads `none` for a
/// reason that is not "the proxy is down".
pub async fn doctor(config_path: &Path, json: bool, codex_only: bool) -> anyhow::Result<i32> {
    let config = load_config(config_path)?;
    let live = fetch_live_status(&config).await;
    let served_recently = match &live {
        Ok(payload) => Some(crate::doctor::requests_in_window(
            payload,
            crate::doctor::REQUEST_WINDOW_MINUTES,
        )),
        Err(LiveStatusError::NoServer) => None,
        Err(reason) => {
            eprintln!(
                "[tcr] warning: something is listening on 127.0.0.1:{} and this build could not read its status ({}). The request count below reads as none for that reason, not because the proxy is down.",
                config.proxy.port,
                reason.why()
            );
            None
        }
    };

    let claude =
        crate::doctor::inspect(config.proxy.port, served_recently).with_config_keys(&config);
    let holds = |provider| status_holds_provider(&live, &config, provider);
    let codex = (codex_only || holds(Provider::Codex))
        .then(|| crate::doctor::inspect_codex(config.proxy.port, live.as_ref(), &config));
    let diagnosis = crate::doctor::Diagnosis {
        claude,
        codex,
        holds_anthropic: holds(Provider::Anthropic),
        codex_only,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&diagnosis.to_json())?);
    } else {
        println!("{}", diagnosis.render_text());
    }
    Ok(diagnosis.exit_code())
}

/// The build `key=value` tail of the `status source=` line.
///
/// `client_sha` is always present, and is not redundant with `server_sha`: the
/// two are the same binary only until someone rebuilds without restarting, which
/// is the normal state of affairs here (`tcr update` rebuilds on purpose without
/// restarting). When they differ, the CLI you just ran is newer than the server
/// answering it — worth seeing before trusting either. `client` is this binary's
/// own build (`BuildInfo::current()`), a parameter so the line can be pinned.
pub(super) fn build_fields(server: Option<&BuildInfo>, client: &BuildInfo) -> String {
    match server {
        Some(server) => format!(
            " server_sha={} server_dirty={} server_built_at={} client_sha={}",
            server.sha,
            server.dirty_str(),
            server.built_at,
            client.sha,
        ),
        None => format!(" client_sha={}", client.sha),
    }
}

/// Compare the RUNNING server's build against the checkout we are standing in,
/// and return the line to print — `None` when there is nothing to say.
///
/// Two silences are deliberate. With no live server there is no running code to
/// be stale (the offline path's numbers come from this very process). Outside
/// tcr's own checkout there is no HEAD that means anything here — see
/// [`build_info::find_tcr_checkout`], which refuses to compare against an
/// unrelated repository's HEAD.
pub(super) fn skew_report(server: Option<&BuildInfo>) -> Option<String> {
    let server = server?;
    let cwd = std::env::current_dir().ok()?;
    let root = build_info::find_tcr_checkout(&cwd)?;
    let checkout = build_info::read_checkout_state(&root, &server.sha);
    build_info::compare(server, &checkout).report_line()
}
