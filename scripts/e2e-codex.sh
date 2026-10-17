#!/usr/bin/env bash
# e2e-codex.sh — two real Codex logins, a scratch tcr, one turn each way, one account disabled.
#
#   usage: scripts/e2e-codex.sh <path-to-tcr> <CODEX_HOME_A> <CODEX_HOME_B>
#
# The manual acceptance run for Codex accounts: the REAL `codex` binary completes a turn through
# a scratch proxy on a pooled account, and once that account is disabled, the next turn lands on
# the other one. Oracles: the reply text, the request counts in `tcr status --json`, which
# account served, and a quota window on that account's row afterwards: with the quota probe off,
# only that turn's own x-codex-* headers can put one there. codex's transport-fallback line is
# printed for information only; the request count already proves the HTTP path. It spends two
# real turns of quota. Never prints a token.
# Both turns use gpt-6-luna, offered on the Team and Free plans checked in review.
# Override TCR_E2E_CODEX_MODEL only with a model both logins offer.
#
# The two CODEX_HOMEs must already be logged in to two DIFFERENT ChatGPT accounts, e.g. ~/.codex and
# a second home made with `mkdir -p ~/.codex-second && CODEX_HOME=~/.codex-second codex login`. Both
# logins are copied into a throwaway HOME and served on TCR_E2E_PORT (a free port when unset, and
# never 3456), so the live proxy and its config are never touched.
#
# Those copies are of REAL logins, and a refresh rotates the single-use refresh token, which logs
# the `codex` CLI out of that home. So this run never lets tcr refresh one: a home whose access
# token expires within 30 minutes is refused, the quota probe is off (a probe's 401 would force a
# refresh), and each copy loses its refresh token before the server starts, so that even a 401 on
# the turn itself cannot be answered with one. The throwaway HOME is deleted, and the scratch server
# stopped, on a pass, a failure, Ctrl-C, TERM, HUP, and a reader that quits early. Only a SIGKILL,
# which nothing can trap, leaves both: then the `scratch HOME:` line names the directory to delete
# by hand, and `scratch server pid:` the process.
#
# Test-only: TCR_E2E_CODEX_UPSTREAM=http://127.0.0.1:<port> writes that loopback origin into the
# scratch config as `codexUpstream`, which is where `tcr login --from-codex` checks each login's
# workspace (GET /backend-api/wham/accounts/check) and where every Codex request goes. It is for a
# fixture run against a stub that answers that check, so that the run sends nothing off this
# machine. Any other value is refused, because that origin receives both logins' access tokens.
# Unset, as in an acceptance run, the scratch config keeps the default, https://chatgpt.com.
#
# Exit 2 is a refusal before anything ran, 1 a failed check. Written for the stock macOS bash 3.2,
# as e2e-first-run.sh is; JSON is read with python3 -I.
set -euo pipefail

usage() {
  echo "usage: scripts/e2e-codex.sh <path-to-tcr> <CODEX_HOME_A> <CODEX_HOME_B>" >&2
  exit 2
}
refuse() { echo "REFUSED: $*" >&2; exit 2; }
fail() { echo "FAIL: $*" >&2; exit 1; }

[ "$#" -eq 3 ] || usage
TCR_DIR="$(cd "$(dirname "$1")" 2>/dev/null && pwd)" || refuse "no directory holds $1"
TCR="$TCR_DIR/$(basename "$1")"
if [ ! -f "$TCR" ] || [ ! -x "$TCR" ]; then refuse "$TCR is not an executable tcr"; fi
HOME_A="$(cd "$2" 2>/dev/null && pwd -P)" || refuse "CODEX_HOME_A ($2) is not a directory"
HOME_B="$(cd "$3" 2>/dev/null && pwd -P)" || refuse "CODEX_HOME_B ($3) is not a directory"
[ "$HOME_A" != "$HOME_B" ] || refuse "CODEX_HOME_A and CODEX_HOME_B are one directory; the rotation needs two logins"

# --- the port: never the live proxy's ---------------------------------------
CODEX_E2E_MODEL="${TCR_E2E_CODEX_MODEL:-gpt-6-luna}"
LIVE_PORT=3456
PORT="${TCR_E2E_PORT:-}"
if [ -z "$PORT" ]; then
  PORT="$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
fi
case "$PORT" in
  '' | 0* | *[!0-9]*) refuse "TCR_E2E_PORT must be a port number, not '$PORT'" ;;
