//! The seam into the proxy: a blackholed fleet still answers 503, a carry is
//! offered only after the fleet has been walked and never twice, and a carried
//! request lands in the ledger and the quota window like a direct one.

use super::*;

// ---------------------------------------------------------------------------
// Item 2: the seam, and item 4: the metric line
// ---------------------------------------------------------------------------

/// **The no-regression half of the seam gate.** A blackholed upstream with no
/// mesh at all answers exactly what it answered before this test file
/// existed: the connect-phase 503, with its `retry-after` and its
/// `x-should-retry: true`.
///
/// `127.0.0.1:1` is refused immediately (RST), so every attempt dies at
/// CONNECT, `unknown_outcome_transport_failure` stays false, and the seam is
/// consulted and answers "nobody to ask". The captured line is the proof the
/// seam RAN rather than merely compiled: a passing 503 alone would be
/// identical whether the hunk executes or not.
#[tokio::test]
async fn a_blackholed_upstream_with_no_mesh_answers_todays_503() {
    let peers = scratch("no-mesh").join("tcr-peers.json");
    // An empty peers file: pinned nobody, so there is no Mac to ask. Written
    // rather than absent so this exercises the read path, and never the
    // operator's config directory.
    tcr::peer::config::save(&peers, &PeerFile::default()).expect("write the peers file");
    assert!(
        egress::install_peers_path(peers),
        "nothing else in this binary installs a peers path, and a lost race here would \
         point the seam at the operator's config directory"
    );

    let (collector, _guard) = capture();
    let response = post_messages(fleet("http://127.0.0.1:1")).await;

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a connect-phase failure with no mesh must still be today's 503"
    );
    assert_eq!(
        response
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert!(response.headers().contains_key("retry-after"));

    let line = collector.matching("returning 503 to client");
    assert_eq!(
        line.len(),
        1,
        "exactly one terminal line, and it is the 503 one: {:?}",
        collector
            .events()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        line[0].fields.contains_key("transport_failures"),
        "the count line the daily baseline reads must carry its figure: {:?}",
        line[0]
    );
    // The seam ran and declined, which is the assertion: `127.0.0.1:1` is not
    // an origin this mesh carries, so no Mac is asked. **A 503 alone would be
    // identical whether the hunk executes or not**, which is why this line is
    // the gate. The branch where a Mac IS asked cannot be reached through
    // `proxy::handle` in a test: its client validates the real certificate for
    // `api.anthropic.com` and a test cannot hand it a root without putting a
    // root-injection seam in the production path: so that branch is driven
    // directly in `a_gateway_that_does_not_answer_leaves_todays_503` and end to
    // end in `a_carry_delivers_the_canned_messages_body_through_a_gateway`.
    assert!(
        !collector
            .matching("not an origin the mesh carries")
            .is_empty(),
        "the seam must have been consulted: {:?}",
        collector
            .events()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
}

/// A pinned Mac that does not answer still leaves the request with today's
/// answer, and says which Mac was asked.
#[tokio::test]
async fn a_gateway_that_does_not_answer_leaves_todays_503() {
    let dir = scratch("dead-gateway");
    let peers = dir.join("tcr-peers.json");
    let mut file = PeerFile::default();
    // Port 1 on loopback: refused instantly, so the test does not wait out a
    // connect timeout.
    //
    // **The carry grant is now `true`**, where it was `false`:
    // a Mac outside the grant is no longer a candidate at all
    // (`only_a_mac_inside_the_carry_grant_is_a_candidate`), so the ungranted
    // row this test used to carry would now be filtered out before the dial
    // and there would be no Mac to name. The fact under test: a Mac that does
    // not answer leaves the request with today's answer, and says which Mac
    // was asked: is unchanged.
    file.pin(carry_row(
        &PeerId([5_u8; 32]),
        "asleep",
        vec!["127.0.0.1:1".to_string()],
        true,
    ));
    tcr::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    // This process may already have had a peers path installed by another test
    // in this binary, so the seam is exercised directly here rather than
    // through the installed cell: the proxy hunk's own call is covered by the
    // no-mesh test above.
    let carried = carry_one_message(&peers).await;
    assert!(
        matches!(carried, egress::ViaCarry::NotTaken),
        "a Mac that does not answer took nothing, so the request keeps today's answer"
    );
    assert!(
        !collector.matching("did not take the carry").is_empty(),
        "the refusal must name the Mac that was asked: {:?}",
        collector
            .events()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
}

/// **Item 4.** The offline path still emits the count line the daily baseline
/// reads, with the fields it reads it by.
///
/// A name that cannot resolve takes the resolver arm, which is the arm the
/// metric is about. The assertion is on the message AND on the two fields,
/// because a cron that greps the message and reads `dns_failures` breaks on
/// either one going missing.
#[tokio::test]
async fn the_offline_path_still_logs_the_503_count_line() {
    let (collector, _guard) = capture();
    let response = post_messages(fleet("https://tcr-egress-test.invalid")).await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a dead resolver is the recoverable 503, not a 502"
    );

    let line = collector.matching("returning 503 to client");
    assert_eq!(
        line.len(),
        1,
        "one terminal line: {:?}",
        collector
            .events()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        line[0].message.contains("name resolution is failing"),
        "the offline arm's own line, not the connect-phase one: {:?}",
        line[0]
    );
    assert!(
        line[0].fields.contains_key("dns_failures"),
        "the field the baseline counts by: {:?}",
        line[0]
    );
    assert!(
        line[0].fields.contains_key("retry_after"),
        "the field a client is steered by: {:?}",
        line[0]
    );
}

// ---------------------------------------------------------------------------
// The carry guard, which is about the REQUEST and not
// about one attempt
// ---------------------------------------------------------------------------

/// **The CRITICAL, red first.** A carry is offered only after the ladder has
/// rotated through the fleet, not on the first account's connect failure.
///
/// Two accounts, an upstream that refuses every connection (`127.0.0.1:1`
/// answers with an RST), and the order of the captured lines is the assertion:
/// the offer must come AFTER a rotation. With the guard as it shipped: one
/// conjunct, `err.is_connect()`, the offer fired on the first account, before
/// the second had been tried at all, and a fleet whose second account WOULD
/// have answered had its POST carried anyway.
///
/// Watched red by deleting `&& rotation_is_spent` from the guard at
/// `src/proxy.rs:2757`: the offer line then precedes every rotation line and
/// this fails on the index comparison.
#[tokio::test]
async fn the_carry_is_offered_only_after_the_fleet_has_been_walked() {
    let (collector, _guard) = capture();
    let response = post_messages(fleet_of(
        "http://127.0.0.1:1",
        &["alice@example.com", "bob@example.com"],
    ))
    .await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the answer is unchanged: a connect-phase failure is today's 503"
    );

    // `127.0.0.1` is not an origin the mesh carries, so this is the seam's own
    // first line and it fires once per offer: which makes it both the "was it
    // offered" and the "how many times" probe, without any peers file.
    let offers = collector.matching("not an origin the mesh carries");
    assert_eq!(
        offers.len(),
        1,
        "one offer per request, never one per attempt: {:?}",
        collector.messages()
    );
    let offered_at = collector
        .first_index("not an origin the mesh carries")
        .expect("the offer line");
    let rotated_at = collector
        .first_index("rotating to another account")
        .expect("a two-account fleet must rotate before it gives up");
    assert!(
        offered_at > rotated_at,
        "the carry must be offered after the rotation, not on the first account's failure: {:?}",
        collector.messages()
    );
}

