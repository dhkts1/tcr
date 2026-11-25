//! Unit tests for the MITM listener and the CONNECT handling in [`super`].

use std::path::PathBuf;

use super::*;
use crate::log_err::LogErr as _;

#[tokio::test]
async fn a_handoff_serves_already_queued_http_before_draining() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut client, server) = tokio::io::duplex(4096);
    client
        .write_all(b"GET /_tcr/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (_signal, drain) = tokio::sync::watch::channel(true);
    let serving = tokio::spawn(serve_http(
        server,
        "127.0.0.1:12345".parse().unwrap(),
        dummy_manager(),
        None,
        drain,
    ));
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    serving.await.unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "an already queued request must get HTTP 200 during drain, got {} bytes",
        response.len()
    );
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
        // `tcr-codex:k3y`: a keyed Codex launch is still marked, so still pooled.
        "Proxy-Authorization: Basic dGNyLWNvZGV4OmszeQ==",
    ] {
        let head = format!("CONNECT chatgpt.com:443 HTTP/1.1\r\n{header}\r\n\r\n");
        assert!(ProxyCredential::from_connect_head(&head, None).codex_marked);
        assert!(host_intercepted(
            "chatgpt.com",
            1,
            ProxyCredential::from_connect_head(&head, None).codex_marked
        ));
    }
    for header in [
        "",
        "Authorization: Basic dGNyLWNvZGV4Og==",
        "Proxy-Authorization: Basic dGNyLWNvZGV4",
        "Proxy-Authorization: Bearer dGNyLWNvZGV4Og==",
        // `tcr:k3y`: keyed, but not the Codex username.
        "Proxy-Authorization: Basic dGNyOmszeQ==",
    ] {
        let head = format!("CONNECT chatgpt.com:443 HTTP/1.1\r\n{header}\r\n\r\n");
        assert!(
            !ProxyCredential::from_connect_head(&head, None).codex_marked,
            "{header}"
        );
        assert!(!host_intercepted(
            "chatgpt.com",
            1,
            ProxyCredential::from_connect_head(&head, None).codex_marked
        ));
    }
    assert!(codex_proxy_marked("http://tcr-codex@127.0.0.1:3999"));
    assert!(codex_proxy_marked("http://tcr-codex:@127.0.0.1:3999"));
    assert!(codex_proxy_marked("http://tcr-codex:k3y@127.0.0.1:3999"));
    assert!(!codex_proxy_marked("http://tcr:k3y@127.0.0.1:3999"));
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
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        access_token: "at-fake-codex".to_string(),
        refresh_token: Some("rt-fake-codex".to_string()),
        disabled: Some(true),
        ..crate::config::Account::fixture("bob@example.com/codex")
    });
    assert!(
        intercepts_now(&m, "chatgpt.com", true),
        "an imported account enables interception; a disabled pool gets an immediate configuration error"
    );
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
        let held = Arc::clone(&manager);
        let (ready, acquired) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            held.with_config_write_for_test(|| {
                ready.send(()).unwrap();
                released
                    .recv_timeout(Duration::from_secs(2))
                    .log_debug("waiting for the test's release signal");
            });
        });
        acquired.recv().unwrap();
        let start = std::time::Instant::now();
        assert_eq!(intercepts_now(&manager, host, marked), expected);
        let elapsed = start.elapsed();
        release
            .send(())
            .log_debug("releasing the test's config writer");
        holder.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(500),
            "{host} waited {elapsed:?}"
        );
    }
}

/// An echo upstream on an ephemeral loopback port: each connection echoes its first read
/// (up to 64 bytes) and closes. The one copy for every test that needs a real upstream
/// socket, because [`TunnelConnector`] dials a `TcpStream`.
async fn spawn_echo_upstream() -> SocketAddr {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = echo.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = echo.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    stream
                        .write_all(&buf[..n])
                        .await
                        .log_debug("echoing in a test upstream");
                }
            });
        }
    });
    addr
}

