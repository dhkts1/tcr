// ---------------------------------------------------------------------------
// The identifier and the locator are different types
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// **An older peers file keeps its routing advice.**
///
/// The old key was `addrs: ["host:port"]` and the new one is `endpoints`, a
/// list that also says WHEN each was observed and WHAT observed it. An
/// operator who upgrades has one file, not two, so the reader has to accept
/// the old key, and the next save has to write the new one, or the migration
/// runs again on every read forever.
///
/// Three facts, because three separate things can break: the endpoint arrives,
/// it arrives with the row's own `addedAt` as its observation instant (the only
/// honest answer, the old key carried no time of its own), and the file that
/// comes back out names `endpoints` and no longer names `addrs`.
///
/// Watched red: with the `for legacy in &disk.addrs` loop deleted from
/// `PeerRowOnDisk`'s `From` impl (`src/peer/config.rs`), this fails on the
/// first assertion, `left: 0, right: 1`, "an upgraded file keeps the one
/// address it was paired over", which is precisely the operator losing the
/// only way back to a Mac they had already trusted.
#[test]
fn a_peers_file_with_the_old_addrs_key_loads_and_re_saves_as_endpoints() {
    let dir = scratch("legacy-addrs");
    let path = dir.join("tcr-peers.json");

    // Written by hand rather than by `save`, because the shape under test is
    // the one no build in this tree can produce any more.
    let legacy = r#"{
      "peers": [
        {
          "node": "248H248H248H248H248H248H248H248H248H248H248H248H248G",
          "label": "studio-mac",
          "addrs": ["192.0.2.7:9600"],
          "addedAt": 1767225600000,
          "allow": {},
          "lend": []
        }
      ]
    }"#;
    std::fs::write(&path, legacy).expect("write the legacy peers file");
    // 0600, because  refuses a peers file this program did
    // not write: an invite secret sits in this file.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("tighten the fixture to the mode the reader requires");

    let file = config::read_or_default(&path).expect("a legacy peers file still reads");
    let row = &file.peers[0];
    assert_eq!(
        row.endpoints.len(),
        1,
        "an upgraded file keeps the one address it was paired over: {:?}",
        row.endpoints
    );
    assert_eq!(
        row.endpoints[0],
        config::Endpoint::direct(
            "192.0.2.7:9600".parse().expect("the literal parses"),
            1_767_225_600_000,
            config::EndpointSource::Paired,
        ),
        "the old key was written at the pin, so it reads back as Paired, observed when the \
         row was added"
    );

    config::save(&path, &file).expect("the migrated file writes");
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("re-read the saved file"))
            .expect("the saved file is json");
    let saved = &written["peers"][0];
    assert!(
        saved["endpoints"].is_array(),
        "the save writes the new key: {saved}"
    );
    assert!(
        saved.get("addrs").is_none(),
        "and stops writing the old one, or the migration runs on every read forever: {saved}"
    );
    assert_eq!(
        saved["endpoints"][0]["kind"], "direct",
        "a socket endpoint is tagged `direct`, so a `via` hop is a different shape and not a \
         string that happens to look different: {saved}"
    );
}

// ---------------------------------------------------------------------------
// A pin that keeps no way back has trusted a Mac it cannot
// reach
// ---------------------------------------------------------------------------

