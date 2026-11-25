// ---------------------------------------------------------------------------
// The order a row's endpoints are tried in
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// **Newest observation first, and a forwarded hop last however new it is.**
///
/// The two rules are asserted against one row that violates both in its
/// stored order, because either rule alone passes a version that has the other
/// backwards: a list sorted only by time puts a fresh `Via` ahead of a direct
/// socket and spends a third machine's bytes to reach a peer this node could
/// have opened a socket to; a list grouped only by kind dials a year-old
/// address before yesterday's.
///
/// A hop costs another operator's machine and its consent, which is why it is
/// what is left when nothing direct answered and never a choice made on a
/// timestamp.
///
/// Watched red: with `order.sort_by_key(Endpoint::is_via)` deleted from
/// `dial::dial_order`, this fails with "a forwarded hop is tried last", the
/// `Via` endpoint leads, because it is the newest thing on the row.
#[test]
fn a_row_is_dialled_newest_first_and_through_a_peer_last() {
    let old: std::net::SocketAddr = "192.0.2.1:9600".parse().expect("a test address");
    let fresh: std::net::SocketAddr = "192.0.2.2:9600".parse().expect("a test address");
    let forwarder = PeerId([0x33; 32]);

    let mut row = PeerRow {
        node: PeerId([0x11; 32]),
        label: "studio-mac".to_string(),
        endpoints: Vec::new(),
        added_at: 1_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    };
    row.observe_endpoint(Endpoint::direct(old, 1_000, EndpointSource::Paired));
    row.observe_endpoint(Endpoint::direct(fresh, 2_000, EndpointSource::Hello));
    // The newest thing on the row, and still the last thing to try.
    row.observe_endpoint(Endpoint::via(forwarder, 3_000, EndpointSource::Hello));

    let order = dial::dial_order(&row);
    let addrs: Vec<Option<std::net::SocketAddr>> =
        order.iter().map(Endpoint::direct_addr).collect();
    assert_eq!(
        addrs,
        vec![Some(fresh), Some(old), None],
        "newest direct socket, then the older one, then the hop: {order:?}"
    );
    assert!(
        order.last().is_some_and(Endpoint::is_via),
        "a forwarded hop is tried last: {order:?}"
    );
}

/// **A lease's tokens land on the path the borrow arrived over, and on no
/// other.**
///
/// PATH-ACCOUNTING metered the paths and could not reach the one place that
/// knows which of a peer's endpoints a SERVE came in on, so `Ledger::debit`
/// charged the meter with no path and every token was unattributed. The wiring
/// is one `note_lease_path` before the debit, inside the same lock, and this is
/// its gate.
///
/// A TOKENS lease on purpose. `Ledger::tokens_of` answers `None` for a
/// `Fraction` by design (the tokens a share of a window buys is a fact about
/// upstream's pricing that nothing on this Mac holds), so a fraction lease
/// charges nothing whatever the meter is told and a gate written on one would
/// pass with its own subject deleted.
///
/// # Why this is asked of the ledger and not of a served borrow
///
/// The parked two-process version of this test cannot be landed against this
/// build, and finding out why is the finding rather than the obstacle:
/// `Ledger::may_relay` refuses every `LeaseUnit::Tokens` lease with
/// `LeaseRefusal::Unsupported` before a relay is ever entered
/// (`fraction_budget` answers `None` for that unit), and `clamp_to_grant`
/// mints fractions only. So no served request can carry a tokens lease, the
/// per-path token branch in `Ledger::debit` is unreachable from the serve path
/// in this build, and a two-process test of it refuses at the lender with
/// `Unsupported`: measured, not inferred. What is reachable, and what this
/// gate holds, is the arithmetic itself: noted path plus tokens unit plus a
/// known grantee charges that path and nothing else.
///
/// Watched red by deleting the `note_lease_path` call below, which is the same
/// deletion the serve-side wiring would suffer: the meter then holds no row for
/// this borrower at all.
#[test]
fn a_leases_tokens_are_charged_to_the_path_it_was_noted_on() {
    let borrower = PeerId([0xb0_u8; 32]);
    let arrived_over: std::net::SocketAddr = "127.0.0.1:9901".parse().expect("a literal address");
    let path = tcr::peer::config::Locator::Direct { addr: arrived_over };
    let now = tcr::now_ms();

    let rows_for = |peer: PeerId| {
        let mut meter = tcr::peer::tunnel::path_meter()
            .lock()
            .expect("the meter lock");
        meter
            .rows(tcr::now_ms())
            .into_iter()
            .filter(|row| row.peer == peer)
            .collect::<Vec<_>>()
    };
    assert!(
        rows_for(borrower).is_empty(),
        "the control: this borrower's key is this test's own, so a row afterwards is this \
         charge and not another test's"
    );

    let mut ledger = Ledger::new();
    ledger.record_scoped(
        token_lease_at(0xb0, 1_000, now),
        borrower,
        tcr_peer_wire::LendScope::All,
    );
    // The line under test. Before the debit, so the tokens land on the path the
    // borrow arrived over rather than on the path of the borrow before it.
    ledger.note_lease_path(0xb0, path);
    let charged = ledger.debit(0xb0, REQUEST, 0.10);
    assert!(
        charged > 0.0,
        "the debit has to charge something, or the token arithmetic below is never reached"
    );

    let rows = rows_for(borrower);
    assert!(
        rows.iter()
            .any(|row| row.tokens_last_hour > 0 && row.locator == path),
        "the tokens have to land on the endpoint this borrow arrived over, or the per-path \
         figure is blank for every lease this Mac serves: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| row.locator == path),
        "and on no other path: {rows:?}"
    );
}

