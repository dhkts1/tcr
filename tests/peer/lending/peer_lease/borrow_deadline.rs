// ---------------------------------------------------------------------------
// A borrow that crossed, a lender that went quiet, and the deadline that
// releases the client
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// **A lender that accepts and never answers releases the client**, and the
/// local picker gets its turn.
///
/// The review's finding at `serve.rs`: the borrow path had no
/// deadline anywhere, a bare `TcpStream::connect`, an unbounded
/// `dial_handshake` and an unbounded wait for the reply: on the answer path of
/// a live client's request, while the listener's own half and the blind-egress
/// carry each carry one. A borrower pointed at a Mac that accepts TCP and then
/// says nothing waited forever with a real client on the other end.
///
/// The silent lender is a bare `TcpListener` that accepts and holds: it is the
/// shape the deadline exists for, and it stalls the exchange at the FIRST hop
/// inside it (the handshake), which no per-frame timeout further down would
/// ever see.
///
/// `Borrowed::NotThisLender` and not an error, because that is what makes the
/// picker proceed: an `Err` is a protocol failure and is logged as one. It is
/// the NOT-DELIVERED arm specifically, because this lender stalls in the
/// handshake and no body ever crossed: the other arm, a deadline reached after
/// the body crossed, is
/// `a_borrow_that_was_delivered_is_never_offered_to_another_lender`.
///
/// The whole call is wrapped in a five-second test-side timeout so that
/// deleting the production deadline goes RED rather than hanging the suite.
/// Watch it fail by replacing `open_serve_within`'s
/// `tokio::time::timeout(deadline, ...)` with a bare await of `borrow_once`:
/// the harness reports "the borrow never returned".
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_that_never_answers_releases_the_client_at_the_deadline() {
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = PeerId([5_u8; 32]);

    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a lender that will never answer");
    let addr = silent.local_addr().expect("the silent lender's address");
    tokio::spawn(async move {
        // Accepted and HELD: the connection is open, so the borrower is past
        // `connect` and stuck in the handshake, which is the hop the review
        // named first.
        let mut held = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            held.push(stream);
        }
    });

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(lender_id, true, vec![addr.to_string()])],
    );
    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");
    let lease = lease_with(LEASE, 0.20, 0.0);
    let ask = ask_for("/v1/messages");

    let deadline = std::time::Duration::from_millis(200);
    let started = std::time::Instant::now();
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        serve::open_serve_within(
            &lender_id,
            &lease,
            &ask,
            &HeaderMap::new(),
            &store,
            deadline,
        ),
    )
    .await
    .expect("the borrow never returned: a client is waiting on this path and nothing bounds it");
    let waited = started.elapsed();

    assert!(
        matches!(answer, Ok(serve::Borrowed::NotThisLender)),
        "a lender that never took the body is `NotThisLender`: the same fact to the \
         caller as a refusal, so the local picker proceeds, and not an error"
    );
    assert!(
        waited < std::time::Duration::from_secs(2),
        "the client was held for {waited:?} against a 200ms deadline"
    );
}

