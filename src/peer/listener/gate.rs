use std::collections::VecDeque;

use anyhow::Result;
use tcr_peer_wire::{PeerId, StreamHeader, StreamKind, TunnelTarget};

use crate::peer::config::{PeerRow, PeerStore};
use crate::peer::noise::Handshake;

use crate::peer::lease::{REQUEST_DEDUP_CAPACITY, REQUEST_DEDUP_TTL_MS};

/// May the owner's bearer go out on a session this handshake produced?
///
/// The `Handoff` frame travels on an `IK` session and nothing
/// else, and until a review asked, nothing checked it. `IK` ([`Handshake::Return`])
/// is the only pattern that proves the dialler holds the private half of a key
/// this Mac has PINNED, which is the whole of "the grantee and nobody else":
///
/// - `XX` ([`Handshake::Pair`]) proves a static key that this Mac has not
///   pinned yet; the six digits are compared by a human afterwards.
/// - `IKpsk1` ([`Handshake::Enrol`]) proves an invite secret, which is a
///   one-use admission to the mesh and not an identity this Mac lends to.
/// - `NN` and `NNpsk0` ([`Handshake::Knock`], [`Handshake::KnockPsk`]) carry no
///   static key at all.
///
/// # This is a second lock on a door that is already shut
///
/// `serve_stream` returns a `Pair` session at the six-digit line and an `Enrol`
/// session at `serve_enrolment`, so no pattern other than `Return` reaches
/// `serve_control` in this build, and no integration test can drive one there.
/// That is exactly why the rule is written as a value a test can ask about:
/// the day a pattern is added, or one of those early returns becomes a
/// fall-through, this answers the same way it does today.
pub const fn handoff_is_permitted(handshake: Handshake) -> bool {
    match handshake {
        Handshake::Return => true,
        Handshake::Pair | Handshake::Enrol | Handshake::Knock | Handshake::KnockPsk => false,
    }
}

/// The ONE authorization function for an inbound peer stream, checked in this
/// order:
///
/// 1. the handshake has been read to the point where the initiator's static is
///    known, and [`crate::peer::noise::pin_check`] agrees, **before message 2
///    is written**;
/// 2. [`StreamHeader::kind`] is granted for that peer, per its row's `allow`;
/// 3. [`StreamHeader::hops_remaining`] is above zero;
/// 4. this node's own id is not already in [`StreamHeader::via`];
/// 5. [`StreamHeader::request_id`] is not in THIS CONNECTION's dedup cache.
///
/// Order matters: every later check reads a field a peer controls, so the pin
/// check has to answer first.
///
/// **Check 5 cannot fire as the serving path is wired**, and the review's H2b
/// is that saying otherwise sends the next reader hunting for a defence that is
/// somewhere else. [`RequestDedup::new`] is called per `serve_stream`, one
/// stream carries one request, and [`peer_stream_gate_hop`] is asked exactly
/// once against a cache that is empty every time. The cross-connection
/// defence (a diamond, or a replay on a second connection) is the ledger's
/// `served`/`debited` caches ([`crate::peer::lease::Ledger`]), which are
/// process-lifetime and are what actually makes one request one debit. This
/// check is the per-connection half and is kept as one: it costs nothing and
/// it is the half that survives the day a stream carries two requests.
///
/// **Checks 4 and 5 are not reachable through this signature**, and that is
/// reported to the lead rather than papered over: check 4 compares `via`
/// against THIS NODE'S OWN ID, which a [`PeerStore`] does not carry, and check
/// 5 needs the request-id cache, which is process state and not file state.
/// They are [`peer_stream_gate_hop`], the single implementation of those two,
/// and the serving path calls it immediately after this function. The two
/// functions share no check, so there is nothing here that can drift out of
/// step with a copy.
pub fn peer_stream_gate(
    peer: &PeerId,
    header: &StreamHeader,
    store: &PeerStore,
) -> Result<(), StreamRefusal> {
    let row = store.row(peer);
    peer_stream_gate_rows(header, row.as_ref())
}

/// Checks 1 to 3 against the pinned row itself: `None` is an unpinned peer,
/// which is the same answer as "not authorized for anything".
pub const fn peer_stream_gate_rows(
    header: &StreamHeader,
    row: Option<&PeerRow>,
) -> Result<(), StreamRefusal> {
    // Check 2 runs on the kind before the grant is read, because an unknown
    // kind has no grant to read: refusal is the only safe answer, never a
    // degrade to a kind this build does know.
    if let StreamKind::Unknown(raw) = header.kind {
        return Err(StreamRefusal::UnknownKind(raw));
    }
    let Some(row) = row else {
        return Err(StreamRefusal::NotGranted(header.kind));
    };
    let granted = match (header.kind, &header.target) {
        // A pinned peer may always open CONTROL. What it is TOLD there is
        // assembled from its own `allow.control` grants by
        // [`hello_for_peer`], so an ungranted control stream learns booleans
        // and addresses and nothing else.
        (StreamKind::Control, _) => true,
        // A pinned Mac may park a carrier, and being pinned is the whole
        // admission. There is no grant to read because parking asks this node
        // for nothing it has not already agreed to: the socket is the far
        // Mac's, it is spent only by a forward that `allow.relay` has to admit
        // on its own terms, and what it costs here is a desk slot bounded by
        // `tunnel::MAX_PARKED_PER_PEER`. `park_reverse_carry` asks the same
        // question again against its own store, the belt and braces SERVE
        // documents, because a row can be forgotten between this frame and the
        // park.
        (StreamKind::Park, _) => true,
        (StreamKind::Serve, _) => row.allow.inspect,
        (StreamKind::Tunnel, Some(TunnelTarget::Origin { .. })) => row.allow.gateway,
        (StreamKind::Tunnel, Some(TunnelTarget::Peer { .. })) => row.allow.relay,
        // A tunnel with no target is not something this node can carry.
        (StreamKind::Tunnel, None) => false,
        (StreamKind::Unknown(_), _) => false,
    };
    if !granted {
        return Err(StreamRefusal::NotGranted(header.kind));
    }
    if header.hops_remaining == 0 {
        return Err(StreamRefusal::HopsExhausted);
    }
    Ok(())
}

