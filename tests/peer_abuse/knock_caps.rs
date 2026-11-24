//! The caps at the knock door: the knock bucket, the pending-knock rows, the
//! unauthenticated sockets, the knock frame bound, the accepted window and the
//! one-hour mute.

use super::*;

// ===========================================================================
// Cap: 1 knock / 10 s per address, burst 3 (`listener::KNOCK_INTERVAL_MS`,
// `listener::KNOCK_BURST`)
// ===========================================================================

/// **Thirty-two callers released together take three tokens, not thirty-two.**
///
/// The attack a sequential loop cannot run. `the_fourth_knock_in_ten_seconds_\
/// gets_nothing` (`tests/peer_pairing.rs`) spends its tokens one at a time, and
/// a token bucket refuses the fourth call whatever else is true: so it proves
/// the bucket is wired and proves nothing about a race. This releases
/// thirty-two threads from a barrier onto the SAME `Admission` at the SAME
/// `now_ms`, which is the shape that wins against a read-then-decrement that
/// is not under one lock: every caller reads three tokens left and every caller
/// spends one.
///
/// One `now_ms` for all thirty-two on purpose: with a shared clock the bucket
/// cannot refill mid-race, so the number of successes is a property of the
/// mutual exclusion alone and not of how long the threads took.
#[test]
fn a_parallel_burst_from_one_address_gets_three_tokens_not_thirty_two() {
    let admission = Arc::new(std::sync::Mutex::new(Admission::new()));
    let attackers = 32_usize;
    let barrier = Arc::new(std::sync::Barrier::new(attackers));
    let now = 1_700_000_000_000_i64;

    let taken = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..attackers {
            let admission = Arc::clone(&admission);
            let barrier = Arc::clone(&barrier);
            let taken = &taken;
            scope.spawn(move || {
                // The barrier is what makes this parallel rather than a loop
                // with extra steps: no thread proceeds until all thirty-two
                // are here.
                barrier.wait();
                let mut guard = match admission.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if guard.take_knock_token(LOOPBACK, now) {
                    taken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
    });

    assert_eq!(
        taken.load(std::sync::atomic::Ordering::SeqCst),
        usize::try_from(plan::KNOCK_BURST).expect("the burst fits a usize"),
        "{attackers} callers released together took {} tokens; the burst is {} and a bucket \
         that hands out more than its burst under a race is not a rate limit",
        taken.load(std::sync::atomic::Ordering::SeqCst),
        plan::KNOCK_BURST
    );

    let left = with_admission_arc(&admission, |guard| guard.knock_tokens(LOOPBACK, now));
    assert_eq!(
        left, 0,
        "the bucket must be empty after the burst, and it reports {left} tokens"
    );
}

/// **A burst timed to land exactly on the refill instant earns one token, not a
/// fresh bucket**: and re-asking inside the interval earns nothing, however
/// many times it asks.
///
/// Two ways a refill computed from elapsed time goes wrong, both attacked here.
/// It can credit a whole bucket at the boundary instead of one token (then a
/// flooder gets `KNOCK_BURST` every interval instead of one). Or it can credit
/// the same interval twice by moving `last_ms` to `now` rather than forward by
/// the intervals it actually spent: which would let a caller who asks a
/// thousand times inside one interval collect a token on some of them.
#[test]
fn a_burst_on_the_exact_refill_boundary_earns_one_token_not_a_full_bucket() {
    let mut admission = Admission::new();
    let start = 1_700_000_000_000_i64;
    for spend in 0..plan::KNOCK_BURST {
        assert!(
            admission.take_knock_token(LOOPBACK, start),
            "token {spend} of the burst must be available at the same instant as the rest"
        );
    }
    assert!(
        !admission.take_knock_token(LOOPBACK, start),
        "the burst is {} and the next one at the same instant must be refused",
        plan::KNOCK_BURST
    );

    // One millisecond before the refill: still nothing.
    let just_before = start + plan::KNOCK_INTERVAL_MS - 1;
    assert!(
        !admission.take_knock_token(LOOPBACK, just_before),
        "a knock one millisecond before the refill instant must still be refused"
    );

    // Exactly on it: one token, and only one.
    let boundary = start + plan::KNOCK_INTERVAL_MS;
    assert!(
        admission.take_knock_token(LOOPBACK, boundary),
        "the refill instant must credit a token"
    );
    assert!(
        !admission.take_knock_token(LOOPBACK, boundary),
        "the refill instant credits ONE token; a full bucket every interval is the cap \
         multiplied by {}",
        plan::KNOCK_BURST
    );

    // And asking a thousand times inside the next interval collects nothing:
    // the refill is credited from `last_ms` moved FORWARD by whole intervals,
    // so repeated asks cannot each earn the same elapsed time.
    for step in 1..1_000 {
        assert!(
            !admission.take_knock_token(LOOPBACK, boundary + step % plan::KNOCK_INTERVAL_MS),
            "asking repeatedly inside one interval must not re-earn the same elapsed time"
        );
    }

    // Three whole intervals later the bucket is full and NOT overfull: a
    // saturating refill that forgot to clamp would hand out the backlog.
    let long_quiet = boundary + plan::KNOCK_INTERVAL_MS * 10;
    assert_eq!(
        admission.knock_tokens(LOOPBACK, long_quiet),
        plan::KNOCK_BURST,
        "ten quiet intervals must refill to the burst and no further"
    );
}

/// **Thirty knocks over real sockets earn at most three acks**, and leave
/// exactly one row for the operator.
///
/// The end-to-end half of the bucket attack: the pure-counter test above proves
/// the mutual exclusion, and this proves the accept loop actually consults the
/// bucket before it writes a byte.
///
/// # Why this fires in waves of two and not thirty at once, which is a defect
/// # the positive control caught
///
/// Thirty simultaneous `connect`s from one address do not measure the knock
/// bucket at all: with nothing granted, `unauthenticated_allowance` is
/// `(16, 2)`, so the twenty-eighth of them is refused by
/// `MAX_UNAUTHENTICATED_PER_ADDRESS`: a different cap, checked earlier, whose
/// refusal is deliberately indistinguishable on the wire. Measured: with
/// `take_knock_token` replaced by a function that always admits, the
/// all-at-once version of this test still saw at most three acks and stayed
/// GREEN. It was reporting on the socket cap and calling it the bucket.
///
/// So the knocks go out in waves of [`plan::MAX_UNAUTHENTICATED_PER_ADDRESS`],
/// which is the most that can be in flight from one address without the earlier
/// cap having an opinion: each wave still released together from its own
/// barrier, so the concurrency is real, and the whole run stays inside one
/// refill interval, which is asserted, so no ack can be explained by the bucket
/// refilling mid-run. With the bucket removed this now reds with thirty acks.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn thirty_knocks_in_waves_of_two_earn_at_most_three_acks() {
    let node = Node::new("bucket-wire");
    let addr = serve(node.context()).await;

    let wave = plan::MAX_UNAUTHENTICATED_PER_ADDRESS;
    let waves = 15_usize;
    let attackers = wave * waves;
    let started = std::time::Instant::now();
    let mut acked = 0_usize;
    for _ in 0..waves {
        let barrier = Arc::new(tokio::sync::Barrier::new(wave));
        let mut tasks = Vec::with_capacity(wave);
        for _ in 0..wave {
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                knock_at(addr, churned_instance(), None).await.is_ok()
            }));
        }
        for task in tasks {
            if task.await.expect("the knocking task does not panic") {
                acked += 1;
            }
        }
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed
            < Duration::from_millis(
                u64::try_from(plan::KNOCK_INTERVAL_MS).expect("the interval fits a u64")
            ),
        "the whole burst has to finish inside one refill interval or an ack could be a \
         refill rather than a cap failure; it took {elapsed:?}"
    );
    assert!(
        acked <= usize::try_from(plan::KNOCK_BURST).expect("the burst fits a usize"),
        "{attackers} knocks in waves of {wave} earned {acked} acks and the burst is {}; \
         none of them could have been refused by the per-address socket cap, so every \
         refusal above three was the bucket's",
        plan::KNOCK_BURST
    );
    assert!(
        acked >= 1,
        "positive control: at least one knock must get through, or this test would pass \
         against a listener that refuses everything"
    );

    // And what the operator sees is one row, not thirty.
    let visible = node.state().visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "thirty knocks from one address must coalesce to one row; the operator sees {visible:?}"
    );
}