/// **A borrow whose body has crossed is never offered to another lender.**
///
/// The review's finding: a borrow that hit `BORROW_TIMEOUT` answered
/// `Ok(None)`, which is the same word the path uses for a refusal and for a
/// lender with no address, so `try_serve` handed the SAME body to the next
/// lender with a fresh request id. One POST was then executed and billed on
/// two accounts, and the borrower could not tell: both answers were 200s it
/// never waited for.
///
/// The fleet here is two lenders in front of ONE origin that holds every
/// answer for two seconds, against a 300 ms borrow deadline. So the first
/// lender takes the body, puts it on the origin, and cannot answer in time.
/// The instrument is the origin's own arrival count: it is the only place that
/// can say how many times this fleet sent one request.
///
/// Watch it fail by answering `Ok(Borrowed::NotThisLender)` in
/// `open_serve_within`'s delivered-timeout arm: the count goes to 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_borrow_that_was_delivered_is_never_offered_to_another_lender() {
    let (upstream, arrivals) = fleet::spawn_slow_upstream(std::time::Duration::from_secs(2)).await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let borrower_id = tcr::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    // Two lenders, each a real listener with its own key, ledger and peers
    // file, both serving through the same slow origin.
    let mut rows = Vec::new();
    let mut homes = Vec::new();
    for _ in 0..2 {
        let row = lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        );
        let (home, borrower_row) = spawn_lender_in_its_own_home(row, lender_proxy.clone()).await;
        rows.push(borrower_row);
        homes.push(home);
    }

    // The deadline is the borrower's own file, which is the production path:
    // shorter than the origin's delay, so the first lender runs out of time
    // with the body already gone.
    let file = PeerFile {
        peers: rows,
        borrow_timeout_ms: 300,
        ..PeerFile::default()
    };
    tcr::peer::config::save(&borrower_peers, &file).expect("the borrower's peers file writes");

    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let ask = ask_for("/v1/messages");
    let answer = provider
        .try_serve(&ask)
        .await
        .expect("a delivered borrow answers the client rather than falling through");
    assert_eq!(
        answer.status().as_u16(),
        502,
        "the client is told the outcome is unknown, not handed a retryable 429"
    );
    assert_eq!(
        answer
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("false"),
        "a client that retries this would pay for the request twice"
    );

    // The origin's own count, after long enough for a second lender's copy to
    // have arrived if one had been sent.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(
        arrivals.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one POST, one arrival: a borrow that was delivered must not be executed on a \
         second account"
    );

    drop(homes);
}

/// **A hand-mode failure that never reached an upstream is not a delivery.**
///
/// The review's finding at `lease.rs`: the hand arm of `try_serve` mapped EVERY
/// `Err` from `serve_on_handed_bearer` to the no-retry 502, including a connect
/// or resolver failure where no byte left this Mac. `reqwest` reports "the
/// socket never opened" and "the reply never arrived" as one type, so a lease
/// whose upstream this Mac could not even reach ended the ladder and told the
/// client its request may already have been paid for.
///
/// What the arm now asks is this predicate, and it is the same pair of
/// questions `src/proxy/mod.rs` asks on the direct path (`is_connect`, plus the
/// resolver check). It is measured against REAL `reqwest` errors carrying the
/// production context string, on both sides of the answer: the connect failure
/// that must fall through to the next lender, and the timeout that must not. A
/// test that measured only the first would pass against a predicate answering
/// "nothing left this Mac" for everything, which is the expensive direction to
/// be wrong in.
///
/// Watch it fail by dropping `cause.is_connect()` from the predicate: a refused
/// connect is then read as a request that may already have run, which is the
/// behaviour this replaces. Wrapping is NOT where the risk turned out to be:
/// `anyhow`'s own `downcast_ref` already walks the chain, measured by mutating
/// the walk back to a bare `downcast_ref` and watching this test stay green.
#[tokio::test(flavor = "multi_thread")]
async fn a_connect_failure_is_not_a_delivery_and_a_timeout_is() {
    let client = tcr::http_client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_millis(150))
        .build()
        .expect("a client");

    // A port bound and immediately dropped: nothing listens, so the connect is
    // refused rather than hanging.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port to learn its number");
    let dead = closed.local_addr().expect("the dead address");
    drop(closed);

    let refused = client
        .post(format!("http://{dead}/v1/messages"))
        .send()
        .await
        .expect_err("nothing listens there");
    assert!(refused.is_connect(), "the fixture is a connect failure");
    let wrapped = anyhow::Error::new(refused)
        // The production context, verbatim from `serve_on_handed_bearer`.
        .context("peer hand: the request on the handed bearer did not reach upstream");
    assert!(
        lease::nothing_left_this_mac(&wrapped),
        "a connect failure under a context string is still a connect failure, and the \
         next lender may be asked"
    );

    // THE OTHER SIDE, and the one that costs money if it is wrong. A request
    // that went out and whose answer never came back may have run upstream.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the silent upstream");
    let silent_addr = silent.local_addr().expect("the silent address");
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((conn, _)) = silent.accept().await {
            open.push(conn);
        }
    });
    let timed_out = client
        .post(format!("http://{silent_addr}/v1/messages"))
        .send()
        .await
        .expect_err("a server that never answers");
    held.abort();
    assert!(
        !timed_out.is_connect(),
        "the fixture connected before it timed out"
    );
    let wrapped = anyhow::Error::new(timed_out)
        .context("peer hand: the request on the handed bearer did not reach upstream");
    assert!(
        !lease::nothing_left_this_mac(&wrapped),
        "a request that connected and then got no answer may have run on the owner's \
         account, so it is never offered to another lender"
    );

    // And something that is not a transport error at all is never read as one.
    assert!(
        !lease::nothing_left_this_mac(&anyhow::anyhow!("peer hand: a frame did not parse")),
        "only a transport failure can prove nothing left this Mac"
    );
}

