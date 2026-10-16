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
    /// `request usage` lines with `throttle_ms` above 0.
    pub throttled_requests: usize,
    /// `request usage` lines in the phase: one per served `/v1/messages` request.
    pub usage_lines: usize,
    /// Served `/v1/messages` requests, streamed and buffered.
    #[serde(default)]
    pub served_messages: usize,
    /// Served `count_tokens` requests, which log no `request usage` line.
    #[serde(default)]
    pub served_count_tokens: usize,
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
    /// The size each request was generated to in a big-body phase.
    #[serde(default)]
    pub big_body_bytes: Option<usize>,
    /// Bytes the proxy wrote to disk over the phase (`proc_pid_rusage`, through `sysinfo`):
    /// its log, its ledger and its cache saves.
    #[serde(default)]
    pub disk_written_bytes: Option<u64>,
    /// The proxy's thread count at the phase's end (`ps -M`).
    #[serde(default)]
    pub threads: Option<usize>,
    /// Lines the proxy logged at INFO during the phase.
    #[serde(default)]
    pub info_lines: Option<usize>,
    /// Those lines by message.
    #[serde(default)]
    pub info_messages: BTreeMap<String, usize>,
}

impl PhaseResult {
    /// CPU seconds per minute of wall time: in the idle phase, what an idle minute costs.
    pub fn cpu_s_per_minute(&self) -> Option<f64> {
        (self.wall_s > 0.0).then(|| self.cpu_seconds / self.wall_s * 60.0)
    }

    /// Megabytes the proxy wrote to disk over the phase.
    pub fn disk_written_mb(&self) -> Option<f64> {
        self.disk_written_bytes
            .map(|bytes| bytes as f64 / (1024.0 * 1024.0))
    }

    /// INFO lines logged per served request.
    pub fn info_lines_per_request(&self) -> Option<f64> {
        let lines = self.info_lines?;
        (self.served > 0).then(|| lines as f64 / self.served as f64)
    }

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
        // The idle phase sends nothing after its warm-up, so it serves nothing and logs no
        // usage line by design.
        let sends = self.phase != Phase::Idle;
        if sends && self.served == 0 {
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
        if sends && self.usage_lines == 0 {
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

/// The phase's throttle line, with its denominator named. `throttle_ms` comes from the proxy's
/// `request usage` lines, one per served `/v1/messages` request; `count_tokens` logs none, so
/// the count is smaller than `served`. With the throttles off, `throttle_send` takes no lock
/// and never sleeps (`src/manager/throttle.rs`), yet `src/proxy.rs` still times the call, so a
/// nonzero value there is the call itself being descheduled, not a wait.
pub fn throttle_line(r: &PhaseResult) -> String {
    let nonzero = r.throttled_requests;
    let lines = r.usage_lines;
    let max = r.throttle_ms.map_or(0.0, |s| s.max);
    let mut line = if r.throttles_off {
        if nonzero == 0 {
            format!("- throttle: off; `throttle_ms` was 0 on all {lines} `request usage` lines")
        } else {
            format!(
                "- throttle: off. {nonzero} of {lines} `request usage` lines still show \
                 `throttle_ms` above 0 (at most {max:.0} ms): with both buckets off the throttle \
                 call takes no lock and never sleeps, so this is the call itself being \
                 descheduled on a loaded machine, not a wait"
            )
        }
    } else {
        format!(
            "- throttle: {nonzero} of {lines} `request usage` lines waited (`throttle_ms` above 0)"
        )
    };
    line.push_str(&format!(
        "; there is one such line per served `/v1/messages` request ({} served, plus {} \
         `count_tokens`, which log none)",
        r.served_messages, r.served_count_tokens
    ));
    if lines < r.served_messages {
        line.push_str(&format!(
            "; {} more served than logged, so some lines are missing",
            r.served_messages - lines
        ));
    } else if lines > r.served_messages {
        line.push_str(&format!(
            "; {} more logged than served, so lines from outside the phase were counted",
            lines - r.served_messages
        ));
    }
    line
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
        "| phase | sent | served | errors | CPU s / 1k req | CPU s / min | disk MB written | threads | INFO lines / req | added ms p50 / p95 / p99 / max | throttle ms p50 / p95 / p99 / max | peak footprint MB | upstream max/s | load 1m start → end |"
    );
    out!(
        md,
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---:|---:|---|"
    );
    for r in results {
        out!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.0} | {} | {} → {} |",
            r.phase.as_str(),
            r.sent,
            r.served,
            r.errors + r.unfinished,
            r.cpu_s_per_1k.map_or("n/a".into(), |v| format!("{v:.2}")),
            r.cpu_s_per_minute()
                .map_or("n/a".into(), |v| format!("{v:.3}")),
            r.disk_written_mb()
                .map_or("n/a".into(), |v| format!("{v:.2}")),
            r.threads.map_or("n/a".into(), |v| v.to_string()),
            r.info_lines_per_request()
                .map_or("n/a".into(), |v| format!("{v:.2}")),
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
        if let Some(bytes) = r.big_body_bytes {
            out!(
                md,
                "- each request a generated long conversation of about {:.1} MiB; CPU per request {}",
                bytes as f64 / (1024.0 * 1024.0),
                r.cpu_s_per_1k
                    .map_or("n/a".into(), |v| format!("{v:.2} ms"))
            );
        }
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
            "- disk written {} MB; {} threads at the end; INFO lines {} ({:?})",
            r.disk_written_mb()
                .map_or("n/a".into(), |v| format!("{v:.2}")),
            r.threads.map_or("n/a".into(), |v| v.to_string()),
            r.info_lines.map_or("n/a".into(), |v| v.to_string()),
            r.info_messages
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
        out!(md, "{}", throttle_line(r));
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
    use crate::stats::summarize;

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
            served_messages: 1,
            served_count_tokens: 0,
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
            big_body_bytes: None,
            disk_written_bytes: None,
            threads: None,
            info_lines: None,
            info_messages: BTreeMap::new(),
        }
    }

