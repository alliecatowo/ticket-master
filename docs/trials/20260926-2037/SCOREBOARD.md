# Trial scoreboard

This scoreboard covers the six trial result files in this directory. Rubric scores are averages on a 1–5 scale; higher is better. “Passed” is the result recorded by the trial, and does not always mean the issue was actually fixed (see Hugo below). Median tokens use only trials where a token count was recorded: tm has 4 of 6; OpenCode has 6 of 6.

## Results by tool

| Tool | Passes | Navigation quality | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 2/6 (33%) | 2.83 | 1.50 | 2.17 | 2.83 | 2.00 | 1.67 | 2.00 | 98.5 sec | 156,369* |
| OpenCode | 4/6 (67%) | 3.83 | 3.50 | 4.17 | 4.33 | 4.17 | 4.17 | 4.50 | 112 sec | 30,575 |

*tm's median is based on the four available counts; token data was missing for ripgrep and Click. Scores and pass rates are simple averages across six trials. Wall time is elapsed seconds.

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| Hugo #15360 | Passed*; 235 sec; 376,235 tokens | Passed; 118 sec; 30,461 tokens | tm found the decoder but made no patch before stopping. OpenCode fixed BOM handling and added regression cases. *The recorded package test passed for tm, but the issue was not fixed. ([gohugoio-hugo-15360.json](gohugoio-hugo-15360.json)) |
| Click #3822 | Failed; 78.7 sec; tokens unavailable | Failed; 127.7 sec; 26,931 tokens | tm stopped after provider rate limits without an implementation. OpenCode implemented the typing change, but missed a default-typing case. ([pallets-click-3822.json](pallets-click-3822.json)) |
| Cobra #2257 | Failed; 84 sec; 129,206 tokens | Passed; 85 sec; 30,689 tokens | tm repeated reads and stopped on HTTP 429 without a patch; OpenCode made a focused fix and regression test. ([spf13-cobra-2257.json](spf13-cobra-2257.json)) |
| ripgrep #3376 | Passed; 136 sec; tokens unavailable | Passed; 177 sec; 34,898 tokens | tm found relevant files but repeated reads and ended without a patch or verification. OpenCode produced a fix and tests; the trial notes a limitation in the independent final test check. ([BurntSushi-ripgrep-3376.json](BurntSushi-ripgrep-3376.json)) |
| Ky #878 | Failed; 105 sec; 183,531 tokens | Failed; 79 sec; 20,821 tokens | Both made the empty-body fix. tm hit provider/recovery and verification problems; OpenCode reported focused tests accurately, while the broad test command also encountered a pre-existing lint failure. ([sindresorhus-ky-878.json](sindresorhus-ky-878.json)) |
| Requests #7432 | Failed; 92 sec; 103,467 tokens | Passed; 106 sec; 37,494 tokens | tm never formed a plan or made a change; OpenCode implemented the fix and passed the focused regression check. ([psf-requests-7432.json](psf-requests-7432.json)) |

## Five biggest improvements for tm

1. **Stop looping on the same reads; make a small plan and act.** Repeated file and history inspections used time and tokens without producing patches in Hugo, Cobra, ripgrep, and Requests. State the likely change and the test to run, then move from exploration to implementation. ([gohugoio-hugo-15360.json](gohugoio-hugo-15360.json), [spf13-cobra-2257.json](spf13-cobra-2257.json), [BurntSushi-ripgrep-3376.json](BurntSushi-ripgrep-3376.json), [psf-requests-7432.json](psf-requests-7432.json))

2. **Handle provider rate limits with bounded retries and clear status.** Several runs ended at HTTP 429, sometimes after messages that sounded like repeated dispatches. Retry with sensible backoff, identify each attempt, and explain plainly when the run has stopped and why. ([pallets-click-3822.json](pallets-click-3822.json), [spf13-cobra-2257.json](spf13-cobra-2257.json), [BurntSushi-ripgrep-3376.json](BurntSushi-ripgrep-3376.json), [sindresorhus-ky-878.json](sindresorhus-ky-878.json))

3. **Make recovery a clear, state-aware next step.** The retry and resume choices were confusing or left entirely to the user; explain what each command does, provide one copyable action, and guide a retry rather than ending with raw provider errors. ([pallets-click-3822.json](pallets-click-3822.json), [psf-requests-7432.json](psf-requests-7432.json), [gohugoio-hugo-15360.json](gohugoio-hugo-15360.json))

4. **Report verification accurately—including what did not get tested.** A passing command can be misleading if no implementation was made, and failed broad checks should be separated from successful focused checks. Say what changed, name the exact commands and outcomes, and do not imply the issue is fixed solely because an existing suite passed. ([gohugoio-hugo-15360.json](gohugoio-hugo-15360.json), [sindresorhus-ky-878.json](sindresorhus-ky-878.json), [BurntSushi-ripgrep-3376.json](BurntSushi-ripgrep-3376.json))

5. **Improve output clarity and usage reporting.** Provide machine-friendly ticket IDs, preserve the useful cause in command errors, report elapsed time consistently, and make token totals visible and unambiguous. These details matter most when a run fails and the user needs to understand or resume it. ([pallets-click-3822.json](pallets-click-3822.json), [BurntSushi-ripgrep-3376.json](BurntSushi-ripgrep-3376.json), [sindresorhus-ky-878.json](sindresorhus-ky-878.json), [psf-requests-7432.json](psf-requests-7432.json))
