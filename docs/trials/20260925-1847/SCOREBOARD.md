# Trial scoreboard

This scoreboard covers the five trials with results for both tools. `Passed` uses the trial's recorded pass/fail flag. Scores are on the rubric's 1–5 scale; higher is better. Times are seconds and token counts are the reported counts. The Hugo file was read but marked `skipped` because its linked fixing PR was closed unmerged, so it is not counted in the comparisons.

## Overall results

| Tool | Pass rate | Mean navigation | Mean context efficiency | Mean plan quality | Mean verification honesty | Mean UX clarity | Mean recoverability | Mean cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 2/5 (40%) | 2.8 | 1.6 | 2.2 | 2.0 | 1.8 | 1.6 | 2.2 | 511 s | 1,386,134 |
| OpenCode | 4/5 (80%) | 4.2 | 3.8 | 4.0 | 4.2 | 4.2 | 4.0 | 4.8 | 127 s | 32,223 |

## tm vs OpenCode by trial

| Trial | tm result (time; tokens) | OpenCode result (time; tokens) | Comparison |
|---|---|---|---|
| Click #3822 (`pallets-click-3822.json`) | Fail (511 s; 1,386,134) | Pass (141 s; 32,664) | OpenCode implemented the typing fix and passed pytest and mypy; tm did not submit a change and its checks failed. |
| Ky #878 (`sindresorhus-ky-878.json`) | Fail (43 s; 423,469) | Fail (62 s; 27,412) | Both got the relevant stream tests passing (19/19), but the full check was blocked by missing Playwright browsers and pnpm build-script restrictions. tm's progress did not clearly show test outcomes; OpenCode explained the blockers. |
| Requests #7432 (`psf-requests-7432.json`) | Fail (605 s; 3,291,872) | Pass (47 s; 32,223) | OpenCode added a focused regression test and passed the targeted file after setup; tm stopped without submitting a fix. |
| ripgrep #3376 (`BurntSushi-ripgrep-3376.json`) | Pass* (292 s; 1,312,951) | Pass* (287 s; 35,612) | OpenCode reported implementing and testing a fix. tm's run ended without submitting; the mechanical pass used the merged test-file checkout, which also contained the reference implementation. Treat both recorded passes cautiously. |
| Cobra #2257 (`spf13-cobra-2257.json`) | Pass* (895 s; 4,113,841) | Pass (127 s; 22,492) | OpenCode made a focused fix and reported full-suite tests. tm ended at the step limit after an internal submission error; its mechanical test-file checkout pass does not establish that tm fixed the issue. |

`*` The raw recorded `passed` flag is true, but the trial narrative says the tm run did not submit an implementation. In the ripgrep and Cobra trials, checking out the merged PR test file also brought in reference implementation code, so those passes should not be read as proof that tm solved the issue.

## Five biggest improvements for tm

1. **Make sure a run reaches a submitted fix, or clearly say that it did not.** This is the main outcome gap: tm submitted no fix in Click and Requests, and the ripgrep and Cobra narratives also describe runs ending without submission. Click's verification had a failing pytest and 11 mypy errors; Requests stopped after 605 seconds without a fix. (`pallets-click-3822.json`, `psf-requests-7432.json`, `BurntSushi-ripgrep-3376.json`, `spf13-cobra-2257.json`)
2. **Cut repeated investigation and carry useful context across retries.** tm's context-efficiency average was 1.6/5, versus 3.8 for OpenCode. Click reports repeated overlapping reads; Requests reports broad/repeated searches and about 3.29 million tokens; Cobra reports revisiting the same paths and about 4.1 million tokens. Deduplicate reads, keep a short working summary, and resume retries from it. (`pallets-click-3822.json`, `psf-requests-7432.json`, `spf13-cobra-2257.json`)
3. **Set a compact plan and stop before the run burns its budget.** tm averaged 511 seconds versus OpenCode's 127, and its median token count was about 43 times higher. Cobra reached the 64-step limit after 895 seconds; Click took 511 seconds and Requests 605 seconds without a submitted change. Bound exploration, state the next action, and reserve time for implementation and tests. (`spf13-cobra-2257.json`, `pallets-click-3822.json`, `psf-requests-7432.json`)
4. **Finish with an honest, visible verification summary.** tm's verification-honesty average was 2.0/5. In Ky, progress only said “Ran tests,” without an outcome; in Click the tests and mypy failed, and in Cobra the run ended without a clear account of what was verified. Always report the exact command, pass/fail result, blockers, and whether checks apply to tm's own change or a reference checkout. (`sindresorhus-ky-878.json`, `pallets-click-3822.json`, `spf13-cobra-2257.json`, `BurntSushi-ripgrep-3376.json`)
5. **Make errors and recovery state understandable.** tm averaged 1.6/5 on recoverability and 1.8 on UX clarity. Click's retry repeated investigation and again failed to submit; Requests showed a generic turn-failed/retry message without clarifying saved work; Ky exposed confusing transient submission errors despite eventually succeeding; Cobra ended with “something went wrong” and a retry command. Explain the cause, what work was retained, and what the retry will do. (`pallets-click-3822.json`, `psf-requests-7432.json`, `sindresorhus-ky-878.json`, `spf13-cobra-2257.json`)
