//! tcr's own tailnet node: one `tailscale::Device`, supervised, behind the
//! `tailnet-node` cargo feature.
//!
//! With the setting at `node` (`tcr peer tailnet node`), tcr joins a tailnet as
//! a machine of its own, through Tailscale's Rust library, so a Mac without the
//! Tailscale app can still be reached. Only tcr is on the tailnet, not the rest
//! of the Mac. Spec: `docs/design/tailnet.md` § B.
//!
//! # Source builds only
//!
//! The library supports Apple silicon and Linux, not Intel Macs, and brings a
//! C crypto library this repository otherwise avoids. Gil, 2026-10-20: source
//! builds only, behind the `tailnet-node` feature, which no release build
//! turns on. [`SUPPORTED`] is the one place that decides whether this binary
//! can act on the setting; everything here that does not touch the library
//! itself compiles and is tested in every build, so CI exercises the
//! supervisor, the key file and the join and leave orderings without the
//! feature.
//!
//! # Who does what
//!
//! - **Joining is the CLI's** ([`join_with`]). It reads the auth key from
//!   stdin, builds a device in its own process, waits until control says the
//!   node is authorized, writes the key file, shuts the device down, and only
//!   then sets the mode. The auth key is held in an [`AuthKey`], which has no
//!   `Display` and a redacting `Debug`, and is never written anywhere.
//! - **Serving is the server's** ([`Supervisor`]). It starts its device from
//!   the key file alone, listens on the node's address at the peer port, and
//!   hands every accepted connection to the same
//!   [`crate::peer::listener::serve_one`] the kernel listener uses. Admission
//!   needs no change: the source is a tailnet address and the bind is the
//!   node's, neither LAN nor loopback, so a stranger on the tailnet gets
//!   return visits and enrolments only (`internet_admission`).
//! - **Leaving is the CLI's** ([`leave_with`]): mode off, wait until the
//!   serving process has let go, then delete the key file.
//!
//! # Why a supervisor
//!
//! About a minute without network panics the library's relay and control
//! tasks, and nothing restarts them (upstream issue #439): the process lives
//! on with a dead tailnet, which is exactly what a laptop that sleeps does. So
//! the server checks the device every [`HEALTH_EVERY`] (a `self_node()`
//! lookup under [`CONTROL_TIMEOUT`]) and whenever the default gateway changes,
//! and a failed check shuts the device down within [`SHUTDOWN_DEADLINE`] and
//! builds a new one from the same keys.
//!
//! # Every wait on control is bounded
//!
//! Before registration, `ipv4_addr`, `self_node` and `is_authorized` wait
//! until control answers, which with no network is forever. Every such call
//! here runs under [`CONTROL_TIMEOUT`], and nothing asks for an address before
//! [`TailnetDevice::authorization`] has said yes.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::peer::config::TailnetMode;
use crate::peer::state::{TailnetRecord, TailnetState};

/// Whether this binary can act on `tailnet: "node"`: built with the
/// `tailnet-node` feature, for the one Mac target the library supports.
///
/// Read by the verb (`node` is refused without it), by `tcr peer tailnet
/// --json` (`nodeSupported`) and by the server's supervisor
/// (`nodeUnsupported`).
pub const SUPPORTED: bool = cfg!(all(
    feature = "tailnet-node",
    target_os = "macos",
    target_arch = "aarch64"
));

/// The key file's name, in the config directory beside `tcr-node.key`.
pub const KEY_FILE_NAME: &str = "tcr-tailnet.key";

/// How often the supervisor wakes: re-reads the mode and refreshes the record.
pub const TICK: Duration = Duration::from_secs(10);

/// How often a running device is asked whether it is still alive.
pub const HEALTH_EVERY: Duration = Duration::from_secs(30);

/// The ceiling on any one wait on control: `is_authorized`, `self_node`,
/// `ipv4_addr`. Each waits forever without network.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// The ceiling on building a device (`Device::new` waits for its actors to
/// start).
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a device gets to stop gracefully before it is killed.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// How long the first failed start waits before the next attempt. Each failure
/// in a row doubles it, up to [`RETRY_CAP`]; a device that passes a health
/// check puts it back here.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(30);

/// How long keys the tailnet refused wait before they are tried again: an
/// operator has to act (approve the machine, or join again), so asking control
/// every 30 s would be noise. The first rung for a refusal; it doubles to
/// [`RETRY_CAP`] like any other failure.
pub const RETRY_AFTER_REFUSAL: Duration = Duration::from_secs(300);

/// The longest a failed start waits before the next attempt: an offline
/// laptop asks control at most every ten minutes.
pub const RETRY_CAP: Duration = Duration::from_secs(600);

/// The wait after `failures` failures in a row, the first of them at `first`:
/// `first`, doubling, capped at [`RETRY_CAP`].
pub fn retry_wait(first: Duration, failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    first.saturating_mul(1_u32 << doublings).min(RETRY_CAP)
}

/// How long `tcr peer tailnet node --auth-key -` waits for control to accept
/// the key.
pub const JOIN_DEADLINE: Duration = Duration::from_secs(60);

/// How long `tcr peer tailnet leave` waits for the serving process to let go
/// of the node before it refuses to delete the key file.
pub const LEAVE_WAIT: Duration = Duration::from_secs(45);

/// How often `leave` re-reads the state file while it waits.
pub const LEAVE_POLL: Duration = Duration::from_millis(500);

/// A record whose content has not changed is still rewritten this often, so
/// its `recordedAtMs` stays inside the 60 s a reader calls fresh.
pub const RECORD_REFRESH: Duration = Duration::from_secs(20);

/// `<config_dir>/tcr-tailnet.key`.
pub fn key_path(config_dir: &Path) -> PathBuf {
    config_dir.join(KEY_FILE_NAME)
}

/// The key file that goes with a peers file: in the same directory, the way
/// the server finds `tcr-node.key`.
pub fn key_path_for_peers(peers_path: &Path) -> PathBuf {
    key_path(
        &peers_path
            .parent()
            .map_or_else(crate::peer::id::default_config_dir, Path::to_path_buf),
    )
}

// ---------------------------------------------------------------------------
// The auth key
// ---------------------------------------------------------------------------

/// A Tailscale auth key, read once from stdin.
///
/// No `Display`, a `Debug` that prints nothing of it, no `Serialize`: the only
/// way out is [`Self::into_secret`], called where the library takes it.
pub struct AuthKey(String);

impl AuthKey {
    /// Wrap a key read from stdin. Refuses an empty one.
    pub fn new(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("peer tailnet: no auth key on standard input");
        }
        Ok(Self(trimmed.to_string()))
    }

    /// The key itself, for the library and nothing else.
    pub fn into_secret(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for AuthKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthKey(..)")
    }
}

// ---------------------------------------------------------------------------
// The key file
// ---------------------------------------------------------------------------

/// What `tcr-tailnet.key` holds: the node's keys and the name it asked for.
///
/// `key_state` is the library's `PersistState` exactly as the library
/// serializes it, kept opaque here so a build without the library can still
/// check, move and delete the file. The same shape as the library's own key
/// file (`{"key_state": ...}`), so either can read the other's.
#[derive(Clone, Serialize, Deserialize)]
pub struct NodeKeyFile {
    /// The node, machine and network-lock keys. Secret.
    pub key_state: serde_json::Value,
    /// The name this node asks control for, `--name` at join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

impl std::fmt::Debug for NodeKeyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeKeyFile")
            .field("key_state", &"..")
            .field("hostname", &self.hostname)
            .finish()
    }
}

/// Read the key file at `path`, or `None` when there is none.
///
/// Refuses a file with a mode other than 0600, the rule `tcr-node.key` follows
/// (`crate::peer::id::PRIVATE_KEY_MODE`): a wider mode means something other
/// than tcr wrote it, and the file IS the node's identity on the tailnet.
pub fn read_key_file(path: &Path) -> Result<Option<NodeKeyFile>> {
    use std::os::unix::fs::PermissionsExt as _;

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    if !meta.file_type().is_file() {
        bail!(
            "{} is not a regular file: refusing to trust a tailnet key file this program did \
             not write",
            path.display()
        );
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != crate::peer::id::PRIVATE_KEY_MODE {
        bail!(
            "{} has mode {mode:o}, expected {:o}: refusing to trust a tailnet key file this \
             program did not write",
            path.display(),
            crate::peer::id::PRIVATE_KEY_MODE
        );
    }
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file: NodeKeyFile = serde_json::from_slice(&raw)
        .with_context(|| format!("{} is not a tailnet key file", path.display()))?;
    Ok(Some(file))
}

/// Write the key file at `path`, mode 0600, refusing one that already exists.
///
/// Through the writer `tcr-node.key` uses, never the library's (which leaves
/// the process umask, 0644: upstream #449), and never over an existing file,
/// so the keys are never re-minted behind the operator's back.
pub fn write_key_file(path: &Path, file: &NodeKeyFile) -> Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let bytes = serde_json::to_vec(file).context("serializing the tailnet key file")?;
    crate::peer::id::write_key_file(path, &bytes, crate::peer::id::PRIVATE_KEY_MODE)
}

/// Delete the key file at `path`. Reports whether there was one.
pub fn remove_key_file(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("deleting {}", path.display())),
    }
}

/// Refuse a `--name` control would not accept as a machine name: 1 to 63
/// letters, digits and hyphens, not starting or ending with a hyphen.
pub fn check_node_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        bail!(
            "peer tailnet: `{name}` is not a machine name; use 1 to 63 letters, digits and \
             hyphens, for example `tcr-studio`"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The device, behind a trait
// ---------------------------------------------------------------------------

/// What control says about this node's keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorization {
    /// Registered and accepted.
    Authorized,
    /// Control answered and does not accept these keys: the auth key was
    /// refused, the node was removed, or its key expired.
    NotAuthorized,
}

/// This node as control sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSelf {
    /// The node's tailnet IPv4.
    pub address: Ipv4Addr,
    /// The name on the tailnet (the first label of its DNS name on the tailnet).
    pub name: String,
}

/// One connection a [`TailnetListener`] accepted.
pub struct Accepted<S> {
    /// The stream.
    pub stream: S,
    /// The far end, a tailnet address.
    pub from: SocketAddr,
    /// The node's own address and port this arrived on.
    pub bind: SocketAddr,
}

/// A listening socket on the node.
pub trait TailnetListener: Send + Sync + 'static {
    /// The stream an accept yields.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Wait for the next connection. An error ends the accept loop; the
    /// supervisor's next tick notices and rebuilds.
    fn accept(&self) -> impl Future<Output = Result<Accepted<Self::Stream>>> + Send;
}

