# Trial scoreboard

Scores are on the rubric's 1–5 scale. Pass rate and summary statistics use the five trials with results for both tools. Hugo was skipped because its linked fix PR was closed without a merge, so it is excluded from the rates and averages.

## tm

| Measure | Result |
|---|---:|
| Pass rate | 2/5 (40%) |
| Mean navigation quality | 2.8/5 |
| Mean context efficiency | 1.4/5 |
| Mean plan quality | 1.8/5 |
| Mean verification honesty | 2.6/5 |
| Mean UX copy clarity | 1.8/5 |
| Mean recoverability | 2.0/5 |
| Mean cost/time | 2.2/5 |
| Median wall time | 467.5 seconds |
| Median tokens | 1,708,426 |

## OpenCode

| Measure | Result |
|---|---:|
| Pass rate | 4/5 (80%) |
| Mean navigation quality | 4.0/5 |
| Mean context efficiency | 3.6/5 |
| Mean plan quality | 4.2/5 |
| Mean verification honesty | 3.8/5 |
| Mean UX copy clarity | 4.0/5 |
| Mean recoverability | 3.8/5 |
| Mean cost/time | 4.8/5 |
| Median wall time | 98 seconds |
| Median tokens | 33,330 |

## Head-to-head by trial

| Trial | tm | OpenCode | Summary |
|---|---|---|---|
| Click #3822 | Failed; 177.6 sec, 775,067 tokens | Failed; 100.7 sec, 51,140 tokens | OpenCode made and checked a focused attempt, but the PR's hidden typing/runtime checks still failed. tm did not submit a patch or run tests. |
| ky #878 | Passed; 73 sec, 895,489 tokens | Passed; 84 sec, 33,330 tokens | Both passed the hidden stream checks. tm was faster, while OpenCode had clearer planning and reporting. |
| ripgrep #3376 | Passed; 467.5 sec, 1,708,426 tokens | Passed; 192.7 sec, 44,194 tokens | Both were marked passed, though the package test used a checked-out fix file and therefore did not independently prove either tool's patch. OpenCode recovered from mistakes and finished much faster. |
| Requests #7432 | Failed; 606 sec, 4,121,406 tokens | Passed; 50 sec, 19,454 tokens | tm spent a long time investigating without a patch. OpenCode found the change quickly but missed that the project virtual environment could run the tests. |
| Cobra #2257 | Failed; 885 sec, 3,846,515 tokens | Passed; 98 sec, 20,014 tokens | OpenCode fixed the argument-slice aliasing bug and passed the Go test suite. tm found the relevant code but did not submit a patch or run tests. |
| Hugo #15360 | Skipped | Skipped | No merged fix PR with tests was available. |

## Five biggest improvements for tm

1. **Stop unproductive loops much sooner.** Requests and Cobra each consumed several million tokens and 10–15 minutes without producing a patch; ripgrep also involved repeated reads and no submitted patch. Detect repeated searches/reads without progress, then pause and redirect rather than continuing at high cost. (Requests #7432, Cobra #2257, ripgrep #3376)
2. **Make the next step concrete when a run fails.** “Something went wrong” and a generic rerun suggestion left users unsure whether work was saved or a retry was already scheduled. Explain what was completed, what was retained, what failed, and the useful next action. (Click #3822, ripgrep #3376, Requests #7432, Cobra #2257)
3. **Show a scoped plan and keep context focused.** OpenCode generally stated a short plan and reached a fix faster. tm's repeated reads of large files and searches outside the repository inflated token use without helping it converge. (Click #3822, ripgrep #3376, Cobra #2257)
4. **Improve verification, including test setup and evidence.** tm did not run tests in Click or Cobra; in ripgrep, the reported package pass came after checking out a file containing the fix, so it was not independent evidence of tm's solution. Make it easier to choose focused tests and clearly distinguish a real check of the agent's patch from environmental or fixture results. (Click #3822, ripgrep #3376, Cobra #2257)
5. **Make usage and event commands understandable.** The ky run reported 895,489 input and zero output tokens, which is not a useful-looking breakdown; Click's `tm events` showed help and exited instead of providing the event history. Label unavailable usage clearly and make event inspection's default behavior actionable. (ky #878, Click #3822)
