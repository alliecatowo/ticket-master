# Trial scoreboard

Six paired issue-fixing trials were scored. Scores are on a 1–5 scale; higher is better. “Pass” is the trial result recorded in each JSON. Tokens and wall time are the recorded run totals. Medians are across the six trials.

## Results by tool

| Tool | Pass rate | Navigation quality (mean) | Context efficiency (mean) | Plan quality (mean) | Verification honesty (mean) | UX copy clarity (mean) | Recoverability (mean) | Cost/time (mean) | Median wall time | Median tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| tm | 1/6 (16.7%) | 2.00 | 1.17 | 1.17 | 2.50 | 1.83 | 1.33 | 1.00 | 103.2 sec | 398,933 |
| OpenCode | 3/6 (50.0%) | 4.00 | 3.83 | 4.17 | 4.00 | 4.33 | 3.50 | 4.67 | 61.0 sec | 26,054 |

## tm vs OpenCode, trial by trial

| Trial | tm result | OpenCode result | Score comparison (tm vs OpenCode, mean across dimensions) | What happened |
|---|---|---|---:|---|
| [ripgrep #3376](https://github.com/BurntSushi/ripgrep/issues/3376) | Pass · 124.8 sec · 489,009 tokens | Pass · 57.2 sec · 27,997 tokens | 1.71 vs 3.29 | Neither tool produced a fix. tm found the subsystem but repeated inspections, exhausted three attempts, and escalated; OpenCode also explored without reproducing the issue. |
| [Cobra #2257](https://github.com/spf13/cobra/issues/2257) | Fail · 103.2 sec · 513,981 tokens | Pass · 43.3 sec · 159,242 tokens | 1.29 vs 4.71 | tm stopped after repeated inspection without a change. OpenCode fixed the shared backing-array append, added regression coverage, and passed. |
| [Click #3822](https://github.com/pallets/click/issues/3822) | Fail · 99.1 sec · 345,682 tokens | Fail · 59.8 sec · 183,665 tokens | 1.14 vs 4.43 | tm did not make a change. OpenCode implemented the typing change, but its fix still failed the hidden typing test. |
| [ky #878](https://github.com/sindresorhus/ky/issues/878) | Fail · 70.0 sec · 313,527 tokens | Fail · 86.0 sec · 21,792 tokens | 2.57 vs 4.00 | tm found and made the flush change, but its evidence and verification were problematic. OpenCode added a focused fix and test; full-suite verification was blocked by environment/setup issues. |
| [Requests #7432](https://github.com/psf/requests/issues/7432) | Fail · 103.3 sec · 141,497 tokens | Pass · 62.2 sec · 20,030 tokens | 1.71 vs 3.86 | tm repeatedly revisited the same code and made no change. OpenCode implemented duck-typed detection; its own test ran against the wrong checkout, but the independent hidden test passed. |
| [Hugo #15360](https://github.com/gohugoio/hugo/issues/15360) | Fail · 159.0 sec · 452,183 tokens | Fail · 92.0 sec · 24,111 tokens | 1.14 vs 4.14 | tm repeated decoder/history inspections and made no change. OpenCode added BOM handling and focused regression cases; the exact merged test set could not compile against the base checkout. |

## Five biggest improvements for tm

1. **Break out of repeated, no-progress investigation loops.** Several runs reread or searched the same files across three attempts without implementing anything. Detect when an attempt is repeating prior work, preserve what was learned, and move to a concrete edit or focused reproduction. This is the clearest pattern in Cobra #2257, Click #3822, Requests #7432, and Hugo #15360; it also appeared in ripgrep #3376.
2. **Spend far fewer tokens and time before taking action.** tm used a median 398,933 tokens versus OpenCode’s 26,054, and took a median 103.2 seconds versus 61.0. Set a tighter exploration budget, avoid rereading unchanged context, and use an early test or reproduction to guide investigation. The largest examples include Cobra #2257 (513,981 tokens) and ripgrep #3376 (489,009 tokens).
3. **State a short plan, then implement and test the issue directly.** The repeated no-change trials scored poorly on planning. Turn the issue’s reproduction into a focused test, name the likely code path and intended change, then make the smallest fix. Cobra #2257 and Requests #7432 both found relevant code but did not change it; ripgrep #3376 likewise never tested the reported behavior.
4. **Make verification prove the actual patch.** Run the relevant regression test after editing, distinguish focused checks from full-suite checks, and be explicit when environment problems prevent a valid run. tm sometimes made a change without useful evidence (ky #878), while several other runs never reached a test at all (Hugo #15360, Requests #7432). The successful post-checkout or hidden checks described in the notes are not a substitute for verifying tm’s own patch.
5. **Give one accurate, actionable recovery instruction when retrying or escalating.** Messages sometimes told users both to resume and to wait for an automatic retry, or gave a session-resume command despite the ticket ending with no retry scheduled. Keep the final status and next command consistent with the actual run state, and summarize the finding so the user does not have to repeat discovery. This was especially clear in ripgrep #3376, Click #3822, ky #878, and Hugo #15360.

*Source: the six top-level JSON trial results in this directory. Per-dimension means and cross-trial medians are calculated from the recorded arm scores and metrics.*
