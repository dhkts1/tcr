//! `tcr-fan-helper`: the one piece of TcrBar that runs as root, because the
//! kernel refuses a fan write from anyone else (`kIOReturnNotPrivileged`,
//! measured 2026-10-18).
//!
//! # What it is
//!
//! A launchd daemon, registered by TcrBar with `SMAppService.daemon` from
//! `Contents/Library/LaunchDaemons/` and started by launchd on demand, when
//! the proxy first sends to its Mach service. It answers four operations
//! (version, set-manual, set-target, hand-back; `tcr_fan::wire`) and does
//! nothing else: no policy, no timer of its own beyond the disconnect window,
//! no file, no path, no shell, no child process. Every decision is in
//! `tcr_fan::root`, where tests run it over a fake SMC without root.
//!
//! # Who may call
//!
//! Only the proxy of this build: `tcr` signed `io.github.dhkts1.tcr` (or `.dev`)
//! with the project's Developer ID team clause. The requirement is set on the
//! listener before it is activated, so no message is read from anyone else.
//! Which build this helper is comes from its own signature
//! (`FanIdentity::of_this_process`): a helper signed ad hoc matches neither
//! and exits 1 without listening.
//!
//! # When it stops
//!
//! When it is idle: no connection, no holder and no disconnect window. When the
//! proxy holding the fans disconnects (it quit, crashed or was killed), root
//! hands the fans back at once if they are still the ones it set, and retries
//! over about a minute if that fails; it then exits. launchd's SIGTERM hands
//! back once and exits.
//!
//! # The recovery limit
//!
//! If all seven hand-back attempts fail, the fans stay manual at the last
//! target until the next proxy start (its record still matches, so it resumes
//! or hands back), `tcr fan auto --force`, or a reboot. Every failed attempt is
//! logged at `LOG_CRIT`, meant to surface as a fault in the unified log; which
//! level `log show` actually reports for it is checked on hardware and has not
//! been observed yet.

#[cfg(target_os = "macos")]
mod helper {
    use std::ffi::{CString, c_void};
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    use std::time::Duration;

    use tcr_fan::identity::{FanIdentity, Role};
    use tcr_fan::root::{Level, Next, PeerId, Root, WindowId};
    use tcr_fan::wire::{self, MemDict};
    use tcr_fan::xpc::{self, Listener, Queue, Service, XpcDict, XpcError};
    use tcr_smc::{FanWriter, SmcConnection};

    /// How long a freshly started helper waits for its first connection. launchd
    /// starts it because a message is waiting, so one arrives at once; a request
    /// the listener's requirement dropped may leave nothing to wait for.
    const STARTUP_IDLE_GRACE: Duration = Duration::from_secs(10);

    /// The commit this helper was built from, as `build-tcrbar.sh` passes it in.
    const BUILD: &str = match option_env!("TCR_BUILD_SHA") {
        Some(sha) => sha,
        None => "unknown",
    };

    struct State {
        root: Mutex<Root<SmcConnection>>,
        queue: Arc<Queue>,
    }

    /// Set once in `main`, read by the dispatch callbacks, all of which run on
    /// the one serial queue: the mutex never waits.
    static STATE: OnceLock<State> = OnceLock::new();

    fn log(level: Level, line: &str) {
        let priority = match level {
            Level::Notice => libc::LOG_NOTICE,
            Level::Error => libc::LOG_ERR,
            Level::Critical => libc::LOG_CRIT,
        };
        let Ok(line) = CString::new(line.replace('\0', "?")) else {
            return;
        };
        // SAFETY: a constant format string with one `%s`, given one live C string.
        unsafe { libc::syslog(priority, c"%s".as_ptr(), line.as_ptr()) }
    }

    fn quit(code: i32) -> ! {
        // SAFETY: ends the process; nothing is borrowed across it.
        unsafe { libc::exit(code) }
    }

    /// Run `f` on root. Every caller is on the serial queue.
    fn with_root<T>(f: impl FnOnce(&mut Root<SmcConnection>) -> T) -> Option<T> {
        let state = STATE.get()?;
        let mut root = state.root.lock().unwrap_or_else(PoisonError::into_inner);
        Some(f(&mut root))
    }

