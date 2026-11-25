// ---------------------------------------------------------------------------
// A lease has a SCOPE, and it never leaves the lender
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// **`--scope` round-trips through one parser**, and every label goes through
/// the shared sanitizer on the way in.
///
/// One parser for the CLI's input, its output and anything that reads a scope
/// back out of a file as text: two spellings of what `group:work` means is how
/// a lease comes to draw from somewhere the operator did not name.
///
/// The refusals are the interesting half. A bare `2` is not a scope, an
/// `account:` with nothing after it names no account, and a label that is an
/// email or a uuid is refused by `sanitize_label`: the same refusal the rest
/// of the peer surface gives, because a scope is written to a file and printed
/// by a CLI in a PUBLIC repository.
///
/// Watch it fail by making `LendScope::parse` accept a bare word as a group
/// name: `"2"` then parses and the first refusal below goes green.
#[test]
fn a_lend_scope_round_trips_and_refuses_what_is_not_a_scope() {
    use tcr_peer_wire::{LendScope, LendScopeRefusal};

    for (spec, expected) in [
        ("all", LendScope::All),
        ("ALL", LendScope::All),
        ("group:work", LendScope::Group("work".to_string())),
        (
            "account:studio-mac",
            LendScope::Accounts(vec!["studio-mac".to_string()]),
        ),
        (
            "accounts:studio-mac,attic-nuc",
            LendScope::Accounts(vec!["studio-mac".to_string(), "attic-nuc".to_string()]),
        ),
    ] {
        let parsed = LendScope::parse(spec).expect(spec);
        assert_eq!(parsed, expected, "{spec} parses to the scope it names");
        // And the spelling it prints parses back to the same scope, so a
        // printed scope can be pasted into the flag it came from.
        assert_eq!(
            LendScope::parse(&parsed.to_spec()).expect("the printed form parses back"),
            expected,
            "{spec} does not survive a round trip through {}",
            parsed.to_spec()
        );
    }

    assert!(matches!(
        LendScope::parse("2"),
        Err(LendScopeRefusal::Shape { .. })
    ));
    assert!(matches!(
        LendScope::parse("work"),
        Err(LendScopeRefusal::Shape { .. })
    ));
    assert_eq!(
        LendScope::parse("account:,,"),
        Err(LendScopeRefusal::NoAccounts)
    );
    // An email and a uuid are both refused, by the sanitizer rather than by a
    // second list here.
    assert!(matches!(
        LendScope::parse("account:alice@example.com"),
        Err(LendScopeRefusal::Label(_))
    ));
    assert!(matches!(
        LendScope::parse("group:11111111-1111-1111-1111-111111111111"),
        Err(LendScopeRefusal::Label(_))
    ));

    // The default is every account, which is what a lease meant before scopes
    // existed: so a grant written by an older build keeps behaving as it did.
    assert_eq!(LendScope::default(), LendScope::All);
}

/// **One answer to "is this account inside that scope"**, read by the headroom
/// arithmetic, by the picker restriction and by the `lentTo` line.
///
/// Three readers and one function on purpose: a scope that meant one set of
/// accounts when the headroom was computed and another when the request was
/// served would advertise one account's room and spend another's.
///
/// Watch it fail by making the `Group` arm compare against the label instead of
/// the group list: the second assertion then goes green for the wrong reason
/// and the third goes red.
#[test]
fn a_scope_covers_exactly_the_accounts_it_names() {
    use tcr_peer_wire::LendScope;

    let groups = vec!["work".to_string(), "spare".to_string()];
    assert!(lease::scope_covers(&LendScope::All, "studio-mac", &groups));
    assert!(lease::scope_covers(
        &LendScope::Group("work".to_string()),
        "studio-mac",
        &groups
    ));
    assert!(!lease::scope_covers(
        &LendScope::Group("studio-mac".to_string()),
        "studio-mac",
        &groups
    ));
    assert!(lease::scope_covers(
        &LendScope::Accounts(vec!["attic-nuc".to_string(), "studio-mac".to_string()]),
        "studio-mac",
        &groups
    ));
    assert!(!lease::scope_covers(
        &LendScope::Accounts(vec!["attic-nuc".to_string()]),
        "studio-mac",
        &groups
    ));
    // An account in NO group is covered by `All` and by its own name, and by
    // nothing else: which is what stops an ungrouped account leaking into a
    // group-scoped lease.
    assert!(lease::scope_covers(&LendScope::All, "loner", &[]));
    assert!(!lease::scope_covers(
        &LendScope::Group("work".to_string()),
        "loner",
        &[]
    ));
}

// ---------------------------------------------------------------------------
// A lease can be bounded in time
// ---------------------------------------------------------------------------

