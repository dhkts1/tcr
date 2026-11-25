//! The time-derived rendezvous port: how both ends derive it, what a dialler
//! tries when the recorded port stops answering, and the slots two Macs punch on.

use super::*;

// ---------------------------------------------------------------------------
// Item 3: the time-derived port
// ---------------------------------------------------------------------------

/// A seeded xorshift64, so a failing row can be re-run.
///
/// "One thousand random secrets" with a real random source is a test that
/// fails once a month on somebody else's machine and cannot be reproduced.
/// This is the same statistical coverage with the seed written down.
struct Seeded(u64);

impl Seeded {
    const fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn secret(&mut self) -> [u8; 32] {
        let mut out = [0_u8; 32];
        for chunk in out.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_be_bytes());
        }
        out
    }
}

/// **The gate for item 3, first half**: both ends of a real pairing derive the
/// same port for the same slot, with nothing sent about ports.
///
/// The handshake is driven in memory rather than over a socket, three `XX`
/// messages between two `snow` states, because the claim under test is that
/// the two sides' handshake hashes agree, and a socket adds nothing to that. If
/// this ever fails while `the_six_digit_code_binds_the_handshake_not_the_keys`
/// passes, the derivation is at fault and not the handshake.
#[test]
fn both_ends_of_a_pairing_derive_one_port_per_slot() {
    let (initiator_secret, _) = noise::generate_static().expect("a static keypair");
    let (responder_secret, _) = noise::generate_static().expect("a second static keypair");

    let mut dialling =
        noise::initiator_with_secret(&initiator_secret, noise::PATTERN_PAIR, None, None)
            .expect("an XX initiator");
    let mut accepting = noise::responder_with_secret(&responder_secret, noise::PATTERN_PAIR, &[])
        .expect("an XX responder");

    let mut wire = [0_u8; 1024];
    let mut payload = [0_u8; 1024];

    let one = dialling
        .write_message(&[0x11; 8], &mut wire)
        .expect("XX message 1");
    accepting
        .read_message(&wire[..one], &mut payload)
        .expect("the responder reads message 1");
    let two = accepting
        .write_message(&[], &mut wire)
        .expect("XX message 2");
    dialling
        .read_message(&wire[..two], &mut payload)
        .expect("the initiator reads message 2");
    let three = dialling
        .write_message(&[], &mut wire)
        .expect("XX message 3");
    accepting
        .read_message(&wire[..three], &mut payload)
        .expect("the responder reads message 3");

    assert!(
        dialling.is_handshake_finished() && accepting.is_handshake_finished(),
        "both sides must have completed the handshake before a hash is read off it"
    );

    let dialling_secret = noise::port_secret(dialling.get_handshake_hash());
    let accepting_secret = noise::port_secret(accepting.get_handshake_hash());
    assert_eq!(
        dialling_secret, accepting_secret,
        "the two ends of one handshake must derive one port secret; if this differs, no \
         amount of clock agreement will make them meet"
    );

    // Several slots, not one: a derivation that ignored the slot would pass a
    // single-slot check and then never rotate.
    let slots = [0_u64, 1, 57_812_345, u64::from(u32::MAX), u64::MAX - 1];
    for slot in slots {
        assert_eq!(
            reach::derived_port(&dialling_secret, slot),
            reach::derived_port(&accepting_secret, slot),
            "slot {slot} must give both ends the same port"
        );
    }
}

/// **The gate for item 3, second half**: a one-slot skew still meets, and the
/// window is exactly three slots wide.
///
/// The second assertion is the one with teeth. A check that accepted "any
/// port this secret ever derives" would pass the first assertion and have no
/// window at all, so the far slot has to be refused.
#[test]
fn a_one_slot_skew_still_meets_and_a_far_slot_does_not() {
    let mut seeded = Seeded(0x5eed_0000_0000_0003);
    let secret = seeded.secret();
    let slot = reach::current_slot(1_758_200_000);

    for skew in [-1_i64, 0, 1] {
        let theirs = slot.wrapping_add(skew as u64);
        let port = reach::derived_port(&secret, theirs);
        assert!(
            reach::port_is_accepted(&secret, slot, port),
            "a peer {skew} slots off derives {port}, which this end must still accept"
        );
    }

    assert_eq!(
        reach::accepted_ports(&secret, slot),
        [
            reach::derived_port(&secret, slot),
            reach::derived_port(&secret, slot - 1),
            reach::derived_port(&secret, slot + 1),
        ],
        "the window is the slot and its two neighbours, in that order"
    );
    for far in [slot - 2, slot + 2, slot + 100] {
        let port = reach::derived_port(&secret, far);
        assert!(
            !reach::port_is_accepted(&secret, slot, port),
            "slot {far} is outside the window; accepting its port {port} would mean there \
             is no window"
        );
    }
}

