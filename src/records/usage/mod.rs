//! Per-account usage buckets and the append-only usage ledger.
//!
//! # Why the proxy is the right place to count
//!
//! ccusage answers "what did I spend today" by re-parsing every Claude Code
//! transcript under `~/.claude/projects/` on each call: slow, blind to which
//! account served the request, and its "5-hour block" is a guess (first message,
//! floored to the UTC hour). This proxy already sees every request exactly once,
//! already parses all four token dimensions, already knows the model and the
//! serving account, and reads the REAL 5-hour window off Anthropic's headers.
//! Aggregating here at request time makes a poll a lookup instead of a sweep.
//!
//! # The three structures
//!
//! * A **360-slot ring of per-minute buckets** per account — six hours, which
//!   covers any 5-hour window plus the trailing hour with room to spare. A slot
//!   carries the minute it belongs to, so a stale slot is recognised and cleared
//!   rather than read as current, and the ring never needs sweeping.
//! * A **today accumulator** per account, keyed by the local calendar day. It is
//!   NOT derived from the ring: a day is 24 hours and the ring is six.
//! * A **weekly accumulator** per Codex account, of per-minute totals over the
//!   last seven days: the source of `usage.sevenDayWindow`, whose window reaches
//!   back past both the ring's six hours and midnight. Only Codex records land
//!   in it.
//!
//! The ring and the day are keyed by model, because "what did opus cost me
//! today" is the question, and a per-model split cannot be recovered from a
//! total afterwards. The week answers one total per window, so it keeps none.
//!
//! # Minute granularity
//!
//! Window membership is decided per MINUTE, not per millisecond: a request is in
//! the 5-hour window if its minute is at or after the window's opening minute.
//! At the boundary that admits at most 59 seconds of requests either side, which
//! is well inside the resolution anyone reads these numbers at, and it is what
//! makes the ring O(1) per record instead of a sorted list per account.
//!
//! # The ledger
//!
//! In-memory buckets die with the process, and this proxy is restarted for every
//! TcrBar update. One JSON line per served request goes to
//! `~/.cache/tcr/usage/<UTC-date>.jsonl` (directory `0700`, files `0600`
//! — the same posture as the session-affinity pin file and the log directory),
//! and boot replays today's and yesterday's files back through the same
//! recording path, and the seven days before today when the fleet holds a Codex
//! account, so its weekly window survives a restart too. Keys are one or two
//! characters on purpose: ~110 bytes a line at ~17k requests a day is ~2 MB,
//! against 40-66 MB/day for the tracing log.
//!
//! # The ledger write is not on the request path
//!
//! [`UsageTracker::record`] runs inside the proxy's request handling, on a tokio
//! worker. It therefore does NO file I/O: it hands the line to a bounded channel
//! ([`LEDGER_QUEUE_DEPTH`]) and returns, and one dedicated thread owns the open
//! file, the day rollover and every syscall. `~/.cache` on a stalled volume — a
//! network home directory, a full disk, an fsevents stall — then costs the
//! ledger, never the fleet: a multi-second `write` blocks that one thread while
//! requests keep being served. A full queue DROPS the line and counts the drop
//! rather than applying backpressure to traffic, because losing accounting is
//! not worth failing (or delaying) traffic.
//!
//! That is also why the in-memory update no longer comes second. The old
//! ordering ("ledger first, so a crash between the two loses the cheaper half")
//! bought a window measured in microseconds and cost a blocking syscall per
//! request; the line is now queued, so there is no ordering left to trade.
//!
//! A ledger that is not keeping up is reported on every TRANSITION — healthy to
//! failing and back — with the dropped-line count, and
//! [`UsageTracker::is_persisting`] answers FALSE for as long as it lasts. Three
//! separate states count as "not keeping up", because an operator asking "will
//! today's totals survive a restart" gets the same answer — no — from all three:
//! a write that FAILED, a queue that was FULL when a line arrived, and a writer
//! thread that has GONE. A ledger that dies at 10:00 must not go on claiming
//! today's totals will survive a restart, and it must say so at 10:00 rather
//! than at the next boot.
//!
//! # Who owns the health flags
//!
//! Two flags, and neither is written from both sides. `healthy` is the WRITER's,
//! set from write results and from nowhere else. `dropping` is the queue's to
//! RAISE — it is the only side that sees a line lost to a full queue — and the
//! writer's to LOWER, once it has proof the pressure eased (a completed write
//! with no new drop behind it). One warning when an episode starts, one line
//! when it ends.
//!
//! The alternative — letting the queue clear `healthy` and the writer set it —
//! is a flap, not a report: under sustained saturation every completed write
//! frees exactly one slot, the next request refills it and the one after is
//! dropped, so the pair flips on every write. That is a log line per served
//! request and a `persisting` that alternates between two reads instead of
//! reading false for as long as the incident lasts.
//!
//! # Shutdown
//!
//! The queue is the whole reason a shutdown has work to do: lines sitting in it
//! are counted, priced and visible in `tcr status`, and until the writer takes
//! them they are not on disk. [`UsageTracker::shutdown_ledger`] therefore
//! flushes the queue and JOINS the writer, so the day file is complete and
//! closed before the process exits — `ServerHandle::shutdown_within` calls it
//! after the affinity pins.
//!
//! The two steps get SEPARATE budgets. Sharing one deadline meant a flush that
//! spent it — which is precisely what a stalled volume produces — handed the
//! join zero milliseconds and abandoned a writer that had not been asked to stop
//! yet: the channel closes only when the flush returns and the handle drops, so
//! the join is the first moment the writer can finish at all. It now always gets
//! at least [`MIN_LEDGER_SHUTDOWN`] of its own after that, and a writer that
//! does not drain inside it is abandoned rather than waited on: a `tcr` that
//! quits into a stalled volume must still quit. The result says which happened,
//! and it never says `Abandoned` about a writer that finished.
//!
//! File names are UTC dates; "today" is a LOCAL day computed from the record
//! timestamps inside the files. The two deliberately need not agree — the file
//! name is a shard key, not a claim about anyone's calendar.
//!
//! # Closed days are gzipped
//!
//! A day of ledger is about 20 MB on a busy fleet and is kept for
//! `usageRetentionDays` (90 by default), so plain files head for gigabytes. The
//! writer thread gzips every day older than yesterday into `<date>.jsonl.gz`
//! ([`compress_closed_days`]), when it starts and each time a new day opens;
//! JSON lines this repetitive compress to a small fraction of their size. Today's
//! file stays plain because it is being appended to, and yesterday's for one
//! more day so a late line and boot's replay both meet a plain file.
//!
//! Every reader goes through [`read_ledger_day_checked`], which reads either form and
//! both at once, so nothing that reads the ledger needs to know which days are
//! compressed: boot's replay, `tcr wrap`, and the scripts that read these files
//! by hand (`scripts/fit-quota-weights.py`) read both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc::Receiver;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex, RwLock};

