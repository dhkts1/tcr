//! Replay: the ledger round trip, the handoff rebuild, the weekly window, and resolving a line
//! to the account that wrote it.

use super::*;

/// THE LEDGER ROUND TRIP: write N records through a tracker with a ledger
/// attached, then replay the files into a FRESH tracker and get identical
/// totals. This is what makes a restart cost nothing.
#[test]
fn a_replayed_ledger_reproduces_the_totals_exactly() {
    let dir = scratch("ledger");
    let names = vec![named("alice@example.com"), named("bob@example.com")];
    let now = crate::now_ms();

    let writer = tracker();
    let report = writer.attach_ledger(dir.clone(), 90, &names, now);
    assert_eq!(report.replayed, 0, "an empty directory replays nothing");
    assert!(writer.is_persisting());
    for i in 0..5 {
        writer.record(
            0,
            &UsageRecord::from_quota_input(
                now - i * 60_000,
                Some("claude-opus-5".to_string()),
                Some(7),
                5_500,
                200,
                300,
                4_000,
                50,
            ),
            &names[0].name,
            None,
            None,
        );
    }
    writer.record(
        1,
        &record(now, "claude-haiku-4-5-20251001", 10),
        &names[1].name,
        None,
        None,
    );
    // The write is off the request path now, so the file is complete only
    // after the writer has drained — see the module docs.
    assert!(writer.flush_ledger(), "the writer answered the flush");
    let before = writer.row(0, now, Some(now + 60_000));

    let replayed = tracker();
    let report = replayed.attach_ledger(dir.clone(), 90, &names, now);
    assert_eq!(report.replayed, 6, "every line came back");
    assert_eq!(report.unresolved, 0);
    assert_eq!(report.malformed, 0);
    let after = replayed.row(0, now, Some(now + 60_000));
    assert_eq!(
        before, after,
        "a replayed ledger reproduces the row exactly"
    );
    assert_eq!(after.today.requests, 5);
    assert_eq!(after.today.cache_creation_1h_tokens, 1_500);
    assert_eq!(
        after.today.cache_creation_tokens, 2_500,
        "cacheCreationTokens is BOTH TTLs, the same quantity the row-level field carries"
    );
    assert_eq!(
        replayed.row(1, now, None).today.requests,
        1,
        "the second account's line was attributed to it, not to the first"
    );

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// After a listener handoff two processes append to one day file: the
/// successor replayed it at boot while the predecessor was still draining,
/// so every line the predecessor wrote after that replay is on disk and
/// missing from the successor's totals. Rebuilding from the whole file once
/// the predecessor is done counts every line of both writers exactly once,
/// including the successor's own lines and the ones it records while the
/// rebuild runs.
#[test]
fn a_rebuild_counts_every_line_of_both_writers_once() {
    let dir = scratch("rebuild");
    let names = vec![named("alice@example.com")];
    let now = crate::now_ms();
    let line = move || record(now, "claude-opus-5", 10);

    // Lines on disk before either process below started.
    let earlier = tracker();
    earlier.attach_ledger(dir.clone(), 90, &names, now);
    for _ in 0..3 {
        earlier.record(0, &line(), &names[0].name, None, None);
    }
    assert_eq!(
        earlier.shutdown_ledger(std::time::Duration::from_secs(5)),
        LedgerShutdown::Flushed
    );

    // The successor boots and replays them.
    let successor = Arc::new(tracker());
    let report = successor.attach_ledger(dir.clone(), 90, &names, now);
    assert_eq!(
        report.replayed, 3,
        "positive control: the boot replay read the file"
    );

    // The predecessor, still draining, writes to the same file after that replay.
    let predecessor = tracker();
    predecessor.attach_ledger(dir.clone(), 90, &names, now);
    for _ in 0..4 {
        predecessor.record(0, &line(), &names[0].name, None, None);
    }
    assert_eq!(
        predecessor.shutdown_ledger(std::time::Duration::from_secs(5)),
        LedgerShutdown::Flushed
    );

    // The successor serves requests of its own in the same window.
    for _ in 0..2 {
        successor.record(0, &line(), &names[0].name, None, None);
    }
    assert_eq!(
        successor.row(0, now, None).today.requests,
        5,
        "positive control: before the rebuild the predecessor's four lines are missing"
    );

    // Requests that land while the rebuild runs.
    let recording = Arc::clone(&successor);
    let alice = names[0].name.clone();
    let during = std::thread::spawn(move || {
        for _ in 0..50 {
            recording.record(0, &line(), &alice, None, None);
        }
    });
    let rebuilt = successor.rebuild_from_ledger(&names, now);
    during.join().expect("the recording thread finished");

    assert!(rebuilt.is_ok(), "the rebuild ran: {rebuilt:?}");
    assert_eq!(
        successor.row(0, now, None).today.requests,
        3 + 4 + 2 + 50,
        "every line of both writers, and every request recorded during the rebuild, \
         counted exactly once"
    );
    assert!(successor.flush_ledger(), "the writer answered the flush");
    assert_eq!(
        lines_in(&dir, now),
        3 + 4 + 2 + 50,
        "the file holds the same lines the totals count"
    );
    assert_eq!(
        successor.shutdown_ledger(std::time::Duration::from_secs(5)),
        LedgerShutdown::Flushed
    );
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// One live Codex account with no org.
fn codex_named(name: &str) -> LedgerAccount {
    LedgerAccount {
        name: name.to_string(),
        provider: Provider::Codex,
        ..LedgerAccount::default()
    }
}

/// A Codex account's weekly window survives a restart. Boot replays the eight UTC day
/// files a seven-day window can reach back into, and the pruner keeps them whatever the
/// retention says. The window sums what the account spent since it opened, at minute
/// granularity. A window that opened before the oldest minute the accumulator holds is not
/// reported at all. The control: a Claude-only fleet replays and prunes exactly as before.
#[test]
fn the_weekly_window_replays_across_day_files() {
    let dir = scratch("weekly");
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let now: i64 = 1_800_000_000_000; // 2027-01-15T08:00:00Z
    let day: i64 = 86_400_000;
    let bob = "bob@example.com/codex";
    let line = |t: i64, a: &str, m: &str| {
        format!(r#"{{"t":{t},"a":"{a}","m":"{m}","i":100,"c5":0,"c1":0,"r":50,"o":10,"rt":4}}"#)
    };
    let mut files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (t, count) in [
        (now - 8 * day, 1),
        (now - 6 * day, 2),
        (now - 3 * day, 2),
        (now - day, 2),
        (now - 3_600_000, 2),
    ] {
        for _ in 0..count {
            files
                .entry(date_string(ledger_date(t)))
                .or_default()
                .push(line(t, bob, "gpt-5.1-codex"));
        }
    }
    files
        .entry(date_string(ledger_date(now)))
        .or_default()
        .push(line(now - 60_000, "alice@example.com", "claude-opus-5"));
    for (date, lines) in &files {
        std::fs::write(dir.join(format!("{date}.jsonl")), lines.join("\n") + "\n")
            .expect("write a day file");
    }

    let tracker = tracker();
    let report = tracker.attach_ledger(
        dir.clone(),
        0,
        &[named("alice@example.com"), codex_named(bob)],
        now,
    );
    assert_eq!(
        (report.replayed, report.unresolved, report.malformed),
        (9, 0, 0),
        "bob's eight lines inside the week and alice's one; the line from eight days ago is not read"
    );
    assert_eq!(
        report.pruned, 1,
        "with retention 0, only the file older than the week goes"
    );

    let reset = now + day;
    let week = tracker
        .seven_day_window(1, now, reset, WEEK_MS)
        .expect("a seven-day window");
    assert_eq!(week.since, reset - WEEK_MS);
    let t = week.totals;
    assert_eq!(
        (
            t.requests,
            t.input_tokens,
            t.cache_read_tokens,
            t.output_tokens,
            t.reasoning_tokens,
            t.unpriced_requests,
            t.cost_usd
        ),
        (8, 800, 400, 80, 32, 8, None)
    );
    let short = tracker
        .seven_day_window(1, now, reset, 3 * day)
        .expect("a three-day window");
    assert_eq!(
        short.totals.requests, 4,
        "only the lines since it opened, two days ago"
    );
    assert!(
        tracker.seven_day_window(1, now, reset, 9 * day).is_none(),
        "it opened eight days ago, before the oldest minute the week holds"
    );

    let claude_only = self::tracker();
    let report = claude_only.attach_ledger(
        dir.clone(),
        0,
        &[named("alice@example.com"), named(bob)],
        now,
    );
    assert_eq!(
        report.replayed, 5,
        "yesterday's and today's files only, as before"
    );
    assert_eq!(
        report.pruned, 2,
        "every file older than yesterday goes, as before"
    );
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// A Codex account's 5-hour `window` is still the ring every Claude row reads. The weekly
/// accumulator sits beside it, and `usage.window` is never repointed at the week. Its
/// boundary is a Claude row's: 4h58m in, 5h01m out.
#[test]
fn the_five_hour_window_is_unchanged() {
    let tracker = tracker();
    let now = 1_800_000_000_000;
    let codex = |ts: i64| UsageRecord {
        provider: Provider::Codex,
        ..record(ts, "gpt-5.1-codex", 100)
    };
    for ts in [
        now - (4 * 60 + 58) * 60_000,
        now - (5 * 60 + 1) * 60_000,
        now - 2 * 86_400_000,
    ] {
        tracker.record_in_memory(1, &codex(ts));
    }
    let row = tracker.row(1, now, Some(now + 60_000));
    assert_eq!(row.window.expect("a 5-hour window").totals.requests, 1);
    assert_eq!(row.last_hour.requests, 0);
    assert!(
        row.seven_day_window.is_none(),
        "the row alone never names the week"
    );
    let week = tracker
        .seven_day_window(1, now, now + 86_400_000, WEEK_MS)
        .expect("a week");
    assert_eq!(week.totals.requests, 3);
}

/// A ledger line naming an account this config no longer has is dropped,
/// not attributed to whoever now sits at that index.
#[test]
fn a_line_for_an_unknown_account_is_not_reattributed() {
    let dir = scratch("unknown");
    let now = crate::now_ms();
    let writer = tracker();
    writer.attach_ledger(dir.clone(), 90, &[named("gone@example.com")], now);
    writer.record(
        0,
        &record(now, "claude-opus-5", 5),
        "gone@example.com",
        None,
        None,
    );
    assert!(writer.flush_ledger());

    let replayed = tracker();
    let report = replayed.attach_ledger(dir.clone(), 90, &[named("alice@example.com")], now);
    assert_eq!(report.replayed, 0);
    assert_eq!(report.unresolved, 1);
    assert_eq!(replayed.row(0, now, None).today.requests, 0);
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDING 4. The same email in two orgs is TWO accounts (`identity.rs`),
/// and each one's day must replay onto its own index. Resolving by name
/// alone put both onto whichever came first: one account's day doubled, the
/// other's zeroed, and the boot log still said `unresolved=0`.
#[test]
fn two_accounts_sharing_a_name_replay_to_their_own_orgs() {
    let dir = scratch("same-name");
    let now = crate::now_ms();
    let accounts = vec![
        LedgerAccount {
            name: "alice@example.com".to_string(),
            org: Some("org-a".to_string()),
            ..LedgerAccount::default()
        },
        LedgerAccount {
            name: "alice@example.com".to_string(),
            org: Some("org-b".to_string()),
            ..LedgerAccount::default()
        },
    ];

    let writer = tracker();
    writer.attach_ledger(dir.clone(), 90, &accounts, now);
    writer.record(
        0,
        &record(now, "claude-opus-5", 1_000),
        "alice@example.com",
        None,
        Some("org-a"),
    );
    for _ in 0..3 {
        writer.record(
            1,
            &record(now, "claude-opus-5", 7),
            "alice@example.com",
            None,
            Some("org-b"),
        );
    }
    assert!(writer.flush_ledger());

    let replayed = tracker();
    let report = replayed.attach_ledger(dir.clone(), 90, &accounts, now);
    assert_eq!(report.replayed, 4);
    assert_eq!(report.unresolved, 0);
    assert_eq!(
        replayed.row(0, now, None).today.requests,
        1,
        "org A replays onto org A's index only"
    );
    assert_eq!(
        replayed.row(1, now, None).today.requests,
        3,
        "org B keeps the day it actually served"
    );
    assert_eq!(
        replayed.row(0, now, None).today.input_tokens,
        1_000,
        "and the tokens went with it"
    );

    // A line whose org matches NOTHING, against an ambiguous name, is
    // unresolved rather than guessed onto the first match.
    let orphan = tracker();
    let report = orphan.attach_ledger(dir.clone(), 90, &accounts, now);
    assert_eq!(report.unresolved, 0, "control: these four all resolve");
    let ambiguous = vec![named("alice@example.com"), named("alice@example.com")];
    let guessing = tracker();
    let report = guessing.attach_ledger(dir.clone(), 90, &ambiguous, now);
    assert_eq!(
        report.replayed, 0,
        "two live accounts wear this name and neither carries the org: a name match \
         would be a coin flip"
    );
    assert_eq!(report.unresolved, 4);

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDING 2 (round 2). A line that CARRIES an org and matches no live
/// (name, org) pair is unresolved, never handed to a same-named survivor.
/// Falling through to the name-only path replayed a removed sibling's whole
/// day onto the account that remained — the misattribution the `g` field
/// exists to prevent — while the boot log reported `unresolved=0`.
#[test]
fn a_line_whose_org_is_gone_is_unresolved_not_given_to_the_same_name() {
    let dir = scratch("org-gone");
    let now = crate::now_ms();
    let both = vec![
        LedgerAccount {
            name: "alice@example.com".to_string(),
            org: Some("org-a".to_string()),
            ..LedgerAccount::default()
        },
        LedgerAccount {
            name: "alice@example.com".to_string(),
            org: Some("org-b".to_string()),
            ..LedgerAccount::default()
        },
    ];

    let writer = tracker();
    writer.attach_ledger(dir.clone(), 90, &both, now);
    writer.record(
        0,
        &record(now, "claude-opus-5", 11),
        "alice@example.com",
        None,
        Some("org-a"),
    );
    for _ in 0..3 {
        writer.record(
            1,
            &record(now, "claude-opus-5", 22),
            "alice@example.com",
            None,
            Some("org-b"),
        );
    }
    assert!(writer.flush_ledger());

    // org-b is removed; one live `alice@example.com` remains.
    let survivor = vec![LedgerAccount {
        name: "alice@example.com".to_string(),
        org: Some("org-a".to_string()),
        ..LedgerAccount::default()
    }];
    let replayed = tracker();
    let report = replayed.attach_ledger(dir.clone(), 90, &survivor, now);
    assert_eq!(report.replayed, 1, "only org A's own line replays");
    assert_eq!(
        report.unresolved, 3,
        "org B's day is counted as unresolved and said out loud, not given away"
    );
    assert_eq!(
        replayed.row(0, now, None).today.requests,
        1,
        "org A's day is its own traffic and nothing else's"
    );
    assert_eq!(replayed.row(0, now, None).today.input_tokens, 11);

    // The pre-identity shape still resolves: a line with NO org against a
    // uniquely-named account is not ambiguous and must keep replaying.
    let dir2 = scratch("org-none");
    let old = tracker();
    old.attach_ledger(dir2.clone(), 90, &[named("alice@example.com")], now);
    old.record(
        0,
        &record(now, "claude-opus-5", 7),
        "alice@example.com",
        None,
        None,
    );
    assert!(old.flush_ledger());
    let replayed = tracker();
    let report = replayed.attach_ledger(dir2.clone(), 90, &survivor, now);
    assert_eq!(
        (report.replayed, report.unresolved),
        (1, 0),
        "control: an org-less line still resolves by a unique name"
    );

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
    std::fs::remove_dir_all(&dir2).log_debug("removing a test directory");
}

/// An account with the full identity, written the way
/// `Manager::attach_usage_ledger` writes it.
fn identified(name: &str, uuid: &str, org_uuid: Option<&str>, org_name: &str) -> LedgerAccount {
    LedgerAccount {
        name: name.to_string(),
        uuid: Some(uuid.to_string()),
        // `org_key`: the uuid when there is one, else the name.
        org: Some(org_uuid.unwrap_or(org_name).to_string()),
        org_name: Some(org_name.to_string()),
        provider: Provider::Anthropic,
    }
}

/// Record one line under `account`'s own identity, as the manager does.
fn record_as(tracker: &UsageTracker, idx: usize, account: &LedgerAccount, input: u64) {
    record_as_at(tracker, idx, account, input, crate::now_ms());
}

/// [`record_as`], stamped `ts_ms`: a test that replays and reads its lines at one instant
/// stamps them with that instant, so no midnight can fall between the stamp and the read.
fn record_as_at(
    tracker: &UsageTracker,
    idx: usize,
    account: &LedgerAccount,
    input: u64,
    ts_ms: i64,
) {
    tracker.record(
        idx,
        &record(ts_ms, "claude-opus-5", input),
        &account.name,
        account.uuid.as_deref(),
        account.org.as_deref(),
    );
}

/// Two seats of one ChatGPT workspace are two accounts, and a replay tells their lines
/// apart: each Codex row names its user as the uuid and its workspace as the org, so each
/// line resolves to the seat that wrote it. The control is the shape a Codex row had before
/// the user was split from the workspace, the workspace as the uuid on both and no org: two
/// accounts carry that uuid, and a coin flip between them is refused, so both lines replay
/// as unresolved.
#[test]
fn ledger_replay_tells_two_seats_of_one_workspace_apart() {
    const WORKSPACE: &str = "66666666-6666-4666-8666-666666666666";
    let seat = |name: &str, uuid: &str, org: Option<&str>| LedgerAccount {
        name: name.to_string(),
        uuid: Some(uuid.to_string()),
        org: org.map(str::to_string),
        org_name: None,
        provider: Provider::Codex,
    };
    let now = crate::now_ms();
    for (tag, seats, want) in [
        (
            "seats-split",
            [
                seat(
                    "alice@example.com/codex",
                    "55555555-5555-4555-8555-555555555555",
                    Some(WORKSPACE),
                ),
                seat(
                    "bob@example.com/codex",
                    "77777777-7777-4777-8777-777777777777",
                    Some(WORKSPACE),
                ),
            ],
            (2, 0),
        ),
        (
            "seats-joined",
            [
                seat("alice@example.com/codex", WORKSPACE, None),
                seat("bob@example.com/codex", WORKSPACE, None),
            ],
            (0, 2),
        ),
    ] {
        let dir = scratch(tag);
        let writer = tracker();
        writer.attach_ledger(dir.clone(), 90, &seats, now);
        // Stamped `now`, the instant the replay and the reads below use: a fresh clock
        // read here can land past a UTC midnight (the line goes to a file the replay never
        // reads) or a local one (the day `today` reads is empty).
        record_as_at(&writer, 0, &seats[0], 3, now);
        record_as_at(&writer, 1, &seats[1], 5, now);
        assert!(writer.flush_ledger());

        let replayed = tracker();
        let report = replayed.attach_ledger(dir.clone(), 90, &seats, now);
        assert_eq!((report.replayed, report.unresolved), want, "{tag}");
        if want == (2, 0) {
            assert_eq!(
                replayed.row(0, now, None).today.input_tokens,
                3,
                "alice's own line"
            );
            assert_eq!(
                replayed.row(1, now, None).today.input_tokens,
                5,
                "bob's own line"
            );
        }
        std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
    }
}

/// FINDING 1 (round 3). A re-login BACKFILLS a missing `org_uuid`
/// (`Manager::add_or_update_account`), so `org_key` stops returning the org
/// NAME and starts returning the uuid — half way through a day, on a routine
/// credential refresh nobody thinks of as an identity edit. Keyed on the org
/// alone, every line written before that moment matched nothing at the next
/// restart and the whole morning replayed as `unresolved`: five hours of
/// requests, tokens and `costUsd` gone from today's totals, presented as a
/// measured number.
#[test]
fn an_org_uuid_backfill_does_not_orphan_the_morning() {
    let dir = scratch("org-backfill");
    let now = crate::now_ms();

    // 09:00. The config carries an org NAME and no org uuid, so every line
    // this account writes carries `g: "Acme"`.
    let before = identified("alice@example.com", "acct-1", None, "Acme");
    // 14:00, after the refresh: the same account, one field richer.
    let after = identified("alice@example.com", "acct-1", Some("org-uuid-1"), "Acme");

    let writer = tracker();
    writer.attach_ledger(dir.clone(), 90, std::slice::from_ref(&before), now);
    for _ in 0..5 {
        record_as(&writer, 0, &before, 3);
    }
    for _ in 0..2 {
        record_as(&writer, 0, &after, 7);
    }
    assert!(writer.flush_ledger());

    // The next restart — a TcrBar update forces one — reads the backfilled
    // config.
    let replayed = tracker();
    let report = replayed.attach_ledger(dir.clone(), 90, std::slice::from_ref(&after), now);
    assert_eq!(
        (report.replayed, report.unresolved),
        (7, 0),
        "the morning is this account's own traffic, and an org backfill is not a reason \
         to lose it"
    );
    assert_eq!(replayed.row(0, now, None).today.requests, 7);
    assert_eq!(
        replayed.row(0, now, None).today.input_tokens,
        5 * 3 + 2 * 7,
        "and the tokens came back with the requests"
    );

    // The same edit on an account that never had an `account_uuid` either —
    // the fully-legacy shape, where the org NAME on the line is the only
    // thing left to match, and it still names this account's org.
    let dir2 = scratch("org-backfill-legacy");
    let legacy_before = LedgerAccount {
        name: "bob@example.com".to_string(),
        org: Some("Initech".to_string()),
        org_name: Some("Initech".to_string()),
        ..LedgerAccount::default()
    };
    let legacy_after = LedgerAccount {
        org: Some("org-uuid-2".to_string()),
        ..legacy_before.clone()
    };
    let writer = tracker();
    writer.attach_ledger(dir2.clone(), 90, std::slice::from_ref(&legacy_before), now);
    for _ in 0..4 {
        record_as(&writer, 0, &legacy_before, 1);
    }
    assert!(writer.flush_ledger());
    let replayed = tracker();
    let report = replayed.attach_ledger(dir2.clone(), 90, &[legacy_after], now);
    assert_eq!(
        (report.replayed, report.unresolved),
        (4, 0),
        "the uuid and the name are the same org written twice, before and after the \
         backfill"
    );

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
    std::fs::remove_dir_all(&dir2).log_debug("removing a test directory");
}

/// FINDING 1 (round 3), the other half: what the account uuid must NOT buy.
/// A line resolves to the account that wrote it or to nobody — never to a
/// neighbour — so a uuid that names no live account is unresolved however
/// unique the name is, and one person's two orgs stay two accounts.
#[test]
fn a_uuid_resolves_to_its_own_account_or_to_nobody() {
    let dir = scratch("uuid-identity");
    let now = crate::now_ms();

    // One person, two orgs: two real accounts sharing a name AND a uuid.
    let corp = identified("alice@example.com", "acct-1", Some("org-a"), "Corp");
    let personal = identified("alice@example.com", "acct-1", Some("org-b"), "Personal");
    let fleet = vec![corp.clone(), personal.clone()];

    let writer = tracker();
    writer.attach_ledger(dir.clone(), 90, &fleet, now);
    record_as(&writer, 0, &corp, 11);
    for _ in 0..3 {
        record_as(&writer, 1, &personal, 22);
    }
    assert!(writer.flush_ledger());

    let replayed = tracker();
    let report = replayed.attach_ledger(dir.clone(), 90, &fleet, now);
    assert_eq!((report.replayed, report.unresolved), (4, 0));
    assert_eq!(
        replayed.row(0, now, None).today.requests,
        1,
        "a shared uuid is not a licence to hand one org's day to the other"
    );
    assert_eq!(replayed.row(1, now, None).today.requests, 3);

    // The org-b account is removed. Its lines name a uuid that IS still in
    // the fleet, on the sibling — and they are still not the sibling's.
    let survivor = vec![corp];
    let replayed = tracker();
    let report = replayed.attach_ledger(dir.clone(), 90, &survivor, now);
    assert_eq!(
        (report.replayed, report.unresolved),
        (1, 3),
        "the org this line names is not in this fleet; unresolved and counted is the \
         honest answer"
    );
    assert_eq!(replayed.row(0, now, None).today.requests, 1);

    // A uuid that matches NOTHING is unresolved even against a uniquely
    // named live account: a removed account's uuid stays gone.
    let stranger = vec![identified(
        "alice@example.com",
        "acct-9",
        Some("org-a"),
        "Corp",
    )];
    let replayed = tracker();
    let report = replayed.attach_ledger(dir.clone(), 90, &stranger, now);
    assert_eq!(
        (report.replayed, report.unresolved),
        (0, 4),
        "a unique name is not evidence about a uuid nobody in this fleet carries"
    );

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}
