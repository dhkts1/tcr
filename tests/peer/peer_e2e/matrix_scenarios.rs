//! The E2E-MATRIX scenarios: the shapes the ring and the forward tests do not
//! cover, each one scenario, red first.

use super::*;

// ---------------------------------------------------------------------------
// E2E-MATRIX: four shapes the ring and the forward tests
// above do not cover, each one scenario, red first.
// ---------------------------------------------------------------------------

/// `tcr peer ls --json`'s `until` for one Mac's row on `node`.
///
/// Only meaningful against a grant minted with `--for` or `--until`:
/// `Lease::until` is copied straight from the GRANT's own end
/// (`src/peer/lease/mod.rs`'s `grant()`, `let end = granted.as_ref().and_then(|g|
/// g.until)`), so an open-ended grant (no `--for`, what [`lend_to`] mints)
/// reads `until: null` REGARDLESS of whether a cached lease is live. A
/// scenario that wants "is spender's cached lease for this row still there"
/// as an observable must lend with an explicit end; see
/// [`lend_to_for_a_bounded_time`].
fn until_for(mac: &Mac, node: &str) -> Option<u64> {
    let ls = mac.ls();
    let rows = ls["peers"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: no peers array: {ls}", mac.label));
    let row = rows
        .iter()
        .find(|row| row["node"].as_str() == Some(node))
        .unwrap_or_else(|| panic!("{}: no pinned row for {node}: {ls}", mac.label));
    row["until"].as_u64()
}

/// [`lend_to`] with an explicit end (`--for`), so the lease it mints carries a
/// real `until` an observer can read with [`until_for`] rather than the
/// permanent `null` an open-ended grant produces. Otherwise identical: same
/// five CLI steps, same fraction, same `max-inflight`.
fn lend_to_for_a_bounded_time(lender: &Mac, borrower: &Mac, label: &str, for_: &str) -> String {
    lend_to_with(lender, borrower, label, &["--for", for_])
}

/// Rewrite `mac`'s row for `node` to hold exactly one endpoint: `addr`, a
/// direct locator observed just now. Used to put an address on the row that
/// this test controls rather than one a real dial happened to leave there,
/// [`strip_endpoints`]'s sibling, writing a value in rather than emptying it.
fn set_direct_endpoint(mac: &Mac, node: &str, addr: &str) {
    let body = std::fs::read_to_string(&mac.peers).expect("the peers file reads back");
    let mut file: serde_json::Value = serde_json::from_str(&body).expect("the peers file is JSON");
    let rows = file["peers"]
        .as_array_mut()
        .expect("the peers file has a peers array");
    let row = rows
        .iter_mut()
        .find(|row| row["node"].as_str() == Some(node))
        .unwrap_or_else(|| panic!("{}: no pinned row for {node}", mac.label));
    row["endpoints"] = serde_json::json!([{
        "kind": "direct",
        "addr": addr,
        "observedAtMs": 1_700_000_000_000_i64,
        "source": "paired",
    }]);
    std::fs::write(
        &mac.peers,
        format!(
            "{}\n",
            serde_json::to_string(&file).expect("it re-serializes")
        ),
    )
    .expect("the peers file rewrites");
    std::fs::set_permissions(
        &mac.peers,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("the peers file takes 0600");
}

/// **No global IPv6: the recorded endpoint is IPv6 and unreachable, and the
/// borrow still completes, through the forwarder.**
///
/// This machine has no global IPv6 address (loopback-only CI, `reach.rs`'s own
/// module doc says a global v6 address is what lets two Macs skip a relay
/// entirely), so a row whose only direct endpoint is an IPv6 socket is
/// exactly the shape `dial_peer_reaching_within` (`src/peer/serve/mod.rs`)
/// must fall through on, the same way it falls through a stale IPv4 endpoint
/// in `a_forwarded_borrow_reaches_the_lender_through_a_third_mac` above. The
/// difference from that test is deliberate: there the row has NO address at
/// all; here it has one, on a family this box cannot use to reach it, so the
/// direct attempt must fail fast (connection refused on `::1`, not a timeout)
/// before the forwarder loop runs.
///
/// Watch it fail by deleting the `for forwarder in forwarders_for(...)` loop
/// in `dial_peer_reaching_within` (`src/peer/serve/mod.rs`): the IPv6 attempt
/// still fails, nothing else is tried, and this test times out at 429 instead
/// of reaching 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn no_ipv6_endpoint_falls_through_to_a_forwarder() {
    let (upstream, seen) = spawn_upstream().await;
    let (mut lender, mut carrier, mut borrower) = boot_lender_carrier_borrower(&upstream);

    step(2, "warm the lender's own account");
    warm(&lender).await;

    step(3, "borrower joins lender and is granted a lease");
    let lender_at_borrower = lend_to(&lender, &borrower, "borrower-mac");

    step(4, "borrower joins carrier and both grants are set");
    let (borrower_at_carrier, carrier_at_borrower) = join_to(&carrier, &borrower, "borrower-mac");
    borrower.peer_ok(&["allow", &carrier_at_borrower, "carry", "on"]);
    carrier.peer_ok(&["allow", &borrower_at_carrier, "forward", "on"]);

    step(
        5,
        "lender joins carrier and says hello, so carrier can forward to it",
    );
    let (_lender_at_carrier, carrier_at_lender) = join_to(&carrier, &lender, "lender-mac");
    let hello = lender.peer_ok(&["hello", &carrier_at_lender]);
    assert!(
        hello.contains("peer hello: ok"),
        "the hello round trip must confirm: {hello}"
    );

    step(
        6,
        "the lender's row on borrower is rewritten to an IPv6-only endpoint no route on this \
         box can reach: no global IPv6, and this address is loopback-only IPv6, so the direct \
         attempt fails fast rather than timing out",
    );
    set_direct_endpoint(&borrower, &lender_at_borrower, "[::1]:1");
    let rewritten = endpoints_for(&borrower, &lender_at_borrower);
    assert_eq!(
        rewritten.len(),
        1,
        "the row must carry exactly the one IPv6 endpoint this step wrote: {rewritten:?}"
    );
    assert_eq!(
        rewritten[0]["addr"].as_str(),
        Some("[::1]:1"),
        "and it must be the IPv6 socket, not a IPv4 one left over from the join: {rewritten:?}"
    );

    step(
        7,
        "the borrow is served anyway, through the carrier, on the lender's credential and \
         the fallback provider the dry fleet installs for itself",
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
        "the borrowed request must fall through the dead IPv6 endpoint to the carrier. \
         Borrower tail:\n{}\nCarrier tail:\n{}\nLender tail:\n{}",
        tail(&borrower.log(), 14),
        tail(&carrier.log(), 14),
        tail(&lender.log(), 14)
    );
    let credentials = seen.credentials();
    assert!(
        credentials.len() > before,
        "the request never reached the upstream: {credentials:?}"
    );
    assert_eq!(
        credentials.last().map(String::as_str),
        Some("Bearer at-fake-lender"),
        "the answer must be on the LENDER's token, proving it reached the lender and was not \
         answered by the carrier: {credentials:?}"
    );
    // The carrier writes this line when the carried stream CLOSES, which is
    // after the borrower already holds its 200, so the line is waited for and
    // never read once. Without it the borrow found some other route and this
    // test is not measuring a forward; the wait panics with the carrier's tail.
    carrier.wait_for_log("peer forward: carried");
    println!(
        "{}",
        matrix::matrix_line(
            "no_ipv6",
            "IPv6-only direct endpoint fell through to the carrier and served on the lender's \
             credential"
        )
    );

    borrower.shutdown();
    carrier.shutdown();
    lender.shutdown();
}

/// **A forwarder killed mid-borrow releases the client instead of hanging to
/// the deadline.**
///
/// The topology is the forwarded-borrow one, with the lender's own endpoint
/// stripped so every borrow must go through `carrier`. The upstream answers
/// slowly (1.5s), so there is a real window between the borrower opening the
/// forwarded stream and the reply landing; `carrier` is killed inside that
/// window, once the upstream has recorded the borrowed request on the lender's
/// token (never on a timer, which can fire while the lease is still being
/// asked for). `borrow_once` (`src/peer/serve/mod.rs`) is mid-stream against a socket
/// whose other end just vanished, so the read must fail with an I/O error
/// almost immediately, and that is the case [`open_serve_within`]'s own doc
/// distinguishes from a lender that is merely slow: that one is `Ok(None)` at
/// the 10-second deadline, this one is `Err` well inside it, and
/// `PeerLeaseProvider::try_serve` logs the two differently
/// (`"peer lease: the SERVE stream failed"` only on the `Err` arm). That log
/// line is this test's named refusal.
///
/// Watch it fail by deleting the `tracing::warn!` call on the `Err(err)` arm
/// of `try_serve`'s match on `serve::open_serve` (`src/peer/lease/mod.rs`): the
/// borrow still fails, but the line this test greps for is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn forwarder_killed_mid_borrow_releases_the_client_promptly() {
    let (upstream, seen) = spawn_slow_upstream(Duration::from_millis(1500)).await;
    let (mut lender, mut carrier, mut borrower) = boot_lender_carrier_borrower(&upstream);

    step(2, "warm the lender's own account against the slow upstream");
    warm(&lender).await;

    step(3, "borrower joins lender and is granted a lease");
    let lender_at_borrower = lend_to(&lender, &borrower, "borrower-mac");

    step(
        4,
        "borrower joins carrier: carry / forward, both grants explicit",
    );
    let (borrower_at_carrier, carrier_at_borrower) = join_to(&carrier, &borrower, "borrower-mac");
    borrower.peer_ok(&["allow", &carrier_at_borrower, "carry", "on"]);
    carrier.peer_ok(&["allow", &borrower_at_carrier, "forward", "on"]);

    step(5, "lender joins carrier and says hello");
    let (_lender_at_carrier, carrier_at_lender) = join_to(&carrier, &lender, "lender-mac");
    let hello = lender.peer_ok(&["hello", &carrier_at_lender]);
    assert!(
        hello.contains("peer hello: ok"),
        "the hello round trip must confirm: {hello}"
    );

    step(
        6,
        "the lender's addresses go stale on borrower's row: every borrow must go via carrier",
    );
    strip_endpoints(&borrower, &lender_at_borrower);
    assert!(
        endpoints_for(&borrower, &lender_at_borrower).is_empty(),
        "the row must be addressless before the kill, or a direct dial could still answer"
    );

    step(
        7,
        "start one borrow against the slow upstream, then kill carrier while it is in flight",
    );
    // The kill waits for the borrowed request to reach the upstream on the
    // lender's token, which the slow upstream records before it holds the
    // answer for 1.5s. A fixed sleep here raced the lease ask instead: on a
    // loaded CI runner the carrier died before the lease was granted, the
    // borrower logged `asking for a lease failed` and answered 429 without
    // any SERVE stream ever opening, and neither line asserted below exists.
    let before = seen.requests();
    let start = Instant::now();
    let proxy = borrower.proxy.clone();
    let task = tokio::spawn(async move { post_messages(&proxy).await });
    let arrival_deadline = Instant::now() + LINE_TIMEOUT;
    while !seen.saw_since(before, "/v1/messages", "Bearer at-fake-lender") {
        assert!(
            !task.is_finished(),
            "the borrow ended before it reached the upstream through carrier, so carrier was \
             never killed mid-borrow. Borrower tail:\n{}\nCarrier tail:\n{}",
            tail(&borrower.log(), 20),
            tail(&carrier.log(), 20)
        );
        assert!(
            Instant::now() < arrival_deadline,
            "the borrow never reached the upstream on the lender's token within \
             {LINE_TIMEOUT:?}. Borrower tail:\n{}\nCarrier tail:\n{}\nLender tail:\n{}",
            tail(&borrower.log(), 20),
            tail(&carrier.log(), 20),
            tail(&lender.log(), 20)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let in_flight_after = start.elapsed();
    carrier.shutdown();
    let (status, _body) = task.await.expect("the borrow task panicked");
    let elapsed = start.elapsed();
    println!(
        "         borrow status={status} elapsed={elapsed:?} (in flight at the upstream after \
         {in_flight_after:?}, released {:?} after the kill)",
        elapsed.saturating_sub(in_flight_after)
    );

    assert_ne!(
        status, 200,
        "with carrier dead mid-stream and the lender's own row addressless, nothing could have \
         served this request"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "a forwarder that dies mid-stream must release the client promptly, not ride out the \
         full {BORROW_TIMEOUT_SECS}s borrow deadline as though the lender were merely slow: \
         took {elapsed:?}"
    );
    let borrower_log = borrower.log();
    // EITHER line is the stream failing, and which one depends on whether the
    // body had crossed when the carrier died. The distinction this assertion
    // exists for is the other one: a stream that FAILED, against the
    // deadline-reached line a slow-but-alive lender would produce.
    //
    // The delivered line is the one this fixture takes, since the kill waits
    // for the body to reach the upstream, and it is not
    // the SERVE-stream-failed line any more: a borrow whose body has crossed
    // stops the ladder rather than offering the same request to the next
    // lender, so it is reported as delivered-and-unknown. See
    // `peer::serve::Borrowed`.
    let failed = [
        "peer lease: the SERVE stream failed",
        "the borrowed request was delivered and the exchange then failed",
    ]
    .into_iter()
    .any(|line| borrower_log.contains(line));
    assert!(
        failed,
        "the borrower must log the SERVE-stream failure this test kills carrier to cause, not \
         the deadline-reached line a slow-but-alive lender would produce. Tail:\n{}",
        tail(&borrower_log, 30)
    );
    println!(
        "{}",
        matrix::matrix_line(
            "forwarder_down_mid_borrow",
            "carrier killed mid-stream; borrower released promptly with a named SERVE-stream \
             refusal"
        )
    );

    borrower.shutdown();
    lender.shutdown();
}

/// Mirrors `BORROW_TIMEOUT` (`src/peer/serve/mod.rs`), kept as a named constant
/// here rather than a bare `10` in the assertion above, since this test's
/// whole point is proving the release happens well inside it.
const BORROW_TIMEOUT_SECS: u64 = 10;

/// **A lender that vanishes: the next borrow re-asks rather than hanging on a
/// stale lease, and the eighth-lease cap is not left in a bad state by the
/// crash.**
///
/// `bank` lends to `spender`, one request is served (warming the cache and
/// `until` on `spender`'s own `peer ls --json`), then `bank` is killed:
/// `Mac::shutdown` is `child.kill()`, so this is a crash, not a graceful stop.
/// `spender`'s next request has nothing to dial and must be refused; the case
/// this test pins is that the failure also DROPS the cached lease
/// (`PeerLeaseProvider::try_serve`'s `Ok(None)` arm in `src/peer/lease/mod.rs`),
/// which is what "re-asks" means operationally: `until` reads back `null`
/// rather than a lease `spender` can never spend again. `bank` then reboots on
/// the same port (its identity, in its own scratch HOME, is untouched by the
/// kill) with a FRESH in-memory ledger: `held` for `spender` is back to zero,
/// and `spender`'s next request must be served again, which is the re-ask
/// actually completing. The eighth-lease cap
/// ([`MAX_LEASES_PER_PEER`](../../../src/peer/lease/mod.rs), `src/peer/lease/mod.rs:472`)
/// is asserted by ABSENCE: `bank`'s fresh boot log must never contain the
/// cap's own refusal line, because a ledger that came back corrupted (still
/// counting the pre-crash lease as live) is exactly the failure mode that
/// would produce it here, against a single well-behaved borrower.
///
/// Watch it fail by deleting the cache-drop block (`let dropped = ...
/// self.persist_borrowed();`) from the `Ok(None)` arm of `try_serve`'s match
/// on `serve::open_serve` (`src/peer/lease/mod.rs`): `until` stays non-null after
/// the kill, because nothing ever told `spender` the lease was dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_vanished_lenders_next_borrow_re_asks_and_the_cap_is_not_corrupted() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut bank = Mac::new("bank", &upstream, "bank-fake", "at-fake-bank", false);
    let mut spender = Mac::new(
        "spender",
        &upstream,
        "spender-fake",
        "at-fake-spender",
        true,
    );

    step(
        1,
        "boot two tcr processes: bank (lender) and spender (borrower)",
    );
    bank.boot();
    spender.boot();

    step(2, "warm bank's own account");
    warm(&bank).await;

    step(
        3,
        "spender joins bank and is granted a lease that ends in 30 minutes",
    );
    let bank_at_spender = lend_to_for_a_bounded_time(&bank, &spender, "spender-mac", "30m");

    step(4, "spender restarts, installing the fallback");
    spender.shutdown();
    spender.boot();
    let installed = spender.wait_for_log("peer-lease fallback");
    assert!(
        installed.contains("outcome=Yes"),
        "boot must install the fallback: {installed}"
    );

    step(5, "one request is served, warming spender's cached lease");
    let (first, _) = post_messages(&spender.proxy).await;
    assert_eq!(
        first, 200,
        "the first borrow must be served while bank is alive"
    );
    let until_before = until_for(&spender, &bank_at_spender);
    assert!(
        until_before.is_some(),
        "a served borrow must leave a live `until` on spender's row for bank: {until_before:?}"
    );

    step(
        6,
        "bank is killed mid-lease, a crash and not a graceful stop",
    );
    bank.shutdown();

    step(
        7,
        "spender's next request is refused, and the cached lease is dropped",
    );
    let (refused, _) = post_messages(&spender.proxy).await;
    assert_ne!(
        refused, 200,
        "nothing can serve this request with bank dead"
    );
    let until_after_kill = until_for(&spender, &bank_at_spender);
    assert_eq!(
        until_after_kill, None,
        "a refused borrow against a dead lender must drop the cached lease, or spender's next \
         attempt would spend a lease bank no longer holds any record of: {until_after_kill:?}"
    );

    step(
        8,
        "bank reboots on the same port, with a fresh in-memory ledger",
    );
    bank.boot();
    let rebooted = bank.wait_for_log("listening on http://");
    assert!(
        rebooted.contains("listening"),
        "bank must come back up: {rebooted}"
    );

    step(
        9,
        "bank warms its own account again: a fresh boot's headroom ticker reads 0.0 until \
         measured, and an unwarmed bank refuses every ask OwnerGuard regardless of the cap",
    );
    let (rewarm, _) = post_messages(&bank.proxy).await;
    assert_eq!(
        rewarm, 200,
        "bank must serve its own request again after rebooting"
    );

    step(
        10,
        "spender's next request re-asks bank fresh and is served again",
    );
    let mut result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        result = post_messages(&spender.proxy).await;
        println!("         spender attempt {attempt} -> {}", result.0);
        if result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    assert_eq!(
        result.0,
        200,
        "the re-ask must succeed once bank is back. Spender tail:\n{}\nBank tail:\n{}",
        tail(&spender.log(), 20),
        tail(&bank.log(), 20)
    );
    let until_after_reask = until_for(&spender, &bank_at_spender);
    assert!(
        until_after_reask.is_some(),
        "the re-ask must leave a fresh live `until`: {until_after_reask:?}"
    );

    let bank_log = bank.log();
    assert!(
        !bank_log.contains("this peer already holds the most leases"),
        "bank's FRESH ledger must not refuse spender's one legitimate re-ask with the \
         eighth-lease cap; that would mean the crash left the cap in a corrupted state rather \
         than a clean one. Bank tail:\n{}",
        tail(&bank_log, 30)
    );
    println!(
        "{}",
        matrix::matrix_line(
            "lender_vanishes",
            "kill dropped the cached lease, the re-ask after reboot was served, and the \
             eighth-lease cap was not spuriously tripped"
        )
    );

    spender.shutdown();
    bank.shutdown();
}

/// **A captive portal: every outbound request gets an HTTP 302 from a fake
/// gateway, and the keeper is expected to report "no network" without pinning
/// or writing anything.**
///
/// `asks_for_other_lanes`: no production code in this tree reports a "no
/// network" state at all. `spawn_mapping_keeper` (`src/peer/reach/mod.rs`)
/// only ever answers "no gateway responded" (silence) or a NAT-PMP mapping; a
/// captive portal answering every request with a 302 on the TCP link a
/// NAT-PMP UDP probe never touches would today read as the SAME "no gateway"
/// silence a plain unplugged cable produces, with no distinguishing state and
/// nothing a test can observe from outside the process, and `tcr peer reach`
/// has not landed in this tree, and until it
/// does there is no CLI surface this scenario could assert against, and no
/// state field it could read for "nothing is pinned or written" either. This
/// needs a connectivity read ("the keeper holds no
/// mapping and no global IPv6, read `reach`'s state") plus a captive-portal
/// classifier neither piece of work names as its own.
///
/// Left `#[ignore]` rather than deleted, so the file states the gap where a
/// reader looking for this scenario will look for it, and a green run of this
/// scenario one day is what its own `#[ignore]` should have removed.
#[tokio::test]
#[ignore = "needs_unbuilt_pieces: no keeper 'no network' state and no `tcr peer reach` verb \
            exist in this tree to assert a captive-portal HTTP 302 against; those and \
            src/peer/reach/mod.rs's MappingKeeper are not built yet"]
async fn a_captive_portal_reports_no_network_and_pins_nothing() {
    unimplemented!(
        "blocked on `tcr peer reach` (PUNCH) and the keeper's no-network read (REVERSE); see \
         this test's doc-comment"
    );
}
