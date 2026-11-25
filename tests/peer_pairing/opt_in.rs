// ---------------------------------------------------------------------------
// A fresh install can opt in with one command
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// The line a verb prints when it gave this Mac a port, in the two halves the
/// three tests below check: what was written, and when it opens.
///
/// Spelled out here rather than read from `config::default_listen()`: a test
/// that asserts a value against the very constant that produced it agrees with
/// any change to it, including a change nobody meant. Written this way, moving
/// the port reds these three, which is the conversation that move deserves.
const LISTEN_WRITTEN: &str = "peer.listen: 0.0.0.0:7755 written into";
const LISTEN_RESTART: &str = "quit TcrBar and open it again";

/// The address the three verbs must write, as an operator would read it off
/// the screen and type it into the other Mac.
fn the_default_port() -> SocketAddr {
    "0.0.0.0:7755".parse().expect("a literal address")
}

/// A peers file that exists, is well formed, and names no listener: a fresh
/// install, which is the case every one of these is about.
fn peers_file_with_no_listener(tag: &str) -> std::path::PathBuf {
    let dir = scratch(tag);
    let peers = dir.join("tcr-peers.json");
    let fresh = PeerFile::default();
    assert_eq!(
        fresh.listen, None,
        "the default must still be no listener at all; that default is the privacy promise \
         these three verbs are allowed to break only on an explicit opt-in"
    );
    config::save(&peers, &fresh).expect("seed a peers file with no listener");
    peers
}

/// One pinned Mac, so `peer share on` has somebody to share with. Fake key,
/// fake label: this repository is public.
fn pin_one_peer(peers: &std::path::Path) {
    let mut file = config::read_or_default(peers).expect("the seeded peers file reads back");
    file.peers.push(config::PeerRow {
        node: PeerId([0x5a; 32]),
        label: "laptop-2".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: config::Allow::default(),
        lend: Vec::new(),
    });
    config::save(peers, &file).expect("pin one peer");
}

/// **`tcr peer find on` gives this Mac a port instead of refusing.**
///
/// It used to bail with "peer.find needs a listener port; set \"listen\" in
/// ... first", which is the first command of the walkthrough and a dead end: no
/// other verb writes that key, so the only way on was a text editor.
///
/// Watch it fail on the base: `find on` exits non-zero with that refusal, the
/// seeded file still has no `listen`, and neither printed line appears.
#[test]
fn find_on_gives_this_mac_a_port_when_it_has_none() {
    let peers = peers_file_with_no_listener("listen-find");

    let (stdout, stderr, ok) = run_tcr(&peers, &["find", "on"]);
    assert!(
        ok,
        "`tcr peer find on` must no longer refuse a Mac with no listener.\n\
         stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(LISTEN_WRITTEN),
        "it must say what it wrote: {stdout}"
    );
    assert!(
        stdout.contains(LISTEN_RESTART),
        "and that the port opens at the next start: {stdout}"
    );
    assert_eq!(
        config::read_or_default(&peers)
            .expect("the peers file reads back")
            .listen,
        Some(the_default_port()),
        "the chosen port must be on disk, not only in the message"
    );

    // A port the operator chose is the operator's: a second run changes
    // nothing and says nothing about it.
    let chosen: SocketAddr = "127.0.0.1:19000".parse().expect("a literal address");
    crate::peer_support::hand_edit_listen(&peers, chosen);

    let (stdout, stderr, ok) = run_tcr(&peers, &["find", "on"]);
    assert!(
        ok,
        "`tcr peer find on` again.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("peer.listen:"),
        "a listener that is already set must not be mentioned, let alone moved: {stdout}"
    );
    assert_eq!(
        config::read_or_default(&peers)
            .expect("the peers file reads back")
            .listen,
        Some(chosen),
        "the hand-chosen port must survive untouched"
    );
}

/// **`tcr peer share on` gives this Mac a port too.** Sharing means those Macs
/// reach this one, and it used to turn on with nothing listening and no word
/// about it.
///
/// Watch it fail on the base: the command exits 0, prints no `peer.listen:`
/// line and leaves `listen` absent, so `peer share on` reports success for a
/// Mac nobody can reach.
#[test]
fn share_on_gives_this_mac_a_port_when_it_has_none() {
    let peers = peers_file_with_no_listener("listen-share");
    pin_one_peer(&peers);

    let (stdout, stderr, ok) = run_tcr(&peers, &["share", "on"]);
    assert!(
        ok,
        "`tcr peer share on`.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(LISTEN_WRITTEN) && stdout.contains(LISTEN_RESTART),
        "it must say what it wrote and when the port opens: {stdout}"
    );
    assert_eq!(
        config::read_or_default(&peers)
            .expect("the peers file reads back")
            .listen,
        Some(the_default_port()),
        "the chosen port must be on disk"
    );

    // Turning sharing OFF is not an opt-in, so it opens nothing: the port
    // written above stays, and a file that never had one never gets one here.
    let untouched = peers_file_with_no_listener("listen-share-off");
    pin_one_peer(&untouched);
    let (stdout, stderr, ok) = run_tcr(&untouched, &["share", "off"]);
    assert!(
        ok,
        "`tcr peer share off`.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        config::read_or_default(&untouched)
            .expect("the peers file reads back")
            .listen,
        None,
        "turning something off must never open a port: {stdout}"
    );
}

