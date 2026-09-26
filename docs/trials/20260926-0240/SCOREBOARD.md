# Trial scoreboard

This summarizes the six trial result files in this folder (six runs per tool). Scores are rubric ratings from 1 (poor) to 5 (strong); means are rounded to two decimals. “Passed” is the trial's recorded mechanical pass flag, not necessarily proof that the agent's own patch passed: see the notes below. Wall time is in seconds and token counts are reported as given by the trials.

## Results by tool

| Tool | Pass rate | Navigation quality | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time (s) | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 3/6 (50%) | 2.67 | 1.17 | 1.83 | 2.17 | 1.83 | 1.50 | 1.33 | 510.0 | 2,213,027 |
| OpenCode | 4/6 (67%) | 4.00 | 4.00 | 4.33 | 4.67 | 4.17 | 4.00 | 4.83 | 140.1 | 30,213 |

The token median for tm includes the Hugo run's reported zero tokens. OpenCode had a higher recorded pass rate and higher mean score in every rubric dimension in this set. These are six benchmark runs, not a broad estimate of all usage.

## tm vs OpenCode, trial by trial

Overall score is the mean of the seven rubric dimensions for that run. Times and tokens are shown as `seconds / tokens`.

| Trial | tm: pass; mean score; time / tokens | OpenCode: pass; mean score; time / tokens |
|---|---|---|
| BurntSushi/ripgrep #3376 | Yes; 1.57; 1,163.2 / 10,865,871 | Yes; 4.00; 220.1 / 34,810 |
| gohugoio/hugo #15360 | Yes*; 1.00; 1.4 / 0 | Yes; 4.43; 233.9 / 21,837 |
| pallets/click #3822 | No; 1.86; 242 / 602,712 | Yes; 4.14; 254 / 24,791 |
| psf/requests #7432 | No; 1.71; 778 / 3,542,417 | No; 4.29; 43 / 35,965 |
| sindresorhus/ky #878 | No; 2.57; 136 / 883,636 | No; 4.29; 60 / 25,616 |
| spf13/cobra #2257 | Yes*; 2.00; 959 / 4,064,274 | Yes; 4.57; 59 / 274,945 |

`*` A recorded pass has an important qualification: in Hugo, the run never reached implementation because another worker had leased the ticket, and the exact regression test was not among the restored tests. In ripgrep, the tested file was restored from the fix PR after the run, so the test could not establish that tm's own code passed. Thus the raw 3/6 tm pass rate overstates what those runs demonstrate about tm's implementation. The ripgrep pass has the same file-restoration limitation for OpenCode. In requests and ky, neither tool passed the recorded check.

## Five biggest improvements for tm

1. **Use far less repeated context.** Several runs reread the same large files or repeated broad searches. tm's median was about 2.21 million tokens versus OpenCode's 30,213; ripgrep reached 10.87 million tokens. Deduplicate reads, keep compact notes from exploration, and focus on the relevant code and tests. (ripgrep #3376, cobra #2257, requests #7432, ky #878)

2. **Make runs bounded and recoverable.** Runs exhausted step limits, stalled, or ended without a patch, while retry guidance was unclear or simply said to rerun. Preserve investigation state, show remaining time/steps and retry status, and offer a specific resume action with a useful summary. (ripgrep #3376, cobra #2257, requests #7432, click #3822)

3. **Show a short plan and clear progress.** Some runs had no scoped plan, making it hard to tell what tm intended to change or verify. State the target, implementation step, and verification plan early, then report progress in plain language. (click #3822, requests #7432, ky #878)

4. **Make ticket ownership and CLI guidance understandable.** In Hugo, ticket creation silently led to a background worker leasing the ticket, then the requested run conflicted; status did not explain the active lease. In click and ky, bare `tm events` gave generic help rather than the event stream. Show who owns an active run and provide working, discoverable status/event commands. (hugo #15360, click #3822, ky #878)

5. **Report failures and verification limits precisely.** End-of-turn failure messages did not clarify whether work was saved or submitted, and some progress messages gave confusing recovery instructions. Also distinguish tests that actually exercised the agent's patch from tests blocked by setup, unavailable dependencies, or restored PR files. (click #3822, requests #7432, ky #878, ripgrep #3376, hugo #15360)

## Trial source files

Metrics and observations above come from `BurntSushi-ripgrep-3376.json`, `gohugoio-hugo-15360.json`, `pallets-click-3822.json`, `psf-requests-7432.json`, `sindresorhus-ky-878.json`, and `spf13-cobra-2257.json` in this folder.
