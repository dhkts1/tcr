//! Inside a tunnel tcr terminates, the tunnel's own host decides which provider a request may
//! reach. tcr routes a request by its path (`teamclaude_rs::provider`), and the host has to
//! agree; a request that came through `CONNECT chatgpt.com:443` is a request for `chatgpt.com`,
//! whatever `Host` it declares, and if it declares none. These tests open real tunnels through
//! the real listener ([`teamclaude_rs::mitm::serve`]), over a leaf minted into a temp dir, to an
//! Anthropic and a Codex origin on loopback, and check that:
//!
//! - a request whose path belongs to the other provider is answered 421 and reaches neither
//!   origin, with no `Host` over HTTP/1.1 or HTTP/1.0, and with a loopback `Host`;
//! - a request that names any host but the tunnel's is answered 421 as well;
//! - a request whose path agrees with its tunnel is served, with or without a `Host`;
//! - a base-URL request, which comes through no tunnel, still routes by its path alone.
//!
//! # Why this is its own test binary
//!
//! `handle_connect` terminates a `CONNECT` only for a host covered by the leaf this PROCESS
//! loaded last, and blind-tunnels any other host to the real one. Lib tests in `src/mitm.rs`
//! (`load_tls_prefers_the_preexisting_js_leaf` among them) load the JS proxy's leaf, which tcr
//! counts as covering `api.anthropic.com` alone, so a `chatgpt.com` tunnel opened in the lib test
//! binary while one of them ran would leave the machine. Every test here loads a leaf tcr mints,
//! which covers every intercepted host, and [`start`] checks that it does before any tunnel is
//! opened. A test added to this file must start through [`start`] and must never load the JS leaf.
//!
//! # House rules
//!
//! No real account or token, and no request leaves 127.0.0.1: both origins are on loopback, and
//! every tunnel is to a host the minted leaf covers, so tcr terminates it instead of connecting
//! out. That takes a Codex account in the fleet as well: with none, tcr blind-tunnels
//! `chatgpt.com` to the real host, so [`start`] checks the fleet holds one. Every listener binds
//! `127.0.0.1:0`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde_json::json;
use teamclaude_rs::manager::Manager;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// The bearer a `codex` client sends inside a `chatgpt.com` tunnel: its own ChatGPT login.
const CHATGPT_BEARER: &str = "Bearer at-chatgpt-client";
/// The bearer a Claude client sends inside an `api.anthropic.com` tunnel.
const CLAUDE_BEARER: &str = "Bearer at-claude-client";

/// One request a fake origin received: its path, and the bearer it carried.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    path: String,
    bearer: Option<String>,
}

/// A request for `path` that carried `bearer`, as an origin records it.
fn seen(path: &str, bearer: &str) -> Seen {
    Seen {
        path: path.to_string(),
        bearer: Some(bearer.to_string()),
    }
}

/// What one fake origin received, in order.
#[derive(Clone, Default)]
struct Origin(Arc<Mutex<Vec<Seen>>>);

impl Origin {
    fn seen(&self) -> Vec<Seen> {
        self.0
            .lock()
            .expect("an origin's log is never poisoned")
            .clone()
    }
}