/// **A hand-mode grant is served from the BORROWER, end to end, through the
/// production provider.** The one path this file could not drive.
///
/// Every other hand-mode gate lives in `tests/peer/lending/peer_hand.rs` and calls
/// `serve_on_handed_bearer` directly, so the whole of the borrower's own
/// decision, does this lease have a live bearer and which of the two modes does
/// that make it, was covered by nothing. This test starts at the real listener:
/// the lender grants a `hand` lease, pushes its bearer on the same control
/// session, and `PeerLeaseProvider::try_serve` then answers its client from
/// this Mac.
///
/// # Why it waits on `handed_tokens` rather than on a sleep
///
/// The bearer arrives on the session AFTER the grant frame, and the reader task
/// stores it, so a borrow can legitimately reach the fork before its own bearer
/// does and fall through to the serve path. Waiting for the store is what makes
/// the assertion below about the MODE rather than about who won a race.
///
/// The store it waits on is the process-wide `lease::handed_tokens()`, which is
/// the store the provider reads and the only one it can read; a bearer put into
/// a `HandedTokens` the test built is invisible to this path. The bearer is
/// forgotten at the end so no later test in this binary inherits it.
///
/// Watch it fail by making `try_serve`'s fork unconditional (`if false`): the
/// request crosses to the owner, `borrower_hits` reads 0 and the lender's own
/// upstream answers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_hand_grant_is_served_from_the_borrower_through_the_provider() {
    let (owner_upstream, owner_hits, _owner_served) = fleet::spawn_upstream().await;
    let manager = fleet::lending_manager(&owner_upstream, Default::default());
    // A bearer is handed over only for an account whose window this Mac has
    // MEASURED: the frame carries the baseline the borrower's own rises are
    // counted against, so an unmeasured account has no handoff to make.
    let mut measured = HeaderMap::new();
    measured.insert(
        "anthropic-ratelimit-unified-7d-utilization",
        "0.10".parse().expect("a header value"),
    );
    manager.update_quota(0, &measured, 200);
    let owner_proxy = fleet::spawn_proxy(Arc::clone(&manager)).await;

    let pair = lease_pair(LendGrant {
        mode: tcr::peer::config::LendMode::Hand,
        ..LendGrant::new(Window::SevenDay, 0.20, 300, 2)
    });
    let borrower_peers = &pair.borrower_peers;
    let peer_addr = mesh::spawn_lender(
        pair.lender_home.path().to_path_buf(),
        pair.lender_peers.clone(),
        shared_ledger(None),
        owner_proxy.clone(),
        Arc::new(serve::NoFleetUtilization),
        Arc::clone(&manager),
    )
    .await;
    point_borrower_at(borrower_peers, pair.lender_id, peer_addr);

    // The borrower's OWN origin, on its own port. Which origin received the
    // request is the whole instrument: hand mode means these bytes never touch
    // the owner's Mac.
    let (borrower_origin, borrower_hits, borrower_served) = fleet::spawn_origin().await;
    let provider = PeerLeaseProvider::new(borrower_peers.clone()).with_hand_egress(
        borrower_origin,
        tcr::base::http_client::builder().build().unwrap(),
    );

    // The first ask is what mints the lease and opens the session the bearer
    // arrives on, so it is also the one that may reach the fork early. Its
    // outcome is not asserted; the lease it leaves behind is what the second
    // one borrows on.
    let _first = provider.try_serve(&ask_for("/v1/messages")).await;

    // The lease the provider is holding, read off the borrower's own state
    // file, which is where it writes the borrowed rows. Its id is what the
    // bearer is keyed by and what this test forgets on the way out.
    let borrower_state = tcr::peer::state::peer_state_path(borrower_peers);
    let handed = tcr::peer::state::load(&borrower_state, tcr::now_ms())
        .expect("the borrower's state file reads")
        .borrowed
        .first()
        .expect("the first ask left a borrowed lease behind")
        .lease
        .lease_id;

    let store = lease::handed_tokens();
    let mut arrived = false;
    for _ in 0..200 {
        arrived = store
            .lock()
            .expect("the handed-token store")
            .bearer(handed, tcr::now_ms())
            .is_some();
        if arrived {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        arrived,
        "the owner hands a bearer over for a hand-mode grant, on the session that granted it"
    );

    let before = borrower_hits.load(std::sync::atomic::Ordering::SeqCst);
    let owner_before = owner_hits.load(std::sync::atomic::Ordering::SeqCst);
    let response = provider
        .try_serve(&ask_for("/v1/messages"))
        .await
        .expect("a lease with a live handed bearer is served");
    assert_eq!(
        response.status().as_u16(),
        200,
        "the borrower answers its client with what its own origin said"
    );
    assert_eq!(
        borrower_hits.load(std::sync::atomic::Ordering::SeqCst),
        before + 1,
        "the hand-mode request leaves from the BORROWER"
    );
    assert_eq!(
        owner_hits.load(std::sync::atomic::Ordering::SeqCst),
        owner_before,
        "and nothing about it reaches the owner's own upstream"
    );

    let seen = borrower_served.lock().expect("served lock").clone();
    let last = seen
        .last()
        .expect("the borrower's origin recorded the request");
    assert_eq!(
        last.token,
        fleet::LENDER_TOKEN,
        "spent on the OWNER's handed bearer, which is what makes it a borrow"
    );
    for required in ["anthropic-version", "content-type"] {
        assert!(
            last.header(required).is_some(),
            "a hand-mode request the API would refuse is not a served answer: {:?}",
            last.headers
        );
    }
    assert!(
        !last
            .values()
            .iter()
            .any(|value| value.contains("not-a-real-client-token")),
        "and the client's own credential is not on it: {:?}",
        last.headers
    );

    // The store is this process's, shared with every other test in this
    // binary, so what this one put in it goes out with it.
    store.lock().expect("the handed-token store").forget(handed);
}

