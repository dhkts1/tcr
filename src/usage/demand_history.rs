//! The 28-day demand history the forecast reads: per-hour and per-minute token vectors
//! by model family, built from the ledger's records and from the day files at boot. The
//! ledger owns demand, so the history lives beside it and `forecast` reads it, never the
//! other way round. The helpers at the top are shared with `forecast`'s calibration.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::UsageRecord;
use crate::provider::Provider;

pub(crate) const HOUR_MS: i64 = 3_600_000;
const HISTORY_HOURS: i64 = 672;
pub(crate) type Tokens = BTreeMap<String, [u64; 2]>;

pub(crate) fn family(provider: Provider, model: Option<&str>) -> Option<String> {
    let model = model.filter(|model| !model.is_empty())?;
    match provider {
        Provider::Anthropic => crate::model::family(model).map(str::to_owned),
        Provider::Codex => Some(model.to_owned()),
    }
}

pub(crate) fn vector(record: &UsageRecord) -> Option<[u64; 2]> {
    let input = record
        .input
        .checked_add(record.cache_5m)?
        .checked_add(record.cache_1h)?;
    (input.checked_add(record.cache_read)? == record.quota_input
        && record.reasoning <= record.output)
        .then_some([input, record.output])
}

pub(crate) fn add(target: &mut [u64; 2], value: [u64; 2]) -> Option<()> {
    let next = [
        target[0].checked_add(value[0])?,
        target[1].checked_add(value[1])?,
    ];
    *target = next;
    Some(())
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
#[derive(Debug, Default)]
pub(crate) struct History {
    providers: [ProviderHistory; 2],
    live_since: Option<i64>,
    newest: Option<i64>,
    /// The hour and minute of `newest` the maps were last pruned to. Pruning walks every
    /// map and ran on every request (3-24 µs each, against 0.2 µs without it), but its
    /// cutoffs move only when `newest` enters a new hour or minute.
    pruned: Option<(i64, i64)>,
    /// Sticky: once any evidence is lost the snapshot stays partial until a boot replay
    /// builds a new `History`. Nothing clears it in place.
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
            live_since: Some(now),
            newest: Some(now),
            pruned: None,
            partial: false,
        }
    }

    /// Evidence was lost: the snapshot is partial from here until a new `History` is built.
    pub(crate) const fn mark_partial(&mut self) {
        self.partial = true;
    }

    pub(crate) fn record(&mut self, record: &UsageRecord) {
        let Some((family, value)) =
            family(record.provider, record.model.as_deref()).zip(vector(record))
        else {
            self.partial = true;
            return;
        };
        // An empty tracker has no observed clock yet. Seed it from the first
        // exact record, so explicit-time callers never inherit wall time.
        self.live_since.get_or_insert(record.ts_ms);
        let newest = self.newest.get_or_insert(record.ts_ms);
        *newest = (*newest).max(record.ts_ms);
        let newest = *newest;
        let hour = record.ts_ms.div_euclid(HOUR_MS);
        let oldest = newest.div_euclid(HOUR_MS) - HISTORY_HOURS;
        let newest_minute = newest.div_euclid(60_000);
        self.prune(newest.div_euclid(HOUR_MS), newest_minute);
        if hour < oldest {
            return;
        }
        let minute = record.ts_ms.div_euclid(60_000);
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
            // A minute already behind the retained hour is not kept. Such a minute
            // has no entry yet (pruning removed it), so skipping the add loses no
            // overflow that would have marked the snapshot partial.
            || (minute >= newest_minute - 60
                && add(
                    history
                        .minutes
                        .entry(minute)
                        .or_default()
                        .entry(family)
                        .or_default(),
                    value,
                )
                .is_none())
        {
            self.partial = true;
        }
    }

    /// Drop hours and covered hours older than [`HISTORY_HOURS`] before `newest_hour`, and
    /// minutes older than an hour before `newest_minute`. Runs only when either moved:
    /// `record` and `cover_day` never insert behind the cutoffs, so between moves there
    /// is nothing to drop.
    fn prune(&mut self, newest_hour: i64, newest_minute: i64) {
        let (last_hour, last_minute) = self.pruned.unwrap_or((i64::MIN, i64::MIN));
        if newest_hour != last_hour {
            let oldest = newest_hour - HISTORY_HOURS;
            for provider in &mut self.providers {
                provider.hours.retain(|h, _| *h >= oldest);
                provider.covered.retain(|h| *h >= oldest);
            }
        }
        if newest_minute != last_minute {
            for provider in &mut self.providers {
                provider.minutes.retain(|m, _| *m >= newest_minute - 60);
            }
        }
        self.pruned = Some((newest_hour, newest_minute));
    }

    pub(crate) fn cover_day(&mut self, start: i64, end: i64) {
        let oldest = self.newest.map_or(i64::MIN, |newest| {
            newest.div_euclid(HOUR_MS) - HISTORY_HOURS
        });
        for provider in &mut self.providers {
            if let Some(first) = provider.first {
                let from = start.max(first).div_euclid(HOUR_MS).max(oldest);
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
                let live = self.live_since?;
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
mod tests {
    use super::*;

    /// Pruning only when the newest hour or minute moves keeps exactly the maps that
    /// pruning after every record kept: out-of-order, too-old and same-minute records,
    /// and covered days reaching behind the window, all included.
    #[test]
    fn forecast_history_prunes_on_bucket_moves_exactly_as_on_every_record() {
        fn every_record(history: &mut History) {
            let newest = history.newest.unwrap_or(i64::MIN);
            let oldest = newest.div_euclid(HOUR_MS) - HISTORY_HOURS;
            for provider in &mut history.providers {
                provider.hours.retain(|h, _| *h >= oldest);
                provider.covered.retain(|h| *h >= oldest);
                provider
                    .minutes
                    .retain(|m, _| *m >= newest.div_euclid(60_000) - 60);
            }
        }
        let start = 1_767_225_600_000_i64;
        let mut history = History::new(start);
        let mut reference = History::new(start);
        let models = ["claude-fable-5", "claude-opus-5", "gpt-example"];
        // A walk that steps forward by 0.7 to 90 minutes, and every seventh record
        // reaches 10 minutes to 40 days back.
        let mut ts = start;
        // The largest hours, minutes and covered map each provider reached.
        let mut most = [[0_usize; 3]; 2];
        for i in 0..4_000_i64 {
            ts += [42_000, 61_000, 3_600_000, 5_400_000][(i % 4) as usize];
            let at = if i % 7 == 0 {
                ts - [600_000, 7_200_000, 40 * 86_400_000][(i % 3) as usize]
            } else {
                ts
            };
            let mut record = UsageRecord::from_quota_input(
                at,
                Some(models[(i % 3) as usize].into()),
                None,
                100,
                0,
                0,
                0,
                25,
            );
            record.provider = if i % 3 == 2 {
                Provider::Codex
            } else {
                Provider::Anthropic
            };
            if i % 50 == 0 {
                // Today, two weeks back, and a day straddling the 28-day cutoff.
                let back = [0, 14, 28][(i / 50 % 3) as usize] * 86_400_000;
                let day = (ts - back).div_euclid(86_400_000) * 86_400_000;
                history.cover_day(day, day + 86_400_000);
                reference.cover_day(day, day + 86_400_000);
            }
            history.record(&record);
            reference.record(&record);
            every_record(&mut reference);
            for (slot, (ours, theirs)) in history
                .providers
                .iter()
                .zip(&reference.providers)
                .enumerate()
            {
                assert_eq!(ours.hours, theirs.hours, "hours after record {i}");
                assert_eq!(ours.minutes, theirs.minutes, "minutes after record {i}");
                assert_eq!(ours.covered, theirs.covered, "covered after record {i}");
                assert_eq!(ours.first, theirs.first, "first after record {i}");
                let seen = &mut most[slot];
                *seen = [
                    seen[0].max(ours.hours.len()),
                    seen[1].max(ours.minutes.len()),
                    seen[2].max(ours.covered.len()),
                ];
            }
            assert_eq!(history.partial, reference.partial);
        }
        assert!(
            most.iter()
                .all(|[hours, minutes, covered]| *hours > 100 && *minutes > 0 && *covered > 24),
            "the walk must exercise every map of both providers: {most:?}"
        );
    }
}
