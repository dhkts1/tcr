//! The caps on a connection once it is open: a failed accept, slow and headerless
//! sessions, the gateway allow-list, bans on every arm, the hop budget and a
//! carry that died inside the pump.

use super::*;

// ---------------------------------------------------------------------------
// One failed accept is one connection, not the end of the mesh
// ---------------------------------------------------------------------------

/// **The errors a busy machine really produces do not end the accept loop.**
///
/// The loop used to propagate every `accept` error. `ECONNABORTED` (a peer
/// that hung up between the SYN and the accept) and `EMFILE` (this process
/// momentarily out of descriptors, which a burst of leases or anything else in
/// the proxy can cause) are both ordinary events on a busy node, and either
/// one ended the peer listener for the life of the process: `server::supervise`
/// supervises a task that has already returned, so nothing starts it again.
///
/// Watch it fail: make `accept_error_is_fatal` answer `true` for everything
/// and the first three rows below go red.
#[test]
fn a_transient_accept_error_does_not_end_the_listener() {
    use std::io::{Error, ErrorKind};

    // ECONNABORTED (53 on macOS, 103 on Linux) and EMFILE/ENFILE: one
    // connection's problem, or a shortage that clears.
    for transient in [
        Error::from(ErrorKind::ConnectionAborted),
        Error::from_raw_os_error(24), // EMFILE
        Error::from_raw_os_error(23), // ENFILE
        Error::from(ErrorKind::Interrupted),
        Error::from(ErrorKind::WouldBlock),
    ] {
        assert!(
            !listener::accept_error_is_fatal(&transient),
            "{transient:?} is one connection's problem; ending the accept loop over it takes \
             the whole mesh down until the next restart"
        );
    }

    // EBADF and a listener that is not a socket: retrying answers the same way
    // forever, so the loop has to end and say so.
    for fatal in [
        Error::from_raw_os_error(9), // EBADF
        Error::from(ErrorKind::InvalidInput),
        Error::from(ErrorKind::NotConnected),
    ] {
        assert!(
            listener::accept_error_is_fatal(&fatal),
            "{fatal:?} says the listening socket itself is gone; retrying it is a spin"
        );
    }
}

/// **A third knock in flight from one address gets nothing back.**
///
/// The socket slot used to be released the instant message 1 was in hand, for
/// every pattern. That is right for a connection going on to authenticate, and
/// wrong for a knock: a knock never authenticates, and everything after its
/// message 1 is a state-file write under a lock that waits with
/// `std::thread::sleep`. So the knock path had no in-flight bound at all, and
/// one LAN host could hold as many of them open as it could open sockets.
///
/// Watch it fail: move the `drop(slot)` in `serve_connection` back above the
/// knock arm and the third connection is admitted, the counter reads zero
/// while all three are in flight, and this goes red on both assertions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_knocks_in_flight_from_one_address_hold_two_slots_and_the_third_gets_nothing() {
    let node = Node::new("knock-in-flight");
    let context = node.context();
    let addr = serve(context.clone()).await;

    // Two knocks that deliver a valid message 1 and then say nothing: both
    // are mid-knock, so both hold a slot.
    let mut held = Vec::new();
    for _ in 0..plan::MAX_UNAUTHENTICATED_PER_ADDRESS {
        held.push(stall_after(addr, &knock_message_1()).await);
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK)),
        plan::MAX_UNAUTHENTICATED_PER_ADDRESS,
        "a knock in flight has authenticated nothing, so it has to hold its slot for as long \
         as it is in flight"
    );

    // The third one is refused with zero bytes written, which is what a bound
    // means on the wire.
    assert_eq!(
        count_written(addr, &knock_message_1()).await,
        0,
        "the third knock in flight from one address is over the per-address allowance and \
         must get nothing back"
    );

    // And the slots come back: the stalls hit `MESSAGE_1_TIMEOUT` inside the
    // knock handshake, the connections close, and the counter empties.
    drop(held);
    tokio::time::sleep(plan::MESSAGE_1_TIMEOUT + Duration::from_secs(2)).await;
    assert_eq!(
        with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK)),
        0,
        "every slot is released on every exit path, or the bound becomes a lockout"
    );
}

