//! Demand estimates and cold, runtime-only quota calibration. Nothing here selects an
//! account or fetches a reading. The ledger owns demand, and the Manager supplies exact
//! completed usage vectors and quiescent readings while holding its account lock.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{
    base::provider::Provider,
    records::usage::{
        UsageRecord,
        demand_history::{Tokens, add, family, vector},
    },
};

/// Caps both a window's fit samples and an account's records pending a reading.
///
/// Not justified by a measurement, and the one taken argues for less: `fit` over a full
/// deque of four families cost 0.06 ms at 256 samples, 0.2 ms at 1024 and 3.7-6.7 ms at
/// 2048 (standalone copy, `rustc -O`, 2026-10-31), and it runs on every rising tick of
/// each of three windows under the Manager's accounts write lock. A fit needs 20 samples
/// per family. Lowering it also lowers the pending cap, which needs its own number.
const MAX_SAMPLES: usize = 2048;
/// The family whose Fable-only weekly window (`7d_oi`, the third) is calibrated.
const FABLE: &str = "fable";
/// Anthropic's unified windows, in calibration-window order: utilization and reset header.
const ANTHROPIC_TICK_HEADERS: [(&str, &str); 3] = [
    (
        "anthropic-ratelimit-unified-5h-utilization",
        "anthropic-ratelimit-unified-5h-reset",
    ),
    (
        "anthropic-ratelimit-unified-7d-utilization",
        "anthropic-ratelimit-unified-7d-reset",
    ),
    (
        "anthropic-ratelimit-unified-7d_oi-utilization",
        "anthropic-ratelimit-unified-7d_oi-reset",
    ),
];

/// Codex's two slots: used-percent and reset-at header.
const CODEX_TICK_HEADERS: [(&str, &str); 2] = [
    ("x-codex-primary-used-percent", "x-codex-primary-reset-at"),
    (
        "x-codex-secondary-used-percent",
        "x-codex-secondary-reset-at",
    ),
];

/// Whether [`header_ticks`] returns `Some` for these headers: a response that names any
/// window's utilization. Header lookups only, so a caller that will not use the ticks
/// can ask this instead of building them.
pub(crate) fn carries_ticks(provider: Provider, headers: &reqwest::header::HeaderMap) -> bool {
    match provider {
        Provider::Anthropic => ANTHROPIC_TICK_HEADERS
            .iter()
            .any(|(utilization, _)| headers.contains_key(*utilization)),
        Provider::Codex => CODEX_TICK_HEADERS
            .iter()
            .any(|(utilization, _)| headers.contains_key(*utilization)),
    }
}

