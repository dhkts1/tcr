//! The durable log file's writer: one background thread fed by a queue.
//!
//! Every log event used to take a `Mutex<RollingFileAppender>` and make a `write` system call on
//! the tokio worker that emitted it. Under load that lock was the proxy's top frame
//! (`docs/performance.md`, the `overhead` phase: `make_writer_for` 8 to 20% of samples,
//! `__psynch_mutexwait` the top leaf). Here the emitter only formats its line and hands it to
//! [`tracing_appender::non_blocking`]'s queue; one thread, [`WRITER_THREAD`], does the writes.
//!
//! # The hazard this module exists to contain
//!
//! `non_blocking` returns a [`WorkerGuard`], and dropping that guard stops the writer thread with
//! no error and no warning: every event after it silently stops reaching disk while every gate
//! stays green. (That is why `src/main.rs` used the synchronous appender until now.) So the guard
//! is never handed out bare. [`start`] returns a [`FileLog`] that owns it, and the owner decides
//! when logging ends:
//!
//! * Dropping the [`FileLog`] drains the queue to disk, then stops the thread. `run_server` owns
//!   it for the process's whole life, so a normal return, an error return and a panic that unwinds
//!   through `main` all drain it on the way out.
//! * `std::process::exit` runs no destructor, so an exit after logging started takes the
//!   [`FileLog`] by value and closes it first (`stand_down_exit` in `src/main.rs`).
//! * A panic that does not end the process (a tokio task's, which `server::supervise` catches)
//!   must not stop the writer, so the panic hook from [`install_panic_flush`] does not drop
//!   anything: it waits, bounded, until everything queued before the panic is on disk. A crash
//!   that follows without unwinding still leaves its last lines in the file, which is why the
//!   file log exists.
//!
//! # Lossy off
//!
//! A full queue makes the emitter wait rather than drop the line, the same back-pressure the
//! synchronous writer had. [`QUEUE_LINES`] sizes the queue so that only a stalled disk ever
//! reaches it.

use std::io::{self, Write};
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};

/// Lines the queue holds before an emitter waits.
///
/// The busiest rate measured is the load test's `overhead` phase: about 3,200 requests a second,
/// one INFO `request usage` line each, about 720 bytes a line (`docs/performance.md`). 16,384 lines
/// is five seconds of that rate and hours of the fleet's real one (1.5 a second), so a queue this
/// size only fills when the disk itself stops. Its cost is bounded: a full queue holds about
/// 12 MB of lines, against 92 MB for the library's default of 128,000, which would double the
/// proxy's footprint (about 50 MB) before back-pressure began.
pub const QUEUE_LINES: usize = 16_384;

/// The writer thread's name, so a profile or a panic names it.
pub const WRITER_THREAD: &str = "tcr-log-writer";

/// How long the panic hook waits for the queue to reach disk. The queue at its cap is about 12 MB,
/// which a working disk writes well inside this; a stalled one must not turn a panic into a hang.
pub const PANIC_SYNC_TIMEOUT: Duration = Duration::from_secs(2);

/// How many sync barriers the writer thread has passed, whether the [`FileLog`] has closed, and
/// the condition a waiter sleeps on.
#[derive(Default)]
struct Passed {
    state: Mutex<Barriers>,
    advanced: Condvar,
}

/// What a waiter reads under [`Passed`]'s lock. `closed` sits under the same lock as `count` so a
/// close cannot slip between a waiter's check and its wait.
#[derive(Default)]
struct Barriers {
    count: u64,
    closed: bool,
}

/// The writer thread's end of the queue: the file, behind a buffer, and the barrier count.
///
/// The worker calls `write_all` once per queued line and `flush` whenever the queue runs empty
/// (`tracing_appender`'s `worker.rs`), so the buffer turns a burst of lines into one `write` call
/// and never holds a line once the queue is idle.
///
/// An EMPTY line is a sync barrier: the formatter never writes an empty line, and the worker passes
/// this one to `write_all` like any other. Reaching it means every line queued before it has been
/// handed to the file, so the sink flushes and counts it.
struct Sink<W: Write> {
    file: io::BufWriter<W>,
    passed: Arc<Passed>,
}

