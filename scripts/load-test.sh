#!/usr/bin/env bash
# Load-test the real `tcr` binary against a local fake Anthropic, and profile it.
#
# Usage: scripts/load-test.sh [options]
#
#   --phases LIST       comma-separated, from overhead,steady,peak,runaway,big1m,big4m
#                       (default: all six)
#   --overhead S        seconds of the overhead phase (default 20)
#   --steady S          seconds of the steady phase (default 75: covers fifteen 5 s cache
#                       saves and one 60 s bound-token save)
#   --peak S            seconds of the peak phase (default 30: six bursts)
#   --runaway S         seconds of the runaway phase (default 20)
#   --big S             seconds of each big-body phase, big1m and big4m (default 20)
#   --reply-ms MS       the stub's reply time in the throttled phases (default 300)
#   --body-kb KB        each request's system prompt size (default 32)
#   --profile-ms MS     `sample` interval for the flame graphs (default 10; 1 ms costs the
#                       proxy enough time to halve the overhead phase's throughput)
#   --no-profile        skip `sample` and the flame graphs
#   --per-thread-svg    also draw <phase>.threads.svg, one tower per thread rather than
#                       per kind of thread (the default <phase>.svg)
#   --skip-build        reuse the binaries already in the target directory
#   --out DIR           where the run's files go (default: a new temp directory)
#
# What it does, and what keeps it away from the live install, is in docs/performance.md.
# macOS only: it profiles with /usr/bin/sample and fences the proxy with a DYLD-injected
# connect guard (tests/support/connect_guard.c).
set -euo pipefail

phases="overhead,steady,peak,runaway,big1m,big4m"
overhead_s=20
steady_s=75
peak_s=30
runaway_s=20
big_s=20
reply_ms=300
body_kb=32
profile=1
profile_ms=10
per_thread_svg=0
build=1
out=""

while [ $# -gt 0 ]; do
  case "$1" in
    --phases) phases="$2"; shift 2 ;;
    --overhead) overhead_s="$2"; shift 2 ;;
    --steady) steady_s="$2"; shift 2 ;;
    --peak) peak_s="$2"; shift 2 ;;
    --runaway) runaway_s="$2"; shift 2 ;;
    --big) big_s="$2"; shift 2 ;;
    --reply-ms) reply_ms="$2"; shift 2 ;;
    --body-kb) body_kb="$2"; shift 2 ;;
    --profile-ms) profile_ms="$2"; shift 2 ;;
    --no-profile) profile=0; shift ;;
    --per-thread-svg) per_thread_svg=1; shift ;;
    --skip-build) build=0; shift ;;
    --out) out="$2"; shift 2 ;;
    -h|--help) sed -n '2,27p' "$0"; exit 0 ;;
    *) echo "load-test: unknown option $1" >&2; exit 2 ;;
  esac
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "load-test: macOS only (sample, DYLD_INSERT_LIBRARIES)" >&2
  exit 2
fi

repo="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
# Its own target directory, so the profiling build (symbols, no stripping) never
# replaces the binary another build or an install reads.
target="${TCR_LOAD_TARGET_DIR:-$HOME/.cache/tcr-load-test-target}"
if [ -z "$out" ]; then
  out="$(mktemp -d "${TMPDIR:-/tmp}/tcr-load.XXXXXX")"
fi
mkdir -p "$out"
scratch="$out/scratch"
mkdir -p "$scratch"

cleanup() {
  local status=$?
  for pidfile in "$out"/*.proxy.pid; do
    [ -e "$pidfile" ] || continue
    pid="$(cat "$pidfile")"
    # Only a process still running the binary this script built.
    if [ -n "$pid" ] && ps -p "$pid" -o command= 2>/dev/null | grep -qF "$target/release/tcr"; then
      echo "load-test: stopping leftover test proxy $pid" >&2
      kill -TERM "$pid" 2>/dev/null || true
    fi
  done
  exit "$status"
}
trap cleanup EXIT INT TERM

if [ "$build" = 1 ]; then
  echo "load-test: building tcr and the load driver (release, with symbols) in $target" >&2
  CARGO_TARGET_DIR="$target" \
  CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
  CARGO_PROFILE_RELEASE_STRIP=none \
    cargo build --manifest-path "$repo/Cargo.toml" --release --locked --bin tcr --example load_test
fi
tcr="$target/release/tcr"
driver="$target/release/examples/load_test"
for bin in "$tcr" "$driver"; do
  [ -x "$bin" ] || { echo "load-test: missing $bin (run without --skip-build)" >&2; exit 1; }
done

guard="$scratch/connect-guard.dylib"
cc -dynamiclib "$repo/tests/support/connect_guard.c" -o "$guard"
"$driver" guard-check --tcr "$tcr" --guard "$guard" --scratch "$scratch"

duration_of() {
  case "$1" in
    overhead) echo "$overhead_s" ;;
    steady) echo "$steady_s" ;;
    peak) echo "$peak_s" ;;
    runaway) echo "$runaway_s" ;;
    big1m|big4m) echo "$big_s" ;;
    *) echo "load-test: unknown phase $1" >&2; exit 2 ;;
  esac
}

IFS=',' read -r -a phase_list <<< "$phases"
for phase in "${phase_list[@]}"; do
  duration="$(duration_of "$phase")"
  # The closed-loop phases measure the proxy alone, so their stub answers at once.
  phase_reply=$reply_ms
  case "$phase" in overhead|big1m|big4m) phase_reply=0 ;; esac
  args=(phase --phase "$phase" --tcr "$tcr" --guard "$guard" --scratch "$scratch/$phase"
        --out "$out" --duration "$duration" --reply-ms "$phase_reply" --body-kb "$body_kb")
  [ "$profile" = 1 ] && args+=(--profile-ms "$profile_ms")
  echo "load-test: $phase for ${duration}s" >&2
  "$driver" "${args[@]}"
  if [ "$profile" = 1 ] && [ -s "$out/$phase.sample.txt" ]; then
    # Rust's v0 symbols (`_RNv...`) are unreadable; Xcode's c++filt (LLVM) demangles them.
    # A demangled array type carries "; " (`[u8; 32]`), which the folded format would read
    # as a frame boundary, so it becomes a full-width semicolon.
    inferno-collapse-sample "$out/$phase.sample.txt" | c++filt | sed 's/; /； /g' > "$out/$phase.threads.folded"
    # `sample` roots every stack at its thread (`Thread_395926894: tokio-rt-worker`), which
    # draws one narrow tower per thread. Rooting at the thread's name merges them; the report's
    # top-frame lists read this merged file.
    "$driver" merge-threads "$out/$phase.threads.folded" "$out/$phase.folded"
    inferno-flamegraph --title "tcr load test: $phase" < "$out/$phase.folded" > "$out/$phase.svg"
    if [ "$per_thread_svg" = 1 ]; then
      inferno-flamegraph --title "tcr load test: $phase, per thread" \
        < "$out/$phase.threads.folded" > "$out/$phase.threads.svg"
    fi
  fi
done

sha="$(git -C "$repo" rev-parse --short HEAD)"
meta="tcr ${sha}, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(sysctl -n hw.ncpu) cores, $(sysctl -n hw.model); phases ${phases}; durations overhead ${overhead_s}s steady ${steady_s}s peak ${peak_s}s runaway ${runaway_s}s big ${big_s}s; stub reply ${reply_ms} ms; body ${body_kb} KiB; sample every $([ "$profile" = 1 ] && echo "${profile_ms} ms" || echo "never")"
"$driver" report --out "$out" --meta "$meta"
echo "load-test: done, $out/report.md" >&2
