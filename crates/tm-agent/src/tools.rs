//! The [`ToolRegistry`]: every tool from `SPEC.md` §11, its JSON schema, the
//! [`tm_types::Action`] it maps to for authority gating, its cost class, and its dispatch into
//! `tm-codeintel`, `tm-context`, `tm-core` or [`crate::patch::PatchEngine`].
//!
//! Dispatch never trusts the model: every call is mapped to an `Action` and checked against
//! `Authority::permits` *before* it touches any of those crates, a denial comes back as a
//! structured [`ToolOutcome::Denied`] result rather than an error the loop has to special-case,
//! and results are bounded — anything past [`MAX_INLINE_RESULT_BYTES`] is stored as an artifact
//! and referenced by id rather than inlined into the transcript.
//!
//! # Gaps in the closed `Action`/`TicketAuthority` vocabulary
//!
//! `tm_types::TicketOp` only names `create_children`, `delegate_children` and
//! `modify_siblings` as governed ticket-graph powers; there is no dedicated op for submitting
//! evidence, leaving a comment, recording a decision, storing an artifact or attaching
//! evidence. Since `Action`/`TicketOp` are closed enums this module cannot extend, every one of
//! those five tools (`ticket.submit`, `ticket.comment`, `decision.record`, `artifact.store`,
//! `evidence.attach`) is gated under `Action::Ticket { op: TicketOp::ModifySibling }` — the
//! closest existing governance switch, and a safe (denies-by-default) choice rather than an
//! exact semantic match. `ticket.delegate` similarly has a payload
//! (`tm_events::payload::TicketDelegatedPayload`) but no `Store` method that emits it, and
//! `ticket.comment` has no backing table at all; both are recorded through
//! `Store::update_ticket`'s documented "extra JSON fields" convention rather than inventing a
//! new write path into a finished crate.
//!
//! Similarly, `git.worktree` has no dedicated `GitOp`; it is gated as `GitOp::Branch`, the
//! nearest-blast-radius existing operation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use tm_codeintel::semantic::SemanticSearchOptions;
use tm_codeintel::{CodeIntel, Query, Reference, RetrievalContext, SignalWeights, Symbol};
use tm_context::command::{
    self, ArtifactStream, CommandCache, CommandExecutor, CommandSpec, Query as CommandQuery,
    QueryAnswer,
};
use tm_core::{
    ArtifactKind, ContextRef, EvidenceKind, ExecutorRequirements, RetryPolicy, Store, TicketKind,
    VerificationPolicy,
};
use tm_types::{
    Action, ArtifactId, Authority, Budget, Clock, GitOp, IdSource, ParticipantId, Predicate,
    Result, Role, SessionId, TicketId, TicketOp, TmError, Tolerance,
};

use crate::patch::{Edit, PatchEngine};

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

impl ToolSpec {
    /// Whether `authority` could possibly permit *any* call to this tool, admitting it into a
    /// request's tool surface or not (`SPEC.md` §30.1: "A ticket whose authority disallows
    /// shell does not receive shell tool definitions — not a disabled tool ... no schema in the
    /// context at all").
    ///
    /// This is a **structural, argument-independent** approximation of [`Authority::permits`]:
    /// at listing time there is no call input yet (a tool's `to_action` needs real arguments,
    /// e.g. a path, that don't exist until the model actually calls it), so this classifies each
    /// [`ToolName`] statically by the slice of `authority` it could ever exercise, rather than
    /// simulating a call. [`ToolRegistry::dispatch`]'s `Authority::permits` check is what
    /// actually enforces the boundary per call; this function only decides what's worth
    /// advertising, and is deliberately permissive within a tool's structural bucket (e.g. any
    /// nonempty `repository.write` pattern admits every write-shaped tool, even one whose
    /// specific call later gets denied for touching a path outside that pattern).
    pub fn required_by(&self, authority: &Authority) -> bool {
        match self.name {
            // Read-scoped: search, symbol and history lookups, and read-shaped fs tools all
            // dispatch through `Action::ReadPath`.
            ToolName::SearchSemantic
            | ToolName::SearchExact
            | ToolName::SearchRegex
            | ToolName::SearchHybrid
            | ToolName::SymbolDefinition
            | ToolName::SymbolReferences
            | ToolName::SymbolCallers
            | ToolName::SymbolCallees
            | ToolName::SymbolOutline
            | ToolName::SymbolRenamePreview
            | ToolName::HistoryWhy
            | ToolName::HistorySearch
            | ToolName::HistoryDeleted
            | ToolName::FsRead
            | ToolName::FsReadRange
            | ToolName::FsList
            | ToolName::FsStat => !authority.repository.read.is_empty(),

            // Write/edit/patch: dispatch through `Action::WritePath`.
            ToolName::EditApplyPatch
            | ToolName::EditWriteFile
            | ToolName::EditCreateFile
            | ToolName::EditDeleteFile => !authority.repository.write.is_empty(),

            // shell.run-shaped: `shell.run`/`shell.query_output`/`test.run`/`build.run` map
            // through `action_run_argv`, and `git.status`/`git.diff`/`git.log` map through their
            // own `action_git_*` helpers — but all of them produce `Action::RunCommand`, not
            // `Action::Git`, so they're gated by shell authority at dispatch time too (see the
            // module doc comment on the `git.worktree`/`GitOp::Branch` gap for the same pattern
            // applied to a different action). They belong in this bucket, not the git one below.
            ToolName::ShellRun
            | ToolName::ShellQueryOutput
            | ToolName::GitStatus
            | ToolName::GitDiff
            | ToolName::GitLog
            | ToolName::TestRun
            | ToolName::BuildRun => authority.shell.enabled && !authority.shell.allow.is_empty(),

            // Per-op git: `git.commit` maps to `Action::Git { op: Commit }`.
            ToolName::GitCommit => authority.git.commit,
            // `git.branch` and `git.worktree` both map to `Action::Git { op: Branch }` (see the
            // module doc comment: `git.worktree` has no dedicated `GitOp`).
            ToolName::GitBranch | ToolName::GitWorktree => authority.git.branch,

            // Per-op ticket powers.
            ToolName::TicketCreateChild => authority.tickets.create_children,
            // `ticket.delegate` and `ask.human` both map to `Action::Ticket { op: Delegate }`
            // (see `action_ask_human`'s doc comment: asking a human hands off control the same
            // way delegating a child ticket does).
            ToolName::TicketDelegate | ToolName::AskHuman => authority.tickets.delegate_children,
            // `ticket.submit`, `ticket.comment`, `decision.record`, `artifact.store` and
            // `evidence.attach` are all gated under `TicketOp::ModifySibling` at dispatch time
            // (see the module doc comment on the closed `Action`/`TicketOp` vocabulary); mirror
            // that same grouping here rather than inventing a finer split dispatch doesn't
            // actually enforce.
            ToolName::TicketSubmit
            | ToolName::TicketComment
            | ToolName::DecisionRecord
            | ToolName::ArtifactStore
            | ToolName::EvidenceAttach => authority.tickets.modify_siblings,
        }
    }
}

/// The full catalog of tools an [`crate::agent_loop::AgentLoop`] offers a provider.
pub struct ToolRegistry {
    specs: BTreeMap<ToolName, ToolSpec>,
}

// ---------------------------------------------------------------------------------------------
// Action mappings. Kept as small, named, captureless functions so `ToolSpec::to_action` can
// hold plain function pointers, per the field's own doc comment.
// ---------------------------------------------------------------------------------------------

fn action_read_repo(_input: &Value) -> Result<Action> {
    Ok(Action::ReadPath { path: ".".into() })
}

fn action_read_path_field(input: &Value) -> Result<Action> {
    Ok(Action::ReadPath {
        path: get_string(input, "path")?,
    })
}

fn action_read_from_path_field(input: &Value) -> Result<Action> {
    Ok(Action::ReadPath {
        path: get_string(input, "from_path")?,
    })
}

fn action_write_path_field(input: &Value) -> Result<Action> {
    Ok(Action::WritePath {
        path: get_string(input, "path")?,
    })
}

fn action_run_argv(input: &Value) -> Result<Action> {
    Ok(Action::RunCommand {
        command: get_string_vec(input, "argv")?,
    })
}

fn action_git_status(_input: &Value) -> Result<Action> {
    Ok(Action::RunCommand {
        command: vec!["git".into(), "status".into()],
    })
}

fn action_git_diff(_input: &Value) -> Result<Action> {
    Ok(Action::RunCommand {
        command: vec!["git".into(), "diff".into()],
    })
}

fn action_git_log(_input: &Value) -> Result<Action> {
    Ok(Action::RunCommand {
        command: vec!["git".into(), "log".into()],
    })
}

fn action_git_commit(_input: &Value) -> Result<Action> {
    Ok(Action::Git { op: GitOp::Commit })
}

fn action_git_branch(_input: &Value) -> Result<Action> {
    Ok(Action::Git { op: GitOp::Branch })
}

