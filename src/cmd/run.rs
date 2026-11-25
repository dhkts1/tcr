//! `tcr run`: start Claude Code or Codex through the proxy. This file finds the agent's
//! binary and checks what the launch needs; `launch_env` builds the environment it starts in.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tcr::base::config::Config;
use tcr::proxy::{self, GROUP_HEADER_NAME};
use tcr::verbs::cli::{self, validate_group_label_chars};
use tcr::verbs::doctor::{no_proxy_entry_exempts, points_at_tcr};
use tcr::{base::tls, proxy::live_client, status, verbs::codex_route};

use super::ConfigFile;
use super::server::load_config;
use crate::cmd::tool_env;

mod launch_env;
#[cfg(test)]
mod tests;

use launch_env::{
    LaunchKey, apply_base_url_env, apply_codex_see_through_env, apply_nested_claude_ca,
    apply_see_through_env, compose_group_header, mitm_codex_host,
};

#[derive(clap::Args)]
pub(crate) struct RunArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Serve this session from accounts labelled with this group. Strict by
    /// default: when no member can serve, the request waits for one or gets a
    /// 429, and is never sent to an account outside the group, even one the
    /// session is already pinned to. Set `groupSettings.<group>.spillToPool:
    /// true` in the config to let it fall back to the whole pool instead (a
    /// reserved group stays strict either way).
    /// Claude sessions only; a Codex session cannot ask for a group.
    #[arg(long)]
    group: Option<String>,
    /// Launch `codex` instead of `claude`, routed through the proxy's Codex accounts.
    #[arg(long)]
    codex: bool,
    /// Args passed verbatim to `claude`, or to `codex` with `--codex` (e.g. `tcr run -- -p "hi"`,
    /// or `tcr run --codex -- exec "hi"`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

/// `TCR_CLAUDE_BIN` overrides which `claude` binary we launch, taking
/// priority over the legacy `CLAUDE_BIN` (kept for compatibility with
/// whatever already sets it outside tcr); neither set falls back to `claude`
/// on `PATH`. An empty value in either counts as unset — `TCR_CLAUDE_BIN=`
/// left behind by a shell must not silently win over a real `CLAUDE_BIN`.
const TCR_CLAUDE_BIN_ENV: &str = "TCR_CLAUDE_BIN";
/// See [`TCR_CLAUDE_BIN_ENV`].
const CLAUDE_BIN_ENV: &str = "CLAUDE_BIN";

/// Resolves which harness binary to launch, through an ordered pair of env
/// keys: the pair documented at [`TCR_CLAUDE_BIN_ENV`] for `claude`. Named
/// generally — "harness", not "claude" — because it was written for any
/// harness; today it serves `claude` only. `codex` is found by
/// `codex_local::find_codex`, the one rule `tcr run --codex`, `tcr login
/// --codex` and the doctor share, which also searches past `PATH`.
fn resolve_harness_bin(specific_key: &str, legacy_key: &str, default: &str) -> String {
    resolve_harness_bin_from(
        std::env::var(specific_key).ok(),
        std::env::var(legacy_key).ok(),
        default,
    )
}

/// [`resolve_harness_bin`]'s pure half, split out so the precedence order and
/// the empty-string-counts-as-unset rule are unit-testable without mutating
/// process-global env vars (which is unsound to do from parallel tests).
fn resolve_harness_bin_from(
    specific: Option<String>,
    legacy: Option<String>,
    default: &str,
) -> String {
    for value in [specific, legacy].into_iter().flatten() {
        if !value.is_empty() {
            return value;
        }
    }
    default.to_string()
}

/// The `claude` binary this process should launch or probe, per
/// [`resolve_harness_bin`].
fn claude_bin() -> String {
    resolve_harness_bin(TCR_CLAUDE_BIN_ENV, CLAUDE_BIN_ENV, "claude")
}

/// A fresh [`std::process::Command`] for [`claude_bin`] — the one place that
/// spawns or would spawn the user's harness, so every call site resolves the
/// same way.
fn claude_command() -> std::process::Command {
    let name = claude_bin();
    std::process::Command::new(
        tool_env::executable(Path::new(&name)).unwrap_or_else(|| PathBuf::from(name)),
    )
}