/// **A lease whose `until` has passed is refused**, with budget left on it and
/// its renewal deadline still ahead.
///
/// Both halves in one test, because the second is the control that stops the
/// first from passing for the wrong reason: the same lease with the end one
/// hour AHEAD is allowed, so the refusal is about `until` and not about an
/// expired ttl, a spent budget or an unmeasured window.
///
/// The word is `lease-expired` and not an `ended` of its own: adding a
/// `LeaseRefusal` variant is outside this test's scope on the wire crate, and
/// `LeaseExpired`'s own doc is "the lease's absolute deadline has passed",
/// which `until` is. The borrower's action is identical either way: stop
/// spending this lease.
///
/// Watch it fail by deleting the `lease.until` check from `Ledger::may_relay`.
#[test]
fn a_lease_whose_end_has_passed_is_refused_with_budget_left() {
    let now_ms = 1_700_000_000_000_i64;
    let now_s = 1_700_000_000_u64;

    let ended = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        granted_at_ms: now_ms - 60_000,
        // The RENEWAL deadline is still ahead: this lease is not expired.
        expires_at_ms: now_ms + 300_000,
        // And it is not spent.
        spent: 0.0,
        max_inflight: 2,
        until: Some(now_s - 1),
    };
    let mut ledger = Ledger::new();
    ledger.record(ended);
    ledger.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(
        ledger.may_relay(LEASE, now_ms),
        Err(LeaseRefusal::LeaseExpired),
        "at `until` the lender stops renewing, whatever is left on the budget"
    );

    // The control: the same lease, one hour of lending left.
    let mut running = Ledger::new();
    running.record(Lease {
        until: Some(now_s + 3_600),
        ..ended
    });
    running.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(
        running.may_relay(LEASE, now_ms),
        Ok(()),
        "a lease still inside its end serves, so the refusal above is about the end"
    );

    // And a lease with NO end serves, which is the default an operator gets by
    // not asking for one.
    let mut endless = Ledger::new();
    endless.record(Lease {
        until: None,
        ..ended
    });
    endless.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(endless.may_relay(LEASE, now_ms), Ok(()));
}

/// **A grant whose end has already passed mints nothing**, and one still ahead
/// puts its end on the lease.
///
/// Refusing to mint is the same answer `may_relay` would give on the first
/// relay, told one round trip earlier: the same reasoning `clamp_to_grant`
/// already applies to a window with no measurement.
///
/// Watch it fail by deleting the `end.is_some_and(..)` check from
/// `clamp_to_grant`: the first assertion then gets a lease.
#[test]
fn a_grant_past_its_end_mints_nothing_and_one_ahead_carries_it() {
    let now_ms = 1_700_000_000_000_i64;
    let now_s = 1_700_000_000_u64;
    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };
    let granted = LendGrant::new(Window::SevenDay, 0.20, 300, 2);

    let past = lease::clamp_to_grant(
        &ask,
        Some(granted.clone()),
        true,
        0.40,
        now_ms,
        LEASE,
        Some(now_s - 1),
        None,
    );
    assert_eq!(past.lease, None);
    assert_eq!(past.refusal, Some(LeaseRefusal::LeaseExpired));

    let ahead = lease::clamp_to_grant(
        &ask,
        Some(granted.clone()),
        true,
        0.40,
        now_ms,
        LEASE,
        Some(now_s + 7_200),
        None,
    );
    assert_eq!(
        ahead
            .lease
            .expect("a lease inside its end is granted")
            .until,
        Some(now_s + 7_200),
        "the lease carries the lender's end, which is the one time field a borrower may know"
    );

    // No end asked for, no end on the lease.
    let endless = lease::clamp_to_grant(&ask, Some(granted), true, 0.40, now_ms, LEASE, None, None);
    assert_eq!(
        endless.lease.expect("a lease with no end is granted").until,
        None
    );
}

