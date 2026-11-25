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
use crate::setup::{BASE_URL_VAR, routes_to_port};
use crate::stats::GateReason;
use crate::status::StatusPayload;

/// The base URL Claude Code talks to when nothing sets one.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

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

/// The process holding the proxy port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortHolder {
    pub pid: u32,
    /// The process's own name, as the OS reports it.
    pub name: String,
    /// Whether that process is a `tcr`.
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
    let is_proxy_program = |token: &str| token == "tcr" || token.ends_with("/tcr");
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
    pub const fn routed_by_shell(&self) -> bool {
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
    pub const fn exit_code(&self) -> i32 {
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
        Self {
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

    /// Whether Codex's selected extra-CA file names or contains tcr's certificate.
    fn trusts(&self, ca: &Path) -> bool {
        self.codex_ca_certificate
            .as_deref()
            .or(self.ssl_cert_file.as_deref())
            .is_some_and(|file| crate::tls::ca_bundle_contains(Path::new(file), ca))
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
        use crate::codex_route::{LEAF_MISSING_HOST, MITM_OFF, NO_CA, NO_CODEX_ACCOUNT};
        if self.route.pooled {
            return "yes";
        }
        match self.route.code.as_deref() {
            Some(NO_CA) => "yes",
            Some(MITM_OFF | NO_CODEX_ACCOUNT | LEAF_MISSING_HOST) => "no",
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
                "auth.json is absent; a keyring-backed Codex login is not checked. Run codex \
                 once to verify its own sign-in before using the pool"
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
    let ca = crate::tls::minted_ca_path();
    let ca_readable = ca.is_file();
    let mut route = crate::codex_route::codex_route(live, port, &ca, ca_readable, rows.len());
    if route.pooled
        && let Err(reason) =
            crate::codex_route::codex_ca_for_launch(&ca, &|var| std::env::var(var).ok())
    {
        route.pooled = false;
        route.code = Some(crate::codex_route::NO_CA.to_string());
        route.reason = Some(reason);
    }
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
    if entry.is_empty() || entry.contains('/') || entry.parse::<IpAddr>().is_ok() {
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
mod tests;