use tcr_status_wire::{UsageRow, UsageTotals, UsageWindow};
use time::{Date, OffsetDateTime, UtcOffset};

use crate::base::pricing::{PricingTable, cost_nanos};
use crate::base::provider::Provider;

mod day_files;
pub(crate) mod demand_history;
mod writer;

pub(crate) use day_files::{
    LedgerLine, date_string, filed_by_model_id, ledger_date, line_provider, parse_ledger_day,
};
use day_files::{history_day, ledger_record, parse_ledger_day_checked, prune};
pub use writer::LedgerShutdown;
use writer::{Ledger, LedgerHandle, LedgerMsg, ledger_writer};

/// Minutes held in the ring: six hours, so a 5-hour window plus the trailing
/// hour is always fully covered.
const RING_MINUTES: i64 = 360;
/// The 5-hour quota window, in milliseconds — the span `window` reports over.
pub const FIVE_HOURS_MS: i64 = 5 * 60 * 60 * 1000;
/// One week. The longest window the weekly accumulator answers for, and how far back a
/// fleet holding a Codex account replays and keeps its ledger.
pub const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// [`WEEK_MS`] in minutes, the accumulator's key unit.
const WEEK_MINUTES: i64 = WEEK_MS / 60_000;
/// The trailing span `last_hour` reports over.
pub const ONE_HOUR_MS: i64 = 60 * 60 * 1000;
/// Bucket key for a request whose body carried no parseable `model`.
const UNKNOWN_MODEL: &str = "unknown";
/// How many un-written ledger lines may be in flight before [`UsageTracker::record`]
/// starts dropping them. A few thousand lines is seconds of this fleet's peak
/// traffic — enough to ride out a slow `write`, small enough that a writer stuck
/// for good costs bounded memory.
const LEDGER_QUEUE_DEPTH: usize = 4096;
/// How long the ledger writer waits, once its queue is empty, before it blocks
/// on the next line (see `ledger_writer`). A line reaches the file up to this
/// much later; at the fleet's peak of a few dozen requests a second that is well
/// under one line queued, against [`LEDGER_QUEUE_DEPTH`].
const LEDGER_LINGER: std::time::Duration = std::time::Duration::from_millis(2);

/// The floor on what the writer JOIN gets in [`UsageTracker::shutdown_ledger`],
/// measured from the moment the channel closes.
///
/// The join cannot start earlier than that: the writer ends when the last sender
/// drops, and the last sender is the handle the flush is still using. So a
/// budget shared with the flush is a budget the join can be handed empty, which
/// is the guaranteed loss this floor exists to remove. Small enough to stay a
/// pause — draining a few thousand queued lines onto a working disk is
/// milliseconds, and a disk that cannot manage it in a quarter of a second is
/// the stall the bound exists for.
///
/// Lives here rather than in `server.rs` because it bounds a step of the
/// shutdown this module owns; `ServerHandle::shutdown_within` uses the same
/// constant as the floor on the whole ledger budget.
pub const MIN_LEDGER_SHUTDOWN: std::time::Duration = std::time::Duration::from_millis(250);

/// Latched the first time a caller's `quota_input` came in below its own cache
/// components, so the warning is one line per boot rather than one per request.
static QUOTA_INPUT_UNDERFLOWED: AtomicBool = AtomicBool::new(false);

/// One served request's usage, as recorded and as persisted.
///
/// `input` is BASE input only — cache creation and cache reads are separate
/// billing dimensions and are carried separately, because they bill at
/// different rates.
///
/// `quota_input` is the caller's own quota figure, held VERBATIM. The quota
/// counter the rest of the manager keeps (`AccountRuntime::input_tokens`) is
/// exactly this number added up, and [`Self::input_total`] hands it back
/// untouched — so routing a request through here cannot change what
/// `inputTokens` has meant for months, whatever relationship the caller's own
/// components happen to have to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub ts_ms: i64,
    /// `None` when the request body carried no `model` and, for a Codex turn, neither did its
    /// response, which is where tcr reads a zstd body's model: the stream's terminal event for a
    /// streamed turn, the Responses object's top-level `model` otherwise — buckets file it under
    /// `"unknown"` and it prices as unpriced, never as free.
    pub model: Option<String>,
    pub session: Option<u64>,
    /// The caller's quota input figure, verbatim: what `sum_input_tokens`
    /// produced in `proxy.rs`, or what an embedder passed to
    /// `Manager::update_usage`. NOT re-derived from the fields below.
    pub quota_input: u64,
    /// BASE input, for pricing only: `quota_input` less the cache components,
    /// floored at 0. Build it with [`Self::from_quota_input`] rather than by
    /// hand, so the floor and its warning live in one place.
    pub input: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub output: u64,
    /// Which provider served the request. `Manager::record_usage` stamps it from the serving
    /// account, and the boot replay from the account a line resolves to. Every record
    /// [`Self::from_quota_input`] builds starts as Anthropic.
    pub provider: Provider,
    /// A Codex turn's reasoning tokens, already inside `output`. 0 for every Anthropic turn.
    pub reasoning: u64,
}

impl UsageRecord {
    /// Build a record from the caller's verbatim quota figure and the cache
    /// components that are a SUBSET of it.
    ///
    /// The base input priced at the input rate is what is left after the cache
    /// dimensions are taken out. A caller whose components exceed its own
    /// `quota_input` is inconsistent — a `pub` entry point on a library crate
    /// can be handed anything — so the base is floored at 0 and said out loud
    /// once per boot. What must NOT happen is the old behaviour: silently
    /// re-summing the floored parts and handing the quota counter a bigger
    /// number than the caller passed.
    #[expect(
        clippy::too_many_arguments,
        reason = "one response's token counts, each a separate field of the ledger line"
    )]
    pub fn from_quota_input(
        ts_ms: i64,
        model: Option<String>,
        session: Option<u64>,
        quota_input: u64,
        cache_5m: u64,
        cache_1h: u64,
        cache_read: u64,
        output: u64,
    ) -> Self {
        let components = cache_5m.saturating_add(cache_1h).saturating_add(cache_read);
        let input = quota_input.checked_sub(components).unwrap_or_else(|| {
            if !QUOTA_INPUT_UNDERFLOWED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    quota_input,
                    cache_creation = cache_5m + cache_1h,
                    cache_read,
                    "a usage record's cache components exceed its own input total; \
                     pricing its base input as 0 (reported once per boot)"
                );
            }
            0
        });
        Self {
            ts_ms,
            model,
            session,
            quota_input,
            input,
            cache_5m,
            cache_1h,
            cache_read,
            output,
            provider: Provider::Anthropic,
            reasoning: 0,
        }
    }

    /// The quota counter's input figure — the caller's number, unchanged.
    pub const fn input_total(&self) -> u64 {
        self.quota_input
    }

    /// Total cache-creation tokens, both TTLs — the existing
    /// `cache_creation_tokens` counter's meaning, unchanged.
    pub const fn cache_creation(&self) -> u64 {
        self.cache_5m + self.cache_1h
    }

    fn model_key(&self) -> &str {
        self.model.as_deref().unwrap_or(UNKNOWN_MODEL)
    }
}

