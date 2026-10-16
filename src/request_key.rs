//! The request key: a secret a caller presents to say it was launched on purpose.
//!
//! Binding to `127.0.0.1` does not keep other callers out. A container on this
//! Mac that connects to `host.docker.internal` arrives at the listener with source
//! address `127.0.0.1`, so to tcr it looks like a process on the Mac, and both the
//! loopback exemption in `proxy::handle_request` and the open `CONNECT` tunnel in
//! `mitm::handle_connect` serve it. The request key is what tells the two apart.
//!
//! This wave only LEARNS the key. Nothing is refused: a caller without it is
//! counted ([`UnkeyedCounters`]) and served exactly as before. The counters are
//! what decides when refusing is safe, which is a later change.
//!
//! The key lives at `proxy.requestKey` in the config, separate from
//! `proxy.apiKey`: that one gates the `/_tcr/` control routes, and a container
//! forwarder that holds the request key must not get control access with it.
//! The server writes one at boot when the file has none ([`generate`],
//! `config::save_request_key`); `tcr request-key` prints it and never writes one,
//! so the CLI and the server cannot hold two different keys.
//!
//! A caller presents it in one of four places, all read by ONE parse
//! ([`ProxyCredential`]):
//!
//! - `Proxy-Authorization: Basic base64(<user>:<key>)` on a `CONNECT`, which is
//!   what `HTTPS_PROXY=http://<user>:<key>@127.0.0.1:<port>` makes a client send.
//!   The username stays the route marker it already was: `tcr-codex` selects
//!   Codex interception ([`crate::mitm::CODEX_PROXY_USERNAME`]), with or without
//!   a key in the password.
//! - [`HEADER`] (`x-tcr-key: <key>`), for a base-URL client, which Claude Code
//!   sends through `ANTHROPIC_CUSTOM_HEADERS`. Its own name, because `x-api-key`
//!   may already carry a per-run placeholder that session affinity reads.
//! - `x-api-key: <key>`, for an SDK client with no login of its own.
//! - `Proxy-Authorization` again, on a plain request through `HTTP_PROXY`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use axum::http::HeaderMap;

/// The header a base-URL client sends the request key in. Stripped wherever a
/// client credential is stripped: the pooled upstream headers, the passthrough
/// relay, and the peer borrower's scrub.
pub const HEADER: &str = "x-tcr-key";

/// Bytes of randomness in a generated key, shown as twice as many hex digits.
const KEY_BYTES: usize = 32;

/// A fresh request key: 32 bytes from the platform CSPRNG, as lowercase hex.
///
/// An error surfaces. A key that is not random is a key anyone can guess, so
/// there is no fallback that is not worse than having none.
pub fn generate() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; KEY_BYTES];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// How a server holding a key treats a caller without it. One variant in this
/// wave, and no config field picks it: refusing is a later change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKeyMode {
    /// Count and log an unkeyed caller once per kind; refuse nothing.
    Log,
}

impl RequestKeyMode {
    /// The wire spelling on `GET /_tcr/status` (`requestKeyMode`). Its PRESENCE
    /// is the contract a launcher reads: "this server understands keys".
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
        }
    }
}

/// What a caller presented, built ONCE at the edge: per `CONNECT` head in
/// `mitm::handle_connect`, per plain request in `proxy::handle_request`. Every
/// consumer reads this value; nothing re-parses the headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyCredential {
    /// The `Proxy-Authorization` username is `tcr-codex`, whatever the password.
    pub codex_marked: bool,
    /// One of the places this module's doc lists carries the server's key.
    /// Always `false` when the server holds none.
    pub key_ok: bool,
    /// The caller's `User-Agent`, folded to a bounded family. Read in the same
    /// pass because the unkeyed counters split by it.
    pub user_agent: UaFamily,
}

impl ProxyCredential {
    /// The credential on a `CONNECT` request head (request line, headers, blank
    /// line). The request line is skipped; parsing stops at the blank line.
    pub fn from_connect_head(head: &str, request_key: Option<&str>) -> Self {
        Self::from_pairs(
            head.lines()
                .skip(1)
                .take_while(|line| !line.is_empty())
                .filter_map(|line| line.split_once(':')),
            request_key,
        )
    }

