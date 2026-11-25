//! The proxy's boot sequence as a **library** call.
//!
//! This is the body of what used to be `main.rs::run_server`, with the two
//! things a library may not do removed:
//!
//! * it never calls `std::process::exit` — the stand-down is returned as a
//!   [`StandDown`] value and the *binary* maps it to an exit code;
//! * it never blocks until shutdown — [`serve`] returns as soon as the listener
//!   is bound, handing back a [`ServerHandle`] that OWNS every task it spawned.
//!   The caller decides how to wait (`tcr` runs the TUI or blocks on Ctrl-C; a
//!   test issues one request and shuts down).
//!
//! Everything else is unchanged on purpose: the same takeover decision, the same
//! affinity restore/flush, the same background loops, the same `server started`
//! boot marker emitted only after a SUCCESSFUL bind.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures::FutureExt as _;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::base::config::Config;
use crate::manager::Manager;
use crate::{
    base::build_info, base::tls, proxy::live_client, proxy::mitm, stores::affinity,
    stores::session_wire_persist, stores::singleton,
};

#[cfg(unix)]
pub mod handoff;
mod peer_mesh;
use peer_mesh::{boot_peer_listener, peers_file_beside_config};

/// How long [`ServerHandle::shutdown`] waits for a task to stop before aborting
/// it. Long enough for a loop to finish an iteration and an in-progress atomic
/// write to land; short enough that quitting `tcr` on a wedged filesystem is a
/// pause and not a hang.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

// The floor on what `ShutdownHandle::shutdown_within` gives the usage ledger to
// drain. The ledger flush runs LAST, so a shutdown that spent its whole grace on
// a wedged task would otherwise hand it zero and lose the queue by construction.
// Defined in `usage`, which owns the shutdown it bounds and floors the writer
// join at the same value: two copies of "how long is a real attempt" is how the
// two steps come to disagree.
use crate::records::usage::MIN_LEDGER_SHUTDOWN;

/// Where the MITM listener's TLS material comes from.
///
/// `tcr` always uses [`TlsSetup::Load`], which is what `run_server` did inline.
/// [`TlsSetup::Disabled`] exists because loading mints/reads a CA on disk, and a
/// caller that only needs base-URL mode (an in-process test) must be able to opt
/// out of that side effect rather than work around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsSetup {
    /// Load (or mint) the MITM CA + leaf, as the binary does. A failure is
    /// non-fatal: base-URL mode still serves, CONNECT answers 503.
    #[default]
    Load,
    /// Do not touch the TLS material at all. CONNECT answers 503.
    Disabled,
}

/// What [`serve`] may do to a process that already holds the port.
///
/// This was a `bool` named `replace`, which is how the most destructive
/// operation in this system ended up one keystroke from every caller: setting it
/// reaches [`singleton::takeover_port`], which SIGTERMs and then SIGKILLs a
/// command-verified proxy holding the port, wiping its session→account pin map.
/// Under the `--replace` flag that is the intended, operator-typed recovery.
/// From an embedder or a test it is a catastrophe with the same spelling.
///
/// So the signalling choices are not values a caller can land on — they are
/// *named constructors* that say what they do, and the private field means
/// `Default`, struct-literal syntax and `..Default::default()` can only ever
/// produce [`never_signal`](Self::never_signal). "Follow the docs and you cannot
/// kill anything" is the property being bought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IncumbentPolicy(Signal);

/// The private half of [`IncumbentPolicy`]. Deliberately unnameable outside this
/// module so no caller can construct the signalling variants directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Signal {
    /// Signal nothing, ever. A recognized incumbent means stand down.
    #[default]
    Never,
    /// The binary's default (`--no-replace`): signal nothing, stand down for a
    /// `tcr` peer, and say so on stderr (and name a non-proxy holder) the way
    /// TcrBar reads it.
    Report,
    /// `--replace`: replace whichever recognized proxy holds the port.
    Recognized,
}

impl IncumbentPolicy {
    /// **The default.** Send no signal to any process under any circumstance; a
    /// recognized proxy on the port produces [`ServeOutcome::StoodDown`].
    ///
    /// The only correct policy for a library caller or a test, which is why it
    /// is what you get for free.
    pub const fn never_signal() -> Self {
        Self(Signal::Never)
    }

    /// `tcr server` without `--replace`: signal nothing, and stand down for a
    /// `tcr` peer on the port with the stand-down message TcrBar parses
    /// ([`singleton::INCUMBENT_MARKER`]).
    pub const fn report_and_stand_down() -> Self {
        Self(Signal::Report)
    }

    /// Legacy explicit takeover: **SIGTERM, then SIGKILL after 800ms**, whichever
    /// recognized proxy holds the port — including a live `tcr` serving real
    /// traffic. This cuts its live connections. The CLI's `--replace` now uses
    /// listener handoff instead, leaving accepted connections to drain.
    ///
    /// Reserve this for an operator who explicitly requested a destructive
    /// takeover. An embedder should use `never_signal`.
    pub const fn kill_the_incumbent_proxy() -> Self {
        Self(Signal::Recognized)
    }

    /// Whether this policy can signal a process at all — for a caller that wants
    /// to assert it is holding the harmless one.
    pub const fn signals_anything(&self) -> bool {
        matches!(self.0, Signal::Recognized)
    }
}

/// Everything [`serve`] needs, derived from what `run_server` actually read.
///
/// Every field that can reach outside this process — the config file, the pin
/// cache, the incumbent on the port — defaults to the inert choice, so the
/// *dangerous* configuration is the one you have to spell out. See
/// [`ServeOptions::new`].
///
/// Note what is NOT here. `--headless` is not a serving parameter: it selects
/// the *logging subscriber* and *how the caller waits*, both of which stay with
/// the binary. The config is passed already loaded rather than as a path,
/// because loading it prints operator-facing `[tcr]` diagnostics and decides
/// whether the file may be written back — a binary concern (`main::load_config`).
pub struct ServeOptions {
    /// The already-loaded config.
    pub config: Config,
    /// Where the config may be written back, or `None` to make every persist a
    /// no-op (a corrupt file must never be clobbered with defaults).
    ///
    /// `None` also means **refreshed OAuth tokens are never written to disk**.
    /// Anthropic's refresh tokens are single-use, so a long-lived embedder that
    /// leaves this `None` while pointing at a real config leaves already-spent
    /// tokens on disk and every account fails to refresh on the next boot. If
    /// you serve from a real config, pass its path.
    pub persist_path: Option<PathBuf>,
    /// Overrides `config.proxy.port` when set — the `--port` flag. `Some(0)`
    /// binds an ephemeral port, which is how a test gets a real server without
    /// contending for the configured one.
    pub port: Option<u16>,
    /// What to do about a recognized proxy already holding the port. Defaults to
    /// [`IncumbentPolicy::never_signal`]. The CLI's `--replace` supplies an
    /// inherited listener and uses this default rather than signalling its peer.
    pub incumbent: IncumbentPolicy,
    /// The session-affinity pin cache, or `None` to keep pins **in memory only**
    /// — nothing is read at boot and nothing is written at shutdown.
    ///
    /// `None` is the default because the binary's path ([`affinity::default_path`])
    /// is one shared file: a second process that serves briefly and shuts down
    /// atomically replaces the live proxy's pin map with its own (usually empty)
    /// one, and the live proxy — whose flusher only writes when its map changed —
    /// never repairs it. The next boot then cold-starts every session's prompt
    /// cache. Point this somewhere disposable, or leave it `None`.
    pub affinity_path: Option<PathBuf>,
    /// The Sessions/Tools panel cache (`tcr status --json`'s `sessions` array —
    /// `docs/design/panel-tabs.md`), or `None` to keep it **in memory only** —
    /// nothing is read at boot and nothing is written at shutdown, so a restart
    /// empties the panel exactly as it always has.
    ///
    /// `None` is the default for the same reason as [`Self::affinity_path`]: the
    /// binary's path ([`crate::stores::session_wire_persist::default_path`]) is one
    /// shared file, and a second process that serves briefly would overwrite
    /// the live proxy's Sessions/Tools cache with its own (usually empty) one.
    /// Point this somewhere disposable, or leave it `None`.
    pub wire_sessions_path: Option<PathBuf>,
    /// The usage-ledger DIRECTORY, or `None` to keep usage **in memory only** —
    /// nothing is replayed at boot and nothing is written, so a restart starts
    /// the day at zero.
    ///
    /// `None` is the default for the same reason as [`Self::affinity_path`]:
    /// the binary's path ([`crate::records::usage::default_dir`]) is one shared
    /// directory, and a second process that serves briefly would append its
    /// traffic into the live proxy's day file and replay the live proxy's
    /// traffic into its own totals. Point this somewhere disposable, or leave
    /// it `None`.
    pub usage_dir: Option<PathBuf>,
    /// Where the MITM TLS material comes from.
    pub tls: TlsSetup,
    /// What is hosting this proxy — a standalone `tcr` process
    /// ([`singleton::ProxyHost::Cli`]) or an application serving it in-process
    /// ([`singleton::ProxyHost::Embedded`]).
    ///
    /// **Every caller states this; the library never infers it.** Inferring it
    /// from `argv[0]` is precisely the bug the owner file exists to fix (see
    /// [`crate::stores::singleton`]): an embedded proxy's `argv[0]` is the host
    /// application's, which the name matcher does not recognize at all, and the
    /// consequence is `tcr login` no longer refusing to run beside a live server
    /// that will then overwrite its fresh single-use refresh tokens.
    ///
    /// Recorded in the owner file, and only there — so it has no effect unless
    /// [`Self::owner_dir`] is set.
    pub host: singleton::ProxyHost,
    /// The DIRECTORY to write the port claim in, or `None` to write no claim at
    /// all. The file name inside it is not the caller's to choose: [`serve`]
    /// derives it with [`singleton::owner_path_in`] from the port it actually
    /// bound.
    ///
    /// A directory rather than a path, because the NAME is a contract. Every
    /// reader — [`singleton::live_proxy_server`], [`singleton::takeover_port`] —
    /// looks the claim up as `proxy-owner-<port>.json`; a claim written under any
    /// other name is consulted by nothing, with no error anywhere. Handing the
    /// caller a free-form `owner_path` made that a one-typo failure, and worse, it
    /// let the caller name the file after a port it did not bind: `port: Some(0)`
    /// resolves to an ephemeral port at bind time, so the name and the contents
    /// disagreed and the proxy stayed invisible while the write logged success.
    ///
    /// `None` is the default because the directory is *shared state*: a second
    /// process serving briefly would otherwise leave its own claim where the live
    /// proxy's belongs. The binary passes [`singleton::default_owner_dir`]; a test
    /// points it somewhere disposable.
    ///
    /// Omitting it is safe, never silent: identity then falls back to the
    /// command-line matcher, which is what every `tcr` did before this file
    /// existed. It is only an *embedded* proxy that the matcher cannot see, and an
    /// embedder must therefore pass a directory.
    pub owner_dir: Option<PathBuf>,
    /// A listening socket handed over by a predecessor instead of bound here,
    /// with the handoff channel it came over.
    ///
    /// When set, this proxy does not bind and does not consult the incumbent
    /// policy: the predecessor gave us its socket, so there is no port to
    /// contest and nothing to signal. The port therefore never goes unbound
    /// across the swap, and a connection arriving mid-handoff waits in the
    /// kernel's accept queue instead of being refused.
    ///
    /// The channel closes when the predecessor's usage ledger is on disk; see
    /// [`crate::stores::restart::Handover`] for what this process does then.
    ///
    /// The caller is responsible for having verified who it took this from.
    /// `serve` cannot: by the time the descriptor is in hand the peer may
    /// already be gone, and a socket is not self-describing.
    pub handover: Option<crate::stores::restart::Handover>,
}