// ===========================================================================
// Cap: 8 pending knocks, coalesced by address (`state::MAX_PENDING_KNOCKS`)
// ===========================================================================

/// **Twenty knocks with a fresh random instance id each leave ONE row.**
///
/// The id changer, run in parallel and with real churn.
/// `a_rotating_instance_id_from_one_address_is_one_row`
/// (`tests/peer_pairing.rs`) rotates three FIXED ids one at a time; this fires
/// twenty at once with an unpredictable id each, which is the shape that wins
/// against coalescing keyed on the id (twenty rows), against a queue cap
/// checked outside the write lock (nine rows), and against a reservation that
/// is never released (eight placeholder rows and a queue that is full forever).
///
/// Both numbers are asserted, because they are two different failures: the
/// queue must never exceed [`state::MAX_PENDING_KNOCKS`] rows at all, and what
/// the operator is SHOWN must be one row.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn twenty_parallel_knocks_with_a_churned_instance_id_leave_one_row() {
    let node = Node::new("id-churn");
    let addr = serve(node.context()).await;

    let attackers = 20_usize;
    let barrier = Arc::new(tokio::sync::Barrier::new(attackers));
    let mut tasks = Vec::with_capacity(attackers);
    for _ in 0..attackers {
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            knock_at(addr, churned_instance(), None).await.is_ok()
        }));
    }
    let mut acked = 0_usize;
    for task in tasks {
        if task.await.expect("the knocking task does not panic") {
            acked += 1;
        }
    }
    assert!(
        acked >= 1,
        "positive control: the flood must not be refused in its entirety, or the row count \
         below proves nothing"
    );

    let after = node.state();
    assert!(
        after.knocks_queued() <= plan::MAX_PENDING_KNOCKS,
        "the queue holds {} rows and the cap is {}; a rotating instance id must not buy a \
         row each",
        after.knocks_queued(),
        plan::MAX_PENDING_KNOCKS
    );
    let visible = after.visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "one address is one row however many ids it churns through; the operator sees \
         {visible:?}"
    );
    assert_eq!(
        visible[0].addr, LOOPBACK,
        "the row must be keyed on the address, which is the field a completed TCP handshake \
         makes real"
    );
}