/// Checks 4 and 5: the loop stamp and the dedup cache.
///
/// Separate from [`peer_stream_gate_rows`] only because the two pieces of state
/// they read (this node's own id, and a process-lifetime cache) are not in a
/// [`PeerStore`]. Called immediately after it, so the documented order holds.
pub fn peer_stream_gate_hop(
    node: &PeerId,
    header: &StreamHeader,
    dedup: &mut RequestDedup,
    now_ms: i64,
) -> Result<(), StreamRefusal> {
    if header.via.contains(node) {
        return Err(StreamRefusal::LoopDetected);
    }
    if !dedup.admit(header.request_id, now_ms) {
        return Err(StreamRefusal::Duplicate);
    }
    Ok(())
}

/// The request ids already served **on one connection**, for check 5.
///
/// Exists for ACCOUNTING, never for the replay defence: a `via` stamp cannot
/// catch a diamond, two disjoint paths converging on one terminal, and a
/// diamond served twice is two debits for one request. Bounded by
/// [`REQUEST_DEDUP_CAPACITY`] and [`REQUEST_DEDUP_TTL_MS`], because a cache a
/// peer can grow without bound is a memory bug with a security label.
///
/// # This cache is per connection, and a diamond does not arrive on one
///
/// Said plainly here rather than left for a third reader to
/// re-derive: [`Self::new`] is called inside `serve_stream`, so each accepted
/// stream gets an EMPTY cache, one stream carries one request, and
/// [`peer_stream_gate_hop`] asks [`Self::admit`] once, which therefore always
/// answers `true`. The two-debits-for-one-request hole this was filed against
/// is closed one layer down and process-wide, at the ledger's `served` and
/// `debited` caches ([`crate::peer::lease::Ledger::enter_relay`] and
/// [`crate::peer::lease::Ledger::debit`]), whose bounds are this type's own
/// ([`crate::peer::lease::SERVED_CAPACITY`] names them through it, so the two
/// layers cannot disagree about what a burst is).
///
/// So what lives here is the per-connection half: real the day a stream carries
/// more than one request, and not the thing standing between this lender and a
/// replayed debit today.
#[derive(Debug, Default)]
pub struct RequestDedup {
    seen: VecDeque<(u128, i64)>,
}

impl RequestDedup {
    /// An empty cache.
    pub const fn new() -> Self {
        Self {
            seen: VecDeque::new(),
        }
    }

    /// Record `id` and report whether it is new. `false` means this request has
    /// already been served here.
    pub fn admit(&mut self, id: u128, now_ms: i64) -> bool {
        self.seen
            .retain(|(_, at)| now_ms.saturating_sub(*at) < REQUEST_DEDUP_TTL_MS);
        if self.seen.iter().any(|(seen, _)| *seen == id) {
            return false;
        }
        while self.seen.len() >= REQUEST_DEDUP_CAPACITY {
            self.seen.pop_front();
        }
        self.seen.push_back((id, now_ms));
        true
    }

    /// How many ids are remembered right now.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Why a stream was refused. **Every arm closes the connection and logs.**
///
/// [`Self::UnknownKind`] is the one that must never degrade to anything else: a
/// newer peer on the same LAN will speak a kind this build does not know, the
/// PARSE deliberately survives that ([`StreamKind::Unknown`]), and a handler
/// that then treated it as a tunnel would be an open relay. Refusal is the only
/// safe answer here, the inverse of `ProxyHost::Unknown`
/// (`src/singleton.rs:105-125`), where degrading is what keeps a live proxy
/// from being signalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamRefusal {
    /// The kind is not granted for this peer. The default for every grant is
    /// false, so this is what an unconfigured mesh answers to everything.
    NotGranted(StreamKind),
    /// A kind this build does not know.
    UnknownKind(u16),
    /// Hop budget spent.
    HopsExhausted,
    /// Our own id is already in `via`, a cycle, killed outright.
    LoopDetected,
    /// This request id has already been served **on this connection**. Exists
    /// for ACCOUNTING: a via stamp cannot catch a diamond (two disjoint paths
    /// converging on one terminal), and a diamond served twice is two debits
    /// for one request.
    ///
    /// Unreachable as the serving path is wired, one stream, one request, one
    /// empty cache, and the diamond is caught process-wide by the ledger
    /// instead. See [`RequestDedup`], which carries the whole of the review's
    /// H2b.
    Duplicate,
}

impl std::fmt::Display for StreamRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGranted(kind) => write!(f, "peer stream refused: {kind:?} is not granted"),
            Self::UnknownKind(raw) => write!(
                f,
                "peer stream refused: stream kind {raw} is unknown to this build"
            ),
            Self::HopsExhausted => write!(f, "peer stream refused: hop budget spent"),
            Self::LoopDetected => write!(f, "peer stream refused: this node is already in `via`"),
            Self::Duplicate => write!(f, "peer stream refused: request id already served here"),
        }
    }
}

impl std::error::Error for StreamRefusal {}
