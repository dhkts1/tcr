//! The in-memory buckets: the ring's windows, the day, pricing, and growth under load.

use super::*;

/// The window boundary, which is the whole reason the ring exists: a request
/// made after this account's current 5-hour window opened counts against it;
/// one from before belongs to the window before and must not.
///
/// The window here opened 4h59m ago (its reset is a minute from now), so
/// 4h58m ago is inside it and 5h01m ago is outside — the pair the bridge
/// names, either side of a boundary the ring decides at minute granularity.
#[test]
fn the_five_hour_window_admits_4h58m_and_excludes_5h01m() {
    let tracker = tracker();
    let now = 1_800_000_000_000;
    // Reset a minute out, so the window opened 4h59m ago.
    let reset = now + 60_000;
    tracker.record_in_memory(
        0,
        &record(now - (4 * 60 + 58) * 60_000, "claude-opus-5", 10),
    );
    tracker.record_in_memory(0, &record(now - (5 * 60 + 1) * 60_000, "claude-opus-5", 99));

    let row = tracker.row(0, now, Some(reset));
    let window = row.window.expect("a known reset names a window");
    assert_eq!(
        window.since,
        reset - FIVE_HOURS_MS,
        "the window opens five hours before its reset"
    );
    assert_eq!(
        window.totals.requests, 1,
        "4h58m ago is in the window, 5h01m ago is not"
    );
    assert_eq!(window.totals.input_tokens, 10, "and it is the right one");
    assert_eq!(row.today.requests, 2, "today keeps both");
}

/// An unknown reset cannot name a window start, so there is no window at all
/// — never a guessed span, and never a zero-filled one.
#[test]
fn an_unknown_reset_yields_no_window() {
    let tracker = tracker();
    let now = 1_800_000_000_000;
    tracker.record_in_memory(0, &record(now, "claude-opus-5", 10));
    let row = tracker.row(0, now, None);
    assert!(row.window.is_none());
    assert_eq!(row.today.requests, 1, "today is still measured");
}

/// `lastHour` is the trailing 60 minutes, and nothing older.
#[test]
fn last_hour_holds_59_minutes_and_drops_61() {
    let tracker = tracker();
    let now = 1_800_000_000_000;
    tracker.record_in_memory(0, &record(now - 59 * 60_000, "claude-opus-5", 7));
    tracker.record_in_memory(0, &record(now - 61 * 60_000, "claude-opus-5", 9));
    let row = tracker.row(0, now, None);
    assert_eq!(row.last_hour.requests, 1, "only the 59-minute-old record");
    assert_eq!(row.last_hour.input_tokens, 7);
    assert_eq!(row.today.requests, 2, "today keeps both");
}

/// An unknown model is counted and left unpriced. With NOTHING priced in the
/// bucket the cost is `null`, not `$0.00`.
#[test]
fn an_unknown_model_is_counted_but_unpriced() {
    let tracker = tracker();
    let now = 1_800_000_000_000;
    tracker.record_in_memory(0, &record(now, "some-unknown-model", 1_000));
    let row = tracker.row(0, now, None);
    assert_eq!(row.today.requests, 1);
    assert_eq!(row.today.unpriced_requests, 1);
    assert_eq!(row.today.cost_usd, None, "unpriced is null, never 0.0");

    // One priced request beside it makes the total partial, not absent.
    tracker.record_in_memory(0, &record(now, "claude-opus-5", 1_000_000));
    let row = tracker.row(0, now, None);
    assert_eq!(row.today.requests, 2);
    assert_eq!(row.today.unpriced_requests, 1);
    assert_eq!(row.today.cost_usd, Some(5.0));
}

/// The per-model split, which is what a total cannot be decomposed into
/// afterwards.
#[test]
fn today_splits_by_model() {
    let tracker = tracker();
    let now = 1_800_000_000_000;
    tracker.record_in_memory(0, &record(now, "claude-opus-5", 1_000_000));
    tracker.record_in_memory(0, &record(now, "claude-sonnet-5", 1_000_000));
    let row = tracker.row(0, now, None);
    assert_eq!(row.today_by_model.len(), 2);
    assert_eq!(
        row.today_by_model["claude-opus-5"].cost_usd,
        Some(5.0),
        "opus input is $5/MTok"
    );
    assert_eq!(
        row.today_by_model["claude-sonnet-5"].cost_usd,
        Some(2.0),
        "sonnet-5 input is $2/MTok"
    );
    assert_eq!(row.today.cost_usd, Some(7.0));
}

