// ---------------------------------------------------------------------------
// Item 10: every cap, each driving the (n+1)th
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// **The fourth knock in ten seconds gets zero bytes.**
///
/// One per ten seconds, burst three: a person pressing Trust, mistyping and
/// pressing it again is never rate-limited, and a script is.
///
/// Driven from four DIFFERENT instance ids so that what refuses the fourth is
/// the bucket and not the pending-row coalescing: the rows coalesce either
/// way, so a test that could not tell them apart would pass with the bucket
/// deleted.
///
/// Watched red: delete the `take_knock_token` block from
/// `listener::serve_knock` and the fourth knock is taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fourth_knock_in_ten_seconds_gets_nothing() {
    let node = Node::new("cap-knock-bucket");
    let context = node.context();
    let admission = Arc::clone(context.admission());
    let addr = serve(context).await;

    for (n, id) in [0xB1_u8, 0xB2, 0xB3].into_iter().enumerate() {
        knock_at(addr, instance(id), None)
            .await
            .unwrap_or_else(|err| panic!("knock {n} is inside the burst of 3: {err:#}"));
    }

    let left = admission
        .lock()
        .expect("the admission lock")
        .knock_tokens("127.0.0.1", state::now_ms());
    assert_eq!(
        left, 0,
        "three knocks spend the whole burst, and {left} tokens are left"
    );

    let refused = knock_at(addr, instance(0xB4), None).await;
    assert!(
        refused.is_err(),
        "the fourth knock inside the interval must earn nothing at all"
    );
}

/// **The ninth outstanding knock is refused.**
///
/// The many-address flood: a `/24` of hosts each knocking once costs eight rows
/// and not 254. Driven against `PeerState` directly with eight distinct
/// addresses, because the transport half of this is loopback and every
/// connection from it shares one address: a socket-level test could not
/// produce nine distinct addresses on this box.
///
/// Watched red: delete the `pending.len() >= MAX_PENDING_KNOCKS` arm from
/// `record_knock` and the ninth is queued.
#[test]
fn the_ninth_outstanding_knock_is_refused() {
    let mut value = PeerState::default();
    for n in 0..state::MAX_PENDING_KNOCKS {
        let addr = format!("192.0.2.{n}");
        assert!(
            value
                .record_knock(&addr, instance(n as u8), None, 1, None, 1_000)
                .expect("inside the cap"),
            "row {n} is a new address and must be a new row"
        );
    }
    assert_eq!(value.knocks_queued(), state::MAX_PENDING_KNOCKS);

    let refused = value.record_knock("192.0.2.99", instance(0xFF), None, 1, None, 1_000);
    assert!(
        matches!(refused, Err(state::KnockRefusal::QueueFull { .. })),
        "the ninth outstanding knock must be refused, and this one returned {refused:?}"
    );
    assert_eq!(
        value.knocks_queued(),
        state::MAX_PENDING_KNOCKS,
        "a refused knock must leave no row"
    );

    // And an address ALREADY in the queue still updates at the cap, because a
    // full queue must not make a machine the operator is already looking at
    // unreachable.
    assert!(
        !value
            .record_knock("192.0.2.0", instance(0xFE), None, 1, None, 1_100)
            .expect("an existing row updates even at the cap"),
        "an existing address at the cap updates in place rather than being refused"
    );
}

