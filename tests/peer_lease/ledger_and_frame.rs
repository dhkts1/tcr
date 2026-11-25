// ---------------------------------------------------------------------------
// What a relayed frame may never carry, and the ledger that decides whether a
// lease may relay at all
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// **The scrub.** A SERVE frame carries neither `authorization` nor
/// `x-api-key`, because the borrower removes both before the frame is written.
///
/// On the borrower, not the lender: a lender-side refusal happens after the
/// bytes are already in the lender's process, its memory and its request log.
///
/// Watch it fail by deleting the scrub.
#[test]
fn relay_frame_carries_no_client_credential() {
    let mut headers = HeaderMap::new();
    // Obviously fake, and shaped like the real thing so a prefix match cannot
    // pass this test by accident.
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer not-a-real-token"),
    );
    headers.insert("x-api-key", HeaderValue::from_static("not-a-real-key"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));

    let removed = serve::scrub_client_credentials(&mut headers);

    assert_eq!(removed, 2, "both credential headers must be removed");
    assert!(headers.get("authorization").is_none());
    assert!(headers.get("x-api-key").is_none());
    assert!(
        headers.get("content-type").is_some(),
        "the scrub removes credentials, not the request"
    );
}

/// **The refusal.** A request to one of the client-credential paths never opens
/// a relay at all: it falls through to this machine's own local path.
///
/// Six of the seven are prefixes in `CLIENT_CREDENTIAL_PREFIXES`
/// (`src/proxy.rs:820-827`). Falling through is the right answer, not an error:
/// the request is perfectly servable here.
///
/// Watch it fail by emptying the refusal list.
#[test]
fn a_client_credential_path_never_opens_a_relay() {
    for path in [
        "/v1/code",
        "/v1/code/",
        "/api/oauth/files",
        "/api/oauth/file_upload",
        "/api/oauth/organizations",
        "/v1/mcp_servers",
        "/v1/sessions",
    ] {
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} is a client-credential path and must never be relayed"
        );
    }

    assert!(
        serve::serve_is_allowed_for_path("/v1/messages"),
        "an ordinary inference request is exactly what a lease is for"
    );
}

/// **The seventh path**, which is the one a coder who reads only the array will
/// miss.
///
/// `/v1/oauth/token` lives in its own constant (`CLIENT_TOKEN_REFRESH_PATH`,
/// `src/proxy.rs:839`), not in the six-element array, and its own doc records
/// the live defect that omitting it caused: an exact compare let the
/// trailing-slash spelling fall through to the pooled path, putting our Bearer
/// on a client's token exchange. So both spellings are asserted here, and the
/// match is `path_is_under` rather than an equality test.
///
/// It is also the one path a BLIND tunnel carries for free: that is how an
/// offline machine refreshes its own tokens, so "free to carry, forbidden to
/// relay" is one fact about two stream kinds, not a contradiction.
///
/// Watch it fail by swapping `path_is_under` for an exact compare, which is
/// precisely the shape of the recorded defect.
#[test]
fn oauth_token_exchange_never_relays() {
    for spelling in ["/v1/oauth/token", "/v1/oauth/token/"] {
        assert!(
            !serve::serve_is_allowed_for_path(spelling),
            "{spelling} is a client's own credential exchange and must never be relayed"
        );
    }
}

/// **The guard band.** A lease with budget left is still refused when the
/// lender's own window crosses its guard, and the refusal is
/// `owner-guard`: its own case, not a variant of "spent".
///
/// It has to be able to fire alone: the headers this arithmetic reads lag, and
/// upstream answers 200s for accounts it is about to bench, which is why a
/// guard exists rather than the raw threshold.
///
/// Watch it fail at lender utilization just under the guard, which is the
/// control that passes vacuously if the ordering is wrong.
#[test]
fn a_lease_can_never_spend_the_owners_guard() {
    let mut ledger = Ledger::new();
    ledger.record(lease_with(LEASE, 0.500, 0.002));

    // The lender's own window has crossed its guard, so `lendable_fraction`
    // answers 0.0: the figure the default config produces at a utilization of
    // 0.91 against a guard of 0.95 - 0.05.
    ledger.note_owner_headroom(Window::SevenDay, 0.0);
    assert_eq!(
        ledger.may_relay(LEASE, NOW),
        Err(LeaseRefusal::OwnerGuard),
        "0.498 of budget is left and the deadline is ahead, so this refusal fired ALONE"
    );

    // The control that makes the assertion above non-vacuous: just UNDER the
    // guard, 0.89 against 0.90, the same lease proceeds. If the ordering were
    // wrong (owner-guard folded into spent, or checked before expiry), this is
    // the arm that would break.
    ledger.note_owner_headroom(Window::SevenDay, 0.90 - 0.89);
    assert_eq!(
        ledger.may_relay(LEASE, NOW),
        Ok(()),
        "a lender at 0.89 against a 0.90 guard still has headroom to lend"
    );

    // ... and the guard is not the only refusal that can fire, which is what
    // makes the ORDER a contract rather than a coincidence.
    let mut spent = Ledger::new();
    spent.record(lease_with(LEASE, 0.500, 0.500));
    spent.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(spent.may_relay(LEASE, NOW), Err(LeaseRefusal::LeaseSpent));

    let mut expired = Ledger::new();
    expired.record(lease_with(LEASE, 0.500, 0.500));
    expired.note_owner_headroom(Window::SevenDay, 0.0);
    assert_eq!(
        expired.may_relay(LEASE, NOW + 1_000_000),
        Err(LeaseRefusal::LeaseExpired),
        "expired outranks both spent and the owner's guard, which are also true here"
    );
}

/// An unmeasured window lends nothing.
///
/// A ledger that was never told the owner's headroom refuses, rather than
/// treating silence as room. This is the direction where a wrong guess costs the
/// OWNER their quota, so it is the direction that fails closed.
#[test]
fn a_window_with_no_measurement_refuses() {
    let mut ledger = Ledger::new();
    ledger.record(lease_with(LEASE, 0.500, 0.0));
    assert_eq!(
        ledger.may_relay(LEASE, NOW),
        Err(LeaseRefusal::OwnerGuard),
        "no headroom measurement is a refusal, never unlimited room"
    );
}

/// One relayed request, one debit, even when two disjoint paths deliver it.
///
/// The `via` stamp cannot catch a diamond: two paths converging on one
/// terminal, so the request id is what makes the second delivery free.
#[test]
fn one_request_id_debits_exactly_once() {
    let mut ledger = Ledger::new();
    ledger.record(lease_with(LEASE, 0.500, 0.0));

    let first = ledger.debit(LEASE, REQUEST, 0.031);
    let diamond = ledger.debit(LEASE, REQUEST, 0.031);

    assert!(
        (first - 0.031).abs() < f64::EPSILON,
        "the first delivery charges the observed rise, got {first}"
    );
    assert_eq!(
        diamond, 0.0,
        "the second delivery of the SAME request id charges nothing"
    );
    let spent = ledger
        .live(NOW)
        .first()
        .copied()
        .expect("the lease is live at NOW")
        .spent;
    assert!(
        (spent - 0.031).abs() < f64::EPSILON,
        "one request, one debit on the ledger's own total, got {spent}"
    );

    // A DIFFERENT request under the same lease is a second, real debit.
    let second = ledger.debit(LEASE, REQUEST + 1, 0.0);
    assert!(
        (second - lease::MIN_DEBIT).abs() < f64::EPSILON,
        "a zero observed rise still charges MIN_DEBIT, got {second}"
    );
}