/// The node itself: the library's `Device` in a real build, a fake in tests.
pub trait TailnetDevice: Send + Sync + 'static {
    /// The stream type of this device's sockets.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    /// Its listener.
    type Listener: TailnetListener<Stream = Self::Stream>;

    /// Whether control accepts these keys. Waits for control; every caller
    /// bounds it.
    fn authorization(&self) -> impl Future<Output = Result<Authorization>> + Send;

    /// This node's address and name. Waits for control; every caller bounds
    /// it, and only asks after [`Self::authorization`] said yes.
    fn self_node(&self) -> impl Future<Output = Result<NodeSelf>> + Send;

    /// Listen on `addr`, which is the node's own address.
    fn listen(&self, addr: SocketAddr) -> impl Future<Output = Result<Self::Listener>> + Send;

    /// Stop, gracefully within `deadline`, then by force. Reports whether it
    /// stopped gracefully.
    fn shutdown(self, deadline: Duration) -> impl Future<Output = bool> + Send;
}

/// What builds a [`TailnetDevice`] from a key file.
pub trait DeviceFactory: Send + Sync + 'static {
    /// The device it builds.
    type Device: TailnetDevice;

    /// Whether this factory can build anything at all. `false` is a binary
    /// without the library: the supervisor then records `nodeUnsupported`.
    const SUPPORTED: bool;

    /// Fresh keys for a node that has none. Called by a join only, never by
    /// the server: keys are never re-minted automatically.
    fn mint_keys(&self) -> Result<serde_json::Value>;

    /// Build a device from `keys`, registering with `auth_key` when given.
    fn build(
        &self,
        keys: &NodeKeyFile,
        auth_key: Option<AuthKey>,
    ) -> impl Future<Output = Result<Self::Device>> + Send;
}

/// Run `fut` under `limit`, naming `what` when it runs out.
async fn bounded<T>(
    limit: Duration,
    what: &str,
    fut: impl Future<Output = Result<T>> + Send,
) -> Result<T> {
    tokio::time::timeout(limit, fut).await.map_err(|_| {
        anyhow!(
            "{what} did not answer within {}s (no network, or control is unreachable)",
            limit.as_secs()
        )
    })?
}

/// What came of shutting down a device the supervisor shares with in-flight
/// calls.
enum Shutdown<D> {
    /// It was shut down; `graceful` is false when it had to be killed.
    Stopped { graceful: bool },
    /// Another holder still had it at the deadline, so it was NOT shut down
    /// and is still running. Handed back so the caller keeps it, and builds
    /// nothing from the same keys until it has been shut down.
    StillHeld(Arc<D>),
}

