# Performance: the load test

`scripts/load-test.sh` runs the real `tcr` binary against a local fake Anthropic, drives it at
the fleet's real rates and past them, and reports what the proxy costs per request. It writes a
flame graph for each phase. It runs by hand, on macOS; a CI gate comes later, once enough runs
show how far the numbers move on their own (see [Toward a CI gate](#toward-a-ci-gate)).

## Running it

```
scripts/load-test.sh                    # all four phases, about four minutes
scripts/load-test.sh --phases overhead  # one phase
scripts/load-test.sh --help             # every flag
```

It needs Xcode's command-line tools (`cc`, `c++filt`, and the system's `sample`, `vmmap` and
`lsof`) and the two [inferno](https://github.com/jonhoo/inferno) renderers,
`inferno-collapse-sample` and `inferno-flamegraph`. It installs nothing.

The first step builds `tcr` and the driver (`examples/load_test`) in release mode with line
tables and no stripping, in a target directory of their own (`$HOME/.cache/tcr-load-test-target`,
or `TCR_LOAD_TARGET_DIR`), so the profiling build never replaces a binary that another build or
an install reads. `--skip-build` reuses what is there.

Everything a run produces goes to one new temporary directory (or `--out DIR`), which the last
line names. Nothing in it belongs in the repository: the logs, profiles and reports carry local
paths.

| file | what it holds |
|---|---|
| `report.md`, `report.json` | the run's figures, per phase |
| `<phase>.json` | one phase's figures, as `report` and `spread` read them |
| `<phase>.svg` | the flame graph, one tower per kind of thread (`tokio-rt-worker`, `tcr-usage-ledger`, `tcr-log-writer`) |
| `<phase>.folded` | the folded stacks behind it, demangled and merged by thread name |
| `<phase>.threads.folded` | the same stacks before merging, one root per thread; `--per-thread-svg` also draws them as `<phase>.threads.svg` |
| `<phase>.sample.txt` | `sample`'s own output |
| `<phase>.proxy.log` | the test proxy's whole log |
| `<phase>.lsof.txt` | the proxy's sockets, halfway through the phase |
| `scratch/<phase>/` | the scratch HOME the proxy ran in |

To compare runs, `load_test spread <run-dir> <run-dir> ...` prints the minimum, median and maximum
of each headline figure (the binary is `<target>/release/examples/load_test`).

## The phases

Each phase starts a fresh proxy, so its CPU and memory figures belong to that phase alone. Before
every phase the bound-token map is filled to its cap (`BOUND_TOKEN_CAP`, 65,536 tokens, written
through `tcr::bound_tokens::save` so the format is the proxy's own), because the map at its cap
is where the 2026-10-15 profile found the proxy's largest cost. Every reply from the fake
Anthropic mints a new message id and thinking signature, and every session echoes its last one
on its next turn, so each `/v1/messages` request looks the map up and its reply records into it.

| phase | default length | what is sent | throttles | stub reply |
|---|---|---|---|---|
| `overhead` | 20 s | 16 sessions, each sending again the moment its last reply ends | off (`"accountThrottle": {}`, `"fleetThrottle": {}`) | at once |
| `steady` | 75 s | Poisson arrivals at 1.5 a second over 18 sessions: the fleet's five-day average | defaults | 300 ms |
| `peak` | 30 s | the steady stream, plus a burst of 36 sends inside one second every 5 s over 48 sessions: the fleet's busiest second | defaults | 300 ms |
| `runaway` | 20 s | 30 a second over 64 sessions, three times the fleet ceiling's 10 | defaults | 300 ms |

The rates come from the replay described in `src/config.rs` (`default_account_throttle`,
`default_fleet_throttle`). `steady` runs 75 s so it spans fifteen of the 5 s cache saves and one
60 s rewrite of the bound-token file at its cap. Requests are 85% streamed `/v1/messages`, 5%
buffered, 10% `count_tokens`, each with a 32 KiB system prompt (`--body-kb`) and a Claude Code
shaped `metadata.user_id`.

Before measuring, the driver sends one warm-up request per session so connections and session
pins exist. Before a throttled phase it then rests 10 s so both throttle buckets refill; without
the rest, the warm-up's own burst would be charged to the phase.

## What each number means

**CPU s / 1k req** is the headline. It is the proxy process's user plus system CPU time over the
phase, divided by the requests it served, times 1,000. The development machine's load ran from
about 50 to 222 on 14 cores while this was built, which moves every latency figure far more than any
change to the proxy would; CPU time per request moved 10% across the three runs below.

Read it beside **CPU % of a core** (CPU seconds over the phase's wall time). In `overhead` the
proxy is busy and the per-request figure is its cost per request. In `steady` and `peak` it
serves one or a few requests a second and spends most of its CPU on timers (the 5 s cache saves,
the 60 s bound-token rewrite), so CPU per request there is mostly the fixed cost spread over few
requests; the share of a core is the figure to watch.

**added ms** is what the proxy adds to a request: the client's whole request time minus the
stub's own time for the same request. The driver tags each request with a header, the stub
records when it began and finished that request, and both run in one process on one clock. In
the throttled phases it includes throttle waits, which **throttle ms** reports on their own (the
`throttle_ms` field of the proxy's `request usage` lines for the phase).

The proxy writes one `request usage` line per served `/v1/messages` request and none for
`count_tokens`, so the throttle line's count is the served `/v1/messages` requests, not
`served`; the report prints both. In `overhead` a handful of lines still show a few ms of
`throttle_ms` (8 of 58,315 in run 3 below, at most 8 ms; 3 of 79,667 in a later pass, at most
6 ms). Nothing is throttled there: with both buckets off, `Manager::throttle_send` takes no lock
and never sleeps (`src/manager/throttle.rs`), but `src/proxy.rs` still times the call, so on a
machine at load 100 and more a task descheduled inside it shows up as a millisecond or two.

**peak footprint** is the proxy's lifetime peak physical footprint, from `vmmap --summary` at
the end of the phase: the memory macOS charges the process for, compressed pages included. Each
phase has its own process, so it covers boot (loading 65,536 bound tokens) and the phase.
Resident size is in the phase detail too, but it drops when macOS compresses pages, so it reads
low on a loaded box.

**upstream max/s** is the most requests the stub received inside any one second. With throttles
on, the report prints the fleet ceiling beside it (burst plus one per spacing: 32 + 10 = 42 at
the defaults, read from the scratch config through the proxy's own loader). A phase fails if
the stub saw more than the ceiling plus 2, the allowance for arrivals bunching on a loaded box.

**load 1m** is the 1-minute load average at the phase's start and end.

**Top frames** are two lists per phase, from the folded stacks. *By self time* is the leaf of
each sampled stack, which is often a system call (`write`, `__psynch_mutexwait`). *`tcr` frames*
credits each sample to its deepest frame in the `tcr` binary, which names the code that made
that call. Both read the stacks merged by thread name; merging only renames each stack's root
frame, so the lists match what the per-thread stacks give (checked on a 20 s `overhead` pass).

### What profiling costs

`sample` suspends the process to walk every thread's stack. At its default of one stack a
millisecond, a 10 s `overhead` phase served 680 requests a second at 0.44 CPU seconds per
1,000, against about 1,750 a second at 0.35 in two runs without it (this machine, 2026-10-16,
one profiled run, load 62 to 71). So the harness samples every 10 ms (`--profile-ms`). The CPU
figures still include that smaller cost; `--no-profile` takes it out, and a CI gate should
measure without profiling. Idle threads (parked in a condition variable or `kevent`) do not
appear in the folded stacks, so a near-idle phase has only a few dozen samples.

`samply` is installed here too and was not used: attaching to a running process on macOS needs
its one-time `samply setup` codesigning step, and `sample` needs nothing.

## What keeps it away from the live install

A proxy may be serving real traffic on `127.0.0.1:3456` while this runs, with its config, its
cache and its peer listener on `0.0.0.0:7755`. The test proxy is kept from all of it, and each
run checks that it was.

- **Its own files.** The proxy starts with a cleared environment: `HOME`, `XDG_CONFIG_HOME`,
  `XDG_CACHE_HOME` and `TMPDIR` point into the run's `scratch/<phase>/`, its config is passed
  with `--config`, and nothing that names a proxy (`HTTPS_PROXY` and the rest) is inherited.
  `TCR_CLAUDE_CODE_CREDENTIALS` and `TCR_CODEX_AUTH` point at files that do not exist, which is
  how a run says this machine has no login to import. Every path the proxy logs before it
  listens is kept in the phase's `boot_path_lines`, and a phase fails if one names the
  operator's own `.config`, `.cache` or `Library`.
- **Its own port.** `--port 0`, read back from the `listening on` line; a phase fails if that is
  3456. No peers file exists in the scratch config directory, so the peer listener, its mDNS and
  its UPnP never start.
- **No connection but the stub.** The connect guard the integration tests use
  (`tests/support/connect_guard.c`) is injected with `DYLD_INSERT_LIBRARIES`. It refuses, before
  anything is sent, any connection to a non-loopback address or to port 3456, and counts it; a
  phase with one refused connection fails. Before the first phase, `load_test guard-check`
  proves the guard works on this very binary: it runs `tcr status` against a listener the
  driver owns, with that port denied, and requires the refusal to be logged and the listener to
  see nothing. Halfway through each phase `lsof -nP -a -p <pid> -i` lists the proxy's sockets,
  and a phase fails if any is not loopback, touches 3456 or 7755, or if there are none at all.
- **What reaches out is off.** The quota probe and the plan backfill call a hard-coded
  `api.anthropic.com` (`probe::USAGE_URL`, `oauth::PROFILE_URL`) and cannot be pointed at the
  stub, so `quotaProbeSeconds` is 0, and each account's plan is already in the config. The
  warmer stays at its default of off. Token refresh never comes due: the fake tokens expire in a
  year and the stub never answers 401. The proxy itself checks for no updates (`tcr update` is a
  separate command). `upstream` and `codexUpstream` both name the stub.

The guard was watched failing both ways when it was built: with a library that interposes
nothing, `guard-check` refuses to start; with `upstream` pointed at a non-loopback address, the
phase fails with every connection refused and counted.

## Results on this machine

Three full runs with the defaults, on 2026-10-16, at commit `709fe44a`: a 14-core Mac16,7 that
was busy with other work the whole time. The 1-minute load average ran between 108 and 222,
so the latency figures below say more about the machine than about the proxy. Every phase of
every run passed every check: no errors, no refused connection, only loopback sockets, the
bound-token map restored at 65,536, and the fleet ceiling held.

CPU seconds per 1,000 requests, with the load at the phase's start:

| phase | run 1 | run 2 | run 3 |
|---|---|---|---|
| overhead | 0.30 (load 222) | 0.31 (157) | 0.33 (141) |
| steady | 2.40 (210) | 2.19 (154) | 2.24 (124) |
| peak | 1.28 (188) | 1.23 (160) | 1.36 (118) |
| runaway | 1.60 (183) | 1.71 (134) | 1.83 (109) |

The spread, as `load_test spread` prints it (each cell is min / median / max across the three):

| phase | served | CPU s / 1k req | CPU % of a core | added ms p50 | added ms p99 | peak footprint MB | load 1m at start |
|---|---|---|---|---|---|---|---|
| overhead | 20067 / 23947 / 64712 | 0.30 / 0.31 / 0.33 | 31.02 / 36.24 / 105.02 | 2.44 / 7.17 / 8.45 | 35.12 / 78.35 / 91.33 | 49.50 / 50.40 / 50.60 | 141.07 / 157.06 / 222.46 |
| steady | 105 / 105 / 105 | 2.19 / 2.24 / 2.40 | 0.31 / 0.31 / 0.34 | 2.75 / 4.23 / 9.08 | 22.04 / 22.77 / 34.84 | 45.10 / 51.00 / 51.10 | 123.93 / 154.28 / 209.52 |
| peak | 257 / 257 / 257 | 1.23 / 1.28 / 1.36 | 1.05 / 1.09 / 1.17 | 1.52 / 3.93 / 7.12 | 17.67 / 21.36 / 36.26 | 50.20 / 51.70 / 52.50 | 117.73 / 160.48 / 187.82 |
| runaway | 600 / 600 / 600 | 1.60 / 1.71 / 1.83 | 1.68 / 1.79 / 1.92 | 16837.92 / 16840.67 / 16841.44 | 36435.01 / 36438.15 / 36452.46 | 104.40 / 106.90 / 108.70 | 108.51 / 133.83 / 183.29 |

What the numbers say:

- **The proxy costs about 0.3 ms of CPU per request** when it is busy (`overhead`), and that
  figure moved 10% across the three runs while the load moved from 141 to 222 and throughput
  from 1,000 to 3,200 requests a second. Throughput is the machine's, not the proxy's.
- **At the fleet's real rates the proxy is nearly idle**: a third of one percent of a core in
  `steady`, about one percent in `peak`. Its CPU per request there is mostly the fixed cost of
  its timers, spread over 105 or 257 requests.
- **The time it adds is a few milliseconds at the median** in every unthrottled phase, with a
  p99 in the tens of milliseconds on a box at load 100 and more.
- **The throttles hold.** In `peak` the 36-send bursts pass without a single throttle wait
  (`throttle_ms` was 0 for every request in all three runs), and the stub never saw more than
  40 requests in one second. In `runaway` the stub saw at most 31 in any second against the
  ceiling's 42, and the queue drained at the fleet's 10 a second: half the requests waited
  about 17 s, the last about 37 s, and none failed. That wait is the throttle working as
  designed, and it is all of the added time in that phase.
- **Memory stays near 50 MB**, boot included, and peaks at about 105 MB in `runaway`, where
  roughly 370 requests with 32 KiB bodies sit in the throttle queue when the sending stops.

What the flame graphs say:

- In `overhead` the proxy's busiest single place is **its own logging**: the mutex around the
  rolling log file (`Mutex<RollingFileAppender>::make_writer_for`) was the top `tcr` frame in all
  three runs, holding 8 to 20% of the samples, and `__psynch_mutexwait` was the top leaf frame
  (19 to 36%). Every served `/v1/messages` request writes one INFO `request usage` line under
  that lock; the log grew by about 720 bytes per such line. Headless mode also writes every line
  to stdout, under a second lock (`Stdout::lock`, 3 to 7% in two of the runs, with stdout on
  `/dev/null`). The bound-token map's lookups and inserts (`Manager::bound_token_account`,
  `Manager::record_bound_tokens`) and JSON parsing of the 32 KiB bodies
  (`serde_json::read::SliceRead::skip_to_escape`) each reached the top five in two of the three
  runs, at 3 to 7%.
- In `steady`, the log file write was the top `tcr` frame in two runs, and file opens
  (`open_c`) came next in all three: every one of them under `config::write_atomic_with`, the
  5 s cache saves, each a temporary file, a write and a rename.
- The 2026-10-15 cost is gone: no sampled stack in any phase of any run sorts the bound-token
  map on a request, with the map at its cap in every phase. One `steady` stack sorts it inside
  the once-a-minute save, which is where that sort now lives.

The harness sends the proxy's stdout to `/dev/null`, as TcrBar does (`ServerController.swift`),
so the durable log is the only file each line is written to. In early runs that captured stdout
to a file as well, `Stdout::lock` was the top `tcr` frame in `overhead`, so a proxy whose stdout
goes to a file or a slow terminal pays more than these figures.

The figures above predate the log writer queue below, which removed the first finding.

## The log writer queue

Since `perf/log-writer-queue`, the log file is written by one thread, `tcr-log-writer`, fed by a
queue (`src/file_log.rs`): a tokio worker formats its line and queues it, and takes no lock on
the file. A headless proxy whose stdout is the null device no longer formats or writes a stdout
copy at all; a terminal, a pipe or a file still gets one.

Three `overhead` runs each side, `--phases overhead`, alternating before and after, on
2026-10-16: before is `bf610b31` (main), after is the branch. Both binaries were built in one
target directory with the script's own flags. The frame shares come from each run's
`overhead.folded`: *mutex wait* is the share of samples whose leaf is `__psynch_mutexwait`, and
*log writing* the share of samples with a frame from the log's writer on the stack
(`tracing_appender`, `RollingFileAppender`, `Stdout`, `tcr::file_log`, or the `tcr-log-writer`
thread).

| run | load 1m at start | served | CPU s / 1k req | added ms p99 | mutex wait | log writing |
|---|---|---|---|---|---|---|
| before 1 | 57 | 102,867 | 0.32 | 15.5 | 16.5% | 21.7% |
| before 2 | 51 | 76,262 | 0.35 | 23.5 | 20.0% | 23.5% |
| before 3 | 45 | 39,688 | 0.34 | 80.6 | 24.5% | 25.2% |
| after 1 | 47 | 65,523 | 0.26 | 43.6 | 11.9% | 7.0% |
| after 2 | 48 | 76,267 | 0.28 | 32.7 | 11.0% | 7.2% |
| after 3 | 46 | 55,596 | 0.28 | 58.5 | 13.1% | 6.2% |

- **CPU per 1,000 requests fell from 0.32 to 0.35 to 0.26 to 0.28**, median 0.34 to 0.28, about
  18% less; the two ranges do not overlap.
- **The log's locks are gone from the profile.** Before, the waits under
  `Mutex<RollingFileAppender>::make_writer_for` (6.6 to 10.5% of samples) and `Stdout::lock`
  (3.0 to 3.5%) led every run. After, neither appears; what mutex wait remains is under the
  bound-token map (`Manager::bound_token_account`, `Manager::record_bound_tokens`) and
  `UsageTracker::record`, as before, plus 0.8% at most in the queue's own wake-up
  (`crossbeam_channel::waker::SyncWaker::notify`). The writer thread itself is 3.4 to 3.8% of
  samples.
- **Added p99 did not measurably change.** The before runs spread from 15.5 to 80.6 ms and the
  after runs from 32.7 to 58.5 ms; three runs a side on a shared machine cannot separate them,
  which is the reason [Toward a CI gate](#toward-a-ci-gate) says not to gate on latency.

The queue holds 16,384 lines and the writer is not lossy: a full queue makes the emitter wait
rather than drop a line. The queue's owner lives as long as the server and drains to disk on
every way out, a panic included; why that matters, and what each exit does, is the module doc of
`src/file_log.rs`. Because a line now reaches the file a moment after it is logged, the driver
reads a phase's starting offset only once the log has been quiet for 500 ms, and reads its
lines after the proxy has stopped and drained. Without that, the after runs above counted up to
16 warm-up lines as the phase's. A fourth run each side with the fixed driver counted exactly one
`request usage` line per served request on both, at 0.28 (after) and 0.34 (before) CPU seconds
per 1,000.

## Toward a CI gate

What three runs on a loaded machine support:

- **Gate on CPU per 1,000 requests in `overhead`, measured with `--no-profile`.** It was the
  steadiest figure (0.30 to 0.33 here); a margin of 25% over a baseline taken on the CI runner
  itself leaves room for the runner's own noise and still catches a regression the size of the
  bound-token sort (which made one function half of all busy samples).
- **Gate on the pass/fail checks in every phase**: errors, refused connections, foreign
  sockets, the bound-token count at boot, the fleet ceiling. These do not depend on load.
- **Do not gate on latency** from a shared runner. The added-time percentiles moved by a factor
  of three between runs here with no change to the proxy.
- **Take the baseline on the runner that will enforce it.** These numbers come from one
  developer machine under heavy load; a hosted macOS runner will differ. Several runs there
  first, as here, give the spread the margin has to cover.

The harness is macOS-only (`sample`, `vmmap`, `DYLD_INSERT_LIBRARIES`), so a gate belongs on the
`macos` job, or needs Linux twins of those three (`perf`, `/proc/<pid>/status`, `LD_PRELOAD`,
which the connect guard already supports).