/// **The `confirm` half of a pinned row records the address the pin was taken
/// over; the `accept_enrolment` half never records the accepted socket at
/// all.**
///
/// **What this test used to assert, and why that changed.** It used to assert
/// that `accept_enrolment` wrote exactly one endpoint, `Some(arrived_from)`
/// with `arrived_from` an ephemeral source port
/// (`192.0.2.8:51234`), and that the row on disk carried it too. That was the
/// bug this repository's "no address is ever learned from a carrier's socket"
/// fix removed: `arrived_from`'s port is the far side's kernel's own choice
/// for that ONE connection, never a port anything listens on, and writing it
/// as a dialable endpoint is what left a Mac pinned at a port that refused
/// every connect. The registrar records the joiner's IP and the ANNOUNCED
/// port instead, never the accepted socket's port, and it learns the
/// announced port from the joiner's own `Control::Hello`, sent on the same
/// live stream immediately after this call returns
/// (`pair::join_as`), through the ordinary CONTROL arm's combine
/// (`config::endpoints_and_connection_from_hello`). `accept_enrolment` itself
/// never sees that frame, so this test, which calls it directly, now asserts
/// it writes nothing; `a_headless_join_teaches_the_registrar_the_joiners_listening_port`
/// (above) is what watches the end-to-end port over the shipped accept loop.
///
/// The `confirm` half's contract is unchanged: it is the CLASS invariant this
/// test still keeps a live assertion for, a row a completed six-digit pin
/// created has at least one endpoint, and its own watched-red line still
/// holds.
///
/// Watched red twice.
/// - `confirm` half: with `pin_row(store, *peer, &label, addr)` changed back to
///   `None`, this fails on "the six-digit pin records the socket the compare
///   ran over" with `left: 0, right: 1`.
/// - `accept_enrolment` half: restore `row.observe_endpoint(Endpoint::direct(from, ...))`
///   with `from` widened back to a bare `SocketAddr`, and this fails on "the
///   registrar records no endpoint from the socket it accepted" with
///   `left: 1, right: 0`.
#[test]
fn every_pin_writer_records_the_address_it_was_reached_over() {
    let node = Node::new("pin-records-address");
    let store = config::PeerStore::open(&node.peers).expect("open the store");

    // ---- the six-digit compare, as `tcr peer pair <host:port> <code>` runs it
    let paired_over: SocketAddr = "192.0.2.7:9600".parse().expect("the literal parses");
    let peer = PeerId([0x21_u8; 32]);
    pair::confirm(&store, &peer, "123456", Some(paired_over)).expect("the pin writes");

    let row = node
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == peer)
        .expect("the compare pinned a row");
    assert_eq!(
        row.endpoints.len(),
        1,
        "the six-digit pin records the socket the compare ran over: {:?}",
        row.endpoints
    );
    assert_eq!(
        row.endpoints[0].direct_addr(),
        Some(paired_over),
        "and it is that socket and not a rewritten one: {:?}",
        row.endpoints
    );
    assert_eq!(
        row.endpoints[0].source,
        config::EndpointSource::Paired,
        "recorded as Paired, because the pin itself is what observed it"
    );

    // ---- the registrar's half of an enrolment
    // An invite needs a listen address to hand out, so the file names one; the
    // port is never bound here because this half drives `accept_enrolment`
    // directly rather than over a socket.
    let mut file = node.file();
    file.listen = Some("127.0.0.1:9600".parse().expect("the literal parses"));
    node.write_file(&file);
    let store = config::PeerStore::open(&node.peers).expect("re-open after the listen edit");
    let (invite, _token) = pair::mint_invite_as(&store, &node.key, "attic-nuc", 600, 1, None)
        .expect("mint a one-use invite");
    let joiner = PeerId([0x22_u8; 32]);
    let arrived_from: SocketAddr = "192.0.2.8:51234".parse().expect("the literal parses");
    let enrolled = pair::accept_enrolment(
        &node.peers,
        joiner,
        &tcr_peer_wire::Enroll {
            invite_id: 0,
            label: "attic-nuc".to_string(),
        },
        &invite.secret,
        1_767_225_600_000,
        Some(arrived_from),
    )
    .expect("the joiner proves the invite and is pinned");

    assert_eq!(
        enrolled.endpoints.len(),
        0,
        "the registrar records no endpoint from the socket it accepted: its port is the \
         joiner's ephemeral one for this one connection, never a port to dial; the dialable \
         one comes from the joiner's own Hello on the same stream, which this direct call \
         does not send: {:?}",
        enrolled.endpoints
    );

    // The row on disk agrees, which is the half that survives a restart: an
    // enrolment pins a joiner it cannot yet dial back, and stays that way
    // until the joiner's own Hello, on the very next frame of the same
    // stream, teaches the announced port.
    let saved = node
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == joiner)
        .expect("the enrolment pinned a row");
    assert_eq!(
        saved.endpoints, enrolled.endpoints,
        "the empty endpoint list is on disk and not only in the returned row"
    );
}

// ---------------------------------------------------------------------------
// An endpoint is a side effect of an authenticated session
// ---------------------------------------------------------------------------

