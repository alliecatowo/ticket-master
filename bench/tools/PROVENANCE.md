# Bench task provenance and licensing

Per-task provenance and license for every fixture under `bench/fixtures/` that has a matching
`bench/tasks/<id>.toml` (`bench-swe-lite-fixture-ingestion`, SPEC.md §10). `bench/tools/
check-fixtures.sh` proves each one mechanically: `test_command` fails against the fixture as
committed, and passes after `bench/solutions/<id>.patch` is applied.

None of these vendor a real upstream repository or commit — the full SWE-bench corpus (real
GitHub issues/PRs against real, sometimes GB-scale checkouts) is explicitly out of scope for this
task per its own spec ("a few MB at most", "Do not attempt the full SWE-bench corpus"). Each task
below is instead a small, original, hermetic fixture authored for this bench suite in the shape
SWE-bench-style tasks take (a single-file library function, a pre-existing test suite that already
covers the bug, one minimal reference patch), so the harness and scoring path can be exercised
without a network fetch, a multi-MB checkout, or an external license to track.

| id | language | source | license |
| --- | --- | --- | --- |
| `py-binary-search-bound` | Python 3 (stdlib `unittest`) | Authored for this repo, 2026-09-25 | This repository's own license (`MIT OR Apache-2.0`, per the workspace `Cargo.toml`) |
| `py-list-chunk-off-by-one` | Python 3 (stdlib `unittest`) | Authored for this repo, 2026-09-25 | `MIT OR Apache-2.0` |
| `py-merge-counts-overwrites` | Python 3 (stdlib `unittest`) | Authored for this repo, 2026-09-25 | `MIT OR Apache-2.0` |
| `py-palindrome-ignores-case` | Python 3 (stdlib `unittest`) | Authored for this repo, 2026-09-25 | `MIT OR Apache-2.0` |

Each fixture's bug is a single, realistic off-by-one/logic-inversion defect (a `<` that should be
`<=`, a slice bound that drops an element, a dict-merge that overwrites instead of summing, a
case-sensitive comparison against a docstring that promises case-insensitivity) — the kind SWE-lite
issues describe in miniature, small enough that `bench/tools/check-fixtures.sh` runs each one in
well under a second with no extra toolchain beyond the system `python3` (not a `mise.toml`-managed
tool; any `python3` on `$PATH` with a stdlib `unittest` module works).

Adding a task: drop a new `bench/fixtures/<id>/` (a minimal Python module plus its
already-passing-once-fixed test file), a `bench/tasks/<id>.toml` (`test_command`/`setup_commands`
under `[fixture]`, see any existing task in `bench/tasks/` for the full shape), a
`bench/solutions/<id>.patch` (a plain `--- solution.py` / `+++ solution.py` unified diff, applied
with `patch -p0` — keep it outside `bench/fixtures/` so it's never visible to whatever solves the
task), and a row in the table above naming its real source and license (this repository's own
license for anything authored fresh for this repo; the actual upstream license and commit/URL for
anything genuinely vendored). Then run `bash bench/tools/check-fixtures.sh` and confirm it prints
`PASS <id>`.
