//! How a fan is named in the SMC, and what its mode key says: the keys and
//! types [`crate::HeatReader`] and [`crate::FanWriter`] both spell fans with.

use crate::codec::{FourCc, KeySource, SmcError};

/// Ten fans at most: the `n` in `F{n}Md` is one digit.
pub const MAX_FANS: u8 = 10;

pub(crate) const FAN_COUNT_KEY: FourCc = FourCc::new(*b"FNum");
pub(crate) const LOWER_MODE_PROBE_KEY: FourCc = FourCc::new(*b"F0md");
pub(crate) const UNLOCK_KEY: FourCc = FourCc::new(*b"Ftst");

/// A fan's index, 0 to 9: the `n` in `F{n}Ac`, which is one ASCII digit, so
/// ten fans is the most a key can name. Parsed once, by [`Self::new`], so a key
/// built from a fan index can never name `F:Ac` or worse, whatever number came
/// in (a root helper builds keys from an index that crossed a wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FanIndex(u8);

impl FanIndex {
    /// `None` past 9.
    pub const fn new(index: u8) -> Option<Self> {
        if index < 10 { Some(Self(index)) } else { None }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// `F{n}{suffix}`, for a fan index already known to be one digit.
pub const fn fan_key(fan: FanIndex, suffix: [u8; 2]) -> FourCc {
    FourCc::new([b'F', b'0' + fan.0, suffix[0], suffix[1]])
}

/// A fan's mode key, three ways. Never a `bool`: a read that fails is
/// `Unreadable`, which a controller treats as "unknown, write nothing", where
/// a `false` would read as "macOS has it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanMode {
    /// 0, or 3 (Stats' second automatic value).
    Auto,
    /// 1: a program fixed this fan's speed.
    Manual,
    /// The key did not read, is not a `ui8`, holds another value, or this Mac
    /// has no mode key at all.
    Unreadable,
}

impl FanMode {
    /// A mode key's `ui8`: 0 or 3 automatic, 1 manual, anything else unknown.
    pub(crate) const fn from_ui8(value: u8) -> Self {
        match value {
            0 | 3 => Self::Auto,
            1 => Self::Manual,
            _ => Self::Unreadable,
        }
    }
}

/// `md` where `F0md` exists, else `Md`: some Apple Silicon models spell the
/// mode key in lower case. Stats probes `F0md` the same way. Probed once and
/// kept in `kept`; a probe that fails is not kept, and is asked again next
/// time. [`HeatReader`](crate::HeatReader) and [`FanWriter`](crate::FanWriter)
/// both spell the key through this.
pub(crate) fn mode_key_suffix<S: KeySource>(
    smc: &mut S,
    kept: &mut Option<bool>,
) -> Result<[u8; 2], SmcError> {
    if kept.is_none() {
        *kept = Some(match smc.read(LOWER_MODE_PROBE_KEY) {
            Ok(_) => true,
            Err(SmcError::KeyNotFound(_)) => false,
            Err(error) => return Err(error),
        });
    }
    Ok(if *kept == Some(true) { *b"md" } else { *b"Md" })
}
