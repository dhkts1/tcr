// ---------------------------------------------------------------------------
// What a second, overlapping or ninth lease is cut out of
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// **A second ask is cut out of what is LEFT, not out of the whole.**
///
/// The review's finding: `Ledger::grant` took `lendable` from `headroom_for`
/// alone, which is what this Mac's fleet has left, and subtracted nothing for
/// the leases it had already handed out. A borrower that re-asked therefore
/// held up to `MAX_LEASES_PER_PEER` leases at a time, EACH clamped to the full
/// fraction: eight asks promised eight times the room that existed, and every
/// one of them passed `may_relay` until the account's own quota ran out.
///
/// Measured on the fractions the ledger minted, not on a log line: 0.30 of
/// room, a grant ceiling of 0.20, so the first lease is 0.20 and the second
/// can only be 0.10.
///
/// Watch it fail by dropping the `- self.committed_fraction(..)` term from
/// `Ledger::grant`: the second lease is 0.20 as well, and the two together
/// promise 0.40 of a 0.30 window.
#[test]
fn a_second_ask_is_cut_out_of_what_is_left_not_out_of_the_whole() {
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
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 600,
        max_inflight: 2,
    };

    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    let first = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("the first ask mints against 0.30 of room");
    assert_eq!(
        first.unit,
        LeaseUnit::Fraction(0.20),
        "the first lease is the grant's own ceiling"
    );
    assert!(
        (ledger.committed_fraction(
            &tcr_peer_wire::LendScope::All,
            Window::SevenDay,
            tcr::now_ms()
        ) - 0.20)
            .abs()
            < 1e-9,
        "and 0.20 of the window is now promised"
    );

    let second = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("0.10 is still lendable, so the second ask mints a smaller lease");
    let LeaseUnit::Fraction(fraction) = second.unit else {
        panic!("a fraction lease was asked for and a fraction lease must come back");
    };
    assert!(
        (fraction - 0.10).abs() < 1e-9,
        "the second lease is what is LEFT (0.30 - 0.20), not the grant ceiling again: \
         {fraction}"
    );
}

/// **Two scopes over the same accounts subtract from each other.**
///
/// The review's finding: `committed_fraction` counted only live leases whose
/// scope STRING matched the new ask's, while `headroom_for` combines the two
/// figures with `scoped.min(all)` and so treats them as one pool. An `all`
/// lease and a `group:work` lease over the same accounts therefore never
/// subtracted from each other, and the same headroom was promised twice: the
/// `all` lease could spend the whole window and the group lease still read as
/// fully funded.
///
/// Measured on the figure itself rather than through a mint, because the mint
/// needs a fleet reader that can enforce a group and the claim under test is
/// the arithmetic. Both directions are asserted: the group ask sees the `all`
/// lease, and the `all` ask sees the group lease. A scope this ledger CAN
/// prove disjoint is still disjoint, which is the third leg: two account sets
/// that name nobody in common do not subtract.
///
/// Watch it fail by restoring the `scope_key(..) == key` filter in
/// `Ledger::committed_fraction`: the first two assertions read 0.0, and the
/// ledger says nothing is promised while two leases are live.
#[test]
fn a_lease_on_an_overlapping_scope_is_already_promised_room() {
    let now = tcr::now_ms();
    let borrower = PeerId([7_u8; 32]);
    let work = tcr_peer_wire::LendScope::Group("work".to_string());
    let all = tcr_peer_wire::LendScope::All;
    let live = |lease_id: u128, budget: f64| Lease {
        lease_id,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(budget),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    };

    let mut ledger = Ledger::new();
    ledger.record_scoped(live(0xA11, 0.20), borrower, all.clone());
    ledger.record_scoped(live(0xB22, 0.05), borrower, work.clone());

    assert!(
        (ledger.committed_fraction(&work, Window::SevenDay, now) - 0.25).abs() < 1e-9,
        "a `group:work` ask is cut out of what is left after BOTH leases, and the ledger \
         says {}",
        ledger.committed_fraction(&work, Window::SevenDay, now)
    );
    assert!(
        (ledger.committed_fraction(&all, Window::SevenDay, now) - 0.25).abs() < 1e-9,
        "and so is an `all` ask, in the other direction: {}",
        ledger.committed_fraction(&all, Window::SevenDay, now)
    );

    // The one pair this ledger can prove apart without the manager's group
    // membership: two account sets naming nobody in common.
    let mut named = Ledger::new();
    named.record_scoped(
        live(0xC33, 0.20),
        borrower,
        tcr_peer_wire::LendScope::Accounts(vec!["alice".to_string()]),
    );
    assert_eq!(
        named.committed_fraction(
            &tcr_peer_wire::LendScope::Accounts(vec!["bob".to_string()]),
            Window::SevenDay,
            now
        ),
        0.0,
        "`account:bob` draws from nothing `account:alice` holds, so nothing is promised"
    );
    assert!(
        (named.committed_fraction(
            &tcr_peer_wire::LendScope::Accounts(vec!["bob".to_string(), "alice".to_string()]),
            Window::SevenDay,
            now
        ) - 0.20)
            .abs()
            < 1e-9,
        "and a set that names alice as well does overlap"
    );
}

