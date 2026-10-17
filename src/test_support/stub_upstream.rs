//! Fake upstreams for unit and integration tests, all served from two loopback listeners per test
//! binary. A `tcr` child process a test starts reaches them the same way: they are ordinary
//! loopback URLs.
//!
//! Every test used to bind its own listener for its fake upstream, so a run of the proxy tests
//! opened and closed a few hundred ports. Here one listener serves every HTTP stub and one serves
//! every raw-socket stub, and each stub is a ROUTE on it: its address is
//! `127.0.0.1:<shared port>/stub/<id>`, which the proxy turns into `…/stub/<id>/v1/messages`
//! because it builds an upstream URL by appending the request's path to the configured base
//! (`format!("{}{}", manager.upstream_for(provider), path_and_query)` in `proxy.rs`). The server
//! strips the prefix again, so a stub sees the same path, query, headers and body it saw on a
//! port of its own.
//!
//! Two listeners rather than one because the two kinds route at different grains. An HTTP stub
//! routes per REQUEST, and every HTTP answer closes its connection: a client pools connections by
//! host and port, every stub now shares both, and a pooled connection must never outlive the test
//! that opened it (see [`dispatch_http`]). A raw stub owns its whole CONNECTION (it writes bytes,
//! sleeps, or hangs up), so it is routed once, by the first request on it, and every raw reply
//! ends that connection.
//!
//! The listeners run on their own runtime on their own thread, which outlives any single
//! `#[tokio::test]` runtime. A stub is never unregistered: its route lives until the binary exits,
//! as the per-test server it replaces lived until its runtime did, and a test that formats the
//! address into a temporary cannot lose its route by dropping a handle early.
//!
//! What still needs a socket of its own does not belong here: a port nothing listens on, a peer
//! the code under test dials by a fixed port, an HTTP/2 stream reset, and a test that drives the
//! proxy's own listener over a real socket (see each test's comment).
//!
//! # The Unix socket
//!
//! Most of a proxy test's connections are the proxy's OWN clients (the per-account serving client
//! and the relay client) dialing a stub, and each was a loopback TCP connection and an ephemeral
//! port. So the same routes are also served on one Unix domain socket per binary, and a serving or
//! relay client built on a thread that registered a stub sends every request over it
//! ([`client_socket`], read by `build_serving_client` in `manager/mod.rs` and `relay_client` in
//! `proxy.rs` only in a test build). The request still carries its `http://127.0.0.1:<port>/stub/<id>`
//! URL and `Host`, so a stub sees what it saw over TCP. A connection on the socket is routed by its
//! first request: a raw stub gets the connection, an HTTP stub's request is served by hyper.
//!
//! A `unix_socket` client sends EVERYTHING over the socket and never resolves or dials, so a test
//! whose serving client must reach something else (a refused port, its own HTTP/2 listener, a name
//! that does not resolve) calls [`tcp_clients`] first. Forgetting to is loud: the socket answers a
//! request that names no stub `421` with a message saying so.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tower::ServiceExt as _;

use crate::log_err::LogErr as _;

/// Where one stub answers. `Display` prints `127.0.0.1:<port>/stub/<id>`, so the
/// `format!("http://{addr}")` every test already writes for an upstream base still yields the
/// stub's base URL; [`StubAddr::url`] is the same string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StubAddr {
    listener: SocketAddr,
    id: u64,
}

impl StubAddr {
    /// The base URL to configure as an upstream: `http://127.0.0.1:<port>/stub/<id>`.
    pub fn url(&self) -> String {
        format!("http://{self}")
    }
}

impl fmt::Display for StubAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}{}", self.listener, PREFIX, self.id)
    }
}

const PREFIX: &str = "/stub/";

/// A byte stream a stub reads and writes: a loopback TCP connection or one on the Unix socket.
pub trait StubIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> StubIo for T {}

/// One accepted connection, handed to the raw stub its first request named.
pub struct RawConn {
    /// The connection, positioned just after [`RawConn::head`].
    pub stream: Box<dyn StubIo>,
    /// What the shared listener read of the request (at least its whole head), prefix included.
    pub head: Vec<u8>,
}

type RawFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type RawHandler = Arc<dyn Fn(RawConn) -> RawFuture + Send + Sync>;