esac
if [ "${#PORT}" -gt 5 ] || [ "$PORT" -gt 65535 ]; then refuse "TCR_E2E_PORT must be a port number, not '$PORT'"; fi
[ "$PORT" -ne "$LIVE_PORT" ] || refuse "port $LIVE_PORT is the live proxy's: set TCR_E2E_PORT to another port, or unset it for a free one"
# A process already on the port would receive both imports over its live account-add route, or
# make our server stand down. Asked of 127.0.0.1 only, and only after the refusal above.
if python3 -I -c 'import socket, sys; s = socket.socket(); s.settimeout(2); sys.exit(s.connect_ex(("127.0.0.1", int(sys.argv[1]))) != 0)' "$PORT"; then
  refuse "something is already listening on 127.0.0.1:$PORT: set TCR_E2E_PORT to a free port"
fi

# --- test-only: a loopback Codex origin, never another (see the header) ---------
CODEX_UPSTREAM="${TCR_E2E_CODEX_UPSTREAM:-}"
if [ -n "$CODEX_UPSTREAM" ]; then
  UPSTREAM_PORT="${CODEX_UPSTREAM#http://127.0.0.1:}"
  case "$UPSTREAM_PORT" in
    "$CODEX_UPSTREAM" | '' | 0* | *[!0-9]*)
      refuse "TCR_E2E_CODEX_UPSTREAM must be http://127.0.0.1:<port>, not '$CODEX_UPSTREAM': it receives both logins' access tokens" ;;
  esac
  if [ "${#UPSTREAM_PORT}" -gt 5 ] || [ "$UPSTREAM_PORT" -gt 65535 ]; then
    refuse "TCR_E2E_CODEX_UPSTREAM must be http://127.0.0.1:<port>, not '$CODEX_UPSTREAM'"
  fi
  if [ "$UPSTREAM_PORT" -eq "$LIVE_PORT" ] || [ "$UPSTREAM_PORT" -eq "$PORT" ]; then
    refuse "TCR_E2E_CODEX_UPSTREAM must name neither the live proxy's port nor the scratch server's"
  fi
fi

# --- both logins must outlive the run, so that tcr never refreshes one ----------
MIN_LEFT_MINUTES=30
# Seconds until a CODEX_HOME's access token expires, from the JWT payload's `exp` and nothing
# else, or "unreadable". Prints no token, no claim and no exception text, which could quote the file.
token_seconds_left() {
  python3 -I -c '
import base64, json, sys, time
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        token = json.load(f)["tokens"]["access_token"]
    payload = token.split(".")[1]
    exp = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))["exp"]
    if isinstance(exp, bool) or not isinstance(exp, (int, float)):
        raise ValueError("exp is not a number")
except Exception:
    print("unreadable")
else:
    print(int(exp - time.time()))
' "$1/auth.json"
}
minutes() { if [ "$1" -eq 1 ]; then echo "1 minute"; else echo "$1 minutes"; fi; }
check_expiry() { # $1 = label, $2 = CODEX_HOME
  local left state
  left="$(token_seconds_left "$2")"
  if [ "$left" = unreadable ]; then
    refuse "$1 ($2) holds no ChatGPT login this run can read: log it in with \`CODEX_HOME=$2 codex login\` and retry"
  fi
  if [ "$left" -le $((MIN_LEFT_MINUTES * 60)) ]; then
    state="expires in $(minutes $((left / 60)))"
    [ "$left" -gt 0 ] || state="has expired"
    refuse "$1's access token $state, and this run needs more than $MIN_LEFT_MINUTES minutes on it so that tcr never refreshes it: log it in again with \`CODEX_HOME=$2 codex login\` and retry"
  fi
  echo "$1: access token valid for another $(minutes $((left / 60)))"
}

echo "== preflight"
echo "model: $CODEX_E2E_MODEL (must be available to both logins)"
check_expiry CODEX_HOME_A "$HOME_A"
check_expiry CODEX_HOME_B "$HOME_B"
# `tcr run --codex` launches TCR_CODEX_BIN, else CODEX_BIN, else `codex` on PATH; empty is unset.
CODEX="${TCR_CODEX_BIN:-${CODEX_BIN:-codex}}"
CODEX_PATH="$(command -v "$CODEX")" || refuse "no codex to launch: '$CODEX' is not on PATH"
echo "codex: $CODEX_PATH"
echo "port: $PORT"
[ -z "$CODEX_UPSTREAM" ] || echo "codexUpstream: $CODEX_UPSTREAM (test-only)"

