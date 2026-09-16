//! The [`ToolRegistry`]: every tool from `SPEC.md` §11, its JSON schema, the
//! [`tm_types::Action`] it maps to for authority gating, its cost class, and its dispatch into
//! `tm-codeintel`, `tm-context`, `tm-core` or [`crate::patch::PatchEngine`].
//!
//! Dispatch never trusts the model: every call is mapped to an `Action` and checked against
//! `Authority::permits` *before* it touches any of those crates, a denial comes back as a
//! structured [`ToolOutcome::Denied`] result rather than an error the loop has to special-case,
//! and results are bounded — anything past [`MAX_INLINE_RESULT_BYTES`] is stored as an artifact
//! and referenced by id rather than inlined into the transcript.

use std::collections::BTreeMap;

use tm_codeintel::CodeIntel;
use tm_context::command::{CommandCache, CommandExecutor};
use tm_core::Store;
use tm_types::{Action, ArtifactId, Authority, Clock, IdSource, ParticipantId, Result, SessionId, TicketId};

use crate::patch::PatchEngine;

/// Bytes past which a tool result is spilled to an artifact and referenced by id instead of
/// being inlined into the model-visible transcript.
pub const MAX_INLINE_RESULT_BYTES: usize = 8 * 1024;

/// One tool name from `SPEC.md` §11's fixed catalog. Variant names mirror the spec's dotted
/// tool names (`Search*` = `search.*`, etc.) so [`ToolName::as_str`] round-trips exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolName {
    /// `search.semantic`
    SearchSemantic,
    /// `search.exact`
    SearchExact,
    /// `search.regex`
    SearchRegex,
    /// `search.hybrid`
    SearchHybrid,
    /// `symbol.definition`
    SymbolDefinition,
    /// `symbol.references`
    SymbolReferences,
    /// `symbol.callers`
    SymbolCallers,
    /// `symbol.callees`
    SymbolCallees,
    /// `symbol.outline`
    SymbolOutline,
    /// `symbol.rename_preview`
    SymbolRenamePreview,
    /// `history.why`
    HistoryWhy,
    /// `history.search`
    HistorySearch,
    /// `history.deleted`
    HistoryDeleted,
    /// `fs.read`
    FsRead,
    /// `fs.read_range`
    FsReadRange,
    /// `fs.list`
    FsList,
    /// `fs.stat`
    FsStat,
    /// `edit.apply_patch`
    EditApplyPatch,
    /// `edit.write_file`
    EditWriteFile,
    /// `edit.create_file`
    EditCreateFile,
    /// `edit.delete_file`
    EditDeleteFile,
    /// `shell.run`
    ShellRun,
    /// `shell.query_output`
    ShellQueryOutput,
    /// `git.status`
    GitStatus,
    /// `git.diff`
    GitDiff,
    /// `git.log`
    GitLog,
    /// `git.commit`
    GitCommit,
    /// `git.branch`
    GitBranch,
    /// `git.worktree`
    GitWorktree,
    /// `test.run`
    TestRun,
    /// `build.run`
    BuildRun,
    /// `ticket.create_child`
    TicketCreateChild,
    /// `ticket.delegate`
    TicketDelegate,
    /// `ticket.submit`
    TicketSubmit,
    /// `ticket.comment`
    TicketComment,
    /// `decision.record`
    DecisionRecord,
    /// `artifact.store`
    ArtifactStore,
    /// `evidence.attach`
    EvidenceAttach,
    /// `ask.human`
    AskHuman,
}