/// **A `lentTo` row names its Mac by the wire id as well as by the label**, so
/// a reader can join the line to the peer row it is about.
///
/// It carried the label alone. A label is the operator's display string: two
/// Macs can wear the same one, it is renamed whenever its owner feels like it,
/// and every other cross-surface join in this tree moved to the wire id, so a
/// panel merging an account card's line with a peers row had nothing to merge
/// on and drew the line beside the wrong Mac whenever two labels matched.
///
/// The assertion is the VALUE and not the key: the id is compared against the
/// one the peers row itself carries, so writing the label into the new field
/// fails here.
///
/// Watch it fail by putting `row.label.clone()` back in `lent_to`'s `peer_id`.
#[test]
fn a_lent_to_row_carries_the_wire_id_of_the_mac_it_names() {
    let dir = tempfile::tempdir().expect("a temp home");
    let peers = dir.path().join("tcr-peers.json");

    // Two Macs wearing ONE label, which is the case the label alone cannot
    // tell apart and the case an operator reaches by pairing a replacement Mac
    // before retiring the old one.
    let first = PeerId([21_u8; 32]);
    let second = PeerId([22_u8; 32]);
    let grant = |fraction: f64| LendGrant::new(Window::SevenDay, fraction, 300, 2);
    let mut one = lender_row_for(first, vec![grant(0.20)]);
    one.label = "studio-mac".to_string();
    let mut two = lender_row_for(second, vec![grant(0.30)]);
    two.label = "studio-mac".to_string();
    write_peers(&peers, vec![one, two]);

    let store = PeerStore::open(&peers).expect("the peers file");
    let lent = lease::lent_to(
        &store,
        &[("lender-fake".to_string(), Vec::new(), Provider::Anthropic)],
    );
    let rows = lent
        .get("lender-fake")
        .expect("an `all` grant covers every local account");

    let ids: Vec<&str> = rows.iter().map(|row| row.peer_id.as_str()).collect();
    assert_eq!(
        ids,
        [first.to_wire(), second.to_wire()],
        "each row names the Mac whose grant it came from, in the wire form the \
         peers rows are keyed by: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| row.peer == "studio-mac"),
        "the label stays what it was, for the screen: {rows:?}"
    );

    let json = serde_json::to_string(&lent).expect("the lentTo map serializes");
    assert!(
        json.contains(&format!("\"peerId\":\"{}\"", first.to_wire())),
        "and the panel reads it under `peerId`: {json}"
    );
}

