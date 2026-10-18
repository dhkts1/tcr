//! What the root fan helper does, as plain Rust a test can run without root:
//! the four operations, who holds the fans, the disconnect rule with its
//! bounded hand-back window, and when the helper may exit.
//!
//! `tcr-fan-helper`'s `main.rs` owns the XPC listener and the dispatch queue
//! and calls into [`Root`] from that one serial queue, so every accept, message,
//! disconnect, timer and signal is handled one at a time, in order. Nothing in
//! here sleeps between window attempts: [`Next::RetryAfter`] tells the caller
//! when to call [`Root::window_due`], and the helper schedules that with
//! `dispatch_after_f`, so the queue stays free for a new connection meanwhile.
//!
//! # The rules
//!
//! - **set-manual** makes the caller the holder. A newer connection supersedes
//!   an older one, and a pending disconnect window is cancelled: the new
//!   holder takes the fans as they are. Any failure inside the writer's unlock
//!   sequence hands every fan back before the error is answered.
//! - **set-target** only from the holder, only for a fan it put in manual.
//! - **hand-back** from the holder, or from anyone while nobody holds.
//! - **disconnect** of the holder: if the fans are still ours
//!   ([`holder::still_ours`]), hand back at once, then again after waits of
//!   1, 2, 4, 8, 16 and 32 s until one works (7 attempts over 63 s). Each
//!   attempt judges the fans afresh, so one another program took meanwhile is
//!   left alone. If another program
//!   changed them, log it and leave them. **A disconnect of any other
//!   connection changes nothing**: a stale connection's drop must never undo a
//!   newer holder's fans.
//! - **exit** only when idle: no connection, no holder, no window, and still
//!   so [`IDLE_EXIT_GRACE`] later (launchd will not start the helper again
//!   sooner than 10 s after its last start). A
//!   connection counts once one of its messages reaches root, so a peer whose
//!   every message libxpc drops (it failed the signature check) cannot keep
//!   root alive.
//! - **a failed set-manual** that wrote nothing (no such fan, or `FNum` did
//!   not read) changes no hands: the previous holder and its window stand.
//!
//! # The recovery limit
//!
//! When all seven attempts fail, root forgets the hold and the fans stay manual
//! at the last target until one of these happens: the next proxy start (its
//! record still matches the fans, so it resumes or hands back), `tcr fan auto
//! --force`, or a reboot. Whether a reboot resets the fan modes on Apple Silicon
//! is measured on hardware, not assumed here.

use std::collections::BTreeSet;
use std::time::Duration;

use tcr_smc::{FanIndex, FanWriter, KeyAccess, SetManualFailed, SmcError};

use crate::holder::{self, FanState, HeldFan, StillOurs};
use crate::wire::{self, HelperError, Refusal, Reply, Request};

/// The waits between the disconnect window's attempts. The first attempt runs
/// at once, so this is seven attempts over about 63 seconds.
pub const DISCONNECT_RETRY_GAPS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(32),
];

/// Every attempt the window makes, the first included.
pub const DISCONNECT_ATTEMPTS: usize = DISCONNECT_RETRY_GAPS.len() + 1;

/// How long the helper stays up once idle before it exits, if still idle then.
/// launchd starts a job no sooner than 10 s after its previous start (measured
/// 2026-10-18 on an M4 Pro: starts at 22:40:34.13, 22:40:44.29 and
/// 22:40:54.71), so a helper that exited at once left a caller arriving inside
/// those 10 s waiting on launchd, and its first call timed out. Staying 15 s
/// means the helper has been up longer than the spacing before it exits, so
/// the next start is never held back.
pub const IDLE_EXIT_GRACE: Duration = Duration::from_secs(15);

/// One scheduled idle check. Anything that reaches root after it was issued,
/// or a later check, makes it stale: a stale check never exits the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleCheck(u64);

impl IdleCheck {
    /// The id as a number, for the helper to carry through a timer's context
    /// pointer.
    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// One XPC connection, numbered by the helper in the order they arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub u64);

/// One disconnect window. A timer that fires for a window that was cancelled
/// or finished carries an id that no longer matches, and does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowId(u64);

impl WindowId {
    /// The id as a number, for the helper to carry through a timer's context
    /// pointer.
    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// How loud a line is: the helper maps these to syslog levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Ordinary events: a hand-back that worked, fans left to another program.
    Notice,
    /// A refused request.
    Error,
    /// A hand-back that failed: fans may be left manual.
    Critical,
}

/// Where root's lines go.
pub type Log = Box<dyn FnMut(Level, &str) + Send>;

/// How root reaches the SMC: opened on the first fan operation and kept, so a
/// version call costs no IOKit at all.
pub type WriterOpener<K> = Box<dyn FnMut() -> Result<FanWriter<K>, SmcError> + Send>;

/// What the helper does after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Next {
    /// Keep running.
    Stay,
    /// Idle: no connection, no holder, no window. Call [`Root::idle_due`]
    /// with this check after this long ([`IDLE_EXIT_GRACE`]), and exit only
    /// if it says so.
    ExitIfStillIdle { check: IdleCheck, after: Duration },
    /// Call [`Root::window_due`] with this id after this long.
    RetryAfter { window: WindowId, after: Duration },
}

