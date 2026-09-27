# Trial scoreboard

Scores are on the rubric's 1–5 scale. The six usable paired trials were `sindresorhus/ky#878`, `psf/requests#7432`, `BurntSushi/ripgrep#3376`, `spf13/cobra#2257`, and `pallets/click#3822`—five scored trials, with one tm and one OpenCode run each. `gohugoio/hugo#15360` was skipped because its linked PR was closed and unmerged, so it is excluded from rates and averages. Lower time and token counts are better; tokens are the reported totals in the trial files.

## tm

| Measure | Result |
|---|---:|
| Pass rate | 3/5 (60%) |
| Mean navigation quality | 2.0 / 5 |
| Mean context efficiency | 1.4 / 5 |
| Mean plan quality | 1.8 / 5 |
| Mean verification honesty | 3.0 / 5 |
| Mean UX copy clarity | 2.4 / 5 |
| Mean recoverability | 2.2 / 5 |
| Mean cost/time score | 1.4 / 5 |
| Median wall time | 473 seconds (7m 53s) |
| Median tokens | 1,634,307 |

## OpenCode

| Measure | Result |
|---|---:|
| Pass rate | 5/5 (100%) |
| Mean navigation quality | 3.8 / 5 |
| Mean context efficiency | 3.0 / 5 |
| Mean plan quality | 4.2 / 5 |
| Mean verification honesty | 4.8 / 5 |
| Mean UX copy clarity | 4.0 / 5 |
| Mean recoverability | 3.6 / 5 |
| Mean cost/time score | 4.0 / 5 |
| Median wall time | 114.3 seconds (1m 54s) |
| Median tokens | 33,137 |

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| `sindresorhus/ky#878` | Did not pass; 0.05s, 0 tokens | Passed; 114.3s, 22,477 tokens | tm stopped before the agent turn because `coder.fast` named an unregistered provider. OpenCode fixed the stream flush behavior and added a regression test. |
| `psf/requests#7432` | Passed; 140.5s, 789,281 tokens | Passed; 102.9s, 300,291 tokens | Both got the fix. tm eventually did so after repeated rereading and did not give a clear test-pass report; OpenCode added a regression test and reported its focused verification. |
| `BurntSushi/ripgrep#3376` | Passed flag; 473s, 4,386,531 tokens | Passed flag; 86s, 33,137 tokens | Neither agent completed a verified fix. tm submitted after extensive repeated exploration; its reported test was against the unmodified base. OpenCode accurately reported that it had not confirmed or fixed the regression. |
| `spf13/cobra#2257` | Passed; 629.2s, 3,661,949 tokens | Passed; 148.2s, 322,253 tokens | Both produced the argument-copy fix. tm reported test runs, while OpenCode added a regression test and completed the full suite. |
| `pallets/click#3822` | Did not pass; 585.7s, 1,634,307 tokens | Passed; 138.3s, 24,253 tokens | tm did not submit a patch and gave misleading instructions about retained edits. OpenCode completed the change and passed. |

The ripgrep pass flags need context: the recorded `passed` value does not mean either agent delivered a verified fix in that trial. The trial notes say the mechanical test ran against the base code before the merge checkout replaced the changed file.

## Five biggest improvements for tm

1. **Prevent repeated exploration from consuming the run.** Repeated reads and searches dominated the Requests, ripgrep, Cobra, and Click trials. Requests used 789,281 tokens; ripgrep used 4,386,531 and 64 tool steps; Cobra took 629 seconds. Track already-inspected files and interrupt loops with a compact checkpoint and next action. (Trials: `psf/requests#7432`, `BurntSushi/ripgrep#3376`, `spf13/cobra#2257`, `pallets/click#3822`.)
2. **Make plans narrow and test the change that was actually made.** Start with the affected function and a focused regression test, then run the relevant test after editing. Requests lacked a clear test-pass report, ripgrep's test was not evidence for the agent's fix, and Click spent a long time inspecting without submitting. (Trials: `psf/requests#7432`, `BurntSushi/ripgrep#3376`, `pallets/click#3822`.)
3. **Make setup failures recoverable before starting ticket work.** In ky, a provider name that was not configured ended the run before any agent work. Guide the user to a valid configured provider or a repair action, and make clear that no work was performed. (Trial: `sindresorhus/ky#878`.)
4. **Keep completion messages faithful to the actual state.** Click's run said edits were retained even though it submitted no patch. Cobra's ending asked the user to accept or reject after describing the work as submitted, leaving the next step unclear. Summarize plainly whether there is a patch, what was verified, and what the user should do next. (Trials: `pallets/click#3822`, `spf13/cobra#2257`.)
5. **Make submission requirements and usage visible and consistent.** Requests' worker instructions allowed an empty evidence list even though submission rejected it, creating an avoidable failure and extra artifact-store step. Cobra's ticket view showed zero spent tokens despite actual usage being available through stats. Align instructions with submission rules and show real usage in the ticket summary. (Trials: `psf/requests#7432`, `spf13/cobra#2257`.)