// ===========================================================================
// Cap: what an unauthenticated connection may hold, and for how long
// (`listener::MAX_UNAUTHENTICATED_SOCKETS`, `listener::HANDSHAKE_TIMEOUT`)
// ===========================================================================

/// Hold the lock for `path` from another thread, from now until the returned
/// handle is joined.
///
/// The contention every write on the listener's own paths meets in real use:
/// `tcr peer accept` writing the state file, or a crashed holder that left the
/// lockfile behind. A waiter breaks a lockfile older than
/// `config::LOCK_STALE_MS` (two seconds), so this holds one for longer than
/// that and lets the waiter break it: two seconds is the longest a single
/// contended write can be made to wait, and it is the lever this file has for
/// stretching one.
fn hold_the_lock(path: &std::path::Path) -> std::thread::JoinHandle<()> {
    let path = path.to_path_buf();
    let (ready, taken) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let lock = config::FileLock::acquire(&path).expect("take the lock");
        ready.send(()).expect("the test is still waiting");
        // Long past `LOCK_STALE_MS`: the waiter breaks it, which is exactly
        // the two-second wait this is here to impose.
        std::thread::sleep(Duration::from_secs(10));
        drop(lock);
    });
    taken
        .recv()
        .expect("the lock is taken before the test goes on");
    handle
}

