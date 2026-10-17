//! OAuth token refresh, ported faithfully from `teamclaude/src/oauth.js`.
//!
//! The request shape (endpoint, client id, JSON body, headers) matches the JS
//! reference exactly so it stays a drop-in against the same Anthropic OAuth
//! backend. The one behaviour that matters downstream is the error split: a
//! `400/401/403` means the refresh token itself is dead (the account must drop
//! out of rotation until re-login), whereas a `5xx`/network failure is transient
//! (keep the current token and fail this one request over to another account).
//! A Codex account refreshes against `auth.openai.com` instead
//! ([`crate::codex_oauth`], dispatched by [`LiveRefresher`]). Every Codex `401` and the
//! `refresh_token_expired`, `refresh_token_reused` and `refresh_token_invalidated` codes at
//! any error status are permanent, as is `invalid_grant` on a `400`. Other failures are
//! transient.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Anthropic OAuth token endpoint (from the JS `DEFAULT_TOKEN_ENDPOINT`).
pub const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code OAuth client id (from the JS `DEFAULT_CLIENT_ID`).
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Threshold used by [`is_expiring_soon`]: refresh proactively 5 minutes out.
pub const EXPIRING_SOON_MS: i64 = 5 * 60 * 1000;

const MAX_RETRIES: u32 = 2;
const BASE_DELAY_MS: u64 = 500;
/// The bound on one refresh POST, whichever provider's endpoint it goes to: the manager holds an
/// account's refresh lock across the call, so a POST that never answered would stall every
/// request on that account. `pub(crate)` so [`crate::codex_oauth`] applies the same bound.
pub(crate) const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Freshly-minted OAuth credentials returned by a refresh.
#[derive(Debug, Clone)]
pub struct Tokens {
    /// A refreshed Codex id token, when returned: carried to the imported auth.json so Codex
    /// reads current plan/workspace claims. Other OAuth producers leave it absent.
    pub id_token: Option<String>,
    pub access_token: String,
    /// `None` only for a `claude setup-token` credential ([`login_with_token`]):
    /// that mint carries only the `user:inference` scope, and the CLI that
    /// produces it never surfaces a refresh token to the caller at all — so
    /// there is nothing to store. Every OTHER producer of a `Tokens` MUST
    /// yield `Some(...)`: [`tokens_from_exchange_body`] `bail!`s rather than
    /// return `None`, and [`refresh_access_token_at`] falls back to the
    /// refresh token it was just called with when the endpoint omits a
    /// rotated one. See that fallback's doc comment before touching either.
    /// The Codex refresher, [`crate::codex_oauth::refresh_access_token_at`], is
    /// bound by the same invariant and keeps the old token the same way, an
    /// empty rotated one included.
    pub refresh_token: Option<String>,
    /// Epoch **milliseconds** at which `access_token` expires.
    pub expires_at_ms: i64,
}

/// Why a refresh failed. The variant drives account state: [`OAuthError::AuthRejected`]
/// sidelines the account (dead refresh token), [`OAuthError::Transient`] does not.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// `code` is the OAuth error code that named the rejection, when the endpoint gave one tcr
    /// recognises (`refresh_token_reused`, `invalid_grant`, …), and only ever one of tcr's own
    /// static spellings, never the body's text. A Codex row keeps it as the reason its login is
    /// dead (`AccountRuntime::login_error`); the Anthropic refresher never sets it.
    #[error("token refresh rejected by auth (HTTP {status}) — re-login needed")]
    AuthRejected {
        status: u16,
        code: Option<&'static str>,
    },
    #[error("token refresh failed transiently: {0}")]
    Transient(String),
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Normalize an `expires_at` to milliseconds. OAuth backends may report seconds;
/// Claude Code credentials use milliseconds. Values below ~`1e12` are seconds
/// (that boundary is year 2001 in ms, far in the future in seconds).
pub fn normalize_expires_at(value: i64) -> i64 {
    if value < 1_000_000_000_000 {
        value * 1000
    } else {
        value
    }
}

/// Absolute expiry (ms) from `now_ms` and an optional `expires_in` (seconds,
/// default 3600). Saturating so a hostile/huge `expires_in` can never overflow
/// the i64 multiply into a negative expiry (which would make [`is_expired`]
/// always true → a single-use-refresh-token storm).
fn expires_at_from(now_ms: i64, expires_in: Option<i64>) -> i64 {
    now_ms.saturating_add(expires_in.unwrap_or(3600).saturating_mul(1000))
}

/// Has the token already expired at `now_ms`? A `None` expiry is treated as
/// non-expiring (nothing to refresh).
pub fn is_expired(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match expires_at_ms {
        Some(exp) => now_ms >= exp,
        None => false,
    }
}

/// Is the token within [`EXPIRING_SOON_MS`] of expiry at `now_ms`?
pub fn is_expiring_soon(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match expires_at_ms {
        Some(exp) => now_ms + EXPIRING_SOON_MS >= exp,
        None => false,
    }
}

/// `Accept` header the token endpoint expects from a "recognised" client.
/// Shared between [`refresh_access_token_at`], [`exchange_code`] and
/// `mint::exchange_mint_code` — see [`OAUTH_USER_AGENT`] for why sending it
/// matters. `pub(crate)` rather than private so `mint.rs` sends the identical
/// value instead of a second copy that could drift.
pub(crate) const OAUTH_ACCEPT: &str = "application/json, text/plain, */*";

/// `User-Agent` header the token endpoint expects from a "recognised" client.
/// Without this header (and [`OAUTH_ACCEPT`]) an otherwise-valid exchange was
/// refused a bogus `{"type":"rate_limit_error"}` 21 times over 7 minutes; the
/// identical payload with these headers was evaluated on the first attempt —
/// measured against the live endpoint, `docs/design/long-lived-tokens.md`
/// ("The endpoint 429s clients it does not recognise"). `pub(crate)` for the
/// same reason as [`OAUTH_ACCEPT`]: `mint.rs`'s exchange hits the same
/// endpoint and must never fall behind this value.
pub(crate) const OAUTH_USER_AGENT: &str = "axios/1.13.6";

/// Refresh an access token against [`TOKEN_ENDPOINT`], retrying `5xx`/network
/// failures with exponential backoff. Auth rejections are returned immediately.
///
/// Always returns `Some(...)` in [`Tokens::refresh_token`] — see the fallback
/// inside [`refresh_access_token_at`] and that field's doc comment. This is a
/// load-bearing invariant: [`crate::manager::Manager::apply_refresh`] and
/// [`crate::manager::Manager::persist_tokens`] write this value straight onto
/// an existing account's row, so a stray `None` here would silently make a
/// healthy account unrefreshable on its very next rotation.
pub async fn refresh_access_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<Tokens, OAuthError> {
    refresh_access_token_at(client, refresh_token, TOKEN_ENDPOINT).await
}

/// [`refresh_access_token`] against an explicit endpoint (for tests).
pub async fn refresh_access_token_at(
    client: &reqwest::Client,
    refresh_token: &str,
    endpoint: &str,
) -> Result<Tokens, OAuthError> {
    let body = RefreshRequest {
        grant_type: "refresh_token",
        refresh_token,
        client_id: CLIENT_ID,
    };

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = BASE_DELAY_MS * (1u64 << (attempt - 1));
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        let send = client
            .post(endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", OAUTH_ACCEPT)
            .header("User-Agent", OAUTH_USER_AGENT)
            .timeout(REFRESH_TIMEOUT)
            .json(&body)
            .send()
            .await;

        let response = match send {
            Ok(r) => r,
            Err(err) => {
                // Network/timeout: retry while attempts remain, else transient.
                if attempt < MAX_RETRIES {
                    continue;
                }
                return Err(OAuthError::Transient(err.to_string()));
            }
        };

        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            // 5xx is retryable; exhausting retries leaves it transient (not auth).
            if status.is_server_error() && attempt < MAX_RETRIES {
                continue;
            }
            if matches!(code, 400 | 401 | 403) {
                return Err(OAuthError::AuthRejected {
                    status: code,
                    code: None,
                });
            }
            let detail = response.text().await.unwrap_or_default();
            return Err(OAuthError::Transient(format!("HTTP {code}: {detail}")));
        }

        let data: RefreshResponse = response
            .json()
            .await
            .map_err(|e| OAuthError::Transient(e.to_string()))?;

        let expires_at_ms = data.expires_at.map_or_else(
            || expires_at_from(crate::now_ms(), data.expires_in),
            normalize_expires_at,
        );

        return Ok(Tokens {
            id_token: None,
            access_token: data.access_token,
            refresh_token: Some(
                data.refresh_token
                    .unwrap_or_else(|| refresh_token.to_string()),
            ),
            expires_at_ms,
        });
    }

    Err(OAuthError::Transient("refresh retries exhausted".into()))
}

/// Future returned by [`TokenRefresher::refresh`]. `'static` so it can be awaited
/// after the manager's state lock is released.
pub type RefreshFuture = Pin<Box<dyn Future<Output = Result<Tokens, OAuthError>> + Send>>;

/// Abstraction over "turn a refresh token into fresh tokens", so the manager can
/// be exercised in tests without hitting the network.
///
/// `provider` is the provider of the account the token belongs to: a refresh
/// token is only good at the endpoint that issued it, so the live refresher
/// sends each one to its own provider's.
pub trait TokenRefresher: Send + Sync {
    fn refresh(&self, provider: crate::provider::Provider, refresh_token: String) -> RefreshFuture;
}

/// The production refresher: a real HTTPS call to the OAuth endpoint of the
/// account's provider — Anthropic's, or Codex's ([`crate::codex_oauth`]).
pub struct LiveRefresher {
    client: reqwest::Client,
    endpoint: String,
    /// Where Codex refreshes go: [`crate::codex_oauth::TOKEN_ENDPOINT`] unless a
    /// test pointed it elsewhere with [`Self::with_codex_endpoint`].
    codex_endpoint: String,
}

impl LiveRefresher {
    pub fn new() -> Self {
        Self {
            // A dedicated client: the upstream proxy client must NOT carry a
            // total timeout (streams run long), but a refresh must not hang.
            //
            // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
            // proxy — routing our own upstream through an ambient proxy (e.g. a
            // teamclaude on :3456) loops us through the very thing we replace and fails
            // as "upstream unreachable". Always reach the token endpoint directly,
            // Anthropic's and Codex's alike.
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build reqwest client"),
            endpoint: TOKEN_ENDPOINT.to_string(),
            codex_endpoint: crate::codex_oauth::TOKEN_ENDPOINT.to_string(),
        }
    }

    /// Send Codex refreshes to `endpoint` instead of
    /// [`crate::codex_oauth::TOKEN_ENDPOINT`]. A test seam: a stub endpoint is
    /// the only way to watch a refresh without spending a real single-use token.
    pub fn with_codex_endpoint(mut self, endpoint: String) -> Self {
        self.codex_endpoint = endpoint;
        self
    }
}

impl Default for LiveRefresher {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenRefresher for LiveRefresher {
    fn refresh(&self, provider: crate::provider::Provider, refresh_token: String) -> RefreshFuture {
        let client = self.client.clone();
        match provider {
            crate::provider::Provider::Anthropic => {
                let endpoint = self.endpoint.clone();
                Box::pin(async move {
                    refresh_access_token_at(&client, &refresh_token, &endpoint).await
                })
            }
            crate::provider::Provider::Codex => {
                let endpoint = self.codex_endpoint.clone();
                Box::pin(async move {
                    crate::codex_oauth::refresh_access_token_at(&client, &refresh_token, &endpoint)
                        .await
                })
            }
        }
    }
}

/// The error a [`NoRefresh`] attempt fails with, surfaced when an offline snapshot
/// (`tcr status`, `tcr accounts --probe`) meets an already-expired token.
pub const NO_REFRESH_MESSAGE: &str = "offline snapshot never refreshes — token expired; check the running server's TUI or re-run after tcr login";

/// [`NO_REFRESH_MESSAGE`]'s twin for a Codex account, whose remedy is a different command:
/// `tcr login` signs in Claude accounts only. `<name>` is written literally, because the
/// message is built before anyone knows which row it is about. It reaches a tracing event only,
/// never a person, so offline `tcr status` shows such a row as `probe=error`.
pub const NO_REFRESH_CODEX_MESSAGE: &str = "offline snapshot never refreshes — token expired; check the running server's TUI or re-run after `tcr login --codex --account <name>`";

/// A refresher that NEVER contacts the OAuth token endpoint: every attempt fails
/// with [`NO_REFRESH_MESSAGE`] (a *transient* error, so it never falsely sidelines
/// the credential as dead). Injected by [`crate::cli::snapshot_offline`] so a
/// second process (`tcr status`) can never perform a real OAuth refresh — which,
/// because refresh tokens are SINGLE-USE, would rotate and thereby REVOKE the copy
/// the running server still holds, killing that account (observed live 2026-07-19).
/// Accounts with a still-valid access token probe normally (no refresh is
/// attempted); expired ones surface a visible probe error instead of refreshing.
pub struct NoRefresh;

impl TokenRefresher for NoRefresh {
    fn refresh(
        &self,
        provider: crate::provider::Provider,
        _refresh_token: String,
    ) -> RefreshFuture {
        let message = match provider {
            crate::provider::Provider::Anthropic => NO_REFRESH_MESSAGE,
            crate::provider::Provider::Codex => NO_REFRESH_CODEX_MESSAGE,
        };
        Box::pin(async move { Err(OAuthError::Transient(message.to_string())) })
    }
}

// ===========================================================================
// Browser OAuth login (PKCE), ported from `teamclaude/src/oauth.js`
// (`loginOAuth` / `startCallbackServer` / `raceWithStdinCode` / `openBrowser` /
// `fetchProfile`). Adds a new account to the drop-in config so friends can
// onboard without hand-editing JSON.
// ===========================================================================

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope, TokenUrl};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;

use crate::config::{self, Account, Config};
use crate::singleton;

/// Authorize endpoint (from the JS `OAUTH_AUTHORIZE`).
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// Profile endpoint used to name the account (from the JS `PROFILE_URL`).
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// OAuth scopes requested at login. Narrowed from the JS reference's six
/// scopes to the three the token endpoint will grant a custom `expires_in`
/// for. `org:create_api_key`, `user:sessions:claude_code` and
/// `user:mcp_servers` are dropped: requesting any of them alongside a
/// custom `expires_in` gets HTTP 400 `Custom expires_in not allowed for
/// scope 'user:mcp_servers'` — measured against the live endpoint,
/// `docs/design/long-lived-tokens.md` ("The finding" / "A reduced scope set
/// loses nothing tcr uses"). `user:profile` is kept because `rate_limit_tier`
/// is not in the token response, so `fetch_profile` still needs it.
pub const OAUTH_SCOPES: &str = "user:inference user:profile user:file_upload";

/// Overall login timeout matching the JS 2-minute callback-server deadline.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(120);

/// Who is on the other end of `tcr login`.
///
/// Every interactive thing the login does — reading stdin, prompting for a
/// name, opening a browser, narrating on stdout — is decided by this one
/// value rather than by four scattered `is_terminal()` probes, so a caller
/// cannot end up half-interactive: [`LoginUi::Machine`] is exactly the set of
/// behaviours a GUI parent (TcrBar's `LoginSession`) can drive, and there is
/// no way to switch one of them on without the others.
///
/// The browser is deliberately NOT opened in [`LoginUi::Machine`]: the parent
/// opens it, because only the parent can bring its own window forward
/// afterwards. A CLI `open` from a background child leaves the browser behind
/// whatever had focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginUi {
    /// A human at a terminal. Prose progress on stdout, `read_stdin_code` and
    /// `prompt_account_name` live, browser opened here.
    Terminal,
    /// A parent process. One JSON object per line on stdout
    /// ([`LoginEvent::line`]), stdin never read, browser left to the caller.
    Machine,
}

/// Whether the human-prose lines this module prints on the way to a finished
/// login are wanted at all.
///
/// Ambient rather than threaded, deliberately and narrowly: the three prose
/// lines that survive deep inside the persist path
/// ([`finish_login_account`], [`persist_via_file`], [`account_write_result`]) would
/// otherwise need a display mode threaded through six function signatures and
/// twenty test call sites to silence one `println!` each. Nothing about
/// BEHAVIOUR reads this — stdin, the browser and the name prompt are all
/// decided by an explicit [`LoginUi`] argument — so the worst a wrong value
/// can do is print or omit a sentence. One process runs one login, which is
/// what makes a process-wide answer the true one here.
static PROSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Print one human line, unless [`PROSE`] is off (machine mode), in which case
/// stdout stays one JSON object per line and nothing else.
fn prose(message: std::fmt::Arguments<'_>) {
    if PROSE.load(std::sync::atomic::Ordering::Relaxed) {
        println!("{message}");
    }
}

impl LoginUi {
    /// Point [`PROSE`] at this mode for the rest of the process. Called once,
    /// at the top of [`login`] and of every other login entry point.
    pub(crate) fn install(self) {
        PROSE.store(
            self == LoginUi::Terminal,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// True when stdin is a completion path and a prompt has somewhere to go.
    fn reads_stdin(self) -> bool {
        self == LoginUi::Terminal
    }

    /// Emit one progress event, or nothing at all in [`LoginUi::Terminal`]
    /// (which narrates in prose instead — see [`LoginUi::say`]).
    pub(crate) fn emit(self, event: LoginEvent<'_>) {
        if self == LoginUi::Machine {
            println!("{}", event.line());
        }
    }

    /// Print one line of human prose, suppressed in [`LoginUi::Machine`] so
    /// the machine stdout stays one-JSON-object-per-line with nothing else in
    /// it. Goes through [`prose`] rather than `println!` so this mode and the
    /// deep persist-path lines can never disagree about what silence means.
    fn say(self, message: &str) {
        if self == LoginUi::Terminal {
            prose(format_args!("{message}"));
        }
    }
}

/// A `tcr login --non-interactive` progress line.
///
/// The wire format is the contract `apps/macos/Sources/TcrBarCore/LoginSession.swift`
/// parses, so it is a type with one rendering function rather than four
/// `println!("{{\"event\":…")` call sites that can drift from the parser one
/// at a time.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginEvent<'a> {
    /// Open this URL. Nothing has been read or written yet.
    Browser { url: &'a str },
    /// The authorize URL is out; waiting for the loopback callback.
    Waiting,
    /// The account is written. `account` is the name it was written under.
    Saved { account: &'a str },
    /// The login failed, and this is the whole reason, on one line.
    Error { reason: &'a str },
    /// The authorize URL, with the browser already open on it: codex opened it itself, so the
    /// caller opens nothing. Never written with `opened: false`; a URL the caller must open is
    /// [`LoginEvent::Browser`].
    BrowserOpened { url: &'a str },
    /// A sentence for the person signing in, shown verbatim, one paragraph each; never a
    /// failure. Every notice comes before `saved` or `error`.
    Notice { text: &'a str },
    /// A Codex account is written: the name it was written under, and its plan as `tcr status`
    /// names it (`ChatGPT Pro`). No `plan` key when the plan is unknown. A Claude save stays
    /// [`LoginEvent::Saved`], whose line carries neither key.
    SavedCodex {
        account: &'a str,
        plan: Option<&'a str>,
    },
    /// A refusal the caller titles by its code ([`LoginRefusal`]); the reason is carried
    /// verbatim. A failure with no code stays [`LoginEvent::Error`].
    Refused { code: &'a str, reason: &'a str },
}

impl LoginEvent<'_> {
    /// One JSON object, no newline. `serde_json` does the escaping — a URL
    /// carries `&` and `=` and a failure reason carries quotes and newlines,
    /// and hand-built JSON is how a reason containing a `"` becomes an
    /// unparseable line at the worst possible moment.
    pub fn line(&self) -> String {
        let value = match self {
            LoginEvent::Browser { url } => {
                serde_json::json!({ "event": "browser", "url": url })
            }
            LoginEvent::Waiting => serde_json::json!({ "event": "waiting" }),
            LoginEvent::Saved { account } => {
                serde_json::json!({ "event": "saved", "account": account })
            }
            LoginEvent::Error { reason } => {
                serde_json::json!({ "event": "error", "reason": reason })
            }
            LoginEvent::BrowserOpened { url } => {
                serde_json::json!({ "event": "browser", "opened": true, "url": url })
            }
            LoginEvent::Notice { text } => serde_json::json!({ "event": "notice", "text": text }),
            LoginEvent::SavedCodex { account, plan } => {
                let mut value = serde_json::json!({
                    "account": account,
                    "event": "saved",
                    "provider": crate::provider::Provider::Codex.as_str(),
                });
                if let Some(plan) = plan {
                    value["plan"] = serde_json::Value::from(*plan);
                }
                value
            }
            LoginEvent::Refused { code, reason } => {
                serde_json::json!({ "code": code, "event": "error", "reason": reason })
            }
        };
        value.to_string()
    }
}

/// A login refusal a caller can title by its code (`codex-identity`, `account-is-codex`, …):
/// the `--non-interactive` error event carries `code` and this reason, verbatim and without the
/// context chain, so a GUI shows tcr's words and decides nothing from them
/// ([`machine_error_line`]). Its `Display` is the reason alone, so a refusal reads the same in
/// the terminal as it did before it carried a code.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct LoginRefusal {
    pub code: &'static str,
    pub reason: String,
}

impl LoginRefusal {
    /// The login belongs to a FedRAMP ChatGPT workspace, which tcr does not pool.
    pub const CODEX_FEDRAMP: &'static str = "codex-fedramp";
    /// The workspace check placed the login's workspace somewhere other than chatgpt.com.
    pub const CODEX_ROUTED: &'static str = "codex-routed";
    /// The workspace check could not complete, so the workspace is unconfirmed.
    pub const CODEX_UNCONFIRMED: &'static str = "codex-unconfirmed";
    /// No `codex` executable was found, or a variable naming one names none.
    pub const CODEX_MISSING: &'static str = "codex-missing";
    /// The ChatGPT login that came back is not the row `--account` named.
    pub const CODEX_IDENTITY: &'static str = "codex-identity";
    /// `codex login` ended without leaving a login behind.
    pub const CODEX_LOGIN_FAILED: &'static str = "codex-login-failed";
    /// `tcr login --account` named a Codex row, which only a Codex login can sign in.
    pub const ACCOUNT_IS_CODEX: &'static str = "account-is-codex";
    /// `--codex` or `--from-codex` with `--account` named a Claude row.
    pub const ACCOUNT_IS_CLAUDE: &'static str = "account-is-claude";

    /// The `--non-interactive` error line this refusal is.
    pub fn line(&self) -> String {
        LoginEvent::Refused {
            code: self.code,
            reason: &self.reason,
        }
        .line()
    }
}

/// The `--non-interactive` error line for `error`, and the one-line reason the same failure
/// prints on stderr.
///
/// A [`LoginRefusal`] anywhere in the chain gives its coded line, with its own reason and none of
/// the context a caller wrapped it in: the reason is tcr's whole sentence, written to be shown
/// as it is. Anything else gives the uncoded line it always gave, its reason flattened from the
/// whole chain by [`one_line_reason`].
pub fn machine_error_line(error: &anyhow::Error) -> (String, String) {
    match error
        .chain()
        .find_map(|cause| cause.downcast_ref::<LoginRefusal>())
    {
        Some(refusal) => {
            let reason = refusal
                .reason
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let line = LoginEvent::Refused {
                code: refusal.code,
                reason: &reason,
            }
            .line();
            (line, reason)
        }
        None => {
            let reason = one_line_reason(error);
            let line = LoginEvent::Error { reason: &reason }.line();
            (line, reason)
        }
    }
}

/// Printed once a Codex account is saved into a config that held no Codex row before, in any
/// state, enabled, disabled or needing a re-login. The account enables marked Codex connections
/// to join the pool (`mitm::handle_connect`); unmarked clients keep their own TLS and login.
pub const CODEX_FIRST_ACCOUNT_NOTICE: &str = "tcr pools marked Codex connections to chatgpt.com: use `tcr run --codex`; keep \
    the marker scoped to that launch, since unmarked connections keep their own login";

/// The warning for a `tcr` at `path`, reporting `version`, that is 0.2.45 or older: it predates
/// the `credentialVersion` check, so it would read a Codex row as a Claude account and send its
/// OpenAI token to Anthropic the next time it starts on this config.
pub fn older_tcr_reads_codex_as_claude_notice(path: &Path, version: &str) -> String {
    format!(
        "the tcr at {} ({version}) predates Codex accounts: update it before it next starts — it \
         would read this login as a Claude account and send its token to Anthropic",
        path.display()
    )
}

/// The warning for a `tcr` at `path`, reporting `version`, from 0.2.46 on that still lacks
/// `--from-codex`: it checks `credentialVersion`, so it refuses a config holding a Codex row
/// outright, and every command it runs on this config fails until it is updated.
pub fn older_tcr_refuses_config_notice(path: &Path, version: &str) -> String {
    format!(
        "the tcr at {} ({version}) predates Codex accounts: update it — until then it refuses \
         this config, so every command it runs fails",
        path.display()
    )
}

/// The notice for a codex with no login of its own at `path` (an existence check, never a
/// read): tcr re-credentials a pooled codex's requests, but codex itself still has to start
/// signed in. A login kept in the system keyring (codex's `keyring` and `auto` stores) is
/// invisible to tcr, which is why the sentence leaves that case open.
pub fn codex_has_no_login_notice(path: &Path) -> String {
    format!(
        "codex needs a login of its own to start, even when pooled, and none is at {}: unless \
         codex keeps its login in the keyring, run `codex login` once",
        path.display()
    )
}

/// The line `tcr login --from-codex` prints after its import lines in the terminal: how to use
/// the account it just saved.
pub const CODEX_IMPORT_RUN_LINE: &str = "[tcr] start a pooled session with `tcr run --codex`";

/// The command each login entry point names when it tells someone to run it again.
pub const LOGIN_COMMAND: &str = "tcr login";
/// See [`LOGIN_COMMAND`].
pub const LOGIN_FROM_CODEX_COMMAND: &str = "tcr login --from-codex";
/// See [`LOGIN_COMMAND`].
pub const LOGIN_FROM_CLAUDE_CODE_COMMAND: &str = "tcr login --from-claude-code";
/// See [`LOGIN_COMMAND`].
pub const LOGIN_CODEX_COMMAND: &str = "tcr login --codex";

/// Flatten an error and its whole `anyhow` context chain onto ONE line.
///
/// `{:#}` already does this, but a message can still carry an embedded
/// newline (a `bail!` with a wrapped multi-line explanation — this module has
/// several), and one physical line is what both the JSON event stream and a
/// GUI's single-line failure label need. Whitespace runs collapse to a single
/// space rather than being dropped, so words never fuse together.
pub fn one_line_reason(error: &anyhow::Error) -> String {
    format!("{error:#}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// PKCE + CSRF material for one login attempt, produced by the `oauth2` crate.
/// Holds the plaintext `verifier` and `state` secrets we replay at the manual
/// token exchange, plus the fully-built authorize URL.
struct LoginFlow {
    /// The PKCE `code_verifier` secret, replayed at token exchange.
    verifier: String,
    /// The anti-CSRF `state` secret, validated on the callback.
    state: String,
    /// The authorize URL to open in the browser.
    auth_url: String,
}

/// Build the authorize URL, PKCE challenge and CSRF state via the standard
/// `oauth2` client. PKCE (S256), `state`, and standard params (`response_type`,
/// `client_id`, `redirect_uri`, `scope`, `code_challenge*`) are all produced by
/// the library; only Claude's non-standard `code=true` is added by hand.
///
/// `login_hint`, when present, is passed through to `claude.ai/oauth/authorize`
/// so the login page pre-selects that address — **ergonomics only**, never the
/// source of truth. Measured 2026-08-14 (σ3, N=1, clean incognito session): the
/// endpoint honors it and pre-fills the exact address passed. Untested (σ1):
/// whether it overrides an *already signed-in* browser session — precisely the
/// case `open_browser` produces by handing the URL to the default browser.
/// Because that case is unverified, the caller must never rely on the hint for
/// correctness; the identity assertion after `fetch_profile` returns is what
/// actually enforces the requested account, and depends on nothing external.
fn build_login_flow(redirect_uri: &str, login_hint: Option<&str>) -> anyhow::Result<LoginFlow> {
    let client = BasicClient::new(ClientId::new(CLIENT_ID.to_string()))
        .set_auth_uri(AuthUrl::new(AUTHORIZE_URL.to_string()).context("invalid authorize URL")?)
        .set_token_uri(TokenUrl::new(TOKEN_ENDPOINT.to_string()).context("invalid token URL")?)
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.to_string()).context("invalid redirect URI")?,
        );

    // oauth2 generates a random verifier and its S256 challenge together.
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();

    let mut request = client
        // 32-byte state, NOT the crate-default `new_random()` (16 bytes / 22
        // chars): Claude's authorize endpoint rejects the short state with
        // "Invalid request format" once a logged-in session validates the
        // request. 32 bytes (43 base64url chars) matches the working JS
        // reference (`randomBytes(32).toString('base64url')`).
        .authorize_url(|| CsrfToken::new_random_len(32))
        .add_scope(Scope::new(OAUTH_SCOPES.to_string()))
        .set_pkce_challenge(challenge)
        // Claude's authorize endpoint requires this non-standard flag.
        .add_extra_param("code", "true");
    if let Some(email) = login_hint {
        request = request.add_extra_param("login_hint", email);
    }
    let (auth_url, csrf) = request.url();

    Ok(LoginFlow {
        verifier: verifier.secret().clone(),
        state: csrf.secret().clone(),
        auth_url: auth_url.to_string(),
    })
}

/// Extract `(code, state, error)` from a URL query string (`k=v&k2=v2`).
///
/// Decoding is [`form_urlencoded`]'s — the same parser `url`, `axum` and
/// `reqwest` already pull into this tree, so it costs no package — rather than
/// the hand-rolled `%XX`/`+` decoder this replaces. It is lossy on invalid
/// UTF-8, so a hostile callback still cannot panic the CLI.
///
/// The explicit loop is kept deliberately over a `collect()` into a map: it
/// preserves **last-occurrence-wins** on a duplicated key, which is what the
/// hand-rolled version did.
///
/// One disclosed behaviour change: `form_urlencoded` percent-decodes the KEY as
/// well as the value, so `%63ode=x` now matches `code` where it previously did
/// not. That is the more spec-correct reading and is unreachable from a real
/// Claude redirect. A differential run over 4,132 inputs (36 hand-picked edges
/// plus a 4,096-case brute-force sweep) found this as the ONLY divergence —
/// nothing changed for `;` separators, bare keys, `code=a=b`, lone surrogates,
/// `%00`, `%FF`, a trailing `%`, or `+`-vs-`%2B`.
fn parse_oauth_query(query: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut code, mut state, mut error) = (None, None, None);
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    (code, state, error)
}

/// Resolve a callback into the authorization code, enforcing OAuth security:
/// an `error` fails; `state` MUST be present and equal to ours (mismatch is a
/// CSRF signal and is rejected); a missing code fails.
fn resolve_callback_code(
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    expected_state: &str,
) -> anyhow::Result<String> {
    if let Some(err) = error {
        bail!("OAuth error: {err}");
    }
    if state.as_deref() != Some(expected_state) {
        bail!("OAuth state mismatch");
    }
    code.ok_or_else(|| anyhow!("callback missing authorization code"))
}

/// Open `url` in the default browser (best-effort; ignores failure like the JS).
fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "start"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}

/// Minimal local callback server on `listener` (already bound to 127.0.0.1).
/// Accepts connections until a valid `GET /callback?...` arrives, then replies
/// `302` to the success page and returns the code. A callback carrying an
/// `error` or a mismatched `state` fails immediately.
async fn run_callback_server(
    listener: TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;

        // Read one chunk — the request line + headers arrive together; we only
        // need the request line (`GET /callback?query HTTP/1.1`).
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));

        if path != "/callback" {
            let _ = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot found")
                .await;
            continue;
        }

        let (code, state, error) = parse_oauth_query(query);
        match resolve_callback_code(code, state, error, expected_state) {
            Ok(code) => {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: https://platform.claude.com/oauth/code/success?app=claude-code\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
                let _ = stream.flush().await;
                return Ok(code);
            }
            Err(err) => {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body><h2>Authentication failed</h2><p>You can close this tab.</p></body></html>",
                    )
                    .await;
                let _ = stream.flush().await;
                return Err(err);
            }
        }
    }
}

/// Fallback path: accept a pasted callback URL or raw code from stdin. Empty
/// lines keep waiting; a full URL has its `state` validated (reject on
/// mismatch) exactly like the callback. On a non-TTY / EOF stdin, this never
/// settles so the browser callback or the timeout wins (mirrors the JS
/// `isTTY` guard).
async fn read_stdin_code(expected_state: &str) -> anyhow::Result<String> {
    eprint!("Paste authorization code here (or wait for browser callback): ");
    let _ = std::io::stderr().flush();

    let mut reader = BufReader::new(tokio::io::stdin());
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            // EOF (piped/closed stdin): never settle from here.
            std::future::pending::<()>().await;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A pasted callback URL: validate state, extract code.
        if let Some((_, query)) = trimmed.split_once('?') {
            let (code, state, _error) = parse_oauth_query(query);
            if let Some(code) = code {
                // Only reject when a state was supplied and does not match —
                // a bare code without state stays acceptable (JS parity).
                if let Some(state) = state
                    && state != expected_state
                {
                    bail!("OAuth state mismatch");
                }
                return Ok(code);
            }
        }
        // Otherwise treat the whole line as the raw authorization code.
        return Ok(trimmed.to_string());
    }
}

/// Wait for the authorization code from whichever source arrives first — the
/// browser callback or a manual stdin paste — bounded by the 2-minute timeout.
/// In [`LoginUi::Machine`] the stdin branch is not merely inert, it is not
/// constructed: a GUI parent's child process has a pipe (or `/dev/null`) on
/// stdin, and `read_stdin_code`'s first act is an `eprint!` prompt to a human
/// who is not there. The callback and the timeout are the only two ways that
/// mode can settle.
async fn wait_for_code(
    listener: TcpListener,
    expected_state: &str,
    ui: LoginUi,
) -> anyhow::Result<String> {
    if !ui.reads_stdin() {
        return tokio::select! {
            result = run_callback_server(listener, expected_state) => result,
            _ = tokio::time::sleep(LOGIN_TIMEOUT) => Err(anyhow!("Login timed out after 2 minutes")),
        };
    }
    tokio::select! {
        result = run_callback_server(listener, expected_state) => result,
        result = read_stdin_code(expected_state) => result,
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => Err(anyhow!("Login timed out after 2 minutes")),
    }
}

/// Lifetime (seconds) requested for the access token minted at login, sent
/// as a client-supplied `expires_in` in the exchange body. The token endpoint
/// is CLIENT-DRIVEN here, not server-fixed: measured against the live
/// endpoint, the same request with no `expires_in` was granted `28800` (8h);
/// with `expires_in: 31536000` it was granted `31536000` (365d) verbatim —
/// `docs/design/long-lived-tokens.md` ("The finding"). This only works
/// because [`OAUTH_SCOPES`] excludes the scopes the endpoint refuses a custom
/// `expires_in` for; see that constant's doc comment.
pub const LOGIN_TOKEN_LIFETIME_SECS: u64 = 31_536_000;

/// Build the JSON body for [`exchange_code`]'s `POST {TOKEN_ENDPOINT}`, as a
/// pure function so the request shape can be asserted without the network —
/// mirrors `warm_request_spec` in `src/warmer.rs`.
fn exchange_request_body(
    code: &str,
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> serde_json::Value {
    serde_json::json!({
        "code": code,
        "state": state,
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
        "expires_in": LOGIN_TOKEN_LIFETIME_SECS,
    })
}

/// Exchange the authorization code for tokens (`POST {TOKEN_ENDPOINT}`).
///
/// This is the ONE step kept as a manual `reqwest` JSON POST rather than
/// oauth2's `exchange_code`: Claude's token endpoint is non-standard — it wants
/// a JSON body with `Content-Type: application/json` (as the JS reference
/// sends), not the RFC 6749 form-encoding oauth2 emits, and it echoes `state`
/// in the body. Uses a `.no_proxy()` client so an ambient `HTTPS_PROXY` cannot
/// swallow the request.
async fn exchange_code(
    code: &str,
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> anyhow::Result<Exchanged> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("build OAuth token-exchange client")?;

    let response = client
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", OAUTH_ACCEPT)
        .header("User-Agent", OAUTH_USER_AGENT)
        .json(&exchange_request_body(code, verifier, state, redirect_uri))
        .send()
        .await
        .context("token exchange request failed")?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Token exchange failed ({}): {text}", status.as_u16());
    }

    tokens_from_exchange_body(&text)
}

