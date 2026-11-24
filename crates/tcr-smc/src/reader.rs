//! The reader: CPU temperature and every fan, from one key source, with the
//! per-Mac facts it finds once and keeps.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::codec::{FourCc, KeySource, SmcError};
use crate::fan::{
    FAN_COUNT_KEY, FanIndex, FanMode, LOWER_MODE_PROBE_KEY, MAX_FANS, UNLOCK_KEY, fan_key,
    mode_key_suffix,
};

/// One fan, as read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FanReading {
    /// 0-based, the `n` in `F{n}Ac`.
    pub index: FanIndex,
    pub actual_rpm: f64,
    /// 0 when it did not read this time.
    pub min_rpm: f64,
    /// 0 when it did not read this time: "no maximum", not a ceiling of 0.
    pub max_rpm: f64,
    /// The fan's mode key, three ways: 1 is manual (a program fixed its
    /// speed, so it no longer follows heat), 0 and 3 are both automatic
    /// (Stats' `FanMode`), and a key that did not read is
    /// [`FanMode::Unreadable`], never "automatic". A fan controller judges
    /// whose the fans are from this, so a failed read must not look like macOS.
    pub mode: FanMode,
    /// `F{n}Tg`, the speed the fan is told to hold. `None` when it did not
    /// read this time.
    pub target_rpm: Option<f64>,
}

impl FanReading {
    /// The mode key reads 1.
    pub fn is_manual(&self) -> bool {
        self.mode == FanMode::Manual
    }
}

/// How hot the CPU is and how hard the fans work.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HeatReading {
    /// The hottest CPU sensor, °C, always within [`PLAUSIBLE_CELSIUS`].
    /// `None` when this Mac has neither `TCMz` nor any readable `Tp*` sensor,
    /// or when none of them read this time.
    pub cpu_celsius: Option<f64>,
    /// Empty on a Mac with no fans (`FNum` 0, a MacBook Air), and missing any
    /// fan whose speed did not read.
    pub fans: Vec<FanReading>,
    /// `Ftst`, the unlock flag a fan writer raises: `Some(true)` when it reads
    /// other than 0. `None` on a Mac without it, or when it did not read.
    pub ftst: Option<bool>,
}

/// A sensor that reads 0 or below, or past 150 °C, is one not fitted or not
/// powered, not a hot CPU; the reading drops it, whichever key it came from.
/// NaN is outside every range.
pub const PLAUSIBLE_CELSIUS: std::ops::RangeInclusive<f64> = 0.5..=150.0;

/// How long a failed `Tp*` enumeration waits before the next try. A starting
/// value, not a measured one: long enough that a key table that keeps failing
/// does not cost 3,462 kernel calls every poll.
pub const ENUMERATION_RETRY_DELAY: Duration = Duration::from_secs(60);

const CPU_MAX_KEY: FourCc = FourCc::new(*b"TCMz");

/// `Some(celsius)` when it is inside [`PLAUSIBLE_CELSIUS`].
pub fn plausible(celsius: f64) -> Option<f64> {
    PLAUSIBLE_CELSIUS.contains(&celsius).then_some(celsius)
}

/// Where a reader's "now" comes from: time since the reader was made, so a
/// test can hand in its own clock.
pub type Clock = Box<dyn Fn() -> Duration + Send>;
/// Where a reader's one-per-distinct-error lines go.
pub type Report = Box<dyn FnMut(&str) + Send>;

/// The reader: one key source, and the per-Mac facts (where the CPU
/// temperature lives, which spelling the fan mode key uses, each fan's
/// limits) found once and kept.
///
/// Not locked: [`HeatReader::read`] takes `&mut self`, so a caller sharing one
/// puts it behind its own lock.
pub struct HeatReader<S> {
    smc: S,
    now: Clock,
    report: Report,
    /// `TCMz` is a 4-byte `flt ` on this Mac (`Some(true)`), is not or is
    /// absent (`Some(false)`), or has not answered yet (`None`).
    use_max_key: Option<bool>,
    /// The `Tp*` sensors, once an enumeration has succeeded.
    processor_keys: Option<Vec<FourCc>>,
    enumeration_failed_at: Option<Duration>,
    mode_key_is_lower: Option<bool>,
    /// Each fan's `Mn` and `Mx`, kept once both read: they do not change.
    fan_limits: HashMap<FanIndex, (f64, f64)>,
    /// Every line already reported, so a key that fails on every poll is one
    /// log line, not one a poll.
    reported: HashSet<String>,
}

impl<S: KeySource> HeatReader<S> {
    /// A reader on `smc` with the real clock, reporting through `report`.
    pub fn new(smc: S, report: Report) -> Self {
        let start = Instant::now();
        Self::with_clock(smc, Box::new(move || start.elapsed()), report)
    }

