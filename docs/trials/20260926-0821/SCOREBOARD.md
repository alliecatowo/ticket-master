# Trial scoreboard

## Overall results

Five trials had comparable `tm` and OpenCode results. Scores are rubric averages on a 1–5 scale; higher is better. Pass rate means the trial's recorded `passed` value. Time and token figures are medians across those five trials. Tokens are the recorded totals, not normalized for task size.

| Tool | Pass rate | Median wall time | Median tokens |
|---|---:|---:|---:|
| tm | 2/5 (40%) | 392 sec | 1,635,916 |
| OpenCode | 4/5 (80%) | 168.7 sec | 22,664 |

## Mean score by rubric dimension

| Rubric dimension | tm | OpenCode |
|---|---:|---:|
| Navigation quality | 3.0 | 3.6 |
| Context efficiency | 1.2 | 3.6 |
| Plan quality | 1.6 | 4.0 |
| Verification honesty | 2.6 | 3.4 |
| UX copy clarity | 2.2 | 3.6 |
| Recoverability | 1.4 | 3.4 |
| Cost/time | 1.4 | 4.2 |

## tm vs OpenCode by trial

| Trial | tm | OpenCode | Comparison |
|---|---|---|---|
| [pallets/click #3822](https://github.com/pallets/click/issues/3822) | Fail; 205 sec; 648,885 tokens | Fail; 194 sec; 22,664 tokens | Both found the relevant Path typing issue but neither left a working change. tm did not submit work; OpenCode changed the sibling tm clone and incorrectly reported success. |
| [sindresorhus/ky #878](https://github.com/sindresorhus/ky/issues/878) | Pass; 1,200 sec; 33,096 tokens | Pass; 206 sec; 19,583 tokens | Both made the behavior change and the upstream stream test passed. tm hit its time limit and had confusing command/evidence errors; OpenCode was much quicker, though its trial had a non-fresh second invocation and the browser test remained unverified. |
| [psf/requests #7432](https://github.com/psf/requests/issues/7432) | Fail; 392 sec; 2,751,047 tokens | Pass; 52 sec; 20,593 tokens | tm found relevant code but ended without a patch. OpenCode made a focused change and test; it missed a better test command, but the independent hidden-test run passed. |
| [BurntSushi/ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376) | Pass recorded; 330 sec; 1,635,916 tokens | Pass recorded; 125 sec; 41,390 tokens | tm did not submit a patch or verify its work. OpenCode implemented a fix and regression test. **The recorded post-checkout pass is mechanical for both arms:** the required test-file checkout also imported the reference implementation, so it does not independently prove either agent's fix. |
| [spf13/cobra #2257](https://github.com/spf13/cobra/issues/2257) | Fail; 1,160 sec; 3,234,277 tokens | Pass; 169 sec; 25,713 tokens | tm found the relevant code but spent nearly the full time limit without submitting a patch or verification. OpenCode made a focused change and accurately reported verification. |

## Five biggest things for tm to improve

1. **Stop rereading and keep context focused.** Repeated large-file reads drove very high token use without moving work forward: 35 tool calls and 2.75 million tokens in requests #7432; 22 calls and 1.64 million tokens in ripgrep #3376; repeated reads in cobra #2257. Keep a short working summary and inspect targeted snippets after the first pass.
2. **Recover when a turn ends without submission.** In click #3822, requests #7432, ripgrep #3376, and cobra #2257, tm ended with no submitted patch or evidence. A bounded continuation should resume from the diagnosis, attempt the smallest useful change, and clearly say what remains if it still cannot submit.
3. **Make command output and saved evidence dependable.** Ky #878 had a ticket confirmation that was not machine-parseable, missing-evidence errors despite eventual submission, and a verification-result save that first failed parsing. Click #3822 and cobra #2257 also found `tm events` printed usage instead of a useful snapshot. Provide stable structured output and make failed saves unambiguously fail or recover.
4. **Stop promptly once verified work is complete.** Ky #878 reached the 1,200-second run bound even though the one-line fix and relevant stream test were done. Detect completed, verified tasks and end the run instead of spending the remaining budget.
5. **Make failure status and retry instructions concrete.** Several no-submit endings told the user to rerun but did not explain what was retained or what would happen next (click #3822, requests #7432, ripgrep #3376, cobra #2257). State plainly whether a patch exists, which checks ran, what useful finding was preserved, and the exact next recovery action.

## Coverage note

`gohugoio-hugo-15360.json` was read but excluded from the scoreboard: it is marked skipped because no merged fix PR with the required issue-specific regression test was available. The five scored trials above are all root-level trial result JSON files with both tool arms.