/// What one code-for-token exchange produced: the credential, and the identity
/// the token endpoint sent beside it.
struct Exchanged {
    tokens: Tokens,
    identity: ExchangeIdentity,
}

/// The `account` and `organization` objects of a token-exchange response.
///
/// The token endpoint sends these on every exchange, whatever the scope
/// (`docs/design/long-lived-tokens.md`, "Identity arrives free at exchange
/// time"). [`login`] still asks the profile endpoint first, because only it
/// carries the plan fields; this is what names the row when that fetch comes
/// back empty, instead of [`unnamed_fallback`].
#[derive(Debug, Default, Deserialize)]
struct ExchangeIdentity {
    #[serde(default)]
    account: Option<ExchangeAccount>,
    #[serde(default)]
    organization: Option<ExchangeOrg>,
}

#[derive(Debug, Deserialize)]
struct ExchangeAccount {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExchangeOrg {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// The token-exchange response: the refresh fields plus the identity. A
/// separate struct from [`RefreshResponse`], because a refresh never needs
/// the identity.
#[derive(Deserialize)]
struct ExchangeResponse {
    #[serde(flatten)]
    refresh: RefreshResponse,
    #[serde(flatten)]
    identity: ExchangeIdentity,
}

/// Turn a raw token-endpoint response body into [`Tokens`], failing the exchange
/// when no usable `refresh_token` is present.
///
/// Unlike the *refresh* path — which has the prior refresh token to fall back on
/// (`refresh_token.unwrap_or_else(|| refresh_token.to_string())`) — a code→token
/// *exchange* has no prior token. So an absent or empty/whitespace `refresh_token`
/// here would mint an account whose stored token is `""`; that account dies with
/// `AuthRejected` on its first refresh (the refresh POST sends the empty token).
/// Fail the exchange instead of persisting a self-destructing account.
fn tokens_from_exchange_body(text: &str) -> anyhow::Result<Exchanged> {
    let ExchangeResponse {
        refresh: data,
        identity,
    } = serde_json::from_str(text).context("parse token-exchange response")?;

    let refresh_token = match data.refresh_token {
        Some(token) if !token.trim().is_empty() => token,
        _ => bail!("token exchange returned no refresh_token"),
    };

    let expires_at_ms = data.expires_at.map_or_else(
        || expires_at_from(crate::now_ms(), data.expires_in),
        normalize_expires_at,
    );

    Ok(Exchanged {
        tokens: Tokens {
            id_token: None,
            access_token: data.access_token,
            refresh_token: Some(refresh_token),
            expires_at_ms,
        },
        identity,
    })
}

/// The account+org identity fetched from the profile endpoint. Every field is
/// optional so a partial or failed fetch degrades gracefully: `email` names the
/// account (falls back to a prompt when absent) and `account_uuid`/`org_uuid`/
/// `org_name` key the account's identity so multi-org logins stay distinct.
pub struct Profile {
    email: Option<String>,
    account_uuid: Option<String>,
    org_uuid: Option<String>,
    org_name: Option<String>,
    /// `organization.organization_type`, verbatim — the account's PLAN. See
    /// [`crate::config::Account::organization_type`] for why this field and not
    /// `account.has_claude_max`.
    pub organization_type: Option<String>,
    /// `organization.rate_limit_tier`, verbatim — what separates Max 20x from
    /// Max 5x.
    pub rate_limit_tier: Option<String>,
    /// `organization.seat_tier`, verbatim — which seat this row holds on a
    /// seated org (`team_standard`, `team_tier_1`). `None` on Max and Pro,
    /// which have no seats.
    pub seat_tier: Option<String>,
}

impl Profile {
    /// `account.email`, or `None` when the fetch failed or the endpoint sent none.
    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    /// `account.uuid`, or `None` when the fetch failed or the endpoint sent none.
    pub fn account_uuid(&self) -> Option<&str> {
        self.account_uuid.as_deref()
    }

    /// This profile, with each identity field the profile endpoint left empty
    /// taken from the token-exchange response instead. A field the profile
    /// endpoint did send is kept. Both answers describe the one token the
    /// exchange just minted, so they cannot name two different accounts.
    fn or_exchange_identity(self, identity: ExchangeIdentity) -> Profile {
        let (account_uuid, email) = identity
            .account
            .map_or((None, None), |a| (a.uuid, a.email_address));
        let (org_uuid, org_name) = identity
            .organization
            .map_or((None, None), |o| (o.uuid, o.name));
        Profile {
            email: self.email.or(email),
            account_uuid: self.account_uuid.or(account_uuid),
            org_uuid: self.org_uuid.or(org_uuid),
            org_name: self.org_name.or(org_name),
            ..self
        }
    }
}

/// Fetch the account+org identity from the profile endpoint. Returns an
/// all-`None` [`Profile`] on any failure (network, non-2xx, or malformed body)
/// so the caller can still prompt for a name and login without org info. Serde
/// ignores unknown fields, so extra profile keys are harmless.
pub async fn fetch_profile(access_token: &str) -> Profile {
    fetch_profile_at(PROFILE_URL, access_token).await
}

/// [`fetch_profile`] against an explicit profile URL — the proxy's client-identity
/// check resolves a client bearer against its configured upstream, which in tests
/// is a local fake rather than [`PROFILE_URL`].
pub async fn fetch_profile_at(url: &str, access_token: &str) -> Profile {
    try_fetch_profile_at(url, access_token)
        .await
        .unwrap_or(Profile {
            email: None,
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
        })
}

/// Why a profile fetch produced no [`Profile`]. The proxy's client-identity check
/// treats the two arms differently: a status is upstream's verdict on the bearer
/// and can be remembered for a while, a transport failure says nothing about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileFetchError {
    /// The request never completed, or the body did not parse.
    Transport,
    /// Upstream answered, with this non-2xx status.
    Status(u16),
}

/// [`fetch_profile_at`], keeping the reason for a miss.
pub async fn try_fetch_profile_at(
    url: &str,
    access_token: &str,
) -> Result<Profile, ProfileFetchError> {
    #[derive(Deserialize)]
    struct ProfileResponse {
        account: Option<ProfileAccount>,
        organization: Option<ProfileOrg>,
    }
    #[derive(Deserialize)]
    struct ProfileAccount {
        uuid: Option<String>,
        email: Option<String>,
    }
    #[derive(Deserialize)]
    struct ProfileOrg {
        uuid: Option<String>,
        name: Option<String>,
        organization_type: Option<String>,
        rate_limit_tier: Option<String>,
        seat_tier: Option<String>,
    }

    async fn inner(url: &str, access_token: &str) -> Result<ProfileResponse, ProfileFetchError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|_| ProfileFetchError::Transport)?;
        let response = client
            .get(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|_| ProfileFetchError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProfileFetchError::Status(status.as_u16()));
        }
        response
            .json()
            .await
            .map_err(|_| ProfileFetchError::Transport)
    }

    let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());
    inner(url, access_token).await.map(|profile| {
        let account = profile.account;
        let organization = profile.organization;
        Profile {
            email: non_empty(account.as_ref().and_then(|a| a.email.clone())),
            account_uuid: non_empty(account.and_then(|a| a.uuid)),
            org_uuid: non_empty(organization.as_ref().and_then(|o| o.uuid.clone())),
            org_name: non_empty(organization.as_ref().and_then(|o| o.name.clone())),
            organization_type: non_empty(
                organization
                    .as_ref()
                    .and_then(|o| o.organization_type.clone()),
            ),
            rate_limit_tier: non_empty(
                organization
                    .as_ref()
                    .and_then(|o| o.rate_limit_tier.clone()),
            ),
            seat_tier: non_empty(organization.and_then(|o| o.seat_tier)),
        }
    })
}

/// Prompt for an account name on stdin, falling back to `fallback` on empty
/// input or unreadable stdin.
fn prompt_account_name(fallback: &str) -> String {
    eprint!("Name this account [{fallback}]: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(_) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                fallback.to_string()
            } else {
                trimmed.to_string()
            }
        }
        Err(_) => fallback.to_string(),
    }
}

/// Add or update an OAuth account in `config`, keyed on `(account_uuid, org)`
/// via [`identity::resolve`] — never a loose first-match — so the legacy
/// two-org tie ([`identity::same_identity_strict`]) is broken the same way
/// [`config::save_account`] and [`crate::manager::Manager::add_or_update_account`]
/// already break it, and an unbreakable tie is REFUSED rather than guessed:
/// guessing here means stamping a fresh credential over a DIFFERENT account's
/// single-use refresh token (see [`identity::Resolved::Many`]'s doc-comment).
/// An existing account with the same identity has its tokens refreshed in
/// place and any absent identity field backfilled (never duplicated);
/// otherwise a new account is appended with the next-highest priority and
/// every identity field populated.
///
/// Backward compatible: when the probe and the stored entry both lack identity
/// fields (today's real config), `identity::resolve` falls back to name
/// equality — so a single-org re-login matches its existing entry exactly as
/// before.
/// Takes the whole [`Profile`] rather than its fields one by one: the plan
/// fields joined the identity ones here, and five loose `Option<String>`
/// positional arguments of the same type are a call site where two of them can
/// be transposed with nothing to catch it.
pub fn upsert_account(
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    profile: &Profile,
) -> anyhow::Result<()> {
    let account_uuid = profile.account_uuid.clone();
    let org_uuid = profile.org_uuid.clone();
    let org_name = profile.org_name.clone();
    // An Anthropic login, so an Anthropic probe: it never resolves to a Codex row
    // that shares its name (`identity::same_identity`), and the row it appends below
    // is Anthropic too.
    let probe = crate::identity::probe(
        crate::provider::Provider::Anthropic,
        name,
        account_uuid.clone(),
        org_uuid.clone(),
        org_name.clone(),
    );

    match crate::identity::resolve(config.accounts.iter().enumerate(), &probe) {
        crate::identity::Resolved::One(index) => {
            let account = &mut config.accounts[index];
            account.account_type = crate::config::AccountType::OAuth;
            account.access_token = tokens.access_token.clone();
            account.refresh_token = tokens.refresh_token.clone();
            account.expires_at = Some(tokens.expires_at_ms);
            // Backfill any identity field the stored entry was missing (e.g. a
            // legacy pre-org entry newly profiled), without overwriting known
            // values.
            if account.account_uuid.is_none() {
                account.account_uuid = account_uuid;
            }
            if account.org_uuid.is_none() {
                account.org_uuid = org_uuid;
            }
            if account.org_name.is_none() {
                account.org_name = org_name;
            }
            // OVERWRITTEN, not backfilled, unlike the identity fields above —
            // an org upgrades its plan, so the first reading must not be pinned
            // forever. Same rule `config::merge_account` applies on disk; see
            // `config::Account::organization_type`. A `None` writes nothing, so
            // a profile fetch that failed never blanks a known plan.
            if profile.organization_type.is_some() {
                account.organization_type = profile.organization_type.clone();
            }
            if profile.rate_limit_tier.is_some() {
                account.rate_limit_tier = profile.rate_limit_tier.clone();
            }
            if profile.seat_tier.is_some() {
                account.seat_tier = profile.seat_tier.clone();
            }
            Ok(())
        }
        crate::identity::Resolved::None => {
            let next_priority = config
                .accounts
                .iter()
                .filter_map(|a| a.priority)
                .max()
                .map_or(0, |max| max + 1);

            config.accounts.push(Account {
                name: name.to_string(),
                account_type: crate::config::AccountType::OAuth,
                provider: Default::default(),
                account_uuid,
                org_uuid,
                org_name,
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires_at: Some(tokens.expires_at_ms),
                priority: Some(next_priority),
                switch_threshold: None,
                disabled: None,
                groups: None,
                organization_type: profile.organization_type.clone(),
                rate_limit_tier: profile.rate_limit_tier.clone(),
                seat_tier: profile.seat_tier.clone(),
                egress: crate::config::Egress::Local,
                egress_strict: false,
                codex_home: None,
                extra: serde_json::Map::new(),
            });
            Ok(())
        }
        // Recompute the loose set for its names: `Resolved::Many` itself
        // carries none, and this is exactly the set `resolve` drew from (same
        // predicate, same candidates) — mirrors
        // `Manager::add_or_update_account`'s identical recomputation.
        crate::identity::Resolved::Many => {
            let candidates: Vec<String> = config
                .accounts
                .iter()
                .filter(|a| crate::identity::same_identity(a, &probe))
                .map(|a| a.name.clone())
                .collect();
            bail!("{}", crate::cli::ambiguous_query_message(name, &candidates));
        }
    }
}

/// The proxy port a login must guard against — the port the running server binds.
/// Reads `proxy.port` from the config (falling back to the serde default when the
/// file is missing or unreadable), so the guard checks the SAME port `tcr server`
/// takes over.
fn login_target_port(config_path: &Path) -> u16 {
    config::load(config_path)
        .unwrap_or_else(|_| {
            serde_json::from_str("{}").expect("empty object is a valid default config")
        })
        .proxy
        .port
}

/// What `login()` should do with the finished credential, decided purely from
/// the incumbent detected on the guarded port, whether a probe already
/// confirmed THAT incumbent carries the live account-add route, and
/// `--force`. Returned by [`login_guard_refusal_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoginRoute {
    /// No live proxy on the port at all — the historical file-only login,
    /// unchanged. Also reached under `--force` when the probe could not
    /// confirm a safe live route (no route, or an unusable answer):
    /// `--force` is the escape hatch PAST A REFUSAL, never a way to skip a
    /// live route that IS there — `Live` below takes priority over it. An
    /// api-key rejection (`AddCapability::Unauthorized`) never resolves to
    /// this, under `--force` or otherwise: that is positive evidence the
    /// proxy is alive, so there is no unsafe file write for `--force` to
    /// rescue — see `login_route`'s `Unauthorized` arm.
    File,
    /// A live proxy is there and its add route is confirmed: route the
    /// finished credential through it instead of the file. Wins even against
    /// `--force` — forcing the unsafe file write only makes sense when the
    /// safe live route is not available.
    Live,
    /// A live proxy is there without the route, and `--force` was not given:
    /// refuse outright with this message, unchanged from before this route
    /// existed.
    Refuse(String),
}

/// The pure login-guard DECISION: given whether the HTTP capability probe
/// already confirmed a live account-add route ([`probe_add_route`], run
/// impure and BEFORE this), any live proxy separately detected on the port
/// (the impure lsof/owner-file detection,
/// [`crate::singleton::live_proxy_server`]), the port, and the `--force`
/// flag, return which [`LoginRoute`] `login()` should take. Split from both
/// impure detectors so the decision itself stays unit-testable, mirroring
/// singleton's pure-decision / impure-executor split.
///
/// `has_add_route` is decided FIRST and is decisive on its own, independent
/// of `incumbent` — including when `incumbent` is `None`. The HTTP probe is
/// first-hand evidence that a live proxy with the route is answering on this
/// exact port; `incumbent` is an INFERENCE from a pid, an argv string, or an
/// owner-file claim, any of which can be stale, absent, or blind to a host
/// application the pid/argv matcher was never taught to recognize (see
/// `ProxyHost::Unknown`'s doc-comment). Gating the live path on `incumbent`
/// agreeing would mean any future regression in that inference silently
/// reintroduces the whole-file clobber this unit exists to remove — as a
/// silent fallback rather than a loud one. So a confirmed route wins even
/// against a `None` incumbent, and `incumbent` is consulted only to build the
/// REFUSAL message, for the one case where the probe did NOT confirm the
/// route.
///
/// The incumbent's KIND decides the refusal instruction, and it is not
/// cosmetic. Since the owner file, detection reaches a proxy served INSIDE a
/// host application, and the pid reported is then the HOST's. "kill {pid}"
/// would SIGTERM a GUI process that installs no handler for it: no
/// `applicationWillTerminate`, no final session→account pin write, and every
/// live session cold-starts its prompt cache at the next boot.
/// `takeover_decision` and `incumbents_to_signal` are both hardened against
/// exactly that signal; a message that ADVISES it would walk around both. So
/// an embedded incumbent is told to be quit, not killed.
///
/// The refusal names `command`, the login that was run (`tcr login`,
/// `tcr login --from-codex`, `tcr login --codex`, …), as the one to run again
/// once the server is stopped: telling someone who ran an import to run
/// `tcr login` sends them into the browser flow they did not ask for.
fn login_guard_refusal_for(
    incumbent: Option<singleton::Incumbent>,
    has_add_route: bool,
    port: u16,
    force: bool,
    command: &str,
) -> LoginRoute {
    // `has_add_route` is checked BEFORE `force`, deliberately: a confirmed
    // route wins even against the escape hatch. Checking `force` first (the
    // pre-fix order) meant `--force` chose the file write even when the live
    // route was confirmed safe — the exact whole-file clobber this unit
    // exists to remove, reintroduced by the escape hatch meant to route
    // around a REFUSAL, not around a safe route that was never refused.
    if has_add_route {
        return LoginRoute::Live;
    }
    if force {
        return LoginRoute::File;
    }
    match incumbent {
        Some(incumbent) => {
            let pid = incumbent.pid;
            LoginRoute::Refuse(match incumbent.kind {
                singleton::ProxyKind::TcrEmbedded => format!(
                    "a tcr server is already running on port {port} (pid {pid}); logging in now would be overwritten by the server's next token refresh — that pid is the HOST APPLICATION serving the proxy in-process, so do not signal it: quit the host application (killing it skips its shutdown and loses the session pin map), run '{command}', then start it again. Re-run with --force to log in anyway."
                ),
                singleton::ProxyKind::Tcr | singleton::ProxyKind::LegacyJs => format!(
                    "a tcr server is already running on port {port} (pid {pid}); logging in now would be overwritten by the server's next token refresh — stop it (kill {pid}, or Ctrl-C in its terminal), run '{command}', then restart 'tcr server'. Re-run with --force to log in anyway."
                ),
            })
        }
        None => LoginRoute::File,
    }
}

/// What a live proxy said, structurally, about whether it can accept a live
/// account add. Read from [`crate::cli::post_add_account`]'s own
/// [`crate::cli::LiveControlError`] classification, which is itself driven by
/// [`crate::proxy::ENDPOINT_HEADER`] — never by matching error text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AddCapability {
    /// The route answered (stamped, whatever the status) — a live add is
    /// possible.
    Present,
    /// A connection could not even be made — genuinely nothing is listening
    /// on the port. The ONLY outcome that may resolve to `LoginRoute::File`
    /// without `--force` and without `incumbent` separately confirming a
    /// server: there is truly no live process to disagree with the file.
    Absent,
    /// The proxy answered and rejected our api-key. Its own condition — never
    /// folded into `Absent`: that would either wrongly read as an older proxy
    /// with no route, or let `login()` silently fall back to writing the file
    /// out from under a server that is right there, listening.
    Unauthorized,
    /// Something holds the port and is either an older proxy answering
    /// without the route (identified structurally, never by error text) or
    /// accepted the connection and then produced nothing usable before the
    /// deadline — the wedged-port shape `cli::probe_incumbent`'s own tests
    /// document as measured and real (`a_listener_that_never_answers_is_silent_not_answering`).
    /// Both are first-hand evidence a live process holds the port — at least
    /// as strong as the `incumbent` pid heuristic, and stronger exactly where
    /// that heuristic is documented to miss (an embedded host). Split out of
    /// `Absent` because the two are NOT the same conservative answer: folding
    /// them together let a `None` incumbent silently choose `File` beside a
    /// real, wedged server, without `--force` ever being asked for. Carries
    /// the probe's own account of what happened, for the refusal message.
    Unusable(String),
}

/// [`probe_add_route`]'s capability alone. Test-only: every production caller
/// goes through `probe_add_route`, which this delegates to, so the tests that
/// pin the capability pin the very probe production sends.
#[cfg(test)]
async fn probe_add_capability(config: &Config) -> AddCapability {
    probe_add_route(config).await.0
}

/// Probe whether the proxy `config` points at can accept a live account add,
/// via a deliberately-invalid POST to [`crate::proxy::ADD_ACCOUNT_PATH`] — a
/// blank name and access token, always rejected by the route's own
/// validation — so this can run BEFORE any OAuth tokens exist and before the
/// browser opens. Never probes by adding a real account.
///
/// Also returns the providers the server named in
/// [`crate::proxy::PROVIDERS_HEADER`] on that SAME answer, so a login of a
/// provider that must be proven storable (anything but Anthropic: see that
/// header) learns it with no second request. Empty when the server named none,
/// which is how a server that predates the header, and every non-answer, reads.
async fn probe_add_route(config: &Config) -> (AddCapability, Vec<crate::provider::Provider>) {
    let probe = Account {
        name: String::new(),
        account_type: crate::config::AccountType::OAuth,
        provider: Default::default(),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        priority: None,
        switch_threshold: None,
        disabled: None,
        groups: None,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        egress: crate::config::Egress::Local,
        egress_strict: false,
        codex_home: None,
        extra: serde_json::Map::new(),
    };
    let (answer, providers) = crate::cli::post_add_account_with_providers(config, &probe).await;
    let capability = match answer {
        // A blank name/token can never be added, so `Ok` cannot happen in
        // practice — but a stamped success is just as much "the route
        // exists" as a stamped rejection, so it counts as `Present` too.
        Ok(_) => AddCapability::Present,
        // The route validated the (deliberately bad) body and said so,
        // stamped — exactly the positive capability signal.
        Err(crate::cli::LiveControlError::Rejected(_)) => AddCapability::Present,
        Err(crate::cli::LiveControlError::Unauthorized) => AddCapability::Unauthorized,
        // A connection could not be made at all — the ONLY genuinely-absent
        // case.
        Err(crate::cli::LiveControlError::NoServer) => AddCapability::Absent,
        // Everything else (`NoRoute`, `NoAnswer`, `Unusable`) is first-hand
        // evidence something holds the port, just not usably.
        Err(other) => AddCapability::Unusable(other.why()),
    };
    (capability, providers)
}

/// Resolve which [`LoginRoute`] `login()` should take. Runs the impure HTTP
/// capability probe UNCONDITIONALLY — including under `--force` — deliberately
/// never gated on whether [`crate::singleton::live_proxy_server`]
/// (`incumbent`) noticed anything on the port first, NOR on `force`: skipping
/// the probe under `--force` is exactly the bug bridge item C removed, since
/// it is what let `--force` take the file path even when the live route was
/// confirmed and safe. `force` only changes what happens when the probe
/// itself produced an UNUSABLE answer (something holds the port but is not
/// confirmed safe) — that case falls back to `LoginRoute::File` under
/// `--force` instead of erroring out, mirroring the historical "force always
/// succeeds via the file" behaviour. An api-key rejection (`Unauthorized`) is
/// a DIFFERENT case, and `--force` never rescues it: it is positive evidence
/// of a live, healthy proxy — it answered, parsed our request, and rejected
/// the key — which makes it the worst-informed moment to whole-file-write
/// beside it, since a live server rotating tokens is exactly what makes that
/// write destructive. Fully resolves before `login()` starts the OAuth
/// dance, so a user is never told to stop a server AFTER completing a full
/// browser round-trip, and never silently falls back to the file when a live
/// proxy answered but rejected our api-key.
async fn login_route(
    config_path: &Path,
    incumbent: Option<singleton::Incumbent>,
    port: u16,
    force: bool,
) -> anyhow::Result<LoginRoute> {
    login_route_for(config_path, incumbent, port, force, LOGIN_COMMAND).await
}

/// [`login_guard_refusal_for`] for plain `tcr login`, whose refusal says `run 'tcr login'`. The
/// guard's own tests pin the decision through it.
#[cfg(test)]
fn login_guard_refusal(
    incumbent: Option<singleton::Incumbent>,
    has_add_route: bool,
    port: u16,
    force: bool,
) -> LoginRoute {
    login_guard_refusal_for(incumbent, has_add_route, port, force, LOGIN_COMMAND)
}

/// [`login_route`] for the login `command` names (`tcr login --from-claude-code`, …), which a
/// refusal tells the user to run again ([`login_guard_refusal_for`]).
async fn login_route_for(
    config_path: &Path,
    incumbent: Option<singleton::Incumbent>,
    port: u16,
    force: bool,
    command: &str,
) -> anyhow::Result<LoginRoute> {
    login_route_with_providers(config_path, incumbent, port, force, command)
        .await
        .map(|(route, _)| route)
}

/// [`login_route`], plus the providers the probe's answer advertised
/// ([`probe_add_route`]). Every decision is `login_route`'s; the list rides
/// along for the callers that must refuse a live server unable to store their
/// provider: `import_codex_login`, and `tcr login --codex` before codex runs
/// ([`codex_login_route_check`]). `command` is the login that was run, which a
/// refusal names ([`login_guard_refusal_for`]).
async fn login_route_with_providers(
    config_path: &Path,
    incumbent: Option<singleton::Incumbent>,
    port: u16,
    force: bool,
    command: &str,
) -> anyhow::Result<(LoginRoute, Vec<crate::provider::Provider>)> {
    let config = load_or_default(config_path)?;
    let (capability, providers) = probe_add_route(&config).await;
    let route = match capability {
        // Positive evidence the proxy is alive: it answered, parsed our
        // request, and rejected the key. Refuses regardless of `--force` —
        // unlike `Unusable` below, there is no unsafe-but-forceable file
        // write to fall back to here, only a safe path one config edit away.
        AddCapability::Unauthorized => {
            bail!(
                "the proxy on :{port} rejected the api-key in {} while checking whether it \
                 could take a live login — no browser was opened and nothing was changed. This \
                 refuses even under --force: a rejected api-key is proof the proxy is alive, \
                 which makes this the worst-informed moment to write the config file beside it. \
                 Fix `proxy.apiKey` in the config to match the running server, or stop the \
                 server and log in offline.",
                config_path.display()
            );
        }
        AddCapability::Present => login_guard_refusal_for(incumbent, true, port, force, command),
        AddCapability::Absent => login_guard_refusal_for(incumbent, false, port, force, command),
        // Bridge item D: something holds the port but answered unusably —
        // first-hand evidence at least as strong as `incumbent`, so this
        // refuses REGARDLESS of what `incumbent` separately concluded, never
        // falling through to `LoginRoute::File` the way folding this into
        // `Absent` used to when `incumbent` was `None`.
        AddCapability::Unusable(why) => {
            if force {
                return Ok((LoginRoute::File, providers));
            }
            bail!(
                "the proxy on :{port} answered but not usably ({why}) while checking whether \
                 it could take a live login — no browser was opened and nothing was changed. \
                 This is first-hand evidence something holds the port even though process \
                 detection may have missed it, so the historical whole-file-write hazard still \
                 applies. Re-run with --force to log in via the file instead (this still risks \
                 the running server overwriting it on its next token refresh)."
            );
        }
    };
    Ok((route, providers))
}

/// What the browser half of a login produced: the PKCE/CSRF material, the
/// redirect URI it was bound to, and the authorization code that came back.
/// All three are needed together by [`exchange_code`], so they travel as one
/// value rather than as three locals a caller could pair wrongly.
struct Authorized {
    flow: LoginFlow,
    redirect_uri: String,
    code: String,
}

/// The offline half of [`login`]: bind the loopback callback, build the
/// authorize URL, tell whoever is listening about it, and wait for the code.
///
/// Split out of `login` so the event sequence is testable at all. Everything
/// after this point talks to Anthropic's token and profile endpoints, so an
/// end-to-end test of `login` is not possible without the network; a test can
/// drive THIS with a fake callback and assert both the emitted lines and the
/// code that comes back. `emit` is the sink for that reason — the shipping
/// caller hands it [`LoginUi::emit`] (stdout), a test hands it a `Vec`.
///
/// The two modes differ in exactly two places, both here: who opens the
/// browser, and whether the progress is prose or JSON.
async fn authorize_and_wait(
    hint: Option<&str>,
    ui: LoginUi,
    emit: &mut dyn FnMut(LoginEvent<'_>),
) -> anyhow::Result<Authorized> {
    // Bind the callback server on a random loopback port (127.0.0.1 only).
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind local OAuth callback server")?;
    let callback_port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{callback_port}/callback");

    // PKCE + CSRF state + authorize URL, built by the oauth2 crate.
    let flow = build_login_flow(&redirect_uri, hint)?;
    match ui {
        LoginUi::Terminal => {
            println!("Opening browser for authentication...");
            println!("If it doesn't open, visit:\n  {}\n", flow.auth_url);
            open_browser(&flow.auth_url);
        }
        LoginUi::Machine => emit(LoginEvent::Browser {
            url: &flow.auth_url,
        }),
    }
    emit(LoginEvent::Waiting);

    let code = wait_for_code(listener, &flow.state, ui).await?;
    Ok(Authorized {
        flow,
        redirect_uri,
        code,
    })
}

/// Run the full browser OAuth login and persist the account. Returns the
/// account name on success. Never logs or prints the tokens.
///
/// Refuses (unless `force`) when a live proxy server already holds the
/// configured port AND that proxy has no live account-add route: an older
/// tcr reads config only at boot, and its next `persist_tokens` writes its
/// boot-time TOKENS back over the file, silently clobbering the fresh ones
/// this login writes (observed live 2026-07-19 — a server refresh overwrote a
/// re-login within seconds). When the live route IS there, this routes the
/// finished credential through the server instead ([`finish_login`]) — the
/// server owns the write with its own current state, so the window above
/// does not exist on that path at all. Detection is read-only; the server is
/// never signalled.
pub async fn login(
    config_path: &Path,
    force: bool,
    account: Option<&str>,
    name_override: Option<&str>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    ui.install();
    let port = login_target_port(config_path);
    let incumbent = singleton::live_proxy_server(port);
    let route = login_route(config_path, incumbent, port, force).await?;
    if let LoginRoute::Refuse(msg) = route {
        bail!("{}", msg);
    }

    // Resolve --account early, ONLY to compute the login_hint (ergonomics —
    // see build_login_flow's doc-comment). Fail fast here on a typo'd or
    // ambiguous name, and on a row this sign-in can never confirm (a Codex
    // account, `refuse_a_non_claude_row`), rather than after a full browser
    // round trip. The SAME resolution runs again below, against a freshly
    // loaded config, where it is load-bearing rather than a convenience.
    let hint = match account {
        Some(query) => {
            let probe_config = load_or_default(config_path)?;
            let idx = crate::cli::resolve_account(&probe_config.accounts, query)?;
            refuse_a_non_claude_row(&probe_config.accounts[idx])?;
            let email = crate::identity::email_of(&probe_config.accounts[idx].name);
            // Only ever send something address-shaped: a non-email display
            // name (e.g. an account named "work") produces `login_hint=work`
            // otherwise — an unvalidated non-address value on an endpoint
            // this crate already knows is picky about param shape (the
            // 32-byte-state note above). Skipping it there costs nothing:
            // the hint is ergonomics only (see this function's doc-comment),
            // never the source of truth.
            if looks_like_email(email) {
                Some(email.to_string())
            } else {
                None
            }
        }
        None => None,
    };

    let authorized = authorize_and_wait(hint.as_deref(), ui, &mut |event| ui.emit(event)).await?;
    let Authorized {
        flow,
        redirect_uri,
        code,
    } = authorized;

    ui.say("Exchanging authorization code for tokens...");
    let Exchanged { tokens, identity } =
        exchange_code(&code, &flow.verifier, &flow.state, &redirect_uri).await?;

    // Load the existing config once (missing file → empty default; a corrupt
    // file surfaces as an error so login never clobbers it with defaults).
    let mut config = load_or_default(config_path)?;

    // Fetch the account+org identity, then MINT a name for it. A failed
    // profile fetch still names the row from the exchange response.
    let profile = fetch_profile(&tokens.access_token)
        .await
        .or_exchange_identity(identity);
    let name = mint_login_name(&config, &profile, account, name_override, ui)?;

    let saved = finish_login_checked(
        config_path,
        route,
        &mut config,
        &name,
        &tokens,
        profile,
        account,
    )
    .await?;
    ui.emit(LoginEvent::Saved { account: &saved });
    Ok(saved)
}

/// The name a browser login writes under.
///
/// Nothing is prompted for on the happy path. The profile's email is minted into
/// a free name by [`crate::identity::mint_name`] — the bare email when no other
/// row carries it, `email/<org-slug>` when one does — so a second org of the same
/// person lands beside the first instead of on top of it.
///
/// Three things override or replace that, in order:
///
/// * `name_override` (`--name`) wins outright, and is REFUSED when some other
///   account already holds it. Taking a name off an existing row is how a login
///   overwrites the wrong credential, and it is the one case a name collision
///   would be silent rather than migrated.
/// * A RE-login (`--account`) keeps the name the row already has. Renaming a
///   row on re-login would break the pins, groups and control-account key that
///   name is the identity for; the row is confirmed to be the same account by
///   [`assert_requested_identity`] a moment later.
/// * A profile with no email at all is the only path left that prompts. A
///   browser login reaches it only when the token-exchange response carried no
///   email either ([`Profile::or_exchange_identity`]); a setup-token login,
///   which never sees an exchange response, reaches it every time. The prompt is
///   [`prompt_account_name`], and the name it comes back with is minted for
///   uniqueness too. Under [`LoginUi::Machine`] there is
///   nobody to prompt, so that path takes [`unnamed_fallback`] directly — the
///   same answer an empty reply to the prompt gives.
fn mint_login_name(
    config: &Config,
    profile: &Profile,
    requested_account: Option<&str>,
    name_override: Option<&str>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    let taken: std::collections::HashSet<String> =
        config.accounts.iter().map(|a| a.name.clone()).collect();

    if let Some(explicit) = name_override {
        if taken.contains(explicit) {
            bail!(
                "'{explicit}' is already the name of another account — account names are unique. \
                 Pick a different --name, or re-login that account with `tcr login --account \
                 {explicit}`. Nothing was changed."
            );
        }
        return Ok(explicit.to_string());
    }

    // A re-login keeps the row's own name: it IS the identity every pin, group
    // and `controlAccount` key already points at.
    if let Some(query) = requested_account {
        let idx = crate::cli::resolve_account(&config.accounts, query)?;
        return Ok(config.accounts[idx].name.clone());
    }

    let base = match (&profile.email, ui) {
        (Some(email), _) => email.clone(),
        // No email and no human: take the same fallback the prompt itself
        // defaults to on an empty answer, rather than prompting into a pipe
        // and reading EOF. A machine caller that wants a specific name passes
        // `--name`.
        (None, LoginUi::Machine) => unnamed_fallback(&config.accounts),
        (None, LoginUi::Terminal) => prompt_account_name(&unnamed_fallback(&config.accounts)),
    };
    Ok(crate::identity::mint_name(
        &base,
        profile.org_name.as_deref(),
        profile.org_uuid.as_deref(),
        &taken,
    ))
}

/// Our ASSUMPTION about a `claude setup-token` credential's lifetime — one
/// year, matching the `expires_in: 31536000` the Claude CLI requests when it
/// mints one. Not something [`login_with_token`] can read out of the token
/// itself (it never sees the token-endpoint response, only the finished
/// access token pasted or piped in), so this is a best estimate, stamped so
/// `tcr status` shows something truthful rather than "no expiry".
const SETUP_TOKEN_ASSUMED_LIFETIME_MS: i64 = 365 * 24 * 60 * 60 * 1000;

/// Read a `claude setup-token` access token from stdin — never from argv (see
/// `--token`'s doc comment in `main.rs`: an argv value is visible in `ps` and
/// lands in shell history). Prompts on stderr first when stdin is a
/// terminal; reads a bare line straight through when it is piped. Never logs
/// or prints the token back.
fn read_setup_token() -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        eprint!("Paste the token from `claude setup-token`: ");
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read setup-token from stdin")?;
    let token = line.trim().to_string();
    if token.is_empty() {
        bail!("no token read from stdin — pipe or paste the output of `claude setup-token`");
    }
    Ok(token)
}

