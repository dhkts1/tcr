#!/bin/sh
# tailnet-find
# Two Macs of one operator find each other over the tailnet, with no mDNS.
#
# Cast: node-a1-overlay and node-b1-overlay, each on its own home network and
# on `overlay` (100.64.99.0/24, inside 100.64.0.0/10), with `find` OFF on both,
# so no row can come from mDNS. Routers a and b only give each home a gateway.
#
# The Tailscale app is a fake: one /usr/local/bin/tailscale (the second path
# `tailscale_app::CLI_PATHS` looks at) shared by every namespace in the lab
# container. It answers `status --json` with a document built from the
# overlay address of the namespace it runs in: `Self` is that address, user
# 111; the peer is the other node's overlay address, same user, macOS,
# online; plus a coworker (user 222) at an address nobody holds.
#
# Steps
#   1. install the fake tailscale, up both routers and both nodes
#   2. on both: tcr peer tailnet app
#   3. wait for each to list the other
#   4. node-a1 presses Trust on node-b1's tailnet address: knock, accept,
#      six digits on both screens
#
# Assertions
#   - each node's `tcr peer ls --json` holds a found row for the other node's
#     overlay address on the peer port, with "tailnet" in its sources and
#     not "lan" (find is off)
#   - each node's `tcr peer tailnet --json` held is app/ready, its address is
#     its own overlay address, and found >= 1
#   - the knock and the first pairing over the tailnet are answered (your own
#     Mac, on this Mac's own tailnet address, in app mode) and the pairing
#     pins
#   - node-a1's `tcr peer status --json` row for node-b1 carries
#     `tailnet.route = relayed` (the fake reports a relay, no direct address)
#
# Runs under --netlab only. The fake has to be installed at a fixed path, and
# under --native that path is the host's own /usr/local/bin.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="tailnet-find"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"

A1_OVERLAY="100.64.99.11"
B1_OVERLAY="100.64.99.12"
FAKE=/usr/local/bin/tailscale

if [ "${NETLAB:-0}" != "1" ] || [ ! -f /.dockerenv ]; then
  fail "tailnet-find runs under --netlab only: it installs a fake tailscale at $FAKE"
  finish
  exit 1
fi

TOPOLOGY="$HERE/../topologies/two-homes-overlay.json"
export TOPOLOGY
NETLAB_UPSTREAM="http://5.5.5.20:8080"
export NETLAB_UPSTREAM
# shellcheck source=../lib/netlab.sh
. "$HERE/../lib/netlab.sh"

if [ -e "$FAKE" ]; then
  fail "$FAKE already exists in the lab container; refusing to overwrite it"
  finish
  exit 1
fi
cat > "$FAKE" <<'FAKE_EOF'
#!/bin/sh
# A fake Tailscale CLI for tests/containers/scenarios/tailnet-find.sh.
[ "$1 $2" = "status --json" ] || exit 9
me="$(ip -4 -o addr show | awk '{print $4}' | cut -d/ -f1 | grep '^100\.64\.99\.' | head -n 1)"
case "$me" in
  100.64.99.11) other=100.64.99.12; other_name=laptop-2 ;;
  100.64.99.12) other=100.64.99.11; other_name=laptop-1 ;;
  *) echo "fake tailscale: no overlay address in this namespace" >&2; exit 1 ;;
esac
cat <<EOF
{"BackendState": "Running",
 "Self": {"HostName": "this-mac", "OS": "macOS", "UserID": 111, "TailscaleIPs": ["$me"]},
 "Peer": {
   "nodekey:01": {"HostName": "$other_name", "OS": "macOS", "UserID": 111, "Online": true,
                  "TailscaleIPs": ["$other"], "Tags": null, "CurAddr": "", "Relay": "fra"},
   "nodekey:02": {"HostName": "coworker", "OS": "macOS", "UserID": 222, "Online": true,
                  "TailscaleIPs": ["100.64.99.99"], "Tags": null, "CurAddr": "", "Relay": "fra"}
 }}
EOF
FAKE_EOF
chmod 755 "$FAKE"
pass "installed a fake tailscale at $FAKE"

up_with_routers router-a router-b -- node-a1-overlay node-b1-overlay || { finish; exit 1; }
pass "both routers and both nodes are up, find off on both"

for node in node-a1-overlay node-b1-overlay; do
  set_out="$(nx "$node" tcr peer tailnet app --peers "$PEERS" 2>&1 || true)"
  if [ "$set_out" = "peer.tailnet: app" ]; then
    pass "$node: tcr peer tailnet app"
  else
    fail "$node: tcr peer tailnet app said: $set_out"
    finish
    exit 1
  fi
done

