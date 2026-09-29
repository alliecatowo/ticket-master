# Trial scoreboard

Three trials had results for both tools. The fourth JSON file, `BurntSushi-ripgrep-3376.json`, was skipped because the issue was already fixed and the merged change had no test-file changes. Pass rates and averages below use only the three scored trials; rubric scores are out of 5.

## Results by tool

| Tool | Pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 0/3 (0%) | 2.33 | 1.00 | 1.33 | 2.33 | 2.00 | 1.00 | 2.33 | 72.262 s | 219,737 |
| OpenCode | 2/3 (67%) | 4.00 | 4.00 | 4.33 | 4.00 | 4.00 | 4.00 | 5.00 | 72.300 s | 38,959 |

Median tokens were about 5.6 times higher for tm, while median wall time was nearly the same. Each dimension is the arithmetic mean across the three trials, rounded to two decimal places.

## tm vs OpenCode by trial

| Trial | tm result (wall time; tokens) | OpenCode result (wall time; tokens) | What happened |
|---|---|---|---|
| `pallets-click-3822.json` | Fail; 72.262 s; 219,737 | Fail; 91.911 s; 23,037 | tm found relevant files but repeated reads and submitted no change. OpenCode made a change and ran tests, but missed default-`click.Path()` typing and runtime cases. |
| `psf-requests-7432.json` | Fail; 67.7 s; 198,654 | Pass; 72.3 s; 192,547 | tm found the relevant code but repeated reads/tool errors and submitted no patch. OpenCode changed stream detection, added a regression test, and passed the hidden regression test. |
| `spf13-cobra-2257.json` | Fail; 95 s; 429,435 | Pass; 62 s; 38,959 | tm identified the right files but did not attempt the small fix. OpenCode made the fix, tested it, and reported passing results. |

## Five biggest improvements for tm

1. **Turn investigation into an edit sooner.** In all three scored trials, tm found relevant code but ended without submitting a fix; the Cobra run specifically identified a small slice-copy fix and still did not attempt it. Set a clear threshold for exploration, then make the smallest plausible change and test it. (`spf13-cobra-2257.json`; also `psf-requests-7432.json` and `pallets-click-3822.json`.)
2. **Stop rereading the same files and carry findings across continuations.** Repeated source reads or exploration were called out in the Click, Requests, and Cobra trials. Keep a concise working summary of files inspected, diagnosis, and next action so retries build on prior work. (`pallets-click-3822.json`; `psf-requests-7432.json`; `spf13-cobra-2257.json`.)
3. **Make continuation recovery productive and specific.** The Click and Requests runs exhausted continuation attempts without useful work; Cobra had three failed dispatches. After a failure, preserve the established diagnosis and direct the next attempt to an edit or targeted check, rather than restarting exploration. (`pallets-click-3822.json`; `psf-requests-7432.json`; `spf13-cobra-2257.json`.)
4. **Improve tool-error handling and telemetry consistency.** Requests reported file-read errors without a useful explanation, while Cobra's wall time and token telemetry disagreed with the reported stats (95 seconds versus 46 seconds, and 429,435 total tokens versus zero input/output tokens). Surface actionable errors and make reported time/token measures agree. (`psf-requests-7432.json`; `spf13-cobra-2257.json`.)
5. **Use plain-language status and one clear recovery instruction.** Click's “focused continuation” and “provider transport retries” language was unclear, and it offered two recovery commands without explaining which preserves the session. Requests also described the continuation failure generically. Say what happened, whether edits were retained, and give one recommended next step. (`pallets-click-3822.json`; `psf-requests-7432.json`.)

## Source files

- `pallets-click-3822.json`
- `psf-requests-7432.json`
- `spf13-cobra-2257.json`
- `BurntSushi-ripgrep-3376.json` (skipped; excluded from score calculations)
