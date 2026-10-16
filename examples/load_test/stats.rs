//! The arithmetic the report rests on, kept apart from the I/O so each piece has a test.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// p50, p95, p99 and max of one series, in the series' own unit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub count: usize,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

/// Nearest-rank percentile of an ascending slice: the smallest value with at least `p`
/// percent of the series at or below it. `None` for an empty slice.
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let index = rank.clamp(1, sorted.len()) - 1;
    sorted.get(index).copied()
}

/// The four figures the report prints for a series, or `None` when nothing was measured.
pub fn summarize(values: &[f64]) -> Option<Summary> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    Some(Summary {
        count: sorted.len(),
        p50: percentile(&sorted, 50.0)?,
        p95: percentile(&sorted, 95.0)?,
        p99: percentile(&sorted, 99.0)?,
        max: *sorted.last()?,
    })
}

/// CPU seconds the proxy spent per 1,000 requests it served. `None` when it served none,
/// because a figure divided by zero requests is not a cost.
pub fn cpu_seconds_per_thousand(cpu_ms: u64, served: usize) -> Option<f64> {
    if served == 0 {
        return None;
    }
    Some(cpu_ms as f64 / 1_000.0 / served as f64 * 1_000.0)
}

/// The most events inside any window of `window_ms`, over event times in milliseconds
/// (any order). The upstream-rate check reads it: the fleet ceiling holds when no
/// one-second window carries more sends than its burst plus its rate allows.
pub fn max_in_window(times_ms: &[u64], window_ms: u64) -> usize {
    let mut sorted = times_ms.to_vec();
    sorted.sort_unstable();
    let mut best = 0;
    let mut start = 0;
    for end in 0..sorted.len() {
        while sorted[end] - sorted[start] >= window_ms {
            start += 1;
        }
        best = best.max(end - start + 1);
    }
    best
}

/// Sends a GCRA bucket of `burst` with one send per `spacing_ms` can admit inside any window
/// of `window_ms`: the burst, plus one per spacing the window spans. Arrival times at the stub
/// carry scheduling jitter, so the check reading this adds a small allowance on top.
pub fn gcra_window_bound(burst: u32, spacing_ms: u64, window_ms: u64) -> usize {
    burst as usize + window_ms.div_ceil(spacing_ms.max(1)) as usize
}

/// One frame's share of the samples it was on top of the stack for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub name: String,
    pub self_samples: u64,
    pub share: f64,
}

/// The `n` frames with the most SELF time in a folded-stack file (`a;b;c 12`, the format
/// `inferno-collapse-sample` writes): the leaf of each line is the frame that was running.
/// Ties order by name so the list is stable from run to run.
pub fn top_self_frames(folded: &str, n: usize) -> Vec<Frame> {
    let mut by_frame: HashMap<&str, u64> = HashMap::new();
    let mut total = 0u64;
    for line in folded.lines() {
        let Some((stack, count)) = line.trim_end().rsplit_once(' ') else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        let leaf = stack.rsplit(';').next().unwrap_or(stack);
        *by_frame.entry(leaf).or_default() += count;
        total += count;
    }
    rank(by_frame, total, n)
}

/// The `n` frames of module `module` (`tcr`) that most often sat nearest the leaf: each stack's
/// samples go to its deepest frame from that module. Where [`top_self_frames`] names the
/// system call a thread was in, this names the proxy code that made it.
pub fn top_module_frames(folded: &str, module: &str, n: usize) -> Vec<Frame> {
    let prefix = format!("{module}`");
    let mut by_frame: HashMap<&str, u64> = HashMap::new();
    let mut total = 0u64;
    for line in folded.lines() {
        let Some((stack, count)) = line.trim_end().rsplit_once(' ') else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        total += count;
        if let Some(frame) = stack.rsplit(';').find(|f| f.starts_with(&prefix)) {
            *by_frame.entry(frame).or_default() += count;
        }
    }
    rank(by_frame, total, n)
}

/// Every sample in a folded-stack file.
pub fn total_samples(folded: &str) -> u64 {
    folded
        .lines()
        .filter_map(|l| l.trim_end().rsplit_once(' '))
        .filter_map(|(_, count)| count.parse::<u64>().ok())
        .sum()
}

fn rank(by_frame: HashMap<&str, u64>, total: u64, n: usize) -> Vec<Frame> {
    let mut frames: Vec<(&str, u64)> = by_frame.into_iter().collect();
    frames.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    frames
        .into_iter()
        .take(n)
        .map(|(name, self_samples)| Frame {
            name: name.to_string(),
            self_samples,
            share: if total == 0 {
                0.0
            } else {
                self_samples as f64 / total as f64
            },
        })
        .collect()
}

