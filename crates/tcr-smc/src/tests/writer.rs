use super::*;

// The fan writer, against the fake SMC. Every figure is the Mac16,7
// reading: two fans, 1,350 to 5,777 rpm, `flt ` targets.

/// Two fans, mode keys `F{n}Md` at 0, `Ftst` at `ftst` (absent if `None`).
fn apple_silicon(ftst: Option<u8>) -> FakeSmc {
    let mut smc = FakeSmc::default();
    smc.set("FNum", Ok(SmcValue::ui8(2)));
    for fan in ["F0", "F1"] {
        smc.set(&format!("{fan}Mn"), flt(1350.0));
        smc.set(&format!("{fan}Mx"), flt(5777.0));
        smc.set(&format!("{fan}Tg"), flt(0.0));
        smc.set(&format!("{fan}Ac"), flt(2000.0));
        smc.set(&format!("{fan}Md"), Ok(SmcValue::ui8(0)));
    }
    if let Some(ftst) = ftst {
        smc.set("Ftst", Ok(SmcValue::ui8(ftst)));
    }
    smc
}

/// A writer on `smc` whose waits are recorded, not slept.
fn writer(smc: FakeSmc) -> (FanWriter<FakeSmc>, Arc<Mutex<Vec<Duration>>>) {
    let waits = Arc::new(Mutex::new(Vec::new()));
    let w = Arc::clone(&waits);
    let writer = FanWriter::with_sleeper(smc, Box::new(move |d| w.lock().unwrap().push(d)));
    (writer, waits)
}

fn written(writer: &FanWriter<FakeSmc>) -> Vec<String> {
    writer
        .source()
        .writes
        .iter()
        .map(|(k, _)| k.to_string())
        .collect()
}

fn ui8_at(writer: &mut FanWriter<FakeSmc>, key: &str) -> u8 {
    writer
        .source_mut()
        .read(code(key))
        .unwrap()
        .uint8_value()
        .unwrap()
}

fn float_at(writer: &mut FanWriter<FakeSmc>, key: &str) -> f32 {
    writer
        .source_mut()
        .read(code(key))
        .unwrap()
        .float_value()
        .unwrap()
}

#[test]
fn set_manual_refuses_fan_past_fnum() {
    let (mut fans, waits) = writer(apple_silicon(Some(0)));
    for fan in [2, 9] {
        let failed = fans.set_manual(fan_index(fan)).unwrap_err();
        assert_eq!(
            failed,
            SetManualFailed {
                cause: FanError::NoSuchFan {
                    fan: fan_index(fan),
                    count: 2
                },
                hand_back: None,
            }
        );
        assert_eq!(
            fans.set_target(fan_index(fan), 3000.0),
            Err(FanError::NoSuchFan {
                fan: fan_index(fan),
                count: 2
            })
        );
        assert!(fans.limits(fan_index(fan)).is_err());
        assert_eq!(fans.mode(fan_index(fan)), FanMode::Unreadable);
    }
    assert!(fans.source().attempts.is_empty(), "nothing written");
    assert!(waits.lock().unwrap().is_empty());
}

/// `FNum` past ten is ten: a fan key's index is one digit.
#[test]
fn fnum_is_capped_at_ten() {
    let mut smc = apple_silicon(Some(0));
    smc.set("FNum", Ok(SmcValue::ui8(15)));
    smc.set("F9Mn", flt(1000.0));
    smc.set("F9Mx", flt(4000.0));
    smc.set("F9Tg", flt(0.0));
    let (mut fans, _) = writer(smc);
    assert_eq!(fans.fan_count(), Ok(10));
    assert_eq!(fans.set_target(fan_index(9), 3000.0), Ok(3000.0));
    // No `FNum` at all: no fans, not an error.
    let mut none = FakeSmc::default();
    none.set("Ftst", Ok(SmcValue::ui8(0)));
    let (mut fans, _) = writer(none);
    assert_eq!(fans.fan_count(), Ok(0));
    assert_eq!(fans.hand_back(), Ok(()));
}