    #[test]
    fn an_idle_phase_that_served_nothing_has_not_failed() {
        let mut r = clean();
        r.phase = Phase::Idle;
        r.served = 0;
        r.usage_lines = 0;
        assert!(r.failures().is_empty(), "{:?}", r.failures());
        r.phase = Phase::Steady;
        assert_eq!(r.failures().len(), 2, "{:?}", r.failures());
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

    #[test]
    fn the_throttle_line_names_its_denominator_and_what_off_means() {
        // run 3's overhead phase: throttles off, 8 nonzero values, the largest 8 ms.
        let mut r = clean();
        r.throttles_off = true;
        r.served = 64_712;
        r.served_messages = 58_315;
        r.served_count_tokens = 6_397;
        r.usage_lines = 58_315;
        r.throttled_requests = 8;
        r.throttle_ms = summarize(&[0.0, 8.0]);
        let line = throttle_line(&r);
        assert!(line.contains("off. 8 of 58315"), "{line}");
        assert!(line.contains("at most 8 ms"), "{line}");
        assert!(line.contains("not a wait"), "{line}");
        assert!(
            line.contains("58315 served, plus 6397 `count_tokens`"),
            "{line}"
        );
        assert!(!line.contains("missing"), "{line}");

        r.throttles_off = false;
        let line = throttle_line(&r);
        assert!(
            line.starts_with("- throttle: 8 of 58315 `request usage` lines waited"),
            "{line}"
        );

        r.usage_lines = 58_300;
        assert!(throttle_line(&r).contains("15 more served than logged"));

        // The other sign used to print "-16 more served than logged".
        r.usage_lines = 58_331;
        let line = throttle_line(&r);
        assert!(line.contains("16 more logged than served"), "{line}");
        assert!(!line.contains("missing"), "{line}");
    }
}
