# shellcheck shell=bash
# Sourced by the mutation scripts in this directory; not run on its own.
#
# test_module_path <module> <test-name>
#
# Prints the libtest path of <test-name> INSIDE the test module `<module>.rs`, the one file of
# that name anywhere under `tests/` outside `it/`, `support/` and `tools/` (the area folders
# hold them, e.g. `tests/peer/pairing/peer_wire.rs`): `<name>` when the test sits in that file,
# `<child>::<name>` when it sits in a child module under its sibling folder `<module>/`. The
# child is read from the source, so the next time a test file is split into a directory the
# filter follows it and a mutation cannot start reporting a false "stayed green" because its
# filter matches nothing.
#
# The module file must exist exactly once, and the name must be defined exactly once among
# those files. Zero or two of either is a loud failure (exit 1, a message on stderr), never a
# guess: a filter that matches no test makes every mutation look like a gate that did not catch it.
test_module_path() {
  local module="$1" name="$2" root hits file count module_file module_dir
  root="${ROOT:?ROOT must be set before test_module_path}"
  local -a roots=() files=()
  while IFS= read -r file; do
    roots+=("$file")
  done < <(find "$root/tests" \( -path "$root/tests/it" -o -path "$root/tests/support" \
    -o -path "$root/tests/tools" \) -prune -o -type f -name "$module.rs" -print | sort)
  if [[ ${#roots[@]} -ne 1 ]]; then
    printf 'test_module_path: %d files named %s.rs under %s/tests (need exactly 1): %s\n' \
      "${#roots[@]}" "$module" "$root" "${roots[*]}" >&2
    return 1
  fi
  module_file="${roots[0]}"
  module_dir="${module_file%.rs}"
  files+=("$module_file")
  if [[ -d "$module_dir" ]]; then
    while IFS= read -r file; do
      files+=("$file")
    done < <(find "$module_dir" -name '*.rs' | sort)
  fi
  hits=()
  for file in "${files[@]}"; do
    count="$(grep -cE "fn ${name}[[:space:]]*\(" "$file")"
    [[ "$count" -gt 0 ]] || continue
    for _ in $(seq "$count"); do hits+=("$file"); done
  done
  if [[ ${#hits[@]} -ne 1 ]]; then
    printf 'test_module_path: fn %s is defined %d times under %s (need exactly 1)\n' \
      "$name" "${#hits[@]}" "${module_file#"$root/"}" >&2
    return 1
  fi
  file="${hits[0]}"
  if [[ "$file" == "$module_file" ]]; then
    printf '%s\n' "$name"
  else
    file="${file#"$module_dir/"}"
    file="${file%.rs}"
    file="${file%/mod}"
    printf '%s::%s\n' "${file//\//::}" "$name"
  fi
}
