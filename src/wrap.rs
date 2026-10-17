//! `tcr wrap` — a weekly (or N-day) usage report, read straight off the usage
//! ledger `src/usage.rs` writes.
//!
//! # Why this reads the ledger and not `tcr status`
//!
//! `tcr status` answers "what is happening right now" from the live process's
//! in-memory buckets (today, the last hour, the 5-hour window). This answers
//! "what happened this week", which is a question about history the running
//! process does not keep — the ring only covers six hours and `today` rolls
//! over at midnight. The ledger's day files are the only durable record, so
//! this reads them directly, offline, whether or not a proxy is running.
//!
//! # Reuse, not a second parser
//! [`crate::usage::parse_ledger_day`] and the `LedgerLine` shape are shared
//! with [`crate::usage::UsageTracker`]'s own boot-time replay — this module
//! never re-implements the line format. Pricing is [`crate::pricing`], the
//! exact table and formula the live proxy uses.
//!
//! # Grouping by account, not by identity
//!
//! `tcr status` resolves a ledger line back to a *configured* account
//! (`resolve_account`), because it must attribute a line to a specific,
//! currently-live rotation slot. This report has no such need — it just adds
//! up what each stored name did — so it groups on `LedgerLine::a` verbatim,
//! with no config or account-identity resolution at all. That also means it
//! runs with no config file present, and reports an account that has since
//! been removed from the fleet.
//!
//! # Days are UTC calendar days
//!
//! One file per UTC day is what is on disk, so "day" here means the UTC day a
//! record's own ledger file is named after ([`crate::usage::ledger_date`]),
//! not the local calendar day `tcr status`'s `today` uses. That is also what
//! makes every number in this report reproducible with one `jq` command over
//! the files directly — see `docs/cli.md`.
//!
//! # Codex lines
//!
//! A line's provider is its `p`, or, for a line written before `p` existed, its
//! model id's ([`crate::usage::line_provider`]). A Codex bucket no price covers
//! prints `n/a` with its unpriced count, never `$0.00`: no OpenAI model has a
//! built-in price ([`crate::pricing`]), so a zero there would be a figure
//! nobody measured. The `by provider` block, the second totals line, the
//! unpriced annotations and `(priced requests only)` print only when the period
//! holds a Codex line, so a Claude-only ledger's report is byte for byte what it
//! was.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Serialize;
use time::Date;

use crate::pricing::{PricingOverride, PricingTable, cost_nanos};
use crate::provider::Provider;
use crate::usage::{self, LedgerLine};

/// How many of the longest-running sessions to list.
const TOP_SESSIONS: usize = 3;

/// One bucket's running totals — requests, the five token dimensions, and cost
/// in nanodollars (see `pricing::cost_nanos` on why nanodollars). Mirrors
/// `usage::Totals` in shape, deliberately: this module aggregates the same
/// dimensions, just sliced by model/account/day instead of by minute.
#[derive(Debug, Clone, Copy, Default)]
struct Agg {
    requests: u64,
    /// Uncached input only: the ledger's `i`, which `cost_nanos` prices at the
    /// base input rate apart from the cache writes and reads beside it.
    input: u64,
    output: u64,
    /// Both cache-write lengths, `c5 + c1`.
    cache_write: u64,
    cache_read: u64,
    cost_nanos: u64,
    /// The Codex lines no price covers, which `cost_nanos` leaves out. An unpriced
    /// Claude line is not counted, so a Claude-only bucket prints the figure it
    /// always has.
    unpriced: u64,
}

impl Agg {
    fn add(&mut self, pricing: &PricingTable, line: &LedgerLine) {
        self.requests += 1;
        self.input += line.i;
        self.output += line.o;
        self.cache_write += line.c5 + line.c1;
        self.cache_read += line.r;
        match line.m.as_deref().and_then(|m| pricing.lookup(m)) {
            Some(price) => {
                self.cost_nanos += cost_nanos(&price, line.i, line.c5, line.c1, line.r, line.o);
            }
            None => match usage::line_provider(line) {
                Provider::Anthropic => {}
                Provider::Codex => self.unpriced += 1,
            },
        }
    }

    fn cost_usd(&self) -> f64 {
        self.cost_nanos as f64 / 1e9
    }
}

/// A period's headline numbers, and the wire shape for `--json`'s `totals` and
/// `previous` fields.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
    /// Codex requests in the period that no price covers, which `cost_usd` leaves out. Absent
    /// at 0, so a Claude-only report's JSON is byte for byte what it was.
    #[serde(skip_serializing_if = "is_zero")]
    pub unpriced_requests: u64,
    /// The share of all prompt tokens that were read from cache:
    /// `cache_read / (input + cache_write + cache_read)`, so always within
    /// 0..=1. `input_tokens` here is the ledger's UNCACHED input, so dividing by
    /// it alone, as this once did, printed 120827.3% for a 30-day window whose
    /// real hit rate was 97.6%. The denominator is the same total prompt that
    /// `cli.rs::cache_hit_ratio` divides by for `tcr status`, whose
    /// `input_tokens` already includes the cached tokens.
    ///
    /// `None` when that total is 0: nothing served is not a measured 0% hit
    /// rate. Same honest-null idiom `tcr status` uses.
    pub cache_hit_ratio: Option<f64>,
}

impl From<Agg> for Totals {
    fn from(agg: Agg) -> Self {
        let prompt = agg.input + agg.cache_write + agg.cache_read;
        Self {
            requests: agg.requests,
            input_tokens: agg.input,
            output_tokens: agg.output,
            cache_write_tokens: agg.cache_write,
            cache_read_tokens: agg.cache_read,
            cost_usd: agg.cost_usd(),
            unpriced_requests: agg.unpriced,
            cache_hit_ratio: if prompt == 0 {
                None
            } else {
                Some(agg.cache_read as f64 / prompt as f64)
            },
        }
    }
}

