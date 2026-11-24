use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tcr_peer_wire::PeerId;

use crate::peer::config::PeerFile;

// ---------------------------------------------------------------------------
// The caps, each one a number from `abuse-resistance.md` with its own gate
// ---------------------------------------------------------------------------

/// How often one address may knock: one per ten seconds, sustained.
///
/// `abuse-resistance.md`'s "knock flood from one address" row.
/// Over the bucket the socket is closed after message 1 with **zero bytes
/// written**, no queue entry and no UI row, so a flooder cannot tell a full
/// bucket from a machine that is simply not listening.
pub const KNOCK_INTERVAL_MS: i64 = 10_000;

/// How many knocks one address may make back to back before the interval binds.
/// Three, so a person pressing Trust, mistyping and pressing it again is never
/// rate-limited, and the fourth in ten seconds is.
pub const KNOCK_BURST: u32 = 3;

/// How many connections that have said NOTHING YET this node will hold at
/// once, across all addresses. `abuse-resistance.md`'s "resource exhaustion
/// before auth" row.
///
/// # What "yet" means here, because getting it wrong caps a real peer
///
/// The slot is held from `accept` to the moment message 1 has been read, and it
/// is released there, not when the handshake completes. That window is exactly
/// the threat the row names: a socket opened and left silent, which costs a
/// stranger one `connect` and costs this node a task and a buffer, bounded here
/// and by [`MESSAGE_1_TIMEOUT`].
///
/// **Holding it for the whole handshake is wrong, and it was measured wrong.**
/// The first version of this cap held the slot until the session authenticated,
/// and it silently limited a PINNED peer to two concurrent streams from one
/// address, so a lender serving a borrower's `max_inflight: 5` refused the
/// third with zero bytes and the borrower read it as "the handshake with the
/// lender failed". One TCP is one Noise session is one stream in this design
/// (`crate::peer::mod`), so concurrent streams from one peer are concurrent
/// connections from one address, and a pre-auth cap of two is a concurrency cap
/// of two. Caught by `borrowed_requests_are_paced_by_the_lenders_own_bucket`
/// (`tests/peer_lease.rs`), which fires three at once.
pub const MAX_UNAUTHENTICATED_SOCKETS: usize = 16;

/// How many of those one address may hold, **before the operator's own grants
/// are added**. Two, so a retry overlapping a stalled attempt still gets in and
/// a single host cannot take all sixteen.
///
/// The number a connection is really judged against is
/// [`unauthenticated_allowance`], which adds what this node has GRANTED. A
/// fixed two is right for a stranger and wrong for a peer the operator told
/// this node to serve five requests at once: three simultaneous `connect`s are
/// three accepted sockets before any of their tasks has read a byte, so a fixed
/// two refuses the third every time.
pub const MAX_UNAUTHENTICATED_PER_ADDRESS: usize = 2;

