use super::*;

// The reader's rules, against a fake SMC.

/// A reader on `smc` with a clock the test moves and a log it reads.
struct Rig {
    reader: HeatReader<FakeSmc>,
    clock: Arc<Mutex<Duration>>,
    reported: Arc<Mutex<Vec<String>>>,
}

impl Rig {
    fn new(smc: FakeSmc) -> Self {
        let clock = Arc::new(Mutex::new(Duration::ZERO));
        let reported = Arc::new(Mutex::new(Vec::new()));
        let (c, r) = (Arc::clone(&clock), Arc::clone(&reported));
        let reader = HeatReader::with_clock(
            smc,
            Box::new(move || *c.lock().unwrap()),
            Box::new(move |line| r.lock().unwrap().push(line.to_string())),
        );
        Self {
            reader,
            clock,
            reported,
        }
    }

    fn reported(&self) -> Vec<String> {
        self.reported.lock().unwrap().clone()
    }

    fn set_clock(&self, to: Duration) {
        *self.clock.lock().unwrap() = to;
    }
}

/// A CPU key that fails leaves the fans, and it is reported.
#[test]
fn a_failing_cpu_key_leaves_the_fans() {
    let mut smc = FakeSmc::default();
    smc.set(
        "TCMz",
        Err(SmcError::SmcRefused {
            key: code("TCMz"),
            result: 0x85,
        }),
    );
    smc.one_fan(2000.0);
    let mut rig = Rig::new(smc);
    let heat = rig.reader.read();
    assert_eq!(heat.cpu_celsius, None);
    assert_eq!(
        heat.fans.iter().map(|f| f.actual_rpm).collect::<Vec<_>>(),
        [2000.0]
    );
    let reported = rig.reported();
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(reported[0].contains("TCMz"), "{reported:?}");
}

/// A fan key that fails leaves the CPU, and is reported once however many
/// polls it keeps failing.
#[test]
fn a_failing_fan_key_leaves_the_cpu_and_is_reported_once() {
    let mut smc = FakeSmc::default();
    smc.set("TCMz", flt(61.0));
    smc.one_fan(2000.0);
    smc.set("F0Ac", Err(SmcError::Kernel(5)));
    let mut rig = Rig::new(smc);
    for _ in 0..3 {
        let heat = rig.reader.read();
        assert_eq!(heat.cpu_celsius, Some(61.0));
        assert_eq!(heat.fans, []);
    }
    assert_eq!(rig.reported().len(), 1, "{:?}", rig.reported());
}

/// A `TCMz` that is not a 4-byte `flt ` is not the CPU key; the hottest
/// `Tp*` sensor is.
#[test]
fn a_tcmz_of_another_type_falls_back_to_the_sensors() {
    let mut smc = FakeSmc::with_table(Ok(vec![code("Tp01"), code("TA0P"), code("Tp02")]));
    smc.set("TCMz", Ok(SmcValue::ui8(3)));
    smc.set("Tp01", flt(50.0));
    smc.set("Tp02", flt(70.0));
    smc.set("TA0P", flt(90.0));
    let mut rig = Rig::new(smc);
    assert_eq!(rig.reader.read().cpu_celsius, Some(70.0));
    assert_eq!(rig.reported(), Vec::<String>::new());
}

/// A failed enumeration is reported, not kept, and not retried before the
/// delay is up.
#[test]
fn a_failed_enumeration_is_retried_after_the_delay() {
    let mut rig = Rig::new(FakeSmc::with_table(Err(SmcError::Kernel(5))));
    assert_eq!(rig.reader.read().cpu_celsius, None);
    assert_eq!(rig.reader.source().keys_calls, 1);
    assert_eq!(rig.reported().len(), 1);

    rig.set_clock(Duration::from_secs(ENUMERATION_RETRY_DELAY.as_secs() - 1));
    assert_eq!(rig.reader.read().cpu_celsius, None);
    assert_eq!(
        rig.reader.source().keys_calls,
        1,
        "retried before the delay"
    );

    rig.set_clock(ENUMERATION_RETRY_DELAY);
    rig.reader.source_mut().key_table = Some(Ok(vec![code("Tp01")]));
    rig.reader.source_mut().set("Tp01", flt(55.0));
    assert_eq!(rig.reader.read().cpu_celsius, Some(55.0));
    assert_eq!(rig.reader.source().keys_calls, 2);

    // Once it has worked, it is kept.
    rig.set_clock(ENUMERATION_RETRY_DELAY + Duration::from_secs(1000));
    rig.reader.read();
    assert_eq!(rig.reader.source().keys_calls, 2);
}