impl ServeOptions {
    /// Serve this config while touching **nothing outside this process**: no
    /// config write-back, no pin cache, no signal to whatever holds the port,
    /// and (via [`TlsSetup::Load`]) the same TLS material the binary uses.
    ///
    /// This deliberately is NOT "the binary's defaults" — it used to say so
    /// while defaulting `persist_path` to `None`, which is the opposite of what
    /// the binary passes. The binary spells its own options out in
    /// `main::run_server`, because every one of them is a decision about files
    /// and processes a library caller must not make by accident.
    pub const fn new(config: Config) -> Self {
        Self {
            config,
            persist_path: None,
            port: None,
            incumbent: IncumbentPolicy::never_signal(),
            affinity_path: None,
            wire_sessions_path: None,
            usage_dir: None,
            tls: TlsSetup::Load,
            // Inert, like every other field here: with no owner dir, `host` is
            // recorded nowhere and this value cannot be read by anyone. A caller
            // that DOES claim the port must state its host, and both callers that
            // write a file spell the pair out together.
            host: singleton::ProxyHost::Cli,
            owner_dir: None,
            handover: None,
        }
    }
}

/// A recognized proxy incumbent holds the port, so we did not bind.
///
/// Returned as DATA. The exit code, the build line and the operator warning are
/// the binary's job (`main::run_server`); a library caller gets the same facts
/// without a process exit and can decide for itself.
pub struct StandDown {
    /// The port that was contended.
    pub port: u16,
    /// The incumbent's pid, as `singleton` identified it.
    pub pid: u32,
    /// WHICH proxy that pid is. Carried because it decides what a caller may tell
    /// the operator to do about it: [`singleton::ProxyKind::TcrEmbedded`] means
    /// the pid belongs to a host application, so neither a signal nor `--replace`
    /// is an available recovery — advising either is advising the loss of the
    /// app's shutdown and its final session→account pin write.
    pub kind: singleton::ProxyKind,
    /// ONE probe of the incumbent: which build it runs, and whether it answers
    /// at all. Both halves are needed to pick an exit code.
    pub probe: live_client::IncumbentProbe,
    /// The build comparison, verdict + human line produced together.
    pub report: build_info::StandDownReport,
}

/// What [`serve`] did.
pub enum ServeOutcome {
    /// The listener is bound and every background task is running.
    Started(ServerHandle),
    /// An incumbent holds the port and was deliberately left alone. Nothing was
    /// bound and nothing was spawned.
    StoodDown(StandDown),
}

impl ServeOutcome {
    /// The handle, or a panic — for callers (tests) that require a bound server.
    ///
    /// # Panics
    /// If the outcome was a stand-down.
    pub fn expect_started(self) -> ServerHandle {
        match self {
            Self::Started(handle) => handle,
            Self::StoodDown(stand_down) => panic!(
                "expected a bound server; an incumbent (pid {}) holds :{}",
                stand_down.pid, stand_down.port
            ),
        }
    }
}

/// What one cache file's final write on a clean shutdown did. One type for all three
/// ([`SavedCache`]), because the three had each grown their own copy of these arms.
///
/// Not an `Option<usize>`: "this cache is off" and "the write FAILED and the entries are lost"
/// are the same value in an `Option`, and the only place the difference survived was a
/// `tracing::warn!`, which a library embedder, having installed no subscriber, never sees. A
/// caller reading a clean report while the next boot starts cold is exactly the silence this
/// type exists to break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheFlush {
    /// Nothing to write: the cache is off in the config, or no path was configured for it
    /// ([`ServeOptions::affinity_path`] or [`ServeOptions::wire_sessions_path`] was `None`).
    Disabled,
    /// Nothing had changed since the last save, so nothing was written. Only the bound-token
    /// map reports this: its file can be megabytes, so its final write is skipped when clean.
    Unchanged,
    /// The entries were written for the next boot. Zero is a normal count.
    Written(usize),
    /// The write failed and the entries are **lost**. Carries the rendered error because the
    /// caller may have no tracing subscriber to read the warning in.
    Failed(String),
}

impl CacheFlush {
    /// The entry count when one was actually written, else `None`.
    pub const fn written(&self) -> Option<usize> {
        match self {
            Self::Written(count) => Some(*count),
            _ => None,
        }
    }

    /// Did a write that was supposed to happen fail? The one condition a caller should
    /// surface even though shutdown is not fallible.
    pub const fn failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// What a clean [`ServerHandle::shutdown`] did, so a caller can assert on it
/// instead of trusting that the function returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// How many tasks the handle owned and joined: the accept loop plus every
    /// background loop that was actually spawned for this config.
    pub tasks_joined: usize,
    /// How many tasks did NOT stop inside the grace period and were aborted.
    /// Non-zero means a loop was wedged — the flush below still ran, which is
    /// the whole reason the grace exists.
    pub tasks_aborted: usize,
    /// What the final bound-token map write did.
    pub bound_tokens: CacheFlush,
    /// What the final pin write did.
    pub affinity: CacheFlush,
    /// What the final Sessions/Tools cache write did.
    pub wire_sessions: CacheFlush,
    /// What the usage ledger's drain-and-join did: `Flushed` means every line
    /// this process queued is on disk, `Abandoned` means the writer outlasted
    /// the grace and its tail is gone.
    pub ledger: crate::records::usage::LedgerShutdown,
}

/// A bound, running proxy — and the owner of every task [`serve`] spawned.
///
/// Ownership is the point. `run_server` could hand its background loops to "the
/// process is about to exit"; a library caller cannot, so dropping this handle
/// aborts them and [`shutdown`](Self::shutdown) stops them in order and gives
/// the affinity map its final write.
pub struct ServerHandle {
    /// A second descriptor keeps the accept queue bound through a handoff.
    listener: Option<std::net::TcpListener>,
    connections: Option<tokio::sync::mpsc::Receiver<()>>,
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    manager: Arc<Manager>,
    affinity_path: Option<PathBuf>,
    wire_sessions_path: Option<PathBuf>,
    /// The port claim written after the bind, to be removed on shutdown. `None`
    /// when the caller asked for no claim (or the write failed — a claim that was
    /// never written is nothing to remove).
    owner_path: Option<PathBuf>,
    /// Tasks joined and tasks aborted so far. Kept on the handle, not in a local,
    /// so a `shutdown` future that is dropped mid-join and re-issued reports the
    /// whole truth rather than only what the last attempt saw.
    tasks_joined: usize,
    tasks_aborted: usize,
    /// The `mitm::serve` accept loop. `None` once [`Self::shutdown`] joined it.
    server: Option<JoinHandle<()>>,
    /// Whether that task has already been awaited to completion. A `JoinHandle`
    /// may only be polled to completion once, and [`Self::serving_stopped`] can
    /// get there before [`Self::shutdown`] does.
    server_finished: bool,
    /// The affinity flusher / quota prober / keep-warm loops, whichever this
    /// config enabled.
    background: Vec<JoinHandle<()>>,
    /// The LAN peer mesh's second socket, when this boot started one. See
    /// [`boot_peer_listener`]. `None` is the default and the whole of the
    /// feature flag: a peers file with no `listen` opens no port.
    ///
    /// Held so a caller (and a test) can learn the port the kernel assigned for
    /// a `listen` of `:0` without reading the peers file back, the same way
    /// [`Self::addr`] answers it for the proxy's own socket.
    peer_addr: Option<SocketAddr>,
    /// The handoff channel, once [`Self::handoff`] has sent the listener over
    /// it. Held until [`Self::shutdown_within`] has drained the usage ledger:
    /// its close tells the successor the day file now holds every line this
    /// process will write, which is when it recounts (see
    /// [`crate::stores::restart::Handover`]).
    successor: Option<std::os::unix::net::UnixStream>,
}

