#!/usr/bin/env bash
# Fixture test for same-tree-pr-run.sh, offline: a stub `gh` answers each REST
# endpoint from a file, so every case below is one recorded API shape.
#
# The cases come in both directions. A lookup that can only say "found" would
# skip every push to main untested, and one that can only say "none" saves
# nothing, so each refusal below is paired with the one fact that flips it.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/same-tree-pr-run.sh"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/same-tree-pr-run.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT

# The stub: `gh api <endpoint> --jq <expr>` prints `jq -r <expr>` of the file
# named after the endpoint, and fails like a 404 when there is no such file.
mkdir -p "$scratch/bin"
cat >"$scratch/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[ "$1" = api ] || { echo "stub gh: only 'api' is faked" >&2; exit 64; }
endpoint="$2"; shift 2
expr=.
while [ $# -gt 0 ]; do
  case "$1" in
    --jq) expr="$2"; shift 2 ;;
    *) echo "stub gh: unexpected flag $1" >&2; exit 64 ;;
  esac
done
file="$FIXTURE/$(printf '%s' "$endpoint" | tr -c 'A-Za-z0-9' '_')"
[ -f "$file" ] || { echo "gh: Not Found (HTTP 404) $endpoint" >&2; exit 1; }
jq -r "$expr" "$file"
STUB
chmod +x "$scratch/bin/gh"

REPO=o/r
SHA=push0000
BEFORE=base0000
HEAD_SHA=head0000
TREE=tree1111
WF=.github/workflows/ci.yml

put() { # endpoint, json
  printf '%s' "$2" >"$FIXTURE/$(printf '%s' "$1" | tr -c 'A-Za-z0-9' '_')"
}

# A world where reuse is correct: the merged PR's head has the push's tree, the
# old tip of main is its ancestor, and ci.yml passed on it.
seed() {
  FIXTURE="$scratch/fx-$1"; export FIXTURE
  rm -rf "$FIXTURE"; mkdir -p "$FIXTURE"
  put "repos/$REPO/commits/$SHA" "{\"commit\":{\"tree\":{\"sha\":\"$TREE\"}}}"
  put "repos/$REPO/commits/$SHA/pulls" "[{\"number\":7,\"merged_at\":\"2026-11-09T21:00:00Z\",\"head\":{\"sha\":\"$HEAD_SHA\"}}]"
  put "repos/$REPO/commits/$HEAD_SHA" "{\"commit\":{\"tree\":{\"sha\":\"$TREE\"}}}"
  put "repos/$REPO/compare/$BEFORE...$HEAD_SHA" '{"status":"ahead"}'
  put "repos/$REPO/actions/runs?head_sha=$HEAD_SHA&event=pull_request&status=completed&per_page=100" \
    "{\"workflow_runs\":[{\"id\":42,\"path\":\"$WF\",\"event\":\"pull_request\",\"conclusion\":\"success\",\"head_sha\":\"$HEAD_SHA\",\"repository\":{\"full_name\":\"$REPO\"}}]}"
}

# One file's `git diff --unified=0` section: path, then the changed lines.
hunk() {
  local p="$1"; shift
  printf 'diff --git a/%s b/%s\n--- a/%s\n+++ b/%s\n@@ -1 +1 @@\n' "$p" "$p" "$p" "$p"
  printf '%s\n' "$@"
}
CODE="$(hunk src/main.rs '-fn a() {}' '+fn b() {}')"

fails=0
run_case() { # label, want (a run id, "none" or "error"), diff text on stdin of the script
  local label="$1" want="$2" diff="$3"
  local out rc=0 got
  out="$(printf '%s\n' "$diff" | PATH="$scratch/bin:$PATH" \
    REPO="$REPO" SHA="$SHA" BEFORE="$BEFORE" WORKFLOW_PATH="$WF" \
    "$script" 2>"$scratch/stderr")" || rc=$?
  if [ "$rc" -ne 0 ]; then got=error
  else
    got="${out#reuse=}"; [ -n "$got" ] || got=none
  fi
  if [ "$got" = "$want" ]; then
    echo "  ok   $label -> $got"
  else
    echo "  FAIL $label: want $want, got $got (exit $rc)" >&2
    sed 's/^/       /' "$scratch/stderr" >&2
    fails=$((fails+1))
  fi
}

