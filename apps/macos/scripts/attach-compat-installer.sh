#!/usr/bin/env bash
# attach-compat-installer.sh — give a published release its
# `teamclaude-rs-installer.sh`, built from that release's own `tcr-installer.sh`.
#
#   apps/macos/scripts/attach-compat-installer.sh <vX.Y.Z> [--repo owner/name]
#
# Why the file exists at all: build-compat-installer.sh's header. In short, a
# CLI on v1.1.21 or older updates by fetching `teamclaude-rs-installer.sh` from
# the latest release, so every release from 1.2.0 on must carry one, and it must
# be THIS release's installer under the old variable names.
#
# release-tcrbar.sh stage 9 calls this while the release is still marked
# prerelease, so `/releases/latest/` never points an old CLI at a release
# without the file. Run it by hand for a release that skipped stage 9 (a
# CLI-only release) or to repair one; it is idempotent (--clobber).
#
# Steps, each refusing rather than guessing:
#   1. Wait for `tcr-installer.sh` to be on the release. The tag-triggered
#      cargo-dist workflow creates the release with every asset in one call, so
#      this normally returns at once; the wait covers a hand run right after
#      the tag push.
#   2. Download it and check it is the installer for THIS version.
#   3. Build the compat file (build-compat-installer.sh).
#   4. Upload with --clobber, download it back and compare bytes: gh's exit
#      status says the upload went through, not what the release serves.
#
# Environment:
#   RELEASE_REPO               owner/name (default dhkts1/tcr); --repo wins
#   COMPAT_INSTALLER_WAIT      seconds to wait for tcr-installer.sh (default 600)
set -euo pipefail

die() { printf 'attach-compat-installer: error: %s\n' "$*" >&2; exit 1; }
note() { printf '    %s\n' "$*"; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The asset name every v1.1.21-or-older CLI fetches (src/update.rs INSTALLER_ASSET
# at v1.1.21). Kept here, not in src/legacy_names.rs: no Rust code reads it.
compat_name="teamclaude-rs-installer.sh"
installer_name="tcr-installer.sh"

tag="" repo="${RELEASE_REPO:-dhkts1/tcr}"
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) repo="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,5p' "${BASH_SOURCE[0]}"; exit 0 ;;
    -*) die "unknown flag: $1" ;;
    *) [ -z "$tag" ] || die "one tag only (got '$tag' and '$1')"; tag="$1"; shift ;;
  esac
done
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: attach-compat-installer.sh vX.Y.Z [--repo owner/name]"
[ -n "$repo" ] || die "--repo needs owner/name"
command -v gh >/dev/null || die "gh is not installed"

work="$(mktemp -d "${TMPDIR:-/tmp}/attach-compat.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# 1. wait for the installer asset
wait_seconds="${COMPAT_INSTALLER_WAIT:-600}"
waited=0
# The asset list is captured, not piped into `grep -q`: under pipefail, grep
# quitting early can SIGPIPE gh and read as "not there yet" forever.
has_installer() {
  local names
  names="$(gh api "repos/$repo/releases/tags/$tag" --jq '.assets[].name' 2>/dev/null)" || return 1
  grep -qx "$installer_name" <<<"$names"
}
until has_installer; do
  [ "$waited" -lt "$wait_seconds" ] \
    || die "$tag on $repo still has no $installer_name after ${wait_seconds}s; check: gh run list --repo $repo --branch $tag"
  [ "$waited" = 0 ] && note "waiting for $installer_name on ${tag}…"
  sleep 10
  waited=$((waited + 10))
done

# 2. download it and check it is this version's installer
gh release download "$tag" --repo "$repo" --pattern "$installer_name" --dir "$work/in" >/dev/null \
  || die "could not download $installer_name from $tag"
grep -qx "APP_VERSION=\"${tag#v}\"" "$work/in/$installer_name" \
  || die "$tag's $installer_name is not the ${tag#v} installer (no APP_VERSION=\"${tag#v}\")"

# 3. build
"$here/build-compat-installer.sh" "$work/in/$installer_name" "$work/$compat_name" >/dev/null \
  || die "could not build $compat_name from $tag's $installer_name"

# 4. upload, read back, compare
gh release upload "$tag" "$work/$compat_name" --clobber --repo "$repo" >/dev/null \
  || die "could not upload $compat_name to $tag; re-run: $0 $tag --repo $repo"
gh release download "$tag" --repo "$repo" --pattern "$compat_name" --dir "$work/check" >/dev/null \
  || die "uploaded, but $compat_name on $tag cannot be read back to check it"
cmp -s "$work/check/$compat_name" "$work/$compat_name" \
  || die "$tag carries a $compat_name that is not the one just built; re-run: $0 $tag --repo $repo"
note "$tag carries $compat_name, built from its own $installer_name (read back and compared)"