/// **A busy borrower cannot evict another lease's replay defence, nor its
/// own.**
///
/// The review's finding: the served cache was one FIFO for the whole process,
/// bounded by `SERVED_CAPACITY`. A borrower with requests of its own to make
/// pushed every other lease's `(lease, request)` pair out of it, and the
/// replay defence for those leases went quiet: a request id already served for
/// them was admitted again. It could also evict its OWN older pair and then
/// replay that.
///
/// Both halves are measured here, at the bound itself rather than at a
/// plausible-looking number: one lease fills its own cache and the OTHER
/// lease's remembered pair still refuses a replay.
///
/// Watch it fail by putting `admit_pair`'s eviction back on the whole cache
/// (`while cache-wide length >= SERVED_CAPACITY { drop the global oldest }`
/// with no per-lease bound): the quiet lease's pair is gone and the replay is
/// admitted.
#[test]
fn a_busy_lease_cannot_evict_another_leases_replay_defence() {
    let quiet = 0x9111_u128;
    let busy = 0x9222_u128;
    let peer = PeerId([7_u8; 32]);
    let now = tcr::now_ms();

    let mut ledger = Ledger::new();
    for lease_id in [quiet, busy] {
        ledger.record_scoped(
            Lease {
                lease_id,
                window: Window::SevenDay,
                unit: LeaseUnit::Fraction(0.20),
                granted_at_ms: now,
                expires_at_ms: now + 600_000,
                spent: 0.0,
                max_inflight: 64,
                until: None,
            },
            peer,
            tcr_peer_wire::LendScope::All,
        );
    }
    ledger.note_owner_headroom(Window::SevenDay, 0.90);

    // The quiet lease serves one request and releases its slot.
    ledger
        .enter_relay(quiet, &peer, 1, now)
        .expect("the quiet lease's first request is admitted");
    ledger.leave_relay(quiet);

    // The busy one serves more than the whole process used to remember.
    for request_id in 0..(lease::SERVED_CAPACITY as u128 + 16) {
        if ledger.enter_relay(busy, &peer, request_id, now).is_ok() {
            ledger.leave_relay(busy);
        }
    }

    assert!(
        ledger.served_len_for(busy) <= lease::SERVED_PER_LEASE_CAPACITY,
        "a lease may not remember more ids than its own bound: {}",
        ledger.served_len_for(busy)
    );
    assert_eq!(
        ledger.enter_relay(quiet, &peer, 1, now),
        Err(RelayRefusal::Replayed),
        "the quiet lease's one served pair must still be remembered: a busy borrower may \
         not turn another lease's replay defence off"
    );
}

