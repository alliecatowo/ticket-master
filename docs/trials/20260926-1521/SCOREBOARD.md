# Trial scoreboard

This scoreboard covers the five trials with results for both tools. The Hugo JSON file was skipped because it says there is no merged fixing PR, so it has no tool runs to score. Rubric scores are on a 1–5 scale; higher is better. “Pass” is the result recorded in each trial file, with important verification caveats noted below.

## Results by tool

| Tool | Pass rate | Mean navigation | Mean context efficiency | Mean plan quality | Mean verification honesty | Mean UX clarity | Mean recoverability | Mean cost/time score | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 3/5 (60%) | 2.6 | 1.0 | 2.2 | 2.4 | 2.2 | 2.4 | 2.0 | 347.15 sec (5m 47s) | 1,521,775 |
| OpenCode | 3/5 (60%) | 4.4 | 4.0 | 4.2 | 4.0 | 4.2 | 4.0 | 4.8 | 71.64 sec (1m 12s) | 31,600 |

The pass counts are equal, but they hide differences in the work and its verification. For example, the ripgrep run was mechanically marked passed after the merged source was checked out over the agent's work; that does not establish that either agent's own fix passed. In the ky trial, the hidden browser test failed for both tools, despite other focused tests passing. See the trial notes.

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What stands out |
|---|---|---|---|
| Click #3822 | Did not pass; 747 sec; 1,985,812 tokens | Did not pass; 65 sec; 186,607 tokens | tm repeatedly reread overlapping source and did not complete a patch. OpenCode was faster and found the implementation, but missed typing-fixture mismatches found by the PR tests. |
| Requests #7432 | Passed; 580.70 sec; 2,030,788 tokens | Passed; 95.18 sec; 31,600 tokens | Both made the correct compatibility change. tm recovered from an evidence/submission issue, but repeated reads and lacked a concise final test summary. OpenCode recovered from a bad fixture and test invocation; its focused and PR tests passed. |
| ky #878 | Did not pass; 153.03 sec; 1,107,519 tokens | Did not pass; 71.64 sec; 21,327 tokens | Both found the flush logic. tm repeated searches and test-file reads, and the run's captured submission state was confusing. OpenCode corrected its fixture and reported focused tests accurately, but the independent hidden browser test still failed. |
| ripgrep #3376 | Marked passed; 342.76 sec; 1,521,775 tokens | Marked passed; 153.63 sec; 47,174 tokens | tm did not finish its fix or verification before reporting Cargo unavailable, although Cargo was available to the benchmark shell. OpenCode implemented and tested a fix. For both, the mandatory checkout replaced the relevant file with merged code, so the recorded post-checkout pass is not proof of the agent's patch. |
| Cobra #2257 | Passed; 347.15 sec; 1,121,197 tokens | Passed; 64.88 sec; 21,853 tokens | Both found the backing-array aliasing bug and the recorded tests passed. tm hit a misleading Go-unavailable error and retried after broad rereads; its token telemetry also needs clarification. OpenCode found the issue and verified it much faster. |

## Five biggest improvements for tm

1. **Stop repeated exploration sooner and retain what has already been learned.** Duplicate and overlapping reads consumed much of the run without moving the fix forward. Add a bounded exploration loop and carry forward concise findings. This was especially visible in Click (747 seconds, no patch), Requests (53 tool calls and repeated `models.py` reads), and ky (repeated searches and reads of `test/stream.ts`).

2. **Make runs dramatically more efficient.** Across these five trials, tm's median was about 5m 47s and 1.52 million tokens, versus OpenCode's 1m 12s and 31,600 tokens. Reduce repeated context loading and unnecessary tool calls, and investigate why telemetry shows such high token counts. Examples: Requests used 2,030,788 tokens versus 31,600; Cobra used 1,121,197 versus 21,853. (Trial files: `pallets-click-3822.json`, `psf-requests-7432.json`, `spf13-cobra-2257.json`.)

3. **Preflight tools in the same environment that runs commands, and show exact failures.** Click, ripgrep, and Cobra each report a required toolchain as unavailable, although the benchmark shell had the relevant tool available in the latter cases. Check PATH and tool availability in the child execution environment before starting, and include the exact command and output when a check fails. (Trial files: `pallets-click-3822.json`, `BurntSushi-ripgrep-3376.json`, `spf13-cobra-2257.json`.)

4. **Verify against the repository's real fixtures and independent tests before calling the work done.** Click's PR typing tests found two default-inference mismatches after OpenCode's own suite passed; in ky, the hidden browser test failed for both agents despite focused stream tests passing. tm should identify and run the relevant fixtures early, then clearly distinguish its own passing checks from independent or hidden-test failures. (Trial files: `pallets-click-3822.json`, `sindresorhus-ky-878.json`.)

5. **End with a concise, unambiguous status and a useful recovery path.** Say what changed, which exact checks ran, and whether they passed; distinguish a saved result from a submitted result and explain any retry or corrected attempt. When blocked, give the actionable next step inline. Repeated dispatch messages and unclear submission states hurt Click and ky; Requests also lacked a clear final test summary, and Cobra lacked a clear user-facing summary. (Trial files: `pallets-click-3822.json`, `psf-requests-7432.json`, `sindresorhus-ky-878.json`, `spf13-cobra-2257.json`.)

## How the summary was calculated

Each mean rubric score is the arithmetic mean across that tool's five recorded arms. Pass rate uses the five `passed` fields as written. Wall time and token figures are medians across those same arms. The skipped Hugo file is excluded because it contains no arms; nested JSON files are not trial-result files and were not counted.
