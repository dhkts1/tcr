use super::*;
use crate::peer::noise::port_secret_for;

// ---------------------------------------------------------------------------
// The punch: two Macs dialling each other at a moment they both computed
// ---------------------------------------------------------------------------

/// How many slots a punch spends before it gives up.
///
/// Three, the number the dial order names, and the same three the
/// derivation already accepts on either side of a boundary. Each one costs a
/// slot's width of waiting, so a fourth buys a case the first three did not:
/// a NAT that refused the first two will refuse the fourth for the same
/// reason.
pub const PUNCH_SLOTS: u32 = 3;

/// One attempt of a punch: when to start, and on which port.
///
/// Both ends compute this from the same pair secret and the same clock and get
/// the same list, which is the whole reason a punch needs no coordination
/// message beyond "start at slot N".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchSlot {
    /// The slot number, which is what one side names to the other.
    pub slot: u64,
    /// The port both ends bind and dial in this slot ([`derived_port`]).
    pub port: u16,
    /// When this slot opens, in unix milliseconds. Absolute, because the two
    /// sides are agreeing on an instant and a duration from "now" is two
    /// different instants.
    pub opens_at_unix_ms: i64,
}

/// The next `slots` slots, each with the port this pair punches on.
///
/// # Why it never starts in the slot that is already open
///
/// The slot in progress is part spent: one side may compute the plan with 29
/// seconds left in it and the other with 200 milliseconds left, and the second
/// one would bind, dial once and move on before the first had started. The
/// next boundary is an instant both sides can name without knowing how far
/// through the current slot the other one is. It costs up to one slot of
/// waiting, which is the cost of the two of them being in the same slot at
/// all.
pub fn punch_plan(secret: &[u8; 32], now_unix_ms: i64, slots: u32) -> Vec<PunchSlot> {
    let now_seconds = u64::try_from(now_unix_ms.max(0)).unwrap_or(0) / 1_000;
    let first = current_slot(now_seconds).saturating_add(1);
    (0..u64::from(slots))
        .map(|step| {
            let slot = first.saturating_add(step);
            let opens_at_unix_ms = slot
                .saturating_mul(SLOT_SECONDS)
                .saturating_mul(1_000)
                .try_into()
                .unwrap_or(i64::MAX);
            PunchSlot {
                slot,
                port: derived_port(secret, slot),
                opens_at_unix_ms,
            }
        })
        .collect()
}

/// The plan the other side computes when it is told "punch at slot N".
///
/// Named separately from [`punch_plan`] because the receiving side must not
/// re-derive the starting slot from its own clock: the point of the number on
/// the wire is that one side chose it. A slot already past is not extended
/// into the future here either; the caller gets the slots it was told about
/// and [`punch`] skips the ones whose instant has gone.
pub fn punch_plan_from_slot(secret: &[u8; 32], first: u64, slots: u32) -> Vec<PunchSlot> {
    (0..u64::from(slots))
        .map(|step| {
            let slot = first.saturating_add(step);
            let opens_at_unix_ms = slot
                .saturating_mul(SLOT_SECONDS)
                .saturating_mul(1_000)
                .try_into()
                .unwrap_or(i64::MAX);
            PunchSlot {
                slot,
                port: derived_port(secret, slot),
                opens_at_unix_ms,
            }
        })
        .collect()
}