/// A fresh [`std::process::Command`] for the `codex` binary `tcr run --codex` launches:
/// [`claude_command`]'s twin, resolved by `codex_local::find_codex`, the rule `tcr login
/// --codex` and the doctor use too. `TCR_CODEX_BIN`, then `CODEX_BIN`, always decide when set;
/// otherwise `PATH`, TcrBar's directories and npm's global prefix. A miss is the search's own
/// sentence, so nothing has been printed or read yet when it bails. Resolve before
/// adding the nested-agent launchers to PATH, so they cannot resolve to themselves.
fn codex_command() -> anyhow::Result<(std::process::Command, PathBuf)> {
    let codex = tcr::clients::codex_local::find_codex()
        .map_err(|miss| anyhow::anyhow!("{}", miss.sentence()))?;
    Ok((std::process::Command::new(&codex.path), codex.path))
}

/// `tcr run [--codex] [-- args…]` — `exec` Claude Code, or `codex` with
/// `--codex`, already pointed at this proxy.
///
/// If the proxy is not listening we launch `claude` untouched, so a stopped proxy never breaks the
/// shell alias.
///
/// `codex` is routed in see-through mode or not at all: tcr has no base-URL
/// route for `codex` yet, so a launch this function does not route through the
/// pool is announced on stderr with the reason, never a silent bypass. Which of
/// the two it gets is the RUNNING proxy's answer — see [`codex_launch`].
pub(crate) async fn run_harness(args: RunArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    let (config, _) = load_config(&config_path)?;
    let port = config.proxy.port;

    // Ahead of everything below that can spawn a process, `claude --version` included.
    refuse_group_with_codex(&args)?;
    if args.codex {
        let (mut cmd, codex) = codex_command()?;
        mark_run_active(&mut cmd);
        // ONE read of the running proxy answers both "is it listening" and "does
        // it intercept chatgpt.com", so the two cannot disagree. The CA is named,
        // never loaded: loading it here could re-mint a leaf the proxy never reads.
        let live = live_client::fetch_live_status(&config).await;
        let ca = tls::minted_ca_path();
        let launch = codex_launch(&args, port, &live, &ca, ca.is_file(), &|var| {
            std::env::var(var).ok()
        })?;
        // Pooled, but the pool may have no account free: say so before the banner, from the
        // same read, so a session that will be refused is not a surprise.
        if let (CodexLaunch::SeeThrough { .. }, Ok(payload)) = (&launch, &live)
            && let Some(line) = codex_pool_preflight(payload, time::OffsetDateTime::now_utc())
        {
            eprintln!("{line}");
        }
        // The same read decides the request key: sent only to a server that
        // reports `requestKeyMode`.
        let launch_key = LaunchKey::decide(config.usable_request_key(), &live);
        for line in apply_codex_launch(
            &mut cmd,
            port,
            &launch,
            &launch_key,
            &tool_env::Host::current(),
        ) {
            eprintln!("{line}");
        }
        if matches!(launch, CodexLaunch::SeeThrough { .. }) {
            for line in tool_env::apply(&mut cmd, Path::new(&claude_bin()), &config_path, false)? {
                eprintln!("{line}");
            }
        }
        // Generated shell-tool defaults precede explicit caller overrides.
        cmd.args(&args.args);
        return Err(exec_agent(&mut cmd))
            .with_context(|| format!("failed to launch `{}`", codex.display()));
    }

    // `--group`: validate the name and the claude version BEFORE spawning
    // anything, in this order — a. name, b. version — so a typo or too-old
    // `claude` never launches a session silently ungrouped.
    if let Some(group) = args.group.as_deref() {
        validate_group(&config, group)?;
        match check_claude_version() {
            ClaudeVersionCheck::TooOld(found) => {
                anyhow::bail!(
                    "claude {found} is older than {MIN_CLAUDE_VERSION_FOR_GROUP_STR}, the minimum \
                     that forwards ANTHROPIC_CUSTOM_HEADERS as a real request header — `--group` \
                     cannot work on this install. Upgrade claude and retry."
                );
            }
            // Phase 1 is PREFER-only, so degrading to ordinary (ungrouped) routing on an
            // unreadable/missing `claude` is safe — warn and continue rather than refuse.
            // Phase 2's `--only` must refuse here instead: there the operator believes
            // they are CONTAINED to the group, and silently routing everywhere is the
            // wrong kind of wrong for that contract.
            ClaudeVersionCheck::Unknown => {
                eprintln!(
                    "[tcr] --group {group}: could not determine the installed claude version \
                     (need >= {MIN_CLAUDE_VERSION_FOR_GROUP_STR} for ANTHROPIC_CUSTOM_HEADERS) — \
                     proceeding, but if it is too old this session will silently route to the \
                     whole pool instead of the group"
                );
            }
            ClaudeVersionCheck::Ok => {}
        }
    }

    let mut cmd = claude_command();
    cmd.args(&args.args);
    mark_run_active(&mut cmd);

    // c/d. Compose and set the group header — merging with, never clobbering,
    // whatever `ANTHROPIC_CUSTOM_HEADERS` this process already inherited (a
    // user's own headers, or an outer `tcr run`'s — see [`RUN_ACTIVE_ENV`]).
    // Set unconditionally of see-through vs base-URL mode below: both apply
    // env to the same `cmd`, and this line runs before either.
    if let Some(group) = args.group.as_deref() {
        let inherited = std::env::var("ANTHROPIC_CUSTOM_HEADERS").ok();
        let composed = compose_group_header(inherited.as_deref(), group);
        cmd.env("ANTHROPIC_CUSTOM_HEADERS", &composed);
        eprintln!("[tcr] --group {group}: routing this session via {GROUP_HEADER_NAME}");
    }

    if cli::proxy_is_up(port) {
        // ONE read of the running proxy's status per launch. It decides the request key (sent
        // only to a server that reports `requestKeyMode`), and, for a config that holds Codex
        // accounts and no Claude one, whether the proxy will refuse this whole session.
        let live = live_client::fetch_live_status(&config).await;
        if holds_only_codex(&config)
            && let Ok(payload) = &live
            && let Some(warning) = no_claude_account_warning(port, payload)
        {
            eprintln!("{warning}");
        }
        let launch_key = LaunchKey::decide(config.usable_request_key(), &live);
        if let Some(notice) = withheld_api_key_notice(
            config.loopback_api_key().is_some(),
            std::env::var_os("ANTHROPIC_API_KEY").is_some(),
        ) {
            eprintln!("{notice}");
        }
        // Two ways to route claude at ourselves, and they are NOT equivalent to
        // Claude Code — see `apply_see_through_env` for why we prefer the first.
        // Anything missing from the MITM material lands us in base-URL mode, which
        // always works; there is no half-applied third state.
        let mut lines = match see_through_ca() {
            Some(ca) => {
                let mut lines = apply_see_through_env(&mut cmd, port, &ca, &launch_key);
                lines.extend(tool_env::apply(
                    &mut cmd,
                    Path::new(&claude_bin()),
                    &config_path,
                    true,
                )?);
                lines
            }
            None => apply_base_url_env(&mut cmd, port, &launch_key),
        };
        lines.extend(launch_key.notice(port));
        for line in lines {
            eprintln!("{line}");
        }
    } else {
        eprintln!("[tcr] proxy not listening on :{port} — launching claude directly");
    }

    Err(exec_agent(&mut cmd)).context("failed to launch `claude` — is it on PATH?")
}