/// Accumulated usage for one bucket-and-model pair. Cost is kept in
/// nanodollars so a per-request rounding error cannot accumulate into the day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    requests: u64,
    input: u64,
    cache_5m: u64,
    cache_1h: u64,
    cache_read: u64,
    output: u64,
    cost_nanos: u64,
    unpriced_requests: u64,
    /// Reasoning tokens, a part of `output` that only a Codex turn reports. 0 in every
    /// bucket that holds no reasoning Codex turn.
    reasoning: u64,
}

impl Totals {
    const fn add(&mut self, other: &Self) {
        self.requests += other.requests;
        self.input += other.input;
        self.cache_5m += other.cache_5m;
        self.cache_1h += other.cache_1h;
        self.cache_read += other.cache_read;
        self.output += other.output;
        self.cost_nanos += other.cost_nanos;
        self.unpriced_requests += other.unpriced_requests;
        self.reasoning += other.reasoning;
    }

    /// Project onto the wire shape. `costUsd` is `None` when the bucket served
    /// requests and NOT ONE of them could be priced — publishing `0.0` there
    /// would be an unmeasured number wearing a measurement's clothes. A bucket
    /// with some priced and some unpriced requests reports the partial sum, and
    /// `unpricedRequests` is how a reader knows it is partial. A bucket with NO
    /// requests reports `0.0`: nothing served is a measured zero, and it is the
    /// same zero `lastHour` has always reported for the same idle account.
    ///
    /// `cacheCreationTokens` is the FULL cache-creation count, both TTLs — the
    /// same quantity the row-level field of that name carries, so one key never
    /// means two things in one row. `cacheCreation1hTokens` is the 1-hour
    /// SUBSET of it, and the 5-minute part is the difference.
    fn to_wire(self) -> UsageTotals {
        let cost_usd = if self.requests > 0 && self.unpriced_requests >= self.requests {
            None
        } else {
            Some(self.cost_nanos as f64 / 1e9)
        };
        UsageTotals {
            requests: self.requests,
            input_tokens: self.input,
            cache_creation_tokens: self.cache_5m + self.cache_1h,
            cache_creation_1h_tokens: self.cache_1h,
            cache_read_tokens: self.cache_read,
            output_tokens: self.output,
            cost_usd,
            unpriced_requests: self.unpriced_requests,
            reasoning_tokens: self.reasoning,
        }
    }
}

/// One minute of one account's traffic, split by model.
#[derive(Debug, Clone, Default)]
struct MinuteBucket {
    /// Which minute (`ts_ms / 60_000`) this slot currently holds. `None` when
    /// the slot has never been written.
    minute: Option<i64>,
    models: BTreeMap<String, Totals>,
}

/// One account's usage state.
#[derive(Debug)]
struct AccountUsage {
    ring: Vec<MinuteBucket>,
    /// The local calendar day `today` holds, or `None` before the first record.
    day: Option<Date>,
    today: BTreeMap<String, Totals>,
    /// A Codex account's per-minute totals over the last week, keyed by minute (`ms / 60_000`):
    /// the source of its weekly window, which reaches back past the ring's six hours and past
    /// midnight. Only Codex records land here, and it never holds more than a week.
    week: BTreeMap<i64, Totals>,
}

impl Default for AccountUsage {
    fn default() -> Self {
        Self {
            ring: vec![MinuteBucket::default(); RING_MINUTES as usize],
            day: None,
            today: BTreeMap::new(),
            week: BTreeMap::new(),
        }
    }
}

/// The account at `idx`, growing the vector to reach it.
///
/// The rotation GROWS at runtime (`tcr login`, TcrBar's add-account, the
/// `POST /_tcr/accounts` path), so a vector sized once at boot silently discards
/// a live-added account's whole day — a zero that reads as measured, which is
/// the one failure this module exists to prevent. Grow on demand instead: an
/// index is a position in the live rotation, and this structure follows it.
///
/// One function rather than one copy per caller, so recording and reading a row
/// cannot drift apart on what an out-of-range index means.
///
/// Runs `f` under that account's own lock, with the list's read lock held, so
/// a request on another account neither waits for it nor is waited on. The
/// list's write lock is taken only when `idx` is past its end.
fn with_account<R>(
    accounts: &RwLock<Vec<Mutex<AccountUsage>>>,
    idx: usize,
    f: impl FnOnce(&mut AccountUsage) -> R,
) -> R {
    {
        let list = accounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(account) = list.get(idx) {
            return f(&mut account
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner));
        }
    }
    let mut list = accounts
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Another caller may have grown the list between the read lock and this one.
    if let Some(slot) = list.get_mut(idx) {
        return f(slot
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
    }
    // `idx` is past the end: the account's usage starts empty, `f` records into it, and it
    // is stored at `idx`, with an empty row for every index between the old end and it.
    let mut usage = AccountUsage::default();
    let result = f(&mut usage);
    list.resize_with(idx, Mutex::default);
    list.push(Mutex::new(usage));
    result
}

fn add_into(map: &mut BTreeMap<String, Totals>, model: &str, totals: &Totals) {
    map.entry(model.to_string()).or_default().add(totals);
}

fn sum(map: &BTreeMap<String, Totals>) -> Totals {
    let mut out = Totals::default();
    for t in map.values() {
        out.add(t);
    }
    out
}

/// The local calendar day for a Unix-millisecond instant, on the machine this
/// process runs on.
///
/// `time` 0.3.55 resolves the offset with `localtime_r` for the instant given
/// (`src/sys/local_offset_at/unix.rs`), so this is DST-correct per record and
/// there is no boot-captured offset to go stale. It still needs the crate's
/// `local-offset` feature; without it, and on any platform where the lookup
/// fails, the offset is `Err` and this falls back to UTC — see
/// [`warn_if_local_offset_unavailable`], which says so once at boot rather than
/// letting every day boundary silently mean something else.
pub fn local_day(ts_ms: i64) -> Date {
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ts_ms) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let offset = UtcOffset::local_offset_at(at).unwrap_or(UtcOffset::UTC);
    at.to_offset(offset).date()
}

