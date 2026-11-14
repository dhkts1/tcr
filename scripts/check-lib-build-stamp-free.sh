#!/usr/bin/env bash
# check-lib-build-stamp-free.sh: proves the `tcr` library does not depend on the
# build stamp (TCR_BUILD_SHA, TCR_BUILD_DIRTY, TCR_BUILD_TIME) that build.rs
# writes.
#
# Why: TCR_BUILD_TIME is the wall clock and build.rs re-runs whenever HEAD
# moves. When the library sat in the package that owns build.rs, every re-run
# recompiled the whole library, even with no source change, and two checkouts
# of one commit never shared a cached build. The stamp now belongs to the
# binary's package only; the library is handed it as a value at startup
# (`tcr::build_info::install`).
#
# What it does, at one commit and with no source change:
#   1. `cargo check` the `tcr` binary (which builds the library under it) and
#      record the library's rmeta hash.
#   2. Detach HEAD at the same commit and wait one second, so the build script
#      re-runs and stamps a new TCR_BUILD_TIME (it changes only by the whole
#      second). The branch is checked out again on exit.
#   3. `cargo check` the binary again.
# It passes when the build script did re-run with a new TCR_BUILD_TIME (the
# positive control: the stamp really changed) and the library came back
# `fresh` with the same rmeta hash.
#
# Usage: scripts/check-lib-build-stamp-free.sh
# Needs cargo and jq. Uses the checkout's normal target dir, so a warm tree is
# quick. bash 3.2 compatible, BSD and GNU userland alike.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

command -v jq >/dev/null || fail "jq is not on PATH"

scratch="$(mktemp -d "${TMPDIR:-/tmp}/tcr-stamp-gate.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT

# One `cargo check` of the binary; its JSON messages land in $1.
check_bin() {
    if ! cargo check --locked -p tcr --bin tcr --message-format=json >"$1" 2>"$1.err"; then
        cat "$1.err" >&2
        fail "cargo check -p tcr --bin tcr failed"
    fi
}

# The library artifact: the `lib` target whose crate name is `tcr`, from
# whichever package owns it.
lib_artifact() {
    jq -c 'select(.reason == "compiler-artifact"
                  and .target.name == "tcr"
                  and (.target.kind | index("lib")))' "$1"
}

# TCR_BUILD_TIME from the build script run of the binary's package, or empty
# when the script did not run in that build.
stamp_time() {
    jq -r 'select(.reason == "build-script-executed")
           | .env[]? | select(.[0] == "TCR_BUILD_TIME") | .[1]' "$1"
}

rmeta_hash() {
    local path
    path="$(printf '%s\n' "$1" | jq -r '.filenames[] | select(endswith(".rmeta"))')"
    [ -n "$path" ] && [ -f "$path" ] || fail "no rmeta in artifact: $1"
    shasum -a 256 "$path" | awk '{print $1}'
}

check_bin "$scratch/first.json"
first_lib="$(lib_artifact "$scratch/first.json")"
[ -n "$first_lib" ] || fail "first build reported no artifact for the tcr library"
first_hash="$(rmeta_hash "$first_lib")"

# Detach HEAD at the same commit: the HEAD file's content changes (a ref name
# becomes a sha), which is a rerun trigger build.rs declares, while the commit
# and every source file stay as they are. Touching build.rs is not enough:
# cargo judges a file by its content, not its mtime. The trap puts the branch
# back.
branch="$(git symbolic-ref -q HEAD)" || fail "HEAD is detached; run this on a branch"
trap 'git checkout -q "${branch#refs/heads/}"; rm -rf "$scratch"' EXIT
git checkout -q --detach HEAD
sleep 1

check_bin "$scratch/second.json"
second_time="$(stamp_time "$scratch/second.json")"
first_time="$(stamp_time "$scratch/first.json")"
[ -n "$second_time" ] || fail "build.rs did not re-run after touching it, so this check proved nothing"
[ "$second_time" != "$first_time" ] || fail "TCR_BUILD_TIME did not change ($second_time), so this check proved nothing"
pass "control: build.rs re-ran and restamped TCR_BUILD_TIME ${first_time:-<cached>} -> $second_time"

second_lib="$(lib_artifact "$scratch/second.json")"
[ -n "$second_lib" ] || fail "second build reported no artifact for the tcr library"
fresh="$(printf '%s\n' "$second_lib" | jq -r '.fresh')"
second_hash="$(rmeta_hash "$second_lib")"

[ "$fresh" = "true" ] || fail "the tcr library was recompiled after the build stamp changed (fresh=$fresh); it still depends on build.rs"
[ "$first_hash" = "$second_hash" ] || fail "the tcr library's rmeta changed with the build stamp ($first_hash -> $second_hash)"
pass "the tcr library stayed fresh across a restamp, rmeta $second_hash"
