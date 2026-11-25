// ---------------------------------------------------------------------------
// The hostile borrower: a correctly pinned, correctly authenticated peer that
// hand-builds frames its own code would have refused to build. That is the case
// the lender's backstops exist for, and the only way to reach them: the
// borrower's own half refuses first, which is the point of it.
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// **A borrowed request pays the LENDER's per-organization GCRA bucket**: the
/// executable version of the claim `src/peer/serve.rs`'s module doc makes in
/// prose.
///
/// The lender's throttle is tightened to one instant slot then one per 350 ms,
/// and its fleet is one account in one organization, so three concurrent
/// BORROWED requests land in one bucket and the closed form is `(3-1)*350 =
/// 700 ms`. The control is the same three requests through a lender with the
/// throttle off.
///
/// Margins are deliberately wide (500 ms against a predicted 700, 300 ms
/// against a predicted ~0): this asserts that a borrowed request is paced at
/// all, not the tuning of the pacing.
///
/// Watch it fail by serving the relayed request anywhere other than through the
/// lender's own proxy: which is the only way to get a borrowed request past
/// `Manager::throttle_send` at all.
#[tokio::test(flavor = "multi_thread")]
async fn borrowed_requests_are_paced_by_the_lenders_own_bucket() {
    let paced = borrowed_burst(fleet::tight_throttle()).await;
    let loose = borrowed_burst(Default::default()).await;

    assert!(
        paced >= std::time::Duration::from_millis(500),
        "three borrowed requests on one org must be paced by that org's bucket, took {paced:?}"
    );
    assert!(
        loose < std::time::Duration::from_millis(300),
        "the control: with the throttle off the same three are not paced, took {loose:?}"
    );
}

/// Three concurrent borrowed requests against one lender, and how long the last
/// one took. The lender's fleet is one account in one organization, so every
/// one of the three lands in the same per-org bucket.
async fn borrowed_burst(throttle: tcr::config::ThrottleConfig) -> std::time::Duration {
    let (upstream, _hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy = fleet::spawn_proxy(fleet::lending_manager(&upstream, throttle)).await;

    let pair = lease_pair(LendGrant::new(Window::SevenDay, 0.50, 300, 5));
    let (lender_id, borrower_peers) = (pair.lender_id, pair.borrower_peers.clone());
    let lease = live_lease(0.50, 5);
    let ledger = shared_ledger(Some((lease, pair.borrower_id)));
    let peer_addr = spawn_unprobed_lender(
        pair.lender_home.path(),
        pair.lender_peers.clone(),
        ledger,
        lender_proxy,
    )
    .await;
    point_borrower_at(&borrower_peers, lender_id, peer_addr);

    // One warm request first, so the measured burst is not paying for whatever
    // the first request through a fresh proxy costs (a connect, a pin, a first
    // selection). The bucket is left to refill before the timed burst.
    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");
    let warm = ask_for("/v1/messages");
    serve::open_serve(&lender_id, &lease, &warm, &HeaderMap::new(), &store)
        .await
        .expect("the warmup ran")
        .served()
        .expect("the warmup served");
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;

    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..3 {
        let peers = borrower_peers.clone();
        handles.push(tokio::spawn(async move {
            let store = PeerStore::open(&peers).expect("the borrower's peers file");
            let ask = ask_for("/v1/messages");
            serve::open_serve(&lender_id, &lease, &ask, &HeaderMap::new(), &store)
                .await
                .expect("the SERVE stream ran")
                .served()
                .expect("the lender served it");
        }));
    }
    for handle in handles {
        handle.await.expect("a borrowed request finished");
    }
    started.elapsed()
}

/// **The lender refuses a local-control path IN A FRAME**: hole C, on the one
/// route where its absence is reachable end to end.
///
/// A borrower's own refusal is the check that matters, and it runs before
/// anything is opened. This is the backstop against an older or a malicious
/// borrower, and here the borrower is malicious on purpose: it hand-builds a
/// frame naming `/_tcr/accounts`, which the lender would otherwise forward to
/// its OWN proxy on loopback: where that route ADDS A LIVE CREDENTIAL and its
/// entire authorization is that the caller reached loopback.
///
/// Measured on the fake upstream's hit count and on the stream: the lender
/// answers nothing and serves nothing.
///
/// Watch it fail by deleting the `LOCAL_PREFIX` arm of
/// `serve_is_allowed_for_path`: the frame is then forwarded and the lender's own
/// proxy answers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_refuses_a_local_control_path_in_a_frame() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = hostile_pair();
    let peer_addr = pair.spawn_lender(lender_proxy).await;

    // The malicious borrower: a well-formed, correctly authenticated stream
    // carrying a path its own code would have refused to build.
    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        hostile_request("POST", "/_tcr/accounts"),
    )
    .await;

    assert!(
        answered.is_err(),
        "the lender closes on a forbidden path rather than answering it"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing was served: a relayed request for this Mac's own control route \
         never reaches its proxy"
    );
}

