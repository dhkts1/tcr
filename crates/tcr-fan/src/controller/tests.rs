//! The controller over a fake helper that acts on a fake machine: set-manual
//! puts the fan in manual, set-target moves its target (clamped), hand-back
//! puts every fan back on automatic, so what the controller reads next is
//! what its writes did.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

use tcr_smc::FanMode;

use super::*;

fn fan(index: u8) -> FanIndex {
    FanIndex::new(index).unwrap()
}

/// Where the rig's command numbers start, as the proxy seeds them from the
/// wall clock.
const COMMANDS_BEFORE: u64 = 1_760_000_000_000;

/// The controller's channel, numbering commands in channel order as the
/// proxy's handle does.
#[derive(Clone)]
struct Inbox {
    tx: Sender<Input>,
    sent: Arc<Mutex<u64>>,
}

impl Inbox {
    /// Send `command` under the next number, and return that number.
    fn command(&self, command: Command) -> u64 {
        let mut sent = self.sent.lock().unwrap();
        *sent += 1;
        self.tx
            .send(Input::Command {
                seq: *sent,
                command,
            })
            .unwrap();
        *sent
    }
}

type Hook = Box<
    dyn FnMut(&Request, &mut Machine, &Inbox, &FakeClock) -> Option<Result<Reply, Fault>> + Send,
>;

struct Machine {
    reading: Option<HeatReading>,
    macs: Result<bool, String>,
    requests: Vec<Request>,
    opened: Vec<Generation>,
    closed: usize,
    /// How long the first reply on each new connection takes: launchd
    /// starting the helper late. Past that call's deadline it is `NoReply`.
    first_reply_after: Duration,
    /// No request sent yet on the current connection.
    fresh: bool,
}

impl Machine {
    fn fan_mut(&mut self, index: FanIndex) -> Option<&mut FanReading> {
        self.reading
            .as_mut()?
            .fans
            .iter_mut()
            .find(|f| f.index == index)
    }

    /// What root would do.
    fn act(&mut self, request: &Request) -> Result<Reply, Fault> {
        Ok(match *request {
            Request::Version => Reply::Version {
                protocol: PROTOCOL_VERSION,
                build: "test".into(),
            },
            Request::SetManual { fan } => {
                if let Some(f) = self.fan_mut(fan) {
                    f.mode = FanMode::Manual;
                }
                Reply::Manual { unlocked: false }
            }
            Request::SetTarget { fan, rpm } => {
                let Some(f) = self.fan_mut(fan) else {
                    return Ok(Reply::Refused(HelperError::BadRequest));
                };
                let written = rpm.clamp(f.min_rpm, f.max_rpm);
                f.target_rpm = Some(written);
                Reply::Target { rpm: written }
            }
            Request::HandBack => {
                if let Some(reading) = self.reading.as_mut() {
                    for f in &mut reading.fans {
                        f.mode = FanMode::Auto;
                    }
                }
                Reply::HandedBack
            }
        })
    }

    fn written(&self) -> Vec<Request> {
        self.requests
            .iter()
            .copied()
            .filter(|r| !matches!(r, Request::Version))
            .collect()
    }
}

#[derive(Clone)]
struct World(Arc<Mutex<Machine>>);

impl World {
    fn lock(&self) -> MutexGuard<'_, Machine> {
        self.0.lock().unwrap()
    }
}

struct FakeHelper {
    world: World,
    hook: Option<Hook>,
    inject: Inbox,
    clock: FakeClock,
    hooks: Arc<Mutex<Option<Hook>>>,
}

impl Helper for FakeHelper {
    fn open(&mut self, generation: Generation) -> Result<(), Fault> {
        let mut machine = self.world.lock();
        machine.opened.push(generation);
        machine.fresh = true;
        Ok(())
    }

    fn send(&mut self, request: &Request, deadline: Duration) -> Result<Reply, Fault> {
        if self.hook.is_none() {
            self.hook = self.hooks.lock().unwrap().take();
        }
        let mut machine = self.world.lock();
        machine.requests.push(*request);
        if std::mem::take(&mut machine.fresh) && !machine.first_reply_after.is_zero() {
            let waited = machine.first_reply_after.min(deadline);
            self.clock.advance(waited);
            if machine.first_reply_after > deadline {
                return Err(Fault::NoReply);
            }
        }
        if let Some(hook) = self.hook.as_mut()
            && let Some(answer) = hook(request, &mut machine, &self.inject, &self.clock)
        {
            return answer;
        }
        machine.act(request)
    }

    fn close(&mut self) {
        self.world.lock().closed += 1;
    }
}

struct FakeHeat(World);

impl Heat for FakeHeat {
    fn read(&mut self) -> Option<HeatReading> {
        self.0.lock().reading.clone()
    }
}

struct FakeProbe(World);

impl Probe for FakeProbe {
    fn macs_running(&mut self) -> Result<bool, String> {
        self.0.lock().macs.clone()
    }
}

#[derive(Clone)]
struct FakeClock(Arc<Mutex<(Instant, i64)>>);

impl FakeClock {
    fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap();
        now.0 += by;
        now.1 += i64::try_from(by.as_millis()).unwrap();
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.0.lock().unwrap().0
    }

    fn wall_ms(&self) -> i64 {
        self.0.lock().unwrap().1
    }

    fn sleep(&self, duration: Duration) {
        self.advance(duration);
    }
}

#[derive(Clone, Default)]
struct FakeStore(Arc<Mutex<Vec<Saved>>>);

impl Store for FakeStore {
    fn save(&mut self, saved: &Saved) {
        self.0.lock().unwrap().push(saved.clone());
    }
}

type Logs = Arc<Mutex<Vec<(Level, String)>>>;

type Under = Controller<FakeHelper, FakeHeat, FakeProbe, FakeClock, FakeStore>;

struct Rig {
    c: Under,
    world: World,
    inbox: Inbox,
    clock: FakeClock,
    store: FakeStore,
    hooks: Arc<Mutex<Option<Hook>>>,
    logs: Logs,
    /// Every snapshot the controller published, in order.
    published: Arc<Mutex<Vec<Snapshot>>>,
}

/// Two fans of the Mac16,7 reading, automatic, CPU at 50 °C.
fn machine() -> HeatReading {
    HeatReading {
        cpu_celsius: Some(50.0),
        fans: (0..2)
            .map(|i| FanReading {
                index: fan(i),
                actual_rpm: 1400.0,
                min_rpm: 1350.0,
                max_rpm: 5777.0,
                mode: FanMode::Auto,
                target_rpm: Some(0.0),
            })
            .collect(),
        ftst: Some(false),
    }
}

fn rig_from(saved: Option<Saved>, reading: Option<HeatReading>) -> Rig {
    let world = World(Arc::new(Mutex::new(Machine {
        reading,
        macs: Ok(false),
        requests: Vec::new(),
        opened: Vec::new(),
        closed: 0,
        first_reply_after: Duration::ZERO,
        fresh: false,
    })));
    let (tx, rx) = mpsc::channel();
    let inbox = Inbox {
        tx,
        sent: Arc::new(Mutex::new(COMMANDS_BEFORE)),
    };
    let clock = FakeClock(Arc::new(Mutex::new((Instant::now(), 1_000_000))));
    let store = FakeStore::default();
    let hooks: Arc<Mutex<Option<Hook>>> = Arc::new(Mutex::new(None));
    let logs: Logs = Arc::default();
    let written_logs = Arc::clone(&logs);
    let published: Arc<Mutex<Vec<Snapshot>>> = Arc::default();
    let shown = Arc::clone(&published);
    let parts = Parts {
        helper: FakeHelper {
            world: world.clone(),
            hook: None,
            inject: inbox.clone(),
            clock: clock.clone(),
            hooks: Arc::clone(&hooks),
        },
        heat: FakeHeat(world.clone()),
        probe: FakeProbe(world.clone()),
        clock: clock.clone(),
        store: store.clone(),
        inbox: rx,
        log: Box::new(move |level, line: &str| {
            written_logs.lock().unwrap().push((level, line.to_string()));
        }),
        publish: Box::new(move |snapshot: &Snapshot| {
            shown.lock().unwrap().push(snapshot.clone());
        }),
        pid: 4242,
        commands_before: COMMANDS_BEFORE,
    };
    Rig {
        c: Controller::new(parts, saved),
        world,
        inbox,
        clock,
        store,
        hooks,
        logs,
        published,
    }
}