/// Log once, at boot, if the local UTC offset cannot be read — after which
/// "today" is a UTC day and the operator knows why.
pub fn warn_if_local_offset_unavailable() {
    if UtcOffset::local_offset_at(OffsetDateTime::now_utc()).is_err() {
        tracing::warn!(
            "could not read this machine's UTC offset; usage day boundaries will follow UTC"
        );
    }
}

/// `usage/` in the cache directory ([`crate::base::paths::cache_dir`]:
/// `~/.cache/tcr/`) — a cache dir for the same reason the pin file is one:
/// regenerable-ish state written at high frequency has no business next to
/// live OAuth credentials.
pub fn default_dir() -> PathBuf {
    crate::base::paths::cache_dir().usage_dir()
}

/// What a boot-time replay found. Reported so the operator can tell "the ledger
/// restored the day" apart from "the ledger was empty and today starts at zero".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// Lines replayed into the buckets.
    pub replayed: usize,
    /// Lines skipped because no configured account carries that name.
    pub unresolved: usize,
    /// Lines that were not parseable JSON of the expected shape.
    pub malformed: usize,
    /// Day files deleted for being older than the retention window.
    pub pruned: usize,
}

/// Why [`UsageTracker::rebuild_from_ledger`] left the totals as they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildSkipped {
    /// No ledger directory was ever attached, so there is nothing to read.
    NoLedger,
    /// The writer is failing or has been detached, so lines of ours may be
    /// missing from the files.
    NotPersisting,
    /// This many lines found the queue full and were never written.
    LinesDropped(u64),
    /// The writer did not confirm the flush inside [`REBUILD_FLUSH_BUDGET`].
    FlushTimedOut,
}

/// How long [`UsageTracker::rebuild_from_ledger`] waits for this process's own
/// queued lines to reach the file. Requests wait on the rebuild for at most
/// this plus the replay, so it is the request path's bound too.
pub const REBUILD_FLUSH_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether any of these accounts is a Codex account, whose weekly window makes
/// a replay read the week and keeps its files from being pruned.
fn holds_codex(accounts: &[LedgerAccount]) -> bool {
    accounts.iter().any(|a| match a.provider {
        Provider::Anthropic => false,
        Provider::Codex => true,
    })
}

/// A live account as the ledger knows it: the display name it is recorded
/// under, plus the identity fields that make that name resolvable. Built from
/// the rotation by `Manager::attach_usage_ledger`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerAccount {
    pub name: String,
    /// `account_uuid`, when this account has one — the half of the identity
    /// that does not move under a rename or an org backfill.
    pub uuid: Option<String>,
    /// [`crate::base::identity::org_key`] for this account: org UUID, else org name,
    /// else `None`.
    pub org: Option<String>,
    /// The org NAME, kept alongside the key because a line written before this
    /// account's `org_uuid` was backfilled carries the name where a line
    /// written after carries the uuid — and both name the same org. `None` when
    /// the account has no org name, and equal to `org` when it has no org uuid.
    pub org_name: Option<String>,
    /// The account's provider. It decides how a line that resolves to this account is
    /// counted (a Codex record also fills the weekly window), and whether boot must replay
    /// the week: a fleet holding a Codex account does.
    pub provider: Provider,
}

/// Per-account usage buckets, the pricing table, and the ledger.
///
/// One of these hangs off `Manager`. Recording takes the serving account's own
/// buckets lock for an O(1) update; a snapshot takes the same lock for a read.
/// Neither is ever held across the manager's accounts lock —
/// `Manager::record_usage` updates the counters, releases that lock, and only
/// then records here.
///
/// The locks are per account, behind a read lock on the list, because one lock
/// over every account's buckets made every served request wait on every other
/// one (2026-10-16: the busiest lock left after the log file's). Two requests
/// now wait on each other only when one account served both. The buckets are
/// not plain counters: each is a minute ring, a day and a week split by model,
/// rolled over by the record's own time, so a record stays one locked update of
/// one account rather than a set of atomics that could be read half-applied.
/// The list's write lock is taken only to grow it for an account added at
/// runtime. The ledger handle is behind a read-write lock for the same reason:
/// every record reads it, and only attaching and shutting down write it.
#[derive(Debug)]
pub struct UsageTracker {
    accounts: RwLock<Vec<Mutex<AccountUsage>>>,
    pricing: PricingTable,
    ledger: RwLock<Option<LedgerHandle>>,
    /// The writer thread, kept so shutdown can JOIN it. Dropping this handle
    /// instead detaches the thread, and a detached writer holding an open file
    /// is exactly how a queued line goes missing at exit.
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Whether a ledger is attached at all, readable without taking the lock.
    attached: AtomicBool,
    /// Whether the writer's last write SUCCEEDED. Held here rather than by the
    /// writer thread so `is_persisting` can read it without a lock, and so it
    /// outlives a writer that ends; the writer thread is still its only WRITER.
    healthy: Arc<AtomicBool>,
    /// Whether lines are currently being lost to a full queue — see the module
    /// docs on flag ownership.
    dropping: Arc<AtomicBool>,
    /// Lines the queue had no room for, over the life of the process.
    dropped: Arc<AtomicU64>,
    /// The directory [`Self::attach_ledger`] was given, kept for
    /// [`Self::rebuild_from_ledger`], which reads the same files again.
    dir: Mutex<Option<PathBuf>>,
    /// Makes one [`Self::record`] (its ledger line AND its buckets) a single
    /// step as far as [`Self::rebuild_from_ledger`] can see. A record holds it
    /// shared, so records never wait on each other here; a rebuild holds it
    /// exclusively, so no record is half done while it flushes and replays.
    rebuild_gate: RwLock<()>,
    forecast_history: Mutex<demand_history::History>,
}

impl UsageTracker {
    pub fn new(account_count: usize, pricing: PricingTable) -> Self {
        let mut accounts = Vec::with_capacity(account_count);
        accounts.resize_with(account_count, Mutex::default);
        Self {
            accounts: RwLock::new(accounts),
            pricing,
            ledger: RwLock::new(None),
            writer: Mutex::new(None),
            attached: AtomicBool::new(false),
            healthy: Arc::new(AtomicBool::new(true)),
            dropping: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            dir: Mutex::new(None),
            rebuild_gate: RwLock::new(()),
            forecast_history: Mutex::new(demand_history::History::default()),
        }
    }

