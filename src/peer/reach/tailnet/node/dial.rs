//! Dialling through the node: the slot a running node publishes its dialler
//! in, the dial route's dial, and the CLI's dial through the running tcr.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncWrite};

use super::device::{TailnetDevice, bounded};
use super::live::{LiveConnections, LiveGuard};
use crate::peer::config::TailnetMode;
use crate::peer::state::TailnetState;

// ---------------------------------------------------------------------------
// Dialling through the node
// ---------------------------------------------------------------------------

/// How long one dial through the node may take, connect included.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// One dial through the running node, type-erased so the proxy's route and
/// the peer dialler need not know the device type.
pub type DialFn = Arc<
    dyn Fn(
            SocketAddr,
        ) -> Pin<Box<dyn Future<Output = Result<crate::peer::noise::PeerStream>> + Send>>
        + Send
        + Sync,
>;

/// Where a running node's dialler is published for the rest of the process.
///
/// Set by the [`Supervisor`](crate::peer::reach::tailnet::node::Supervisor) when a device is on the tailnet and cleared
/// before it is shut down. The dialler holds the device weakly, so a
/// published dialler never keeps a device alive past its shutdown.
#[derive(Clone, Default)]
pub struct DialSlot(Arc<Mutex<Option<Published>>>);

/// What a running node publishes: how to dial through it, and the socket it
/// listens on (its address at the peer port), which the `Hello` advertises.
#[derive(Clone)]
pub struct Published {
    /// One dial through the node.
    pub dial: DialFn,
    /// The node's own listening socket.
    pub listen: SocketAddr,
    /// The peers file this node serves, whose rows decide which ports the
    /// dial route may reach (`dial_for_route`).
    pub peers: PathBuf,
}

impl DialSlot {
    /// Publish a running node, or clear the slot with `None`.
    pub fn set(&self, published: Option<Published>) {
        match self.0.lock() {
            Ok(mut held) => *held = published,
            Err(poisoned) => *poisoned.into_inner() = published,
        }
    }

    /// The dialler, when a node is running.
    pub fn get(&self) -> Option<DialFn> {
        self.published().map(|published| published.dial)
    }

    /// The node's listening socket, when a node is running.
    pub fn listen(&self) -> Option<SocketAddr> {
        self.published().map(|published| published.listen)
    }

    /// What the running node published, when one is running.
    pub fn published(&self) -> Option<Published> {
        match self.0.lock() {
            Ok(held) => held.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// The socket this process's node listens on, for the `Hello`
/// (`crate::peer::serve::NodeFacts::listening`): `None` when this process
/// runs no node.
pub fn advertised_listen() -> Option<SocketAddr> {
    server_slot().listen()
}

/// The serving process's slot: the one [`run_in_server`](crate::peer::listener::run_in_server) hands its
/// supervisor, and the one [`dial_through_node`] reads.
pub fn server_slot() -> &'static DialSlot {
    static SLOT: std::sync::OnceLock<DialSlot> = std::sync::OnceLock::new();
    SLOT.get_or_init(DialSlot::default)
}

/// Whether `ip` is a tailnet address: `100.64.0.0/10` or
/// `fd7a:115c:a1e0::/48`, the ranges Tailscale numbers a tailnet out of.
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => crate::peer::reach::is_carrier_grade_nat(v4),
        IpAddr::V6(v6) => crate::peer::reach::is_tailscale_v6(v6),
    }
}

