// ---------------------------------------------------------------------------
// Item 11: the network key
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// **A knock without the network key gets zero bytes at a node that has one.**
///
/// The sentence `abuse-resistance.md` promises: "a Mac without the key sees
/// nothing and can send nothing that reaches the UI". The refusal happens
/// inside message 1 (`NNpsk0` mixes the psk before the first token), so there
/// is no queue entry and no row.
///
/// Watched red: delete the `(Some(_), Handshake::Knock)` arm from
/// `listener::serve_knock`'s network-key match and the plain knock is taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_without_the_network_key_gets_zero_bytes() {
    let node = Node::new("netkey-knock");
    let key = NetworkKey::from_bytes([0x5A; 32]);
    let mut file = node.file();
    file.network_key = Some(key);
    node.write_file(&file);
    let addr = serve(node.context()).await;

    let refused = knock_at(addr, instance(0xC1), None).await;
    assert!(
        refused.is_err(),
        "a knock with no network key must earn nothing at a node that requires one"
    );
    assert!(
        node.state().knocks_queued() == 0,
        "and it must reach no row: {:?}",
        node.state()
    );

    // A knock under the WRONG key fails the same way, which is the case that
    // matters on an office network with two meshes on it.
    let refused = knock_at_with_key(addr, instance(0xC2), None, Some(&[0x11; 32])).await;
    assert!(
        refused.is_err(),
        "a knock under another network key must earn nothing either"
    );
    assert_eq!(node.state().knocks_queued(), 0);

    // The positive control: with the right key, the same knock is taken.
    knock_at_with_key(addr, instance(0xC3), None, Some(key.as_bytes()))
        .await
        .expect("positive control: the right network key is admitted");
    assert_eq!(
        node.state().knocks_queued(),
        1,
        "the refusals above are the key check and not a listener that takes no knocks"
    );
}

/// **A knock whose message 1 does not validate reserves nothing**: the ordering
/// found here: the reservation was taken BEFORE
/// message 1 was decrypted, so a stranger sending the right NUMBER of wrong
/// bytes claimed one of the eight pairing slots and made this node write its
/// state file twice before the cryptography had said a word about the sender.
///
/// The instrument is the state FILE, not the queue. Both orders end with an
/// empty `pending` (the old one reserved and then released), so a test that
/// only read `pending` could not tell them apart, and none did. What the two
/// orders cannot both do is leave the file untouched: a reservation is a
/// `state::save`, and `state::save` creates the file.
///
/// Watched red: move the `reserve_knock_slot` block in `listener::serve_knock`
/// back above the `read_message_1_matching` call and this test fails on the
/// first assertion, with `peer-state.json` present and its `pending` empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_whose_message_1_does_not_validate_reserves_nothing() {
    let node = Node::new("knock-order");
    let key = NetworkKey::from_bytes([0x7C; 32]);
    let mut file = node.file();
    file.network_key = Some(key);
    node.write_file(&file);
    let addr = serve(node.context()).await;

    // The file must not exist yet, or the assertion below proves nothing about
    // who created it.
    assert!(
        !node.state.exists(),
        "the premise: this node has written no state file yet"
    );

    // A knock under the WRONG network key. Its message 1 is exactly the right
    // length: `Handshake::from_message_1_len` accepts it and dispatches it as
    // a keyed knock: and it fails inside `NNpsk0`, which mixes the psk before
    // the first token.
    let refused = knock_at_with_key(addr, instance(0xE1), None, Some(&[0x22; 32])).await;
    assert!(
        refused.is_err(),
        "a knock under another network key must earn nothing"
    );
    assert!(
        !node.state.exists(),
        "and it must not have reserved a slot: a reservation writes the state \
         file, and nothing here was ever entitled to one"
    );
    assert!(
        node.state().knocks_queued() == 0,
        "no row either, which was true before this fix as well"
    );

    // The positive control, on the same node: a knock that DOES validate
    // reserves, records and leaves the file behind. Without it, the two
    // assertions above would pass against a listener that had stopped writing
    // state at all.
    knock_at_with_key(addr, instance(0xE2), None, Some(key.as_bytes()))
        .await
        .expect("positive control: the right network key is admitted");
    assert!(
        node.state.exists(),
        "a knock that validates does write the state file"
    );
    assert_eq!(
        node.state().knocks_queued(),
        1,
        "and it leaves exactly one row for the operator"
    );
}