    /// The price for `model`, or `None` when the pricing table has no entry — see
    /// [`PricingTable::lookup`]. The one entry point for pricing a wire-session's per-model
    /// token tally (`src/manager/wire_sessions.rs`) with the SAME table (config overrides
    /// included) the account-level ledger prices with, so the two never disagree about what
    /// a model costs.
    pub(crate) fn price_for(&self, model: &str) -> Option<crate::base::pricing::ModelPrice> {
        self.pricing.lookup(model)
    }

    /// Whether this tracker is writing a ledger — i.e. whether today's totals
    /// will survive a restart.
    ///
    /// FALSE while the ledger is not keeping up, not just when none was
    /// attached: a ledger that stopped writing at 10:00, one whose queue is
    /// full, and one whose writer thread has gone all answer this question
    /// exactly the same way as one that was never there, because the answer to
    /// "will today's totals survive" is no in every one of them.
    pub fn is_persisting(&self) -> bool {
        self.attached.load(Ordering::Relaxed)
            && self.healthy.load(Ordering::Relaxed)
            && !self.dropping.load(Ordering::Relaxed)
    }

    /// Whether a ledger is attached at all — i.e. whether a caller's per-request
    /// work to build a line has a reader. Deliberately NOT
    /// [`Self::is_persisting`]: a ledger whose writes are currently failing is
    /// still attached, still queueing, and a line queued now is still written
    /// when it recovers, so it must arrive with everything it needs to resolve.
    pub fn is_attached(&self) -> bool {
        self.attached.load(Ordering::Relaxed)
    }

    /// Ledger lines dropped because the writer's queue was full — always 0 on a
    /// healthy fleet, and the number that says how much of the day a stalled
    /// volume cost.
    pub fn dropped_lines(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Block until every line queued so far has been written, or five seconds
    /// pass. For shutdown and for tests; the request path never calls it.
    /// Returns false if no ledger is attached or the writer did not answer
    /// within the bound.
    pub fn flush_ledger(&self) -> bool {
        self.flush_ledger_within(std::time::Duration::from_secs(5))
    }

    /// [`Self::flush_ledger`] with the bound spelled out.
    ///
    /// The SENDER — not the handle — is cloned out from under the ledger mutex
    /// and the guard dropped before anything is sent. Holding the guard across
    /// the send would put every concurrent [`Self::record`] behind this call,
    /// and the case where this call waits is the stalled writer, which is
    /// exactly the case where the request path must not wait for anything.
    /// Cloning the HANDLE instead would leave a second sender alive for the
    /// length of the flush, which is what [`Self::shutdown_ledger`]'s drop must
    /// be the last of.
    pub fn flush_ledger_within(&self, budget: std::time::Duration) -> bool {
        let tx = self
            .ledger
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|handle| handle.tx.clone());
        let deadline = std::time::Instant::now() + budget;
        tx.is_some_and(|tx| LedgerHandle::flush_via(tx, deadline))
    }

    /// Flush the queue and JOIN the writer, so the day file holds every line
    /// this process queued and is closed before it exits.
    ///
    /// Detaches the ledger first, so a request that lands mid-shutdown is
    /// counted in memory and neither queued nor waited on, and
    /// [`Self::is_persisting`] answers false from that instant.
    ///
    /// `budget` bounds the FLUSH. The join is bounded separately, by whatever is
    /// left of `budget` but never by less than [`MIN_LEDGER_SHUTDOWN`], because
    /// the join is the first moment the writer can end at all: the channel
    /// closes when the handle below drops, which happens after the flush has
    /// returned. Sharing one deadline handed the join zero milliseconds every
    /// time the flush spent the budget — a stalled volume, the case all of this
    /// is for — and abandoned a writer that had never been asked to stop.
    ///
    /// A writer that has not drained when the join budget runs out is ABANDONED,
    /// not waited on: `tcr` quits into a stalled volume every bit as often as
    /// into a healthy one, and a shutdown that hangs is worse than a ledger that
    /// loses its tail — which the returned [`LedgerShutdown`] says out loud. It
    /// never says it about a writer that finished: a writer ends only after it
    /// has taken every message queued before the channel closed, which is the
    /// same fact a flush acknowledgement reports and strictly more of it.
    pub fn shutdown_ledger(&self, budget: std::time::Duration) -> LedgerShutdown {
        let flush_deadline = std::time::Instant::now() + budget;
        self.attached.store(false, Ordering::Relaxed);
        let Some(handle) = self
            .ledger
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return LedgerShutdown::NotAttached;
        };
        let flushed = handle.flush_within(flush_deadline);
        // The writer ends when the last sender goes, so this drop is what asks
        // it to stop — after the flush, never before.
        drop(handle);
        let join_deadline = flush_deadline.max(std::time::Instant::now() + MIN_LEDGER_SHUTDOWN);
        // One mapping, at one exit: a second copy of "which bool means which
        // outcome" is how a shutdown report comes to disagree with itself
        // depending on the path it took.
        let drained = match self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            // No writer thread to join (it never started, or a previous
            // shutdown already took it), so the flush is all the evidence there
            // is.
            None => flushed,
            Some(writer) => {
                while !writer.is_finished() {
                    if std::time::Instant::now() >= join_deadline {
                        return LedgerShutdown::Abandoned;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                if writer.join().is_err() {
                    tracing::warn!(
                        "the usage-ledger writer thread panicked; its last lines are gone"
                    );
                    return LedgerShutdown::Abandoned;
                }
                true
            }
        };
        if drained {
            LedgerShutdown::Flushed
        } else {
            LedgerShutdown::Abandoned
        }
    }

    /// Wedge the writer thread: hand it a flush it cannot answer, and return the
    /// receiver that holds it there. Dropping the receiver frees it — a
    /// rendezvous `send` fails the moment its receiver goes.
    ///
    /// Test-only, and the one way to build the stalled-volume case without a
    /// stalled volume. `pub(crate)` because the shutdown it stalls is
    /// `ServerHandle::shutdown_within`'s, which lives in another module.
    #[cfg(test)]
    pub(crate) fn wedge_writer_for_test(&self) -> Receiver<()> {
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        self.ledger
            .read()
            .expect("ledger lock poisoned")
            .as_ref()
            .expect("a ledger is attached")
            .tx
            .send(LedgerMsg::Flush(ack_tx))
            .expect("the writer is alive");
        // No wait is needed for the wedge to bite: the channel is FIFO, so
        // everything queued after this marker stays queued until the writer gets
        // past it, whether or not it has reached it yet.
        ack_rx
    }

