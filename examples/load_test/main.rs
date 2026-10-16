//! A load test of the whole proxy: the real `tcr` binary against a local fake Anthropic.
//!
//! `scripts/load-test.sh` is the entry point and `docs/performance.md` explains the numbers.
//! This program does the parts that need a clock shared with the stub:
//!
//! - `guard-check`: prove the connect guard interposes on the built `tcr`.
//! - `phase`: start the stub, a scratch-HOME proxy and the driver, run one phase, and write
//!   `<out>/<phase>.json`, the proxy's log, `lsof` output, and a `sample` profile.
//! - `report`: turn a run's phase files and folded stacks into `report.md` and `report.json`.
//! - `spread`: min, median and max of the headline figures across several runs.

mod driver;
mod harness;
mod plan;
mod report;
mod stats;
mod stub;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use clap::{Parser, Subcommand};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::plan::Phase;
use crate::report::PhaseResult;

#[derive(Parser)]
#[command(about = "Load-test the real tcr binary against a local fake Anthropic")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Prove the connect guard interposes on this tcr binary.
    GuardCheck {
        #[arg(long)]
        tcr: PathBuf,
        #[arg(long)]
        guard: PathBuf,
        #[arg(long)]
        scratch: PathBuf,
    },
    /// Run one phase end to end.
    Phase {
        #[arg(long)]
        phase: String,
        #[arg(long)]
        tcr: PathBuf,
        #[arg(long)]
        guard: PathBuf,
        /// A fresh directory for this phase's scratch HOME.
        #[arg(long)]
        scratch: PathBuf,
        /// Where the phase's results go.
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        duration: f64,
        /// The stub's time from a `/v1/messages` arrival to its last byte.
        #[arg(long, default_value_t = 0)]
        reply_ms: u64,
        /// Size of each request's system prompt, in KiB.
        #[arg(long, default_value_t = 32)]
        body_kb: usize,
        /// How long to wait for requests still in flight after the last send.
        #[arg(long, default_value_t = 90.0)]
        drain: f64,
        /// Record a `sample` profile of the proxy for the length of the phase, one stack
        /// every this many ms. At 1 ms `sample` halves the overhead phase's throughput.
        #[arg(long = "profile-ms")]
        profile: Option<u64>,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Write `report.md` and `report.json` for one run directory.
    Report {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "")]
        meta: String,
    },
    /// Min, median and max of the headline figures across run directories.
    Spread { runs: Vec<PathBuf> },
    /// Rewrite folded stacks rooted at `Thread_<id>: <name>` to root at the thread's name,
    /// so one flame graph tower covers every thread of a kind.
    MergeThreads { input: PathBuf, output: PathBuf },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Cmd::GuardCheck {
            tcr,
            guard,
            scratch,
        } => {
            harness::guard_positive_control(&tcr, &guard, &scratch)?;
            println!("connect guard: refused and logged a connection to an owned, denied listener");
            Ok(())
        }
        Cmd::Phase {
            phase,
            tcr,
            guard,
            scratch,
            out,
            duration,
            reply_ms,
            body_kb,
            drain,
            profile,
            seed,
        } => {
            let phase = Phase::parse(&phase).with_context(|| format!("unknown phase {phase:?}"))?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()?;
            let result = runtime.block_on(run_phase(PhaseArgs {
                phase,
                tcr,
                guard,
                scratch,
                out: out.clone(),
                duration,
                reply_ms,
                body_kb,
                drain,
                profile,
                seed,
            }))?;
            let path = out.join(format!("{}.json", phase.as_str()));
            std::fs::write(&path, serde_json::to_string_pretty(&result)?)?;
            let failures = result.failures();
            println!(
                "{}: sent {} served {} errors {} cpu/1k {} peak footprint {:.0} MB",
                phase.as_str(),
                result.sent,
                result.served,
                result.errors + result.unfinished,
                result
                    .cpu_s_per_1k
                    .map_or("n/a".into(), |v| format!("{v:.2} s")),
                result.peak_footprint_mb
            );
            if !failures.is_empty() {
                bail!("{} failed: {failures:#?}", phase.as_str());
            }
            Ok(())
        }
        Cmd::Report { out, meta } => write_report(&out, &meta),
        Cmd::MergeThreads { input, output } => {
            let folded = std::fs::read_to_string(&input)
                .with_context(|| format!("reading {}", input.display()))?;
            std::fs::write(&output, stats::merge_threads(&folded))?;
            Ok(())
        }
        Cmd::Spread { runs } => {
            print!("{}", spread_table(&runs)?);
            Ok(())
        }
    }
}

