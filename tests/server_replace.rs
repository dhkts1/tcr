//! Replacement must keep the TCP listener bound and existing CONNECT streams
//! alive. Both processes use a scratch HOME, synthetic config and random port.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tcr::log_err::LogErr as _;

struct Proxy {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Proxy {
    fn spawn(home: &Path, port: u16, replace: bool) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tcr"));
        cmd.args([
            "server",
            "--headless",
            "--port",
            &port.to_string(),
            "--config",
        ])
        .arg(home.join("config.json"))
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("TCR_CLAUDE_CODE_CREDENTIALS", home.join("no-login.json"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        if replace {
            cmd.arg("--replace");
        }
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                tx.send(line).log_debug("test log receiver closed");
            }
        });
        Self { child, lines }
    }

    fn ready(&mut self) -> SocketAddr {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            if let Ok(line) = self.lines.recv_timeout(Duration::from_millis(100)) {
                if let Some((_, after)) = line.split_once("listening on http://") {
                    return after.split_whitespace().next().unwrap().parse().unwrap();
                }
                seen.push(line);
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut error = String::new();
                self.child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut error)
                    .unwrap();
                panic!("proxy exited {status} before readiness: {error}; {seen:?}");
            }
        }
        panic!("proxy never became ready: {seen:?}");
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.child.kill().log_debug("test proxy already stopped");
        self.child.wait().log_debug("reaping test proxy");
    }
}

fn scratch_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.json"),
        r#"{
        "proxy": {"port": 0}, "accounts": [], "claudeCodeImport": false,
        "quotaProbeSeconds": 0, "warmupSeconds": 0
    }"#,
    )
    .unwrap();
    home
}

#[test]
fn connections_arriving_during_replace_are_never_refused() {
    let home = scratch_home();
    let mut old = Proxy::spawn(home.path(), 0, false);
    let addr = old.ready();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_client = Arc::clone(&stop);
    let (started_tx, started_rx) = mpsc::channel();
    let clients = std::thread::spawn(move || {
        let mut attempts = 0;
        let mut errors = Vec::new();
        while !stop_client.load(Ordering::Relaxed) {
            // The inherited listener may briefly queue arrivals while the new
            // process boots. A loaded host can take longer than 250 ms; the
            // regression is an unbound socket (ConnectionRefused), not a
            // subsecond startup latency guarantee.
            let response = (|| -> std::io::Result<()> {
                let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(30))?;
                stream.set_read_timeout(Some(Duration::from_secs(30)))?;
                stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                stream.write_all(
                    b"GET /_tcr/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )?;
                let mut status = String::new();
                BufReader::new(stream).read_line(&mut status)?;
                if !status.starts_with("HTTP/1.1 200") {
                    return Err(std::io::Error::other(format!(
                        "unexpected status: {status}"
                    )));
                }
                Ok(())
            })();
            if let Err(error) = response {
                errors.push(error.to_string());
            }
            attempts += 1;
            started_tx.send(()).log_debug("test ready receiver closed");
            std::thread::sleep(Duration::from_millis(2));
        }
        (attempts, errors)
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut successor = Proxy::spawn(home.path(), addr.port(), true);
    let bound = successor.ready();
    stop.store(true, Ordering::Relaxed);
    let (attempts, errors) = clients.join().unwrap();
    let mut health = TcpStream::connect(addr).unwrap();
    health
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    health
        .write_all(b"GET /_tcr/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut status_response = String::new();
    health
        .read_to_string(&mut status_response)
        .expect("successor serves status requests");
    assert!(
        status_response.starts_with("HTTP/1.1 200"),
        "successor status: {status_response}"
    );
    assert_eq!(bound, addr);
    assert!(
        attempts > 1,
        "must attempt connections across the replacement"
    );
    assert!(
        errors.is_empty(),
        "{attempts} connection attempts during --replace: {errors:?}"
    );
}

#[test]
fn a_connect_tunnel_drains_after_the_successor_is_listening() {
    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let echo_addr = echo.local_addr().unwrap();
    let echo_task = std::thread::spawn(move || {
        let (mut stream, _) = echo.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(40)))
            .unwrap();
        let mut buf = [0; 128];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 || stream.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });
    let home = scratch_home();
    let mut old = Proxy::spawn(home.path(), 0, false);
    let addr = old.ready();
    let mut tunnel = TcpStream::connect(addr).unwrap();
    tunnel
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        tunnel,
        "CONNECT {echo_addr} HTTP/1.1\r\nHost: {echo_addr}\r\n\r\n"
    )
    .unwrap();
    let mut response = [0; 39];
    tunnel.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    tunnel.write_all(b"before").unwrap();
    let mut echoed = [0; 6];
    tunnel.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"before");

    let mut successor = Proxy::spawn(home.path(), addr.port(), true);
    assert_eq!(successor.ready(), addr);
    let mut health = TcpStream::connect(addr).unwrap();
    health
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        health,
        "GET /_tcr/status HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut status_response = String::new();
    health
        .read_to_string(&mut status_response)
        .expect("successor serves while predecessor drains");
    assert!(
        status_response.starts_with("HTTP/1.1 200"),
        "successor status: {status_response}"
    );
    tunnel.write_all(b"after!").unwrap();
    tunnel
        .read_exact(&mut echoed)
        .expect("--replace cut the existing CONNECT tunnel");
    assert_eq!(&echoed, b"after!");
    assert!(
        old.child.try_wait().unwrap().is_none(),
        "the predecessor must stay alive while its tunnel drains"
    );
    drop(tunnel);
    echo_task.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = old.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "predecessor did not exit cleanly: {status}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "predecessor did not exit after its tunnel drained"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn predecessor_retains_its_listener_until_the_receiver_acknowledges_it() {
    use std::os::unix::net::UnixStream;
    let home = scratch_home();
    let mut old = Proxy::spawn(home.path(), 0, false);
    let addr = old.ready();
    let endpoint = tcr::restart::socket_path(old.child.id());
    let mut peer = UnixStream::connect(endpoint).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    peer.write_all(b"tcr-replace/2\n").unwrap();
    // Deliberately leave the descriptor queued. A sendmsg return is not proof
    // that recvmsg installed it in the successor's descriptor table.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        old.child.try_wait().unwrap().is_none(),
        "the predecessor exited before the receiver acknowledged the listener"
    );
    let listener = tcr_fdpass::recv_listener(&peer).unwrap();
    assert_eq!(listener.local_addr().unwrap(), addr);
    peer.write_all(b"listener-received\n").unwrap();
    let mut released = [0; b"listener-released\n".len()];
    peer.read_exact(&mut released).unwrap();
    assert_eq!(&released, b"listener-released\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = old.child.try_wait().unwrap() {
            assert!(status.success(), "predecessor exit: {status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "predecessor did not finish the acknowledged handoff"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn replacement_closes_an_idle_http_keep_alive_connection() {
    let home = scratch_home();
    let mut old = Proxy::spawn(home.path(), 0, false);
    let addr = old.ready();
    let mut http = TcpStream::connect(addr).unwrap();
    http.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    http.write_all(b"GET /_tcr/status HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut reader = BufReader::new(http);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"));
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
            length = value.trim().parse().unwrap();
        }
    }
    reader.read_exact(&mut vec![0; length]).unwrap();
    let mut successor = Proxy::spawn(home.path(), addr.port(), true);
    successor.ready();
    assert_eq!(
        reader
            .read(&mut [0; 1])
            .expect("idle h1 must close after handoff"),
        0
    );
}

