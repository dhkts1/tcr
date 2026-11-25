// ---------------------------------------------------------------------------
// Item 1: `accept_enrolment` gets its production caller
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// **A headless join leaves a pinned row on BOTH sides, through the shipped
/// accept loop.**
///
/// `accept_enrolment` used to have no production caller at all
/// (`pair.rs:318`), which means the whole headless path: the one a Mac with no
/// screen has, did nothing: `tcr peer join` completed its `IKpsk1` handshake,
/// sent its `Control::Enroll`, and the registrar's stream gate refused it for
/// want of the very pinned row that frame exists to create. The joiner then
/// waited for a `Hello` that never came.
///
/// Driven through `listener::serve_on_with`, which is the production accept
/// loop, and `pair::join_as`, which is what the CLI calls. Both sides are
/// asserted, because "the joiner pinned the registrar" was already true
/// beforehand and told nobody anything: the registrar's row is the half that was
/// missing.
///
/// Watched red: comment out the `if session.handshake == Handshake::Enrol { …
/// serve_enrolment … }` block in `listener::serve_stream` and this fails with
/// the joiner's own refusal: "the registrar closed the stream without
/// answering the enrolment": and no row on the registrar.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_two_process_join_leaves_a_pinned_row_on_both_sides() {
    let registrar = Node::new("enrol-registrar");
    let joiner = Node::new("enrol-joiner");

    // The registrar needs a listen address in its file before it can mint an
    // invite: a token with no address to dial is not a token.
    let addr = serve(registrar.context()).await;
    registrar.listen_on(addr);

    let store = config::PeerStore::open(&registrar.peers).expect("open the registrar's store");
    let (invite, token) = pair::mint_invite_as(&store, &registrar.key, "laptop-2", 600, 1, None)
        .expect("mint a one-use invite");
    assert_eq!(invite.uses_left, 1);

    let joiner_store = config::PeerStore::open(&joiner.peers).expect("open the joiner's store");
    let pinned = pair::join_as(&joiner_store, &joiner.key, &token, "this-mac")
        .await
        .expect("the join completes and is acknowledged");
    assert_eq!(pinned.peer, registrar.key.id());

    // The registrar's half: the one this used to never write.
    let registrar_file = registrar.file();
    assert!(
        registrar_file
            .peers
            .iter()
            .any(|row| row.node == joiner.key.id()),
        "the registrar must hold a pinned row for the joiner; it holds {:?}",
        registrar_file
            .peers
            .iter()
            .map(|row| row.node.display())
            .collect::<Vec<_>>()
    );
    // And the invite is spent in the same write, so a one-use key is one use.
    assert!(
        registrar_file.invite(invite.id).is_none(),
        "a one-use invite must be gone once it has admitted a machine, and it remains as {:?}",
        registrar_file.invite(invite.id)
    );

    // The joiner's half.
    assert!(
        joiner
            .file()
            .peers
            .iter()
            .any(|row| row.node == registrar.key.id()),
        "the joiner must hold a pinned row for the registrar"
    );

    // Every grant is at its default on the row enrolment wrote: a bare pin can
    // say hello and nothing else.
    let row = registrar_file
        .peers
        .iter()
        .find(|row| row.node == joiner.key.id())
        .expect("the row just asserted");
    assert!(!row.allow.inspect, "enrolment grants nothing");
    assert!(!row.allow.gateway, "enrolment grants nothing");
    assert!(!row.allow.relay, "enrolment grants nothing");
    assert!(row.lend.is_empty(), "enrolment lends nothing");
}