/// Replace this process with the agent, so the terminal's foreground program is
/// the agent and not `tcr`. Returns only when the launch fails.
fn exec_agent(cmd: &mut std::process::Command) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;
    cmd.exec()
}

/// `--group` is a Claude Code mechanism (`ANTHROPIC_CUSTOM_HEADERS`); Codex has no custom-header
/// env, so until the `[model_providers]` route lands this is refused rather than silently
/// routing the session to the whole Codex pool.
fn refuse_group_with_codex(args: &RunArgs) -> anyhow::Result<()> {
    if args.codex && args.group.is_some() {
        anyhow::bail!("--group is not supported with --codex yet; run without --group");
    }
    Ok(())
}

/// How `tcr run --codex` launches `codex`: [`codex_launch`]'s decision, which
/// [`apply_codex_launch`] carries out.
#[derive(Debug, PartialEq, Eq)]
enum CodexLaunch {
    /// Pooled: see-through via this tcr, trusting `ca` for the leaf it presents.
    SeeThrough {
        ca: PathBuf,
        /// The inherited `NO_PROXY` spellings that exempt `chatgpt.com`, and
        /// what codex gets instead ([`no_proxy_rewrites`]).
        no_proxy: Vec<NoProxyRewrite>,
    },
    /// Not pooled: `codex` goes direct, and the notice says why.
    Direct {
        /// The reason, as the notice words it.
        reason: String,
        /// Inherited proxy variables that point at this tcr, removed from
        /// codex's environment ([`proxy_vars_at`]).
        unset: Vec<&'static str>,
    },
}

