//! One phase's result as JSON, and the run's report as markdown and JSON.

use std::collections::BTreeMap;
use std::path::Path;

/// Append one formatted line to a `String`.
macro_rules! out {
    ($md:expr, $($fmt:tt)*) => {{
        $md.push_str(&format!($($fmt)*));
        $md.push('\n');
    }};
}

use serde::{Deserialize, Serialize};

use crate::harness::SocketCheck;
use crate::plan::Phase;
use crate::stats::{Frame, Summary};

/// Sends allowed over the fleet ceiling in one second before the check fails: the stub
/// timestamps arrivals, not the proxy's sends, and a loaded box can bunch a few together.
pub const CEILING_JITTER: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseResult {
    pub phase: Phase,
    pub duration_s: f64,
    pub reply_ms: u64,
    pub sessions: usize,
    /// Whether the proxy booted with `"accountThrottle": {}` and `"fleetThrottle": {}`.
    pub throttles_off: bool,
    pub sent: usize,
    pub served: usize,
    pub errors: usize,
    pub unfinished: usize,
    pub statuses: BTreeMap<String, usize>,
    /// What the proxy added: client total minus the stub's own time, in ms.
    pub added_ms: Option<Summary>,
    pub ttfb_ms: Option<Summary>,
    /// The client's whole request, first byte sent to last byte read, in ms.
    pub total_ms: Option<Summary>,
    /// `throttle_ms` from the proxy's `request usage` lines in this phase.
    pub throttle_ms: Option<Summary>,
    pub throttled_requests: usize,
    pub usage_lines: usize,
    pub cpu_seconds: f64,
    /// Wall time the CPU figure was taken over: the phase plus its drain.
    pub wall_s: f64,
    pub cpu_s_per_1k: Option<f64>,
    pub peak_rss_mb: f64,
    /// The process's lifetime peak physical footprint (`vmmap`), boot included: the memory
    /// figure to read, because resident size drops when macOS compresses pages.
    pub peak_footprint_mb: f64,
    pub rss_after_boot_mb: f64,
    pub load_start: [f64; 3],
    pub load_end: [f64; 3],
    pub upstream_requests: usize,
    pub upstream_max_per_s: usize,
    /// The most sends the fleet throttle admits in one second (burst plus rate), when on.
    pub fleet_ceiling_per_s: Option<usize>,
    pub bound_tokens_restored: Option<usize>,
    pub unexpected_upstream_paths: BTreeMap<String, usize>,
    pub untagged_upstream_requests: u64,
    pub refused_connects: usize,
    pub sockets: SocketCheck,
    pub boot_path_lines: Vec<String>,
    pub live_path_mentions: Vec<String>,
    /// The `sample` interval when the phase was profiled; the profile costs the proxy time.
    pub profile_interval_ms: Option<u64>,
}

impl PhaseResult {
    /// Every reason this phase's numbers cannot be trusted, or the fence was crossed.
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.refused_connects > 0 {
            out.push(format!(
                "the connect guard refused {} connection(s) to a non-loopback host or the live port",
                self.refused_connects
            ));
        }
        if !self.sockets.foreign.is_empty() {
            out.push(format!(
                "lsof saw non-loopback or live-port sockets: {:?}",
                self.sockets.foreign
            ));
        }
        if self.sockets.endpoints == 0 {
            out.push("lsof saw no sockets at all, so the socket check proves nothing".into());
        }
        if !self.live_path_mentions.is_empty() {
            out.push(format!(
                "boot named a live path: {:?}",
                self.live_path_mentions
            ));
        }
        if self.bound_tokens_restored != Some(tcr::bound_tokens::BOUND_TOKEN_CAP) {
            out.push(format!(
                "the bound-token map did not boot at its cap ({:?} restored)",
                self.bound_tokens_restored
            ));
        }
        if self.served == 0 {
            out.push("nothing was served".into());
        }
        if let Some(ceiling) = self.fleet_ceiling_per_s {
            if self.upstream_max_per_s > ceiling + CEILING_JITTER {
                out.push(format!(
                    "the fleet throttle did not hold: {} upstream sends in one second against a ceiling of {ceiling}",
                    self.upstream_max_per_s
                ));
            }
        }
        if self.usage_lines == 0 {
            out.push(
                "the proxy logged no `request usage` line, so it never parsed a stub reply".into(),
            );
        }
        if self.errors > 0 {
            out.push(format!(
                "{} request(s) failed: {:?}",
                self.errors, self.statuses
            ));
        }
        if !self.unexpected_upstream_paths.is_empty() {
            out.push(format!(
                "the proxy asked the stub for {:?}",
                self.unexpected_upstream_paths
            ));
        }
        if self.untagged_upstream_requests > 0 {
            out.push(format!(
                "{} upstream request(s) lost the driver's tag, so their added time is unknown",
                self.untagged_upstream_requests
            ));
        }
        out
    }
}

