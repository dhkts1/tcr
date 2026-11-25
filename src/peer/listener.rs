//! The peer listener: a SECOND socket, with its own authorization function.
//!
//! # This socket does not reuse `local_endpoint_gate`, and that is deliberate
//!
//! `local_endpoint_gate` (`src/proxy.rs:1137`) authorizes the LOCAL `/_tcr/`
//! endpoints, and its own doc-comment says the thing that matters here: bind
//! scope is not authorization, because `127.0.0.1` is reachable by every
//! process and every container on this host. Behind it sits the one privileged
//! local mutation that ADDS A LIVE CREDENTIAL, whose doc says the gate plus a
//! content type is its entire authorization.
//!
//! So: a peer frame must never reach it. The peer listener is its own
//! `TcpListener`, bound where `tcr-peers.json` says and absent
//! entirely when it says nothing, and it has its own gate
//! ([`peer_stream_gate`]). `local_endpoint_gate` is not relaxed by one
//! character, and **neither arm of the existing listener is widened**, the
//! serving socket has two origins, an inherited descriptor
//! (`src/server.rs:1133-1143`) and the loopback bind below it, and a peer
//! listener bolted onto either would inherit a gate written for loopback.
//!
//! One implementation of the gate, for the same reason `local_endpoint_gate`'s
//! own doc gives for being one: two copies drift, and the copy that drifts is
//! the one on the route added later.
//!
//! # Speaks Noise, and nothing else
//!
//! The first bytes of every connection are one length-prefixed Noise message 1
//! of exactly the length its pattern fixes ([`crate::peer::noise`] pins both
//! numbers against `snow` in a test, rather than taking them from the spec).
//! Anything else closes the socket **with nothing written**: no banner, no
//! version string, no error frame. A stranger scanning this port learns that
//! something accepted a TCP connection and closed it.
//!
//! An `XX` message 1 is the one shape that is a well-formed Noise message and
//! still gets that silence by default: it has no prior key to check, so
//! answering it would hand this node's static key to whoever asked. It is
//! answered only inside a pairing window the operator opened
//! ([`crate::peer::pair::open_pairing_window`]), and the log a refused
//! connection writes is bounded to one line ([`RefusalLog`]), because a line
//! per connection is a disk-filling primitive handed to anyone who can reach
//! the port.
//!
//! # The policy half is re-read per connection
//!
//! `tcr-peers.json`'s socket half is boot-time and its policy half is
//! hot ([`crate::peer::config`]). This file takes the strictest reading of
//! "hot": the pinned rows and the outstanding invites are read from the file
//! for every connection and re-checked **before every frame**, which is what
//! makes `tcr peer forget` close a live session within one frame instead of at
//! the next handshake.
//!
//! # What this file decides about a SERVE, which is nothing
//!
//! [`serve_stream`] dispatches `StreamKind::Serve` to
//! [`crate::peer::serve::handle_serve_on`] and holds the three things a
//! listener cannot derive ([`LeaseServing`]: the ledger, this node's own proxy
//! base, a reader for its own quota). Every decision about the request, the
//! path refusal, the method, the lease, the debit, which account serves, is in
//! `src/peer/serve.rs`, and this file's only contribution is the gate that
//! already ran above.
//!
//! A build with no [`LeaseServing`] REFUSES a SERVE rather than accepting a
//! stream it cannot answer, which is the same shape every ungranted kind has.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tcr_peer_wire::{InstanceId, PeerId, StreamHeader, StreamKind};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;

use crate::peer::config::{PeerFile, PeerRow, PeerStore};
use crate::peer::id::KEY_BYTES;
use crate::peer::id::NodeKey;
use crate::peer::lease::Ledger;
use crate::peer::noise::{self, Handshake};
use crate::peer::reach::is_lan_scope;
use crate::peer::serve::{self, LeaseServing};
use crate::peer::state::knock_address;

mod admission;
mod connection;
mod gate;
mod knock;
mod pairing;
mod scope;

