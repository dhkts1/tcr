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
//! [`FanHandle`] keeps the last [`Snapshot`] the controller published, plus the
//! wish. The wish is updated by the handle as it sends, under the same lock,
//! so a status read right after `POST /_tcr/fan` already names the new mode
//! even while the controller is still busy with the old one.
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
    Clock, Command, Controller, FanMode, FollowCurve, ForeignHolder, Hold, Input, Level, ModeError,
    Parts, Rpm, Saved, Snapshot, State, Store, Unavailable,
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
    fn of(saved: &Saved, saved_at_ms: i64) -> Self {
        Self {
            version: FAN_FILE_VERSION,
            saved_at_ms,
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
fn load(path: &Path) -> Option<Saved> {
    match read_cache::<FanFile>(path, FAN_FILE_VERSION) {
        CacheRead::Missing => None,
        CacheRead::Degraded(why) => {
            tracing::warn!(path = %path.display(), "fan control: ignored the saved fan mode ({why}); starting on automatic");
            None
        }
        CacheRead::Read(file) => match file.parse() {
            Ok(saved) => Some(saved),
            Err(why) => {
                tracing::warn!(path = %path.display(), "fan control: ignored the saved fan mode ({why}); starting on automatic");
                None
            }
        },
    }
}

struct FileStore {
    path: PathBuf,
}

impl Store for FileStore {
    fn save(&mut self, saved: &Saved) {
        let file = FanFile::of(saved, wall_ms());
        if let Err(error) = write_cache(&self.path, &file, Layout::Pretty) {
            tracing::warn!(path = %self.path.display(), %error, "fan control: could not save the fan mode");
        }
    }
}

// Modes at the edge.

/// A wish as the wire spells it.
pub const fn desired_row(mode: FanMode) -> FanDesiredRow {
    match mode {
        FanMode::Auto => FanDesiredRow::Auto,
        FanMode::Fixed { rpm } => FanDesiredRow::Fixed { rpm: rpm.get() },
        FanMode::FollowCpu { curve } => FanDesiredRow::FollowCpu {
            quiet_below_c: curve.quiet_below_c(),
            full_at_c: curve.full_at_c(),
        },
    }
}

/// A wish from the wire, checked.
pub fn mode_of(row: FanDesiredRow) -> Result<FanMode, ModeError> {
    Ok(match row {
        FanDesiredRow::Auto => FanMode::Auto,
        FanDesiredRow::Fixed { rpm } => FanMode::Fixed {
            rpm: Rpm::new(rpm)?,
        },
        FanDesiredRow::FollowCpu {
            quiet_below_c,
            full_at_c,
        } => FanMode::FollowCpu {
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
        Ok(match self {
            Self::Auto { force: true } => Command::ForceHandBack,
            Self::Auto { force: false } => Command::Set(FanMode::Auto),
            Self::Fixed { rpm } => Command::Set(mode_of(FanDesiredRow::Fixed { rpm })?),
            Self::FollowCpu {
                quiet_below_c,
                full_at_c,
            } => Command::Set(mode_of(FanDesiredRow::FollowCpu {
                quiet_below_c,
                full_at_c,
            })?),
        })
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

// The status object.

/// The `fanControl` object for a wish and a snapshot.
pub fn row(desired: FanMode, snapshot: &Snapshot) -> FanControlRow {
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
        desired: desired_row(desired),
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
        since_ms: snapshot.since_ms,
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
        _ => None,
    }
}

// The handle.

/// What the status shows: the wish and the controller's last snapshot.
struct Shown {
    desired: FanMode,
    snapshot: Snapshot,
}

/// The controller's thread stopped: nothing receives a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotRunning;

/// The route's and the status payload's way to the controller.
pub struct FanHandle {
    tx: Sender<Input>,
    shown: Arc<Mutex<Shown>>,
    /// Hears once when the thread's loop returns.
    done: Mutex<mpsc::Receiver<()>>,
}

impl FanHandle {
    /// Send `command`; the answer is the `fanControl` object as of now, the
    /// wish already updated.
    pub fn send(&self, command: Command) -> Result<FanControlRow, NotRunning> {
        let mut shown = self.shown.lock().unwrap_or_else(PoisonError::into_inner);
        self.tx
            .send(Input::Command(command))
            .map_err(|_gone| NotRunning)?;
        match command {
            Command::Set(mode) => shown.desired = mode,
            Command::ForceHandBack => shown.desired = FanMode::Auto,
            Command::Shutdown { .. } => {}
        }
        Ok(row(shown.desired, &shown.snapshot))
    }

    /// The `fanControl` object as of now.
    pub fn row(&self) -> FanControlRow {
        let shown = self.shown.lock().unwrap_or_else(PoisonError::into_inner);
        row(shown.desired, &shown.snapshot)
    }

    /// Ask the controller to hand back what it holds and stop, and wait at
    /// most `grace` for it. Blocking: call it off the runtime.
    pub fn shutdown(&self, grace: Duration) {
        let wait = grace.min(SHUTDOWN_HAND_BACK);
        let deadline = Instant::now() + wait;
        if self
            .tx
            .send(Input::Command(Command::Shutdown { deadline }))
            .is_err()
        {
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
    let saved = load(&file);
    let desired = saved.as_ref().map_or(FanMode::Auto, |s| s.desired);
    let (tx, rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let shown = Arc::new(Mutex::new(Shown {
        desired,
        snapshot: Snapshot {
            state: State::Off,
            hot_guard: false,
            targets: Vec::new(),
            detail: None,
            since_ms: wall_ms(),
        },
    }));
    let published = Arc::clone(&shown);
    let events = tx.clone();
    std::thread::Builder::new()
        .name("tcr-fan".into())
        .spawn(move || {
            let parts = Parts {
                helper: platform::helper(events),
                heat: ProcessHeat(crate::heat::process()),
                probe: platform::probe(),
                clock: SystemClock,
                store: FileStore { path: file },
                inbox: rx,
                log: Box::new(log_line),
                publish: Box::new(move |snapshot: &Snapshot| {
                    published
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .snapshot = snapshot.clone();
                }),
                pid: std::process::id(),
            };
            Controller::new(parts, saved).run();
            if done_tx.send(()).is_err() {
                // Nobody waits on the stop: the handle is gone.
            }
        })?;
    Ok(Arc::new(FanHandle {
        tx,
        shown,
        done: Mutex::new(done_rx),
    }))
}

/// A handle on a real controller thread that can reach nothing: no SMC, no
/// helper, no file. For the route's tests, which check the HTTP contract and
/// must never touch the machine's fans.
#[cfg(test)]
pub(crate) fn start_inert() -> Arc<FanHandle> {
    struct NoSmc;
    impl tcr_fan::controller::Heat for NoSmc {
        fn read(&mut self) -> Option<tcr_smc::HeatReading> {
            None
        }
    }
    struct NoHelper;
    impl tcr_fan::controller::Helper for NoHelper {
        fn open(
            &mut self,
            _: tcr_fan::controller::Generation,
        ) -> Result<(), tcr_fan::controller::Fault> {
            Err(tcr_fan::controller::Fault::NotSignedForFans)
        }
        fn send(
            &mut self,
            _: &tcr_fan::wire::Request,
            _: Duration,
        ) -> Result<tcr_fan::wire::Reply, tcr_fan::controller::Fault> {
            Err(tcr_fan::controller::Fault::NotSignedForFans)
        }
        fn close(&mut self) {}
    }
    struct NoMacs;
    impl tcr_fan::controller::Probe for NoMacs {
        fn macs_running(&mut self) -> Result<bool, String> {
            Ok(false)
        }
    }
    struct Nowhere;
    impl Store for Nowhere {
        fn save(&mut self, _: &Saved) {}
    }

    let (tx, rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let shown = Arc::new(Mutex::new(Shown {
        desired: FanMode::Auto,
        snapshot: Snapshot {
            state: State::Off,
            hot_guard: false,
            targets: Vec::new(),
            detail: None,
            since_ms: 0,
        },
    }));
    let published = Arc::clone(&shown);
    std::thread::spawn(move || {
        let parts = Parts {
            helper: NoHelper,
            heat: NoSmc,
            probe: NoMacs,
            clock: SystemClock,
            store: Nowhere,
            inbox: rx,
            log: Box::new(|_, _| {}),
            publish: Box::new(move |snapshot: &Snapshot| {
                published.lock().unwrap().snapshot = snapshot.clone();
            }),
            pid: std::process::id(),
        };
        Controller::new(parts, None).run();
        done_tx.send(()).ok();
    });
    Arc::new(FanHandle {
        tx,
        shown,
        done: Mutex::new(done_rx),
    })
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
            state,
            hot_guard: false,
            targets: targets.iter().map(|&(i, rpm)| (fan(i), rpm)).collect(),
            detail,
            since_ms: 1_760_000_000_000,
        }
    }

    /// What this code emits for each state TcrBar draws is, value for value,
    /// the committed contract sample.
    #[test]
    fn fan_status_rows_match_the_committed_contract() {
        let samples = fixture();
        let fixed = FanMode::Fixed {
            rpm: Rpm::new(5000).unwrap(),
        };
        let follow = FanMode::FollowCpu {
            curve: FollowCurve::new(60, 90).unwrap(),
        };
        let cases = [
            ("off", FanMode::Auto, snapshot(State::Off, &[], None)),
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
                FanMode::Auto,
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
                serde_json::to_value(row(desired, &snapshot)).unwrap(),
                samples[name],
                "{name}"
            );
        }
    }

    #[test]
    fn fan_status_reasons_with_fields_get_a_detail() {
        let locked = row(
            FanMode::Auto,
            &snapshot(
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
        let version = row(
            FanMode::Auto,
            &snapshot(
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
            since_ms: 0,
            ..snapshot(state, targets, detail)
        };
        let fixed = FanMode::Fixed {
            rpm: Rpm::new(5000).unwrap(),
        };
        let cases = [
            (
                FanMode::Auto,
                at(State::Off, &[], None),
                r#"{"desired":{"mode":"auto"},"state":"off","reason":null,"hotGuard":false,"targets":[],"detail":null,"sinceMs":0}"#,
            ),
            (
                fixed,
                at(State::Holding, &[(0, 5000.0), (1, 5000.0)], None),
                r#"{"desired":{"mode":"fixed","rpm":5000},"state":"holding","reason":null,"hotGuard":false,"targets":[{"fan":0,"rpm":5000.0},{"fan":1,"rpm":5000.0}],"detail":null,"sinceMs":0}"#,
            ),
            (
                FanMode::Auto,
                at(
                    State::Foreign(ForeignHolder::MacsFanControl),
                    &[],
                    Some(Detail::WaitingForMacs),
                ),
                r#"{"desired":{"mode":"auto"},"state":"foreign","reason":"macsFanControl","hotGuard":false,"targets":[],"detail":"fans go back to automatic when Macs Fan Control quits","sinceMs":0}"#,
            ),
        ];
        for (desired, snapshot, expected) in cases {
            assert_eq!(
                serde_json::to_string(&row(desired, &snapshot)).unwrap(),
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
            desired: FanMode::Fixed {
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
        FileStore { path: path.clone() }.save(&saved);
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
}
