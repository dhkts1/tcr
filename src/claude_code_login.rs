//! Read the login the `claude` CLI itself already holds on this machine.
//!
//! Claude Code keeps its OAuth credential in one JSON document with a
//! `claudeAiOauth` object in it. On macOS that document lives in the login
//! Keychain under the generic-password service `Claude Code-credentials`; on
//! Linux (and on a mac where the Keychain item is absent) the same JSON is a
//! file at `$HOME/.claude/.credentials.json`. Both are read here, Keychain
//! first, because a mac that has both should answer with the one the CLI
//! actually uses.
//!
//! Nothing in this module ever prints, logs or returns a token in an error
//! message. The parse takes a `&str` rather than doing its own I/O so the unit
//! tests below feed it fake JSON and never touch the real credential store.
//!
//! # Why the env override exists
//!
//! [`CREDENTIALS_PATH_ENV`] names a file to read *instead of* everything above
//! — Keychain included. Two callers need it: a user whose credential file is
//! not under `$HOME`, and this crate's own tests, which spawn the real `tcr`
//! binary with a scratch `HOME`. A scratch `HOME` does not scratch the
//! Keychain: without this override, a test on a developer's mac would read
//! that developer's REAL Claude Code login and import it into the temp config
//! under test. Pointing the variable at a path that does not exist is how a
//! test says "this machine has no Claude Code login".

use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
use crate::log_err::LogErr as _;
use anyhow::Context;
use serde::Deserialize;

/// Read this file instead of the Keychain and instead of the default path.
/// A value naming a file that does not exist means "no login on this machine".
pub const CREDENTIALS_PATH_ENV: &str = "TCR_CLAUDE_CODE_CREDENTIALS";

/// The login Keychain's generic-password service Claude Code stores under.
pub const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// How long to wait for `security find-generic-password` before giving up.
///
/// `security` can block on a GUI authorization dialog when this binary is not
/// on the Keychain item's ACL. A dialog the user ignores must not wedge
/// `tcr status` (TcrBar polls it), so the read is bounded and a timeout is
/// simply "no login found" — the same outcome as denying the dialog.
#[cfg(target_os = "macos")]
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The Claude Code login, as much of it as tcr can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeLogin {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Epoch **milliseconds**, the same unit as `config::Account::expires_at`.
    pub expires_at: Option<i64>,
    /// `max`, `pro`, … — shown back to the user on import, never acted on.
    pub subscription_type: Option<String>,
}

#[derive(Deserialize)]
struct CredentialsDocument {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthSection>,
}

#[derive(Deserialize)]
struct OauthSection {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// Parse one Claude Code credentials document.
///
/// `Ok(None)` means the document is well-formed but carries no usable login:
/// no `claudeAiOauth` object at all (a Claude Code install that has only ever
/// done MCP OAuth has exactly this shape), or one with no access token. That
/// is a *state*, not a failure, and it is the case where `tcr` falls back to
/// telling the user to run `tcr login`.
///
/// `Err` is reserved for a document that exists and cannot be read as one:
/// invalid JSON, or a field of the wrong type. The error text names the
/// problem and never quotes the document, which holds tokens.
pub fn parse(json: &str) -> anyhow::Result<Option<ClaudeCodeLogin>> {
    let document: CredentialsDocument = serde_json::from_str(json)
        .map_err(|err| anyhow::anyhow!("not a Claude Code credentials document: {err}"))?;
    let Some(oauth) = document.claude_ai_oauth else {
        return Ok(None);
    };
    let Some(access_token) = oauth.access_token.filter(|t| !t.trim().is_empty()) else {
        return Ok(None);
    };
    Ok(Some(ClaudeCodeLogin {
        access_token,
        refresh_token: oauth.refresh_token.filter(|t| !t.trim().is_empty()),
        expires_at: oauth.expires_at,
        subscription_type: oauth.subscription_type,
    }))
}

/// The machine's Claude Code login, or `Ok(None)` when there is none.
///
/// Order: [`CREDENTIALS_PATH_ENV`] when set (and then nothing else), else the
/// Keychain on macOS, else `$HOME/.claude/.credentials.json`.
pub fn read_claude_code_login() -> anyhow::Result<Option<ClaudeCodeLogin>> {
    if let Some(path) = override_path() {
        return read_file(&path);
    }
    #[cfg(target_os = "macos")]
    match read_keychain_item(KEYCHAIN_SERVICE, None) {
        Ok(Some(json)) => {
            return parse(&json).context("reading the Claude Code login from the login Keychain");
        }
        Err(KeychainError::TimedOut) => eprintln!(
            "[tcr] warning: reading the Claude Code login from the Keychain took longer than {}s \
             (an unanswered authorization dialog?), continuing without it",
            KEYCHAIN_TIMEOUT.as_secs()
        ),
        Ok(None) | Err(KeychainError::Off | KeychainError::Refused) => {}
    }
    match default_file_path() {
        Some(path) => read_file(&path),
        None => Ok(None),
    }
}

fn override_path() -> Option<PathBuf> {
    std::env::var_os(CREDENTIALS_PATH_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `$HOME/.claude/.credentials.json`. `None` when `HOME` is unset, which is
/// the same answer as "there is no file there".
fn default_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".claude")
                .join(".credentials.json")
        })
}