/// **The gate for item 3, third half**: one thousand secrets, and what the
/// derivation owes them.
///
/// Three properties in one pass over the same draw. Every port is inside the
/// declared range, because a port outside it is a bind failure or a collision
/// with a service somebody chose. Different pairs get different ports: with
/// 40000 buckets and 1000 draws the birthday estimate is about 12 collisions
/// (1000^2 / (2 * 40000)), so the floor is 950 distinct and a derivation that
/// keyed on anything constant would land far below it. And one secret's port
/// moves with the slot, which is the whole point of a time-derived port.
#[test]
fn a_thousand_secrets_spread_across_the_range_and_move_with_the_slot() {
    let mut seeded = Seeded(0x5eed_0000_0000_0001);
    let mut ports = Vec::with_capacity(1000);
    let slot = reach::current_slot(1_758_200_000);

    for _ in 0..1000 {
        let secret = seeded.secret();
        let port = reach::derived_port(&secret, slot);
        assert!(
            (reach::PORT_FLOOR..reach::PORT_CEILING).contains(&port),
            "{port} is outside {}..{}",
            reach::PORT_FLOOR,
            reach::PORT_CEILING
        );
        ports.push(port);
    }

    let mut distinct = ports.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(
        distinct.len() >= 950,
        "1000 secrets gave only {} distinct ports; the birthday estimate for 40000 buckets \
         is about 12 collisions",
        distinct.len()
    );

    let secret = seeded.secret();
    let mut over_slots: Vec<u16> = (0..1000)
        .map(|step| reach::derived_port(&secret, slot + step))
        .collect();
    over_slots.sort_unstable();
    over_slots.dedup();
    assert!(
        over_slots.len() >= 950,
        "one secret over 1000 consecutive slots gave only {} distinct ports; a derivation \
         that ignored the slot would give 1",
        over_slots.len()
    );
}

/// The three numbers the scheme is written down as. A change to any of them is
/// a change both ends must make on the same day, so it is asserted rather than
/// left to a reader of two files.
#[test]
fn the_port_windows_own_numbers() {
    assert_eq!(reach::SLOT_SECONDS, 30, "one slot, in seconds");
    assert_eq!(reach::PORT_FLOOR, 20_000);
    assert_eq!(
        reach::PORT_CEILING,
        60_000,
        "below the ephemeral range macOS allocates from"
    );
    assert_eq!(reach::current_slot(89), 2, "89 / 30");
    assert_eq!(
        reach::current_slot(90),
        3,
        "the boundary belongs to the later slot"
    );
}

// ---------------------------------------------------------------------------
// Item 3: what a dialler tries when the recorded port stops answering
// ---------------------------------------------------------------------------

/// A pinned row with one recorded endpoint and nothing else.
fn row_at(node: tcr_peer_wire::PeerId, recorded: SocketAddr) -> peer_config::PeerRow {
    let mut row = peer_config::PeerRow {
        node,
        label: "studio-mac".to_string(),
        endpoints: Vec::new(),
        added_at: 1,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: peer_config::Allow::default(),
        lend: Vec::new(),
    };
    row.observe_endpoint(peer_config::Endpoint::direct(
        recorded,
        1_000,
        peer_config::EndpointSource::Paired,
    ));
    row
}

/// A loopback port nothing is listening on: bound, read back, released.
///
/// Not a hard-coded number, because a hard-coded "surely nothing is here"
/// port is the kind of thing that passes for a year and then collides with
/// whatever another test started.
async fn a_closed_loopback_port() -> SocketAddr {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a kernel-chosen loopback port");
    let addr = held.local_addr().expect("its address");
    drop(held);
    addr
}

/// How many stand-in inputs [`a_stand_in_whose_port_binds`] tries. Each one's
/// derived port is an independent draw from forty thousand, so needing more
/// than a couple means loopback is nearly full, and that is worth a failure
/// rather than a longer search.
const STAND_INS: u8 = 8;

/// `[seed; N]` with its first byte stepped by `step`: the stand-in secret or
/// handshake hash [`a_stand_in_whose_port_binds`] tries at `step`. Step 0 is
/// the plain `[seed; N]`.
fn stepped<const N: usize>(seed: u8, step: u8) -> [u8; N] {
    let mut bytes = [seed; N];
    bytes[0] = seed.wrapping_add(step);
    bytes
}

/// A stand-in derivation input (a port secret, or a handshake hash standing in
/// for a completed handshake) whose derived port this process can bind on
/// loopback, with that port bound and returned.
///
/// One derived port is a single machine-chosen number in 20000..60000, and a
/// CI runner had it taken on 2026-10-20 (`none of [55978] could be bound`),
/// which failed a test whose subject is the derivation and not that port. A
/// test that stands in for a handshake may pick its input, so this tries
/// `stand_in(0)`, `stand_in(1)` and on, and keeps the first whose port binds.
/// `port_in_slot` names WHICH derived port the test needs (the current slot's,
/// the previous one's) for the slot this reads off the clock. The listener is
/// handed back so the caller holds the port until its dial.
///
/// Panics, never skips, when none of them bind.
async fn a_stand_in_whose_port_binds<T>(
    stand_in: impl Fn(u8) -> T,
    port_in_slot: impl Fn(&T, u64) -> u16,
) -> (T, tokio::net::TcpListener, u16) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let slot = reach::current_slot(now);
    let mut busy = Vec::new();
    for step in 0..STAND_INS {
        let input = stand_in(step);
        let port = port_in_slot(&input, slot);
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(bound) => return (input, bound, port),
            Err(err) => busy.push(format!("{port} ({err})")),
        }
    }
    panic!(
        "none of {STAND_INS} stand-in inputs had a derived port this process could bind on \
         loopback: {busy:?}"
    );
}

