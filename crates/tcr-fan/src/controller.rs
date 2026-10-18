//! The proxy's fan controller: holds Auto, Fixed or Follow CPU through the root
//! helper, judging on every step whose the fans are.
//!
//! Pure and synchronous. It runs on one std thread in the proxy (`src/fan.rs`,
//! never a tokio worker) and is generic over everything that touches the
//! machine: the helper connection ([`Helper`]), the heat reading ([`Heat`]),
//! the Macs Fan Control probe ([`Probe`]), the clock ([`Clock`]) and the
//! record on disk ([`Store`]). Tests run every path on Linux with fakes.
//!
//! # One input channel
//!
//! Commands from the route and connection events from XPC arrive on one
//! channel as [`Input`]. Helper calls are synchronous with a deadline
//! ([`Helper::send`]), which is safe because this thread does nothing else;
//! between every step of entering manual the controller drains the channel,
//! so a mode change made during the 3 s unlock is seen before the next write
//! (requirement H2), and the latest wish wins.
//!
//! # Whose fans
//!
//! Never a remembered flag. Every tick reads the fans, probes for Macs Fan
//! Control, and asks [`holder::classify`] against the record of what was
//! written (requirement H1): a target another program moved is not ours, a
//! read that failed is unknown, and neither is written to. The probe runs
//! again right before every write.
//!
//! # Generations
//!
//! Each connection gets a [`Generation`]. An event carries the generation of
//! the connection it came from, and one that is not the current connection's
//! is dropped (requirement M2), so a late invalidation from a connection
//! already replaced, or from a client already dropped, never touches the new
//! one.

use std::collections::VecDeque;
use std::fmt;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use tcr_smc::{FanIndex, FanReading, HeatReading};

use crate::holder::{self, FanState, HeldFan, Holder, StillOurs};
use crate::wire::{HelperError, PROTOCOL_VERSION, Reply, Request};

/// How often a manual mode is re-checked and re-targeted. A starting value,
/// not a measured one.
pub const TICK: Duration = Duration::from_secs(3);
/// The version call's deadline.
pub const VERSION_DEADLINE: Duration = Duration::from_secs(2);
/// set-manual's deadline: root may wait 3 s for the unlock to settle and then
/// try the mode write 50 times 100 ms apart, and a launchd cold start comes on
/// top. Not to be shortened.
pub const SET_MANUAL_DEADLINE: Duration = Duration::from_secs(15);
/// set-target's deadline.
pub const SET_TARGET_DEADLINE: Duration = Duration::from_secs(2);
/// hand-back's deadline.
pub const HAND_BACK_DEADLINE: Duration = Duration::from_secs(5);
/// How many hand-backs a release tries, [`RELEASE_GAP`] apart.
pub const RELEASE_TRIES: usize = 3;
pub const RELEASE_GAP: Duration = Duration::from_secs(1);
/// Entries that fail in a row before the automatic retry stops until the
/// next command or restart, so a fan that will not unlock is not put through
/// the `Ftst` dance every minute.
pub const ENTRY_FAILURES_BEFORE_STOP: u32 = 3;
/// More than this many re-entries inside [`REENTRY_WINDOW`] is contention:
/// something keeps taking the fans back.
pub const REENTRY_LIMIT: usize = 3;
pub const REENTRY_WINDOW: Duration = Duration::from_secs(600);
/// The reconnect waits while the helper cannot be reached; the last repeats.
pub const BACKOFF: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(40),
    Duration::from_secs(60),
];
/// In Follow CPU a new target is written only when it moved at least this
/// share of the fan's range (or reached an end, or the hot guard flipped). A
/// starting value, not a measured one.
pub const WRITE_STEP: f64 = 0.05;
/// A tick whose wall clock moved this much further than its monotonic clock
/// slept in between: the log line names the cause "after sleep".
pub const SLEEP_GAP: Duration = Duration::from_secs(10);
/// The highest rpm a Fixed mode may ask for. Root clamps to each fan's own
/// range; this only refuses nonsense at the edge.
pub const MAX_RPM: u32 = 20_000;
/// The lowest temperature a Follow CPU curve may start at, °C.
pub const MIN_QUIET_C: u8 = 20;
/// The narrowest a Follow CPU curve may be, °C.
pub const MIN_CURVE_SPAN_C: u8 = 5;

/// The hot guard: in any manual mode, full speed from [`Self::ON`] until the
/// CPU cools below [`Self::OFF`]. Unmeasured starting values; the rule for
/// setting them from hardware is check H12.
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
pub enum FanMode {
    /// macOS has them.
    Auto,
    Fixed {
        rpm: Rpm,
    },
    FollowCpu {
        curve: FollowCurve,
    },
}

impl FanMode {
    pub const fn is_manual(self) -> bool {
        !matches!(self, Self::Auto)
    }
}

/// A command from the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Set(FanMode),
    /// `tcr fan auto --force`: hand back without checking the record (never
    /// without checking for Macs Fan Control). The operator's recovery for a
    /// hold no process remembers.
    ForceHandBack,
    /// The proxy is stopping: hand back what is ours by `deadline`, then
    /// stop. The wish on disk is left as it is, so the next start re-applies.
    Shutdown {
        deadline: Instant,
    },
}

/// Who else has the fans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignHolder {
    MacsFanControl,
    AnotherApp,
}

