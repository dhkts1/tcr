//! The ledger's day files on disk: the line format, reading a day in either form, gzipping
//! closed days, and pruning old ones.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};

use super::UsageRecord;
use crate::base::provider::Provider;

/// The UTC date a record's ledger file is named after.
///
/// `pub(crate)` so `tcr wrap` (`src/verbs/wrap.rs`) groups a line by the same day its
/// own file is named after, rather than re-deriving that from the timestamp a
/// second way.
pub(crate) fn ledger_date(ts_ms: i64) -> Date {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ts_ms) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .date()
}

/// `pub(crate)` so `tcr wrap` names the same `<date>.jsonl` file this module
/// writes and replays, instead of formatting the date a second way.
pub(crate) fn date_string(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// One ledger line. Short keys are deliberate — see the module docs on size.
///
/// `pub(crate)`, fields included, so `tcr wrap` (`src/verbs/wrap.rs`) reads the exact
/// shape [`parse_ledger_day`] parses rather than a second struct that could
/// drift from it field by field.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LedgerLine {
    /// Unix milliseconds.
    pub(crate) t: i64,
    /// The serving account's `name`. Replay resolves a line by identity and
    /// never by POSITION, so reordering or removing an account in the config
    /// cannot silently reattribute yesterday's traffic. The name is the weakest
    /// of the three keys ([`resolve_account`]) and the only one an account with
    /// no stored identity has.
    pub(crate) a: String,
    /// The serving account's `account_uuid` — the STABLE half of its identity,
    /// and the first thing [`resolve_account`] tries.
    ///
    /// The name and the org key both move under a live account: a re-login
    /// backfills a missing `org_uuid` (`Manager::add_or_update_account`), which
    /// changes what [`crate::base::identity::org_key`] returns from the org NAME to
    /// the org UUID from that moment on. Keyed on the org alone, the lines
    /// written before that edit match nothing afterwards and a whole morning
    /// replays as `unresolved` — a measured-looking zero, which is the one
    /// failure this module exists to prevent. The account UUID does not move.
    ///
    /// Absent for an API-key account and for every line written before this
    /// field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) u: Option<String>,
    /// The serving account's org discriminator ([`crate::base::identity::org_key`]),
    /// because a NAME is not an identity here: the same email legitimately
    /// appears twice in one fleet, once per org (`identity.rs`), and replaying
    /// both onto whichever index came first doubles one account's day and
    /// zeroes the other's. It is also what tells two orgs of the same PERSON
    /// apart, which is the one case `u` cannot decide alone.
    ///
    /// Absent when the account carries no org at all, which is every config
    /// written before org identity existed — such a line still resolves by
    /// name, but only while that name is unique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) g: Option<String>,
    /// The provider, written for a Codex line only (`"codex"`) and omitted for an Anthropic one,
    /// as `u` and `g` are omitted when empty, so a Claude-only ledger's lines do not change. A
    /// string, not [`Provider`], so a line from a later tcr naming a provider this one does not
    /// know still parses. A line written before this key existed has none, and is filed by its
    /// model id ([`line_provider`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) p: Option<String>,
    #[serde(default)]
    pub(crate) m: Option<String>,
    #[serde(default)]
    pub(crate) s: Option<u64>,
    pub(crate) i: u64,
    pub(crate) c5: u64,
    pub(crate) c1: u64,
    pub(crate) r: u64,
    pub(crate) o: u64,
    /// A Codex turn's reasoning tokens, already inside `o`; omitted at 0. Not `r`: that key is
    /// the cache-read count.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) rt: u64,
}

/// For `#[serde(skip_serializing_if)]` on [`LedgerLine::rt`]: a line with no reasoning tokens
/// writes no key, so an Anthropic line is byte for byte what it was.
const fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Parse one day's ledger into its lines, skipping blank lines and counting
/// (never failing on) malformed ones. The day is read in whichever form it is
/// on disk ([`read_ledger_day_checked`]).
///
/// `pub(crate)` and the ONLY place either [`UsageTracker::replay_day`] or
/// `tcr wrap` reads a ledger day off disk, so "a malformed line is counted,
/// not fatal" is one piece of behaviour instead of two copies that could drift.
/// A missing day is the ordinary case (no traffic that day) and returns
/// nothing, the same as replay always has.
pub(crate) fn parse_ledger_day(dir: &Path, date: Date) -> (Vec<LedgerLine>, usize) {
    let day = parse_ledger_day_checked(dir, date);
    (day.lines, day.malformed)
}