/// Start a fake origin on loopback that records every request and answers it 200 `{}`, and
/// return its base URL.
async fn spawn_origin(origin: Origin) -> String {
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let origin = origin.clone();
        async move {
            let bearer = req
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            origin
                .0
                .lock()
                .expect("an origin's log is never poisoned")
                .push(Seen {
                    path: req.uri().path().to_string(),
                    bearer,
                });
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("build the origin's answer")
        }
    }));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a fake origin");
    let addr = listener.local_addr().expect("the origin's address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// A running tcr: the real listener, over a leaf minted into its own temp dir, in front of an
/// Anthropic and a Codex origin on loopback.
struct Tcr {
    addr: SocketAddr,
    ca_path: PathBuf,
    anthropic: Origin,
    codex: Origin,
    _leaf_dir: tempfile::TempDir,
}

/// Mint a leaf into a fresh temp dir, check that it covers both hosts the tests tunnel to and
/// that the fleet holds a Codex account (so each tunnel is terminated here and never carried to
/// the real host), and serve a fleet of one Anthropic and one Codex account on `127.0.0.1:0`.
async fn start() -> Tcr {
    let leaf_dir = tempfile::tempdir().expect("a temp dir for the leaf");
    let assets = teamclaude_rs::mitm::load_tls_in(leaf_dir.path()).expect("mint a leaf");
    for host in ["chatgpt.com", "api.anthropic.com"] {
        assert!(
            assets
                .covered_hosts
                .iter()
                .any(|covered| covered.eq_ignore_ascii_case(host)),
            "a tunnel to {host} must be terminated here, never carried out: {:?}",
            assets.covered_hosts
        );
    }
    let ca_path = assets.ca_path.clone().expect("a minted leaf names its CA");

    let (anthropic, codex) = (Origin::default(), Origin::default());
    let anthropic_url = spawn_origin(anthropic.clone()).await;
    let codex_url = spawn_origin(codex.clone()).await;
    let in_an_hour = teamclaude_rs::now_ms() + 3_600_000;
    let config = serde_json::from_value(json!({
        "proxy": { "port": 0 },
        "upstream": anthropic_url,
        "codexUpstream": codex_url,
        "accounts": [
            {
                "name": "alice@example.com",
                "accessToken": "at-fake-anthropic",
                "refreshToken": "rt-fake-anthropic",
                "expiresAt": in_an_hour,
            },
            {
                "name": "bob@example.com/codex",
                "provider": "codex",
                "accountUuid": "11111111-1111-4111-8111-111111111111",
                "accessToken": "at-fake-codex",
                "refreshToken": "rt-fake-codex",
                "expiresAt": in_an_hour,
            },
        ],
    }))
    .expect("the test fleet's config parses");
    let manager = Manager::with_live_refresher(config, None);
    assert!(
        manager.provider_account_count(teamclaude_rs::provider::Provider::Codex) > 0,
        "a fleet with no Codex account blind-tunnels chatgpt.com out of the machine"
    );

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind tcr's listener");
    let addr = listener.local_addr().expect("tcr's address");
    tokio::spawn(teamclaude_rs::mitm::serve(
        listener,
        manager,
        Some(Arc::new(assets.acceptor)),
    ));
    Tcr {
        addr,
        ca_path,
        anthropic,
        codex,
        _leaf_dir: leaf_dir,
    }
}

/// A TLS client that trusts the CA at `ca_path` and nothing else.
fn trusting(ca_path: &Path) -> tokio_rustls::TlsConnector {
    let pem = std::fs::read(ca_path).expect("read tcr's CA");
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut &pem[..]) {
        roots
            .add(cert.expect("parse tcr's CA"))
            .expect("trust tcr's CA");
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Read a response head byte by byte, up to and including the blank line, so nothing after it
/// (the start of the TLS handshake) is consumed.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).await.expect("read tcr's answer");
        assert_ne!(n, 0, "tcr closed before it answered the CONNECT");
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// Send `request` as raw bytes and read until tcr closes; every request here asks for that
/// (`connection: close`, or HTTP/1.0). Returns the status and the whole response.
async fn exchange<S>(stream: &mut S, request: &[u8]) -> (u16, String)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(request).await.expect("send the request");
    stream.flush().await.expect("flush the request");
    let mut response = Vec::new();
    let read = async {
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&chunk[..n]),
                // A peer that closes without TLS's close_notify has still sent its answer.
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => panic!("read tcr's answer: {err}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .expect("tcr answers within 10 seconds");
    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("an HTTP status line: {response:?}"));
    (status, response)
}

/// Open `CONNECT {host}:443` through tcr, finish TLS trusting tcr's CA, send `request` inside
/// the tunnel, and return the status and the whole response.
async fn through_tunnel(tcr: &Tcr, host: &str, request: &[u8]) -> (u16, String) {
    let mut tcp = TcpStream::connect(tcr.addr).await.expect("reach tcr");
    tcp.write_all(format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").as_bytes())
        .await
        .expect("send the CONNECT");
    let head = read_head(&mut tcp).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "tcr opens the tunnel to {host}: {head:?}"
    );
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).expect("a DNS name");
    let mut tls = trusting(&tcr.ca_path)
        .connect(name, tcp)
        .await
        .expect("tcr terminates the tunnel with a leaf its CA signed");
    exchange(&mut tls, request).await
}

