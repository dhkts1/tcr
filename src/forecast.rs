//! Demand estimates and cold, runtime-only quota calibration. Nothing here selects an
//! account or fetches a reading. The ledger owns demand, and the Manager supplies exact
//! completed usage vectors and quiescent readings while holding its account lock.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

use crate::{provider::Provider, usage::UsageRecord};

const HOUR_MS: i64 = 3_600_000;
const HISTORY_HOURS: i64 = 672;
const MAX_SAMPLES: usize = 2048;
type Tokens = BTreeMap<String, [u64; 2]>;

fn family(provider: Provider, model: Option<&str>) -> Option<String> {
    let model = model.filter(|model| !model.is_empty())?;
    match provider {
        Provider::Anthropic => crate::model::family(model).map(str::to_owned),
        Provider::Codex => Some(model.to_owned()),
    }
}

fn vector(record: &UsageRecord) -> Option<[u64; 2]> {
    let input = record
        .input
        .checked_add(record.cache_5m)?
        .checked_add(record.cache_1h)?;
    (input.checked_add(record.cache_read)? == record.quota_input
        && record.reasoning <= record.output)
        .then_some([input, record.output])
}

fn add(target: &mut [u64; 2], value: [u64; 2]) -> Option<()> {
    let next = [
        target[0].checked_add(value[0])?,
        target[1].checked_add(value[1])?,
    ];
    *target = next;
    Some(())
}

/// Input-equivalent tokens for one whole percent, and output's relative cost.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scale {
    pub tokens_per_percent: f64,
    pub output_weight: f64,
}

#[derive(Debug, Clone)]
struct Anchor {
    at: i64,
    reset: i64,
    percent: f64,
    tokens: Tokens,
}

#[derive(Debug, Default)]
struct WindowFit {
    anchor: Option<Anchor>,
    samples: VecDeque<(Tokens, f64)>,
    scales: BTreeMap<String, Scale>,
}

impl WindowFit {
    fn invalidate(&mut self) {
        self.anchor = None;
        self.samples.clear();
        self.scales.clear();
    }

    fn observe(&mut self, bucket: Option<crate::probe::UsageBucket>, now: i64, tokens: Tokens) {
        let reading = bucket
            .and_then(|b| Some((b.utilization?, b.reset_at_ms?)))
            .filter(|(u, reset)| u.is_finite() && *u >= 0.0 && *reset > now);
        let Some((u, reset)) = reading else {
            self.invalidate();
            return;
        };
        let current = Anchor {
            at: now,
            reset,
            // Whole-percent decimal readings such as 0.29 can multiply to the
            // preceding float. Compensate only for multiplication roundoff.
            percent: (100.0 * u + 8.0 * f64::EPSILON * (100.0 * u).abs()).floor(),
            tokens,
        };
        let Some(prior) = self.anchor.take() else {
            self.anchor = Some(current);
            return;
        };
        if now <= prior.at
            || now >= prior.reset
            || reset.abs_diff(prior.reset) > 120_000
            || current.percent < prior.percent
        {
            self.anchor = Some(current);
            return;
        }
        if current.percent == prior.percent {
            // Several completed requests can fit inside one quantized tick. Keep the
            // original watermark until a rising tick brackets all of them.
            self.anchor = Some(prior);
            return;
        }
        let mut delta = Tokens::new();
        for (family, values) in &current.tokens {
            let before = prior.tokens.get(family).copied().unwrap_or_default();
            let Some(x) = values[0].checked_sub(before[0]) else {
                self.invalidate();
                return;
            };
            let Some(o) = values[1].checked_sub(before[1]) else {
                self.invalidate();
                return;
            };
            if x != 0 || o != 0 {
                delta.insert(family.clone(), [x, o]);
            }
        }
        if delta.is_empty() {
            // A rising percent without exact classified tokens is unexplained.
            self.invalidate();
        } else {
            self.samples
                .push_back((delta, current.percent - prior.percent));
            if self.samples.len() > MAX_SAMPLES {
                self.samples.pop_front();
            }
            self.scales = fit(&self.samples).unwrap_or_default();
        }
        self.anchor = Some(current);
    }
}

/// One stable account slot. No identity, anchors or fit is persisted at restart.
#[derive(Debug, Default)]
pub(crate) struct Calibration {
    tokens: Tokens,
    windows: [WindowFit; 3],
    last_reading: Option<i64>,
}

