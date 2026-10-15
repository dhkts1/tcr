#!/usr/bin/env bash
# Fixture test for build-compat-installer.sh. No network.
#
#   apps/macos/scripts/compat-installer-fixture-test.sh [<real tcr-installer.sh>]
#
# A fake tcr-installer.sh stands in for the cargo-dist one: it reimplements the
# installer's install-directory precedence (TCR_INSTALL_DIR, then
# CARGO_DIST_FORCE_INSTALL_DIR + /bin, then TCR_UNMANAGED_INSTALL, else a
# default directory) and writes `tcr` there, plus a record of the URL and token
# variables it saw. The built compat file is then run the way v1.1.21's
# `tcr update` runs it: executed directly, with TEAMCLAUDE_RS_UNMANAGED_INSTALL
# set to the running binary's directory.
#
# With an argument, it also builds from a REAL generated installer (for example
# `dist build --artifacts=global` output, target/distrib/tcr-installer.sh) and
# installs a stub tarball served from a file:// URL into a scratch directory,
# with HOME pointed at scratch too. That proves the real installer's precedence,
# not just the fake's.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
builder="$here/build-compat-installer.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/compat-installer-fixture.XXXXXX")"
trap 'rm -rf "$work"' EXIT

fail=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; fail=1; }

# Run a command with a clean environment plus the given NAME=VALUE pairs, the
# way a child of a scrubbed `Command` sees it.
run_clean() { env -i PATH="$PATH" HOME="$work/home" "$@"; }

# ---- the fake installer -----------------------------------------------------
fake="$work/tcr-installer.sh"
cat >"$fake" <<'FAKE'
#!/bin/sh
# shellcheck shell=dash
APP_NAME="tcr"
# Stand-in for cargo-dist's installer. Reads TCR_UNMANAGED_INSTALL like the real one.
UNMANAGED_INSTALL="${TCR_UNMANAGED_INSTALL:-}"
if [ -n "${TCR_INSTALL_DIR:-}" ]; then
  d="$TCR_INSTALL_DIR/bin"
elif [ -n "${CARGO_DIST_FORCE_INSTALL_DIR:-}" ]; then
  d="$CARGO_DIST_FORCE_INSTALL_DIR/bin"
elif [ -n "$UNMANAGED_INSTALL" ]; then
  d="$UNMANAGED_INSTALL"
else
  d="$FAKE_DEFAULT_DIR"
fi
mkdir -p "$d"
echo installed >"$d/tcr"
{
  echo "TCR_DOWNLOAD_URL=${TCR_DOWNLOAD_URL:-}"
  echo "TCR_GITHUB_TOKEN=${TCR_GITHUB_TOKEN:-}"
  echo "TCR_INSTALLER_GHE_BASE_URL=${TCR_INSTALLER_GHE_BASE_URL:-}"
  echo "TCR_INSTALLER_GITHUB_BASE_URL=${TCR_INSTALLER_GITHUB_BASE_URL:-}"
} >"$FAKE_RECORD"
FAKE
chmod 755 "$fake"

compat="$work/teamclaude-rs-installer.sh"
"$builder" "$fake" "$compat" >/dev/null || { fail "the builder refused the fake installer"; exit 1; }

# Every file named tcr anywhere an install could land (the default dir, the
# intended dir, the decoy dir, and HOME), one per line. Not the whole of $work:
# case 8's stub tarball tree holds a tcr too.
installs() {
  (cd "$work" && for d in default target elsewhere home; do
     [ -d "$d" ] && find "./$d" -name tcr -type f
   done | sort)
}
reset() { rm -rf "${work:?}/default" "${work:?}/target" "${work:?}/elsewhere" "${work:?}/home" "${work:?}/record"; mkdir -p "$work/home"; }

