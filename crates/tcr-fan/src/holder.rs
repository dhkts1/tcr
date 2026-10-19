//! Whose the fans are, judged from the fans' own keys and a record of what was
//! written to them.
//!
//! One rule for both ends: the root helper asks it when the connection that
//! holds the fans goes away ([`still_ours`]: hand back, or leave them), and the
//! proxy's controller asks it on every tick ([`classify`]). So the two can never
//! disagree about whether a fan is still ours.
//!
//! "Ours" is never a remembered flag. A manual fan is ours only while its
//! `F{n}Tg` still equals the target that was written to it (within
//! [`TARGET_TOLERANCE_RPM`]). A fan another program set manual, or whose target
//! another program moved, is not ours, and nothing writes to it. A fan whose
//! mode did not read is unknown, and nothing writes to it either.

use tcr_smc::{FanIndex, FanMode};

/// How far `F{n}Tg` may read from the target written and still be the same
/// target: the `flt ` write reads back exactly and an `fpe2` one keeps
/// quarters, so 1 rpm separates "ours" from a target any other program chose.
/// Whether Apple Silicon reads `F0Tg` back exactly is hardware check H4.
pub const TARGET_TOLERANCE_RPM: f64 = 1.0;

/// One fan, as read now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FanState {
    pub fan: FanIndex,
    pub mode: FanMode,
    /// `F{n}Tg`; `None` when it did not read.
    pub target_rpm: Option<f64>,
}

/// One fan the holder put in manual, and the target last written to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeldFan {
    pub fan: FanIndex,
    /// `None` between set-manual and the first set-target: no target of ours
    /// is on it yet. Whether that still makes the fan ours is [`Unwritten`].
    pub written_rpm: Option<f64>,
}

/// What a manual fan whose record carries no target (`written_rpm: None`)
/// counts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unwritten {
    /// The holder is between its set-manual and its first set-target: root,
    /// which put the fan in manual itself and judges its own record.
    Ours,
    /// The proxy's controller, outside its entry: an entry that ended hands
    /// back, so a record with no target is a leftover, and a manual fan
    /// carrying none of our targets is another program's.
    NotOurs,
}

/// [`still_ours`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StillOurs {
    /// Every manual fan is one we hold, carrying the target we wrote.
    Ours,
    /// This manual fan is not one we hold, or its target moved: another
    /// program has the fans. Write nothing.
    Changed { fan: FanIndex },
    /// This fan's mode or target did not read, or a fan we hold is missing
    /// from the reading. Write nothing now.
    Unknown { fan: FanIndex },
}

/// Who has the fans, for the controller's tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// Every fan reads automatic.
    MacOs,
    /// Every fan held reads manual with our target ([`StillOurs::Ours`]).
    Ours,
    /// Every manual fan is ours, and a fan we hold reads automatic again:
    /// something put that one back, so it is not held.
    PartlyAutomatic,
    /// Macs Fan Control is running: whatever the keys say, write nothing.
    MacsFanControl,
    /// A manual fan that is not ours.
    AnotherApp,
    /// A key did not read.
    Unknown,
}

/// Are the manual fans in `reading` still the ones `held` describes?
///
/// A fan reading automatic is fine either way: macOS has it, or someone
/// already handed it back. A held fan missing from the reading is unknown,
/// never assumed gone. A manual fan whose record has no target yet is
/// `unwritten`'s call.
pub fn still_ours(reading: &[FanState], held: &[HeldFan], unwritten: Unwritten) -> StillOurs {
    for state in reading {
        match state.mode {
            FanMode::Unreadable => return StillOurs::Unknown { fan: state.fan },
            FanMode::Auto => {}
            FanMode::Manual => {
                let Some(ours) = held.iter().find(|h| h.fan == state.fan) else {
                    return StillOurs::Changed { fan: state.fan };
                };
                match (ours.written_rpm, unwritten) {
                    (Some(written), _) => {
                        let Some(target) = state.target_rpm else {
                            return StillOurs::Unknown { fan: state.fan };
                        };
                        if (target - written).abs() > TARGET_TOLERANCE_RPM {
                            return StillOurs::Changed { fan: state.fan };
                        }
                    }
                    (None, Unwritten::Ours) => {}
                    (None, Unwritten::NotOurs) => return StillOurs::Changed { fan: state.fan },
                }
            }
        }
    }
    if let Some(missing) = missing(reading, held) {
        return StillOurs::Unknown { fan: missing };
    }
    StillOurs::Ours
}