impl ToolName {
    /// Every tool name, in `SPEC.md` §11's declared order.
    pub const ALL: &'static [ToolName] = &[
        ToolName::SearchSemantic,
        ToolName::SearchExact,
        ToolName::SearchRegex,
        ToolName::SearchHybrid,
        ToolName::SymbolDefinition,
        ToolName::SymbolReferences,
        ToolName::SymbolCallers,
        ToolName::SymbolCallees,
        ToolName::SymbolOutline,
        ToolName::SymbolRenamePreview,
        ToolName::HistoryWhy,
        ToolName::HistorySearch,
        ToolName::HistoryDeleted,
        ToolName::FsRead,
        ToolName::FsReadRange,
        ToolName::FsList,
        ToolName::FsStat,
        ToolName::EditApplyPatch,
        ToolName::EditWriteFile,
        ToolName::EditCreateFile,
        ToolName::EditDeleteFile,
        ToolName::ShellRun,
        ToolName::ShellQueryOutput,
        ToolName::GitStatus,
        ToolName::GitDiff,
        ToolName::GitLog,
        ToolName::GitCommit,
        ToolName::GitBranch,
        ToolName::GitWorktree,
        ToolName::TestRun,
        ToolName::BuildRun,
        ToolName::TicketCreateChild,
        ToolName::TicketDelegate,
        ToolName::TicketSubmit,
        ToolName::TicketComment,
        ToolName::DecisionRecord,
        ToolName::ArtifactStore,
        ToolName::EvidenceAttach,
        ToolName::AskHuman,
    ];

    /// The dotted wire name, e.g. `"search.semantic"`.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolName::SearchSemantic => "search.semantic",
            ToolName::SearchExact => "search.exact",
            ToolName::SearchRegex => "search.regex",
            ToolName::SearchHybrid => "search.hybrid",
            ToolName::SymbolDefinition => "symbol.definition",
            ToolName::SymbolReferences => "symbol.references",
            ToolName::SymbolCallers => "symbol.callers",
            ToolName::SymbolCallees => "symbol.callees",
            ToolName::SymbolOutline => "symbol.outline",
            ToolName::SymbolRenamePreview => "symbol.rename_preview",
            ToolName::HistoryWhy => "history.why",
            ToolName::HistorySearch => "history.search",
            ToolName::HistoryDeleted => "history.deleted",
            ToolName::FsRead => "fs.read",
            ToolName::FsReadRange => "fs.read_range",
            ToolName::FsList => "fs.list",
            ToolName::FsStat => "fs.stat",
            ToolName::EditApplyPatch => "edit.apply_patch",
            ToolName::EditWriteFile => "edit.write_file",
            ToolName::EditCreateFile => "edit.create_file",
            ToolName::EditDeleteFile => "edit.delete_file",
            ToolName::ShellRun => "shell.run",
            ToolName::ShellQueryOutput => "shell.query_output",
            ToolName::GitStatus => "git.status",
            ToolName::GitDiff => "git.diff",
            ToolName::GitLog => "git.log",
            ToolName::GitCommit => "git.commit",
            ToolName::GitBranch => "git.branch",
            ToolName::GitWorktree => "git.worktree",
            ToolName::TestRun => "test.run",
            ToolName::BuildRun => "build.run",
            ToolName::TicketCreateChild => "ticket.create_child",
            ToolName::TicketDelegate => "ticket.delegate",
            ToolName::TicketSubmit => "ticket.submit",
            ToolName::TicketComment => "ticket.comment",
            ToolName::DecisionRecord => "decision.record",
            ToolName::ArtifactStore => "artifact.store",
            ToolName::EvidenceAttach => "evidence.attach",
            ToolName::AskHuman => "ask.human",
        }
    }

    /// Parse a dotted wire name back into a [`ToolName`].
    pub fn parse(name: &str) -> Option<ToolName> {
        ToolName::ALL.iter().copied().find(|t| t.as_str() == name)
    }
}

/// A coarse cost tier, used by [`crate::agent_loop::AgentLoop`] to charge
/// [`tm_types::Budget`] spend for a tool call before it dispatches (provider calls are metered
/// by `tm_provider::Usage`; tool calls need their own estimate since most never touch a
/// provider).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CostClass {
    /// Pure in-memory read, effectively free (`fs.stat`, `symbol.outline`, ...).
    Free,
    /// A bounded local read or index query (`fs.read`, `search.exact`, ...).
    Cheap,
    /// A recomputation over the codebase or a subprocess (`search.semantic`, `build.run`, ...).
    Moderate,
    /// A write, a shell command with side effects, or anything that mutates project state.
    Mutating,
    /// Suspends the loop for external (human) input.
    Blocking,
}