/// Add an account from a `claude setup-token` credential instead of the
/// browser flow. Reuses every downstream layer [`login`] does — the same
/// [`login_route`] guard against a live server, the same [`fetch_profile`] /
/// [`finish_login`] pipeline — and replaces only the browser half: instead of
/// a PKCE round trip, the access token is read from stdin
/// ([`read_setup_token`]) and wrapped in a [`Tokens`] with `refresh_token:
/// None` — a setup-token mint carries only the `user:inference` scope, and
/// the Claude CLI that produces it never surfaces its refresh token to the
/// caller, so there is genuinely nothing to store.
///
/// `--account` is refused up front (before the port probe or the
/// stdin read): an inference-only token carries no identity for
/// [`assert_requested_identity`] to confirm, and an assertion that cannot be
/// evaluated must fail closed rather than pass — so this calls [`finish_login`]
/// directly, never `finish_login_checked`.
///
/// Because the scope is inference-only, [`fetch_profile`] will very likely
/// come back all-`None` (no email, no org) — expected, not an error. The
/// account name falls back to `--name`, then a prompt, exactly like the
/// browser flow's own fallback ([`prompt_account_name`]).
///
/// Always prints [`crate::proxy::NO_REFRESH_TOKEN_WARNING`] on success: this
/// account serves until its (assumed) one-year expiry and then goes dead,
/// with nothing to renew it, and — unlike the live-add wire route — this
/// offline path has no other caller that would say so.
/// The name a setup-token account falls back to when the operator declines to
/// type one: `unnamed`, or the first free `unnamed-N` when that is taken.
///
/// Deliberately NOT the browser flow's `account-{len+1}` fallback. That one is
/// derived from the account COUNT, so removing a row and adding another hands
/// out a name already in use — harmless for a credential that carries an email
/// and a uuid to be resolved by, and not harmless here. A setup-token account
/// has neither (the scope is inference-only), so
/// [`crate::identity::resolve`] can only match it on its NAME: two accounts
/// sharing one would resolve onto the same row, and the second login would
/// overwrite the first one's token instead of adding an account.
///
/// Scanning for the first free suffix rather than counting is what makes that
/// structurally impossible instead of merely unlikely.
fn unnamed_fallback(accounts: &[Account]) -> String {
    const BASE: &str = "unnamed";
    if !accounts.iter().any(|a| a.name == BASE) {
        return BASE.to_string();
    }
    // Start at 2: `unnamed` itself is the first, so the next one a human reads
    // should be `unnamed-2`, not `unnamed-1`.
    (2..)
        .map(|n| format!("{BASE}-{n}"))
        .find(|candidate| !accounts.iter().any(|a| &a.name == candidate))
        .expect("an unbounded range always yields a free name")
}

/// What one authenticated call said about a pasted setup token, before it is
/// written anywhere.
#[derive(Debug, PartialEq, Eq)]
pub enum SetupTokenCheck {
    /// Upstream accepted the credential (2xx, or a 429/5xx — a quota or server
    /// answer is still one that got past auth).
    Accepted,
    /// Upstream refused the credential (401 or 403). Nothing may be written.
    Rejected { status: u16 },
    /// The call never got an upstream answer (DNS, TLS, timeout). Proves
    /// nothing either way.
    Unreachable { message: String },
}

/// Make ONE authenticated call with `access_token` against `upstream` — the
/// same 1-token `POST /v1/messages` the warmer sends — and classify the answer.
///
/// This exists because a `claude setup-token` credential is the one kind of
/// account nothing else ever validates: it has no refresh token, so the prober
/// skips it, a refresh has nothing to send, and the proxy's 401 arm can only
/// sideline it after the first real request has already failed. A bad paste
/// therefore used to be accepted silently and reported `active` for as long
/// as the config held it. The Messages endpoint is used rather than the
/// profile endpoint because the profile fetch usually fails for an
/// inference-only scope whether the token is good or not, so it cannot tell
/// the two apart. The call is a real 1-token completion: it spends a trivial
/// amount of quota and starts the account's 5h window, which a login is about
/// to do anyway.
pub async fn verify_setup_token(upstream: &str, access_token: &str) -> SetupTokenCheck {
    use crate::warmer::AccountWarmer as _;
    let warmer = crate::warmer::LiveWarmer::new();
    match warmer
        .warm(access_token.to_string(), upstream.to_string())
        .await
    {
        Ok(_) => SetupTokenCheck::Accepted,
        Err(crate::warmer::WarmError {
            status: Some(status @ (401 | 403)),
            ..
        }) => SetupTokenCheck::Rejected { status },
        Err(crate::warmer::WarmError {
            status: Some(_), ..
        }) => SetupTokenCheck::Accepted,
        Err(crate::warmer::WarmError {
            status: None,
            message,
        }) => SetupTokenCheck::Unreachable { message },
    }
}

pub async fn login_with_token(
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
) -> anyhow::Result<String> {
    if account.is_some() {
        bail!(
            "--token cannot be combined with --account: a `claude setup-token` \
             credential carries only the user:inference scope, so it has no email and no \
             account id for that flag to confirm against — an identity assertion that \
             cannot be evaluated must fail closed rather than pass. Log in with --token alone \
             (use --name to choose the account name), or drop --token and use the browser flow \
             with --account instead. Nothing was written."
        );
    }

    let port = login_target_port(config_path);
    let incumbent = singleton::live_proxy_server(port);
    let route = login_route(config_path, incumbent, port, force).await?;
    if let LoginRoute::Refuse(msg) = route {
        bail!("{}", msg);
    }

    let access_token = read_setup_token()?;
    let tokens = Tokens {
        id_token: None,
        access_token,
        refresh_token: None,
        expires_at_ms: crate::now_ms() + SETUP_TOKEN_ASSUMED_LIFETIME_MS,
    };

    let mut config = load_or_default(config_path)?;

    // One authenticated call BEFORE anything is written: a rejected token is
    // refused here, because nothing downstream would ever catch it (see
    // `verify_setup_token`).
    match verify_setup_token(&config.upstream, &tokens.access_token).await {
        SetupTokenCheck::Accepted => {}
        SetupTokenCheck::Rejected { status } => bail!(
            "upstream refused this token (HTTP {status}) — it is expired, revoked or mistyped. \
             Mint a fresh one with `claude setup-token` and try again. Nothing was written."
        ),
        SetupTokenCheck::Unreachable { message } => {
            eprintln!(
                "[tcr] warning: could not reach {} to verify the token ({message}) — \
                 storing it unverified; a bad token will be marked `error` on its first request",
                config.upstream
            );
        }
    }

    let profile = fetch_profile(&tokens.access_token).await;
    // Always `Terminal`: `--token` reads the credential from stdin itself, so
    // it is refused in combination with `--non-interactive` (see `LoginArgs`
    // in `src/main.rs`) and this path never runs under a GUI parent.
    let resolved_name = mint_login_name(&config, &profile, None, name, LoginUi::Terminal)?;

    let result = finish_login(
        config_path,
        route,
        &mut config,
        &resolved_name,
        &tokens,
        profile,
    )
    .await;
    if result.is_ok() {
        eprintln!("[tcr] warning: {}", crate::proxy::NO_REFRESH_TOKEN_WARNING);
    }
    result
}

/// What importing the `claude` CLI's own login costs the user, in the words
/// they are told it in — printed by BOTH the explicit `--from-claude-code`
/// verb and the first-run auto-import, from this one constant so the two can
/// never drift into telling the user two different things.
///
/// It is a real cost, and it is stated rather than hidden: refresh tokens
/// rotate on every use (measured 2026-09-12, `docs/design/long-lived-tokens.md`),
/// so the first time tcr refreshes the imported credential, the copy the
/// `claude` CLI is still holding becomes dead and `claude` asks for a browser
/// login once.
pub const CLAUDE_CODE_IMPORT_CONSEQUENCE: &str = "this copies the login the `claude` CLI itself uses, and each refresh token is single-use, \
     so the next time tcr refreshes it, `claude` will ask you to log in again in the browser, \
     once. Sessions started with `tcr run` are not affected.";

/// The limits of sharing an imported Codex login, printed on every import.
/// Codex refresh tokens rotate and are single-use. tcr checks the recorded auth file before
/// updating it, but Codex does not take tcr's writer lock and can race the replacement.
pub const CODEX_IMPORT_CONSEQUENCE: &str = "this shares the login the `codex` CLI itself uses. \
    Codex refresh tokens are single-use: tcr checks auth.json before writing refreshed tokens \
    and can adopt replacement tokens for the same ChatGPT account. A simultaneous codex refresh \
    can still race this update. Signing in again with `codex login` in that folder revokes \
    this shared login; tcr may then need sign-in. `tcr login --codex` gives tcr a separate login.";

/// A standalone override names no Codex auth.json to synchronize after a refresh.
pub const CODEX_IMPORT_STANDALONE_CONSEQUENCE: &str = "this imports a shared Codex login \
    without automatic file synchronization. Codex refresh tokens are single-use: refreshing \
    here can leave the original store stale, and signing in again with `codex login` there \
    revokes this shared login. `tcr login --codex` gives tcr a separate login.";

/// The compatibility notice printed beside [`CODEX_IMPORT_CONSEQUENCE`] on every import:
/// the config now holds a Codex row, and a tcr built before Codex accounts cannot tell it from a
/// Claude one. What such a build does with the file depends on its age:
///
/// - one with the `credentialVersion` check (0.2.46 onward) refuses the whole file, because the
///   row is written at `config::CODEX_CREDENTIAL_VERSION`, so its server refuses to start and
///   leaves the file as it is;
/// - one from before that check (0.2.45 and older) never reads the key, and serves the Codex
///   login as a Claude account, sending its OpenAI tokens to Anthropic. Nothing in the file can
///   stop that build.
///
/// So the notice names both outcomes and asks for every tcr that may start on this config to be
/// updated before it next starts, the one TcrBar bundles (and runs ahead of `PATH`) included.
pub const CODEX_IMPORT_OLDER_PROXY: &str = "Update every tcr that may start on this config, \
    TcrBar's bundled one included, before it next starts: an older one refuses to start, or reads \
    this login as a Claude account.";

/// `" (max)"` for a known subscription, `""` for none — the plan is shown back
/// to the user as confirmation of WHICH login was taken, never acted on.
fn plan_suffix(subscription_type: Option<&str>) -> String {
    match subscription_type {
        Some(plan) if !plan.trim().is_empty() => format!(" ({plan})"),
        _ => String::new(),
    }
}

/// The Claude Code credential as a [`Tokens`], the shape every write path in
/// this module already takes.
///
/// A credential with no `expiresAt` is stamped as expiring NOW, so the very
/// first use refreshes it — correct here, because this credential carries a
/// refresh token. Deliberately NOT the year-long assumption
/// [`login_with_token`] stamps: that one has no refresh token and no way to
/// find out when it dies, and this one has both.
fn tokens_from_claude_code_login(login: crate::claude_code_login::ClaudeCodeLogin) -> Tokens {
    Tokens {
        id_token: None,
        access_token: login.access_token,
        refresh_token: login.refresh_token,
        expires_at_ms: login
            .expires_at
            .map_or_else(crate::now_ms, normalize_expires_at),
    }
}

/// `tcr login --from-claude-code` — import the login this machine's `claude`
/// CLI already holds, instead of opening a browser.
///
/// The explicit redo of what [`auto_import_claude_code_login`] does by itself
/// on a first run: same read, same write path, and it is the way back in after
/// a deliberate `tcr remove` (the auto path refuses to re-import a fleet the
/// user emptied on purpose).
///
/// `ui` decides how it speaks, as it does for [`login`]: the terminal gets the import line on
/// stderr, and [`LoginUi::Machine`] gets the import's cost as one `notice` event and then
/// `saved`, each exactly once, with no prose on stdout.
pub async fn login_from_claude_code(
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    ui.install();
    let Some(login) = crate::claude_code_login::read_claude_code_login()? else {
        bail!(
            "no Claude Code login found on this machine — nothing was written. Log in with the \
             `claude` CLI first, or use `tcr login` for the browser flow."
        );
    };
    let subscription_type = login.subscription_type.clone();
    let saved = import_claude_code_login(
        config_path,
        ImportInto::AnyFleet,
        force,
        name,
        account,
        login,
        ui,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("the import skipped a non-empty fleet it was told to accept"))?;
    match ui {
        LoginUi::Terminal => eprintln!(
            "[tcr] imported as '{saved}'{}; {CLAUDE_CODE_IMPORT_CONSEQUENCE}",
            plan_suffix(subscription_type.as_deref())
        ),
        LoginUi::Machine => {
            ui.emit(LoginEvent::Notice {
                text: CLAUDE_CODE_IMPORT_CONSEQUENCE,
            });
            ui.emit(LoginEvent::Saved { account: &saved });
        }
    }
    Ok(saved)
}

/// `tcr login --from-codex` — import the login this machine's `codex` CLI
/// already holds, read by [`crate::codex_login::read_codex_login`]. One of the
/// two Codex logins: `tcr login --codex` ([`crate::codex_cli_login::login`])
/// signs in one of tcr's own instead. A second account imported this way is
/// logged in with `CODEX_HOME=<dir> codex login` and imported from that dir
/// (`codex_home`).
///
/// This is the READ: which file ([`crate::codex_login::auth_path`], which
/// refuses `TCR_CODEX_AUTH` and `--codex-home` together), and what is in it —
/// no file, or a file that is not a ChatGPT login, each named by its path.
/// Everything after it is `import_codex_login`, split out so a test can hand
/// it a login without this process's environment deciding which one.
/// [`CODEX_IMPORT_CONSEQUENCE`] and [`CODEX_IMPORT_OLDER_PROXY`] print on every
/// import.
///
/// An auth.json import records its absolute parent (`codexHome`) for guarded token sharing
/// and TcrBar's re-import. A differently named override file is imported without a sync folder:
/// its parent does not identify the auth.json that Codex writes. `ui` decides how the
/// import speaks: the terminal gets its lines on stderr, then [`CODEX_IMPORT_RUN_LINE`], then
/// each notice that holds ([`crate::codex_local::codex_notices`]); [`LoginUi::Machine`] gets the
/// import's cost as one `notice` event, then those notices as events, then `saved` with the
/// provider and the plan. Nothing is said before the import is written, so a refusal is the
/// only line a refused import prints.
pub async fn login_from_codex(
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    codex_home: Option<&Path>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    ui.install();
    let path = crate::codex_login::auth_path(codex_home)?;
    let Some(login) = crate::codex_login::read_codex_login(&path)? else {
        bail!("{}", no_codex_login_message(&path));
    };
    let folder = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let folder = (path.file_name() == Some(std::ffi::OsStr::new(crate::paths::CODEX_AUTH_FILE)))
        .then(|| {
            std::path::absolute(folder)
                .with_context(|| format!("making {} absolute", folder.display()))
        })
        .transpose()?;
    let plan = tcr_status_wire::codex_plan_label(login.plan_type.as_deref());
    let first = holds_no_codex_row(config_path)?;
    let saved =
        import_codex_login(config_path, force, name, account, login, folder.as_deref()).await?;
    let first_account = if first {
        Some(read_first_codex_account_notice(config_path).await)
    } else {
        None
    };
    let notices = crate::codex_local::codex_notices(first_account);
    let consequence = if folder.is_some() {
        CODEX_IMPORT_CONSEQUENCE
    } else {
        CODEX_IMPORT_STANDALONE_CONSEQUENCE
    };
    match ui {
        LoginUi::Terminal => {
            // The plan as `tcr status` names it (`ChatGPT Pro`), not the raw claim (`pro`).
            eprintln!(
                "[tcr] imported as '{saved}'{}; {consequence} {CODEX_IMPORT_OLDER_PROXY}",
                plan.as_deref()
                    .map_or_else(String::new, |label| format!(" ({label})"))
            );
            eprintln!("{CODEX_IMPORT_RUN_LINE}");
            say_codex_notices(ui, &notices);
        }
        LoginUi::Machine => {
            let consequence = format!("{consequence} {CODEX_IMPORT_OLDER_PROXY}");
            ui.emit(LoginEvent::Notice { text: &consequence });
            say_codex_notices(ui, &notices);
            ui.emit(LoginEvent::SavedCodex {
                account: &saved,
                plan: plan.as_deref(),
            });
        }
    }
    Ok(saved)
}

/// What `tcr login --from-codex` says when there is no login at `path`: how to make one, and,
/// last, that a login codex keeps in the system keyring (its `keyring` and `auto` stores) has no
/// file tcr can read, which `tcr login --codex` does not need.
pub fn no_codex_login_message(path: &Path) -> String {
    format!(
        "no Codex login found at {} — nothing was written. Log in with `codex login` first (for \
         a second account: `mkdir -p <dir>`, `CODEX_HOME=<dir> codex login`, then `tcr login \
         --from-codex --codex-home <dir>`). If codex keeps this login in the system keyring, \
         which tcr does not read, use tcr login --codex instead, which needs no file.",
        path.display()
    )
}

/// Whether the config at `config_path` holds no Codex row in any state: enabled, disabled or
/// needing a re-login all count. Read before a Codex save, it decides
/// [`CODEX_FIRST_ACCOUNT_NOTICE`].
pub(crate) fn holds_no_codex_row(config_path: &Path) -> anyhow::Result<bool> {
    Ok(!load_or_default(config_path)?
        .accounts
        .iter()
        .any(|row| row.provider == crate::provider::Provider::Codex))
}

/// What a Codex save says when the config held no Codex row before it: whether tcr pools codex
/// from now on. [`CODEX_FIRST_ACCOUNT_NOTICE`] only when the proxy's leaf takes `chatgpt.com`:
/// the running proxy reports a leaf it minted that intercepts it, or nothing listens on `port`
/// and the next boot mints such a leaf because the JS teamclaude leaf is not on disk
/// (`js_leaf_on_disk`). Otherwise `codex is not pooled: {reason}`, the sentence `tcr status`
/// prints after `codexVerdict:`, with [`crate::codex_route::codex_route`]'s reason over the same
/// read, so it never claims what an unread proxy does.
///
/// Keyed on the leaf, not on the route's `pooled`: a running server may not have admitted the
/// new row yet, and the route over its payload would then say it holds no Codex account.
pub fn first_codex_account_notice(
    live: &Result<crate::status::StatusPayload, crate::cli::LiveStatusError>,
    port: u16,
    ca: &Path,
    ca_readable: bool,
    js_leaf_on_disk: bool,
) -> String {
    let leaf_takes_chatgpt = match live {
        Ok(payload) => payload.mitm.as_ref().is_some_and(|mitm| {
            mitm.leaf == crate::status::MitmStatus::LEAF_MINTED
                && mitm.intercepts(crate::provider::CODEX_HOST)
        }),
        Err(crate::cli::LiveStatusError::NoServer) => !js_leaf_on_disk,
        Err(_) => false,
    };
    let codex_rows = live
        .as_ref()
        .map_or(0, crate::codex_route::listed_codex_accounts);
    let route = crate::codex_route::codex_route(live.as_ref(), port, ca, ca_readable, codex_rows);
    match route.reason {
        Some(reason) if !leaf_takes_chatgpt => format!("codex is not pooled: {reason}"),
        _ => CODEX_FIRST_ACCOUNT_NOTICE.to_string(),
    }
}

/// [`first_codex_account_notice`] over one read of the proxy the config at `config_path` names,
/// tcr's CA, and the JS teamclaude leaf's files: `teamclaude-leaf.pem` and `teamclaude-leaf.key`
/// beside the default config, the pair `mitm::load_tls_in` serves first when both are there.
/// Existence checks, never a read of either file.
///
/// It follows a save that already happened, so it cannot fail the login: a config it cannot read
/// is read as the default one, as [`login_target_port`] reads it.
pub(crate) async fn read_first_codex_account_notice(config_path: &Path) -> String {
    let config = config::load(config_path).unwrap_or_else(|_| {
        serde_json::from_str("{}").expect("empty object is a valid default config")
    });
    let live = crate::cli::fetch_live_status(&config).await;
    let ca = crate::mitm::minted_ca_path();
    let dir = config::default_path()
        .parent()
        .map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
    let js_leaf_on_disk = dir.join(crate::legacy_names::JS_LEAF_CERT).is_file()
        && dir.join(crate::legacy_names::JS_LEAF_KEY).is_file();
    first_codex_account_notice(&live, config.proxy.port, &ca, ca.is_file(), js_leaf_on_disk)
}

/// Say each of `notices`, in order: a `notice` event apiece in [`LoginUi::Machine`]; a stderr
/// line apiece in the terminal, on the `[tcr] ` rung or, for a warning, `[tcr] warning: `.
pub(crate) fn say_codex_notices(ui: LoginUi, notices: &[crate::codex_local::CodexNotice]) {
    for notice in notices {
        match ui {
            LoginUi::Machine => ui.emit(LoginEvent::Notice { text: &notice.text }),
            LoginUi::Terminal if notice.warning => eprintln!("[tcr] warning: {}", notice.text),
            LoginUi::Terminal => eprintln!("[tcr] {}", notice.text),
        }
    }
}

/// The route check `tcr login --codex` runs before codex does, so a proxy that cannot take the
/// login refuses it before the person spends a browser sign-in on it: the route, as every login
/// checks it, and on the live route the provider gate [`import_codex_login`] applies, naming
/// `tcr login --codex` as the command to run again.
pub(crate) async fn codex_login_route_check(config_path: &Path, force: bool) -> anyhow::Result<()> {
    let port = login_target_port(config_path);
    let incumbent = singleton::live_proxy_server(port);
    let pid = incumbent.map(|incumbent| incumbent.pid);
    let (route, providers) =
        login_route_with_providers(config_path, incumbent, port, force, LOGIN_CODEX_COMMAND)
            .await?;
    if let LoginRoute::Refuse(msg) = route {
        bail!("{}", msg);
    }
    if route == LoginRoute::Live && !providers.contains(&crate::provider::Provider::Codex) {
        bail!(
            "{}",
            codex_live_route_refusal(port, pid, LOGIN_CODEX_COMMAND)
        );
    }
    Ok(())
}

/// The write half of `tcr login --from-codex`: everything after the read, given
/// the login. The Codex twin of [`import_claude_code_login`]: the same route
/// probe and persist path every other login takes ([`login_route`],
/// [`finish_login_account`]), so a running proxy, `--force` and a corrupt config
/// behave as they do for `tcr login`. The row is [`codex_account_row`]'s, with
/// `provider: codex`.
///
/// In order, and nothing is written before the last step:
///
/// 0. **A FedRAMP login is refused**, before anything else and offline: its token
///    carries `chatgpt_account_is_fedramp` ([`crate::codex_login::CodexLogin::is_fedramp`]),
///    so codex marks its requests `X-OpenAI-Fedramp` and may route them away from
///    chatgpt.com, where tcr would send them.
/// 1. `account` (`--account`) — a guard against importing the wrong `CODEX_HOME`.
///    A name that is a row, exactly as `tcr accounts` prints it, must be a Codex
///    row this login is ([`crate::identity::same_identity`] against the row
///    [`codex_account_row`] would write); a Claude row is refused with
///    [`LoginRefusal::ACCOUNT_IS_CLAUDE`], since no Codex login re-signs it. A
///    name that is no row must be the login's email. All of it before the route
///    probe, so a refusal asks nothing of anyone.
/// 2. The route.
/// 3. **The provider gate.** On the LIVE route the server must have named
///    `codex` among the providers it can store ([`crate::proxy::PROVIDERS_HEADER`],
///    read off the probe's own answer). A server that did not predates Codex
///    accounts: it would drop the row's `provider`, store this login as a Claude
///    account and from then on send its OpenAI tokens to Anthropic — a row a later
///    restart does not heal. Refused, and `--force` does not get past it: a confirmed
///    live route is never traded for the file ([`login_guard_refusal_for`]), and the file
///    would not help either — at its next start that server refuses it if it has the
///    `credentialVersion` check, and reads the login as a Claude account if it predates it.
/// 4. **The row this login already is** ([`existing_codex_row`]), if any. A
///    re-import updates that row and renames nothing, so it keeps the row's name
///    and refuses a different `--name`. And it is refused outright when tcr holds a
///    NEWER credential for it than the import. A stale file can remain when guarded
///    write-back could not complete; writing it over tcr's live pair would kill the account
///    at its next refresh.
/// 5. Otherwise the name [`mint_codex_login_name`] picks.
/// 6. **The workspace check**, last before the write because it is the one step that
///    sends the login's bearer anywhere: `GET {codexUpstream}/backend-api/wham/accounts/check`
///    ([`crate::codex_login::check_workspace_routing`]). A workspace routed to another
///    backend origin or by a residency override is refused, and so is any check that
///    cannot complete: tcr sends every pooled Codex request to `codexUpstream`, so it pools
///    only a login served there. Then the write.
///
/// `codex_home` is the folder the login was read from, recorded on the row as `codexHome`;
/// `None` for a login of tcr's own, which leaves no folder to re-import from.
pub(crate) async fn import_codex_login(
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    login: crate::codex_login::CodexLogin,
    codex_home: Option<&Path>,
) -> anyhow::Result<String> {
    import_codex_login_as(
        LOGIN_FROM_CODEX_COMMAND,
        config_path,
        force,
        name,
        account,
        login,
        codex_home,
    )
    .await
}

/// [`import_codex_login`], for the login `command` names, which a refusal tells the user to run
/// again: `tcr login --codex` writes through here too.
pub(crate) async fn import_codex_login_as(
    command: &str,
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    login: crate::codex_login::CodexLogin,
    codex_home: Option<&Path>,
) -> anyhow::Result<String> {
    if login.is_fedramp {
        return Err(LoginRefusal {
            code: LoginRefusal::CODEX_FEDRAMP,
            reason: CODEX_FEDRAMP_REFUSAL.to_string(),
        }
        .into());
    }
    if let Some(wanted) = account {
        let config = load_or_default(config_path)?;
        let is_this_login = match crate::identity::match_one(&config.accounts, wanted) {
            crate::identity::Match::One(idx) => {
                let row = &config.accounts[idx];
                match row.provider {
                    crate::provider::Provider::Anthropic => {
                        return Err(LoginRefusal {
                            code: LoginRefusal::ACCOUNT_IS_CLAUDE,
                            reason: claude_account_codex_import_refusal(&row.name),
                        }
                        .into());
                    }
                    crate::provider::Provider::Codex => codex_login_is_row(row, &login),
                }
            }
            crate::identity::Match::None => login.email.as_deref() == Some(wanted),
            crate::identity::Match::Ambiguous(names) => {
                bail!("{}", crate::cli::ambiguous_query_message(wanted, &names))
            }
        };
        if !is_this_login {
            bail!("the Codex login on this machine is not '{wanted}'; nothing was written.");
        }
    }
    let port = login_target_port(config_path);
    let incumbent = singleton::live_proxy_server(port);
    let pid = incumbent.map(|incumbent| incumbent.pid);
    let (route, providers) =
        login_route_with_providers(config_path, incumbent, port, force, command).await?;
    if let LoginRoute::Refuse(msg) = route {
        bail!("{}", msg);
    }
    if route == LoginRoute::Live && !providers.contains(&crate::provider::Provider::Codex) {
        bail!("{}", codex_live_route_refusal(port, pid, command));
    }
    let config = load_or_default(config_path)?;
    let resolved_name = match existing_codex_row(&config, &login) {
        Some(row) => {
            if let Some(explicit) = name.filter(|explicit| *explicit != row.name) {
                bail!(
                    "this login is already saved as '{}' — a re-import updates that row and \
                     renames nothing, so --name '{explicit}' cannot apply. Drop --name. Nothing \
                     was written.",
                    row.name
                );
            }
            if holds_newer_credential(row.expires_at, login.expires_at_ms) {
                bail!(
                    "tcr already holds a newer credential for this account than `codex` does; \
                     run `codex login` first if you meant to replace it; nothing was written."
                );
            }
            row.name.clone()
        }
        None => mint_codex_login_name(
            &config,
            login.email.as_deref(),
            login.user_id.as_deref(),
            login.account_id.as_deref(),
            name,
        )?,
    };
    // The check carries the login's bearer and account id, so it follows no redirect: a 3xx
    // would carry the account id (reqwest drops only the bearer across origins) to an origin
    // whose answer would then decide the import. A redirect is refused as an unconfirmed check.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the client for the Codex workspace check")?;
    if let Err(refusal) =
        crate::codex_login::check_workspace_routing(&client, &config.codex_upstream, &login).await
    {
        return Err(codex_workspace_refusal(refusal).into());
    }
    finish_login_account(
        config_path,
        route,
        &config,
        codex_account_row(resolved_name, login, codex_home),
    )
    .await
}

/// [`import_codex_login`]'s refusal of a FedRAMP login, before it does anything else.
const CODEX_FEDRAMP_REFUSAL: &str = "this Codex login belongs to a FedRAMP ChatGPT workspace (its \
     token carries chatgpt_account_is_fedramp): codex marks such a workspace's requests \
     X-OpenAI-Fedramp and may route them to the workspace's own backend, and tcr pools a Codex \
     login only when it is served at https://chatgpt.com with no routing of its own. Nothing was \
     written.";

/// [`import_codex_login`]'s refusal of a login the workspace check did not clear
/// ([`crate::codex_login::check_workspace_routing`]), coded by which way it failed: a
/// workspace served elsewhere, or a check that could not complete.
fn codex_workspace_refusal(refusal: crate::codex_login::WorkspaceRefusal) -> LoginRefusal {
    match refusal {
        crate::codex_login::WorkspaceRefusal::NotPoolable(said) => LoginRefusal {
            code: LoginRefusal::CODEX_ROUTED,
            reason: format!(
                "this Codex login's ChatGPT workspace is not served at https://chatgpt.com with no \
                 routing of its own ({said}): codex sends that workspace's traffic to its own \
                 backend, or routes it by residency, and tcr pools a Codex login only at \
                 https://chatgpt.com. Nothing was written."
            ),
        },
        crate::codex_login::WorkspaceRefusal::Unconfirmed(why) => LoginRefusal {
            code: LoginRefusal::CODEX_UNCONFIRMED,
            reason: format!(
                "tcr could not confirm that this Codex login's ChatGPT workspace is served at \
                 https://chatgpt.com ({why}), and imports a Codex login only once that check \
                 answers: a FedRAMP or data-residency workspace must not be pooled. Nothing was \
                 written."
            ),
        },
    }
}

/// [`import_codex_login`]'s refusal of a live proxy that did not name `codex`
/// among the providers it can store. `pid` is the incumbent process detection
/// found on the port, when it found one; `command` is the login that was run,
/// named as the one to run again.
fn codex_live_route_refusal(port: u16, pid: Option<u32>, command: &str) -> String {
    let pid = pid.map_or_else(|| "pid unknown".to_string(), |pid| format!("pid {pid}"));
    format!(
        "the proxy on :{port} ({pid}) predates Codex accounts and would store this login as a \
         Claude account and send its token to Anthropic — nothing was written. Restart the proxy \
         on this build (update TcrBar first if it supervises the proxy), then re-run `{command}`. \
         --force does not get past this: it never trades a confirmed live route for a file \
         write, and that proxy would refuse the file at its next start, or read it the same way \
         if it is older still."
    )
}

/// The row of `config` this Codex login already is, resolved the way the write
/// resolves its row: [`crate::identity::resolve`] over a Codex probe carrying the
/// login's email, its ChatGPT user and its workspace, so only a Codex row of that
/// user in that workspace can match. `None` when no row is this login, and when
/// two are — a tie the write refuses itself.
fn existing_codex_row<'a>(
    config: &'a Config,
    login: &crate::codex_login::CodexLogin,
) -> Option<&'a Account> {
    let this_login = crate::identity::probe(
        crate::provider::Provider::Codex,
        login.email.as_deref().unwrap_or_default(),
        login.user_id.clone(),
        login.account_id.clone(),
        None,
    );
    match crate::identity::resolve(config.accounts.iter().enumerate(), &this_login) {
        crate::identity::Resolved::One(index) => config.accounts.get(index),
        crate::identity::Resolved::None | crate::identity::Resolved::Many => None,
    }
}

/// Whether the credential tcr already stores (`stored`, its `expiresAt`) is
/// NEWER than the one being imported (`import`). Only a provable "newer" counts:
/// an expiry missing on either side is no evidence, and an equal one is the same
/// credential imported twice, which is harmless.
fn holds_newer_credential(stored: Option<i64>, import: Option<i64>) -> bool {
    matches!((stored, import), (Some(stored), Some(import)) if stored > import)
}

/// The config row a Codex login becomes: `provider: codex`, its identity a ChatGPT
/// user plus a workspace — the user (`chatgpt_user_id`) as `accountUuid` and the
/// workspace (the ChatGPT account id, which tcr sends as `chatgpt-account-id`) as
/// `orgUuid`, the pair [`crate::identity::same_identity`] compares within one
/// provider — and the plan as its `organizationType`. So one person in two
/// workspaces is two accounts, and so are two seats of one workspace: each pair
/// holds its own token and its own quota. No org name: a workspace is named by its
/// id alone. `codex_home` is the folder the login was imported from, when it was.
fn codex_account_row(
    name: String,
    login: crate::codex_login::CodexLogin,
    codex_home: Option<&Path>,
) -> Account {
    Account {
        name,
        account_type: crate::config::AccountType::OAuth,
        provider: crate::provider::Provider::Codex,
        account_uuid: login.user_id,
        org_uuid: login.account_id,
        org_name: None,
        access_token: login.access_token,
        refresh_token: Some(login.refresh_token),
        expires_at: login.expires_at_ms,
        priority: None,
        switch_threshold: None,
        disabled: None,
        groups: None,
        organization_type: login.plan_type,
        rate_limit_tier: None,
        seat_tier: None,
        egress: crate::config::Egress::Local,
        egress_strict: false,
        codex_home: codex_home.map(|folder| folder.display().to_string()),
        extra: serde_json::Map::new(),
    }
}

/// Whether `login` is the account `row` holds, by the rule the write resolves its row by:
/// [`crate::identity::same_identity`] against the row [`codex_account_row`] would write, so a
/// Codex row of the same ChatGPT user in the same workspace.
///
/// A login that does not name both its user and its workspace is never the row's. Without
/// them `same_identity` falls back to comparing names, and the probe row here carries `row`'s
/// own name, so any such login would pass.
pub(crate) fn codex_login_is_row(row: &Account, login: &crate::codex_login::CodexLogin) -> bool {
    login.user_id.is_some()
        && login.account_id.is_some()
        && crate::identity::same_identity(
            row,
            &codex_account_row(row.name.clone(), login.clone(), None),
        )
}

/// The name a Codex import writes under.
///
/// `--name` wins, and must be free. Else the email; if an existing row of ANY
/// provider already holds it, `<email>/codex`, mirroring the `email/<org-slug>`
/// rule for duplicated Anthropic emails, then `-2`, `-3`, … as
/// [`crate::identity::mint_name`] numbers its own. Names are unique across the
/// file (`config::migrate_duplicate_names`).
///
/// Free means free FOR THIS LOGIN: no row holds the name, or the row holding it
/// is this same Codex account by [`crate::identity::same_identity`] (its ChatGPT
/// user, `user_id`, in its workspace, `workspace_id`), the rule the write itself
/// resolves its row by. So a re-import of the same login keeps its name and
/// updates its row in place, while the same email in a second workspace (a
/// personal and a Team workspace) gets a name of its own: the write would ADD
/// that row, and two rows of one name is the state `migrate_duplicate_names`
/// exists to repair. `--force` does not waive this — it overrides a running
/// proxy's refusal, and no route makes a shared name safe.
fn mint_codex_login_name(
    config: &Config,
    email: Option<&str>,
    user_id: Option<&str>,
    workspace_id: Option<&str>,
    name: Option<&str>,
) -> anyhow::Result<String> {
    let free = |candidate: &str| {
        let this_login = crate::identity::probe(
            crate::provider::Provider::Codex,
            candidate,
            user_id.map(str::to_string),
            workspace_id.map(str::to_string),
            None,
        );
        config
            .accounts
            .iter()
            .filter(|a| a.name == candidate)
            .all(|a| crate::identity::same_identity(a, &this_login))
    };
    if let Some(explicit) = name {
        if !free(explicit) {
            bail!(
                "'{explicit}' is already the name of another account — account names are unique. \
                 Pick a different --name. Nothing was written."
            );
        }
        return Ok(explicit.to_string());
    }
    let Some(email) = email.filter(|e| !e.is_empty()) else {
        bail!("the Codex id token carries no email; pass --name <name>.");
    };
    if free(email) {
        return Ok(email.to_string());
    }
    let base = format!("{email}{}codex", crate::identity::ORG_SEPARATOR);
    Ok(std::iter::once(base.clone())
        .chain((2..).map(|n| format!("{base}-{n}")))
        .find(|candidate| free(candidate))
        .expect("an unbounded range always yields a free name"))
}

