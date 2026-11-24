//! The caps on what a stranger can make this Mac keep: found rows, refusal log
//! lines, queued knocks and the reservations that hold them, and an accepted
//! window that has run out.

use super::*;

// ===========================================================================
// Cap: 12 found rows shown, 2 per address, 60 s row TTL
// (`discovery::MAX_FOUND_ROWS`, `discovery::MAX_FOUND_PER_ADDRESS`)
// ===========================================================================

/// One announcement, as a row that has already passed the inbound checks.
fn found_row(instance: u8, addr: &str) -> Discovered {
    Discovered {
        instance_id: InstanceId([instance; INSTANCE_ID_BYTES]),
        name: None,
        addrs: vec![addr.to_string()],
        port: 9600,
        sources: vec![FoundSource::Lan],
    }
}

/// **One host that rotates BOTH its instance id and its announced address takes
/// a held row each, and the twelve-row display cap still holds.**
///
/// The per-address cap is the one an id changer meets, and
/// `one_address_holds_at_most_two_found_rows` (`tests/peer_discovery.rs`)
/// proves it against a rotating id. This attacks the cap's KEY instead: the
/// address it counts by is the one the announcement carries
/// (`Discovered::addrs`), and `Discovered` holds no source address at all, so
/// an announcer that varies what it claims as its address is a new "address"
/// every time. Announcements are UDP multicast and forgeable down to their
/// source (`abuse-resistance.md`'s own first table row), so this costs an
/// attacker nothing.
///
/// What holds is the display cap, which is what the operator sees, and the
/// honest footer count. What does not hold is the per-address bound on rows
/// HELD: this test measures the real cost rather than asserting a cap the type
/// cannot enforce, and the finding is reported with it.
#[test]
fn one_host_rotating_its_announced_address_still_shows_only_twelve_rows() {
    let mut found = FoundList::new();
    let flood: Vec<Discovered> = (0..40_u8)
        .map(|n| found_row(n, &format!("192.0.2.{n}")))
        .collect();
    found.observe(flood, 1_000);

    assert_eq!(
        found.shown().len(),
        plan::MAX_FOUND_ROWS,
        "the display cap is what the operator sees and it must hold whatever arrives: {} \
         rows shown",
        found.shown().len()
    );
    assert_eq!(
        found.not_shown(),
        found.len() - plan::MAX_FOUND_ROWS,
        "the footer count must be honest about what is held back"
    );

    // The measurement behind the finding: forty rotations from one announcer
    // are forty HELD rows, because the per-address cap counts the address the
    // announcement claims and nothing in `Discovered` records where it came
    // from.
    assert_eq!(
        found.len(),
        40,
        "a rotation per row is a row per rotation; this is the cost the per-address cap does \
         not bound"
    );

    // And the cap does still bind when the claimed address repeats, which is
    // the positive control on the assertion above: it is the KEY that is weak,
    // not the counting.
    let mut repeated = FoundList::new();
    let same: Vec<Discovered> = (0..40_u8).map(|n| found_row(n, "192.0.2.7")).collect();
    repeated.observe(same, 1_000);
    assert_eq!(
        repeated.len(),
        plan::MAX_FOUND_PER_ADDRESS,
        "one claimed address holds {} rows and the cap is {}",
        repeated.len(),
        plan::MAX_FOUND_PER_ADDRESS
    );
}