struct PhaseArgs {
    phase: Phase,
    tcr: PathBuf,
    guard: PathBuf,
    scratch: PathBuf,
    out: PathBuf,
    duration: f64,
    reply_ms: u64,
    body_kb: usize,
    drain: f64,
    profile: Option<u64>,
    seed: u64,
}

const MB: f64 = 1024.0 * 1024.0;

/// Rest between the warm-up and a throttled phase.
const SETTLE: Duration = Duration::from_secs(10);

async fn run_phase(a: PhaseArgs) -> anyhow::Result<PhaseResult> {
    let name = a.phase.as_str();
    std::fs::create_dir_all(&a.out)?;
    let reply = Duration::from_millis(a.reply_ms);
    let stub = stub::Stub::new(reply);
    let stub_port = stub::start(stub.clone()).await?;

    let scratch = harness::Scratch::create(&a.scratch)?;
    let now = tcr::now_ms();
    std::fs::write(
        scratch.config_path(),
        serde_json::to_string_pretty(&harness::config_json(
            stub_port,
            a.phase.throttles_off(),
            now,
        ))?,
    )?;
    harness::seed_bound_tokens(&scratch.bound_token_path(), now)?;
    // The fleet ceiling as the proxy will read it: through its own loader, from the file it
    // boots on, rather than a copy of the defaults that could drift from them.
    let booted = tcr::config::load(&scratch.config_path())?;
    let fleet_ceiling_per_s = match (
        booted.fleet_throttle.burst,
        booted.fleet_throttle.effective_min_spacing(),
    ) {
        (Some(burst), Some(spacing)) => Some(stats::gcra_window_bound(burst, spacing, 1_000)),
        _ => None,
    };

    let stderr_path = a.out.join(format!("{name}.proxy.stderr"));
    let mut proxy = harness::Proxy::spawn(&a.tcr, &scratch, &a.guard, &stderr_path)?;
    let pid = proxy.pid();
    // For the entry script's exit trap: a Ctrl-C kills this process before any Drop runs.
    std::fs::write(a.out.join(format!("{name}.proxy.pid")), pid.to_string())?;
    let port = proxy.wait_listening(Duration::from_secs(90))?;
    let boot_log = proxy.log_text()?;
    let boot_path_lines = harness::boot_path_lines(&boot_log);
    let real_home = std::env::var("HOME").unwrap_or_default();
    let live_path_mentions = if real_home.is_empty() {
        Vec::new()
    } else {
        harness::live_path_mentions(&boot_path_lines, &real_home)
    };
    let bound_tokens_restored = harness::restored_bound_tokens(&boot_log);

    let sessions = plan::sessions(a.phase);
    let driver = driver::Driver::new(port, stub.clone(), sessions, a.body_kb)?;
    driver.warm_up(sessions).await;
    // The warm-up sends one request per session at once, which empties the fleet bucket
    // (burst 32). Rest long enough for both buckets to refill (per-org: 24 x 350 ms = 8.4 s),
    // so a throttled phase starts from a rested fleet rather than the warm-up's debt.
    if !a.phase.throttles_off() {
        tokio::time::sleep(SETTLE).await;
    }
    stub.take_arrivals();

    let mut sys = System::new();
    let spid = Pid::from_u32(pid);
    let refresh = |sys: &mut System| {
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[spid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        sys.process(spid)
            .map(|p| (p.accumulated_cpu_time(), p.memory()))
    };
    let (cpu0, rss_boot) = refresh(&mut sys).context("the test proxy vanished before the phase")?;
    let load0 = System::load_average();
    let log_offset = proxy.log_text()?.len();

    // Peak RSS, sampled every 250 ms for the life of the phase.
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(rss_boot));
    let sampler = {
        let stop = stop.clone();
        let peak = peak.clone();
        std::thread::spawn(move || {
            let mut sys = System::new();
            while !stop.load(Ordering::Relaxed) {
                sys.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[spid]),
                    true,
                    ProcessRefreshKind::nothing().with_memory(),
                );
                if let Some(p) = sys.process(spid) {
                    peak.fetch_max(p.memory(), Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
    };

    let profiler = if let Some(interval_ms) = a.profile {
        let seconds = a.duration.ceil().max(1.0) as u64;
        Some(
            Command::new("/usr/bin/sample")
                .args([
                    &pid.to_string(),
                    &seconds.to_string(),
                    &interval_ms.max(1).to_string(),
                    "-mayDie",
                    "-file",
                ])
                .arg(a.out.join(format!("{name}.sample.txt")))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("starting sample")?,
        )
    } else {
        None
    };

    // The socket check runs halfway through, while traffic is flowing.
    let lsof_task = {
        let out = a.out.join(format!("{name}.lsof.txt"));
        let wait = Duration::from_secs_f64(a.duration / 2.0);
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let text = tokio::task::spawn_blocking(move || harness::lsof(pid)).await??;
            std::fs::write(&out, &text)?;
            anyhow::Ok(harness::check_sockets(&text))
        })
    };

    let started = Instant::now();
    let (records, unfinished) = match a.phase {
        Phase::Overhead => {
            driver
                .run_closed(sessions, Duration::from_secs_f64(a.duration), a.seed)
                .await
        }
        _ => {
            let schedule = plan::schedule(a.phase, a.duration, a.seed);
            driver
                .run_schedule(&schedule, Duration::from_secs_f64(a.drain))
                .await
        }
    };
    let elapsed = started.elapsed();

    let (cpu1, _) = refresh(&mut sys).context("the test proxy vanished during the phase")?;
    let load1 = System::load_average();
    stop.store(true, Ordering::Relaxed);
    if sampler.join().is_err() {
        bail!("the RSS sampler panicked");
    }
    let sockets = lsof_task.await??;
    let arrivals = stub.take_arrivals();
    let refused_connects = proxy.refused_connects()?;
    let peak_footprint_mb =
        tokio::task::spawn_blocking(move || harness::peak_footprint_mb(pid)).await??;
    if let Some(mut profiler) = profiler {
        let status = tokio::task::spawn_blocking(move || profiler.wait()).await??;
        if !status.success() {
            eprintln!("[load-test] sample exited {status} for {name}");
        }
    }
    let log = proxy.log_text()?;
    let phase_log = log
        .get(log_offset..)
        .context("the proxy's log shrank during the phase")?;
    let waits = stats::throttle_waits(phase_log);
    proxy.stop()?;
    // The whole log beside the report, written after the drain so it ends with the shutdown.
    std::fs::write(a.out.join(format!("{name}.proxy.log")), proxy.log_text()?)?;

    let served = records.iter().filter(|r| r.ok).count();
    let mut statuses = BTreeMap::new();
    for r in &records {
        let key = match (&r.error, r.status) {
            (Some(err), _) => err.clone(),
            (None, Some(status)) => status.to_string(),
            (None, None) => "no status".into(),
        };
        *statuses.entry(key).or_insert(0) += 1;
    }
    let added: Vec<f64> = records.iter().filter_map(|r| r.added_ms).collect();
    let total: Vec<f64> = records
        .iter()
        .filter(|r| r.ok)
        .map(|r| r.total_ms)
        .collect();
    let ttfb: Vec<f64> = records
        .iter()
        .filter(|r| r.ok)
        .filter_map(|r| r.ttfb_ms)
        .collect();
    let origin = arrivals.iter().min().copied();
    let arrival_ms: Vec<u64> = arrivals
        .iter()
        .map(|t| origin.map_or(0, |o| t.duration_since(o).as_millis() as u64))
        .collect();
    let cpu_ms = cpu1.saturating_sub(cpu0);
    eprintln!(
        "[load-test] {name}: {} records in {:.1} s",
        records.len(),
        elapsed.as_secs_f64()
    );
    Ok(PhaseResult {
        phase: a.phase,
        duration_s: a.duration,
        reply_ms: a.reply_ms,
        sessions,
        throttles_off: a.phase.throttles_off(),
        sent: records.len() + unfinished,
        served,
        errors: records.len() - served,
        unfinished,
        statuses,
        added_ms: stats::summarize(&added),
        ttfb_ms: stats::summarize(&ttfb),
        total_ms: stats::summarize(&total),
        throttle_ms: stats::summarize(&waits.iter().map(|&w| w as f64).collect::<Vec<_>>()),
        throttled_requests: waits.iter().filter(|&&w| w > 0).count(),
        usage_lines: waits.len(),
        served_messages: records
            .iter()
            .filter(|r| r.ok && r.kind != plan::Kind::CountTokens)
            .count(),
        served_count_tokens: records
            .iter()
            .filter(|r| r.ok && r.kind == plan::Kind::CountTokens)
            .count(),
        cpu_seconds: cpu_ms as f64 / 1_000.0,
        wall_s: elapsed.as_secs_f64(),
        cpu_s_per_1k: stats::cpu_seconds_per_thousand(cpu_ms, served),
        peak_rss_mb: peak.load(Ordering::Relaxed) as f64 / MB,
        peak_footprint_mb,
        rss_after_boot_mb: rss_boot as f64 / MB,
        load_start: [load0.one, load0.five, load0.fifteen],
        load_end: [load1.one, load1.five, load1.fifteen],
        upstream_requests: arrivals.len(),
        upstream_max_per_s: stats::max_in_window(&arrival_ms, 1_000),
        fleet_ceiling_per_s,
        bound_tokens_restored,
        unexpected_upstream_paths: stub.unexpected(),
        untagged_upstream_requests: stub.untagged(),
        refused_connects,
        sockets,
        boot_path_lines,
        live_path_mentions,
        profile_interval_ms: a.profile,
    })
}

fn write_report(out: &Path, meta: &str) -> anyhow::Result<()> {
    let mut results = Vec::new();
    let mut frames = BTreeMap::new();
    for phase in Phase::ALL {
        let path = out.join(format!("{}.json", phase.as_str()));
        if !path.exists() {
            continue;
        }
        results.push(report::read_phase(&path)?);
        let folded = out.join(format!("{}.folded", phase.as_str()));
        // No folded file is a run with `--no-profile`; an unreadable one is an error.
        if folded.exists() {
            let text = std::fs::read_to_string(&folded)?;
            frames.insert(
                phase.as_str().to_string(),
                report::Profile {
                    samples: stats::total_samples(&text),
                    self_frames: stats::top_self_frames(&text, 5),
                    tcr_frames: stats::top_module_frames(&text, "tcr", 5),
                },
            );
        }
    }
    if results.is_empty() {
        bail!("no phase results in {}", out.display());
    }
    std::fs::write(
        out.join("report.md"),
        report::markdown(&results, &frames, meta),
    )?;
    std::fs::write(
        out.join("report.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "meta": meta,
            "phases": results,
            "top_frames": frames,
        }))?,
    )?;
    println!("{}", out.join("report.md").display());
    Ok(())
}