/// Why no manual mode can be held now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// This tcr is not signed as either fan identity, so it never dials.
    NotSignedForFans,
    /// No AppleSMC: a VM, or not a Mac.
    NoSmc,
    /// `FNum` 0.
    NoFans,
    /// Not registered, awaiting approval, or not answering on its service.
    HelperUnreachable,
    /// A call ran past its deadline.
    HelperNoReply,
    /// What answered is not the helper this build trusts, or the requirement
    /// itself was refused. Never retried automatically.
    HelperSignature,
    /// The helper speaks another protocol; `None` when its reply was not the
    /// contract's shape at all.
    HelperVersion {
        installed: Option<u64>,
    },
    /// This fan would not leave automatic.
    ModeLocked {
        fan: u64,
    },
    /// Something keeps taking the fans back.
    Contended,
    HelperRefused(HelperError),
}

impl Unavailable {
    /// Whether the backoff retries this on its own.
    const fn retries(self) -> bool {
        matches!(
            self,
            Self::HelperUnreachable
                | Self::HelperNoReply
                | Self::ModeLocked { .. }
                | Self::HelperRefused(
                    HelperError::SmcRefused { .. }
                        | HelperError::HandBackIncomplete { .. }
                        | HelperError::LimitsUnreadable { .. }
                )
        )
    }
}

/// The controller's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Auto wanted, nothing held.
    Off,
    Entering,
    Holding,
    Releasing,
    Foreign(ForeignHolder),
    /// A read failed: write nothing until one succeeds.
    Unknown,
    Unavailable(Unavailable),
    /// Hand-back failed after its tries; root's disconnect window took over.
    Stuck,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::Entering => write!(f, "entering"),
            Self::Holding => write!(f, "holding"),
            Self::Releasing => write!(f, "releasing"),
            Self::Foreign(ForeignHolder::MacsFanControl) => write!(f, "foreign (Macs Fan Control)"),
            Self::Foreign(ForeignHolder::AnotherApp) => write!(f, "foreign (another app)"),
            Self::Unknown => write!(f, "unknown"),
            Self::Unavailable(why) => write!(f, "unavailable ({why:?})"),
            Self::Stuck => write!(f, "stuck"),
        }
    }
}

/// A line beside the state for the person reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Auto is wanted and Macs Fan Control runs: nothing is written until it
    /// quits.
    WaitingForMacs,
    /// The fans went back to automatic across a sleep and are being taken
    /// again.
    AfterSleep,
    /// Three entries in a row failed; no automatic retry until the next
    /// command or restart.
    RetryStopped,
    /// Another tcr's set-manual made it the holder.
    TakenOver,
}

impl fmt::Display for Detail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WaitingForMacs => {
                write!(f, "fans go back to automatic when Macs Fan Control quits")
            }
            Self::AfterSleep => write!(f, "re-applying after sleep"),
            Self::RetryStopped => write!(f, "three tries failed; send the mode again to retry"),
            Self::TakenOver => write!(f, "another tcr took the fans"),
        }
    }
}

/// Why a tick wrote nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    MacsFanControl,
    AnotherApp,
    /// The SMC read failed or a key did not read.
    Unreadable,
    /// The Macs Fan Control probe failed.
    ProbeFailed,
    /// The fans were not ours to tick (all automatic): re-entering.
    Reentering,
    /// A helper call failed.
    Helper,
    /// A fan's range did not read, so no target could be worked out.
    LimitsUnknown,
}

/// What one tick did. Every path through [`Controller::tick`] ends in one
/// (requirement L5: no silent early return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    Wrote(u8),
    NoChange,
    HeldBack(Why),
}

/// A connection-level failure, as the controller sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// This binary has no fan identity; nothing was dialled.
    NotSignedForFans,
    /// `XPC_ERROR_CONNECTION_INVALID`, or no object to dial with.
    Invalid,
    /// `XPC_ERROR_CONNECTION_INTERRUPTED`.
    Interrupted,
    /// `XPC_ERROR_PEER_CODE_SIGNING_REQUIREMENT`.
    PeerCodeSigningRequirement,
    /// Setting the requirement on the connection was refused.
    Requirement,
    /// No reply within the deadline.
    NoReply,
    /// A reply that is not the contract's shape.
    Malformed,
}

impl Fault {
    const fn unavailable(self) -> Unavailable {
        match self {
            Self::NotSignedForFans => Unavailable::NotSignedForFans,
            Self::Invalid | Self::Interrupted => Unavailable::HelperUnreachable,
            Self::PeerCodeSigningRequirement | Self::Requirement => Unavailable::HelperSignature,
            Self::NoReply => Unavailable::HelperNoReply,
            Self::Malformed => Unavailable::HelperVersion { installed: None },
        }
    }
}

/// Which connection an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u64);

/// One thing on the controller's channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Command(Command),
    /// A connection error delivered by XPC, tagged with its connection.
    Xpc {
        generation: Generation,
        fault: Fault,
    },
}

/// The root helper, one connection at a time.
pub trait Helper {
    /// Open a connection numbered `generation`; events it delivers later carry
    /// that number. Replaces any open one.
    fn open(&mut self, generation: Generation) -> Result<(), Fault>;
    /// One request, waited on for at most `deadline`.
    fn send(&mut self, request: &Request, deadline: Duration) -> Result<Reply, Fault>;
    /// Close the connection. Root sees the disconnect and, when the fans are
    /// still what it wrote, hands them back.
    fn close(&mut self);
}

/// The machine's fans and CPU temperature, read now.
pub trait Heat {
    /// `None` when there is no AppleSMC.
    fn read(&mut self) -> Option<HeatReading>;
}

/// Is Macs Fan Control running?
pub trait Probe {
    fn macs_running(&mut self) -> Result<bool, String>;
}

pub trait Clock {
    fn now(&self) -> Instant;
    /// Milliseconds since the Unix epoch.
    fn wall_ms(&self) -> i64;
    fn sleep(&self, duration: Duration);
}

