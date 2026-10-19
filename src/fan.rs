//! Fan control in the proxy: the `tcr-fan` thread that runs
//! [`tcr_fan::controller::Controller`], the file that remembers the wish
//! across a restart, and the [`FanHandle`] the `/_tcr/fan` route and the
//! status payload use.
//!
//! # One thread, one channel
//!
//! The controller runs on its own std thread, never on a tokio worker: its
//! helper calls wait synchronously (up to 15 s for a set-manual), and the
//! design forbids XPC on the runtime. The route sends a [`Command`] into the
//! controller's one channel and answers at once; XPC's connection events go
//! into the same channel, tagged with the connection's generation.
//!
//! # What the status shows
//!
//! [`FanHandle`] keeps the last [`Snapshot`] the controller published, wish
//! included. The handle does not write the wish as it sends: a wish written
//! ahead of the controller would pair the new mode with a state the
//! controller reached for the old one (fans going back to automatic, Macs
//! Fan Control starting) and `tcr fan` would judge that state as its
//! answer. So a status read right after `POST /_tcr/fan` can still name the
//! old mode, until the controller acts on the new one.
//!
//! The handle numbers each command it sends, in channel order, and the POST
//! answers with that number ([`FanHandle::send`], read as [`FanAccepted`]),
//! beside the object from before the command and `sinceMs`, which 1.2.12's
//! `tcr fan` reads. The status carries the number
//! of the newest command the controller acted on, beside the wish it acted on,
//! so `tcr fan` waits until the number reaches its own, then reads the wish: its
//! own is its answer, any other means a later command replaced it.
//!
//! # Persistence
//!
//! `fan.json` in the cache directory ([`crate::paths::CacheDir::fan_file`]),
//! through [`crate::cache_file`]: the wish and the hold record (which fans
//! this process put in manual, what it wrote, its pid). Not the config file:
//! that holds credentials, is a boot-time snapshot, and a slider would churn
//! it. A missing or unreadable file is Auto with no hold, said once.
//!
//! # Fail closed
//!
//! The proxy dials the helper only when its own signature is one of the two
//! fan identities (`tcr_fan::identity`). A standalone or ad-hoc `tcr` reports
//! `unavailable / notSignedForFans` the first time a manual mode is asked for,
//! and never dials. Off macOS there is no SMC, so the state is
//! `unavailable / noSmc` from the start.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tcr_fan::controller::{
    Clock, Command, Controller, FanDesired, FollowCurve, ForeignHolder, Hold, Input, Level,
    ModeError, Parts, Rpm, Saved, Snapshot, State, Store, Unavailable,
};
use tcr_fan::holder::HeldFan;
use tcr_smc::FanIndex;
use tcr_status_wire::{FanControlRow, FanDesiredRow, FanReasonRow, FanStateRow, FanTargetRow};

use crate::cache_file::{CacheRead, Layout, read_cache, write_cache};

/// `fan.json`'s format version.
pub const FAN_FILE_VERSION: u32 = 1;

/// How long a graceful stop gives the controller to hand back, inside
/// `server::DEFAULT_SHUTDOWN_GRACE`. Past it, the closing connection makes
/// root hand back on its own.
pub const SHUTDOWN_HAND_BACK: Duration = Duration::from_secs(3);