/// **`--for 2h` writes an end two hours ahead, `--until 18:00` writes today's
/// 18:00 in the operator's own offset, and a clock that has already passed is
/// REFUSED.**
///
/// The clock is injected, which is what makes this a measurement rather than an
/// approximation: a function reading the wall clock could only be asserted
/// about to within the time the assertion took.
///
/// # Why the injected instant is built rather than written down
///
/// A bare `18:00` resolves in the operator's own offset, so a fixed unix
/// timestamp would assert a different wall clock on every machine: this file
/// used to inject `1_700_000_000` and read `23:00` where the resolution is UTC
/// and `21:00` where it is `+02:00`. The instant here is built AT noon in
/// whatever offset the resolution uses, so `18:00` is always still ahead and
/// `09:00` always already gone, in every zone, and the hour is asserted in that
/// same offset.
///
/// # The refusal is the behaviour, not an edge
///
/// A clock that has gone used to roll to tomorrow. It does not any more, and
/// that is the fix rather than a regression: the panel's own refusal and this
/// function disagreed about which day a bare `18:00` meant, so a lease edited
/// at 18:00 for "tomorrow at 18:00" round-tripped as an end later the same
/// minute. Refusing names the instant it resolved to and leaves `--for 24h` as
/// the way to say tomorrow.
///
/// A bare number is REFUSED rather than read as seconds or as an hour: `2` is
/// two hours to one reader and two seconds to another, and a lease is not
/// something to guess a unit on.
///
/// Watch it fail by making the duration arm default to seconds for a bare
/// number: `"2"` then parses and the last assertion goes green; or by rolling a
/// passed clock to tomorrow again, which turns the refusal below into an end
/// this test then reads as `Some`.
#[test]
fn a_lend_end_is_parsed_from_a_duration_or_a_clock_against_an_injected_now() {
    // The offset the resolution will use, asked for exactly as `parse_lend_end`
    // asks for it, including the fallback: the lookup refuses in a
    // multithreaded process and both callers then mean UTC, so the two agree
    // whichever answer the host gives.
    //
    // Asked AT the injected date and not at today's: a zone with summer time is
    // a different offset in November than in September, and building a November
    // noon with September's offset puts the resolution and the read-back an
    // hour apart, which is exactly what the first version of this test did.
    let date = time::Date::from_calendar_date(2023, time::Month::November, 14)
        .expect("a literal calendar date");
    let noon = time::Time::from_hms(12, 0, 0).expect("a literal time of day");
    let probe = time::OffsetDateTime::new_in_offset(date, noon, time::UtcOffset::UTC);
    let offset = time::UtcOffset::local_offset_at(probe).unwrap_or(time::UtcOffset::UTC);
    // Noon on that date, IN that offset. Noon is the whole trick: 18:00 is
    // ahead of it and 09:00 behind it in every zone, so neither assertion below
    // depends on where this machine is.
    let now = time::OffsetDateTime::new_in_offset(date, noon, offset);
    let now_unix = now.unix_timestamp();

    // A duration is offset-free: it is arithmetic on the instant, and the same
    // number of seconds wherever the machine is.
    let ahead = |seconds: i64| Some(u64::try_from(now_unix + seconds).expect("in range"));
    assert_eq!(
        lease::parse_lend_end("2h", now).expect("2h parses"),
        ahead(7_200)
    );
    assert_eq!(
        lease::parse_lend_end("90m", now).expect("90m parses"),
        ahead(5_400)
    );
    assert_eq!(
        lease::parse_lend_end("30s", now).expect("30s parses"),
        ahead(30)
    );
    assert_eq!(
        lease::parse_lend_end("3d", now).expect("3d parses"),
        ahead(3 * 86_400)
    );

    // No end, named rather than implied, so an operator can clear an end with
    // the same flag they set it with.
    assert_eq!(
        lease::parse_lend_end("none", now).expect("none parses"),
        None
    );
    assert_eq!(lease::parse_lend_end("", now).expect("empty parses"), None);

    // A clock time still AHEAD: 18:00 against a noon now. Read back in the same
    // offset it was resolved in, which is what makes 18:00 mean 18:00 rather
    // than whatever 18:00 elsewhere looks like from here.
    let today = lease::parse_lend_end("18:00", now)
        .expect("18:00 parses")
        .expect("a clock time still ahead is an end");
    let today = time::OffsetDateTime::from_unix_timestamp(i64::try_from(today).expect("in range"))
        .expect("a valid instant")
        .to_offset(offset);
    assert_eq!(
        (today.hour(), today.minute()),
        (18, 0),
        "the end is the operator's own 18:00, not 18:00 somewhere else"
    );
    assert_eq!(today.date(), now.date(), "still ahead today is today");

    // And one that has already GONE is refused, naming what it resolved to,
    // rather than rolled into tomorrow where nobody asked for it.
    let refusal = lease::parse_lend_end("09:00", now)
        .expect_err("09:00 has passed at noon, so it is not an end this build will write");
    let refusal = format!("{refusal}");
    assert!(
        refusal.contains("already passed today"),
        "the refusal says the clock has gone, or an operator reads it as a parse error: \
         {refusal}"
    );
    assert!(
        refusal.contains("09:00"),
        "and names the time it was given, so the operator can see what was read: {refusal}"
    );

    // Refusals: a bare number, a unit this build does not know, a duration
    // that is not into the future.
    for refused in ["2", "2w", "0h", "-1h", "25:00", "18:60", "18:00:00:00"] {
        assert!(
            lease::parse_lend_end(refused, now).is_err(),
            "{refused:?} is not an end and must be refused rather than guessed at"
        );
    }
}