/// **A borrower speaking the older SERVE flow is refused BY NAME, and nothing
/// is served on its behalf.**
///
/// The ack is a change to the ORDER of frames on this stream: flow 1 wrote the
/// body straight after the request frame and waited for a reply, flow 2 waits
/// to be acknowledged first. Serving a flow-1 borrower on this build would mean
/// reading a body nobody acknowledged and leaving that borrower to report every
/// outcome as "may have been billed", which is the defect the ack exists to
/// fix, so the lender refuses rather than guesses.
///
/// The missing field reads 0 through `#[serde(default)]`, so a frame from a
/// build written before the field existed lands in the same refusal rather than
/// failing to parse.
///
/// Watch it fail by deleting the `request.flow != SERVE_FLOW` arm in
/// `handle_serve_on`: the lender acks, serves the request on its own account,
/// and the upstream's hit count goes to 1.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrower_on_the_older_serve_flow_is_refused_and_nothing_is_served() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = hostile_pair();
    let peer_addr = pair.spawn_lender(lender_proxy).await;

    // Flow 1: the order this stream had before the ack. `0` is the same case,
    // it is what a frame with no `flow` field at all parses to.
    for flow in [0, 1] {
        let answered = hostile_serve(
            peer_addr,
            &pair.lender_key,
            &pair.borrower_key,
            serve::ServeRequest {
                request_id: u128::from(flow) + 1,
                flow,
                ..hostile_request("POST", "/v1/messages")
            },
        )
        .await;
        assert!(
            answered.is_err(),
            "a borrower on SERVE flow {flow} is refused, not served on a guess about \
             which frame comes next"
        );
    }

    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing reached the lender's own account: a flow this build does not speak is \
         refused before anything is sent upstream"
    );
}

/// **An answer that cannot be carried back says `x-should-retry: false`.**
///
/// The review's finding: `undeliverable_answer` is the one answer this build is
/// CERTAIN already ran on the lender's account and was debited, and it was the
/// one answer with no `x-should-retry` on it. Its siblings both set it
/// (`lease.rs`'s `delivered_unknown_response`, `egress.rs`'s of the same name)
/// and `src/proxy.rs` makes it the house rule for an answer that means "do not
/// send this again". Without it the SDK behind the borrower retries a request
/// that has already been paid for.
///
/// The branch is reached the way production reaches it: the lender's own
/// upstream answers a `content-length` it does not deliver and closes, so
/// reading the body fails AFTER the account has served the request. The
/// `upstream` here is that server directly rather than the lender's proxy,
/// because a proxy in between would answer its own error and the read would
/// succeed.
///
/// Watch it fail by removing the header pair from `undeliverable_answer`: the
/// status is still 502 and the assertion on the header goes red.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_cannot_be_carried_back_is_never_retryable() {
    // An upstream that promises 4 096 bytes, sends 8, and closes.
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the truncating upstream");
    let truncating = listening
        .local_addr()
        .expect("the truncating upstream's addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listening.accept().await {
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut scratch = vec![0_u8; 8192];
                stream
                    .read(&mut scratch)
                    .await
                    .log_debug("reading a test request");
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                          content-length: 4096\r\n\r\ntruncate",
                    )
                    .await
                    .log_debug("writing a test response");
                stream.flush().await.log_debug("flushing a test stream");
            });
        }
    });

    let pair = hostile_pair();
    let peer_addr = pair.spawn_lender(format!("http://{truncating}")).await;

    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        hostile_request("POST", "/v1/messages"),
    )
    .await
    .expect("an answer that cannot be read is still an answer, never a dead stream");

    let serve::ServeReply::Served {
        status, headers, ..
    } = answered
    else {
        panic!("the lender answers `Served` with its own 502, not a refusal");
    };
    assert_eq!(
        status, 502,
        "the request ran and its answer cannot be carried"
    );
    let retry = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-should-retry"))
        .map(|(_, value)| value.as_str());
    assert_eq!(
        retry,
        Some("false"),
        "this request was served and debited on the lender's account; a client that \
         retries it pays for it twice"
    );
}