/// The CURRENT slot's port for `secret`, which is `reach::accepted_ports`'s
/// first entry and the first rendezvous port a dialler tries, so holding it
/// keeps the dial from landing on whatever owns a busy port.
fn current_port(secret: &[u8; 32], slot: u64) -> u16 {
    reach::accepted_ports(secret, slot)[0]
}

/// **The gate for item 3**: the recorded port is dead, and the dial lands on
/// the pair's rendezvous port for the current slot.
///
/// The row's only endpoint points at a closed port, so a dialler that tried
/// the recorded endpoints and stopped returns nothing here. What makes the
/// second attempt possible is the pair's port secret, which both ends compute
/// from a handshake they both ran and neither one sends.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_recorded_port_falls_through_to_the_pairs_rendezvous_port() {
    let node = tcr_peer_wire::PeerId([0x31; 32]);
    // The CURRENT slot's port, which is `accepted_ports`'s first entry. The
    // two neighbours are the skew case and get their own test below. The
    // secret is chosen so that port is free here, and held until the dial.
    let (secret, listening, port) =
        a_stand_in_whose_port_binds(|step| stepped::<32>(0x5a, step), current_port).await;
    noise::remember_port_secret(node, secret);

    let accepting = tokio::spawn(async move { listening.accept().await.map(|(_, from)| from) });

    let dead = a_closed_loopback_port().await;
    let row = row_at(node, dead);
    let reached = tcr::peer::tunnel::dial::dial_peer_within(&row, 3_000)
        .await
        .map(|(addr, _stream)| addr);

    assert_eq!(
        reached,
        Some(SocketAddr::new(dead.ip(), port)),
        "the recorded port {} is closed, so the dial must land on this pair's rendezvous \
         port {port}",
        dead.port()
    );
    accepting
        .await
        .expect("the accept task")
        .expect("the listener accepted the dial");
}

/// **The gate for item 3, the skew half**: a listener one slot behind is still
/// reached.
///
/// `accepted_ports` is current, previous, next, so a peer whose clock or whose
/// bind landed in the slot before this one is on the list. Without the two
/// neighbours a dial that crossed a slot boundary between deriving the port
/// and connecting would miss by one, every thirty seconds, for no reason the
/// operator could see.
#[tokio::test(flavor = "multi_thread")]
async fn one_slot_of_skew_still_meets() {
    let node = tcr_peer_wire::PeerId([0x32; 32]);
    // The PREVIOUS slot's port, with the secret chosen so that port is free
    // here, and held until the dial.
    let (secret, listening, port) = a_stand_in_whose_port_binds(
        |step| stepped::<32>(0x77, step),
        |secret, slot| reach::derived_port(secret, slot - 1),
    )
    .await;
    noise::remember_port_secret(node, secret);

    let accepting = tokio::spawn(async move { listening.accept().await.map(|(_, from)| from) });

    let dead = a_closed_loopback_port().await;
    let row = row_at(node, dead);
    let reached = tcr::peer::tunnel::dial::dial_peer_within(&row, 3_000)
        .await
        .map(|(addr, _stream)| addr);

    assert_eq!(
        reached,
        Some(SocketAddr::new(dead.ip(), port)),
        "a peer listening on the PREVIOUS slot's port is one slot of skew away and must \
         still be reached"
    );
    accepting
        .await
        .expect("the accept task")
        .expect("the listener accepted the dial");
}

/// The control: with no port secret for this pair, the dial tries the recorded
/// endpoints and stops.
///
/// Without this, a dialler that scanned ports would pass both tests above and
/// nobody would know. It also pins the floor `reach::port_secrets` states: a
/// process holding no secret for a pair, from a session or from the peers
/// file, has exactly the reach it had before derived ports existed.
#[tokio::test(flavor = "multi_thread")]
async fn with_no_secret_for_the_pair_nothing_but_the_recorded_endpoint_is_tried() {
    let node = tcr_peer_wire::PeerId([0x33; 32]);
    // A listener on what WOULD be this pair's current port, had its secret
    // been registered. The secret is deliberately NOT registered against
    // `node`, and is chosen so that port is free here.
    let (_secret, listening, _port) =
        a_stand_in_whose_port_binds(|step| stepped::<32>(0x11, step), current_port).await;
    let _held = listening;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();

    let dead = a_closed_loopback_port().await;
    let row = row_at(node, dead);
    let reached = tcr::peer::tunnel::dial::dial_peer_within(&row, 900)
        .await
        .map(|(addr, _stream)| addr);

    assert_eq!(
        reached,
        None,
        "this process holds no secret for {}, so there are no rendezvous ports to try and \
         the dial must give up after the recorded endpoint",
        node.display()
    );
    assert!(
        reach::rendezvous_ports(&node, now).is_empty(),
        "and the port list itself must be empty, not merely unreachable"
    );
}

