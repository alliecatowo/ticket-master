# Trial scoreboard

Plain-English summary of the available trial result files. Scores are on the rubric's 1–5 scale. “Pass” is the trial's recorded pass/fail result; time and token figures are medians across the five comparable trials. Lower time and token use are better.

## Results by tool

| Tool | Pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 0/5 (0%) | 2.4 | 1.2 | 1.4 | 2.8 | 2.0 | 1.6 | 1.8 | 122 sec | 488,329 |
| OpenCode | 3/5 (60%) | 4.4 | 4.0 | 4.6 | 4.0 | 4.4 | 4.0 | 4.8 | 99.7 sec | 20,859 |

The mean scores above average each rubric dimension over the five trials. OpenCode passed ky, requests, and ripgrep; it did not pass cobra or click. tm did not pass any of the five. In these runs, tm's median token use was about 23 times OpenCode's.

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| ky #878 | Fail; 185 sec; 199,182 tokens | Fail; 97 sec; 21,784 tokens | Both passed the hidden stream tests (19/19), while the full command hit the same existing lint error and browser tests could not run because Playwright browsers were missing. tm found and edited the right code, but used far more tokens and had trouble with ticket ID output. OpenCode gave a more focused run. |
| requests #7432 | Fail; 122 sec; 449,461 tokens | Pass; 37 sec; 19,729 tokens | tm repeatedly inspected the relevant code, retried, and stopped without a patch. OpenCode made a focused fix and regression test; its own verification attempts had environment issues, but the independently applied PR test passed. |
| ripgrep #3376 | Fail; 139.9 sec; 599,006 tokens | Pass; 112.1 sec; 37,914 tokens | tm found the relevant matcher but repeated exploration over three attempts without a patch. OpenCode made the fix, added a regression test, and passed crate tests. Its final test-count summary was slightly inaccurate. |
| cobra #2257 | Fail; 98.4 sec; 488,329 tokens | Pass; 126.6 sec; 20,146 tokens | tm repeatedly inspected the same files and never made a patch. OpenCode made a targeted fix and `go test ./...` passed. |
| click #3822 | Fail; 111.7 sec; 450,110 tokens | Fail; 99.7 sec; 20,859 tokens | tm made no edit after repeated reads. OpenCode implemented the typing change and its checks passed, but restored PR tests found the default-path typing case was still incomplete. |

## Five biggest improvements for tm

1. **Stop repeating the same investigation; turn findings into an edit.** In requests, tm found the relevant stream-detection code but reread it over repeated attempts without patching (requests #7432). The same pattern ended without a patch in ripgrep #3376, cobra #2257, and click #3822. Detect repeated file/range inspections and make the next step a concrete, scoped change or one targeted question.
2. **Make retries preserve progress and say clearly what happens next.** In ripgrep, the agent suggested `tm --resume S-3`, while the final failure message said `tm ticket retry T-1`; cobra and click also gave resume guidance while attempts were being retried or escalated. Explain whether a retry resumes the existing work or starts a fresh attempt, and carry useful findings forward (ripgrep #3376; cobra #2257; click #3822).
3. **Cut token use sharply.** tm used 199,182–599,006 tokens per trial, versus 19,729–37,914 for OpenCode. Repeated reads and retries were especially costly in requests, ripgrep, cobra, and click. Set a smaller context budget for repeated exploration and summarize findings instead of reloading them (requests #7432; ripgrep #3376; cobra #2257; click #3822).
4. **Give a short plan and a clear final verification summary.** tm's plans scored poorly across most trials. On ky, it found the right path and the hidden tests passed, but the final account should clearly separate that result from the failing full test command and unavailable browser checks. State the next concrete action early and report each check as passed, failed, or unavailable (ky #878; requests #7432).
5. **Make ticket and status output easy to act on.** In ky, `tm ticket new` printed a descriptive sentence instead of a bare ID, and that output caused `tm run` to fail until the ID was manually extracted. Across several failed runs, retry/status messages also made the next action unclear. Provide a stable machine-readable ticket ID and make status and recovery instructions consistent (ky #878; ripgrep #3376; cobra #2257; click #3822).

## Scope note

The Hugo result was skipped: its JSON says there was no merged fix PR with regression tests, so it is not a scored tool comparison. The click result is in the nested `pallets-click-3822/pallets-click-3822.json` file. No other JSON files in the trial directory were result records.