/// **A reservation placeholder is never shown to the operator.**
///
///
/// A reservation is a row in `PeerState::pending` with an all-zero instance id
/// and no name, pushed before the handshake so the eight-row cap is taken
/// atomically. `tcr peer pending` and the panel render `pending` verbatim, so a
/// connection that stalled showed the operator a pairing request from nobody,
/// with a blank name, that no Accept could complete.
///
/// Two readers of one row, which is why this is a filter and not a narrower
/// field: the listener must keep COUNTING it (it is the cap) and the operator
/// must not be SHOWN it.
///
/// Watched red: make `PeerState::visible_pending` return `self.pending.clone()`
/// and the first assertion fails.
#[test]
fn a_reservation_placeholder_is_counted_but_never_rendered() {
    let mut value = PeerState::default();
    let now = state::now_ms();

    assert_eq!(
        value.reserve_knock_slot("10.0.0.9", now),
        Ok(true),
        "the reservation is created"
    );
    assert_eq!(
        value.knocks_queued(),
        1,
        "and the listener counts it, which is the whole reason it exists"
    );
    assert!(
        value.visible_pending().is_empty(),
        "but the operator is shown nothing: {:?}",
        value.visible_pending()
    );

    // Once the handshake fills it in, the SAME row is visible: the filter is
    // about an unfilled placeholder and not about the address.
    value
        .record_knock(
            "10.0.0.9",
            instance(0x5B),
            Some("studio-mac".to_string()),
            tcr_peer_wire::PROTO_VERSION,
            None,
            now,
        )
        .expect("the knock records onto the reserved row");
    assert_eq!(
        value.knocks_queued(),
        1,
        "it coalesced onto the reservation rather than adding a second row"
    );
    assert_eq!(
        value.visible_pending().len(),
        1,
        "and now the operator sees it"
    );
    assert_eq!(value.visible_pending()[0].instance_id, instance(0x5B));
}

/// A knock that CARRIES the key at a node that has none is refused too.
///
/// The symmetric case, and it is not cosmetic: a half-configured office would
/// otherwise have Macs that could knock at each other in one direction only,
/// which is the sort of asymmetry that gets diagnosed as a network fault.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_keyed_knock_at_a_keyless_node_is_refused() {
    let node = Node::new("netkey-asymmetric");
    let addr = serve(node.context()).await;

    let refused = knock_at_with_key(addr, instance(0xD1), None, Some(&[0x33; 32])).await;
    assert!(
        refused.is_err(),
        "a node with no network key has nothing to verify a keyed knock against"
    );
    assert_eq!(node.state().knocks_queued(), 0);

    knock_at(addr, instance(0xD2), None)
        .await
        .expect("positive control: a plain knock is taken at a keyless node");
}

/// HMAC-SHA256 is verified against **RFC 4231 test case 2**, not against its
/// own output.
///
/// A hand-rolled MAC that is only ever compared to itself is a MAC that agrees
/// with nobody: it would be self-consistent, pass every round-trip test in this
/// file, and interoperate with no other implementation, including the next
/// build of this program, if the construction were ever replaced by the `hmac`
/// crate.
///
/// Case 2 is `key = "Jefe"`, `data = "what do ya want for nothing?"`, and its
/// published HMAC-SHA-256 digest is
/// `5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843`.
///
/// The construction here takes a `[u8; 32]` key, and `"Jefe"` is four bytes :
/// so this drives the private function through a shim in the module that owns
/// it, where the key width is the one the RFC vector needs.
#[test]
fn hmac_sha256_matches_rfc_4231_case_2() {
    let digest = config::drop::hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        "the HMAC construction must agree with RFC 4231 case 2, or it agrees with nobody"
    );
}

/// The network key round-trips through its paste string, and a short or
/// mistyped paste is refused rather than silently truncated.
#[test]
fn the_network_key_round_trips_through_its_paste_string() {
    let key = NetworkKey::from_bytes([0x7C; 32]);
    let pasted = key.to_paste_string();
    assert_eq!(
        pasted.chars().count(),
        52,
        "32 bytes in Crockford base32 is 52 characters, and this is {pasted}"
    );
    assert_eq!(
        NetworkKey::try_from(pasted.clone()).expect("the paste string reads back"),
        key
    );
    // Whitespace around a paste is what a paste buffer adds, and refusing it
    // would send an operator hunting a problem they cannot see.
    assert_eq!(
        NetworkKey::try_from(format!("  {pasted}\n")).expect("a trimmed paste reads back"),
        key
    );
    // A short paste is refused, not padded.
    assert!(NetworkKey::try_from(pasted[..40].to_string()).is_err());
    // And the debug form never prints the bytes: a `{:?}` in a log line is how
    // a shared secret ends up in a file.
    assert_eq!(format!("{key:?}"), "NetworkKey(set)");
}