/// **The cap is a refusal, not a queue.** The (n+1)th concurrent borrowed
/// request is told no while n are running, and a slot freeing lets the next one
/// in.
///
/// It exists because the debit reads headers this tree documents as lagging: an
/// unbounded pipe can overdraw a small lease inside the lag window, and the cap
/// bounds the overdraft to a known number of requests rather than to however
/// many the borrower can open.
#[test]
fn the_n_plus_first_concurrent_borrowed_request_is_refused() {
    let grantee = PeerId([41_u8; 32]);
    let mut ledger = Ledger::new();
    // `record_scoped` and a FRESH request id per entry, both added together:
    // the review's M1 binds a lease to its grantee, and its H2 refuses a
    // `request_id` this lease has already relayed. The old body recorded the
    // lease with no grantee and entered three times under no id at all, which
    // is exactly the two shapes now refused: so it is rewritten rather than
    // relaxed, and what it measures (the cap, and the cap alone) is unchanged.
    ledger.record_scoped(
        lease_with(LEASE, 0.500, 0.0),
        grantee,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, NOW), Ok(()));
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 1, NOW),
        Ok(())
    );
    assert_eq!(ledger.inflight(LEASE), 2);

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 2, NOW),
        Err(RelayRefusal::TooManyInflight { max: 2 }),
        "the third request is refused with the cap named, never queued"
    );

    ledger.leave_relay(LEASE);
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 3, NOW),
        Ok(()),
        "a freed slot admits the next borrowed request"
    );

    // The lease's own refusals still come first: a full pipe must not mask an
    // expired lease, or a borrower would retry a lease that is gone.
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 4, NOW + 1_000_000),
        Err(RelayRefusal::Lease(LeaseRefusal::LeaseExpired))
    );
}

/// **A cap refusal forgets nothing.** At the cap, the refusal leaves the
/// counter at the cap and the next request is refused too: the cap keeps
/// binding for as long as the requests it counted are still running.
///
/// Testing found this: `enter_relay` answered a cap refusal
/// with `self.inflight.remove(&lease_id)`, which dropped the count of the
/// `max` requests that were in flight at that instant. The very next
/// `enter_relay` then read zero and admitted, so the cap bound exactly once
/// per lease and a borrower that simply retried on `InFlightFull`: which is
/// the one refusal `LeaseRefusal::InFlightFull` tells it to retry after, got
/// an unbounded pipe. `the_n_plus_first_concurrent_borrowed_request_is_refused`
/// above cannot see it, because it calls `leave_relay` before testing
/// admission again and a missing key makes that call a no-op.
///
/// Watched red: restore the `self.inflight.remove(&lease_id)` line in
/// `Ledger::enter_relay` (`src/peer/lease.rs`) and the counter reads 0 rather
/// than 2 right after the refusal, while the fourth request is admitted.
#[test]
fn a_cap_refusal_leaves_the_in_flight_counter_where_it_found_it() {
    let grantee = PeerId([42_u8; 32]);
    let mut ledger = Ledger::new();
    ledger.record_scoped(
        lease_with(LEASE, 0.500, 0.0),
        grantee,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, NOW), Ok(()));
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 1, NOW),
        Ok(())
    );
    assert_eq!(ledger.inflight(LEASE), 2, "the cap's worth is in flight");

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 2, NOW),
        Err(RelayRefusal::TooManyInflight { max: 2 })
    );
    assert_eq!(
        ledger.inflight(LEASE),
        2,
        "the two requests still running were not forgotten by the refusal"
    );
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 3, NOW),
        Err(RelayRefusal::TooManyInflight { max: 2 }),
        "so the next one is refused too: the cap binds every time, not once"
    );
    assert_eq!(ledger.inflight(LEASE), 2);

    // And only when a slot is genuinely released does the next one get in,
    // leaving exactly the cap in flight again.
    ledger.leave_relay(LEASE);
    assert_eq!(ledger.inflight(LEASE), 1);
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 4, NOW),
        Ok(())
    );
    assert_eq!(ledger.inflight(LEASE), 2);
}

/// **A lease that may relay nothing parks no counter.** `max_inflight` zero
/// refuses the first request, and the refusal leaves the in-flight map without
/// an entry at all: `Ledger::leave_relay` documents zero and absent as one
/// fact, and an `entry(..).or_insert(0)` on the refusal path broke that for
/// every lease that could never relay.
///
/// Watched red: change the `self.inflight(lease_id)` read in
/// `Ledger::enter_relay` back to `self.inflight.entry(lease_id).or_insert(0)`
/// with the `remove` line deleted, and the map holds a zero for this lease.
#[test]
fn a_zero_in_flight_cap_refuses_and_parks_no_counter() {
    let grantee = PeerId([43_u8; 32]);
    let mut ledger = Ledger::new();
    let mut lease = lease_with(LEASE, 0.500, 0.0);
    lease.max_inflight = 0;
    ledger.record_scoped(lease, grantee, tcr_peer_wire::LendScope::All);
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST, NOW),
        Err(RelayRefusal::TooManyInflight { max: 0 }),
        "a lease that may relay nothing refuses its first request"
    );
    assert_eq!(ledger.inflight(LEASE), 0);
    assert_eq!(
        ledger.inflight_tracked(),
        0,
        "and no entry is parked for it: zero and absent are the same fact"
    );
}

/// **The review's H2, as an arithmetic fact about the ceiling.** One
/// `request_id` replayed is refused, and the lease's budget therefore actually
/// runs out.
///
/// The defect: `debit` was idempotent per `(lease_id, request_id)` and the SERVE
/// was not, so every copy of one request id was served on the lender's account
/// while `Lease::spent` moved once. `may_relay`'s `spent >= budget` could then
/// never become true and the lease was unbounded in the one dimension it exists
/// to bound.
///
/// Watched red: delete the `admit_served` refusal from
/// `Ledger::enter_relay` (`src/peer/lease.rs`) and the second `enter_relay`
/// below answers `Ok(())`.
#[test]
fn one_request_id_is_relayed_once_and_the_ceiling_then_binds() {
    let grantee = PeerId([42_u8; 32]);
    let mut ledger = Ledger::new();
    // A budget of two MIN_DEBITs, so a bounded lease spends out in two
    // requests and an unbounded one never does.
    ledger.record_scoped(
        lease_with(LEASE, lease::MIN_DEBIT * 2.0, 0.0),
        grantee,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST, NOW),
        Ok(()),
        "the first copy of a request id is served"
    );
    ledger.leave_relay(LEASE);
    assert!(ledger.debit(LEASE, REQUEST, 0.0) > 0.0, "and it is charged");

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST, NOW),
        Err(RelayRefusal::Replayed),
        "the SECOND copy of the same request id is REFUSED, never served again          for free: this is the whole of H2"
    );

    // And the ceiling binds: a fresh id spends the rest of the budget, and the
    // one after it is out of money rather than out of luck.
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 1, NOW),
        Ok(())
    );
    ledger.leave_relay(LEASE);
    ledger.debit(LEASE, REQUEST + 1, 0.0);
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 2, NOW),
        Err(RelayRefusal::Lease(LeaseRefusal::LeaseSpent)),
        "two MIN_DEBITs out of a two-MIN_DEBIT lease is spent"
    );
}

