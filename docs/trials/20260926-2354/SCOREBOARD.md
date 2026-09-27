# Trial scoreboard

Five trial-result files were scored: Click #3822, ripgrep #3376, Cobra #2257, Requests #7432, and ky #878. Scores are means on the rubric's 1–5 scale. Pass rate uses each result's `passed` field. Time and token figures are medians; tokens are the reported run totals.

## Ticket Master (tm)

| Measure | Result |
|---|---:|
| Pass rate | 1/5 (20%) |
| Mean navigation quality | 2.2/5 |
| Mean context efficiency | 1.0/5 |
| Mean plan quality | 1.2/5 |
| Mean verification honesty | 2.8/5 |
| Mean UX copy clarity | 1.8/5 |
| Mean recoverability | 1.6/5 |
| Mean cost/time | 1.4/5 |
| Median wall time | 162 seconds |
| Median tokens | 537,397 |

## OpenCode

| Measure | Result |
|---|---:|
| Pass rate | 2/5 (40%) |
| Mean navigation quality | 3.6/5 |
| Mean context efficiency | 3.6/5 |
| Mean plan quality | 4.2/5 |
| Mean verification honesty | 3.8/5 |
| Mean UX copy clarity | 4.0/5 |
| Mean recoverability | 4.0/5 |
| Mean cost/time | 5.0/5 |
| Median wall time | 104 seconds |
| Median tokens | 22,131 |

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| Click #3822 | Failed; 224s; 596,292 tokens | Failed; 146s; 25,267 tokens | tm found relevant files but repeated investigation without a code change. OpenCode made a focused typing change and ran mypy and pyright, but missed the prescribed hidden typing file. [pallets-click-3822.json] |
| ripgrep #3376 | Passed*; 162s; 235,757 tokens | Passed*; 104s; 34,140 tokens | Both had a passing mechanical test, but checking out the prescribed test file also brought in the upstream production fix, so that pass does not establish either tool independently solved it. OpenCode made a focused change and regression test; tm did not edit. [BurntSushi-ripgrep-3376.json] |
| Cobra #2257 | Failed; 152s; 537,397 tokens | Passed; 83s; 20,860 tokens | tm repeated searches and made no patch. OpenCode fixed the argument handling, added a regression test, and passed `go test ./...`. [spf13-cobra-2257.json] |
| Requests #7432 | Failed; 112s; 354,474 tokens | Failed; 100s; 22,131 tokens | tm found the right file but did not produce a patch. OpenCode had a sound plan but edited the sibling checkout and could not verify its edit; it disclosed that limitation. [psf-requests-7432.json] |
| ky #878 | Failed; 303s; 675,280 tokens | Failed; 107s; 20,768 tokens | tm ultimately made the correct one-line change, but only after repeated attempts. Both reported passing stream tests and browser tests blocked by missing browsers; the full test command also hit an existing lint error. OpenCode added a focused regression test. [sindresorhus-ky-878.json] |

*The ripgrep pass is recorded as passed in the result, but the evaluator notes that the test-file checkout supplied the upstream implementation, so interpret it cautiously.

## Five biggest improvements for tm

1. **Break out of repeated investigation and start making a focused change.** Across Click, ripgrep, Cobra, and Requests, tm repeatedly reread overlapping files or repeated the same searches without producing a patch. In ky it took two full attempts before editing. A no-progress check should trigger a concrete next step, not another near-identical attempt. [pallets-click-3822.json; BurntSushi-ripgrep-3376.json; spf13-cobra-2257.json; psf-requests-7432.json; sindresorhus-ky-878.json]
2. **Put a firm ceiling on context and token use.** tm's median was 537,397 tokens versus OpenCode's 22,131, and tm used 675,280 tokens on ky alone. Stop, summarize what is known, and continue with only the relevant context when a run grows without progress. [all five trial files]
3. **Make retries change strategy.** The automatic attempts often repeated the same investigation instead of carrying forward a specific action. After the first stall, a retry should name the next file, intended edit, or test to try. [BurntSushi-ripgrep-3376.json; spf13-cobra-2257.json; psf-requests-7432.json; sindresorhus-ky-878.json]
4. **Give one clear, consistent recovery instruction.** Several runs told users to resume a session and also to retry a ticket, leaving it unclear which to do or whether another attempt was already underway. The final message should state the ticket/session status and one actionable command. [pallets-click-3822.json; BurntSushi-ripgrep-3376.json; spf13-cobra-2257.json; psf-requests-7432.json; sindresorhus-ky-878.json]
5. **Explain verification and tool errors plainly.** In ripgrep, the test pass was not independent evidence because the checkout included the upstream implementation. In ky, shell-start and submission errors appeared before recovery with little explanation. Say exactly what was tested, what the result proves, what was blocked, and what the user can do next. [BurntSushi-ripgrep-3376.json; sindresorhus-ky-878.json]