/// One registered tool: its schema, gating and cost metadata.
pub struct ToolSpec {
    /// This tool's name.
    pub name: ToolName,
    /// Shown to the model to decide when to call this tool.
    pub description: &'static str,
    /// JSON Schema for this tool's input.
    pub input_schema: serde_json::Value,
    /// This tool's cost tier.
    pub cost: CostClass,
    /// Map a call's parsed input to the [`Action`] `Authority::permits` is checked against.
    ///
    /// A function pointer (not a closure) so every mapping is a pure, inspectable, unit-testable
    /// function of the call's arguments alone — it never reaches into ambient state.
    pub to_action: fn(&serde_json::Value) -> Result<Action>,
}

/// The full catalog of tools an [`crate::agent_loop::AgentLoop`] offers a provider.
pub struct ToolRegistry {
    specs: BTreeMap<ToolName, ToolSpec>,
}

impl ToolRegistry {
    /// Build the standard registry: one [`ToolSpec`] per [`ToolName::ALL`] entry, per
    /// `SPEC.md` §11.
    // IMPL: for each `ToolName` in `ToolName::ALL`, construct a `ToolSpec` with: a JSON Schema
    // matching that tool's real parameters (e.g. `fs.read` takes `{path: string}`,
    // `edit.apply_patch` takes `{path, edits: [...], expected_hash}`, `shell.run` takes
    // `{argv: [string], cwd, cacheable}`, `ticket.create_child` takes the subset of
    // `tm_core::Store::create_ticket`'s parameters an agent may set); a `to_action` function
    // pointer mapping to `Action::ReadPath` for every `search.*`/`symbol.*`/`history.*`/`fs.*`
    // tool, `Action::WritePath` for every `edit.*` tool, `Action::RunCommand` for
    // `shell.*`/`test.run`/`build.run`/`git.status`/`git.diff`/`git.log`, `Action::Git` for
    // `git.commit`/`git.branch`/`git.worktree`, `Action::Ticket` for `ticket.*`,
    // `Action::Project` for none (agents don't get project-authority tools), and a bespoke
    // always-`Action::Ticket{op: TicketOp::Delegate}`-shaped check for `ask.human` (it delegates
    // control to a human); and a `CostClass` per the doc comment on that enum. Each mapping fn
    // must parse only the fields it needs from the `serde_json::Value` and return
    // `TmError::parse` on a malformed call rather than panicking.
    pub fn standard() -> Self {
        todo!("build one ToolSpec per ToolName::ALL entry with a real schema, action mapping and cost class, per the IMPL note")
    }