/// A `Tp*` key that fails once is read again next poll.
#[test]
fn a_tp_key_that_fails_once_is_not_dropped() {
    let mut smc = FakeSmc::with_table(Ok(vec![code("Tp01"), code("Tp02")]));
    smc.set("Tp01", flt(50.0));
    smc.set(
        "Tp02",
        Err(SmcError::SmcRefused {
            key: code("Tp02"),
            result: 0x85,
        }),
    );
    let mut rig = Rig::new(smc);
    assert_eq!(rig.reader.read().cpu_celsius, Some(50.0));
    rig.reader.source_mut().set("Tp02", flt(80.0));
    assert_eq!(rig.reader.read().cpu_celsius, Some(80.0));
}

/// NaN, infinities, negatives and out-of-band temperatures never leave the
/// reader.
#[test]
fn non_finite_and_out_of_band_readings_are_dropped() {
    for bad in [f32::NAN, f32::INFINITY, 200.0, 0.0, -3.0] {
        let mut smc = FakeSmc::default();
        smc.set("TCMz", flt(bad));
        assert_eq!(Rig::new(smc).reader.read().cpu_celsius, None, "TCMz {bad}");
    }
    let mut smc = FakeSmc::with_table(Ok(vec![code("Tp01"), code("Tp02")]));
    smc.set("Tp01", flt(f32::NAN));
    smc.set("Tp02", flt(48.0));
    smc.set("FNum", Ok(SmcValue::ui8(3)));
    for fan in 0..3 {
        smc.values
            .insert(fan_key(fan_index(fan), *b"Mn"), flt(1000.0));
        smc.values
            .insert(fan_key(fan_index(fan), *b"Mx"), flt(5000.0));
    }
    smc.set("F0Ac", flt(-5.0));
    smc.set("F1Ac", flt(f32::INFINITY));
    smc.set("F2Ac", flt(1200.0));
    let heat = Rig::new(smc).reader.read();
    assert_eq!(heat.cpu_celsius, Some(48.0));
    assert_eq!(
        heat.fans.iter().map(|f| f.index.get()).collect::<Vec<_>>(),
        [2]
    );
}

/// `Mn` and `Mx` are read once and kept; `Ac` every poll.
#[test]
fn fan_limits_are_read_once() {
    let mut smc = FakeSmc::default();
    smc.one_fan(2000.0);
    let mut rig = Rig::new(smc);
    for _ in 0..3 {
        assert_eq!(
            rig.reader.read().fans.first().map(|f| f.max_rpm),
            Some(5000.0)
        );
    }
    let smc = rig.reader.source();
    assert_eq!(smc.read_count("F0Mn"), 1);
    assert_eq!(smc.read_count("F0Mx"), 1);
    assert_eq!(smc.read_count("F0Ac"), 3);
}

/// The mode key: `F0md` where it exists, and 1 is manual.
#[test]
fn a_fan_whose_mode_key_reads_one_is_manual() {
    let mut smc = FakeSmc::default();
    smc.one_fan(2000.0);
    smc.set("F0md", Ok(SmcValue::ui8(1)));
    assert_eq!(
        Rig::new(smc).reader.read().fans.first().map(|f| f.mode),
        Some(FanMode::Manual)
    );
    let mut smc = FakeSmc::default();
    smc.one_fan(2000.0);
    smc.set("F0Md", Ok(SmcValue::ui8(3)));
    assert_eq!(
        Rig::new(smc).reader.read().fans.first().map(|f| f.mode),
        Some(FanMode::Auto)
    );
}

/// A mode key that will not read is unknown, never automatic: a fan
/// controller that read `false` here would take a held fan for macOS's.
#[test]
fn a_mode_key_that_fails_is_unreadable_not_auto() {
    let mut smc = FakeSmc::default();
    smc.one_fan(2000.0);
    smc.set("F0md", Err(SmcError::Kernel(SmcError::NOT_PRIVILEGED_CODE)));
    let reading = Rig::new(smc).reader.read();
    assert_eq!(
        reading.fans.first().map(|f| f.mode),
        Some(FanMode::Unreadable)
    );
}

/// The target and `Ftst` ride the reading, read every time.
#[test]
fn the_reading_carries_the_target_and_ftst() {
    let mut smc = FakeSmc::default();
    smc.one_fan(2000.0);
    smc.set("F0Tg", Ok(SmcValue::flt(3000.0).unwrap()));
    smc.set("Ftst", Ok(SmcValue::ui8(1)));
    let mut rig = Rig::new(smc);
    let reading = rig.reader.read();
    assert_eq!(
        reading.fans.first().and_then(|f| f.target_rpm),
        Some(3000.0)
    );
    assert_eq!(reading.ftst, Some(true));
    rig.reader.read();
    assert_eq!(rig.reader.source().read_count("F0Tg"), 2);
}

