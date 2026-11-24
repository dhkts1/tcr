//! Enrolment and the files it writes: the pairing window, the join token, the
//! refusal log, the pinned rows an enrolment leaves, and the one safe writer and
//! lock for each file.

use super::*;

// ---------------------------------------------------------------------------
// The pairing window: retired here, moved to per-instance-id approval
// ---------------------------------------------------------------------------
//
// This section used to hold two tests keyed on `listener::accept_peer_session`
// and the node-wide `PairingWindow` it gated `Handshake::Pair` with: a
// stranger refused outside the window, and the SAME dial answered inside it.
// That node-wide window was replaced with a window keyed to one
// accepted instance id at one
// address (`PeerState::accepted_window`, `listener::accept_pairing_or_return`)
// specifically because the old one answered ANY stranger who reached the port
// while any pairing was in progress. `accept_peer_session` was the pre-
// decision-10 seam that still exercised the retired gate; it is deleted from
// pre-existing seam that still exercised the retired gate; it is deleted from
// `listener.rs` as of this commit, and the two tests that measured it go with it rather than being
// converted to `noise::accept_handshake`, which has no window parameter to
// convert them to. The SAME invariant, under the CURRENT gate, is
// `tests/peer_pairing.rs::xx_from_an_unaccepted_instance_gets_zero_bytes` (the
// zero-bytes refusal) and its own positive control (the same dial answered
// after Accept); `the_six_digit_code_binds_the_handshake_not_the_keys` above
// keeps the six-digit-code assertion these two also made, through
// `noise::accept_handshake` directly.

/// The window is a deadline on the wall clock, not a flag: it opens, it is open,
/// and it is closed again one second past [`pair::PAIRING_WINDOW_SECS`] with
/// nobody having to remember to close it.
#[test]
fn a_pairing_window_opens_and_then_expires_on_its_own() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");

    assert_eq!(
        pair::pairing_window(&state, 1_000).expect("a missing state file reads as closed"),
        PairingWindow::Closed,
        "a node that never paired has no window, which is the same answer as a closed one"
    );

    let until = pair::open_pairing_window(&state, 1_000).expect("the window opens");
    assert_eq!(until, 1_000 + pair::PAIRING_WINDOW_SECS * 1_000);
    assert_eq!(
        pair::pairing_window(&state, 1_000).expect("the window reads"),
        PairingWindow::Open
    );
    assert_eq!(
        pair::pairing_window(&state, until - 1).expect("the window reads"),
        PairingWindow::Open
    );
    assert_eq!(
        pair::pairing_window(&state, until).expect("the window reads"),
        PairingWindow::Closed,
        "the deadline is absolute and exclusive: a clock that moved is not a second chance"
    );

    pair::open_pairing_window(&state, until).expect("it can be opened again");
    pair::close_pairing_window(&state).expect("and closed by hand");
    assert_eq!(
        pair::pairing_window(&state, until).expect("the window reads"),
        PairingWindow::Closed
    );
}

/// Opening the window rewrites `peer-state.json`, which belongs to
/// `crate::peer::state`: so every other key in it has to survive the write.
///
/// Watch it fail by dropping the `#[serde(flatten)] rest` field from
/// `crate::peer::state::PeerState`: the key a later phase wrote vanishes, and
/// an older build's two-minute deadline silently truncates a file a newer one
/// is keeping state in. (The deadline moved into `PeerState` as a
/// typed field (see `the_pairing_window_survives_a_save_and_a_load`), so the
/// flattened remainder that keeps this test green lives there now, not in a
/// local document in `pair.rs`.)
///
/// The seed file goes in through `state::save` and not `std::fs::write`,
/// because the two writers do not produce the same file: `save` publishes at
/// mode 0600 and a bare `fs::write` lands at this box's umask, which
/// `state::load`'s mode tripwire quarantines. That is the tripwire working, so
/// the fix is the writer this program actually uses: see
/// `a_state_file_a_wider_mode_is_quarantined_and_the_saved_one_is_0600`, which
/// measures both halves of that contract.
#[test]
fn opening_the_window_keeps_every_other_key_in_the_state_file() {
    use std::os::unix::fs::PermissionsExt as _;
    use tcr::peer::state;

    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    let seeded: state::PeerState =
        serde_json::from_value(serde_json::json!({"somethingPhaseFourAdds": {"keep": "me"}}))
            .expect("a state with one key this build does not know");
    state::save(&state, &seeded).expect("a state file written by another phase");

    pair::open_pairing_window(&state, 5_000).expect("the window opens");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).expect("the file reads"))
            .expect("it is still JSON");
    assert_eq!(after["version"], 1, "the format version must survive");
    assert_eq!(
        after["somethingPhaseFourAdds"]["keep"], "me",
        "a key this writer does not know about must survive it"
    );
    assert_eq!(
        after["pairingWindowUntilMs"],
        5_000 + pair::PAIRING_WINDOW_SECS * 1_000
    );
    let mode = std::fs::metadata(&state)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "a read/modify/write of this file republishes it at 0600, not {mode:o}: \
         otherwise the next `load` quarantines what this one just wrote"
    );
}

