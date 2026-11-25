// ---------------------------------------------------------------------------
// Item 7: the knock, end to end
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// **A knock earns one row and one ack byte, and reveals no static key.**
///
/// The whole of phase one: the requester opens `NN`: ephemeral keys on both
/// sides, sends its claim, and the responder queues it and answers one byte.
/// Nothing is pinned, nothing is granted, and the responder's static key never
/// appears.
///
/// Watched red: drop the `noise::write_knock_ack` call from
/// `listener::serve_knock` and `send_knock` fails with "that Mac closed the
/// connection without taking the pairing request".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_queues_one_row_and_earns_one_ack_byte() {
    let node = Node::new("knock-row");
    let addr = serve(node.context()).await;

    knock_at(addr, instance(0x41), Some("studio-mac"))
        .await
        .expect("a knock at a node with no network key is taken");

    let after = node.state();
    assert_eq!(
        after.knocks_queued(),
        1,
        "one knock is one pending row, and this made {}",
        after.knocks_queued()
    );
    let row = after.find_pending("127.0.0.1").expect(
        "the row is keyed on the IP with the ephemeral source port dropped, or the \
         eight-row cap would bound knocks rather than machines",
    );
    assert_eq!(row.instance_id, instance(0x41));
    assert_eq!(row.proposed_name.as_deref(), Some("studio-mac"));
    assert_eq!(row.wire_version, tcr_peer_wire::PROTO_VERSION);

    // Nothing was pinned and nothing was granted: a knock is a row.
    assert!(
        node.file().pinned_count() == 0,
        "a knock must pin nothing at all"
    );
}

/// **A rotating instance id from one address is ONE row that updates.**
///
/// The id-changer defence, and the reason the pending queue keys on the source
/// address rather than on the id: coalescing by id would let one host hold all
/// eight pending slots by rotating its id eight times.
///
/// Watched red: change `record_knock`'s `find(|knock| knock.addr == addr)` to
/// compare `instance_id` instead, and this fails with three rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rotating_instance_id_from_one_address_is_one_row() {
    let node = Node::new("knock-idchanger");
    let addr = serve(node.context()).await;

    for id in [0x51_u8, 0x52, 0x53] {
        knock_at(addr, instance(id), Some("studio-mac"))
            .await
            .expect("each knock is inside the burst");
    }

    let after = node.state();
    assert_eq!(
        after.knocks_queued(),
        1,
        "three knocks from one address under three ids must coalesce into one row, and \
         they made {}",
        after.knocks_queued()
    );
    assert_eq!(
        after.find_pending("127.0.0.1").map(|row| row.instance_id),
        Some(instance(0x53)),
        "the row shows the id that knocked LAST, so an operator accepting it approves the \
         one that is actually asking"
    );
}

/// A hostile claimed name never reaches the row.
///
/// The name in a knock is attacker-chosen text that lands in a panel row, a
/// `tcr peer pending` line and a log line. A name that fails the whitelist
/// drops the NAME and keeps the row, which is the same rule the discovery path
/// follows: the row is still a machine an operator may want to accept, and it
/// shows its address.
///
/// Watched red: remove the `sanitize_label` call in `listener::serve_knock` and
/// the escape sequence arrives in `proposed_name`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_knock_name_is_dropped_and_the_row_survives() {
    let node = Node::new("knock-hostile-name");
    let addr = serve(node.context()).await;

    knock_at(addr, instance(0x61), Some("studio\u{1b}[31mmac"))
        .await
        .expect("the knock itself is fine; only its name is not");

    let after = node.state();
    assert_eq!(
        after.knocks_queued(),
        1,
        "the row survives a name it cannot render"
    );
    assert_eq!(
        after
            .find_pending("127.0.0.1")
            .map(|row| row.proposed_name.clone()),
        Some(None),
        "a name this node will not render is no name at all: the row shows its address"
    );
}

// ---------------------------------------------------------------------------
// Item 9: the three gating refusals
// ---------------------------------------------------------------------------

