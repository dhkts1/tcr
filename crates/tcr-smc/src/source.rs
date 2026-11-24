//! A process's one reader: the platform's key source, opened on the first
//! read and opened again after a failure.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

#[cfg(not(target_os = "macos"))]
use crate::codec::{FourCc, SmcValue};
use crate::codec::{KeySource, SmcError};
#[cfg(target_os = "macos")]
use crate::macos::SmcConnection;
use crate::reader::{Clock, ENUMERATION_RETRY_DELAY, HeatReader, HeatReading, Report};

/// What [`HeatSource::open`] opens: AppleSMC on a Mac.
#[cfg(target_os = "macos")]
pub type PlatformSmc = SmcConnection;
/// What [`HeatSource::open`] opens: nothing, off a Mac.
#[cfg(not(target_os = "macos"))]
pub type PlatformSmc = NoSmc;

/// The key source off a Mac. There is none, so no value of this type exists.
#[cfg(not(target_os = "macos"))]
pub enum NoSmc {}

#[cfg(not(target_os = "macos"))]
impl KeySource for NoSmc {
    fn read(&mut self, _key: FourCc) -> Result<SmcValue, SmcError> {
        match *self {}
    }

    fn keys(&mut self) -> Result<Vec<FourCc>, SmcError> {
        match *self {}
    }
}

/// How a [`HeatSource`] opens its key source.
pub type Opener<S> = Box<dyn FnMut() -> Result<S, SmcError> + Send>;

enum Opened<S> {
    /// Nothing tried yet: the first [`HeatSource::read`] opens.
    NotYet,
    Ready(Box<HeatReader<S>>),
    /// No AppleSMC: a VM, or not a Mac. Nothing about that changes while the
    /// process runs, so it is never asked again.
    Unavailable,
    /// AppleSMC is there and would not open, at this time on the source's
    /// clock. Tried again [`ENUMERATION_RETRY_DELAY`] later: a refusal at
    /// boot is not a reason to go without heat for the life of the process.
    Failed {
        at: Duration,
    },
}

/// A process's one reader, opened on the first read, or why there is none.
pub struct HeatSource<S> {
    opened: Opened<S>,
    open: Opener<S>,
    /// Shared with the reader once one opens, so both the open failures and
    /// the reader's own errors go to the one place.
    report: Arc<Mutex<Report>>,
    now: Clock,
    /// The last open failure reported, so one that repeats every retry is
    /// one line, not one a minute.
    last_failure: Option<SmcError>,
}

impl HeatSource<PlatformSmc> {
    /// The platform's source, with the real clock. Opens nothing yet.
    pub fn open(report: Report) -> Self {
        #[cfg(target_os = "macos")]
        let open: Opener<PlatformSmc> = Box::new(SmcConnection::open);
        #[cfg(not(target_os = "macos"))]
        let open: Opener<PlatformSmc> = Box::new(|| Err(SmcError::Unavailable));
        let start = Instant::now();
        Self::with_opener(open, report, Box::new(move || start.elapsed()))
    }
}

impl<S: KeySource> HeatSource<S> {
    pub fn with_opener(open: Opener<S>, report: Report, now: Clock) -> Self {
        Self {
            opened: Opened::NotYet,
            open,
            report: Arc::new(Mutex::new(report)),
            now,
            last_failure: None,
        }
    }

    fn try_open(&mut self) {
        match (self.open)() {
            Ok(smc) => {
                let shared = Arc::clone(&self.report);
                let report: Report = Box::new(move |line| {
                    let mut report = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    report(line);
                });
                self.opened = Opened::Ready(Box::new(HeatReader::new(smc, report)));
                self.last_failure = None;
            }
            Err(SmcError::Unavailable) => self.opened = Opened::Unavailable,
            Err(error) => {
                if self.last_failure != Some(error) {
                    let mut report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
                    report(&format!(
                        "SMC: AppleSMC would not open (retrying every {} s): {error}",
                        ENUMERATION_RETRY_DELAY.as_secs()
                    ));
                    self.last_failure = Some(error);
                }
                self.opened = Opened::Failed { at: (self.now)() };
            }
        }
    }

    /// Read now. `None` means one thing: this machine has no AppleSMC. An SMC
    /// that would not open is an empty reading, not `None`, and is opened
    /// again once [`ENUMERATION_RETRY_DELAY`] has passed.
    pub fn read(&mut self) -> Option<HeatReading> {
        let due = match &self.opened {
            Opened::NotYet => true,
            Opened::Failed { at } => (self.now)().saturating_sub(*at) >= ENUMERATION_RETRY_DELAY,
            Opened::Ready(_) | Opened::Unavailable => false,
        };
        if due {
            self.try_open();
        }
        match &mut self.opened {
            Opened::Ready(reader) => Some(reader.read()),
            Opened::Unavailable => None,
            Opened::NotYet | Opened::Failed { .. } => Some(HeatReading::default()),
        }
    }
}