/// Why a punch did not happen, or did not work.
///
/// Every arm is a NAME a dial can print, which is the half of the gate that
/// says "never hangs": a punch that fails has to say which of these it was, so
/// an operator reading one line knows whether to fix an address, pair the two
/// Macs again, or stop expecting a direct path from this network at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PunchFailure {
    /// No peer has ever told this Mac where it sees that peer, and the row
    /// holds no dialable endpoint either, so there is no address to aim at.
    /// The ordinary answer for a pair that has only ever met on one LAN with
    /// no direct locator recorded.
    PeerAddressUnknown,
    /// No completed handshake with this peer since boot and no secret on its
    /// row, so the pair cannot compute a port. The pair still shares a key:
    /// one session fixes it.
    NoRendezvousSecret,
    /// Every slot opened, both sides dialled, and nothing connected.
    ///
    /// **This is what a symmetric NAT looks like from in here.** A NAT that
    /// assigns a fresh external port per destination (RFC 4787 calls the
    /// mapping address dependent) never maps the port the pair derived, so the
    /// far side's dial arrives at a port nothing is behind, in every slot,
    /// forever. The punch cannot tell that from a Mac that is simply asleep,
    /// so it names what it observed and how many times.
    NoSlotConnected {
        /// How many slots were actually waited out.
        slots_tried: u32,
        /// The port each of those slots was on, so a reader can check it
        /// against a packet capture.
        ports: Vec<u16>,
    },
    /// Nothing has ever told this Mac what address IT is reachable at, so a
    /// punch could ask the peer to aim at nothing. One session with any pinned
    /// peer on this build fixes it.
    SelfAddressUnknown,
    /// No pinned Mac could carry the ask to the peer, so the peer will not be
    /// in the slot. The punch is not attempted rather than spent on an empty
    /// slot.
    NotAnnounced,
    /// A peer asked for a punch and gave an address that is not one.
    ///
    /// Kept, rather than dropped as a malformed frame: the session it arrived
    /// on is authenticated, so this is a pinned Mac on a build that disagrees
    /// with this one about a format, which is worth a line naming what it sent.
    AddressNotUnderstood {
        /// What the peer sent, as it sent it.
        told: String,
    },
}

impl std::fmt::Display for PunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PeerAddressUnknown => write!(
                f,
                "punch: no peer has told this Mac an address for that one, and the row \
                 holds no dialable endpoint either, so there is nothing to punch at"
            ),
            Self::NoRendezvousSecret => write!(
                f,
                "punch: this process holds no rendezvous secret for that peer, so the two \
                 cannot compute a port; one completed session records it"
            ),
            Self::SelfAddressUnknown => write!(
                f,
                "punch: no peer has told this Mac its own public address yet, so there is nothing to ask that peer to aim at"
            ),
            Self::NotAnnounced => write!(
                f,
                "punch: no pinned Mac could carry the ask to that peer, so it would not be in the slot"
            ),
            Self::AddressNotUnderstood { told } => write!(
                f,
                "punch: that peer asked for a punch from `{told}`, which is not a socket address"
            ),
            Self::NoSlotConnected { slots_tried, ports } => write!(
                f,
                "punch: {slots_tried} slots opened on ports {ports:?} and neither side got \
                 through; a NAT that hands out a fresh port per destination cannot be \
                 punched through and a sleeping Mac looks the same from here"
            ),
        }
    }
}

impl std::error::Error for PunchFailure {}

/// The address to aim a punch at from a row's own endpoints, when the
/// observation register has nothing.
///
/// Three rules and nothing else: only a locator this node can dial itself
/// ([`crate::peer::config::Endpoint::direct_addr`]), a preference for one
/// whose IP is not LAN scope over one that is
/// ([`is_lan_scope`] reads a tailnet address as
/// internet scope, which is correct here: a tailnet address is directly
/// dialable), and otherwise the row's own order, which is newest observation
/// first. Every direct locator on the row qualifies whatever its band: each
/// got there under a rule that already proved it.
pub fn aimable_address(endpoints: &[crate::peer::config::Endpoint]) -> Option<SocketAddr> {
    let dialable: Vec<SocketAddr> = endpoints
        .iter()
        .filter_map(crate::peer::config::Endpoint::direct_addr)
        .collect();
    dialable
        .iter()
        .find(|addr| !is_lan_scope(addr.ip()))
        .or_else(|| dialable.first())
        .copied()
}

