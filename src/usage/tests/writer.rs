//! The writer: health transitions, a stalled or saturated queue, flush and shutdown budgets.

use super::*;

/// FINDING 6. A ledger whose writes fail must say so on every transition
/// AND stop claiming today's totals will survive a restart. The old code
/// warned once, left `is_persisting()` true forever, and a disk that filled
/// at 10:00 produced a boot at 18:00 reporting a fraction of the day with a
/// complete day's confidence.
#[test]
fn a_failing_ledger_stops_claiming_it_is_persisting() {
    // A regular FILE where the ledger wants its directory: every
    // `open_day_file` fails, for the whole life of the writer.
    let blocker = scratch("failing");
    std::fs::write(&blocker, "not a directory").expect("write blocker");
    let dir = blocker.join("usage");
    let now = crate::now_ms();

    let tracker = tracker();
    tracker.attach_ledger(dir, 90, &[named("alice@example.com")], now);
    assert!(
        tracker.is_persisting(),
        "nothing has failed yet, so the attach is honest"
    );
    tracker.record(
        0,
        &record(now, "claude-opus-5", 5),
        "alice@example.com",
        None,
        None,
    );
    assert!(tracker.flush_ledger(), "the writer is alive, just failing");
    assert!(
        !tracker.is_persisting(),
        "a ledger that cannot write must not report that today's totals will survive"
    );
    assert_eq!(
        tracker.dropped_lines(),
        0,
        "the line was queued and attempted, not dropped for a full queue"
    );
    assert_eq!(
        tracker.row(0, now, None).today.requests,
        1,
        "the in-memory day is unaffected by the file failing"
    );

    std::fs::remove_file(&blocker).log_debug("removing a test file");
}

/// T4 (bridge unit 2): the write-failure policy — warn once per health
/// transition, in each direction — gets a test of its own.
///
/// Calls `Ledger::append` directly, on this thread, rather than going
/// through `UsageTracker`/the writer thread: the writer runs on a spawned
/// `std::thread`, which has no tracing dispatcher of its own unless one is
/// set globally, so a `with_default` scoped to this thread (the pattern
/// `proxy.rs::a_malformed_non_streamed_body_is_logged_and_a_usage_less_one_is_not`
/// already uses) would never see events fired there.
///
/// `dropped_lines` is a DIFFERENT counter from this policy and is asserted
/// unmoved throughout: it counts lines a FULL QUEUE ejects before `append`
/// is ever called (see `LedgerHandle::queue`), never a line that reached
/// `append` and failed to write — `a_failing_ledger_stops_claiming_it_is_persisting`
/// already pins that fact for one failed line; this generalizes it to N
/// failures and to the recovery half.
#[test]
fn write_failures_warn_once_per_transition_and_never_move_dropped_lines() {
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<(tracing::Level, String)>>>);
    struct Msg(Option<String>);
    impl Visit for Msg {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut m = Msg(None);
            event.record(&mut m);
            if let Some(msg) = m.0 {
                self.0
                    .lock()
                    .expect("capture lock")
                    .push((*event.metadata().level(), msg));
            }
        }
    }
    let count = |cap: &Capture, level: tracing::Level, needle: &str| {
        cap.0
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|(l, msg)| *l == level && msg.contains(needle))
            .count()
    };

    // A regular FILE where the ledger wants its directory: every
    // `open_day_file` fails until the blocker is removed — same fixture
    // shape as `a_failing_ledger_stops_claiming_it_is_persisting`.
    let blocker = scratch("write-failure-policy");
    std::fs::write(&blocker, "not a directory").expect("write blocker");
    let dir = blocker.join("usage");
    let now = crate::now_ms();
    let line = || LedgerLine {
        t: now,
        a: "alice@example.com".to_string(),
        u: None,
        g: None,
        p: None,
        m: None,
        s: None,
        i: 1,
        c5: 0,
        c1: 0,
        r: 0,
        o: 0,
        rt: 0,
    };

    let mut ledger = Ledger {
        dir,
        open: None,
        healthy: Arc::new(AtomicBool::new(true)),
        dropping: Arc::new(AtomicBool::new(false)),
        dropped: Arc::new(AtomicU64::new(0)),
        drops_seen: 0,
    };

    let cap = Capture::default();
    let subscriber = tracing_subscriber::registry().with(cap.clone());
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..5 {
            ledger.append(&line());
        }
    });
    assert_eq!(
        count(&cap, tracing::Level::WARN, "stopped writing"),
        1,
        "5 failed writes in a row must warn exactly ONCE, on the transition"
    );
    assert_eq!(
        ledger.dropped.load(Ordering::Relaxed),
        0,
        "a write failure is not a full-queue drop; the counter must not move"
    );

    // The sink recovers.
    std::fs::remove_file(&blocker).expect("remove blocker");
    let cap2 = Capture::default();
    let subscriber2 = tracing_subscriber::registry().with(cap2.clone());
    tracing::subscriber::with_default(subscriber2, || {
        for _ in 0..3 {
            ledger.append(&line());
        }
    });
    assert_eq!(
        count(&cap2, tracing::Level::INFO, "writing again"),
        1,
        "the recovery must be reported exactly once, not once per successful write"
    );
    assert_eq!(
        ledger.dropped.load(Ordering::Relaxed),
        0,
        "recovery must not retroactively count the failed lines as dropped either"
    );

    std::fs::remove_dir_all(&blocker).log_debug("removing a test directory");
}