impl Calibration {
    pub(crate) fn record(&mut self, record: &UsageRecord) {
        let known = family(record.provider, record.model.as_deref()).zip(vector(record));
        let Some((family, value)) = known else {
            self.invalidate();
            return;
        };
        if self.last_reading.is_some_and(|at| record.ts_ms <= at)
            || add(self.tokens.entry(family).or_default(), value).is_none()
        {
            self.invalidate();
        }
    }

    fn invalidate(&mut self) {
        for window in &mut self.windows {
            window.invalidate();
        }
    }

    pub(crate) fn observe(
        &mut self,
        buckets: [Option<crate::probe::UsageBucket>; 3],
        now: i64,
        unfinished: bool,
    ) {
        if unfinished || self.last_reading.is_some_and(|prior| now < prior) {
            self.invalidate();
            self.last_reading = Some(now);
            return;
        }
        self.last_reading = Some(now);
        for (idx, (window, bucket)) in self.windows.iter_mut().zip(buckets).enumerate() {
            let tokens = self
                .tokens
                .iter()
                .filter(|(family, _)| idx != 2 || *family == "fable")
                .map(|(family, tokens)| (family.clone(), *tokens))
                .collect();
            window.observe(bucket, now, tokens);
        }
    }

    pub(crate) fn scale(&self, window: usize, family: &str) -> Option<Scale> {
        self.windows.get(window)?.scales.get(family).copied()
    }

    pub(crate) fn ready(&self, families: &BTreeSet<String>, scoped: bool) -> bool {
        !families.is_empty()
            && families.iter().all(|family| {
                (0..if scoped && family == "fable" { 3 } else { 2 }).all(|w| {
                    self.scale(w, family).is_some_and(|scale| {
                        scale.tokens_per_percent > 0.0 && scale.output_weight > 0.0
                    })
                })
            })
    }
}