# found_tailnet_row <ls.json> <address:port>: exit 0 when the listing holds a
# found row at that address whose sources include tailnet and not lan.
found_tailnet_row() {
  python3 - "$1" "$2" <<'PY'
import json, sys
listing = json.load(open(sys.argv[1]))
for row in listing.get("peers") or []:
    if "node" in row:
        continue
    sources = row.get("sources") or []
    if row.get("address") == sys.argv[2] and "tailnet" in sources and "lan" not in sources:
        sys.exit(0)
sys.exit(1)
PY
}

# A beacon sent before the far side bound its socket is lost, and one heard
# is folded in on the NEXT 20 s wake, so allow three wakes and some margin.
wait_found() {
  node="$1"; want="$2"; out="$SCRATCH/$node-ls.json"
  i=0
  while [ "$i" -lt 90 ]; do
    if ls_json "$node" "$out" && found_tailnet_row "$out" "$want"; then
      return 0
    fi
    sleep 1
    i=$((i + 1))
  done
  return 1
}

if wait_found node-a1-overlay "$B1_OVERLAY:7755"; then
  pass "node-a1-overlay lists node-b1-overlay at $B1_OVERLAY:7755 with sources [tailnet]"
else
  fail "node-a1-overlay has no tailnet row for $B1_OVERLAY:7755 within 90s: $(cat "$SCRATCH/node-a1-overlay-ls.json" 2>/dev/null)"
fi
if wait_found node-b1-overlay "$A1_OVERLAY:7755"; then
  pass "node-b1-overlay lists node-a1-overlay at $A1_OVERLAY:7755 with sources [tailnet]"
else
  fail "node-b1-overlay has no tailnet row for $A1_OVERLAY:7755 within 90s: $(cat "$SCRATCH/node-b1-overlay-ls.json" 2>/dev/null)"
fi

# held: what each serving process recorded on its last tick.
for pair in "node-a1-overlay $A1_OVERLAY" "node-b1-overlay $B1_OVERLAY"; do
  node="${pair% *}"; own="${pair#* }"
  held="$SCRATCH/$node-tailnet.json"
  nx "$node" tcr peer tailnet --peers "$PEERS" --json > "$held" 2>&1 || true
  if python3 - "$held" "$own" <<'PY'
import json, sys
held = json.load(open(sys.argv[1])).get("held") or {}
ok = (held.get("mode") == "app" and held.get("state") == "ready"
      and held.get("address") == sys.argv[2] and (held.get("found") or 0) >= 1)
sys.exit(0 if ok else 1)
PY
  then
    pass "$node: held is app/ready at $own with found >= 1"
  else
    fail "$node: held is not app/ready at $own with found >= 1: $(cat "$held")"
  fi
done

# --- Trust over the tailnet: your own Mac may knock (Gil, 2026-10-20) --------
# The knock and the first pairing come from a tailnet address, which the
# listener refuses with zero bytes from anyone else. node-a1 is node-b1's own
# Mac in the fake app's reading, and it arrives on node-b1's own tailnet
# address, so both are answered and the six digits decide.
if pair_nodes node-a1-overlay node-b1-overlay "$B1_OVERLAY:7755"; then
  pass "node-a1-overlay paired with node-b1-overlay over the tailnet with the six digits"
else
  fail "the pairing over the tailnet did not complete"
  finish
  exit 1
fi

# The paired row on node-a1 dials node-b1's tailnet address, and the running
# proxy's status says how the app reaches it: the fake reports a relay and no
# direct address, so `relayed`. The first tick after the pin records it, so
# allow one browse interval and some margin.
status_json="$SCRATCH/a1-status.json"
route=""
i=0
while [ "$i" -lt 45 ]; do
  nx node-a1-overlay tcr peer status --config /scratch/.config/tcr.json --json > "$status_json" 2>&1 || true
  route="$(python3 - "$status_json" "$B1_OVERLAY" <<'PY' || true
import json, sys
try:
    doc = json.load(open(sys.argv[1]))
except ValueError:
    sys.exit(1)
rows = doc.get("peers", doc) if isinstance(doc, dict) else doc
for row in rows or []:
    if str(row.get("address", "")).startswith(sys.argv[2] + ":"):
        tailnet = row.get("tailnet")
        if tailnet is not None:
            print(tailnet.get("route"))
            sys.exit(0)
sys.exit(1)
PY
)"
  [ "$route" = "relayed" ] && break
  sleep 1
  i=$((i + 1))
done
if [ "$route" = "relayed" ]; then
  pass "node-a1-overlay's status row for node-b1 carries tailnet.route = relayed"
else
  fail "node-a1-overlay's status row for node-b1 has no tailnet route [$route]: $(cat "$status_json")"
fi

finish