/// **A pair the serve cache has forgotten is charged when it is served again.**
///
/// The serve cache and the charge cache were two maps evicted independently, on
/// two different clocks: the serve side recorded at the caller's `now_ms`, the
/// charge side at the wall clock, and only the requests that reached the charge
/// ever entered the second one. So the two FIFOs drifted, and a pair that had
/// left the serve cache while still sitting in the charge cache was admitted
/// again (new to the serve side) and charged nothing (old to the charge side):
/// a relay served on the lender's own account for free, repeatable for as long
/// as the borrower kept the charge entry alive.
///
/// One cache with a state per pair is what closes it: forgetting a pair forgets
/// both facts about it at once, so "admitted again" and "charged again" can no
/// longer disagree.
///
/// Watched red: give the charge its own map again and the final assertion reads
/// `0`.
#[test]
fn a_pair_the_serve_cache_forgot_is_charged_when_it_is_served_again() {
    let peer = PeerId([9_u8; 32]);
    let now = tcr::now_ms();
    let mut ledger = Ledger::new();
    ledger.record_scoped(
        Lease {
            lease_id: LEASE,
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.50),
            granted_at_ms: now,
            expires_at_ms: now + 600_000,
            spent: 0.0,
            max_inflight: 64,
            until: None,
        },
        peer,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.90);

    // One relay that reaches the charge, so the pair is in both halves of the
    // accounting at once. This is the only request in the test that does.
    ledger
        .enter_relay(LEASE, &peer, REQUEST, now)
        .expect("the first relay of a fresh id is admitted");
    ledger.leave_relay(LEASE);
    assert!(
        ledger.debit(LEASE, REQUEST, 0.0) > 0.0,
        "and it is charged, or the pair never enters the charge half and this test has no \
         subject"
    );

    // Then the borrower pushes that pair out of the serve cache with its own
    // traffic: every one of these is admitted and none of them reaches the
    // charge, which is exactly the shape a borrower whose relays fail upstream
    // produces.
    for nth in 0..(lease::SERVED_PER_LEASE_CAPACITY as u128 + 8) {
        ledger
            .enter_relay(LEASE, &peer, REQUEST + 1 + nth, now)
            .expect("each fresh id is admitted");
        ledger.leave_relay(LEASE);
    }

    // The original pair is old news to this ledger now, so it is served again.
    assert_eq!(
        ledger.enter_relay(LEASE, &peer, REQUEST, now),
        Ok(()),
        "a pair this ledger no longer remembers is admitted again, which is the honest \
         answer once it has been forgotten"
    );
    ledger.leave_relay(LEASE);
    assert!(
        ledger.debit(LEASE, REQUEST, 0.0) > 0.0,
        "and a relay this ledger admitted is CHARGED: a pair forgotten by one half of the \
         accounting and remembered by the other is a request served on the lender's own \
         account for nothing"
    );
}