/// The state file's own mode contract, both halves: `state::save` creates it
/// 0600, and `state::load` refuses to trust any wider mode: it renames the
/// file aside and answers with an empty state rather than reading a file this
/// program did not write.
///
/// The file carries `accepted` (the whole authorization for a first pairing)
/// and `banned` (the block list), so a mode anyone on the box could have
/// written is not an input to either decision.
///
/// Watched red: with the `if mode != 0o600` branch in `crate::peer::state::load`
/// deleted, the widened file is read back and `loaded.banned` is 1, not 0, and
/// nothing is quarantined.
#[test]
fn a_state_file_a_wider_mode_is_quarantined_and_the_saved_one_is_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    use tcr::peer::state;

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");

    let mut value = state::PeerState::default();
    value.banned.push(state::Ban {
        addr: state::knock_address(&"127.0.0.1:9600".parse().expect("an addr")),
        key: None,
        since_ms: 1_000,
        reason: state::BanReason::Blocked,
    });
    state::save(&path, &value).expect("the state file writes");

    let mode = std::fs::metadata(&path)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the one writer creates this file 0600, not {mode:o}"
    );
    assert_eq!(
        state::load(&path, 2_000)
            .expect("a 0600 file loads")
            .banned
            .len(),
        1,
        "a file at the mode the writer produces is trusted"
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("widen it by hand");
    let loaded = state::load(&path, 2_000).expect("a widened file still boots the node");
    assert!(
        loaded.banned.is_empty(),
        "a widened state file decides nothing: the node starts cold"
    );
    assert!(
        !path.exists(),
        "and the file is renamed aside rather than left in place to be read again"
    );
    let quarantined: Vec<_> = std::fs::read_dir(dir.path())
        .expect("the dir reads")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".corrupt-"))
        .collect();
    assert_eq!(
        quarantined.len(),
        1,
        "the evidence survives the cold start: {quarantined:?}"
    );
}

/// A state file that has to be created carries the version
/// `crate::peer::state::load` expects, or that loader renames it aside as
/// untrusted and the node starts cold for no reason at all.
#[test]
fn a_state_file_this_writer_creates_carries_the_format_version() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    pair::open_pairing_window(&state, 5_000).expect("the window opens");

    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).expect("the file reads")).expect("JSON");
    assert_eq!(
        written["version"],
        serde_json::json!(tcr::peer::state::FORMAT_VERSION),
        "a version-less file is one `state::load` quarantines"
    );

    let loaded = tcr::peer::state::load(&state, 5_000)
        .expect("the state loader reads a file the pairing window created");
    assert!(
        loaded.leases.is_empty(),
        "and it reads as an empty state rather than a quarantined one"
    );
}

// ---------------------------------------------------------------------------
// The join token
// ---------------------------------------------------------------------------

/// The token round-trips through a paste buffer: it is the one string in this
/// design an operator moves between two machines by hand, so a field that
/// survives rendering and not parsing is a wrong-key dial.
#[test]
fn a_join_token_round_trips() {
    for addr in ["127.0.0.1:9600", "[::1]:9600"] {
        let token = pair::JoinToken::new(
            vec![addr.parse().expect("a test address")],
            PeerId([3_u8; 32]),
            [7_u8; 32],
        );
        let rendered = token.to_token();
        assert!(
            rendered.starts_with(pair::KEY_PREFIX),
            "an operator has to be able to see what they are pasting: {rendered}"
        );
        assert_eq!(
            pair::JoinToken::parse(&rendered).expect("the token parses"),
            token,
            "the address {addr} did not survive the round trip"
        );
        assert_eq!(
            pair::JoinToken::parse(&format!("  {rendered}\n")).expect("a pasted token parses"),
            token,
            "a paste carries whitespace"
        );
    }
}

/// An unknown version is refused rather than guessed at, because the guess is a
/// silent dial to the wrong key: and a token that carries no secret at all is
/// refused too.
/// Watch it fail by letting `JoinToken::parse` fall through to the body when
/// the prefix does not match: every other shape below is refused on some other
/// ground, so only a token that is well-formed in every field EXCEPT its
/// version measures the version check.
#[test]
fn a_join_token_with_an_unknown_version_is_refused() {
    let good = pair::JoinToken::new(
        vec!["127.0.0.1:9600".parse().expect("a test address")],
        PeerId([3_u8; 32]),
        [7_u8; 32],
    )
    .to_token();
    // v1, v2 and v3 are all read now, so the fixture is the next version
    // along: the check is that an unknown one is refused for BEING unknown.
    // (It used to say v3 here; moved when `JoinToken::parse` grew a v3 arm,
    // the change this test's own doc comment says to watch for.)
    let unknown = good.replace("tcr-join:v2:", "tcr-join:v4:");
    assert_ne!(
        good, unknown,
        "the fixture has to differ from the good token"
    );
    let error = pair::JoinToken::parse(&unknown)
        .expect_err("a v4 token must be refused rather than read as one of the three known ones");
    assert!(
        format!("{error:#}").contains("not a join key"),
        "the refusal has to be about the VERSION: a token whose only fault is its version \
         must not be refused by accident, further in, on a field that happened to move: \
         {error:#}"
    );

    for bad in [
        "tcr-join:v2:127.0.0.1:9600:AAAA:BBBB",
        "127.0.0.1:9600",
        "tcr-join:v1:127.0.0.1:9600",
        "tcr-join:v1:not-an-address:0000000000000000000000000000000000000000000000000000:0000000000000000000000000000000000000000000000000000",
    ] {
        let refused = pair::JoinToken::parse(bad);
        assert!(
            refused.is_err(),
            "{bad:?} parsed into a token, which means `tcr peer join` would dial something \
             nobody typed"
        );
    }
}