/// **A row cannot be kept alive by a stale re-announcement, and the TTL is not
/// restarted by the scan that drops it.**
///
/// `a_row_that_stops_announcing_leaves_after_sixty_seconds`
/// (`tests/peer_discovery.rs`) covers the ordinary expiry. The attack here is
/// the boundary: a row observed at the exact TTL instant must be gone (the
/// bound is strict), and a flood arriving in the same scan must not refresh
/// rows it is not for.
#[test]
fn a_found_row_dies_exactly_at_its_ttl_and_a_flood_does_not_revive_it() {
    let mut found = FoundList::new();
    found.observe(vec![found_row(1, "192.0.2.7")], 1_000);
    assert_eq!(found.len(), 1);

    // One millisecond inside the TTL: still there.
    found.observe(Vec::new(), 1_000 + plan::FOUND_TTL_MS - 1);
    assert_eq!(
        found.len(),
        1,
        "a row one millisecond inside its TTL must survive"
    );

    // Exactly at the TTL, and the scan that arrives is somebody else's flood:
    // the old row goes, and the flood does not inherit its freshness.
    let flood: Vec<Discovered> = (0..5_u8)
        .map(|n| found_row(100 + n, &format!("198.51.100.{n}")))
        .collect();
    found.observe(flood, 1_000 + plan::FOUND_TTL_MS);
    assert!(
        !found
            .shown()
            .iter()
            .any(|row| row.instance_id == InstanceId([1; INSTANCE_ID_BYTES])),
        "the row is at exactly its TTL and the bound is strict, so it must be gone: {:?}",
        found.shown()
    );
    assert_eq!(
        found.len(),
        5,
        "and the flood's own five rows are all that is left"
    );
}

// ===========================================================================
// Cap: one refusal log line per address per hour
// (`listener::REFUSAL_LOG_QUIET_MS`, `listener::REFUSAL_LOG_ADDRESSES`)
// ===========================================================================

/// **One address's quiet hour can be beaten, and what it costs is a full
/// thousand fresh addresses.**
///
/// `an_unauthenticated_flood_writes_one_log_line_and_then_counts` and
/// `the_refusal_log_is_bounded_in_addresses` (`tests/peer_noise.rs`) cover the
/// time bound and the map bound separately. This attacks the two TOGETHER,
/// which is where they interact: the map is capped at
/// [`listener::REFUSAL_LOG_ADDRESSES`] and evicts the address whose last line
/// is OLDEST, so a flooder that keeps refusing from address A while filling the
/// map with fresh addresses gets A's entry evicted: and A's next refusal is a
/// brand-new address as far as the log is concerned, so it writes a second line
/// inside the same hour.
///
/// The eviction is deliberate and documented (`listener::RefusalLog::admit`):
/// the alternative is a map a stranger can grow without bound, which is the
/// worse failure. So this test asserts the honest bound rather than the cap's
/// literal words: the amplification an attacker gets is one extra line per
/// [`listener::REFUSAL_LOG_ADDRESSES`] distinct source addresses, and every one
/// of those addresses wrote its own line anyway. Both halves are asserted: that
/// half a mapful is NOT enough (so the cost really is the map size), and that a
/// full mapful is.
#[test]
fn beating_one_addresss_quiet_hour_costs_a_thousand_fresh_addresses() {
    let flooder = "192.0.2.7";

    // Half a mapful changes nothing: the quiet hour holds.
    let mut log = listener::RefusalLog::new();
    assert_eq!(
        log.admit(flooder, 0),
        Some(0),
        "the first refusal is always logged"
    );
    for n in 0..(plan::REFUSAL_LOG_ADDRESSES / 2) {
        let churn = format!("198.51.100.{}.{}", n / 256, n % 256);
        log.admit(&churn, 1);
    }
    assert_eq!(
        log.admit(flooder, 2),
        None,
        "half a mapful of fresh addresses must not evict the flooder's entry, or the cost \
         asserted below is not the real one"
    );

    // A full mapful does evict it, and the flooder writes a second line two
    // milliseconds into its own quiet hour.
    let mut log = listener::RefusalLog::new();
    assert_eq!(log.admit(flooder, 0), Some(0));
    for n in 0..plan::REFUSAL_LOG_ADDRESSES {
        let churn = format!("198.51.100.{}.{}", n / 256, n % 256);
        log.admit(&churn, 1);
    }
    assert!(
        log.admit(flooder, 2).is_some(),
        "measured, and reported as a finding: filling the address map evicts the flooder's \
         own entry and its next refusal reads as a first one. The cost is \
         {} distinct source addresses per extra line, each of which wrote a line of its own",
        plan::REFUSAL_LOG_ADDRESSES
    );
    assert!(
        log.len() <= plan::REFUSAL_LOG_ADDRESSES,
        "and the map is still bounded, which is what the eviction is for: {} entries",
        log.len()
    );
}