/// **Eight concurrent reservations at a seven-row queue
/// leave exactly eight rows, never nine: and only one of the eight racers
/// wins the one free slot.**
///
/// `listener::serve_knock`'s pre-handshake admission check used to READ the
/// state file unlocked, then run the whole Noise handshake, then only under
/// `FileLock` write the real row via `record_knock`. Eight knocks arriving
/// together could all read the same "there is room" snapshot before any of
/// them wrote anything, so all eight ran the handshake and earned message 2
///: a real cost paid by seven connections the queue was always going to
/// refuse. The fix moved the check itself under the lock and turned it into
/// a RESERVATION (`PeerState::reserve_knock_slot`), so the second racer to
/// reach the lock sees the first racer's claim and is refused before any
/// handshake bytes are ever considered.
///
/// This drives that exact mechanism: `config::FileLock::acquire` plus
/// `PeerState::reserve_knock_slot`, on the real state file, from eight real
/// OS threads: rather than going through a socket. A genuine socket-level
/// race needs eight DISTINCT source addresses, and this box cannot produce
/// them: every outbound loopback connection binds `127.0.0.1` (binding a
/// second loopback address such as `127.0.0.2` fails with `EADDRNOTAVAIL`
/// unless it is configured as an interface alias, which needs root and is
/// not available here: confirmed with a plain `socket.bind`). The
/// admission mechanism under test is the state file and its lock, not the
/// TCP or Noise layer above it, so calling it directly with eight distinct
/// address strings measures the real fix; the wire-bytes half of the
/// invariant (a refused knock writes nothing) is covered on a real socket by
/// [`the_ninth_outstanding_knock_writes_zero_bytes_on_the_wire`] below, using
/// the one real address this box has.
///
/// Watched red: with `reserve_knock_slot` reverted to a read-only check (no
/// `self.pending.push`, i.e. exactly `refusal_for_knock_source(..)` wrapped
/// in `Ok`/`Err`), none of the eight threads ever sees a sibling's claim, so
/// this fails with `admitted` anywhere from 2 to 8 depending on scheduling
/// and `pending.len()` correspondingly above 8.
#[test]
fn eight_concurrent_reservations_at_a_seven_row_queue_leave_exactly_eight_never_nine() {
    let node = Node::new("cap-race");

    let mut seeded = PeerState::default();
    for n in 0..7_u8 {
        seeded
            .record_knock(&format!("192.0.2.{n}"), instance(n), None, 1, None, 1_000)
            .expect("seed row");
    }
    assert_eq!(
        seeded.knocks_queued(),
        7,
        "seed exactly seven rows before the race"
    );
    node.write_state(&seeded);

    let state_path = node.state.clone();
    let handles: Vec<std::thread::JoinHandle<Result<bool, state::KnockRefusal>>> = (0..8_u8)
        .map(|n| {
            let state_path = state_path.clone();
            std::thread::spawn(move || {
                // Eight distinct new addresses: TEST-NET-3, never a real
                // loopback source, so nothing here collides with a real
                // socket test's `127.0.0.1`.
                let addr = format!("203.0.113.{n}");
                let _lock = config::FileLock::acquire(&state_path).expect("acquire the state lock");
                let mut value = state::load(&state_path, 1_000).expect("load state");
                let outcome = value.reserve_knock_slot(&addr, 1_000);
                if outcome.is_ok() {
                    state::save(&state_path, &value).expect("save the reservation");
                }
                outcome
            })
        })
        .collect();

    let mut admitted = 0;
    let mut refused = 0;
    for handle in handles {
        match handle.join().expect("a racing thread must not panic") {
            Ok(true) => admitted += 1,
            Ok(false) => panic!(
                "every address here is brand new to this queue, so none of them should \
                 coalesce onto an existing row"
            ),
            Err(state::KnockRefusal::QueueFull { .. }) => refused += 1,
            Err(other) => panic!("unexpected refusal from a fresh, unbanned address: {other:?}"),
        }
    }

    assert_eq!(
        admitted, 1,
        "exactly one of the eight new addresses fits in the one free slot"
    );
    assert_eq!(
        refused, 7,
        "the other seven must be refused outright, not merely queued behind the winner"
    );

    let after = node.state();
    assert_eq!(
        after.knocks_queued(),
        state::MAX_PENDING_KNOCKS,
        "the queue must leave exactly eight rows, never nine, after the race: {after:?}"
    );
}

