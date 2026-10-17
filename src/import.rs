//! `tcr import`: find the Claude and Codex subscription logins other tools on this machine
//! already hold, say which of them tcr does not have yet, and, with `--yes`, import those.
//!
//! # Where it looks
//!
//! - **claude-swap**: `<root>/sequence.json` lists the accounts (email, account UUID, org UUID,
//!   org name, and a `kind` that is `api_key` for an API-key slot); each slot's login is
//!   `<root>/credentials/.creds-<n>-<email>.enc` (standard base64 of a Claude Code credentials
//!   document) when that file exists, else the login Keychain item with service `claude-swap`
//!   and account `account-<n>-<email>` (macOS only). `<root>` is `~/.claude-swap-backup` on
//!   macOS; elsewhere `$XDG_DATA_HOME/claude-swap` when that is set and absolute, else
//!   `~/.local/share/claude-swap` (claude-swap 8fdc89a, `paths.py:105-123`,
//!   `credentials.py:1076-1093,1177-1215`). The slot `activeAccountNumber` names is skipped:
//!   Claude Code has refreshed that login since claude-swap backed it up, and the Claude Code
//!   source below holds the live copy.
//! - **`CLIProxyAPI`** (and Quotio and ccs, which keep their pools in its format): every `*.json`
//!   directly inside `~/.cli-proxy-api`, `~/.ccs/cliproxy/auth` and `~/.ccs/cliproxy/auth-paused`.
//!   One file per login, with a `type`: `claude` and `codex` are read, every other type is
//!   ignored without a line (`CLIProxyAPI` 911f49c, `internal/auth/claude/token.go:21-51`,
//!   `internal/auth/codex/token.go:220-245`).
//! - **Claude Code**: the login `tcr login --from-claude-code` reads
//!   ([`crate::claude_code_login::read_claude_code_login`]), named by `~/.claude.json`'s
//!   `oauthAccount` (`accountUuid`, `organizationUuid`, `emailAddress`; nothing else in that file
//!   is read).
//! - **Codex CLI**: the login `tcr login --from-codex` reads ([`crate::codex_login::auth_path`]).
//!
//! Each location takes an override, for a store kept elsewhere and for the tests, which must
//! never read the developer's real stores: [`CLAUDE_SWAP_DIR_ENV`], [`CLI_PROXY_API_DIRS_ENV`],
//! [`CLAUDE_CODE_ACCOUNT_ENV`], and the two the login verbs already honour. Keychain reads all
//! stop under [`crate::claude_code_login::NO_KEYCHAIN_ENV`].
//!
//! # What the listing does not do
//!
//! `tcr import` alone reads files and Keychain items and prints. It never refreshes a token,
//! never fetches a profile, never writes the config and never talks to a running proxy. A login
//! is "already in tcr" by the identity its source stores ([`StoredClaudeIdentity::existing_row`],
//! [`crate::oauth::existing_codex_row`]), never by asking anyone.
//!
//! # What `--yes` costs
//!
//! Refresh tokens are single-use: the first refresh tcr makes kills the copy the other tool
//! holds. So an import from claude-swap or `CLIProxyAPI` is a take, not a share, and the run says
//! so once before its first write ([`take_notice`]). Claude Code and Codex CLI imports print the
//! lines their `tcr login` twins print.
//!
//! Every write goes through the path `tcr login` uses, so the live-proxy add route, naming and
//! the refusals behave the same. A login that is already a row is never written, checked again
//! against the config each write is based on: an import must not replace tcr's own working
//! token with another tool's copy.
//!
//! Nothing here prints a token, puts one in an error, or quotes a whole document. A file that
//! does not parse is named with serde's error class and position, never its message, which
//! quotes the value it could not use.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

use crate::claude_code_login::{ClaudeCodeLogin, KeychainError};
use crate::codex_login::CodexLogin;
use crate::config::Config;
use crate::oauth::StoredClaudeIdentity;
use crate::provider::Provider;

/// claude-swap's data directory, read instead of the platform default.
pub const CLAUDE_SWAP_DIR_ENV: &str = "TCR_CLAUDE_SWAP_DIR";
/// The `CLIProxyAPI` auth folders to scan instead of the three defaults, separated as `PATH` is.
pub const CLI_PROXY_API_DIRS_ENV: &str = "TCR_CLIPROXYAPI_DIRS";
/// The file to read Claude Code's `oauthAccount` from instead of `~/.claude.json`.
pub const CLAUDE_CODE_ACCOUNT_ENV: &str = "TCR_CLAUDE_CODE_ACCOUNT";

/// The Keychain service claude-swap keeps each slot's login under (`credentials.py:113`).
const CLAUDE_SWAP_KEYCHAIN_SERVICE: &str = "claude-swap";

/// Why an API-key entry is not imported (Gil, 2026-10-14: subscriptions only).
pub const API_KEY_SKIP: &str = "API key, tcr serves subscriptions only";
/// Why an entry whose Keychain read failed is not imported. Said once per entry; the read is
/// not retried in the same run.
pub const KEYCHAIN_SKIP: &str = "keychain read refused or timed out";

/// Where a login was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    ClaudeCode,
    CodexCli,
    ClaudeSwap,
    CliProxyApi,
}

impl Source {
    /// The name the listing gives it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::CodexCli => "Codex CLI",
            Self::ClaudeSwap => "claude-swap",
            Self::CliProxyApi => "CLIProxyAPI",
        }
    }

    /// Whether importing from it is a take ([`take_notice`]): the source keeps refreshing its
    /// own copy and has no line of its own about that.
    const fn is_take(self) -> bool {
        matches!(self, Self::ClaudeSwap | Self::CliProxyApi)
    }
}

