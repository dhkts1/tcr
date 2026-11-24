// ---------------------------------------------------------------------------
// The CLI verbs that used to overwrite, or to say ok and store nothing
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// Run the `tcr` this build produced, against one Mac's files.
fn tcr(args: &[&str]) -> std::process::Output {
    let peers = args
        .windows(2)
        .find(|pair| pair[0] == "--peers")
        .map(|pair| std::path::Path::new(pair[1]))
        .expect("every peer command must name its fixture's peers file");
    tcr_command(
        &peers
            .parent()
            .expect("the peers fixture has a directory")
            .join("cli-home"),
    )
    .args(args)
    .output()
    .expect("the tcr this build produced runs")
}

/// **A join link carrying a network key does not replace one this Mac already
/// has, unless the operator says so.**
///
/// `tcr peer network-key join` has refused this since it shipped, and
/// `docs/peers.md` promises the refusal, but `tcr peer join` took the same key
/// by a different door and overwrote in silence: one clicked `tcr://peer/join`
/// link was enough to cut a Mac off from every Mac still holding the old key.
///
/// Watch it fail by removing the `replaced && !a.replace` refusal in the Join
/// arm: the first run below exits 0 and the second assertion, that the old key
/// is still on disk, goes red.
#[test]
fn a_join_link_does_not_replace_a_network_key_without_replace() {
    let theirs = Node::new("join-link-source");
    let mine = Node::new("join-link-target");

    // The other Mac's link: its own network key, no invite.
    let minted = tcr(&[
        "peer",
        "network-key",
        "set",
        "--peers",
        theirs.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(minted.status.success(), "the other Mac mints a key");
    let linked = tcr(&[
        "peer",
        "link",
        "--peers",
        theirs.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(linked.status.success(), "the other Mac prints a link");
    let link = String::from_utf8_lossy(&linked.stdout)
        .lines()
        .next()
        .expect("the link is the first line")
        .to_string();

    // This Mac already has one.
    assert!(
        tcr(&[
            "peer",
            "network-key",
            "set",
            "--peers",
            mine.peers.to_str().expect("a utf-8 path"),
        ])
        .status
        .success()
    );
    let before = mine.file().network_key.expect("this Mac has a key");

    let refused = tcr(&[
        "peer",
        "join",
        &link,
        "--peers",
        mine.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(
        !refused.status.success(),
        "a link that would replace the network key is refused: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let text = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(
        text.contains("--replace"),
        "the refusal names the flag that means it: {text}"
    );
    assert_eq!(
        mine.file().network_key.expect("the key is still there"),
        before,
        "a refusal changes nothing on disk"
    );

    // And with the flag, it is taken: the refusal is a gate, not a wall.
    let taken = tcr(&[
        "peer",
        "join",
        &link,
        "--replace",
        "--peers",
        mine.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(
        taken.status.success(),
        "--replace means it: {}",
        String::from_utf8_lossy(&taken.stderr)
    );
    assert_ne!(
        mine.file().network_key.expect("a key is set"),
        before,
        "the link's key replaced this Mac's own"
    );
}

/// **A join link carrying BOTH a network key and a join key still pairs when
/// the network key alone is refused.**
///
/// Before this fix, the network-key refusal `anyhow::bail!`ed the whole
/// `Join` arm, so a link with both halves paired NOTHING when this Mac
/// already held a different network key: the join token was never even
/// read.
///
/// The join token here points at a port nothing is listening on, so the join
/// half fails as loudly as it can; this test is about what still RAN before
/// that failure, not about a successful pairing.
///
/// Watched red: restore `anyhow::bail!` where `network_key_refusal =
/// Some(...)` now sits, and the CLI's stderr goes back to the refusal alone,
/// never reaching `pair::join`'s own "could not reach". The assertion below
/// on that string is what catches it.
#[test]
fn a_join_link_with_both_keys_still_reads_the_join_token_when_the_network_key_is_refused() {
    let dir = scratch("join-link-both-keys");
    let peers = dir.join("tcr-peers.json");
    config::save(&peers, &PeerFile::default()).expect("write the peers file");

    // This Mac already has a network key.
    let (_out, _err, ok) = run_tcr(&peers, &["network-key", "set"]);
    assert!(ok, "this Mac mints its own key first");
    let before = config::read_or_default(&peers)
        .expect("the peers file reads")
        .network_key
        .expect("this Mac has a key");

    // A link from elsewhere, carrying a DIFFERENT network key and a join
    // token that points nowhere.
    let other_key = NetworkKey::from_bytes([0x77; 32]);
    let link = pair::ShareLink {
        network_key: other_key,
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
        "the join half must fail (nothing is listening on that port): {out}{err}"
    );
    assert!(
        out.contains("a network key is already set on this Mac"),
        "the network-key refusal is still reported, on stdout not a bail: {out}"
    );
    assert!(
        err.contains("nothing answered at any address this key carries"),
        "the join token must have been READ and tried, so the failure is \
         pair::join's own connect error, not the earlier network-key bail: {err}"
    );

    let file = config::read_or_default(&peers).expect("the peers file reads");
    assert_eq!(
        file.network_key,
        Some(before),
        "a refused network key must not land on disk"
    );
}

/// **`tcr peer share on` keeps the mode and the schedule of the grant it is
/// replacing, and `--fraction 0` removes rather than grants.**
///
/// It used to build a fresh `LendGrant::new` per row: mode serve, no end, no
/// daily window. So an operator who had lent one Mac a BEARER (`--mode hand`,
/// ending at 18:00, weekdays only) and then adjusted the shared fraction got a
/// serve grant with no end and no schedule, and nothing said so. `peer lend`
/// already carries that rule and explains it; this verb now does too.
///
/// Watch it fail by dropping the `replacing` block in the Share arm: `mode`
/// reads `serve` and `until` reads null.
#[test]
fn share_on_keeps_the_mode_and_the_window_of_the_grant_it_replaces() {
    use tcr::peer::config::{LendGrant, LendMode};

    let mac = Node::new("share-preserves-mode");
    let mut file = mac.file();
    let mut granted = LendGrant::new(tcr_peer_wire::Window::SevenDay, 0.1, 600, 1);
    granted.mode = LendMode::Hand;
    granted.until = Some(4_102_444_800);
    granted.between = Some("09:00-18:00".parse().expect("a daily window"));
    granted.ensure_id().expect("an id");
    file.peers
        .push(pinned_row(PeerId([0x31; 32]), "studio-mac", granted));
    mac.write_file(&file);

    let shared = tcr(&[
        "peer",
        "share",
        "on",
        "--window",
        "7d",
        "--fraction",
        "0.25",
        "--peers",
        mac.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(
        shared.status.success(),
        "share on: {}",
        String::from_utf8_lossy(&shared.stderr)
    );

    let after = mac.file();
    let grant = after.peers[0]
        .lend
        .first()
        .expect("the row still has its grant")
        .clone();
    assert_eq!(
        grant.mode,
        LendMode::Hand,
        "a hand grant must not become a serve grant because the fraction moved: {grant:?}"
    );
    assert_eq!(
        grant.until,
        Some(4_102_444_800),
        "the end the operator set survives: {grant:?}"
    );
    assert!(
        grant.between.is_some(),
        "and so does the daily window: {grant:?}"
    );
    assert!(
        (grant.fraction - 0.25).abs() < f64::EPSILON,
        "the fraction is the thing that DID change: {grant:?}"
    );

    // Zero is a removal, the rule `peer lend` already follows.
    let removed = tcr(&[
        "peer",
        "share",
        "on",
        "--window",
        "7d",
        "--fraction",
        "0",
        "--peers",
        mac.peers.to_str().expect("a utf-8 path"),
    ]);
    assert!(
        removed.status.success(),
        "share on --fraction 0: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        mac.file().peers[0].lend.is_empty(),
        "--fraction 0 removes the grant rather than lending nothing: {:?}",
        mac.file().peers[0].lend
    );
}

/// One pinned row with one grant, for the CLI tests above.
fn pinned_row(node: PeerId, label: &str, grant: config::LendGrant) -> config::PeerRow {
    config::PeerRow {
        node,
        label: label.to_string(),
        endpoints: Vec::new(),
        added_at: 1,
        allow: Default::default(),
        lend: vec![grant],
        rendezvous_secret: None,
        sees_us_at: None,
    }
}

/// **A block or an ignore whose target is neither a pending request nor an
/// address is an error, not an ok.**
///
/// Both verbs used to fall back to the raw argv. The listener compares against
/// the bare IP a connection arrived on, so `tcr peer block laptop` recorded a
/// ban nothing could ever equal and printed ok: the operator believed a Mac was
/// blocked and it was not.
///
/// Watch it fail by restoring the `None => a.target.clone()` arm: the command
/// exits 0 and the ban lands under the name.
#[test]
fn block_and_ignore_refuse_a_target_that_is_neither_a_request_nor_an_address() {
    let mac = Node::new("block-selector");
    let peers = mac.peers.to_str().expect("a utf-8 path").to_string();

    for verb in ["block", "ignore"] {
        let refused = tcr(&["peer", verb, "studio-mac", "--peers", &peers]);
        assert!(
            !refused.status.success(),
            "`tcr peer {verb} studio-mac` must not report success: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let text = String::from_utf8_lossy(&refused.stderr).to_string();
        assert!(
            text.contains("is not an address"),
            "the refusal says what the target had to be: {text}"
        );
    }
    let state = mac.state();
    assert!(
        state.banned.is_empty() && state.muted.is_empty(),
        "a refusal writes nothing: {state:?}"
    );

    // The control: a bare IP with no pending row is still allowed, because
    // "make this stop" is a reasonable thing to type at a row that expired.
    let accepted = tcr(&["peer", "block", "192.0.2.51", "--peers", &peers]);
    assert!(
        accepted.status.success(),
        "an address is still accepted: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert!(
        mac.state()
            .banned
            .iter()
            .any(|ban| ban.addr == "192.0.2.51"),
        "and it lands under the address the listener compares"
    );
}

/// **A reservation placeholder is not something `accept` can open a window
/// for.**
///
/// The placeholder holds the pending cap for a connection that has said
/// nothing yet; it carries an all-zero instance id and every screen already
/// hides it (`PeerState::visible_pending`). `find_pending` matched it anyway,
/// so `tcr peer accept <address>` against a stalled connection opened a
/// 120-second window keyed to an id no real pairing can present, and told the
/// operator that Mac was admitted.
///
/// Watch it fail by dropping the `is_reservation_placeholder` filter in
/// `find_pending`.
#[test]
fn a_reservation_placeholder_is_never_selected() {
    let mut state = PeerState::default();
    let reserved_at = 1_000;
    state
        .reserve_knock_slot("192.0.2.9", reserved_at)
        .expect("the slot is reserved");
    assert!(
        state.find_pending("192.0.2.9").is_none(),
        "a placeholder is not a pairing request: {:?}",
        state
    );
    assert!(
        state.accept_knock("192.0.2.9", reserved_at, 120).is_none(),
        "and no window opens for one: {:?}",
        state
    );
    assert!(
        !state.holds_accepted_row("192.0.2.9"),
        "nothing was admitted: {:?}",
        state
    );
    assert_eq!(
        state.knocks_queued(),
        1,
        "the reservation still holds its slot, which is what it is for"
    );
}

/// **A static key a pairing revealed outlives the two-minute window, so a
/// block afterwards bans the key as well as the address.**
///
/// The key is written onto the accepted window and nowhere else, and the window
/// was dropped the moment it expired. An operator blocking a Mac after the
/// digits failed to match, which is the ordinary reason to block, recorded an
/// address-only ban and read "no handshake ever revealed a static key". A new
/// DHCP lease walked past it.
///
/// Watch it fail by restoring the plain deadline test in `PeerState::expire`.
#[test]
fn a_learned_key_survives_the_window_it_was_learned_in() {
    let mut state = PeerState::default();
    let learned = PeerId([0x44; 32]);
    state.open_accepted_window(state::AcceptedInstance {
        instance_id: InstanceId([0x01; INSTANCE_ID_BYTES]),
        addr: "192.0.2.30".to_string(),
        until_ms: 2_000,
        opened_at_ms: 1_000,
        learned_key: Some(learned),
    });
    // A window with no key at all: it goes, which is what keeps the kept rows
    // from being "expire stopped working".
    state.open_accepted_window(state::AcceptedInstance {
        instance_id: InstanceId([0x02; INSTANCE_ID_BYTES]),
        addr: "192.0.2.31".to_string(),
        until_ms: 2_000,
        opened_at_ms: 1_000,
        learned_key: None,
    });

    let after_the_window = 900_000;
    state.expire(after_the_window);

    assert_eq!(
        state.key_learned_at("192.0.2.30"),
        Some(learned),
        "the key that address revealed is still readable: {:?}",
        state
    );
    assert_eq!(
        state.key_learned_at("192.0.2.31"),
        None,
        "and a window that learned nothing left nothing behind"
    );
    assert!(
        !state.accepted_window(
            "192.0.2.30",
            &InstanceId([0x01; INSTANCE_ID_BYTES]),
            after_the_window
        ),
        "the kept row authorizes nothing: it is a record, not a window"
    );

    state.ban(
        "192.0.2.30",
        state.key_learned_at("192.0.2.30"),
        BanReason::Blocked,
        after_the_window,
    );
    assert!(
        state.is_key_banned(&learned),
        "so the block bans the key as well as the address: {:?}",
        state.banned
    );
}

/// **An open pairing window does not cost a learned key.**
///
/// A closed accepted window is kept for the static key its handshake revealed,
/// which is the half of a ban that survives a new DHCP lease, and the list is
/// bounded at [`state::MAX_LEARNED_KEYS`]. The bound was counted in CLOSED rows
/// and the eviction in the length of the WHOLE list, open windows included, so
/// every window still open evicted one extra key: the oldest went early, and an
/// operator who blocked that Mac afterwards got an address-only ban against a
/// key this node had really learned.
///
/// Measured at the bound, with exactly one row over it and one window open, so
/// the arithmetic is the only thing that can decide the count.
///
/// Watch it fail by putting `self.accepted.len()` back in `PeerState::expire`'s
/// `take(..)`: two closed rows go instead of one and the oldest key is gone.
#[test]
fn an_open_window_does_not_evict_an_extra_learned_key() {
    let now = 10_000_000_i64;
    let mut value = PeerState::default();

    // One over the bound, every one closed and every one carrying a key. The
    // oldest is first, so "the oldest went" is visible by name.
    for nth in 0..=state::MAX_LEARNED_KEYS {
        let byte = u8::try_from(nth).expect("the bound is well under 255");
        value.open_accepted_window(state::AcceptedInstance {
            instance_id: InstanceId([byte; INSTANCE_ID_BYTES]),
            addr: format!("10.0.0.{byte}"),
            opened_at_ms: now - 600_000 + i64::from(byte),
            until_ms: now - 300_000,
            learned_key: Some(PeerId([byte; 32])),
        });
    }
    // And one window still OPEN, which is what used to cost a key.
    value.open_accepted_window(state::AcceptedInstance {
        instance_id: InstanceId([200_u8; INSTANCE_ID_BYTES]),
        addr: "10.0.0.200".to_string(),
        opened_at_ms: now - 1_000,
        until_ms: now + 60_000,
        learned_key: None,
    });

    value.expire(now);

    let kept_keys = (0..=state::MAX_LEARNED_KEYS)
        .filter(|nth| value.key_learned_at(&format!("10.0.0.{nth}")).is_some())
        .count();
    assert_eq!(
        kept_keys,
        state::MAX_LEARNED_KEYS,
        "one row over the bound drops exactly one row, whatever else is open: {value:?}"
    );
    assert!(
        value.key_learned_at("10.0.0.1").is_some(),
        "and the second-oldest key is still there: only the oldest was over the bound"
    );
    assert!(
        value.key_learned_at("10.0.0.0").is_none(),
        "while the oldest is the one that went, which is the rule the bound states"
    );
    assert!(
        value.holds_accepted_row("10.0.0.200"),
        "the open window is untouched: it is not a learned key and never was"
    );
}

// ---------------------------------------------------------------------------
// revoke and enrolment race on the same invite
// ---------------------------------------------------------------------------

/// **A concurrent revoke and enrolment never resurrect the invite or drop the
/// pinned row.**
///
/// `revoke_invite` and `mint_invite_as` read-modify-write the peers file with
/// no lock, while `accept_enrolment` holds `FileLock` across its own
/// read-modify-write. Unlocked, a revoke that reads before and saves after
/// `accept_enrolment`'s own save clobbers it with a stale copy: either the
/// revoked invite comes back (the stale copy still has it) or the row
/// `accept_enrolment` just pinned disappears (the stale copy predates it).
///
/// Watched red: remove the `FileLock::acquire` line this test's production
/// change adds to `revoke_invite`, and this fails inside the first few trials
/// with either "the invite came back" or "`accept_enrolment` said Ok but the row
/// is gone".
#[test]
fn a_concurrent_revoke_and_enrolment_never_resurrect_or_drop_a_row() {
    const TRIALS: usize = 20;

    for trial in 0..TRIALS {
        let node = Node::new(&format!("revoke-race-{trial}"));
        let mut file = node.file();
        file.listen = Some("127.0.0.1:9600".parse().expect("a loopback address"));
        node.write_file(&file);

        let store = config::PeerStore::open(&node.peers).expect("open the store");
        let (invite, token) = pair::mint_invite_as(&store, &node.key, "laptop-2", 600, 1, None)
            .expect("mint a one-use invite");

        let joiner = PeerId([0x33; 32]);
        let enroll = tcr_peer_wire::Enroll {
            invite_id: 0,
            label: "laptop-2".to_string(),
        };

        let peers_path = node.peers.clone();
        let now = pair::now_ms();
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let revoke_handle = {
            let store = config::PeerStore::open(&node.peers).expect("open the store");
            let barrier = Arc::clone(&barrier);
            let id = invite.id;
            std::thread::spawn(move || {
                barrier.wait();
                pair::revoke_invite(&store, id)
            })
        };
        let enrol_handle = {
            let secret = token.secret;
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
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
        };

        let revoked = revoke_handle.join().expect("the revoking thread");
        let enrolled = enrol_handle.join().expect("the enrolling thread");

        let after = node.file();
        assert!(
            after.invite(invite.id).is_none(),
            "trial {trial}: the invite came back after a concurrent revoke and enrolment \
             raced (revoked={revoked:?}), as {:?}",
            after.invite(invite.id)
        );

        if let Ok(row) = &enrolled {
            assert!(
                after.peers.iter().any(|existing| existing.node == row.node),
                "trial {trial}: accept_enrolment said Ok but the pinned row for {:?} is gone \
                 from disk after the race with revoke_invite (revoked={revoked:?})",
                row.node
            );
        }

        std::fs::remove_dir_all(&node.dir).log_debug("removing a test directory");
    }
}