#[derive(Debug, Clone)]
struct Hold {
    peer: PeerId,
    fans: Vec<HeldFan>,
}

#[derive(Debug, Clone)]
struct Window {
    id: WindowId,
    attempts: usize,
    fans: Vec<HeldFan>,
}

pub struct Root<K> {
    open: WriterOpener<K>,
    writer: Option<FanWriter<K>>,
    build: String,
    log: Log,
    peers: BTreeSet<PeerId>,
    holder: Option<Hold>,
    window: Option<Window>,
    windows_opened: u64,
    /// Bumped by every request and every idle check issued; an
    /// [`IdleCheck`] is current only while this still equals it.
    activity: u64,
}

impl<K: KeyAccess> Root<K> {
    /// `build` is the helper's own build stamp, answered by the version op
    /// (cut to [`wire::MAX_BUILD_BYTES`]).
    pub fn new(open: WriterOpener<K>, build: &str, log: Log) -> Self {
        Self {
            open,
            writer: None,
            build: wire::bounded_build(build),
            log,
            peers: BTreeSet::new(),
            holder: None,
            window: None,
            windows_opened: 0,
            activity: 0,
        }
    }

    /// No counted connection, no holder, no window.
    pub fn is_idle(&self) -> bool {
        self.peers.is_empty() && self.holder.is_none() && self.window.is_none()
    }

    /// A check that stays current until anything reaches root: the helper's
    /// startup check, scheduled before the first connection.
    pub const fn idle_check_now(&self) -> IdleCheck {
        IdleCheck(self.activity)
    }

    /// The timer for `check` fired: `true` when root is idle and nothing has
    /// reached it since the check was issued, so the helper may exit.
    pub fn idle_due(&self, check: IdleCheck) -> bool {
        self.is_idle() && check.0 == self.activity
    }

    /// The connection holding the fans, if any.
    pub fn holder(&self) -> Option<PeerId> {
        self.holder.as_ref().map(|h| h.peer)
    }

    /// A disconnect window is pending.
    pub const fn window_pending(&self) -> bool {
        self.window.is_some()
    }

    /// A connection arrived. It is not counted yet: libxpc drops every message
    /// from a peer that fails the listener's code-signing requirement, so a
    /// connection counted here could keep root from ever going idle without
    /// once reaching it. A peer counts from its first message that reaches
    /// [`Self::request`].
    pub const fn connected(&mut self, _peer: PeerId) {}

    /// One request, already decoded by [`wire::decode_request`]. A refused
    /// decode is answered without opening the SMC. The peer counts as
    /// connected from here until it disconnects.
    pub fn request(&mut self, peer: PeerId, request: Result<Request, Refusal>) -> Reply {
        self.peers.insert(peer);
        self.activity += 1;
        let result = match request {
            Err(Refusal::BadRequest) => Err(HelperError::BadRequest),
            Err(Refusal::FanPastMax { fan }) => self.no_such_fan(fan),
            Ok(Request::Version) => Ok(Reply::Version {
                protocol: wire::PROTOCOL_VERSION,
                build: self.build.clone(),
            }),
            Ok(Request::SetManual { fan }) => self.set_manual(peer, fan),
            Ok(Request::SetTarget { fan, rpm }) => self.set_target(peer, fan, rpm),
            Ok(Request::HandBack) => self.hand_back(peer),
        };
        result.unwrap_or_else(|error| {
            self.say(
                Level::Error,
                &format!("refused a request from connection {}: {error}", peer.0),
            );
            Reply::Refused(error)
        })
    }

    /// A connection went away. Only the holder's departure does anything.
    pub fn disconnected(&mut self, peer: PeerId) -> Next {
        self.peers.remove(&peer);
        let Some(hold) = self.holder.take_if(|h| h.peer == peer) else {
            return self.exit_if_idle();
        };
        self.windows_opened += 1;
        let id = WindowId(self.windows_opened);
        self.window = Some(Window {
            id,
            attempts: 0,
            fans: hold.fans,
        });
        self.attempt(id)
    }