    /// Price one record and fold it into the ring and the day accumulator.
    /// `now`-independent: everything is decided from `record.ts_ms`, which is
    /// what lets replay run the very same path.
    fn record_in_memory(&self, idx: usize, record: &UsageRecord) {
        let price = record
            .model
            .as_deref()
            .and_then(|model| self.pricing.lookup(model));
        let mut totals = Totals {
            requests: 1,
            input: record.input,
            cache_5m: record.cache_5m,
            cache_1h: record.cache_1h,
            cache_read: record.cache_read,
            output: record.output,
            cost_nanos: 0,
            unpriced_requests: 0,
            reasoning: record.reasoning,
        };
        match price {
            Some(price) => {
                totals.cost_nanos = cost_nanos(
                    &price,
                    record.input,
                    record.cache_5m,
                    record.cache_1h,
                    record.cache_read,
                    record.output,
                );
            }
            None => totals.unpriced_requests = 1,
        }

        let model = record.model_key().to_string();
        let minute = record.ts_ms.div_euclid(60_000);
        let day = local_day(record.ts_ms);

        with_account(&self.accounts, idx, |account| {
            // The ring. A slot holding a NEWER minute than this record's is not
            // ours to clear — that happens when a replayed line is older than the
            // six hours the ring covers, or when two records land out of order.
            // Dropping the record from the ring is right there: it is genuinely
            // outside every window the ring answers for. The day accumulator below
            // still takes it.
            let slot = minute.rem_euclid(RING_MINUTES) as usize;
            if let Some(bucket) = account.ring.get_mut(slot) {
                match bucket.minute {
                    Some(held) if held > minute => {}
                    Some(held) if held == minute => add_into(&mut bucket.models, &model, &totals),
                    _ => {
                        bucket.minute = Some(minute);
                        bucket.models.clear();
                        add_into(&mut bucket.models, &model, &totals);
                    }
                }
            }

            // The day. A record for a LATER day than the one held rolls the
            // accumulator over; a record for an EARLIER day (replaying yesterday's
            // file after today's, or a clock step) is not today's traffic and must
            // not be added to it.
            match account.day {
                Some(held) if held == day => add_into(&mut account.today, &model, &totals),
                Some(held) if held > day => {}
                _ => {
                    account.day = Some(day);
                    account.today.clear();
                    add_into(&mut account.today, &model, &totals);
                }
            }

            // The week, for a Codex record only. Pruned here, against the newest minute held, so it
            // never holds more than a week.
            match record.provider {
                Provider::Anthropic => {}
                Provider::Codex => {
                    account.week.entry(minute).or_default().add(&totals);
                    let newest = account
                        .week
                        .last_key_value()
                        .map_or(minute, |(&held, _)| held);
                    while let Some(oldest) = account.week.first_entry() {
                        if *oldest.key() >= newest - WEEK_MINUTES {
                            break;
                        }
                        oldest.remove();
                    }
                }
            }
        });
    }