/// **`lentTo` never names a Codex account, under any scope**: no lease draws on
/// one, so a Codex card must not read "Lent to".
///
/// Three Macs hold one grant each: `all`, `group:work` and
/// `account:codex-fake`. Every one of them would reach `codex-fake` by scope
/// alone, which is what makes its absence the assertion that means something.
/// The control flips the same account to a Claude one, and it then appears
/// under all three.
///
/// Watch it fail by dropping the provider skip in `lent_to`: `codex-fake` then
/// has a line under every scope.
#[test]
fn lent_to_skips_codex_accounts_under_every_scope() {
    let dir = tempfile::tempdir().expect("a temp home");
    let peers = dir.path().join("tcr-peers.json");

    let scoped = |scope: tcr_peer_wire::LendScope| {
        let mut grant = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
        grant.scope = scope;
        grant
    };
    let mut all = lender_row_for(
        PeerId([41_u8; 32]),
        vec![scoped(tcr_peer_wire::LendScope::All)],
    );
    all.label = "all-mac".to_string();
    let mut group = lender_row_for(
        PeerId([42_u8; 32]),
        vec![scoped(tcr_peer_wire::LendScope::Group("work".to_string()))],
    );
    group.label = "group-mac".to_string();
    let mut named = lender_row_for(
        PeerId([43_u8; 32]),
        vec![scoped(tcr_peer_wire::LendScope::Accounts(vec![
            "codex-fake".to_string(),
        ]))],
    );
    named.label = "named-mac".to_string();
    write_peers(&peers, vec![all, group, named]);
    let store = PeerStore::open(&peers).expect("the peers file");

    let lender = (
        "lender-fake".to_string(),
        vec!["work".to_string()],
        Provider::Anthropic,
    );
    let lent = lease::lent_to(
        &store,
        &[
            lender.clone(),
            (
                "codex-fake".to_string(),
                vec!["work".to_string()],
                Provider::Codex,
            ),
        ],
    );
    let on_lender: Vec<&str> = lent
        .get("lender-fake")
        .expect("the Claude account is lent")
        .iter()
        .map(|row| row.peer.as_str())
        .collect();
    assert_eq!(
        on_lender,
        ["all-mac", "group-mac"],
        "the `all` and `group:work` grants reach the Claude account: {lent:?}"
    );
    assert!(
        !lent.contains_key("codex-fake"),
        "no grant reaches a Codex account, whatever its scope says: {lent:?}"
    );

    // The control: the same account, as a Claude account, is under all three.
    let lent = lease::lent_to(
        &store,
        &[
            lender,
            (
                "codex-fake".to_string(),
                vec!["work".to_string()],
                Provider::Anthropic,
            ),
        ],
    );
    let on_flipped: Vec<&str> = lent
        .get("codex-fake")
        .expect("as a Claude account it is lent")
        .iter()
        .map(|row| row.peer.as_str())
        .collect();
    assert_eq!(on_flipped, ["all-mac", "group-mac", "named-mac"]);
}

/// **`lentTo`, per account label, from the lender's own grants and no wire** :
/// the read-only, time-bounded view of a lease from the account's side.
///
/// Two Macs with a grant on one account is the shape the panel draws as "Lent
/// to attic-nuc 20 % · studio-mac 20 %", and it is the one this asserts. An
/// account no grant reaches is ABSENT from the map rather than present with an
/// empty list, because the panel hides the line when it is empty and "no key"
/// is one fact for it to read rather than two.
///
/// Nothing is asked of a peer: this reads the lender's own file. A figure that
/// needed a reachable peer would render blank on a sleeping laptop, which is
/// every laptop most of the time.
///
/// Watch it fail by making `lent_to` skip the scope check (`continue` on every
/// account): the ungrouped account then appears too.
#[test]
fn lent_to_names_every_mac_a_grant_lends_one_account_to() {
    let dir = tempfile::tempdir().expect("a temp home");
    let peers = dir.path().join("tcr-peers.json");

    let grant = |fraction: f64| LendGrant::new(Window::SevenDay, fraction, 300, 2);
    // Two trusted Macs, each holding a grant on this node.
    let mut attic = lender_row_for(PeerId([11_u8; 32]), vec![grant(0.20)]);
    attic.label = "attic-nuc".to_string();
    let mut studio = lender_row_for(PeerId([12_u8; 32]), vec![grant(0.35)]);
    studio.label = "studio-mac".to_string();
    // And one pinned Mac that was never lent anything.
    let mut idle = lender_row_for(PeerId([13_u8; 32]), Vec::new());
    idle.label = "idle-mac".to_string();
    write_peers(&peers, vec![attic, studio, idle]);

    let store = PeerStore::open(&peers).expect("the peers file");
    let accounts = vec![
        (
            "lender-fake".to_string(),
            vec!["work".to_string()],
            Provider::Anthropic,
        ),
        ("spare-fake".to_string(), Vec::new(), Provider::Anthropic),
    ];
    let lent = lease::lent_to(&store, &accounts);

    let on_first = lent
        .get("lender-fake")
        .expect("the account every grant covers has a line");
    let peers_named: Vec<&str> = on_first.iter().map(|row| row.peer.as_str()).collect();
    assert_eq!(
        peers_named,
        ["attic-nuc", "studio-mac"],
        "both Macs holding a grant appear on the account's line: {on_first:?}"
    );
    assert_eq!(on_first[0].window, Window::SevenDay);
    assert!((on_first[0].fraction - 0.20).abs() < f64::EPSILON);
    assert!((on_first[1].fraction - 0.35).abs() < f64::EPSILON);
    assert_eq!(
        on_first[0].scope,
        tcr_peer_wire::LendScope::All,
        "every grant is `All` until `LendGrant` carries a scope"
    );
    assert_eq!(on_first[0].until, None);

    // The JSON the panel actually reads: camelCase, and `until` absent rather
    // than null when the operator set no end.
    let json = serde_json::to_string(&lent).expect("the lentTo map serializes");
    assert!(json.contains("\"peer\":\"attic-nuc\""), "{json}");
    assert!(json.contains("\"fraction\":0.2"), "{json}");
    assert!(
        !json.contains("until"),
        "an absent end is an absent key, never a null: {json}"
    );

    // A pinned Mac with no grant is on nobody's line, and a `Group`-scoped
    // grant reaching an ungrouped account would be the other failure: both
    // are absences, so the positive control above is what makes them mean
    // something.
    assert!(
        !lent.values().flatten().any(|row| row.peer == "idle-mac"),
        "a pinned Mac that was lent nothing is on no account's line: {lent:?}"
    );
}

