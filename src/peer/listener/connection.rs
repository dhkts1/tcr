use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tcr_peer_wire::{Control, PeerId, StreamHeader, StreamKind, TunnelTarget};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::peer::config::{self as config, PeerStore};
use crate::peer::id::KEY_BYTES;
use crate::peer::noise::{self, Handshake, PeerSession};
use crate::peer::serve;
use crate::peer::state::knock_address;

use super::admission::{
    HeaderlessSession, MAX_HEADERLESS_PER_PEER, MESSAGE_1_TIMEOUT, PunchInFlight,
    STREAM_HEADER_TIMEOUT, SocketSlot, unauthenticated_allowance,
};
use super::gate::{
    RequestDedup, handoff_is_permitted, peer_stream_gate_hop, peer_stream_gate_rows,
};
use super::knock::{KnockClock, record_pairing_key, serve_knock};
use super::pairing::{AcceptedSession, accept_pairing_or_return};
use super::scope::{InternetAdmission, admission_for_this_host};
use super::{
    ConnectionFailure, HANDSHAKE_TIMEOUT, SessionContext, now_ms, on_blocking_thread,
    outstanding_secrets,
};
use crate::peer::serve::{NodeFacts, hello_for_peer};

/// Handshake, gate, and then serve one stream until it closes.
///
/// # The order, which is the security of this file
///
/// 1. the ban list, read from `peer-state.json`, a blocked address gets zero
///    bytes before this node allocates a buffer, let alone reads a frame;
/// 2. the unauthenticated socket caps ([`SocketSlot`]), so sixteen strangers
///    cannot make this node hold a seventeenth;
/// 3. message 1, under [`MESSAGE_1_TIMEOUT`] and bounded by
///    [`crate::peer::noise::MAX_MESSAGE_1_BYTES`] **before the allocation**;
/// 4. which pattern it is, decided by length in one place
///    ([`Handshake::from_message_1_len`]);
/// 5. the internet gate ([`internet_admission`](crate::peer::listener::internet_admission)), which is the only check
///    here that reads the SOURCE address: a knock or a first pairing from
///    outside LAN scope, arriving on a listener bound to a globally routable
///    address, ends here, whatever `peer.internet` says;
/// 6. the pattern's own admission, meaning the knock bucket for a knock, the
///    operator's accepted window for a first pairing, the pin check for a
///    return visit, an outstanding invite for an enrolment;
/// 7. only then a byte is written.
pub(super) async fn serve_connection<S>(
    mut stream: S,
    context: &SessionContext,
    from: &SocketAddr,
    bind: SocketAddr,
    accepted_on: Option<SocketAddr>,
) -> std::result::Result<(), ConnectionFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let addr = knock_address(from);
    let now = now_ms();

    // ONE store per accepted connection, and the review's L2 is why it is a
    // store rather than two reads: everything after this point that needs the
    // peers file asks this value ([`PeerStore::file`]), and the connection
    // reads the runtime state file once, here, so an accepted connection
    // opens each file once. It used to open the peers file twice, here, and
    // again in `serve_stream` for the same bytes, which `tests/peer_lease.rs`
    // now counts through `config::peers_file_opens`.
    //
    // Before `SocketSlot::acquire`, because the ban list and the cap the slot
    // is taken against are both read out of these two values.
    let store = PeerStore::open(&context.peers_path).map_err(ConnectionFailure::unauthenticated)?;
    let file = store.file();
    let peer_state = crate::peer::state::load(&context.state_path, now)
        .map_err(ConnectionFailure::unauthenticated)?;

    // Check 1. A banned address is refused before anything else happens, which
    // is the cheapest refusal this file has and the one an operator asked for
    // explicitly.
    if peer_state.is_address_banned(&addr) {
        return Err(ConnectionFailure::unauthenticated(anyhow::anyhow!(
            "peer listener: {addr} is blocked (`tcr peer unblock {addr}` lifts it); \
             closed with nothing written"
        )));
    }

    // Check 2, held for the whole of the unauthenticated phase and released on
    // every exit path by `Drop`. See [`SocketSlot`].
    let slot = SocketSlot::acquire(&context.admission, &addr, unauthenticated_allowance(&file))
        .map_err(|refusal| ConnectionFailure::unauthenticated(anyhow::anyhow!("{refusal}")))?;

    // **One deadline for everything done while that slot is held**, measured
    // from the moment it was taken rather than from each step's own start.
    //
    // A pattern that authenticates gives the slot back as soon as message 1 is
    // in hand, so [`MESSAGE_1_TIMEOUT`] already bounds it. A KNOCK keeps the
    // slot to the end, and the steps after message 1 each carried their own
    // deadline and nothing added them up: five seconds to deliver message 1,
    // then up to [`crate::peer::config::LOCK_WAIT_MS`] waiting for the state
    // file's lock, then another five for the rest of the handshake. A knocker
    // that spent every one of them held a sixteenth of this node's
    // unauthenticated capacity for fifteen seconds while saying almost
    // nothing.
    //
    // [`HANDSHAKE_TIMEOUT`] is the number because it is what a connection that
    // really does authenticate is allowed, and no unauthenticated one should
    // hold a slot longer than that.
    let slot_deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;

    // Check 3. A socket opened and left silent costs five seconds, and the
    // length prefix cannot reserve more than the largest message 1 any pattern
    // here has.
    let message_1 = match tokio::time::timeout(
        MESSAGE_1_TIMEOUT,
        noise::read_frame_bounded(&mut stream, noise::MAX_MESSAGE_1_BYTES),
    )
    .await
    {
        Ok(frame) => frame.map_err(ConnectionFailure::unauthenticated)?,
        Err(elapsed) => {
            return Err(ConnectionFailure::unauthenticated(
                anyhow::Error::new(elapsed).context(
                    "peer listener: no message 1 within the pre-authentication timeout; \
                     closed with nothing written",
                ),
            ));
        }
    };

    // The slot is released as soon as message 1 is in hand for every pattern
    // that goes on to AUTHENTICATE: holding it through the handshake would cap
    // a pinned peer's concurrent streams at two. See
    // [`MAX_UNAUTHENTICATED_SOCKETS`] for the measurement.
    //
    // A KNOCK never authenticates, so it keeps its slot to the end (the arm
    // below). Dropping it here left the knock path with no in-flight bound at
    // all: message 1 is cheap to produce, and everything after it is a state
    // file write under a lock, so a LAN host could hold as many knocks in
    // flight as it could open sockets. Bounded now by the same two counters
    // that bound the silent window, and by `MESSAGE_1_TIMEOUT` on the
    // handshake that follows.

    // Check 4, in one place: see [`Handshake::from_message_1_len`].
    let Some(pattern) = Handshake::from_message_1_len(message_1.len()) else {
        return Err(ConnectionFailure::unauthenticated(anyhow::anyhow!(
            "peer listener: {} first bytes are not a Noise message 1 ({} for a knock, {} for \
             a knock under the network key, {} for a first pairing, {} for a return or an \
             enrolment); closing with nothing written",
            message_1.len(),
            noise::KNOCK_MESSAGE_1_LEN,
            noise::KNOCK_PSK_MESSAGE_1_LEN,
            noise::XX_MESSAGE_1_LEN,
            noise::IK_MESSAGE_1_LEN
        )));
    };

    // Check 5, and the only one that reads where the connection came from.
    // Before the knock arm, so a knock from off the LAN never reaches the
    // bucket, the queue or the operator's screen. Gated on `bind`, the socket
    // this listener is bound to, and on `file.internet` for the one case the
    // bind class alone got wrong: two Macs on one home LAN with ISP-delegated
    // IPv6. See [`internet_admission`]'s own doc for both halves. Then the one
    // tailnet case: a knock from the operator's own Mac, arriving on this
    // Mac's own tailnet address in app mode ([`own_mac_admission`]).
    if admission_for_this_host(
        bind.ip(),
        accepted_on.map(|local| local.ip()),
        from.ip(),
        pattern,
        &file,
        &crate::peer::tailnet_find::OWN_MAC_READINGS,
        crate::peer::tailnet_find::read_the_app,
    )
    .await
        == InternetAdmission::Refuse
    {
        return Err(ConnectionFailure::unauthenticated(anyhow::anyhow!(
            "peer listener: this listener is bound to a globally routable address and {addr} \
             is neither on this LAN nor in one of this Mac's own IPv6 /64 prefixes, so a {} \
             gets nothing back; only a return visit against a pinned key is answered from off \
             the LAN, and over the tailnet a knock from one of your own Macs in app mode",
            pattern.pattern()
        )));
    }

    // Check 6, the knock arm. Everything about a knock ends here: it never
    // reaches a stream header, a gate or a handler, because the only thing it
    // can produce is a row an operator reads.
    if matches!(pattern, Handshake::Knock | Handshake::KnockPsk) {
        let outcome = serve_knock(
            &mut stream,
            context,
            &addr,
            pattern,
            &message_1,
            &file,
            KnockClock {
                now_ms: now,
                slot_deadline,
            },
        )
        .await
        .map_err(ConnectionFailure::unauthenticated);
        drop(slot);
        return outcome;
    }
    drop(slot);

    let psks = outstanding_secrets(&file);
    let session = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        accept_pairing_or_return(
            &mut stream,
            context,
            &addr,
            pattern,
            &message_1,
            &file.pinned_ids(),
            &psks,
            &peer_state,
            now,
        ),
    )
    .await
    {
        Ok(accepted) => accepted.map_err(ConnectionFailure::unauthenticated)?,
        Err(elapsed) => {
            return Err(ConnectionFailure::unauthenticated(
                anyhow::Error::new(elapsed)
                    .context("peer handshake: timed out before it completed"),
            ));
        }
    };

    if session.session.handshake == Handshake::Pair {
        // The responder's half of the six-digit compare: the operator in front
        // of THIS Mac has to be able to read the digits the dialling Mac is
        // showing, or the compare has one screen and proves nothing.
        tracing::info!(
            code = %session.session.code,
            peer_addr = %from,
            "peer pairing: compare these six digits with the other Mac, then trust it there"
        );
        // A first pairing ENDS here, and it writes no pin. The `XX` handshake
        // has proved a static key to both machines and bound it with the six
        // digits; what turns that into a row is the operator confirming on each
        // side (`tcr peer pair <host:port> <code>`), which is a separate act by
        // design. An earlier build let this fall through into the stream gate,
        // where it was refused for want of the row it had not created yet, a
        // successful pairing that logged a refusal.
        //
        // The learned static key is recorded against the accepted window so
        // that `tcr peer block` can ban the KEY and not only the address: this
        // is the one moment a knock-then-pair sequence has one in hand.
        if let Err(err) = record_pairing_key(context, &addr, session.session.peer, now).await {
            tracing::warn!(
                error = %err,
                peer_addr = %from,
                "peer pairing: could not record the static key this pairing learned; a later \
                 `tcr peer block` will ban the address only"
            );
        }
        return Ok(());
    }

    // BY VALUE, not `&mut`: a TUNNEL spawns a Noise pump that outlives this
    // frame's borrow, so `tunnel::handle_tunnel_on` needs the stream itself
    // (`Send + 'static`). Every other kind still works on a `&mut` taken
    // inside `serve_stream`.
    serve_stream(stream, session, context, &store, *from)
        .await
        .map_err(ConnectionFailure::authenticated)
}