/// **A knock that renames itself every time takes no second row, and a knock
/// that borrows a pinned Mac's name changes nothing about the pin.**
///
/// Two attacks on the same surface. Name churn tries for a row per name, which
/// would be a queue cap of "eight names" rather than eight machines. Name
/// theft claims a label an operator already trusts: `abuse-resistance.md`'s
/// impersonation row: "a name is a label, never identity". So the assertion is
/// not that the name is refused (it is shown, beside the address, which is what
/// makes the row honest) but that the PEERS FILE is byte-for-byte what it was:
/// no row added, no key moved, no address learned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_that_renames_itself_never_takes_a_second_row_or_a_pinned_macs_pin() {
    let node = Node::new("name-churn");

    // One already-pinned Mac, with a label the attacker will try to wear.
    let pinned = PeerId([7_u8; 32]);
    let mut file = node.file();
    file.peers.push(PeerRow {
        node: pinned,
        label: "studio-mac".to_string(),
        endpoints: vec![Endpoint::direct(
            "192.0.2.10:9600"
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
    let before = std::fs::read(&node.peers).expect("read the peers file before the attack");

    let addr = serve(node.context()).await;

    // Three knocks, which is the whole burst: a different name each time, and
    // the last one is the pinned Mac's own label.
    let names = ["laptop-2", "attic-nuc", "studio-mac"];
    let mut acked = 0_usize;
    for name in names {
        if knock_at(addr, churned_instance(), Some(name)).await.is_ok() {
            acked += 1;
        }
    }
    assert!(
        acked >= 1,
        "positive control: at least one of the three knocks must be taken, or the row \
         assertions below are vacuous"
    );

    let visible = node.state().visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "a new name per knock must not buy a new row: {visible:?}"
    );

    // The row is a claim, shown beside the address. The pin is untouched.
    let after = std::fs::read(&node.peers).expect("read the peers file after the attack");
    assert_eq!(
        before, after,
        "a knock proposing a pinned Mac's name must not touch the peers file at all"
    );
    let file = node.file();
    assert_eq!(file.peers.len(), 1, "no row was added");
    assert_eq!(
        file.peers[0].node, pinned,
        "the pinned key is still the one the operator trusted; a name is a label, never \
         identity"
    );
}

// ===========================================================================
// Cap: 2 unauthenticated sockets per address, 5 s to deliver message 1,
// 10 s handshake (`listener::MAX_UNAUTHENTICATED_PER_ADDRESS`,
// `listener::MESSAGE_1_TIMEOUT`, `listener::HANDSHAKE_TIMEOUT`)
// ===========================================================================

/// **Sockets opened and held silent take two slots and no more, and every slot
/// comes back after the message-1 deadline.**
///
/// `the_third_unauthenticated_socket_from_one_address_is_refused`
/// (`tests/peer_pairing.rs`) proves the third is refused. The attack this adds
/// is the one that makes a cap into a lockout: hold the sockets past the
/// deadline and see whether the counter is ever given back. A slot leaked on
/// the timeout path is a permanent refusal of every future connection from
/// that address, which is a denial of service built out of the defence against
/// one: and it is invisible to any test that closes its sockets politely.
///
/// Ten attackers against a cap of two, all connected before anything is read,
/// so the assertion is about the counter and not about scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ten_silent_sockets_hold_two_slots_and_give_them_all_back() {
    let node = Node::new("silent-sockets");
    let context = node.context();
    let addr = serve(context.clone()).await;

    let mut held = Vec::new();
    for _ in 0..10 {
        // Connect and say NOTHING: the cheapest thing a stranger can do.
        if let Ok(stream) = TcpStream::connect(addr).await {
            held.push(stream);
        }
    }
    // The accept loop needs a moment to pick every connection up; the slot is
    // taken inside `serve_connection`, not by `accept`.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (live, total) = with_admission(&context, |guard| {
        (
            guard.live_unauthenticated(LOOPBACK),
            guard.live_unauthenticated_total(),
        )
    });
    assert!(
        live <= plan::MAX_UNAUTHENTICATED_PER_ADDRESS,
        "ten silent sockets from one address hold {live} slots and the per-address allowance \
         is {}",
        plan::MAX_UNAUTHENTICATED_PER_ADDRESS
    );
    assert!(
        live >= 1,
        "positive control: a silent socket must hold a slot, or the cap above is satisfied \
         by a counter that never counts"
    );
    assert!(
        total <= plan::MAX_UNAUTHENTICATED_SOCKETS,
        "the node-wide allowance is {} and {total} sockets are counted",
        plan::MAX_UNAUTHENTICATED_SOCKETS
    );

    // Past the message-1 deadline: every slot must come back, even though the
    // attacker never closed a socket and never said a word.
    tokio::time::sleep(plan::MESSAGE_1_TIMEOUT + Duration::from_secs(2)).await;
    let after = with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK));
    assert_eq!(
        after, 0,
        "every slot must be released when the message-1 deadline closes the socket; {after} \
         are still held, which is a lockout an attacker gets for free"
    );
    // The sockets are dropped only here, so nothing above was measured against
    // a connection the kernel had already torn down.
    drop(held);

    // And the listener still works for a real knock afterwards.
    knock_at(addr, instance(9), None)
        .await
        .expect("a knock after the flood must still be taken");
    assert_eq!(
        node.state().visible_pending().len(),
        1,
        "the queue must still accept a real knock after ten stalled sockets"
    );
}