/// FINDING 1. The ledger write is not on the request path: with the writer
/// thread wedged, `record()` still returns — it queues, drops what will not
/// fit, and keeps the in-memory day complete. Before this, `record()` held
/// a mutex across `write_all`, so a stalled volume queued every request in
/// the fleet behind one disk.
#[test]
fn record_does_not_block_behind_a_stalled_ledger_writer() {
    let dir = scratch("stalled");
    let now = crate::now_ms();
    let tracker = Arc::new(tracker());
    tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

    // Wedge the writer: a flush it can never complete, because this test
    // holds the receiver and never reads it.
    let (ack_tx, _ack_rx) = sync_channel::<()>(0);
    tracker
        .ledger
        .read()
        .expect("ledger lock poisoned")
        .as_ref()
        .expect("a ledger is attached")
        .tx
        .send(LedgerMsg::Flush(ack_tx))
        .expect("the writer is alive");

    // Twice the queue depth, so the queue is provably full partway through.
    let calls = LEDGER_QUEUE_DEPTH * 2;
    let finished = Arc::new(AtomicBool::new(false));
    let worker = {
        let tracker = Arc::clone(&tracker);
        let finished = Arc::clone(&finished);
        std::thread::spawn(move || {
            for _ in 0..calls {
                tracker.record(
                    0,
                    &record(now, "claude-opus-5", 1),
                    "alice@example.com",
                    None,
                    None,
                );
            }
            finished.store(true, Ordering::SeqCst);
        })
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !finished.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        finished.load(Ordering::SeqCst),
        "record() blocked behind a stalled ledger writer — the request path is not free"
    );
    worker.join().expect("the recording thread finished");

    assert!(
        tracker.dropped_lines() > 0,
        "a full queue drops lines and counts them; it must not wait for the disk"
    );
    assert_eq!(
        tracker.row(0, now, None).today.requests,
        calls as u64,
        "every request is still counted in memory, whatever the file did"
    );

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// Wedge the writer thread: send it a flush it cannot answer, and hand back
/// the receiver that holds it there. Dropping the returned receiver frees
/// it — `SyncSender::send` on a rendezvous channel fails the moment its
/// receiver goes.
fn wedge(tracker: &UsageTracker) -> Receiver<()> {
    tracker.wedge_writer_for_test()
}

/// FINDING 0 (round 2). Shutdown DRAINS the queue and joins the writer.
/// Nothing did, so every restart — and a TcrBar update forces one — threw
/// away whatever was still queued, and the boot after it reported a day
/// missing its final minutes as a measured number.
///
/// The writer is wedged while the lines are recorded, so they are provably
/// still in the queue and not on disk when the shutdown starts: the file
/// being complete afterwards is the shutdown's doing and nothing else's.
#[test]
fn shutdown_drains_every_queued_line_to_disk() {
    let dir = scratch("shutdown");
    let now = crate::now_ms();
    let tracker = tracker();
    tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

    let ack_rx = wedge(&tracker);
    for _ in 0..8 {
        tracker.record(
            0,
            &record(now, "claude-opus-5", 5),
            "alice@example.com",
            None,
            None,
        );
    }
    assert_eq!(
        lines_in(&dir, now),
        0,
        "control: with the writer wedged, not one line has reached the file yet"
    );

    // Freed a beat AFTER the shutdown begins, so the shutdown genuinely
    // waits for the drain rather than finding it already done.
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        drop(ack_rx);
    });

    assert_eq!(
        tracker.shutdown_ledger(std::time::Duration::from_secs(10)),
        LedgerShutdown::Flushed,
        "the writer drained and the file was closed inside the budget"
    );
    assert_eq!(
        lines_in(&dir, now),
        8,
        "every line queued before the shutdown is on disk"
    );
    assert!(
        !tracker.is_persisting() && !tracker.is_attached(),
        "and the tracker stops claiming a ledger it has just closed"
    );
    releaser.join().expect("the releasing thread finished");
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDING 1 (round 2). A FULL queue and a DEAD writer both lose lines
/// without any write ever failing, so `healthy` — set only from write
/// results — kept `is_persisting()` true through both. A stalled volume at
/// 10:00 then produced a `tcr status` at 18:00 showing a fraction of the
/// day, presented as measured.
#[test]
fn a_full_queue_stops_claiming_it_is_persisting() {
    let dir = scratch("full-queue");
    let now = crate::now_ms();
    let tracker = tracker();
    tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);
    assert!(tracker.is_persisting(), "control: nothing has failed yet");

    let ack_rx = wedge(&tracker);
    for _ in 0..LEDGER_QUEUE_DEPTH + 16 {
        tracker.record(
            0,
            &record(now, "claude-opus-5", 1),
            "alice@example.com",
            None,
            None,
        );
    }
    assert!(
        tracker.dropped_lines() > 0,
        "control: the queue filled and lines were dropped"
    );
    assert!(
        !tracker.is_persisting(),
        "a ledger dropping lines must not report that today's totals will survive"
    );

    drop(ack_rx);
    assert!(tracker.flush_ledger(), "the writer drains once it is freed");
    tracker.record(
        0,
        &record(now, "claude-opus-5", 1),
        "alice@example.com",
        None,
        None,
    );
    assert!(tracker.flush_ledger());
    assert!(
        tracker.is_persisting(),
        "and a successful write says so again"
    );
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDING 3 (round 2). `flush_ledger` held the ledger mutex across a
/// blocking send on the bounded queue — so calling it with the queue full
/// (the stalled-writer case it exists for) blocked forever AND put every
/// concurrent `record()` behind that same mutex, reinstating the fleet-wide
/// stall this module exists to remove.
#[test]
fn a_flush_against_a_stalled_writer_is_bounded_and_frees_the_request_path() {
    let dir = scratch("flush-bound");
    let now = crate::now_ms();
    let tracker = Arc::new(tracker());
    tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

    let ack_rx = wedge(&tracker);
    for _ in 0..LEDGER_QUEUE_DEPTH + 16 {
        tracker.record(
            0,
            &record(now, "claude-opus-5", 1),
            "alice@example.com",
            None,
            None,
        );
    }

    // Requests keep landing for as long as the flush runs, on their own
    // thread, counting as they go.
    let served = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let tracker = Arc::clone(&tracker);
        let served = Arc::clone(&served);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                tracker.record(
                    0,
                    &record(now, "claude-opus-5", 1),
                    "alice@example.com",
                    None,
                    None,
                );
                served.fetch_add(1, Ordering::SeqCst);
            }
        })
    };

    // Read MID-FLUSH, not after it: a request path blocked behind the
    // flush's own lock still catches up the instant the flush returns, so a
    // count taken afterwards cannot tell the two apart. This one is taken
    // while the flush is provably still waiting.
    let sampler = {
        let served = Arc::clone(&served);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            served.load(Ordering::SeqCst)
        })
    };

    let started = std::time::Instant::now();
    assert!(
        !tracker.flush_ledger_within(std::time::Duration::from_millis(600)),
        "a flush that cannot be queued reports failure rather than joining the stall"
    );
    let waited = started.elapsed();
    assert!(
        waited < std::time::Duration::from_secs(3),
        "the flush must return inside its own bound, not the writer's: waited {waited:?}"
    );

    let mid_flush = sampler.join().expect("the sampling thread finished");
    stop.store(true, Ordering::SeqCst);
    worker.join().expect("the recording thread finished");
    assert!(
        mid_flush > 0,
        "requests were served WHILE the flush was waiting on the wedged writer — the \
         request path never waits on the ledger"
    );
    drop(ack_rx);
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDING 0 (round 3). The flush and the writer JOIN had one deadline
/// between them, so a flush that spent it — a stalled volume, the case the
/// whole bound exists for — handed the join zero milliseconds and abandoned
/// a writer that had not been asked to stop yet: the channel closes when the
/// handle drops, which is AFTER the flush returns, so the join is the first
/// moment the writer can finish at all.
///
/// Here the flush's budget is 5ms against a wedged writer, so it is gone
/// before the join starts. The writer is freed a beat later and drains
/// every line.
#[test]
fn the_writer_join_gets_its_own_budget_after_the_flush_spent_everything() {
    let dir = scratch("join-budget");
    let now = crate::now_ms();
    let tracker = tracker();
    tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

    let ack_rx = wedge(&tracker);
    for _ in 0..8 {
        tracker.record(
            0,
            &record(now, "claude-opus-5", 5),
            "alice@example.com",
            None,
            None,
        );
    }
    assert_eq!(
        lines_in(&dir, now),
        0,
        "control: with the writer wedged, not one line has reached the file yet"
    );

    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(60));
        drop(ack_rx);
    });

    let started = std::time::Instant::now();
    let outcome = tracker.shutdown_ledger(std::time::Duration::from_millis(5));
    let waited = started.elapsed();
    assert_eq!(
        outcome,
        LedgerShutdown::Flushed,
        "the writer drained inside the join's own budget; reporting `Abandoned` here \
         tells an operator their last requests are lost when they are on disk"
    );
    assert_eq!(
        lines_in(&dir, now),
        8,
        "every line queued before the shutdown is on disk"
    );
    assert!(
        waited < MIN_LEDGER_SHUTDOWN * 4,
        "and it is still bounded: waited {waited:?}"
    );
    releaser.join().expect("the releasing thread finished");
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDING 0 (round 3), the other side of the same budget: a writer that
/// really is wedged is still abandoned — but at the END of the join's
/// budget, not at 0ms. `tcr` quitting into a stalled volume must still quit.
#[test]
fn a_wedged_writer_is_abandoned_at_the_end_of_the_join_budget_not_at_zero() {
    let dir = scratch("join-wedged");
    let now = crate::now_ms();
    let tracker = tracker();
    tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

    let ack_rx = wedge(&tracker);
    for _ in 0..4 {
        tracker.record(
            0,
            &record(now, "claude-opus-5", 5),
            "alice@example.com",
            None,
            None,
        );
    }

    let started = std::time::Instant::now();
    let outcome = tracker.shutdown_ledger(std::time::Duration::from_millis(5));
    let waited = started.elapsed();
    assert_eq!(outcome, LedgerShutdown::Abandoned);
    assert!(
        waited >= MIN_LEDGER_SHUTDOWN,
        "the join must get a real attempt of its own, not the flush's leftovers: \
         waited {waited:?}"
    );
    assert!(
        waited < MIN_LEDGER_SHUTDOWN * 8,
        "and a quit into a stalled volume is a pause, not a hang: waited {waited:?}"
    );

    drop(ack_rx);
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDINGS 4 and 7 (round 3). `healthy` had two writers — the writer thread
/// setting it from write results, the queue clearing it on a drop — and
/// under sustained saturation they flip it against each other once per
/// completed write: every write frees one slot, the next request fills it,
/// the one after is dropped. `persisting` then alternates between two reads
/// instead of reading false for as long as the incident lasts, and the
/// transition log — bounded by design — becomes a line per served request.
///
/// The writer is stepped one message at a time here, so the reading below is
/// taken at the exact point the two-writer shape disagrees with the truth:
/// one write has just completed, and the queue behind it is still full.
#[test]
fn a_saturated_queue_reports_not_persisting_for_the_whole_episode() {
    let dir = scratch("flap");
    let now = crate::now_ms();
    let tracker = tracker();
    tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);
    assert!(tracker.is_persisting(), "control: nothing has failed yet");

    let line = |tracker: &UsageTracker| {
        tracker.record(
            0,
            &record(now, "claude-opus-5", 1),
            "alice@example.com",
            None,
            None,
        );
    };

    // [park] [one line] [step] [park] — rendezvous markers the writer cannot
    // pass until this thread receives, so every observation is taken while
    // it is stopped at a known point.
    let park_first = wedge(&tracker);
    line(&tracker);
    let step = wedge(&tracker);
    let park_again = wedge(&tracker);
    for _ in 0..LEDGER_QUEUE_DEPTH + 16 {
        line(&tracker);
    }
    let dropped = tracker.dropped_lines();
    assert!(
        dropped > 0,
        "control: the queue filled and lines were dropped"
    );
    assert!(
        !tracker.is_persisting(),
        "a ledger dropping lines must not report that today's totals will survive"
    );

    // Let exactly one line through: the answer to `step` is the proof that
    // write finished, and `park_again` stops the writer before the next one.
    park_first
        .recv()
        .expect("the writer answers the first marker");
    step.recv()
        .expect("the writer wrote the line and reached the step");
    assert_eq!(
        tracker.dropped_lines(),
        dropped,
        "control: the reading below is taken between two drops, not after the episode"
    );
    assert!(
        !tracker.is_persisting(),
        "one completed write does not end an episode that is still eating lines — a \
         `healthy` the queue clears and the writer sets reads TRUE right here"
    );

    // And it recovers, once the queue is genuinely drained.
    drop(park_again);
    assert!(tracker.flush_ledger(), "the writer drains once it is freed");
    assert!(
        tracker.is_persisting(),
        "a ledger that caught up says so again"
    );
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDING 5 (round 3). `shutdown_ledger`'s `drop(handle)` is the ONLY thing
/// that closes the writer's channel, and the join right after it waits for a
/// writer that ends when the channel closes. A second live sender anywhere —
/// a flush on another thread, a future health endpoint — turns that drop
/// into a no-op, and the shutdown spends its whole budget waiting for a
/// writer nobody asked to stop. `LedgerHandle` is therefore not `Clone`, and
/// a flush lets go of its cloned SENDER before it waits.
#[test]
fn a_flush_in_flight_does_not_keep_the_writers_channel_open() {
    let dir = scratch("one-sender");
    let now = crate::now_ms();
    let tracker = Arc::new(tracker());
    tracker.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);

    let ack_rx = wedge(&tracker);
    for _ in 0..4 {
        tracker.record(
            0,
            &record(now, "claude-opus-5", 5),
            "alice@example.com",
            None,
            None,
        );
    }

    // A flush that is still outstanding when the shutdown drops the handle.
    let flusher = {
        let tracker = Arc::clone(&tracker);
        std::thread::spawn(move || tracker.flush_ledger_within(std::time::Duration::from_secs(3)))
    };
    std::thread::sleep(std::time::Duration::from_millis(40));

    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(60));
        drop(ack_rx);
    });
    assert_eq!(
        tracker.shutdown_ledger(std::time::Duration::from_millis(5)),
        LedgerShutdown::Flushed,
        "the writer ended, so the channel it reads was closed — no other sender \
         outlived the handle"
    );
    assert_eq!(lines_in(&dir, now), 4);
    flusher.join().expect("the flushing thread finished");
    releaser.join().expect("the releasing thread finished");
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}