pub use admission::{
    ACCEPTED_WINDOW_SECS, Admission, HeaderlessSession, KNOCK_BURST, KNOCK_INTERVAL_MS,
    MAX_HEADERLESS_PER_PEER, MAX_KNOCK_FRAME_BYTES, MAX_UNAUTHENTICATED_PER_ADDRESS,
    MAX_UNAUTHENTICATED_SOCKETS, MESSAGE_1_TIMEOUT, PunchInFlight, REFUSAL_LOG_ADDRESSES,
    REFUSAL_LOG_QUIET_MS, RefusalLog, STREAM_HEADER_TIMEOUT, unauthenticated_allowance,
};
pub use connection::serve_accepted;
pub use gate::{
    RequestDedup, StreamRefusal, handoff_is_permitted, peer_stream_gate, peer_stream_gate_hop,
    peer_stream_gate_rows,
};
pub use scope::{
    InternetAdmission, internet_admission, own_mac_admission, shares_a_global_v6_prefix,
};

use admission::lock_admission;
use connection::serve_connection;

/// How long a peer has to complete a handshake before the socket is closed.
/// A LAN round trip is milliseconds; this is a bound on a stalled connection,
/// not a latency budget.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind the peer listener and serve it until shutdown.
///
/// Returns immediately with `Ok(None)` when no listen address is configured,
/// which is the default: a fresh install opens no port. That is the whole of
/// the feature flag, there is no second switch to forget.
///
/// The `Ok(None)` arm is unreachable through this signature, which takes a
/// concrete [`SocketAddr`]: the caller that reads `listen: Option<SocketAddr>`
/// is the one that can answer "not configured". Reported to the lead rather
/// than re-signed here.
pub async fn serve(
    addr: SocketAddr,
    node: &NodeKey,
    store: &PeerStore,
    serving: Option<LeaseServing>,
) -> Result<Option<SocketAddr>> {
    let listener = bind(addr).await?;
    let local = listener
        .local_addr()
        .context("peer listener: the bound socket has no address")?;
    tracing::info!(
        peer_listen = %local,
        peers_file = %store.path().display(),
        "peer listener up (a second socket; the local /_tcr/ gate is untouched)"
    );
    // [`internet_admission`] decides, per connection, off the SOCKET this
    // listener is bound to, not off `peer.internet`, so a globally routable
    // bind is worth one line at boot even with the switch off: this line is
    // the only place an operator who typed `listen: 0.0.0.0:7755` learns that
    // a stranger's knock reaches this process at all.
    if !is_lan_scope(local.ip()) {
        tracing::warn!(
            peer_listen = %local,
            "peer listener: bound to a globally routable address; a knock or a first pairing \
             from off the LAN gets nothing back, only a return visit against a pinned key or an \
             enrolment reaches the pin check"
        );
    }

    // `peer.internet`, through the one function every serving process calls
    // for it ([`crate::peer::reach::start_peer_mapping`]): `server.rs` boots
    // through `bind` + `serve_on_with` rather than through here, so a mapping
    // started inline in this function was a mapping the shipped proxy never
    // started.
    //
    // The guard is held across the serve below so that a shutdown that DROPS
    // this future still takes the router mapping away: see
    // [`crate::peer::reach::MappingGuard`].
    let _mapping =
        crate::peer::reach::start_peer_mapping(store.path(), store.file().internet, local);

    serve_on(listener, node, store, serving).await?;
    Ok(Some(local))
}

/// Bind the peer socket, separately from serving it, so a test can learn the
/// kernel-assigned port before the first connection arrives.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("peer listener: could not bind {addr}"))
}

/// Accept connections until the listener fails.
///
/// One task per connection, each holding its own Noise session: one TCP is one
/// Noise session is one stream, and there is no multiplexer anywhere in this
/// design.
pub async fn serve_on(
    listener: TcpListener,
    node: &NodeKey,
    store: &PeerStore,
    serving: Option<LeaseServing>,
) -> Result<()> {
    let context = SessionContext::new(node, store.path(), &crate::peer::state::default_path())
        .with_lease_serving(serving);
    serve_on_with(listener, context).await
}

