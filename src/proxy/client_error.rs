//! A refusal tcr writes itself: one value carrying the status, the kind, the message and the
//! retry advice. On the request path it is answered in the envelope of the request's provider
//! ([`ClientError::respond`]); on a local `/_tcr/*` route, which has no provider, in the
//! Anthropic envelope every local route has always answered in ([`ClientError::respond_local`]).

use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;

use crate::provider::Provider;

use super::{codex_error_response, error_response, quiet_error_response};

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
    /// tcr failed at its own work (a payload that would not serialize, a config write that
    /// failed), as distinct from [`Self::Proxy`], a failure reaching an upstream.
    Api,
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
            Self::Api => "api_error",
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
    pub(super) const fn retry_after(mut self, seconds: i64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// Whether the client may resend on its own, sent as `x-should-retry`.
    pub(super) const fn should_retry(mut self, should_retry: bool) -> Self {
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

    /// The answer of a local `/_tcr/*` route. Those routes are tcr's own and serve no
    /// provider, so they keep the Anthropic envelope they have always answered in, which is
    /// what `tcr`'s CLI reads; the warn line is the same as on the request path.
    pub(super) fn respond_local(self) -> Response {
        self.build(Provider::Anthropic, true)
    }

    /// The envelope is the provider's: Anthropic's `{"type":"error","error":{...}}`, and
    /// codex's own `{"error":{...}}` (the shape [`super::codex_error_body`] writes, without a
    /// [`tcr_status_wire::CodexRefusalKind`] code, since these refusals are not one of those).
    /// The Anthropic envelope with its warn line is [`super::error_response`] itself, so that
    /// function stays the one constructor of a warned Anthropic refusal.
    fn build(self, provider: Provider, warn: bool) -> Response {
        let kind = self.kind.as_str();
        let mut response = match (provider, warn) {
            (Provider::Anthropic, true) => {
                error_response(self.status, kind, &self.message, self.retry_after)
            }
            (Provider::Anthropic, false) => {
                quiet_error_response(self.status, kind, &self.message, self.retry_after)
            }
            (Provider::Codex, warn) => {
                if warn {
                    warn_refusal(self.status, kind, &self.message);
                }
                codex_error_response(
                    self.status,
                    serde_json::json!({
                        "error": { "type": kind, "message": self.message },
                    })
                    .to_string(),
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

    /// The local `/_tcr/*` routes answer every refusal in the bytes they answered before
    /// their error type became an [`ErrorKind`]: the Anthropic envelope, serialized with
    /// sorted keys exactly as `serde_json::json!` wrote it, `application/json`, no advice
    /// headers, and the route's stamp where the route stamps. Each route's error shapes are
    /// driven through [`super::super::app`], so the gate, the method fallbacks and the
    /// command prologue are the production ones.
    mod local_routes {
        use std::net::SocketAddr;
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::header::{CONTENT_TYPE, HOST, ORIGIN};
        use axum::http::{Method, Request, StatusCode};
        use tower::ServiceExt as _;

        use crate::config::{Account, Config};
        use crate::manager::Manager;
        use crate::proxy::{
            ADD_ACCOUNT_PATH, CONFIG_PATH, CONTROL_PATH, ClientAddr, DISABLED_PATH,
            ENDPOINT_HEADER, PRIORITY_PATH, PROVIDERS_HEADER, QUOTA_PATH, STATUS_PATH,
        };

        use super::super::{ClientError, ErrorKind};

        struct NoRefresh;
        impl crate::oauth::TokenRefresher for NoRefresh {
            fn refresh(
                &self,
                _provider: crate::provider::Provider,
                _t: String,
            ) -> crate::oauth::RefreshFuture {
                Box::pin(async { Err(crate::oauth::OAuthError::Transient("unused".into())) })
            }
        }

        fn manager(api_key: Option<&str>) -> Arc<Manager> {
            let mut config = Config::fixture(vec![Account::fixture("alice@example.com")]);
            config.proxy.api_key = api_key.map(str::to_string);
            Manager::new(
                config,
                Arc::new(NoRefresh),
                Arc::new(crate::probe::LiveUsageProber::new()),
                Arc::new(crate::warmer::LiveWarmer::new()),
                None,
            )
        }

        fn loopback() -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], 54_321))
        }

        /// The exact body bytes a local refusal of `kind` carrying `message` was written as.
        fn envelope(kind: &str, message: &str) -> String {
            format!(r#"{{"error":{{"message":"{message}","type":"{kind}"}},"type":"error"}}"#)
        }

        struct Case<'a> {
            method: Method,
            path: &'static str,
            /// `None`: the request did not come through the listener, so carries no peer.
            peer: Option<SocketAddr>,
            headers: &'static [(axum::http::HeaderName, &'static str)],
            body: &'static str,
            api_key: Option<&'static str>,
            status: StatusCode,
            stamp: Option<&'static str>,
            kind: &'static str,
            message: &'a str,
        }

        const JSON: &[(axum::http::HeaderName, &str)] = &[(CONTENT_TYPE, "application/json")];
        const TEXT: &[(axum::http::HeaderName, &str)] = &[(CONTENT_TYPE, "text/plain")];

        async fn check(case: Case<'_>) {
            let mut builder = Request::builder()
                .method(case.method.clone())
                .uri(case.path);
            for (name, value) in case.headers {
                builder = builder.header(name, *value);
            }
            let mut req = builder.body(Body::from(case.body)).expect("build request");
            if let Some(peer) = case.peer {
                req.extensions_mut().insert(ClientAddr(peer));
            }
            let response = crate::proxy::app(manager(case.api_key))
                .oneshot(req)
                .await
                .expect("router response");

            let label = format!("{} {}", case.method, case.path);
            assert_eq!(response.status(), case.status, "{label}");
            let headers = response.headers().clone();
            assert_eq!(headers[CONTENT_TYPE], "application/json", "{label}");
            assert!(headers.get("retry-after").is_none(), "{label}");
            assert!(headers.get("x-should-retry").is_none(), "{label}");
            assert_eq!(
                headers
                    .get(ENDPOINT_HEADER)
                    .map(|v| v.to_str().expect("ascii stamp")),
                case.stamp,
                "{label}"
            );
            assert_eq!(
                headers.contains_key(PROVIDERS_HEADER),
                case.stamp == Some("accounts-add"),
                "{label}: only the account-add route advertises its providers"
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read body");
            assert_eq!(
                String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
                envelope(case.kind, case.message),
                "{label}"
            );
        }

        /// The read routes: their 405s, unstamped, and each of the gate's four refusals.
        #[tokio::test]
        async fn the_read_routes_and_the_gate_answer_in_the_same_bytes() {
            let cases = [
                Case {
                    method: Method::POST,
                    path: STATUS_PATH,
                    peer: Some(loopback()),
                    headers: &[],
                    body: "",
                    api_key: None,
                    status: StatusCode::METHOD_NOT_ALLOWED,
                    stamp: None,
                    kind: "invalid_request_error",
                    message: "The tcr status endpoint is read-only: GET.",
                },
                Case {
                    method: Method::POST,
                    path: QUOTA_PATH,
                    peer: Some(loopback()),
                    headers: &[],
                    body: "",
                    api_key: None,
                    status: StatusCode::METHOD_NOT_ALLOWED,
                    stamp: None,
                    kind: "invalid_request_error",
                    message: "The tcr quota endpoint is read-only: GET.",
                },
                Case {
                    method: Method::GET,
                    path: STATUS_PATH,
                    peer: None,
                    headers: &[],
                    body: "",
                    api_key: None,
                    status: StatusCode::FORBIDDEN,
                    stamp: None,
                    kind: "permission_error",
                    message: "The tcr status endpoint is loopback-only.",
                },
                Case {
                    method: Method::GET,
                    path: QUOTA_PATH,
                    peer: Some(loopback()),
                    headers: &[],
                    body: "",
                    api_key: Some("sk-test-proxy-key"),
                    status: StatusCode::UNAUTHORIZED,
                    stamp: None,
                    kind: "authentication_error",
                    message: "Missing or invalid x-api-key.",
                },
                Case {
                    method: Method::GET,
                    path: STATUS_PATH,
                    peer: Some(loopback()),
                    headers: &[(HOST, "evil.example")],
                    body: "",
                    api_key: None,
                    status: StatusCode::MISDIRECTED_REQUEST,
                    stamp: None,
                    kind: "invalid_request_error",
                    message: "The tcr status endpoint answers only to a loopback host, not to 'evil.example'.",
                },
                Case {
                    method: Method::GET,
                    path: QUOTA_PATH,
                    peer: Some(loopback()),
                    headers: &[(ORIGIN, "https://evil.example")],
                    body: "",
                    api_key: None,
                    status: StatusCode::FORBIDDEN,
                    stamp: None,
                    kind: "permission_error",
                    message: "The tcr quota endpoint does not serve cross-site requests.",
                },
                // A mutating route's gate refusal is not stamped either.
                Case {
                    method: Method::POST,
                    path: DISABLED_PATH,
                    peer: None,
                    headers: JSON,
                    body: "{}",
                    api_key: None,
                    status: StatusCode::FORBIDDEN,
                    stamp: None,
                    kind: "permission_error",
                    message: "The tcr disable-account endpoint is loopback-only.",
                },
            ];
            for case in cases {
                check(case).await;
            }
        }

        /// Every command route's stamped 405 and 415, and the not-found and conflict
        /// refusals a fixture fleet with no config file can reach.
        #[tokio::test]
        async fn the_command_routes_answer_in_the_same_stamped_bytes() {
            let routes = [
                (
                    DISABLED_PATH,
                    "accounts-disabled",
                    "disable-account",
                    "POST",
                ),
                (ADD_ACCOUNT_PATH, "accounts-add", "account-add", "POST"),
                (CONTROL_PATH, "account-control", "control-account", "POST"),
                (PRIORITY_PATH, "accounts-priority", "priority", "POST"),
                (CONFIG_PATH, "config", "settings", "GET or POST"),
            ];
            for (path, stamp, name, verbs) in routes {
                let not_allowed = format!("The tcr {name} endpoint takes {verbs}.");
                let not_json =
                    format!("The tcr {name} endpoint requires Content-Type: application/json.");
                check(Case {
                    method: Method::PUT,
                    path,
                    peer: Some(loopback()),
                    headers: JSON,
                    body: "{}",
                    api_key: None,
                    status: StatusCode::METHOD_NOT_ALLOWED,
                    stamp: Some(stamp),
                    kind: "invalid_request_error",
                    message: &not_allowed,
                })
                .await;
                check(Case {
                    method: Method::POST,
                    path,
                    peer: Some(loopback()),
                    headers: TEXT,
                    body: "{}",
                    api_key: None,
                    status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    stamp: Some(stamp),
                    kind: "invalid_request_error",
                    message: &not_json,
                })
                .await;
            }

            check(Case {
                method: Method::POST,
                path: DISABLED_PATH,
                peer: Some(loopback()),
                headers: JSON,
                body: r#"{"query":"nobody","disabled":true}"#,
                api_key: None,
                status: StatusCode::NOT_FOUND,
                stamp: Some("accounts-disabled"),
                kind: "not_found_error",
                message: "No account in the live rotation matches 'nobody'.",
            })
            .await;
            check(Case {
                method: Method::POST,
                path: CONFIG_PATH,
                peer: Some(loopback()),
                headers: JSON,
                body: r#"{"key":"switchThreshold","value":"0.9"}"#,
                api_key: None,
                status: StatusCode::CONFLICT,
                stamp: Some("config"),
                kind: "invalid_request_error",
                message: "This proxy was started without a config file, so it has nowhere to save a setting. Nothing was changed.",
            })
            .await;
        }

        /// The one kind no fixture request reaches, a local 500, in the bytes
        /// `error_response(status, "api_error", message, None)` wrote.
        #[tokio::test]
        async fn a_local_api_error_is_the_same_bytes() {
            let response = ClientError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorKind::Api,
                "Could not serialize the status snapshot.",
            )
            .respond_local();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
            assert_eq!(response.headers().len(), 1, "content-type and nothing else");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read body");
            assert_eq!(
                String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
                envelope("api_error", "Could not serialize the status snapshot.")
            );
        }
    }
}
