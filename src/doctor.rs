//! `tcr doctor`: whether Claude Code is actually reaching this proxy, and
//! which file decides that.
//!
//! # The failure this exists to end
//!
//! A colleague installed tcr, added a shared account, and every answer kept
//! coming from their own keychain account: the added account stayed at zero
//! requests with the proxy up and healthy. Nothing in `tcr status` could say
//! why, because the fleet view reports the accounts this proxy holds and never
//! the question underneath it: is any client pointed at this proxy at all.
//!
//! `tcr run` exports `ANTHROPIC_BASE_URL` onto the child it launches
//! (`src/main.rs`, `apply_base_url_env`). Claude Code then applies its
//! settings files' `env` block on top of the process environment it inherited
//! (measured, and the reason `apply_capability_defaults` sets its variables at
//! EXEC time rather than leaving them to settings, see its doc-comment in
//! `src/main.rs`). So a base URL written in a settings file WINS over the one
//! the launcher exported, silently, and the proxy sits there serving nobody.
//!
//! # What it reports
//!
//! One greppable `key: value` line each: the base URL Claude Code will use,
//! where that value comes from, who holds the proxy port and whether that
//! process is a `tcr`, how many requests this proxy served in the last
//! [`REQUEST_WINDOW_MINUTES`] minutes, and a `verdict:` line that answers the
//! question in one sentence. Exit code 0 when Claude is routed here, 2 when it
//! is routed somewhere else, 3 when it is routed here and no proxy answers.
//!
//! # The Codex half
//!
//! A fleet that holds a Codex account, or `tcr doctor --codex`, gets a second
//! block ([`CodexReport`]) answering the other routing question: would a
//! `codex` started from this shell be pooled, and if not, why. It reports the
//! running proxy's interception of `chatgpt.com`, tcr's CA, the Codex upstream,
//! the `codex` binary `tcr run --codex` would launch, the shell's proxy and CA
//! variables, codex's own login, and what the proxy did with codex lately. Its
//! verdict reuses the one route decision ([`crate::codex_route::codex_route`])
//! that `tcr run --codex` acts on and `tcr status` prints, so the doctor can
//! never word it differently from them. [`Diagnosis`] decides which verdict a
//! fleet's shape ends on and whose exit code it takes: a fleet with a Claude
//! account keeps Claude's, and a Claude-only fleet prints what it always did.
//!
//! # Boundaries
//!
//! Everything in this module below [`inspect`] is pure: resolution takes the
//! environment value and two directories as arguments, so a test drives it
//! with a temp HOME and never reads the operator's own files. The I/O half
//! (loading the config, reading the live status endpoint, printing) lives in
//! `crate::cli::doctor`.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tcr_status_wire::{CodexRoute, CodexTraffic};

use crate::config_keys::UnknownKey;
use crate::stats::GateReason;
use crate::status::StatusPayload;

/// The base URL Claude Code talks to when nothing sets one.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The environment variable Claude Code reads its base URL from, and the one
/// `tcr run` exports onto its child.
pub const BASE_URL_VAR: &str = "ANTHROPIC_BASE_URL";

/// How many minutes of serving history the `requests10m:` line covers. Ten
/// minutes because the question it answers is "is anything reaching this proxy
/// right now", not "has it ever served anything": a cumulative counter stays
/// non-zero forever after a single request and would call a dead route healthy.
pub const REQUEST_WINDOW_MINUTES: usize = 10;

/// Where the base URL Claude Code will use comes from, in the order Claude Code
/// itself resolves them: the project's settings files first, then the user's,
/// then the process environment, then the built-in default.
///
/// The settings files outrank the process environment, which is the whole point
/// of this verb: `tcr run` sets the variable on its child and a settings file
/// overwrites it afterwards. See the module doc-comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BaseUrlSource {
    /// `<cwd>/.claude/settings.local.json`, the project's un-shared settings.
    ProjectLocalSettings { path: PathBuf },
    /// `<cwd>/.claude/settings.json`, the project's shared settings.
    ProjectSettings { path: PathBuf },
    /// `<home>/.claude/settings.json`, the user's settings.
    UserSettings { path: PathBuf },
    /// The process environment this `tcr` inherited.
    ProcessEnv,
    /// Nothing set one, so Anthropic's own endpoint.
    Default,
}

impl BaseUrlSource {
    /// The settings file this source names, when it is a file at all.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::ProjectLocalSettings { path }
            | Self::ProjectSettings { path }
            | Self::UserSettings { path } => Some(path),
            Self::ProcessEnv | Self::Default => None,
        }
    }

    /// How the source is named in the operator-facing lines: the file's path
    /// when it is a file, the variable's name when it is the environment. A
    /// path rather than a label like "user settings" because the operator's
    /// next action is to open the file, and the verdict line should hand them
    /// the argument for that.
    pub fn label(&self) -> String {
        match self {
            Self::ProjectLocalSettings { path }
            | Self::ProjectSettings { path }
            | Self::UserSettings { path } => path.display().to_string(),
            Self::ProcessEnv => BASE_URL_VAR.to_string(),
            Self::Default => "the default".to_string(),
        }
    }
}

/// What one settings file had to say about the base URL.
///
/// `Unreadable` is its own variant rather than being folded into "no value
/// here": a settings file this build cannot parse is the single most likely
/// place for the answer to be hiding, and swallowing that would make `doctor`
/// confidently name the WRONG source. It is reported and resolution continues,
/// which is the only honest degradation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsRead {
    /// No such file.
    Missing,
    /// The file exists and could not be read or parsed; the string says why.
    Unreadable(String),
    /// Parsed, with no `env.ANTHROPIC_BASE_URL` in it.
    NoBaseUrl,
    /// Parsed, and it sets this base URL.
    BaseUrl(String),
}

/// Read `env.ANTHROPIC_BASE_URL` out of one Claude Code settings file.
pub fn read_settings_base_url(path: &Path) -> SettingsRead {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SettingsRead::Missing,
        Err(error) => return SettingsRead::Unreadable(error.to_string()),
    };
    let document: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => return SettingsRead::Unreadable(format!("not readable as JSON: {error}")),
    };
    match document.get("env").and_then(|env| env.get(BASE_URL_VAR)) {
        None => SettingsRead::NoBaseUrl,
        Some(serde_json::Value::String(url)) => SettingsRead::BaseUrl(url.clone()),
        Some(other) => SettingsRead::Unreadable(format!(
            "env.{BASE_URL_VAR} is {other}, which is not a string"
        )),
    }
}

/// The base URL Claude Code will use, and which of the four sources set it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub url: String,
    pub source: BaseUrlSource,
}

/// [`resolve_base_url`]'s answer plus anything that got in the way of reaching
/// it. `problems` is never silently dropped: `doctor` prints each one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub route: Route,
    pub problems: Vec<String>,
}

/// Resolve the base URL the way Claude Code does, from an environment value and
/// the two directories whose `.claude` holds a settings file.
///
/// Takes `env_value`, `home` and `cwd` as parameters rather than reading the
/// process's own: every test here runs against a temp HOME, and a function that
/// read `$HOME` itself could not be tested without touching the operator's real
/// settings.
pub fn resolve_base_url(env_value: Option<&str>, home: &Path, cwd: &Path) -> Resolution {
    let mut problems = Vec::new();
    let candidates = [
        BaseUrlSource::ProjectLocalSettings {
            path: cwd.join(".claude").join("settings.local.json"),
        },
        BaseUrlSource::ProjectSettings {
            path: cwd.join(".claude").join("settings.json"),
        },
        BaseUrlSource::UserSettings {
            path: home.join(".claude").join("settings.json"),
        },
    ];
    for source in candidates {
        let Some(path) = source.path() else {
            continue;
        };
        match read_settings_base_url(path) {
            SettingsRead::BaseUrl(url) => {
                let source = source.clone();
                return Resolution {
                    route: Route { url, source },
                    problems,
                };
            }
            SettingsRead::Missing | SettingsRead::NoBaseUrl => {}
            SettingsRead::Unreadable(why) => {
                problems.push(format!("{}: {why}", path.display()));
            }
        }
    }
    let route = match env_value.filter(|value| !value.trim().is_empty()) {
        Some(url) => Route {
            url: url.to_string(),
            source: BaseUrlSource::ProcessEnv,
        },
        None => Route {
            url: DEFAULT_BASE_URL.to_string(),
            source: BaseUrlSource::Default,
        },
    };
    Resolution { route, problems }
}

/// The host and port of an `http[s]://…` URL, without a URL parser.
///
/// Hand-rolled deliberately: this crate has no `url` dependency and adding one
/// for two fields would be a new dependency for nothing. Scope is exactly what
/// the caller needs: scheme, optional userinfo, host (bracketed IPv6
/// included), optional port. Anything else returns `None` rather than a
/// half-parsed guess.
pub fn authority(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|part| !part.is_empty())?;
    // Userinfo, if any, is everything before the LAST `@`.
    let authority = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(digits) => digits.parse().ok()?,
            None => default_port,
        };
        return Some((host.to_string(), port));
    }
    match authority.split_once(':') {
        Some((host, digits)) => Some((host.to_string(), digits.parse().ok()?)),
        None => Some((authority.to_string(), default_port)),
    }
}

/// Is this URL one that reaches a proxy on THIS machine's `port`?
///
/// Loopback by address, not by spelling: `127.0.0.1`, `127.0.0.2`, `localhost`
/// and `[::1]` all reach the same listener, and an operator who wrote any of
/// them has a working route. A non-loopback host is another machine's proxy
/// (or Anthropic), whatever port it names.
pub fn routes_to_port(url: &str, port: u16) -> bool {
    let Some((host, url_port)) = authority(url) else {
        return false;
    };
    if url_port != port {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// The process holding the proxy port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortHolder {
    pub pid: u32,
    /// The process's own name, as the OS reports it.
    pub name: String,
    /// Whether that process is a `tcr` (or the JS `teamclaude` it replaced).
    pub is_tcr: bool,
}

/// Build a holder row from a pid and what the OS says about it. Split from
/// [`port_holder`] so the recognition rule is testable without a live process.
///
/// A holder counts as a `tcr` on its argv0 or its process name, not on
/// [`crate::singleton::classify_proxy_server`]: that function answers a
/// different question (is this safe to REPLACE on the port) and deliberately
/// returns `None` for `tcr run`, which is a tcr by any reading an operator
/// cares about here.
pub fn holder_from(pid: u32, name: &str, argv: &[String]) -> PortHolder {
    let js_bin = crate::legacy_names::JS_PROXY_BIN;
    let is_proxy_program = |token: &str| {
        token == "tcr"
            || token.ends_with("/tcr")
            || token == js_bin
            || token
                .strip_suffix(js_bin)
                .is_some_and(|dir| dir.ends_with('/'))
    };
    let is_tcr =
        argv.first().is_some_and(|argv0| is_proxy_program(argv0)) || is_proxy_program(name);
    PortHolder {
        pid,
        name: name.to_string(),
        is_tcr,
    }
}

/// Who is listening on `port`, on any address.
///
/// Same enumeration `src/singleton.rs` uses for the takeover decision (the
/// `listeners` crate, no subprocess, so no `lsof` to be missing or to conflate
/// "no holder" with "the tool failed"), and the same failure posture: an
/// enumeration error is warned about, never turned into a confident "nobody
/// holds the port".
pub fn port_holder(port: u16) -> Option<PortHolder> {
    let all = match listeners::get_all() {
        Ok(all) => all,
        Err(error) => {
            tracing::warn!(port, %error, "listeners::get_all failed; doctor cannot name the port holder");
            return None;
        }
    };
    let pid = all
        .into_iter()
        .filter(|listener| listener.protocol == listeners::Protocol::TCP)
        .filter(|listener| listener.state == listeners::SocketState::Listen)
        .find(|listener| listener.socket.port() == port)
        .map(|listener| listener.process.pid)?;

    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        false,
        sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
    );
    let process = system.process(sysinfo::Pid::from_u32(pid));
    let name = process.map_or_else(String::new, |p| p.name().to_string_lossy().into_owned());
    let argv = process.map_or_else(Vec::new, |p| {
        p.cmd()
            .iter()
            .map(|token| token.to_string_lossy().into_owned())
            .collect()
    });
    Some(holder_from(pid, &name, &argv))
}