fn action_ticket_create_child(_input: &Value) -> Result<Action> {
    Ok(Action::Ticket {
        op: TicketOp::CreateChild,
    })
}

fn action_ticket_delegate(_input: &Value) -> Result<Action> {
    Ok(Action::Ticket {
        op: TicketOp::Delegate,
    })
}

/// Shared fallback for the ticket-adjacent tools `TicketOp` has no dedicated op for; see the
/// module-level doc comment.
fn action_ticket_modify(_input: &Value) -> Result<Action> {
    Ok(Action::Ticket {
        op: TicketOp::ModifySibling,
    })
}

/// `ask.human` delegates control to a human; gated the same as delegating a child ticket, since
/// both hand authority to a different actor.
fn action_ask_human(_input: &Value) -> Result<Action> {
    Ok(Action::Ticket {
        op: TicketOp::Delegate,
    })
}

// ---------------------------------------------------------------------------------------------
// Input parsing helpers. Every one returns `TmError::parse` on malformed input rather than
// panicking.
// ---------------------------------------------------------------------------------------------

fn missing(field: &str) -> TmError {
    TmError::parse(format!("missing or malformed field `{field}`"))
}

fn get_str<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| missing(field))
}

fn get_string(input: &Value, field: &str) -> Result<String> {
    Ok(get_str(input, field)?.to_string())
}

fn get_opt_string(input: &Value, field: &str) -> Option<String> {
    input.get(field).and_then(Value::as_str).map(str::to_string)
}

fn get_u64(input: &Value, field: &str) -> Result<u64> {
    input
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| missing(field))
}

fn get_u32(input: &Value, field: &str) -> Result<u32> {
    Ok(get_u64(input, field)? as u32)
}

fn get_usize(input: &Value, field: &str) -> Result<usize> {
    Ok(get_u64(input, field)? as usize)
}

fn get_u64_or(input: &Value, field: &str, default: u64) -> u64 {
    input.get(field).and_then(Value::as_u64).unwrap_or(default)
}

fn get_bool_or(input: &Value, field: &str, default: bool) -> bool {
    input.get(field).and_then(Value::as_bool).unwrap_or(default)
}

fn get_string_vec(input: &Value, field: &str) -> Result<Vec<String>> {
    let arr = input
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| missing(field))?;
    arr.iter()
        .map(|v| v.as_str().map(str::to_string).ok_or_else(|| missing(field)))
        .collect()
}

fn get_string_vec_or_empty(input: &Value, field: &str) -> Vec<String> {
    input
        .get(field)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Join `path` onto `root`, rejecting absolute paths and `..` components so a tool can never
/// escape the project root.
fn resolve_repo_path(root: &Path, path: &str) -> Result<PathBuf> {
    let rel = Path::new(path);
    if rel.is_absolute() {
        return Err(TmError::parse(format!(
            "path `{path}` must be repository-relative"
        )));
    }
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(TmError::parse(format!(
            "path `{path}` may not contain `..`"
        )));
    }
    Ok(root.join(rel))
}

fn symbol_json(sym: &Symbol) -> Value {
    json!({
        "id": sym.id,
        "name": sym.name,
        "kind": format!("{:?}", sym.kind),
        "path": sym.path,
        "line_start": sym.range.line_start,
        "line_end": sym.range.line_end,
        "container": sym.container,
        "signature": sym.signature,
        "doc": sym.doc,
    })
}

fn reference_json(r: &Reference) -> Value {
    json!({
        "symbol_hint": r.symbol_hint,
        "path": r.path,
        "line_start": r.range.line_start,
        "line_end": r.range.line_end,
    })
}

/// The subset of [`tm_core::Store::create_ticket`]'s parameters a `ticket.create_child` caller
/// may set; everything else (parent, milestone, resources, executor requirements, verification
/// policy, retry policy) is fixed by this tool rather than left to the model.
#[derive(Debug, Deserialize)]
struct CreateChildInput {
    kind: TicketKind,
    objective: String,
    authority: Authority,
    #[serde(default)]
    budget: Budget,
    #[serde(default)]
    success: Vec<Predicate>,
    #[serde(default)]
    context_refs: Vec<ContextRef>,
    #[serde(default)]
    priority: i32,
}

/// Default requirements for a child ticket spawned by an agent: a fast coder, no human
/// required, willing to degrade one capability tier.
fn default_executor_requirements() -> ExecutorRequirements {
    ExecutorRequirements {
        role: Role::CoderFast,
        human_required: false,
        min_capability: Tolerance::Preferred,
    }
}

/// A conservative default retry policy for agent-spawned children.
fn default_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 30,
        backoff_multiplier: 2.0,
        max_delay_seconds: 600,
    }
}

/// Deterministic command-cache key from `argv` and `cwd` alone.
///
/// The real key formula (`tm_context::fingerprint::cache_key`) also folds in a repository
/// dirty-state fingerprint via an injected `GitInspector`, but [`ToolContext`] carries no such
/// inspector — only a [`CommandCache`]/[`CommandExecutor`] pair. Keying on `argv`/`cwd` alone
/// means a cacheable call is only reused across identical invocations, never invalidated by an
/// intervening repository change; callers that need that invalidation should mark their call
/// non-cacheable.
fn deterministic_command_key(argv: &[String], cwd: &str) -> String {
    let mut key = String::new();
    for arg in argv {
        key.push_str(arg);
        key.push('\u{1}');
    }
    key.push('\u{1e}');
    key.push_str(cwd);
    key
}

fn resolve_cwd(root: &Path, input: &Value) -> String {
    let rel = get_opt_string(input, "cwd").unwrap_or_else(|| ".".to_string());
    root.join(rel).to_string_lossy().into_owned()
}

fn command_result_json(result: &command::CommandResult) -> Value {
    json!({
        "exit_code": result.exit_code,
        "duration_ms": result.duration_ms,
        "stdout_artifact": result.stdout_artifact.as_str(),
        "stderr_artifact": result.stderr_artifact.as_str(),
        "from_cache": result.from_cache,
    })
}

fn parse_command_query(input: &Value) -> Result<CommandQuery> {
    match get_str(input, "query_type")? {
        "head" => Ok(CommandQuery::Head(get_usize(input, "n")?)),
        "tail" => Ok(CommandQuery::Tail(get_usize(input, "n")?)),
        "grep" => Ok(CommandQuery::Grep(get_string(input, "pattern")?)),
        "range" => Ok(CommandQuery::Range(
            get_usize(input, "start")?,
            get_usize(input, "end")?,
        )),
        "json" => Ok(CommandQuery::Json(get_string(input, "pointer")?)),
        other => Err(TmError::parse(format!("unknown query_type `{other}`"))),
    }
}

fn query_answer_json(answer: QueryAnswer) -> Value {
    match answer {
        QueryAnswer::Lines(lines) => json!({"found": true, "lines": lines}),
        QueryAnswer::Json(value) => json!({"found": true, "json": value}),
        QueryAnswer::NotFound => json!({"found": false}),
    }
}