fn spread_table(runs: &[PathBuf]) -> anyhow::Result<String> {
    let mut md = String::from(
        "Each cell is min / median / max across the runs.\n\n| phase | served | CPU s / 1k req | CPU % of a core | added ms p50 | added ms p99 | peak footprint MB | load 1m at start |\n|---|---|---|---|---|---|---|---|\n",
    );
    for phase in Phase::ALL {
        let mut rows = Vec::new();
        for run in runs {
            let path = run.join(format!("{}.json", phase.as_str()));
            if path.exists() {
                rows.push(report::read_phase(&path)?);
            }
        }
        if rows.is_empty() {
            continue;
        }
        let fig = |f: &dyn Fn(&PhaseResult) -> Option<f64>| {
            let values: Vec<f64> = rows.iter().filter_map(f).collect();
            // Counts print as counts; everything else to two decimals.
            let places = if values.iter().all(|v| v.fract() == 0.0) {
                0
            } else {
                2
            };
            report::spread(&values).map_or("n/a".into(), |(lo, mid, hi)| {
                format!("{lo:.places$} / {mid:.places$} / {hi:.places$}")
            })
        };
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            phase.as_str(),
            fig(&|r| Some(r.served as f64)),
            fig(&|r| r.cpu_s_per_1k),
            fig(&|r| (r.wall_s > 0.0).then(|| r.cpu_seconds / r.wall_s * 100.0)),
            fig(&|r| r.added_ms.map(|s| s.p50)),
            fig(&|r| r.added_ms.map(|s| s.p99)),
            fig(&|r| Some(r.peak_footprint_mb)),
            fig(&|r| Some(r.load_start[0])),
        ));
    }
    Ok(md)
}