/// Requests this proxy served in the last `minutes` wall-clock minutes, summed
/// over every live Claude session's per-minute sparkline
/// ([`tcr_status_wire::SessionRow::req_per_minute`], 30 entries, oldest first).
///
/// The sparkline rather than [`crate::status::AccountStatus::requests`]: that
/// counter is cumulative since the server booted, so it answers "has this proxy
/// ever served anything", and a route that broke an hour ago would still read
/// as healthy.
///
/// A Codex session (`provider` set) is left out, because its traffic says nothing
/// about Claude's route and counting it would read a broken route as healthy.
pub fn requests_in_window(payload: &StatusPayload, minutes: usize) -> u64 {
    payload
        .sessions
        .iter()
        .filter(|session| session.provider.is_none())
        .map(|session| session_requests_in_window(session, minutes))
        .sum()
}

/// One session's requests in the newest `minutes` of its sparkline
/// ([`tcr_status_wire::SessionRow::req_per_minute`]); a sparkline shorter than
/// the window counts what it has.
pub(crate) fn session_requests_in_window(
    session: &tcr_status_wire::SessionRow,
    minutes: usize,
) -> u64 {
    let minutes = session.req_per_minute.len().min(minutes);
    session
        .req_per_minute
        .iter()
        .rev()
        .take(minutes)
        .map(|&requests| u64::from(requests))
        .sum::<u64>()
}

/// Everything `tcr doctor` prints, decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub base_url: String,
    pub base_url_source: BaseUrlSource,
    /// The port this tcr's config says the proxy uses.
    pub proxy_port: u16,
    /// Whether [`Self::base_url`] reaches a proxy on this machine on that port.
    pub routed_here: bool,
    pub port_holder: Option<PortHolder>,
    /// Requests served in the last [`REQUEST_WINDOW_MINUTES`] minutes, or
    /// `None` when no proxy answered on the port, which is a different fact
    /// from zero and must never be flattened into one.
    pub requests_in_window: Option<u64>,
    /// Anything that got in the way of resolving the route (an unreadable
    /// settings file, most often).
    pub problems: Vec<String>,
    /// Top-level config keys this build does not recognise, each with its
    /// nearest known spelling. Reported and never rejected: the file keeps
    /// them as written (see [`crate::config_keys`]). They do not change
    /// [`Self::exit_code`], which is about routing alone.
    pub unknown_keys: Vec<UnknownKey>,
    /// The startup file holding `tcr setup shell`'s `claude` function for the
    /// login shell, when it is installed. That function runs `tcr run`, which
    /// sets the base URL on its own child, so it routes a typed `claude` here
    /// whenever no settings file names a base URL (a settings file outranks
    /// the process environment, see the module doc-comment).
    pub shell_route: Option<PathBuf>,
}

impl Report {
    /// The base URL alone does not reach this proxy, and a typed `claude`
    /// still does: the shell route is installed and nothing in a settings
    /// file overrides what `tcr run` exports.
    pub fn routed_by_shell(&self) -> bool {
        !self.routed_here
            && self.shell_route.is_some()
            && matches!(
                self.base_url_source,
                BaseUrlSource::ProcessEnv | BaseUrlSource::Default
            )
    }

    /// 0 routed here and serving, 2 routed somewhere else, 3 routed here with
    /// nothing answering. The shell route counts as routed here.
    ///
    /// "Somewhere else" outranks "nothing answered": when Claude is pointed at
    /// another URL, whether THIS proxy is up is not the operator's problem.
    pub fn exit_code(&self) -> i32 {
        if !self.routed_here && !self.routed_by_shell() {
            2
        } else if self.requests_in_window.is_none() {
            3
        } else {
            0
        }
    }

    /// The one-sentence answer, on its own `verdict:` line.
    pub fn verdict(&self) -> String {
        let source = self.base_url_source.label();
        if let (true, Some(file)) = (self.routed_by_shell(), &self.shell_route) {
            let served = match self.requests_in_window {
                Some(served) => format!("this proxy has served {served} requests"),
                None => format!("no proxy answered on 127.0.0.1:{}", self.proxy_port),
            };
            return self.shell_verdict(file, &served);
        }
        if !self.routed_here {
            return format!(
                "verdict: this proxy is not on Claude's route: {source} sets {}",
                self.base_url
            );
        }
        match self.requests_in_window {
            Some(served) => format!(
                "verdict: Claude is routed to {} ({source}); this proxy has served {served} requests",
                self.base_url
            ),
            None => format!(
                "verdict: Claude is routed to {} ({source}); no proxy answered on 127.0.0.1:{}",
                self.base_url, self.proxy_port
            ),
        }
    }

    /// The greppable `key: value` block, verdict last.
    pub fn render_text(&self) -> String {
        let mut lines = self.fact_lines();
        lines.push(self.verdict());
        lines.join("\n")
    }

    /// [`Self::render_text`]'s lines without the verdict: the seven keys, `baseUrl:` to
    /// `shellRoute:`, then a `problem:` line for each problem, then an `unknownKey:` line for each
    /// unknown config key. Beside a Codex block ([`Diagnosis`]) these lines come first and the
    /// verdict last, so a config's unknown keys sit after `shellRoute:` and before
    /// `codexAccounts:`.
    pub fn fact_lines(&self) -> Vec<String> {
        let holder = match &self.port_holder {
            Some(holder) => format!("pid {} {}", holder.pid, holder.name),
            None => "none".to_string(),
        };
        let holder_is_tcr = match &self.port_holder {
            Some(holder) if holder.is_tcr => "yes",
            Some(_) => "no",
            None => "unknown",
        };
        let requests = match self.requests_in_window {
            Some(served) => served.to_string(),
            None => "none (no proxy answered)".to_string(),
        };
        let mut lines = vec![
            format!("baseUrl: {}", self.base_url),
            format!("baseUrlSource: {}", self.base_url_source.label()),
            format!("proxyPort: {}", self.proxy_port),
            format!("portHolder: {holder}"),
            format!("portHolderIsTcr: {holder_is_tcr}"),
            format!("requests{REQUEST_WINDOW_MINUTES}m: {requests}"),
            format!(
                "shellRoute: {}",
                self.shell_route.as_ref().map_or_else(
                    || "not installed".to_string(),
                    |file| file.display().to_string()
                )
            ),
        ];
        for problem in &self.problems {
            lines.push(format!("problem: {problem}"));
        }
        lines.extend(self.unknown_key_lines());
        lines
    }

    /// One `unknownKey:` line per top-level config key this build does not recognise.
    pub fn unknown_key_lines(&self) -> Vec<String> {
        self.unknown_keys
            .iter()
            .map(|unknown| format!("unknownKey: {}", crate::config_keys::describe(unknown)))
            .collect()
    }

    /// [`Self::verdict`], as it reads beside a Codex block: a served arm, the base URL's or the
    /// shell route's, says the requests it counts are Claude's (`requests10m` counts Claude
    /// sessions only), since a reader who also sees `codexRequests10m` would otherwise take it
    /// for every request the proxy served. The arms with no count already name Claude's route
    /// and keep their words. [`Self::verdict`] itself is untouched: a Claude-only fleet's doctor
    /// and `tcr status` print it as they always did.
    pub fn verdict_beside_codex(&self) -> String {
        let claude_requests = |served: u64| {
            format!(
                "this proxy has served {served} Claude request{}",
                if served == 1 { "" } else { "s" }
            )
        };
        match (self.requests_in_window, &self.shell_route) {
            (Some(served), _) if self.routed_here => format!(
                "verdict: Claude is routed to {} ({}); {}",
                self.base_url,
                self.base_url_source.label(),
                claude_requests(served)
            ),
            (Some(served), Some(file)) if self.routed_by_shell() => {
                self.shell_verdict(file, &claude_requests(served))
            }
            _ => self.verdict(),
        }
    }

    /// The shell route's verdict, around `served`, the clause that says what this proxy served:
    /// [`Self::verdict`] counts its requests plainly, and [`Self::verdict_beside_codex`] as
    /// Claude's.
    fn shell_verdict(&self, file: &Path, served: &str) -> String {
        format!(
            "verdict: claude typed in a terminal goes through tcr run (the tcr block in {}); apps that start claude themselves use {} ({}); {served}",
            file.display(),
            self.base_url,
            self.base_url_source.label()
        )
    }

    /// The same report as one JSON object, with the two derived fields the text
    /// form carries (`verdict`, `exitCode`) written out rather than left for
    /// the reader to re-derive.
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(object) = value.as_object_mut() {
            object.insert("verdict".to_string(), self.verdict().into());
            object.insert("exitCode".to_string(), self.exit_code().into());
            object.insert(
                "requestWindowMinutes".to_string(),
                REQUEST_WINDOW_MINUTES.into(),
            );
        }
        value
    }

    /// Fill [`Self::unknown_keys`] from the loaded config's unmodelled map.
    /// Separate from [`inspect`] so `tcr status`, which calls `inspect` for
    /// its route line alone, is untouched.
    pub fn with_config_keys(mut self, config: &crate::config::Config) -> Self {
        self.unknown_keys =
            crate::config_keys::unknown_keys(&serde_json::Value::Object(config.extra.clone()));
        self
    }
}

/// Build the report for `port` from this process's own environment, HOME and
/// working directory, plus the request count the caller read off the live
/// status endpoint (`None` when nothing answered).
///
/// The live read stays with the caller: `crate::cli` already owns the one
/// client that speaks to the status endpoint, and a second one here would be a
/// second set of timeouts and api-key rules to keep in step.
pub fn inspect(port: u16, requests_in_window: Option<u64>) -> Report {
    let env_value = std::env::var(BASE_URL_VAR).ok();
    let home = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_default();
    let resolution = resolve_base_url(env_value.as_deref(), &home, &cwd);
    let routed_here = routes_to_port(&resolution.route.url, port);
    Report {
        base_url: resolution.route.url,
        base_url_source: resolution.route.source,
        proxy_port: port,
        routed_here,
        port_holder: port_holder(port),
        requests_in_window,
        problems: resolution.problems,
        unknown_keys: Vec::new(),
        shell_route: installed_shell_route(),
    }
}

/// One Codex account as the Codex block counts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRow {
    pub name: String,
    /// The gate the running proxy reports for the row. `None` when no proxy answered: the row
    /// then comes from the config, which holds no gate.
    pub gate: Option<GateReason>,
    /// The row's groups that are reserved, in the order the source lists them.
    pub reserved_groups: Vec<String>,
    /// The limit ChatGPT says the account reached, while that refusal stands.
    pub limit_reached_type: Option<String>,
}

/// What this shell would hand a `codex` started here: the variables that decide whether its
/// `https://chatgpt.com` traffic reaches tcr, and which CA it trusts when it does.
///
/// A variable is `(the spelling found, its value)`. An empty value reads as unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexEnv {
    /// The proxy codex uses for an `https://` URL: the first of `HTTPS_PROXY`, `https_proxy`,
    /// `ALL_PROXY` and `all_proxy` that is set. That is codex's own order: its HTTP clients keep
    /// reqwest's environment proxy (`openai/codex` f82e365,
    /// `http-client/src/outbound_proxy.rs:95-105`), which takes the `https` spelling first and
    /// falls back to `all`. So only this one variable decides whether codex meets tcr.
    pub https_proxy: Option<(String, String)>,
    /// `NO_PROXY`, else `no_proxy`.
    pub no_proxy: Option<(String, String)>,
    /// `SSL_CERT_FILE`, the CA codex trusts while `CODEX_CA_CERTIFICATE` is unset.
    pub ssl_cert_file: Option<String>,
    /// `CODEX_CA_CERTIFICATE`, the CA codex trusts first (`openai/codex` f82e365,
    /// `http-client/src/custom_ca.rs:385-409`: it wins over `SSL_CERT_FILE`, and an empty value
    /// counts as unset).
    pub codex_ca_certificate: Option<String>,
}