/// A day accumulator holding yesterday reads as an empty today, never as
/// yesterday's number relabelled.
#[test]
fn a_stale_day_reads_as_an_empty_today() {
    let tracker = tracker();
    let now = 1_800_000_000_000;
    tracker.record_in_memory(0, &record(now, "claude-opus-5", 1_000_000));
    let tomorrow = now + 86_400_000;
    let row = tracker.row(0, tomorrow, None);
    assert_eq!(row.today.requests, 0);
    assert_eq!(row.today_by_model.len(), 0);
}

/// FINDING 3. The rotation grows at runtime (`tcr login`, TcrBar's
/// add-account), and an account appended after boot serves real traffic. A
/// vector sized once at `Manager::new` dropped its every record and read
/// back `requests: 0` — a zero indistinguishable from a measurement.
#[test]
fn an_account_added_after_boot_is_recorded_and_read_back() {
    let tracker = tracker(); // sized for 2 accounts
    let now = 1_800_000_000_000;
    tracker.record_in_memory(4, &record(now, "claude-opus-5", 1_000_000));
    let row = tracker.row(4, now, None);
    assert_eq!(row.today.requests, 1, "an index past the boot size records");
    assert_eq!(row.today.input_tokens, 1_000_000);
    assert_eq!(row.today.cost_usd, Some(5.0), "and it is priced");
    assert_eq!(
        tracker.row(1, now, None).today.requests,
        0,
        "growing the vector did not disturb the accounts that were there"
    );
}

/// FINDING 8. Zero traffic is a MEASURED zero. An idle account reported
/// `today.costUsd: null` next to `lastHour.costUsd: 0` for the same absence
/// of traffic, and `null` is the wire's word for "could not be priced".
#[test]
fn an_idle_day_reports_a_zero_cost_not_null() {
    let tracker = tracker();
    let now = 1_800_000_000_000;

    // Never served at all.
    let row = tracker.row(0, now, None);
    assert_eq!(row.today.requests, 0);
    assert_eq!(
        row.today.cost_usd,
        Some(0.0),
        "an idle day is a measured zero, the same one lastHour reports"
    );
    assert_eq!(
        row.last_hour.cost_usd,
        Some(0.0),
        "control: lastHour agrees"
    );

    // Served yesterday, idle today — the shape after every local midnight.
    tracker.record_in_memory(1, &record(now, "claude-opus-5", 1_000_000));
    let tomorrow = now + 86_400_000;
    let row = tracker.row(1, tomorrow, None);
    assert_eq!(row.today.requests, 0);
    assert_eq!(
        row.today.cost_usd,
        Some(0.0),
        "a stale day accumulator reads as an empty day, not an unpriced one"
    );
}

/// FINDING 9. `cacheCreationTokens` carries the SAME quantity inside
/// `usage` as it does at row level: both TTLs together, with the 1-hour
/// part also broken out beside it.
#[test]
fn cache_creation_tokens_is_both_ttls() {
    let tracker = tracker();
    let now = 1_800_000_000_000;
    tracker.record_in_memory(
        0,
        &UsageRecord::from_quota_input(
            now,
            Some("claude-opus-5".to_string()),
            None,
            1_600_000,
            1_200_000,
            400_000,
            0,
            0,
        ),
    );
    let row = tracker.row(0, now, None);
    assert_eq!(
        row.today.cache_creation_tokens, 1_600_000,
        "5m + 1h, matching the row-level counter fed by `UsageRecord::cache_creation`"
    );
    assert_eq!(row.today.cache_creation_1h_tokens, 400_000, "the 1h subset");
    assert_eq!(
        row.today.cache_creation_tokens - row.today.cache_creation_1h_tokens,
        1_200_000,
        "and the 5-minute part is the difference"
    );
}

