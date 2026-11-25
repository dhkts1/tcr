//! The reverse carry: reaching a Mac nothing can dial.

use std::collections::VecDeque;

use anyhow::Result;
use tcr_peer_wire::PeerId;

use super::{MAX_OPEN_CARRIES_PER_PEER, dial_pinned_peer};
use crate::peer::config::{PeerRow, PeerStore};

/// How long a parked carrier is treated as usable.
///
/// Five minutes, which is five of the prober's own rounds
/// ([`crate::peer::probe::PROBE_INTERVAL`]): a keeper that stops opening
/// carriers loses its desk slot within a few probe windows of going quiet, and
/// a socket whose far end died without a FIN is dropped rather than spliced
/// into a requester's stream. It is a ceiling and never a liveness claim: the
/// only proof a parked socket still works is the splice itself, which is why
/// [`ReverseDesk::take`] hands out one carrier and forgets it.
pub const REVERSE_PARK_TTL_MS: i64 = 5 * 60 * 1000;

/// How many carriers one peer may keep parked here at once.
///
/// One TCP connection is one Noise session is one stream
/// ([`tcr_peer_wire::StreamKind`]'s own doc says why there is no multiplexer),
/// so a parked carrier serves exactly ONE forward and is then gone. This
/// number is therefore the concurrency an undialable friend gets on this Mac,
/// and it is [`MAX_OPEN_CARRIES_PER_PEER`] for that constant's own reason: a
/// session opens several requests at once, and a friend held to one would look
/// like a hang rather than a refusal.
pub const MAX_PARKED_PER_PEER: usize = MAX_OPEN_CARRIES_PER_PEER as usize;

/// One carrier a peer opened TO this node, waiting for a forward to ride back
/// out over it.
struct ParkedCarry {
    stream: crate::peer::noise::PeerStream,
    parked_at_ms: i64,
}

/// Why a carrier was not parked.
///
/// A typed answer rather than a bool, because the two cases are the operator's
/// two different problems: a Mac this node never pinned is a pairing that did
/// not happen, and a full desk is a friend opening carriers faster than
/// anything uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkRefusal {
    /// The peer is not a row in this Mac's peers file. The same rule
    /// [`authorize_forward`](super::authorize_forward) enforces on the other side of the same forward:
    /// nothing here holds a socket for a Mac this node has not pinned.
    NotPinned,
    /// This peer already holds [`MAX_PARKED_PER_PEER`] carriers here.
    DeskFull { open: usize },
}

/// What [`park_reverse_carry`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkOutcome {
    /// Parked, with how many this peer now holds.
    Parked { open: usize },
    /// Not parked, and the socket was dropped rather than held.
    Refused(ParkRefusal),
}

/// The carriers pinned Macs have opened to this node, one queue per peer.
///
/// # Why a desk and not a dial
///
/// Everything else in this file reaches a peer by opening a socket to an
/// address its row carries. A Mac behind a NAT that maps by destination has no
/// such address: nothing this node writes down can be dialled, and the row is
/// not stale, it is unusable by construction. What that Mac CAN do is open a
/// socket outwards, and a socket is bidirectional, so the one it opened is the
/// way in. This is where those sockets wait.
///
/// # It holds ciphertext and it is not a session
///
/// A parked carrier is a raw socket, spliced by [`handle_forward_on`](super::handle_forward_on) exactly
/// as a dialled one is: the payload is a fresh Noise session between the
/// REQUESTER and the peer that parked it, and this node holds no key for it.
/// So nothing here reads a byte, and a carrier that is used is used once,
/// because there is no multiplexer in this design and framing one here would
/// be inventing the component the wire crate's doc says does not exist.
pub struct ReverseDesk {
    open: std::collections::HashMap<PeerId, VecDeque<ParkedCarry>>,
}

impl Default for ReverseDesk {
    fn default() -> Self {
        Self::new()
    }
}