/// **B changes its listen port, tells A in the next Hello, and A reaches it
/// without re-pairing.**
///
/// This is the whole of what "identity is the key, an address is advice" buys.
/// A pinned row is not re-pinned, no operator presses anything, no six digits
/// are compared again: B dials A inside an ordinary `IK` session against the
/// static key A already trusts, its `Hello` carries the socket it now listens
/// on, and A writes that against B's row. WireGuard's roaming rule, in this
/// tree's own types.
///
/// Driven over real loopback sockets and the shipped accept loop, because the
/// claim is about two facts meeting: the producer filling `Hello.addrs` and
/// the consumer merging them. A unit test on either half alone would pass with
/// the other half missing, which is exactly the state this tree was in, the
/// field has been on the wire since the skeleton with nothing filling it.
///
/// The stale endpoint is a POSITIVE CONTROL and not scenery: the first
/// assertion proves A really cannot reach B before the Hello, so the third
/// assertion is about the Hello and not about a port that was always open.
///
/// Watched red: with the `Control::Hello(incoming)` arm's
/// `config::observe_endpoints(...)` call deleted from
/// `listener::serve_control`, the last assertion fails, "A dials the port B
/// announced", because A's row still holds only the dead port, which is a
/// peer that moved one desk and is now unreachable forever.
#[tokio::test]
async fn a_peer_that_moved_announces_its_new_port_and_is_reached_there() {
    let a = Node::new("hello-refresh-a");
    let b = Node::new("hello-refresh-b");

    // A accepts; B's real listener is a second accept loop on its own port.
    let a_addr = serve(a.context()).await;
    let b_addr = serve(b.context()).await;

    // A dead port B is NOT listening on: bound, read for its number, dropped.
    let dead = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port to learn its number");
        probe.local_addr().expect("the bound address")
    };

    // Both sides pin the other, which is what an `IK` session needs. A's row
    // for B names the dead port and nothing else, the state a Mac that moved
    // leaves behind.
    let a_store = config::PeerStore::open(&a.peers).expect("open A's store");
    let b_store = config::PeerStore::open(&b.peers).expect("open B's store");
    pair::confirm(&a_store, &b.key.id(), "111111", Some(dead)).expect("A pins B");
    pair::confirm(&b_store, &a.key.id(), "222222", Some(a_addr)).expect("B pins A");

    // B announces the port it really listens on.
    let mut b_file = b.file();
    b_file.listen = Some(b_addr);
    b.write_file(&b_file);

    // ---- control: before the Hello, A cannot reach B at all.
    let stale_row = a
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == b.key.id())
        .expect("A has a row for B");
    assert!(
        tcr::peer::serve::dial_peer(&stale_row).await.is_none(),
        "the control: A's only endpoint for B is a port nobody listens on, so the dial \
         must fail before the Hello, otherwise the last assertion proves nothing"
    );

    // ---- B says hello to A, over the endpoint B has for A.
    let answered = tcr::peer::serve::say_hello(&b_store, &a.key.id())
        .await
        .expect("the hello round trip completes")
        .expect("A is pinned and answered");
    assert_eq!(
        answered.node,
        a.key.id(),
        "the answer comes from A's own static key"
    );

    // ---- A's row for B now names the port B announced, and A reaches it.
    let refreshed = config::read_or_default(&a.peers)
        .expect("A's file reads")
        .peers
        .into_iter()
        .find(|row| row.node == b.key.id())
        .expect("A still has one row for B, not two");
    assert!(
        refreshed
            .endpoints
            .iter()
            .any(|endpoint| endpoint.direct_addr() == Some(b_addr)),
        "A learned the port B announced: {:?}",
        refreshed.endpoints
    );
    assert_eq!(
        refreshed.added_at, stale_row.added_at,
        "and learned it without re-pinning: the row is the same row, so every grant on it \
         survived"
    );
    assert!(
        tcr::peer::serve::dial_peer(&refreshed).await.is_some(),
        "A dials the port B announced and B answers: {:?}",
        refreshed.endpoints
    );
}