/// **The over-cap refusal writes zero bytes on the wire,
/// not just a zero-length `pending` delta.**
///
/// `the_ninth_outstanding_knock_is_refused` (above) proves the ROW count at
/// the `PeerState` level with fake addresses; it asserts nothing about bytes,
/// so a fix that moved the queue-full refusal to AFTER the handshake would
/// pass it unchanged. This proves the other half, on a real socket: seed the
/// queue to the cap with eight rows at addresses no real loopback connection
/// can ever present (`192.0.2.x`, TEST-NET-1), then knock for real from
/// `127.0.0.1` (a genuinely new, ninth address), and assert both the
/// refusal and that nothing came back over the wire.
///
/// Watched red: with the pre-fix unlocked read-then-handshake-then-write
/// order restored (the state at the top of this file, before
/// `reserve_knock_slot` existed), this fails with `written != 0`: the ninth
/// knock's connection completes the Noise handshake and receives message 2
/// before the locked `record_knock` call two reads later refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ninth_outstanding_knock_writes_zero_bytes_on_the_wire() {
    let node = Node::new("cap-bytes");

    let mut seeded = PeerState::default();
    for n in 0..state::MAX_PENDING_KNOCKS {
        seeded
            .record_knock(
                &format!("192.0.2.{n}"),
                instance(n as u8),
                None,
                1,
                None,
                state::now_ms(),
            )
            .expect("seed row");
    }
    node.write_state(&seeded);

    let addr = serve(node.context()).await;
    let written = count_written(addr, &knock_message_1()).await;
    assert_eq!(
        written, 0,
        "a knock from a genuinely new address against a full queue must write zero bytes, \
         and {written} came back"
    );

    let after = node.state();
    assert_eq!(
        after.knocks_queued(),
        state::MAX_PENDING_KNOCKS,
        "the refused knock must leave no row"
    );
}

/// **A knock sitting for ten minutes expires on its own.**
///
/// Nothing else leaves PENDING without the operator, which is the state
/// diagram's one exception: so the TTL is what keeps a queue that filled up
/// while nobody was at the Mac from staying full for ever.
#[test]
fn a_knock_expires_after_ten_minutes() {
    let mut value = PeerState::default();
    value
        .record_knock("192.0.2.7", instance(1), None, 1, None, 1_000)
        .expect("queued");
    value
        .record_knock("192.0.2.8", instance(2), None, 1, None, 1_000)
        .expect("queued");

    // One keeps asking; the other does not.
    value
        .record_knock(
            "192.0.2.7",
            instance(1),
            None,
            1,
            None,
            1_000 + state::KNOCK_TTL_MS,
        )
        .expect("re-knocking refreshes");
    let dropped = value.expire(1_000 + state::KNOCK_TTL_MS + 1);
    assert_eq!(dropped, 1, "exactly the silent row goes");
    assert_eq!(value.knocks_queued(), 1);
    assert!(value.find_pending("192.0.2.7").is_some());
}

/// **A mute lifts on its own after an hour; a ban does not lift at all.**
///
/// The two are different states and the difference is the operator's intent:
/// Ignore means "not now", Block means "never". A mute that needed unblocking,
/// or a ban that expired, would each be the other verb.
#[test]
fn a_mute_expires_and_a_ban_does_not() {
    let mut value = PeerState::default();
    value.mute("192.0.2.7", 1_000);
    value.ban("192.0.2.8", None, BanReason::Blocked, 1_000);

    assert!(value.is_muted("192.0.2.7", 1_000));
    value.expire(1_000 + state::MUTE_MS + 1);
    assert!(
        !value.is_muted("192.0.2.7", 1_000 + state::MUTE_MS + 1),
        "a mute must lift on its own, or Ignore is a second Block"
    );
    assert!(
        value.is_address_banned("192.0.2.8"),
        "a ban must survive every expiry, or Block is a second Ignore"
    );
    assert!(value.unblock("192.0.2.8"), "only unblock clears a ban");
    assert!(!value.is_address_banned("192.0.2.8"));
}