/// **The review's M1**: a lease is not a bearer token. Peer B presenting peer
/// A's lease id is refused, and told nothing that distinguishes "not yours"
/// from "no such lease".
///
/// Watched red: delete the `grantees` comparison at the top of
/// `Ledger::enter_relay` and the stranger's relay answers `Ok(())`.
#[test]
fn another_peers_lease_id_is_refused_and_leaks_nothing() {
    let grantee = PeerId([43_u8; 32]);
    let stranger = PeerId([44_u8; 32]);
    let mut ledger = Ledger::new();
    ledger.record_scoped(
        lease_with(LEASE, 0.500, 0.0),
        grantee,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(
        ledger.enter_relay(LEASE, &stranger, REQUEST, NOW),
        Err(RelayRefusal::NotTheGrantee),
        "a pinned peer holding `inspect` may not spend a lease another peer          negotiated"
    );
    assert_eq!(
        RelayRefusal::NotTheGrantee.to_wire(),
        LeaseRefusal::LeaseExpired,
        "and the wire word is the SAME one an unknown lease id gets, so the          refusal is not an oracle for lease ids"
    );
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST, NOW),
        Ok(()),
        "the grantee's own relay is unaffected: the stranger's attempt must not          have burned this request id"
    );
}

/// **The review's M4**: none of the ledger's three peer-driven collections
/// grows without bound.
///
/// The bar is the feature's own, stated twice next door in `listener.rs`: "a map
/// a stranger can grow without bound is a memory bug with a security label".
///
/// Watched red: delete the prune from `Ledger::record` and `leases()` stays at
/// 300; drop the `inflight.remove` at zero in `leave_relay` and the in-flight
/// map keeps a key per lease; raise `SERVED_CAPACITY` past the loop count and
/// the pair cache grows past it.
#[test]
fn the_ledger_does_not_grow_without_bound_on_peer_driven_input() {
    let grantee = PeerId([45_u8; 32]);
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    // 300 leases asked for and expired, one after another: which is what a
    // peer that asks once a second for five minutes leaves behind.
    for nth in 0..300_u128 {
        let mut expired = lease_with(LEASE + 1 + nth, 0.500, 0.0);
        expired.expires_at_ms = tcr::now_ms() - 1;
        ledger.record_scoped(expired, grantee, tcr_peer_wire::LendScope::All);
    }
    // One live lease last, so the vector is not empty for a reason unrelated to
    // the prune.
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.expires_at_ms = tcr::now_ms() + 300_000;
    ledger.record_scoped(live, grantee, tcr_peer_wire::LendScope::All);
    assert!(ledger.live(tcr::now_ms()).len() == 1, "one live lease");

    // In flight, then out: the key goes with it.
    let now = tcr::now_ms();
    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, now), Ok(()));
    assert_eq!(ledger.inflight(LEASE), 1);
    ledger.leave_relay(LEASE);
    assert_eq!(
        ledger.inflight(LEASE),
        0,
        "a released slot reads zero, whether the key is there or not"
    );

    // The pair cache is bounded by its own constant, not by how many requests
    // a borrower can send.
    for nth in 0..(lease::SERVED_CAPACITY as u128 + 64) {
        ledger.leave_relay(LEASE);
        assert_eq!(
            ledger.enter_relay(LEASE, &grantee, REQUEST + 1 + nth, now),
            Ok(()),
            "a released slot admits the next request"
        );
    }
    assert!(
        ledger.served_len() <= lease::SERVED_CAPACITY,
        "the served-pair cache is bounded by SERVED_CAPACITY ({}), got {}",
        lease::SERVED_CAPACITY,
        ledger.served_len()
    );
}

/// **The review's H1**: a borrower-chosen path that the upstream URL parser
/// would read differently than these prefix compares do is refused outright.
///
/// Each shape below was measured to normalize somewhere else: the dot segments
/// onto the lender's own privileged `/_tcr/` routes, the unrooted one onto a
/// host the borrower named (an SSRF with the lender's network position), the
/// backslash onto a `CLIENT_CREDENTIAL_PREFIXES` path.
///
/// Watched red: delete the `relay_path_is_routable` call from
/// `serve::serve_is_allowed_for_path` and every shape below is allowed.
#[test]
fn a_path_the_url_parser_would_reroute_never_opens_a_relay() {
    for path in [
        // Dot segments, which the parser collapses away.
        "/x/../_tcr/accounts/control",
        "/x/../_tcr/accounts",
        "/v1/messages/../_tcr/status",
        "/v1/messages/%2e%2e/_tcr/status",
        // Not rooted: the authority becomes userinfo and the request leaves
        // the machine.
        "@169.254.169.254/latest/meta-data",
        "v1/messages",
        // Protocol-relative: rooted, and still names a host.
        "//169.254.169.254/latest/meta-data",
        // A backslash, which WHATWG treats as a path separator.
        "/v1/code\\foo",
        "/v1\\messages",
    ] {
        assert!(
            !serve::relay_path_is_routable(path),
            "{path} must not be routable: the parser reads it as something else"
        );
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} must never open a relay"
        );
        let ask = ask_for(path);
        assert!(
            serve::serve_request_from(&ask, &HeaderMap::new(), LEASE, REQUEST).is_err(),
            "{path} must not even build a frame"
        );
    }

    // The positive control, without which every assertion above could be
    // passing because the function refuses everything.
    for ordinary in ["/v1/messages", "/v1/messages/count_tokens", "/"] {
        assert!(
            serve::relay_path_is_routable(ordinary),
            "{ordinary} is an ordinary relayable path and must stay routable"
        );
    }
    assert!(
        serve::serve_is_allowed_for_path("/v1/messages"),
        "and the ordinary path still opens a relay"
    );
}