fn held_tunnel(addr: SocketAddr) -> (TcpStream, TcpStream) {
    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut tunnel = TcpStream::connect(addr).unwrap();
    tunnel
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        tunnel,
        "CONNECT {} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        echo.local_addr().unwrap()
    )
    .unwrap();
    let (upstream, _) = echo.accept().unwrap();
    let mut response = [0; 39];
    tunnel.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    (tunnel, upstream)
}

fn wait_exit(proxy: &mut Proxy, seconds: u64) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = proxy.child.try_wait().unwrap() {
            assert!(status.success(), "shutdown failed: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "predecessor failed to shut down");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_never_ending_tunnel_has_a_drain_ceiling() {
    let home = scratch_home();
    let mut old = Proxy::spawn(home.path(), 0, false);
    let addr = old.ready();
    let _held = held_tunnel(addr);
    let mut successor = Proxy::spawn(home.path(), addr.port(), true);
    successor.ready();
    wait_exit(&mut old, tcr::restart::DRAIN_CEILING.as_secs() + 10);
}

#[test]
fn termination_signals_interrupt_drain_and_shutdown_cleanly() {
    for signal in ["-TERM", "-INT"] {
        let home = scratch_home();
        let mut old = Proxy::spawn(home.path(), 0, false);
        let addr = old.ready();
        let _held = held_tunnel(addr);
        let mut successor = Proxy::spawn(home.path(), addr.port(), true);
        successor.ready();
        assert!(old.child.try_wait().unwrap().is_none());
        assert!(
            Command::new("/bin/kill")
                .args([signal, &old.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        wait_exit(&mut old, 8);
        let lines: Vec<_> = old.lines.try_iter().collect();
        assert!(
            lines.iter().any(|line| line.contains("during drain")),
            "{lines:?}"
        );
    }
}

#[test]
fn a_receiver_that_dies_after_receipt_still_drains_the_predecessor() {
    use std::os::unix::net::UnixStream;
    let home = scratch_home();
    let mut old = Proxy::spawn(home.path(), 0, false);
    let addr = old.ready();
    let (_client, mut upstream) = held_tunnel(addr);
    let mut peer = UnixStream::connect(tcr::restart::socket_path(old.child.id())).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    peer.write_all(b"tcr-replace/2\n").unwrap();
    let listener = tcr_fdpass::recv_listener(&peer).unwrap();
    drop(peer);
    drop(listener);
    // With an EOF instead of a receipt, the old process must drain its live
    // tunnel, then reach the ordinary shutdown path instead of a `?` exit.
    std::thread::sleep(Duration::from_millis(250));
    assert!(old.child.try_wait().unwrap().is_none());
    upstream.write_all(b"alive").unwrap();
    drop(upstream);
    drop(_client);
    wait_exit(&mut old, 8);
    let lines: Vec<_> = old.lines.try_iter().collect();
    assert!(
        lines.iter().any(|line| line.contains("handoff failed")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn replacement_sends_goaway_to_an_idle_http2_client() {
    let home = scratch_home();
    let mut old = Proxy::spawn(home.path(), 0, false);
    let addr = old.ready();
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(stream),
    )
    .await
    .unwrap();
    let connection = tokio::spawn(connection);
    let response = sender
        .send_request(
            hyper::Request::builder()
                .uri(format!("http://{addr}/_tcr/status"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), hyper::StatusCode::OK);
    axum::body::to_bytes(axum::body::Body::new(response.into_body()), 1_000_000)
        .await
        .unwrap();
    let mut successor = Proxy::spawn(home.path(), addr.port(), true);
    successor.ready();
    tokio::time::timeout(Duration::from_secs(5), connection)
        .await
        .expect("h2 client must receive GOAWAY and finish")
        .unwrap()
        .unwrap();
    assert!(sender.is_closed());
}
