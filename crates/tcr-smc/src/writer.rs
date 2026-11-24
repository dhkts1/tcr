//! The fan writer: the writes that take fans from macOS and give them back.

use std::fmt;
use std::time::Duration;

use crate::codec::{FourCc, KeyAccess, SmcError, SmcValue};
use crate::fan::{
    FAN_COUNT_KEY, FanIndex, FanMode, MAX_FANS, UNLOCK_KEY, fan_key, mode_key_suffix,
};

/// How long [`FanWriter::set_manual`] waits after raising `Ftst` for macOS's
/// thermal daemon to let go of the fans. Stats waits the same
/// (`SMC/smc.swift:590`, `usleep(3_000_000)`). Unmeasured here.
pub const UNLOCK_SETTLE: Duration = Duration::from_secs(3);
/// After [`UNLOCK_SETTLE`], the mode write is tried this many times,
/// [`MODE_RETRY_GAP`] apart. Stats allows 300; 50 bounds a stuck unlock to
/// about 8 s. Unmeasured.
pub const MODE_RETRIES: usize = 50;
pub const MODE_RETRY_GAP: Duration = Duration::from_millis(100);
/// Every hand-back write is tried this many times, [`HAND_BACK_GAP`] apart, as
/// Stats' `writeWithRetry` does: it is the write that must not be lost.
/// Unmeasured.
pub const HAND_BACK_TRIES: usize = 10;
pub const HAND_BACK_GAP: Duration = Duration::from_millis(50);
/// After the hand-back's writes, the keys are re-read up to this many times,
/// [`HAND_BACK_SETTLE_GAP`] apart, until every one reads 0: the SMC takes a
/// moment to apply a mode write. Measured 2026-10-18 on an M4 Pro: a single
/// re-read at once found both mode keys still 1, and a read about one second
/// later found `F0Md`, `F1Md` and `Ftst` all 0. Twenty polls 100 ms apart wait
/// up to about 2 s, twice that, and keep the whole hand-back inside the
/// controller's 5 s deadline even with every write retried.
pub const HAND_BACK_SETTLE_POLLS: usize = 20;
pub const HAND_BACK_SETTLE_GAP: Duration = Duration::from_millis(100);

/// Where a [`FanWriter`]'s waits go: [`std::thread::sleep`] in a helper, a
/// recorder in a test, so the 3 s unlock takes no time and every wait counts.
pub type Sleeper = Box<dyn FnMut(Duration) + Send>;

/// One fan's own speed range, from `F{n}Mn` and `F{n}Mx`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FanLimits {
    pub min_rpm: f64,
    pub max_rpm: f64,
}

impl FanLimits {
    /// `rpm` inside the range. NaN and the infinities are refused, never
    /// clamped: a caller sending one has a bug, and full speed or minimum are
    /// both guesses about what it meant.
    pub const fn clamp(self, rpm: f64) -> Result<f64, FanError> {
        if !rpm.is_finite() {
            return Err(FanError::NotANumber);
        }
        Ok(rpm.max(self.min_rpm).min(self.max_rpm))
    }
}

/// After a hand-back, what still did not read 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandBackIncomplete {
    /// Bit `n` set: fan `n`'s mode key still reads other than 0, or did not
    /// read. A fan with no mode key at all has nothing to hand back.
    pub fans: u16,
    /// `Ftst` still reads other than 0, or did not read. `false` when absent.
    pub ftst: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FanError {
    /// `fan` is not below `count`, this Mac's `FNum` capped at [`MAX_FANS`].
    NoSuchFan {
        fan: FanIndex,
        count: u8,
    },
    /// An rpm that is NaN or infinite, refused rather than clamped.
    NotANumber,
    /// `Mn` or `Mx` is not finite, or the minimum is above the maximum: no
    /// range to clamp into, and no guess at one.
    LimitsUnreadable {
        fan: FanIndex,
    },
    /// The mode key stayed automatic through the whole unlock sequence, or
    /// was refused on a Mac with no `Ftst` to unlock it with.
    ModeLocked {
        fan: FanIndex,
    },
    /// A read or write failed: the SMC's own error, kept whole. A wrong type
    /// or size is a bug in the writer or a Mac it does not know, never a lock.
    Smc(SmcError),
    HandBackIncomplete(HandBackIncomplete),
}

