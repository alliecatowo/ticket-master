# Trial scoreboard

## Overall results

Scores are average rubric ratings on a 1–5 scale. Pass rate is the trial result in the JSON. Times and tokens are medians across the five comparable trials. Lower time and token counts are better.

| Tool | Pass rate | Navigation quality | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time (s) | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 0/5 (0%) | 2.4 | 1.2 | 1.4 | 2.6 | 2.0 | 1.6 | 1.2 | 117.05 | 348,974 |
| OpenCode | 3/5 (60%) | 4.0 | 3.4 | 4.4 | 4.8 | 4.2 | 4.0 | 4.6 | 68.00 | 23,960 |

The one skipped file, `BurntSushi-ripgrep-3376.json`, is excluded: it says there is no merged PR with tests to evaluate, so it has no paired tool results. The five scored trials are the ones below.

## tm vs OpenCode, trial by trial

Time and token differences are tm minus OpenCode; negative means tm used less. “Pass” is the recorded trial outcome.

| Trial | tm pass | OpenCode pass | tm time (s) | OpenCode time (s) | Time difference | tm tokens | OpenCode tokens | Token difference |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `psf-requests-7432` | No | Yes | 117.05 | 82.08 | +34.97 | 169,075 | 299,243 | -130,168 |
| `pallets-click-3822` | No | Yes | 118.06 | 85.23 | +32.83 | 348,974 | 22,464 | +326,510 |
| `sindresorhus-ky-878` | No | No | 67.01 | 63.25 | +3.76 | 47,672 | 222,988 | -175,316 |
| `spf13-cobra-2257` | No | Yes | 31.00 | 62.00 | -31.00 | 389,211 | 23,960 | +365,251 |
| `gohugoio-hugo-15360` | No | No | 174.00 | 68.00 | +106.00 | 364,054 | 23,215 | +340,839 |

The two recorded failures for both tools were not the same kind of outcome: in the Hugo trial the independent hidden-test overlay could not compile against the pinned base; in the Ky trial the hidden regression checks passed but the full browser suite could not run because Firefox and WebKit were unavailable. See the trial notes for each tool in the source JSONs.

## Five biggest things tm should improve

1. **Make provider-rate-limit recovery reliable and visible.** Rate limits stopped work in the Click, Cobra, and Hugo trials; Ky also stopped on HTTP 429. Use a bounded wait/backoff when feasible, say whether an attempt is waiting or restarting, and retain the investigation so retrying does not mean starting over. The repeated dispatch notices and ineffective retries were especially confusing in Click. (`pallets-click-3822.json`, `spf13-cobra-2257.json`, `gohugoio-hugo-15360.json`, `sindresorhus-ky-878.json`)
2. **Stop repeated exploration and control context use.** Re-reading the same implementation, tests, and search results drove very high token use without forward progress in Click, Cobra, and Hugo. Once the relevant function and test are identified, carry those findings forward and move to an edit or a focused verification step. (`pallets-click-3822.json`, `spf13-cobra-2257.json`, `gohugoio-hugo-15360.json`)
3. **Turn navigation into a concrete, scoped implementation plan.** Several runs reached a relevant location but did not produce a patch. State the likely change and regression test early, then execute those steps; Requests in particular found `prepare_body` but stopped without evidence or a repository change. (`psf-requests-7432.json`, `gohugoio-hugo-15360.json`, `pallets-click-3822.json`)
4. **Finish with meaningful verification, or clearly state why it is absent.** tm did not run tests in Requests, Ky, Click, or Hugo, and the hidden test failed for Cobra. Report exactly what changed, what command ran, and what remains unverified; after transient failures, resume at the targeted test rather than ending with an unverified attempt. (`psf-requests-7432.json`, `sindresorhus-ky-878.json`, `pallets-click-3822.json`, `gohugoio-hugo-15360.json`, `spf13-cobra-2257.json`)
5. **Make failure summaries and recovery instructions specific and actionable.** Say plainly whether there is a patch, whether tests ran, what the provider failure means, and the single correct next action. The Requests guidance pointed to a session resume even though the log called for ticket retry; Hugo and Click did not summarize the no-change/no-test state clearly. (`psf-requests-7432.json`, `gohugoio-hugo-15360.json`, `pallets-click-3822.json`)

## Notes on reading these results

This is a small set of five paired trials. “Pass” is the recorded outcome, not a claim that every environment-level test failure reflects the tool's code. The dimension means are simple averages of each tool's five JSON ratings.
