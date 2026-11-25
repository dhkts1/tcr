//! Reaching a peer whose recorded address no longer works: a moved peer and a
//! refreshed hello, a borrow forwarded through a third Mac, a lender nobody can
//! dial, and a join key that falls through to its second address.

use super::*;

// ---------------------------------------------------------------------------
// A moved peer
//
// Both plausible paths read one production gap, and only one of
// them has a patch small enough to hand over here; see gap 5 in
// this file's module docs. The second path, "through
// the beacon bridge with Hello disabled," needs a binding that outlives one
// CLI process and is not written here at all, not even ignored: the same
// call a hand-mode leg needs and is still waiting on its own
// rather than shipping as a red placeholder.
// ---------------------------------------------------------------------------

/// **A moved peer, reached through the endpoint a refreshed `Hello` carries,
/// with no re-pairing.**
///
/// `lender` and `mover` pair the way `a_borrowed_request_...` does (join, so
/// both rows carry a dialable address from the start: the joiner's own Hello
/// on the enrolment stream announces its listen port to the accepting side).
/// `mover` then shuts down and reboots on a FRESH port, so `lender`'s row
/// for it names a socket nothing answers on any more. `mover` calls the verb
/// this test is written against, `tcr peer hello <lender>`, which dials
/// `lender` on the address it still has (lender never moved) and sends one
/// `Control::Hello`. The listener's own answering half already records the
/// caller's address on an incoming `Hello` (`listener.rs:1734`, wired and
/// green today, proven by `two_macs_find_each_other_...` recording the same
/// kind of endpoint at pairing time), so this one round trip is also
/// `lender` learning where `mover` is now, and `mover` never goes through
/// `pair` or `join` a second time to get there.
///
/// Ran `#[ignore]`d until the `tcr peer hello` verb this is
/// written against was wired (gap 5, this file's module docs); nothing below this line
/// changed when the verb landed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moved_peer_is_reached_through_a_refreshed_hello_endpoint() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut mover = Mac::new("mover", &upstream, "mover-fake", "at-fake-mover", true);

    step(1, "boot two tcr processes: lender and mover");
    lender.boot();
    mover.boot();

    step(
        2,
        "mover joins lender; lender's row for it names mover's own listen port, the one \
         the joiner's Hello announced, never the ephemeral port the join arrived FROM",
    );
    let minted = lender.peer_ok(&["invite", "--label", "mover-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&mover, &key);
    let mover_node = lender.first_pinned_node();
    let lender_node = mover.first_pinned_node();
    let before = lender.ls()["peers"][0]["endpoints"].clone();
    assert_eq!(
        before.as_array().map(Vec::len),
        Some(1),
        "the join must leave exactly one endpoint on lender's row for mover: {before}"
    );
    assert_eq!(
        before[0]["addr"].as_str(),
        Some(format!("127.0.0.1:{}", mover.peer_port).as_str()),
        "a join's endpoint on the ACCEPTING side is the port the joiner's own Hello \
         announced; it used to be the ephemeral port the connection arrived from, and \
         this assertion was the inverse until that was fixed: {before}"
    );

    step(
        3,
        "mover restarts on a FRESH port; lender still has no way to dial it",
    );
    mover.shutdown();
    let old_port = mover.peer_port;
    // Hold the old port until mover is back up, so neither the draw below nor
    // a redraw inside `boot` (it draws again when it loses the port race) can
    // get it from the kernel. The bind can fail if another process took the
    // port first; that also keeps the kernel from handing it out, and
    // `loopback_port_other_than` still refuses it if it comes back anyway.
    let held_old_port = std::net::TcpListener::bind(("127.0.0.1", old_port));
    mover.move_listen_port(loopback_port_other_than(old_port, free_loopback_port));
    mover.boot();
    drop(held_old_port);
    assert_ne!(
        mover.peer_port, old_port,
        "the whole point of this test is a peer answering on a DIFFERENT port"
    );

    step(
        4,
        "mover says hello to lender on lender's still-good address; the one round trip \
         is also lender learning mover's new listen port",
    );
    let hello = mover.peer_ok(&["hello", &lender_node]);
    assert!(
        hello.contains("peer hello: ok"),
        "the verb must confirm the round trip: {hello}"
    );

    step(
        5,
        "lender's row for mover now names a DIALABLE address, mover's fresh listen port, \
         sourced hello, and NOT by re-pairing",
    );
    let ls = lender.ls();
    let endpoints = ls["peers"][0]["endpoints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let refreshed = endpoints
        .iter()
        .find(|endpoint| {
            endpoint["addr"].as_str() == Some(format!("127.0.0.1:{}", mover.peer_port).as_str())
        })
        .unwrap_or_else(|| {
            panic!(
                "no endpoint on lender's row for mover names its fresh listen port: {endpoints:?}"
            )
        });
    assert_eq!(
        refreshed["source"].as_str(),
        Some("hello"),
        "an endpoint learned from this round trip is sourced `hello`, never `paired`: {refreshed}"
    );
    assert_eq!(
        refreshed["kind"].as_str(),
        Some("direct"),
        "a socket this node can dial itself is a direct locator: {refreshed}"
    );
    assert_eq!(
        ls["peers"].as_array().map(Vec::len),
        Some(1),
        "lender must still hold exactly one pinned row for mover, not a second one from a \
         re-pair: {ls}"
    );
    assert_eq!(
        ls["peers"][0]["node"].as_str(),
        Some(mover_node.as_str()),
        "the refreshed row must be the SAME pinned key, which is what proves nothing re-paired"
    );

    lender.shutdown();
    mover.shutdown();
}

// ---------------------------------------------------------------------------
// A forwarded borrow
//
// The leg `scripts/peer-e2e-local.sh --nodes 3 --moved` runs under the filter
// `forward`, which matched nothing until this file. Its subject is the client
// half of a forward: three real `tcr` processes, the borrower holding NO
// address for its lender at all, and the request arriving anyway because a
// third Mac both of them pinned carried the stream.
// ---------------------------------------------------------------------------

/// **A borrow that arrives through a third Mac, with the lender's own address
/// gone from the borrower's file.**
///
/// The topology is the one the forward-dial item exists for. `borrower` holds
/// a lease from `lender` and not one address it answers on; `carrier` is
/// pinned by both, grants `borrower` the right to ask it to forward, and holds
/// `lender`'s real listen port because `lender` said hello to it. The request
/// is then served on the LENDER's own credential, which is what proves the
/// stream reached the lender itself rather than being answered by the Mac in
/// the middle: a forwarder holds no key for what it carries and could not
/// have served this if it wanted to.
///
/// # Three things are asserted and each fails differently
///
/// The borrower's row for the lender has NO endpoint at the moment the request
/// is made, so a pass cannot come from a direct dial that quietly still worked.
/// The upstream saw the lender's credential, so the answer came from the
/// lender. And `carrier`'s log carries `peer forward: carried`, so the bytes
/// really went through the third Mac rather than by some route this test did
/// not think of.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_forwarded_borrow_reaches_the_lender_through_a_third_mac() {
    let (upstream, seen) = spawn_upstream().await;
    let (mut lender, mut carrier, mut borrower) = boot_lender_carrier_borrower(&upstream);

    step(
        2,
        "warm the lender's own account, so it has a measured window to lend from",
    );
    warm(&lender).await;

    step(3, "borrower joins lender and is granted a lease");
    let lender_at_borrower = lend_to(&lender, &borrower, "borrower-mac");

    step(
        4,
        "borrower joins carrier: borrower may ASK it to carry (`carry`), carrier may \
         forward FOR it (`forward`). Two grants, two directions, both explicit",
    );
    let (borrower_at_carrier, carrier_at_borrower) = join_to(&carrier, &borrower, "borrower-mac");
    borrower.peer_ok(&["allow", &carrier_at_borrower, "carry", "on"]);
    carrier.peer_ok(&["allow", &borrower_at_carrier, "forward", "on"]);

    step(
        5,
        "lender joins carrier and says hello, so carrier holds the lender's real LISTEN \
         port and not the ephemeral port the join arrived from",
    );
    let (lender_at_carrier, carrier_at_lender) = join_to(&carrier, &lender, "lender-mac");
    let hello = lender.peer_ok(&["hello", &carrier_at_lender]);
    assert!(
        hello.contains("peer hello: ok"),
        "the round trip that teaches the carrier where the lender listens must confirm: \
         {hello}"
    );
    let listening = format!("127.0.0.1:{}", lender.peer_port);
    let at_carrier = endpoints_for(&carrier, &lender_at_carrier);
    assert!(
        at_carrier
            .iter()
            .any(|endpoint| endpoint["addr"].as_str() == Some(listening.as_str())),
        "the carrier cannot forward to an address it does not hold: {at_carrier:?}"
    );

    step(
        6,
        "the lender's addresses go stale in the borrower's file: its row keeps the pinned \
         key, the lease and the grant, and nothing to dial",
    );
    strip_endpoints(&borrower, &lender_at_borrower);
    let stripped = endpoints_for(&borrower, &lender_at_borrower);
    assert!(
        stripped.is_empty(),
        "the whole case is a lender with no address on the borrower's row: {stripped:?}"
    );

    step(
        7,
        "the borrower's request is served anyway, through the fallback provider it installs \
         itself for on this dry fleet, on the LENDER's own credential, and the carrier's log \
         says it carried the stream",
    );
    let before = seen.requests();
    let mut result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        result = post_messages(&borrower.proxy).await;
        println!("         borrower attempt {attempt} -> {}", result.0);
        if result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    assert_eq!(
        result.0,
        200,
        "the borrowed request must be served through the carrier. Borrower tail:\n{}\n\
         Carrier tail:\n{}\nLender tail:\n{}",
        tail(&borrower.log(), 14),
        tail(&carrier.log(), 14),
        tail(&lender.log(), 14)
    );
    let credentials = seen.credentials();
    assert!(
        credentials.len() > before,
        "the request never reached the upstream at all: {credentials:?}"
    );
    assert_eq!(
        credentials.last().map(String::as_str),
        Some("Bearer at-fake-lender"),
        "the request that served the borrower must be on the LENDER's own token: a \
         forwarder holds no key for what it carries and could not have answered this: \
         {credentials:?}"
    );

    // The carrier writes this line when the carried stream CLOSES, which is
    // after the borrower already holds its 200, so the line is waited for and
    // never read once. Without it the borrow found some other route and this
    // test is not measuring a forward; the wait panics with the carrier's tail.
    carrier.wait_for_log("peer forward: carried");
    // And the row is STILL addressless, so nothing along the way quietly wrote
    // an endpoint back and turned the next run into a direct dial.
    let after = endpoints_for(&borrower, &lender_at_borrower);
    assert!(
        after.is_empty(),
        "the borrower's row for the lender gained an endpoint during the borrow, so a \
         second run of this test would not be measuring a forward: {after:?}"
    );

    borrower.shutdown();
    carrier.shutdown();
    lender.shutdown();
}

// ---------------------------------------------------------------------------
// The Mac nobody can dial
//
// The leg `scripts/peer-e2e-local.sh --nodes 3 --undialable` runs under the
// filter `undialable`. Its subject is the topology the reverse carry exists
// for: the lender has no address ANYWHERE, not on the borrower's row and not
// on the carrier's, so neither Mac can open a socket to it and the only way in
// is a socket the lender itself opened.
// ---------------------------------------------------------------------------

/// **A lender nothing can dial is still SERVED, over the socket it parked at
/// a friend, inside the borrow timeout.**
///
/// # What this measures today
///
/// The reverse topology in three real processes: the lender has no address on
/// either Mac's row, so the borrower cannot dial it and neither can the Mac it
/// asks to carry. The borrow goes out through the carrier anyway (the
/// borrower's own log line says so, and this asserts it, otherwise the case
/// would pass on a borrower that never asked anyone) and it ENDS: no 200 from
/// a path this test did not set up, and no request held open, which is the
/// half of the gate that has to hold whether or not a carrier was parked.
///
/// The sentence the carrier says when it finds neither a parked carrier nor an
/// address is gated in process, in `tests/peer/reach/peer_forward.rs`, rather than here:
/// a refusal reaches a peer's log through a per-address rate limit
/// (`listener::RefusalLog`) that an enrolment refusal from the same loopback
/// address earlier in the same run can spend, so an assertion on it here would
/// be a flake with a story.
///
/// # What flips it to a served 200, and it landed
///
/// The lender parking a carrier at the carrier Mac: `StreamKind::Park` on the
/// peer wire, the listener arm that reads it and calls
/// `tunnel::park_reverse_carry`, and the boot supervisor that opens one
/// carrier per friend when nothing can dial this Mac. So the assertion below
/// is the 200 the forwarded-borrow test makes, over a socket the lender
/// opened outwards, and the in-process half stays gated by
/// `tests/peer/reach/peer_forward.rs::a_forward_rides_the_carrier_an_undialable_target_parked`.
///
/// Watched red by removing the `StreamKind::Park` arm from
/// `listener::serve_stream`: the carrier never holds a socket, step 7's wait
/// for the desk's own line times out, and the borrow goes back to ending
/// unserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn an_undialable_lender_ends_the_borrow_through_a_carrier_and_never_hangs() {
    let (upstream, _seen) = spawn_upstream().await;
    let (mut lender, mut carrier, mut borrower) = boot_lender_carrier_borrower(&upstream);

    step(
        2,
        "warm the lender's own account, so it has a window to lend from",
    );
    warm(&lender).await;

    step(3, "borrower joins lender and is granted a lease");
    let lender_at_borrower = lend_to(&lender, &borrower, "borrower-mac");

    step(
        4,
        "borrower joins carrier: `carry` one way, `forward` the other, both explicit",
    );
    let (borrower_at_carrier, carrier_at_borrower) = join_to(&carrier, &borrower, "borrower-mac");
    borrower.peer_ok(&["allow", &carrier_at_borrower, "carry", "on"]);
    carrier.peer_ok(&["allow", &borrower_at_carrier, "forward", "on"]);

    step(5, "lender joins carrier, so the carrier pins it at all");
    let (lender_at_carrier, carrier_at_lender) = join_to(&carrier, &lender, "lender-mac");
    // The lender's own half of "this Mac may hold a socket for me". Without it
    // `tunnel::reverse_carriers` hands back an empty list and the lender parks
    // nothing, which is the same grant `forwarders_for` reads for every other
    // carry and deliberately not a new one.
    lender.peer_ok(&["allow", &carrier_at_lender, "carry", "on"]);

    step(
        6,
        "the lender becomes undialable: its addresses go from BOTH files, so neither the \
         borrower nor the carrier holds a socket address for it",
    );
    strip_endpoints(&borrower, &lender_at_borrower);
    strip_endpoints(&carrier, &lender_at_carrier);
    assert!(
        endpoints_for(&borrower, &lender_at_borrower).is_empty()
            && endpoints_for(&carrier, &lender_at_carrier).is_empty(),
        "the whole case is a lender with no address on either Mac's row"
    );

    step(
        7,
        "the lender restarts with no address anywhere, so its boot asks the carrier to hold \
         a socket for it",
    );
    lender.shutdown();
    lender.boot();
    // The CARRIER's line, not the lender's: it is the one that says a socket
    // reached the desk and was admitted, which is the fact step 9 rides. The
    // lender's own line says only that it sent a header.
    let parked = carrier.wait_for_log("holding a carrier for a Mac that cannot be dialled");
    assert!(
        parked.contains("open="),
        "the desk reports how many carriers it holds for that Mac, and one of them is this \
         one: {parked}"
    );

    step(
        8,
        "warm the restarted lender again, so it has a MEASURED window to lend from",
    );
    // A restart takes the fleet's utilization with it: `Ledger::may_relay`
    // refuses on an ABSENT measurement (`OwnerGuard`), and the warm-up in step
    // 2 measured a process that is no longer running. This is the same request
    // as step 2 and it is here for the same reason.
    let (rewarm, _) = post_messages(&lender.proxy).await;
    assert_eq!(rewarm, 200, "the restarted lender serves its own request");

    step(
        9,
        "the borrow is SERVED, over the socket the lender parked, and through the fallback \
         provider the borrower's dry fleet installs for itself, inside the borrow timeout",
    );
    let started = Instant::now();
    let (status, _body) = post_messages(&borrower.proxy).await;
    let elapsed = started.elapsed();
    assert_eq!(
        status,
        200,
        "the only way in to this lender is the carrier it parked, and this is the whole \
         point of the reverse path: nothing else in this run can answer. Carrier tail:\n{}\n\
         Lender tail:\n{}",
        tail(&carrier.log(), 20),
        tail(&lender.log(), 20)
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "the borrow has to END, and it took {elapsed:?}: a forward with nowhere to go must \
         refuse rather than hold the request open"
    );

    // The carry was really ATTEMPTED, so this is a measurement of the reverse
    // topology and not of a borrower that gave up before it asked anyone: the
    // borrower's own line says its lender answered on no address of its own
    // and that a Mac it may ask to carry took the stream. What happens after
    // that, on the carrier, is the half this file exercises and
    // `tests/peer/reach/peer_forward.rs` gates in process, because the carrier's refusal
    // reaches its log through a per-address rate limit that an enrolment
    // refusal in the same run can spend.
    let attempted = borrower.wait_for_log("a Mac this node may ask to carry reached it");
    assert!(
        attempted.contains("via="),
        "the borrow has to have gone out through the carrier for this case to be about \
         reach at all: {attempted}"
    );

    borrower.shutdown();
    carrier.shutdown();
    lender.shutdown();
}