/// What a punch at `peer` needs before it starts: an address to aim at and the
/// pair's secret.
///
/// One function rather than two lookups at the call site, because "can these
/// two punch" is one question with two named answers, and a caller that asked
/// it in two places would print two different sentences for the same state.
///
/// The address comes from either of two places. The per boot observation
/// register first, because a source address a connection just arrived from is
/// the freshest fact there is; the row's own endpoints when the register has
/// nothing, because the register is never persisted and is empty for every
/// peer after any restart, move or no move, while the row may already hold
/// that peer's current public address from a moved link or a fetched dead
/// drop.
pub fn punch_target(
    peer: &tcr_peer_wire::PeerId,
    endpoints: &[crate::peer::config::Endpoint],
) -> Result<(IpAddr, [u8; 32]), PunchFailure> {
    let seen = observed_peer_for(peer)
        .map(|addr| addr.ip())
        .or_else(|| aimable_address(endpoints).map(|addr| addr.ip()));
    let Some(ip) = seen else {
        return Err(PunchFailure::PeerAddressUnknown);
    };
    let Some(secret) = port_secret_for(peer) else {
        return Err(PunchFailure::NoRendezvousSecret);
    };
    Ok((ip, secret))
}

/// Whether a punch at `peer` could be attempted right now.
///
/// The question `tcr peer reach` prints an answer to, stated as one call so
/// the verb and the dial cannot disagree about what "possible" means.
pub fn punch_is_possible(
    peer: &tcr_peer_wire::PeerId,
    endpoints: &[crate::peer::config::Endpoint],
) -> bool {
    punch_target(peer, endpoints).is_ok()
}

/// The two sockets a punch needs, as the one seam a test can stand behind.
///
/// # Why a trait and not two free functions
///
/// A punch is the one thing in this tree that cannot be exercised over
/// loopback as it ships. It needs two Macs whose PUBLIC ports are equal to the
/// ports they bound, and this machine has exactly one loopback address, so two
/// simulated routers would have to hold the same external port on
/// `127.0.0.1` at the same time and cannot. The seam is the socket layer and
/// nothing above it: [`punch`] itself, the slot waiting, the retry cadence and
/// the race between the two halves are the shipped code either way, and the
/// test swaps only what a router would have been.
///
/// Both methods take the LOCAL port, because that is the whole trick: the
/// outbound connect has to leave from the port the other side is dialling, or
/// the mapping it opens is for a port nobody will aim at.
pub trait PunchNet {
    /// Accept one connection on `port`, within `within`.
    fn accept_on(
        &self,
        port: u16,
        within: Duration,
    ) -> impl Future<Output = std::io::Result<crate::peer::noise::PeerStream>> + Send;

    /// Connect to `target` FROM local `port`, within `within`.
    fn connect_from(
        &self,
        port: u16,
        target: SocketAddr,
        within: Duration,
    ) -> impl Future<Output = std::io::Result<crate::peer::noise::PeerStream>> + Send;
}

/// Which half of the punch got through.
///
/// Recorded and reported because the two are not the same event on the wire
/// and an operator reading a packet capture needs to know which one to look
/// for. It decides NOTHING about roles: the side that asked for the punch runs
/// the dialling handshake and the side that was told runs the responding one,
/// whichever socket completed, because TCP's idea of who called whom is not
/// this protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchArrival {
    /// This node's own connect completed.
    OurConnect,
    /// A connection arrived on the port this node bound.
    TheirConnect,
}

/// A punch that got through.
pub struct Punched {
    /// The slot it landed in.
    pub slot: u64,
    /// The port it landed on.
    pub port: u16,
    /// Which half completed.
    pub arrival: PunchArrival,
    /// The stream, with nothing sent on it yet: the caller runs the handshake
    /// its role calls for, and a stranger who guessed the port gets no further
    /// than that handshake.
    pub stream: crate::peer::noise::PeerStream,
}

