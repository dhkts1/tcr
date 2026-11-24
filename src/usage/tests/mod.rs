//! The usage tests, by concern, and the helpers they share.

use super::day_files::{
    compress_closed_days, gz_day_path, plain_day_path, read_ledger_day, write_gz,
};
use super::writer::ensure_dir;
use super::*;
use crate::log_err::LogErr as _;

fn tracker() -> UsageTracker {
    UsageTracker::new(2, PricingTable::default())
}

fn record(ts_ms: i64, model: &str, input: u64) -> UsageRecord {
    UsageRecord::from_quota_input(ts_ms, Some(model.to_string()), None, input, 0, 0, 0, 0)
}

/// One live account with no org — the single-org shape every pre-identity
/// config has.
fn named(name: &str) -> LedgerAccount {
    LedgerAccount {
        name: name.to_string(),
        ..LedgerAccount::default()
    }
}

/// A scratch directory named after this process AND thread, so concurrent
/// tests never share one.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-usage-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
    dir
}

fn lines_in(dir: &Path, now: i64) -> usize {
    let path = dir.join(format!("{}.jsonl", date_string(ledger_date(now))));
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

mod buckets;
mod day_files;
mod forecast;
mod replay;
mod writer;
