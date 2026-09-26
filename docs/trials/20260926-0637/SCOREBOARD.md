# Trial scoreboard

Four issues had benchmarkable merged fixes. Scores are on the rubric's 1–5 scale; higher is better. “Passed” is the trial's reported result. Two other JSON files were skipped because there was no eligible merged fix and test set, so they are not counted in the averages.

## Results by tool

| Tool | Pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 0/4 (0%) | 2.75 | 1.25 | 1.75 | 1.75 | 1.75 | 1.50 | 1.75 | 185.18 s | 762,790 |
| OpenCode | 3/4 (75%) | 4.50 | 3.75 | 4.50 | 4.50 | 4.25 | 4.00 | 4.25 | 102.74 s | 24,985 |

Tokens and wall time are medians across the four benchmarked arms for each tool. In the Cobra tm run, the runner failed before tm started; its recorded zero seconds and zero tokens are included because it was scored as a failed trial.

## tm vs OpenCode, issue by issue

| Trial | tm | OpenCode | Comparison |
|---|---|---|---|
| [Click #3822](https://github.com/pallets/click/issues/3822) | Fail · 434 s · 1,100,017 tokens | Pass · 256 s · 25,763 tokens | tm found relevant code but submitted no patch after two no-submit failures. OpenCode implemented the typing change and its restored PR tests passed. |
| [Requests #7432](https://github.com/psf/requests/issues/7432) | Fail · 313.9 s · 2,547,146 tokens | Pass · 67.66 s · 190,226 tokens | tm found the redirect rewind path but ended without a patch or test evidence. OpenCode fixed stream detection; the hidden redirect regression passed. |
| [Cobra #2257](https://github.com/spf13/cobra/issues/2257) | Fail · 0 s · 0 tokens | Pass · 39 s · 22,935 tokens | tm never started because the runner failed shell syntax validation. OpenCode fixed append behavior, added a regression test, and passed the full suite. |
| [ky #878](https://github.com/sindresorhus/ky/issues/878) | Fail · 56.46 s · 425,563 tokens | Fail · 137.81 s · 24,207 tokens | tm made the intended fix but had tool/schema errors and no confirmed test evidence. OpenCode found and scoped the change well, but required hidden-test verification could not start because pnpm rejected ignored build scripts. |

## Five biggest improvements for tm

1. **Recover usefully when a run fails to submit.** In Click and Requests, tm ended with no patch and offered essentially a rerun, despite repeated failures. Resume or inspect the saved attempt, identify what stopped, and give the user a specific next action. ([Click #3822](https://github.com/pallets/click/issues/3822), [Requests #7432](https://github.com/psf/requests/issues/7432))
2. **Spend far less context and avoid repeating investigation.** tm reread source/history or probed the environment repeatedly; its median was 762,790 tokens versus OpenCode’s 24,985. Set tighter investigation bounds, preserve findings, and avoid reprocessing large context. ([Click #3822](https://github.com/pallets/click/issues/3822), [Requests #7432](https://github.com/psf/requests/issues/7432), [ky #878](https://github.com/sindresorhus/ky/issues/878))
3. **Make runner setup reliable and failures visible.** The Cobra run failed before tm initialized due to shell syntax validation, leaving no agent run. Validate command construction (including multiline issue text) and report setup failures plainly. ([Cobra #2257](https://github.com/spf13/cobra/issues/2257))
4. **Guide tool arguments and recover from schema errors.** ky recorded relative-path parsing and result-schema errors, then a submission without evidence; explain these errors and recover rather than exposing confusing action-log failures. ([ky #878](https://github.com/sindresorhus/ky/issues/878))
5. **Show a clear plan and honest, actionable verification status.** Scores for plan quality and verification honesty averaged 1.75 for tm. State the ordered steps, identify which test/check ran and whether it passed, and explain blockers such as required package-manager approval. Use the available project environment early. ([Click #3822](https://github.com/pallets/click/issues/3822), [Requests #7432](https://github.com/psf/requests/issues/7432), [ky #878](https://github.com/sindresorhus/ky/issues/878))

## Skipped inputs

- Hugo #15360: skipped because the linked fix PR was closed and unmerged.
- ripgrep #3376: skipped because no merged fix PR with changed test files existed for the issue.
