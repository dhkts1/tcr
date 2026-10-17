//! A refusal tcr writes itself, on the request path: one value carrying the status, the kind,
//! the message and the retry advice, answered in the envelope of the request's provider.

use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;

use crate::provider::Provider;

use super::{error_response, quiet_error_response};

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

    fn build(self, provider: Provider, warn: bool) -> Response {
        let mut response = match provider {
            Provider::Anthropic | Provider::Codex => {
                let build = if warn {
                    error_response
                } else {
                    quiet_error_response
                };
                build(
                    self.status,
                    self.kind.as_str(),
                    &self.message,
                    self.retry_after,
                )
            }
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