/// An [`Ask`] whose client sent a query string, which is the shape both borrow
/// arms used to drop.
fn ask_with_query<'a>(path: &'a str, query: &'a str) -> Ask<'a> {
    Ask {
        query: Some(query),
        ..ask_for(path)
    }
}

/// **A borrowed request reaches the lender's upstream with its query string.**
///
/// The seam split the client's URI into a query-stripped path and a query, and
/// then carried only the path: the relay frame had nowhere to put the second
/// half and the lender rebuilt the URL from the first. So a client that asked
/// for `/v1/messages?beta=true` had `/v1/messages` answered on somebody else's
/// account, and whether its parameters were honoured depended on which account
/// happened to serve it: the direct path and the carry path both keep them.
///
/// The path stays query-stripped on purpose, because every refusal the borrow
/// owes the client matches on the path and a query string must not be able to
/// decide one of those. So the query travels as its own field and is put back
/// on the URL the lender builds.
///
/// Watched red: drop the `set_query` from `serve_on_own_account`
/// (`src/peer/serve.rs`) and the upstream sees a bare `/v1/messages`.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrowed_request_carries_its_query_string_to_the_lenders_upstream() {
    let (upstream, hits, served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let rig = serve_rig(
        LendGrant::new(Window::SevenDay, 0.20, 300, 2),
        lender_proxy.clone(),
    )
    .await;

    let headers = borrower_credentials();
    let ask = ask_with_query("/v1/messages", "beta=true");
    let response = serve::open_serve(&rig.pair.lender_id, &rig.lease, &ask, &headers, &rig.store)
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the lender served it");
    assert_eq!(response.status().as_u16(), 200);

    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one arrival, or the assertion below is reading somebody else's request"
    );
    let seen = served.lock().expect("served lock").clone();
    assert_eq!(
        seen[0].uri, "/v1/messages?beta=true",
        "the lender's upstream has to be asked the question the borrower's client asked, \
         parameters included"
    );
}

/// **A hand-mode borrow carries its query string too.**
///
/// The same defect in the other arm, and the reason both are one change: a
/// hand-mode request is built on the BORROWER's Mac against the owner's bearer,
/// by a second URL builder that also set only the path. Fixing the relayed arm
/// alone would leave a client's parameters honoured or dropped depending on
/// which kind of lease it happened to hold, which is the harder failure to see.
///
/// Watched red: drop the `set_query` from `serve_on_handed_bearer`
/// (`src/peer/lease/mod.rs`) and the upstream sees a bare `/v1/messages`.
#[tokio::test(flavor = "multi_thread")]
async fn a_handed_request_carries_its_query_string_to_the_owners_upstream() {
    let (upstream, hits, served) = fleet::spawn_origin().await;
    // A lease id of this test's own, because the handed-token store is one
    // process-wide map and every test in this binary shares it.
    let lease_id = 0xc0ff_ee00_u128;
    let now = tcr::now_ms();
    lease::handed_tokens()
        .lock()
        .expect("the handed-token store")
        .put(lease_id, "not-a-real-owner-token".to_string(), now + 60_000);

    let client = tcr::base::http_client::builder()
        .no_proxy()
        .build()
        .expect("a client");
    let ask = ask_with_query("/v1/messages", "beta=true");
    let response =
        serve::serve_on_handed_bearer(&upstream, &client, &ask, lease_id, Window::SevenDay, now)
            .await
            .expect("the handed request reached the fake upstream")
            .expect("a lease with a live handed bearer is served from here");
    assert_eq!(response.status().as_u16(), 200);

    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one arrival, or the assertion below is reading somebody else's request"
    );
    let seen = served.lock().expect("served lock").clone();
    assert_eq!(
        seen[0].uri, "/v1/messages?beta=true",
        "a handed borrow asks the owner's upstream the question the client asked, \
         parameters included"
    );
}

