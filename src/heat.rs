//! The serving Mac's heat, for [`crate::status::StatusPayload::heat`]: CPU
//! temperature and fan speeds, read through AppleSMC by the `tcr-smc` crate.
//!
//! # Off the runtime, briefly cached, and never waited on for long
//!
//! One read is a handful of kernel calls into AppleSMC (and, on a Mac without
//! `TCMz`, a one-time walk of a key table thousands of entries long), so it runs
//! on tokio's blocking pool, never on a runtime worker. The panel polls the
//! status endpoint every few seconds, and more than one client may poll it, so
//! a reading is kept for [`MAX_AGE`]: a burst of status requests inside that
//! window costs one SMC read.
//!
//! A read that is already in flight is never queued behind: a second caller
//! gets the last reading (or none) at once, through `try_lock`. And the caller
//! that started the read waits at most [`READ_DEADLINE`] for it, then answers
//! with the last reading too. So an SMC that wedges costs one blocking-pool
//! thread, the one stuck in the kernel, and never a status request: the
//! endpoint, and the blocking pool that token refresh shares, keep moving.
//!
//! Heat is not a secret, so the endpoint's no-secret invariant
//! ([`crate::status`]) is unchanged by carrying it.

use std::sync::{Arc, LazyLock, Mutex, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use tcr_status_wire::{FanRow, HeatRow};

/// How long one reading answers for. A starting value, not a measured one:
/// short against the panel's three-second poll, so the figure is never more
/// than a poll old, and long enough that two clients polling together read
/// the SMC once.
pub const MAX_AGE: Duration = Duration::from_secs(2);

/// How long a status request waits for an SMC read before it answers with
/// the last reading. A starting value, not a measured one: an uncached read
/// measured 1.4 to 1.7 ms here, so this is hundreds of reads' worth of
/// patience, and still short enough that a wedged SMC is a slow answer rather
/// than no answer.
pub const READ_DEADLINE: Duration = Duration::from_millis(500);

/// What a cache reads with: `None` is "this machine has no AppleSMC".
pub type Read<T = HeatRow> = Box<dyn FnMut() -> Option<T> + Send>;

/// One reader and its last answer.
///
/// Two locks on purpose. `reader` is held across the SMC read and only ever
/// taken with `try_lock`, so nobody waits on it; `last` is held for a clone,
/// so a caller that finds a read in flight can still answer from it.
pub struct HeatCache<T = HeatRow> {
    max_age: Duration,
    reader: Mutex<Read<T>>,
    last: Mutex<Option<(Instant, Option<T>)>>,
}

impl<T: Clone> HeatCache<T> {
    pub fn new(max_age: Duration, read: Read<T>) -> Self {
        Self {
            max_age,
            reader: Mutex::new(read),
            last: Mutex::new(None),
        }
    }

    /// The last reading, however old, or `None` when there has never been one.
    pub fn last(&self) -> Option<T> {
        self.last_entry().and_then(|(_, reading)| reading)
    }

    /// A poisoned lock means a read panicked. What it guards is a reader and
    /// an answer, both still whole, so the next request reads on rather than
    /// losing heat for the life of the process.
    fn last_entry(&self) -> Option<(Instant, Option<T>)> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn fresh(&self, now: Instant) -> Option<Option<T>> {
        let (at, reading) = self.last_entry()?;
        (now.saturating_duration_since(at) < self.max_age).then_some(reading)
    }

    /// The reading as of `now`: the kept one while it is younger than the
    /// cache's maximum age, a fresh one otherwise. Blocking while it reads:
    /// call it off the async runtime.
    ///
    /// Never waits on another caller's read: when one is in flight this
    /// answers with the last reading, however old, or `None`.
    ///
    /// "No AppleSMC" is kept like any other answer, so a VM pays for the
    /// question once per window too.
    pub fn get(&self, now: Instant) -> Option<T> {
        if let Some(reading) = self.fresh(now) {
            return reading;
        }
        let mut read = match self.reader.try_lock() {
            Ok(read) => read,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return self.last(),
        };
        // The read that held the lock a moment ago may have just answered.
        if let Some(reading) = self.fresh(now) {
            return reading;
        }
        let reading = read();
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some((now, reading.clone()));
        reading
    }

    /// A fresh reading, whatever the kept one's age, kept as the answer for
    /// the next [`Self::get`]. For the fan controller's tick (`crate::fan`),
    /// which must judge the fans from what they read NOW, on its own thread:
    /// it waits for a read already in flight rather than taking an older
    /// answer, and a status request inside the next [`MAX_AGE`] costs no SMC
    /// read of its own.
    pub fn refresh(&self, now: Instant) -> Option<T> {
        let mut read = self.reader.lock().unwrap_or_else(PoisonError::into_inner);
        let reading = read();
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some((now, reading.clone()));
        reading
    }
}

/// The wire form of one `tcr-smc` reading.
pub fn to_row(reading: &tcr_smc::HeatReading) -> HeatRow {
    HeatRow {
        cpu_celsius: reading.cpu_celsius,
        fans: reading
            .fans
            .iter()
            .map(|fan| FanRow {
                index: fan.index.get(),
                actual_rpm: fan.actual_rpm,
                min_rpm: fan.min_rpm,
                max_rpm: fan.max_rpm,
                manual: fan.is_manual(),
                target_rpm: fan.target_rpm,
            })
            .collect(),
        ftst: reading.ftst,
    }
}

/// The process's one cache, on the process's one AppleSMC source, which opens
/// on the first status request that asks and, if AppleSMC refuses, again
/// a minute later (`tcr_smc::HeatSource`). Each distinct SMC error is one
/// warning. It keeps the `tcr-smc` reading itself, with each fan's three-way
/// mode, because the fan controller reads through it too ([`process`]).
static PROCESS: LazyLock<Arc<HeatCache<tcr_smc::HeatReading>>> = LazyLock::new(|| {
    let mut source = tcr_smc::HeatSource::open(Box::new(|line| tracing::warn!("{line}")));
    Arc::new(HeatCache::new(MAX_AGE, Box::new(move || source.read())))
});

/// The process's one cache, for the fan controller: one reader, so the
/// controller's tick and the status route never read the SMC twice for one
/// moment.
pub fn process() -> Arc<HeatCache<tcr_smc::HeatReading>> {
    Arc::clone(&PROCESS)
}

/// The machine's heat now, through the process's cache: see [`read_within`].
pub async fn current() -> Option<HeatRow> {
    read_within(Arc::clone(&PROCESS), Instant::now(), READ_DEADLINE)
        .await
        .as_ref()
        .map(to_row)
}

/// `cache.get(now)` on the blocking pool, waited on for at most `deadline`.
/// Past it, or when the blocking task itself failed (logged), the answer is
/// the last reading, or `None`. The read that ran over keeps running on its
/// own thread and fills the cache when it lands.
pub async fn read_within<T: Clone + Send + 'static>(
    cache: Arc<HeatCache<T>>,
    now: Instant,
    deadline: Duration,
) -> Option<T> {
    let reading = Arc::clone(&cache);
    match tokio::time::timeout(
        deadline,
        tokio::task::spawn_blocking(move || reading.get(now)),
    )
    .await
    {
        Ok(Ok(reading)) => reading,
        Ok(Err(error)) => {
            tracing::warn!(%error, "the heat read did not finish");
            cache.last()
        }
        Err(_elapsed) => {
            tracing::warn!(
                deadline_ms = deadline.as_millis(),
                "the SMC read ran past its deadline; answering with the last reading"
            );
            cache.last()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;

    fn counted(reads: &Arc<AtomicUsize>, row: Option<HeatRow>, pause: Duration) -> Read {
        let reads = Arc::clone(reads);
        Box::new(move || {
            reads.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(pause);
            row.clone()
        })
    }

    fn reading(celsius: f64) -> HeatRow {
        HeatRow {
            cpu_celsius: Some(celsius),
            fans: vec![FanRow {
                index: 0,
                actual_rpm: 2400.0,
                min_rpm: 1350.0,
                max_rpm: 5777.0,
                manual: false,
                target_rpm: None,
            }],
            ftst: None,
        }
    }

    /// A read that returns `first` at once, then blocks every later call until
    /// the returned sender sends (or is dropped, which a failing test does on
    /// its way out, so no blocking-pool thread outlives it). The receiver
    /// hears each blocked call start.
    fn first_then_blocking(first: HeatRow) -> (Read, mpsc::Sender<()>, mpsc::Receiver<()>) {
        let (release, wait) = mpsc::channel::<()>();
        let (started, blocked) = mpsc::channel::<()>();
        let mut calls = 0;
        let read: Read = Box::new(move || {
            calls += 1;
            if calls > 1 {
                started.send(()).ok();
                // Err is the sender gone: released either way.
                wait.recv().ok();
            }
            Some(first.clone())
        });
        (read, release, blocked)
    }

    /// Eight requests at once, each finding the cache empty and the read
    /// taking 50 ms: one SMC read between them, and nobody waits on it.
    #[test]
    fn heat_cache_serves_a_burst_with_one_read() {
        let reads = Arc::new(AtomicUsize::new(0));
        let cache = Arc::new(HeatCache::new(
            MAX_AGE,
            counted(&reads, Some(reading(61.0)), Duration::from_millis(50)),
        ));
        let now = Instant::now();
        let answers: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || cache.get(now))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert!(answers.contains(&Some(reading(61.0))), "{answers:?}");
        assert_eq!(cache.get(now), Some(reading(61.0)));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    /// A read that never comes back does not hold a second caller: it gets the
    /// last reading straight away instead of queuing behind the stuck one.
    #[test]
    fn heat_cache_never_queues_behind_a_read_in_flight() {
        let (read, release, blocked) = first_then_blocking(reading(50.0));
        let cache = Arc::new(HeatCache::new(MAX_AGE, read));
        let start = Instant::now();
        assert_eq!(cache.get(start), Some(reading(50.0)));

        let stale = start + MAX_AGE;
        let stuck = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || cache.get(stale))
        };
        // The stuck read is inside the SMC, holding the reader lock.
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        let (tx, rx) = mpsc::channel();
        {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || tx.send(cache.get(stale)).ok());
        }
        let second = rx.recv_timeout(Duration::from_secs(2));
        release.send(()).ok();
        assert_eq!(
            second,
            Ok(Some(reading(50.0))),
            "queued behind the stuck read"
        );
        assert_eq!(stuck.join().unwrap(), Some(reading(50.0)));
    }

    /// The caller that starts a read that wedges waits only until the
    /// deadline, then answers with the last reading.
    #[tokio::test(flavor = "multi_thread")]
    async fn heat_read_answers_with_the_last_reading_at_its_deadline() {
        let (read, release, _blocked) = first_then_blocking(reading(50.0));
        let cache = Arc::new(HeatCache::new(MAX_AGE, read));
        let start = Instant::now();
        assert_eq!(
            read_within(Arc::clone(&cache), start, READ_DEADLINE).await,
            Some(reading(50.0))
        );

        let asked = Instant::now();
        let answer = tokio::time::timeout(
            Duration::from_secs(5),
            read_within(
                Arc::clone(&cache),
                start + MAX_AGE,
                Duration::from_millis(50),
            ),
        )
        .await;
        let waited = asked.elapsed();
        release.send(()).ok();
        assert_eq!(answer, Ok(Some(reading(50.0))), "waited on a wedged read");
        assert!(waited < Duration::from_secs(1), "{waited:?}");
    }

    /// A reading answers for less than the maximum age and no longer.
    #[test]
    fn heat_cache_reads_again_once_the_reading_is_old() {
        let reads = Arc::new(AtomicUsize::new(0));
        let cache = HeatCache::new(
            MAX_AGE,
            counted(&reads, Some(reading(61.0)), Duration::ZERO),
        );
        let start = Instant::now();
        assert_eq!(cache.get(start), Some(reading(61.0)));
        assert_eq!(
            cache.get(start + Duration::from_millis(1999)),
            Some(reading(61.0))
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(cache.get(start + MAX_AGE), Some(reading(61.0)));
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    /// "No AppleSMC" is an answer and is kept like one.
    #[test]
    fn heat_cache_keeps_no_smc_too() {
        let reads = Arc::new(AtomicUsize::new(0));
        let cache = HeatCache::new(MAX_AGE, counted(&reads, None, Duration::ZERO));
        let now = Instant::now();
        assert_eq!(cache.get(now), None);
        assert_eq!(cache.get(now), None);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn heat_row_carries_every_fan_field() {
        let row = to_row(&tcr_smc::HeatReading {
            cpu_celsius: Some(93.25),
            fans: vec![tcr_smc::FanReading {
                index: tcr_smc::FanIndex::new(1).unwrap(),
                actual_rpm: 5013.0,
                min_rpm: 1350.0,
                max_rpm: 5777.0,
                mode: tcr_smc::FanMode::Manual,
                target_rpm: Some(5000.0),
            }],
            ftst: Some(true),
        });
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            serde_json::json!({
                "cpuCelsius": 93.25,
                "fans": [{"index": 1, "actualRpm": 5013.0, "minRpm": 1350.0,
                          "maxRpm": 5777.0, "manual": true, "targetRpm": 5000.0}],
                "ftst": true,
            })
        );
    }

    /// The controller's read is always fresh and answers the next status
    /// request, so a manual mode costs no second SMC read per poll.
    #[test]
    fn heat_refresh_reads_now_and_feeds_the_next_get() {
        let reads = Arc::new(AtomicUsize::new(0));
        let cache = HeatCache::new(
            MAX_AGE,
            counted(&reads, Some(reading(61.0)), Duration::ZERO),
        );
        let start = Instant::now();
        assert_eq!(cache.get(start), Some(reading(61.0)));
        assert_eq!(cache.refresh(start), Some(reading(61.0)));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "refresh reads even when fresh"
        );
        assert_eq!(
            cache.get(start + Duration::from_millis(10)),
            Some(reading(61.0))
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }
}