/// **An escaped separator is refused too.** `/v1/messages/..%2F_tcr` is not a
/// dot segment to `Url::parse` (it does not decode `%2F`), so it reaches the
/// lender intact, and a hop that DOES decode before routing reads it as
/// `/v1/messages/../_tcr`: the same escape `relay_path_is_routable` exists to
/// refuse, arriving one decode later. Both cases of both spellings.
///
/// Watched red: delete the `%2f`/`%5c` refusal from
/// `serve::relay_path_is_routable` and every shape below is routable.
#[test]
fn an_escaped_path_separator_never_opens_a_relay() {
    for path in [
        "/v1/messages/..%2F_tcr",
        "/v1/messages/..%2f_tcr",
        "/v1/messages/..%5C_tcr",
        "/v1/messages/..%5c_tcr",
        "/v1%2f..%2f_tcr/accounts/control",
        "/v1/code%5Cfoo",
    ] {
        assert!(
            !serve::relay_path_is_routable(path),
            "{path} carries an escaped separator and must not be routable: a hop that decodes \
             it before routing reads a dot segment"
        );
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} must never open a relay"
        );
        let ask = ask_for(path);
        assert!(
            serve::serve_request_from(&ask, &HeaderMap::new(), LEASE, REQUEST).is_err(),
            "{path} must not even build a frame"
        );
    }

    // The positive control: an ordinary escape that is NOT a separator stays
    // routable, so this refusal is about `/` and `\` and not about `%`.
    assert!(
        serve::relay_path_is_routable("/v1/messages%20count"),
        "an escaped space is not a path separator and must stay routable"
    );
}

/// **What `Url::set_path` plus the under-the-base check actually refuse: none
/// of the H1 shapes.**
///
/// `serve_on_own_account`'s comment used to say this pair was what stopped
/// `//169.254.169.254/latest/meta-data` if `relay_path_is_routable` were
/// deleted. It is not. `set_path` cannot move the authority: which is what
/// makes the borrower's host harmless, and every shape below therefore lands
/// UNDER this Mac's own base, including the dot-segment path that collapses
/// straight onto the privileged local route. The refusal is
/// `relay_path_is_routable`'s alone, and this test is what keeps the comment
/// honest about that.
///
/// The one thing the check does catch is its last case: an `upstream` carrying
/// a path, whose path `set_path` replaces.
#[test]
fn set_path_keeps_the_authority_and_the_under_base_check_is_not_what_refuses() {
    let base = "http://127.0.0.1:3456";
    for path in [
        "//169.254.169.254/latest/meta-data",
        "/x/../_tcr/accounts/control",
        "@169.254.169.254/latest/meta-data",
        "/v1/code\\foo",
        "/v1/messages/..%2F_tcr",
    ] {
        let mut url = reqwest::Url::parse(base).expect("the base parses");
        url.set_path(path);
        assert_eq!(
            url.host_str(),
            Some("127.0.0.1"),
            "set_path must never move the authority, which is the real defence: {path} -> {url}"
        );
        assert!(
            url.as_str().starts_with(base),
            "and every one of these therefore passes the under-the-base check, so that check \
             refuses none of them: {path} -> {url}"
        );
    }

    // The dot segment collapses onto this Mac's own privileged route and is
    // still under the base: the case the old comment claimed this line caught.
    let mut collapsed = reqwest::Url::parse(base).expect("the base parses");
    collapsed.set_path("/x/../_tcr/accounts/control");
    assert_eq!(collapsed.path(), "/_tcr/accounts/control");
    assert!(
        !serve::relay_path_is_routable("/x/../_tcr/accounts/control"),
        "which is why the refusal has to happen before the URL is built"
    );

    // And the case the check is genuinely for: a base with a path of its own.
    let mut moved = reqwest::Url::parse("http://127.0.0.1:3456/api").expect("the base parses");
    moved.set_path("/v1/messages");
    assert!(
        !moved
            .as_str()
            .starts_with("http://127.0.0.1:3456/api".trim_end_matches('/')),
        "a base carrying a path has it replaced, and the result is outside that base: {moved}"
    );
}

/// **The three clamps, narrowest wins**, and which refusal each miss produces.
///
/// The measured term (`lendable`) is last, so an operator who granted a generous
/// fraction still cannot lend quota the owner's guard band does not leave.
#[test]
fn a_grant_is_clamped_by_the_narrowest_of_the_three() {
    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 600,
        max_inflight: 8,
    };
    let granted = LendGrant::new(Window::SevenDay, 0.20, 300, 2);

    // The operator's grant is the narrowest of the three.
    let grant = lease::clamp_to_grant(
        &ask,
        Some(granted.clone()),
        true,
        0.40,
        NOW,
        LEASE,
        None,
        None,
    );
    let minted = grant.lease.expect("0.20 is lendable, so a lease is minted");
    assert_eq!(minted.unit, LeaseUnit::Fraction(0.20));
    assert_eq!(minted.max_inflight, 2, "the grant's cap, not the ask's");
    assert_eq!(
        minted.expires_at_ms,
        NOW + 300_000,
        "the grant's ttl, not the ask's"
    );

    // The MEASURED headroom is the narrowest of the three.
    let tight = lease::clamp_to_grant(
        &ask,
        Some(granted.clone()),
        true,
        0.03,
        NOW,
        LEASE,
        None,
        None,
    );
    assert_eq!(
        tight.lease.map(|lease| lease.unit),
        Some(LeaseUnit::Fraction(0.03)),
        "the owner's own headroom clamps a grant it cannot back"
    );

    // Below MIN_DEBIT there is no lease to mint: the first request would
    // overdraw it. Nothing is spent, so the refusal names the owner's guard.
    let none_left = lease::clamp_to_grant(
        &ask,
        Some(granted.clone()),
        true,
        0.0005,
        NOW,
        LEASE,
        None,
        None,
    );
    assert_eq!(none_left.lease, None);
    assert_eq!(none_left.refusal, Some(LeaseRefusal::OwnerGuard));

    // The lender's own opt-in is off: SERVE is not on offer at all, whatever the
    // arithmetic would have said.
    let blind = lease::clamp_to_grant(
        &ask,
        Some(granted.clone()),
        false,
        0.40,
        NOW,
        LEASE,
        None,
        None,
    );
    assert_eq!(blind.lease, None);
    assert_eq!(blind.refusal, Some(LeaseRefusal::InspectNotGranted));

    // A grant for a DIFFERENT window is not a grant for this one.
    let other_window = LendGrant {
        window: Window::FiveHour,
        ..granted.clone()
    };
    let mismatch =
        lease::clamp_to_grant(&ask, Some(other_window), true, 0.40, NOW, LEASE, None, None);
    assert_eq!(mismatch.lease, None);
    assert_eq!(mismatch.refusal, Some(LeaseRefusal::InspectNotGranted));

    // A window, or a unit, this build does not know is refused rather than
    // guessed at.
    let unknown_window = LeaseRequest {
        window: Window::Unknown,
        ..ask
    };
    assert_eq!(
        lease::clamp_to_grant(
            &unknown_window,
            Some(granted.clone()),
            true,
            0.40,
            NOW,
            LEASE,
            None,
            None
        )
        .refusal,
        Some(LeaseRefusal::Unsupported)
    );
    let token_unit = LeaseRequest {
        unit: LeaseUnit::Tokens(1_000),
        ..ask
    };
    assert_eq!(
        lease::clamp_to_grant(
            &token_unit,
            Some(granted),
            true,
            0.40,
            NOW,
            LEASE,
            None,
            None
        )
        .refusal,
        Some(LeaseRefusal::Unsupported)
    );
}