/// **A lease stops serving when its lending hours close, not when its TTL
/// runs out.**
///
/// The `--between` / `--days` schedule was consulted once, at the mint, and
/// never again. A lease minted a minute before the window closed then kept
/// serving on the owner's account until its own TTL or its `until` said
/// otherwise, which for a week-long grant is days outside the hours the
/// operator lent. An operator who writes "22:00-08:00" means the requests, not
/// the paperwork.
///
/// The fixture's grant names every weekday EXCEPT today's, so it is closed
/// whatever hour the suite runs at and whatever the host's timezone is. The
/// lease itself is live on every other axis, a fresh TTL, budget untouched,
/// headroom noted, so a refusal here can only be the schedule.
///
/// Watched red: delete the schedule block from `handle_serve_on`
/// (`src/peer/serve.rs`) and the lender serves it.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_outside_the_grants_hours_is_refused_rather_than_served() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    // Every day but today's, read off the same local clock `Schedule::contains`
    // converts to, so this fixture is closed on every host and at every hour.
    let today = time::OffsetDateTime::now_utc()
        .to_offset(
            time::UtcOffset::local_offset_at(time::OffsetDateTime::now_utc())
                .unwrap_or(time::UtcOffset::UTC),
        )
        .weekday();
    let every_other_day: Vec<&str> = [
        ("mon", time::Weekday::Monday),
        ("tue", time::Weekday::Tuesday),
        ("wed", time::Weekday::Wednesday),
        ("thu", time::Weekday::Thursday),
        ("fri", time::Weekday::Friday),
        ("sat", time::Weekday::Saturday),
        ("sun", time::Weekday::Sunday),
    ]
    .into_iter()
    .filter(|(_, day)| *day != today)
    .map(|(name, _)| name)
    .collect();
    let mut closed_now = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    closed_now.days = Some(
        tcr::peer::config::schedule::Days::try_from(every_other_day.join(","))
            .expect("a day list this test built"),
    );

    // The lease is live on every axis the lease itself carries, so the refusal
    // below can only be about the hours.
    let rig = serve_rig(closed_now, lender_proxy.clone()).await;

    let headers = borrower_credentials();
    let ask = ask_for("/v1/messages");
    let outcome = serve::open_serve(&rig.pair.lender_id, &rig.lease, &ask, &headers, &rig.store)
        .await
        .expect("the SERVE stream ran");

    assert!(
        outcome.served().is_none(),
        "a lease whose lending hours are closed may not be served, whatever is left on its \
         TTL or its budget"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "and nothing reached the owner's upstream: the hours are enforced before the \
         request is sent, not after it has been paid for"
    );
    assert_eq!(
        rig.ledger.lock().expect("ledger lock").inflight(LEASE),
        0,
        "a refusal takes no in-flight slot, or one closed window strands the lease for its \
         whole TTL"
    );
}

