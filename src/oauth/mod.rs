//! OAuth token refresh.
//!
//! The one behaviour that matters downstream is the error split: a
//! `400/401/403` means the refresh token itself is dead (the account must drop
//! out of rotation until re-login), whereas a `5xx`/network failure is transient
//! (keep the current token and fail this one request over to another account).
//! A Codex account refreshes against `auth.openai.com` instead
//! ([`codex`], dispatched by [`LiveRefresher`]). Every Codex `401` and the
//! `refresh_token_expired`, `refresh_token_reused` and `refresh_token_invalidated` codes at
//! any error status are permanent, as is `invalid_grant` on a `400`. Other failures are
//! transient.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Anthropic OAuth token endpoint.
pub const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code OAuth client id.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Threshold used by [`is_expiring_soon`]: refresh proactively 5 minutes out.
pub const EXPIRING_SOON_MS: i64 = 5 * 60 * 1000;

const MAX_RETRIES: u32 = 2;
const BASE_DELAY_MS: u64 = 500;
/// The bound on one refresh POST, whichever provider's endpoint it goes to: the manager holds an
/// account's refresh lock across the call, so a POST that never answered would stall every
/// request on that account. [`codex`], a child module, applies the same bound.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Freshly-minted OAuth credentials returned by a refresh.
#[derive(Debug, Clone)]
pub struct Tokens {
    /// A refreshed Codex id token, when returned: carried to the imported auth.json so Codex
    /// reads current plan/workspace claims. Other OAuth producers leave it absent.
    pub id_token: Option<String>,
    pub access_token: String,
    /// `None` only for a `claude setup-token` credential ([`crate::login::login_with_token`]):
    /// that mint carries only the `user:inference` scope, and the CLI that
    /// produces it never surfaces a refresh token to the caller at all — so
    /// there is nothing to store. Every OTHER producer of a `Tokens` MUST
    /// yield `Some(...)`: [`crate::login::tokens_from_exchange_body`] `bail!`s rather than
    /// return `None`, and [`refresh_access_token_at`] falls back to the
    /// refresh token it was just called with when the endpoint omits a
    /// rotated one. See that fallback's doc comment before touching either.
    /// The Codex refresher, [`codex::refresh_access_token_at`], is
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
pub(crate) struct RefreshResponse {
    pub(crate) access_token: String,
    #[serde(default)]
    pub(crate) refresh_token: Option<String>,
    #[serde(default)]
    pub(crate) expires_at: Option<i64>,
    #[serde(default)]
    pub(crate) expires_in: Option<i64>,
}

/// Normalize an `expires_at` to milliseconds. OAuth backends may report seconds;
/// Claude Code credentials use milliseconds. Values below ~`1e12` are seconds
/// (that boundary is year 2001 in ms, far in the future in seconds).
pub const fn normalize_expires_at(value: i64) -> i64 {
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
pub(crate) fn expires_at_from(now_ms: i64, expires_in: Option<i64>) -> i64 {
    now_ms.saturating_add(expires_in.unwrap_or(3600).saturating_mul(1000))
}

/// Has the token already expired at `now_ms`? A `None` expiry is treated as
/// non-expiring (nothing to refresh).
pub const fn is_expired(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match expires_at_ms {
        Some(exp) => now_ms >= exp,
        None => false,
    }
}

/// Is the token within [`EXPIRING_SOON_MS`] of expiry at `now_ms`?
pub const fn is_expiring_soon(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match expires_at_ms {
        Some(exp) => now_ms + EXPIRING_SOON_MS >= exp,
        None => false,
    }
}

/// `Accept` header the token endpoint expects from a "recognised" client.
/// Shared between [`refresh_access_token_at`], [`crate::login::exchange_code`] and
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
/// account's provider — Anthropic's, or Codex's ([`codex`]).
pub struct LiveRefresher {
    client: reqwest::Client,
    endpoint: String,
    /// Where Codex refreshes go: [`codex::TOKEN_ENDPOINT`] unless a
    /// test pointed it elsewhere with [`Self::with_codex_endpoint`].
    codex_endpoint: String,
}

impl LiveRefresher {
    pub fn new() -> Self {
        // Building fails only when the TLS backend cannot initialise. A refresher without
        // its client cannot exist, and a default client would honour ambient proxies.
        #[expect(
            clippy::expect_used,
            reason = "reqwest's builder has no infallible form, and the constructor returns no Result"
        )]
        let client = crate::http_client::builder()
            .no_proxy()
            .build()
            .expect("build reqwest client");
        Self {
            // A dedicated client: the upstream proxy client must NOT carry a
            // total timeout (streams run long), but a refresh must not hang.
            //
            // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
            // proxy — routing our own upstream through an ambient proxy (e.g. one
            // on :3456) loops us back through a proxy and fails
            // as "upstream unreachable". Always reach the token endpoint directly,
            // Anthropic's and Codex's alike.
            client,
            endpoint: TOKEN_ENDPOINT.to_string(),
            codex_endpoint: codex::TOKEN_ENDPOINT.to_string(),
        }
    }

    /// Send Codex refreshes to `endpoint` instead of
    /// [`codex::TOKEN_ENDPOINT`]. A test seam: a stub endpoint is
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
                    codex::refresh_access_token_at(&client, &refresh_token, &endpoint).await
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

pub mod codex;
mod profile;
#[cfg(test)]
mod tests;

pub use profile::*;
