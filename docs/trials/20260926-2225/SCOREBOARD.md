# Trial scoreboard

Scores are on a 1–5 scale. Pass rate and score averages use the five trials with recorded runs; Hugo was skipped because no qualifying merged PR with regression tests was found. Times and token counts are medians across those five runs. “Tokens” are the counts recorded in the trial results.

## Results by tool

| Tool | Pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time (s) | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 2/5 (40%) | 2.6 | 1.0 | 1.6 | 3.2 | 2.2 | 1.6 | 1.4 | 125.9 | 393,181 |
| OpenCode | 5/5 (100%) | 4.0 | 4.0 | 4.0 | 4.8 | 4.0 | 3.6 | 4.6 | 97.0 | 25,306 |

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| [Click #3822](https://github.com/pallets/click/issues/3822) | **Failed** · 30 s · 213,149 tokens | **Passed** · 100 s · 25,306 tokens | tm repeated inspections, produced no patch, and hit a provider rate limit. OpenCode fixed the typing issue and passed mypy, pyright, and the relevant tests. |
| [Requests #7432](https://github.com/psf/requests/issues/7432) | **Failed** · 125.9 s · 393,181 tokens | **Passed** · 72.56 s · 22,921 tokens | tm found the function but repeated searches without editing or testing. OpenCode fixed the regression and passed the focused test. |
| [Cobra #2257](https://github.com/spf13/cobra/issues/2257) | **Failed** · 95 s · 176,997 tokens | **Passed** · 97 s · 23,631 tokens | tm repeatedly inspected the same code and did not produce a patch. OpenCode implemented the change and passed the Go test suite. |
| [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376) | **Passed*** · 144.115 s · 612,969 tokens | **Passed*** · 61.738 s · 32,797 tokens | Both runs missed the actual cache fix. The reported hidden-test check passed only after the PR’s changed production file was overlaid, so it does not establish that either tool solved the issue. |
| [ky #878](https://github.com/sindresorhus/ky/issues/878) | **Passed** · 145 s · 666,298 tokens | **Passed** · 153 s · 28,823 tokens | Both found the fix; tm needed retries and substantially more tokens. OpenCode added a focused test and refined it after its first version failed. |
| [Hugo #15360](https://github.com/gohugoio/hugo/issues/15360) | **Skipped** | **Skipped** | The result file says no merged PR with the requested regression tests was found; neither tool has a scored run. |

\* The trial file marks the run passed, but explicitly cautions that the hidden-test check used the full PR production change and therefore does not prove the tool solved the task.

## Five biggest things tm should improve

1. **Stop repeated no-progress exploration and turn findings into a concrete next step.** Several runs reread or searched the same code without editing or testing. Add a strong repetition check: after one repeated inspection, make a focused edit, investigate a genuinely different lead, or stop with a concise status. This pattern was especially clear in [Requests #7432](https://github.com/psf/requests/issues/7432) (three attempts, no patch) and [Cobra #2257](https://github.com/spf13/cobra/issues/2257) (repeated reads, no patch).
2. **Cut token use sharply.** tm’s median was 393,181 tokens versus OpenCode’s 25,306—about 15.5 times as many—while tm passed only two of five trials. Repeated discovery drove extreme totals in [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376) (612,969) and [ky #878](https://github.com/sindresorhus/ky/issues/878) (666,298). Set a tighter investigation budget and summarize/reuse findings across retries.
3. **Make retries and recovery clear, bounded, and useful.** In [Click #3822](https://github.com/pallets/click/issues/3822) and [Cobra #2257](https://github.com/spf13/cobra/issues/2257), “retry scheduled,” “resume,” and manual retry instructions conflicted or repeated the failed approach. In [ky #878](https://github.com/sindresorhus/ky/issues/878), submission errors did not clearly say what failed or how to recover. Give one unambiguous status and next action, and carry prior findings into a retry.
4. **Navigate to the right code sooner and follow through with a scoped plan.** tm scored 2.6 for navigation and 1.6 for plan quality. It focused on repeated `walk.rs` reads rather than the `dir.rs` cache behavior in [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376), and did not move from locating the relevant function to an edit in [Requests #7432](https://github.com/psf/requests/issues/7432). State the likely target and a short edit-and-verify plan before spending more context.
5. **Run the right verification and report what it proves.** tm did not run a test in failed [Click #3822](https://github.com/pallets/click/issues/3822), [Requests #7432](https://github.com/psf/requests/issues/7432), or [Cobra #2257](https://github.com/spf13/cobra/issues/2257). In [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376), the mechanical pass was not evidence of a tm-authored fix because production PR code had been overlaid. Verify the task’s targeted regression against the tool’s own patch, and label pre-existing, overlaid, or broader checks precisely.