/// The fans this process put in manual and what it wrote, with its pid.
#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub writer_pid: u32,
    pub fans: Vec<HeldFan>,
}

/// What persists across a restart: the wish and the hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub desired: FanMode,
    pub hold: Option<Hold>,
}

/// Where [`Saved`] goes. An implementation reports its own write failures.
pub trait Store {
    fn save(&mut self, saved: &Saved);
}

/// How loud a line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
}

pub type Log = Box<dyn FnMut(Level, &str) + Send>;

/// What the status route shows, minus the wish (which the route's side
/// publishes as it sends, so a status read never shows an older one).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub state: State,
    pub hot_guard: bool,
    /// What was last written to each fan held.
    pub targets: Vec<(FanIndex, f64)>,
    pub detail: Option<Detail>,
    /// When the state, or the wish it serves, last changed.
    pub since_ms: i64,
}

pub type Publish = Box<dyn FnMut(&Snapshot) + Send>;

/// Whether the loop goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Exit,
}

/// Everything the controller is built from.
pub struct Parts<H, R, P, C, S> {
    pub helper: H,
    pub heat: R,
    pub probe: P,
    pub clock: C,
    pub store: S,
    pub inbox: Receiver<Input>,
    pub log: Log,
    pub publish: Publish,
    /// This process, for the hold record.
    pub pid: u32,
}

/// How an attempt to enter ended, when it did not end holding.
enum Stop {
    /// The wish changed (or a stop arrived) between steps: release now.
    Changed,
    /// The state was set; nothing more to do this round.
    Settled,
}

pub struct Controller<H, R, P, C, S> {
    parts: Parts<H, R, P, C, S>,
    desired: FanMode,
    hold: Option<Hold>,
    state: State,
    generation: u64,
    connected: bool,
    hot: bool,
    detail: Option<Detail>,
    since_ms: i64,
    failed_entries: u32,
    backoff_step: usize,
    retry_at: Option<Instant>,
    next_tick: Option<Instant>,
    reentries: VecDeque<Instant>,
    /// A hand-back is wanted and waits on Macs Fan Control or a read.
    release_pending: bool,
    force_pending: bool,
    /// A command arrived since the last [`Self::advance`].
    fresh: bool,
    stopping: Option<Instant>,
    last_tick: Option<(Instant, i64)>,
    last_outcome: Option<TickOutcome>,
}

impl<H: Helper, R: Heat, P: Probe, C: Clock, S: Store> Controller<H, R, P, C, S> {
    /// A controller starting from `saved` (`None`: Auto, no hold). Reads the
    /// fans once to reconcile a hold from an earlier process: one that still
    /// matches becomes this process's; one the fans no longer match is
    /// dropped.
    pub fn new(parts: Parts<H, R, P, C, S>, saved: Option<Saved>) -> Self {
        let since_ms = parts.clock.wall_ms();
        let (desired, hold) = saved.map_or((FanMode::Auto, None), |s| (s.desired, s.hold));
        let mut controller = Self {
            parts,
            desired,
            hold,
            state: State::Off,
            generation: 0,
            connected: false,
            hot: false,
            detail: None,
            since_ms,
            failed_entries: 0,
            backoff_step: 0,
            retry_at: None,
            next_tick: None,
            reentries: VecDeque::new(),
            release_pending: false,
            force_pending: false,
            fresh: false,
            stopping: None,
            last_tick: None,
            last_outcome: None,
        };
        controller.boot();
        controller
    }

    fn boot(&mut self) {
        let Some(reading) = self.parts.heat.read() else {
            self.set_state(State::Unavailable(Unavailable::NoSmc));
            return;
        };
        if let Some(hold) = &self.hold {
            match holder::classify(&states(&reading), Some(&hold.fans), false) {
                Holder::Ours => {
                    self.log(
                        Level::Info,
                        "fan control: the fans still carry the targets the last run wrote; taking them over",
                    );
                    let pid = self.parts.pid;
                    if let Some(hold) = &mut self.hold {
                        hold.writer_pid = pid;
                    }
                    self.save();
                }
                Holder::MacOs | Holder::AnotherApp | Holder::MacsFanControl => {
                    self.hold = None;
                    self.save();
                }
                Holder::Unknown => {}
            }
        }
        if !self.desired.is_manual() && self.hold.is_some() {
            // Due at once: the first [`Self::pump`] hands back.
            self.release_pending = true;
            self.next_tick = Some(self.parts.clock.now());
        }
        self.publish();
    }

    // Accessors, for the thread and for tests.

    pub const fn state(&self) -> State {
        self.state
    }

    pub const fn desired(&self) -> FanMode {
        self.desired
    }

    pub const fn generation(&self) -> Generation {
        Generation(self.generation)
    }

    pub const fn connected(&self) -> bool {
        self.connected
    }

    pub const fn hold(&self) -> Option<&Hold> {
        self.hold.as_ref()
    }

    pub const fn last_outcome(&self) -> Option<TickOutcome> {
        self.last_outcome
    }

