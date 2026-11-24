// ---------------------------------------------------------------------------
// The port a knock says to answer on
//
// A knock arrives on a connection whose SOURCE port is ephemeral, so the
// address the queue keys on is the bare IP and there is nothing in it to dial
// back. The answer went to whatever was listening on the default port: on a
// Mac whose own listener is elsewhere, that is nobody, and on the machine that
// sent the knock it is that machine's own listener. The knock carries the
// port now, and every surface that shows a pending row shows it.
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// The SHIPPED listener writes the port a knock named onto the pending row,
/// and the row's dial address is that port.
///
/// Watched red: drop `listen_port` from the `record_knock` call in
/// `src/peer/listener.rs` and this reports `dial_address` of `127.0.0.1`
/// against the expected `127.0.0.1:7766`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_that_names_its_port_is_dialled_back_at_that_port() {
    let node = Node::new("knock-port");
    let addr = serve(node.context()).await;

    knock_at_with_port(addr, instance(0x71), Some("studio-mac"), Some(7766))
        .await
        .expect("the knock is taken");

    let value = node.state();
    let row = value
        .visible_pending()
        .into_iter()
        .next()
        .expect("the knock left a row an operator can see");
    assert_eq!(
        row.listen_port,
        Some(7766),
        "the port the knock named has to reach the row: it is the only dialable number"
    );
    assert_eq!(
        row.dial_address(),
        "127.0.0.1:7766",
        "the row's dial address is the source IP with the port the knocker named"
    );
    assert_eq!(
        row.addr, "127.0.0.1",
        "the KEY stays the bare IP: the mutes, the bans and the accepted windows all \
         compare against the address a connection arrives on"
    );
}

/// A knock from a build that predates the field, or from a Mac with no
/// listener of its own, leaves the bare address and no invented port.
///
/// Watched red: default `listen_port` to `Some(7755)` anywhere on the path and
/// this reports a dial address of `127.0.0.1:7755`, which is the guess that
/// dialled the wrong Mac in the first place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knock_that_names_no_port_leaves_the_bare_address() {
    let node = Node::new("knock-no-port");
    let addr = serve(node.context()).await;

    knock_at_with_port(addr, instance(0x72), Some("studio-mac"), None)
        .await
        .expect("the knock is taken");

    let row = node
        .state()
        .visible_pending()
        .into_iter()
        .next()
        .expect("a knock with no port is still a pending row");
    assert_eq!(row.listen_port, None, "nothing is invented for it");
    assert_eq!(
        row.dial_address(),
        "127.0.0.1",
        "with no port named there is only the host, and the dialling command defaults it"
    );
}

/// A Mac that moved its listener re-knocks, and the row follows it rather than
/// keeping the port that is no longer served.
#[test]
fn a_second_knock_replaces_the_port_the_row_was_holding() {
    let mut value = PeerState::default();
    value
        .record_knock("192.0.2.31", instance(0x81), None, 1, Some(7766), 1_000)
        .expect("the first knock records");
    value
        .record_knock("192.0.2.31", instance(0x81), None, 1, Some(7788), 2_000)
        .expect("the second knock coalesces onto the same address");

    assert_eq!(value.knocks_queued(), 1, "one address is one row");
    assert_eq!(
        value
            .find_pending("192.0.2.31")
            .expect("the row")
            .dial_address(),
        "192.0.2.31:7788",
        "the newest knock's port wins: the old one is not being listened on"
    );

    // And back to nothing, which is what a downgraded build sends.
    value
        .record_knock("192.0.2.31", instance(0x81), None, 1, None, 3_000)
        .expect("a third knock coalesces too");
    assert_eq!(
        value
            .find_pending("192.0.2.31")
            .expect("the row")
            .dial_address(),
        "192.0.2.31",
        "a knock that names no port must not be answered at a port an older knock named"
    );
}