impl CodexEnv {
    /// Read the four from `get`, a variable's value by name ([`std::env::var`] in production,
    /// a table in a test).
    pub fn from_env(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let first = |names: &[&str]| {
            names.iter().find_map(|name| {
                get(name)
                    .filter(|value| !value.is_empty())
                    .map(|value| (name.to_string(), value))
            })
        };
        let set = |name: &str| get(name).filter(|value| !value.is_empty());
        CodexEnv {
            https_proxy: first(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]),
            no_proxy: first(&["NO_PROXY", "no_proxy"]),
            ssl_cert_file: set("SSL_CERT_FILE"),
            codex_ca_certificate: set("CODEX_CA_CERTIFICATE"),
        }
    }

    /// Whether `NO_PROXY` exempts `chatgpt.com`, so a codex started here goes around the proxy.
    fn exempts_codex(&self) -> bool {
        self.no_proxy.as_ref().is_some_and(|(_, value)| {
            value
                .split(',')
                .any(|entry| no_proxy_entry_exempts(entry, crate::provider::CODEX_HOST))
        })
    }

    /// Whether a codex started here trusts `ca`: the CA it reads first names it.
    fn trusts(&self, ca: &Path) -> bool {
        self.codex_ca_certificate
            .as_deref()
            .or(self.ssl_cert_file.as_deref())
            .is_some_and(|file| Path::new(file) == ca)
    }
}

/// `value`, a proxy URL, with any credential in it replaced by `…`: `http://…@127.0.0.1:3999`.
/// The doctor's output is meant to be pasted into a bug report.
fn mask_credentials(value: &str) -> String {
    let start = value.find("://").map_or(0, |at| at + 3);
    let end = value[start..]
        .find(['/', '?', '#'])
        .map_or(value.len(), |at| start + at);
    match value[start..end].rfind('@') {
        Some(at) => format!("{}…{}", &value[..start], &value[start + at..]),
        None => value.to_string(),
    }
}

/// Whether codex would be pooled from this shell, and if not, why: everything the Codex block
/// of `tcr doctor` prints, decided. Built by [`inspect_codex`]; every method is pure.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexReport {
    /// The port this config's proxy uses.
    pub port: u16,
    pub rows: Vec<CodexRow>,
    /// [`crate::codex_route::codex_route`]'s answer, the decision `tcr run --codex` acts on.
    pub route: CodexRoute,
    /// tcr's CA, and whether it is a readable file.
    pub ca: PathBuf,
    pub ca_readable: bool,
    /// Where a Codex account's requests go, and whether that is the default.
    pub upstream: String,
    pub upstream_is_default: bool,
    /// The `codex` that `tcr run --codex` would launch and which rule found it, or why none
    /// was found ([`crate::codex_local::find_codex`]).
    pub bin: Result<(PathBuf, String), crate::codex_local::CodexSearch>,
    pub env: CodexEnv,
    /// The login codex keeps, and whether it exists. Existence only: it is never read.
    pub login: PathBuf,
    pub login_exists: bool,
    /// Codex requests served in the last [`REQUEST_WINDOW_MINUTES`] minutes, or `None` when no
    /// proxy answered.
    pub requests_10m: Option<u64>,
    /// What the proxy did with codex lately, or `None` when it did not say (or did not answer).
    pub traffic: Option<CodexTraffic>,
}

impl CodexReport {
    /// `codexAccounts`: the rows counted by gate, in `GateReason` order and in the words tcr's
    /// own 429 uses for a Codex account ([`crate::proxy::describe_codex_gates`]). A row held by
    /// an upstream hold or 403 while it carries ChatGPT's own refusal counts as refused by
    /// ChatGPT, as TcrBar counts it: that refusal outlasts the hold. With no proxy answering
    /// the rows have no gate, and with no row it is `0`.
    pub fn accounts_line(&self) -> String {
        let total = self.rows.len();
        if total == 0 {
            return "0".to_string();
        }
        let mut tally: BTreeMap<GateReason, usize> = BTreeMap::new();
        for row in &self.rows {
            let Some(gate) = row.gate else {
                return format!("{total} (no proxy answered)");
            };
            let gate = match gate {
                GateReason::Hold | GateReason::Refused if row.limit_reached_type.is_some() => {
                    GateReason::Rejected
                }
                gate => gate,
            };
            *tally.entry(gate).or_default() += 1;
        }
        let phrases = crate::proxy::describe_codex_gates(&tally);
        format!(
            "{total} ({})",
            phrases.strip_prefix(": ").unwrap_or(&phrases)
        )
    }

    /// `codexIntercept`: whether the proxy can terminate marked `chatgpt.com` connections. `yes` when codex is
    /// pooled, and when only tcr's CA is missing (a marked codex connection that
    /// does not trust the CA fails TLS). `no` when the proxy serves a leaf that cannot cover
    /// `chatgpt.com`, runs without MITM, or blind-tunnels it for want of a Codex account.
    /// `unknown` when no proxy answered, its status could not be read, it predates the report,
    /// or it reports a leaf this build does not know.
    pub fn intercept(&self) -> &'static str {
        use crate::codex_route::{JS_LEAF, LEAF_MISSING_HOST, MITM_OFF, NO_CA, NO_CODEX_ACCOUNT};
        if self.route.pooled {
            return "yes";
        }
        match self.route.code.as_deref() {
            Some(NO_CA) => "yes",
            Some(JS_LEAF | MITM_OFF | NO_CODEX_ACCOUNT | LEAF_MISSING_HOST) => "no",
            _ => "unknown",
        }
    }

    /// A shell proxy URL that needs routing guidance: either it lacks Codex's marker,
    /// or it opts into interception without trusting tcr's CA.
    fn proxy_var_at_tcr(&self) -> Option<&str> {
        let (var, value) = self.env.https_proxy.as_ref()?;
        (points_at_tcr(value, self.port)
            && !self.env.exempts_codex()
            && (!crate::mitm::codex_proxy_marked(value) || !self.env.trusts(&self.ca))
            && self.intercept() != "no")
            .then_some(var.as_str())
    }

    /// One sentence per problem, in order: this shell's proxy variable; each Codex row in a
    /// reserved group, which no Codex request can reach (a Codex request carries no group);
    /// codex's own missing login, which it needs to start even when pooled; and, last, why codex
    /// is not pooled.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(var) = self.proxy_var_at_tcr() {
            let marked = self
                .env
                .https_proxy
                .as_ref()
                .is_some_and(|(_, value)| crate::mitm::codex_proxy_marked(value));
            problems.push(if marked {
                format!(
                    "{var} in this shell points at tcr with the Codex marker but without its CA, \
                     so codex can fail TLS: use `tcr run --codex` with a CA bundle that \
                     includes tcr's CA; keep the marker scoped to that launch"
                )
            } else {
                format!(
                    "{var} in this shell points at tcr with an unmarked URL, so chatgpt.com is \
                     tunneled untouched on codex's own login: use `tcr run --codex` to join \
                     the pool for that launch instead of marking the whole shell"
                )
            });
        }
        for row in self
            .rows
            .iter()
            .filter(|row| !row.reserved_groups.is_empty())
        {
            problems.push(format!(
                "{} is reserved for {}, which no Codex request can ask for, so it serves nothing",
                row.name,
                row.reserved_groups.join(", ")
            ));
        }
        if !self.login_exists {
            problems.push(
                "codex needs a login of its own to start, even when pooled: run `codex login` \
                 once"
                    .to_string(),
            );
        }
        if !self.route.pooled {
            problems.push(self.route.reason.clone().unwrap_or_default());
        }
        problems
    }

    /// The bare verdict sentence: pooled, with the Codex requests served lately, or not pooled
    /// and why, in the words `tcr run --codex` and `tcr status` use for the same decision.
    pub fn verdict(&self) -> String {
        if self.route.pooled {
            let requests = self.requests_10m.unwrap_or(0);
            let noun = if requests == 1 { "request" } else { "requests" };
            return format!(
                "codex is pooled: `tcr run --codex` routes chatgpt.com through this proxy, which \
                 served {requests} Codex {noun} in the last {REQUEST_WINDOW_MINUTES} min"
            );
        }
        format!(
            "codex is not pooled: {}",
            self.route.reason.as_deref().unwrap_or_default()
        )
    }

    /// The doctor's exit codes, with their meanings: 0 pooled; 3 when no proxy answered or its
    /// status could not be read (as Claude's route exits 3 with nothing answering); 2 when a
    /// proxy answered and codex would not be pooled, which the verdict explains.
    pub fn exit_code(&self) -> i32 {
        use crate::codex_route::{NO_PROXY, STATUS_UNREADABLE};
        if self.route.pooled {
            return 0;
        }
        match self.route.code.as_deref() {
            Some(NO_PROXY | STATUS_UNREADABLE) => 3,
            _ => 2,
        }
    }

    /// The block's `key: value` lines, problems last. The verdict line is the caller's: it is
    /// keyed by the fleet's shape ([`Diagnosis`]).
    pub fn block_lines(&self) -> Vec<String> {
        let answered = self.requests_10m.is_some();
        let count = |value: Option<u64>| match (value, answered) {
            (Some(n), _) => n.to_string(),
            (None, true) => "n/a (the proxy does not report it)".to_string(),
            (None, false) => "none (no proxy answered)".to_string(),
        };
        let bin = match &self.bin {
            Ok((path, source)) => format!("{} ({source})", path.display()),
            Err(crate::codex_local::CodexSearch::NotFound { searched }) => {
                format!("not found (searched {})", searched.join(", "))
            }
            Err(crate::codex_local::CodexSearch::BadVariable {
                var,
                value,
                bare: false,
            }) => format!("not found ({var} is {value}, which is not an executable file)"),
            Err(crate::codex_local::CodexSearch::BadVariable {
                var,
                value,
                bare: true,
            }) => format!("not found ({var} is {value}, and no executable {value} is on PATH)"),
        };
        let mut lines = vec![
            format!("codexAccounts: {}", self.accounts_line()),
            format!("codexIntercept: {}", self.intercept()),
            format!(
                "codexCa: {} ({})",
                self.ca.display(),
                if self.ca_readable {
                    "readable"
                } else {
                    "not readable"
                }
            ),
            format!(
                "codexUpstream: {} ({})",
                self.upstream,
                if self.upstream_is_default {
                    "default"
                } else {
                    "set by codexUpstream"
                }
            ),
            format!("codexBin: {bin}"),
            format!("codexEnv: {}", self.env_text()),
            format!(
                "codexLogin: {} {} (existence only; never read)",
                self.login.display(),
                if self.login_exists {
                    "exists"
                } else {
                    "does not exist"
                }
            ),
            format!(
                "codexRequests{REQUEST_WINDOW_MINUTES}m: {}",
                count(self.requests_10m)
            ),
            format!(
                "codexRefused{REQUEST_WINDOW_MINUTES}m: {}",
                count(self.traffic.as_ref().map(|t| t.refused_10m))
            ),
            format!(
                "codexUnpooled{REQUEST_WINDOW_MINUTES}m: {}",
                count(self.traffic.as_ref().map(|t| t.unpooled_10m))
            ),
        ];
        lines.extend(
            self.problems()
                .into_iter()
                .map(|problem| format!("codexProblem: {problem}")),
        );
        lines
    }

    /// `codexEnv`'s value: the proxy variable codex reads (masked, and marked when it points at
    /// tcr), `NO_PROXY` (marked when it exempts `chatgpt.com`) and `SSL_CERT_FILE`.
    fn env_text(&self) -> String {
        let proxy = match &self.env.https_proxy {
            None => "HTTPS_PROXY unset".to_string(),
            Some((var, value)) if points_at_tcr(value, self.port) => {
                format!("{var}={} (points at tcr)", mask_credentials(value))
            }
            Some((var, value)) => format!("{var}={}", mask_credentials(value)),
        };
        let no_proxy = match &self.env.no_proxy {
            None => "NO_PROXY unset".to_string(),
            Some((var, value)) if self.env.exempts_codex() => {
                format!("{var}={value} (exempts {})", crate::provider::CODEX_HOST)
            }
            Some((var, value)) => format!("{var}={value}"),
        };
        let ssl_cert_file = match &self.env.ssl_cert_file {
            None => "SSL_CERT_FILE unset".to_string(),
            Some(file) => format!("SSL_CERT_FILE={file}"),
        };
        format!("{proxy} · {no_proxy} · {ssl_cert_file}")
    }

    /// The same report as `tcr doctor --json`'s `codex` object. `accounts.byGate` counts the
    /// rows by the gate the wire reports, and is `{}` when no proxy answered; a count that is
    /// not known is `null`; `route` is the route as `tcr status --json` carries it; `verdict`
    /// is the bare sentence.
    pub fn to_json(&self) -> serde_json::Value {
        let mut by_gate = serde_json::Map::new();
        for gate in self.rows.iter().filter_map(|row| row.gate) {
            let token = serde_json::to_value(gate)
                .ok()
                .and_then(|token| token.as_str().map(str::to_string))
                .unwrap_or_default();
            let count = by_gate
                .entry(token)
                .or_insert_with(|| serde_json::Value::from(0u64));
            *count = serde_json::Value::from(count.as_u64().unwrap_or(0) + 1);
        }
        let bin = match &self.bin {
            Ok((path, source)) => serde_json::json!({
                "path": path.display().to_string(),
                "source": source,
            }),
            Err(_) => serde_json::json!({}),
        };
        let mut env = serde_json::json!({
            "pointsAtTcr": self
                .env
                .https_proxy
                .as_ref()
                .is_some_and(|(_, value)| points_at_tcr(value, self.port)),
            "noProxyKeepsChatgpt": self.env.exempts_codex(),
        });
        if let Some((_, value)) = &self.env.https_proxy {
            env["httpsProxy"] = mask_credentials(value).into();
        }
        if let Some(file) = &self.env.ssl_cert_file {
            env["sslCertFile"] = file.as_str().into();
        }
        serde_json::json!({
            "accounts": {"total": self.rows.len(), "byGate": by_gate},
            "intercept": self.intercept(),
            "route": serde_json::to_value(&self.route).unwrap_or(serde_json::Value::Null),
            "ca": {"path": self.ca.display().to_string(), "readable": self.ca_readable},
            "upstream": {"url": self.upstream, "isDefault": self.upstream_is_default},
            "bin": bin,
            "env": env,
            "login": {"path": self.login.display().to_string(), "exists": self.login_exists},
            "requests10m": self.requests_10m,
            "refused10m": self.traffic.as_ref().map(|t| t.refused_10m),
            "unpooled10m": self.traffic.as_ref().map(|t| t.unpooled_10m),
            "problems": self.problems(),
            "verdict": self.verdict(),
            "exitCode": self.exit_code(),
        })
    }
}