#[test]
fn target_is_clamped_to_the_fans_own_range() {
    let (mut fans, _) = writer(apple_silicon(Some(0)));
    assert_eq!(fans.set_target(fan_index(0), 9000.0), Ok(5777.0));
    assert_eq!(float_at(&mut fans, "F0Tg"), 5777.0);
    assert_eq!(fans.set_target(fan_index(1), 200.0), Ok(1350.0));
    assert_eq!(float_at(&mut fans, "F1Tg"), 1350.0);
    assert_eq!(fans.set_target(fan_index(0), -1.0), Ok(1350.0));
    assert_eq!(fans.set_target(fan_index(0), 5000.0), Ok(5000.0));
    let last = fans.source().writes.last().unwrap().1.clone();
    assert_eq!(last.bytes, hex("00409c45"), "5000.0 as measured");
}

/// The range is read from the SMC at the time of each call, never kept.
#[test]
fn clamp_reads_limits_each_call() {
    let (mut fans, _) = writer(apple_silicon(Some(0)));
    assert_eq!(fans.set_target(fan_index(0), 9000.0), Ok(5777.0));
    fans.source_mut().set("F0Mx", flt(4000.0));
    assert_eq!(fans.set_target(fan_index(0), 9000.0), Ok(4000.0));
    assert_eq!(float_at(&mut fans, "F0Tg"), 4000.0);
    fans.source_mut().set("F0Mn", flt(2500.0));
    assert_eq!(fans.set_target(fan_index(0), 100.0), Ok(2500.0));
    assert_eq!(
        fans.limits(fan_index(0)),
        Ok(FanLimits {
            min_rpm: 2500.0,
            max_rpm: 4000.0
        })
    );
}

#[test]
fn nan_and_infinity_refused() {
    let (mut fans, _) = writer(apple_silicon(Some(0)));
    for rpm in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            fans.set_target(fan_index(0), rpm),
            Err(FanError::NotANumber)
        );
    }
    assert!(fans.source().attempts.is_empty());
    assert_eq!(float_at(&mut fans, "F0Tg"), 0.0);
}

/// Limits that are no range are an error, never a guess at one.
#[test]
fn unusable_limits_are_refused() {
    for (min, max) in [
        (6000.0, 5777.0),
        (f32::NAN, 5777.0),
        (1350.0, f32::INFINITY),
    ] {
        let mut smc = apple_silicon(Some(0));
        smc.set("F0Mn", flt(min));
        smc.set("F0Mx", flt(max));
        let (mut fans, _) = writer(smc);
        assert_eq!(
            fans.set_target(fan_index(0), 3000.0),
            Err(FanError::LimitsUnreadable { fan: fan_index(0) }),
            "{min}..{max}"
        );
        assert!(fans.source().attempts.is_empty());
    }
}

/// Intel keeps fan speeds in `fpe2`: the target goes back in that type,
/// and the answer is what the SMC holds, quarters and all.
#[test]
fn target_is_written_in_the_keys_own_type() {
    let mut smc = FakeSmc::default();
    smc.set("FNum", Ok(SmcValue::ui8(1)));
    smc.set("F0Mn", SmcValue::fpe2(1200.0));
    smc.set("F0Mx", SmcValue::fpe2(6200.0));
    smc.set("F0Tg", SmcValue::fpe2(0.0));
    let (mut fans, _) = writer(smc);
    assert_eq!(fans.set_target(fan_index(0), 5000.0), Ok(5000.0));
    assert_eq!(
        fans.source().writes.last().unwrap().1,
        SmcValue::fpe2(5000.0).unwrap()
    );
    assert_eq!(fans.set_target(fan_index(0), 3000.1), Ok(3000.0));
    // A target key of any other type is not written.
    fans.source_mut().set("F0Tg", Ok(SmcValue::ui16(0)));
    assert_eq!(
        fans.set_target(fan_index(0), 3000.0),
        Err(FanError::Smc(SmcError::UnsupportedType(FourCc::UI16)))
    );
    assert_eq!(fans.source().writes.len(), 2);
}

/// Where the mode key takes the write directly, nothing else is touched.
#[test]
fn manual_writes_the_mode_key_directly_when_allowed() {
    let (mut fans, waits) = writer(apple_silicon(Some(0)));
    assert_eq!(
        fans.set_manual(fan_index(1)),
        Ok(Entered { unlocked: false })
    );
    assert_eq!(written(&fans), ["F1Md"]);
    assert_eq!(ui8_at(&mut fans, "F1Md"), 1);
    assert_eq!(fans.mode(fan_index(1)), FanMode::Manual);
    assert_eq!(fans.mode(fan_index(0)), FanMode::Auto);
    assert!(waits.lock().unwrap().is_empty());
}