/// [`serve_connection`] for a caller that already has the stream and the
/// address it came from.
///
/// The accept loop is not the only thing that knows a source address, and on
/// this machine it is not the thing that can produce an interesting one:
/// macOS puts only `127.0.0.1`, `::1` and one link-local address on `lo0`, so
/// `10.0.0.7` and `2001:db8::1` cannot be dialled from here at all
/// (`tests/peer_abuse.rs` measured it, and adding an alias needs root on a
/// machine that is also serving a live proxy). This takes the address as the
/// value it already is inside [`serve_connection`], so the refusal a peer off
/// the LAN gets can be observed as BYTES on a real stream rather than inferred
/// from a verdict.
///
/// Loses the authenticated/unauthenticated split the accept loop uses to bound
/// its own logging, which is why the loop still calls [`serve_connection`]
/// directly.
///
/// `bind` is the socket [`internet_admission`](crate::peer::listener::internet_admission) gates on, the same way the
/// accept loop reads it off the real `TcpListener`: a caller here can name
/// `0.0.0.0:7755` without actually binding it, which is how the globally
/// routable case is exercised without opening a socket this box does not own.
pub async fn serve_accepted<S>(
    stream: S,
    context: &SessionContext,
    from: &SocketAddr,
    bind: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    serve_connection(stream, context, from, bind, None)
        .await
        .map_err(|failure| failure.error)
}

/// Serve a punched connection, with the recursion stated rather than inferred.
///
/// [`serve_accepted`] leads back to `serve_control`, which is where a punch is
/// spawned, so a punch that serves its own result is an async cycle and the
/// compiler cannot decide by inference whether the future is `Send`. Boxing it
/// as `dyn Future + Send` erases the cycle and asserts the fact, which is the
/// standard shape for recursive async and the only reason this function is not
/// one line at the call site.
fn serve_punched(
    stream: noise::PeerStream,
    context: SessionContext,
    from: SocketAddr,
    bind: SocketAddr,
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async move { serve_accepted(stream, &context, &from, bind).await })
}

/// What each of the peers-file writes a `Control::Hello` makes answered.
///
/// One value carried back off the blocking thread rather than four thread
/// hops, and a struct rather than a tuple so the arm that reports them cannot
/// pair a sentence with the wrong outcome.
struct HelloWrites {
    /// The pair's rendezvous secret.
    rendezvous: Result<bool>,
    /// The neighbour briefs, when the frame carried any.
    briefs: Option<Result<usize>>,
    /// The endpoints this peer says it listens on, plus the host it was seen
    /// from on the port it announced.
    endpoints: Result<config::Observed>,
    /// The address this peer says it sees us at, when it named one that parses.
    sees_us_at: Option<Result<bool>>,
}

impl HelloWrites {
    /// Say what did not get written, in the words each failure deserves.
    ///
    /// Every one is surfaced and none is fatal: the session is authenticated
    /// and useful, and what failed is a hint the next Hello offers again. A
    /// rendezvous secret that would not write costs this pair the derived-port
    /// fallback after a restart, which is the reach it had before that
    /// fallback existed; it must not cost them the session open right now.
    fn report(self, peer: PeerId, from: SocketAddr) {
        if let Err(err) = self.rendezvous {
            tracing::warn!(
                peer = %peer.display(),
                error = %err,
                "peer control: could not record this pair's rendezvous secret",
            );
        }
        if let Some(Err(err)) = self.briefs {
            tracing::warn!(
                peer = %peer.display(),
                error = %err,
                "peer control: could not record a neighbor brief from this peer",
            );
        }
        if let Err(err) = self.endpoints {
            tracing::warn!(
                peer = %peer.display(),
                peer_addr = %from,
                error = %err,
                "peer control: could not record where this peer answers; it stays reachable \
                 at the endpoints already on its row"
            );
        }
        if let Some(Err(err)) = self.sees_us_at {
            tracing::warn!(
                peer = %peer.display(),
                error = %err,
                "peer control: could not record where this peer sees us",
            );
        }
    }
}