/// Import this machine's Claude Code login on a first run, if there is one.
///
/// Called by the verbs that read the config when the fleet is EMPTY — the
/// config was just created, or it exists with no accounts in it. Returns the
/// account name on success and `None` every other way, and it is `None` that
/// makes the caller print the ordinary `no accounts configured` hint.
///
/// Best-effort by construction: a machine with no Claude Code login, a
/// Keychain read that fails, a profile fetch that fails, a live proxy that
/// refuses the add route — each is ONE warning line on stderr and the verb
/// carries on with zero accounts. None of them is an error worth failing
/// `tcr status` over, which TcrBar renders as a failed poll. One attempt per
/// process, no retry loop.
///
/// The caller is responsible for only calling this on an empty fleet: an
/// import must never run over a config that already has accounts, or a user
/// who removed their account on purpose would find it back. `--from-claude-code`
/// is the explicit way to redo it.
pub async fn auto_import_claude_code_login(config_path: &Path) -> Option<String> {
    let login = match crate::claude_code_login::read_claude_code_login() {
        Ok(Some(login)) => login,
        Ok(None) => return None,
        Err(err) => {
            eprintln!(
                "[tcr] warning: could not read this machine's Claude Code login ({err:#}) — \
                 continuing with no accounts"
            );
            return None;
        }
    };
    let subscription_type = login.subscription_type.clone();
    // `LoginUi::Machine`: no prompt (nobody typed `tcr login` here) and no
    // prose on STDOUT — `tcr status --json` and `tcr accounts --json` are piped
    // into `jq` and decoded by TcrBar, so everything this path says goes to
    // stderr.
    match import_claude_code_login(
        config_path,
        ImportInto::EmptyFleetOnly,
        false,
        None,
        None,
        login,
        LoginUi::Machine,
    )
    .await
    {
        // Another `tcr` process (the server booting while `tcr status` polls
        // it, typically) imported first. Its row is the import; adding ours
        // too would be the `unnamed` + `unnamed-2` duplicate.
        Ok(None) => None,
        Ok(Some(saved)) => {
            eprintln!(
                "[tcr] imported '{saved}'{} from your Claude Code login; \
                 {CLAUDE_CODE_IMPORT_CONSEQUENCE}",
                plan_suffix(subscription_type.as_deref())
            );
            Some(saved)
        }
        Err(err) => {
            eprintln!(
                "[tcr] warning: could not import this machine's Claude Code login ({err:#}) — \
                 continuing with no accounts"
            );
            None
        }
    }
}

/// The write both Claude Code import paths share: the SAME route probe, name
/// mint and finishing path a browser login uses ([`login`]), so account-name
/// collision, the live-proxy add route and `--force` behave identically. The
/// only thing that differs is where the credential came from.
///
/// `--account` is honoured, unlike [`login_with_token`]'s refusal of it: this
/// credential carries the full `user:profile` scope, so `fetch_profile` returns
/// a real identity for [`assert_requested_identity`] to confirm the requested
/// row against. There is nothing here that cannot be evaluated.
///
/// Returns `Ok(None)` only under [`ImportInto::EmptyFleetOnly`], when the
/// config it loads already has an account in it.
async fn import_claude_code_login(
    config_path: &Path,
    into: ImportInto,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    login: crate::claude_code_login::ClaudeCodeLogin,
    ui: LoginUi,
) -> anyhow::Result<Option<String>> {
    ui.install();
    let port = login_target_port(config_path);
    let incumbent = singleton::live_proxy_server(port);
    let route = login_route_for(
        config_path,
        incumbent,
        port,
        force,
        LOGIN_FROM_CLAUDE_CODE_COMMAND,
    )
    .await?;
    if let LoginRoute::Refuse(msg) = route {
        bail!("{}", msg);
    }

    let tokens = tokens_from_claude_code_login(login);

    let mut config = load_or_default(config_path)?;
    // The caller's "fleet is empty" check ran on an earlier read, and the route
    // probe above can take a while against a server that is still booting.
    // Another `tcr` process may have imported in that gap, and minting a name
    // against its row would add the same login a second time.
    // A fleet emptied by `tcr remove` says so in the file; refilling it here
    // would undo that delete on the next poll.
    if into == ImportInto::EmptyFleetOnly
        && (!config.accounts.is_empty() || config.refuses_claude_code_import())
    {
        return Ok(None);
    }
    let profile = fetch_profile(&tokens.access_token).await;
    let resolved_name = mint_login_name(&config, &profile, account, name, ui)?;

    finish_login_checked(
        config_path,
        route,
        &mut config,
        &resolved_name,
        &tokens,
        profile,
        account,
    )
    .await
    .map(Some)
}

/// Which configs [`import_claude_code_login`] may write a Claude Code login into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportInto {
    /// `tcr login --from-claude-code`: the user asked, so any config.
    AnyFleet,
    /// The automatic first-run import: only a config with no accounts, checked
    /// again on the read the write is based on.
    EmptyFleetOnly,
}

/// What `tcr login --account` says to a row that is not a Claude account, before it does
/// anything else ([`refuse_a_non_claude_row`]): the row, why `tcr login` cannot sign it in, the
/// two commands that can (a login of tcr's own, or a re-import), and that nothing was written.
pub fn codex_account_login_refusal(name: &str) -> String {
    format!(
        "'{name}' is a Codex account; `tcr login` signs in Claude accounts only. Sign it in again \
         with `tcr login --codex --account {name}`, or re-import it with `tcr login --from-codex \
         --account {name}`. Nothing was written."
    )
}

/// Refuse a `--account` row that a Claude sign-in can never confirm: anything but a Claude
/// account. Both a browser login and a Claude Code import come back with an Anthropic identity,
/// which no Codex row matches ([`assert_requested_identity`]'s probe), so going on would end in
/// a mismatch that tells the user to sign out and retry the same command. Called before the
/// browser opens, and again by the identity check itself.
fn refuse_a_non_claude_row(account: &Account) -> anyhow::Result<()> {
    match account.provider {
        crate::provider::Provider::Anthropic => Ok(()),
        crate::provider::Provider::Codex => Err(LoginRefusal {
            code: LoginRefusal::ACCOUNT_IS_CODEX,
            reason: codex_account_login_refusal(&account.name),
        }
        .into()),
    }
}

/// What `tcr login --codex --account` says to a Claude row: a ChatGPT sign-in can never confirm
/// it, so it is refused before codex runs, naming the command that can.
pub fn claude_account_codex_login_refusal(name: &str) -> String {
    format!(
        "'{name}' is a Claude account; `tcr login --codex` signs in Codex accounts only. Sign it \
         in again with `tcr login --account {name}`. Nothing was written."
    )
}

/// What `tcr login --from-codex --account` says to a Claude row, before the route probe and any
/// write: a Codex import can never re-sign it.
pub fn claude_account_codex_import_refusal(name: &str) -> String {
    format!(
        "'{name}' is a Claude account; `tcr login --from-codex` imports Codex accounts only. Sign \
         it in again with `tcr login --account {name}`. Nothing was written."
    )
}

/// The row `tcr login --codex --account <query>` re-signs: `query` resolved by the rule every
/// verb's `--account` uses ([`crate::cli::resolve_account`], an exact row name), refused with
/// [`LoginRefusal::ACCOUNT_IS_CLAUDE`] when it names a Claude row. Runs before anything else the
/// login does, so a refusal spawns nothing and writes nothing.
pub fn codex_relogin_target(config: &Config, query: &str) -> anyhow::Result<usize> {
    let idx = crate::cli::resolve_account(&config.accounts, query)?;
    let row = &config.accounts[idx];
    match row.provider {
        crate::provider::Provider::Codex => Ok(idx),
        crate::provider::Provider::Anthropic => Err(LoginRefusal {
            code: LoginRefusal::ACCOUNT_IS_CLAUDE,
            reason: claude_account_codex_login_refusal(&row.name),
        }
        .into()),
    }
}

/// The refusal of a `tcr login --codex --account <name>` whose ChatGPT login came back as
/// someone else: `got` is that login's email (or a stand-in when it has none). The browser
/// is the usual cause, already signed in to chatgpt.com as another person, which is the remedy
/// it names.
pub fn codex_identity_refusal(got: &str, name: &str) -> LoginRefusal {
    LoginRefusal {
        code: LoginRefusal::CODEX_IDENTITY,
        reason: format!(
            "the ChatGPT login that came back is {got}, not the one '{name}' holds. Sign out of \
             chatgpt.com in the browser first, or use a private window, then retry `tcr login \
             --codex --account {name}`. Nothing was written."
        ),
    }
}

/// The refusal of a `tcr login --codex --account <name>` whose login is the row's own ChatGPT
/// user in another workspace: the same person, so both workspaces are known and differ, and the
/// remedy is to pick the row's workspace when ChatGPT asks.
///
/// `email` is the login's (or `None`); `got` and `want` are the login's and the row's workspace
/// ids, shown as their first 8 hex digits; each plan is a raw plan word, labelled the way
/// `tcr status` labels it, and its parenthesis is dropped when it is unknown.
pub fn codex_relogin_workspace_refusal(
    email: Option<&str>,
    got: &str,
    got_plan: Option<&str>,
    name: &str,
    want: &str,
    want_plan: Option<&str>,
) -> LoginRefusal {
    let short =
        |id: &str| -> String { id.chars().filter(char::is_ascii_hexdigit).take(8).collect() };
    let plan = |raw: Option<&str>| {
        tcr_status_wire::codex_plan_label(raw)
            .map_or_else(String::new, |label| format!(" ({label})"))
    };
    LoginRefusal {
        code: LoginRefusal::CODEX_IDENTITY,
        reason: format!(
            "the ChatGPT login that came back is {} in ChatGPT workspace {}{}, but '{name}' holds \
             workspace {}{}. Retry `tcr login --codex --account {name}` and choose that workspace \
             when ChatGPT asks which one to use. Nothing was written.",
            email.unwrap_or("the same ChatGPT user"),
            short(got),
            plan(got_plan),
            short(want),
            plan(want_plan),
        ),
    }
}

/// Whether `s` is plausibly an email address — no whitespace, and exactly one
/// `@` with at least one character on each side. Deliberately not a full RFC
/// 5322 validator: this only ever gates whether a value is safe to hand to
/// `login_hint` (see `build_login_flow`'s doc-comment), where the failure mode
/// of being too strict is "no pre-fill" and the failure mode of being too
/// loose is an unvalidated string on an endpoint already known to reject
/// malformed params outright.
fn looks_like_email(s: &str) -> bool {
    if s.contains(char::is_whitespace) {
        return false;
    }
    match s.split_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.is_empty() && !domain.contains('@'),
        None => false,
    }
}

/// [`finish_login`], but first asserting the requested `--account` identity
/// when one was given. Split out so the assertion + write sequence `login()`
/// runs is directly testable without a real browser or network round trip —
/// exactly the reason [`finish_login`] itself was split from [`login`].
#[allow(clippy::too_many_arguments)]
async fn finish_login_checked(
    config_path: &Path,
    route: LoginRoute,
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    profile: Profile,
    requested_account: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(query) = requested_account {
        assert_requested_identity(config, query, &profile)?;
    }
    finish_login(config_path, route, config, name, tokens, profile).await
}

/// The load-bearing half of `tcr login --account <query>`:
/// resolve `query` against `config` via [`crate::cli::resolve_account`]
/// — the exact rule `tcr enable`/`tcr disable` and TcrBar's row buttons
/// already rely on, not a second copy of it — then refuse to proceed unless
/// the identity that came back from `fetch_profile` resolves, through
/// [`crate::identity::resolve`], to that SAME row.
///
/// Deliberately reuses `identity::resolve`/`identity::probe` — the exact
/// functions [`upsert_account`] writes through — rather than a hand-rolled
/// comparison: an account UUID alone identifies the *person*, not the
/// account. The same person routinely holds more than one organization (a
/// corporate Pro org and a personal Max org), each with its own token and
/// quota (`src/identity.rs:3-8`), so a `uuid`-only check cannot tell those
/// apart — it would happily confirm a re-login intended for the Corp row
/// against a browser session signed into Personal, because both carry the
/// same `uuid`. `identity::resolve` folds the org discriminator in on top of
/// the uuid, which is why it — not a second copy of only half its logic — is
/// what both the check and the write need to agree on.
///
/// Called strictly BEFORE any write path
/// ([`finish_login`]/[`persist_via_file`]/[`persist_via_account`]): on
/// mismatch, on an unresolvable/ambiguous requested account, on a requested
/// row that is not a Claude account ([`refuse_a_non_claude_row`]), or on a profile
/// that carries neither an email nor a uuid (`fetch_profile`'s all-`None`
/// failure return, `src/oauth.rs:649-654` — an assertion that cannot be
/// evaluated must fail closed, not pass by default), nothing is written.
fn assert_requested_identity(
    config: &Config,
    query: &str,
    profile: &Profile,
) -> anyhow::Result<()> {
    let idx = crate::cli::resolve_account(&config.accounts, query)?;
    let requested = &config.accounts[idx];
    refuse_a_non_claude_row(requested)?;

    if profile.email.is_none() && profile.account_uuid.is_none() {
        bail!(
            "could not confirm the identity that just authenticated (the profile carried no \
             email and no account id) — refusing to touch '{}'. Nothing was changed.",
            requested.name
        );
    }

    // The identity a fresh login resolves to, computed the SAME way
    // `upsert_account` resolves one for a write: build a probe carrying only
    // what the browser told us, and resolve it against the WHOLE fleet, not
    // just `requested`. Comparing against the whole fleet (rather than just
    // asking "does `requested` match the probe?") is what makes the org
    // discriminator load-bearing: a uuid-only local comparison against
    // `requested` alone would still pass when Corp was requested and the
    // browser returned Personal, because it never looks at whether that
    // identity actually belongs to a DIFFERENT row.
    let probe_name = profile.email.as_deref().unwrap_or(&requested.name);
    // The browser just authenticated an ANTHROPIC account, so this probe is one: a
    // requested Codex row is never confirmed by it (`identity::same_identity`).
    let probe = crate::identity::probe(
        crate::provider::Provider::Anthropic,
        probe_name,
        profile.account_uuid.clone(),
        profile.org_uuid.clone(),
        profile.org_name.clone(),
    );
    let resolved = crate::identity::resolve(config.accounts.iter().enumerate(), &probe);
    let resolved_idx = match resolved {
        crate::identity::Resolved::One(i) => Some(i),
        crate::identity::Resolved::None | crate::identity::Resolved::Many => None,
    };

    if resolved_idx != Some(idx) {
        let got = match resolved_idx {
            // The identity resolved cleanly, just to a DIFFERENT existing
            // row — name that row, it is the actionable fact.
            Some(other) => config.accounts[other].name.clone(),
            None if requested.account_uuid.is_none() && !requested.name.contains('@') => {
                // `requested` has no stored account id, and its own display
                // name isn't email-shaped (e.g. an account named "work"), so
                // there is no stored fact to compare the browser's identity
                // against at all — this is not "wrong browser session", it
                // is "nothing on file to check against". Telling the
                // operator to sign out cannot help here; say so honestly.
                bail!(
                    "'{}' has no stored account id, and its name is not an email address, so \
                     `--account` cannot confirm the browser authenticated as the right person — \
                     there is nothing on file to compare against. Nothing was written. Run `tcr \
                     login` once WITHOUT --account, confirm from the printed email that it is the \
                     account you intended, then either rename this entry to that email or use \
                     that email with --account from now on.",
                    requested.name
                );
            }
            None => {
                let email = profile
                    .email
                    .as_deref()
                    .unwrap_or("an account with no email in its profile");
                match profile.org_name.as_deref().or(profile.org_uuid.as_deref()) {
                    Some(org) => format!("{email} (org {org})"),
                    None => email.to_string(),
                }
            }
        };
        bail!(
            "requested re-login for '{}', but the browser authenticated as '{got}' instead — \
             nothing was written. Sign out of that account in the browser first, or use a \
             private/incognito window, then retry `tcr login --account {}`.",
            requested.name,
            query
        );
    }

    Ok(())
}

/// The file half of a login: a surgical [`config::save_account`] write of the
/// finished `account` row, sharing the exact identity-resolution and backfill
/// semantics [`upsert_account`] used to apply in memory.
///
/// Used to be `upsert_account` (mutating the in-memory `config` this
/// process loaded at the top of `login()`) followed by a whole-file
/// [`config::save`] of that snapshot. That snapshot is taken BEFORE the
/// profile fetch, the browser round-trip and a possibly-unbounded stdin
/// prompt — the exact window `persist_via_account`'s doc-comment names —
/// so a whole-file write here silently reverted anything an out-of-band
/// writer (a live `tcr` server, a concurrent `tcr login`, a hand-edited
/// top-level key such as `sessionAffinity`) added or changed on disk during
/// that window. `config::save_account` re-reads the document fresh from
/// disk immediately before writing and touches only `account`'s own row, so
/// there is nothing left in this path for a stale in-memory snapshot to
/// clobber. It takes no `config` at all, deliberately: mutating the caller's
/// in-memory snapshot here would just resurrect the same stale-snapshot hazard
/// on any FUTURE caller that grew a reason to read it back after this call.
///
/// One case `config::save_account` cannot serve on its own: no file exists
/// at `config_path` yet at all. Its `read_document` is a bare
/// `fs::read_to_string`, which cannot create a missing path — mirrors the
/// same NotFound guard [`persist_via_account_or_file`] already carries for
/// exactly this reason. There is nothing on disk in that case for a stale
/// snapshot to clobber, so falling back to a fresh single-account
/// [`config::save`] is safe rather than a reintroduction of the hazard this
/// function exists to remove.
fn persist_via_file(config_path: &Path, account: Account) -> anyhow::Result<String> {
    match config::save_account(config_path, &account) {
        Ok(outcome) => account_write_result(config_path, &account, outcome),
        Err(config::ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            let name = account.name.clone();
            let mut fresh: Config =
                serde_json::from_str("{}").expect("empty object is a valid default config");
            let mut first_account = account;
            first_account.priority = Some(0);
            fresh.accounts.push(first_account);
            config::save(config_path, &fresh).context("save config after login")?;
            prose(format_args!(
                "Saved account '{name}' to {}",
                config_path.display()
            ));
            Ok(name)
        }
        Err(err) => Err(err).context("save config after login"),
    }
}

/// The surgical half of a live-route fallback — [`config::save_account`]'s
/// single-account read-modify-write — used when [`probe_add_route`]
/// confirmed the route moments earlier but [`crate::cli::post_add_account`]
/// itself then could not apply it (the route disappeared, or the proxy
/// stopped answering before its 5s deadline). Bridge item A: this replaces
/// [`persist_via_file`] on exactly those two arms of [`finish_login_account`].
///
/// Unlike `persist_via_file`, this never touches any account row but
/// `account`'s own, and it is safe to call even though `account` was built
/// from data gathered before this call's own async window (the profile
/// fetch, the possible stdin prompt, the up-to-5s `post_add_account`
/// timeout): `config::save_account` re-reads the config document FRESH from
/// disk and resolves identity itself, rather than whole-file-saving the
/// (possibly already-stale) in-memory `config` this process is holding — so
/// there is no stale snapshot for it to clobber a concurrent server-side
/// rotation with. This is the same write the server's own persist path uses.
fn persist_via_account(config_path: &Path, account: &Account) -> anyhow::Result<String> {
    let outcome = config::save_account(config_path, account).context("save account after login")?;
    account_write_result(config_path, account, outcome)
}

/// Turn a [`config::AccountWrite`] outcome into `persist_via_account`'s return
/// value, shared with [`persist_via_account_or_file`] so the two callers agree
/// on the exact same success/refusal wording rather than drifting apart.
fn account_write_result(
    config_path: &Path,
    account: &Account,
    outcome: config::AccountWrite,
) -> anyhow::Result<String> {
    match outcome {
        config::AccountWrite::Added | config::AccountWrite::Updated => {
            prose(format_args!(
                "Saved account '{}' to {}",
                account.name,
                config_path.display()
            ));
            Ok(account.name.clone())
        }
        config::AccountWrite::Ambiguous => bail!(
            "'{}''s identity matches more than one account already in {} — give each row its own \
             accountUuid/orgUuid and retry. Nothing was changed.",
            account.name,
            config_path.display()
        ),
        config::AccountWrite::Unwritable => bail!(
            "the accounts key in {} is not a JSON array — refusing to write. Nothing was \
             changed.",
            config_path.display()
        ),
    }
}

/// [`persist_via_account`], but falling back to [`persist_via_file`] when
/// there is no file yet for `save_account`'s `read_document` to read —
/// `fs::read_to_string` cannot create a missing file, and a first-ever login
/// must still be able to. Used by `finish_login_account`'s `NoServer` arm, which
/// otherwise carries the exact same clobber risk `NoRoute` and the
/// timeout/`Unusable` arm already write surgically around: the caller's config
/// snapshot was taken before the profile fetch, the possible
/// stdin prompt, and the live-add round-trip, so a whole-file write here
/// would revert any rotation a server that exited moments ago made to any
/// OTHER account inside that window.
fn persist_via_account_or_file(config_path: &Path, account: &Account) -> anyhow::Result<String> {
    match config::save_account(config_path, account) {
        Err(config::ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            persist_via_file(config_path, account.clone())
        }
        result => account_write_result(
            config_path,
            account,
            result.context("save account after login")?,
        ),
    }
}

/// Persist a finished login's `account` row per `route`, and return the name
/// it was saved under: through the live proxy when it is [`LoginRoute::Live`],
/// via [`persist_via_file`] when it is [`LoginRoute::File`]. Split out of
/// [`login`] so WHERE the credential lands is testable without driving a real
/// browser.
///
/// Provider-agnostic: the Anthropic flow builds its row from a profile fetch
/// ([`finish_login`]), the Codex import from id-token claims
/// ([`import_codex_login`]); everything from the row onward is the same. The
/// row's `provider` rides in the live route's request body (the serialized
/// `config::Account`), but it reaches the running server's rotation only when
/// that server knows the field: a tcr that predates it drops the key and stores
/// the row as a Claude account. So a non-Anthropic row takes the live route only
/// after the server ADVERTISED its provider ([`crate::proxy::PROVIDERS_HEADER`],
/// checked by [`import_codex_login`]), and the server's answer is checked here
/// as well: the `provider` it echoes must be the row's, or this is a hard error
/// telling the user to remove the row it stored.
///
/// On the live path the running server owns the durable write — this must
/// NOT also whole-file [`config::save`] the (possibly already-stale) `config`
/// this process is holding; that write is exactly the clobber this unit
/// exists to remove. The quiet [`crate::cli::LiveControlError::NoServer`]
/// case carries that SAME clobber risk, not a lesser one: the proxy the
/// earlier capability probe confirmed alive can have exited any time in the
/// profile-fetch / stdin-prompt / round-trip window since, so any OTHER
/// account it rotated in that window is still on disk waiting to be
/// reverted. It therefore routes through [`persist_via_account_or_file`]'s
/// surgical write exactly like `NoRoute`/`Unusable` below, never straight to
/// a whole-file one. A server that answered and REFUSED us (`Unauthorized`,
/// `Rejected`) must surface, never silently write the file instead, because
/// the two halves would then disagree about what the account's credentials
/// are.
async fn finish_login_account(
    config_path: &Path,
    route: LoginRoute,
    config: &Config,
    account: Account,
) -> anyhow::Result<String> {
    match route {
        LoginRoute::Refuse(_) => {
            unreachable!("every login bails on a refusal before it has a credential to persist")
        }
        LoginRoute::File => persist_via_file(config_path, account),
        LoginRoute::Live => {
            match crate::cli::post_add_account(config, &account).await {
                // Behind the provider gate, and checked anyway: a non-Anthropic
                // row the server did not confirm storing AS that provider is a
                // row it stored as a Claude account, whose tokens go to Anthropic
                // from now on. Anthropic rows need no echo (`None` is any server
                // older than the field, which stored them exactly right).
                Ok(applied)
                    if !account.provider.is_anthropic()
                        && applied.provider != Some(account.provider) =>
                {
                    bail!(
                        "{}",
                        misstored_provider_refusal(config.proxy.port, &applied, account.provider)
                    )
                }
                Ok(applied) => {
                    prose(format_args!(
                        "Saved account '{}' to the running proxy on :{}",
                        applied.name, config.proxy.port
                    ));
                    if let Some(warning) = &applied.warning {
                        eprintln!("[tcr] warning: {warning}");
                    }
                    if !applied.persisted {
                        eprintln!(
                            "[tcr] warning: this account is live but NOT saved to {} — it will not survive a restart.",
                            config_path.display()
                        );
                    }
                    Ok(applied.name)
                }
                // Nothing is listening any more (it exited between the probe
                // and here). NOT the safe "offline the whole time" case
                // `persist_via_file` is for: a proxy WAS confirmed alive
                // moments ago, and it can have rotated any OTHER account's
                // tokens on disk any time in the profile-fetch / stdin-prompt
                // / round-trip window since. Same surgical write as
                // `NoRoute`/`Unusable` below, falling back to a whole-file
                // write only when there is no file yet at all.
                Err(crate::cli::LiveControlError::NoServer) => {
                    persist_via_account_or_file(config_path, &account)
                }
                // The proxy rejected our api-key. Writing the file here would
                // be the clobber this unit exists to remove, in a new place:
                // the running server keeps whatever it already had while the
                // file quietly disagrees. Change nothing, exit non-zero.
                Err(crate::cli::LiveControlError::Unauthorized) => bail!(
                    "the proxy on :{} rejected the api-key in {} — the config was NOT changed, \
                     because writing it would leave the running proxy unaware of this login. \
                     Fix `proxy.apiKey` and retry.",
                    config.proxy.port,
                    config_path.display()
                ),
                // The route ran and refused the submission itself (e.g. the
                // identity matched more than one live account). Do not fall
                // back — the file's own resolution could land on a different
                // account than the one the server was talking about.
                Err(crate::cli::LiveControlError::Rejected(message)) => bail!(
                    "the proxy running on :{} refused this: {message} Nothing was changed.",
                    config.proxy.port
                ),
                // The route is missing: the capability probe said it was
                // there and it no longer is (a race, or a downgrade mid-run).
                // Bridge item A: this is reached with a CONFIRMED-live server
                // on the other end of the port, seconds after the config
                // snapshot above was loaded (a profile fetch, possibly an
                // unbounded stdin prompt, then this call itself came between)
                // — so a whole-file `persist_via_file` here would revert any
                // edit the live server made to any OTHER account in that
                // window. `persist_via_account` writes only this one row,
                // re-reading the file fresh instead of trusting the stale
                // snapshot. It is still HALF a login — say so loudly,
                // mirroring `set_enabled`'s NoRoute arm.
                Err(crate::cli::LiveControlError::NoRoute) => {
                    let saved = persist_via_account(config_path, &account)?;
                    eprintln!(
                        "[tcr] WARNING: the proxy running on :{} is too old to accept a live login (no {} route), so the account was written to {} while the proxy is running. It will not see this account until it restarts. {}",
                        config.proxy.port,
                        crate::proxy::ADD_ACCOUNT_PATH,
                        config_path.display(),
                        crate::cli::RESTART_HINT,
                    );
                    Ok(saved)
                }
                // It answered something unusable, or did not answer in time
                // (the wedged-port shape `cli::probe_incumbent`'s own tests
                // document as measured and real — the probe confirmed the
                // route seconds ago and `post_add_account` then hit its own
                // 5s deadline). Same shape of consequence as `NoRoute`, same
                // surgical write for the same reason, different cause, and
                // equally never silent.
                Err(other) => {
                    let why = other.why();
                    let saved = persist_via_account(config_path, &account)?;
                    eprintln!(
                        "[tcr] WARNING: could not apply this login to the proxy running on :{} ({why}), so the account was written to {} while the proxy is running — it may not see this account until it restarts.",
                        config.proxy.port,
                        config_path.display(),
                    );
                    Ok(saved)
                }
            }
        }
    }
}

/// What [`finish_login_account`] says when a live server did not confirm storing
/// a `wanted`-provider row as one: the row is live and on disk as a Claude
/// account, so the only safe next step is removing it, then a fresh `codex
/// login` (its token may already have reached Anthropic).
fn misstored_provider_refusal(
    port: u16,
    applied: &crate::proxy::AddAccountResponse,
    wanted: crate::provider::Provider,
) -> String {
    let noun = |provider: crate::provider::Provider| match provider {
        crate::provider::Provider::Anthropic => "Claude",
        crate::provider::Provider::Codex => "Codex",
    };
    let said = match applied.provider {
        Some(stored) => format!("stored '{}' as a {} account", applied.name, noun(stored)),
        None => format!(
            "did not say how it stored '{}' — only a proxy older than Codex accounts answers that way",
            applied.name
        ),
    };
    format!(
        "the proxy on :{port} {said}, not as a {} account. That row is now live, and on disk, as \
         a Claude account, and its token may already have reached Anthropic: run `tcr remove \
         '{}'` now, then `codex login` again.",
        noun(wanted),
        applied.name
    )
}

/// [`finish_login_account`] for an Anthropic login: the row is the profile the
/// login authenticated as, plus the tokens it produced.
async fn finish_login(
    config_path: &Path,
    route: LoginRoute,
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    profile: Profile,
) -> anyhow::Result<String> {
    let account = Account {
        name: name.to_string(),
        account_type: crate::config::AccountType::OAuth,
        provider: Default::default(),
        account_uuid: profile.account_uuid,
        org_uuid: profile.org_uuid,
        org_name: profile.org_name,
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        expires_at: Some(tokens.expires_at_ms),
        priority: None,
        switch_threshold: None,
        disabled: None,
        groups: None,
        // Carried on the SERVER route too, and for free: the request body
        // `finish_login_account` posts is a serialized `config::Account`, so the
        // two serde fields put the plan on the wire without a second payload
        // shape to keep in step. Pinned by
        // `add_account_request_carries_the_plan_fields`.
        organization_type: profile.organization_type,
        rate_limit_tier: profile.rate_limit_tier,
        seat_tier: profile.seat_tier,
        egress: crate::config::Egress::Local,
        egress_strict: false,
        codex_home: None,
        extra: serde_json::Map::new(),
    };
    finish_login_account(config_path, route, config, account).await
}