impl CodexLaunch {
    /// The `[tcr]` lines this launch prints, besides the see-through banner
    /// [`apply_codex_see_through_env`] prints itself.
    fn notices(&self, port: u16) -> Vec<String> {
        match self {
            Self::SeeThrough { no_proxy, .. } => no_proxy
                .iter()
                .map(|rewrite| {
                    format!(
                        "[tcr] dropped {} from {} for codex: {} {} from the proxy, and the \
                         session would bypass tcr",
                        rewrite.dropped.join(", "),
                        rewrite.var,
                        if rewrite.dropped.len() == 1 {
                            "it exempts"
                        } else {
                            "they exempt"
                        },
                        mitm_codex_host(),
                    )
                })
                .collect(),
            Self::Direct { reason, unset } => {
                let mut lines = vec![format!(
                    "[tcr] launching codex directly (not pooled): {reason}"
                )];
                if !unset.is_empty() {
                    lines.push(format!(
                        "[tcr] removed {} from codex's environment: {} at tcr on :{port}, which \
                         this launch bypasses",
                        unset.join(", "),
                        if unset.len() == 1 {
                            "it points"
                        } else {
                            "they point"
                        },
                    ));
                }
                lines
            }
        }
    }
}

/// Decide how `tcr run --codex` launches `codex`. Read-only: caller CA bundles are
/// checked for tcr's certificate, and nothing here mints or writes a certificate.
///
/// # The running proxy decides
///
/// The proxy loads its MITM leaf once, at boot, and nothing reloads it. This
/// used to be decided by a load in THIS process instead (`tls::load_tls`),
/// which re-mints a stale leaf on disk and reports coverage the running proxy
/// does not have. So with an older proxy running, or a current one whose TLS
/// load failed at boot, it printed the see-through banner over a session
/// whose `chatgpt.com` traffic the proxy blind-tunnelled: silently un-pooled.
/// The inputs are now the running proxy's own account (`status`, one read of
/// its status endpoint), whether tcr's CA is on disk at `ca`, and the
/// inherited environment (`inherited`).
///
/// # Rows
///
/// * `--group` is refused before anything launches (`refuse_group_with_codex`).
/// * Pooled exactly when [`codex_route::codex_route`] says so: the proxy
///   reports a tcr-minted leaf that intercepts `chatgpt.com`, lists a Codex
///   account (in any state, as the proxy counts them), and its CA is on disk.
///   `tcr status` words the same decision, so the two never disagree.
/// * Direct otherwise, with the route's reason. Only a proxy that predates the
///   status key is told to restart: MITM off survives any restart.
///
/// Every direct launch also drops the inherited proxy variables that point at
/// this tcr ([`proxy_vars_at`]), so a tcr that is down, or not intercepting,
/// cannot capture the launch that goes around it. A dead tcr behind an
/// exported `HTTPS_PROXY` refuses the connection instead.
fn codex_launch(
    args: &RunArgs,
    port: u16,
    status: &Result<status::StatusPayload, live_client::LiveStatusError>,
    ca: &Path,
    ca_is_file: bool,
    inherited: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<CodexLaunch> {
    refuse_group_with_codex(args)?;
    let route = codex_route::codex_route(
        status.as_ref(),
        port,
        ca,
        ca_is_file,
        status
            .as_ref()
            .map_or(0, codex_route::listed_codex_accounts),
    );
    if route.pooled {
        let trusted_ca = match codex_route::codex_ca_for_launch(ca, inherited) {
            Ok(bundle) => bundle,
            Err(reason) => {
                return Ok(CodexLaunch::Direct {
                    reason,
                    unset: proxy_vars_at(port, inherited),
                });
            }
        };
        return Ok(CodexLaunch::SeeThrough {
            ca: trusted_ca,
            no_proxy: no_proxy_rewrites(mitm_codex_host(), inherited),
        });
    }
    Ok(CodexLaunch::Direct {
        reason: route.reason.unwrap_or_default(),
        unset: proxy_vars_at(port, inherited),
    })
}

/// The line `tcr run --codex` prints before its banner when the running proxy pools codex but
/// no Codex account in its pool can serve right now: why each is out, worded as tcr's own 429
/// lists its gates but in Codex words ([`tcr::records::stats::describe_codex_gates`]), and when the first one
/// frees ([`proxy::at_utc`]), or that none reports when. `None` when a Codex account is in
/// rotation, or when the pool holds none (the launch already said so).
///
/// A row held by an upstream hold or 403 that carries ChatGPT's own refusal
/// (`limitReachedType`) is counted as refused by ChatGPT: the refusal outlasts the hold, and is
/// why the account stays out.
fn codex_pool_preflight(
    payload: &status::StatusPayload,
    now: time::OffsetDateTime,
) -> Option<String> {
    use tcr::records::stats::GateReason;
    let codex: Vec<&status::AccountStatus> = payload
        .accounts_of(tcr::base::provider::Provider::Codex)
        .collect();
    if codex.is_empty() || codex.iter().any(|row| row.gate == GateReason::Ok) {
        return None;
    }
    let mut tally: std::collections::BTreeMap<GateReason, usize> =
        std::collections::BTreeMap::new();
    for row in &codex {
        let gate = match row.gate {
            GateReason::Hold | GateReason::Refused if row.limit_reached_type.is_some() => {
                GateReason::Rejected
            }
            gate => gate,
        };
        *tally.entry(gate).or_default() += 1;
    }
    let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
    let soonest = codex
        .iter()
        .filter_map(|row| row.free_at_ms)
        .filter(|&at_ms| at_ms > now_ms)
        .min()
        .and_then(|at_ms| {
            time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000).ok()
        });
    let tail = match soonest {
        Some(at) => format!("the soonest frees at {}", proxy::at_utc(at, now)),
        None => "none reports when it frees".to_string(),
    };
    Some(format!(
        "[tcr] no Codex account can serve right now{} — {tail}",
        tcr::records::stats::describe_codex_gates(&tally)
    ))
}

