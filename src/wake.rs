//! Noticing that the machine slept, so a pooled upstream connection that died
//! during the sleep is retired before a request is sent on it.
//!
//! A Mac on battery sleeps and DarkWakes roughly every 15 minutes. While it
//! sleeps, Anthropic's edge (or a NAT on the way) closes the idle connections
//! tcr keeps pooled. On wake, the first send per account checks one of them out
//! and fails with `ConnectionReset`; one day's log held 935 such same-account
//! retries and 311 relay 502s, 1078 of 1246 within 10 s of a `Wake`/`DarkWake`
//! line in `pmset -g log`.
//!
//! The pool cannot see it on its own. `std::time::Instant`, and with it every
//! tokio and hyper timer, reads `CLOCK_UPTIME_RAW` on macOS (`CLOCK_MONOTONIC`
//! on Linux), and neither advances while the machine is asleep. A connection
//! idle for 60 s before a 15-minute sleep still looks 60 s idle afterwards, so
//! neither `pool_idle_timeout` nor the h2 keep-alive ping has fired by the first
//! send. The wall clock does advance across a sleep, so the gap between the two
//! clocks over one observation interval is how long the machine was asleep.
//!
//! Each client holder owns its own [`WakeDetector`] rather than sharing one
//! process-wide epoch: every holder observes on its own send path, so each
//! learns about every wake by itself, and a test that injects a wall-clock
//! offset into one holder's detector cannot make a parallel test's holder see a
//! sleep.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

/// How far the wall clock must run ahead of the monotonic clock over one
/// observation interval before it counts as a sleep.
///
/// 10 s sits between the two things it separates. Below it: clock noise. NTP on
/// a synced machine slews or steps by milliseconds to low single seconds, and
/// reading the two clocks a few instructions apart under heavy load differs by
/// microseconds. Above it: the sleeps that matter, which are minutes long (the
/// DarkWake cadence is ~15 minutes). A false positive (a large forward NTP
/// step) costs one TCP+TLS handshake per client and nothing else, so erring
/// low is cheap; erring high only misses sleeps short enough that the pooled
/// connection most likely survived them.
pub const SLEEP_THRESHOLD: Duration = Duration::from_secs(10);

/// How long the machine slept between two observations, or `None` if it did
/// not.
///
/// `mono_elapsed` is the [`Instant`] delta, `wall_elapsed` the [`SystemTime`]
/// delta, `None` when the wall clock stepped BACKWARDS (a manual clock change
/// or an NTP correction). A backwards step is not a sleep: sleeping only ever
/// moves the wall clock forward relative to the monotonic one.
pub fn slept_between(mono_elapsed: Duration, wall_elapsed: Option<Duration>) -> Option<Duration> {
    let gap = wall_elapsed?.checked_sub(mono_elapsed)?;
    (gap > SLEEP_THRESHOLD).then_some(gap)
}

/// Remembers the last (monotonic, wall) pair seen and reports a sleep when the
/// next observation finds the wall clock ran ahead by more than
/// [`SLEEP_THRESHOLD`].
///
/// Exactly one caller sees each sleep: the pair is read and replaced under one
/// lock, so two sends observing at once cannot both report it.
pub struct WakeDetector {
    last: Mutex<(Instant, SystemTime)>,
    /// Test-only seconds added to every wall-clock reading, so a test can
    /// simulate a sleep without sleeping a Mac.
    #[cfg(test)]
    wall_offset_secs: std::sync::atomic::AtomicU64,
}