/// Send `request` to tcr's listener directly, as a base-URL client does: no tunnel.
async fn base_url(tcr: &Tcr, request: &[u8]) -> (u16, String) {
    let mut tcp = TcpStream::connect(tcr.addr).await.expect("reach tcr");
    exchange(&mut tcp, request).await
}

/// A `GET` of `path` over HTTP/1.1 that carries `bearer` and the extra header lines in
/// `headers` (each ending in CRLF), and asks tcr to close after its answer.
fn get(path: &str, bearer: &str, headers: &[u8]) -> Vec<u8> {
    let mut request =
        format!("GET {path} HTTP/1.1\r\nauthorization: {bearer}\r\nconnection: close\r\n")
            .into_bytes();
    request.extend_from_slice(headers);
    request.extend_from_slice(b"\r\n");
    request
}

/// **Inside a `chatgpt.com` tunnel, an Anthropic path is refused however the request names its
/// host.** By its path alone, `/api/oauth/profile` goes to the Anthropic origin carrying the
/// client's own bearer, which inside this tunnel is a ChatGPT token. A request naming
/// `chatgpt.com` was refused already; one naming no host, over HTTP/1.1 or HTTP/1.0, or naming a
/// loopback host, was routed by its path and reached the Anthropic origin with the ChatGPT
/// bearer. The tunnel's host decides now: each is answered 421, naming the tunnel's host and the
/// path, and neither origin sees a byte. The same tunnel still serves a Codex path with no
/// `Host`.
#[tokio::test]
async fn a_chatgpt_com_tunnel_refuses_an_anthropic_path_whatever_host_it_names() {
    let tcr = start().await;
    for (shape, request) in [
        (
            "no Host, HTTP/1.1",
            get("/api/oauth/profile", CHATGPT_BEARER, b""),
        ),
        (
            "no Host, HTTP/1.0",
            format!("GET /api/oauth/profile HTTP/1.0\r\nauthorization: {CHATGPT_BEARER}\r\n\r\n")
                .into_bytes(),
        ),
        (
            "Host: localhost",
            get("/api/oauth/profile", CHATGPT_BEARER, b"host: localhost\r\n"),
        ),
    ] {
        let (status, response) = through_tunnel(&tcr, "chatgpt.com", &request).await;
        assert_eq!(status, 421, "{shape}: {response}");
        assert!(
            response.contains("chatgpt.com") && response.contains("/api/oauth/profile"),
            "{shape}: the refusal names the tunnel's host and the path: {response}"
        );
    }
    assert_eq!(tcr.anthropic.seen(), [], "nothing reached Anthropic");
    assert_eq!(tcr.codex.seen(), [], "nor Codex");

    let (status, response) = through_tunnel(
        &tcr,
        "chatgpt.com",
        &get("/backend-api/wham/usage", CHATGPT_BEARER, b""),
    )
    .await;
    assert_eq!(status, 200, "a Codex path, no Host: {response}");
    assert_eq!(
        tcr.codex.seen(),
        [seen("/backend-api/wham/usage", CHATGPT_BEARER)],
        "it reached Codex with the client's own bearer"
    );
    assert_eq!(tcr.anthropic.seen(), [], "and never Anthropic");
}

/// **The mirror: inside an `api.anthropic.com` tunnel, a `/backend-api/` path is refused
/// however the request names its host.** By its path alone, `/backend-api/wham/usage` goes to
/// the Codex origin carrying the client's own bearer, a Claude token in this tunnel. With no
/// `Host`, or a loopback one, it is answered 421 now and reaches neither origin; the same tunnel
/// still serves an Anthropic path with no `Host`.
#[tokio::test]
async fn an_anthropic_tunnel_refuses_a_backend_api_path_whatever_host_it_names() {
    let tcr = start().await;
    for (shape, extra) in [
        ("no Host", &b""[..]),
        ("Host: 127.0.0.1", &b"host: 127.0.0.1\r\n"[..]),
    ] {
        let request = get("/backend-api/wham/usage", CLAUDE_BEARER, extra);
        let (status, response) = through_tunnel(&tcr, "api.anthropic.com", &request).await;
        assert_eq!(status, 421, "{shape}: {response}");
        assert!(
            response.contains("api.anthropic.com") && response.contains("/backend-api/wham/usage"),
            "{shape}: the refusal names the tunnel's host and the path: {response}"
        );
    }
    assert_eq!(tcr.codex.seen(), [], "nothing reached Codex");
    assert_eq!(tcr.anthropic.seen(), [], "nor Anthropic");

    let (status, response) = through_tunnel(
        &tcr,
        "api.anthropic.com",
        &get("/api/oauth/profile", CLAUDE_BEARER, b""),
    )
    .await;
    assert_eq!(status, 200, "an Anthropic path, no Host: {response}");
    assert_eq!(
        tcr.anthropic.seen(),
        [seen("/api/oauth/profile", CLAUDE_BEARER)],
        "it reached Anthropic with the client's own bearer"
    );
    assert_eq!(tcr.codex.seen(), [], "and never Codex");
}