impl std::fmt::Debug for Punched {
    /// Without the stream, which has no useful debug form.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Punched")
            .field("slot", &self.slot)
            .field("port", &self.port)
            .field("arrival", &self.arrival)
            .finish_non_exhaustive()
    }
}

/// How often a slot re-tries its outbound connect.
///
/// A punch is a race between two sides opening their mappings, and the loser
/// of that race is refused rather than queued: whoever connects first arrives
/// before the other end's router has a mapping, and gets a closed port. So the
/// connect is repeated for the width of the slot. A quarter second is short
/// enough that a 30 second slot holds about a hundred attempts and long enough
/// that two Macs are not spinning.
pub const PUNCH_RETRY: Duration = Duration::from_millis(250);

/// Run a punch: for each slot, bind the pair's port, and race an inbound
/// accept against a repeated outbound connect to the peer's public address on
/// the same port.
///
/// # Why both halves, on both sides
///
/// The outbound connect is not how this node reaches the peer. It is how this
/// node's own router is made to hold a mapping on the punched port, so the
/// peer's connect has somewhere to arrive. Both sides do both things and
/// whichever socket completes first is the connection; the other is dropped
/// when this function returns, and a peer that sees it close reads exactly
/// what it reads from any abandoned dial.
///
/// A slot whose instant has already passed is skipped rather than run late:
/// the far side is not there any more, and dialling into an empty slot is how
/// a plan that drifted turns into three connects nobody answers.
pub async fn punch<N: PunchNet + Sync>(
    net: &N,
    plan: &[PunchSlot],
    peer_ip: IpAddr,
    slot_window: Duration,
) -> Result<Punched, PunchFailure> {
    let mut tried = 0_u32;
    let mut ports = Vec::new();

    for entry in plan {
        let now = crate::now_ms();
        let window_ms = i64::try_from(slot_window.as_millis()).unwrap_or(i64::MAX);
        if entry.opens_at_unix_ms.saturating_add(window_ms) <= now {
            tracing::debug!(
                slot = entry.slot,
                port = entry.port,
                "peer punch: this slot is already over; the far side is not in it"
            );
            continue;
        }
        let wait_ms = entry.opens_at_unix_ms.saturating_sub(now).max(0);
        tokio::time::sleep(Duration::from_millis(u64::try_from(wait_ms).unwrap_or(0))).await;

        tried = tried.saturating_add(1);
        ports.push(entry.port);
        let target = SocketAddr::new(peer_ip, entry.port);

        let accepted = net.accept_on(entry.port, slot_window);
        let dialled = punch_connect_loop(net, entry.port, target, slot_window);
        tokio::pin!(accepted);
        tokio::pin!(dialled);

        let landed = tokio::select! {
            inbound = &mut accepted => inbound.map(|stream| (PunchArrival::TheirConnect, stream)),
            outbound = &mut dialled => outbound.map(|stream| (PunchArrival::OurConnect, stream)),
        };

        match landed {
            Ok((arrival, stream)) => {
                tracing::info!(
                    slot = entry.slot,
                    port = entry.port,
                    peer_addr = %target,
                    arrival = ?arrival,
                    "peer punch: a slot got through"
                );
                return Ok(Punched {
                    slot: entry.slot,
                    port: entry.port,
                    arrival,
                    stream,
                });
            }
            Err(err) => tracing::debug!(
                slot = entry.slot,
                port = entry.port,
                peer_addr = %target,
                error = %err,
                "peer punch: this slot came and went with nothing through; trying the next"
            ),
        }
    }

    Err(PunchFailure::NoSlotConnected {
        slots_tried: tried,
        ports,
    })
}