/// **A knock that is slow at every step still gives its socket slot back
/// within one handshake.**
///
/// Every step under that slot used to carry its own deadline and nothing added
/// them up: five seconds to deliver message 1, then a wait on the state file's
/// lock, then a fresh five seconds for the rest of the knock handshake. A
/// knocker that spent all three held a sixteenth of this node's
/// unauthenticated capacity for longer than a connection that really does
/// authenticate is allowed.
///
/// The attack, in the timings below: message 1 delivered at 4.3 s (inside its
/// own five-second bound), the state file's lock taken just before it lands so
/// the reservation waits the full stale bound, and then silence. Before the
/// fix that adds up to about 11.3 s under the slot; the bound asserted here is
/// [`plan::HANDSHAKE_TIMEOUT`] plus 800 ms of scheduling slack.
///
/// Watch it fail: drop `slot_deadline` from `serve_knock` and the release
/// lands past the bound, with the elapsed time in the message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_that_is_slow_at_every_step_gives_its_slot_back_within_a_handshake() {
    let node = Node::new("slow-knock-slot");
    let context = node.context();
    let addr = serve(context.clone()).await;

    let started = std::time::Instant::now();
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("connect to the test listener");

    // Late, but inside message 1's own bound: the slot has been held for 4.3 s
    // by the time this node has read a single frame.
    tokio::time::sleep(Duration::from_millis(4_300)).await;
    let holder = hold_the_lock(&node.state);
    let message_1 = knock_message_1();
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(&message_1);
    stream.write_all(&framed).await.expect("write message 1");
    stream.flush().await.expect("flush message 1");

    // Then nothing at all, and the slot is watched until it comes back.
    let mut released = None;
    while started.elapsed() < Duration::from_secs(20) {
        if with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK)) == 0 {
            released = Some(started.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let released = released.unwrap_or_else(|| {
        panic!(
            "the slot was still held after {:?}, which is a sixteenth of this node's \
             unauthenticated capacity held by a socket that said one frame",
            started.elapsed()
        )
    });

    // The positive control on the clock: a knock that was refused at once
    // would satisfy the bound below while proving nothing about it.
    assert!(
        released >= plan::MESSAGE_1_TIMEOUT,
        "positive control: this knock has to have been admitted and worked on, and it was \
         released after only {released:?}"
    );
    assert!(
        released <= plan::HANDSHAKE_TIMEOUT + Duration::from_millis(800),
        "an unauthenticated knock held its slot for {released:?}, and the longest any \
         connection may hold one is a handshake ({:?})",
        plan::HANDSHAKE_TIMEOUT
    );

    drop(stream);
    holder.join().expect("the lock holder finishes");
}

/// **A `Hello` arriving while the peers file is locked does not park the
/// proxy's runtime.**
///
/// Every state-file write on the listener's own unauthenticated paths moved to
/// a blocking thread, and `on_blocking_thread`'s own doc claimed all of them
/// had. The CONTROL path had not: a `Hello` makes four locked
/// read-modify-writes of the peers file, and `FileLock::acquire` waits for a
/// contended lock with `std::thread::sleep`. Run inline on the proxy's
/// runtime, one Hello per worker thread parks every one of them, and what
/// stops answering is the LOCAL proxy: the thing the mesh is not allowed to
/// cost anything.
///
/// What is measured is a task spawned onto the same runtime while those Hellos
/// are in flight, and how long it waited to be run at all. Two workers and two
/// Hellos, because the shape of the defect is one blocked worker per frame.
///
/// Watch it fail: call `observe_rendezvous_secret` inline in the `Hello` arm
/// again and the probe waits for the lock rather than for a scheduler slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hello_arriving_while_the_peers_file_is_locked_leaves_the_runtime_free() {
    let listening = Node::new("hello-lock-listener");
    let caller = Node::new("hello-lock-caller");
    let addr = serve(listening.context()).await;

    let listening_store = config::PeerStore::open(&listening.peers).expect("open the store");
    let caller_store = config::PeerStore::open(&caller.peers).expect("open the store");
    pair::confirm(&listening_store, &caller.key.id(), "111111", None)
        .expect("the listening node pins the caller");
    pair::confirm(&caller_store, &listening.key.id(), "222222", Some(addr))
        .expect("the caller pins the listening node");

    let holder = hold_the_lock(&listening.peers);

    // Two Hellos, released together onto a two-worker runtime.
    let target = listening.key.id();
    let mut hellos = Vec::new();
    for _ in 0..2 {
        let store = config::PeerStore::open(&caller.peers).expect("open the store");
        hellos.push(tokio::spawn(async move {
            tcr::peer::serve::say_hello(&store, &target).await
        }));
    }

    // The local proxy's stand-in: a task that does nothing but report how long
    // it waited for a worker. Spawned from OUTSIDE the runtime, after the
    // Hellos are in flight, because a runtime whose every worker is parked
    // cannot run the task that would notice.
    let handle = tokio::runtime::Handle::current();
    let (waited, latency) = std::sync::mpsc::channel();
    let prober = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let spawned = std::time::Instant::now();
        handle.spawn(async move {
            waited
                .send(spawned.elapsed())
                .log_debug("reporting the probe's wait");
        });
    });

    let waited_for_a_worker = latency
        .recv_timeout(Duration::from_secs(20))
        .expect("the probe task ran at all");
    assert!(
        waited_for_a_worker < Duration::from_secs(1),
        "a task spawned while two Hellos were in flight waited {waited_for_a_worker:?} for a \
         worker thread; the peers-file writes a Hello makes belong on a blocking thread, or \
         a peer that says hello while anything holds that lock stops the local proxy"
    );

    prober.join().expect("the prober finishes");
    for hello in hellos {
        // Answered or refused is not what this test is about: the runtime is.
        hello
            .await
            .log_debug("joining a test hello")
            .and_then(|answered| answered.log_debug("saying hello under the held lock"));
    }
    holder.join().expect("the lock holder finishes");
}