/// FINDING 7. `UsageRecord` holds the caller's quota figure verbatim, so
/// the cumulative counter grows by exactly what was passed even when the
/// caller's own components exceed it. The old code re-summed floored parts
/// and grew the counter by 800 for an `input_tokens` of 100.
#[test]
fn a_records_quota_input_is_the_callers_number_verbatim() {
    // The consistent shape: components are a subset of the total.
    let ok = UsageRecord::from_quota_input(0, None, None, 100, 20, 30, 40, 50);
    assert_eq!(ok.input_total(), 100, "the caller's number, unchanged");
    assert_eq!(
        ok.input, 10,
        "base input is what the cache dimensions leave"
    );
    assert_eq!(ok.cache_creation(), 50);

    // The inconsistent shape a `pub` entry point can be handed: 100 total,
    // 800 of components.
    let bad = UsageRecord::from_quota_input(0, None, None, 100, 50, 0, 750, 0);
    assert_eq!(
        bad.input_total(),
        100,
        "the quota counter still grows by exactly what the caller passed"
    );
    assert_eq!(bad.input, 0, "the base priced at the input rate is floored");
}

/// The quota counter's input figure must be reproducible from a record, or
/// routing this through `record_usage` would change what `inputTokens`
/// means on a wire that has carried it for months.
#[test]
fn input_total_folds_every_input_dimension() {
    let rec = UsageRecord::from_quota_input(0, None, None, 100, 20, 30, 40, 50);
    assert_eq!(rec.input, 10, "base input is the remainder");
    assert_eq!(rec.input_total(), 100);
    assert_eq!(rec.cache_creation(), 50);
}

/// Eight threads record at once: four share account 0, and each of the other four has an
/// account of its own past the end of a two-account tracker. Those four also record into a
/// fresh account every tenth request, so the list keeps growing under all of them. Every
/// request lands exactly once: account 0 holds four threads' worth, each own account one
/// thread's, each fresh account one request, today and in the last hour alike. A lost update
/// (two records reading the same bucket and writing back one) or a grow that drops an
/// account another thread just added shows up as a short count.
///
/// The grow race needs two grows to overlap, which one pass does not always produce, so the
/// scenario runs five times on fresh trackers (a planted shrink was caught in 4 of 6 single
/// passes).
///
/// The `tsan` CI job runs every test whose name starts `concurrent_lock_stress`.
#[test]
fn concurrent_lock_stress_usage() {
    for _ in 0..5 {
        usage_stress_pass();
    }
}

fn usage_stress_pass() {
    const THREADS: usize = 8;
    const PER_THREAD: u64 = 2_000;
    const FRESH_EVERY: u64 = 10;
    let tracker = Arc::new(tracker());
    let now = crate::now_ms();
    let account_of = |t: usize| if t.is_multiple_of(2) { 0 } else { t };
    // A fresh account per (odd thread, tenth request), past every own account.
    let fresh = |t: usize, i: u64| THREADS + 4 * (i / FRESH_EVERY) as usize + t / 2;
    let one = move |idx: usize, tracker: &UsageTracker| {
        tracker.record(
            idx,
            &record(now, "claude-opus-4-1", 3),
            "alice@example.com",
            None,
            None,
        );
    };
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let tracker = Arc::clone(&tracker);
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    one(account_of(t), &tracker);
                    if !t.is_multiple_of(2) && i.is_multiple_of(FRESH_EVERY) {
                        one(fresh(t, i), &tracker);
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("a stress thread panicked");
    }
    let shared = tracker.row(0, now, None);
    assert_eq!(shared.today.requests, 4 * PER_THREAD);
    assert_eq!(shared.today.input_tokens, 3 * 4 * PER_THREAD);
    assert_eq!(shared.last_hour.requests, 4 * PER_THREAD);
    for t in (1..THREADS).step_by(2) {
        let own = tracker.row(t, now, None);
        assert_eq!(own.today.requests, PER_THREAD, "account {t}");
        assert_eq!(own.last_hour.input_tokens, 3 * PER_THREAD, "account {t}");
        for i in (0..PER_THREAD).step_by(FRESH_EVERY as usize) {
            let idx = fresh(t, i);
            assert_eq!(
                tracker.row(idx, now, None).today.requests,
                1,
                "account {idx}"
            );
        }
    }
}