/// Where a stream's transport socket actually came from, once a peer's own
/// `via` stamp has been read: the accepting socket itself, or a carrier that
/// relayed for a peer that stamped its own id into the inner header.
///
/// No address is ever learned from a carrier's socket: a carrier's connection
/// to this node proves the carrier's static key, nothing about who is behind
/// it, so [`Self::observed_socket`] answers `None` for the carried case and
/// every caller that would otherwise write the socket onto a row, into the
/// observation register, or back to the peer as `observed_you_at` has to ask
/// it first. [`Self::socket`] still answers the raw socket for a caller that
/// only wants it for a log line, where naming the carrier's own connection is
/// honest and writing it as the peer's address is not.
#[derive(Debug, Clone, Copy)]
enum StreamOrigin {
    /// This socket is the peer's own connection to this node.
    Direct(SocketAddr),
    /// This socket is a carrier's connection; the peer arrived through it.
    Carried { socket: SocketAddr, carrier: PeerId },
}

impl StreamOrigin {
    /// Built once per stream, from the accepted socket and the header's
    /// `via`: non-empty means the frame's own origin stamped a carrier in, so
    /// nothing after this point may treat `from` as the peer's address.
    const fn from_header(from: SocketAddr, via: &[PeerId]) -> Self {
        match via.first() {
            Some(&carrier) => Self::Carried {
                socket: from,
                carrier,
            },
            None => Self::Direct(from),
        }
    }

    /// The socket to treat as evidence about the peer: `Some` only when this
    /// stream's own transport connection is the peer's, never a carrier's.
    const fn observed_socket(&self) -> Option<SocketAddr> {
        match self {
            Self::Direct(addr) => Some(*addr),
            Self::Carried { .. } => None,
        }
    }

    /// The raw transport socket, for a log line that wants to say which
    /// connection the bytes arrived on, whichever kind it is.
    const fn socket(&self) -> SocketAddr {
        match self {
            Self::Direct(addr) => *addr,
            Self::Carried { socket, .. } => *socket,
        }
    }

    /// The carrier's id, for a log line; `None` for a direct arrival.
    const fn carrier(&self) -> Option<PeerId> {
        match self {
            Self::Direct(_) => None,
            Self::Carried { carrier, .. } => Some(*carrier),
        }
    }
}

