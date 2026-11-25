//! MITM / forward-proxy "backwards mode": a hybrid listener that serves the
//! existing base-URL proxy **and** terminates `HTTPS_PROXY`-style `CONNECT`
//! tunnels on the same port, so a client with `HTTPS_PROXY=http://127.0.0.1:<port>`
//! is served by `tcr` too.
//!
//! How a request flows (reusing `tcr`'s account-selection + token-injection
//! instead of reimplementing the forward):
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
//!      inject nothing). This is the open tunnel for everything else Claude
//!      Code needs (its bridge, telemetry) through the same `HTTPS_PROXY`, and
//!      it is ALSO the anti-outage fallback for a host
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
//! [`crate::request_key::CODEX_PROXY_AUTHORIZATION`]. The pinned Tungstenite `proxy.rs`
//! parses a username without a password and builds the same Basic header as reqwest's
//! hyper-util matcher. This permits one listener without terminating another client's
//! `chatgpt.com` TLS. The marker selects routing; it is not an authentication secret.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt as _;
use tracing::instrument::WithSubscriber as _;

use crate::manager::Manager;
use crate::request_key::{CODEX_PROXY_USERNAME, ConnectClass, ProxyCredential, Unkeyed};
use crate::tls::{host_allowed, host_covered_by_loaded_leaf};

/// Does a proxy URL opt in to Codex interception? Used by the doctor without displaying userinfo.
/// The username alone decides, as on a `CONNECT` ([`ProxyCredential::codex_marked`]): a keyed
/// launch puts the request key in the password, and it must still read as marked.
pub fn codex_proxy_marked(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| url.username() == CODEX_PROXY_USERNAME)
}

const RESP_CONNECT_OK: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const RESP_BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_UNAVAILABLE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

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
    shutdown: impl Future<Output = ()>,
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
    shutdown: impl Future<Output = ()>,
    connector: TunnelConnector,
) {
    let (connections, _drain) = tokio::sync::mpsc::channel(1);
    serve_tracked_with_shutdown_using_connector(
        listener,
        manager,
        tls,
        shutdown,
        connector,
        connections,
    )
    .await;
}

/// Each accepted connection holds a sender until it finishes. Once acceptance
/// stops, receiver closure proves all accepted connections have drained.
pub(crate) async fn serve_tracked_with_shutdown(
    listener: TcpListener,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
    shutdown: impl Future<Output = ()>,
    connections: tokio::sync::mpsc::Sender<()>,
) {
    serve_tracked_with_shutdown_using_connector(
        listener,
        manager,
        tls,
        shutdown,
        direct_tunnel_connector(),
        connections,
    )
    .await;
}

