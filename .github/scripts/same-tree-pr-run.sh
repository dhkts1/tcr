#!/usr/bin/env bash
# On a push to main, find a successful pull_request run of this same workflow
# that already tested this exact tree, so ci.yml can skip its heavy jobs.
#
# WHY. Tree 2e94b3519c passed in PR run 42694804670, then push run 42698347980
# built and tested it again: ci 598 s, macos 1,092 s, tsan 273 s, miri 340 s.
# Over the 40 push runs on main up to 2026-11-09, 39 had a passing pull_request
# run of ci.yml on the same tree, and they re-spent about 1,250 runner-minutes.
#
# THE PROOF is the API's own records, never a label or a commit message:
#   1. the PR merged as this commit (GET commits/<sha>/pulls, merged_at set);
#   2. that PR's head commit has this commit's tree (GET commits/<sha>);
#   3. the old tip of main is an ancestor of that head (GET compare), so the
#      pull_request run's merge ref had the head's tree too, not a merge of it
#      with some other base;
#   4. a run of THIS workflow file, in THIS repository, event pull_request,
#      concluded success on that head (GET actions/runs?head_sha=).
# The tree pins ci.yml itself, rust-toolchain.toml and Cargo.lock, so the PR
# run ran the same jobs on the same toolchain channel and the same lock.
#
# One more refusal is about caches, not correctness: a pull_request run saves
# its cargo cache under the PR's ref, where main cannot read it. When a push
# moves the Swatinem/rust-cache key, the push must build so main gets a cache
# under the new key. The rule below mirrors how rust-cache v2 derives that key
# (src/config.ts): it hashes rust-toolchain{,.toml} and .cargo/config.toml
# whole, Cargo.toml with every `version` set to 0.0.0, and only the Cargo.lock
# packages that carry a `source` or `checksum`. So the release bump that edits
# a workspace crate's version in Cargo.toml and Cargo.lock moves no key, and
# is not refused; a dependency change, which always moves a checksum line, is.
#
# Env: REPO (owner/name), SHA (the pushed commit), BEFORE (the old tip of
# main), WORKFLOW_PATH (.github/workflows/ci.yml).
# Stdin: `git diff --unified=0 BEFORE SHA`.
# Stdout: `reuse=<run id>` when found, `reuse=` when not; the reasons go to
# stderr. Exit non-zero on any lookup error: the caller then runs everything.
set -euo pipefail

: "${REPO:?}" "${SHA:?}" "${BEFORE:?}" "${WORKFLOW_PATH:?}"

none() { echo "same-tree: $*; running everything." >&2; echo "reuse="; exit 0; }

any=0 path=""
while IFS= read -r line; do
  case "$line" in
    "diff --git "*) any=1; path="${line##* b/}"; continue ;;
    "--- a/"*|"--- /dev/null"|"+++ b/"*|"+++ /dev/null") continue ;;
    [-+]*) ;;
    *) continue ;;
  esac
  # A changed line of $path.
  case "$path" in
    rust-toolchain|*/rust-toolchain|rust-toolchain.toml|*/rust-toolchain.toml|.cargo/*|*/.cargo/*)
      none "$path changed, and main's cargo cache needs a build under the new key" ;;
    Cargo.lock|*/Cargo.lock)
      case "$line" in
        [-+]"checksum = "*|[-+]"source = "*)
          none "a dependency in $path changed, and main's cargo cache needs a build under the new key" ;;
      esac ;;
    Cargo.toml|*/Cargo.toml)
      case "$line" in
        [-+]"version = \""*) ;;
        *) none "$path changed beyond a version line, and main's cargo cache needs a build under the new key" ;;
      esac ;;
  esac
done
[ "$any" = 1 ] || none "no changed paths between $BEFORE and $SHA"

tree="$(gh api "repos/$REPO/commits/$SHA" --jq .commit.tree.sha)"
[ -n "$tree" ] || { echo "same-tree: no tree for $SHA" >&2; exit 2; }
echo "same-tree: $SHA has tree $tree" >&2

heads="$(gh api "repos/$REPO/commits/$SHA/pulls" --jq '.[] | select(.merged_at != null) | .head.sha')"
[ -n "$heads" ] || none "no merged pull request names $SHA"

for head in $heads; do
  head_tree="$(gh api "repos/$REPO/commits/$head" --jq .commit.tree.sha)"
  if [ "$head_tree" != "$tree" ]; then
    echo "same-tree: PR head $head has tree $head_tree, not $tree" >&2; continue
  fi
  status="$(gh api "repos/$REPO/compare/$BEFORE...$head" --jq .status)"
  case "$status" in
    ahead|identical) ;;
    *) echo "same-tree: $BEFORE is not an ancestor of $head ($status)" >&2; continue ;;
  esac
  run="$(gh api "repos/$REPO/actions/runs?head_sha=$head&event=pull_request&status=completed&per_page=100" \
    --jq "[.workflow_runs[] | select(.path == \"$WORKFLOW_PATH\" and .event == \"pull_request\" and .conclusion == \"success\" and .head_sha == \"$head\" and .repository.full_name == \"$REPO\")] | first | .id // empty")"
  if [ -n "$run" ]; then
    echo "same-tree: run $run of $WORKFLOW_PATH concluded success on $head, tree $tree" >&2
    echo "reuse=$run"
    exit 0
  fi
  echo "same-tree: no successful pull_request run of $WORKFLOW_PATH on $head" >&2
done
none "no pull request head with tree $tree has a passing run"