# ---- 1. control: the fake alone, old name only -> default dir ---------------
# Proves the fake ignores TEAMCLAUDE_RS_UNMANAGED_INSTALL, so case 2 passing is
# the compat header's doing and not the fake's.
reset
run_clean FAKE_DEFAULT_DIR="$work/default" FAKE_RECORD="$work/record" \
  TEAMCLAUDE_RS_UNMANAGED_INSTALL="$work/target" "$fake"
if [ "$(installs)" = "./default/tcr" ]; then
  pass "control: the plain new installer ignores the old name and installs to its default"
else
  fail "control: the plain new installer with the old name installed to: $(installs | tr '\n' ' ')"
fi

# ---- 2. the compat file, as v1.1.21 runs it -> exactly <dir>/tcr ------------
reset
run_clean FAKE_DEFAULT_DIR="$work/default" FAKE_RECORD="$work/record" \
  TEAMCLAUDE_RS_UNMANAGED_INSTALL="$work/target" "$compat"
if [ "$(installs)" = "./target/tcr" ]; then
  pass "compat: TEAMCLAUDE_RS_UNMANAGED_INSTALL=<dir> installs <dir>/tcr and nothing else"
else
  fail "compat: expected only ./target/tcr, got: $(installs | tr '\n' ' ')"
fi

# ---- 3. URL and token names are never carried over --------------------------
reset
run_clean FAKE_DEFAULT_DIR="$work/default" FAKE_RECORD="$work/record" \
  TEAMCLAUDE_RS_UNMANAGED_INSTALL="$work/target" \
  TEAMCLAUDE_RS_DOWNLOAD_URL="http://attacker.invalid/dl" \
  TEAMCLAUDE_RS_GITHUB_TOKEN="fake-token" \
  TEAMCLAUDE_RS_INSTALLER_GHE_BASE_URL="http://attacker.invalid/ghe" \
  TEAMCLAUDE_RS_INSTALLER_GITHUB_BASE_URL="http://attacker.invalid/gh" \
  "$compat"
if grep -q 'attacker\|fake-token' "$work/record"; then
  fail "an old URL or token name became a TCR_ one: $(tr '\n' ' ' <"$work/record")"
else
  pass "TEAMCLAUDE_RS_DOWNLOAD_URL/_GITHUB_TOKEN/_GHE_/_GITHUB_BASE_URL do not become TCR_ ones"
fi

# ---- 4. inherited TCR_ overrides are cleared --------------------------------
# v1.1.21 scrubs only the old names. A TCR_INSTALL_DIR left in the user's shell
# outranks TCR_UNMANAGED_INSTALL and would install <x>/bin/tcr, a second copy.
reset
run_clean FAKE_DEFAULT_DIR="$work/default" FAKE_RECORD="$work/record" \
  TEAMCLAUDE_RS_UNMANAGED_INSTALL="$work/target" \
  TCR_INSTALL_DIR="$work/elsewhere" \
  TCR_DOWNLOAD_URL="http://attacker.invalid/dl" \
  TCR_GITHUB_TOKEN="fake-token" \
  "$compat"
if [ "$(installs)" = "./target/tcr" ] && ! grep -q 'attacker\|fake-token' "$work/record"; then
  pass "an inherited TCR_INSTALL_DIR/TCR_DOWNLOAD_URL/TCR_GITHUB_TOKEN does not reach the installer"
else
  fail "inherited TCR_ overrides leaked: installs=$(installs | tr '\n' ' ') record=$(tr '\n' ' ' <"$work/record")"
fi

# ---- 5. TEAMCLAUDE_RS_INSTALL_DIR carries over -------------------------------
reset
run_clean FAKE_DEFAULT_DIR="$work/default" FAKE_RECORD="$work/record" \
  TEAMCLAUDE_RS_INSTALL_DIR="$work/elsewhere" "$compat"
if [ "$(installs)" = "./elsewhere/bin/tcr" ]; then
  pass "TEAMCLAUDE_RS_INSTALL_DIR carries over to TCR_INSTALL_DIR"