/// **A tunnelled request that names a host has to name the tunnel's**, even on a path that
/// agrees with the tunnel. tcr serves a tunnel only for the host it was opened to, so a request
/// naming a loopback host, another intercepted host (in its `Host`, or in an absolute-form
/// target), or a `Host` too garbled to read (one byte outside ASCII), is answered 421 and
/// reaches neither origin. The tunnel's own host passes in any letter case and with a port.
#[tokio::test]
async fn a_tunnelled_request_that_names_a_host_must_name_the_tunnels() {
    let tcr = start().await;
    let usage = |extra: &[u8]| get("/backend-api/wham/usage", CHATGPT_BEARER, extra);
    for (shape, request) in [
        ("Host: localhost", usage(b"host: localhost\r\n")),
        ("Host: 127.0.0.1:443", usage(b"host: 127.0.0.1:443\r\n")),
        ("an unreadable Host", usage(b"host: chatgpt.com\xe9\r\n")),
        (
            "another intercepted host",
            usage(b"host: api.anthropic.com\r\n"),
        ),
        (
            "an absolute-form target naming another intercepted host",
            get(
                "https://api.anthropic.com/backend-api/wham/usage",
                CHATGPT_BEARER,
                b"",
            ),
        ),
    ] {
        let (status, response) = through_tunnel(&tcr, "chatgpt.com", &request).await;
        assert_eq!(status, 421, "{shape}: {response}");
        assert!(
            response.contains("chatgpt.com") && response.contains("/backend-api/wham/usage"),
            "{shape}: the refusal names the tunnel's host and the path: {response}"
        );
    }
    assert_eq!(tcr.codex.seen(), [], "nothing reached Codex");
    assert_eq!(tcr.anthropic.seen(), [], "nor Anthropic");

    let request = get(
        "/backend-api/wham/usage",
        CHATGPT_BEARER,
        b"host: ChatGPT.com:443\r\n",
    );
    let (status, response) = through_tunnel(&tcr, "chatgpt.com", &request).await;
    assert_eq!(status, 200, "the tunnel's own host: {response}");
    assert_eq!(
        tcr.codex.seen(),
        [seen("/backend-api/wham/usage", CHATGPT_BEARER)]
    );
}

/// **A base-URL request still routes by its path alone.** It comes through no tunnel, so there
/// is no host to hold it to: with no `Host`, `/backend-api/wham/usage` reaches the Codex origin
/// and `/api/oauth/profile` the Anthropic one, each with the client's own bearer. A fix that
/// refused every request naming no host would break this.
#[tokio::test]
async fn a_base_url_request_with_no_host_keeps_its_paths_provider() {
    let tcr = start().await;
    let (status, response) =
        base_url(&tcr, &get("/backend-api/wham/usage", CHATGPT_BEARER, b"")).await;
    assert_eq!(status, 200, "a Codex path, no Host: {response}");
    let (status, response) = base_url(&tcr, &get("/api/oauth/profile", CLAUDE_BEARER, b"")).await;
    assert_eq!(status, 200, "an Anthropic path, no Host: {response}");
    assert_eq!(
        tcr.codex.seen(),
        [seen("/backend-api/wham/usage", CHATGPT_BEARER)]
    );
    assert_eq!(
        tcr.anthropic.seen(),
        [seen("/api/oauth/profile", CLAUDE_BEARER)]
    );
}
