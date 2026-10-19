//! What fan control costs, as (thread CPU, wall) pairs. Ignored: run by hand with
//! `cargo test -p tcr-fan --release --test cost -- --ignored --nocapture`.
//!
//! Read-only on the machine: the stage 1 reader and the Macs Fan Control probe.
//! The controller's own tick runs over fakes (no helper is dialled, nothing is
//! written), and the real per-tick cost is that plus one reader read and one probe.
#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "a measurement run by hand: a failed setup should stop it, and it prints what it measured"
)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tcr_fan::controller::{
    Clock, Command, Controller, FanDesired, Fault, FollowCurve, Generation, Heat, Helper, Input,
    Parts, Probe, Rpm, Saved, Store,
};
use tcr_fan::wire::{PROTOCOL_VERSION, Reply, Request};
use tcr_smc::{FanIndex, FanMode, FanReading, HeatReading};

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a live timespec for the call to fill; CLOCK_THREAD_CPUTIME_ID
    // exists on every macOS this builds for.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) };
    assert_eq!(rc, 0);
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Mean (CPU, wall) per call over `n` calls.
fn measure(n: u32, mut f: impl FnMut()) -> (Duration, Duration) {
    let (cpu, wall) = (thread_cpu(), Instant::now());
    for _ in 0..n {
        f();
    }
    // A thread's CPU clock never runs backwards; saturating keeps a broken one at zero
    // rather than panicking mid-measurement.
    (thread_cpu().saturating_sub(cpu) / n, wall.elapsed() / n)
}

fn show(what: &str, (cpu, wall): (Duration, Duration)) {
    println!(
        "{what}: cpu {:.1} us, wall {:.1} us",
        cpu.as_secs_f64() * 1e6,
        wall.as_secs_f64() * 1e6
    );
}

/// Root as a fake: answers every op at once and keeps the targets.
struct Root(Fans);

impl Helper for Root {
    fn open(&mut self, _: Generation) -> Result<(), Fault> {
        Ok(())
    }

    fn send(&mut self, request: &Request, _: Duration) -> Result<Reply, Fault> {
        Ok(match *request {
            Request::Version => Reply::Version {
                protocol: PROTOCOL_VERSION,
                build: "cost".into(),
            },
            Request::SetManual { fan } => {
                for f in &mut self.0.0.lock().unwrap().fans {
                    if f.index == fan {
                        f.mode = FanMode::Manual;
                    }
                }
                Reply::Manual { unlocked: false }
            }
            Request::SetTarget { fan, rpm } => {
                for f in &mut self.0.0.lock().unwrap().fans {
                    if f.index == fan {
                        f.target_rpm = Some(rpm);
                    }
                }
                Reply::Target { rpm }
            }
            Request::HandBack => Reply::HandedBack,
        })
    }

    fn close(&mut self) {}
}

/// The fans as root left them: manual at whatever target the controller asks for.
#[derive(Clone)]
struct Fans(std::sync::Arc<std::sync::Mutex<HeatReading>>);

impl Heat for Fans {
    fn read(&mut self) -> Option<HeatReading> {
        Some(self.0.lock().unwrap().clone())
    }
}

struct NoMacs;

impl Probe for NoMacs {
    fn macs_running(&mut self) -> Result<bool, String> {
        Ok(false)
    }
}

struct Now;

impl Clock for Now {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall_ms(&self) -> i64 {
        0
    }

    fn sleep(&self, _: Duration) {}
}

struct Nowhere;

impl Store for Nowhere {
    fn save(&mut self, _: &Saved) {}
}

fn held_at(rpm: f64) -> HeatReading {
    HeatReading {
        cpu_celsius: Some(70.0),
        fans: (0..2)
            .map(|i| FanReading {
                index: FanIndex::new(i).unwrap(),
                actual_rpm: rpm,
                min_rpm: 1350.0,
                max_rpm: 5777.0,
                mode: FanMode::Auto,
                target_rpm: Some(rpm),
            })
            .collect(),
        ftst: Some(true),
    }
}

/// A controller holding `mode`, whose fake fans read back exactly what it wrote.
fn holding(mode: FanDesired, rpm: f64) -> Controller<Root, Fans, NoMacs, Now, Nowhere> {
    let fans = Fans(std::sync::Arc::new(std::sync::Mutex::new(held_at(rpm))));
    let (tx, rx) = mpsc::channel();
    tx.send(Input::Command {
        seq: 1,
        command: Command::Set(mode),
    })
    .unwrap();
    let mut controller = Controller::new(
        Parts {
            helper: Root(fans.clone()),
            heat: fans,
            probe: NoMacs,
            clock: Now,
            store: Nowhere,
            inbox: rx,
            log: Box::new(|_, _| {}),
            publish: Box::new(|_| {}),
            pid: 1,
            commands_before: 0,
        },
        None,
    );
    controller.pump();
    assert_eq!(controller.state(), tcr_fan::controller::State::Holding);
    assert_eq!(
        controller.tick(false),
        tcr_fan::controller::TickOutcome::NoChange
    );
    controller
}

#[test]
#[ignore = "a measurement, run by hand"]
fn cost_pairs() {
    let n = 2000;
    let mut fixed = holding(
        FanDesired::Fixed {
            rpm: Rpm::new(5000).unwrap(),
        },
        5000.0,
    );
    show(
        "controller tick, Fixed, fakes",
        measure(n, || {
            fixed.tick(false);
        }),
    );
    // At 70 °C on a 60-90 curve the target is a third of the range.
    let third = 1350.0 + (5777.0 - 1350.0) / 3.0;
    let mut follow = holding(
        FanDesired::FollowCpu {
            curve: FollowCurve::new(60, 90).unwrap(),
        },
        third,
    );
    show(
        "controller tick, Follow CPU, fakes",
        measure(n, || {
            follow.tick(false);
        }),
    );

    let mut probe = tcr_fan::macs::MacsProbe::new();
    show(
        "Macs Fan Control probe, first (cold cache)",
        measure(1, || {
            probe.running().unwrap();
        }),
    );
    show(
        "Macs Fan Control probe, warm",
        measure(200, || {
            probe.running().unwrap();
        }),
    );
    println!("Macs Fan Control running now: {}", probe.running().unwrap());

    let mut source = tcr_smc::HeatSource::open(Box::new(|line| println!("{line}")));
    show(
        "stage 1 reader, first read",
        measure(1, || {
            source.read();
        }),
    );
    show(
        "stage 1 reader, warm read (one tick's read)",
        measure(200, || {
            source.read();
        }),
    );
}