/// **`tcr peer internet on` gives this Mac a port too.** What it asks the
/// router for is a mapping TO the listener's port, so with none it wrote the
/// flag and printed "no listener is configured", which is a switch that does
/// nothing dressed as one that worked.
///
/// Watch it fail on the base: the command exits 0, says "no listener is
/// configured, so there is no port to map yet; set \"listen\" in the peers
/// file", and leaves `listen` absent.
#[test]
fn internet_on_gives_this_mac_a_port_when_it_has_none() {
    let peers = peers_file_with_no_listener("listen-internet");

    let (stdout, stderr, ok) = run_tcr(&peers, &["internet", "on"]);
    assert!(
        ok,
        "`tcr peer internet on`.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(LISTEN_WRITTEN) && stdout.contains(LISTEN_RESTART),
        "it must say what it wrote and when the port opens: {stdout}"
    );
    assert!(
        !stdout.contains("to map yet"),
        "and must not still report that there is nothing to map: {stdout}"
    );
    let written = config::read_or_default(&peers).expect("the peers file reads back");
    assert_eq!(
        written.listen,
        Some(the_default_port()),
        "the chosen port must be on disk"
    );
    assert!(
        written.internet,
        "and the switch this verb is actually about must still be written"
    );
    assert!(
        stdout.contains("port 7755"),
        "the verb must now name the port it maps: {stdout}"
    );
}

// MARK: - The `--json` lines a panel reads

/// The exact bytes of every `tcr peer pair --json` line.
///
/// # Why a whole-line assertion and not a field-by-field one
///
/// The panel's side of this is a `Decodable` enum keyed on `event`
/// (`apps/macos/Sources/TcrBarCore/PeerPairEvent.swift`), and what breaks it is
/// not a wrong value but a renamed key: `waitSeconds` becoming `wait_seconds`
/// decodes to nothing, the sheet never leaves its first state, and every gate
/// on both sides stays green. So these compare the rendered line, which is the
/// thing that actually crosses the pipe.
///
/// Serde writes the fields in declaration order and the tag first, which is why
/// these literals can be exact. A reordering is a failing test here rather than
/// a silent difference nobody renders.
#[test]
fn the_json_pairing_lines_are_exactly_these() {
    assert_eq!(
        pair::PairEvent::Asking {
            addr: "10.0.1.24:7749".to_string(),
            instance: "8f2c1ad63b0e4471".to_string(),
            wait_seconds: 600,
        }
        .line()
        .expect("an asking line renders"),
        r#"{"event":"asking","addr":"10.0.1.24:7749","instance":"8f2c1ad63b0e4471","waitSeconds":600}"#
    );
    assert_eq!(
        pair::PairEvent::Comparing {
            code: "418902".to_string(),
        }
        .line()
        .expect("a comparing line renders"),
        r#"{"event":"comparing","code":"418902"}"#
    );
    assert_eq!(
        pair::PairEvent::Trusted {
            peer: "tcr-4b8we1r0zp".to_string(),
        }
        .line()
        .expect("a trusted line renders"),
        r#"{"event":"trusted","peer":"tcr-4b8we1r0zp"}"#
    );
    assert_eq!(
        pair::PairEvent::Refused {
            message: "peer pair: refused, 418902 here, 418903 there".to_string(),
        }
        .line()
        .expect("a refused line renders"),
        r#"{"event":"refused","message":"peer pair: refused, 418902 here, 418903 there"}"#
    );
}

/// The wait a reader is told about is the wait this command actually keeps.
///
/// Two numbers, one fact: the panel arms its deadline off `waitSeconds` and
/// `pair()` gives up at `PAIR_WAIT`. A literal 600 in the CLI would let one
/// move without the other, and the panel would then either cancel a live
/// handshake or sit forever on a process that has already exited.
#[test]
fn the_announced_wait_is_the_deadline_the_command_keeps() {
    let main = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/cmd/peer/pairing.rs"
    ))
    .expect("src/cmd/peer/pairing.rs reads");
    assert!(
        main.contains("wait_seconds: tcr::peer::pair::PAIR_WAIT.as_secs()"),
        "the asking line no longer takes its wait from PAIR_WAIT, so the panel's deadline and \
         the command's can drift apart"
    );
}

/// A refusal reaches stdout as a typed line, not only stderr.
///
/// The panel reads stdout. A refusal that went to stderr alone would leave the
/// sheet in its last state with the process gone, which is the shape of the
/// blocker this whole flag exists to close.
#[test]
fn every_refusal_is_announced_before_it_is_returned() {
    let main = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/cmd/peer/pairing.rs"
    ))
    .expect("src/cmd/peer/pairing.rs reads");
    let start = main
        .find("async fn run_peer_pair(")
        .expect("the pair verb is still there");
    let arm = &main[start..];
    let end = arm
        .find("async fn run_peer_invite(")
        .expect("the pair verb still ends where the invite verb begins");
    let arm = &arm[..end];
    assert_eq!(
        arm.matches("PairEvent::Refused").count(),
        3,
        "the pair arm no longer announces all three of its refusals (the knock, the ten-minute \
         deadline and a mismatch) on stdout"
    );
    assert!(
        arm.contains("if !a.json {"),
        "the prose half is no longer gated, so a parser would be handed English on the stream \
         it is reading"
    );
}