impl ReverseDesk {
    /// An empty desk.
    pub fn new() -> Self {
        Self {
            open: std::collections::HashMap::new(),
        }
    }

    /// Park one carrier, or say why not.
    ///
    /// The pinned check is the CALLER's ([`park_reverse_carry`]), because the
    /// peers file is the caller's to read and a desk that opened one would be
    /// a second reader of the same fact.
    fn park(
        &mut self,
        peer: PeerId,
        stream: crate::peer::noise::PeerStream,
        now_ms: i64,
    ) -> ParkOutcome {
        let queue = self.open.entry(peer).or_default();
        drop_expired(queue, now_ms);
        if queue.len() >= MAX_PARKED_PER_PEER {
            return ParkOutcome::Refused(ParkRefusal::DeskFull { open: queue.len() });
        }
        queue.push_back(ParkedCarry {
            stream,
            parked_at_ms: now_ms,
        });
        ParkOutcome::Parked { open: queue.len() }
    }

    /// Take the carrier this peer parked longest ago, if one is still inside
    /// [`REVERSE_PARK_TTL_MS`].
    ///
    /// Oldest first, which is the opposite of what a cache would do and right
    /// here: every carrier is equally able to carry, and the oldest is the one
    /// closest to its TTL, so spending it first is what keeps a desk from
    /// holding a socket until it expires unused.
    pub fn take(&mut self, peer: &PeerId, now_ms: i64) -> Option<crate::peer::noise::PeerStream> {
        let queue = self.open.get_mut(peer)?;
        drop_expired(queue, now_ms);
        queue.pop_front().map(|carry| carry.stream)
    }

    /// How many usable carriers this peer holds here, expired ones dropped.
    pub fn open_for(&mut self, peer: &PeerId, now_ms: i64) -> usize {
        match self.open.get_mut(peer) {
            Some(queue) => {
                drop_expired(queue, now_ms);
                queue.len()
            }
            None => 0,
        }
    }

    /// Drop every expired carrier on the desk, and report how many went.
    pub fn prune(&mut self, now_ms: i64) -> usize {
        let mut dropped = 0;
        for queue in self.open.values_mut() {
            let before = queue.len();
            drop_expired(queue, now_ms);
            dropped += before.saturating_sub(queue.len());
        }
        dropped
    }
}

/// Drop the carriers at the front of `queue` that are past their TTL.
///
/// A queue is parked in time order, so the expired ones are a prefix and this
/// is a `pop_front` loop rather than a scan of the whole queue.
fn drop_expired(queue: &mut VecDeque<ParkedCarry>, now_ms: i64) {
    while let Some(front) = queue.front() {
        if now_ms.saturating_sub(front.parked_at_ms) < REVERSE_PARK_TTL_MS {
            return;
        }
        queue.pop_front();
    }
}

/// The one desk this process keeps, for [`path_meter`](super::path_meter)'s reason: the
/// alternative is a handle threaded from boot through the listener and the
/// forwarder, and both ends of a parked carrier are handled by code that was
/// never given one.
pub fn reverse_desk() -> &'static std::sync::Mutex<ReverseDesk> {
    static DESK: std::sync::OnceLock<std::sync::Mutex<ReverseDesk>> = std::sync::OnceLock::new();
    DESK.get_or_init(|| std::sync::Mutex::new(ReverseDesk::new()))
}