enum Route {
    Http(Router),
    Raw(RawHandler),
}

struct Shared {
    http: SocketAddr,
    raw: SocketAddr,
    socket: PathBuf,
    routes: Mutex<HashMap<u64, Route>>,
    next: AtomicU64,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(start)
}

thread_local! {
    /// Whether a serving or relay client built on this thread goes over the Unix socket:
    /// `None` until the thread registers a stub (then `true`) or calls [`tcp_clients`] (then
    /// `false`, for good). libtest runs each test on a thread of its own, so this is per test.
    static SOCKET_CLIENTS: Cell<Option<bool>> = const { Cell::new(None) };
}

/// The Unix socket a serving or relay client built on this thread should send over, or `None`
/// to dial TCP as production does. `Some` once this thread has registered a stub, unless it called
/// [`tcp_clients`]. Read only by the two client builders, and only in a test build.
pub fn client_socket() -> Option<&'static Path> {
    match SOCKET_CLIENTS.with(Cell::get) {
        Some(true) => Some(shared().socket.as_path()),
        _ => None,
    }
}

/// Keep this thread's serving and relay clients on TCP, for a test whose upstream is not a stub
/// (a refused port, a listener of its own, an unresolvable name) while it also registers one.
pub fn tcp_clients() {
    SOCKET_CLIENTS.with(|cell| cell.set(Some(false)));
}

/// `$TMPDIR/tcr-stub-<pid>.sock`. The pid keeps two test binaries apart; a stale file a dead
/// binary with the same pid left behind is removed before binding. The file outlives the binary,
/// since nothing runs at process exit to unlink it; it is an empty inode in the temp dir.
fn socket_path() -> PathBuf {
    std::env::temp_dir().join(format!("tcr-stub-{}.sock", std::process::id()))
}

fn start() -> Shared {
    let http =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind the shared HTTP stub listener");
    let raw =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind the shared raw stub listener");
    let addrs = (
        http.local_addr().expect("HTTP stub listener addr"),
        raw.local_addr().expect("raw stub listener addr"),
    );
    http.set_nonblocking(true)
        .expect("HTTP stub listener nonblocking");
    raw.set_nonblocking(true)
        .expect("raw stub listener nonblocking");
    let path = socket_path();
    if path.exists() {
        std::fs::remove_file(&path).expect("remove a stale stub socket");
    }
    let socket =
        std::os::unix::net::UnixListener::bind(&path).expect("bind the shared stub Unix socket");
    socket
        .set_nonblocking(true)
        .expect("stub Unix socket nonblocking");
    std::thread::Builder::new()
        .name("stub-upstreams".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("stub upstream runtime");
            runtime.block_on(async move {
                let http = tokio::net::TcpListener::from_std(http).expect("adopt HTTP listener");
                let raw = tokio::net::TcpListener::from_std(raw).expect("adopt raw listener");
                let socket =
                    tokio::net::UnixListener::from_std(socket).expect("adopt stub Unix socket");
                tokio::spawn(serve_raw(raw));
                tokio::spawn(serve_socket(socket));
                axum::serve(http, Router::new().fallback(dispatch_http))
                    .await
                    .log_debug("serving the shared HTTP stub listener");
            });
        })
        .expect("spawn the stub upstream thread");
    Shared {
        http: addrs.0,
        raw: addrs.1,
        socket: path,
        routes: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
    }
}

fn register(route: Route, raw: bool) -> StubAddr {
    SOCKET_CLIENTS.with(|cell| {
        if cell.get().is_none() {
            cell.set(Some(true));
        }
    });
    let shared = shared();
    let id = shared.next.fetch_add(1, Ordering::SeqCst);
    shared
        .routes
        .lock()
        .expect("stub routes poisoned")
        .insert(id, route);
    StubAddr {
        listener: if raw { shared.raw } else { shared.http },
        id,
    }
}

/// `/stub/<id>/rest?q` → (`id`, `/rest?q`). `None` for anything else.
fn split_route(path_and_query: &str) -> Option<(u64, String)> {
    let rest = path_and_query.strip_prefix(PREFIX)?;
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let id = rest[..end].parse().ok()?;
    let tail = &rest[end..];
    let tail = if tail.starts_with('/') {
        tail.to_string()
    } else {
        format!("/{tail}")
    };
    Some((id, tail))
}