/// The two caps a connection is judged against: `(total, per address)`.
///
/// # Why these are derived and not constants
///
/// `abuse-resistance.md` gives 16 and 2, and both are right about a STRANGER.
/// Neither can bind a peer the operator configured, and a fixed 2 does: one TCP
/// is one Noise session is one stream (`crate::peer::mod`), so a borrower
/// granted `max_inflight: 5` opens five connections from one address, and every
/// one of them is "unauthenticated" for the microseconds between `accept` and
/// its task reading message 1. Three simultaneous `connect`s therefore collide
/// on a fixed cap of two **whatever the handshake costs**, which is why
/// narrowing the slot to the silent window was necessary and not sufficient.
///
/// So the floor is the doc's number and the LARGEST single grant is added on
/// top of both: with nothing granted this returns exactly `(16, 2)`, and the
/// numbers rise only because an operator asked for concurrency this node then
/// has to be able to accept. A stranger benefits from the raised ceiling too,
/// and that is the honest cost, bounded by the biggest burst one peer may
/// open, and a grant is the operator's act.
///
/// **The total was the SUM of every grant, and that was the review's L3**: a
/// node lending to six peers at five each carried a 46-socket pre-auth surface
/// instead of 16, so the ceiling an unrelated stranger met rose with grants
/// that had nothing to do with it. The largest grant is what the headroom is
/// for, which is what the per-address number always used.
///
/// Measured, not reasoned: with the fixed pair,
/// `borrowed_requests_are_paced_by_the_lenders_own_bucket`
/// (`tests/peer_lease.rs`) fails with the lender closing the third connection
/// with zero bytes and the borrower reporting "the handshake with the lender
/// failed"; it stays green on the largest-grant form.
pub fn unauthenticated_allowance(file: &PeerFile) -> (usize, usize) {
    // **The LARGEST grant, on both numbers**, the review's L3.
    //
    // The total used to be the SUM of every grant's `max_inflight`, so a node
    // lending to six peers at five each carried a 46-socket pre-auth surface
    // instead of 16, and every one of those thirty extra sockets was available
    // to an address with no grant at all: the ceiling rose with grants that had
    // nothing to do with the address connecting.
    //
    // The largest single grant is what the headroom is actually FOR. A slot is
    // held only until message 1 arrives (`serve_connection` drops it there, see
    // `MAX_UNAUTHENTICATED_SOCKETS`), so what has to fit is the burst one peer
    // can open at once, which is bounded by that peer's own cap, and the
    // per-address number has always been `MAX_UNAUTHENTICATED_PER_ADDRESS +
    // largest` for exactly this reason. Two numbers derived one way rather than
    // two.
    //
    // Measured, not reasoned, both before and after:
    // `borrowed_requests_are_paced_by_the_lenders_own_bucket`
    // (`tests/peer_lease.rs`) is the gate, with the bare constants it fails
    // with the lender closing the third connection with zero bytes, and it
    // stays green on the largest-grant form.
    let largest: usize = file
        .peers
        .iter()
        .flat_map(|row| row.lend.iter())
        .map(|grant| usize::from(grant.max_inflight))
        .max()
        .unwrap_or(0);
    (
        MAX_UNAUTHENTICATED_SOCKETS.saturating_add(largest),
        MAX_UNAUTHENTICATED_PER_ADDRESS.saturating_add(largest),
    )
}

/// How long a connection has to deliver message 1. Five seconds: a LAN round
/// trip is milliseconds, and a socket opened and left silent is the cheapest
/// thing a stranger can do.
pub const MESSAGE_1_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an authenticated session has to deliver its stream header.
///
/// The pre-authentication socket slot is released the moment the handshake
/// completes, so before this deadline existed a session that finished its
/// handshake and then said nothing held a task and a socket with nothing
/// counting it. A replayed `IK` message 1 reaches exactly that state without
/// the far side having to keep anything alive.
///
/// The same number as [`HANDSHAKE_TIMEOUT`], for the same reason: the header
/// is one frame a caller already has in hand when it dials.
pub const STREAM_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// How many authenticated sessions one peer may hold that have not delivered a
/// stream header yet.
///
/// The deadline above bounds how long each one lives; this bounds how many a
/// peer can open inside it. Per peer rather than node-wide, so one pinned Mac
/// that reconnects in a loop cannot close the door on the others.
pub const MAX_HEADERLESS_PER_PEER: usize = 4;

/// The largest knock frame this node will read. A knock is an instance id, a
/// short name and a version number; 512 bytes is generous for that and far
/// under [`tcr_peer_wire::MAX_FRAME_BYTES`], which is what an unauthenticated
/// sender would otherwise get to allocate.
pub const MAX_KNOCK_FRAME_BYTES: usize = 512;

/// How long the 120-second window an Accept opens really is, in seconds.
///
/// The same number as [`crate::peer::pair::PAIRING_WINDOW_SECS`] and named
/// through it rather than re-typed: two spellings of one deadline is how the
/// CLI and the listener come to disagree about whether a window is open.
pub const ACCEPTED_WINDOW_SECS: i64 = crate::peer::pair::PAIRING_WINDOW_SECS;

/// How many served request ids are remembered, per connection here, and, by
/// [`crate::peer::lease::SERVED_CAPACITY`], per process in the ledger.
///
/// Sized from the design's own figure: the accounting backstop has to outlive a
/// burst rather than a session. The ledger's cache is the one that actually
/// catches a replayed debit ([`RequestDedup`] says why); this figure is named
/// through to it so the two layers cannot disagree about what a burst is.
pub const REQUEST_DEDUP_CAPACITY: usize = 4096;