// The file.

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FanFile {
    version: u32,
    saved_at_ms: i64,
    desired: FanDesiredRow,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hold: Option<HoldFile>,
    /// The newest command number the writer had given out when it saved, so
    /// the next process numbers above it ([`commands_before`]). Absent from a
    /// file an older tcr wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HoldFile {
    writer_pid: u32,
    fans: Vec<HeldFanFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
struct HeldFanFile {
    fan: u8,
    /// `None` between set-manual and the first set-target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rpm: Option<f64>,
}

impl FanFile {
    fn of(saved: &Saved, saved_at_ms: i64, command: u64) -> Self {
        Self {
            version: FAN_FILE_VERSION,
            saved_at_ms,
            command: Some(command),
            desired: desired_row(saved.desired),
            hold: saved.hold.as_ref().map(|hold| HoldFile {
                writer_pid: hold.writer_pid,
                fans: hold
                    .fans
                    .iter()
                    .map(|f| HeldFanFile {
                        fan: f.fan.get(),
                        rpm: f.written_rpm,
                    })
                    .collect(),
            }),
        }
    }

    /// The file as the controller takes it. A wish that no longer passes
    /// today's rules, or a fan index past 9, is refused whole.
    fn parse(self) -> Result<Saved, String> {
        let desired = mode_of(self.desired).map_err(|e| format!("the saved mode: {e}"))?;
        let hold = match self.hold {
            None => None,
            Some(hold) => Some(Hold {
                writer_pid: hold.writer_pid,
                fans: hold
                    .fans
                    .into_iter()
                    .map(|f| {
                        FanIndex::new(f.fan)
                            .map(|fan| HeldFan {
                                fan,
                                written_rpm: f.rpm,
                            })
                            .ok_or_else(|| format!("fan {} in the hold record", f.fan))
                    })
                    .collect::<Result<_, _>>()?,
            }),
        };
        Ok(Saved { desired, hold })
    }
}

/// Read `fan.json`. Missing is Auto, quietly; anything else that stops it is
/// Auto and one warning.
#[cfg(test)]
fn load(path: &Path) -> Option<Saved> {
    load_file(path).0
}

/// [`load`], and the file as read, for [`FileStore`] to compare against.
fn load_file(path: &Path) -> (Option<Saved>, Option<FanFile>) {
    match read_cache::<FanFile>(path, FAN_FILE_VERSION) {
        CacheRead::Missing => (None, None),
        CacheRead::Degraded(why) => {
            tracing::warn!(path = %path.display(), "fan control: ignored the saved fan mode ({why}); starting on automatic, numbering fan commands from the clock");
            (None, None)
        }
        CacheRead::Read(file) => match file.clone().parse() {
            Ok(saved) => (Some(saved), Some(file)),
            Err(why) => {
                tracing::warn!(path = %path.display(), "fan control: ignored the saved fan mode ({why}); starting on automatic");
                (None, Some(file))
            }
        },
    }
}

/// Where a process's command numbers start: the wall clock in ms, or one past
/// the last number an earlier process saved when that is higher (the clock
/// went back, or that process gave out more numbers than milliseconds passed
/// before this start). So the counter never goes backwards across a restart,
/// and a command sent to an earlier process reads as acted on.
fn commands_before(saved: Option<u64>, clock_ms: i64) -> u64 {
    let clock = u64::try_from(clock_ms).unwrap_or(0);
    saved.map_or(clock, |last| clock.max(last.saturating_add(1)))
}

/// `fan.json`, written by one process at a time.
///
/// Across `tcr --replace` two proxies overlap: the new one's set-manual makes
/// it root's holder and it saves a record with its own pid; the old one, on
/// its way out, must not save over that (a `hold: None` from its shutdown
/// would make the next start forget fans the new one holds). So a save
/// checks the file first: when it changed since this process last read or
/// wrote it, and it now names another process as the writer of a hold, the
/// file is that process's and this one writes nothing more.
struct FileStore {
    path: PathBuf,
    pid: u32,
    /// The file as this process last read or wrote it.
    known: Option<FanFile>,
    /// Said once that the file is another process's now.
    yielded: bool,
    /// The newest command number given out, shared with [`FanHandle::sent`]:
    /// every save writes it, so the next process numbers above it.
    sent: Arc<Mutex<u64>>,
}

impl FileStore {
    /// The store for `path`, and the wish saved there. Its
    /// [`Self::sent`] starts at [`commands_before`] for the number saved
    /// there and the clock now.
    fn open(path: PathBuf, pid: u32) -> (Self, Option<Saved>) {
        let (saved, known) = load_file(&path);
        let last = known.as_ref().and_then(|file| file.command);
        let first = commands_before(last, wall_ms());
        (
            Self {
                path,
                pid,
                known,
                yielded: false,
                sent: Arc::new(Mutex::new(first)),
            },
            saved,
        )
    }

    /// Another process wrote a hold of its own since this one last looked.
    fn taken_by_another(&self) -> Option<u32> {
        let CacheRead::Read(on_disk) = read_cache::<FanFile>(&self.path, FAN_FILE_VERSION) else {
            return None;
        };
        if self.known.as_ref() == Some(&on_disk) {
            return None;
        }
        on_disk
            .hold
            .map(|hold| hold.writer_pid)
            .filter(|&writer| writer != self.pid)
    }
}

impl Store for FileStore {
    fn save(&mut self, saved: &Saved) {
        if let Some(writer) = self.taken_by_another() {
            if !self.yielded {
                self.yielded = true;
                tracing::info!(
                    path = %self.path.display(),
                    writer,
                    "fan control: the fan record now belongs to a newer tcr; this one leaves it alone"
                );
            }
            return;
        }
        let command = *self.sent.lock().unwrap_or_else(PoisonError::into_inner);
        let file = FanFile::of(saved, wall_ms(), command);
        match write_cache(&self.path, &file, Layout::Pretty) {
            Ok(()) => self.known = Some(file),
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "fan control: could not save the fan mode");
            }
        }
    }
}

// Modes at the edge.

/// A wish as the wire spells it.
pub const fn desired_row(mode: FanDesired) -> FanDesiredRow {
    match mode {
        FanDesired::Auto => FanDesiredRow::Auto,
        FanDesired::Fixed { rpm } => FanDesiredRow::Fixed { rpm: rpm.get() },
        FanDesired::FollowCpu { curve } => FanDesiredRow::FollowCpu {
            quiet_below_c: curve.quiet_below_c(),
            full_at_c: curve.full_at_c(),
        },
    }
}

/// A wish from the wire, checked.
pub fn mode_of(row: FanDesiredRow) -> Result<FanDesired, ModeError> {
    Ok(match row {
        FanDesiredRow::Auto => FanDesired::Auto,
        FanDesiredRow::Fixed { rpm } => FanDesired::Fixed {
            rpm: Rpm::new(rpm)?,
        },
        FanDesiredRow::FollowCpu {
            quiet_below_c,
            full_at_c,
        } => FanDesired::FollowCpu {
            curve: FollowCurve::new(quiet_below_c, full_at_c)?,
        },
    })
}

/// The body of `POST /_tcr/fan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum FanRequest {
    Auto {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    Fixed {
        rpm: u32,
    },
    FollowCpu {
        quiet_below_c: u8,
        full_at_c: u8,
    },
}

impl FanRequest {
    /// The command it asks for, or why it is refused.
    pub fn command(self) -> Result<Command, ModeError> {
        match self {
            Self::Auto { force: true } => Ok(Command::ForceHandBack),
            other => mode_of(other.desired_row()).map(Command::Set),
        }
    }

    /// The wish this request sets, as the status spells it. `auto --force`
    /// sets Auto too.
    pub const fn desired_row(self) -> FanDesiredRow {
        match self {
            Self::Auto { .. } => FanDesiredRow::Auto,
            Self::Fixed { rpm } => FanDesiredRow::Fixed { rpm },
            Self::FollowCpu {
                quiet_below_c,
                full_at_c,
            } => FanDesiredRow::FollowCpu {
                quiet_below_c,
                full_at_c,
            },
        }
    }

    /// The state that means this request is done: holding for a manual
    /// mode, off for automatic.
    pub const fn settled_state(self) -> FanStateRow {
        match self {
            Self::Auto { .. } => FanStateRow::Off,
            Self::Fixed { .. } | Self::FollowCpu { .. } => FanStateRow::Holding,
        }
    }
}

/// What `tcr fan` reads of the 202 of `POST /_tcr/fan`: the number the command
/// was sent under, which [`FanControlRow::command`] reaches once the
/// controller has acted on it. The body is a whole `fanControl` object
/// ([`FanHandle::send`]); a bare `{"command": n}` reads the same. Every other
/// key is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct FanAccepted {
    /// Always written by this build. `None` only when reading the answer of a
    /// server older than the command counter (1.2.12), whose `fanControl`
    /// object has no `command`.
    #[serde(default)]
    pub command: Option<u64>,
}