impl Default for WakeDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeDetector {
    /// A detector whose first observation is measured from now.
    pub fn new() -> Self {
        Self {
            last: Mutex::new((Instant::now(), SystemTime::now())),
            #[cfg(test)]
            wall_offset_secs: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Record the current clocks and return how long the machine slept since
    /// the previous observation, if it did. Cheap enough for every send: one
    /// uncontended lock and two clock reads.
    pub fn observe(&self) -> Option<Duration> {
        self.observe_at(Instant::now(), self.wall_now())
    }

    fn observe_at(&self, mono: Instant, wall: SystemTime) -> Option<Duration> {
        // The guarded value is a plain pair that is always whole, so a panic
        // elsewhere while it was held leaves nothing half-written to refuse.
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let (prev_mono, prev_wall) = *last;
        *last = (mono, wall);
        drop(last);
        slept_between(
            mono.saturating_duration_since(prev_mono),
            wall.duration_since(prev_wall).ok(),
        )
    }

    #[cfg(not(test))]
    fn wall_now(&self) -> SystemTime {
        SystemTime::now()
    }

    #[cfg(test)]
    fn wall_now(&self) -> SystemTime {
        let offset = self
            .wall_offset_secs
            .load(std::sync::atomic::Ordering::SeqCst);
        SystemTime::now() + Duration::from_secs(offset)
    }

    /// Move this detector's wall clock forward by `by`, as a sleep of that
    /// length would. Test-only.
    #[cfg(test)]
    pub(crate) fn inject_sleep(&self, by: Duration) {
        self.wall_offset_secs
            .fetch_add(by.as_secs(), std::sync::atomic::Ordering::SeqCst);
    }
}

/// One shared client that is rebuilt the first time it is asked for after the
/// machine slept, so the first request after a wake opens a fresh connection
/// instead of checking out one the sleep killed.
///
/// Callers that already hold the old `Arc` keep it, and the connections under
/// it, until they drop it; only requests that ask after the wake get the new
/// one.
pub struct SleepAwareClient<T> {
    what: &'static str,
    build: fn() -> Option<T>,
    detector: WakeDetector,
    current: Mutex<Option<Arc<T>>>,
}

impl<T> SleepAwareClient<T> {
    /// `what` names the client in the one log line a rebuild writes. `build`
    /// returns `None` when the client cannot be built; it logs its own reason,
    /// and the next call tries again.
    pub fn new(what: &'static str, build: fn() -> Option<T>) -> Self {
        Self {
            what,
            build,
            detector: WakeDetector::new(),
            current: Mutex::new(None),
        }
    }

    /// The client to send on now. Rebuilt when the machine slept since the last
    /// call, and when no client exists yet.
    pub fn get(&self) -> Option<Arc<T>> {
        let slept = self.detector.observe();
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slept) = slept {
            if current.take().is_some() {
                tracing::debug!(
                    client = self.what,
                    slept_secs = slept.as_secs(),
                    "the machine slept; retiring this client's pooled connections"
                );
            }
        }
        if current.is_none() {
            *current = (self.build)().map(Arc::new);
        }
        current.clone()
    }

    #[cfg(test)]
    pub(crate) fn detector(&self) -> &WakeDetector {
        &self.detector
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn a_wall_clock_far_ahead_of_the_monotonic_one_is_a_sleep() {
        assert_eq!(
            slept_between(SECOND, Some(Duration::from_secs(900))),
            Some(Duration::from_secs(899)),
            "900 s of wall time over 1 s of monotonic time is an 899 s sleep"
        );
    }

    #[test]
    fn clocks_that_agree_are_not_a_sleep() {
        assert_eq!(
            slept_between(Duration::from_secs(60), Some(Duration::from_secs(60))),
            None,
            "equal deltas mean the machine was awake the whole interval"
        );
    }

    #[test]
    fn a_gap_at_or_under_the_threshold_is_not_a_sleep() {
        assert_eq!(
            slept_between(SECOND, Some(SECOND + SLEEP_THRESHOLD)),
            None,
            "a gap of exactly the threshold is clock noise, not a sleep"
        );
        assert_eq!(
            slept_between(SECOND, Some(Duration::from_secs(5))),
            None,
            "a 4 s gap is under the threshold"
        );
    }

    #[test]
    fn a_wall_clock_stepping_backwards_is_not_a_sleep() {
        assert_eq!(
            slept_between(Duration::from_secs(900), None),
            None,
            "a backwards wall step is a clock change, not a sleep"
        );
        assert_eq!(
            slept_between(Duration::from_secs(900), Some(SECOND)),
            None,
            "a wall clock BEHIND the monotonic one is a backwards step too"
        );
    }

    #[test]
    fn the_detector_reports_an_injected_sleep_once() {
        let detector = WakeDetector::new();
        assert_eq!(detector.observe(), None, "no sleep yet");
        detector.inject_sleep(Duration::from_secs(900));
        assert!(
            detector
                .observe()
                .is_some_and(|d| d >= Duration::from_secs(899)),
            "the observation after a 900 s jump reports the sleep"
        );
        assert_eq!(
            detector.observe(),
            None,
            "the same sleep is reported once, not on every later observation"
        );
    }
}
