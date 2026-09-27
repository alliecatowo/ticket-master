# Trial scoreboard

## Results by tool

There are four comparable trials. “Pass” is the trial's recorded pass/fail result. Rubric scores are on the source files' 1–5 scale; higher is better. Wall time and tokens are medians across the four trials. Median tokens are shown as thousands for readability.

| Tool | Pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 0/4 (0%) | 2.50 | 2.00 | 1.25 | 2.50 | 2.00 | 1.50 | 2.00 | 47.7 s | 214.7k |
| OpenCode | 2/4 (50%) | 4.00 | 3.25 | 4.25 | 4.50 | 4.25 | 3.75 | 5.00 | 75.2 s | 29.0k |

## tm vs OpenCode in each trial

| Trial | tm result (time; tokens) | OpenCode result (time; tokens) | What stood out |
|---|---|---|---|
| `spf13-cobra-2257.json` | Fail; 31.0 s; 160,718 | Pass; 55.9 s; 59,776 | tm stopped at its ten-step limit without a patch or tests. OpenCode made the copy-before-append fix and ran `go test ./...`. |
| `psf-requests-7432.json` | Fail; 64.4 s; 268,751 | Fail; 87.5 s; 20,602 | tm repeated reads/searches, then hit a provider 429 on retry; its regression test failed. OpenCode edited the tm clone instead of its own checkout, so the target implementation remained unchanged and its regression test failed there. |
| `sindresorhus-ky-878.json` | Fail; 86.0 s; 400,221 | Fail; 63.0 s; 33,646 | tm eventually submitted a focused fix on retry, but the full suite failed on an existing XO error; its first run exhausted the step budget. OpenCode's focused checks passed, but the full suite had the same existing XO failure, and its browser test could not run because Playwright browsers were absent. |
| `pallets-click-3822.json` | Fail; 20.0 s; 7,114 | Pass; 96.0 s; 24,395 | tm ended after a provider 429 instead of recovering in the foreground. OpenCode made the change and verified it with mypy, pyright, and focused pytest. |

## Five biggest improvements for tm

1. **Recover automatically from provider limits.** A provider 429 ended the Click run, and the Requests retry also hit a 429. Retry with the provider's requested wait, show the attempt clearly, and preserve the work (`pallets-click-3822.json`, `psf-requests-7432.json`).
2. **Make step limits produce progress, not a dead end.** Runs used their ten-step allowance on repeated inspection or temporary verification scripts and stopped without a patch. Detect stalled loops sooner and continue safely with a compact handoff (`spf13-cobra-2257.json`, `sindresorhus-ky-878.json`, `psf-requests-7432.json`).
3. **Avoid duplicate reads and searches.** Re-reading the same files and repeating overlapping searches consumed substantial effort, especially in the Requests and Cobra trials. Track what has already been inspected and narrow follow-up searches (`psf-requests-7432.json`, `spf13-cobra-2257.json`).
4. **Explain retries and the actual state plainly.** The user was told to rerun even when a retry had already been scheduled; in another run the first attempt said no change was submitted although the second attempt later made one. Say whether work exists, whether a retry is underway, and what happens next (`sindresorhus-ky-878.json`, `spf13-cobra-2257.json`, `pallets-click-3822.json`).
5. **Use fewer tokens and verify the final result.** tm's median was 214.7k tokens versus OpenCode's 29.0k, and tm passed none of the four trials. Keep investigation focused, run the relevant regression/full checks where possible, and make clear when a mechanical failure (such as an existing suite error) prevents a pass (`sindresorhus-ky-878.json`, `psf-requests-7432.json`).

## Coverage and notes

Two JSON files were read but excluded from the tool comparison because they have no paired tool runs: `BurntSushi-ripgrep-3376.json` says no merged fix PR with tests exists, and `gohugoio-hugo-15360.json` says no merged fix PR exists. This scoreboard includes the four files with `arms` results and uses their recorded `wall_seconds`, `tokens`, `passed`, and rubric values.
