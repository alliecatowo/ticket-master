# Trial scoreboard

This summary covers the **five completed, comparable trials** with results for both tools. `BurntSushi-ripgrep-3376.json` was skipped because its merged fix had no changed test files, so it is not counted. Pass rate is the recorded `passed` result. Rubric scores run from 1 (weak) to 5 (strong). Times and token counts are medians; tokens are the values recorded by each trial, so the counting methods may differ by tool.

## Results by tool

| Tool | Pass rate | Navigation quality (mean) | Context efficiency (mean) | Plan quality (mean) | Verification honesty (mean) | UX copy clarity (mean) | Recoverability (mean) | Cost/time (mean) | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 2/5 (40%) | 2.6 | 1.2 | 2.2 | 2.6 | 1.8 | 2.0 | 1.6 | 515.9 s | 2,529,469 |
| OpenCode | 4/5 (80%) | 4.2 | 4.0 | 4.8 | 4.4 | 4.2 | 4.2 | 4.4 | 78 s | 25,504 |

## tm vs OpenCode per trial

| Trial | tm result (time; tokens) | OpenCode result (time; tokens) | Comparison |
|---|---|---|---|
| [psf/requests #7432](psf-requests-7432.json) | Fail; 515.9 s; 2,529,469 | Pass; 42.6 s; 17,324 | OpenCode passed after a focused fix and regression test. tm found relevant code but did not submit; repeated exploration and a turn-without-submission failure consumed far more time and tokens. |
| [pallets/click #3822](pallets-click-3822.json) | Fail; 198 s; 705,457 | Pass; 167 s; 25,504 | OpenCode implemented generic `Path` and passed checks. tm did not produce a patch, and the independent hidden-test check failed. |
| [gohugoio/hugo #15360](gohugoio-hugo-15360.json) | Pass; 1,057.1 s; 7,712,825 | Pass; 55.7 s; 25,771 | Both passed. tm eventually fixed the issue but took 81 calls; OpenCode made a focused fix, added regression cases, and ran the relevant package tests. |
| [spf13/cobra #2257](spf13-cobra-2257.json) | Pass; 724 s; 3,961,908 | Pass; 78 s; 205,285 | Both fixes passed independent regression tests. tm found the bug but hit submission/evidence errors and spent 64 steps; OpenCode submitted a focused fix with a regression test. |
| [sindresorhus/ky #878](sindresorhus-ky-878.json) | Fail; 172 s; 1,871,148 | Fail; 197.9 s; 23,519 | Both had the same pre-existing full-suite XO failure; targeted hidden progress tests passed for both. OpenCode scored higher on focused execution and context use. |

## Five biggest improvements for tm

1. **Use less context and stop repeating broad searches.** This is the clearest measured gap: context-efficiency mean was 1.2 vs 4.0, median tokens 2.53M vs 25.5K, and tm median time was 8.5 minutes vs OpenCode's 78 seconds. The Hugo run used 7.7M tokens and 81 calls on a narrow fix; the Cobra run used 3.96M tokens and 64 steps, repeatedly revisiting the same code. Start with a short plan, constrain exploration to the issue and likely regression test, and avoid rereading already-understood paths. ([Hugo](gohugoio-hugo-15360.json), [Cobra](spf13-cobra-2257.json), [requests](psf-requests-7432.json))

2. **Make submission and evidence failures understandable and recoverable.** In the requests and Click trials, tm ended without submitting; in Cobra, submission required evidence but the error was surfaced as inconsistent state, and an artifact operation rejected `verification`. Give direct guidance on valid evidence/artifact IDs and formats, preserve verified work, and offer a bounded recovery step. ([requests](psf-requests-7432.json), [Click](pallets-click-3822.json), [Cobra](spf13-cobra-2257.json))

3. **Explain failures in plain language and finish with a useful summary.** Generic messages such as “something went wrong,” raw API/invariant failures, and retry-only advice leave users unsure what happened. State whether a change was made or submitted, what verification passed or failed, and the next actionable step. The Hugo run also surfaced internal tool errors before recovering; Click ended without a useful recovery. ([Click](pallets-click-3822.json), [Hugo](gohugoio-hugo-15360.json), [Cobra](spf13-cobra-2257.json))

4. **Choose the correct project root at startup and keep tools anchored there.** The ky run began from the trial parent, reported a file-access error, and explored sibling clones before correcting its path. Make the selected project root explicit and ensure file and test operations consistently use it. ([ky](sindresorhus-ky-878.json))

5. **Make the event-log command discoverable and useful by default.** In multiple trials, the requested bare `tm events` printed usage rather than events, forcing discovery of a subcommand. Provide a useful one-shot event listing or clearly suggest `events tail --no-follow` so users can inspect what happened. ([requests](psf-requests-7432.json), [Click](pallets-click-3822.json), [ky](sindresorhus-ky-878.json))

## Notes

- The rubric dimensions are averaged across the five included trials and rounded to one decimal (displayed to two decimals where helpful).
- The ky full `pnpm test` check failed for both tools at the same pre-existing XO error in `test/http-error.ts:218`; targeted hidden progress tests passed. This is why that trial records `passed: false` for each arm despite the targeted success.
- The trials report different token-accounting sources. Treat token totals as the recorded measure, not necessarily a perfectly like-for-like tokenizer count.