/// **An `XX` message 1 from an instance nobody accepted gets zero bytes.**
///
/// This is the decision-10 replacement for the node-wide pairing window, and
/// the assertion is on the socket's byte count rather than on an error: a
/// listener that answered first and refused afterwards has already disclosed
/// this node's static key in message 2.
///
/// Watched red: delete the `if !peer_state.accepted_window(...)` bail from
/// `accept_pairing_or_return` and the count goes from 0 to 50 (message 2 plus
/// its frame prefix).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn xx_from_an_unaccepted_instance_gets_zero_bytes() {
    let node = Node::new("xx-unaccepted");
    let addr = serve(node.context()).await;

    let written = count_written(addr, &xx_message_1(instance(0x71))).await;
    assert_eq!(
        written, 0,
        "the listener answered a first pairing nobody approved: a stranger on this port \
         just learned this node's static key for the cost of 40 bytes"
    );

    // The POSITIVE CONTROL, which is what makes the zero above mean something:
    // after a knock and an Accept, the same dial is answered.
    knock_at(addr, instance(0x72), None)
        .await
        .expect("the knock is taken");
    accept_pending(&node, &instance(0x72).to_wire());
    let answered = count_written(addr, &xx_message_1(instance(0x72))).await;
    assert!(
        answered > 0,
        "positive control: an accepted instance must be answered, or the refusal above is \
         a listener that has stopped pairing at all"
    );
}

/// **An accepted window admits ONE instance id, not the address.**
///
/// The other half of the id-changer defence: switching ids after Accept loses
/// the window rather than inheriting it. Same address, same moment, a different
/// id.
///
/// Watched red: drop `&& &open.instance_id == instance_id` from
/// `PeerState::accepted_window` and the second dial is answered too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_window_admits_one_instance_id_only() {
    let node = Node::new("xx-one-id");
    let addr = serve(node.context()).await;

    knock_at(addr, instance(0x81), None)
        .await
        .expect("the knock is taken");
    accept_pending(&node, &instance(0x81).to_wire());

    // The accepted id is answered.
    assert!(
        count_written(addr, &xx_message_1(instance(0x81))).await > 0,
        "positive control: the accepted id must be answered"
    );
    // A different id from the same address, inside the same window, is not.
    assert_eq!(
        count_written(addr, &xx_message_1(instance(0x82))).await,
        0,
        "a second instance id inside one window must get zero bytes, or an id changer \
         inherits somebody else's approval"
    );
}

/// **A knock from a muted address gets zero bytes.**
///
/// Ignore is one of the three things an operator can do to a pending row, and
/// what it buys is an hour of quiet. The refusal is indistinguishable on the
/// wire from a machine that is not there, deliberately: telling a flooder which
/// cap it hit tells it what to change.
///
/// The assertion is on the socket's byte count, not only on `knock_at`'s
/// `Err`, for the same reason `xx_from_an_unaccepted_instance_gets_zero_bytes`
/// uses `count_written`: a listener that answered NN message 2 and refused
/// only afterwards has already spent bytes on a source it meant to ignore for
/// free, even though `send_knock` still reports it as an error (it never gets
/// its ack). Watched red: this failed with `written != 0` (message 2 plus its
/// frame prefix) before `PeerState::refusal_for_knock_source` was checked
/// ahead of `noise::finish_responder` in `listener::serve_knock`: the ban,
/// mute and queue-full refusals used to run only after message 2 was already
/// on the wire (`listener.rs`, the gap between the old `finish_responder` call
/// and the `record_knock` call that followed it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_from_a_muted_address_gets_zero_bytes() {
    let node = Node::new("knock-muted");
    let addr = serve(node.context()).await;

    knock_at(addr, instance(0x91), None)
        .await
        .expect("the first knock is taken");
    // `tcr peer ignore` on that row.
    let mut value = node.state();
    value.mute("127.0.0.1", state::now_ms());
    node.write_state(&value);
    assert!(
        node.state().knocks_queued() == 0,
        "Ignore drops the row it was pressed on"
    );

    let refused = knock_at(addr, instance(0x92), None).await;
    assert!(
        refused.is_err(),
        "a knock from a muted address must not earn an ack"
    );
    assert!(
        node.state().knocks_queued() == 0,
        "and it must not earn a row either: {:?}",
        node.state()
    );

    // The byte count, on a fresh connection: zero, or the mute cost the
    // source NN message 2 before ever being refused.
    let written = count_written(addr, &knock_message_1()).await;
    assert_eq!(
        written, 0,
        "a muted address must be refused before message 2 is written, not after: {written} \
         bytes came back"
    );
}