impl ServerHandle {
    /// The address actually bound — the resolved port when `0` was requested.
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The live [`Manager`], for a caller that wants the same state the TUI reads.
    pub const fn manager(&self) -> &Arc<Manager> {
        &self.manager
    }

    /// The LAN peer mesh's socket, if this boot opened one.
    ///
    /// `None` is a fresh install, and every install that has not written a
    /// `listen` into its peers file: the port is opt-in and there is no second
    /// switch.
    pub const fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    /// How many background loops this config spawned (the accept loop excluded).
    pub const fn background_task_count(&self) -> usize {
        self.background.len()
    }

    /// Resolves when the accept loop stops **on its own** — which in practice
    /// means it panicked, since it otherwise runs until shutdown. Pends forever
    /// once that has happened (or if shutdown already joined it), so it is safe
    /// to park a `select!` arm on it.
    ///
    /// Cancel-safe: nothing is taken out of `self` until the join actually
    /// completes, so losing the race leaves the task owned — and therefore still
    /// aborted by `Drop`.
    pub async fn serving_stopped(&mut self) {
        if self.server_finished {
            std::future::pending::<()>().await;
        }
        let result = match self.server.as_mut() {
            Some(server) => server.await,
            None => std::future::pending().await,
        };
        self.server_finished = true;
        if let Err(err) = result {
            tracing::error!(error = %err, "server task join error");
        }
    }

    /// Stop acceptance and background mutation, flush the predecessor, then
    /// transfer the still-bound listener. The receiver starts only after the
    /// existing handoff ownership gate has been released.
    pub async fn handoff(&mut self, peer: &std::os::unix::net::UnixStream) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + DEFAULT_SHUTDOWN_GRACE;
        self.flush_cache_finally(SavedCache::BoundTokens);
        self.flush_cache_finally(SavedCache::WireSessions);
        let listener = self.listener.as_ref().context("no listener to hand over")?;
        handoff::complete_handoff(&self.manager, self.affinity_path.as_deref(), listener, peer)?;
        // The listener is on its way, so the successor is serving or about to
        // be. From here the channel stays open until the ledger is on disk,
        // whatever the caller does with its own copy and whatever fails below.
        match peer.try_clone() {
            Ok(channel) => self.successor = Some(channel),
            Err(error) => tracing::warn!(
                %error,
                "could not keep the handoff channel open; the successor will recount usage \
                 before this process's last lines are written"
            ),
        }
        crate::base::log_err::LogErr::log_debug(
            self.shutdown.send(true),
            "signalling server shutdown",
        );
        // The controller also mutates hardware and saves its wish. Finish its
        // last save before the successor can take ownership and save Auto;
        // accepted connections may keep draining after that release.
        if let Some(fan) = self.manager.fan() {
            let fan = Arc::clone(fan);
            let stop = tokio::task::spawn_blocking(move || fan.shutdown(DEFAULT_SHUTDOWN_GRACE));
            let stopped = tokio::time::timeout_at(deadline, stop)
                .await
                .context("fan control exceeded the handoff shutdown grace")?
                .context("fan control shutdown task failed")?;
            anyhow::ensure!(stopped, "fan control did not stop before handoff");
        }
        self.stop_tasks(deadline).await;
        crate::stores::restart::wait_for_listener_receipt(peer).await?;
        self.listener.take();
        crate::stores::restart::confirm_listener_released(peer)?;
        Ok(())
    }

    /// Wait for accepted connections after HTTP graceful shutdown. The binary
    /// bounds this wait and continues polling termination signals.
    pub async fn drain_connections(&mut self) {
        if let Some(connections) = self.connections.as_mut() {
            connections.recv().await;
        }
        self.connections.take();
    }

    async fn stop_tasks(&mut self, deadline: tokio::time::Instant) {
        if let Some(server) = self.server.as_mut() {
            // `&mut JoinHandle` polls the join without consuming it: losing the
            // race to the caller's own timeout leaves the task owned here.
            let stopped = self.server_finished
                || if tokio::time::timeout_at(deadline, &mut *server)
                    .await
                    .is_ok()
                {
                    true
                } else {
                    // A wedged synchronous write cannot be awaited after abort.
                    server.abort();
                    false
                };
            self.server = None;
            if stopped {
                self.server_finished = true;
                self.tasks_joined += 1;
            } else {
                self.tasks_aborted += 1;
            }
        }
        while let Some(task) = self.background.last_mut() {
            if tokio::time::timeout_at(deadline, &mut *task).await.is_ok() {
                self.tasks_joined += 1;
            } else {
                task.abort();
                self.tasks_aborted += 1;
            }
            self.background.pop();
        }
    }

    /// Stop serving, then flush.
    ///
    /// The order is `run_server`'s: stop the accept loop, persist the config
    /// (refreshed tokens are already written incrementally; this is the final
    /// belt-and-braces write), then write the session-affinity pins.
    ///
    /// **In-flight connections are left to finish.** Each accepted connection
    /// runs on its own detached task, exactly as before this extraction, so
    /// cancelling the accept loop has never cut one; a proxied response can be a
    /// long stream and killing it mid-flight would be a behaviour change and a
    /// worse one. What stops immediately is *accepting new* connections — the
    /// listener is dropped with the accept loop, so the port refuses at once.
    ///
    /// Bounded by [`DEFAULT_SHUTDOWN_GRACE`] and **cannot hang** — see
    /// [`shutdown_within`](Self::shutdown_within) for why that is not optional.
    pub async fn shutdown(&mut self) -> ShutdownReport {
        self.shutdown_within(DEFAULT_SHUTDOWN_GRACE).await
    }

    /// [`shutdown`](Self::shutdown) with the join deadline spelled out.
    ///
    /// # Why there is a deadline at all
    ///
    /// `run_server` called `server.abort()` and flushed immediately; it could not
    /// hang. Joining instead is better — a loop gets to finish its current
    /// iteration — but only if the join is bounded, because the affinity flusher
    /// performs a **blocking `std::fs` write inside async code**
    /// (`flush_affinity` → `config::write_atomic`) and cancellation only lands at
    /// an await point. On a full, slow or hung filesystem an unbounded join means
    /// `tcr` quits into a hang with the terminal already restored, no listener
    /// bound, no prompt — and `persist_now` never runs. So a task that has not
    /// stopped within `grace` is aborted, counted in
    /// [`ShutdownReport::tasks_aborted`], and left behind; the config persist and
    /// the final pin write happen either way.
    ///
    /// # Cancel-safety
    ///
    /// Takes `&mut self`, and a task is removed from the handle only once it has
    /// actually been joined. A caller that bounds this with its own deadline and
    /// drops the future keeps a usable handle, keeps every un-joined task owned
    /// (so `Drop` still aborts them), and may simply call it again — the counters
    /// carry over and the flushes are idempotent.
    pub async fn shutdown_within(&mut self, grace: Duration) -> ShutdownReport {
        self.listener.take();
        crate::base::log_err::LogErr::log_debug(
            self.shutdown.send(true),
            "signalling server shutdown",
        );
        let deadline = tokio::time::Instant::now() + grace;

        // Fans first, while the process is still whole: the controller hands
        // back what it holds within `fan::SHUTDOWN_HAND_BACK` (3 s, inside
        // this grace). It blocks, so it waits on the blocking pool; if it runs
        // over, the helper connection closes with the process and root hands
        // back on its own.
        if let Some(fan) = self.manager.fan() {
            let fan = Arc::clone(fan);
            let stop = tokio::task::spawn_blocking(move || fan.shutdown(grace));
            if tokio::time::timeout_at(deadline, stop).await.is_err() {
                tracing::warn!("fan control did not stop inside the shutdown grace");
            }
        }

        self.stop_tasks(deadline).await;

        // Withdraw the port claim once the accept loop is done, so the next `tcr`
        // does not read a claim for a proxy that has stopped listening. Ordered
        // after the joins above and before the persists below for exactly that
        // reason. Taken (not just read) so a re-issued `shutdown` does not try
        // again — and a leftover file is harmless anyway: `singleton` re-checks the
        // pid against the live listeners before believing any claim.
        //
        // Removed only if it still names US: the listener was freed at the top of
        // this function while the joins below it can take hundreds of milliseconds,
        // so a successor may already have bound the port and written its own claim
        // to this same port-named path. See `singleton::remove_owner_file_if_owned`.
        if let Some(path) = self.owner_path.take() {
            singleton::remove_owner_file_if_owned(&path, std::process::id(), self.addr.port());
        }

        self.manager.persist_now();

        // The three cache files' final write, one path, in the order they have always been
        // written: bound tokens, pins, Sessions/Tools. Belt-and-braces for the changes inside the
        // last flusher interval; the flusher's own timer is what survives a SIGKILL, which is the
        // case that matters. Done here, after every task is joined or aborted, and not inside
        // the bound-token flusher task as it was: a flusher aborted at the grace deadline lost
        // that write, and its failure was logged and reported nowhere.
        let bound_tokens = self.flush_cache_finally(SavedCache::BoundTokens);
        let affinity = self.flush_cache_finally(SavedCache::Affinity);
        let wire_sessions = self.flush_cache_finally(SavedCache::WireSessions);

        // Then the usage ledger, LAST, because it is the one flush whose input
        // is still arriving: an in-flight request that finishes during the joins
        // above records its usage, and that line is in the writer's queue rather
        // than on disk. Bounded by whatever is left of the caller's grace — a
        // stalled volume must not turn a quit into a hang — with a floor, so a
        // shutdown that has already spent its whole budget still gets one real
        // attempt instead of a guaranteed loss.
        let ledger_budget = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .max(MIN_LEDGER_SHUTDOWN);
        let ledger = self.shutdown_ledger_within(ledger_budget).await;
        match ledger {
            crate::records::usage::LedgerShutdown::NotAttached => {}
            crate::records::usage::LedgerShutdown::Flushed => tracing::info!(
                dropped_lines = self.manager.usage_dropped_lines(),
                "the usage ledger drained and closed; today's totals will replay at the next boot"
            ),
            crate::records::usage::LedgerShutdown::Abandoned => tracing::warn!(
                budget_ms = ledger_budget.as_millis(),
                dropped_lines = self.manager.usage_dropped_lines(),
                "the usage ledger did not drain inside the shutdown budget; the last requests \
                 it served are not on disk"
            ),
        }
        // Only now may a successor recount the day file: everything this
        // process will ever write to it is there (or reported lost above).
        drop(self.successor.take());

        ShutdownReport {
            tasks_joined: self.tasks_joined,
            tasks_aborted: self.tasks_aborted,
            bound_tokens,
            affinity,
            wire_sessions,
            ledger,
        }
    }

    /// The ledger drain, on a BLOCKING thread and awaited with a timeout.
    ///
    /// `UsageTracker::shutdown_ledger` is synchronous through and through — a
    /// retry loop around `try_send`, a `recv_timeout`, a writer-finish poll —
    /// and it can occupy its whole budget. Called inline it would be a
    /// multi-second stretch of an `async fn` with no await point in it, which
    /// breaks both promises this function makes: a caller that wraps it in its
    /// own `tokio::time::timeout` cannot interrupt it (cancellation only lands
    /// at an await), and a Tokio worker is held off its other tasks for the
    /// duration.
    ///
    /// So it runs on the blocking pool and this awaits it, bounded by the same
    /// budget. Dropping the returned future — the caller's timeout firing —
    /// stops the WAIT, not the drain: the blocking thread finishes the flush and
    /// the join on its own, which is the right half to keep going, and the
    /// process exits when it is done either way. A drain nobody is left to hear
    /// about is reported as `Abandoned`, which is what a caller that stopped
    /// waiting has to assume.
    async fn shutdown_ledger_within(
        &self,
        budget: Duration,
    ) -> crate::records::usage::LedgerShutdown {
        let manager = Arc::clone(&self.manager);
        let drain = tokio::task::spawn_blocking(move || manager.shutdown_usage_ledger(budget));
        // The drain's own worst case is the flush budget PLUS the writer join's
        // floor, so bounding this at `budget` alone would cut off exactly the
        // slow-but-alive writer the floor exists to let finish — and report a
        // completed drain as abandoned.
        let bound = budget + MIN_LEDGER_SHUTDOWN;
        match tokio::time::timeout(bound, drain).await {
            Ok(Ok(report)) => report,
            // The blocking pool is gone, or the drain panicked: either way this
            // process has no evidence the day file is complete.
            Ok(Err(err)) => {
                tracing::warn!(
                    error = %err,
                    "the usage-ledger drain did not run to completion"
                );
                crate::records::usage::LedgerShutdown::Abandoned
            }
            Err(_) => crate::records::usage::LedgerShutdown::Abandoned,
        }
    }

    /// The shutdown write of one cache file, as a value. Logs for the binary's operator, and
    /// returns the same fact for a caller with no subscriber.
    fn flush_cache_finally(&self, cache: SavedCache) -> CacheFlush {
        if self.manager.mutation_is_released() {
            return CacheFlush::Disabled;
        }
        let Some(path) = cache.path(
            self.affinity_path.as_deref(),
            self.wire_sessions_path.as_deref(),
        ) else {
            return CacheFlush::Disabled;
        };
        match cache.save_finally(&self.manager, &path) {
            Ok(Some(count)) => {
                tracing::info!(
                    path = %path.display(),
                    entries = count,
                    "{} written for the next boot",
                    cache.name()
                );
                CacheFlush::Written(count)
            }
            Ok(None) => CacheFlush::Unchanged,
            Err(err) => {
                cache.warn(&path, &err);
                CacheFlush::Failed(err.to_string())
            }
        }
    }
}

