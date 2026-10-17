//! `tcr migrate` end to end, against the BUILT BINARY and a scratch server.
//!
//! Two contracts only the binary can show. The refusal: while a tcr server is
//! running on the configured port, `tcr migrate` exits 3 and moves nothing,
//! because the server resolved its paths at boot and would keep writing the
//! old file. And the hand-off: after a migrate on a HOME that held only the
//! old names, a fresh server reads the new names, claims the port from the new
//! cache directory and lists its accounts.
//!
//! And the negative: a server start alone moves nothing.
//!
//! # Isolation
//!
//! Same rules as `tests/legacy_install_boot.rs`: `HOME` is a fresh
//! [`tempfile::TempDir`], `XDG_CACHE_HOME` is removed, both on the spawned
//! `Command`. The scratch server binds a port the kernel just handed out, never
//! the live proxy's 3456, and runs under the test network guard, which refuses
//! 3456 and every non-loopback connection before it is made. Every account is
//! invented.
use crate::connect_guard;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tcr::{legacy_names, paths, singleton::owner_path_in};

const BOOT_TIMEOUT: Duration = Duration::from_secs(90);
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);

/// A port nothing holds right now, for the child `tcr server` to bind itself.
use tcr::test_support::loopback::unused_port as free_port;

/// A HOME holding only the old names: the config (on `port`, one invented
/// account) and a cache directory with a marker file and an old-prefix log.
fn old_install(port: u16) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("a scratch HOME");
    let config_dir = home.path().join(".config");
    std::fs::create_dir_all(&config_dir).expect("mkdir .config");
    std::fs::write(
        config_dir.join(legacy_names::CONFIG_FILE),
        format!(
            r#"{{ "proxy": {{ "port": {port} }}, "upstream": "http://127.0.0.1:0",
                 "codexUpstream": "http://127.0.0.1:0",
                 "accounts": [{{ "name": "alice@example.com", "type": "oauth",
                                 "accessToken": "at-alice", "expiresAt": 4102444800000 }}] }}"#
        ),
    )
    .expect("write the old config");
    let logs = old_cache(home.path()).join("logs");
    std::fs::create_dir_all(&logs).expect("mkdir the old cache");
    std::fs::write(old_cache(home.path()).join("marker"), "kept").expect("write a marker");
    std::fs::write(
        logs.join(format!("{}.2026-10-01", legacy_names::LOG_PREFIX)),
        "an old log line\n",
    )
    .expect("write an old log");
    home
}

fn old_config(home: &Path) -> PathBuf {
    home.join(".config").join(legacy_names::CONFIG_FILE)
}

fn new_config(home: &Path) -> PathBuf {
    home.join(".config").join(paths::CONFIG_FILE)
}

fn old_cache(home: &Path) -> PathBuf {
    home.join(".cache").join(legacy_names::CACHE_DIR)
}

fn new_cache(home: &Path) -> PathBuf {
    home.join(".cache").join(paths::CACHE_DIR)
}

/// The built `tcr` isolated to `home`: no real config, cache, Keychain login or Codex home.
fn isolated_tcr(home: &Path) -> Command {
    let mut cmd = crate::tcr_env::tcr(home);
    cmd.env("CODEX_HOME", home.join(".codex"))
        .env("TCR_CODEX_AUTH", home.join("no-such-codex-auth.json"))
        .env("NO_COLOR", "1")
        .current_dir(home);
    cmd
}

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Out {
    fn says(&self) -> String {
        format!(
            "exit {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        )
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {}", self.says()))
    }
}

/// Run the built `tcr` under `home` and the network guard, and require that it
/// tried no connection the guard forbids (3456, or anything off loopback).
fn tcr(home: &Path, args: &[&str]) -> Out {
    let mut cmd = isolated_tcr(home);
    cmd.args(args);
    let guard = connect_guard::ConnectGuard::attach(&mut cmd, home);
    let out = cmd.output().expect("run the built tcr");
    guard.assert_no_connections();
    Out {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A scratch server on the port its config names, stopped on drop.
struct Server {
    child: Child,
    _guard: connect_guard::ConnectGuard,
}

impl Server {
    /// Boot `tcr server --headless` under `home` and wait for its listening line.
    fn boot(home: &Path, port: u16) -> Self {
        let mut cmd = isolated_tcr(home);
        cmd.args(["server", "--headless", "--no-replace"]);
        let guard = connect_guard::ConnectGuard::attach(&mut cmd, home);
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the built tcr server");
        let stdout = child.stdout.take().expect("stdout was piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let server = Self {
            child,
            _guard: guard,
        };
        let deadline = Instant::now() + BOOT_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "tcr never printed a 'listening on' line within {BOOT_TIMEOUT:?}"
            );
            match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Ok(line) if line.contains("listening on http://") => {
                    assert!(
                        line.contains(&format!(":{port}")),
                        "the server must bind the configured port {port}: {line}"
                    );
                    break;
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("tcr exited before it printed a listening line")
                }
            }
        }
        // Keep draining so a full pipe never blocks the server.
        std::thread::spawn(move || while rx.recv().is_ok() {});
        server
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let pid = self.child.id().to_string();
        let signalled = Command::new("kill").args(["-TERM", &pid]).status();
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while signalled.as_ref().is_ok_and(|s| s.success()) && Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Err(err) = self.child.kill() {
            eprintln!("could not kill the scratch server (pid {pid}): {err}");
        }
        if let Err(err) = self.child.wait() {
            eprintln!("could not reap the scratch server (pid {pid}): {err}");
        }
    }
}