// ---------------------------------------------------------------------------
// The log a stranger can write
// ---------------------------------------------------------------------------

/// A refused connection writes ONE log line PER ADDRESS PER HOUR, and then the
/// listener is silent to that address: with the count of what was silenced
/// folded into the next line, so the bound never turns a flood into an absence
/// of evidence.
///
/// The grain changed: this used to be one line per MINUTE for the whole
/// listener, which was wrong in both directions, one flooder silenced every
/// other address's first refusal, and a slow scan from a `/24` still wrote 254
/// lines a minute. `abuse-resistance.md` asks for one line per address per
/// hour, and the second half of this test is what makes "per address" real.
///
/// Watch it fail by returning `Some(0)` unconditionally from
/// `RefusalLog::admit`: a port scan is thousands of connections, and a line each
/// is a disk-filling primitive handed to anyone who can reach the port.
#[test]
fn an_unauthenticated_flood_writes_one_log_line_and_then_counts() {
    let mut log = listener::RefusalLog::new();
    assert_eq!(
        log.admit("192.0.2.7", 0),
        Some(0),
        "the first refusal is always logged, and nothing was suppressed before it"
    );
    for step in 1..500 {
        assert_eq!(
            log.admit("192.0.2.7", step),
            None,
            "a second line inside the quiet period is the flood this bounds"
        );
    }
    assert_eq!(
        log.admit("192.0.2.7", listener::REFUSAL_LOG_QUIET_MS),
        Some(499),
        "the line that ends the quiet period carries the count of what it stood for"
    );
    assert_eq!(
        log.admit("192.0.2.7", listener::REFUSAL_LOG_QUIET_MS + 1),
        None,
        "and the quiet period starts again from the line that was emitted"
    );
}

/// **One flooding address must not silence another address's first line.**
///
/// This is the half a global throttle cannot hold, and it is the half that
/// matters operationally: the machine you need to hear about is the one that
/// knocked once while somebody else was hammering the port.
///
/// Watched red: with `RefusalLog` reverted to a single `last_ms`/`suppressed`
/// pair for the whole listener, the second assertion here returns `None` and
/// this fails with "the quiet period for one address must not silence
/// another".
#[test]
fn one_flooding_address_does_not_silence_another() {
    let mut log = listener::RefusalLog::new();
    for step in 0..500 {
        log.admit("192.0.2.7", step);
    }
    assert_eq!(
        log.admit("192.0.2.8", 500),
        Some(0),
        "the quiet period for one address must not silence another: a Mac that knocked once          during somebody else's flood is the line an operator needs"
    );
    // And the flooder is still silenced, so the bound did not simply stop
    // working.
    assert_eq!(
        log.admit("192.0.2.7", 501),
        None,
        "the flooding address is still inside its own quiet hour"
    );
}

/// The refusal log is bounded in ADDRESSES too, not only in time.
///
/// A map keyed on something a stranger chooses is a map a stranger can grow: a
/// `/16` of source addresses is 65 536 entries at one refusal each, all inside
/// one quiet hour, so time alone does not bound it.
///
/// Watched red by deleting the `while … >= REFUSAL_LOG_ADDRESSES` eviction
/// loop: the length comes back as 2 048.
#[test]
fn the_refusal_log_is_bounded_in_addresses() {
    let mut log = listener::RefusalLog::new();
    for n in 0..2_048_u32 {
        log.admit(&format!("10.0.{}.{}", n / 256, n % 256), i64::from(n));
    }
    assert!(
        log.len() <= listener::REFUSAL_LOG_ADDRESSES,
        "the refusal log grew to {} addresses, above the {} cap: a map keyed on a value a \
         stranger picks has to be bounded",
        log.len(),
        listener::REFUSAL_LOG_ADDRESSES
    );
    assert!(
        !log.is_empty(),
        "positive control: the log must still be remembering something, or the cap above \
         is being satisfied by a log that records nothing"
    );
}

// ---------------------------------------------------------------------------
// `tcr peer find --announce-name`, through the built binary
// ---------------------------------------------------------------------------

/// `--announce-name` is a stored preference, so it is written and the command
/// exits 0: on the `off` arm too, which is where it used to be dropped.
///
/// Runs the binary this build produced (`CARGO_BIN_EXE_tcr`), never the
/// installed one, and points the whole peer surface at a temp file with
/// `--peers`, so it reads no real config and touches no running proxy.
///
/// Watch it fail by moving the `args.announce_name` flip back inside the `on`
/// arm of `run_peer_find`: the exit code stays 0, the file is still written, and
/// `announceName` is silently whatever it already was.
#[test]
fn peer_find_off_stores_announce_name_and_exits_zero() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");

    for (flag, expected) in [("off", false), ("on", true)] {
        let output = crate::tcr_env::tcr(dir.path())
            .args([
                "peer",
                "find",
                "off",
                "--announce-name",
                flag,
                "--peers",
                peers.to_str().expect("a utf-8 temp path"),
            ])
            .output()
            .expect("the tcr this build produced runs");
        assert!(
            output.status.success(),
            "`tcr peer find off --announce-name {flag}` exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&peers).expect("the peers file was written"))
                .expect("it is JSON");
        assert_eq!(
            written["announceName"],
            serde_json::json!(expected),
            "--announce-name {flag} was accepted and then not stored"
        );
    }
}