/// What a found entry holds. Deliberately not `Debug`: a [`ClaudeCodeLogin`] prints its tokens.
pub enum Holding {
    Claude {
        login: ClaudeCodeLogin,
        identity: StoredClaudeIdentity,
    },
    Codex(CodexLogin),
    /// Already this row of the config, by the identity the source stores, so its login was
    /// never read: a claude-swap slot tcr already has costs no Keychain read, and no dialog.
    Present(String),
    /// Not importable, and why.
    Skipped(String),
}

/// One entry a source holds.
pub struct Found {
    pub source: Source,
    /// Which entry of the source: a slot, a file. Empty for a source that holds one login.
    pub place: String,
    /// `None` only for a line about the source itself (a file that does not parse).
    pub provider: Option<Provider>,
    pub email: Option<String>,
    pub holding: Holding,
}

impl Found {
    fn skipped(
        source: Source,
        place: String,
        provider: Option<Provider>,
        email: Option<String>,
        why: impl Into<String>,
    ) -> Self {
        Self {
            source,
            place,
            provider,
            email,
            holding: Holding::Skipped(why.into()),
        }
    }
}

/// What tcr makes of an entry, against the config as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    New,
    /// The source stores no identity to compare, so only the import can tell: `--yes` asks the
    /// profile endpoint, and writes nothing when the answer is a row tcr already has.
    Unknown,
    Already(String),
    Skipped(String),
}

impl Verdict {
    const fn imports(&self) -> bool {
        matches!(self, Self::New | Self::Unknown)
    }

    fn text(&self) -> String {
        match self {
            Self::New => "new".to_string(),
            Self::Unknown => {
                "unknown: no stored identity, --yes checks it before writing".to_string()
            }
            Self::Already(name) => format!("already in tcr as '{name}'"),
            Self::Skipped(why) => format!("skipped: {why}"),
        }
    }
}

/// The verdict on `found` against `accounts`.
pub fn verdict(found: &Found, config: &Config) -> Verdict {
    match &found.holding {
        Holding::Skipped(why) => Verdict::Skipped(why.clone()),
        Holding::Present(name) => Verdict::Already(name.clone()),
        Holding::Claude { identity, .. } => {
            if let Some(row) = identity.existing_row(&config.accounts) {
                Verdict::Already(row.name.clone())
            } else if identity.account_uuid.is_none() && identity.email.is_none() {
                Verdict::Unknown
            } else {
                Verdict::New
            }
        }
        Holding::Codex(login) => crate::oauth::existing_codex_row(config, login)
            .map_or(Verdict::New, |row| Verdict::Already(row.name.clone())),
    }
}

/// Where every source is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locations {
    pub claude_swap_root: Option<PathBuf>,
    pub cli_proxy_api_dirs: Vec<PathBuf>,
    pub claude_code_account_file: Option<PathBuf>,
    /// `$HOME`, to shorten the paths the listing prints.
    pub home: Option<PathBuf>,
}

/// The environment [`Locations`] is decided from, already read, so a test decides it over
/// values rather than this process's shared environment.
#[derive(Debug, Clone, Default)]
pub struct LocationInputs {
    pub home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub claude_swap_dir: Option<PathBuf>,
    pub cli_proxy_api_dirs: Option<std::ffi::OsString>,
    pub claude_code_account: Option<PathBuf>,
    /// Whether claude-swap lays itself out the macOS way.
    pub macos: bool,
}

impl Locations {
    /// This process's own.
    pub fn from_env() -> Self {
        let path = |key: &str| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self::from_inputs(LocationInputs {
            home: path("HOME"),
            xdg_data_home: path("XDG_DATA_HOME"),
            claude_swap_dir: path(CLAUDE_SWAP_DIR_ENV),
            cli_proxy_api_dirs: std::env::var_os(CLI_PROXY_API_DIRS_ENV)
                .filter(|value| !value.is_empty()),
            claude_code_account: path(CLAUDE_CODE_ACCOUNT_ENV),
            macos: cfg!(target_os = "macos"),
        })
    }

    /// An override wins; then claude-swap's own rule (`paths.py:105-123`) and the three
    /// `CLIProxyAPI` folders under `HOME`. With no `HOME` and no override a source has no location.
    pub fn from_inputs(inputs: LocationInputs) -> Self {
        let home = inputs.home;
        let claude_swap_root = inputs.claude_swap_dir.or_else(|| {
            if inputs.macos {
                return home.as_ref().map(|h| h.join(".claude-swap-backup"));
            }
            match inputs.xdg_data_home.filter(|xdg| xdg.is_absolute()) {
                Some(xdg) => Some(xdg.join("claude-swap")),
                None => home
                    .as_ref()
                    .map(|h| h.join(".local").join("share").join("claude-swap")),
            }
        });
        let cli_proxy_api_dirs = match inputs.cli_proxy_api_dirs {
            Some(list) => std::env::split_paths(&list)
                .filter(|dir| !dir.as_os_str().is_empty())
                .collect(),
            None => home.as_ref().map_or_else(Vec::new, |h| {
                vec![
                    h.join(".cli-proxy-api"),
                    h.join(".ccs").join("cliproxy").join("auth"),
                    h.join(".ccs").join("cliproxy").join("auth-paused"),
                ]
            }),
        };
        let claude_code_account_file = inputs
            .claude_code_account
            .or_else(|| home.as_ref().map(|h| h.join(".claude.json")));
        Self {
            claude_swap_root,
            cli_proxy_api_dirs,
            claude_code_account_file,
            home,
        }
    }

