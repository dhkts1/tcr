# shellcheck shell=bash
# Sourced by the mutation scripts in this directory; not run on its own.
#
# test_module_path <module> <test-name>
#
# Prints the libtest path of <test-name> INSIDE the test module `tests/<module>.rs`:
# `<name>` when the test sits in that file, `<child>::<name>` when it sits in a child
# module under `tests/<module>/`. The child is read from the source, so the next time a
# test file is split into a directory the filter follows it and a mutation cannot start
# reporting a false "stayed green" because its filter matches nothing.
#
# The name must be defined exactly once among those files. Zero or two definitions is a
# loud failure (exit 1, a message on stderr), never a guess: a filter that matches no test
# makes every mutation look like a gate that did not catch it.
test_module_path() {
  local module="$1" name="$2" root hits file count
  root="${ROOT:?ROOT must be set before test_module_path}"
  local -a files=()
  [[ -f "$root/tests/$module.rs" ]] && files+=("$root/tests/$module.rs")
  if [[ -d "$root/tests/$module" ]]; then
    while IFS= read -r file; do
      files+=("$file")
    done < <(find "$root/tests/$module" -name '*.rs' | sort)
  fi
  if [[ ${#files[@]} -eq 0 ]]; then
    printf 'test_module_path: no tests/%s.rs or tests/%s/ under %s\n' "$module" "$module" "$root" >&2
    return 1
  fi
  hits=()
  for file in "${files[@]}"; do
    count="$(grep -cE "fn ${name}[[:space:]]*\(" "$file")"
    [[ "$count" -gt 0 ]] || continue
    for _ in $(seq "$count"); do hits+=("$file"); done
  done
  if [[ ${#hits[@]} -ne 1 ]]; then
    printf 'test_module_path: fn %s is defined %d times under tests/%s (need exactly 1)\n' \
      "$name" "${#hits[@]}" "$module" >&2
    return 1
  fi
  file="${hits[0]}"
  if [[ "$file" == "$root/tests/$module.rs" ]]; then
    printf '%s\n' "$name"
  else
    file="${file#"$root/tests/$module/"}"
    file="${file%.rs}"
    file="${file%/mod}"
    printf '%s::%s\n' "${file//\//::}" "$name"
  fi
}
