use super::*;

// ---------------------------------------------------------------------------
// The address the far side of a NAT sees this Mac at
// ---------------------------------------------------------------------------

/// Where each pinned peer last saw THIS Mac from, and where this Mac last saw
/// each peer from.
///
/// # Why a second register and not the peers file
///
/// An endpoint on a row is an address a peer ANSWERS on, written by
/// `observe_endpoints` and dialled by `dial_order`. What is here is a
/// different fact: the source address a connection ARRIVED from, which for a
/// Mac behind a NAT is the router's address and not anything that Mac listens
/// on. Writing it as an endpoint would put an address nothing accepts on into
/// the dial order, where it would cost a connect timeout on every dial.
///
/// It is the reflexive fact [`crate::peer::listener::NodeFacts::listening`]
/// says it does not carry, learned without a STUN server: the other end of an
/// authenticated session is the third party, and it already knows the answer.
///
/// In memory only, and per boot, exactly as the port-secret register was
/// before that gave it a file. Persisting it needs a field on
/// `PeerRow` (`src/peer/config.rs`), and it has none.
#[derive(Default)]
struct Observations {
    /// Keyed by peer: the address that peer reported seeing this Mac at.
    self_at: HashMap<tcr_peer_wire::PeerId, SocketAddr>,
    /// Keyed by peer: the source address a connection from that peer arrived
    /// from.
    peer_at: HashMap<tcr_peer_wire::PeerId, SocketAddr>,
}

fn observations() -> &'static Mutex<Observations> {
    static SEEN: OnceLock<Mutex<Observations>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(Observations::default()))
}

/// Take the register, recovering a poisoned lock rather than panicking: a
/// routing hint is never worth taking a process down for.
fn observations_locked() -> std::sync::MutexGuard<'static, Observations> {
    match observations().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Record that `peer` told this Mac it sees us at `addr`.
///
/// The value is ROUTING ADVICE and the session that carried it had already
/// proved the pinned static key, so it is recorded as told. A peer that lies
/// costs this node one failed punch attempt against an address nobody is
/// behind, which is the same cost as a stale endpoint.
pub fn remember_observed_self(peer: tcr_peer_wire::PeerId, addr: SocketAddr) {
    observations_locked().self_at.insert(peer, addr);
}

/// The address `peer` last said it sees this Mac at.
pub fn observed_self_for(peer: &tcr_peer_wire::PeerId) -> Option<SocketAddr> {
    observations_locked().self_at.get(peer).copied()
}

/// Record the source address a connection from `peer` arrived from.
pub fn remember_observed_peer(peer: tcr_peer_wire::PeerId, addr: SocketAddr) {
    observations_locked().peer_at.insert(peer, addr);
}

/// The address this Mac last saw `peer` arrive from.
pub fn observed_peer_for(peer: &tcr_peer_wire::PeerId) -> Option<SocketAddr> {
    observations_locked().peer_at.get(peer).copied()
}

/// Every peer this Mac has been told an address by, for the verb that prints
/// them. Sorted by peer id so two runs print the same order.
pub fn observed_self_addresses() -> Vec<(tcr_peer_wire::PeerId, SocketAddr)> {
    let mut rows: Vec<_> = observations_locked()
        .self_at
        .iter()
        .map(|(peer, addr)| (*peer, *addr))
        .collect();
    rows.sort_by_key(|(peer, _)| peer.0);
    rows
}

/// Drop both observations for `peer`. Answers whether anything was held.
pub fn forget_observations(peer: &tcr_peer_wire::PeerId) -> bool {
    let mut held = observations_locked();
    let had_self = held.self_at.remove(peer).is_some();
    let had_peer = held.peer_at.remove(peer).is_some();
    had_self || had_peer
}
