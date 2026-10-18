//! The serving Mac's heat, for [`crate::status::StatusPayload::heat`]: CPU
//! temperature and fan speeds, read through AppleSMC by the `tcr-smc` crate.
//!
//! # Off the runtime, and briefly cached
//!
//! One read is a handful of kernel calls into AppleSMC (and, on a Mac without
//! `TCMz`, a one-time walk of a key table thousands of entries long), so it runs
//! on tokio's blocking pool, never on a runtime worker. The panel polls the
//! status endpoint every few seconds, and more than one client may poll it, so
//! a reading is kept for [`MAX_AGE`]: a burst of status requests inside that
//! window costs one SMC read, and the lock is held across the read so the
//! requests that queue behind it get its answer rather than starting their own.
//!
//! Heat is not a secret, so the endpoint's no-secret invariant
//! ([`crate::status`]) is unchanged by carrying it.

use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tcr_status_wire::{FanRow, HeatRow};

/// How long one reading answers for. A starting value, not a measured one:
/// short against the panel's three-second poll, so the figure is never more
/// than a poll old, and long enough that two clients polling together read
/// the SMC once.
pub const MAX_AGE: Duration = Duration::from_secs(2);

/// What a cache reads with: `None` is "this machine has no AppleSMC".
pub type Read = Box<dyn FnMut() -> Option<HeatRow> + Send>;

/// One reader and its last answer, behind one lock.
pub struct HeatCache {
    max_age: Duration,
    state: Mutex<State>,
}

struct State {
    read: Read,
    last: Option<(Instant, Option<HeatRow>)>,
}

impl HeatCache {
    pub fn new(max_age: Duration, read: Read) -> Self {
        Self {
            max_age,
            state: Mutex::new(State { read, last: None }),
        }
    }

    /// The reading as of `now`: the kept one while it is younger than the
    /// cache's maximum age, a fresh one otherwise. Blocking: call it off the
    /// async runtime.
    ///
    /// "No AppleSMC" is kept like any other answer, so a VM pays for the
    /// question once per window too.
    pub fn get(&self, now: Instant) -> Option<HeatRow> {
        // A panic inside a read poisons the lock. The state it guards is a
        // reader and a cached answer, both still whole, so the next request
        // reads on rather than losing heat for the life of the process.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, reading)) = &state.last
            && now.saturating_duration_since(*at) < self.max_age
        {
            return reading.clone();
        }
        let reading = (state.read)();
        state.last = Some((now, reading.clone()));
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
                index: fan.index,
                actual_rpm: fan.actual_rpm,
                min_rpm: fan.min_rpm,
                max_rpm: fan.max_rpm,
                manual: fan.is_manual,
            })
            .collect(),
    }
}

/// The process's one cache, on the process's one AppleSMC connection, opened
/// on the first status request that asks. The reader reports each distinct
/// SMC error once, as a warning.
static PROCESS: LazyLock<HeatCache> = LazyLock::new(|| {
    let mut source: Option<tcr_smc::HeatSource> = None;
    HeatCache::new(
        MAX_AGE,
        Box::new(move || {
            source
                .get_or_insert_with(|| {
                    tcr_smc::HeatSource::open(Box::new(|line| tracing::warn!("{line}")))
                })
                .read()
                .as_ref()
                .map(to_row)
        }),
    )
});

/// The machine's heat now, read on the blocking pool through the process's
/// cache. `None` when there is no AppleSMC (a VM, or not a Mac), and when the
/// blocking task itself failed, which is logged.
pub async fn current() -> Option<HeatRow> {
    match tokio::task::spawn_blocking(|| PROCESS.get(Instant::now())).await {
        Ok(reading) => reading,
        Err(error) => {
            tracing::warn!(%error, "the heat read did not finish");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn counted(reads: &Arc<AtomicUsize>, row: Option<HeatRow>, pause: Duration) -> Read {
        let reads = Arc::clone(reads);
        Box::new(move || {
            reads.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(pause);
            row.clone()
        })
    }

    fn reading() -> HeatRow {
        HeatRow {
            cpu_celsius: Some(61.5),
            fans: vec![FanRow {
                index: 0,
                actual_rpm: 2400.0,
                min_rpm: 1350.0,
                max_rpm: 5777.0,
                manual: false,
            }],
        }
    }

    /// Eight requests at once, each waiting on a read that takes 50 ms: one
    /// SMC read between them, and every one of them gets its answer.
    #[test]
    fn heat_cache_serves_a_burst_with_one_read() {
        let reads = Arc::new(AtomicUsize::new(0));
        let cache = Arc::new(HeatCache::new(
            MAX_AGE,
            counted(&reads, Some(reading()), Duration::from_millis(50)),
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
        assert!(answers.iter().all(|a| *a == Some(reading())), "{answers:?}");
    }

    /// A reading answers for less than the maximum age and no longer.
    #[test]
    fn heat_cache_reads_again_once_the_reading_is_old() {
        let reads = Arc::new(AtomicUsize::new(0));
        let cache = HeatCache::new(MAX_AGE, counted(&reads, Some(reading()), Duration::ZERO));
        let start = Instant::now();
        assert_eq!(cache.get(start), Some(reading()));
        assert_eq!(
            cache.get(start + Duration::from_millis(1999)),
            Some(reading())
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(cache.get(start + MAX_AGE), Some(reading()));
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
                index: 1,
                actual_rpm: 5013.0,
                min_rpm: 1350.0,
                max_rpm: 5777.0,
                is_manual: true,
            }],
        });
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            serde_json::json!({
                "cpuCelsius": 93.25,
                "fans": [{"index": 1, "actualRpm": 5013.0, "minRpm": 1350.0,
                          "maxRpm": 5777.0, "manual": true}],
            })
        );
    }
}