// The status object.

/// The `fanControl` object for a snapshot.
pub fn row(snapshot: &Snapshot) -> FanControlRow {
    let (state, reason) = match snapshot.state {
        State::Off => (FanStateRow::Off, None),
        State::Entering => (FanStateRow::Entering, None),
        State::Holding => (FanStateRow::Holding, None),
        State::Releasing => (FanStateRow::Releasing, None),
        State::Foreign(ForeignHolder::MacsFanControl) => {
            (FanStateRow::Foreign, Some(FanReasonRow::MacsFanControl))
        }
        State::Foreign(ForeignHolder::AnotherApp) => {
            (FanStateRow::Foreign, Some(FanReasonRow::AnotherApp))
        }
        State::Unknown => (FanStateRow::Unknown, None),
        State::Unavailable(why) => (FanStateRow::Unavailable, Some(reason_row(why))),
        State::Stuck => (FanStateRow::Stuck, None),
    };
    FanControlRow {
        desired: desired_row(snapshot.desired),
        state,
        reason,
        hot_guard: snapshot.hot_guard,
        targets: snapshot
            .targets
            .iter()
            .map(|&(fan, rpm)| FanTargetRow {
                fan: fan.get(),
                rpm,
            })
            .collect(),
        detail: detail_text(snapshot),
        command: snapshot.command,
        // For 1.2.12's `tcr fan`, which requires `sinceMs` and waits on it.
        // Remove it after the release after next, once no client that old is
        // left; nothing in this build reads it.
        since_ms: Some(snapshot.since_ms),
    }
}

const fn reason_row(why: Unavailable) -> FanReasonRow {
    match why {
        Unavailable::NotSignedForFans => FanReasonRow::NotSignedForFans,
        Unavailable::NoSmc => FanReasonRow::NoSmc,
        Unavailable::NoFans => FanReasonRow::NoFans,
        Unavailable::HelperUnreachable => FanReasonRow::HelperUnreachable,
        Unavailable::HelperNoReply => FanReasonRow::HelperNoReply,
        Unavailable::HelperSignature => FanReasonRow::HelperSignature,
        Unavailable::HelperVersion { installed } => FanReasonRow::HelperVersion { installed },
        Unavailable::ModeLocked { fan } => FanReasonRow::ModeLocked(fan),
        Unavailable::Contended => FanReasonRow::Contended,
        Unavailable::HelperRefused(_) => FanReasonRow::HelperRefused,
        Unavailable::ControllerStopped => FanReasonRow::ControllerStopped,
    }
}

/// The controller's own detail, else what a reason with fields says.
fn detail_text(snapshot: &Snapshot) -> Option<String> {
    if let Some(detail) = snapshot.detail {
        return Some(detail.to_string());
    }
    match snapshot.state {
        State::Unavailable(Unavailable::ModeLocked { fan }) => {
            Some(format!("fan {fan} would not leave automatic"))
        }
        State::Unavailable(Unavailable::HelperVersion {
            installed: Some(protocol),
        }) => Some(format!(
            "the installed helper speaks protocol {protocol}; this tcr speaks {}",
            tcr_fan::wire::PROTOCOL_VERSION
        )),
        State::Unavailable(Unavailable::HelperRefused(error)) => {
            Some(format!("the helper refused: {error}"))
        }
        State::Unavailable(Unavailable::ControllerStopped) => Some(
            "the fan controller stopped on an internal error; restart tcr to control the fans again"
                .to_string(),
        ),
        _ => None,
    }
}

// The handle.

/// The controller's thread stopped: nothing receives a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotRunning;

/// The route's and the status payload's way to the controller.
pub struct FanHandle {
    tx: Sender<Input>,
    /// The controller's last snapshot, wish included.
    shown: Arc<Mutex<Snapshot>>,
    /// The number of the last command sent, shared with the store so every
    /// save persists it. Held across the send, so numbers follow channel
    /// order, which the controller acts in.
    sent: Arc<Mutex<u64>>,
    /// Hears once when the thread's loop returns.
    done: Mutex<mpsc::Receiver<()>>,
}

impl FanHandle {
    /// Send `command` under the next number, and answer with the route's 202
    /// body: the `fanControl` object as it stood when the command went in,
    /// before the controller acted on it, with `command` the number this
    /// command was given. The status's [`FanControlRow::command`] reaches that
    /// number once the controller has acted on it; this build's `tcr fan` reads
    /// only that ([`FanAccepted`]). The rest is 1.2.12's answer, whose
    /// `tcr fan` keeps its `desired` to tell "not taken yet" from "replaced",
    /// so `shown` is held across the send and nothing the controller publishes
    /// after taking this command can be in it.
    pub fn send(&self, command: Command) -> Result<FanControlRow, NotRunning> {
        let mut sent = self.sent.lock().unwrap_or_else(PoisonError::into_inner);
        let shown = self.shown.lock().unwrap_or_else(PoisonError::into_inner);
        let seq = *sent + 1;
        self.tx
            .send(Input::Command { seq, command })
            .map_err(|_gone| NotRunning)?;
        *sent = seq;
        Ok(FanControlRow {
            command: seq,
            ..row(&shown)
        })
    }