/// **A socket that delivers a VALID message 1 and then goes silent holds
/// neither a slot nor a queue row.**
///
/// The subtler version of the attack above, and the one the caps are shaped
/// around: a stalled knock holds its socket slot AND a RESERVED PENDING ROW,
/// one eighth of the operator's pairing queue, and this test is about the
/// second one (the slot is
/// `three_knocks_in_flight_from_one_address_hold_two_slots_and_the_third_gets_\
/// nothing`, below). Three stalls is the whole knock
/// burst, so if a reservation outlived its connection this node would be
/// refusing real knocks with nothing an operator could see, and
/// `state::visible_pending` hides the placeholder from them.
///
/// `a_knock_whose_message_1_does_not_validate_reserves_nothing`
/// (`tests/peer_pairing.rs`) covers the INVALID message 1. This is the valid
/// one, which is the case that gets a reservation in the first place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_valid_message_one_then_silence_holds_no_slot_and_no_queue_row() {
    let node = Node::new("stalled-reservation");
    let context = node.context();
    let addr = serve(context.clone()).await;

    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(stall_after(addr, &knock_message_1()).await);
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    // What the stall holds: the third connection is refused at the socket cap
    // (the per-address allowance is two), so this reads the reservations the
    // two admitted ones took.
    let during = node.state();
    assert!(
        during.knocks_queued() <= plan::MAX_PENDING_KNOCKS,
        "three stalled knocks reserved {} rows, and the cap is {}",
        during.knocks_queued(),
        plan::MAX_PENDING_KNOCKS
    );
    assert!(
        during.visible_pending().is_empty(),
        "a reservation is a placeholder with no instance id and no name; showing the \
         operator a pairing request from nobody is the defect `visible_pending` exists for, \
         and they are shown {:?}",
        during.visible_pending()
    );

    // Past the deadline the reservation is released, so a stalled connection
    // cannot hold a slice of the queue for as long as it keeps a TCP open.
    tokio::time::sleep(plan::MESSAGE_1_TIMEOUT + Duration::from_secs(2)).await;
    let after = node.state();
    assert!(
        after.knocks_queued() == 0,
        "every reservation must be released once the handshake misses its deadline; {after:?} \
         remain, and each one is an eighth of the pairing queue held by a socket that never \
         said who it was"
    );
    assert_eq!(
        with_admission(&context, |guard| guard.live_unauthenticated(LOOPBACK)),
        0,
        "and no socket slot is still held either"
    );
    drop(held);
}