impl ToolRegistry {
    /// Build the standard registry: one [`ToolSpec`] per [`ToolName::ALL`] entry, per
    /// `SPEC.md` §11.
    pub fn standard() -> Self {
        let entries: Vec<ToolSpec> = vec![
            ToolSpec {
                name: ToolName::SearchSemantic,
                description: "Semantic vector search over the project's code index.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["query"]
                }),
                cost: CostClass::Moderate,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::SearchExact,
                description: "Literal substring search over the working tree.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::SearchRegex,
                description: "Regular-expression search over the working tree.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"pattern": {"type": "string"}},
                    "required": ["pattern"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::SearchHybrid,
                description: "Fused hybrid search across semantic, lexical, symbol, path and history signals.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "seed_paths": {"type": "array", "items": {"type": "string"}},
                        "seed_symbols": {"type": "array", "items": {"type": "integer"}}
                    },
                    "required": ["query"]
                }),
                cost: CostClass::Moderate,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::SymbolDefinition,
                description: "Find the symbol defining `name` as seen from `from_path`.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "from_path": {"type": "string"}
                    },
                    "required": ["name", "from_path"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_from_path_field,
            },
            ToolSpec {
                name: ToolName::SymbolReferences,
                description: "Every reference site to a symbol id.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"symbol_id": {"type": "integer"}},
                    "required": ["symbol_id"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::SymbolCallers,
                description: "Every symbol that calls a symbol id.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"symbol_id": {"type": "integer"}},
                    "required": ["symbol_id"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::SymbolCallees,
                description: "Every symbol a symbol id calls.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"symbol_id": {"type": "integer"}},
                    "required": ["symbol_id"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::SymbolOutline,
                description: "A rendered outline of a file's top-level symbols.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
                cost: CostClass::Free,
                to_action: action_read_path_field,
            },
            ToolSpec {
                name: ToolName::SymbolRenamePreview,
                description: "Preview the edits a rename of a symbol id to `new_name` would make.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "symbol_id": {"type": "integer"},
                        "new_name": {"type": "string"}
                    },
                    "required": ["symbol_id", "new_name"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::HistoryWhy,
                description: "Git history: why a line range looks the way it does.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "line_start": {"type": "integer"},
                        "line_end": {"type": "integer"}
                    },
                    "required": ["path", "line_start", "line_end"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_path_field,
            },
            ToolSpec {
                name: ToolName::HistorySearch,
                description: "Search commit messages and diffs.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::HistoryDeleted,
                description: "Find implementations matching a query that were deleted and never reintroduced.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_repo,
            },
            ToolSpec {
                name: ToolName::FsRead,
                description: "Read a repository-relative file's full text content.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_path_field,
            },
            ToolSpec {
                name: ToolName::FsReadRange,
                description: "Read a byte range `[byte_start, byte_end)` of a file.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "byte_start": {"type": "integer"},
                        "byte_end": {"type": "integer"}
                    },
                    "required": ["path", "byte_start", "byte_end"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_path_field,
            },
            ToolSpec {
                name: ToolName::FsList,
                description: "List a repository-relative directory's entries.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
                cost: CostClass::Cheap,
                to_action: action_read_path_field,
            },
            ToolSpec {
                name: ToolName::FsStat,
                description: "Metadata (existence, size, kind) for a repository-relative path.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
                cost: CostClass::Free,
                to_action: action_read_path_field,
            },
            ToolSpec {
                name: ToolName::EditApplyPatch,
                description: "Apply one or more byte-range replacements to a file, conflict-checked.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "byte_start": {"type": "integer"},
                                    "byte_end": {"type": "integer"},
                                    "replacement": {"type": "string"}
                                },
                                "required": ["byte_start", "byte_end", "replacement"]
                            }
                        },
                        "expected_hash": {"type": ["string", "null"]}
                    },
                    "required": ["path", "edits"]
                }),
                cost: CostClass::Mutating,
                to_action: action_write_path_field,
            },
            ToolSpec {
                name: ToolName::EditWriteFile,
                description: "Overwrite a file's entire content, conflict-checked.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"},
                        "expected_hash": {"type": ["string", "null"]}
                    },
                    "required": ["path", "content"]
                }),
                cost: CostClass::Mutating,
                to_action: action_write_path_field,
            },
            ToolSpec {
                name: ToolName::EditCreateFile,
                description: "Create a new file; fails as a conflict if the path already exists.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
                cost: CostClass::Mutating,
                to_action: action_write_path_field,
            },
            ToolSpec {
                name: ToolName::EditDeleteFile,
                description: "Delete a file, conflict-checked against its last-observed content hash.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "expected_hash": {"type": ["string", "null"]}
                    },
                    "required": ["path"]
                }),
                cost: CostClass::Mutating,
                to_action: action_write_path_field,
            },
            ToolSpec {
                name: ToolName::ShellRun,
                description: "Run a command (argv, never a shell string).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "argv": {"type": "array", "items": {"type": "string"}},
                        "cwd": {"type": "string"},
                        "cacheable": {"type": "boolean"}
                    },
                    "required": ["argv"]
                }),
                cost: CostClass::Moderate,
                to_action: action_run_argv,
            },
            ToolSpec {
                name: ToolName::ShellQueryOutput,
                description: "Answer a head/tail/grep/range/json query against a previously run command's stored output, without re-running it.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "argv": {"type": "array", "items": {"type": "string"}},
                        "cwd": {"type": "string"},
                        "cacheable": {"type": "boolean"},
                        "stream": {"type": "string", "enum": ["stdout", "stderr"]},
                        "query_type": {"type": "string", "enum": ["head", "tail", "grep", "range", "json"]},
                        "n": {"type": "integer"},
                        "pattern": {"type": "string"},
                        "start": {"type": "integer"},
                        "end": {"type": "integer"},
                        "pointer": {"type": "string"}
                    },
                    "required": ["argv", "stream", "query_type"]
                }),
                cost: CostClass::Cheap,
                to_action: action_run_argv,
            },
            ToolSpec {
                name: ToolName::GitStatus,
                description: "`git status`.",
                input_schema: json!({"type": "object", "properties": {}}),
                cost: CostClass::Cheap,
                to_action: action_git_status,
            },
            ToolSpec {
                name: ToolName::GitDiff,
                description: "`git diff`.",
                input_schema: json!({"type": "object", "properties": {}}),
                cost: CostClass::Cheap,
                to_action: action_git_diff,
            },
            ToolSpec {
                name: ToolName::GitLog,
                description: "`git log`.",
                input_schema: json!({"type": "object", "properties": {}}),
                cost: CostClass::Cheap,
                to_action: action_git_log,
            },
            ToolSpec {
                name: ToolName::GitCommit,
                description: "Create a commit.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]
                }),
                cost: CostClass::Moderate,
                to_action: action_git_commit,
            },
            ToolSpec {
                name: ToolName::GitBranch,
                description: "Create or switch a branch.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }),
                cost: CostClass::Moderate,
                to_action: action_git_branch,
            },
            ToolSpec {
                name: ToolName::GitWorktree,
                description: "Add a git worktree.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "branch": {"type": "string"}
                    },
                    "required": ["path", "branch"]
                }),
                cost: CostClass::Moderate,
                to_action: action_git_branch,
            },
            ToolSpec {
                name: ToolName::TestRun,
                description: "Run the project's test suite (argv, never a shell string).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "argv": {"type": "array", "items": {"type": "string"}},
                        "cwd": {"type": "string"},
                        "cacheable": {"type": "boolean"}
                    },
                    "required": ["argv"]
                }),
                cost: CostClass::Moderate,
                to_action: action_run_argv,
            },
            ToolSpec {
                name: ToolName::BuildRun,
                description: "Run the project's build (argv, never a shell string).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "argv": {"type": "array", "items": {"type": "string"}},
                        "cwd": {"type": "string"},
                        "cacheable": {"type": "boolean"}
                    },
                    "required": ["argv"]
                }),
                cost: CostClass::Moderate,
                to_action: action_run_argv,
            },
            ToolSpec {
                name: ToolName::TicketCreateChild,
                description: "Create a child of the current ticket.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string"},
                        "objective": {"type": "string"},
                        "authority": {"type": "object"},
                        "budget": {"type": "object"},
                        "success": {"type": "array"},
                        "context_refs": {"type": "array"},
                        "priority": {"type": "integer"}
                    },
                    "required": ["kind", "objective", "authority"]
                }),
                cost: CostClass::Mutating,
                to_action: action_ticket_create_child,
            },
            ToolSpec {
                name: ToolName::TicketDelegate,
                description: "Delegate a child ticket to another executor.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "child": {"type": "string"},
                        "delegate": {"type": "string"}
                    },
                    "required": ["child", "delegate"]
                }),
                cost: CostClass::Mutating,
                to_action: action_ticket_delegate,
            },
            ToolSpec {
                name: ToolName::TicketSubmit,
                description: "Submit work on the current ticket, carrying evidence artifacts.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "summary": {"type": "string"},
                        "evidence": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["summary", "evidence"]
                }),
                cost: CostClass::Mutating,
                to_action: action_ticket_modify,
            },
            ToolSpec {
                name: ToolName::TicketComment,
                description: "Leave a comment on a ticket (defaults to the current ticket).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "ticket": {"type": "string"},
                        "body": {"type": "string"}
                    },
                    "required": ["body"]
                }),
                cost: CostClass::Mutating,
                to_action: action_ticket_modify,
            },
            ToolSpec {
                name: ToolName::DecisionRecord,
                description: "Record a decision.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "subject": {"type": "string"},
                        "decision": {"type": "string"},
                        "reason": {"type": "string"},
                        "evidence": {"type": "array", "items": {"type": "string"}},
                        "affected_tickets": {"type": "array", "items": {"type": "string"}},
                        "affected_paths": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["subject", "decision", "reason"]
                }),
                cost: CostClass::Mutating,
                to_action: action_ticket_modify,
            },
            ToolSpec {
                name: ToolName::ArtifactStore,
                description: "Store bytes (as UTF-8 text) as a new artifact.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string"},
                        "media_type": {"type": "string"},
                        "content": {"type": "string"},
                        "meta": {"type": "object"},
                        "ticket": {"type": "string"}
                    },
                    "required": ["kind", "media_type", "content"]
                }),
                cost: CostClass::Mutating,
                to_action: action_ticket_modify,
            },
            ToolSpec {
                name: ToolName::EvidenceAttach,
                description: "Attach an existing artifact to a ticket as evidence.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "ticket": {"type": "string"},
                        "kind": {"type": "string"},
                        "artifact": {"type": "string"},
                        "summary": {"type": "string"}
                    },
                    "required": ["kind", "artifact", "summary"]
                }),
                cost: CostClass::Mutating,
                to_action: action_ticket_modify,
            },
            ToolSpec {
                name: ToolName::AskHuman,
                description: "Suspend and ask a human a question.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"question": {"type": "string"}},
                    "required": ["question"]
                }),
                cost: CostClass::Blocking,
                to_action: action_ask_human,
            },
        ];

        let specs = entries.into_iter().map(|s| (s.name, s)).collect();
        ToolRegistry { specs }
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

    /// [`ToolRegistry::tool_defs`], admitted by authority rather than availability
    /// (`SPEC.md` §30.1): only specs whose [`ToolSpec::required_by`] holds for `authority` are
    /// rendered, so a tool a ticket's authority structurally cannot exercise never appears in
    /// the request's tool surface at all — not merely a disabled tool that would answer
    /// `Authority::permits` with a denial at dispatch time.
    pub fn tool_defs_for(&self, authority: &Authority) -> Vec<tm_provider::ToolDef> {
        self.specs
            .values()
            .filter(|spec| spec.required_by(authority))
            .map(|spec| tm_provider::ToolDef {
                name: spec.name.as_str().to_string(),
                description: spec.description.to_string(),
                input_schema: spec.input_schema.clone(),
            })
            .collect()
    }

    /// The per-tool byte/token cost of exactly the tool surface [`ToolRegistry::tool_defs_for`]
    /// would send for `authority` (`SPEC.md` §30.2): the ecosystem-survey cost the section opens
    /// with — a dozen connected MCP servers' worth of schemas paid on every turn — made
    /// attributable as a line item, the same way [`tm_context::ContextPack`]'s sections are.
    pub fn tool_surface_cost_for(&self, authority: &Authority) -> Vec<tm_context::ToolSurfaceCost> {
        self.specs
            .values()
            .filter(|spec| spec.required_by(authority))
            .map(|spec| {
                tm_context::ToolSurfaceCost::compute(
                    spec.name.as_str(),
                    spec.description,
                    &spec.input_schema,
                )
            })
            .collect()
    }

    /// Dispatch one model-issued call: resolve its tool, map to an [`Action`], check `authority`,
    /// and — if permitted — execute against `ctx`.
    ///
    /// Never returns `Err`: an unknown tool name, a malformed input, an authority denial, or a
    /// dispatch-time failure are all represented as a variant of [`ToolOutcome`] so the caller
    /// always has a tool result to hand back to the model.
    pub fn dispatch(&self, call: &ToolCall, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Some(tool) = ToolName::parse(&call.name) else {
            return ToolOutcome::Errored {
                detail: format!("unknown tool `{}`", call.name),
            };
        };
        let Some(spec) = self.specs.get(&tool) else {
            return ToolOutcome::Errored {
                detail: format!("no registered spec for `{}`", tool.as_str()),
            };
        };
        let action = match (spec.to_action)(&call.input) {
            Ok(action) => action,
            Err(e) => {
                return ToolOutcome::Errored {
                    detail: e.to_string(),
                }
            }
        };
        match ctx.authority.permits(&action) {
            tm_types::Decision::Allow => {}
            tm_types::Decision::Deny(reason) => return ToolOutcome::Denied { reason },
            tm_types::Decision::NeedsApproval(reason) => {
                return ToolOutcome::Errored {
                    detail: format!(
                        "internal error: dispatch reached with an action needing approval ({reason}); \
                         the caller must suspend via AgentOutcome::AwaitingApproval before calling dispatch again"
                    ),
                };
            }
        }
        match execute(tool, &call.input, ctx) {
            Ok(value) => match bound_result(value, ctx) {
                Ok((result, artifact)) => ToolOutcome::Completed { result, artifact },
                Err(e) => ToolOutcome::Errored {
                    detail: e.to_string(),
                },
            },
            Err(e) => ToolOutcome::Errored {
                detail: e.to_string(),
            },
        }
    }
}