/// Does `config` hold a Codex account and no Claude account, in any state? Only then does
/// `tcr run` ask the running proxy whether it holds a Claude account.
fn holds_only_codex(config: &Config) -> bool {
    config.holds_provider(tcr::base::provider::Provider::Codex)
        && !config.holds_provider(tcr::base::provider::Provider::Anthropic)
}

/// The warning `tcr run` prints for a claude session when the running proxy on `port` holds
/// Codex accounts and no Claude account: every request this session sends will be refused.
/// `None` when the proxy lists any Claude account, or no account at all.
fn no_claude_account_warning(port: u16, payload: &status::StatusPayload) -> Option<String> {
    (payload.holds_provider(tcr::base::provider::Provider::Codex)
        && !payload.holds_provider(tcr::base::provider::Provider::Anthropic))
    .then(|| {
        format!(
            "[tcr] warning: the proxy on :{port} holds no Claude account, so it will refuse \
             every request from this session — add one with `tcr login`"
        )
    })
}

/// Carry out [`codex_launch`]'s decision on `cmd`, and return its stderr lines
/// for the caller to print. `key` applies to a see-through launch only: a direct
/// one never reaches tcr. Everything it reads from the machine (the caller's
/// environment, the Codex config file, tcr's CA path) comes from `host`.
#[must_use]
fn apply_codex_launch(
    cmd: &mut std::process::Command,
    port: u16,
    launch: &CodexLaunch,
    key: &LaunchKey,
    host: &tool_env::Host<'_>,
) -> Vec<String> {
    let mut lines = Vec::new();
    match launch {
        CodexLaunch::SeeThrough { ca, no_proxy } => {
            lines.extend(apply_codex_see_through_env(
                cmd,
                port,
                ca,
                (host.env)("CODEX_CA_CERTIFICATE").is_some_and(|value| !value.is_empty()),
                key,
            ));
            apply_nested_claude_ca(cmd, ca, (host.env)("NODE_EXTRA_CA_CERTS").is_some());
            lines.extend(tool_env::apply_codex_shell_policy(cmd, host));
            lines.extend(key.notice(port));
            for rewrite in no_proxy {
                cmd.env(rewrite.var, &rewrite.kept);
            }
        }
        CodexLaunch::Direct { unset, .. } => {
            for var in unset {
                cmd.env_remove(var);
            }
        }
    }
    lines.extend(launch.notices(port));
    lines
}