/// The fallback provider refuses a client-credential path BEFORE it looks at a
/// lease, so the refusal cannot be reached around by configuring one.
///
/// It answers `None` ("not me"), which costs the caller only the next rung of
/// the ladder, and the last rung is the honest 429 the proxy already had.
///
/// The provider is pointed at a peers file that does not exist, which is the
/// control that makes this non-vacuous in the other direction: it proves the
/// path refusal runs BEFORE the peers file is read, because a provider that
/// read the file first would have logged a read failure and answered `None` for
/// the wrong reason.
#[test]
fn the_provider_declines_a_credential_path_without_consulting_a_lease() {
    let provider = PeerLeaseProvider::new(PathBuf::from("/nonexistent/tcr-peers.json"));
    assert_eq!(provider.name(), "peer-lease");

    for path in [
        "/v1/oauth/token/",
        "/api/oauth/organizations",
        "/v1/code",
        "/_tcr/accounts",
        "/_tcr/status",
    ] {
        let ask = ask_for(path);
        let answered = futures::executor::block_on(provider.try_serve(&ask));
        assert!(
            answered.is_none(),
            "{path} must never be served through a peer's account"
        );
    }
}

/// **The local control surface is never relayed** (hole C).
///
/// `/_tcr/…` is not an upstream path at all: behind it sits this proxy's own
/// control route, whose entire authorization is that the caller reached
/// loopback (`local_endpoint_gate`, `src/proxy.rs:1137`): and one of its
/// endpoints ADDS A LIVE CREDENTIAL. The lender serves a relayed request
/// through its OWN proxy on loopback, so a borrower that put `/_tcr/accounts`
/// in a frame would be handed that gate, having satisfied it by construction.
///
/// This is the one refusal in the list whose absence is REACHABLE end to end,
/// which is why `a_lender_refuses_a_local_control_path_in_a_frame` below drives
/// it over a real stream as well as asserting the predicate here.
///
/// Watch it fail by deleting the `LOCAL_PREFIX` arm of
/// `serve_is_allowed_for_path`.
#[test]
fn a_local_control_path_is_never_relayed() {
    for path in [
        "/_tcr",
        "/_tcr/status",
        "/_tcr/accounts",
        "/_tcr/accounts/add",
    ] {
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} is this proxy's own control surface and must never be relayed"
        );
    }
    // The control, and it is the same matcher the proxy's own guard uses on
    // this prefix: `/_tcrother` is NOT under `/_tcr`, and the proxy forwards it
    // upstream rather than answering it locally (`src/proxy.rs:2001`,
    // `path_is_under`). Refusing it here would make the two lists disagree
    // about what "local" means, which is the drift this file exists to avoid.
    assert!(serve::serve_is_allowed_for_path("/v1/messages"));
    assert!(serve::serve_is_allowed_for_path("/_tcrother"));
}

/// **A built SERVE frame carries no credential of any shape** (hole A).
///
/// The four credential names and every hop-by-hop request header are gone from
/// the frame, and the hop-by-hop list is `src/proxy.rs`'s own
/// (`is_request_hop_by_hop`) rather than a second copy here.
///
/// Asserted on the SERIALIZED frame as well as on the header list: a name that
/// survived under a different case, or a value that came along inside another
/// field, is invisible to a per-name check and visible in the bytes.
///
/// Watch it fail by deleting the `scrub_client_credentials` call in
/// `serve_request_from`.
#[test]
fn a_built_serve_frame_carries_no_client_credential() {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer not-a-real-token"),
        ("x-api-key", "not-a-real-key"),
        ("proxy-authorization", "Bearer not-a-real-proxy-key"),
        ("cookie", "sessionKey=not-a-real-cookie"),
        ("connection", "keep-alive"),
        ("content-type", "application/json"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).expect("a header value"),
        );
    }

    let ask = ask_for("/v1/messages");
    let frame = serve::serve_request_from(&ask, &headers, LEASE, REQUEST)
        .expect("an ordinary inference path builds a frame");

    let names: Vec<&str> = frame
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["content-type"],
        "only the request survives the scrub, not one credential and not one \
         hop-by-hop header"
    );

    let bytes = serde_json::to_vec(&frame).expect("the frame serializes");
    let wire = String::from_utf8(bytes).expect("the frame is utf-8");
    for secret in [
        "not-a-real-token",
        "not-a-real-key",
        "not-a-real-proxy-key",
        "not-a-real-cookie",
    ] {
        assert!(
            !wire.contains(secret),
            "{secret} reached the wire inside a SERVE frame"
        );
    }
    // The positive control: the frame is not empty, so the absences above are
    // about the scrub and not about a frame that never got built.
    assert!(wire.contains("/v1/messages"));
}

/// A frame is never built for a path a relay must not carry, and never for a
/// body one frame cannot hold.
///
/// Both are refusals rather than a truncation or a silent split: chunking a
/// body needs a sequence number, an end marker and a debit that survives a
/// half-delivered body, and inventing half of that is worse than saying no.
#[test]
fn a_frame_is_refused_for_a_forbidden_path_and_an_oversized_body() {
    let credential_path = Ask {
        query: None,
        path: "/v1/code",
        model: None,
        group: None,
        affinity: None,
        tried_local: 1,
        body: Bytes::from_static(b"{}"),
        method: "POST",
        headers: HeaderMap::new(),
    };
    assert!(
        serve::serve_request_from(&credential_path, &HeaderMap::new(), LEASE, REQUEST).is_err()
    );

    // A body over ONE FRAME is carried now, in as many frames as it takes: a
    // request that size is the routine shape of a long conversation, and
    // refusing it declined exactly the traffic this mesh exists for.
    let over_one_frame = Ask {
        query: None,
        path: "/v1/messages",
        model: None,
        group: None,
        affinity: None,
        tried_local: 1,
        body: Bytes::from(vec![b'x'; serve::MAX_BODY_BYTES + 1]),
        method: "POST",
        headers: HeaderMap::new(),
    };
    let built = serve::serve_request_from(&over_one_frame, &HeaderMap::new(), LEASE, REQUEST)
        .expect("a body over one frame is chunked, not refused");
    assert_eq!(built.body_bytes, serve::MAX_BODY_BYTES + 1);
    assert_eq!(
        built.flow,
        serve::SERVE_FLOW,
        "a frame this build writes says which flow it is going to use"
    );

    // What is still refused is a body over what the whole stream carries.
    let huge = Ask {
        query: None,
        path: "/v1/messages",
        model: None,
        group: None,
        affinity: None,
        tried_local: 1,
        body: Bytes::from(vec![b'x'; serve::MAX_RELAYED_BODY_BYTES + 1]),
        method: "POST",
        headers: HeaderMap::new(),
    };
    let refused = serve::serve_request_from(&huge, &HeaderMap::new(), LEASE, REQUEST)
        .expect_err("a body over the whole relay's ceiling is refused");
    assert!(
        refused
            .to_string()
            .contains(&serve::MAX_RELAYED_BODY_BYTES.to_string()),
        "the refusal names the ceiling it hit: {refused}"
    );
}

