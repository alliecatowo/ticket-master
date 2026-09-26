# Trial scoreboard

Plain-English summary of the six top-level `*.json` trial results in this directory. The Hugo #15360 file is marked **skipped** because there was no merged fix, so it has no tool runs or scores. The ky #878 trial is **incomplete**: tm was not run because dependency setup failed; OpenCode did run. Pass rate and medians below use only runs with recorded results (tm: 4 runs; OpenCode: 5). Scores are rubric means on the source files' 1–5 scale; higher is better. Wall time and token medians are across those same completed runs.

## Per-tool scoreboard

| Tool | Pass rate | Navigation quality | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 2/4 (50%) | 2.75 | 1.00 | 1.25 | 2.75 | 2.50 | 1.50 | 1.25 | 474.6 s | 1,466,785 |
| OpenCode | 5/5 (100%) | 4.00 | 3.60 | 4.20 | 4.20 | 4.00 | 4.00 | 4.80 | 66.0 s | 24,410 |

The pass numbers are the recorded `passed` fields, not a claim that every passing result independently proves the agent implemented the fix. In ripgrep #3376, the test protocol restored the PR's changed file—including the fix—so tm's mechanical pass did not show that tm implemented it. In cobra #2257, tm's retained implementation passed hidden tests, but the run did not submit a ticket. Read those pass results with the trial notes in mind.

## tm vs OpenCode, trial by trial

| Trial | tm result (pass; seconds; tokens) | OpenCode result (pass; seconds; tokens) | What stood out |
|---|---|---|---|
| [Click #3822](pallets-click-3822.json) | No; 250.8; 575,981 | Yes; 87.2; 33,556 | tm found the relevant Path typing work but submitted no patch; its runtime test failed and the typing fixture had mypy and pyright errors. OpenCode implemented and verified the runtime and typing changes. |
| [Requests #7432](psf-requests-7432.json) | No; 621.1; 1,445,727 | Yes; 58.7; 24,410 | tm found the relevant code but stopped without a patch after repeated exploration. OpenCode implemented a focused fix and test, corrected a fixture mistake, and reported its successful test accurately. |
| [Cobra #2257](spf13-cobra-2257.json) | Yes; 1,026.4; 3,601,360 | Yes; 59.9; 21,820 | tm's retained code passed hidden tests, but it hit the 64-step limit without submitting. OpenCode quickly made and tested the slice-copy fix. |
| [ripgrep #3376](BurntSushi-ripgrep-3376.json) | Yes*; 328.0; 1,487,843 | Yes*; 195.3; 34,372 | tm did not complete a fix and encountered a Cargo/toolchain check problem; its reported pass was not evidence of implementation because the protocol restored the PR's changed production file. OpenCode implemented the cache-key fix and recovered from test/fixture issues; its pass is also qualified because the restored PR file contained the fix. |
| [ky #878](sindresorhus-ky-878.json) | Not run; setup failed | Yes; 66.0; 22,935 | pnpm setup failed in both clones, so tm has no score or run result. OpenCode's focused AVA regression test passed after the broad test path hit a lint error. |
| Hugo #15360 | Skipped | Skipped | No merged fix was available to benchmark. |

`*` The recorded `passed` value is true, but the trial notes qualify what that pass demonstrates; see above.

## Five biggest improvements for tm

1. **Stop repeated reads and searches from consuming the run.** Use targeted file/line reads, avoid reopening unchanged files, and detect cycles/no progress early. Requests #7432 spent 621 seconds and 1.45M tokens on 41 calls without a patch; Cobra #2257 repeated reads and searches for 1,026 seconds and 3.60M tokens; Click #3822 also reread `src/click/types.py` repeatedly. These are the clearest drivers of tm's 1.00 context-efficiency mean.
2. **Make a short plan and bias toward a minimal patch plus submission.** In Click #3822, Requests #7432, and Cobra #2257, tm found relevant implementation areas but ended without a submitted change. A brief issue-specific plan, early focused edit, and explicit submit checkpoint would turn useful discovery into delivered work. The plan-quality mean was 1.25.
3. **Fail fast on blockers and explain what actually happened.** In ripgrep #3376, investigate why the Cargo check said Cargo was unavailable even though it was usable in the shell; name the failing command and actionable next step rather than spending more time. In ky #878, setup blocked the tm run entirely, so record it as unavailable rather than implying an agent outcome. Click #3822 also needs a clear account of its failed runtime test and typing errors.
4. **Make the final status precise: retained work, submitted work, and verified work are different.** Cobra #2257 retained an implementation that passed hidden tests but did not submit; Click #3822 submitted nothing and tests failed; ripgrep #3376's mechanical pass came from restoring the PR's own changed file. Summaries should spell out these distinctions and avoid letting a test-protocol pass read as proof of agent implementation. This would strengthen the 2.75 verification-honesty mean.
5. **Give users a concrete recovery path and useful progress updates.** After stopping, summarize findings and remaining work, state where saved work/session state lives, and provide a copyable resume command with the actual session identifier. Requests #7432 left the user to discover the session ID and reconstruct the task; Cobra #2257 offered no bounded continuation or concise account of retained work. In Cobra, generic repeated “Read completions.go” updates also stopped communicating progress. These issues match tm's 1.50 recoverability mean.
