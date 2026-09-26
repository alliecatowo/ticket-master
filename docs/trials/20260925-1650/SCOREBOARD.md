# Trial scoreboard

This summary uses the five JSON results that contain both a `tm` and an `opencode` run. The ripgrep result (`BurntSushi-ripgrep-3376.json`) was skipped because it says there is no merged fixing PR with tests, so it has no comparable tool scores. Scores are on the rubric's 1–5 scale. Times are seconds; token counts are those recorded in the trial files.

## Overall by tool

| Tool | Pass rate | Navigation quality (mean) | Context efficiency (mean) | Plan quality (mean) | Verification honesty (mean) | UX copy clarity (mean) | Recoverability (mean) | Cost/time (mean) | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 3/5 (60%) | 2.0 | 1.0 | 1.6 | 2.8 | 2.0 | 1.2 | 1.0 | 509.06 s | 1,696,094 |
| OpenCode | 5/5 (100%) | 4.2 | 4.0 | 4.6 | 4.6 | 4.0 | 4.0 | 5.0 | 66.57 s | 23,274 |

Pass rate is calculated from each result's `passed` field. One caution: the Hugo result is marked passed for tm, but its reviewer note says tm ended without submitting a patch; the passing prescribed command ran independently and did not test the reported regression. Treat that recorded pass as a caveat, not evidence that tm fixed the issue.

## tm vs OpenCode in each trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| `pallets-click-3822` — Click path typing | **Fail**, 206.4 s, 1,139,896 tokens | **Pass**, 118.3 s, 22,724 tokens | tm found the implementation area but repeatedly revisited the same file and submitted no patch. OpenCode implemented the type propagation and validated it with type checks and focused tests. |
| `gohugoio-hugo-15360` — Hugo BOM handling | **Recorded pass†**, 509.1 s, 1,696,094 tokens | **Pass**, 66.6 s, 24,343 tokens | tm spent a long time rereading/searching, then stopped without a patch. OpenCode added targeted decoder cases and ran package tests, while noting that the test coverage did not fully target the regression. |
| `psf-requests-7432` — Requests stream detection | **Recorded pass**, 545.4 s, 6,026,090 tokens | **Pass**, 36.3 s, 19,191 tokens | tm reached the right condition but hit the 64-step ceiling without submitting. OpenCode made the compatibility fix and focused test, but did not find the project's prepared virtual environment for verification. |
| `sindresorhus-ky-878` — ky stream completion | **Fail**, 4.0 s, 0 tokens | **Pass**, 171.0 s, 28,375 tokens | tm could not start because its configured provider was unavailable and offered an ineffective retry. OpenCode implemented the completion event, added a regression test, and recovered from a hanging test. |
| `spf13-cobra-2257` — Cobra completion args | **Pass**, 1,112.0 s, 5,216,210 tokens | **Pass**, 64.9 s, 23,274 tokens | Both passed. tm eventually fixed it, but repeated reads and test-edit churn made the run much slower. OpenCode made a narrow fix and ran the full Go test suite; its test could have matched the reported end-to-end behavior more closely. |

† See the Hugo pass-rate caveat above. “Recorded pass” preserves the JSON field as supplied.

## Five biggest improvements for tm

1. **Stop rereading the same context; keep token use under control.** tm's median was 1.70 million tokens versus OpenCode's 23,274. Review notes repeatedly call out duplicate file reads and broad repository searches. In Requests, tm used 6.03 million tokens and reread implementation/test files; in Cobra it used 5.22 million while revisiting the same files and reworking tests. (Trials: `psf-requests-7432`, `spf13-cobra-2257`, `gohugoio-hugo-15360`.)
2. **Prevent long runs from ending without a submitted result.** Click ended after about 206 seconds without a patch; Hugo after about 509 seconds; Requests hit its 64-step limit after about 545 seconds. Show remaining limits, save/resume progress, and make submission robust so useful work is not lost at the end of a run. (Trials: `pallets-click-3822`, `gohugoio-hugo-15360`, `psf-requests-7432`.)
3. **Check provider readiness before starting, and give a real recovery step.** The ky run failed before the agent began because `coder.fast` resolved to an unavailable, unregistered `devpass` provider despite the environment file being sourced. The suggested retry could not fix that configuration. Validate provider/credential availability up front and name the concrete config repair. (Trial: `sindresorhus-ky-878`.)
4. **Make verification target the reported bug and report exactly what passed.** The Hugo run's prescribed command passed but did not exercise the BOM regression, and no patch was submitted. Requests also spent time probing Python environments instead of selecting the project venv. Check the precise regression, use the repository's intended environment, and clearly separate agent-run results from independent test outcomes. (Trials: `gohugoio-hugo-15360`, `psf-requests-7432`.)
5. **Give a clear plan, useful progress, and actionable failure messages.** Cobra's low-level progress and vague “unexpected internal problem” messages obscured what failed and how to recover; Click's final response did not clearly state that no changes or verification were produced or disclose the usage. Summarize the plan, communicate meaningful milestones, and end with a concise result, verification status, usage, and next step. (Trials: `spf13-cobra-2257`, `pallets-click-3822`.)