/// **A full pipe has its own wire word**: `in-flight-full`, not `lease-spent`.
///
/// The two tell a borrower opposite things: spent means stop asking, a full
/// pipe means retry in a moment. The variant was added to
/// `crates/tcr-peer-wire` under the decision granting it.
#[test]
fn a_full_pipe_refuses_with_its_own_wire_word() {
    assert_eq!(
        RelayRefusal::TooManyInflight { max: 2 }.to_wire(),
        LeaseRefusal::InFlightFull
    );
    // Every lease refusal passes through unchanged: the lender does not
    // re-interpret its own ledger's answer on the way to the wire.
    for refusal in [
        LeaseRefusal::LeaseExpired,
        LeaseRefusal::LeaseSpent,
        LeaseRefusal::OwnerGuard,
        LeaseRefusal::InspectNotGranted,
        LeaseRefusal::Unsupported,
    ] {
        assert_eq!(RelayRefusal::Lease(refusal).to_wire(), refusal);
    }
}

/// **`Ledger::grant` reads the row, mints an unguessable id, and records what it
/// minted.**
///
/// Against a real peers file, because the row lookup and its hot reload are half
/// of what this function does. The refusal arms are `clamp_to_grant`'s and are
/// tested above without a file.
#[test]
fn a_grant_reads_the_peers_file_and_records_what_it_minted() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let borrower = PeerId([7_u8; 32]);
    let store = lender_store(
        &peers_path,
        borrower,
        vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
    );

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 600,
        max_inflight: 8,
    };

    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    let first = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer;
    let lease = first.lease.expect("0.20 is granted and 0.40 is lendable");
    assert_eq!(lease.unit, LeaseUnit::Fraction(0.20));
    assert_eq!(lease.max_inflight, 2);
    assert_eq!(
        ledger.live(tcr::now_ms()).len(),
        1,
        "a minted lease is in the ledger, or the lender cannot honour it"
    );

    // A second grant is a DIFFERENT lease id. 128 bits from the platform
    // CSPRNG, so a collision here is not a flake: it is the random source
    // having stopped being random, which is the failure that matters.
    let second = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("a second lease");
    assert_ne!(lease.lease_id, second.lease_id);

    // An unpinned peer gets no lease at all, whatever it asks for.
    let stranger = PeerId([9_u8; 32]);
    assert_eq!(
        ledger
            .grant(&stranger, &ask, &store, &serve::NoFleetUtilization)
            .answer
            .refusal,
        Some(LeaseRefusal::InspectNotGranted)
    );
}

/// **The provider is configured from the peers file, on the BORROWING grant.**
///
/// A lease needs an explicit act on both machines and the act on this one is
/// `disclose`. A row with `inspect` says the opposite thing: that this Mac
/// LENDS, and must not turn the borrowing seam on, which is the confusion this
/// test exists to pin.
///
/// Reads through a `PeerStore` since the filter became `dial::has_a_way_back`:
/// answering "is there a Mac that could carry to this lender" needs the other
/// rows, not just this one.
#[test]
fn the_provider_is_configured_by_a_disclose_grant_and_an_address() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let lender = PeerId([3_u8; 32]);

    let mut file = PeerFile {
        peers: vec![borrower_row_for(lender, false, Vec::new())],
        ..PeerFile::default()
    };
    let store_of = |file: &PeerFile| {
        tcr::peer::config::save(&peers_path, file).expect("write the peers file");
        PeerStore::open(&peers_path).expect("open the peers file")
    };
    assert!(
        fallback::peer_lease_provider(&store_of(&file)).is_none(),
        "a bare pin borrows nothing"
    );

    // `inspect` is the LENDING direction and does not configure this seam.
    file.row_mut(&lender).expect("the lender row").allow.inspect = true;
    file.row_mut(&lender)
        .expect("the lender row")
        .observe_endpoint(Endpoint::direct(
            "127.0.0.1:1"
                .parse()
                .expect("a fixture address is a socket address"),
            0,
            EndpointSource::Paired,
        ));
    assert!(
        fallback::peer_lease_provider(&store_of(&file)).is_none(),
        "lending to a peer is not permission to disclose to it"
    );

    // With `disclose` and an address there is something to borrow from.
    file.row_mut(&lender)
        .expect("the lender row")
        .allow
        .allow_disclose = true;
    assert!(fallback::peer_lease_provider(&store_of(&file)).is_some());

    // A WAY BACK is part of it: a row with nothing to dial and no Mac that
    // could carry to it is a row that answers nothing, and the provider would
    // walk it on every dry-fleet request forever. An address is one way back;
    // `a_lender_with_no_address_still_installs_the_provider_through_a_carrier`
    // (`tests/peer_forward.rs`) holds the other.
    file.row_mut(&lender)
        .expect("the lender row")
        .endpoints
        .clear();
    assert!(fallback::peer_lease_provider(&store_of(&file)).is_none());
}

/// **A `Control::LeaseGrant` round-trips.** Measured, because it did not: the
/// whole lease arm was unreachable for a reason neither half of it could see.
///
/// [`tcr_peer_wire::Control`] is internally tagged, so serde deserializes it by
/// buffering the frame into its private `Content` tree first: and that tree has
/// no 128-bit arm, so a `u128` field inside any variant answers
/// `Err("u128 is not supported")` on the way back IN while serializing
/// perfectly on the way out. A lender that granted a lease and a borrower that
/// reported "a frame did not parse" were both telling the truth.
///
/// Watched red: revert `#[serde(with = "id_hex")]` on
/// `tcr_peer_wire::Lease::lease_id` and this test fails with that exact
/// message, while the assertion below it (the value survives) still passes :
/// which is the shape of the defect: the send side never noticed.
#[test]
fn a_lease_grant_survives_the_control_envelope() {
    let lease = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        granted_at_ms: NOW,
        expires_at_ms: NOW + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    };
    let message = tcr_peer_wire::Control::LeaseGrant(tcr_peer_wire::LeaseGrant {
        lease: Some(lease),
        refusal: None,
    });

    let bytes = serde_json::to_vec(&message).expect("a grant serializes");
    let back: tcr_peer_wire::Control =
        serde_json::from_slice(&bytes).expect("and a grant must PARSE BACK");
    assert_eq!(
        back, message,
        "the lease id must survive the envelope exactly, not approximately"
    );

    // The id is a STRING on the wire, which is the second reason for the shape:
    // a `u128` as a JSON number is a number no double can hold, so any reader
    // that parses JSON into doubles rounds it and then revokes a lease nobody
    // minted.
    let json = String::from_utf8(bytes).expect("the frame is utf-8");
    assert!(
        json.contains(&format!("\"leaseId\":\"{LEASE:032x}\"")),
        "the lease id goes on the wire as 32 hex characters: {json}"
    );

    // And a receipt, which has the identical shape and had the identical
    // defect.
    let receipt = tcr_peer_wire::Control::LeaseReceipt(tcr_peer_wire::LeaseReceipt {
        lease_id: LEASE,
        request_id: REQUEST,
        spent: 0.031,
    });
    let bytes = serde_json::to_vec(&receipt).expect("a receipt serializes");
    let back: tcr_peer_wire::Control =
        serde_json::from_slice(&bytes).expect("and a receipt must parse back");
    assert_eq!(back, receipt);
}

