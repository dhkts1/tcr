//! The TLS chain tcr presents on an intercepted `CONNECT`: which hosts it means
//! to intercept ([`ALLOWED_HOSTS`], [`host_allowed`]), the CA and leaf it mints,
//! persists and loads ([`load_tls`]), and which hosts the loaded leaf can
//! actually present ([`TlsAssets`], `host_covered_by_loaded_leaf`).
//!
//! The listener that uses it, and the per-`CONNECT` decision, live in
//! `crate::proxy::mitm`. This module holds no socket: a client names the running
//! proxy's CA through it ([`minted_ca_path`]) and the status wire reports the
//! loaded leaf's coverage through it ([`TlsAssets::intercepted_hosts`]),
//! neither of which needs the listener.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio_rustls::TlsAcceptor;

/// Hosts we intend to MITM-terminate + inject tokens for — a POLICY list, not
/// a firewall (everything else on `CONNECT` is blind-tunneled, no
/// interception). This is necessary but not sufficient for interception:
/// `mitm::handle_connect` additionally requires [`host_covered_by_loaded_leaf`],
/// because being on this list says nothing about whether the leaf actually
/// loaded for this process can present a valid certificate for the host — see
/// the module doc comment and [`load_tls_in`]'s design comment.
///
/// `console.anthropic.com` and `platform.anthropic.com` were removed 2026-08:
/// measured zero references in three consecutive Claude Code releases
/// (2.1.232-234) against the shipped binary, i.e. dead. `platform.claude.com`
/// was added in their place: it is the host Claude Code's own OAuth refresh
/// (`POST /v1/oauth/token`) targets — 2 occurrences in the same binary — and
/// `src/proxy/mod.rs`'s `RelayMode::Raw` handling for that path was unreachable in
/// MITM mode without it.
///
/// `chatgpt.com` was added 2026-10 for Codex, whose inference and backend
/// calls go there by default (`openai/codex` `model-provider-info/src/lib.rs:80`,
/// `CHATGPT_CODEX_BASE_URL`). It is intercepted only on a marked Codex connection while the pool
/// holds a Codex account (`host_intercepted`), so an unmarked client or a codex with none keeps
/// its own login. A login whose workspace is routed to another backend origin
/// sends its calls there instead, to a host not on this list; tcr pools no such
/// workspace, and relays a request that names a routing of its own on the
/// client's credential (`crate::base::provider::CODEX_ROUTING_HEADERS`).
/// `auth.openai.com` is deliberately NOT here: it is where the Codex client
/// refreshes its OWN token, and that exchange must reach the real host
/// untouched — tcr has nothing to inject into it, and Codex refresh tokens are
/// single-use, so a refresh tcr mishandled would cost the client its login.
/// Off this list it is blind-tunneled.
///
/// Peer egress has its own narrower list ([`crate::peer::tunnel::egress::PEER_EGRESS_HOSTS`]);
/// adding a local interception host grants no LAN gateway access to it.
pub const ALLOWED_HOSTS: &[&str] = &["api.anthropic.com", "platform.claude.com", "chatgpt.com"];

/// SAN list for the rcgen fallback leaf (the policy allowlist + a probe
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
/// the acceptor, not the `TlsAssets` it came from — so `mitm::handle_connect`,
/// which has no other way to reach the coverage of the leaf it is about to
/// present, reads it here via [`host_covered_by_loaded_leaf`]. The lib tests
/// read `TlsAssets::covered_hosts` off the returned value instead; a test that
/// opens a tunnel does rely on this global, which is why
/// `tests/proxy/intercepted_host.rs` is its own binary. Starts empty — "nothing
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
/// before a re-mint completes.
pub(crate) fn host_covered_by_loaded_leaf(host: &str) -> bool {
    COVERED_HOSTS
        .lock()
        .is_ok_and(|hosts| leaf_covers(&hosts, host))
}