pub(super) struct DecodedDay {
    pub(super) lines: Vec<LedgerLine>,
    pub(super) malformed: usize,
    present: bool,
    complete: bool,
}

/// Fold one decoded UTC day into the forecast's demand history: every line through the
/// checked decoder, independently of account resolution, and the day's hours marked
/// covered only when the file was present, complete and clean.
pub(super) fn history_day(
    history: &mut super::demand_history::History,
    day: &DecodedDay,
    date: Date,
    now_ms: i64,
) {
    if !day.complete || day.malformed != 0 {
        history.mark_partial();
    }
    for line in &day.lines {
        if line
            .p
            .as_deref()
            .is_some_and(|p| Provider::from_name(p).is_none())
        {
            history.mark_partial();
            continue;
        }
        if let Some(record) = ledger_record(line, line_provider(line)) {
            history.record(&record);
        } else {
            history.mark_partial();
        }
    }
    if day.present && day.complete && day.malformed == 0 {
        let start = time::PrimitiveDateTime::new(date, time::Time::MIDNIGHT)
            .assume_utc()
            .unix_timestamp()
            * 1000;
        history.cover_day(start, (start + 86_400_000).min(now_ms));
    }
}

pub(super) fn parse_ledger_day_checked(dir: &Path, date: Date) -> DecodedDay {
    let day = read_ledger_day_checked(dir, date);
    let mut lines = Vec::new();
    let mut malformed = 0;
    for raw in day.text.lines().filter(|raw| !raw.trim().is_empty()) {
        match serde_json::from_str::<LedgerLine>(raw) {
            Ok(line) => lines.push(line),
            Err(_) => malformed += 1,
        }
    }
    DecodedDay {
        lines,
        malformed,
        present: day.present,
        complete: day.complete,
    }
}

/// The plain file a day's lines are appended to: `<UTC date>.jsonl`.
pub(super) fn plain_day_path(dir: &Path, date: Date) -> PathBuf {
    dir.join(format!("{}.jsonl", date_string(date)))
}

/// The file a closed day is compressed into: `<UTC date>.jsonl.gz`.
pub(super) fn gz_day_path(dir: &Path, date: Date) -> PathBuf {
    dir.join(format!("{}.jsonl.gz", date_string(date)))
}

/// Where [`compress_day`] writes before it renames into place. Dot-led, so no reader's
/// `<date>.jsonl` or `<date>.jsonl.gz` name ever matches it.
fn gz_day_temp_path(dir: &Path, date: Date) -> PathBuf {
    dir.join(format!(".{}.jsonl.gz.tmp", date_string(date)))
}

/// Every byte of `path` decompressed, or `None` when it cannot be read. Multi-member, so a
/// file that is several gzip streams back to back reads as all of them.
fn read_gz(path: &Path) -> Option<Vec<u8>> {
    read_gz_checked(path).ok()
}

fn read_gz_checked(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)?;
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file)).read_to_end(&mut out)?;
    Ok(out)
}

/// One day's ledger text, in whichever form it is on disk.
///
/// A day is plain (`<date>.jsonl`) while it is written and gzipped (`<date>.jsonl.gz`) once
/// closed ([`compress_closed_days`]). Both can exist at once, for two reasons, and this is the
/// one place that tells them apart:
///
/// - a compression stopped between renaming the `.gz` into place and removing the plain file:
///   the `.gz` already ends with every byte of the plain file, so the plain file is a leftover
///   and is not read a second time;
/// - a line arrived for a day after it was compressed, which reopens its plain file: the plain
///   file then holds only lines the `.gz` does not, and they are read after it.
///
/// An unreadable file reads as absent, as a missing one always has.
#[cfg(test)]
pub(super) fn read_ledger_day(dir: &Path, date: Date) -> String {
    read_ledger_day_checked(dir, date).text
}

struct DayRead {
    text: String,
    present: bool,
    complete: bool,
}