/// **A first frame above the pre-authentication bound is refused before the
/// allocation.**
///
/// `read_frame` sizes a `Vec` from the peer-controlled two-byte length prefix,
/// so without a bound any host that can reach the port makes this node reserve
/// 65 kB by writing two bytes and then nothing: and with sixteen
/// unauthenticated sockets allowed that is a megabyte pinned for 32 bytes of
/// traffic.
///
/// The number is [`noise::MAX_MESSAGE_1_BYTES`], which is the largest message 1
/// any pattern here has. `abuse-resistance.md` says 64, and 64 is wrong: an
/// `IK` message 1 is 96 bytes, so that cap would refuse every returning peer
/// and every headless enrolment.
///
/// Watched red: change `read_frame_bounded`'s guard to `if false` and the frame
/// is accepted (and then fails further in, on the pattern dispatch, which is
/// after the allocation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_frame_above_the_bound_is_refused_before_allocation() {
    let (mut reader, mut writer) = tokio::io::duplex(8192);
    let oversized = noise::MAX_MESSAGE_1_BYTES + 1;
    let mut framed = Vec::new();
    framed.extend_from_slice(&u16::try_from(oversized).expect("fits").to_be_bytes());
    framed.resize(2 + oversized, 0);
    writer.write_all(&framed).await.expect("write the frame");
    writer.flush().await.expect("flush");

    let refused = noise::read_frame_bounded(&mut reader, noise::MAX_MESSAGE_1_BYTES).await;
    let error = refused.expect_err("an oversized first frame must be refused");
    assert!(
        format!("{error:#}").contains("refused BEFORE allocating"),
        "the refusal has to name the reason, because the whole point is WHERE it happens: \
         {error:#}"
    );

    // The positive control: exactly the bound is accepted, so the refusal is
    // the bound and not a reader that refuses everything.
    let (mut reader, mut writer) = tokio::io::duplex(8192);
    let mut framed = Vec::new();
    framed.extend_from_slice(
        &u16::try_from(noise::MAX_MESSAGE_1_BYTES)
            .expect("fits")
            .to_be_bytes(),
    );
    framed.resize(2 + noise::MAX_MESSAGE_1_BYTES, 0);
    writer.write_all(&framed).await.expect("write the frame");
    writer.flush().await.expect("flush");
    let accepted = noise::read_frame_bounded(&mut reader, noise::MAX_MESSAGE_1_BYTES)
        .await
        .expect("a frame exactly at the bound must be read");
    assert_eq!(accepted.len(), noise::MAX_MESSAGE_1_BYTES);
}

/// **A socket opened and left silent is closed after five seconds.**
///
/// The cheapest thing a stranger can do, and the one that would otherwise hold
/// an unauthenticated slot for as long as the handshake timeout: ten seconds
/// for a connection that has not said anything at all.
///
/// Watched red: raise `MESSAGE_1_TIMEOUT` to 60 seconds and this fails on the
/// elapsed assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_socket_is_closed_after_the_message_one_timeout() {
    let node = Node::new("cap-silence");
    let addr = serve(node.context()).await;

    let started = std::time::Instant::now();
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut back = Vec::new();
    // The far side closes when the timeout fires, which ends this read.
    let read = tokio::time::timeout(
        listener::MESSAGE_1_TIMEOUT * 3,
        stream.read_to_end(&mut back),
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        read.is_ok(),
        "the listener must close a silent socket rather than hold it, and it was still \
         open after {elapsed:?}"
    );
    assert_eq!(
        back.len(),
        0,
        "and it must close with nothing written: a silent connection learns nothing"
    );
    assert!(
        elapsed < listener::MESSAGE_1_TIMEOUT * 2,
        "the close took {elapsed:?}, which is well past the {:?} bound",
        listener::MESSAGE_1_TIMEOUT
    );
}