/// **The double-send, red first.** An upstream answer anywhere in the request
/// means the request left this box, so there is no carry: even when the
/// attempt that fails last failed at connect.
///
/// The fake upstream answers ONE request and then stops listening, so attempt
/// one gets a real HTTP status and attempt two gets a connect refusal from the
/// same address. That is exactly the shape the shipped guard admitted: its only
/// conjunct described the current attempt, so a request an upstream may already
/// have acted on was carried a second time through a peer.
///
/// Watched red by deleting the `every_attempt_transport_failed(..)` conjunct
/// from the guard: the offer line appears and this fails.
#[tokio::test]
async fn an_upstream_answer_anywhere_in_the_request_stops_the_carry() {
    let listener = loopback().await;
    let upstream = format!(
        "http://{}",
        listener.local_addr().expect("the upstream address")
    );
    // One answer, then the listener is gone: the SECOND attempt's connect is
    // refused by the kernel. The listener is dropped BEFORE the response is
    // written, so the ordering is not a race.
    let fake_upstream = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.expect("the first attempt connects");
        let mut seen = vec![0_u8; 4096];
        let _read = conn.read(&mut seen).await.expect("the request arrives");
        drop(listener);
        conn.write_all(
            b"HTTP/1.1 403 Forbidden\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        )
        .await
        .expect("the answer is written");
        conn.flush().await.expect("the answer is flushed");
    });

    let (collector, _guard) = capture();
    let response = post_messages(fleet_of(
        &upstream,
        &["alice@example.com", "bob@example.com"],
    ))
    .await;
    fake_upstream.await.expect("the fake upstream finishes");

    assert_ne!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "an upstream answered, so this is not the no-route 503"
    );
    assert!(
        collector
            .matching("not an origin the mesh carries")
            .is_empty(),
        "nothing may be carried once an upstream has answered this request: {:?}",
        collector.messages()
    );
}

