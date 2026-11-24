//! The ledger writer: the queue the request path hands lines to, and the one thread that owns
//! the open day file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use time::Date;

use super::LEDGER_LINGER;
use super::day_files::{LedgerLine, compress_closed_days, date_string, ledger_date};

/// What the writer thread receives. A `Flush` is a rendezvous: the writer
/// answers it only after every line queued before it has been written, which is
/// what lets a caller (a test, or shutdown) know the file is complete.
pub(super) enum LedgerMsg {
    Line(Box<LedgerLine>),
    Flush(SyncSender<()>),
}

/// The write half of the ledger, as seen from the request path. Holds no file
/// and performs no I/O — it only queues.
///
/// Deliberately NOT `Clone`. [`UsageTracker::shutdown_ledger`]'s correctness
/// rests on its `drop(handle)` being the last sender: that is what closes the
/// channel, ends the writer, and makes the join it then waits on able to finish
/// at all. A second live handle anywhere — a flush on another thread, a future
/// health endpoint — silently turns that drop into a no-op and the shutdown
/// spends its whole budget waiting for a writer nobody asked to stop. A caller
/// that only needs to SEND clones the `SyncSender` instead ([`Self::flush_via`])
/// and lets go of it before it waits, which keeps both properties: the ledger
/// mutex is not held across a send, and no sender outlives the call.
#[derive(Debug)]
pub(super) struct LedgerHandle {
    pub(super) tx: SyncSender<LedgerMsg>,
    /// Lines the queue had no room for. Shared with the writer, which reports
    /// the running count on every health transition.
    pub(super) dropped: Arc<AtomicU64>,
    /// Raised here and lowered only by the writer — see the module docs on flag
    /// ownership. A line the queue had no room for is a line that will not
    /// survive a restart, exactly like one whose write failed, and the writer
    /// never sees it to say so.
    pub(super) dropping: Arc<AtomicBool>,
}

impl LedgerHandle {
    /// Queue one line, or count it as dropped. NEVER blocks: a writer stuck on
    /// a stalled volume must cost accounting, not latency.
    ///
    /// A drop RAISES `dropping`, which is this side's only flag write: `Full`
    /// means the writer is not keeping up and `Disconnected` means it is gone;
    /// in both, this line is lost and so is every one behind it until the queue
    /// moves again, so `is_persisting()` must stop saying today's totals will
    /// survive. The warning is on the flag's false→true edge, so an episode
    /// costs one line however many requests it eats — and the flag stays raised
    /// until the WRITER lowers it, which is what stops the report flapping once
    /// per completed write under sustained saturation.
    pub(super) fn queue(&self, line: LedgerLine) {
        let Err(err) = self.tx.try_send(LedgerMsg::Line(Box::new(line))) else {
            return;
        };
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        let reason = match err {
            std::sync::mpsc::TrySendError::Full(_) => "the writer's queue is full",
            std::sync::mpsc::TrySendError::Disconnected(_) => "the writer thread has ended",
        };
        if !self.dropping.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                reason,
                dropped_lines = dropped,
                "the usage ledger is dropping lines; today's totals will NOT survive a \
                 restart until it recovers"
            );
        }
    }

    /// Block until the writer has drained everything queued before this call,
    /// or `deadline` passes. Bounded at BOTH steps — queueing the marker and
    /// waiting for its answer — because the point of the writer thread is that a
    /// stalled disk cannot hold anything else up. An unbounded `send` here would
    /// block forever on precisely the full queue a stall produces, which is the
    /// one case the bound exists for.
    ///
    /// Takes the sender BY VALUE and drops it before waiting on the ack, so a
    /// flush overlapping a shutdown cannot be the sender that keeps the writer's
    /// channel open — see [`LedgerHandle`]'s own docs.
    pub(super) fn flush_via(tx: SyncSender<LedgerMsg>, deadline: std::time::Instant) -> bool {
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        let mut msg = LedgerMsg::Flush(ack_tx);
        loop {
            match tx.try_send(msg) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
                Err(std::sync::mpsc::TrySendError::Full(back)) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    msg = back;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        drop(tx);
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        ack_rx.recv_timeout(left).is_ok()
    }

    /// [`Self::flush_via`] on a borrowed handle, for the one caller that still
    /// holds the handle itself.
    pub(super) fn flush_within(&self, deadline: std::time::Instant) -> bool {
        Self::flush_via(self.tx.clone(), deadline)
    }
}