/// **The gate for the punch bound**: a `PunchAt` frame spawns a task that
/// sleeps to a slot boundary, up to thirty seconds, and then binds sockets.
/// Nothing counted those tasks, so a pinned peer could hold as many sleeping
/// tasks and as many pending binds as it managed to send frames. One punch per
/// peer is the bound, because a peer cannot be punching two slots at once.
///
/// Driven through the guard rather than through a real punch: binding sockets
/// and waiting out a slot boundary would measure the network. Watch it fail by
/// making `PunchInFlight::acquire` ignore what the set insert returned, which
/// is the unbounded behaviour this replaced.
#[test]
fn a_peer_gets_one_in_flight_punch_and_no_more() {
    let admission = Arc::new(std::sync::Mutex::new(Admission::new()));
    let peer = PeerId([7_u8; 32]);
    let other = PeerId([9_u8; 32]);

    let first =
        listener::PunchInFlight::acquire(&admission, peer).expect("the first punch takes the slot");
    assert!(
        listener::PunchInFlight::acquire(&admission, peer).is_none(),
        "a second PunchAt from the same peer, while the first is still sleeping to its slot \
         boundary, takes no slot and spawns nothing"
    );
    let elsewhere = listener::PunchInFlight::acquire(&admission, other)
        .expect("another peer's punch is not blocked by this one");
    assert_eq!(
        with_admission_arc(&admission, |guard| guard.punches_in_flight()),
        2,
        "the bound is per peer, so two peers punching at once is two slots"
    );

    drop(first);
    assert_eq!(
        with_admission_arc(&admission, |guard| guard.punches_in_flight()),
        1,
        "a punch that ended releases its slot, whether it connected or gave up"
    );
    let again = listener::PunchInFlight::acquire(&admission, peer)
        .expect("and the peer can punch again once its last punch is done");

    drop(again);
    drop(elsewhere);
    assert_eq!(
        with_admission_arc(&admission, |guard| guard.punches_in_flight()),
        0,
        "nothing leaks: a peer that punched is not locked out for the life of the process"
    );
}

// ===========================================================================
// Cap: the stream header after an authenticated handshake
// (`listener::STREAM_HEADER_TIMEOUT`, `listener::MAX_HEADERLESS_PER_PEER`)
// ===========================================================================

/// **A pinned peer that finishes its handshake and then says nothing is closed
/// on a deadline, and the sessions it has waiting are counted.**
///
/// The pre-authentication socket slot is given back the instant a handshake
/// completes, and until this deadline existed nothing counted what happened
/// after: the task parked on the header read forever. The far side does not
/// even have to stay: a replayed message 1 reaches the same state.
///
/// A real handshake over a real socket against the shipped accept loop,
/// because what is under test is the state the loop is left in, not a verdict.
/// Watch it fail by taking the `tokio::time::timeout` off the header read in
/// `serve_stream`: the read below never returns and the outer timeout fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authenticated_peer_that_never_sends_a_header_is_closed_on_a_deadline() {
    let node = Node::new("headerless");
    let client_dir = scratch("headerless-dialler");
    let client = NodeKey::load_or_mint(&client_dir).expect("mint the dialler's key");

    let mut file = node.file();
    file.pin(PeerRow {
        node: client.id(),
        label: "dialler".to_string(),
        endpoints: vec![Endpoint::direct(
            "192.0.2.11:9600"
                .parse()
                .expect("a fixture address is a socket address"),
            0,
            EndpointSource::Paired,
        )],
        added_at: 1_700_000_000_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    });
    node.write_file(&file);

    let context = node.context();
    let addr = serve(context.clone()).await;
    let mut stream = TcpStream::connect(addr).await.expect("dial the listener");

    // The pinned dialler's own `IK`, finished, and then nothing.
    let server_static = node.key.id().0;
    let mut handshake = noise::initiator(&client, noise::PATTERN_RETURN, &server_static, None)
        .expect("an IK initiator");
    let mut scratch_bytes = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    let len = handshake
        .write_message(&[], &mut scratch_bytes)
        .expect("IK message 1");
    noise::write_frame(&mut stream, &scratch_bytes[..len])
        .await
        .expect("send message 1");
    let message_2 = noise::read_frame(&mut stream)
        .await
        .expect("the responder answers a pinned IK");
    let mut payload = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    handshake
        .read_message(&message_2, &mut payload)
        .expect("read message 2");

    // The session is authenticated and has sent no header: it is counted.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let waiting = with_admission(&context, |guard| guard.headerless_sessions(&client.id()));
    assert_eq!(
        waiting, 1,
        "an authenticated session with no header yet must be counted, or the per-peer \
         allowance counts nothing"
    );

    let started = std::time::Instant::now();
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(
        listener::STREAM_HEADER_TIMEOUT * 3,
        stream.read(&mut byte[..]),
    )
    .await
    .expect("the listener closes a headerless session rather than parking on it forever");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "the socket must be closed, not written to: {read:?}"
    );
    assert!(
        started.elapsed() < listener::STREAM_HEADER_TIMEOUT * 2,
        "the close took {:?} and the deadline is {:?}",
        started.elapsed(),
        listener::STREAM_HEADER_TIMEOUT
    );

    let after = with_admission(&context, |guard| guard.headerless_sessions(&client.id()));
    assert_eq!(
        after, 0,
        "and the slot comes back on the deadline path, or the cap becomes a lockout"
    );
}