/// **A lender that answers nothing has taken nothing, and the ladder goes
/// on.**
///
/// The review's finding: the borrower set `delivered` on the line that wrote
/// the body, so every lender-side refusal that happens BEFORE an upstream is
/// reached, the header gate with `inspect` turned off, and each `bail!` in
/// `handle_serve_on`, produced a no-retry 502 claiming the request may have
/// been billed, and skipped every remaining lender for the rest of the ask's
/// TTL. Nothing had been sent anywhere.
///
/// The fix is one frame: [`serve::ServeAck`]. The lender acknowledges taking
/// the request once its gates have passed and before it sends upstream, and the
/// borrower writes the body only after that ack. No ack is "not this lender".
///
/// The lender here reads the stream header and the request frame and then
/// answers nothing, which is what every one of those refusals looks like from
/// the borrower's end.
///
/// Watch it fail on the pre-ack build: the body is written before anything is
/// read back, so the answer is `DeliveredUnknown`.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_that_never_acknowledges_has_taken_nothing() {
    let pair = lease_pair(LendGrant::new(Window::SevenDay, 0.50, 300, 5));
    let addr = mesh::spawn_lender_that_takes_nothing(
        pair.lender_home.path().to_path_buf(),
        pair.lender_peers.clone(),
    )
    .await;
    let store = open_borrower_store(&pair.borrower_peers, pair.lender_id, addr);

    let lease = lease_with(LEASE, 0.20, 0.0);
    let ask = ask_for("/v1/messages");
    let answer = serve::open_serve_within(
        &pair.lender_id,
        &lease,
        &ask,
        &client_headers(),
        &store,
        std::time::Duration::from_millis(400),
    )
    .await;

    assert!(
        matches!(answer, Ok(serve::Borrowed::NotThisLender)),
        "a lender that never acknowledged has nothing of this request, so the next lender \
         may be asked and the client must not be told its request may have been billed"
    );
}