fn fmt_summary(s: Option<&Summary>) -> String {
    match s {
        Some(s) => format!("{:.1} / {:.1} / {:.1} / {:.1}", s.p50, s.p95, s.p99, s.max),
        None => "n/a".into(),
    }
}

fn fmt_load(l: [f64; 3]) -> String {
    format!("{:.1}", l[0])
}

/// What one phase's flame graph says, in two lists: the leaf frames (often a system call) and
/// the proxy code nearest them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub samples: u64,
    pub self_frames: Vec<Frame>,
    pub tcr_frames: Vec<Frame>,
}

/// The run's markdown report: one table of headline figures, then the detail per phase.
pub fn markdown(results: &[PhaseResult], frames: &BTreeMap<String, Profile>, meta: &str) -> String {
    let mut md = String::new();
    out!(md, "# tcr load test\n\n{meta}\n");
    out!(
        md,
        "| phase | sent | served | errors | CPU s / 1k req | added ms p50 / p95 / p99 / max | throttle ms p50 / p95 / p99 / max | peak footprint MB | upstream max/s | load 1m start → end |"
    );
    out!(md, "|---|---:|---:|---:|---:|---|---|---:|---:|---|");
    for r in results {
        out!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {:.0} | {} | {} → {} |",
            r.phase.as_str(),
            r.sent,
            r.served,
            r.errors + r.unfinished,
            r.cpu_s_per_1k.map_or("n/a".into(), |v| format!("{v:.2}")),
            fmt_summary(r.added_ms.as_ref()),
            if r.throttles_off {
                "off".into()
            } else {
                fmt_summary(r.throttle_ms.as_ref())
            },
            r.peak_footprint_mb,
            r.upstream_max_per_s,
            fmt_load(r.load_start),
            fmt_load(r.load_end),
        );
    }
    for r in results {
        out!(md, "\n## {}\n", r.phase.as_str());
        out!(
            md,
            "- {} s, {} sessions, stub reply {} ms, throttles {}{}",
            r.duration_s,
            r.sessions,
            r.reply_ms,
            if r.throttles_off {
                "off"
            } else {
                "at their defaults"
            },
            r.profile_interval_ms.map_or(String::new(), |ms| format!(
                ", profiled with `sample` every {ms} ms"
            ))
        );
        out!(
            md,
            "- sent {}, served {}, errors {}, unfinished at drain {}, statuses {:?}",
            r.sent,
            r.served,
            r.errors,
            r.unfinished,
            r.statuses
        );
        out!(
            md,
            "- proxy CPU {:.2} s over {:.1} s of wall time ({:.0}% of one core), {} s per 1,000 requests",
            r.cpu_seconds,
            r.wall_s,
            if r.wall_s > 0.0 { r.cpu_seconds / r.wall_s * 100.0 } else { 0.0 },
            r.cpu_s_per_1k.map_or("n/a".into(), |v| format!("{v:.2}")),
        );
        out!(
            md,
            "- memory: peak footprint {:.0} MB (lifetime, boot included); resident {:.0} MB after boot, {:.0} MB peak",
            r.peak_footprint_mb,
            r.rss_after_boot_mb,
            r.peak_rss_mb
        );
        out!(
            md,
            "- client ms (p50 / p95 / p99 / max): first byte {}; whole request {}",
            fmt_summary(r.ttfb_ms.as_ref()),
            fmt_summary(r.total_ms.as_ref())
        );
        out!(
            md,
            "- throttle: {} of {} served requests waited",
            r.throttled_requests,
            r.usage_lines
        );
        out!(
            md,
            "- upstream: {} requests, busiest second {}{}",
            r.upstream_requests,
            r.upstream_max_per_s,
            r.fleet_ceiling_per_s
                .map_or(String::new(), |c| format!(" (fleet ceiling admits {c})"))
        );
        out!(
            md,
            "- load average 1/5/15 min: start {:.1} {:.1} {:.1}, end {:.1} {:.1} {:.1}",
            r.load_start[0],
            r.load_start[1],
            r.load_start[2],
            r.load_end[0],
            r.load_end[1],
            r.load_end[2]
        );
        out!(
            md,
            "- fence: bound tokens restored {:?}, {} socket endpoint(s) seen by lsof, {} foreign, {} connect(s) refused by the guard",
            r.bound_tokens_restored,
            r.sockets.endpoints,
            r.sockets.foreign.len(),
            r.refused_connects
        );
        if let Some(profile) = frames.get(r.phase.as_str()) {
            for (title, list) in [
                ("Top frames by self time", &profile.self_frames),
                (
                    "Top `tcr` frames (each sample credited to its deepest `tcr` frame)",
                    &profile.tcr_frames,
                ),
            ] {
                out!(md, "\n{title}, of {} samples:\n", profile.samples);
                for f in list {
                    out!(
                        md,
                        "1. `{}`: {} samples, {:.1}%",
                        f.name,
                        f.self_samples,
                        f.share * 100.0
                    );
                }
            }
        }
        let failures = r.failures();
        if !failures.is_empty() {
            out!(md, "\n**Failed:**\n");
            for f in failures {
                out!(md, "- {f}");
            }
        }
    }
    md
}