/// Shut down a device the supervisor shares with in-flight calls: wait (up to
/// `deadline`) for the other holders to let go, then shut it down in what is
/// left of the deadline.
async fn shutdown_shared<D: TailnetDevice>(device: Arc<D>, deadline: Duration) -> Shutdown<D> {
    let until = tokio::time::Instant::now() + deadline;
    let mut device = device;
    loop {
        match Arc::try_unwrap(device) {
            Ok(owned) => {
                let left = until.saturating_duration_since(tokio::time::Instant::now());
                let graceful = owned.shutdown(left.max(Duration::from_millis(100))).await;
                return Shutdown::Stopped { graceful };
            }
            Err(shared) => {
                if tokio::time::Instant::now() >= until {
                    return Shutdown::StillHeld(shared);
                }
                device = shared;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Live connections through the node
// ---------------------------------------------------------------------------

/// The connections open through the node, by far address, for `connected`.
#[derive(Debug, Default)]
pub struct LiveConnections {
    open: Mutex<HashMap<IpAddr, usize>>,
}

impl LiveConnections {
    /// Count one connection from `ip` until the returned guard drops.
    pub fn enter(self: &Arc<Self>, ip: IpAddr) -> LiveGuard {
        let mut open = match self.open.lock() {
            Ok(open) => open,
            Err(poisoned) => poisoned.into_inner(),
        };
        *open.entry(ip).or_default() += 1;
        LiveGuard {
            live: Arc::clone(self),
            ip,
        }
    }

    /// How many distinct far addresses have a connection open now.
    pub fn distinct(&self) -> u32 {
        let open = match self.open.lock() {
            Ok(open) => open,
            Err(poisoned) => poisoned.into_inner(),
        };
        u32::try_from(open.len()).unwrap_or(u32::MAX)
    }
}

/// One counted connection; uncounted when dropped.
pub struct LiveGuard {
    live: Arc<LiveConnections>,
    ip: IpAddr,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let mut open = match self.live.open.lock() {
            Ok(open) => open,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(count) = open.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.ip);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The supervisor
// ---------------------------------------------------------------------------

/// What the accept loop hands each connection to: in the server,
/// [`crate::peer::listener::serve_one`] over the server's session context.
pub type ServeFn<S> =
    Arc<dyn Fn(Accepted<S>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Reads the default gateway, the network-change signal the router mapping
/// already uses (`crate::peer::reach`). Blocking; run off the async threads.
pub type GatewayFn = Arc<dyn Fn() -> Option<Ipv4Addr> + Send + Sync>;

/// The files the supervisor reads and writes.
#[derive(Debug, Clone)]
pub struct NodePaths {
    /// `tcr-peers.json`: the mode.
    pub peers: PathBuf,
    /// `tcr-tailnet.key`.
    pub key: PathBuf,
    /// `peer-state.json`: the record.
    pub state: PathBuf,
}

/// A running device and its accept loop.
struct Running<D: TailnetDevice> {
    device: Arc<D>,
    accept: tokio::task::JoinHandle<()>,
    me: NodeSelf,
}

/// Why the last start did not end in a running device, and when to try again.
#[derive(Debug, Clone)]
struct Backoff {
    wait: Duration,
    waited: Duration,
    state: TailnetState,
    detail: String,
}

/// A device build that is still running in its own task.
type PendingBuild<D> = tokio::task::JoinHandle<Result<D>>;

/// The server's half of node mode: owns one device, keeps it alive, records
/// what it holds. See the module docs.
///
/// # Never two devices from one key
///
/// Two devices with one node key fight over it at control. So at most one
/// device exists at a time, counting three ways a device can be alive besides
/// the running one: a build still in its task ([`Self::pending_build`]), a
/// device mid-start ([`Self::starting`]), and a device another holder kept
/// past its shutdown deadline ([`Self::lingering`]). Nothing new is built
/// while any of them exists.
pub struct Supervisor<F: DeviceFactory> {
    factory: Arc<F>,
    paths: NodePaths,
    port: u16,
    serve: ServeFn<<F::Device as TailnetDevice>::Stream>,
    gateway: GatewayFn,
    live: Arc<LiveConnections>,
    running: Option<Running<F::Device>>,
    /// A build that outlived [`BUILD_TIMEOUT`]. Kept rather than dropped:
    /// dropping `Device::new` part way leaves whatever runtime it already
    /// spawned with nothing to stop it. The next start waits on this one
    /// instead of building another.
    pending_build: Option<PendingBuild<F::Device>>,
    /// A device built and not yet running (authorizing, listening), so a
    /// shutdown that cancels the wake mid-start can still stop it.
    starting: Option<Arc<F::Device>>,
    /// A device still held by another caller at its shutdown deadline: alive,
    /// not shut down. Shut down as soon as it is free.
    lingering: Option<Arc<F::Device>>,
    backoff: Option<Backoff>,
    /// Failed starts and failed checks in a row; a passed health check
    /// resets it.
    failures: u32,
    since_health: Duration,
    last_gateway: Option<Option<Ipv4Addr>>,
    last_record: Option<(TailnetRecord, tokio::time::Instant)>,
    /// The ceiling on each wait on control, [`CONTROL_TIMEOUT`] outside tests.
    control_timeout: Duration,
    /// The ceiling on one build, [`BUILD_TIMEOUT`] outside tests.
    build_timeout: Duration,
    /// How long a device gets to stop, [`SHUTDOWN_DEADLINE`] outside tests.
    shutdown_deadline: Duration,
    /// Whether the last wake read the mode as `node`.
    was_node: bool,
    /// Whether the mode left `node` and the `nodeReleased` record has not
    /// been written yet, because a device was still alive.
    release_owed: bool,
    /// Devices built so far, for the log and for tests.
    builds: u64,
}

impl<F: DeviceFactory> Supervisor<F> {
    /// A supervisor with nothing running yet.
    pub fn new(
        factory: F,
        paths: NodePaths,
        port: u16,
        serve: ServeFn<<F::Device as TailnetDevice>::Stream>,
        gateway: GatewayFn,
    ) -> Self {
        Self {
            factory: Arc::new(factory),
            paths,
            port,
            serve,
            gateway,
            live: Arc::new(LiveConnections::default()),
            running: None,
            pending_build: None,
            starting: None,
            lingering: None,
            backoff: None,
            failures: 0,
            since_health: Duration::ZERO,
            last_gateway: None,
            last_record: None,
            control_timeout: CONTROL_TIMEOUT,
            build_timeout: BUILD_TIMEOUT,
            shutdown_deadline: SHUTDOWN_DEADLINE,
            was_node: false,
            release_owed: false,
            builds: 0,
        }
    }

    /// The same supervisor with a shorter ceiling on each wait on control, so
    /// a test of a hung device does not wait the real ten seconds.
    pub const fn with_control_timeout(mut self, limit: Duration) -> Self {
        self.control_timeout = limit;
        self
    }

    /// The same supervisor with a shorter build ceiling, for a test.
    pub const fn with_build_timeout(mut self, limit: Duration) -> Self {
        self.build_timeout = limit;
        self
    }

    /// The same supervisor with a shorter shutdown deadline, for a test.
    pub const fn with_shutdown_deadline(mut self, limit: Duration) -> Self {
        self.shutdown_deadline = limit;
        self
    }

    /// The live-connection counter, shared with whatever dials through the
    /// node.
    pub fn live(&self) -> Arc<LiveConnections> {
        Arc::clone(&self.live)
    }

    /// How many devices this supervisor has built.
    pub const fn builds(&self) -> u64 {
        self.builds
    }

    /// Whether a device is running now.
    pub const fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The running device, for a test that holds it the way an in-flight
    /// call does.
    #[cfg(test)]
    fn running_device(&self) -> Option<Arc<F::Device>> {
        self.running
            .as_ref()
            .map(|running| Arc::clone(&running.device))
    }

    /// Run until `stop` flips, one [`Self::tick`] per `every`, then shut the
    /// device down.
    ///
    /// The stop is watched DURING a wake too: a start can take a minute (a
    /// build plus three waits on control), and a quit that waited it out would
    /// usually be cut by the server's shutdown grace before the device was
    /// shut down at all.
    pub async fn run(mut self, every: Duration, mut stop: tokio::sync::watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let now = tokio::time::Instant::now();
                    let elapsed = now.saturating_duration_since(last);
                    last = now;
                    tokio::select! {
                        () = self.tick(elapsed, crate::now_ms()) => {}
                        _ = stop.changed() => break,
                    }
                }
                _ = stop.changed() => break,
            }
        }
        self.stop_everything().await;
    }

    /// On the way out of the process: stop every device that is alive, each
    /// within the shutdown deadline. A build still in its task is given the
    /// same deadline to finish and then shut down.
    async fn stop_everything(&mut self) {
        self.stop_device().await;
        if let Some(starting) = self.starting.take() {
            self.settle(starting).await;
        }
        if let Some(lingering) = self.lingering.take() {
            self.settle(lingering).await;
        }
        if let Some(pending) = self.pending_build.take() {
            match tokio::time::timeout(self.shutdown_deadline, pending).await {
                Ok(Ok(Ok(device))) => {
                    device.shutdown(self.shutdown_deadline).await;
                }
                Ok(Ok(Err(_)) | Err(_)) => {}
                Err(_) => tracing::warn!(
                    "peer tailnet node: a node build was still running at shutdown and was left \
                     to the process exit"
                ),
            }
        }
    }

    /// Shut `device` down, or keep it in [`Self::lingering`] when another
    /// holder still has it.
    async fn settle(&mut self, device: Arc<F::Device>) {
        match shutdown_shared(device, self.shutdown_deadline).await {
            Shutdown::Stopped { graceful: true } => {}
            Shutdown::Stopped { graceful: false } => tracing::warn!(
                deadline_secs = self.shutdown_deadline.as_secs(),
                "peer tailnet node: the node did not stop gracefully within its deadline and \
                 was stopped by force"
            ),
            Shutdown::StillHeld(device) => {
                tracing::warn!(
                    holders = Arc::strong_count(&device),
                    "peer tailnet node: the node was still in use at its shutdown deadline, so it \
                     is left running; no new node is built from the same keys until it stops"
                );
                self.lingering = Some(device);
            }
        }
    }

    /// Shut down a lingering device if it is free now. Reports whether none
    /// lingers any more.
    async fn reap_lingering(&mut self) -> bool {
        let Some(device) = self.lingering.take() else {
            return true;
        };
        match Arc::try_unwrap(device) {
            Ok(owned) => {
                if !owned.shutdown(self.shutdown_deadline).await {
                    tracing::warn!(
                        "peer tailnet node: the previous node was stopped by force once it was \
                         free"
                    );
                }
                true
            }
            Err(shared) => {
                self.lingering = Some(shared);
                false
            }
        }
    }

    /// A build that finished after its wait, outside node mode: shut its
    /// device down. Reports whether no build is pending any more.
    async fn reap_pending_build(&mut self) -> bool {
        let Some(pending) = self.pending_build.take() else {
            return true;
        };
        if !pending.is_finished() {
            self.pending_build = Some(pending);
            return false;
        }
        if let Ok(Ok(device)) = pending.await {
            device.shutdown(self.shutdown_deadline).await;
        }
        true
    }

    /// One wake: re-read the mode, start, check or stop the device, and record
    /// what came of it. `elapsed` is the time since the last wake.
    pub async fn tick(&mut self, elapsed: Duration, now_ms: i64) {
        self.since_health = self.since_health.saturating_add(elapsed);
        if let Some(backoff) = &mut self.backoff {
            backoff.waited = backoff.waited.saturating_add(elapsed);
        }

        // Hot, like `discovery`: a peers file that does not read for a moment
        // is not the same fact as the mode being off, so whatever runs is
        // left as it is until the next wake.
        let mode = match crate::peer::config::read_or_default(&self.paths.peers) {
            Ok(file) => file.tailnet.effective(),
            Err(err) => {
                tracing::warn!(
                    path = %self.paths.peers.display(),
                    error = %err,
                    "peer tailnet node: the peers file did not read, so the node is left as it \
                     is until the next wake"
                );
                return;
            }
        };

        let was_node = std::mem::replace(&mut self.was_node, mode == TailnetMode::Node);
        if mode != TailnetMode::Node {
            if was_node {
                self.release_owed = true;
            }
            if self.running.is_some() {
                self.stop_device().await;
                tracing::info!(
                    mode = mode.as_str(),
                    "peer tailnet node: the mode is no longer `node`, so the node was shut down"
                );
            }
            self.backoff = None;
            self.failures = 0;
            let free = self.reap_lingering().await;
            let built = self.reap_pending_build().await;
            // The one record this supervisor writes outside node mode:
            // `nodeReleased`, once, on any way out of `node`, and only when no
            // device from the node's keys is alive any more. Nothing else
            // writes it, so `leave` can wait for it and delete the keys
            // knowing nothing holds them. Every other off and app record is
            // the browse tick's, and a Mac that never used node mode never has
            // this supervisor write the state file at all.
            if self.release_owed && free && built {
                self.release_owed = false;
                self.last_record = None;
                self.record(mode, TailnetState::NodeReleased, None, now_ms);
            }
            return;
        }
        self.release_owed = false;

        if !F::SUPPORTED {
            self.record(
                TailnetMode::Node,
                TailnetState::NodeUnsupported,
                Some(
                    "This tcr was built without tcr's own tailnet node; build it from source \
                     with `--features tailnet-node`, or use the Tailscale app."
                        .to_string(),
                ),
                now_ms,
            );
            return;
        }

        let keys = match read_key_file(&self.paths.key) {
            Ok(Some(keys)) => keys,
            Ok(None) => {
                self.stop_device().await;
                self.record(TailnetMode::Node, TailnetState::NodeNotJoined, None, now_ms);
                return;
            }
            Err(err) => {
                self.stop_device().await;
                self.record(
                    TailnetMode::Node,
                    TailnetState::NodeNotJoined,
                    Some(format!("The node's key file was refused: {err:#}")),
                    now_ms,
                );
                return;
            }
        };

        if self.running.is_some() {
            if self.check_health().await {
                self.record_ready(now_ms);
                return;
            }
            self.stop_device().await;
            self.fail(
                RETRY_AFTER_FAILURE,
                TailnetState::NodeConnecting,
                "The node stopped answering; it is being rebuilt from the same keys.".to_string(),
                now_ms,
            );
            // The first failure after a healthy run rebuilds at once; a
            // device that keeps failing waits out its backoff.
            if self.failures > 1 {
                return;
            }
        } else if let Some(backoff) = &self.backoff
            && backoff.waited < backoff.wait
        {
            let (state, detail) = (backoff.state, backoff.detail.clone());
            self.record(TailnetMode::Node, state, Some(detail), now_ms);
            return;
        }

        if !self.reap_lingering().await {
            self.record(
                TailnetMode::Node,
                TailnetState::NodeConnecting,
                Some(
                    "The previous node is still in use; a new one starts once it has stopped."
                        .to_string(),
                ),
                now_ms,
            );
            return;
        }
        self.start(&keys, now_ms).await;
    }

    /// Whether the running device is still alive: its accept loop has not
    /// ended, and on a health wake (every [`HEALTH_EVERY`]) this Mac is on the
    /// same network as before and control still answers a `self_node()`. A
    /// passed health wake resets the failure count.
    ///
    /// The gateway is read on health wakes only, because it runs a route tool:
    /// a wake from sleep comes with an elapsed time past [`HEALTH_EVERY`], so
    /// the first wake after a move is a health wake anyway.
    async fn check_health(&mut self) -> bool {
        let Some(running) = &self.running else {
            return false;
        };
        if running.accept.is_finished() {
            tracing::warn!(
                "peer tailnet node: the node's accept loop ended, so the node is rebuilt"
            );
            return false;
        }
        if self.since_health < HEALTH_EVERY {
            return true;
        }
        self.since_health = Duration::ZERO;
        let device = Arc::clone(&running.device);

        let gateway = self.read_gateway().await;
        let moved = self.last_gateway.is_some_and(|before| before != gateway);
        self.last_gateway = Some(gateway);
        if moved {
            tracing::info!(
                "peer tailnet node: this Mac is on another network, so the node is rebuilt"
            );
            return false;
        }

        match bounded(
            self.control_timeout,
            "the node's health check",
            device.self_node(),
        )
        .await
        {
            Ok(me) => {
                if let Some(running) = &mut self.running {
                    running.me = me;
                }
                self.failures = 0;
                true
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "peer tailnet node: the node failed its health check, so it is rebuilt from \
                     the same keys"
                );
                false
            }
        }
    }

    /// The default gateway now, off the async threads. A read that panicked
    /// is the last one seen, with a line.
    async fn read_gateway(&self) -> Option<Ipv4Addr> {
        let read = Arc::clone(&self.gateway);
        match tokio::task::spawn_blocking(move || read()).await {
            Ok(gateway) => gateway,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "peer tailnet node: the default gateway could not be read this wake"
                );
                self.last_gateway.flatten()
            }
        }
    }

    /// Build a device from `keys`, or take the one a build still in its task
    /// produces, under [`Self::build_timeout`]. A build that outlives the
    /// wait is kept in [`Self::pending_build`] for the next start.
    async fn build(&mut self, keys: &NodeKeyFile) -> Result<F::Device> {
        // Held in `self` while it is awaited, so a wake cancelled by the
        // process stopping still leaves the build where `stop_everything`
        // finds it.
        if self.pending_build.is_none() {
            self.builds += 1;
            let factory = Arc::clone(&self.factory);
            let keys = keys.clone();
            self.pending_build = Some(tokio::spawn(
                async move { factory.build(&keys, None).await },
            ));
        }
        let Some(pending) = self.pending_build.as_mut() else {
            bail!("the node's build task was not kept");
        };
        match tokio::time::timeout(self.build_timeout, pending).await {
            Ok(joined) => {
                self.pending_build = None;
                match joined {
                    Ok(built) => built,
                    Err(join) => Err(anyhow!("the node's build task failed: {join}")),
                }
            }
            Err(_elapsed) => {
                bail!(
                    "the node did not start within {}s (no network, or control is \
                     unreachable); that build is kept and waited on again",
                    self.build_timeout.as_secs()
                )
            }
        }
    }

    /// Build a device from `keys`, check it is authorized, listen on its
    /// address, and start the accept loop. Every failure is recorded and
    /// backed off.
    async fn start(&mut self, keys: &NodeKeyFile, now_ms: i64) {
        self.record(
            TailnetMode::Node,
            TailnetState::NodeConnecting,
            None,
            now_ms,
        );
        let device = match self.build(keys).await {
            Ok(device) => Arc::new(device),
            Err(err) => {
                self.fail(
                    RETRY_AFTER_FAILURE,
                    TailnetState::NodeConnecting,
                    format!("The node could not start: {err:#}"),
                    now_ms,
                );
                return;
            }
        };
        self.starting = Some(Arc::clone(&device));

        let refusal = match bounded(self.control_timeout, "control", device.authorization()).await {
            Ok(Authorization::Authorized) => None,
            Ok(Authorization::NotAuthorized) => Some((
                RETRY_AFTER_REFUSAL,
                TailnetState::NodeNotJoined,
                "The tailnet no longer accepts this node's keys; join again with `tcr peer \
                 tailnet leave` then `tcr peer tailnet node --auth-key -`."
                    .to_string(),
            )),
            Err(err) => Some((
                RETRY_AFTER_FAILURE,
                TailnetState::NodeConnecting,
                format!("{err:#}"),
            )),
        };
        let started = if let Some(refused) = refusal {
            Err(refused)
        } else {
            let limit = self.control_timeout;
            let port = self.port;
            async {
                let me = bounded(limit, "the node's address", device.self_node()).await?;
                let bind = SocketAddr::from((me.address, port));
                let listener = bounded(limit, "the node's listener", device.listen(bind)).await?;
                Ok::<_, anyhow::Error>((me, listener))
            }
            .await
            .map_err(|err| {
                (
                    RETRY_AFTER_FAILURE,
                    TailnetState::NodeConnecting,
                    format!("{err:#}"),
                )
            })
        };
        let (me, listener) = match started {
            Ok(started) => started,
            Err((first, state, detail)) => {
                self.starting = None;
                self.settle(device).await;
                self.fail(first, state, detail, now_ms);
                return;
            }
        };

        let accept = tokio::spawn(accept_loop(
            listener,
            Arc::clone(&self.serve),
            Arc::clone(&self.live),
        ));
        tracing::info!(
            address = %me.address,
            name = %me.name,
            port = self.port,
            builds = self.builds,
            "peer tailnet node: on the tailnet, listening on the node's own address"
        );
        self.starting = None;
        self.running = Some(Running { device, accept, me });
        self.backoff = None;
        self.since_health = Duration::ZERO;
        // The network this device started on, so the first health wake can
        // tell a move from no move.
        self.last_gateway = Some(self.read_gateway().await);
        self.record_ready(now_ms);
    }

    /// Record a failure and back off: `first` for the first failure in a
    /// row, doubling to [`RETRY_CAP`].
    fn fail(&mut self, first: Duration, state: TailnetState, detail: String, now_ms: i64) {
        self.failures = self.failures.saturating_add(1);
        let wait = retry_wait(first, self.failures);
        tracing::warn!(
            detail = %detail,
            failures = self.failures,
            retry_secs = wait.as_secs(),
            "peer tailnet node: the node is not on the tailnet"
        );
        self.record(TailnetMode::Node, state, Some(detail.clone()), now_ms);
        self.backoff = Some(Backoff {
            wait,
            waited: Duration::ZERO,
            state,
            detail,
        });
    }

    /// Stop the accept loop and shut the device down, if one runs. A device
    /// another holder keeps past the deadline lingers (see
    /// [`Self::settle`]).
    async fn stop_device(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        running.accept.abort();
        self.settle(running.device).await;
    }

    fn record_ready(&mut self, now_ms: i64) {
        let Some(running) = &self.running else {
            return;
        };
        let record = TailnetRecord {
            mode: TailnetMode::Node.into(),
            state: TailnetState::Ready,
            address: Some(running.me.address),
            node_name: Some(running.me.name.clone()),
            found: 0,
            connected: self.live.distinct(),
            detail: None,
            recorded_at_ms: now_ms,
        };
        self.write_record(record);
    }

    fn record(
        &mut self,
        mode: TailnetMode,
        state: TailnetState,
        detail: Option<String>,
        now_ms: i64,
    ) {
        let record = TailnetRecord {
            mode: mode.into(),
            state,
            address: None,
            node_name: None,
            found: 0,
            connected: 0,
            detail,
            recorded_at_ms: now_ms,
        };
        self.write_record(record);
    }

    /// Write `record` when it says something new, or when the last write is
    /// older than [`RECORD_REFRESH`]: every write moves the state file's mtime,
    /// and an unchanged record every 10 s is a write for nothing.
    fn write_record(&mut self, record: TailnetRecord) {
        let now = tokio::time::Instant::now();
        if let Some((last, at)) = &self.last_record {
            let same = TailnetRecord {
                recorded_at_ms: record.recorded_at_ms,
                ..last.clone()
            } == record;
            if same && now.saturating_duration_since(*at) < RECORD_REFRESH {
                return;
            }
        }
        match crate::peer::state::save_tailnet(&self.paths.state, record.clone(), Vec::new()) {
            Ok(()) => self.last_record = Some((record, now)),
            Err(err) => tracing::warn!(
                path = %self.paths.state.display(),
                error = %err,
                "peer tailnet node: the tailnet record could not be written, so `tcr peer \
                 tailnet` reports an older one until the next wake"
            ),
        }
    }
}

