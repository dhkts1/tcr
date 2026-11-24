//! The borrowing path: a borrowed request reaches the lender without the
//! borrower's credential, a chained borrow, inflight caps against one and two
//! lenders, and the comparison measurements.

use super::*;

// ---------------------------------------------------------------------------
// The borrowing path
// ---------------------------------------------------------------------------

/// **A borrowed request, end to end through the two processes**: enrol, share,
/// lend, borrow, forget.
///
/// The lender is warmed with one ordinary local request first, because a
/// lender with no measured window has no headroom to lend
/// (`Manager::lendable_fraction` answers `0.0` on an unmeasured window and
/// `Ledger::grant` reads the last note).
///
/// Step 7 asserts the served 200 through the lease arm (gap 4 in this file's
/// docs, `listener::serve_control`, `src/peer/listener.rs:1486`), with the
/// chain in front of it proven too: the provider installed itself for this
/// dry fleet with no restart, the borrower dialled the lender, the lender's
/// Noise handshake completed. Until LEASE-WIRE lands that arm in this tree,
/// this test is RED at step 7, with the borrower's dry-fleet ladder exhausted
/// at 429 in the failure message instead of served.
///
/// Watched red: `tests/tools/e2e/watch-peer-e2e-fail.sh` deletes the
/// `install_peer_lease_provider` block from `src/server.rs` and this fails at
/// step 7 with no `peer-lease fallback` line at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_borrowed_request_reaches_the_lender_and_never_carries_the_borrowers_credential() {
    let (upstream, seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot two tcr processes");
    lender.boot();
    borrower.boot();

    step(
        2,
        "one ordinary request on the lender, so its 7d window is measured",
    );
    warm(&lender).await;
    assert_eq!(
        seen.credentials(),
        vec!["Bearer at-fake-lender".to_string()],
        "the warm request must have gone out on the lender's own token"
    );

    step(
        3,
        "the lender mints a one-use join key; the borrower joins with it",
    );
    let minted = lender.peer_ok(&["invite", "--label", "borrower-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&borrower, &key);

    step(
        4,
        "both sides hold a pinned row, and the borrower's carries an address",
    );
    let lender_node = borrower.first_pinned_node();
    let borrower_node = lender.first_pinned_node();
    assert_ne!(
        lender_node, borrower_node,
        "the two Macs must have pinned each OTHER, not themselves"
    );
    let endpoints = borrower.ls()["peers"][0]["endpoints"].clone();
    assert_eq!(
        endpoints[0]["addr"].as_str(),
        Some(format!("127.0.0.1:{}", lender.peer_port).as_str()),
        "the joiner's row must name the address it enrolled through: {endpoints}"
    );
    assert_eq!(
        endpoints[0]["source"].as_str(),
        Some("paired"),
        "an enrolment writes the socket it arrived on, sourced `paired`: {endpoints}"
    );

    step(
        5,
        "share on, then one lease grant for that peer, on the lender",
    );
    let shared = lender.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.20"]);
    assert!(
        shared.contains("peer share: on peers=1"),
        "share must say what it changed: {shared}"
    );
    assert!(
        shared.contains("READ those requests in full"),
        "share must name the disclosure it grants: {shared}"
    );
    let lent = lender.peer_ok(&[
        "lend",
        &borrower_node,
        "--window",
        "7d",
        "--fraction",
        "0.20",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    assert!(
        lent.contains("peer lend: ok") && lent.contains("window=7d"),
        "lend must confirm the grant: {lent}"
    );
    let grants = lender.ls()["peers"][0]["lend"].clone();
    assert_eq!(
        grants.as_array().map(Vec::len),
        Some(1),
        "the lender's row must carry exactly one grant: {grants}"
    );

    step(
        6,
        "disclose on, on the borrower: the other half of the lease",
    );
    let disclosed = borrower.peer_ok(&["allow", &lender_node, "disclose", "on"]);
    assert!(
        disclosed.contains("grant=disclose state=on"),
        "allow must confirm the grant: {disclosed}"
    );

    step(
        7,
        "a request on the borrower's proxy, served through the lease the provider installs itself for",
    );
    let mut last = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        last = post_messages(&borrower.proxy).await;
        println!("         borrow attempt {attempt} -> {}", last.0);
        if last.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    // The chain in front of the answer: the borrower really reached the
    // lender's listener over Noise, whichever way the answer went.
    let asked = borrower.log();
    let reached_the_lender = asked.contains("peer lease: asking for a lease failed")
        || asked.contains("served through a fallback provider");
    assert!(
        reached_the_lender,
        "the borrower's dry-fleet arm never reached its provider at all. Tail:\n{}",
        tail(&asked, 12)
    );

    assert_eq!(
        last.0,
        200,
        "the lease arm must serve the borrowed request (LEASE-WIRE's \
         listener::serve_control Control::LeaseRequest arm, \
         src/peer/listener.rs:1486). Borrower tail:\n{}\nLender tail:\n{}",
        tail(&asked, 12),
        tail(&lender.log(), 12)
    );
    assert!(
        seen.requests() >= 2,
        "a served borrow must have reached the fake upstream a second time"
    );

    step(8, "the borrower's own credential never left the borrower");
    let credentials = seen.credentials();
    assert!(
        !credentials
            .iter()
            .any(|seen| seen.contains("at-fake-borrower")),
        "the borrower's token reached the upstream, which is the one thing this \
         whole design forbids: {credentials:?}"
    );
    assert!(
        credentials
            .iter()
            .all(|seen| seen == "Bearer at-fake-lender"),
        "every request the upstream saw must be on the lender's own token: {credentials:?}"
    );

    step(9, "forget, and the next request is today's 429");
    let forgotten = lender.peer_ok(&["forget", &borrower_node]);
    assert!(
        forgotten.contains("peer forget: ok"),
        "forget must confirm: {forgotten}"
    );
    let (after, body) = post_messages(&borrower.proxy).await;
    assert_eq!(
        after, 429,
        "a forgotten borrower gets the honest 429 the ladder always had"
    );
    assert!(
        body.contains("exhausted"),
        "and it is the exhausted-fleet body: {body}"
    );
    // The refusal is the lender's, and it names the pin check: which is what
    // makes this 429 different from the one above it.
    let refusal = lender.wait_for_log("peer connection refused before it authenticated");
    assert!(
        refusal.contains("is not pinned on this node"),
        "the lender must refuse the forgotten peer at the pin check: {refusal}"
    );

    lender.shutdown();
    borrower.shutdown();
}

// ---------------------------------------------------------------------------
// N=3: chained borrow, two-hop carry, and inflight contention across two
// lenders
//
// Each test below stands up its own fixed three-Mac fleet rather than reading
// `TCR_E2E_NODES`: they need a specific topology (a middle node that is both
// a lender and a borrower, or two lenders and one borrower), not "whatever N
// the ring happens to be", so they run every time this binary runs,
// independent of the ring test's env var.
// ---------------------------------------------------------------------------

/// **Chained borrow**: `root` lends to `middle`, and `middle`, separately,
/// lends to `leaf`: two independent lease relationships, in a chain, proven
/// not to interfere with each other. First `leaf`'s request is served and the
/// upstream sees exactly `middle`'s own credential for it (the pair
/// (`middle`, `leaf`) works exactly like the two-process borrow test above).
/// Then `middle`'s own account is durably rejected by the upstream, `middle`'s
/// own local fleet has nothing left to serve with, and `middle`'s OWN request
/// falls through to ITS fallback provider (`root`), so the upstream sees
/// exactly `root`'s credential for that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chained_borrow_leaf_from_middle_middle_from_root() {
    let middle_credential = "Bearer at-fake-middle".to_string();
    let (upstream, seen, trip_middle) = spawn_upstream_with_trip(middle_credential).await;

    let mut root = Mac::new("root", &upstream, "root-fake", "at-fake-root", false);
    let mut middle = Mac::new("middle", &upstream, "middle-fake", "at-fake-middle", false);
    let mut leaf = Mac::new("leaf", &upstream, "leaf-fake", "at-fake-leaf", true);

    step(1, "boot three tcr processes: root, middle, leaf");
    root.boot();
    middle.boot();
    leaf.boot();

    step(
        2,
        "warm root's own account, so it has a measured window to lend from",
    );
    warm(&root).await;

    step(3, "middle joins root and borrows from it");
    let minted = root.peer_ok(&["invite", "--label", "middle-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&middle, &key);
    let root_node = middle.first_pinned_node();
    let middle_node_at_root = root.first_pinned_node();
    root.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.20"]);
    root.peer_ok(&[
        "lend",
        &middle_node_at_root,
        "--window",
        "7d",
        "--fraction",
        "0.20",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    middle.peer_ok(&["allow", &root_node, "disclose", "on"]);
    middle.shutdown();
    middle.boot();
    let installed = middle.wait_for_log("peer-lease fallback");
    assert!(
        installed.contains("outcome=Yes"),
        "middle's boot must install a fallback now that it has a lender (root): {installed}"
    );

    step(
        4,
        "warm middle's own account too, so IT has a measured window to lend from",
    );
    let (warm_middle, _) = post_messages(&middle.proxy).await;
    assert_eq!(
        warm_middle, 200,
        "middle must serve its own request before it can lend a fraction of it"
    );

    step(5, "leaf joins middle and borrows from it");
    let minted = middle.peer_ok(&["invite", "--label", "leaf-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&leaf, &key);
    let middle_node_at_leaf = leaf.first_pinned_node();
    let leaf_node = middle.first_pinned_node();
    middle.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.20"]);
    middle.peer_ok(&[
        "lend",
        &leaf_node,
        "--window",
        "7d",
        "--fraction",
        "0.20",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    leaf.peer_ok(&["allow", &middle_node_at_leaf, "disclose", "on"]);
    leaf.shutdown();
    leaf.boot();
    let installed_leaf = leaf.wait_for_log("peer-lease fallback");
    assert!(
        installed_leaf.contains("outcome=Yes"),
        "leaf's boot must install a fallback now that it has a lender (middle): {installed_leaf}"
    );

    step(
        6,
        "leaf's request is served, and the upstream sees exactly middle's own credential",
    );
    let before = seen.requests();
    let mut leaf_result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        leaf_result = post_messages(&leaf.proxy).await;
        println!("         leaf attempt {attempt} -> {}", leaf_result.0);
        if leaf_result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    assert_eq!(
        leaf_result.0,
        200,
        "leaf's borrowed request must be served through middle. Tail:\n{}",
        tail(&leaf.log(), 12)
    );
    let credentials = seen.credentials();
    assert!(
        credentials[before..]
            .iter()
            .all(|credential| credential == "Bearer at-fake-middle"),
        "every request the upstream saw while serving leaf must be on middle's own \
         token: {credentials:?}"
    );

    step(
        7,
        "middle's own account is durably rejected by the upstream",
    );
    trip_middle.store(true, Ordering::SeqCst);

    step(
        8,
        "middle's own next request falls through to its OWN fallback (root), and \
         the upstream sees root's credential",
    );
    let before = seen.requests();
    let mut middle_result = (0_u16, String::new());
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        middle_result = post_messages(&middle.proxy).await;
        println!("         middle attempt {attempt} -> {}", middle_result.0);
        if middle_result.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    let middle_log = middle.log();
    let reached_fallback = middle_log.contains("peer lease: asking for a lease failed")
        || middle_log.contains("served through a fallback provider");
    assert!(
        reached_fallback,
        "middle's own request never reached its own fallback provider. Tail:\n{}",
        tail(&middle_log, 12)
    );
    assert_eq!(
        middle_result.0,
        200,
        "middle's own request must fall through to root once middle's own account is \
         durably rejected. Tail:\n{}\nRoot tail:\n{}",
        tail(&middle_log, 12),
        tail(&root.log(), 12)
    );
    // `credentials[before..]` covers the whole ladder for this ONE request:
    // the rejected attempt on middle's own token (the 429 that tripped the
    // durable rejection) comes first, then the served attempt through root.
    // Only the LAST entry is what actually served the client, so that is what
    // gets the "on root's own token" claim; the earlier middle entry is
    // asserted separately, naming the sequence rather than asserting "all".
    let credentials = seen.credentials();
    assert!(
        credentials.len() > before,
        "middle's own overflow must have reached the upstream at all: {credentials:?}"
    );
    assert_eq!(
        credentials.last().map(String::as_str),
        Some("Bearer at-fake-root"),
        "the request that actually served middle's own client must be on root's own \
         token: {credentials:?}"
    );
    assert!(
        credentials[before..credentials.len() - 1]
            .iter()
            .all(|credential| credential == "Bearer at-fake-middle"),
        "every rejected attempt before the fallback served must have been on middle's \
         own (durably rejected) token: {credentials:?}"
    );

    leaf.shutdown();
    middle.shutdown();
    root.shutdown();
}

/// **Five concurrent borrows against one lender, capped at `max_inflight:
/// 2`.** `spender` is a dry-fleet Mac pinned to `bank1` alone: the brief for
/// this item asked for a SECOND lender (`bank2`) as overflow, and the
/// exploration for this test built exactly that topology first and found a
/// real race that made it the wrong control, reported here rather than
/// hidden: [`PeerLeaseProvider::lease_for`] (`src/peer/lease.rs:1455`) checks
/// its lease cache and only mints+caches a NEW lease on a miss; the check and
/// the mint are two steps with an `.await` between them and no lock held
/// across it, so N concurrent Asks that all see the SAME cache MISS each mint
/// their OWN fresh lease with its OWN fresh `lease_id`, and `max_inflight` is
/// enforced PER `lease_id` (`Ledger::enter_relay`): never per lender, never
/// per borrower. Measured live: three of five concurrent first-asks refused
/// by `bank1`'s cached lease (`InFlightFull` in the log, proving THIS
/// invariant works) fell through to `bank2` and every one of the three
/// independently raced a cache miss there, minted three DIFFERENT leases, and
/// all three were served: five of five succeeded, zero refused, which would
/// have been this codebase's own DNA's "vacuously-passing control" if it had
/// shipped as the gate. So this test primes ONE lease first (sequential, so
/// there is exactly one cached `lease_id` to contend on) and never joins
/// `bank2` at all, which is what makes "at most 2 of 5 may be served at once"
/// a claim about `max_inflight`, and not an artifact of how many lenders were
/// reachable to race across.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn five_concurrent_borrows_respect_max_inflight() {
    let (upstream, _seen) = spawn_slow_upstream(Duration::from_millis(1500)).await;
    let mut bank1 = Mac::new("bank1", &upstream, "bank1-fake", "at-fake-bank1", false);
    let mut spender = Mac::new(
        "spender",
        &upstream,
        "spender-fake",
        "at-fake-spender",
        true,
    );

    step(
        1,
        "boot two tcr processes: one lender (bank1), one borrower (spender)",
    );
    bank1.boot();
    spender.boot();

    step(2, "warm the lender's own account");
    warm(&bank1).await;

    step(
        3,
        "spender joins bank1 and borrows a max_inflight:2 lease from it",
    );
    let minted = bank1.peer_ok(&["invite", "--label", "spender-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&spender, &key);
    let bank1_node_at_spender = spender.first_pinned_node();
    let spender_node = bank1.first_pinned_node();
    bank1.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.90"]);
    bank1.peer_ok(&[
        "lend",
        &spender_node,
        "--window",
        "7d",
        "--fraction",
        "0.90",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    spender.peer_ok(&["allow", &bank1_node_at_spender, "disclose", "on"]);
    spender.shutdown();
    spender.boot();
    let installed = spender.wait_for_log("peer-lease fallback");
    assert!(
        installed.contains("outcome=Yes"),
        "spender's boot must install a fallback now that it has a lender: {installed}"
    );

    step(
        4,
        "one priming request, sequential, so spender caches ONE lease against bank1 \
         before any concurrency is asked of it",
    );
    let (primed, _) = post_messages(&spender.proxy).await;
    assert_eq!(
        primed, 200,
        "the priming request must be served, so the lease it caches is real"
    );

    step(
        5,
        "five concurrent borrows against that ONE cached lease: at most 2 (its \
         max_inflight) may be served at once",
    );
    let statuses = five_concurrent_borrows(&spender.proxy).await;

    let served = statuses.iter().filter(|status| **status == 200).count();
    let refused = statuses.len() - served;
    assert_eq!(statuses.len(), 5, "must have five results, one per task");
    assert!(
        served <= 2,
        "at most 2 of 5 concurrent borrows against one cached lease \
         (max_inflight 2) may be served at once: {statuses:?}"
    );
    assert!(
        refused >= 1,
        "at least one of the five must be refused for max_inflight to be the thing \
         under test here: {statuses:?}"
    );

    let spender_log = spender.log();
    assert!(
        spender_log.contains("InFlightFull"),
        "the refused borrow(s) must be logged with the max_inflight wire word \
         (LeaseRefusal::InFlightFull). Tail:\n{}",
        tail(&spender_log, 30)
    );

    spender.shutdown();
    bank1.shutdown();
}

// ---------------------------------------------------------------------------
// Comparison measurements
//
// These print `measure: <name> <value> <unit>` lines that
// `scripts/peer-compare-local.sh` greps; they are `#[ignore]`d because they
// are wall-clock measurements, not correctness gates: the borrow path's
// correctness is `a_borrowed_request_reaches_the_lender_...` above.
// ---------------------------------------------------------------------------

/// The value at `pct` (0..=100) in `values`, nearest-rank. Panics on an empty
/// slice rather than returning a made-up number for zero samples.
fn percentile(values: &[f64], pct: usize) -> f64 {
    assert!(
        !values.is_empty(),
        "cannot take a percentile of zero samples"
    );
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaNs in a wall-clock sample"));
    let rank = (pct * (sorted.len() - 1)) / 100;
    sorted[rank]
}

/// 50 requests served locally on the lender's own proxy versus 50 borrowed
/// through the lender via the borrower's proxy, through the same two-process
/// setup `a_borrowed_request_reaches_the_lender_...` proves correct. Prints
/// p50/p95 of each leg and the delta.
///
/// If the borrowed leg cannot reach a served 200 (the LEASE-WIRE arm named in
/// this file's module docs, gap 4, not yet in the tree), that is reported as
/// `measure: latency_borrowed_blocked` with the <file:line> and status/body
/// that blocked it, and the local leg's numbers are still printed, this
/// test never fakes a borrowed-leg number and never implements the missing
/// arm itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "wall-clock measurement; run via scripts/peer-compare-local.sh"]
async fn measure_latency_local_vs_borrowed() {
    const SAMPLES: u32 = 50;

    let (upstream, _seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    lender.boot();
    borrower.boot();

    warm(&lender).await;

    let minted = lender.peer_ok(&["invite", "--label", "borrower-mac", "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(&borrower, &key);

    let lender_node = borrower.first_pinned_node();
    let borrower_node = lender.first_pinned_node();
    lender.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.20"]);
    lender.peer_ok(&[
        "lend",
        &borrower_node,
        "--window",
        "7d",
        "--fraction",
        "0.20",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ]);
    borrower.peer_ok(&["allow", &lender_node, "disclose", "on"]);

    println!("STEP: {SAMPLES} requests served locally on the lender");
    let mut local_ms = Vec::with_capacity(SAMPLES as usize);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let (status, _) = post_messages(&lender.proxy).await;
        assert_eq!(status, 200, "a locally served request must succeed");
        local_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    println!("STEP: priming the borrowed leg until it serves a 200");
    let mut primed = false;
    for attempt in 1..=BORROW_SERVED_ATTEMPTS {
        let (status, _) = post_messages(&borrower.proxy).await;
        if status == 200 {
            primed = true;
            break;
        }
        println!("         prime attempt {attempt} -> {status}");
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    println!(
        "measure: latency_local_p50 {:.2} ms",
        percentile(&local_ms, 50)
    );
    println!(
        "measure: latency_local_p95 {:.2} ms",
        percentile(&local_ms, 95)
    );

    if primed {
        println!("STEP: {SAMPLES} requests borrowed through the lender");
        let mut borrowed_ms = Vec::with_capacity(SAMPLES as usize);
        let mut blocked_at: Option<(u32, u16, String)> = None;
        for i in 1..=SAMPLES {
            let start = Instant::now();
            let (status, body) = post_messages(&borrower.proxy).await;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            if status != 200 {
                blocked_at = Some((i, status, body));
                break;
            }
            borrowed_ms.push(elapsed);
        }
        if let Some((i, status, body)) = blocked_at {
            println!(
                "measure: latency_borrowed_blocked request {i} of {SAMPLES} answered \
             status={status} body={body}"
            );
        } else {
            let borrowed_p50 = percentile(&borrowed_ms, 50);
            let borrowed_p95 = percentile(&borrowed_ms, 95);
            println!("measure: latency_borrowed_p50 {borrowed_p50:.2} ms");
            println!("measure: latency_borrowed_p95 {borrowed_p95:.2} ms");
            println!(
                "measure: latency_delta_p50 {:.2} ms",
                borrowed_p50 - percentile(&local_ms, 50)
            );
        }
    } else {
        println!(
            "measure: latency_borrowed_blocked src/peer/listener.rs:1486 \
             (listener::serve_control's Control::LeaseRequest arm) never reached a served 200 \
             within {BORROW_SERVED_ATTEMPTS} attempts"
        );
    }

    lender.shutdown();
    borrower.shutdown();
}

/// Proxy start-to-ready wall clock, ten boots with a `tcr-peers.json` present
/// and ten without, keyed on the same log line `Mac::boot` waits for
/// (`"tcr listening on http://"`, `src/main.rs:3732`). Prints the
/// median of each and the delta.
#[test]
#[ignore = "wall-clock measurement; run via scripts/peer-compare-local.sh"]
fn measure_boot_cost_with_and_without_peers_file() {
    const RUNS: u32 = 10;

    println!("STEP: {RUNS} boots with a peers file present");
    let with_peers: Vec<f64> = (0..RUNS).map(|_| boot_cost_once(true)).collect();
    println!("STEP: {RUNS} boots with no peers file");
    let without_peers: Vec<f64> = (0..RUNS).map(|_| boot_cost_once(false)).collect();

    let with_median = percentile(&with_peers, 50);
    let without_median = percentile(&without_peers, 50);
    println!("measure: boot_with_peers_median {with_median:.1} ms");
    println!("measure: boot_without_peers_median {without_median:.1} ms");
    println!("measure: boot_delta {:.1} ms", with_median - without_median);
}

/// One boot of the built `tcr`, `--headless --port 0`, timed from spawn to
/// the ready log line. `with_peers` controls whether a `tcr-peers.json` is
/// written into the scratch HOME before the boot.
fn boot_cost_once(with_peers: bool) -> f64 {
    let home = tempfile::tempdir().expect("a scratch HOME");
    let config_dir = home.path().join(".config");
    std::fs::create_dir_all(&config_dir).expect("the scratch .config");
    std::fs::write(
        config_dir.join("tcr.json"),
        r#"{"proxy": {"port": 0}, "accounts": []}"#,
    )
    .expect("the scratch config writes");
    if with_peers {
        let port = free_loopback_port();
        let peers = config_dir.join("tcr-peers.json");
        std::fs::write(
            &peers,
            format!("{{\"listen\":\"127.0.0.1:{port}\",\"peers\":[]}}\n"),
        )
        .expect("the scratch peers file writes");
        std::fs::set_permissions(&peers, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("the peers file takes 0600");
    }

    let log = home.path().join("boot.log");
    let out = std::fs::File::create(&log).expect("the boot log opens");
    let err = out.try_clone().expect("the boot log clones for stderr");
    let mut command = crate::tcr_env::tcr(home.path());
    command
        .args(["--headless", "--port", "0", "--no-replace"])
        // The same colour-off contract as `Mac::command`; this sample owns no
        // `Mac` to borrow it from.
        .env("NO_COLOR", "1")
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));

    let start = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|err| panic!("spawning the built tcr for a boot-cost sample: {err}"));

    let deadline = Instant::now() + LINE_TIMEOUT;
    loop {
        let body = strip_ansi(&std::fs::read_to_string(&log).unwrap_or_default());
        if body.contains("listening on http://") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no ready line within {LINE_TIMEOUT:?} (with_peers={with_peers}). Tail:\n{}",
            tail(&body, 12)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;

    child.kill().log_debug("killing a test child");
    child.wait().log_debug("reaping a test child");
    elapsed
}

/// **Five concurrent borrows against TWO lenders, each capped at
/// `max_inflight: 2`: at most four are served at once and the rest are
/// refused.**
///
/// This is the original topology as first written, and it is back
/// because the race that made it unwinnable is fixed. The exploration for
/// [`five_concurrent_borrows_respect_max_inflight`] built this exact topology
/// first and measured five served, zero refused:
/// `PeerLeaseProvider::lease_for` checked its lease cache, missed, released
/// the lock and only then asked the lender, so five concurrent first asks each
/// minted their OWN `lease_id`: and `max_inflight` is enforced per
/// `lease_id`, so the cap bound nothing across them. That test kept the gate
/// meaningful by priming ONE lease and using ONE lender; this one needs no
/// priming, because `lease_for` now single-flights the first ask per lender
/// and five concurrent borrows share the one lease each lender grants.
///
/// So the ceiling here is two lenders times `max_inflight: 2` (four), and it
/// is a claim about the borrower's ask path as much as about the lender's cap.
///
/// Watch it fail by deleting the ask-gate block in `lease_for`
/// (`src/peer/lease.rs`): five leases are minted at bank1 alone and all five
/// borrows are served.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn five_concurrent_borrows_across_two_lenders_stop_at_four() {
    let (upstream, _seen) = spawn_slow_upstream(Duration::from_millis(1500)).await;
    let mut bank1 = Mac::new("bank1", &upstream, "bank1-fake", "at-fake-bank1", false);
    let mut bank2 = Mac::new("bank2", &upstream, "bank2-fake", "at-fake-bank2", false);
    let mut spender = Mac::new(
        "spender",
        &upstream,
        "spender-fake",
        "at-fake-spender",
        true,
    );

    step(1, "boot three tcr processes: two lenders, one borrower");
    bank1.boot();
    bank2.boot();
    spender.boot();

    step(
        2,
        "warm both lenders' own accounts, so each has a window to lend from",
    );
    for lender in [&bank1, &bank2] {
        warm(lender).await;
    }

    step(
        3,
        "spender joins bank1, which lends it a max_inflight:2 lease",
    );
    let bank1_at_spender = lend_to(&bank1, &spender, "spender-mac-1");

    step(4, "spender joins bank2 as well, on the same terms");
    let bank2_at_spender = lend_to(&bank2, &spender, "spender-mac-2");
    assert_ne!(
        bank1_at_spender, bank2_at_spender,
        "two joins must leave two distinct pinned lenders"
    );

    step(
        5,
        "restart spender so the peer-lease fallback is installed with BOTH lenders",
    );
    spender.shutdown();
    spender.boot();
    let installed = spender.wait_for_log("peer-lease fallback");
    assert!(
        installed.contains("outcome=Yes"),
        "spender's boot must install a fallback now that it has lenders: {installed}"
    );

    step(
        6,
        "five concurrent borrows, no priming: two lenders times max_inflight 2 is \
         the ceiling",
    );
    let statuses = five_concurrent_borrows(&spender.proxy).await;

    let served = statuses.iter().filter(|status| **status == 200).count();
    let refused = statuses.len() - served;
    assert_eq!(statuses.len(), 5, "must have five results, one per task");
    assert!(
        served <= 4,
        "two lenders at max_inflight 2 may serve at most 4 of 5 concurrent \
         borrows; 5 means the borrower minted a lease per request and the cap \
         bound nothing: {statuses:?}\nSpender tail:\n{}",
        tail(&spender.log(), 30)
    );
    // The control on the instrument. A run where nothing was served is a run
    // where no lease was ever granted, and it would satisfy the ceiling above
    // while proving nothing about it.
    assert!(
        served >= 2,
        "at least one lender's lease must actually have served requests, or the \
         ceiling above is vacuous: {statuses:?}\nSpender tail:\n{}",
        tail(&spender.log(), 30)
    );
    assert!(
        refused >= 1,
        "the fifth borrow has no lender left with room and must be refused: \
         {statuses:?}"
    );

    let spender_log = spender.log();
    assert!(
        spender_log.contains("InFlightFull"),
        "the refused borrow(s) must be logged with the max_inflight wire word \
         (LeaseRefusal::InFlightFull). Tail:\n{}",
        tail(&spender_log, 30)
    );

    spender.shutdown();
    bank2.shutdown();
    bank1.shutdown();
}