/// **The gate for the leading race's own budget**: two endpoints that
/// BLACKHOLE, rather than refuse, must not let the leading race spend the
/// whole borrow deadline. `dial_peer_reaching_within` has a later step after
/// the leading race even when nothing else is configured for this
/// peer -- a punch attempt and the forwarder loop, both of which fail fast
/// here since this row has no punch secret and no other peer is allowed to
/// carry -- and this measures that the WHOLE call still returns with real
/// time to spare before the ten second deadline, rather than landing right at
/// it.
///
/// Both addresses are from the documentation range (RFC 5737 TEST-NET-3),
/// which nobody answers on the real internet: a connect attempt gets no
/// SYN-ACK and no RST, a genuine blackhole. A refused port returns almost
/// instantly and would not reproduce this at all.
///
/// Watched red: today's `run_direct_race` bounds the leading race by the
/// WHOLE `borrow_timeout_ms`, so with both candidates blackholing, the race's
/// own external timeout does not fire until the full ten seconds are spent,
/// and the call returns at essentially the whole deadline. This asserts the
/// call returns comfortably under nine seconds instead.
#[tokio::test(flavor = "multi_thread")]
async fn two_blackholing_endpoints_leave_a_later_step_time_to_run() {
    let node = tcr_peer_wire::PeerId([0x59; 32]);
    let blackhole_one: SocketAddr = "203.0.113.10:41000"
        .parse()
        .expect("a literal address parses");
    let blackhole_two: SocketAddr = "203.0.113.11:41000"
        .parse()
        .expect("a literal address parses");

    // A registered port secret so each candidate's own attempt tries the
    // recorded port PLUS this pair's rendezvous ports
    // (`DIAL_ATTEMPTS_PER_ENDPOINT`), which is what makes one blackholing
    // candidate cost close to the whole borrow deadline on its own. Without
    // this, `rendezvous_ports` answers empty, each candidate tries only the
    // recorded port, and the leading race concludes in well under a second
    // regardless of its bound, proving nothing about the bound at all.
    let secret = [0x59_u8; 32];
    noise::remember_port_secret(node, secret);

    let mut row = peer_config::PeerRow {
        node,
        label: "far-mac".to_string(),
        endpoints: Vec::new(),
        added_at: 1,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: peer_config::Allow::default(),
        lend: Vec::new(),
    };
    row.observe_endpoint(peer_config::Endpoint::direct(
        blackhole_one,
        1_000,
        peer_config::EndpointSource::Paired,
    ));
    row.observe_endpoint(peer_config::Endpoint::direct(
        blackhole_two,
        1_000,
        peer_config::EndpointSource::Paired,
    ));

    let dir = tempfile::tempdir().expect("a temp dir for the peers file");
    let path = dir.path().join("tcr-peers.json");
    let file = PeerFile {
        peers: vec![row],
        ..Default::default()
    };
    peer_config::save(&path, &file).expect("the row writes");
    let store = peer_config::PeerStore::open(&path).expect("the peer store opens");
    let dial_row = store.row(&node).expect("the row reads back");

    let deadline_ms: u64 = 10_000;
    let start = Instant::now();
    let reached =
        tcr::peer::tunnel::dial::dial_peer_reaching_within(&dial_row, &store, deadline_ms).await;
    let elapsed = start.elapsed();

    assert!(
        reached.is_none(),
        "neither blackholing endpoint can answer, this row has no punch secret and no other \
         peer is allowed to carry, so nothing can reach this peer"
    );
    assert!(
        elapsed < Duration::from_millis(9_000),
        "the leading race must not spend the whole {deadline_ms} ms deadline on two \
         blackholing candidates: a later step (the punch attempt and the forwarder loop) must \
         still get a turn with real time to spare, and this call took {elapsed:?}"
    );
}

