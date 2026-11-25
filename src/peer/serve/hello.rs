use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use tcr_peer_wire::{
    Caps, Control, Hello, Lendable, MAX_NEIGHBOR_BRIEFS, NeighborBrief, PROTO_VERSION, PeerId,
    StreamHeader, StreamKind,
};

use crate::peer::config::{ControlGrants, Locator, PeerStore};
use crate::peer::id::NodeKey;
use crate::peer::noise::{self, Handshake, recv_control, send_control};
use crate::peer::reach::probe;

use crate::peer::config::node_key_dir;
use crate::peer::state::peer_state_path;
use crate::peer::tunnel::dial::dial_peer_with_endpoint;

/// Say hello to one pinned peer: tell it where this node listens, and record
/// where it says it listens.
///
/// # Why this exists, and why nothing else could do its job
///
/// `Hello.addrs` had a producer and no sender. The listener ANSWERS a `Hello`
/// with its own ([`hello_for_peer`]), so the address a
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
    let mut facts = NodeFacts::listening(key.id(), file.listen);
    facts.briefs = file.neighbor_briefs(*peer);
    // Deliberately NOT set on this side. `observed_you_at` means "the source
    // address your packets arrived from", which only the accepting end
    // observes; what a dialler holds is the address it CHOSE to dial, and
    // sending that would give the field two meanings and hand a peer its own
    // LAN address as a public one.
    let mine = hello_for_peer(&facts, &row.allow.control);
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
    match probe::with_table(|table| table.clone()) {
        Ok(mut table) => {
            match probe::probe_session(
                &mut stream,
                &mut session,
                &mut table,
                locator,
                1,
                probe::PROBE_INTERVAL,
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
                    if let Err(err) = probe::with_table(|installed| *installed = table.clone()) {
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

/// Everything this node could tell a peer about itself, before any grant is
/// consulted.
///
/// A struct rather than nine parameters, and deliberately **not** the `Hello`
/// itself: the optional blocks are assembled into a `Hello` by
/// [`hello_for_peer`] from the grants, so an ungranted figure is never
/// constructed into a message and then filtered out on the way past. A filter
/// is something a later edit forgets to apply.
pub struct NodeFacts {
    /// This node's id.
    pub node: PeerId,
    /// The operator's display name for this machine, already sanitized.
    pub label: String,
    /// Monotonic per sender.
    pub seq: u64,
    /// The three booleans every pinned peer is told.
    pub caps: Caps,
    /// Addresses this node can be reached on.
    pub addrs: Vec<String>,
    /// How long this advert may be believed.
    pub ttl_s: u32,
    /// Per-window lendable amounts. Needs `control.lendable`.
    pub lendable: Vec<Lendable>,
    /// Hops from here to egress. Needs `control.lendable`.
    pub hops_to_egress: Option<u8>,
    /// One level of neighbour briefs. Needs `control.briefs`.
    pub briefs: Vec<NeighborBrief>,
    /// This build's sha. Needs `control.diag`.
    pub build_sha: String,
    /// This boot's id. Needs `control.diag`.
    pub boot_id: u64,
    /// The source address this node saw the peer these facts are being
    /// assembled FOR arrive from, when it has seen one.
    ///
    /// The one field here that is about the reader. It is what makes
    /// [`tcr_peer_wire::Hello::observed_you_at`] answerable, and it is per
    /// peer, so it is set by the caller that knows which peer this Hello is
    /// for rather than read from a register inside the assembly.
    pub observed_peer_at: Option<SocketAddr>,
}

impl NodeFacts {
    /// The facts a node has before any of phase 4's accounting exists: an id,
    /// no capabilities, nothing countable.
    ///
    /// Announces no address either, which is why it is not what a listener
    /// sends: see [`Self::listening`].
    pub fn minimal(node: PeerId) -> Self {
        Self {
            node,
            label: String::new(),
            seq: 0,
            caps: Caps::default(),
            addrs: Vec::new(),
            ttl_s: HELLO_TTL_S,
            lendable: Vec::new(),
            hops_to_egress: None,
            briefs: Vec::new(),
            build_sha: String::new(),
            boot_id: 0,
            observed_peer_at: None,
        }
    }
}

impl NodeFacts {
    /// [`Self::minimal`], plus the socket this node actually listens on.
    ///
    /// **This is `Hello.addrs`'s producer, and for a long time there was
    /// none.** The field has been on the wire since the skeleton with nothing
    /// filling it, so a peer that moved had no way to say so and the far side
    /// went on dialling the address it was paired over until that failed.
    ///
    /// Two addresses at most, and never every address of every interface: the
    /// configured listen socket, which is what a peer on this LAN acts on, and
    /// the router mapping this node holds when `peer.internet` is on
    /// ([`crate::peer::reach::external_socket`]), which is what a peer off it
    /// acts on. Both are sockets a peer can dial. Reflexive discovery, meaning
    /// what a peer SEES this node as from the far side of a NAT it did not map
    /// itself, is still a different mechanism and is not here.
    pub fn listening(node: PeerId, listen: Option<SocketAddr>) -> Self {
        let mut facts = Self::minimal(node);
        facts.addrs = listen.map_or_default(|addr| vec![addr.to_string()]);
        // tcr's own tailnet node, when this process runs one: its address at
        // the peer port. After the configured socket for the same reason the
        // mapping is, and before the mapping, because it needs no router.
        if let Some(node) = crate::peer::reach::tailnet::node::advertised_listen() {
            let node = node.to_string();
            if !facts.addrs.contains(&node) {
                facts.addrs.push(node);
            }
        }
        // The router mapping, when this node holds one. It goes AFTER the
        // configured socket rather than before it: a peer on the same LAN
        // should reach the LAN address first, and the mapped one is what is
        // left for a peer that is not.
        if let Some(mapped) = crate::peer::reach::external_socket() {
            let mapped = mapped.to_string();
            if !facts.addrs.contains(&mapped) {
                facts.addrs.push(mapped);
            }
        }
        facts
    }
}

/// How long a `Hello` may be believed, in seconds.
const HELLO_TTL_S: u32 = 60;

/// Assemble the `Hello` one peer is entitled to.
///
/// Assembled from the grant set rather than filtered on the way out. Without
/// the three `allow.control` booleans, pinning a peer would silently subscribe
/// it to per-window lendable amounts with account counts, this build's sha, its
/// boot id, and a list of this node's other peers.
///
/// **No credential and nothing countable can reach this message by accident**:
/// `tcr_peer_wire::Hello` has no token field and must never gain one, and every
/// countable block here is behind its own grant.
pub fn hello_for_peer(facts: &NodeFacts, grants: &ControlGrants) -> Hello {
    Hello {
        proto: PROTO_VERSION,
        node: facts.node,
        label: facts.label.clone(),
        seq: facts.seq,
        caps: facts.caps,
        addrs: facts.addrs.clone(),
        ttl_s: facts.ttl_s,
        lendable: grants.lendable.then(|| facts.lendable.clone()),
        hops_to_egress: grants.lendable.then_some(facts.hops_to_egress).flatten(),
        briefs: grants.briefs.then(|| {
            facts
                .briefs
                .iter()
                .take(MAX_NEIGHBOR_BRIEFS)
                .cloned()
                .collect()
        }),
        build_sha: grants.diag.then(|| facts.build_sha.clone()),
        boot_id: grants.diag.then_some(facts.boot_id),
        // Behind no grant: it is the reader's own public address, told back to
        // it, and a Mac that is told nothing here cannot punch at all.
        observed_you_at: facts.observed_peer_at.map(|addr| addr.to_string()),
    }
}