/// Refused: `Ftst` to 1, wait 3 s for thermalmonitord, then the mode again.
#[test]
fn refused_mode_write_raises_ftst_waits_and_retries() {
    let mut smc = apple_silicon(Some(0));
    smc.refusals.insert(code("F0Md"), 1);
    let (mut fans, waits) = writer(smc);
    assert_eq!(
        fans.set_manual(fan_index(0)),
        Ok(Entered { unlocked: true })
    );
    assert_eq!(written(&fans), ["Ftst", "F0Md"]);
    assert_eq!(fans.ftst(), Ok(Some(true)));
    assert_eq!(*waits.lock().unwrap(), [UNLOCK_SETTLE]);
}

/// `Ftst` already raised (another fan's unlock): not written again, and
/// no settle wait.
#[test]
fn raised_ftst_is_not_written_again() {
    let mut smc = apple_silicon(Some(1));
    smc.refusals.insert(code("F0Md"), 2);
    let (mut fans, waits) = writer(smc);
    assert_eq!(
        fans.set_manual(fan_index(0)),
        Ok(Entered { unlocked: false })
    );
    assert_eq!(written(&fans), ["F0Md"]);
    assert_eq!(*waits.lock().unwrap(), [MODE_RETRY_GAP]);
}

/// The bounded retries, counted: one settle, then 49 gaps between 50
/// tries, and every fan handed back before the error.
#[test]
fn mode_that_never_unlocks_is_bounded_and_handed_back() {
    let mut smc = apple_silicon(Some(0));
    smc.refusals.insert(code("F0Md"), usize::MAX);
    let (mut fans, waits) = writer(smc);
    let failed = fans.set_manual(fan_index(0)).unwrap_err();
    assert_eq!(failed.cause, FanError::ModeLocked { fan: fan_index(0) });
    assert_eq!(failed.hand_back, Some(Ok(())));
    let waits = waits.lock().unwrap().clone();
    assert_eq!(waits.len(), 1 + MODE_RETRIES - 1);
    assert_eq!(waits[0], UNLOCK_SETTLE);
    assert!(waits[1..].iter().all(|d| *d == MODE_RETRY_GAP));
    assert_eq!(
        fans.source()
            .attempts
            .iter()
            .filter(|k| **k == code("F0Md"))
            .count(),
        1 + MODE_RETRIES
    );
    assert_eq!(written(&fans), ["Ftst", "Ftst"], "raised, then lowered");
    assert_eq!(fans.ftst(), Ok(Some(false)));
}

/// A Mac with no `Ftst` has no unlock: a refused mode write is locked.
#[test]
fn refused_mode_without_ftst_is_locked() {
    let mut smc = apple_silicon(None);
    smc.refusals.insert(code("F0Md"), 1);
    let (mut fans, waits) = writer(smc);
    let failed = fans.set_manual(fan_index(0)).unwrap_err();
    assert_eq!(failed.cause, FanError::ModeLocked { fan: fan_index(0) });
    assert!(fans.source().writes.is_empty());
    assert!(waits.lock().unwrap().is_empty());
}

/// A wrong type or size is a bug, not a lock: it reaches the caller as
/// itself, and the unlock never starts.
#[test]
fn mode_write_type_mismatch_is_surfaced_not_locked() {
    for wrong in [
        SmcError::WrongSize {
            ty: FourCc::UI8,
            expected: 1,
            actual: 4,
        },
        SmcError::WrongType {
            expected: FourCc::FLT,
            actual: FourCc::UI8,
        },
    ] {
        let mut smc = apple_silicon(Some(0));
        smc.failures.insert(code("F0Md"), wrong);
        let (mut fans, waits) = writer(smc);
        let failed = fans.set_manual(fan_index(0)).unwrap_err();
        assert_eq!(failed.cause, FanError::Smc(wrong));
        assert!(fans.source().writes.is_empty(), "no Ftst write");
        assert!(waits.lock().unwrap().is_empty());
    }
    // The real check, not a scripted failure: a mode key that is not a
    // `ui8` refuses the `ui8` the writer sends.
    let mut smc = apple_silicon(Some(0));
    smc.set("F0Md", flt(0.0));
    let (mut fans, _) = writer(smc);
    let failed = fans.set_manual(fan_index(0)).unwrap_err();
    assert_eq!(
        failed.cause,
        FanError::Smc(SmcError::WrongType {
            expected: FourCc::FLT,
            actual: FourCc::UI8
        })
    );
    assert!(!fans.source().attempts.contains(&code("Ftst")));
}

