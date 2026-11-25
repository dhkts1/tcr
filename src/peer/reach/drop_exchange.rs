//! The dead drop's exchange: what this Mac publishes for each friend, and
//! what it fetches from a friend's drop and records.
//!
//! The crypto, the naming and the store client are
//! [`crate::peer::config::drop`]; this half reads the peers file, this Mac's
//! own addresses ([`crate::peer::reach`]) and the discovery rules, which is
//! why it sits above them here rather than beside the crypto.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tcr_peer_wire::PeerId;

use crate::peer::config::drop::{
    DROP_VERSION, DeadDropStore, DropKeys, DropName, DropRecord, MAX_RECORD_ENDPOINTS,
    accepted_slots, current_slot, open, seal,
};
use crate::peer::config::{Endpoint, EndpointSource, Observed};

// ---------------------------------------------------------------------------
// Publish and fetch
// ---------------------------------------------------------------------------

/// What this Mac believes it is reachable at, for a record to `peer`.
///
/// Built from [`crate::peer::reach::global_v6_addresses`],
/// [`crate::peer::reach::external_socket`] and
/// [`crate::peer::reach::observed_self_addresses`] filtered to `peer`, capped
/// at [`MAX_RECORD_ENDPOINTS`], de-duplicated, public IPv6 first: the three
/// sources are pushed in that order and a later duplicate of an address
/// already pushed is dropped rather than moved, so the order the sources are
/// read in is the order the record carries them.
///
/// `listen` supplies the port for the IPv6 addresses, which
/// `global_v6_addresses` does not carry: it answers which address the kernel
/// would source a connection from, not which port this Mac listens on.
///
/// An observed self address is published only when its PORT is one this Mac
/// actually accepts on: `listen.port()`, or a router mapping's external port.
/// `observed_self_addresses` is the source address a peer saw this Mac arrive
/// FROM, and this Mac dials out from an ephemeral port on every outbound
/// connection, so that source address routinely carries a port nothing is
/// listening on. Publishing it anyway spends a friend's dial budget on an
/// address that was never going to answer; a Mac reached over a mapped or
/// listening port instead teaches something that can.
pub fn own_endpoints(listen: SocketAddr, peer: &PeerId) -> Vec<SocketAddr> {
    let mut found: Vec<SocketAddr> = Vec::new();
    let mapped = crate::peer::reach::external_socket();

    for addr in crate::peer::reach::global_v6_addresses() {
        found.push(SocketAddr::new(IpAddr::V6(addr), listen.port()));
    }
    if let Some(mapped) = mapped {
        found.push(mapped);
    }
    let accepted_ports = [Some(listen.port()), mapped.map(|addr| addr.port())];
    for (node, addr) in crate::peer::reach::observed_self_addresses() {
        if &node == peer && accepted_ports.contains(&Some(addr.port())) {
            found.push(addr);
        }
    }

    let mut seen = std::collections::HashSet::new();
    found.retain(|addr| seen.insert(*addr));
    found.truncate(MAX_RECORD_ENDPOINTS);
    found
}

/// Publish this Mac's current address for one friend, in `slot`.
///
/// `Ok(false)` when nothing was published and that is not a failure: the
/// peer has no row here, the row lacks `allow.control.drop`, or the row has
/// no `rendezvous_secret` yet, the same three-way refusal
/// [`crate::peer::reach::rendezvous_ports`] gives a pair with no secret. The
/// cadence rules in the design doc (publish on slot roll or on address
/// change, a sixty-second floor between writes) are [`keep_drops_published`]'s
/// job, which calls this once per tick: this function itself always writes
/// when it is allowed to, so a caller deciding not to call it is the one
/// place the floor is enforced.
pub async fn publish_for<S: DeadDropStore + Sync>(
    store: &S,
    peers_path: &std::path::Path,
    us: &PeerId,
    friend: &PeerId,
    listen: SocketAddr,
    now_s: u64,
) -> Result<bool> {
    let file = crate::peer::config::read_or_default(peers_path)
        .context("dead drop: the peers file did not read, so nothing was published")?;
    let Some(row) = file.peers.iter().find(|row| &row.node == friend) else {
        return Ok(false);
    };
    if !row.allow.control.drop {
        return Ok(false);
    }
    let Some(secret) = row.rendezvous_secret else {
        return Ok(false);
    };

    let eps = own_endpoints(listen, friend);
    let slot = current_slot(now_s);
    let keys = DropKeys::derive(&secret);
    let name = DropName::for_slot(&keys, us, slot);
    let record = DropRecord {
        v: DROP_VERSION,
        publisher: *us,
        slot,
        at: now_s,
        eps,
    };
    let sealed = seal(&keys, &name, &record).context("dead drop: the record would not seal")?;
    store
        .put(&name, &sealed)
        .await
        .context("dead drop: the store refused the write")?;
    Ok(true)
}

/// Whether this process already fetched `friend`'s drop in `slot`.
///
/// A process-local register in `reach::port_secrets`'s shape, so a dial that
/// fails five times in one slot buys one store request and not
/// five: [`fetch_for`] checks this first and only calls the store when it
/// answers `false`, and records the attempt itself once it has decided there
/// is something to fetch.
pub fn fetched_this_slot(friend: &PeerId, slot: u64) -> bool {
    let held = match fetch_register().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.get(friend) == Some(&slot)
}

