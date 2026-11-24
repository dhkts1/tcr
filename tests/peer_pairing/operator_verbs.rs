// ---------------------------------------------------------------------------
// Item 8: the operator verbs, through the built binary
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

#[test]
fn peer_commands_keep_their_node_keys_in_separate_fixture_homes() {
    let first = scratch("first-cli-home");
    let second = scratch("second-cli-home");
    let token = pair::JoinToken::new(
        vec!["127.0.0.1:1".parse().expect("a loopback address")],
        PeerId([0x88; 32]),
        [0x99; 32],
    )
    .to_token();
    let join = |dir: &std::path::Path| {
        let peers = dir.join("tcr-peers.json");
        config::save(&peers, &PeerFile::default()).expect("write the peers file");
        let (out, err, ok) = run_tcr(&peers, &["join", &token]);
        assert!(
            !ok,
            "the deliberately closed registrar must refuse: {out}{err}"
        );
        assert!(
            err.contains("127.0.0.1:1"),
            "the command must reach the fixture's registrar address: {out}{err}"
        );
        std::fs::read(dir.join("cli-home/.config/tcr-node.pub"))
            .expect("the node key must stay inside this fixture's temporary HOME")
    };

    let first_key = join(&first);
    assert_eq!(
        join(&first),
        first_key,
        "one fixture keeps its node identity"
    );
    assert_ne!(
        join(&second),
        first_key,
        "separate fixtures have separate node identities"
    );
}