/// **The other half of the double-send, red first.** An attempt that died PAST
/// the connect phase means the request may already be on the wire, so no later
/// connect failure may carry it.
///
/// The fake upstream accepts, reads the request, stops listening and then
/// closes WITHOUT answering: attempt one fails past connect
/// (`unknown_outcome_transport_failure`), and the retry's connect is refused.
/// Every conjunct but `!unknown_outcome_transport_failure` is satisfied here :
/// nothing reached an upstream, the last attempt died at connect, the ladder is
/// out of moves: so this test isolates that one.
///
/// Watched red by deleting `!unknown_outcome_transport_failure` from the guard:
/// the offer line appears and this fails.
#[tokio::test]
async fn an_attempt_that_died_past_connect_stops_the_carry() {
    let listener = loopback().await;
    let upstream = format!(
        "http://{}",
        listener.local_addr().expect("the upstream address")
    );
    let fake_upstream = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.expect("the first attempt connects");
        let mut seen = vec![0_u8; 4096];
        // The request is READ (it left this box), and then the connection
        // dies with no answer, which is the "unknown outcome" the guard is
        // about.
        let _read = conn.read(&mut seen).await.expect("the request arrives");
        drop(listener);
        drop(conn);
    });

    let (collector, _guard) = capture();
    let response = post_messages(fleet_of(&upstream, &["alice@example.com"])).await;
    fake_upstream.await.expect("the fake upstream finishes");

    assert!(
        collector
            .matching("not an origin the mesh carries")
            .is_empty(),
        "a request that may already be on the wire must never be carried again: {:?}",
        collector.messages()
    );
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "and the answer is the unknown-outcome 502 it was before"
    );
}

// ---------------------------------------------------------------------------
// A carried request is a served request
// ---------------------------------------------------------------------------

/// **Item 4.** A carried request lands in the ledger the TUI and
/// `tcr status --json` read, against the account whose credential it used.
///
/// Driven through `record_carried` rather than through `proxy::handle`, because
/// a carry that gets as far as a response needs the upstream to BE
/// `api.anthropic.com` (the origin allow-list), and this suite reaches no
/// network beyond loopback. The proxy's own call site passes exactly these
/// values; `handle`'s terminal outcome passes the same ones to the same three
/// methods.
///
/// Watched red by deleting the `push_log` call from `record_carried`.
#[tokio::test]
async fn a_carried_request_lands_in_the_ledger() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);
    let before = manager
        .snapshot(time::OffsetDateTime::now_utc())
        .recent
        .len();

    egress::record_carried(
        &manager,
        egress::CarriedRecord {
            session_key: Some(7),
            wire_session_id: Some("11111111-1111-1111-1111-111111111111"),
            model: Some("claude-opus-4-6".to_string()),
            ..carried_answer(200, &axum::http::HeaderMap::new())
        },
    )
    .await;

    let snapshot = manager.snapshot(time::OffsetDateTime::now_utc());
    assert_eq!(
        snapshot.recent.len(),
        before + 1,
        "a carried request is one line in the ring buffer, like any other"
    );
    let line = snapshot.recent.first().expect("the line just pushed");
    assert_eq!(line.path, "/v1/messages");
    assert_eq!(line.method, "POST");
    assert_eq!(line.status, 200);
    assert_eq!(
        line.account, "alice@example.com",
        "the account is OURS: a carry borrows a route, never an account"
    );
    assert!(
        snapshot.sessions.iter().any(|session| session.requests > 0),
        "and the served counter moved: {:?}",
        snapshot.sessions
    );
}