impl fmt::Display for FanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchFan { fan, count } => {
                write!(f, "no fan {} (this Mac has {count})", fan.get())
            }
            Self::NotANumber => write!(f, "rpm is not a finite number"),
            Self::LimitsUnreadable { fan } => {
                write!(f, "fan {}'s Mn/Mx are not a usable range", fan.get())
            }
            Self::ModeLocked { fan } => write!(f, "fan {} would not leave automatic", fan.get()),
            Self::Smc(error) => write!(f, "SMC: {error}"),
            Self::HandBackIncomplete(HandBackIncomplete { fans, ftst }) => write!(
                f,
                "hand-back incomplete: fans {fans:#06x} not automatic, Ftst raised: {ftst}"
            ),
        }
    }
}

impl std::error::Error for FanError {}

impl From<SmcError> for FanError {
    fn from(error: SmcError) -> Self {
        Self::Smc(error)
    }
}

/// [`FanWriter::set_manual`] succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entered {
    /// This call raised `Ftst` (the mode key refused the direct write).
    pub unlocked: bool,
}

/// [`FanWriter::set_manual`] failed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetManualFailed {
    pub cause: FanError,
    /// `None` when the call failed before trying any write (no such fan, or
    /// `FNum` did not read), so there was nothing to undo. Otherwise every fan
    /// was handed back before returning, and this is how that went.
    pub hand_back: Option<Result<(), FanError>>,
}

/// The writes that take fans out of macOS's hands and give them back, in the
/// order Apple Silicon wants. A port of `FanWriter` in the Swift
/// `FanControlPolicy.swift` (`e642c541`), with the hand-back order of Stats
/// (exelban/stats, MIT, `SMC/smc.swift`: modes first in `setFanMode`, then
/// `Ftst` in `resetFanControl`), and a re-read after it.
///
/// It writes only `F{n}md`/`F{n}Md` (0 or 1), `F{n}Tg` and `Ftst` (0 or 1).
/// It keeps no record of what it holds: who holds the fans is the caller's
/// question, answered from the keys themselves.
///
/// Not locked: every call takes `&mut self`, and a root helper calls it from
/// one serial queue.
pub struct FanWriter<K> {
    smc: K,
    sleep: Sleeper,
    /// `F0md` exists (`Some(true)`), does not (`Some(false)`), or has not been
    /// asked yet.
    mode_key_is_lower: Option<bool>,
}

impl<K: KeyAccess> FanWriter<K> {
    /// A writer that waits with [`std::thread::sleep`].
    pub fn new(smc: K) -> Self {
        Self::with_sleeper(smc, Box::new(std::thread::sleep))
    }

    pub fn with_sleeper(smc: K, sleep: Sleeper) -> Self {
        Self {
            smc,
            sleep,
            mode_key_is_lower: None,
        }
    }

    /// The key source, for a test that reads what was written.
    pub const fn source(&self) -> &K {
        &self.smc
    }

    pub const fn source_mut(&mut self) -> &mut K {
        &mut self.smc
    }

    /// `FNum`, at most [`MAX_FANS`]. 0 on a Mac with no `FNum` (no fans).
    pub fn fan_count(&mut self) -> Result<u8, FanError> {
        match self.smc.read(FAN_COUNT_KEY).and_then(|v| v.uint8_value()) {
            Ok(count) => Ok(count.min(MAX_FANS)),
            Err(SmcError::KeyNotFound(_)) => Ok(0),
            Err(error) => Err(error.into()),
        }
    }

    fn check_fan(&mut self, fan: FanIndex) -> Result<(), FanError> {
        let count = self.fan_count()?;
        if fan.get() < count {
            Ok(())
        } else {
            Err(FanError::NoSuchFan { fan, count })
        }
    }

    /// The fan's range, read now and never kept: the call is rare, and a kept
    /// range is one more thing that could be stale.
    pub fn limits(&mut self, fan: FanIndex) -> Result<FanLimits, FanError> {
        self.check_fan(fan)?;
        let min_rpm = self.smc.read(fan_key(fan, *b"Mn"))?.numeric_value()?;
        let max_rpm = self.smc.read(fan_key(fan, *b"Mx"))?.numeric_value()?;
        if !min_rpm.is_finite() || !max_rpm.is_finite() || min_rpm > max_rpm {
            return Err(FanError::LimitsUnreadable { fan });
        }
        Ok(FanLimits { min_rpm, max_rpm })
    }