/// **The gate for the retry backoff, through the production dial**: a dead
/// endpoint that just failed is cooled down, and the next order read from the
/// same row drops it while the endpoint that answered is still there.
///
/// No port secret is registered for this row, so the dial tries only the
/// row's own two recorded endpoints and stops there, which is what keeps this
/// deterministic: `with_no_secret_for_the_pair_nothing_but_the_recorded_endpoint_is_tried`
/// above is the control for that half. The dead endpoint is observed AFTER
/// the live one, so it sorts first on `dial_order`'s newest-first tiebreak
/// and is the one the dial tries and fails before it reaches the live
/// listener.
///
/// This is the half the compile-error red in `probe.rs` could not prove:
/// both `PathTable::cool_down` and `probe::drop_cooling_endpoints` are
/// exercised directly by unit tests in `tests/peer_probe.rs`, so a dial that
/// forgot to call `peer::reach::probe::cool_down` on failure would still leave those
/// green. Only a real dial that writes into the INSTALLED table and a second
/// read of `dial_order` against that same table can show the wiring works.
///
/// Watched red: comment out the two `crate::peer::reach::probe::cool_down(...)`
/// calls in `src/peer/serve.rs` and the dead endpoint is still in the second
/// order.
#[tokio::test(flavor = "multi_thread")]
async fn a_dial_that_fails_one_endpoint_cools_it_and_the_next_order_drops_it() {
    let node = tcr_peer_wire::PeerId([0x35; 32]);
    // Deliberately no `reach::remember_port_secret`: this process holds no
    // rendezvous ports for this pair, so the dial tries only the row's two
    // recorded endpoints.

    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a live loopback listener");
    let live = listening.local_addr().expect("its address");
    let accepting = tokio::spawn(async move { listening.accept().await.map(|(_, from)| from) });

    let dead = a_closed_loopback_port().await;

    let mut row = row_at(node, live);
    row.observe_endpoint(peer_config::Endpoint::direct(
        dead,
        2_000,
        peer_config::EndpointSource::Paired,
    ));

    let reached = tcr::peer::tunnel::dial::dial_peer_within(&row, 3_000)
        .await
        .map(|(addr, _stream)| addr);
    assert_eq!(
        reached,
        Some(live),
        "the dead endpoint is tried first (it is the newer observation) and fails, so the \
         dial must fall through to the live listener"
    );
    accepting
        .await
        .expect("the accept task")
        .expect("the listener accepted the dial");

    let order = tcr::peer::tunnel::dial::dial_order(&row);
    let remaining: Vec<SocketAddr> = order
        .iter()
        .filter_map(peer_config::Endpoint::direct_addr)
        .collect();
    assert!(
        !remaining.contains(&dead),
        "the endpoint that just failed a dial must be cooling and dropped from the next \
         order: {remaining:?}"
    );
    assert!(
        remaining.contains(&live),
        "and the endpoint that answered must still be there, with its own cooldown cleared: \
         {remaining:?}"
    );
}

/// **The gate for the race itself, through the production dial**: a row with
/// two LIVE loopback listeners must come back from [`dial_order`] with BOTH
/// endpoints after the dial, because whichever one lost the race was
/// cancelled, not refused.
///
/// Both listeners are live on purpose: a dead second endpoint would leave the
/// race's own semantics unproven, since a candidate that fails outright is
/// supposed to be cooled and its absence from the next order would not tell
/// the two cases apart. Here neither endpoint is ever refused, so if EITHER
/// went missing from the next order, that would be the race cooling a
/// candidate it only cancelled.
///
/// Watched red: make the race cool down every candidate it started (both the
/// winner and whatever it did not use), not only the ones whose own attempt
/// answered nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_dial_that_races_two_live_endpoints_does_not_cool_the_one_it_did_not_use() {
    let node = tcr_peer_wire::PeerId([0x36; 32]);

    let first = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the first live listener");
    let first_addr = first.local_addr().expect("its address");

    let second = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the second live listener");
    let second_addr = second.local_addr().expect("its address");

    let mut row = row_at(node, first_addr);
    row.observe_endpoint(peer_config::Endpoint::direct(
        second_addr,
        2_000,
        peer_config::EndpointSource::Paired,
    ));

    let reached = tcr::peer::tunnel::dial::dial_peer_within(&row, 3_000)
        .await
        .map(|(addr, _stream)| addr);
    assert!(
        reached == Some(first_addr) || reached == Some(second_addr),
        "one of the two live listeners must answer the race: {reached:?}"
    );

    let order = tcr::peer::tunnel::dial::dial_order(&row);
    let remaining: Vec<SocketAddr> = order
        .iter()
        .filter_map(peer_config::Endpoint::direct_addr)
        .collect();
    assert!(
        remaining.contains(&first_addr) && remaining.contains(&second_addr),
        "whichever endpoint the race did not use was cancelled, not refused, and must not be \
         cooling: {remaining:?}"
    );

    drop(first);
    drop(second);
}