    pub const fn detail(&self) -> Option<Detail> {
        self.detail
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            state: self.state,
            hot_guard: self.hot,
            targets: self
                .hold
                .iter()
                .flat_map(|h| h.fans.iter())
                .filter_map(|f| f.written_rpm.map(|rpm| (f.fan, rpm)))
                .collect(),
            detail: self.detail,
            since_ms: self.since_ms,
        }
    }

    // The loop.

    /// Run until a [`Command::Shutdown`] is done or the channel closes.
    pub fn run(mut self) {
        // What boot left due: a manual wish to enter, a hold to hand back.
        if self.pump() == Flow::Exit {
            return;
        }
        loop {
            let input = match self.wait() {
                Some(wait) => match self.parts.inbox.recv_timeout(wait) {
                    Ok(input) => Some(input),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => Some(self.closed_channel()),
                },
                None => Some(
                    self.parts
                        .inbox
                        .recv()
                        .unwrap_or_else(|_closed| self.closed_channel()),
                ),
            };
            if let Some(input) = input {
                self.accept(input);
            }
            if self.pump() == Flow::Exit {
                return;
            }
        }
    }

    /// Nobody can send another command: stop as a shutdown would.
    fn closed_channel(&self) -> Input {
        Input::Command(Command::Shutdown {
            deadline: self.parts.clock.now() + Duration::from_secs(3),
        })
    }

    /// How long the loop may wait for input before it has work of its own.
    fn wait(&self) -> Option<Duration> {
        let now = self.parts.clock.now();
        [self.next_tick, self.retry_at]
            .into_iter()
            .flatten()
            .min()
            .map(|at| at.saturating_duration_since(now))
    }

    /// Drain the channel, then do whatever is due. What a loop iteration does
    /// after its wait; tests call it directly.
    pub fn pump(&mut self) -> Flow {
        self.drain();
        self.advance()
    }

    /// Take everything queued, latest wish last.
    fn drain(&mut self) {
        loop {
            match self.parts.inbox.try_recv() {
                Ok(input) => self.accept(input),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Take one input.
    pub fn accept(&mut self, input: Input) {
        match input {
            Input::Command(Command::Set(mode)) => {
                self.desired = mode;
                self.fresh = true;
                self.failed_entries = 0;
                self.backoff_step = 0;
                self.retry_at = None;
                self.reentries.clear();
                self.since_ms = self.parts.clock.wall_ms();
                self.save();
                self.publish();
            }
            Input::Command(Command::ForceHandBack) => {
                self.desired = FanMode::Auto;
                self.force_pending = true;
                self.fresh = true;
                self.since_ms = self.parts.clock.wall_ms();
                self.save();
                self.publish();
            }
            Input::Command(Command::Shutdown { deadline }) => self.stopping = Some(deadline),
            Input::Xpc { generation, fault } => self.on_xpc(generation, fault),
        }
    }

    fn on_xpc(&mut self, generation: Generation, fault: Fault) {
        if generation != Generation(self.generation) || !self.connected {
            self.log(
                Level::Debug,
                &format!(
                    "fan control: ignored {fault:?} from connection {} (current {}, connected {})",
                    generation.0, self.generation, self.connected
                ),
            );
            return;
        }
        match fault {
            Fault::Interrupted => {
                self.close();
                if matches!(self.state, State::Holding | State::Entering) {
                    self.log(
                        Level::Info,
                        "fan control: the helper went away; re-applying",
                    );
                    self.reenter_counted();
                }
            }
            Fault::Invalid
            | Fault::PeerCodeSigningRequirement
            | Fault::Requirement
            | Fault::NotSignedForFans => {
                self.close();
                if self.desired.is_manual() {
                    self.unavailable(fault.unavailable());
                } else if self.state != State::Stuck {
                    self.set_state(State::Off);
                }
            }
            Fault::NoReply | Fault::Malformed => {
                self.log(
                    Level::Debug,
                    &format!("fan control: connection event {fault:?}"),
                );
            }
        }
    }

    /// Do what is due now.
    pub fn advance(&mut self) -> Flow {
        let fresh = std::mem::take(&mut self.fresh);
        if let Some(deadline) = self.stopping {
            self.shut_down(deadline);
            return Flow::Exit;
        }
        if std::mem::take(&mut self.force_pending) {
            self.release(true);
            return Flow::Continue;
        }
        let now = self.parts.clock.now();
        let tick_due = self.next_tick.is_some_and(|at| at <= now);
        let retry_due = self.retry_at.is_some_and(|at| at <= now);
        if !self.desired.is_manual() {
            if self.release_pending {
                if fresh || tick_due {
                    self.release(false);
                }
            } else if !fresh {
                // Nothing of ours to watch: no tick while Auto.
                if tick_due {
                    self.next_tick = None;
                }
            } else {
                match self.state {
                    State::Holding
                    | State::Entering
                    | State::Unknown
                    | State::Foreign(_)
                    | State::Stuck => self.release(false),
                    State::Unavailable(_) | State::Off | State::Releasing => {
                        if self.hold.is_some() || self.connected {
                            self.release(false);
                        } else if !matches!(self.state, State::Unavailable(Unavailable::NoSmc)) {
                            self.retry_at = None;
                            self.next_tick = None;
                            self.set_state(State::Off);
                        }
                    }
                }
            }
            return Flow::Continue;
        }
        match self.state {
            State::Off | State::Stuck | State::Entering | State::Releasing => self.enter(),
            // Only a command or a scheduled retry tries again, and a retry is
            // scheduled only for a reason that retries
            // ([`Unavailable::retries`]): a signature refusal waits for the
            // next command, so a refused requirement is never retried in a
            // loop.
            State::Unavailable(_) => {
                if fresh || retry_due {
                    self.retry_at = None;
                    self.enter();
                }
            }
            State::Holding | State::Unknown | State::Foreign(_) => {
                if fresh || tick_due {
                    self.tick(fresh);
                }
            }
        }
        Flow::Continue
    }

    // Entering.

    fn enter(&mut self) {
        match self.try_enter() {
            Ok(()) => {}
            Err(Stop::Settled) => {}
            Err(Stop::Changed) => {
                if self.stopping.is_none() {
                    let force = std::mem::take(&mut self.force_pending);
                    self.release(force);
                }
            }
        }
    }

    fn try_enter(&mut self) -> Result<(), Stop> {
        self.retry_at = None;
        self.detail = None;
        self.set_state(State::Entering);
        let Some(reading) = self.parts.heat.read() else {
            self.unavailable(Unavailable::NoSmc);
            return Err(Stop::Settled);
        };
        if reading.fans.is_empty() {
            self.unavailable(Unavailable::NoFans);
            return Err(Stop::Settled);
        }
        self.check_foreign(&reading)?;

        let generation = self.open()?;
        match self.parts.helper.send(&Request::Version, VERSION_DEADLINE) {
            Ok(Reply::Version { protocol, .. }) if protocol == PROTOCOL_VERSION => {}
            Ok(Reply::Version { protocol, .. }) => {
                self.close();
                self.unavailable(Unavailable::HelperVersion {
                    installed: Some(protocol),
                });
                return Err(Stop::Settled);
            }
            Ok(Reply::Refused(error)) => {
                return Err(self.entry_failed(Failure::Refused(error), false));
            }
            Ok(_other) => return Err(self.entry_failed(Failure::Fault(Fault::Malformed), false)),
            Err(fault) => return Err(self.entry_failed(Failure::Fault(fault), false)),
        }

        // The record goes to disk before the first write: a crash between the
        // two leaves a record that matches, never fans that match no record.
        let fans: Vec<FanIndex> = reading.fans.iter().map(|f| f.index).collect();
        self.hold = Some(Hold {
            writer_pid: self.parts.pid,
            fans: fans
                .iter()
                .map(|&fan| HeldFan {
                    fan,
                    written_rpm: None,
                })
                .collect(),
        });
        self.save();

        for &fan in &fans {
            self.between_steps(generation)?;
            self.probe_before_write()?;
            match self
                .parts
                .helper
                .send(&Request::SetManual { fan }, SET_MANUAL_DEADLINE)
            {
                Ok(Reply::Manual { .. }) => {}
                Ok(Reply::Refused(error)) => {
                    return Err(self.entry_failed(Failure::Refused(error), true));
                }
                Ok(_other) => return Err(self.entry_failed(Failure::Fault(Fault::Malformed), true)),
                Err(fault) => return Err(self.entry_failed(Failure::Fault(fault), true)),
            }
        }

        self.hot = guard(self.hot, reading.cpu_celsius);
        for fan_reading in &reading.fans {
            self.between_steps(generation)?;
            let Some(target) = target_for(self.desired, fan_reading, reading.cpu_celsius, self.hot)
            else {
                return Err(self.entry_failed(
                    Failure::Refused(HelperError::LimitsUnreadable {
                        fan: u64::from(fan_reading.index.get()),
                    }),
                    true,
                ));
            };
            self.probe_before_write()?;
            self.write_target(fan_reading.index, target)
                .map_err(|failure| self.entry_failed(failure, true))?;
        }

        self.failed_entries = 0;
        self.backoff_step = 0;
        self.set_state(State::Holding);
        self.arm_tick();
        Ok(())
    }

    /// Between two writes: take what arrived, and stop when the wish is no
    /// longer manual or this connection is gone (requirement H2).
    fn between_steps(&mut self, generation: Generation) -> Result<(), Stop> {
        self.drain();
        if self.stopping.is_some() || self.force_pending || !self.desired.is_manual() {
            return Err(Stop::Changed);
        }
        if !self.connected || Generation(self.generation) != generation {
            // An event replaced the state already (interrupted, invalid).
            return Err(Stop::Settled);
        }
        Ok(())
    }

    /// Macs Fan Control, or a probe that failed, means no write now.
    fn probe_before_write(&mut self) -> Result<(), Stop> {
        match self.parts.probe.macs_running() {
            Ok(false) => Ok(()),
            Ok(true) => {
                self.set_state(State::Foreign(ForeignHolder::MacsFanControl));
                self.arm_tick();
                Err(Stop::Settled)
            }
            Err(why) => {
                self.log(
                    Level::Warn,
                    &format!("fan control: the Macs Fan Control probe failed: {why}"),
                );
                self.set_state(State::Unknown);
                self.arm_tick();
                Err(Stop::Settled)
            }
        }
    }

    /// Before entering: whoever else has the fans keeps them.
    fn check_foreign(&mut self, reading: &HeatReading) -> Result<(), Stop> {
        self.probe_before_write()?;
        let record = self.hold.as_ref().map(|h| h.fans.as_slice());
        match holder::classify(&states(reading), record, false) {
            Holder::MacOs | Holder::Ours => Ok(()),
            Holder::AnotherApp | Holder::MacsFanControl => {
                self.set_state(State::Foreign(ForeignHolder::AnotherApp));
                self.arm_tick();
                Err(Stop::Settled)
            }
            Holder::Unknown => {
                self.set_state(State::Unknown);
                self.arm_tick();
                Err(Stop::Settled)
            }
        }
    }

    fn open(&mut self) -> Result<Generation, Stop> {
        self.close();
        self.generation += 1;
        let generation = Generation(self.generation);
        match self.parts.helper.open(generation) {
            Ok(()) => {
                self.connected = true;
                Ok(generation)
            }
            Err(fault) => Err(self.entry_failed(Failure::Fault(fault), false)),
        }
    }

    fn close(&mut self) {
        if self.connected {
            self.parts.helper.close();
            self.connected = false;
        }
    }

    /// One set-target, recorded and saved.
    fn write_target(&mut self, fan: FanIndex, rpm: f64) -> Result<(), Failure> {
        match self
            .parts
            .helper
            .send(&Request::SetTarget { fan, rpm }, SET_TARGET_DEADLINE)
        {
            Ok(Reply::Target { rpm: written }) => {
                if let Some(hold) = &mut self.hold
                    && let Some(held) = hold.fans.iter_mut().find(|h| h.fan == fan)
                {
                    held.written_rpm = Some(written);
                }
                self.save();
                self.log(
                    Level::Debug,
                    &format!("fan control: fan {} target {written:.0} rpm", fan.get()),
                );
                Ok(())
            }
            Ok(Reply::Refused(error)) => Err(Failure::Refused(error)),
            Ok(_other) => Err(Failure::Fault(Fault::Malformed)),
            Err(fault) => Err(Failure::Fault(fault)),
        }
    }

    /// Entering failed partway. Anything written is handed back BEFORE the
    /// error is reported (requirement H3).
    fn entry_failed(&mut self, failure: Failure, wrote: bool) -> Stop {
        if failure == Failure::Refused(HelperError::NotHolder) {
            self.taken_over();
            return Stop::Settled;
        }
        if wrote && self.connected {
            self.hand_back_once(HAND_BACK_DEADLINE);
        }
        self.close();
        if failure == Failure::Fault(Fault::Interrupted) {
            self.reenter_counted();
            return Stop::Settled;
        }
        self.failed_entries += 1;
        let why = failure.unavailable();
        self.log(
            Level::Warn,
            &format!("fan control: could not take the fans: {why:?}"),
        );
        self.unavailable(why);
        Stop::Settled
    }

    /// Another tcr's set-manual made it the holder: stop writing, hand
    /// nothing back, and leave the record on disk to its new writer.
    fn taken_over(&mut self) {
        self.close();
        self.hold = None;
        self.detail = Some(Detail::TakenOver);
        self.set_state(State::Unavailable(Unavailable::HelperRefused(
            HelperError::NotHolder,
        )));
        self.next_tick = None;
        self.retry_at = None;
    }

    /// Set `Unavailable(why)` and schedule the retry, if this one retries
    /// and the run of failures is short enough.
    fn unavailable(&mut self, why: Unavailable) {
        self.next_tick = None;
        self.retry_at = None;
        if why.retries() && self.desired.is_manual() {
            if self.failed_entries >= ENTRY_FAILURES_BEFORE_STOP {
                self.detail = Some(Detail::RetryStopped);
            } else {
                let wait = BACKOFF[self.backoff_step.min(BACKOFF.len() - 1)];
                self.backoff_step += 1;
                self.retry_at = Some(self.parts.clock.now() + wait);
            }
        }
        self.set_state(State::Unavailable(why));
    }

    /// Re-enter after the fans went back to automatic or the helper went
    /// away, unless that keeps happening.
    fn reenter_counted(&mut self) {
        let now = self.parts.clock.now();
        self.reentries.push_back(now);
        while self
            .reentries
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) > REENTRY_WINDOW)
        {
            self.reentries.pop_front();
        }
        if self.reentries.len() > REENTRY_LIMIT {
            self.log(
                Level::Warn,
                "fan control: the fans keep going back to automatic; stopping",
            );
            if self.connected {
                self.hand_back_once(HAND_BACK_DEADLINE);
            }
            self.close();
            self.unavailable(Unavailable::Contended);
            return;
        }
        self.enter();
    }

    // The tick.

    fn arm_tick(&mut self) {
        self.next_tick = Some(self.parts.clock.now() + TICK);
    }

    /// One tick: read, probe, classify, guard, target, gate, write.
    /// `forced`: a new command, so the gate does not hold a changed target
    /// back.
    pub fn tick(&mut self, forced: bool) -> TickOutcome {
        self.arm_tick();
        let outcome = self.tick_inner(forced);
        self.last_outcome = Some(outcome);
        outcome
    }

    fn tick_inner(&mut self, forced: bool) -> TickOutcome {
        let after_sleep = self.slept();
        let Some(reading) = self.parts.heat.read() else {
            self.set_state(State::Unknown);
            return TickOutcome::HeldBack(Why::Unreadable);
        };
        match self.parts.probe.macs_running() {
            Ok(false) => {}
            Ok(true) => {
                self.set_state(State::Foreign(ForeignHolder::MacsFanControl));
                return TickOutcome::HeldBack(Why::MacsFanControl);
            }
            Err(why) => {
                self.log(
                    Level::Warn,
                    &format!("fan control: the Macs Fan Control probe failed: {why}"),
                );
                self.set_state(State::Unknown);
                return TickOutcome::HeldBack(Why::ProbeFailed);
            }
        }
        let record = self.hold.as_ref().map(|h| h.fans.as_slice());
        match holder::classify(&states(&reading), record, false) {
            Holder::Unknown => {
                self.set_state(State::Unknown);
                TickOutcome::HeldBack(Why::Unreadable)
            }
            Holder::AnotherApp | Holder::MacsFanControl => {
                if self.hold.take().is_some() {
                    self.log(
                        Level::Info,
                        "fan control: another program changed the fans; leaving them to it",
                    );
                    self.save();
                }
                self.close();
                self.set_state(State::Foreign(ForeignHolder::AnotherApp));
                TickOutcome::HeldBack(Why::AnotherApp)
            }
            Holder::MacOs => {
                let counted = self.state == State::Holding
                    || (self.state == State::Unknown && self.hold.is_some());
                if counted {
                    if after_sleep {
                        self.detail = Some(Detail::AfterSleep);
                    }
                    self.log(
                        Level::Info,
                        if after_sleep {
                            "fan control: every fan is automatic again after sleep; re-applying"
                        } else {
                            "fan control: every fan is automatic again; re-applying"
                        },
                    );
                    self.reenter_counted();
                } else {
                    self.enter();
                }
                TickOutcome::HeldBack(Why::Reentering)
            }
            Holder::Ours => {
                if !self.connected {
                    self.enter();
                    return TickOutcome::HeldBack(Why::Reentering);
                }
                if self.state != State::Holding {
                    self.detail = None;
                    self.set_state(State::Holding);
                }
                self.write_round(&reading, forced)
            }
        }
    }

    /// Work out each held fan's target and write the ones past the gate.
    fn write_round(&mut self, reading: &HeatReading, forced: bool) -> TickOutcome {
        let was_hot = self.hot;
        self.hot = guard(self.hot, reading.cpu_celsius);
        let flipped = was_hot != self.hot;
        let mut wrote = 0_u8;
        let mut limits_unknown = false;
        for fan in &reading.fans {
            let Some(previous) = self
                .hold
                .as_ref()
                .and_then(|h| h.fans.iter().find(|held| held.fan == fan.index))
                .map(|held| held.written_rpm)
            else {
                continue;
            };
            let Some(target) = target_for(self.desired, fan, reading.cpu_celsius, self.hot) else {
                limits_unknown = true;
                continue;
            };
            if !should_write(previous, target, fan, self.desired, flipped || forced) {
                continue;
            }
            match self.parts.probe.macs_running() {
                Ok(false) => {}
                Ok(true) => {
                    self.set_state(State::Foreign(ForeignHolder::MacsFanControl));
                    return TickOutcome::HeldBack(Why::MacsFanControl);
                }
                Err(_probe_failed) => {
                    self.set_state(State::Unknown);
                    return TickOutcome::HeldBack(Why::ProbeFailed);
                }
            }
            match self.write_target(fan.index, target) {
                Ok(()) => wrote = wrote.saturating_add(1),
                Err(Failure::Refused(HelperError::NotHolder)) => {
                    self.taken_over();
                    return TickOutcome::HeldBack(Why::Helper);
                }
                Err(Failure::Fault(Fault::Interrupted)) => {
                    self.close();
                    self.reenter_counted();
                    return TickOutcome::HeldBack(Why::Helper);
                }
                Err(failure) => {
                    self.hand_back_once(HAND_BACK_DEADLINE);
                    self.close();
                    self.failed_entries += 1;
                    self.unavailable(failure.unavailable());
                    return TickOutcome::HeldBack(Why::Helper);
                }
            }
        }
        if flipped {
            self.publish();
        }
        if wrote > 0 {
            self.publish();
            TickOutcome::Wrote(wrote)
        } else if limits_unknown {
            TickOutcome::HeldBack(Why::LimitsUnknown)
        } else {
            TickOutcome::NoChange
        }
    }

    /// Did the machine sleep since the last tick? The monotonic clock stops
    /// in sleep on macOS; the wall clock does not.
    fn slept(&mut self) -> bool {
        let now = (self.parts.clock.now(), self.parts.clock.wall_ms());
        let slept = self.last_tick.is_some_and(|(at, wall)| {
            let monotonic = now.0.saturating_duration_since(at).as_millis();
            let wall = u128::try_from(now.1.saturating_sub(wall)).unwrap_or(0);
            wall.saturating_sub(monotonic) > SLEEP_GAP.as_millis()
        });
        self.last_tick = Some(now);
        slept
    }

    // Releasing.

    /// Hand the fans back. `force` skips the record check, never the Macs
    /// Fan Control check.
    fn release(&mut self, force: bool) {
        self.release_pending = false;
        self.retry_at = None;
        self.detail = None;
        self.set_state(State::Releasing);
        match self.parts.probe.macs_running() {
            Ok(false) => {}
            Ok(true) => {
                self.release_pending = true;
                self.force_pending = force;
                self.detail = Some(Detail::WaitingForMacs);
                self.set_state(State::Foreign(ForeignHolder::MacsFanControl));
                self.arm_tick();
                return;
            }
            Err(why) => {
                self.log(
                    Level::Warn,
                    &format!("fan control: the Macs Fan Control probe failed: {why}"),
                );
                self.release_pending = true;
                self.force_pending = force;
                self.set_state(State::Unknown);
                self.arm_tick();
                return;
            }
        }
        if !force {
            let Some(hold) = self.hold.clone() else {
                self.close();
                self.finish_off();
                return;
            };
            let Some(reading) = self.parts.heat.read() else {
                self.close();
                self.finish_off();
                return;
            };
            match holder::still_ours(&states(&reading), &hold.fans) {
                StillOurs::Ours => {}
                StillOurs::Changed { .. } => {
                    self.log(
                        Level::Info,
                        "fan control: another program has the fans; nothing to hand back",
                    );
                    self.hold = None;
                    self.save();
                    self.close();
                    self.finish_off();
                    return;
                }
                StillOurs::Unknown { .. } => {
                    self.release_pending = true;
                    self.set_state(State::Unknown);
                    self.arm_tick();
                    return;
                }
            }
        }
        self.hand_back_with_tries(None);
    }

    /// Up to [`RELEASE_TRIES`] hand-backs; `deadline` bounds them all.
    fn hand_back_with_tries(&mut self, deadline: Option<Instant>) {
        for attempt in 0..RELEASE_TRIES {
            let budget = deadline.map_or(HAND_BACK_DEADLINE, |d| {
                d.saturating_duration_since(self.parts.clock.now())
                    .min(HAND_BACK_DEADLINE)
            });
            if budget.is_zero() {
                break;
            }
            if !self.connected {
                self.close();
                self.generation += 1;
                match self.parts.helper.open(Generation(self.generation)) {
                    Ok(()) => self.connected = true,
                    Err(fault) => {
                        self.log(
                            Level::Warn,
                            &format!(
                                "fan control: could not reach the helper to hand back: {fault:?}"
                            ),
                        );
                        if matches!(
                            fault,
                            Fault::NotSignedForFans
                                | Fault::Requirement
                                | Fault::PeerCodeSigningRequirement
                        ) {
                            break;
                        }
                        self.pause_between_tries(attempt, deadline);
                        continue;
                    }
                }
            }
            match self.hand_back_once(budget) {
                HandBack::Done => {
                    self.close();
                    self.finish_off();
                    return;
                }
                HandBack::NotHolder => {
                    self.hold = None;
                    self.close();
                    self.finish_off();
                    return;
                }
                HandBack::Failed => self.pause_between_tries(attempt, deadline),
            }
        }
        self.log(
            Level::Warn,
            "fan control: hand-back failed; the helper hands back on its own once this connection closes",
        );
        self.close();
        self.next_tick = None;
        self.set_state(State::Stuck);
    }

    fn pause_between_tries(&self, attempt: usize, deadline: Option<Instant>) {
        if attempt + 1 >= RELEASE_TRIES {
            return;
        }
        let gap = deadline.map_or(RELEASE_GAP, |d| {
            d.saturating_duration_since(self.parts.clock.now())
                .min(RELEASE_GAP)
        });
        self.parts.clock.sleep(gap);
    }

    /// One hand-back on the open connection. Success clears the record when
    /// this process wrote it.
    fn hand_back_once(&mut self, deadline: Duration) -> HandBack {
        match self.parts.helper.send(&Request::HandBack, deadline) {
            Ok(Reply::HandedBack) => {
                if self
                    .hold
                    .as_ref()
                    .is_some_and(|h| h.writer_pid == self.parts.pid)
                {
                    self.hold = None;
                    self.save();
                }
                HandBack::Done
            }
            Ok(Reply::Refused(HelperError::NotHolder)) => HandBack::NotHolder,
            Ok(other) => {
                self.log(
                    Level::Warn,
                    &format!("fan control: hand-back answered {other:?}"),
                );
                HandBack::Failed
            }
            Err(fault) => {
                self.log(
                    Level::Warn,
                    &format!("fan control: hand-back failed: {fault:?}"),
                );
                HandBack::Failed
            }
        }
    }

    fn finish_off(&mut self) {
        self.next_tick = None;
        self.release_pending = false;
        self.force_pending = false;
        self.detail = None;
        self.set_state(State::Off);
    }

    /// The proxy is stopping. Hand back what is ours by `deadline`; the wish
    /// stays on disk for the next start. With Macs Fan Control running nothing
    /// is written, and the closing connection leaves the decision to root.
    fn shut_down(&mut self, deadline: Instant) {
        self.next_tick = None;
        self.retry_at = None;
        if self.hold.is_none() && !self.connected {
            return;
        }
        let macs = matches!(self.parts.probe.macs_running(), Ok(true));
        if !macs && self.hold.is_some() {
            self.set_state(State::Releasing);
            self.hand_back_with_tries(Some(deadline));
        }
        self.close();
    }

    // Small things.

    fn set_state(&mut self, state: State) {
        if state != self.state {
            self.log(
                Level::Info,
                &format!("fan control: {} -> {}", self.state, state),
            );
            self.state = state;
            self.since_ms = self.parts.clock.wall_ms();
        }
        self.publish();
    }

    fn publish(&mut self) {
        let snapshot = self.snapshot();
        (self.parts.publish)(&snapshot);
    }

    fn save(&mut self) {
        let saved = Saved {
            desired: self.desired,
            hold: self.hold.clone(),
        };
        self.parts.store.save(&saved);
    }

    fn log(&mut self, level: Level, line: &str) {
        (self.parts.log)(level, line);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandBack {
    Done,
    NotHolder,
    Failed,
}

/// Why a helper step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Fault(Fault),
    Refused(HelperError),
}

impl Failure {
    const fn unavailable(self) -> Unavailable {
        match self {
            Self::Fault(fault) => fault.unavailable(),
            Self::Refused(HelperError::ModeLocked { fan }) => Unavailable::ModeLocked { fan },
            Self::Refused(error) => Unavailable::HelperRefused(error),
        }
    }
}

/// The reading as the holder rule reads it.
fn states(reading: &HeatReading) -> Vec<FanState> {
    reading
        .fans
        .iter()
        .map(|f| FanState {
            fan: f.index,
            mode: f.mode,
            target_rpm: f.target_rpm,
        })
        .collect()
}

/// The hot guard after this reading: on at [`HotGuard::ON`], off below
/// [`HotGuard::OFF`], unchanged without a reading.
fn guard(on: bool, celsius: Option<f64>) -> bool {
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
fn target_for(mode: FanMode, fan: &FanReading, celsius: Option<f64>, hot: bool) -> Option<f64> {
    let range = limits(fan);
    if hot && mode.is_manual() {
        return range.map(|(_, max)| max);
    }
    match mode {
        FanMode::Auto => None,
        FanMode::Fixed { rpm } => {
            let rpm = f64::from(rpm.get());
            Some(range.map_or(rpm, |(min, max)| rpm.clamp(min, max)))
        }
        FanMode::FollowCpu { curve } => {
            let (min, max) = range?;
            Some(celsius.map_or(max, |c| curve.share(c).mul_add(max - min, min)))
        }
    }
}

/// The write gate. `forced`: a new command or a hot-guard flip.
fn should_write(
    previous: Option<f64>,
    target: f64,
    fan: &FanReading,
    mode: FanMode,
    forced: bool,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if (target - previous).abs() <= holder::TARGET_TOLERANCE_RPM {
        return false;
    }
    if forced || matches!(mode, FanMode::Fixed { .. }) {
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

#[cfg(test)]
mod tests;
