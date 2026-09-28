# Trial scoreboard

Based on all six top-level trial result files in this folder. “Pass” is the recorded arm result; the score averages are the rubric scores in those files. Time and token figures are medians across trials.

## Results by tool

| Tool | Pass rate | Navigation quality | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 3/6 (50%) | 2.17 | 1.00 | 1.67 | 2.33 | 2.17 | 1.33 | 1.17 | 634.7 s | 539,591 |
| OpenCode | 6/6 (100%) | 4.00 | 3.67 | 4.33 | 4.83 | 4.00 | 4.33 | 4.83 | 88.0 s | 23,889 |

## Head-to-head by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| Hugo issue 15360 ([result](gohugoio-hugo-15360.json)) | Pass · 755 s · 566,717 tokens | Pass · 141 s · 25,662 tokens | OpenCode made and tested a focused BOM-handling fix. tm repeatedly inspected the same code, made no change, and did not run task verification; the pass reflects an independent mechanical check, not a tm-authored fix. |
| Click issue 3822 ([result](pallets-click-3822.json)) | Fail · 463.4 s · 512,464 tokens | Pass · 65.9 s · 22,115 tokens | tm found the relevant typing code but made no patch. OpenCode made a focused generic-type change and passed runtime and typing checks. |
| Cobra issue 2257 ([result](spf13-cobra-2257.json)) | Fail · 622.5 s · 591,956 tokens | Pass · 94.9 s · 19,150 tokens | tm did not get beyond repeated inspection. OpenCode fixed the argument-slice aliasing bug and passed the regression test. |
| ripgrep issue 3376 ([result](BurntSushi-ripgrep-3376.json)) | Pass · 850.5 s · 608,146 tokens | Pass · 80.7 s · 42,967 tokens | OpenCode made and tested a fix. tm made no patch and ran no task verification; its pass was only the independent post-run check. The tm run also removed `/tmp/reproduce`, outside the project. |
| ky issue 878 ([result](sindresorhus-ky-878.json)) | Pass · 647 s · 471,917 tokens | Pass · 193 s · 20,425 tokens | Both passed. tm made a fix but used repeated exploratory test runs and much more time and context. OpenCode handled a null-body case discovered by its first test and then passed focused tests. |
| Requests issue 7432 ([result](psf-requests-7432.json)) | Fail · 577.3 s · 139,430 tokens | Pass · 81 s · 33,049 tokens | tm ended without a patch after repeated reads. OpenCode passed, while acknowledging and correcting a test run that had loaded another checkout. |

## Five biggest improvements for tm

1. **Turn repeated inspection into a concrete edit-and-test step.** Several runs found the right code quickly, then repeated the same reads across attempts without changing files. Carry findings forward and require a distinct next action after inspection stalls. This is the main failure pattern in Click 3822, Cobra 2257, Hugo 15360, ripgrep 3376, and Requests 7432.
2. **Make retries reuse work instead of restarting it.** After a stalled attempt, tm replayed essentially the same investigation, sometimes several times. Keep the useful findings and give the next attempt a narrow task and a clear limit. This is especially evident in Cobra 2257 and Requests 7432; Click 3822 also repeats the same two files across three attempts.
3. **Cut context and elapsed time sharply.** tm's median was about 10.6 minutes and 539,591 tokens, versus OpenCode's 1.5 minutes and 23,889 tokens. Repeated reads drove the waste in the failed Click, Cobra, Hugo, and Requests runs; even the successful ky run used 471,917 tokens over 647 seconds ([ky result](sindresorhus-ky-878.json)).
4. **Give one accurate, actionable recovery instruction.** Messages variously told users to resume, retry, or wait even when retries were already scheduled—or said no retry was scheduled while still showing a retry command. State whether tm is working, stopped, or needs the user, and give exactly the next useful action. Examples: Hugo 15360, Click 3822, ripgrep 3376, and Requests 7432.
5. **Verify the change and report what the pass means.** A passing external or mechanical check is not evidence that tm produced a fix. Make task-specific tests part of the run, clearly distinguish tm's own verification from independent checks, and report the result honestly. The Hugo 15360 and ripgrep 3376 results both record a pass despite no tm-authored patch or task verification; Cobra 2257 shows the opposite clearly, with the regression test failing for tm and passing for OpenCode.