/// **A completed session registers the pair's rendezvous secret, with nothing
/// sent to say so.**
///
/// The dialler and the responder both end on the same handshake hash, and each
/// one derives the pair's port secret from its own copy: that is what lets a
/// moved Mac be met on a port neither end ever named. Until this fix the hash
/// was dropped by `into_transport` the instant the session went to transport
/// mode, so `reach::remember_pair_hash` had no production caller and the
/// register stayed empty on a real session, no matter how many completed.
///
/// The `is_none` before the handshake is the positive control: the key pair is
/// generated in this test, so nothing else in this binary can have registered
/// it, and an assertion that only ever saw a populated register would pass
/// against a register that is never emptied.
///
/// Watched red: drop the `remember_pair_hash` call from `noise.rs`'s dialling
/// constructor and the second assertion reads `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_return_visit_registers_the_pairs_rendezvous_secret() {
    use tcr::peer::reach;

    let node = Node::new("rendezvous-secret");
    let (peer_secret, peer_public) = noise::generate_static().expect("a peer keypair");
    let peer_id = PeerId(peer_public);

    let mut file = node.file();
    file.peers.push(config::PeerRow {
        node: peer_id,
        label: "laptop-9".to_string(),
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
    let responder_id = PeerId(responder_public);

    assert!(
        reach::port_secret_for(&responder_id).is_none(),
        "the responder's key was generated for this test, so nothing may hold a secret \
         for it before the handshake runs"
    );

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let session = noise::dial_handshake(
        &mut stream,
        &peer_secret,
        Handshake::Return,
        Some(&responder_public),
        None,
    )
    .await
    .expect("a pinned peer completes a return visit");

    assert!(
        !session.handshake_hash.is_empty(),
        "the session carries the hash it ended on, or nothing downstream can derive a port"
    );
    assert_eq!(
        reach::port_secret_for(&responder_id),
        Some(reach::port_secret(&session.handshake_hash)),
        "the register holds the DERIVED secret for this pair, and it is the one \
         `reach::port_secret` computes from the session's own hash"
    );
}

/// **A `Hello` carries a brief about a mutual friend, over the shipped accept
/// loop, and the far side records it.**
///
/// The unit gate for the brief wiring. `discovery::neighbor_briefs` and
/// `observe_neighbor_briefs` have tests of their own, and those tests call
/// both functions directly: they stay green with every call site deleted,
/// which was the state of this tree before this fix, the field on the wire
/// and nothing filling it.
///
/// Driven the way `a_peer_that_moved_announces_its_new_port_...` above is, on
/// two real listeners, because the claim is again about two halves meeting:
/// A's answering `Hello` PRODUCING a brief for C, and B's dialling half
/// RECORDING it.
///
/// C never runs. It does not have to: a brief is one trusted Mac's word about
/// another, and what is under test is whether that word travels and lands, not
/// whether C is awake.
///
/// Watched red: with `facts.briefs` set to an empty `Vec` in
/// `listener::serve_control`'s `Control::Hello` arm, the answered Hello
/// carries no brief and the first assertion fails.
#[tokio::test]
async fn a_hello_carries_a_brief_for_a_mutual_friend_and_the_far_side_records_it() {
    let a = Node::new("brief-wire-a");
    let b = Node::new("brief-wire-b");
    let c = PeerId([0x5c; 32]);
    let c_addr: SocketAddr = "192.0.2.44:9700".parse().expect("a literal address");

    let a_addr = serve(a.context()).await;

    let a_store = config::PeerStore::open(&a.peers).expect("open A's store");
    let b_store = config::PeerStore::open(&b.peers).expect("open B's store");
    pair::confirm(&a_store, &b.key.id(), "111111", None).expect("A pins B");
    pair::confirm(&b_store, &a.key.id(), "222222", Some(a_addr)).expect("B pins A");

    // A knows where C answers and grants B the one level of briefs. B trusts C
    // too, with no address for it at all: the state a Mac is in when a friend
    // it has not spoken to since it moved.
    let mut a_file = a.file();
    a_file.peers.push(config::PeerRow {
        node: c,
        label: "studio-mac".to_string(),
        endpoints: vec![config::Endpoint::direct(
            c_addr,
            1_000,
            config::EndpointSource::Hello,
        )],
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow::default(),
        lend: Vec::new(),
    });
    for row in &mut a_file.peers {
        if row.node == b.key.id() {
            row.allow.control.briefs = true;
        }
    }
    a.write_file(&a_file);

    let mut b_file = b.file();
    b_file.peers.push(config::PeerRow {
        node: c,
        label: "the-studio".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow::default(),
        lend: Vec::new(),
    });
    // B grants A the same flag, because `allow.control.briefs` is read on both
    // sides now: it decides what this node TELLS a peer and what it ACCEPTS
    // from one. Before that, any pinned peer could write endpoints onto rows
    // here, whatever the operator had granted it. So the two nodes swapping
    // neighbour lists is two grants, and this test sets both rather than
    // relying on the applying side taking whatever arrives.
    for row in &mut b_file.peers {
        if row.node == a.key.id() {
            row.allow.control.briefs = true;
        }
    }
    b.write_file(&b_file);

    // ---- control: B holds no way to reach C before the Hello.
    let before = b
        .file()
        .peers
        .into_iter()
        .find(|row| row.node == c)
        .expect("B has a row for C");
    assert!(
        before.endpoints.is_empty(),
        "the control: B must start with no endpoint for C, or the assertion below is \
         about an address B always had"
    );

    let answered = tcr::peer::serve::say_hello(&b_store, &a.key.id())
        .await
        .expect("the hello round trip completes")
        .expect("A is pinned and answered");

    // ---- the producing half: A's answer names C.
    let briefs = answered
        .briefs
        .as_deref()
        .expect("A granted B briefs, so the answer carries the list rather than omitting it");
    assert!(
        briefs
            .iter()
            .any(|brief| brief.node == c
                && brief.addrs.iter().any(|addr| addr == &c_addr.to_string())),
        "A's answering Hello has to carry C's locator, or nothing downstream can record \
         it: {briefs:?}"
    );

    // ---- the recording half: B's row for C gains that locator, as a brief.
    let after = config::read_or_default(&b.peers)
        .expect("B's file reads")
        .peers
        .into_iter()
        .find(|row| row.node == c)
        .expect("B still has one row for C, not two");
    let learned = after
        .endpoints
        .iter()
        .find(|endpoint| endpoint.direct_addr() == Some(c_addr))
        .unwrap_or_else(|| panic!("C's locator, learned through A, is on B's row: {after:?}"));
    assert_eq!(
        learned.source,
        config::EndpointSource::Brief,
        "recorded as what taught it, a trusted peer's word, and not as a session B ran \
         with C itself"
    );
}