// ===========================================================================
// Cap: 8 pending knocks across DISTINCT addresses, and the reservation that
// holds one (`state::MAX_PENDING_KNOCKS`, `state::reserve_knock_slot`,
// `state::release_knock_reservation`)
// ===========================================================================

/// **A flood from a whole `/24` costs eight rows, not two hundred and
/// fifty-four.**
///
/// The sentence `state::MAX_PENDING_KNOCKS`'s own doc-comment makes, asserted
/// for the first time. Every other pending-queue test in this file: and in
/// `tests/peer_pairing.rs`, knocks from `127.0.0.1`, which is the only address
/// a loopback test can dial from, and `state::knock_address` coalesces all of
/// them onto ONE row. So they measure the coalescing and they never once reach
/// the cap: an assertion that the queue holds `<= 8` rows is satisfied by a
/// queue that holds one, and it would be satisfied just as happily by a cap
/// that had stopped working.
///
/// This spends the cap for real, at the seam the accept loop itself decides on
/// ([`state::PeerState::reserve_knock_slot`], which is what `serve_knock` calls
/// before it writes a byte), with 254 distinct addresses from the `192.0.2.0/24`
/// documentation range.
///
/// # Which earlier cap could be answering instead, and the control
///
/// `refusal_for_knock_source` tries BANNED first, then MUTED, then coalescing,
/// and only then the queue: and all four refusals are deliberately
/// indistinguishable on the wire. A test that counted "how many were refused"
/// would be answered identically by a state file that banned the whole range.
/// So this does not count refusals: it matches the refusal ARM, and every
/// outcome that is not `QueueFull` or a plain admission is collected into
/// `unexpected` and named in the failure. A ban or a mute answering instead
/// reds this test with the arm that fired.
#[test]
fn a_flood_from_a_whole_slash_twenty_four_costs_eight_rows_not_two_hundred_and_fifty_four() {
    let hosts = 254_usize;
    let mut value = PeerState::default();
    let now = pair::now_ms();

    let mut admitted = 0_usize;
    let mut queue_full = 0_usize;
    let mut unexpected = Vec::new();
    for host in 1..=hosts {
        let addr = format!("192.0.2.{host}");
        match value.reserve_knock_slot(&addr, now) {
            Ok(true) => admitted += 1,
            Ok(false) => unexpected.push(format!("{addr} coalesced onto a row it never had")),
            Err(state::KnockRefusal::QueueFull { .. }) => queue_full += 1,
            Err(refusal) => unexpected.push(format!("{addr} was refused {refusal:?}")),
        }
    }

    assert!(
        unexpected.is_empty(),
        "every address in the range is fresh, unbanned and unmuted, so the only two \
         outcomes are an admission and QueueFull; these were neither: {unexpected:?}"
    );
    assert_eq!(
        admitted,
        plan::MAX_PENDING_KNOCKS,
        "a /24 flood must buy {} rows and it bought {admitted}",
        plan::MAX_PENDING_KNOCKS
    );
    assert_eq!(
        queue_full,
        hosts - plan::MAX_PENDING_KNOCKS,
        "and the other {} addresses must each be refused QueueFull; {queue_full} were",
        hosts - plan::MAX_PENDING_KNOCKS
    );
    assert_eq!(
        value.knocks_queued(),
        plan::MAX_PENDING_KNOCKS,
        "the queue itself holds {} rows and the cap is {}",
        value.knocks_queued(),
        plan::MAX_PENDING_KNOCKS
    );
}