/// Dropping the handle must not leak the accept loop, the prober, the warmer or
/// the affinity flusher. The binary never relied on this (the process exited);
/// a library caller has nothing else to fall back on.
impl Drop for ServerHandle {
    fn drop(&mut self) {
        crate::base::log_err::LogErr::log_debug(
            self.shutdown.send(true),
            "signalling shutdown to a stopped server",
        );
        if let Some(server) = &self.server {
            server.abort();
        }
        for task in &self.background {
            task.abort();
        }
    }
}

/// Extract a human-readable message from a caught panic payload. Panics carry
/// either a `&'static str` (the common `panic!("literal")` case) or a `String`
/// (`panic!("{}", x)`, `.unwrap()`'s formatted message); anything else falls
/// back to a fixed string rather than failing to log at all.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// The three caches a restart reads back, each saved by [`save_on_change`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavedCache {
    /// `session-affinity.json`: see [`crate::stores::affinity`].
    Affinity,
    /// `server-tool-pins.json`: see [`crate::stores::bound_tokens`].
    BoundTokens,
    /// `session-wire.json`: see [`crate::stores::session_wire_persist`].
    WireSessions,
}

impl SavedCache {
    /// How long after a change the save runs, so a burst of changes is one write.
    const fn delay(self) -> Duration {
        match self {
            Self::Affinity => crate::manager::AFFINITY_SAVE_DELAY,
            Self::BoundTokens => crate::manager::BOUND_TOKENS_SAVE_DELAY,
            Self::WireSessions => crate::manager::WIRE_SESSIONS_SAVE_DELAY,
        }
    }

    /// Resolves once this cache has changed since the last time it resolved.
    async fn changed(self, manager: &Manager) {
        match self {
            Self::Affinity => manager.affinity_changed().await,
            Self::BoundTokens => manager.bound_tokens_changed().await,
            Self::WireSessions => manager.wire_sessions_changed().await,
        }
    }

    /// Clear the dirty flag, then save if the content differs from the file.
    fn save(
        self,
        manager: &Manager,
        path: &std::path::Path,
    ) -> Result<crate::stores::save_gate::SaveOutcome, crate::base::config::ConfigError> {
        match self {
            Self::Affinity => manager.flush_affinity_if_changed(path),
            Self::BoundTokens => manager.flush_bound_tokens_if_changed(path),
            Self::WireSessions => manager.flush_wire_sessions_if_changed(path),
        }
    }

    /// What the cache is called in a log line.
    const fn name(self) -> &'static str {
        match self {
            Self::Affinity => "session-affinity pins",
            Self::BoundTokens => "bound-token map",
            Self::WireSessions => "Sessions/Tools cache",
        }
    }

    /// Where the cache lives. The bound-token file is a sibling of the affinity file, keyed on
    /// its path, so `None` for the affinity path is `None` for both.
    fn path(
        self,
        affinity_path: Option<&std::path::Path>,
        wire_sessions_path: Option<&std::path::Path>,
    ) -> Option<PathBuf> {
        match self {
            Self::Affinity => affinity_path.map(std::path::Path::to_path_buf),
            Self::BoundTokens => affinity_path.map(crate::stores::bound_tokens::path_beside),
            Self::WireSessions => wire_sessions_path.map(std::path::Path::to_path_buf),
        }
    }

    /// The shutdown write: `Some(count)` when written, `None` when there was nothing to write
    /// (affinity off in the config, or a clean bound-token map).
    fn save_finally(
        self,
        manager: &Manager,
        path: &std::path::Path,
    ) -> Result<Option<usize>, crate::base::config::ConfigError> {
        match self {
            Self::Affinity if !manager.session_affinity_enabled() => Ok(None),
            Self::Affinity => manager.flush_affinity(path).map(Some),
            Self::BoundTokens => manager.flush_bound_tokens_if_dirty(path),
            Self::WireSessions => manager.flush_wire_sessions(path).map(Some),
        }
    }

    /// The warning a failed save logs, naming what the failure costs.
    fn warn(self, path: &std::path::Path, err: &crate::base::config::ConfigError) {
        match self {
            Self::Affinity => tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not write the session-affinity pin file; pins will not survive this restart"
            ),
            Self::BoundTokens => tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not write the bound-token file; conversations bound this boot may be re-keyed after a restart"
            ),
            Self::WireSessions => tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not write the Sessions/Tools cache; the panel will start empty next restart"
            ),
        }
    }
}

/// Spawn the loop that keeps `cache` saved on `path` until the shutdown signal. The final write
/// is not here: [`ServerHandle::shutdown_within`] makes it for all three caches after the join.
/// Cancelling the loop cannot tear a file, because every write is synchronous, so the loss is at
/// most one interval and that final write recovers it.
fn spawn_flusher(
    task: &'static str,
    cache: SavedCache,
    manager: &Arc<Manager>,
    path: PathBuf,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let manager = Arc::clone(manager);
    tokio::spawn(supervise(task, async move {
        let flush = save_on_change(cache, &manager, &path);
        tokio::select! {
            () = flush => {}
            _ = stop.changed() => {}
        }
    }))
}

