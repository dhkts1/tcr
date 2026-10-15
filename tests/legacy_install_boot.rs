//! Proves an install from before the rename keeps its old names through a
//! server boot: given `~/.config/<legacy config>` and an existing
//! `~/.cache/<legacy cache dir>`, `tcr server` creates no new-name config file
//! and no new-name cache directory, and the port-owner claim and the logs land
//! in the old cache directory.
//!
//! `tests/first_run_no_config.rs` covers the old CONFIG through offline verbs;
//! nothing else ran the built binary against an old CACHE directory, and the
//! cache is where a split would hurt (a new `~/.cache/tcr/` beside the old
//! folder silently drops the usage ledger and the session pins).
//!
//! # Isolation
//!
//! Same rules as `tests/headless_sigterm.rs`: `HOME` is a fresh
//! [`tempfile::TempDir`], `XDG_CACHE_HOME` is removed, both on the spawned
//! `Command`; `--port 0` never touches the live proxy's 3456; the Claude Code
//! login import is pointed at a path that does not exist.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use tcr::{legacy_names, paths, singleton::owner_path_in};

const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);

/// The port out of `tcr listening on http://127.0.0.1:<port> (headless)`.
fn parse_listening_port(line: &str) -> Option<u16> {
    let marker = "listening on http://";
    let after = &line[line.find(marker)? + marker.len()..];
    after
        .split_whitespace()
        .next()?
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

/// Every path under `dir`, relative to `root`, sorted.
fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        out.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        if path.is_dir() {
            walk(root, &path, out);
        }
    }
    out.sort();
}

/// Boot, wait for the listening line, stop, and return the port it bound.
fn boot_and_stop(home: &Path) -> u16 {
    let bin = env!("CARGO_BIN_EXE_tcr");
    let mut child = Command::new(bin)
        .args(["server", "--headless", "--port", "0"])
        .env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("no-such-claude-code-credentials.json"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary ({bin}) failed: {err}"));

    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + BOOT_TIMEOUT;
    let port = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            panic!("tcr never printed a 'listening on http://' line within {BOOT_TIMEOUT:?}");
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(line) if line.contains("listening on http://") => {
                break parse_listening_port(&line).unwrap_or_else(|| {
                    let _ = child.kill();
                    panic!("could not parse a port out of the listening line: {line:?}")
                });
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                panic!("tcr exited before it printed a listening line");
            }
        }
    };

    // A graceful stop withdraws the port-owner claim, so it is read while the
    // process is still up.
    let live_owner = owner_path_in(&home.join(".cache").join(legacy_names::CACHE_DIR), port);
    assert!(
        live_owner.is_file(),
        "the port-owner claim must be written into the OLD cache directory while \
         the server is up: {}",
        live_owner.display()
    );

    let pid = child.id().to_string();
    let signalled = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("kill is on PATH");
    assert!(signalled.success(), "SIGTERM to our own child (pid {pid})");
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        if child.try_wait().expect("polling the child").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("tcr did not exit within {EXIT_TIMEOUT:?} of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    port
}

/// An existing install: the legacy config file and the legacy cache directory
/// are used where they are, and nothing with a new name appears beside them.
///
/// Mutation note: make `paths::pick` return the new generation unconditionally
/// and this fails on the first assertion after the boot (or on the live claim
/// check), with the new-name entries listed.
#[test]
fn a_legacy_install_keeps_its_names_through_a_boot() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let config_dir = home.path().join(".config");
    let cache_dir = home.path().join(".cache").join(legacy_names::CACHE_DIR);
    std::fs::create_dir_all(&config_dir).expect("mkdir .config");
    std::fs::create_dir_all(&cache_dir).expect("mkdir the legacy cache dir");
    let legacy_config = config_dir.join(legacy_names::CONFIG_FILE);
    std::fs::write(&legacy_config, r#"{"accounts":[]}"#).expect("write the legacy config");

    boot_and_stop(home.path());

    let new_config = config_dir.join(paths::CONFIG_FILE);
    let new_cache = home.path().join(".cache").join(paths::CACHE_DIR);
    let mut tree = Vec::new();
    walk(home.path(), home.path(), &mut tree);
    let listing = tree
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !new_config.exists(),
        "a new-name config appeared beside the legacy one; tree:\n{listing}"
    );
    assert!(
        !new_cache.exists(),
        "a new-name cache directory appeared beside the legacy one; tree:\n{listing}"
    );
    assert!(
        legacy_config.is_file(),
        "the legacy config must still be there"
    );

    // The logs landed in the old directory: a rotating file under `logs/`.
    let logs = cache_dir.join("logs");
    let log_files: Vec<_> = std::fs::read_dir(&logs)
        .unwrap_or_else(|err| panic!("no logs dir at {}: {err}; tree:\n{listing}", logs.display()))
        .filter_map(Result::ok)
        .collect();
    assert!(
        !log_files.is_empty(),
        "the old cache directory's logs/ must hold the log file; tree:\n{listing}"
    );
    let logged = log_files.iter().any(|entry| {
        std::fs::read_to_string(entry.path())
            .is_ok_and(|body| body.contains("listening on http://"))
    });
    assert!(
        logged,
        "the boot's 'listening on' line must be in the old cache directory's log; \
         tree:\n{listing}"
    );
}