/// How long a served request id is remembered.
pub const REQUEST_DEDUP_TTL_MS: i64 = 600_000;

/// How long the listener stays silent, **per address**, after logging one
/// refusal.
///
/// One hour, which is `abuse-resistance.md`'s "log spam" row: "one log line per
/// address per hour for refusals". An earlier version was one line per MINUTE
/// for the whole listener, which is the wrong grain in both directions, a
/// single flooder silenced every other address's first refusal, and an hour of
/// slow scanning from a `/24` still wrote 254 lines a minute.
pub const REFUSAL_LOG_QUIET_MS: i64 = 3_600_000;

/// How many addresses the refusal log remembers. A map a stranger can grow
/// without bound is a memory bug with a security label, which is the same
/// sentence [`RequestDedup`] carries: an address that has not been refused
/// within [`REFUSAL_LOG_QUIET_MS`] is dropped, and above this cap the oldest
/// entry goes so the map cannot outgrow it even inside one hour.
pub const REFUSAL_LOG_ADDRESSES: usize = 1_024;

/// One log line per address per [`REFUSAL_LOG_QUIET_MS`], then silence, with
/// the count of what was silenced folded into the next line that is emitted, so
/// the bound never turns a flood into an absence of evidence.
///
/// Keyed by address, not global: see [`REFUSAL_LOG_QUIET_MS`]. The same type
/// serves the authenticated and the unauthenticated path, from two separate
/// instances, so a flood of anonymous refusals cannot silence a pinned peer's
/// stream ending, and neither one is unbounded.
#[derive(Debug, Default)]
pub struct RefusalLog {
    /// Per address: when a line was last emitted, and how many were silenced
    /// since.
    per_address: HashMap<String, (i64, u64)>,
}

impl RefusalLog {
    /// A log that has emitted nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this refusal should be logged. `Some(n)` means log it and that
    /// `n` refusals from THIS address were silenced since the previous line;
    /// `None` means stay silent.
    pub fn admit(&mut self, addr: &str, now_ms: i64) -> Option<u64> {
        // THIS address is resolved before anything is pruned. Pruning first was
        // a real defect and a quiet one: the entry for the address being
        // admitted aged out on the very call that was about to emit its line,
        // so the suppressed COUNT was dropped and the line said it stood for
        // nothing, a bound that turns a flood into an absence of evidence,
        // which is the failure this type exists to avoid.
        if let Some((last, suppressed)) = self.per_address.get_mut(addr) {
            if now_ms.saturating_sub(*last) < REFUSAL_LOG_QUIET_MS {
                *suppressed = suppressed.saturating_add(1);
                return None;
            }
            *last = now_ms;
            return Some(std::mem::take(suppressed));
        }
        // A new address, so now is the moment to drop the ones whose quiet hour
        // has passed: they carry no count worth keeping and their only cost is
        // the map entry.
        self.per_address
            .retain(|_, (last, _)| now_ms.saturating_sub(*last) < REFUSAL_LOG_QUIET_MS);
        while self.per_address.len() >= REFUSAL_LOG_ADDRESSES {
            // Bounded even inside one quiet window: drop the address whose last
            // line is oldest, which is the one a new refusal is least likely to
            // be silencing anything useful for.
            let Some(oldest) = self
                .per_address
                .iter()
                .min_by_key(|(_, (last, _))| *last)
                .map(|(addr, _)| addr.clone())
            else {
                break;
            };
            self.per_address.remove(&oldest);
        }
        self.per_address.insert(addr.to_string(), (now_ms, 0));
        Some(0)
    }

    /// How many addresses are remembered right now.
    pub fn len(&self) -> usize {
        self.per_address.len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.per_address.is_empty()
    }
}

/// A token bucket per source address: [`KNOCK_BURST`] tokens, one refilled
/// every [`KNOCK_INTERVAL_MS`].
///
/// Held as a last-refill instant plus a token count rather than a timer,
/// because a timer per address is a thing a flooder gets to create.
#[derive(Debug, Clone, Copy)]
struct KnockBucket {
    tokens: u32,
    last_ms: i64,
}

