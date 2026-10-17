//! Fake upstreams for unit tests, all served from two loopback listeners per test binary.
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
//! routes per REQUEST: a client pools connections by host and port, and every stub now shares
//! both, so one kept-alive connection may carry requests for two stubs in turn. A raw stub owns
//! its whole CONNECTION (it writes bytes, sleeps, or hangs up), so it is routed once, by the first
//! request on it, and every raw reply ends that connection.
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
pub(crate) struct StubAddr {
    listener: SocketAddr,
    id: u64,
}

impl StubAddr {
    /// The base URL to configure as an upstream: `http://127.0.0.1:<port>/stub/<id>`.
    pub(crate) fn url(&self) -> String {
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
pub(crate) struct RawConn {
    /// The socket, positioned just after [`RawConn::head`].
    pub(crate) stream: TcpStream,
    /// What one read of the request returned (at least its request line), prefix included.
    pub(crate) head: Vec<u8>,
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
    match router.oneshot(req).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

async fn serve_raw(listener: tokio::net::TcpListener) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            // The request line is all routing needs; one read nearly always holds it.
            while !head.windows(2).any(|pair| pair == b"\r\n") {
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
pub(crate) fn http(router: Router) -> StubAddr {
    register(Route::Http(router), false)
}

/// Hand every connection whose first request names this stub to `handler`.
pub(crate) fn raw<F, Fut>(handler: F) -> StubAddr
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
pub(crate) fn scripted(script: Vec<Option<String>>) -> (StubAddr, Arc<AtomicUsize>) {
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
pub(crate) fn reply(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// [`scripted`] with one entry: every connection gets `raw`.
pub(crate) fn canned(raw: impl Into<String>) -> StubAddr {
    scripted(vec![Some(raw.into())]).0
}

/// [`raw`] for a stub that reads the request, waits `delay`, then writes `response` and closes.
pub(crate) fn delayed(response: String, delay: std::time::Duration) -> StubAddr {
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
pub(crate) fn stalled(partial: String) -> StubAddr {
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

    /// Two stubs share one port, and a pooled client that reuses a connection across them still
    /// reaches each stub's own router with the path it was sent.
    #[tokio::test]
    async fn two_http_stubs_on_one_pooled_connection_each_see_their_own_path() {
        let a = http(Router::new().fallback(|uri: Uri| async move { format!("a {uri}") }));
        let b = http(Router::new().fallback(|uri: Uri| async move { format!("b {uri}") }));
        assert_eq!(
            a.listener, b.listener,
            "one listener serves every HTTP stub"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        for _ in 0..2 {
            for (stub, want) in [
                (a, "a /v1/messages?beta=true"),
                (b, "b /v1/messages?beta=true"),
            ] {
                let text = client
                    .get(format!("{}/v1/messages?beta=true", stub.url()))
                    .send()
                    .await
                    .expect("send")
                    .text()
                    .await
                    .expect("body");
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
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
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