    /// `path`, with `$HOME` written as `~`.
    fn shown(&self, path: &Path) -> String {
        match self
            .home
            .as_deref()
            .and_then(|home| path.strip_prefix(home).ok())
        {
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        }
    }
}

/// Keychain reads for one run: after one is refused or times out, every later one is answered
/// with the same failure without asking again, so an ignored dialog costs one timeout, not one
/// per slot.
#[derive(Default)]
struct KeychainGate {
    failed: Option<KeychainError>,
}

impl KeychainGate {
    fn read(&mut self, service: &str, account: &str) -> Result<Option<String>, KeychainError> {
        if let Some(failed) = self.failed {
            return Err(failed);
        }
        let read = crate::claude_code_login::read_keychain_item(service, Some(account));
        if let Err(err @ (KeychainError::Refused | KeychainError::TimedOut)) = read {
            self.failed = Some(err);
        }
        read
    }
}

/// Every entry every source holds, in a fixed order: Claude Code, Codex CLI, claude-swap,
/// `CLIProxyAPI`. `config` is read only to skip the Keychain read of a claude-swap slot tcr
/// already has ([`Holding::Present`]).
pub fn discover(locations: &Locations, config: &Config) -> Vec<Found> {
    let mut found = Vec::new();
    found.extend(claude_code_entry(locations));
    found.extend(codex_cli_entry());
    let mut keychain = KeychainGate::default();
    if let Some(root) = &locations.claude_swap_root {
        found.extend(claude_swap_entries(locations, root, config, &mut keychain));
    }
    for dir in &locations.cli_proxy_api_dirs {
        found.extend(cli_proxy_api_entries(locations, dir));
    }
    found
}

/// A serde error as a reason that names its class and position and never quotes the input.
fn shape_error(what: &str, err: &serde_json::Error) -> String {
    format!(
        "{what} is not the expected JSON ({:?} error at line {} column {})",
        err.classify(),
        err.line(),
        err.column()
    )
}

/// Why a Claude login with no refresh token is not imported: it is a setup-token, which tcr
/// adds by its own verb.
const NO_REFRESH_SKIP: &str = "no refresh token (a setup-token: add it with `tcr login --token`)";

// ---------------------------------------------------------------------------------------------
// Claude Code and the Codex CLI
// ---------------------------------------------------------------------------------------------

/// `~/.claude.json`'s `oauthAccount`, and only the three keys tcr compares by.
fn claude_code_identity(file: Option<&Path>) -> StoredClaudeIdentity {
    #[derive(Deserialize)]
    struct Document {
        #[serde(rename = "oauthAccount")]
        oauth_account: Option<OauthAccount>,
    }
    #[derive(Deserialize)]
    struct OauthAccount {
        #[serde(rename = "accountUuid")]
        account_uuid: Option<String>,
        #[serde(rename = "organizationUuid")]
        organization_uuid: Option<String>,
        #[serde(rename = "emailAddress")]
        email_address: Option<String>,
    }
    let account = file
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Document>(&text).ok())
        .and_then(|doc| doc.oauth_account);
    match account {
        Some(a) => {
            StoredClaudeIdentity::new(a.email_address, a.account_uuid, a.organization_uuid, None)
        }
        None => StoredClaudeIdentity::default(),
    }
}

fn claude_code_entry(locations: &Locations) -> Option<Found> {
    let identity = claude_code_identity(locations.claude_code_account_file.as_deref());
    let email = identity.email.clone();
    let source = Source::ClaudeCode;
    let provider = Some(Provider::Anthropic);
    match crate::claude_code_login::read_claude_code_login() {
        Ok(None) => None,
        Err(_) => Some(Found::skipped(
            source,
            String::new(),
            provider,
            email,
            "its stored login could not be read as a Claude Code credentials document",
        )),
        Ok(Some(login)) if login.refresh_token.is_none() => Some(Found::skipped(
            source,
            String::new(),
            provider,
            email,
            NO_REFRESH_SKIP,
        )),
        Ok(Some(login)) => Some(Found {
            source,
            place: String::new(),
            provider,
            email,
            holding: Holding::Claude { login, identity },
        }),
    }
}

