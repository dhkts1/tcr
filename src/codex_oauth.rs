//! Codex's half of OAuth: refreshing an access token and reading the JWT claims tcr needs.
//!
//! Codex logs in against `auth.openai.com` with the public client id below and refreshes with a
//! JSON `refresh_token` grant (`openai/codex` `login/src/auth/manager.rs:212,1627-1660,1718`;
//! the body encoding is `TokenEncoding::Json`, `login/src/oauth/client.rs:76-110`). The response
//! carries `id_token`, `access_token` and `refresh_token`, each optional. Refresh tokens ROTATE
//! and are single-use: a reused one comes back as `refresh_token_reused`, which Codex itself
//! classifies as exhausted (`manager.rs:1683-1693`). Like Codex at f82e365, tcr treats
//! `refresh_token_expired`, `refresh_token_reused` and `refresh_token_invalidated` as permanent
//! at any error status, and `invalid_grant` as permanent on a 400 (`manager.rs:1659-1672`).
//! Codes are read from `error.code`, a string `error`, or top-level `code`, as CodexBar's
//! `CodexTokenRefresher` does. Every 401 remains permanent even if its body cannot be read;
//! reading its code is bounded to one second so a stalled body cannot delay rejection until
//! the refresh timeout.
//!
//! The access token is a JWT whose `exp` claim is the expiry (`login/src/token_data.rs:104,145`);
//! identity claims live under the `https://api.openai.com/auth` key (`token_data.rs`,
//! `IdClaims`). No signature is verified here: tcr is the holder of the token, not its audience,
//! and the claims only name which account a credential belongs to and when to refresh it. The
//! base64url decoder is hand-rolled so this adds no crate.

use serde::{Deserialize, Serialize};

use crate::oauth::{OAuthError, Tokens};

pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Used when a fresh access token carries no readable `exp`, or one that is not strictly in the
/// future: one hour, a deliberately short stand-in, so the proactive refresh still fires rather
/// than never. It is not Codex's lifetime: measured access tokens had about ten days left, and
/// codex itself, when it cannot read `exp`, refreshes 8 days after `last_refresh` (`openai/codex`
/// f82e365, `login/src/auth/manager.rs:203-204,3001-3023`). An `exp` at or before local now is
/// treated as unreadable: taken at face value, `exp: 0` would make every expiry check true, so
/// every request would refresh and spend a single-use token. A future `exp` is used as-is, since
/// it is the token's expiry (see the module doc); the proactive-refresh margin absorbs small
/// clock skew.
pub const DEFAULT_LIFETIME_MS: i64 = 60 * 60 * 1000;

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'static str,
    client_id: &'static str,
    refresh_token: &'a str,
}

/// The two fields a refresh reads. The response's `id_token` is not modelled: a refresh changes
/// nothing tcr stores about whose account this is, so serde's default of ignoring unknown keys
/// drops it.
#[derive(Deserialize, Default)]
struct RefreshResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// The error codes `auth.openai.com` uses for a refresh token that will never work again.
const PERMANENT_ERROR_CODES: &[&str] = &[
    "refresh_token_expired",
    "refresh_token_reused",
    "refresh_token_invalidated",
];

