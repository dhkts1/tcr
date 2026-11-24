//! The device, behind a trait, and the factory a build without the library
//! has.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncWrite};

use super::keys::{AuthKey, NodeKeyFile};

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

    /// Open a TCP connection to `addr` through the tailnet. Waits for
    /// control (the library asks for its own address first); every caller
    /// bounds it.
    fn connect(&self, addr: SocketAddr) -> impl Future<Output = Result<Self::Stream>> + Send;
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
pub(super) async fn bounded<T>(
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
pub(super) enum Shutdown<D> {
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
pub(super) async fn shutdown_shared<D: TailnetDevice>(
    device: Arc<D>,
    deadline: Duration,
) -> Shutdown<D> {
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

    async fn connect(&self, _addr: SocketAddr) -> Result<Self::Stream> {
        match *self {}
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
