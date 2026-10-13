#!/usr/bin/env bash
# check-e2e-first-run-safe.sh: proves scripts/e2e-first-run.sh is safe to run on
# a developer Mac, without starting a real `tcr`.
#
# Two hazards it guards against:
#   1. TCR_CLAUDE_CODE_CREDENTIALS unset when `tcr` runs. `tcr` then reads the
#      macOS login Keychain, which a throwaway HOME does not redirect, so a
#      developer's real Claude Code login could be imported and its refresh
#      token rotated.
#   2. The server booting on 3456, the default port a live proxy holds.
#
# Static checks run first and never execute the script under test. Only when
# they pass is the script run, and then only with a stub in place of `tcr`
# that records any call, so a regression shows up as a marker file instead of
# a real server.
#
# Usage: scripts/check-e2e-first-run-safe.sh [path-to-e2e-first-run.sh]
# bash 3.2 compatible, BSD and GNU userland alike.
set -euo pipefail

SCRIPT="${1:-$(cd "$(dirname "$0")" && pwd)/e2e-first-run.sh}"
[ -f "$SCRIPT" ] || { echo "FAIL: no script at $SCRIPT" >&2; exit 2; }

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# --- static: read the script, run nothing -----------------------------------
# The first line that calls `tcr`, i.e. `"$TCR"` followed by an argument, not
# the lines that assign TCR.
first_call="$(awk '/^[[:space:]]*TCR=/ { next } /"\$TCR"[[:space:]]/ { print NR; exit }' "$SCRIPT")"
[ -n "$first_call" ] || fail "found no \"\$TCR\" call in $SCRIPT; this check no longer matches the script"

unset_line="$(awk '/^[[:space:]]*unset[[:space:]].*TCR_CLAUDE_CODE_CREDENTIALS/ { print NR; exit }' "$SCRIPT")"
[ -z "$unset_line" ] || fail "$SCRIPT:$unset_line unsets TCR_CLAUDE_CODE_CREDENTIALS, so tcr falls back to the login Keychain"

export_line="$(awk '/^[[:space:]]*export[[:space:]]+TCR_CLAUDE_CODE_CREDENTIALS="?[^"[:space:]]/ { print NR; exit }' "$SCRIPT")"
[ -n "$export_line" ] || fail "$SCRIPT never exports TCR_CLAUDE_CODE_CREDENTIALS to a path, so tcr can read the login Keychain"
[ "$export_line" -lt "$first_call" ] || fail "$SCRIPT exports TCR_CLAUDE_CODE_CREDENTIALS at line $export_line, after the first tcr call at line $first_call"
pass "static: TCR_CLAUDE_CODE_CREDENTIALS exported at line $export_line, before the first tcr call at line $first_call, never unset"

# --- dynamic: run the script with a stub tcr --------------------------------
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
MARKER="$WORK/tcr-was-called"
STUB="$WORK/tcr"
printf '#!/bin/sh\necho "$@" >>"%s"\nexit 1\n' "$MARKER" >"$STUB"
chmod 755 "$STUB"

# An explicit 3456 is refused before any tcr call.
set +e
env -u TCR_E2E_DRY_RUN -u TCR_CLAUDE_CODE_CREDENTIALS TCR_E2E_PORT=3456 \
  bash "$SCRIPT" "$STUB" >"$WORK/refuse.out" 2>"$WORK/refuse.err"
rc=$?
set -e
[ ! -e "$MARKER" ] || fail "with TCR_E2E_PORT=3456 the script called tcr ($(cat "$MARKER")) instead of refusing"
[ "$rc" -ne 0 ] || fail "with TCR_E2E_PORT=3456 the script exited 0 instead of refusing"
grep -q 3456 "$WORK/refuse.err" || fail "the refusal does not name port 3456: $(cat "$WORK/refuse.err")"
pass "TCR_E2E_PORT=3456 refused with exit $rc before any tcr call"

# With no port given, the dry run names a free port that is not 3456 and a
# credentials path inside the throwaway HOME.
set +e
env -u TCR_E2E_PORT -u TCR_CLAUDE_CODE_CREDENTIALS TCR_E2E_DRY_RUN=1 \
  bash "$SCRIPT" "$STUB" >"$WORK/dry.out" 2>"$WORK/dry.err"
rc=$?
set -e
[ ! -e "$MARKER" ] || fail "the dry run called tcr ($(cat "$MARKER"))"
[ "$rc" -eq 0 ] || fail "the dry run exited $rc: $(cat "$WORK/dry.err")"
port="$(sed -n 's/^port=//p' "$WORK/dry.out")"
creds="$(sed -n 's/^credentials=//p' "$WORK/dry.out")"
home="$(sed -n 's/^home=//p' "$WORK/dry.out")"
case "$port" in
  '' | *[!0-9]*) fail "the dry run printed no port number: $(cat "$WORK/dry.out")" ;;
esac
[ "$port" != "3456" ] || fail "with TCR_E2E_PORT unset the script picked 3456"
[ -n "$home" ] && [ "$home" != "$HOME" ] || fail "the dry run's HOME is not a throwaway directory: '$home'"
case "$creds" in
  "$home"/*) ;;
  *) fail "TCR_CLAUDE_CODE_CREDENTIALS is '$creds', not a path inside the throwaway HOME '$home'" ;;
esac
pass "unset TCR_E2E_PORT picks port $port; credentials read only from $creds"

echo "check-e2e-first-run-safe: $SCRIPT cannot reach the Keychain or bind 3456"
