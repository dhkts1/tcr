//! Which upstream an account, and a request, belongs to.
//!
//! `tcr` was an Anthropic-only proxy until 2026-10: one `upstream`, one token shape, one set of
//! quota headers. Codex (the `codex` CLI on a ChatGPT login) is the second upstream. This is one
//! enum rather than a trait because there are two variants and every provider-specific site in the
//! crate is an exhaustive `match`: adding a third provider is a compiler-guided walk through each
//! of them, which is the property this crate's explicit, documented style wants.
//!
//! # Request routing
//!
//! Backend paths under `/backend-api/` belong to Codex; other paths default to Anthropic.
//! A marked ChatGPT CONNECT is the exception: its authoritative tunnel host keeps every
//! path on ChatGPT, and paths outside `/backend-api/` relay on the client's credential.
//! The proxy refuses a request that names a different host than its tunnel, and refuses
//! an Anthropic tunnel carrying a Codex backend path. Base-URL requests retain path routing.

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
/// The Codex Responses path tcr re-credentials. Exact match, like `/v1/messages`
/// (`openai/codex` `codex-api/src/endpoint/responses.rs`).
pub const CODEX_RESPONSES_PATH: &str = "/backend-api/codex/responses";
/// Request header naming the ChatGPT account the bearer belongs to, which codex sends as
/// `ChatGPT-Account-ID` beside its bearer (`openai/codex`
/// `codex-rs/model-provider/src/bearer_auth_provider.rs`, `BearerAuthProvider::add_auth_headers`,
/// read at f82e365). tcr replaces it with the serving account's.
pub const CODEX_ACCOUNT_HEADER: &str = "chatgpt-account-id";
/// The request headers by which a ChatGPT workspace that is not served as a pooled account is
/// routed: a `us` or `us_cr` data residency's override, and FedRAMP's marker, sent `true` when
/// the login's `chatgpt_account_is_fedramp` claim is set (`openai/codex` f82e365,
/// `model-provider/src/workspace_routing.rs:12,39-77`,
/// `model-provider/src/bearer_auth_provider.rs:43-45`). A Codex request carrying either is
/// relayed on the client's own credential and never pooled (`proxy::names_codex_routing`).
pub const CODEX_ROUTING_HEADERS: [&str; 2] =
    ["x-openai-account-routing-override", "x-openai-fedramp"];
/// Sent by Codex on every turn (`codex-api/src/requests/headers.rs:8`); the session-affinity key.
pub const CODEX_SESSION_HEADER: &str = "session-id";
/// The quota probe (`backend-client/src/client/rate_limit_resets.rs:127`). No inference, no cost.
pub const CODEX_USAGE_PATH: &str = "/backend-api/wham/usage";

impl Provider {
    /// Every provider, in declaration order: what a server built from this source can store,
    /// and therefore what it advertises on its account-add route (`proxy::PROVIDERS_HEADER`).
    /// `all_holds_every_provider_exactly_once` checks it against the variant list serde's derive
    /// writes from the enum itself, so a variant that is not listed here fails that test.
    pub const ALL: [Self; 2] = [Self::Anthropic, Self::Codex];

    /// The provider whose [`Self::as_str`] is exactly `name`; `None` for a name this build does
    /// not know. The inverse of `as_str` over [`Self::ALL`], so the two cannot disagree.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.as_str() == name)
    }

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
            Self::Codex => matches!(
                path,
                CODEX_RESPONSES_PATH
                    | "/backend-api/codex/images/generations"
                    | "/backend-api/codex/images/edits"
                    | "/backend-api/codex/memories/trace_summarize"
                    | "/backend-api/codex/alpha/search"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Codex => "codex",
        }
    }

    /// May this provider's requests and accounts cross the peer mesh, carried to a peer's exit or
    /// lent as a lease? Codex stays off it in this version: a lease is sized on the lender's
    /// Anthropic windows, and the carry path builds an Anthropic URL and splices an Anthropic
    /// body. The one spelling of the rule: a provider added later answers here, and letting Codex
    /// onto the mesh is a one-line change.
    pub fn on_peer_mesh(self) -> bool {
        match self {
            Self::Anthropic => true,
            Self::Codex => false,
        }
    }

    /// Does this provider have a Fable weekly quota bucket? It is an Anthropic fact: a Codex
    /// request whose model id merely contains "fable" never consults the bucket.
    pub fn has_fable_bucket(self) -> bool {
        match self {
            Self::Anthropic => true,
            Self::Codex => false,
        }
    }

    /// For `#[serde(skip_serializing_if)]` on `config::Account::provider`: an Anthropic row writes
    /// no key, so an upgraded Anthropic-only config re-saves without a `provider` key.
    pub fn is_anthropic(&self) -> bool {
        match self {
            Self::Anthropic => true,
            Self::Codex => false,
        }
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
    fn policy_questions_are_answered_per_provider() {
        assert!(Provider::Anthropic.on_peer_mesh());
        assert!(!Provider::Codex.on_peer_mesh());
        assert!(Provider::Anthropic.has_fable_bucket());
        assert!(!Provider::Codex.has_fable_bucket());
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

    /// The variant names serde's derive wrote for [`Provider`], in declaration order: the
    /// `variants` it hands `deserialize_enum`, recorded by a deserializer that reads nothing.
    fn serde_variant_names() -> &'static [&'static str] {
        use serde::de::{Error as _, Visitor};

        struct Recorder(std::cell::Cell<&'static [&'static str]>);

        impl<'de> serde::Deserializer<'de> for &Recorder {
            type Error = serde::de::value::Error;

            fn deserialize_enum<V: Visitor<'de>>(
                self,
                _name: &'static str,
                variants: &'static [&'static str],
                _visitor: V,
            ) -> Result<V::Value, Self::Error> {
                self.0.set(variants);
                Err(Self::Error::custom("recorded the variant names"))
            }

            fn deserialize_any<V: Visitor<'de>>(
                self,
                _visitor: V,
            ) -> Result<V::Value, Self::Error> {
                Err(Self::Error::custom("Provider deserializes as an enum"))
            }

            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
                byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map
                struct identifier ignored_any
            }
        }

        let recorder = Recorder(std::cell::Cell::new(&[]));
        assert!(<Provider as serde::Deserialize>::deserialize(&recorder).is_err());
        recorder.0.get()
    }

    /// [`Provider::ALL`] holds every variant exactly once, in declaration order, which is what
    /// keeps a server from forgetting to advertise a provider it can store. The variants are read
    /// from serde's derive, the one list of them the compiler writes from the enum itself, so a
    /// variant added to the enum and not to `ALL` fails here. Neither a `match` nor a `const`
    /// can count them on stable Rust: a `match` sees only the values it is handed, and
    /// `std::mem::variant_count` is unstable. Comparing names also ties [`Provider::as_str`] to
    /// the name on disk for every variant, a future one included.
    ///
    /// Watched red: a temporary third variant, given an arm in every `match` but left out of
    /// `ALL`, fails the first assertion; a fixed count of two passed it.
    #[test]
    fn all_holds_every_provider_exactly_once() {
        let listed: Vec<&str> = Provider::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(
            listed,
            serde_variant_names(),
            "Provider::ALL must list every variant once, in declaration order"
        );
        for provider in Provider::ALL {
            assert_eq!(serde_json::to_value(provider).unwrap(), provider.as_str());
            assert_eq!(Provider::from_name(provider.as_str()), Some(provider));
        }
        assert_eq!(
            Provider::from_name("Codex"),
            None,
            "case-sensitive, like serde"
        );
        assert_eq!(Provider::from_name("openai"), None);
    }
}