# --- the scratch HOME: copies of two real logins, deleted on every exit -----------
E2E_HOME=""
SERVER=""
cleanup() {
  local rc=$? i=0 log
  set +e
  trap '' INT TERM HUP PROF # a second Ctrl-C must not cut the deletion short
  if [ -n "$SERVER" ]; then
    kill "$SERVER" 2>/dev/null
    while [ "$i" -lt 50 ] && kill -0 "$SERVER" 2>/dev/null; do
      sleep 0.2
      i=$((i + 1))
    done
    if kill -0 "$SERVER" 2>/dev/null; then kill -9 "$SERVER" 2>/dev/null; fi
    wait "$SERVER" 2>/dev/null
  fi
  if [ -n "$E2E_HOME" ]; then
    if [ "$rc" -ne 0 ]; then
      for log in server.log codex.err; do
        [ -s "$E2E_HOME/$log" ] || continue
        echo "--- last 20 lines of $log" >&2
        tail -n 20 "$E2E_HOME/$log" >&2
      done
    fi
    rm -rf "$E2E_HOME"
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
# bash runs the EXIT trap when a catchable signal it was not told to trap ends the script, except
# SIGPROF, which it leaves at its default action: that would skip cleanup.
trap 'exit 155' PROF
# A reader that quits early (`| head`, leaving `| less`, Ctrl-C on `2>&1 | tee run.log`) must turn
# the next write into a failed command, which both `set -e` and cleanup survive. Left at its
# default, SIGPIPE kills the shell instead, before cleanup or partway through it, and the scratch
# HOME and server outlive the run. Not `trap 'exit 141' PIPE`: with 2>&1 cleanup's own writes go to
# that same dead pipe, and the first of them would end it before the deletion.
trap '' PIPE
TMP_ROOT="${TMPDIR:-/tmp}"
E2E_HOME="$(mktemp -d "${TMP_ROOT%/}/tcr-e2e-codex.XXXXXX")"
echo "scratch HOME: $E2E_HOME (deleted on exit, though not after a SIGKILL)"

export HOME="$E2E_HOME"
# Nothing of the caller's steers the scratch run: not their config or cache dir, not
# TCR_CODEX_AUTH (beside --codex-home it makes both imports refuse to run), not their Claude Code
# login, which a scratch HOME does not hide (it is in the login Keychain), and not a proxy
# variable. That last is for codex's sake, not tcr's, whose upstream clients ignore proxy
# variables: a turn that is not pooled launches codex directly, with only the variables that name
# the scratch port removed, so an inherited HTTPS_PROXY aimed at the live proxy would carry that
# turn through it before the not-pooled check below fails the run.
unset XDG_CONFIG_HOME XDG_CACHE_HOME TCR_CODEX_AUTH \
  HTTPS_PROXY https_proxy HTTP_PROXY http_proxy ALL_PROXY all_proxy || true
export TCR_CLAUDE_CODE_CREDENTIALS="$E2E_HOME/no-claude-login"
CONFIG="$HOME/.config/tcr.json"
WORK="$E2E_HOME/work" # codex's working directory: empty, so a turn reads no repository
mkdir -p "$(dirname "$CONFIG")" "$WORK"
if [ -n "$CODEX_UPSTREAM" ]; then
  printf '{ "proxy": { "port": %s }, "quotaProbeSeconds": 0, "codexUpstream": "%s", "accounts": [] }\n' \
    "$PORT" "$CODEX_UPSTREAM" >"$CONFIG"
else
  printf '{ "proxy": { "port": %s }, "quotaProbeSeconds": 0, "accounts": [] }\n' "$PORT" >"$CONFIG"
fi
chmod 600 "$CONFIG"

echo "== import both logins"
"$TCR" login --from-codex --codex-home "$HOME_A" --config "$CONFIG" || fail "importing CODEX_HOME_A failed"
"$TCR" login --from-codex --codex-home "$HOME_B" --config "$CONFIG" || fail "importing CODEX_HOME_B failed"
python3 -I - "$CONFIG" <<'PY' || fail "the scratch config does not hold the two imported Codex accounts"
import json, os, sys

path = sys.argv[1]
with open(path, encoding="utf-8") as f:
    config = json.load(f)
rows = config.get("accounts", [])
if len(rows) != 2 or any(row.get("provider") != "codex" for row in rows):
    sys.exit("expected two Codex accounts, found %d account(s): do both homes hold the same login?" % len(rows))
for row in rows:
    row.pop("refreshToken", None)
fd = os.open(path + ".tmp", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w", encoding="utf-8") as f:
    json.dump(config, f, indent=2)
    f.write("\n")
os.replace(path + ".tmp", path)
PY
echo "dropped both copies' refresh tokens: tcr cannot refresh, and so cannot log out, either login"

# --- the scratch server, ready only once it says it bound OUR port ---------------
echo "== start the scratch proxy"
# The readiness line is logged at info, so the caller's RUST_LOG must not decide whether it appears.
RUST_LOG=info "$TCR" server --headless --config "$CONFIG" >"$E2E_HOME/server.log" 2>&1 &
SERVER=$!
echo "scratch server pid: $SERVER (stopped on exit, though not after a SIGKILL)"
READY="listening on http://127.0.0.1:$PORT (headless)"
deadline=$((SECONDS + 30))
until grep -qF "$READY" "$E2E_HOME/server.log"; do
  kill -0 "$SERVER" 2>/dev/null || fail "the scratch server exited before it logged: $READY"
  [ "$SECONDS" -lt "$deadline" ] || fail "the scratch server did not log within 30s: $READY"
  sleep 0.2
done
echo "ready: $READY"

# --- reading the fleet ------------------------------------------------------------
# One program, several questions, each over `tcr status --json` files in the scratch HOME. Every
# answer insists the rows are LIVE: on an offline fallback no request count was measured, so none
# may be read as 0.
IFS= read -r -d '' STATUS_PY <<'PY' || true
import json, sys


def fail(message):
    sys.exit("FAIL: " + message)


def load(path):
    with open(path, encoding="utf-8") as f:
        rows = json.load(f)
    if not isinstance(rows, list) or not rows:
        fail("tcr status --json listed no accounts")
    if any(row.get("source") != "live" for row in rows):
        fail("tcr status --json fell back to an offline snapshot, so no request count was measured")
    return {row["name"]: row for row in rows}


def requests(row):
    count = row.get("requests")
    if isinstance(count, bool) or not isinstance(count, int):
        fail("the row for %s carries no request count" % row.get("name"))
    return count


def growth(before, after):
    return {
        name: requests(row) - (requests(before[name]) if name in before else 0)
        for name, row in after.items()
    }


def most_grown(deltas):
    grown = [name for name, delta in deltas.items() if delta > 0]
    return max(grown, key=lambda name: deltas[name]) if grown else ""


def percent(fraction):
    return "-" if fraction is None else "%.0f%%" % (100 * fraction)


def has_window(row):
    # Either window is enough: a Pro login reports no five-hour one, and 0% is a reading.
    return row.get("fiveHour") is not None or row.get("sevenDay") is not None


def must_show_window(rows, name):
    # The quota probe is off, so a window on the serving row came from its turn's headers.
    if name and not has_window(rows[name]):
        fail("%s served the turn but its row shows no quota window: tcr read no x-codex-* "
             "quota header off the response" % name)


mode, args = sys.argv[1], sys.argv[2:]
if mode == "table":
    for name, row in load(args[0]).items():
        print("  %s  provider=%s requests=%d disabled=%s gate=%s 5h=%s 7d=%s" % (
            name, row.get("provider", "anthropic"), requests(row),
            "true" if row.get("disabled") else "false", row.get("gate"),
            percent(row.get("fiveHour")), percent(row.get("sevenDay"))))
elif mode == "fresh":
    rows = load(args[0])
    if len(rows) != 2 or any(row.get("provider") != "codex" for row in rows.values()):
        fail("the scratch server should hold exactly the two imported Codex accounts")
    if any(requests(row) > 0 or row.get("disabled") for row in rows.values()):
        fail("the fresh scratch server reports a request, or a disabled account")
    if any(has_window(row) for row in rows.values()):
        fail("the fresh scratch server already reports a quota window, so one after a turn "
             "would prove nothing")
elif mode == "served":
    # before, after: the account the turn between them landed on, or "" for none; that
    # account's row must show a quota window.
    after = load(args[1])
    served = most_grown(growth(load(args[0]), after))
    must_show_window(after, served)
    print(served)
elif mode == "disabled":
    # status, name: the running server holds that account out of rotation.
    row = load(args[0]).get(args[1])
    if row is None or not row.get("disabled"):
        fail("%s is not disabled on the running scratch server" % args[1])
elif mode == "rotated":
    # before, after: like `served`, but no disabled account may have served in between.
    after = load(args[1])
    deltas = growth(load(args[0]), after)
    for name, delta in deltas.items():
        if delta > 0 and after[name].get("disabled"):
            fail("the disabled account %s served %d request(s) in turn 2" % (name, delta))
    served = most_grown(deltas)
    must_show_window(after, served)
    print(served)
else:
    fail("unknown question " + mode)
PY
fleet() { (cd "$E2E_HOME" && python3 -I -c "$STATUS_PY" "$@"); }
snapshot() { # $1 = file name under the scratch HOME
  kill -0 "$SERVER" 2>/dev/null || fail "the scratch server is no longer running"
  if ! "$TCR" status --json --config "$CONFIG" >"$E2E_HOME/$1" 2>"$E2E_HOME/status.err"; then
    fail "tcr status --json failed: $(tail -n 3 "$E2E_HOME/status.err")"
  fi
}

# --- one turn of the real codex binary --------------------------------------------
NOT_POOLED="launching codex directly (not pooled)"
run_turn() { # $1 = the turn's number
  local n="$1" rc=0 line reply folded
  echo "== turn $n"
  # codex keeps CODEX_HOME_A's own login (tcr swaps the credential on inference only) and runs in
  # an empty directory, its log level pinned so that codex.err never carries debug output.
  (cd "$WORK" && CODEX_HOME="$HOME_A" RUST_LOG=codex_core=info \
    "$TCR" run --codex --config "$CONFIG" -- exec --model "$CODEX_E2E_MODEL" --skip-git-repo-check "Reply with exactly: ok" \
    </dev/null >"$E2E_HOME/turn$n.out" 2>"$E2E_HOME/codex.err") || rc=$?
  if line="$(grep -m 1 -F "$NOT_POOLED" "$E2E_HOME/codex.err")"; then
    fail "turn $n was not pooled: $line"
  fi
  # Both Codex transports carry the proxy URL userinfo as a CONNECT marker. This opt-in
  # confines TLS termination to the session; an unmarked chatgpt.com connection stays blind.
  grep -F "tunnelling via http://tcr-codex@127.0.0.1:$PORT;" "$E2E_HOME/codex.err" >/dev/null \
    || fail "turn $n did not use the marked Codex proxy URL"
  [ "$rc" -eq 0 ] || fail "turn $n: tcr run --codex exited $rc"
  reply="$(sed -e '/^[[:space:]]*$/d' "$E2E_HOME/turn$n.out" | tail -n 1)"
  echo "$reply"
  # Its letters only, lowercased: ok, OK., `ok`, "ok" and **ok** pass; looks, tokens, okay do not.
  folded="$(printf '%s' "$reply" | tr -cd '[:alpha:]' | tr '[:upper:]' '[:lower:]')"
  [ "$folded" = ok ] || fail "turn $n did not answer ok"
}

echo "== before"
snapshot before.json
fleet fresh before.json
fleet table before.json

run_turn 1
echo "== after turn 1"
snapshot after-1.json
fleet table after-1.json
SERVED="$(fleet served before.json after-1.json)"
[ -n "$SERVED" ] || fail "no account recorded a request — the turn bypassed the pool"
echo "served by: $SERVED"
# Printed for information, not an oracle. codex's own log only: the server logs the 426 it
# answers at debug, and at info its log already carries an unrelated "peer-lease fallback" line
# from boot.
grep -m 3 -iE 'fall(ing)? ?back|(^|[^0-9])426([^0-9]|$)' "$E2E_HOME/codex.err" ||
  echo "(no transport-fallback line captured; the pooled request count above is the proof)"

echo "== disable $SERVED, turn 2 must land elsewhere"
"$TCR" disable "$SERVED" --config "$CONFIG" || fail "tcr disable $SERVED failed"
snapshot disabled.json
fleet disabled disabled.json "$SERVED"
run_turn 2
echo "== after turn 2"
snapshot after-2.json
fleet table after-2.json
OTHER="$(fleet rotated disabled.json after-2.json)"
[ -n "$OTHER" ] || fail "no account recorded a request in turn 2 — the turn bypassed the pool"
[ "$OTHER" != "$SERVED" ] || fail "turn 2 did not rotate to the other account"
echo "PASS: pooled ($SERVED), then rotated ($OTHER)"