    /// The `fanControl` object as of now.
    pub fn row(&self) -> FanControlRow {
        row(&self.shown.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Ask the controller to hand back what it holds and stop, and wait at
    /// most `grace` for it. Blocking: call it off the runtime.
    pub fn shutdown(&self, grace: Duration) {
        let wait = grace.min(SHUTDOWN_HAND_BACK);
        let deadline = Instant::now() + wait;
        // Not a numbered command: the controller does not count a shutdown.
        let seq = *self.sent.lock().unwrap_or_else(PoisonError::into_inner);
        let command = Command::Shutdown { deadline };
        if self.tx.send(Input::Command { seq, command }).is_err() {
            return;
        }
        let done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        if done
            .recv_timeout(wait + Duration::from_millis(500))
            .is_err()
        {
            tracing::warn!(
                "fan control: the controller did not stop in time; the helper hands back when this process exits"
            );
        }
    }
}

/// Start the controller on its thread, from the wish saved at `file`. The
/// binary's call, once the server is bound.
pub fn start(file: PathBuf) -> std::io::Result<Arc<FanHandle>> {
    let (store, saved) = FileStore::open(file, std::process::id());
    let desired = saved.as_ref().map_or(FanDesired::Auto, |s| s.desired);
    let sent = Arc::clone(&store.sent);
    let commands_before = *sent.lock().unwrap_or_else(PoisonError::into_inner);
    let (tx, rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let shown = Arc::new(Mutex::new(Snapshot {
        desired,
        state: State::Off,
        hot_guard: false,
        targets: Vec::new(),
        detail: None,
        command: commands_before,
        since_ms: wall_ms(),
    }));
    let published = Arc::clone(&shown);
    let after_panic = Arc::clone(&shown);
    let events = tx.clone();
    std::thread::Builder::new()
        .name("tcr-fan".into())
        .spawn(move || {
            run_guarded(
                move || {
                    let parts = Parts {
                        helper: platform::helper(events),
                        heat: ProcessHeat(crate::heat::process()),
                        probe: platform::probe(),
                        clock: SystemClock,
                        store,
                        inbox: rx,
                        log: Box::new(log_line),
                        publish: Box::new(move |snapshot: &Snapshot| {
                            *published.lock().unwrap_or_else(PoisonError::into_inner) =
                                snapshot.clone();
                        }),
                        pid: std::process::id(),
                        commands_before,
                    };
                    Controller::new(parts, saved).run();
                },
                &after_panic,
            );
            if done_tx.send(()).is_err() {
                // Nobody waits on the stop: the handle is gone.
            }
        })?;
    Ok(Arc::new(FanHandle {
        tx,
        shown,
        sent,
        done: Mutex::new(done_rx),
    }))
}

/// Run the controller; a panic on its thread publishes
/// `unavailable / controllerStopped` instead of leaving the last snapshot
/// (say, "holding") standing for a controller that is gone. The panic drops
/// the helper connection on its way out, so root judges the fans and hands
/// back what is still its doing.
fn run_guarded(run: impl FnOnce(), shown: &Mutex<Snapshot>) {
    let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) else {
        return;
    };
    let why = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_string());
    tracing::error!(%why, "fan control: the controller stopped; restart tcr to control the fans again");
    let mut shown = shown.lock().unwrap_or_else(PoisonError::into_inner);
    *shown = Snapshot {
        desired: shown.desired,
        state: State::Unavailable(Unavailable::ControllerStopped),
        hot_guard: false,
        targets: Vec::new(),
        detail: None,
        command: shown.command,
        since_ms: wall_ms(),
    };
}

/// A handle on a real controller thread that can reach nothing: no SMC, no
/// helper, no file. For the route's tests, which check the HTTP contract and
/// must never touch the machine's fans.
#[cfg(test)]
pub(crate) fn start_inert() -> Arc<FanHandle> {
    let (handle, controller) = inert(None);
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        controller.run();
        done_tx.send(()).ok();
    });
    *handle.done.lock().unwrap() = done_rx;
    handle
}

#[cfg(test)]
type Inert = Controller<inert::NoHelper, inert::NoSmc, inert::NoMacs, SystemClock, inert::Nowhere>;

/// A handle and the controller behind it, not running: a test steps it by
/// hand, so it can land a step for the old wish between a send and the
/// controller taking it.
#[cfg(test)]
fn inert(saved: Option<Saved>) -> (Arc<FanHandle>, Inert) {
    let (tx, rx) = mpsc::channel();
    let (_never, done_rx) = mpsc::channel();
    let shown = Arc::new(Mutex::new(Snapshot {
        desired: saved.as_ref().map_or(FanDesired::Auto, |s| s.desired),
        state: State::Off,
        hot_guard: false,
        targets: Vec::new(),
        detail: None,
        command: 0,
        since_ms: 0,
    }));
    let published = Arc::clone(&shown);
    let parts = Parts {
        helper: inert::NoHelper,
        heat: inert::NoSmc,
        probe: inert::NoMacs,
        clock: SystemClock,
        store: inert::Nowhere,
        inbox: rx,
        log: Box::new(|_, _| {}),
        publish: Box::new(move |snapshot: &Snapshot| {
            *published.lock().unwrap() = snapshot.clone();
        }),
        pid: std::process::id(),
        commands_before: 0,
    };
    let controller = Controller::new(parts, saved);
    let handle = Arc::new(FanHandle {
        tx,
        shown,
        sent: Arc::new(Mutex::new(0)),
        done: Mutex::new(done_rx),
    });
    (handle, controller)
}

#[cfg(test)]
mod inert {
    use std::time::Duration;

    use tcr_fan::controller::{Fault, Generation, Heat, Helper, Probe, Saved, Store};
    use tcr_fan::wire::{Reply, Request};

    pub(super) struct NoSmc;
    impl Heat for NoSmc {
        fn read(&mut self) -> Option<tcr_smc::HeatReading> {
            None
        }
    }
    pub(super) struct NoHelper;
    impl Helper for NoHelper {
        fn open(&mut self, _: Generation) -> Result<(), Fault> {
            Err(Fault::NotSignedForFans)
        }
        fn send(&mut self, _: &Request, _: Duration) -> Result<Reply, Fault> {
            Err(Fault::NotSignedForFans)
        }
        fn close(&mut self) {}
    }
    pub(super) struct NoMacs;
    impl Probe for NoMacs {
        fn macs_running(&mut self) -> Result<bool, String> {
            Ok(false)
        }
    }
    pub(super) struct Nowhere;
    impl Store for Nowhere {
        fn save(&mut self, _: &Saved) {}
    }
}

fn log_line(level: Level, line: &str) {
    match level {
        Level::Debug => tracing::debug!("{line}"),
        Level::Info => tracing::info!("{line}"),
        Level::Warn => tracing::warn!("{line}"),
    }
}

