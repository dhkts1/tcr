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
        Self::spawn_with_env(home, port, replace, &[])
    }

    fn spawn_with_env(home: &Path, port: u16, replace: bool, env: &[(&str, &str)]) -> Self {
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
        cmd.envs(env.iter().copied());
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
                // The rest of its stdout too: a test binary re-run as this
                // child with a stale `--exact` name says `running 0 tests`
                // there and exits 0, and that line is the diagnosis.
                while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(1)) {
                    seen.push(line);
                }
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
fn incumbent_keeps_serving_after_failed_descriptor_send() {
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    let home = scratch_home();
    let mut old = Proxy::spawn(home.path(), 0, false);
    let addr = old.ready();
    let endpoint = tcr::restart::socket_path(old.child.id());
    let mut peer = UnixStream::connect(&endpoint).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // Refuse incoming descriptors before requesting one. The request still
    // reaches the incumbent, but its send fails without a timing race.
    peer.shutdown(Shutdown::Read).unwrap();
    peer.write_all(b"tcr-replace/2\n").unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    loop {
        if let Ok(line) = old.lines.recv_timeout(Duration::from_millis(100)) {
            let failed = line.contains("listener handoff failed");
            seen.push(line);
            if failed {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "incumbent did not report the failed descriptor send: {seen:?}"
        );
    }
    assert!(
        seen.iter().any(|line| {
            line.contains("listener handoff failed")
                && (line.contains("os error 22") || line.contains("os error 32"))
        }),
        "the descriptor send must fail before testing recovery: {seen:?}"
    );
    drop(peer);

    let second_peer = (|| -> std::io::Result<UnixStream> {
        let mut peer = UnixStream::connect(&endpoint)?;
        peer.set_write_timeout(Some(Duration::from_secs(5)))?;
        peer.shutdown(Shutdown::Read)?;
        peer.write_all(b"tcr-replace/2\n")?;
        Ok(peer)
    })();
    assert!(
        second_peer.is_ok(),
        "incumbent did not answer the second replacement request: {second_peer:?}; {seen:?}"
    );
    let second_peer = second_peer.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(line) = old.lines.recv_timeout(Duration::from_millis(100)) {
            let failed = line.contains("listener handoff failed");
            seen.push(line);
            if failed {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "incumbent did not answer the second replacement request: {seen:?}"
        );
    }
    drop(second_peer);

    // Check continued service after the failed handoff has been processed,
    // including time for an erroneous fall-through to drain and shutdown.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        assert!(
            old.child.try_wait().unwrap().is_none(),
            "incumbent stopped after the failed descriptor send: {seen:?}"
        );
        let response = (|| -> std::io::Result<String> {
            let mut http = TcpStream::connect_timeout(&addr, Duration::from_secs(1))?;
            http.set_read_timeout(Some(Duration::from_secs(1)))?;
            http.set_write_timeout(Some(Duration::from_secs(1)))?;
            http.write_all(
                b"GET /_tcr/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )?;
            let mut status = String::new();
            BufReader::new(http).read_line(&mut status)?;
            Ok(status)
        })();
        assert!(
            matches!(&response, Ok(status) if status.starts_with("HTTP/1.1 200")),
            "incumbent stopped serving HTTP 200 after the failed descriptor send: {response:?}; {seen:?}"
        );
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &old.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_exit(&mut old, 8);
    assert!(
        !endpoint.exists(),
        "incumbent must remove its restart socket"
    );
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

/// The predecessor's ceiling is shortened to 3 s here (the real one is minutes,
/// which a test cannot wait out): it must be gone shortly after it.
#[test]
fn a_never_ending_tunnel_has_a_drain_ceiling() {
    const CEILING: Duration = Duration::from_secs(3);
    let home = scratch_home();
    let ceiling_ms = CEILING.as_millis().to_string();
    let mut old = Proxy::spawn_with_env(
        home.path(),
        0,
        false,
        &[(tcr::restart::DRAIN_CEILING_TEST_ENV, &ceiling_ms)],
    );
    let addr = old.ready();
    let _held = held_tunnel(addr);
    let mut successor = Proxy::spawn(home.path(), addr.port(), true);
    successor.ready();
    wait_exit(&mut old, CEILING.as_secs() + 10);
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

fn assert_stand_down_flushes_the_actual_log_owner() {
    let incumbent_home = scratch_home();
    let mut old = Proxy::spawn(incumbent_home.path(), 0, false);
    let addr = old.ready();
    let home = scratch_home();
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join("config.json")).unwrap())
            .unwrap();
    for index in 0..30000 {
        config[format!("fixture_{}_{index:04}", "x".repeat(40))] = serde_json::json!(true);
    }
    config["zz_fixture_final_log_sentinel"] = serde_json::json!(true);
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    let logs = home.path().join(".cache/tcr/logs");
    std::fs::create_dir_all(&logs).unwrap();
    let date = time::OffsetDateTime::now_utc()
        .format(time::macros::format_description!("[year]-[month]-[day]"))
        .unwrap();
    let fifo = logs.join(format!("tcr.log.{date}"));
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let (release_tx, release_rx) = mpsc::channel();
    let (log_tx, log_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut file = std::fs::File::open(fifo).unwrap();
        release_rx.recv_timeout(Duration::from_secs(120)).unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        log_tx.send(text).unwrap();
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tcr"));
    cmd.args([
        "server",
        "--headless",
        "--port",
        &addr.port().to_string(),
        "--config",
    ])
    .arg(home.path().join("config.json"))
    .env_clear()
    .env("HOME", home.path())
    .env("PATH", "/usr/bin:/bin")
    .env("TCR_CLAUDE_CODE_CREDENTIALS", home.path().join("no-login"))
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    let mut child = tcr::test_support::child::KillOnDrop::spawn(&mut cmd).unwrap();
    let stderr = child.stderr.take().unwrap();
    let (notice_tx, notice_rx) = mpsc::channel();
    let stderr_reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("exiting without binding") {
                notice_tx.send(()).unwrap();
            }
        }
    });
    let notice = notice_rx.recv_timeout(Duration::from_secs(90));
    std::thread::sleep(Duration::from_millis(100));
    release_tx.send(()).unwrap();
    assert!(
        notice.is_ok(),
        "the real server must reach its stand-down exit"
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stand-down did not finish after log reader resumed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = log_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    reader.join().unwrap();
    stderr_reader.join().unwrap();
    assert!(
        text.contains("zz_fixture_final_log_sentinel"),
        "stand-down lost the queued tail; only {} bytes were drained",
        text.len()
    );
    assert!(
        old.child.try_wait().unwrap().is_none(),
        "stand-down must preserve its own fixture incumbent"
    );
}