    /// `F{n}Tg`, the speed the fan is told to hold.
    pub fn target_rpm(&mut self, fan: FanIndex) -> Result<f64, FanError> {
        self.check_fan(fan)?;
        Ok(self.smc.read(fan_key(fan, *b"Tg"))?.numeric_value()?)
    }

    /// The fan's mode key, three ways. A fan index past `FNum`, or an `FNum`
    /// that does not read, is [`FanMode::Unreadable`] too.
    pub fn mode(&mut self, fan: FanIndex) -> FanMode {
        if self.check_fan(fan).is_err() {
            return FanMode::Unreadable;
        }
        let Ok(key) = self.mode_key(fan) else {
            return FanMode::Unreadable;
        };
        self.smc
            .read(key)
            .and_then(|v| v.uint8_value())
            .map_or(FanMode::Unreadable, FanMode::from_ui8)
    }

    /// `Ftst`: `None` on a Mac that has no such key, otherwise whether it
    /// reads other than 0.
    pub fn ftst(&mut self) -> Result<Option<bool>, SmcError> {
        match self.smc.read(UNLOCK_KEY).and_then(|v| v.uint8_value()) {
            Ok(value) => Ok(Some(value != 0)),
            Err(SmcError::KeyNotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Mode key to 1. If the SMC refuses, `Ftst` to 1 (unless it already
    /// is), wait [`UNLOCK_SETTLE`], then try the mode write up to
    /// [`MODE_RETRIES`] times [`MODE_RETRY_GAP`] apart. Stats' order.
    ///
    /// Any failure after the fan is found to exist hands every fan back
    /// before returning (requirement H3): a fan entered by an earlier call is
    /// not left manual with no one to finish the job.
    pub fn set_manual(&mut self, fan: FanIndex) -> Result<Entered, SetManualFailed> {
        if let Err(cause) = self.check_fan(fan) {
            return Err(SetManualFailed {
                cause,
                hand_back: None,
            });
        }
        self.enter(fan).map_err(|cause| SetManualFailed {
            cause,
            hand_back: Some(self.hand_back()),
        })
    }

    fn enter(&mut self, fan: FanIndex) -> Result<Entered, FanError> {
        let key = self.mode_key(fan)?;
        if self.try_write_mode(key, 1)? {
            return Ok(Entered { unlocked: false });
        }
        let unlocked = match self.ftst()? {
            None => return Err(FanError::ModeLocked { fan }),
            Some(true) => false,
            Some(false) => {
                self.smc.write(UNLOCK_KEY, &SmcValue::ui8(1))?;
                (self.sleep)(UNLOCK_SETTLE);
                true
            }
        };
        for attempt in 0..MODE_RETRIES {
            if self.try_write_mode(key, 1)? {
                return Ok(Entered { unlocked });
            }
            if attempt + 1 < MODE_RETRIES {
                (self.sleep)(MODE_RETRY_GAP);
            }
        }
        Err(FanError::ModeLocked { fan })
    }

    /// `true` when the mode write went through, `false` when the SMC refused
    /// it ([`is_refusal`]: what a held fan answers, and what the unlock is
    /// for). Anything else, a wrong type or size or a missing key or no
    /// privilege, is returned as itself and never starts the unlock.
    fn try_write_mode(&mut self, key: FourCc, mode: u8) -> Result<bool, FanError> {
        match self.smc.write(key, &SmcValue::ui8(mode)) {
            Ok(()) => Ok(true),
            Err(error) if is_refusal(error) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// The fan's target, clamped to its own range read now, written in the
    /// type the target key already has (`flt ` on Apple Silicon, `fpe2` on
    /// Intel). Returns the rpm written, as the SMC will read it back (`fpe2`
    /// keeps quarters).
    ///
    /// Does not put the fan in manual and hands nothing back on failure.
    pub fn set_target(&mut self, fan: FanIndex, rpm: f64) -> Result<f32, FanError> {
        if !rpm.is_finite() {
            return Err(FanError::NotANumber);
        }
        let clamped = self.limits(fan)?.clamp(rpm)?;
        let key = fan_key(fan, *b"Tg");
        let current = self.smc.read(key)?;
        let value = match current.ty {
            FourCc::FLT => SmcValue::flt(clamped as f32)?,
            FourCc::FPE2 => SmcValue::fpe2(clamped)?,
            other => return Err(SmcError::UnsupportedType(other).into()),
        };
        let written = value.numeric_value()? as f32;
        self.smc.write(key, &value)?;
        Ok(written)
    }

    /// Every fan back to macOS: each mode key that reads other than 0 to 0,
    /// THEN `Ftst` to 0 where present and raised (requirement M3; Stats does
    /// modes first). Each write is tried [`HAND_BACK_TRIES`] times; a failure
    /// moves on to the next key rather than stopping. Then every key is read
    /// again, up to [`HAND_BACK_SETTLE_POLLS`] times [`HAND_BACK_SETTLE_GAP`]
    /// apart until all read 0, and anything still not 0 is
    /// [`FanError::HandBackIncomplete`].
    ///
    /// A mode key that did not read is written 0 anyway: the write checks the
    /// key's own type, and the re-read is the judge.
    pub fn hand_back(&mut self) -> Result<(), FanError> {
        let count = self.fan_count()?;
        let fans: Vec<FanIndex> = (0..count).filter_map(FanIndex::new).collect();
        let mode_keys: Vec<(FanIndex, FourCc)> =
            match mode_key_suffix(&mut self.smc, &mut self.mode_key_is_lower) {
                Ok(suffix) => fans
                    .iter()
                    .map(|&fan| (fan, fan_key(fan, suffix)))
                    .collect(),
                // The probe failed: try both spellings, the missing one answers
                // "no such key" and is skipped.
                Err(_) => fans
                    .iter()
                    .flat_map(|&fan| [(fan, fan_key(fan, *b"md")), (fan, fan_key(fan, *b"Md"))])
                    .collect(),
            };
        for &(_, key) in &mode_keys {
            if self.raised(key) {
                self.write_retried(key, 0);
            }
        }
        if self.raised(UNLOCK_KEY) {
            self.write_retried(UNLOCK_KEY, 0);
        }

        // The SMC applies the writes a moment later: poll until every key
        // reads 0, and judge only what is still raised when the time is up.
        let mut poll = 1;
        loop {
            let incomplete = self.still_raised(&mode_keys);
            if incomplete.fans == 0 && !incomplete.ftst {
                return Ok(());
            }
            if poll >= HAND_BACK_SETTLE_POLLS {
                return Err(FanError::HandBackIncomplete(incomplete));
            }
            (self.sleep)(HAND_BACK_SETTLE_GAP);
            poll += 1;
        }
    }

    /// One re-read of every mode key and `Ftst`: what does not read 0.
    fn still_raised(&mut self, mode_keys: &[(FanIndex, FourCc)]) -> HandBackIncomplete {
        let mut incomplete = HandBackIncomplete {
            fans: 0,
            ftst: false,
        };
        for &(fan, key) in mode_keys {
            if self.raised(key) {
                incomplete.fans |= 1 << fan.get();
            }
        }
        incomplete.ftst = self.raised(UNLOCK_KEY);
        incomplete
    }

    /// `key` reads other than 0, or does not read. An absent key is not
    /// raised: there is nothing to hand back.
    fn raised(&mut self, key: FourCc) -> bool {
        !matches!(
            self.smc.read(key).and_then(|v| v.uint8_value()),
            Ok(0) | Err(SmcError::KeyNotFound(_))
        )
    }

    /// One `ui8` write, tried up to [`HAND_BACK_TRIES`] times while the SMC
    /// refuses it. Any other error ends the tries at once: a wrong type does
    /// not get righter. The caller's re-read reports what did not land.
    fn write_retried(&mut self, key: FourCc, value: u8) {
        for attempt in 0..HAND_BACK_TRIES {
            match self.smc.write(key, &SmcValue::ui8(value)) {
                Ok(()) => return,
                Err(error) if is_refusal(error) && attempt + 1 < HAND_BACK_TRIES => {
                    (self.sleep)(HAND_BACK_GAP);
                }
                Err(_) => return,
            }
        }
    }

    /// `F{n}md` where `F0md` exists, else `F{n}Md`, probed once and kept, as
    /// [`HeatReader`](crate::HeatReader) does. A probe that fails is not kept.
    fn mode_key(&mut self, fan: FanIndex) -> Result<FourCc, SmcError> {
        Ok(fan_key(
            fan,
            mode_key_suffix(&mut self.smc, &mut self.mode_key_is_lower)?,
        ))
    }
}

/// The answers a held fan gives a mode write: the SMC's own refusal, or an
/// IOKit error. Not [`SmcError::NotPrivileged`]: no unlock makes a non-root
/// caller root.
const fn is_refusal(error: SmcError) -> bool {
    matches!(error, SmcError::SmcRefused { .. } | SmcError::Kernel(_))
}