/// Column-scaled nonnegative least squares. Full rank is checked independently of the
/// nonnegative solve; positive finite coefficients and whole-percent RMS gate readiness.
fn fit(samples: &VecDeque<(Tokens, f64)>) -> Option<BTreeMap<String, Scale>> {
    let families: Vec<_> = samples
        .iter()
        .flat_map(|(x, _)| x.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let n = 2 * families.len();
    if n == 0 || samples.len() < 10 * n {
        return None;
    }
    let xs: Vec<Vec<f64>> = samples
        .iter()
        .map(|(row, _)| {
            families
                .iter()
                .flat_map(|family| {
                    row.get(family)
                        .copied()
                        .unwrap_or_default()
                        .map(|v| v as f64)
                })
                .collect()
        })
        .collect();
    let norms: Vec<f64> = (0..n)
        .map(|col| xs.iter().map(|x| x[col] * x[col]).sum::<f64>().sqrt())
        .collect();
    if norms.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return None;
    }
    let xs: Vec<Vec<f64>> = xs
        .into_iter()
        .map(|row| row.iter().zip(&norms).map(|(x, s)| x / s).collect())
        .collect();
    let mut ata = vec![vec![0.0; n]; n];
    let mut aty = vec![0.0; n];
    for (row, (_, y)) in xs.iter().zip(samples) {
        for a in 0..n {
            aty[a] += row[a] * y;
            for b in 0..n {
                ata[a][b] += row[a] * row[b];
            }
        }
    }
    let mut rank = ata.clone();
    for col in 0..n {
        let pivot = (col..n).max_by(|&a, &b| rank[a][col].abs().total_cmp(&rank[b][col].abs()))?;
        if rank[pivot][col].abs() < 1e-10 {
            return None;
        }
        rank.swap(pivot, col);
        let p = rank[col][col];
        for value in &mut rank[col][col..] {
            *value /= p;
        }
        let (above, below) = rank.split_at_mut(col + 1);
        let pivot_row = &above[col];
        for row in below {
            let f = row[col];
            for (value, pivot) in row[col..].iter_mut().zip(&pivot_row[col..]) {
                *value -= f * pivot;
            }
        }
    }
    let mut weights = vec![0.0_f64; n];
    let mut converged = false;
    for _ in 0..10_000 {
        let mut change = 0.0_f64;
        for col in 0..n {
            let rest: f64 = (0..n)
                .filter(|&j| j != col)
                .map(|j| ata[col][j] * weights[j])
                .sum();
            let next = ((aty[col] - rest) / ata[col][col]).max(0.0);
            change = change.max((next - weights[col]).abs());
            weights[col] = next;
        }
        if change < 1e-10 {
            converged = true;
            break;
        }
    }
    if !converged {
        return None;
    }
    let rms = (xs
        .iter()
        .zip(samples)
        .map(|(row, (_, y))| {
            (row.iter().zip(&weights).map(|(x, w)| x * w).sum::<f64>() - y).powi(2)
        })
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt();
    if !rms.is_finite() || rms > 1.0 {
        return None;
    }
    let weights: Vec<f64> = weights.iter().zip(norms).map(|(w, s)| w / s).collect();
    let mut result = BTreeMap::new();
    for (idx, family) in families.into_iter().enumerate() {
        let (a, b) = (weights[2 * idx], weights[2 * idx + 1]);
        let scale = Scale {
            tokens_per_percent: 1.0 / a,
            output_weight: b / a,
        };
        if a <= 0.0
            || b <= 0.0
            || !scale.tokens_per_percent.is_finite()
            || !scale.output_weight.is_finite()
        {
            return None;
        }
        result.insert(family, scale);
    }
    Some(result)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub(crate) struct Level {
    pub input: f64,
    pub output: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Band {
    pub lower: Level,
    pub upper: Level,
}

/// Only aggregates reach the routing document. Account/session identity stays in its owner.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderEstimate {
    pub provider: Provider,
    pub history_hours: f64,
    pub profile: Vec<f64>,
    pub level: BTreeMap<String, Level>,
    pub band: BTreeMap<String, Band>,
    pub calibrated: bool,
    #[serde(skip)]
    pub partial: bool,
}

#[derive(Debug, Default)]
struct ProviderHistory {
    hours: BTreeMap<i64, Tokens>,
    minutes: BTreeMap<i64, Tokens>,
    covered: BTreeSet<i64>,
    first: Option<i64>,
}

/// 28 days of demand, separate from ordinary usage totals and calibration. Missing files
/// contribute no coverage. Malformed, unknown or lost evidence marks the whole snapshot partial.
#[derive(Debug)]
pub(crate) struct History {
    providers: [ProviderHistory; 2],
    live_since: i64,
    newest: i64,
    pub(crate) partial: bool,
}

const fn slot(provider: Provider) -> usize {
    match provider {
        Provider::Anthropic => 0,
        Provider::Codex => 1,
    }
}

const fn week_hour(hour: i64) -> usize {
    // Unix epoch was Thursday. Monday is the first profile slot.
    (hour + 3 * 24).rem_euclid(168) as usize
}

impl History {
    pub(crate) fn new(now: i64) -> Self {
        Self {
            providers: Default::default(),
            live_since: now,
            newest: now,
            partial: false,
        }
    }

    pub(crate) fn record(&mut self, record: &UsageRecord) {
        let Some((family, value)) =
            family(record.provider, record.model.as_deref()).zip(vector(record))
        else {
            self.partial = true;
            return;
        };
        self.newest = self.newest.max(record.ts_ms);
        let hour = record.ts_ms.div_euclid(HOUR_MS);
        let oldest = self.newest.div_euclid(HOUR_MS) - HISTORY_HOURS;
        if hour < oldest {
            return;
        }
        let history = &mut self.providers[slot(record.provider)];
        history.first = Some(
            history
                .first
                .map_or(record.ts_ms, |first| first.min(record.ts_ms)),
        );
        if add(
            history
                .hours
                .entry(hour)
                .or_default()
                .entry(family.clone())
                .or_default(),
            value,
        )
        .is_none()
            || add(
                history
                    .minutes
                    .entry(record.ts_ms.div_euclid(60_000))
                    .or_default()
                    .entry(family)
                    .or_default(),
                value,
            )
            .is_none()
        {
            self.partial = true;
        }
        for provider in &mut self.providers {
            provider.hours.retain(|h, _| *h >= oldest);
            provider.covered.retain(|h| *h >= oldest);
            provider
                .minutes
                .retain(|m, _| *m >= self.newest.div_euclid(60_000) - 60);
        }
    }

    pub(crate) fn cover_day(&mut self, start: i64, end: i64) {
        for provider in &mut self.providers {
            if let Some(first) = provider.first {
                let from = start.max(first).div_euclid(HOUR_MS);
                for hour in from..end.div_euclid(HOUR_MS) {
                    if hour * HOUR_MS >= first {
                        provider.covered.insert(hour);
                    }
                }
            }
        }
    }

    pub(crate) fn estimates(&self, now: i64) -> Vec<ProviderEstimate> {
        let current = now.div_euclid(HOUR_MS);
        Provider::ALL
            .into_iter()
            .filter_map(|provider| {
                let history = &self.providers[slot(provider)];
                history.first?;
                let live = self.live_since;
                let live_start =
                    live.div_euclid(HOUR_MS) + i64::from(live.rem_euclid(HOUR_MS) != 0);
                let covered: BTreeSet<i64> = history
                    .covered
                    .iter()
                    .copied()
                    .chain(live_start..current)
                    .filter(|h| *h >= current - HISTORY_HOURS && *h < current)
                    .collect();
                if covered.is_empty() {
                    return None;
                }
                let hours = covered.len() as f64;
                let mut sums = [0.0; 168];
                let mut counts = [0.0; 168];
                let mut families = BTreeSet::new();
                for &hour in &covered {
                    let idx = week_hour(hour);
                    counts[idx] += 1.0;
                    if let Some(tokens) = history.hours.get(&hour) {
                        sums[idx] += tokens
                            .values()
                            .map(|v| v[0] as f64 + v[1] as f64)
                            .sum::<f64>();
                        families.extend(tokens.keys().cloned());
                    }
                }
                if families.is_empty() {
                    return None;
                }
                let mean = sums.iter().sum::<f64>() / hours;
                let shrink = (hours / 168.0).min(1.0);
                let mut profile: Vec<f64> = sums
                    .iter()
                    .zip(counts)
                    .map(|(sum, count)| {
                        let rate = if count > 0.0 { sum / count } else { mean };
                        if mean > 0.0 {
                            1.0 - shrink + shrink * rate / mean
                        } else {
                            1.0
                        }
                    })
                    .collect();
                let norm = profile.iter().sum::<f64>() / 168.0;
                for p in &mut profile {
                    *p /= norm;
                }
                let mut level = BTreeMap::new();
                let mut band = BTreeMap::new();
                for family in families {
                    let mut l = [0.0; 2];
                    let mut residual = [0.0; 2];
                    for (index, &hour) in covered.iter().enumerate() {
                        let value = history
                            .hours
                            .get(&hour)
                            .and_then(|v| v.get(&family))
                            .copied()
                            .unwrap_or_default();
                        let p = profile[week_hour(hour)];
                        if p <= 0.0 {
                            continue;
                        }
                        for dim in 0..2 {
                            let rate = value[dim] as f64 / p;
                            if index == 0 {
                                l[dim] = rate;
                            } else {
                                residual[dim] =
                                    0.75 * residual[dim] + 0.25 * (rate - l[dim]).powi(2);
                                l[dim] = 0.75 * l[dim] + 0.25 * rate;
                            }
                        }
                    }
                    // The existing minute granularity supplies the most recent complete hour.
                    // Use it only when all 60 minutes are covered by this process or retained days.
                    let minute = now.div_euclid(60_000);
                    let recent_known = (minute - 60..minute)
                        .all(|m| covered.contains(&m.div_euclid(60)) || m * 60_000 >= live);
                    if recent_known {
                        let mut recent = [0.0; 2];
                        for (m, tokens) in history.minutes.range(minute - 60..minute) {
                            if let Some(value) = tokens.get(&family) {
                                let p = profile[week_hour(m.div_euclid(60))];
                                if p > 0.0 {
                                    for dim in 0..2 {
                                        recent[dim] += value[dim] as f64 / p;
                                    }
                                }
                            }
                        }
                        // At an hour boundary that same hour already entered the EWMA.
                        if now.rem_euclid(HOUR_MS) >= 60_000 {
                            for dim in 0..2 {
                                l[dim] = 0.75 * l[dim] + 0.25 * recent[dim];
                            }
                        }
                    }
                    let mut lower = [0.0; 2];
                    let mut upper = [0.0; 2];
                    for dim in 0..2 {
                        let s = residual[dim]
                            .sqrt()
                            .max(l[dim] / hours.max(1.0).sqrt())
                            .max(if hours < 168.0 { l[dim] } else { 0.0 });
                        lower[dim] = (l[dim] - 2.0 * s).max(0.0);
                        upper[dim] = l[dim] + 2.0 * s;
                    }
                    level.insert(
                        family.clone(),
                        Level {
                            input: l[0],
                            output: l[1],
                        },
                    );
                    band.insert(
                        family,
                        Band {
                            lower: Level {
                                input: lower[0],
                                output: lower[1],
                            },
                            upper: Level {
                                input: upper[0],
                                output: upper[1],
                            },
                        },
                    );
                }
                Some(ProviderEstimate {
                    provider,
                    history_hours: hours,
                    profile,
                    level,
                    band,
                    calibrated: false,
                    partial: self.partial,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod record_clock_tests {
    #[test]
    fn forecast_live_history_uses_the_explicit_record_clock() {
        let tracker = crate::usage::UsageTracker::new(1, crate::pricing::PricingTable::default());
        let at = 1_767_225_600_000_i64;
        assert!(tracker.forecast_estimates(at).is_empty());
        tracker.record(
            0,
            &crate::usage::UsageRecord::from_quota_input(
                at,
                Some("claude-fable-5".into()),
                None,
                100,
                0,
                0,
                0,
                25,
            ),
            "alice@example.com",
            None,
            None,
        );
        assert!(
            tracker.forecast_estimates(at + 3_600_000 - 1).is_empty(),
            "no full observed hour yet"
        );
        let estimates = tracker.forecast_estimates(at + 3_600_000);
        assert!(
            !estimates.is_empty(),
            "explicit record clock must yield demand estimates"
        );
        assert_eq!(estimates[0].history_hours, 1.0);
        assert!(estimates[0].level["fable"].input > 0.0);
    }
}

#[cfg(test)]
mod restored_forecast_call_tests {
    use crate::config::{Account, Config};
    use crate::manager::Manager;
    use std::io::Write as _;

    #[test]
    fn restored_demand_and_live_calibration_add_zero_upstream_calls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let ledger = tempfile::tempdir().unwrap();
        let now = time::OffsetDateTime::now_utc();
        let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
        for back in 1..=48 {
            let ts = now_ms - back * 3_600_000;
            let day = crate::usage::date_string(crate::usage::ledger_date(ts));
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(ledger.path().join(format!("{day}.jsonl")))
                .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"t":ts,"a":"alice@example.com",
                "m":"claude-fable-5","i":100,"c5":0,"c1":0,"r":500,"o":25})
            )
            .unwrap();
        }
        let mut config = Config::fixture(vec![Account::fixture("alice@example.com")]);
        config.upstream = format!("http://{address}");
        config.codex_upstream = format!("http://{address}");
        let manager = Manager::with_live_refresher(config, None);
        manager.attach_usage_ledger(ledger.path().to_path_buf(), 29);
        let cold = crate::routing::document(&manager, now).unwrap().forecast;
        let reset = now_ms + 3_600_000;
        for step in 0..=24 {
            let at = now + time::Duration::seconds(step * 2);
            if step != 0 {
                let (input, output) = if step % 2 == 0 { (100, 0) } else { (0, 25) };
                manager.record_usage(
                    0,
                    crate::usage::UsageRecord::from_quota_input(
                        (at.unix_timestamp_nanos() / 1_000_000) as i64 - 1,
                        Some("claude-fable-5".into()),
                        None,
                        input,
                        0,
                        0,
                        0,
                        output,
                    ),
                );
            }
            let bucket = Some(crate::probe::UsageBucket {
                utilization: Some((step as f64 + 10.001) / 100.0),
                reset_at_ms: Some(reset),
            });
            manager.apply_usage_at(
                0,
                &crate::probe::Usage {
                    five_hour: bucket,
                    seven_day: bucket,
                    seven_day_oi: bucket,
                    ..Default::default()
                },
                at,
            );
        }
        let warm = crate::routing::document(&manager, now + time::Duration::seconds(50))
            .unwrap()
            .forecast;
        let shutdown = manager.shutdown_usage_ledger(std::time::Duration::from_secs(2));
        assert_eq!(shutdown, crate::usage::LedgerShutdown::Flushed);
        let cold = cold.expect("restored observed demand produces estimates");
        assert_eq!(
            cold["providers"][0]["calibrated"], false,
            "boot restores demand only"
        );
        assert!(cold["providers"][0]["historyHours"].as_f64().unwrap() >= 24.0);
        let warm = warm.expect("calibrated demand stays visible");
        assert_eq!(
            warm["providers"][0]["calibrated"], true,
            "public ticks reach every window fit"
        );
        assert!(warm["task"].is_null() && warm["bestProvider"].is_null());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "restored estimates and fitting add no upstream request"
        );
        listener.set_nonblocking(false).unwrap();
        let _control =
            std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2))
                .unwrap();
        assert!(
            listener.accept().is_ok(),
            "the same listener detects a connection"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageRecord;
    use time::OffsetDateTime;

    fn record(at: i64, family: &str, input: u64, output: u64) -> UsageRecord {
        UsageRecord::from_quota_input(
            at,
            Some(format!("claude-{family}-5")),
            None,
            input + 600,
            input / 4,
            input / 4,
            600,
            output,
        )
    }

    fn buckets(percent: f64, reset: i64) -> [Option<crate::probe::UsageBucket>; 3] {
        [Some(crate::probe::UsageBucket {
            utilization: Some((percent + 0.001) / 100.0),
            reset_at_ms: Some(reset),
        }); 3]
    }

    fn train(families: &[&str]) -> Calibration {
        let mut c = Calibration::default();
        let mut now = 1_000_000;
        let mut percent = 10.0;
        let mut scoped = 10.0;
        c.observe(buckets(percent, 10_000_000), now, false);
        for _ in 0..12 {
            for family in families {
                for (input, output) in [(100, 0), (0, 25)] {
                    now += 1000;
                    c.record(&record(now - 1, family, input, output));
                    percent += 1.0;
                    if *family == "fable" {
                        scoped += 1.0;
                    }
                    let mut ticks = buckets(percent, 10_000_000);
                    ticks[2] = buckets(scoped, 10_000_000)[2];
                    c.observe(ticks, now, false);
                }
            }
        }
        c
    }

    #[test]
    fn forecast_fits_every_window_and_family_without_charging_reads_or_reasoning_twice() {
        let c = train(&crate::model::FAMILIES);
        for window in 0..3 {
            for family in crate::model::FAMILIES {
                let scale = c.scale(window, family);
                if window == 2 && family != "fable" {
                    assert!(scale.is_none(), "scoped window fits only fable");
                    continue;
                }
                let scale = scale.expect("full-rank sufficient evidence");
                assert!((scale.tokens_per_percent - 100.0).abs() < 2.0, "{scale:?}");
                assert!((scale.output_weight - 4.0).abs() < 0.1, "{scale:?}");
            }
        }
        let mut codex = Calibration::default();
        let mut percent = 0.0;
        codex.observe(buckets(percent, 10_000_000), 0, false);
        for n in 1..=24 {
            let mut r = record(
                n * 1000 - 1,
                "unused",
                if n % 2 == 0 { 100 } else { 0 },
                if n % 2 == 0 { 0 } else { 25 },
            );
            r.provider = Provider::Codex;
            r.model = Some("gpt-example".into());
            r.reasoning = r.output;
            codex.record(&r);
            percent += 1.0;
            codex.observe(buckets(percent, 10_000_000), n * 1000, false);
        }
        assert!((codex.scale(0, "gpt-example").unwrap().output_weight - 4.0).abs() < 0.1);
    }

    #[test]
    fn forecast_stays_cold_for_rank_deficiency_and_bad_or_unaligned_evidence() {
        let mut c = Calibration::default();
        c.observe(buckets(0.0, 10_000_000), 0, false);
        for n in 1..=50 {
            c.record(&record(n * 1000 - 1, "fable", 100, 25));
            c.observe(buckets(n as f64 * 2.0, 10_000_000), n * 1000, false);
        }
        assert!(
            c.scale(0, "fable").is_none(),
            "collinear input/output cannot identify two weights"
        );
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            let mut c = train(&["fable"]);
            c.observe(buckets(bad, 10_000_000), 2_000_000, false);
            assert!(c.scale(0, "fable").is_none());
        }
        let mut c = train(&["fable"]);
        c.observe(buckets(80.0, 10_000_000), 2_000_000, false);
        assert!(
            c.scale(0, "fable").is_none(),
            "unexplained percent is not a fit"
        );
        let mut c = train(&["fable"]);
        c.observe(buckets(40.0, 10_000_000), 2_000_000, true);
        assert!(
            c.scale(0, "fable").is_none(),
            "unfinished overlap cannot bracket exact usage"
        );
        let mut c = train(&["fable"]);
        c.record(&record(0, "fable", 100, 0));
        assert!(
            c.scale(0, "fable").is_none(),
            "late records invalidate alignment"
        );
        let mut c = train(&["fable"]);
        let mut missing = record(2_000_000, "fable", 100, 0);
        missing.model = None;
        c.record(&missing);
        assert!(
            c.scale(0, "fable").is_none(),
            "unclassified tokens cannot disappear"
        );
    }

    #[test]
    fn forecast_never_pairs_across_reset_missing_or_decreasing_readings() {
        let mut c = Calibration::default();
        c.observe(buckets(10.0, 5000), 1000, false);
        c.record(&record(2000, "fable", 100, 0));
        c.observe(buckets(11.0, 20_000), 6000, false);
        assert_eq!(c.windows[0].samples.len(), 0);
        c.record(&record(7000, "fable", 100, 0));
        c.observe([None; 3], 8000, false);
        c.observe(buckets(12.0, 20_000), 9000, false);
        assert_eq!(c.windows[0].samples.len(), 0);
        c.record(&record(10_000, "fable", 100, 0));
        c.observe(buckets(1.0, 20_000), 11_000, false);
        assert_eq!(c.windows[0].samples.len(), 0);
    }

    #[test]
    fn forecast_profile_is_monday_first_normalized_shrunk_and_banded() {
        let start = OffsetDateTime::parse(
            "2026-09-28T00:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
        .unix_timestamp()
            * 1000;
        let mut h = History::new(start);
        for hour in 0..672 {
            let input = if hour % 168 == 0 { 200 } else { 100 };
            h.record(&record(start + hour * HOUR_MS + 1, "opus", input, 25));
        }
        let now = start + 672 * HOUR_MS;
        let e = h.estimates(now).remove(0);
        assert_eq!(e.history_hours, 672.0);
        assert!((e.profile.iter().sum::<f64>() - 168.0).abs() < 1e-8);
        assert!((e.profile[0] / e.profile[1] - 225.0 / 125.0).abs() < 1e-8);
        let level = e.level.get("opus").unwrap();
        let band = e.band.get("opus").unwrap();
        assert!(band.lower.input <= level.input && band.upper.input >= level.input);
        let mut thin = History::new(start);
        thin.record(&record(start + 1, "opus", 100, 25));
        assert!(
            thin.estimates(start + HOUR_MS - 1).is_empty(),
            "no full hour yet"
        );
        let e = thin.estimates(start + HOUR_MS).remove(0);
        assert!(e.band["opus"].upper.input >= 3.0 * e.level["opus"].input);
        assert_eq!(e.band["opus"].lower.input, 0.0);
        assert!(e.profile.iter().all(|p| (*p - 1.0).abs() < 1e-8));
    }
}

#[cfg(test)]
mod public_forecast_tests {
    use crate::{
        config::{Account, Config},
        manager::Manager,
    };

    #[test]
    fn forecast_learning_and_reads_add_zero_upstream_calls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = Config::fixture(vec![Account::fixture("alice@example.com")]);
        config.upstream = format!("http://{address}");
        config.codex_upstream = format!("http://{address}");
        let manager = Manager::with_live_refresher(config, None);
        let now = time::OffsetDateTime::now_utc();
        for step in 0..30 {
            let at = now + time::Duration::seconds(step);
            let bucket = Some(crate::probe::UsageBucket {
                utilization: Some(step as f64 / 100.0),
                reset_at_ms: Some(crate::now_ms() + 3_600_000),
            });
            manager.apply_usage_at(
                0,
                &crate::probe::Usage {
                    five_hour: bucket,
                    seven_day: bucket,
                    seven_day_oi: bucket,
                    ..Default::default()
                },
                at,
            );
            manager.record_usage(
                0,
                crate::usage::UsageRecord::from_quota_input(
                    (at.unix_timestamp_nanos() / 1_000_000) as i64 + 1,
                    Some("claude-fable-5".into()),
                    None,
                    100,
                    0,
                    0,
                    0,
                    25,
                ),
            );
            assert!(crate::routing::document(&manager, at).is_some());
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "learning/observing never calls upstream"
        );
        listener.set_nonblocking(false).unwrap();
        let _control =
            std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2))
                .unwrap();
        assert!(
            listener.accept().is_ok(),
            "the same listener detects a real connection"
        );
    }
}
