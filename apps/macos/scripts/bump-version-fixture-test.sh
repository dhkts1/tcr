#!/usr/bin/env bash
# Fixture test for bump-version.sh, the Cargo.toml + Cargo.lock edit that
# ship-release.sh commits as the "bump to <next>" PR after every release.
#
# Copies the tracked tree (working-tree contents, not HEAD) into a scratch
# directory, runs the real bump-version.sh there from the current version to
# the next patch, then asks cargo to resolve the result with `--locked`, the
# flag every CI job builds with. A bump that moves one package in the lock and
# not another passes `cargo metadata --no-deps` and fails every `--locked` job;
# this test exists to catch that.
#
# Run directly: apps/macos/scripts/bump-version-fixture-test.sh
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/bump-version-fixture.XXXXXX")"
trap 'rm -rf "$work"' EXIT

fail=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; fail=1; }

git -C "$root" ls-files -z | (cd "$root" && tar --null -T - -cf -) | tar -xf - -C "$work"

version="$(awk -F'"' '/^version = "/ { print $2; exit }' "$work/Cargo.toml")"
[[ "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || { echo "FAIL: no X.Y.Z version in Cargo.toml: '$version'" >&2; exit 1; }
next="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((BASH_REMATCH[3] + 1))"

# The precondition: the copied tree resolves before the bump, so a red below is
# the bump's doing.
if cargo metadata --manifest-path "$work/Cargo.toml" --format-version 1 --locked >/dev/null 2>"$work/pre.err"; then
  pass "the copied tree resolves with --locked at $version"
else
  cat "$work/pre.err" >&2
  echo "FAIL: the copied tree does not resolve with --locked before any bump" >&2
  exit 1
fi

if "$here/bump-version.sh" "$work" "$version" "$next" 2>"$work/bump.err"; then
  pass "bump-version.sh $version -> $next exited 0"
else
  cat "$work/bump.err" >&2
  fail "bump-version.sh $version -> $next refused the bump"
fi

if cargo metadata --manifest-path "$work/Cargo.toml" --format-version 1 --locked >/dev/null 2>"$work/post.err"; then
  pass "the bumped tree resolves with --locked"
else
  cat "$work/post.err" >&2
  fail "the bumped tree does not resolve with --locked: Cargo.lock is stale"
fi

# Every workspace package that inherits the workspace version moved, in the
# lock as well as the manifest.
for pkg in tcr tcr-lib; do
  got="$(awk -v name="name = \"$pkg\"" '$0 == name { getline; print; exit }' "$work/Cargo.lock")"
  if [ "$got" = "version = \"$next\"" ]; then
    pass "Cargo.lock has $pkg at $next"
  else
    fail "Cargo.lock has $pkg at '$got', not $next"
  fi
done

exit "$fail"