/// Load the config for a login, creating the file with defaults when it does
/// not exist yet (the very first login on a fresh box) while surfacing genuine
/// parse/permission errors so a corrupt file is never overwritten.
///
/// Shares [`config::load_or_init`] with every other verb rather than keeping a
/// second copy of the missing-file rule here.
pub(crate) fn load_or_default(config_path: &Path) -> anyhow::Result<Config> {
    let (config, _created) = config::load_or_init(config_path).context("load config for login")?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_treats_small_values_as_seconds() {
        // 1_700_000_000 s → ms.
        assert_eq!(normalize_expires_at(1_700_000_000), 1_700_000_000_000);
    }

    #[test]
    fn normalize_leaves_millisecond_values_unchanged() {
        assert_eq!(normalize_expires_at(1_700_000_000_000), 1_700_000_000_000);
    }

    #[test]
    fn is_expired_true_when_now_past_expiry() {
        assert!(is_expired(Some(1000), 2000));
    }

    #[test]
    fn is_expired_false_for_none() {
        assert!(!is_expired(None, 2000));
    }

    #[test]
    fn is_expiring_soon_within_window() {
        // Expiry 1 minute out is inside the 5-minute window.
        let now = 10_000_000;
        assert!(is_expiring_soon(Some(now + 60_000), now));
        assert!(!is_expiring_soon(Some(now + 10 * 60_000), now));
    }

    // --- PKCE (via the oauth2 crate) ---------------------------------------

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // The canonical RFC 7636 Appendix B S256 example: the oauth2 crate must
        // map this fixed verifier to exactly this base64url (no-pad) challenge.
        let verifier = oauth2::PkceCodeVerifier::new(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string(),
        );
        let challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
        assert_eq!(
            challenge.as_str(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn login_flow_produces_verifier_and_state() {
        // The oauth2-built flow yields a non-empty verifier and CSRF state, and
        // an authorize URL rooted at Claude's endpoint.
        let flow = build_login_flow("http://localhost:12345/callback", None).unwrap();
        assert!(!flow.verifier.is_empty());
        assert!(!flow.state.is_empty());
        assert!(flow.auth_url.starts_with(AUTHORIZE_URL));
        // Claude's authorize endpoint rejects short states ("Invalid request
        // format", observed live 2026-07-17 with the 22-char crate default).
        // 32 random bytes -> 43 base64url chars, matching the JS reference.
        assert_eq!(
            flow.state.len(),
            43,
            "state must be 32 bytes base64url (43 chars), got {} chars",
            flow.state.len()
        );
    }

    // --- callback / URL parsing --------------------------------------------

    #[test]
    fn parse_callback_extracts_code_and_state() {
        let (code, state, error) = parse_oauth_query("code=abc123&state=xyz789");
        assert_eq!(code.as_deref(), Some("abc123"));
        assert_eq!(state.as_deref(), Some("xyz789"));
        assert!(error.is_none());
    }

    #[test]
    fn parse_callback_percent_decodes_values() {
        let (code, _state, _error) = parse_oauth_query("code=a%2Fb%2Bc&state=s");
        assert_eq!(code.as_deref(), Some("a/b+c"));
    }

    /// The hostile-decode cases the deleted private `percent_decode` helper
    /// covered, re-pointed through the public parser now that `form_urlencoded`
    /// owns the decoding. Same inputs, same guarantees — coverage is unchanged,
    /// only the entry point moved.
    #[test]
    fn parse_oauth_query_survives_bare_percent_and_multibyte() {
        // Valid %XX still decodes.
        let (code, ..) = parse_oauth_query("code=a%20b");
        assert_eq!(code.as_deref(), Some("a b"));

        // A trailing bare `%` is preserved, never panics.
        let (code, ..) = parse_oauth_query("code=100%");
        assert_eq!(code.as_deref(), Some("100%"));

        // A `%` immediately before a multibyte UTF-8 char (hostile callback)
        // must NOT panic on a char-boundary slice.
        let (code, ..) = parse_oauth_query("code=a%\u{20AC}b");
        let code = code.expect("a % before a multibyte char must still parse");
        assert!(code.contains('a') && code.contains('b'));

        // A lone `%€` (percent right before the euro sign) also survives.
        let _ = parse_oauth_query("code=%\u{20AC}");
    }

    #[test]
    fn expires_at_from_saturates_on_huge_expires_in() {
        // A hostile/huge expires_in must saturate, never overflow into a
        // negative (past) expiry that would force a refresh storm.
        let now = crate::now_ms();
        let saturated = expires_at_from(now, Some(i64::MAX));
        assert_eq!(saturated, i64::MAX);
        assert!(
            saturated >= now,
            "expiry must be in the future, not negative"
        );
        // Normal values are unchanged: default 3600s and an explicit value.
        assert_eq!(expires_at_from(0, None), 3_600 * 1000);
        assert_eq!(expires_at_from(1000, Some(60)), 1000 + 60 * 1000);
    }

    // --- `--non-interactive` (the TcrBar in-app login) ----------------------

    /// The whole wire contract `LoginSession.swift` parses, pinned as literal
    /// text. Hardcoded rather than built from the same `serde_json::json!`
    /// the production code uses — an assertion written that way passes no
    /// matter what the key names become, which is the one thing this test
    /// exists to stop.
    #[test]
    fn every_event_renders_the_exact_line_the_app_parses() {
        // Key ORDER is serde's (alphabetical) and no JSON parser cares, so
        // each object is compared as a whole parsed value: that still fails on
        // a renamed key, a dropped key or an extra one, which is what the
        // Swift side would break on.
        let parsed = |line: String| -> serde_json::Value {
            serde_json::from_str(&line).expect("every event line is one JSON object")
        };
        assert_eq!(
            parsed(
                LoginEvent::Browser {
                    url: "https://claude.ai/oauth/authorize?code=true&state=abc"
                }
                .line()
            ),
            serde_json::json!({
                "event": "browser",
                "url": "https://claude.ai/oauth/authorize?code=true&state=abc"
            })
        );
        assert_eq!(LoginEvent::Waiting.line(), r#"{"event":"waiting"}"#);
        assert_eq!(
            parsed(
                LoginEvent::Saved {
                    account: "alice@example.com"
                }
                .line()
            ),
            serde_json::json!({ "event": "saved", "account": "alice@example.com" })
        );
        assert_eq!(
            parsed(
                LoginEvent::Error {
                    reason: "the proxy on :3456 rejected the api-key"
                }
                .line()
            ),
            serde_json::json!({
                "event": "error",
                "reason": "the proxy on :3456 rejected the api-key"
            })
        );
    }

    /// The three Claude lines, byte for byte. A Codex login adds keys to its own lines
    /// (`opened`, `provider`, `plan`, `code`), and none of them may reach a Claude line, which
    /// TcrBar and every older parser read exactly as they read them before Codex accounts.
    ///
    /// Watched red: rendering `Saved` through the Codex arm adds `provider` to alice's line.
    #[test]
    fn claude_saved_event_is_unchanged() {
        assert_eq!(
            LoginEvent::Saved {
                account: "alice@example.com"
            }
            .line(),
            r#"{"account":"alice@example.com","event":"saved"}"#
        );
        assert_eq!(
            LoginEvent::Browser {
                url: "https://claude.ai/oauth/authorize?code=true"
            }
            .line(),
            r#"{"event":"browser","url":"https://claude.ai/oauth/authorize?code=true"}"#
        );
        assert_eq!(
            LoginEvent::Error { reason: "x" }.line(),
            r#"{"event":"error","reason":"x"}"#
        );
    }

    /// The Codex events, against the fixture TcrBar's decoder is tested on
    /// (`apps/macos/Tests/Fixtures/codex-login-events.jsonl`): producer and consumer are pinned to
    /// one file, so neither can drift from the other. Compared as parsed values, because key
    /// order is serde's and no parser cares.
    ///
    /// Watched red: one word of the first-account notice changed fails on fixture line 3.
    #[test]
    fn codex_login_events_match_the_fixture() {
        let fixture = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("apps/macos/Tests/Fixtures/codex-login-events.jsonl"),
        )
        .expect("the committed fixture");
        let want: Vec<serde_json::Value> = fixture
            .lines()
            .map(|line| serde_json::from_str(line).expect("each fixture line is one JSON object"))
            .collect();
        assert_eq!(want.len(), 5, "{fixture}");
        let produced = [
            LoginEvent::BrowserOpened {
                url: "https://auth.openai.com/oauth/authorize?client_id=app_EXAMPLE&state=EXAMPLE",
            }
            .line(),
            LoginEvent::Waiting.line(),
            LoginEvent::Notice {
                text: CODEX_FIRST_ACCOUNT_NOTICE,
            }
            .line(),
            LoginEvent::SavedCodex {
                account: "bob@example.com",
                plan: Some("ChatGPT Pro"),
            }
            .line(),
            codex_identity_refusal("carol@example.com", "erin@example.com/codex").line(),
        ];
        for (index, (line, want)) in produced.iter().zip(&want).enumerate() {
            let got: serde_json::Value = serde_json::from_str(line).expect("one JSON object");
            assert_eq!(&got, want, "fixture line {}", index + 1);
        }
        assert!(
            !LoginEvent::SavedCodex {
                account: "bob@example.com",
                plan: None
            }
            .line()
            .contains("plan"),
            "no plan key when the plan is unknown"
        );
    }

    /// A refusal's machine line carries its code and its own reason, without the context a
    /// caller wrapped it in; any other failure keeps today's uncoded line, its whole chain
    /// flattened onto it.
    ///
    /// Watched red: rendering every error through the plain arm loses the code and prefixes
    /// the context.
    #[test]
    fn a_refusal_keeps_its_code_and_its_reason_verbatim() {
        let refused = anyhow::Error::from(LoginRefusal {
            code: "codex-routed",
            reason: "r".into(),
        })
        .context("importing the Codex login failed");
        assert_eq!(
            machine_error_line(&refused),
            (
                r#"{"code":"codex-routed","event":"error","reason":"r"}"#.to_string(),
                "r".to_string()
            )
        );
        let plain = anyhow!("boom").context("OAuth login failed");
        assert_eq!(
            machine_error_line(&plain),
            (
                r#"{"event":"error","reason":"OAuth login failed: boom"}"#.to_string(),
                "OAuth login failed: boom".to_string()
            )
        );
    }

    /// A reason carrying a quote and a newline still leaves ONE parseable
    /// line. Hand-built JSON is what makes a message like this an unparseable
    /// line at the worst possible moment, so the escaping is asserted rather
    /// than assumed.
    #[test]
    fn an_event_line_survives_quotes_and_newlines_in_its_payload() {
        let line = LoginEvent::Error {
            reason: "config \"accounts\"\nis not an array",
        }
        .line();
        assert!(
            !line.contains('\n'),
            "one event is one physical line: {line}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("must stay parseable");
        assert_eq!(parsed["reason"], "config \"accounts\"\nis not an array");
    }

    /// `one_line_reason` flattens an `anyhow` chain AND any newline inside a
    /// single message, because both reach a GUI label that has one line to
    /// render them in.
    #[test]
    fn one_line_reason_flattens_the_whole_chain_onto_one_line() {
        let error = anyhow!("the proxy on :3456 answered\nbut not usably (timeout)")
            .context("OAuth login failed");
        let reason = one_line_reason(&error);
        assert_eq!(
            reason,
            "OAuth login failed: the proxy on :3456 answered but not usably (timeout)"
        );
    }

    /// The event sequence, driven for real: `authorize_and_wait` in
    /// [`LoginUi::Machine`] emits `browser` then `waiting`, and a fake
    /// callback — the only completion path that mode has — hands back the
    /// code. The URL in the `browser` event is the one the callback answers,
    /// state and all, so this also pins that the app is told to open the URL
    /// this very listener is waiting on.
    ///
    /// Watched failing: with the `emit(LoginEvent::Waiting)` line deleted the
    /// sequence assertion fails with `["browser"] != ["browser", "waiting"]`;
    /// with `LoginUi::Machine`'s arm switched to `open_browser` and no emit,
    /// it fails on an empty sequence.
    #[tokio::test]
    async fn machine_mode_emits_browser_then_waiting_and_completes_on_the_callback() {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        // The fake browser: wait for the `browser` event, then GET the
        // loopback callback exactly as Claude's redirect would.
        let watched = recorded.clone();
        let browser = tokio::spawn(async move {
            let url = loop {
                let first = watched.lock().expect("sink mutex").first().cloned();
                if let Some(line) = first {
                    let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
                    break parsed["url"].as_str().unwrap().to_string();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            let field = |key: &str| -> String {
                let Some(raw) = url
                    .split(&format!("{key}="))
                    .nth(1)
                    .and_then(|rest| rest.split('&').next())
                else {
                    panic!("the authorize URL carries {key}: {url}")
                };
                percent_decode(raw)
            };
            let callback = format!(
                "{}?code=fake-code&state={}",
                field("redirect_uri"),
                field("state")
            );
            let _ = reqwest::get(&callback).await;
        });

        let collected = recorded.clone();
        let mut sink = |event: LoginEvent<'_>| {
            collected.lock().expect("sink mutex").push(event.line());
        };
        let authorized = authorize_and_wait(None, LoginUi::Machine, &mut sink)
            .await
            .expect("the fake callback must settle the login");
        browser.await.expect("fake browser task");

        let events = recorded.lock().expect("sink mutex").clone();
        assert_eq!(
            events,
            vec![
                LoginEvent::Browser {
                    url: &authorized.flow.auth_url
                }
                .line(),
                LoginEvent::Waiting.line(),
            ],
            "machine mode publishes exactly browser then waiting before the callback"
        );
        assert_eq!(authorized.code, "fake-code");
        assert!(
            authorized.redirect_uri.starts_with("http://localhost:"),
            "the callback is loopback-only: {}",
            authorized.redirect_uri
        );
    }

    /// Percent-decode just enough for the test above to read the
    /// `redirect_uri` back out of an authorize URL.
    fn percent_decode(value: &str) -> String {
        form_urlencoded::parse(format!("v={value}").as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    }

    /// The email-less profile — the one path that prompts — takes the
    /// prompt's own default instead of reading a stdin that is a pipe.
    #[test]
    fn machine_mode_names_an_email_less_account_without_prompting() {
        let empty: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        let name =
            mint_login_name(&empty, &all_none_profile(), None, None, LoginUi::Machine).unwrap();
        assert_eq!(name, "unnamed");
    }

    /// A browser login whose profile fetch came back empty takes its name from
    /// the token-exchange response, not from [`unnamed_fallback`]. The fixture
    /// is the response shape `docs/design/long-lived-tokens.md` records.
    #[test]
    fn a_login_with_an_empty_profile_is_named_from_the_exchange_response() {
        let body = r#"{
            "access_token": "at",
            "refresh_token": "rt",
            "expires_in": 31536000,
            "account": {
                "uuid": "11111111-1111-1111-1111-111111111111",
                "email_address": "alice@example.com"
            },
            "organization": {
                "uuid": "22222222-2222-2222-2222-222222222222",
                "name": "Example Org"
            }
        }"#;

        // A config where the fallback would hand out `unnamed-2`, and one
        // where alice's bare email is already taken.
        let with_unnamed: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "unnamed", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
        )
        .unwrap();
        let with_alice: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
        )
        .unwrap();

        for (config, expected) in [
            (&with_unnamed, "alice@example.com"),
            (&with_alice, "alice@example.com/example-org"),
        ] {
            let Exchanged { identity, .. } =
                tokens_from_exchange_body(body).expect("a present refresh_token must return Ok");
            let profile = all_none_profile().or_exchange_identity(identity);
            let name = mint_login_name(config, &profile, None, None, LoginUi::Machine).unwrap();
            assert_eq!(name, expected);
            assert_eq!(
                profile.account_uuid(),
                Some("11111111-1111-1111-1111-111111111111")
            );
            assert_eq!(
                profile.org_uuid.as_deref(),
                Some("22222222-2222-2222-2222-222222222222")
            );
        }
    }

    /// A field the profile endpoint DID send is kept; only its gaps are filled.
    #[test]
    fn exchange_identity_never_overrides_what_the_profile_sent() {
        let body = r#"{
            "access_token": "at",
            "refresh_token": "rt",
            "account": { "uuid": "33333333", "email_address": "bob@example.com" },
            "organization": { "uuid": "44444444", "name": "Other Org" }
        }"#;
        let Exchanged { identity, .. } = tokens_from_exchange_body(body).unwrap();
        let profile = profile_with_identity(
            "alice@example.com",
            Some("11111111"),
            Some("22222222"),
            None,
        )
        .or_exchange_identity(identity);
        assert_eq!(profile.email(), Some("alice@example.com"));
        assert_eq!(profile.account_uuid(), Some("11111111"));
        assert_eq!(profile.org_uuid.as_deref(), Some("22222222"));
        assert_eq!(profile.org_name.as_deref(), Some("Other Org"));
    }

    // --- token-exchange refresh_token guard --------------------------------

    #[test]
    fn exchange_body_fails_without_usable_refresh_token() {
        // An exchange response has no prior refresh token to fall back on, so a
        // missing, empty, or whitespace `refresh_token` must FAIL the exchange —
        // never mint a Tokens with an empty refresh_token that dies on first use.
        for body in [
            r#"{"access_token":"at"}"#,                       // absent
            r#"{"access_token":"at","refresh_token":""}"#,    // empty
            r#"{"access_token":"at","refresh_token":"   "}"#, // whitespace
            r#"{"access_token":"at","refresh_token":null}"#,  // explicit null
        ] {
            assert!(
                tokens_from_exchange_body(body).is_err(),
                "expected Err for body: {body}"
            );
        }
    }

    #[test]
    fn exchange_body_ok_with_refresh_token() {
        // A response carrying a real refresh_token yields Ok with that token
        // preserved verbatim.
        let tokens = tokens_from_exchange_body(
            r#"{"access_token":"at","refresh_token":"rt","expires_at":1700000000000}"#,
        )
        .expect("a present refresh_token must return Ok")
        .tokens;
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        assert_eq!(tokens.expires_at_ms, 1_700_000_000_000);
    }

    /// A response carrying `expires_in: 31536000` (one year) must land as an
    /// `expires_at_ms` about 365 days from now — `tokens_from_exchange_body`
    /// derives it via `expires_at_from`, which just adds `expires_in * 1000`
    /// to `now_ms()`, so this pins that the year-long value survives the
    /// round trip rather than being silently capped or defaulted.
    #[test]
    fn exchange_body_with_year_long_expires_in_yields_year_out_expiry() {
        let before = crate::now_ms();
        let tokens = tokens_from_exchange_body(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":31536000}"#,
        )
        .expect("a present refresh_token must return Ok")
        .tokens;
        let after = crate::now_ms();

        let year_ms: i64 = 31_536_000 * 1000;
        assert!(
            tokens.expires_at_ms >= before + year_ms && tokens.expires_at_ms <= after + year_ms,
            "expected expires_at_ms about one year out, got {} (now was {before}..{after})",
            tokens.expires_at_ms
        );
    }

    /// The serialized exchange body must request the year-long lifetime as a
    /// client-supplied `expires_in`, mirroring `warm_request_spec`'s
    /// no-network assertion pattern in `src/warmer.rs`.
    #[test]
    fn exchange_request_body_carries_year_long_expires_in() {
        let body = exchange_request_body("a-code", "a-verifier", "a-state", "https://cb");
        assert_eq!(body["expires_in"], LOGIN_TOKEN_LIFETIME_SECS);
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code"], "a-code");
    }

    /// The three scopes the token endpoint refuses a custom `expires_in` for
    /// must never come back: assert their absence explicitly so a future
    /// re-add (e.g. restoring MCP or Claude Code session scopes) fails this
    /// test loudly instead of silently reintroducing the HTTP 400.
    #[test]
    fn oauth_scopes_excludes_the_custom_expires_in_incompatible_scopes() {
        for forbidden in [
            "org:create_api_key",
            "user:sessions:claude_code",
            "user:mcp_servers",
        ] {
            assert!(
                !OAUTH_SCOPES.contains(forbidden),
                "OAUTH_SCOPES must not contain {forbidden:?}, got {OAUTH_SCOPES:?}"
            );
        }
    }

    /// THE TRAP (bridge §"THE TRAP"): a refresh whose response omits a
    /// rotated `refresh_token` must fall back to the token it was called
    /// with — never `None`. `apply_refresh` (`manager/refresh.rs`) and
    /// `persist_tokens` (`manager/mod.rs`) both write `tokens.refresh_token`
    /// straight onto an existing account's row with no other check, so a
    /// bare `None` here would silently make a healthy account unrefreshable
    /// on its very next rotation, with no error and no log line.
    ///
    /// Watched failing: with the `unwrap_or_else` fallback in
    /// `refresh_access_token_at` replaced by a bare `data.refresh_token`
    /// (still type-correct — `Option<String>` on both sides, so this is a
    /// silent behavioural regression, not a compile error), this test failed
    /// with `left: None, right: Some("rt-original")`. Restored, it passes.
    #[tokio::test]
    async fn refresh_access_token_at_preserves_stored_refresh_token_when_response_omits_one() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{port}/");
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(|| async {
                    // A real rotation response that omits `refresh_token` —
                    // exactly the shape Anthropic's endpoint sends when it
                    // does not rotate the refresh token on this call.
                    axum::Json(serde_json::json!({
                        "access_token": "at-fresh",
                        "expires_at": 1_893_456_000_000i64
                    }))
                }),
            );
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let tokens = refresh_access_token_at(&client, "rt-original", &endpoint)
            .await
            .expect("a refresh with no rotated refresh_token must still succeed");
        assert_eq!(
            tokens.refresh_token.as_deref(),
            Some("rt-original"),
            "must fall back to the refresh token it was called with, never None"
        );
    }

    /// `LiveRefresher` sends each provider's refresh token to that provider's
    /// own endpoint, in that provider's grant. A refresh token is only good
    /// where it was issued, so a Codex token posted to Anthropic is not a slow
    /// refresh but a rejected one — and a rejection sidelines the account as
    /// dead until it is logged in again.
    ///
    /// Both endpoints point at one loopback stub — the Anthropic one by setting
    /// the private field, since it has no builder seam — so a broken dispatch
    /// fails here instead of posting a fake token to a real token endpoint. The
    /// client ids pin the pairing of grant to endpoint as well as the route.
    #[tokio::test]
    async fn live_refresher_sends_each_provider_to_its_own_token_endpoint() {
        type Seen = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, serde_json::Value)>>>;
        let seen: Seen = Seen::default();
        let recorder = |route: &'static str, seen: Seen| {
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push((route, body));
                    axum::Json(serde_json::json!({
                        "access_token": format!("at-from-{route}"),
                        "refresh_token": "rt-rotated",
                        "expires_in": 3600
                    }))
                }
            }
        };
        let app = axum::Router::new()
            .route(
                "/anthropic",
                axum::routing::post(recorder("anthropic", seen.clone())),
            )
            .route(
                "/codex",
                axum::routing::post(recorder("codex", seen.clone())),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut refresher =
            LiveRefresher::new().with_codex_endpoint(format!("http://{addr}/codex"));
        refresher.endpoint = format!("http://{addr}/anthropic");

        let codex = refresher
            .refresh(crate::provider::Provider::Codex, "rt-codex".to_string())
            .await
            .expect("the Codex refresh must succeed against the stub");
        let anthropic = refresher
            .refresh(
                crate::provider::Provider::Anthropic,
                "rt-anthropic".to_string(),
            )
            .await
            .expect("the Anthropic refresh must succeed against the stub");

        assert_eq!(
            codex.access_token, "at-from-codex",
            "a Codex refresh must reach the Codex token endpoint"
        );
        assert_eq!(
            anthropic.access_token, "at-from-anthropic",
            "an Anthropic refresh must reach the Anthropic token endpoint"
        );
        let seen = seen.lock().unwrap();
        let body_at = |route: &str| {
            let (_, body) = seen
                .iter()
                .find(|(hit, _)| *hit == route)
                .unwrap_or_else(|| panic!("the {route} endpoint was never called: {seen:?}"));
            body.clone()
        };
        assert_eq!(body_at("codex")["client_id"], crate::codex_oauth::CLIENT_ID);
        assert_eq!(body_at("codex")["refresh_token"], "rt-codex");
        assert_eq!(body_at("anthropic")["client_id"], CLIENT_ID);
        assert_eq!(body_at("anthropic")["refresh_token"], "rt-anthropic");
    }

    #[test]
    fn resolve_callback_accepts_matching_state() {
        let code = resolve_callback_code(
            Some("the-code".into()),
            Some("expected".into()),
            None,
            "expected",
        )
        .expect("matching state should resolve");
        assert_eq!(code, "the-code");
    }

    #[test]
    fn resolve_callback_rejects_state_mismatch() {
        let result = resolve_callback_code(
            Some("the-code".into()),
            Some("attacker".into()),
            None,
            "expected",
        );
        assert!(result.is_err(), "mismatched state must be rejected");
    }

    #[test]
    fn resolve_callback_rejects_missing_state() {
        // A callback with no state at all is a CSRF signal → reject.
        let result = resolve_callback_code(Some("the-code".into()), None, None, "expected");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_callback_rejects_oauth_error() {
        let result = resolve_callback_code(
            None,
            Some("expected".into()),
            Some("access_denied".into()),
            "expected",
        );
        assert!(result.is_err());
    }

    // --- config append / update --------------------------------------------

    /// An identity-only [`Profile`] for the [`upsert_account`] tests — the
    /// three fields those tests vary, with no plan and no email (the name is
    /// passed separately).
    fn ident(
        account_uuid: Option<&str>,
        org_uuid: Option<&str>,
        org_name: Option<&str>,
    ) -> Profile {
        Profile {
            email: None,
            account_uuid: account_uuid.map(str::to_string),
            org_uuid: org_uuid.map(str::to_string),
            org_name: org_name.map(str::to_string),
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
        }
    }

    fn tokens(access: &str, refresh: &str, expires: i64) -> Tokens {
        Tokens {
            id_token: None,
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at_ms: expires,
        }
    }

    /// A plan is not an identity: an org that upgraded, or an account that
    /// joined a Team, must show its NEW plan after a re-login rather than the
    /// one it was first seen on. This is the one place `upsert_account` treats
    /// a field differently from the uuid/org fields beside it.
    #[test]
    fn upsert_overwrites_a_stale_plan_while_still_backfilling_identity() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "old-at",
                  "orgName": "Kept", "organizationType": "claude_max",
                  "rateLimitTier": "default_claude_max_5x" }
            ] }"#,
        )
        .unwrap();

        let mut profile = ident(None, None, Some("Ignored"));
        profile.organization_type = Some("claude_team".to_string());
        profile.rate_limit_tier = Some("default_raven".to_string());
        profile.seat_tier = Some("team_standard".to_string());
        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("new-at", "new-rt", 999),
            &profile,
        )
        .unwrap();

        let account = &config.accounts[0];
        assert_eq!(
            account.org_name.as_deref(),
            Some("Kept"),
            "identity stays backfill-only — a known org name is never overwritten"
        );
        assert_eq!(account.organization_type.as_deref(), Some("claude_team"));
        assert_eq!(account.rate_limit_tier.as_deref(), Some("default_raven"));
        assert_eq!(account.seat_tier.as_deref(), Some("team_standard"));
    }

    /// A profile fetch that failed carries no plan, and "I did not learn this"
    /// must never blank a plan already on the row.
    #[test]
    fn upsert_never_blanks_a_known_plan_when_the_profile_carried_none() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "old-at",
                  "organizationType": "claude_team", "seatTier": "team_standard" }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("new-at", "new-rt", 999),
            &ident(None, None, None),
        )
        .unwrap();

        assert_eq!(
            config.accounts[0].organization_type.as_deref(),
            Some("claude_team")
        );
        assert_eq!(
            config.accounts[0].seat_tier.as_deref(),
            Some("team_standard")
        );
    }

    /// A brand-new account carries the plan the login just read.
    #[test]
    fn upsert_stores_the_plan_on_a_freshly_appended_account() {
        let mut config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        upsert_account(
            &mut config,
            "new@example.com",
            &tokens("at1", "rt1", 111),
            &profile_with_plan(
                "new@example.com",
                "claude_max",
                Some("default_claude_max_20x"),
                None,
            ),
        )
        .unwrap();

        assert_eq!(
            config.accounts[0].organization_type.as_deref(),
            Some("claude_max")
        );
        assert_eq!(
            config.accounts[0].rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );
        assert_eq!(config.accounts[0].seat_tier, None);
    }

    #[test]
    fn upsert_appends_new_account() {
        let mut config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        upsert_account(
            &mut config,
            "new@example.com",
            &tokens("at1", "rt1", 111),
            &ident(None, None, None),
        )
        .unwrap();

        assert_eq!(config.accounts.len(), 1);
        let account = &config.accounts[0];
        assert_eq!(account.name, "new@example.com");
        assert_eq!(account.account_type, crate::config::AccountType::OAuth);
        assert_eq!(account.access_token, "at1");
        assert_eq!(account.refresh_token.as_deref(), Some("rt1"));
        assert_eq!(account.expires_at, Some(111));
        assert_eq!(account.priority, Some(0));
    }

    #[test]
    fn upsert_updates_existing_email_without_duplicating() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "old-at",
                  "refreshToken": "old-rt", "expiresAt": 100, "priority": 0 }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("new-at", "new-rt", 999),
            &ident(None, None, None),
        )
        .unwrap();

        // Same account count — updated in place, not duplicated.
        assert_eq!(config.accounts.len(), 1);
        let account = &config.accounts[0];
        assert_eq!(account.access_token, "new-at");
        assert_eq!(account.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(account.expires_at, Some(999));
        // Existing priority is preserved on update.
        assert_eq!(account.priority, Some(0));
    }

    #[test]
    fn upsert_assigns_next_priority_for_additional_account() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "first@example.com", "type": "oauth", "accessToken": "at",
                  "refreshToken": "rt", "expiresAt": 100, "priority": 3 }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "second@example.com",
            &tokens("at2", "rt2", 200),
            &ident(None, None, None),
        )
        .unwrap();

        assert_eq!(config.accounts.len(), 2);
        // max existing priority (3) + 1.
        assert_eq!(config.accounts[1].priority, Some(4));
    }

    #[test]
    fn upsert_second_org_same_email_appends_distinct_account() {
        // #0: the same email logging into a DIFFERENT org must NOT overwrite the
        // first — once both entries carry org keys they are distinct accounts.
        let mut config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-corp", "rt-corp", 100),
            &ident(Some("uuid-person"), Some("org-corp"), Some("Corp")),
        )
        .unwrap();
        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-pers", "rt-pers", 200),
            &ident(Some("uuid-person"), Some("org-personal"), Some("Personal")),
        )
        .unwrap();

        assert_eq!(config.accounts.len(), 2, "second org is a distinct account");
        assert_eq!(config.accounts[0].access_token, "at-corp");
        assert_eq!(config.accounts[1].access_token, "at-pers");
        assert_eq!(config.accounts[1].org_uuid.as_deref(), Some("org-personal"));
    }

    #[test]
    fn upsert_backfills_legacy_entry_org_without_duplicating() {
        // A legacy entry (uuid but no org) meeting a freshly-profiled login of the
        // same person backfills the org in place rather than duplicating.
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "uuid-person",
                  "accessToken": "old-at", "refreshToken": "old-rt", "expiresAt": 100, "priority": 0 }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("new-at", "new-rt", 999),
            &ident(Some("uuid-person"), Some("org-corp"), Some("Corp")),
        )
        .unwrap();

        assert_eq!(
            config.accounts.len(),
            1,
            "legacy entry backfilled, not duplicated"
        );
        let account = &config.accounts[0];
        assert_eq!(account.access_token, "new-at");
        assert_eq!(account.org_uuid.as_deref(), Some("org-corp"));
        assert_eq!(account.org_name.as_deref(), Some("Corp"));
    }

    /// THE MEASURED FAILURE bridge item B exists to fix: entries
    /// `[{u1, org-a, at-ORG-A/rt-ORG-A}, {u1, no org, at-LEGACY/rt-LEGACY}]`
    /// with a login identity `{u1, no org}`. Under the old `.find()`
    /// first-match-wins, `same_identity` matches BOTH rows — a legacy no-org
    /// entry loosely matches every org of that person — so `.find()` silently
    /// took index 0, stamping the fresh credential over org-a's row and
    /// destroying its single-use refresh token. `identity::resolve` exists
    /// exactly to break this tie via `same_identity_strict`: only the legacy
    /// row (also org-less) is a STRICT match, so the target resolves to it.
    #[test]
    fn upsert_resolves_the_legacy_two_org_tie_onto_the_legacy_row() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-ORG-A", "refreshToken": "rt-ORG-A",
                  "expiresAt": 100, "priority": 0 },
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "accessToken": "at-LEGACY", "refreshToken": "rt-LEGACY",
                  "expiresAt": 100, "priority": 1 }
            ] }"#,
        )
        .unwrap();

        upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-NEW", "rt-NEW", 999),
            &ident(Some("u1"), None, None),
        )
        .unwrap();

        assert_eq!(config.accounts.len(), 2, "no new row appended");
        assert_eq!(
            config.accounts[0].access_token, "at-ORG-A",
            "org-a's row must be untouched: {:?}",
            config.accounts[0]
        );
        assert_eq!(
            config.accounts[0].refresh_token.as_deref(),
            Some("rt-ORG-A"),
            "org-a's refresh token is single-use — clobbering it bricks the account"
        );
        assert_eq!(
            config.accounts[1].access_token, "at-NEW",
            "the legacy row (also org-less) is the strict match, not org-a"
        );
        assert_eq!(config.accounts[1].refresh_token.as_deref(), Some("rt-NEW"));
    }

    /// A genuine, unbreakable tie — two rows sharing the exact same identity —
    /// refuses rather than guesses, and writes NOTHING: the config must be
    /// byte-for-byte unchanged after the refusal.
    #[test]
    fn upsert_refuses_and_writes_nothing_on_an_unbreakable_tie() {
        let mut config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-1", "refreshToken": "rt-1",
                  "expiresAt": 100, "priority": 0 },
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-2", "refreshToken": "rt-2",
                  "expiresAt": 100, "priority": 1 }
            ] }"#,
        )
        .unwrap();
        let before = serde_json::to_value(&config).unwrap();

        let err = upsert_account(
            &mut config,
            "me@example.com",
            &tokens("at-NEW", "rt-NEW", 999),
            &ident(Some("u1"), Some("org-a"), Some("Org A")),
        )
        .expect_err("an unbreakable tie must refuse rather than guess");
        assert!(err.to_string().contains("ambiguous"), "{err}");

        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            before,
            "nothing was written on refusal"
        );
    }

    // --- authorize URL ------------------------------------------------------

    #[test]
    fn authorize_url_carries_required_params() {
        // The oauth2-built authorize URL must carry every required OAuth param
        // plus Claude's non-standard `code=true`.
        let url = build_login_flow("http://localhost:12345/callback", None)
            .unwrap()
            .auth_url;
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("code=true"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state="));
        // redirect_uri is percent-encoded by oauth2.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A12345%2Fcallback"));
        // No --account given: no login_hint at all (add-a-new-account path).
        assert!(!url.contains("login_hint"));
    }

    #[test]
    fn authorize_url_carries_login_hint_when_given() {
        let url = build_login_flow("http://localhost:12345/callback", Some("alice@example.com"))
            .unwrap()
            .auth_url;
        assert!(url.contains("login_hint=alice%40example.com"));
    }

    #[tokio::test]
    async fn no_refresh_never_hits_the_endpoint_and_fails_transiently() {
        // The whole point of Fix 1: an offline snapshot's refresher must fail
        // locally (no OAuth token-endpoint call) so a `tcr status` beside a live
        // server can never rotate/revoke its single-use refresh token. Transient,
        // never AuthRejected — it must NOT falsely mark the credential dead.
        let err = NoRefresh
            .refresh(
                crate::provider::Provider::Anthropic,
                "rt-should-never-be-sent".to_string(),
            )
            .await
            .expect_err("NoRefresh must always fail — it never contacts the endpoint");
        assert!(matches!(err, OAuthError::Transient(_)));
        assert_eq!(
            err.to_string(),
            format!("token refresh failed transiently: {NO_REFRESH_MESSAGE}")
        );
    }

    /// A live proxy of the given kind, as `singleton::live_proxy_server` reports
    /// it — pid 4242 throughout, so a message assertion naming it is unambiguous.
    fn incumbent(kind: singleton::ProxyKind) -> Option<singleton::Incumbent> {
        Some(singleton::Incumbent { pid: 4242, kind })
    }

    /// Unwrap a [`LoginRoute::Refuse`]'s message, or panic naming what came back
    /// instead — every existing refusal test still wants the bare message
    /// string, and this is the one place that unwrap lives now that the guard
    /// returns a three-way enum instead of `Option<String>`.
    fn expect_refuse(route: LoginRoute) -> String {
        match route {
            LoginRoute::Refuse(msg) => msg,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn login_guard_refusal_message_carries_pid_and_stop_login_restart_sequence() {
        let msg = expect_refuse(login_guard_refusal(
            incumbent(singleton::ProxyKind::Tcr),
            false,
            3456,
            false,
        ));
        // Actionable + greppable: names the port, the PID, and the escape hatch.
        assert!(msg.contains("3456"), "names the port: {msg}");
        assert!(msg.contains("4242"), "names the pid: {msg}");
        assert!(msg.contains("--force"), "names the escape hatch: {msg}");
        assert_eq!(
            msg.lines().count(),
            1,
            "the refusal is a single line: {msg}"
        );
        // The recovery is the ORDERED stop → login → restart sequence: stop names the
        // exact PID, then `tcr login`, then restart `tcr server`.
        let stop = msg.find("kill 4242").expect("stop step names the pid");
        let login = msg.find("tcr login").expect("login step present");
        let restart = msg.find("restart").expect("restart step present");
        assert!(
            stop < login && login < restart,
            "stop → login → restart, in order: {msg}"
        );
        assert!(
            msg.contains("tcr server"),
            "restart targets the server: {msg}"
        );
    }

    #[test]
    fn login_guard_allows_with_force_or_no_server() {
        // --force is the deliberate escape hatch even when a server is live —
        // it takes the file path regardless of whether the route is there.
        assert_eq!(
            login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), false, 3456, true),
            LoginRoute::File
        );
        // No server on the port → nothing to refuse.
        assert_eq!(
            login_guard_refusal(None, false, 3456, false),
            LoginRoute::File
        );
    }

    /// THE BRIDGE ITEM C FIX: `--force` overrides only the `Refuse` outcome,
    /// never a confirmed-safe `Live` route. The pre-fix code checked `force`
    /// BEFORE `has_add_route`, so `--force` took the file path even when the
    /// live route was confirmed and safe — the exact bug this unit exists to
    /// remove, self-inflicted by the escape hatch. Nobody wants the file
    /// write *because* it is a file write; they want the login not refused.
    #[test]
    fn login_guard_prefers_live_over_force_when_the_route_is_confirmed() {
        assert_eq!(
            login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), true, 3456, true),
            LoginRoute::Live,
            "--force must not choose the file write when the live route IS available and safe"
        );
    }

    /// THE NEW OUTCOME. A live proxy that HAS the add route is no longer
    /// refused at all — the whole point of this unit.
    #[test]
    fn login_guard_proceeds_live_when_the_incumbent_has_the_add_route() {
        assert_eq!(
            login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), true, 3456, false),
            LoginRoute::Live
        );
        // Even an embedded host, whose absent-route refusal has the special
        // "quit, don't kill" wording — once the route is confirmed there is
        // nothing left to refuse, embedded or not.
        assert_eq!(
            login_guard_refusal(
                incumbent(singleton::ProxyKind::TcrEmbedded),
                true,
                3456,
                false
            ),
            LoginRoute::Live
        );
    }

    /// THE CASE THE PROBE-FIRST ORDERING EXISTS FOR: the HTTP probe confirmed
    /// the route, but pid/argv/owner-file detection found NOTHING on the port
    /// (`incumbent: None`) — a host application `singleton::classify_proxy_server`
    /// was never taught to recognize, or a stale/unreadable owner file. The
    /// probe is first-hand evidence and must win on its own; requiring
    /// `incumbent` to agree would silently fall back to `File` — the
    /// whole-file clobber this unit exists to remove — the moment that
    /// unrelated detection heuristic misses.
    #[test]
    fn login_guard_proceeds_live_on_a_confirmed_route_even_when_incumbent_detection_missed_it() {
        assert_eq!(
            login_guard_refusal(None, true, 3456, false),
            LoginRoute::Live
        );
    }

    /// THE ADVICE MUST BE SURVIVABLE. `live_proxy_server` can now name the pid of
    /// the host application that serves the proxy in-process, and telling the
    /// operator to `kill` it is the one action `singleton` is hardened against
    /// twice over: AppKit installs no SIGTERM handler, so the app dies without
    /// `applicationWillTerminate` and the final session→account pin write is lost.
    /// A guard that refuses correctly and then advises the destructive fix is worse
    /// than no guard, because the operator does what the tool says.
    #[test]
    fn the_login_guard_never_tells_an_operator_to_kill_a_host_application() {
        let msg = expect_refuse(login_guard_refusal(
            incumbent(singleton::ProxyKind::TcrEmbedded),
            false,
            3456,
            false,
        ));
        assert!(
            !msg.contains("kill 4242"),
            "must not advise signalling the host application: {msg}"
        );
        assert!(
            !msg.contains("Ctrl-C"),
            "the host application is not a foreground process to interrupt: {msg}"
        );
        assert!(
            msg.contains("quit the host application"),
            "must name the one action that stops an embedded proxy safely: {msg}"
        );
        // Everything the CLI refusal is load-bearing for still holds: one line,
        // names the port, the pid and the escape hatch.
        assert!(
            msg.contains("3456") && msg.contains("4242") && msg.contains("--force"),
            "{msg}"
        );
        assert_eq!(
            msg.lines().count(),
            1,
            "the refusal is a single line: {msg}"
        );

        // And the control: a plain CLI peer, which the operator CAN signal, still
        // gets the kill instruction — so the assertions above are about the kind,
        // not about the message having been softened for everyone.
        let cli = expect_refuse(login_guard_refusal(
            incumbent(singleton::ProxyKind::Tcr),
            false,
            3456,
            false,
        ));
        assert!(cli.contains("kill 4242"), "{cli}");
    }

    #[test]
    fn login_refuses_over_proxy_command_lines_but_allows_an_unrelated_holder() {
        use crate::singleton::is_proxy_server;
        // The real chain: a port holder blocks login IFF its argv is a
        // recognized tcr/teamclaude *server*. Compose the pure classifier with the
        // pure guard decision — refuse / refuse / allow. Argv, not a joined
        // command line: `singleton::classify_proxy_server` takes pre-split argv
        // (as `sysinfo::Process::cmd()` returns it) so a space in an executable's
        // path can never be re-split into the wrong tokens.
        let refuses = |cmd: &[&str]| {
            let argv: Vec<String> = cmd
                .iter()
                .map(|s| s.replace("{js}", crate::legacy_names::JS_PROXY_BIN))
                .collect();
            let server = singleton::classify_proxy_server(&argv)
                .map(|kind| singleton::Incumbent { pid: 4242, kind });
            assert_eq!(
                server.is_some(),
                is_proxy_server(&argv),
                "the bool and the classifying form must agree: {cmd:?}"
            );
            matches!(
                login_guard_refusal(server, false, 3456, false),
                LoginRoute::Refuse(_)
            )
        };
        assert!(
            refuses(&["/opt/{js}-rs/target/release/tcr", "server"]),
            "a live tcr server must block login"
        );
        assert!(
            refuses(&["node", "/opt/nvm/bin/{js}", "server"]),
            "a live JS proxy server must block login"
        );
        assert!(
            !refuses(&["python3", "-m", "http.server", "3456"]),
            "an unrelated process on the port must NOT block login"
        );
    }

    #[test]
    fn login_target_port_reads_config_and_defaults_when_missing() {
        // Reads proxy.port so the guard checks the SAME port the server binds.
        let path =
            std::env::temp_dir().join(format!("tcr-oauth-login-port-{}.json", std::process::id()));
        std::fs::write(&path, r#"{ "proxy": { "port": 4999 } }"#).unwrap();
        assert_eq!(login_target_port(&path), 4999);
        std::fs::remove_file(&path).ok();
        // A missing/unreadable config falls back to the serde default (3456).
        let missing = std::env::temp_dir().join("tcr-oauth-login-port-does-not-exist.json");
        assert_eq!(login_target_port(&missing), 3456);
    }

    // --- probe_add_capability -----------------------------------------------

    #[tokio::test]
    async fn probe_add_capability_reads_a_stamped_400_as_present() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();
        let manager = crate::manager::Manager::with_live_refresher(config.clone(), None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        assert_eq!(probe_add_capability(&config).await, AddCapability::Present);
    }

    /// Bridge item D: an "older tcr" — something answers, but not on this
    /// route at all (no `ENDPOINT_HEADER`, unlike every response this crate's
    /// own router produces, even its 404s and 405s) — is first-hand evidence
    /// a live process holds the port. It must NOT collapse into `Absent`
    /// (renamed from this test's old expectation): folding it in there is
    /// exactly what let `login_route` silently choose `File` beside a real,
    /// answering-but-old server whenever pid detection missed it.
    #[tokio::test]
    async fn probe_add_capability_unstamped_404_is_unusable_not_absent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, axum::Router::new()).await;
        });
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();
        assert!(matches!(
            probe_add_capability(&config).await,
            AddCapability::Unusable(_)
        ));
    }

    /// The genuinely-absent case, for contrast with the test above: nothing
    /// is bound to the port at all, so the connection is refused outright.
    /// This is the ONLY shape that may still resolve to `LoginRoute::File`
    /// without `--force` and without `incumbent` separately confirming a
    /// server.
    #[tokio::test]
    async fn probe_add_capability_connection_refused_is_absent() {
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {dead_port} }} }}"#)).unwrap();
        assert_eq!(probe_add_capability(&config).await, AddCapability::Absent);
    }

    /// The WEDGED shape `cli::probe_incumbent`'s own tests document as
    /// measured and real (`a_listener_that_never_answers_is_silent_not_answering`):
    /// bound, the connect succeeds off the kernel backlog, and nothing is
    /// ever written back. Costs the probe's own 5s deadline by construction.
    #[tokio::test]
    async fn probe_add_capability_wedged_port_is_unusable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

        assert!(matches!(
            probe_add_capability(&config).await,
            AddCapability::Unusable(_)
        ));
        drop(listener);
    }

    #[tokio::test]
    async fn probe_add_capability_wrong_api_key_is_unauthorized_not_absent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#
        ))
        .unwrap();
        let manager = crate::manager::Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // The caller's config carries the WRONG key.
        let client_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
        ))
        .unwrap();
        assert_eq!(
            probe_add_capability(&client_config).await,
            AddCapability::Unauthorized,
            "a rejected api-key must surface as itself, never read as 'no route'"
        );
    }

    // --- login_route ----------------------------------------------------------

    /// THE INTEGRATION-LEVEL VERSION of
    /// `login_guard_proceeds_live_on_a_confirmed_route_even_when_incumbent_detection_missed_it`:
    /// a real live server answering the real route, called with `incumbent:
    /// None` (simulating a pid/argv/owner-file detection MISS — the TcrBar
    /// case, where `argv[0]` ends in `/TcrBar` and
    /// `singleton::classify_proxy_server` recognizes nothing). `login_route`
    /// must still probe and proceed live — it must never gate the probe
    /// itself on `incumbent.is_some()`, which is exactly the shape of bug
    /// this test catches: skipping the probe here would fall through to
    /// `LoginRoute::File` beside a real, answering server.
    #[tokio::test]
    async fn login_route_proceeds_live_even_when_incumbent_detection_missed_a_real_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-missed-incumbent-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(r#"{{ "proxy": {{ "port": {port} }} }}"#);
        std::fs::write(&path, &seed).unwrap();

        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager = crate::manager::Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let route = login_route(&path, None, port, false)
            .await
            .expect("a confirmed route must not error");
        assert_eq!(
            route,
            LoginRoute::Live,
            "the probe must decide this on its own, independent of incumbent detection"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Bridge item C, at the `login_route` level: `--force` must still probe
    /// and still prefer `Live` when a real server confirms the route — never
    /// skip straight to `File` the way the pre-fix early-return did.
    #[tokio::test]
    async fn login_route_prefers_live_over_force_when_a_real_server_confirms_the_route() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-force-live-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(r#"{{ "proxy": {{ "port": {port} }} }}"#);
        std::fs::write(&path, &seed).unwrap();

        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager = crate::manager::Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        let route = login_route(&path, None, port, true)
            .await
            .expect("a confirmed route under --force must not error");
        assert_eq!(
            route,
            LoginRoute::Live,
            "--force must still probe and prefer Live when the route is confirmed"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Bridge item D, at the `login_route` level: a wedged port — something
    /// answers the TCP connect and then never responds — must refuse even
    /// when `incumbent` is `None` (pid/argv detection missed it entirely, the
    /// TcrBar shape). Before this fix, `AddCapability::Absent` covered this
    /// case too, and `login_guard_refusal`'s `None => LoginRoute::File` arm
    /// let it through with no `--force` needed at all.
    #[tokio::test]
    async fn login_route_refuses_a_wedged_port_even_when_incumbent_detection_missed_it() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-wedged-{}-{port}.json",
            std::process::id()
        ));
        std::fs::write(&path, format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

        let err = login_route(&path, None, port, false)
            .await
            .expect_err("a wedged port must refuse, not silently choose File");
        assert!(err.to_string().contains("--force"), "{err}");

        std::fs::remove_file(&path).ok();
        drop(listener);
    }

    /// The escape hatch still works past a wedged-port refusal: `--force`
    /// resolves to `File` rather than erroring out.
    #[tokio::test]
    async fn login_route_force_resolves_to_file_past_a_wedged_port_refusal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-wedged-force-{}-{port}.json",
            std::process::id()
        ));
        std::fs::write(&path, format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

        let route = login_route(&path, None, port, true)
            .await
            .expect("--force must still resolve past a wedged-port refusal");
        assert_eq!(route, LoginRoute::File);

        std::fs::remove_file(&path).ok();
        drop(listener);
    }

    /// THE OVERRULE: `--force` is the escape hatch past a wedged-port
    /// refusal (above), but it must NOT be one past `AddCapability::
    /// Unauthorized`. A rejected api-key is positive evidence the proxy is
    /// alive and healthy — the worst-informed moment to fall back to a
    /// whole-file write beside it — so `login_route` must keep refusing even
    /// under `--force`, and nothing along the way may touch the config file.
    #[tokio::test]
    async fn login_route_refuses_unauthorized_even_under_force() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#
        ))
        .unwrap();
        let manager = crate::manager::Manager::with_live_refresher(server_config, None);
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // The file `login_route` reads carries the WRONG key.
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-route-unauthorized-force-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#);
        std::fs::write(&path, &seed).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = login_route(&path, None, port, true)
            .await
            .expect_err("--force must not rescue a rejected api-key");
        assert!(err.to_string().contains("api-key"), "{err}");
        assert!(
            err.to_string().contains("even under --force"),
            "the message must say --force does not apply here: {err}"
        );

        assert_eq!(
            before,
            std::fs::read_to_string(&path).unwrap(),
            "the file must be byte-identical — --force must not trigger a write here"
        );
        std::fs::remove_file(&path).ok();
    }

    // --- finish_login ---------------------------------------------------------

    /// A [`Profile`] carrying only an email — the common case, no org info.
    fn profile_named(email: &str) -> Profile {
        Profile {
            email: Some(email.to_string()),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
        }
    }

    /// A [`Profile`] carrying an email and a plan — the shape a real profile
    /// fetch returns for a seated org.
    fn profile_with_plan(
        email: &str,
        organization_type: &str,
        rate_limit_tier: Option<&str>,
        seat_tier: Option<&str>,
    ) -> Profile {
        Profile {
            email: Some(email.to_string()),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            organization_type: Some(organization_type.to_string()),
            rate_limit_tier: rate_limit_tier.map(str::to_string),
            seat_tier: seat_tier.map(str::to_string),
        }
    }

    // --- `login_hint` value validation --------------------------------------

    #[test]
    fn looks_like_email_accepts_and_rejects() {
        assert!(looks_like_email("alice@example.com"));
        assert!(!looks_like_email("work"), "a bare display name, no '@'");
        assert!(!looks_like_email("personal-max"), "no '@'");
        assert!(!looks_like_email("@example.com"), "empty local part");
        assert!(!looks_like_email("alice@"), "empty domain");
        assert!(!looks_like_email("alice @example.com"), "whitespace");
        assert!(!looks_like_email("alice@b@c"), "two '@'s");
    }

    // --- `--account` identity assertion (`tcr login --account`) ------------

    fn two_account_seed() -> String {
        r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
              "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 },
            { "name": "bob@example.com", "type": "oauth", "accessToken": "at-bob",
              "refreshToken": "rt-bob", "expiresAt": 1893456000000, "priority": 1 }
        ] }"#
            .to_string()
    }

    /// A mismatch between the requested account and the identity that came
    /// back writes NOTHING — the config file is byte-identical afterwards —
    /// and the error names both sides.
    #[tokio::test]
    async fn finish_login_checked_mismatch_writes_nothing() {
        let seed = two_account_seed();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-mismatch-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &seed).unwrap();
        let mut config = load_or_default(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "alice@example.com",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            profile_named("mallory@example.com"),
            Some("alice@example.com"),
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("alice@example.com"), "names requested: {msg}");
        assert!(msg.contains("mallory@example.com"), "names returned: {msg}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a mismatch must leave the config untouched");

        std::fs::remove_file(&path).ok();
    }

    /// A match writes normally, through the exact same `finish_login` path as
    /// today's no-`--account` flow.
    #[tokio::test]
    async fn finish_login_checked_match_writes_normally() {
        let seed = two_account_seed();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-match-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &seed).unwrap();
        let mut config = load_or_default(&path).unwrap();

        let name = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "alice@example.com",
            &tokens("at-fresh", "rt-fresh", 1_893_456_000_000),
            profile_named("alice@example.com"),
            Some("alice@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(name, "alice@example.com");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("rt-fresh"),
            "a matching identity must be written: {after}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// `fetch_profile`'s all-`None` failure return (`src/oauth.rs:649-654`)
    /// must refuse rather than pass by default — an assertion that cannot be
    /// evaluated is not evidence of a match.
    #[tokio::test]
    async fn finish_login_checked_all_none_profile_refuses() {
        let seed = two_account_seed();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-allnone-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &seed).unwrap();
        let mut config = load_or_default(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let all_none = Profile {
            email: None,
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
        };
        let err = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "alice@example.com",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            all_none,
            Some("alice@example.com"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("alice@example.com"),
            "{}",
            err.to_string()
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an unconfirmable identity must write nothing"
        );

        std::fs::remove_file(&path).ok();
    }

    /// An `--account` naming no row is an error, not a guess — the same
    /// zero/one/many rule `crate::cli::resolve_account` enforces for every other
    /// command, and the arm that survives now that a name can never match two
    /// rows. The query used here is the email PORTION of a migrated row's name,
    /// which is precisely what used to match both of them.
    #[tokio::test]
    async fn finish_login_checked_unmatched_query_is_an_error() {
        let seed = r#"{ "accounts": [
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-1",
              "refreshToken": "rt-1", "expiresAt": 1893456000000, "priority": 0,
              "orgUuid": "org-a", "orgName": "Corp A" },
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-2",
              "refreshToken": "rt-2", "expiresAt": 1893456000000, "priority": 1,
              "orgUuid": "org-b", "orgName": "Corp B" }
        ] }"#;
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-ambiguous-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, seed).unwrap();
        // Migrates: the two rows become `dup@example.com` and
        // `dup@example.com/corp-b`. `corp-b` on its own names neither.
        let mut config = load_or_default(&path).unwrap();
        assert_eq!(config.accounts[1].name, "dup@example.com/corp-b");
        let before = std::fs::read_to_string(&path).unwrap();

        let err = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "dup@example.com",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            profile_named("dup@example.com"),
            Some("corp-b"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("no account matches 'corp-b'"),
            "{}",
            err.to_string()
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "an unmatched query must not guess and write");

        std::fs::remove_file(&path).ok();
    }

    /// The migration is what makes a re-login of one org addressable at all:
    /// the loader gives each row its own name, and `--account <that name>`
    /// resolves to exactly one row and writes only that row's credential.
    ///
    /// This is the same fleet shape that used to be an ambiguous refusal, then
    /// a refusal plus an org flag. Note the assertion is on which row got
    /// `rt-new`: a re-login that guessed would still return a plausible name.
    #[tokio::test]
    async fn finish_login_checked_re_logs_in_the_named_row_of_a_migrated_duplicate() {
        let seed = r#"{ "accounts": [
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-1",
              "refreshToken": "rt-1", "expiresAt": 1893456000000, "priority": 0,
              "accountUuid": "uuid-dup", "orgUuid": "org-a", "orgName": "Corp A" },
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-2",
              "refreshToken": "rt-2", "expiresAt": 1893456000000, "priority": 1,
              "accountUuid": "uuid-dup", "orgUuid": "org-b", "orgName": "Corp B" }
        ] }"#;
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-org-disambig-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, seed).unwrap();
        // Loading migrates the duplicate: Corp A keeps the bare email (lower
        // priority number), Corp B takes its org's slug.
        let mut config = load_or_default(&path).unwrap();
        assert_eq!(config.accounts[0].name, "dup@example.com");
        assert_eq!(config.accounts[1].name, "dup@example.com/corp-b");

        let name = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "dup@example.com",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            profile_with_identity(
                "dup@example.com",
                Some("uuid-dup"),
                Some("org-a"),
                Some("Corp A"),
            ),
            Some("dup@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(name, "dup@example.com");

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let corp_a = after["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["orgUuid"] == "org-a")
            .expect("Corp A row still present");
        assert_eq!(
            corp_a["refreshToken"],
            serde_json::json!("rt-new"),
            "the disambiguated row must be the one that was written: {after}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A [`Profile`] carrying an account uuid and org — the branch the plain
    /// email-only [`profile_named`] can never exercise, and the one the review
    /// found untested (`identity::resolve`'s uuid+org comparison, not merely
    /// its name-equality fallback).
    fn profile_with_identity(
        email: &str,
        account_uuid: Option<&str>,
        org_uuid: Option<&str>,
        org_name: Option<&str>,
    ) -> Profile {
        Profile {
            email: Some(email.to_string()),
            account_uuid: account_uuid.map(str::to_string),
            org_uuid: org_uuid.map(str::to_string),
            org_name: org_name.map(str::to_string),
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
        }
    }

    /// THE CANONICAL FAILURE the review found: `account_uuid` alone cannot
    /// distinguish a person's two orgs. Config holds the SAME person
    /// (`uuid-person`) in both a Corp row and a Personal row, each with its
    /// own org and its own refresh token. Corp's token is dead; the operator
    /// requests `--account` re-login for Corp. The browser — plausibly, this
    /// IS the feature's premise — is signed into Personal instead.
    /// `fetch_profile` returns `{uuid: uuid-person, org: org-personal}`. A
    /// uuid-only comparison takes the `(Some, Some)` arm and passes, because
    /// both rows share the same person. The org-aware check must refuse, and
    /// Corp's own refresh token must be UNCHANGED afterwards — not merely
    /// "an error came back", which a check that still wrote to the wrong row
    /// before erroring would also produce.
    #[tokio::test]
    async fn finish_login_checked_same_uuid_different_org_is_a_mismatch() {
        let seed = r#"{ "accounts": [
            { "name": "me@example.com (Corp)", "type": "oauth", "accessToken": "at-corp",
              "refreshToken": "rt-corp-DEAD", "expiresAt": 1893456000000, "priority": 0,
              "accountUuid": "uuid-person", "orgUuid": "org-corp", "orgName": "Corp" },
            { "name": "me@example.com (Personal)", "type": "oauth", "accessToken": "at-pers",
              "refreshToken": "rt-personal-ALIVE", "expiresAt": 1893456000000, "priority": 1,
              "accountUuid": "uuid-person", "orgUuid": "org-personal", "orgName": "Personal" }
        ] }"#;
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-same-uuid-diff-org-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, seed).unwrap();
        let mut config = load_or_default(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // The browser authenticated as the SAME person, but the Personal org.
        let wrong_org_profile = profile_with_identity(
            "me@example.com",
            Some("uuid-person"),
            Some("org-personal"),
            Some("Personal"),
        );

        let err = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "me@example.com",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            wrong_org_profile,
            Some("me@example.com (Corp)"),
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("me@example.com (Corp)"),
            "names requested: {msg}"
        );
        assert!(
            msg.contains("Personal") || msg.contains("me@example.com (Personal)"),
            "names the org actually authenticated: {msg}"
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "a same-person-different-org mismatch must leave BOTH rows untouched \
             (Corp's dead token in particular must not be silently 'fixed' onto Personal's row)"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A fleet of one Claude row and one Codex row, `bob@example.com/codex`, whose proxy is on
    /// a port nothing listens on, written to a fresh tempdir.
    async fn claude_and_codex_rows() -> (tempfile::TempDir, std::path::PathBuf) {
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::paths::CONFIG_FILE);
        std::fs::write(
            &path,
            format!(
                r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                    {{ "name": "alice@example.com", "accessToken": "at-alice" }},
                    {{ "name": "bob@example.com/codex", "provider": "codex",
                       "accessToken": "at-bob" }}
                ] }}"#
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// `tcr login --account` on a Codex row is refused before the browser sign-in starts and
    /// before any event, naming the row and the two commands that can sign it in, with the code
    /// a GUI titles it by, and nothing is written. The sign-in is Anthropic's, and the identity
    /// it returns can never confirm a Codex row, so going on ended in "sign out and retry" with
    /// the same command, every time.
    ///
    /// Watched red: without the refusal, or with it moved below `authorize_and_wait`, `login`
    /// starts the sign-in and waits for its callback until the timeout below fires.
    #[tokio::test]
    async fn a_relogin_of_a_codex_row_is_refused_before_the_browser_opens() {
        let (_dir, path) = claude_and_codex_rows().await;
        let before = std::fs::read(&path).unwrap();

        let err = tokio::time::timeout(
            Duration::from_secs(5),
            login(
                &path,
                false,
                Some("bob@example.com/codex"),
                None,
                LoginUi::Machine,
            ),
        )
        .await
        .expect("refused before the sign-in, which would wait for its callback")
        .expect_err("`tcr login` cannot sign in a Codex row");
        let refusal = err
            .downcast_ref::<LoginRefusal>()
            .expect("a refusal a GUI can title by its code");
        assert_eq!(refusal.code, "account-is-codex");
        assert_eq!(
            refusal.reason,
            "'bob@example.com/codex' is a Codex account; `tcr login` signs in Claude accounts \
             only. Sign it in again with `tcr login --codex --account bob@example.com/codex`, or \
             re-import it with `tcr login --from-codex --account bob@example.com/codex`. Nothing \
             was written."
        );
        assert_eq!(err.to_string(), refusal.reason);
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    }

    /// `tcr login --codex --account` resolves its row first: a Codex row is the target, a Claude
    /// row is refused with the code a GUI titles it by, before codex runs, and a name no row has
    /// is the error every `--account` gives it.
    ///
    /// Watched red: without the provider check, the Claude row resolves to `Ok(0)`.
    #[tokio::test]
    async fn login_codex_account_on_a_claude_row_is_refused_before_any_event() {
        let (_dir, path) = claude_and_codex_rows().await;
        let config = load_or_default(&path).unwrap();

        let err = codex_relogin_target(&config, "alice@example.com")
            .expect_err("a Claude row is never re-signed by a ChatGPT login");
        let refusal = err.downcast_ref::<LoginRefusal>().expect("a coded refusal");
        assert_eq!(refusal.code, "account-is-claude");
        assert_eq!(
            refusal.reason,
            "'alice@example.com' is a Claude account; `tcr login --codex` signs in Codex \
             accounts only. Sign it in again with `tcr login --account alice@example.com`. \
             Nothing was written."
        );
        assert_eq!(
            codex_relogin_target(&config, "bob@example.com/codex").unwrap(),
            1
        );
        assert_eq!(
            codex_relogin_target(&config, "nobody@example.com")
                .unwrap_err()
                .to_string(),
            "no account matches 'nobody@example.com'"
        );
    }

    /// The identity check refuses a Codex row by name too, for `--from-claude-code --account`,
    /// which reaches it with a credential already in hand: a Codex row is not an account whose
    /// browser signed in as someone else.
    ///
    /// Watched red: without the refusal, the error is the mismatch, "requested re-login for …,
    /// but the browser authenticated as …".
    #[tokio::test]
    async fn the_identity_check_refuses_a_codex_row_by_name() {
        let (_dir, path) = claude_and_codex_rows().await;
        let config = load_or_default(&path).unwrap();

        let err = assert_requested_identity(
            &config,
            "bob@example.com/codex",
            &profile_named("bob@example.com"),
        )
        .expect_err("a Codex row is never confirmed by an Anthropic identity");
        let text = err.to_string();
        assert!(
            text.contains("'bob@example.com/codex' is a Codex account")
                && text.contains("`tcr login` signs in Claude accounts only"),
            "{text}"
        );
    }

    /// The mirror of the canonical failure: the SAME uuid AND the SAME org
    /// must still succeed — the org-aware check is not merely stricter, it is
    /// exactly as permissive as `upsert_account`'s own write-time resolution.
    #[tokio::test]
    async fn finish_login_checked_same_uuid_same_org_matches() {
        let seed = r#"{ "accounts": [
            { "name": "me@example.com (Corp)", "type": "oauth", "accessToken": "at-corp",
              "refreshToken": "rt-corp-DEAD", "expiresAt": 1893456000000, "priority": 0,
              "accountUuid": "uuid-person", "orgUuid": "org-corp", "orgName": "Corp" },
            { "name": "me@example.com (Personal)", "type": "oauth", "accessToken": "at-pers",
              "refreshToken": "rt-personal-ALIVE", "expiresAt": 1893456000000, "priority": 1,
              "accountUuid": "uuid-person", "orgUuid": "org-personal", "orgName": "Personal" }
        ] }"#;
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-same-uuid-same-org-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, seed).unwrap();
        let mut config = load_or_default(&path).unwrap();

        let right_profile = profile_with_identity(
            "me@example.com",
            Some("uuid-person"),
            Some("org-corp"),
            Some("Corp"),
        );

        let name = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "me@example.com (Corp)",
            &tokens("at-fresh-corp", "rt-fresh-corp", 1_893_456_000_000),
            right_profile,
            Some("me@example.com (Corp)"),
        )
        .await
        .unwrap();
        assert_eq!(name, "me@example.com (Corp)");

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let corp = after["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["orgUuid"] == "org-corp")
            .expect("Corp row still present");
        assert_eq!(corp["refreshToken"], serde_json::json!("rt-fresh-corp"));
        let personal = after["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["orgUuid"] == "org-personal")
            .expect("Personal row untouched and still present");
        assert_eq!(
            personal["refreshToken"],
            serde_json::json!("rt-personal-ALIVE"),
            "Personal's own token must be untouched by Corp's re-login"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A requested account with no stored `account_uuid` AND a non-email
    /// display name (e.g. an account named "work") cannot be verified by
    /// email-equality either — there is nothing on file to compare the
    /// browser identity against. The message must not suggest "sign out",
    /// which cannot help here, and it must still refuse (fail closed) rather
    /// than accept blindly.
    #[tokio::test]
    async fn finish_login_checked_non_email_name_with_no_uuid_refuses_honestly() {
        let seed = r#"{ "accounts": [
            { "name": "work", "type": "oauth", "accessToken": "at-work",
              "refreshToken": "rt-work-DEAD", "expiresAt": 1893456000000, "priority": 0 }
        ] }"#;
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-non-email-name-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, seed).unwrap();
        let mut config = load_or_default(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "work",
            &tokens("at-new", "rt-new", 1_893_456_000_000),
            profile_named("someone@example.com"),
            Some("work"),
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            !msg.to_lowercase().contains("sign out"),
            "advising a sign-out that cannot help is dishonest: {msg}"
        );
        assert!(
            msg.contains("no stored account id") || msg.contains("nothing on file"),
            "must state the real reason verification is impossible: {msg}"
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "must still fail closed and write nothing");

        std::fs::remove_file(&path).ok();
    }

    /// No `--account` behaves exactly as before: no identity check runs at
    /// all, so an unrelated returned identity still writes normally (this is
    /// the add-a-new-account path, which must not grow an assertion it
    /// cannot satisfy).
    #[tokio::test]
    async fn finish_login_checked_no_account_skips_the_check() {
        let seed = two_account_seed();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-account-none-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, &seed).unwrap();
        let mut config = load_or_default(&path).unwrap();

        let name = finish_login_checked(
            &path,
            LoginRoute::File,
            &mut config,
            "carol@example.com",
            &tokens("at-carol", "rt-carol", 1_893_456_000_000),
            profile_named("carol@example.com"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(name, "carol@example.com");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("carol@example.com"), "{after}");

        std::fs::remove_file(&path).ok();
    }

    /// THE BITING TEST for the whole-file-write hazard `finish_login` exists to
    /// remove. The documented failure: `login()` loads a config snapshot, does
    /// slow async work (a profile fetch, an unbounded stdin prompt), and THEN
    /// whole-file `config::save`s that now-stale snapshot — silently reverting
    /// any account the live server rotated in the meantime. Refresh tokens are
    /// single-use, so that does not just lose a write, it bricks the reverted
    /// account.
    ///
    /// Reproduced here without a real browser: seed a live server + config file
    /// with two accounts, take a CLIENT-SIDE snapshot (what `login()` would be
    /// holding across its own async window), then have the LIVE SERVER itself
    /// rotate one of those accounts' credentials — via the same
    /// `/_tcr/accounts` route, a real re-login — AFTER the snapshot was taken.
    /// Only then does `finish_login` run, on the STALE snapshot, adding a THIRD
    /// account live. If it whole-file-saved that snapshot, the rotation would
    /// be reverted. It must not be.
    #[tokio::test]
    async fn finish_login_live_path_does_not_clobber_a_concurrent_rotation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-clobber-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }},
                {{ "name": "bob@example.com", "type": "oauth", "accessToken": "at-bob-STALE",
                  "refreshToken": "rt-bob-STALE", "expiresAt": 1893456000000, "priority": 1 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();

        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager =
            crate::manager::Manager::with_live_refresher(server_config, Some(path.clone()));
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // The stale snapshot: what `login()` would have loaded before its own
        // async window (profile fetch / stdin prompt) opened.
        let mut client_config = load_or_default(&path).unwrap();

        // The live server rotates bob's credentials WHILE that snapshot is
        // held — a real re-login through the same route this unit adds,
        // exactly the race `login()`'s doc-comment describes.
        let rotated_bob = Account {
            name: "bob@example.com".to_string(),
            account_type: crate::config::AccountType::OAuth,
            provider: Default::default(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-bob-ROTATED".to_string(),
            refresh_token: Some("rt-bob-ROTATED".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: crate::config::Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: serde_json::Map::new(),
        };
        let rotate_config = load_or_default(&path).unwrap();
        crate::cli::post_add_account(&rotate_config, &rotated_bob)
            .await
            .expect("the live re-login must succeed");

        // Sanity: the rotation really did land on disk before the race window
        // (the `finish_login` call below) closes.
        let after_rotation = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_rotation.contains("rt-bob-ROTATED"),
            "setup: the rotation must be on disk first: {after_rotation}"
        );

        // Now finish a THIRD account's login, live, using the STALE snapshot.
        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "charlie@example.com",
            &tokens("at-charlie", "rt-charlie", 1_893_456_000_000),
            profile_named("charlie@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(name, "charlie@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let accounts = final_doc["accounts"].as_array().unwrap();
        assert_eq!(
            accounts.len(),
            3,
            "alice, rotated bob, and charlie: {final_doc}"
        );
        let bob = accounts
            .iter()
            .find(|a| a["name"] == "bob@example.com")
            .expect("bob still on disk");
        assert_eq!(
            bob["refreshToken"],
            serde_json::json!("rt-bob-ROTATED"),
            "the live add must NOT whole-file-write a stale snapshot back over \
             bob's rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "charlie@example.com"),
            "charlie was added live: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A server that answered and REFUSED us (wrong api-key) must surface,
    /// never silently fall back to writing the file — that silent fallback is
    /// how the two halves end up disagreeing about an account's credentials.
    #[tokio::test]
    async fn finish_login_live_unauthorized_does_not_fall_back_to_file() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seed = format!(r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#);
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-unauthorized-{}-{port}.json",
            std::process::id()
        ));
        std::fs::write(&path, &seed).unwrap();

        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager =
            crate::manager::Manager::with_live_refresher(server_config, Some(path.clone()));
        tokio::spawn(async move { crate::mitm::serve(listener, manager, None).await });

        // The caller's config carries the WRONG key.
        let mut client_config: Config = serde_json::from_str(&format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
        ))
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "dave@example.com",
            &tokens("at-dave", "rt-dave", 1_893_456_000_000),
            profile_named("dave@example.com"),
        )
        .await
        .expect_err("a rejected api-key must not silently write the file");
        assert!(err.to_string().contains("api-key"), "{err}");

        assert_eq!(
            before,
            std::fs::read_to_string(&path).unwrap(),
            "the file must be byte-identical after a surfaced Unauthorized"
        );
        std::fs::remove_file(&path).ok();
    }

    /// The quiet fallback: nothing is listening at all, so there is no live
    /// rotation to disagree with the file — `finish_login` writes it exactly
    /// as the historical file-only path always has.
    #[tokio::test]
    async fn finish_login_no_server_falls_back_to_file_quietly() {
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-noserver-{}-{dead_port}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(r#"{{ "proxy": {{ "port": {dead_port} }}, "accounts": [] }}"#),
        )
        .unwrap();
        let mut config = load_or_default(&path).unwrap();

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut config,
            "erin@example.com",
            &tokens("at-erin", "rt-erin", 1_893_456_000_000),
            profile_named("erin@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(name, "erin@example.com");

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["accounts"][0]["name"],
            serde_json::json!("erin@example.com")
        );
        std::fs::remove_file(&path).ok();
    }

    /// THE σ5 EXPERIMENT bridge item A calls for. The WEDGED shape
    /// `cli::probe_incumbent`'s own tests document as measured and real
    /// (`a_listener_that_never_answers_is_silent_not_answering`): bound, the
    /// connect succeeds off the kernel backlog, and nothing is ever written
    /// back — reached on the `Live` route when the capability probe confirmed
    /// the route seconds earlier and `post_add_account` then hits its own 5s
    /// deadline.
    ///
    /// Reproduces the same race `finish_login_live_path_does_not_clobber_a_concurrent_rotation`
    /// does — a stale client-side snapshot plus a write that lands on disk
    /// AFTER that snapshot was taken — but via `config::save_account` (the
    /// server's own persist primitive) directly, rather than a live HTTP
    /// round-trip: the port in THIS scenario is deliberately silent for the
    /// test's whole lifetime, so nothing can be routed through it. Before
    /// this fix, the `NoAnswer` arm fell through to `persist_via_file`'s
    /// WHOLE-FILE `config::save` of that now-stale snapshot, reverting
    /// alice's rotation — the exact hazard `finish_login` exists to remove,
    /// reintroduced in this one arm (watched failing directly: with that arm
    /// reverted to `persist_via_file`, this test fails with `left:
    /// "rt-alice" right: "rt-alice-ROTATED"`). Fixed, it lands through
    /// `config::save_account`'s surgical single-row write instead, so
    /// alice's rotated row survives and the new account is still added.
    #[tokio::test]
    async fn finish_login_live_timeout_does_not_clobber_other_accounts() {
        // Bound and never accepted: connections queue in the kernel backlog
        // and the connect succeeds, but nothing is ever written back —
        // `post_add_account` times out at its own 5s deadline.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-timeout-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();

        // The stale snapshot: what `login()` would have loaded before its own
        // async window (profile fetch / stdin prompt) opened.
        let mut client_config = load_or_default(&path).unwrap();

        // A rotation lands on disk AFTER that snapshot was taken — the live
        // server's own persist path, used directly since the port itself is
        // silent in this scenario and cannot carry a real round-trip.
        let rotated_alice = Account {
            name: "alice@example.com".to_string(),
            account_type: crate::config::AccountType::OAuth,
            provider: Default::default(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-alice-ROTATED".to_string(),
            refresh_token: Some("rt-alice-ROTATED".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: crate::config::Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: serde_json::Map::new(),
        };
        config::save_account(&path, &rotated_alice).expect("the rotation must land on disk");
        let after_rotation = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_rotation.contains("rt-alice-ROTATED"),
            "setup: the rotation must be on disk first: {after_rotation}"
        );

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "carol@example.com",
            &tokens("at-carol", "rt-carol", 1_893_456_000_000),
            profile_named("carol@example.com"),
        )
        .await
        .expect("a timed-out live add must still fall back to a file write");
        assert_eq!(name, "carol@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let accounts = final_doc["accounts"].as_array().unwrap();
        let alice = accounts
            .iter()
            .find(|a| a["name"] == "alice@example.com")
            .expect("alice must still be on disk");
        assert_eq!(
            alice["refreshToken"],
            serde_json::json!("rt-alice-ROTATED"),
            "the timed-out live add must NOT whole-file-write a stale snapshot back over \
             alice's rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "carol@example.com"),
            "carol was added: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
        drop(listener);
    }

    /// THE σ5 EXPERIMENT for the `NoServer` arm specifically — same shape as
    /// `finish_login_live_path_does_not_clobber_a_concurrent_rotation` and
    /// `finish_login_live_timeout_does_not_clobber_other_accounts`, but for
    /// the arm those two do not reach: the proxy the capability probe found
    /// alive has exited entirely by the time `post_add_account` connects, so
    /// the connection is refused outright (`is_connect()`, not `is_timeout()`
    /// or a 404/405). Before this fix, this arm alone still called
    /// `persist_via_file` directly regardless of what any other test in this
    /// file exercises — a stale client-side snapshot whole-file-written over
    /// a rotation that landed on disk in the profile-fetch / stdin-prompt /
    /// round-trip window. Watched failing directly: with the `NoServer` arm
    /// reverted to `persist_via_file`, this test fails with `left:
    /// "rt-alice" right: "rt-alice-ROTATED"`.
    #[tokio::test]
    async fn finish_login_no_server_does_not_clobber_a_concurrent_rotation() {
        // Bind to grab a free port, then drop the listener immediately: it
        // was live a moment ago (mirroring the capability probe having
        // confirmed it), but by the time `finish_login` runs, nothing is
        // listening and a connect is refused outright.
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-noserver-clobber-{}-{dead_port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {dead_port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();

        // The stale snapshot: what `login()` would have loaded before its own
        // async window (profile fetch / stdin prompt) opened.
        let mut client_config = load_or_default(&path).unwrap();

        // A rotation lands on disk AFTER that snapshot was taken — the
        // (now-gone) server's own persist path, used directly since the port
        // is silent for the whole test and cannot carry a real round-trip.
        let rotated_alice = Account {
            name: "alice@example.com".to_string(),
            account_type: crate::config::AccountType::OAuth,
            provider: Default::default(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-alice-ROTATED".to_string(),
            refresh_token: Some("rt-alice-ROTATED".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: crate::config::Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: serde_json::Map::new(),
        };
        config::save_account(&path, &rotated_alice).expect("the rotation must land on disk");
        let after_rotation = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_rotation.contains("rt-alice-ROTATED"),
            "setup: the rotation must be on disk first: {after_rotation}"
        );

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "carol@example.com",
            &tokens("at-carol", "rt-carol", 1_893_456_000_000),
            profile_named("carol@example.com"),
        )
        .await
        .expect("a NoServer live add must still fall back to a surgical write");
        assert_eq!(name, "carol@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let accounts = final_doc["accounts"].as_array().unwrap();
        let alice = accounts
            .iter()
            .find(|a| a["name"] == "alice@example.com")
            .expect("alice must still be on disk");
        assert_eq!(
            alice["refreshToken"],
            serde_json::json!("rt-alice-ROTATED"),
            "the NoServer live add must NOT whole-file-write a stale snapshot back over \
             alice's rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "carol@example.com"),
            "carol was added: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// THE σ5 EXPERIMENT for `LoginRoute::File` itself — the arm
    /// `persist_via_file` serves directly, not one of its live-path
    /// fallbacks. Reproduces the 2026-08-14 incident: a `tcr login` lands on
    /// `File` (no live proxy answers on the guarded port — the exact
    /// condition every restart window produces), and while that login's own
    /// async window (profile fetch / possible stdin prompt) is open, TWO
    /// things change on disk out from under the config snapshot this call
    /// was handed: an unrelated top-level key gets added (standing in for
    /// `sessionAffinity`, which is exactly this shape — a key with no
    /// modelled field in [`Config`] at all, so nothing short of a
    /// document-level fresh read can preserve it) and a DIFFERENT account's
    /// tokens get rotated by some other writer. Before this fix,
    /// `persist_via_file` whole-file-saved the stale snapshot taken before
    /// this call, silently reverting both — watched failing directly: with
    /// `persist_via_file` reverted to `upsert_account` + whole-file
    /// [`config::save`], this test fails on the `sessionAffinity` assertion
    /// first (the key is entirely unmodelled, so it cannot survive a
    /// snapshot round-trip at all) and, independently, on the rotated
    /// refresh token.
    #[tokio::test]
    async fn finish_login_file_route_does_not_clobber_out_of_band_changes() {
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-file-route-clobber-{}.json",
            std::process::id()
        ));
        let seed = r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
              "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }
        ] }"#;
        std::fs::write(&path, seed).unwrap();

        // The stale snapshot: what `login()` loads before its own async
        // window (profile fetch / possible stdin prompt) opens. No live
        // proxy is involved at all — this is the historical `File` route.
        let mut client_config = load_or_default(&path).unwrap();

        // Out-of-band changes land on disk AFTER that snapshot was taken:
        // an unmodelled top-level key (standing in for `sessionAffinity`)
        // and a rotation of alice's tokens by some other writer.
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        doc["sessionAffinity"] = serde_json::json!(true);
        doc["accounts"][0]["accessToken"] = serde_json::json!("at-alice-ROTATED");
        doc["accounts"][0]["refreshToken"] = serde_json::json!("rt-alice-ROTATED");
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        let after_out_of_band = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_out_of_band.contains("rt-alice-ROTATED")
                && after_out_of_band.contains("sessionAffinity"),
            "setup: both out-of-band changes must be on disk first: {after_out_of_band}"
        );

        let name = finish_login(
            &path,
            LoginRoute::File,
            &mut client_config,
            "carol@example.com",
            &tokens("at-carol", "rt-carol", 1_893_456_000_000),
            profile_named("carol@example.com"),
        )
        .await
        .expect("the File route must still succeed");
        assert_eq!(name, "carol@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            final_doc["sessionAffinity"],
            serde_json::json!(true),
            "the File route must NOT whole-file-write a stale snapshot back over an \
             out-of-band top-level key: {final_doc}"
        );
        let accounts = final_doc["accounts"].as_array().unwrap();
        let alice = accounts
            .iter()
            .find(|a| a["name"] == "alice@example.com")
            .expect("alice must still be on disk");
        assert_eq!(
            alice["refreshToken"],
            serde_json::json!("rt-alice-ROTATED"),
            "the File route must NOT whole-file-write a stale snapshot back over an \
             out-of-band rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "carol@example.com"),
            "carol was added: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A direct exercise of `finish_login`'s `NoRoute` arm. Before this test
    /// it was covered only by inference — every existing clobber test in
    /// this file drives either `Live`'s success path, `NoServer`, or the
    /// timeout/`Unusable` catch-all (`Err(other)`), never the specific
    /// unstamped-404 shape `NoRoute` is. Same "older tcr" server as
    /// `probe_add_capability_unstamped_404_is_unusable_not_absent`: an empty
    /// axum router answers every path, so `post_add_account` sees a 404 with
    /// no `ENDPOINT_HEADER` and classifies it structurally as `NoRoute`
    /// rather than `Unusable`.
    #[tokio::test]
    async fn finish_login_no_route_does_not_clobber_other_accounts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, axum::Router::new()).await;
        });

        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-noroute-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();

        // The stale snapshot: what `login()` would have loaded before its own
        // async window (profile fetch / stdin prompt) opened.
        let mut client_config = load_or_default(&path).unwrap();

        // A rotation lands on disk AFTER that snapshot was taken — this
        // "older tcr" has no add-account route to have produced it, standing
        // in for any other writer that touched the file in the window, same
        // as every sibling clobber test in this file.
        let rotated_alice = Account {
            name: "alice@example.com".to_string(),
            account_type: crate::config::AccountType::OAuth,
            provider: Default::default(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-alice-ROTATED".to_string(),
            refresh_token: Some("rt-alice-ROTATED".to_string()),
            expires_at: Some(1_893_456_000_000),
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: crate::config::Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: serde_json::Map::new(),
        };
        config::save_account(&path, &rotated_alice).expect("the rotation must land on disk");

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "dana@example.com",
            &tokens("at-dana", "rt-dana", 1_893_456_000_000),
            profile_named("dana@example.com"),
        )
        .await
        .expect("a NoRoute live add must still fall back to a surgical write");
        assert_eq!(name, "dana@example.com");

        let final_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let accounts = final_doc["accounts"].as_array().unwrap();
        let alice = accounts
            .iter()
            .find(|a| a["name"] == "alice@example.com")
            .expect("alice must still be on disk");
        assert_eq!(
            alice["refreshToken"],
            serde_json::json!("rt-alice-ROTATED"),
            "the NoRoute live add must NOT whole-file-write a stale snapshot back over \
             alice's rotation: {final_doc}"
        );
        assert!(
            accounts.iter().any(|a| a["name"] == "dana@example.com"),
            "dana was added: {final_doc}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// `persist_via_account_or_file`'s NotFound guard: `config::save_account`'s
    /// `read_document` opens the file with `fs::read_to_string`, which cannot
    /// create one that is not there — so a `NoServer` result reached with NO
    /// config file on disk at all must still fall back to `persist_via_file`
    /// (the same whole-file write `LoginRoute::File` always used) instead of
    /// bailing on the read error. A first-ever login must be able to create
    /// the file exactly as it always has.
    #[tokio::test]
    async fn finish_login_no_server_creates_the_file_when_none_exists() {
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-live-noserver-nofile-{}-{dead_port}.json",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        assert!(!path.exists(), "setup: no config file must exist yet");

        // In-memory only — no file backs this, mirroring what `login()` would
        // hold when there is nothing on disk yet. Must carry `dead_port`
        // explicitly: leaving `proxy.port` at its serde default would send
        // this test's request to whatever real port 3456 happens to hold.
        let mut client_config: Config =
            serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {dead_port} }} }}"#)).unwrap();

        let name = finish_login(
            &path,
            LoginRoute::Live,
            &mut client_config,
            "frank@example.com",
            &tokens("at-frank", "rt-frank", 1_893_456_000_000),
            profile_named("frank@example.com"),
        )
        .await
        .expect("a NoServer live add with no file yet must still create one");
        assert_eq!(name, "frank@example.com");

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["accounts"][0]["name"],
            serde_json::json!("frank@example.com")
        );

        std::fs::remove_file(&path).ok();
    }

    // --- setup-token (`tcr login --token`) ----------------------------------

    /// An all-`None` [`Profile`] — the expected shape for a `claude
    /// setup-token` credential, whose scope is inference-only and so its
    /// `/api/oauth/profile` fetch will very likely fail.
    fn all_none_profile() -> Profile {
        Profile {
            email: None,
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
        }
    }

    /// A setup-token-shaped write: [`finish_login`] given a [`Tokens`] with
    /// `refresh_token: None` (what [`login_with_token`] always constructs)
    /// must write `refreshToken` ABSENT from the JSON — not `null` — and
    /// `expiresAt` set to the assumed one-year expiry.
    ///
    /// Watched failing: with `persist_via_file`'s account literal changed to
    /// `refresh_token: Some("bogus-rt".to_string())` (a plausible slip if
    /// someone "fixed" the E0308 that removing `Some(...)` around the new
    /// `Option<String>` field would otherwise cause), this test failed with
    /// `refreshToken must be ABSENT ... {"refreshToken":"bogus-rt",...}`.
    /// Restored, it passes.
    #[tokio::test]
    async fn setup_token_account_writes_with_refresh_token_absent_and_expires_at_set() {
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-setup-token-write-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, r#"{ "accounts": [] }"#).unwrap();
        let mut config = load_or_default(&path).unwrap();

        let setup_tokens = Tokens {
            id_token: None,
            access_token: "at-setup".to_string(),
            refresh_token: None,
            expires_at_ms: 1_893_456_000_000,
        };

        let name = finish_login(
            &path,
            LoginRoute::File,
            &mut config,
            "gina",
            &setup_tokens,
            all_none_profile(),
        )
        .await
        .expect("a setup-token credential with no refresh token must still write");
        assert_eq!(name, "gina");

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let account = &doc["accounts"][0];
        assert_eq!(account["name"], serde_json::json!("gina"));
        assert!(
            account.get("refreshToken").is_none(),
            "refreshToken must be ABSENT (skip_serializing_if), not written as null or a \
             leftover value: {account}"
        );
        assert_eq!(
            account["expiresAt"],
            serde_json::json!(1_893_456_000_000i64)
        );

        std::fs::remove_file(&path).ok();
    }

    /// `--token` combined with `--account` must refuse and write nothing —
    /// checked byte-for-byte, not just "still has 2 accounts": an inference-
    /// only token carries no identity for `--account` to confirm against.
    ///
    /// Watched failing: with the `account.is_some() || org.is_some()` guard
    /// at the top of `login_with_token` deleted, this test hung waiting on a
    /// real stdin read (the function fell through past the refusal straight
    /// into `read_setup_token`) instead of returning the expected `Err` —
    /// the observable failure for a test with no stdin to feed it. Restored,
    /// it returns immediately with the refusal and the file is untouched.
    #[tokio::test]
    async fn login_with_token_refuses_with_account_and_writes_nothing() {
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-setup-token-refuse-account-{}-{}.json",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, two_account_seed()).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = login_with_token(&path, false, None, Some("alice@example.com"))
            .await
            .expect_err("--token combined with --account must refuse");
        assert!(err.to_string().contains("--account"), "{}", err.to_string());

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "the file must be byte-identical — --token+--account must write nothing"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A login mints its own name, and never prompts on the happy path: the
    /// bare email when nothing carries it, `email/<org-slug>` when something
    /// does. This is the whole mechanism that stops a second org of the same
    /// person from landing on the first one's row.
    #[test]
    fn a_login_mints_the_bare_email_when_free_and_qualifies_it_when_not() {
        let empty: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        let profile = profile_with_identity(
            "me@example.com",
            Some("uuid-person"),
            Some("22222222-2222"),
            Some("Henry Token"),
        );
        assert_eq!(
            mint_login_name(&empty, &profile, None, None, LoginUi::Terminal).unwrap(),
            "me@example.com"
        );

        let occupied: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
        )
        .unwrap();
        assert_eq!(
            mint_login_name(&occupied, &profile, None, None, LoginUi::Terminal).unwrap(),
            "me@example.com/henry-token"
        );

        // No org name on the profile: the org uuid's first eight characters.
        let no_org_name = profile_with_identity(
            "me@example.com",
            Some("uuid-person"),
            Some("22222222-2222"),
            None,
        );
        assert_eq!(
            mint_login_name(&occupied, &no_org_name, None, None, LoginUi::Terminal).unwrap(),
            "me@example.com/22222222"
        );
    }

    /// `--name` wins over the minted name, and is REFUSED when another account
    /// already holds it. Taking a name off an existing row is how a login
    /// overwrites the wrong credential, and it is the one collision the loader's
    /// migration would never see.
    #[test]
    fn a_name_override_wins_but_never_takes_a_name_already_in_use() {
        let occupied: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
        )
        .unwrap();
        let profile = profile_named("bob@example.com");

        assert_eq!(
            mint_login_name(
                &occupied,
                &profile,
                None,
                Some("bob-work"),
                LoginUi::Terminal
            )
            .unwrap(),
            "bob-work"
        );

        let err = mint_login_name(
            &occupied,
            &profile,
            None,
            Some("alice@example.com"),
            LoginUi::Terminal,
        )
        .expect_err("a --name already in use must refuse");
        assert!(
            err.to_string()
                .contains("already the name of another account"),
            "{err}"
        );
    }

    /// A re-login keeps the row's own name rather than re-minting one: that
    /// name is what every pin, group label and `controlAccount` key points at.
    #[test]
    fn a_re_login_keeps_the_rows_existing_name() {
        let config: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "me@example.com/corp", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
        )
        .unwrap();
        let profile = profile_named("me@example.com");
        assert_eq!(
            mint_login_name(
                &config,
                &profile,
                Some("me@example.com/corp"),
                None,
                LoginUi::Terminal
            )
            .unwrap(),
            "me@example.com/corp"
        );
    }

    /// A canned upstream that answers every connection with `status_line` and an
    /// empty body, for [`verify_setup_token`]. Returns its base URL.
    async fn canned_upstream(status_line: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 {status_line}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// The bug this guards: a pasted token upstream refuses with 401 must be
    /// classified `Rejected`, so `login_with_token` writes nothing. Before the
    /// check existed the account was stored and reported `active` while every
    /// request it served came back 401.
    #[tokio::test]
    async fn verify_setup_token_classifies_401_as_rejected() {
        let upstream = canned_upstream("401 Unauthorized").await;
        assert_eq!(
            verify_setup_token(&upstream, "bad-token").await,
            SetupTokenCheck::Rejected { status: 401 }
        );
    }

    #[tokio::test]
    async fn verify_setup_token_classifies_403_as_rejected() {
        let upstream = canned_upstream("403 Forbidden").await;
        assert_eq!(
            verify_setup_token(&upstream, "refused-token").await,
            SetupTokenCheck::Rejected { status: 403 }
        );
    }

    /// A 2xx is acceptance, and so is a 429: a quota answer is one that got past
    /// auth, and refusing a valid-but-exhausted token would block the operator
    /// from adding an account they mean to use once its window resets.
    #[tokio::test]
    async fn verify_setup_token_accepts_2xx_and_429() {
        let ok = canned_upstream("200 OK").await;
        assert_eq!(
            verify_setup_token(&ok, "good-token").await,
            SetupTokenCheck::Accepted
        );
        let exhausted = canned_upstream("429 Too Many Requests").await;
        assert_eq!(
            verify_setup_token(&exhausted, "exhausted-token").await,
            SetupTokenCheck::Accepted
        );
    }

    /// No upstream answer at all proves nothing about the token — the caller
    /// stores it with a warning rather than refusing an offline operator.
    #[tokio::test]
    async fn verify_setup_token_reports_dead_upstream_as_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(matches!(
            verify_setup_token(&format!("http://{addr}"), "any-token").await,
            SetupTokenCheck::Unreachable { .. }
        ));
    }

    /// An all-`None` profile plus `--name` must produce that exact name,
    /// without ever reaching the stdin prompt — the `--name` branch is taken
    /// instead of falling through to `prompt_account_name`. This is the
    /// setup-token case: an inference-only credential's profile fetch normally
    /// comes back empty, so `--name` is the only name there is.
    #[test]
    fn a_name_override_skips_the_prompt_on_an_empty_profile() {
        let empty: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        let name = mint_login_name(
            &empty,
            &all_none_profile(),
            None,
            Some("bob-work"),
            LoginUi::Terminal,
        )
        .unwrap();
        assert_eq!(name, "bob-work");
    }

    /// `--name` wins over the profile's email too. It used to be the other way
    /// round, which made the flag unusable exactly when a person wanted to name
    /// their second org's row something they would recognise.
    #[test]
    fn a_name_override_wins_over_the_profile_email() {
        let empty: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        let name = mint_login_name(
            &empty,
            &profile_named("carol@example.com"),
            None,
            Some("carol-personal"),
            LoginUi::Terminal,
        )
        .unwrap();
        assert_eq!(name, "carol-personal");
    }

    // --- the `unnamed` fallback ---------------------------------------------

    /// An `Account` carrying nothing but a name — the only field
    /// [`unnamed_fallback`] reads.
    fn named_account(name: &str) -> Account {
        Account {
            name: name.to_string(),
            account_type: crate::config::AccountType::OAuth,
            provider: Default::default(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: "at-placeholder".to_string(),
            refresh_token: None,
            expires_at: None,
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: crate::config::Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn unnamed_fallback_is_plain_unnamed_when_free() {
        assert_eq!(unnamed_fallback(&[]), "unnamed");
        assert_eq!(
            unnamed_fallback(&[named_account("alice@example.com")]),
            "unnamed"
        );
    }

    /// The collision case, and the reason this function exists at all: an
    /// inference-only account has no email and no uuid, so `identity::resolve`
    /// can only match it by NAME. Handing out `unnamed` twice would resolve the
    /// second login onto the first row and overwrite its token instead of
    /// adding an account.
    #[test]
    fn unnamed_fallback_skips_taken_names() {
        assert_eq!(unnamed_fallback(&[named_account("unnamed")]), "unnamed-2");
        assert_eq!(
            unnamed_fallback(&[named_account("unnamed"), named_account("unnamed-2")]),
            "unnamed-3"
        );
    }

    /// Gaps are filled rather than skipped past — the name is chosen by
    /// scanning for the first FREE suffix, never by counting rows, so removing
    /// `unnamed-2` and adding an account reuses that slot instead of colliding
    /// with `unnamed-3`. A count-derived fallback gets this wrong.
    #[test]
    fn unnamed_fallback_fills_a_gap_left_by_a_removed_account() {
        let accounts = [named_account("unnamed"), named_account("unnamed-3")];
        assert_eq!(unnamed_fallback(&accounts), "unnamed-2");
    }
    // --- the Claude Code import ----------------------------------------------

    /// A fake Claude Code credentials document, values invented.
    const FAKE_CLAUDE_CODE_JSON: &str = r#"{
      "claudeAiOauth": {
        "accessToken": "at-imported-FAKE",
        "refreshToken": "rt-imported-FAKE",
        "expiresAt": 1789000000000,
        "subscriptionType": "max"
      }
    }"#;

    /// The whole point of importing this credential rather than a
    /// `setup-token` one: it carries a refresh token and a real expiry, so the
    /// row lands able to refresh itself. Mutation note: drop `refresh_token`
    /// in `tokens_from_claude_code_login` (the shape `login_with_token` has to
    /// use) and the `has_refresh` assertion fails; stamp
    /// `SETUP_TOKEN_ASSUMED_LIFETIME_MS` instead of the document's own
    /// `expiresAt` and the expiry assertion fails.
    #[tokio::test]
    async fn an_imported_claude_code_login_lands_with_its_refresh_token_and_expiry() {
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-claude-code-import-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, r#"{ "accounts": [] }"#).unwrap();
        let mut config = load_or_default(&path).unwrap();

        let login = crate::claude_code_login::parse(FAKE_CLAUDE_CODE_JSON)
            .expect("the fake document parses")
            .expect("it carries a login");
        let tokens = tokens_from_claude_code_login(login);

        let name = finish_login(
            &path,
            LoginRoute::File,
            &mut config,
            "imported@example.com",
            &tokens,
            profile_named("imported@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(name, "imported@example.com");

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let row = &doc["accounts"][0];
        assert_eq!(row["name"], serde_json::json!("imported@example.com"));
        assert!(
            row["refreshToken"].is_string(),
            "an imported Claude Code login must land WITH its refresh token"
        );
        assert_eq!(
            row["expiresAt"],
            serde_json::json!(1_789_000_000_000i64),
            "the row's expiry must be the document's own `expiresAt`, in ms"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A credential with no `expiresAt` is stamped as already expired, so the
    /// first use refreshes it — never the year-long `--token` assumption,
    /// which would leave a dead credential looking healthy for months.
    #[test]
    fn a_claude_code_login_with_no_expiry_is_stamped_as_due_a_refresh() {
        let login = crate::claude_code_login::parse(
            r#"{"claudeAiOauth": {"accessToken": "at-FAKE", "refreshToken": "rt-FAKE"}}"#,
        )
        .expect("parses")
        .expect("has a login");
        let before = crate::now_ms();
        let tokens = tokens_from_claude_code_login(login);
        assert!(
            tokens.expires_at_ms >= before && tokens.expires_at_ms <= crate::now_ms(),
            "an unknown expiry must stamp NOW (got {}), not a year out",
            tokens.expires_at_ms
        );
    }

    /// Seconds-vs-milliseconds is the classic way an imported expiry lands
    /// wrong; the same `normalize_expires_at` every other path uses guards it.
    #[test]
    fn a_seconds_expiry_is_normalized_to_milliseconds() {
        let login = crate::claude_code_login::parse(
            r#"{"claudeAiOauth": {"accessToken": "at-FAKE", "expiresAt": 1789000000}}"#,
        )
        .expect("parses")
        .expect("has a login");
        assert_eq!(
            tokens_from_claude_code_login(login).expires_at_ms,
            1_789_000_000_000
        );
    }

    /// The plan is shown back as confirmation of WHICH login was taken, and an
    /// absent one must not render as `()` or `(None)`.
    #[test]
    fn the_plan_suffix_is_empty_when_there_is_no_plan() {
        assert_eq!(plan_suffix(Some("max")), " (max)");
        assert_eq!(plan_suffix(None), "");
        assert_eq!(plan_suffix(Some("  ")), "");
    }

    /// The consequence is stated, not hidden — and stated once, from one
    /// constant, so the explicit verb and the auto-import cannot drift.
    #[test]
    fn the_import_consequence_names_the_re_login_cost() {
        assert!(
            CLAUDE_CODE_IMPORT_CONSEQUENCE.contains("log in again")
                && CLAUDE_CODE_IMPORT_CONSEQUENCE.contains("single-use"),
            "the user must be told the `claude` CLI will need one browser login: \
             {CLAUDE_CODE_IMPORT_CONSEQUENCE}"
        );
    }

    // --- Codex import (`tcr login --from-codex`) ------------------------------

    const CODEX_ACCOUNT_A: &str = "11111111-1111-4111-8111-111111111111";
    const CODEX_ACCOUNT_B: &str = "22222222-2222-4222-8222-222222222222";
    /// The ChatGPT user of alice's login, in workspace [`CODEX_ACCOUNT_A`].
    const CODEX_USER_A: &str = "55555555-5555-4555-8555-555555555555";

    /// A config holding exactly these rows, each `(name, provider, account id)`.
    fn config_with_rows(rows: &[(&str, crate::provider::Provider, Option<&str>)]) -> Config {
        let mut config: Config = serde_json::from_str("{}").unwrap();
        for (name, provider, account_id) in rows {
            config.accounts.push(crate::identity::probe(
                *provider,
                name,
                account_id.map(str::to_string),
                None,
                None,
            ));
        }
        config
    }

    #[test]
    fn mint_codex_login_name_prefers_email_then_suffixes_codex_when_an_anthropic_row_holds_it() {
        use crate::provider::Provider;
        let empty = config_with_rows(&[]);
        assert_eq!(
            mint_codex_login_name(
                &empty,
                Some("alice@example.com"),
                Some(CODEX_ACCOUNT_A),
                None,
                None
            )
            .unwrap(),
            "alice@example.com"
        );
        let anthropic = config_with_rows(&[("alice@example.com", Provider::Anthropic, None)]);
        assert_eq!(
            mint_codex_login_name(
                &anthropic,
                Some("alice@example.com"),
                Some(CODEX_ACCOUNT_A),
                None,
                None
            )
            .unwrap(),
            "alice@example.com/codex"
        );
    }

    /// A re-import of the same Codex login updates its row in place, so it keeps
    /// the name that row has: the bare email, or the suffixed one when an
    /// Anthropic row holds the email.
    #[test]
    fn mint_codex_login_name_reuses_an_existing_codex_row_name() {
        use crate::provider::Provider;
        let codex =
            config_with_rows(&[("alice@example.com", Provider::Codex, Some(CODEX_ACCOUNT_A))]);
        assert_eq!(
            mint_codex_login_name(
                &codex,
                Some("alice@example.com"),
                Some(CODEX_ACCOUNT_A),
                None,
                None
            )
            .unwrap(),
            "alice@example.com"
        );
        let beside = config_with_rows(&[
            ("alice@example.com", Provider::Anthropic, None),
            (
                "alice@example.com/codex",
                Provider::Codex,
                Some(CODEX_ACCOUNT_A),
            ),
        ]);
        assert_eq!(
            mint_codex_login_name(
                &beside,
                Some("alice@example.com"),
                Some(CODEX_ACCOUNT_A),
                None,
                None
            )
            .unwrap(),
            "alice@example.com/codex"
        );
    }

    #[test]
    fn mint_codex_login_name_requires_a_name_when_the_token_has_no_email() {
        let empty = config_with_rows(&[]);
        let err = mint_codex_login_name(&empty, None, Some(CODEX_ACCOUNT_A), None, None)
            .expect_err("no email and no --name is no name");
        assert!(err.to_string().contains("--name"), "{err}");
        assert_eq!(
            mint_codex_login_name(&empty, None, Some(CODEX_ACCOUNT_A), None, Some("work")).unwrap(),
            "work"
        );
    }

    /// A second ChatGPT account under the same email (a personal and a Team
    /// workspace) is a second row, so it needs a name of its own: the write ADDS
    /// it, and a row named like the first would be a duplicate name.
    #[test]
    fn mint_codex_login_name_gives_another_codex_account_of_the_same_email_its_own_name() {
        use crate::provider::Provider;
        let first =
            config_with_rows(&[("alice@example.com", Provider::Codex, Some(CODEX_ACCOUNT_A))]);
        assert_eq!(
            mint_codex_login_name(
                &first,
                Some("alice@example.com"),
                Some(CODEX_ACCOUNT_B),
                None,
                None
            )
            .unwrap(),
            "alice@example.com/codex"
        );
        let suffixed = config_with_rows(&[
            ("alice@example.com", Provider::Anthropic, None),
            (
                "alice@example.com/codex",
                Provider::Codex,
                Some(CODEX_ACCOUNT_A),
            ),
        ]);
        assert_eq!(
            mint_codex_login_name(
                &suffixed,
                Some("alice@example.com"),
                Some(CODEX_ACCOUNT_B),
                None,
                None
            )
            .unwrap(),
            "alice@example.com/codex-2"
        );
    }

    /// `--name` must be free: another account's name, of either provider, is
    /// refused rather than shared. The name of this login's own row is not
    /// another account's, so a re-import may name the row it updates.
    #[test]
    fn mint_codex_login_name_refuses_a_name_another_account_holds() {
        use crate::provider::Provider;
        let config = config_with_rows(&[
            ("work", Provider::Anthropic, None),
            ("team", Provider::Codex, Some(CODEX_ACCOUNT_B)),
        ]);
        for taken in ["work", "team"] {
            let err = mint_codex_login_name(
                &config,
                Some("alice@example.com"),
                Some(CODEX_ACCOUNT_A),
                None,
                Some(taken),
            )
            .expect_err("another account's name must be refused");
            assert!(
                err.to_string()
                    .contains("already the name of another account"),
                "{taken}: {err}"
            );
        }
        let own = config_with_rows(&[("work", Provider::Codex, Some(CODEX_ACCOUNT_A))]);
        assert_eq!(
            mint_codex_login_name(
                &own,
                Some("alice@example.com"),
                Some(CODEX_ACCOUNT_A),
                None,
                Some("work")
            )
            .unwrap(),
            "work"
        );
    }

    /// A Codex import over the LIVE route reaches the running proxy as a Codex
    /// account, and lands beside the Anthropic row that holds its email, never on
    /// it. That Anthropic row even carries the Codex login's own account id, the
    /// strongest pull toward a match there is, so only the provider keeps the two
    /// apart: this fails if the provider is lost anywhere on the way, in the row
    /// the import builds, in the request body or at the route.
    ///
    /// Watched red: let `proxy::AddAccountRequest` drop `provider` and the server
    /// files the import as an Anthropic account, resolves it onto alice's row by
    /// the shared id and overwrites alice's credential with the Codex one.
    #[tokio::test]
    async fn a_codex_import_over_the_live_route_lands_as_codex_beside_the_anthropic_row() {
        use crate::provider::Provider;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-codex-live-{}-{port}.json",
            std::process::id()
        ));
        let seed = format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accountUuid": "{CODEX_USER_A}",
                  "accessToken": "at-alice", "refreshToken": "rt-alice",
                  "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
        );
        std::fs::write(&path, &seed).unwrap();
        let server_config: Config = serde_json::from_str(&seed).unwrap();
        let manager =
            crate::manager::Manager::with_live_refresher(server_config, Some(path.clone()));
        let served = std::sync::Arc::clone(&manager);
        tokio::spawn(async move { crate::mitm::serve(listener, served, None).await });

        let login = crate::codex_login::CodexLogin {
            access_token: "at-codex".to_string(),
            refresh_token: "rt-codex".to_string(),
            account_id: Some(CODEX_ACCOUNT_A.to_string()),
            user_id: Some(CODEX_USER_A.to_string()),
            email: Some("alice@example.com".to_string()),
            plan_type: Some("pro".to_string()),
            expires_at_ms: Some(1_800_000_000_000),
            is_fedramp: false,
        };
        let config = load_or_default(&path).unwrap();
        let name = mint_codex_login_name(
            &config,
            login.email.as_deref(),
            login.user_id.as_deref(),
            login.account_id.as_deref(),
            None,
        )
        .unwrap();
        assert_eq!(name, "alice@example.com/codex");
        let saved = finish_login_account(
            &path,
            LoginRoute::Live,
            &config,
            codex_account_row(name, login, None),
        )
        .await
        .unwrap();
        assert_eq!(saved, "alice@example.com/codex");

        // The running proxy: the import is a Codex account of its own, and alice
        // is still an Anthropic account holding her own credential.
        assert_eq!(manager.account_count(), 2, "added beside, not onto, alice");
        let index_of = |name: &str| {
            (0..manager.account_count())
                .find(|&i| manager.account_name(i).as_deref() == Some(name))
                .unwrap_or_else(|| panic!("no live account named {name}"))
        };
        let codex = index_of("alice@example.com/codex");
        assert_eq!(manager.provider(codex), Some(Provider::Codex));
        assert_eq!(manager.access_token(codex).as_deref(), Some("at-codex"));
        let alice = index_of("alice@example.com");
        assert_eq!(manager.provider(alice), Some(Provider::Anthropic));
        assert_eq!(manager.access_token(alice).as_deref(), Some("at-alice"));

        // The file the server persisted: the same two rows.
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let rows = doc["accounts"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "{doc}");
        assert_eq!(rows[0]["name"], "alice@example.com");
        assert_eq!(rows[0]["accessToken"], "at-alice");
        assert!(
            rows[0].get("provider").is_none(),
            "alice stays Anthropic: {doc}"
        );
        assert_eq!(rows[1]["name"], "alice@example.com/codex");
        assert_eq!(rows[1]["provider"], "codex");
        assert_eq!(rows[1]["accessToken"], "at-codex");
        assert_eq!(rows[1]["accountUuid"], CODEX_USER_A, "the ChatGPT user");
        assert_eq!(rows[1]["orgUuid"], CODEX_ACCOUNT_A, "the workspace");

        std::fs::remove_file(&path).ok();
    }

    // --- import_codex_login: the provider gate, the echo, the re-import rules --

    /// alice's Codex login for account A, expiring at `expires_at_ms`, with these
    /// fake tokens.
    fn alice_codex_login(
        expires_at_ms: i64,
        access_token: &str,
        refresh_token: &str,
    ) -> crate::codex_login::CodexLogin {
        crate::codex_login::CodexLogin {
            access_token: access_token.to_string(),
            refresh_token: refresh_token.to_string(),
            account_id: Some(CODEX_ACCOUNT_A.to_string()),
            user_id: Some(CODEX_USER_A.to_string()),
            email: Some("alice@example.com".to_string()),
            plan_type: Some("pro".to_string()),
            expires_at_ms: Some(expires_at_ms),
            is_fedramp: false,
        }
    }

    /// A config file in a fresh tempdir whose proxy is on `port`, whose `codexUpstream` is
    /// `codex_upstream` and whose `accounts` array is `accounts` (JSON).
    ///
    /// An import that gets past its local checks asks `codexUpstream` where the login's
    /// workspace is served before it writes, so every test names a loopback stub here
    /// ([`accounts_check_stub`]) and none reaches chatgpt.com.
    fn import_config(
        port: u16,
        codex_upstream: &str,
        accounts: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::paths::CONFIG_FILE);
        std::fs::write(
            &path,
            format!(
                r#"{{ "proxy": {{ "port": {port} }}, "codexUpstream": "{codex_upstream}", "accounts": {accounts} }}"#
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// One request [`accounts_check_stub`] received: its path, and the bearer and ChatGPT
    /// account id it carried.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SeenCheck {
        path: String,
        bearer: Option<String>,
        account_id: Option<String>,
    }

    /// A stub `codexUpstream` on 127.0.0.1:0 that answers every request `status` with `body`
    /// and records it. Returns its base URL and the record.
    async fn accounts_check_stub(
        status: u16,
        body: &str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<SeenCheck>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&seen);
        let body = body.to_string();
        let origin = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let recorded = std::sync::Arc::clone(&recorded);
            let body = body.clone();
            async move {
                let header = |name: &str| {
                    req.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                recorded.lock().unwrap().push(SeenCheck {
                    path: req.uri().path().to_string(),
                    bearer: header("authorization"),
                    account_id: header(crate::provider::CODEX_ACCOUNT_HEADER),
                });
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, origin).await;
        });
        (format!("http://{addr}"), seen)
    }

    /// An accounts/check answer naming account A's workspace with these two routing values.
    fn workspace_answer(origin: &str, routing: &str) -> String {
        format!(
            r#"{{"accounts":{{"{CODEX_ACCOUNT_A}":{{"workspace_backend_origin":"{origin}","account_routing_override":"{routing}","plan_type":"pro"}}}},"account_ordering":["{CODEX_ACCOUNT_A}"]}}"#
        )
    }

    /// A stub `codexUpstream` that clears account A's workspace for the pool.
    async fn unconstrained_upstream() -> String {
        accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT"))
            .await
            .0
    }

    /// The one request an import sends `codexUpstream`: the workspace check, with the login's
    /// own bearer and account id.
    fn workspace_check_of(bearer: &str) -> SeenCheck {
        SeenCheck {
            path: crate::codex_login::ACCOUNTS_CHECK_PATH.to_string(),
            bearer: Some(format!("Bearer {bearer}")),
            account_id: Some(CODEX_ACCOUNT_A.to_string()),
        }
    }

    /// A stub of the account-add route on 127.0.0.1:0, recording every request
    /// body it receives. Every answer is stamped with the route's
    /// `ENDPOINT_HEADER`, as every tcr with the route stamps them; `advertise`
    /// decides whether it also names its providers (a current server) or not (a
    /// server that predates them). The blank probe gets the route's 400; a real
    /// add gets a 200 whose `provider` is `echo` (absent when `None`, as an older
    /// server's body is).
    async fn stub_add_route(
        advertise: bool,
        echo: Option<crate::provider::Provider>,
    ) -> (
        u16,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        use axum::response::IntoResponse as _;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&seen);
        let route = axum::Router::new().route(
            crate::proxy::ADD_ACCOUNT_PATH,
            axum::routing::post(move |body: axum::body::Bytes| {
                let recorded = std::sync::Arc::clone(&recorded);
                async move {
                    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    recorded.lock().unwrap().push(doc.clone());
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert(
                        crate::proxy::ENDPOINT_HEADER,
                        axum::http::HeaderValue::from_static(crate::proxy::ADD_ACCOUNT_ENDPOINT),
                    );
                    if advertise {
                        headers.insert(
                            crate::proxy::PROVIDERS_HEADER,
                            axum::http::HeaderValue::from_static("anthropic,codex"),
                        );
                    }
                    let name = doc["name"].as_str().unwrap_or_default().to_string();
                    if name.is_empty() {
                        let refusal = r#"{"type":"error","error":{"type":"invalid_request_error","message":"\"name\" (string) is required and must not be empty."}}"#;
                        return (axum::http::StatusCode::BAD_REQUEST, headers, refusal)
                            .into_response();
                    }
                    let mut applied = serde_json::json!({
                        "name": name, "added": true, "index": 0, "persisted": true, "warning": null
                    });
                    if let Some(echo) = echo {
                        applied["provider"] = serde_json::json!(echo);
                    }
                    (axum::http::StatusCode::OK, headers, applied.to_string()).into_response()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, route).await;
        });
        (port, seen)
    }

    /// THE GATE. A live proxy that answers the route but names no providers predates
    /// Codex accounts: it would drop the row's `provider`, store this login as a Claude
    /// account and send its tokens to Anthropic. The import refuses — `--force` included
    /// — writes nothing, and posts nothing but the blank probe.
    #[tokio::test]
    async fn a_codex_import_is_refused_by_a_live_proxy_that_names_no_providers() {
        let (port, seen) = stub_add_route(false, None).await;
        let (_dir, path) = import_config(
            port,
            &unconstrained_upstream().await,
            r#"[{ "name": "alice@example.com", "accessToken": "at-alice" }]"#,
        );
        let before = std::fs::read(&path).unwrap();

        for force in [false, true] {
            let err = import_codex_login(
                &path,
                force,
                None,
                None,
                alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
                None,
            )
            .await
            .expect_err("a proxy that predates Codex accounts must refuse the import");
            let text = err.to_string();
            assert!(
                text.contains(&format!(":{port}"))
                    && text.contains("predates Codex accounts")
                    && text.contains("store this login as a Claude account")
                    && text.contains("send its token to Anthropic")
                    && text.contains("Restart the proxy on this build"),
                "force={force}: {text}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "force={force}: nothing may be written"
            );
        }
        let bodies = seen.lock().unwrap();
        assert_eq!(
            bodies.len(),
            2,
            "one probe per attempt and nothing else: {bodies:?}"
        );
        assert!(
            bodies
                .iter()
                .all(|body| body["name"] == "" && body["accessToken"] == ""),
            "only the blank probe may reach it, never the login: {bodies:?}"
        );
    }

    /// The gate's other side, against the real server: it names `codex`, so the import
    /// takes the live route and lands as a Codex account beside alice's Anthropic row,
    /// in the running proxy and in the file it persists — at the credential version an
    /// older proxy refuses.
    #[tokio::test]
    async fn a_codex_import_over_a_proxy_that_names_codex_lands_live_as_codex() {
        use crate::provider::Provider;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_dir, path) = import_config(
            port,
            &unconstrained_upstream().await,
            &format!(
                r#"[{{ "name": "alice@example.com", "type": "oauth", "accountUuid": "{CODEX_ACCOUNT_A}",
                      "accessToken": "at-alice", "refreshToken": "rt-alice",
                      "expiresAt": 1893456000000, "priority": 0 }}]"#
            ),
        );
        let server_config: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let manager =
            crate::manager::Manager::with_live_refresher(server_config, Some(path.clone()));
        let served = std::sync::Arc::clone(&manager);
        tokio::spawn(async move { crate::mitm::serve(listener, served, None).await });

        let saved = import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .expect("a server that names codex takes the import live");
        assert_eq!(saved, "alice@example.com/codex");

        let codex = (0..manager.account_count())
            .find(|&i| manager.account_name(i).as_deref() == Some("alice@example.com/codex"))
            .expect("the import is in the live rotation");
        assert_eq!(manager.provider(codex), Some(Provider::Codex));
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let rows = doc["accounts"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "{doc}");
        assert!(rows[0].get("provider").is_none(), "alice stays Anthropic");
        assert_eq!(rows[1]["provider"], "codex");
        assert_eq!(rows[1]["credentialVersion"], 2, "{doc}");
    }

    /// Behind the gate, the echo: a server that names `codex` but answers the add with
    /// another provider, or none, stored the row as a Claude account. That is a hard
    /// error telling the user to remove the row it names, never a success line.
    #[tokio::test]
    async fn a_live_add_not_echoed_as_codex_is_a_hard_error_naming_the_row_to_remove() {
        for echo in [None, Some(crate::provider::Provider::Anthropic)] {
            let (port, seen) = stub_add_route(true, echo).await;
            let (_dir, path) = import_config(port, &unconstrained_upstream().await, "[]");

            let err = import_codex_login(
                &path,
                false,
                None,
                None,
                alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
                None,
            )
            .await
            .expect_err("an add not confirmed as Codex must fail loudly");
            let text = err.to_string();
            assert!(
                text.contains("live, and on disk, as a Claude account")
                    && text.contains("may already have reached Anthropic")
                    && text.contains("tcr remove 'alice@example.com'")
                    && text.contains("`codex login` again"),
                "echo {echo:?}: {text}"
            );
            assert_eq!(
                seen.lock().unwrap().len(),
                2,
                "echo {echo:?}: the add was posted, and the answer is what failed"
            );
        }
    }

    /// A re-import of a login already saved under another name than the one the
    /// mint would pick (its email is free again) keeps the stored name — the write
    /// updates that row in place — and a `--name` that is not that name is refused,
    /// since the write would rename nothing.
    #[tokio::test]
    async fn a_re_import_keeps_the_name_its_row_is_saved_under() {
        let port = 0;
        let (_dir, path) = import_config(
            port,
            &unconstrained_upstream().await,
            &format!(
                r#"[{{ "name": "alice@example.com/codex", "provider": "codex",
                      "accountUuid": "{CODEX_USER_A}", "orgUuid": "{CODEX_ACCOUNT_A}", "accessToken": "at-old",
                      "refreshToken": "rt-old", "expiresAt": 1700000000000 }}]"#
            ),
        );

        let saved = import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-new", "rt-new"),
            None,
        )
        .await
        .expect("a re-import updates its row");
        assert_eq!(
            saved, "alice@example.com/codex",
            "the stored name, not the email"
        );
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let rows = doc["accounts"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{doc}");
        assert_eq!(rows[0]["name"], "alice@example.com/codex");
        assert_eq!(rows[0]["accessToken"], "at-new");

        let before = std::fs::read(&path).unwrap();
        let err = import_codex_login(
            &path,
            false,
            Some("work"),
            None,
            alice_codex_login(1_900_000_000_000, "at-newer", "rt-newer"),
            None,
        )
        .await
        .expect_err("a --name that is not the row's name renames nothing and is refused");
        assert!(
            err.to_string()
                .contains("this login is already saved as 'alice@example.com/codex'"),
            "{err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    }

    /// Re-importing an older credential, such as one left after a skipped write-back, over tcr's newer one
    /// would kill the account at its next refresh: refused, nothing written. A login
    /// NEWER than tcr's (a fresh `codex login`) still replaces it.
    #[tokio::test]
    async fn a_re_import_of_an_older_credential_is_refused_and_writes_nothing() {
        let port = 0;
        let (_dir, path) = import_config(
            port,
            &unconstrained_upstream().await,
            &format!(
                r#"[{{ "name": "alice@example.com", "provider": "codex",
                      "accountUuid": "{CODEX_USER_A}", "orgUuid": "{CODEX_ACCOUNT_A}", "accessToken": "at-live",
                      "refreshToken": "rt-old", "expiresAt": 1900000000000 }}]"#
            ),
        );
        let before = std::fs::read(&path).unwrap();

        let err = import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-stale", "rt-stale"),
            None,
        )
        .await
        .expect_err("an older credential must not overwrite tcr's newer one");
        assert!(
            err.to_string()
                .contains("tcr already holds a newer credential for this account"),
            "{err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");

        import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(2_000_000_000_000, "at-fresh", "rt-fresh"),
            None,
        )
        .await
        .expect("a newer login still replaces tcr's");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["accounts"][0]["refreshToken"], "rt-fresh");
    }

    // --- import_codex_login: `--account` ----------------------------------------

    /// `--account` names the login the user means to import, so a `CODEX_HOME` that holds
    /// another is refused before anything else is asked, naming the account, and nothing is
    /// written. The add route and `codexUpstream` are stubs that record what they are asked, and
    /// neither is asked anything.
    ///
    /// Watched red: without the guard, alice's login is imported over the live route.
    #[tokio::test]
    async fn an_import_for_another_account_is_refused_and_writes_nothing() {
        let (port, probes) = stub_add_route(true, Some(crate::provider::Provider::Codex)).await;
        let (upstream, checks) =
            accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT")).await;
        let (_dir, path) = import_config(port, &upstream, "[]");
        let before = std::fs::read(&path).unwrap();

        let err = import_codex_login(
            &path,
            false,
            None,
            Some("bob@example.com"),
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .expect_err("alice's login must not be imported as bob");
        let text = err.to_string();
        assert!(
            text.contains("is not 'bob@example.com'") && text.contains("nothing was written"),
            "{text}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
        assert!(
            probes.lock().unwrap().is_empty(),
            "not even the route probe: {:?}",
            probes.lock().unwrap()
        );
        assert!(
            checks.lock().unwrap().is_empty(),
            "no workspace check: {:?}",
            checks.lock().unwrap()
        );
    }

    /// The control: `--account` naming the login's own email imports it, as any import does.
    ///
    /// Watched red: with the guard refusing the account it should accept, nothing is written.
    #[tokio::test]
    async fn an_import_for_its_own_account_is_written() {
        let (_dir, path) = import_config(0, &unconstrained_upstream().await, "[]");

        let saved = import_codex_login(
            &path,
            false,
            None,
            Some("alice@example.com"),
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .expect("alice's own login is imported");
        assert_eq!(saved, "alice@example.com");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["accounts"][0]["name"], "alice@example.com", "{doc}");
        assert_eq!(doc["accounts"][0]["provider"], "codex", "{doc}");
    }

    /// `--account` naming a Codex row refuses a login that does not name both its ChatGPT
    /// user and its workspace: such a login cannot be shown to be that row's account. The
    /// identity check fell back to comparing names, and its probe row carried the target row's
    /// own name, so the check passed for any such login and the import wrote a row for whoever
    /// it was. It is refused like any other login that is not the row's, before the route probe
    /// and the workspace check, with nothing written. `--codex` refuses the same login.
    ///
    /// Watched red: without the check, carol's login with no user id is imported beside bob's
    /// row, as `carol@example.com`.
    #[tokio::test]
    async fn an_account_import_refuses_a_login_that_names_no_user_or_workspace() {
        let (port, probes) = stub_add_route(true, Some(crate::provider::Provider::Codex)).await;
        let (upstream, checks) =
            accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT")).await;
        let (_dir, path) = import_config(
            port,
            &upstream,
            &format!(
                r#"[{{ "name": "bob@example.com/codex", "provider": "codex",
                      "accountUuid": "{CODEX_USER_A}", "orgUuid": "{CODEX_ACCOUNT_A}",
                      "accessToken": "at-bob", "refreshToken": "rt-bob",
                      "expiresAt": 1700000000000 }}]"#
            ),
        );
        let before = std::fs::read(&path).unwrap();
        let carol = || crate::codex_login::CodexLogin {
            email: Some("carol@example.com".to_string()),
            ..alice_codex_login(1_800_000_000_000, "at-carol", "rt-carol")
        };
        for (what, login) in [
            (
                "no user",
                crate::codex_login::CodexLogin {
                    user_id: None,
                    ..carol()
                },
            ),
            (
                "no workspace",
                crate::codex_login::CodexLogin {
                    account_id: None,
                    ..carol()
                },
            ),
        ] {
            let err = import_codex_login(
                &path,
                false,
                None,
                Some("bob@example.com/codex"),
                login,
                None,
            )
            .await
            .expect_err(what);
            assert_eq!(
                err.to_string(),
                "the Codex login on this machine is not 'bob@example.com/codex'; nothing was \
                 written.",
                "{what}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "{what}: nothing was written"
            );
        }
        assert!(
            probes.lock().unwrap().is_empty(),
            "not even the route probe: {:?}",
            probes.lock().unwrap()
        );
        assert!(
            checks.lock().unwrap().is_empty(),
            "no workspace check: {:?}",
            checks.lock().unwrap()
        );
    }

    // --- import_codex_login: logins tcr must not pool -----------------------------

    /// A FedRAMP login is refused first: offline, before the route probe and the workspace
    /// check, with nothing written, and `--force` does not get past it. The add route and
    /// `codexUpstream` are both stubs that record what they are asked, and neither is asked
    /// anything.
    ///
    /// Watched red twice: without the refusal the import succeeds, and with it moved after the
    /// route probe the route stub records the blank probe.
    #[tokio::test]
    async fn a_fedramp_login_is_refused_before_anything_else() {
        let (port, probes) = stub_add_route(true, Some(crate::provider::Provider::Codex)).await;
        let (upstream, checks) =
            accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT")).await;
        let (_dir, path) = import_config(port, &upstream, "[]");
        let before = std::fs::read(&path).unwrap();
        let mut login = alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex");
        login.is_fedramp = true;

        let err = import_codex_login(&path, true, None, None, login, None)
            .await
            .expect_err("a FedRAMP login must not be imported");
        let text = err.to_string();
        assert!(
            text.contains("FedRAMP")
                && text.contains("chatgpt_account_is_fedramp")
                && text.contains("Nothing was written"),
            "{text}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
        assert!(
            probes.lock().unwrap().is_empty(),
            "not even the route probe: {:?}",
            probes.lock().unwrap()
        );
        assert!(
            checks.lock().unwrap().is_empty(),
            "no workspace check: {:?}",
            checks.lock().unwrap()
        );
    }

    /// A login whose workspace is served anywhere but chatgpt.com, by another backend origin or
    /// a residency override, is refused naming what the workspace check said, after one check
    /// carrying the imported bearer and account id, and nothing is written.
    ///
    /// Watched red: without the check, the import writes the row.
    #[tokio::test]
    async fn a_routed_workspace_is_refused_naming_its_routing_and_writes_nothing() {
        for (origin, routing, named) in [
            (
                "https://gov.chatgpt.com",
                "NO_CONSTRAINT",
                "https://gov.chatgpt.com",
            ),
            ("NO_CONSTRAINT", "us", "\"us\""),
        ] {
            let (upstream, checks) =
                accounts_check_stub(200, &workspace_answer(origin, routing)).await;
            let (_dir, path) = import_config(0, &upstream, "[]");
            let before = std::fs::read(&path).unwrap();

            let err = import_codex_login(
                &path,
                false,
                None,
                None,
                alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
                None,
            )
            .await
            .expect_err("a routed workspace must not be imported");
            let text = err.to_string();
            assert!(
                text.contains(named)
                    && text.contains("not served at https://chatgpt.com")
                    && text.contains("Nothing was written"),
                "{routing}: {text}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
            assert_eq!(
                *checks.lock().unwrap(),
                [workspace_check_of("at-codex")],
                "one check, with the imported bearer"
            );
        }
    }

    /// The same refusal over the live route, where an add would put the login in the running
    /// proxy's pool at once: the workspace check runs there too, before the add. The route stub
    /// sees only the blank probe, the check stub sees one check with the imported bearer, and
    /// nothing is written.
    ///
    /// Watched red: with the check run only on the file route, the login is added live.
    #[tokio::test]
    async fn a_routed_workspace_is_refused_over_the_live_route_and_never_added() {
        let (port, probes) = stub_add_route(true, Some(crate::provider::Provider::Codex)).await;
        let (upstream, checks) = accounts_check_stub(
            200,
            &workspace_answer("https://gov.chatgpt.com", "NO_CONSTRAINT"),
        )
        .await;
        let (_dir, path) = import_config(port, &upstream, "[]");
        let before = std::fs::read(&path).unwrap();

        let err = import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .expect_err("a routed workspace must not be imported over the live route");
        let text = err.to_string();
        assert!(
            text.contains("https://gov.chatgpt.com")
                && text.contains("not served at https://chatgpt.com")
                && text.contains("Nothing was written"),
            "{text}"
        );
        let bodies = probes.lock().unwrap().clone();
        assert!(
            bodies.len() == 1 && bodies[0]["name"] == "" && bodies[0]["accessToken"] == "",
            "the blank probe and no add: {bodies:?}"
        );
        assert_eq!(
            *checks.lock().unwrap(),
            [workspace_check_of("at-codex")],
            "one check, with the imported bearer"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    }

    /// A workspace check that cannot complete refuses the import, naming why, with nothing
    /// written: an error status, an answer that is not JSON or names no routing, and an
    /// upstream nobody listens on.
    ///
    /// Watched red: without the check, every one of these imports writes the row.
    #[tokio::test]
    async fn a_workspace_check_that_cannot_complete_refuses_the_import() {
        let dead = "http://127.0.0.1:0".to_string();
        let mut upstreams = vec![(dead, "could not connect".to_string())];
        for (status, body, why) in [
            (500, "{}", "answered HTTP 500"),
            (401, "{}", "answered HTTP 401"),
            (200, "not json", "not JSON"),
            (
                200,
                r#"{"accounts":{}}"#,
                "names no workspace_backend_origin",
            ),
        ] {
            let (upstream, _checks) = accounts_check_stub(status, body).await;
            upstreams.push((upstream, why.to_string()));
        }
        for (upstream, why) in upstreams {
            let (_dir, path) = import_config(0, &upstream, "[]");
            let before = std::fs::read(&path).unwrap();

            let err = import_codex_login(
                &path,
                false,
                None,
                None,
                alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
                None,
            )
            .await
            .expect_err("an unconfirmed workspace must not be imported");
            let text = err.to_string();
            assert!(
                text.contains("could not confirm")
                    && text.contains(&why)
                    && text.contains("Nothing was written"),
                "{why}: {text}"
            );
            assert!(
                !text.contains("at-codex"),
                "the bearer never appears: {text}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
        }
    }

    /// A stub `codexUpstream` on 127.0.0.1:0 that answers every request `302 Found` with
    /// `Location: location`, and records it as [`accounts_check_stub`] does.
    async fn redirecting_stub(
        location: &str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<SeenCheck>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&seen);
        let location = location.to_string();
        let origin = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let recorded = std::sync::Arc::clone(&recorded);
            let location = location.clone();
            async move {
                let header = |name: &str| {
                    req.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                recorded.lock().unwrap().push(SeenCheck {
                    path: req.uri().path().to_string(),
                    bearer: header("authorization"),
                    account_id: header(crate::provider::CODEX_ACCOUNT_HEADER),
                });
                (
                    axum::http::StatusCode::FOUND,
                    [(axum::http::header::LOCATION, location)],
                )
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, origin).await;
        });
        (format!("http://{addr}"), seen)
    }

    /// A workspace check answered with a redirect refuses the import, and the redirect is never
    /// followed: the check carries the imported bearer and account id, so it goes to
    /// `codexUpstream` and nowhere else. Here `codexUpstream` answers 302 to a second loopback
    /// origin whose answer would clear the login; that origin is asked nothing, and nothing is
    /// written.
    ///
    /// Watched red: on a client that follows redirects, the second origin is asked, with the
    /// account id though without the bearer, and its answer imports the login.
    #[tokio::test]
    async fn a_redirected_workspace_check_refuses_the_import_and_is_never_followed() {
        let (elsewhere, redirected) =
            accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT")).await;
        let (upstream, checks) = redirecting_stub(&format!("{elsewhere}/elsewhere")).await;
        let (_dir, path) = import_config(0, &upstream, "[]");
        let before = std::fs::read(&path).unwrap();

        let err = import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .expect_err("a redirected workspace check must not clear the login");
        let text = err.to_string();
        assert!(
            text.contains("answered HTTP 302") && text.contains("Nothing was written"),
            "{text}"
        );
        assert!(
            redirected.lock().unwrap().is_empty(),
            "the redirect's target is asked nothing: {:?}",
            redirected.lock().unwrap()
        );
        assert_eq!(
            *checks.lock().unwrap(),
            [workspace_check_of("at-codex")],
            "one check, to codexUpstream"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    }

    /// A workspace served at chatgpt.com is imported, after exactly one check that carried the
    /// imported bearer and account id to `codexUpstream`.
    ///
    /// Watched red: without the check, the stub is asked nothing.
    #[tokio::test]
    async fn an_unconstrained_workspace_is_imported_after_one_check_with_the_imported_bearer() {
        let (upstream, checks) = accounts_check_stub(
            200,
            &workspace_answer("https://chatgpt.com", "NO_CONSTRAINT"),
        )
        .await;
        let (_dir, path) = import_config(0, &upstream, "[]");

        let saved = import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .expect("a workspace served at chatgpt.com is imported");
        assert_eq!(saved, "alice@example.com");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["accounts"][0]["provider"], "codex", "{doc}");
        assert_eq!(*checks.lock().unwrap(), [workspace_check_of("at-codex")]);
    }

    /// The first-run race the macOS e2e caught: this process saw an empty
    /// fleet when its verb started, and by the time the import reads the
    /// config, another `tcr` process (the server booting) has written its
    /// import there. The automatic import must stop on that read, not mint
    /// `unnamed-2` beside the other process's `unnamed`. Returns before the
    /// profile fetch, so no network.
    #[tokio::test]
    async fn first_run_import_stops_when_another_process_imported_first() {
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let path = std::env::temp_dir().join(format!(
            "tcr-oauth-import-race-{}-{dead_port}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(
                r#"{{ "proxy": {{ "port": {dead_port} }}, "accounts": [
                     {{ "name": "unnamed", "accessToken": "at-fake-first", "refreshToken": "rt-fake-first" }}
                   ] }}"#
            ),
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let login = crate::claude_code_login::ClaudeCodeLogin {
            access_token: "at-fake-second".to_string(),
            refresh_token: Some("rt-fake-second".to_string()),
            expires_at: Some(4_102_444_800_000),
            subscription_type: Some("max".to_string()),
        };

        let outcome = import_claude_code_login(
            &path,
            ImportInto::EmptyFleetOnly,
            false,
            None,
            None,
            login,
            LoginUi::Machine,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome, None,
            "the automatic import must skip a fleet that is no longer empty"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the config must be left byte-for-byte alone"
        );
        std::fs::remove_file(&path).ok();
    }

    // --- refusals a GUI titles by their code -----------------------------------------

    /// The three import refusals carry their codes, read back through the error a caller holds,
    /// and read exactly as they always did: the routed, unconfirmed and FedRAMP tests above
    /// still pass on `err.to_string()`.
    ///
    /// Watched red: with the refusals back to plain `bail!`s, no `LoginRefusal` is in the chain.
    #[tokio::test]
    async fn the_codex_import_refusals_carry_their_codes() {
        let code_of = |err: &anyhow::Error| match err.downcast_ref::<LoginRefusal>() {
            Some(refusal) => refusal.code,
            None => panic!("no coded refusal in: {err:#}"),
        };

        let (_dir, path) = import_config(0, &unconstrained_upstream().await, "[]");
        let mut login = alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex");
        login.is_fedramp = true;
        let err = import_codex_login(&path, false, None, None, login, None)
            .await
            .unwrap_err();
        assert_eq!(code_of(&err), "codex-fedramp");
        assert!(err.to_string().contains("FedRAMP"), "{err}");

        let (upstream, _) = accounts_check_stub(
            200,
            &workspace_answer("https://gov.chatgpt.com", "NO_CONSTRAINT"),
        )
        .await;
        let (_dir, path) = import_config(0, &upstream, "[]");
        let err = import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(code_of(&err), "codex-routed");
        assert!(
            err.to_string()
                .contains("not served at https://chatgpt.com"),
            "{err}"
        );

        let (upstream, _) = accounts_check_stub(500, "{}").await;
        let (_dir, path) = import_config(0, &upstream, "[]");
        let err = import_codex_login(
            &path,
            false,
            None,
            None,
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(code_of(&err), "codex-unconfirmed");
        assert!(err.to_string().contains("could not confirm"), "{err}");
    }

    /// `--from-codex --account` takes the row name `tcr accounts` prints, as every other verb's
    /// `--account` does, and still takes the login's email. A name that is a Claude row is
    /// refused before the route probe and any write: no Codex import re-signs it. A name that is
    /// no row keeps the email check and its words.
    ///
    /// Watched red: comparing the email alone refuses the row name as "not
    /// 'alice@example.com/codex'".
    #[tokio::test]
    async fn from_codex_account_accepts_the_row_name() {
        let (_dir, path) = import_config(
            0,
            &unconstrained_upstream().await,
            &format!(
                r#"[{{ "name": "alice@example.com/codex", "provider": "codex",
                      "accountUuid": "{CODEX_USER_A}", "orgUuid": "{CODEX_ACCOUNT_A}",
                      "accessToken": "at-old", "refreshToken": "rt-old",
                      "expiresAt": 1700000000000 }}]"#
            ),
        );
        let saved = import_codex_login(
            &path,
            false,
            None,
            Some("alice@example.com/codex"),
            alice_codex_login(1_800_000_000_000, "at-new", "rt-new"),
            None,
        )
        .await
        .expect("the row name is accepted");
        assert_eq!(saved, "alice@example.com/codex");
        let rows = |path: &Path| -> Vec<serde_json::Value> {
            let doc: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            doc["accounts"].as_array().unwrap().clone()
        };
        let written = rows(&path);
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(written[0]["accessToken"], "at-new");

        let saved = import_codex_login(
            &path,
            false,
            None,
            Some("alice@example.com"),
            alice_codex_login(1_900_000_000_000, "at-newer", "rt-newer"),
            None,
        )
        .await
        .expect("the login's email is still accepted");
        assert_eq!(saved, "alice@example.com/codex");
        assert_eq!(rows(&path)[0]["accessToken"], "at-newer");

        let before = std::fs::read(&path).unwrap();
        let err = import_codex_login(
            &path,
            false,
            None,
            Some("erin@example.com"),
            alice_codex_login(2_000_000_000_000, "at-x", "rt-x"),
            None,
        )
        .await
        .expect_err("a name that is no row and not the login's email");
        assert_eq!(
            err.to_string(),
            "the Codex login on this machine is not 'erin@example.com'; nothing was written."
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");

        let (port, probes) = stub_add_route(true, Some(crate::provider::Provider::Codex)).await;
        let (upstream, checks) =
            accounts_check_stub(200, &workspace_answer("NO_CONSTRAINT", "NO_CONSTRAINT")).await;
        let (_dir, claude) = import_config(
            port,
            &upstream,
            r#"[{ "name": "dana@example.com", "accessToken": "at-dana" }]"#,
        );
        let before = std::fs::read(&claude).unwrap();
        let err = import_codex_login(
            &claude,
            false,
            None,
            Some("dana@example.com"),
            alice_codex_login(1_800_000_000_000, "at-codex", "rt-codex"),
            None,
        )
        .await
        .expect_err("a Claude row is never re-signed by a Codex import");
        let refusal = err.downcast_ref::<LoginRefusal>().expect("a coded refusal");
        assert_eq!(refusal.code, "account-is-claude");
        assert_eq!(
            refusal.reason,
            "'dana@example.com' is a Claude account; `tcr login --from-codex` imports Codex \
             accounts only. Sign it in again with `tcr login --account dana@example.com`. \
             Nothing was written."
        );
        assert_eq!(
            std::fs::read(&claude).unwrap(),
            before,
            "nothing was written"
        );
        assert!(
            probes.lock().unwrap().is_empty(),
            "not even the route probe"
        );
        assert!(checks.lock().unwrap().is_empty(), "no workspace check");
    }

    // --- remedies that name the right command ------------------------------------------

    /// An offline snapshot never refreshes; for a Codex row its remedy names the command that
    /// signs a Codex account in again, and the Anthropic remedy is the one it always was.
    ///
    /// Watched red: one message for both providers names `tcr login` for a Codex row.
    #[tokio::test]
    async fn the_offline_expired_token_remedy_names_tcr_login_codex() {
        let err = NoRefresh
            .refresh(
                crate::provider::Provider::Codex,
                "rt-never-sent".to_string(),
            )
            .await
            .expect_err("NoRefresh never refreshes");
        assert!(matches!(err, OAuthError::Transient(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            format!("token refresh failed transiently: {NO_REFRESH_CODEX_MESSAGE}")
        );
        assert!(
            NO_REFRESH_CODEX_MESSAGE.contains("re-run after `tcr login --codex --account <name>`"),
            "{NO_REFRESH_CODEX_MESSAGE}"
        );
        let claude = NoRefresh
            .refresh(
                crate::provider::Provider::Anthropic,
                "rt-never-sent".to_string(),
            )
            .await
            .expect_err("NoRefresh never refreshes");
        assert_eq!(
            claude.to_string(),
            format!("token refresh failed transiently: {NO_REFRESH_MESSAGE}")
        );
    }

    /// A refusal over a running proxy names the login that was run as the one to run again,
    /// for both kinds of incumbent; plain `tcr login` still says `run 'tcr login'`.
    ///
    /// Watched red: with `'tcr login'` written into the message, the import's refusal sends
    /// its user to the browser flow.
    #[test]
    fn the_running_proxy_refusal_echoes_the_command_that_was_run() {
        for kind in [singleton::ProxyKind::Tcr, singleton::ProxyKind::TcrEmbedded] {
            let msg = expect_refuse(login_guard_refusal_for(
                incumbent(kind),
                false,
                3456,
                false,
                "tcr login --from-codex",
            ));
            assert!(
                msg.contains("run 'tcr login --from-codex'"),
                "{kind:?}: {msg}"
            );
            let plain = expect_refuse(login_guard_refusal(incumbent(kind), false, 3456, false));
            assert!(plain.contains("run 'tcr login',"), "{kind:?}: {plain}");
        }
        let live = codex_live_route_refusal(3456, Some(4242), LOGIN_CODEX_COMMAND);
        assert!(
            live.contains("then re-run `tcr login --codex`. --force does not get past this"),
            "{live}"
        );
    }

    /// With no login file, `--from-codex` says how to make one, and, last, that a login codex
    /// keeps in the system keyring has no file to import, which `tcr login --codex` does not need.
    ///
    /// Watched red: the message without its last sentence.
    #[test]
    fn a_keyring_held_codex_login_names_the_keyring() {
        assert_eq!(
            no_codex_login_message(Path::new("/Users/example/.codex/auth.json")),
            "no Codex login found at /Users/example/.codex/auth.json — nothing was written. Log \
             in with `codex login` first (for a second account: `mkdir -p <dir>`, \
             `CODEX_HOME=<dir> codex login`, then `tcr login --from-codex --codex-home <dir>`). If \
             codex keeps this login in the system keyring, which tcr does not read, use tcr login \
             --codex instead, which needs no file."
        );
    }

    /// The same ChatGPT user in another workspace names both workspaces by their first 8 hex,
    /// each with its plan in parentheses, and drops a parenthesis whose plan is unknown.
    #[test]
    fn codex_relogin_workspace_refusal_drops_unknown_plans() {
        let named = codex_relogin_workspace_refusal(
            Some("erin@example.com"),
            "99999999-9999-4999-8999-999999999999",
            Some("business"),
            "work",
            "88888888-8888-4888-8888-888888888888",
            Some("pro"),
        );
        assert_eq!(named.code, "codex-identity");
        assert_eq!(
            named.reason,
            "the ChatGPT login that came back is erin@example.com in ChatGPT workspace 99999999 \
             (ChatGPT Business), but 'work' holds workspace 88888888 (ChatGPT Pro). Retry `tcr \
             login --codex --account work` and choose that workspace when ChatGPT asks which one \
             to use. Nothing was written."
        );
        let unknown = codex_relogin_workspace_refusal(
            None,
            "99999999-9999-4999-8999-999999999999",
            None,
            "work",
            "88888888-8888-4888-8888-888888888888",
            None,
        );
        assert_eq!(
            unknown.reason,
            "the ChatGPT login that came back is the same ChatGPT user in ChatGPT workspace \
             99999999, but 'work' holds workspace 88888888. Retry `tcr login --codex --account \
             work` and choose that workspace when ChatGPT asks which one to use. Nothing was \
             written."
        );
    }

    /// A running proxy's status read, as `cli::fetch_live_status` hands it over: parsed from the
    /// wire, listing no Codex row (the server may not have admitted the new one yet), with the
    /// listener `mitm` reports.
    fn live_proxy_reporting(
        mitm: serde_json::Value,
    ) -> Result<crate::status::StatusPayload, crate::cli::LiveStatusError> {
        Ok(serde_json::from_value(serde_json::json!({
            "kind": crate::status::STATUS_KIND,
            "accounts": [],
            "build": {"sha": "abc1234"},
            "mitm": mitm,
        }))
        .expect("a status payload"))
    }

    const NOTICE_PORT: u16 = 3999;

    fn notice_ca() -> &'static Path {
        Path::new("/tmp/tcr-run-test/tcr-ca.pem")
    }

    /// A proxy serving the JS teamclaude leaf cannot terminate marked Codex connections, so the
    /// first account is told codex is not pooled, and why. The same holds before any proxy runs
    /// when the JS leaf is on disk, because the next boot serves it.
    ///
    /// Watched red: with the first-account notice keyed on the config's count alone, the
    /// JS-leaf proxy gets a notice claiming marked Codex connections are pooled.
    #[test]
    fn a_js_leaf_proxy_gets_the_not_pooled_notice() {
        let js = live_proxy_reporting(
            serde_json::json!({"interceptedHosts": ["api.anthropic.com"], "leaf": "js"}),
        );
        assert_eq!(
            first_codex_account_notice(&js, NOTICE_PORT, notice_ca(), true, false),
            format!(
                "codex is not pooled: the proxy on :3999 is serving the JS {} leaf, which \
             covers api.anthropic.com only and is never re-minted — a restart will not change \
             this; see docs/troubleshooting.md",
                crate::legacy_names::JS_PROXY_BIN
            )
        );
        assert_eq!(
            first_codex_account_notice(
                &Err(crate::cli::LiveStatusError::NoServer),
                NOTICE_PORT,
                notice_ca(),
                true,
                true,
            ),
            "codex is not pooled: no proxy answered on 127.0.0.1:3999",
            "nothing listens, and the JS leaf on disk is what the next boot serves"
        );
        assert_eq!(
            first_codex_account_notice(
                &Err(crate::cli::LiveStatusError::NoAnswer(
                    "the server did not answer within 5s".to_string()
                )),
                NOTICE_PORT,
                notice_ca(),
                true,
                false,
            ),
            "codex is not pooled: could not read the running proxy's status (the server did not \
             answer within 5s)",
            "a proxy that could not be read is not claimed to intercept anything"
        );
    }

    /// A proxy whose minted leaf covers chatgpt.com can take marked Codex traffic
    /// from now on, though its status, read right after the save, may not list the new row yet:
    /// the notice keys on the leaf, not on the route. So does a released port with no JS leaf on
    /// disk, where the next boot mints such a leaf.
    #[test]
    fn a_minted_chatgpt_leaf_gets_the_first_account_notice() {
        let minted = live_proxy_reporting(serde_json::json!({
            "interceptedHosts": ["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
            "leaf": "minted",
        }));
        assert_eq!(
            first_codex_account_notice(&minted, NOTICE_PORT, notice_ca(), true, false),
            "tcr pools marked Codex connections to chatgpt.com: use `tcr run --codex`; keep \
             the marker scoped to that launch, since unmarked connections keep their own login"
        );
        assert_eq!(
            first_codex_account_notice(
                &Err(crate::cli::LiveStatusError::NoServer),
                NOTICE_PORT,
                notice_ca(),
                false,
                false,
            ),
            CODEX_FIRST_ACCOUNT_NOTICE
        );
    }
}