seed match
run_case "PR head has the push's tree and passed"        42    "$CODE"

seed tree
put "repos/$REPO/commits/$HEAD_SHA" '{"commit":{"tree":{"sha":"tree2222"}}}'
run_case "PR head has a different tree"                  none  "$CODE"

seed ancestor
put "repos/$REPO/compare/$BEFORE...$HEAD_SHA" '{"status":"diverged"}'
run_case "old tip of main is not in the PR head"         none  "$CODE"

seed failed
put "repos/$REPO/actions/runs?head_sha=$HEAD_SHA&event=pull_request&status=completed&per_page=100" \
  "{\"workflow_runs\":[{\"id\":42,\"path\":\"$WF\",\"event\":\"pull_request\",\"conclusion\":\"failure\",\"head_sha\":\"$HEAD_SHA\",\"repository\":{\"full_name\":\"$REPO\"}}]}"
run_case "the PR run failed"                             none  "$CODE"

seed otherwf
put "repos/$REPO/actions/runs?head_sha=$HEAD_SHA&event=pull_request&status=completed&per_page=100" \
  "{\"workflow_runs\":[{\"id\":42,\"path\":\".github/workflows/e2e-install.yml\",\"event\":\"pull_request\",\"conclusion\":\"success\",\"head_sha\":\"$HEAD_SHA\",\"repository\":{\"full_name\":\"$REPO\"}}]}"
run_case "only another workflow passed"                  none  "$CODE"

seed otherrepo
put "repos/$REPO/actions/runs?head_sha=$HEAD_SHA&event=pull_request&status=completed&per_page=100" \
  "{\"workflow_runs\":[{\"id\":42,\"path\":\"$WF\",\"event\":\"pull_request\",\"conclusion\":\"success\",\"head_sha\":\"$HEAD_SHA\",\"repository\":{\"full_name\":\"fork/r\"}}]}"
run_case "the passing run belongs to another repository" none  "$CODE"

seed unmerged
put "repos/$REPO/commits/$SHA/pulls" "[{\"number\":7,\"merged_at\":null,\"head\":{\"sha\":\"$HEAD_SHA\"}}]"
run_case "the PR is not merged"                          none  "$CODE"

# The cache rule, mirroring rust-cache v2's key (see the script's header).
# A release bump moves only workspace `version` lines, which the key ignores.
BUMP="$CODE
$(hunk Cargo.toml '-version = "1.2.26"' '+version = "1.2.27"')
$(hunk Cargo.lock '-version = "1.2.26"' '+version = "1.2.27"')"
seed bump
run_case "a release bump of Cargo.toml and Cargo.lock"   42    "$BUMP"
seed lockdep
run_case "a Cargo.lock dependency checksum changed"      none  "$CODE
$(hunk Cargo.lock '-version = "1.0.1"' '+version = "1.0.2"' '-checksum = "aa"' '+checksum = "bb"')"
seed locksource
run_case "a Cargo.lock git source was added"             none  "$(hunk Cargo.lock '+source = "git+https://example.invalid/x#1"')"
seed membertoml
run_case "a member Cargo.toml gained a dependency"       none  "$(hunk crates/x/Cargo.toml '+serde = "1"')"
seed toolchain
run_case "rust-toolchain.toml changed"                   none  "$(hunk rust-toolchain.toml '-channel = "1.99.0"' '+channel = "1.99.1"')"
seed cargocfg
run_case ".cargo/config.toml changed"                    none  "$(hunk .cargo/config.toml '+[build]')"
seed nestedcfg
run_case "a member's .cargo/config.toml changed"         none  "$(hunk crates/x/.cargo/config.toml '+[build]')"

seed nopaths
run_case "no changed paths at all"                       none  ""

seed apierror
rm "$FIXTURE/$(printf '%s' "repos/$REPO/commits/$SHA/pulls" | tr -c 'A-Za-z0-9' '_')"
run_case "an API error"                                  error "$CODE"

if [ "$fails" -ne 0 ]; then
  echo "same-tree-pr-run: $fails fixture case(s) failed" >&2; exit 1
fi
echo "same-tree-pr-run: fixture ok (16 cases)"