/// No privilege is no privilege: an unlock would not change it.
#[test]
fn not_privileged_is_not_a_lock() {
    let mut smc = apple_silicon(Some(0));
    smc.failures.insert(code("F0Md"), SmcError::NotPrivileged);
    let (mut fans, _) = writer(smc);
    let failed = fans.set_manual(fan_index(0)).unwrap_err();
    assert_eq!(failed.cause, FanError::Smc(SmcError::NotPrivileged));
    assert!(!fans.source().attempts.contains(&code("Ftst")));
}

/// `F0md` where the Mac spells it that way (Stats' probe).
#[test]
fn lower_case_mode_key_is_used_where_it_exists() {
    let mut smc = apple_silicon(Some(0));
    smc.set("F0md", Ok(SmcValue::ui8(0)));
    smc.set("F1md", Ok(SmcValue::ui8(0)));
    let (mut fans, _) = writer(smc);
    fans.set_manual(fan_index(1)).unwrap();
    assert_eq!(written(&fans), ["F1md"]);
    fans.hand_back().unwrap();
    assert_eq!(written(&fans), ["F1md", "F1md"]);
}

/// Requirement M3: every mode key to 0 first, THEN `Ftst` to 0, then the
/// re-read.
#[test]
fn hand_back_order_is_modes_then_ftst() {
    let mut smc = apple_silicon(Some(1));
    smc.set("F0Md", Ok(SmcValue::ui8(1)));
    smc.set("F1Md", Ok(SmcValue::ui8(1)));
    let (mut fans, waits) = writer(smc);
    assert_eq!(fans.hand_back(), Ok(()));
    assert_eq!(written(&fans), ["F0Md", "F1Md", "Ftst"]);
    assert!(
        fans.source()
            .writes
            .iter()
            .all(|(_, v)| *v == SmcValue::ui8(0))
    );
    assert_eq!(fans.mode(fan_index(0)), FanMode::Auto);
    assert_eq!(fans.mode(fan_index(1)), FanMode::Auto);
    assert_eq!(fans.ftst(), Ok(Some(false)));
    assert!(waits.lock().unwrap().is_empty());
}

/// Nothing raised, nothing written: a hand-back on fans macOS already
/// has touches no key.
#[test]
fn hand_back_of_automatic_fans_writes_nothing() {
    for ftst in [Some(0), None] {
        let (mut fans, _) = writer(apple_silicon(ftst));
        assert_eq!(fans.hand_back(), Ok(()));
        assert!(fans.source().attempts.is_empty(), "{ftst:?}");
    }
}

/// Requirement H3 on root's side: fan 0 entered, fan 1 would not; fan 0
/// and `Ftst` are back at 0 before the error is returned.
#[test]
fn second_fan_failure_hands_back_before_error() {
    let mut smc = apple_silicon(Some(0));
    smc.refusals.insert(code("F1Md"), usize::MAX);
    let (mut fans, _) = writer(smc);
    assert_eq!(
        fans.set_manual(fan_index(0)),
        Ok(Entered { unlocked: false })
    );
    let failed = fans.set_manual(fan_index(1)).unwrap_err();
    assert_eq!(failed.cause, FanError::ModeLocked { fan: fan_index(1) });
    assert_eq!(failed.hand_back, Some(Ok(())));
    assert_eq!(written(&fans), ["F0Md", "Ftst", "F0Md", "Ftst"]);
    assert_eq!(ui8_at(&mut fans, "F0Md"), 0);
    assert_eq!(ui8_at(&mut fans, "F1Md"), 0);
    assert_eq!(ui8_at(&mut fans, "Ftst"), 0);
}

/// The hand-back is the write that must not be lost: a refused write is
/// tried again, 50 ms apart.
#[test]
fn hand_back_retries_a_refused_write() {
    let mut smc = apple_silicon(Some(1));
    smc.refusals.insert(code("Ftst"), 3);
    let (mut fans, waits) = writer(smc);
    assert_eq!(fans.hand_back(), Ok(()));
    assert_eq!(ui8_at(&mut fans, "Ftst"), 0);
    assert_eq!(*waits.lock().unwrap(), [HAND_BACK_GAP; 3]);
}