/// Serve one authenticated stream: the header, the gate, then the handler.
///
/// The row set is re-read and the gate re-run **before every frame**, so
/// `tcr peer forget` drops a live session within one frame. Cheap by design:
/// the file is small and a peer stream is not a hot loop.
async fn serve_stream<S>(
    stream: S,
    accepted: AcceptedSession,
    context: &SessionContext,
    store: &PeerStore,
    from: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Taken by value because the TUNNEL arm hands the stream to a pump that
    // outlives this function's frame; every other arm borrows it from here, so
    // nothing else changes shape.
    let mut stream = stream;
    let AcceptedSession {
        mut session,
        enrol_secret,
    } = accepted;
    let mut dedup = RequestDedup::new();
    // **The header is the second thing counted, and the first was given back.**
    //
    // The pre-authentication socket slot is released as soon as the handshake
    // completes, so an authenticated session that never says anything else was
    // held by nothing at all: the task parked on this read forever. A replayed
    // `IK` message 1 reaches this point without the far side keeping anything
    // alive, so "authenticated" is not the same as "somebody is still there".
    // The deadline ends one such session and the slot bounds how many a peer
    // can have waiting at once.
    let headerless =
        HeaderlessSession::acquire(context.admission(), session.peer).map_err(|live| {
            anyhow::anyhow!(
                "peer stream: this peer holds {live} authenticated sessions that have not sent \
                 a stream header and the allowance is {MAX_HEADERLESS_PER_PEER} \
                 (MAX_HEADERLESS_PER_PEER); closed"
            )
        })?;
    let frame = match tokio::time::timeout(
        STREAM_HEADER_TIMEOUT,
        noise::recv_encrypted(&mut stream, &mut session.transport),
    )
    .await
    {
        Ok(frame) => frame?,
        Err(_elapsed) => bail!(
            "peer stream: no stream header within {} s of the handshake completing; closed",
            STREAM_HEADER_TIMEOUT.as_secs()
        ),
    };
    drop(headerless);
    let header: StreamHeader =
        serde_json::from_slice(&frame).context("peer stream: the first frame is not a header")?;

    // **The enrolment exemption, and the only thing it admits.**
    //
    // A joiner is not pinned, that is what enrolling means, so the gate below
    // would refuse the very frame that creates its row. The rules are stated in
    // full in `crate::peer::noise`'s module docs; this is where they are
    // enforced, and for a long time nothing enforced them: `accept_enrolment`
    // had no production caller, so `tcr peer join` completed its handshake,
    // sent its `Control::Enroll`, was refused for want of a row, and the
    // registrar wrote nothing. The joiner then waited for a `Hello` that never
    // came, which is at least an honest failure, but the headless path, the
    // one a machine with no screen has, did not work at all.
    //
    // The exemption is exactly one frame wide: the header must be CONTROL, the
    // next frame must be a `Control::Enroll`, and the handshake must have been
    // `IKpsk1` under a secret that matched an OUTSTANDING invite (proved inside
    // message 1, before this node wrote a byte). Everything after it meets the
    // ordinary gate against the row it just created.
    if session.handshake == Handshake::Enrol {
        let Some(secret) = enrol_secret else {
            bail!(
                "peer stream: an enrolment session arrived with no invite secret attached, \
                 which is a bug in this listener rather than anything the joiner did; \
                 nothing was pinned"
            );
        };
        if header.kind != StreamKind::Control {
            bail!(
                "peer stream: an enrolment may open a CONTROL stream and nothing else, and \
                 this one declared {:?}; nothing was pinned",
                header.kind
            );
        }
        // The origin is built here, after the two checks just above (CONTROL
        // kind, an outstanding invite proved in message 1): those are the
        // enrolment path's own gate, the fields a peer controls that must
        // answer first, and there is no row yet for `peer_stream_gate_rows`
        // to check.
        let origin = StreamOrigin::from_header(from, &header.via);
        return serve_enrolment(&mut stream, &mut session, context, &secret, store, origin).await;
    }

    // Read BEFORE the session can be moved into a carry: the TUNNEL arm hands
    // `session` to the pump, and the peer id is what every line after that
    // names.
    let accepted_peer = session.peer;
    // The bytes this connection already read, unless the operator edited the
    // file while the handshake ran, `reload_if_changed` stats and re-reads
    // only then, so the gate is as fresh as it ever was and the ordinary
    // connection opens the file once (the review's L2).
    store.reload_if_changed();
    let file = store.file();
    let row = file.row(&session.peer);
    peer_stream_gate_rows(&header, row).map_err(anyhow::Error::new)?;
    peer_stream_gate_hop(&context.node, &header, &mut dedup, now_ms())
        .map_err(anyhow::Error::new)?;

    // Built only after the two gates above: they read fields a peer controls
    // and have to answer first (`peer_stream_gate`'s own doc,
    // `:3374-3377`), and `header.via` is one of those fields.
    let origin = StreamOrigin::from_header(from, &header.via);

    match header.kind {
        StreamKind::Control => {
            serve_control(&mut stream, &mut session, context, store, origin).await
        }
        // The lender's half. The gate above has already agreed this peer holds
        // `inspect`; `handle_serve_on` asks the same question again through
        // this store, which is the belt-and-braces the SERVE module documents
        // rather than a duplicate, the row can be revoked between the header
        // frame and the request frame, and one frame is the revocation latency
        // this file promises. It stays true through the connection's own store
        // because `handle_serve` opens with `store.reload_if_changed()`
        // (`src/peer/serve.rs:675`): a re-read when the file moved, and no open
        // at all when it did not, which is the review's L2.
        StreamKind::Serve => {
            let Some(serving) = &context.serving else {
                bail!(
                    "peer stream: SERVE is granted for this peer but this build wired no lease \
                     ledger, so there is nothing to serve it against (a lender needs \
                     `listener::serve` to be given a `LeaseServing`)"
                );
            };
            serve::handle_serve_on(
                &mut stream,
                &mut session,
                store,
                serving,
                header.request_id,
                now_ms(),
                from,
            )
            .await
        }
        // The gateway's half of blind egress. The gate above has already
        // agreed this peer holds `allow.gateway` for an origin target
        // (`peer_stream_gate_rows`); everything else a carry needs, the
        // allow-listed origins, the per-peer hourly byte cap and the SNI check,
        // is `tunnel::handle_tunnel_on`'s, and it answers the first two
        // before a socket to the origin exists.
        //
        // The stream goes in BY VALUE: the carry spawns a pump over it that
        // outlives this frame, which is the whole reason `serve_stream` takes
        // it owned.
        StreamKind::Tunnel => {
            let Some(target) = header.target.as_ref() else {
                bail!(
                    "peer stream: a TUNNEL with no target is not something this node can \
                     carry"
                );
            };
            // **Which of the two handlers, decided by the TARGET and by
            // nothing else.** An origin target is a gateway carry and a peer
            // target is a forward, and the two differ in what they are handed
            // rather than only in where they dial: a forward needs this Mac's
            // peers file (the requester's `allow.relay` and the TARGET's own
            // row), which `handle_tunnel_on` is deliberately not given, so a
            // build that routed both through it would either be an open relay
            // or, as before, refuse every forward.
            //
            // The route is this node's own DECISION and is built here from the
            // target the gate already admitted, never from a second field a
            // peer could disagree with: `authorize_forward` refuses the two
            // when they differ, and this is the only production caller that
            // sets them.
            let carry = crate::peer::tunnel::Carry {
                route: match target {
                    TunnelTarget::Peer { node } => crate::peer::tunnel::OriginRoute::Peer(*node),
                    TunnelTarget::Origin { .. } => crate::peer::tunnel::OriginRoute::Resolve,
                },
                peer: accepted_peer,
                target,
                hosts: crate::peer::tunnel::egress::PEER_EGRESS_HOSTS,
                cap_bytes: crate::peer::tunnel::DEFAULT_MAX_TUNNEL_BYTES_PER_HOUR,
                budget: &context.tunnels,
                now_ms: now_ms(),
            };
            let (kind, (up, down)) = match target {
                TunnelTarget::Peer { .. } => (
                    "FORWARD",
                    crate::peer::tunnel::handle_forward_on(
                        stream,
                        session,
                        carry,
                        crate::peer::tunnel::Forward {
                            store,
                            node: context.node,
                            hops_remaining: header.hops_remaining,
                            via: &header.via,
                        },
                    )
                    .await?,
                ),
                TunnelTarget::Origin { .. } => (
                    "TUNNEL",
                    crate::peer::tunnel::handle_tunnel_on(stream, session, carry).await?,
                ),
            };
            // What this carry cost, against the path it arrived over, so the
            // per-path "carried this hour" figure is a measurement rather than
            // a total split by guesswork. Unattributable (no recorded endpoint
            // with this source's host, or a hop whose socket is the
            // forwarder's) charges nothing: `locator_from` answers `None`
            // rather than naming the first locator on the row.
            if let Some(path) = store
                .row(&accepted_peer)
                .and_then(|row| row.locator_from(from))
            {
                if let Ok(mut meter) = crate::peer::tunnel::path_meter().lock() {
                    meter.charge_bytes(&accepted_peer, path, up + down, now_ms());
                } else {
                    tracing::warn!(
                        peer = %accepted_peer.display(),
                        "peer stream: the path meter's lock is poisoned, so this carry is not on any path's hour"
                    );
                }
            }
            tracing::debug!(
                bytes_up = up,
                bytes_down = down,
                peer = %accepted_peer.display(),
                "peer stream: {kind} closed"
            );
            Ok(())
        }
        // The carrier desk's half of the reverse path. The stream goes in BY
        // VALUE and never comes back: from here it is a raw socket on the desk
        // waiting for a forward to spend it, and this function returning is
        // what LEAVES it open rather than what closes it.
        //
        // The Noise session this arrived over is dropped right here, unused.
        // It proved who is parking, which is all it is for: whatever rides
        // this socket next is a fresh session the far Mac's own listener
        // answers, and a byte written here to keep the socket warm would land
        // in the middle of somebody's carried session.
        StreamKind::Park => {
            let outcome = crate::peer::tunnel::park_reverse_carry(
                store,
                session.peer,
                Box::new(stream),
                now_ms(),
            );
            match outcome {
                crate::peer::tunnel::ParkOutcome::Parked { open } => {
                    tracing::info!(
                        peer = %session.peer.display(),
                        peer_addr = %from,
                        open,
                        "peer reverse: holding a carrier for a Mac that cannot be dialled"
                    );
                    Ok(())
                }
                // A refusal is the end of this connection and nothing else:
                // the socket is dropped with the outcome, and the far Mac
                // learns it has no carrier here by the same means it learns
                // anything, its next attempt.
                crate::peer::tunnel::ParkOutcome::Refused(refusal) => {
                    bail!("peer stream: this carrier was not parked ({refusal:?})")
                }
            }
        }
        StreamKind::Unknown(raw) => bail!("peer stream: stream kind {raw} is unknown"),
    }
}