/// Accept on the node until its listener fails, handing each connection to
/// `serve` and counting it in `live` while it is open.
async fn accept_loop<L: TailnetListener>(
    listener: L,
    serve: ServeFn<L::Stream>,
    live: Arc<LiveConnections>,
) {
    loop {
        match listener.accept().await {
            Ok(accepted) => {
                let guard = live.enter(accepted.from.ip());
                let served = serve(accepted);
                tokio::spawn(async move {
                    served.await;
                    drop(guard);
                });
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "peer tailnet node: the node's listener stopped; the next wake rebuilds it"
                );
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Join and leave, the CLI's half
// ---------------------------------------------------------------------------

/// `tcr peer tailnet node --auth-key -`: join the tailnet, then set the mode.
///
/// The order is the contract. A device is built in this process with the
/// auth key; control has to say the node is authorized within `deadline`; the
/// device is shut down; the key file is written (0600, never over an existing
/// one); and only then is `tailnet: "node"` written, so a server never sees
/// the mode before the keys it needs exist. A refused key leaves both files as
/// they were.
///
/// Refuses while the mode is already `node` with a key file: the serving
/// process may be running a device with those keys right now, and two
/// devices with one node key fight over it. `leave` first.
pub async fn join_with<F: DeviceFactory>(
    factory: &F,
    peers_path: &Path,
    key_path: &Path,
    name: Option<String>,
    auth_key: AuthKey,
    deadline: Duration,
) -> Result<NodeSelf> {
    if !F::SUPPORTED {
        bail!(
            "peer tailnet: this tcr was built without tcr's own tailnet node, so it cannot join \
             one; build tcr from source with `--features tailnet-node`, or use the Tailscale app"
        );
    }
    if let Some(name) = &name {
        check_node_name(name)?;
    }
    let file = crate::peer::config::read_or_default(peers_path)?;
    let existing = read_key_file(key_path)?;
    if file.tailnet.effective() == TailnetMode::Node && existing.is_some() {
        bail!(
            "peer tailnet: this Mac is already a node on a tailnet; run `tcr peer tailnet leave` \
             first to join again"
        );
    }
    let fresh = existing.is_none();
    let keys = match existing {
        Some(keys) => {
            if name.is_some() && name != keys.hostname {
                bail!(
                    "peer tailnet: {} already holds keys under another name; run `tcr peer \
                     tailnet leave` first to join under a new one",
                    key_path.display()
                );
            }
            keys
        }
        None => NodeKeyFile {
            key_state: factory.mint_keys()?,
            hostname: name,
        },
    };

    let device = bounded(
        deadline,
        "joining the tailnet",
        factory.build(&keys, Some(auth_key)),
    )
    .await?;
    let joined = async {
        // A refused key does not come back as `NotAuthorized` at 0.6.1: the
        // library's control task exits and the question fails with its
        // "actor missing" error (upstream #383; seen here with a made-up
        // key). So a failure is worded as both things it can be.
        let answer = bounded(deadline, "control", device.authorization())
            .await
            .context(
                "peer tailnet: the tailnet did not accept this auth key, or control could not \
                 be reached; check the key in the Tailscale admin console and the network",
            )?;
        match answer {
            Authorization::Authorized => {}
            Authorization::NotAuthorized => bail!(
                "peer tailnet: the tailnet did not accept this auth key; make a new one in the \
                 Tailscale admin console and try again"
            ),
        }
        bounded(CONTROL_TIMEOUT, "the node's address", device.self_node()).await
    }
    .await;
    // The device is shut down on every path: a node left running in a CLI
    // process that is about to exit is a node control sees vanish.
    let me = match joined {
        Ok(me) => me,
        Err(err) => {
            device.shutdown(SHUTDOWN_DEADLINE).await;
            return Err(err);
        }
    };
    // Shut down BEFORE the key file exists: a server already in node mode
    // starts its own device from that file on its next wake, and two devices
    // on one key fight over it.
    if !device.shutdown(SHUTDOWN_DEADLINE).await {
        tracing::warn!("peer tailnet: the joining node was stopped by force");
    }
    if fresh && let Err(err) = write_key_file(key_path, &keys) {
        return Err(err.context(
            "peer tailnet: the tailnet accepted this node but its keys could not be saved; \
             remove the machine in the Tailscale admin console and join again",
        ));
    }

    let _lock = crate::peer::config::FileLock::acquire(peers_path)?;
    let mut file = crate::peer::config::read_or_default(peers_path)?;
    file.tailnet = TailnetMode::Node.into();
    crate::peer::config::save(peers_path, &file)?;
    Ok(me)
}

/// What `leave` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveOutcome {
    /// The key file was deleted.
    Deleted,
    /// There was no key file; only the mode was set.
    NoKeyFile,
}

/// `tcr peer tailnet leave`: set the mode to `off`, wait until the serving
/// process has released the node, then delete the key file.
///
/// The wait is what stops the delete pulling the keys out from under a node
/// the server is still running. It waits for `nodeReleased` written after
/// this call set the mode: the one record only the node's supervisor writes,
/// and only once no device built from the keys is alive. `mode == off` is not
/// enough, because the browse tick writes `off` records too, whatever the node
/// is doing.
///
/// It is skipped when no fresh node-mode record exists when this starts
/// (`held` absent, stale, or not in node mode): then no server holds a node.
/// When a server holds one and has not released it within `wait`, the key file
/// is kept and this fails, saying so.
pub async fn leave_with(
    peers_path: &Path,
    key_path: &Path,
    wait: Duration,
    poll: Duration,
) -> Result<LeaveOutcome> {
    let state_path = crate::peer::serve::peer_state_path(peers_path);
    let held_at = |now_ms: i64| -> Result<Option<TailnetRecord>> {
        Ok(crate::peer::state::load(&state_path, now_ms)
            .with_context(|| format!("reading {}", state_path.display()))?
            .tailnet)
    };

    let asked_ms = crate::now_ms();
    let holds_a_node = held_at(asked_ms)?.is_some_and(|record| {
        record.mode.effective() == TailnetMode::Node && !record.is_stale(asked_ms)
    });
    {
        let _lock = crate::peer::config::FileLock::acquire(peers_path)?;
        let mut file = crate::peer::config::read_or_default(peers_path)?;
        file.tailnet = TailnetMode::Off.into();
        crate::peer::config::save(peers_path, &file)?;
    }

    let until = tokio::time::Instant::now() + wait;
    if holds_a_node {
        loop {
            let released = held_at(crate::now_ms())?.is_some_and(|record| {
                record.state == TailnetState::NodeReleased && record.recorded_at_ms >= asked_ms
            });
            if released {
                break;
            }
            if tokio::time::Instant::now() >= until {
                bail!(
                    "peer tailnet leave: the mode is now off, but the running tcr has not released \
                 the node after {}s, so {} was kept; run this again in a moment",
                    wait.as_secs(),
                    key_path.display()
                );
            }
            tokio::time::sleep(poll).await;
        }
    }

    if remove_key_file(key_path)? {
        Ok(LeaveOutcome::Deleted)
    } else {
        Ok(LeaveOutcome::NoKeyFile)
    }
}

/// The sentence `leave` ends with. The library has no call that logs a node
/// out of control (its `Device` has none at 0.6.1), so the machine stays in
/// the admin console until it is removed there.
pub const LEAVE_ADMIN_CONSOLE_NOTE: &str = "tcr cannot log a node out of the tailnet itself, so \
     this machine stays listed in the Tailscale admin console until you remove it there";

// ---------------------------------------------------------------------------
// The library, or its absence
// ---------------------------------------------------------------------------

/// The factory a build without the library has: it builds nothing, and the
/// supervisor records `nodeUnsupported`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unsupported;

/// A device that cannot exist.
#[derive(Debug)]
pub enum NoDevice {}

/// A listener that cannot exist.
#[derive(Debug)]
pub enum NoListener {}

impl TailnetListener for NoListener {
    type Stream = tokio::io::DuplexStream;

    async fn accept(&self) -> Result<Accepted<Self::Stream>> {
        match *self {}
    }
}

impl TailnetDevice for NoDevice {
    type Stream = tokio::io::DuplexStream;
    type Listener = NoListener;

    async fn authorization(&self) -> Result<Authorization> {
        match *self {}
    }

    async fn self_node(&self) -> Result<NodeSelf> {
        match *self {}
    }

    async fn listen(&self, _addr: SocketAddr) -> Result<Self::Listener> {
        match *self {}
    }

    async fn shutdown(self, _deadline: Duration) -> bool {
        match self {}
    }
}

impl DeviceFactory for Unsupported {
    type Device = NoDevice;
    const SUPPORTED: bool = false;

    fn mint_keys(&self) -> Result<serde_json::Value> {
        bail!("peer tailnet: this tcr was built without tcr's own tailnet node")
    }

    async fn build(&self, _keys: &NodeKeyFile, _auth_key: Option<AuthKey>) -> Result<NoDevice> {
        bail!("peer tailnet: this tcr was built without tcr's own tailnet node")
    }
}

#[cfg(all(feature = "tailnet-node", target_os = "macos", target_arch = "aarch64"))]
mod library {
    //! The `tailscale` crate behind [`super::TailnetDevice`].

    use std::net::SocketAddr;
    use std::time::Duration;

    use anyhow::{Context as _, Result, anyhow};

    use super::{Accepted, AuthKey, Authorization, NodeKeyFile, NodeSelf};

    /// Builds `tailscale::Device`s.
    #[derive(Debug, Clone, Copy, Default)]
    pub(crate) struct LibraryFactory;

    /// One `tailscale::Device`.
    pub(crate) struct LibraryDevice(tailscale::Device);

    /// One `tailscale` TCP listener.
    pub(crate) struct LibraryListener(tailscale::netstack::TcpListener);

    impl super::TailnetListener for LibraryListener {
        type Stream = tailscale::netstack::TcpStream;

        async fn accept(&self) -> Result<Accepted<Self::Stream>> {
            let stream = self
                .0
                .accept()
                .await
                .map_err(|err| anyhow!("the node's listener: {err}"))?;
            Ok(Accepted {
                from: stream.remote_addr(),
                bind: stream.local_addr(),
                stream,
            })
        }
    }

    impl super::TailnetDevice for LibraryDevice {
        type Stream = tailscale::netstack::TcpStream;
        type Listener = LibraryListener;

        async fn authorization(&self) -> Result<Authorization> {
            match self.0.is_authorized().await.context("asking control")? {
                tailscale::AuthState::Authorized => Ok(Authorization::Authorized),
                // The login URL is not used: version one is auth keys only
                // (upstream #383), and the URL is not something to log.
                tailscale::AuthState::NotAuthorized(_url) => Ok(Authorization::NotAuthorized),
            }
        }

        async fn self_node(&self) -> Result<NodeSelf> {
            let node = self.0.self_node().await.context("asking for this node")?;
            let address = node.tailnet_address.ipv4.addr();
            Ok(NodeSelf {
                address,
                name: node.hostname,
            })
        }

        async fn listen(&self, addr: SocketAddr) -> Result<Self::Listener> {
            self.0
                .tcp_listen(addr)
                .await
                .map(LibraryListener)
                .with_context(|| format!("listening on {addr}"))
        }

        async fn shutdown(self, deadline: Duration) -> bool {
            self.0.shutdown(Some(deadline)).await
        }
    }

    /// The library's keys out of the key file.
    ///
    /// Through text, not `serde_json::from_value`: the library's key types
    /// deserialize only from a BORROWED string, which an owned `Value` cannot
    /// lend ("expected a borrowed string"). And the parse error is dropped,
    /// not wrapped, because serde quotes the value it choked on, which here is
    /// a private key.
    pub(super) fn persist_state(keys: &NodeKeyFile) -> Result<tailscale::keys::PersistState> {
        let text = serde_json::to_string(&keys.key_state)
            .map_err(|_| anyhow!("the tailnet key file's keys could not be read"))?;
        serde_json::from_str(&text).map_err(|_| {
            anyhow!(
                "the tailnet key file's keys are not in the form this build of the library \
                 reads (written by another version?)"
            )
        })
    }

    impl super::DeviceFactory for LibraryFactory {
        type Device = LibraryDevice;
        const SUPPORTED: bool = true;

        fn mint_keys(&self) -> Result<serde_json::Value> {
            serde_json::to_value(tailscale::keys::PersistState::default())
                .context("serializing fresh tailnet keys")
        }

        async fn build(
            &self,
            keys: &NodeKeyFile,
            auth_key: Option<AuthKey>,
        ) -> Result<LibraryDevice> {
            // Both rustls providers are compiled in with this feature, and
            // rustls then refuses to choose: the library's control client
            // calls `ClientConfig::builder()`, which panics unless a default
            // is installed. Installed here, the one place a device is built,
            // so no order of calls elsewhere can get ahead of it.
            crate::http_client::install_crypto_provider();
            let key_state = persist_state(keys)?;
            let config = tailscale::Config {
                key_state,
                client_name: Some("tcr".to_string()),
                requested_hostname: keys.hostname.clone(),
                ..tailscale::Config::default()
            };
            tailscale::Device::new(&config, auth_key.map(AuthKey::into_secret))
                .await
                .map(LibraryDevice)
                .context("starting the node")
        }
    }
}

#[cfg(all(
    test,
    feature = "tailnet-node",
    target_os = "macos",
    target_arch = "aarch64"
))]
mod library_tests {
    use super::DeviceFactory as _;