/// **A headless join teaches the registrar a dialable port, never the
/// accepted socket's ephemeral one.**
///
/// The registrar's only address fact about a joiner used to be the socket the
/// enrolment frame arrived on, written whole at rank 0: a joiner's ephemeral
/// source port for that one connection, which answers nothing once it closes.
/// Measured in the lab: after a sealed pairing one side's row held
/// `100.64.99.11:56804` while `7755` is what actually listened, and a connect
/// on the recorded port was refused while a connect on the listening one was
/// fine.
///
/// The joiner is given its own served listener, exactly as the registrar is
/// above, so this test can tell the two ports apart: the registrar's row for
/// the joiner must hold the joiner's LISTENING port and hold nothing at the
/// ephemeral port the dial actually used.
///
/// Watched red: this test fails on the base with the recorded port being the
/// ephemeral one, not the joiner's listening port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_headless_join_teaches_the_registrar_the_joiners_listening_port() {
    let registrar = Node::new("enrol-port-registrar");
    let joiner = Node::new("enrol-port-joiner");

    let registrar_addr = serve(registrar.context()).await;
    registrar.listen_on(registrar_addr);

    // The joiner also runs the shipped accept loop on its own kernel-chosen
    // port and records it, the same way the registrar's own listen address is
    // set above: this is the port a dial back has to reach, and it is not the
    // port the joiner's outbound connection to the registrar happens to use.
    let joiner_addr = serve(joiner.context()).await;
    joiner.listen_on(joiner_addr);

    let store = config::PeerStore::open(&registrar.peers).expect("open the registrar's store");
    let (_invite, token) = pair::mint_invite_as(&store, &registrar.key, "laptop-3", 600, 1, None)
        .expect("mint a one-use invite");

    let joiner_store = config::PeerStore::open(&joiner.peers).expect("open the joiner's store");
    pair::join_as(&joiner_store, &joiner.key, &token, "this-mac")
        .await
        .expect("the join completes and is acknowledged");

    let registrar_file = registrar.file();
    let row = registrar_file
        .peers
        .iter()
        .find(|row| row.node == joiner.key.id())
        .expect("the registrar pinned a row for the joiner");

    assert!(
        row.endpoints
            .iter()
            .any(|endpoint| endpoint.direct_addr() == Some(joiner_addr)),
        "the registrar must hold a dialable endpoint at the joiner's listening port \
         {joiner_addr}: {:?}",
        row.endpoints
    );
    assert!(
        row.endpoints
            .iter()
            .all(|endpoint| endpoint.direct_addr() != Some(registrar_addr)),
        "sanity: no endpoint on this row should equal the registrar's own listening address"
    );
    assert_eq!(
        row.endpoints.len(),
        1,
        "the registrar must hold exactly the joiner's listening port and nothing else, not \
         also the accepted socket's ephemeral source port: {:?}",
        row.endpoints
    );
}

// ---------------------------------------------------------------------------
// Item 2: a one-use invite is atomic
// ---------------------------------------------------------------------------

/// **Two joiners racing one one-use invite leave exactly one `Ok`.**
///
/// Testing measured this at 40 double-accepts out of 40 trials:
/// `accept_enrolment` read the file, saw `usesLeft: 1`, and wrote: and two
/// copies of that body running together each read the same 1. `save` being
/// atomic does not help, because atomicity is about the file never being
/// half-written and this is two complete writes each built on a stale read.
///
/// Forty trials rather than one, deliberately. A single trial of a race is a
/// coin toss: it can pass on the unlocked code by luck. Forty is the same
/// number that testing used, so this test is measured against the same
/// instrument that found the bug.
///
/// Watched red: remove the `FileLock::acquire` line from `accept_enrolment` and
/// this fails inside the first few trials with "trial N admitted 2 machines on
/// a ONE-USE invite".
#[test]
fn two_concurrent_joiners_on_one_use_invite_leave_exactly_one_ok() {
    /// The same trial count, kept so this gate is as sensitive as the
    /// probe that found the defect.
    const TRIALS: usize = 40;

    for trial in 0..TRIALS {
        let node = Node::new(&format!("atomic-{trial}"));
        node.listen_on("127.0.0.1:9600".parse().expect("a loopback address"));

        let store = config::PeerStore::open(&node.peers).expect("open the store");
        let (invite, token) = pair::mint_invite_as(&store, &node.key, "laptop-2", 600, 1, None)
            .expect("mint a one-use invite");

        // Two joiners with DIFFERENT static keys, which is the case that
        // matters: two rows for two machines off one key.
        let first = PeerId([0x11; 32]);
        let second = PeerId([0x22; 32]);
        let enroll = tcr_peer_wire::Enroll {
            invite_id: 0,
            label: "laptop-2".to_string(),
        };

        let peers_path = node.peers.clone();
        let now = state::now_ms();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|joiner| {
                let peers_path = peers_path.clone();
                let enroll = enroll.clone();
                let secret = token.secret;
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    // Both threads arrive at the read together, which is what
                    // makes this a race and not two sequential calls.
                    barrier.wait();
                    pair::accept_enrolment(
                        &peers_path,
                        joiner,
                        &enroll,
                        &secret,
                        now,
                        Some(SocketAddr::from(([127, 0, 0, 1], 9600))),
                    )
                })
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("the enrolling thread"))
            .collect();
        let admitted = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(
            admitted, 1,
            "trial {trial} admitted {admitted} machines on a ONE-USE invite. A one-use join \
             key that admits two Macs is the whole failure this lock exists to prevent"
        );

        // And the file agrees: one pinned row, no invite left.
        let after = node.file();
        assert_eq!(
            after.peers.len(),
            1,
            "trial {trial}: the peers file holds {} rows after a one-use invite",
            after.peers.len()
        );
        assert!(
            after.invite(invite.id).is_none(),
            "trial {trial}: the spent invite is still on disk, which is still a live PSK"
        );

        std::fs::remove_dir_all(&node.dir).log_debug("removing a test directory");
    }
}