else
  fail "TEAMCLAUDE_RS_INSTALL_DIR: expected ./elsewhere/bin/tcr, got: $(installs | tr '\n' ' ')"
fi

# ---- 6. the built file is header + the installer, byte for byte -------------
size_in=$(wc -c <"$fake")
if tail -c "$size_in" "$compat" | cmp -s - "$fake" && [ "$(head -n 1 "$compat")" = "#!/bin/sh" ] && [ -x "$compat" ]; then
  pass "the built file is executable, starts with #!/bin/sh and ends with the installer unchanged"
else
  fail "the built file is not an executable #!/bin/sh header followed by the installer bytes"
fi

# ---- 7. the builder refuses something that is not the tcr installer ---------
# GitHub's body for a missing release asset. It parses as sh (`Not` run with an
# argument), so only the builder's own checks can refuse it.
printf 'Not Found' >"$work/notfound.sh"
if "$builder" "$work/notfound.sh" "$work/bad.sh" >/dev/null 2>&1 || [ -e "$work/bad.sh" ]; then
  fail "the builder accepted a 404 page as an installer"
else
  pass "the builder refuses an input that is not the tcr installer"
fi

# ---- 8. optional: the REAL generated installer, offline ---------------------
real="${1:-}"
if [ -n "$real" ]; then
  arch="$(uname -m)"; os="$(uname -s)"
  case "$os-$arch" in
    Darwin-arm64) triple=aarch64-apple-darwin ;;
    Darwin-x86_64) triple=x86_64-apple-darwin ;;
    Linux-aarch64) triple=aarch64-unknown-linux-musl ;;
    Linux-x86_64) triple=x86_64-unknown-linux-musl ;;
    *) fail "real: no target triple for $os-$arch"; triple="" ;;
  esac
  if [ -n "$triple" ]; then
    reset
    mkdir -p "$work/srv/tcr-$triple"
    printf '#!/bin/sh\necho stub-tcr\n' >"$work/srv/tcr-$triple/tcr"
    chmod 755 "$work/srv/tcr-$triple/tcr"
    tar -C "$work/srv" -cJf "$work/srv/tcr-$triple.tar.xz" "tcr-$triple"
    "$builder" "$real" "$work/real-compat.sh" >/dev/null
    # INSTALLER_DOWNLOAD_URL is the shared name the old CLI also scrubs; it is
    # used here only to serve the stub tarball from disk. The header leaves it
    # alone, exactly as the old installer did.
    if run_clean INSTALLER_DOWNLOAD_URL="file://$work/srv" \
         TEAMCLAUDE_RS_UNMANAGED_INSTALL="$work/target" \
         TCR_INSTALL_DIR="$work/elsewhere" \
         "$work/real-compat.sh" >"$work/real.log" 2>&1 \
       && [ "$(installs)" = "./target/tcr" ] \
       && [ "$("$work/target/tcr")" = stub-tcr ]; then
      pass "real installer: installs <dir>/tcr only, with TCR_INSTALL_DIR inherited"
    else
      fail "real installer: installs=$(installs | tr '\n' ' '); log follows"
      sed 's/^/    /' "$work/real.log" >&2
    fi
    # Control: the real tcr-installer.sh on its own, given only the old name,
    # installs under the scratch HOME instead: the second copy.
    rm -rf "${work:?}/target" "${work:?}/elsewhere" "${work:?}/home"; mkdir -p "$work/home"
    run_clean INSTALLER_DOWNLOAD_URL="file://$work/srv" \
      TEAMCLAUDE_RS_UNMANAGED_INSTALL="$work/target" sh "$real" >"$work/real-control.log" 2>&1 || true
    got="$(installs | tr '\n' ' ')"
    case "$got" in
      ./home/*) pass "real control: the plain tcr-installer.sh ignores the old name ($got)" ;;
      *) fail "real control: expected an install under the scratch HOME, got: $got" ;;
    esac
  fi
fi

exit "$fail"
