//! MITM / forward-proxy "backwards mode": a hybrid listener that serves the
//! existing base-URL proxy **and** terminates `HTTPS_PROXY`-style `CONNECT`
//! tunnels on the same port, so a client with `HTTPS_PROXY=http://127.0.0.1:<port>`
//! is a drop-in against `tcr`.
//!
//! How a request flows (mirrors `teamclaude/src/mitm.js`, reusing `tcr`'s
//! account-selection + token-injection instead of reimplementing the forward):
//!   1. A client sends `CONNECT <host>:<port>`. We peek the request line.
//!   2. `api.anthropic.com`, `platform.claude.com` (Claude Code's own OAuth
//!      refresh host, `POST /v1/oauth/token`) and `chatgpt.com` (Codex's
//!      inference and backend host, on a marked Codex connection while the pool holds an account:
//!      `host_intercepted`) are MITM-terminated **only when the leaf
//!      certificate actually loaded for this process covers that host** —
//!      see [`host_allowed`] (policy) and
//!      [`host_covered_by_loaded_leaf`] (SAN truthfulness) below, both of which
//!      must agree. When they do: we reply `200 Connection Established`,
//!      TLS-accept with our leaf (a cert the client already trusts via its
//!      CA), then serve HTTP over the TLS stream through the SAME axum router
//!      as base-URL mode — authenticate, select an account, inject the pooled
//!      `Bearer`, forward to the real upstream.
//!   3. Every OTHER host — and any policy-allowed host the loaded leaf does
//!      NOT cover, and an unmarked `chatgpt.com` connection or one with no pooled account —
//!      is **blind-tunneled**: a raw TCP byte-pipe to
//!      `<host>:<port>` with TLS left untouched (we never see plaintext and
//!      inject nothing). This matches the JS proxy's open tunnel for
//!      everything else Claude Code needs (its bridge, telemetry) through the
//!      same `HTTPS_PROXY`, and it is ALSO the anti-outage fallback for a host
//!      we intend to intercept but whose currently-loaded cert cannot present:
//!      claiming a host and then failing the TLS handshake is strictly worse
//!      than tunneling it untouched. Safe here because `tcr` binds
//!      `127.0.0.1` only — reachable solely by the local user, who can already
//!      open any connection directly, so it is no wider a surface than the
//!      shell itself.
//!
//! The decrypted request arrives in origin form (`POST /v1/messages` with
//! `Host: api.anthropic.com`), so the router routes it to `manager.upstream()`
//! verbatim — no proxy handler change is needed. The outbound client keeps
//! `.no_proxy()` (set in [`crate::manager`]) so we never loop our own upstream
//! back through an ambient `HTTPS_PROXY`.
//!
//! Codex connections opt in with `HTTPS_PROXY=http://tcr-codex@127.0.0.1:<port>`.
//! Codex's HTTP client passes proxy URL userinfo to reqwest; its WebSocket client enables
//! the pinned Tungstenite forks' proxy support (`openai/codex` `http-client/src/outbound_proxy.rs`,
//! `websocket-client/src/dialer.rs`, `Cargo.toml`). Both send that userinfo as HTTP CONNECT
//! `Proxy-Authorization: Basic dGNyLWNvZGV4Og==`. The pinned Tungstenite `proxy.rs`
//! parses a username without a password and builds the same Basic header as reqwest's
//! hyper-util matcher. This permits one listener without terminating another client's
//! `chatgpt.com` TLS. The marker selects routing; it is not an authentication secret.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt as _;
use tracing::instrument::WithSubscriber as _;

use crate::manager::Manager;

/// Hosts we intend to MITM-terminate + inject tokens for — a POLICY list, not
/// a firewall (everything else on `CONNECT` is blind-tunneled, no
/// interception). This is necessary but not sufficient for interception:
/// [`handle_connect`] additionally requires [`host_covered_by_loaded_leaf`],
/// because being on this list says nothing about whether the leaf actually
/// loaded for this process can present a valid certificate for the host — see
/// the module doc comment and [`load_tls_in`]'s design comment.
///
/// `console.anthropic.com` and `platform.anthropic.com` were removed 2026-08:
/// measured zero references in three consecutive Claude Code releases
/// (2.1.232-234) against the shipped binary, i.e. dead. `platform.claude.com`
/// was added in their place: it is the host Claude Code's own OAuth refresh
/// (`POST /v1/oauth/token`) targets — 2 occurrences in the same binary — and
/// `src/proxy.rs`'s `RelayMode::Raw` handling for that path was unreachable in
/// MITM mode without it.
///
/// `chatgpt.com` was added 2026-10 for Codex, whose inference and backend
/// calls go there by default (`openai/codex` `model-provider-info/src/lib.rs:80`,
/// `CHATGPT_CODEX_BASE_URL`). It is intercepted only on a marked Codex connection while the pool
/// holds a Codex account (`host_intercepted`), so an unmarked client or a codex with none keeps
/// its own login. A login whose workspace is routed to another backend origin
/// sends its calls there instead, to a host not on this list; tcr pools no such
/// workspace, and relays a request that names a routing of its own on the
/// client's credential (`crate::provider::CODEX_ROUTING_HEADERS`).
/// `auth.openai.com` is deliberately NOT here: it is where the Codex client
/// refreshes its OWN token, and that exchange must reach the real host
/// untouched — tcr has nothing to inject into it, and Codex refresh tokens are
/// single-use, so a refresh tcr mishandled would cost the client its login.
/// Off this list it is blind-tunneled.
///
/// Peer egress has its own narrower list ([`crate::peer::egress::PEER_EGRESS_HOSTS`]);
/// adding a local interception host grants no LAN gateway access to it.
pub const ALLOWED_HOSTS: &[&str] = &["api.anthropic.com", "platform.claude.com", "chatgpt.com"];

/// Proxy URL userinfo set by `tcr run --codex`, selecting Codex interception on CONNECT.
pub const CODEX_PROXY_USERNAME: &str = "tcr-codex";
/// Basic encoding of `tcr-codex:` sent by both Codex transports, including an empty password.
pub const CODEX_PROXY_AUTHORIZATION: &str = "Basic dGNyLWNvZGV4Og==";

/// Does a proxy URL opt in to Codex interception? Used by the doctor without displaying userinfo.
pub fn codex_proxy_marked(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.username() == CODEX_PROXY_USERNAME && url.password().is_none_or(str::is_empty)
    })
}

/// Read the marker from CONNECT headers only, without exposing or forwarding proxy credentials.
fn codex_connect_marked(head: &str) -> bool {
    head.lines()
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("proxy-authorization")
                && value.trim().split_once(' ').is_some_and(|(scheme, value)| {
                    scheme.eq_ignore_ascii_case("Basic") && value.trim() == "dGNyLWNvZGV4Og=="
                })
        })
}

/// SAN list for the rcgen fallback leaf (the policy allowlist + the JS test
/// host, kept so a regenerated chain still answers the credential-free
/// `www.example.org` probe). This is also the REQUIRED coverage set: a
/// persisted leaf whose sidecar (see [`load_tls_in`]) does not match this
/// exactly is treated as stale and re-minted.
///
/// That makes adding a host here, as `chatgpt.com` was in 2026-10, the whole
/// upgrade path for an install serving a leaf tcr minted: its sidecar stops
/// matching, so on the next start [`load_tls_in`] takes its "persisted leaf
/// SAN coverage is stale" branch and [`remint_leaf_under_persisted_ca`] signs
/// a fresh leaf under the CA key persisted beside it. The CA its clients
/// already trust stays valid, so nobody re-trusts anything. (A chain minted
/// before `tcr-ca.key` existed rotates its CA once instead; see
/// [`MintedPaths::ca_key`].)
const FALLBACK_LEAF_SANS: &[&str] = &[
    "api.anthropic.com",
    "platform.claude.com",
    "chatgpt.com",
    "www.example.org",
];

/// Process-wide snapshot of the SAN coverage of whatever leaf [`load_tls`]
/// most recently loaded for this process, populated by [`load_tls_in`] as a
/// side effect on every successful return. This exists ONLY because the
/// `tls: Option<Arc<TlsAcceptor>>` threaded from `server.rs` through
/// `serve`/`serve_with_shutdown`/`handle_conn`/`handle_connect` carries just
/// the acceptor, not the `TlsAssets` it came from — so [`handle_connect`],
/// which has no other way to reach the coverage of the leaf it is about to
/// present, reads it here via [`host_covered_by_loaded_leaf`]. The lib tests
/// read `TlsAssets::covered_hosts` off the returned value instead; a test that
/// opens a tunnel does rely on this global, which is why
/// `tests/intercepted_host.rs` is its own binary. Starts empty — "nothing
/// loaded yet" — which is the safe default: under-claiming coverage means
/// blind-tunnel, never a broken handshake.
static COVERED_HOSTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn set_covered_hosts(hosts: &[String]) {
    if let Ok(mut guard) = COVERED_HOSTS.lock() {
        *guard = hosts.to_vec();
    }
}

/// Is `host` present (case-insensitively) in `covered`, the SAN set of one
/// particular loaded leaf? Pure and parameterized so tests can assert against
/// [`TlsAssets::covered_hosts`] directly, without touching the process-wide
/// snapshot [`host_covered_by_loaded_leaf`] reads from in production.
fn leaf_covers(covered: &[String], host: &str) -> bool {
    covered.iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// Is `host` covered by the SAN set of the leaf actually loaded for THIS
/// process, per the most recent [`load_tls`]/[`load_tls_in`] call? This is
/// independent of [`host_allowed`]'s policy question — a host can be
/// policy-allowed and still fail this, e.g. right after a SAN change and
/// before a re-mint completes, or when the reused JS leaf's coverage is
/// unknown (see the conservative branch-A default in [`load_tls_in`]).
fn host_covered_by_loaded_leaf(host: &str) -> bool {
    COVERED_HOSTS
        .lock()
        .is_ok_and(|hosts| leaf_covers(&hosts, host))
}

const RESP_CONNECT_OK: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const RESP_BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_UNAVAILABLE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Loaded TLS material plus the CA path to advertise via `NODE_EXTRA_CA_CERTS`.
pub struct TlsAssets {
    pub acceptor: TlsAcceptor,
    /// The CA cert clients should trust to accept [`Self::acceptor`]'s leaf.
    /// `None` only when we cannot name one — a reused leaf with no companion CA
    /// beside it. Callers that must *tell* a client what to trust (see-through
    /// mode in `tcr run`) treat `None` as "cannot do that", not as "no CA needed":
    /// an unnameable CA may still be trusted ambiently, but we cannot prove it.
    pub ca_path: Option<PathBuf>,
    /// SAN coverage of [`Self::acceptor`]'s leaf, as actually loaded — not
    /// derived from [`ALLOWED_HOSTS`] or [`FALLBACK_LEAF_SANS`], because which
    /// branch [`load_tls_in`] took determines what this really is (see its
    /// design comment). This is the field that makes the CONNECT interception
    /// decision SAN-truthful: [`host_covered_by_loaded_leaf`] reads a
    /// process-wide snapshot of it.
    pub covered_hosts: Vec<String>,
    /// Which `load_tls_in` branch produced [`Self::acceptor`]'s leaf. The
    /// coverage above says WHAT is intercepted; this says WHY, which is the
    /// half `tcr run --codex` needs when `chatgpt.com` is missing: a restart
    /// re-mints a stale minted leaf, and never changes the JS one.
    pub leaf: LeafSource,
}

/// Where a loaded leaf came from, one variant per kind of `load_tls_in`
/// branch. Carried to clients on the status wire as
/// [`crate::status::MitmStatus::leaf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafSource {
    /// Branch A: the JS proxy's `teamclaude-leaf.pem`. Its coverage is pinned
    /// to `api.anthropic.com` (unknowable without an x509 parser) and tcr
    /// never re-mints it, so no restart widens what it intercepts.
    Js,
    /// Branches B, C and D: a chain tcr minted under `tcr-ca.pem`
    /// ([`minted_ca_path`]), covering `FALLBACK_LEAF_SANS` by construction
    /// and re-minted at boot whenever that set changes.
    Minted,
}