    pub fn with_clock(smc: S, now: Clock, report: Report) -> Self {
        Self {
            smc,
            now,
            report,
            use_max_key: None,
            processor_keys: None,
            enumeration_failed_at: None,
            mode_key_is_lower: None,
            fan_limits: HashMap::new(),
            reported: HashSet::new(),
        }
    }

    /// The key source, for a test that counts what was asked of it.
    pub const fn source(&self) -> &S {
        &self.smc
    }

    /// Mutable access to the key source, for a test that changes what it serves.
    pub const fn source_mut(&mut self) -> &mut S {
        &mut self.smc
    }

    /// Read now. Every error is reported once per distinct error, and the
    /// reading carries whatever did read: a bad fan key never removes the CPU,
    /// and the reverse.
    pub fn read(&mut self) -> HeatReading {
        HeatReading {
            cpu_celsius: self.read_cpu(),
            fans: self.read_fans(),
            ftst: self.read_ftst(),
        }
    }

    /// `Ftst`, one key. Absent is `None` and quiet; any other failure is
    /// reported once and `None`.
    fn read_ftst(&mut self) -> Option<bool> {
        match self.smc.read(UNLOCK_KEY).and_then(|v| v.uint8_value()) {
            Ok(value) => Some(value != 0),
            Err(SmcError::KeyNotFound(_)) => None,
            Err(error) => {
                self.note(&format!("{UNLOCK_KEY} read"), &error);
                None
            }
        }
    }

    fn note(&mut self, what: &str, error: &SmcError) {
        let line = format!("SMC: {what}: {error}");
        if !self.reported.contains(&line) {
            (self.report)(&line);
            self.reported.insert(line);
        }
    }

    // CPU

    /// `TCMz` when this Mac has it as a 4-byte `flt `; otherwise the hottest
    /// `Tp*` sensor.
    fn read_cpu(&mut self) -> Option<f64> {
        if self.use_max_key != Some(false) {
            match self.smc.read(CPU_MAX_KEY) {
                Ok(value) if value.ty == FourCc::FLT && value.bytes.len() == 4 => {
                    self.use_max_key = Some(true);
                    return match value.float_value() {
                        Ok(celsius) => plausible(f64::from(celsius)),
                        Err(error) => {
                            self.note(&format!("{CPU_MAX_KEY} read"), &error);
                            None
                        }
                    };
                }
                // Some other type or size: not the key this reader knows.
                Ok(_) | Err(SmcError::KeyNotFound(_)) => self.use_max_key = Some(false),
                Err(error) => {
                    self.note(&format!("{CPU_MAX_KEY} read"), &error);
                    // A key that has read before failed this once: no figure
                    // this poll. One never yet read: try the sensors, ask
                    // again next poll.
                    if self.use_max_key == Some(true) {
                        return None;
                    }
                }
            }
        }
        let keys = self.resolved_processor_keys()?;
        let mut hottest: Option<f64> = None;
        for key in keys {
            let celsius = match self.smc.read(key).and_then(|value| value.float_value()) {
                Ok(celsius) => f64::from(celsius),
                Err(error) => {
                    self.note(&format!("{key} read"), &error);
                    continue;
                }
            };
            let Some(celsius) = plausible(celsius) else {
                continue;
            };
            hottest = Some(hottest.map_or(celsius, |known| known.max(celsius)));
        }
        hottest
    }

    /// The `Tp*` sensors, enumerated once: 3,462 keys on every poll is not a
    /// price to pay for one number. A failed enumeration is NOT kept: it is
    /// reported and tried again no sooner than [`ENUMERATION_RETRY_DELAY`]
    /// later.
    fn resolved_processor_keys(&mut self) -> Option<Vec<FourCc>> {
        if let Some(keys) = &self.processor_keys {
            return Some(keys.clone());
        }
        if let Some(failed_at) = self.enumeration_failed_at
            && (self.now)().saturating_sub(failed_at) < ENUMERATION_RETRY_DELAY
        {
            return None;
        }
        match self.processor_sensor_keys() {
            Ok(keys) => {
                self.processor_keys = Some(keys.clone());
                self.enumeration_failed_at = None;
                Some(keys)
            }
            Err(error) => {
                self.note(
                    &format!(
                        "key table enumeration (retrying in {} s)",
                        ENUMERATION_RETRY_DELAY.as_secs()
                    ),
                    &error,
                );
                self.enumeration_failed_at = Some((self.now)());
                None
            }
        }
    }

