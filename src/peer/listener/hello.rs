use std::net::SocketAddr;

use tcr_peer_wire::{
    Caps, Hello, Lendable, MAX_NEIGHBOR_BRIEFS, NeighborBrief, PROTO_VERSION, PeerId,
};

use crate::peer::config::ControlGrants;

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
        if let Some(node) = crate::peer::tailnet_node::advertised_listen() {
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