impl<W: Write> Write for Sink<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if !buf.is_empty() {
            return self.file.write_all(buf);
        }
        let flushed = self.file.flush();
        // Counted even when the flush failed: a waiter must be released either way, and the
        // failure is the same one every later line would meet.
        let mut state = self
            .passed
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.count += 1;
        self.passed.advanced.notify_all();
        flushed
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// A handle that can wait for the queue to reach disk without stopping the writer.
///
/// Cloned into the panic hook. It holds no [`WorkerGuard`], so keeping one alive does not keep
/// the writer alive; once the [`FileLog`] is gone, [`Flusher::sync`] returns `false` at once.
///
/// That rests on the closed flag the [`FileLog`] sets once its guard has dropped, not on the
/// queue: the guard's drop waits at most about a second for the writer and then returns while the
/// writer thread, and with it the queue's receiver, can live on (`tracing_appender`'s
/// `WorkerGuard::drop` and `worker.rs`). A barrier sent then is accepted into a queue nobody will
/// read again.
#[derive(Clone)]
pub struct Flusher {
    queue: NonBlocking,
    requested: Arc<AtomicU64>,
    passed: Arc<Passed>,
}

impl Flusher {
    /// Wait until every line queued before this call is written and flushed, or `timeout` passes.
    /// Returns whether it got there.
    ///
    /// Safe with several callers at once. Each takes a ticket and waits until that many barriers
    /// have passed. Barriers holding the earlier tickets number one fewer than this caller's
    /// ticket, so once the count reaches it, a barrier queued after this call has passed, and the
    /// queue is first in, first out: so has every line queued before it.
    ///
    /// Returns `false` at once when the [`FileLog`] has closed, and a sync already waiting returns
    /// `false` when it closes.
    pub fn sync(&self, timeout: Duration) -> bool {
        if self.lock_state().closed {
            return false;
        }
        let ticket = self.requested.fetch_add(1, Ordering::SeqCst) + 1;
        // `NonBlocking::write_all` sends the empty line as one message; with lossy off it waits for
        // room, and it errs only when the writer thread has gone.
        if self.queue.clone().write_all(&[]).is_err() {
            return false;
        }
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        while state.count < ticket {
            if state.closed {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .passed
                .advanced
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, Barriers> {
        self.passed
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Mark the log closed and wake every waiter, under the lock a waiter checks it with.
    fn mark_closed(&self) {
        self.lock_state().closed = true;
        self.passed.advanced.notify_all();
    }
}

/// The owner of the log writer thread. Dropping it drains the queue to disk and stops the thread;
/// nothing logged after that reaches the file. See the module doc for who owns it.
#[must_use = "dropping the FileLog stops the log writer thread: every later line is lost"]
pub struct FileLog {
    flusher: Flusher,
    /// Always `Some` until [`Drop`] takes it, so the drop can order the guard before the flag.
    guard: Option<WorkerGuard>,
}

impl FileLog {
    /// A handle for the panic hook, or for any caller that needs the queue on disk now.
    pub fn flusher(&self) -> Flusher {
        self.flusher.clone()
    }

    /// Wait until everything queued so far is on disk; the writer keeps running.
    pub fn sync(&self, timeout: Duration) -> bool {
        self.flusher.sync(timeout)
    }

    /// Drain the queue to disk and stop the writer. Spelled out for exits that run no destructor
    /// (`std::process::exit`); dropping the value does the same.
    pub fn close(self) {
        drop(self);
    }
}

impl Drop for FileLog {
    /// Drop the guard first: it drains the queue, so a sync whose barrier the drain passes still
    /// returns `true`. Then mark the flushers closed, which wakes a sync whose barrier the drain
    /// never reached (the guard gave up on a stuck writer) and answers every later sync at once.
    fn drop(&mut self) {
        drop(self.guard.take());
        self.flusher.mark_closed();
    }
}

/// Start the writer thread over `file` and return the queue's writer for the fmt layer, and the
/// [`FileLog`] that keeps the thread alive.
pub fn start<W: Write + Send + 'static>(file: W) -> (NonBlocking, FileLog) {
    let passed = Arc::new(Passed::default());
    let sink = Sink {
        file: io::BufWriter::new(file),
        passed: Arc::clone(&passed),
    };
    let (queue, guard) = NonBlockingBuilder::default()
        .lossy(false)
        .buffered_lines_limit(QUEUE_LINES)
        .thread_name(WRITER_THREAD)
        .finish(sink);
    let flusher = Flusher {
        queue: queue.clone(),
        requested: Arc::new(AtomicU64::new(0)),
        passed,
    };
    (
        queue,
        FileLog {
            flusher,
            guard: Some(guard),
        },
    )
}

/// The flusher the panic hook uses. Set once per process: there is one log file per process.
static PANIC_FLUSH: OnceLock<Flusher> = OnceLock::new();

/// Make every panic wait, after the previous hook has run, until the queue is on disk.
///
/// Chains the hook already installed (the default one prints the panic to stderr), and is chained
/// by any installed later (the TUI's, which calls this one for a panic on its own thread). A
/// second call is ignored. A panic on the writer thread itself does not wait, because nothing
/// would ever drain the queue it is waiting on.
pub fn install_panic_flush(flusher: Flusher) {
    if PANIC_FLUSH.set(flusher).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        if std::thread::current().name() == Some(WRITER_THREAD) {
            return;
        }
        if let Some(flusher) = PANIC_FLUSH.get() {
            flusher.sync(PANIC_SYNC_TIMEOUT);
        }
    }));
}