/// **A key carries every address that Mac answers at, and the joiner works
/// down the list.**
///
/// The real event this closes: a key minted on a Mac listening on `0.0.0.0`
/// carried `0.0.0.0`, and the friend's `tcr peer join` dialled its own
/// machine. The fix is a list, and a list is only worth carrying if the second
/// address is really tried when the first one refuses, which is what this
/// measures: the key handed to the guest below names a port nothing is
/// listening on FIRST and the host's real one second.
///
/// Two processes, because the claim is about the shipped binary: the joiner's
/// own fall-through is invisible from inside the library, and the line an
/// operator reads (`peer join: ok addr=…`) has to name the address that
/// answered rather than the one at the top of the key.
///
/// Watched red: with `connect_in_key_order` returning after its first
/// attempt, the join exits non-zero and the failure names the dead port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_key_falls_through_to_the_second_address() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut host = Mac::new("host", &upstream, "host-fake", "at-fake-host", false);
    let mut guest = Mac::new("guest", &upstream, "guest-fake", "at-fake-guest", false);

    step(1, "boot the two tcr processes");
    host.boot();
    guest.boot();

    step(
        2,
        "the host mints a key, which carries the address it listens on",
    );
    // `--plain`: this test doctors the address text inside the key, which
    // only the readable v2 spelling carries; the opaque v3 one this build
    // mints by default is not what step 3 below is about.
    let minted = host.peer_ok(&["invite", "--label", "guest-mac", "--ttl", "300", "--plain"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    let live = format!("127.0.0.1:{}", host.peer_port);
    assert!(
        key.contains(&live),
        "the key has to carry the address the host really listens on: {key}"
    );
    assert!(
        minted.contains(&format!("peer invite: chosen {live}")),
        "the mint names each address it carried: {minted}"
    );

    step(
        3,
        "put a port nothing is listening on at the FRONT of the key's address list",
    );
    // Borrowed and released, so a connect to it is refused rather than
    // answered by something this test did not start.
    let dead = format!("127.0.0.1:{}", free_loopback_port());
    let doctored = key.replacen(&live, &format!("{dead},{live}"), 1);
    assert_ne!(doctored, key, "the fixture has to differ from the real key");

    step(
        4,
        "the guest joins: the first address refuses, the second answers",
    );
    let stdout = join_with_stdin(&guest, &doctored);
    assert!(
        stdout.contains(&format!("peer join: ok addr={live}")),
        "the line an operator reads must name the address that ANSWERED, not the first \
         one in the key: {stdout}"
    );
    assert!(
        !stdout.contains(&dead),
        "the dead address is not what this join ran over: {stdout}"
    );

    step(5, "and both Macs hold a pinned row for the other");
    assert_eq!(
        guest.pinned_nodes().len(),
        1,
        "the joiner pins the registrar it enrolled with"
    );
    assert_eq!(
        host.pinned_nodes().len(),
        1,
        "and the registrar pins the joiner"
    );

    guest.shutdown();
    host.shutdown();
}
