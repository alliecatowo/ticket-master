# Trial scoreboard

Scores are on a 1–5 scale. Pass rate is based on the benchmark's `passed` field. Five trials had usable results; `gohugoio/hugo#15360` was skipped because its proposed fix PR was closed without a merge, so there was no merged fix and regression test to benchmark.

## tm

| Measure | Result |
|---|---:|
| Pass rate | 2/5 (40%) |
| Mean navigation quality | 2.8/5 |
| Mean context efficiency | 1.0/5 |
| Mean plan quality | 1.4/5 |
| Mean verification honesty | 3.0/5 |
| Mean UX copy clarity | 2.0/5 |
| Mean recoverability | 1.4/5 |
| Mean cost/time | 1.2/5 |
| Median wall time | 136.1 seconds |
| Median tokens | 451,616 |

## OpenCode

| Measure | Result |
|---|---:|
| Pass rate | 2/5 (40%) |
| Mean navigation quality | 4.0/5 |
| Mean context efficiency | 3.6/5 |
| Mean plan quality | 4.2/5 |
| Mean verification honesty | 3.6/5 |
| Mean UX copy clarity | 4.0/5 |
| Mean recoverability | 4.0/5 |
| Mean cost/time | 4.8/5 |
| Median wall time | 79.2 seconds |
| Median tokens | 24,965 |

## tm vs OpenCode, trial by trial

| Trial | tm result | OpenCode result | Comparison |
|---|---|---|---|
| [pallets/click#3822](https://github.com/pallets/click/issues/3822) | Fail; 136.1 s; 187,695 tokens | Fail; 66.81 s; 22,913 tokens | Neither fixed the hidden typing assertions. OpenCode made a patch and tested more, but missed the default-path case; tm did not produce a patch. |
| [psf/requests#7432](https://github.com/psf/requests/issues/7432) | Pass; 132.2 s; 451,616 tokens | Fail; 79.192 s; 48,058 tokens | tm made the production change and passed the hidden redirect regression test. OpenCode's local test passed, but it did not change the production detector and the hidden test failed. |
| [BurntSushi/ripgrep#3376](https://github.com/BurntSushi/ripgrep/issues/3376) | Pass*; 148 s; 628,019 tokens | Pass*; 152 s; 36,254 tokens | Both were marked passing, but the hidden-test checkout included the production fix, so these passes do not show that either tool solved the issue. OpenCode did produce and test a fix beforehand; tm did not produce a patch. |
| [spf13/cobra#2257](https://github.com/spf13/cobra/issues/2257) | Fail; 114 s; 203,538 tokens | Pass; 70.4 s; 24,965 tokens | OpenCode fixed the append side effect, added a regression test, and passed the full Go suite. tm found relevant code but made no change or verification run. |
| [sindresorhus/ky#878](https://github.com/sindresorhus/ky/issues/878) | Fail; 186.313 s; 728,884 tokens | Fail; 92.157 s; 19,155 tokens | OpenCode made a minimal fix and passed its focused test, but missed a callback-shape condition in hidden tests. tm made no patch and ran no verification. |

\*The benchmark JSON records both as passes, but the accompanying trial notes explain why the mechanical hidden-test pass was not independent validation.

## Five biggest improvements for tm

1. **Stop repeated no-progress attempts much sooner.** In click, ripgrep, cobra, and ky, tm revisited substantially the same files across three attempts without producing a patch; each of those trials used 187,695–728,884 tokens. Track what an attempt learned and stop or change strategy when a retry adds nothing. (click#3822, ripgrep#3376, cobra#2257, ky#878)
2. **Set a clear plan and a bounded budget before exploring.** Plan-quality scores averaged 1.4/5, and cost/time averaged 1.2/5. State the likely code path, intended change, target regression test, and a token/time limit early; carry findings forward instead of restarting. (click#3822, requests#7432, ripgrep#3376, cobra#2257, ky#878)
3. **Prioritize implementing a focused fix over continued inspection.** In three trials tm found relevant code but delivered neither a change nor a targeted verification run. Turn the diagnosis into the smallest plausible patch, or clearly explain the blocker and stop. (click#3822, cobra#2257, ky#878)
4. **Make retries and failures understandable and actionable.** Tell the user whether a retry is a transport retry or a new work attempt, what has already been tried, why it stopped, and one accurate next step. Avoid vague provider-failure notices followed by a generic failure. (click#3822, requests#7432, cobra#2257, ky#878)
5. **Improve the ticket handoff and output contract.** Make ticket creation return a directly usable structured ticket ID rather than a sentence that scripts can accidentally pass as the ID. The wrapper recovered from this capture problem in click; the ripgrep note also called out the same unsafe output shape. (click#3822, ripgrep#3376)