/// Whether `fd` is the null device, as when TcrBar hands the proxy `FileHandle.nullDevice` for
/// stdout (`ServerController.swift`). Compared by device and inode against `/dev/null` itself, so
/// a symlink or a second open of it reads the same. Anything that cannot be checked counts as not
/// null, so a sink is only ever skipped when it certainly discards what it is given.
pub fn is_null_device(fd: BorrowedFd<'_>) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(owned) = fd.try_clone_to_owned() else {
        return false;
    };
    let (Ok(this), Ok(null)) = (
        std::fs::File::from(owned).metadata(),
        std::fs::metadata("/dev/null"),
    ) else {
        return false;
    };
    this.dev() == null.dev() && this.ino() == null.ino()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd as _;
    use std::process::Command;
    use std::sync::mpsc;
    use tracing_subscriber::layer::SubscriberExt as _;

    const CHILD_ENV: &str = "TCR_FILE_LOG_TEST_CHILD";
    const CHILD_FILE_ENV: &str = "TCR_FILE_LOG_TEST_FILE";
    /// Lines the child logs before it leaves. All of them fit in the queue at once, so the child
    /// reaches its exit while nearly all of them are still queued behind [`SlowFile`].
    const CHILD_LINES: usize = 300;

    /// A file whose every write takes [`SLOW_WRITE`], so the writer is certainly behind when the
    /// child exits. Without it the writer keeps pace with the formatter and an exit that skipped
    /// the drain lost one line or none, which is a test that cannot fail. 300 lines of about
    /// 90 bytes reach the file in four buffered writes, under half a second: inside both the
    /// panic hook's wait and the guard's own one-second drain.
    struct SlowFile(std::fs::File);
    const SLOW_WRITE: Duration = Duration::from_millis(100);

    impl Write for SlowFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(SLOW_WRITE);
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    fn subscriber(queue: NonBlocking) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(queue),
        )
    }

    fn scratch_file(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join(name);
        (dir, path)
    }

    /// The writer outlives `start`: a line logged after it returns reaches the file while the
    /// writer is still running, and so does one logged after a sync. A guard dropped inside
    /// `start` (what a `()`-returning init would force) loses both.
    #[test]
    fn lines_logged_after_start_returns_reach_the_file() {
        let (_dir, path) = scratch_file("after-start.log");
        let (queue, file_log) = start(std::fs::File::create(&path).expect("create the log"));
        let dispatch = tracing::Dispatch::new(subscriber(queue));

        tracing::dispatcher::with_default(&dispatch, || tracing::info!("first-after-start"));
        assert!(
            file_log.sync(Duration::from_secs(10)),
            "the writer never passed the barrier"
        );
        let written = std::fs::read_to_string(&path).expect("read the log");
        assert!(
            written.contains("first-after-start"),
            "file held: {written:?}"
        );

        tracing::dispatcher::with_default(&dispatch, || tracing::info!("second-after-sync"));
        assert!(
            file_log.sync(Duration::from_secs(10)),
            "the writer stopped after one sync"
        );
        let written = std::fs::read_to_string(&path).expect("read the log");
        assert!(
            written.contains("second-after-sync"),
            "file held: {written:?}"
        );
    }

    /// Several threads syncing at once each get their own lines on disk, and none waits forever.
    #[test]
    fn concurrent_syncs_each_see_their_own_lines() {
        let (_dir, path) = scratch_file("concurrent.log");
        let (queue, file_log) = start(std::fs::File::create(&path).expect("create the log"));
        let dispatch = tracing::Dispatch::new(subscriber(queue));
        let flusher = file_log.flusher();
        std::thread::scope(|scope| {
            for n in 0..8 {
                let dispatch = dispatch.clone();
                let flusher = flusher.clone();
                let path = path.clone();
                scope.spawn(move || {
                    for i in 0..200 {
                        let marker = format!("thread-{n}-line-{i}-end");
                        tracing::dispatcher::with_default(&dispatch, || tracing::info!("{marker}"));
                        assert!(flusher.sync(Duration::from_secs(10)));
                        let written = std::fs::read_to_string(&path).expect("read the log");
                        assert!(
                            written.contains(&marker),
                            "{marker} not on disk after its sync"
                        );
                    }
                });
            }
        });
    }

    /// A file whose first `write` or `flush` parks the writer thread until the test releases it,
    /// so the thread is certainly still alive, and certainly not reading the queue, while the test
    /// closes the `FileLog` around it. Closing then gives up on its one-second drain and returns
    /// with the thread alive and its queue still open: the state a slow-exiting thread leaves.
    struct HeldFile {
        hold: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    }

    impl HeldFile {
        fn park(&mut self) {
            if let Some((entered, release)) = self.hold.take() {
                entered
                    .send(())
                    .expect("tell the test the writer is parked");
                release.recv().expect("the test releases the writer");
            }
        }
    }

    impl Write for HeldFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.park();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.park();
            Ok(())
        }
    }

    /// A [`HeldFile`], the receiver that hears the writer park in it, and the sender that frees it.
    fn held_file() -> (HeldFile, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let file = HeldFile {
            hold: Some((entered_tx, release_rx)),
        };
        (file, entered_rx, release_tx)
    }

    /// The writer thread outlives the close, so the barrier a later sync queues would be accepted
    /// and never read. The sync must still answer at once.
    #[test]
    fn a_sync_after_close_returns_false_while_the_writer_is_still_alive() {
        let (file, entered, release) = held_file();
        let (mut queue, file_log) = start(file);
        queue.write_all(b"held line\n").expect("queue a line");
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the writer reached the file");
        let flusher = file_log.flusher();
        file_log.close();
        let began = Instant::now();
        let synced = flusher.sync(Duration::from_secs(10));
        let took = began.elapsed();
        release.send(()).expect("release the writer");
        assert!(!synced, "a sync after close reported the queue synced");
        assert!(took < Duration::from_secs(2), "took {took:?}");
    }

    /// A sync whose barrier the close's drain does pass returns true: closing does not cut short
    /// a sync it was about to satisfy.
    #[test]
    fn a_sync_begun_before_close_returns_true_once_the_drain_passes_its_barrier() {
        let (file, entered, release) = held_file();
        let (_queue, file_log) = start(file);
        let flusher = file_log.flusher();
        let syncer = std::thread::spawn(move || flusher.sync(Duration::from_secs(10)));
        // The writer parks in the flush the sync's own barrier asks for, so the barrier is already
        // off the queue and the close's shutdown message is queued behind it.
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the writer reached the barrier");
        let closer = std::thread::spawn(move || file_log.close());
        // Inside the close's one-second drain, so the barrier passes while the close is waiting.
        std::thread::sleep(Duration::from_millis(200));
        release.send(()).expect("release the writer");
        assert!(
            syncer.join().expect("the syncer"),
            "the drain passed the barrier but the sync said it did not"
        );
        closer.join().expect("the closer");
    }

    /// A sync already waiting when the close gives up on the writer returns false as the close
    /// finishes, rather than waiting out its own timeout for a barrier nobody will pass.
    #[test]
    fn a_sync_waiting_when_the_close_gives_up_returns_false_once_it_closes() {
        let (file, entered, release) = held_file();
        let (_queue, file_log) = start(file);
        let flusher = file_log.flusher();
        let began = Instant::now();
        let syncer = std::thread::spawn(move || flusher.sync(Duration::from_secs(30)));
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the writer reached the barrier");
        file_log.close();
        let synced = syncer.join().expect("the syncer");
        let took = began.elapsed();
        release.send(()).expect("release the writer");
        assert!(
            !synced,
            "the writer never passed the barrier, yet the sync said it did"
        );
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }

    #[test]
    fn the_null_device_is_recognised_and_a_file_is_not() {
        let null = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("open /dev/null");
        assert!(is_null_device(null.as_fd()));
        let (_dir, path) = scratch_file("not-null");
        let file = std::fs::File::create(&path).expect("create a file");
        assert!(!is_null_device(file.as_fd()));
        let (reader, _writer) = io::pipe().expect("a pipe");
        assert!(!is_null_device(reader.as_fd()));
    }

    /// Run this test binary again as a child that logs [`CHILD_LINES`] lines and then leaves by
    /// `scenario`, and return the child's log file.
    fn run_child(scenario: &str) -> String {
        let (_dir, path) = scratch_file("child.log");
        let output = Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "file_log::tests::child_entry",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, scenario)
            .env(CHILD_FILE_ENV, &path)
            .output()
            .expect("run the child");
        crate::test_support::libtest::assert_ran_one_test(&output.stdout, &output.stderr);
        assert_eq!(
            output.status.code(),
            Some(7),
            "the child did not reach its exit ({:?})\n--- child stdout\n{}\n--- child stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::read_to_string(&path).expect("read the child's log")
    }

    /// The child half of the exit tests: a no-op in a normal run.
    #[test]
    fn child_entry() {
        let Ok(scenario) = std::env::var(CHILD_ENV) else {
            return;
        };
        let path = std::env::var_os(CHILD_FILE_ENV).expect("the child's log path");
        let (queue, file_log) = start(SlowFile(
            std::fs::File::create(path).expect("create the log"),
        ));
        install_panic_flush(file_log.flusher());
        let dispatch = tracing::Dispatch::new(subscriber(queue));
        tracing::dispatcher::with_default(&dispatch, || {
            for i in 0..CHILD_LINES {
                tracing::info!("child line {i}");
            }
            tracing::info!("child-last-line");
        });
        match scenario.as_str() {
            // `stand_down_exit`'s shape: close, then an exit that runs no destructor.
            "close-then-exit" => {
                file_log.close();
                std::process::exit(7);
            }
            // A panic that does not unwind through the owner, then an exit that runs no
            // destructor: only the panic hook stands between the queue and the floor.
            "panic-then-exit" => {
                assert!(
                    std::thread::spawn(|| panic!("child panics on purpose"))
                        .join()
                        .is_err(),
                    "the child thread panics"
                );
                std::mem::forget(file_log);
                std::process::exit(7);
            }
            other => panic!("unknown child scenario {other}"),
        }
    }

    /// A line logged just before an exit that runs no destructor is on disk afterwards, when the
    /// exit closes the `FileLog` first.
    #[test]
    fn closing_before_process_exit_keeps_the_last_line() {
        let written = run_child("close-then-exit");
        assert!(
            written.contains("child-last-line"),
            "the last line was lost; the file ends with {:?}",
            written.lines().last()
        );
    }

    /// A line logged just before a panic is on disk afterwards, even when the process then leaves
    /// without unwinding through the `FileLog`.
    #[test]
    fn a_panic_leaves_the_last_line_on_disk() {
        let written = run_child("panic-then-exit");
        assert!(
            written.contains("child-last-line"),
            "the last line before the panic was lost; the file ends with {:?}",
            written.lines().last()
        );
    }
}
