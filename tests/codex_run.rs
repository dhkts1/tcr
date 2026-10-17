//! Codex launch and upgrade diagnostics through the built CLI. Each fixture owns
//! its HOME, `CODEX_HOME` and loopback status stub; the fake codex prints argv and
//! never sends a request. Both configured upstreams fail closed on loopback.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct Fixture {
    home: tempfile::TempDir,
    config: PathBuf,
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(current: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 3456);
        listener.set_nonblocking(true).unwrap();
        let config = home.path().join(".config").join(tcr::paths::CONFIG_FILE);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(
            &config,
            serde_json::json!({
                "proxy": {"port": port},
                "upstream": "http://127.0.0.1:0",
                "codexUpstream": "http://127.0.0.1:0",
                "accounts": [{
                    "name": "alice@example.com/codex", "provider": "codex", "credentialVersion": 2,
                    "accountUuid": "11111111-1111-1111-1111-111111111111",
                    "orgUuid": "22222222-2222-2222-2222-222222222222",
                    "accessToken": "at-fake", "refreshToken": "rt-fake", "expiresAt": 4_102_444_800_000_i64
                }]
            }).to_string(),
        ).unwrap();
        std::fs::write(
            home.path().join(".config/tcr-ca.pem"),
            "fixture CA, never used for TLS",
        )
        .unwrap();
        let mut payload = serde_json::json!({
            "kind": "tcr.status.v1", "build": {"sha": "abc1234"}, "accounts": []
        });
        if current {
            payload["mitm"] =
                serde_json::json!({"interceptedHosts": ["chatgpt.com"], "leaf":"minted"});
            payload["accounts"] = serde_json::json!([{
                "name":"alice@example.com/codex", "provider":"codex", "priority":0,
                "status":"active", "disabled":false, "requests":0, "inputTokens":0,
                "outputTokens":0, "cacheReadTokens":0, "cacheCreationTokens":0,
                "probeStatus":"never", "quotaState":"normal", "gate":"ok", "threshold":0.9
            }]);
        }
        let body = payload.to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept fixture request: {error}"),
                };
                // Darwin inherits the listener's O_NONBLOCK flag on accept.
                // The request may arrive after accept returns, so a WouldBlock
                // must not be mistaken for a client that closed its socket.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let mut head = Vec::new();
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                let mut buffer = [0; 1024];
                while head.len() < 16_384 && !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => head.extend_from_slice(&buffer[..count]),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) && std::time::Instant::now() < deadline =>
                        {
                            continue;
                        }
                        Err(error) => panic!("read fixture status request: {error}"),
                    }
                }
                if head.is_empty() {
                    continue;
                }
                assert!(
                    head.windows(4).any(|bytes| bytes == b"\r\n\r\n"),
                    "fixture receives a complete request head"
                );
                assert!(
                    head.starts_with(b"GET /_tcr/status "),
                    "only status reaches the stub"
                );
                write!(stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("write fixture status response");
            }
        });
        Self {
            home,
            config,
            port,
            stop,
            worker: Some(worker),
        }
    }

    fn command(&self) -> Command {
        let mut command = crate::tcr_env::tcr(self.home.path());
        command
            .env("CODEX_HOME", self.home.path().join(".codex"))
            .env("TCR_CODEX_AUTH", self.home.path().join("no-such-auth.json"))
            .env("TCR_CODEX_BIN", self.home.path().join("fake-codex"))
            .env("NO_COLOR", "1")
            .current_dir(self.home.path());
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.worker.take().unwrap().join();
        if !std::thread::panicking() {
            result.expect("status fixture worker");
        }
    }
}

fn details(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn the_codex_status_stub_accepts_delayed_request_headers() {
    let fixture = Fixture::new(true);
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
    std::thread::sleep(Duration::from_millis(350));
    stream
        .write_all(b"GET /_tcr/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "the stub tolerates scheduling delays"
    );
}

#[test]
fn a_pooled_codex_launch_gives_shell_tools_unmarked_proxy_defaults() {
    let fixture = Fixture::new(true);
    let codex = fixture.home.path().join("fake-codex");
    std::fs::write(&codex, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let before = std::fs::read(&fixture.config).unwrap();
    let caller_override = "shell_environment_policy.set.HTTPS_PROXY=\"http://proxy.example:3999\"";
    let output = fixture
        .command()
        .args(["run", "--codex", "--config"])
        .arg(&fixture.config)
        .args(["--", "exec", "--json", "-c", caller_override, "hello"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", details(&output));
    let actual: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        actual,
        [
            "-c".to_string(),
            format!(
                "shell_environment_policy.set.HTTPS_PROXY=\"http://127.0.0.1:{}\"",
                fixture.port
            ),
            "-c".to_string(),
            format!(
                "shell_environment_policy.set.https_proxy=\"http://127.0.0.1:{}\"",
                fixture.port
            ),
            "exec".to_string(),
            "--json".to_string(),
            "-c".to_string(),
            caller_override.to_string(),
            "hello".to_string(),
        ],
        "{}",
        details(&output)
    );
    assert_eq!(
        std::fs::read(&fixture.config).unwrap(),
        before,
        "launch defaults never rewrite configuration"
    );
}

#[test]
fn an_older_proxy_keeps_the_configured_codex_status_verdict() {
    let fixture = Fixture::new(false);
    let output = fixture
        .command()
        .args(["status", "--config"])
        .arg(&fixture.config)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{}", details(&output));
    assert!(text.contains("status source=live"), "{}", details(&output));
    assert!(
        text.contains("predates Codex interception"),
        "{}",
        details(&output)
    );
    assert!(
        text.contains("restart it on this build"),
        "{}",
        details(&output)
    );
}

#[test]
fn an_older_proxy_keeps_the_configured_codex_doctor_verdict() {
    let fixture = Fixture::new(false);
    let output = fixture
        .command()
        .args(["doctor", "--config"])
        .arg(&fixture.config)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("predates Codex interception"),
        "{}",
        details(&output)
    );
    assert!(
        text.contains("restart it on this build"),
        "{}",
        details(&output)
    );
}