/// **An `XX` message 1 inside an open window, then silence, is closed by the
/// handshake deadline** and leaves the window usable.
///
/// The one path with the longer deadline: `HANDSHAKE_TIMEOUT` rather than
/// `MESSAGE_1_TIMEOUT`, because this connection HAS delivered message 1 and is
/// mid-handshake. An attacker who could hold that state open would hold the
/// operator's 120-second window hostage. Ten seconds is asserted as an upper
/// bound on the wire: the socket has to be closed, not merely refused
/// eventually.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_handshake_that_stalls_is_closed_by_the_handshake_deadline() {
    let node = Node::new("stalled-xx");
    let addr = serve(node.context()).await;

    let id = instance(4);
    knock_at(addr, id, Some("laptop-2"))
        .await
        .expect("the knock is taken");
    let window = accept_pending(&node, LOOPBACK);
    assert_eq!(window.instance_id, id);

    // Message 1 goes in, and then nothing. The far side answers message 2 :
    // this id IS accepted: and then waits for message 3 that never comes.
    let started = std::time::Instant::now();
    let mut stalled = stall_after(addr, &xx_message_1(id)).await;
    let mut back = Vec::new();
    let read = tokio::time::timeout(
        plan::HANDSHAKE_TIMEOUT + Duration::from_secs(5),
        stalled.read_to_end(&mut back),
    )
    .await;
    assert!(
        read.is_ok(),
        "the listener must close a stalled handshake on its own; after {:?} the socket was \
         still open, which is a slot held by saying nothing",
        started.elapsed()
    );
    assert!(
        started.elapsed() >= plan::MESSAGE_1_TIMEOUT,
        "positive control on the clock: an accepted XX must not be closed before the \
         handshake has had its time, and this one closed after {:?}",
        started.elapsed()
    );
    assert!(
        !back.is_empty(),
        "positive control: an ACCEPTED instance really does get message 2, so the closure \
         above is the deadline and not a refusal"
    );

    // The window survives its abandoned handshake: the operator's Accept is not
    // spent by a stranger stalling.
    let state = node.state();
    assert!(
        state.accepted_window(LOOPBACK, &id, state::now_ms()),
        "a stalled handshake must not consume the operator's window"
    );
}

// ===========================================================================
// Cap: the knock frame bound (`listener::MAX_KNOCK_FRAME_BYTES`)
// ===========================================================================

/// A knock whose serialized plaintext is exactly `target` bytes, so a test can
/// size the encrypted FRAME to the byte: the frame carries the ciphertext,
/// which is the plaintext plus the 16-byte Poly1305 tag.
fn knock_of_serialized_len(id: InstanceId, target: usize) -> Knock {
    let probe = Knock {
        instance_id: id,
        proposed_name: Some(String::new()),
        wire_version: PROTO_VERSION,
        listen_port: None,
    };
    let base = serde_json::to_vec(&probe)
        .expect("a knock serializes")
        .len();
    let name_len = target
        .checked_sub(base)
        .expect("the target must be above the knock's own fixed cost");
    let knock = Knock {
        instance_id: id,
        proposed_name: Some("a".repeat(name_len)),
        wire_version: PROTO_VERSION,
        listen_port: None,
    };
    assert_eq!(
        serde_json::to_vec(&knock)
            .expect("a knock serializes")
            .len(),
        target,
        "the name has to absorb exactly the difference, or this test is not at the bound"
    );
    knock
}