/// Hold a carrier a pinned peer opened to this node, so a forward for that
/// peer has a way back out.
///
/// The admission is one question and it is the same one
/// [`authorize_forward`](super::authorize_forward) asks about a forward's target: is this Mac in the
/// peers file. A carrier from a stranger is dropped here, before it can take a
/// desk slot from a friend.
pub fn park_reverse_carry(
    store: &PeerStore,
    peer: PeerId,
    stream: crate::peer::noise::PeerStream,
    now_ms: i64,
) -> ParkOutcome {
    if store.row(&peer).is_none() {
        tracing::warn!(
            peer = %peer.display(),
            "peer reverse: a Mac this node has not pinned offered to keep a carrier here; dropping it"
        );
        return ParkOutcome::Refused(ParkRefusal::NotPinned);
    }
    let outcome = if let Ok(mut desk) = reverse_desk().lock() {
        desk.park(peer, stream, now_ms)
    } else {
        tracing::error!(
            peer = %peer.display(),
            "peer reverse: the carrier desk's lock is poisoned, so this carrier is dropped \
             and that peer stays unreachable from here until restart"
        );
        ParkOutcome::Refused(ParkRefusal::DeskFull { open: 0 })
    };
    match outcome {
        ParkOutcome::Parked { open } => tracing::debug!(
            peer = %peer.display(),
            open,
            "peer reverse: holding a carrier for a Mac that cannot be dialled"
        ),
        ParkOutcome::Refused(ParkRefusal::DeskFull { open }) => tracing::warn!(
            peer = %peer.display(),
            open,
            "peer reverse: this peer already holds every carrier slot here; dropping the offer"
        ),
        ParkOutcome::Refused(ParkRefusal::NotPinned) => {}
    }
    outcome
}

/// How a forward got to its target.
///
/// Reported for the same reason [`crate::peer::tunnel::dial::Reached`] is: the meter
/// and the log line both need to name the path, and a boxed stream cannot be
/// asked afterwards which one it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachedTarget {
    /// A socket this node opened to one of the row's own addresses.
    Direct(std::net::SocketAddr),
    /// A carrier the target itself opened to this node and parked.
    Reverse,
}

impl ReachedTarget {
    /// The one word a log line names this path by.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Direct(_) => "direct",
            Self::Reverse => "reverse",
        }
    }
}

/// A parked carrier for this target, or a dial of its own addresses, in that
/// order.
///
/// # The order is the whole of the reverse story
///
/// A parked carrier is proof the target was alive and able to reach this Mac
/// as recently as [`REVERSE_PARK_TTL_MS`] ago, and for the Mac it exists for,
/// the one nothing can dial, the row's addresses are not stale but unusable.
/// Dialling first would spend the borrow's whole timeout proving that before
/// reaching for the socket that was already open. A target that is dialable
/// parks nothing, so this costs it one lock and no round trip.
pub(super) async fn reach_target(
    row: &PeerRow,
    now_ms: i64,
) -> Option<(ReachedTarget, crate::peer::noise::PeerStream)> {
    if let Ok(mut desk) = reverse_desk().lock() {
        if let Some(parked) = desk.take(&row.node, now_ms) {
            tracing::debug!(
                peer = %row.node.display(),
                "peer forward: this target parked a carrier here, so the forward rides back \
                 out over it and nothing is dialled"
            );
            return Some((ReachedTarget::Reverse, parked));
        }
    } else {
        tracing::error!(
            peer = %row.node.display(),
            "peer forward: the carrier desk's lock is poisoned, so a parked carrier cannot be \
             read and this forward falls back to dialling the row"
        );
    }
    let (addr, stream) = dial_pinned_peer(row).await?;
    Some((ReachedTarget::Direct(addr), stream))
}

/// Whether this node needs pinned friends to carry for it at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverseNeed {
    /// Nothing dialable was found, so a friend's open socket is the way in.
    Wanted,
    /// This node can be dialled, and says how.
    NotWanted(ReverseNotWanted),
}

/// The reason a node needs no carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverseNotWanted {
    /// A router mapping is held, so the public address in it is dialable.
    HoldsMapping,
    /// This Mac has a global IPv6 address, which needs no mapping at all.
    HasGlobalV6,
}