fn wall_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall_ms(&self) -> i64 {
        wall_ms()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The process's one heat reader ([`crate::heat::process`]): the tick's read
/// is fresh, and answers the next status request too.
struct ProcessHeat(Arc<crate::heat::HeatCache<tcr_smc::HeatReading>>);

impl tcr_fan::controller::Heat for ProcessHeat {
    fn read(&mut self) -> Option<tcr_smc::HeatReading> {
        self.0.refresh(Instant::now())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    use tcr_fan::controller::{Fault, Generation, Helper, Input, Probe};
    use tcr_fan::identity::{FanIdentity, NotSignedForFans, Role};
    use tcr_fan::macs::MacsProbe;
    use tcr_fan::wire::{Reply, Request};
    use tcr_fan::xpc::{Client, XpcError};

    pub(super) const fn helper(events: Sender<Input>) -> XpcHelper {
        XpcHelper {
            identity: None,
            client: None,
            events,
        }
    }

    pub(super) fn probe() -> Macs {
        Macs(MacsProbe::new())
    }

    /// The proxy's end of the helper connection.
    pub(super) struct XpcHelper {
        /// Decided from this binary's own signature on first use, then kept.
        identity: Option<Result<FanIdentity, NotSignedForFans>>,
        client: Option<Client>,
        events: Sender<Input>,
    }

    const fn fault(error: &XpcError) -> Fault {
        match error {
            XpcError::Invalid | XpcError::BadName | XpcError::NoObject(_) => Fault::Invalid,
            XpcError::Interrupted | XpcError::TerminationImminent => Fault::Interrupted,
            XpcError::PeerCodeSigningRequirement => Fault::PeerCodeSigningRequirement,
            XpcError::Requirement(_) => Fault::Requirement,
            XpcError::NoReply(_) => Fault::NoReply,
            XpcError::Reply(_) | XpcError::Unexpected => Fault::Malformed,
        }
    }

    impl Helper for XpcHelper {
        fn open(&mut self, generation: Generation) -> Result<(), Fault> {
            self.client = None;
            let identity = self
                .identity
                .get_or_insert_with(|| FanIdentity::of_this_process(Role::Client));
            let identity = match identity {
                Ok(identity) => *identity,
                Err(why) => {
                    tracing::debug!("fan control: not dialling the helper: {why}");
                    return Err(Fault::NotSignedForFans);
                }
            };
            let events = self.events.clone();
            let client = Client::connect(
                identity.helper_label(),
                identity.helper_requirement(),
                move |error| {
                    // Tagged with this connection's generation, so the
                    // controller drops it once a newer one replaced it, even
                    // when it arrives after this client was dropped.
                    if events
                        .send(Input::Xpc {
                            generation,
                            fault: fault(&error),
                        })
                        .is_err()
                    {
                        // The controller has stopped; nothing to tell.
                    }
                },
            )
            .map_err(|error| {
                tracing::warn!(%error, "fan control: could not open the helper connection");
                fault(&error)
            })?;
            self.client = Some(client);
            Ok(())
        }

        fn send(&mut self, request: &Request, deadline: Duration) -> Result<Reply, Fault> {
            let Some(client) = self.client.as_ref() else {
                return Err(Fault::Invalid);
            };
            client.send(request, deadline).map_err(|e| fault(&e))
        }

        fn close(&mut self) {
            self.client = None;
        }
    }

    pub(super) struct Macs(MacsProbe);

    impl Probe for Macs {
        fn macs_running(&mut self) -> Result<bool, String> {
            self.0.running().map_err(|e| e.to_string())
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    use tcr_fan::controller::{Fault, Generation, Helper, Input, Probe};
    use tcr_fan::wire::{Reply, Request};

    pub(super) fn helper(_events: Sender<Input>) -> NoHelper {
        NoHelper
    }

    pub(super) const fn probe() -> NoMacs {
        NoMacs
    }

    /// Off macOS there is no helper to dial.
    pub(super) struct NoHelper;

    impl Helper for NoHelper {
        fn open(&mut self, _generation: Generation) -> Result<(), Fault> {
            Err(Fault::NotSignedForFans)
        }

        fn send(&mut self, _request: &Request, _deadline: Duration) -> Result<Reply, Fault> {
            Err(Fault::NotSignedForFans)
        }

        fn close(&mut self) {}
    }

    /// Macs Fan Control is a macOS app.
    pub(super) struct NoMacs;

    impl Probe for NoMacs {
        fn macs_running(&mut self) -> Result<bool, String> {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcr_fan::controller::Detail;

    fn fixture() -> serde_json::Map<String, serde_json::Value> {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fan-control-contract.json");
        match serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap() {
            serde_json::Value::Object(samples) => samples,
            other => panic!("{other}"),
        }
    }

    fn fan(index: u8) -> FanIndex {
        FanIndex::new(index).unwrap()
    }

    fn snapshot(state: State, targets: &[(u8, f64)], detail: Option<Detail>) -> Snapshot {
        Snapshot {
            desired: FanDesired::Auto,
            state,
            hot_guard: false,
            targets: targets.iter().map(|&(i, rpm)| (fan(i), rpm)).collect(),
            detail,
            command: 1_760_000_000_000,
            since_ms: 1_760_000_000_000,
        }
    }

    /// The row for `snapshot` with `desired` as its wish.
    fn row_with(desired: FanDesired, snapshot: Snapshot) -> FanControlRow {
        row(&Snapshot {
            desired,
            ..snapshot
        })
    }

    /// What this code emits for each state TcrBar draws is, value for value,
    /// the committed contract sample.
    #[test]
    fn fan_status_rows_match_the_committed_contract() {
        let samples = fixture();
        let fixed = FanDesired::Fixed {
            rpm: Rpm::new(5000).unwrap(),
        };
        let follow = FanDesired::FollowCpu {
            curve: FollowCurve::new(60, 90).unwrap(),
        };
        let cases = [
            ("off", FanDesired::Auto, snapshot(State::Off, &[], None)),
            (
                "holdingFixed5000",
                fixed,
                snapshot(State::Holding, &[(0, 5000.0), (1, 5000.0)], None),
            ),
            (
                "holdingFollow",
                follow,
                snapshot(State::Holding, &[(0, 5777.0), (1, 5777.0)], None),
            ),
            (
                "foreignMacsFanControl",
                fixed,
                snapshot(State::Foreign(ForeignHolder::MacsFanControl), &[], None),
            ),
            (
                "foreignMacsFanControlWaiting",
                FanDesired::Auto,
                snapshot(
                    State::Foreign(ForeignHolder::MacsFanControl),
                    &[(0, 5000.0), (1, 5000.0)],
                    Some(Detail::WaitingForMacs),
                ),
            ),
            (
                "unavailableModeLocked",
                fixed,
                snapshot(
                    State::Unavailable(Unavailable::ModeLocked { fan: 1 }),
                    &[],
                    None,
                ),
            ),
            (
                "unavailableHelperVersion",
                fixed,
                snapshot(
                    State::Unavailable(Unavailable::HelperVersion { installed: Some(2) }),
                    &[],
                    None,
                ),
            ),
            (
                "unavailableHelperUnreachable",
                fixed,
                snapshot(
                    State::Unavailable(Unavailable::HelperUnreachable),
                    &[],
                    None,
                ),
            ),
        ];
        assert_eq!(cases.len(), samples.len(), "every sample is checked");
        for (name, desired, snapshot) in cases {
            assert_eq!(
                serde_json::to_value(row_with(desired, snapshot)).unwrap(),
                samples[name],
                "{name}"
            );
        }
    }

    #[test]
    fn fan_status_reasons_with_fields_get_a_detail() {
        let locked = row_with(
            FanDesired::Auto,
            snapshot(
                State::Unavailable(Unavailable::ModeLocked { fan: 1 }),
                &[],
                None,
            ),
        );
        assert_eq!(locked.reason, Some(FanReasonRow::ModeLocked(1)));
        assert_eq!(
            locked.detail.as_deref(),
            Some("fan 1 would not leave automatic")
        );
        let wire = serde_json::to_value(&locked).unwrap();
        assert_eq!(wire["reason"], serde_json::json!({"modeLocked": 1}));
        let version = row_with(
            FanDesired::Auto,
            snapshot(
                State::Unavailable(Unavailable::HelperVersion { installed: Some(2) }),
                &[],
                None,
            ),
        );
        assert_eq!(
            serde_json::to_value(&version).unwrap()["reason"],
            serde_json::json!({"helperVersion": {"installed": 2}})
        );
    }

    /// The three states the popover's decoder was written against, byte for byte as
    /// TcrBar's side spelled them (Off, Holding at fixed 5000, Foreign to Macs Fan
    /// Control while Auto waits).
    #[test]
    fn fan_status_serializes_exactly_what_the_popover_decodes() {
        let at = |state, targets: &[(u8, f64)], detail| Snapshot {
            command: 0,
            since_ms: 0,
            ..snapshot(state, targets, detail)
        };
        let fixed = FanDesired::Fixed {
            rpm: Rpm::new(5000).unwrap(),
        };
        let cases = [
            (
                FanDesired::Auto,
                at(State::Off, &[], None),
                r#"{"desired":{"mode":"auto"},"state":"off","reason":null,"hotGuard":false,"targets":[],"detail":null,"command":0,"sinceMs":0}"#,
            ),
            (
                fixed,
                at(State::Holding, &[(0, 5000.0), (1, 5000.0)], None),
                r#"{"desired":{"mode":"fixed","rpm":5000},"state":"holding","reason":null,"hotGuard":false,"targets":[{"fan":0,"rpm":5000.0},{"fan":1,"rpm":5000.0}],"detail":null,"command":0,"sinceMs":0}"#,
            ),
            (
                FanDesired::Auto,
                at(
                    State::Foreign(ForeignHolder::MacsFanControl),
                    &[],
                    Some(Detail::WaitingForMacs),
                ),
                r#"{"desired":{"mode":"auto"},"state":"foreign","reason":"macsFanControl","hotGuard":false,"targets":[],"detail":"fans go back to automatic when Macs Fan Control quits","command":0,"sinceMs":0}"#,
            ),
        ];
        for (desired, snapshot, expected) in cases {
            assert_eq!(
                serde_json::to_string(&row_with(desired, snapshot)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn fan_route_bodies_parse_exactly() {
        let parse = |body: &str| serde_json::from_str::<FanRequest>(body);
        assert_eq!(
            parse(r#"{"mode":"auto"}"#).unwrap(),
            FanRequest::Auto { force: false }
        );
        assert_eq!(
            parse(r#"{"mode":"auto","force":true}"#).unwrap(),
            FanRequest::Auto { force: true }
        );
        assert_eq!(
            parse(r#"{"mode":"fixed","rpm":5000}"#).unwrap(),
            FanRequest::Fixed { rpm: 5000 }
        );
        assert_eq!(
            parse(r#"{"mode":"followCpu","quietBelowC":60,"fullAtC":90}"#).unwrap(),
            FanRequest::FollowCpu {
                quiet_below_c: 60,
                full_at_c: 90
            }
        );
        for refused in [
            r#"{"mode":"fixed","rpm":5000,"extra":1}"#,
            r#"{"mode":"fixed","rpm":5000.5}"#,
            r#"{"mode":"fixed","rpm":-1}"#,
            r#"{"mode":"turbo"}"#,
            r#"{"mode":"followCpu","quietBelowC":60}"#,
            r#"{"rpm":5000}"#,
        ] {
            assert!(parse(refused).is_err(), "{refused}");
        }
        assert_eq!(
            FanRequest::Fixed { rpm: 0 }.command(),
            Err(ModeError::RpmOutOfRange { rpm: 0 })
        );
        assert_eq!(
            FanRequest::FollowCpu {
                quiet_below_c: 60,
                full_at_c: 101
            }
            .command(),
            Err(ModeError::FullAboveGuard { full_at: 101 })
        );
        assert_eq!(
            FanRequest::Auto { force: true }.command(),
            Ok(Command::ForceHandBack)
        );
    }

    /// What `fanControl` adds to a warm status request: the handle's copy and
    /// its JSON. A measurement, run by hand with `--ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn fan_status_cost_on_a_warm_status_request() {
        let handle = start_inert();
        let n: u32 = 20_000;
        let started = Instant::now();
        let mut bytes = 0;
        for _ in 0..n {
            bytes += serde_json::to_string(&handle.row()).unwrap().len();
        }
        let per = started.elapsed() / n;
        println!(
            "fanControl on a status request: {:.2} us wall per request, {} bytes",
            per.as_secs_f64() * 1e6,
            bytes / n as usize
        );
        handle.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn fan_file_round_trips_and_refuses_what_today_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fan.json");
        assert_eq!(load(&path), None, "missing is automatic");
        let saved = Saved {
            desired: FanDesired::Fixed {
                rpm: Rpm::new(5000).unwrap(),
            },
            hold: Some(Hold {
                writer_pid: 4242,
                fans: vec![
                    HeldFan {
                        fan: fan(0),
                        written_rpm: Some(5000.0),
                    },
                    HeldFan {
                        fan: fan(1),
                        written_rpm: None,
                    },
                ],
            }),
        };
        FileStore::open(path.clone(), 4242).0.save(&saved);
        assert_eq!(load(&path), Some(saved));
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["desired"],
            serde_json::json!({"mode": "fixed", "rpm": 5000})
        );
        assert_eq!(value["hold"]["writerPid"], 4242);
        assert_eq!(
            value["hold"]["fans"],
            serde_json::json!([{"fan": 0, "rpm": 5000.0}, {"fan": 1}])
        );

        std::fs::write(
            &path,
            r#"{"version":1,"savedAtMs":0,"desired":{"mode":"fixed","rpm":99999}}"#,
        )
        .unwrap();
        assert_eq!(load(&path), None, "an out-of-range wish is not obeyed");
        std::fs::write(&path, "{").unwrap();
        assert_eq!(load(&path), None);
    }

    fn held_by(pid: u32, hold: bool) -> Saved {
        Saved {
            desired: FanDesired::Fixed {
                rpm: Rpm::new(5000).unwrap(),
            },
            hold: hold.then(|| Hold {
                writer_pid: pid,
                fans: vec![HeldFan {
                    fan: fan(0),
                    written_rpm: Some(5000.0),
                }],
            }),
        }
    }

    /// `tcr --replace`: the new process takes the record over and writes its
    /// own; the old one's shutdown, clearing its hold, must not save over it.
    #[test]
    fn fan_file_an_old_process_never_overwrites_a_newer_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fan.json");
        let (mut old, _) = FileStore::open(path.clone(), 100);
        old.save(&held_by(100, true));

        let (mut new, saved) = FileStore::open(path.clone(), 200);
        assert_eq!(saved, Some(held_by(100, true)));
        new.save(&held_by(200, true));

        old.save(&held_by(100, false));
        assert_eq!(
            load(&path),
            Some(held_by(200, true)),
            "the newer record stands"
        );
        old.save(&held_by(100, true));
        assert_eq!(load(&path), Some(held_by(200, true)));

        // The newer process still writes its own file.
        new.save(&held_by(200, false));
        assert_eq!(load(&path), Some(held_by(200, false)));
    }

    /// A record a process now gone left behind, unchanged since this one
    /// read it at boot, is this one's to clear (the boot drops a record the
    /// fans no longer match).
    #[test]
    fn fan_file_a_record_from_a_process_gone_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fan.json");
        FileStore::open(path.clone(), 100)
            .0
            .save(&held_by(100, true));
        let (mut next, _) = FileStore::open(path.clone(), 300);
        next.save(&held_by(300, false));
        assert_eq!(load(&path), Some(held_by(300, false)));
    }

    /// The command counter never goes backwards across a restart: a number
    /// saved above the clock (the clock went back, or many commands in a
    /// short run) wins, and the next process numbers past it.
    #[test]
    fn fan_file_a_saved_command_number_above_the_clock_wins() {
        assert_eq!(commands_before(None, 1_000), 1_000);
        assert_eq!(commands_before(Some(5), 1_000), 1_000);
        assert_eq!(commands_before(Some(5_000), 1_000), 5_001);
        assert_eq!(commands_before(None, -1), 0, "a clock before 1970");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fan.json");
        let ahead = u64::try_from(wall_ms()).unwrap() + 3_600_000;
        let (mut first, _) = FileStore::open(path.clone(), 100);
        *first.sent.lock().unwrap() = ahead;
        first.save(&held_by(100, false));
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["command"], ahead, "{text}");

        let (next, saved) = FileStore::open(path.clone(), 200);
        assert_eq!(saved, Some(held_by(100, false)));
        assert_eq!(*next.sent.lock().unwrap(), ahead + 1);

        // A file an older tcr wrote, with no number, and an unreadable one:
        // the clock.
        std::fs::write(
            &path,
            r#"{"version":1,"savedAtMs":0,"desired":{"mode":"auto"}}"#,
        )
        .unwrap();
        let before = u64::try_from(wall_ms()).unwrap();
        let (old, _) = FileStore::open(path.clone(), 300);
        assert!((before..ahead).contains(&*old.sent.lock().unwrap()));
        std::fs::write(&path, "{").unwrap();
        let (unreadable, saved) = FileStore::open(path, 300);
        assert_eq!(saved, None);
        assert!((before..ahead).contains(&*unreadable.sent.lock().unwrap()));
    }

    /// A panic on the controller's thread leaves `unavailable /
    /// controllerStopped`, never the last state it published.
    #[test]
    fn fan_status_a_panicked_controller_reads_unavailable() {
        let fixed = FanDesired::Fixed {
            rpm: Rpm::new(5000).unwrap(),
        };
        let holding = || {
            Mutex::new(Snapshot {
                desired: fixed,
                ..snapshot(State::Holding, &[(0, 5000.0), (1, 5000.0)], None)
            })
        };
        let shown = holding();
        run_guarded(|| panic!("a controller bug"), &shown);
        let after = row(&shown.lock().unwrap());
        assert_eq!(
            after.desired,
            FanDesiredRow::Fixed { rpm: 5000 },
            "the wish stays"
        );
        assert_eq!(after.state, FanStateRow::Unavailable);
        assert_eq!(after.reason, Some(FanReasonRow::ControllerStopped));
        assert_eq!(after.targets, vec![]);
        assert!(after.detail.is_some_and(|d| d.contains("restart tcr")));

        // A run that returns leaves what it published.
        let shown = holding();
        run_guarded(|| {}, &shown);
        assert_eq!(shown.lock().unwrap().state, State::Holding);
    }
    /// The race the first cut of `tcr fan` could lose: a state change made
    /// for the old wish lands after `POST /_tcr/fan` sent the new one and
    /// before the controller took it. No status read may pair the new wish
    /// with that state; the wish shows once the controller acted on it.
    #[test]
    fn fan_status_never_pairs_a_new_wish_with_the_old_wishs_state() {
        let fixed = FanDesired::Fixed {
            rpm: Rpm::new(5000).unwrap(),
        };
        let (handle, mut controller) = inert(Some(Saved {
            desired: fixed,
            hold: None,
        }));
        let before = handle.row();
        assert_eq!(before.desired, FanDesiredRow::Fixed { rpm: 5000 });
        assert_eq!(before.state, FanStateRow::Unavailable, "no SMC here");

        let answer = handle.send(Command::Set(FanDesired::Auto)).unwrap();
        let mine = answer.command;
        assert!(
            mine > before.command,
            "the POST answers with a number the status has not reached"
        );
        assert_eq!(
            FanControlRow {
                command: before.command,
                ..answer
            },
            before,
            "the rest of the 202 is the object from before the command"
        );

        // The old wish's tick, before the controller drains the channel: the
        // SMC still does not read, so the state turns unknown.
        controller.tick(false);
        let between = handle.row();
        assert_eq!(between.state, FanStateRow::Unknown);
        assert_eq!(
            between.desired,
            FanDesiredRow::Fixed { rpm: 5000 },
            "the new wish paired with a state reached for the old one: {between:?}"
        );
        assert!(between.command < mine, "{between:?}");

        controller.pump();
        let after = handle.row();
        assert_eq!(
            (after.desired, after.state),
            (FanDesiredRow::Auto, FanStateRow::Off)
        );
        assert_eq!(after.command, mine);
    }

    /// The handle numbers commands one after another, from where the process
    /// started; a shutdown takes no number.
    #[test]
    fn fan_route_numbers_each_command_in_order() {
        let (handle, _controller) = inert(None);
        let first = handle.send(Command::Set(FanDesired::Auto)).unwrap();
        let second = handle.send(Command::ForceHandBack).unwrap();
        assert_eq!((first.command, second.command), (1, 2));
        let (handle, controller) = inert(None);
        drop(controller);
        assert_eq!(
            handle.send(Command::Set(FanDesired::Auto)),
            Err(NotRunning),
            "a controller that is gone answers no number"
        );
        assert_eq!(*handle.sent.lock().unwrap(), 0, "and takes none");
    }

    /// `FanControlRow` as 1.2.12 declared it (`a52420ab`): `sinceMs` required,
    /// no `command`. What a 1.2.12 `tcr fan` parses both the 202 and the
    /// status's `fanControl` as.
    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FanControlRow1212 {
        desired: FanDesiredRow,
        state: FanStateRow,
        reason: Option<FanReasonRow>,
        hot_guard: bool,
        targets: Vec<FanTargetRow>,
        detail: Option<String>,
        since_ms: i64,
    }

    /// 1.2.12's `fan_wait`, its `Waiting` answer only: the status still shows
    /// the wish from before the command, or is older than the send, or moves.
    fn waiting_1212(
        sent_at: i64,
        before: FanDesiredRow,
        mine: FanDesiredRow,
        row: &FanControlRow1212,
    ) -> bool {
        if row.desired != mine {
            return row.desired == before;
        }
        row.since_ms < sent_at
            || matches!(row.state, FanStateRow::Entering | FanStateRow::Releasing)
    }

    /// For one release, a 1.2.12 `tcr fan` works against this server: it parses
    /// the 202 as the object from before its command, waits while the old
    /// wish's state moves, and settles once the controller acts on its own.
    #[test]
    fn fan_route_a_1212_client_reads_the_answer_and_the_status() {
        let fixed = FanDesired::Fixed {
            rpm: Rpm::new(5000).unwrap(),
        };
        let (handle, mut controller) = inert(Some(Saved {
            desired: fixed,
            hold: None,
        }));
        let as_1212 = |row: &FanControlRow| -> FanControlRow1212 {
            serde_json::from_value(serde_json::to_value(row).unwrap())
                .unwrap_or_else(|e| panic!("1.2.12 cannot parse {row:?}: {e}"))
        };
        let shown = handle.row();
        let sent_at = wall_ms();
        let accepted = as_1212(&handle.send(Command::Set(FanDesired::Auto)).unwrap());
        assert_eq!(
            (
                accepted.desired,
                accepted.state,
                accepted.reason,
                accepted.hot_guard,
                accepted.targets.len(),
                accepted.detail,
            ),
            (
                shown.desired,
                shown.state,
                shown.reason,
                shown.hot_guard,
                shown.targets.len(),
                shown.detail,
            ),
            "the 202 is the object from before the command"
        );
        let before = accepted.desired;
        let mine = FanDesiredRow::Auto;

        controller.tick(false);
        let between = as_1212(&handle.row());
        assert!(waiting_1212(sent_at, before, mine, &between), "{between:?}");

        controller.pump();
        let after = as_1212(&handle.row());
        assert!(!waiting_1212(sent_at, before, mine, &after), "{after:?}");
        assert_eq!((after.desired, after.state), (mine, FanStateRow::Off));
    }
}