/// Execute one already-authorized call. Split out from `dispatch` so the authority check above
/// is the only path into this function.
fn execute(tool: ToolName, input: &Value, ctx: &ToolContext<'_>) -> Result<Value> {
    match tool {
        ToolName::SearchSemantic => {
            let query = get_str(input, "query")?;
            let top_k = get_u64_or(input, "limit", 20) as usize;
            let opts = SemanticSearchOptions {
                top_k,
                ..SemanticSearchOptions::default()
            };
            let hits = ctx.ci.search_semantic(query, opts)?;
            Ok(json!(hits
                .iter()
                .map(|h| json!({
                    "path": h.path, "line_start": h.line_start, "line_end": h.line_end,
                    "score": h.score, "text": h.text,
                }))
                .collect::<Vec<_>>()))
        }
        ToolName::SearchExact => {
            let needle = get_str(input, "query")?;
            let result = ctx.ci.search_exact(needle)?;
            Ok(json!({
                "truncated": result.truncated,
                "hits": result.hits.iter().map(|h| json!({
                    "path": h.path, "line": h.line, "col": h.col, "line_text": h.line_text,
                })).collect::<Vec<_>>(),
            }))
        }
        ToolName::SearchRegex => {
            let pattern = get_str(input, "pattern")?;
            let result = ctx.ci.search_regex(pattern)?;
            Ok(json!({
                "truncated": result.truncated,
                "hits": result.hits.iter().map(|h| json!({
                    "path": h.path, "line": h.line, "col": h.col, "line_text": h.line_text,
                })).collect::<Vec<_>>(),
            }))
        }
        ToolName::SearchHybrid => {
            let text = get_string(input, "query")?;
            let seed_paths = get_string_vec_or_empty(input, "seed_paths");
            let seed_symbols: Vec<u64> = input
                .get("seed_symbols")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            let query = Query {
                text,
                seed_symbols,
                seed_paths,
            };
            let retrieval_ctx = RetrievalContext {
                claimed_paths: Vec::new(),
                recently_edited: Vec::new(),
            };
            let hits = ctx
                .ci
                .search_hybrid(&query, &retrieval_ctx, SignalWeights::default())?;
            Ok(json!(hits
                .iter()
                .map(|h| json!({
                    "path": h.path, "line_start": h.line_start, "line_end": h.line_end,
                    "snippet": h.snippet, "fused_score": h.fused_score,
                }))
                .collect::<Vec<_>>()))
        }
        ToolName::SymbolDefinition => {
            let name = get_str(input, "name")?;
            let from_path = get_str(input, "from_path")?;
            let def = ctx.ci.definition(name, from_path)?;
            Ok(json!(def.as_ref().map(symbol_json)))
        }
        ToolName::SymbolReferences => {
            let symbol_id = get_u64(input, "symbol_id")?;
            let index = ctx.ci.symbol_index()?;
            let refs = index.references(symbol_id);
            Ok(json!(refs
                .iter()
                .map(|r| reference_json(r))
                .collect::<Vec<_>>()))
        }
        ToolName::SymbolCallers => {
            let symbol_id = get_u64(input, "symbol_id")?;
            let index = ctx.ci.symbol_index()?;
            let callers = index.callers(symbol_id);
            Ok(json!(callers
                .iter()
                .map(|s| symbol_json(s))
                .collect::<Vec<_>>()))
        }
        ToolName::SymbolCallees => {
            let symbol_id = get_u64(input, "symbol_id")?;
            let index = ctx.ci.symbol_index()?;
            let callees = index.callees(symbol_id);
            Ok(json!(callees
                .iter()
                .map(|s| symbol_json(s))
                .collect::<Vec<_>>()))
        }
        ToolName::SymbolOutline => {
            let path = get_str(input, "path")?;
            let outline = ctx.ci.outline(path)?;
            Ok(json!(outline
                .iter()
                .map(
                    |e| json!({"symbol_id": e.symbol_id, "depth": e.depth, "rendered": e.rendered})
                )
                .collect::<Vec<_>>()))
        }
        ToolName::SymbolRenamePreview => {
            let symbol_id = get_u64(input, "symbol_id")?;
            let new_name = get_str(input, "new_name")?;
            let index = ctx.ci.symbol_index()?;
            let patch = index.rename_preview(symbol_id, new_name)?;
            Ok(json!({
                "edits": patch.edits.iter().map(|e| json!({
                    "path": e.path, "byte_start": e.byte_start, "byte_end": e.byte_end,
                    "replacement": e.replacement,
                })).collect::<Vec<_>>(),
                "skipped_ambiguous": patch.skipped_ambiguous.iter().map(reference_json).collect::<Vec<_>>(),
            }))
        }
        ToolName::HistoryWhy => {
            let path = get_str(input, "path")?;
            let line_start = get_u32(input, "line_start")?;
            let line_end = get_u32(input, "line_end")?;
            let answer = ctx.ci.history_why(path, line_start, line_end)?;
            Ok(json!({
                "path": answer.path, "line_start": answer.line_start, "line_end": answer.line_end,
                "commits": answer.commits.iter().map(commit_json).collect::<Vec<_>>(),
            }))
        }
        ToolName::HistorySearch => {
            let query = get_str(input, "query")?;
            let hits = ctx.ci.history_search(query)?;
            Ok(json!(hits
                .iter()
                .map(|h| json!({
                    "commit": commit_json(&h.commit), "path": h.path, "snippet": h.snippet,
                }))
                .collect::<Vec<_>>()))
        }
        ToolName::HistoryDeleted => {
            let query = get_str(input, "query")?;
            let hits = ctx.ci.history_deleted(query)?;
            Ok(json!(hits
                .iter()
                .map(|h| json!({
                    "path": h.path, "commit": commit_json(&h.commit), "removed_text": h.removed_text,
                }))
                .collect::<Vec<_>>()))
        }
        ToolName::FsRead => {
            let path = get_str(input, "path")?;
            let full = resolve_repo_path(ctx.patch_engine.root(), path)?;
            let content = std::fs::read_to_string(&full)?;
            Ok(json!({"path": path, "content": content}))
        }
        ToolName::FsReadRange => {
            let path = get_str(input, "path")?;
            let byte_start = get_usize(input, "byte_start")?;
            let byte_end = get_usize(input, "byte_end")?;
            let full = resolve_repo_path(ctx.patch_engine.root(), path)?;
            let bytes = std::fs::read(&full)?;
            if byte_start > byte_end || byte_end > bytes.len() {
                return Err(TmError::parse(format!(
                    "invalid range [{byte_start}, {byte_end}) for {path} of length {}",
                    bytes.len()
                )));
            }
            let slice = String::from_utf8_lossy(&bytes[byte_start..byte_end]).into_owned();
            Ok(
                json!({"path": path, "byte_start": byte_start, "byte_end": byte_end, "content": slice}),
            )
        }
        ToolName::FsList => {
            let path = get_str(input, "path")?;
            let full = resolve_repo_path(ctx.patch_engine.root(), path)?;
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&full)? {
                let entry = entry?;
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                entries.push(json!({
                    "name": entry.file_name().to_string_lossy().into_owned(),
                    "is_dir": is_dir,
                }));
            }
            Ok(json!({"path": path, "entries": entries}))
        }
        ToolName::FsStat => {
            let path = get_str(input, "path")?;
            let full = resolve_repo_path(ctx.patch_engine.root(), path)?;
            match std::fs::metadata(&full) {
                Ok(meta) => Ok(json!({
                    "path": path, "exists": true, "is_dir": meta.is_dir(), "len": meta.len(),
                })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok(json!({"path": path, "exists": false}))
                }
                Err(e) => Err(TmError::from(e)),
            }
        }
        ToolName::EditApplyPatch => {
            let path = get_string(input, "path")?;
            let mut expected_hash = get_opt_string(input, "expected_hash");
            let edits = input
                .get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| missing("edits"))?;
            let mut applied = Vec::new();
            for e in edits {
                let byte_start =
                    e.get("byte_start")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| missing("byte_start"))? as usize;
                let byte_end = e
                    .get("byte_end")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| missing("byte_end"))? as usize;
                let replacement = e
                    .get("replacement")
                    .and_then(Value::as_str)
                    .ok_or_else(|| missing("replacement"))?
                    .to_string();
                let edit = Edit::RangeReplace {
                    path: path.clone(),
                    byte_start,
                    byte_end,
                    replacement,
                    expected_hash: expected_hash.clone(),
                };
                match ctx.patch_engine.apply(&edit) {
                    Ok(patch) => {
                        expected_hash = patch.hash_after.clone();
                        applied.push(json!({"applied": true, "patch": patch_json(&patch)}));
                    }
                    Err(e) => {
                        applied.push(json!({"applied": false, "error": e.to_string()}));
                        break;
                    }
                }
            }
            Ok(json!({"path": path, "edits": applied}))
        }
        ToolName::EditWriteFile => {
            let path = get_string(input, "path")?;
            let content = get_string(input, "content")?;
            let expected_hash = get_opt_string(input, "expected_hash");
            let edit = Edit::Write {
                path,
                content,
                expected_hash,
            };
            Ok(patch_outcome_json(ctx.patch_engine.apply(&edit)))
        }
        ToolName::EditCreateFile => {
            let path = get_string(input, "path")?;
            let content = get_string(input, "content")?;
            let edit = Edit::Create { path, content };
            Ok(patch_outcome_json(ctx.patch_engine.apply(&edit)))
        }
        ToolName::EditDeleteFile => {
            let path = get_string(input, "path")?;
            let expected_hash = get_opt_string(input, "expected_hash");
            let edit = Edit::Delete {
                path,
                expected_hash,
            };
            Ok(patch_outcome_json(ctx.patch_engine.apply(&edit)))
        }
        ToolName::ShellRun => run_shell_like(input, ctx),
        ToolName::TestRun => run_shell_like(input, ctx),
        ToolName::BuildRun => run_shell_like(input, ctx),
        ToolName::ShellQueryOutput => {
            let argv = get_string_vec(input, "argv")?;
            let cwd = resolve_cwd(ctx.patch_engine.root(), input);
            let cacheable = get_bool_or(input, "cacheable", false);
            let key = command_key(ctx, &argv, &cwd, cacheable);
            let Some(result) = ctx.command_cache.get(&key)? else {
                return Ok(json!({"found": false}));
            };
            let stream = match get_str(input, "stream")? {
                "stdout" => ArtifactStream::Stdout,
                "stderr" => ArtifactStream::Stderr,
                other => return Err(TmError::parse(format!("unknown stream `{other}`"))),
            };
            let query = parse_command_query(input)?;
            let answer = result.query(stream, ctx.command_cache, query)?;
            Ok(query_answer_json(answer))
        }
        ToolName::GitStatus => run_fixed_git(&["git", "status"], ctx),
        ToolName::GitDiff => run_fixed_git(&["git", "diff"], ctx),
        ToolName::GitLog => run_fixed_git(&["git", "log", "--oneline", "-n", "50"], ctx),
        ToolName::GitCommit => {
            let message = get_string(input, "message")?;
            run_fixed_git(&["git", "commit", "-m", &message], ctx)
        }
        ToolName::GitBranch => {
            let name = get_string(input, "name")?;
            run_fixed_git(&["git", "checkout", "-B", &name], ctx)
        }
        ToolName::GitWorktree => {
            let path = get_string(input, "path")?;
            let branch = get_string(input, "branch")?;
            run_fixed_git(&["git", "worktree", "add", &path, &branch], ctx)
        }
        ToolName::TicketCreateChild => {
            let parsed: CreateChildInput = serde_json::from_value(input.clone())?;
            let events = ctx.store.create_ticket(
                parsed.kind,
                parsed.objective,
                Some(ctx.ticket.clone()),
                None,
                parsed.authority,
                Vec::new(),
                default_executor_requirements(),
                parsed.context_refs,
                parsed.success,
                VerificationPolicy::Single,
                parsed.budget,
                default_retry_policy(),
                parsed.priority,
                ctx.actor.clone(),
            )?;
            let child = events
                .iter()
                .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()));
            Ok(json!({"child": child.map(|t| t.into_string())}))
        }
        ToolName::TicketDelegate => {
            let child: TicketId = get_str(input, "child")?.parse()?;
            let delegate: ParticipantId = get_str(input, "delegate")?.parse()?;
            let events = ctx.store.update_ticket(
                &child,
                json!({"delegated_to": delegate.as_str()}),
                ctx.actor.clone(),
            )?;
            Ok(
                json!({"child": child.as_str(), "delegate": delegate.as_str(), "events": events.len()}),
            )
        }
        ToolName::TicketSubmit => {
            let summary = get_string(input, "summary")?;
            let evidence: Vec<ArtifactId> = get_string_vec(input, "evidence")?
                .into_iter()
                .map(|s| s.parse::<ArtifactId>())
                .collect::<Result<Vec<ArtifactId>>>()?;
            let events = ctx
                .store
                .submit(ctx.ticket, summary, evidence, ctx.actor.clone())?;
            Ok(json!({"ticket": ctx.ticket.as_str(), "events": events.len()}))
        }
        ToolName::TicketComment => {
            let target: TicketId = match get_opt_string(input, "ticket") {
                Some(s) => s.parse()?,
                None => ctx.ticket.clone(),
            };
            let body = get_string(input, "body")?;
            let events = ctx.store.update_ticket(
                &target,
                json!({"comment": {"author": ctx.actor.as_str(), "body": body}}),
                ctx.actor.clone(),
            )?;
            Ok(json!({"ticket": target.as_str(), "events": events.len()}))
        }
        ToolName::DecisionRecord => {
            let subject = get_string(input, "subject")?;
            let decision = get_string(input, "decision")?;
            let reason = get_string(input, "reason")?;
            let evidence: Vec<ArtifactId> = get_string_vec_or_empty(input, "evidence")
                .into_iter()
                .map(|s| s.parse::<ArtifactId>())
                .collect::<Result<Vec<ArtifactId>>>()?;
            let affected_tickets: Vec<TicketId> =
                get_string_vec_or_empty(input, "affected_tickets")
                    .into_iter()
                    .map(|s| s.parse::<TicketId>())
                    .collect::<Result<Vec<TicketId>>>()?;
            let affected_paths = get_string_vec_or_empty(input, "affected_paths");
            let events = ctx.store.record_decision(
                subject,
                decision,
                reason,
                evidence,
                affected_tickets,
                affected_paths,
                ctx.actor.clone(),
            )?;
            let decision_id = events
                .iter()
                .find_map(|e| e.payload.as_decision_created().map(|p| p.decision.clone()));
            Ok(json!({"decision": decision_id.map(|d| d.into_string())}))
        }
        ToolName::ArtifactStore => {
            let kind: ArtifactKind =
                serde_json::from_value(Value::String(get_string(input, "kind")?))?;
            let media_type = get_string(input, "media_type")?;
            let content = get_string(input, "content")?;
            let meta = input.get("meta").cloned().unwrap_or_else(|| json!({}));
            let ticket = match get_opt_string(input, "ticket") {
                Some(s) => Some(s.parse()?),
                None => Some(ctx.ticket.clone()),
            };
            let events = ctx.store.store_artifact(
                kind,
                media_type,
                content.into_bytes(),
                meta,
                ticket,
                ctx.actor.clone(),
            )?;
            let artifact = events
                .iter()
                .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()));
            Ok(json!({"artifact": artifact.map(|a| a.into_string())}))
        }
        ToolName::EvidenceAttach => {
            let target: TicketId = match get_opt_string(input, "ticket") {
                Some(s) => s.parse()?,
                None => ctx.ticket.clone(),
            };
            let kind: EvidenceKind =
                serde_json::from_value(Value::String(get_string(input, "kind")?))?;
            let artifact: ArtifactId = get_str(input, "artifact")?.parse()?;
            let summary = get_string(input, "summary")?;
            let events =
                ctx.store
                    .attach_evidence(&target, kind, &artifact, summary, ctx.actor.clone())?;
            Ok(json!({"ticket": target.as_str(), "events": events.len()}))
        }
        ToolName::AskHuman => {
            let question = get_string(input, "question")?;
            Ok(json!({"asked": true, "question": question}))
        }
    }
}