/// A stream counted in [`LiveConnections`] for as long as it is open.
struct Counted<S> {
    inner: S,
    _live: LiveGuard,
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A dialler over `device`, held weakly, counting what it opens in `live`.
///
/// A dial holds the device only while its connect runs, and `cancel` ends
/// that connect the moment the supervisor stops the device: an in-flight dial
/// cannot keep a device alive past its shutdown and into a rebuild from the
/// same keys.
pub(super) fn dialler<D: TailnetDevice>(
    device: &Arc<D>,
    live: &Arc<LiveConnections>,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> DialFn {
    let device = Arc::downgrade(device);
    let live = Arc::clone(live);
    Arc::new(move |addr| {
        let device = std::sync::Weak::clone(&device);
        let live = Arc::clone(&live);
        let mut cancel = cancel.clone();
        Box::pin(async move {
            if *cancel.borrow() {
                bail!("tcr's tailnet node is stopping, so {addr} was not dialled");
            }
            let Some(device) = device.upgrade() else {
                bail!("tcr's tailnet node stopped before {addr} could be dialled");
            };
            let stream = tokio::select! {
                connected = device.connect(addr) => connected?,
                _ = cancel.wait_for(|stopping| *stopping) => {
                    bail!("tcr's tailnet node stopped while {addr} was being dialled")
                }
            };
            drop(device);
            let counted = Counted {
                inner: stream,
                _live: live.enter(addr.ip()),
            };
            Ok(Box::new(counted) as crate::peer::noise::PeerStream)
        })
    })
}

/// Dial `addr` through this process's node, when there is one to dial
/// through: `None` when this process runs no node or `addr` is not a tailnet
/// address, so the caller dials through the kernel as before.
///
/// The dialler picks the transport, not the endpoint: the same pinned
/// `100.x` row is dialled through the kernel on a Mac with the Tailscale app
/// and through the node here.
pub async fn dial_through_node(
    addr: SocketAddr,
    within: Duration,
) -> Option<Result<crate::peer::noise::PeerStream>> {
    dial_in_slot(server_slot(), addr, within).await
}

/// [`dial_through_node`] against an explicit slot, for a test.
pub async fn dial_in_slot(
    slot: &DialSlot,
    addr: SocketAddr,
    within: Duration,
) -> Option<Result<crate::peer::noise::PeerStream>> {
    if !is_tailnet_ip(addr.ip()) {
        return None;
    }
    let dial = slot.get()?;
    Some(bounded(within, "a dial through the tailnet node", dial(addr)).await)
}

/// The peer port every Mac listens on unless its peers file says otherwise
/// (`crate::peer::config::default_listen`).
pub fn default_peer_port() -> u16 {
    crate::peer::config::default_listen().port()
}

/// The ports `/_tcr/peer/dial` may reach through the node: the default peer
/// port, this Mac's own configured `listen` port, and every port in a direct
/// endpoint of any row in `file`.
///
/// Gil, 2026-10-20: the route connects only to a peer port. In node mode tcr
/// is this Mac's only presence on the tailnet, so a route that dialled any
/// port would hand every local process this machine's reach to every service
/// the tailnet's ACLs let it see. Pending invites carry no address, so they
/// add no port.
pub fn allowed_dial_ports(file: &crate::peer::config::PeerFile) -> std::collections::BTreeSet<u16> {
    let mut ports = std::collections::BTreeSet::from([default_peer_port()]);
    if let Some(listen) = file.listen {
        ports.insert(listen.port());
    }
    for addr in file.direct_addrs() {
        ports.insert(addr.port());
    }
    ports
}

/// What `/_tcr/peer/dial` got when it asked to dial.
pub enum RouteDial {
    /// No node runs in this process.
    NoNode,
    /// The target is not a tailnet address.
    NotTailnet,
    /// The target's port is not a peer port; nothing was dialled. The
    /// sentence says which port and why.
    PortRefused(String),
    /// The dial was tried and failed.
    Failed(anyhow::Error),
    /// The stream to the target.
    Dialled(crate::peer::noise::PeerStream),
}

/// The dial route's dial: through this process's node, to a tailnet address
/// at a peer port ([`allowed_dial_ports`]) only. The port is checked against
/// the node's own peers file BEFORE the node is asked for anything.
pub async fn dial_for_route(target: SocketAddr, within: Duration) -> RouteDial {
    dial_for_route_in(server_slot(), target, within).await
}

/// [`dial_for_route`] against an explicit slot, for a test.
pub async fn dial_for_route_in(slot: &DialSlot, target: SocketAddr, within: Duration) -> RouteDial {
    let Some(published) = slot.published() else {
        return RouteDial::NoNode;
    };
    if !is_tailnet_ip(target.ip()) {
        return RouteDial::NotTailnet;
    }
    let file = match crate::peer::config::read_or_default(&published.peers) {
        Ok(file) => file,
        Err(err) => {
            return RouteDial::Failed(err.context(
                "the peers file did not read, so the peer ports are unknown and nothing was dialled",
            ));
        }
    };
    let allowed = allowed_dial_ports(&file);
    if !allowed.contains(&target.port()) {
        let list = allowed
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return RouteDial::PortRefused(format!(
            "port {} is not a peer port, so tcr's tailnet node will not dial {target}; this \
             route reaches only peer ports ({list}): the default, this Mac's own listen port, \
             and the ports of the rows in the peers file",
            target.port()
        ));
    }
    match bounded(
        within,
        "a dial through the tailnet node",
        (published.dial)(target),
    )
    .await
    {
        Ok(stream) => RouteDial::Dialled(stream),
        Err(err) => RouteDial::Failed(err),
    }
}
/// This Mac's node address, as the serving process recorded it: a fresh
/// `ready` record in node mode. What `tcr peer invite` (a separate process,
/// with no node of its own) puts first in a key, the way it reads the router
/// mapping off the same file.
pub fn recorded_node_address(state_path: &Path, now_ms: i64) -> Option<Ipv4Addr> {
    let record = crate::peer::state::load(state_path, now_ms).ok()?.tailnet?;
    (record.mode.effective() == TailnetMode::Node
        && record.state == TailnetState::Ready
        && !record.is_stale(now_ms))
    .then_some(record.address)
    .flatten()
}

/// Read an HTTP/1.1 response head off `stream`, one byte at a time so not a
/// byte of what follows a `101` is taken. At most 8 KiB.
async fn read_response_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<String> {
    use tokio::io::AsyncReadExt as _;

    let mut head = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= 8 * 1024 {
            bail!("the running tcr's answer had no end to its head within 8 KiB");
        }
        let read = stream
            .read(&mut byte)
            .await
            .context("reading the running tcr's answer")?;
        if read == 0 {
            bail!("the running tcr closed the connection without answering");
        }
        head.push(byte[0]);
    }
    String::from_utf8(head).context("the running tcr's answer was not text")
}

/// Ask the running tcr on `127.0.0.1:port` to dial `target` through its node,
/// and return the connection it splices to it.
///
/// One `POST /_tcr/peer/dial` with `Upgrade: tcr-peer-dial` and the target
/// in a header, the api key on it when one is set (no local route has a
/// loopback exemption), and a `101` back. After the `101` the socket is a
/// byte stream to `target`; anything else is the route's refusal, read and
/// named.
pub async fn dial_through_server(
    port: u16,
    api_key: Option<&str>,
    target: SocketAddr,
    within: Duration,
) -> Result<tokio::net::TcpStream> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = bounded(within, "the running tcr", async {
        tokio::net::TcpStream::connect(local)
            .await
            .with_context(|| format!("no tcr answered on {local} to dial through its node"))
    })
    .await?;
    let mut request = format!(
        "POST {} HTTP/1.1\r\nHost: {local}\r\nConnection: Upgrade\r\nUpgrade: {}\r\n{}: \
         {target}\r\nContent-Length: 0\r\n",
        crate::proxy::PEER_DIAL_PATH,
        crate::proxy::PEER_DIAL_UPGRADE,
        crate::proxy::PEER_DIAL_TARGET_HEADER,
    );
    if let Some(key) = api_key {
        request.push_str("x-api-key: ");
        request.push_str(key);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .context("asking the running tcr to dial")?;

    let head = bounded(
        within,
        "the running tcr's dial",
        read_response_head(&mut stream),
    )
    .await?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("the running tcr's answer had no status"))?;
    if status == 101 {
        // Only the dial route answers this upgrade, and it stamps the answer:
        // a `101` without the stamp is something else on that port.
        let stamped = head.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.trim()
                    .eq_ignore_ascii_case(crate::proxy::ENDPOINT_HEADER)
                    && value.trim() == crate::proxy::PEER_DIAL_ENDPOINT
            })
        });
        if !stamped {
            bail!(
                "something on {local} answered the dial with a 101 but is not tcr's dial route; \
                 is the configured port another program's?"
            );
        }
        return Ok(stream);
    }
    // The refusal's body says why; read what it promises, bounded.
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
        .min(16 * 1024);
    let mut body = vec![0_u8; length];
    let said = match tokio::time::timeout(within, stream.read_exact(&mut body)).await {
        Ok(Ok(_)) => String::from_utf8_lossy(&body).into_owned(),
        _ => String::new(),
    };
    bail!("the running tcr would not dial {target} through its tailnet node ({status}): {said}")
}