/// A write that never lands is tried ten times, nine gaps, and the other
/// keys are still handed back; the re-read names what did not.
#[test]
fn hand_back_that_does_not_land_is_reported() {
    let mut smc = apple_silicon(Some(1));
    smc.set("F0Md", Ok(SmcValue::ui8(1)));
    smc.set("F1Md", Ok(SmcValue::ui8(1)));
    smc.refusals.insert(code("F0Md"), usize::MAX);
    let (mut fans, waits) = writer(smc);
    assert_eq!(
        fans.hand_back(),
        Err(FanError::HandBackIncomplete(HandBackIncomplete {
            fans: 0b01,
            ftst: false
        }))
    );
    assert_eq!(
        fans.source()
            .attempts
            .iter()
            .filter(|k| **k == code("F0Md"))
            .count(),
        HAND_BACK_TRIES
    );
    let mut expected = vec![HAND_BACK_GAP; HAND_BACK_TRIES - 1];
    expected.extend([HAND_BACK_SETTLE_GAP; HAND_BACK_SETTLE_POLLS - 1]);
    assert_eq!(*waits.lock().unwrap(), expected);
    assert_eq!(written(&fans), ["F1Md", "Ftst"]);
}

/// An SMC that says yes and does not change is caught by the re-read,
/// after every poll of the settle wait.
#[test]
fn hand_back_rereads_what_it_wrote() {
    let mut smc = apple_silicon(Some(1));
    smc.set("F1Md", Ok(SmcValue::ui8(1)));
    smc.sticky.insert(code("Ftst"));
    smc.sticky.insert(code("F1Md"));
    let (mut fans, waits) = writer(smc);
    assert_eq!(
        fans.hand_back(),
        Err(FanError::HandBackIncomplete(HandBackIncomplete {
            fans: 0b10,
            ftst: true
        }))
    );
    assert_eq!(written(&fans), ["F1Md", "Ftst"]);
    assert_eq!(
        *waits.lock().unwrap(),
        [HAND_BACK_SETTLE_GAP; HAND_BACK_SETTLE_POLLS - 1]
    );
}

/// The hardware's shape (M4 Pro, 2026-10-18): the writes land, but the
/// mode keys and `Ftst` read 1 for a while after. The hand-back waits for
/// them and succeeds, one settle gap per read that still saw 1.
#[test]
fn hand_back_waits_for_the_smc_to_apply_it() {
    for lag in [1, 5, HAND_BACK_SETTLE_POLLS - 1] {
        let mut smc = apple_silicon(Some(1));
        smc.set("F0Md", Ok(SmcValue::ui8(1)));
        smc.set("F1Md", Ok(SmcValue::ui8(1)));
        for key in ["F0Md", "F1Md", "Ftst"] {
            smc.lag.insert(code(key), lag);
        }
        let (mut fans, waits) = writer(smc);
        assert_eq!(fans.hand_back(), Ok(()), "lag {lag}");
        assert_eq!(written(&fans), ["F0Md", "F1Md", "Ftst"]);
        assert_eq!(*waits.lock().unwrap(), vec![HAND_BACK_SETTLE_GAP; lag]);
    }
}

/// Slower than the settle wait allows: reported, naming what still read 1.
#[test]
fn hand_back_slower_than_the_settle_wait_is_reported() {
    let mut smc = apple_silicon(Some(1));
    smc.set("F0Md", Ok(SmcValue::ui8(1)));
    smc.lag.insert(code("F0Md"), HAND_BACK_SETTLE_POLLS);
    let (mut fans, waits) = writer(smc);
    assert_eq!(
        fans.hand_back(),
        Err(FanError::HandBackIncomplete(HandBackIncomplete {
            fans: 0b01,
            ftst: false
        }))
    );
    assert_eq!(
        *waits.lock().unwrap(),
        [HAND_BACK_SETTLE_GAP; HAND_BACK_SETTLE_POLLS - 1]
    );
}

