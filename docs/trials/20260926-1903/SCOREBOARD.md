# Trial scoreboard

There were five scored trials. The ripgrep trial (`BurntSushi-ripgrep-3376.json`) was skipped because no merged fix with tests was available, so it is excluded from the rates and averages below. Scores are on the rubric's 1–5 scale. “Pass” uses each tool's recorded `passed` result; the ky trial is marked failed for both because the full browser command could not run on Firefox/WebKit, even though OpenCode's hidden stream tests passed.

## tm

| Measure | Result |
| --- | ---: |
| Pass rate | 1/5 (20%) |
| Mean navigation quality | 3.0 |
| Mean context efficiency | 2.8 |
| Mean plan quality | 1.6 |
| Mean verification honesty | 3.2 |
| Mean UX copy clarity | 2.0 |
| Mean recoverability | 1.8 |
| Mean cost/time score | 1.8 |
| Median wall time | 71.7 seconds |
| Median tokens | 30,037 |

## OpenCode

| Measure | Result |
| --- | ---: |
| Pass rate | 4/5 (80%) |
| Mean navigation quality | 4.2 |
| Mean context efficiency | 3.4 |
| Mean plan quality | 4.6 |
| Mean verification honesty | 4.6 |
| Mean UX copy clarity | 4.0 |
| Mean recoverability | 4.0 |
| Mean cost/time score | 4.4 |
| Median wall time | 71.3 seconds |
| Median tokens | 36,700 |

## tm vs OpenCode by trial

| Trial | tm | OpenCode | What happened |
| --- | --- | --- | --- |
| Hugo #15360 | Pass; 96s; 7,243 tokens | Pass; 56s; 23,397 tokens | tm spent 67 seconds indexing, then hit a provider 429 before implementing. OpenCode made a focused BOM fix and passed package tests. |
| Click #3822 | Fail; 71.7s; 197,839 tokens | Pass; 83.5s; 291,688 tokens | tm hit repeated provider 429s and made no fix. OpenCode updated the type and verified type narrowing and pytest. |
| ky #878 | Fail; 13.6s; 7,157 tokens | Fail; 71.3s; 23,206 tokens | tm's hidden stream cases failed after a provider 429 prevented work. OpenCode's hidden stream tests passed, but the overall command failed because Firefox/WebKit were unavailable. |
| Requests #7432 | Fail; 34.6s; 30,037 tokens | Pass; 84.6s; 36,700 tokens | tm stopped on provider 429s without a fix. OpenCode corrected stream detection and passed the focused regression test; broader tests had external fixture failures. |
| Cobra #2257 | Fail; 149s; 108,438 tokens | Pass; 55.2s; 44,707 tokens | tm had provider 429s and did not fix the bug. OpenCode made a defensive-copy fix and reported suite results accurately. |

## Five biggest improvements for tm

1. **Handle provider rate limits instead of abandoning the work.** 429 errors prevented fixes in Click, Requests, and Cobra; in Hugo and ky they stopped the run before implementation too. Use bounded backoff, respect a provider retry delay when supplied, and keep the ticket ready to resume. (Click #3822, Requests #7432, Cobra #2257, Hugo #15360, ky #878.)
2. **Make retry instructions match what the scheduler is doing.** Runs said to rerun `tm run T-1` while events recorded `ticket.retry_scheduled`, leaving it unclear whether an automatic retry was already queued. State whether it will retry, when, and the single best next action; avoid encouraging an immediate duplicate request. (Hugo #15360, ky #878, Cobra #2257.)
3. **Make reported time reflect actual elapsed time, and separate setup from work.** Hugo's summary reported one second despite a 96-second run and 67 seconds spent indexing. The ky run also reported one second in stats despite taking 13.56 seconds. Show setup/indexing and execution time separately, with totals that agree with observed wall time. (Hugo #15360, ky #878.)
4. **Improve the run plan and user-facing updates.** tm's mean plan score was 1.6/5 and UX clarity was 2.0/5; several runs stopped with little explanation beyond a provider error. Give a short plan, explain what blocked progress and whether code/tests ran, and end with a clear result. (Click #3822, Requests #7432, Cobra #2257; contrast with OpenCode's scoped plans in Click #3822 and ky #878.)
5. **Spend fewer tokens when a run cannot make progress.** Click used 197,839 tokens and Cobra 108,438 without producing a fix; Requests also spent 30,037 tokens across attempts that stopped at provider limits. Detect repeated rate limits early, stop unproductive retries, and preserve useful context for a later resume. (Click #3822, Cobra #2257, Requests #7432.)