/// **The ninth address is refused `QueueFull` and says eight are outstanding :
/// and a full queue still lets the eight machines already in it re-knock.**
///
/// The boundary, to the row, plus the lockout the flood above would otherwise
/// buy for free. A queue cap that refused EVERY knock once it was full would
/// pass the flood test and would hand an attacker the real prize: fill the
/// eight slots from a `/24`, and the operator's own Mac, which already holds a
/// row: can never refresh it. `refusal_for_knock_source` returns `None` for an
/// address already pending before it ever looks at the length, and that
/// ordering is what this pins.
///
/// # Which earlier cap could be answering instead, and the control
///
/// Same three arms as above, and the same control: the refusal is matched as
/// `QueueFull { pending: 8 }` (both the arm and its payload), so a ban or a
/// mute cannot satisfy it, and a cap that refused at some other depth reds it
/// with the number it actually reported.
#[test]
fn the_ninth_address_is_refused_queue_full_and_the_eight_already_queued_can_still_reknock() {
    let mut value = PeerState::default();
    let now = pair::now_ms();

    for host in 1..=plan::MAX_PENDING_KNOCKS {
        let addr = format!("192.0.2.{host}");
        assert_eq!(
            value.reserve_knock_slot(&addr, now),
            Ok(true),
            "{addr} is within the first {} and must take a row",
            plan::MAX_PENDING_KNOCKS
        );
    }

    let ninth = format!("192.0.2.{}", plan::MAX_PENDING_KNOCKS + 1);
    assert_eq!(
        value.reserve_knock_slot(&ninth, now),
        Err(state::KnockRefusal::QueueFull {
            pending: plan::MAX_PENDING_KNOCKS
        }),
        "the ninth address must be refused QueueFull with {} outstanding",
        plan::MAX_PENDING_KNOCKS
    );

    // The lockout that would make the flood worth running: an address that
    // already holds a row must not be refused by the cap its own row counts
    // towards.
    assert_eq!(
        value.reserve_knock_slot("192.0.2.1", now + 1),
        Ok(false),
        "a machine already in the queue must coalesce onto its own row, not be locked out \
         by a queue its row is part of"
    );
    assert_eq!(
        value.knocks_queued(),
        plan::MAX_PENDING_KNOCKS,
        "and the re-knock added nothing: {} rows",
        value.knocks_queued()
    );
}

/// **Eight stalled reservations, released, give the whole queue back.**
///
/// The denial of service the reservation itself creates. `reserve_knock_slot`
/// pushes a placeholder row BEFORE the handshake, which is what makes the cap
/// atomic: and it means eight connections that open and then say nothing hold
/// the entire pairing queue. If `release_knock_reservation` did not give the
/// row back, eight TCP connections would shut the node's pairing out
/// permanently for every other machine on the LAN, at a cost of eight sockets.
///
/// `a_valid_message_one_then_silence_holds_no_slot_and_no_queue_row` covers the
/// wire path for ONE address; because loopback coalesces, it can only ever hold
/// one row, so it cannot show the queue being emptied. This holds all eight and
/// shows the ninth address getting in afterwards: which is the observable that
/// says the queue is really back, not merely shorter.
///
/// # Which earlier cap could be answering instead, and the control
///
/// The ninth address's admission at the end is the control: if the releases had
/// done nothing, that call returns `Err(QueueFull)` and the test reds. The
/// intermediate `pending.is_empty()` assertion alone would also be satisfied by
/// a `release` that cleared the queue indiscriminately, so the test that
/// follows this one pins the other direction.
#[test]
fn eight_stalled_reservations_released_give_the_whole_queue_back() {
    let mut value = PeerState::default();
    let now = pair::now_ms();

    let mut held = Vec::new();
    for host in 1..=plan::MAX_PENDING_KNOCKS {
        let addr = format!("192.0.2.{host}");
        assert_eq!(
            value.reserve_knock_slot(&addr, now),
            Ok(true),
            "{addr} takes a placeholder row"
        );
        held.push(addr);
    }
    assert!(
        value.visible_pending().is_empty(),
        "eight placeholders are eight rows the operator must not be shown: {:?}",
        value.visible_pending()
    );
    assert_eq!(
        value.reserve_knock_slot("192.0.2.200", now),
        Err(state::KnockRefusal::QueueFull {
            pending: plan::MAX_PENDING_KNOCKS
        }),
        "positive control: with all eight held, a fresh address really is shut out"
    );

    for addr in &held {
        value.release_knock_reservation(addr, now);
    }
    assert!(
        value.knocks_queued() == 0,
        "every stalled reservation must be released; {:?} remain",
        value
    );
    assert_eq!(
        value.reserve_knock_slot("192.0.2.200", now),
        Ok(true),
        "and the address that was shut out a moment ago now gets in, which is what says the \
         queue is back rather than merely shorter"
    );
}