/// A server start moves nothing: the old names are still there after a boot,
/// and no new name appears.
///
/// `tests/legacy_install_boot.rs` asserts the same, but its config names no
/// port, so a migration wrongly run at boot would look at 3456; on a machine
/// with a live proxy there it refuses and that test passes anyway. Here the
/// config names the scratch server's own free port, so nothing else on this
/// machine can stop a misplaced migration from moving the files.
#[test]
fn a_server_start_alone_moves_nothing() {
    let port = free_port();
    let home = old_install(port);
    let server = Server::boot(home.path(), port);
    assert!(
        owner_path_in(&old_cache(home.path()), port).is_file(),
        "the running server claims the port from the OLD cache directory"
    );
    drop(server);
    assert!(old_config(home.path()).is_file(), "the old config stays");
    assert!(old_cache(home.path()).is_dir(), "the old cache stays");
    assert!(!new_config(home.path()).exists(), "no new config appears");
    assert!(!new_cache(home.path()).exists(), "no new cache appears");
}

/// While a server runs on the configured port, `tcr migrate` refuses with
/// exit 3 and one plain line, moves nothing, and says so in `--json`.
#[test]
fn migrate_refuses_while_a_server_runs_and_moves_nothing() {
    let port = free_port();
    let home = old_install(port);
    let server = Server::boot(home.path(), port);

    let json = tcr(home.path(), &["migrate", "--json"]);
    assert_eq!(json.code, Some(3), "{}", json.says());
    let doc = json.json();
    assert_eq!(doc["outcome"], "refused", "{}", json.says());
    assert_eq!(doc["refusal"]["port"], port, "{}", json.says());
    assert_eq!(doc["refusal"]["pid"], server.pid(), "{}", json.says());

    let plain = tcr(home.path(), &["migrate"]);
    assert_eq!(plain.code, Some(3), "{}", plain.says());
    assert_eq!(
        plain.stderr.trim().lines().count(),
        1,
        "one plain line: {}",
        plain.says()
    );
    assert!(
        plain.stderr.contains("tcr server is running"),
        "{}",
        plain.says()
    );

    assert!(old_config(home.path()).is_file(), "the old config stays");
    assert!(!new_config(home.path()).exists(), "no new config appears");
    assert!(old_cache(home.path()).is_dir(), "the old cache stays");
    assert!(!new_cache(home.path()).exists(), "no new cache appears");
    drop(server);
}

/// After a migrate on a HOME that held only the old names, nothing old is
/// left, a second run moves nothing, and a fresh server reads the new names:
/// it claims the port from the new cache directory and lists its account.
#[test]
fn after_a_migrate_a_server_reads_the_new_names_and_lists_its_accounts() {
    let port = free_port();
    let home = old_install(port);

    let first = tcr(home.path(), &["migrate", "--json"]);
    assert_eq!(first.code, Some(0), "{}", first.says());
    let doc = first.json();
    assert_eq!(doc["outcome"], "moved", "{}", first.says());
    assert_eq!(doc["units"][0]["result"], "moved", "{}", first.says());
    assert_eq!(doc["units"][1]["result"], "moved", "{}", first.says());
    assert_eq!(doc["logs"][0]["result"], "renamed", "{}", first.says());

    assert!(!old_config(home.path()).exists());
    assert!(new_config(home.path()).is_file());
    assert!(!old_cache(home.path()).exists());
    assert_eq!(
        std::fs::read_to_string(new_cache(home.path()).join("marker")).expect("the marker moved"),
        "kept"
    );
    assert!(
        new_cache(home.path())
            .join("logs")
            .join(format!("{}.2026-10-01", paths::LOG_PREFIX))
            .is_file()
    );

    let second = tcr(home.path(), &["migrate", "--json"]);
    assert_eq!(second.code, Some(0), "{}", second.says());
    assert_eq!(
        second.json()["outcome"],
        "nothing_to_move",
        "{}",
        second.says()
    );
    let plain = tcr(home.path(), &["migrate"]);
    assert_eq!(plain.code, Some(0), "{}", plain.says());
    assert!(plain.stdout.contains("nothing to move"), "{}", plain.says());

    let server = Server::boot(home.path(), port);
    assert!(
        owner_path_in(&new_cache(home.path()), port).is_file(),
        "the port claim lands in the NEW cache directory"
    );
    let status = tcr(home.path(), &["status", "--json"]);
    assert_eq!(status.code, Some(0), "{}", status.says());
    let rows: Vec<Value> = serde_json::from_str(&status.stdout)
        .unwrap_or_else(|e| panic!("status --json is a row array ({e}): {}", status.says()));
    let alice: Vec<&Value> = rows
        .iter()
        .filter(|r| r["name"] == "alice@example.com")
        .collect();
    assert_eq!(alice.len(), 1, "{}", status.says());
    assert_eq!(
        alice[0]["source"],
        "live",
        "the running server answered: {}",
        status.says()
    );
    drop(server);
    assert!(
        !old_config(home.path()).exists() && !old_cache(home.path()).exists(),
        "the server recreated nothing at the old names"
    );
}
