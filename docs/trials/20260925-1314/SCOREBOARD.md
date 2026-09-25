# Trial scoreboard

Six issue-fix trials were scored, from the two tools' run records. Scores are on a 1–5 scale; higher is better. “Passed” is the recorded trial outcome, not a claim that every implementation independently passed hidden tests. Times and token counts are medians across the six runs; tokens are the counts recorded in the trial files.

## Results by tool

| Tool | Pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX copy clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 3/6 (50%) | 1.83 | 1.00 | 1.83 | 2.50 | 1.83 | 1.67 | 1.33 | 906.718 s (15.1 min) | 4,082,265 |
| OpenCode | 4/6 (66.7%) | 4.33 | 3.83 | 4.33 | 4.00 | 4.17 | 3.33 | 5.00 | 67.086 s (1.1 min) | 23,655 |

## tm vs OpenCode, trial by trial

| Trial | tm | OpenCode | What happened |
|---|---|---|---|
| [cobra #2257](spf13-cobra-2257.json) | Pass; 924 s; 4,625,656 tokens | Pass; 67.1 s; 26,285 tokens | Both passed. tm found and fixed the bug, but submit failed after testing, so the user got a misleading failure report. |
| [ripgrep #3376](BurntSushi-ripgrep-3376.json) | Pass; 673 s; 4,375,051 tokens | Pass; 82 s; 30,201 tokens | Both recorded as passing. tm did not submit a fix or report a test result; the post-run check had a caveat because the checked-out test file included production code. OpenCode was honest that it had investigated but had not isolated the issue. |
| [requests #7432](psf-requests-7432.json) | Fail; 1,063 s; 3,789,478 tokens | Pass; 45.4 s; 18,855 tokens | tm spent most of the run rereading files and submitted no implementation. OpenCode made a focused fix; its own test command was blocked by the Python version shim, and the trial notes say the independent regression test passed. |
| [ky #878](sindresorhus-ky-878.json) | Fail; 8.6 s; 0 tokens | Fail; 71.1 s; 20,508 tokens | tm stopped before code work because its provider was unregistered, then advised an unhelpful retry. OpenCode implemented a fix and passed its stream tests, but the complete browser suite could not run because Playwright browsers were missing. |
| [click #3822](pallets-click-3822.json) | Fail; 1,200 s; 1,206,103 tokens | Fail; 67.1 s; 32,353 tokens | tm timed out without a change or tests. OpenCode made a focused change, but typing checks still showed two errors; the recorded pytest result passed. |
| [Hugo #15360](gohugoio-hugo-15360.json) | Pass; 889 s; 4,570,207 tokens | Pass; 43.8 s; 21,025 tokens | Both passed. tm reached the right decoder and recovered from errors, but took much longer and traversed unrelated files. OpenCode found the decoder quickly; its focused check was blocked by missing Go-version configuration. |

## Five biggest improvements for tm

1. **Make investigations much more focused and economical.** Context efficiency averaged 1.00/5; median runtime was 13.5 times OpenCode’s, and median recorded tokens were about 172 times higher. Repeated reads and broad history searches were called out in requests, ripgrep, cobra, click, and Hugo. Start with the issue’s concrete behavior, search the likely code path, and avoid rereading material without a clear new question. ([requests](psf-requests-7432.json), [ripgrep](BurntSushi-ripgrep-3376.json), [cobra](spf13-cobra-2257.json), [click](pallets-click-3822.json), [Hugo](gohugoio-hugo-15360.json))
2. **Always finish with a clear, truthful run summary.** Say whether a patch was submitted, what tests ran and their result, and what remains blocked. In requests and click there was no patch; in cobra a patch and passing tests existed despite a submit failure; in ripgrep testing was external to the run. ([requests](psf-requests-7432.json), [click](pallets-click-3822.json), [cobra](spf13-cobra-2257.json), [ripgrep](BurntSushi-ripgrep-3376.json))
3. **Replace generic retry advice with actionable recovery.** A retry with unchanged settings will not fix an unregistered provider, and repeating the same investigation did not help ripgrep. Explain the cause, preserve findings, and state the next useful action—or switch approach on retry. ([ky](sindresorhus-ky-878.json), [ripgrep](BurntSushi-ripgrep-3376.json), [requests](psf-requests-7432.json))
4. **Make completion and tool errors robust and understandable.** The cobra run had a correct, tested change but failed at submission; Hugo’s transient parse/state errors produced vague messages even though it recovered. Preserve completed work and test evidence when final submission fails, and explain errors in language that tells the user what happened. ([cobra](spf13-cobra-2257.json), [Hugo](gohugoio-hugo-15360.json))
5. **Check the environment and the exact regression before calling work done.** The click run ended at its time bound without implementation or tests. In ky, the unregistered provider prevented any work. Validate dependencies/provider availability early, then reproduce the issue with the relevant regression case and report any remaining verification limits. ([click](pallets-click-3822.json), [ky](sindresorhus-ky-878.json), [ripgrep](BurntSushi-ripgrep-3376.json))

*Method note: summaries and caveats above come from each JSON trial record. In particular, passing outcomes can have qualifications: ripgrep’s checked-out test file included production code, and some OpenCode test runs were blocked or only partially complete. The scoreboard reflects the recorded `passed` flags and recorded rubric scores as-is.*
