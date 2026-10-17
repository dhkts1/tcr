//! Loopback helpers every test reaches for: a client that never goes through an ambient proxy,
//! and a port nothing is listening on.

use std::time::Duration;

/// The port the live proxy serves on, on the machine these tests were written on. A helper here
/// never hands it out, so a test cannot talk to that proxy by accident.
const LIVE_PROXY_PORT: u16 = 3456;

/// A `reqwest` builder that ignores `HTTP_PROXY` and friends, so a loopback request reaches the
/// loopback server and not whatever proxy the shell exports. Add what a test needs (a timeout,
/// a root certificate, a proxy of its own) before `build`.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_proxy()
}

/// [`client_builder`], built. One per test, not one per binary: a `reqwest::Client` keeps its
/// pooled connections on the runtime that opened them, and each `#[tokio::test]` has its own
/// runtime, so a client shared across tests hands the next test a connection whose runtime is
/// gone.
pub fn client() -> reqwest::Client {
    client_builder().build().expect("build a loopback client")
}

/// [`client_builder`] with a whole-request `timeout`, built.
pub fn client_with_timeout(timeout: Duration) -> reqwest::Client {
    client_builder()
        .timeout(timeout)
        .build()
        .expect("build a loopback client")
}

/// A loopback port nothing is listening on: the kernel hands one out, and it is released.
///
/// For a test that needs a REFUSED connection, or a port number to write into a config that a
/// `tcr` child then binds itself. A listener bound in this process cannot be handed to a child,
/// so the second case keeps the small race of a port released and claimed again; a test that
/// can hold a listener should bind `127.0.0.1:0` and keep it.
pub fn unused_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch loopback port");
    let port = listener.local_addr().expect("scratch port address").port();
    assert_ne!(port, LIVE_PROXY_PORT, "never the live proxy's port");
    port
}