/// Connect to `target` from `port`, over and over, until `within` is spent.
///
/// The repeat is the point: see [`PUNCH_RETRY`]. Every refusal is the ordinary
/// outcome of being the first of the two to dial, so they are counted and the
/// last one is what the caller is told about, rather than each being an error
/// in its own right.
async fn punch_connect_loop<N: PunchNet + Sync>(
    net: &N,
    port: u16,
    target: SocketAddr,
    within: Duration,
) -> std::io::Result<crate::peer::noise::PeerStream> {
    let deadline = tokio::time::Instant::now() + within;
    let mut last = std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "the slot ended before any connect was attempted",
    );
    let mut attempts = 0_u32;
    while tokio::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        attempts = attempts.saturating_add(1);
        match net.connect_from(port, target, left.min(within)).await {
            Ok(stream) => return Ok(stream),
            Err(err) => last = err,
        }
        tokio::time::sleep(
            PUNCH_RETRY.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
    Err(std::io::Error::new(
        last.kind(),
        format!("{attempts} connects to {target} from port {port} went unanswered: {last}"),
    ))
}

/// The [`PunchNet`] the program ships with: real sockets on this Mac.
///
/// `SO_REUSEADDR` and `SO_REUSEPORT` on both, because the punch binds one port
/// twice at once, a listener and a connecting socket, which is the whole shape
/// of a TCP simultaneous open and is refused outright without them.
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelPunchNet;

impl KernelPunchNet {
    /// A socket bound to `port` on every interface, with both reuse options
    /// set. Errors surface: a bind that failed is the difference between a
    /// punch that cannot work and one that did not this time.
    fn bound(port: u16) -> std::io::Result<tokio::net::TcpSocket> {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.set_reuseaddr(true)?;
        socket.set_reuseport(true)?;
        socket.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))?;
        Ok(socket)
    }
}

impl PunchNet for KernelPunchNet {
    async fn accept_on(
        &self,
        port: u16,
        within: Duration,
    ) -> std::io::Result<crate::peer::noise::PeerStream> {
        let listener = Self::bound(port)?.listen(8)?;
        match tokio::time::timeout(within, listener.accept()).await {
            Ok(Ok((stream, _from))) => Ok(Box::new(stream) as crate::peer::noise::PeerStream),
            Ok(Err(err)) => Err(err),
            Err(_elapsed) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("nothing arrived on punched port {port} inside the slot"),
            )),
        }
    }

    async fn connect_from(
        &self,
        port: u16,
        target: SocketAddr,
        within: Duration,
    ) -> std::io::Result<crate::peer::noise::PeerStream> {
        let socket = Self::bound(port)?;
        match tokio::time::timeout(within, socket.connect(target)).await {
            Ok(Ok(stream)) => Ok(Box::new(stream) as crate::peer::noise::PeerStream),
            Ok(Err(err)) => Err(err),
            Err(_elapsed) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("the connect to {target} from port {port} did not finish in time"),
            )),
        }
    }
}

/// What a peer's "meet me at slot N, I am at this address" frame asks for.
///
/// The whole of the listener's punch arm, here rather than there, so the
/// decision is a function a test can call: the arm itself only spawns what
/// this returns. Recording the address is part of the decision and not a step
/// the caller could forget, because an address a peer just told us is the
/// freshest one there is and the punch is about to aim at it.
pub fn punch_request(
    peer: tcr_peer_wire::PeerId,
    slot: u64,
    public_addr: &str,
    endpoints: &[crate::peer::config::Endpoint],
) -> Result<(IpAddr, Vec<PunchSlot>), PunchFailure> {
    let Ok(addr) = public_addr.parse::<SocketAddr>() else {
        return Err(PunchFailure::AddressNotUnderstood {
            told: public_addr.to_string(),
        });
    };
    remember_observed_peer(peer, addr);
    let (peer_ip, secret) = punch_target(&peer, endpoints)?;
    Ok((peer_ip, punch_plan_from_slot(&secret, slot, PUNCH_SLOTS)))
}