/// Every string a surface prints for a pending row selects that row: the
/// operator pastes back what they just read.
#[test]
fn the_dial_address_a_row_is_printed_under_selects_that_row() {
    let mut value = PeerState::default();
    value
        .record_knock("192.0.2.32", instance(0x82), None, 1, Some(7766), 1_000)
        .expect("the knock records");

    let printed = value
        .find_pending("192.0.2.32")
        .expect("the row")
        .dial_address();
    assert_eq!(printed, "192.0.2.32:7766");
    assert!(
        value.find_pending(&printed).is_some(),
        "`tcr peer accept` on the address the row was printed under has to find it"
    );
    assert!(
        value.find_pending("192.0.2.32").is_some(),
        "and the bare key still selects it, which is what every older note says to type"
    );
}

/// An IPv6 knock's dial address is bracketed, because `2001:db8::1:7766` is
/// not an address anything can split.
#[test]
fn an_ipv6_row_brackets_its_host() {
    let mut value = PeerState::default();
    value
        .record_knock("2001:db8::1", instance(0x83), None, 1, Some(7766), 1_000)
        .expect("the knock records");
    assert_eq!(
        value
            .find_pending("2001:db8::1")
            .expect("the row")
            .dial_address(),
        "[2001:db8::1]:7766"
    );
}

/// **`tcr peer pending` and `tcr peer ls --json` say the same thing**, through
/// the binary this build produced, against a temp `--peers` and the state file
/// beside it.
///
/// Both read `PeerState::visible_pending` and both now render
/// `Knock::dial_address`, so the address a terminal prints and the address the
/// panel dials cannot disagree. The two surfaces were read minutes apart on
/// one machine and appeared to disagree; what differed was which state FILE
/// each invocation resolved, never the rows.
#[test]
fn the_two_pending_surfaces_print_the_same_dial_address() {
    let node = Node::new("pending-surfaces");
    let mut value = PeerState::default();
    value
        .record_knock(
            "192.0.2.33",
            instance(0x84),
            Some("studio-mac".to_string()),
            1,
            Some(7766),
            pair::now_ms(),
        )
        .expect("the knock with a port records");
    value
        .record_knock(
            "192.0.2.34",
            instance(0x85),
            Some("attic-nuc".to_string()),
            1,
            None,
            pair::now_ms(),
        )
        .expect("the knock with no port records");
    node.write_state(&value);

    // `run_tcr`, the same helper the other CLI test in this file drives the
    // shipped binary with: one spawn, `--peers` at a temp path, nothing read
    // from the operator's own directories.
    let run = |args: &[&str]| -> String {
        let (out, err, ok) = run_tcr(&node.peers, args);
        assert!(ok, "tcr peer {args:?} failed: {err}");
        out
    };

    // The address each prose row is FILED under, which is the leading field of
    // the `<addr>: pending: …` line. Read as a field rather than searched for
    // as a substring: the line's own separator is a colon too, so
    // `contains("192.0.2.34:")` is true of a row with no port at all.
    let prose = run(&["pending"]);
    let prose_addrs: Vec<&str> = prose
        .lines()
        .filter_map(|line| line.split_once(": pending: "))
        .map(|(addr, _)| addr)
        .collect();
    assert_eq!(
        prose_addrs,
        vec!["192.0.2.33:7766", "192.0.2.34"],
        "`tcr peer pending` prints the port to answer on, and nothing at all for a knock \
         that named none: {prose}"
    );

    let listing = run(&["ls", "--json"]);
    let document: serde_json::Value =
        serde_json::from_str(&listing).expect("`tcr peer ls --json` is one JSON document");
    let rows = document["pending"]
        .as_array()
        .expect("the document carries a pending array");
    let addrs: Vec<&str> = rows
        .iter()
        .map(|row| row["addr"].as_str().expect("every row names an address"))
        .collect();
    assert_eq!(
        addrs,
        vec!["192.0.2.33:7766", "192.0.2.34"],
        "the panel is handed the same two strings the terminal printed, and it passes \
         them to `tcr peer pair` unchanged"
    );
    assert_eq!(
        addrs, prose_addrs,
        "the two surfaces disagree about what to dial, which is the whole point of \
         reading them from one place"
    );
}