#[test]
fn a_stand_down_flushes_its_log_and_a_taken_handoff_socket_still_serves() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("TCR_HANDOFF_SOCKET_TAKEN").is_some() {
        let path = tcr::restart::socket_path(std::process::id());
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let listener: std::os::fd::OwnedFd = listener.into();
        // The holder keeps this process's endpoint live across exec. Its input
        // closes when tcr exits, then it removes only this fixture's path.
        let mut holder = Command::new("/bin/sh")
            .args(["-c", "read -r _; /bin/rm -- \"$1\"", "endpoint-holder"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(listener))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let error = Command::new(env!("CARGO_BIN_EXE_tcr"))
            .args(["server", "--headless", "--port", "0", "--config"])
            .arg(Path::new(&std::env::var_os("HOME").unwrap()).join("config.json"))
            .stdin(Stdio::from(holder.stdin.take().unwrap()))
            .exec();
        holder.wait().unwrap();
        panic!("fixture exec failed: {error}");
    }
    assert_stand_down_flushes_the_actual_log_owner();
    let home = scratch_home();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "server_replace::a_stand_down_flushes_its_log_and_a_taken_handoff_socket_still_serves",
            "--nocapture",
        ])
        .env_clear()
        .env("TCR_HANDOFF_SOCKET_TAKEN", "1")
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("TCR_CLAUDE_CODE_CREDENTIALS", home.path().join("no-login"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            tx.send(line).log_debug("fixture receiver closed");
        }
    });
    let mut proxy = Proxy { child, lines };
    let addr = proxy.ready();
    let mut connection = TcpStream::connect(addr).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    connection
        .write_all(b"GET /_tcr/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    BufReader::new(connection).read_line(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "bind failure must preserve serving: {response}"
    );
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &proxy.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_exit(&mut proxy, 15);
    let mut stderr = String::new();
    proxy
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("listener handoff disabled"),
        "bind failure must warn: {stderr}"
    );
    let path = tcr::restart::socket_path(proxy.child.id());
    let deadline = Instant::now() + Duration::from_secs(5);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture endpoint holder did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap();
}

/// `--replace` cannot take over a proxy served from inside another process (an
/// embedded host): it stands down. TcrBar tells a stand-down from a crash only by
/// `singleton::INCUMBENT_MARKER` on stderr, so this path must print it too, or the
/// panel reports "Server exited" for a server that never bound.
#[test]
fn replace_against_an_embedded_proxy_prints_the_stand_down_marker() {
    let home = scratch_home();
    // This test process plays the embedded host: it holds the port and claims it.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let owner_dir = tcr::paths::cache_dir_in(&tcr::paths::Roots {
        home: Some(home.path().to_path_buf()),
        xdg_cache_home: None,
    })
    .affinity_file()
    .parent()
    .unwrap()
    .to_path_buf();
    std::fs::create_dir_all(&owner_dir).unwrap();
    tcr::singleton::write_owner_file(
        &tcr::singleton::owner_path_in(&owner_dir, port),
        &tcr::singleton::ProxyOwner {
            pid: std::process::id(),
            port,
            sha: "0000000".to_string(),
            host: tcr::singleton::ProxyHost::Embedded,
        },
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args([
            "server",
            "--headless",
            "--replace",
            "--port",
            &port.to_string(),
            "--config",
        ])
        .arg(home.path().join("config.json"))
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.path().join("no-login.json"),
        )
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(tcr::singleton::INCUMBENT_MARKER),
        "--replace against an embedded proxy must print the stand-down marker; status {}, \
         stderr: {stderr}",
        output.status
    );
    assert!(
        stderr.contains("served from inside its host application"),
        "the line must name the embedded host, not offer --replace; stderr: {stderr}"
    );
    drop(held);
}