// ---------------------------------------------------------------------------
// The pairing window is a typed field, and a moved clock closes it
// ---------------------------------------------------------------------------

/// The window survives the round trip through `peer-state.json`'s own loader
/// and writer, because it is a FIELD of that file's type now rather than a key
/// a second writer grafted on.
///
/// `state::save` used to erase it: `PeerState` had no such field, so a save
/// from anywhere else in the process dropped the deadline an operator had just
/// opened. Open, save, load, still open.
///
/// Watched red: with `#[serde(skip)]` on `PeerState::pairing_window_until_ms`,
/// the reload reads `Closed` and this test fails on the third assertion.
#[test]
fn the_pairing_window_survives_a_save_and_a_load() {
    use tcr::peer::state::{self, PairingDeadline};

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");

    let mut fresh = state::load(&path, 1_000).expect("a missing state file loads as empty");
    assert_eq!(fresh.pairing_window(1_000), PairingDeadline::Closed);

    let until = fresh.open_pairing_window(1_000, pair::PAIRING_WINDOW_SECS);
    state::save(&path, &fresh).expect("the state file is written");

    let reloaded = state::load(&path, 1_000).expect("and read back");
    assert_eq!(
        reloaded.pairing_window(1_000),
        PairingDeadline::Open { until_ms: until },
        "a save must not erase the window an operator just opened"
    );
    assert_eq!(reloaded.pairing_window_until_ms, Some(until));

    // Closing is a save too, and it has to survive the same round trip.
    let mut closing = reloaded;
    assert!(closing.close_pairing_window());
    state::save(&path, &closing).expect("the close is written");
    assert_eq!(
        state::load(&path, 1_000)
            .expect("read back")
            .pairing_window(1_000),
        PairingDeadline::Closed
    );
}

/// **A clock that moved is not a second chance, in either direction.**
///
/// The window is the interval between the instant the operator opened it and
/// its deadline, so a clock set BACKWARD reads closed rather than extending a
/// stranger's chance to make this node disclose its static key: the
/// fail-closed rule the invite TTL already follows. Asserted through the same
/// `pair::pairing_window` the listener calls, on a real file.
///
/// Watched red: with the `now_ms >= opened_at_ms` half of
/// `PeerState::pairing_window` deleted (the earlier condition, a bare
/// `until > now`), the backward-clock assertion fails: the window reads Open
/// two whole minutes before it was opened.
#[test]
fn a_clock_that_moved_closes_the_pairing_window() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");

    let opened_at = 1_000_000_i64;
    let until = pair::open_pairing_window(&path, opened_at).expect("the window opens");

    assert_eq!(
        pair::pairing_window(&path, opened_at).expect("the window reads"),
        PairingWindow::Open,
        "the instant it was opened is inside it"
    );
    assert_eq!(
        pair::pairing_window(&path, until - 1).expect("the window reads"),
        PairingWindow::Open
    );
    assert_eq!(
        pair::pairing_window(&path, until).expect("the window reads"),
        PairingWindow::Closed,
        "the deadline is absolute and exclusive"
    );
    assert_eq!(
        pair::pairing_window(&path, opened_at - 1).expect("the window reads"),
        PairingWindow::Closed,
        "a clock set backward must CLOSE the window, not extend it: outside the interval \
         the operator's own act defines, this node answers a first pairing with nothing"
    );
    assert_eq!(
        pair::pairing_window(&path, 0).expect("the window reads"),
        PairingWindow::Closed,
        "and a clock reset to the epoch is the same answer"
    );
}

// ---------------------------------------------------------------------------
// Enrolment writes a pinned row, and spends the invite
// ---------------------------------------------------------------------------

/// A peers file at mode 0600 with a listener address, through the one writer.
fn peers_file_with_listener(path: &std::path::Path, listen: std::net::SocketAddr) {
    let file = tcr::peer::config::PeerFile {
        listen: Some(listen),
        ..Default::default()
    };
    tcr::peer::config::save(path, &file).expect("the peers file is written");
}

