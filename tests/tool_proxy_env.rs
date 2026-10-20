//! The Claude launch contract, using executable fixtures and real shells.
//! Actual installed-client routing is exercised by test-agent-proxy-isolation.py.
#![expect(clippy::unwrap_used, reason = "test helpers fail with context")]
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tcr::log_err::LogErr as _;

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
        let stopping = Arc::clone(&stop);
        let task = std::thread::spawn(move || {
            let body = serde_json::json!({
                "kind": "tcr.status.v1", "mitm": {"interceptedHosts": ["api.anthropic.com", "chatgpt.com"], "leaf": "minted"},
                "requestKeyMode": "log",
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
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ).log_debug("fixture client disconnected");
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
        self.task.take().unwrap().join().unwrap();
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
    std::fs::create_dir(home.join("codex-home")).unwrap();
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
    // The caller's env file applies its own network values after cleanup.
    std::fs::write(
        home.join("caller env"),
        "
export CALLER_SENTINEL=kept
export HTTPS_PROXY=http://env-file.example:8124
",
    )
    .unwrap();
    script(
        &claude,
        r#"
test "$ANTHROPIC_BASE_URL" = https://api.anthropic.com
test "$HTTPS_PROXY" = "$EXPECTED_AGENT_PROXY"
test "$https_proxy" = "$EXPECTED_AGENT_PROXY"
test -f "$NODE_EXTRA_CA_CERTS"
command='. "$CLAUDE_ENV_FILE"; test "$CALLER_SENTINEL" = kept; test "$HTTPS_PROXY" = http://env-file.example:8124; test -z "${https_proxy:-}"; test -z "${HTTP_PROXY:-}"; test -z "${http_proxy:-}"; test "$ALL_PROXY" = http://proxy.example:3999; test "$all_proxy" = http://proxy.example:3999; test -z "${NODE_EXTRA_CA_CERTS:-}"; test -z "${CODEX_CA_CERTIFICATE:-}"; codex; printf "shell-direct\n"'
exec "$CLAUDE_CODE_SHELL_PREFIX" "set -eu; $command"
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["run", "--config"])
        .arg(home.join("config.json"))
        .env_clear()
        .env("HOME", home)
        .env("CODEX_HOME", home.join("codex-home"))
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
        .env("ALL_PROXY", "http://proxy.example:3999")
        .env("all_proxy", "http://proxy.example:3999")
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

struct AgentFixture {
    home: tempfile::TempDir,
    _status: StatusStub,
}
impl AgentFixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("codex-home")).unwrap();
        let status = StatusStub::new();
        let port = status.listener.local_addr().unwrap().port();
        std::fs::write(home.path().join("config.json"), format!(r#"{{"proxy":{{"port":{port},"requestKey":"fixture-key-one"}},"claudeCodeImport":false,"accounts":[{{"name":"alice@example.com","accessToken":"fixture"}}]}}"#)).unwrap();
        std::fs::create_dir_all(home.path().join(".config")).unwrap();
        tcr::mitm::load_tls_in(&home.path().join(".config")).unwrap();
        Self {
            home,
            _status: status,
        }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tcr"));
        cmd.args(["run", "--config"])
            .arg(self.home.path().join("config.json"))
            .env_clear()
            .env("HOME", self.home.path())
            .env("CODEX_HOME", self.home.path().join("codex-home"))
            .env(
                "PATH",
                self.home.path().join("bin").to_string_lossy().to_string() + ":/usr/bin:/bin",
            )
            .env("TCR_TEST_BIN", env!("CARGO_BIN_EXE_tcr"))
            .env("SHELL", "/bin/bash")
            .env("TCR_CLAUDE_BIN", self.home.path().join("real-claude"))
            .env("TCR_CODEX_BIN", self.home.path().join("real-codex"));
        cmd
    }
    fn run(&self, body: &str) -> std::process::Output {
        script(&self.home.path().join("real-claude"), body);
        let output = self.command().output().unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

#[test]
fn nested_claude_preserves_explicit_overrides() {
    let fixture = AgentFixture::new();
    fixture.run(r#"
if [ "${NESTED:-}" = yes ]; then
    test "$ANTHROPIC_BASE_URL" = http://127.0.0.1:4000
    test "$HTTPS_PROXY" = http://proxy.example:3999
    test "$NODE_EXTRA_CA_CERTS" = /caller/ca.pem
    printf 'overrides-kept\n'
    exit
fi
exec "$CLAUDE_CODE_SHELL_PREFIX" 'NESTED=yes ANTHROPIC_BASE_URL=http://127.0.0.1:4000 HTTPS_PROXY=http://proxy.example:3999 NODE_EXTRA_CA_CERTS=/caller/ca.pem claude'
"#);
    // Cleanup is observable after the foreground client exits.
    let parent = fixture.home.path().join(".cache/tcr/tool-env");
    assert_eq!(std::fs::read_dir(parent).unwrap().count(), 0);
}

#[test]
fn nested_claude_from_codex_has_a_plain_marker_and_claudes_ca() {
    let fixture = AgentFixture::new();
    script(
        &fixture.home.path().join("real-claude"),
        r#"
case "$HTTPS_PROXY" in *tcr-codex*) exit 72;; esac
test "$NODE_EXTRA_CA_CERTS" = "$HOME/.config/tcr-ca.pem"
printf 'plain-claude\n'
"#,
    );
    script(
        &fixture.home.path().join("real-codex"),
        r#"
exec "$CLAUDE_CODE_SHELL_PREFIX" 'claude'
"#,
    );
    let output = fixture.command().arg("--codex").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("plain-claude"));
}

#[test]
fn a_prefix_with_flags_is_refused_before_starting_the_client() {
    let fixture = AgentFixture::new();
    script(&fixture.home.path().join("real-claude"), "exit 79");
    let output = fixture
        .command()
        .env(
            "CLAUDE_CODE_SHELL_PREFIX",
            "/usr/local/bin/audit-wrap --log",
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("executable wrapper script"));
}

#[test]
fn a_callers_codex_bin_overrides_the_nested_launcher_default() {
    let fixture = AgentFixture::new();
    script(&fixture.home.path().join("real-codex"), "exit 78");
    script(
        &fixture.home.path().join("override-codex"),
        "printf 'override-codex\\n'",
    );
    fixture.run(
        r#"
exec "$CLAUDE_CODE_SHELL_PREFIX" 'CODEX_BIN="$HOME/override-codex" codex'
"#,
    );
}

#[test]
fn nested_tcr_commands_skip_the_path_launchers() {
    let fixture = AgentFixture::new();
    std::fs::create_dir(fixture.home.path().join("bin")).unwrap();
    script(
        &fixture.home.path().join("bin/claude"),
        "printf 'real-client\\n'",
    );
    let output = fixture.run(r#"
if [ "${INNER:-}" = yes ]; then exit 73; fi
# Remove explicit binary overrides, forcing the inner tcr to search PATH.
exec "$CLAUDE_CODE_SHELL_PREFIX" 'unset TCR_CLAUDE_BIN; INNER=yes "$TCR_TEST_BIN" run --config "$HOME/config.json"'
"#);
    assert!(String::from_utf8_lossy(&output.stdout).contains("real-client"));
}

#[test]
fn relative_cache_roots_produce_absolute_helpers_and_insecure_roots_are_refused() {
    let fixture = AgentFixture::new();
    script(
        &fixture.home.path().join("real-claude"),
        r#"
case "$CLAUDE_CODE_SHELL_PREFIX" in /*) ;; *) exit 74;; esac
"#,
    );
    let output = fixture
        .command()
        .env("XDG_CACHE_HOME", "relative-cache")
        .current_dir(fixture.home.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let root = fixture.home.path().join("relative-cache/tcr/tool-env");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
    let output = fixture
        .command()
        .env("XDG_CACHE_HOME", "relative-cache")
        .current_dir(fixture.home.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mode 0700"));
}

#[test]
fn nested_tcr_codex_run_uses_its_own_config_and_real_binary() {
    let fixture = AgentFixture::new();
    let inner = StatusStub::new();
    let inner_port = inner.listener.local_addr().unwrap().port();
    std::fs::write(
        fixture.home.path().join("other.json"),
        format!(r#"{{"proxy":{{"port":{inner_port}}},"accounts":[],"claudeCodeImport":false}}"#),
    )
    .unwrap();
    std::fs::create_dir(fixture.home.path().join("bin")).unwrap();
    script(
        &fixture.home.path().join("bin/codex"),
        &format!(
            "test \"$HTTPS_PROXY\" = 'http://tcr-codex@127.0.0.1:{inner_port}'\nprintf 'inner-config-kept\\n'"
        ),
    );
    script(&fixture.home.path().join("real-codex"), "exit 75");
    let output = fixture.run(r#"
exec "$CLAUDE_CODE_SHELL_PREFIX" 'unset TCR_CODEX_BIN; "$TCR_TEST_BIN" run --codex --config "$HOME/other.json"'
"#);
    assert!(String::from_utf8_lossy(&output.stdout).contains("inner-config-kept"));
}

#[test]
fn helper_files_never_contain_the_runtime_request_key() {
    let fixture = AgentFixture::new();
    fixture.run(
        r#"
root=$(dirname "$CLAUDE_CODE_SHELL_PREFIX")
for helper in "$root"/*; do
    if /usr/bin/grep -q fixture-key-one "$helper"; then exit 71; fi
done
"#,
    );
}

#[test]
fn review2_shell_boundaries() {
    let fixture = AgentFixture::new();
    let mut failures = Vec::new();
    script(
        &fixture.home.path().join("real-claude"),
        r#"
if [ "${INNER:-}" = yes ]; then
    test "$HTTPS_PROXY" = "$EXPECTED_MODEL"
    test "$NODE_EXTRA_CA_CERTS" = "$HOME/.config/tcr-ca.pem"
    printf 'nested-pooled\n'; exit
fi
exec "$CLAUDE_CODE_SHELL_PREFIX" 'INNER=yes claude'
"#,
    );
    let fixture_config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.home.path().join("config.json")).unwrap())
            .unwrap();
    let port = fixture_config["proxy"]["port"].as_u64().unwrap();
    let output = fixture
        .command()
        .env("HTTPS_PROXY", "http://corporate.example:8123")
        .env("NODE_EXTRA_CA_CERTS", "/caller/ca.pem")
        .env(
            "EXPECTED_MODEL",
            format!("http://tcr:fixture-key-one@127.0.0.1:{port}/"),
        )
        .output()
        .unwrap();
    if !output.status.success() {
        failures.push("M2 restored proxy/CA bypass nested pooling");
    }
    let output = fixture
        .command()
        .env("NODE_EXTRA_CA_CERTS", "/caller/ca.pem")
        .env(
            "EXPECTED_MODEL",
            format!("http://tcr:fixture-key-one@127.0.0.1:{port}/"),
        )
        .output()
        .unwrap();
    if !output.status.success() {
        failures.push("M2 restored CA breaks nested trust");
    }
    std::fs::write(
        fixture.home.path().join("caller.env"),
        "export HTTPS_PROXY=http://env-file.example:8124\nexport NODE_EXTRA_CA_CERTS=/env/ca.pem\n",
    )
    .unwrap();
    script(
        &fixture.home.path().join("real-claude"),
        r#"exec "$CLAUDE_CODE_SHELL_PREFIX" '. "$CLAUDE_ENV_FILE"; test "$HTTPS_PROXY" = http://env-file.example:8124; test "$NODE_EXTRA_CA_CERTS" = /env/ca.pem'"#,
    );
    let output = fixture
        .command()
        .env("CLAUDE_ENV_FILE", fixture.home.path().join("caller.env"))
        .output()
        .unwrap();
    if !output.status.success() {
        failures.push("M3 env-file values lost after sourcing");
    }
    script(
        &fixture.home.path().join("real-codex"),
        "printf 'codex-ok\\n'",
    );
    let output = fixture
        .command()
        .arg("--codex")
        .env("CLAUDE_CODE_SHELL_PREFIX", "/audit --flag")
        .output()
        .unwrap();
    if !output.status.success() {
        failures.push("S2 codex refused Claude prefix flags");
    }
    let output = fixture.command().arg("--codex").output().unwrap();
    let notices = String::from_utf8_lossy(&output.stderr)
        .matches("shell tools use the caller's")
        .count();
    if notices != 1 {
        failures.push("N3 duplicate shell routing notices");
    }
    let wrap = fixture.home.path().join("audit -wrapper");
    script(&wrap, "exec /bin/bash -c \"$1\"");
    script(
        &fixture.home.path().join("real-claude"),
        r#"exec "$CLAUDE_CODE_SHELL_PREFIX" 'printf wrapper-ok'"#,
    );
    let output = fixture
        .command()
        .env("CLAUDE_CODE_SHELL_PREFIX", &wrap)
        .output()
        .unwrap();
    if !output.status.success() {
        failures.push("S2 executable space-dash path refused");
    }
    let stale = fixture
        .home
        .path()
        .join(".cache/tcr/tool-env/launch-gone/shell-prefix");
    let output = fixture
        .command()
        .env("CLAUDE_CODE_SHELL_PREFIX", stale)
        .output()
        .unwrap();
    if !output.status.success() {
        failures.push("S3 stale helper executed as caller wrapper");
    }
    let alias = fixture.home.path().join("tcr-run");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_tcr"), &alias).unwrap();
    script(
        &fixture.home.path().join("real-claude"),
        r#"if [ "${ALIAS_STAGE:-}" = nested ]; then exit 76; fi; export ALIAS_STAGE=nested; exec "$CLAUDE_CODE_SHELL_PREFIX" 'codex'"#,
    );
    let cmd = fixture.command();
    // Reuse the complete synthetic environment, but enter through the alias.
    let mut aliased = Command::new(alias);
    aliased
        .arg("--config")
        .arg(fixture.home.path().join("config.json"));
    aliased.env_clear();
    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            aliased.env(k, v);
        }
    }
    let output = aliased.output().unwrap();
    if !output.status.success() {
        failures.push("M5 tcr-run nested codex argv is doubled");
    }
    assert!(failures.is_empty(), "shell boundary defects: {failures:?}");
}