/// What [`UsageTracker::shutdown_ledger`] did — reported rather than logged
/// alone, so a caller (and a test) can tell "the day file is complete" apart
/// from "the writer was abandoned mid-stall".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerShutdown {
    /// No ledger was attached, so nothing was queued and nothing was lost.
    NotAttached,
    /// Every line queued before the shutdown is on disk, and the writer thread
    /// has ended with its file closed.
    Flushed,
    /// The writer did not drain inside the budget and was left behind. Whatever
    /// was still queued is gone, and this is the process saying so.
    Abandoned,
}

/// The append-only file half, owned by ONE dedicated thread. Holds the
/// currently-open day's file so the common case is one `write` syscall per
/// request, and rolls over when the UTC date changes.
#[derive(Debug)]
pub(super) struct Ledger {
    pub(super) dir: PathBuf,
    pub(super) open: Option<(Date, std::fs::File)>,
    /// Cleared while writes are failing and set again when one succeeds. Read
    /// by [`UsageTracker::is_persisting`], so an operator asking "will today's
    /// totals survive a restart" gets the truth rather than the answer that was
    /// true at boot. This thread is its ONLY writer.
    pub(super) healthy: Arc<AtomicBool>,
    /// Raised by [`LedgerHandle::queue`] when a line is lost to a full queue,
    /// and lowered HERE, by [`Self::note_drop_episode`], once a completed write
    /// has proof the pressure eased.
    pub(super) dropping: Arc<AtomicBool>,
    pub(super) dropped: Arc<AtomicU64>,
    /// The drop count as of this thread's previous completed write. An episode
    /// has ended when a write completes and this has not moved: every drop
    /// means a full queue, a full queue means lines waiting, and lines waiting
    /// mean another write — so a write with nothing new behind it is the
    /// pressure being over, not a lull inside it.
    pub(super) drops_seen: u64,
}

impl Ledger {
    /// Write a line, or answer a flush once every line before it is written.
    fn handle(&mut self, msg: LedgerMsg) {
        match msg {
            LedgerMsg::Line(line) => self.append(&line),
            LedgerMsg::Flush(ack) => {
                // The only error here is a flusher that timed out and dropped
                // its receiver, and it has already stopped waiting.
                if ack.send(()).is_err() {
                    tracing::debug!(
                        "a usage-ledger flush was abandoned before the writer answered"
                    );
                }
            }
        }
    }

    /// The UTC date of the day file open for appending, if one is.
    fn open_date(&self) -> Option<Date> {
        self.open.as_ref().map(|(date, _)| *date)
    }

    pub(super) fn append(&mut self, line: &LedgerLine) {
        // Report every TRANSITION, in both directions. Warning once and then
        // falling silent forever is how a ledger that died at 10:00 goes on
        // looking healthy at 18:00; warning per failed write is a log flood. A
        // transition is the one event that is both bounded and never absent.
        if let Err(err) = self.try_append(line) {
            if self.set_healthy(false) {
                tracing::warn!(
                    dir = %self.dir.display(),
                    error = %err,
                    dropped_lines = self.dropped.load(Ordering::Relaxed),
                    "the usage ledger stopped writing; today's totals will NOT survive a \
                     restart until it recovers"
                );
            }
        } else {
            if self.set_healthy(true) {
                tracing::info!(
                    dir = %self.dir.display(),
                    dropped_lines = self.dropped.load(Ordering::Relaxed),
                    "the usage ledger is writing again; the lines it missed are gone"
                );
            }
            self.note_drop_episode();
        }
    }

    /// Move `healthy` to `to`, answering whether that was a TRANSITION. One
    /// helper rather than a `swap` open-coded at each arm, so "which direction
    /// is worth a line" is decided in one place and cannot drift between the
    /// two.
    fn set_healthy(&self, to: bool) -> bool {
        self.healthy.swap(to, Ordering::Relaxed) != to
    }