/// The registrar's half of an enrolment: read the one `Control::Enroll`, write
/// the pinned row, answer the `Hello` that is the joiner's acknowledgement,
/// then fall into the ordinary CONTROL loop against the row just created.
///
/// The write is [`crate::peer::pair::accept_enrolment`], which spends the
/// invite and pins the joiner in ONE locked read-modify-write. See its docs
/// for why two writes cannot be correct here, and
/// [`crate::peer::config::FileLock`] for why the lock is what makes a one-use
/// key admit one machine.
///
/// The `Hello` is the ordinary message any pinned peer may receive, so the
/// acknowledgement needs no new wire type; [`crate::peer::pair::join_as`] pins
/// the registrar and reports success only when it arrives.
async fn serve_enrolment<S>(
    stream: &mut S,
    session: &mut PeerSession,
    context: &SessionContext,
    secret: &[u8; KEY_BYTES],
    store: &PeerStore,
    origin: StreamOrigin,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = noise::recv_encrypted(stream, &mut session.transport).await?;
    let message: Control = serde_json::from_slice(&frame)
        .context("peer enrol: this frame is not a control message")?;
    let Control::Enroll(enroll) = message else {
        bail!(
            "peer enrol: the one frame an unpinned enrolment may send is a Control::Enroll, \
             and this one is {message:?}; nothing was pinned"
        );
    };

    if let Some(carrier) = origin.carrier() {
        tracing::debug!(
            peer = %session.peer.display(),
            carrier = %carrier.display(),
            "peer enrol: this enrolment arrived through a carrier; no address is learned \
             about the joiner from this socket"
        );
    }

    // **On a blocking thread**, the rule [`on_blocking_thread`] states: this is
    // a locked read-modify-write of the peers file, the lock waits with
    // `std::thread::sleep`, and an enrolment is served on a connection whose
    // only credential is a one-use invite. Run inline it parked one of the
    // proxy's worker threads for as long as whatever holds that lock takes,
    // and what stops answering is the LOCAL proxy.
    let row = on_blocking_thread({
        let peers_path = context.peers_path.clone();
        let peer = session.peer;
        let secret = *secret;
        let observed_socket = origin.observed_socket();
        move || {
            crate::peer::pair::accept_enrolment(
                &peers_path,
                peer,
                &enroll,
                &secret,
                now_ms(),
                observed_socket,
            )
        }
    })
    .await?;

    let mut facts = NodeFacts::listening(context.node, store.file().listen);
    facts.briefs = store.file().neighbor_briefs(session.peer);
    let hello = hello_for_peer(&facts, &row.allow.control);
    let bytes = serde_json::to_vec(&Control::Hello(hello))
        .context("peer enrol: the acknowledging Hello did not serialize")?;
    noise::send_encrypted(stream, &mut session.transport, &bytes).await?;

    // The row exists now, so every later frame on this stream is an ordinary
    // pinned peer's frame and meets the ordinary gate. `serve_control` opens
    // with `store.reload_if_changed()`, and `accept_enrolment` has just
    // written the file, so the row it wrote is the row the next frame is
    // gated against.
    serve_control(stream, session, context, store, origin).await
}

/// The CONTROL handler: answer `Hello` with a `Hello` assembled from this
/// peer's grants, answer `Ping`, refuse everything phase 4 owns.
/// How often an idle CONTROL session wakes to ask whether a handed bearer is
/// due for renewal.
///
/// A choice, not a measurement, and the two things it is bounded by are named
/// rather than left implicit. It has to be well under
/// [`crate::peer::lease::HANDOFF_RENEW_LEAD_MS`], or the lead time this wakes
/// to honour would elapse between wakes; and it has to be long enough that an
/// idle session is not a busy loop. One second on a per-connection timer that
/// does nothing but compare two integers when nothing is handed.
const HANDOFF_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Push a fresh `Handoff` for every hand-mode lease on this session whose
/// bearer is close to expiring.
///
/// The owner renews rather than the borrower asking, because the borrower has
/// nothing to ask WITH: the token is the only thing it holds and a request for
/// a new one carries no proof the old one is still legitimate.
///
/// Revocation is the absence of this call, which is what
/// [`crate::peer::lease::handoff_for`]'s doc means by "revocation IS stop
/// renewing": a lease dropped from the ledger, past its `until`, or expired is
/// simply not in the list below, and no recall frame exists because a token on
/// another machine cannot be taken back.
async fn push_due_handoffs<S>(
    stream: &mut S,
    session: &mut PeerSession,
    context: &SessionContext,
    store: &PeerStore,
    handed: &mut HashMap<u128, crate::peer::lease::HandedBearer>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(serving) = &context.serving else {
        return Ok(());
    };
    if !handoff_is_permitted(session.handshake) {
        return Ok(());
    }
    if handed.is_empty() {
        return Ok(());
    }
    let now = now_ms();

    // Only leases this session ALREADY handed a bearer for, which is what makes
    // the mode question disappear: a lease in this map was funded by a hand
    // grant, because the grant arm is the only thing that puts one here and it
    // reads the mode off the grant the ledger itself picked
    // (`lease::Granted::mode`).
    //
    // Re-deriving the mode here by window would pick an ENDED hand grant beside
    // the live serve grant that actually funded the lease, which is the review's
    // first HIGH and has its own gate
    // (`an_ended_hand_grant_beside_a_live_serve_grant_hands_nothing_over`). The
    // first draft of this function did exactly that and that gate caught it.
    //
    // A lease nobody handed needs no renewal: its bearer went out with the
    // grant, on this same session.
    //
    // The ledger's lock is taken and dropped before any await: a guard held
    // across a send is how one slow borrower stops every other lease on this
    // Mac, which is the rule the grant arm already follows.
    let live: Vec<tcr_peer_wire::Lease> = {
        let Ok(held) = serving.ledger.lock() else {
            return Ok(());
        };
        held.live(now)
            .into_iter()
            .filter(|lease| held.grantee_of(lease.lease_id) == Some(session.peer))
            .filter(|lease| handed.contains_key(&lease.lease_id))
            .collect()
    };
    // Revocation IS the absence of a renewal: a lease dropped from the ledger,
    // past its `until`, or expired is simply not in `live`, and it leaves this
    // map with it. There is no recall frame, because a token already on another
    // machine cannot be taken back.
    handed.retain(|id, _| live.iter().any(|lease| lease.lease_id == *id));

    store.reload_if_changed();
    let file = store.file();
    let Some(row) = file.row(&session.peer) else {
        // Forgotten mid-session. The next frame closes the stream; this one
        // just does not renew, which IS the revoke.
        return Ok(());
    };

    for lease in live {
        let last = handed.get(&lease.lease_id).copied();
        if !crate::peer::lease::handoff_renewal_due(last.map(|sent| sent.expires_at_ms), now) {
            continue;
        }
        // Whether the bearer this poll would send is worth sending is decided
        // below, once it is known, by `handoff_push_is_due`. This first check
        // is only here to keep an undue lease from reading the owner's token
        // out of the config at all.
        // The funding grant, re-read per renewal and through the SAME selector
        // `Ledger::grant` uses. Two things ride on it. An operator who revokes
        // the grant stops the renewals within one poll, which is what
        // `handoff_for`'s "revocation IS stop renewing" means when there is no
        // recall frame and no ledger removal; and the selector skips ENDED
        // grants, so this cannot pick the ended `hand` grant beside a live
        // `serve` one, which is the review's first HIGH.
        let Some(grant) = row.grant_for(
            lease.window,
            u64::try_from(now / 1_000).unwrap_or_default(),
            &|scope| {
                serving.utilization.scope_restriction(scope)
                    != crate::peer::lease::ScopeRestriction::Unenforceable
            },
        ) else {
            continue;
        };
        if grant.mode != config::LendMode::Hand {
            continue;
        }
        let scope = match serving.ledger.lock() {
            Ok(held) => held.scope_of(lease.lease_id),
            Err(_) => continue,
        };
        let Some(frame) = crate::peer::lease::handoff_for(
            grant.mode,
            &lease,
            // The LEASE's window, so the baseline the frame carries and the
            // room the grant was measured on are one window's.
            serving.manager.handoff_bearer(&scope, lease.window),
            now,
        ) else {
            continue;
        };
        if let Control::Handoff {
            lease_id,
            expires_at_ms,
            access_token,
            utilization: _,
        } = &frame
        {
            let next = crate::peer::lease::HandedBearer {
                expires_at_ms: *expires_at_ms,
                fingerprint: crate::peer::lease::bearer_fingerprint(access_token.reveal()),
            };
            // The bearer the borrower already holds is not a renewal. The
            // record is left as it is when this skips, so the moment the
            // owner's credential IS refreshed the next poll sends the new one.
            if !crate::peer::lease::handoff_push_is_due(last, next, now) {
                continue;
            }
            handed.insert(*lease_id, next);
        }
        let bytes = serde_json::to_vec(&frame)
            .context("peer control: the renewing Handoff did not serialize")?;
        noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
        tracing::debug!(
            peer = %session.peer.display(),
            lease = %config::lease_id_string(lease.lease_id),
            "peer control: pushed a fresh bearer before the last one expires"
        );
    }
    Ok(())
}