fn rig() -> Rig {
    rig_from(None, Some(machine()))
}

fn fixed(rpm: u32) -> FanDesired {
    FanDesired::Fixed {
        rpm: Rpm::new(rpm).unwrap(),
    }
}

fn follow(quiet: u8, full: u8) -> FanDesired {
    FanDesired::FollowCpu {
        curve: FollowCurve::new(quiet, full).unwrap(),
    }
}

impl Rig {
    fn send(&mut self, command: Command) -> Flow {
        self.inbox.command(command);
        self.c.pump()
    }

    fn hook(&self, hook: Hook) {
        *self.hooks.lock().unwrap() = Some(hook);
    }

    /// Advance past the tick and run it.
    fn next_tick(&mut self) -> Option<TickOutcome> {
        self.clock.advance(TICK);
        self.c.pump();
        self.c.last_outcome()
    }

    fn clear_requests(&self) {
        self.world.lock().requests.clear();
    }

    fn written(&self) -> Vec<Request> {
        self.world.lock().written()
    }

    fn set_target(&self, index: u8, rpm: f64) {
        self.world.lock().fan_mut(fan(index)).unwrap().target_rpm = Some(rpm);
    }

    fn cpu(&self, celsius: f64) {
        self.world.lock().reading.as_mut().unwrap().cpu_celsius = Some(celsius);
    }

    fn macs(&self, running: Result<bool, String>) {
        self.world.lock().macs = running;
    }

    /// Every fan back on automatic, as macOS taking them would leave it.
    fn all_auto(&self) {
        for f in &mut self.world.lock().reading.as_mut().unwrap().fans {
            f.mode = FanMode::Auto;
        }
    }

    /// The helper answers `result` to every request `when` picks, and acts
    /// as root would on the rest.
    fn answer(
        &self,
        when: impl Fn(&Request) -> bool + Send + 'static,
        result: Result<Reply, Fault>,
    ) {
        self.hook(Box::new(move |request, _machine, _inject, _clock| {
            when(request).then(|| result.clone())
        }));
    }

    /// A connection event from connection `generation`, then a pump.
    fn fault(&mut self, generation: Generation, fault: Fault) {
        self.inbox
            .tx
            .send(Input::Xpc { generation, fault })
            .unwrap();
        self.c.pump();
    }
}

fn holding(mode: FanDesired) -> Rig {
    let mut rig = rig();
    rig.send(Command::Set(mode));
    assert_eq!(rig.c.state(), State::Holding);
    rig
}

fn set_target(index: u8, rpm: f64) -> Request {
    Request::SetTarget {
        fan: fan(index),
        rpm,
    }
}

fn set_manual(index: u8) -> Request {
    Request::SetManual { fan: fan(index) }
}

/// Both fans' set-targets at `rpm`.
fn targets(rpm: f64) -> Vec<Request> {
    vec![set_target(0, rpm), set_target(1, rpm)]
}

/// An entry's writes: both set-manuals, then both set-targets at `rpm`.
fn entry(rpm: f64) -> Vec<Request> {
    [vec![set_manual(0), set_manual(1)], targets(rpm)].concat()
}

/// An entry's requests, the version call first.
fn entry_requests(rpm: f64) -> Vec<Request> {
    [vec![Request::Version], entry(rpm)].concat()
}

// The happy paths.

#[test]
fn fixed_enters_every_fan_then_targets_them() {
    let rig = holding(fixed(5000));
    assert_eq!(rig.world.lock().requests, entry_requests(5000.0));
    let hold = rig.c.hold().unwrap();
    assert_eq!(hold.writer_pid, 4242);
    assert!(hold.fans.iter().all(|f| f.written_rpm == Some(5000.0)));
    // The record was on disk before the first write.
    let saves = rig.store.0.lock().unwrap();
    assert!(saves.iter().any(|s| {
        s.hold
            .as_ref()
            .is_some_and(|h| h.fans.iter().all(|f| f.written_rpm.is_none()))
    }));
}

#[test]
fn auto_hands_back_and_clears_the_record() {
    let mut rig = holding(fixed(5000));
    rig.clear_requests();
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.c.state(), State::Off);
    assert_eq!(rig.written(), vec![Request::HandBack]);
    assert_eq!(rig.c.hold(), None);
    assert!(!rig.c.connected());
}

#[test]
fn a_steady_fixed_tick_writes_nothing() {
    let mut rig = holding(fixed(5000));
    rig.clear_requests();
    assert_eq!(rig.next_tick(), Some(TickOutcome::NoChange));
    assert_eq!(rig.written(), vec![]);
}

#[test]
fn follow_writes_only_past_the_step() {
    let mut rig = holding(follow(60, 90));
    // 50 °C is quiet: the minimum.
    assert_eq!(rig.c.hold().unwrap().fans[0].written_rpm, Some(1350.0));
    rig.clear_requests();
    // 61 °C moves the target 1/30 of the range: under 5 %.
    rig.cpu(61.0);
    assert_eq!(rig.next_tick(), Some(TickOutcome::NoChange));
    // 75 °C is half way.
    rig.cpu(75.0);
    assert_eq!(rig.next_tick(), Some(TickOutcome::Wrote(2)));
    let half = 1350.0 + (5777.0 - 1350.0) / 2.0;
    assert_eq!(rig.written(), targets(half));
}

#[test]
fn the_hot_guard_runs_full_until_it_cools() {
    // A curve that reaches full only at 100 °C, so between 90 and 100 °C the
    // guard, not the curve, is what holds full speed.
    let mut rig = holding(follow(40, 100));
    rig.clear_requests();
    rig.cpu(100.0);
    assert_eq!(rig.next_tick(), Some(TickOutcome::Wrote(2)));
    assert!(rig.c.snapshot().hot_guard);
    rig.cpu(95.0);
    assert_eq!(rig.next_tick(), Some(TickOutcome::NoChange));
    assert!(rig.c.snapshot().hot_guard);
    rig.cpu(89.0);
    assert_eq!(rig.next_tick(), Some(TickOutcome::Wrote(2)));
    assert!(!rig.c.snapshot().hot_guard);
    let cooled = (49.0_f64 / 60.0).mul_add(5777.0 - 1350.0, 1350.0);
    assert_eq!(rig.written(), [targets(5777.0), targets(cooled)].concat());
}

/// A round's plan, worked out before any write: the cases the two tests above
/// drive through the rig, plus a forced round, a fan not held and a range
/// that did not read.
#[test]
fn a_round_plans_the_hot_guard_and_the_gate() {
    let range = 5777.0 - 1350.0;
    let held = |rpm: f64| Hold {
        writer_pid: 4242,
        fans: (0..2)
            .map(|i| HeldFan {
                fan: fan(i),
                written_rpm: Some(rpm),
            })
            .collect(),
    };
    let both = |rpm: f64| vec![(fan(0), rpm), (fan(1), rpm)];
    let at = |celsius: f64| HeatReading {
        cpu_celsius: Some(celsius),
        ..machine()
    };
    let step = (1.0_f64 / 30.0).mul_add(range, 1350.0);
    let half = 1350.0 + range / 2.0;
    let cooled = (49.0_f64 / 60.0).mul_add(range, 1350.0);
    // (desired, cpu, previous, was hot, forced) -> (hot, writes)
    let cases = [
        (follow(60, 90), 61.0, 1350.0, false, false, false, vec![]),
        (follow(60, 90), 61.0, 1350.0, false, true, false, both(step)),
        (
            follow(60, 90),
            75.0,
            1350.0,
            false,
            false,
            false,
            both(half),
        ),
        (
            follow(40, 100),
            100.0,
            1350.0,
            false,
            false,
            true,
            both(5777.0),
        ),
        (follow(40, 100), 95.0, 5777.0, true, false, true, vec![]),
        (
            follow(40, 100),
            89.0,
            5777.0,
            true,
            false,
            false,
            both(cooled),
        ),
        (fixed(5000), 104.0, 5000.0, false, false, false, vec![]),
        (fixed(5100), 50.0, 5000.0, false, false, false, both(5100.0)),
    ];
    for (desired, cpu, previous, was_hot, forced, hot, writes) in cases {
        let round = plan_round(desired, &at(cpu), Some(&held(previous)), was_hot, forced);
        let case = format!("{desired:?} at {cpu} from {previous}, hot {was_hot}, forced {forced}");
        assert_eq!(round.hot, hot, "{case}");
        assert_eq!(round.writes, writes, "{case}");
        assert!(!round.limits_unknown, "{case}");
    }

    // No record: nothing is ours to write.
    let round = plan_round(fixed(5000), &machine(), None, false, true);
    assert_eq!(round.writes, vec![]);
    // A held fan whose range did not read, in a mode that needs it.
    let mut no_range = machine();
    no_range.fans[1].max_rpm = 0.0;
    let round = plan_round(follow(60, 90), &no_range, Some(&held(5000.0)), false, true);
    assert_eq!(round.writes, vec![(fan(0), 1350.0)]);
    assert!(round.limits_unknown);
}