/// **One pinned peer may hold eight live leases here, and the ninth ask is
/// refused**: the review's finding.
///
/// Nothing capped how many leases one peer could mint, and every mint rewrites
/// the whole state file under the same `FileLock` that `tcr peer accept` and
/// `tcr peer block` take with a hard five-second give-up: an authenticated peer
/// asking in a loop was both an unbounded ledger and an operator whose Accept
/// could not get the lock.
///
/// Nine asks, all identical and all from a peer this Mac lends to. Eight rows,
/// a refusal on the ninth, and the eight are untouched: a cap that refused by
/// dropping an older lease would break the borrower it was granted to.
///
/// A SECOND peer still gets its lease, which is what makes this a per-grantee
/// cap rather than a per-Mac one: one noisy borrower must not close the lender
/// to every other Mac in the mesh.
///
/// Watch it fail by raising `MAX_LEASES_PER_PEER` to 9: the ninth ask mints and
/// `ninth.lease.is_none()` goes red. (Measured: the first draft of this test
/// looped to the constant itself and stayed GREEN under exactly that mutation,
/// which is why the eight and the nine below are literals.)
#[test]
fn a_ninth_live_lease_for_one_peer_is_refused_and_the_eight_stand() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let borrower = PeerId([7_u8; 32]);
    let other = PeerId([8_u8; 32]);
    write_peers(
        &peers_path,
        vec![
            lender_row_for(
                borrower,
                vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
            ),
            lender_row_for(other, vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)]),
        ],
    );
    let store = PeerStore::open(&peers_path).expect("the peers file opens");
    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 600,
        max_inflight: 8,
    };

    let mut ledger = Ledger::new();
    // ENOUGH ROOM FOR EIGHT, because this test is about the COUNT cap and
    // nothing else. It used to note 0.40, which was room for two leases of
    // 0.20 and eight leases only because `grant` did not subtract what it had
    // already promised: the third ask minted a full fraction out of headroom
    // that was already spoken for. `Ledger::committed_fraction` subtracts it
    // now, so a fixture that wants eight leases has to have room for eight,
    // and the ask that runs out of room is
    // `a_second_ask_is_cut_out_of_what_is_left_not_out_of_the_whole`.
    ledger.note_owner_headroom(Window::SevenDay, 2.0);
    // EIGHT as a literal, and nine asks as a literal: a loop bounded by
    // `MAX_LEASES_PER_PEER` would pass against any value of it, which is a test
    // of nothing. The constant is asserted separately, as the contract it is.
    assert_eq!(
        lease::MAX_LEASES_PER_PEER,
        8,
        "this test is written against a cap of eight"
    );
    let mut minted = Vec::new();
    for ask_number in 1..=8 {
        let grant = ledger
            .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
            .answer;
        minted.push(
            grant
                .lease
                .unwrap_or_else(|| panic!("ask {ask_number} is inside the cap and must mint")),
        );
    }

    let ninth = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer;
    assert!(
        ninth.lease.is_none(),
        "the ninth live lease for one peer must not be minted"
    );
    assert_eq!(
        ninth.refusal,
        Some(LeaseRefusal::TooManyLeases),
        "the ninth ask is answered with a refusal frame naming the count cap, not the owner's \
         guard: a borrower that reads one for the other either hammers a dead lease or \
         backs off from a live one for the wrong reason"
    );

    let now_ms = tcr::now_ms();
    assert_eq!(
        ledger.live_for(&borrower, now_ms),
        8,
        "the refusal leaves the leases this peer already holds exactly where it found them"
    );
    let live: Vec<u128> = ledger
        .live(now_ms)
        .iter()
        .map(|lease| lease.lease_id)
        .collect();
    for lease in &minted {
        assert!(
            live.contains(&lease.lease_id),
            "a cap that evicted an already-granted lease would break the borrower \
             holding it"
        );
    }

    // The cap is per GRANTEE: another Mac is not refused because this one asked
    // nine times.
    assert!(
        ledger
            .grant(&other, &ask, &store, &serve::NoFleetUtilization)
            .answer
            .lease
            .is_some(),
        "a second peer's first lease is refused by a cap that counts per Mac instead \
         of per grantee"
    );
}

/// **Five concurrent first asks against one lender mint ONE lease.**
///
/// The race measured live: `PeerLeaseProvider::lease_for` read
/// its cache under the mutex, missed, RELEASED the lock, then asked the lender
/// over CONTROL. Five borrows arriving together all missed, all asked, and all
/// got a lease of their own: so `max_inflight`, which bounds requests within
/// ONE lease, bound nothing across them, and the only ceiling left was the
/// lender's eight-lease-per-peer cap. Five concurrent borrows were five served
/// and zero refused.
///
/// The count is the lender's own `Control::LeaseRequest` frames and not the
/// leases its ledger holds: a lender that read five asks and minted one lease
/// would be a lender with a deduplicating ledger, which is a different fix in a
/// different process, and this gate is about what the BORROWER sends.
///
/// Watch it fail by deleting the ask-gate block in `lease_for` (the
/// `let _asking = gate.lock().await;` and the cache re-read under it): the
/// counter reads 5.
#[tokio::test(flavor = "multi_thread")]
async fn five_concurrent_first_asks_mint_one_lease() {
    let LeasePair {
        lender_home,
        borrower_home,
        lender_peers,
        borrower_peers,
        lender_id,
        borrower_id: _,
    } = lease_pair(LendGrant::new(Window::SevenDay, 0.20, 300, 2));

    let asks: mesh::LeaseAsks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Long enough that the four later asks are inside the first one's round
    // trip on any machine this suite runs on, and short enough that the test
    // costs a third of a second.
    let peer_addr = mesh::spawn_counting_lease_lender(
        lender_home.path().to_path_buf(),
        lender_peers.clone(),
        Arc::clone(&asks),
        std::time::Duration::from_millis(300),
    )
    .await;
    point_borrower_at(&borrower_peers, lender_id, peer_addr);

    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let borrows = (0..5).map(|_| {
        let provider = &provider;
        async move {
            let ask = ask_for("/v1/messages");
            provider
                .try_serve(&ask)
                .await
                .map(|response| response.status().as_u16())
        }
    });
    let answers = futures::future::join_all(borrows).await;

    assert_eq!(
        asks.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "five concurrent first borrows against one lender ask it for ONE lease; \
         a borrower that asks per request holds one lease per request and \
         `max_inflight` binds nothing"
    );
    assert_eq!(
        answers,
        vec![Some(200); 5],
        "every borrow is served: single-flighting the ASK must not refuse a \
         request, only share the lease it waits for"
    );

    let state_path = borrower_home.path().join("peer-state.json");
    let state = tcr::peer::state::load(&state_path, tcr::now_ms())
        .expect("the borrower's state file reads back");
    assert_eq!(
        state.borrowed.len(),
        1,
        "one lease was granted, so one borrowed row is on disk"
    );
}