fn commit_json(c: &tm_codeintel::history::CommitSummary) -> Value {
    json!({"sha": c.sha, "author": c.author, "authored_at": c.authored_at, "message": c.message})
}

fn patch_json(p: &crate::patch::Patch) -> Value {
    json!({
        "path": p.path,
        "unified_diff": p.unified_diff,
        "hash_before": p.hash_before,
        "hash_after": p.hash_after,
        "lines_added": p.summary.lines_added,
        "lines_removed": p.summary.lines_removed,
        "created": p.summary.created,
        "deleted": p.summary.deleted,
    })
}

/// Edit conflicts and out-of-scope writes are actionable feedback for the model, not dispatch
/// failures, so a [`crate::patch::PatchOutcome`] is always turned into a `Completed`-shaped
/// value rather than propagated as an error.
fn patch_outcome_json(outcome: crate::patch::PatchOutcome) -> Value {
    match outcome {
        Ok(patch) => json!({"applied": true, "patch": patch_json(&patch)}),
        Err(e) => json!({"applied": false, "error": e.to_string()}),
    }
}

fn command_key(ctx: &ToolContext<'_>, argv: &[String], cwd: &str, cacheable: bool) -> String {
    if cacheable {
        deterministic_command_key(argv, cwd)
    } else {
        format!(
            "noncacheable-{}-{}",
            ctx.ticket.as_str(),
            ctx.ids.random_hex(16)
        )
    }
}

