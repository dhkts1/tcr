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

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
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

/// One accepted connection on the raw listener, handed to the stub its first request named.
pub struct RawConn {
    /// The socket, positioned just after [`RawConn::head`].
    pub stream: TcpStream,
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
    routes: Mutex<HashMap<u64, Route>>,
    next: AtomicU64,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(start)
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
                tokio::spawn(serve_raw(raw));
                axum::serve(http, Router::new().fallback(dispatch_http))
                    .await
                    .log_debug("serving the shared HTTP stub listener");
            });
        })
        .expect("spawn the stub upstream thread");
    Shared {
        http: addrs.0,
        raw: addrs.1,
        routes: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
    }
}

fn register(route: Route, raw: bool) -> StubAddr {
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
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            // The request line is all routing needs, but read the whole head: a stub that
            // answered and closed with request bytes still unread would reset the connection
            // under its own reply. One read nearly always holds it all.
            while !head.windows(4).any(|quad| quad == b"\r\n\r\n") {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let line = String::from_utf8_lossy(&head);
            let target = line.split_whitespace().nth(1).unwrap_or("");
            let handler = split_route(target).and_then(|(id, _)| {
                match shared()
                    .routes
                    .lock()
                    .expect("stub routes poisoned")
                    .get(&id)
                {
                    Some(Route::Raw(handler)) => Some(Arc::clone(handler)),
                    _ => None,
                }
            });
            match handler {
                Some(handler) => handler(RawConn { stream, head }).await,
                None => {
                    stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .await
                        .log_debug("answering an unrouted raw stub request");
                }
            }
        });
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