/// Fixed holds exactly the rpm asked for, however hot (Gil, 2026-10-18: the
/// guard turned `fixed 5000` into 5777 at 104-106 °C). Entering hot, holding
/// hot, and switching to Fixed while the guard is on in Follow CPU.
#[test]
fn the_hot_guard_never_raises_a_fixed_target() {
    let mut hot_reading = machine();
    hot_reading.cpu_celsius = Some(106.0);
    let mut rig = rig_from(None, Some(hot_reading));
    rig.send(Command::Set(fixed(5000)));
    assert_eq!(rig.c.state(), State::Holding);
    assert_eq!(rig.written(), entry(5000.0));
    assert!(!rig.c.snapshot().hot_guard);
    rig.clear_requests();
    rig.cpu(104.0);
    assert_eq!(rig.next_tick(), Some(TickOutcome::NoChange));
    assert_eq!(rig.written(), vec![]);
    assert!(!rig.c.snapshot().hot_guard);

    let mut rig = holding(follow(40, 100));
    rig.cpu(105.0);
    assert_eq!(rig.next_tick(), Some(TickOutcome::Wrote(2)));
    assert!(rig.c.snapshot().hot_guard);
    rig.clear_requests();
    rig.send(Command::Set(fixed(3000)));
    assert!(!rig.c.snapshot().hot_guard);
    assert_eq!(rig.written(), targets(3000.0));
    assert_eq!(rig.next_tick(), Some(TickOutcome::NoChange));
    assert_eq!(rig.c.hold().unwrap().fans[0].written_rpm, Some(3000.0));
}

#[test]
fn a_new_fixed_speed_is_written_at_once() {
    let mut rig = holding(fixed(5000));
    rig.clear_requests();
    rig.send(Command::Set(fixed(5100)));
    assert_eq!(rig.written(), targets(5100.0));
}

// H1: whose fans.

/// A target another program moved is not ours: Foreign, and no write.
#[test]
fn foreign_target_is_not_ours() {
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.set_target(0, 4200.0);
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::AnotherApp))
    );
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::AnotherApp));
    assert_eq!(rig.c.hold(), None);
    assert!(!rig.c.connected());
}

/// Even a new command never writes over a moved target.
#[test]
fn tick_never_writes_when_target_changed() {
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.set_target(1, 3000.0 + 2.0);
    rig.send(Command::Set(fixed(3500)));
    assert_eq!(rig.written(), vec![]);
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::AnotherApp));
    for _ in 0..3 {
        rig.next_tick();
        assert_eq!(rig.written(), vec![]);
    }
}

#[test]
fn no_write_while_macs_runs() {
    // Entering: not even a version call.
    let mut rig = rig();
    rig.macs(Ok(true));
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::MacsFanControl));
    assert_eq!(rig.world.lock().requests, vec![]);
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::MacsFanControl))
    );
    assert_eq!(rig.world.lock().requests, vec![]);

    // Holding.
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.macs(Ok(true));
    rig.cpu(100.0);
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::MacsFanControl))
    );
    rig.send(Command::Set(fixed(4000)));
    assert_eq!(rig.written(), vec![]);
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::MacsFanControl));

    // Releasing: Auto waits, then hands back once it quits.
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.macs(Ok(true));
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.written(), vec![]);
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::MacsFanControl));
    assert_eq!(rig.c.detail(), Some(Detail::WaitingForMacs));
    rig.next_tick();
    assert_eq!(rig.written(), vec![]);
    rig.macs(Ok(false));
    rig.next_tick();
    assert_eq!(rig.written(), vec![Request::HandBack]);
    assert_eq!(rig.c.state(), State::Off);

    // A force waits too.
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.macs(Ok(true));
    rig.send(Command::ForceHandBack);
    assert_eq!(rig.written(), vec![]);
}

#[test]
fn read_error_writes_nothing() {
    // Follow CPU: at 100 °C the curve wants full speed, so every held-back
    // tick below is a write that was refused, not one with nothing to do.
    let mut rig = holding(follow(60, 90));
    rig.clear_requests();
    rig.world.lock().fan_mut(fan(1)).unwrap().mode = FanMode::Unreadable;
    rig.cpu(100.0);
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::Unreadable))
    );
    assert_eq!(rig.c.state(), State::Unknown);
    rig.world.lock().fan_mut(fan(1)).unwrap().target_rpm = None;
    rig.world.lock().fan_mut(fan(1)).unwrap().mode = FanMode::Manual;
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::Unreadable))
    );
    let probe_failed = {
        rig.macs(Err("proc_listallpids failed".into()));
        rig.next_tick()
    };
    assert_eq!(probe_failed, Some(TickOutcome::HeldBack(Why::ProbeFailed)));
    assert_eq!(rig.written(), vec![]);
    // Readable again and still ours: back to holding, and full speed is written.
    rig.macs(Ok(false));
    rig.world.lock().fan_mut(fan(1)).unwrap().target_rpm = Some(1350.0);
    assert_eq!(rig.next_tick(), Some(TickOutcome::Wrote(2)));
    assert_eq!(rig.c.state(), State::Holding);
}

#[test]
fn holder_rechecked_each_tick() {
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    assert_eq!(rig.next_tick(), Some(TickOutcome::NoChange));
    assert_eq!(rig.next_tick(), Some(TickOutcome::NoChange));
    rig.set_target(0, 2500.0);
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::AnotherApp))
    );
    assert_eq!(rig.written(), vec![]);
    // When it hands the fans back, we take them again (not counted).
    rig.all_auto();
    rig.next_tick();
    assert_eq!(rig.c.state(), State::Holding);
}

// H2: a wish that changes during the unlock.

/// set-manual takes 3 s (the unlock settle); Auto arrives inside it. The
/// controller must see Auto before its next write: no second set-manual, no
/// set-target, a hand-back, and Off.
#[test]
fn fixed_then_auto_within_unlock_ends_off() {
    let mut rig = rig();
    let mut sent = false;
    rig.hook(Box::new(move |request, _machine, inject, clock| {
        if matches!(request, Request::SetManual { .. }) && !sent {
            clock.advance(Duration::from_secs(3));
            inject.command(Command::Set(FanDesired::Auto));
            sent = true;
        }
        None
    }));
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.c.state(), State::Off);
    assert_eq!(rig.c.desired(), FanDesired::Auto);
    assert_eq!(rig.written(), vec![set_manual(0), Request::HandBack]);
    assert_eq!(rig.c.hold(), None);
    let machine = rig.world.lock();
    assert!(
        machine
            .reading
            .as_ref()
            .unwrap()
            .fans
            .iter()
            .all(|f| f.mode == FanMode::Auto)
    );
}

/// The latest of several wishes wins.
#[test]
fn the_latest_wish_during_the_unlock_wins() {
    let mut rig = rig();
    let mut sent = false;
    rig.hook(Box::new(move |request, _machine, inject, _clock| {
        if matches!(request, Request::SetManual { .. }) && !sent {
            for rpm in [4000, 4500] {
                let mode = FanDesired::Fixed {
                    rpm: Rpm::new(rpm).unwrap(),
                };
                inject.command(Command::Set(mode));
            }
            sent = true;
        }
        None
    }));
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.c.state(), State::Holding);
    assert_eq!(rig.written(), entry(4500.0));
}