/// Everything the accept loop knows that is neither on disk nor in one
/// connection: the per-address knock buckets, the unauthenticated socket
/// counters, and the two refusal logs.
///
/// One instance per listener, shared by every connection task. Process state
/// rather than file state on purpose: a rate limit that survived a restart
/// would be a rate limit an operator could not clear, and a restart already
/// costs more than a knock does.
#[derive(Debug, Default)]
pub struct Admission {
    knocks: HashMap<String, KnockBucket>,
    live_per_address: HashMap<String, usize>,
    live_total: usize,
    unauthenticated_refusals: RefusalLog,
    authenticated_refusals: RefusalLog,
    punching: HashSet<PeerId>,
    headerless: HashMap<PeerId, usize>,
}

impl Admission {
    /// An empty one.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one knock token for `addr`, or report the bucket empty.
    ///
    /// Refills are computed from elapsed time rather than ticked, so an address
    /// that has been quiet for a minute finds a full bucket and one that is
    /// hammering finds an empty one, with no background task either way.
    pub fn take_knock_token(&mut self, addr: &str, now_ms: i64) -> bool {
        // An address whose bucket is full and idle is indistinguishable from an
        // address that has never knocked, so it is dropped: the map holds only
        // addresses that are currently spending.
        self.knocks.retain(|_, bucket| {
            let refilled = bucket
                .tokens
                .saturating_add(refill_tokens(bucket.last_ms, now_ms));
            refilled < KNOCK_BURST
        });
        let bucket = self.knocks.entry(addr.to_string()).or_insert(KnockBucket {
            tokens: KNOCK_BURST,
            last_ms: now_ms,
        });
        let refill = refill_tokens(bucket.last_ms, now_ms);
        if refill > 0 {
            bucket.tokens = bucket.tokens.saturating_add(refill).min(KNOCK_BURST);
            bucket.last_ms = bucket
                .last_ms
                .saturating_add(i64::from(refill).saturating_mul(KNOCK_INTERVAL_MS));
        }
        if bucket.tokens == 0 {
            return false;
        }
        bucket.tokens -= 1;
        true
    }

    /// How many connections from `addr` have not authenticated yet.
    ///
    /// Read by a gate, which is the only reason it is `pub`: the cap is
    /// enforced by `SocketSlot::acquire` against the same counter, so a test
    /// that inferred the count from timing would be measuring the scheduler.
    pub fn live_unauthenticated(&self, addr: &str) -> usize {
        self.live_per_address.get(addr).copied().unwrap_or(0)
    }

    /// How many connections across all addresses have not authenticated yet.
    pub const fn live_unauthenticated_total(&self) -> usize {
        self.live_total
    }

    /// How many authenticated sessions `peer` holds that have not delivered a
    /// stream header. For a gate to assert on, the same way
    /// [`Self::live_unauthenticated`] is.
    pub fn headerless_sessions(&self, peer: &PeerId) -> usize {
        self.headerless.get(peer).copied().unwrap_or(0)
    }

    /// How many peers hold an in-flight punch right now.
    ///
    /// Read by a gate, the same way [`Self::live_unauthenticated`] is: the cap
    /// is enforced by [`PunchInFlight::acquire`] against this set, so a test
    /// that inferred the count from timing would be measuring the scheduler.
    pub fn punches_in_flight(&self) -> usize {
        self.punching.len()
    }

    /// How many knock tokens `addr` has left. For a gate to assert on, and for
    /// the gauge `abuse-resistance.md` asks for instead of a log line.
    pub fn knock_tokens(&self, addr: &str, now_ms: i64) -> u32 {
        match self.knocks.get(addr) {
            None => KNOCK_BURST,
            Some(bucket) => bucket
                .tokens
                .saturating_add(refill_tokens(bucket.last_ms, now_ms))
                .min(KNOCK_BURST),
        }
    }