/// **The third unauthenticated socket from one address is refused.**
///
/// Two per address, sixteen in total. Driven by holding two silent connections
/// open: silent, because a connection that has not delivered message 1 is
/// exactly what the counter counts, and then opening a third.
///
/// Watched red: delete the `per_address >= MAX_UNAUTHENTICATED_PER_ADDRESS`
/// arm from `SocketSlot::acquire` and the third connection is held for the full
/// message-1 timeout instead of being closed at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_third_unauthenticated_socket_from_one_address_is_refused() {
    let node = Node::new("cap-sockets");
    let context = node.context();
    let admission = Arc::clone(context.admission());
    let addr = serve(context).await;

    // Two silent connections, held open.
    let mut held = Vec::new();
    for _ in 0..listener::MAX_UNAUTHENTICATED_PER_ADDRESS {
        held.push(TcpStream::connect(addr).await.expect("connect"));
    }
    // Wait until the listener has really counted both, rather than assuming the
    // accept loop has run: the counter is the thing under test, so reading it
    // is the honest way to know the setup is in place.
    for _ in 0..200 {
        if admission
            .lock()
            .expect("the admission lock")
            .live_unauthenticated("127.0.0.1")
            >= listener::MAX_UNAUTHENTICATED_PER_ADDRESS
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        admission
            .lock()
            .expect("the admission lock")
            .live_unauthenticated("127.0.0.1"),
        listener::MAX_UNAUTHENTICATED_PER_ADDRESS,
        "setup: both silent connections must be counted before the third is opened"
    );

    // The third is closed at once: well inside the message-1 timeout, which is
    // what distinguishes "refused by the cap" from "timed out like the others".
    let mut third = TcpStream::connect(addr).await.expect("connect");
    let mut back = Vec::new();
    let closed = tokio::time::timeout(
        listener::MESSAGE_1_TIMEOUT / 2,
        third.read_to_end(&mut back),
    )
    .await;
    assert!(
        closed.is_ok(),
        "the third unauthenticated socket from one address must be closed at once, not \
         held until the message-1 timeout"
    );
    assert_eq!(back.len(), 0, "and closed with nothing written");

    // The slot is released when a connection ends, so the cap is not a
    // permanent lockout after two refusals: the failure mode an RAII-free
    // counter has.
    drop(held);
    for _ in 0..200 {
        if admission
            .lock()
            .expect("the admission lock")
            .live_unauthenticated("127.0.0.1")
            == 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        admission
            .lock()
            .expect("the admission lock")
            .live_unauthenticated("127.0.0.1"),
        0,
        "every slot must be released when its connection ends, or sixteen refusals lock \
         this node out of its own mesh"
    );
}