/// Believe the address a peer's own `CollapseHint` carried, and answer what
/// was recorded.
///
/// # Why a frame is believed here when a socket is not
///
/// The session this frame arrived on has already proved the sender's pinned
/// static key: a carried CONTROL stream is a fresh end to end Noise session
/// nested inside a TUNNEL, which is what makes the carrier blind to it. So the
/// address is routing advice from a proved sender, exactly as
/// [`punch_request`] below treats the address in a `PunchAt`, and the socket
/// the bytes arrived on is a carrier's connection that proves nothing about
/// who is behind it.
///
/// Every filter is [`crate::peer::config::endpoints_from_hello`]'s, reused
/// rather than restated: an entry that is not a socket address is skipped and
/// so is one that parses but cannot be dialed, which is the case that function
/// measured against a peer honestly naming its own `0.0.0.0` listen socket.
/// [`aimable_address`] then picks among what survived, the same picker a punch
/// uses on a row's own endpoints, so a hint and a row cannot disagree about
/// which of two addresses is worth aiming at.
///
/// Answers the address rather than a bool, so a caller logs the VALUE it
/// learned. "A hint was accepted" and "the punch will now aim at
/// 192.0.2.77:7755" are different lines to read at three in the morning.
pub fn learn_from_hint(
    peer: &tcr_peer_wire::PeerId,
    hint: &tcr_peer_wire::CollapseHint,
) -> Option<SocketAddr> {
    let endpoints = crate::peer::config::endpoints_from_hello(&hint.addrs, crate::now_ms());
    let addr = aimable_address(&endpoints)?;
    remember_observed_peer(*peer, addr);
    Some(addr)
}

/// This Mac's own hint for `peer`: where to reach it directly, in the frame
/// that says so.
///
/// **One builder, because two would drift.** The asker sends this before a
/// punch and the answering Mac sends it straight back, and the two are the
/// same fact in opposite directions; a second assembly of the same list is a
/// second answer to "which of my addresses do I name, and how many".
///
/// The order is [`self_address_for`]'s first, because the address that is
/// right for THIS peer is the one that peer itself measured, then every other
/// peer's observation behind it: a Mac behind one router looks the same from
/// everywhere outside it, so the rest are a real fallback rather than noise.
/// Duplicates are dropped, and the list is capped at
/// [`crate::peer::discovery::MAX_BRIEF_ADDRS`], the cap the sibling weak bands
/// already use for exactly this reason.
///
/// [`crate::peer::probe::collapse_hint`] fills the round trip, and a poisoned
/// path table costs the figure and nothing else: `0` is what that field's own
/// doc defines as "no measurement", which is also what every build before the
/// probe module would have sent.
pub fn own_collapse_hint(
    own_id: tcr_peer_wire::PeerId,
    peer: &tcr_peer_wire::PeerId,
) -> tcr_peer_wire::CollapseHint {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for addr in self_address_for(peer)
        .into_iter()
        .chain(observed_self_addresses().into_iter().map(|(_, addr)| addr))
    {
        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
        if addrs.len() == crate::peer::discovery::MAX_BRIEF_ADDRS {
            break;
        }
    }
    let addrs: Vec<String> = addrs.iter().map(SocketAddr::to_string).collect();
    match crate::peer::probe::with_table(|table| {
        crate::peer::probe::collapse_hint(own_id, addrs.clone(), table)
    }) {
        Ok(hint) => hint,
        Err(err) => {
            tracing::warn!(
                peer = %peer.display(),
                error = %err,
                "peer aim: the path table could not be read, so this hint carries no measured \
                 round trip"
            );
            crate::peer::probe::collapse_hint(
                own_id,
                addrs,
                &crate::peer::probe::PathTable::default(),
            )
        }
    }
}

/// How long one side spends inside a slot: the slot's own width.
///
/// Stated as a [`Duration`] once, because three callers need it and a second
/// spelling of "a slot is 30 seconds" is a second answer.
pub const fn slot_window() -> Duration {
    Duration::from_secs(SLOT_SECONDS)
}

