# TUI showcase

One continuous, fully live `tm` session — every keystroke on camera, every model answer,
tool call, and ticket transition real. The binary is `tm 0.1.0` at the showcase commit,
driven against a scratch project with the live configured provider
(`devpass/muse-spark-1.3-contributor`).

## What "live" means here, precisely

On camera (in [`tui-live.cast`](tui-live.cast), 2½ minutes): welcome screen, `@`-mention
with Tab completion, a tool-using chat turn, the tickets screen, peeking and accepting a
submission, dispatching a new ticket, watching its background worker verify, `/cost`, exit.

Prepared just before (via plain CLI, not on camera): the scratch project's two hand-written
files (`calc.py`, `tests/test_calc.py`), two tickets (`tm ticket new` + `activate`), and
one real foreground worker run (`tm run T-2`) that left T-2 `submitted`. Nothing on camera
is rehearsed or faked — including the worker's mid-recording `doctest`/`pytest` run and the
fact that T-3 was still `working` when the 5-minute recorder cap hit.

## Playback

- Full session: [`tui-live.cast`](tui-live.cast) — `asciinema play docs/showcase/tui-live.cast`.
- GIFs below are excerpts cut with `agg --select`; `.webm` files are the same clips
  encoded with `ffmpeg` (VP9).

## Gallery

### Live turn: mention, read, repro, answer

![Live turn: @calc.py completion, file reads, live crash repro, ticket-aware answer](tui-live.gif)

Cast `3s..50s`. `@calc` opens file completion, `Tab` inserts `@calc.py`, the question goes
out, and the turn really works: `Read(calc.py)`, reads of both tickets, a `Bash` repro that
runs `div(1,0)` and `pytest`, then a ticket-aware answer. Same clip:
[`tui-live.webm`](tui-live.webm).

### Review: peek a submission, accept it

![Tickets screen: peek T-2's submission, press 1, it closes](tui-accept.gif)

Cast `50s..70s`. `/tickets` shows T-1 working (lease lapsed) and T-2 ready for review;
`Space` peeks the submission, `1` accepts it, T-2 lands in Completed. Same clip:
[`tui-accept.webm`](tui-accept.webm).

### Dispatch: a new ticket gets worked in the background

![Dispatch T-3, watch its worker run doctest and pytest live in the row](tui-dispatch.gif)

Cast `64s..102s`. A new ticket is typed into the dispatch input and handed to a background
worker; back in the chat for `/cost`, then back to watch T-3's row stream its worker's
live verification command. Same clip: [`tui-dispatch.webm`](tui-dispatch.webm).

## Beat sheet (cast timestamps)

| Time | Beat |
| --- | --- |
| 4s | Welcome screen (`/help`, `/status` hints, cwd, model) |
| 10s | `@calc` file completion, `Tab` completes `@calc.py` |
| 22s | Question sent; `Read(calc.py)`, `Ticket(T-1)`, `Ticket(T-2)` |
| 41s | `Bash` repro: `div(1,0)` traceback + `pytest` |
| 46s | Ticket-aware answer streams in |
| 51s | `/tickets`: T-1 working, T-2 ready for review |
| 58s | `Space` peeks T-2's submission (accept/reject) |
| 63s | `1` accepts — T-2 closes into Completed |
| 72s | T-3 dispatched from the dispatch input |
| 74s | T-3 `working on attempt 1` seconds later |
| ~100s | T-3's row streams `doctest` + `pytest` verification |
| ~115s | `/cost` back in the chat while the worker runs |
| 141s | `/exit` quits cleanly |

## Reproduction

```sh
mise run build
export TM_DEMO="$(mktemp -d /tmp/tm-demo-XXXXXX)"
./target/debug/tm init --fresh "$TM_DEMO"
# put a couple of real files in $TM_DEMO to ask about, optionally create +
# activate a ticket or two with tm ticket new / tm ticket activate ...
# Start the recorder BEFORE launching the TUI, keep sibling sessions idle:
./target/debug/tm --project "$TM_DEMO"
```

Convert with `agg --idle-time-limit 2 --fps-cap 10 --font-size 14 --select A..B in.cast out.gif`.

## Friction found on camera

1. **T-3 outlived the recording.** The 5-minute recorder cap hit with T-3 still `working`,
   so no on-camera completion for the dispatched ticket — the footage ends mid-verification.
   A second take could wait it out, at the cost of provider time.
2. **The agent probes git in a non-git project.** The turn burned two failing tool calls
   (`git.status` 128, `git.diff` usage error) before doing the actual repro. Harmless here,
   but a cwd capability check could save the roundtrips.
3. **The answer slightly overstated T-1.** It called T-1 "(running)" when it was merely ready
   with a lapsed lease — ticket-state wording from the model, not the UI (the row itself
   reads "no worker attached; its lease lapsed").
4. **Prompt PII at the shell boundary.** After `/exit`, the shell prompt (hostname, username,
   email) is captured. The published cast is trimmed before the post-exit shell redraw;
   check with `grep -c gmail` before publishing.
5. **`startRecording` captures every PTY session in the process**, not just the TUI one —
   keep sibling sessions idle or their output lands in the same cast.
6. **Worker patches landed in the launcher cwd, not `--project`.** Both `tm --project SEED
   run T-2` (shell cwd = repo root) and T-3's in-TUI scheduler worker (TUI launched with
   `--project SEED`, process cwd = repo root) wrote `./calc.py` / `./tests/test_calc.py`
   into the repo root; the seed dir's `calc.py` is untouched. Chat-turn tools ran in the
   project dir correctly. Strays removed, no tracked files touched (verified via
   `git status`) — but launch worker-driving commands with cwd set to the project dir
   until this is fixed.