impl TlsAssets {
    /// The hosts `handle_connect` can terminate with this leaf: every
    /// [`ALLOWED_HOSTS`] entry the leaf covers. That is
    /// `host_allowed && host_covered_by_loaded_leaf`, evaluated over every host
    /// the policy names, read off this value rather than the process-wide
    /// `COVERED_HOSTS`. `handle_connect` also requires a marker and a pooled Codex
    /// account for `chatgpt.com` (`host_intercepted`), checked on every CONNECT
    /// independently of this leaf; so `chatgpt.com`
    /// is listed here whether or not the pool holds one. Raw
    /// [`Self::covered_hosts`] would over-report: a minted leaf also carries
    /// `www.example.org`, which is never intercepted.
    pub fn intercepted_hosts(&self) -> Vec<String> {
        ALLOWED_HOSTS
            .iter()
            .filter(|host| leaf_covers(&self.covered_hosts, host))
            .map(|host| (*host).to_string())
            .collect()
    }
}

/// Is `host` one we intend to MITM-terminate, by POLICY? Case-insensitive.
///
/// This alone does not mean we WILL intercept `host` — the interception
/// decision in [`handle_connect`] additionally requires
/// [`host_covered_by_loaded_leaf`], because this function says nothing about
/// whether the leaf loaded for this process can actually present a valid
/// certificate for it. Kept as a standalone, TLS-independent check because
/// `src/proxy.rs`'s base-URL-mode misroute guard also calls it, on a plain-HTTP
/// path where no TLS handshake — and therefore no SAN coverage question —
/// exists at all.
pub fn host_allowed(host: &str) -> bool {
    ALLOWED_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// Is `host` one [`handle_connect`] intercepts NOW, by policy? [`host_allowed`], plus one
/// conditions that belong to `chatgpt.com` ([`crate::provider::CODEX_HOST`]) alone: the CONNECT
/// carries the Codex marker and the pool holds a Codex account (`codex_accounts`, which
/// [`Manager::provider_account_count`] counts in any state, leaving out a Codex account deleted
/// while the server runs). With none, a codex that reached tcr
/// could only be answered `no_codex_account`; blind-tunneled, it runs on its own login, as it
/// did before tcr pooled Codex, with no CA to trust and its WebSocket intact. Asked on every
/// CONNECT, so an account imported into a running server takes effect on the next tunnel.
///
/// Coverage is the separate question [`host_covered_by_loaded_leaf`] answers. Pure, with
/// [`host_allowed`]'s host matching — unit-tested.
fn host_intercepted(host: &str, codex_accounts: usize, codex_connection: bool) -> bool {
    host_allowed(host)
        && (!host.eq_ignore_ascii_case(crate::provider::CODEX_HOST)
            || (codex_accounts > 0 && codex_connection))
}

/// [`host_intercepted`] against `manager`'s Codex accounts, counted on every call and never
/// cached. Only a marked ChatGPT CONNECT needs the count: other tunnels never wait
/// behind the config write lock. Configured but unusable pools remain explicitly pooled;
/// their HTTP requests get an immediate configuration error instead of a retryable hold.
fn intercepts_now(manager: &Manager, host: &str, codex_connection: bool) -> bool {
    if !host.eq_ignore_ascii_case(crate::provider::CODEX_HOST) {
        return host_allowed(host);
    }
    if !codex_connection {
        return false;
    }
    host_intercepted(
        host,
        manager.provider_account_count(crate::provider::Provider::Codex),
        true,
    )
}

/// Parse the target of a `CONNECT host:port HTTP/1.1` request line. Returns the
/// `(host, port)` or `None` if the line is not a well-formed CONNECT.
pub fn parse_connect_target(line: &str) -> Option<(String, u16)> {
    let mut parts = line.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let authority = parts.next()?;
    // `host:port` — split on the LAST colon so a stray colon in the host (never
    // valid here, but be defensive) does not steal the port.
    let (host, port) = authority.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// `~/.config` — the dir holding the drop-in config and the shared certs. Mirrors
/// [`crate::config::default_path`]'s parent so cert reuse lands where the JS proxy
/// (and Gil's already-trusting clients) put them.
fn config_dir() -> PathBuf {
    crate::config::default_path()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Where tcr's own CA lives (`~/.config/tcr-ca.pem`): the CA every
/// [`LeafSource::Minted`] leaf chains to, and one that stays put across leaf
/// re-mints now that its key is persisted beside it (branch D).
///
/// A path, not a load: no IO at all. A CLIENT names the running proxy's CA
/// with this, never with [`load_tls`], which re-mints a stale leaf on disk
/// while the proxy keeps serving the leaf it loaded at boot (nothing reloads
/// it) and so reports coverage that proxy does not have.
pub fn minted_ca_path() -> PathBuf {
    MintedPaths::in_dir(&config_dir()).ca
}

/// Ensure a process-wide rustls crypto provider is installed before building a
/// `ServerConfig`. Two providers may be compiled (ours + reqwest's), which makes
/// `ServerConfig::builder()` panic when no default is set; installing one first
/// (ignoring "already installed") makes the builder deterministic.
fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The CA + leaf tcr mints for itself. Named in exactly one place so the loader
/// and the writer cannot drift apart (they did: the loader probed `teamclaude-*`
/// while the writer produced `tcr-*`, so a minted chain was never read back and
/// every restart re-minted the CA out from under `NODE_EXTRA_CA_CERTS`).
struct MintedPaths {
    ca: PathBuf,
    /// The CA's private key — persisted (mode `0600`) so a future leaf SAN
    /// change re-mints only the leaf under the SAME CA, never rotating
    /// `NODE_EXTRA_CA_CERTS` out from under a client that already trusts it.
    /// Absent on any chain minted before this field existed; that absence is
    /// exactly what forces one unavoidable CA rotation the first time this
    /// runs against an older-minted chain (see [`load_tls_in`]).
    ca_key: PathBuf,
    leaf_cert: PathBuf,
    leaf_key: PathBuf,
    /// One host per line, the SAN set [`leaf_cert`](Self::leaf_cert) was
    /// minted with. Not a certificate field we could read back — a sidecar,
    /// deliberately, so drift detection needs no x509 parser: see the design
    /// comment on [`load_tls_in`] for why an x509 parser was ruled out (no new
    /// dependency, and the parse-then-compare shape is strictly more code and
    /// more attack surface than a one-host-per-line text file we wrote
    /// ourselves). A missing sidecar means "minted before this change", which
    /// is treated the same as a mismatch: coverage unknown → drifted →
    /// re-mint.
    sans: PathBuf,
}

impl MintedPaths {
    fn in_dir(dir: &Path) -> Self {
        Self {
            ca: dir.join("tcr-ca.pem"),
            ca_key: dir.join("tcr-ca.key"),
            leaf_cert: dir.join("tcr-leaf.pem"),
            leaf_key: dir.join("tcr-leaf.key"),
            sans: dir.join("tcr-leaf.sans"),
        }
    }

    fn all_present(&self) -> bool {
        self.ca.is_file() && self.leaf_cert.is_file() && self.leaf_key.is_file()
    }
}

fn fallback_hosts_vec() -> Vec<String> {
    FALLBACK_LEAF_SANS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Write `hosts`, one per line, to `path` (mode `0644` — not secret, just a
/// coverage record).
fn write_sans_sidecar(path: &Path, hosts: &[&str]) -> io::Result<()> {
    let mut content = String::new();
    for host in hosts {
        content.push_str(host);
        content.push('\n');
    }
    write_file(path, content.as_bytes(), 0o644)
}

/// Read a SAN sidecar written by [`write_sans_sidecar`]. `None` on any read
/// failure (including "does not exist") — callers treat that identically to a
/// present-but-mismatched sidecar: coverage unknown, so stale.
fn read_sans_sidecar(path: &Path) -> Option<Vec<String>> {
    let data = std::fs::read_to_string(path).ok()?;
    Some(
        data.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Set-equality (case-insensitive, order-independent) between a sidecar's
/// recorded hosts and the currently `required` coverage.
fn sans_match(sidecar: &[String], required: &[&str]) -> bool {
    if sidecar.len() != required.len() {
        return false;
    }
    let mut have: Vec<String> = sidecar.iter().map(|h| h.to_ascii_lowercase()).collect();
    let mut want: Vec<String> = required.iter().map(|h| h.to_ascii_lowercase()).collect();
    have.sort();
    want.sort();
    have == want
}

/// Load the TLS material for MITM: reuse `~/.config/teamclaude-leaf.pem` +
/// `.key` if present and loadable (the primary path — a cert Gil's clients
/// already trust), else reuse the chain we minted on an earlier run, else mint a
/// fresh CA+leaf with rcgen and persist it.
///
/// `holds_codex` is whether the config holds a Codex account. It changes only
/// what a load says: once one is configured, the fresh-CA hint and the re-mint
/// line name `CODEX_CA_CERTIFICATE` beside `NODE_EXTRA_CA_CERTS`
/// ([`ca_export_hint`], [`remint_note`]), because codex reads its extra CA from
/// that variable alone. Without one, both read as they always did.
pub fn load_tls(holds_codex: bool) -> anyhow::Result<TlsAssets> {
    load_tls_in_with(&config_dir(), holds_codex)
}

/// [`load_tls`] against an explicit config dir: for tests, the integration tests that run the
/// real listener over a leaf in a temp dir among them.
///
/// Every successful call also replaces the process-wide coverage (`COVERED_HOSTS`) that
/// `handle_connect` reads for every listener in this process, whatever acceptor that listener
/// was given. Loading the JS leaf narrows it to `api.anthropic.com`, so a concurrent
/// `chatgpt.com` or `platform.claude.com` tunnel in the same process is blind-tunnelled to the
/// real host. A test that opens tunnels must not share a binary with one that loads the JS leaf
/// (see `tests/intercepted_host.rs`).
///
/// Three branches, in order, and this is the function that makes the whole
/// module SAN-truthful — every return path sets `covered_hosts` to what the
/// chosen leaf can ACTUALLY present, never to the policy list:
///
/// - **A: the JS proxy's `teamclaude-leaf.pem`.** Its SAN coverage is
///   unknowable here without an x509 parser (deliberately not added — see
///   below), so `covered_hosts` is the single conservative entry
///   `api.anthropic.com`: under-claiming means a blind tunnel for
///   `platform.claude.com` (today's safe behavior on this path), while
///   over-claiming would mean presenting this leaf for a host it may not
///   cover, breaking the TLS handshake outright — a live OAuth-refresh
///   outage. Asymmetric costs, so the conservative side is the only side.
/// - **B: our own previously-minted chain.** Reused ONLY when a sidecar file
///   beside it (`tcr-leaf.sans`, one host per line, written by every mint)
///   matches `FALLBACK_LEAF_SANS` exactly. A leaf minted before this SAN set
///   changed carries the OLD coverage — reusing it unconditionally is exactly
///   the bug this change exists to fix (see the module doc comment): it is
///   what let a "just add the host to a const" change silently break the
///   handshake. A sidecar, not a parsed certificate extension, because rcgen's
///   x509-parser feature is a new (transitive) dependency this change is not
///   approved to add, and re-deriving trust from a file we ourselves wrote is
///   simpler and no less trustworthy than parsing DER we ourselves wrote.
///   Mismatch (or a missing sidecar, meaning "minted before this field
///   existed") re-mints the leaf — reusing the persisted CA key when present,
///   so `NODE_EXTRA_CA_CERTS` survives (branch D); a chain minted before the
///   CA key was persisted forces exactly one more CA rotation, never more
///   than one.
/// - **C: mint fresh.** `covered_hosts` is `FALLBACK_LEAF_SANS`, known by
///   construction.
pub fn load_tls_in(dir: &Path) -> anyhow::Result<TlsAssets> {
    load_tls_in_with(dir, false)
}

/// [`load_tls_in`], with [`load_tls`]'s `holds_codex` threaded to the two notices a load can
/// print: the re-mint line and the fresh-CA hint.
fn load_tls_in_with(dir: &Path, holds_codex: bool) -> anyhow::Result<TlsAssets> {
    ensure_crypto_provider();
    let leaf_cert = dir.join(crate::legacy_names::JS_LEAF_CERT);
    let leaf_key = dir.join(crate::legacy_names::JS_LEAF_KEY);

    if leaf_cert.is_file() && leaf_key.is_file() {
        match build_acceptor(&leaf_cert, &leaf_key) {
            Ok(acceptor) => {
                // The JS proxy writes its CA next to the leaf it signs. Report it
                // when it is there: a caller that has to TELL a client what to
                // trust (see-through mode passes it as NODE_EXTRA_CA_CERTS) cannot
                // derive the path itself, and reporting `None` here used to strand
                // it — on the very path Gil is actually on — with the answer
                // sitting one `join` away. Absent → `None`, same as before.
                let companion_ca = dir.join(crate::legacy_names::JS_CA_CERT);
                let ca_path = companion_ca.is_file().then_some(companion_ca);
                // Conservative: this leaf's real SAN set is unknown, so claim
                // only the one host we know every such leaf has always carried.
                let covered_hosts = vec!["api.anthropic.com".to_string()];
                set_covered_hosts(&covered_hosts);
                tracing::info!(
                    cert = %leaf_cert.display(),
                    ca = ca_path.as_ref().map_or_else(String::new, |p| p.display().to_string()),
                    hosts = ?covered_hosts,
                    "MITM: reusing existing leaf certificate (conservative SAN coverage — unknown leaf)"
                );
                return Ok(TlsAssets {
                    acceptor,
                    ca_path,
                    covered_hosts,
                    leaf: LeafSource::Js,
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "MITM: reusing leaf failed — trying our own minted chain");
            }
        }
    }

    // Reuse the chain from an earlier mint, but only when its recorded SAN
    // coverage still matches what we require today — see the branch-B design
    // note above. Without the sidecar check this branch would (and did) serve
    // a stale leaf forever: `MintedPaths::all_present` says nothing about
    // WHAT the leaf covers, only that files exist.
    let minted = MintedPaths::in_dir(dir);
    if minted.all_present() {
        let fresh = read_sans_sidecar(&minted.sans)
            .is_some_and(|sans| sans_match(&sans, FALLBACK_LEAF_SANS));
        if fresh {
            match build_acceptor(&minted.leaf_cert, &minted.leaf_key) {
                Ok(acceptor) => {
                    let covered_hosts = fallback_hosts_vec();
                    set_covered_hosts(&covered_hosts);
                    tracing::info!(cert = %minted.leaf_cert.display(), hosts = ?covered_hosts, "MITM: reusing minted leaf certificate");
                    return Ok(TlsAssets {
                        acceptor,
                        ca_path: Some(minted.ca),
                        covered_hosts,
                        leaf: LeafSource::Minted,
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "MITM: reusing minted leaf failed — re-minting");
                }
            }
        } else {
            tracing::info!(
                cert = %minted.leaf_cert.display(),
                "MITM: persisted leaf SAN coverage is stale (or from before this field existed) — re-minting"
            );
        }

        if let Some(assets) = remint_leaf_under_persisted_ca(&minted, holds_codex) {
            return Ok(assets);
        }
    }

    generate_and_persist(dir, holds_codex)
}

/// Branch D: sign a fresh leaf under the CA key already persisted beside
/// `minted`, when both the CA cert and CA key are on disk. `None` when there
/// is no persisted CA key to reuse (a chain minted before this field existed,
/// or any read/sign failure) — the caller falls through to
/// [`generate_and_persist`], which mints a brand-new CA. That fallback is the
/// one unavoidable CA rotation described on [`MintedPaths::ca_key`]; after it,
/// this branch is what keeps every later SAN change from rotating the CA
/// again. `holds_codex` words its log line ([`remint_note`]).
fn remint_leaf_under_persisted_ca(minted: &MintedPaths, holds_codex: bool) -> Option<TlsAssets> {
    if !(minted.ca.is_file() && minted.ca_key.is_file()) {
        return None;
    }
    let result = (|| -> anyhow::Result<TlsAssets> {
        let ca_key_pem = std::fs::read_to_string(&minted.ca_key)?;
        let ca = CaChain {
            cert_pem: None,
            key_pem: ca_key_pem,
        };
        let (leaf_pem, leaf_key_pem) = sign_leaf(FALLBACK_LEAF_SANS, &ca)?;
        write_file(&minted.leaf_cert, leaf_pem.as_bytes(), 0o644)?;
        write_file(&minted.leaf_key, leaf_key_pem.as_bytes(), 0o600)?;
        write_sans_sidecar(&minted.sans, FALLBACK_LEAF_SANS)?;
        let acceptor = build_acceptor(&minted.leaf_cert, &minted.leaf_key)?;
        let covered_hosts = fallback_hosts_vec();
        set_covered_hosts(&covered_hosts);
        tracing::info!(
            cert = %minted.leaf_cert.display(),
            hosts = ?covered_hosts,
            "{}",
            remint_note(holds_codex)
        );
        Ok(TlsAssets {
            acceptor,
            ca_path: Some(minted.ca.clone()),
            covered_hosts,
            leaf: LeafSource::Minted,
        })
    })();
    match result {
        Ok(assets) => Some(assets),
        Err(err) => {
            tracing::warn!(error = %err, "MITM: re-mint under persisted CA failed — minting a fresh CA instead");
            None
        }
    }
}

/// Build a rustls `TlsAcceptor` presenting the leaf at `cert_path`/`key_path`.
fn build_acceptor(cert_path: &Path, key_path: &Path) -> anyhow::Result<TlsAcceptor> {
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn load_certs(path: &Path) -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let data = std::fs::read(path)?;
    let mut reader = std::io::BufReader::new(&data[..]);
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", path.display());
    }
    Ok(certs)
}

fn load_key(path: &Path) -> anyhow::Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let data = std::fs::read(path)?;
    let mut reader = std::io::BufReader::new(&data[..]);
    rustls_pemfile::private_key(&mut reader)?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", path.display()))
}

/// rcgen fallback: mint a fresh CA + a leaf covering [`FALLBACK_LEAF_SANS`],
/// persist them to `~/.config/tcr-{ca,leaf}.pem` + `tcr-ca.key` + `tcr-leaf.key`
/// (both keys `0600`) plus the `tcr-leaf.sans` coverage sidecar, and hand back
/// an acceptor over the fresh leaf. The CA path is advertised so the user can
/// `export NODE_EXTRA_CA_CERTS=<ca>`, and `CODEX_CA_CERTIFICATE=<ca>` too once
/// the config holds a Codex account (`holds_codex`, [`ca_export_hint`]).
///
/// Only ever mints a NEW CA — reusing an existing one is
/// [`remint_leaf_under_persisted_ca`]'s job, tried first by [`load_tls_in`].
/// This is the path that used to re-mint (and thus rotate) the CA on every
/// SAN change; persisting `tcr-ca.key` here is what makes that a one-time
/// cost instead of a recurring one.
fn generate_and_persist(dir: &Path, holds_codex: bool) -> anyhow::Result<TlsAssets> {
    let ca = generate_ca()?;
    let ca_cert_pem = ca
        .cert_pem
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("generate_ca must always mint a cert_pem"))?;
    let (leaf_pem, leaf_key_pem) = sign_leaf(FALLBACK_LEAF_SANS, &ca)?;

    std::fs::create_dir_all(dir)?;
    let minted = MintedPaths::in_dir(dir);
    write_file(&minted.ca, ca_cert_pem.as_bytes(), 0o644)?;
    write_file(&minted.ca_key, ca.key_pem.as_bytes(), 0o600)?;
    write_file(&minted.leaf_cert, leaf_pem.as_bytes(), 0o644)?;
    write_file(&minted.leaf_key, leaf_key_pem.as_bytes(), 0o600)?;
    write_sans_sidecar(&minted.sans, FALLBACK_LEAF_SANS)?;

    let acceptor = build_acceptor(&minted.leaf_cert, &minted.leaf_key)?;
    let covered_hosts = fallback_hosts_vec();
    set_covered_hosts(&covered_hosts);
    eprintln!("{}", ca_export_hint(&minted.ca, holds_codex));
    tracing::info!(hosts = ?covered_hosts, "MITM: minted a fresh CA + leaf");
    Ok(TlsAssets {
        acceptor,
        ca_path: Some(minted.ca),
        covered_hosts,
        leaf: LeafSource::Minted,
    })
}

/// What tcr prints when it mints a fresh CA: the export that makes a client trust it.
/// `NODE_EXTRA_CA_CERTS` is the variable claude (node) reads; once the config holds a Codex
/// account (`holds_codex`) the line also names `CODEX_CA_CERTIFICATE`, the one codex reads
/// (`openai/codex` f82e365, `http-client/src/custom_ca.rs:385-409`), so a codex reaching this
/// proxy some other way than `tcr run --codex` can trust the leaf too.
pub fn ca_export_hint(ca: &Path, holds_codex: bool) -> String {
    let codex = if holds_codex {
        format!(" CODEX_CA_CERTIFICATE={}", ca.display())
    } else {
        String::new()
    };
    format!(
        "[tcr] minted a fresh MITM CA — trust it with:\n         export NODE_EXTRA_CA_CERTS={}{codex}",
        ca.display()
    )
}

/// The log line for a leaf re-minted under the persisted CA, which keeps every exported CA
/// variable valid: `NODE_EXTRA_CA_CERTS`, and `CODEX_CA_CERTIFICATE` once the config holds a
/// Codex account.
fn remint_note(holds_codex: bool) -> &'static str {
    if holds_codex {
        "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS and \
         CODEX_CA_CERTIFICATE stay valid"
    } else {
        "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS stays valid"
    }
}

/// The server's boot log line naming how a client trusts the CA it loaded:
/// `NODE_EXTRA_CA_CERTS`, and `CODEX_CA_CERTIFICATE` once the config holds a Codex account.
pub fn advertise_note(holds_codex: bool) -> &'static str {
    if holds_codex {
        "MITM: advertise this CA via NODE_EXTRA_CA_CERTS and CODEX_CA_CERTIFICATE"
    } else {
        "MITM: advertise this CA via NODE_EXTRA_CA_CERTS"
    }
}

/// A CA's private key, PEM-encoded, plus its cert PEM when we happen to have
/// just minted it. `cert_pem` is carried ONLY for the freshly-minted case
/// ([`generate_ca`]) so [`generate_and_persist`] has something to write to
/// disk; re-signing a leaf under an already-persisted CA
/// ([`remint_leaf_under_persisted_ca`]) never reads or needs it — it re-derives
/// the issuer from [`ca_params`] instead, which is what lets that path skip
/// parsing the persisted cert back (no x509 parser dependency).
struct CaChain {
    cert_pem: Option<String>,
    key_pem: String,
}

/// The CA's parameters, reconstructed identically on every mint AND every
/// leaf re-mint — never parsed back from a persisted certificate. This is the
/// mechanism that lets [`sign_leaf`] sign a fresh leaf under a previously
/// generated CA key (branch D) with no x509 parser: [`Issuer::from_params`]
/// only needs a `CertificateParams` + the signing key, and as long as this
/// function is the ONLY place those params are constructed, a re-mint's
/// issuer always matches the CA already on disk — same distinguished name,
/// same key usages, same `is_ca` — because a self-signed cert's subject IS
/// those same params, whichever call minted it. (`Issuer::from_ca_cert_pem`
/// would let a re-mint parse the persisted CA cert directly instead, but that
/// method is gated behind rcgen's `x509-parser` feature, a new transitive
/// dependency not approved for this change.)
fn ca_params() -> anyhow::Result<rcgen::CertificateParams> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyUsagePurpose};
    let mut params = CertificateParams::new(Vec::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "tcr Local CA");
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    Ok(params)
}

/// Mint a brand-new CA key + self-signed cert.
fn generate_ca() -> anyhow::Result<CaChain> {
    let ca_key = rcgen::KeyPair::generate()?;
    let params = ca_params()?;
    let ca_cert = params.self_signed(&ca_key)?;
    Ok(CaChain {
        cert_pem: Some(ca_cert.pem()),
        key_pem: ca_key.serialize_pem(),
    })
}

/// Sign a fresh leaf for `hosts` under `ca`'s key, reconstructing the SAME CA
/// params [`generate_ca`] used (via [`ca_params`]) so the leaf's issuer
/// matches whatever CA certificate is already persisted on disk, without ever
/// parsing it back.
fn sign_leaf(hosts: &[&str], ca: &CaChain) -> anyhow::Result<(String, String)> {
    use rcgen::{
        CertificateParams, DnType, ExtendedKeyUsagePurpose, Issuer, KeyPair, KeyUsagePurpose,
    };

    let ca_key = KeyPair::from_pem(&ca.key_pem)?;
    let issuer_params = ca_params()?;
    let issuer = Issuer::from_params(&issuer_params, ca_key);

    let leaf_key = KeyPair::generate()?;
    let san_list: Vec<String> = hosts.iter().map(|s| (*s).to_string()).collect();
    let mut leaf_params = CertificateParams::new(san_list)?;
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, hosts[0]);
    leaf_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer)?;

    Ok((leaf_cert.pem(), leaf_key.serialize_pem()))
}