// ---------------------------------------------------------------------------
// Item 12: the share link
// ---------------------------------------------------------------------------

/// **A share link round-trips**, with and without a join key riding along.
///
/// Watched red: change `ShareLink::to_link` to emit `v=2` and the parse refuses
/// with "is not v1".
#[test]
fn a_share_link_round_trips_with_and_without_a_join_key() {
    let network_key = NetworkKey::from_bytes([0x2B; 32]);
    let token = join_key::JoinToken::new(
        vec!["127.0.0.1:9600".parse().expect("a loopback address")],
        PeerId([0x3C; 32]),
        [0x4D; 32],
    );

    let bare = pair::ShareLink {
        network_key,
        join: None,
    };
    let rendered = bare.to_link();
    assert!(
        rendered.starts_with(pair::LINK_PREFIX),
        "a link must be openable by the URL handler, and this one is {rendered}"
    );
    assert_eq!(
        pair::ShareLink::parse(&rendered).expect("a bare link parses"),
        bare
    );

    let invited = pair::ShareLink {
        network_key,
        join: Some(token.clone()),
    };
    let rendered = invited.to_link();
    let parsed = pair::ShareLink::parse(&rendered).expect("an invite link parses");
    assert_eq!(parsed, invited);
    assert_eq!(
        parsed
            .join
            .as_ref()
            .map(|join| join.sockets().collect::<Vec<_>>()),
        Some(token.sockets().collect::<Vec<_>>()),
        "the join key inside a link must carry the address to dial"
    );

    // Every refusal names its cause, because the operator's next move depends
    // on which of these it was.
    for (bad, needle) in [
        ("https://example.com/", "not a share link"),
        ("tcr://peer/join?nk=AAAA", "no `v=` version"),
        (
            &format!("tcr://peer/join?v=2&nk={}", network_key.to_paste_string()),
            "not v1",
        ),
        ("tcr://peer/join?v=1", "no `nk=` network key"),
        (
            &format!(
                "tcr://peer/join?v=1&nk={}&surprise=1",
                network_key.to_paste_string()
            ),
            "field this build does not know",
        ),
    ] {
        let error = pair::ShareLink::parse(bad)
            .expect_err(&format!("{bad:?} must be refused, not guessed at"));
        assert!(
            format!("{error:#}").contains(needle),
            "the refusal for {bad:?} has to name {needle:?}: {error:#}"
        );
    }
}

/// **A link whose join key is spent still sets the network key.**
///
/// The two halves are independent by design: a link posted in a channel is
/// opened by whoever gets there first, and everyone after them still has to
/// end up on the office mesh: otherwise the second person to click reads
/// "expired" and has no idea their real problem is now that they cannot see
/// anybody.
///
/// Driven through the parse and the two accessors `tcr peer join` uses, so what
/// is asserted is what the CLI acts on.
#[test]
fn a_link_with_a_spent_join_key_still_sets_the_network_key() {
    let network_key = NetworkKey::from_bytes([0x6E; 32]);
    let link = pair::ShareLink {
        network_key,
        // A secret for an invite that is no longer on the registrar's disk:
        // the enrolment will fail, and the network key must not.
        join: Some(join_key::JoinToken::new(
            vec!["127.0.0.1:9600".parse().expect("a loopback address")],
            PeerId([0x7F; 32]),
            [0x80; 32],
        )),
    };

    let input = pair::JoinInput::parse(&link.to_link()).expect("the link parses");
    assert_eq!(
        input.network_key(),
        Some(network_key),
        "the network key half of a link is what a `tcr peer join` sets FIRST, before it \
         tries to enrol: so a spent join key cannot cost it"
    );
    assert!(
        input.join_token().is_some(),
        "and the join key is still offered, so the enrolment is attempted and its failure \
         is reported"
    );
}