/// [`serve_on`] with the context handed in, so the pairing-window file is the
/// CALLER's choice.
///
/// `serve_on` resolves that file with [`crate::peer::state::default_path`],
/// which is this machine's real runtime state. A two-process test must not read
/// it, and must not be able to be affected by an operator who happens to have
/// a pairing window open while the suite runs, so the test builds its own
/// [`SessionContext`] over a temp dir and calls this.
pub async fn serve_on_with(listener: TcpListener, context: SessionContext) -> Result<()> {
    // The ledger's charges are written HERE, off the relay path. See
    // `Ledger::dirty`. Started beside the accept loop rather than in
    // `server.rs` so every caller that serves peers gets it, including the
    // two-process tests, and stopped with the loop: the task holds nothing but
    // a clone of the ledger `Arc`, and it cannot be cancelled mid-write
    // because `Ledger::flush` has no await inside it.
    let ledger = context.lease_ledger();
    let flusher = ledger
        .clone()
        .map(|ledger| tokio::spawn(flush_ledger_periodically(ledger)));
    let outcome = accept_peer_connections(listener, context).await;
    if let Some(flusher) = flusher {
        flusher.abort();
    }
    // One last write on the way out, so an accept loop that ends between two
    // ticks does not take the charges of its final requests with it. (A caller
    // that DROPS this future instead, `server.rs`'s shutdown select, never
    // reaches here; the flusher it spawned outlives the drop and keeps writing
    // until the runtime goes down, which is the same bound either way.)
    if let Some(ledger) = ledger {
        let mut held = match ledger.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        held.flush();
    }
    outcome
}

/// How often the lender writes the charges its relays have recorded.
///
/// Short enough that a crash costs at most this much of the `spent` figure
/// `may_relay` bounds a lease by, long enough that a busy relay path is not
/// paying a locked file round trip per request. The write itself is skipped
/// entirely when nothing moved ([`Ledger::flush`]).
pub const LEDGER_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Write the ledger's pending charges every [`LEDGER_FLUSH_INTERVAL`], for as
/// long as this lender is serving.
///
/// A poisoned ledger lock is recovered rather than ending the flusher: the
/// counters a panicking relay left behind are still the best record of what
/// was spent, and the alternative is a lender that silently stops persisting
/// for the rest of the process's life.
async fn flush_ledger_periodically(ledger: Arc<Mutex<Ledger>>) {
    let mut ticker = tokio::time::interval(LEDGER_FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let mut held = match ledger.lock() {
            Ok(held) => held,
            Err(poisoned) => {
                tracing::warn!(
                    "peer lease: the ledger's lock is poisoned; flushing what it holds anyway"
                );
                poisoned.into_inner()
            }
        };
        if held.flush() {
            tracing::debug!("peer lease: the ledger's charges were written");
        }
    }
}

/// How long the accept loop waits after a transient accept error.
///
/// Long enough that an exhausted descriptor table is not a spin (EMFILE
/// answers immediately, so a bare retry loop would burn a core), short enough
/// that a peer dialling during the pause simply waits in the kernel's backlog.
pub const ACCEPT_RETRY_PAUSE: Duration = Duration::from_millis(100);

/// Whether an `accept` error ends this listener for good, or is about one
/// connection.
///
/// # What ending the loop cost
///
/// [`accept_peer_connections`] used to propagate every accept error. A peer
/// that hung up between the SYN and the accept (`ECONNABORTED`), or a process
/// momentarily out of descriptors (`EMFILE`, which a burst of leases or any
/// other part of the proxy can cause), therefore ended the mesh for the life
/// of the process: `server::supervise` supervises a task that has RETURNED, so
/// nothing starts it again, and the operator sees one warning line and a node
/// that answers nobody until the next restart.
///
/// Fatal is the short list, and it is the one where retrying would spin
/// forever on a socket that will never accept again: the listener's own
/// descriptor is gone or was never a socket.
pub fn accept_error_is_fatal(err: &std::io::Error) -> bool {
    /// `EBADF`, the one errno that says the listening descriptor itself is
    /// gone. Written as the number because this crate links no libc binding.
    const EBADF: i32 = 9;

    if err.raw_os_error() == Some(EBADF) {
        return true;
    }
    matches!(
        err.kind(),
        std::io::ErrorKind::NotConnected | std::io::ErrorKind::InvalidInput
    )
}

