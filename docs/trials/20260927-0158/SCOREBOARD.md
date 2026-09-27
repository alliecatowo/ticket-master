# Trial scoreboard

Scores are on a 1–5 scale. The scoreboard covers the five trials with results for both tools. Hugo #15360 was skipped because its linked fix was closed, not merged, and no separately landed fix was found; it has no comparable tool scores.

## Results by tool

| Tool | Pass rate | Navigation quality | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 1/5 (20%) | 2.4 | 1.2 | 1.4 | 3.6 | 2.0 | 1.6 | 1.2 | 144.511 s | 323,267 |
| OpenCode | 2/5 (40%) | 4.0 | 3.6 | 4.4 | 4.6 | 4.0 | 3.8 | 4.4 | 116.366 s | 30,161 |

“Passed” is the trial result’s `passed` value. Means and medians use the five paired trials only. Lower wall time and token use are better. tm’s one pass was ripgrep; OpenCode passed requests and cobra.

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| Click #3822 — generic `click.Path` | Fail · 216.242 s · 667,511 tokens | Fail · 116.366 s · 30,161 tokens | tm found the class but repeated investigation without editing. OpenCode made a focused change, but missed required default typing and runtime generic behavior. |
| ky #878 — empty-body download progress | Fail · 167.636 s · 323,267 tokens | Fail · 133.824 s · 27,332 tokens | tm found the right code, but spent heavily retrying and left test results unclear. OpenCode’s focused Node regression passed, but the real Chromium browser case still failed. |
| Requests #7432 | Fail · 78 s · 319,195 tokens | Pass · 63 s · 32,013 tokens | tm made no code change after three similar attempts. OpenCode implemented the fix and the independent hidden PR test passed after its project test dependencies were installed. |
| Cobra #2257 | Fail · 123.177 s · 194,694 tokens | Pass · 119.299 s · 20,950 tokens | tm retried without editing; the hidden test showed the `--` argument bug remained. OpenCode fixed the argument handling and the independent regression passed. |
| ripgrep #3376 | Pass · 144.511 s · 658,678 tokens | Pass · 106.008 s · 64,728 tokens | tm’s result passed, but its attempts repeatedly investigated overlapping files and its recovery directions conflicted. OpenCode did not change code and honestly reported its focused test failure. |

## Five biggest things tm should improve

1. **Break out of repetitive retries and get to an edit.** In Click, Requests, Cobra, and ripgrep, attempts repeated similar reads/searches, often using the full step allowance without changing code. Requests and Cobra ended without a fix; ripgrep passed but still spent heavily on repeated exploration. Carry findings into the next attempt, stop rereading the same material, and make a focused implementation step the goal. (Click #3822; Requests #7432; Cobra #2257; ripgrep #3376.)
2. **Cut token use sharply.** Across the five paired runs, tm’s median was 323,267 tokens versus OpenCode’s 30,161—over ten times as many. Especially high tm totals appeared in Click (667,511), ripgrep (658,678), Requests (319,195), and ky (323,267). Bound repeated investigation and retry spend. (All five paired trials.)
3. **Make recovery instructions match the actual state.** Messages alternated between “resume this session,” “a retry is scheduled,” and “retry the ticket,” sometimes with stale session IDs. That leaves users unsure whether to wait, resume, or retry. Give one valid next step based on the ticket and retry state. (Click #3822; ky #878; Requests #7432; Cobra #2257; ripgrep #3376.)
4. **Plan toward the acceptance test, then verify the exact behavior.** The main failures were not just lack of activity: Click needed the expected default and runtime generic behavior; Requests and Cobra never reached a fix; ky’s test result was unclear and the full browser checks were blocked. Inspect the expected behavior early, make a targeted change, and report which relevant checks passed or failed. (Click #3822; ky #878; Requests #7432; Cobra #2257.)
5. **Make user-facing status and tool errors clearer.** Retry/attempt messages did not explain that investigation was repeating, test outcomes were hard to see in ky, and a missing ticket error (“couldn’t find that”) did not identify what was missing. Explain what was tried, what happened, and the single next action in plain terms. (Click #3822; ky #878; Requests #7432; Cobra #2257; ripgrep #3376.)
