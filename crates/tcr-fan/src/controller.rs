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

use crate::holder::{self, FanState, HeldFan, Holder, StillOurs, Unwritten};
use crate::wire::{HelperError, PROTOCOL_VERSION, Reply, Request};

/// How often a manual mode is re-checked and re-targeted. A starting value,
/// not a measured one.
pub const TICK: Duration = Duration::from_secs(3);
/// The version call's deadline. It is the first call on a new connection, so
/// it waits for launchd to start the helper, which launchd does no sooner than
/// 10 s after the helper's previous start (measured 2026-10-18 on an M4 Pro:
/// starts at 22:40:34.13, 22:40:44.29 and 22:40:54.71), and then about 0.5 s
/// for the helper to listen. 2 s failed every command sent inside that window
/// with no reply; 15 s covers it with room.
pub const VERSION_DEADLINE: Duration = Duration::from_secs(15);
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

/// A command from the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Set(FanDesired),
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
    /// The controller's thread stopped on an internal error: nothing runs
    /// until the proxy restarts. Its closed connection lets root judge the
    /// fans and hand back what is still its doing.
    ControllerStopped,
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
    /// A command, numbered by its sender in channel order: [`Snapshot::command`]
    /// reaches `seq` once the controller has acted on it. A shutdown's number
    /// is not counted.
    Command { seq: u64, command: Command },
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
    pub desired: FanDesired,
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

/// What the status route shows.
///
/// `desired` and `command` are set together and only together, when the
/// controller stamps ([`Controller::advance`] after acting on a command, or a
/// state change with no command waiting): every snapshot pairs a state with
/// the wish it was reached for. A command taken but not yet acted on does not
/// show; the wish stays the one the controller last acted on.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The wish the controller last acted on.
    pub desired: FanDesired,
    pub state: State,
    pub hot_guard: bool,
    /// What was last written to each fan held.
    pub targets: Vec<(FanIndex, f64)>,
    pub detail: Option<Detail>,
    /// The number of the newest command the controller has acted on
    /// ([`Input::Command`]). Taking a command does not move it, so a snapshot
    /// from before the controller acted on a command reads below that
    /// command's number. Starts at [`Parts::commands_before`].
    pub command: u64,
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
    /// Where command numbers start: every command this process is sent is
    /// numbered above it. The proxy seeds it from the wall clock, so a
    /// command sent to a process that has since restarted reads as acted on.
    pub commands_before: u64,
}

/// How an attempt to enter ended, when it did not end holding.
enum Stop {
    /// The wish changed (or a stop arrived) between steps: release now.
    Changed,
    /// The state was set; nothing more to do this round.
    Settled,
}

/// Why no write may happen now, before the state says so.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Blocked {
    /// Macs Fan Control is running.
    Macs,
    /// The Macs Fan Control probe failed.
    ProbeFailed(String),
}

impl Blocked {
    const fn why(&self) -> Why {
        match self {
            Self::Macs => Why::MacsFanControl,
            Self::ProbeFailed(_) => Why::ProbeFailed,
        }
    }
}

/// A release that is wanted and waits (on Macs Fan Control, or a read).
/// Forced wins wherever it is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Release,
    /// `auto --force`: hand back without the record check.
    ForcedRelease,
}

/// What a release (or a shutdown) found it may do.
enum Verdict {
    /// The fans are ours (or the hand-back is forced): hand back.
    HandBack,
    /// Nothing of ours is on the fans.
    NothingOurs,
    /// No write now; a later look decides.
    Wait(Waiting),
}

enum Waiting {
    Blocked(Blocked),
    /// A fan's mode or target did not read, or the reading lost a held fan.
    Unreadable,
}