/// The accept loop itself. Split out of [`serve_on_with`] so that function has
/// one place to stop the flusher it started, on every exit path.
async fn accept_peer_connections(listener: TcpListener, context: SessionContext) -> Result<()> {
    // Read once, outside the loop: the socket a listener is bound to does not
    // change between accepts, and this is the value [`internet_admission`]
    // gates on for every connection this loop hands off.
    let bind = listener
        .local_addr()
        .context("peer listener: the bound socket has no address")?;
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                if !accept_error_is_fatal(&err) {
                    // A transient accept error is about ONE connection: the
                    // peer hung up between the SYN and the accept, or this
                    // process is momentarily out of descriptors. Ending the
                    // loop over it takes the whole mesh down until the next
                    // restart, and nothing restarts it (`server::supervise`
                    // supervises the task, and this task has RETURNED).
                    //
                    // The pause is what keeps EMFILE from becoming a spin: an
                    // exhausted descriptor table answers instantly, so a bare
                    // `continue` would burn a core until something else closed
                    // a file.
                    tracing::warn!(
                        error = %err,
                        kind = ?err.kind(),
                        "peer listener: one accept failed and the loop carries on"
                    );
                    tokio::time::sleep(ACCEPT_RETRY_PAUSE).await;
                    continue;
                }
                return Err(anyhow::Error::new(err).context("peer listener: accept failed"));
            }
        };
        // The address THIS connection was sent to, which on a wildcard bind
        // is the one fact that says which interface it came in on. Only the
        // own-Mac tailnet rule reads it; with none, that rule refuses.
        let accepted_on = match stream.local_addr() {
            Ok(local) => Some(local),
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "peer listener: an accepted socket has no local address"
                );
                None
            }
        };
        tokio::spawn(serve_one(stream, context.clone(), from, bind, accepted_on));
    }
}

/// Serve one accepted connection to its end, logging how it ended under the
/// same bounds the accept loop uses.
///
/// What the accept loop spawns per connection, and public so a second accept
/// loop (tcr's own tailnet node, [`crate::peer::tailnet_node`]) feeds the
/// same code rather than a copy of it: the same admission, the same bounded
/// refusal log, with `from`, `bind` and `accepted_on` (the address the
/// connection was sent to) taken off whatever socket it accepted.
pub async fn serve_one<S>(
    stream: S,
    context: SessionContext,
    from: SocketAddr,
    bind: SocketAddr,
    accepted_on: Option<SocketAddr>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let addr = knock_address(&from);
    if let Err(failure) = serve_connection(stream, &context, &from, bind, accepted_on).await {
        // Every refusal closes the connection and logs. There is no
        // arm that answers the peer: what this node will say to a
        // stranger is nothing at all.
        //
        // **Both paths are bounded, and by the same type from two
        // instances.** An earlier build logged every AUTHENTICATED failure
        // unconditionally on the reasoning that a pinned peer's stream
        // ending is always worth a line, which is true of the first
        // one and false of the thousandth: a pinned peer in a reconnect
        // loop is a pinned peer filling a disk, and `tcr peer forget`
        // is not the remedy an operator reaches for when the log is
        // what is broken. Two instances rather than one so that a flood
        // of anonymous refusals cannot silence a real peer's first
        // line.
        let admitted = lock_admission(&context.admission).admit_refusal(
            failure.authenticated,
            &addr,
            now_ms(),
        );
        let Some(suppressed) = admitted else {
            return;
        };
        if failure.authenticated {
            tracing::warn!(
                peer_addr = %from,
                error = %failure.error,
                suppressed,
                "peer connection closed"
            );
        } else {
            tracing::warn!(
                peer_addr = %from,
                error = %failure.error,
                suppressed,
                "peer connection refused before it authenticated"
            );
        }
    }
}

/// Why one connection ended, and whether the peer had authenticated by then.
///
/// The flag is what lets [`serve_on`] bound the log: a refusal before the
/// handshake completed is something any host that can reach the port can
/// produce on demand, and a failure on an authenticated session is a pinned
/// peer's stream ending, which is always worth a line.
struct ConnectionFailure {
    authenticated: bool,
    error: anyhow::Error,
}

