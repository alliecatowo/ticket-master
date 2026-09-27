# Trial scoreboard

## Results by tool

Scores are on the rubric’s 1–5 scale. Pass rate, mean rubric scores, and median usage/time include the five trials with both tool runs. The Hugo JSON was read but excluded: it is marked skipped because no merged fix PR with tests was available (`gohugoio-hugo-15360.json`).

| Tool | Pass rate | Mean navigation | Mean context efficiency | Mean plan quality | Mean verification honesty | Mean UX clarity | Mean recoverability | Mean cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 1/5 (20%) | 2.20 | 1.00 | 1.20 | 2.20 | 2.00 | 1.60 | 1.40 | 113.48 s | 357,389 |
| OpenCode | 4/5 (80%) | 3.60 | 3.00 | 3.60 | 4.20 | 3.60 | 3.40 | 4.20 | 68.69 s | 34,541 |

## tm vs OpenCode by trial

| Trial | tm result | OpenCode result | Comparison |
|---|---|---|---|
| `pallets-click-3822.json` | Fail; 200 s; 459,141 tokens | Pass; 90 s; 353,403 tokens | OpenCode implemented the change and passed runtime and typing verification; tm got stuck rereading and made no patch. |
| `sindresorhus-ky-878.json` | Fail; 180.07 s; 608,692 tokens | Fail; 68.69 s; 27,508 tokens | Both implementations passed hidden stream tests, but neither trial counted as a pass. tm's correct diff was reported as failed; OpenCode reported its test run and unrelated lint failure more clearly. |
| `psf-requests-7432.json` | Fail; 113.48 s; 242,490 tokens | Pass; 69.97 s; 34,541 tokens | tm hit provider HTTP 429 before editing or testing; OpenCode made the fix and its targeted tests passed. |
| `spf13-cobra-2257.json` | Fail; 101.81 s; 357,389 tokens | Pass; 64.05 s; 256,755 tokens | tm stopped after repetitive investigation without a patch; OpenCode changed the implementation, added a regression test, and passed the suite. |
| `BurntSushi-ripgrep-3376.json` | Pass*; 97.28 s; 88,485 tokens | Pass*; 41.58 s; 20,006 tokens | Neither agent produced a patch; the hidden regression test passed mechanically against checked-out test source. OpenCode's recorded outcome was faster and used fewer tokens. |

*The result JSON marks both runs as passed, while its notes say the hidden test passed independently of either agent's implementation. Treat this pass as a mechanical test outcome, not evidence that the agent fixed the issue (`BurntSushi-ripgrep-3376.json`).

## Five biggest improvements for tm

1. **Stop looping over the same files; turn investigation into a patch.** Repeated reads/searches consumed the run without an edit on Click and Cobra, and tm's mean plan-quality score was just 1.20/5. After a few focused steps, summarize what is known and take one implementation step—or clearly say what is blocking it. (`pallets-click-3822.json`, `spf13-cobra-2257.json`)
2. **Control context and token use.** tm's median was 357,389 tokens versus OpenCode's 34,541, and tm scored 1.00/5 in context efficiency. Reuse findings, avoid rereading unchanged regions, and bound investigations. The Ky run reached 608,692 tokens while revisiting the same files. (`sindresorhus-ky-878.json`; aggregate scores in the five scored trials)
3. **Handle provider throttling with bounded backoff and a specific explanation.** Two tm runs encountered HTTP 429 and stopped without producing changes or running tests. Retry briefly when appropriate, honor any retry delay, and tell the user directly when a wait is needed rather than saying only “provider unavailable.” (`psf-requests-7432.json`, `BurntSushi-ripgrep-3376.json`)
4. **Check the workspace before declaring failure.** On Ky, a source diff was present and hidden stream tests passed, yet tm reported that no patch or evidence had been submitted. Before finalizing, inspect the diff and test evidence and distinguish “patch exists,” “verification passed,” and “ticket passed.” (`sindresorhus-ky-878.json`)
5. **Make recovery and next steps actionable.** tm's mean recoverability score was 1.60/5. When a run stalls or stops, explain whether any code changed, which checks ran and their results, and offer a useful next action; retry guidance should help continue from collected findings rather than simply repeat the same investigation. (`pallets-click-3822.json`, `spf13-cobra-2257.json`, `BurntSushi-ripgrep-3376.json`)