/// For `#[serde(skip_serializing_if)]` on every `unpriced_requests`: a report with no
/// unpriced Codex request writes no such key, which keeps a Claude-only report's JSON
/// byte for byte what it was.
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One provider's share of the period, using the shared provider vocabulary.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRow {
    pub provider: Provider,
    pub requests: u64,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub unpriced_requests: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRow {
    pub model: String,
    pub requests: u64,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub unpriced_requests: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRow {
    pub account: String,
    pub requests: u64,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub unpriced_requests: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayRow {
    pub date: String,
    pub requests: u64,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub unpriced_requests: u64,
    /// Whether this is the busiest day (most requests) in the period. At most
    /// one row carries `true`; a tie is broken by the earliest date, so the
    /// mark never moves depending on map iteration order.
    pub busiest: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    /// The session id `s` carries — a hash, not a secret, and printed as the
    /// plain number it is.
    pub session: u64,
    pub requests: u64,
}

/// Cost and request-count change against the previous period, as a fraction
/// (`0.12` renders as `+12%`). `None` when the previous period had nothing to
/// compare against (0 requests, or 0 cost), because a percentage change off a
/// zero denominator is not a measurement.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparison {
    pub cost_change: Option<f64>,
    pub requests_change: Option<f64>,
}

fn pct_change(current: f64, previous: f64) -> Option<f64> {
    if previous <= 0.0 {
        None
    } else {
        Some((current - previous) / previous)
    }
}

/// The full report `tcr wrap` prints, and the whole of `--json`'s output.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WrapReport {
    pub days: u32,
    /// Inclusive UTC date range of the current period, `YYYY-MM-DD`.
    pub since: String,
    pub until: String,
    pub totals: Totals,
    /// Requests and cost per provider that served in the period, present only when the period
    /// holds a Codex line: a Claude-only report has no such key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_provider: Option<Vec<ProviderRow>>,
    /// Sorted by cost, highest first.
    pub by_model: Vec<ModelRow>,
    /// Sorted by requests, highest first.
    pub by_account: Vec<AccountRow>,
    /// One row per UTC day in the period, in date order.
    pub by_day: Vec<DayRow>,
    pub distinct_sessions: usize,
    /// The longest-running sessions, by request count, highest first.
    pub top_sessions: Vec<SessionRow>,
    /// The immediately preceding period of the same length.
    pub previous: Totals,
    pub comparison: Comparison,
    /// Lines that were not parseable JSON of the expected shape, over the
    /// current period's files. Counted, never fatal — see
    /// [`crate::usage::parse_ledger_day`].
    pub malformed_lines: usize,
    /// Current-period lines with no provider that their model id filed under Codex, which
    /// are lines written before the ledger recorded one. Said in the text's `note:` line
    /// only, so `--json` gains no key.
    #[serde(skip)]
    pub lines_filed_by_model_id: usize,
}

/// `n` UTC dates ending at (and including) `end`, ascending.
fn dates_ending_at(end: Date, n: u32) -> Vec<Date> {
    (0..n)
        .rev()
        .map(|back| end - time::Duration::days(i64::from(back)))
        .collect()
}

/// Read one period's files into `model`/`account`/`day`/`sessions`, fold every
/// line into `total`, and into `providers` by the provider it belongs to (0 is
/// claude, 1 is codex; [`usage::line_provider`]). `filed_by_model_id` counts the
/// lines with no provider that their model id filed under Codex. Returns the
/// malformed-line count over the period.
#[allow(clippy::too_many_arguments)]
fn accumulate(
    dir: &Path,
    dates: &[Date],
    pricing: &PricingTable,
    total: &mut Agg,
    model: &mut BTreeMap<String, Agg>,
    account: &mut BTreeMap<String, Agg>,
    day: &mut BTreeMap<Date, Agg>,
    sessions: &mut BTreeMap<u64, u64>,
    providers: &mut HashMap<Provider, Agg>,
    filed_by_model_id: &mut usize,
) -> usize {
    let mut malformed = 0;
    for date in dates {
        // Plain or gzipped: a closed day is compressed (`usage::read_ledger_day`).
        let (lines, bad) = usage::parse_ledger_day(dir, *date);
        malformed += bad;
        for line in &lines {
            total.add(pricing, line);
            model
                .entry(line.m.clone().unwrap_or_else(|| "unknown".to_string()))
                .or_default()
                .add(pricing, line);
            account
                .entry(line.a.clone())
                .or_default()
                .add(pricing, line);
            day.entry(usage::ledger_date(line.t))
                .or_default()
                .add(pricing, line);
            if let Some(session) = line.s {
                *sessions.entry(session).or_default() += 1;
            }
            providers
                .entry(usage::line_provider(line))
                .or_default()
                .add(pricing, line);
            if usage::filed_by_model_id(line) {
                *filed_by_model_id += 1;
            }
        }
    }
    malformed
}