async fn dispatch_http(mut req: Request) -> Response {
    let target = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_default();
    let Some((id, tail)) = split_route(&target) else {
        return (StatusCode::NOT_FOUND, format!("no stub route in {target}")).into_response();
    };
    let router = match shared()
        .routes
        .lock()
        .expect("stub routes poisoned")
        .get(&id)
    {
        Some(Route::Http(router)) => router.clone(),
        _ => return (StatusCode::NOT_FOUND, format!("no HTTP stub {id}")).into_response(),
    };
    *req.uri_mut() = match tail.parse::<Uri>() {
        Ok(uri) => uri,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let mut response = match router.oneshot(req).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    // Every answer ends its connection. Production keeps one process-wide relay client
    // (`RELAY_CLIENT` in `proxy.rs`), whose pool is keyed by host and port, and every stub now
    // shares both: a connection it pooled on one test's runtime would be handed to the next test
    // after that runtime is gone, and the relay would answer 502. With a port per stub the key
    // never repeated, so this never came up.
    response.headers_mut().insert(
        axum::http::header::CONNECTION,
        axum::http::HeaderValue::from_static("close"),
    );
    response
}

async fn serve_raw(listener: tokio::net::TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(route_connection(Box::new(stream), Listener::Raw));
    }
}

async fn serve_socket(listener: tokio::net::UnixListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(route_connection(Box::new(stream), Listener::Socket));
    }
}

/// Which listener accepted a connection [`route_connection`] routes.
#[derive(Clone, Copy)]
enum Listener {
    /// The raw TCP listener: only raw stubs answer on it.
    Raw,
    /// The Unix socket: every stub answers on it.
    Socket,
}

/// Route one connection by its first request: a raw stub takes the connection, and on the Unix
/// socket an HTTP stub's request is served by hyper from the bytes already read.
async fn route_connection(mut stream: Box<dyn StubIo>, listener: Listener) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    // The request line is all routing needs, but read the whole head: a stub that answered and
    // closed with request bytes still unread would reset the connection under its own reply. One
    // read nearly always holds it all.
    while !head.windows(4).any(|quad| quad == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let line = String::from_utf8_lossy(&head);
    let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
    enum Found {
        Raw(RawHandler),
        Http,
        Nothing,
    }
    let found = match split_route(&target) {
        Some((id, _)) => match shared()
            .routes
            .lock()
            .expect("stub routes poisoned")
            .get(&id)
        {
            Some(Route::Raw(handler)) => Found::Raw(Arc::clone(handler)),
            Some(Route::Http(_)) => Found::Http,
            None => Found::Nothing,
        },
        None => Found::Nothing,
    };
    match (found, listener) {
        (Found::Raw(handler), _) => handler(RawConn { stream, head }).await,
        (Found::Http, Listener::Socket) => {
            let service =
                hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| async {
                    Ok::<_, std::convert::Infallible>(
                        dispatch_http(req.map(axum::body::Body::new)).await,
                    )
                });
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    hyper_util::rt::TokioIo::new(Replay {
                        head,
                        read: 0,
                        stream,
                    }),
                    service,
                )
                .await
                .log_debug("serving an HTTP stub over the Unix socket");
        }
        (_, Listener::Raw) => {
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .log_debug("answering an unrouted raw stub request");
        }
        (Found::Nothing, Listener::Socket) => {
            let body = format!(
                "{target} names no stub, but it came over the stub Unix socket: a serving or \
                 relay client built on a thread that registered a stub sends everything here. A \
                 test whose upstream is not a stub calls stub_upstream::tcp_clients() first."
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 421 Misdirected Request\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .log_debug("answering a misdirected stub socket request");
        }
    }
}

/// A connection whose first bytes were already read for routing: reads return `head` first,
/// then the rest of `stream`. Writes go straight to `stream`.
struct Replay {
    head: Vec<u8>,
    read: usize,
    stream: Box<dyn StubIo>,
}

impl AsyncRead for Replay {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.read < this.head.len() {
            let n = out.remaining().min(this.head.len() - this.read);
            out.put_slice(&this.head[this.read..this.read + n]);
            this.read += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.stream).poll_read(cx, out)
    }
}