/// **The allowance itself**: four sessions waiting for a header is all one peer
/// gets, and each one gives its slot back.
///
/// Driven through the guard rather than four real handshakes, for the reason
/// the punch bound is: what is under test is the counter.
#[test]
fn a_fifth_headerless_session_from_one_peer_is_refused() {
    let admission = Arc::new(std::sync::Mutex::new(Admission::new()));
    let peer = PeerId([11_u8; 32]);
    let other = PeerId([12_u8; 32]);

    let mut held = Vec::new();
    for _ in 0..listener::MAX_HEADERLESS_PER_PEER {
        held.push(
            listener::HeaderlessSession::acquire(&admission, peer)
                .expect("every session inside the allowance is taken"),
        );
    }
    assert_eq!(
        listener::HeaderlessSession::acquire(&admission, peer)
            .err()
            .expect("the session past the allowance is refused"),
        listener::MAX_HEADERLESS_PER_PEER,
        "the refusal reports what this peer already holds"
    );
    let elsewhere = listener::HeaderlessSession::acquire(&admission, other)
        .expect("another peer is not shut out by this one");

    held.pop();
    let again = listener::HeaderlessSession::acquire(&admission, peer)
        .expect("a session that ended gave its slot back");
    drop(again);
    drop(held);
    drop(elsewhere);
    assert_eq!(
        with_admission_arc(&admission, |guard| guard.headerless_sessions(&peer)),
        0,
        "nothing leaks: a peer that waited is not locked out for the life of the process"
    );
}

// ===========================================================================
// Cap: the gateway's origin allow-list is a name AND a port
// (`tunnel::allowed_origin`, `egress::PEER_EGRESS_PORT`)
// ===========================================================================