/// **A carried request's spend is visible to the rotation model.**
///
/// The review's finding at `egress.rs:840`: a carried response carries the
/// pooled account's `anthropic-ratelimit-unified-*` headers exactly as a direct
/// one does, and until `record_carried` folded them in, that spend was invisible
/// to `lendable_fraction`: the figure this whole feature lends on. So a Mac
/// that had spent its window through a peer went on advertising it as lendable.
///
/// The figures asserted are the ones the fake upstream "sent": utilization
/// `0.42` on a 5-hour window that resets in two hours, read back through the
/// public accessor `lendable_fraction` itself reads
/// (`Manager::window_utilizations`).
///
/// Watched red by deleting the `manager.update_quota` call from
/// `record_carried` (`src/peer/tunnel/egress.rs`): the window stays `None`, i.e. "we
/// have never seen this account's quota", and the assertion below names it.
#[tokio::test]
async fn a_carried_requests_spend_reaches_the_quota_window() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);
    let now = time::OffsetDateTime::now_utc();
    assert_eq!(
        manager.window_utilizations(tcr_peer_wire::Window::FiveHour, now),
        vec![None],
        "before the carry this fleet has never seen this account's window"
    );

    let reset = (now + time::Duration::hours(2)).unix_timestamp();
    let mut upstream_headers = axum::http::HeaderMap::new();
    upstream_headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.42".parse().expect("a header value"),
    );
    upstream_headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.to_string().parse().expect("a header value"),
    );

    egress::record_carried(
        &manager,
        egress::CarriedRecord {
            session_key: Some(9),
            wire_session_id: Some("11111111-1111-1111-1111-111111111111"),
            model: Some("claude-opus-4-6".to_string()),
            ..carried_answer(200, &upstream_headers)
        },
    )
    .await;

    assert_eq!(
        manager.window_utilizations(tcr_peer_wire::Window::FiveHour, now),
        vec![Some(0.42)],
        "the carried response's own headers are what the window must now read"
    );
    assert!(
        manager.lendable_fraction(
            &tcr_peer_wire::LendScope::All,
            tcr_peer_wire::Window::FiveHour,
            now
        ) < 1.0,
        "and the figure this feature lends on is the one that moved"
    );
}

/// **A carried rejection holds the account, exactly as a direct one does.**
///
/// The review's finding: `PinnedEgress::Answered` returns the carried response
/// from ABOVE the rotation loop, so the 401/403/429/529 ladder never saw it. A
/// pinned account that upstream had just rejected went on being selected, and
/// the next request took the same rejection: the pin turned off pacing,
/// refreshing and holding for the one account whose traffic an operator most
/// wanted shaped.
///
/// The half of the ladder that is a FACT ABOUT THE ACCOUNT is recorded in
/// `record_carried` now. The other half, rotating this request away, cannot
/// apply: the response is already on its way to the client, and a pinned
/// account has nowhere to rotate to anyway.
///
/// Watched red by deleting the `record_the_hold_the_ladder_would` call from
/// `record_carried`: the account reads `Active` and serves the next request
/// straight into the same rejection.
#[tokio::test]
async fn a_carried_rejection_holds_the_account_the_way_a_direct_one_does() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);
    assert_ne!(
        manager.account_status(0),
        Some(tcr::manager::AccountStatus::Throttled),
        "before the carry this account is servable: the control for the assertion below"
    );

    let mut upstream_headers = axum::http::HeaderMap::new();
    upstream_headers.insert(
        "anthropic-ratelimit-unified-status",
        "rejected".parse().expect("a header value"),
    );
    upstream_headers.insert(
        "anthropic-ratelimit-unified-5h-status",
        "rejected".parse().expect("a header value"),
    );
    upstream_headers.insert("retry-after", "600".parse().expect("a header value"));

    egress::record_carried(&manager, carried_answer(429, &upstream_headers)).await;

    assert_eq!(
        manager.account_status(0),
        Some(tcr::manager::AccountStatus::Throttled),
        "a rejection that arrived through a peer holds the account, or the next request \
         is sent straight into it again"
    );
    assert!(
        !manager.hold_expired(0, tcr::now_ms()),
        "and the hold is still running: it was armed for what the origin asked"
    );
}