/// The ciphertext overhead one Noise transport message adds: the Poly1305 tag.
const NOISE_TAG_BYTES: usize = 16;

/// **A knock frame at the bound is taken; one byte over gets nothing and
/// leaves no row.**
///
/// `a_first_frame_above_the_bound_is_refused_before_allocation`
/// (`tests/peer_pairing.rs`) bounds MESSAGE 1. This is the second bound, on the
/// knock frame that rides the session message 1 establishes: the frame whose
/// length prefix a stranger who has completed an `NN` handshake gets to choose.
/// Both sides of the boundary, because a bound tested only from above can be
/// satisfied by a listener that refuses everything.
///
/// The over-bound half runs on its own node: an over-bound knock is refused
/// inside `read_knock`, which is AFTER the reservation is taken, so the row it
/// must not leave behind is a row its own failure has to release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_frame_at_the_bound_is_taken_and_one_byte_over_gets_nothing() {
    // One byte over the bound, alone, so nothing else could have created the
    // row this asserts is absent.
    let over_node = Node::new("frame-over");
    let over_addr = serve(over_node.context()).await;
    let over = knock_of_serialized_len(
        instance(2),
        plan::MAX_KNOCK_FRAME_BYTES - NOISE_TAG_BYTES + 1,
    );
    let refused = knock_frame_at(over_addr, &over).await;
    assert!(
        refused.is_err(),
        "a knock frame of {} bytes is one over MAX_KNOCK_FRAME_BYTES ({}) and must earn \
         nothing",
        plan::MAX_KNOCK_FRAME_BYTES + 1,
        plan::MAX_KNOCK_FRAME_BYTES
    );
    let over_state = over_node.state();
    assert!(
        over_state.knocks_queued() == 0,
        "an over-bound knock must leave no row at all, not even the reservation its own \
         handshake took: {over_state:?}"
    );

    // Exactly at the bound, on a fresh node with a full bucket.
    let at_node = Node::new("frame-at");
    let at_addr = serve(at_node.context()).await;
    let at = knock_of_serialized_len(instance(3), plan::MAX_KNOCK_FRAME_BYTES - NOISE_TAG_BYTES);
    knock_frame_at(at_addr, &at)
        .await
        .expect("a knock frame exactly at the bound must be read, not refused");
    let visible = at_node.state().visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "the at-bound knock must leave exactly one row: {visible:?}"
    );
    assert!(
        visible[0].proposed_name.is_none(),
        "a 400-character name is inside the FRAME bound and outside the label whitelist, so \
         the row keeps the address and drops the name; it kept {:?}",
        visible[0].proposed_name
    );
}

// ===========================================================================
// Cap: the 120-second accepted window, one instance id
// (`listener::ACCEPTED_WINDOW_SECS`, `pair::PAIRING_WINDOW_SECS`)
// ===========================================================================

/// **An Accept admits one instance id even when ten others race it inside the
/// window.**
///
/// `an_accepted_window_admits_one_instance_id_only`
/// (`tests/peer_pairing.rs`) sends one impostor. This sends ten at once, with a
/// churned id each, and brackets them with the accepted id so the zeros cannot
/// be explained by the socket cap having refused everything: the accepted id
/// gets bytes before the flood AND after it.
///
/// Answering an `XX` message 1 hands over this node's static key, so a window
/// that widened under concurrency would be a key harvest with a 120-second
/// window and no operator involvement at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_accept_admits_one_instance_id_even_when_ten_others_race_it() {
    let node = Node::new("window-race");
    let addr = serve(node.context()).await;

    let accepted = instance(5);
    knock_at(addr, accepted, Some("laptop-2"))
        .await
        .expect("the knock is taken");
    accept_pending(&node, LOOPBACK);

    // Before: the accepted id gets an answer, which is what makes the zeros
    // below mean something.
    let before = count_written(addr, &xx_message_1(accepted)).await;
    assert!(
        before > 0,
        "positive control: the accepted instance must be answered, and it got {before} bytes"
    );

    let attackers = 10_usize;
    let barrier = Arc::new(tokio::sync::Barrier::new(attackers));
    let mut tasks = Vec::with_capacity(attackers);
    for _ in 0..attackers {
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            count_written(addr, &xx_message_1(churned_instance())).await
        }));
    }
    for task in tasks {
        let written = task.await.expect("the impostor task does not panic");
        assert_eq!(
            written, 0,
            "an XX message 1 from an id the operator never accepted must get zero bytes \
             inside the window, and this one got {written}"
        );
    }

    // After: still answered, so the ten zeros above were the window's decision
    // and not the socket cap refusing the whole batch.
    let after = count_written(addr, &xx_message_1(accepted)).await;
    assert!(
        after > 0,
        "positive control: the accepted instance must still be answered after the flood, \
         and it got {after} bytes"
    );
}