/// First-hand header evidence only, projected from the quota parser's result. A cached
/// utilization or synthesized reset can still govern quota, but cannot train a fit.
pub(crate) fn header_ticks(
    provider: Provider,
    headers: &reqwest::header::HeaderMap,
    quota: &crate::records::quota::Quota,
) -> Option<[Option<crate::records::probe::UsageBucket>; 3]> {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let reported_util = |name: &str| {
        text(name)
            .and_then(|s| s.parse::<f64>().ok())
            .is_some_and(|u| u.is_finite() && u >= 0.0)
    };
    let epoch_reset = |name: &str| {
        text(name)
            .and_then(|s| s.parse::<i64>().ok())
            .and_then(|s| time::OffsetDateTime::from_unix_timestamp(s).ok())
    };
    let bucket = |window: Option<crate::records::quota::QuotaWindow>| {
        window.map(|window| crate::records::probe::UsageBucket {
            utilization: Some(window.utilization),
            reset_at_ms: window
                .reset
                .map(|reset| (reset.unix_timestamp_nanos() / 1_000_000) as i64),
        })
    };
    if !carries_ticks(provider, headers) {
        return None;
    }
    match provider {
        Provider::Anthropic => {
            let windows = [quota.five_hour, quota.seven_day, quota.seven_day_oi];
            Some(std::array::from_fn(|idx| {
                let (utilization, reset) = ANTHROPIC_TICK_HEADERS[idx];
                let window = windows[idx];
                let reset = epoch_reset(reset).or_else(|| {
                    text(reset).and_then(|s| {
                        time::OffsetDateTime::parse(
                            s,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .ok()
                    })
                });
                bucket(
                    window.filter(|w| {
                        reported_util(utilization) && reset.is_some() && w.reset == reset
                    }),
                )
            }))
        }
        Provider::Codex => {
            // The parser owns primary/secondary length mapping and same-window merging.
            // All supplied slots need explicit resets before that merged reading is exact.
            let mut supplied = 0;
            for (utilization, reset) in CODEX_TICK_HEADERS {
                if headers.contains_key(utilization) {
                    supplied += 1;
                    if !reported_util(utilization) || epoch_reset(reset).is_none() {
                        return Some([None; 3]);
                    }
                }
            }
            let Some((distinct, [five_hour, seven_day])) = quota.codex_header_readings() else {
                return Some([None; 3]);
            };
            if distinct != supplied {
                return Some([None; 3]);
            }
            Some([bucket(five_hour), bucket(seven_day), None])
        }
    }
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

    /// A reading from a request still open cannot bracket completed vectors: the fit goes, and
    /// the quiescent anchor and samples stay, so intervening response headers do not erase
    /// evidence between two valid probe readings.
    fn observe_unfinished(&mut self) {
        self.scales.clear();
    }

    /// The fitted scale for `family`, if this window's fit has one.
    fn scale(&self, family: &str) -> Option<Scale> {
        self.scales.get(family).copied()
    }

    fn observe(
        &mut self,
        bucket: Option<crate::records::probe::UsageBucket>,
        now: i64,
        tokens: Tokens,
    ) {
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
    pending: BTreeMap<i64, Tokens>,
    windows: [WindowFit; 3],
    last_reading: Option<i64>,
    /// Whether `last_reading` came on the headers of a request still open, with no other
    /// request on the account open ([`Self::observe_serving`]).
    last_serving: bool,
}

impl Calibration {
    pub(crate) fn record(&mut self, record: &UsageRecord) {
        let known = family(record.provider, record.model.as_deref()).zip(vector(record));
        let Some((family, value)) = known else {
            self.invalidate();
            return;
        };
        // A record at or before the last reading's watermark that arrives after it may be
        // in that reading's percent without being in its tokens. The one exception is a
        // serving reading's own request: it completed after the headers it was read from,
        // so a record in that same millisecond waits for the next reading.
        if self
            .last_reading
            .is_some_and(|at| record.ts_ms < at || (record.ts_ms == at && !self.last_serving))
            || add(
                self.pending
                    .entry(record.ts_ms)
                    .or_default()
                    .entry(family)
                    .or_default(),
                value,
            )
            .is_none()
        {
            self.invalidate();
        }
        if self.pending.len() > MAX_SAMPLES {
            // Dropped vectors cannot remain bracketed by an existing anchor.
            self.pending.clear();
            self.invalidate();
        }
    }

    fn invalidate(&mut self) {
        for window in &mut self.windows {
            window.invalidate();
        }
    }

    /// A finished reading taken from the headers of a request that is still open, when
    /// no other request on the account is: [`Self::observe`], and that request's own
    /// record may then share the reading's millisecond and goes to the next interval.
    pub(crate) fn observe_serving(
        &mut self,
        buckets: [Option<crate::records::probe::UsageBucket>; 3],
        now: i64,
    ) {
        self.observe(buckets, now, false);
        self.last_serving = true;
    }

    pub(crate) fn observe(
        &mut self,
        buckets: [Option<crate::records::probe::UsageBucket>; 3],
        now: i64,
        unfinished: bool,
    ) {
        if unfinished {
            for window in &mut self.windows {
                window.observe_unfinished();
            }
            return;
        }
        self.last_serving = false;
        if self.last_reading.is_some_and(|prior| now < prior) {
            self.invalidate();
            self.last_reading = Some(now);
            return;
        }
        self.last_reading = Some(now);
        // A queued reading can precede an already recorded completion. Only fold
        // records through this upper watermark; later records wait for a later
        // reading. Anchor differences retain the exclusive lower watermark.
        while let Some(entry) = self.pending.first_entry() {
            if *entry.key() > now {
                break;
            }
            for (family, value) in entry.remove() {
                if add(self.tokens.entry(family).or_default(), value).is_none() {
                    self.invalidate();
                    return;
                }
            }
        }
        for (idx, (window, bucket)) in self.windows.iter_mut().zip(buckets).enumerate() {
            let tokens = self
                .tokens
                .iter()
                .filter(|(family, _)| idx != 2 || *family == FABLE)
                .map(|(family, tokens)| (family.clone(), *tokens))
                .collect();
            window.observe(bucket, now, tokens);
        }
    }

    pub(crate) fn scale(&self, window: usize, family: &str) -> Option<Scale> {
        self.windows.get(window)?.scale(family)
    }

    pub(crate) fn ready(&self, families: &BTreeSet<String>, scoped: bool) -> bool {
        !families.is_empty()
            && families.iter().all(|family| {
                (0..if scoped && family == FABLE { 3 } else { 2 }).all(|w| {
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

#[cfg(test)]
mod response_header_tests {
    use crate::base::config::{Account, Config};
    use crate::base::provider::Provider;
    use crate::manager::Manager;

    fn headers(provider: Provider, percent: u64, start: i64) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        let fields: Vec<(String, String)> = match provider {
            Provider::Anthropic => ["5h", "7d", "7d_oi"]
                .into_iter()
                .flat_map(|window| {
                    let reset = start + if window == "5h" { 18_000 } else { 604_800 };
                    [
                        (
                            format!("anthropic-ratelimit-unified-{window}-utilization"),
                            format!("{}", percent as f64 / 100.0),
                        ),
                        (
                            format!("anthropic-ratelimit-unified-{window}-reset"),
                            reset.to_string(),
                        ),
                    ]
                })
                .collect(),
            Provider::Codex => [("primary", 10080, 604_800), ("secondary", 300, 18_000)]
                .into_iter()
                .flat_map(|(slot, minutes, seconds)| {
                    [
                        (format!("x-codex-{slot}-used-percent"), percent.to_string()),
                        (
                            format!("x-codex-{slot}-window-minutes"),
                            minutes.to_string(),
                        ),
                        (
                            format!("x-codex-{slot}-reset-at"),
                            (start + seconds).to_string(),
                        ),
                    ]
                })
                .collect(),
        };
        for (name, value) in fields {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn forecast_response_headers_calibrate_both_providers_and_reject_unfinished_or_missing_reset() {
        let mut second = Account::fixture("bob@example.com");
        second.provider = Provider::Codex;
        let mut config = Config::fixture(vec![Account::fixture("alice@example.com"), second]);
        config.upstream = "http://127.0.0.1:1".into();
        config.codex_upstream = "http://127.0.0.1:1".into();
        let manager = Manager::with_live_refresher(config, None);
        let start = time::OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
        let models = ["claude-fable-5", "gpt-example"];
        for (idx, model) in models.iter().enumerate() {
            manager.record_usage(
                idx,
                crate::records::usage::UsageRecord::from_quota_input(
                    start.unix_timestamp() * 1000,
                    Some((*model).into()),
                    None,
                    100,
                    0,
                    0,
                    0,
                    25,
                ),
            );
        }
        let now = start + time::Duration::hours(2);
        // The order the proxy runs a request in: take its in-flight guard, read the
        // response headers while it is held, then record its usage at the body's end
        // (here in the same millisecond, the tightest case) and release the guard. Each
        // reading therefore brackets every request before its own.
        for (idx, provider) in Provider::ALL.into_iter().enumerate() {
            for step in 0..=24 {
                let at = now + time::Duration::seconds(step * 2);
                let flight = manager.enter_in_flight_at(idx, at);
                manager.update_quota_serving_at(
                    &flight,
                    &headers(provider, 10 + step as u64, start.unix_timestamp()),
                    200,
                    at,
                );
                let (input, output) = if step % 2 == 0 { (100, 0) } else { (0, 25) };
                manager.record_usage(
                    idx,
                    crate::records::usage::UsageRecord::from_quota_input(
                        (at.unix_timestamp_nanos() / 1_000_000) as i64,
                        Some(models[idx].into()),
                        None,
                        input,
                        0,
                        0,
                        0,
                        output,
                    ),
                );
                drop(flight);
            }
        }
        let at = now + time::Duration::seconds(50);
        let forecast = crate::proxy::routing::document(&manager, at)
            .unwrap()
            .forecast
            .unwrap();
        assert!(
            forecast["providers"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["calibrated"] == true),
            "response-header ticks must calibrate every reported window"
        );
        // A reading on one request while ANOTHER request on the account is open.
        let other = manager.enter_in_flight_at(0, at);
        let flight = manager.enter_in_flight_at(0, at);
        manager.update_quota_serving_at(
            &flight,
            &headers(Provider::Anthropic, 35, start.unix_timestamp()),
            200,
            at,
        );
        let forecast = crate::proxy::routing::document(&manager, at)
            .unwrap()
            .forecast
            .unwrap();
        assert_eq!(
            forecast["providers"][0]["calibrated"], false,
            "unfinished requests cannot bracket exact vectors"
        );
        drop(flight);
        drop(other);
        let mut incomplete = headers(Provider::Codex, 34, start.unix_timestamp());
        incomplete.remove("x-codex-primary-reset-at");
        manager.update_quota_at(1, &incomplete, 200, at);
        let forecast = crate::proxy::routing::document(&manager, at)
            .unwrap()
            .forecast
            .unwrap();
        assert_eq!(
            forecast["providers"][1]["calibrated"], false,
            "a synthesized reset is not calibration evidence"
        );
    }
    #[test]
    fn forecast_quiescent_probes_keep_history_across_unfinished_header_readings() {
        let mut config = Config::fixture(vec![Account::fixture("alice@example.com")]);
        config.upstream = "http://127.0.0.1:1".into();
        let manager = Manager::with_live_refresher(config, None);
        let start = time::OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
        manager.record_usage(
            0,
            crate::records::usage::UsageRecord::from_quota_input(
                start.unix_timestamp() * 1000,
                Some("claude-fable-5".into()),
                None,
                100,
                0,
                0,
                0,
                25,
            ),
        );
        let now = start + time::Duration::hours(2);
        let tick = |percent: u64| {
            let bucket = |seconds| {
                Some(crate::records::probe::UsageBucket {
                    utilization: Some(percent as f64 / 100.0),
                    reset_at_ms: Some((start.unix_timestamp() + seconds) * 1000),
                })
            };
            crate::records::probe::Usage {
                five_hour: bucket(18_000).into(),
                seven_day: bucket(604_800),
                seven_day_oi: bucket(604_800),
                ..Default::default()
            }
        };
        manager.apply_usage_at(0, &tick(10), now);
        for step in 1..=24 {
            let at = now + time::Duration::seconds(step * 2);
            let flight = manager.enter_in_flight_at(0, at - time::Duration::seconds(1));
            manager.update_quota_at(
                0,
                &headers(
                    Provider::Anthropic,
                    10 + step as u64,
                    start.unix_timestamp(),
                ),
                200,
                at - time::Duration::milliseconds(2),
            );
            let (input, output) = if step % 2 == 0 { (100, 0) } else { (0, 25) };
            manager.record_usage(
                0,
                crate::records::usage::UsageRecord::from_quota_input(
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
            drop(flight);
            manager.apply_usage_at(0, &tick(10 + step as u64), at);
        }
        let forecast = crate::proxy::routing::document(&manager, now + time::Duration::seconds(50))
            .unwrap()
            .forecast
            .unwrap();
        assert_eq!(
            forecast["providers"][0]["calibrated"], true,
            "unfinished header readings must not discard quiescent probe evidence"
        );
    }
}

#[cfg(test)]
mod record_clock_tests {
    #[test]
    fn forecast_live_history_uses_the_explicit_record_clock() {
        let tracker = crate::records::usage::UsageTracker::new(
            1,
            crate::base::pricing::PricingTable::default(),
        );
        let at = 1_767_225_600_000_i64;
        assert!(tracker.forecast_estimates(at).is_empty());
        tracker.record(
            0,
            &crate::records::usage::UsageRecord::from_quota_input(
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
    use crate::base::config::{Account, Config};
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
            let day = crate::records::usage::date_string(crate::records::usage::ledger_date(ts));
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
        let cold = crate::proxy::routing::document(&manager, now)
            .unwrap()
            .forecast;
        let reset = now_ms + 3_600_000;
        for step in 0..=24 {
            let at = now + time::Duration::seconds(step * 2);
            if step != 0 {
                let (input, output) = if step % 2 == 0 { (100, 0) } else { (0, 25) };
                manager.record_usage(
                    0,
                    crate::records::usage::UsageRecord::from_quota_input(
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
            let bucket = Some(crate::records::probe::UsageBucket {
                utilization: Some((step as f64 + 10.001) / 100.0),
                reset_at_ms: Some(reset),
            });
            manager.apply_usage_at(
                0,
                &crate::records::probe::Usage {
                    five_hour: bucket.into(),
                    seven_day: bucket,
                    seven_day_oi: bucket,
                    ..Default::default()
                },
                at,
            );
        }
        let warm = crate::proxy::routing::document(&manager, now + time::Duration::seconds(50))
            .unwrap()
            .forecast;
        let shutdown = manager.shutdown_usage_ledger(std::time::Duration::from_secs(2));
        assert_eq!(shutdown, crate::records::usage::LedgerShutdown::Flushed);
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
    use crate::records::usage::{
        UsageRecord,
        demand_history::{HOUR_MS, History},
    };
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

    fn buckets(percent: f64, reset: i64) -> [Option<crate::records::probe::UsageBucket>; 3] {
        [Some(crate::records::probe::UsageBucket {
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
        let c = train(&crate::base::model::FAMILIES);
        for window in 0..3 {
            for family in crate::base::model::FAMILIES {
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
        base::config::{Account, Config},
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
            let bucket = Some(crate::records::probe::UsageBucket {
                utilization: Some(step as f64 / 100.0),
                reset_at_ms: Some(crate::now_ms() + 3_600_000),
            });
            manager.apply_usage_at(
                0,
                &crate::records::probe::Usage {
                    five_hour: bucket.into(),
                    seven_day: bucket,
                    seven_day_oi: bucket,
                    ..Default::default()
                },
                at,
            );
            manager.record_usage(
                0,
                crate::records::usage::UsageRecord::from_quota_input(
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
            assert!(crate::proxy::routing::document(&manager, at).is_some());
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

#[cfg(test)]
mod reviewer_upper_watermark {
    use crate::{
        base::config::{Account, Config},
        manager::Manager,
        records::usage::UsageRecord,
    };
    fn readiness(offset_ms: i64) -> bool {
        let mut config = Config::fixture(vec![Account::fixture("alice@example.com")]);
        config.upstream = "http://127.0.0.1:1".into();
        config.codex_upstream = "http://127.0.0.1:1".into();
        let manager = Manager::with_live_refresher(config, None);
        let start = time::OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
        manager.record_usage(
            0,
            UsageRecord::from_quota_input(
                start.unix_timestamp() * 1000,
                Some("claude-fable-5".into()),
                None,
                100,
                0,
                0,
                0,
                25,
            ),
        );
        let now = start + time::Duration::hours(2);
        let ticks = |percent: u64| {
            let b = Some(crate::records::probe::UsageBucket {
                utilization: Some((percent as f64 + 0.001) / 100.0),
                reset_at_ms: Some((start.unix_timestamp() + 18_000) * 1000),
            });
            crate::records::probe::Usage {
                five_hour: b.into(),
                seven_day: b,
                seven_day_oi: b,
                ..Default::default()
            }
        };
        manager.apply_usage_at(0, &ticks(10), now);
        let mut percent = 10;
        for n in 1..=24 {
            let reading = now + time::Duration::seconds(2 * n);
            let delta = if n % 4 == 0 { 7 } else { 1 };
            let (input, output) = if n % 2 == 0 {
                (100 * delta, 0)
            } else {
                (0, 25 * delta)
            };
            // A queued observation can acquire the account lock after a
            // completion whose timestamp is beyond that observation's upper bound.
            manager.record_usage(
                0,
                UsageRecord::from_quota_input(
                    reading.unix_timestamp() * 1000 + offset_ms,
                    Some("claude-fable-5".into()),
                    None,
                    input,
                    0,
                    0,
                    0,
                    output,
                ),
            );
            percent += delta;
            manager.apply_usage_at(0, &ticks(percent), reading);
        }
        let forecast = crate::proxy::routing::document(&manager, now + time::Duration::seconds(50))
            .unwrap()
            .forecast
            .expect("complete seeded demand is visible");
        assert!(forecast["providers"][0]["historyHours"].as_f64().unwrap() >= 2.0);
        forecast["providers"][0]["calibrated"].as_bool().unwrap()
    }
    #[test]
    fn reviewer_a_reading_cannot_include_records_after_its_upper_watermark() {
        assert!(
            readiness(-1),
            "the same full-rank vectors inside each watermark do calibrate"
        );
        assert!(
            !readiness(500),
            "records beyond a reading upper watermark cannot train an exact fit"
        );
    }
}

#[cfg(test)]
mod reviewer_pending_bound {
    use super::*;
    use crate::records::usage::UsageRecord;

    fn tick(percent: f64) -> [Option<crate::records::probe::UsageBucket>; 3] {
        [Some(crate::records::probe::UsageBucket {
            utilization: Some((percent + 0.001) / 100.0),
            reset_at_ms: Some(10_000_000_000),
        }); 3]
    }

    #[test]
    fn reviewer_overflow_invalidates_calibration_before_fresh_anchors() {
        let mut c = Calibration::default();
        c.observe(tick(10.0), 1_000, false);
        for n in 1..=24 {
            let (input, output) = if n % 2 == 0 { (100, 0) } else { (0, 25) };
            c.record(&UsageRecord::from_quota_input(
                1_000 + n * 2 - 1,
                Some("claude-fable-5".into()),
                None,
                input,
                0,
                0,
                0,
                output,
            ));
            c.observe(tick(10.0 + n as f64), 1_000 + n * 2, false);
        }
        assert!(c.windows.iter().all(|window| {
            window.anchor.is_some()
                && !window.samples.is_empty()
                && window.scales.contains_key("fable")
        }));

        for n in 1..=MAX_SAMPLES as i64 + 1 {
            c.record(&UsageRecord::from_quota_input(
                2_000 + n,
                Some("claude-fable-5".into()),
                None,
                100,
                0,
                0,
                0,
                25,
            ));
        }
        assert!(
            c.pending.is_empty()
                && c.windows.iter().all(|window| {
                    window.anchor.is_none() && window.samples.is_empty() && window.scales.is_empty()
                }),
            "an overflow drop must invalidate so no dropped interval is sampled"
        );

        let reading = 2_000 + MAX_SAMPLES as i64 + 3;
        c.record(&UsageRecord::from_quota_input(
            reading - 1,
            Some("claude-fable-5".into()),
            None,
            100,
            0,
            0,
            0,
            25,
        ));
        c.observe(tick(40.0), reading, false);
        assert!(c.pending.is_empty());
        for window in &c.windows {
            let anchor = window
                .anchor
                .as_ref()
                .expect("a fresh anchor is established");
            assert_eq!(anchor.at, reading);
            assert_eq!(anchor.percent, 40.0);
            assert_eq!(
                anchor.tokens,
                Tokens::from([("fable".into(), [1_300, 325])])
            );
            assert!(
                window.samples.is_empty() && window.scales.is_empty(),
                "an overflow drop must invalidate so no dropped interval is sampled"
            );
        }
    }

    #[test]
    fn reviewer_pending_records_stay_bounded_without_a_finished_reading() {
        let mut c = Calibration::default();
        c.observe(tick(10.0), 1_000, false);
        for n in 1..=4_096_i64 {
            c.record(&UsageRecord::from_quota_input(
                1_000 + n,
                Some("claude-fable-5".into()),
                None,
                100,
                0,
                0,
                0,
                25,
            ));
            // The account stays busy: every reading arrives with a request in flight.
            c.observe(tick(10.0), 1_000 + n, true);
        }
        let len = c.pending.len();
        assert!(
            c.pending.len() <= MAX_SAMPLES,
            "pending retained {len} completed records without a finished reading"
        );
    }
}

#[cfg(test)]
mod reviewer_lower_watermark {
    use crate::{
        base::config::{Account, Config},
        manager::Manager,
        records::usage::UsageRecord,
    };
    fn readiness(offset_ms: i64) -> bool {
        let mut config = Config::fixture(vec![Account::fixture("alice@example.com")]);
        config.upstream = "http://127.0.0.1:1".into();
        config.codex_upstream = "http://127.0.0.1:1".into();
        let manager = Manager::with_live_refresher(config, None);
        let start = time::OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
        manager.record_usage(
            0,
            UsageRecord::from_quota_input(
                start.unix_timestamp() * 1000,
                Some("claude-fable-5".into()),
                None,
                100,
                0,
                0,
                0,
                25,
            ),
        );
        let now = start + time::Duration::hours(2);
        let ticks = |percent: u64| {
            let b = Some(crate::records::probe::UsageBucket {
                utilization: Some((percent as f64 + 0.001) / 100.0),
                reset_at_ms: Some((start.unix_timestamp() + 18_000) * 1000),
            });
            crate::records::probe::Usage {
                five_hour: b.into(),
                seven_day: b,
                seven_day_oi: b,
                ..Default::default()
            }
        };
        manager.apply_usage_at(0, &ticks(10), now);
        let mut percent = 10;
        for n in 1..=24 {
            let reading = now + time::Duration::seconds(2 * n);
            let delta = if n % 4 == 0 { 7 } else { 1 };
            let (input, output) = if n % 2 == 0 {
                (100 * delta, 0)
            } else {
                (0, 25 * delta)
            };
            // A queued observation can acquire the account lock after a
            // completion whose timestamp is beyond that observation's upper bound.
            manager.record_usage(
                0,
                UsageRecord::from_quota_input(
                    reading.unix_timestamp() * 1000 + offset_ms,
                    Some("claude-fable-5".into()),
                    None,
                    input,
                    0,
                    0,
                    0,
                    output,
                ),
            );
            percent += delta;
            manager.apply_usage_at(0, &ticks(percent), reading);
        }
        let forecast = crate::proxy::routing::document(&manager, now + time::Duration::seconds(50))
            .unwrap()
            .forecast
            .expect("complete seeded demand is visible");
        assert!(forecast["providers"][0]["historyHours"].as_f64().unwrap() >= 2.0);
        forecast["providers"][0]["calibrated"].as_bool().unwrap()
    }
    #[test]
    fn reviewer_a_record_at_the_lower_watermark_cannot_enter_its_interval() {
        assert!(
            readiness(-1),
            "the same full-rank vectors inside each watermark do calibrate"
        );
        assert!(
            !readiness(-2000),
            "a record stamped at the lower reading cannot enter its interval"
        );
    }
}