    /// Close out a full-queue episode, if this write ended one.
    ///
    /// Called only after a SUCCESSFUL write, and only this thread ever lowers
    /// the flag — the queue side would lower it on the very next completed
    /// write and re-raise it on the request after that, which is a flap rather
    /// than a report. The re-read after the clear closes the one race that is
    /// left: a line dropped while this was deciding leaves the counter moved,
    /// and the flag goes straight back up.
    fn note_drop_episode(&mut self) {
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped != self.drops_seen {
            // Still inside the episode: lines are being lost faster than this
            // thread is clearing them. Say nothing and stay unhealthy.
            self.drops_seen = dropped;
            return;
        }
        if !self.dropping.swap(false, Ordering::Relaxed) {
            return;
        }
        if self.dropped.load(Ordering::Relaxed) != dropped {
            self.dropping.store(true, Ordering::Relaxed);
            return;
        }
        tracing::info!(
            dir = %self.dir.display(),
            dropped_lines = dropped,
            "the usage ledger caught up; the lines it dropped while its queue was full are gone"
        );
    }

    fn try_append(&mut self, line: &LedgerLine) -> std::io::Result<()> {
        let date = ledger_date(line.t);
        if self.open.as_ref().is_none_or(|(open, _)| *open != date) {
            self.open = Some((date, open_day_file(&self.dir, date)?));
        }
        let Some((_, file)) = self.open.as_mut() else {
            return Ok(());
        };
        let mut buf = serde_json::to_vec(line)?;
        buf.push(b'\n');
        file.write_all(&buf)
    }
}

/// The writer thread's whole life: take lines off the queue, write them, answer
/// flushes. Ends when the last [`LedgerHandle`] is dropped and the channel
/// closes. On a clean shutdown that is [`UsageTracker::shutdown_ledger`]
/// dropping the handle after a flush and then joining this thread, so the last
/// line served is on disk; a tracker dropped in a test ends it the same way,
/// with nobody waiting.
///
/// When the queue runs empty it waits [`LEDGER_LINGER`] before it blocks.
/// A blocked receiver makes every send take the channel's waker mutex to wake
/// it, and senders are the request path. Once the ledger handle's own lock
/// stopped putting senders in single file, that waker mutex became the proxy's
/// busiest lock under load (2026-10-16 profile: up to 111 of 2,219 samples in
/// `SyncWaker::notify`). Under traffic the linger leaves lines waiting, so the
/// next receive takes one without blocking and no send has a sleeper to wake.
///
/// It also compresses closed days ([`compress_closed_days`]): once when it starts, and again
/// each time a line opens a new day's file, which is when one more day closes. Doing it here
/// rather than on a thread of its own costs no thread and no timer, and the queue is answered
/// between files.
pub(super) fn ledger_writer(rx: Receiver<LedgerMsg>, mut ledger: Ledger) {
    let mut compressed_for = ledger_date(crate::now_ms());
    compress_ledger(&rx, &mut ledger, compressed_for);
    loop {
        let msg = match rx.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {
                std::thread::sleep(LEDGER_LINGER);
                match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => return,
                }
            }
        };
        ledger.handle(msg);
        if let Some(open) = ledger.open_date()
            && open > compressed_for
        {
            compressed_for = open;
            compress_ledger(&rx, &mut ledger, open);
        }
    }
}

/// One pass of [`compress_closed_days`] over the ledger's directory, answering whatever the
/// queue holds between files, and one INFO line when it compressed anything.
fn compress_ledger(rx: &Receiver<LedgerMsg>, ledger: &mut Ledger, today: Date) {
    let dir = ledger.dir.clone();
    let report = compress_closed_days(&dir, today, || {
        while let Ok(msg) = rx.try_recv() {
            ledger.handle(msg);
        }
    });
    if report.days > 0 {
        tracing::info!(
            dir = %dir.display(),
            days = report.days,
            plain_bytes = report.plain_bytes,
            gz_bytes = report.gz_bytes,
            failed = report.failed,
            "usage ledger: closed days compressed"
        );
    }
}

/// Create the ledger directory `0700` and open (or create) one day's file
/// `0600`, append-only. Same confidentiality posture as the log directory and
/// the pin file: the parent `~/.cache/tcr/` is `0755`, so an owner-only
/// mode has to be asserted here or these files are world-readable on a
/// multi-user box. The mode is requested at creation time rather than
/// `chmod`-ed afterwards, so there is no window in which the file exists at the
/// umask's mode.
fn open_day_file(dir: &Path, date: Date) -> std::io::Result<std::fs::File> {
    ensure_dir(dir)?;
    let path = dir.join(format!("{}.jsonl", date_string(date)));
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(super) fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}