// H3: partial failure.

#[test]
fn second_fan_failure_hands_back_before_error() {
    let mut rig = rig();
    rig.answer(
        |request| *request == set_manual(1),
        Ok(Reply::Refused(HelperError::ModeLocked { fan: 1 })),
    );
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(
        rig.written(),
        vec![set_manual(0), set_manual(1), Request::HandBack]
    );
    assert_eq!(
        rig.c.state(),
        State::Unavailable(Unavailable::ModeLocked { fan: 1 })
    );
    assert_eq!(
        rig.c.hold(),
        None,
        "the hand-back worked, so nothing is held"
    );
    assert!(!rig.c.connected());
}

#[test]
fn a_target_failure_hands_back_too() {
    let mut rig = rig();
    rig.answer(
        |request| *request == set_target(1, 3000.0),
        Err(Fault::NoReply),
    );
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.written().last(), Some(&Request::HandBack));
    assert_eq!(
        rig.c.state(),
        State::Unavailable(Unavailable::HelperNoReply)
    );
}

/// launchd starts the helper no sooner than 10 s after its previous start
/// (measured 2026-10-18), so the first reply on a new connection can take
/// 11 s: the version call waits for it and the entry goes on.
#[test]
fn a_helper_launchd_starts_late_still_answers_the_version_call() {
    let mut late = rig();
    late.world.lock().first_reply_after = Duration::from_secs(11);
    late.send(Command::Set(fixed(5000)));
    assert_eq!(late.c.state(), State::Holding);
    assert_eq!(late.world.lock().requests, entry_requests(5000.0));

    // Past the deadline is still no reply: the wait is bounded.
    let mut too_late = rig();
    too_late.world.lock().first_reply_after = VERSION_DEADLINE + Duration::from_secs(1);
    too_late.send(Command::Set(fixed(5000)));
    assert_eq!(
        too_late.c.state(),
        State::Unavailable(Unavailable::HelperNoReply)
    );
    assert_eq!(too_late.written(), vec![]);
}

/// A fan that will not leave automatic is not put through the unlock every
/// minute: three refusals in a row stop the retry until the next command, a
/// sleep included.
#[test]
fn three_failed_entries_stop_the_retry() {
    let mut rig = rig();
    rig.answer(
        |request| *request == set_manual(0),
        Ok(Reply::Refused(HelperError::ModeLocked { fan: 0 })),
    );
    rig.send(Command::Set(fixed(3000)));
    for wait in [5, 10] {
        assert_eq!(
            rig.c.state(),
            State::Unavailable(Unavailable::ModeLocked { fan: 0 })
        );
        rig.clock.advance(Duration::from_secs(wait));
        rig.c.pump();
    }
    assert_eq!(rig.c.detail(), Some(Detail::RetryStopped));
    assert_eq!(rig.c.wait(), None, "nothing scheduled");
    let calls = rig.world.lock().requests.len();
    rig.clock.advance(Duration::from_secs(600));
    rig.c.pump();
    rig.slept_for(Duration::from_secs(3_600));
    rig.c.pump();
    assert_eq!(
        rig.world.lock().requests.len(),
        calls,
        "no fourth try on its own"
    );
    // A new command tries again.
    rig.send(Command::Set(fixed(3000)));
    assert!(rig.world.lock().requests.len() > calls);
}

impl Rig {
    /// The machine slept for `asleep`: the wall clock moves and the monotonic
    /// one does not, as `mach_absolute_time` stands still in sleep. Then one
    /// tick's worth of both, the loop's next look.
    fn slept_for(&self, asleep: Duration) {
        self.clock.0.lock().unwrap().1 += i64::try_from(asleep.as_millis()).unwrap();
        self.clock.advance(TICK);
    }

    fn opens(&self) -> usize {
        self.world.lock().opened.len()
    }

    /// Lines logged at Info or louder since the last [`Rig::take_logs`].
    fn loud_logs(&self) -> Vec<String> {
        self.take_logs()
            .into_iter()
            .filter(|(level, _)| *level != Level::Debug)
            .map(|(_, line)| line)
            .collect()
    }
}

/// A helper whose reply never comes (the SMC taking no mode write in a dark
/// wake, 2026-10-19). Three failures in a row, then `answers` decides.
fn silent_three_times(answers: &Arc<std::sync::atomic::AtomicBool>) -> Rig {
    let mut rig = rig();
    let answers = Arc::clone(answers);
    rig.answer(
        move |request| {
            *request == Request::Version && !answers.load(std::sync::atomic::Ordering::SeqCst)
        },
        Err(Fault::NoReply),
    );
    rig.send(Command::Set(fixed(3000)));
    for wait in [5, 10] {
        rig.clock.advance(Duration::from_secs(wait));
        rig.c.pump();
    }
    assert_eq!(rig.opens(), 3);
    assert_eq!(
        rig.c.state(),
        State::Unavailable(Unavailable::HelperNoReply)
    );
    rig
}

/// The overnight case (2026-10-19): the three tries were spent in dark wakes
/// and nothing took the fans back after the real wake. While a manual mode is
/// wished a silent or unreachable helper is tried every minute past the
/// third failure, with no line logged per try, until it answers.
#[test]
fn a_silent_helper_is_retried_every_minute_past_three_quietly() {
    let answers = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut rig = silent_three_times(&answers);
    assert_eq!(rig.c.detail(), Some(Detail::RetryingEveryMinute));
    rig.take_logs();
    for n in 4..=8 {
        rig.clock
            .advance(RETRY_EVERY.checked_sub(Duration::from_secs(1)).unwrap());
        rig.c.pump();
        assert_eq!(rig.opens(), n - 1, "not before a minute");
        rig.clock.advance(Duration::from_secs(1));
        rig.c.pump();
        assert_eq!(rig.opens(), n, "try {n} at a minute");
        assert_eq!(
            rig.c.state(),
            State::Unavailable(Unavailable::HelperNoReply)
        );
        assert_eq!(rig.c.detail(), Some(Detail::RetryingEveryMinute));
    }
    assert_eq!(rig.loud_logs(), Vec::<String>::new(), "no line per try");

    // An unreachable helper is the same.
    let mut unreachable = rig_for_unreachable();
    for _ in 0..6 {
        let opens = unreachable.opens();
        unreachable.clock.advance(RETRY_EVERY);
        unreachable.c.pump();
        assert_eq!(unreachable.opens(), opens + 1);
    }

    // The helper answers: holding, said once.
    answers.store(true, std::sync::atomic::Ordering::SeqCst);
    rig.clock.advance(RETRY_EVERY);
    rig.c.pump();
    assert_eq!(rig.c.state(), State::Holding);
    assert_eq!(rig.c.detail(), None);
    let loud = rig.loud_logs();
    assert_eq!(loud.len(), 1, "{loud:?}");
    assert!(loud[0].ends_with("-> holding"), "{loud:?}");
}

fn rig_for_unreachable() -> Rig {
    let mut rig = rig();
    rig.answer(|request| *request == Request::Version, Err(Fault::Invalid));
    rig.send(Command::Set(fixed(3000)));
    for wait in [5, 10] {
        rig.clock.advance(Duration::from_secs(wait));
        rig.c.pump();
    }
    assert_eq!(rig.opens(), 3);
    assert_eq!(
        rig.c.state(),
        State::Unavailable(Unavailable::HelperUnreachable)
    );
    rig
}