    /// Every `Tp*` key holding a 4-byte `flt ` (102 on a Mac16,7). A key that
    /// will not read now is kept, reported, and read again on the next poll:
    /// one failure is not a reason to drop a sensor for the life of the
    /// process.
    fn processor_sensor_keys(&mut self) -> Result<Vec<FourCc>, SmcError> {
        let tp = u32::from(u16::from_be_bytes(*b"Tp"));
        let mut kept = Vec::new();
        for key in self.smc.keys()? {
            if key.0 >> 16 != tp {
                continue;
            }
            match self.smc.read(key) {
                Ok(value) => {
                    if value.ty == FourCc::FLT && value.bytes.len() == 4 {
                        kept.push(key);
                    }
                }
                Err(error) => {
                    self.note(&format!("{key} read"), &error);
                    kept.push(key);
                }
            }
        }
        Ok(kept)
    }

    // Fans

    fn read_fans(&mut self) -> Vec<FanReading> {
        let count = match self.smc.read(FAN_COUNT_KEY).and_then(|v| v.uint8_value()) {
            Ok(count) => count,
            Err(SmcError::KeyNotFound(_)) => return Vec::new(),
            Err(error) => {
                self.note(&format!("{FAN_COUNT_KEY} read"), &error);
                return Vec::new();
            }
        };
        // Ten fans is the most a fan key's single digit can name.
        (0..count.min(MAX_FANS))
            .filter_map(|fan| FanIndex::new(fan).and_then(|fan| self.read_fan(fan)))
            .collect()
    }

    /// `None` when the fan's speed does not read, or reads as no speed at all
    /// (not finite, or negative).
    fn read_fan(&mut self, fan: FanIndex) -> Option<FanReading> {
        let actual_rpm = self.rpm(fan, *b"Ac")?;
        let (min_rpm, max_rpm) = self.limits(fan);
        Some(FanReading {
            index: fan,
            actual_rpm,
            min_rpm,
            max_rpm,
            mode: self.mode(fan),
            target_rpm: self.target(fan),
        })
    }

    /// `F{n}Tg`, read every time: another program may move it at any moment,
    /// which is exactly what a fan controller needs to see. Any number reads,
    /// so a controller compares what was written with what is there.
    fn target(&mut self, fan: FanIndex) -> Option<f64> {
        let key = fan_key(fan, *b"Tg");
        match self.smc.read(key).and_then(|v| v.numeric_value()) {
            Ok(value) => value.is_finite().then_some(value),
            Err(SmcError::KeyNotFound(_)) => None,
            Err(error) => {
                self.note(&format!("{key} read"), &error);
                None
            }
        }
    }

    /// `Mn` and `Mx`, read once both read and then kept. One that does not
    /// read is 0 this poll ([`FanReading::max_rpm`] 0 is "no maximum") and is
    /// asked again next poll.
    fn limits(&mut self, fan: FanIndex) -> (f64, f64) {
        if let Some(known) = self.fan_limits.get(&fan) {
            return *known;
        }
        let low = self.rpm(fan, *b"Mn");
        let high = self.rpm(fan, *b"Mx");
        let (Some(low), Some(high)) = (low, high) else {
            return (low.unwrap_or(0.0), high.unwrap_or(0.0));
        };
        self.fan_limits.insert(fan, (low, high));
        (low, high)
    }

    fn rpm(&mut self, fan: FanIndex, suffix: [u8; 2]) -> Option<f64> {
        let key = fan_key(fan, suffix);
        let value = match self.smc.read(key).and_then(|v| v.numeric_value()) {
            Ok(value) => value,
            Err(error) => {
                self.note(&format!("{key} read"), &error);
                return None;
            }
        };
        (value.is_finite() && value >= 0.0).then_some(value)
    }

    /// The fan's mode key, three ways, as
    /// [`FanWriter::mode`](crate::FanWriter::mode) reads it. No mode
    /// key at all (older Intel Macs keep the bit in `FS! `) is
    /// [`FanMode::Unreadable`]: nothing says who holds the fan, so a
    /// controller writes nothing to it.
    fn mode(&mut self, fan: FanIndex) -> FanMode {
        let suffix = match mode_key_suffix(&mut self.smc, &mut self.mode_key_is_lower) {
            Ok(suffix) => suffix,
            Err(error) => {
                self.note(&format!("{LOWER_MODE_PROBE_KEY} read"), &error);
                return FanMode::Unreadable;
            }
        };
        let key = fan_key(fan, suffix);
        match self.smc.read(key).and_then(|v| v.uint8_value()) {
            Ok(value) => FanMode::from_ui8(value),
            Err(SmcError::KeyNotFound(_)) => FanMode::Unreadable,
            Err(error) => {
                self.note(&format!("{key} read"), &error);
                FanMode::Unreadable
            }
        }
    }
}
