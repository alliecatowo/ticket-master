# Trial scoreboard

## What was counted

This covers the five JSON results that contain scored `tm` and OpenCode runs. `gohugoio-hugo-15360.json` was skipped because it says no merged fix PR with tests was available, so it has no comparable runs or scores. Pass rates below use the JSON `passed` fields as recorded. One qualification matters: ripgrep's `tm` result is marked passed, but its notes say the prescribed checkout replaced the agent's work before the test ran, so that pass is not evidence that tm solved the issue.

Scores are on the supplied rubric scale (higher is better). Times are seconds; tokens are the recorded totals. Medians describe the five runs for each tool.

## Per-tool results

| Tool | Recorded pass rate | Mean navigation | Mean context efficiency | Mean plan quality | Mean verification honesty | Mean UX clarity | Mean recoverability | Mean cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 1/5 (20%) | 2.4 | 1.4 | 1.8 | 2.4 | 2.0 | 1.6 | 2.2 | 498 s (8m 18s) | 732,981 |
| OpenCode | 3/5 (60%) | 3.6 | 3.4 | 3.8 | 3.0 | 4.2 | 3.2 | 5.0 | 66.91 s (1m 7s) | 21,775 |

## tm vs OpenCode, trial by trial

| Trial | tm result (time; tokens) | OpenCode result (time; tokens) | What happened |
|---|---|---|---|
| [pallets-click-3822](pallets-click-3822.json) | Fail; 324 s; 732,981 | Fail; 67.136 s; 24,288 | tm ended without a patch after repeated reads and gave vague recovery guidance. OpenCode made a scoped attempt, but missed the full typing test that would have exposed the remaining type mismatch. |
| [sindresorhus-ky-878](sindresorhus-ky-878.json) | Fail; 73.252 s; 30,383 | Fail; 34.891 s; 20,032 | tm found and changed the right code; 19 AVA tests and the Chromium regression passed, but the full test run was blocked by environment/tooling issues. OpenCode edited the sibling tm clone instead of its own clone, so the intended fix and hidden regression test failed. |
| [psf-requests-7432](psf-requests-7432.json) | Fail; 619 s; 83,380 | Pass; 62 s; 19,889 | tm eventually found the right area but ended without a patch after repeated exploration and environment probes. OpenCode implemented the fix quickly, though its own pytest attempts used the wrong Python environment. |
| [BurntSushi-ripgrep-3376](BurntSushi-ripgrep-3376.json) | Pass*; 498 s; 2,483,595 | Pass; 153 s; 33,253 | tm spent much longer exploring and submitted no patch or evidence; the recorded test pass came from checking out the merged file, which replaced its implementation. OpenCode implemented a focused fix and test. |
| [spf13-cobra-2257](spf13-cobra-2257.json) | Fail; 840 s; 3,661,274 | Pass; 66.91 s; 21,775 | tm ended without a patch after repeated reads. OpenCode fixed the argument aliasing, added a regression test, and ran the Go test suite successfully. |

## Five biggest things tm should improve

1. **Stop expensive runs that are making no progress, and avoid rereading the same material.** In click, requests, ripgrep, and cobra, notes describe repeated or overlapping exploration without a submitted patch. tm used a median 498 seconds and 732,981 tokens, versus OpenCode's 66.91 seconds and 21,775 tokens. The gap was especially large in cobra (3,661,274 tokens vs 21,775) and ripgrep (2,483,595 vs 33,253). See [click](pallets-click-3822.json), [requests](psf-requests-7432.json), [ripgrep](BurntSushi-ripgrep-3376.json), and [cobra](spf13-cobra-2257.json).

2. **Give a short plan and navigate directly to the relevant code and tests.** tm's mean plan score was 1.8 and navigation score 2.4, compared with OpenCode's 3.8 and 3.6. Repeated reads of `command.go`/`completions.go` (cobra), `walk.rs`/`dir.rs` (ripgrep), and source/history (requests) point to a need for sharper scoping. See [cobra](spf13-cobra-2257.json), [ripgrep](BurntSushi-ripgrep-3376.json), and [requests](psf-requests-7432.json).

3. **Make failure recovery specific and honest about what happened.** Several runs ended without a patch but offered generic “something went wrong” messaging and a retry, after substantial work; users need a concise account of what was inspected, what changed (if anything), why the run stopped, and a useful next action. tm's mean recoverability score was 1.6 versus 3.2 for OpenCode. See [click](pallets-click-3822.json), [requests](psf-requests-7432.json), [ripgrep](BurntSushi-ripgrep-3376.json), and [cobra](spf13-cobra-2257.json).

4. **Improve command and environment diagnostics, including exit-code handling.** The ky run had confusing ticket/init and retry messages, plus a shell pipeline that masked a lint exit code. The requests run probed system Python instead of using the clone's prepared virtualenv. Better command status reporting and project-environment detection would avoid wasted attempts and make verification summaries trustworthy. See [ky](sindresorhus-ky-878.json) and [requests](psf-requests-7432.json).

5. **Keep the run anchored to the intended checkout and verify the actual requested behavior.** The ky OpenCode run demonstrates how editing a sibling clone can invalidate an otherwise plausible fix; in tm's ky run, the right narrow code change was made, but broader verification could not complete. In click, a focused check missed the complete typing test that would have caught the remaining bug. Confirm the working directory before edits, then run the most relevant full or hidden-equivalent regression check and report exactly what it covered. See [ky](sindresorhus-ky-878.json) and [click](pallets-click-3822.json).

*The raw tm pass rate is 1/5; treating the non-independent ripgrep test as not demonstrating a tm fix, the evidence-backed rate is 0/5.*