/// A sleep shows as the wall clock running ahead of the monotonic one. The
/// loop looks at least every tick while a retry is scheduled, so a wake is
/// seen at once; the backoff starts over and the helper is tried then, not
/// at the minute.
#[test]
fn a_sleep_resets_the_backoff_and_retries_at_once() {
    // Early: after the first failure the try is due in 5 s, and a wake 3 s
    // on tries at once.
    let mut rig = rig();
    rig.answer(|request| *request == Request::Version, Err(Fault::Invalid));
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.opens(), 1);
    rig.slept_for(Duration::from_secs(3_600));
    rig.c.pump();
    assert_eq!(rig.opens(), 2, "tried on waking");
    // The backoff starts over at 5 s rather than going on to 10 s.
    rig.clock
        .advance(BACKOFF[0].checked_sub(Duration::from_secs(1)).unwrap());
    rig.c.pump();
    assert_eq!(rig.opens(), 2);
    rig.clock.advance(Duration::from_secs(1));
    rig.c.pump();
    assert_eq!(rig.opens(), 3);

    // Past three, quietly every minute: a sleep tries at once too, and only
    // a real gap counts.
    let answers = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut quiet = silent_three_times(&answers);
    assert!(
        matches!(quiet.c.wait(), Some(wait) if wait <= TICK),
        "the loop looks at the clocks every tick while a try is scheduled"
    );
    quiet.clock.advance(TICK);
    quiet.c.pump();
    assert_eq!(quiet.opens(), 3, "three seconds awake is not a sleep");
    quiet.slept_for(SLEEP_GAP + Duration::from_secs(1));
    quiet.c.pump();
    assert_eq!(
        quiet.opens(),
        4,
        "a sleep longer than the gap tries at once"
    );
    // The backoff starts over: the next try is 5 s on.
    quiet.clock.advance(BACKOFF[0]);
    quiet.c.pump();
    assert_eq!(quiet.opens(), 5);
    // The real wake: the helper answers, and the fans are taken back.
    answers.store(true, std::sync::atomic::Ordering::SeqCst);
    quiet.slept_for(Duration::from_secs(3_600));
    quiet.c.pump();
    assert_eq!(quiet.c.state(), State::Holding);
}

/// Auto never retries anything, asleep or awake, and schedules nothing.
#[test]
fn auto_never_retries_anything() {
    let mut rig = rig_for_unreachable();
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.c.state(), State::Off);
    assert_eq!(rig.c.wait(), None);
    let opens = rig.opens();
    for _ in 0..10 {
        rig.clock.advance(RETRY_EVERY);
        rig.c.pump();
        rig.slept_for(Duration::from_secs(3_600));
        rig.c.pump();
    }
    assert_eq!(rig.opens(), opens);
}

/// A rig whose helper refuses the signature check while `old` is set: the
/// replaced app's helper, still up for its idle grace after an update.
fn refused_signature(old: &Arc<std::sync::atomic::AtomicBool>, fault: Fault) -> Rig {
    let rig = rig();
    let old = Arc::clone(old);
    rig.answer(
        move |request| {
            *request == Request::Version && old.load(std::sync::atomic::Ordering::SeqCst)
        },
        Err(fault),
    );
    rig
}

/// The update race (2026-10-19, 1.2.12 to 1.2.13): the new proxy reached the
/// old helper, which failed the signature check, and nothing tried again.
/// Now one try goes out on its own, after the old helper's idle grace and
/// launchd's restart spacing, and takes the fans with no command sent.
#[test]
fn a_signature_refusal_is_retried_once_after_the_old_helper_is_gone() {
    assert!(
        SIGNATURE_RETRY_AFTER > crate::root::IDLE_EXIT_GRACE + Duration::from_secs(10),
        "past the idle grace plus launchd's 10 s spacing"
    );
    for fault in [Fault::PeerCodeSigningRequirement, Fault::Requirement] {
        let old = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut rig = refused_signature(&old, fault);
        rig.send(Command::Set(fixed(3000)));
        assert_eq!(
            rig.c.state(),
            State::Unavailable(Unavailable::HelperSignature)
        );
        assert_eq!(rig.opens(), 1);
        // A sleep does not bring it forward: the old helper's grace stood
        // still through it too.
        rig.slept_for(Duration::from_secs(3_600));
        rig.c.pump();
        rig.clock.advance(
            SIGNATURE_RETRY_AFTER
                .checked_sub(TICK + Duration::from_secs(1))
                .unwrap(),
        );
        rig.c.pump();
        assert_eq!(rig.opens(), 1, "not before {SIGNATURE_RETRY_AFTER:?}");
        // The old helper exited; launchd starts the new one.
        old.store(false, std::sync::atomic::Ordering::SeqCst);
        rig.clock.advance(Duration::from_secs(1));
        rig.c.pump();
        assert_eq!(rig.opens(), 2, "{fault:?}: tried once on its own");
        assert_eq!(rig.c.state(), State::Holding, "{fault:?}");
    }
}

/// A second refusal is a helper this build really does not trust: no more
/// tries until the next command, asleep or awake, so the connection the XPC
/// layer leaks on a refused requirement is leaked at most twice per command.
#[test]
fn a_second_signature_refusal_stops_for_good() {
    let old = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut rig = refused_signature(&old, Fault::Requirement);
    rig.send(Command::Set(fixed(3000)));
    rig.clock.advance(SIGNATURE_RETRY_AFTER);
    rig.c.pump();
    assert_eq!(rig.opens(), 2);
    assert_eq!(
        rig.c.state(),
        State::Unavailable(Unavailable::HelperSignature)
    );
    assert_eq!(rig.c.wait(), None, "nothing scheduled");
    for _ in 0..20 {
        rig.clock.advance(RETRY_EVERY);
        rig.c.pump();
        rig.slept_for(Duration::from_secs(3_600));
        rig.c.pump();
    }
    assert_eq!(rig.opens(), 2);
}

/// A command starts it over: one try now, one more on its own, then stop.
#[test]
fn a_command_after_the_signature_stop_retries_again() {
    let old = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut rig = refused_signature(&old, Fault::Requirement);
    rig.send(Command::Set(fixed(3000)));
    rig.clock.advance(SIGNATURE_RETRY_AFTER);
    rig.c.pump();
    assert_eq!(rig.opens(), 2);
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.opens(), 3, "the command tries at once");
    rig.clock.advance(SIGNATURE_RETRY_AFTER);
    rig.c.pump();
    assert_eq!(rig.opens(), 4, "and once more on its own");
    rig.clock.advance(SIGNATURE_RETRY_AFTER * 10);
    rig.c.pump();
    assert_eq!(rig.opens(), 4);
    // The helper is the right one now: the next command takes the fans.
    old.store(false, std::sync::atomic::Ordering::SeqCst);
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.c.state(), State::Holding);
}

/// Auto after a signature refusal retries nothing: the scheduled try goes.
#[test]
fn auto_never_retries_a_signature_refusal() {
    let old = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut rig = refused_signature(&old, Fault::PeerCodeSigningRequirement);
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.opens(), 1);
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.c.state(), State::Off);
    assert_eq!(rig.c.wait(), None);
    for _ in 0..10 {
        rig.clock.advance(SIGNATURE_RETRY_AFTER);
        rig.c.pump();
    }
    assert_eq!(rig.opens(), 1);
}

// M1 and M2: connection events.

#[test]
fn interrupted_reapplies() {
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    let first = rig.c.generation();
    rig.fault(first, Fault::Interrupted);
    assert_eq!(rig.c.state(), State::Holding);
    assert!(rig.c.generation() > first);
    assert_eq!(rig.world.lock().requests, entry_requests(3000.0));
}

#[test]
fn stale_generation_event_ignored() {
    let mut rig = holding(fixed(3000));
    let old = rig.c.generation();
    rig.fault(old, Fault::Interrupted);
    let current = rig.c.generation();
    assert!(current > old);
    rig.clear_requests();
    // The old connection's invalidation arrives late.
    for fault in [
        Fault::Invalid,
        Fault::Interrupted,
        Fault::PeerCodeSigningRequirement,
    ] {
        rig.fault(old, fault);
    }
    assert_eq!(rig.c.state(), State::Holding);
    assert!(rig.c.connected());
    assert_eq!(rig.c.generation(), current);
    assert_eq!(rig.world.lock().requests, vec![]);
    // The current one's does count.
    rig.fault(current, Fault::Invalid);
    assert_eq!(
        rig.c.state(),
        State::Unavailable(Unavailable::HelperUnreachable)
    );
}

#[test]
fn wake_with_every_fan_automatic_reenters_and_says_why() {
    let mut rig = holding(fixed(3000));
    // The wall clock jumps an hour while the monotonic one does not.
    rig.next_tick();
    rig.clock.0.lock().unwrap().1 += 3_600_000;
    rig.all_auto();
    rig.clear_requests();
    rig.next_tick();
    assert_eq!(rig.c.state(), State::Holding);
    assert_eq!(rig.c.detail(), None, "the detail clears once holding again");
    assert_eq!(rig.written().len(), 4);
}

