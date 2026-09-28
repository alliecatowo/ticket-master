# Trial scoreboard

Four trials had results for both tools. The Hugo trial (`gohugoio-hugo-15360.json`) was skipped because there was no merged fix PR with tests, so it is excluded from all rates and averages. Scores are on the rubric's 1–5 scale. “Pass” is the recorded trial outcome; medians are across the four runs for each tool.

## Results by tool

| Tool | Pass rate | Mean navigation quality | Mean context efficiency | Mean plan quality | Mean verification honesty | Mean UX copy clarity | Mean recoverability | Mean cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 1/4 (25%) | 2.50 | 1.50 | 1.50 | 3.00 | 2.25 | 1.75 | 1.50 | 102.8 s | 430,494 |
| OpenCode | 3/4 (75%) | 4.00 | 3.75 | 4.50 | 4.25 | 3.75 | 4.25 | 4.75 | 106.4 s | 23,912 |

The largest gap is effort: tm's median reported token use was about 18 times OpenCode's, while its median wall time was similar. OpenCode passed three trials; both tools passed the ky trial, and both failed the Click trial. tm failed Requests and Cobra without submitting a patch.

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| Click #3822 (`pallets-click-3822.json`) | **Fail** · 127 s · 555,265 tokens · mean rubric score 1.71/5 | **Fail** · 105 s · 21,627 tokens · mean 3.86/5 | tm found the relevant source and typing tests but repeated reconnaissance and made no edit. OpenCode made an edit and passed the runtime tests, but left two typing errors and mismatched the expected inferred return type. |
| Requests #7432 (`psf-requests-7432.json`) | **Fail** · 91.5 s · 352,318 tokens · mean 1.43/5 | **Pass** · 107.8 s · 36,633 tokens · mean 3.86/5 | tm repeatedly inspected the same model file and escalated without a patch. OpenCode implemented the stream-check fix, recovered from a bad reproduction, and passed. |
| ky #878 (`sindresorhus-ky-878.json`) | **Pass** · 39.5 s · 183,478 tokens · mean 3.43/5 | **Pass** · 73.5 s · 20,225 tokens · mean 4.57/5 | Both passed the targeted four tests. tm made the focused fix but used much more context and left the completion/test result unclear. OpenCode reported its verification accurately; its review note says browser-level coverage could have been clearer. |
| Cobra #2257 (`spf13-cobra-2257.json`) | **Fail** · 114 s · 508,670 tokens · mean 1.43/5 | **Pass** · 130.4 s · 26,196 tokens · mean 4.57/5 | tm found the relevant files but repeated inspection across three attempts and submitted no change. OpenCode copied the aliased slice, added a regression test, recovered from a test-fixture issue, and passed focused and full Go tests. |

Mean rubric scores are the arithmetic mean of each run's seven dimensions. The scores capture more than pass/fail: tm's Click run, for example, was still judged more honestly and clearly than its other failed runs, despite not producing a patch.

## Five biggest things tm should improve

1. **Stop repeated investigation and turn findings into an edit.** In Click, tm reread source and typing tests across three attempts; in Requests it inspected `src/requests/models.py` three times; in Cobra it repeated inspections across all three attempts. In all three, it ended without a patch (`pallets-click-3822.json`, `psf-requests-7432.json`, `spf13-cobra-2257.json`). Carry forward what has already been learned and require a concrete edit or a useful, early escalation.
2. **Put a firm bound on retries and context spend.** The three no-patch failures used 352,318–555,265 tokens each, versus 21,627–36,633 for OpenCode on those same trials. Click also had a retry with working directory `/null` (`pallets-click-3822.json`). Stop retry loops when progress stalls, preserve a valid working directory, and cap cumulative spend (`pallets-click-3822.json`, `psf-requests-7432.json`, `spf13-cobra-2257.json`).
3. **Make recovery instructions clear and consistent with the run state.** Cobra told the user to resume while retries were being scheduled, then ended with a ticket-retry instruction; Requests escalated with a generic manual-resume suggestion. Give one actionable next step, say whether a patch exists, and make that instruction agree with the ticket/run state (`spf13-cobra-2257.json`, `psf-requests-7432.json`).
4. **Make completion reports explicit about verification and outcome.** On ky, the targeted tests passed, but tm's terse activity output did not clearly summarize test results or distinguish submission from acceptance. On Requests, the user needed to know directly that no fix or verification was completed (`sindresorhus-ky-878.json`, `psf-requests-7432.json`). End with a concise statement of what changed, what tests ran and their result, and whether work was submitted or accepted.
5. **Spend effort on broad enough checks and a smaller, relevant context.** OpenCode's Click patch still had two typing failures and a return-type mismatch despite passing runtime tests; the trial's hidden checks exposed that gap. On ky, browser-level behavior was an additional useful check. Use focused tests plus the affected typing/runtime or browser contract where relevant, while avoiding repeated reads that do not advance the fix (`pallets-click-3822.json`, `sindresorhus-ky-878.json`).
