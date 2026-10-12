//! Which upstream an account, and a request, belongs to.
//!
//! `tcr` was an Anthropic-only proxy until 2026-10: one `upstream`, one token shape, one set of
//! quota headers. Codex (the `codex` CLI on a ChatGPT login) is the second upstream. This is one
//! enum rather than a trait because there are two variants and every provider-specific site in the
//! crate is an exhaustive `match`: adding a third provider is a compiler-guided walk through each
//! of them, which is the property this crate's explicit, documented style wants.
//!
//! # A request's provider is decided by PATH, not by host
//!
//! Every Codex request lives under `/backend-api/` (`openai/codex`,
//! `model-provider-info/src/lib.rs:80`, `CHATGPT_CODEX_BASE_URL`), and nothing Anthropic sends
//! does. Resolving by path works identically in MITM mode (where the CONNECT host is
//! `chatgpt.com`) and in the origin-form requests this crate's in-process tests send, which carry
//! no meaningful host. The host guard in `proxy.rs` stays the consistency check it already is.

use serde::{Deserialize, Serialize};

/// The upstream a credential belongs to. Absent on disk means [`Provider::Anthropic`], which is
/// what every account was before this type existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Anthropic,
    Codex,
}

/// The host Codex's inference and backend calls go to by default. A ChatGPT login whose workspace
/// routing names another backend origin sends them there instead, and a residency-routed or
/// FedRAMP workspace marks them with a header of its own (`openai/codex` f82e365,
/// `model-provider/src/workspace_routing.rs:12,39-77`,
/// `model-provider/src/bearer_auth_provider.rs:43-45`).
pub const CODEX_HOST: &str = "chatgpt.com";
/// Default `Config::codex_upstream`.
pub const CODEX_UPSTREAM: &str = "https://chatgpt.com";
/// Every Codex request path starts here; nothing Anthropic sends does. Classify with
/// [`Provider::of_path`]; a bare `starts_with` on this constant also matches `/backend-apix`.
pub const CODEX_PATH_PREFIX: &str = "/backend-api";
/// The ONE Codex path tcr re-credentials. Exact match, like `/v1/messages`
/// (`openai/codex` `codex-api/src/endpoint/responses.rs`).
pub const CODEX_RESPONSES_PATH: &str = "/backend-api/codex/responses";
/// Request header naming the ChatGPT account the bearer belongs to, which codex sends as
/// `ChatGPT-Account-ID` beside its bearer (`openai/codex`
/// `codex-rs/model-provider/src/bearer_auth_provider.rs`, `BearerAuthProvider::add_auth_headers`,
/// read at f82e365). tcr replaces it with the serving account's.
pub const CODEX_ACCOUNT_HEADER: &str = "chatgpt-account-id";
/// Sent by Codex on every turn (`codex-api/src/requests/headers.rs:8`); the session-affinity key.
pub const CODEX_SESSION_HEADER: &str = "session-id";
/// The quota probe (`backend-client/src/client/rate_limit_resets.rs:127`). No inference, no cost.
pub const CODEX_USAGE_PATH: &str = "/backend-api/wham/usage";

impl Provider {
    /// Which provider a request path belongs to. `path` is the query-stripped path `handle`
    /// already derives.
    pub fn of_path(path: &str) -> Self {
        if path == CODEX_PATH_PREFIX || path.starts_with("/backend-api/") {
            Self::Codex
        } else {
            Self::Anthropic
        }
    }

    /// Is `path` an inference call this provider re-credentials from the pool? Exact matches
    /// only: `/v1/messages/count_tokens` is inference for Anthropic (it must never be relayed with
    /// the client's own key), `/backend-api/codex/responses/anything` is not for Codex.
    pub fn is_inference_path(self, path: &str) -> bool {
        match self {
            Self::Anthropic => path == "/v1/messages" || path == "/v1/messages/count_tokens",
            Self::Codex => path == CODEX_RESPONSES_PATH,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Codex => "codex",
        }
    }

    /// For `#[serde(skip_serializing_if)]` on `config::Account::provider`: an Anthropic row writes
    /// no key, so an upgraded Anthropic-only config re-saves byte-identically.
    pub fn is_anthropic(&self) -> bool {
        *self == Self::Anthropic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn of_path_puts_backend_api_under_codex_and_everything_else_under_anthropic() {
        assert_eq!(
            Provider::of_path("/backend-api/codex/responses"),
            Provider::Codex
        );
        assert_eq!(
            Provider::of_path("/backend-api/wham/usage"),
            Provider::Codex
        );
        assert_eq!(Provider::of_path("/backend-api"), Provider::Codex);
        assert_eq!(Provider::of_path("/backend-apix/evil"), Provider::Anthropic);
        assert_eq!(Provider::of_path("/v1/messages"), Provider::Anthropic);
        assert_eq!(Provider::of_path("/api/oauth/usage"), Provider::Anthropic);
        assert_eq!(Provider::of_path(""), Provider::Anthropic);
    }

    #[test]
    fn inference_paths_are_exact_matches_per_provider() {
        assert!(Provider::Anthropic.is_inference_path("/v1/messages"));
        assert!(Provider::Anthropic.is_inference_path("/v1/messages/count_tokens"));
        assert!(!Provider::Anthropic.is_inference_path("/v1/messages/"));
        assert!(Provider::Codex.is_inference_path("/backend-api/codex/responses"));
        assert!(!Provider::Codex.is_inference_path("/backend-api/codex/responses/"));
        assert!(!Provider::Codex.is_inference_path("/backend-api/wham/usage"));
        assert!(!Provider::Codex.is_inference_path("/v1/messages"));
        assert!(!Provider::Anthropic.is_inference_path("/backend-api/codex/responses"));
    }

    #[test]
    fn serde_is_lowercase_and_defaults_to_anthropic() {
        assert_eq!(
            serde_json::to_string(&Provider::Codex).unwrap(),
            "\"codex\""
        );
        assert_eq!(
            serde_json::from_str::<Provider>("\"codex\"").unwrap(),
            Provider::Codex
        );
        assert_eq!(
            serde_json::from_str::<Provider>("\"anthropic\"").unwrap(),
            Provider::Anthropic
        );
        assert!(serde_json::from_str::<Provider>("\"openai\"").is_err());
        // Case-sensitive: the on-disk spelling is exactly the lowercase one.
        assert!(serde_json::from_str::<Provider>("\"Codex\"").is_err());
        // The name logs print (`as_str`) is the name on disk (serde), for every variant.
        for p in [Provider::Anthropic, Provider::Codex] {
            assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
        }
        assert_eq!(Provider::default(), Provider::Anthropic);
        assert!(Provider::Anthropic.is_anthropic());
        assert!(!Provider::Codex.is_anthropic());
    }
}