    /// Look up a tool by its dotted wire name.
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        ToolName::parse(name).and_then(|t| self.specs.get(&t))
    }

    /// Every registered spec, in [`ToolName::ALL`] order.
    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.specs.values()
    }

    /// Render every registered tool as a `tm_provider::ToolDef`, for
    /// `tm_provider::CompletionRequest::tools`.
    pub fn tool_defs(&self) -> Vec<tm_provider::ToolDef> {
        self.specs
            .values()
            .map(|spec| tm_provider::ToolDef {
                name: spec.name.as_str().to_string(),
                description: spec.description.to_string(),
                input_schema: spec.input_schema.clone(),
            })
            .collect()
    }

    /// Dispatch one model-issued call: resolve its tool, map to an [`Action`], check `authority`,
    /// and — if permitted — execute against `ctx`.
    ///
    /// Never returns `Err`: an unknown tool name, a malformed input, an authority denial, or a
    /// dispatch-time failure are all represented as a variant of [`ToolOutcome`] so the caller
    /// always has a tool result to hand back to the model.
    // IMPL: (1) `ToolName::parse(call.name)`; unknown -> `ToolOutcome::Errored`. (2) look up the
    // `ToolSpec`; call `(spec.to_action)(&call.input)`; parse failure -> `ToolOutcome::Errored`.
    // (3) `ctx.authority.permits(&action)`; `Decision::Deny(reason)` -> `ToolOutcome::Denied`;
    // `Decision::NeedsApproval(reason)` -> the caller (`AgentLoop`) is responsible for turning
    // this into `AgentOutcome::AwaitingApproval` *before* calling dispatch again, so `dispatch`
    // itself only ever sees `Allow` here — treat `NeedsApproval` reaching this point as a
    // caller bug and return `ToolOutcome::Errored`. (4) on `Allow`, match on the parsed
    // `ToolName` and call into `ctx.ci`/`ctx.store`/`ctx.patch_engine`/
    // `ctx.command_cache`+`ctx.command_executor` as appropriate, then run the result through
    // `bound_result` before wrapping it in `ToolOutcome::Completed`.
    pub fn dispatch(&self, call: &ToolCall, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        todo!("resolve tool, map to Action, check authority, dispatch into the owning crate, bound the result, per the IMPL note")
    }
}

/// One model-issued tool call, translated from a `tm_provider::ContentBlock::ToolUse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The provider-assigned id correlating this call to its result.
    pub id: String,
    /// The tool name called.
    pub name: String,
    /// The call's arguments.
    pub input: serde_json::Value,
}

/// The outcome of one dispatched [`ToolCall`]; shared with the transcript shape in
/// [`crate::outcome::StepRecord`].
pub type ToolOutcome = crate::outcome::ToolCallResolution;

/// Everything [`ToolRegistry::dispatch`] needs to actually execute a call, borrowed for the
/// duration of one dispatch.
pub struct ToolContext<'a> {
    /// The authority every call is checked against.
    pub authority: &'a Authority,
    /// Code intelligence facade for `search.*`/`symbol.*`/`history.*` tools.
    pub ci: &'a CodeIntel,
    /// Project state store for `ticket.*`/`decision.record`/`artifact.store`/`evidence.attach`.
    pub store: &'a Store,
    /// Patch engine for `edit.*` tools.
    pub patch_engine: &'a PatchEngine,
    /// Command result cache for `shell.*`/`test.run`/`build.run`/read-only `git.*` tools.
    pub command_cache: &'a dyn CommandCache,
    /// Command executor for the same command-shaped tools.
    pub command_executor: &'a dyn CommandExecutor,
    /// Injected clock; dispatch never reads the wall clock directly.
    pub clock: &'a dyn Clock,
    /// Injected id source; dispatch never mints ids itself.
    pub ids: &'a dyn IdSource,
    /// The ticket this dispatch happens on behalf of.
    pub ticket: &'a TicketId,
    /// The session this dispatch happens inside.
    pub session: &'a SessionId,
    /// Who/what is issuing this call, for event/evidence attribution.
    pub actor: &'a ParticipantId,
}

/// Bound a raw tool result: if `value`'s serialized size is at or under
/// [`MAX_INLINE_RESULT_BYTES`], return it unchanged with no artifact; otherwise store the full
/// value as an artifact via `ctx.store` and return a truncated preview referencing it.
// IMPL: serialize `value` with `serde_json::to_vec`; under the limit, return `(value, None)`
// unchanged. Over the limit, call `ctx.store.store_artifact` (kind `Report` or `CommandOutput`
// as appropriate to the caller) with the full bytes, and return a small JSON object
// `{"truncated": true, "artifact": "<id>", "preview": "<first N bytes as text>"}` alongside
// `Some(artifact_id)`.
fn bound_result(
    value: serde_json::Value,
    ctx: &mut ToolContext<'_>,
) -> Result<(serde_json::Value, Option<ArtifactId>)> {
    todo!("inline small results verbatim; spill large ones to a stored artifact and return a bounded preview referencing it, per the IMPL note")
}