/// Build the report for the `days` UTC days ending at `now_ms`'s own UTC day,
/// reading `dir` for both this period and the equal-length period immediately
/// before it.
pub fn build_report(dir: &Path, days: u32, now_ms: i64, pricing: &PricingTable) -> WrapReport {
    let days = days.max(1);
    let today = usage::ledger_date(now_ms);
    let current_dates = dates_ending_at(today, days);
    let previous_end = today - time::Duration::days(i64::from(days));
    let previous_dates = dates_ending_at(previous_end, days);

    let mut total = Agg::default();
    let mut model_agg: BTreeMap<String, Agg> = BTreeMap::new();
    let mut account_agg: BTreeMap<String, Agg> = BTreeMap::new();
    let mut day_agg: BTreeMap<Date, Agg> = BTreeMap::new();
    let mut sessions: BTreeMap<u64, u64> = BTreeMap::new();
    let mut providers = HashMap::new();
    let mut lines_filed_by_model_id = 0;
    let malformed_lines = accumulate(
        dir,
        &current_dates,
        pricing,
        &mut total,
        &mut model_agg,
        &mut account_agg,
        &mut day_agg,
        &mut sessions,
        &mut providers,
        &mut lines_filed_by_model_id,
    );

    let mut previous_total = Agg::default();
    let mut unused_model = BTreeMap::new();
    let mut unused_account = BTreeMap::new();
    let mut unused_day = BTreeMap::new();
    let mut unused_sessions = BTreeMap::new();
    let mut unused_providers = HashMap::new();
    let mut unused_filed_by_model_id = 0;
    accumulate(
        dir,
        &previous_dates,
        pricing,
        &mut previous_total,
        &mut unused_model,
        &mut unused_account,
        &mut unused_day,
        &mut unused_sessions,
        &mut unused_providers,
        &mut unused_filed_by_model_id,
    );

    // Provider declaration order, and only when Codex served during this period.
    // A Claude-only report keeps its previous shape.
    let by_provider = providers.contains_key(&Provider::Codex).then(|| {
        Provider::ALL
            .into_iter()
            .filter_map(|provider| {
                providers.get(&provider).map(|agg| ProviderRow {
                    provider,
                    requests: agg.requests,
                    cost_usd: agg.cost_usd(),
                    unpriced_requests: agg.unpriced,
                })
            })
            .collect()
    });

    let mut by_model: Vec<ModelRow> = model_agg
        .into_iter()
        .map(|(model, agg)| ModelRow {
            model,
            requests: agg.requests,
            cost_usd: agg.cost_usd(),
            unpriced_requests: agg.unpriced,
        })
        .collect();
    by_model.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.model.cmp(&b.model))
    });

    let mut by_account: Vec<AccountRow> = account_agg
        .into_iter()
        .map(|(account, agg)| AccountRow {
            account,
            requests: agg.requests,
            cost_usd: agg.cost_usd(),
            unpriced_requests: agg.unpriced,
        })
        .collect();
    by_account.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.account.cmp(&b.account))
    });

    let busiest_date = day_agg
        .iter()
        .max_by(|a, b| a.1.requests.cmp(&b.1.requests).then(b.0.cmp(a.0)))
        .map(|(date, _)| *date);
    let by_day: Vec<DayRow> = day_agg
        .into_iter()
        .map(|(date, agg)| DayRow {
            date: usage::date_string(date),
            requests: agg.requests,
            cost_usd: agg.cost_usd(),
            unpriced_requests: agg.unpriced,
            busiest: Some(date) == busiest_date,
        })
        .collect();

    let mut top_sessions: Vec<SessionRow> = sessions
        .into_iter()
        .map(|(session, requests)| SessionRow { session, requests })
        .collect();
    top_sessions.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.session.cmp(&b.session))
    });
    let distinct_sessions = top_sessions.len();
    top_sessions.truncate(TOP_SESSIONS);

    let totals = Totals::from(total);
    let previous = Totals::from(previous_total);
    let comparison = Comparison {
        cost_change: pct_change(totals.cost_usd, previous.cost_usd),
        requests_change: pct_change(totals.requests as f64, previous.requests as f64),
    };

    WrapReport {
        days,
        since: usage::date_string(current_dates[0]),
        until: usage::date_string(today),
        totals,
        by_provider,
        by_model,
        by_account,
        by_day,
        distinct_sessions,
        top_sessions,
        previous,
        comparison,
        malformed_lines,
        lines_filed_by_model_id,
    }
}

/// Best-effort pricing overrides from the config file at `config_path` — the
/// same table the running proxy would price with, so a request that would
/// price the same live prices the same in a week-old report. Any read/parse
/// failure (no config, a broken one) falls back to the built-in table alone:
/// this is a read-only report and must never fail because a config is
/// missing.
pub fn pricing_table(config_path: &Path) -> PricingTable {
    let overrides: BTreeMap<String, PricingOverride> = crate::config::load(config_path)
        .map(|config| config.pricing.into_iter().collect())
        .unwrap_or_default();
    PricingTable::new(overrides)
}

fn fmt_pct(change: Option<f64>) -> String {
    match change {
        None => "n/a".to_string(),
        Some(change) => format!("{:+.0}%", change * 100.0),
    }
}

fn fmt_ratio(ratio: Option<f64>) -> String {
    match ratio {
        None => "n/a".to_string(),
        Some(ratio) => format!("{:.1}%", ratio * 100.0),
    }
}

/// A bucket's `cost=` value. With no unpriced Codex request it is the dollar figure it
/// always was, `$0.00` for an unpriced Claude model included: that zero is a known defect,
/// kept as is. With some, it never prints a zero nobody measured. It prints `n/a` and the
/// count when nothing was priced, and the priced part plus the count otherwise.
fn fmt_cost(cost_usd: f64, unpriced: u64) -> String {
    if unpriced == 0 {
        format!("${cost_usd:.2}")
    } else if cost_usd > 0.0 {
        format!("${cost_usd:.2} (+{unpriced} unpriced)")
    } else {
        format!("n/a ({unpriced} unpriced)")
    }
}