    /// The credential on a plain (base-URL or absolute-form) request.
    pub fn from_request_headers(headers: &HeaderMap, request_key: Option<&str>) -> Self {
        Self::from_pairs(
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
            request_key,
        )
    }

    fn from_pairs<'a>(
        pairs: impl Iterator<Item = (&'a str, &'a str)>,
        request_key: Option<&str>,
    ) -> Self {
        let mut credential = Self {
            codex_marked: false,
            key_ok: false,
            user_agent: UaFamily::Missing,
        };
        for (name, value) in pairs {
            let value = value.trim();
            if name.eq_ignore_ascii_case("proxy-authorization") {
                let Some((user, password)) = basic_user_password(value) else {
                    continue;
                };
                credential.codex_marked |= user == crate::mitm::CODEX_PROXY_USERNAME;
                credential.key_ok |= key_ok(Some(&password), request_key);
            } else if name.eq_ignore_ascii_case(HEADER) || name.eq_ignore_ascii_case("x-api-key") {
                credential.key_ok |= key_ok(Some(value), request_key);
            } else if name.eq_ignore_ascii_case("user-agent") {
                credential.user_agent = UaFamily::of(value);
            }
        }
        credential
    }
}

/// Does `provided` match the server's key? A server with no key matches nothing,
/// and an empty key is never one (a hand-edited `"requestKey": ""` must not make
/// every caller with an empty password keyed).
fn key_ok(provided: Option<&str>, request_key: Option<&str>) -> bool {
    request_key
        .filter(|key| !key.is_empty())
        .is_some_and(|key| crate::proxy::key_matches(provided, key))
}

/// The `(user, password)` of a `Basic` credential (RFC 7617), scheme matched
/// case-insensitively. `None` for any other scheme, a value that is not base64,
/// not UTF-8, or has no `:`, since a user-pass always has one: `tcr-codex:` is
/// the marker, a bare `tcr-codex` is not.
fn basic_user_password(value: &str) -> Option<(String, String)> {
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    // The standard alphabet, read through the URL-safe decoder the crate already
    // has: the two alphabets differ only in these two characters.
    let url_safe: String = encoded
        .trim()
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect();
    let decoded = String::from_utf8(crate::codex_oauth::base64url_decode(&url_safe)?).ok()?;
    let (user, password) = decoded.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

/// A caller's `User-Agent`, folded to a bounded set so the counters cannot grow
/// with whatever a client sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UaFamily {
    /// Claude Code: `claude-cli/<version> (...)`.
    ClaudeCli,
    /// The Codex CLI: `codex_cli_rs/...`, `codex_exec/...` and the like.
    Codex,
    /// Bun's own fetch, which Claude Code's `HEAD /api/hello` carries.
    Bun,
    Curl,
    /// Python's HTTP clients and the Anthropic Python SDK.
    Python,
    /// Node's HTTP clients and the Anthropic TypeScript SDK.
    Node,
    /// Anything else that sent a `User-Agent`.
    Other,
    /// No `User-Agent` at all.
    Missing,
}

impl UaFamily {
    pub fn of(user_agent: &str) -> Self {
        let ua = user_agent.trim().to_ascii_lowercase();
        if ua.is_empty() {
            Self::Missing
        } else if ua.starts_with("claude-cli/") {
            Self::ClaudeCli
        } else if ua.starts_with("codex") {
            Self::Codex
        } else if ua.starts_with("bun/") {
            Self::Bun
        } else if ua.starts_with("curl/") {
            Self::Curl
        } else if ua.contains("python") {
            Self::Python
        } else if ua.starts_with("node") || ua.contains("anthropic/js") || ua.starts_with("undici")
        {
            Self::Node
        } else {
            Self::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCli => "claude-cli",
            Self::Codex => "codex",
            Self::Bun => "bun",
            Self::Curl => "curl",
            Self::Python => "python",
            Self::Node => "node",
            Self::Other => "other",
            Self::Missing => "none",
        }
    }
}

/// Which host an unkeyed `CONNECT` asked for: the three tcr intercepts, or any
/// other (a blind tunnel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConnectClass {
    AnthropicApi,
    ClaudePlatform,
    Chatgpt,
    Other,
}