/// Read the error code from a nested `error.code`, a string `error`, or top-level `code`.
fn error_code(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("error")
        .and_then(|e| e.get("code"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| v.get("error").and_then(serde_json::Value::as_str))
        .or_else(|| v.get("code").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

/// The error codes RFC 6749 §5.2 gives a token endpoint for a refused grant.
const OAUTH_TOKEN_ERROR_CODES: &[&str] = &[
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
];

/// The error code a Transient message may name, which reaches the log at WARN
/// (`manager/refresh.rs` logs every one): one of [`OAUTH_TOKEN_ERROR_CODES`] or
/// [`PERMANENT_ERROR_CODES`], and only as spelled in that list. Anything else is never echoed,
/// because the field is whatever the body put there: a 400 that echoed the refresh token back in
/// `error`, or any other text, would otherwise reach the log verbatim. A check of the value's
/// shape is not enough, since a token can have exactly the shape of a code.
fn known_error_code(code: &str) -> Option<&'static str> {
    OAUTH_TOKEN_ERROR_CODES
        .iter()
        .chain(PERMANENT_ERROR_CODES)
        .find(|known| known.eq_ignore_ascii_case(code))
        .copied()
}

/// Refresh a Codex access token at `endpoint` ([`TOKEN_ENDPOINT`] in production), taking its
/// arguments in the order of its Anthropic twin, [`crate::oauth::refresh_access_token_at`].
///
/// Bounded by `crate::oauth::REFRESH_TIMEOUT`, as that twin is: the manager holds the account's
/// refresh lock across this call, so a POST that never answered would stall every request on the
/// account. A 401, any error naming one of `PERMANENT_ERROR_CODES`, or a 400 naming
/// `invalid_grant` is [`OAuthError::AuthRejected`]; every other failure is
/// [`OAuthError::Transient`].
pub async fn refresh_access_token_at(
    client: &reqwest::Client,
    refresh_token: &str,
    endpoint: &str,
) -> Result<Tokens, OAuthError> {
    let response = client
        .post(endpoint)
        .timeout(crate::oauth::REFRESH_TIMEOUT)
        .json(&RefreshRequest {
            grant_type: "refresh_token",
            client_id: CLIENT_ID,
            refresh_token,
        })
        .send()
        .await
        .map_err(|e| OAuthError::Transient(format!("codex refresh transport: {e}")))?;
    let status = response.status();
    // Preserve a 401's permanent status even if its body stalls or breaks. When the body is
    // available, its code distinguishes a reused token so the manager can recover newer tokens.
    let body = if status.as_u16() == 401 {
        match tokio::time::timeout(std::time::Duration::from_secs(1), response.bytes()).await {
            Ok(Ok(body)) => body,
            _ => {
                return Err(OAuthError::AuthRejected {
                    status: 401,
                    code: None,
                });
            }
        }
    } else {
        response
            .bytes()
            .await
            .map_err(|e| OAuthError::Transient(format!("codex refresh body: {e}")))?
    };
    let code = error_code(&body);
    let permanent_code = code.as_deref().is_some_and(|c| {
        PERMANENT_ERROR_CODES
            .iter()
            .any(|p| p.eq_ignore_ascii_case(c))
    });
    let invalid_grant = status.as_u16() == 400
        && code
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("invalid_grant"));
    if !status.is_success() && (status.as_u16() == 401 || permanent_code || invalid_grant) {
        // tcr's own spelling of the code, never the body's text ([`known_error_code`]), so
        // it can be shown as why the login is dead (`AccountRuntime::login_error`).
        return Err(OAuthError::AuthRejected {
            status: status.as_u16(),
            code: code.as_deref().and_then(known_error_code),
        });
    }
    if status.as_u16() == 400 {
        let shown = match code.as_deref() {
            Some(code) => known_error_code(code).unwrap_or("unrecognised error code"),
            None => "no error code",
        };
        return Err(OAuthError::Transient(format!(
            "codex refresh HTTP 400 ({shown})"
        )));
    }
    if !status.is_success() {
        return Err(OAuthError::Transient(format!(
            "codex refresh HTTP {status}"
        )));
    }
    // The serde error's class and position only, never its message: serde quotes the value it
    // could not use, so a 200 whose body is a bare JSON string would be echoed into the log.
    let parsed: RefreshResponse = serde_json::from_slice(&body).map_err(|e| {
        OAuthError::Transient(format!(
            "codex refresh response was not a token object ({:?} error at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    let access_token = parsed
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            OAuthError::Transient("codex refresh response carried no access_token".to_string())
        })?;
    // An `exp` that is not strictly in the future is no `exp` at all: see `DEFAULT_LIFETIME_MS`.
    let now_ms = crate::now_ms();
    let expires_at_ms = jwt_exp_ms(&access_token)
        .filter(|&exp| exp > now_ms)
        .unwrap_or(now_ms + DEFAULT_LIFETIME_MS);
    // Same fallback as the Anthropic refresher: an omitted rotated token means the old one is
    // still live (`oauth::Tokens::refresh_token` doc), never `None`.
    let refresh_token = parsed
        .refresh_token
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| refresh_token.to_string());
    Ok(Tokens {
        access_token,
        refresh_token: Some(refresh_token),
        expires_at_ms,
    })
}

/// `exp` of a JWT, in epoch milliseconds. `None` for anything that is not a three-part JWT with
/// a JSON payload carrying a numeric `exp`, and for an `exp` too large to express in
/// milliseconds: checked for the reason `oauth`'s `expires_at_from` saturates, since a multiply
/// that overflowed into a negative expiry would make every expiry check true — a refresh on every
/// request, each one spending a single-use token.
pub fn jwt_exp_ms(jwt: &str) -> Option<i64> {
    let payload = jwt_payload(jwt)?;
    payload.get("exp")?.as_i64()?.checked_mul(1000)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdClaims {
    pub email: Option<String>,
    pub chatgpt_account_id: Option<String>,
    pub chatgpt_plan_type: Option<String>,
    /// The ChatGPT user the token belongs to: `chatgpt_user_id` under
    /// `https://api.openai.com/auth`, else `user_id` there, where codex reads it (`openai/codex`
    /// f82e365, `login/src/token_data.rs:71-105,142-147,174-198`). The account id names the
    /// workspace; this names the person in it.
    pub chatgpt_user_id: Option<String>,
    /// `chatgpt_account_is_fedramp`, under `https://api.openai.com/auth`: the login belongs to a
    /// FedRAMP workspace, and codex sends its requests with `X-OpenAI-Fedramp: true`
    /// (`openai/codex` f82e365, `login/src/token_data.rs:39-40,98`;
    /// `model-provider/src/bearer_auth_provider.rs:43-45`). `false` when the claim is absent.
    pub chatgpt_account_is_fedramp: bool,
}

/// The identity claims of a Codex id token or access token (`openai/codex`
/// `login/src/token_data.rs`, `IdClaims`, at commit 0c329ad).
///
/// `email` is read where Codex reads it: the top-level claim first, then the one under
/// `https://api.openai.com/profile` (`openai/codex` `login/src/token_data.rs:71-85,174-178` at
/// commit 0c329ad), so an id token that names its email only in that namespace still has one.
pub fn id_claims(jwt: &str) -> Option<IdClaims> {
    let payload = jwt_payload(jwt)?;
    let auth = payload.get("https://api.openai.com/auth");
    let profile = payload.get("https://api.openai.com/profile");
    let s = |v: Option<&serde_json::Value>, k: &str| {
        v.and_then(|o| o.get(k))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    Some(IdClaims {
        email: s(Some(&payload), "email").or_else(|| s(profile, "email")),
        chatgpt_account_id: s(auth, "chatgpt_account_id"),
        chatgpt_plan_type: s(auth, "chatgpt_plan_type"),
        chatgpt_user_id: s(auth, "chatgpt_user_id").or_else(|| s(auth, "user_id")),
        chatgpt_account_is_fedramp: auth
            .and_then(|o| o.get("chatgpt_account_is_fedramp"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    // A JWT has exactly three parts: two is not one, and neither is four.
    if parts.next().is_some() {
        return None;
    }
    let bytes = base64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// RFC 4648 §5 (URL-safe alphabet), padding optional. `None` on any character outside the
/// alphabet or an impossible length.
pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if s.len() % 4 == 1 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JWT_HEADER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0";
    const ACCESS_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiIxMTExMTExMS0xMTExLTQxMTEtODExMS0xMTExMTExMTExMTEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBybyJ9fQ";
    const ID_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiMTExMTExMTEtMTExMS00MTExLTgxMTEtMTExMTExMTExMTExIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwcm8iLCJjaGF0Z3B0X3VzZXJfaWQiOiJ1c2VyLWZha2UifX0";

    /// `{"exp":4102444800}`: 2100-01-01, an `exp` that stays in the future.
    const EXP_2100_PAYLOAD: &str = "eyJleHAiOjQxMDI0NDQ4MDB9";
    /// `{"exp":0}`: an `exp` already past.
    const EXP_0_PAYLOAD: &str = "eyJleHAiOjB9";

    fn access_jwt() -> String {
        format!("{JWT_HEADER}.{ACCESS_PAYLOAD}.sig")
    }
    fn id_jwt() -> String {
        format!("{JWT_HEADER}.{ID_PAYLOAD}.sig")
    }

    /// `{"https://api.openai.com/auth":{"user_id":"user-b"}}`: a token naming its user under the
    /// fallback key only.
    const USER_ID_ONLY_PAYLOAD: &str =
        "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsidXNlcl9pZCI6InVzZXItYiJ9fQ";
    /// `{"https://api.openai.com/auth":{}}`: a token naming no user at all.
    const NO_USER_PAYLOAD: &str = "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ";

    /// The ChatGPT user is read where codex reads it (openai/codex at f82e365,
    /// `codex-rs/login/src/token_data.rs:71-105,142-147,174-198`): the auth namespace's
    /// `chatgpt_user_id`, else its `user_id`, else nothing.
    ///
    /// Watched red: without the claim, every case reads `None`.
    #[test]
    fn the_user_id_is_read_from_the_auth_namespace_with_its_fallback() {
        let claims = |payload: &str| id_claims(&format!("{JWT_HEADER}.{payload}.sig"));
        assert_eq!(
            id_claims(&id_jwt()).and_then(|c| c.chatgpt_user_id),
            Some("user-fake".to_string())
        );
        assert_eq!(
            claims(USER_ID_ONLY_PAYLOAD).and_then(|c| c.chatgpt_user_id),
            Some("user-b".to_string())
        );
        assert_eq!(
            claims(NO_USER_PAYLOAD).and_then(|c| c.chatgpt_user_id),
            None
        );
    }

    #[test]
    fn base64url_decodes_with_and_without_padding() {
        assert_eq!(base64url_decode("aGk").unwrap(), b"hi");
        assert_eq!(base64url_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64url_decode("aGV5").unwrap(), b"hey");
        assert_eq!(base64url_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert!(base64url_decode("a").is_none());
        assert!(
            base64url_decode("a+b/").is_none(),
            "standard alphabet is not URL-safe alphabet"
        );
    }

    #[test]
    fn exp_is_read_in_milliseconds() {
        assert_eq!(jwt_exp_ms(&access_jwt()), Some(1_800_000_000_000));
        assert_eq!(jwt_exp_ms("not.a-jwt"), None);
        assert_eq!(
            jwt_exp_ms(&format!("{JWT_HEADER}.e30.sig")),
            None,
            "{{}} has no exp"
        );
    }

    /// An `exp` whose milliseconds overflow an `i64` reads as no `exp`, so a refresh falls back to
    /// [`DEFAULT_LIFETIME_MS`] rather than panicking (debug) or wrapping (release). The payload is
    /// `{"exp":9223372036854775807}`.
    #[test]
    fn an_exp_too_large_for_milliseconds_is_no_exp() {
        assert_eq!(
            jwt_exp_ms(&format!(
                "{JWT_HEADER}.eyJleHAiOjkyMjMzNzIwMzY4NTQ3NzU4MDd9.sig"
            )),
            None
        );
    }

    /// A JWT has exactly three parts. Four is not one, whatever its second part decodes to.
    ///
    /// Watched red: drop the check for a fourth part and the four-part token reads its `exp`.
    #[test]
    fn a_token_of_four_parts_is_not_a_jwt() {
        assert_eq!(
            jwt_exp_ms(&format!("{JWT_HEADER}.{ACCESS_PAYLOAD}.sig.extra")),
            None
        );
        assert_eq!(
            jwt_exp_ms(&format!("{JWT_HEADER}.{ACCESS_PAYLOAD}.sig")),
            Some(1_800_000_000_000),
            "control: the same token in three parts is read"
        );
    }

    #[test]
    fn identity_claims_come_from_the_openai_auth_namespace() {
        let claims = id_claims(&id_jwt()).unwrap();
        assert_eq!(claims.email.as_deref(), Some("alice@example.com"));
        assert_eq!(
            claims.chatgpt_account_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        assert_eq!(claims.chatgpt_plan_type.as_deref(), Some("pro"));
        let access = id_claims(&access_jwt()).unwrap();
        assert_eq!(access.email, None, "the access token carries no email");
        assert_eq!(
            access.chatgpt_account_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    /// An id token may carry its email under `https://api.openai.com/profile` instead of at the
    /// top level, and Codex reads it there (`login/src/token_data.rs:174-178` at commit 0c329ad);
    /// a top-level email still wins when both are present, Codex's order. The payloads are
    /// `{"https://api.openai.com/profile":{"email":"alice@example.com"}}` and
    /// `{"email":"alice@example.com","https://api.openai.com/profile":{"email":"bob@example.com"}}`.
    #[test]
    fn an_email_only_in_the_profile_claim_is_read_and_a_top_level_one_wins() {
        const PROFILE_ONLY_PAYLOAD: &str =
            "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL3Byb2ZpbGUiOnsiZW1haWwiOiJhbGljZUBleGFtcGxlLmNvbSJ9fQ";
        const BOTH_EMAILS_PAYLOAD: &str = "eyJlbWFpbCI6ImFsaWNlQGV4YW1wbGUuY29tIiwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9wcm9maWxlIjp7ImVtYWlsIjoiYm9iQGV4YW1wbGUuY29tIn19";
        let profile_only = id_claims(&format!("{JWT_HEADER}.{PROFILE_ONLY_PAYLOAD}.sig")).unwrap();
        assert_eq!(profile_only.email.as_deref(), Some("alice@example.com"));
        let both = id_claims(&format!("{JWT_HEADER}.{BOTH_EMAILS_PAYLOAD}.sig")).unwrap();
        assert_eq!(both.email.as_deref(), Some("alice@example.com"));
    }

    /// A FedRAMP workspace's token carries `chatgpt_account_is_fedramp: true` under
    /// `https://api.openai.com/auth`, the claim codex sends `X-OpenAI-Fedramp` on (`openai/codex`
    /// f82e365, `login/src/token_data.rs:39-40,98`). A token without the claim, or with it false,
    /// is not one. The payloads are `{"exp":1800000000,"email":"alice@example.com",
    /// "https://api.openai.com/auth":{"chatgpt_account_id":"11111111-1111-4111-8111-111111111111",
    /// "chatgpt_plan_type":"enterprise","chatgpt_account_is_fedramp":true}}` and
    /// `{"https://api.openai.com/auth":{"chatgpt_account_is_fedramp":false}}`.
    #[test]
    fn the_fedramp_claim_is_read_from_the_openai_auth_namespace() {
        const FEDRAMP_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiMTExMTExMTEtMTExMS00MTExLTgxMTEtMTExMTExMTExMTExIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJlbnRlcnByaXNlIiwiY2hhdGdwdF9hY2NvdW50X2lzX2ZlZHJhbXAiOnRydWV9fQ";
        const NOT_FEDRAMP_PAYLOAD: &str =
            "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lzX2ZlZHJhbXAiOmZhbHNlfX0";
        let fedramp = id_claims(&format!("{JWT_HEADER}.{FEDRAMP_PAYLOAD}.sig")).unwrap();
        assert!(fedramp.chatgpt_account_is_fedramp, "{fedramp:?}");
        assert_eq!(fedramp.chatgpt_plan_type.as_deref(), Some("enterprise"));
        let not_fedramp = id_claims(&format!("{JWT_HEADER}.{NOT_FEDRAMP_PAYLOAD}.sig")).unwrap();
        assert!(!not_fedramp.chatgpt_account_is_fedramp, "{not_fedramp:?}");
        assert!(
            !id_claims(&id_jwt()).unwrap().chatgpt_account_is_fedramp,
            "a token without the claim is not FedRAMP"
        );
    }

    #[test]
    fn error_code_reads_all_three_body_shapes() {
        assert_eq!(
            error_code(br#"{"error":"invalid_grant"}"#).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            error_code(br#"{"error":{"code":"refresh_token_reused","message":"x"}}"#).as_deref(),
            Some("refresh_token_reused")
        );
        assert_eq!(
            error_code(br#"{"code":"refresh_token_invalidated"}"#).as_deref(),
            Some("refresh_token_invalidated")
        );
        assert_eq!(error_code(b"nope"), None);
    }

    /// A stub token endpoint on an ephemeral loopback port, for the network tests below: it
    /// answers every POST with `status` and `body`, and keeps the JSON grant it was sent.
    async fn stub(
        status: u16,
        body: &'static str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    ) {
        use axum::{routing::post, Router};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen2 = seen.clone();
        let app = Router::new().route(
            "/oauth/token",
            post(move |axum::Json(body_in): axum::Json<serde_json::Value>| {
                let seen2 = seen2.clone();
                async move {
                    *seen2.lock().unwrap() = Some(body_in);
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/oauth/token"), seen)
    }

    /// The real response carries three fields; `id_token` rides along and is ignored, since
    /// `RefreshResponse` does not model it. The access token's `exp` is in 2100, so it is a
    /// future `exp`, kept as-is, on whatever day this runs: one in the past is no `exp` at all
    /// (`an_exp_already_past_takes_the_default_lifetime`).
    #[tokio::test]
    async fn a_successful_refresh_sends_the_json_grant_and_reads_exp() {
        let access = format!("{JWT_HEADER}.{EXP_2100_PAYLOAD}.sig");
        let (url, seen) = stub(
            200,
            Box::leak(
                format!(
                    r#"{{"access_token":"{access}","refresh_token":"rt-2","id_token":"{}"}}"#,
                    id_jwt()
                )
                .into_boxed_str(),
            ),
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let tokens = refresh_access_token_at(&client, "rt-1", &url)
            .await
            .unwrap();
        assert_eq!(tokens.access_token, access);
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-2"));
        assert_eq!(tokens.expires_at_ms, 4_102_444_800_000);
        let body = seen.lock().unwrap().clone().unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["refresh_token"], "rt-1");
    }

    #[tokio::test]
    async fn an_omitted_rotated_refresh_token_keeps_the_old_one() {
        let (url, _) = stub(
            200,
            Box::leak(format!(r#"{{"access_token":"{}"}}"#, access_jwt()).into_boxed_str()),
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let tokens = refresh_access_token_at(&client, "rt-1", &url)
            .await
            .unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
    }

    #[tokio::test]
    async fn invalid_grant_and_401_are_permanent_but_a_500_is_transient() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (url, _) = stub(400, r#"{"error":"invalid_grant"}"#).await;
        assert!(matches!(
            refresh_access_token_at(&client, "rt", &url).await,
            Err(OAuthError::AuthRejected {
                status: 400,
                code: Some("invalid_grant")
            })
        ));
        let (url, _) = stub(400, r#"{"error":{"code":"refresh_token_reused"}}"#).await;
        assert!(matches!(
            refresh_access_token_at(&client, "rt", &url).await,
            Err(OAuthError::AuthRejected {
                status: 400,
                code: Some("refresh_token_reused")
            })
        ));
        let (url, _) = stub(401, r#"{}"#).await;
        assert!(matches!(
            refresh_access_token_at(&client, "rt", &url).await,
            Err(OAuthError::AuthRejected {
                status: 401,
                code: None
            })
        ));
        let (url, _) = stub(400, r#"{"error":"temporarily_unavailable"}"#).await;
        assert!(matches!(
            refresh_access_token_at(&client, "rt", &url).await,
            Err(OAuthError::Transient(_))
        ));
        let (url, _) = stub(500, "boom").await;
        assert!(matches!(
            refresh_access_token_at(&client, "rt", &url).await,
            Err(OAuthError::Transient(_))
        ));
    }

    #[tokio::test]
    async fn dead_refresh_token_codes_are_permanent_at_every_error_status_and_body_shape() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut failures = Vec::new();
        for status in [400, 401, 403, 500] {
            for code in [
                "refresh_token_expired",
                "refresh_token_reused",
                "refresh_token_invalidated",
            ] {
                for body in [
                    serde_json::json!({"error": {"code": code}}),
                    serde_json::json!({"error": code}),
                    serde_json::json!({"code": code}),
                ] {
                    let body = body.to_string();
                    let (url, _) = stub(status, Box::leak(body.clone().into_boxed_str())).await;
                    let outcome = refresh_access_token_at(&client, "rt", &url).await;
                    let permanent = matches!(
                        &outcome,
                        Err(OAuthError::AuthRejected { status: actual_status, code: actual_code })
                            if *actual_status == status
                                && *actual_code == Some(code)
                    );
                    if !permanent {
                        failures.push(format!("HTTP {status} {body}: {outcome:?}"));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[tokio::test]
    async fn invalid_grant_without_a_400_or_401_is_transient() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for status in [403, 500] {
            let (url, _) = stub(status, r#"{"error":"invalid_grant"}"#).await;
            let outcome = refresh_access_token_at(&client, "rt", &url).await;
            assert!(
                matches!(outcome, Err(OAuthError::Transient(_))),
                "{outcome:?}"
            );
        }
    }

    /// An empty rotated refresh token is no rotated token: the old one is kept, never stored as
    /// `Some("")`, which no refresh could ever spend again.
    ///
    /// Watched red: drop the empty filter on `refresh_token` and `Some("")` comes back.
    #[tokio::test]
    async fn an_empty_rotated_refresh_token_keeps_the_old_one() {
        let (url, _) = stub(
            200,
            Box::leak(
                format!(
                    r#"{{"access_token":"{JWT_HEADER}.{EXP_2100_PAYLOAD}.sig","refresh_token":""}}"#
                )
                .into_boxed_str(),
            ),
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let tokens = refresh_access_token_at(&client, "rt-1", &url)
            .await
            .unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
    }

    /// An empty access token is no access token: the refresh failed, transiently.
    ///
    /// Watched red: drop the empty filter on `access_token` and the refresh succeeds with `""`.
    #[tokio::test]
    async fn an_empty_access_token_is_transient() {
        let (url, _) = stub(200, r#"{"access_token":"","refresh_token":"rt-2"}"#).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let outcome = refresh_access_token_at(&client, "rt-1", &url).await;
        assert!(
            matches!(outcome, Err(OAuthError::Transient(_))),
            "{outcome:?}"
        );
    }

    /// A 400's `error` is repeated in its Transient message, which the manager logs at WARN, only
    /// when it is an error code this module knows. Anything else is replaced by a fixed phrase:
    /// the refresh token echoed back with text around it, a code-shaped value past 64 bytes, and
    /// a short code-shaped value no list holds, here the bare token, which is exactly the shape
    /// of a code. A known one is still named: the control.
    ///
    /// Watched red: echo every value again and the token reaches the message; go back to
    /// echoing any short code-shaped value and the bare token does.
    #[tokio::test]
    async fn a_400_names_only_a_known_error_code_and_never_echoes_anything_else() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let message_for = |body: &'static str| {
            let client = client.clone();
            async move {
                let (url, _) = stub(400, body).await;
                match refresh_access_token_at(&client, "rt", &url).await {
                    Err(OAuthError::Transient(message)) => message,
                    other => panic!("a non-permanent 400 is transient: {other:?}"),
                }
            }
        };

        let echoed = message_for(r#"{"error":"rt-must-not-echo was refused"}"#).await;
        assert!(!echoed.contains("rt-must-not-echo"), "{echoed}");
        assert!(echoed.contains("unrecognised error code"), "{echoed}");

        let long_code = message_for(Box::leak(
            format!(r#"{{"error":"{}"}}"#, "a".repeat(65)).into_boxed_str(),
        ))
        .await;
        assert!(long_code.contains("unrecognised error code"), "{long_code}");

        let bare_token = message_for(r#"{"error":"rt-must-not-echo"}"#).await;
        assert!(!bare_token.contains("rt-must-not-echo"), "{bare_token}");
        assert!(
            bare_token.contains("unrecognised error code"),
            "{bare_token}"
        );

        let named = message_for(r#"{"error":"invalid_request"}"#).await;
        assert!(named.contains("invalid_request"), "control: {named}");
    }

    /// A 200 that is not a token object is Transient, and its message carries serde's class and
    /// position, never serde's own text: for a body that is a bare JSON string, that text quotes
    /// the string, and the message reaches the log at WARN.
    ///
    /// Watched red: put serde's message back and the body's string reaches the message.
    #[tokio::test]
    async fn a_200_that_is_not_a_token_object_never_echoes_its_body() {
        let (url, _) = stub(200, r#""rt-must-not-echo""#).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        match refresh_access_token_at(&client, "rt", &url).await {
            Err(OAuthError::Transient(message)) => {
                assert!(!message.contains("rt-must-not-echo"), "{message}");
                assert!(message.contains("Data error at line 1"), "{message}");
            }
            other => panic!("a 200 that is not a token object is transient: {other:?}"),
        }
    }

    /// An `exp` already past is no `exp`: `exp: 0` at face value makes every expiry
    /// check true, a refresh on every request, each spending a single-use token. It takes
    /// [`DEFAULT_LIFETIME_MS`] from now, as a missing `exp` does. A future `exp` is kept as-is:
    /// `a_successful_refresh_sends_the_json_grant_and_reads_exp`.
    ///
    /// Watched red: drop the `exp > now` filter and the expiry is 0.
    #[tokio::test]
    async fn an_exp_already_past_takes_the_default_lifetime() {
        let (url, _) = stub(
            200,
            Box::leak(
                format!(r#"{{"access_token":"{JWT_HEADER}.{EXP_0_PAYLOAD}.sig"}}"#)
                    .into_boxed_str(),
            ),
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let before = crate::now_ms();
        let tokens = refresh_access_token_at(&client, "rt-1", &url)
            .await
            .unwrap();
        let after = crate::now_ms();
        assert!(
            tokens.expires_at_ms >= before + DEFAULT_LIFETIME_MS
                && tokens.expires_at_ms <= after + DEFAULT_LIFETIME_MS,
            "expected about {DEFAULT_LIFETIME_MS} ms from now ({before}..{after}), got {}",
            tokens.expires_at_ms
        );
    }

    /// A 401 is permanent even when its body stalls. The stub sends the status line and a
    /// `content-length` at once and the body never: reading without a bound delayed rejection
    /// until the refresh timeout and returned Transient, to be retried.
    ///
    /// Watched red: read the body before the 401 check again and the refresh outlives the wait.
    #[tokio::test]
    async fn a_401_with_a_stalled_body_is_still_rejected_promptly() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
                      content-length: 64\r\n\r\n",
                )
                .await
                .unwrap();
            // The body is never sent; the connection stays open past the wait below.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            drop(socket);
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{addr}/oauth/token");
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            refresh_access_token_at(&client, "rt", &url),
        )
        .await
        .expect("a 401 is rejected promptly even when its body never arrives");
        assert!(
            matches!(
                outcome,
                Err(OAuthError::AuthRejected {
                    status: 401,
                    code: None
                })
            ),
            "{outcome:?}"
        );
    }
}