async fn serve_tracked_with_shutdown_using_connector(
    listener: TcpListener,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
    shutdown: impl Future<Output = ()>,
    connector: TunnelConnector,
    connections: tokio::sync::mpsc::Sender<()>,
) {
    let mut shutdown = std::pin::pin!(shutdown);
    let (draining, _) = tokio::sync::watch::channel(false);
    loop {
        let accepted = tokio::select! {
            biased;
            () = &mut shutdown => {
                draining.send_replace(true);
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
        let manager = Arc::clone(&manager);
        let tls = tls.clone();
        let connector = Arc::clone(&connector);
        let connection = connections.clone();
        let drain = draining.subscribe();
        tokio::spawn(
            async move {
                let _connection = connection;
                if let Err(err) = handle_conn(stream, peer, manager, tls, connector, drain).await {
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
    drain: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    if peek_is_connect(&stream).await? {
        handle_connect(stream, peer, manager, tls, connector, drain).await
    } else {
        // Base-URL mode: cleartext straight through the router. `serve_http`
        // auto-negotiates, so an h2-prior-knowledge client is served as h2 here
        // and everything else stays h1. No tunnel, so no intercepted host.
        serve_http(stream, peer, manager, None, drain).await;
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
    drain: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    let head = read_request_head(&mut stream).await?;
    let first_line = head.lines().next().unwrap_or("");
    let Some((host, port)) = parse_connect_target(first_line) else {
        stream.write_all(RESP_BAD_REQUEST).await?;
        stream.flush().await?;
        return Ok(());
    };

    // A non-policy-allowed host → blind tunnel (raw byte-pipe, no
    // interception). This is the open tunnel Claude Code relies on to reach its bridge and telemetry through the
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
    //
    // The head's credential is parsed ONCE, here: the Codex marker (the username)
    // and the request key (the password). Log mode: an unkeyed CONNECT is counted
    // and then served exactly as before, intercepted or blind.
    let credential = ProxyCredential::from_connect_head(&head, manager.request_key());
    if manager.request_key().is_some() && !credential.key_ok {
        manager.unkeyed().record(
            Unkeyed::Connect(ConnectClass::of(&host)),
            credential.user_agent,
            peer.port(),
        );
    }
    let codex_marked = credential.codex_marked;
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
    serve_http(
        tls_stream,
        peer,
        manager,
        Some(Tunnel {
            host,
            keyed: credential.key_ok,
        }),
        drain,
    )
    .await;
    Ok(())
}

/// Blind-tunnel a non-allowlisted `CONNECT`: open a raw TCP connection to
/// `host:port` and pipe bytes both directions until either side closes. TLS is
/// never terminated, so the client's end-to-end encryption to the real host is
/// untouched and no credentials are involved — a plain forward tunnel (Claude
/// Code needs one to reach `platform.claude.com`, its bridge, and telemetry). Only reachable
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
    Ok(String::from_utf8_lossy_owned(buf))
}

/// The `CONNECT` a decrypted stream came through: its host, and whether its head
/// carried the request key.
struct Tunnel {
    host: String,
    keyed: bool,
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
/// to, not to the one the request declares. A tunnel whose `CONNECT` carried the
/// request key also stamps [`crate::proxy::TunnelKeyed`], so its requests are not
/// judged again on their headers.
async fn serve_http<I>(
    io: I,
    peer: SocketAddr,
    manager: Arc<Manager>,
    intercepted: Option<Tunnel>,
    mut drain: tokio::sync::watch::Receiver<bool>,
) where
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
            if let Some(tunnel) = &intercepted {
                req.extensions_mut()
                    .insert(crate::proxy::InterceptedHost(tunnel.host.clone()));
                if tunnel.keyed {
                    req.extensions_mut().insert(crate::proxy::TunnelKeyed);
                }
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
    //
    // `_with_upgrades` so a local route may answer `101` and take the socket:
    // `/_tcr/peer/dial` (`proxy::peer_dial_handler`) is the one that does.
    // hyper hands the socket over only for a `101`, or a `2xx` to a
    // `CONNECT`; a base-URL `CONNECT` is peeked off before hyper sees it, and
    // no route here answers one inside a tunnel with a `2xx`, so every other
    // request is served exactly as before.
    let builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    let connection =
        builder.serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(io), service);
    tokio::pin!(connection);
    tokio::select! {
        // Poll ready request bytes before applying shutdown, including a task
        // first scheduled after the handoff signal. Already accepted requests
        // then get their response instead of a reset at the migration boundary.
        biased;
        result = &mut connection => {
            if let Err(err) = result { tracing::debug!(error = %err, "http connection error"); }
            return;
        }
        () = async {
            if !*drain.borrow() { crate::log_err::LogErr::log_debug(drain.changed().await, "waiting for connection drain"); }
        } => {}
    }
    // h1 completes the current response then closes; h2 sends GOAWAY. Blind
    // CONNECT byte-pipes never enter this function and retain their streams.
    connection.as_mut().graceful_shutdown();
    if let Err(err) = connection.await {
        tracing::debug!(error = %err, "http connection drain error");
    }
}

#[cfg(test)]
mod tests;