// ---------------------------------------------------------------------------
// Lease scope, time-bounded lending,
// and the storage both of them need.
// ---------------------------------------------------------------------------

/// **A peers file written before scope existed (one grant OBJECT, not a list)
/// reads as a one-element list**, with today's defaults for everything it
/// could not carry.
///
/// An operator's grant is not something to lose to a shape change, and the
/// alternative is worse than losing it: a parse error on the `lend` key takes
/// the whole peers file down, which means the pinned rows, the allows and the
/// network key go with it.
///
/// Watch it fail by putting `#[serde(default)]` back on `PeerRow::lend` in
/// place of `deserialize_with = "one_or_many_grants"`: the read then fails with
/// "invalid type: map, expected a sequence".
#[test]
fn an_old_single_grant_object_reads_as_a_one_element_list() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let node = PeerId([31_u8; 32]).to_wire();
    write_raw_peers(
        &peers,
        &format!(
            r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "attic-nuc",
              "addedAt": 1,
              "allow": {{ "inspect": true }},
              "lend": {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2 }}
            }}
          ]
        }}"#
        ),
    );

    let file =
        tcr::peer::config::read_or_default(&peers).expect("the pre-decision-12 shape still reads");
    let row = file.row(&PeerId([31_u8; 32])).expect("the one pinned row");
    assert_eq!(row.lend.len(), 1, "one grant object is one lease: {row:?}");
    let grant = &row.lend[0];
    assert_eq!(grant.window, Window::SevenDay);
    assert!((grant.fraction - 0.20).abs() < f64::EPSILON);
    assert_eq!(
        grant.scope,
        tcr_peer_wire::LendScope::All,
        "a grant written before scopes existed meant every account, and still does"
    );
    assert_eq!(grant.until, None, "and it had no end");
    assert!(!grant.ended);
    assert_eq!(grant.id, 0, "no id until the file is next written");

    // The positive control: the modern LIST shape reads too, so the reader
    // above is not simply accepting everything as one grant.
    assert_the_list_shape_reads(dir.path(), &node);
}

/// **`ended` is derived against the clock and NEVER read off the file.**
///
/// Two grants in one hand-written file: one whose end has passed and which the
/// file claims is still running, and one with no end at all which the file
/// claims has ended. Both claims are ignored.
///
/// The direction that matters is the second: a `"ended": true` a text editor
/// could plant on a live grant would switch lending off without any of the
/// verbs that are supposed to do that, and nothing would say why.
///
/// Two layers, and the test names both. The DERIVATION (`read_or_default`,
/// `PeerStore::peers`, `PeerStore::row`) is what every caller sees; the
/// `skip_deserializing` on the field is the layer under it, for a reader
/// written later that forgets to derive: so it is asserted through a raw
/// `serde_json` read, the one path in this program that does not derive.
///
/// Watch it fail two ways: delete the derivation loop in `read_or_default` (the
/// first assertion goes green-for-the-wrong-reason and the store assertions
/// fail), or remove `skip_deserializing` from `LendGrant::ended` (the raw-read
/// assertion at the bottom fails).
#[test]
fn an_ended_flag_in_the_file_is_ignored_and_the_clock_decides() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let node = PeerId([31_u8; 32]).to_wire();
    write_raw_peers(
        &peers,
        &format!(
            r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "attic-nuc",
              "addedAt": 1,
              "lend": [
                {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2,
                  "until": 1, "ended": false }},
                {{ "window": "5h", "fraction": 0.1, "ttlS": 300, "maxInflight": 1,
                  "ended": true }}
              ]
            }}
          ]
        }}"#
        ),
    );

    let file = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    let lend = &file
        .row(&PeerId([31_u8; 32]))
        .expect("the one pinned row")
        .lend;
    assert!(
        lend[0].ended,
        "unix second 1 is long past, so this lease has ended whatever the file says"
    );
    assert!(
        !lend[1].ended,
        "a grant with no end has not ended, and a hand-planted `ended: true` does not \
         make it so"
    );

    // And through the STORE, which is what the serving path reads.
    let store = PeerStore::open(&peers).expect("the store opens");
    let row = store.peers().remove(0);
    assert!(row.lend[0].ended);
    assert!(!row.lend[1].ended);
    // The one reader the picker uses skips the ended grant and finds the live
    // one on its own window.
    let now_s = u64::try_from(tcr::now_ms() / 1_000).expect("a positive clock");
    // Every scope in this fixture is `All`, which every fleet can hold, so the
    // enforceability predicate answers `true` and the ENDED grant is the only
    // thing being skipped here.
    let any_scope = |_: &tcr_peer_wire::LendScope| true;
    assert!(row.grant_for(Window::SevenDay, now_s, &any_scope).is_none());
    assert!(row.grant_for(Window::FiveHour, now_s, &any_scope).is_some());

    // The layer UNDER the derivation: a raw deserialize, which no production
    // caller does, still cannot pick up the file's `"ended": true`.
    let raw: PeerFile = serde_json::from_str(&std::fs::read_to_string(&peers).expect("the bytes"))
        .expect("the fixture deserializes");
    assert!(
        !raw.row(&PeerId([31_u8; 32]))
            .expect("the one pinned row")
            .lend[1]
            .ended,
        "`ended` is derived, so a file claiming true cannot deserialize into true"
    );
}