async fn serve_control<S>(
    stream: &mut S,
    session: &mut PeerSession,
    context: &SessionContext,
    store: &PeerStore,
    origin: StreamOrigin,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // What this session last handed over, per lease, so the pusher below knows
    // whether a fresh bearer is due. Per SESSION and not global: a Handoff can
    // only travel on the `IK`-authenticated stream to its own grantee, so the
    // one that would send it is the one that has to remember.
    let mut handed: HashMap<u128, crate::peer::lease::HandedBearer> = HashMap::new();

    // The frame reader lives as long as the session, and that is the whole
    // point of it. The timeout below cancels the read once a second on every
    // control session, so the bytes already taken off the socket have to
    // survive in something the loop owns rather than in the cancelled future:
    // a frame whose halves straddle a tick is otherwise torn, and the next read
    // reads ciphertext as a length prefix. `FrameReader`'s doc has the failure
    // in full.
    let mut frames = noise::FrameReader::new();

    loop {
        // A bounded wait rather than a bare read, so a borrower that sits idle
        // mid-lease still gets its bearer renewed. Without it the renewal could
        // only ride a frame the borrower happened to send, and a borrower with
        // nothing to say is exactly the one whose token quietly expires.
        //
        // No spawned task: the Handoff has to go out on THIS stream, and a task
        // holding the stream would be a second writer on one Noise transport.
        let frame = match tokio::time::timeout(
            HANDOFF_POLL_INTERVAL,
            frames.recv_encrypted(stream, &mut session.transport),
        )
        .await
        {
            Ok(frame) => frame?,
            Err(_elapsed) => {
                push_due_handoffs(stream, session, context, store, &mut handed).await?;
                continue;
            }
        };
        let message: Control = serde_json::from_slice(&frame)
            .context("peer control: this frame is not a control message")?;

        // Re-read per frame: a grant revoked while this stream is open takes
        // effect on the next message, not at the next handshake. Through the
        // store's mtime check rather than an unconditional open, a revoke
        // moves the mtime, so the frame after it still reads the new file,
        // and a stream that sits there pinging stops re-reading bytes that did
        // not change (the review's L2).
        store.reload_if_changed();
        let file = store.file();
        let Some(row) = file.row(&session.peer) else {
            bail!(
                "peer control: this peer is no longer pinned; closing the live session \
                 (`tcr peer forget` takes effect within one frame)"
            );
        };

        match message {
            // **The refresh rule, and the reason a peer that moved is still
            // reachable.** Two facts land here and they are different kinds of
            // fact: the addresses the peer CLAIMS it listens on, and the
            // address this node SAW it from. Neither is identity: the session
            // that carried them already proved the pinned static key, so
            // both are recorded as advice, and the timestamp on each is this
            // node's own clock rather than anything off the wire.
            //
            // WireGuard's roaming rule, stated in its own words: a peer's
            // address is a side effect of a correctly authenticated packet
            // arriving, never a configured fact.
            Control::Hello(incoming) => {
                // The accepted socket goes through the same combine the
                // dialing side uses, and not onto the row whole, but only when
                // this stream's own transport connection is the peer's: its
                // HOST is the one fact this frame proves about where that peer
                // is, and its PORT is the ephemeral one the far side's kernel
                // picked for this connection, which answers nothing once it
                // closes. The rule for pairing the two is
                // [`crate::peer::config::endpoints_and_connection_from_hello`].
                // A carried Hello has no connection worth pairing: the socket
                // is the carrier's, so only what the Hello itself claims is
                // used, through [`config::endpoints_from_hello`].
                let learned = if let Some(socket) = origin.observed_socket() {
                    config::endpoints_and_connection_from_hello(&incoming.addrs, socket, now_ms())
                } else {
                    tracing::debug!(
                        peer = %session.peer.display(),
                        carrier = ?origin.carrier().map(|carrier| carrier.display()),
                        "peer control: this Hello arrived through a carrier; only the \
                         addresses it claims are used, never the carrier's own socket"
                    );
                    config::endpoints_from_hello(&incoming.addrs, now_ms())
                };
                // The two reflexive halves of this frame, recorded in memory
                // and never onto the row: the socket whole is the address a
                // NAT in front of that peer rewrote its packets to, so it is
                // advice for a punch and never an endpoint to dial. A carried
                // Hello teaches nothing here either: the socket belongs to the
                // carrier, not the peer.
                if let Some(socket) = origin.observed_socket() {
                    crate::peer::reach::remember_observed_peer(session.peer, socket);
                } else {
                    tracing::debug!(
                        peer = %session.peer.display(),
                        carrier = ?origin.carrier().map(|carrier| carrier.display()),
                        "peer control: not recording an observed address for a peer reached \
                         through a carrier"
                    );
                }
                let sees_us_at = match incoming.observed_you_at.as_deref() {
                    None => None,
                    Some(told) => match told.parse::<SocketAddr>() {
                        Ok(addr) => {
                            crate::peer::reach::remember_observed_self(session.peer, addr);
                            Some(addr)
                        }
                        Err(err) => {
                            tracing::debug!(
                                peer = %session.peer.display(),
                                told = %told,
                                error = %err,
                                "peer control: this peer said it sees us at something that is \
                                 not a socket address; ignoring the hint"
                            );
                            None
                        }
                    },
                };

                // **Every peers-file write this frame makes runs on a blocking
                // thread**, which is the rule [`on_blocking_thread`] states and
                // the one this arm did not keep. Four locked
                // read-modify-writes of the peers file ran inline on the
                // proxy's runtime, and `FileLock::acquire` waits for a
                // contended lock with `std::thread::sleep`: one Hello arriving
                // while anything else held that lock parked a worker thread for
                // up to [`crate::peer::config::LOCK_WAIT_MS`], and what stops
                // answering is the LOCAL proxy.
                //
                // One hop for all four rather than four hops: they take the
                // same lock in turn, so splitting them would buy nothing and
                // cost three more handoffs. Each write's own outcome comes back
                // so the failure an operator reads is still the sentence that
                // failure deserves.
                let written = on_blocking_thread({
                    let peers_path = context.peers_path.clone();
                    let peer = session.peer;
                    let secret = noise::port_secret(&session.handshake_hash);
                    let briefs = incoming.briefs.clone();
                    move || {
                        Ok(HelloWrites {
                            // The pair's rendezvous secret, recorded on the
                            // frame that already writes this file rather than
                            // on every accepted connection: a SERVE must still
                            // open the peers file exactly once
                            // (`an_accepted_connection_opens_the_peers_file_once`,
                            // `tests/peer_lease.rs`), and a Hello is the
                            // exchange that exists to answer "where and how do
                            // we meet next time".
                            rendezvous: config::observe_rendezvous_secret(
                                &peers_path,
                                &peer,
                                secret,
                            ),
                            // The sender goes with the briefs:
                            // `observe_neighbor_briefs` applies them only for a
                            // peer this node would brief in return
                            // (`allow.control.briefs`), which is the same flag
                            // `hello_for_peer` reads when it builds the
                            // outgoing side.
                            briefs: briefs.map(|briefs| {
                                crate::peer::discovery::observe_neighbor_briefs(
                                    &peers_path,
                                    &peer,
                                    &briefs,
                                    now_ms(),
                                )
                            }),
                            endpoints: config::observe_endpoints(&peers_path, &peer, &learned),
                            // And onto the row, so a restart still knows where
                            // this Mac is seen.
                            sees_us_at: sees_us_at.map(|addr| {
                                config::observe_seen_address(
                                    &peers_path,
                                    &peer,
                                    addr,
                                    u64::try_from(now_ms().max(0)).unwrap_or_default(),
                                )
                            }),
                        })
                    }
                })
                .await;
                match written {
                    Ok(written) => written.report(session.peer, origin.socket()),
                    Err(err) => tracing::warn!(
                        peer = %session.peer.display(),
                        error = %err,
                        "peer control: what this peer told us about where it answers was not \
                         recorded; it stays reachable at the endpoints already on its row"
                    ),
                }
                let mut facts = NodeFacts::listening(context.node, store.file().listen);
                facts.briefs = store.file().neighbor_briefs(session.peer);
                facts.observed_peer_at = origin.observed_socket();
                if facts.observed_peer_at.is_none() {
                    tracing::debug!(
                        peer = %session.peer.display(),
                        carrier = ?origin.carrier().map(|carrier| carrier.display()),
                        "peer control: telling this peer nothing about where it appeared to \
                         answer; the socket seen here is a carrier's"
                    );
                }
                let hello = hello_for_peer(&facts, &row.allow.control);
                let bytes = serde_json::to_vec(&Control::Hello(hello))
                    .context("peer control: the Hello did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
            }
            // **The punch's one message.** A Mac nobody can dial cannot be
            // told anything directly, so this frame usually arrives through a
            // mutual friend, blind, inside a TUNNEL. Everything it asks for is
            // decided by `reach::punch_request`, and what happens here is only
            // the spawn: the punch waits for a slot boundary, which is up to
            // thirty seconds, and this loop has other frames to read.
            //
            // A punched socket is served exactly as an accepted one is. The
            // roles are the protocol's and not TCP's: the side that ASKED for
            // the punch dials the handshake, so this side responds to it
            // whichever of the two sockets completed, and a stranger who
            // guessed the port meets the same pin check as anybody else.
            Control::PunchAt { slot, public_addr } => {
                match crate::peer::reach::punch_request(
                    session.peer,
                    slot,
                    &public_addr,
                    &row.endpoints,
                ) {
                    Ok((peer_ip, plan)) => {
                        let Some(in_flight) =
                            PunchInFlight::acquire(context.admission(), session.peer)
                        else {
                            tracing::info!(
                                peer = %session.peer.display(),
                                peer_addr = %origin.socket(),
                                "peer punch: this peer already has a punch in flight, so this \
                                 frame is dropped; one punch at a time is all a peer can be \
                                 waiting on"
                            );
                            continue;
                        };
                        let context = context.clone();
                        let peer = session.peer;
                        tokio::spawn(async move {
                            // Held for the whole task, not just the punch: the
                            // slot is free again once this peer's punch has
                            // either connected and been served or given up.
                            let _in_flight = in_flight;
                            let outcome = crate::peer::reach::punch(
                                &crate::peer::reach::KernelPunchNet,
                                &plan,
                                peer_ip,
                                crate::peer::reach::slot_window(),
                            )
                            .await;
                            match outcome {
                                Ok(punched) => {
                                    let from = SocketAddr::new(peer_ip, punched.port);
                                    let bind = SocketAddr::new(
                                        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                        punched.port,
                                    );
                                    if let Err(err) =
                                        serve_punched(punched.stream, context, from, bind).await
                                    {
                                        tracing::warn!(
                                            peer = %peer.display(),
                                            peer_addr = %from,
                                            error = %err,
                                            "peer punch: a punched connection did not serve"
                                        );
                                    }
                                }
                                Err(failure) => tracing::info!(
                                    peer = %peer.display(),
                                    failure = %failure,
                                    "peer punch: this peer asked for a punch and none of the slots got through"
                                ),
                            }
                        });
                    }
                    Err(failure) => tracing::info!(
                        peer = %session.peer.display(),
                        peer_addr = %origin.socket(),
                        failure = %failure,
                        "peer punch: this peer asked for a punch this node cannot join"
                    ),
                }
            }
            Control::Ping => {
                let bytes = serde_json::to_vec(&Control::Ping)
                    .context("peer control: the Ping did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
            }
            // **The echo is the whole of the responder's half.** Both fields go
            // back untouched: the nonce is what pairs an answer with its
            // question, and `sent_ms` is the ASKER's clock, so reading it here,
            // comparing it to this Mac's clock, or replacing it with this Mac's
            // clock would each turn a round trip into a measurement of how far
            // apart two NTP daemons are. The asker times its own probe against
            // its own monotonic clock (`crate::peer::probe`).
            //
            // Answered on the same authorization every other frame on this
            // stream has: a pinned static key, re-checked against a re-read row
            // at the top of this loop. A probe grants nothing and discloses
            // nothing this session has not already proved.
            Control::Probe { nonce, sent_ms } => {
                let bytes = serde_json::to_vec(&Control::ProbeAck { nonce, sent_ms })
                    .context("peer control: the ProbeAck did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
            }
            // An ack nobody on this side asked for. Logged and NOT refused:
            // probing runs on the dialling side, which reads its own acks
            // inline, so one arriving here is a peer's stray frame and closing
            // a working session over it would cost a cold prefix to say
            // nothing. It is not silence: the line names the peer, and no
            // measurement is recorded from a probe this node never sent.
            Control::ProbeAck { .. } => {
                tracing::debug!(
                    peer = %session.peer.display(),
                    peer_addr = %origin.socket(),
                    "peer control: a probe ack arrived on a stream this node never probed on; \
                     ignoring it"
                );
            }
            // **THE ARM THAT JOINS THE TWO HALVES OF LEASING.**
            //
            // Both halves existed for a long time and nothing connected
            // them: the borrower's `lease::request_lease` sends this frame and
            // `Ledger::grant` answers it, and in between sat the `other =>`
            // refusal below. So a borrowed request reached the lender, was
            // refused here, and the borrower fell through to its own honest 429,
            // which from the outside is indistinguishable from a lender that
            // was never wired at all. `tests/peer_e2e.rs` asserted exactly that
            // gap.
            //
            // **Both opt-ins are checked, and neither is checked here.** The
            // lender's half is `row.allow.inspect`, asked inside
            // `clamp_to_grant` (it refuses `InspectNotGranted`), against a row
            // re-read by `Ledger::grant`'s own `reload_if_changed`, so a grant
            // revoked a second ago refuses without a restart. The borrower's
            // half is `allow.disclose` and it is checked ON THE BORROWER before
            // it ever asks (`lease::request_lease`), because a lender that
            // enforced both would be pretending it can see the other Mac's
            // config. The scope and the `until` are read off
            // the operator's own `LendGrant` by the same function.
            //
            // A build with no `LeaseServing` REFUSES rather than answering a
            // grant it could not fund, the same shape every other ungranted
            // kind has.
            Control::LeaseRequest(ask) => {
                let Some(serving) = &context.serving else {
                    bail!(
                        "peer control: this peer asked for a lease and this build wired no \
                         lease ledger, so there is nothing to grant it against (a lender \
                         needs `listener::serve` to be given a `LeaseServing`)"
                    );
                };
                // The guard is dropped before the await below: a `MutexGuard`
                // held across a send is how one slow borrower stops every other
                // lease on this Mac.
                let granted = {
                    let mut held = serving.ledger.lock().map_err(|_| {
                        anyhow::anyhow!("peer control: the lease ledger's lock is poisoned")
                    })?;
                    held.grant(&session.peer, &ask, store, serving.utilization.as_ref())
                };
                // The MODE this lease was funded under, read off the grant the
                // ledger itself picked. Never looked up again here: the review's
                // first HIGH was this arm re-finding the grant by window alone,
                // which picked an ENDED `hand` grant beside the live `serve` one
                // that actually minted the lease. See [`lease::Granted`].
                let mode = granted.mode();
                // And the SCOPE the same call recorded beside the lease. Read
                // here for the review's M2: this arm used to ask the ledger
                // again through a second lock and read a poisoned one as
                // `LendScope::All`, which is a silent widening of the
                // operator's own grant to every account on this Mac. The
                // second lock is gone, so the fallback has nowhere to live.
                let scope = granted.scope;
                let grant = granted.answer;
                // One log line, and it names the peer, the window and the
                // outcome, never an account, never a scope. A scope names the
                // lender's own accounts and this line is read beside a borrower's
                // name; the scope is in `tcr peer ls --json`, which is the
                // lender's own surface.
                tracing::info!(
                    peer = %session.peer.display(),
                    window = ?ask.window,
                    granted = grant.lease.is_some(),
                    refusal = ?grant.refusal,
                    "peer control: answered a lease request"
                );
                // Copied before the grant moves into the frame: the Handoff
                // below is about the lease this answer just minted.
                // `Option<Lease>` is `Copy`, so this is a read and not a clone.
                let minted = grant.lease;
                let bytes = serde_json::to_vec(&Control::LeaseGrant(grant))
                    .context("peer control: the LeaseGrant did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
                // A HAND grant follows its lease with the owner's
                // short-lived bearer, on THIS session, which is the only
                // `IK`-authenticated one the grantee has. Every refusal about
                // the LEASE is in `lease::handoff_for` (serve mode, an ended
                // lease, no bearer the scope covers); the one about the SOCKET
                // is [`handoff_is_permitted`] and it is asked here, because the
                // pattern that ran is a property of this session and of
                // nothing the lease knows.
                if let Some(minted) = minted {
                    if mode == config::LendMode::Hand && !handoff_is_permitted(session.handshake) {
                        // Asked BEFORE the manager is, so a session that may
                        // not receive the bearer never causes it to be read
                        // out of the config at all. The lease itself stands:
                        // it was granted on its own merits and the borrower
                        // can spend it the serve way.
                        tracing::warn!(
                            peer = %session.peer.display(),
                            peer_addr = %origin.socket(),
                            handshake = ?session.handshake,
                            "peer control: a hand grant was answered on a session that is not \
                             a return visit to a pinned key, so the owner's bearer stays on \
                             this Mac"
                        );
                    } else if let Some(frame) = crate::peer::lease::handoff_for(
                        mode,
                        &minted,
                        serving.manager.handoff_bearer(&scope, minted.window),
                        now_ms(),
                    ) {
                        if let Control::Handoff {
                            lease_id,
                            expires_at_ms,
                            access_token,
                            utilization: _,
                        } = &frame
                        {
                            handed.insert(
                                *lease_id,
                                crate::peer::lease::HandedBearer {
                                    expires_at_ms: *expires_at_ms,
                                    fingerprint: crate::peer::lease::bearer_fingerprint(
                                        access_token.reveal(),
                                    ),
                                },
                            );
                        }
                        let bytes = serde_json::to_vec(&frame)
                            .context("peer control: the Handoff did not serialize")?;
                        noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
                    }
                }
            }
            // The borrower's own report of what a hand-mode response cost. It
            // can only raise `spent`; every refusal is in the ledger.
            Control::UsageHint { lease_id, spent } => {
                let Some(serving) = &context.serving else {
                    bail!("peer control: a usage hint arrived and this build wired no ledger");
                };
                let mut held = serving.ledger.lock().map_err(|_| {
                    anyhow::anyhow!("peer control: the lease ledger's lock is poisoned")
                })?;
                // Only the grantee of that lease may report against it.
                if held.grantee_of(lease_id) != Some(session.peer) {
                    bail!("peer control: a usage hint for a lease this peer does not hold");
                }
                let charged = held.apply_usage_hint(lease_id, spent);
                tracing::info!(
                    peer = %session.peer.display(),
                    charged,
                    "peer control: applied a borrower's usage hint"
                );
            }
            // **The other half of "both Macs moved".** A peer that changed
            // networks names its current addresses here, and this Mac answers
            // with its own, so one carried round trip leaves both of them with
            // somewhere to aim. After that the friend carries nothing.
            //
            // # Why this frame's address is believed and the socket under it is
            // not
            //
            // The session already proved this peer's pinned static key: a
            // carried CONTROL stream is a fresh end to end Noise session
            // nested inside a TUNNEL, which is what makes the carrier blind to
            // it and is stated on the wire type itself
            // (`tcr_peer_wire::Control`'s own doc). So the address in the
            // frame is routing advice from a proved sender, and a wrong one
            // costs a connect timeout while the handshake re-proves identity
            // at the far end regardless. The transport socket is the opposite
            // case: it is the CARRIER's connection to this node and proves
            // only the carrier's key, so it is read here for a LOG FIELD and
            // never for an address, which is what [`StreamOrigin`] exists to
            // keep true.
            Control::CollapseHint(hint) => {
                // A hint is about its sender, so a hint about somebody else is
                // refused before anything is recorded. A third party's address
                // is `NeighborBrief`'s job and travels under a grant
                // (`discovery::neighbor_brief_endpoints`); believing one on
                // this stream would be transitive trust with nothing behind
                // it, which is the recorded rule here and not a judgement this
                // arm gets to make.
                if hint.node != session.peer {
                    bail!(
                        "peer control: a CollapseHint is about its own sender and this one \
                         names {}, so the session is closed; a third party's address travels \
                         as a NeighborBrief under a grant",
                        hint.node.display()
                    );
                }
                if let Some(addr) = crate::peer::reach::learn_from_hint(&session.peer, &hint) {
                    tracing::info!(
                        peer = %session.peer.display(),
                        addr = %addr,
                        carried_on = %origin.socket(),
                        carrier = ?origin.carrier().map(|carrier| carrier.display()),
                        "peer control: this peer named where it is now, so a punch has \
                         somewhere to aim"
                    );
                } else {
                    tracing::debug!(
                        peer = %session.peer.display(),
                        carried_on = %origin.socket(),
                        carrier = ?origin.carrier().map(|carrier| carrier.display()),
                        "peer control: nothing this peer named is an address anything can \
                         dial, so it stays reachable at whatever was already recorded"
                    );
                }
                // Answered whether anything was learned or not: the asker
                // needs this Mac's addresses to aim at, and a peer whose own
                // claims were merely undialable has said nothing wrong.
                let answer = crate::peer::reach::own_collapse_hint(context.node, &session.peer);
                let bytes = serde_json::to_vec(&Control::CollapseHint(answer))
                    .context("peer control: the CollapseHint did not serialize")?;
                noise::send_encrypted(stream, &mut session.transport, &bytes).await?;
            }
            other => bail!(
                "peer control: {other:?} is not answered by this build (the lease lifecycle \
                 is phase 4)"
            ),
        }
    }
}
