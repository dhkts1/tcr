#!/usr/bin/env bash
# bump-version.sh: move the workspace from one release version to the next in
# Cargo.toml and Cargo.lock, the edit ship-release.sh's step 5 commits.
#
#   apps/macos/scripts/bump-version.sh <root> <X.Y.Z> <next X.Y.Z>
#
# Its own file so apps/macos/scripts/bump-version-fixture-test.sh can run the
# exact edit on a copy of the tree; ship-release.sh only calls it.
set -euo pipefail

die() { printf 'bump-version: error: %s\n' "$*" >&2; exit 1; }

[ $# -eq 3 ] || die "usage: bump-version.sh <root> <X.Y.Z> <next X.Y.Z>"
root="$1" version="$2" next="$3"

cargo_version() { awk -F'"' '/^version = "/ { print $2; exit }' "$root/Cargo.toml"; }

V="$version" N="$next" perl -0pi -e 's/^version = "\Q$ENV{V}\E"/version = "$ENV{N}"/m' "$root/Cargo.toml"
# The first `version =` line is `[workspace.package]`'s, so it moves every
# package that inherits it: the `tcr` binary and the `tcr-lib` library. Both
# lock entries move with it.
V="$version" N="$next" perl -0pi -e 's/(name = "tcr(?:-lib)?"\nversion = ")\Q$ENV{V}\E"/$1$ENV{N}"/g' "$root/Cargo.lock"
[ "$(cargo_version)" = "$next" ] || die "Cargo.toml was not moved to $next"
for pkg in tcr tcr-lib; do
  grep -A1 "^name = \"$pkg\"\$" "$root/Cargo.lock" | grep -q "^version = \"$next\"$" \
    || die "Cargo.lock was not moved to $next for $pkg"
done
# Resolving, not `--no-deps`: only a resolve reads the lock against the
# manifests, and `--locked` is what every CI job builds with.
cargo metadata --manifest-path "$root/Cargo.toml" --format-version 1 --locked >/dev/null \
  || die "Cargo.lock does not match Cargo.toml after the bump"