/// **A granted peer gets one port on an allow-listed name, not every service
/// behind it.**
///
/// A blind hop can enforce host-and-port and nothing else, and the gateway used
/// to read only the name: the port half lived on the asking node, which is the
/// side a peer that wants to reach something else is not running. So a pinned
/// peer could ask an allow-listed gateway for any port on `api.anthropic.com`,
/// and the gateway dialled it.
///
/// Watch it fail by dropping the port comparison from `allowed_origin`: every
/// refusal below becomes the allowed tuple.
#[test]
fn a_gateway_carries_an_allow_listed_name_on_one_port_only() {
    use tcr::peer::egress::PEER_EGRESS_HOSTS;
    use tcr::peer::tunnel;
    use tcr::peer::tunnel::PEER_EGRESS_PORT;

    let peer = PeerId([13_u8; 32]);
    let carried = tcr_peer_wire::TunnelTarget::Origin {
        host: "api.anthropic.com".to_string(),
        port: PEER_EGRESS_PORT,
    };
    assert_eq!(
        tunnel::allowed_origin(&carried, PEER_EGRESS_HOSTS, peer).expect("the carried origin"),
        ("api.anthropic.com", PEER_EGRESS_PORT),
        "positive control: the one host-and-port this gateway exists to carry"
    );

    for port in [22_u16, 80, 8443, 9200] {
        let elsewhere = tcr_peer_wire::TunnelTarget::Origin {
            host: "api.anthropic.com".to_string(),
            port,
        };
        let refusal = tunnel::allowed_origin(&elsewhere, PEER_EGRESS_HOSTS, peer)
            .expect_err("a port this gateway does not carry is refused");
        let said = format!("{refusal:#}");
        assert!(
            said.contains(&format!("port {PEER_EGRESS_PORT}")),
            "the refusal must name the port that IS carried, so an operator reading the log \
             knows what was asked and what is allowed: {said}"
        );
    }

    let unknown = tcr_peer_wire::TunnelTarget::Origin {
        host: "example.com".to_string(),
        port: PEER_EGRESS_PORT,
    };
    assert!(
        tunnel::allowed_origin(&unknown, PEER_EGRESS_HOSTS, peer).is_err(),
        "and the name half is unchanged: a host off the list is still refused"
    );
}

// ===========================================================================
// Cap: a banned key stays banned on every arm, enrolment included
// ===========================================================================

/// **A blocked key holding a live invite still gets nothing.**
///
/// The Pair arm and the Return arm both refuse a banned static key inside the
/// pin callback, before message 2 is written. The enrolment arm did not, and
/// its authorization is a PSK anybody with the invite holds, so a key the
/// operator blocked could dial from an address the ban does not name and
/// enrol. A ban an invite silently lifts is not a ban.
///
/// Both halves are here on one listener: the blocked key gets zero bytes back,
/// and a second key with the same invite is answered, which is what makes the
/// first assertion about the BAN rather than about a broken invite.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_banned_key_is_refused_on_the_enrolment_arm_too() {
    let node = Node::new("enrol-ban");
    let blocked = NodeKey::load_or_mint(&scratch("enrol-ban-blocked")).expect("the blocked key");
    let welcome = NodeKey::load_or_mint(&scratch("enrol-ban-welcome")).expect("a second joiner");
    let invite_secret = [3_u8; 32];

    let mut file = node.file();
    file.add_invite(config::PendingInvite {
        id: 1,
        label: "joiner".to_string(),
        secret: invite_secret,
        expires_at_ms: state::now_ms() + 600_000,
        uses_left: 5,
    });
    node.write_file(&file);

    // Banned by KEY at an address this test never dials from, so the address
    // half of the ban cannot be what refuses the connection below.
    let mut peer_state = node.state();
    peer_state.ban(
        "192.0.2.77",
        Some(blocked.id()),
        state::BanReason::Blocked,
        state::now_ms(),
    );
    node.write_state(&peer_state);

    let addr = serve(node.context()).await;
    let server_static = node.key.id().0;

    let offered = |joiner: &NodeKey| {
        let secret = *joiner.secret_bytes();
        async move {
            let mut stream = TcpStream::connect(addr).await.expect("dial the listener");
            let mut handshake = noise::initiator_with_secret(
                &secret,
                noise::PATTERN_ENROL,
                Some(&server_static),
                Some(&invite_secret),
            )
            .expect("an IKpsk1 initiator");
            let mut scratch_bytes = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
            let len = handshake
                .write_message(&[], &mut scratch_bytes)
                .expect("enrolment message 1");
            noise::write_frame(&mut stream, &scratch_bytes[..len])
                .await
                .expect("send message 1");
            tokio::time::timeout(Duration::from_secs(15), noise::read_frame(&mut stream))
                .await
                .expect("the listener answers or closes within the handshake deadline")
        }
    };

    let refused = offered(&blocked).await;
    assert!(
        refused.is_err(),
        "a blocked key must get nothing back, invite or no invite: {refused:?}"
    );

    let answered = offered(&welcome).await;
    assert!(
        answered.is_ok(),
        "positive control: the same invite from a key nobody blocked is answered, so the \
         refusal above is the ban and not a broken invite: {answered:?}"
    );
}

