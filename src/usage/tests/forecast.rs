//! The forecast's demand history: what boot reads, what a rebuild keeps.

use super::*;

#[test]
fn forecast_restart_gzip_dedup_removed_accounts_and_partial_days() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let now = 1_800_000_000_000_i64;
    let day = ledger_date(now - 2 * 86_400_000);
    let start = day.with_hms(0, 0, 0).unwrap().assume_utc().unix_timestamp() * 1000;
    let mut data = Vec::new();
    for hour in 0..24 {
        writeln!(
            &mut data,
            "{}",
            serde_json::json!({"t":start+hour*3_600_000+1,
            "a":"removed@example.com","m":"claude-opus-5","i":100,"c5":0,"c1":0,"r":700,"o":25})
        )
        .unwrap();
    }
    std::fs::write(plain_day_path(dir.path(), day), &data).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&data).unwrap();
    std::fs::write(gz_day_path(dir.path(), day), gz.finish().unwrap()).unwrap();
    let t = tracker();
    t.attach_ledger(dir.path().to_path_buf(), 29, &[], now);
    let before = t.forecast_estimates(now);
    assert_eq!(before.len(), 1);
    assert!(before[0].history_hours >= 23.0 && before[0].history_hours <= 24.0);
    assert!(
        (before[0].level["opus"].input - 100.0).abs() < 1e-8,
        "plain suffix was not replayed twice"
    );
    assert!(!before[0].partial);
    t.rebuild_from_ledger(&[], now).unwrap();
    assert_eq!(
        before,
        t.forecast_estimates(now),
        "handoff replaces history, never adds it"
    );
    assert_eq!(
        t.shutdown_ledger(std::time::Duration::from_secs(2)),
        LedgerShutdown::Flushed
    );
    let damaged = ledger_date(now - 86_400_000);
    std::fs::write(plain_day_path(dir.path(), damaged), b"malformed\n").unwrap();
    let t = tracker();
    t.attach_ledger(dir.path().to_path_buf(), 29, &[], now);
    assert!(
        t.forecast_estimates(now)[0].partial,
        "malformed history is never known zero"
    );
    assert_eq!(
        t.shutdown_ledger(std::time::Duration::from_secs(2)),
        LedgerShutdown::Flushed
    );
    std::fs::write(plain_day_path(dir.path(),damaged),serde_json::json!({"t":now-1000,
        "a":"removed@example.com","p":"future-provider","m":"claude-opus-5","i":1,"c5":0,"c1":0,"r":0,"o":0}).to_string()).unwrap();
    let t = tracker();
    t.attach_ledger(dir.path().to_path_buf(), 29, &[], now);
    assert!(
        t.forecast_estimates(now)[0].partial,
        "unknown provider is not filed as known demand"
    );
    assert_eq!(
        t.shutdown_ledger(std::time::Duration::from_secs(2)),
        LedgerShutdown::Flushed
    );
}

/// A handoff rebuild re-reads the quota window's files, never the forecast's 29 days,
/// and keeps the history built at boot and fed live since. The day five back is outside
/// a Claude-only fleet's two-day quota window; deleting its file after boot shows
/// whether the rebuild read it again.
#[test]
fn forecast_rebuild_keeps_the_boot_history_and_never_rereads_it() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let now = crate::now_ms();
    let day = ledger_date(now - 5 * 86_400_000);
    let start = day.with_hms(0, 0, 0).unwrap().assume_utc().unix_timestamp() * 1000;
    let mut data = Vec::new();
    for hour in 0..24 {
        writeln!(
            &mut data,
            "{}",
            serde_json::json!({"t":start+hour*3_600_000+1,
            "a":"alice@example.com","m":"claude-opus-5","i":100,"c5":0,"c1":0,"r":0,"o":25})
        )
        .unwrap();
    }
    std::fs::write(plain_day_path(dir.path(), day), &data).unwrap();
    let accounts = [named("alice@example.com")];
    let t = tracker();
    t.attach_ledger(dir.path().to_path_buf(), 29, &accounts, now);
    t.record(
        0,
        &record(now, "claude-opus-5", 100),
        "alice@example.com",
        None,
        None,
    );
    let before = t.forecast_estimates(now + 1);
    assert_eq!(before.len(), 1);
    assert!(
        before[0].history_hours >= 23.0,
        "the boot replay read the day five back"
    );
    std::fs::remove_file(plain_day_path(dir.path(), day)).unwrap();
    t.rebuild_from_ledger(&accounts, now + 1).unwrap();
    assert_eq!(
        t.row(0, now + 1, None).today.requests,
        1,
        "the rebuild replayed the quota window, including the live line"
    );
    assert_eq!(
        t.forecast_estimates(now + 1),
        before,
        "a rebuild neither resets the history nor reads the 29 days again"
    );
    assert_eq!(
        t.shutdown_ledger(std::time::Duration::from_secs(2)),
        LedgerShutdown::Flushed
    );
}

#[test]
fn forecast_unreadable_history_and_concurrent_rebuild_never_claim_complete_zero() {
    let dir = tempfile::tempdir().unwrap();
    let now = crate::now_ms();
    let day = ledger_date(now - 86_400_000);
    std::fs::create_dir(plain_day_path(dir.path(), day)).unwrap();
    let t = tracker();
    t.attach_ledger(
        dir.path().to_path_buf(),
        29,
        &[named("alice@example.com")],
        now,
    );
    assert!(t.forecast_history.lock().unwrap().partial);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for n in 0..100 {
                t.record(
                    0,
                    &record(now + n, "claude-opus-5", 100),
                    "alice@example.com",
                    None,
                    None,
                );
            }
        });
        for _ in 0..3 {
            t.rebuild_from_ledger(&[named("alice@example.com")], now + 1000)
                .unwrap();
        }
    });
    t.rebuild_from_ledger(&[named("alice@example.com")], now + 1000)
        .unwrap();
    assert_eq!(t.row(0, now + 1000, None).today.requests, 100);
    assert!(t.forecast_history.lock().unwrap().partial);
    assert_eq!(
        t.shutdown_ledger(std::time::Duration::from_secs(2)),
        LedgerShutdown::Flushed
    );
}