/// Run `tcr peer internet on|off` against a temp peers file: `(stdout, stderr, ok)`.
fn run_internet(peers: &Path, state: &str) -> (String, String, bool) {
    let output = tcr_beside(peers)
        .args(["peer", "internet", state])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .output()
        .expect("spawn tcr peer internet");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// **The gate for the `tcr peer internet on|off` arm**: the switch is the one
/// thing between a Mac that answers only its own LAN and one a pinned key can
/// reach from anywhere, so the verb has to move the flag in the FILE, which is
/// what both the mapping keeper and the accept gate read.
///
/// Asserted on the file's own bytes rather than through `PeerFile`, for the
/// reason `peers_file_with_one_peer` gives: a field added to that struct
/// elsewhere must not turn this into a compile error.
///
/// The `off` leg asserts the flag and the wording, never a deleted mapping: a
/// machine running this suite may have no gateway at all, and the verb says so
/// either way.
#[test]
fn peer_internet_on_then_off_moves_the_flag_in_the_peers_file() {
    let dir = scratch("cli-internet");
    let peers = peers_file_with_one_peer(&dir, 41567);

    let raw = std::fs::read_to_string(&peers).expect("the fixture reads");
    assert!(
        !raw.contains("\"internet\""),
        "the fixture starts with no internet key at all, so a default of false is what \
         the first run has to change: {raw}"
    );

    let (out, err, ok) = run_internet(&peers, "on");
    assert!(ok, "tcr peer internet on exited non-zero: {err}");
    assert!(
        out.contains("peer.internet: on") && out.contains("41567"),
        "the on run names the port it will map, so an operator can see WHICH socket \
         goes through the router: {out}"
    );
    let raw = std::fs::read_to_string(&peers).expect("the peers file reads after on");
    assert!(
        raw.contains("\"internet\": true"),
        "on has to reach the file the keeper and the accept gate read: {raw}"
    );

    let (out, err, ok) = run_internet(&peers, "off");
    assert!(ok, "tcr peer internet off exited non-zero: {err}");
    assert!(
        out.contains("peer.internet: off"),
        "the off run says so plainly, with or without a gateway to unmap at: {out}"
    );
    let raw = std::fs::read_to_string(&peers).expect("the peers file reads after off");
    assert!(
        !raw.contains("\"internet\": true"),
        "off has to put the flag back, or a Mac stays reachable off its LAN after \
         the operator said stop: {raw}"
    );
}

/// **The gate for the rendezvous secret on disk**: a process that has
/// completed no session reaches a peer on the pair's derived port, because a
/// previous boot wrote the secret to the peers file.
///
/// Three properties in one test, because they are one mechanism.
///
/// 1. What is written is the DERIVED secret and never the handshake hash it
///    came from (decision row 17). The hash's own hex is asserted absent from
///    the file bytes, with the secret's hex present in the same read as the
///    positive control: an absence assertion against a file that was never
///    written would pass on its own.
/// 2. `restore_from_peers` puts it in the register, which is what a boot does.
/// 3. The dial then lands on the pair's port for this slot, with the recorded
///    endpoint dead, in a process where no handshake has run.
///
/// Watched red by making `observe_rendezvous_secret` return `Ok(false)`
/// without writing: the restore counts 0 and the dial reaches `None`.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_reaches_a_peer_on_the_derived_port_from_the_peers_file() {
    use tcr::peer::config;

    let node = tcr_peer_wire::PeerId([0x44; 32]);
    // Stands in for a completed handshake: the value never leaves this test,
    // and what matters is that the FILE holds something else. Chosen so its
    // current-slot port is free here, and that port is held from now until the
    // dial: see `a_stand_in_whose_port_binds`.
    let (handshake_hash, listening, port) = a_stand_in_whose_port_binds(
        |step| stepped::<48>(0x7e, step),
        |hash, slot| current_port(&noise::port_secret(hash), slot),
    )
    .await;
    let secret = noise::port_secret(&handshake_hash);
    let hash_hex: String = handshake_hash.iter().map(|b| format!("{b:02x}")).collect();
    let secret_hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();

    assert!(
        noise::port_secret_for(&node).is_none(),
        "the control: this process has completed no session with that key, which is the \
         state a restart leaves behind"
    );

    let dir = scratch("restart-rendezvous");
    let peers = dir.join("tcr-peers.json");
    std::fs::write(
        &peers,
        format!(
            r#"{{
  "peers": [
    {{ "node": "{node_wire}", "label": "studio-mac", "addedAt": 1 }}
  ]
}}"#,
            node_wire = node.to_wire()
        ),
    )
    .expect("write the peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }

    assert!(
        config::observe_rendezvous_secret(&peers, &node, secret).expect("the write runs"),
        "a pinned row takes the secret"
    );

    let raw = std::fs::read_to_string(&peers).expect("the peers file reads");
    assert!(
        raw.contains(&secret_hex),
        "the positive control: the derived secret IS in the file, so the absence below \
         is about what was written and not about an empty file"
    );
    assert!(
        !raw.contains(&hash_hex),
        "and the handshake hash is NOT, because it is what the six-digit pairing compare \
         is built from and it stays in memory: {raw}"
    );

    let file = config::read_or_default(&peers).expect("the peers file parses back");
    assert_eq!(
        reach::restore_from_peers(&file),
        1,
        "a boot restores the one row that carries a secret"
    );
    assert_eq!(
        noise::port_secret_for(&node),
        Some(secret),
        "and the register holds exactly what the file did, byte for byte"
    );

    let accepting = tokio::spawn(async move { listening.accept().await.map(|(_, from)| from) });

    let dead = a_closed_loopback_port().await;
    let row = row_at(node, dead);
    let reached = tcr::peer::tunnel::dial::dial_peer_within(&row, 3_000)
        .await
        .map(|(addr, _stream)| addr);

    assert_eq!(
        reached,
        Some(SocketAddr::new(dead.ip(), port)),
        "the recorded port {} is dead and no session has run in this process, so the only \
         thing that can reach this peer is the secret the last boot wrote down",
        dead.port()
    );
    accepting
        .await
        .expect("the accept task")
        .expect("the listener accepted the dial");
}

