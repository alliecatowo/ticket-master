# Trial scoreboard

This report covers the four comparable trials with results for both tools. A fifth JSON file, `gohugoio-hugo-15360.json`, is marked skipped because the PR was closed without a merge, so it has no scored run. Scores are on the rubric's 1–5 scale. “Passed” means the independent hidden regression test passed; it does not necessarily mean the tool submitted a patch or completed the ticket.

## Results by tool

| Tool | Hidden test pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 2/4 (50%) | 3.00 | 1.25 | 1.75 | 2.00 | 2.00 | 1.25 | 2.00 | 543 s (9m 3s) | 3,783,496 |
| OpenCode | 2/4 (50%) | 4.25 | 4.25 | 4.25 | 4.00 | 4.00 | 3.50 | 5.00 | 71 s (1m 11s) | 28,167 |

Medians use the four scored trials. Median tokens are the reported token counts. The pass rate is tied, while OpenCode's median wall time and token use are substantially lower in this sample.

## tm vs OpenCode, trial by trial

| Trial | Hidden test | tm: time / tokens | OpenCode: time / tokens | Comparison |
|---|---|---:|---:|---|
| `pallets-click-3822` — generic `click.Path` | Both failed | 410 s / 1,416,202 | 161 s / 41,206 | tm found the right implementation area but never submitted an edit. OpenCode made the generic change but missed the unparameterized `Path()` type case, and overstated verification after checking the wrong typing test. |
| `psf-requests-7432` — streamed redirect proxy | Both passed | 676 s / 6,150,789 | 52 s / 18,619 | tm explored and reread without submitting a patch or running tests; the independent test passed against the retained result. OpenCode made the compatibility fix and disclosed its environment-related test failures. |
| `sindresorhus-ky-878` — empty response body | Both failed | 58.5 s / 511,713 | 85.1 s / 36,254 | tm's implementation was correct and all 19 independently selected stream tests passed, but the recorded overall run was marked failed due to pre-existing lint and missing browser binaries. OpenCode also reported those blockers clearly; build and TypeScript checks passed. |
| `spf13-cobra-2257` — completion append capacity | Both passed | 1,446 s / 6,759,875 | 57 s / 20,079 | tm produced the correct capacity fix but hit step limits and did not submit the ticket. OpenCode made a narrow fix, ran `go test ./...` and `git diff --check`, and reported its result accurately. |

## Five biggest things tm should improve

1. **Stop repeating investigation and control context cost.** Carry forward file findings, avoid overlapping rereads, and detect repeated searches. In `psf-requests-7432`, tm used 6,150,789 tokens and 676 seconds rereading the same implementation and test areas; in `spf13-cobra-2257`, it used 6,759,875 tokens and 1,446 seconds amid repeated reads/searches. OpenCode completed those trials in 52 and 57 seconds, respectively.

2. **Protect time and steps for a finished submission.** Reserve budget for editing, verification, and submitting rather than exploring until limits are reached. In `psf-requests-7432`, the 64-step limit arrived without a patch or tests. In `spf13-cobra-2257`, the first run hit the limit and the retry also ended without submission, despite a correct retained edit.

3. **Make completion and retry status explicit.** Tell users whether a patch was submitted, what state was preserved, whether a retry is merely scheduled or actually completed, and the exact next action. In `pallets-click-3822`, both attempts ended without submission, but the user-facing instruction simply repeated “Run `tm run T-1` again to retry.” In `psf-requests-7432`, tm scheduled a retry but did not clearly say no patch was submitted and tests had not run.

4. **Report verification accurately, with the command and result.** Distinguish a passing focused check from a failing overall command or a check that never ran. In `sindresorhus-ky-878`, progress only said “Ran tests” even though XO stopped the recorded test command on a pre-existing lint error and browser tests could not run without Playwright binaries. `pallets-click-3822` also ended without a submission, while its independent typing test failed. The report should say exactly what was verified and what failed or was skipped.

5. **Make transient failures recoverable and explain what happened.** Automatically continue after a text-only model stop where possible, keep a useful transcript/checkpoint, and explain recovery in plain language. In `sindresorhus-ky-878`, submission returned internal/response errors before eventually succeeding, without explaining the recovery. In `spf13-cobra-2257`, retries left the edit unsubmitted, and bare `tm events` showed help rather than useful history. A checkpoint plus clear recovery instructions would avoid making users guess whether work survived.

## Source files

- `pallets-click-3822.json`
- `psf-requests-7432.json`
- `sindresorhus-ky-878.json`
- `spf13-cobra-2257.json`
- `gohugoio-hugo-15360.json` (skipped; no merged fix)