/// Save `cache` to `path` for as long as the future runs: wait for a change, let
/// [`SavedCache::delay`] pass so a burst of changes is one write, then save if the content
/// differs from the file. A proxy with no traffic never wakes this.
///
/// It replaced a timer that woke every 5 s (60 s for the bound tokens) to look at a dirty flag,
/// which kept an idle proxy waking twelve times a minute per cache for nothing. A save its gate
/// defers ([`crate::stores::save_gate::SaveOutcome::Deferred`]) is retried at the instant the gate
/// names even with no further change, so a deferred field still reaches the disk within the
/// gate's refresh period.
///
/// Never returns; the caller races it against the shutdown signal. The write is synchronous,
/// so cancelling the future can never tear a file half-written.
async fn save_on_change(cache: SavedCache, manager: &Manager, path: &std::path::Path) {
    let mut due: Option<std::time::Instant> = None;
    loop {
        match due {
            Some(at) => {
                tokio::select! {
                    () = cache.changed(manager) => {}
                    () = tokio::time::sleep_until(tokio::time::Instant::from_std(at)) => {}
                }
            }
            None => cache.changed(manager).await,
        }
        tokio::time::sleep(cache.delay()).await;
        due = match cache.save(manager, path) {
            Ok(crate::stores::save_gate::SaveOutcome::Deferred(at)) => Some(at),
            Ok(_) => None,
            Err(err) => {
                cache.warn(path, &err);
                None
            }
        };
    }
}

/// Supervise a background loop: log (via `tracing::error!`) if it ends by
/// panicking, and otherwise do nothing else — it is NOT restarted. This repo's
/// three background loops (affinity flusher, quota prober, keep-warm) run
/// forever until the shutdown signal, and until now nothing observed them
/// while serving: a panic inside one silently stopped it forever with no
/// signal anywhere, and whatever it was keeping current (quota data, pin
/// flushes, keep-warm sweeps) then froze at its last value while `tcr status`
/// kept rendering it as live.
///
/// Catching the panic HERE, inside the task body, rather than adding a second
/// watcher task over the `JoinHandle` outside it, is deliberate: a watcher that
/// merely awaited the real task's `JoinHandle` would need to be the thing
/// stored in `background` for `ServerHandle::shutdown_within`/`Drop` to
/// join/abort — and aborting the WATCHER does not abort the task it was
/// watching, silently detaching it. Catching the panic in-place instead means
/// the spawned task's own `JoinHandle` always completes normally, so every
/// existing join/abort site in this file needs no change at all, and
/// `tasks_joined`/`tasks_aborted` accounting stays exactly as before.
///
/// `task` identifies which loop died, so the log line is actionable without
/// re-deriving it from a bare panic backtrace.
async fn supervise(task: &'static str, fut: impl Future<Output = ()>) {
    if let Err(payload) = std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        let reason = panic_message(&*payload);
        tracing::error!(
            task,
            reason,
            "background task panicked and stopped; it will NOT be restarted \
             — whatever it was keeping current is now frozen at its last value"
        );
    }
}

/// How long a successor waits for its predecessor's handoff channel to close
/// before recounting usage without it. The predecessor's handoff and drain are
/// bounded by the drain ceiling, and its shutdown after them by one grace for
/// the joins and at most one more for the ledger's drain.
fn predecessor_wait() -> Duration {
    crate::stores::restart::drain_ceiling() + DEFAULT_SHUTDOWN_GRACE * 2
}

/// The successor's half of a handoff's usage accounting. The boot replay ran
/// while the predecessor was still draining and writing to the same day file;
/// once its channel closes the file holds every line it will write, and the
/// day is recounted from the files (see `UsageTracker::rebuild_from_ledger`).
async fn recount_usage_after_predecessor(
    predecessor: std::os::unix::net::UnixStream,
    manager: Arc<Manager>,
    mut stop: watch::Receiver<bool>,
) {
    let wait = predecessor_wait();
    let closed = tokio::select! {
        closed = tokio::time::timeout(wait, predecessor_closed(predecessor)) => closed,
        _ = stop.changed() => return,
    };
    match closed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            %error,
            "could not read the handoff channel; recounting usage from the ledger now"
        ),
        Err(_) => tracing::warn!(
            wait_secs = wait.as_secs(),
            "the predecessor still holds the handoff channel; recounting usage without its last lines"
        ),
    }
    match tokio::task::spawn_blocking(move || manager.rebuild_usage_from_ledger()).await {
        Ok(Ok(report)) => tracing::info!(
            replayed = report.replayed,
            unresolved = report.unresolved,
            malformed = report.malformed,
            "the predecessor finished writing; usage recounted from the ledger"
        ),
        Ok(Err(skipped)) => tracing::warn!(
            ?skipped,
            "usage was not recounted after the handoff; the predecessor's last requests are \
             missing from today's totals until the next restart"
        ),
        Err(error) => tracing::warn!(%error, "the usage recount after the handoff did not run"),
    }
}

/// Resolves when the predecessor closes the handoff channel. Nothing is sent on
/// it after the release confirmation, so this reads to the end.
async fn predecessor_closed(channel: std::os::unix::net::UnixStream) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt as _;
    channel.set_nonblocking(true)?;
    let mut channel = tokio::net::UnixStream::from_std(channel)?;
    let mut buf = [0u8; 64];
    while channel.read(&mut buf).await? > 0 {}
    Ok(())
}

/// Give the server a request key ([`crate::request_key`]) when its config has none:
/// generate one and write it to the config file through the targeted writer
/// ([`Config::persist_request_key`]), then hold whatever the file now says.
///
/// Only with a file to write. A key the server held and the file did not would
/// be one `tcr request-key` cannot print, so a launcher could never present it;
/// the same reasoning keeps the server keyless when the write fails, with a
/// warning. A library caller with no `persist_path` writes nothing, as
/// [`ServeOptions::new`] promises, and serves as before.
///
/// One boot line says a key was written or adopted. It never carries the key.
pub(crate) fn ensure_request_key(config: &mut Config, persist_path: Option<&std::path::Path>) {
    if config.usable_request_key().is_some() {
        return;
    }
    let Some(path) = persist_path else {
        return;
    };
    let candidate = match crate::request_key::generate() {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(error = %err, "request key: the platform CSPRNG refused, so this server holds no key");
            return;
        }
    };
    match config.persist_request_key(path, &candidate) {
        Ok(crate::base::config::RequestKeyWrite::Written(_)) => {
            tracing::info!(
                path = %path.display(),
                "request key: generated one and saved it to the config (`tcr request-key` prints it)"
            );
        }
        Ok(crate::base::config::RequestKeyWrite::AlreadyPresent(_)) => {
            tracing::info!(
                path = %path.display(),
                "request key: the config gained one since it was loaded; using that"
            );
        }
        Err(err) => tracing::warn!(
            path = %path.display(),
            error = %err,
            "request key: could not save a new one to the config, so this server holds no key"
        ),
    }
}

/// The most tokio worker threads the binary starts by default. tokio's own default is one per
/// core, 14 on the machine this was measured on, for a proxy that serves a few requests a
/// second. The load test (`docs/performance.md`, "Threads, wake-ups and disk") found 2 workers
/// cost nothing measurable in CPU or added time at the fleet's steady and peak rates, and 4
/// served the saturating phases as well as 14. Four rather than two leaves room for the cache
/// saves, which write synchronously on a worker.
pub const DEFAULT_WORKER_THREADS: usize = 4;

/// How many tokio worker threads the binary runs: `TOKIO_WORKER_THREADS` when it holds a
/// positive number (tokio's own name for the setting, so an operator who knows tokio finds it),
/// else [`DEFAULT_WORKER_THREADS`] or the machine's cores, whichever is fewer. `env` is the
/// variable's value and `cores` the machine's parallelism, passed in so the rule is testable.
///
/// An unusable value is reported on stderr, where it is seen before the log exists, and the
/// default is used rather than refusing to start.
pub fn worker_threads(env: Option<&str>, cores: usize) -> usize {
    let cores = cores.max(1);
    if let Some(raw) = env {
        match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => return n,
            _ => eprintln!(
                "tcr: TOKIO_WORKER_THREADS={raw:?} is not a positive number; using {} worker threads",
                DEFAULT_WORKER_THREADS.min(cores)
            ),
        }
    }
    DEFAULT_WORKER_THREADS.min(cores)
}

