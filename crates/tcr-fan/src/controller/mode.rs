//! The modes the fans can be put in, and the speed each asks a fan for: the
//! edge checks a mode passes before it reaches the controller, the hot guard,
//! the Follow CPU curve and the write gate. Pure functions of a reading.

use std::fmt;

use tcr_smc::FanReading;

use crate::holder;

/// In Follow CPU a new target is written only when it moved at least this
/// share of the fan's range (or reached an end, or the hot guard flipped). A
/// starting value, not a measured one.
pub const WRITE_STEP: f64 = 0.05;
/// The highest rpm a Fixed mode may ask for. Root clamps to each fan's own
/// range; this only refuses nonsense at the edge.
pub const MAX_RPM: u32 = 20_000;
/// The lowest temperature a Follow CPU curve may start at, °C.
pub const MIN_QUIET_C: u8 = 20;
/// The narrowest a Follow CPU curve may be, °C.
pub const MIN_CURVE_SPAN_C: u8 = 5;

/// The hot guard: in Follow CPU, full speed from [`Self::ON`] until the CPU
/// cools below [`Self::OFF`]. Never in Fixed, which holds exactly the rpm
/// asked for (Gil, 2026-10-18: on his M4 Pro under load the CPU sat at
/// 104-106 °C and the guard turned `fixed 5000` into 5777). Unmeasured
/// starting values; the rule for setting them from hardware is check H12.
pub struct HotGuard;

impl HotGuard {
    pub const ON: u8 = 100;
    pub const OFF: u8 = 90;
}

/// A Fixed speed, in whole rpm: `1..=MAX_RPM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rpm(u32);

impl Rpm {
    pub const fn new(rpm: u32) -> Result<Self, ModeError> {
        if rpm == 0 || rpm > MAX_RPM {
            Err(ModeError::RpmOutOfRange { rpm })
        } else {
            Ok(Self(rpm))
        }
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Follow CPU's two temperatures, in whole °C: quiet (the fan's minimum) at
/// or below `quiet_below`, full speed at `full_at`, a straight line between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowCurve {
    quiet_below: u8,
    full_at: u8,
}

impl FollowCurve {
    /// `MIN_QUIET_C <= quiet`, `quiet + MIN_CURVE_SPAN_C <= full <=
    /// HotGuard::ON` (requirement L3: the curve never asks for less than full
    /// speed where the hot guard would).
    pub const fn new(quiet_below: u8, full_at: u8) -> Result<Self, ModeError> {
        if quiet_below < MIN_QUIET_C {
            return Err(ModeError::QuietTooLow { quiet_below });
        }
        if full_at > HotGuard::ON {
            return Err(ModeError::FullAboveGuard { full_at });
        }
        if (full_at as u16) < quiet_below as u16 + MIN_CURVE_SPAN_C as u16 {
            return Err(ModeError::CurveTooNarrow {
                quiet_below,
                full_at,
            });
        }
        Ok(Self {
            quiet_below,
            full_at,
        })
    }

    pub const fn quiet_below_c(self) -> u8 {
        self.quiet_below
    }

    pub const fn full_at_c(self) -> u8 {
        self.full_at
    }

    /// Where `celsius` sits on the curve, 0 (quiet) to 1 (full).
    fn share(self, celsius: f64) -> f64 {
        let quiet = f64::from(self.quiet_below);
        let full = f64::from(self.full_at);
        ((celsius - quiet) / (full - quiet)).clamp(0.0, 1.0)
    }
}

/// Why a mode was refused at the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeError {
    RpmOutOfRange { rpm: u32 },
    QuietTooLow { quiet_below: u8 },
    CurveTooNarrow { quiet_below: u8, full_at: u8 },
    FullAboveGuard { full_at: u8 },
}

impl fmt::Display for ModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RpmOutOfRange { rpm } => {
                write!(f, "rpm {rpm} is not between 1 and {MAX_RPM}.")
            }
            Self::QuietTooLow { quiet_below } => write!(
                f,
                "the quiet temperature {quiet_below} °C is below {MIN_QUIET_C} °C."
            ),
            Self::CurveTooNarrow {
                quiet_below,
                full_at,
            } => write!(
                f,
                "full speed at {full_at} °C must be at least {MIN_CURVE_SPAN_C} °C above quiet at {quiet_below} °C."
            ),
            Self::FullAboveGuard { full_at } => write!(
                f,
                "full speed at {full_at} °C is above the hot guard's {} °C.",
                HotGuard::ON
            ),
        }
    }
}

impl std::error::Error for ModeError {}

/// What the fans should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanDesired {
    /// macOS has them.
    Auto,
    /// Exactly this rpm, clamped only by root to each fan's range: no hot
    /// guard, as Macs Fan Control does (Gil, 2026-10-18).
    Fixed {
        rpm: Rpm,
    },
    FollowCpu {
        curve: FollowCurve,
    },
}

impl FanDesired {
    pub const fn is_manual(self) -> bool {
        !matches!(self, Self::Auto)
    }

    /// The [`HotGuard`] runs in this mode: Follow CPU only.
    pub const fn has_hot_guard(self) -> bool {
        matches!(self, Self::FollowCpu { .. })
    }
}

/// The hot guard after this reading: on at [`HotGuard::ON`], off below
/// [`HotGuard::OFF`], unchanged without a reading.
pub(super) fn guard(on: bool, celsius: Option<f64>) -> bool {
    match celsius {
        Some(c) if c >= f64::from(HotGuard::ON) => true,
        Some(c) if c < f64::from(HotGuard::OFF) => false,
        _ => on,
    }
}

/// The fan's range, when both ends read and make sense.
fn limits(fan: &FanReading) -> Option<(f64, f64)> {
    (fan.max_rpm > 0.0 && fan.min_rpm <= fan.max_rpm).then_some((fan.min_rpm, fan.max_rpm))
}

/// What this fan should turn at. `None` when the mode needs the fan's range
/// and it did not read.
pub(super) fn target_for(
    mode: FanDesired,
    fan: &FanReading,
    celsius: Option<f64>,
    hot: bool,
) -> Option<f64> {
    let range = limits(fan);
    if hot && mode.has_hot_guard() {
        return range.map(|(_, max)| max);
    }
    match mode {
        FanDesired::Auto => None,
        FanDesired::Fixed { rpm } => {
            let rpm = f64::from(rpm.get());
            Some(range.map_or(rpm, |(min, max)| rpm.clamp(min, max)))
        }
        FanDesired::FollowCpu { curve } => {
            let (min, max) = range?;
            Some(celsius.map_or(max, |c| curve.share(c).mul_add(max - min, min)))
        }
    }
}

/// The write gate. `forced`: a new command or a hot-guard flip.
pub(super) fn should_write(
    previous: Option<f64>,
    target: f64,
    fan: &FanReading,
    mode: FanDesired,
    forced: bool,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if (target - previous).abs() <= holder::TARGET_TOLERANCE_RPM {
        return false;
    }
    if forced || matches!(mode, FanDesired::Fixed { .. }) {
        return true;
    }
    let Some((min, max)) = limits(fan) else {
        return true;
    };
    #[expect(
        clippy::float_cmp,
        reason = "the target was clamped to exactly this end"
    )]
    let at_end = target == min || target == max;
    at_end || (target - previous).abs() >= WRITE_STEP * (max - min)
}
