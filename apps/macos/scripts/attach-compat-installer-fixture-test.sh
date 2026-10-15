#!/usr/bin/env bash
# Fixture test for attach-compat-installer.sh against a stub `gh` on PATH. No
# network, no real release: the stub keeps a release's assets in a directory.
#
# Run directly: apps/macos/scripts/attach-compat-installer-fixture-test.sh
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
attach="$here/attach-compat-installer.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/attach-compat-fixture.XXXXXX")"
trap 'rm -rf "$work"' EXIT
# Not a real repository, so even a gh that somehow is not the stub writes nothing.
repo="example-owner/example-not-a-repo"

fail=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; fail=1; }

mkdir -p "$work/bin"
cat >"$work/bin/gh" <<'STUB'
#!/usr/bin/env bash
# Stub gh: one release whose assets live in $STUB_ASSETS.
set -euo pipefail
case "$1 ${2:-}" in
  "api "*)
    [ -d "$STUB_ASSETS" ] || exit 1
    ls "$STUB_ASSETS"
    ;;
  "release download")
    shift 3; pattern="" dir=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --pattern) pattern="$2"; shift 2 ;;
        --dir) dir="$2"; shift 2 ;;
        --repo) shift 2 ;;
        *) shift ;;
      esac
    done
    [ -f "$STUB_ASSETS/$pattern" ] || exit 1
    mkdir -p "$dir"; cp "$STUB_ASSETS/$pattern" "$dir/"
    ;;
  "release upload")
    shift 3
    while [ $# -gt 0 ]; do
      case "$1" in
        --clobber) shift ;;
        --repo) shift 2 ;;
        *) cp "$1" "$STUB_ASSETS/"; echo "$(basename "$1")" >>"$STUB_UPLOADS"; shift ;;
      esac
    done
    ;;
  *) echo "stub gh: unexpected: $*" >&2; exit 64 ;;
esac
STUB
chmod 755 "$work/bin/gh"

installer() {  # <version> <path>: a minimal file that passes the builder's checks
  # shellcheck disable=SC2016  # the ${…} is meant literally: it is installer text
  printf '#!/bin/sh\nAPP_NAME="tcr"\nAPP_VERSION="%s"\nUNMANAGED_INSTALL="${TCR_UNMANAGED_INSTALL:-}"\n' "$1" >"$2"
}
run() { PATH="$work/bin:$PATH" STUB_ASSETS="$work/assets" STUB_UPLOADS="$work/uploads" COMPAT_INSTALLER_WAIT=0 "$attach" "$@"; }
reset() { rm -rf "${work:?}/assets" "${work:?}/uploads"; mkdir -p "$work/assets"; : >"$work/uploads"; }

# 1. happy path: the uploaded file is the builder's output for this release's installer
reset
installer 1.2.0 "$work/assets/tcr-installer.sh"
"$here/build-compat-installer.sh" "$work/assets/tcr-installer.sh" "$work/expected.sh" >/dev/null
if run v1.2.0 --repo "$repo" >"$work/log" 2>&1 \
   && cmp -s "$work/assets/teamclaude-rs-installer.sh" "$work/expected.sh"; then
  pass "attaches teamclaude-rs-installer.sh built from the release's own tcr-installer.sh"
else
  fail "happy path: $(cat "$work/log")"
fi

# 2. the release's installer is for another version -> refuse, upload nothing
reset
installer 1.1.99 "$work/assets/tcr-installer.sh"
if run v1.2.0 --repo "$repo" >"$work/log" 2>&1; then
  fail "attached a compat file built from another version's installer"
elif [ -s "$work/uploads" ]; then
  fail "refused, but uploaded first: $(cat "$work/uploads")"
else
  pass "refuses an installer whose APP_VERSION is not the tag's, before uploading"
fi

# 3. no tcr-installer.sh on the release -> give up, upload nothing
reset
if run v1.2.0 --repo "$repo" >"$work/log" 2>&1; then
  fail "succeeded with no tcr-installer.sh on the release"
elif [ -s "$work/uploads" ]; then
  fail "no installer, but uploaded: $(cat "$work/uploads")"
else
  pass "gives up when the release has no tcr-installer.sh"
fi

# 4. a bad tag is refused before any gh call
reset
if run 1.2.0 --repo "$repo" >"$work/log" 2>&1; then
  fail "accepted a tag without the v prefix"
else
  pass "refuses a tag that is not vX.Y.Z"
fi

exit "$fail"