/// `tcr peer join` accepts a link OR a bare join key, and refuses anything
/// else by naming both shapes.
#[test]
fn join_input_accepts_a_link_or_a_bare_key() {
    let token = join_key::JoinToken::new(
        vec!["127.0.0.1:9600".parse().expect("a loopback address")],
        PeerId([0x11; 32]),
        [0x22; 32],
    );
    let bare = pair::JoinInput::parse(&token.to_token()).expect("a bare key parses");
    assert_eq!(bare.join_token(), Some(&token));
    assert_eq!(
        bare.network_key(),
        None,
        "a bare join key carries no network key, and inventing one would put a Mac on a \
         mesh nobody offered it"
    );

    let link = pair::ShareLink {
        network_key: NetworkKey::from_bytes([0x33; 32]),
        join: Some(token.clone()),
    };
    let from_link = pair::JoinInput::parse(&link.to_link()).expect("a link parses");
    assert_eq!(from_link.join_token(), Some(&token));
    assert!(from_link.network_key().is_some());

    let error = pair::JoinInput::parse("hello").expect_err("neither shape must be refused");
    let text = format!("{error:#}");
    assert!(
        text.contains("tcr://peer/join?") && text.contains("tcr-join:"),
        "the refusal has to name BOTH shapes, because the operator does not know which one \
         they were given: {text}"
    );
}

// ---------------------------------------------------------------------------
// Item 4: `--stdin`, and no log line carries the secret
// ---------------------------------------------------------------------------

/// **`tcr peer join --stdin` reads the key off standard input**, and neither
/// its output nor its error output ever carries the secret.
///
/// Runs the binary this build produced (`CARGO_BIN_EXE_tcr`), never the
/// installed one, and points the whole peer surface at a temp file with
/// `--peers`, so it reads no real config and touches no running proxy.
///
/// The join itself FAILS here: there is no registrar at the address in the
/// token, and that is the case worth greping: a failure path prints more than
/// a success path, so if any line is going to echo the token back it is one of
/// these.
///
/// Watched red: add `eprintln!("{}", token.to_token())` to the `Join` arm of
/// `run_peer` and this fails with "the join key must never appear in output".
#[test]
fn join_stdin_keeps_the_key_out_of_argv_and_every_log_line() {
    let dir = scratch("join-stdin");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // A token pointing at a port nothing is listening on: the dial fails, which
    // is the noisy path.
    let token = join_key::JoinToken::new(
        vec!["127.0.0.1:1".parse().expect("a loopback address")],
        PeerId([0x99; 32]),
        [0xAA; 32],
    );
    let rendered = token.to_token();
    let secret_field = tcr_peer_wire::Crockford::encode(&token.secret);

    let mut child = tcr_command(&dir.join("cli-home"))
        .args([
            "peer",
            "join",
            "--peers",
            peers.to_str().expect("a utf-8 path"),
            "--stdin",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the tcr built by this build");
    child
        .stdin
        .as_mut()
        .expect("the child's stdin")
        .write_all(format!("{rendered}\n").as_bytes())
        .expect("write the token to stdin");
    let output = child.wait_with_output().expect("the child exits");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Positive control: the command really ran and really read the token: it
    // names the address the token carries. Without this, a binary that failed
    // on its arguments would satisfy every absence below.
    assert!(
        combined.contains("127.0.0.1:1"),
        "positive control: the command did not get as far as dialling the token's address, \
         so its silence about the secret proves nothing. Output: {combined}"
    );

    assert!(
        !combined.contains(&secret_field),
        "the join key's secret field must never appear in output: a terminal keeps \
         scrollback and a failed paste is still a live bearer secret. Output: {combined}"
    );
    assert!(
        !combined.contains(&rendered),
        "the whole join key must never appear in output either. Output: {combined}"
    );
}

/// The `--stdin` reader takes ONE line, refuses an empty one, and never prints
/// what it read.
///
/// One line rather than read-to-end: reading to end of file would make a
/// trailing newline plus anything after it part of the secret field, and the
/// operator would see a refusal about base32 for a key that was fine.
#[test]
fn the_stdin_reader_takes_one_line_and_refuses_an_empty_one() {
    let token = join_key::JoinToken::new(
        vec!["127.0.0.1:9600".parse().expect("a loopback address")],
        PeerId([0x44; 32]),
        [0x55; 32],
    );
    let rendered = token.to_token();

    let input = pair::join_input_from_reader(std::io::Cursor::new(format!(
        "{rendered}\nand a second line nobody asked for\n"
    )))
    .expect("one line is read");
    assert_eq!(input.join_token(), Some(&token));

    let error = pair::join_input_from_reader(std::io::Cursor::new("\n"))
        .expect_err("an empty line must be refused");
    assert!(
        format!("{error:#}").contains("--stdin"),
        "the refusal has to name the flag, because that is what the operator has to fix: \
         {error:#}"
    );
}