/// `tcr peer join` used to report success while the registrar recorded
/// nothing: the joiner's `Control::Enroll` met a stream gate that wanted a
/// pinned row, and the row is the thing enrolment exists to create.
///
/// This drives BOTH halves in one process against two temp config dirs and one
/// loopback socket: the joiner runs `pair::join_as`, and the registrar's half
/// is the same five steps `src/peer/listener.rs` must run (read message 1,
/// `read_message_1_matching` for the psk that matched, `finish_responder`,
/// `accept_enrolment`, answer with `Hello`). A pinned row on both sides is the
/// gate.
///
/// Watched red twice while writing it: with `accept_enrolment` not called, the
/// registrar's file has no row and this fails on `registrar_row`; with the
/// registrar's `Hello` not sent, `join_as` returns the refusal it now produces
/// instead of `Ok` and the joiner pins nothing: which is exactly the reporting
/// bug, seen from the joiner's side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_sided_enrolment_leaves_a_pinned_row_on_both_sides() {
    use tcr::peer::config::{self as peer_config, PeerStore};
    use tcr::peer::id::NodeKey;
    use tcr::peer::listener::outstanding_secrets;
    use tcr_peer_wire::Control;

    let registrar_dir = tempfile::tempdir().expect("a temp dir");
    let joiner_dir = tempfile::tempdir().expect("a temp dir");
    let registrar_peers = registrar_dir.path().join("tcr-peers.json");
    let joiner_peers = joiner_dir.path().join("tcr-peers.json");

    let registrar_key = NodeKey::load_or_mint(registrar_dir.path()).expect("a registrar keypair");
    let joiner_key = NodeKey::load_or_mint(joiner_dir.path()).expect("a joiner keypair");

    // Bind first, so the token carries the port the kernel actually gave us.
    let listener = listener::bind("127.0.0.1:0".parse().expect("an addr"))
        .await
        .expect("a loopback peer socket");
    let addr = listener.local_addr().expect("the bound address");
    peers_file_with_listener(&registrar_peers, addr);
    peers_file_with_listener(&joiner_peers, "127.0.0.1:1".parse().expect("an addr"));

    let registrar_store = PeerStore::open(&registrar_peers).expect("the registrar's file opens");
    let (invite, token) =
        pair::mint_invite_as(&registrar_store, &registrar_key, "laptop-2", 600, 1, None)
            .expect("an invite is minted");

    // The registrar's half: exactly what the listener has to do for an
    // enrolment, and the body the listener's own patch hands over.
    let registrar_secret = *registrar_key.secret_bytes();
    let registrar_id = registrar_key.id();
    let registrar_path = registrar_peers.clone();
    let accepting = tokio::spawn(async move {
        let (mut stream, _from) = listener.accept().await.expect("the joiner connects");
        let file =
            tcr::peer::config::read_or_default(&registrar_path).expect("the peers file reads");
        let psks = outstanding_secrets(&file);
        let message_1 = noise::read_frame(&mut stream).await.expect("message 1");
        let mut scratch = vec![0_u8; tcr_peer_wire::MAX_FRAME_BYTES];
        let read = noise::read_message_1_matching(
            &registrar_secret,
            Handshake::Enrol,
            &psks,
            &message_1,
            &mut scratch,
        )
        .expect("the invite's secret decrypts message 1");
        let matched = read
            .psk
            .expect("an IKpsk1 message 1 names the invite that matched it");
        let mut session =
            noise::finish_responder(&mut stream, read.state, Handshake::Enrol, |remote| {
                Ok(PeerId(
                    <[u8; 32]>::try_from(remote).expect("a 32-byte remote static"),
                ))
            })
            .await
            .expect("the enrolment handshake completes");

        // The stream header first, as on every other stream: the enrolment
        // exemption the listener needs is "no pinned row for the first frame",
        // never "no header".
        let header_frame = noise::recv_encrypted(&mut stream, &mut session.transport)
            .await
            .expect("the joiner's stream header");
        let header: StreamHeader =
            serde_json::from_slice(&header_frame).expect("the first frame is the header");
        assert_eq!(
            header.kind,
            StreamKind::Control,
            "an enrolment is a CONTROL stream and nothing else may be opened on it"
        );

        let frame = noise::recv_encrypted(&mut stream, &mut session.transport)
            .await
            .expect("the joiner's first control frame");
        let Control::Enroll(enroll) =
            serde_json::from_slice::<Control>(&frame).expect("it is a control message")
        else {
            panic!("an enrolment session may send exactly one Enroll and nothing else");
        };
        pair::accept_enrolment(
            &registrar_path,
            session.peer,
            &enroll,
            &matched,
            tcr::peer::state::now_ms(),
            Some(std::net::SocketAddr::from(([127, 0, 0, 1], 9600))),
        )
        .expect("the joiner is pinned and the invite is spent");

        // The acknowledgement: the ordinary Hello a pinned peer may receive.
        let hello = hello_for_peer(&NodeFacts::minimal(registrar_id), &ControlGrants::default());
        let bytes = serde_json::to_vec(&Control::Hello(hello)).expect("the Hello serializes");
        noise::send_encrypted(&mut stream, &mut session.transport, &bytes)
            .await
            .expect("the Hello is sent");
    });

    let joiner_store = PeerStore::open(&joiner_peers).expect("the joiner's file opens");
    let pinned = pair::join_as(&joiner_store, &joiner_key, &token, "laptop-2")
        .await
        .expect("the joiner enrols");
    accepting.await.expect("the registrar's half");

    assert_eq!(
        pinned.peer, registrar_id,
        "the joiner pins the key the token named, as the handshake proved it"
    );

    let registrar_file =
        peer_config::read_or_default(&registrar_peers).expect("the registrar's file reads");
    let registrar_row = registrar_file
        .peers
        .iter()
        .find(|row| row.node == joiner_key.id())
        .expect("the registrar recorded the joiner: the hole this test exists for");
    assert_eq!(registrar_row.label, "laptop-2");
    assert!(
        !registrar_row.allow.inspect
            && !registrar_row.allow.gateway
            && !registrar_row.allow.relay
            && !registrar_row.allow.accept_move,
        "a bare pin can do nothing but say hello: {:?}",
        registrar_row.allow
    );

    let joiner_file = peer_config::read_or_default(&joiner_peers).expect("the joiner's file reads");
    assert!(
        joiner_file.peers.iter().any(|row| row.node == registrar_id),
        "and the joiner pins the registrar, so the row exists on both sides"
    );

    // Item 3: `--uses` is decremented, and a one-use invite is gone.
    assert!(
        registrar_file.invite(invite.id).is_none(),
        "a one-use invite must be deleted when it is spent, not left on disk with \
         usesLeft: 0: a spent row is still a PSK in a file"
    );
    assert!(
        outstanding_secrets(&registrar_file).is_empty(),
        "and the registrar offers no secret for a second trial"
    );
    assert_eq!(invite.uses_left, 1, "the minted row started with one use");
}

