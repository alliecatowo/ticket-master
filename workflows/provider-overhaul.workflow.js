export const meta = {
  name: "ticket-master-provider-overhaul",
  description: "Four isolated Luna implementation tracks, one serialized integration build, a blind CLI trial, and a final repair and verification pass.",
  phases: ["implement", "integrate", "repair", "blind-trial", "final-gate"],
  version: "1.0.0"
};

// Worktrees are created explicitly BEFORE invoking this workflow. ODW's cwd is not
// worktree isolation. Its opencode adapter is configured for --auto so autonomous
// edits are possible ONLY inside the four worktree paths below. No secrets are
// copied into implementation worktrees; no agent may run tm against a checkout.
// Run with --concurrency 2: four model workers, zero Rust builds in their tracks.
// The integrator alone builds (mise run verify, -j 2). Never share CARGO_TARGET_DIR.

const root = "/Users/allie/Develop/ticket-master";
const base = `${root}/.claude/worktrees`;
const luna = "openai/gpt-6-luna";
const policy = `First confirm git rev-parse --show-toplevel equals your assigned cwd. Stop if not. This is a separate worktree; never edit the primary checkout. Do not read .env or print secrets. Never run the compiled tm binary against a checkout: use a new mktemp project and launch from THAT temp project directory; avoid all live provider calls in an implementation track. Never share CARGO_TARGET_DIR. Do not build or run mise/cargo in implementation tracks (RAM/disk: central integrator owns builds). Only edit your assigned files. Before committing inspect git status, diff and recent log; stage only intended files. Make one or more focused commits; report the final commit SHA. Do a skeptical self-review and fix issues before reporting. Do not push or tag. Consult CLAUDE.md and docs/decisions/D-022-unified-provider-model-config-ux.md. Treat tracked HELLO*.md as other people's work. Return exactly one JSON object with commit (40-character SHA), summary, files (array), and risks (array).`;

const report = {
  type: "object",
  properties: {
    commit: { type: "string" },
    summary: { type: "string" },
    files: { type: "array", items: { type: "string" } },
    risks: { type: "array", items: { type: "string" } }
  },
  required: ["commit", "summary", "files", "risks"]
};