/// **`Ledger::grant` carries the operator's `until` onto the lease, refuses a
/// grant whose end has passed, and `may_relay` refuses it afterwards** :
/// time-bounded lending, end to end, against a real peers file.
///
/// `clamp_to_grant`'s own gates already cover the arithmetic with an injected
/// end. What this adds is the wiring those gates cannot see: that the end
/// travels from the FILE to the lease, which is the half that was passing
/// `None` before this fix.
///
/// Watch it fail by passing `None` for `end` in `Ledger::grant` again: the
/// first assertion then gets a lease with no end and the third gets one at
/// all.
#[test]
fn a_grant_with_an_end_puts_it_on_the_lease_and_a_passed_end_mints_nothing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([21_u8; 32]);
    let now_s = u64::try_from(tcr::now_ms() / 1_000).expect("a positive clock");

    let mut ahead = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    ahead.until = Some(now_s + 7_200);
    let store = lender_store(&peers, borrower, vec![ahead]);

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    let lease = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("a grant inside its end mints a lease");
    assert_eq!(
        lease.until,
        Some(now_s + 7_200),
        "the end came off the operator's own grant, not from a parameter nobody sets"
    );

    // Past the end, that same lease is refused: and it is refused for the
    // END and not for the ttl, which is still 300 seconds away.
    assert_eq!(
        ledger.may_relay(
            lease.lease_id,
            (i64::try_from(now_s).expect("in range") + 7_201) * 1_000
        ),
        Err(LeaseRefusal::LeaseExpired),
        "at `until` the lender stops renewing"
    );

    // A grant whose end has ALREADY passed mints nothing at all. Same file,
    // same ledger, one field different.
    let mut passed = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    passed.until = Some(now_s - 1);
    write_peers(&peers, vec![lender_row_for(borrower, vec![passed])]);
    let store = PeerStore::open(&peers).expect("the peers file re-opens");
    let refused = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer;
    assert_eq!(refused.lease, None);
    assert_eq!(
        refused.refusal,
        Some(LeaseRefusal::InspectNotGranted),
        "an ended grant is not a grant for this window at all: `grant_for` skips it, so the \
         answer is the same one an unpinned peer gets rather than a lease nobody can spend"
    );
}

