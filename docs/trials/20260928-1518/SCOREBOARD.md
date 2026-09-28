# Trial scoreboard

Scores are on the trial rubric's 1–5 scale. Pass rate uses the recorded `passed` result. Wall time is in seconds; token counts are reported as recorded. The five trials with both tools are included. Hugo was skipped because its candidate fix PR was closed unmerged, so it had no eligible trial result.

## Results by tool

| Tool | Pass rate | Navigation quality | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 0/5 (0%) | 3.0 | 1.2 | 1.6 | 3.2 | 2.0 | 1.4 | 1.6 | 150.8 s | 379,770 |
| OpenCode | 2/5 (40%) | 4.4 | 3.4 | 4.2 | 4.6 | 4.2 | 4.0 | 4.6 | 83.0 s | 23,716 |

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376) | Fail; 150.8 s; 584,797 tokens | Pass; 97.61 s; 29,161 tokens | tm repeated inspection and made no patch; OpenCode fixed the root-specific cache behavior, added a regression, and passed checks. |
| [Click #3822](https://github.com/pallets/click/issues/3822) | Fail; 183 s; 211,725 tokens | Fail; 83 s; 25,206 tokens | tm made no edit. OpenCode implemented the generic Path change, but the hidden typing tests still failed for default Path values. |
| [Requests #7432](https://github.com/psf/requests/issues/7432) | Fail; 156.07 s; 159,179 tokens | Fail; 86.13 s; 23,716 tokens | Both hit recursive `httpbin` fixture errors in the prescribed hidden-test run. tm's actual worktree change was unclear in its final status; OpenCode made a localized fix and passed its focused authored tests, but the mechanical check still failed. |
| [ky #878](https://github.com/sindresorhus/ky/issues/878) | Fail; 126.71 s; 462,023 tokens | Fail; 80.55 s; 21,772 tokens | tm changed the relevant code, but repeated reads/retries and did not clearly report verification. OpenCode implemented and tested the change; its overlaid changed-file run failed on an unrelated WebKit network error. |
| [Cobra #2257](https://github.com/spf13/cobra/issues/2257) | Fail; 124.843 s; 379,770 tokens | Pass; 44.754 s; 18,916 tokens | tm found the relevant append but did not produce a scoped fix or test. OpenCode copied the args before appending, ran the full Go test suite, and the hidden regression confirmed the fix. |

## Five biggest improvements for tm

1. **Stop repeating the same investigation; convert findings into a change.** Several runs revisited the same files across multiple no-change attempts and still ended without a patch. In ripgrep #3376 tm spent 584,797 tokens without a patch; in Cobra #2257 it found the append site but never made the fix. After an initial focused inspection, require a concrete edit, test, or clear stop/handoff.

2. **Carry context forward during recovery and make retries bounded.** Repeated attempts should build on prior findings rather than restart exploration. Requests #7432 and ky #878 describe repeated reads/retries, while Click #3822 reports three largely repeated attempts. Preserve the identified files, likely fix, and next test in the continuation, and stop if the next attempt has no distinct action.

3. **Give one accurate, actionable status that matches the ticket state.** Distinguish a provider retry from a no-progress recovery, and say whether a patch exists, what was verified, and what should happen next. In Click #3822 and Cobra #2257, the final retry guidance conflicted with an escalated state; in Requests #7432, the final message said no patch was submitted even though the worktree contained the implementation change.

4. **Make token and time costs understandable and reduce waste.** tm's median was 379,770 tokens and 150.8 seconds, versus OpenCode's 23,716 tokens and 83 seconds. Ripgrep #3376 reached 584,797 tokens; Requests #7432 also notes that the displayed token count appeared attached to only the last attempt. Report clearly whether usage is per attempt or cumulative, and use that visibility to stop runaway retries.

5. **Finish with a reliable verification and a concise account of the actual diff.** Report the exact test command and its real outcome, separate environment/test-fixture failures from implementation failures, and state what files changed. In ky #878, the final submission did not clearly say what verification happened; in Click #3822, OpenCode found a hidden typing failure it still needed to fix, illustrating why the complete relevant test—not only a custom assertion—matters.

*Source note: `gohugoio-hugo-15360.json` records a skipped case, not a tool run: the fixing PR was closed unmerged. It is excluded from pass rates and medians.*