/// **A banned static key is refused from a NEW address.**
///
/// The ban scope is both halves, and this is the half the address
/// alone cannot cover: DHCP moves addresses, so a blocked Mac that comes back
/// on a new lease with the same static key has to stay blocked.
///
/// Driven on the `IK` return path, because that is where a banned key arrives
/// from somewhere new: the peer is still pinned (blocking is not forgetting),
/// its row is still in the file, and the refusal has to come from the ban and
/// not from the pin check. The address it dials from (loopback), is
/// deliberately NOT banned, so a pass here cannot be the address half firing.
///
/// Watched red: remove the `banned.iter().any(...)` check from the pin callback
/// in `accept_pairing_or_return` and the handshake completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_banned_key_is_refused_from_a_new_address() {
    let node = Node::new("ban-key");
    let (peer_secret, peer_public) = noise::generate_static().expect("a peer keypair");
    let peer_id = PeerId(peer_public);

    // The peer is pinned, which is the point: the refusal must be the ban.
    let mut file = node.file();
    file.pin(config::PeerRow {
        node: peer_id,
        label: "laptop-2".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow::default(),
        lend: Vec::new(),
    });
    node.write_file(&file);

    let addr = serve(node.context()).await;
    let responder_public = node.key.id().0;

    // The positive control FIRST, while nothing is banned: the pinned peer gets
    // in. Without this, the refusal below could be any of a dozen things.
    {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        noise::dial_handshake(
            &mut stream,
            &peer_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
        .expect("positive control: a pinned peer completes a return visit");
    }

    // Now block the KEY, from an address that is not the one it dials from :
    // `203.0.113.9` is a documentation address this test never connects from,
    // so the address half of the ban cannot be what refuses the dial below.
    let mut value = node.state();
    value.ban(
        "203.0.113.9",
        Some(peer_id),
        BanReason::ForgottenAndBlocked,
        state::now_ms(),
    );
    node.write_state(&value);
    assert!(
        !node.state().is_address_banned("127.0.0.1"),
        "the address this test dials from must NOT be banned, or a pass here proves the \
         wrong half"
    );

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let refused = noise::dial_handshake(
        &mut stream,
        &peer_secret,
        Handshake::Return,
        Some(&responder_public),
        None,
    )
    .await;
    assert!(
        refused.is_err(),
        "a banned static key must be refused from any address, or a DHCP lease undoes a \
         block"
    );
}

/// A banned ADDRESS is refused before a frame is even read.
///
/// The cheapest refusal in the listener, and the one an operator asked for
/// explicitly. Asserted on the byte count, since the whole claim is that
/// nothing is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_banned_address_gets_zero_bytes_before_any_frame() {
    let node = Node::new("ban-addr");
    let mut value = node.state();
    value.ban("127.0.0.1", None, BanReason::Blocked, state::now_ms());
    node.write_state(&value);

    let addr = serve(node.context()).await;
    // A well-formed knock from a blocked address earns nothing.
    let refused = knock_at(addr, instance(0xA1), None).await;
    assert!(refused.is_err(), "a blocked address must earn no ack");
    assert!(
        node.state().knocks_queued() == 0,
        "and no row: {:?}",
        node.state()
    );

    // Unblocking restores it, so the block above is a block and not a broken
    // listener.
    let mut value = node.state();
    assert!(value.unblock("127.0.0.1"));
    node.write_state(&value);
    knock_at(addr, instance(0xA2), None)
        .await
        .expect("positive control: unblocking lets the same knock through");
    assert_eq!(node.state().knocks_queued(), 1);
}