/// Loaded TLS material plus the CA path to advertise via `NODE_EXTRA_CA_CERTS`.
pub struct TlsAssets {
    pub acceptor: TlsAcceptor,
    /// The CA cert clients should trust to accept [`Self::acceptor`]'s leaf:
    /// `tcr-ca.pem` ([`minted_ca_path`]), the CA every leaf tcr serves chains to.
    pub ca_path: PathBuf,
    /// SAN coverage of [`Self::acceptor`]'s leaf, as actually loaded — not
    /// derived from [`ALLOWED_HOSTS`], because the policy list and the leaf can
    /// differ (see [`load_tls_in`]'s design comment). This is the field that
    /// makes the CONNECT interception decision SAN-truthful:
    /// [`host_covered_by_loaded_leaf`] reads a process-wide snapshot of it.
    pub covered_hosts: Vec<String>,
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
/// decision in `mitm::handle_connect` additionally requires
/// [`host_covered_by_loaded_leaf`], because this function says nothing about
/// whether the leaf loaded for this process can actually present a valid
/// certificate for it. Kept as a standalone, TLS-independent check because
/// `src/proxy/mod.rs`'s base-URL-mode misroute guard also calls it, on a plain-HTTP
/// path where no TLS handshake — and therefore no SAN coverage question —
/// exists at all.
pub fn host_allowed(host: &str) -> bool {
    ALLOWED_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// `~/.config` — the dir holding the config and tcr's certs: the parent of
/// [`crate::base::config::default_path`].
fn config_dir() -> PathBuf {
    crate::base::config::default_path()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Where tcr's own CA lives (`~/.config/tcr-ca.pem`): the CA every
/// leaf tcr mints chains to, and one that stays put across leaf
/// re-mints now that its key is persisted beside it (branch D).
///
/// A path, not a load: no IO at all. A CLIENT names the running proxy's CA
/// with this, never with [`load_tls`], which re-mints a stale leaf on disk
/// while the proxy keeps serving the leaf it loaded at boot (nothing reloads
/// it) and so reports coverage that proxy does not have.
pub fn minted_ca_path() -> PathBuf {
    MintedPaths::in_dir(&config_dir()).ca
}

/// Resolve the CA of an existing usable leaf without writing certificate material.
/// Server startup owns SAN migrations and minting. A client must keep trusting the
/// persisted chain even when its sidecar is stale or its CA key predates persistence.
/// This lookup never changes the process-wide loaded-leaf coverage.
pub fn existing_ca_path_in(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    ensure_crypto_provider();
    let minted = MintedPaths::in_dir(dir);
    if !minted.all_present() {
        return Ok(None);
    }
    build_acceptor(&minted.leaf_cert, &minted.leaf_key)?;
    load_certs(&minted.ca)?;
    Ok(Some(minted.ca))
}

/// Ensure a process-wide rustls crypto provider is installed before building a
/// `ServerConfig`, whose `builder()` panics when no default is set. The same
/// install every `reqwest` client needs: [`crate::base::http_client`].
fn ensure_crypto_provider() {
    crate::base::http_client::install_crypto_provider();
}

/// The CA + leaf tcr mints for itself. Named in exactly one place so the loader
/// and the writer cannot drift apart (they did: the loader probed one name
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

/// Load the TLS material for MITM: reuse the chain we minted on an earlier run,
/// else mint a fresh CA+leaf with rcgen and persist it.
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
/// was given. A test that opens tunnels runs in its own binary (see
/// `tests/proxy/intercepted_host.rs`).
///
/// Three branches, in order (B, C and D, the names this file's other comments
/// use), and this is the function that makes the whole module SAN-truthful —
/// every return path sets `covered_hosts` to what the chosen leaf can ACTUALLY
/// present, never to the policy list:
///
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
                        ca_path: minted.ca,
                        covered_hosts,
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
            ca_path: minted.ca.clone(),
            covered_hosts,
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
    let mut reader = io::BufReader::new(&data[..]);
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", path.display());
    }
    Ok(certs)
}

fn load_key(path: &Path) -> anyhow::Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let data = std::fs::read(path)?;
    let mut reader = io::BufReader::new(&data[..]);
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
        ca_path: minted.ca,
        covered_hosts,
    })
}

/// Compare certificate bytes, so an extra-CA bundle can retain corporate roots and tcr's CA.
/// An identical path needs no second read; the route already checked tcr's CA exists.
pub fn ca_bundle_contains(bundle: &Path, ca: &Path) -> bool {
    if bundle == ca {
        return true;
    }
    let certs = |path: &Path| -> io::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
        let bytes = std::fs::read(path)?;
        rustls_pemfile::certs(&mut bytes.as_slice()).collect()
    };
    match (certs(bundle), certs(ca)) {
        (Ok(bundle), Ok(required)) => {
            !required.is_empty() && required.iter().all(|cert| bundle.contains(cert))
        }
        _ => false,
    }
}

/// Scope Codex's proxy marker and CA to its launch; never export either for the whole shell.
/// Claude can keep using its existing `NODE_EXTRA_CA_CERTS` setup.
pub fn ca_export_hint(ca: &Path, holds_codex: bool) -> String {
    let mut line = format!(
        "[tcr] minted a fresh MITM CA — trust it with:\n         export NODE_EXTRA_CA_CERTS={}",
        ca.display()
    );
    if holds_codex {
        line.push_str("\n[tcr] for Codex, use `tcr run --codex` to scope proxy routing and CA trust to that launch");
    }
    line
}

/// The log line for a leaf re-minted under the persisted CA, which keeps every exported CA
/// variable valid: `NODE_EXTRA_CA_CERTS`, and `CODEX_CA_CERTIFICATE` once the config holds a
/// Codex account.
const fn remint_note(holds_codex: bool) -> &'static str {
    if holds_codex {
        "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS and \
         CODEX_CA_CERTIFICATE stay valid"
    } else {
        "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS stays valid"
    }
}

/// The server's boot log line naming how a client trusts the CA it loaded:
/// `NODE_EXTRA_CA_CERTS`, and `CODEX_CA_CERTIFICATE` once the config holds a Codex account.
pub const fn advertise_note(holds_codex: bool) -> &'static str {
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
/// re-assert, the same belt-and-braces `crate::base::config` uses on its token file.
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

#[cfg(test)]
mod tests;