impl AsyncWrite for Replay {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// Serve `router` as a fake upstream.
pub fn http(router: Router) -> StubAddr {
    register(Route::Http(router), false)
}

/// Hand every connection whose first request names this stub to `handler`.
pub fn raw<F, Fut>(handler: F) -> StubAddr
where
    F: Fn(RawConn) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    register(
        Route::Raw(Arc::new(move |conn| -> RawFuture {
            Box::pin(handler(conn))
        })),
        true,
    )
}

/// A fake upstream that answers by CONNECTION ORDINAL: connection `n` gets `script[n]`, and
/// connections past the end reuse the last entry. `Some(raw)` writes `raw` verbatim; `None` reads
/// the request and hangs up without a reply, which a client sees as a transport failure. Every
/// scripted reply must send `connection: close` so attempt N is always connection N. The counter
/// is the number of connections this stub has been handed, i.e. of upstream attempts.
pub fn scripted(script: Vec<Option<String>>) -> (StubAddr, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let script = Arc::new(script);
    let addr = raw(move |mut conn| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let reply = script.get(n).or_else(|| script.last()).cloned().flatten();
        async move {
            if let Some(raw) = reply {
                conn.stream
                    .write_all(raw.as_bytes())
                    .await
                    .log_debug("writing a test response");
            }
            // `None`: dropping the socket with no reply is "connection closed before
            // message completed" on the client side, a transport error.
        }
    });
    (addr, attempts)
}

/// A whole raw HTTP/1.1 reply: `status` (`"200 OK"`), one `content-type`, a `content-length`
/// for `body`, and `connection: close`, the shape every [`scripted`] reply must have.
pub fn reply(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A fake upstream that answers every request `200` with the JSON `answer`, calling `on_request`
/// first (a test that watches what the caller did before asking). The counter is how many
/// requests it has answered.
pub fn json_answering(
    answer: &'static str,
    on_request: impl Fn() + Send + 'static,
) -> (StubAddr, Arc<AtomicUsize>) {
    let answered = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&answered);
    // `Mutex` so a callback that is `Send` but not `Sync` can be shared by connections.
    let on_request = Mutex::new(on_request);
    let addr = raw(move |mut conn| {
        (on_request.lock().expect("stub callback poisoned"))();
        counter.fetch_add(1, Ordering::SeqCst);
        async move {
            conn.stream
                .write_all(reply("200 OK", "application/json", answer).as_bytes())
                .await
                .log_debug("writing a test response");
        }
    });
    (addr, answered)
}

/// [`scripted`] with one entry: every connection gets `raw`.
pub fn canned(raw: impl Into<String>) -> StubAddr {
    scripted(vec![Some(raw.into())]).0
}

/// [`raw`] for a stub that reads the request, waits `delay`, then writes `response` and closes.
pub fn delayed(response: String, delay: std::time::Duration) -> StubAddr {
    let response = Arc::new(response);
    raw(move |mut conn| {
        let response = Arc::clone(&response);
        async move {
            tokio::time::sleep(delay).await;
            conn.stream
                .write_all(response.as_bytes())
                .await
                .log_debug("writing a test response");
        }
    })
}