/// The same decode/merge for totals and forecasts. Ordinary replay retains its historical
/// best-effort text; forecasts additionally see whether each source was readable UTF-8.
fn read_ledger_day_checked(dir: &Path, date: Date) -> DayRead {
    let plain = std::fs::read(plain_day_path(dir, date));
    let gz = read_gz_checked(&gz_day_path(dir, date));
    let absent = |result: &std::io::Result<Vec<u8>>| {
        result
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    };
    let present = !absent(&plain) || !absent(&gz);
    let complete = (plain.is_ok() || absent(&plain)) && (gz.is_ok() || absent(&gz));
    let bytes = match (gz.ok(), plain.ok()) {
        (None, None) => Vec::new(),
        (Some(bytes), None) | (None, Some(bytes)) => bytes,
        (Some(gz), Some(plain)) if gz.ends_with(&plain) => gz,
        (Some(mut gz), Some(plain)) => {
            gz.extend_from_slice(&plain);
            gz
        }
    };
    let valid_utf8 = std::str::from_utf8(&bytes).is_ok();
    DayRead {
        text: String::from_utf8_lossy_owned(bytes),
        present,
        complete: complete && valid_utf8,
    }
}

/// What one pass of [`compress_closed_days`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompressReport {
    /// Days whose plain file was compressed (or folded into an existing `.gz`) and removed.
    pub days: usize,
    /// The plain files' bytes before.
    pub plain_bytes: u64,
    /// The `.gz` files' bytes after.
    pub gz_bytes: u64,
    /// Days left plain because a step failed; each is retried on the next pass.
    pub failed: usize,
}

/// Gzip every closed day in `dir` that is still plain: every `<date>.jsonl` dated before
/// yesterday (`today` is a UTC date, as the file names are). Today's file is being written,
/// and yesterday's is left plain for a day so a late line (a request that began before
/// midnight) appends to a plain file, and boot's replay of yesterday reads it without
/// decompressing.
///
/// Each day is compressed safely ([`compress_day`]): the plain file is removed only after its
/// `.gz` has been read back and found identical. `between` runs after each day, so the ledger
/// writer, which runs this, answers its queue between files rather than once at the end.
pub(crate) fn compress_closed_days(
    dir: &Path,
    today: Date,
    mut between: impl FnMut(),
) -> CompressReport {
    let mut report = CompressReport::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return report;
    };
    let cutoff = today - time::Duration::days(1);
    let mut days: Vec<Date> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let stem = name.to_str()?.strip_suffix(".jsonl")?;
            parse_day(stem)
        })
        .filter(|date| *date < cutoff)
        .collect();
    days.sort();
    for date in days {
        match compress_day(dir, date) {
            Ok(Some((plain, gz))) => {
                report.days += 1;
                report.plain_bytes += plain;
                report.gz_bytes += gz;
            }
            Ok(None) => {}
            Err(err) => {
                report.failed += 1;
                tracing::warn!(
                    dir = %dir.display(),
                    date = %date_string(date),
                    error = %err,
                    "could not compress a closed usage-ledger day; it stays plain and is retried"
                );
            }
        }
        between();
    }
    report
}

/// A ledger file's date stem (`2026-10-16`) as a date.
fn parse_day(stem: &str) -> Option<Date> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    Date::parse(stem, &format).ok()
}

/// Compress one closed day: `<date>.jsonl` becomes `<date>.jsonl.gz`, folded after the lines
/// of an existing `.gz` when there is one. Returns the plain bytes and the `.gz` bytes, or
/// `None` when there was no plain file.
///
/// The order is what makes it safe to interrupt anywhere: write the `.gz` beside under a
/// temporary name, sync it, read it back and compare it with what it must hold, rename it into
/// place, and only then remove the plain file. A failure before the rename leaves the day as
/// it was. A stop after the rename leaves both files, the `.gz` ending with the plain file's
/// bytes, which [`read_ledger_day_checked`] reads once and the next pass finishes removing.
fn compress_day(dir: &Path, date: Date) -> std::io::Result<Option<(u64, u64)>> {
    let plain_path = plain_day_path(dir, date);
    let plain = match std::fs::read(&plain_path) {
        Ok(plain) => plain,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let gz_path = gz_day_path(dir, date);
    let mut content = match std::fs::metadata(&gz_path) {
        Ok(_) => read_gz(&gz_path).ok_or_else(|| {
            std::io::Error::other(format!("{} exists and does not read", gz_path.display()))
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    if !content.is_empty() && content.ends_with(&plain) {
        // A pass that stopped after its rename: the `.gz` already holds these lines.
        std::fs::remove_file(plain_path)?;
        return Ok(Some((
            plain.len() as u64,
            std::fs::metadata(gz_path)?.len(),
        )));
    }
    content.extend_from_slice(&plain);

    let temp = gz_day_temp_path(dir, date);
    let written = write_gz(&temp, &content).and_then(|()| {
        if read_gz(&temp).as_deref() == Some(content.as_slice()) {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "{} did not read back identical",
                temp.display()
            )))
        }
    });
    if let Err(err) = written.and_then(|()| std::fs::rename(&temp, &gz_path)) {
        if let Err(cleanup) = std::fs::remove_file(&temp)
            && cleanup.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %temp.display(), error = %cleanup, "could not remove a temporary usage-ledger file");
        }
        return Err(err);
    }
    std::fs::remove_file(plain_path)?;
    Ok(Some((
        plain.len() as u64,
        std::fs::metadata(gz_path)?.len(),
    )))
}

