#!/usr/bin/env bash
# check-fixtures.sh -- prove every vendored bug-fix-with-failing-test bench task
# (bench-swe-lite-fixture-ingestion, SPEC.md §10) is genuinely a bug fix, mechanically.
#
# For each bench/tasks/<id>.toml that has a `test_command` (the hello-world task doesn't, and is
# skipped): copy its fixture (bench/fixtures/<id>/) to a fresh mktemp dir, run test_command there
# and assert it FAILS, apply bench/solutions/<id>.patch and assert test_command now PASSES. A
# task whose test fails to fail-first, or fails to pass-after, is not proving anything and fails
# this script.
#
# The solution patch deliberately lives outside bench/fixtures/ (in bench/solutions/) so it is
# never copied into a fixture's working copy and never visible to whatever solves the task.
#
# Usage: bash bench/tools/check-fixtures.sh [task-id ...]
#   With no arguments, checks every task under bench/tasks/ that has a test_command.
#   With one or more task ids, checks only those.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tasks_dir="$repo_root/bench/tasks"
fixtures_dir="$repo_root/bench/fixtures"
solutions_dir="$repo_root/bench/solutions"

# Extract a top-level `key = "value"` scalar from a toml file (first match only -- good enough
# for this script's own flat fixtures, not a general toml parser).
toml_scalar() {
  local file="$1" key="$2"
  sed -n "s/^${key}[[:space:]]*=[[:space:]]*\"\\(.*\\)\"[[:space:]]*\$/\\1/p" "$file" | head -n1
}

# Extract a top-level `key = ["a", "b", ...]` string array as newline-separated values.
toml_array() {
  local file="$1" key="$2"
  local line
  line="$(sed -n "s/^${key}[[:space:]]*=[[:space:]]*\\[\\(.*\\)\\][[:space:]]*\$/\\1/p" "$file" | head -n1)"
  [ -n "$line" ] || return 0
  printf '%s\n' "$line" | tr ',' '\n' | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//' -e '/^[[:space:]]*$/d'
}

if [ "$#" -gt 0 ]; then
  ids=("$@")
else
  ids=()
  for f in "$tasks_dir"/*.toml; do
    [ -e "$f" ] || continue
    grep -q '^test_command' "$f" || continue
    ids+=("$(basename "$f" .toml)")
  done
fi

if [ "${#ids[@]}" -eq 0 ]; then
  echo "check-fixtures: no test_command-bearing tasks found under $tasks_dir" >&2
  exit 1
fi

failures=0
for id in "${ids[@]}"; do
  task_file="$tasks_dir/$id.toml"
  patch_file="$solutions_dir/$id.patch"
  if [ ! -f "$task_file" ]; then
    echo "FAIL $id: no task file $task_file" >&2
    failures=$((failures + 1))
    continue
  fi
  fixture_rel="$(toml_scalar "$task_file" 'path')"
  if [ -z "$fixture_rel" ]; then
    echo "FAIL $id: no [fixture] path in $task_file" >&2
    failures=$((failures + 1))
    continue
  fi
  fixture_src="$repo_root/bench/$fixture_rel"
  if [ ! -d "$fixture_src" ]; then
    echo "FAIL $id: fixture dir $fixture_src does not exist" >&2
    failures=$((failures + 1))
    continue
  fi
  if [ ! -f "$patch_file" ]; then
    echo "FAIL $id: no solution patch at $patch_file" >&2
    failures=$((failures + 1))
    continue
  fi

  cmd=()
  while IFS= read -r part; do
    cmd+=("$part")
  done < <(toml_array "$task_file" 'test_command')
  if [ "${#cmd[@]}" -eq 0 ]; then
    echo "FAIL $id: no test_command in $task_file" >&2
    failures=$((failures + 1))
    continue
  fi

  work="$(mktemp -d "${TMPDIR:-/tmp}/tm-bench-check.XXXXXX")"
  cp -R "$fixture_src/." "$work/"

  setup_line="$(sed -n '/^setup_commands[[:space:]]*=/p' "$task_file" | head -n1)"
  # setup_commands is an array of arrays; this task set's own tasks all use `[]`, so only a
  # single flat-array shape needs handling here. Refuse to silently ignore anything more complex
  # rather than guess at running it wrong.
  if [ -n "$setup_line" ] && ! printf '%s' "$setup_line" | grep -qE '=[[:space:]]*\[\][[:space:]]*$'; then
    echo "FAIL $id: setup_commands beyond '[]' not supported by this script yet" >&2
    failures=$((failures + 1))
    rm -rf "$work"
    continue
  fi

  set +e
  (cd "$work" && "${cmd[@]}" >/dev/null 2>&1)
  pre_status=$?
  set -e
  if [ "$pre_status" -eq 0 ]; then
    echo "FAIL $id: test_command passed BEFORE the patch (fixture isn't actually broken)" >&2
    failures=$((failures + 1))
    rm -rf "$work"
    continue
  fi

  if ! patch -p0 -d "$work" <"$patch_file" >/dev/null; then
    echo "FAIL $id: solution patch $patch_file did not apply cleanly" >&2
    failures=$((failures + 1))
    rm -rf "$work"
    continue
  fi

  set +e
  (cd "$work" && "${cmd[@]}" >/dev/null 2>&1)
  post_status=$?
  set -e
  if [ "$post_status" -ne 0 ]; then
    echo "FAIL $id: test_command still failed AFTER the patch" >&2
    failures=$((failures + 1))
    rm -rf "$work"
    continue
  fi

  echo "PASS $id"
  rm -rf "$work"
done

if [ "$failures" -gt 0 ]; then
  echo "check-fixtures: $failures task(s) failed" >&2
  exit 1
fi

echo "check-fixtures: all tasks fail-before/pass-after their solution patch"