    /// **Keys tcr mints are keys the library reads back**, through the key
    /// file's opaque `key_state`, so a node that joined can be started by
    /// the server from the file alone.
    #[test]
    fn minted_keys_round_trip_through_the_key_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = super::key_path(dir.path());
        let minted = super::library::LibraryFactory
            .mint_keys()
            .expect("fresh keys");
        super::write_key_file(
            &path,
            &super::NodeKeyFile {
                key_state: minted,
                hostname: Some("tcr-studio".to_string()),
            },
        )
        .expect("write");
        let read = super::read_key_file(&path).expect("read").expect("present");
        super::library::persist_state(&read).expect("the library reads its keys back");
    }
}

/// The factory this binary builds devices with.
#[cfg(all(feature = "tailnet-node", target_os = "macos", target_arch = "aarch64"))]
pub(crate) type BuiltFactory = library::LibraryFactory;

/// The factory this binary builds devices with.
#[cfg(not(all(feature = "tailnet-node", target_os = "macos", target_arch = "aarch64")))]
pub(crate) type BuiltFactory = Unsupported;

/// `tcr peer tailnet node --auth-key -` with this binary's factory.
pub async fn join(peers_path: &Path, name: Option<String>, auth_key: AuthKey) -> Result<NodeSelf> {
    join_with(
        &BuiltFactory::default(),
        peers_path,
        &key_path_for_peers(peers_path),
        name,
        auth_key,
        JOIN_DEADLINE,
    )
    .await
}

