# Trial scoreboard

Six paired trials were scored. A higher rubric score is better (scale: 1–5). “Pass” is the recorded trial result. Times are seconds; token counts are as reported by each trial. Medians are across the six trials.

## Overall results

| Tool | Pass rate | Mean navigation | Mean context efficiency | Mean plan quality | Mean verification honesty | Mean UX clarity | Mean recoverability | Mean cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 0/6 (0%) | 2.33 | 1.33 | 1.33 | 3.33 | 2.33 | 1.50 | 1.83 | 129.74 s | 565,264 |
| OpenCode | 2/6 (33%) | 3.50 | 3.33 | 4.33 | 4.17 | 4.00 | 4.00 | 4.33 | 92.32 s | 31,607 |

In these trials, tm's strongest average was verification honesty, though it still trailed OpenCode. Its biggest gaps were context efficiency, planning, and recovery. The median reported token use for tm was about 18 times OpenCode's.

## Head-to-head by trial

| Trial | tm result (time; tokens) | OpenCode result (time; tokens) | Comparison |
|---|---|---|---|
| [Click #3822](pallets-click-3822.json) | Fail; 154 s; 595,029 | Fail; 75 s; 36,276 | Neither passed. tm found relevant code but made no change; OpenCode edited the sibling tm checkout instead of its own. |
| [Hugo #15360](gohugoio-hugo-15360.json) | Fail; 392.6 s; 996,809 | Fail; 74.745 s; 26,938 | Neither passed. tm repeatedly reread files without submitting a change; OpenCode made a focused fix and passed its focused test, but the overlaid broader test set failed. |
| [Ky #878](sindresorhus-ky-878.json) | Fail; 96.4 s; 218,913 | Fail; 181.4 s; 23,486 | Neither passed the full trial. Both implemented the fix; tm's focused stream test passed (19 tests), while OpenCode's focused run timed out. Both hit a repository lint failure. |
| [Requests #7432](psf-requests-7432.json) | Fail; 104.66 s; 456,874 | **Pass**; 109.63 s; 289,624 | OpenCode passed the hidden redirect test. tm repeated inspection and made no edit. |
| [ripgrep #3376](BurntSushi-ripgrep-3376.json) | Fail; 205.438 s; 613,326 | Fail; 235.530 s; 37,347 | Neither passed. tm made no change; OpenCode edited the wrong part of the behavior and its hidden test still failed. |
| [Cobra #2257](spf13-cobra-2257.json) | Fail; 105.47 s; 535,499 | **Pass**; 55.54 s; 18,871 | OpenCode passed, while tm's regression test failed because the observed argument was `--` instead of `x`. |

## Five biggest things tm should improve

1. **Stop repeating investigations that are going nowhere.** In Click, Hugo, Requests, ripgrep, and Cobra, retries substantially repeated reads/searches and ended without a code change. Preserve the diagnosis across retries and require each new attempt to take a distinct next step—edit, run a focused test, or stop with a useful blocker. ([Click](pallets-click-3822.json), [Hugo](gohugoio-hugo-15360.json), [Requests](psf-requests-7432.json), [ripgrep](BurntSushi-ripgrep-3376.json), [Cobra](spf13-cobra-2257.json))

2. **Put a hard cumulative limit on time and tokens.** tm used a median 565,264 tokens per trial versus 31,607 for OpenCode, and the Hugo run reached 996,809 tokens over 392.6 seconds without a submitted change. Count usage across retries and stop or ask for a new direction before repeated work becomes this expensive. ([Hugo](gohugoio-hugo-15360.json), [ripgrep](BurntSushi-ripgrep-3376.json), [Click](pallets-click-3822.json))

3. **Make the plan lead to a concrete implementation and test.** tm scored 1.33/5 on plan quality. On five of six issues it failed to submit a change; the exception, Ky, included a fix and a passing focused test. Start with a short plan, then move promptly from locating the code to a targeted edit and the relevant regression test. ([Click](pallets-click-3822.json), [Hugo](gohugoio-hugo-15360.json), [Requests](psf-requests-7432.json), [ripgrep](BurntSushi-ripgrep-3376.json), [Cobra](spf13-cobra-2257.json), [Ky](sindresorhus-ky-878.json))

4. **Give one clear, truthful recovery instruction.** Several runs told users to resume a session while also saying an automatic retry was scheduled, then ended with a different manual retry command. The user should see one action that matches the ticket's actual state, plus whether to wait or act now. ([Click](pallets-click-3822.json), [Hugo](gohugoio-hugo-15360.json), [Requests](psf-requests-7432.json), [ripgrep](BurntSushi-ripgrep-3376.json), [Cobra](spf13-cobra-2257.json))

5. **Improve focused diagnosis and verification before declaring the work done.** The no-change runs sometimes reached the wrong verification target: Cobra's generated test saw `--` instead of `x`, and ripgrep searched broadly for a binary after Cargo's build output was not where expected. In the Ky success path, tm's focused stream tests passed but the full command failed at an unrelated existing lint error; summarize those outcomes separately. ([Cobra](spf13-cobra-2257.json), [ripgrep](BurntSushi-ripgrep-3376.json), [Ky](sindresorhus-ky-878.json))

## How to read this

The scores and pass/fail results are those recorded in the JSON trials; this is a six-issue sample, not a general benchmark. Each linked trial file contains the evaluator's detailed notes, including test scope and failure explanations.
