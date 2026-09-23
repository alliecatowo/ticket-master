# D-021: Provider onboarding and config from the chat (`/connect`, `/provider`, `/config`)

Status: accepted
Date: 2026-09-23
Supersedes: none

## Context

Every provider operation lived outside the TUI: `tm auth <provider>` prints setup
instructions, `tm provider detect/list/test` inspects, `tm harness show/set` views and
proposes config — all from a shell. A chat session that answers through one provider had
no way to onboard a second one, check what answers it, or inspect project configuration
without leaving the TUI. The slash-command table (`tm-tui`) plus `run_command`
(`tm-cli/src/tui/chat_ops.rs`) is the established path for closing that gap: the screen
owns popups/pickers, the app owns effects, and `ChatAction::Command` already carries
arbitrary new commands with no screen changes beyond the table.

## Decision

Three commands, split by verb the way the CLI splits them:

- `/connect [provider]` (alias `/auth`) is the action: bare it opens a provider picker
  (a third `PickerPurpose`, reusing the `/model` picker's choose→`Command` round trip);
  named it validates the slug and renders `tm auth`'s instructions in-chat.
- `/provider` is the read: which `provider/model` answers this chat, which slugs have
  their env present, and how many more are known. No network probes, no billed calls.
- `/config [show|get|set]` is project configuration in-chat: a dashboard (root, scope,
  state dir, session, model, ticket, harness presence, epoch count, `coder.fast`
  routing), dotted-key `get`, and `set` through the exact validation path as
  `tm harness set`.

Deliberate non-goals: no secret entry or persistence in-chat (keys stay env vars, shown
as present/absent only, per the secret-redaction discipline); no reachability probes in
`/provider` (they would block the TUI thread); no new epoch-persistence semantics —
`/config set` mirrors `tm harness set` byte for byte, including its validate-only shape.

## Why

Discoverability: `/` lists everything runnable, so provider onboarding stops being tribal
knowledge. Safety: everything new is read-only or validation-only except ticket/dispatch
flows that already exist; the chat gains no way to exfiltrate or store a credential.
Symmetry: each command has a CLI twin with identical semantics (`tm auth`, `tm provider
list`, `tm harness show/set`), so docs describe behavior once.

## What this costs, stated plainly

- `tm harness set`'s validate-only shape (it never writes `harness.toml`; `promote`
  reads the candidate from disk) is now reachable from two surfaces instead of one.
  Documented in `/config set`'s own output rather than fixed here — fixing epoch
  persistence is a `tm-core` promotion-gate discussion, not a chat feature.
- The provider picker marks only env-presence, not reachability: a "configured" local
  backend with nothing listening still fails at turn time, exactly as `tm provider list`
  already reports it as `unreachable` with a probe the chat refuses to run.
- Three more rows in the `/` popup and one more picker purpose to maintain.
