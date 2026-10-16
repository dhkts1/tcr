//! The Claude launch contract, using executable fixtures and real shells.
//! Actual installed-client routing is exercised by test-agent-proxy-isolation.py.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

struct StatusStub {
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl StatusStub {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let serving = listener.try_clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let task = std::thread::spawn(move || {
            let body = serde_json::json!({
                "kind": "tcr.status.v1", "mitm": {"interceptedHosts": ["api.anthropic.com", "chatgpt.com"], "leaf": "minted"},
                "accounts": [{"name": "bob@example.com", "provider": "codex", "priority": 0,
                    "status": "active", "disabled": false, "requests": 0, "inputTokens": 0,
                    "outputTokens": 0, "cacheReadTokens": 0, "cacheCreationTokens": 0,
                    "probeStatus": "never", "quotaState": "normal", "gate": "ok", "threshold": 0.9}]
            }).to_string();
            while !stopping.load(Ordering::Relaxed) {
                let (mut stream, _) = match serving.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("status stub: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut head = Vec::new();
                let mut buf = [0; 1024];
                while head.len() < 16_384 && !head.windows(4).any(|v| v == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                if head.is_empty() {
                    continue;
                }
                assert!(head.starts_with(b"GET /_tcr/status "));
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self {
            listener,
            stop,
            task: Some(task),
        }
    }
}

impl Drop for StatusStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.task.take().unwrap().join();
    }
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn claude_shells_go_direct_and_nested_codex_regains_the_pool() {
    // Spaces and quotes exercise generated shell paths, without accessing any login.
    let scratch = tempfile::Builder::new()
        .prefix("tcr tools ' ")
        .tempdir()
        .unwrap();
    let home = scratch.path();
    let status = StatusStub::new();
    let port = status.listener.local_addr().unwrap().port();
    std::fs::write(
        home.join("config.json"),
        format!(
            r#"{{
        "proxy": {{"port": {port}}}, "claudeCodeImport": false,
        "accounts": [{{"name": "alice@example.com", "accessToken": "fixture-claude"}},
                     {{"name": "bob@example.com", "provider": "codex", "accessToken": "fixture-codex"}}]
    }}"#
        ),
    )
    .unwrap();
    // The client now reads existing CA material; only server startup may mint it.
    let tls_dir = home.join(".config");
    std::fs::create_dir_all(&tls_dir).unwrap();
    tcr::mitm::load_tls_in(&tls_dir).unwrap();
    let claude = home.join("real claude");
    let codex = home.join("real codex");
    script(
        &codex,
        r#"
test "$HTTPS_PROXY" = "$EXPECTED_PROXY"
test "$https_proxy" = "$EXPECTED_PROXY"
test -f "$CODEX_CA_CERTIFICATE"
printf 'nested-codex-pooled\n'
"#,
    );
    script(
        &home.join("caller prefix"),
        r#"
test -z "${HTTPS_PROXY:-}" || { printf "shell still inherited HTTPS_PROXY\n" >&2; exit 77; }
printf 'caller-prefix-preserved\n'
exec /bin/bash -c "$1"
"#,
    );
    // The caller's env file must still run, but cannot reintroduce tcr's proxy.
    std::fs::write(
        home.join("caller env"),
        r#"
export CALLER_SENTINEL=kept
export HTTPS_PROXY="$EXPECTED_PROXY"
"#,
    )
    .unwrap();
    script(
        &claude,
        r#"
test "$ANTHROPIC_BASE_URL" = https://api.anthropic.com
test "$HTTPS_PROXY" = "$EXPECTED_AGENT_PROXY"
test "$https_proxy" = "$EXPECTED_AGENT_PROXY"
test -f "$NODE_EXTRA_CA_CERTS"
command='. "$CLAUDE_ENV_FILE"; test "$CALLER_SENTINEL" = kept; test -z "${HTTPS_PROXY:-}"; test -z "${https_proxy:-}"; test -z "${HTTP_PROXY:-}"; test -z "${http_proxy:-}"; test -z "${ALL_PROXY:-}"; test -z "${all_proxy:-}"; codex; printf "shell-direct\n"'
exec "$CLAUDE_CODE_SHELL_PREFIX" "set -eu; $command"
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["run", "--config"])
        .arg(home.join("config.json"))
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("SHELL", "/bin/bash")
        .env("TCR_CLAUDE_BIN", &claude)
        .env("TCR_CODEX_BIN", &codex)
        .env("TCR_CLAUDE_CODE_CREDENTIALS", home.join("absent"))
        .env("CLAUDE_CODE_SHELL_PREFIX", home.join("caller prefix"))
        .env("CLAUDE_ENV_FILE", home.join("caller env"))
        .env(
            "EXPECTED_PROXY",
            format!("http://tcr-codex@127.0.0.1:{port}"),
        )
        .env("EXPECTED_AGENT_PROXY", format!("http://127.0.0.1:{port}"))
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("all_proxy", "http://127.0.0.1:1")
        .current_dir(home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "caller-prefix-preserved",
        "nested-codex-pooled",
        "shell-direct",
    ] {
        assert!(out.contains(expected), "missing {expected}: {out}");
    }
}
