#!/usr/bin/env bash
# Check the root fan helper's LaunchDaemon plist, as bundled at
# `Contents/Library/LaunchDaemons/<label>.plist`. Called by build-tcrbar.sh on
# the copy it just wrote, and by release-tcrbar.sh (`--verify-only` and stage 3)
# on the bundle it is about to ship, so the two scripts hold one set of rules.
#
#   check-fan-helper-plist.sh <plist> <label> <app-bundle-id>
#
# What it holds, each for a reason:
#   - the file lints and is named `<label>.plist`: `SMAppService.daemon` finds
#     it by that name (`FanHelperRegistration.plistName`);
#   - `Label` is the label, and `MachServices` is exactly `{ "<label>": true }`:
#     the helper listens on its label (`FanIdentity::helper_label`), so a
#     second or other service name is one launchd would start it for and
#     nobody answers;
#   - `BundleProgram` is `Contents/MacOS/tcr-fan-helper`;
#   - `AssociatedBundleIdentifiers` is exactly `[ "<app-bundle-id>" ]`;
#   - no `RunAtLoad` and no `KeepAlive`: the helper runs as root on demand
#     only and exits when idle. Either key would keep a root process alive
#     with nobody asking for it.
#
# Prints one line per failure and exits 1 if there was any, 2 on bad usage.
set -euo pipefail

[ "$#" -eq 3 ] || { echo "usage: $0 <plist> <label> <app-bundle-id>" >&2; exit 2; }
plist="$1" label="$2" app_id="$3"

failures=0
fail() {
  printf 'fan helper plist %s: %s\n' "$plist" "$1" >&2
  failures=$((failures + 1))
}

# A top-level key's value: a string as itself, a dictionary or an array as
# compact JSON, nothing when the key is absent. `plutil -extract` splits its key
# path on dots, so only top-level keys go through here; the label, which is
# full of dots, is compared inside the JSON instead.
string() {
  plutil -extract "$1" raw -o - "$plist" 2>/dev/null || true
}
json() {
  plutil -extract "$1" json -o - "$plist" 2>/dev/null || true
}
present() {
  plutil -extract "$1" xml1 -o /dev/null "$plist" 2>/dev/null
}

[ -f "$plist" ] || { fail "no such file"; exit 1; }
plutil -lint "$plist" >/dev/null || { fail "does not lint"; exit 1; }
[ "$(basename "$plist")" = "$label.plist" ] || fail "named $(basename "$plist"), not $label.plist"

[ "$(string Label)" = "$label" ] || fail "Label is '$(string Label)', not '$label'"
[ "$(json MachServices)" = "{\"$label\":true}" ] \
  || fail "MachServices is '$(json MachServices)', not '{\"$label\":true}'"
[ "$(string BundleProgram)" = "Contents/MacOS/tcr-fan-helper" ] \
  || fail "BundleProgram is '$(string BundleProgram)', not 'Contents/MacOS/tcr-fan-helper'"
[ "$(json AssociatedBundleIdentifiers)" = "[\"$app_id\"]" ] \
  || fail "AssociatedBundleIdentifiers is '$(json AssociatedBundleIdentifiers)', not '[\"$app_id\"]'"
for key in RunAtLoad KeepAlive; do
  if present "$key"; then
    fail "has $key; the helper starts on demand only"
  fi
done

[ "$failures" -eq 0 ] || exit 1
echo "    fan helper plist: $label, on demand only (no RunAtLoad, no KeepAlive)"
