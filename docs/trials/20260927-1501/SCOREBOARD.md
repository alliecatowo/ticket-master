# Trial scoreboard

Five trials had results for both tools. `gohugoio/hugo #15360` was skipped because no merged fix PR with tests was available, so it is excluded from the rates and averages below.

## Results by tool

Scores are mean rubric scores on a 1–5 scale. Wall time and token counts are medians. A pass means the trial result's `passed` field is true.

| Tool | Pass rate | Navigation | Context efficiency | Plan quality | Verification honesty | UX clarity | Recoverability | Cost/time | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 2/5 (40%) | 2.4 | 1.2 | 2.2 | 2.2 | 2.0 | 1.6 | 1.4 | 116.3 sec | 418,337 |
| OpenCode | 4/5 (80%) | 4.0 | 3.4 | 4.0 | 4.2 | 4.0 | 3.4 | 3.8 | 147 sec | 29,641 |

## tm vs OpenCode by trial

| Trial | tm | OpenCode | Comparison |
|---|---|---|---|
| pallets/click #3822 | Fail; 98 sec; 418,337 tokens | Fail; 147 sec; 210,605 tokens | OpenCode scored higher across every rubric dimension and produced a plausible implementation, but missed the new typing fixture; tm made no edit. |
| spf13/cobra #2257 | Fail; 116.3 sec; 501,751 tokens | Pass; 205.7 sec; 32,990 tokens | OpenCode implemented the fix and passed `go test ./...`; tm repeatedly explored without changing code or testing. |
| BurntSushi/ripgrep #3376 | Pass; 134 sec; 558,210 tokens | Pass; 61 sec; 20,025 tokens | Both passed, though neither account describes a strong implementation/verification result. OpenCode was faster and much more token-efficient; tm's scores were lower in every dimension. |
| sindresorhus/ky #878 | Pass; 176.5 sec; 386,454 tokens | Pass; 275 sec; 29,641 tokens | Both passed. tm's second attempt found and fixed the stream flush issue, but OpenCode scored higher across all rubric dimensions and used far fewer tokens. |
| psf/requests #7432 | Fail; 89.7 sec; 315,055 tokens | Pass; 108.5 sec; 21,865 tokens | OpenCode implemented and verified the compatibility fix; tm repeated source inspection without reaching an edit or test. |

## Five biggest improvements for tm

1. **Turn repeated investigation into a focused next action.** In click #3822, Cobra #2257, and Requests #7432, tm reread or re-searched overlapping code across attempts and never reached a patch. Carry forward findings, state a concrete hypothesis, and move to one targeted edit rather than restarting discovery.
2. **Put a firm check on context and token use.** Across the five trials, tm's median was 418,337 tokens versus OpenCode's 29,641. The gap was especially large in ripgrep #3376 (558,210 vs 20,025) and Cobra #2257 (501,751 vs 32,990). Stop redundant reads early and reserve enough budget for implementation and verification.
3. **Make retry and recovery instructions agree with actual state.** Click #3822, Cobra #2257, ky #878, and Requests #7432 describe confusing combinations of automatic retry, escalation, and `tm --resume` or ticket-retry advice. Tell the user plainly whether a retry is queued, what already happened, and give one valid next step.
4. **Prioritize an edit and focused regression check over more exploration.** In Cobra #2257 and Requests #7432 tm made no change or test; in ripgrep #3376 the feedback calls for a causal hypothesis, regression test, and targeted verification. A bounded workflow should reserve time to implement and run the most relevant check.
5. **Improve plans and tool-level error guidance.** tm's mean plan-quality score was 2.2/5 versus OpenCode's 4.0. In ky #878, the feedback specifically recommends a short explicit plan and actionable malformed-input errors at the tool-call level. State the next steps up front and explain how to correct failed tool input.