impl ConnectClass {
    pub fn of(host: &str) -> Self {
        if host.eq_ignore_ascii_case("api.anthropic.com") {
            Self::AnthropicApi
        } else if host.eq_ignore_ascii_case("platform.claude.com") {
            Self::ClaudePlatform
        } else if host.eq_ignore_ascii_case(crate::provider::CODEX_HOST) {
            Self::Chatgpt
        } else {
            Self::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicApi => "api.anthropic.com",
            Self::ClaudePlatform => "platform.claude.com",
            Self::Chatgpt => "chatgpt.com",
            Self::Other => "other",
        }
    }
}

/// Which path an unkeyed plain request asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PathClass {
    /// `/v1/messages` and everything under it (`count_tokens`, batches).
    Messages,
    /// Any other `/v1/` path (`/v1/models`, ...).
    V1Other,
    /// A Codex path ([`crate::provider::Provider::of_path`]).
    Codex,
    /// `/api/...`, where Claude Code's unkeyed `HEAD /api/hello` lands.
    Api,
    Other,
}

impl PathClass {
    pub fn of(path: &str) -> Self {
        if crate::provider::Provider::of_path(path) == crate::provider::Provider::Codex {
            Self::Codex
        } else if path == "/v1/messages" || path.starts_with("/v1/messages/") {
            Self::Messages
        } else if path.starts_with("/v1/") {
            Self::V1Other
        } else if path.starts_with("/api/") {
            Self::Api
        } else {
            Self::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::V1Other => "v1-other",
            Self::Codex => "codex",
            Self::Api => "api",
            Self::Other => "other",
        }
    }
}

/// One unkeyed arrival: a tunnel by its host, or a plain request by its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unkeyed {
    Connect(ConnectClass),
    Request(PathClass),
}

impl Unkeyed {
    fn kind(self) -> &'static str {
        match self {
            Self::Connect(_) => "connect",
            Self::Request(_) => "request",
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Connect(class) => class.as_str(),
            Self::Request(class) => class.as_str(),
        }
    }
}

/// How a plain request stands, decided once in `proxy::handle_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKeying {
    /// The server holds no key, so there is nothing to check.
    NoServerKey,
    /// Decrypted from a tunnel whose `CONNECT` carried the key
    /// (`proxy::TunnelKeyed`), or a plain request that carried it.
    Keyed,
    /// Decrypted from a tunnel whose `CONNECT` did not carry the key. Counted
    /// once, at that `CONNECT`, and not again per request.
    CountedAtConnect,
    /// A plain request without the key.
    Unkeyed(UaFamily),
}

/// The unkeyed counters `GET /_tcr/status` reports, split by kind, class and
/// user-agent family. Every dimension is a closed enum, so the map is bounded.
///
/// Only an unkeyed arrival touches the lock; a keyed one never reaches here.
#[derive(Debug, Default)]
pub struct UnkeyedCounters {
    counts: Mutex<BTreeMap<(Unkeyed, UaFamily), u64>>,
}

/// One row of [`UnkeyedCounters::snapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnkeyedRow {
    pub what: Unkeyed,
    pub user_agent: UaFamily,
    pub count: u64,
}

impl UnkeyedCounters {
    /// Count one unkeyed arrival. The first of each (kind, class, family) logs
    /// one INFO line with the caller's port, so `lsof -nP -iTCP:<port>` can name
    /// the process while it is still connected. Never a line per request
    /// (Gil, 2026-10-15).
    pub fn record(&self, what: Unkeyed, user_agent: UaFamily, peer_port: u16) {
        let first = {
            let Ok(mut counts) = self.counts.lock() else {
                tracing::warn!("request key: the unkeyed counters' lock is poisoned; not counted");
                return;
            };
            let count = counts.entry((what, user_agent)).or_insert(0);
            *count += 1;
            *count == 1
        };
        if first {
            tracing::info!(
                kind = what.kind(),
                class = what.class(),
                user_agent = user_agent.as_str(),
                peer_port,
                "request key: first unkeyed caller of this kind (log mode, nothing refused)"
            );
        }
    }