/// Every spelling of a proxy variable `codex`'s HTTP stack reads for an
/// `https://` request: `HTTPS_PROXY` first, then `ALL_PROXY`, either case. The
/// `HTTP_PROXY` pair is not consulted for `https://`, but a codex child process
/// may read it, and it is just as stale when it names this tcr.
const CODEX_PROXY_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// The inherited [`CODEX_PROXY_VARS`] that point at THIS tcr ([`points_at_tcr`]),
/// and so must not reach a `codex` launched around it.
///
/// Only those, never all six: a corporate egress proxy, or a loopback px or
/// cntlm on another port, may be how this machine reaches the internet at all.
/// Removing it would turn an un-pooled launch that works into one that fails.
fn proxy_vars_at(port: u16, inherited: &dyn Fn(&str) -> Option<String>) -> Vec<&'static str> {
    CODEX_PROXY_VARS
        .into_iter()
        .filter(|var| inherited(var).is_some_and(|value| points_at_tcr(&value, port)))
        .collect()
}

/// One inherited `NO_PROXY` spelling, rewritten for a see-through launch.
#[derive(Debug, PartialEq, Eq)]
struct NoProxyRewrite {
    var: &'static str,
    /// What codex gets: the inherited entries minus [`Self::dropped`], in
    /// their order. Empty when nothing else was listed; see
    /// [`no_proxy_rewrites`] for why the variable stays set then.
    kept: String,
    /// The entries that exempted the host, trimmed.
    dropped: Vec<String>,
}

/// Each inherited `NO_PROXY`/`no_proxy`, without the entries that exempt
/// `host`, for the ones that had any.
///
/// Such an entry (`chatgpt.com`, `.com`, `*`) sends codex around the proxy,
/// so the session is un-pooled while the see-through banner says otherwise.
/// The user asked for a pooled session, so those entries are dropped, and
/// said so, rather than the launch going direct; every other entry keeps
/// exempting what it did. A variable left empty stays SET, as `""`: codex
/// reads the first of the two spellings that is set, so unsetting one would
/// hand that choice to the other.
fn no_proxy_rewrites(
    host: &str,
    inherited: &dyn Fn(&str) -> Option<String>,
) -> Vec<NoProxyRewrite> {
    ["NO_PROXY", "no_proxy"]
        .into_iter()
        .filter_map(|var| {
            let value = inherited(var)?;
            let (dropped, kept): (Vec<&str>, Vec<&str>) = value
                .split(',')
                .partition(|entry| no_proxy_entry_exempts(entry, host));
            (!dropped.is_empty()).then(|| NoProxyRewrite {
                var,
                kept: kept.join(","),
                dropped: dropped
                    .iter()
                    .map(|entry| entry.trim().to_string())
                    .collect(),
            })
        })
        .collect()
}

/// The oldest Claude Code that forwards `ANTHROPIC_CUSTOM_HEADERS` as a real
/// outbound header on every `/v1/messages` request — verified against
/// Anthropic's gateway-protocol documentation. Older than this, `--group`
/// would set an env var claude silently never sends.
const MIN_CLAUDE_VERSION_FOR_GROUP: (u64, u64, u64) = (2, 1, 227);
const MIN_CLAUDE_VERSION_FOR_GROUP_STR: &str = "2.1.227";