/// A one-use invite refuses its second joiner, at the file level: the row is
/// gone, so the registrar has nothing to trial message 1 against and
/// `read_message_1` refuses before a byte is written.
///
/// Watched red: with the decrement in `accept_enrolment` removed (leaving the
/// row on disk), `outstanding_secrets` still returns one secret and the
/// `Handshake::Enrol` read below succeeds: the second joiner gets in on a key
/// the operator minted for one machine.
#[test]
fn a_one_use_invite_refuses_its_second_joiner() {
    use tcr::peer::config::{self as peer_config, PeerStore};
    use tcr::peer::id::NodeKey;
    use tcr::peer::listener::outstanding_secrets;
    use tcr_peer_wire::Enroll;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    peers_file_with_listener(&peers, "127.0.0.1:9600".parse().expect("an addr"));
    let node = NodeKey::load_or_mint(dir.path()).expect("a keypair");
    let store = PeerStore::open(&peers).expect("the file opens");

    let (invite, _token) =
        pair::mint_invite_as(&store, &node, "laptop-2", 600, 1, None).expect("an invite is minted");
    let enroll = Enroll {
        invite_id: 0,
        label: "laptop-2".to_string(),
    };

    let first = pair::accept_enrolment(
        &peers,
        PeerId([4_u8; 32]),
        &enroll,
        &invite.secret,
        tcr::peer::state::now_ms(),
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], 9600))),
    )
    .expect("the first joiner proves the invite and is pinned");
    assert_eq!(first.node, PeerId([4_u8; 32]));

    let second = pair::accept_enrolment(
        &peers,
        PeerId([5_u8; 32]),
        &enroll,
        &invite.secret,
        tcr::peer::state::now_ms(),
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], 9600))),
    );
    let refusal = second.expect_err("a spent invite admits nobody");
    assert!(
        format!("{refusal:#}").contains("no outstanding invite"),
        "the refusal has to say the invite is gone: {refusal:#}"
    );

    let file = peer_config::read_or_default(&peers).expect("the file reads");
    assert_eq!(
        file.peers.len(),
        1,
        "the second joiner must leave no row: {:?}",
        file.peers
    );
    assert!(outstanding_secrets(&file).is_empty());
}

/// An enrolment carrying a hostile label is refused, and refusing it does NOT
/// spend the invite: a stranger who cannot pass the sanitizer must not be able
/// to burn an operator's one-use key.
#[test]
fn a_hostile_enrolment_label_is_refused_and_spends_nothing() {
    use tcr::peer::config::{self as peer_config, PeerStore};
    use tcr::peer::id::NodeKey;
    use tcr_peer_wire::Enroll;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    peers_file_with_listener(&peers, "127.0.0.1:9600".parse().expect("an addr"));
    let node = NodeKey::load_or_mint(dir.path()).expect("a keypair");
    let store = PeerStore::open(&peers).expect("the file opens");
    let (invite, _token) =
        pair::mint_invite_as(&store, &node, "laptop-2", 600, 1, None).expect("an invite is minted");

    for hostile in ["alice@example.com", "laptop\u{1b}[31m2", "laptop\n2"] {
        let refusal = pair::accept_enrolment(
            &peers,
            PeerId([6_u8; 32]),
            &Enroll {
                invite_id: 0,
                label: hostile.to_string(),
            },
            &invite.secret,
            tcr::peer::state::now_ms(),
            Some(std::net::SocketAddr::from(([127, 0, 0, 1], 9600))),
        )
        .expect_err("a label this node will not render is refused");
        assert!(
            format!("{refusal:#}").contains("label"),
            "the refusal names the field: {refusal:#}"
        );
    }

    let file = peer_config::read_or_default(&peers).expect("the file reads");
    assert!(file.peers.is_empty(), "nothing is pinned on a refusal");
    assert_eq!(
        file.invite(invite.id).map(|row| row.uses_left),
        Some(1),
        "and the invite is untouched: a refused joiner must not be able to spend an \
         operator's one-use key"
    );
}

// ---------------------------------------------------------------------------
// One writer for each file, and it is a safe one
// ---------------------------------------------------------------------------