/// Each sleep hands the fans back to macOS, and the contention window runs on
/// the monotonic clock, which stands still in sleep: a night of dark wakes
/// would read as "something keeps taking the fans" and stop for good. A
/// re-entry after a sleep is not counted.
#[test]
fn reentries_after_sleep_are_not_contention() {
    let mut rig = holding(fixed(3000));
    for _ in 0..=REENTRY_LIMIT * 3 {
        rig.all_auto();
        rig.slept_for(Duration::from_mins(30));
        rig.c.pump();
        assert_eq!(rig.c.state(), State::Holding);
    }
}

#[test]
fn fans_that_keep_going_automatic_are_contended() {
    let mut rig = holding(fixed(3000));
    for _ in 0..=REENTRY_LIMIT {
        rig.all_auto();
        rig.next_tick();
    }
    assert_eq!(rig.c.state(), State::Unavailable(Unavailable::Contended));
}

#[test]
fn another_tcr_taking_over_stops_without_a_hand_back() {
    let mut rig = holding(follow(60, 90));
    rig.clear_requests();
    rig.answer(
        |request| matches!(request, Request::SetTarget { .. }),
        Ok(Reply::Refused(HelperError::NotHolder)),
    );
    rig.cpu(80.0);
    rig.next_tick();
    assert!(!rig.written().contains(&Request::HandBack));
    assert_eq!(rig.c.detail(), Some(Detail::TakenOver));
    assert_eq!(rig.c.hold(), None);
}

// Restarts.

#[test]
fn a_matching_record_at_boot_is_taken_over_and_resumed() {
    let mut reading = machine();
    for f in &mut reading.fans {
        f.mode = FanMode::Manual;
        f.target_rpm = Some(5000.0);
    }
    let saved = Saved {
        desired: fixed(5000),
        hold: Some(Hold {
            writer_pid: 1,
            fans: (0..2)
                .map(|i| HeldFan {
                    fan: fan(i),
                    written_rpm: Some(5000.0),
                })
                .collect(),
        }),
    };
    let mut rig = rig_from(Some(saved), Some(reading));
    assert_eq!(rig.c.hold().unwrap().writer_pid, 4242);
    rig.c.pump();
    assert_eq!(rig.c.state(), State::Holding);
}

#[test]
fn a_matching_record_with_auto_wanted_is_handed_back_at_boot() {
    let mut reading = machine();
    for f in &mut reading.fans {
        f.mode = FanMode::Manual;
        f.target_rpm = Some(5000.0);
    }
    let saved = Saved {
        desired: FanDesired::Auto,
        hold: Some(Hold {
            writer_pid: 1,
            fans: (0..2)
                .map(|i| HeldFan {
                    fan: fan(i),
                    written_rpm: Some(5000.0),
                })
                .collect(),
        }),
    };
    let mut rig = rig_from(Some(saved), Some(reading));
    // The loop's first pump, with no command and no time passed.
    rig.c.pump();
    assert_eq!(rig.written(), vec![Request::HandBack]);
    assert_eq!(rig.c.state(), State::Off);
}

#[test]
fn a_record_the_fans_no_longer_match_is_dropped_at_boot() {
    let mut reading = machine();
    reading.fans[0].mode = FanMode::Manual;
    reading.fans[0].target_rpm = Some(2222.0);
    let saved = Saved {
        desired: FanDesired::Auto,
        hold: Some(Hold {
            writer_pid: 1,
            fans: vec![HeldFan {
                fan: fan(0),
                written_rpm: Some(5000.0),
            }],
        }),
    };
    let rig = rig_from(Some(saved), Some(reading));
    assert_eq!(rig.c.hold(), None);
    assert_eq!(rig.store.0.lock().unwrap().last().unwrap().hold, None);
}

#[test]
fn shutdown_hands_back_and_keeps_the_wish() {
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    let deadline = rig.clock.now() + Duration::from_secs(3);
    assert_eq!(rig.send(Command::Shutdown { deadline }), Flow::Exit);
    assert_eq!(rig.written(), vec![Request::HandBack]);
    let last = rig.store.0.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.desired, fixed(3000));
    assert_eq!(last.hold, None);
}

#[test]
fn a_failing_hand_back_is_stuck_after_three_tries() {
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.answer(
        |request| *request == Request::HandBack,
        Ok(Reply::Refused(HelperError::HandBackIncomplete {
            fans: 1,
            ftst: false,
        })),
    );
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.c.state(), State::Stuck);
    assert_eq!(rig.written(), vec![Request::HandBack; RELEASE_TRIES]);
    assert!(!rig.c.connected());
}

#[test]
fn no_smc_and_no_fans_are_unavailable() {
    let mut rig = rig_from(None, None);
    assert_eq!(rig.c.state(), State::Unavailable(Unavailable::NoSmc));
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.c.state(), State::Unavailable(Unavailable::NoSmc));
    assert_eq!(rig.world.lock().requests, vec![]);
    let mut rig = rig_from(None, Some(HeatReading::default()));
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.c.state(), State::Unavailable(Unavailable::NoFans));
}

// L3 and L5.

#[test]
fn follow_full_above_guard_refused() {
    assert_eq!(
        FollowCurve::new(60, HotGuard::ON + 1),
        Err(ModeError::FullAboveGuard { full_at: 101 })
    );
    assert!(FollowCurve::new(60, HotGuard::ON).is_ok());
    assert_eq!(
        FollowCurve::new(60, 64),
        Err(ModeError::CurveTooNarrow {
            quiet_below: 60,
            full_at: 64
        })
    );
    assert_eq!(
        FollowCurve::new(19, 90),
        Err(ModeError::QuietTooLow { quiet_below: 19 })
    );
    assert_eq!(Rpm::new(0), Err(ModeError::RpmOutOfRange { rpm: 0 }));
    assert_eq!(
        Rpm::new(MAX_RPM + 1),
        Err(ModeError::RpmOutOfRange { rpm: MAX_RPM + 1 })
    );
}

/// Each way through a tick, and that each left an outcome behind.
#[test]
fn every_tick_path_has_an_outcome() {
    let mut seen = Vec::new();

    let mut rig = holding(follow(60, 90));
    seen.push(rig.next_tick());
    rig.cpu(90.0);
    seen.push(rig.next_tick());
    rig.macs(Err("probe".into()));
    seen.push(rig.next_tick());
    rig.macs(Ok(true));
    seen.push(rig.next_tick());
    rig.macs(Ok(false));
    rig.world.lock().fan_mut(fan(0)).unwrap().mode = FanMode::Unreadable;
    seen.push(rig.next_tick());
    rig.world.lock().fan_mut(fan(0)).unwrap().mode = FanMode::Manual;
    rig.world.lock().fan_mut(fan(0)).unwrap().max_rpm = 0.0;
    seen.push(rig.next_tick());
    rig.world.lock().fan_mut(fan(0)).unwrap().max_rpm = 5777.0;
    rig.set_target(1, 1.0);
    seen.push(rig.next_tick());
    let reading = rig.world.lock().reading.take();
    seen.push(rig.next_tick());
    rig.world.lock().reading = reading;

    let mut rig = holding(fixed(3000));
    rig.all_auto();
    seen.push(rig.next_tick());
    rig.answer(
        |request| matches!(request, Request::SetTarget { .. }),
        Err(Fault::NoReply),
    );
    rig.send(Command::Set(fixed(3500)));
    seen.push(rig.c.last_outcome());

    assert!(seen.iter().all(Option::is_some), "{seen:?}");
    let seen: Vec<TickOutcome> = seen.into_iter().flatten().collect();
    for expected in [
        TickOutcome::NoChange,
        TickOutcome::Wrote(2),
        TickOutcome::HeldBack(Why::ProbeFailed),
        TickOutcome::HeldBack(Why::MacsFanControl),
        TickOutcome::HeldBack(Why::Unreadable),
        TickOutcome::HeldBack(Why::LimitsUnknown),
        TickOutcome::HeldBack(Why::AnotherApp),
        TickOutcome::HeldBack(Why::Reentering),
        TickOutcome::HeldBack(Why::Helper),
    ] {
        assert!(seen.contains(&expected), "{expected:?} not in {seen:?}");
    }
}