/// **Forgetting a Mac takes its rendezvous secret away too.**
///
/// `tcr peer forget` drops the row, and until this fix the process kept that
/// pair's port secret: the peers file no longer named the Mac and a dialler in
/// the same process would still have computed its three accepted ports for as
/// long as `tcr` ran. Forgetting has to mean every way of reaching it, not the
/// ways written down.
///
/// The row's own copy goes with the row, because the row is what held it. This
/// asserts both halves: the register is empty afterwards and the saved file
/// holds no `rendezvousSecret` for that key.
///
/// The `is_some` before the forget is the positive control: without it, an
/// assertion that the register is empty afterwards would pass against a
/// register that was never filled.
///
/// Watched red by dropping the `forget_port_secret` call from `pair::forget`:
/// the first assertion still reads the secret after the peer is forgotten.
#[test]
fn forgetting_a_peer_clears_its_rendezvous_secret() {
    use tcr::peer::config;

    let node = tcr_peer_wire::PeerId([0x66; 32]);
    let secret = noise::port_secret(&[0x1d_u8; 48]);

    let dir = scratch("forget-rendezvous");
    let peers = dir.join("tcr-peers.json");
    std::fs::write(
        &peers,
        format!(
            r#"{{
  "peers": [
    {{ "node": "{node_wire}", "label": "studio-mac", "addedAt": 1 }}
  ]
}}"#,
            node_wire = node.to_wire()
        ),
    )
    .expect("write the peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }
    config::observe_rendezvous_secret(&peers, &node, secret).expect("the write runs");
    noise::remember_port_secret(node, secret);

    assert_eq!(
        noise::port_secret_for(&node),
        Some(secret),
        "the control: this process holds the pair's secret before the forget, so what \
         follows is the forget and not an empty register"
    );

    let store = config::PeerStore::open(&peers).expect("open the peers file");
    assert!(
        tcr::peer::pair::forget(&store, &node).expect("the forget runs"),
        "the row was there to remove"
    );

    assert_eq!(
        noise::port_secret_for(&node),
        None,
        "a forgotten Mac leaves no way to compute its rendezvous ports, or `forget` \
         means only the ways that were written down"
    );
    let raw = std::fs::read_to_string(&peers).expect("the peers file reads");
    assert!(
        !raw.contains("rendezvousSecret"),
        "and the row's own copy went with the row: {raw}"
    );
}

// ---------------------------------------------------------------------------
// The schedule two Macs punch on
// ---------------------------------------------------------------------------

/// Two Macs a fraction of a second apart compute the SAME first slot, the same
/// port and the same instant.
///
/// The whole reason a punch needs no port on the wire. If the plan started in
/// the slot already open, the side that computed it late would bind, dial and
/// give up before the other side had started, and both would report a NAT
/// problem they do not have.
#[test]
fn both_sides_of_a_pair_plan_the_same_slots() {
    let secret = [0x5a_u8; 32];
    let slot_ms = i64::try_from(reach::SLOT_SECONDS).expect("the slot width fits") * 1_000;
    // One instant just after a boundary and one just before the next: the
    // widest disagreement two clocks inside one slot can have.
    let early = 1_000_000 * slot_ms + 1;
    let late = 1_000_000 * slot_ms + slot_ms - 1;

    let first = reach::punch_plan(&secret, early, reach::PUNCH_SLOTS);
    let second = reach::punch_plan(&secret, late, reach::PUNCH_SLOTS);

    assert_eq!(
        first, second,
        "two clocks inside one slot plan the same punch, or the port is a number the two \
         sides would have to tell each other"
    );
    assert_eq!(
        first.len(),
        reach::PUNCH_SLOTS as usize,
        "three slots, the number the dial order names"
    );
    assert_eq!(
        first[0].slot, 1_000_001,
        "and the plan starts at the NEXT boundary, never in the slot already part spent"
    );
    assert_eq!(
        first[0].opens_at_unix_ms,
        1_000_001 * slot_ms,
        "the instant is the boundary itself, absolute rather than a duration from now"
    );
    for (step, entry) in first.iter().enumerate() {
        assert_eq!(
            entry.port,
            reach::derived_port(&secret, entry.slot),
            "slot {step} punches on the port the pair already derives for it"
        );
        assert!(
            (reach::PORT_FLOOR..reach::PORT_CEILING).contains(&entry.port),
            "and it is inside the window the derivation promises: {entry:?}"
        );
    }
}