/// **The lender's ledger survives a restart**: the review's M2, which promised
/// exactly this in two doc-comments while nothing serialized anything.
///
/// The grantee and the scope are restored with the lease, because a lease
/// restored without them is a bearer token drawing on the whole fleet: the two
/// defects fixed together (M1 and the lease-scope rule) would both come back at the
/// first reboot.
///
/// Watched red: return early from `Ledger::persist` and `restored` is 0.
#[test]
fn the_ledger_is_persisted_and_restored_with_its_grantee_and_scope() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    let grantee = PeerId([46_u8; 32]);
    let scope = tcr_peer_wire::LendScope::Group("work".to_string());

    let now = tcr::now_ms();
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.granted_at_ms = now;
    live.expires_at_ms = now + 300_000;
    // And one already dead, so the `expired=M` half of the log line has
    // something to count and the restore has something to drop.
    let mut dead = lease_with(LEASE + 1, 0.500, 0.0);
    dead.expires_at_ms = now - 1;

    {
        let mut ledger = Ledger::restored_from(&state);
        ledger.record_scoped(live, grantee, scope.clone());
        ledger.record_scoped(dead, grantee, tcr_peer_wire::LendScope::All);
        ledger.note_owner_headroom(Window::SevenDay, 0.30);
        // The charge has to survive too: `spent` is the ceiling H2 exists to
        // keep binding, and a restart that forgot it would reopen that hole one
        // reboot wide.
        assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, now), Ok(()));
        ledger.leave_relay(LEASE);
        assert!(ledger.debit(LEASE, REQUEST, 0.031) > 0.0);
        // The flush is what writes a CHARGE now: `debit` used to persist on
        // the relay path, holding the ledger mutex through a locked file round
        // trip per request. `listener::flush_ledger_periodically` is this call
        // in production, and `a_charge_is_written_by_the_flush_and_not_by_the_relay`
        // is the gate on the split. A grant is still durable without it, which
        // is why the two rows above need no flush.
        ledger.flush();
    }

    let restored = Ledger::restored_from(&state);
    let rows = restored.live(now);
    assert_eq!(
        rows.len(),
        1,
        "the live lease is restored and the expired one is dropped: {rows:?}"
    );
    assert_eq!(rows[0].lease_id, LEASE);
    assert!(
        (rows[0].spent - 0.031).abs() < 1e-9,
        "the spend survives, so the ceiling still binds: {}",
        rows[0].spent
    );
    assert_eq!(
        restored.grantee_of(LEASE),
        Some(grantee),
        "the grantee survives, so the lease is not restored as a bearer token"
    );
    assert_eq!(
        restored.scope_of(LEASE),
        scope,
        "and the scope survives, so it is not restored drawing on the whole fleet"
    );

    // A restored lease is spendable by its grantee and by nobody else, which is
    // the property the restore exists to keep rather than merely the row.
    let mut restored = restored;
    restored.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(
        restored.enter_relay(LEASE, &PeerId([47_u8; 32]), REQUEST + 9, now),
        Err(RelayRefusal::NotTheGrantee)
    );
    assert_eq!(
        restored.enter_relay(LEASE, &grantee, REQUEST + 9, now),
        Ok(())
    );
}

/// **A lease with no grantee is never persisted and never restored.**
///
/// `Ledger::persist` used to write the all-zero peer id for a lease it held no
/// grantee for, and `state::LeaseRow` called that "the honest round-trip of a
/// ledger row that had none". It is not honest: `enter_relay` answers
/// `NotTheGrantee` to every peer asking against such a lease, so nobody can
/// spend it, while `live` and `committed_fraction` still count it: a restart would
/// bring back a row that holds this lender's headroom for its whole TTL on
/// nobody's behalf.
///
/// Both ends are measured, because the file is hand-editable JSON and an older
/// build really did write the zero id: the writer drops the lease, and the
/// reader drops a row that carries one.
///
/// Watched red: restore the `.unwrap_or(PeerId([0_u8; 32]))` fallback in
/// `Ledger::persist` and the first half fails with one row on disk; drop the
/// all-zero check from `Ledger::restored_from` and the second half restores the
/// unspendable lease.
#[test]
fn a_lease_with_no_grantee_is_neither_persisted_nor_restored() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");

    let now = tcr::now_ms();
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.granted_at_ms = now;
    live.expires_at_ms = now + 300_000;

    // The writer. `record` is the entry point that takes no grantee, which is
    // exactly the row this rule is about.
    {
        let mut ledger = Ledger::restored_from(&state);
        ledger.record(live);
        ledger.note_owner_headroom(Window::SevenDay, 0.30);
        assert_eq!(
            ledger.live(now).len(),
            1,
            "in memory it is a lease; the question is what a restart is told"
        );
        // `record` does not persist on its own: `record_scoped` and `debit`
        // do: so the write is forced through the same path a granted lease
        // takes, which is `record_scoped` for a DIFFERENT, granted lease.
        let mut second = lease_with(LEASE + 1, 0.500, 0.0);
        second.granted_at_ms = now;
        second.expires_at_ms = now + 300_000;
        ledger.record_scoped(second, PeerId([48_u8; 32]), tcr_peer_wire::LendScope::All);
    }

    let on_disk = tcr::peer::state::load(&state, now).expect("the state file reads");
    assert_eq!(
        on_disk.leases.len(),
        1,
        "only the granted lease is written: {:?}",
        on_disk.leases
    );
    assert_eq!(on_disk.leases[0].lease.lease_id, LEASE + 1);
    assert_eq!(on_disk.leases[0].peer, PeerId([48_u8; 32]));

    // The reader, against a file an older build could have left behind.
    let mut planted = on_disk;
    planted.leases.push(tcr::peer::state::LeaseRow {
        lease: live,
        peer: PeerId([0_u8; 32]),
        scope: tcr_peer_wire::LendScope::All,
    });
    tcr::peer::state::save(&state, &planted).expect("plant the zero-grantee row");

    let restored = Ledger::restored_from(&state);
    let ids: Vec<u128> = restored
        .live(now)
        .iter()
        .map(|lease| lease.lease_id)
        .collect();
    assert_eq!(
        ids,
        vec![LEASE + 1],
        "a row with no grantee is dropped at restore, not brought back to hold headroom"
    );
    assert_eq!(
        restored.grantee_of(LEASE),
        None,
        "and nothing about it is remembered"
    );
}

