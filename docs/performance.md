# Performance: the load test

`scripts/load-test.sh` runs the real `tcr` binary against a local fake Anthropic, drives it at
the fleet's real rates and past them, and reports what the proxy costs per request. It writes a
flame graph for each phase. It runs by hand, on macOS; a CI gate comes later, once enough runs
show how far the numbers move on their own (see [Toward a CI gate](#toward-a-ci-gate)).

## Running it

```
scripts/load-test.sh                    # all seven phases, about seven minutes
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
| `big1m`, `big4m` | 20 s each (`--big`) | as `overhead`, over 4 sessions, each request a generated long conversation of about 1 MiB or 4 MiB | off | at once |
| `idle` | 60 s (`--idle`) | the `steady` sessions' warm-up, then nothing: what an idle minute costs | defaults | 300 ms |

The big-body phases send what a long Claude Code session sends (`examples/load_test/conversation.rs`):
a system prompt with a cache breakpoint, 24 tools, then hundreds of turns of signed thinking, a
`Bash` tool call and about 4 KiB of tool output full of escaped quotes, backslashes and newlines,
ending in a user message with a cache breakpoint. Every word is generated. A breakpoint carries no
`ttl`, as Claude Code sends it, so the proxy's `ttl: 1h` rewrite changes every request, as it does
live. Real requests are this size: the live log's median prompt is about 365,000 tokens.

The rates come from the replay described in `src/config.rs` (`default_account_throttle`,
`default_fleet_throttle`). `steady` runs 75 s, which before `perf/idle-cost` spanned fifteen of
the 5 s cache saves and one 60 s rewrite of the bound-token file at its cap; since then a cache is
saved only when its content changed, and the bound-token file every ten minutes (see
[Threads, wake-ups and disk](#threads-wake-ups-and-disk)). Requests are 85% streamed `/v1/messages`, 5%
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
serves one or a few requests a second, so CPU per request there includes the fixed cost of its
cache saves and log writes spread over few requests; the share of a core is the figure to watch.

**CPU s / min**, **disk MB written**, **threads** and **INFO lines / req** are what the proxy
costs beside its CPU per request: CPU seconds per minute of wall time (in `idle`, the cost of an
idle minute), the bytes it wrote to disk over the phase (`proc_pid_rusage`'s count, through
`sysinfo`: its log, its ledger and its cache saves, before the shutdown write), its thread count
at the phase's end (`ps -M`), and the lines its log took at the default INFO filter per served
request, with each phase's detail listing them by message.

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

## The bound-token map and the usage buckets

With the log's locks gone, the lock waits left in `overhead` were the bound-token map's one
mutex (`Manager::record_bound_tokens`, `Manager::bound_token_account`) and the usage tracker's
(`UsageTracker::record`, which took a mutex over the ledger handle and one over every account's
buckets). Since `perf/fewer-locks`:

- the map is 16 parts by token hash, each with its own mutex and a sixteenth of the cap
  (`ShardedBoundTokens` in `src/manager/bound_tokens.rs`, whose doc says what that changes in
  eviction order and why a `DashMap` lost);
- the usage buckets have a mutex per account, and the ledger handle a read-write lock;
- the ledger's writer thread waits 2 ms before it blocks on an empty queue. Without that, the
  request threads, no longer queued behind the handle's mutex, met at the channel's own waker
  mutex instead (`SyncWaker::notify`, up to 5% of samples in a run without it).

The account-choice locks under `Manager` (affinity pins, group selection, the session wire) are
untouched: they keep a conversation on one account, which is what keeps its cache warm.

Three `overhead` runs each side, alternating before and after, on 2026-10-16. Before is `ec199f65`
(main), after is the branch rebased on it. The lock columns count samples whose leaf is a mutex
wait, by the deepest `tcr` frame above it, out of all samples.

| run | load 1m at start | served | CPU s / 1k req | added ms p99 | any lock wait | map | usage record | std channel notify |
|---|---|---|---|---|---|---|---|---|
| before 1 | 75 | 74,193 | 0.24 | 30.3 | 16.3% | 62 | 32 | 0 |
| before 2 | 64 | 25,982 | 0.26 | 122.3 | 20.5% | 139 | 94 | 2 |
| before 3 | 78 | 26,607 | 0.26 | 137.5 | 21.9% | 207 | 25 | 1 |
| after 1 | 69 | 47,436 | 0.23 | 40.7 | 3.9% | 1 | 0 | 0 |
| after 2 | 64 | 27,805 | 0.25 | 77.9 | 10.9% | 35 | 1 | 1 |
| after 3 | 86 | 71,350 | 0.24 | 33.1 | 6.5% | 20 | 0 | 0 |

- **Lock wait fell from 16 to 22% of samples to 4 to 11%.** The map's waits went from 62 to 207
  samples a run to 1 to 35, and the usage tracker's from 25 to 94 to at most 1.
- **CPU per 1,000 requests moved from a median of 0.26 to 0.24**, with the ranges touching (0.24
  to 0.26 before, 0.23 to 0.25 after), so this is a small gain at best on a machine at load 64
  to 86.
- **Added p99 cannot be read from these runs**: it spread from 30 to 138 ms before and 33 to
  78 ms after.
- What waits now is mostly the account-choice locks left alone above, the map's own parts
  (`ShardedBoundTokens::shard`, 20 to 35 samples in two runs), and the log queue's waker
  (`crossbeam_channel::waker::SyncWaker::notify`, up to 33 samples), the same pattern the usage
  ledger had.

## One parse per request body

Since `perf/parse-once`, a `/v1/messages` body is read once (`src/request_body.rs`). Before it,
seven places read it on their own, and at a multi-megabyte conversation those passes were most of
what tcr spent on a request.

What one Claude Code request with a cache breakpoint (the rewrite changes it) cost in passes over
the whole body, counted from the code; "parse" is a `serde_json` walk, "scan" a plain byte loop,
"copy" a new buffer the size of the body:

| reader | before | after |
|---|---|---|
| collect the body (`to_bytes`) | 1 copy | 1 copy |
| `ttl: 1h` rewrite (`cache_ttl::extend_all_ttls`) | 2 scans (the `"cache_control"` pre-check, UTF-8), 1 parse, 1 copy | in the one walk; 1 UTF-8 scan, 1 copy |
| model (`model::parse_request_model`) | 1 parse | in the one walk |
| session peek (`metadata`, thread ids) | 1 parse | in the one walk |
| tool events (`session_wire::extract_tool_events`) | 1 parse of `messages` | in the one walk |
| bound tokens (`session_wire::extract_bound_tokens`) | 3 parses of `messages` | in the one walk |
| affinity key (`stable_session_key`) | 1 parse | in the one walk |
| extended-window answer (`cache_ttl::requests_extended_ttl`) | 1 parse that buffered every message's content into serde's untagged-enum tree: a second copy, in pieces | in the one walk |
| account-uuid patch, per attempt (`account_uuid::patch_account_uuid`) | 1 parse, 1 copy when the uuid differs | the offset from the walk; 1 copy when the uuid differs |
| total | 11 parses or scans, 4 copies | 1 parse, 1 UTF-8 scan, 3 copies |

`system` and `tools` are read twice in the one walk (borrowed, then walked for breakpoints); they
are a few percent of a long conversation. A body the rewrite does not change is never copied after
it is collected. The pinned-exit carry in `src/peer/egress.rs` still patches the uuid with its own
parse; it runs only for an account locked to one Mac.

Each answer is the one its old parse gave, including when that parse failed, on the body as
forwarded: `request_body`'s tests run the old functions beside the walk on a Claude Code shaped
conversation and 61 edge cases, rewrite off and on. The two shapes where it differs, a top-level
JSON array and a body that is not UTF-8, are both bodies Anthropic rejects; the module doc names
them.

Three runs each side, alternating, `--phases overhead,big1m,big4m --overhead 20 --big 20`, on
2026-10-16 at a 1-minute load of 66 to 147 on 14 cores. Before is `fcac23b9` (main) with the
big-body phases added, after is the branch; both built with the script's flags, driven by one
driver binary.

| phase | | CPU ms / request | added ms p50 | added ms p99 | peak footprint MB | served in 20 s |
|---|---|---|---|---|---|---|
| `big1m` | before | 6.61 / 6.62 / 6.64 | 24.4 / 28.7 / 32.5 | 241 / 275 / 407 | 85.6 / 90.5 / 92.6 | 1,200 / 1,499 / 1,647 |
| `big1m` | after | 1.55 / 1.57 / 1.57 | 8.9 / 9.9 / 11.1 | 104 / 108 / 216 | 77.3 / 81.3 / 87.8 | 3,139 / 4,405 / 4,521 |
| `big4m` | before | 25.12 / 25.80 / 25.93 | 123 / 155 / 169 | 529 / 582 / 713 | 108.4 / 115.3 / 125.4 | 389 / 404 / 492 |
| `big4m` | after | 4.95 / 4.96 / 5.03 | 22.2 / 27.8 / 29.1 | 175 / 213 / 260 | 103.1 / 106.5 / 116.0 | 1,447 / 1,746 / 1,991 |
| `overhead` | before | 0.25 / 0.25 / 0.25 | 3.5 / 5.4 / 6.1 | 69 / 100 / 144 | 55.4 / 56.5 / 57.2 | 24,089 / 29,705 / 35,224 |
| `overhead` | after | 0.23 / 0.23 / 0.24 | 2.7 / 3.6 / 7.1 | 75 / 85 / 119 | 55.5 / 56.2 / 56.4 | 26,361 / 37,848 / 37,872 |

Each cell is min / median / max across the three runs.

- **CPU per request fell by about three quarters at 1 MiB and four fifths at 4 MiB**, and the
  before and after ranges are far apart. It still grows with the body, at about 1.2 ms per MiB
  against about 6.3 before: the walk, the UTF-8 check and the copies that remain.
- **The time tcr adds fell from about 29 to 10 ms at the median at 1 MiB, and from about 155 to
  28 ms at 4 MiB.** The added time includes moving the body twice over loopback, client to tcr and
  tcr to the stub.
- **Memory did not grow**: the peak footprint is a few MB lower at both sizes while tcr served
  three to four times as many requests in the same 20 s.
- **The 32 KiB `overhead` phase moved within its noise** (0.25 to 0.23 to 0.24).
- **The profile agrees with the count.** In `big4m` before, the stacks under the old readers held
  `requests_extended_ttl` 19 to 30% of samples, `extract_bound_tokens` 16 to 19%,
  `extend_all_ttls` 8 to 9%, and the rest 2 to 4% each, with `serde_json` under 60 to 67% of all
  samples. After, none of them appears, `request_body` holds 28 to 36%, and `serde_json` 18 to 27%.

What this predicts for the live log's slowest row (prompts of 700,000 tokens and more, a hold
`wait_ms` of 22 ms at p90 and 90 ms at p99 on 2026-10-16): at roughly 4 bytes a token those bodies
are about 3 MiB, where the CPU cost falls from about 19 ms to about 4 ms. The p90 hold should fall
to about a quarter of what it is, a few milliseconds; the p99 depends on how many such requests
arrive at once. It cannot be measured until a release carries the change, and the bytes-per-token
figure is an assumption, not a measurement.

## Threads, wake-ups and disk

What the proxy costs when it is barely used, which is most of its life: at the fleet's rates it
serves a request a second or two, and between sessions nothing at all. Since `perf/idle-cost`:

- **One INFO line per served request.** `serving request` and a 2xx `upstream response` or
  `relayed response` are DEBUG; `request usage` carries the account's index, the path and the
  status they named. A non-2xx answer keeps its `upstream response` or `relayed response` line at
  INFO, which is what finds a 401 storm or a 429. On the live proxy the four lines were about
  4.5 a request and 75 to 93 MB of log a day.
- **A cache is saved only when its content changed** (`src/save_gate.rs`). The pins and the
  Sessions/Tools table are saved 5 s after a change that adds, removes or moves an entry, and
  within a minute of a change to anything else (a pin's touch time, a session's counters), which
  moves on every request. The bound-token file, about 8 MB at its cap, is saved ten minutes after
  a change and at shutdown, where it was every minute. A crash or SIGKILL loses at most that
  window of new tokens; a clean shutdown loses none.
- **No timer runs while nothing changes.** Each cache's saver sleeps until a request changes it,
  where three timers woke every 5 s (one every 60 s) to look at a flag.
- **Four tokio worker threads by default**, or the machine's cores when fewer, where tokio's
  default is one per core. `TOKIO_WORKER_THREADS` sets the count (`tcr::server::worker_threads`).
- **A closed day of the usage ledger is gzipped** (`src/usage.rs`, "Closed days are gzipped"):
  every day older than yesterday, once when the ledger's writer starts and again each day. Every
  reader reads both forms.

Three runs each side of `steady` and `peak`, and of a 60 s `idle` phase, `--no-profile`,
alternating before and after, on 2026-10-16 at a 1-minute load of 40 to 192 on 14 cores. Before
is `eb67eb7b`, after is the branch with tokio's one worker per core (the worker count was
measured on its own, below). Both ran under the same driver binary. Each cell is min / median /
max across the three runs.

| phase | | CPU s / 1k req | CPU s / min | disk MB written | INFO lines / req | threads | peak footprint MB |
|---|---|---|---|---|---|---|---|
| `steady` | before | 2.08 / 2.22 / 2.29 | 0.17 / 0.19 / 0.19 | 7.97 | 3.09 | 17 | 49.5 / 51.3 / 52.2 |
| `steady` | after | 1.70 / 1.80 / 1.84 | 0.14 / 0.15 / 0.15 | 0.04 | 1.09 | 17 | 50.3 / 51.5 / 51.9 |
| `peak` | before | 1.18 / 1.20 / 1.30 | 0.60 / 0.62 / 0.67 | 0.47 | 3.10 | 17 | 54.9 / 56.0 / 56.2 |
| `peak` | after | 1.12 / 1.13 / 1.35 | 0.58 / 0.58 / 0.69 | 0.02 | 1.09 / 1.10 / 1.11 | 17 | 54.5 / 55.7 / 56.5 |
| `idle` | before | | 0.03 / 0.03 / 0.03 | 7.51 | | 17 | 49.2 / 49.5 / 60.6 |
| `idle` | after | | 0.00 | 0.00 | | 17 | 49.2 / 49.6 / 50.1 |

- **CPU per request in `steady` fell by about a fifth**, and the ranges do not overlap. Most of
  the difference is the bound-token file: before, the 60 s save rewrote all 8 MB of it once in
  every `steady` phase, which is the 7.97 MB written. `peak` moved within its noise; at 30 s it
  never reached a 60 s save.
- **INFO lines per served request fell from 3.09 to 1.09.** The one line is `request usage`, one per
  served `/v1/messages`; `count_tokens` (10% of requests) logs none, and the rest are the shutdown
  lines and the routing events (`affinity: divert`, `bound token pins request ...`) that fire on a
  few requests in a hundred at these rates.
- **The idle minute's 7.5 MB and its CPU are one save**, the bound-token file written the minute
  after the warm-up minted tokens into it. The after build writes the same file ten minutes later
  instead, outside the minute. So in an idle phase the difference is when, not whether; the
  reduction is in a busy hour, where the file was rewritten once or twice a minute and is now
  rewritten six times.
- **The timers themselves** were measured apart from any save: from 90 s after boot, past every
  save either build makes, two 55 s windows each side read the proxy's interrupt wake-ups and CPU
  from `proc_pid_rusage` (a loop of 50 ms sleeps, as a control, read 92 wake-ups in 5 s). Before:
  56 and 57 wake-ups, 1.5 and 2.3 ms of CPU. After: 0 and 0 wake-ups.
- **Memory did not move.**

### Worker threads

The after build on its own, with `TOKIO_WORKER_THREADS` set through the driver (the harness passes
that one variable to the proxy), at the same flags. `steady` and `peak` three runs at 2 workers
against the three after runs at tokio's default of 14 above; `overhead` and `big4m` one run each.

| phase | workers | threads | CPU s / 1k req | added ms p50 | added ms p99 | served |
|---|---|---|---|---|---|---|
| `steady` | 14 | 17 | 1.70 / 1.80 / 1.84 | 2.8 / 7.1 / 9.1 | 20.5 / 20.6 / 40.3 | 105 |
| `steady` | 2 | 5 | 1.57 / 1.62 / 1.78 | 2.8 / 2.9 / 3.2 | 12.5 / 14.2 / 15.6 | 105 |
| `peak` | 14 | 17 | 1.12 / 1.13 / 1.35 | 3.9 / 7.6 / 9.3 | 25.2 / 32.0 / 32.4 | 257 |
| `peak` | 2 | 5 | 1.07 / 1.07 / 1.18 | 2.5 / 4.4 / 4.7 | 16.1 / 18.6 / 18.6 | 257 |
| `overhead` | 14 | 17 | 0.22 | 3.5 | 81.6 | 37,197 |
| `overhead` | 4 | 7 | 0.21 | 3.2 | 65.4 | 44,501 |
| `overhead` | 2 | 5 | 0.19 | 4.1 | 60.2 | 35,652 |
| `big4m` | 14 | 17 | 4.95 | 24.1 | 131.5 | 1,993 |
| `big4m` | 4 | 7 | 4.78 | 18.5 | 151.3 | 2,526 |
| `big4m` | 2 | 5 | 4.70 | 21.6 | 151.0 | 2,234 |

Fewer workers cost nothing these runs can see: CPU per request is the same or lower, and the
added time is no worse (the 2-worker `steady` and `peak` runs met a lower load, 38 to 54 against
40 to 188, so their lower latency is the machine's, not the workers'). Even the saturating phases
served as much at 4 workers as at 14. The default is 4 rather than 2 because the cache saves
write synchronously on a worker, and two workers would leave one while the other writes. The
branch's own build, with that default, ran one `steady` and `peak` pass at 7 threads (4 workers,
the main thread, the log writer and the ledger writer) where tokio's default had 17: 1.72 and
1.11 CPU s per 1,000 requests, 0.04 and 0.02 MB written, 1.09 and 1.10 INFO lines per request.

### The usage ledger's closed days

A generated day in the ledger's own line shape (`usage::tests::ledger_day_compression_on_a_generated_day`,
180,000 lines, the shape the writer serializes, with fake identities and varied counts): 37.6 MB
plain, 4.2 MB gzipped at flate2's default level, 11.2% of its size. Compressing it took 21.8 s in a
debug build; a release build was not timed. The live ledger is about 20 MB a day, so its 90 days
of retention would hold about 200 MB instead of 1.8 GB, if real lines compress as the generated
ones do, which is not measured.

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