    /// Record one served request: queue the ledger line, update the buckets.
    ///
    /// Called from the proxy's request path, so it does NO file I/O — see the
    /// module docs. The line goes on a bounded queue the writer thread owns; a
    /// full queue drops it and counts the drop rather than making a client wait
    /// on a disk.
    ///
    /// `account_uuid` and `account_org` are the serving account's identity
    /// ([`crate::base::identity`]), and they are what make the line resolvable back to
    /// THIS account on replay: the uuid survives a rename and an org backfill,
    /// and the org tells two orgs of the same person apart. A name alone does
    /// neither.
    pub fn record(
        &self,
        idx: usize,
        record: &UsageRecord,
        account_name: &str,
        account_uuid: Option<&str>,
        account_org: Option<&str>,
    ) {
        // Held to the end: the line and the buckets below are one step to a
        // rebuild, so a line it reads from disk is never added in memory again.
        let _gate = self
            .rebuild_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.attached.load(Ordering::Relaxed) {
            let line = LedgerLine {
                t: record.ts_ms,
                a: account_name.to_string(),
                u: account_uuid.map(str::to_string),
                g: account_org.map(str::to_string),
                p: (!record.provider.is_anthropic()).then(|| record.provider.as_str().to_string()),
                m: record.model.clone(),
                s: record.session,
                i: record.input,
                c5: record.cache_5m,
                c1: record.cache_1h,
                r: record.cache_read,
                o: record.output,
                rt: record.reasoning,
            };
            if let Some(ledger) = self
                .ledger
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                ledger.queue(line);
            }
        }
        self.record_in_memory(idx, record);
        self.forecast_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(record);
    }

    pub(crate) fn forecast_estimates(&self, now_ms: i64) -> Vec<demand_history::ProviderEstimate> {
        let mut history = self
            .forecast_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Sticky until the next boot: a dropped line or a failed write is missing from
        // the history for good, and a handoff rebuild no longer replaces the history.
        if self.dropped_lines() != 0
            || (self.is_attached() && !self.healthy.load(Ordering::Relaxed))
        {
            history.mark_partial();
        }
        history.estimates(now_ms)
    }

    /// Attach `dir` as this tracker's ledger: replay today's and yesterday's UTC
    /// files back through the recording path, plus the six before them when the
    /// fleet holds a Codex account, whose weekly window reaches back that far;
    /// Forecast history additionally reads at most 29 retained UTC files through
    /// the same checked decoder, independently of account resolution and totals.
    /// Then prune files older than `retention_days`, never below what was
    /// replayed; then start appending.
    ///
    /// Replay happens BEFORE the writer is armed, so a replayed line is never
    /// written back into the file it came from.
    pub fn attach_ledger(
        &self,
        dir: PathBuf,
        retention_days: u32,
        accounts: &[LedgerAccount],
        now_ms: i64,
    ) -> ReplayReport {
        let _gate = self
            .rebuild_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut report = ReplayReport::default();
        let today = ledger_date(now_ms);
        // A fleet holding a Codex account replays the eight UTC files its lines
        // can be in and never prunes one of them. A Claude-only fleet replays
        // yesterday and today and prunes as it always has.
        let holds_codex = holds_codex(accounts);
        // The one time the 29-day forecast history is read. A handoff rebuild never
        // re-reads it: `record` has fed the history ever since.
        let mut history = demand_history::History::new(now_ms);
        self.replay_window(&dir, accounts, now_ms, &mut report, Some(&mut history));
        *self
            .forecast_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = history;
        *self
            .dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dir.clone());
        let keep_days = if holds_codex {
            retention_days.max(7)
        } else {
            retention_days
        };
        report.pruned = prune(&dir, today, keep_days);
        // Replay ran synchronously, above and before this point, so a replayed
        // line can never be written back into the file it came from — and it
        // runs before the listener binds, which is why the one blocking read
        // here is not on anybody's request path.
        let (tx, rx) = sync_channel::<LedgerMsg>(LEDGER_QUEUE_DEPTH);
        let ledger = Ledger {
            dir,
            open: None,
            healthy: Arc::clone(&self.healthy),
            dropping: Arc::clone(&self.dropping),
            dropped: Arc::clone(&self.dropped),
            drops_seen: self.dropped.load(Ordering::Relaxed),
        };
        self.healthy.store(true, Ordering::Relaxed);
        self.dropping.store(false, Ordering::Relaxed);
        match std::thread::Builder::new()
            .name("tcr-usage-ledger".to_string())
            .spawn(move || ledger_writer(rx, ledger))
        {
            // The thread handle is KEPT, not dropped: `shutdown_ledger` joins it
            // so the file is closed before the process exits.
            Ok(handle) => {
                *self
                    .writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
                *self
                    .ledger
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LedgerHandle {
                    tx,
                    dropped: Arc::clone(&self.dropped),
                    dropping: Arc::clone(&self.dropping),
                });
                self.attached.store(true, Ordering::Relaxed);
            }
            // A process that cannot spawn a thread keeps serving with no
            // ledger, and says so rather than pretending: `is_persisting` stays
            // false, which is the truth.
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "could not start the usage-ledger writer thread; today's totals will not \
                     survive a restart"
                );
            }
        }
        report
    }

    /// Replace the in-memory totals with a fresh replay of the ledger files.
    /// For a successor whose predecessor has just finished writing to them.
    ///
    /// After a listener handoff both processes append to the same day file,
    /// and this one replayed it at boot while the predecessor was still
    /// draining, so every line the predecessor wrote after that replay is on
    /// disk and missing here. A line names no writer, so the two processes'
    /// lines cannot be told apart. They need not be: once the predecessor is
    /// done the files hold every line of both exactly once, and replaying all
    /// of them is the count.
    ///
    /// This process's own lines must be on disk first, so the queue is flushed
    /// under the gate every [`Self::record`] holds: no record sits between
    /// queueing its line and adding it to the buckets while the flush and the
    /// replay run, and one that arrives meanwhile waits and lands on the
    /// rebuilt totals. Requests wait for one flush and one replay of the
    /// window's files, once per handoff. The replay fills a separate tracker
    /// and is swapped in whole, so a reader sees the old totals or the new
    /// ones, never a half-built day.
    ///
    /// Leaves the totals alone when the files cannot be trusted to hold this
    /// process's lines; the error says which. They then keep missing the
    /// predecessor's tail, which is the smaller loss than replacing them with
    /// a file that lacks lines of ours.
    pub fn rebuild_from_ledger(
        &self,
        accounts: &[LedgerAccount],
        now_ms: i64,
    ) -> Result<ReplayReport, RebuildSkipped> {
        let dir = self
            .dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(RebuildSkipped::NoLedger)?;
        let _gate = self
            .rebuild_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let trusted = |tracker: &Self| {
            let dropped = tracker.dropped_lines();
            if dropped > 0 {
                Err(RebuildSkipped::LinesDropped(dropped))
            } else if tracker.is_persisting() {
                Ok(())
            } else {
                Err(RebuildSkipped::NotPersisting)
            }
        };
        trusted(self)?;
        if !self.flush_ledger_within(REBUILD_FLUSH_BUDGET) {
            return Err(RebuildSkipped::FlushTimedOut);
        }
        // A write that failed during the flush lost a line just as surely.
        trusted(self)?;
        let fresh = Self::new(accounts.len(), self.pricing.clone());
        let mut report = ReplayReport::default();
        fresh.replay_window(&dir, accounts, now_ms, &mut report, None);
        let mut rebuilt = fresh
            .accounts
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut live = self
            .accounts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rebuilt.len() < live.len() {
            rebuilt.resize_with(live.len(), Mutex::default);
        }
        *live = rebuilt;
        // The forecast history is left as it is. It was read once at boot and fed by
        // `record` since, so re-reading 29 days here would hold every request behind
        // the gate for the length of that read. The cost: lines the predecessor wrote
        // after this process's boot replay are in the totals above but not in it.
        Ok(report)
    }

    /// Replay the files a boot replays into this tracker's buckets: today's and
    /// yesterday's UTC files, and when the fleet holds a Codex account, whose
    /// weekly window reaches back seven days, the six before them, oldest first.
    ///
    /// With `history`, the same pass also builds the forecast's demand history from
    /// the 29 UTC files it covers, reading each file once. Only the boot replay
    /// passes one: a handoff rebuild leaves the history `record` has kept since boot.
    fn replay_window(
        &self,
        dir: &Path,
        accounts: &[LedgerAccount],
        now_ms: i64,
        report: &mut ReplayReport,
        mut history: Option<&mut demand_history::History>,
    ) {
        let days_back: i64 = if holds_codex(accounts) { 7 } else { 1 };
        let oldest_back = if history.is_some() { 28 } else { days_back };
        for back in (0..=oldest_back).rev() {
            let date = ledger_date(now_ms - back * 86_400_000);
            let day = parse_ledger_day_checked(dir, date);
            if let Some(history) = history.as_deref_mut() {
                history_day(history, &day, date, now_ms);
            }
            if back <= days_back {
                report.malformed += day.malformed;
                self.replay_day(day.lines, accounts, report);
            }
        }
    }

    fn replay_day(
        &self,
        lines: Vec<LedgerLine>,
        accounts: &[LedgerAccount],
        report: &mut ReplayReport,
    ) {
        for line in lines {
            let Some(idx) = resolve_account(accounts, &line) else {
                report.unresolved += 1;
                continue;
            };
            let provider = accounts
                .get(idx)
                .map_or(Provider::Anthropic, |a| a.provider);
            let Some(record) = ledger_record(&line, provider) else {
                report.malformed += 1;
                continue;
            };
            self.record_in_memory(idx, &record);
            report.replayed += 1;
        }
    }

    /// This account's usage as of `now_ms`. `five_hour_reset_ms` is the
    /// account's own 5-hour window reset, as its provider reported it on a
    /// response's rate-limit headers or to the quota probe: Anthropic's for a
    /// Claude account, and Codex's own for a Codex one. `None` there means the
    /// window's start cannot be named, so `window` is `null` rather than a
    /// guessed span. The row never names the weekly window:
    /// [`Self::seven_day_window`] answers for that, and only Codex records reach it.
    pub fn row(&self, idx: usize, now_ms: i64, five_hour_reset_ms: Option<i64>) -> UsageRow {
        let day = local_day(now_ms);
        with_account(&self.accounts, idx, |account| {
            // A day accumulator holding some OTHER day is not today's traffic. It
            // reads as an empty day, which is the truth after midnight on an idle
            // proxy — not as yesterday's total wearing today's label.
            let today_current = account.day == Some(day);
            let today_by_model: BTreeMap<String, UsageTotals> = if today_current {
                account
                    .today
                    .iter()
                    .map(|(model, t)| (model.clone(), t.to_wire()))
                    .collect()
            } else {
                BTreeMap::new()
            };
            // An empty day goes through `to_wire` like every other bucket, so it
            // reports `costUsd: 0.0` — the same measured zero `lastHour` reports
            // for the same idle account. `UsageTotals::default()` would say `null`,
            // and `null` means "could not be priced", not "served nothing".
            let today = if today_current {
                sum(&account.today).to_wire()
            } else {
                Totals::default().to_wire()
            };
            let window = five_hour_reset_ms.map(|reset| {
                let since = reset - FIVE_HOURS_MS;
                UsageWindow {
                    since,
                    totals: range(account, since, now_ms).to_wire(),
                }
            });
            UsageRow {
                today,
                window,
                last_hour: range(account, now_ms - ONE_HOUR_MS, now_ms).to_wire(),
                today_by_model,
                seven_day_window: None,
            }
        })
    }

    /// What account `idx` spent in a weekly window that opened `length_ms` before `reset_ms`:
    /// the weekly accumulator summed from the window's opening minute to `now_ms`'s, the same
    /// minute granularity `window` uses. `None` when the window opened more than [`WEEK_MS`]
    /// before `now_ms`, further back than the accumulator holds, so a partial sum is never
    /// reported as the whole window. Only Codex records reach the accumulator.
    pub fn seven_day_window(
        &self,
        idx: usize,
        now_ms: i64,
        reset_ms: i64,
        length_ms: i64,
    ) -> Option<UsageWindow> {
        let since = reset_ms - length_ms;
        if since < now_ms - WEEK_MS {
            return None;
        }
        let (from, to) = (since.div_euclid(60_000), now_ms.div_euclid(60_000));
        let mut totals = Totals::default();
        // A window that has not opened yet sums nothing, and `range` must not see from > to.
        if from <= to {
            with_account(&self.accounts, idx, |account| {
                for (_, minute_totals) in account.week.range(from..=to) {
                    totals.add(minute_totals);
                }
            });
        }
        Some(UsageWindow {
            since,
            totals: totals.to_wire(),
        })
    }
}

