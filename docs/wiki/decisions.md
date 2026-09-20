+++
[doc]
id = "wiki/decisions"
mode = "generated"
derived_from = ["docs/decisions/**"]
+++

# Decisions

| Decision | Title | Status |
|---|---|---|
| [D-001](decisions/D-001-executor-strategy.md) | Build a minimal runtime, and make other harnesses first-class executors | accepted |
| [D-002](decisions/D-002-terminal-ui-stack.md) | The terminal UI stack | accepted |
| [D-003](decisions/D-003-project-scope.md) | Project scope: workspace vs. state, and where bare `tm` writes | accepted |
| [D-004](decisions/D-004-acp-wire-framing.md) | ACP wire framing, and how `tm-acp` plugs into the executor registry | accepted |
| [D-005](decisions/D-005-devpass-default-provider.md) | DevPass as the default provider for interactive turns | accepted |
| [D-006](decisions/D-006-tui-navigation-shell.md) | A real navigation shell and Kanban board for the TUI | accepted |
| [D-007](decisions/D-007-desktop-notifications.md) | Desktop notifications on `approval.requested`/`ticket.escalated` | accepted |
| [D-008](decisions/D-008-ticket-checkpoint-fork.md) | Ticket checkpointing: `tm ticket fork <T> --at <seq>` and per-turn workspace snapshots | accepted |
| [D-009](decisions/D-009-oversight-policy-wiring.md) | `oversight.toml`: loading and wiring `Oversight::review` at a real effect boundary | accepted |
| [D-010](decisions/D-010-opentelemetry-tracing.md) | OpenTelemetry tracing export, opt-in behind `otel` | accepted |
| [D-011](decisions/D-011-secret-redaction.md) | Secret-shaped-substring redaction at `Fabric::execute`, `Store::append` and `Store::store_artifact` | accepted |
| [D-012](decisions/D-012-run-worktree-isolation.md) | `tm run <ticket> --worktree`: per-run `git worktree` isolation | accepted |
| [D-013](decisions/D-013-hooks-agents-skills.md) | AGENTS.md conventions, SKILL.md progressive disclosure, and shell-only hooks.toml | accepted |
| [D-014](decisions/D-014-pattern-subset-double-star-fix.md) | `PatternSet::is_subset_of` wrongly approved escapes through `**` | accepted |