export default async () => {
  phase("implement");

  const tracks = await parallel({
    provider: () => agent({
      id: "provider-registry-router",
      provider: "opencode", model: luna, cwd: `${base}/odw-provider`,
      timeoutMs: 2400000, retry: false,
      schema: report, structuredOutput: { transport: "auto" },
      prompt: `${policy}\nYour ONLY owned paths: crates/tm-provider/src/** (tests there too). Audit finding priorities: fix alternative env aliases for Gemini GOOGLE_API_KEY and GitHub Models GITHUB_MODELS_TOKEN; reject unconstructible Bedrock as ready; local server absent/empty/populated must be distinguished from configured/ready; never suggest 'default' as a local model; expose actual pulled local models via cheap probe where appropriate; strict validation for unknown slugs/typoed candidate policy; route tolerances and degraded_ok must have honest semantics; do not offer a fake Anthropic embedder. Preserve existing API where possible. Add meaningful pure tests and mock local-server tests without real network credentials. Never edit tm-cli or docs outside your paths. Commit. Describe any interface changes needed by integrator.`
    }),
    config: () => agent({
      id: "project-config-persistence",
      provider: "opencode", model: luna, cwd: `${base}/odw-config`,
      timeoutMs: 2400000, retry: false,
      schema: report, structuredOutput: { transport: "auto" },
      prompt: `${policy}\nYour ONLY owned paths: crates/tm-cli/src/project.rs, ops.rs, args.rs, tui/config_cmd.rs (and new config helpers under crates/tm-cli/src/ only if named without overlapping other tracks). Fix the dual-schema bug: harness.toml exclusively HarnessConfig; providers.toml exclusively RoleTable, legacy role-shaped harness.toml read-only fallback if feasible. tm init scaffolds both *complete valid defaults*, never overwrites existing user files. tm harness show uses defaults when absent; harness set must persist the validated change durably and truthfully so promote reads the new value, with correct next_epoch and no lost update. Wire provider list/status/test to the same effective project routing, with human-friendly errors (no 'Anthropic auth failed' for an Ollama-only user) and a CLI provider default get/set/clear if supported by args. Config dashboard/get/set must tell truth and use exactly the same loader/persistence as CLI. Define a public(crate) load_role_table(project: Option<&Project>) interface unchanged for chat, and coordinate with integrator on any API used by the chat track. Do not edit agent.rs, chat_ops.rs, dispatch.rs, tm-provider, tm-tui, or docs. Add temp-project end-to-end tests. Commit.`
    }),
    chat: () => agent({
      id: "model-session-tiers",
      provider: "opencode", model: luna, cwd: `${base}/odw-chat`,
      timeoutMs: 2400000, retry: false,
      schema: report, structuredOutput: { transport: "auto" },
      prompt: `${policy}\nYour ONLY owned paths: crates/tm-cli/src/agent.rs, tui/chat_ops.rs, tui.rs, crates/tm-tui/src/chat/**, crates/tm-tui/src/screens/chat.rs and screens/chat/**. Fix model UX: /model lists every configured, actually routable provider/model (not Anthropic-only); unconfigured models lead to /connect not a cryptic auth error; /model provider/model with slashes inside model IDs works; current displayed model must match the effective routed default (fabric.prefer(None) must NOT reset to dead Anthropic primary); session model choice persists even before the first turn; project-scoped default persists for new sessions (provide pub(crate) load_default_model and save_default_model functions for integration); /level fast|deep provides honest role/task tier selection with session persistence. Expose a build_fabric_for_project(project,clock) core that reads crate::ops::load_role_table(Some(project)) so turns and dispatch respect project providers.toml; keep existing build_fabric(clock) for legacy tests and callers. Do not edit ops.rs, args.rs, project.rs, tm-provider, dispatch.rs or docs. Add tests asserting shown model equals one the registered provider can actually serve; no network. Commit.`
    }),
    dispatch: () => agent({
      id: "dispatch-root-epochs-color",
      provider: "opencode", model: luna, cwd: `${base}/odw-dispatch`,
      timeoutMs: 2400000, retry: false,
      schema: report, structuredOutput: { transport: "auto" },
      prompt: `${policy}\nYour ONLY owned paths: crates/tm-cli/src/dispatch.rs, sched.rs, render.rs, crates/tm-scheduler/src/dispatch.rs, and tests adjacent to those files. CRITICAL: worker AgentLoop root must always be project.root or explicitly selected worktree root, never process cwd when --project targets elsewhere; write a regression test using an external temp project with launcher cwd elsewhere, never touch repo state. Respect promoted harness epoch where scheduler and chat can take it (coordinate integration for agent.rs owned elsewhere). Fix --role ignored behavior honestly: implement with authority and traceability or reject explicitly rather than silently ignoring. CLI color: style human table headers and status on TTY, respect NO_COLOR and CLICOLOR; JSON and pipes must stay escape-free. Do not edit ops.rs, agent.rs, project.rs, tm-provider, tm-tui or docs. No builds here. Commit.`
    })
  });

  const commits = [tracks.provider, tracks.config, tracks.chat, tracks.dispatch]
    .map((r) => r.json?.commit);
  if (commits.some((c) => typeof c !== "string" || !/^[0-9a-f]{40}$/.test(c))) {
    throw new Error("An implementation track did not provide a commit SHA; inspect artifacts before integration.");
  }
  context.set("tracks", { commits, risks: [tracks.provider.json.risks, tracks.config.json.risks, tracks.chat.json.risks, tracks.dispatch.json.risks] });

  phase("integrate");
  const integration = await agent({
    id: "integrate-and-verify",
    provider: "opencode", model: luna, cwd: `${base}/odw-integrate`,
    timeoutMs: 5400000, retry: false,
    schema: report, structuredOutput: { transport: "auto" },
    prompt: `You are the ONLY build worker. Confirm git rev-parse --show-toplevel equals ${base}/odw-integrate; stop otherwise. Work only here, never in primary. Cherry-pick these four commits in order: ${commits.join(" ")}. Read and resolve interface conflicts by following D-022 and the deep audit; check that cli/app/scheduler paths use ONE effective project role-table loader and that fresh init -> config set -> promote -> new session -> background ticket follows persisted settings. Fix route table and status discrepancies, absent local backend/default model failure, API clashes, clippy and test failures. Run 'mise run fmt' then 'mise run verify' (only in this worktree, -j 2, no shared CARGO_TARGET_DIR); fix failures and rerun verify until green. No live provider calls. Test new CLI --help and safe commands ONLY against fresh mktemp root, never this checkout. Update docs/providers.md, D-022, README, CLAUDE.md to match reality. Adversarially review and commit integration fixes. Do not push/tag/release. No secrets in logs. Return exactly one JSON object with commit (final SHA), summary, files (array), and risks (array); if blocked, say so under risks rather than claiming green.`
  });

  phase("repair");
  const repair = await agent({
    id: "skeptical-integration-repair",
    provider: "opencode", model: luna, cwd: `${base}/odw-integrate`,
    timeoutMs: 3600000, retry: false,
    schema: report, structuredOutput: { transport: "auto" },
    prompt: `This is a second, distrustful pass over the integrated checkout ${base}/odw-integrate at ${integration.json?.commit}. Confirm toplevel path before any write. Inspect diff from base 97b105c, verify the integration agent actually ran a clean 'mise run verify' and did not mask failures. Challenge defaults, local availability, 'configured' vs usable, alias credentials, model picker order, session persistence, epoch pinning, working directory, GUI picker/help and no-provider guidance. Fix actual bugs and add meaningful tests. Run 'mise run verify' once after fixes, rerun only if fixes/tests fail. Keep to this integration checkout. Do not push/tag/release. Commit fixes, return final SHA, summary, files, risks as JSON. If no fixes, report the existing HEAD SHA.`
  });

  phase("blind-trial");
  const trial = await agent({
    id: "naive-user-provider-trial",
    provider: "opencode", model: luna, cwd: `${base}/odw-integrate`,
    timeoutMs: 1800000, retry: false,
    schema: {
      type: "object",
      properties: {
        verdict: { type: "string" },
        steps: { type: "array", items: { type: "string" } },
        friction: { type: "array", items: { type: "string" } }
      },
      required: ["verdict", "steps", "friction"]
    }, structuredOutput: { transport: "auto" },
    prompt: `Act as a FIRST-TIME USER. You do not know this codebase. Confirm toplevel is ${base}/odw-integrate; do NOT edit code or docs or git. First read only the root README and tm --help. Use a FRESH mktemp directory OUTSIDE EVERY checkout for all tm commands, run the binary with process cwd set to that temp dir (project root and process cwd must match), never against a checkout. Test tm init, provider detect/list/status/auth, setting a default and tiers, changing models between at least two ACTUALLY CONFIGURED providers if credentials are available, and ticket dispatch workflow. Do not invent success or ask for secrets. Avoid real network if credentials are absent; report actual availability and state precisely. No token/secret values in output. Report every confusing message verbatim and reproduction steps, especially Anthropic errors when another provider is selected, mismatch between shown and answering model, inability to change default or role. Return only JSON verdict/steps/friction; never write to a repo checkout.`
  });

  phase("final-gate");
  const final = await agent({
    id: "naive-feedback-final-gate",
    provider: "opencode", model: luna, cwd: `${base}/odw-integrate`,
    timeoutMs: 3600000, retry: false,
    schema: report, structuredOutput: { transport: "auto" },
    prompt: `Confirm git toplevel ${base}/odw-integrate; stop if not. New user's blind trial: ${JSON.stringify(trial.json?.friction ?? [])}. Fix all reproducible P0/P1 friction, document honestly what's not supported, ensure no test fixture leaked into checkout. Run 'mise run verify' again AFTER all fixes. Do one final skeptical diff review for hardcoded providers, secret leakage, broken CLI/TUI docs, test coverage, and release readiness. Stage only intended changes, commit them; do NOT push/tag. Return final SHA, summary, files, risks as JSON.`
  });

  return {
    trackCommits: commits,
    integrationCommit: integration.json?.commit,
    reviewCommit: repair.json?.commit,
    blindTrial: trial.json,
    finalCommit: final.json?.commit,
    remainingRisks: final.json?.risks
  };
};