    /// Whether a refused connection from `addr` earns a log line, through the
    /// refusal log for its side of the handshake. See [`RefusalLog::admit`].
    pub(super) fn admit_refusal(
        &mut self,
        authenticated: bool,
        addr: &str,
        now_ms: i64,
    ) -> Option<u64> {
        let log = if authenticated {
            &mut self.authenticated_refusals
        } else {
            &mut self.unauthenticated_refusals
        };
        log.admit(addr, now_ms)
    }

    /// Count one more unauthenticated socket from `addr`, or report which cap
    /// refused it. [`SocketSlot::acquire`] is the one caller.
    fn take_socket_slot(
        &mut self,
        addr: &str,
        allowance: (usize, usize),
    ) -> Result<(), SocketRefusal> {
        let (total_allowed, per_address_allowed) = allowance;
        if self.live_total >= total_allowed {
            return Err(SocketRefusal::NodeFull {
                live: self.live_total,
                allowed: total_allowed,
            });
        }
        let per_address = self.live_per_address.get(addr).copied().unwrap_or(0);
        if per_address >= per_address_allowed {
            return Err(SocketRefusal::AddressFull {
                live: per_address,
                allowed: per_address_allowed,
            });
        }
        self.live_total += 1;
        *self.live_per_address.entry(addr.to_string()).or_insert(0) += 1;
        Ok(())
    }

    /// Give back a slot [`Self::take_socket_slot`] counted.
    fn release_socket_slot(&mut self, addr: &str) {
        self.live_total = self.live_total.saturating_sub(1);
        if let Some(count) = self.live_per_address.get_mut(addr) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.live_per_address.remove(addr);
            }
        }
    }

    /// Mark `peer` as punching. `false` when it already is.
    fn start_punch(&mut self, peer: PeerId) -> bool {
        self.punching.insert(peer)
    }

    /// Give back the punch [`Self::start_punch`] marked.
    fn end_punch(&mut self, peer: &PeerId) {
        self.punching.remove(peer);
    }

    /// Count one more headerless session for `peer`, or report how many it
    /// already holds when that is [`MAX_HEADERLESS_PER_PEER`].
    fn take_headerless(&mut self, peer: PeerId) -> Result<(), usize> {
        let live = self.headerless.get(&peer).copied().unwrap_or(0);
        if live >= MAX_HEADERLESS_PER_PEER {
            return Err(live);
        }
        *self.headerless.entry(peer).or_insert(0) += 1;
        Ok(())
    }

    /// Give back a session [`Self::take_headerless`] counted.
    fn release_headerless(&mut self, peer: &PeerId) {
        if let Some(count) = self.headerless.get_mut(peer) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.headerless.remove(peer);
            }
        }
    }
}

/// How many whole [`KNOCK_INTERVAL_MS`] intervals have passed. Saturating and
/// clamped to `u32`, because `now_ms` comes from a wall clock that can jump.
fn refill_tokens(last_ms: i64, now_ms: i64) -> u32 {
    let elapsed = now_ms.saturating_sub(last_ms).max(0);
    u32::try_from(elapsed / KNOCK_INTERVAL_MS).unwrap_or(u32::MAX)
}

/// One unauthenticated socket's slot in the two counters, released on drop.
///
/// RAII rather than a decrement at the end of the handler: the handler has a
/// dozen early returns (every refusal is one) and a counter that leaks on any
/// of them turns the cap into a permanent lockout after sixteen refused
/// connections, a denial of service built out of the defence against one.
pub(super) struct SocketSlot {
    admission: Arc<Mutex<Admission>>,
    addr: String,
}

impl SocketSlot {
    /// Take a slot, or report which cap refused it.
    ///
    /// `allowance` is [`unauthenticated_allowance`]'s pair, read from the peers
    /// file the caller already holds. See that function for why neither number
    /// can be a constant.
    pub(super) fn acquire(
        admission: &Arc<Mutex<Admission>>,
        addr: &str,
        allowance: (usize, usize),
    ) -> Result<Self, SocketRefusal> {
        lock_admission(admission).take_socket_slot(addr, allowance)?;
        Ok(Self {
            admission: Arc::clone(admission),
            addr: addr.to_string(),
        })
    }
}

impl Drop for SocketSlot {
    fn drop(&mut self) {
        lock_admission(&self.admission).release_socket_slot(&self.addr);
    }
}