/// Render the report as the plain-text terminal output, with no colour. The first totals line
/// carries every token count and can pass 100 columns; the lines a Codex period adds, and the
/// bucket rows beside them, stay under 100 columns at seven-digit counts and five-digit dollar
/// figures.
pub fn render_text(report: &WrapReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "tcr wrap: last {} day(s), {} to {} (UTC)\n\n",
        report.days, report.since, report.until
    ));
    // `n/a` only when the period has unpriced Codex requests and nothing in it was priced:
    // that zero was never measured. Every other period prints the dollar figure it always has.
    let total_cost = if report.totals.unpriced_requests > 0 && report.totals.cost_usd <= 0.0 {
        "n/a".to_string()
    } else {
        format!("${:.2}", report.totals.cost_usd)
    };
    out.push_str(&format!(
        "totals requests={} input={} output={} cacheWrite={} cacheRead={} cost={} cacheHitRatio={}\n",
        report.totals.requests,
        report.totals.input_tokens,
        report.totals.output_tokens,
        report.totals.cache_write_tokens,
        report.totals.cache_read_tokens,
        total_cost,
        fmt_ratio(report.totals.cache_hit_ratio),
    ));
    if report.by_provider.is_some() && report.totals.unpriced_requests > 0 {
        out.push_str(&format!(
            "totals unpriced={} (Codex requests with no price; the cost above covers priced requests only)\n",
            report.totals.unpriced_requests
        ));
    }
    out.push('\n');

    if let Some(rows) = &report.by_provider {
        out.push_str("by provider:\n");
        for row in rows {
            out.push_str(&format!(
                "  {:<28} requests={:<8} cost={}\n",
                row.provider.as_str(),
                row.requests,
                fmt_cost(row.cost_usd, row.unpriced_requests)
            ));
        }
        out.push('\n');
    }

    out.push_str("by model (sorted by cost):\n");
    for row in &report.by_model {
        out.push_str(&format!(
            "  {:<28} requests={:<8} cost={}\n",
            row.model,
            row.requests,
            fmt_cost(row.cost_usd, row.unpriced_requests)
        ));
    }
    out.push('\n');

    out.push_str("by account (sorted by requests):\n");
    for row in &report.by_account {
        out.push_str(&format!(
            "  {:<28} requests={:<8} cost={}\n",
            row.account,
            row.requests,
            fmt_cost(row.cost_usd, row.unpriced_requests)
        ));
    }
    out.push('\n');

    out.push_str("by day (busiest marked *):\n");
    for row in &report.by_day {
        out.push_str(&format!(
            "  {} {} requests={:<8} cost={}\n",
            row.date,
            if row.busiest { "*" } else { " " },
            row.requests,
            fmt_cost(row.cost_usd, row.unpriced_requests)
        ));
    }
    out.push('\n');

    out.push_str(&format!(
        "sessions: {} distinct; top {} by requests:\n",
        report.distinct_sessions,
        report.top_sessions.len()
    ));
    for row in &report.top_sessions {
        out.push_str(&format!(
            "  session {} requests={}\n",
            row.session, row.requests
        ));
    }
    out.push('\n');

    // The cost comparison covers priced requests only once either period has a Codex request
    // no price covers; said only when the period holds a Codex line.
    let priced_only = report.by_provider.is_some()
        && report.comparison.cost_change.is_some()
        && report.totals.unpriced_requests + report.previous.unpriced_requests > 0;
    out.push_str(&format!(
        "compared with the previous {} day(s): cost={}{} requests={}\n",
        report.days,
        fmt_pct(report.comparison.cost_change),
        if priced_only {
            " (priced requests only)"
        } else {
            ""
        },
        fmt_pct(report.comparison.requests_change),
    ));
    if report.malformed_lines > 0 {
        out.push_str(&format!(
            "note: {} malformed ledger line(s) were skipped, not fatal\n",
            report.malformed_lines
        ));
    }
    if report.lines_filed_by_model_id > 0 {
        out.push_str(&format!(
            "note: {} older ledger line(s) carry no provider; filed by model id\n",
            report.lines_filed_by_model_id
        ));
    }
    out
}