fn codex_cli_entry() -> Option<Found> {
    let source = Source::CodexCli;
    let provider = Some(Provider::Codex);
    let Ok(path) = crate::codex_login::auth_path(None) else {
        return None;
    };
    match crate::codex_login::read_codex_login(&path) {
        Ok(None) => None,
        Ok(Some(login)) => Some(Found {
            source,
            place: String::new(),
            provider,
            email: login.email.clone(),
            holding: Holding::Codex(login),
        }),
        Err(_) => {
            let api_key = std::fs::read_to_string(&path)
                .is_ok_and(|text| crate::codex_login::auth_json_is_api_key(&text));
            let why = if api_key {
                API_KEY_SKIP
            } else {
                "auth.json could not be read as a ChatGPT login"
            };
            Some(Found::skipped(source, String::new(), provider, None, why))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// claude-swap
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct SequenceFile {
    #[serde(rename = "activeAccountNumber", default)]
    active_account_number: Option<i64>,
    #[serde(default)]
    accounts: std::collections::BTreeMap<String, SequenceAccount>,
}

#[derive(Deserialize)]
struct SequenceAccount {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(rename = "organizationUuid", default)]
    organization_uuid: Option<String>,
    #[serde(rename = "organizationName", default)]
    organization_name: Option<String>,
    /// `api_key` for an API-key slot; absent or `oauth` for a login (`switcher.py:3131-3140`).
    #[serde(default)]
    kind: Option<String>,
}

fn claude_swap_entries(
    locations: &Locations,
    root: &Path,
    config: &Config,
    keychain: &mut KeychainGate,
) -> Vec<Found> {
    let source = Source::ClaudeSwap;
    let provider = Some(Provider::Anthropic);
    let index = root.join("sequence.json");
    let text = match std::fs::read_to_string(&index) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => {
            return vec![Found::skipped(
                source,
                locations.shown(&index),
                provider,
                None,
                "sequence.json could not be read",
            )];
        }
    };
    let sequence: SequenceFile = match serde_json::from_str(&text) {
        Ok(sequence) => sequence,
        Err(err) => {
            return vec![Found::skipped(
                source,
                locations.shown(&index),
                provider,
                None,
                shape_error("sequence.json", &err),
            )];
        }
    };
    let mut slots: Vec<(i64, SequenceAccount)> = sequence
        .accounts
        .into_iter()
        .filter_map(|(key, account)| key.parse::<i64>().ok().map(|n| (n, account)))
        .collect();
    slots.sort_by_key(|(n, _)| *n);
    slots
        .into_iter()
        .map(|(n, account)| {
            claude_swap_slot(
                root,
                n,
                account,
                sequence.active_account_number == Some(n),
                config,
                keychain,
            )
        })
        .collect()
}

fn claude_swap_slot(
    root: &Path,
    n: i64,
    account: SequenceAccount,
    active: bool,
    config: &Config,
    keychain: &mut KeychainGate,
) -> Found {
    let source = Source::ClaudeSwap;
    let provider = Some(Provider::Anthropic);
    let place = format!("slot {n}");
    let identity = StoredClaudeIdentity::new(
        account.email,
        account.uuid,
        account.organization_uuid,
        account.organization_name,
    );
    let email = identity.email.clone();
    if account.kind.as_deref() == Some("api_key") {
        return Found::skipped(source, place, provider, email, API_KEY_SKIP);
    }
    let Some(address) = email.clone() else {
        return Found::skipped(source, place, provider, None, "no email in sequence.json");
    };
    if active {
        return Found::skipped(
            source,
            place,
            provider,
            email,
            "the account active in Claude Code, which the Claude Code entry covers",
        );
    }
    if let Some(row) = identity.existing_row(&config.accounts) {
        return Found {
            source,
            place,
            provider,
            email,
            holding: Holding::Present(row.name.clone()),
        };
    }
    let json = match claude_swap_credentials(root, n, &address, keychain) {
        Ok(json) => json,
        Err(why) => return Found::skipped(source, place, provider, email, why),
    };
    match crate::claude_code_login::parse(&json) {
        Ok(Some(login)) if login.refresh_token.is_some() => Found {
            source,
            place,
            provider,
            email,
            holding: Holding::Claude { login, identity },
        },
        Ok(Some(_)) => Found::skipped(source, place, provider, email, NO_REFRESH_SKIP),
        Ok(None) => Found::skipped(
            source,
            place,
            provider,
            email,
            "its stored login holds no Claude OAuth login",
        ),
        Err(_) => Found::skipped(
            source,
            place,
            provider,
            email,
            "its stored login is not a Claude Code credentials document",
        ),
    }
}

/// Slot `n`'s Claude Code credentials document: the `.enc` file when it exists (claude-swap
/// reads it first, `credentials.py:1177-1215`), else the Keychain item. `Err` is the reason the
/// listing gives.
fn claude_swap_credentials(
    root: &Path,
    n: i64,
    email: &str,
    keychain: &mut KeychainGate,
) -> Result<String, String> {
    let enc = root
        .join("credentials")
        .join(format!(".creds-{n}-{email}.enc"));
    match std::fs::read_to_string(&enc) {
        Ok(encoded) => {
            return crate::codex_oauth::base64_standard_decode(encoded.trim())
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| "its .enc file is not base64 text".to_string());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("its .enc file could not be read".to_string()),
    }
    match keychain.read(
        CLAUDE_SWAP_KEYCHAIN_SERVICE,
        &format!("account-{n}-{email}"),
    ) {
        Ok(Some(json)) => Ok(json),
        Ok(None) => Err("no stored login (no .enc file and no keychain item)".to_string()),
        Err(KeychainError::Off) => Err(format!(
            "no .enc file, and keychain reads are off ({})",
            crate::claude_code_login::NO_KEYCHAIN_ENV
        )),
        Err(KeychainError::Refused | KeychainError::TimedOut) => Err(KEYCHAIN_SKIP.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// CLIProxyAPI
// ---------------------------------------------------------------------------------------------

/// One `CLIProxyAPI` auth file, as much of it as tcr reads. Every field optional: which are
/// present depends on `type`.
#[derive(Deserialize)]
struct AuthFile {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    account_uuid: Option<String>,
    #[serde(default)]
    organization_uuid: Option<String>,
    #[serde(default)]
    organization_name: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    plan_type: Option<String>,
    /// The access token's expiry, RFC 3339.
    #[serde(default)]
    expired: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    use_siwc: Option<bool>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// An RFC 3339 time as epoch milliseconds, `None` when it does not parse.
fn rfc3339_ms(value: Option<&str>) -> Option<i64> {
    let parsed =
        time::OffsetDateTime::parse(value?, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

fn cli_proxy_api_entries(locations: &Locations, dir: &Path) -> Vec<Found> {
    let source = Source::CliProxyApi;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => {
            return vec![Found::skipped(
                source,
                locations.shown(dir),
                None,
                None,
                "the folder could not be read",
            )];
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json") && path.is_file())
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|path| {
            let place = locations.shown(path);
            match std::fs::read_to_string(path) {
                Ok(text) => cli_proxy_api_entry(place, &text),
                Err(_) => Some(Found::skipped(
                    source,
                    place,
                    None,
                    None,
                    "the file could not be read",
                )),
            }
        })
        .collect()
}

/// The entry one auth file holds, `None` for a type tcr does not serve.
fn cli_proxy_api_entry(place: String, text: &str) -> Option<Found> {
    let source = Source::CliProxyApi;
    let file: AuthFile = match serde_json::from_str(text) {
        Ok(file) => file,
        Err(err) => {
            return Some(Found::skipped(
                source,
                place,
                None,
                None,
                shape_error("the file", &err),
            ));
        }
    };
    let provider = match file.kind.as_deref() {
        Some("claude") => Provider::Anthropic,
        Some("codex") => Provider::Codex,
        _ => return None,
    };
    let email = non_empty(file.email.clone());
    let access_token = non_empty(file.access_token);
    let refresh_token = non_empty(file.refresh_token);
    let has_api_key = non_empty(file.api_key).is_some();
    let (Some(access_token), Some(refresh_token)) = (access_token, refresh_token) else {
        let why = if has_api_key {
            API_KEY_SKIP
        } else if provider == Provider::Codex {
            // An entry signed in through the other OAuth client keeps its tokens in `siwc`, which
            // tcr cannot refresh (`token.go:220-245`).
            "signed in with another client"
        } else {
            "no access or refresh token"
        };
        return Some(Found::skipped(source, place, Some(provider), email, why));
    };
    if provider == Provider::Codex && file.use_siwc == Some(true) {
        return Some(Found::skipped(
            source,
            place,
            Some(provider),
            email,
            "signed in with another client",
        ));
    }
    let expires_at_ms = rfc3339_ms(file.expired.as_deref());
    let holding = match provider {
        Provider::Anthropic => Holding::Claude {
            login: ClaudeCodeLogin {
                access_token,
                refresh_token: Some(refresh_token),
                expires_at: expires_at_ms,
                subscription_type: None,
            },
            identity: StoredClaudeIdentity::new(
                email.clone(),
                file.account_uuid,
                file.organization_uuid,
                file.organization_name,
            ),
        },
        Provider::Codex => {
            let mut login = crate::codex_login::login_from_tokens(
                file.id_token.as_deref(),
                access_token,
                refresh_token,
                file.account_id,
            );
            login.email = login.email.or_else(|| email.clone());
            login.plan_type = login.plan_type.or_else(|| non_empty(file.plan_type));
            login.expires_at_ms = login.expires_at_ms.or(expires_at_ms);
            Holding::Codex(login)
        }
    };
    let email = match &holding {
        Holding::Codex(login) => login.email.clone(),
        _ => email,
    };
    Some(Found {
        source,
        place,
        provider: Some(provider),
        email,
        holding,
    })
}

// ---------------------------------------------------------------------------------------------
// The listing and the import
// ---------------------------------------------------------------------------------------------

/// The listing's lines, one per entry, in columns: where, `claude` or `codex`, the email, and
/// the verdict. Separated by two spaces, so a column never runs into the next.
fn lines(found: &[Found], verdicts: &[Verdict]) -> Vec<String> {
    let cells: Vec<[String; 3]> = found
        .iter()
        .map(|found| {
            let source = if found.place.is_empty() {
                found.source.label().to_string()
            } else {
                format!("{} {}", found.source.label(), found.place)
            };
            let provider = match found.provider {
                Some(Provider::Anthropic) => "claude",
                Some(Provider::Codex) => "codex",
                None => "-",
            };
            let email = found.email.as_deref().unwrap_or("-").to_string();
            [source, provider.to_string(), email]
        })
        .collect();
    let width = |column: usize| {
        cells
            .iter()
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or(0)
    };
    let (source_width, provider_width, email_width) = (width(0), width(1), width(2));
    cells
        .iter()
        .zip(verdicts)
        .map(|([source, provider, email], verdict)| {
            format!(
                "{source:<source_width$}  {provider:<provider_width$}  {email:<email_width$}  {}",
                verdict.text()
            )
        })
        .collect()
}

/// What the run says once before its first write when it takes logins from tools that keep
/// refreshing their own copies: which tools, and what to do there.
pub fn take_notice(sources: &[Source]) -> Option<String> {
    let mut named: Vec<&str> = Vec::new();
    for source in sources.iter().filter(|s| s.is_take()) {
        if !named.contains(&source.label()) {
            named.push(source.label());
        }
    }
    if named.is_empty() {
        return None;
    }
    let tools = named.join(" and ");
    Some(format!(
        "[tcr] tcr refreshes the logins it imports from now on, and each refresh token is \
         single-use, so the copies in {tools} stop working at tcr's first refresh: remove \
         those accounts from {tools}."
    ))
}

/// The config `tcr import` compares against, read and never written: the file's contents, or
/// an empty fleet when there is no file yet. A file that does not parse is an error naming its
/// position only, since the config holds tokens.
fn read_config(config_path: &Path) -> anyhow::Result<Config> {
    match crate::config::load(config_path) {
        Ok(config) => Ok(config),
        Err(crate::config::ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            serde_json::from_str("{}").context("building an empty config")
        }
        Err(crate::config::ConfigError::Io(err)) => {
            Err(err).with_context(|| format!("reading {}", config_path.display()))
        }
        Err(crate::config::ConfigError::Parse(err)) => anyhow::bail!(
            "{} is not a config tcr can read ({:?} error at line {} column {})",
            config_path.display(),
            err.classify(),
            err.line(),
            err.column()
        ),
    }
}

/// `tcr import [--yes]`.
pub async fn run(config_path: &Path, yes: bool) -> anyhow::Result<()> {
    let locations = Locations::from_env();
    let config = read_config(config_path)?;
    let found = discover(&locations, &config);
    if found.is_empty() {
        println!(
            "no logins found in Claude Code, the Codex CLI, claude-swap or CLIProxyAPI's auth \
             folders"
        );
        return Ok(());
    }
    let verdicts: Vec<Verdict> = found.iter().map(|f| verdict(f, &config)).collect();
    for line in lines(&found, &verdicts) {
        println!("{}", line.trim_end());
    }
    let new = verdicts.iter().filter(|v| v.imports()).count();
    if !yes {
        if new > 0 {
            let logins = if new == 1 { "login" } else { "logins" };
            println!(
                "run `{}` to import the {new} new {logins}",
                crate::oauth::IMPORT_COMMAND
            );
        }
        return Ok(());
    }
    if new == 0 {
        println!("nothing to import");
        return Ok(());
    }
    let taking: Vec<Source> = found
        .iter()
        .zip(&verdicts)
        .filter(|(_, v)| v.imports())
        .map(|(f, _)| f.source)
        .collect();
    if let Some(notice) = take_notice(&taking) {
        eprintln!("{notice}");
    }
    let mut failed = 0usize;
    for (entry, verdict) in found.into_iter().zip(verdicts) {
        if !verdict.imports() {
            continue;
        }
        let who = format!(
            "{} {}",
            entry.source.label(),
            entry.email.as_deref().unwrap_or(&entry.place)
        );
        match import_one(config_path, entry, verdict).await {
            Ok(Imported::Saved(name)) => println!("imported {who} as '{name}'"),
            Ok(Imported::Already(name)) => {
                println!("{who} is already in tcr as '{name}'; left as it is");
            }
            Err(err) => {
                failed += 1;
                eprintln!("[tcr] could not import {who}: {err:#}");
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} of {new} new logins could not be imported");
    }
    Ok(())
}

/// What one import did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Imported {
    Saved(String),
    /// The config the write was based on already held it: nothing was written.
    Already(String),
}

/// Whether a token whose expiry is `expires_at_ms` has to be refreshed before it can name its
/// account or pass a workspace check. A missing expiry is not proof of anything, and the write
/// path refreshes such a login at its first use.
const fn needs_refresh(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    crate::oauth::is_expired(expires_at_ms, now_ms)
}

/// A refused refresh as the failure line says it: the status, never the response body.
fn refresh_failure(err: &crate::oauth::OAuthError) -> anyhow::Error {
    match err {
        crate::oauth::OAuthError::AuthRejected { status, .. } => anyhow::anyhow!(
            "its access token had expired, and the refresh was refused (HTTP {status}): the \
             login no longer works there either. Nothing was written."
        ),
        crate::oauth::OAuthError::Transient(_) => anyhow::anyhow!(
            "its access token had expired, and the refresh failed (network or server error); \
             try again. Nothing was written."
        ),
    }
}

fn refresh_client() -> anyhow::Result<reqwest::Client> {
    crate::http_client::builder()
        .no_proxy()
        .build()
        .context("building the client for the refresh")
}

async fn import_one(
    config_path: &Path,
    entry: Found,
    verdict: Verdict,
) -> anyhow::Result<Imported> {
    let source = entry.source;
    match entry.holding {
        Holding::Skipped(why) => anyhow::bail!("skipped: {why}"),
        Holding::Present(name) => Ok(Imported::Already(name)),
        Holding::Claude { login, identity } => {
            import_claude(config_path, source, login, &identity, &verdict).await
        }
        Holding::Codex(login) => import_codex(config_path, source, login).await,
    }
}

async fn import_claude(
    config_path: &Path,
    source: Source,
    mut login: ClaudeCodeLogin,
    identity: &StoredClaudeIdentity,
    verdict: &Verdict,
) -> anyhow::Result<Imported> {
    if needs_refresh(login.expires_at, crate::now_ms()) {
        if *verdict == Verdict::Unknown {
            anyhow::bail!(
                "its access token has expired and its source stores no identity, so a refresh \
                 would spend the login before tcr could tell whether it already has it. Nothing \
                 was written."
            );
        }
        let refresh_token = login.refresh_token.clone().unwrap_or_default();
        let tokens = crate::oauth::refresh_access_token(&refresh_client()?, &refresh_token)
            .await
            .map_err(|err| refresh_failure(&err))?;
        login = ClaudeCodeLogin {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: Some(tokens.expires_at_ms),
            subscription_type: login.subscription_type,
        };
    }
    let imported = crate::oauth::import_claude_login(
        config_path,
        crate::oauth::ImportInto::NewRowOnly,
        false,
        None,
        None,
        login,
        Some(identity),
        crate::oauth::LoginCommand::Import,
        crate::oauth::LoginUi::Machine,
    )
    .await?;
    match imported {
        crate::oauth::ClaudeImported::Saved(name) => {
            if source == Source::ClaudeCode {
                eprintln!(
                    "[tcr] Claude Code: {}",
                    crate::oauth::CLAUDE_CODE_IMPORT_CONSEQUENCE
                );
            }
            Ok(Imported::Saved(name))
        }
        crate::oauth::ClaudeImported::AlreadyAs(name) => Ok(Imported::Already(name)),
        crate::oauth::ClaudeImported::FleetNotEmpty => {
            anyhow::bail!("the import refused a config that already has accounts")
        }
    }
}

async fn import_codex(
    config_path: &Path,
    source: Source,
    mut login: CodexLogin,
) -> anyhow::Result<Imported> {
    // Checked against the config this write is based on: an earlier import in this run may have
    // added this login from another source, and the write would update that row.
    let config = read_config(config_path)?;
    if let Some(row) = crate::oauth::existing_codex_row(&config, &login) {
        return Ok(Imported::Already(row.name.clone()));
    }
    if source == Source::CodexCli {
        let saved = crate::oauth::login_from_codex(
            config_path,
            false,
            None,
            None,
            None,
            crate::oauth::LoginUi::Terminal,
        )
        .await?;
        return Ok(Imported::Saved(saved));
    }
    if needs_refresh(login.expires_at_ms, crate::now_ms()) {
        let tokens = crate::codex_oauth::refresh_access_token_at(
            &refresh_client()?,
            &login.refresh_token,
            crate::codex_oauth::TOKEN_ENDPOINT,
        )
        .await
        .map_err(|err| refresh_failure(&err))?;
        let mut fresh = crate::codex_login::login_from_tokens(
            tokens.id_token.as_deref(),
            tokens.access_token,
            tokens.refresh_token.unwrap_or(login.refresh_token),
            login.account_id.clone(),
        );
        fresh.email = fresh.email.or(login.email);
        fresh.plan_type = fresh.plan_type.or(login.plan_type);
        login = fresh;
    }
    let saved = crate::oauth::import_codex_login_as(
        crate::oauth::LoginCommand::Import,
        config_path,
        false,
        None,
        None,
        login,
        None,
    )
    .await?;
    Ok(Imported::Saved(saved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Account;

    const ALICE_UUID: &str = "11111111-1111-4111-8111-111111111111";
    const ORG_A: &str = "22222222-2222-4222-8222-222222222222";
    const ORG_B: &str = "33333333-3333-4333-8333-333333333333";

    fn claude_row(name: &str, uuid: Option<&str>, org: Option<&str>) -> Account {
        Account {
            account_uuid: uuid.map(str::to_string),
            org_uuid: org.map(str::to_string),
            ..Account::new(name, Provider::Anthropic, "at-fake-row")
        }
    }

    fn config_with(accounts: Vec<Account>) -> Config {
        let mut config: Config = serde_json::from_str("{}").unwrap();
        config.accounts = accounts;
        config
    }

    fn claude_found(identity: StoredClaudeIdentity) -> Found {
        Found {
            source: Source::ClaudeSwap,
            place: "slot 1".to_string(),
            provider: Some(Provider::Anthropic),
            email: identity.email.clone(),
            holding: Holding::Claude {
                login: ClaudeCodeLogin {
                    access_token: "at-fake".to_string(),
                    refresh_token: Some("rt-fake".to_string()),
                    expires_at: None,
                    subscription_type: None,
                },
                identity,
            },
        }
    }

    fn identity(
        email: Option<&str>,
        uuid: Option<&str>,
        org: Option<&str>,
    ) -> StoredClaudeIdentity {
        StoredClaudeIdentity::new(
            email.map(str::to_string),
            uuid.map(str::to_string),
            org.map(str::to_string),
            None,
        )
    }

    /// One person in two orgs is two rows: the account UUID with its own org is already in tcr,
    /// the same UUID in another org is new. A source with no UUID falls back to the email.
    ///
    /// Watched red against an `existing_row` that compares the account UUID alone.
    #[test]
    fn a_claude_login_is_already_in_tcr_by_account_and_org_then_by_email() {
        let config = config_with(vec![claude_row(
            "alice@example.com",
            Some(ALICE_UUID),
            Some(ORG_A),
        )]);
        let same = claude_found(identity(
            Some("alice@example.com"),
            Some(ALICE_UUID),
            Some(ORG_A),
        ));
        assert_eq!(
            verdict(&same, &config),
            Verdict::Already("alice@example.com".to_string())
        );
        let other_org = claude_found(identity(
            Some("alice@example.com"),
            Some(ALICE_UUID),
            Some(ORG_B),
        ));
        assert_eq!(verdict(&other_org, &config), Verdict::New);
        let by_email = claude_found(identity(Some("ALICE@example.com"), None, None));
        assert_eq!(
            verdict(&by_email, &config),
            Verdict::Already("alice@example.com".to_string())
        );
        let nobody = claude_found(identity(None, None, None));
        assert_eq!(verdict(&nobody, &config), Verdict::Unknown);
        let stranger = claude_found(identity(Some("bob@example.com"), None, None));
        assert_eq!(verdict(&stranger, &config), Verdict::New);
    }

    /// claude-swap's root: the override, else `~/.claude-swap-backup` on macOS, else an absolute
    /// `$XDG_DATA_HOME/claude-swap`, else `~/.local/share/claude-swap`. The `CLIProxyAPI` folders
    /// are the three defaults unless the list overrides them.
    #[test]
    fn locations_follow_each_tools_own_rule_and_the_overrides_win() {
        let home = PathBuf::from("/tmp/x/home");
        let inputs = |macos: bool, xdg: Option<&str>| LocationInputs {
            home: Some(home.clone()),
            xdg_data_home: xdg.map(PathBuf::from),
            macos,
            ..LocationInputs::default()
        };
        let mac = Locations::from_inputs(inputs(true, Some("/tmp/x/xdg")));
        assert_eq!(mac.claude_swap_root, Some(home.join(".claude-swap-backup")));
        let linux = Locations::from_inputs(inputs(false, Some("/tmp/x/xdg")));
        assert_eq!(
            linux.claude_swap_root,
            Some(PathBuf::from("/tmp/x/xdg/claude-swap"))
        );
        let relative = Locations::from_inputs(inputs(false, Some("xdg")));
        assert_eq!(
            relative.claude_swap_root,
            Some(home.join(".local/share/claude-swap"))
        );
        assert_eq!(
            mac.cli_proxy_api_dirs,
            vec![
                home.join(".cli-proxy-api"),
                home.join(".ccs/cliproxy/auth"),
                home.join(".ccs/cliproxy/auth-paused"),
            ]
        );
        assert_eq!(
            mac.claude_code_account_file,
            Some(home.join(".claude.json"))
        );
        let overridden = Locations::from_inputs(LocationInputs {
            claude_swap_dir: Some(PathBuf::from("/tmp/x/swap")),
            cli_proxy_api_dirs: Some("/tmp/x/a:/tmp/x/b".into()),
            claude_code_account: Some(PathBuf::from("/tmp/x/claude.json")),
            ..inputs(true, None)
        });
        assert_eq!(
            overridden.claude_swap_root,
            Some(PathBuf::from("/tmp/x/swap"))
        );
        assert_eq!(
            overridden.cli_proxy_api_dirs,
            vec![PathBuf::from("/tmp/x/a"), PathBuf::from("/tmp/x/b")]
        );
        assert_eq!(
            overridden.claude_code_account_file,
            Some(PathBuf::from("/tmp/x/claude.json"))
        );
    }

    /// A `CLIProxyAPI` `claude` file becomes a login with its stored identity and its RFC 3339
    /// expiry; an API-key file and a Codex file with no outer tokens are skipped by name; any
    /// other type is not listed at all.
    #[test]
    fn cli_proxy_api_files_are_read_by_type() {
        let claude = r#"{"type":"claude","access_token":"at-fake","refresh_token":"rt-fake","email":"carol@example.com","account_uuid":"11111111-1111-4111-8111-111111111111","organization_uuid":"22222222-2222-4222-8222-222222222222","expired":"2100-01-01T00:00:00Z"}"#;
        let found = cli_proxy_api_entry("a.json".to_string(), claude).expect("a claude entry");
        let Holding::Claude { login, identity } = &found.holding else {
            panic!("a claude file is a Claude login");
        };
        assert_eq!(login.expires_at, Some(4_102_444_800_000));
        assert_eq!(identity.org_uuid.as_deref(), Some(ORG_A));
        assert_eq!(found.email.as_deref(), Some("carol@example.com"));

        let api_key = r#"{"type":"claude","api_key":"sk-fake","email":"carol@example.com"}"#;
        let found = cli_proxy_api_entry("b.json".to_string(), api_key).expect("listed");
        assert!(matches!(&found.holding, Holding::Skipped(why) if why == API_KEY_SKIP));

        let siwc = r#"{"type":"codex","email":"dave@example.com","use_siwc":true,"siwc":{"access_token":"x"}}"#;
        let found = cli_proxy_api_entry("c.json".to_string(), siwc).expect("listed");
        assert!(
            matches!(&found.holding, Holding::Skipped(why) if why == "signed in with another client")
        );

        assert!(
            cli_proxy_api_entry(
                "d.json".to_string(),
                r#"{"type":"gemini","access_token":"x"}"#
            )
            .is_none()
        );
    }

    /// A file of the wrong shape is named by serde's class and position, never by serde's
    /// message, which quotes the value it could not use: here, a token.
    ///
    /// Watched red against a reason built from `{err}`.
    #[test]
    fn a_file_of_the_wrong_shape_never_quotes_its_values() {
        let found = cli_proxy_api_entry(
            "e.json".to_string(),
            r#"{"type":"codex","use_siwc":"rt-must-not-echo"}"#,
        )
        .expect("listed");
        let Holding::Skipped(why) = &found.holding else {
            panic!("a file that does not parse is skipped");
        };
        assert!(!why.contains("rt-must-not-echo"), "{why}");
        assert!(why.contains("line 1"), "{why}");
    }

    /// The take notice names each take source once, and says nothing for Claude Code and the
    /// Codex CLI, whose imports print their own lines.
    #[test]
    fn the_take_notice_names_each_tool_once_and_skips_the_shared_ones() {
        assert_eq!(take_notice(&[Source::ClaudeCode, Source::CodexCli]), None);
        let notice = take_notice(&[Source::ClaudeSwap, Source::CliProxyApi, Source::ClaudeSwap])
            .expect("a notice");
        assert_eq!(notice.matches("claude-swap").count(), 2, "{notice}");
        assert!(notice.contains("claude-swap and CLIProxyAPI"), "{notice}");
        assert!(notice.contains("single-use"), "{notice}");
    }

    #[test]
    fn rfc3339_expiries_read_as_milliseconds_and_garbage_as_unknown() {
        assert_eq!(
            rfc3339_ms(Some("2100-01-01T00:00:00Z")),
            Some(4_102_444_800_000)
        );
        assert_eq!(rfc3339_ms(Some("soon")), None);
        assert_eq!(rfc3339_ms(None), None);
    }
}
