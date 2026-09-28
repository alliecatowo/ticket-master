# Trial scoreboard

Plain-English summary of the six top-level JSON result files. Five contain scored tool runs; `gohugoio/hugo#15360` was skipped because its proposed fix PR was closed but not merged, so it is not counted in scores or pass rates. Scores are means on the recorded 1–5 rubric. Times and tokens are medians across available values; Cobra's tm token count was not reported.

## Results by tool

| Tool | Pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 1/5 (20%) | 2.4 | 1.2 | 1.2 | 2.6 | 1.8 | 1.4 | 1.6 | 80.0 s | 269,784* |
| OpenCode | 4/5 (80%) | 4.6 | 3.6 | 4.4 | 4.6 | 4.2 | 4.0 | 5.0 | 70.3 s | 49,636 |

\* tm's median uses the four reported token counts; the Cobra run did not expose tokens. The `ripgrep` result is marked passed for both tools, but its post-run test checkout overwrote the implementation under test, so that pass is mechanical rather than independent implementation evidence.

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What stood out |
|---|---|---|---|
| `pallets/click#3822` | Failed; 77.2 s; 199,995 tokens | Passed; 76.3 s; 21,160 tokens | tm found the right code and typing precedent but did not make a fix; repeated inspection and tests failed. OpenCode implemented and verified the generic typing fix. |
| `psf/requests#7432` | Failed; 62.6 s; 198,517 tokens | Passed; 67.7 s; 212,802 tokens | tm repeatedly inspected the same target without submitting a patch. OpenCode fixed stream detection, added focused coverage, and recovered from test setup problems. |
| `BurntSushi/ripgrep#3376` | Marked passed; 108.5 s; 365,884 tokens | Marked passed; 104.8 s; 536,165 tokens | tm did not implement a change or run a targeted test. OpenCode made the fix and regression test. Both post-run passes are not independent evidence because checking out the test file also replaced the implementation. |
| `spf13/cobra#2257` | Failed; 92.6 s; tokens unavailable | Passed; 70.3 s; 27,036 tokens | tm explored the likely code but made no change or focused test. OpenCode fixed the shared-slice mutation and passed the suite, though it did not add the regression test it had promised. |
| `sindresorhus/ky#878` | Failed; 80.0 s; 339,572 tokens | Failed; 56.5 s; 49,636 tokens | Both made focused fixes and passed the empty-response test; the full browser test had a WebKit network failure in both arms. tm spent far more reported tokens and gave confusing retry updates; OpenCode had the more focused run. |
| `gohugoio/hugo#15360` | Skipped | Skipped | Excluded by the trial protocol because the fix PR was closed and unmerged. |

## Five biggest improvements for tm

1. **Turn discovery into a concrete change sooner.** In Click, Requests, Cobra, and ripgrep, tm found the relevant subsystem or file but then ended without implementing a fix or running a targeted check. OpenCode completed an implementation in each of those runs (`pallets/click#3822`, `psf/requests#7432`, `spf13/cobra#2257`, `BurntSushi/ripgrep#3376`).
2. **Stop repeating exploration and carry findings forward.** Re-reading the same files and repeating searches across retries consumed substantial effort without progress in Click, Requests, ripgrep, and Ky. Keep a concise record of what was already learned, then use the next attempt for an edit or focused test (`pallets/click#3822`; `psf/requests#7432`; `BurntSushi/ripgrep#3376`; `sindresorhus/ky#878`).
3. **Make retry and recovery messages match the actual ticket state.** Several runs told users both that a retry was scheduled and that none was scheduled, or recommended `tm ticket retry T-1` after escalation. Give one accurate status and one valid next action, especially when the retry budget is exhausted (`pallets/click#3822`; `psf/requests#7432`; `spf13/cobra#2257`; `sindresorhus/ky#878`).
4. **Control context and explain usage.** tm's reported median was about 270k tokens versus OpenCode's 50k, with tm reaching 365,884 on ripgrep and 339,572 on Ky. Avoid repeated inspection, set or surface a useful budget, and explain cumulative usage when it is unexpectedly high (`BurntSushi/ripgrep#3376`; `sindresorhus/ky#878`; `pallets/click#3822`).
5. **Finish with focused, trustworthy verification and a clear summary.** Run the most relevant regression test after the change, distinguish focused results from unrelated environment failures, and state exactly what was verified. The Ky full browser command failed on a WebKit network error even though the focused empty-response test passed; ripgrep's post-checkout pass was mechanical, not proof of tm's implementation (`sindresorhus/ky#878`; `BurntSushi/ripgrep#3376`).
