use super::*;
use crate::peer::noise::{port_secret_for, port_secrets, remember_port_secret};

// ---------------------------------------------------------------------------
// A port both ends compute, and nobody else can guess
// ---------------------------------------------------------------------------

/// How long one port slot lasts, in seconds.
///
/// Thirty seconds, with the two adjacent slots also accepted, so the window a
/// dial may land in is 90 seconds wide. That is the number the pair of clocks
/// has to agree inside, and two Macs syncing to the same time servers are
/// nowhere near a second apart, the width is for the dial's own flight time
/// and for a slot boundary crossed between the derive and the connect, not for
/// clock skew.
pub const SLOT_SECONDS: u64 = 30;

/// The lowest port a derivation may return.
///
/// Above the registered range and above every port this program binds on
/// purpose, so a derived port never collides with a service somebody chose.
pub const PORT_FLOOR: u16 = 20_000;

/// One past the highest port a derivation may return.
///
/// Below 61000, where macOS starts handing out ephemeral source ports
/// (`sysctl net.inet.ip.portrange.first`), so a derived listener is not
/// competing with the kernel's own allocator for its own number.
pub const PORT_CEILING: u16 = 60_000;

/// How many ports the derivation chooses between.
const PORT_SPAN: u64 = (PORT_CEILING - PORT_FLOOR) as u64;

// The reduction below adds a value under PORT_SPAN to PORT_FLOOR and casts the
// sum to u16. That is exact only while the sum fits, so the condition is
// checked at COMPILE time rather than trusted: a widened span is otherwise a
// debug panic in one build and a silently wrapped port number -- a port outside
// the window, which nothing is listening on -- in the release build that ships.
// Found by mutation: raising the span to 65535 panicked with "attempt to add
// with overflow" in three tests and would have wrapped in release.
const _: () = assert!(PORT_FLOOR as u64 + PORT_SPAN <= u16::MAX as u64);

/// Which slot a unix timestamp falls in.
pub const fn current_slot(unix_seconds: u64) -> u64 {
    unix_seconds / SLOT_SECONDS
}

/// The port this pair listens on, and dials, during one slot.
///
/// Both ends run this over the same secret and the same slot number and get
/// the same answer, so neither has to tell the other a port: the port IS the
/// shared secret plus the clock. A scanner without the secret sees one port in
/// forty thousand move every thirty seconds.
///
/// # The modulo, stated rather than hidden
///
/// Eight bytes of MAC reduced into a 40000-wide range is biased, by the
/// remainder of 2^64 over 40000, about one part in 4.6e14. That is not a
/// number any attacker can stand on, and rejection sampling here would make
/// the two ends disagree the moment their loop counts differed, so the bias is
/// taken deliberately.
pub fn derived_port(secret: &[u8; 32], slot: u64) -> u16 {
    use crate::peer::config::drop::hmac_sha256;

    let mut message = [0_u8; 12];
    message[..4].copy_from_slice(b"slot");
    message[4..].copy_from_slice(&slot.to_be_bytes());
    let mac = hmac_sha256(secret, &message);

    let mut head = [0_u8; 8];
    head.copy_from_slice(&mac[..8]);
    let choice = u64::from_be_bytes(head) % PORT_SPAN;

    // The cast is exact: `choice` is below PORT_SPAN, so the sum is below
    // PORT_CEILING, which is a u16.
    PORT_FLOOR + choice as u16
}

/// Every port a dial in slot `slot` may legitimately arrive on: the slot
/// before, the slot itself, and the slot after.
///
/// Three rather than one because a dial crosses a slot boundary for free, the
/// deriving end may compute its port at 11:59:59.9 and connect at 12:00:00.1,
/// and because the listener has to be up on the port before the dialler picks
/// it. Ordered current, previous, next, which is the order a listener should
/// bind in and a test should check in.
pub fn accepted_ports(secret: &[u8; 32], slot: u64) -> [u16; 3] {
    [
        derived_port(secret, slot),
        derived_port(secret, slot.saturating_sub(1)),
        derived_port(secret, slot.saturating_add(1)),
    ]
}

/// Fill the register from the peers file, at boot.
///
/// Returns how many rows carried a secret, for the one log line the caller
/// prints: a count is what tells an operator whether a restart kept the fleet's
/// rendezvous reach or started from nothing, and "restored=0" on a file that
/// should have had rows is a real signal.
///
/// A row without the key is skipped and is not an error: it predates the key,
/// or nothing has completed a session with that peer yet.
pub fn restore_from_peers(file: &crate::peer::config::PeerFile) -> usize {
    let mut restored = 0;
    for (node, secret, seen) in file.session_memory() {
        if let Some(secret) = secret {
            remember_port_secret(node, secret);
            restored += 1;
        }
        // The reflexive half, on the same pass and counted separately below by
        // nothing: a row may carry either key, both, or neither, and a boot
        // that restored one and not the other would report a number that is
        // true of neither. What the count means is "rows that had something to
        // restore", which is what the boot line says.
        if let Some(seen) = seen {
            match seen.parse::<SocketAddr>() {
                Ok(addr) => remember_observed_self(node, addr),
                Err(err) => tracing::debug!(
                    peer = %node.display(),
                    seen = %seen,
                    error = %err,
                    "peer reach: the address on this row is not a socket address; ignoring it"
                ),
            }
        }
    }
    restored
}

/// Drop the port secret this process holds for `peer`.
///
/// Called where `tcr peer forget` drops the row. Forgetting a Mac has to take
/// away every way of reaching it, and the rendezvous secret is a way of
/// reaching it that survives the row: the peers file no longer names the peer,
/// and a register nobody cleared would still hand a dialler that pair's three
/// ports for as long as the process lived. The row on disk is cleared by the
/// same act, because a forgotten peer has no row to hold it.
///
/// Answers whether anything was held, for the caller's log line: "nothing to
/// clear" and "cleared" are different facts and neither is a failure.
///
/// It clears the reflexive observations too ([`forget_observations`]), on the
/// same sentence: the address a forgotten Mac saw this one at is another way
/// the two could still meet, and a caller that had to remember a second call
/// is a caller that forgets it.
pub fn forget_port_secret(peer: &tcr_peer_wire::PeerId) -> bool {
    let mut held = match port_secrets().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    let had_secret = held.remove(peer).is_some();
    let had_observation = forget_observations(peer);
    had_secret || had_observation
}

/// The ports a dialler should try for `peer` when the recorded one stopped
/// answering: the pair's three accepted ports for the current slot, or an
/// empty list when this process holds no secret for it.
///
/// Current, previous, next, which is [`accepted_ports`]'s own order: the two
/// neighbours are what make one slot of clock skew, or a slot boundary crossed
/// between the derive and the connect, still meet.
pub fn rendezvous_ports(peer: &tcr_peer_wire::PeerId, unix_seconds: u64) -> Vec<u16> {
    match port_secret_for(peer) {
        Some(secret) => accepted_ports(&secret, current_slot(unix_seconds)).to_vec(),
        None => Vec::new(),
    }
}

/// Whether `port` is one this pair could be reaching each other on in `slot`.
pub fn port_is_accepted(secret: &[u8; 32], slot: u64, port: u16) -> bool {
    accepted_ports(secret, slot).contains(&port)
}