/// If the hand-back after a failed entry fails too, both are returned.
#[test]
fn failed_hand_back_after_failed_entry_is_reported() {
    let mut smc = apple_silicon(Some(0));
    smc.refusals.insert(code("F1Md"), usize::MAX);
    smc.sticky.insert(code("F0Md"));
    let (mut fans, _) = writer(smc);
    fans.set_manual(fan_index(0)).unwrap();
    fans.source_mut().set("F0Md", Ok(SmcValue::ui8(1)));
    let failed = fans.set_manual(fan_index(1)).unwrap_err();
    assert_eq!(failed.cause, FanError::ModeLocked { fan: fan_index(1) });
    assert_eq!(
        failed.hand_back,
        Some(Err(FanError::HandBackIncomplete(HandBackIncomplete {
            fans: 0b01,
            ftst: false
        })))
    );
}

/// The mode is three-way: a read that fails is never "automatic".
#[test]
fn mode_is_three_way() {
    let mut smc = apple_silicon(Some(0));
    smc.set("F0Md", Ok(SmcValue::ui8(3)));
    smc.set(
        "F1Md",
        Err(SmcError::SmcRefused {
            key: code("F1Md"),
            result: 0x85,
        }),
    );
    let (mut fans, _) = writer(smc);
    assert_eq!(fans.mode(fan_index(0)), FanMode::Auto);
    assert_eq!(fans.mode(fan_index(1)), FanMode::Unreadable);
    fans.source_mut().set("F1Md", Ok(SmcValue::ui8(2)));
    assert_eq!(fans.mode(fan_index(1)), FanMode::Unreadable);
    fans.source_mut().set("F1Md", flt(1.0));
    assert_eq!(fans.mode(fan_index(1)), FanMode::Unreadable);
    fans.source_mut().set("F1Md", Ok(SmcValue::ui8(1)));
    assert_eq!(fans.mode(fan_index(1)), FanMode::Manual);
}

#[test]
fn ftst_and_target_reads() {
    let (mut fans, _) = writer(apple_silicon(None));
    assert_eq!(fans.ftst(), Ok(None));
    fans.source_mut().set("Ftst", Ok(SmcValue::ui8(1)));
    assert_eq!(fans.ftst(), Ok(Some(true)));
    let refused = SmcError::Kernel(5);
    fans.source_mut().set("Ftst", Err(refused));
    assert_eq!(fans.ftst(), Err(refused));
    fans.set_target(fan_index(1), 4321.0).unwrap();
    assert_eq!(fans.target_rpm(fan_index(1)), Ok(4321.0));
}

/// Every operation, on an SMC full of other keys, writes only the mode
/// keys (0 or 1), the targets and `Ftst` (0 or 1).
#[test]
fn writer_writes_only_allowed_keys() {
    let mut smc = apple_silicon(Some(0));
    // Keys a careless writer might touch, each answerable.
    for key in ["F0Ac", "F1Ac", "TCMz", "FS! ", "F0Mn", "F0Mx", "#KEY"] {
        smc.values
            .entry(code(key))
            .or_insert_with(|| Ok(SmcValue::ui8(0)));
    }
    smc.refusals.insert(code("F1Md"), 1);
    let (mut fans, _) = writer(smc);
    fans.fan_count().unwrap();
    fans.limits(fan_index(0)).unwrap();
    fans.mode(fan_index(0));
    fans.ftst().unwrap();
    fans.target_rpm(fan_index(0)).unwrap();
    fans.set_manual(fan_index(0)).unwrap();
    fans.set_manual(fan_index(1)).unwrap();
    fans.set_target(fan_index(0), 3000.0).unwrap();
    fans.set_target(fan_index(1), 9000.0).unwrap();
    fans.hand_back().unwrap();
    // And the failure paths.
    fans.source_mut().refusals.insert(code("F0Md"), usize::MAX);
    fans.source_mut().set("Ftst", Ok(SmcValue::ui8(0)));
    fans.set_manual(fan_index(0)).unwrap_err();
    fans.set_target(fan_index(0), f64::NAN).unwrap_err();
    fans.set_manual(fan_index(5)).unwrap_err();

    let allowed = ["F0Md", "F1Md", "F0Tg", "F1Tg", "Ftst"].map(code);
    let attempts = &fans.source().attempts;
    assert!(attempts.len() > 6, "{attempts:?}");
    for key in attempts {
        assert!(allowed.contains(key), "wrote {key}");
    }
    for (key, value) in &fans.source().writes {
        if *key == code("Ftst") || key.to_string().ends_with("Md") {
            assert!(
                *value == SmcValue::ui8(0) || *value == SmcValue::ui8(1),
                "{key} = {value:?}"
            );
        }
    }
}