impl ConnectionFailure {
    /// A refusal from before the handshake completed.
    const fn unauthenticated(error: anyhow::Error) -> Self {
        Self {
            authenticated: false,
            error,
        }
    }

    /// A failure on a session that had already proved who it was.
    const fn authenticated(error: anyhow::Error) -> Self {
        Self {
            authenticated: true,
            error,
        }
    }
}

/// What one connection needs from this node, owned so the task can outlive the
/// borrow of [`NodeKey`] and [`PeerStore`].
#[derive(Clone)]
pub struct SessionContext {
    /// This node's static secret, for `snow`'s builder and nothing else.
    secret: [u8; KEY_BYTES],
    /// This node's own id, which is what check 4 compares `via` against.
    node: PeerId,
    /// Where the policy half is re-read from, per connection and per frame.
    peers_path: PathBuf,
    /// Where the pairing window is read from, per connection: the runtime-state
    /// file `tcr peer pair` writes its deadline into
    /// ([`crate::peer::pair::open_pairing_window`]). A separate path because it
    /// is a separate tier, operator INTENT is the peers file, and a two-minute
    /// deadline is something the process learned.
    state_path: PathBuf,
    /// This node's own per-boot instance id, which is what it ANNOUNCES and
    /// what it knocks under. Held here so the beacon and the knock cannot
    /// disagree about it: one value per process, minted once.
    instance_id: InstanceId,
    /// The process-lifetime caps: the per-address knock buckets, the
    /// unauthenticated socket counters and the two refusal logs. Shared across
    /// every connection task on this listener, which is what makes a cap a cap.
    admission: Arc<Mutex<Admission>>,
    /// The gateway's per-peer per-hour byte ledger for carried TUNNELs, shared
    /// across every connection task on this listener the way [`Self::admission`]
    /// is, which is what makes the cap a cap rather than a per-connection
    /// allowance a peer can reset by reconnecting.
    tunnels: Arc<Mutex<crate::peer::tunnel::TunnelBudget>>,
    /// What the lender's half of a SERVE needs, or `None` for a build that
    /// wired none. See [`LeaseServing`].
    serving: Option<LeaseServing>,
}

impl SessionContext {
    /// Build a context from this node's key, its peers file and its
    /// runtime-state file.
    ///
    /// Serves no lease: SERVE is the one stream kind that needs process state
    /// beyond these three, so it is added with [`Self::with_lease_serving`]
    /// rather than defaulted into existence.
    pub fn new(node: &NodeKey, peers_path: &Path, state_path: &Path) -> Self {
        Self {
            secret: *node.secret_bytes(),
            node: node.id(),
            peers_path: peers_path.to_path_buf(),
            state_path: state_path.to_path_buf(),
            instance_id: crate::peer::id::boot_instance_id(),
            admission: Arc::new(Mutex::new(Admission::new())),
            tunnels: Arc::new(Mutex::new(crate::peer::tunnel::TunnelBudget::new())),
            serving: None,
        }
    }

    /// Wire the lender's half of SERVE onto this context, or leave it unwired
    /// with `None`.
    #[must_use]
    pub fn with_lease_serving(mut self, serving: Option<LeaseServing>) -> Self {
        self.serving = serving;
        self
    }

    /// This node's per-boot instance id, what it announces and knocks under.
    pub const fn instance_id(&self) -> InstanceId {
        self.instance_id
    }

    /// The shared caps, so a gate can read a token count or a live-socket
    /// count off the same state the accept loop decides on. Reading a copy
    /// would prove nothing about what the listener did.
    pub const fn admission(&self) -> &Arc<Mutex<Admission>> {
        &self.admission
    }

    /// The lender's ledger, or `None` on a build with no lease serving wired.
    ///
    /// For [`flush_ledger_periodically`], which needs the `Arc` and nothing
    /// else out of [`LeaseServing`].
    fn lease_ledger(&self) -> Option<Arc<Mutex<Ledger>>> {
        self.serving
            .as_ref()
            .map(|serving| Arc::clone(&serving.ledger))
    }
}