pub fn read_phase(path: &Path) -> anyhow::Result<PhaseResult> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// Min, median and max of one figure across runs.
pub fn spread(values: &[f64]) -> Option<(f64, f64, f64)> {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let median = if v.len() % 2 == 1 {
        v[v.len() / 2]
    } else {
        (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
    };
    Some((v[0], median, v[v.len() - 1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_is_min_median_max() {
        assert_eq!(spread(&[3.0, 1.0, 2.0]), Some((1.0, 2.0, 3.0)));
        assert_eq!(spread(&[4.0, 1.0, 2.0, 3.0]), Some((1.0, 2.5, 4.0)));
        assert_eq!(spread(&[]), None);
    }

    fn clean() -> PhaseResult {
        PhaseResult {
            phase: Phase::Steady,
            duration_s: 1.0,
            reply_ms: 0,
            sessions: 1,
            throttles_off: false,
            sent: 1,
            served: 1,
            errors: 0,
            unfinished: 0,
            statuses: BTreeMap::new(),
            added_ms: None,
            ttfb_ms: None,
            total_ms: None,
            throttle_ms: None,
            throttled_requests: 0,
            usage_lines: 1,
            cpu_seconds: 0.0,
            wall_s: 1.0,
            cpu_s_per_1k: None,
            peak_rss_mb: 0.0,
            peak_footprint_mb: 0.0,
            rss_after_boot_mb: 0.0,
            load_start: [0.0; 3],
            load_end: [0.0; 3],
            upstream_requests: 1,
            upstream_max_per_s: 1,
            fleet_ceiling_per_s: Some(42),
            bound_tokens_restored: Some(tcr::bound_tokens::BOUND_TOKEN_CAP),
            unexpected_upstream_paths: BTreeMap::new(),
            untagged_upstream_requests: 0,
            refused_connects: 0,
            sockets: SocketCheck {
                endpoints: 2,
                foreign: vec![],
            },
            boot_path_lines: vec![],
            live_path_mentions: vec![],
            profile_interval_ms: None,
        }
    }

    #[test]
    fn a_clean_phase_has_no_failures_and_each_breach_is_named() {
        assert!(clean().failures().is_empty());
        let mut r = clean();
        r.refused_connects = 1;
        assert!(r.failures()[0].contains("connect guard"));
        let mut r = clean();
        r.sockets.foreign = vec!["tcr TCP 10.0.0.1:1->1.1.1.1:443".into()];
        assert!(r.failures()[0].contains("non-loopback"));
        let mut r = clean();
        r.bound_tokens_restored = Some(10);
        assert!(r.failures()[0].contains("cap"));
        let mut r = clean();
        r.usage_lines = 0;
        assert!(r.failures()[0].contains("request usage"));
        let mut r = clean();
        r.upstream_max_per_s = 42 + CEILING_JITTER;
        assert!(r.failures().is_empty(), "inside the jitter allowance");
        r.upstream_max_per_s += 1;
        assert!(r.failures()[0].contains("did not hold"));
        r.fleet_ceiling_per_s = None;
        assert!(r.failures().is_empty(), "no ceiling with throttles off");
    }
}