#[test]
fn auto_with_nothing_held_writes_nothing_and_does_not_tick() {
    let mut rig = rig();
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.c.state(), State::Off);
    assert_eq!(rig.world.lock().requests, vec![]);
    assert_eq!(rig.c.wait(), None, "no timer while off");
}

// Review fixes.

impl Rig {
    fn take_logs(&self) -> Vec<(Level, String)> {
        std::mem::take(&mut *self.logs.lock().unwrap())
    }
}

/// While Macs Fan Control runs, a release that waits on it (or on a failed
/// probe) says so once, never again on each tick; and with nothing held it
/// does not wait at all.
#[test]
fn a_waiting_release_logs_once_not_every_tick() {
    // Something held: Auto waits, quietly.
    let mut rig = holding(fixed(3000));
    rig.macs(Ok(true));
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::MacsFanControl));
    assert!(!rig.take_logs().is_empty(), "the change itself is said");
    for _ in 0..5 {
        rig.next_tick();
    }
    assert_eq!(rig.take_logs(), vec![], "nothing more while it waits");
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::MacsFanControl));
    assert_eq!(rig.c.detail(), Some(Detail::WaitingForMacs));

    // A forced hand-back waits the same way.
    let mut rig = holding(fixed(3000));
    rig.macs(Ok(true));
    rig.send(Command::ForceHandBack);
    rig.take_logs();
    for _ in 0..5 {
        rig.next_tick();
    }
    assert_eq!(rig.take_logs(), vec![]);

    // A probe that keeps failing: one warning, not one per tick.
    let mut rig = holding(fixed(3000));
    rig.macs(Err("proc_listallpids failed".into()));
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.c.state(), State::Unknown);
    let warnings = |logs: &[(Level, String)]| logs.iter().filter(|l| l.0 == Level::Warn).count();
    assert_eq!(warnings(&rig.take_logs()), 1);
    for _ in 0..5 {
        rig.next_tick();
    }
    assert_eq!(rig.take_logs(), vec![]);

    // Nothing held and no connection: Auto is simply off, no tick.
    let mut rig = rig_from(None, Some(machine()));
    rig.macs(Ok(true));
    rig.send(Command::Set(fixed(3000)));
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::MacsFanControl));
    assert_eq!(rig.c.hold(), None);
    rig.send(Command::Set(FanDesired::Auto));
    assert_eq!(rig.c.state(), State::Off);
    assert_eq!(rig.c.wait(), None, "no timer while off");
    rig.take_logs();
    for _ in 0..5 {
        rig.next_tick();
    }
    assert_eq!(rig.take_logs(), vec![]);
    assert_eq!(rig.world.lock().requests, vec![]);
}

/// Macs Fan Control starting (or its probe failing) after the first
/// set-manual: the fan set manual goes back before the state says why, and
/// no record is left behind.
#[test]
fn a_block_after_the_first_set_manual_hands_back_first() {
    for (block, state) in [
        (Ok(true), State::Foreign(ForeignHolder::MacsFanControl)),
        (Err("probe".to_string()), State::Unknown),
    ] {
        // Between the two set-manuals.
        let mut rig = rig();
        let during = block.clone();
        rig.hook(Box::new(move |request, machine, _inject, _clock| {
            if *request == set_manual(0) {
                machine.macs = during.clone();
            }
            None
        }));
        rig.send(Command::Set(fixed(3000)));
        assert_eq!(rig.written(), vec![set_manual(0), Request::HandBack]);
        assert_eq!(rig.c.state(), state);
        assert_eq!(rig.c.hold(), None);
        assert!(!rig.c.connected());

        // Between two set-targets.
        let mut rig = rig_from(None, Some(machine()));
        let during = block.clone();
        rig.hook(Box::new(move |request, machine, _inject, _clock| {
            if *request == set_target(0, 3000.0) {
                machine.macs = during.clone();
            }
            None
        }));
        rig.send(Command::Set(fixed(3000)));
        assert_eq!(
            rig.written(),
            vec![
                set_manual(0),
                set_manual(1),
                set_target(0, 3000.0),
                Request::HandBack
            ]
        );
        assert_eq!(rig.c.state(), state);
        assert_eq!(rig.c.hold(), None);
    }
}

/// A record with no target written is ours only inside the entry that wrote
/// it: at boot, fans in manual against such a record are not taken over.
#[test]
fn a_record_without_a_target_is_not_ours_after_entry() {
    let mut reading = machine();
    for f in &mut reading.fans {
        f.mode = FanMode::Manual;
        f.target_rpm = Some(2000.0);
    }
    let saved = Saved {
        desired: fixed(5000),
        hold: Some(Hold {
            writer_pid: 1,
            fans: (0..2)
                .map(|i| HeldFan {
                    fan: fan(i),
                    written_rpm: None,
                })
                .collect(),
        }),
    };
    let mut rig = rig_from(Some(saved), Some(reading));
    assert_eq!(rig.c.hold(), None);
    rig.c.pump();
    assert_eq!(rig.c.state(), State::Foreign(ForeignHolder::AnotherApp));
    assert_eq!(rig.written(), vec![]);
}

/// Boot names its own rule for a fan with no target yet: a state set before
/// boot (here mid-entry, where such a fan counts as ours) does not change it.
#[test]
fn boot_judges_an_unwritten_fan_not_ours_whatever_the_state() {
    let mut reading = machine();
    for f in &mut reading.fans {
        f.mode = FanMode::Manual;
        f.target_rpm = Some(2000.0);
    }
    let hold = Hold {
        writer_pid: 1,
        fans: (0..2)
            .map(|i| HeldFan {
                fan: fan(i),
                written_rpm: None,
            })
            .collect(),
    };
    let mut rig = rig_from(None, Some(reading));
    rig.c.hold = Some(hold);
    rig.c.state = State::Entering;
    rig.c.boot();
    assert_eq!(rig.c.hold(), None);
}

/// One held fan back on automatic while the other keeps our target: taken
/// again (counted), never left reading "holding".
#[test]
fn a_held_fan_back_on_automatic_alone_is_retaken() {
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.world.lock().fan_mut(fan(1)).unwrap().mode = FanMode::Auto;
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::Reentering))
    );
    assert_eq!(rig.written(), entry(3000.0));
    assert_eq!(rig.c.state(), State::Holding);
    assert_eq!(
        rig.world.lock().fan_mut(fan(1)).unwrap().mode,
        FanMode::Manual
    );
    // And it counts toward contention like every fan going back.
    for _ in 0..REENTRY_LIMIT {
        rig.world.lock().fan_mut(fan(1)).unwrap().mode = FanMode::Auto;
        rig.next_tick();
    }
    assert_eq!(rig.c.state(), State::Unavailable(Unavailable::Contended));
}

/// `FNum` that did not read gives no fans at all; a fan that did not read is
/// left out. Either, while holding, is unknown: no write, the record kept.
#[test]
fn an_empty_or_partial_reading_while_holding_is_unknown() {
    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    let full = rig.world.lock().reading.clone();
    rig.world.lock().reading.as_mut().unwrap().fans.clear();
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::Unreadable))
    );
    assert_eq!(rig.c.state(), State::Unknown);
    // One fan read, automatic, the other missing.
    {
        let mut machine = rig.world.lock();
        let mut partial = full.clone().unwrap();
        partial.fans.truncate(1);
        partial.fans[0].mode = FanMode::Auto;
        machine.reading = Some(partial);
    }
    assert_eq!(
        rig.next_tick(),
        Some(TickOutcome::HeldBack(Why::Unreadable))
    );
    assert_eq!(rig.c.state(), State::Unknown);
    assert_eq!(rig.written(), vec![]);
    assert!(rig.c.hold().is_some(), "no hold dropped");
    // Whole again: still ours.
    rig.world.lock().reading = full;
    assert_eq!(rig.next_tick(), Some(TickOutcome::NoChange));
    assert_eq!(rig.c.state(), State::Holding);
}