/// **A stalled connection's release never deletes the row that coalesced onto
/// its placeholder.**
///
/// The attack the release opens, and the nastier half of the pair. An attacker
/// connects from an address, takes the placeholder, and waits. A real machine
/// behind the same NAT (or the attacker's own earlier honest knock), completes
/// its handshake and `record_knock` fills that same row in, because coalescing
/// is keyed on the address. The attacker then drops its socket. If release were
/// keyed on the address alone, the drop would delete the operator's real
/// pairing request, and the attacker could erase every incoming knock from its
/// network for as long as it cared to keep reconnecting.
///
/// `release_knock_reservation` guards on the placeholder being untouched: an
/// all-zero instance id AND `first_seen_ms` still equal to the reserved instant
///: and this is the test that spends both halves of that guard.
///
/// # Which earlier cap could be answering instead, and the control
///
/// Nothing earlier can: no ban, mute or queue-full path removes a row, and the
/// row is confirmed present and VISIBLE (a filled row, not a placeholder)
/// immediately before the release is called, so a green result cannot be
/// explained by the row never having existed.
#[test]
fn a_stalled_connections_release_never_deletes_the_row_that_coalesced_onto_it() {
    let mut value = PeerState::default();
    let reserved_at = pair::now_ms();
    let victim = "192.0.2.42";

    assert_eq!(
        value.reserve_knock_slot(victim, reserved_at),
        Ok(true),
        "the attacker's connection takes the placeholder first"
    );

    // A real handshake from the same address completes and fills the row in.
    let real = instance(3);
    assert_eq!(
        value.record_knock(
            victim,
            real,
            Some("laptop-2".to_string()),
            PROTO_VERSION,
            None,
            reserved_at + 5
        ),
        Ok(false),
        "a completed handshake coalesces onto the placeholder rather than taking a ninth row"
    );
    assert_eq!(
        value.visible_pending().len(),
        1,
        "control: the operator really is being shown a pairing request at this point"
    );

    // The attacker drops its socket, and the listener releases what it
    // reserved.
    value.release_knock_reservation(victim, reserved_at);

    let visible = value.visible_pending();
    assert_eq!(
        visible.len(),
        1,
        "the real row must survive the stalled connection's release; the operator is now \
         shown {visible:?}"
    );
    assert_eq!(
        visible[0].instance_id, real,
        "and it is still the row the real handshake wrote"
    );
}

// ===========================================================================
// Cap: the accepted window runs out (`pair::PAIRING_WINDOW_SECS`)
// ===========================================================================