/// The side that is TOLD a slot plans from that number and not from its own
/// clock.
#[test]
fn the_told_side_plans_from_the_slot_it_was_given() {
    let secret = [0x5b_u8; 32];
    let plan = reach::punch_plan_from_slot(&secret, 4_242, reach::PUNCH_SLOTS);
    assert_eq!(
        plan.iter().map(|entry| entry.slot).collect::<Vec<_>>(),
        vec![4_242, 4_243, 4_244],
        "the number on the wire is the one that was chosen, not one re-derived here"
    );
    assert_eq!(
        plan[0].port,
        reach::derived_port(&secret, 4_242),
        "on the port that slot derives"
    );
}

/// A punch that cannot start says which of the two things is missing.
///
/// Both arms, and in order: a pair with no observed address AND no dialable
/// row endpoint is the ordinary state of two Macs that have only met on one
/// LAN, and a pair with an address but no secret is what a restart leaves.
/// They need different answers from an operator, so they are different
/// names.
#[test]
fn a_punch_that_cannot_start_names_what_is_missing() {
    let node = tcr_peer_wire::PeerId([0x75; 32]);
    assert_eq!(
        reach::punch_target(&node, &[]),
        Err(reach::PunchFailure::PeerAddressUnknown),
        "nothing has ever seen this peer and the row is empty, so there is no address to aim at"
    );
    assert!(!reach::punch_is_possible(&node, &[]));

    // A row that holds only an undialable endpoint (a hop this node would
    // have to reach through another Mac) still refuses with the same
    // variant: the new source is the row's own DIRECT locators, not the row
    // itself.
    let via_only = [tcr::peer::config::Endpoint::via(
        tcr_peer_wire::PeerId([0x76; 32]),
        1_000,
        tcr::peer::config::EndpointSource::Brief,
    )];
    assert_eq!(
        reach::punch_target(&node, &via_only),
        Err(reach::PunchFailure::PeerAddressUnknown),
        "an endpoint that needs another Mac to carry it is not something this node can aim at"
    );

    reach::remember_observed_peer(
        node,
        "203.0.113.20:41000".parse().expect("a test address parses"),
    );
    assert_eq!(
        reach::punch_target(&node, &[]),
        Err(reach::PunchFailure::NoRendezvousSecret),
        "an address without a secret is a port the two cannot compute"
    );

    noise::remember_port_secret(node, [0x5c; 32]);
    let (addr, secret) = reach::punch_target(&node, &[]).expect("both halves are held now");
    assert_eq!(
        addr,
        "203.0.113.20".parse::<IpAddr>().expect("parses"),
        "the punch aims at the address, and the port comes from the derivation"
    );
    assert_eq!(secret, [0x5c; 32]);
    assert!(reach::punch_is_possible(&node, &[]));

    let named = reach::PunchFailure::NoSlotConnected {
        slots_tried: 3,
        ports: vec![20_001, 20_002, 20_003],
    }
    .to_string();
    assert!(
        named.contains("3 slots") && named.contains("20001"),
        "and the failure a symmetric NAT produces prints the count and the ports: {named}"
    );
}

/// A punch may aim at an address the row already holds, when the per boot
/// observation register has nothing.
///
/// The register is never persisted, so it is empty for every peer after any
/// restart, move or no move ([`tcr::peer::reach::observed_peer_for`]'s
/// own doc). The row may already hold that peer's current public address from
/// a moved link, and only the IP matters here because the port comes from the
/// pair's own slot derivation.
#[test]
fn aim_off_the_row() {
    let node = tcr_peer_wire::PeerId([0x77; 32]);
    let moved: SocketAddr = "198.51.100.7:7755".parse().expect("a test address parses");
    let endpoints = [tcr::peer::config::Endpoint::direct(
        moved,
        2_000,
        tcr::peer::config::EndpointSource::Moved,
    )];

    noise::remember_port_secret(node, [0x5d; 32]);
    let (addr, secret) = reach::punch_target(&node, &endpoints).expect(
        "punch_target refused with PeerAddressUnknown while the row held \
         198.51.100.7:7755 from a moved link",
    );
    assert_eq!(
        addr,
        moved.ip(),
        "the row's own address is what a punch aims at when the register holds nothing"
    );
    assert_eq!(secret, [0x5d; 32]);
    assert!(reach::punch_is_possible(&node, &endpoints));

    // The register still wins over the row when both hold something: a
    // source address a connection just arrived from is the freshest fact.
    reach::remember_observed_peer(
        node,
        "203.0.113.9:9000".parse().expect("a test address parses"),
    );
    let (fresher, _) = reach::punch_target(&node, &endpoints).expect("both halves are held");
    assert_eq!(
        fresher,
        "203.0.113.9".parse::<IpAddr>().expect("parses"),
        "the freshest observation still wins over a row endpoint"
    );
}
