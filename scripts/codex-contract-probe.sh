#!/usr/bin/env bash
# codex-contract-probe.sh — what the REAL Codex backend answers, with no secret printed.
#
# Reads the login at $CODEX_HOME/auth.json (default ~/.codex) with python3 -I, keeps the token
# in a shell variable that is never echoed, and prints ONLY:
#   1. the shape of GET /backend-api/wham/usage: every JSON key name verbatim, the type of each
#      value, and the values of just used_percent, limit_window_seconds, plan_type, allowed and
#      limit_reached (none held an id, email or token in the payload measured 2026-10-12)
#   2. the HTTP status the inference path answers a WebSocket upgrade with
# Run this BEFORE trusting the wham/usage field names src/codex_probe.rs parses and the 426 the
# proxy answers upgrades with; the source read (openai/codex@f82e365, codex-rs/: the usage payload
# model codex-backend-openapi-models/src/models/rate_limit_window_snapshot.rs and
# rate_limit_status_details.rs, fetched by backend-client/src/client/rate_limit_resets.rs; the 426
# at core/src/client.rs:2010-2014) is the expectation, this is the measurement. Manual only: it
# spends no inference, but it does use a real credential, which is visible in curl's argument
# list while each request runs.
set -euo pipefail
{ set +x; } 2>/dev/null   # an inherited xtrace (bash -x, SHELLOPTS) would print the token
# -q must be curl's FIRST argument: it skips any curlrc, whose "verbose" or "trace" line would
# print the bearer (to stderr, or to a trace file) right above the "nothing above is a secret" footer.
CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
AUTH="$CODEX_HOME/auth.json"
[ -f "$AUTH" ] || { echo "no $AUTH — run codex login first" >&2; exit 1; }
# One value per line, one read each: on a shared line an absent access_token would shift
# account_id into TOKEN, past the guard below.
{ read -r TOKEN; read -r ACCOUNT; } < <(python3 -I - "$AUTH" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
t = d.get("tokens") or {}
print(t.get("access_token") or "")
print(t.get("account_id") or "")
PY
)
[ -n "$TOKEN" ] || { echo "auth.json holds no ChatGPT access token" >&2; exit 1; }
echo "== 1. usage payload shape (key names verbatim; values redacted to types except used_percent, limit_window_seconds, plan_type, allowed, limit_reached)"
curl -q -sS --fail-with-body --max-time 20 https://chatgpt.com/backend-api/wham/usage \
  -H "Authorization: Bearer $TOKEN" -H "chatgpt-account-id: $ACCOUNT" -H "Accept: application/json" \
| python3 -I -c '
import json, sys
d = json.load(sys.stdin)
def shape(o, depth=0):
    pad = "  " * depth
    if isinstance(o, dict):
        for k, v in o.items():
            if isinstance(v, (dict, list)): print(f"{pad}{k}:"); shape(v, depth + 1)
            elif k in ("limit_window_seconds", "used_percent", "plan_type", "allowed", "limit_reached"): print(f"{pad}{k} = {v!r}")
            else: print(f"{pad}{k}: {type(v).__name__}")
    elif isinstance(o, list): print(f"{pad}[{len(o)} items]")
shape(d)'
echo "== 2. websocket upgrade status on the inference path (measured 2026-10-12: 101, the backend upgrades; tcr's own 426 is what makes the CLI fall back to HTTP)"
# A fresh key per run: RFC 6455 §4.1 wants 16 random bytes, and the RFC's own sample nonce is a
# literal the pre-commit gitleaks gate flags as a generic api key. --max-time bounds a 101 answer,
# which otherwise holds the socket open until the server hangs up.
curl -q -sS -o /dev/null -w '%{http_code}\n' --http1.1 --max-time 20 https://chatgpt.com/backend-api/codex/responses \
  -H "Authorization: Bearer $TOKEN" -H "chatgpt-account-id: $ACCOUNT" \
  -H "Connection: Upgrade" -H "Upgrade: websocket" -H "Sec-WebSocket-Version: 13" \
  -H "Sec-WebSocket-Key: $(head -c 16 /dev/urandom | base64)" \
  || echo "(curl exit $?: 000 means no answer at all; 101 holds the socket open until --max-time and is still the backend's answer)"
echo "== done; nothing above is a secret. Compare window lengths with CODEX_FIVE_HOUR_CUTOFF_* in src/."