    /// The timer for window `id` fired.
    pub fn window_due(&mut self, id: WindowId) -> Next {
        match &self.window {
            Some(window) if window.id == id => self.attempt(id),
            // Cancelled by a new holder or a hand-back, or already finished:
            // whatever made it stale also decided whether to exit.
            _ => Next::Stay,
        }
    }

    /// launchd's SIGTERM: one hand-back if the fans are still ours, then the
    /// caller exits whatever happened.
    pub fn terminate(&mut self) {
        let fans = match (self.holder.take(), self.window.take()) {
            (Some(hold), _) => hold.fans,
            (None, Some(window)) => window.fans,
            (None, None) => return,
        };
        match self.judge(&fans) {
            Ok(StillOurs::Ours) => match self
                .writer()
                .and_then(|w| w.hand_back().map_err(HelperError::from_fan))
            {
                Ok(()) => self.say(Level::Notice, "SIGTERM: fans handed back to macOS"),
                Err(error) => self.say(
                    Level::Critical,
                    &format!("SIGTERM: hand-back failed ({error}); fans may stay manual"),
                ),
            },
            Ok(StillOurs::Changed { fan }) => self.say(
                Level::Notice,
                &format!(
                    "SIGTERM: fan {} changed since root wrote it; left as it is",
                    fan.get()
                ),
            ),
            Ok(StillOurs::Unknown { fan }) => self.say(
                Level::Critical,
                &format!("SIGTERM: fan {} did not read; wrote nothing", fan.get()),
            ),
            Err(error) => self.say(
                Level::Critical,
                &format!("SIGTERM: fans did not read ({error}); wrote nothing"),
            ),
        }
    }

    /// Idle: a fresh check, which makes every earlier one stale, so the
    /// helper exits [`IDLE_EXIT_GRACE`] after the LAST time it went idle.
    fn exit_if_idle(&mut self) -> Next {
        if !self.is_idle() {
            return Next::Stay;
        }
        self.activity += 1;
        Next::ExitIfStillIdle {
            check: IdleCheck(self.activity),
            after: IDLE_EXIT_GRACE,
        }
    }

    fn say(&mut self, level: Level, line: &str) {
        (self.log)(level, line);
    }

    fn writer(&mut self) -> Result<&mut FanWriter<K>, HelperError> {
        if self.writer.is_none() {
            let opened = (self.open)().map_err(HelperError::from_smc)?;
            self.writer = Some(opened);
        }
        self.writer.as_mut().ok_or(HelperError::SmcUnavailable)
    }

    fn no_such_fan(&mut self, fan: u64) -> Result<Reply, HelperError> {
        let count = self.writer()?.fan_count().map_err(HelperError::from_fan)?;
        Err(HelperError::NoSuchFan {
            fan,
            count: u64::from(count),
        })
    }

    fn set_manual(&mut self, peer: PeerId, fan: FanIndex) -> Result<Reply, HelperError> {
        // The fan is checked before anything changes hands: a refused index
        // leaves the holder and any window exactly as they were.
        let count = self.writer()?.fan_count().map_err(HelperError::from_fan)?;
        if fan.get() >= count {
            return Err(HelperError::NoSuchFan {
                fan: u64::from(fan.get()),
                count: u64::from(count),
            });
        }

        // What root put in manual for an earlier holder, or for a connection
        // whose window is running, is still root's doing, so the record
        // carries over to whoever holds next.
        let mut fans = self
            .holder
            .as_ref()
            .map(|h| h.fans.clone())
            .unwrap_or_default();
        if let Some(window) = &self.window {
            merge(&mut fans, &window.fans);
        }

        match self.writer()?.set_manual(fan) {
            Ok(entered) => {
                if !fans.iter().any(|h| h.fan == fan) {
                    fans.push(HeldFan {
                        fan,
                        written_rpm: None,
                    });
                }
                self.take_over(peer, fans);
                Ok(Reply::Manual {
                    unlocked: entered.unlocked,
                })
            }
            Err(SetManualFailed { cause, hand_back }) => {
                match hand_back {
                    // Nothing was written, so no hands change: the previous
                    // holder, and any window still running, stand as they were.
                    None => {}
                    // Every fan is automatic again: nobody holds anything.
                    Some(Ok(())) => {
                        self.holder = None;
                        self.window = None;
                    }
                    // Fans may be manual and nobody else is holding them now,
                    // so this connection holds them and its disconnect window
                    // tries again.
                    Some(Err(error)) => {
                        self.take_over(peer, fans);
                        self.say(
                            Level::Critical,
                            &format!(
                                "set-manual failed ({cause}) and the hand-back after it failed too: {error}"
                            ),
                        );
                    }
                }
                Err(HelperError::from_fan(cause))
            }
        }
    }