fn read_file(path: &Path) -> anyhow::Result<Option<ClaudeCodeLogin>> {
    match std::fs::read_to_string(path) {
        Ok(json) => parse(&json).with_context(|| format!("reading {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Set to any non-empty value, this turns every login Keychain read in the
/// process off: [`read_keychain_item`] answers [`KeychainError::Off`] without
/// spawning `security`. The tests set it on every binary they spawn
/// (`tests/support/tcr_env.rs`), because a scratch `HOME` does not scratch the
/// Keychain, and `tcr import` reads claude-swap's items from it.
pub const NO_KEYCHAIN_ENV: &str = "TCR_NO_KEYCHAIN";

/// Why [`read_keychain_item`] read nothing. An item that is not there is not
/// one of these: that is `Ok(None)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeychainError {
    /// [`NO_KEYCHAIN_ENV`] is set.
    Off,
    /// `security` could not be run, or exited non-zero for a reason other than
    /// "no such item" (a denied authorization dialog, a locked Keychain).
    Refused,
    /// `security` was still waiting past [`KEYCHAIN_TIMEOUT`], most likely on
    /// an authorization dialog nobody answered. It was killed.
    TimedOut,
}

/// Whether [`NO_KEYCHAIN_ENV`] turns Keychain reads off in this process.
pub fn keychain_is_off() -> bool {
    std::env::var_os(NO_KEYCHAIN_ENV).is_some_and(|value| !value.is_empty())
}

/// `security find-generic-password` exits with this when no item matches.
#[cfg(target_os = "macos")]
const SECURITY_NOT_FOUND: i32 = 44;

/// The secret of the login Keychain's generic-password item under `service`
/// (and `account`, when given), trimmed: `Ok(None)` when there is no such item
/// or it is empty, `Err` naming why nothing could be read.
///
/// Bounded by [`KEYCHAIN_TIMEOUT`]: `security` blocks on a GUI authorization
/// dialog when this binary is not on the item's access list, and a dialog the
/// user ignores must not wedge `tcr status` (TcrBar polls it). Prints nothing
/// and never retries; each caller decides what a failure means to it. The
/// secret is never logged: `security`'s stderr is discarded, its stdout is
/// the return value.
#[cfg(target_os = "macos")]
pub fn read_keychain_item(
    service: &str,
    account: Option<&str>,
) -> Result<Option<String>, KeychainError> {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    if keychain_is_off() {
        return Err(KeychainError::Off);
    }
    let mut command = Command::new("security");
    command.args(["find-generic-password", "-s", service]);
    if let Some(account) = account {
        command.args(["-a", account]);
    }
    let mut child = command
        .arg("-w")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| KeychainError::Refused)?;

    let deadline = Instant::now() + KEYCHAIN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) if status.code() == Some(SECURITY_NOT_FOUND) => return Ok(None),
            Ok(Some(_)) | Err(_) => return Err(KeychainError::Refused),
            Ok(None) => {
                if Instant::now() >= deadline {
                    child
                        .kill()
                        .log_debug("killing the timed-out keychain reader");
                    child
                        .wait()
                        .log_debug("reaping the timed-out keychain reader");
                    return Err(KeychainError::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|_| KeychainError::Refused)?;
    let secret = String::from_utf8(output.stdout).map_err(|_| KeychainError::Refused)?;
    let secret = secret.trim();
    Ok((!secret.is_empty()).then(|| secret.to_string()))
}

/// There is no login Keychain off macOS: every read is "no such item", or
/// [`KeychainError::Off`] when [`NO_KEYCHAIN_ENV`] says so.
#[cfg(not(target_os = "macos"))]
pub fn read_keychain_item(
    _service: &str,
    _account: Option<&str>,
) -> Result<Option<String>, KeychainError> {
    if keychain_is_off() {
        return Err(KeychainError::Off);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full shape measured on a real machine, with fake values.
    const FULL: &str = r#"{
      "mcpOAuth": {"some-server": {"accessToken": "mcp-fake"}},
      "claudeAiOauth": {
        "accessToken": "sk-ant-oat-FAKE-ACCESS",
        "refreshToken": "sk-ant-ort-FAKE-REFRESH",
        "expiresAt": 1789000000000,
        "refreshTokenExpiresAt": 1791000000000,
        "scopes": ["user:inference", "user:profile"],
        "subscriptionType": "max",
        "rateLimitTier": "default"
      }
    }"#;

    #[test]
    fn parses_the_full_shape() {
        let login = parse(FULL)
            .expect("the full shape parses")
            .expect("the full shape carries a login");
        assert_eq!(login.access_token, "sk-ant-oat-FAKE-ACCESS");
        assert_eq!(
            login.refresh_token.as_deref(),
            Some("sk-ant-ort-FAKE-REFRESH")
        );
        assert_eq!(login.expires_at, Some(1_789_000_000_000));
        assert_eq!(login.subscription_type.as_deref(), Some("max"));
    }

    #[test]
    fn expires_at_is_read_as_milliseconds_unchanged() {
        // The same unit as `config::Account::expires_at`: no conversion here,
        // and none wanted — a value in the 1.7e12 range is already epoch-ms.
        let login = parse(FULL).expect("parses").expect("has a login");
        assert_eq!(
            crate::oauth::normalize_expires_at(login.expires_at.expect("expiresAt is present")),
            1_789_000_000_000,
            "an epoch-ms expiry must survive the config's own normalization unchanged"
        );
    }

    #[test]
    fn no_claude_ai_oauth_is_no_login_rather_than_an_error() {
        let only_mcp = r#"{"mcpOAuth": {"server": {"accessToken": "mcp-fake"}}}"#;
        assert_eq!(
            parse(only_mcp).expect("a document with no claudeAiOauth is not a failure"),
            None
        );
    }

    #[test]
    fn an_empty_access_token_is_no_login() {
        assert_eq!(
            parse(r#"{"claudeAiOauth": {"accessToken": ""}}"#).expect("well-formed"),
            None
        );
    }

    #[test]
    fn a_missing_refresh_token_still_yields_a_login() {
        let no_refresh =
            r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat-FAKE", "expiresAt": 1789000000000}}"#;
        let login = parse(no_refresh).expect("parses").expect("has a login");
        assert_eq!(login.refresh_token, None);
        assert_eq!(login.expires_at, Some(1_789_000_000_000));
        assert_eq!(login.subscription_type, None);
    }

    #[test]
    fn malformed_json_is_an_error_and_never_quotes_the_document() {
        let err = parse(r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat-FAKE-SECRET",}"#)
            .expect_err("invalid JSON must be an error");
        let text = format!("{err:#}");
        assert!(
            text.contains("not a Claude Code credentials document"),
            "the error must name the problem, got: {text}"
        );
        assert!(
            !text.contains("sk-ant-oat-FAKE-SECRET"),
            "an error message must never carry the document's token material"
        );
    }

    #[test]
    fn a_wrongly_typed_expiry_is_an_error() {
        parse(r#"{"claudeAiOauth": {"accessToken": "x", "expiresAt": "soon"}}"#)
            .expect_err("a string where an epoch-ms number belongs must not parse");
    }

    #[test]
    fn a_missing_file_is_no_login_and_a_present_one_parses() {
        let dir = tempfile::tempdir().expect("a scratch dir");
        let missing = dir.path().join("nope.json");
        assert_eq!(
            read_file(&missing).expect("a missing credentials file is not a failure"),
            None,
            "a path that does not exist means this machine has no Claude Code login"
        );

        let present = dir.path().join(".credentials.json");
        std::fs::write(&present, FULL).expect("writing the fake credentials file");
        let login = read_file(&present)
            .expect("a present file parses")
            .expect("it carries a login");
        assert_eq!(login.subscription_type.as_deref(), Some("max"));
    }
}