/// Decision row 14, the half that has to be enforced rather than stored:
/// `Ledger::grant` refuses a lease asked for OUTSIDE the grant's window, and
/// mints the same lease inside it.
///
/// # Why the window is built around now instead of being written down
///
/// The refusal reads the wall clock, `Schedule::contains` asks what time it is
/// on this Mac, because that is the question an operator means by "between
/// 22:00 and 08:00". A test cannot hand `Ledger::grant` a pretend instant, so
/// it does the opposite: it builds one window that is open at whatever time the
/// suite happens to run and one that is not, three hours away on either side,
/// and the assertion is the same at any hour and in any timezone. The literal
/// 21:59-vs-22:01 boundary the decision names is pinned where an instant CAN be
/// handed over, against `schedule_refusal` itself, in `tests/peer/peer_props.rs`.
///
/// Watch it fail by deleting the `schedule_refusal` call from `Ledger::grant`:
/// the closed window then mints a lease, which is the defect this exists to
/// catch: the field was stored, printed, and enforced nowhere.
#[test]
fn a_grant_outside_its_window_is_refused_and_inside_it_mints() {
    use tcr::peer::schedule::Between;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([23_u8; 32]);

    // This Mac's own wall clock, read the way the refusal reads it.
    let now_utc = time::OffsetDateTime::now_utc();
    let offset = time::UtcOffset::local_offset_at(now_utc).unwrap_or(time::UtcOffset::UTC);
    let local = now_utc.to_offset(offset);
    let hour =
        |shift: i8| -> u8 { ((i16::from(local.hour()) + i16::from(shift)).rem_euclid(24)) as u8 };
    let open = format!(
        "{:02}:{:02}-{:02}:{:02}",
        hour(-3),
        local.minute(),
        hour(3),
        local.minute()
    );
    let shut = format!(
        "{:02}:{:02}-{:02}:{:02}",
        hour(3),
        local.minute(),
        hour(6),
        local.minute()
    );

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);

    let mut inside = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    inside.between = Some(open.parse::<Between>().expect("the open window parses"));
    let store = lender_store(&peers, borrower, vec![inside]);
    assert!(
        ledger
            .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
            .answer
            .lease
            .is_some(),
        "a grant whose window is open right now ({open}) must still mint"
    );

    let mut outside = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    outside.between = Some(shut.parse::<Between>().expect("the shut window parses"));
    write_peers(&peers, vec![lender_row_for(borrower, vec![outside])]);
    let store = PeerStore::open(&peers).expect("the peers file re-opens");
    let refused = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer;
    assert_eq!(refused.lease, None, "a window that is shut mints nothing");
    assert_eq!(
        refused.refusal,
        Some(LeaseRefusal::OutsideSchedule),
        "and it says WHICH refusal: a borrower told `InspectNotGranted` would go and ask \
         its operator for a grant it already has"
    );
}

/// A2-3. `--between`/`--days` is consulted at MINT ONLY: `Ledger::grant`
/// checks `schedule_refusal` once, and nothing after it ever asks the
/// schedule again. So a lease minted moments before its window closes used to
/// keep serving for its whole TTL, hours past the hours it was lent for.
///
/// Same clock trick as
/// [`a_grant_outside_its_window_is_refused_and_inside_it_mints`]: the window
/// is built off this Mac's real wall clock, closing about two minutes from
/// mint, with a one-HOUR ttl on the ask, so a fix that only shortens the
/// grant's own `ttl_s` cannot pass this by coincidence.
///
/// Watch it fail by reverting the fifth clamp in `clamp_to_grant`: minted
/// keeps its full one-hour `expires_at_ms`, `may_relay` two minutes later
/// still says `Ok(())`, and the window closed ten minutes ago as far as the
/// schedule is concerned.
#[test]
fn a_lease_minted_before_its_window_closes_stops_serving_at_the_close() {
    use tcr::peer::schedule::Between;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([24_u8; 32]);

    let now_utc = time::OffsetDateTime::now_utc();
    let offset = time::UtcOffset::local_offset_at(now_utc).unwrap_or(time::UtcOffset::UTC);
    let local = now_utc.to_offset(offset);
    let minute = |shift: i16| -> (u8, u8) {
        let total = i32::from(local.hour()) * 60 + i32::from(local.minute()) + i32::from(shift);
        let total = total.rem_euclid(24 * 60);
        ((total / 60) as u8, (total % 60) as u8)
    };
    let (start_h, start_m) = minute(-3);
    let (end_h, end_m) = minute(2);
    let window = format!("{start_h:02}:{start_m:02}-{end_h:02}:{end_m:02}");

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        // A full hour, deliberately far longer than the two minutes left on
        // the window: the clamp this test guards is the one that shortens
        // THIS, not the grant's own `ttl_s`.
        ttl_s: 3600,
        max_inflight: 2,
    };
    let mut grant = LendGrant::new(Window::SevenDay, 0.20, 3600, 2);
    grant.between = Some(window.parse::<Between>().expect("the window parses"));
    let store = lender_store(&peers, borrower, vec![grant]);

    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    let minted = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("the window is open right now, so this mints");

    let mint_ms = tcr::now_ms();
    assert!(
        minted.expires_at_ms - mint_ms <= 2 * 60_000 + 5_000,
        "expires_at_ms must be clamped to the window's close, not the ask's one-hour ttl: \
         got {} ms out from mint",
        minted.expires_at_ms - mint_ms
    );

    // Two minutes on, past the window's close and nowhere near the lease's
    // full one-hour ttl.
    let two_minutes_on = mint_ms + 2 * 60_000;
    assert_eq!(
        ledger.may_relay(minted.lease_id, two_minutes_on),
        Err(LeaseRefusal::LeaseExpired),
        "a lease whose window closed must stop serving at the close, not ride out its own \
         renewal ttl"
    );
}