/// The process-local register [`fetched_this_slot`] reads and
/// [`remember_fetched`] writes.
fn fetch_register() -> &'static Mutex<HashMap<PeerId, u64>> {
    static REGISTER: OnceLock<Mutex<HashMap<PeerId, u64>>> = OnceLock::new();
    REGISTER.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that this process fetched `friend`'s drop in `slot`, so the next
/// call in the same slot is answered from the register rather than the
/// store.
fn remember_fetched(friend: PeerId, slot: u64) {
    let mut held = match fetch_register().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.insert(friend, slot);
}

/// Fetch one friend's record and record what it holds.
///
/// Tries [`accepted_slots`] of `current_slot(now_s)` in order and stops at
/// the first record that opens. Returns the endpoints that were WRITTEN, not
/// how many the record carried: [`crate::peer::discovery::admissible_drop_endpoints`]
/// decides that.
///
/// The caller that just re-taught a locator needs to know WHICH one, not
/// merely that something changed: a dial that cooled that same locator down a
/// moment earlier has to clear that cooldown and no other, or the fetch looks
/// like it fetched nothing. A count would make the caller re-derive the list
/// by re-reading the row and guessing which entries are new; returning the
/// list itself is the one place that already knows it.
///
/// A store that answers nothing is `Ok(vec![])`, not an error. Absence is
/// never read as "the friend is gone".
///
/// **A peer this node does not pin is refused before the store is ever
/// asked.** The row is read first; a missing row, or one with no
/// `rendezvous_secret`, returns `Ok(vec![])` with zero calls made against
/// `store`. A fetch racing `tcr peer forget` therefore touches neither the
/// file (the eventual write goes through
/// [`crate::peer::config::observe_endpoints`], which independently refuses to
/// create a row) nor the store.
pub async fn fetch_for<S: DeadDropStore + Sync>(
    store: &S,
    peers_path: &std::path::Path,
    friend: &PeerId,
    now_s: u64,
) -> Result<Vec<Endpoint>> {
    let slot = current_slot(now_s);
    if fetched_this_slot(friend, slot) {
        return Ok(Vec::new());
    }

    let file = crate::peer::config::read_or_default(peers_path)
        .context("dead drop: the peers file did not read, so nothing was fetched")?;
    let Some(row) = file.peers.iter().find(|row| &row.node == friend) else {
        return Ok(Vec::new());
    };
    let Some(secret) = row.rendezvous_secret else {
        return Ok(Vec::new());
    };

    remember_fetched(*friend, slot);

    let keys = DropKeys::derive(&secret);
    for candidate_slot in accepted_slots(slot) {
        let name = DropName::for_slot(&keys, friend, candidate_slot);
        let sealed = match store.get(&name).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(err) => {
                tracing::debug!(
                    peer = %friend.display(),
                    error = %err,
                    "dead drop: the fetch failed; the dial continues to Via exactly as it \
                     does today"
                );
                continue;
            }
        };
        let record = match open(&keys, &name, candidate_slot, friend, &sealed, now_s) {
            Ok(record) => record,
            Err(refusal) => {
                tracing::debug!(
                    peer = %friend.display(),
                    error = %refusal,
                    "dead drop: the fetched record was refused"
                );
                continue;
            }
        };

        let observed_at_ms = i64::try_from(now_s.saturating_mul(1_000)).unwrap_or(i64::MAX);
        let learned: Vec<Endpoint> = record
            .eps
            .iter()
            .map(|addr| Endpoint::direct(*addr, observed_at_ms, EndpointSource::Drop))
            .collect();
        let admissible = crate::peer::discovery::admissible_drop_endpoints(row, &learned);
        if admissible.is_empty() {
            return Ok(Vec::new());
        }
        return match crate::peer::config::observe_endpoints(peers_path, friend, &admissible)? {
            Observed::Written { .. } => Ok(admissible),
            Observed::NothingDialable | Observed::NoRow => Ok(Vec::new()),
        };
    }
    Ok(Vec::new())
}

/// Keep every switched-on friend's drop current, until shutdown.
///
/// [`crate::peer::reach::keep_internet_mapping`]'s shape, spawned beside it
/// in `server::boot_peer_listener` as `supervise("peer-dead-drop", ...)`. No
/// shutdown branch here: this loop runs forever, and shutdown is the
/// caller's `tokio::select!` against its own stop signal, unit 3b's job and
/// not this one's.
pub async fn keep_drops_published<S: DeadDropStore + Sync>(
    store: S,
    peers_path: std::path::PathBuf,
    us: PeerId,
    listen: SocketAddr,
    tick: Duration,
) {
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;

        let file = match crate::peer::config::read_or_default(&peers_path) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    path = %peers_path.display(),
                    error = %err,
                    "dead drop: the peers file did not read, so this tick published nothing"
                );
                continue;
            }
        };
        if !file.drop_is_live() {
            continue;
        }

        let now_ms = crate::now_ms().max(0);
        let now_s = u64::try_from(now_ms / 1_000).unwrap_or(0);
        let friends: Vec<PeerId> = file
            .peers
            .iter()
            .filter(|row| row.allow.control.drop)
            .map(|row| row.node)
            .collect();
        for friend in friends {
            if let Err(err) = publish_for(&store, &peers_path, &us, &friend, listen, now_s).await {
                tracing::warn!(
                    peer = %friend.display(),
                    error = %err,
                    "dead drop: publish failed; retrying at the next tick"
                );
            }
        }
    }
}
