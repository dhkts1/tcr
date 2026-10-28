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
# --resets is GET-only and keeps login material inside isolated Python memory. --stdin
# accepts synthetic {usage:{status,body},details:{status,body}} without reading a login.
if [ "${1:-}" = "--resets" ]; then
  exec python3 -I -c '
import datetime, json, os, re, signal, sys, urllib.request
LIMIT = 1048576
KEYS = frozenset("plan_type rate_limit allowed limit_reached primary_window secondary_window used_percent limit_window_seconds reset_after_seconds reset_at credits has_credits unlimited balance spend_control reached rate_limit_reached_type type details rate_limit_reset_credits available_count id reset_type status granted_at expires_at title description".split())
def kind(value):
    if value is None: return "null"
    if isinstance(value, bool): return "boolean"
    if isinstance(value, dict): return "object"
    if isinstance(value, list): return "array"
    if isinstance(value, str): return "string"
    if isinstance(value, int): return "integer"
    return "number"
def shapes(value, path):
    result = {path + ": " + kind(value)}
    if isinstance(value, dict):
        for key, child in value.items():
            if key in KEYS:
                result.update(shapes(child, path + "." + key))
            else:
                result.add(path + ".<unrecognized-key>: redacted")
    elif isinstance(value, list):
        for child in value:
            result.update(shapes(child, path + "[]"))
    return result
def count(value):
    return type(value) is int and 0 <= value <= 9223372036854775807
def timestamp(value):
    if not isinstance(value, str): raise ValueError()
    if re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?(?:Z|[+-][0-9]{2}:[0-9]{2})", value) is None: raise ValueError()
    parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None or "T" not in value: raise ValueError()
    return parsed.timestamp()
def validate(usage, details):
    if not isinstance(usage, dict) or not isinstance(details, dict): return False
    if usage.get("plan_type") is not None and not isinstance(usage["plan_type"], str): return False
    rate = usage.get("rate_limit")
    if rate is not None:
        if not isinstance(rate, dict): return False
        for key in ("allowed", "limit_reached"):
            if rate.get(key) is not None and type(rate[key]) is not bool: return False
        for key in ("primary_window", "secondary_window"):
            window = rate.get(key)
            if window is None: continue
            if not isinstance(window, dict): return False
            if window.get("used_percent") is not None and type(window["used_percent"]) not in (int, float): return False
            for field in ("limit_window_seconds", "reset_after_seconds", "reset_at"):
                if window.get(field) is not None and type(window[field]) is not int: return False
    summary = usage["rate_limit_reset_credits"]
    n = summary["available_count"]
    if not count(n) or not count(details["available_count"]): return False
    if details["available_count"] != n: return False
    ids, available = set(), 0
    if not isinstance(details["credits"], list): return False
    now = datetime.datetime.now(datetime.timezone.utc).timestamp()
    for row in details["credits"]:
        identity = row["id"]
        if not isinstance(identity, str) or not identity.strip() or identity in ids: return False
        ids.add(identity)
        if row["reset_type"] != "codex_rate_limits": return False
        if row["status"] not in ("available", "redeeming", "redeemed"): return False
        granted = timestamp(row["granted_at"])
        expires = None if row.get("expires_at") is None else timestamp(row["expires_at"])
        if expires is not None and expires < granted: return False
        for key in ("title", "description"):
            if row.get(key) is not None and not isinstance(row[key], str): return False
        if row["status"] == "available":
            if granted > now or (expires is not None and expires <= now): return False
            available += 1
    return available == n
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs): return None
def read_json(stream):
    body = stream.read(LIMIT + 1)
    if len(body) > LIMIT: raise ValueError()
    return json.loads(body)
def get(opener, path, headers):
    request = urllib.request.Request("https://chatgpt.com/backend-api/wham/" + path, headers=headers, method="GET")
    with opener.open(request, timeout=8) as response:
        return {"status": response.status, "body": read_json(response)}
def main():
    if sys.argv[1:] == ["--stdin"]:
        envelope = read_json(sys.stdin.buffer)
    elif not sys.argv[1:]:
        signal.signal(signal.SIGALRM, lambda *args: sys.exit(1))
        signal.alarm(20)
        home = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")
        with open(os.path.join(home, "auth.json"), "rb") as source:
            tokens = read_json(source)["tokens"]
        token = tokens["access_token"]
        if not isinstance(token, str) or not token: raise ValueError()
        headers = {"Authorization": "Bearer " + token, "Accept": "application/json", "User-Agent": "tcr-contract-probe"}
        account = tokens.get("account_id")
        if account is not None:
            if not isinstance(account, str): raise ValueError()
            headers["ChatGPT-Account-ID"] = account
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        envelope = {"usage": get(opener, "usage", headers), "details": get(opener, "rate-limit-reset-credits", headers)}
        signal.alarm(0)
    else:
        raise ValueError()
    valid = True
    for name in ("usage", "details"):
        result = envelope[name]
        ok = type(result["status"]) is int and 200 <= result["status"] < 300
        print(name + " HTTP: " + ("pass" if ok else "fail"))
        if ok:
            for shape in sorted(shapes(result["body"], name)):
                print(shape)
        valid = valid and ok
    valid = valid and validate(envelope["usage"]["body"], envelope["details"]["body"])
    print("reset contract: " + ("pass" if valid else "fail"))
    return 0 if valid else 1
try:
    sys.exit(main())
except Exception:
    print("reset contract: fail")
    sys.exit(1)
' "${@:2}"
fi
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
