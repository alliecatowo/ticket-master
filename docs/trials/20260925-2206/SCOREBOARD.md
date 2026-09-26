# Trial scoreboard

Plain-English summary of the five trial result files in this directory. Scores are on the recorded 1–5 rubric scale; higher is better. A pass means the trial's `passed` field is true. Times and tokens are medians across the five runs. Token counts are reported as recorded by each tool, so they may not be directly comparable in every trial.

## Tool-level results

| Tool | Pass rate | Navigation quality (mean) | Context efficiency (mean) | Plan quality (mean) | Verification honesty (mean) | UX copy clarity (mean) | Recoverability (mean) | Cost/time (mean) | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 3/5 (60%) | 3.2 | 1.2 | 2.4 | 2.2 | 1.8 | 2.4 | 1.6 | 241 s | 2,786,544 |
| OpenCode | 4/5 (80%) | 3.8 | 4.0 | 3.6 | 4.6 | 4.0 | 3.2 | 4.6 | 94.8 s | 24,130 |

## tm vs OpenCode by trial

| Trial | tm result | OpenCode result | Faster | Fewer recorded tokens |
|---|---|---|---|---|
| `sindresorhus-ky-878` | Fail; 122 s; 462,517 tokens | Fail; 211 s; 24,130 tokens | tm | OpenCode |
| `psf-requests-7432` | Pass; 241 s; 2,786,544 tokens | Pass; 79.5 s; 21,777 tokens | OpenCode | OpenCode |
| `spf13-cobra-2257` | Pass; 972.3 s; 3,808,073 tokens | Pass; 60.1 s; 28,365 tokens | OpenCode | OpenCode |
| `gohugoio-hugo-15360` | Pass; 912.7 s; 5,841,099 tokens | Pass; 53.9 s; 22,873 tokens | OpenCode | OpenCode |
| `pallets-click-3822` | Fail; 185 s; 916,284 tokens | Pass; 110 s; 296,320 tokens | OpenCode | OpenCode |

## Five biggest improvement opportunities for tm

1. **Cut repeated exploration and context use.** tm averaged just 1.2/5 on context efficiency and used far more recorded tokens in four of five trials. In Hugo, it repeatedly searched overlapping BOM/trim/unmarshal terms and read unrelated config/template paths before settling on the relevant implementation; in Cobra it reread large files and searched Go installation paths for 972 seconds. Start from the likely implementation and adjacent tests, avoid repeating searches or reads, and set a focused exploration budget. (Trials: `gohugoio-hugo-15360`, `spf13-cobra-2257`; also the 2,786,544-token run in `psf-requests-7432`.)

2. **Make failures specific and recoverable.** tm scored 2.4/5 on recoverability. Generic messages such as “hit an unexpected internal problem,” “got a response it couldn't understand,” or “model ended turn without submitting” did not explain what failed or what state was saved. In Cobra it reached the 64-step limit without submitting; in Click a retry repeated the investigation without producing a patch. Say what completed, what did not, preserve the underlying error, and offer a concrete next step. (Trials: `sindresorhus-ky-878`, `spf13-cobra-2257`, `pallets-click-3822`, `gohugoio-hugo-15360`.)

3. **Report verification precisely and honestly.** tm averaged 2.2/5 on verification honesty. Its final summary should name the exact test/check command and whether it passed, failed, or could not run, and distinguish a submitted patch from a verified result. Requests included attempts with interpreters that could not import the project; Ky exposed environment blockers including ignored dependency build scripts, a pre-existing lint error, and missing browsers. (Trials: `psf-requests-7432`, `sindresorhus-ky-878`, `pallets-click-3822`.)

4. **State a narrow plan before diving in.** tm averaged 2.4/5 on plan quality. The Hugo and Click reviews called out the lack of a clear scoped plan; Click ended without an implementation or verification after substantial exploration. Briefly state the suspected code path, intended edit, and targeted test before exploring broadly. (Trials: `gohugoio-hugo-15360`, `pallets-click-3822`.)

5. **Improve user-facing progress and completion messages.** tm averaged 1.8/5 on UX copy clarity. Progress like “Ran tests” hid useful detail in Ky, and generic submit/save errors left users unable to tell what happened. Provide concise, action-specific progress and a final recap of the change, verification outcome, and any blocker. (Trials: `sindresorhus-ky-878`, `gohugoio-hugo-15360`, `psf-requests-7432`.)

## How to read the comparison

OpenCode passed four trials to tm's three, was faster in four of five, and recorded fewer tokens in four of five. tm was faster only on Ky, where both tools failed. These are five task runs, so the results are directional rather than a guarantee about other tasks.