/// **A local client cannot bench this Mac's accounts by sending the
/// account-set header.**
///
/// `x-tcr-accounts` is how a lender's own serving leg tells its picker which of
/// its accounts a lease's scope allows. The picker read it off any inbound
/// request, and the only thing saying that could not happen was a comment
/// claiming the borrowed path was the only one that reached it. So any process
/// that could talk to the proxy could bench accounts by name: a set naming no
/// account leaves the whole fleet benched and the client gets the exhausted
/// 429, which is a local denial of service spelled in one header.
///
/// The marker the relaying leg already writes is what makes the comment true: a
/// request that has not crossed a Mac holds no lease, so it has no account-set
/// scope and its copy of the header is ignored.
///
/// The fleet here is healthy and its one account is NOT the one the header
/// names, so a honoured header benches everything and produces a 429. A served
/// 200 is the header having been ignored.
///
/// Watched red: drop the `RELAYED_HEADER_NAME` filter from the account-set
/// block in `src/proxy/mod.rs` and this answers 429.
#[tokio::test(flavor = "multi_thread")]
async fn the_account_set_header_is_ignored_on_a_request_that_crossed_no_mac() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let proxy = fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;
    let client = tcr::base::http_client::builder()
        .no_proxy()
        .build()
        .expect("a loopback client");

    let forged = client
        .post(format!("{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header(
            tcr::proxy::ACCOUNTS_HEADER_NAME,
            "no-such-account-on-this-mac",
        )
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .expect("the proxy answered");

    assert_eq!(
        forged.status().as_u16(),
        200,
        "a local client benched this Mac's whole fleet with one header it had no lease for"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "and the request really was served upstream, so the 200 above is a serve rather \
         than something short-circuited before the picker"
    );
}

/// **No Codex path is relayed over the mesh, from either side** (Codex is not
/// lent in this version). A lease is sized on the lender's
/// Anthropic windows, and its picker would serve a Codex request on a Codex
/// account no lease accounts for. Every Codex path, inference or not, is
/// refused by the one predicate both halves ask: the borrower's seam builds no
/// `Ask`, and the frame builder builds no frame.
///
/// The controls: the predicate still allows the ordinary Anthropic inference
/// path, and `/backend-apix/…` is NOT Codex (`Provider::of_path` matches whole
/// segments), so the refusal is the provider's classification and not a bare
/// prefix.
///
/// Watch it fail by deleting the Codex arm of `serve_is_allowed_for_path`.
#[test]
fn codex_paths_never_open_a_relay() {
    for path in [
        "/backend-api/codex/responses",
        "/backend-api/wham/usage",
        "/backend-api",
    ] {
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} is a Codex path and must never be relayed"
        );
        let ask = ask_for(path);
        assert!(
            serve::serve_request_from(&ask, &HeaderMap::new(), LEASE, REQUEST).is_err(),
            "{path} must not even build a frame"
        );
    }
    assert!(serve::serve_is_allowed_for_path("/v1/messages"));
    assert!(serve::serve_is_allowed_for_path("/backend-apix/evil"));
}

/// **A lender refuses a Codex path IN A FRAME, even with a Codex account to
/// serve it on.** The borrower's refusal is the one that matters; this is the
/// backstop against an older or a malicious borrower, which here hand-builds a
/// frame naming the Codex inference path. The lender's own fleet is one CODEX
/// account over the fake upstream, so a lender that took the frame would serve
/// it there and the hit count would read 1.
///
/// Watch it fail by deleting the Codex arm of `serve_is_allowed_for_path`: the
/// lender acks, serves the frame on its Codex account, and the upstream counts
/// the hit.
#[tokio::test(flavor = "multi_thread")]
async fn codex_path_in_a_frame_is_refused_by_the_lender() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy = fleet::spawn_proxy(fleet::codex_lending_manager(&upstream)).await;

    let pair = hostile_pair();
    let peer_addr = pair.spawn_lender(lender_proxy).await;

    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        hostile_request("POST", tcr::base::provider::CODEX_RESPONSES_PATH),
    )
    .await;

    assert!(
        answered.is_err(),
        "the lender closes on a Codex path rather than serving it: {answered:?}"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing was served on the lender's Codex account"
    );
}
