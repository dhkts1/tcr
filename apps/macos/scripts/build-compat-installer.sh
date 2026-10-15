#!/usr/bin/env bash
# build-compat-installer.sh — build `teamclaude-rs-installer.sh` from a release's
# own `tcr-installer.sh`.
#
#   apps/macos/scripts/build-compat-installer.sh <tcr-installer.sh> <out>
#
# WHY THIS EXISTS. Until 1.2.0 the package was called `teamclaude-rs`, so a CLI
# on v1.1.21 or older runs `tcr update` like this (src/update.rs at v1.1.21):
# it fetches `releases/download/<latest tag>/teamclaude-rs-installer.sh` from
# dhkts1/teamclaude-rs (GitHub redirects that to dhkts1/tcr), scrubs the OLD
# override names from the child's environment, sets
# TEAMCLAUDE_RS_UNMANAGED_INSTALL=<directory of the running binary>, runs the
# script directly (so it needs a shebang), and then refuses to report success
# unless <that directory>/tcr exists. From 1.2.0 cargo-dist publishes
# `tcr-installer.sh` instead, which reads only TCR_* names. Without this file an
# old CLI's update is a 404. With the plain new installer under the old name it
# would ignore TEAMCLAUDE_RS_UNMANAGED_INSTALL and install a SECOND copy
# somewhere else, which is the failure src/update.rs exists to prevent.
#
# WHAT IT BUILDS. A short header, then the input installer byte for byte. The
# old CLI already trusts whatever this asset holds, so nothing is fetched here
# that the release does not itself contain. The header:
#
#   1. Unsets every TCR_* variable that can move where the installer writes,
#      where it downloads from, or what token it sends. v1.1.21 scrubs only the
#      TEAMCLAUDE_RS_* names, so a TCR_INSTALL_DIR or TCR_DOWNLOAD_URL left in
#      the user's shell would otherwise reach the new installer unscrubbed.
#      TCR_INSTALL_DIR ranks ABOVE TCR_UNMANAGED_INSTALL in the installer's
#      precedence chain and selects the layout that appends /bin, so an
#      inherited one is the second-copy failure again.
#   2. Carries over the two install-location names and nothing else:
#      TEAMCLAUDE_RS_UNMANAGED_INSTALL -> TCR_UNMANAGED_INSTALL and
#      TEAMCLAUDE_RS_INSTALL_DIR -> TCR_INSTALL_DIR. The URL and token names
#      (TEAMCLAUDE_RS_DOWNLOAD_URL, ..._INSTALLER_GHE_BASE_URL,
#      ..._INSTALLER_GITHUB_BASE_URL, ..._GITHUB_TOKEN) are never carried over:
#      they are the attack surface update.rs scrubs.
#
# So this file answers to the old installer's interface and installs the new
# release's binary. The names are kept here rather than in src/legacy_names.rs
# because no Rust code reads them.
#
# It can go once no v1.1.21-or-older CLI is expected to update (docs/RELEASING.md
# § "The teamclaude-rs-installer.sh compat asset").
set -euo pipefail

die() { printf 'build-compat-installer: error: %s\n' "$*" >&2; exit 1; }

[ $# -eq 2 ] || die "usage: build-compat-installer.sh <tcr-installer.sh> <out>"
in="$1" out="$2"
[ -f "$in" ] || die "$in does not exist"

# Refuse anything that is not a cargo-dist installer for the `tcr` app: a 404
# page or an old-name installer saved under the new name would otherwise ship
# as an executable that every old CLI runs.
[ "$(head -n 1 "$in")" = "#!/bin/sh" ] || die "$in does not start with #!/bin/sh"
grep -q '^APP_NAME="tcr"$' "$in" || die "$in is not the tcr installer (no APP_NAME=\"tcr\")"
grep -q 'TCR_UNMANAGED_INSTALL' "$in" || die "$in does not read TCR_UNMANAGED_INSTALL"

tmp="$(mktemp "${TMPDIR:-/tmp}/compat-installer.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

cat >"$tmp" <<'HEADER'
#!/bin/sh
# teamclaude-rs-installer.sh: a compatibility wrapper for `tcr update` on
# v1.1.21 and older, which still fetches the installer under its old name and
# passes the install directory as TEAMCLAUDE_RS_UNMANAGED_INSTALL. This header
# maps the old install-location names to the new ones; everything after it is
# this release's own tcr-installer.sh, unchanged. Built by
# apps/macos/scripts/build-compat-installer.sh in dhkts1/tcr.
unset TCR_INSTALL_DIR TCR_UNMANAGED_INSTALL TCR_DOWNLOAD_URL \
  TCR_INSTALLER_GHE_BASE_URL TCR_INSTALLER_GITHUB_BASE_URL TCR_GITHUB_TOKEN
if [ -n "${TEAMCLAUDE_RS_UNMANAGED_INSTALL:-}" ]; then
  TCR_UNMANAGED_INSTALL="$TEAMCLAUDE_RS_UNMANAGED_INSTALL"
  export TCR_UNMANAGED_INSTALL
fi
if [ -n "${TEAMCLAUDE_RS_INSTALL_DIR:-}" ]; then
  TCR_INSTALL_DIR="$TEAMCLAUDE_RS_INSTALL_DIR"
  export TCR_INSTALL_DIR
fi
# ---- tcr-installer.sh follows, byte for byte ----
HEADER
cat "$in" >>"$tmp"

# The header must not have eaten or altered a byte of the installer.
header_bytes=$(( $(wc -c <"$tmp") - $(wc -c <"$in") ))
tail -c +"$((header_bytes + 1))" "$tmp" | cmp -s - "$in" || die "the built file does not end with $in byte for byte"
sh -n "$tmp" || die "the built file does not parse as sh"

chmod 755 "$tmp"
mv "$tmp" "$out"
trap - EXIT
printf 'built %s (%s-byte header + %s)\n' "$out" "$header_bytes" "$in"
