# Trial scoreboard

Results from the six top-level trial JSON files in this directory. Pass rate is the recorded `passed` flag. Rubric scores are averages on the 1–5 scale. Wall time is in seconds; tokens are the reported input-token count. Lower time and token counts are better.

## tm

| Measure | Result |
|---|---:|
| Pass rate | 3/6 (50%) |
| Mean navigation quality | 3.17 / 5 |
| Mean context efficiency | 1.50 / 5 |
| Mean plan quality | 2.00 / 5 |
| Mean verification honesty | 3.17 / 5 |
| Mean UX copy clarity | 1.83 / 5 |
| Mean recoverability | 2.00 / 5 |
| Mean cost/time score | 2.50 / 5 |
| Median wall time | 311 seconds (5m 11s) |
| Median reported tokens | 1,261,110 |

## OpenCode

| Measure | Result |
|---|---:|
| Pass rate | 6/6 (100%) |
| Mean navigation quality | 4.17 / 5 |
| Mean context efficiency | 3.83 / 5 |
| Mean plan quality | 4.67 / 5 |
| Mean verification honesty | 4.17 / 5 |
| Mean UX copy clarity | 4.00 / 5 |
| Mean recoverability | 3.67 / 5 |
| Mean cost/time score | 4.50 / 5 |
| Median wall time | 133.4 seconds (2m 13s) |
| Median reported tokens | 27,993 |

## tm vs OpenCode, trial by trial

Total rubric score is the sum of the seven dimensions (maximum 35). “Score gap” is tm minus OpenCode, so a negative number favors OpenCode. OpenCode passed all six recorded trials; tm passed three.

| Trial | tm result / score | OpenCode result / score | Score gap | tm time vs OpenCode | tm tokens vs OpenCode |
|---|---:|---:|---:|---:|---:|
| `spf13-cobra-2257` — Cobra issue 2257 | Fail / 14 | Pass / 29 | -15 | 3s vs 207s | 0 vs 27,879 |
| `BurntSushi-ripgrep-3376` — multi-root `.gitignore` | Pass / 17 | Pass / 28 | -11 | 344s vs 132s | 1,777,863 vs 28,543 |
| `psf-requests-7432` — delegated stream detection | Fail / 12 | Pass / 31 | -19 | 493s vs 56s | 2,886,820 vs 28,107 |
| `sindresorhus-ky-878` — empty response body | Pass / 22 | Pass / 27 | -5 | 32s vs 134.7s | 327,398 vs 35,263 |
| `pallets-click-3822` — generic `click.Path` | Fail / 12 | Pass / 29 | -17 | 278s vs 88s | 744,357 vs 23,225 |
| `gohugoio-hugo-15360` — UTF-8 BOM handling | Pass / 20 | Pass / 30 | -10 | 871.2s vs 171.4s | 5,431,756 vs 23,664 |

The Cobra trial stopped before navigation: tm's configured `devpass` provider was unavailable. Its zero tokens and three-second duration reflect that early failure, not an especially efficient solution. In ripgrep, both arms' recorded pass flags are true, but neither made an implementation change; the issue-specific regression itself was not verified. In Hugo, focused issue tests passed in both arms; some merged tests could not compile against the pinned base because they depended on production APIs from the merge. (Source: `spf13-cobra-2257.json`, `BurntSushi-ripgrep-3376.json`, and `gohugoio-hugo-15360.json`.)

## Five biggest things tm should improve

1. **Make agent/provider startup dependable, and explain configuration failures.** The Cobra trial never reached code work because `devpass` was not registered. The suggested retry did not explain how to fix the configuration. Check readiness before dispatch and give a specific repair step when startup fails (`spf13-cobra-2257`).
2. **Control token use and repeated exploration.** Across these trials tm's median was 1.26 million reported tokens versus OpenCode's 27,993. The Requests run spent 2.89 million tokens and 493 seconds; the Hugo run spent 5.43 million tokens and 871 seconds. Avoid re-reading the same code, broad historical dives, and redundant verification; add useful time/token guardrails (`psf-requests-7432`, `gohugoio-hugo-15360`).
3. **Finish with a clear, actionable status when a run stops or errors.** Several tm runs ended without a submitted change and offered vague or unexplained retry advice. Say plainly whether a patch was submitted, what verification ran, whether a retry is automatic or required, and what the user can do next (`pallets-click-3822`, `BurntSushi-ripgrep-3376`, `psf-requests-7432`).
4. **Make progress and submission errors understandable, including what happened after retries.** In the Ky and Hugo runs, progress displayed internal errors and later success without clarifying whether the operation was retried or duplicated. Name the failed action and state the final outcome and retry status in ordinary language (`sindresorhus-ky-878`, `gohugoio-hugo-15360`).
5. **Use a concise plan and verify the actual reported behavior with a targeted test.** The tm rubric averaged 2.00/5 for plan quality. In ripgrep, investigation did not lead to the issue-specific multi-root regression check; in Ky, the fix landed but tm did not add its own regression test. Start with the smallest reproducer, implement, and run the most direct relevant test (`BurntSushi-ripgrep-3376`, `sindresorhus-ky-878`).

## How to read this

The scoreboard summarizes only the six top-level `*.json` trial result files. Tool pass flags are used as recorded, not independently reinterpreted; the trial notes describe important limits on what was actually implemented or verified. Token values are the counts recorded by each trial and may not represent directly comparable accounting between tools.