    /// `peer` becomes the holder of `fans`, ending any earlier hold and any
    /// pending disconnect window.
    fn take_over(&mut self, peer: PeerId, fans: Vec<HeldFan>) {
        if let Some(old) = self.holder.take()
            && old.peer != peer
        {
            self.say(
                Level::Notice,
                &format!(
                    "connection {} takes the fans from connection {}",
                    peer.0, old.peer.0
                ),
            );
        }
        if self.window.take().is_some() {
            self.say(
                Level::Notice,
                &format!(
                    "connection {} took the fans; disconnect window cancelled",
                    peer.0
                ),
            );
        }
        self.holder = Some(Hold { peer, fans });
    }

    fn set_target(&mut self, peer: PeerId, fan: FanIndex, rpm: f64) -> Result<Reply, HelperError> {
        let holds_fan = self
            .holder
            .as_ref()
            .is_some_and(|h| h.peer == peer && h.fans.iter().any(|f| f.fan == fan));
        if !holds_fan {
            return Err(HelperError::NotHolder);
        }
        let written = self
            .writer()?
            .set_target(fan, rpm)
            .map_err(HelperError::from_fan)?;
        if let Some(held) = self
            .holder
            .as_mut()
            .and_then(|h| h.fans.iter_mut().find(|f| f.fan == fan))
        {
            held.written_rpm = Some(f64::from(written));
        }
        Ok(Reply::Target {
            rpm: f64::from(written),
        })
    }

    fn hand_back(&mut self, peer: PeerId) -> Result<Reply, HelperError> {
        if self.holder.as_ref().is_some_and(|h| h.peer != peer) {
            return Err(HelperError::NotHolder);
        }
        match self.writer()?.hand_back() {
            Ok(()) => {
                self.holder = None;
                self.window = None;
                Ok(Reply::HandedBack)
            }
            Err(error) => {
                self.say(
                    Level::Critical,
                    &format!("hand-back asked by connection {} failed: {error}", peer.0),
                );
                Err(HelperError::from_fan(error))
            }
        }
    }

    /// Every fan's mode and target, read now.
    fn reading(&mut self) -> Result<Vec<FanState>, HelperError> {
        let writer = self.writer()?;
        let count = writer.fan_count().map_err(HelperError::from_fan)?;
        Ok((0..count)
            .filter_map(FanIndex::new)
            .map(|fan| FanState {
                fan,
                mode: writer.mode(fan),
                target_rpm: writer.target_rpm(fan).ok(),
            })
            .collect())
    }

    fn judge(&mut self, fans: &[HeldFan]) -> Result<StillOurs, HelperError> {
        // Root put every fan in its record in manual itself, so a fan with
        // no target yet is still its doing to hand back.
        Ok(holder::still_ours(
            &self.reading()?,
            fans,
            holder::Unwritten::Ours,
        ))
    }

    /// One attempt of window `id`, which is current.
    fn attempt(&mut self, id: WindowId) -> Next {
        let Some(window) = self.window.as_mut() else {
            return self.exit_if_idle();
        };
        window.attempts += 1;
        let attempts = window.attempts;
        let fans = window.fans.clone();

        let failure = match self.judge(&fans) {
            Ok(StillOurs::Changed { fan }) => {
                self.say(
                    Level::Notice,
                    &format!(
                        "holder gone; fan {} changed since root wrote it, so the fans are left as they are",
                        fan.get()
                    ),
                );
                self.window = None;
                return self.exit_if_idle();
            }
            Ok(StillOurs::Ours) => {
                match self
                    .writer()
                    .and_then(|w| w.hand_back().map_err(HelperError::from_fan))
                {
                    Ok(()) => {
                        self.say(Level::Notice, "holder gone; fans handed back to macOS");
                        self.window = None;
                        return self.exit_if_idle();
                    }
                    Err(error) => format!("hand-back failed: {error}"),
                }
            }
            Ok(StillOurs::Unknown { fan }) => {
                format!("fan {} did not read, so nothing was written", fan.get())
            }
            Err(error) => format!("the fans did not read ({error}), so nothing was written"),
        };
        self.say(
            Level::Critical,
            &format!("holder gone; attempt {attempts} of {DISCONNECT_ATTEMPTS}: {failure}"),
        );
        if let Some(&after) = DISCONNECT_RETRY_GAPS.get(attempts - 1) {
            return Next::RetryAfter { window: id, after };
        }
        self.say(
            Level::Critical,
            &format!("gave up; fans stay manual at {}", describe(&fans)),
        );
        self.window = None;
        self.exit_if_idle()
    }
}