/// **A relay charges without writing a file; the flush writes it.**
///
/// `Ledger::debit` called `persist`, which takes `config::FileLock`, loads the
/// whole state file and saves it: and `serve::handle_serve_on` calls `debit`
/// with the ledger's own mutex held, once per relayed request. So the relay
/// path paid a locked file round trip per request while holding the lock every
/// other relay, `tcr peer ls` and the panel row needs.
///
/// A grant still writes immediately: it is rare, nothing is in flight against
/// it, and it is the row a restart cannot reconstruct. A charge marks the
/// ledger dirty and the lender's debounced flusher
/// (`listener::flush_ledger_periodically`) writes it.
///
/// Watched red: put `self.persist();` back in `Ledger::debit` in place of
/// `self.dirty = true;`, the on-disk `spent` reads 0.031 before any flush and
/// `is_dirty` is false, so both halves of this fail.
#[test]
fn a_charge_is_written_by_the_flush_and_not_by_the_relay() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    let grantee = PeerId([49_u8; 32]);

    let now = tcr::now_ms();
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.granted_at_ms = now;
    live.expires_at_ms = now + 300_000;

    let mut ledger = Ledger::restored_from(&state);
    ledger.record_scoped(live, grantee, tcr_peer_wire::LendScope::All);
    ledger.note_owner_headroom(Window::SevenDay, 0.30);
    assert!(
        !ledger.is_dirty(),
        "the grant itself wrote, so nothing is pending"
    );
    let granted = tcr::peer::state::load(&state, now).expect("the state file reads");
    assert_eq!(
        granted.leases.len(),
        1,
        "a grant is durable the instant it is made: {:?}",
        granted.leases
    );

    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, now), Ok(()));
    ledger.leave_relay(LEASE);
    assert!(ledger.debit(LEASE, REQUEST, 0.031) > 0.0);

    let after_charge = tcr::peer::state::load(&state, now).expect("the state reads");
    assert!(
        (after_charge.leases[0].lease.spent - 0.0).abs() < 1e-9,
        "the charge must not have gone to disk on the relay path: {}",
        after_charge.leases[0].lease.spent
    );
    assert!(ledger.is_dirty(), "it is pending instead");

    assert!(ledger.flush(), "the flush has something to write");
    let after_flush = tcr::peer::state::load(&state, now).expect("the state reads");
    assert!(
        (after_flush.leases[0].lease.spent - 0.031).abs() < 1e-9,
        "and the flush writes it, so a restart still sees the ceiling: {}",
        after_flush.leases[0].lease.spent
    );
    assert!(
        !ledger.flush(),
        "a second flush with nothing pending takes no lock and writes nothing"
    );
}

/// **A relay returns while the state file's lock is held by somebody else.**
///
/// This is the cost the last test's defect actually had. `config::FileLock`
/// waits out `LOCK_STALE_MS` before it breaks a lockfile whose holder may
/// still be alive, so with `persist` on the relay path an operator running
/// `tcr peer accept` (one locked read-modify-write of the same file), stalled
/// every relayed request on this Mac for seconds, each one holding the ledger
/// mutex while it waited.
///
/// Watched red: put `self.persist();` back in `Ledger::debit` and the charge
/// takes 2 s (`LOCK_STALE_MS`, the point at which the held lock is broken),
/// well past the bound below.
#[test]
fn a_charge_returns_while_the_state_file_lock_is_held() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    let grantee = PeerId([50_u8; 32]);

    let now = tcr::now_ms();
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.granted_at_ms = now;
    live.expires_at_ms = now + 300_000;

    let mut ledger = Ledger::restored_from(&state);
    ledger.record_scoped(live, grantee, tcr_peer_wire::LendScope::All);
    ledger.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, now), Ok(()));
    ledger.leave_relay(LEASE);

    // Somebody else's read-modify-write of the same file, mid-flight.
    let held = tcr::peer::config::FileLock::acquire(&state).expect("take the file lock first");

    let started = std::time::Instant::now();
    let charge = ledger.debit(LEASE, REQUEST, 0.031);
    let elapsed = started.elapsed();
    drop(held);

    assert!(charge > 0.0, "the charge is still made");
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "a relay must not wait on another process's lock: the charge took {elapsed:?}, and \
         the lock it used to queue behind is only broken after {}ms",
        tcr::peer::config::LOCK_STALE_MS
    );
    assert!(
        ledger.is_dirty(),
        "the charge is pending for the debounced flusher, which is the one thing that waits"
    );
}

/// **A write of the lease rows over a QUARANTINED state file is refused.**
///
/// `state::save_leases` is a locked read-modify-write, and its read is
/// `state::load`: which answers an untrusted file with an empty state, because
/// for a READER this file is a cache and losing it costs a cold start. For this
/// writer that was data loss dressed as a healthy file: the lease rows were
/// written on top of an empty knock queue, no mutes, no bans and no accepted
/// pairing windows, which this file's own docs call "the whole of the
/// authorization for a first pairing".
///
/// So it refuses, naming the file and where the evidence went, and the next
/// boot starts cold on its own terms.
///
/// Watched red: change `save_leases` back to `let mut state = load(path,
/// crate::now_ms())?;` with no origin check, it returns `Ok(())`, a fresh
/// `peer-state.json` appears carrying the lease row and an empty `banned`, and
/// the operator's block list is gone.
#[test]
fn writing_lease_rows_over_a_quarantined_state_file_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");

    // An operator's own decisions, written by the one writer that creates this
    // file at 0600.
    let mut value = tcr::peer::state::PeerState::default();
    value.banned.push(tcr::peer::state::Ban {
        addr: "192.0.2.61".to_string(),
        key: None,
        since_ms: NOW,
        reason: tcr::peer::state::BanReason::Blocked,
    });
    tcr::peer::state::save(&path, &value).expect("the state file writes");

    // Something widened it, so `load` will not trust it.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("widen it by hand");

    let row = tcr::peer::state::LeaseRow {
        lease: lease_with(LEASE, 0.500, 0.0),
        peer: PeerId([51_u8; 32]),
        scope: tcr_peer_wire::LendScope::All,
    };
    let refusal = tcr::peer::state::save_leases(&path, std::slice::from_ref(&row))
        .expect_err("a quarantined read must refuse the write");
    let refusal = format!("{refusal:#}");
    assert!(
        refusal.contains(&path.display().to_string()),
        "the refusal names the file: {refusal}"
    );
    assert!(
        refusal.contains(".corrupt-"),
        "and where the evidence went: {refusal}"
    );
    assert!(
        !path.exists(),
        "no partial state is published in its place: the next boot starts cold and says so"
    );

    // And the ordinary cases still write. A MISSING file is not this case:
    // there is nothing to lose and a first grant has to create it.
    tcr::peer::state::save_leases(&path, std::slice::from_ref(&row))
        .expect("a missing state file is created by the first write of a lease row");
    let created = tcr::peer::state::load(&path, NOW).expect("it reads back");
    assert_eq!(created.leases.len(), 1);

    // A TRUSTED file keeps every other key through the same write.
    let mut trusted = created;
    trusted.banned.push(tcr::peer::state::Ban {
        addr: "192.0.2.62".to_string(),
        key: None,
        since_ms: NOW,
        reason: tcr::peer::state::BanReason::Blocked,
    });
    tcr::peer::state::save(&path, &trusted).expect("write the ban");
    tcr::peer::state::save_leases(&path, &[]).expect("clearing the lease rows writes");
    let after = tcr::peer::state::load(&path, NOW).expect("it reads back");
    assert!(
        after.leases.is_empty(),
        "the lease rows are the ones replaced"
    );
    assert_eq!(
        after.banned.len(),
        1,
        "and the operator's block list survives a lease write, as it always did"
    );
}