/// **The five approval verbs work on a real pending row, and `ls --json`
/// reports the counts.**
///
/// Driven through the shipped binary rather than the library, because the claim
/// is about the surface an operator and the panel both read: `tcr peer pending
/// --json` and `tcr peer ls --json` are what the Peers tab is fed from, and a
/// library-level test would not catch a verb that never reached the dispatcher.
///
/// Watched red: delete the `pending`/`pendingCount` fields from `run_peer`'s
/// `PeerLsJson` and this fails on the `pendingCount` assertion.
#[test]
fn the_approval_verbs_and_ls_json_agree_about_one_pending_row() {
    let dir = scratch("verbs");
    let peers = dir.join("tcr-peers.json");
    let state_file = dir.join("peer-state.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // One pending row, written the way the listener writes it.
    let mut value = PeerState::default();
    value
        .record_knock(
            "192.0.2.24",
            instance(0xE1),
            Some("studio-mac".to_string()),
            1,
            None,
            state::now_ms(),
        )
        .expect("queued");
    state::save(&state_file, &value).expect("write the state file");

    // `pending`, as text: the row `abuse-resistance.md` specifies.
    let (out, err, ok) = run_tcr(&peers, &["pending"]);
    assert!(ok, "`tcr peer pending` must exit 0: {err}");
    assert!(
        out.contains("studio-mac")
            && out.contains("192.0.2.24")
            && out.contains("wants to connect"),
        "the pending row must name the Mac AND its address, because a name is a label and \
         never identity: {out}"
    );

    // `pending --json`, which is what the panel reads.
    let (out, err, ok) = run_tcr(&peers, &["pending", "--json"]);
    assert!(ok, "`tcr peer pending --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["pending"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        parsed["pending"][0]["instanceId"].as_str(),
        Some(instance(0xE1).to_wire().as_str())
    );

    // `ls --json` gains the counts and the rows.
    let (out, err, ok) = run_tcr(&peers, &["ls", "--json"]);
    assert!(ok, "`tcr peer ls --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["pendingCount"].as_u64(), Some(1));
    assert_eq!(parsed["blockedCount"].as_u64(), Some(0));
    assert_eq!(parsed["mutedCount"].as_u64(), Some(0));
    assert_eq!(parsed["limited"].as_u64(), Some(0));
    // The caps ride along, so the panel's Advanced pane and this binary cannot
    // disagree about what they are.
    assert_eq!(
        parsed["caps"]["pending"].as_u64(),
        Some(state::MAX_PENDING_KNOCKS as u64)
    );
    assert_eq!(
        parsed["caps"]["foundRows"].as_u64(),
        Some(tcr::peer::discovery::MAX_FOUND_ROWS as u64)
    );

    // `accept` by instance id opens a window for that one Mac.
    let (out, err, ok) = run_tcr(&peers, &["accept", &instance(0xE1).to_wire()]);
    assert!(ok, "`tcr peer accept` must exit 0: {err}");
    assert!(out.contains("peer accept: ok"), "{out}");
    let after = state::load(&state_file, state::now_ms()).expect("the state reads");
    assert!(
        after.knocks_queued() == 0,
        "accepting a row consumes it: the operator has decided"
    );
    assert!(
        after.accepted_window("192.0.2.24", &instance(0xE1), state::now_ms()),
        "and it opens a window for that one instance at that one address"
    );

    // `block` by address bans it, `unblock` lifts it, and the counts follow.
    let (out, err, ok) = run_tcr(&peers, &["block", "192.0.2.24"]);
    assert!(ok, "`tcr peer block` must exit 0: {err}");
    assert!(out.contains("peer block: ok"), "{out}");
    let (out, _err, _ok) = run_tcr(&peers, &["ls", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["blockedCount"].as_u64(), Some(1));

    let (out, err, ok) = run_tcr(&peers, &["unblock", "192.0.2.24"]);
    assert!(ok, "`tcr peer unblock` must exit 0: {err}");
    assert!(out.contains("peer unblock: ok"), "{out}");
    let (out, _err, _ok) = run_tcr(&peers, &["ls", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["blockedCount"].as_u64(), Some(0));

    // `ignore` mutes, and the muted count follows.
    let mut value = PeerState::default();
    value
        .record_knock("192.0.2.25", instance(0xE2), None, 1, None, state::now_ms())
        .expect("queued");
    state::save(&state_file, &value).expect("write the state file");
    let (out, err, ok) = run_tcr(&peers, &["ignore", "192.0.2.25"]);
    assert!(ok, "`tcr peer ignore` must exit 0: {err}");
    assert!(out.contains("peer ignore: ok"), "{out}");
    let (out, _err, _ok) = run_tcr(&peers, &["ls", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(parsed["mutedCount"].as_u64(), Some(1));
    assert_eq!(parsed["pendingCount"].as_u64(), Some(0));

    // And a selector nothing matches is a refusal with a next step, never a
    // silent success.
    let (_out, err, ok) = run_tcr(&peers, &["accept", "192.0.2.99"]);
    assert!(!ok, "accepting a row that does not exist must fail");
    assert!(
        err.contains("tcr peer pending"),
        "the refusal has to say where to look: {err}"
    );
}

/// **Every surface that prints a proposed name masks it, `pending` included.**
///
/// A proposed name is text a stranger's Mac chose. It is sanitized on arrival
/// and masked again on the way out, for the reason `masked_label` gives: this
/// repository is public and the state file is hand-editable JSON, so a row can
/// carry a name that never went through a listener at all.
///
/// `tcr peer ls --json` masked it. `tcr peer pending`, in both its text and
/// its `--json` form, did not, which is one rule with two answers, and the
/// surface that skipped it is the one a menu bar segment and a notification
/// banner read. Both now go through `pending_row_for_readers`.
///
/// Watched red against the shipped binary: before the change `pending --json`
/// answers `alice@example.com` on `proposedName` and this fails on the first
/// assertion.
#[test]
fn every_pending_surface_masks_the_name_a_stranger_proposed() {
    let dir = scratch("pending-mask");
    let peers = dir.join("tcr-peers.json");
    let state_file = dir.join("peer-state.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // A name no label rule accepts, written straight into the state file the
    // way a hand edit can: `@` is not one of the characters
    // `tcr_peer_wire::sanitize_label` allows.
    let mut value = PeerState::default();
    value
        .record_knock(
            "192.0.2.24",
            instance(0xE7),
            Some("alice@example.com".to_string()),
            1,
            None,
            state::now_ms(),
        )
        .expect("queued");
    state::save(&state_file, &value).expect("write the state file");

    let (out, err, ok) = run_tcr(&peers, &["pending", "--json"]);
    assert!(ok, "`tcr peer pending --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        parsed["pending"][0]["proposedName"].as_str(),
        Some("[masked]"),
        "the panel, the menu bar and a notification banner all read this field: {out}"
    );
    assert!(
        !out.contains("alice@example.com"),
        "the raw name reached the surface anyway, somewhere else in the document: {out}"
    );

    // The text form is the same rule, and it is what an operator reads.
    let (out, err, ok) = run_tcr(&peers, &["pending"]);
    assert!(ok, "`tcr peer pending` must exit 0: {err}");
    assert!(
        out.contains("[masked]") && !out.contains("alice@example.com"),
        "the two pending surfaces disagree about masking, which is how one of them \
         quietly stops: {out}"
    );

    // And the address is still the dialable one, so masking did not cost the
    // row the thing an answer is aimed at.
    assert!(
        out.contains("192.0.2.24"),
        "the address a reader dials went missing: {out}"
    );

    // `ls --json` masked this before and had no test saying so, which is how
    // the rule came to hold on one surface only. Both are pinned now, on the
    // one function that decides it.
    let (out, err, ok) = run_tcr(&peers, &["ls", "--json"]);
    assert!(ok, "`tcr peer ls --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        parsed["pending"][0]["proposedName"].as_str(),
        Some("[masked]"),
        "the listing stopped masking the name a stranger proposed: {out}"
    );
}

/// **Through the real CLI: a link whose join key is spent still sets the
/// network key.**
///
/// `a_link_with_a_spent_join_key_still_sets_the_network_key` asserts the two
/// accessors; this asserts the ORDER the binary acts in, which is the thing
/// that matters: a `tcr peer join` that enrolled first and set the key second
/// would leave the second person to click a link with "expired" on screen and
/// no idea their real problem is that they now cannot see anybody.
///
/// The join key points at a port nothing is listening on, so the enrolment
/// fails as loudly as it can. The network key must be on disk anyway.
///
/// Watched red: move the `if let Some(network_key) = input.network_key()` block
/// in `run_peer`'s `Join` arm below the `pair::join` call and this fails with
/// the peers file carrying no network key.
#[test]
fn a_link_with_a_dead_join_key_still_sets_the_network_key_through_the_cli() {
    let dir = scratch("link-cli");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    let network_key = NetworkKey::from_bytes([0x9C; 32]);
    let link = pair::ShareLink {
        network_key,
        join: Some(pair::JoinToken::new(
            vec!["127.0.0.1:1".parse().expect("a loopback address")],
            PeerId([0xAB; 32]),
            [0xCD; 32],
        )),
    }
    .to_link();

    let (out, err, ok) = run_tcr(&peers, &["join", &link]);
    assert!(
        !ok,
        "the enrolment half must fail (nothing is listening on that port), so this test \
         is about what survived a failure: {out}{err}"
    );
    assert!(
        out.contains("network key set"),
        "the command must say it set the network key before it tried to enrol: {out}"
    );

    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key,
        Some(network_key),
        "a dead join key must not cost the network key: the two halves of a link are \
         independent, and the key is the half everyone who clicks late still needs"
    );

    // And the failure was reported rather than swallowed: a silent success
    // here would be worse than the refusal.
    assert!(
        err.contains("could not reach") || err.contains("peer join"),
        "the enrolment failure has to surface: {err}"
    );
}

/// **`tcr peer network-key set` prints the key ONCE, and nothing prints it
/// again.**
///
/// A key an operator can re-read from a terminal is a key in every scrollback
/// buffer on the Mac, so `show` reports presence and never the value.
///
/// Watched red: change the `Show` arm to print `key.to_paste_string()` and this
/// fails on the `show` assertion.
#[test]
fn the_network_key_is_printed_once_and_never_again() {
    let dir = scratch("netkey-cli");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    let (out, err, ok) = run_tcr(&peers, &["network-key", "set"]);
    assert!(ok, "`tcr peer network-key set` must exit 0: {err}");
    let printed = out
        .lines()
        .next()
        .expect("the key is the first line")
        .trim()
        .to_string();
    assert_eq!(
        printed.chars().count(),
        52,
        "the minted key must be printed as its 52-character paste string: {out}"
    );

    // It is on disk, at 0600.
    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key.map(|key| key.to_paste_string()).as_deref(),
        Some(printed.as_str()),
        "what was printed must be what was stored"
    );
    let mode = std::fs::metadata(&peers)
        .expect("stat the peers file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "a shared secret lives at 0600, not {mode:o}");

    // And `show` says only that it is set.
    let (out, _err, ok) = run_tcr(&peers, &["network-key", "show"]);
    assert!(ok);
    assert!(out.contains("set"), "{out}");
    assert!(
        !out.contains(&printed),
        "`show` must never print the key again: {out}"
    );

    // `link` carries it, and the link round-trips to the same key.
    let (out, err, ok) = run_tcr(&peers, &["link"]);
    assert!(ok, "`tcr peer link` must exit 0: {err}");
    let link = out
        .lines()
        .next()
        .expect("the link is the first line")
        .trim();
    let parsed = pair::ShareLink::parse(link).expect("the printed link parses");
    assert_eq!(parsed.network_key.to_paste_string(), printed);
    assert!(
        parsed.join.is_none(),
        "without --invite a link carries the network key alone"
    );

    // `clear` removes it, and `link` then refuses rather than printing a link
    // with nothing in it.
    let (out, _err, ok) = run_tcr(&peers, &["network-key", "clear"]);
    assert!(ok);
    assert!(out.contains("cleared"), "{out}");
    let (_out, err, ok) = run_tcr(&peers, &["link"]);
    assert!(!ok, "a link with no network key must be refused");
    assert!(
        err.contains("network-key set"),
        "the refusal has to say what to run: {err}"
    );
}

/// **`tcr peer network-key set` over an existing key requires `--replace`,
/// and the refusal names what a replace cuts off.**
///
/// The verb lives in `main.rs`, applied elsewhere, so this test is the ask,
/// not a claim the production body is fixed: it is red until the patch
/// `run_peer_network_key`'s `Set` arm needs (a `--replace` flag on
/// `PeerNetworkKeyArgs`, checked when `file.network_key` is already
/// `Some`).
///
/// Watched red: as of this commit, a second `set` with no `--replace`
/// silently mints and stores a new key: `ok` is `true` and the printed key
/// changes: so the first two assertions below fail against the unpatched
/// binary.
#[test]
fn network_key_set_over_an_existing_key_requires_replace() {
    let dir = scratch("netkey-replace");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    let (first, _err, ok) = run_tcr(&peers, &["network-key", "set"]);
    assert!(ok, "the first `set` must exit 0: {_err}");
    let first_key = first
        .lines()
        .next()
        .expect("the key is the first line")
        .trim()
        .to_string();

    // A second `set`, no `--replace`: refused, and the stored key is
    // untouched: every Mac still holding it must still be able to knock.
    let (_out, err, ok) = run_tcr(&peers, &["network-key", "set"]);
    assert!(
        !ok,
        "a second `set` with no `--replace` must be refused, not silently mint a new key"
    );
    assert!(
        err.contains("--replace"),
        "the refusal must name the flag that unblocks it: {err}"
    );
    assert!(
        err.contains("OLD key") || err.contains("old key") || err.contains("cuts off"),
        "the refusal must say what a replace cuts off: every Mac still on the old key: {err}"
    );
    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key.map(|key| key.to_paste_string()).as_deref(),
        Some(first_key.as_str()),
        "a refused `set` must not have touched the stored key"
    );

    // With `--replace`: it mints a new one and says so.
    let (second, err, ok) = run_tcr(&peers, &["network-key", "set", "--replace"]);
    assert!(ok, "`set --replace` must exit 0: {err}");
    let second_key = second
        .lines()
        .next()
        .expect("the key is the first line")
        .trim()
        .to_string();
    assert_ne!(
        second_key, first_key,
        "`--replace` must mint a genuinely new key, not repeat the old one"
    );
    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key.map(|key| key.to_paste_string()).as_deref(),
        Some(second_key.as_str()),
        "the replaced key must be what is stored"
    );
}

// ---------------------------------------------------------------------------
// Both tests below are asks to LEASE-WIRE
// (`listener.rs` and `state.rs` are their files), and these two
// tests are the red-first proof handed over with the patch. Neither test
// edits `listener.rs` or `state.rs`: both drive the shipped binary and the
// shipped `PeerState` API exactly as an operator or the panel would.
// ---------------------------------------------------------------------------

/// **Item 2: a bare reservation placeholder must never be operator-visible.**
///
/// [`state::PeerState::reserve_knock_slot`] writes a placeholder row: an
/// all-zero instance id, no name, `wire_version` zero, to hold an address's
/// slot before the handshake that would earn it real details has even
/// started (`state.rs:441-473`). That row is meant to be invisible: it is
/// not a Mac that asked to pair, it is a slot held against one that might.
///
/// Today every read surface (`tcr peer pending`, `tcr peer pending --json`,
/// `tcr peer ls --json`) reads `state.pending` directly with no filter, so a
/// reservation caught between "taken" and "released or filled in": which a
/// stalled connection can hold open indefinitely today, see the next test :
/// shows up as a pending row with nothing to show: no name, address only,
/// the all-zero instance id an operator cannot use with `tcr peer accept`.
///
/// Watched red against the shipped binary; green once `main.rs`'s three read
/// paths call a filter (`PeerState::visible_pending`, in the ask below)
/// instead of touching `state.pending` directly.
#[test]
fn a_reservation_placeholder_is_filtered_from_every_read_surface() {
    let dir = scratch("placeholder-filter");
    let peers = dir.join("tcr-peers.json");
    let state_file = dir.join("peer-state.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // A reservation placeholder, written the exact way
    // `PeerState::reserve_knock_slot` writes one: no production caller in
    // `listener.rs` needs to run for this row to exist.
    let mut value = PeerState::default();
    let created = value
        .reserve_knock_slot("192.0.2.77", state::now_ms())
        .expect("a fresh reservation");
    assert!(
        created,
        "the address must be new for this to be a genuine reservation"
    );
    state::save(&state_file, &value).expect("write the state file");

    let (out, err, ok) = run_tcr(&peers, &["pending"]);
    assert!(ok, "`tcr peer pending` must exit 0: {err}");
    assert!(
        !out.contains("192.0.2.77"),
        "a bare reservation placeholder (no name learned, no instance id learned, the \
         handshake has not even started) must never be operator-visible as a pending row: \
         {out}"
    );

    let (out, err, ok) = run_tcr(&peers, &["pending", "--json"]);
    assert!(ok, "`tcr peer pending --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        parsed["pending"].as_array().map(Vec::len),
        Some(0),
        "the panel reads this exact field, and a placeholder row here is a Mac the operator \
         never actually saw try to pair: {out}"
    );

    let (out, err, ok) = run_tcr(&peers, &["ls", "--json"]);
    assert!(ok, "`tcr peer ls --json` must exit 0: {err}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        parsed["pendingCount"].as_u64(),
        Some(0),
        "the count the panel's badge is built from must not include a reservation \
         placeholder: {out}"
    );
}

/// **Item 3: a knock that stalls after message 1 must be bounded and must
/// release its reservation, not hold a real pending row open forever.**
///
/// This is the regression testing found: `serve_knock` reserves the
/// address's slot (`listener.rs:974-985`) BEFORE `run_knock_handshake`
/// (`:1061-1086`) cryptographically validates message 1, writes message 2
/// and reads the knock frame: and nothing bounds that second half. A
/// connection that delivers a well-formed message 1 and then sends nothing
/// else holds its reservation exactly as long as it likes: `read_knock`
/// blocks on a read with no timeout, `MESSAGE_1_TIMEOUT` only ever bounded
/// the read of message 1's own bytes, which already happened.
///
/// The ask to LEASE-WIRE is to move the reservation to AFTER message 1 is
/// validated (so a wrong-key or malformed message 1 never creates a
/// placeholder at all) and to wrap the rest of the handshake: message 2
/// plus the knock frame read, in a `tokio::time::timeout(MESSAGE_1_TIMEOUT,
/// ..)`, releasing the reservation on either an error or that timeout firing.
///
/// Bounded by `MESSAGE_1_TIMEOUT * 3` so this test cannot hang forever on a
/// red run: it will fail the `elapsed` assertion instead of hanging.
///
/// Watched red against the shipped binary today.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_knock_after_message_1_is_bounded_and_releases_its_reservation() {
    let node = Node::new("cap-stall");
    let addr = serve(node.context()).await;

    let started = std::time::Instant::now();
    let mut stream = TcpStream::connect(addr).await.expect("connect");
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

    // Message 1 has landed. The far side is now mid-handshake: it has
    // written or is about to write message 2, and is waiting to read the
    // knock frame this connection deliberately never sends. Every byte
    // needed to validate the caller has already arrived; the only thing
    // withheld is the frame that would complete the knock.
    let mut back = Vec::new();
    let closed = tokio::time::timeout(
        listener::MESSAGE_1_TIMEOUT * 3,
        stream.read_to_end(&mut back),
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        closed.is_ok(),
        "a knock stalled after message 1 must be closed within a bound the same size as \
         MESSAGE_1_TIMEOUT ({:?}), not held open indefinitely: still open after {elapsed:?}",
        listener::MESSAGE_1_TIMEOUT
    );
    assert!(
        elapsed < listener::MESSAGE_1_TIMEOUT * 2,
        "the close took {elapsed:?}, well past twice the {:?} bound this asks for",
        listener::MESSAGE_1_TIMEOUT
    );

    let state = node.state();
    assert!(
        state.knocks_queued() == 0,
        "a stalled knock handshake must release the reservation it took, not leave a \
         placeholder pending row behind: {state:?}"
    );
}

/// **The help an operator reads agrees with the default the code applies** :
/// the review's L4, on `--announce-name`.
///
/// The doc said "On by default: a row that says 'studio-mac' is the difference
/// between a feature and a manual." That default says the opposite and so does
/// the code: `PeerFile::announce_name` is `#[serde(default)]` on a `bool`, and
/// the beacon carries a name only when the stored flag is set. So the behaviour
/// was right and the one sentence an operator reads before choosing was wrong.
///
/// Asserted against the SHIPPED `--help`, not against the source string,
/// because the source string is what was wrong and a test reading it would have
/// agreed with it.
///
/// Watched red: put "On by default" back in `PeerFindArgs::announce_name`'s doc
/// comment.
#[test]
fn the_announce_name_help_says_it_is_off_by_default() {
    let output = tcr_command(&scratch("announce-name-help-home"))
        .args(["peer", "find", "--help"])
        .output()
        .expect("spawn tcr peer find --help");
    assert!(
        output.status.success(),
        "`tcr peer find --help` must exit 0"
    );
    let help = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        help.contains("--announce-name"),
        "the flag this is about must be in the help: {help}"
    );
    assert!(
        help.to_lowercase().contains("off by default"),
        "the help must state the default the code applies: {help}"
    );
    assert!(
        !help.to_lowercase().contains("on by default"),
        "and must not still claim the opposite: {help}"
    );

    // The code's own answer, so this test is about a DISAGREEMENT and not
    // merely about a string: a fresh peers file announces no name.
    let fresh = PeerFile::default();
    assert!(
        !fresh.announce_name,
        "a file nobody has tuned announces no name, which is what the help now says"
    );
}
