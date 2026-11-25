use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use tcr_peer_wire::{Control, Hello, PeerId, StreamHeader, StreamKind};

use crate::peer::config::{Locator, PeerStore};
use crate::peer::id::NodeKey;
use crate::peer::noise::{self, Handshake, recv_control, send_control};

use super::dial::dial_peer_with_endpoint;
use crate::peer::config::node_key_dir;
use crate::peer::state::peer_state_path;

/// Say hello to one pinned peer: tell it where this node listens, and record
/// where it says it listens.
///
/// # Why this exists, and why nothing else could do its job
///
/// `Hello.addrs` had a producer and no sender. The listener ANSWERS a `Hello`
/// with its own ([`crate::peer::listener::hello_for_peer`]), so the address a
/// node announces only ever travels on the answering side, and the machine
/// that needs to announce a new address is the one that MOVED, which is the
/// side that dials. This is that frame.
///
/// Both halves of the refresh happen on one round trip. Outbound, this node's
/// listen socket reaches the peer, which records it against this node's row.
/// Inbound, the peer's own listen socket reaches this node, together with the
/// fact that the dial to this endpoint worked at all, so the endpoint that
/// answered is re-dated and leads the list next time.
///
/// Returns the peer's `Hello`, or `None` when the peer is not pinned or
/// nothing answered on any of its endpoints. A dial that fails is not an
/// error: a laptop asleep is the normal state of this fleet, and the endpoints
/// already on the row stay exactly as they were.
pub async fn say_hello(store: &PeerStore, peer: &PeerId) -> Result<Option<Hello>> {
    store.reload_if_changed();
    let Some(row) = store.row(peer) else {
        return Ok(None);
    };
    // The socket the dial actually landed on comes back with the stream: it is
    // the one fact here this node observed rather than believed, and a boxed
    // stream cannot be asked for it afterwards.
    let Some((reached, mut stream)) = dial_peer_with_endpoint(&row).await else {
        return Ok(None);
    };

    let key = NodeKey::load_or_mint(&node_key_dir(store))
        .context("peer hello: this node has no keypair to greet a peer with")?;
    let mut session = noise::dial_handshake(
        &mut stream,
        key.secret_bytes(),
        Handshake::Return,
        Some(&peer.0),
        None,
    )
    .await
    .context("peer hello: the handshake with the peer failed")?;

    // The dialling half of the same record the listener writes on its own side:
    // both ends derive this from the handshake they both ran, and each keeps
    // its own copy so either can still meet the other after a restart.
    if let Err(err) = crate::peer::config::observe_rendezvous_secret(
        store.path(),
        peer,
        noise::port_secret(&session.handshake_hash),
    ) {
        tracing::warn!(
            peer = %peer.display(),
            error = %err,
            "peer hello: could not record this pair's rendezvous secret",
        );
    }

    let header = StreamHeader {
        kind: StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: crate::peer::id::random_id()?,
    };
    send_control(&mut stream, &mut session, &header).await?;

    let file = store.file();
    let mut facts = crate::peer::listener::NodeFacts::listening(key.id(), file.listen);
    facts.briefs = crate::peer::discovery::neighbor_briefs(&file.peers, *peer);
    // Deliberately NOT set on this side. `observed_you_at` means "the source
    // address your packets arrived from", which only the accepting end
    // observes; what a dialler holds is the address it CHOSE to dial, and
    // sending that would give the field two meanings and hand a peer its own
    // LAN address as a public one.
    let mine = crate::peer::listener::hello_for_peer(&facts, &row.allow.control);
    send_control(&mut stream, &mut session, &Control::Hello(mine)).await?;

    let answer: Control = recv_control(&mut stream, &mut session).await?;
    let Control::Hello(theirs) = answer else {
        bail!("peer hello: the peer answered a Hello with {answer:?}");
    };

    // What the far side sees this Mac at. The dialling half of the pair of
    // records the listener writes on its own side, and the only thing in this
    // function that could not be derived here: a Mac behind a NAT cannot see
    // its own public address, and the peer that just answered can.
    if let Some(told) = theirs.observed_you_at.as_deref() {
        match told.parse::<SocketAddr>() {
            Ok(addr) => {
                crate::peer::reach::remember_observed_self(*peer, addr);
                // Onto the row too: see the listener's own copy of this arm.
                if let Err(err) = crate::peer::config::observe_seen_address(
                    store.path(),
                    peer,
                    addr,
                    u64::try_from(crate::now_ms().max(0)).unwrap_or_default(),
                ) {
                    tracing::warn!(
                        peer = %peer.display(),
                        error = %err,
                        "peer hello: could not record where this peer sees us",
                    );
                }
            }
            Err(err) => tracing::debug!(
                peer = %peer.display(),
                told = %told,
                error = %err,
                "peer hello: this peer said it sees us at something that is not a socket address; ignoring the hint"
            ),
        }
    }
    // And where this Mac saw IT: the socket the dial landed on is the peer's
    // own address seen from here, which is what a punch back at it would aim
    // for when the recorded endpoints stop answering.
    crate::peer::reach::remember_observed_peer(*peer, reached);

    if let Some(briefs) = theirs.briefs.as_deref()
        && let Err(err) = crate::peer::discovery::observe_neighbor_briefs(
            store.path(),
            peer,
            briefs,
            crate::now_ms(),
        )
    {
        tracing::warn!(
            peer = %peer.display(),
            error = %err,
            "peer hello: could not record a neighbor brief from this peer",
        );
    }

    let now = crate::now_ms();
    // The peer's own claimed addresses, plus the host this dial actually
    // reached on the port that hello announced: see
    // [`crate::peer::config::endpoints_and_connection_from_hello`] for why an
    // unconditional push of `reached` would leave a dead entry on the row next
    // to the address the peer already named.
    let learned =
        crate::peer::config::endpoints_and_connection_from_hello(&theirs.addrs, reached, now);
    // Every outcome is said out loud at debug, because two of the three are a
    // peer this node now holds no fresh way to reach, and a silent write path
    // makes that look identical to a successful one.
    match crate::peer::config::observe_endpoints(store.path(), peer, &learned)
        .context("peer hello: could not record where this peer answers")?
    {
        crate::peer::config::Observed::Written { added } => tracing::debug!(
            peer = %peer.display(),
            added,
            "peer hello: recorded where this peer answers",
        ),
        crate::peer::config::Observed::NothingDialable => tracing::debug!(
            peer = %peer.display(),
            "peer hello: nothing this peer named is an address anything can dial, so its row \
             keeps the endpoints it already had",
        ),
        crate::peer::config::Observed::NoRow => tracing::debug!(
            peer = %peer.display(),
            "peer hello: this peer is no longer pinned here, so nothing was recorded for it",
        ),
    }

    // The probe rides this session for free: it already paid the round trip a
    // fresh dial would cost, and the endpoint just learned above and the cost
    // of reaching it belong in the same write. The table is cloned out of its
    // lock rather than probed in place, because `probe::with_table` holds a
    // `std::sync::Mutex` and probing awaits the network between rounds; one
    // round, on the constants the module already declares.
    let locator = Locator::Direct { addr: reached };
    match crate::peer::probe::with_table(|table| table.clone()) {
        Ok(mut table) => {
            match crate::peer::probe::probe_session(
                &mut stream,
                &mut session,
                &mut table,
                locator,
                1,
                crate::peer::probe::PROBE_INTERVAL,
            )
            .await
            {
                Ok(run) => {
                    tracing::debug!(
                        peer = %peer.display(),
                        sent = run.sent,
                        stop = ?run.stop,
                        "peer hello: probed this session before it closed",
                    );
                    if let Err(err) =
                        crate::peer::probe::with_table(|installed| *installed = table.clone())
                    {
                        tracing::warn!(
                            peer = %peer.display(),
                            error = %err,
                            "peer hello: could not update the path table with this session's \
                             probe",
                        );
                    }
                    let state_path = peer_state_path(store.path());
                    if let Err(err) = table.save(&state_path) {
                        tracing::warn!(
                            peer = %peer.display(),
                            error = %err,
                            "peer hello: could not persist the measured path to the state file",
                        );
                    }
                }
                Err(err) => tracing::debug!(
                    peer = %peer.display(),
                    error = %err,
                    "peer hello: probing this session failed, and the endpoints learned above \
                     still stand",
                ),
            }
        }
        Err(err) => tracing::warn!(
            peer = %peer.display(),
            error = %err,
            "peer hello: could not read the path table, so this session was not probed",
        ),
    }

    Ok(Some(theirs))
}