/// Print `tcr wrap`'s report, text or `--json`, reading `dir` for the ledger
/// and `config_path` for pricing overrides only (never for accounts — see the
/// module docs on why this report resolves nothing against the config).
pub fn print_report(dir: &Path, config_path: &Path, days: u32, json: bool) -> anyhow::Result<()> {
    let pricing = pricing_table(config_path);
    let report = build_report(dir, days, crate::now_ms(), &pricing);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_day(dir: &Path, date: &str, lines: &[&str]) {
        fs::write(dir.join(format!("{date}.jsonl")), lines.join("\n") + "\n")
            .expect("write fixture day file");
    }

    /// Milliseconds for a given UTC date at noon — comfortably inside the day
    /// whichever local offset a CI runner happens to have, since `ledger_date`
    /// is UTC-only.
    fn noon_ms(date: Date) -> i64 {
        use time::{OffsetDateTime, Time};
        OffsetDateTime::new_utc(date, Time::from_hms(12, 0, 0).expect("valid time"))
            .unix_timestamp()
            * 1000
    }

    /// Two days, two models, two accounts, exact totals — gate 1 in the
    /// bridge. Fixture accounts are the repo's standard fake ones.
    #[test]
    fn two_days_two_models_two_accounts_exact_totals() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let day1 = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();
        let day2 = Date::from_calendar_date(2026, time::Month::September, 6).unwrap();
        let ts1 = noon_ms(day1);
        let ts2 = noon_ms(day2);

        // Day 1: alice on opus-5, bob on sonnet-5.
        write_day(
            tmp.path(),
            "2026-09-05",
            &[
                &format!(
                    r#"{{"t":{ts1},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":100000}}"#
                ),
                &format!(
                    r#"{{"t":{ts1},"a":"bob@example.com","m":"claude-sonnet-5","s":2,"i":500000,"c5":0,"c1":0,"r":100000,"o":50000}}"#
                ),
            ],
        );
        // Day 2: alice again on opus-5 (two requests, same session as day 1
        // does not matter — sessions are counted globally), plus one
        // unpriced/unknown-model line to prove it still counts as a request.
        write_day(
            tmp.path(),
            "2026-09-06",
            &[
                &format!(
                    r#"{{"t":{ts2},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#
                ),
                &format!(
                    r#"{{"t":{ts2},"a":"bob@example.com","m":"gpt-9","s":3,"i":10,"c5":0,"c1":0,"r":0,"o":10}}"#
                ),
            ],
        );

        let report = build_report(tmp.path(), 2, noon_ms(day2), &PricingTable::default());

        assert_eq!(report.totals.requests, 4);
        assert_eq!(report.totals.input_tokens, 2_500_010);
        assert_eq!(report.totals.output_tokens, 150_010);
        assert_eq!(report.totals.cache_read_tokens, 100_000);
        // opus-5 $5/MTok input, $25/MTok output: 2 requests of 1M input each =
        // $10.00 input, one with 100k output = $2.50; sonnet-5 $2/MTok input,
        // $10/MTok output: 500k input = $1.00, 50k output = $0.50, 100k
        // cache-read at 0.1x input ($0.20/MTok) = $0.02. gpt-9 is unpriced.
        assert!(
            (report.totals.cost_usd - 14.02).abs() < 1e-9,
            "{}",
            report.totals.cost_usd
        );

        assert_eq!(report.by_model.len(), 3, "opus-5, sonnet-5, gpt-9");
        assert_eq!(
            report.by_model[0].model, "claude-opus-5",
            "highest cost first"
        );
        assert_eq!(report.by_model[0].requests, 2);

        assert_eq!(report.by_account.len(), 2);
        assert_eq!(report.by_account[0].account, "alice@example.com");
        assert_eq!(report.by_account[0].requests, 2);
        assert_eq!(report.by_account[1].account, "bob@example.com");
        assert_eq!(report.by_account[1].requests, 2);

        assert_eq!(report.by_day.len(), 2);
        assert_eq!(report.by_day[0].date, "2026-09-05");
        assert_eq!(report.by_day[0].requests, 2);
        assert_eq!(report.by_day[1].date, "2026-09-06");
        assert_eq!(report.by_day[1].requests, 2);
        // Both days served 2 requests — a tie, broken by earliest date.
        assert!(report.by_day[0].busiest);
        assert!(!report.by_day[1].busiest);

        assert_eq!(report.distinct_sessions, 3);
        assert_eq!(report.top_sessions[0].session, 1, "session 1 served 2 of 4");
        assert_eq!(report.top_sessions[0].requests, 2);
    }

    /// The comparison line: a second period with a known ratio to the first
    /// produces the exact percentage.
    #[test]
    fn comparison_against_the_previous_period_is_exact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = Date::from_calendar_date(2026, time::Month::September, 3).unwrap();
        let curr = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();

        write_day(
            tmp.path(),
            "2026-09-03",
            &[&format!(
                r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#,
                noon_ms(prev)
            )],
        );
        // Current period: two identical requests, so cost and requests both
        // double relative to the previous period -> +100%.
        write_day(
            tmp.path(),
            "2026-09-05",
            &[
                &format!(
                    r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#,
                    noon_ms(curr)
                ),
                &format!(
                    r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#,
                    noon_ms(curr)
                ),
            ],
        );

        // days=1 so the "current" period is just 2026-09-05 and the
        // "previous" period is just 2026-09-03 (one day back from the day
        // before the current period starts): 2026-09-04, empty. To exercise
        // an actual non-empty previous period, use days=2: current =
        // [09-04, 09-05], previous = [09-02, 09-03].
        write_day(tmp.path(), "2026-09-04", &[]);
        write_day(tmp.path(), "2026-09-02", &[]);

        let report = build_report(tmp.path(), 2, noon_ms(curr), &PricingTable::default());
        assert_eq!(report.previous.requests, 1);
        assert_eq!(report.totals.requests, 2);
        assert_eq!(
            report.comparison.requests_change,
            Some(1.0),
            "2 vs 1 request is +100%"
        );
        assert_eq!(
            report.comparison.cost_change,
            Some(1.0),
            "identical per-request cost, double the requests, is +100% cost"
        );
    }

    /// A malformed line is counted, not fatal — matches what
    /// `UsageTracker::replay_day` already does with the same file.
    #[test]
    fn a_malformed_line_is_counted_not_fatal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let day = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();
        write_day(
            tmp.path(),
            "2026-09-05",
            &[
                "not json at all",
                &format!(
                    r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000,"c5":0,"c1":0,"r":0,"o":0}}"#,
                    noon_ms(day)
                ),
            ],
        );
        let report = build_report(tmp.path(), 1, noon_ms(day), &PricingTable::default());
        assert_eq!(report.totals.requests, 1, "the good line is still counted");
        assert_eq!(report.malformed_lines, 1);
    }

    /// The hit rate is the share of the WHOLE prompt read from cache, so it
    /// stays within 0..=1. The ledger's `i` is uncached input only, and
    /// dividing cache reads by it alone printed 120827.3% for a real 30-day
    /// window whose hit rate was 97.6%.
    #[test]
    fn cache_hit_ratio_is_a_share_of_the_whole_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let day = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();
        write_day(
            tmp.path(),
            "2026-09-05",
            &[format!(
                r#"{{"t":{},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":100,"c5":60,"c1":40,"r":800,"o":10}}"#,
                noon_ms(day)
            )
            .as_str()],
        );
        let report = build_report(tmp.path(), 1, noon_ms(day), &PricingTable::default());
        assert_eq!(report.totals.input_tokens, 100);
        assert_eq!(report.totals.cache_write_tokens, 100, "c5 + c1");
        assert_eq!(report.totals.cache_read_tokens, 800);
        assert_eq!(
            report.totals.cache_hit_ratio,
            Some(0.8),
            "800 read of 1000 prompt tokens, not 800 / 100"
        );
    }

    /// An empty ledger directory (no traffic at all) reports honest zeroes and
    /// `None` for ratios/comparisons, never a divide-by-zero panic.
    #[test]
    fn an_empty_ledger_reports_honest_zeroes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let day = Date::from_calendar_date(2026, time::Month::September, 5).unwrap();
        let report = build_report(tmp.path(), 7, noon_ms(day), &PricingTable::default());
        assert_eq!(report.totals.requests, 0);
        assert_eq!(report.totals.cost_usd, 0.0);
        assert!(report.totals.cache_hit_ratio.is_none());
        assert!(report.comparison.cost_change.is_none());
        assert!(report.comparison.requests_change.is_none());
        assert!(report.by_model.is_empty());
        assert!(report.by_day.is_empty());
    }

    /// The Claude-only ledger the wrap goldens read: two current days and one earlier day,
    /// three Claude accounts, a Claude model this build has no price for, and one line that
    /// is not JSON. Returns the report's clock, noon UTC on the last day.
    fn claude_only_ledger(dir: &Path) -> i64 {
        let day = |d: u8| {
            noon_ms(
                Date::from_calendar_date(2026, time::Month::September, d).expect("a calendar date"),
            )
        };
        let (t3, t5, t6) = (day(3), day(5), day(6));
        write_day(
            dir,
            "2026-09-03",
            &[&format!(
                r#"{{"t":{t3},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#
            )],
        );
        write_day(
            dir,
            "2026-09-05",
            &[
                &format!(
                    r#"{{"t":{t5},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":100000}}"#
                ),
                &format!(
                    r#"{{"t":{t5},"a":"bob@example.com","m":"claude-sonnet-5","s":2,"i":500000,"c5":0,"c1":0,"r":100000,"o":50000}}"#
                ),
                &format!(
                    r#"{{"t":{t5},"a":"carol@example.com","m":"claude-opus-4-5","s":3,"i":10,"c5":0,"c1":0,"r":0,"o":10}}"#
                ),
            ],
        );
        write_day(
            dir,
            "2026-09-06",
            &[
                &format!(
                    r#"{{"t":{t6},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#
                ),
                "not json at all",
            ],
        );
        t6
    }

    /// What `tcr wrap` prints for a Claude-only ledger, text and JSON, pinned before the report
    /// learned about Codex: every later change must leave these bytes exactly as they are. The
    /// ledger carries a Claude model this build has no price for, and its rows print
    /// `cost=$0.00`. That zero is a known defect, kept on purpose: fixing it changes what a
    /// Claude-only ledger prints. The JSON is compared in compact form; `--json` pretty-prints
    /// the same keys in the same order.
    #[test]
    fn claude_only_wrap_is_unchanged() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = claude_only_ledger(tmp.path());
        let report = build_report(tmp.path(), 2, now, &PricingTable::default());
        assert_eq!(
            render_text(&report),
            concat!(
                "tcr wrap: last 2 day(s), 2026-09-05 to 2026-09-06 (UTC)\n",
                "\n",
                "totals requests=4 input=2500010 output=150010 cacheWrite=0 cacheRead=100000 cost=$14.02 cacheHitRatio=3.8%\n", // 100000 cache-read tokens of 2600010 prompt tokens
                "\n",
                "by model (sorted by cost):\n",
                "  claude-opus-5                requests=2        cost=$12.50\n",
                "  claude-sonnet-5              requests=1        cost=$1.52\n",
                "  claude-opus-4-5              requests=1        cost=$0.00\n",
                "\n",
                "by account (sorted by requests):\n",
                "  alice@example.com            requests=2        cost=$12.50\n",
                "  bob@example.com              requests=1        cost=$1.52\n",
                "  carol@example.com            requests=1        cost=$0.00\n",
                "\n",
                "by day (busiest marked *):\n",
                "  2026-09-05 * requests=3        cost=$9.02\n",
                "  2026-09-06   requests=1        cost=$5.00\n",
                "\n",
                "sessions: 3 distinct; top 3 by requests:\n",
                "  session 1 requests=2\n",
                "  session 2 requests=1\n",
                "  session 3 requests=1\n",
                "\n",
                "compared with the previous 2 day(s): cost=+180% requests=+300%\n", // cost (14.02 - 5.00)/5.00, requests (4 - 1)/1
                "note: 1 malformed ledger line(s) were skipped, not fatal\n",
            )
        );
        assert_eq!(
            serde_json::to_string(&report).expect("the report serializes"),
            r#"{"days":2,"since":"2026-09-05","until":"2026-09-06","totals":{"requests":4,"inputTokens":2500010,"outputTokens":150010,"cacheWriteTokens":0,"cacheReadTokens":100000,"costUsd":14.02,"cacheHitRatio":0.038461390533113336},"byModel":[{"model":"claude-opus-5","requests":2,"costUsd":12.5},{"model":"claude-sonnet-5","requests":1,"costUsd":1.52},{"model":"claude-opus-4-5","requests":1,"costUsd":0.0}],"byAccount":[{"account":"alice@example.com","requests":2,"costUsd":12.5},{"account":"bob@example.com","requests":1,"costUsd":1.52},{"account":"carol@example.com","requests":1,"costUsd":0.0}],"byDay":[{"date":"2026-09-05","requests":3,"costUsd":9.02,"busiest":true},{"date":"2026-09-06","requests":1,"costUsd":5.0,"busiest":false}],"distinctSessions":3,"topSessions":[{"session":1,"requests":2},{"session":2,"requests":1},{"session":3,"requests":1}],"previous":{"requests":1,"inputTokens":1000000,"outputTokens":0,"cacheWriteTokens":0,"cacheReadTokens":0,"costUsd":5.0,"cacheHitRatio":0.0},"comparison":{"costChange":1.8039999999999998,"requestsChange":3.0},"malformedLines":1}"#
        );
    }

    /// A mixed ledger. alice's Claude traffic falls on the same days as the Claude-only
    /// ledger's, with an earlier day costing $5.00. Beside it are three lines on bob's Codex
    /// account, for a model this build cannot price. Returns the report's clock.
    fn mixed_ledger(dir: &Path) -> i64 {
        let day = |d: u8| {
            noon_ms(
                Date::from_calendar_date(2026, time::Month::September, d).expect("a calendar date"),
            )
        };
        let (t3, t5, t6) = (day(3), day(5), day(6));
        let codex = |t: i64| {
            format!(
                r#"{{"t":{t},"a":"bob@example.com/codex","p":"codex","m":"gpt-5.1-codex","s":4,"i":1000,"c5":0,"c1":0,"r":500,"o":100}}"#
            )
        };
        write_day(
            dir,
            "2026-09-03",
            &[&format!(
                r#"{{"t":{t3},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#
            )],
        );
        write_day(
            dir,
            "2026-09-05",
            &[
                &format!(
                    r#"{{"t":{t5},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":100000}}"#
                ),
                &codex(t5),
                &codex(t5),
            ],
        );
        write_day(
            dir,
            "2026-09-06",
            &[
                &codex(t6),
                &format!(
                    r#"{{"t":{t6},"a":"alice@example.com","m":"claude-opus-5","s":1,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#
                ),
            ],
        );
        t6
    }

    /// One price for `gpt-5.1-codex`, at $1.25 in and $10 out per million tokens.
    fn gpt_pricing() -> PricingTable {
        PricingTable::new(BTreeMap::from([(
            "gpt-5.1-codex".to_string(),
            PricingOverride {
                input: 1.25,
                output: 10.0,
                cache_write_5m: None,
                cache_write_1h: None,
                cache_read: None,
            },
        )]))
    }

    /// A Codex bucket no price covers reads `cost=n/a` and its unpriced count, never `$0.00`.
    /// A bucket mixing priced and unpriced requests prints the priced part and the count it
    /// leaves out. The period gains a second totals line, the `by provider` block, and
    /// `(priced requests only)` on its cost comparison.
    #[test]
    fn a_codex_bucket_reads_cost_na_with_its_unpriced_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = mixed_ledger(tmp.path());
        let text = render_text(&build_report(tmp.path(), 2, now, &PricingTable::default()));
        assert_eq!(
            text,
            concat!(
                "tcr wrap: last 2 day(s), 2026-09-05 to 2026-09-06 (UTC)\n",
                "\n",
                "totals requests=5 input=2003000 output=100300 cacheWrite=0 cacheRead=1500 cost=$12.50 cacheHitRatio=0.1%\n", // 1500 cache-read tokens of 2004500 prompt tokens
                "totals unpriced=3 (Codex requests with no price; the cost above covers priced requests only)\n",
                "\n",
                "by provider:\n",
                "  anthropic                    requests=2        cost=$12.50\n",
                "  codex                        requests=3        cost=n/a (3 unpriced)\n",
                "\n",
                "by model (sorted by cost):\n",
                "  claude-opus-5                requests=2        cost=$12.50\n",
                "  gpt-5.1-codex                requests=3        cost=n/a (3 unpriced)\n",
                "\n",
                "by account (sorted by requests):\n",
                "  bob@example.com/codex        requests=3        cost=n/a (3 unpriced)\n",
                "  alice@example.com            requests=2        cost=$12.50\n",
                "\n",
                "by day (busiest marked *):\n",
                "  2026-09-05 * requests=3        cost=$7.50 (+2 unpriced)\n",
                "  2026-09-06   requests=2        cost=$5.00 (+1 unpriced)\n",
                "\n",
                "sessions: 2 distinct; top 2 by requests:\n",
                "  session 4 requests=3\n",
                "  session 1 requests=2\n",
                "\n",
                "compared with the previous 2 day(s): cost=+150% (priced requests only) requests=+400%\n", // cost (12.50 - 5.00)/5.00, requests (5 - 1)/1
            )
        );
        assert!(!text.contains("$0.00"));
    }

    /// A Codex model priced by an override prints dollars on every surface, with the count of
    /// the requests no price covers beside them: `cost=$x (+n unpriced)`. Never `$0.00`.
    #[test]
    fn a_priced_codex_bucket_prints_dollars_and_its_unpriced_tail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let t6 = noon_ms(
            Date::from_calendar_date(2026, time::Month::September, 6).expect("a calendar date"),
        );
        let line = |model: &str, input: u64| {
            format!(
                r#"{{"t":{t6},"a":"bob@example.com/codex","p":"codex","m":"{model}","s":5,"i":{input},"c5":0,"c1":0,"r":0,"o":0}}"#
            )
        };
        write_day(
            tmp.path(),
            "2026-09-06",
            &[
                &line("gpt-5.1-codex", 1_000_000),
                &line("gpt-5.1-codex", 1_000_000),
                &line("codex-mini-latest", 1_000),
            ],
        );
        let text = render_text(&build_report(tmp.path(), 1, t6, &gpt_pricing()));
        assert_eq!(
            text,
            concat!(
                "tcr wrap: last 1 day(s), 2026-09-06 to 2026-09-06 (UTC)\n",
                "\n",
                "totals requests=3 input=2001000 output=0 cacheWrite=0 cacheRead=0 cost=$2.50 cacheHitRatio=0.0%\n", // 0 cache-read tokens of 2001000 prompt tokens
                "totals unpriced=1 (Codex requests with no price; the cost above covers priced requests only)\n",
                "\n",
                "by provider:\n",
                "  codex                        requests=3        cost=$2.50 (+1 unpriced)\n",
                "\n",
                "by model (sorted by cost):\n",
                "  gpt-5.1-codex                requests=2        cost=$2.50\n",
                "  codex-mini-latest            requests=1        cost=n/a (1 unpriced)\n",
                "\n",
                "by account (sorted by requests):\n",
                "  bob@example.com/codex        requests=3        cost=$2.50 (+1 unpriced)\n",
                "\n",
                "by day (busiest marked *):\n",
                "  2026-09-06 * requests=3        cost=$2.50 (+1 unpriced)\n",
                "\n",
                "sessions: 1 distinct; top 1 by requests:\n",
                "  session 5 requests=3\n",
                "\n",
                "compared with the previous 1 day(s): cost=n/a requests=n/a\n",
            )
        );
    }

    /// The `by provider` block prints only when the period holds a Codex line. With every Codex
    /// line priced, it prints without the unpriced lines: no second totals line, no
    /// annotation, no `(priced requests only)`. A Claude-only ledger prints none of it.
    #[test]
    fn the_by_provider_block_prints_only_with_codex_lines() {
        let claude_dir = tempfile::tempdir().expect("tempdir");
        let now = claude_only_ledger(claude_dir.path());
        let report = build_report(claude_dir.path(), 2, now, &PricingTable::default());
        assert!(report.by_provider.is_none());
        let text = render_text(&report);
        for absent in [
            "by provider:",
            "unpriced",
            "priced requests only",
            "filed by model id",
        ] {
            assert!(
                !text.contains(absent),
                "a Claude-only report says nothing of {absent:?}"
            );
        }

        let priced_dir = tempfile::tempdir().expect("tempdir");
        let day = |d: u8| {
            noon_ms(Date::from_calendar_date(2026, time::Month::September, d).expect("a date"))
        };
        let line = |t: i64| {
            format!(
                r#"{{"t":{t},"a":"bob@example.com/codex","p":"codex","m":"gpt-5.1-codex","s":6,"i":1000000,"c5":0,"c1":0,"r":0,"o":0}}"#
            )
        };
        write_day(priced_dir.path(), "2026-09-04", &[&line(day(4))]);
        write_day(
            priced_dir.path(),
            "2026-09-06",
            &[&line(day(6)), &line(day(6))],
        );
        let text = render_text(&build_report(priced_dir.path(), 2, day(6), &gpt_pricing()));
        assert!(
            text.contains(
                "by provider:\n  codex                        requests=2        cost=$2.50\n\n"
            ),
            "{text}"
        );
        assert!(!text.contains("unpriced"), "{text}");
        assert!(
            text.contains("compared with the previous 2 day(s): cost=+100% requests=+100%\n"), // cost (2.50 - 1.25)/1.25, requests (2 - 1)/1
            "{text}"
        );
    }

    /// A line written before the ledger recorded its provider is filed by its model id.
    /// `gpt-…` and `codex-…` go to codex, and the report says how many lines it filed that way.
    /// A line that names `p` is filed by it, whatever its model id; an Anthropic line carries
    /// no `p` and is not counted. The count is text only: `--json` gains no key for it.
    #[test]
    fn lines_without_a_provider_are_filed_by_model_id_and_counted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let t6 = noon_ms(
            Date::from_calendar_date(2026, time::Month::September, 6).expect("a calendar date"),
        );
        let old = |account: &str, model: &str, session: u64| {
            format!(
                r#"{{"t":{t6},"a":"{account}","m":"{model}","s":{session},"i":10,"c5":0,"c1":0,"r":0,"o":1}}"#
            )
        };
        write_day(
            tmp.path(),
            "2026-09-06",
            &[
                &old("bob@example.com/codex", "gpt-5.1-codex", 7),
                &old("bob@example.com/codex", "codex-mini-latest", 7),
                &old("bob@example.com/codex", "gpt-5.1-codex-max", 7),
                &old("alice@example.com", "claude-opus-5", 8),
                &format!(
                    r#"{{"t":{t6},"a":"dana@example.com/codex","p":"codex","m":"o4-mini","s":9,"i":10,"c5":0,"c1":0,"r":0,"o":1}}"#
                ),
            ],
        );
        let report = build_report(tmp.path(), 1, t6, &PricingTable::default());
        assert_eq!(report.lines_filed_by_model_id, 3);
        let providers: Vec<(&str, u64)> = report
            .by_provider
            .as_deref()
            .expect("the period holds Codex lines")
            .iter()
            .map(|row| (row.provider.as_str(), row.requests))
            .collect();
        assert_eq!(providers, [("anthropic", 1), ("codex", 4)]);
        assert!(
            render_text(&report)
                .ends_with("note: 3 older ledger line(s) carry no provider; filed by model id\n")
        );
        assert!(
            !serde_json::to_string(&report)
                .expect("serializes")
                .contains("iledByModelId")
        );
    }

    /// `unpricedRequests` rides on the totals and every bucket of `--json`, and is absent at
    /// 0. A Claude row in a mixed report, and the whole of a Claude-only one, carry no such key.
    /// `costUsd` stays the priced floor.
    #[test]
    fn wrap_json_skips_unpriced_requests_at_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = mixed_ledger(tmp.path());
        let json = serde_json::to_value(build_report(tmp.path(), 2, now, &PricingTable::default()))
            .expect("serializes");
        assert_eq!(json["totals"]["unpricedRequests"], 3);
        assert_eq!(json["totals"]["costUsd"], 12.5, "the priced floor");
        assert!(
            json["previous"].get("unpricedRequests").is_none(),
            "the earlier period was all priced"
        );
        assert!(
            json["byModel"][0].get("unpricedRequests").is_none(),
            "claude-opus-5"
        );
        assert_eq!(json["byModel"][1]["unpricedRequests"], 3, "gpt-5.1-codex");
        assert_eq!(
            json["byAccount"][0]["unpricedRequests"], 3,
            "bob@example.com/codex"
        );
        assert_eq!(json["byDay"][0]["unpricedRequests"], 2);

        let claude_dir = tempfile::tempdir().expect("tempdir");
        let now = claude_only_ledger(claude_dir.path());
        let compact = serde_json::to_string(&build_report(
            claude_dir.path(),
            2,
            now,
            &PricingTable::default(),
        ))
        .expect("serializes");
        assert!(!compact.contains("unpricedRequests"));
    }

    /// `byProvider` appears only when the period holds a Codex line, right after `totals`, one
    /// row per provider that served, in the text block's order.
    #[test]
    fn wrap_json_adds_by_provider_only_with_codex() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = mixed_ledger(tmp.path());
        let report = build_report(tmp.path(), 2, now, &PricingTable::default());
        let compact = serde_json::to_string(&report).expect("serializes");
        let at = |key: &str| {
            compact
                .find(key)
                .unwrap_or_else(|| panic!("{key} in {compact}"))
        };
        assert!(
            at("\"totals\"") < at("\"byProvider\"") && at("\"byProvider\"") < at("\"byModel\"")
        );
        assert_eq!(
            serde_json::to_value(&report).expect("serializes")["byProvider"],
            serde_json::json!([
                { "provider": "anthropic", "requests": 2, "costUsd": 12.5 },
                { "provider": "codex", "requests": 3, "costUsd": 0.0, "unpricedRequests": 3 }
            ])
        );

        let claude_dir = tempfile::tempdir().expect("tempdir");
        let now = claude_only_ledger(claude_dir.path());
        let compact = serde_json::to_string(&build_report(
            claude_dir.path(),
            2,
            now,
            &PricingTable::default(),
        ))
        .expect("serializes");
        assert!(!compact.contains("byProvider"));
    }

    /// Every line a Codex period adds, and every bucket row with its unpriced count beside the
    /// cost, stays under 100 columns. The test uses seven-digit counts and five-digit dollar
    /// figures. The first totals line is left out: it is the line a Claude-only report has
    /// always printed, and its token counts already take it past 100 columns on this ledger's
    /// own figures.
    #[test]
    fn every_line_stays_under_100_columns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = mixed_ledger(tmp.path());
        let mut report = build_report(tmp.path(), 2, now, &PricingTable::default());
        const BIG: u64 = 9_999_999;
        report.totals.unpriced_requests = BIG;
        for row in report.by_provider.iter_mut().flatten() {
            (row.requests, row.unpriced_requests, row.cost_usd) = (BIG, BIG, 99_999.99);
        }
        for row in &mut report.by_model {
            (row.requests, row.unpriced_requests, row.cost_usd) = (BIG, BIG, 99_999.99);
        }
        for row in &mut report.by_account {
            (row.requests, row.unpriced_requests, row.cost_usd) = (BIG, BIG, 99_999.99);
        }
        for row in &mut report.by_day {
            (row.requests, row.unpriced_requests, row.cost_usd) = (BIG, BIG, 99_999.99);
        }
        report.lines_filed_by_model_id = BIG as usize;
        report.malformed_lines = BIG as usize;
        let text = render_text(&report);
        assert!(
            text.contains("totals unpriced=9999999 ("),
            "the widest line is in the run:\n{text}"
        );
        let first_totals = |line: &&str| line.starts_with("totals requests=");
        assert_eq!(
            text.lines().filter(first_totals).count(),
            1,
            "only the first totals line is left out"
        );
        for line in text.lines().filter(|line| !first_totals(line)) {
            assert!(
                line.chars().count() < 100,
                "{} columns: {line}",
                line.chars().count()
            );
        }
    }
}