/// The blind tunnel pipes bytes to the requested upstream and back, and first
/// answers `200 Connection Established` — proving a non-allowlisted CONNECT
/// (e.g. platform.claude.com) passes straight through instead of being
/// refused, which is what Claude Code needs from the forward proxy.
#[tokio::test]
async fn tunnel_pipes_bytes_through_to_upstream() {
    // Upstream: the shared echo server.
    let echo_port = spawn_echo_upstream().await.port();

    // A front socket standing in for the client<->tcr connection.
    let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front_addr = front.local_addr().unwrap();
    let mut client = TcpStream::connect(front_addr).await.unwrap();
    let (server_side, _) = front.accept().await.unwrap();

    // Tunnel the accepted (tcr) side out to the echo upstream.
    tokio::spawn(async move {
        tunnel(
            server_side,
            "127.0.0.1",
            echo_port,
            &direct_tunnel_connector(),
        )
        .await
        .log_debug("tunnelling in a test");
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

/// A `Manager` with one never-expiring dummy account and no proxy api-key,
/// so a loopback GET on `/_tcr/status` is answered locally with no upstream
/// call and no auth gate — the cheapest real route through `serve_http`'s
/// router to observe which HTTP version `serve_connection` negotiated.
fn dummy_manager() -> Arc<Manager> {
    dummy_manager_at(None)
}

fn dummy_manager_at(config_path: Option<PathBuf>) -> Arc<Manager> {
    dummy_manager_keyed(config_path, None)
}

/// [`dummy_manager_at`], holding `request_key` as `proxy.requestKey`.
fn dummy_manager_keyed(config_path: Option<PathBuf>, request_key: Option<&str>) -> Arc<Manager> {
    let config = crate::config::Config {
        proxy: crate::config::ProxyConfig {
            port: 0,
            api_key: None,
            request_key: request_key.map(str::to_string),
            extra: serde_json::Map::new(),
        },
        upstream: "http://127.0.0.1:1".to_string(),
        ..crate::config::Config::fixture(vec![crate::config::Account::fixture("dummy")])
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
            let manager = Arc::clone(&manager);
            tokio::spawn(async move {
                // Same shape as the production accept loop in
                // `serve_with_shutdown`: a connection error is non-fatal but is
                // never swallowed silently, so a test that fails because the
                // connection died has the reason in the captured log rather
                // than presenting as an unexplained client-side error.
                if let Err(err) = handle_conn(
                    stream,
                    peer,
                    manager,
                    None,
                    direct_tunnel_connector(),
                    tokio::sync::watch::channel(false).1,
                )
                .await
                {
                    tracing::debug!(error = %err, "connection ended with error");
                }
            });
        }
    });
    addr
}

/// Log mode through the real listener: an unkeyed CONNECT is still answered 200 and
/// tunneled, and it bumps the unkeyed counter; a keyed one is answered the same way and
/// counts nothing. The connector dials a held loopback socket, never `example.com`.
#[tokio::test]
async fn an_unkeyed_connect_is_served_and_counted_and_a_keyed_one_is_not_counted() {
    const KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    let upstream_addr = spawn_echo_upstream().await;
    let connector: TunnelConnector =
        Arc::new(move |_host, _port| Box::pin(TcpStream::connect(upstream_addr)));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let manager = dummy_manager_keyed(None, Some(KEY));
    tokio::spawn(serve_with_tunnel_connector(
        listener,
        Arc::clone(&manager),
        None,
        connector,
    ));

    let connect = |auth: Option<String>| async move {
        let mut client = TcpStream::connect(addr).await.unwrap();
        let auth = auth.map_or_else(String::new, |a| format!("Proxy-Authorization: {a}\r\n"));
        let head = format!(
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\
                 User-Agent: curl/8.7.1\r\n{auth}\r\n"
        );
        client.write_all(head.as_bytes()).await.unwrap();
        let mut ok = [0u8; RESP_CONNECT_OK.len()];
        client.read_exact(&mut ok).await.unwrap();
        assert_eq!(&ok, RESP_CONNECT_OK, "log mode answers 200 either way");
        client.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        client.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping", "and tunnels either way");
    };

    // `tcr:<KEY>`, standard base64.
    let keyed = "Basic dGNyOjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTE=";
    connect(Some(keyed.to_string())).await;
    assert!(
        manager.unkeyed().snapshot().is_empty(),
        "a keyed CONNECT counts nothing"
    );

    connect(None).await;
    connect(Some("Basic dGNyOndyb25n".to_string())).await; // `tcr:wrong`
    assert_eq!(
        manager.unkeyed().snapshot(),
        vec![crate::request_key::UnkeyedRow {
            what: Unkeyed::Connect(ConnectClass::Other),
            user_agent: crate::request_key::UaFamily::Curl,
            count: 2,
        }],
        "a missing and a wrong key both count, once each"
    );
}

/// Discriminating control: an ordinary h1 client must still work after the
/// swap to the auto-negotiating builder. If this regresses, the h2 test
/// below is measuring a broken server, not a real negotiation.
#[tokio::test]
async fn base_url_mode_still_serves_http1() {
    let addr = spawn_base_url_listener().await;
    let client = crate::http_client::builder().no_proxy().build().unwrap();
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
    let client = crate::http_client::builder()
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