/// Codex requests this proxy served in the last `minutes` minutes: [`requests_in_window`]'s
/// sum, over the Codex sessions it leaves out.
pub fn codex_requests_in_window(payload: &StatusPayload, minutes: usize) -> u64 {
    payload
        .sessions
        .iter()
        .filter(|session| session.provider.as_deref() == Some("codex"))
        .map(|session| session_requests_in_window(session, minutes))
        .sum()
}

/// Everything `tcr doctor` prints, in the fleet's shape: Claude's report, and the Codex block
/// when the fleet holds a Codex account or `--codex` asked for it.
///
/// | shape | text | exit code |
/// |---|---|---|
/// | no Codex block | Claude's, byte for byte | Claude's |
/// | mixed (`holds_anthropic`) | Claude's facts, the block, `codexVerdict:`, Claude's verdict | Claude's |
/// | Codex only | Claude's facts, the block, `verdict:` with Codex's verdict | Codex's |
/// | `--codex` (`codex_only`) | the block, the unknown keys, `verdict:` with Codex's verdict | Codex's |
///
/// A mixed fleet keeps Claude's exit code, so a script that asks `tcr doctor` whether Claude
/// Code reaches this proxy gets the answer it always got. Claude's verdict stays the last line
/// whenever the fleet holds a Claude account.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnosis {
    pub claude: Report,
    pub codex: Option<CodexReport>,
    /// Whether the fleet holds a Claude account, in any state.
    pub holds_anthropic: bool,
    /// `--codex`: only the Codex block and its verdict.
    pub codex_only: bool,
}

impl Diagnosis {
    /// The text form, by the table on [`Diagnosis`]. A config's unknown keys ride the Claude
    /// half, after `shellRoute:` and its problems; under `--codex` they come right before the
    /// verdict, since a misspelled `codexUpstream` matters most to a reader asking about codex.
    pub fn render_text(&self) -> String {
        let Some(codex) = &self.codex else {
            return self.claude.render_text();
        };
        let mut lines = Vec::new();
        if self.codex_only {
            lines.extend(codex.block_lines());
            lines.extend(self.claude.unknown_key_lines());
            lines.push(format!("verdict: {}", codex.verdict()));
        } else {
            lines.extend(self.claude.fact_lines());
            lines.extend(codex.block_lines());
            if self.holds_anthropic {
                lines.push(format!("codexVerdict: {}", codex.verdict()));
                lines.push(self.claude.verdict_beside_codex());
            } else {
                lines.push(format!("verdict: {}", codex.verdict()));
            }
        }
        lines.join("\n")
    }

    /// The exit code, by the table on [`Diagnosis`].
    pub fn exit_code(&self) -> i32 {
        match &self.codex {
            Some(codex) if self.codex_only || !self.holds_anthropic => codex.exit_code(),
            _ => self.claude.exit_code(),
        }
    }

    /// The JSON form. With no Codex block, Claude's object, unchanged. Beside one, Claude's
    /// object gains `codex` ([`CodexReport::to_json`]); a mixed fleet keeps Claude's `exitCode`
    /// and its verdict in Claude's words beside Codex, and a Codex-only fleet takes Codex's
    /// verdict, keyed as its text line is, and Codex's exit code. `--codex` prints the `codex`
    /// object alone, plus the config's `unknownKeys`.
    pub fn to_json(&self) -> serde_json::Value {
        let Some(codex) = &self.codex else {
            return self.claude.to_json();
        };
        if self.codex_only {
            let mut value = codex.to_json();
            value["unknownKeys"] =
                serde_json::to_value(&self.claude.unknown_keys).unwrap_or_default();
            return value;
        }
        let mut value = self.claude.to_json();
        value["codex"] = codex.to_json();
        if self.holds_anthropic {
            value["verdict"] = self.claude.verdict_beside_codex().into();
        } else {
            value["verdict"] = format!("verdict: {}", codex.verdict()).into();
            value["exitCode"] = codex.exit_code().into();
        }
        value
    }
}

fn codex_rows(
    live: Result<&StatusPayload, &crate::cli::LiveStatusError>,
    config: &crate::config::Config,
) -> Vec<CodexRow> {
    match live {
        Ok(payload) if payload.mitm.is_some() => payload
            .accounts
            .iter()
            .filter(|row| row.provider == crate::provider::Provider::Codex)
            .map(|row| CodexRow {
                name: row.name.clone(),
                gate: Some(row.gate),
                reserved_groups: row.reserved_groups.clone(),
                limit_reached_type: row.limit_reached_type.clone(),
            })
            .collect(),
        Ok(_) | Err(_) => {
            let reserved = config.reserved_group_names();
            config
                .accounts
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
                    CodexRow {
                        name: account.name.clone(),
                        gate: None,
                        reserved_groups,
                        limit_reached_type: None,
                    }
                })
                .collect()
        }
    }
}

/// The Codex block's I/O half, beside [`inspect`]: build the report for the proxy on `port`
/// from the one status read the caller made (`live`), this process's environment, and the
/// files codex and tcr keep.
///
/// The rows are the running proxy's Codex rows when it answered, which leave out an account
/// deleted while it runs, else the config's, which carry no gate. The route is
/// [`crate::codex_route::codex_route`] over the same read, the decision `tcr run --codex` and
/// `tcr status` make, so the doctor can never word it differently. The CA is named and checked
/// for readability, never loaded. The binary is [`crate::codex_local::find_codex`]'s, the one
/// `tcr run --codex` would launch. codex's login is checked for existence, never read.
pub fn inspect_codex(
    port: u16,
    live: Result<&StatusPayload, &crate::cli::LiveStatusError>,
    config: &crate::config::Config,
) -> CodexReport {
    let rows = codex_rows(live, config);
    let ca = crate::mitm::minted_ca_path();
    let ca_readable = ca.is_file();
    let route = crate::codex_route::codex_route(live, port, &ca, ca_readable, rows.len());
    let default_upstream = crate::config::default_codex_upstream();
    let (upstream, upstream_is_default) = match live {
        Ok(payload) => match &payload.codex_upstream {
            Some(url) => (url.clone(), false),
            None => (default_upstream, true),
        },
        Err(_) => (
            config.codex_upstream.clone(),
            config.codex_upstream == default_upstream,
        ),
    };
    let bin = crate::codex_local::find_codex()
        .map(|found| (found.path, found.source.label().to_string()));
    let login = crate::codex_local::codex_login_path();
    let login_exists = crate::codex_local::login_file_exists(&login);
    let (requests_10m, traffic) = match live {
        Ok(payload) => (
            Some(codex_requests_in_window(payload, REQUEST_WINDOW_MINUTES)),
            payload.codex_traffic.clone(),
        ),
        Err(_) => (None, None),
    };
    CodexReport {
        port,
        rows,
        route,
        ca,
        ca_readable,
        upstream,
        upstream_is_default,
        bin,
        env: CodexEnv::from_env(&|var| std::env::var(var).ok()),
        login,
        login_exists,
        requests_10m,
        traffic,
    }
}

/// Does a proxy variable's value name tcr on `port`: host `127.0.0.1`,
/// `localhost` or `[::1]`, and that port (the scheme's default when none is
/// written)? The scheme is optional, as it is to codex's HTTP stack, which
/// reads a bare `host:port` as `http://host:port`. A value that does not
/// parse names nothing.
pub fn points_at_tcr(value: &str, port: u16) -> bool {
    let value = value.trim();
    let parsed = if value.contains("://") {
        reqwest::Url::parse(value)
    } else {
        reqwest::Url::parse(&format!("http://{value}"))
    };
    let Ok(url) = parsed else {
        return false;
    };
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]"
    });
    loopback && url.port_or_known_default() == Some(port)
}

/// Does one `NO_PROXY` entry exempt the domain `host` from the proxy?
///
/// The rules of the matcher codex's HTTP stack applies (`hyper-util` 0.1.20,
/// `src/client/proxy/matcher.rs:445-535`, the version `openai/codex` pins),
/// for a domain host: an entry exempts it when, trimmed and with one leading
/// `.` removed, it equals `host` or a parent domain of it at a label boundary,
/// ignoring ASCII case; or when it is `*`. An IP or CIDR entry never exempts a
/// domain.
pub fn no_proxy_entry_exempts(entry: &str, host: &str) -> bool {
    let entry = entry.trim();
    if entry.is_empty() || entry.contains('/') || entry.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    if entry == "*" {
        return true;
    }
    let domain = entry.strip_prefix('.').unwrap_or(entry);
    if domain.eq_ignore_ascii_case(host) {
        return true;
    }
    let Some(dot) = host.len().checked_sub(domain.len() + 1) else {
        return false;
    };
    host.as_bytes()[dot] == b'.'
        && host
            .get(dot + 1..)
            .is_some_and(|parent| parent.eq_ignore_ascii_case(domain))
}