fn run_shell_like(input: &Value, ctx: &ToolContext<'_>) -> Result<Value> {
    let argv = get_string_vec(input, "argv")?;
    let cwd = resolve_cwd(ctx.patch_engine.root(), input);
    let cacheable = get_bool_or(input, "cacheable", false);
    let key = command_key(ctx, &argv, &cwd, cacheable);
    let spec = CommandSpec {
        argv,
        cwd,
        env_allowlist: Vec::new(),
        declared_inputs: Vec::new(),
        cacheable,
        ticket: Some(ctx.ticket.clone()),
        session: Some(ctx.session.clone()),
    };
    // `command::run`'s own `command.started`/`command.completed` event drafts are dropped here:
    // `Store` exposes no generic "append arbitrary event drafts" entry point, only the typed
    // commands above, so there is nowhere in `tm-core`'s finished public API to commit them.
    let (result, _drafts) = command::run(
        &spec,
        &key,
        ctx.command_cache,
        ctx.authority,
        ctx.command_executor,
        ctx.clock,
        ctx.actor,
    )?;
    Ok(command_result_json(&result))
}

fn run_fixed_git(argv: &[&str], ctx: &ToolContext<'_>) -> Result<Value> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let cwd = ctx.patch_engine.root().to_string_lossy().into_owned();
    let key = command_key(ctx, &argv, &cwd, false);
    let spec = CommandSpec {
        argv,
        cwd,
        env_allowlist: Vec::new(),
        declared_inputs: Vec::new(),
        cacheable: false,
        ticket: Some(ctx.ticket.clone()),
        session: Some(ctx.session.clone()),
    };
    let (result, _drafts) = command::run(
        &spec,
        &key,
        ctx.command_cache,
        ctx.authority,
        ctx.command_executor,
        ctx.clock,
        ctx.actor,
    )?;
    Ok(command_result_json(&result))
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
fn bound_result(
    value: serde_json::Value,
    ctx: &mut ToolContext<'_>,
) -> Result<(serde_json::Value, Option<ArtifactId>)> {
    let bytes = serde_json::to_vec(&value)?;
    if bytes.len() <= MAX_INLINE_RESULT_BYTES {
        return Ok((value, None));
    }
    let events = ctx.store.store_artifact(
        ArtifactKind::Report,
        "application/json".to_string(),
        bytes.clone(),
        json!({"tool_result": true}),
        Some(ctx.ticket.clone()),
        ctx.actor.clone(),
    )?;
    let artifact = events
        .iter()
        .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
        .ok_or_else(|| TmError::invariant("store_artifact did not emit artifact.created"))?;
    let preview_len = MAX_INLINE_RESULT_BYTES.min(bytes.len());
    let preview = String::from_utf8_lossy(&bytes[..preview_len]).into_owned();
    let preview_json = json!({
        "truncated": true,
        "artifact": artifact.as_str(),
        "preview": preview,
    });
    Ok((preview_json, Some(artifact)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tm_types::{FixedClock, PatternSet, TestIds, Timestamp};

    struct FakeCache {
        results: Mutex<BTreeMap<String, command::CommandResult>>,
        artifacts: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl FakeCache {
        fn new() -> Self {
            FakeCache {
                results: Mutex::new(BTreeMap::new()),
                artifacts: Mutex::new(BTreeMap::new()),
            }
        }
    }

    impl CommandCache for FakeCache {
        fn get(&self, key: &str) -> Result<Option<command::CommandResult>> {
            Ok(self.results.lock().unwrap().get(key).cloned())
        }

        fn put(
            &self,
            key: &str,
            argv: &[String],
            exit_code: i32,
            started: Timestamp,
            completed: Timestamp,
            stdout: &[u8],
            stderr: &[u8],
        ) -> Result<command::CommandResult> {
            let stdout_id: ArtifactId = "ART-000000000001".parse().unwrap();
            let stderr_id: ArtifactId = "ART-000000000002".parse().unwrap();
            self.artifacts
                .lock()
                .unwrap()
                .insert("ART-000000000001".into(), stdout.to_vec());
            self.artifacts
                .lock()
                .unwrap()
                .insert("ART-000000000002".into(), stderr.to_vec());
            let result = command::CommandResult {
                key: key.to_string(),
                argv: argv.to_vec(),
                exit_code,
                duration_ms: 0,
                stdout_artifact: stdout_id,
                stderr_artifact: stderr_id,
                started,
                completed,
                from_cache: false,
            };
            self.results
                .lock()
                .unwrap()
                .insert(key.to_string(), result.clone());
            Ok(result)
        }

        fn read_artifact(&self, id: &ArtifactId) -> Result<Vec<u8>> {
            Ok(self
                .artifacts
                .lock()
                .unwrap()
                .get(id.as_str())
                .cloned()
                .unwrap_or_default())
        }
    }

    struct FakeExecutor;

    impl CommandExecutor for FakeExecutor {
        fn execute(&self, spec: &CommandSpec) -> Result<command::ExecutionOutcome> {
            Ok(command::ExecutionOutcome {
                exit_code: 0,
                stdout: format!("ran: {}", spec.argv.join(" ")).into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    struct Harness {
        _dir: TempDir,
        authority: Authority,
        ci: CodeIntel,
        store: Store,
        patch_engine: PatchEngine,
        cache: FakeCache,
        executor: FakeExecutor,
        clock: FixedClock,
        ids: TestIds,
        ticket: TicketId,
        session: SessionId,
        actor: ParticipantId,
    }

    impl Harness {
        fn new() -> Self {
            let dir = TempDir::new().expect("tempdir");
            let authority = Authority::root();
            let ci = CodeIntel::open(dir.path()).expect("codeintel");
            let store = Store::open_with(
                dir.path(),
                Arc::new(FixedClock::epoch()),
                Arc::new(TestIds::new()),
            )
            .expect("store");
            let patch_engine = PatchEngine::new(dir.path().to_path_buf(), authority.clone());
            let actor: ParticipantId = "agent:test/worker".parse().unwrap();
            let events = store
                .create_ticket(
                    TicketKind::Work,
                    "root".into(),
                    None,
                    None,
                    authority.clone(),
                    Vec::new(),
                    default_executor_requirements(),
                    Vec::new(),
                    Vec::new(),
                    VerificationPolicy::None,
                    Budget::unlimited(),
                    default_retry_policy(),
                    0,
                    actor.clone(),
                )
                .expect("seed root ticket");
            let ticket = events
                .iter()
                .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
                .expect("ticket.created payload");
            Harness {
                _dir: dir,
                authority,
                ci,
                store,
                patch_engine,
                cache: FakeCache::new(),
                executor: FakeExecutor,
                clock: FixedClock::epoch(),
                ids: TestIds::new(),
                ticket,
                session: "S-1".parse().unwrap(),
                actor,
            }
        }

        fn ctx(&self) -> ToolContext<'_> {
            ToolContext {
                authority: &self.authority,
                ci: &self.ci,
                store: &self.store,
                patch_engine: &self.patch_engine,
                command_cache: &self.cache,
                command_executor: &self.executor,
                clock: &self.clock,
                ids: &self.ids,
                ticket: &self.ticket,
                session: &self.session,
                actor: &self.actor,
            }
        }
    }

    fn call(name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: name.into(),
            input,
        }
    }

    #[test]
    fn tool_name_round_trips_every_variant() {
        for t in ToolName::ALL {
            assert_eq!(ToolName::parse(t.as_str()), Some(*t));
        }
    }

    #[test]
    fn parse_rejects_an_unknown_name() {
        assert_eq!(ToolName::parse("nope.nope"), None);
    }

    #[test]
    fn standard_registers_every_tool_name_exactly_once() {
        let registry = ToolRegistry::standard();
        let names: Vec<ToolName> = registry.specs().map(|s| s.name).collect();
        assert_eq!(names.len(), ToolName::ALL.len());
        for t in ToolName::ALL {
            assert!(registry.get(t.as_str()).is_some());
        }
    }

    #[test]
    fn tool_defs_carries_every_schema_through() {
        let registry = ToolRegistry::standard();
        let defs = registry.tool_defs();
        assert_eq!(defs.len(), ToolName::ALL.len());
        assert!(defs.iter().any(|d| d.name == "fs.read"));
    }

    // -----------------------------------------------------------------------------------------
    // SPEC.md §30.1: admit by authority, not by availability.
    // -----------------------------------------------------------------------------------------

    fn def_names(defs: &[tm_provider::ToolDef]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn tool_defs_for_root_authority_admits_every_tool() {
        let registry = ToolRegistry::standard();
        let defs = registry.tool_defs_for(&Authority::root());
        assert_eq!(defs.len(), ToolName::ALL.len());
        for t in ToolName::ALL {
            assert!(
                def_names(&defs).contains(&t.as_str()),
                "root authority should admit {}",
                t.as_str()
            );
        }
    }

    #[test]
    fn tool_defs_for_shell_disabled_omits_every_shell_shaped_tool_entirely() {
        let mut authority = Authority::root();
        authority.shell.enabled = false;
        let registry = ToolRegistry::standard();
        let defs = registry.tool_defs_for(&authority);
        let names = def_names(&defs);

        for absent in [
            "shell.run",
            "shell.query_output",
            "test.run",
            "build.run",
            "git.status",
            "git.diff",
            "git.log",
        ] {
            assert!(
                !names.contains(&absent),
                "{absent} should be genuinely absent from the tool surface, not just denied, \
                 when shell is disabled; got {names:?}"
            );
        }
        // Everything outside the shell-shaped bucket is unaffected.
        assert!(names.contains(&"fs.read"));
        assert!(names.contains(&"edit.write_file"));
        assert!(names.contains(&"git.commit"));
        assert_eq!(defs.len(), ToolName::ALL.len() - 7);
    }

    #[test]
    fn tool_defs_for_network_disabled_matches_network_enabled_since_no_tool_is_network_gated() {
        // SPEC.md §30.1 calls out network-touching tools as a bucket, but `ToolName::ALL`
        // currently has no tool that dispatches through `Action::NetFetch` — there is no
        // `fetch.*`/MCP-shaped tool in the §11 catalog yet. This asserts the true-today fact
        // (network authority changes nothing about the admitted set) rather than fabricating a
        // network-gated tool to exercise a bullet with nothing to gate.
        let mut enabled = Authority::root();
        enabled.network.docs = true;
        enabled.network.arbitrary = true;

        let mut disabled = Authority::root();
        disabled.network.docs = false;
        disabled.network.arbitrary = false;
        disabled.network.allowlist.clear();

        let registry = ToolRegistry::standard();
        let with_network = registry.tool_defs_for(&enabled);
        let without_network = registry.tool_defs_for(&disabled);

        assert_eq!(with_network.len(), without_network.len());
        assert_eq!(def_names(&with_network), def_names(&without_network));
    }

    #[test]
    fn tool_defs_for_write_disabled_omits_every_write_shaped_tool_entirely() {
        let mut authority = Authority::root();
        authority.repository.write = PatternSet::empty();
        let registry = ToolRegistry::standard();
        let defs = registry.tool_defs_for(&authority);
        let names = def_names(&defs);

        for absent in [
            "edit.apply_patch",
            "edit.write_file",
            "edit.create_file",
            "edit.delete_file",
        ] {
            assert!(
                !names.contains(&absent),
                "{absent} should be genuinely absent from the tool surface, not just denied, \
                 when repository.write is empty; got {names:?}"
            );
        }
        assert!(names.contains(&"fs.read"));
        assert_eq!(defs.len(), ToolName::ALL.len() - 4);
    }

    #[test]
    fn tool_defs_for_read_disabled_omits_every_read_shaped_tool_entirely() {
        let mut authority = Authority::root();
        authority.repository.read = PatternSet::empty();
        let registry = ToolRegistry::standard();
        let defs = registry.tool_defs_for(&authority);
        let names = def_names(&defs);

        assert!(!names.contains(&"fs.read"));
        assert!(!names.contains(&"search.exact"));
        assert!(!names.contains(&"symbol.outline"));
        assert!(names.contains(&"edit.write_file"));
        assert_eq!(defs.len(), ToolName::ALL.len() - 17);
    }

    #[test]
    fn tool_defs_for_none_authority_admits_nothing() {
        let registry = ToolRegistry::standard();
        let defs = registry.tool_defs_for(&Authority::none());
        assert!(
            defs.is_empty(),
            "no-authority should admit zero tools, got {:?}",
            def_names(&defs)
        );
    }

    #[test]
    fn filter_is_an_economy_optimization_not_a_replacement_for_dispatch_time_enforcement() {
        // A tool can be admitted into the surface (its structural precondition holds) while a
        // *specific* call still gets denied by `Authority::permits` at dispatch time — the
        // filter narrows what's offered, it does not widen what's allowed.
        let mut h = Harness::new();
        h.authority.repository.read = PatternSet::parse(["src/**"]).unwrap();
        let registry = ToolRegistry::standard();

        let defs = registry.tool_defs_for(&h.authority);
        assert!(
            def_names(&defs).contains(&"fs.read"),
            "fs.read should be admitted: repository.read is nonempty"
        );

        let outcome = registry.dispatch(
            &call("fs.read", json!({"path": "other/secret.txt"})),
            &mut h.ctx(),
        );
        assert!(
            matches!(outcome, ToolOutcome::Denied { .. }),
            "a path outside the authority's read scope must still be denied at dispatch, \
             even though fs.read was admitted into the tool surface"
        );
    }

    // -----------------------------------------------------------------------------------------
    // SPEC.md §30.2: every layer pays rent, including the tool surface itself.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn tool_surface_cost_for_mirrors_tool_defs_for_and_is_nonzero() {
        let registry = ToolRegistry::standard();
        let authority = Authority::root();
        let defs = registry.tool_defs_for(&authority);
        let costs = registry.tool_surface_cost_for(&authority);

        assert_eq!(costs.len(), defs.len());
        let cost_names: std::collections::BTreeSet<&str> =
            costs.iter().map(|c| c.tool.as_str()).collect();
        for def in &defs {
            assert!(
                cost_names.contains(def.name.as_str()),
                "tool_surface_cost_for is missing a cost line for {}",
                def.name
            );
        }
        for cost in &costs {
            assert!(
                cost.bytes > 0,
                "{} should have a nonzero byte cost",
                cost.tool
            );
            assert!(
                cost.tokens > 0,
                "{} should have a nonzero token cost",
                cost.tool
            );
        }
    }

    #[test]
    fn tool_surface_cost_for_shrinks_when_the_tool_surface_shrinks() {
        let registry = ToolRegistry::standard();
        let root_cost: usize = registry
            .tool_surface_cost_for(&Authority::root())
            .iter()
            .map(|c| c.bytes)
            .sum();

        let mut scoped = Authority::root();
        scoped.shell.enabled = false;
        scoped.repository.write = PatternSet::empty();
        let scoped_cost: usize = registry
            .tool_surface_cost_for(&scoped)
            .iter()
            .map(|c| c.bytes)
            .sum();

        assert!(
            scoped_cost < root_cost,
            "a narrower authority should pay less tool-surface rent: {scoped_cost} >= {root_cost}"
        );
    }

    #[test]
    fn dispatch_reports_unknown_tool_as_errored_not_panic() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(&call("nope.nope", json!({})), &mut h.ctx());
        assert!(matches!(outcome, ToolOutcome::Errored { .. }));
    }

    #[test]
    fn dispatch_reports_malformed_input_as_errored() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(&call("fs.read", json!({})), &mut h.ctx());
        assert!(matches!(outcome, ToolOutcome::Errored { .. }));
    }

    #[test]
    fn dispatch_denies_a_read_outside_authority_scope() {
        let mut h = Harness::new();
        h.authority.repository.read = PatternSet::empty();
        let registry = ToolRegistry::standard();
        let outcome =
            registry.dispatch(&call("fs.read", json!({"path": "hello.py"})), &mut h.ctx());
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
    }

    #[test]
    fn a_denial_never_touches_the_filesystem() {
        let mut h = Harness::new();
        h.authority.repository.write = PatternSet::empty();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call(
                "edit.create_file",
                json!({"path": "new.txt", "content": "hi"}),
            ),
            &mut h.ctx(),
        );
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
        assert!(!h.patch_engine.root().join("new.txt").exists());
    }

    #[test]
    fn fs_read_round_trips_written_content() {
        let h = Harness::new();
        std::fs::write(h.patch_engine.root().join("hello.txt"), "hi there").unwrap();
        let registry = ToolRegistry::standard();
        let outcome =
            registry.dispatch(&call("fs.read", json!({"path": "hello.txt"})), &mut h.ctx());
        match outcome {
            ToolOutcome::Completed { result, artifact } => {
                assert_eq!(result["content"], "hi there");
                assert!(artifact.is_none());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn fs_read_rejects_a_path_escaping_the_root() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call("fs.read", json!({"path": "../outside.txt"})),
            &mut h.ctx(),
        );
        assert!(matches!(outcome, ToolOutcome::Errored { .. }));
    }

    #[test]
    fn fs_stat_reports_absence_without_erroring() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call("fs.stat", json!({"path": "missing.txt"})),
            &mut h.ctx(),
        );
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["exists"], false),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn edit_create_file_then_write_file_round_trips() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let created = registry.dispatch(
            &call(
                "edit.create_file",
                json!({"path": "a.txt", "content": "one"}),
            ),
            &mut h.ctx(),
        );
        let ToolOutcome::Completed { result, .. } = created else {
            panic!("expected Completed");
        };
        assert_eq!(result["applied"], true);
        assert_eq!(
            std::fs::read_to_string(h.patch_engine.root().join("a.txt")).unwrap(),
            "one"
        );
    }

    #[test]
    fn edit_write_file_conflict_is_completed_not_errored() {
        let h = Harness::new();
        std::fs::write(h.patch_engine.root().join("a.txt"), "one").unwrap();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call(
                "edit.write_file",
                json!({"path": "a.txt", "content": "two", "expected_hash": "deadbeef"}),
            ),
            &mut h.ctx(),
        );
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["applied"], false),
            other => panic!("expected Completed(applied=false), got {other:?}"),
        }
    }

    #[test]
    fn search_exact_finds_a_written_literal() {
        let h = Harness::new();
        std::fs::write(
            h.patch_engine.root().join("m.py"),
            "def unique_marker(): pass\n",
        )
        .unwrap();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call("search.exact", json!({"query": "unique_marker"})),
            &mut h.ctx(),
        );
        match outcome {
            ToolOutcome::Completed { result, .. } => {
                assert_eq!(result["hits"].as_array().unwrap().len(), 1);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn symbol_outline_of_an_unindexed_file_is_empty() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call("symbol.outline", json!({"path": "nope.rs"})),
            &mut h.ctx(),
        );
        match outcome {
            ToolOutcome::Completed { result, .. } => assert!(result.as_array().unwrap().is_empty()),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn shell_run_executes_via_the_injected_executor() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call(
                "shell.run",
                json!({"argv": ["echo", "hi"], "cacheable": true}),
            ),
            &mut h.ctx(),
        );
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["exit_code"], 0),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn shell_run_is_denied_without_shell_authority() {
        let mut h = Harness::new();
        h.authority.shell.enabled = false;
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call("shell.run", json!({"argv": ["echo", "hi"]})),
            &mut h.ctx(),
        );
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
    }

    #[test]
    fn shell_query_output_reports_not_found_before_any_run() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(
            &call(
                "shell.query_output",
                json!({"argv": ["echo", "hi"], "cacheable": true, "stream": "stdout", "query_type": "head", "n": 1}),
            ),
            &mut h.ctx(),
        );
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["found"], false),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn shell_query_output_reads_back_a_cached_runs_stdout() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        registry.dispatch(
            &call(
                "shell.run",
                json!({"argv": ["echo", "hi"], "cacheable": true}),
            ),
            &mut h.ctx(),
        );
        let outcome = registry.dispatch(
            &call(
                "shell.query_output",
                json!({"argv": ["echo", "hi"], "cacheable": true, "stream": "stdout", "query_type": "head", "n": 1}),
            ),
            &mut h.ctx(),
        );
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["found"], true),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn ticket_create_child_is_denied_without_create_children_authority() {
        let mut h = Harness::new();
        h.authority.tickets.create_children = false;
        let registry = ToolRegistry::standard();
        let input = json!({
            "kind": "work",
            "objective": "do a thing",
            "authority": Authority::default(),
        });
        let outcome = registry.dispatch(&call("ticket.create_child", input), &mut h.ctx());
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
    }

    #[test]
    fn ticket_create_child_succeeds_with_authority() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let input = json!({
            "kind": "work",
            "objective": "do a thing",
            "authority": Authority::default(),
        });
        let outcome = registry.dispatch(&call("ticket.create_child", input), &mut h.ctx());
        match outcome {
            ToolOutcome::Completed { result, .. } => assert!(result["child"].is_string()),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn ask_human_maps_to_the_delegate_action() {
        let input = json!({"question": "which approach?"});
        let action = action_ask_human(&input).unwrap();
        assert_eq!(
            action,
            Action::Ticket {
                op: TicketOp::Delegate
            }
        );
    }

    #[test]
    fn ask_human_is_completed_when_delegate_authority_is_held() {
        let h = Harness::new();
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(&call("ask.human", json!({"question": "?"})), &mut h.ctx());
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["question"], "?"),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn ask_human_is_denied_without_delegate_authority() {
        let mut h = Harness::new();
        h.authority.tickets.delegate_children = false;
        let registry = ToolRegistry::standard();
        let outcome = registry.dispatch(&call("ask.human", json!({"question": "?"})), &mut h.ctx());
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
    }

    #[test]
    fn bound_result_inlines_a_small_value() {
        let h = Harness::new();
        let mut ctx = h.ctx();
        let (result, artifact) = bound_result(json!({"ok": true}), &mut ctx).unwrap();
        assert_eq!(result, json!({"ok": true}));
        assert!(artifact.is_none());
    }

    #[test]
    fn bound_result_spills_a_large_value_to_an_artifact() {
        let h = Harness::new();
        let mut ctx = h.ctx();
        let big = "x".repeat(MAX_INLINE_RESULT_BYTES + 1);
        let (result, artifact) = bound_result(json!({"text": big}), &mut ctx).unwrap();
        assert!(artifact.is_some());
        assert_eq!(result["truncated"], true);
        assert!(result["artifact"].is_string());
    }

    #[test]
    fn get_string_vec_rejects_a_non_array_field() {
        let err = get_string_vec(&json!({"x": "not an array"}), "x").unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }

    #[test]
    fn resolve_repo_path_rejects_parent_dir_escape() {
        let err = resolve_repo_path(Path::new("/root"), "../evil").unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }

    #[test]
    fn resolve_repo_path_rejects_absolute_paths() {
        let err = resolve_repo_path(Path::new("/root"), "/etc/passwd").unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }

    #[test]
    fn deterministic_command_key_is_stable_for_the_same_inputs() {
        let a = deterministic_command_key(&["echo".into(), "hi".into()], "/root");
        let b = deterministic_command_key(&["echo".into(), "hi".into()], "/root");
        assert_eq!(a, b);
    }

    #[test]
    fn deterministic_command_key_differs_on_argv() {
        let a = deterministic_command_key(&["echo".into(), "hi".into()], "/root");
        let b = deterministic_command_key(&["echo".into(), "bye".into()], "/root");
        assert_ne!(a, b);
    }
}