/// The one hardware read. Passes, and says so, only where there is no
/// AppleSMC; any other error, opening or reading, fails it.
#[cfg(target_os = "macos")]
#[test]
fn hardware_read_is_plausible_where_there_is_an_smc() {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    let smc = match SmcConnection::open() {
        Ok(smc) => smc,
        Err(SmcError::Unavailable) => {
            eprintln!("skipped: no AppleSMC here (a VM)");
            return;
        }
        Err(other) => panic!("AppleSMC would not open: {other}"),
    };
    // The write check runs before anything reaches the kernel: a `ui8` to
    // `#KEY` (a `ui32` on every SMC) is `WrongType`, where a write that
    // reached the kernel would come back `NotPrivileged`. Only as non-root,
    // so nothing could land.
    // SAFETY: `getuid` takes nothing, cannot fail and touches no memory.
    if unsafe { getuid() } != 0 {
        assert_eq!(
            smc.write(code("#KEY"), &SmcValue::ui8(1)),
            Err(SmcError::WrongType {
                expected: FourCc::UI32,
                actual: FourCc::UI8
            })
        );
    }
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&errors);
    let mut reader = HeatReader::new(
        smc,
        Box::new(move |line| sink.lock().unwrap().push(line.to_string())),
    );
    let heat = reader.read();
    assert_eq!(*errors.lock().unwrap(), Vec::<String>::new());
    let cpu = heat
        .cpu_celsius
        .expect("a Mac with an SMC has a CPU temperature");
    assert!(PLAUSIBLE_CELSIUS.contains(&cpu), "{cpu}");
    for fan in &heat.fans {
        assert!(fan.max_rpm > 0.0, "fan {}", fan.index.get());
        assert!(fan.actual_rpm >= 0.0, "fan {}", fan.index.get());
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn no_smc_off_a_mac() {
    let mut source = HeatSource::open(Box::new(|_| {}));
    assert_eq!(source.read(), None);
}

/// An opener that fails `failures` times with `error`, then opens a fake
/// SMC with one fan, counting every attempt.
fn flaky_opener(error: SmcError, failures: usize, attempts: &Arc<Mutex<usize>>) -> Opener<FakeSmc> {
    let attempts = Arc::clone(attempts);
    Box::new(move || {
        let mut n = attempts.lock().unwrap();
        *n += 1;
        if *n <= failures {
            return Err(error);
        }
        let mut smc = FakeSmc::default();
        smc.set("TCMz", flt(61.0));
        Ok(smc)
    })
}

/// An SMC that would not open is tried again once the delay is up, not
/// before, and its failure is reported once however often it repeats.
#[test]
fn a_failed_open_is_retried_after_the_delay() {
    let attempts = Arc::new(Mutex::new(0));
    let clock = Arc::new(Mutex::new(Duration::ZERO));
    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let (c, r) = (Arc::clone(&clock), Arc::clone(&reported));
    let mut source = HeatSource::with_opener(
        flaky_opener(SmcError::Kernel(5), 2, &attempts),
        Box::new(move |line| r.lock().unwrap().push(line.to_string())),
        Box::new(move || *c.lock().unwrap()),
    );
    assert_eq!(source.read(), Some(HeatReading::default()));
    assert_eq!(*attempts.lock().unwrap(), 1);

    *clock.lock().unwrap() = Duration::from_secs(59);
    assert_eq!(source.read(), Some(HeatReading::default()));
    assert_eq!(*attempts.lock().unwrap(), 1, "retried before the delay");

    *clock.lock().unwrap() = Duration::from_secs(60);
    assert_eq!(source.read(), Some(HeatReading::default()));
    assert_eq!(*attempts.lock().unwrap(), 2);
    assert_eq!(
        reported.lock().unwrap().len(),
        1,
        "{:?}",
        reported.lock().unwrap()
    );

    *clock.lock().unwrap() = Duration::from_secs(120);
    assert_eq!(source.read().and_then(|h| h.cpu_celsius), Some(61.0));
    assert_eq!(*attempts.lock().unwrap(), 3);
    // Once open, it stays open.
    *clock.lock().unwrap() = Duration::from_secs(1000);
    source.read();
    assert_eq!(*attempts.lock().unwrap(), 3);
}

/// No AppleSMC is asked once: a VM does not grow one.
#[test]
fn no_smc_is_never_asked_again() {
    let attempts = Arc::new(Mutex::new(0));
    let clock = Arc::new(Mutex::new(Duration::ZERO));
    let c = Arc::clone(&clock);
    let mut source = HeatSource::with_opener(
        flaky_opener(SmcError::Unavailable, usize::MAX, &attempts),
        Box::new(|_| {}),
        Box::new(move || *c.lock().unwrap()),
    );
    assert_eq!(source.read(), None);
    *clock.lock().unwrap() = Duration::from_secs(3600);
    assert_eq!(source.read(), None);
    assert_eq!(*attempts.lock().unwrap(), 1);
}