/// **An accepted window is shut at the instant it expires, and shut before it
/// opens.**
///
/// The window's OTHER dimension. `an_accept_admits_one_instance_id_even_when_\
/// ten_others_race_it` and `a_fresh_instance_id_knocking_after_an_accept_never_\
/// inherits_the_window` both attack which id the window admits, and both ask
/// only about the present instant: so a window with no deadline at all passes
/// each of them. This asks how long, to the millisecond, and it asks the
/// backward-clock question the constructor's own doc-comment claims: an
/// attacker who can move this node's clock must not be able to reopen a window
/// by moving it back before the operator opened it.
///
/// Answering an `XX` message 1 hands over this node's static key, so "the
/// window never actually closes" is a permanent key-harvest offer to one
/// address, bought with a single Accept the operator may have forgotten.
///
/// # Which earlier cap could be answering instead, and the control
///
/// The id check in the same predicate: a window that was shut for the wrong
/// reason (wrong id) would read identically to one that had expired. It is
/// ruled out by asserting the window OPEN for the same address and the same id
/// one millisecond before the deadline: only the clock differs between the
/// `true` and the `false`.
#[test]
fn an_accepted_window_is_shut_at_the_instant_it_expires_and_before_it_opens() {
    let mut value = PeerState::default();
    let opened = pair::now_ms();
    let accepted = instance(8);
    let addr = "192.0.2.77";

    value
        .record_knock(addr, accepted, None, PROTO_VERSION, None, opened)
        .expect("the knock is queued");
    let window = value
        .accept_knock(addr, opened, plan::PAIRING_WINDOW_SECS)
        .expect("a pending row to accept");
    assert_eq!(
        window.until_ms,
        opened + plan::PAIRING_WINDOW_SECS * 1_000,
        "the window the operator opened must run exactly {} seconds",
        plan::PAIRING_WINDOW_SECS
    );

    assert!(
        value.accepted_window(addr, &accepted, window.until_ms - 1),
        "control: one millisecond before the deadline the window is open, so every `false` \
         below differs from this call only by its clock"
    );
    assert!(
        !value.accepted_window(addr, &accepted, window.until_ms),
        "the deadline is exclusive: at {} the window must be shut",
        window.until_ms
    );
    assert!(
        !value.accepted_window(addr, &accepted, window.until_ms + 60_000),
        "and a minute past it, shut"
    );
    assert!(
        !value.accepted_window(addr, &accepted, opened - 1),
        "a clock set back to before the operator accepted must CLOSE the window, not reopen \
         it: an attacker who can move this node's clock must not be able to buy a second \
         window with it"
    );
}