/// Run one lock-load-save on a blocking thread and wait for it there.
///
/// Every state-file and peers-file write on the listener's own paths goes
/// through this.
/// [`crate::peer::config::FileLock::acquire`] waits for a contended lock with
/// `std::thread::sleep` for up to [`crate::peer::config::LOCK_WAIT_MS`], and
/// the load and the save under it are synchronous file IO: run inline on the
/// proxy's runtime, a handful of simultaneous knocks park that many worker
/// threads for seconds each, and the local proxy stops answering. The knock
/// path is unauthenticated, so who arranges that handful is not this node's
/// choice.
///
/// A panic inside the closure comes back as the `JoinError`, reported as this
/// function's own error rather than resumed: the caller is one connection, and
/// a panicking state write must not take the accept loop's task with it.
async fn on_blocking_thread<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(outcome) => outcome,
        Err(err) => Err(anyhow::Error::new(err)
            .context("peer listener: a state-file write did not finish on its own thread")),
    }
}

/// Open ONE carrier at `friend` and serve whatever rides back over it.
///
/// The lender's half of the reverse path, and the production `run_one` that
/// [`crate::peer::tunnel::keep_reverse_carrier`]'s doc says lives outside that
/// function because it needs a stream header the wire only just learned.
///
/// # Three steps, and the third is the surprising one
///
/// Dial the friend; run the handshake so it knows WHICH pinned Mac is asking;
/// send a [`StreamKind::Park`] header. Then hand the raw socket to this node's
/// OWN accept path and await it. That is not a trick: the socket's far end is
/// on the friend's carrier desk, and what the friend eventually writes into it
/// is a forward, which is a fresh connection to this Mac in every respect
/// except who opened the TCP. Serving it through [`serve_connection`] is what
/// makes it one: the same ban list, the same caps, the same per-frame gate, on
/// a socket that happens to point the other way.
///
/// Returns when the carrier is finished, however it finished, which is the
/// contract `keep_reverse_carrier` loops on.
///
/// `from` is the friend's own address, because that is who is at the far end
/// of this socket. A forward arriving over it was requested by a third Mac,
/// and that Mac proves who it is inside its own Noise session, exactly as it
/// would on a forward the friend dialled.
pub async fn park_one_carrier(friend: &PeerRow, context: &SessionContext) -> Result<()> {
    let Some((addr, mut stream)) = serve::dial_peer_with_endpoint(friend).await else {
        bail!(
            "peer reverse: none of {}'s addresses answered, so no carrier could be parked there",
            friend.node.display()
        );
    };
    let mut session = noise::dial_handshake(
        &mut stream,
        &context.secret,
        Handshake::Return,
        Some(&friend.node.0),
        None,
    )
    .await
    .context("peer reverse: the handshake with the friend failed")?;
    let header = StreamHeader {
        kind: StreamKind::Park,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: crate::peer::id::random_id()
            .context("peer reverse: no request id for the carrier")?,
    };
    let bytes =
        serde_json::to_vec(&header).context("peer reverse: the header did not serialize")?;
    noise::send_encrypted(&mut stream, &mut session.transport, &bytes).await?;
    // The session's whole job is done: it said who is parking. Dropped here so
    // nothing can be tempted to write a keepalive into a socket somebody
    // else's carried session is about to own.
    drop(session);
    tracing::debug!(
        friend = %friend.node.display(),
        friend_addr = %addr,
        "peer reverse: a carrier is parked at this friend; serving whatever rides back"
    );

    // `bind` is the friend's address for the same reason `from` is: this
    // socket has no local listener behind it, and the value is only read by
    // the off-LAN admission gate, which asks whether the FAR end is off this
    // network. Answering it with the address the far end is actually at is the
    // honest reading.
    // No accepted socket of this Mac's own, so no own-Mac tailnet knock.
    serve_connection(stream, context, &addr, addr, None)
        .await
        .map_err(|failure| failure.error)
}

/// The secrets of every invite that is still outstanding, for the `IKpsk1`
/// trial. An expired row is not offered: its deadline is absolute, so a clock
/// that moved is not a second chance.
pub fn outstanding_secrets(file: &PeerFile) -> Vec<[u8; KEY_BYTES]> {
    file.outstanding_secrets(now_ms())
}

/// Unix milliseconds, or 0 if the clock is before the epoch.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_default(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}