/// **A lease's scope is recorded on the LENDER and never on the wire.**
///
/// The wire `Lease` carries window, unit, amount, ttl and a lease
/// id, nothing that names an account. So the ledger knows what the lease draws
/// from and the message does not.
///
/// The second half is asserted on the serialized message rather than by
/// reading the struct: a field added to `Lease` would be invisible to a
/// field-by-field check that only looks at the fields it knows.
///
/// Watch it fail by making `Ledger::grant` record with `LendScope::All`: the
/// first assertion then reads `all`.
#[test]
fn a_lease_scope_stays_on_the_lender_and_never_reaches_the_wire() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([22_u8; 32]);

    let mut scoped = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    scoped.scope = tcr_peer_wire::LendScope::Group("work".to_string());
    let store = lender_store(&peers, borrower, vec![scoped]);

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    // A fleet that can hold every scope: this test is about what the ledger
    // RECORDS beside a lease, not about which scopes are enforceable, and
    // `NoFleetUtilization` would skip the grant before there was a lease to ask
    // about.
    let lease = ledger
        .grant(&borrower, &ask, &store, &AnyScope)
        .answer
        .lease
        .expect("a scoped grant mints a lease like any other");

    assert_eq!(
        ledger.scope_of(lease.lease_id),
        tcr_peer_wire::LendScope::Group("work".to_string()),
        "the lender knows which accounts this lease may draw from"
    );
    // A lease id the ledger never heard of is `All`, which is what every lease
    // minted before scope existed meant.
    assert_eq!(ledger.scope_of(0xdead_beef), tcr_peer_wire::LendScope::All);

    let wire = serde_json::to_string(&lease).expect("a lease serializes");
    for named in ["work", "scope", "group", "account"] {
        assert!(
            !wire.contains(named),
            "the wire lease must not carry {named:?}: {wire}"
        );
    }
}

/// **`lentTo` carries the real scope, the real end, and whether that end has
/// passed**: the account-side view, with the lease's scope in it.
///
/// The `All` case is the sibling test above
/// (`lent_to_names_every_mac_a_grant_lends_one_account_to`). This one is the
/// half that was hardcoded before this fix: a `group:work` lease reaches the
/// grouped account and NOT the ungrouped one, an ended lease is still a row
/// (the ended lease stays, greyed, so it can be re-lent), and the lease id is
/// on the line so the panel's click target can revoke exactly what it shows.
///
/// Watch it fail by reading `LendScope::All` for every grant again, the way
/// `lent_to` did before this fix: `spare-fake` then gets a line too.
#[test]
fn lent_to_carries_the_real_scope_the_real_end_and_whether_it_has_passed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let now_s = u64::try_from(tcr::now_ms() / 1_000).expect("a positive clock");

    let mut live = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    live.scope = tcr_peer_wire::LendScope::Group("work".to_string());
    live.until = Some(now_s + 3_600);
    let mut over = LendGrant::new(Window::FiveHour, 0.10, 300, 1);
    over.scope = tcr_peer_wire::LendScope::Accounts(vec!["lender-fake".to_string()]);
    over.until = Some(now_s - 60);

    let mut attic = lender_row_for(PeerId([23_u8; 32]), vec![live, over]);
    attic.label = "attic-nuc".to_string();
    write_peers(&peers, vec![attic]);
    let store = PeerStore::open(&peers).expect("the peers file opens");

    let accounts = vec![
        (
            "lender-fake".to_string(),
            vec!["work".to_string()],
            Provider::Anthropic,
        ),
        ("spare-fake".to_string(), Vec::new(), Provider::Anthropic),
    ];
    let lent = lease::lent_to(&store, &accounts);

    assert!(
        !lent.contains_key("spare-fake"),
        "neither scope names the ungrouped account, so it is absent from the map: {lent:?}"
    );
    let on_lender = lent
        .get("lender-fake")
        .expect("both scopes cover the grouped account");
    assert_eq!(on_lender.len(), 2, "two leases, two rows: {on_lender:?}");

    assert_eq!(
        on_lender[0].scope,
        tcr_peer_wire::LendScope::Group("work".to_string())
    );
    assert_eq!(on_lender[0].until, Some(now_s + 3_600));
    assert!(!on_lender[0].ended, "an hour ahead has not passed");
    assert_eq!(
        on_lender[1].scope,
        tcr_peer_wire::LendScope::Accounts(vec!["lender-fake".to_string()])
    );
    assert!(
        on_lender[1].ended,
        "a minute ago has passed, and the row is KEPT so it can be re-lent"
    );

    // The lease ids are on the line, distinct, and hex rather than a JSON
    // number no double can hold.
    assert_ne!(on_lender[0].id, on_lender[1].id);
    assert_eq!(
        on_lender[0].id.len(),
        32,
        "32 hex characters: {:?}",
        on_lender[0].id
    );
    let json = serde_json::to_string(&lent).expect("the lentTo map serializes");
    assert!(
        json.contains(&format!("\"id\":\"{}\"", on_lender[0].id)),
        "the id is a string in the JSON a panel parses: {json}"
    );
    assert!(json.contains("\"ended\":true"), "{json}");
}