/// Resolve one ledger line to a live account index — the account that WROTE it,
/// or nobody. Never a neighbour: every step below insists on a unique match, and
/// an ambiguous or absent one is [`ReplayReport::unresolved`], counted and said
/// out loud at boot.
///
/// Three keys, strongest first.
///
/// 1. **The account UUID** (`u`), when the line carries one. It is the only part
///    of the identity that survives the routine edits: a re-login backfills a
///    missing `org_uuid` (`Manager::add_or_update_account`), and from that
///    moment [`crate::base::identity::org_key`] returns the uuid where it used to
///    return the org NAME — so a line keyed on the org alone stops matching its
///    own account half way through the day, and the morning replays as a zero
///    that reads as measured. A uuid that matches NOTHING is unresolved even
///    when the name is unique: a removed account's uuid stays gone, and the
///    survivor wearing its name did not serve that traffic. A uuid is never the
///    whole answer for a line that also carries an org, because one person in
///    several orgs is several accounts (`identity.rs`): the org must agree too,
///    or removing one of them would replay its day onto its sibling — the same
///    misattribution the org key was added to prevent, one key up.
/// 2. **(name, org)**, for a line with no uuid: an API-key account, or one
///    written before this field existed.
/// 3. **The name alone**, and only for a line carrying neither key, and only
///    while exactly one live account wears that name. With two, a name match is
///    a coin flip, and a coin flip that lands wrong doubles one account's day
///    and zeroes the other's.
///
/// A line's org is matched against the account's org KEY *or* its org NAME,
/// because those are the same org written two ways — the uuid after a backfill,
/// the name before it — and a fleet where two different orgs answer to one of
/// those strings is ambiguous rather than resolvable, which the uniqueness check
/// already handles.
fn resolve_account(accounts: &[LedgerAccount], line: &LedgerLine) -> Option<usize> {
    if let Some(uuid) = line.u.as_deref() {
        let same_person = accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.uuid.as_deref() == Some(uuid));
        return match line.g.as_deref() {
            Some(org) => only(same_person.filter(|(_, a)| names_org(a, Some(org)))),
            None => only(same_person),
        };
    }
    if line.g.is_some() {
        return only(
            accounts
                .iter()
                .enumerate()
                .filter(|(_, a)| a.name == line.a && names_org(a, line.g.as_deref())),
        );
    }
    only(
        accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.name == line.a),
    )
}

/// Whether `org` names THIS account's organization — as its key (uuid, else
/// name) or as its name, which is the same org written the way it was written
/// before the uuid was known.
fn names_org(account: &LedgerAccount, org: Option<&str>) -> bool {
    org.is_some_and(|org| {
        account.org.as_deref() == Some(org) || account.org_name.as_deref() == Some(org)
    })
}

/// The index of the ONE candidate, or `None` for none and for several. Every
/// resolution step in this module ends this way, so "ambiguous is unresolved"
/// is stated once rather than re-derived per key.
fn only<'a, I: Iterator<Item = (usize, &'a LedgerAccount)>>(mut candidates: I) -> Option<usize> {
    match (candidates.next(), candidates.next()) {
        (Some((idx, _)), None) => Some(idx),
        _ => None,
    }
}

/// Sum the ring over `[since_ms, now_ms]`, at minute granularity, ignoring any
/// slot that has fallen out of the six hours the ring covers.
fn range(account: &AccountUsage, since_ms: i64, now_ms: i64) -> Totals {
    let from = since_ms.div_euclid(60_000);
    let to = now_ms.div_euclid(60_000);
    let floor = to - RING_MINUTES + 1;
    let mut out = Totals::default();
    for bucket in &account.ring {
        let Some(minute) = bucket.minute else {
            continue;
        };
        if minute >= from && minute <= to && minute >= floor {
            out.add(&sum(&bucket.models));
        }
    }
    out
}

#[cfg(test)]
mod tests;