/// `throttle_ms` of every `request usage` line in a slice of the proxy's log: the time each
/// served request spent inside the proxy's own throttle (`RequestTiming` in `src/proxy.rs`).
pub fn throttle_waits(log: &str) -> Vec<u64> {
    log.lines()
        .filter(|line| line.contains("request usage"))
        .filter_map(|line| field(line, "throttle_ms"))
        .collect()
}

/// The unsigned value of `key=` in one `tracing` fmt line.
fn field(line: &str, key: &str) -> Option<u64> {
    let marker = format!(" {key}=");
    let start = line.find(&marker)? + marker.len();
    let digits: String = line[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// A tiny deterministic generator (xorshift64*), so a phase's arrival plan is the same on
/// every run without a dependency.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.unit() * n as f64) as usize % n.max(1)
    }

    /// An exponential gap with mean `1 / rate`, in milliseconds: Poisson arrivals.
    pub fn exp_gap_ms(&mut self, rate_per_s: f64) -> f64 {
        -(1.0 - self.unit()).ln() / rate_per_s * 1_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles_of_one_to_a_hundred() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = summarize(&values).expect("a summary");
        assert_eq!(
            (s.count, s.p50, s.p95, s.p99, s.max),
            (100, 50.0, 95.0, 99.0, 100.0)
        );
    }

    #[test]
    fn percentiles_ignore_input_order_and_small_series_round_up() {
        let s = summarize(&[9.0, 1.0, 5.0]).expect("a summary");
        assert_eq!((s.p50, s.p95, s.p99, s.max), (5.0, 9.0, 9.0, 9.0));
        assert_eq!(summarize(&[]), None);
    }

    #[test]
    fn cpu_per_thousand_requests() {
        assert_eq!(cpu_seconds_per_thousand(2_500, 500), Some(5.0));
        assert_eq!(cpu_seconds_per_thousand(2_500, 0), None);
    }

    #[test]
    fn busiest_window_counts_events_less_than_a_window_apart() {
        // 0 and 1_000 are a whole window apart, so they never share one.
        let times = [5_000, 0, 100, 999, 1_000, 1_050];
        assert_eq!(max_in_window(&times, 1_000), 4);
        assert_eq!(max_in_window(&[0, 1_000, 2_000], 1_000), 1);
        assert_eq!(max_in_window(&[], 1_000), 0);
    }

    #[test]
    fn the_fleet_ceiling_admits_its_burst_plus_its_rate_in_a_second() {
        assert_eq!(gcra_window_bound(32, 100, 1_000), 42);
        assert_eq!(gcra_window_bound(24, 350, 1_000), 27);
    }

    #[test]
    fn top_frames_rank_leaves_by_self_samples() {
        let folded = "main;serve;sort 40\nmain;serve;hash 25\nmain;sort 10\nmain;serve 25\n";
        let top = top_self_frames(folded, 2);
        assert_eq!(top[0].name, "sort");
        assert_eq!(top[0].self_samples, 50);
        assert!((top[0].share - 0.5).abs() < 1e-9);
        assert_eq!(top[1].name, "hash");
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn module_frames_credit_the_deepest_own_frame() {
        let folded = concat!(
            "t;tcr`serve;tcr`log_line;libc`write 30\n",
            "t;tcr`serve;tcr`log_line;libc`lock;libc`wait 20\n",
            "t;tcr`serve;tcr`route 10\n",
            "t;libc`idle 40\n",
        );
        let top = top_module_frames(folded, "tcr", 3);
        assert_eq!(top[0].name, "tcr`log_line");
        assert_eq!(top[0].self_samples, 50);
        assert!((top[0].share - 0.5).abs() < 1e-9, "share of ALL samples");
        assert_eq!(top[1].name, "tcr`route");
        assert_eq!(top.len(), 2, "a stack with no own frame credits nothing");
        assert_eq!(total_samples(folded), 100);
    }

    #[test]
    fn throttle_waits_read_only_request_usage_lines() {
        let log = concat!(
            "2026-10-16T10:00:00Z  INFO tcr::proxy: request usage account=a wait_ms=3 throttle_ms=120 total_ms=9\n",
            "2026-10-16T10:00:00Z DEBUG tcr::proxy: request timing throttle_ms=999\n",
            "2026-10-16T10:00:01Z  INFO tcr::proxy: request usage account=b throttle_ms=0 upstream_ms=2\n",
        );
        assert_eq!(throttle_waits(log), vec![120, 0]);
    }

    #[test]
    fn the_generator_is_deterministic_and_in_range() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..1_000 {
            let u = a.unit();
            assert!((0.0..1.0).contains(&u));
            assert_eq!(u.to_bits(), b.unit().to_bits());
        }
        let mean: f64 = (0..20_000).map(|_| a.exp_gap_ms(2.0)).sum::<f64>() / 20_000.0;
        assert!((mean - 500.0).abs() < 25.0, "mean gap {mean}");
    }
}
