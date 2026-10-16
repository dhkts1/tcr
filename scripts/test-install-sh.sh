#!/usr/bin/env bash
# test-install-sh.sh — proves install.sh's TcrBar step survives a real
# `curl | sh` with no local checkout to fall back to.
#
# Runs the target install.sh through `sh` on stdin, from an empty directory —
# exactly the shape of the README's one-liner (`curl … | sh`), never
# `bash install.sh`, because BASH_SOURCE[0] resolving under a checkout is
# precisely the bug this test exists to catch. A local http server stands in
# for github.com/api.github.com/raw.githubusercontent.com so the run touches
# no real network and no real /Applications:
#   - a fake TcrBar.app packed into a real dmg with hdiutil (needs macOS)
#   - a fake releases/latest JSON
#   - a copy of scripts/install-tcrbar-from-dmg.sh, standing in for the raw
#     GitHub fetch install.sh does when it finds no local checkout
#
# Usage: scripts/test-install-sh.sh [path-to-install.sh]
#   Defaults to this repo's own install.sh. Pass a copy of an older
#   install.sh (e.g. `git show main:install.sh` written to a temp file) to
#   prove the test fails against the pre-fix script.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SH="${1:-$REPO_ROOT/install.sh}"

if [ ! -f "$INSTALL_SH" ]; then
  echo "no install.sh at $INSTALL_SH" >&2
  exit 1
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo "this test needs hdiutil — macOS only" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
SERVER_PID=""

