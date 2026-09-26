# Trial scoreboard

This scoreboard covers the six top-level trial result files in this directory. Rubric scores are on the scale used in the result files (higher is better); dimension means are averaged across all six trials. “Pass” is the recorded `passed` field, including cases where a full suite was blocked by the environment.

## Overall results

| Tool | Pass rate | Mean navigation | Mean context efficiency | Mean plan quality | Mean verification honesty | Mean UX clarity | Mean recoverability | Mean cost/time | Overall rubric mean | Median wall time | Median reported tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 3/6 (50%) | 2.50 | 1.17 | 1.83 | 2.17 | 2.00 | 2.33 | 1.50 | 1.93 | 650.36 sec (10m 50s) | 1,011,870 |
| OpenCode | 5/6 (83%) | 4.33 | 4.00 | 4.67 | 4.50 | 4.17 | 4.00 | 4.67 | 4.33 | 100.11 sec (1m 40s) | 40,304 |

Median time and token counts are medians of the six recorded runs per tool. Token figures are the trial JSON’s reported totals; at least one trial flags a discrepancy between tm’s reported statistics and event-level usage, so treat those totals as approximate until reconciled (see Cobra below).

## tm vs OpenCode, trial by trial

“Mean score” is the average across that run’s seven rubric dimensions. Times and tokens are the recorded values.

| Trial | tm: pass, mean score, time, tokens | OpenCode: pass, mean score, time, tokens | What stands out |
|---|---|---|---|
| BurntSushi ripgrep #3376 (`BurntSushi-ripgrep-3376.json`) | Pass, 2.00, 99 sec, 332,740 | Pass, 4.14, 242 sec, 54,261 | tm was faster but stopped on a toolchain dependency error despite Cargo being present; OpenCode completed and reported its checks. |
| gohugoio Hugo #15360 (`gohugoio-hugo-15360.json`) | Pass, 1.43, 1,200 sec, 97,269 | Pass, 4.71, 63 sec, 223,827 | tm used fewer reported tokens but spent the full 20-minute session wandering without a clear plan; OpenCode made a focused fix and verified it. |
| pallets Click #3822 (`pallets-click-3822.json`) | Fail, 1.29, 669 sec, 1,691,000 | Pass, 4.29, 141 sec, 26,347 | tm repeatedly revisited the same source and did not submit a patch; OpenCode implemented and checked the change. |
| psf Requests #7432 (`psf-requests-7432.json`) | Fail, 1.57, 1,107 sec, 3,114,837 | Pass, 4.14, 64 sec, 22,665 | tm found the relevant code but looped on repeated reads and ended without a patch; OpenCode completed the change. |
| sindresorhus ky #878 (`sindresorhus-ky-878.json`) | Fail, 2.57, 54 sec, 277,270 | Fail, 4.43, 45 sec, 20,926 | Both are marked fail because the full verification suite could not run cleanly in this environment. Both passed the directly runnable stream and Chromium regression checks according to the verification notes. |
| spf13 Cobra #2257 (`spf13-cobra-2257.json`) | Pass, 2.71, 632 sec, 2,378,178 | Pass, 4.29, 136 sec, 246,471 | tm found the fix and recovered from a bad repro path, but repeated reads and an unclear “submitted” status hurt efficiency and clarity. OpenCode completed the fix and tests. |

## Five biggest improvements for tm

1. **Stop repeated reads and searches before they become a loop.** Cache or summarize what has already been inspected, detect repeated source ranges, and redirect to the next concrete action. This pattern dominates the Click, Requests, Cobra, and Hugo feedback: Click reread overlapping ranges without retaining findings, Requests repeated reads and produced no patch, Cobra reread the same files, and Hugo spent its session on repeated, unfocused exploration. (`pallets-click-3822.json`, `psf-requests-7432.json`, `spf13-cobra-2257.json`, `gohugoio-hugo-15360.json`)

2. **Set a short, visible plan and keep progress reports useful.** State the files or behavior being investigated, the intended edit, and how it will be verified; then provide concise progress rather than a dense stream of low-level tool activity. The missing scope was called out directly in Click, Hugo, and ky. (`pallets-click-3822.json`, `gohugoio-hugo-15360.json`, `sindresorhus-ky-878.json`)

3. **Control and accurately report resource use.** Add visible time/token budgets and reconcile the counters used in final statistics. tm’s recorded median is over ten minutes and about one million tokens, while Cobra’s feedback specifically contrasts 2,378,178 reported tokens with 153,045 tokens in captured usage events. Requests reports 3,114,837 tokens and over 18 minutes for a run that ended without a patch. (`spf13-cobra-2257.json`, `psf-requests-7432.json`; overall medians from all six files)

4. **Make failure and recovery status explicit, with a usable next step.** Distinguish “attempt failed,” “recovered and submitted,” “submitted for review,” and “completed”; surface retries immediately and give a direct resume command when work stops. Click ended without a summary or recovery guidance, Hugo hid a failed attempt in ticket metadata and had confusing ticket-ID handling, and ky showed a submission error before later succeeding. (`pallets-click-3822.json`, `gohugoio-hugo-15360.json`, `sindresorhus-ky-878.json`, `spf13-cobra-2257.json`)

5. **Improve command-environment diagnostics and final verification summaries.** Before declaring a tool unavailable, confirm what the shell can actually resolve and distinguish executable absence from environment/path resolution problems. Then clearly state which checks passed or were blocked. Ripgrep stopped claiming Cargo was unavailable even though it existed; ky’s intermediate submission error obscured later recovery; and Cobra’s final wording could be mistaken for completion when review was still required. (`BurntSushi-ripgrep-3376.json`, `sindresorhus-ky-878.json`, `spf13-cobra-2257.json`)

## Reading the results

The pass rate is the trial’s recorded outcome, not a claim that every failing run had an incorrect code change. In particular, ky records both tools as failing because the full merged verification suite was blocked by unavailable Playwright browsers and a pnpm build-policy error; its notes say both tools passed the runnable stream tests and targeted Chromium regression. The trial files also note special post-run verification caveats for ripgrep, so its recorded pass should not be treated as independent proof of the implementation. (`sindresorhus-ky-878.json`, `BurntSushi-ripgrep-3376.json`)