pub async fn serve(options: ServeOptions) -> anyhow::Result<ServeOutcome> {
    let ServeOptions {
        mut config,
        persist_path,
        port: port_override,
        incumbent,
        affinity_path,
        wire_sessions_path,
        usage_dir,
        tls,
        host,
        owner_dir,
        handover,
    } = options;

    let port = config.apply_port_override(port_override);

    // `config::load` may have just renamed accounts to restore unique names. The
    // pin file is keyed by name, so carry it across before anything reads it —
    // otherwise every pin on a renamed account resolves to nothing and those
    // sessions cold-start their prompt cache, which is the most expensive event
    // in this system.
    let renames: Vec<(String, String)> = config.rename_pairs();
    if let Some(path) = &affinity_path {
        let rewritten = affinity::rename_pins(path, &renames);
        if rewritten > 0 {
            tracing::info!(
                path = %path.display(),
                rewritten,
                "carried session-affinity pins across the account rename"
            );
        }
    }

    // Resolve the port to ONE proxy BEFORE the Manager starts probing/refreshing,
    // so our own startup can never token-war with the incumbent. Only a
    // command-verified `tcr` server on THIS port can be signalled only under
    // the caller's explicit policy. The CLI uses listener handoff.
    //
    // Which of those a caller gets is [`IncumbentPolicy`], and the default sends
    // no signal at all: it uses `live_proxy_server`, the detection-only half of
    // the same port-scoped, command-verified decision, and stands down for
    // anything it finds. Port 0 short-circuits the whole question — the kernel
    // picks an ephemeral port, so no process can be "holding" it and there is
    // nothing an ephemeral-port caller could possibly want signalled.
    let takeover = match (port, incumbent.0) {
        // A handed-over socket settles the port question before it is asked:
        // the predecessor is not an incumbent to displace, it is the peer that
        // just gave us its listener. Signalling it here would kill the process
        // that is still draining the connections it accepted.
        _ if handover.is_some() => singleton::Takeover::Proceed,
        (0, _) => singleton::Takeover::Proceed,
        (_, Signal::Never) => match singleton::live_proxy_server(port) {
            Some(incumbent) => singleton::Takeover::IncumbentPresent(incumbent),
            None => singleton::Takeover::Proceed,
        },
        (_, Signal::Report) => singleton::takeover_port(port, false),
        (_, Signal::Recognized) => singleton::takeover_port(port, true),
    };
    if let singleton::Takeover::IncumbentPresent(incumbent) = takeover {
        // ONE probe of the incumbent, answering two questions: which build it is
        // executing, and whether it is executing anything at all.
        let probe = live_client::probe_incumbent(&config).await;
        // Read the checkout LIVE. The build stamps alone cannot see an edit made
        // since the last commit (build.rs re-runs only when a git ref moves), so
        // comparing two stamps would print "build in sync" for a proxy that
        // predates the edit — see `build_info::stand_down_build_report`.
        let checkout = std::env::current_dir()
            .ok()
            .and_then(|cwd| build_info::find_tcr_checkout(&cwd))
            .map(|root| build_info::read_checkout_state(&root, build_info::SHA));
        // Standing down is cheap and correct, but silent success here would mean
        // `cargo build && tcr` exits 0 with the OLD build still serving — the
        // caller is handed which build actually holds the port so it can say so.
        let report = build_info::stand_down_build_report(
            port,
            &build_info::BuildInfo::current(),
            probe.build.as_ref(),
            checkout.as_ref(),
        );
        return Ok(ServeOutcome::StoodDown(StandDown {
            port,
            pid: incumbent.pid,
            kind: incumbent.kind,
            probe,
            report,
        }));
    }

    // The peer-lease fallback, installed here, after the port question is
    // settled, before the manager probes anything and long before the listener
    // binds, because the seam it fills is on the answer path of a request and
    // must be decided before the first one can arrive.
    // `crate::peer::serve::fallback::PROVIDER` is a `OnceLock` for that reason: it is
    // consulted only when the whole local fleet came up dry, and once it holds
    // a provider nothing re-decides "is there a provider?" per request.
    //
    // This read is the first one and not the only one. A `NothingToBorrowFrom`
    // here arms the peers file to be read again on a later dry fleet
    // (`fallback::install_late_if_the_file_now_allows_it`), so a node paired
    // and granted `disclose` after this line runs borrows without a restart,
    // which is what `tcr peer allow` promises. That arm is why the outcome
    // below is worth logging even when it is "nothing".
    //
    // A stand-down returns above this line, so a process that did not bind
    // installs nothing, the install is process-wide and a second `tcr` that
    // stood down must not leave one behind.
    // Kept before `persist_path` is handed to the manager below, because the
    // peer LISTENER is booted after the proxy's own bind and needs the same
    // resolution. One expression, read twice, rather than two spellings of
    // "the peers file beside the config".
    let persist_path_for_peers = persist_path.clone();
    if let Some(peers_path) = peers_file_beside_config(persist_path.as_deref()) {
        // The egress seam reads the peers file per request and cannot be asked
        // what path it was built from (`crate::peer::tunnel::egress::peers_path`), so
        // the boot path tells it once. Without this line an explicit
        // `--config` elsewhere carried requests against the default peers
        // file rather than against the file this server was actually started
        // with.
        if !crate::peer::tunnel::egress::install_peers_path(peers_path.clone()) {
            tracing::debug!(
                path = %peers_path.display(),
                "peer egress: a peers file was already installed for this process; keeping it"
            );
        }
        match crate::peer::serve::fallback::install_peer_lease_provider(&peers_path) {
            Ok(installed) => tracing::info!(
                path = %peers_path.display(),
                outcome = ?installed,
                "peer-lease fallback: the dry-fleet arm now has somewhere to go"
            ),
            // Never fatal, and never silent either: an unreadable peers file
            // means the operator's trust relationships cannot be read, which is
            // worth a warning and is not worth refusing to serve local traffic
            // over.
            Err(err) => tracing::warn!(
                path = %peers_path.display(),
                error = %err,
                "peer-lease fallback: the peers file could not be read, so nothing is borrowed \
                 (local traffic is unaffected)"
            ),
        }
    }

    // The request key, settled before the manager snapshots the config. After the
    // stand-down return above, so a process that did not bind writes nothing.
    ensure_request_key(&mut config, persist_path.as_deref());

    let manager = Manager::with_live_refresher(config, persist_path);

    // Usage accounting is restored before the listener binds, for the same
    // reason the affinity pins are: the first request after a bounce must not
    // see a day that has reset to zero. Only when a directory was given — see
    // `ServeOptions::usage_dir` on why that is not the default.
    //
    // Never fatal. A ledger that cannot be read or written costs the restart's
    // history and nothing else, so `attach_usage_ledger` reports rather than
    // errors and the proxy serves either way.
    if let Some(usage_dir) = &usage_dir {
        crate::records::usage::warn_if_local_offset_unavailable();
        let retention = manager.usage_retention_days();
        let report = manager.attach_usage_ledger(usage_dir.clone(), retention);
        tracing::info!(
            path = %usage_dir.display(),
            replayed = report.replayed,
            unresolved = report.unresolved,
            malformed = report.malformed,
            pruned = report.pruned,
            retention_days = retention,
            // The two the writer thread owns: whether it is actually writing
            // (false if it could not start, or has already failed), and how
            // many lines its queue had no room for. Both are the answer to
            // "will today's totals survive the next restart", which the count
            // of replayed lines alone cannot give.
            persisting = manager.usage_is_persisting(),
            dropped_lines = manager.usage_dropped_lines(),
            "usage ledger attached"
        );
    }

    // One trigger for every task this function spawns. `watch` rather than a
    // one-shot so each loop can hold its own receiver and shutdown is
    // idempotent (both `shutdown()` and `Drop` may fire it).
    let (shutdown_tx, _) = watch::channel(false);
    let mut background: Vec<JoinHandle<()>> = Vec::new();

    // Session-affinity pins survive a restart via their own cache file (NOT the
    // credential config — see `tcr::stores::affinity`). Restore before the
    // listener binds, so the first request after a bounce already routes on its
    // old pin instead of cold-starting the account's prompt cache.
    //
    // Only when affinity is enabled: with the feature off the map is never
    // consulted, and a restore would put entries in it that nothing reads.
    //
    // And only when a pin cache path was given: with `affinity_path: None` the
    // pins are an in-memory routing table for this process alone, so there is
    // nothing to restore from and nothing to spawn a flusher for.
    if let (true, Some(affinity_path)) = (manager.session_affinity_enabled(), &affinity_path) {
        let report = manager.restore_affinity(affinity_path, affinity::PIN_TTL_MS);
        if let Some(reason) = &report.degraded {
            // Never fatal: the pin file is a cache, so an unusable one costs the
            // warm start it would have bought and nothing else.
            tracing::warn!(
                path = %affinity_path.display(),
                reason = %reason,
                "session-affinity pins ignored; starting with an empty pin map"
            );
        } else {
            tracing::info!(
                path = %affinity_path.display(),
                restored = report.pins.len(),
                expired = report.expired,
                unresolved = report.unresolved,
                ambiguous = report.ambiguous,
                ttl_minutes = affinity::PIN_TTL_MS / 60_000,
                "session-affinity pins restored"
            );
        }

        // Debounced incremental flush. Shutdown-only would miss the case this
        // exists to survive: `--replace` follows SIGTERM with SIGKILL, and a
        // SIGKILL runs no shutdown path at all. The flusher sleeps until the map
        // changes, then writes `AFFINITY_SAVE_DELAY` later, so the loss is bounded
        // by that delay while an idle proxy wakes for nothing. The write is gated
        // on content (`flush_affinity_if_changed`): a new or moved pin is written
        // at once, a touch time within `AFFINITY_TOUCH_REFRESH`.
        background.push(spawn_flusher(
            "affinity-flusher",
            SavedCache::Affinity,
            &manager,
            affinity_path.clone(),
            shutdown_tx.subscribe(),
        ));
    }

    // The account-bound-token map (see `tcr::stores::bound_tokens`) survives a restart the
    // same way pins do, and for a sharper reason: a restart is exactly when live conversations
    // get re-keyed onto other accounts, and a conversation carrying an advisor result, a
    // thinking signature or server-side thread state is rejected outright on every remaining
    // turn once that happens. Restored before the listener binds so the first request after a
    // bounce is already pinned.
    //
    // Keyed on the affinity path — a sibling file in the same directory — rather than on its
    // own `ServeOptions` field: the two caches must never land in different places, and an
    // embedder that points `affinity_path` at a disposable directory gets this one redirected
    // with it instead of writing into the live proxy's cache dir. `affinity_path: None`
    // therefore means in-memory-only here too.
    //
    // NOT gated on `session_affinity_enabled`: this map is not session affinity. It is a
    // correctness constraint on which account CAN answer a request, and it applies whether or
    // not sessions are pinned for cache warmth.
    if let Some(affinity_path) = &affinity_path {
        let bound_token_path = crate::stores::bound_tokens::path_beside(affinity_path);
        let report = manager.restore_bound_tokens(
            &bound_token_path,
            crate::stores::bound_tokens::BOUND_TOKEN_TTL_MS,
        );
        if let Some(reason) = &report.degraded {
            tracing::warn!(
                path = %bound_token_path.display(),
                reason = %reason,
                "bound-token pins ignored; conversations bound before this boot may be re-keyed"
            );
        } else {
            tracing::info!(
                path = %bound_token_path.display(),
                restored = report.tokens.len(),
                expired = report.expired,
                unresolved = report.unresolved,
                ambiguous = report.ambiguous,
                "bound-token pins restored"
            );
        }

        // Saved like the affinity pins above, but as a checkpoint every
        // `BOUND_TOKENS_SAVE_DELAY` (ten minutes) rather than five seconds after a change. This
        // map gains an entry on nearly every assistant turn, and at `BOUND_TOKEN_CAP` the file is
        // megabytes, so any short cadence rewrites all of it, again and again, for as long as
        // anyone works. The cost of the wide window is what a crash or SIGKILL inside it loses:
        // up to that long of newly minted tokens, whose conversations then route as they did
        // before this feature (the constant's doc says so too).
        // A CLEAN shutdown loses none of them: on the shutdown signal the map is written one
        // last time if it changed. Before that, every restart (a TcrBar update is one) dropped
        // the last minute of mints, which are the live conversations' newest tokens; the first
        // requests after the boot then found bound history with no minted token known.
        //
        // The final write is `ServerHandle::shutdown_within`'s, after the joins, the same as for the
        // other two caches; it is not made by this task.
        background.push(spawn_flusher(
            "bound-token-flusher",
            SavedCache::BoundTokens,
            &manager,
            bound_token_path,
            shutdown_tx.subscribe(),
        ));
    }

    // The Sessions/Tools panel cache (F1, `docs/design/panel-tabs.md`) survives a
    // restart the same way pins do — see `session_wire_persist` — restored before
    // the listener binds so the panel is populated from the first `tcr status`
    // after a bounce instead of starting empty.
    //
    // Only when a cache path was given: with `wire_sessions_path: None` the table
    // is an in-memory record for this process alone, so there is nothing to
    // restore from and nothing to spawn a flusher for.
    if let Some(wire_sessions_path) = &wire_sessions_path {
        let report =
            manager.restore_wire_sessions(wire_sessions_path, session_wire_persist::RESTORE_TTL_MS);
        if let Some(reason) = &report.degraded {
            // Never fatal: this is a cache, so an unusable one costs the panel's
            // pre-restart history and nothing else.
            tracing::warn!(
                path = %wire_sessions_path.display(),
                reason = %reason,
                "Sessions/Tools cache ignored; starting with an empty table"
            );
        } else {
            tracing::info!(
                path = %wire_sessions_path.display(),
                restored = report.sessions.len(),
                expired = report.expired,
                ttl_hours = session_wire_persist::RESTORE_TTL_MS / 3_600_000,
                "Sessions/Tools cache restored"
            );
        }

        // Saved on change, the affinity flusher's contract just above, for the same
        // reason: a SIGKILL runs no shutdown path at all, so only a periodic write can
        // survive one. A new or departed session is written `WIRE_SESSIONS_SAVE_DELAY`
        // after it appears; the counters of sessions already on disk within
        // `WIRE_SESSIONS_REFRESH`.
        background.push(spawn_flusher(
            "wire-sessions-flusher",
            SavedCache::WireSessions,
            &manager,
            wire_sessions_path.clone(),
            shutdown_tx.subscribe(),
        ));
    }

    // Background probe: refresh every account's quota around the configured
    // cadence (a value <= 0 in `quotaProbeSeconds` disables it).
    //
    // Boot does ONE immediate whole-fleet sweep, then hands over to `schedule`,
    // which runs each account on its own randomly drawn schedule (`cadence +/-
    // 30%`, random initial offset — see `crate::manager::schedule`). The boot sweep is
    // kept deliberately: `interval`'s first tick used to fire immediately so the
    // bars populate at startup rather than after a lag, and a random first offset
    // would otherwise leave a fresh proxy showing blank bars for up to a whole
    // cadence. It is a single sweep at a known-quiet moment, not a repeating
    // synchronization — every subsequent probe is per-account and random.
    let probe_seconds = manager.probe_interval_seconds();
    if probe_seconds > 0 {
        let prober = Arc::clone(&manager);
        let mut stop = shutdown_tx.subscribe();
        background.push(tokio::spawn(supervise("quota-prober", async move {
            let probe = async {
                prober.probe_all().await;
                crate::manager::schedule::run(
                    Arc::clone(&prober),
                    crate::manager::schedule::Job::Probe,
                    probe_seconds,
                )
                .await;
            };
            tokio::select! {
                () = probe => {}
                _ = stop.changed() => {}
            }
        })));
    }

    // Spend a banked reset two hours before it would expire unused
    // (`manager::resets::auto`). On unless `"autoUseExpiringResets": false`; the tick reads
    // memory only, so it costs nothing until a reset is due.
    if manager.auto_reset_enabled() {
        let m = Arc::clone(&manager);
        let mut stop = shutdown_tx.subscribe();
        background.push(tokio::spawn(supervise("reset-expiry", async move {
            tokio::select! {
                () = crate::manager::resets::auto::run(m) => {}
                _ = stop.changed() => {}
            }
        })));
    }

    // Opt-in keep-warm loop: periodically warm idle accounts so their 5h session
    // window stays live. Ships DARK — `warmupSeconds` defaults to 0, and when it is
    // absent/0 NO task is spawned here at all (unlike the probe, warming spends real
    // quota).
    //
    // Same treatment as the probe and for the same reason — it had the identical
    // synchronized shape — with one deliberate difference: there is NO boot sweep
    // here, because a warm spends real quota and `warm_targets`' boot gate exists
    // precisely to stop a restart from firing one at every account. Each account
    // gets a random initial offset and a random interval thereafter
    // (`crate::manager::schedule`), and the edge-triggered `warm_wake` still starts a
    // one-shot sweep: it is what keeps the boot gate from being a kill switch on
    // a proxy restarted more often than `warmupSeconds`. That wake is handled
    // inside `schedule::run`, where a permit stored by `notify_one` while a warm
    // is in flight is consumed by the next `notified()` rather than lost.
    let warmup_seconds = manager.warmup_interval_seconds();
    if warmup_seconds > 0 {
        let m = Arc::clone(&manager);
        let mut stop = shutdown_tx.subscribe();
        background.push(tokio::spawn(supervise("keep-warm", async move {
            let warm = crate::manager::schedule::run(
                m,
                crate::manager::schedule::Job::Warm,
                warmup_seconds,
            );
            tokio::select! {
                () = warm => {}
                _ = stop.changed() => {}
            }
        })));
    }

    // Load the MITM TLS material (reuse the existing leaf, else mint one). A
    // failure here is non-fatal: base-URL mode still serves; only CONNECT
    // (forward-proxy) mode is unavailable until the cert issue is fixed.
    //
    // What the listener's policy and loaded leaf let it intercept is recorded
    // HERE, from the material it actually got, because nothing reloads it after
    // this point: the status endpoint reports it (`StatusPayload::mitm`), and
    // `tcr run --codex` pools a session only on that answer and the account
    // rows (`chatgpt.com` is terminated only while the pool holds a Codex
    // account), never on the leaf a client finds on disk.
    //
    // Whether the pool holds a Codex account only words the CA notices: once one does, they
    // name `CODEX_CA_CERTIFICATE` too, the variable codex trusts a CA from.
    let holds_codex = manager.provider_account_count(crate::base::provider::Provider::Codex) > 0;
    let tls = match tls {
        TlsSetup::Load => match tls::load_tls(holds_codex) {
            Ok(assets) => {
                tracing::info!(
                    ca = %assets.ca_path.display(),
                    "{}",
                    tls::advertise_note(holds_codex)
                );
                manager.record_mitm_status(crate::status::MitmStatus::of(Some(&assets)));
                Some(Arc::new(assets.acceptor))
            }
            Err(err) => {
                tracing::warn!(error = %err, "MITM disabled: could not load/generate TLS material (base-URL mode still works)");
                manager.record_mitm_status(crate::status::MitmStatus::of(None));
                None
            }
        },
        TlsSetup::Disabled => {
            manager.record_mitm_status(crate::status::MitmStatus::of(None));
            None
        }
    };

    // Hybrid proxy server task: base-URL mode and HTTPS_PROXY/CONNECT mode on the
    // same port. The listener peeks each connection and routes accordingly.
    let (inherited_listener, predecessor) = match handover {
        Some(handover) => (Some(handover.listener), Some(handover.predecessor)),
        None => (None, None),
    };
    // With no ledger there is nothing to recount, and dropping the channel
    // costs the predecessor nothing: it never reads it.
    if let (Some(predecessor), Some(_)) = (predecessor, &usage_dir) {
        background.push(tokio::spawn(recount_usage_after_predecessor(
            predecessor,
            Arc::clone(&manager),
            shutdown_tx.subscribe(),
        )));
    }
    let listener = match inherited_listener {
        // Adopted, not bound. `set_nonblocking` is required before tokio may
        // own it: the descriptor arrives with whatever mode the predecessor had
        // it in, and a blocking listener wedges the accept loop's runtime
        // thread on the first connectionless poll.
        Some(std_listener) => {
            std_listener
                .set_nonblocking(true)
                .with_context(|| "failed to set the inherited listener non-blocking".to_string())?;
            tokio::net::TcpListener::from_std(std_listener)
                .context("failed to adopt the inherited listener")?
        }
        None => tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("failed to bind 127.0.0.1:{port}"))?,
    };
    let bound = listener.local_addr()?;
    let std_listener = listener.into_std()?;
    let handoff_listener = std_listener.try_clone()?;
    let listener = tokio::net::TcpListener::from_std(std_listener)?;

    // The boot marker. The durable log at `~/.cache/tcr/logs/` rotates
    // daily (5 files kept) rather than growing forever, but within one day's
    // file a restart is still invisible without this line: request lines run
    // unbroken across a bounce and the log cannot be sliced "since this boot".
    // Emitted here deliberately — after the subscriber is installed (else it goes
    // nowhere) and after the bind SUCCEEDED — so one line means "this pid is live
    // on this port", not "this pid tried". A restart also wipes the in-memory
    // session→account pin map, the most expensive cache event in this system;
    // counting these lines is how that cost becomes measurable:
    //   rg 'server started' ~/.cache/tcr/logs/*
    //
    // `version` alone could not tell two boots apart: it is `CARGO_PKG_VERSION`,
    // the literal 0.1.0 from Cargo.toml, identical across every build ever made.
    // The build stamp beside it is the field that actually identifies the code
    // this pid is executing — the thing that used to need an `lsof -p <pid>`
    // inode comparison to establish. See `build_info`.
    // `http1_only` rides on the SAME boot line rather than a separate one,
    // deliberately: this repo lost seven hours of prompt-cache once to a
    // default-off knob whose state was invisible from outside the process,
    // and the fix is making the state show up at the one place every boot is
    // already guaranteed to log — not adding a second line an operator has to
    // know to look for. See `Config::http1_only`. `divert_budget` rides the same
    // line for the same reason: `0` (unlimited, today's behaviour) and a nonzero
    // budget are both silent from outside the process otherwise. See
    // `Manager::divert_budget` and the divert-budget design notes §4.7.
    let http1_only = manager.http1_only();
    // `throttle_exempt_noise` rides on this same line for the same reason as
    // `http1_only` above: it is a default-OFF knob (see
    // `Manager::throttle_exempt_noise_enabled`) and this is the one place
    // every boot is already guaranteed to log.
    let throttle_exempt_noise = manager.throttle_exempt_noise_enabled();
    let divert_budget = manager.divert_budget();
    // Every other load-bearing knob rides this same line for the same reason:
    // `sessionAffinity`, `revalidationServe` and `loadBalanceMigration` are
    // each default-ON opt-OUT switches (a config that silently disables one
    // reads identically to the feature working, from outside the process);
    // `quotaProbeSeconds`/`warmupSeconds` decide whether the two timer loops
    // spawned above exist at all; `pacing`/`throttle` are each all-`None`
    // (inert) unless the operator opted in, and a `PacingConfig`/
    // `ThrottleConfig` typo that resolves to "still inert" is otherwise
    // invisible; `lockAccount`/`controlAccount` resolve a NAME to an account
    // index at construction and log an `error!`/nothing respectively on a
    // typo, but neither of those lines carries the RESOLVED identity next to
    // the rest of the boot state.
    let session_affinity = manager.session_affinity_enabled();
    let revalidation_serve = manager.revalidation_serve_enabled();
    let load_balance_migration = manager.load_balance_migration_enabled();
    // Reuse the bindings the probe/warm spawn gates above already computed
    // rather than reading the manager twice for the same values.
    let quota_probe_seconds = probe_seconds;
    let pacing_active = manager.pacing_active();
    let throttle_active = manager.throttle_active();
    let lock_account = manager.locked_account_name();
    let control_account = manager.control_name();
    let control_pooled = manager.control_pooled();
    let control_identity = manager.control_identity().as_str();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        sha = build_info::SHA,
        dirty = build_info::DIRTY,
        built_at = build_info::BUILT_AT,
        pid = std::process::id(),
        port = bound.port(),
        http1_only,
        throttle_exempt_noise,
        divert_budget,
        session_affinity,
        revalidation_serve,
        load_balance_migration,
        quota_probe_seconds,
        warmup_seconds,
        pacing_active,
        throttle_active,
        lock_account,
        control_account,
        control_pooled,
        control_identity,
        "server started"
    );

    // Claim the port by NAME-FREE identity, in the same place and for the same
    // reason as the boot marker above: after the bind SUCCEEDED, so the file means
    // "this pid is serving this port" rather than "this pid tried". A `tcr` in
    // another terminal, and `tcr login`, then recognize this proxy whatever program
    // is hosting it — see [`crate::stores::singleton`] for the silent token loss that
    // depends on it.
    //
    // A write failure is NOT fatal. The claim is an optimisation over the
    // command-line matcher for the CLI host, and refusing to serve because a cache
    // directory is unwritable would be a worse outcome than the matcher we had
    // before. It is loud, because for an EMBEDDED host the matcher recognizes
    // nothing and this file is the only identity there is.
    //
    // A claim that could not be written is dropped from the handle: shutdown then
    // has nothing to remove, rather than deleting a path this process never owned.
    //
    // The file NAME is derived here, from the port actually bound, and not taken
    // from the caller: every reader looks a claim up as `proxy-owner-<port>.json`
    // for the port it is resolving, so a name that does not match is a claim
    // nothing consults. With `port: Some(0)` a caller cannot know the name in
    // advance — the kernel picks the port during this function — which is why the
    // caller supplies a directory and `serve` supplies the name.
    let owner_path = owner_dir.and_then(|dir| {
        let path = singleton::owner_path_in(&dir, bound.port());
        let owner = singleton::ProxyOwner {
            pid: std::process::id(),
            port: bound.port(),
            sha: build_info::SHA.to_string(),
            host,
        };
        match singleton::write_owner_file(&path, &owner) {
            Ok(()) => {
                tracing::info!(
                    path = %path.display(),
                    pid = owner.pid,
                    port = owner.port,
                    host = ?host,
                    "proxy owner file written; this proxy is identifiable without its process name"
                );
                Some(path)
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    host = ?host,
                    "could not write the proxy owner file; identity falls back to command-line matching, which does NOT recognize an embedded proxy"
                );
                None
            }
        }
    });

    // The LAN peer mesh's second socket, opened here and nowhere else.
    //
    // AFTER the proxy's own bind, because the lender's half of a SERVE sends
    // the relayed request through THIS process's own proxy, that is the whole
    // of "own picker, own Bearer, own bucket", so the base it is given has to
    // be the port this process actually bound, which for `port: 0` is a number
    // only the kernel knew a moment ago.
    //
    // Before the accept loop is spawned below, so a peer that connects the
    // instant the port opens finds a process whose own proxy is already bound.
    let peer_addr = match peers_file_beside_config(persist_path_for_peers.as_deref()) {
        Some(peers_path) => {
            boot_peer_listener(
                &peers_path,
                &manager,
                &format!("http://127.0.0.1:{}", bound.port()),
                &shutdown_tx,
                &mut background,
            )
            .await
        }
        None => None,
    };

    let (connections, drain) = tokio::sync::mpsc::channel(1);
    let serve_manager = Arc::clone(&manager);
    let mut stop = shutdown_tx.subscribe();
    let server = tokio::spawn(async move {
        mitm::serve_tracked_with_shutdown(
            listener,
            serve_manager,
            tls,
            async move {
                crate::base::log_err::LogErr::log_debug(
                    stop.changed().await,
                    "waiting for shutdown",
                );
            },
            connections,
        )
        .await;
    });

    Ok(ServeOutcome::Started(ServerHandle {
        listener: Some(handoff_listener),
        connections: Some(drain),
        addr: bound,
        shutdown: shutdown_tx,
        manager,
        affinity_path,
        wire_sessions_path,
        owner_path,
        tasks_joined: 0,
        tasks_aborted: 0,
        server: Some(server),
        server_finished: false,
        background,
        peer_addr,
        successor: None,
    }))
}

/// Unit tests for what only in-crate access can pin: [`Drop for ServerHandle`].
///
/// The integration test (`tests/proxy/serve_library_path.rs`) drops a real handle and
/// watches the port refuse — which proves the loop stopped, but NOT that `Drop`
/// stopped it: the handle owns the `watch::Sender`, so dropping it closes the
/// channel and every `stop.changed()` returns `Err` on its own. That test stayed
/// green with `impl Drop` deleted.
///
/// These build a `ServerHandle` by hand and hold a **`Sender` clone**, so the
/// channel survives the drop and sender-close is off the table as an
/// explanation. Each half of `Drop` then has exactly one thing that can satisfy
/// it: `send(true)` for the tasks that watch the channel, `abort()` for the ones
/// that do not.
#[cfg(test)]
mod tests;
