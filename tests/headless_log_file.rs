//! The durable log file of the BUILT BINARY, run the way TcrBar runs it: `--headless` with stdout
//! on the null device (`ServerController.swift`, `standardOutput = FileHandle.nullDevice`).
//!
//! The file is written by one background thread fed by a queue (`src/file_log.rs`), and that
//! thread stops, silently, when its guard is dropped. `src/main.rs` holds the guard in
//! `run_server` for the process's life. This test checks the two ends of that life from outside:
//!
//! * the `listening on` line, logged after `init_tracing` returned, reaches the file while the
//!   process runs. The test reads the port from the file and nowhere else, so a guard dropped
//!   inside `init_tracing` fails it at boot;
//! * the usage ledger's closing line, the last line a clean shutdown logs (`ServerHandle::
//!   shutdown_within` in `src/server.rs`), is in the file once the process has exited after a
//!   SIGTERM.
//!
//! With stdout on the null device the proxy writes no second copy (`init_tracing`), so this is
//! also the check that skipping stdout did not skip the file.
//!
//! Isolation is the same as `tests/headless_sigterm.rs`: `HOME` is a fresh temporary directory
//! set on the child's `Command`, `XDG_CACHE_HOME` is removed, the Claude Code login import points
//! at a file that does not exist, and `--port 0` takes an ephemeral port, never the live 3456.
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tcr::paths;

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
/// The shutdown grace is 5 s (`server::DEFAULT_SHUTDOWN_GRACE`); this is "it hung", not "slow".
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);
const LAST_SHUTDOWN_LINE: &str = "the usage ledger drained and closed";

fn log_text(home: &Path) -> String {
    let logs = home.join(".cache").join(paths::CACHE_DIR).join("logs");
    let Ok(entries) = std::fs::read_dir(&logs) else {
        return String::new();
    };
    let mut files: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    files.sort();
    files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .collect()
}

fn listening_port(text: &str) -> Option<u16> {
    let marker = "listening on http://";
    let line = text.lines().find(|l| l.contains(marker))?;
    let after = &line[line.find(marker)? + marker.len()..];
    after
        .split_whitespace()
        .next()?
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

#[test]
fn the_log_file_holds_the_boot_and_the_last_shutdown_line_with_stdout_on_the_null_device() {
    let home = tempfile::tempdir().expect("a scratch HOME");
    let bin = env!("CARGO_BIN_EXE_tcr");
    let mut child = Command::new(bin)
        .args(["--headless", "--port", "0", "--no-replace"])
        .env("HOME", home.path())
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.path().join("no-such-claude-code-credentials.json"),
        )
        .env_remove("XDG_CACHE_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|err| panic!("spawning {bin} failed: {err}"));

    let deadline = Instant::now() + BOOT_TIMEOUT;
    let port = loop {
        if let Some(port) = listening_port(&log_text(home.path())) {
            break port;
        }
        if let Some(status) = child.try_wait().expect("poll the child") {
            panic!(
                "tcr exited ({status}) before its log file said it was listening; file held {:?}",
                log_text(home.path())
            );
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "the log file never got the `listening on` line within {BOOT_TIMEOUT:?}: the \
                 writer stopped once init returned, or never ran; file held {:?}",
                log_text(home.path())
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_ne!(port, 3456, "the test proxy must never take the live port");

    let status = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "could not SIGTERM the child");
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    let exit = loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("tcr did not exit within {SHUTDOWN_TIMEOUT:?} of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(exit.success(), "a SIGTERM shutdown exits 0, got {exit:?}");

    let text = log_text(home.path());
    assert!(
        text.contains("SIGTERM received; shutting down"),
        "the shutdown line is not in the file; it ends with {:?}",
        text.lines().last()
    );
    assert!(
        text.contains(LAST_SHUTDOWN_LINE),
        "the last line a clean shutdown logs is not in the file, so the queue was not drained \
         on the way out; it ends with {:?}",
        text.lines().last()
    );
}