    /// Every non-zero count, in a stable order.
    pub fn snapshot(&self) -> Vec<UnkeyedRow> {
        let Ok(counts) = self.counts.lock() else {
            tracing::warn!("request key: the unkeyed counters' lock is poisoned; reporting none");
            return Vec::new();
        };
        counts
            .iter()
            .map(|(&(what, user_agent), &count)| UnkeyedRow {
                what,
                user_agent,
                count,
            })
            .collect()
    }

    /// The `unkeyedConnects` and `unkeyedRequests` wire rows.
    pub fn wire(
        &self,
    ) -> (
        Vec<crate::status::UnkeyedCount>,
        Vec<crate::status::UnkeyedCount>,
    ) {
        let mut connects = Vec::new();
        let mut requests = Vec::new();
        for row in self.snapshot() {
            let wire = crate::status::UnkeyedCount {
                class: row.what.class().to_string(),
                user_agent: row.user_agent.as_str().to_string(),
                count: row.count,
            };
            match row.what {
                Unkeyed::Connect(_) => connects.push(wire),
                Unkeyed::Request(_) => requests.push(wire),
            }
        }
        (connects, requests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn basic(user_pass: &str) -> String {
        // A tiny standard-alphabet encoder for the fixtures only, checked
        // against the known marker value below.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = user_pass.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn connect_head(headers: &str) -> String {
        format!("CONNECT api.anthropic.com:443 HTTP/1.1\r\n{headers}\r\n")
    }

    #[test]
    fn the_fixture_encoder_matches_the_codex_marker() {
        assert_eq!(
            format!("Basic {}", basic("tcr-codex:")),
            crate::mitm::CODEX_PROXY_AUTHORIZATION
        );
    }

    #[test]
    fn a_connect_credential_reads_the_marker_and_the_key_apart() {
        let cases = [
            // (header lines, codex_marked, key_ok)
            (
                format!("Proxy-Authorization: Basic {}\r\n", basic("tcr-codex:")),
                true,
                false,
            ),
            (
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    basic(&format!("tcr-codex:{KEY}"))
                ),
                true,
                true,
            ),
            (
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    basic(&format!("tcr:{KEY}"))
                ),
                false,
                true,
            ),
            (
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    basic(&format!("anyone-at-all:{KEY}"))
                ),
                false,
                true,
            ),
            (
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    basic("tcr:the-wrong-key")
                ),
                false,
                false,
            ),
            (
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    basic("tcr-codex:the-wrong-key")
                ),
                true,
                false,
            ),
            (
                format!(
                    "proxy-authorization: basic {}\r\n",
                    basic(&format!("tcr:{KEY}"))
                ),
                false,
                true,
            ),
            (
                format!(
                    "PROXY-AUTHORIZATION:   BASIC   {}\r\n",
                    basic(&format!("tcr-codex:{KEY}"))
                ),
                true,
                true,
            ),
            (String::new(), false, false),
            ("User-Agent: curl/8.7.1\r\n".to_string(), false, false),
            (
                format!("Authorization: Basic {}\r\n", basic(&format!("tcr:{KEY}"))),
                false,
                false,
            ),
            (
                format!("Proxy-Authorization: Bearer {KEY}\r\n"),
                false,
                false,
            ),
            // A bare user with no `:` is not a user-pass.
            (
                format!("Proxy-Authorization: Basic {}\r\n", basic("tcr-codex")),
                false,
                false,
            ),
        ];
        for (headers, codex_marked, key_ok) in cases {
            let credential = ProxyCredential::from_connect_head(&connect_head(&headers), Some(KEY));
            assert_eq!(
                (credential.codex_marked, credential.key_ok),
                (codex_marked, key_ok),
                "{headers:?}"
            );
        }
    }

    #[test]
    fn with_no_server_key_nothing_is_keyed_and_the_marker_still_reads() {
        let head = connect_head(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            basic(&format!("tcr-codex:{KEY}"))
        ));
        let credential = ProxyCredential::from_connect_head(&head, None);
        assert!(credential.codex_marked);
        assert!(!credential.key_ok);
        // An empty configured key is no key: an empty password must not match it.
        let empty = connect_head(&format!("Proxy-Authorization: Basic {}\r\n", basic("tcr:")));
        assert!(!ProxyCredential::from_connect_head(&empty, Some("")).key_ok);
    }

    #[test]
    fn a_plain_request_is_keyed_by_any_of_three_headers() {
        let keyed = |name: &str, value: String| {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
            ProxyCredential::from_request_headers(&headers, Some(KEY)).key_ok
        };
        assert!(keyed("x-tcr-key", KEY.to_string()));
        assert!(keyed("x-api-key", KEY.to_string()));
        assert!(keyed(
            "proxy-authorization",
            format!("Basic {}", basic(&format!("whoever:{KEY}")))
        ));
        assert!(!keyed("x-tcr-key", "not-the-key".to_string()));
        assert!(!keyed("x-api-key", "via-tcr-run-1".to_string()));
        assert!(!keyed("authorization", format!("Bearer {KEY}")));
        assert!(!ProxyCredential::from_request_headers(&HeaderMap::new(), Some(KEY)).key_ok);
    }

    #[test]
    fn the_user_agent_folds_to_a_bounded_family() {
        for (ua, family) in [
            ("claude-cli/2.1.301 (external, cli)", UaFamily::ClaudeCli),
            (
                "codex_cli_rs/0.50.0 (Mac OS 26.0.0; arm64)",
                UaFamily::Codex,
            ),
            ("Bun/1.4.3", UaFamily::Bun),
            ("curl/8.7.1", UaFamily::Curl),
            ("python-httpx/0.28.1", UaFamily::Python),
            ("Anthropic/Python 0.70.0", UaFamily::Python),
            ("node-fetch/1.0", UaFamily::Node),
            ("Mozilla/5.0", UaFamily::Other),
            ("", UaFamily::Missing),
        ] {
            assert_eq!(UaFamily::of(ua), family, "{ua}");
        }
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", "curl/8.7.1".parse().unwrap());
        assert_eq!(
            ProxyCredential::from_request_headers(&headers, Some(KEY)).user_agent,
            UaFamily::Curl
        );
        assert_eq!(
            ProxyCredential::from_request_headers(&HeaderMap::new(), Some(KEY)).user_agent,
            UaFamily::Missing
        );
    }

    #[test]
    fn a_generated_key_is_64_hex_digits_and_not_repeated() {
        let a = generate().expect("the CSPRNG answers");
        let b = generate().expect("the CSPRNG answers");
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }

    #[test]
    fn the_counters_split_by_kind_class_and_family() {
        let counters = UnkeyedCounters::default();
        counters.record(
            Unkeyed::Connect(ConnectClass::Other),
            UaFamily::Curl,
            50_001,
        );
        counters.record(
            Unkeyed::Connect(ConnectClass::Other),
            UaFamily::Curl,
            50_002,
        );
        counters.record(
            Unkeyed::Request(PathClass::of("/v1/messages")),
            UaFamily::Missing,
            50_003,
        );
        let (connects, requests) = counters.wire();
        assert_eq!(
            connects,
            vec![crate::status::UnkeyedCount {
                class: "other".to_string(),
                user_agent: "curl".to_string(),
                count: 2,
            }]
        );
        assert_eq!(
            requests,
            vec![crate::status::UnkeyedCount {
                class: "messages".to_string(),
                user_agent: "none".to_string(),
                count: 1,
            }]
        );
    }

    #[test]
    fn path_and_host_classes() {
        assert_eq!(PathClass::of("/v1/messages"), PathClass::Messages);
        assert_eq!(
            PathClass::of("/v1/messages/count_tokens"),
            PathClass::Messages
        );
        assert_eq!(PathClass::of("/v1/messagesx"), PathClass::V1Other);
        assert_eq!(PathClass::of("/v1/models"), PathClass::V1Other);
        assert_eq!(PathClass::of("/api/hello"), PathClass::Api);
        assert_eq!(
            PathClass::of("/backend-api/codex/responses"),
            PathClass::Codex
        );
        assert_eq!(PathClass::of("/"), PathClass::Other);
        assert_eq!(
            ConnectClass::of("API.anthropic.com"),
            ConnectClass::AnthropicApi
        );
        assert_eq!(ConnectClass::of("chatgpt.com"), ConnectClass::Chatgpt);
        assert_eq!(ConnectClass::of("example.com"), ConnectClass::Other);
    }
}
