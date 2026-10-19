#!/usr/bin/env bash
# Run CI's clippy command on Linux, in a container, so a Linux-only lint shows up before a push
# instead of in the `ci` job. Clippy on a Mac compiles the macOS side of every
# `#[cfg(target_os = "macos")]` and never sees the Linux side: an import only macOS code uses
# reads as unused on Linux and `-D warnings` refuses it there and nowhere else.
#
#   scripts/linux-clippy.sh [checkout]      # default: the checkout this script is in
#
# The checkout is mounted read-only. The image is the one for the toolchain `rust-toolchain.toml`
# pins; the image ships it without clippy, so rustup adds the file's components on the first run and
# keeps them in a named volume. Build output and the crate cache live in two more named volumes, so
# a second run downloads nothing and only rebuilds what changed. Prints every hit and exits non-zero
# when there is one.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
checkout=$(cd "${1:-$here/..}" && git rev-parse --show-toplevel)
toolchain=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$checkout/rust-toolchain.toml")
[ -n "$toolchain" ] || { echo "linux-clippy: no channel in $checkout/rust-toolchain.toml" >&2; exit 2; }
command -v docker >/dev/null || { echo "linux-clippy: docker is not on PATH" >&2; exit 2; }

tmp=${TMPDIR:-/tmp}
log=$(mktemp "${tmp%/}/linux-clippy.XXXXXX")
status=0
docker run --rm \
  -v "$checkout":/src:ro \
  -v tcr-linux-target:/target \
  -v tcr-linux-cargo:/usr/local/cargo/registry \
  -v "tcr-linux-rustup-$toolchain":/usr/local/rustup \
  -e CARGO_TARGET_DIR=/target \
  -w /src "rust:$toolchain" \
  cargo clippy --workspace --all-targets --locked --keep-going --message-format=short -- -D warnings \
  > "$log" 2>&1 || status=$?

hits=$(grep -E '^(src|tests|crates|examples|build)[^:]*:[0-9]+:[0-9]+: (error|warning)' "$log" | sort -u || true)
if [ -n "$hits" ]; then
  printf '%s\n' "$hits"
  echo "linux-clippy: $(printf '%s\n' "$hits" | wc -l | tr -d ' ') hit(s) on Linux (rust:$toolchain); full log: $log" >&2
  exit 1
fi
if [ "$status" -ne 0 ]; then
  tail -20 "$log" >&2
  echo "linux-clippy: clippy exited $status with no lint lines; full log: $log" >&2
  exit "$status"
fi
echo "linux-clippy: clean on Linux (rust:$toolchain)"