/// The file carrying `tcr setup shell`'s block for the login shell, when the
/// route is installed for it. A read failure here is not doctor's question to
/// answer (`tcr setup status` reports it), so it reads as not installed.
fn installed_shell_route() -> Option<PathBuf> {
    let homes = crate::setup::Homes::from_env().ok()?;
    let shell = crate::setup::chosen_shell(None)?;
    let status = crate::setup::shell_status(Some(shell), &homes).ok()?;
    if !status.installed {
        return None;
    }
    crate::setup::shell_targets(shell, &homes)
        .into_iter()
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the settings directory");
        }
        std::fs::write(path, body).expect("write the settings file");
    }

    fn settings_with(url: &str) -> String {
        format!("{{\"env\": {{\"{BASE_URL_VAR}\": \"{url}\"}}}}")
    }

    /// The defect this verb exists for: the launcher exported the variable and
    /// a settings file overrides it afterwards, so the FILE is the answer.
    #[test]
    fn a_settings_file_outranks_the_process_environment() {
        let home = tempfile::tempdir().expect("temp home");
        let cwd = tempfile::tempdir().expect("temp cwd");
        let settings = home.path().join(".claude").join("settings.json");
        write(&settings, &settings_with("https://gateway.example.com"));

        let resolved = resolve_base_url(Some("http://127.0.0.1:3456"), home.path(), cwd.path());

        assert_eq!(resolved.route.url, "https://gateway.example.com");
        assert_eq!(
            resolved.route.source,
            BaseUrlSource::UserSettings { path: settings }
        );
    }

    #[test]
    fn the_project_settings_outrank_the_user_settings_and_local_outranks_both() {
        let home = tempfile::tempdir().expect("temp home");
        let cwd = tempfile::tempdir().expect("temp cwd");
        write(
            &home.path().join(".claude").join("settings.json"),
            &settings_with("https://user.example.com"),
        );
        write(
            &cwd.path().join(".claude").join("settings.json"),
            &settings_with("https://project.example.com"),
        );

        let project = resolve_base_url(None, home.path(), cwd.path());
        assert_eq!(project.route.url, "https://project.example.com");

        write(
            &cwd.path().join(".claude").join("settings.local.json"),
            &settings_with("https://local.example.com"),
        );
        let local = resolve_base_url(None, home.path(), cwd.path());
        assert_eq!(local.route.url, "https://local.example.com");
        assert_eq!(
            local.route.source,
            BaseUrlSource::ProjectLocalSettings {
                path: cwd.path().join(".claude").join("settings.local.json")
            }
        );
    }

    #[test]
    fn the_environment_wins_when_no_settings_file_sets_one_and_the_default_is_last() {
        let home = tempfile::tempdir().expect("temp home");
        let cwd = tempfile::tempdir().expect("temp cwd");
        write(
            &home.path().join(".claude").join("settings.json"),
            "{\"model\": \"opus\"}",
        );

        let from_env = resolve_base_url(Some("http://127.0.0.1:9999"), home.path(), cwd.path());
        assert_eq!(from_env.route.source, BaseUrlSource::ProcessEnv);
        assert_eq!(from_env.route.url, "http://127.0.0.1:9999");

        let bare = resolve_base_url(None, home.path(), cwd.path());
        assert_eq!(bare.route.source, BaseUrlSource::Default);
        assert_eq!(bare.route.url, DEFAULT_BASE_URL);

        // An empty value is not a route. Claude Code ignores it, and reporting
        // it as the answer would name a source that decides nothing.
        let empty = resolve_base_url(Some("  "), home.path(), cwd.path());
        assert_eq!(empty.route.source, BaseUrlSource::Default);
    }

    /// A settings file that cannot be parsed is the likeliest hiding place for
    /// the answer, so it is reported rather than read as "sets nothing".
    #[test]
    fn an_unreadable_settings_file_is_reported_and_resolution_continues() {
        let home = tempfile::tempdir().expect("temp home");
        let cwd = tempfile::tempdir().expect("temp cwd");
        write(
            &cwd.path().join(".claude").join("settings.json"),
            "{ not json",
        );
        write(
            &home.path().join(".claude").join("settings.json"),
            &settings_with("https://user.example.com"),
        );

        let resolved = resolve_base_url(None, home.path(), cwd.path());

        assert_eq!(resolved.route.url, "https://user.example.com");
        assert_eq!(resolved.problems.len(), 1, "{:?}", resolved.problems);
        assert!(
            resolved.problems[0].contains("settings.json"),
            "the problem must name the file: {}",
            resolved.problems[0]
        );
    }

    #[test]
    fn a_non_string_base_url_is_a_problem_not_a_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.json");
        write(&path, &format!("{{\"env\": {{\"{BASE_URL_VAR}\": 8080}}}}"));

        assert!(matches!(
            read_settings_base_url(&path),
            SettingsRead::Unreadable(_)
        ));
    }

    #[test]
    fn authority_reads_host_and_port_including_ipv6_and_userinfo() {
        assert_eq!(
            authority("http://127.0.0.1:3456"),
            Some(("127.0.0.1".to_string(), 3456))
        );
        assert_eq!(
            authority("https://api.anthropic.com"),
            Some(("api.anthropic.com".to_string(), 443))
        );
        assert_eq!(
            authority("http://localhost/v1/messages?x=1"),
            Some(("localhost".to_string(), 80))
        );
        assert_eq!(
            authority("http://[::1]:3456/v1"),
            Some(("::1".to_string(), 3456))
        );
        assert_eq!(
            authority("http://user:pw@127.0.0.1:3456"),
            Some(("127.0.0.1".to_string(), 3456))
        );
        assert_eq!(authority("api.anthropic.com"), None);
        assert_eq!(authority("ftp://127.0.0.1:3456"), None);
    }

    #[test]
    fn routing_is_loopback_plus_the_configured_port() {
        assert!(routes_to_port("http://127.0.0.1:3456", 3456));
        assert!(routes_to_port("http://localhost:3456", 3456));
        assert!(routes_to_port("http://[::1]:3456", 3456));
        assert!(!routes_to_port("http://127.0.0.1:3457", 3456));
        assert!(!routes_to_port("https://api.anthropic.com", 3456));
        // Another machine's tcr on the same port is still not this proxy.
        assert!(!routes_to_port("http://192.168.1.9:3456", 3456));
    }

    /// The enumeration half, against a listener this test process is holding:
    /// the pid reported must be this process. A holder that reads as `none`
    /// while something is demonstrably listening is the failure that makes the
    /// whole `portHolder:` line worthless, and it cannot be caught by a test
    /// that only feeds [`holder_from`] canned argv.
    #[test]
    fn port_holder_names_the_process_actually_listening() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
        let port = listener.local_addr().expect("local addr").port();

        let holder = port_holder(port).expect("a held port must have a holder");

        assert_eq!(holder.pid, std::process::id());
    }

    #[test]
    fn the_holder_is_a_tcr_by_argv0_or_by_name() {
        let tcr = holder_from(11, "tcr", &["/usr/local/bin/tcr".into(), "server".into()]);
        assert!(tcr.is_tcr);
        let running_claude = holder_from(12, "tcr", &["/opt/tcr".into(), "run".into()]);
        assert!(running_claude.is_tcr, "`tcr run` hosts a proxy too");
        let stranger = holder_from(13, "node", &["/usr/bin/node".into(), "gateway.js".into()]);
        assert!(!stranger.is_tcr);
        assert_eq!(stranger.name, "node");
    }

    fn session(req_per_minute: Vec<u16>) -> tcr_status_wire::SessionRow {
        tcr_status_wire::SessionRow {
            session_id: "11111111-1111-1111-1111-111111111111".to_string(),
            account: None,
            model: None,
            first_seen_ms: 0,
            last_seen_ms: 0,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tools: Default::default(),
            req_per_minute,
            cost_usd: 0.0,
            cache_ttl_secs: None,
            provider: None,
            unpriced_requests: 0,
            reasoning_tokens: 0,
        }
    }

    fn payload_with(sessions: Vec<tcr_status_wire::SessionRow>) -> StatusPayload {
        StatusPayload {
            kind: crate::status::STATUS_KIND.to_string(),
            accounts: Vec::new(),
            build: Default::default(),
            http1_only: false,
            control: None,
            group_colors: Default::default(),
            sessions,
            sessions_summary: Default::default(),
            peers: Vec::new(),
            peers_error: None,
            mitm: None,
            codex_traffic: None,
            codex_upstream: None,
        }
    }

    /// The window is the TAIL of the sparkline (newest minutes), so a session
    /// that was busy twenty minutes ago and idle since contributes nothing.
    #[test]
    fn the_window_counts_only_the_newest_ten_minutes() {
        let mut old_traffic = vec![0u16; 30];
        old_traffic[0] = 40;
        old_traffic[5] = 7;
        let mut recent = vec![0u16; 30];
        recent[29] = 3;
        recent[25] = 2;

        let payload = payload_with(vec![session(old_traffic), session(recent)]);

        assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 5);
    }

    #[test]
    fn a_short_or_absent_sparkline_counts_what_it_has() {
        let payload = payload_with(vec![session(vec![1, 2]), session(Vec::new())]);
        assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 3);
    }

    /// `requests10m` answers "is Claude Code reaching this proxy", so it counts Claude sessions
    /// only. A proxy busy with codex says nothing about Claude's route, and counting its
    /// sessions would read a broken Claude route as healthy.
    #[test]
    fn requests_in_the_window_count_claude_sessions_only() {
        let mut claude = vec![0u16; 30];
        claude[29] = 5;
        let mut codex_traffic = vec![0u16; 30];
        codex_traffic[29] = 7;
        let mut codex = session(codex_traffic);
        codex.provider = Some("codex".to_string());
        let payload = payload_with(vec![session(claude), codex]);
        assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 5);
    }

    fn report_for(url: &str, source: BaseUrlSource, requests: Option<u64>) -> Report {
        Report {
            base_url: url.to_string(),
            base_url_source: source,
            proxy_port: 3456,
            routed_here: routes_to_port(url, 3456),
            port_holder: None,
            requests_in_window: requests,
            problems: Vec::new(),
            unknown_keys: Vec::new(),
            shell_route: None,
        }
    }

    /// `tcr setup claude-settings` writes a value doctor must call on-route.
    #[test]
    fn the_every_launch_value_is_on_route() {
        let url = crate::setup::proxy_base_url(3456);
        let report = report_for(
            &url,
            BaseUrlSource::UserSettings {
                path: PathBuf::from("/home/alice/.claude/settings.json"),
            },
            Some(2),
        );
        assert!(report.routed_here);
        assert_eq!(report.exit_code(), 0);
    }

    /// The shell route routes a typed `claude` when no settings file names a
    /// base URL, and does not when one does.
    #[test]
    fn the_shell_route_is_on_route_unless_a_settings_file_overrides_it() {
        let rc = PathBuf::from("/home/alice/.zshrc");
        let mut report = report_for(DEFAULT_BASE_URL, BaseUrlSource::Default, Some(2));
        assert_eq!(report.exit_code(), 2, "no shell route: off route");
        report.shell_route = Some(rc.clone());
        assert_eq!(report.exit_code(), 0);
        assert!(
            report.verdict().contains("/home/alice/.zshrc"),
            "{}",
            report.verdict()
        );

        let mut silent = report_for(DEFAULT_BASE_URL, BaseUrlSource::Default, None);
        silent.shell_route = Some(rc.clone());
        assert_eq!(silent.exit_code(), 3);

        let mut overridden = report_for(
            "https://gateway.example.com",
            BaseUrlSource::UserSettings {
                path: PathBuf::from("/home/alice/.claude/settings.json"),
            },
            Some(2),
        );
        overridden.shell_route = Some(rc);
        assert_eq!(
            overridden.exit_code(),
            2,
            "a settings file outranks tcr run's export"
        );
    }

    #[test]
    fn a_route_to_another_gateway_exits_two_and_names_the_file() {
        let home = tempfile::tempdir().expect("temp home");
        let settings = home.path().join(".claude").join("settings.json");
        let report = report_for(
            "https://gateway.example.com",
            BaseUrlSource::UserSettings {
                path: settings.clone(),
            },
            None,
        );

        assert_eq!(report.exit_code(), 2);
        assert_eq!(
            report.verdict(),
            format!(
                "verdict: this proxy is not on Claude's route: {} sets https://gateway.example.com",
                settings.display()
            )
        );
    }

    #[test]
    fn a_route_here_with_a_serving_proxy_exits_zero() {
        let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(4));

        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            report.verdict(),
            "verdict: Claude is routed to http://127.0.0.1:3456 (ANTHROPIC_BASE_URL); this proxy has served 4 requests"
        );
    }

    /// Zero served is NOT the same fact as nothing answering, and the exit code
    /// has to keep them apart: a proxy that is up and idle is healthy.
    #[test]
    fn nothing_answering_exits_three_and_zero_served_does_not() {
        let silent = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, None);
        assert_eq!(silent.exit_code(), 3);
        assert!(silent.verdict().contains("no proxy answered"));

        let idle = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0));
        assert_eq!(idle.exit_code(), 0);
    }

    #[test]
    fn the_text_block_is_greppable_and_the_json_carries_the_derived_fields() {
        let mut report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(2));
        report.port_holder = Some(holder_from(77, "tcr", &["/usr/local/bin/tcr".into()]));
        report
            .problems
            .push("settings.json: not readable as JSON".to_string());

        let text = report.render_text();
        assert!(text.contains("baseUrl: http://127.0.0.1:3456"), "{text}");
        assert!(text.contains("baseUrlSource: ANTHROPIC_BASE_URL"), "{text}");
        assert!(text.contains("portHolder: pid 77 tcr"), "{text}");
        assert!(text.contains("portHolderIsTcr: yes"), "{text}");
        assert!(text.contains("requests10m: 2"), "{text}");
        assert!(
            text.contains("problem: settings.json: not readable as JSON"),
            "{text}"
        );
        assert!(
            text.lines()
                .last()
                .is_some_and(|last| last.starts_with("verdict: ")),
            "the verdict is the last line: {text}"
        );

        let json = report.to_json();
        assert_eq!(json["exitCode"], 0);
        assert_eq!(json["requestsInWindow"], 2);
        assert_eq!(json["baseUrl"], "http://127.0.0.1:3456");
        assert_eq!(json["portHolder"]["isTcr"], true);
        assert_eq!(json["requestWindowMinutes"], 10);
        assert!(json["verdict"]
            .as_str()
            .is_some_and(|v| v.starts_with("verdict: ")));
        assert_eq!(
            json["unknownKeys"],
            serde_json::json!([]),
            "an empty array, never absent or null, when every key is known"
        );
    }

    fn config_from(json: &str) -> crate::config::Config {
        serde_json::from_str(json).expect("a test config parses")
    }

    /// The lane's gate, in Rust: a misspelled `quotaProbeSeconds` is named in
    /// the JSON and in the text, with the spelling it meant, and the exit code
    /// stays the routing answer.
    #[test]
    fn a_misspelled_config_key_is_named_and_the_exit_code_is_unchanged() {
        let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0))
            .with_config_keys(&config_from(r#"{"accounts":[],"quotaProbeSecs":60}"#));

        assert_eq!(
            report.to_json()["unknownKeys"],
            serde_json::json!([{"key": "quotaProbeSecs", "nearest": "quotaProbeSeconds"}])
        );
        let text = report.render_text();
        assert!(
            text.contains("unknownKey: quotaProbeSecs (did you mean quotaProbeSeconds?)"),
            "{text}"
        );
        assert_eq!(
            report.exit_code(),
            0,
            "an unknown key is not a routing failure"
        );
    }

    #[test]
    fn the_correct_spelling_reports_no_unknown_keys() {
        let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0))
            .with_config_keys(&config_from(r#"{"accounts":[],"quotaProbeSeconds":60}"#));

        assert_eq!(report.to_json()["unknownKeys"], serde_json::json!([]));
        assert!(!report.render_text().contains("unknownKey:"));
    }

    /// `tcr doctor` on a config holding a Codex account prints no `unknownKey:`
    /// line and reports `unknownKeys: []`. Loaded the way `cli::doctor` loads it,
    /// through `config::load`, so the Codex row's `credentialVersion` 2 is
    /// accepted first, and with a `codexUpstream` the user set, so the key is on
    /// disk. `codexUpstream` and each row's `provider` are modelled fields, so
    /// serde takes them before the unmodelled map doctor reads is built, and
    /// `codexUpstream` is in the registry besides.
    ///
    /// Watched red: rename the `codexUpstream` field's serde key so the key lands
    /// in that map AND drop its registry line, and it is named here. Either alone
    /// stays green, the other one still covering it.
    #[test]
    fn a_codex_config_reports_no_unknown_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(crate::paths::CONFIG_FILE);
        std::fs::write(
            &path,
            r#"{"codexUpstream":"http://127.0.0.1:9100","accounts":[
                 {"name":"alice@example.com","accessToken":"at-a"},
                 {"name":"alice@example.com/codex","provider":"codex","type":"oauth",
                  "accessToken":"at-c","accountUuid":"11111111-1111-4111-8111-111111111111",
                  "credentialVersion":2}]}"#,
        )
        .expect("write the config");
        let config = crate::config::load(&path).expect("a Codex config loads");
        let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0))
            .with_config_keys(&config);

        assert_eq!(report.to_json()["unknownKeys"], serde_json::json!([]));
        let text = report.render_text();
        assert!(!text.contains("unknownKey:"), "{text}");
    }

    // --- the Codex half -------------------------------------------------------

    use crate::stats::GateReason;
    use tcr_status_wire::{CodexRoute, CodexTraffic};

    fn pooled() -> CodexRoute {
        CodexRoute {
            pooled: true,
            code: None,
            reason: None,
            upstream: None,
        }
    }

    fn not_pooled(code: &str, reason: &str) -> CodexRoute {
        CodexRoute {
            pooled: false,
            code: Some(code.to_string()),
            reason: Some(reason.to_string()),
            upstream: None,
        }
    }

    /// A Codex report for a proxy on :3999 that answered: no Codex row yet, tcr's CA readable,
    /// the default upstream, codex on `PATH`, nothing set in this shell, codex signed in, and
    /// nothing served or refused.
    fn codex_report(route: CodexRoute) -> CodexReport {
        CodexReport {
            port: 3999,
            rows: Vec::new(),
            route,
            ca: PathBuf::from("/Users/example/.config/tcr-ca.pem"),
            ca_readable: true,
            upstream: "https://chatgpt.com".to_string(),
            upstream_is_default: true,
            bin: Ok((PathBuf::from("/opt/homebrew/bin/codex"), "PATH".to_string())),
            env: CodexEnv::default(),
            login: PathBuf::from("/Users/example/.codex/auth.json"),
            login_exists: true,
            requests_10m: Some(0),
            traffic: Some(CodexTraffic::default()),
        }
    }

    fn codex_row(name: &str, gate: Option<GateReason>) -> CodexRow {
        CodexRow {
            name: name.to_string(),
            gate,
            reserved_groups: Vec::new(),
            limit_reached_type: None,
        }
    }

    /// The nine Codex rows of a mixed fleet, one per state, in a shuffled order: kim reserved
    /// for research, hal held, gil refused and carol out for the week, the last three carrying
    /// the limit ChatGPT named.
    fn nine_rows() -> Vec<CodexRow> {
        let limited = |mut row: CodexRow| {
            row.limit_reached_type = Some("rate_limit_reached".to_string());
            row
        };
        vec![
            CodexRow {
                reserved_groups: vec!["research".to_string()],
                ..codex_row("kim@example.com", Some(GateReason::Reserved))
            },
            codex_row("bob@example.com/codex", Some(GateReason::Ok)),
            codex_row("erin@example.com/codex", Some(GateReason::Login)),
            limited(codex_row("hal@example.com", Some(GateReason::Hold))),
            codex_row("dana@example.com", Some(GateReason::Ok)),
            limited(codex_row("carol@example.com", Some(GateReason::SevenDay))),
            limited(codex_row("gil@example.com", Some(GateReason::Rejected))),
            codex_row("ivy@example.com", Some(GateReason::FiveHour)),
            codex_row("jo@example.com", Some(GateReason::Ok)),
        ]
    }

    /// `codexAccounts` counts the rows by gate in `GateReason` order and in the Codex words of
    /// tcr's own 429. A row held by an upstream hold or 403 that carries ChatGPT's own refusal
    /// counts as refused by ChatGPT, since that refusal is why it stays out; a window keeps its
    /// row. `byGate` stays keyed on the wire's gate.
    #[test]
    fn codex_accounts_count_rows_by_gate_phrase() {
        use GateReason::{FiveHour, Hold, Login, Ok, Refused, Rejected, Reserved, SevenDay};
        let gated = |gates: &[GateReason]| -> Vec<CodexRow> {
            gates
                .iter()
                .enumerate()
                .map(|(i, gate)| codex_row(&format!("row{i}@example.com"), Some(*gate)))
                .collect()
        };
        let report = |rows: Vec<CodexRow>| CodexReport {
            rows,
            ..codex_report(pooled())
        };

        let shuffled = gated(&[
            Reserved, Ok, Login, Hold, Ok, SevenDay, Rejected, FiveHour, Ok,
        ]);
        assert_eq!(
            report(shuffled.clone()).accounts_line(),
            "9 (3 in rotation, 1 on an upstream hold, 1 in the 5-hour window, 1 in the weekly \
             window, 1 needing re-login, 1 refused by ChatGPT, 1 unreachable in a reserved group)"
        );
        assert_eq!(
            report(shuffled.clone()).to_json()["accounts"],
            serde_json::json!({"total": 9, "byGate": {
                "ok": 3, "hold": 1, "five-hour": 1, "seven-day": 1, "login": 1, "rejected": 1,
                "reserved": 1
            }})
        );

        let mut held_by_chatgpt = shuffled;
        held_by_chatgpt[3].limit_reached_type = Some("rate_limit_reached".to_string());
        assert_eq!(
            report(held_by_chatgpt).accounts_line(),
            "9 (3 in rotation, 1 in the 5-hour window, 1 in the weekly window, 1 needing \
             re-login, 2 refused by ChatGPT, 1 unreachable in a reserved group)"
        );

        assert_eq!(
            report(gated(&[Ok, Ok])).accounts_line(),
            "2 (2 in rotation)"
        );
        let offline = report(vec![
            codex_row("bob@example.com/codex", None),
            codex_row("dana@example.com", None),
        ]);
        assert_eq!(offline.accounts_line(), "2 (no proxy answered)");
        assert_eq!(
            offline.to_json()["accounts"],
            serde_json::json!({"total": 2, "byGate": {}})
        );
        assert_eq!(report(Vec::new()).accounts_line(), "0");

        // The 403 hold, in the words tcr's own 429 gives it, counted after the hold.
        let refused = report(gated(&[Ok, Refused]));
        assert_eq!(
            refused.accounts_line(),
            "2 (1 in rotation, 1 refused upstream (403))"
        );
        assert_eq!(
            refused.to_json()["accounts"]["byGate"],
            serde_json::json!({"ok": 1, "refused": 1})
        );
        let mut refused_by_chatgpt = gated(&[Ok, Refused]);
        refused_by_chatgpt[1].limit_reached_type = Some("rate_limit_reached".to_string());
        let refused_by_chatgpt = report(refused_by_chatgpt);
        assert_eq!(
            refused_by_chatgpt.accounts_line(),
            "2 (1 in rotation, 1 refused by ChatGPT)"
        );
        assert_eq!(
            refused_by_chatgpt.to_json()["accounts"]["byGate"],
            serde_json::json!({"ok": 1, "refused": 1})
        );
    }

    /// The Codex block's lines, keys in a fixed order and one problem per line, as a mixed
    /// fleet's doctor prints them; then each line's other forms, one change at a time.
    #[test]
    fn the_codex_block_lines_are_in_order() {
        let base = CodexReport {
            rows: nine_rows(),
            requests_10m: Some(42),
            traffic: Some(CodexTraffic {
                refused_10m: 12,
                unpooled_10m: 3,
                ..CodexTraffic::default()
            }),
            ..codex_report(pooled())
        };
        let lines = base.block_lines();
        assert_eq!(
            lines,
            [
                "codexAccounts: 9 (3 in rotation, 1 in the 5-hour window, 1 in the weekly \
                 window, 1 needing re-login, 2 refused by ChatGPT, 1 unreachable in a reserved \
                 group)",
                "codexIntercept: yes",
                "codexCa: /Users/example/.config/tcr-ca.pem (readable)",
                "codexUpstream: https://chatgpt.com (default)",
                "codexBin: /opt/homebrew/bin/codex (PATH)",
                "codexEnv: HTTPS_PROXY unset · NO_PROXY unset · SSL_CERT_FILE unset",
                "codexLogin: /Users/example/.codex/auth.json exists (existence only; never read)",
                "codexRequests10m: 42",
                "codexRefused10m: 12",
                "codexUnpooled10m: 3",
                "codexProblem: kim@example.com is reserved for research, which no Codex request \
                 can ask for, so it serves nothing",
            ]
        );
        // The base block, with the line keyed `key` replaced by `line`.
        let one_change = |key: &str, line: &str| -> Vec<String> {
            lines
                .iter()
                .map(|old| {
                    if old.starts_with(&format!("{key}: ")) {
                        line.to_string()
                    } else {
                        old.clone()
                    }
                })
                .collect()
        };

        let unreadable = CodexReport {
            ca_readable: false,
            ..base.clone()
        };
        assert_eq!(
            unreadable.block_lines(),
            one_change(
                "codexCa",
                "codexCa: /Users/example/.config/tcr-ca.pem (not readable)"
            )
        );

        let elsewhere = CodexReport {
            upstream: "http://127.0.0.1:9".to_string(),
            upstream_is_default: false,
            ..base.clone()
        };
        assert_eq!(
            elsewhere.block_lines(),
            one_change(
                "codexUpstream",
                "codexUpstream: http://127.0.0.1:9 (set by codexUpstream)"
            )
        );

        let searched = CodexReport {
            bin: Err(crate::codex_local::CodexSearch::NotFound {
                searched: vec![
                    "TCR_CODEX_BIN".to_string(),
                    "CODEX_BIN".to_string(),
                    "PATH".to_string(),
                ],
            }),
            ..base.clone()
        };
        assert_eq!(
            searched.block_lines(),
            one_change(
                "codexBin",
                "codexBin: not found (searched TCR_CODEX_BIN, CODEX_BIN, PATH)"
            )
        );
        let bad_variable = CodexReport {
            bin: Err(crate::codex_local::CodexSearch::BadVariable {
                var: "TCR_CODEX_BIN",
                value: "/Users/example/plain".to_string(),
                bare: false,
            }),
            ..base.clone()
        };
        assert_eq!(
            bad_variable.block_lines(),
            one_change(
                "codexBin",
                "codexBin: not found (TCR_CODEX_BIN is /Users/example/plain, which is not an \
                 executable file)"
            )
        );
        let bare_name = CodexReport {
            bin: Err(crate::codex_local::CodexSearch::BadVariable {
                var: "CODEX_BIN",
                value: "codex-beta".to_string(),
                bare: true,
            }),
            ..base.clone()
        };
        assert_eq!(
            bare_name.block_lines(),
            one_change(
                "codexBin",
                "codexBin: not found (CODEX_BIN is codex-beta, and no executable codex-beta is \
                 on PATH)"
            )
        );

        let signed_out = CodexReport {
            login_exists: false,
            ..base.clone()
        };
        let mut expected = one_change(
            "codexLogin",
            "codexLogin: /Users/example/.codex/auth.json does not exist (existence only; never \
             read)",
        );
        expected.push(
            "codexProblem: codex needs a login of its own to start, even when pooled: run \
             `codex login` once"
                .to_string(),
        );
        assert_eq!(signed_out.block_lines(), expected);

        let unreported = CodexReport {
            traffic: None,
            ..base.clone()
        };
        let mut expected = one_change(
            "codexRefused10m",
            "codexRefused10m: n/a (the proxy does not report it)",
        );
        let unpooled = expected
            .iter()
            .position(|line| line.starts_with("codexUnpooled10m: "))
            .expect("an unpooled line");
        expected[unpooled] = "codexUnpooled10m: n/a (the proxy does not report it)".to_string();
        assert_eq!(unreported.block_lines(), expected);

        let offline = CodexReport {
            requests_10m: None,
            traffic: None,
            route: not_pooled("no-proxy", "no proxy answered on 127.0.0.1:3999"),
            ..base.clone()
        };
        let offline_lines = offline.block_lines();
        for line in [
            "codexIntercept: unknown",
            "codexRequests10m: none (no proxy answered)",
            "codexRefused10m: none (no proxy answered)",
            "codexUnpooled10m: none (no proxy answered)",
        ] {
            assert!(
                offline_lines.iter().any(|held| held == line),
                "{line:?} missing from {offline_lines:#?}"
            );
        }
        assert_eq!(
            offline_lines.last().map(String::as_str),
            Some("codexProblem: no proxy answered on 127.0.0.1:3999")
        );
    }

    /// A proxy variable in this shell that points at tcr sends a codex started here, outside
    /// `tcr run --codex`, into tcr's interception without its CA. Only the variable codex itself
    /// reads for an `https://` URL decides, and the problem is dropped when that codex goes
    /// around tcr (`NO_PROXY`), trusts tcr's CA, or meets no interception at all.
    #[test]
    fn an_older_proxy_uses_configured_codex_rows_for_the_restart_diagnosis() {
        let payload: StatusPayload = serde_json::from_value(serde_json::json!({
            "kind": crate::status::STATUS_KIND, "accounts": [], "build": {"sha":"abc1234"}
        }))
        .unwrap();
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "accounts": [{"name":"alice@example.com/codex", "provider":"codex", "accessToken":"at-fake"}]
        })).unwrap();
        let rows = codex_rows(Ok(&payload), &config);
        assert_eq!(
            rows.len(),
            1,
            "an older status payload cannot report configured Codex rows"
        );
        assert_eq!(rows[0].name, "alice@example.com/codex");
        assert_eq!(
            rows[0].gate, None,
            "configuration is not a measured live gate"
        );
    }

    #[test]
    fn an_unmarked_codex_proxy_reports_how_to_join_the_pool() {
        let report = CodexReport {
            env: CodexEnv {
                https_proxy: Some(("HTTPS_PROXY".into(), "http://127.0.0.1:3999".into())),
                ..CodexEnv::default()
            },
            ..codex_report(pooled())
        };
        let problem = &report.problems()[0];
        assert!(problem.contains("unmarked"), "{problem}");
        assert!(problem.contains("tcr run --codex"), "{problem}");
        assert!(
            !problem.contains("tcr-codex@"),
            "do not mark the whole shell: {problem}"
        );
        assert!(!problem.contains("fails TLS"), "{problem}");
    }

    #[test]
    fn a_codex_problem_names_https_proxy_pointing_at_tcr() {
        const AT_TCR: &str = "http://tcr-codex@127.0.0.1:3999";
        const CA: &str = "/Users/example/.config/tcr-ca.pem";
        const PROBLEM: &str = "HTTPS_PROXY in this shell points at tcr with the Codex marker \
                               but without its CA, so codex can fail TLS: use `tcr run --codex` \
                               with a CA bundle that includes tcr's CA; keep the marker scoped \
                               to that launch";
        let with_env = |route: CodexRoute, env: CodexEnv| CodexReport {
            env,
            ..codex_report(route)
        };
        let from_env = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            CodexEnv::from_env(&move |name| {
                vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
            })
        };
        let at_tcr = CodexEnv {
            https_proxy: Some(("HTTPS_PROXY".to_string(), AT_TCR.to_string())),
            ..CodexEnv::default()
        };
        let names_tcr = |report: &CodexReport| {
            report
                .problems()
                .iter()
                .any(|problem| problem.contains("in this shell points at tcr"))
        };
        let env_line = |report: &CodexReport| {
            report
                .block_lines()
                .into_iter()
                .find(|line| line.starts_with("codexEnv: "))
                .expect("an env line")
        };

        let report = with_env(pooled(), at_tcr.clone());
        assert_eq!(report.problems()[0], PROBLEM);
        assert_eq!(
            env_line(&report),
            "codexEnv: HTTPS_PROXY=http://…@127.0.0.1:3999 (points at tcr) · NO_PROXY unset · \
             SSL_CERT_FILE unset"
        );

        let exempt = with_env(
            pooled(),
            CodexEnv {
                no_proxy: Some(("NO_PROXY".to_string(), "chatgpt.com".to_string())),
                ..at_tcr.clone()
            },
        );
        assert!(!names_tcr(&exempt), "{:?}", exempt.problems());
        assert_eq!(
            env_line(&exempt),
            "codexEnv: HTTPS_PROXY=http://…@127.0.0.1:3999 (points at tcr) · \
             NO_PROXY=chatgpt.com (exempts chatgpt.com) · SSL_CERT_FILE unset"
        );
        for (label, env) in [
            (
                "CODEX_CA_CERTIFICATE names tcr's CA",
                CodexEnv {
                    codex_ca_certificate: Some(CA.to_string()),
                    ..at_tcr.clone()
                },
            ),
            (
                "SSL_CERT_FILE names tcr's CA and nothing overrides it",
                CodexEnv {
                    ssl_cert_file: Some(CA.to_string()),
                    ..at_tcr.clone()
                },
            ),
            (
                "the proxy is another one",
                CodexEnv {
                    https_proxy: Some((
                        "HTTPS_PROXY".to_string(),
                        "http://127.0.0.1:8080".to_string(),
                    )),
                    ..CodexEnv::default()
                },
            ),
        ] {
            assert!(
                !names_tcr(&with_env(pooled(), env)),
                "no problem when {label}"
            );
        }
        assert!(
            !names_tcr(&with_env(
                not_pooled(
                    "js-leaf",
                    &format!(
                        "the proxy on :3999 is serving the JS {} leaf",
                        crate::legacy_names::JS_PROXY_BIN
                    )
                ),
                at_tcr.clone()
            )),
            "a proxy that intercepts nothing of codex's cannot break its TLS"
        );

        // codex reads SSL_CERT_FILE only while CODEX_CA_CERTIFICATE is unset.
        let overridden = with_env(
            pooled(),
            CodexEnv {
                ssl_cert_file: Some(CA.to_string()),
                codex_ca_certificate: Some("/etc/other.pem".to_string()),
                ..at_tcr.clone()
            },
        );
        assert_eq!(overridden.problems()[0], PROBLEM);

        // Only the first of codex's four spellings that is set decides.
        let all_proxy = with_env(pooled(), from_env(&[("ALL_PROXY", AT_TCR)]));
        assert!(
            all_proxy.problems()[0].starts_with("ALL_PROXY in this shell points at tcr"),
            "{:?}",
            all_proxy.problems()
        );
        let shadowed = with_env(
            pooled(),
            from_env(&[
                ("HTTPS_PROXY", "http://127.0.0.1:8080"),
                ("ALL_PROXY", AT_TCR),
            ]),
        );
        assert!(!names_tcr(&shadowed), "{:?}", shadowed.problems());
        assert_eq!(
            with_env(pooled(), from_env(&[("HTTPS_PROXY", AT_TCR)])).problems()[0],
            PROBLEM,
            "the plain case reads the same through the environment"
        );

        // A proxy whose interception is unknown may still intercept codex.
        let predates = with_env(
            not_pooled(
                "predates",
                "the proxy on :3999 (build abc1234) predates Codex",
            ),
            at_tcr.clone(),
        );
        assert_eq!(predates.intercept(), "unknown");
        assert_eq!(predates.problems()[0], PROBLEM);
        assert_eq!(
            with_env(
                not_pooled("unknown-leaf", "the proxy on :3999 reports a leaf"),
                at_tcr.clone()
            )
            .intercept(),
            "unknown"
        );

        // A credential in the value is never printed.
        let with_userinfo = format!("http://{}:{}@127.0.0.1:3999", "u", "p");
        let credentialed = with_env(
            pooled(),
            CodexEnv {
                https_proxy: Some(("HTTPS_PROXY".to_string(), with_userinfo)),
                ..CodexEnv::default()
            },
        );
        assert_eq!(
            env_line(&credentialed),
            "codexEnv: HTTPS_PROXY=http://…@127.0.0.1:3999 (points at tcr) · NO_PROXY unset · \
             SSL_CERT_FILE unset"
        );
        let secret = format!("{}:{}", "u", "p");
        for line in credentialed.block_lines() {
            assert!(!line.contains(&secret), "{line}");
        }
        assert!(
            !credentialed.to_json().to_string().contains(&secret),
            "{}",
            credentialed.to_json()
        );
        assert_eq!(
            credentialed.to_json()["env"]["httpsProxy"],
            "http://…@127.0.0.1:3999"
        );
    }

    /// A Codex account in a reserved group serves nothing, because a Codex request carries no
    /// group: one problem per such row, its reserved groups joined, in row order.
    #[test]
    fn a_codex_problem_names_a_reserved_codex_account() {
        let reserved = |name: &str, groups: &[&str]| CodexRow {
            reserved_groups: groups.iter().map(|g| g.to_string()).collect(),
            ..codex_row(name, Some(GateReason::Reserved))
        };
        let report = CodexReport {
            rows: vec![
                reserved("kim@example.com", &["research"]),
                codex_row("bob@example.com/codex", Some(GateReason::Ok)),
                reserved("lee@example.com/codex", &["research", "orchard"]),
            ],
            ..codex_report(pooled())
        };
        assert_eq!(
            report.problems(),
            [
                "kim@example.com is reserved for research, which no Codex request can ask for, \
                 so it serves nothing",
                "lee@example.com/codex is reserved for research, orchard, which no Codex \
                 request can ask for, so it serves nothing",
            ]
        );
        let offline = CodexReport {
            rows: vec![codex_row("kim@example.com", None)],
            ..codex_report(pooled())
        };
        assert!(offline.problems().is_empty(), "{:?}", offline.problems());
    }

    /// The pooled verdict counts its requests in the right number: one Codex request reads
    /// `1 Codex request`, and any other count, none included, reads `requests`.
    ///
    /// Watched red: with the noun always plural, one request reads `1 Codex requests`.
    #[test]
    fn the_pooled_verdict_says_one_request_in_the_singular() {
        let verdict = |requests: u64| {
            CodexReport {
                requests_10m: Some(requests),
                ..codex_report(pooled())
            }
            .verdict()
        };
        assert_eq!(
            verdict(1),
            "codex is pooled: `tcr run --codex` routes chatgpt.com through this proxy, which \
             served 1 Codex request in the last 10 min"
        );
        for requests in [0, 2] {
            assert!(
                verdict(requests).ends_with(&format!(
                    "served {requests} Codex requests in the last 10 min"
                )),
                "{}",
                verdict(requests)
            );
        }
    }

    /// The Codex verdict reuses the doctor's exit codes: 0 pooled, 2 when codex would not be
    /// pooled and the verdict says why, 3 when no proxy answered or its status could not be
    /// read.
    #[test]
    fn the_codex_exit_codes_reuse_the_meanings() {
        let no_codex_account = crate::codex_route::no_codex_account_reason(3999, "chatgpt.com");
        assert_eq!(
            no_codex_account,
            "the proxy on :3999 holds no Codex account, so it blind-tunnels chatgpt.com and codex \
             keeps its own login — add one with `tcr login --codex`"
        );
        let mitm_off = "the proxy on :3999 is running without MITM: its TLS material failed to \
                        load at boot — see its log";
        let unreadable = "could not read the running proxy's status (the server did not answer \
                          within 5s)";
        let offline = |route: CodexRoute| CodexReport {
            requests_10m: None,
            traffic: None,
            ..codex_report(route)
        };
        let cases = [
            (
                CodexReport {
                    requests_10m: Some(42),
                    ..codex_report(pooled())
                },
                "codex is pooled: `tcr run --codex` routes chatgpt.com through this proxy, which \
                 served 42 Codex requests in the last 10 min"
                    .to_string(),
                0,
            ),
            (
                codex_report(not_pooled("mitm-off", mitm_off)),
                format!("codex is not pooled: {mitm_off}"),
                2,
            ),
            (
                codex_report(not_pooled("no-codex-account", &no_codex_account)),
                format!("codex is not pooled: {no_codex_account}"),
                2,
            ),
            (
                codex_report(not_pooled(
                    "no-ca",
                    "no readable CA at /Users/example/.config/tcr-ca.pem",
                )),
                "codex is not pooled: no readable CA at /Users/example/.config/tcr-ca.pem"
                    .to_string(),
                2,
            ),
            (
                offline(not_pooled(
                    "no-proxy",
                    "no proxy answered on 127.0.0.1:3999",
                )),
                "codex is not pooled: no proxy answered on 127.0.0.1:3999".to_string(),
                3,
            ),
            (
                offline(not_pooled("status-unreadable", unreadable)),
                format!("codex is not pooled: {unreadable}"),
                3,
            ),
        ];
        for (report, verdict, exit) in cases {
            assert_eq!(report.verdict(), verdict);
            assert_eq!(report.exit_code(), exit, "{verdict}");
            assert_eq!(report.to_json()["verdict"], verdict.as_str());
            assert_eq!(report.to_json()["exitCode"], exit);
        }
    }

    /// Beside a Codex block, Claude's served verdict says it counts Claude requests, in the
    /// text and the JSON alike; a fleet with no Codex block keeps the words it always had. A
    /// mixed fleet's exit code is Claude's; with no Claude account, or with `--codex`, it is
    /// Codex's.
    #[test]
    fn the_claude_verdict_names_claude_only_beside_codex() {
        let claude = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(3));
        let diagnosis = |claude: Report, codex: Option<CodexReport>| Diagnosis {
            claude,
            codex,
            holds_anthropic: true,
            codex_only: false,
        };
        let last_line = |diagnosis: &Diagnosis| {
            diagnosis
                .render_text()
                .lines()
                .last()
                .expect("a last line")
                .to_string()
        };

        let alone = diagnosis(claude.clone(), None);
        let today = "verdict: Claude is routed to http://127.0.0.1:3456 (ANTHROPIC_BASE_URL); \
                     this proxy has served 3 requests";
        assert_eq!(last_line(&alone), today);
        assert_eq!(alone.to_json()["verdict"], today);
        assert_eq!(alone.render_text(), claude.render_text());
        assert_eq!(alone.to_json(), claude.to_json());

        let mixed = diagnosis(claude.clone(), Some(codex_report(pooled())));
        let beside = "verdict: Claude is routed to http://127.0.0.1:3456 (ANTHROPIC_BASE_URL); \
                      this proxy has served 3 Claude requests";
        assert_eq!(last_line(&mixed), beside);
        assert_eq!(mixed.to_json()["verdict"], beside);
        let one = diagnosis(
            report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(1)),
            Some(codex_report(pooled())),
        );
        assert!(
            last_line(&one).ends_with("this proxy has served 1 Claude request"),
            "{}",
            last_line(&one)
        );

        for other_arm in [
            report_for(
                "https://gateway.example.com",
                BaseUrlSource::ProcessEnv,
                None,
            ),
            report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, None),
        ] {
            let verdict = other_arm.verdict();
            let mixed = diagnosis(other_arm.clone(), Some(codex_report(pooled())));
            assert_eq!(last_line(&mixed), verdict);
            assert_eq!(mixed.to_json()["verdict"], verdict.as_str());
            assert_eq!(last_line(&diagnosis(other_arm, None)), verdict);
        }

        // The exit code, per shape: Claude's exits 0, Codex's 3.
        assert_eq!(claude.exit_code(), 0);
        let codex_three = CodexReport {
            requests_10m: None,
            traffic: None,
            ..codex_report(not_pooled(
                "no-proxy",
                "no proxy answered on 127.0.0.1:3999",
            ))
        };
        assert_eq!(codex_three.exit_code(), 3);
        let shape = |codex: Option<CodexReport>, holds_anthropic: bool, codex_only: bool| {
            Diagnosis {
                claude: claude.clone(),
                codex,
                holds_anthropic,
                codex_only,
            }
            .exit_code()
        };
        assert_eq!(shape(None, true, false), 0, "no Codex block");
        assert_eq!(
            shape(Some(codex_three.clone()), true, false),
            0,
            "mixed: Claude's, always"
        );
        assert_eq!(
            shape(Some(codex_three.clone()), false, false),
            3,
            "no Claude account: Codex's"
        );
        assert_eq!(shape(Some(codex_three.clone()), true, true), 3, "--codex");
        assert_eq!(shape(Some(codex_three), false, true), 3, "--codex");
    }

    /// The shell route's served verdict says its count is Claude's beside a Codex block, as
    /// the base-URL arm does: `requests10m` counts Claude sessions alone, and a reader who also
    /// sees `codexRequests10m` would take "3 requests" for every request the proxy served.
    /// With no Codex block it keeps its own words, and with no proxy answering it names no
    /// count to qualify.
    #[test]
    fn the_shell_routes_verdict_names_claude_requests_beside_codex() {
        let shell = |served| {
            let mut report = report_for(DEFAULT_BASE_URL, BaseUrlSource::Default, served);
            report.shell_route = Some(PathBuf::from("/home/alice/.zshrc"));
            report
        };
        let head = "verdict: claude typed in a terminal goes through tcr run (the tcr block in \
                    /home/alice/.zshrc); apps that start claude themselves use \
                    https://api.anthropic.com (the default); ";

        let report = shell(Some(3));
        assert_eq!(
            report.verdict(),
            format!("{head}this proxy has served 3 requests")
        );
        let beside = format!("{head}this proxy has served 3 Claude requests");
        assert_eq!(report.verdict_beside_codex(), beside);
        let mixed = Diagnosis {
            claude: report,
            codex: Some(codex_report(pooled())),
            holds_anthropic: true,
            codex_only: false,
        };
        assert_eq!(
            mixed.render_text().lines().last(),
            Some(beside.as_str()),
            "{}",
            mixed.render_text()
        );
        assert_eq!(mixed.to_json()["verdict"], beside.as_str());
        assert_eq!(mixed.exit_code(), 0, "the shell route is Claude's route");

        assert_eq!(
            shell(Some(1)).verdict_beside_codex(),
            format!("{head}this proxy has served 1 Claude request")
        );
        let silent = shell(None);
        assert_eq!(silent.verdict_beside_codex(), silent.verdict());
    }

    /// A config's unknown keys ride the Claude half: after `requests10m:` and Claude's
    /// `problem:` lines, before `codexAccounts:`, and in the top-level `unknownKeys`. With
    /// `--codex` there is no Claude half, and they come right before the verdict and in the
    /// document's own `unknownKeys`: a misspelled `codexUpstream` matters most there.
    #[test]
    fn unknown_config_keys_ride_the_claude_half_in_every_shape() {
        const LINE: &str = "unknownKey: codexUpstrem (did you mean codexUpstream?)";
        let claude = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(3))
            .with_config_keys(&config_from(
                r#"{"accounts":[],"codexUpstrem":"https://chatgpt.com"}"#,
            ));
        let keys = serde_json::json!([{"key": "codexUpstrem", "nearest": "codexUpstream"}]);
        let diagnosis = |codex: bool, holds_anthropic: bool, codex_only: bool| Diagnosis {
            claude: claude.clone(),
            codex: codex.then(|| codex_report(pooled())),
            holds_anthropic,
            codex_only,
        };
        let position = |lines: &[&str], prefix: &str| {
            lines
                .iter()
                .position(|line| line.starts_with(prefix))
                .unwrap_or_else(|| panic!("no line starts {prefix:?}: {lines:#?}"))
        };

        let alone = diagnosis(false, true, false);
        let text = alone.render_text();
        assert_eq!(text, claude.render_text());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[lines.len() - 2], LINE);

        for (label, holds_anthropic) in [("mixed", true), ("Codex-only", false)] {
            let shape = diagnosis(true, holds_anthropic, false);
            let text = shape.render_text();
            let lines: Vec<&str> = text.lines().collect();
            let unknown = position(&lines, "unknownKey: ");
            assert_eq!(lines[unknown], LINE, "{label}");
            assert!(
                position(&lines, "requests10m: ") < unknown
                    && unknown < position(&lines, "codexAccounts: "),
                "{label}: {text}"
            );
            assert_eq!(shape.to_json()["unknownKeys"], keys, "{label}");
            assert!(shape.to_json()["codex"].is_object(), "{label}");
        }

        let codex_only = diagnosis(true, true, true);
        let text = codex_only.render_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[lines.len() - 2], LINE, "{text}");
        assert!(lines[lines.len() - 1].starts_with("verdict: "), "{text}");
        assert_eq!(codex_only.to_json()["unknownKeys"], keys);
        assert!(codex_only.to_json().get("baseUrl").is_none());
    }

    /// `codexRequests10m` counts Codex sessions only, the sessions `requests10m` leaves out.
    #[test]
    fn codex_requests_in_the_window_count_codex_sessions_only() {
        let mut claude = vec![0u16; 30];
        claude[29] = 5;
        let mut codex_traffic = vec![0u16; 30];
        codex_traffic[29] = 7;
        codex_traffic[10] = 40;
        let mut codex = session(codex_traffic);
        codex.provider = Some("codex".to_string());
        let payload = payload_with(vec![session(claude), codex]);
        assert_eq!(
            codex_requests_in_window(&payload, REQUEST_WINDOW_MINUTES),
            7
        );
        assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 5);
    }
}