/// The server's supervisor, for the peers file at `peers_path`, serving
/// accepted connections through `context` on `port`. Runs until `stop` flips.
pub async fn run_in_server(
    peers_path: PathBuf,
    state_path: PathBuf,
    port: u16,
    context: crate::peer::listener::SessionContext,
    stop: tokio::sync::watch::Receiver<bool>,
) {
    let serve: ServeFn<<<BuiltFactory as DeviceFactory>::Device as TailnetDevice>::Stream> =
        Arc::new(move |accepted| {
            let context = context.clone();
            Box::pin(crate::peer::listener::serve_one(
                accepted.stream,
                context,
                accepted.from,
                accepted.bind,
                // The node's socket is bound to the node's own address, so the
                // address a connection was sent to is the bind itself.
                Some(accepted.bind),
            ))
        });
    let paths = NodePaths {
        key: key_path_for_peers(&peers_path),
        peers: peers_path,
        state: state_path,
    };
    let gateway: GatewayFn = Arc::new(crate::peer::reach::default_gateway_ipv4);
    // A build without the library only ever records `nodeUnsupported` when the
    // mode is `node`, which needs no 10 s wake.
    let every = if <BuiltFactory as DeviceFactory>::SUPPORTED {
        TICK
    } else {
        TICK * 2
    };
    Supervisor::new(BuiltFactory::default(), paths, port, serve, gateway)
        .run(every, stop)
        .await;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::io::DuplexStream;
    use tokio::sync::mpsc;

    use super::*;
    use crate::peer::config::PeerFile;

    /// What the fake device does, shared with the test that drives it.
    #[derive(Clone)]
    struct Script {
        hangs: Arc<AtomicUsize>,
        authorized: Arc<AtomicBool>,
        build_fails: Arc<AtomicBool>,
        builds: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
        auth_keys_seen: Arc<AtomicUsize>,
        incoming: Arc<Mutex<Option<mpsc::Receiver<Accepted<DuplexStream>>>>>,
        /// While set, a build waits for `release` before it answers.
        build_hangs: Arc<AtomicBool>,
        release: Arc<tokio::sync::Notify>,
        /// A key file whose existence each shutdown records in
        /// `key_existed_at_shutdown`.
        watch_key: Arc<Mutex<Option<PathBuf>>>,
        key_existed_at_shutdown: Arc<AtomicBool>,
    }

    impl Script {
        fn new() -> Self {
            Self {
                hangs: Arc::new(AtomicUsize::new(0)),
                authorized: Arc::new(AtomicBool::new(true)),
                build_fails: Arc::new(AtomicBool::new(false)),
                builds: Arc::new(AtomicUsize::new(0)),
                shutdowns: Arc::new(AtomicUsize::new(0)),
                auth_keys_seen: Arc::new(AtomicUsize::new(0)),
                incoming: Arc::new(Mutex::new(None)),
                build_hangs: Arc::new(AtomicBool::new(false)),
                release: Arc::new(tokio::sync::Notify::new()),
                watch_key: Arc::new(Mutex::new(None)),
                key_existed_at_shutdown: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    struct FakeFactory(Script);
    struct FakeDevice(Script);
    struct FakeListener(tokio::sync::Mutex<Option<mpsc::Receiver<Accepted<DuplexStream>>>>);

    impl TailnetListener for FakeListener {
        type Stream = DuplexStream;

        async fn accept(&self) -> Result<Accepted<DuplexStream>> {
            let mut incoming = self.0.lock().await;
            match incoming.as_mut() {
                Some(rx) => match rx.recv().await {
                    Some(accepted) => Ok(accepted),
                    None => std::future::pending().await,
                },
                None => std::future::pending().await,
            }
        }
    }

    impl TailnetDevice for FakeDevice {
        type Stream = DuplexStream;
        type Listener = FakeListener;

        async fn authorization(&self) -> Result<Authorization> {
            Ok(if self.0.authorized.load(Ordering::SeqCst) {
                Authorization::Authorized
            } else {
                Authorization::NotAuthorized
            })
        }

        async fn self_node(&self) -> Result<NodeSelf> {
            if self
                .0
                .hangs
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                // What a device whose control task panicked looks like: the
                // call never returns. The supervisor's timeout is the test.
                std::future::pending::<()>().await;
            }
            Ok(NodeSelf {
                address: Ipv4Addr::new(100, 64, 0, 7),
                name: "tcr-studio".to_string(),
            })
        }

        async fn listen(&self, _addr: SocketAddr) -> Result<FakeListener> {
            let rx = match self.0.incoming.lock() {
                Ok(mut held) => held.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            Ok(FakeListener(tokio::sync::Mutex::new(rx)))
        }

        async fn shutdown(self, _deadline: Duration) -> bool {
            let watched = match self.0.watch_key.lock() {
                Ok(held) => held.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            if let Some(path) = watched {
                self.0
                    .key_existed_at_shutdown
                    .store(path.exists(), Ordering::SeqCst);
            }
            self.0.shutdowns.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    impl DeviceFactory for FakeFactory {
        type Device = FakeDevice;
        const SUPPORTED: bool = true;

        fn mint_keys(&self) -> Result<serde_json::Value> {
            Ok(serde_json::json!({ "fake": "keys" }))
        }

        async fn build(
            &self,
            _keys: &NodeKeyFile,
            auth_key: Option<AuthKey>,
        ) -> Result<FakeDevice> {
            self.0.builds.fetch_add(1, Ordering::SeqCst);
            if auth_key.is_some() {
                self.0.auth_keys_seen.fetch_add(1, Ordering::SeqCst);
            }
            if self.0.build_hangs.load(Ordering::SeqCst) {
                self.0.release.notified().await;
            }
            if self.0.build_fails.load(Ordering::SeqCst) {
                bail!("no network");
            }
            Ok(FakeDevice(self.0.clone()))
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: NodePaths,
    }

    fn fixture(mode: TailnetMode) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers = dir.path().join("tcr-peers.json");
        crate::peer::config::save(
            &peers,
            &PeerFile {
                tailnet: mode.into(),
                ..Default::default()
            },
        )
        .expect("seed the peers file");
        let paths = NodePaths {
            key: key_path_for_peers(&peers),
            state: crate::peer::serve::peer_state_path(&peers),
            peers,
        };
        Fixture { _dir: dir, paths }
    }

    fn some_keys() -> NodeKeyFile {
        NodeKeyFile {
            key_state: serde_json::json!({ "fake": "keys" }),
            hostname: Some("tcr-studio".to_string()),
        }
    }

    fn set_mode(paths: &NodePaths, mode: TailnetMode) {
        let mut file = crate::peer::config::read_or_default(&paths.peers).expect("read");
        file.tailnet = mode.into();
        crate::peer::config::save(&paths.peers, &file).expect("save");
    }

    fn held(paths: &NodePaths) -> TailnetRecord {
        crate::peer::state::load(&paths.state, crate::now_ms())
            .expect("load the state file")
            .tailnet
            .expect("a tailnet record")
    }

    fn supervisor<F: DeviceFactory>(
        factory: F,
        paths: &NodePaths,
        gateway: GatewayFn,
    ) -> Supervisor<F> {
        let serve: ServeFn<<F::Device as TailnetDevice>::Stream> =
            Arc::new(|_accepted| Box::pin(async {}));
        Supervisor::new(factory, paths.clone(), 7755, serve, gateway)
            .with_control_timeout(Duration::from_millis(100))
            .with_shutdown_deadline(Duration::from_millis(200))
    }

    fn fixed_gateway() -> GatewayFn {
        Arc::new(|| Some(Ipv4Addr::new(192, 168, 1, 1)))
    }

    /// **The key file is written 0600, refused at any other mode, and never
    /// written over.**
    ///
    /// Watch it fail by removing the mode check in `read_key_file`: the 0644
    /// file is then read and the `expect_err` fails.
    #[test]
    fn the_key_file_is_0600_and_never_overwritten() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_path(dir.path());
        write_key_file(&path, &some_keys()).expect("write");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "written 0600");
        let read = read_key_file(&path).expect("read").expect("present");
        assert_eq!(read.hostname.as_deref(), Some("tcr-studio"));
        assert_eq!(read.key_state, serde_json::json!({ "fake": "keys" }));

        write_key_file(&path, &some_keys()).expect_err("an existing key file is never replaced");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let refusal = read_key_file(&path)
            .expect_err("a 0644 key file is refused")
            .to_string();
        assert!(refusal.contains("644"), "{refusal}");

        assert!(remove_key_file(&path).expect("delete"));
        assert!(read_key_file(&path).expect("read").is_none());
        assert!(!remove_key_file(&path).expect("delete again"));
    }

    /// **The auth key never prints.**
    #[test]
    fn an_auth_key_does_not_print() {
        let key = AuthKey::new("tskey-auth-fake-1111\n").expect("a key");
        assert_eq!(format!("{key:?}"), "AuthKey(..)");
        assert_eq!(key.into_secret(), "tskey-auth-fake-1111");
        AuthKey::new("  \n").expect_err("an empty key is refused");
    }

    /// **A device that stops answering is shut down and a new one is built
    /// from the same keys.**
    ///
    /// Watch it fail by making `check_health` return `true` without asking
    /// the device: the second build never happens and `builds` stays 1.
    #[tokio::test]
    async fn a_device_that_fails_its_health_check_is_rebuilt() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let script = Script::new();
        let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());

        sup.tick(Duration::ZERO, 1_000).await;
        assert!(sup.is_running());
        assert_eq!(script.builds.load(Ordering::SeqCst), 1);
        let ready = held(&fx.paths);
        assert_eq!(ready.state, TailnetState::Ready);
        assert_eq!(ready.address, Some(Ipv4Addr::new(100, 64, 0, 7)));
        assert_eq!(ready.node_name.as_deref(), Some("tcr-studio"));

        // Healthy at the health wake: nothing is rebuilt.
        sup.tick(HEALTH_EVERY, 2_000).await;
        assert_eq!(script.builds.load(Ordering::SeqCst), 1);

        // Dead (its next self_node hangs), but not yet a health wake: left
        // alone, and nothing has asked it yet.
        script.hangs.store(1, Ordering::SeqCst);
        sup.tick(TICK, 3_000).await;
        assert_eq!(script.builds.load(Ordering::SeqCst), 1);
        assert_eq!(script.hangs.load(Ordering::SeqCst), 1);

        // The health wake: the check times out, the device is shut down and
        // a new one is built, which answers.
        sup.tick(HEALTH_EVERY, 4_000).await;
        assert_eq!(
            script.hangs.load(Ordering::SeqCst),
            0,
            "the check asked the device"
        );
        assert_eq!(
            script.shutdowns.load(Ordering::SeqCst),
            1,
            "the old device was shut down"
        );
        assert_eq!(
            script.builds.load(Ordering::SeqCst),
            2,
            "a new one was built"
        );
        assert!(sup.is_running());
        assert_eq!(held(&fx.paths).state, TailnetState::Ready);
    }

    /// **A move to another network rebuilds the device at the next health
    /// wake, even though the device still answers.**
    ///
    /// Watch it fail by dropping the `moved` branch in `check_health`.
    #[tokio::test]
    async fn another_network_rebuilds_the_device() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let script = Script::new();
        let router = Arc::new(Mutex::new(Some(Ipv4Addr::new(192, 168, 1, 1))));
        let read = Arc::clone(&router);
        let gateway: GatewayFn = Arc::new(move || match read.lock() {
            Ok(held) => *held,
            Err(poisoned) => *poisoned.into_inner(),
        });
        let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, gateway);

        sup.tick(Duration::ZERO, 1_000).await;
        sup.tick(HEALTH_EVERY, 2_000).await;
        assert_eq!(
            script.builds.load(Ordering::SeqCst),
            1,
            "same network, no rebuild"
        );

        match router.lock() {
            Ok(mut held) => *held = Some(Ipv4Addr::new(10, 0, 0, 1)),
            Err(poisoned) => *poisoned.into_inner() = Some(Ipv4Addr::new(10, 0, 0, 1)),
        }
        sup.tick(TICK, 3_000).await;
        assert_eq!(
            script.builds.load(Ordering::SeqCst),
            1,
            "not a health wake yet"
        );
        sup.tick(HEALTH_EVERY, 4_000).await;
        assert_eq!(script.shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(script.builds.load(Ordering::SeqCst), 2);
    }

    /// **The record names each node state**: not joined without a key file,
    /// connecting while a build fails, not joined when control refuses the
    /// keys, ready on the tailnet, and off once the mode is.
    ///
    /// Watch it fail by recording `NodeConnecting` for a missing key file.
    #[tokio::test]
    async fn the_record_names_each_node_state() {
        let fx = fixture(TailnetMode::Node);
        let script = Script::new();
        let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());

        sup.tick(Duration::ZERO, 1_000).await;
        let record = held(&fx.paths);
        assert_eq!(record.mode.effective(), TailnetMode::Node);
        assert_eq!(record.state, TailnetState::NodeNotJoined);
        assert_eq!(
            script.builds.load(Ordering::SeqCst),
            0,
            "no keys, no device"
        );

        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        script.build_fails.store(true, Ordering::SeqCst);
        sup.tick(TICK, 2_000).await;
        let record = held(&fx.paths);
        assert_eq!(record.state, TailnetState::NodeConnecting);
        assert!(
            record
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("no network")),
            "{record:?}"
        );
        // Backed off: the next wake does not build again.
        sup.tick(TICK, 3_000).await;
        assert_eq!(script.builds.load(Ordering::SeqCst), 1);

        script.build_fails.store(false, Ordering::SeqCst);
        script.authorized.store(false, Ordering::SeqCst);
        sup.tick(RETRY_AFTER_FAILURE, 4_000).await;
        let record = held(&fx.paths);
        assert_eq!(record.state, TailnetState::NodeNotJoined, "{record:?}");
        assert_eq!(
            script.shutdowns.load(Ordering::SeqCst),
            1,
            "the refused device is shut down"
        );

        script.authorized.store(true, Ordering::SeqCst);
        // The second failure in a row: the refusal's 300 s rung doubled, at the cap.
        sup.tick(RETRY_AFTER_REFUSAL, 5_000).await;
        assert_eq!(
            held(&fx.paths).state,
            TailnetState::NodeNotJoined,
            "still backing off"
        );
        sup.tick(RETRY_CAP.saturating_sub(RETRY_AFTER_REFUSAL), 5_000)
            .await;
        let record = held(&fx.paths);
        assert_eq!(record.state, TailnetState::Ready);
        assert_eq!(record.recorded_at_ms, 5_000);

        set_mode(&fx.paths, TailnetMode::Off);
        sup.tick(TICK, 6_000).await;
        assert!(!sup.is_running());
        assert_eq!(script.shutdowns.load(Ordering::SeqCst), 2);
        let record = held(&fx.paths);
        assert_eq!(
            (record.mode, record.state),
            (TailnetMode::Off.into(), TailnetState::NodeReleased)
        );
    }

    /// **A Mac that never used node mode never has this supervisor write the
    /// state file**, in any build: off and app records are the browse
    /// tick's.
    ///
    /// Watch it fail by dropping `was_node &&` from the off branch: the first
    /// wake then writes an `off` record.
    #[tokio::test]
    async fn outside_node_mode_nothing_is_written() {
        for mode in [TailnetMode::Off, TailnetMode::App] {
            let fx = fixture(mode);
            let mut sup = supervisor(FakeFactory(Script::new()), &fx.paths, fixed_gateway());
            sup.tick(Duration::ZERO, 1_000).await;
            sup.tick(TICK, 2_000).await;
            assert!(!fx.paths.state.exists(), "{mode:?} wrote the state file");
            let mut unsupported = supervisor(Unsupported, &fx.paths, fixed_gateway());
            unsupported.tick(Duration::ZERO, 3_000).await;
            assert!(!fx.paths.state.exists(), "{mode:?} wrote the state file");
        }
    }

    /// **A binary without the library records `nodeUnsupported`** in node
    /// mode and builds nothing.
    #[tokio::test]
    async fn a_build_without_the_library_records_unsupported() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let mut sup = supervisor(Unsupported, &fx.paths, fixed_gateway());
        sup.tick(Duration::ZERO, 1_000).await;
        assert!(!sup.is_running());
        assert_eq!(held(&fx.paths).state, TailnetState::NodeUnsupported);
    }

    /// **The node's accept loop hands each connection to the same serve
    /// function, with the node's addresses, and counts it while it is open.**
    #[tokio::test]
    async fn accepted_connections_reach_serve_with_their_addresses() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let script = Script::new();
        let (tx, rx) = mpsc::channel(1);
        match script.incoming.lock() {
            Ok(mut held) => *held = Some(rx),
            Err(poisoned) => *poisoned.into_inner() = Some(rx),
        }
        let (seen_tx, mut seen_rx) = mpsc::channel::<(SocketAddr, SocketAddr)>(1);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release = Arc::new(Mutex::new(Some(release_rx)));
        let serve: ServeFn<DuplexStream> = Arc::new(move |accepted| {
            let seen_tx = seen_tx.clone();
            let release = match release.lock() {
                Ok(mut held) => held.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            Box::pin(async move {
                seen_tx
                    .send((accepted.from, accepted.bind))
                    .await
                    .expect("the test listens");
                if let Some(release) = release {
                    release.await.expect("released");
                }
            })
        });
        let mut sup = Supervisor::new(
            FakeFactory(script.clone()),
            fx.paths.clone(),
            7755,
            serve,
            fixed_gateway(),
        );
        sup.tick(Duration::ZERO, 1_000).await;
        let live = sup.live();

        let from: SocketAddr = "100.64.0.9:50123".parse().expect("addr");
        let bind: SocketAddr = "100.64.0.7:7755".parse().expect("addr");
        let (stream, _far) = tokio::io::duplex(64);
        tx.send(Accepted { stream, from, bind })
            .await
            .expect("accept");
        let got = seen_rx.recv().await.expect("served");
        assert_eq!(got, (from, bind));
        assert_eq!(live.distinct(), 1, "counted while open");
        release_tx.send(()).expect("release");
        for _ in 0..100 {
            if live.distinct() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(live.distinct(), 0, "uncounted once it ends");
    }

    /// **A join writes the key file only after control accepts the auth key,
    /// and the mode only after that; a refused key leaves both alone.**
    ///
    /// Watch it fail by moving the mode write above the authorization check:
    /// the refused join then leaves `tailnet: "node"` behind.
    #[tokio::test]
    async fn a_join_saves_keys_then_mode_and_a_refusal_saves_nothing() {
        use std::os::unix::fs::PermissionsExt as _;

        let fx = fixture(TailnetMode::Off);
        let script = Script::new();
        let factory = FakeFactory(script.clone());

        script.authorized.store(false, Ordering::SeqCst);
        let refused = join_with(
            &factory,
            &fx.paths.peers,
            &fx.paths.key,
            Some("tcr-studio".to_string()),
            AuthKey::new("tskey-auth-fake-1111").expect("key"),
            Duration::from_secs(5),
        )
        .await
        .expect_err("a refused key is an error")
        .to_string();
        assert!(refused.contains("did not accept"), "{refused}");
        assert!(read_key_file(&fx.paths.key).expect("read").is_none());
        assert_eq!(
            crate::peer::config::read_or_default(&fx.paths.peers)
                .expect("read")
                .tailnet
                .effective(),
            TailnetMode::Off
        );
        assert_eq!(
            script.shutdowns.load(Ordering::SeqCst),
            1,
            "the device was shut down"
        );

        script.authorized.store(true, Ordering::SeqCst);
        let me = join_with(
            &factory,
            &fx.paths.peers,
            &fx.paths.key,
            Some("tcr-studio".to_string()),
            AuthKey::new("tskey-auth-fake-1111").expect("key"),
            Duration::from_secs(5),
        )
        .await
        .expect("joined");
        assert_eq!(me.address, Ipv4Addr::new(100, 64, 0, 7));
        assert_eq!(script.auth_keys_seen.load(Ordering::SeqCst), 2);
        assert_eq!(script.shutdowns.load(Ordering::SeqCst), 2);
        let keys = read_key_file(&fx.paths.key)
            .expect("read")
            .expect("written");
        assert_eq!(keys.hostname.as_deref(), Some("tcr-studio"));
        let mode = std::fs::metadata(&fx.paths.key)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            crate::peer::config::read_or_default(&fx.paths.peers)
                .expect("read")
                .tailnet
                .effective(),
            TailnetMode::Node
        );
        let raw = std::fs::read_to_string(&fx.paths.key).expect("raw");
        assert!(!raw.contains("tskey"), "the auth key is never written");

        join_with(
            &factory,
            &fx.paths.peers,
            &fx.paths.key,
            None,
            AuthKey::new("tskey-auth-fake-1111").expect("key"),
            Duration::from_secs(5),
        )
        .await
        .expect_err("a joined node is refused a second join");
    }

    /// **A binary without the library refuses to join.**
    #[tokio::test]
    async fn a_build_without_the_library_refuses_to_join() {
        let fx = fixture(TailnetMode::Off);
        let refusal = join_with(
            &Unsupported,
            &fx.paths.peers,
            &fx.paths.key,
            None,
            AuthKey::new("tskey-auth-fake-1111").expect("key"),
            Duration::from_secs(5),
        )
        .await
        .expect_err("refused")
        .to_string();
        assert!(refusal.contains("tailnet-node"), "{refusal}");
    }

    fn save_held(paths: &NodePaths, mode: TailnetMode, state: TailnetState) {
        crate::peer::state::save_tailnet(
            &paths.state,
            TailnetRecord {
                mode: mode.into(),
                state,
                address: None,
                node_name: None,
                found: 0,
                connected: 0,
                detail: None,
                recorded_at_ms: crate::now_ms(),
            },
            Vec::new(),
        )
        .expect("save the record");
    }

    /// **`leave` sets the mode off first, keeps the key file while the server
    /// still holds the node, is not fooled by another writer's `off` record,
    /// and deletes the keys once the supervisor writes `nodeReleased`.**
    ///
    /// Watch it fail two ways: delete the key file before the wait (the
    /// mid-wait assertion fails), or accept `mode == off` as released again
    /// (the keys go on the browse tick's `off` record).
    #[tokio::test]
    async fn leave_waits_for_the_server_before_deleting_the_keys() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        save_held(&fx.paths, TailnetMode::Node, TailnetState::Ready);

        let peers = fx.paths.peers.clone();
        let key = fx.paths.key.clone();
        let leaving = tokio::spawn(async move {
            leave_with(
                &peers,
                &key,
                Duration::from_secs(10),
                Duration::from_millis(20),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            crate::peer::config::read_or_default(&fx.paths.peers)
                .expect("read")
                .tailnet
                .effective(),
            TailnetMode::Off,
            "the mode goes off first"
        );
        assert!(
            read_key_file(&fx.paths.key).expect("read").is_some(),
            "the keys stay while the server holds the node"
        );
        assert!(!leaving.is_finished());

        // The browse tick's own `off` record: not the node letting go.
        save_held(&fx.paths, TailnetMode::Off, TailnetState::Off);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            read_key_file(&fx.paths.key).expect("read").is_some(),
            "an `off` record from another writer does not release the keys"
        );
        assert!(!leaving.is_finished());

        save_held(&fx.paths, TailnetMode::Off, TailnetState::NodeReleased);
        let outcome = leaving.await.expect("joined").expect("left");
        assert_eq!(outcome, LeaveOutcome::Deleted);
        assert!(read_key_file(&fx.paths.key).expect("read").is_none());
    }

    /// **A server that never lets go keeps the key file**, and with no server
    /// recording at all, `leave` does not wait.
    #[tokio::test]
    async fn leave_keeps_the_keys_when_the_server_does_not_let_go() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        save_held(&fx.paths, TailnetMode::Node, TailnetState::Ready);
        leave_with(
            &fx.paths.peers,
            &fx.paths.key,
            Duration::from_millis(100),
            Duration::from_millis(20),
        )
        .await
        .expect_err("the server still holds the node");
        assert!(read_key_file(&fx.paths.key).expect("read").is_some());

        let quiet = fixture(TailnetMode::Node);
        write_key_file(&quiet.paths.key, &some_keys()).expect("keys");
        let outcome = leave_with(
            &quiet.paths.peers,
            &quiet.paths.key,
            Duration::from_millis(100),
            Duration::from_millis(20),
        )
        .await
        .expect("no server, no wait");
        assert_eq!(outcome, LeaveOutcome::Deleted);
    }

    fn switchable_gateway() -> (Arc<Mutex<Option<Ipv4Addr>>>, GatewayFn) {
        let router = Arc::new(Mutex::new(Some(Ipv4Addr::new(192, 168, 1, 1))));
        let read = Arc::clone(&router);
        let gateway: GatewayFn = Arc::new(move || match read.lock() {
            Ok(held) => *held,
            Err(poisoned) => *poisoned.into_inner(),
        });
        (router, gateway)
    }

    fn move_to(router: &Arc<Mutex<Option<Ipv4Addr>>>, to: Ipv4Addr) {
        match router.lock() {
            Ok(mut held) => *held = Some(to),
            Err(poisoned) => *poisoned.into_inner() = Some(to),
        }
    }

    /// **No second device is built while the old one is still alive.** A
    /// caller holding the device past the shutdown deadline (a dial in
    /// flight) leaves it running; the rebuild waits until it is free, shuts
    /// it down, and only then builds from the same keys.
    ///
    /// Watch it fail by dropping the held device in `settle` instead of
    /// keeping it in `lingering`: the rebuild then happens with `shutdowns`
    /// still 0, two devices on one key.
    #[tokio::test]
    async fn a_rebuild_waits_for_the_old_device_to_stop() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let script = Script::new();
        let (router, gateway) = switchable_gateway();
        let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, gateway);
        sup.tick(Duration::ZERO, 1_000).await;
        let in_flight = sup.running_device().expect("running");

        move_to(&router, Ipv4Addr::new(10, 0, 0, 1));
        sup.tick(HEALTH_EVERY, 2_000).await;
        assert_eq!(script.shutdowns.load(Ordering::SeqCst), 0, "still held");
        assert_eq!(
            script.builds.load(Ordering::SeqCst),
            1,
            "no second device while the first is alive"
        );
        let record = held(&fx.paths);
        assert_eq!(record.state, TailnetState::NodeConnecting);
        assert!(
            record
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("still in use")),
            "{record:?}"
        );

        sup.tick(RETRY_AFTER_FAILURE, 3_000).await;
        assert_eq!(script.builds.load(Ordering::SeqCst), 1, "still held");

        drop(in_flight);
        sup.tick(TICK, 4_000).await;
        assert_eq!(
            script.shutdowns.load(Ordering::SeqCst),
            1,
            "shut down once free"
        );
        assert_eq!(
            script.builds.load(Ordering::SeqCst),
            2,
            "then the new one is built"
        );
        assert!(sup.is_running());
    }

    /// **The release record is written on any way out of `node`, and only
    /// once no device from the keys is alive**: leaving for `app` while a
    /// caller still holds the device writes nothing, and the release follows
    /// the shutdown.
    ///
    /// Watch it fail by writing the release whenever the mode leaves `node`
    /// (drop `free && built`): it is then written while the device lingers.
    #[tokio::test]
    async fn the_release_waits_for_the_device_and_covers_app_mode() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let script = Script::new();
        let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());
        sup.tick(Duration::ZERO, 1_000).await;
        let in_flight = sup.running_device().expect("running");

        set_mode(&fx.paths, TailnetMode::App);
        sup.tick(TICK, 2_000).await;
        assert_eq!(script.shutdowns.load(Ordering::SeqCst), 0);
        assert_eq!(
            held(&fx.paths).state,
            TailnetState::Ready,
            "nothing released while the device is alive"
        );

        drop(in_flight);
        sup.tick(TICK, 3_000).await;
        assert_eq!(script.shutdowns.load(Ordering::SeqCst), 1);
        let record = held(&fx.paths);
        assert_eq!(
            (record.mode, record.state, record.recorded_at_ms),
            (TailnetMode::App.into(), TailnetState::NodeReleased, 3_000)
        );
    }

    /// The ladder: 30 s, doubling, capped at ten minutes.
    #[test]
    fn the_retry_ladder_doubles_to_ten_minutes() {
        let waits: Vec<u64> = (1..=7)
            .map(|n| retry_wait(RETRY_AFTER_FAILURE, n).as_secs())
            .collect();
        assert_eq!(waits, vec![30, 60, 120, 240, 480, 600, 600]);
        assert_eq!(retry_wait(RETRY_AFTER_REFUSAL, 1).as_secs(), 300);
        assert_eq!(retry_wait(RETRY_AFTER_REFUSAL, 2).as_secs(), 600);
        assert_eq!(retry_wait(RETRY_AFTER_FAILURE, u32::MAX).as_secs(), 600);
    }

    /// **Failed starts back off 30 s, 60 s, 120 s; a device that passes a
    /// health check puts the ladder back at its first rung.**
    ///
    /// Watch it fail by making `retry_wait` return `first` every time: the
    /// build at 30 s into the second wait then happens.
    #[tokio::test]
    async fn failed_starts_back_off_and_a_healthy_device_resets_it() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let script = Script::new();
        let builds = || script.builds.load(Ordering::SeqCst);
        let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());

        script.build_fails.store(true, Ordering::SeqCst);
        sup.tick(Duration::ZERO, 1_000).await;
        assert_eq!(builds(), 1);
        sup.tick(Duration::from_secs(29), 2_000).await;
        assert_eq!(builds(), 1, "inside the first 30 s");
        sup.tick(Duration::from_secs(1), 3_000).await;
        assert_eq!(builds(), 2, "after 30 s");
        sup.tick(Duration::from_secs(30), 4_000).await;
        assert_eq!(builds(), 2, "the second wait is 60 s, not 30");
        sup.tick(Duration::from_secs(30), 5_000).await;
        assert_eq!(builds(), 3, "after 60 s");
        sup.tick(Duration::from_secs(119), 6_000).await;
        assert_eq!(builds(), 3, "the third wait is 120 s");

        script.build_fails.store(false, Ordering::SeqCst);
        sup.tick(Duration::from_secs(1), 7_000).await;
        assert_eq!(builds(), 4);
        assert!(sup.is_running());
        // A passed health check: the ladder is back at its first rung.
        sup.tick(HEALTH_EVERY, 8_000).await;
        // It dies; the first failure rebuilds at once, and that build fails.
        script.build_fails.store(true, Ordering::SeqCst);
        script.hangs.store(1, Ordering::SeqCst);
        sup.tick(HEALTH_EVERY, 9_000).await;
        assert_eq!(
            builds(),
            5,
            "the first failure after a healthy run rebuilds at once"
        );
        sup.tick(Duration::from_secs(59), 10_000).await;
        assert_eq!(builds(), 5);
        sup.tick(Duration::from_secs(1), 11_000).await;
        assert_eq!(
            builds(),
            6,
            "60 s, the second rung: the count restarted, it did not carry the old three"
        );
    }

    /// **A build that outlives its wait is kept and waited on again, never
    /// started a second time**: an offline Mac holds one half-started build,
    /// not one per retry. When it finishes, the next start uses it.
    ///
    /// Watch it fail by spawning a fresh build on every start (ignore
    /// `pending_build`): `builds` then reaches 2 while the first still runs.
    #[tokio::test]
    async fn a_slow_build_is_kept_not_repeated() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let script = Script::new();
        script.build_hangs.store(true, Ordering::SeqCst);
        let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway())
            .with_build_timeout(Duration::from_millis(50));

        sup.tick(Duration::ZERO, 1_000).await;
        assert!(!sup.is_running());
        assert_eq!(held(&fx.paths).state, TailnetState::NodeConnecting);
        sup.tick(RETRY_AFTER_FAILURE, 2_000).await;
        assert_eq!(
            script.builds.load(Ordering::SeqCst),
            1,
            "the second start waits on the first build"
        );

        script.release.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
        sup.tick(RETRY_CAP, 3_000).await;
        assert!(sup.is_running(), "the kept build became the node");
        assert_eq!(script.builds.load(Ordering::SeqCst), 1);
    }

    /// **Stopping the process does not wait out a start**: the stop is seen
    /// during a wake, and `run` returns within the shutdown deadline even
    /// with a build that never finishes.
    ///
    /// Watch it fail by awaiting the wake without watching `stop`: `run`
    /// then waits the 30 s build ceiling and the 2 s deadline here fails.
    #[tokio::test]
    async fn a_stop_during_a_start_is_not_kept_waiting() {
        let fx = fixture(TailnetMode::Node);
        write_key_file(&fx.paths.key, &some_keys()).expect("keys");
        let script = Script::new();
        script.build_hangs.store(true, Ordering::SeqCst);
        let sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let running = tokio::spawn(sup.run(Duration::from_millis(10), stop_rx));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            script.builds.load(Ordering::SeqCst),
            1,
            "a build is under way"
        );
        stop_tx.send(true).expect("stop");
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .expect("run returns without waiting out the build")
            .expect("joined");
    }

    /// **A join shuts its device down before the key file exists**, so a
    /// server already in node mode cannot start a second device from it
    /// while the CLI's is still up.
    ///
    /// Watch it fail by writing the key file before the shutdown: the
    /// shutdown then sees the file.
    #[tokio::test]
    async fn a_join_shuts_its_device_down_before_writing_the_keys() {
        let fx = fixture(TailnetMode::Off);
        let script = Script::new();
        match script.watch_key.lock() {
            Ok(mut held) => *held = Some(fx.paths.key.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(fx.paths.key.clone()),
        }
        join_with(
            &FakeFactory(script.clone()),
            &fx.paths.peers,
            &fx.paths.key,
            Some("tcr-studio".to_string()),
            AuthKey::new("tskey-auth-fake-1111").expect("key"),
            Duration::from_secs(5),
        )
        .await
        .expect("joined");
        assert_eq!(script.shutdowns.load(Ordering::SeqCst), 1);
        assert!(
            !script.key_existed_at_shutdown.load(Ordering::SeqCst),
            "the key file did not exist yet when the device was shut down"
        );
        assert!(read_key_file(&fx.paths.key).expect("read").is_some());
    }

    #[test]
    fn node_names_are_checked() {
        check_node_name("tcr-studio").expect("ok");
        check_node_name("-x").expect_err("leading hyphen");
        check_node_name("a b").expect_err("space");
        check_node_name("").expect_err("empty");
        check_node_name(&"a".repeat(64)).expect_err("too long");
    }
}