/// Does this node need a friend to hold a carrier for it.
///
/// A pure function over the two facts `reach` already measures, and it takes
/// them rather than reading them, so the answer can be tested against both
/// states without a router, a socket or a NAT. The caller is the process that
/// holds [`crate::peer::reach::MappingKeeper`], which is the only thing that
/// knows whether the mapping it asked for exists this minute.
///
/// A mapping outranks IPv6 in this answer only in which reason is reported:
/// either one alone means a peer can open a socket to this Mac, and a node
/// that asked a friend to carry anyway would be spending a third Mac's bytes
/// on a path it does not need.
pub const fn reverse_carry_is_wanted(
    held: Option<&crate::peer::reach::Mapping>,
    global_v6: &[std::net::Ipv6Addr],
) -> ReverseNeed {
    if held.is_some() {
        return ReverseNeed::NotWanted(ReverseNotWanted::HoldsMapping);
    }
    if !global_v6.is_empty() {
        return ReverseNeed::NotWanted(ReverseNotWanted::HasGlobalV6);
    }
    ReverseNeed::Wanted
}

/// The pinned Macs this node may ask to hold a carrier for it, in the order to
/// ask them.
///
/// [`crate::peer::tunnel::dial::forwarders_for`] with this node's own id as the
/// target, and that is not a trick: "who may carry a stream to X" with X set
/// to self IS "who may carry for me", it already excludes this Mac and any
/// candidate with no direct address of its own, and its order is the prober's.
/// A second list here would be a second answer to one question, and the two
/// would drift the first time `paths.viaAllow` changed.
pub fn reverse_carriers(store: &PeerStore, node: &PeerId) -> Vec<PeerRow> {
    crate::peer::tunnel::dial::forwarders_for(node, store)
        .into_iter()
        .filter_map(|peer| store.row(&peer))
        .collect()
}

/// How often a serving process re-asks whether it can be dialled.
///
/// The answer moves for one reason: a router mapping this node asked for at
/// boot and got seconds later, or lost. Five seconds, the same cadence the
/// internet switch is re-read at, because the two facts come from the same
/// keeper.
pub const REVERSE_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// What the reverse-carry supervisor should do this wake.
///
/// A typed answer over the two facts, what `reach` measures now and whether
/// carriers are parked, so both can be decided without a router or a friend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverseStep {
    /// Nothing can dial this Mac and no friend is holding a carrier for it:
    /// ask them.
    Park,
    /// This Mac can be dialled and friends are holding carriers: let them go.
    Release,
    /// What is held already matches what is needed.
    Idle,
}

/// Decide [`ReverseStep`] from the need as `reach` measures it NOW and whether
/// carriers are parked.
///
/// # Why this is asked more than once
///
/// It used to be asked once, two statements after the mapping keeper thread
/// was spawned and before its first NAT-PMP round trip could publish anything.
/// So a Mac that its router would map perfectly well read `None`, decided
/// nothing could dial it, and parked `MAX_PARKED_PER_PEER` carriers at every
/// friend it had, seconds before it became dialable. The boot test binds
/// loopback, where no mapping is ever asked for, so the ordering was
/// unobserved.
pub const fn reverse_step(need: ReverseNeed, parked: bool) -> ReverseStep {
    match (need, parked) {
        (ReverseNeed::Wanted, false) => ReverseStep::Park,
        (ReverseNeed::NotWanted(_), true) => ReverseStep::Release,
        (ReverseNeed::Wanted, true) | (ReverseNeed::NotWanted(_), false) => ReverseStep::Idle,
    }
}

/// What one run of [`watch_reverse_need`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReverseWatch {
    /// Times carriers were asked for.
    pub parked: usize,
    /// Times they were released because this Mac became dialable.
    pub released: usize,
}