/// One peer's in-flight punch, released on drop.
///
/// A `PunchAt` frame spawns a task that sleeps to the next slot boundary, up
/// to [`crate::peer::reach::slot_window`], and only then binds sockets.
/// Nothing counted those tasks, so a pinned peer could send the frame in a
/// loop and hold as many sleeping tasks and, at the boundary, as many socket
/// binds as it managed to send. One per peer is the natural bound: a peer
/// cannot be punching two slots at once, and the punch it is asking for
/// replaces nothing it already has in flight.
///
/// RAII for the reason [`SocketSlot`] is: the spawned task ends on a punch
/// that connected, on a punch that timed out and on a panic, and a set entry
/// that leaked on any of those would lock that peer out of punching for the
/// life of the process.
pub struct PunchInFlight {
    admission: Arc<Mutex<Admission>>,
    peer: PeerId,
}

impl PunchInFlight {
    /// Take `peer`'s one slot, or `None` when it already holds it.
    ///
    /// `pub` so a gate can drive the cap without a real punch: binding sockets
    /// and waiting out a slot boundary would measure the network rather than
    /// the bound.
    pub fn acquire(admission: &Arc<Mutex<Admission>>, peer: PeerId) -> Option<Self> {
        if !lock_admission(admission).start_punch(peer) {
            return None;
        }
        Some(Self {
            admission: Arc::clone(admission),
            peer,
        })
    }
}

impl Drop for PunchInFlight {
    fn drop(&mut self) {
        lock_admission(&self.admission).end_punch(&self.peer);
    }
}

/// One authenticated session that has not sent its stream header yet,
/// released on drop.
///
/// The counterpart to [`SocketSlot`] on the other side of the handshake: that
/// slot is given back the instant a session authenticates, and nothing counted
/// what happened next. [`MAX_HEADERLESS_PER_PEER`] is the cap and the same
/// RAII reason applies, since the header read has a deadline, a decode failure
/// and a dropped socket as its three endings.
pub struct HeaderlessSession {
    admission: Arc<Mutex<Admission>>,
    peer: PeerId,
}

impl HeaderlessSession {
    /// Take one of `peer`'s slots, or report how many it already holds.
    ///
    /// `pub` for the same reason [`PunchInFlight::acquire`] is: a gate can
    /// drive the cap without opening [`MAX_HEADERLESS_PER_PEER`] real
    /// handshakes.
    pub fn acquire(admission: &Arc<Mutex<Admission>>, peer: PeerId) -> Result<Self, usize> {
        lock_admission(admission).take_headerless(peer)?;
        Ok(Self {
            admission: Arc::clone(admission),
            peer,
        })
    }
}

impl Drop for HeaderlessSession {
    fn drop(&mut self) {
        lock_admission(&self.admission).release_headerless(&self.peer);
    }
}

/// Which pre-authentication cap refused a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SocketRefusal {
    /// The node-wide allowance is spent.
    NodeFull { live: usize, allowed: usize },
    /// This address's allowance is spent.
    AddressFull { live: usize, allowed: usize },
}

impl std::fmt::Display for SocketRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodeFull { live, allowed } => write!(
                f,
                "peer listener: {live} connections that have not delivered a message 1 are \
                 already open and the allowance is {allowed} \
                 (MAX_UNAUTHENTICATED_SOCKETS plus what this node has granted); closed \
                 with nothing written"
            ),
            Self::AddressFull { live, allowed } => write!(
                f,
                "peer listener: {live} connections from this address have not delivered a \
                 message 1 and the allowance is {allowed} \
                 (MAX_UNAUTHENTICATED_PER_ADDRESS plus the largest max_inflight this node \
                 has granted); closed with nothing written"
            ),
        }
    }
}

/// Take the admission lock, recovering a poisoned one.
///
/// A poisoned mutex means another connection task panicked while holding it.
/// Recovering is the safe side here: the counters may be one off, and the
/// alternative is a listener that refuses every connection for the rest of the
/// process's life over one panic.
pub(super) fn lock_admission(
    admission: &Arc<Mutex<Admission>>,
) -> std::sync::MutexGuard<'_, Admission> {
    match admission.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
