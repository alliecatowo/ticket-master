# Trial scoreboard

This compares the five trials that contain results for both tools. A “pass” is the trial JSON's recorded `passed` value; it is not a claim that every post-run check independently validated the tool's patch. Scores are rubric averages on a 1–5 scale. Time and token figures are medians across those five runs.

| Tool | Pass rate | Navigation quality | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 1/5 (20%) | 2.2 | 1.0 | 1.2 | 3.8 | 1.8 | 1.4 | 1.2 | 110 s | 485,153 |
| OpenCode | 3/5 (60%) | 4.0 | 3.4 | 4.4 | 4.2 | 4.2 | 3.8 | 4.4 | 92.7 s | 23,846 |

## tm vs OpenCode by trial

| Trial | tm result | OpenCode result | What happened |
|---|---|---|---|
| [Click #3822](https://github.com/pallets/click/issues/3822) | Fail · 159.4 s · 579,262 tokens | Fail · 94.5 s · 23,846 tokens | tm found relevant typing code but did not produce a patch. OpenCode made a patch in the other arm's clone and reported checks that did not test the actual PR case. |
| [ky #878](https://github.com/sindresorhus/ky/issues/878) | Fail · 110 s · 168,788 tokens | Fail · 83 s · 18,918 tokens | tm eventually produced the expected stream fix, but spent heavily on command/tool errors. OpenCode worked directly in the relevant source and reported focused-test success while noting broader checks were blocked. |
| [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376) | Pass* · 183 s · 605,187 tokens | Pass* · 174 s · 34,085 tokens | tm did not submit a patch; OpenCode implemented and tested a cache-key fix. The recorded mechanical pass in both arms is not proof of either patch: checking out the merged source replaced the implementation under test. |
| [Cobra #2257](https://github.com/spf13/cobra/issues/2257) | Fail · 55 s · 485,153 tokens | Pass · 9 s · 21,741 tokens | tm repeated inspection across retries without a change. OpenCode made a targeted copy-before-append fix and passed the full Go suite. |
| [Requests #7432](https://github.com/psf/requests/issues/7432) | Fail · 89.6 s · 358,691 tokens | Pass · 92.7 s · 30,271 tokens | tm exhausted retries without implementing a fix. OpenCode completed the change and passed the hidden redirect test. |

\* See the [ripgrep trial notes](BurntSushi-ripgrep-3376.json): the merged `dir.rs` checkout included the implementation and regression test, so that check overwrote each arm's implementation. Treat the JSON pass flag as a mechanical result, not validated success.

The Hugo file, [gohugoio-hugo-15360.json](gohugoio-hugo-15360.json), was explicitly skipped because no merged fix with tests was available; it is not counted in the comparisons or averages.

## Five biggest improvements for tm

1. **Break out of repeated exploration and move to a concrete edit or test.** In Requests, Cobra, and ripgrep, tm revisited the same files/searches over multiple attempts and submitted no change; Click also ended without a patch. Reuse findings between retries and change investigative approach when repeated reads stop producing progress. ([Requests #7432](https://github.com/psf/requests/issues/7432), [Cobra #2257](https://github.com/spf13/cobra/issues/2257), [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376), [Click #3822](https://github.com/pallets/click/issues/3822))
2. **Cut token use sharply, especially on failed runs.** tm's median was 485,153 tokens versus OpenCode's 23,846—about 20 times as many—while tm passed only one of five trials. Individual tm runs used 358,691–605,187 tokens without a submitted fix in Requests, Cobra, and ripgrep. Set a no-progress budget and carry forward compact context. ([Requests #7432](https://github.com/psf/requests/issues/7432), [Cobra #2257](https://github.com/spf13/cobra/issues/2257), [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376))
3. **Make retry and recovery instructions agree with the actual run state.** tm messages sometimes said a retry was scheduled while also telling the user to resume, or said no retry was scheduled but suggested continuing the saved session. End each run with one accurate next action and preserve findings when a retry happens. ([Click #3822](https://github.com/pallets/click/issues/3822), [Cobra #2257](https://github.com/spf13/cobra/issues/2257), [Requests #7432](https://github.com/psf/requests/issues/7432), [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376))
4. **Improve command/tool usability and error recovery.** In ky, ticket creation returned human-readable text when the caller needed a bare ID, and shell/tool-input errors plus exploratory snippets consumed substantial effort. Provide script-friendly structured output and schema-specific repair guidance; after a command fails, narrow the next attempt instead of repeating it. ([ky #878](https://github.com/sindresorhus/ky/issues/878))
5. **Start with a short, scoped plan and verify the actual requested behavior.** tm's mean plan-quality score was 1.2/5. In ky, saying tests ran did not establish their outcome; in Click, no scoped plan was stated. Identify the target behavior, relevant code, and check before editing, then report which check passed or failed and whether it covered the hidden/issue scenario. ([Click #3822](https://github.com/pallets/click/issues/3822), [ky #878](https://github.com/sindresorhus/ky/issues/878), [Cobra #2257](https://github.com/spf13/cobra/issues/2257))