// ===========================================================================
// Cap: the hop budget on a forwarded frame is this Mac's, not the sender's
// ===========================================================================

/// **A frame that arrives carrying more hops than this Mac grants is refused,
/// not clamped into a log line.**
///
/// `hops_remaining` is a number the requester wrote. The clamp was applied to
/// the onward count, which rides on nothing (a blind forward writes no header
/// of its own), so a requester that asked for 255 hops was authorized on the
/// budget it had chosen for itself and only a log field disagreed.
///
/// Watch it fail by taking the arrival refusal out of `authorize_forward`: the
/// over-ask below comes back as a plan.
#[test]
fn a_forward_asking_for_more_hops_than_this_mac_grants_is_refused() {
    use tcr::peer::config::PeerStore;
    use tcr::peer::tunnel::{self, Carry, Forward, OriginRoute, TunnelBudget};

    let requester = PeerId([21_u8; 32]);
    let target = PeerId([22_u8; 32]);
    let me = PeerId([23_u8; 32]);
    let max_hops = 1_u8;

    let relay_row = |node: PeerId, label: &str, relay: bool| PeerRow {
        node,
        label: label.to_string(),
        endpoints: vec![Endpoint::direct(
            "192.0.2.30:9600"
                .parse()
                .expect("a fixture address is a socket address"),
            0,
            EndpointSource::Paired,
        )],
        added_at: 1_700_000_000_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            relay,
            ..Allow::default()
        },
        lend: Vec::new(),
    };
    let path = scratch("forward-hops").join("tcr-peers.json");
    config::save(
        &path,
        &PeerFile {
            max_hops,
            peers: vec![
                relay_row(requester, "asker", true),
                relay_row(target, "onward", false),
            ],
            ..PeerFile::default()
        },
    )
    .expect("write the peers file");
    let store = PeerStore::open(&path).expect("open the peers file");

    let asked = tcr_peer_wire::TunnelTarget::Peer { node: target };
    let decide = |hops_remaining: u8| {
        let budget = std::sync::Mutex::new(TunnelBudget::new());
        tunnel::authorize_forward(
            &Carry {
                route: OriginRoute::Peer(target),
                peer: requester,
                target: &asked,
                hosts: tcr::peer::egress::PEER_EGRESS_HOSTS,
                cap_bytes: 1024 * 1024,
                budget: &budget,
                now_ms: 1_767_225_600_000,
            },
            &Forward {
                store: &store,
                node: me,
                hops_remaining,
                via: &[],
            },
        )
    };

    let plan = decide(max_hops).expect("positive control: a frame inside the budget is forwarded");
    assert_eq!(
        plan.onward_hops, 0,
        "one hop is spent here, and this Mac's budget was one"
    );

    for over_ask in [max_hops + 1, 8, u8::MAX] {
        let refusal = decide(over_ask).expect_err("a frame over this Mac's budget is refused");
        let said = format!("{refusal:#}");
        assert!(
            said.contains("maxHops"),
            "the refusal must name the setting that refused it: {said}"
        );
    }
}

// ===========================================================================
// A carry that died inside the pump is not a carry that finished
// ===========================================================================