# shellcheck disable=SC2329 # invoked indirectly via `trap cleanup EXIT` below
cleanup() {
  rc=$?
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" >/dev/null 2>&1 || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORKDIR"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

APP_NAME="TcrBar"
VERSION="0.0.0-test"
TAG="v${VERSION}"

SERVER_ROOT="$WORKDIR/server"
FAKE_APP="$WORKDIR/fake-app/${APP_NAME}.app"
mkdir -p "$FAKE_APP/Contents/MacOS" "$SERVER_ROOT/releases/download/${TAG}"

cat > "$FAKE_APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>${APP_NAME}</string>
<key>CFBundleIdentifier</key><string>com.example.${APP_NAME}</string>
<key>CFBundleShortVersionString</key><string>${VERSION}</string>
</dict></plist>
PLIST

cat > "$FAKE_APP/Contents/MacOS/${APP_NAME}" <<'BIN'
#!/bin/sh
exit 0
BIN
chmod +x "$FAKE_APP/Contents/MacOS/${APP_NAME}"

# install-tcrbar-from-dmg.sh's own codesign -v --deep --strict check refuses
# an unsigned bundle — ad-hoc sign the fake app so the test exercises that
# check as a real pass, not a skip.
codesign --force --deep --sign - "$FAKE_APP" >/dev/null 2>&1

DMG_PATH="$SERVER_ROOT/releases/download/${TAG}/${APP_NAME}-${VERSION}.dmg"
echo "==> building fake dmg at $DMG_PATH"
if ! hdiutil create -volname "$APP_NAME" -srcfolder "$FAKE_APP" -ov -format UDZO "$DMG_PATH" >/dev/null; then
  echo "hdiutil create failed" >&2
  exit 1
fi

cat > "$SERVER_ROOT/releases/latest" <<JSON
{"tag_name": "${TAG}"}
JSON

cp "$REPO_ROOT/scripts/install-tcrbar-from-dmg.sh" "$SERVER_ROOT/install-tcrbar-from-dmg.sh"

PORT=$(( (RANDOM % 20000) + 20000 ))
(
  cd "$SERVER_ROOT" || exit 1
  exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1
) &
SERVER_PID=$!

up=0
for _ in $(seq 1 50); do
  if curl -fsS "http://127.0.0.1:${PORT}/releases/latest" >/dev/null 2>&1; then
    up=1
    break
  fi
  sleep 0.1
done
if [ "$up" -ne 1 ]; then
  echo "local http server on :${PORT} never came up" >&2
  exit 1
fi

RUN_DIR="$WORKDIR/run"
APPLICATIONS_DIR="$WORKDIR/Applications"
mkdir -p "$RUN_DIR" "$APPLICATIONS_DIR"

# The candidate install.sh's own "already installed and running" guard reads
# the REAL /Applications and shells out to the REAL pgrep — on a dev machine
# that already has TcrBar running (see this repo's CLAUDE.md) that guard
# fires and the script exits before ever reaching the code path this test
# exists to exercise, regardless of which install.sh is under test. This test
# only cares whether TcrBar.app lands in the FAKE Applications dir below, so
# shadow pgrep with one that always reports "no match" — never touches or
# signals the real process, just answers the question install.sh asks.
FAKEBIN="$WORKDIR/fakebin"
mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/pgrep" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod +x "$FAKEBIN/pgrep"

export TCR_SKIP_CLI=1
export TCR_APPLICATIONS_DIR="$APPLICATIONS_DIR"
# install.sh tries the redirect of releases/latest before the API; point it
# at a closed local port so that lookup fails fast and falls through to the
# local fake API below, keeping this test hermetic (no real github.com call).
export TCR_LATEST_RELEASE_URL="http://127.0.0.1:9/dead"
export TCR_LATEST_RELEASE_API_URL="http://127.0.0.1:${PORT}/releases/latest"
export TCR_DMG_URL_BASE="http://127.0.0.1:${PORT}/releases/download"
export TCR_DMG_INSTALL_SCRIPT_URL="http://127.0.0.1:${PORT}/install-tcrbar-from-dmg.sh"
export PATH="$FAKEBIN:$PATH"

# A fake `open` records its argv and a fake `launchctl` answers managername.
# Both sit first on PATH, so the real TcrBar is never launched: every case
# also asserts the recorded call came from THIS fake, via the marker file.
cat > "$FAKEBIN/open" <<'EOF2'
#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_OPEN_LOG"
exit "${FAKE_OPEN_RC:-0}"
EOF2
cat > "$FAKEBIN/launchctl" <<'EOF2'
#!/bin/sh
[ "$1" = "managername" ] && echo "${FAKE_MANAGER:-Aqua}"
exit 0
EOF2
chmod +x "$FAKEBIN/open" "$FAKEBIN/launchctl"
export FAKE_OPEN_LOG="$WORKDIR/open.log"

# A dmg-install script that always fails, for the failing-app-step case.
printf '#!/bin/sh\nexit 1\n' > "$SERVER_ROOT/failing-install.sh"

FAILS=0
check() { # name, ok(0/1), detail
  if [ "$2" -eq 0 ]; then echo "PASS: $1"; else echo "FAIL: $1 ($3)" >&2; FAILS=$((FAILS + 1)); fi
}

# run_case <name> <script-url> [VAR=value ...]; leaves OUT, RC, OPENS set.
run_case() {
  name="$1"; script_url="$2"; shift 2
  rm -rf "$APPLICATIONS_DIR/${APP_NAME}.app" "$FAKE_OPEN_LOG"
  : > "$FAKE_OPEN_LOG"
  echo "==> case: $name"
  OUT="$(cd "$RUN_DIR" && env -u CI -u SSH_CONNECTION -u SSH_TTY -u TCR_NO_LAUNCH \
    TCR_DMG_INSTALL_SCRIPT_URL="$script_url" "$@" "${TCR_TEST_SHELL:-sh}" < "$INSTALL_SH" 2>&1)"
  RC=$?
  OPENS="$(cat "$FAKE_OPEN_LOG")"
  echo "$OUT" | sed 's/^/    | /'
}

GOOD_URL="$TCR_DMG_INSTALL_SCRIPT_URL"
FAIL_URL="http://127.0.0.1:${PORT}/failing-install.sh"
WANT_APP="$APPLICATIONS_DIR/${APP_NAME}.app"

run_case "success opens the app" "$GOOD_URL"
check "installs the app" "$([ -d "$WANT_APP" ]; echo $?)" "no $WANT_APP"
check "exit 0" "$([ "$RC" -eq 0 ]; echo $?)" "rc=$RC"
check "fake open called with exactly the app path" "$([ "$OPENS" = "$WANT_APP" ]; echo $?)" "got '$OPENS'"
check "says look for the menu-bar icon" "$(echo "$OUT" | grep -q 'icon at the top right'; echo $?)" "no hint"

for skip in "TCR_NO_LAUNCH=1" "CI=true" "SSH_CONNECTION=1.2.3.4" "SSH_TTY=/dev/ttys000" "FAKE_MANAGER=Background"; do
  run_case "skip on $skip" "$GOOD_URL" "$skip"
  check "$skip: app still installed" "$([ -d "$WANT_APP" ]; echo $?)" "no app"
  check "$skip: open not called" "$([ -z "$OPENS" ]; echo $?)" "open ran: $OPENS"
  check "$skip: says Not opening" "$(echo "$OUT" | grep -q 'Not opening'; echo $?)" "no skip line"
done

run_case "failed open is a warning" "$GOOD_URL" FAKE_OPEN_RC=1
check "failed open: exit 0" "$([ "$RC" -eq 0 ]; echo $?)" "rc=$RC"
check "failed open: warns" "$(echo "$OUT" | grep -q 'could not open'; echo $?)" "no warning"

run_case "failing app step" "$FAIL_URL"
check "failing step: nonzero exit" "$([ "$RC" -ne 0 ]; echo $?)" "rc=$RC"
check "failing step: no Installed line" "$(echo "$OUT" | grep -q '^Installed:'; [ $? -ne 0 ]; echo $?)" "printed Installed"
check "failing step: says it failed" "$(echo "$OUT" | grep -q 'install failed'; echo $?)" "no message"
check "failing step: open not called" "$([ -z "$OPENS" ]; echo $?)" "open ran: $OPENS"

# The CLI step (TCR_SKIP_CLI off): a fake cargo-dist installer served locally.
printf '#!/bin/sh\nexit 3\n' > "$SERVER_ROOT/cli-fail.sh"
printf '#!/bin/sh\necho fake-dist-ok\nexit 0\n' > "$SERVER_ROOT/cli-ok.sh"

run_case "CLI installer exits 3" "$GOOD_URL" TCR_SKIP_CLI=0 TCR_DIST_INSTALLER_URL="http://127.0.0.1:${PORT}/cli-fail.sh"
check "CLI failure: exit is exactly 3" "$([ "$RC" -eq 3 ]; echo $?)" "rc=$RC"
check "CLI failure: says exit 3" "$(echo "$OUT" | grep -q 'tcr CLI install failed (exit 3)'; echo $?)" "no message"
check "CLI failure: app step not reached" "$([ ! -d "$WANT_APP" ] && [ -z "$OPENS" ]; echo $?)" "app step ran"

run_case "CLI installer exits 0" "$GOOD_URL" TCR_SKIP_CLI=0 TCR_DIST_INSTALLER_URL="http://127.0.0.1:${PORT}/cli-ok.sh"
check "CLI ok: dist installer ran" "$(echo "$OUT" | grep -q fake-dist-ok; echo $?)" "fake did not run"
check "CLI ok: exit 0 and app installed" "$([ "$RC" -eq 0 ] && [ -d "$WANT_APP" ]; echo $?)" "rc=$RC"

if [ "$FAILS" -ne 0 ]; then
  echo "$FAILS check(s) failed" >&2
  exit 1
fi
echo "all checks passed"