/// The first held fan the reading does not carry.
fn missing(reading: &[FanState], held: &[HeldFan]) -> Option<FanIndex> {
    held.iter()
        .find(|h| !reading.iter().any(|s| s.fan == h.fan))
        .map(|h| h.fan)
}

/// Who has the fans, for the proxy's controller. `record` is what it believes
/// it holds (`None`: nothing). Macs Fan Control running wins over anything
/// the keys say. A reading that lost a fan the record names (an empty one when
/// `FNum` did not read) is unknown, never "macOS has them". A held fan with
/// no target is `unwritten`'s call: ours only while the controller is
/// entering.
pub fn classify(
    reading: &[FanState],
    record: Option<&[HeldFan]>,
    unwritten: Unwritten,
    macs_running: bool,
) -> Holder {
    if macs_running {
        return Holder::MacsFanControl;
    }
    if reading.iter().any(|s| s.mode == FanMode::Unreadable) {
        return Holder::Unknown;
    }
    let held = record.unwrap_or(&[]);
    if missing(reading, held).is_some() {
        return Holder::Unknown;
    }
    if reading.iter().all(|s| s.mode == FanMode::Auto) {
        return Holder::MacOs;
    }
    match still_ours(reading, held, unwritten) {
        StillOurs::Ours => {
            let dropped = held.iter().any(|h| {
                reading
                    .iter()
                    .any(|s| s.fan == h.fan && s.mode == FanMode::Auto)
            });
            if dropped {
                Holder::PartlyAutomatic
            } else {
                Holder::Ours
            }
        }
        StillOurs::Changed { .. } => Holder::AnotherApp,
        StillOurs::Unknown { .. } => Holder::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fan(index: u8) -> FanIndex {
        FanIndex::new(index).unwrap()
    }

    fn state(index: u8, mode: FanMode, target: Option<f64>) -> FanState {
        FanState {
            fan: fan(index),
            mode,
            target_rpm: target,
        }
    }

    fn held(index: u8, rpm: Option<f64>) -> HeldFan {
        HeldFan {
            fan: fan(index),
            written_rpm: rpm,
        }
    }

    /// Root's judgement: a fan with no target yet is still its own doing.
    fn by_root(reading: &[FanState], record: &[HeldFan]) -> StillOurs {
        still_ours(reading, record, Unwritten::Ours)
    }

    #[test]
    fn our_target_within_a_rpm_is_ours() {
        let reading = [
            state(0, FanMode::Manual, Some(3000.0)),
            state(1, FanMode::Manual, Some(3000.75)),
        ];
        let record = [held(0, Some(3000.0)), held(1, Some(3000.0))];
        assert_eq!(by_root(&reading, &record), StillOurs::Ours);
        assert_eq!(
            classify(&reading, Some(&record), Unwritten::NotOurs, false),
            Holder::Ours
        );
    }

    #[test]
    fn foreign_target_is_not_ours() {
        let reading = [state(0, FanMode::Manual, Some(4200.0))];
        let record = [held(0, Some(3000.0))];
        assert_eq!(
            by_root(&reading, &record),
            StillOurs::Changed { fan: fan(0) }
        );
        assert_eq!(
            classify(&reading, Some(&record), Unwritten::NotOurs, false),
            Holder::AnotherApp
        );
    }

    #[test]
    fn a_manual_fan_we_never_held_is_another_apps() {
        let reading = [
            state(0, FanMode::Manual, Some(3000.0)),
            state(1, FanMode::Manual, Some(3000.0)),
        ];
        let record = [held(0, Some(3000.0))];
        assert_eq!(
            by_root(&reading, &record),
            StillOurs::Changed { fan: fan(1) }
        );
        assert_eq!(
            classify(&reading, None, Unwritten::NotOurs, false),
            Holder::AnotherApp
        );
    }

    /// Root, between its set-manual and the first set-target, holds the fan;
    /// the controller outside its entry does not.
    #[test]
    fn a_record_without_a_target_is_ours_only_while_entering() {
        let reading = [state(0, FanMode::Manual, Some(1800.0))];
        let record = [held(0, None)];
        assert_eq!(by_root(&reading, &record), StillOurs::Ours);
        assert_eq!(
            still_ours(&reading, &record, Unwritten::NotOurs),
            StillOurs::Changed { fan: fan(0) }
        );
        assert_eq!(
            classify(&reading, Some(&record), Unwritten::NotOurs, false),
            Holder::AnotherApp
        );
    }

    #[test]
    fn unreadable_is_unknown_never_ours() {
        let record = [held(0, Some(3000.0))];
        assert_eq!(
            by_root(&[state(0, FanMode::Unreadable, None)], &record),
            StillOurs::Unknown { fan: fan(0) }
        );
        assert_eq!(
            by_root(&[state(0, FanMode::Manual, None)], &record),
            StillOurs::Unknown { fan: fan(0) }
        );
        assert_eq!(
            classify(
                &[state(0, FanMode::Unreadable, None)],
                Some(&record),
                Unwritten::NotOurs,
                false
            ),
            Holder::Unknown
        );
        // A held fan the reading lost.
        assert_eq!(by_root(&[], &record), StillOurs::Unknown { fan: fan(0) });
    }

    /// `FNum` that did not read gives an empty reading, and a fan that did
    /// not read is left out: with a record, neither is "macOS has the fans".
    #[test]
    fn a_reading_missing_a_held_fan_is_unknown_never_macos() {
        let record = [held(0, Some(3000.0)), held(1, Some(3000.0))];
        assert_eq!(
            classify(&[], Some(&record), Unwritten::NotOurs, false),
            Holder::Unknown
        );
        assert_eq!(
            classify(
                &[state(0, FanMode::Auto, Some(0.0))],
                Some(&record),
                Unwritten::NotOurs,
                false
            ),
            Holder::Unknown
        );
        // Without a record an empty reading is still nobody's.
        assert_eq!(
            classify(&[], None, Unwritten::NotOurs, false),
            Holder::MacOs
        );
    }

    /// One held fan back on automatic while the other keeps our target: not
    /// ours to tick as if nothing happened.
    #[test]
    fn a_held_fan_back_on_automatic_is_partly_automatic() {
        let reading = [
            state(0, FanMode::Manual, Some(3000.0)),
            state(1, FanMode::Auto, Some(0.0)),
        ];
        let record = [held(0, Some(3000.0)), held(1, Some(3000.0))];
        assert_eq!(
            classify(&reading, Some(&record), Unwritten::NotOurs, false),
            Holder::PartlyAutomatic
        );
        // Root's disconnect rule still hands that back.
        assert_eq!(by_root(&reading, &record), StillOurs::Ours);
    }

    #[test]
    fn automatic_fans_are_macos_and_still_ours_to_finish() {
        let reading = [
            state(0, FanMode::Auto, Some(0.0)),
            state(1, FanMode::Auto, Some(0.0)),
        ];
        assert_eq!(
            classify(&reading, None, Unwritten::NotOurs, false),
            Holder::MacOs
        );
        assert_eq!(by_root(&reading, &[held(0, Some(3000.0))]), StillOurs::Ours);
    }

    #[test]
    fn macs_fan_control_wins_over_the_keys() {
        let reading = [state(0, FanMode::Manual, Some(3000.0))];
        let record = [held(0, Some(3000.0))];
        assert_eq!(
            classify(&reading, Some(&record), Unwritten::NotOurs, true),
            Holder::MacsFanControl
        );
        assert_eq!(
            classify(&[], None, Unwritten::NotOurs, true),
            Holder::MacsFanControl
        );
    }
}
