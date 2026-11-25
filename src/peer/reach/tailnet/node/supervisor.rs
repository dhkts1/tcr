//! The supervisor: the server's half of node mode, one device kept alive.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::device::{
    Accepted, Authorization, DeviceFactory, NodeSelf, Shutdown, TailnetDevice, TailnetListener,
    bounded, shutdown_shared,
};
use super::dial::{DialSlot, Published, dialler};
use super::keys::{NodeKeyFile, read_key_file};
use super::live::LiveConnections;
use super::{
    BUILD_TIMEOUT, CONTROL_TIMEOUT, HEALTH_EVERY, RECORD_REFRESH, RETRY_AFTER_FAILURE,
    RETRY_AFTER_REFUSAL, SHUTDOWN_DEADLINE, retry_wait,
};
use crate::peer::config::TailnetMode;
use crate::peer::state::{TailnetRecord, TailnetState};

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
    /// Where the running device's dialler is published, if anywhere.
    dial_slot: Option<DialSlot>,
    /// Flipped when the running device stops, to cancel its dials in flight.
    cancel_dials: Option<tokio::sync::watch::Sender<bool>>,
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
            dial_slot: None,
            cancel_dials: None,
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

    /// Publish the running device's dialler in `slot` (the server passes
    /// [`server_slot`](crate::peer::reach::tailnet::node::server_slot)), so the process's peer dials and the CLI's dial route
    /// reach the tailnet through it.
    pub fn with_dial_slot(mut self, slot: DialSlot) -> Self {
        self.dial_slot = Some(slot);
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
    pub(super) fn running_device(&self) -> Option<Arc<F::Device>> {
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
            Ok(file) => file.tailnet_mode(),
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
        if let Some(slot) = &self.dial_slot {
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            self.cancel_dials = Some(cancel);
            slot.set(Some(Published {
                dial: dialler(&device, &self.live, cancelled),
                listen: SocketAddr::from((me.address, self.port)),
                peers: self.paths.peers.clone(),
            }));
        }
        self.running = Some(Running { device, accept, me });
        self.backoff = None;
        self.since_health = Duration::ZERO;
        // The network this device started on, so the first health wake can
        // tell a move from no move.
        self.last_gateway = Some(self.read_gateway().await);
        self.record_ready(now_ms);
    }

    /// Record a failure and back off: `first` for the first failure in a
    /// row, doubling to [`RETRY_CAP`](crate::peer::reach::tailnet::node::RETRY_CAP).
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
        // Unpublished first, so no new dial starts on a device being shut
        // down, then every dial in flight is cancelled, so none holds the
        // device past `settle`'s wait.
        if let Some(slot) = &self.dial_slot {
            slot.set(None);
        }
        if let Some(cancel) = self.cancel_dials.take()
            && cancel.send(true).is_err()
        {
            tracing::debug!("peer tailnet node: no dial was listening for the stop");
        }
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