/// **Three pinned-peer handshakes at once from ONE address all complete.**
///
/// The regression for a shipped defect, later measured: the
/// pre-authentication socket slot originally covered the whole handshake, so
/// `MAX_UNAUTHENTICATED_PER_ADDRESS` (two), silently became a cap on how many
/// streams one pinned peer could have open at once. One TCP is one Noise
/// session is one stream in this design, so a lender serving a borrower's
/// `max_inflight: 5` refused the third with zero bytes and the borrower read it
/// as "the handshake with the lender failed".
///
/// Caught by `borrowed_requests_are_paced_by_the_lenders_own_bucket`
/// (`tests/peer/lending/peer_lease.rs`) in the full workspace run, not
/// by anything this file wrote: which is why it is pinned HERE, in the file
/// that owns the cap, rather than left to a neighbour's test to notice again.
///
/// The peer's grant is `max_inflight: 5`, the same figure the lease test uses,
/// so the allowance this exercises is the operator's own number and not one
/// this test invented.
///
/// **Every socket is connected BEFORE any of them writes**, deliberately: that
/// is the state the cap actually sees. The accept loop spawns a task per
/// connection and the task reads message 1, so three simultaneous `connect`s
/// are three accepted-and-silent sockets whatever the handshake costs: and a
/// version of this test that connected-and-handshook in one step passed by luck
/// on the interleaving while the lease test kept failing.
///
/// Watched red twice. With the caps as fixed constants
/// (`MAX_UNAUTHENTICATED_PER_ADDRESS` alone, no
/// [`listener::unauthenticated_allowance`]), this fails with the third dial
/// reporting "message 2 did not authenticate"; that is the shape the lease test
/// reported as "the handshake with the lender failed". With the `drop(slot)`
/// moved back below `accept_pairing_or_return` it fails the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_handshakes_up_to_the_granted_inflight_all_complete() {
    /// The lease test's own figure.
    const GRANTED_INFLIGHT: u8 = 5;

    let node = Node::new("concurrent-handshakes");
    let (peer_secret, peer_public) = noise::generate_static().expect("a peer keypair");
    let mut file = node.file();
    file.pin(config::PeerRow {
        node: PeerId(peer_public),
        label: "laptop-2".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow {
            inspect: true,
            ..config::Allow::default()
        },
        lend: vec![config::LendGrant::new(
            tcr_peer_wire::Window::SevenDay,
            0.5,
            300,
            GRANTED_INFLIGHT,
        )],
    });
    node.write_file(&file);

    // The allowance really is derived from that grant, and it really is above
    // the bare constant: otherwise the assertion below would pass for the
    // wrong reason.
    let (total, per_address) = listener::unauthenticated_allowance(&node.file());
    assert_eq!(
        per_address,
        listener::MAX_UNAUTHENTICATED_PER_ADDRESS + usize::from(GRANTED_INFLIGHT),
        "the per-address allowance must be the stranger's floor plus what was granted"
    );
    assert!(total >= per_address);

    let addr = serve(node.context()).await;
    let responder_public = node.key.id().0;

    let concurrent = usize::from(GRANTED_INFLIGHT);

    // Every socket open and silent before any handshake starts. This is the
    // state the cap sees.
    let mut sockets = Vec::new();
    for _ in 0..concurrent {
        sockets.push(TcpStream::connect(addr).await.expect("connect"));
    }

    let dials: Vec<_> = sockets
        .into_iter()
        .enumerate()
        .map(|(n, mut stream)| {
            tokio::spawn(async move {
                noise::dial_handshake(
                    &mut stream,
                    &peer_secret,
                    Handshake::Return,
                    Some(&responder_public),
                    None,
                )
                .await
                .map(|_| n)
                .map_err(|err| (n, format!("{err:#}")))
            })
        })
        .collect();

    let mut failed = Vec::new();
    for dial in dials {
        match dial.await.expect("the dialling task") {
            Ok(_) => {}
            Err((n, err)) => failed.push((n, err)),
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {concurrent} concurrent handshakes from one pinned peer were refused: \
         {failed:?}. A pre-authentication cap below the concurrency the operator granted \
         is a cap on how many streams that peer may have open",
        failed.len()
    );
}

/// The caps are exactly `abuse-resistance.md`'s numbers when the operator has
/// granted nothing, which is the default and the home-LAN case.
///
/// Without this, the derived allowance above could have quietly raised the
/// stranger's ceiling for everybody: the thing it must not do.
#[test]
fn with_nothing_granted_the_allowance_is_the_documented_pair() {
    let bare = PeerFile::default();
    assert_eq!(
        listener::unauthenticated_allowance(&bare),
        (
            listener::MAX_UNAUTHENTICATED_SOCKETS,
            listener::MAX_UNAUTHENTICATED_PER_ADDRESS
        ),
        "a fresh config must be judged against 16 and 2 and nothing else"
    );

    // A pinned peer with no LEND grant raises nothing either: what raises the
    // ceiling is granted concurrency, never the mere existence of a peer.
    let pinned = PeerFile {
        peers: vec![config::PeerRow {
            node: PeerId([1; 32]),
            label: "laptop-2".to_string(),
            endpoints: Vec::new(),
            added_at: 0,
            rendezvous_secret: None,
            sees_us_at: None,
            allow: config::Allow::default(),
            lend: Vec::new(),
        }],
        ..PeerFile::default()
    };
    assert_eq!(
        listener::unauthenticated_allowance(&pinned),
        (
            listener::MAX_UNAUTHENTICATED_SOCKETS,
            listener::MAX_UNAUTHENTICATED_PER_ADDRESS
        ),
        "pinning a peer is not granting it concurrency"
    );
}