/// The CLI's dial to a peer: through the running tcr's node when this Mac is
/// in node mode and `addr` is a tailnet address, through the kernel
/// otherwise.
///
/// The knock, the first pairing and the join run in the CLI process, and the
/// node lives in the server, so in node mode the CLI asks the server to dial
/// ([`dial_through_server`]) and runs its Noise handshake over what comes
/// back. On a Mac with the Tailscale app, the same `100.x` address goes
/// through the kernel, as it always did.
pub async fn dial_for_cli(
    peers_path: &Path,
    addr: SocketAddr,
) -> Result<crate::peer::noise::PeerStream> {
    if is_tailnet_ip(addr.ip())
        && crate::peer::config::read_or_default(peers_path)?.tailnet_mode() == TailnetMode::Node
    {
        let config_path = crate::config::default_path();
        let config = crate::config::load(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        let stream = dial_through_server(
            config.proxy.port,
            config.loopback_api_key(),
            addr,
            DIAL_TIMEOUT,
        )
        .await?;
        return Ok(Box::new(stream));
    }
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    Ok(Box::new(stream))
}

/// Splice the CLI's upgraded connection and the node stream, until EITHER
/// side ends, then close both.
///
/// Not `copy_bidirectional`, which half-closes the side that did not end and
/// waits for it: the node stream cannot carry a half-close
/// (`poll_shutdown` does nothing at 0.6.1), so a CLI that hung up would leave
/// the far Mac's socket open until its own idle limit. None of the flows that
/// use this route (knock, first pairing, join) needs a half-close: each ends
/// by closing.
pub async fn splice_until_either_ends<A, B>(a: A, b: B) -> (u64, u64)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    let mut up = 0_u64;
    let mut down = 0_u64;
    let forward = async {
        let copied = tokio::io::copy(&mut a_read, &mut b_write).await;
        match copied {
            Ok(n) => up = n,
            Err(err) => {
                tracing::debug!(error = %err, "peer dial: the CLI-to-node half ended on an error");
            }
        }
    };
    let back = async {
        let copied = tokio::io::copy(&mut b_read, &mut a_write).await;
        match copied {
            Ok(n) => down = n,
            Err(err) => {
                tracing::debug!(error = %err, "peer dial: the node-to-CLI half ended on an error");
            }
        }
    };
    tokio::select! {
        () = forward => {}
        () = back => {}
    }
    (up, down)
}