/// Generate a fresh CA + a leaf for `hosts` with rcgen. Returns
/// `(ca_cert_pem, leaf_cert_pem, leaf_key_pem, ca_key_pem)`. A thin
/// convenience over [`generate_ca`] + [`sign_leaf`] for call sites that want a
/// complete standalone chain in one call — test-only: production always goes
/// through [`generate_ca`]/[`sign_leaf`] directly ([`generate_and_persist`],
/// [`remint_leaf_under_persisted_ca`]) so a re-mint never needs a NEW CA cert.
#[cfg(test)]
fn generate_chain(hosts: &[&str]) -> anyhow::Result<(String, String, String, String)> {
    let ca = generate_ca()?;
    let ca_cert_pem = ca
        .cert_pem
        .clone()
        .ok_or_else(|| anyhow::anyhow!("generate_ca must always mint a cert_pem"))?;
    let (leaf_pem, leaf_key_pem) = sign_leaf(hosts, &ca)?;
    Ok((ca_cert_pem, leaf_pem, leaf_key_pem, ca.key_pem))
}

/// Write `data` to `path` at exactly `mode`, whether or not `path` already exists.
///
/// The `.mode()` on the open is what keeps a freshly-created key from ever being
/// visible at a wider mode, even momentarily. It is NOT sufficient on its own:
/// `OpenOptionsExt::mode()` applies only to the creating open and is ignored
/// outright when the file already exists, so a stale `tcr-leaf.key` left at `0644`
/// by an older build, an interrupted write, a restore, or a umask accident would
/// survive regeneration world-readable — a readable MITM private key lets anything
/// holding it impersonate every host this proxy intercepts. Hence the post-write
/// re-assert, the same belt-and-braces `crate::config` uses on its token file.
fn write_file(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    file.write_all(data)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// A per-listener transport seam for isolated integration tests. Production listeners
/// always use the ordinary host-and-port connector through serve or serve_with_shutdown.
#[doc(hidden)]
pub type TunnelConnector = Arc<
    dyn Fn(String, u16) -> futures::future::BoxFuture<'static, io::Result<TcpStream>> + Send + Sync,
>;

fn direct_tunnel_connector() -> TunnelConnector {
    Arc::new(|host, port| Box::pin(TcpStream::connect((host, port))))
}

/// Run the hybrid listener forever: accept, classify each connection
/// (`CONNECT` → MITM, else base-URL), and serve it on its own task. `tls` is
/// `None` only when TLS material could not be loaded at all — CONNECT then
/// answers `503`, while base-URL mode keeps working.
///
/// Never returns. A caller that needs to stop serving wants
/// [`serve_with_shutdown`]; this is the shape the in-process tests use, where
/// the loop dies with the test's runtime.
pub async fn serve(listener: TcpListener, manager: Arc<Manager>, tls: Option<Arc<TlsAcceptor>>) {
    serve_with_shutdown(listener, manager, tls, std::future::pending::<()>()).await;
}

/// Test support: run the same listener and CONNECT handler with an explicit blind transport.
/// Tests can supply a connector that only dials a held loopback socket, so a broken
/// interception decision cannot resolve or connect to a real upstream.
#[doc(hidden)]
pub async fn serve_with_tunnel_connector(
    listener: TcpListener,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
    connector: TunnelConnector,
) {
    serve_with_shutdown_using_connector(
        listener,
        manager,
        tls,
        std::future::pending::<()>(),
        connector,
    )
    .await;
}

/// [`serve`], plus a shutdown branch on the accept loop.
///
/// Returning drops the `listener`, so the port stops accepting the moment
/// `shutdown` resolves — no `abort()` from outside, which is what a library
/// caller could not rely on. Connections already accepted are NOT touched: each
/// runs on its own detached task and a proxied response can be a long stream, so
/// cutting one at shutdown would be strictly worse than letting it finish. Same
/// property the old `server.abort()` had, now stated instead of incidental.
///
/// `biased` so a pending shutdown wins over a ready accept: under load an
/// unbiased `select!` picks randomly, and shutdown must not be starved by
/// traffic.
pub async fn serve_with_shutdown(
    listener: TcpListener,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    serve_with_shutdown_using_connector(
        listener,
        manager,
        tls,
        shutdown,
        direct_tunnel_connector(),
    )
    .await;
}

async fn serve_with_shutdown_using_connector(
    listener: TcpListener,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
    shutdown: impl std::future::Future<Output = ()>,
    connector: TunnelConnector,
) {
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            biased;
            () = &mut shutdown => {
                tracing::info!("accept loop shutting down; no longer accepting connections");
                return;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                // A transient accept error must not spin the loop hot.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let manager = manager.clone();
        let tls = tls.clone();
        let connector = Arc::clone(&connector);
        tokio::spawn(
            async move {
                if let Err(err) = handle_conn(stream, peer, manager, tls, connector).await {
                    tracing::debug!(error = %err, "connection ended with error");
                }
            }
            .with_current_subscriber(),
        );
    }
}

/// Classify one accepted connection by peeking its first bytes, then dispatch.
async fn handle_conn(
    stream: TcpStream,
    peer: SocketAddr,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
    connector: TunnelConnector,
) -> io::Result<()> {
    if peek_is_connect(&stream).await? {
        handle_connect(stream, peer, manager, tls, connector).await
    } else {
        // Base-URL mode: cleartext straight through the router. `serve_http`
        // auto-negotiates, so an h2-prior-knowledge client is served as h2 here
        // and everything else stays h1. No tunnel, so no intercepted host.
        serve_http(stream, peer, manager, None).await;
        Ok(())
    }
}

/// Peek (non-destructively) at the first bytes to decide if this is a `CONNECT`
/// tunnel. Leaves every byte in the socket buffer so the base-URL path hands an
/// untouched stream to hyper.
async fn peek_is_connect(stream: &TcpStream) -> io::Result<bool> {
    const NEEDLE: &[u8] = b"CONNECT ";
    let mut buf = [0u8; 8];
    // A localhost client delivers the request line in one segment; the bounded
    // retry only covers the pathological byte-dribble case without spinning.
    for _ in 0..64 {
        let n = stream.peek(&mut buf).await?;
        if n == 0 {
            return Ok(false); // EOF before any request line
        }
        let cmp = n.min(NEEDLE.len());
        if buf[..cmp] != NEEDLE[..cmp] {
            return Ok(false); // definitively not a CONNECT
        }
        if n >= NEEDLE.len() {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    Ok(false)
}

/// Handle a `CONNECT`: read the request head, enforce the allowlist, reply `200`,
/// TLS-accept, and serve the decrypted traffic through the router. `serve_http`
/// auto-negotiates, but this path is h1 in practice: [`build_acceptor`]'s rustls
/// `ServerConfig` advertises no ALPN protocols, so a client is never offered `h2`
/// on this handshake.
async fn handle_connect(
    mut stream: TcpStream,
    peer: SocketAddr,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
    connector: TunnelConnector,
) -> io::Result<()> {
    let head = read_request_head(&mut stream).await?;
    let first_line = head.lines().next().unwrap_or("");
    let Some((host, port)) = parse_connect_target(first_line) else {
        stream.write_all(RESP_BAD_REQUEST).await?;
        stream.flush().await?;
        return Ok(());
    };

    // A non-policy-allowed host → blind tunnel (raw byte-pipe, no
    // interception). This is the open-tunnel behavior the JS proxy provides
    // and Claude Code relies on to reach its bridge and telemetry through the
    // same HTTPS_PROXY. TLS is never terminated here, so nothing is decrypted
    // or injected — just forwarded end-to-end.
    //
    // A policy-allowed host the LOADED LEAF does not cover takes the same
    // fallback — this is the SAN-truthfulness check (see the module doc
    // comment and `load_tls_in`'s design comment). Presenting a leaf for a
    // host it cannot cover fails the client's TLS handshake outright; blind-
    // tunneling it is strictly safer, and it is exactly today's behavior for
    // an uncovered host, not a new failure mode.
    //
    // And `chatgpt.com` is blind-tunneled without Codex's marker, or with no pooled account
    // ([`intercepts_now`]): there is no pooled account to serve a codex, and it
    // keeps its own login. Counted on every CONNECT, never cached, so an import
    // takes effect on the next tunnel.
    let codex_marked = codex_connect_marked(&head);
    let policy_intercepts = intercepts_now(&manager, &host, codex_marked);
    let leaf_covers_host = policy_intercepts && host_covered_by_loaded_leaf(&host);
    if host.eq_ignore_ascii_case(crate::provider::CODEX_HOST) {
        tracing::debug!(
            peer_port = peer.port(),
            codex_marked,
            intercepted = leaf_covers_host && tls.is_some(),
            "Codex CONNECT"
        );
    }
    if !policy_intercepts {
        return tunnel(stream, &host, port, &connector).await;
    }
    if !leaf_covers_host {
        tracing::debug!(
            %host,
            "MITM: host is policy-allowed but the loaded leaf does not cover it — blind-tunneling instead of a broken handshake"
        );
        return tunnel(stream, &host, port, &connector).await;
    }

    let Some(acceptor) = tls else {
        tracing::warn!(%host, "MITM: no TLS material — cannot terminate CONNECT");
        stream.write_all(RESP_UNAVAILABLE).await?;
        stream.flush().await?;
        return Ok(());
    };

    stream.write_all(RESP_CONNECT_OK).await?;
    stream.flush().await?;

    let tls_stream = match acceptor.accept(stream).await {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(%host, error = %err, "MITM: TLS handshake failed");
            return Ok(());
        }
    };
    serve_http(tls_stream, peer, manager, Some(host)).await;
    Ok(())
}

/// Blind-tunnel a non-allowlisted `CONNECT`: open a raw TCP connection to
/// `host:port` and pipe bytes both directions until either side closes. TLS is
/// never terminated, so the client's end-to-end encryption to the real host is
/// untouched and no credentials are involved — a plain forward tunnel, the same
/// open-tunnel behavior the JS `teamclaude` proxy provides (and Claude Code needs
/// to reach `platform.claude.com`, its bridge, and telemetry). Only reachable
/// from `127.0.0.1`, so it is no wider a network surface than the local shell.
async fn tunnel(
    mut client: TcpStream,
    host: &str,
    port: u16,
    connector: &TunnelConnector,
) -> io::Result<()> {
    let mut upstream = match connector(host.to_string(), port).await {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(%host, port, error = %err, "MITM: tunnel connect failed");
            client.write_all(RESP_UNAVAILABLE).await?;
            client.flush().await?;
            return Ok(());
        }
    };
    client.write_all(RESP_CONNECT_OK).await?;
    client.flush().await?;
    // Pipe until EOF in either direction; a mid-tunnel error is non-fatal — log
    // it at debug and let both halves drop.
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((c2u, u2c)) => tracing::debug!(%host, port, c2u, u2c, "MITM: tunnel closed"),
        Err(err) => tracing::debug!(%host, port, error = %err, "MITM: tunnel error"),
    }
    Ok(())
}