/// **A lender that refuses at its own gate is "not this lender", and the next
/// one serves.**
///
/// The review's finding: every refusal on the lender's half happens after the
/// borrower has already written the body, and the borrower set `delivered` on
/// that write. So a Mac that had turned `inspect` off, while the borrower still
/// held a live lease from it, answered nothing at all: the stream closed, the
/// borrower read `DeliveredUnknown` and told its client the request may have
/// been billed, with every remaining lender skipped.
///
/// The fix is one frame. The lender acknowledges TAKING the request once its
/// header and lease gates have passed and before it sends anything upstream,
/// and the borrower writes the body only after that ack. A close with no ack is
/// a lender that took nothing.
///
/// Two lenders in file order in front of one origin. The first has `inspect`
/// off, so its stream gate refuses; the second serves. The instrument is the
/// origin's own arrival count plus the status the client is handed.
///
/// Watch it fail on the pre-ack build: the answer is a 502 from the first
/// lender's silent close and the origin sees no arrival at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lender_that_refuses_at_its_gate_is_not_this_lender_and_the_next_serves() {
    let (upstream, arrivals, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let borrower_id = tcr::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    // Two lenders, asked in the order they are written: the first one grants a
    // lease and refuses the SERVE stream, the second one serves.
    let mut rows = Vec::new();
    let mut homes = Vec::new();
    for inspect in [false, true] {
        let mut row = lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        );
        // THE GATE THIS TEST IS ABOUT. `inspect` off is a lender that will hand
        // out a lease over CONTROL and refuse the SERVE stream that spends it,
        // which is exactly the shape an operator creates by turning disclosure
        // off while a borrower holds a cached lease.
        row.allow.inspect = inspect;
        let (home, borrower_row) = spawn_lender_in_its_own_home(row, lender_proxy.clone()).await;
        rows.push(borrower_row);
        homes.push(home);
    }

    let file = PeerFile {
        peers: rows,
        borrow_timeout_ms: 3_000,
        ..PeerFile::default()
    };
    tcr::peer::config::save(&borrower_peers, &file).expect("the borrower's peers file writes");

    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let ask = ask_for("/v1/messages");
    let answer = provider
        .try_serve(&ask)
        .await
        .expect("a refusal on the first lender must not end the ladder");
    assert_eq!(
        answer.status().as_u16(),
        200,
        "a lender that took nothing is not this lender, so the next one serves the client"
    );
    assert_eq!(
        arrivals.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one origin arrival: the refusing lender sent nothing and the serving \
         lender sent it once"
    );

    drop(homes);
}

/// **`borrowTimeoutMs` on the peers file is the deadline `open_serve` actually
/// uses**, not just a value [`serve::open_serve_within`] can be handed by
/// hand.
///
/// A peers file with `borrowTimeoutMs: 50` and the same silent, holding lender
/// as the test above: `open_serve` (the production call, never `_within`)
/// must release the client within a second, an order of magnitude under the
/// 10s built-in default, which proves the value on disk reached the deadline
/// rather than the constant winning anyway.
///
/// Watch it fail by reverting `open_serve` to hand `BORROW_TIMEOUT` to
/// `open_serve_within` instead of `store.file().borrow_timeout_ms`: the 50ms
/// setting is then ignored and the assertion on `waited` goes red at the 10s
/// default (bounded here by the 1s outer timeout so the suite fails fast
/// instead of hanging for ten seconds).
#[tokio::test(flavor = "multi_thread")]
async fn borrow_timeout_ms_on_the_peers_file_is_the_deadline_open_serve_uses() {
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = PeerId([6_u8; 32]);

    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a lender that will never answer");
    let addr = silent.local_addr().expect("the silent lender's address");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            held.push(stream);
        }
    });

    let file = PeerFile {
        peers: vec![borrower_row_for(lender_id, true, vec![addr.to_string()])],
        borrow_timeout_ms: 50,
        ..PeerFile::default()
    };
    tcr::peer::config::save(&borrower_peers, &file).expect("the borrower's peers file writes");

    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");
    let lease = lease_with(LEASE, 0.20, 0.0);
    let ask = ask_for("/v1/messages");

    let started = std::time::Instant::now();
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        serve::open_serve(&lender_id, &lease, &ask, &HeaderMap::new(), &store),
    )
    .await
    .expect(
        "open_serve must return within 1s under a 50ms borrowTimeoutMs: the file's setting \
         was not read",
    );
    let waited = started.elapsed();

    assert!(
        matches!(answer, Ok(serve::Borrowed::NotThisLender)),
        "a lender that does not answer in time is `Ok(None)`: {:?}",
        answer.err()
    );
    assert!(
        waited < std::time::Duration::from_secs(1),
        "the client was held for {waited:?} against a 50ms `borrowTimeoutMs`"
    );
}