// `validate_group_label_chars` moved to `tcr::verbs::cli` (imported below) so
// `tcr group add`'s surgical write can reuse the same Phase 1 validator instead
// of a second one drifting out of sync with this one.

/// `--group`'s validation half (step a): the requested name must be a label
/// SOME configured account actually carries, and every label involved —
/// the requested one AND every configured one, since either could end up in
/// `compose_group_header`'s output — must pass [`validate_group_label_chars`].
/// A typo must never resolve to an empty set — with Phase 1's prefer-only
/// semantics that would silently route every session across the whole pool
/// with no error at all, which is the quiet-wrong-answer this refusal exists
/// to prevent.
fn validate_group(config: &Config, group: &str) -> anyhow::Result<()> {
    if let Err(reason) = validate_group_label_chars(group) {
        anyhow::bail!("--group {group:?}: invalid group label — {reason}");
    }

    let mut configured: Vec<&str> = config.group_labels().collect();
    for label in &configured {
        if let Err(reason) = validate_group_label_chars(label) {
            anyhow::bail!("config carries an invalid group label {label:?}: {reason}");
        }
    }
    configured.sort_unstable();
    configured.dedup();

    if configured.contains(&group) {
        // The session this launches is a Claude session, and only a Claude
        // account can serve one: a group whose every member is a Codex account
        // would route it nowhere. Only a fleet holding a Codex account can have
        // such a group, since a label exists only while some account carries it.
        let has_claude = config
            .group_members(group)
            .any(|account| account.provider == tcr::base::provider::Provider::Anthropic);
        if !has_claude {
            anyhow::bail!("{}", tcr::verbs::provider_guard::group_has_no_claude(group));
        }
        return Ok(());
    }
    if configured.is_empty() {
        anyhow::bail!(
            "--group {group}: no account in the config carries a `groups` label — nothing to route to"
        );
    }
    anyhow::bail!(
        "--group {group}: not a configured group. Configured groups: {}",
        configured.join(", ")
    );
}

/// Three-state result of checking the installed `claude` against
/// [`MIN_CLAUDE_VERSION_FOR_GROUP`] — kept distinct from a plain bool even
/// though Phase 1 collapses `TooOld` and `Unknown` to different outcomes,
/// because Phase 2's `--only` must refuse on BOTH: there, degrading silently
/// on an unreadable version would tell the operator they were contained to
/// the group while the session actually ran across the whole fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeVersionCheck {
    Ok,
    /// The parsed version, for the error message.
    TooOld(String),
    /// `claude --version` failed to run, exited non-zero, or its output did
    /// not parse as `MAJOR.MINOR.PATCH …`.
    Unknown,
}

fn check_claude_version() -> ClaudeVersionCheck {
    let output = match claude_command().arg("--version").output() {
        Ok(o) if o.status.success() => o,
        _ => return ClaudeVersionCheck::Unknown,
    };
    classify_claude_version_output(&String::from_utf8_lossy(&output.stdout))
}

/// [`check_claude_version`]'s classification half, split out so it is testable
/// without spawning a `claude` process: feed it `claude --version`'s stdout
/// directly.
fn classify_claude_version_output(stdout: &str) -> ClaudeVersionCheck {
    match parse_claude_version(stdout) {
        Some(found) if found >= MIN_CLAUDE_VERSION_FOR_GROUP => ClaudeVersionCheck::Ok,
        Some((maj, min, patch)) => ClaudeVersionCheck::TooOld(format!("{maj}.{min}.{patch}")),
        None => ClaudeVersionCheck::Unknown,
    }
}