pub struct Controller<H, R, P, C, S> {
    parts: Parts<H, R, P, C, S>,
    desired: FanDesired,
    hold: Option<Hold>,
    state: State,
    generation: u64,
    connected: bool,
    hot: bool,
    detail: Option<Detail>,
    /// The wish [`Snapshot::desired`] shows: `desired` as of the last
    /// [`Self::stamp`], never ahead of it.
    shown_desired: FanDesired,
    /// The newest command number taken, and the newest acted on (what
    /// [`Snapshot::command`] shows, moved with `shown_desired`).
    taken: u64,
    acted: u64,
    failed_entries: u32,
    backoff_step: usize,
    retry_at: Option<Instant>,
    next_tick: Option<Instant>,
    reentries: VecDeque<Instant>,
    /// A hand-back is wanted and waits on Macs Fan Control or a read.
    pending: Option<Pending>,
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
        let first = parts.commands_before;
        let (desired, hold) = saved.map_or((FanDesired::Auto, None), |s| (s.desired, s.hold));
        let mut controller = Self {
            parts,
            desired,
            hold,
            state: State::Off,
            generation: 0,
            connected: false,
            hot: false,
            detail: None,
            shown_desired: desired,
            taken: first,
            acted: first,
            failed_entries: 0,
            backoff_step: 0,
            retry_at: None,
            next_tick: None,
            reentries: VecDeque::new(),
            pending: None,
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
        if self.hold.is_some() {
            // The state is still `Off` here, so a fan with no target yet is
            // not ours: `whose` passes `Unwritten::NotOurs`.
            match self.whose(&reading) {
                // A held fan back on automatic is re-taken by the first
                // entry, or handed back with the rest when Auto is wanted.
                Holder::Ours | Holder::PartlyAutomatic => {
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
            self.pending = Some(Pending::Release);
            self.next_tick = Some(self.parts.clock.now());
        }
        self.publish();
    }

    // Accessors, for the thread and for tests.

    pub const fn state(&self) -> State {
        self.state
    }

    pub const fn desired(&self) -> FanDesired {
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
            desired: self.shown_desired,
            state: self.state,
            // Off as soon as the shown wish leaves Follow CPU, not at the next
            // tick.
            hot_guard: self.hot && self.shown_desired.has_hot_guard(),
            targets: self
                .hold
                .iter()
                .flat_map(|h| h.fans.iter())
                .filter_map(|f| f.written_rpm.map(|rpm| (f.fan, rpm)))
                .collect(),
            detail: self.detail,
            command: self.acted,
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
        Input::Command {
            seq: self.taken,
            command: Command::Shutdown {
                deadline: self.parts.clock.now() + Duration::from_secs(3),
            },
        }
    }

    /// How long the loop may wait for input before it has work of its own.
    /// Zero while a command taken in the middle of a step waits: it is acted
    /// on at once, not at the next tick.
    fn wait(&self) -> Option<Duration> {
        if self.fresh {
            return Some(Duration::ZERO);
        }
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
            Input::Command {
                seq,
                command: Command::Set(mode),
            } => {
                self.desired = mode;
                self.taken = seq;
                self.fresh = true;
                // A hand-back that waited (on Macs Fan Control, or a read) was
                // for the wish this one replaces: a manual mode must never be
                // handed back by it, and Auto starts its own.
                self.pending = None;
                self.failed_entries = 0;
                self.backoff_step = 0;
                self.retry_at = None;
                self.reentries.clear();
                // No stamp here: taking a command is not acting on it, and
                // the snapshot keeps the wish last acted on until
                // [`Self::advance`] stamps after the step for this one.
                self.save();
                self.publish();
            }
            Input::Command {
                seq,
                command: Command::ForceHandBack,
            } => {
                self.desired = FanDesired::Auto;
                self.taken = seq;
                self.pending = Some(Pending::ForcedRelease);
                self.fresh = true;
                self.save();
                self.publish();
            }
            Input::Command {
                command: Command::Shutdown { deadline },
                ..
            } => self.stopping = Some(deadline),
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
    ///
    /// A stop is acted on whenever it arrived: before the step, or drained in
    /// the middle of one (between two writes of an entry), so the loop never
    /// goes back to waiting on a channel whose stop it already took.
    ///
    /// A step for a command ends with a [`Self::stamp`] (the wish shown and
    /// the command number acted on), whether it changed the state, wrote a
    /// round, or found nothing to do (the same Fixed again): that stamp is
    /// what `tcr fan` waits for. Not when a newer command was taken during the step, which
    /// the next step acts on.
    pub fn advance(&mut self) -> Flow {
        let fresh = std::mem::take(&mut self.fresh);
        if self.stopping.is_none() {
            self.step(fresh);
            if fresh && !self.fresh && self.stopping.is_none() {
                self.stamp();
                self.publish();
            }
        }
        if let Some(deadline) = self.stopping {
            self.shut_down(deadline);
            return Flow::Exit;
        }
        Flow::Continue
    }

    fn step(&mut self, fresh: bool) {
        let now = self.parts.clock.now();
        let tick_due = self.next_tick.is_some_and(|at| at <= now);
        let retry_due = self.retry_at.is_some_and(|at| at <= now);
        if self.pending == Some(Pending::ForcedRelease) {
            // A forced hand-back waiting on Macs Fan Control looks again on
            // the tick, like any other waiting release.
            if fresh || tick_due {
                self.release(true);
            }
            return;
        }
        if !self.desired.is_manual() {
            if self.pending == Some(Pending::Release) {
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
                            self.unschedule();
                            self.set_state(State::Off);
                        }
                    }
                }
            }
            return;
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
    }

    // Entering.

    fn enter(&mut self) {
        match self.try_enter() {
            Ok(()) | Err(Stop::Settled) => {}
            Err(Stop::Changed) => {
                // Still `Entering`, so the release's look at the record
                // counts the fans set manual without a target yet as ours,
                // and hands them back. A stop is the shutdown's to do.
                if self.stopping.is_none() {
                    self.release(self.pending == Some(Pending::ForcedRelease));
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
            if self.hold.is_some() {
                // `FNum` did not read while a record names fans: unknown,
                // never "no fans", and the record stays.
                self.wait_in(State::Unknown);
            } else {
                self.unavailable(Unavailable::NoFans);
            }
            return Err(Stop::Settled);
        }
        self.check_foreign(&reading)?;

        let generation = self
            .dial()
            .map_err(|fault| self.entry_failed(Failure::Fault(fault), false))?;
        match self.call(&Request::Version, VERSION_DEADLINE) {
            Ok(Reply::Version { protocol, .. }) if protocol == PROTOCOL_VERSION => {}
            Ok(Reply::Version { protocol, .. }) => {
                self.close();
                self.unavailable(Unavailable::HelperVersion {
                    installed: Some(protocol),
                });
                return Err(Stop::Settled);
            }
            Ok(_other) => return Err(self.entry_failed(Failure::Fault(Fault::Malformed), false)),
            Err(failure) => return Err(self.entry_failed(failure, false)),
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

        self.take_fans(generation, &fans, &reading)?;

        self.failed_entries = 0;
        self.backoff_step = 0;
        self.set_state(State::Holding);
        self.arm_tick();
        Ok(())
    }

    /// Every set-manual, then every set-target. Every way it stops is settled
    /// here: a block by [`Self::blocked_mid_entry`], a failure by
    /// [`Self::entry_failed`], both handing back first when a set-manual went
    /// out (requirement H3).
    fn take_fans(
        &mut self,
        generation: Generation,
        fans: &[FanIndex],
        reading: &HeatReading,
    ) -> Result<(), Stop> {
        for (sent, &fan) in fans.iter().enumerate() {
            self.between_steps(generation)?;
            if let Err(blocked) = self.probe() {
                // `sent > 0`: an earlier fan's set-manual went out.
                return Err(self.blocked_mid_entry(blocked, sent > 0));
            }
            match self.call(&Request::SetManual { fan }, SET_MANUAL_DEADLINE) {
                Ok(Reply::Manual { .. }) => {}
                Ok(_other) => return Err(self.entry_failed(Failure::Fault(Fault::Malformed), true)),
                Err(failure) => return Err(self.entry_failed(failure, true)),
            }
        }

        self.hot = self.desired.has_hot_guard() && guard(self.hot, reading.cpu_celsius);
        for fan_reading in &reading.fans {
            self.between_steps(generation)?;
            let Some(target) = target_for(self.desired, fan_reading, reading.cpu_celsius, self.hot)
            else {
                let fan = u64::from(fan_reading.index.get());
                return Err(self.entry_failed(
                    Failure::Refused(HelperError::LimitsUnreadable { fan }),
                    true,
                ));
            };
            if let Err(blocked) = self.probe() {
                return Err(self.blocked_mid_entry(blocked, true));
            }
            self.write_target(fan_reading.index, target)
                .map_err(|failure| self.entry_failed(failure, true))?;
        }
        Ok(())
    }

    /// A block stopped an entry partway. Requirement H3: whatever stopped the
    /// entry, a fan already set manual (`owed`) goes back before the state
    /// says why.
    fn blocked_mid_entry(&mut self, blocked: Blocked, owed: bool) -> Stop {
        self.hand_back_then_close(owed);
        self.settle_blocked(blocked);
        Stop::Settled
    }

    /// Between two writes: take what arrived, and stop when the wish is no
    /// longer manual or this connection is gone (requirement H2).
    fn between_steps(&mut self, generation: Generation) -> Result<(), Stop> {
        self.drain();
        let forced = self.pending == Some(Pending::ForcedRelease);
        if self.stopping.is_some() || forced || !self.desired.is_manual() {
            return Err(Stop::Changed);
        }
        if !self.connected || Generation(self.generation) != generation {
            // An event replaced the state already (interrupted, invalid).
            return Err(Stop::Settled);
        }
        Ok(())
    }

    /// Macs Fan Control running, or a probe that failed, means no write now.
    /// Changes nothing: the caller hands back first when it owes one, then
    /// [`Self::settle_blocked`].
    fn probe(&mut self) -> Result<(), Blocked> {
        match self.parts.probe.macs_running() {
            Ok(false) => Ok(()),
            Ok(true) => Err(Blocked::Macs),
            Err(why) => Err(Blocked::ProbeFailed(why)),
        }
    }

    /// The state for a block, the tick kept armed so the hand-over is seen.
    /// A failed probe is said once, when the state turns unknown, not on
    /// every tick it stays failed.
    fn settle_blocked(&mut self, blocked: Blocked) {
        match blocked {
            Blocked::Macs => self.set_state(State::Foreign(ForeignHolder::MacsFanControl)),
            Blocked::ProbeFailed(why) => {
                if self.state != State::Unknown {
                    self.log(
                        Level::Warn,
                        &format!("fan control: the Macs Fan Control probe failed: {why}"),
                    );
                }
                self.set_state(State::Unknown);
            }
        }
        self.arm_tick();
    }

    /// Before entering: whoever else has the fans keeps them. Every way it
    /// stops has set the state already.
    fn check_foreign(&mut self, reading: &HeatReading) -> Result<(), Stop> {
        if let Err(blocked) = self.probe() {
            self.settle_blocked(blocked);
            return Err(Stop::Settled);
        }
        match self.whose(reading) {
            Holder::MacOs | Holder::Ours | Holder::PartlyAutomatic => return Ok(()),
            Holder::AnotherApp | Holder::MacsFanControl => {
                self.wait_in(State::Foreign(ForeignHolder::AnotherApp));
            }
            Holder::Unknown => self.wait_in(State::Unknown),
        }
        Err(Stop::Settled)
    }

    /// Who holds the fans, by the keys against this process's record.
    fn whose(&self, reading: &HeatReading) -> Holder {
        let record = self.hold.as_ref().map(|h| h.fans.as_slice());
        holder::classify(&states(reading), record, self.unwritten(), false)
    }

    /// How a held fan with no target yet counts: ours only in the middle of
    /// an entry, which hands back whenever it stops short.
    fn unwritten(&self) -> Unwritten {
        if self.state == State::Entering {
            Unwritten::Ours
        } else {
            Unwritten::NotOurs
        }
    }

    /// Close, take the next generation, open (M2: one generation per
    /// connection).
    fn dial(&mut self) -> Result<Generation, Fault> {
        self.close();
        self.generation += 1;
        let generation = Generation(self.generation);
        self.parts.helper.open(generation)?;
        self.connected = true;
        Ok(generation)
    }

    /// One request; a refusal and a fault both come back as a [`Failure`].
    /// Each caller matches only the reply it expects.
    fn call(&mut self, request: &Request, deadline: Duration) -> Result<Reply, Failure> {
        match self.parts.helper.send(request, deadline) {
            Ok(Reply::Refused(error)) => Err(Failure::Refused(error)),
            Ok(reply) => Ok(reply),
            Err(fault) => Err(Failure::Fault(fault)),
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
        match self.call(&Request::SetTarget { fan, rpm }, SET_TARGET_DEADLINE) {
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
            Ok(_other) => Err(Failure::Fault(Fault::Malformed)),
            Err(failure) => Err(failure),
        }
    }

    /// Entering failed partway. Anything written is handed back BEFORE the
    /// error is reported (requirement H3).
    fn entry_failed(&mut self, failure: Failure, wrote: bool) -> Stop {
        if failure == Failure::Refused(HelperError::NotHolder) {
            self.taken_over();
            return Stop::Settled;
        }
        self.hand_back_then_close(wrote);
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
        self.unschedule();
    }

    /// Requirement H3: a fan this run set manual (`owed`) goes back before
    /// anything reports, then the connection closes.
    fn hand_back_then_close(&mut self, owed: bool) {
        if owed && self.connected {
            self.hand_back_once(HAND_BACK_DEADLINE);
        }
        self.close();
    }

    /// Set `Unavailable(why)` and schedule the retry, if this one retries
    /// and the run of failures is short enough.
    fn unavailable(&mut self, why: Unavailable) {
        self.unschedule();
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
            self.hand_back_then_close(true);
            self.unavailable(Unavailable::Contended);
            return;
        }
        self.enter();
    }

    // The tick.

    fn arm_tick(&mut self) {
        self.next_tick = Some(self.parts.clock.now() + TICK);
    }

    /// Set the state, then arm the tick that looks again.
    fn wait_in(&mut self, state: State) {
        self.set_state(state);
        self.arm_tick();
    }

    const fn unschedule(&mut self) {
        self.next_tick = None;
        self.retry_at = None;
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
        if let Err(blocked) = self.probe() {
            let why = blocked.why();
            self.settle_blocked(blocked);
            return TickOutcome::HeldBack(why);
        }
        match self.whose(&reading) {
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
            Holder::PartlyAutomatic => {
                // A fan we hold went back to automatic on its own while the
                // others kept our targets: it is not held, so it is taken
                // again, on the same count as every fan going back.
                self.log(
                    Level::Info,
                    "fan control: a held fan is automatic again; re-applying",
                );
                self.reenter_counted();
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

    /// Write each target [`plan_round`] lets through.
    fn write_round(&mut self, reading: &HeatReading, forced: bool) -> TickOutcome {
        let round = plan_round(self.desired, reading, self.hold.as_ref(), self.hot, forced);
        let flipped = round.hot != self.hot;
        self.hot = round.hot;
        let mut wrote = 0_u8;
        for (fan, target) in round.writes {
            if let Err(blocked) = self.probe() {
                let why = blocked.why();
                self.settle_blocked(blocked);
                return TickOutcome::HeldBack(why);
            }
            match self.write_target(fan, target) {
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
                    // Connected: a round runs only on an open connection.
                    self.hand_back_then_close(true);
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
        } else if round.limits_unknown {
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
    ///
    /// The look comes before any state change, so a release that is still
    /// waiting (on Macs Fan Control, or a read) changes nothing and says
    /// nothing new on each tick it waits.
    fn release(&mut self, force: bool) {
        self.pending = None;
        self.retry_at = None;
        match self.judge_release(force) {
            Verdict::NothingOurs => {
                self.close();
                self.finish_off();
            }
            Verdict::Wait(waiting) => {
                self.pending = Some(if force {
                    Pending::ForcedRelease
                } else {
                    Pending::Release
                });
                match waiting {
                    Waiting::Blocked(Blocked::Macs) => {
                        self.detail = Some(Detail::WaitingForMacs);
                        self.settle_blocked(Blocked::Macs);
                    }
                    Waiting::Blocked(blocked) => {
                        self.detail = None;
                        self.settle_blocked(blocked);
                    }
                    Waiting::Unreadable => {
                        self.detail = None;
                        self.wait_in(State::Unknown);
                    }
                }
            }
            Verdict::HandBack => {
                self.detail = None;
                self.set_state(State::Releasing);
                self.hand_back_with_tries(None);
            }
        }
    }

    /// May a release write now, and is there anything of ours to hand back?
    /// One set of checks for [`Self::release`] and [`Self::shut_down`]:
    /// nothing held and nothing forced is nothing to do (Macs Fan Control or
    /// not); Macs Fan Control or a failed probe waits; then the record.
    fn judge_release(&mut self, force: bool) -> Verdict {
        if !force && self.hold.is_none() {
            return Verdict::NothingOurs;
        }
        if let Err(blocked) = self.probe() {
            return Verdict::Wait(Waiting::Blocked(blocked));
        }
        if force {
            return Verdict::HandBack;
        }
        let Some(hold) = self.hold.clone() else {
            return Verdict::NothingOurs;
        };
        let Some(reading) = self.parts.heat.read() else {
            return Verdict::NothingOurs;
        };
        match holder::still_ours(&states(&reading), &hold.fans, self.unwritten()) {
            StillOurs::Ours => Verdict::HandBack,
            StillOurs::Changed { .. } => {
                self.log(
                    Level::Info,
                    "fan control: another program has the fans; nothing to hand back",
                );
                self.hold = None;
                self.save();
                Verdict::NothingOurs
            }
            StillOurs::Unknown { .. } => Verdict::Wait(Waiting::Unreadable),
        }
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
            if !self.connected
                && let Err(fault) = self.dial()
            {
                self.log(
                    Level::Warn,
                    &format!("fan control: could not reach the helper to hand back: {fault:?}"),
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
        // Not through `call`: the line below names any other reply whole, a
        // refusal included.
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
        self.pending = None;
        self.detail = None;
        self.set_state(State::Off);
    }

    /// The proxy is stopping. Hand back what is ours by `deadline`; the wish
    /// stays on disk for the next start. With Macs Fan Control running nothing
    /// is written, and the closing connection leaves the decision to root.
    ///
    /// The same checks as a release ([`Self::judge_release`]): a probe that
    /// failed is not "Macs Fan Control is not running", and fans another
    /// program changed are not handed back.
    fn shut_down(&mut self, deadline: Instant) {
        self.unschedule();
        self.pending = None;
        match self.judge_release(false) {
            Verdict::HandBack => {
                self.set_state(State::Releasing);
                self.hand_back_with_tries(Some(deadline));
            }
            Verdict::NothingOurs => {}
            Verdict::Wait(waiting) => {
                let why = match waiting {
                    Waiting::Blocked(Blocked::Macs) => "Macs Fan Control is running".to_string(),
                    Waiting::Blocked(Blocked::ProbeFailed(why)) => {
                        format!("the Macs Fan Control probe failed: {why}")
                    }
                    Waiting::Unreadable => "the fans did not read".to_string(),
                };
                self.log(
                    Level::Info,
                    &format!(
                        "fan control: stopping without a hand-back ({why}); the helper judges the fans when this connection closes"
                    ),
                );
            }
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
            // With a command taken and not yet acted on, a state reached for
            // the wish before it is not that command's answer.
            if !self.fresh {
                self.stamp();
            }
        }
        self.publish();
    }

    /// The wish shown and the command number acted on, moved together
    /// ([`Snapshot`]).
    const fn stamp(&mut self) {
        self.shown_desired = self.desired;
        self.acted = self.taken;
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
fn target_for(mode: FanDesired, fan: &FanReading, celsius: Option<f64>, hot: bool) -> Option<f64> {
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

/// One tick's writes, worked out before any is sent.
#[derive(Debug, Clone, PartialEq)]
struct Round {
    /// The hot guard after this reading.
    hot: bool,
    /// Each held fan past the write gate, and its target, in reading order.
    writes: Vec<(FanIndex, f64)>,
    /// A held fan's target needs its range, and the range did not read.
    limits_unknown: bool,
}

/// Each held fan's target, and which pass the gate. Pure: a fan's previous
/// target never depends on another fan's write, so planning them all before
/// the first write is what writing them one by one did. `forced`: a new
/// command, so the gate does not hold a changed target back.
fn plan_round(
    desired: FanDesired,
    reading: &HeatReading,
    hold: Option<&Hold>,
    was_hot: bool,
    forced: bool,
) -> Round {
    let hot = desired.has_hot_guard() && guard(was_hot, reading.cpu_celsius);
    let flipped = was_hot != hot;
    let mut round = Round {
        hot,
        writes: Vec::new(),
        limits_unknown: false,
    };
    for fan in &reading.fans {
        let Some(previous) = hold
            .and_then(|h| h.fans.iter().find(|held| held.fan == fan.index))
            .map(|held| held.written_rpm)
        else {
            continue;
        };
        let Some(target) = target_for(desired, fan, reading.cpu_celsius, hot) else {
            round.limits_unknown = true;
            continue;
        };
        if should_write(previous, target, fan, desired, flipped || forced) {
            round.writes.push((fan.index, target));
        }
    }
    round
}

/// The write gate. `forced`: a new command or a hot-guard flip.
fn should_write(
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

#[cfg(test)]
mod tests;