    fn act(next: Next) {
        match next {
            Next::Stay => {}
            Next::Exit => {
                log(
                    Level::Notice,
                    "idle: no connection, no holder, no window; exiting",
                );
                quit(0);
            }
            Next::RetryAfter { window, after } => {
                if let Some(state) = STATE.get() {
                    state.queue.after(after, window.get(), window_due);
                }
            }
        }
    }

    extern "C" fn window_due(context: *mut c_void) {
        let id = WindowId::from_raw(context as usize as u64);
        if let Some(next) = with_root(|root| root.window_due(id)) {
            act(next);
        }
    }

    extern "C" fn idle_check(_context: *mut c_void) {
        if with_root(|root| root.is_idle()) == Some(true) {
            log(Level::Notice, "no connection arrived; exiting");
            quit(0);
        }
    }

    extern "C" fn on_sigterm(_context: *mut c_void) {
        if with_root(Root::terminate).is_none() {
            log(Level::Error, "SIGTERM before setup finished");
        }
        quit(0);
    }

    struct Helper;

    impl Service for Helper {
        fn connected(&self, peer: PeerId) {
            if with_root(|root| root.connected(peer)).is_none() {
                log(Level::Error, "a connection arrived before setup finished");
            }
        }

        fn message(&self, peer: PeerId, request: &XpcDict<'_>) -> MemDict {
            let decoded = wire::decode_request(request);
            let reply = with_root(|root| root.request(peer, decoded))
                .unwrap_or(wire::Reply::Refused(wire::HelperError::SmcUnavailable));
            let mut out = MemDict::new();
            wire::encode_reply(&reply, &mut out);
            out
        }

        fn disconnected(&self, peer: PeerId) {
            if let Some(next) = with_root(|root| root.disconnected(peer)) {
                act(next);
            }
        }

        fn listener_error(&self, error: XpcError) {
            log(Level::Error, &format!("listener: {error}"));
        }
    }

    pub(crate) fn main() -> Result<(), String> {
        // SAFETY: the identity is a static C string, as `openlog` keeps the
        // pointer for the life of the process.
        unsafe { libc::openlog(c"tcr-fan-helper".as_ptr(), libc::LOG_PID, libc::LOG_DAEMON) };

        let identity = FanIdentity::of_this_process(Role::Helper).map_err(|e| {
            log(Level::Error, &format!("not starting: {e}"));
            e.to_string()
        })?;
        let label = identity.helper_label();
        let fail = |what: &str, e: XpcError| {
            let line = format!("not starting: {what}: {e}");
            log(Level::Error, &line);
            line
        };

        let queue = Arc::new(Queue::serial(label).map_err(|e| fail("queue", e))?);
        let root = Root::new(
            Box::new(|| SmcConnection::open().map(FanWriter::new)),
            BUILD,
            Box::new(log),
        );
        if STATE
            .set(State {
                root: Mutex::new(root),
                queue: Arc::clone(&queue),
            })
            .is_err()
        {
            return Err("state set twice".into());
        }

        // SAFETY: ignoring SIGTERM's default action is what lets the dispatch
        // source below see it instead of the process dying first.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        let _sigterm = queue
            .on_signal(libc::SIGTERM, on_sigterm)
            .map_err(|e| fail("SIGTERM source", e))?;

        let _listener = Listener::start(
            label,
            identity.client_requirement(),
            &queue,
            Arc::new(Helper),
        )
        .map_err(|e| fail("listener", e))?;
        queue.after(STARTUP_IDLE_GRACE, 0, idle_check);
        log(
            Level::Notice,
            &format!("listening on {label} ({identity} build {BUILD})"),
        );
        xpc::run()
    }
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), String> {
    helper::main()
}

/// The helper is a macOS launchd daemon; anywhere else it only says so.
#[cfg(not(target_os = "macos"))]
fn main() -> Result<(), String> {
    Err("tcr-fan-helper runs only on macOS".to_owned())
}