/// Write `content` gzipped to a new file at `path`, `0600` like the day files, synced to the
/// disk before it returns: the plain file it replaces is deleted on the strength of it.
pub(super) fn write_gz(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let mut encoder = flate2::write::GzEncoder::new(
        std::io::BufWriter::new(file),
        flate2::Compression::default(),
    );
    encoder.write_all(content)?;
    let file = encoder
        .finish()?
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?;
    file.sync_all()
}

/// The provider a ledger line belongs to: the one its `p` names, when this build knows it;
/// otherwise the one its model id names. An OpenAI id (`gpt-…`, `codex-…`) is Codex, and
/// every other is Anthropic. A line with no `p` was written before the key existed. If such
/// a line is Codex and its model id starts with neither prefix, it reads as Anthropic.
/// [`filed_by_model_id`] lets a report say how many lines it filed on the model id's word.
pub(crate) fn line_provider(line: &LedgerLine) -> Provider {
    line.p
        .as_deref()
        .and_then(Provider::from_name)
        .unwrap_or_else(|| {
            if line
                .m
                .as_deref()
                .is_some_and(crate::base::pricing::is_openai_model_id)
            {
                Provider::Codex
            } else {
                Provider::Anthropic
            }
        })
}

/// Reconstruct the exact dimensions once for ordinary replay and forecast history.
pub(super) fn ledger_record(line: &LedgerLine, provider: Provider) -> Option<UsageRecord> {
    Some(UsageRecord {
        ts_ms: line.t,
        model: line.m.clone(),
        session: line.s,
        quota_input: line
            .i
            .checked_add(line.c5)?
            .checked_add(line.c1)?
            .checked_add(line.r)?,
        input: line.i,
        cache_5m: line.c5,
        cache_1h: line.c1,
        cache_read: line.r,
        output: line.o,
        provider,
        reasoning: line.rt,
    })
}

/// Whether `line` carries no provider and its model id files it under Codex: a Codex line
/// written before the ledger recorded `p`. An Anthropic line never carries `p`, so a line
/// this rule files as Anthropic cannot be told from one that is.
pub(crate) fn filed_by_model_id(line: &LedgerLine) -> bool {
    line.p.is_none() && line_provider(line) == Provider::Codex
}

/// Delete `<date>.jsonl` files older than `retention_days` before `today`.
/// Anything in the directory that is not a dated ledger file is left alone —
/// this function deletes only names it can prove it wrote.
///
/// Today and YESTERDAY are never deleted, whatever the setting says. Those two
/// files are exactly what the boot replay reads, and `retention_days: 0` is
/// documented as the floor rather than as "delete the history you just
/// replayed": on a UTC+3 machine the first three hours of the local day live in
/// yesterday's UTC-named file, so pruning it costs the second restart of the
/// day those hours — silently, and still presented as measured.
///
/// A fleet holding a Codex account passes at least seven days, so the weekly
/// window's files survive the pruner too.
pub(super) fn prune(dir: &Path, today: Date, retention_days: u32) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff = (today - time::Duration::days(i64::from(retention_days)))
        .min(today - time::Duration::days(1));
    let mut pruned = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // A closed day is gzipped, today's and yesterday's are plain, and either
        // form ages out the same way.
        let Some(stem) = name
            .strip_suffix(".jsonl.gz")
            .or_else(|| name.strip_suffix(".jsonl"))
        else {
            continue;
        };
        let Some(date) = parse_day(stem) else {
            continue;
        };
        if date < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            pruned += 1;
        }
    }
    pruned
}