/// Add every fan of `more` that `into` does not already hold.
fn merge(into: &mut Vec<HeldFan>, more: &[HeldFan]) {
    for held in more {
        if !into.iter().any(|h| h.fan == held.fan) {
            into.push(*held);
        }
    }
}

/// "fan 0 3000 rpm, fan 1 no target written" for the last log line.
fn describe(fans: &[HeldFan]) -> String {
    if fans.is_empty() {
        return "no recorded target".into();
    }
    fans.iter()
        .map(|h| match h.written_rpm {
            Some(rpm) => format!("fan {} {rpm:.0} rpm", h.fan.get()),
            None => format!("fan {} no target written", h.fan.get()),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::fake::{FakeSmc, Shared};

    struct Rig {
        root: Root<FakeSmc>,
        smc: Shared,
        lines: Arc<Mutex<Vec<(Level, String)>>>,
        opens: Arc<Mutex<usize>>,
    }

    impl Rig {
        fn new() -> Self {
            let smc = Shared::apple_silicon();
            let lines = Arc::new(Mutex::new(Vec::new()));
            let opens = Arc::new(Mutex::new(0));
            let (s, l, o) = (smc.clone(), Arc::clone(&lines), Arc::clone(&opens));
            let root = Root::new(
                Box::new(move || {
                    *o.lock().unwrap() += 1;
                    Ok(FanWriter::with_sleeper(s.handle(), Box::new(|_| {})))
                }),
                "abc1234",
                Box::new(move |level, line| l.lock().unwrap().push((level, line.to_owned()))),
            );
            Self {
                root,
                smc,
                lines,
                opens,
            }
        }

        fn send(&mut self, peer: u64, request: Request) -> Reply {
            self.root.request(PeerId(peer), Ok(request))
        }

        /// Peer takes fan 0 and 1 to manual at `rpm`.
        fn hold(&mut self, peer: u64, rpm: f64) {
            self.root.connected(PeerId(peer));
            for index in [0, 1] {
                assert!(matches!(
                    self.send(peer, Request::SetManual { fan: fan(index) }),
                    Reply::Manual { .. }
                ));
                assert_eq!(
                    self.send(
                        peer,
                        Request::SetTarget {
                            fan: fan(index),
                            rpm
                        }
                    ),
                    Reply::Target { rpm }
                );
            }
        }

        fn critical(&self) -> usize {
            self.lines
                .lock()
                .unwrap()
                .iter()
                .filter(|(level, _)| *level == Level::Critical)
                .count()
        }
    }

    fn fan(index: u8) -> FanIndex {
        FanIndex::new(index).unwrap()
    }

    /// Root went idle: the check it issued, after asserting the grace.
    fn exit_check(next: Next) -> IdleCheck {
        let Next::ExitIfStillIdle { check, after } = next else {
            panic!("expected an idle check, got {next:?}");
        };
        assert_eq!(after, IDLE_EXIT_GRACE);
        check
    }

    fn assert_exits(next: Next) {
        exit_check(next);
    }

    fn version(rig: &mut Rig, peer: u64) {
        rig.root.connected(PeerId(peer));
        assert!(matches!(
            rig.send(peer, Request::Version),
            Reply::Version { .. }
        ));
    }

    /// launchd starts the helper no sooner than 10 s after its last start
    /// (measured 2026-10-18), so the helper outlasts that once idle.
    #[test]
    fn the_idle_grace_outlasts_launchds_restart_spacing() {
        assert!(IDLE_EXIT_GRACE > Duration::from_secs(10));
        let mut rig = Rig::new();
        version(&mut rig, 1);
        let check = exit_check(rig.root.disconnected(PeerId(1)));
        assert!(rig.root.idle_due(check), "nothing happened, so it may exit");
    }

    /// A connection inside the grace keeps the helper; once it leaves, only
    /// the newest check may exit it, a full grace after that departure.
    #[test]
    fn a_connection_inside_the_idle_grace_keeps_the_helper() {
        let mut rig = Rig::new();
        version(&mut rig, 1);
        let first = exit_check(rig.root.disconnected(PeerId(1)));

        version(&mut rig, 2);
        assert!(!rig.root.idle_due(first), "exited under a live connection");
        let second = exit_check(rig.root.disconnected(PeerId(2)));
        assert!(!rig.root.idle_due(first), "a stale check exited early");
        assert!(rig.root.idle_due(second));

        // A silent connection (dropped by the signature check) that comes and
        // goes restarts the grace too, and still cannot keep root up.
        rig.root.connected(PeerId(3));
        let third = exit_check(rig.root.disconnected(PeerId(3)));
        assert!(!rig.root.idle_due(second));
        assert!(rig.root.idle_due(third));
    }

    /// The startup check exits only if nothing ever reached root.
    #[test]
    fn the_startup_check_exits_only_when_nothing_arrived() {
        let mut rig = Rig::new();
        let startup = rig.root.idle_check_now();
        rig.root.connected(PeerId(1));
        assert!(
            rig.root.idle_due(startup),
            "a silent connection kept root up"
        );

        let mut rig = Rig::new();
        let startup = rig.root.idle_check_now();
        version(&mut rig, 1);
        let after = exit_check(rig.root.disconnected(PeerId(1)));
        assert!(!rig.root.idle_due(startup), "startup check cut the grace");
        assert!(rig.root.idle_due(after));
    }

    #[test]
    fn version_answers_without_opening_the_smc() {
        let mut rig = Rig::new();
        assert_eq!(
            rig.send(7, Request::Version),
            Reply::Version {
                protocol: 1,
                build: "abc1234".into()
            }
        );
        assert_eq!(*rig.opens.lock().unwrap(), 0);
    }

    #[test]
    fn a_refused_request_touches_no_key() {
        let mut rig = Rig::new();
        assert_eq!(
            rig.root.request(PeerId(1), Err(Refusal::BadRequest)),
            Reply::Refused(HelperError::BadRequest)
        );
        assert_eq!(*rig.opens.lock().unwrap(), 0);
        assert!(rig.smc.writes().is_empty());
    }

    #[test]
    fn a_fan_past_nine_is_answered_with_this_macs_count() {
        let mut rig = Rig::new();
        assert_eq!(
            rig.root
                .request(PeerId(1), Err(Refusal::FanPastMax { fan: 12 })),
            Reply::Refused(HelperError::NoSuchFan { fan: 12, count: 2 })
        );
        assert!(rig.smc.writes().is_empty());
    }

    #[test]
    fn set_target_needs_the_holder_and_its_fan() {
        let mut rig = Rig::new();
        let target = Request::SetTarget {
            fan: fan(0),
            rpm: 3000.0,
        };
        // Nobody holds.
        assert_eq!(rig.send(1, target), Reply::Refused(HelperError::NotHolder));
        rig.hold(1, 3000.0);
        // Another connection.
        assert_eq!(rig.send(2, target), Reply::Refused(HelperError::NotHolder));
        assert_eq!(
            rig.send(2, Request::HandBack),
            Reply::Refused(HelperError::NotHolder)
        );
        assert_eq!(rig.smc.writes().len(), 4, "only the holder's four writes");
    }

    #[test]
    fn nan_is_refused_and_never_clamped() {
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        let before = rig.smc.writes().len();
        assert_eq!(
            rig.send(
                1,
                Request::SetTarget {
                    fan: fan(0),
                    rpm: f64::NAN
                }
            ),
            Reply::Refused(HelperError::NotANumber)
        );
        assert_eq!(rig.smc.writes().len(), before);
    }

    #[test]
    fn a_newer_set_manual_supersedes_the_holder() {
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        rig.root.connected(PeerId(2));
        assert!(matches!(
            rig.send(2, Request::SetManual { fan: fan(0) }),
            Reply::Manual { .. }
        ));
        assert_eq!(rig.root.holder(), Some(PeerId(2)));
        assert_eq!(
            rig.send(
                1,
                Request::SetTarget {
                    fan: fan(0),
                    rpm: 2000.0
                }
            ),
            Reply::Refused(HelperError::NotHolder)
        );
        // The old holder's departure no longer hands anything back.
        let before = rig.smc.writes().len();
        assert_eq!(rig.root.disconnected(PeerId(1)), Next::Stay);
        assert_eq!(rig.smc.writes().len(), before);
    }

    #[test]
    fn non_holder_disconnect_does_not_hand_back() {
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        rig.root.connected(PeerId(2));
        assert!(matches!(
            rig.send(2, Request::Version),
            Reply::Version { .. }
        ));
        let before = rig.smc.writes().len();

        assert_eq!(rig.root.disconnected(PeerId(2)), Next::Stay);

        assert_eq!(rig.smc.writes().len(), before, "a non-holder's drop wrote");
        assert_eq!(rig.smc.ui8("F0Md"), 1);
        assert_eq!(rig.smc.ui8("F1Md"), 1);
        assert_eq!(rig.root.holder(), Some(PeerId(1)));
        assert!(!rig.root.window_pending());
    }

    #[test]
    fn holder_disconnect_hands_back_modes_first() {
        let mut rig = Rig::new();
        rig.smc.set_ui8("Ftst", 1);
        rig.hold(1, 3000.0);
        rig.smc.clear_writes();
        assert_exits(rig.root.disconnected(PeerId(1)));
        assert_eq!(rig.smc.written_keys(), ["F0Md", "F1Md", "Ftst"]);
        assert_eq!(rig.smc.ui8("F0Md"), 0);
        assert_eq!(rig.smc.ui8("Ftst"), 0);
    }

    #[test]
    fn a_changed_target_is_left_alone_on_disconnect() {
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        rig.smc.set_flt("F1Tg", 4200.0);
        rig.smc.clear_writes();
        assert_exits(rig.root.disconnected(PeerId(1)));
        assert!(rig.smc.writes().is_empty());
        assert_eq!(rig.smc.ui8("F1Md"), 1);
    }

    #[test]
    fn exit_only_when_idle() {
        let mut rig = Rig::new();
        assert!(rig.root.is_idle());

        // Two connections: the first to leave does not end the helper.
        for peer in [1, 2] {
            rig.root.connected(PeerId(peer));
            assert!(matches!(
                rig.send(peer, Request::Version),
                Reply::Version { .. }
            ));
        }
        assert!(!rig.root.is_idle());
        assert_eq!(rig.root.disconnected(PeerId(1)), Next::Stay);
        assert_exits(rig.root.disconnected(PeerId(2)));

        // A holder whose hand-back keeps failing: no connection is left, but
        // the window is, so the helper stays.
        rig.hold(3, 3000.0);
        rig.smc.refuse("F0Md");
        let Next::RetryAfter { window, .. } = rig.root.disconnected(PeerId(3)) else {
            panic!("a failed hand-back must leave a window, not an exit");
        };
        assert!(rig.root.window_pending());
        assert!(!rig.root.is_idle());

        // A connection arriving during the window keeps the helper alive past
        // the window's end; its own departure is what ends it.
        rig.root.connected(PeerId(4));
        assert!(matches!(
            rig.send(4, Request::Version),
            Reply::Version { .. }
        ));
        rig.smc.accept("F0Md");
        assert_eq!(rig.root.window_due(window), Next::Stay);
        assert!(!rig.root.window_pending());
        assert_exits(rig.root.disconnected(PeerId(4)));

        // A holder that hands back by itself and leaves: idle, exit.
        rig.hold(5, 3000.0);
        assert_eq!(rig.send(5, Request::HandBack), Reply::HandedBack);
        assert_eq!(rig.root.holder(), None);
        assert_exits(rig.root.disconnected(PeerId(5)));
    }

    #[test]
    fn a_connection_that_never_reaches_root_leaves_it_idle() {
        // A peer that failed the signature check: libxpc accepted the
        // connection and dropped every message, so root heard only "connected".
        let mut rig = Rig::new();
        rig.root.connected(PeerId(1));
        assert!(rig.root.is_idle(), "a silent connection kept root alive");
        assert_exits(rig.root.disconnected(PeerId(1)));

        // Beside a counted peer, a silent one's arrival changes nothing either:
        // the counted peer's departure is what ends the helper.
        rig.root.connected(PeerId(2));
        assert!(matches!(
            rig.send(2, Request::Version),
            Reply::Version { .. }
        ));
        rig.root.connected(PeerId(3));
        assert_exits(rig.root.disconnected(PeerId(2)));
    }

    #[test]
    fn a_set_manual_that_wrote_nothing_keeps_the_previous_holder() {
        // A holder, and a second connection whose set-manual fails before any
        // write: `FNum` reads for root's own check, then not for the writer's.
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        rig.smc.fail_reads_after("FNum", 1);
        rig.smc.clear_writes();
        assert!(matches!(
            rig.send(2, Request::SetManual { fan: fan(0) }),
            Reply::Refused(_)
        ));
        rig.smc.accept("FNum");
        assert!(rig.smc.writes().is_empty());
        assert_eq!(rig.root.holder(), Some(PeerId(1)));
        assert_eq!(
            rig.send(
                1,
                Request::SetTarget {
                    fan: fan(1),
                    rpm: 2500.0
                }
            ),
            Reply::Target { rpm: 2500.0 }
        );
        assert_eq!(
            rig.send(
                2,
                Request::SetTarget {
                    fan: fan(0),
                    rpm: 2000.0
                }
            ),
            Reply::Refused(HelperError::NotHolder)
        );

        // The same failure during a disconnect window leaves the window
        // running: its next attempt still hands the fans back.
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        rig.smc.refuse("F0Md");
        let Next::RetryAfter { window, .. } = rig.root.disconnected(PeerId(1)) else {
            panic!("expected a window");
        };
        rig.smc.accept("F0Md");
        rig.smc.fail_reads_after("FNum", 1);
        assert!(matches!(
            rig.send(2, Request::SetManual { fan: fan(0) }),
            Reply::Refused(_)
        ));
        rig.smc.accept("FNum");
        assert_eq!(rig.root.holder(), None);
        assert!(rig.root.window_pending());
        assert_eq!(rig.root.window_due(window), Next::Stay);
        assert_eq!(rig.smc.ui8("F0Md"), 0);
        assert_eq!(rig.smc.ui8("F1Md"), 0);
        assert!(!rig.root.window_pending());
        // Peer 2 sent a message, so it still counts; its departure ends it.
        assert_exits(rig.root.disconnected(PeerId(2)));
    }

    #[test]
    fn disconnect_window_is_bounded_and_cancelled_by_new_holder() {
        // Bounded: a mode key that refuses every write.
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        rig.smc.refuse("F0Md");
        rig.smc.clear_writes();
        let mut next = rig.root.disconnected(PeerId(1));
        let mut gaps = Vec::new();
        while let Next::RetryAfter { window, after } = next {
            gaps.push(after);
            assert!(gaps.len() < 100, "the window does not end");
            next = rig.root.window_due(window);
        }
        assert_exits(next);
        assert_eq!(gaps, DISCONNECT_RETRY_GAPS);
        assert_eq!(gaps.iter().sum::<Duration>(), Duration::from_secs(63));
        // Seven failed attempts and the closing "gave up" line, all critical.
        assert_eq!(rig.critical(), DISCONNECT_ATTEMPTS + 1);
        let last = rig.lines.lock().unwrap().last().unwrap().1.clone();
        assert!(
            last.starts_with("gave up; fans stay manual at fan 0 3000 rpm"),
            "{last}"
        );
        // Each attempt's writes are bounded too: ten tries at the refusing key.
        assert_eq!(
            rig.smc.attempts_at("F0Md"),
            DISCONNECT_ATTEMPTS * tcr_smc::HAND_BACK_TRIES
        );
        assert!(rig.root.is_idle());

        // Cancelled: a new holder's set-manual during the window.
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        rig.smc.refuse("F0Md");
        let Next::RetryAfter { window, .. } = rig.root.disconnected(PeerId(1)) else {
            panic!("expected a window");
        };
        rig.smc.accept("F0Md");
        rig.root.connected(PeerId(2));
        assert!(matches!(
            rig.send(2, Request::SetManual { fan: fan(0) }),
            Reply::Manual { .. }
        ));
        assert!(!rig.root.window_pending());
        rig.smc.clear_writes();
        assert_eq!(rig.root.window_due(window), Next::Stay);
        assert!(rig.smc.writes().is_empty(), "a cancelled window wrote");
        assert_eq!(rig.root.holder(), Some(PeerId(2)));
        // The new holder inherited fan 1, which root still has manual.
        assert_eq!(
            rig.send(
                2,
                Request::SetTarget {
                    fan: fan(1),
                    rpm: 2500.0
                }
            ),
            Reply::Target { rpm: 2500.0 }
        );
    }

    #[test]
    fn a_failed_set_manual_hands_every_fan_back_before_answering() {
        let mut rig = Rig::new();
        rig.root.connected(PeerId(1));
        assert!(matches!(
            rig.send(1, Request::SetManual { fan: fan(0) }),
            Reply::Manual { .. }
        ));
        // Fan 1 never unlocks.
        rig.smc.set_ui8("Ftst", 1);
        rig.smc.refuse("F1Md");
        rig.smc.clear_writes();
        let reply = rig.send(1, Request::SetManual { fan: fan(1) });
        assert_eq!(reply, Reply::Refused(HelperError::ModeLocked { fan: 1 }));
        // Fan 0 went back before the error was answered.
        assert_eq!(rig.smc.ui8("F0Md"), 0);
        assert_eq!(rig.smc.ui8("Ftst"), 0);
        assert_eq!(rig.root.holder(), None);
    }

    #[test]
    fn sigterm_hands_back_when_ours() {
        let mut rig = Rig::new();
        rig.hold(1, 3000.0);
        rig.root.terminate();
        assert_eq!(rig.smc.ui8("F0Md"), 0);
        assert_eq!(rig.smc.ui8("F1Md"), 0);
        assert_eq!(rig.root.holder(), None);
    }
}