/// Parse the leading `MAJOR.MINOR.PATCH` off `claude --version`'s output
/// (observed shape: `"2.1.237 (Claude Code)"`, first token before whitespace).
/// `None` on anything else — garbage, a `v`-prefixed or two-component
/// version, empty output — which is exactly what routes [`check_claude_version`]
/// to `Unknown` rather than a wrong guess.
fn parse_claude_version(output: &str) -> Option<(u64, u64, u64)> {
    let first_token = output.split_whitespace().next()?;
    let mut parts = first_token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// The marker `tcr run` leaves on the agent it `exec`s: **a `tcr run` already
/// launched you, so do not start another one.**
///
/// `tcr run` resolves `claude` from `PATH`, and on a machine where something else
/// also wraps `claude` that lookup can land back on a launcher that wraps in
/// `tcr run` — which then resolves `claude` from `PATH` again. The chain still
/// terminates and the routing environment applied twice is identical, so nothing
/// breaks; what you see is every startup line printed twice. Measured 2026-08-17
/// inside a cmux pane: a hand-typed `tcr run` produced two see-through banners,
/// and dropping cmux's shim directory from `PATH` produced one.
///
/// A launcher cannot infer this from the routing variables — those are also what
/// a `tcr`-launched shell exports to everything it runs — so we state it, and a
/// wrapper that understands the marker hands off instead of wrapping again.
///
/// The name is deliberately **not** `CMUX_`-prefixed. cmux's own claude wrapper
/// clears every variable matching that prefix before exec'ing the real binary, so
/// a marker named for the wrapper it has to survive would be erased in transit by
/// exactly the process it exists to inform. [`marker_survives_the_cmux_prefix_sweep`]
/// pins that.
const RUN_ACTIVE_ENV: &str = "TCR_RUN_ACTIVE";

/// Set on **every** `tcr run` child, including the proxy-down passthrough: the
/// claim is about this chain already containing a `tcr run`, which is true there
/// too, and a launcher re-wrapping that case just prints the passthrough notice
/// twice instead of the routing banner.
fn mark_run_active(cmd: &mut std::process::Command) {
    cmd.env(RUN_ACTIVE_ENV, "1");
}

/// The CA to advertise for see-through mode, or `None` when we must fall back to
/// base-URL mode. Prints the reason on every `None` — a silent downgrade would
/// look exactly like a working see-through session while the capabilities it
/// exists to preserve stay off.
///
/// Read only material already on disk. The server owns certificate creation and
/// migration at startup; a client must never replace the CA while that server still
/// presents the leaf it loaded at boot. Missing or unusable material selects base-URL mode.
fn see_through_ca() -> Option<PathBuf> {
    let ca = tls::minted_ca_path();
    see_through_ca_in(ca.parent().unwrap_or_else(|| Path::new(".")))
}

fn see_through_ca_in(dir: &Path) -> Option<PathBuf> {
    match tls::existing_ca_path_in(dir) {
        Ok(ca_path) => match ca_path {
            Some(ca) if ca.is_file() => Some(ca),
            // A path we cannot read is not a CA we can hand to claude.
            Some(ca) => {
                eprintln!(
                    "[tcr] see-through off: CA {} is not a readable file",
                    ca.display()
                );
                None
            }
            None => {
                eprintln!("[tcr] see-through off: no CA on disk for the MITM leaf we present");
                None
            }
        },
        Err(err) => {
            eprintln!("[tcr] see-through off: MITM TLS material unavailable ({err})");
            None
        }
    }
}

/// Why `tcr run` does NOT hand the configured proxy key to `claude` as
/// `ANTHROPIC_API_KEY`, and says so.
///
/// It used to. Setting that variable makes Claude Code treat an API key as its auth
/// source AHEAD of the claude.ai login, and that **disables every claude.ai
/// connector** — announced once, in one startup line that scrolls away, after which
/// the tools are simply absent. It bought nothing in exchange: the `x-api-key` gate
/// exempts loopback clients (see `proxy::handle`), and the server binds 127.0.0.1
/// only, so a `tcr run` child is always exempt. Measured with the variable absent:
/// `/v1/messages` served and rotated across accounts, and every connector loaded.
///
/// A value the CALLER exported is inherited untouched — an explicit choice wins, and
/// it is the escape hatch for a `claude` with no claude.ai login of its own, which
/// does need some credential to start.
fn withheld_api_key_notice(configured: bool, caller_set: bool) -> Option<&'static str> {
    (configured && !caller_set).then_some(
        "[tcr] not exporting ANTHROPIC_API_KEY from proxy.apiKey: it would outrank claude's \
         claude.ai login and disable every claude.ai connector. The proxy does not need it — \
         its api-key gate exempts loopback clients. Export it yourself if this `claude` has no \
         claude.ai login of its own.",
    )
}
