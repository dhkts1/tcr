//! A `tcr` child a test starts, killed when the test ends, however it ends.
//!
//! A test that panics between `spawn` and its own `kill` used to leave the child running, and a
//! test binary that was itself killed (a timeout, a Ctrl-C to cargo alone, a `kill -9`) left every
//! child it had started: a `tcr --headless --port 0 --no-replace` was found orphaned under
//! launchd two days after the run that started it, still holding its socket.
//!
//! [`KillOnDrop`] covers both:
//!
//! - On drop, which a panic's unwinding runs too, it kills the child and waits for it.
//! - For the case where nothing in the test binary runs at all, it starts a watchdog beside the
//!   child: `/bin/sh` blocked reading a pipe whose only write end this process holds. The kernel
//!   closes that end when this process dies, however it dies, so the read returns and the
//!   watchdog sends the child `SIGKILL`. On an orderly drop the watchdog is stopped first, so it
//!   never signals a pid the child no longer owns.
//!
//! What is left: a test binary killed AFTER its child had already exited on its own and been
//! reaped by launchd leaves the watchdog to signal a pid that may, in that short window, have been
//! reused. The window opens only when the test binary dies with a child that had exited and was
//! never waited on.
//!
//! The child cannot do this itself without a production change: `tcr` does not watch its stdin or
//! its parent. A `tcr` that exited when a pipe it was handed closed (or that registered for its
//! parent's exit with kqueue's `EVFILT_PROC`/`NOTE_EXIT`) would need no watchdog.

use std::io;
use std::ops::{Deref, DerefMut};
use std::process::{Child, ChildStdin, Command, Stdio};

use crate::log_err::LogErr as _;

/// A spawned child, killed and reaped on drop, and killed by a watchdog if this process dies
/// first. Derefs to the [`Child`], so a test signals, polls and reads it as before.
pub struct KillOnDrop {
    child: Child,
    watchdog: Option<Watchdog>,
}

struct Watchdog {
    process: Child,
    /// The only write end of the watchdog's stdin. Held, never written, until the drop.
    pipe: Option<ChildStdin>,
}

impl KillOnDrop {
    /// Spawn `command` and its watchdog.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let child = command.spawn()?;
        let mut guard = Self {
            child,
            watchdog: None,
        };
        guard.watchdog = Some(Watchdog::start(guard.child.id())?);
        Ok(guard)
    }
}

/// The watchdog's script, `$1` the pid to kill: `read` returns at end of file, and `exec` keeps
/// one process, not two.
const WATCHDOG: &str = r#"read -r _; exec /bin/kill -KILL "$1" 2>/dev/null"#;

impl Watchdog {
    fn start(pid: u32) -> io::Result<Self> {
        let mut process = Command::new("/bin/sh")
            .args(["-c", WATCHDOG, "tcr-test-watchdog"])
            .arg(pid.to_string())
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let pipe = process
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("the watchdog's stdin was not piped"))?;
        Ok(Self {
            process,
            pipe: Some(pipe),
        })
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        // Stopped before the pipe closes, so it never reaches its `kill`.
        self.process
            .kill()
            .log_debug("stopping a test child's watchdog");
        self.process
            .wait()
            .log_debug("reaping a test child's watchdog");
        drop(self.pipe.take());
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // The watchdog first, while the child still owns its pid.
        drop(self.watchdog.take());
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        self.child.kill().log_debug("killing a test child");
        self.child.wait().log_debug("reaping a test child");
    }
}

impl Deref for KillOnDrop {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .expect("run /bin/kill")
            .success()
    }

    fn gone_within(pid: u32, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A drop, which unwinding runs too, kills and reaps the child.
    #[test]
    fn a_drop_kills_and_reaps_the_child() {
        let guard = KillOnDrop::spawn(Command::new("/bin/sleep").arg("600")).expect("spawn");
        let pid = guard.id();
        assert!(alive(pid), "positive control: the child is running");
        drop(guard);
        assert!(!alive(pid), "the drop left pid {pid} running");
    }

    /// The case no drop covers: the process holding the pipe dies. Here a `/bin/sh` stands in
    /// for the test binary: it starts a child and a watchdog the same way and is then killed with
    /// `SIGKILL`, so nothing of it runs on the way out.
    #[test]
    fn the_watchdog_kills_the_child_when_its_parent_dies() {
        // The parent: a sleep in the background, its pid printed, and [`WATCHDOG`] on a pipe only
        // this shell holds (`exec 3>` keeps the write end open in the shell; the `sleep 1`s it
        // starts inherit fd 3 too, so the pipe closes within a second of the shell's death).
        let script = r#"
            /bin/sleep 600 </dev/null >/dev/null 2>&1 &
            child=$!
            /usr/bin/mkfifo "$1"
            /bin/sh -c "$2" watchdog "$child" <"$1" &
            exec 3>"$1"
            echo "$child"
            while :; do /bin/sleep 1; done
        "#;
        let dir = tempfile::tempdir().expect("scratch dir");
        let fifo = dir.path().join("pipe");
        let mut parent = Command::new("/bin/sh")
            .args(["-c", script, "parent"])
            .arg(&fifo)
            .arg(WATCHDOG)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the stand-in parent");
        let mut line = String::new();
        io::BufRead::read_line(
            &mut io::BufReader::new(parent.stdout.take().expect("piped")),
            &mut line,
        )
        .expect("read the child's pid");
        let child: u32 = line.trim().parse().expect("a pid");
        assert!(alive(child), "positive control: the child is running");
        parent.kill().expect("SIGKILL the stand-in parent");
        parent.wait().expect("reap it");
        let killed = gone_within(child, Duration::from_secs(5));
        if !killed {
            Command::new("/bin/kill")
                .args(["-KILL", &child.to_string()])
                .status()
                .log_debug("cleaning up after a failed watchdog test");
        }
        assert!(killed, "the child (pid {child}) outlived its parent");
    }

    /// The watchdog's own command, not the shell copy above: closing its pipe kills its target.
    #[test]
    fn closing_the_pipe_fires_the_watchdog() {
        let mut target = Command::new("/bin/sleep")
            .arg("600")
            .spawn()
            .expect("spawn");
        let pid = target.id();
        let mut watchdog = Watchdog::start(pid).expect("watchdog");
        assert!(alive(pid), "positive control: the target is running");
        // Close the pipe WITHOUT the orderly drop, which would stop the watchdog first.
        drop(watchdog.pipe.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        let reaped = loop {
            if let Some(status) = target.try_wait().expect("poll the target") {
                break Some(status);
            }
            if Instant::now() >= deadline {
                target
                    .kill()
                    .log_debug("cleaning up after a failed watchdog test");
                target.wait().log_debug("reaping it");
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            reaped.is_some_and(|status| !status.success()),
            "closing the pipe did not get the target killed within 5 s: {reaped:?}"
        );
    }
}
