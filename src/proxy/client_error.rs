//! A refusal tcr writes itself, on the request path: one value carrying the status, the kind,
//! the message and the retry advice, answered in the envelope of the request's provider.

use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;

use crate::provider::Provider;

use super::{codex_error_response, quiet_error_response};

/// The warn line every proxy-generated refusal carries (see [`super::error_response`]), in
/// either envelope.
pub(super) fn warn_refusal(status: StatusCode, err_type: &str, message: &str) {
    tracing::warn!(
        status = status.as_u16(),
        error.r#type = err_type,
        error.message = message,
        "proxy-generated error response"
    );
}

/// The `error.type` of a refusal tcr writes itself. One spelling per kind, so two sites can
/// never answer the same refusal under two names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ErrorKind {
    InvalidRequest,
    Proxy,
    NotFound,
    Authentication,
    Permission,
    RateLimit,
    /// Codex's 426, which asks its client to fall back to SSE.
    UpgradeRequired,
}

impl ErrorKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request_error",
            Self::Proxy => "proxy_error",
            Self::NotFound => "not_found_error",
            Self::Authentication => "authentication_error",
            Self::Permission => "permission_error",
            Self::RateLimit => "rate_limit_error",
            Self::UpgradeRequired => "upgrade_required",
        }
    }
}

/// A refusal on the request path, built once and answered through [`ClientError::respond`].
#[derive(Debug, Clone)]
pub(super) struct ClientError {
    status: StatusCode,
    kind: ErrorKind,
    message: String,
    retry_after: Option<i64>,
    should_retry: Option<bool>,
}

impl ClientError {
    pub(super) fn new(status: StatusCode, kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after: None,
            should_retry: None,
        }
    }

    /// Seconds the client is asked to wait, sent as `retry-after`.
    pub(super) fn retry_after(mut self, seconds: i64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// Whether the client may resend on its own, sent as `x-should-retry`.
    pub(super) fn should_retry(mut self, should_retry: bool) -> Self {
        self.should_retry = Some(should_retry);
        self
    }

    /// The response, in `provider`'s envelope, with the warn line every refusal carries.
    pub(super) fn respond(self, provider: Provider) -> Response {
        self.build(provider, true)
    }

    /// [`ClientError::respond`] without the warn line, for an answer a healthy session gets as
    /// a matter of course; the caller logs it at the level it deserves.
    pub(super) fn respond_quiet(self, provider: Provider) -> Response {
        self.build(provider, false)
    }

    /// The envelope is the provider's: Anthropic's `{"type":"error","error":{...}}`, and
    /// codex's own `{"error":{...}}` (the shape [`super::codex_error_body`] writes, without a
    /// [`tcr_status_wire::CodexRefusalKind`] code, since these refusals are not one of those).
    fn build(self, provider: Provider, warn: bool) -> Response {
        if warn {
            warn_refusal(self.status, self.kind.as_str(), &self.message);
        }
        let mut response = match provider {
            Provider::Anthropic => quiet_error_response(
                self.status,
                self.kind.as_str(),
                &self.message,
                self.retry_after,
            ),
            Provider::Codex => codex_error_response(
                self.status,
                serde_json::json!({
                    "error": { "type": self.kind.as_str(), "message": self.message },
                })
                .to_string(),
                self.retry_after,
            ),
        };
        if let Some(should_retry) = self.should_retry {
            response.headers_mut().insert(
                "x-should-retry",
                HeaderValue::from_static(if should_retry { "true" } else { "false" }),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_of(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// A Codex request's refusal is in codex's own envelope, with no Anthropic `"type":
    /// "error"` wrapper; the status and both advice headers are the same either way.
    #[tokio::test]
    async fn a_codex_refusal_is_in_codex_envelope() {
        let refusal = ClientError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Proxy,
            "Upstream unreachable.",
        )
        .retry_after(5)
        .should_retry(true);

        let codex = refusal.clone().respond(Provider::Codex);
        assert_eq!(codex.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(codex.headers()["retry-after"], "5");
        assert_eq!(codex.headers()["x-should-retry"], "true");
        assert_eq!(codex.headers()["content-type"], "application/json");
        assert_eq!(
            body_of(codex).await,
            serde_json::json!({
                "error": { "type": "proxy_error", "message": "Upstream unreachable." },
            })
        );

        let anthropic = refusal.respond(Provider::Anthropic);
        assert_eq!(anthropic.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(anthropic.headers()["retry-after"], "5");
        assert_eq!(anthropic.headers()["x-should-retry"], "true");
        assert_eq!(
            body_of(anthropic).await,
            serde_json::json!({
                "type": "error",
                "error": { "type": "proxy_error", "message": "Upstream unreachable." },
            })
        );
    }
}