/// **A new instance id knocking after an Accept never inherits the window.**
///
/// The id changer's best move on this surface: knock, get accepted, then come
/// back under a NEW id from the same address. A knock from an address with an
/// open window updates the PENDING queue, and if the window were keyed on the
/// address alone (or re-keyed by the later knock), that new id would be
/// answered. It is not: the window belongs to the id the operator saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_instance_id_knocking_after_an_accept_never_inherits_the_window() {
    let node = Node::new("window-inherit");
    let addr = serve(node.context()).await;

    let accepted = instance(6);
    knock_at(addr, accepted, Some("laptop-2"))
        .await
        .expect("the first knock is taken");
    accept_pending(&node, LOOPBACK);

    // A second knock from the same address under a different id.
    let usurper = instance(7);
    knock_at(addr, usurper, Some("laptop-2"))
        .await
        .expect("the second knock is taken too; it is a new pending row, nothing more");

    let written = count_written(addr, &xx_message_1(usurper)).await;
    assert_eq!(
        written, 0,
        "the id the operator accepted is the only one the window admits; the usurper got \
         {written} bytes"
    );
    let still = count_written(addr, &xx_message_1(accepted)).await;
    assert!(
        still > 0,
        "positive control: the accepted id must still be answered, and it got {still} bytes"
    );
    assert!(
        node.state()
            .accepted_window(LOOPBACK, &accepted, state::now_ms()),
        "and the window is still keyed to the id the operator saw"
    );
}

// ===========================================================================
// Cap: one-hour mute on Ignore (`state::MUTE_MS`)
// ===========================================================================

/// **Twenty knocks from a muted address get nothing, and the mute is not one
/// millisecond shorter for it.**
///
/// `a_knock_from_a_muted_address_gets_zero_bytes` (`tests/peer_pairing.rs`)
/// sends one knock. The attacks this adds are the two a flooder would actually
/// try: knock enough times in parallel that one of them slips between another's
/// read and write, and knock in the hope that the re-knock itself refreshes,
/// resets or consumes the mute: a mute an attacker can clear by attacking is
/// not a mute.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn twenty_parallel_knocks_from_a_muted_address_get_nothing_and_shorten_nothing() {
    let node = Node::new("muted-flood");
    let addr = serve(node.context()).await;

    // The operator pressed Ignore.
    let muted_at = state::now_ms();
    let mut value = node.state();
    value.mute(LOOPBACK, muted_at);
    node.write_state(&value);
    let before = node.state().muted;
    assert_eq!(before.len(), 1, "the address is muted to begin with");

    let attackers = 20_usize;
    let barrier = Arc::new(tokio::sync::Barrier::new(attackers));
    let mut tasks = Vec::with_capacity(attackers);
    for _ in 0..attackers {
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            knock_at(addr, churned_instance(), Some("laptop-2"))
                .await
                .is_ok()
        }));
    }
    for task in tasks {
        let acked = task.await.expect("the knocking task does not panic");
        assert!(
            !acked,
            "a knock from a muted address must earn no ack; one of twenty was acknowledged"
        );
    }

    let after = node.state();
    assert!(
        after.knocks_queued() == 0,
        "a muted address must reach no row at all, reservation included: {after:?}"
    );
    assert_eq!(
        after.muted, before,
        "the mute must be exactly what the operator set: not extended, not shortened, not \
         replaced by the flood: {:?} became {:?}",
        before, after.muted
    );
    assert!(
        after.is_muted(LOOPBACK, muted_at + plan::MUTE_MS - 1),
        "and it still runs to the end of the hour"
    );
}