/// Keep the reverse carriers in step with whether anything can dial this Mac.
///
/// `need_now` is asked each wake rather than once, which is the whole point:
/// see [`reverse_step`]. `park` opens the carriers and hands back whatever
/// stops them again; `release` takes that back and stops them.
///
/// `rounds` bounds the loop so a test can drive it; [`None`] is the production
/// shape, which is forever. `held` is what `park` produced before this loop
/// started, or [`None`].
pub async fn watch_reverse_need<T, N, P, R>(
    every: std::time::Duration,
    rounds: Option<usize>,
    need_now: N,
    mut park: P,
    mut release: R,
    mut held: Option<T>,
) -> ReverseWatch
where
    N: Fn() -> ReverseNeed,
    P: FnMut() -> Option<T>,
    R: FnMut(T),
{
    let mut tally = ReverseWatch::default();
    let mut ticker = tokio::time::interval(every);
    // `interval`'s first tick is immediate and this loop takes the BOOT answer
    // on it: a Mac that cannot be dialled has to ask its friends at boot, not
    // five seconds into it, and its caller no longer decides anything itself.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut round = 0_usize;
    loop {
        if let Some(limit) = rounds
            && round >= limit
        {
            return tally;
        }
        round += 1;
        ticker.tick().await;

        match reverse_step(need_now(), held.is_some()) {
            ReverseStep::Idle => {}
            ReverseStep::Park => {
                held = park();
                tally.parked += 1;
            }
            ReverseStep::Release => {
                if let Some(held) = held.take() {
                    release(held);
                }
                tally.released += 1;
                tracing::info!(
                    "peer reverse: this Mac can be dialled now, so the friends holding a \
                     carrier for it are let go"
                );
            }
        }
    }
}

/// What one friend's keeper loop did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReverseKeeperRun {
    /// Carriers that were opened, parked, and ran to completion.
    pub carried: usize,
    /// Attempts that never became a parked carrier.
    pub failed: usize,
}

/// Keep one friend supplied with a carrier: open one, hold it until the friend
/// is done with it, open the next.
///
/// # Why this is a loop and not a keepalive
///
/// A parked carrier is a raw socket the friend splices; a byte this side wrote
/// into it to prove liveness would land in the middle of somebody's carried
/// session. So nothing is sent on a parked carrier at all: it is opened, it
/// waits, it is spent by one forward, and this loop opens the next. That is
/// what "keeps it alive" means here, and the probe cadence
/// ([`crate::peer::probe::PROBE_INTERVAL`]) is the caller's `retry` value, the
/// spacing between attempts when the friend cannot be reached at all.
///
/// # The seam
///
/// `run_one` opens one carrier to `friend` and returns when that carrier is
/// finished, however it finished. It is a parameter because the production
/// version writes a stream header the peer wire does not carry yet (the
/// variant and the listener arm that reads it are both missing), while
/// everything this function decides, how many, how often, what a failure
/// costs, is testable with a closure and no socket at all.
///
/// `runs` bounds the loop so a test can drive it; `None` is the production
/// shape, which is forever.
pub async fn keep_reverse_carrier<F, Fut>(
    friend: PeerRow,
    retry: std::time::Duration,
    runs: Option<usize>,
    run_one: F,
) -> ReverseKeeperRun
where
    F: Fn(PeerRow) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut tally = ReverseKeeperRun::default();
    let mut round = 0_usize;
    loop {
        if let Some(limit) = runs
            && round >= limit
        {
            return tally;
        }
        round += 1;
        match run_one(friend.clone()).await {
            Ok(()) => {
                tally.carried += 1;
                tracing::debug!(
                    friend = %friend.node.display(),
                    carried = tally.carried,
                    "peer reverse: a carrier this node parked at a friend was spent; opening the next"
                );
            }
            Err(err) => {
                tally.failed += 1;
                // Surfaced with the friend's own refusal text, because every
                // way this fails, an unreachable friend, a full desk, a peers
                // file that does not pin this node, is a sentence the operator
                // has nowhere else to read.
                tracing::warn!(
                    friend = %friend.node.display(),
                    error = %err,
                    "peer reverse: this friend is not holding a carrier for us"
                );
                if !retry.is_zero() {
                    tokio::time::sleep(retry).await;
                }
            }
        }
    }
}