/// A shutdown drained between two set-manuals is acted on in the same pump:
/// the loop never goes back to wait on a channel whose stop it took.
#[test]
fn a_shutdown_during_entry_is_acted_on() {
    let mut rig = rig();
    let mut sent = false;
    rig.hook(Box::new(move |request, _machine, inject, clock| {
        if *request == set_manual(0) && !sent {
            inject.command(Command::Shutdown {
                deadline: clock.now() + Duration::from_secs(3),
            });
            sent = true;
        }
        None
    }));
    assert_eq!(rig.send(Command::Set(fixed(3000))), Flow::Exit);
    assert_eq!(rig.written(), vec![set_manual(0), Request::HandBack]);
    assert!(!rig.c.connected());
    let last = rig.store.0.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        last.desired,
        fixed(3000),
        "the wish stays for the next start"
    );
    assert_eq!(last.hold, None);
}

/// A shutdown looks before it writes, as a release does: a failed probe is
/// not "Macs Fan Control is not running", and fans another program changed
/// are not handed back.
#[test]
fn a_shutdown_checks_as_a_release_does() {
    let deadline = |rig: &Rig| rig.clock.now() + Duration::from_secs(3);

    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.macs(Err("probe".into()));
    let at = deadline(&rig);
    assert_eq!(rig.send(Command::Shutdown { deadline: at }), Flow::Exit);
    assert_eq!(rig.written(), vec![]);
    assert!(!rig.c.connected());

    let mut rig = holding(fixed(3000));
    rig.clear_requests();
    rig.set_target(0, 4200.0);
    let at = deadline(&rig);
    assert_eq!(rig.send(Command::Shutdown { deadline: at }), Flow::Exit);
    assert_eq!(rig.written(), vec![]);
    assert_eq!(rig.c.hold(), None);
}

/// A forced hand-back that waited on Macs Fan Control is for the wish a new
/// mode replaced: once it quits, the new mode holds and nothing is handed
/// back.
#[test]
fn a_new_mode_drops_a_waiting_hand_back() {
    let mut rig = holding(fixed(3000));
    rig.macs(Ok(true));
    rig.send(Command::ForceHandBack);
    rig.send(Command::Set(fixed(3000)));
    rig.clear_requests();
    rig.macs(Ok(false));
    rig.next_tick();
    rig.next_tick();
    assert!(
        !rig.written().contains(&Request::HandBack),
        "{:?}",
        rig.written()
    );
    assert_eq!(rig.c.state(), State::Holding);
}

// When a command is acted on: the number it was sent under against the one
// the snapshot shows.

/// What `tcr fan` (`fan_wait` in the binary's `src/cli.rs`) reads off a
/// snapshot for command number `mine`: settled once the controller acted on
/// it or a later one, and the state is not on its way somewhere.
fn reads_settled(snapshot: &Snapshot, mine: u64) -> bool {
    snapshot.command >= mine && !matches!(snapshot.state, State::Entering | State::Releasing)
}

impl Rig {
    /// Forget what was published, send `command`, pump, and return the
    /// command's number.
    fn numbered(&mut self, command: Command) -> u64 {
        self.published.lock().unwrap().clear();
        let mine = self.inbox.command(command);
        self.c.pump();
        mine
    }

    /// The first snapshot since [`Self::numbered`] that `tcr fan` would stop
    /// on.
    fn first_settled(&self, mine: u64) -> Option<Snapshot> {
        self.published
            .lock()
            .unwrap()
            .iter()
            .find(|s| reads_settled(s, mine))
            .cloned()
    }
}

/// The sequence Gil's Mac showed: holding at 5000, `auto` sent. The snapshot
/// published as the command is taken still says holding; `tcr fan auto` must
/// read it as not yet acted on and stop only on `off`.
#[test]
fn auto_from_holding_settles_only_on_off() {
    let mut rig = holding(fixed(5000));
    let mine = rig.numbered(Command::Set(FanDesired::Auto));
    let first = rig.published.lock().unwrap().first().cloned().unwrap();
    assert_eq!(first.state, State::Holding, "the taking publishes first");
    assert!(
        !reads_settled(&first, mine),
        "a snapshot from before the release is not the command's answer: {first:?}"
    );
    let settled = rig.first_settled(mine).unwrap();
    assert_eq!(settled.state, State::Off, "{settled:?}");
}

/// A changed Fixed while holding: settled only once the new target is written.
#[test]
fn a_changed_fixed_settles_after_the_write() {
    let mut rig = holding(fixed(5000));
    rig.clear_requests();
    let mine = rig.numbered(Command::Set(fixed(4000)));
    assert_eq!(rig.written(), targets(4000.0));
    let settled = rig.first_settled(mine).unwrap();
    assert_eq!(settled.state, State::Holding);
    assert_eq!(
        settled.targets,
        vec![(fan(0), 4000.0), (fan(1), 4000.0)],
        "settled before the write: {settled:?}"
    );
}

/// The same Fixed again changes nothing on the fans, and still settles: the
/// controller looked and found nothing to do.
#[test]
fn a_repeated_fixed_settles_with_nothing_to_write() {
    let mut rig = holding(fixed(5000));
    rig.clear_requests();
    let mine = rig.numbered(Command::Set(fixed(5000)));
    assert_eq!(rig.written(), vec![], "nothing to write");
    let settled = rig.first_settled(mine).unwrap();
    assert_eq!(settled.state, State::Holding);
    assert_eq!(settled.targets, vec![(fan(0), 5000.0), (fan(1), 5000.0)]);
}

/// A command that arrives in the middle of an entry is taken between two
/// writes, after an earlier fan already got the old target. The entry ending
/// in `holding` is not that command's answer; the round that writes every fan
/// for it is, and it runs at once, not a tick later.
#[test]
fn a_command_taken_mid_entry_settles_after_its_own_round() {
    let mut rig = rig();
    let sent = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&sent);
    rig.hook(Box::new(move |request, _machine, inject, _clock| {
        if *request == set_target(0, 5000.0) && seen.lock().unwrap().is_none() {
            *seen.lock().unwrap() = Some(inject.command(Command::Set(fixed(4000))));
        }
        None
    }));
    rig.send(Command::Set(fixed(5000)));
    let mine = sent.lock().unwrap().unwrap();
    assert_eq!(rig.c.state(), State::Holding);
    assert!(
        rig.c.snapshot().command < mine,
        "the entry for the command before is not this one's answer"
    );
    assert_eq!(
        rig.c.wait(),
        Some(Duration::ZERO),
        "a command taken mid-entry is acted on at once"
    );
    rig.c.pump();
    let settled = rig.first_settled(mine).unwrap();
    assert_eq!(
        settled.targets,
        vec![(fan(0), 4000.0), (fan(1), 4000.0)],
        "settled while fan 0 still held the old target: {settled:?}"
    );
}

#[test]
fn reviewer_control_stopped_predecessor_preserves_successor_auto() {
    let mut old = holding(fixed(5000));
    old.send(Command::Shutdown {
        deadline: old.clock.now() + Duration::from_secs(3),
    });
    old.clear_requests();
    // The successor has handed every fan back and root now has no holder.
    // These are the same hardware reads root's real HandBack produces.
    for fan in &mut old.world.lock().reading.as_mut().unwrap().fans {
        fan.mode = FanMode::Auto;
    }
    old.next_tick();
    assert!(
        old.written().is_empty(),
        "predecessor issued new fan writes after successor Auto: {:?}",
        old.written()
    );
}

/// Two commands drained in one pump are acted on in one step, under the later
/// number: the earlier one reads as acted on, and the wish shown is the later
/// one's, which is what tells the earlier command it was replaced.
#[test]
fn a_drained_pair_is_acted_on_under_the_later_number() {
    let mut rig = holding(fixed(5000));
    let first = rig.inbox.command(Command::Set(fixed(4000)));
    let second = rig.inbox.command(Command::Set(FanDesired::Auto));
    rig.c.pump();
    let shown = rig.c.snapshot();
    assert_eq!(shown.command, second);
    assert!(shown.command > first);
    assert_eq!((shown.desired, shown.state), (FanDesired::Auto, State::Off));
}

/// A new process starts its numbers at `commands_before`, and shows them as
/// acted on: a command sent to the process before it reads as acted on.
#[test]
fn a_new_controller_shows_its_first_number_as_acted_on() {
    let rig = rig();
    assert_eq!(rig.c.snapshot().command, COMMANDS_BEFORE);
}