/// **A frame that does not decrypt ends the pump, and the splice cannot see
/// it.**
///
/// The pump copies PLAINTEXT, so a frame that failed to decrypt under the
/// session key reaches a reader as an ordinary end of stream:
/// `NoiseStream::finish` is the only thing that reports what really happened,
/// and until now nothing in `src/` called it, so a carry that died was logged
/// as "carried". Both splice callers in `tunnel` await it now.
///
/// Watch it fail by making `finish` return `Ok(())` without awaiting the pump.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_carried_stream_that_died_reports_it_through_finish() {
    use tokio::io::AsyncReadExt as _;

    let responder = NodeKey::load_or_mint(&scratch("pump-responder")).expect("a responder key");
    let dialler = NodeKey::load_or_mint(&scratch("pump-dialler")).expect("a dialler key");
    let responder_static = responder.id().0;
    let pinned = dialler.id();

    let (mut theirs, mut ours) = tokio::io::duplex(8192);
    let responder_secret = *responder.secret_bytes();
    let accepting = tokio::spawn(async move {
        let session = noise::accept_handshake(
            &mut ours,
            &responder_secret,
            noise::Handshake::Return,
            &[],
            move |_remote| Ok(pinned),
        )
        .await
        .expect("the responder's handshake");
        (ours, session)
    });
    let _dialled = noise::dial_handshake(
        &mut theirs,
        dialler.secret_bytes(),
        noise::Handshake::Return,
        Some(&responder_static),
        None,
    )
    .await
    .expect("the dialler's handshake");
    let (ours, session) = accepting.await.expect("the responder task");

    let mut carried = noise::NoiseStream::start(ours, session);
    // Not a frame this session can open: the pump breaks on it.
    noise::write_frame(&mut theirs, &[0_u8; 64])
        .await
        .expect("write a frame the session key cannot open");

    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), carried.read(&mut byte[..]))
        .await
        .expect("the carried stream ends rather than hanging");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "the reader sees an ordinary end of stream, which is exactly why finish exists: \
         {read:?}"
    );

    let ended = tokio::time::timeout(Duration::from_secs(5), carried.finish())
        .await
        .expect("the pump joins");
    let said = format!(
        "{:#}",
        ended.expect_err("the pump ended on a decrypt failure")
    );
    assert!(
        said.contains("did not decrypt"),
        "finish must say what ended the carry, so a gateway does not log a dead carry as \
         carried: {said}"
    );
}

/// **The local end hanging up does not throw away the reply already in
/// flight.**
///
/// `Ok(0)` on the local read ended the whole pump, so on a gateway carry the
/// origin's reply was dropped the moment the client closed its own half. A
/// half-close ends one direction; the other runs until the peer closes it or
/// the drain deadline passes.
///
/// Watch it fail by breaking the pump on `Ok(0)` again: the read below returns
/// zero bytes instead of the reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_end_that_closes_still_gets_what_the_peer_already_sent() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let responder = NodeKey::load_or_mint(&scratch("half-close-responder")).expect("a key");
    let dialler = NodeKey::load_or_mint(&scratch("half-close-dialler")).expect("a key");
    let responder_static = responder.id().0;
    let pinned = dialler.id();

    let (mut theirs, mut ours) = tokio::io::duplex(8192);
    let responder_secret = *responder.secret_bytes();
    let accepting = tokio::spawn(async move {
        let session = noise::accept_handshake(
            &mut ours,
            &responder_secret,
            noise::Handshake::Return,
            &[],
            move |_remote| Ok(pinned),
        )
        .await
        .expect("the responder's handshake");
        (ours, session)
    });
    let mut dialled = noise::dial_handshake(
        &mut theirs,
        dialler.secret_bytes(),
        noise::Handshake::Return,
        Some(&responder_static),
        None,
    )
    .await
    .expect("the dialler's handshake");
    let (ours, session) = accepting.await.expect("the responder task");

    let mut carried = noise::NoiseStream::start(ours, session);
    // The local end is done talking, and is still there to be answered.
    carried.shutdown().await.expect("half-close the local end");

    let reply = b"a reply the far end was already sending";
    noise::send_encrypted(&mut theirs, &mut dialled.transport, reply)
        .await
        .expect("the peer writes its reply after the local end closed");

    let mut back = vec![0_u8; reply.len()];
    tokio::time::timeout(Duration::from_secs(5), carried.read_exact(&mut back))
        .await
        .expect("the reply arrives rather than the pump having ended")
        .expect("every byte of the reply");
    assert_eq!(
        back, reply,
        "a half-closed local end must still be handed what the peer sent"
    );
}