/// **A carried response runs the same recovery the direct path runs.**
///
/// The review's finding: `record_the_hold_the_ladder_would` recorded the 429
/// hold and nothing else, and no carried status ever ran `clear_rate_limited`.
/// The direct path runs it on every non-429 (`src/proxy.rs`, "any non-429 is
/// live proof a rate-limit hold no longer binds"), and a pinned account's
/// requests all leave this way, so an account Throttled by ONE carried 429
/// never came back to Active: it dropped out of selection, out of keep-warm and
/// out of `handoff_bearer` for good, while the very next carried 200 proved it
/// was serving.
///
/// Watch it fail by deleting the `clear_rate_limited` call from
/// `record_the_hold_the_ladder_would`: the account is still Throttled after a
/// carried 200.
#[tokio::test]
async fn a_carried_success_clears_the_hold_a_carried_rejection_armed() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);
    manager.mark_rate_limited(0, 300);
    assert_eq!(
        manager.account_status(0),
        Some(tcr::manager::AccountStatus::Throttled),
        "the fixture starts held, or the assertion below is about nothing"
    );

    egress::record_carried(&manager, carried_answer(200, &axum::http::HeaderMap::new())).await;

    assert_ne!(
        manager.account_status(0),
        Some(tcr::manager::AccountStatus::Throttled),
        "a carried 200 is live proof the hold no longer binds, exactly as a direct one is"
    );
}

/// **A carried 401 on a token-only account condemns it, the way the direct path
/// does.**
///
/// The direct path's first 401 arm: an account with no refresh token has a
/// credential nothing but a re-login can revive, so it is marked `Error` rather
/// than left Active to be selected, 401'd and rotated away from on every
/// request while `tcr status` reports it healthy. A carried 401 recorded none
/// of that.
///
/// Watch it fail by deleting the `status == 401` arm from
/// `record_the_hold_the_ladder_would`: the account stays Active.
#[tokio::test]
async fn a_carried_401_on_a_token_only_account_marks_it_error() {
    let manager = fleet_with(
        "http://127.0.0.1:1",
        vec![Account {
            name: "alice@example.com".to_string(),
            account_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
            access_token: "at-0".to_string(),
            // A `tcr login --token` account: inference only, and no refresh
            // that could ever revive a rejected credential.
            refresh_token: None,
            ..fake_account()
        }],
    );

    egress::record_carried(&manager, carried_answer(401, &axum::http::HeaderMap::new())).await;

    assert_eq!(
        manager.account_status(0),
        Some(tcr::manager::AccountStatus::Error),
        "a rejected credential with no refresh is dead whichever route the request left by"
    );
}

/// **A carried 429 with no guidance parks for what the direct path parks, not a
/// minute.**
///
/// `retry_after.unwrap_or(60)` fabricated a sixty-second hold for a 429 that
/// named no `retry-after`, which is the exact number
/// `classify_transient_429` exists to have stopped using: the direct path parks
/// `NO_GUIDANCE_HOLD_SECS` plus up to five seconds of jitter, 15 to 20 seconds,
/// so the fleet un-parks staggered instead of going dark for a minute. A pinned
/// account's traffic all leaves this way, so the old number was the one an
/// operator actually felt.
///
/// The instrument is the hold's own deadline, probed 25 seconds out: past every
/// value the direct path can choose and well inside the fabricated 60.
///
/// Watch it fail by restoring `manager.mark_rate_limited(idx,
/// retry_after.clamp(1, 300))`: the hold is 60 seconds and is still running.
#[tokio::test]
async fn a_carried_transient_429_parks_for_what_the_direct_path_parks() {
    let manager = fleet_of("http://127.0.0.1:1", &["alice@example.com"]);

    // A 429 with NO unified rejection and NO `retry-after`: the transient shape.
    egress::record_carried(&manager, carried_answer(429, &axum::http::HeaderMap::new())).await;

    assert_eq!(
        manager.account_status(0),
        Some(tcr::manager::AccountStatus::Throttled),
        "a 429 still holds the account: this is about how long, not whether"
    );
    assert!(
        !manager.hold_expired(0, tcr::now_ms() + 14_000),
        "and the hold is a real one: still running 14 seconds out"
    );
    assert!(
        manager.hold_expired(0, tcr::now_ms() + 25_000),
        "a transient 429 with no guidance parks 15 to 20 seconds, the way the direct \
         path parks it, and not the fabricated minute"
    );
}