/// [`raw`] for a stub that writes `partial` (a status line, headers and the start of a body that
/// never ends) and then sends nothing more. It holds the connection until the client closes it,
/// which is what the per-test server it replaces did by sleeping an hour on it.
pub fn stalled(partial: String) -> StubAddr {
    let partial = Arc::new(partial);
    raw(move |mut conn| {
        let partial = Arc::clone(&partial);
        async move {
            conn.stream
                .write_all(partial.as_bytes())
                .await
                .log_debug("writing a test response");
            let mut sink = [0u8; 1024];
            while let Ok(n) = conn.stream.read(&mut sink).await {
                if n == 0 {
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_route_strips_the_prefix_and_keeps_path_and_query() {
        assert_eq!(
            split_route("/stub/7/v1/messages?beta=true"),
            Some((7, "/v1/messages?beta=true".into()))
        );
        assert_eq!(split_route("/stub/7?x=1"), Some((7, "/?x=1".into())));
        assert_eq!(split_route("/stub/7"), Some((7, "/".into())));
        assert_eq!(split_route("/v1/messages"), None);
        assert_eq!(split_route("/stub/x/v1"), None);
    }

    /// Two stubs share one port, each reaches its own router with the path it was sent, and
    /// every answer closes its connection so no client can pool one past its test.
    #[tokio::test]
    async fn two_http_stubs_on_one_port_each_see_their_own_path_and_close() {
        let a = http(Router::new().fallback(|uri: Uri| async move { format!("a {uri}") }));
        let b = http(Router::new().fallback(|uri: Uri| async move { format!("b {uri}") }));
        assert_eq!(
            a.listener, b.listener,
            "one listener serves every HTTP stub"
        );
        let client = crate::test_support::loopback::client();
        for _ in 0..2 {
            for (stub, want) in [
                (a, "a /v1/messages?beta=true"),
                (b, "b /v1/messages?beta=true"),
            ] {
                let response = client
                    .get(format!("{}/v1/messages?beta=true", stub.url()))
                    .send()
                    .await
                    .expect("send");
                assert_eq!(
                    response
                        .headers()
                        .get("connection")
                        .and_then(|v| v.to_str().ok()),
                    Some("close"),
                    "a kept-alive stub connection would be pooled across tests"
                );
                let text = response.text().await.expect("body");
                assert_eq!(text, want);
            }
        }
    }

    /// A thread's clients go over the socket once it registers a stub, never before, and never
    /// after it asked for TCP. Each case on a fresh thread, as libtest gives each test.
    #[test]
    fn client_socket_follows_this_threads_stubs() {
        let on_thread =
            |body: fn() -> Option<bool>| std::thread::spawn(body).join().expect("probe thread");
        assert_eq!(
            on_thread(|| Some(client_socket().is_some())),
            Some(false),
            "a thread with no stub dials TCP, as production does"
        );
        assert_eq!(
            on_thread(|| {
                canned(reply("200 OK", "text/plain", "ok"));
                Some(client_socket().is_some())
            }),
            Some(true),
            "a thread that registered a stub sends over the socket"
        );
        assert_eq!(
            on_thread(|| {
                tcp_clients();
                canned(reply("200 OK", "text/plain", "ok"));
                Some(client_socket().is_some())
            }),
            Some(false),
            "tcp_clients() outlasts a later registration"
        );
    }

    /// Over the Unix socket an HTTP stub and a raw stub both answer at the URL they answer at over
    /// TCP, and a request naming no stub gets the 421 that says why.
    #[tokio::test]
    async fn the_unix_socket_serves_http_and_raw_stubs_and_refuses_the_rest() {
        let http_stub = http(Router::new().fallback(|uri: Uri| async move { format!("h {uri}") }));
        let raw_stub = canned(reply("200 OK", "text/plain", "raw"));
        let client = crate::test_support::loopback::client_builder()
            .unix_socket(client_socket().expect("registering made this thread a socket thread"))
            .build()
            .expect("socket client");
        let get = |url: String| {
            let client = client.clone();
            async move {
                let response = client.get(url).send().await.expect("send over the socket");
                (response.status(), response.text().await.expect("body"))
            }
        };
        assert_eq!(
            get(format!("{}/v1/messages?beta=true", http_stub.url())).await,
            (StatusCode::OK, "h /v1/messages?beta=true".to_string())
        );
        assert_eq!(
            get(format!("{}/v1/messages", raw_stub.url())).await,
            (StatusCode::OK, "raw".to_string())
        );
        let (status, body) = get("http://127.0.0.1:1/v1/messages".to_string()).await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
        assert!(body.contains("tcp_clients()"), "{body}");
    }

    /// The raw half: attempt N is connection N, `None` is a transport failure, and the
    /// counter reads the attempts.
    #[tokio::test]
    async fn scripted_answers_by_connection_ordinal() {
        let ok = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
        let (addr, attempts) = scripted(vec![None, Some(ok.to_string())]);
        let client = crate::test_support::loopback::client();
        let url = format!("{}/v1/messages", addr.url());
        assert!(
            client.get(&url).send().await.is_err(),
            "first connection hangs up"
        );
        let text = client
            .get(&url)
            .send()
            .await
            .expect("second")
            .text()
            .await
            .expect("body");
        assert_eq!(text, "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