/// **An `XX` inside a window that has run out gets zero bytes back.**
///
/// The end-to-end half: the predicate above is only a defence if the accept
/// loop consults it with a real clock on every connection. The expired window
/// is written straight into the state file rather than waited for, so the test
/// costs no time and still exercises the shipped path.
///
/// # Which earlier cap could be answering instead, and the control
///
/// Every one of them: the knock bucket, the per-address socket cap, the queue
///: refuses with the same zero bytes, and this test's whole assertion is zero
/// bytes. So the control is the second half: the SAME address, the SAME
/// instance id and the SAME `XX` bytes against a window that is still open must
/// come back with bytes, over a connection opened immediately after the first
/// one closed. The two calls differ in nothing but the window's deadline, so a
/// cap that had refused everything reds the control.
///
/// # What this test does NOT do on its own, measured
///
/// **Two independent mechanisms refuse an expired window, so this test reds
/// only when BOTH are broken, and it must not be read as a gate on either
/// one.** [`state::PeerState::expire`] runs inside every [`state::load`] and
/// drops the row before the connection is even looked at; if it did not,
/// [`state::PeerState::accepted_window`]'s `now_ms < until_ms` clause refuses
/// the same `XX` a moment later. Measured, by deleting each in turn and
/// re-running: with the `accepted_window` clause deleted this test stayed
/// GREEN, and with `expire`'s deadline deleted it stayed GREEN. Only the pair
/// reds it.
///
/// That is good layering and a weak assertion, so each mechanism is given its
/// own red elsewhere:
/// [`an_accepted_window_is_shut_at_the_instant_it_expires_and_before_it_opens`]
/// reds on the predicate, and
/// [`reading_the_state_file_drops_a_window_that_has_run_out`] reds on the load.
/// What this test adds over both is the only thing neither can show: that the
/// SHIPPED accept loop consults one of them with a real clock before it writes
/// a byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_xx_inside_a_window_that_has_run_out_gets_zero_bytes() {
    let node = Node::new("window-expiry-wire");
    let addr = serve(node.context()).await;
    let accepted = instance(9);
    let now = pair::now_ms();

    let expired = state::AcceptedInstance {
        instance_id: accepted,
        addr: LOOPBACK.to_string(),
        until_ms: now - 1_000,
        opened_at_ms: now - plan::PAIRING_WINDOW_SECS * 1_000 - 1_000,
        learned_key: None,
    };
    let mut value = PeerState::default();
    value.open_accepted_window(expired);
    node.write_state(&value);

    let written = count_written(addr, &xx_message_1(accepted)).await;
    assert_eq!(
        written, 0,
        "a window that ran out a second ago must answer an XX with nothing; it wrote \
         {written} bytes, and the first of them is this node's static key on its way to a \
         stranger"
    );

    // Control: same address, same id, same bytes, a window that is still open.
    let live = state::AcceptedInstance {
        instance_id: accepted,
        addr: LOOPBACK.to_string(),
        until_ms: now + plan::PAIRING_WINDOW_SECS * 1_000,
        opened_at_ms: now,
        learned_key: None,
    };
    let mut open = PeerState::default();
    open.open_accepted_window(live);
    node.write_state(&open);

    let still = count_written(addr, &xx_message_1(accepted)).await;
    assert!(
        still > 0,
        "positive control: with the window open the same XX must be answered, or the zero \
         above says nothing about the deadline; it got {still} bytes"
    );
}

/// **Reading the state file drops a window that has run out.**
///
/// The second of the two independent mechanisms that refuse an expired window,
/// given its own red. [`state::PeerState::expire`] runs on every
/// [`state::load`], so a window past its deadline is gone before
/// [`state::PeerState::accepted_window`] is ever consulted: which is why
/// [`an_xx_inside_a_window_that_has_run_out_gets_zero_bytes`] cannot tell the
/// two apart, and why this test exists beside it.
///
/// # Which earlier cap could be answering instead, and the control
///
/// A `load` that returned an empty state for any reason: a parse failure
/// swallowed into a default, a path that never existed, would satisfy "no
/// accepted rows" without expiring anything. The live window written in the
/// second half is the control: the same file, the same writer, the same reader,
/// and it survives. Only the deadline differs.
#[test]
fn reading_the_state_file_drops_a_window_that_has_run_out() {
    let node = Node::new("window-expire-load");
    let accepted = instance(10);
    let now = pair::now_ms();

    let mut value = PeerState::default();
    value.open_accepted_window(state::AcceptedInstance {
        instance_id: accepted,
        addr: LOOPBACK.to_string(),
        until_ms: now - 1_000,
        opened_at_ms: now - plan::PAIRING_WINDOW_SECS * 1_000 - 1_000,
        learned_key: None,
    });
    node.write_state(&value);
    assert!(
        !node.state().holds_accepted_row(LOOPBACK),
        "a window a second past its deadline must not survive the read that every connection \
         makes; it came back as {:?}",
        node.state()
    );

    // Control: the same file and the same reader keep a window that is open.
    let mut open = PeerState::default();
    open.open_accepted_window(state::AcceptedInstance {
        instance_id: accepted,
        addr: LOOPBACK.to_string(),
        until_ms: now + plan::PAIRING_WINDOW_SECS * 1_000,
        opened_at_ms: now,
        learned_key: None,
    });
    node.write_state(&open);
    assert!(
        node.state().holds_accepted_row(LOOPBACK),
        "positive control: an open window must survive the same read, or the assertion above \
         is about the reader and not about the deadline"
    );
}