/// The peers file is created at 0600 by the one writer, and a file with any
/// other mode is refused rather than trusted.
///
/// Watched red: with the `.permissions(0o600)` line removed from
/// `crate::config::write_atomic`, the first assertion reads 0644 (this box's
/// umask) and fails.
#[test]
fn the_peers_file_is_written_at_0600_and_a_wider_mode_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    peers_file_with_listener(&peers, "127.0.0.1:9600".parse().expect("an addr"));

    let mode = std::fs::metadata(&peers)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the peers file holds join secrets; it is created 0600 or not at all"
    );

    std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o644))
        .expect("widen it by hand");
    let refusal =
        tcr::peer::config::read_or_default(&peers).expect_err("a widened peers file is refused");
    assert!(
        format!("{refusal:#}").contains("0600"),
        "the refusal names the mode it wanted: {refusal:#}"
    );
}

/// A symlink planted where this node's private key goes does not capture it.
///
/// `load_or_mint` reaches its writer only when the path does not exist, and a
/// DANGLING symlink is exactly that: `exists()` follows the link and answers
/// false. An earlier writer then opened it with `create(true)`, which follows
/// the link and writes this node's static secret wherever the link points.
///
/// Watched red: with `create_new(true)` in `write_key_file` changed back to
/// `create(true).truncate(true)`, `load_or_mint` returns `Ok` and the 32-byte
/// secret appears at the link's target.
#[test]
fn a_planted_symlink_does_not_capture_the_node_key() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let captured = dir.path().join("captured.key");
    let key_path = tcr::peer::id::private_key_path(dir.path());
    std::os::unix::fs::symlink(&captured, &key_path).expect("plant the link");

    // `NodeKey` has no `Debug` on purpose (it holds the secret), so the
    // refusal is read off the `Err` arm rather than through `expect_err`.
    let refusal = match tcr::peer::id::NodeKey::load_or_mint(dir.path()) {
        Ok(_) => String::from("load_or_mint returned a key"),
        Err(err) => format!("{err:#}"),
    };
    assert!(
        !captured.exists(),
        "this node's private static key was written through a planted symlink: {refusal}"
    );
    assert!(
        refusal.contains("tcr-node.key"),
        "a key path that is already something else is a refusal that names it: {refusal}"
    );
}

// ---------------------------------------------------------------------------
// The join key can arrive on stdin, and never in a log line
// ---------------------------------------------------------------------------

/// The token reaches `tcr peer join` through stdin, so the secret never enters
/// this process's argv: where every other process on this Mac can read it out
/// of `ps`, and the shell records it in its history.
///
/// Watched red: with `token_from_reader` reading nothing and returning the
/// empty-input refusal, the first assertion fails.
#[test]
fn a_join_token_arrives_on_stdin_and_never_in_a_message() {
    let token = pair::JoinToken::new(
        vec!["127.0.0.1:9600".parse().expect("a test address")],
        PeerId([3_u8; 32]),
        [7_u8; 32],
    );
    let rendered = token.to_token();

    assert_eq!(
        pair::token_from_reader(std::io::Cursor::new(format!("{rendered}\n")))
            .expect("a token on stdin parses"),
        token,
        "the panel writes the key to stdin and this is what reads it"
    );

    let empty = pair::token_from_reader(std::io::Cursor::new(String::new()))
        .expect_err("empty stdin is a refusal, not a default");
    assert!(
        format!("{empty:#}").contains("--stdin"),
        "the refusal tells the operator what --stdin expects: {empty:#}"
    );

    // **No refusal on this path may echo the secret.** A pasted key that
    // failed to parse is still a live bearer secret, and the place operators
    // paste one keeps scrollback.
    let sentinel = "SENTINELSECRETVALUE0123456789ABCDEFGHJKMNPQRSTVWXYZ0";
    let malformed = format!(
        "{}{}:{}:{sentinel}",
        pair::TOKEN_PREFIX,
        "127.0.0.1:9600",
        PeerId([3_u8; 32]).to_wire()
    );
    let refusal = pair::JoinToken::parse(&malformed)
        .expect_err("a 51-character secret field is not 32 bytes");
    let text = format!("{refusal:#}");
    assert!(
        !text.contains(sentinel),
        "a refusal must not print the secret it was handed: {text}"
    );
    assert!(
        text.contains("secret field"),
        "it names the field instead: {text}"
    );
}

// ---------------------------------------------------------------------------
// The last two unlocked writes of the peers file
// ---------------------------------------------------------------------------

