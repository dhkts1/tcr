//! Replacement must keep the TCP listener bound and existing CONNECT streams
//! alive. Both processes use a scratch HOME, synthetic config and random port.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

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
                let _ = tx.send(line);
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
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    let stop_client = stop.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let clients = std::thread::spawn(move || {
        let mut attempts = 0;
        let mut errors = Vec::new();
        while !stop_client.load(Ordering::Relaxed) {
            // The inherited listener may briefly queue arrivals while the new
            // process boots. A loaded host can take longer than 250 ms; the
            // regression is an unbound socket (ConnectionRefused), not a
            // subsecond startup latency guarantee.
            if let Err(error) = TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
                errors.push(error.kind());
            }
            attempts += 1;
            let _ = started_tx.send(());
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
    let endpoint = home
        .path()
        .join(".cache")
        .join(tcr::paths::CACHE_DIR)
        .join(format!("restart-{}.sock", old.child.id()));
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