/// Read an HTTP request head (request line + headers) up to and including the
/// blank-line terminator, byte-by-byte so we never consume into the body / the
/// following TLS ClientHello. Bounded at 16 KiB.
async fn read_request_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    while buf.len() < 16 * 1024 {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Serve HTTP over `io` through the existing axum router — same auth, rotation,
/// injection, and streaming for both entry points.
///
/// THE single serving function: both call sites land here — [`handle_conn`]'s
/// base-URL branch with a raw TCP stream, and [`handle_connect`] with a
/// TLS-terminated MITM stream. Anything changed here changes both paths.
///
/// The protocol is auto-negotiated (h1, or h2 via ALPN / cleartext prior
/// knowledge), not fixed at HTTP/1.1. In practice the two paths differ:
/// base-URL clients can reach h2 through prior knowledge, while MITM-terminated
/// CONNECT traffic stays h1 because the rustls `ServerConfig` built in
/// [`build_acceptor`] advertises no ALPN protocols, so `h2` is never offered on
/// that handshake.
///
/// `intercepted` is the host of the `CONNECT` tunnel `io` was decrypted from:
/// [`handle_connect`] passes it, and base-URL mode has none. Every request on the
/// connection carries it as a [`crate::proxy::InterceptedHost`], which no client can
/// forge, so `proxy::handle` holds each request to the host the tunnel was opened
/// to, not to the one the request declares.
async fn serve_http<I>(io: I, peer: SocketAddr, manager: Arc<Manager>, intercepted: Option<String>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Session affinity (opt-in): when enabled, mint ONE session key for this whole
    // connection (= one `claude` session: one CONNECT tunnel is one process). The
    // "one connection is one session" premise used to rest on the server being
    // HTTP/1.1; it now rests on the connection, which is the property that actually
    // matters — every request multiplexed over one connection shares this key, on h1
    // and h2 alike. The MITM path is h1 regardless (no ALPN advertised, see
    // `build_acceptor`); a base-URL h2 client simply pins all its streams together.
    // The key's PRESENCE is what switches affinity on for
    // this connection's requests; the routing key itself is always derived from a
    // stable client identity in `proxy::stable_session_key` (no stable identity →
    // the request routes unpinned). The affinity map is bounded by a size cap + LRU
    // eviction in `Manager::select` — stable pins intentionally survive reconnects —
    // so there is no disconnect-release. When disabled, no key is minted and nothing
    // is injected, so `select` receives `affinity = None` and the path stays inert.
    let session_key = manager
        .session_affinity_enabled()
        .then(|| manager.next_session_key());

    let router = crate::proxy::app(manager);
    let service =
        hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
            // Tag the request with the client's address so the auth layer can exempt
            // loopback clients from the api-key gate (see proxy::ClientAddr).
            req.extensions_mut().insert(crate::proxy::ClientAddr(peer));
            // A request decrypted from a tunnel carries the tunnel's host (see above).
            if let Some(host) = &intercepted {
                req.extensions_mut()
                    .insert(crate::proxy::InterceptedHost(host.clone()));
            }
            // When affinity is on, tag every request on this connection with the
            // connection's session key so selection pins to one account.
            if let Some(key) = session_key {
                req.extensions_mut().insert(crate::proxy::SessionKey(key));
            }
            // The router is always ready; `oneshot` drives readiness + call and is
            // Infallible, so no `.unwrap()` sits on the request hot path.
            router.clone().oneshot(req.map(axum::body::Body::new))
        });
    // Auto-negotiating builder (h1 or h2 via ALPN/prior-knowledge detection) so
    // base-URL clients that support h2 aren't forced down to h1.
    //
    // Resource limit worth knowing about: the h2 half carries hyper's default
    // `max_concurrent_streams: Some(200)` (hyper 1.11.0
    // `src/proto/h2/server.rs:69`, applied at :143), which hyper-util's `auto`
    // builder inherits by constructing a stock `http2::Builder`. So a single h2
    // connection is capped at 200 concurrent streams — a ceiling the base-URL path
    // did not have while it was h1-only, where concurrency was bounded by the
    // client's connection count instead. Left at hyper's default deliberately;
    // raise it with `.http2().max_concurrent_streams(n)` if a client ever hits it.
    if let Err(err) =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(hyper_util::rt::TokioIo::new(io), service)
            .await
    {
        tracing::debug!(error = %err, "http connection error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `write_file`'s `mode` must be enforced when the destination ALREADY exists,
    /// not only when it is created. `OpenOptionsExt::mode()` applies solely to the
    /// creating open, so without a post-write `set_permissions` a pre-existing
    /// `tcr-leaf.key` at `0644` — an older build, an interrupted write, a restore,
    /// a umask accident — survives regeneration world-readable while the call site
    /// reads as though it asked for `0600`. That file is the MITM leaf's private
    /// key: anything that can read it can impersonate every intercepted host.
    #[test]
    fn write_file_enforces_mode_on_an_existing_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "tcr-mitm-mode-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tcr-leaf.key");

        // Pre-create the destination at a looser mode, as a stale artifact would be.
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_file(&path, b"fresh key material", 0o600).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "an overwritten private key must end up at 0600, got {mode:o}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh key material");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minted chain must be reused on the next load. Re-minting silently
    /// invalidates the `NODE_EXTRA_CA_CERTS` the user exported after the first
    /// run, so the fallback path breaks on its second use.
    #[test]
    fn load_tls_reuses_a_minted_chain_across_restarts() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let first = load_tls_in(&dir).expect("first load mints a chain");
        let advertised_ca = first.ca_path.expect("a minted chain advertises its CA");
        let leaf_as_minted = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");

        let second = load_tls_in(&dir).expect("second load reuses the chain");
        let leaf_after_reload = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");

        assert_eq!(
            second.ca_path.as_deref(),
            Some(advertised_ca.as_path()),
            "the same CA must stay advertised across restarts"
        );
        assert_eq!(
            leaf_as_minted, leaf_after_reload,
            "the persisted leaf must be reused, not re-minted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each [`load_tls_in`] branch names the leaf it loaded, and what that leaf
    /// lets the listener intercept is [`ALLOWED_HOSTS`] ∩ its coverage: the two
    /// facts a running proxy reports on its status wire
    /// ([`crate::status::MitmStatus`]) and `tcr run --codex` pools on, with the
    /// account rows. A minted leaf (C, then B, then D) lets it intercept every
    /// policy host, `chatgpt.com` included (terminated only while the pool
    /// holds a Codex account), and never `www.example.org`, which it carries
    /// for the JS probe but [`handle_connect`] blind-tunnels.
    /// The JS leaf (A) wins even over a minted chain that covers more, and
    /// intercepts `api.anthropic.com` alone: the one state no restart fixes.
    #[test]
    fn each_load_branch_reports_its_leaf_and_what_it_intercepts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let minted = crate::status::MitmStatus {
            intercepted_hosts: vec![
                "api.anthropic.com".to_string(),
                "platform.claude.com".to_string(),
                "chatgpt.com".to_string(),
            ],
            leaf: "minted".to_string(),
        };

        let fresh = load_tls_in(dir.path()).expect("C: mint a chain");
        assert_eq!(fresh.leaf, LeafSource::Minted, "C: a fresh mint");
        assert_eq!(crate::status::MitmStatus::of(Some(&fresh)), minted, "C");

        let reused = load_tls_in(dir.path()).expect("B: reuse it");
        assert_eq!(reused.leaf, LeafSource::Minted, "B: a reused mint");
        assert_eq!(crate::status::MitmStatus::of(Some(&reused)), minted, "B");

        write_sans_sidecar(&dir.path().join("tcr-leaf.sans"), &["api.anthropic.com"])
            .expect("drift the sidecar");
        let reminted = load_tls_in(dir.path()).expect("D: re-mint under the same CA");
        assert_eq!(reminted.leaf, LeafSource::Minted, "D: a re-mint");
        assert_eq!(crate::status::MitmStatus::of(Some(&reminted)), minted, "D");

        let (_ca, leaf, key, _ca_key) =
            generate_chain(FALLBACK_LEAF_SANS).expect("mint a stand-in JS leaf");
        write_file(
            &dir.path().join(crate::legacy_names::JS_LEAF_CERT),
            leaf.as_bytes(),
            0o644,
        )
        .expect("write leaf");
        write_file(
            &dir.path().join(crate::legacy_names::JS_LEAF_KEY),
            key.as_bytes(),
            0o600,
        )
        .expect("write key");
        let js = load_tls_in(dir.path()).expect("A: the JS leaf");
        assert_eq!(js.leaf, LeafSource::Js, "A: the JS pair wins");
        assert_eq!(
            crate::status::MitmStatus::of(Some(&js)),
            crate::status::MitmStatus {
                intercepted_hosts: vec!["api.anthropic.com".to_string()],
                leaf: "js".to_string(),
            },
            "A"
        );

        assert_eq!(
            crate::status::MitmStatus::of(None),
            crate::status::MitmStatus {
                intercepted_hosts: Vec::new(),
                leaf: "none".to_string(),
            },
            "no TLS material: MITM off"
        );
    }

    /// The pre-existing JS leaf still wins over our own minted chain, and with no
    /// companion CA beside it there is no CA we can name.
    #[test]
    fn load_tls_prefers_the_preexisting_js_leaf() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-jsleaf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let (_ca, leaf, key, _ca_key) =
            generate_chain(FALLBACK_LEAF_SANS).expect("mint a stand-in JS leaf");
        write_file(
            &dir.join(crate::legacy_names::JS_LEAF_CERT),
            leaf.as_bytes(),
            0o644,
        )
        .expect("write leaf");
        write_file(
            &dir.join(crate::legacy_names::JS_LEAF_KEY),
            key.as_bytes(),
            0o600,
        )
        .expect("write key");

        let assets = load_tls_in(&dir).expect("load");
        assert!(
            assets.ca_path.is_none(),
            "with no companion CA on disk there is none to advertise"
        );
        assert!(
            !dir.join("tcr-leaf.pem").exists(),
            "the JS leaf path must not mint a competing chain"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reusing the JS leaf must advertise the CA sitting beside it. This is the
    /// path Gil's install is on, and see-through mode (`tcr run` handing claude a
    /// first-party base URL + `NODE_EXTRA_CA_CERTS`) is unavailable without it —
    /// the CA the client needs is the one that signed the leaf we present.
    #[test]
    fn load_tls_advertises_the_companion_ca_next_to_a_reused_leaf() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-jsca-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let (ca, leaf, key, _ca_key) =
            generate_chain(FALLBACK_LEAF_SANS).expect("mint a stand-in JS chain");
        write_file(
            &dir.join(crate::legacy_names::JS_LEAF_CERT),
            leaf.as_bytes(),
            0o644,
        )
        .expect("write leaf");
        write_file(
            &dir.join(crate::legacy_names::JS_LEAF_KEY),
            key.as_bytes(),
            0o600,
        )
        .expect("write key");
        write_file(
            &dir.join(crate::legacy_names::JS_CA_CERT),
            ca.as_bytes(),
            0o644,
        )
        .expect("write ca");

        let assets = load_tls_in(&dir).expect("load");
        assert_eq!(
            assets.ca_path.as_deref(),
            Some(dir.join(crate::legacy_names::JS_CA_CERT).as_path()),
            "the CA beside the reused leaf must be advertised"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_connect_accepts_host_port() {
        assert_eq!(
            parse_connect_target("CONNECT api.anthropic.com:443 HTTP/1.1"),
            Some(("api.anthropic.com".to_string(), 443))
        );
        // Method match is case-insensitive; trailing version optional.
        assert_eq!(
            parse_connect_target("connect example.com:8443"),
            Some(("example.com".to_string(), 8443))
        );
    }

    #[test]
    fn parse_connect_rejects_malformed() {
        assert_eq!(parse_connect_target("GET / HTTP/1.1"), None);
        assert_eq!(parse_connect_target("CONNECT api.anthropic.com"), None); // no port
        assert_eq!(parse_connect_target("CONNECT :443 HTTP/1.1"), None); // no host
        assert_eq!(parse_connect_target("CONNECT host:notaport"), None);
        assert_eq!(parse_connect_target(""), None);
    }

    #[test]
    fn host_allowlist_is_case_insensitive_and_closed() {
        assert!(host_allowed("api.anthropic.com"));
        assert!(host_allowed("API.Anthropic.COM"));
        // platform.claude.com replaced the two dead OAuth hosts below — see
        // ALLOWED_HOSTS's doc comment for the measured evidence.
        assert!(host_allowed("platform.claude.com"));
        assert!(host_allowed("Platform.Claude.COM"));
        // console.anthropic.com / platform.anthropic.com are dead in current
        // Claude Code (measured zero references across three releases) and must
        // no longer be policy-allowed.
        assert!(!host_allowed("console.anthropic.com"));
        assert!(!host_allowed("platform.anthropic.com"));
        // Everything else is NOT MITM-terminated — it is blind-tunneled instead
        // (host_allowed decides "decrypt + inject" vs "pass through", not allow
        // vs deny), so these all read false and take the tunnel path.
        assert!(!host_allowed("example.com"));
        assert!(!host_allowed("evil.api.anthropic.com.attacker.net"));
        assert!(!host_allowed("localhost"));
        assert!(!host_allowed(""));
    }

    /// `chatgpt.com`, Codex's inference and backend host, is policy-allowed
    /// case-insensitively; a lookalike that merely contains the name is not.
    /// `auth.openai.com`, where the Codex client refreshes its OWN token, is
    /// not allowed either: that exchange has to reach the real host untouched,
    /// so it must keep taking the blind tunnel.
    #[test]
    fn chatgpt_com_is_policy_allowed_and_lookalikes_are_not() {
        assert!(host_allowed("chatgpt.com"));
        assert!(host_allowed("CHATGPT.COM"));
        assert!(!host_allowed("evil.chatgpt.com.attacker.net"));
        assert!(
            !host_allowed("auth.openai.com"),
            "the client's own refresh stays a blind tunnel"
        );
        assert!(!host_allowed("api.openai.com"));
    }

    /// With no Codex account in the pool, `chatgpt.com` is blind-tunneled, so a codex that
    /// reaches tcr runs on its own login instead of being answered `no_codex_account`; with one,
    /// it is intercepted.
    #[test]
    fn an_unmarked_chatgpt_connect_is_tunneled_even_with_a_codex_pool() {
        assert!(!host_intercepted("chatgpt.com", 1, false));
    }

    #[test]
    fn only_the_codex_proxy_authorization_marks_a_connect() {
        for header in [
            "Proxy-Authorization: Basic dGNyLWNvZGV4Og==",
            "proxy-authorization: basic dGNyLWNvZGV4Og==",
        ] {
            let head = format!("CONNECT chatgpt.com:443 HTTP/1.1\r\n{header}\r\n\r\n");
            assert!(codex_connect_marked(&head));
            assert!(host_intercepted(
                "chatgpt.com",
                1,
                codex_connect_marked(&head)
            ));
        }
        for header in [
            "",
            "Authorization: Basic dGNyLWNvZGV4Og==",
            "Proxy-Authorization: Basic dGNyLWNvZGV4",
            "Proxy-Authorization: Bearer dGNyLWNvZGV4Og==",
        ] {
            let head = format!("CONNECT chatgpt.com:443 HTTP/1.1\r\n{header}\r\n\r\n");
            assert!(!codex_connect_marked(&head), "{header}");
            assert!(!host_intercepted(
                "chatgpt.com",
                1,
                codex_connect_marked(&head)
            ));
        }
        assert!(codex_proxy_marked("http://tcr-codex@127.0.0.1:3999"));
        assert!(codex_proxy_marked("http://tcr-codex:@127.0.0.1:3999"));
        assert!(!codex_proxy_marked("http://127.0.0.1:3999"));
        assert!(!codex_proxy_marked("http://other@127.0.0.1:3999"));
    }

    #[test]
    fn chatgpt_com_is_intercepted_only_while_the_pool_holds_a_codex_account() {
        for host in ["chatgpt.com", "CHATGPT.COM", "ChatGPT.com"] {
            assert!(
                !host_intercepted(host, 0, true),
                "{host}: no Codex account, so the tunnel must be blind"
            );
            assert!(host_intercepted(host, 1, true), "{host}: one Codex account");
            assert!(
                host_intercepted(host, 3, true),
                "{host}: three Codex accounts"
            );
        }
    }

    /// The Codex condition is `chatgpt.com`'s alone: the Anthropic hosts are intercepted whatever
    /// the pool's Codex count, and a host off the policy list never is, whatever it is.
    #[test]
    fn only_chatgpt_com_waits_for_a_codex_account() {
        for (codex_accounts, marked) in [(0, false), (0, true), (1, false), (1, true)] {
            for host in ["api.anthropic.com", "platform.claude.com"] {
                assert!(
                    host_intercepted(host, codex_accounts, marked),
                    "{host} with {codex_accounts} Codex account(s)"
                );
            }
            for host in [
                "auth.openai.com",
                "api.openai.com",
                "evil.chatgpt.com.attacker.net",
                "chatgpt.com.",
                "example.com",
                "",
            ] {
                assert!(
                    !host_intercepted(host, codex_accounts, marked),
                    "{host:?} with {codex_accounts} Codex account(s)"
                );
            }
        }
    }

    /// The count [`handle_connect`] decides on is the pool's Codex accounts, read at each
    /// CONNECT: a Claude-only pool blind-tunnels `chatgpt.com` and intercepts its own hosts, and
    /// a Codex account added to the running pool, disabled even (an account counts in any
    /// state), turns `chatgpt.com` on for the next tunnel.
    #[test]
    fn a_claude_only_pool_tunnels_chatgpt_com_until_a_codex_account_is_added() {
        let m = dummy_manager();
        assert!(
            !intercepts_now(&m, "chatgpt.com", true),
            "a Claude-only pool must blind-tunnel chatgpt.com"
        );
        assert!(intercepts_now(&m, "api.anthropic.com", true));

        m.add_account(crate::config::Account {
            name: "bob@example.com/codex".to_string(),
            account_type: crate::config::AccountType::OAuth,
            provider: crate::provider::Provider::Codex,
            account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
            org_uuid: None,
            org_name: None,
            access_token: "at-fake-codex".to_string(),
            refresh_token: Some("rt-fake-codex".to_string()),
            expires_at: Some(crate::now_ms() + 3_600_000),
            priority: Some(0),
            switch_threshold: None,
            disabled: Some(true),
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: crate::config::Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: serde_json::Map::new(),
        });
        assert!(intercepts_now(&m, "chatgpt.com", true), "an imported account enables interception; a disabled pool gets an immediate configuration error");
    }

    #[test]
    fn unrelated_connects_never_wait_for_a_config_write() {
        for (host, marked, expected) in [
            ("example.com", false, false),
            ("api.anthropic.com", false, true),
            ("chatgpt.com", false, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            std::fs::write(&path, b"{}").unwrap();
            let manager = dummy_manager_at(Some(path));
            let held = manager.clone();
            let (ready, acquired) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                held.with_config_write_for_test(|| {
                    ready.send(()).unwrap();
                    let _ = released.recv_timeout(std::time::Duration::from_secs(2));
                })
            });
            acquired.recv().unwrap();
            let start = std::time::Instant::now();
            assert_eq!(intercepts_now(&manager, host, marked), expected);
            let elapsed = start.elapsed();
            let _ = release.send(());
            holder.join().unwrap();
            assert!(
                elapsed < std::time::Duration::from_millis(500),
                "{host} waited {elapsed:?}"
            );
        }
    }

    /// [`FALLBACK_LEAF_SANS`] is "the policy allowlist + the JS test host", so
    /// every [`ALLOWED_HOSTS`] entry must be one of its SANs. A host that is
    /// policy-allowed but absent from the leaf tcr mints (a typo such as
    /// `chatgtp.com` in one of the two lists) is never terminated:
    /// [`handle_connect`]'s coverage check blind-tunnels it, correctly and
    /// silently, so nothing else would ever notice. Asked the way production
    /// asks it: [`leaf_covers`] over the coverage branches B, C and D report.
    #[test]
    fn every_policy_host_is_covered_by_the_leaf_tcr_mints() {
        let covered = fallback_hosts_vec();
        for host in ALLOWED_HOSTS {
            assert!(
                leaf_covers(&covered, host),
                "{host} is policy-allowed but the leaf tcr mints would not cover it, so it \
                 would be blind-tunneled forever: {covered:?}"
            );
        }
    }

    /// The load-bearing property: `platform.claude.com` is policy-allowed AND
    /// covered by a freshly-minted leaf (branch C, [`FALLBACK_LEAF_SANS`]), so
    /// it is intercepted.
    #[test]
    fn platform_claude_com_is_intercepted_when_the_loaded_leaf_covers_it() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-covers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let assets = load_tls_in(&dir).expect("mints a fresh chain covering FALLBACK_LEAF_SANS");
        assert!(
            host_allowed("platform.claude.com"),
            "platform.claude.com must be policy-allowed"
        );
        assert!(
            leaf_covers(&assets.covered_hosts, "platform.claude.com"),
            "a freshly minted leaf must cover platform.claude.com: {:?}",
            assets.covered_hosts
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The anti-outage property, and the most important test in this file: a
    /// policy-allowed host whose coverage is UNKNOWN (branch A, the reused JS
    /// leaf) must read as NOT covered, so `handle_connect` blind-tunnels it
    /// instead of presenting a leaf that will fail the TLS handshake.
    #[test]
    fn platform_claude_com_is_not_intercepted_when_the_loaded_leaf_lacks_it() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-uncovered-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        // Stand in for a foreign/JS leaf whose SAN coverage tcr cannot verify —
        // realistically one that covers only api.anthropic.com.
        let (_ca, leaf, key, _ca_key) =
            generate_chain(&["api.anthropic.com"]).expect("mint a narrow stand-in leaf");
        write_file(
            &dir.join(crate::legacy_names::JS_LEAF_CERT),
            leaf.as_bytes(),
            0o644,
        )
        .expect("write leaf");
        write_file(
            &dir.join(crate::legacy_names::JS_LEAF_KEY),
            key.as_bytes(),
            0o600,
        )
        .expect("write key");

        let assets = load_tls_in(&dir).expect("load");
        assert!(
            host_allowed("platform.claude.com"),
            "policy still allows it — the point is the SAN check catches what policy cannot"
        );
        assert!(
            !leaf_covers(&assets.covered_hosts, "platform.claude.com"),
            "branch A must not claim coverage it cannot verify: {:?}",
            assets.covered_hosts
        );
        assert!(
            leaf_covers(&assets.covered_hosts, "api.anthropic.com"),
            "branch A's one known-safe claim must still hold"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A persisted leaf whose sidecar SAN set does not match what we require
    /// today (or has no sidecar at all — a leaf minted before this field
    /// existed) must be re-minted, not served stale.
    #[test]
    fn a_stale_or_missing_sidecar_forces_a_remint() {
        let dir =
            std::env::temp_dir().join(format!("tcr-mitm-stale-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let first = load_tls_in(&dir).expect("first load mints a chain");
        let leaf_before = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");

        // Simulate a leaf minted under the OLD (pre-change) SAN set.
        write_sans_sidecar(
            &dir.join("tcr-leaf.sans"),
            &[
                "api.anthropic.com",
                "console.anthropic.com",
                "platform.anthropic.com",
                "www.example.org",
            ],
        )
        .expect("write stale sidecar");

        let second = load_tls_in(&dir).expect("second load re-mints on SAN drift");
        let leaf_after_drift = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
        assert_ne!(
            leaf_before, leaf_after_drift,
            "a SAN-drifted sidecar must trigger a fresh leaf, not a stale reuse"
        );
        assert!(leaf_covers(&second.covered_hosts, "platform.claude.com"));
        assert_eq!(
            first.ca_path, second.ca_path,
            "a leaf re-mint must not change which CA path is advertised"
        );

        // Now simulate a leaf minted before the sidecar field existed at all.
        std::fs::remove_file(dir.join("tcr-leaf.sans")).expect("remove sidecar");
        let third = load_tls_in(&dir).expect("third load re-mints on a missing sidecar");
        let leaf_after_missing = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
        assert_ne!(
            leaf_after_drift, leaf_after_missing,
            "a missing sidecar (pre-change leaf) must re-mint, not silently reuse unknown coverage"
        );
        assert!(leaf_covers(&third.covered_hosts, "platform.claude.com"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The upgrade path for every existing install: a sidecar written before
    /// `chatgpt.com` joined [`FALLBACK_LEAF_SANS`] must read as stale. Branch B
    /// of [`load_tls_in`] reports `FALLBACK_LEAF_SANS` itself as the reused
    /// leaf's coverage, so a pre-Codex sidecar that still matched would claim
    /// `chatgpt.com` for a leaf that cannot present it, and once tcr intercepts
    /// `chatgpt.com`, every Codex handshake it terminates would fail. The
    /// mismatch is what re-mints the leaf instead.
    #[test]
    fn a_sidecar_from_before_codex_is_stale_and_forces_a_remint() {
        let before: Vec<String> = [
            "api.anthropic.com",
            "platform.claude.com",
            "www.example.org",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(
            !sans_match(&before, FALLBACK_LEAF_SANS),
            "the pre-Codex SAN set must read as stale"
        );
        let now: Vec<String> = FALLBACK_LEAF_SANS.iter().map(|s| s.to_string()).collect();
        assert!(sans_match(&now, FALLBACK_LEAF_SANS));
    }

    /// A persisted leaf whose sidecar MATCHES the required coverage is reused
    /// as-is — no gratuitous re-mint (already covered by
    /// `load_tls_reuses_a_minted_chain_across_restarts` above; this asserts
    /// the sidecar specifically is what's being checked, not just presence).
    #[test]
    fn a_matching_sidecar_is_reused_without_a_remint() {
        let dir =
            std::env::temp_dir().join(format!("tcr-mitm-fresh-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let _first = load_tls_in(&dir).expect("first load mints a chain + matching sidecar");
        let leaf_before = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");
        let ca_key_before =
            std::fs::read(dir.join("tcr-ca.key")).expect("CA key persisted on first mint");

        let _second = load_tls_in(&dir).expect("second load reuses the chain");
        let leaf_after = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
        let ca_key_after = std::fs::read(dir.join("tcr-ca.key")).expect("CA key still there");

        assert_eq!(
            leaf_before, leaf_after,
            "a matching sidecar must not trigger a re-mint"
        );
        assert_eq!(
            ca_key_before, ca_key_after,
            "no CA rotation should happen when nothing drifted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Branch D end-to-end: after a SAN-drift re-mint, the CA cert stays byte-
    /// identical (no rotation), AND a client trusting ONLY the persisted CA
    /// completes a real TLS handshake against the newly re-minted leaf — the
    /// strongest available proof the re-mint actually reused the persisted CA
    /// key rather than silently minting a new CA.
    #[tokio::test]
    async fn remint_after_san_drift_reuses_the_persisted_ca_key() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-ca-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let first = load_tls_in(&dir).expect("first load mints CA + leaf");
        let ca_pem_before = std::fs::read(dir.join("tcr-ca.pem")).expect("CA cert persisted");

        // Force a SAN drift so the next load must re-mint the leaf.
        write_sans_sidecar(&dir.join("tcr-leaf.sans"), &["api.anthropic.com"])
            .expect("write drifted sidecar");
        let second = load_tls_in(&dir).expect("second load re-mints under the SAME CA");
        let ca_pem_after = std::fs::read(dir.join("tcr-ca.pem")).expect("CA cert still there");

        assert_eq!(
            ca_pem_before, ca_pem_after,
            "a leaf re-mint must not rotate the CA cert"
        );
        assert_eq!(first.ca_path, second.ca_path);

        // Cryptographic proof: a client trusting ONLY the persisted CA must
        // complete a real TLS handshake against the re-minted leaf.
        ensure_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        let mut reader = std::io::BufReader::new(&ca_pem_after[..]);
        for cert in rustls_pemfile::certs(&mut reader) {
            roots
                .add(cert.expect("parse persisted CA cert"))
                .expect("add CA to root store");
        }
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = second.acceptor;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            acceptor
                .accept(stream)
                .await
                .expect("server-side handshake over the re-minted leaf must succeed")
        });

        let client_stream = TcpStream::connect(addr).await.unwrap();
        let server_name = rustls::pki_types::ServerName::try_from("api.anthropic.com")
            .expect("valid DNS name")
            .to_owned();
        let client_tls = connector
            .connect(server_name, client_stream)
            .await
            .expect("a client trusting the persisted CA must accept the re-minted leaf");
        drop(client_tls);
        server.await.expect("server task must complete");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The blind tunnel pipes bytes to the requested upstream and back, and first
    /// answers `200 Connection Established` — proving a non-allowlisted CONNECT
    /// (e.g. platform.claude.com) passes straight through instead of being
    /// refused, which is what Claude Code needs from the forward proxy.
    #[tokio::test]
    async fn tunnel_pipes_bytes_through_to_upstream() {
        // Upstream: a one-shot echo server.
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = echo.accept().await {
                let mut buf = [0u8; 64];
                if let Ok(n) = s.read(&mut buf).await {
                    let _ = s.write_all(&buf[..n]).await;
                }
            }
        });

        // A front socket standing in for the client<->tcr connection.
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_addr = front.local_addr().unwrap();
        let mut client = TcpStream::connect(front_addr).await.unwrap();
        let (server_side, _) = front.accept().await.unwrap();

        // Tunnel the accepted (tcr) side out to the echo upstream.
        tokio::spawn(async move {
            let _ = tunnel(
                server_side,
                "127.0.0.1",
                echo_port,
                &direct_tunnel_connector(),
            )
            .await;
        });

        // The tunnel first writes the CONNECT-OK line.
        let mut ok = [0u8; RESP_CONNECT_OK.len()];
        client.read_exact(&mut ok).await.unwrap();
        assert_eq!(&ok, RESP_CONNECT_OK);

        // Bytes we send are piped to the upstream and echoed straight back.
        client.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        client.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
    }

    /// Cert load path (no network): mint a chain with the rcgen fallback, persist
    /// it, and prove `build_acceptor` loads the leaf into a rustls `ServerConfig`.
    #[test]
    fn generated_chain_loads_into_acceptor() {
        ensure_crypto_provider();
        let (ca_pem, leaf_pem, leaf_key_pem, ca_key_pem) =
            generate_chain(FALLBACK_LEAF_SANS).expect("generate chain");
        assert!(ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(leaf_pem.contains("BEGIN CERTIFICATE"));
        assert!(leaf_key_pem.contains("PRIVATE KEY"));
        assert!(ca_key_pem.contains("PRIVATE KEY"));

        let dir = std::env::temp_dir().join(format!("tcr-mitm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("leaf.pem");
        let key_path = dir.join("leaf.key");
        write_file(&cert_path, leaf_pem.as_bytes(), 0o644).unwrap();
        write_file(&key_path, leaf_key_pem.as_bytes(), 0o600).unwrap();

        // A single leaf cert parses out, and the acceptor builds without error.
        assert_eq!(load_certs(&cert_path).unwrap().len(), 1);
        assert!(build_acceptor(&cert_path, &key_path).is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `Manager` with one never-expiring dummy account and no proxy api-key,
    /// so a loopback GET on `/_tcr/status` is answered locally with no upstream
    /// call and no auth gate — the cheapest real route through `serve_http`'s
    /// router to observe which HTTP version `serve_connection` negotiated.
    fn dummy_manager() -> Arc<Manager> {
        dummy_manager_at(None)
    }

    fn dummy_manager_at(config_path: Option<std::path::PathBuf>) -> Arc<Manager> {
        let config = crate::config::Config {
            quarantined_accounts: Vec::new(),
            migrated_legacy_throttle: false,
            renamed_accounts: Vec::new(),
            rename_write_error: None,
            proxy: crate::config::ProxyConfig {
                port: 0,
                api_key: None,
                extra: serde_json::Map::new(),
            },
            upstream: "http://127.0.0.1:1".to_string(),
            codex_upstream: crate::config::default_codex_upstream(),
            switch_threshold: 0.90,
            fable_weekly_threshold: None,
            pacing: crate::config::PacingConfig::default(),
            account_throttle: crate::config::ThrottleConfig::default(),
            fleet_throttle: crate::config::ThrottleConfig::default(),
            lock_account: None,
            control_account: None,
            control_reserve: 0.05,
            control_pooled: false,
            control_identity: Default::default(),
            reset_urgency_tier_hours: 24,
            http1_only: false,
            accounts: vec![crate::config::Account {
                name: "dummy".to_string(),
                account_type: crate::config::AccountType::OAuth,
                provider: Default::default(),
                account_uuid: None,
                org_uuid: None,
                org_name: None,
                access_token: "at-dummy".to_string(),
                refresh_token: Some("rt-dummy".to_string()),
                expires_at: Some(crate::now_ms() + 3_600_000),
                priority: Some(0),
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
            }],
            group_settings: std::collections::HashMap::new(),
            pricing: Default::default(),
            usage_retention_days: 90,
            extra: serde_json::Map::new(),
        };
        Manager::new(
            config,
            Arc::new(crate::oauth::NoRefresh),
            Arc::new(crate::probe::LiveUsageProber::new()),
            Arc::new(crate::warmer::LiveWarmer::new()),
            config_path,
        )
    }

    /// Start a base-URL-mode listener (`handle_conn`'s non-CONNECT branch, i.e.
    /// `serve_http` over a raw TCP stream — the exact path this change touches)
    /// and return its address.
    async fn spawn_base_url_listener() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let manager = dummy_manager();
        tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    break;
                };
                let manager = manager.clone();
                tokio::spawn(async move {
                    // Same shape as the production accept loop in
                    // `serve_with_shutdown`: a connection error is non-fatal but is
                    // never swallowed silently, so a test that fails because the
                    // connection died has the reason in the captured log rather
                    // than presenting as an unexplained client-side error.
                    if let Err(err) =
                        handle_conn(stream, peer, manager, None, direct_tunnel_connector()).await
                    {
                        tracing::debug!(error = %err, "connection ended with error");
                    }
                });
            }
        });
        addr
    }

    /// Discriminating control: an ordinary h1 client must still work after the
    /// swap to the auto-negotiating builder. If this regresses, the h2 test
    /// below is measuring a broken server, not a real negotiation.
    #[tokio::test]
    async fn base_url_mode_still_serves_http1() {
        let addr = spawn_base_url_listener().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client
            .get(format!("http://{addr}/_tcr/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    }

    /// The behavior this change exists to add: a base-URL client that speaks
    /// h2 via cleartext prior knowledge (no ALPN/TLS involved — this is the
    /// plain-TCP `serve_http` call site at `handle_conn`'s non-CONNECT branch)
    /// negotiates HTTP/2 instead of being forced onto HTTP/1.1.
    #[tokio::test]
    async fn base_url_mode_negotiates_http2_prior_knowledge() {
        let addr = spawn_base_url_listener().await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://{addr}/_tcr/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.version(), reqwest::Version::HTTP_2);
    }

    /// The CA notices name `CODEX_CA_CERTIFICATE` beside `NODE_EXTRA_CA_CERTS` once the config
    /// holds a Codex account, since codex reads its extra CA from that variable alone. Without one
    /// each reads as it always did.
    #[test]
    fn the_ca_export_hint_names_both_variables_with_a_codex_account() {
        let ca = Path::new("/Users/example/.config/tcr-ca.pem");
        assert_eq!(
            ca_export_hint(ca, false),
            "[tcr] minted a fresh MITM CA — trust it with:\n         \
             export NODE_EXTRA_CA_CERTS=/Users/example/.config/tcr-ca.pem"
        );
        assert_eq!(
            ca_export_hint(ca, true),
            "[tcr] minted a fresh MITM CA — trust it with:\n         \
             export NODE_EXTRA_CA_CERTS=/Users/example/.config/tcr-ca.pem \
             CODEX_CA_CERTIFICATE=/Users/example/.config/tcr-ca.pem"
        );
        assert_eq!(
            remint_note(false),
            "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS stays valid"
        );
        assert_eq!(
            remint_note(true),
            "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS and \
             CODEX_CA_CERTIFICATE stay valid"
        );
        assert_eq!(
            advertise_note(false),
            "MITM: advertise this CA via NODE_EXTRA_CA_CERTS"
        );
        assert_eq!(
            advertise_note(true),
            "MITM: advertise this CA via NODE_EXTRA_CA_CERTS and CODEX_CA_CERTIFICATE"
        );
    }
}