/// **`pair::forget` takes the file lock, so a revocation cannot be lost.**
///
/// It was a bare read-modify-write of the peers file: the one write in this
/// module that still was. Unlocked, a revocation is the write most easily lost:
/// a concurrent enrolment or `tcr peer allow` reads the file, and if it saves
/// after the forget, it saves a snapshot taken while the forgotten row was
/// still in it. The operator is told `peer forget: ok`, the row comes back, and
/// the peer's next handshake passes the pin check.
///
/// **Measured by the WAIT, not by a refusal, and that is a fact about
/// `FileLock` worth having in a test**: a held lockfile does not refuse an
/// acquirer, it delays one. `FileLock::acquire` spins, and breaks a lockfile
/// older than `LOCK_STALE_MS` because a `kill -9`'d holder must not wedge the
/// listener: so a live holder that keeps the lock for longer than that is
/// broken through, with a log line. What a caller that takes the lock therefore
/// looks like from outside is a call that cannot return before that bound while
/// somebody else holds the file, against a control call that returns in
/// milliseconds when nobody does.
///
/// Watched red: delete the `FileLock::acquire` line from `pair::forget` and the
/// locked half returns in 16.77 ms instead of waiting out the 2 s hold.
#[test]
fn forget_takes_the_file_lock() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let doomed = PeerId([64_u8; 32]);
    let other = PeerId([65_u8; 32]);
    let pinned = |node: PeerId, label: &str| PeerRow {
        node,
        label: label.to_string(),
        endpoints: Vec::new(),
        added_at: 1_767_225_600_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    };
    let file = tcr::peer::config::PeerFile {
        peers: vec![pinned(doomed, "studio-mac"), pinned(other, "attic-nuc")],
        ..Default::default()
    };
    tcr::peer::config::save(&peers, &file).expect("write the peers file");
    let store = tcr::peer::config::PeerStore::open(&peers).expect("open the store");

    // The control first, so the comparison is against a measurement rather
    // than against an assumption about how fast a file write is.
    let started = std::time::Instant::now();
    assert!(
        pair::forget(&store, &other).expect("the forget writes"),
        "the row was there, so this reports a removal"
    );
    let unlocked = started.elapsed();
    assert!(
        unlocked < std::time::Duration::from_millis(200),
        "with nobody holding the file a forget is immediate: {unlocked:?}"
    );

    // And now with somebody else's read-modify-write in flight.
    let held = tcr::peer::config::FileLock::acquire(&peers).expect("take the file lock first");
    let started = std::time::Instant::now();
    let removed = pair::forget(&store, &doomed).expect("the forget writes");
    let waited = started.elapsed();
    drop(held);

    assert!(removed, "it still removes the row once it has the lock");
    assert!(
        waited >= std::time::Duration::from_millis(tcr::peer::config::LOCK_STALE_MS),
        "a forget must wait for the holder rather than write over its read-modify-write: it \
         took {waited:?}, and the hold is only broken after {}ms",
        tcr::peer::config::LOCK_STALE_MS
    );
    assert!(
        tcr::peer::config::read_or_default(&peers)
            .expect("the file reads")
            .peers
            .is_empty(),
        "and both rows are gone"
    );
    assert!(
        !pair::forget(&store, &doomed).expect("a second forget is not an error"),
        "a peer that is not pinned reports no removal and writes nothing"
    );
}

/// **A pin takes the same lock**, for the same reason and measured the same
/// way: `pair::confirm` and the joiner's half of an enrolment both wrote the
/// row through an unlocked read-modify-write, so a pin racing any other write
/// to this file could be the snapshot that loses it.
///
/// Safe to lock there because neither caller holds the lock already: the
/// registrar's side writes its row inside `accept_enrolment`'s own lock and
/// never through `pin_row`. The last third of this test is that fact: an
/// enrolment still completes, so the lock added below it is not a deadlock.
///
/// Watched red: delete the `FileLock::acquire` line from `pair::pin_row` and
/// the middle section returns in 16.71 ms instead of waiting out the 2 s hold.
#[test]
fn a_pin_takes_the_file_lock_and_an_enrolment_still_completes() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    // With a listener address, because the enrolment at the end mints an
    // invite and an invite with no address to dial is refused.
    peers_file_with_listener(&peers, "127.0.0.1:9600".parse().expect("an addr"));
    let store = tcr::peer::config::PeerStore::open(&peers).expect("open the store");
    let peer = PeerId([66_u8; 32]);

    let started = std::time::Instant::now();
    pair::confirm(&store, &peer, "123456", None).expect("the pin writes");
    let unlocked = started.elapsed();
    assert!(
        unlocked < std::time::Duration::from_millis(200),
        "with nobody holding the file a pin is immediate: {unlocked:?}"
    );
    let written = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(
        written.peers.len(),
        1,
        "one row, for the peer just confirmed"
    );
    assert_eq!(written.peers[0].node, peer);

    let held = tcr::peer::config::FileLock::acquire(&peers).expect("take the file lock first");
    let started = std::time::Instant::now();
    pair::confirm(&store, &peer, "654321", None).expect("the re-pin writes");
    let waited = started.elapsed();
    drop(held);
    assert!(
        waited >= std::time::Duration::from_millis(tcr::peer::config::LOCK_STALE_MS),
        "a pin must wait for the holder rather than write over its read-modify-write: it took \
         {waited:?}"
    );

    // The registrar's own write is inside `accept_enrolment`'s lock and does
    // not go through `pin_row`, so it still completes: the deadlock this
    // could have introduced.
    let joiner = PeerId([67_u8; 32]);
    let node =
        tcr::peer::id::NodeKey::load_or_mint(dir.path()).expect("a node key for the registrar");
    let (invite, _token) = pair::mint_invite_as(&store, &node, "attic-nuc", 600, 1, None)
        .expect("an invite is minted");
    let row = pair::accept_enrolment(
        &peers,
        joiner,
        &tcr_peer_wire::Enroll {
            invite_id: 0,
            label: "attic-nuc".to_string(),
        },
        &invite.secret,
        tcr::peer::state::now_ms(),
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], 9600))),
    )
    .expect("the registrar pins the joiner under its own lock");
    assert_eq!(row.node, joiner);
}