/// The address to tell a peer to aim at: what THAT peer last saw this Mac as,
/// and failing that whatever any peer last saw it as.
///
/// The peer's own observation first, because two peers on different networks
/// may see this Mac as two different addresses and the one that is right for a
/// punch is the one the punching peer itself measured. Any other observation
/// is the fallback, and it is a fallback rather than nothing because a Mac
/// behind one router looks the same from everywhere outside it.
pub fn self_address_for(peer: &tcr_peer_wire::PeerId) -> Option<SocketAddr> {
    observed_self_for(peer).or_else(|| observed_self_addresses().first().map(|(_, addr)| *addr))
}

/// The slots of `plan` that open inside `budget` from now.
///
/// A dial has a deadline, the borrow timeout the operator set, and a punch
/// whose first slot opens after it would spend the whole budget waiting for a
/// boundary and then report the peer unreachable. So the punch takes the slots
/// that FIT, which is usually one and is sometimes none: a borrow with ten
/// seconds of patience meets a thirty second slot boundary a third of the
/// time. A caller with a longer budget gets all three.
pub fn punch_slots_within(
    plan: &[PunchSlot],
    now_unix_ms: i64,
    budget: Duration,
) -> Vec<PunchSlot> {
    let budget_ms = i64::try_from(budget.as_millis()).unwrap_or(i64::MAX);
    let deadline = now_unix_ms.saturating_add(budget_ms);
    plan.iter()
        .filter(|entry| entry.opens_at_unix_ms <= deadline)
        .copied()
        .collect()
}

/// The two lines `tcr peer reach` prints per pinned Mac: the address that Mac
/// last saw this one at, and whether a punch between them is possible.
///
/// A function here rather than formatting inside the verb, so the verb and the
/// dial cannot disagree about what "possible" means, and so the reading ships
/// without editing `src/main.rs`. The verb calls it with the row's label and
/// id.
///
/// Greppable, in the shape the rest of that verb already prints.
pub fn reach_punch_line(
    label: &str,
    peer: &tcr_peer_wire::PeerId,
    endpoints: &[crate::peer::config::Endpoint],
) -> String {
    let seen = match observed_self_for(peer) {
        Some(addr) => addr.to_string(),
        None => "not told (that Mac has not greeted this one since boot)".to_string(),
    };
    let punch = match punch_target(peer, endpoints) {
        Ok(_) => "yes".to_string(),
        Err(failure) => format!("no: {failure}"),
    };
    format!(
        "reach: peer: {label} {}: sees-us-at: {seen}: punch-possible: {punch}",
        peer.display()
    )
}

/// The last line `tcr peer reach` prints when no protocol will map a port:
/// what an operator can still do about it, in one sentence.
///
/// [`Some`] only for the outcome it is about, a router with no mapping service
/// this node can talk to ([`ReachError::no_natpmp_service`] names it, and the
/// keeper's answer carries the UPnP half too). Every other refusal is a device
/// that ANSWERED and was refused by name, where the thing to do is read that
/// name rather than reach for a workaround, so this stays [`None`] and the
/// verb prints nothing extra.
///
/// A function here rather than a `println!` in the verb, for the reason
/// [`reach_punch_line`] gives: the wording ships next to the decision that
/// selects it.
///
/// No new state, and nothing is asked of the router to produce it.
pub fn reach_no_mapping_line(failure: &ReachError, listen_port: Option<u16>) -> Option<String> {
    if !failure.no_natpmp_service() {
        return None;
    }
    let port = match listen_port {
        Some(port) => format!("tcp port {port}"),
        None => "the peer listener's port".to_string(),
    };
    Some(format!(
        "reach: no-mapping: this router will not open a port over either protocol, so two ways \
         are left: forward {port} to this Mac by hand in the router's own settings, or put both \
         Macs on one private network (Tailscale, or any VPN they both join) and pair over that"
    ))
}
