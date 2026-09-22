//! [`ToolRegistry`]: the authority-gated, provider-based tool surface an
//! [`crate::agent_loop::AgentLoop`] dispatches through.
//!
//! Per `docs/audit-2026-09-18-fable.md` A-01, tools are no longer a single hand-written literal
//! `Vec`; they are contributed by however many [`tm_types::CapabilityProvider`]s a binary
//! assembles (today: just [`BuiltinCapability`], migrating `SPEC.md` §11's fixed 39-tool
//! catalog — `search.*`, `symbol.*`, `history.*`, `fs.*`, `edit.*`, `shell.run`, `git.*`,
//! `ticket.*`, `decision.record`, `artifact.store`, `evidence.attach` — onto the same trait a
//! future `tm-browser`/`tm-computer`/`tm-pty`/MCP-client provider will implement, without
//! special-casing the builtin set). [`ToolRegistry`] itself stays generic: it holds whatever
//! providers it was built with, indexes their tools once, and dispatches by name.
//!
//! Dispatch never trusts the model: every call is mapped to a [`tm_types::Action`] via
//! [`tm_types::CapabilityProvider::to_action`] and checked against `Authority::permits` *before*
//! it reaches [`tm_types::CapabilityProvider::invoke`]; a denial comes back as a structured
//! [`ToolOutcome::Denied`] result rather than an error the loop has to special-case, and results
//! are bounded — anything past [`MAX_INLINE_RESULT_BYTES`] is stored as an artifact and
//! referenced by id rather than inlined into the transcript.
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
use std::sync::Arc;

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
    Action, ArtifactId, Authority, AuthorityRequirement, Budget, CallContext, CapabilityProvider,
    CostClass, Decision, GitOp, ParticipantId, Predicate, Result, Role, TicketId, TicketOp,
    TmError, Tolerance, ToolSchema,
};

use crate::patch::{Edit, PatchEngine};

/// Bytes past which a tool result is spilled to an artifact and referenced by id instead of
/// being inlined into the model-visible transcript.
pub const MAX_INLINE_RESULT_BYTES: usize = 8 * 1024;

/// One tool name from `SPEC.md` §11's fixed catalog. Variant names mirror the spec's dotted
/// tool names (`Search*` = `search.*`, etc.) so [`ToolName::as_str`] round-trips exactly.
///
/// This enum, and the whole builtin dispatch table below, is private to [`BuiltinCapability`] —
/// nothing outside this module needs a closed enum of builtin tool names any more; a caller only
/// ever sees the dotted wire name on a [`tm_types::ToolSchema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ToolName {
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
    pub(crate) const ALL: &'static [ToolName] = &[
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
    pub(crate) fn as_str(self) -> &'static str {
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
    pub(crate) fn parse(name: &str) -> Option<ToolName> {
        ToolName::ALL.iter().copied().find(|t| t.as_str() == name)
    }
}

/// The structural, argument-independent [`AuthorityRequirement`] each builtin tool is advertised
/// under (`SPEC.md` §30.1: "A ticket whose authority disallows shell does not receive shell tool
/// definitions ... no schema in the context at all").
///
/// A direct port of the admit-by-authority filter that landed pre-audit as
/// `ToolSpec::required_by`'s match arms — unchanged in behavior, relocated onto
/// [`tm_types::ToolSchema::requires`]'s shape. See [`ToolSchema`]'s doc comment (in
/// `tm-types::capability`) for why this is per-tool rather than only the provider-wide
/// [`CapabilityProvider::requires`].
fn requirement_for(name: ToolName) -> AuthorityRequirement {
    use AuthorityRequirement::{Git, RepoRead, RepoWrite, Shell, Ticket};
    match name {
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
        | ToolName::FsStat => RepoRead,

        // Write/edit/patch: dispatch through `Action::WritePath`.
        ToolName::EditApplyPatch
        | ToolName::EditWriteFile
        | ToolName::EditCreateFile
        | ToolName::EditDeleteFile => RepoWrite,

        // shell.run-shaped: `shell.run`/`shell.query_output`/`test.run`/`build.run` map through
        // `action_run_argv`, and `git.status`/`git.diff`/`git.log` map through their own
        // `action_git_*` helpers — but all of them produce `Action::RunCommand`, not
        // `Action::Git`, so they're gated by shell authority here too (see the module doc
        // comment on the `git.worktree`/`GitOp::Branch` gap for the same pattern applied to a
        // different action). They belong in this bucket, not the git one below.
        ToolName::ShellRun
        | ToolName::ShellQueryOutput
        | ToolName::GitStatus
        | ToolName::GitDiff
        | ToolName::GitLog
        | ToolName::TestRun
        | ToolName::BuildRun => Shell,

        // Per-op git: `git.commit` maps to `Action::Git { op: Commit }`.
        ToolName::GitCommit => Git(GitOp::Commit),
        // `git.branch` and `git.worktree` both map to `Action::Git { op: Branch }` (see the
        // module doc comment: `git.worktree` has no dedicated `GitOp`).
        ToolName::GitBranch | ToolName::GitWorktree => Git(GitOp::Branch),

        // Per-op ticket powers.
        ToolName::TicketCreateChild => Ticket(TicketOp::CreateChild),
        // `ticket.delegate` and `ask.human` both map to `Action::Ticket { op: Delegate }` (both
        // hand authority to a different actor).
        ToolName::TicketDelegate | ToolName::AskHuman => Ticket(TicketOp::Delegate),
        // `ticket.submit`, `ticket.comment`, `decision.record`, `artifact.store` and
        // `evidence.attach` are all gated under `TicketOp::ModifySibling` at dispatch time (see
        // the module doc comment on the closed `Action`/`TicketOp` vocabulary); mirror that same
        // grouping here rather than inventing a finer split dispatch doesn't actually enforce.
        ToolName::TicketSubmit
        | ToolName::TicketComment
        | ToolName::DecisionRecord
        | ToolName::ArtifactStore
        | ToolName::EvidenceAttach => Ticket(TicketOp::ModifySibling),
    }
}

// ---------------------------------------------------------------------------------------------
// Action mappings. Kept as small, named, captureless functions — [`BuiltinCapability::to_action`]
// matches on the parsed [`ToolName`] and calls straight through to one of these, the same
// grouping the old per-tool `ToolSpec::to_action` function-pointer field used.
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

pub(crate) fn missing(field: &str) -> TmError {
    TmError::parse(format!("missing or malformed field `{field}`"))
}

pub(crate) fn get_str<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
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
/// dirty-state fingerprint via an injected `GitInspector`, but [`CallContext`] carries no such
/// inspector — only what [`BuiltinCapability`] itself owns. Keying on `argv`/`cwd` alone means a
/// cacheable call is only reused across identical invocations, never invalidated by an
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

/// A command-cache key for one call: deterministic from `argv`/`cwd` when `cacheable`, otherwise
/// unique per call so it is never served from — or pollutes — the cache.
fn command_key(ctx: &CallContext<'_>, argv: &[String], cwd: &str, cacheable: bool) -> String {
    if cacheable {
        deterministic_command_key(argv, cwd)
    } else {
        format!(
            "noncacheable-{}-{}",
            ctx.ticket
                .map(|t| t.as_str())
                .unwrap_or_else(|| ctx.session.as_str()),
            ctx.ids.random_hex(16)
        )
    }
}

/// The builtin tool set migrated onto [`CapabilityProvider`] (`docs/audit-2026-09-18-fable.md`
/// A-01): `SPEC.md` §11's fixed 39-tool catalog — `search.*`, `symbol.*`, `history.*`, `fs.*`,
/// `edit.*`, `shell.run`/`test.run`/`build.run`, `git.*`, `ticket.*`, `decision.record`,
/// `artifact.store`, `evidence.attach`, `ask.human` — dispatching into `tm-codeintel`,
/// `tm-context`, `tm-core` or [`crate::patch::PatchEngine`] exactly as it did as a hand-written
/// `Vec` before this migration. Registered as the sole provider by [`ToolRegistry::standard`];
/// not special-cased by [`ToolRegistry`] itself, which only ever sees it through the trait — the
/// seam a future `tm-browser`/`tm-computer` provider slots into the same way.
pub struct BuiltinCapability {
    ci: Arc<CodeIntel>,
    store: Arc<Store>,
    command_cache: Arc<dyn CommandCache + Send + Sync>,
    command_executor: Arc<dyn CommandExecutor + Send + Sync>,
}

impl BuiltinCapability {
    /// Build the builtin capability over already-open project infrastructure.
    pub fn new(
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
    ) -> Self {
        BuiltinCapability {
            ci,
            store,
            command_cache,
            command_executor,
        }
    }

    fn run_shell_like(
        &self,
        input: &Value,
        ctx: &CallContext<'_>,
        patch_engine: &PatchEngine,
    ) -> Result<Value> {
        let argv = get_string_vec(input, "argv")?;
        let cwd = resolve_cwd(patch_engine.root(), input);
        let cacheable = get_bool_or(input, "cacheable", false);
        let key = command_key(ctx, &argv, &cwd, cacheable);
        let spec = CommandSpec {
            argv,
            cwd,
            env_allowlist: Vec::new(),
            declared_inputs: Vec::new(),
            cacheable,
            ticket: ctx.ticket.cloned(),
            session: Some(ctx.session.clone()),
        };
        // `command::run`'s own `command.started`/`command.completed` event drafts previously had
        // nowhere to land (`Store` exposed no generic "append arbitrary event drafts" entry
        // point); `docs/audit-2026-09-18-fable.md` B-05 added `Store::append(Vec<EventDraft>)`
        // for exactly this, so they are committed below instead of dropped. Empty on a cache
        // hit (`command::run` never builds them for one), so this is a no-op transaction in that
        // case rather than an empty-but-real one.
        let (result, drafts) = command::run(
            &spec,
            &key,
            self.command_cache.as_ref(),
            ctx.authority,
            self.command_executor.as_ref(),
            ctx.clock,
            ctx.actor,
        )?;
        if !drafts.is_empty() {
            self.store.append(drafts)?;
        }
        Ok(command_result_json(&result))
    }

    fn run_fixed_git(
        &self,
        argv: &[&str],
        ctx: &CallContext<'_>,
        patch_engine: &PatchEngine,
    ) -> Result<Value> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let cwd = patch_engine.root().to_string_lossy().into_owned();
        let key = command_key(ctx, &argv, &cwd, false);
        let spec = CommandSpec {
            argv,
            cwd,
            env_allowlist: Vec::new(),
            declared_inputs: Vec::new(),
            cacheable: false,
            ticket: ctx.ticket.cloned(),
            session: Some(ctx.session.clone()),
        };
        let (result, drafts) = command::run(
            &spec,
            &key,
            self.command_cache.as_ref(),
            ctx.authority,
            self.command_executor.as_ref(),
            ctx.clock,
            ctx.actor,
        )?;
        if !drafts.is_empty() {
            self.store.append(drafts)?;
        }
        Ok(command_result_json(&result))
    }

    /// As [`BuiltinCapability::run_fixed_git`], but for the one `git.*` tool that is a genuine
    /// external-in-the-sense-of-`SPEC.md`-§21.5 effect rather than a read: a commit changes
    /// repository state that a crash-and-resume must not silently double-apply. Wraps the run in
    /// a `Store::begin_effect`/`EffectGuard::complete` guard (audit B-11) keyed on
    /// `(ticket, ticket.attempts, "git.commit", deterministic_command_key(argv, cwd))` — the same
    /// `(argv, cwd)` shape `run_fixed_git`'s own cache key already uses, just computed
    /// deterministically here instead of the random per-call key `command_key` builds for
    /// non-cacheable commands (a fresh key every call would defeat the idempotency guard
    /// entirely).
    ///
    /// No `confirm()`-shaped recovery probe exists for `git.commit`: unlike `git push` (mutates a
    /// remote another process could inspect via `git ls-remote`) or `tm-mirror`'s adapters
    /// (mutate an external tracker searchable by a marker), a local commit's only source of truth
    /// is the same repository this call would act on, and `git commit` itself already refuses
    /// with a clean, harmless error when there is nothing staged to commit — so on a resumed,
    /// never-completed journal entry the documented, accepted behavior is to re-run (mirroring
    /// this module's plain `command::run` calls, which have no confirm probe at all) rather than
    /// invent a guess-shaped check against the same repository the effect itself would touch.
    /// `git.push` is not dispatched here (or anywhere in this crate): `ToolName` has no `GitPush`
    /// variant on this branch (only `git.status`/`git.diff`/`git.log`/`git.commit`/`git.branch`/
    /// `git.worktree` are agent-invocable `git.*` tools today) — the only real `git push` in this
    /// workspace happens inside `tm-mirror::Tracker` implementations, wrapped separately by
    /// `tm-cli`'s `mirror_push` call site.
    fn run_fixed_git_idempotent(
        &self,
        argv: &[&str],
        effect_kind: &str,
        ctx: &CallContext<'_>,
        patch_engine: &PatchEngine,
    ) -> Result<Value> {
        // The effect journal is keyed per ticket attempt so a resumed scheduler run can't
        // double-apply; a ticketless chat session never resumes from that journal, so it simply
        // runs the command.
        let Some(ticket) = ctx.ticket else {
            return self.run_fixed_git(argv, ctx, patch_engine);
        };
        let argv_owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let cwd = patch_engine.root().to_string_lossy().into_owned();
        let canonical_args = deterministic_command_key(&argv_owned, &cwd);
        let attempt = self
            .store
            .view()?
            .tickets
            .get(ticket)
            .map(|t| t.attempts)
            .unwrap_or(0);
        let key = tm_core::EffectKey::compute(ticket, attempt, effect_kind, &canonical_args);
        let guard = self.store.begin_effect(
            key,
            ticket.clone(),
            attempt,
            effect_kind,
            ctx.actor.clone(),
        )?;
        if guard.already_completed() {
            return Ok(json!({
                "skipped_already_completed": true,
                "receipt_artifact": guard.prior_receipt(),
            }));
        }
        let value = self.run_fixed_git(argv, ctx, patch_engine)?;
        let receipt = value
            .get("stdout_artifact")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        guard.complete(&self.store, receipt.as_deref())?;
        Ok(value)
    }

    /// Execute one already-authorized call. Split out from [`CapabilityProvider::invoke`] so
    /// that method is a thin `PatchEngine`-construction-plus-dispatch wrapper.
    fn execute(
        &self,
        tool: ToolName,
        input: &Value,
        ctx: &CallContext<'_>,
        patch_engine: &PatchEngine,
    ) -> Result<Value> {
        match tool {
            ToolName::SearchSemantic => {
                let query = get_str(input, "query")?;
                let top_k = get_u64_or(input, "limit", 20) as usize;
                let opts = SemanticSearchOptions {
                    top_k,
                    ..SemanticSearchOptions::default()
                };
                let hits = self.ci.search_semantic(query, opts)?;
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
                let result = self.ci.search_exact(needle)?;
                Ok(json!({
                    "truncated": result.truncated,
                    "hits": result.hits.iter().map(|h| json!({
                        "path": h.path, "line": h.line, "col": h.col, "line_text": h.line_text,
                    })).collect::<Vec<_>>(),
                }))
            }
            ToolName::SearchRegex => {
                let pattern = get_str(input, "pattern")?;
                let result = self.ci.search_regex(pattern)?;
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
                let hits =
                    self.ci
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
                let def = self.ci.definition(name, from_path)?;
                Ok(json!(def.as_ref().map(symbol_json)))
            }
            ToolName::SymbolReferences => {
                let symbol_id = get_u64(input, "symbol_id")?;
                let index = self.ci.symbol_index()?;
                let refs = index.references(symbol_id);
                Ok(json!(refs
                    .iter()
                    .map(|r| reference_json(r))
                    .collect::<Vec<_>>()))
            }
            ToolName::SymbolCallers => {
                let symbol_id = get_u64(input, "symbol_id")?;
                let index = self.ci.symbol_index()?;
                let callers = index.callers(symbol_id);
                Ok(json!(callers
                    .iter()
                    .map(|s| symbol_json(s))
                    .collect::<Vec<_>>()))
            }
            ToolName::SymbolCallees => {
                let symbol_id = get_u64(input, "symbol_id")?;
                let index = self.ci.symbol_index()?;
                let callees = index.callees(symbol_id);
                Ok(json!(callees
                    .iter()
                    .map(|s| symbol_json(s))
                    .collect::<Vec<_>>()))
            }
            ToolName::SymbolOutline => {
                let path = get_str(input, "path")?;
                let outline = self.ci.outline(path)?;
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
                let index = self.ci.symbol_index()?;
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
                let answer = self.ci.history_why(path, line_start, line_end)?;
                Ok(json!({
                    "path": answer.path, "line_start": answer.line_start, "line_end": answer.line_end,
                    "commits": answer.commits.iter().map(commit_json).collect::<Vec<_>>(),
                }))
            }
            ToolName::HistorySearch => {
                let query = get_str(input, "query")?;
                let hits = self.ci.history_search(query)?;
                Ok(json!(hits
                    .iter()
                    .map(|h| json!({
                        "commit": commit_json(&h.commit), "path": h.path, "snippet": h.snippet,
                    }))
                    .collect::<Vec<_>>()))
            }
            ToolName::HistoryDeleted => {
                let query = get_str(input, "query")?;
                let hits = self.ci.history_deleted(query)?;
                Ok(json!(hits
                    .iter()
                    .map(|h| json!({
                        "path": h.path, "commit": commit_json(&h.commit), "removed_text": h.removed_text,
                    }))
                    .collect::<Vec<_>>()))
            }
            ToolName::FsRead => {
                let path = get_str(input, "path")?;
                let full = resolve_repo_path(patch_engine.root(), path)?;
                let content = std::fs::read_to_string(&full)?;
                Ok(json!({"path": path, "content": content}))
            }
            ToolName::FsReadRange => {
                let path = get_str(input, "path")?;
                let byte_start = get_usize(input, "byte_start")?;
                let byte_end = get_usize(input, "byte_end")?;
                let full = resolve_repo_path(patch_engine.root(), path)?;
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
                let full = resolve_repo_path(patch_engine.root(), path)?;
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
                let full = resolve_repo_path(patch_engine.root(), path)?;
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
                    let byte_end =
                        e.get("byte_end")
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
                    match patch_engine.apply(&edit) {
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
                Ok(patch_outcome_json(patch_engine.apply(&edit)))
            }
            ToolName::EditCreateFile => {
                let path = get_string(input, "path")?;
                let content = get_string(input, "content")?;
                let edit = Edit::Create { path, content };
                Ok(patch_outcome_json(patch_engine.apply(&edit)))
            }
            ToolName::EditDeleteFile => {
                let path = get_string(input, "path")?;
                let expected_hash = get_opt_string(input, "expected_hash");
                let edit = Edit::Delete {
                    path,
                    expected_hash,
                };
                Ok(patch_outcome_json(patch_engine.apply(&edit)))
            }
            ToolName::ShellRun => self.run_shell_like(input, ctx, patch_engine),
            ToolName::TestRun => self.run_shell_like(input, ctx, patch_engine),
            ToolName::BuildRun => self.run_shell_like(input, ctx, patch_engine),
            ToolName::ShellQueryOutput => {
                let argv = get_string_vec(input, "argv")?;
                let cwd = resolve_cwd(patch_engine.root(), input);
                let cacheable = get_bool_or(input, "cacheable", false);
                let key = command_key(ctx, &argv, &cwd, cacheable);
                let Some(result) = self.command_cache.get(&key)? else {
                    return Ok(json!({"found": false}));
                };
                let stream = match get_str(input, "stream")? {
                    "stdout" => ArtifactStream::Stdout,
                    "stderr" => ArtifactStream::Stderr,
                    other => return Err(TmError::parse(format!("unknown stream `{other}`"))),
                };
                let query = parse_command_query(input)?;
                let answer = result.query(stream, self.command_cache.as_ref(), query)?;
                Ok(query_answer_json(answer))
            }
            ToolName::GitStatus => self.run_fixed_git(&["git", "status"], ctx, patch_engine),
            ToolName::GitDiff => self.run_fixed_git(&["git", "diff"], ctx, patch_engine),
            ToolName::GitLog => {
                self.run_fixed_git(&["git", "log", "--oneline", "-n", "50"], ctx, patch_engine)
            }
            ToolName::GitCommit => {
                let message = get_string(input, "message")?;
                self.run_fixed_git_idempotent(
                    &["git", "commit", "-m", &message],
                    "git.commit",
                    ctx,
                    patch_engine,
                )
            }
            ToolName::GitBranch => {
                let name = get_string(input, "name")?;
                self.run_fixed_git(&["git", "checkout", "-B", &name], ctx, patch_engine)
            }
            ToolName::GitWorktree => {
                let path = get_string(input, "path")?;
                let branch = get_string(input, "branch")?;
                self.run_fixed_git(
                    &["git", "worktree", "add", &path, &branch],
                    ctx,
                    patch_engine,
                )
            }
            ToolName::TicketCreateChild => {
                let parsed: CreateChildInput = serde_json::from_value(input.clone())?;
                let events = self.store.create_ticket(
                    parsed.kind,
                    parsed.objective,
                    ctx.ticket.cloned(),
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
                let events = self.store.update_ticket(
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
                let ticket = require_ticket(ctx, "ticket.submit")?;
                let events = self
                    .store
                    .submit(ticket, summary, evidence, ctx.actor.clone())?;
                Ok(json!({"ticket": ticket.as_str(), "events": events.len()}))
            }
            ToolName::TicketComment => {
                let target: TicketId = match get_opt_string(input, "ticket") {
                    Some(s) => s.parse()?,
                    None => require_ticket(ctx, "this tool")?.clone(),
                };
                let body = get_string(input, "body")?;
                let events = self.store.update_ticket(
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
                let events = self.store.record_decision(
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
                    None => ctx.ticket.cloned(),
                };
                let events = self.store.store_artifact(
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
                    None => require_ticket(ctx, "this tool")?.clone(),
                };
                let kind: EvidenceKind =
                    serde_json::from_value(Value::String(get_string(input, "kind")?))?;
                let artifact: ArtifactId = get_str(input, "artifact")?.parse()?;
                let summary = get_string(input, "summary")?;
                let events = self.store.attach_evidence(
                    &target,
                    kind,
                    &artifact,
                    summary,
                    ctx.actor.clone(),
                )?;
                Ok(json!({"ticket": target.as_str(), "events": events.len()}))
            }
            ToolName::AskHuman => {
                let question = get_string(input, "question")?;
                Ok(json!({"asked": true, "question": question}))
            }
        }
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for BuiltinCapability {
    fn id(&self) -> &str {
        "builtin"
    }

    fn tools(&self) -> Vec<ToolSchema> {
        vec![
            ToolSchema {
                name: ToolName::SearchSemantic.as_str(),
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
                requires: requirement_for(ToolName::SearchSemantic),
            },
            ToolSchema {
                name: ToolName::SearchExact.as_str(),
                description: "Literal substring search over the working tree.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::SearchExact),
            },
            ToolSchema {
                name: ToolName::SearchRegex.as_str(),
                description: "Regular-expression search over the working tree.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"pattern": {"type": "string"}},
                    "required": ["pattern"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::SearchRegex),
            },
            ToolSchema {
                name: ToolName::SearchHybrid.as_str(),
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
                requires: requirement_for(ToolName::SearchHybrid),
            },
            ToolSchema {
                name: ToolName::SymbolDefinition.as_str(),
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
                requires: requirement_for(ToolName::SymbolDefinition),
            },
            ToolSchema {
                name: ToolName::SymbolReferences.as_str(),
                description: "Every reference site to a symbol id.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"symbol_id": {"type": "integer"}},
                    "required": ["symbol_id"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::SymbolReferences),
            },
            ToolSchema {
                name: ToolName::SymbolCallers.as_str(),
                description: "Every symbol that calls a symbol id.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"symbol_id": {"type": "integer"}},
                    "required": ["symbol_id"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::SymbolCallers),
            },
            ToolSchema {
                name: ToolName::SymbolCallees.as_str(),
                description: "Every symbol a symbol id calls.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"symbol_id": {"type": "integer"}},
                    "required": ["symbol_id"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::SymbolCallees),
            },
            ToolSchema {
                name: ToolName::SymbolOutline.as_str(),
                description: "A rendered outline of a file's top-level symbols.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
                cost: CostClass::Free,
                requires: requirement_for(ToolName::SymbolOutline),
            },
            ToolSchema {
                name: ToolName::SymbolRenamePreview.as_str(),
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
                requires: requirement_for(ToolName::SymbolRenamePreview),
            },
            ToolSchema {
                name: ToolName::HistoryWhy.as_str(),
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
                requires: requirement_for(ToolName::HistoryWhy),
            },
            ToolSchema {
                name: ToolName::HistorySearch.as_str(),
                description: "Search commit messages and diffs.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::HistorySearch),
            },
            ToolSchema {
                name: ToolName::HistoryDeleted.as_str(),
                description: "Find implementations matching a query that were deleted and never reintroduced.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::HistoryDeleted),
            },
            ToolSchema {
                name: ToolName::FsRead.as_str(),
                description: "Read a repository-relative file's full text content.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::FsRead),
            },
            ToolSchema {
                name: ToolName::FsReadRange.as_str(),
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
                requires: requirement_for(ToolName::FsReadRange),
            },
            ToolSchema {
                name: ToolName::FsList.as_str(),
                description: "List a repository-relative directory's entries.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::FsList),
            },
            ToolSchema {
                name: ToolName::FsStat.as_str(),
                description: "Metadata (existence, size, kind) for a repository-relative path.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
                cost: CostClass::Free,
                requires: requirement_for(ToolName::FsStat),
            },
            ToolSchema {
                name: ToolName::EditApplyPatch.as_str(),
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
                requires: requirement_for(ToolName::EditApplyPatch),
            },
            ToolSchema {
                name: ToolName::EditWriteFile.as_str(),
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
                requires: requirement_for(ToolName::EditWriteFile),
            },
            ToolSchema {
                name: ToolName::EditCreateFile.as_str(),
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
                requires: requirement_for(ToolName::EditCreateFile),
            },
            ToolSchema {
                name: ToolName::EditDeleteFile.as_str(),
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
                requires: requirement_for(ToolName::EditDeleteFile),
            },
            ToolSchema {
                name: ToolName::ShellRun.as_str(),
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
                requires: requirement_for(ToolName::ShellRun),
            },
            ToolSchema {
                name: ToolName::ShellQueryOutput.as_str(),
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
                requires: requirement_for(ToolName::ShellQueryOutput),
            },
            ToolSchema {
                name: ToolName::GitStatus.as_str(),
                description: "`git status`.",
                input_schema: json!({"type": "object", "properties": {}}),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::GitStatus),
            },
            ToolSchema {
                name: ToolName::GitDiff.as_str(),
                description: "`git diff`.",
                input_schema: json!({"type": "object", "properties": {}}),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::GitDiff),
            },
            ToolSchema {
                name: ToolName::GitLog.as_str(),
                description: "`git log`.",
                input_schema: json!({"type": "object", "properties": {}}),
                cost: CostClass::Cheap,
                requires: requirement_for(ToolName::GitLog),
            },
            ToolSchema {
                name: ToolName::GitCommit.as_str(),
                description: "Create a commit.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]
                }),
                cost: CostClass::Moderate,
                requires: requirement_for(ToolName::GitCommit),
            },
            ToolSchema {
                name: ToolName::GitBranch.as_str(),
                description: "Create or switch a branch.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }),
                cost: CostClass::Moderate,
                requires: requirement_for(ToolName::GitBranch),
            },
            ToolSchema {
                name: ToolName::GitWorktree.as_str(),
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
                requires: requirement_for(ToolName::GitWorktree),
            },
            ToolSchema {
                name: ToolName::TestRun.as_str(),
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
                requires: requirement_for(ToolName::TestRun),
            },
            ToolSchema {
                name: ToolName::BuildRun.as_str(),
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
                requires: requirement_for(ToolName::BuildRun),
            },
            ToolSchema {
                name: ToolName::TicketCreateChild.as_str(),
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
                requires: requirement_for(ToolName::TicketCreateChild),
            },
            ToolSchema {
                name: ToolName::TicketDelegate.as_str(),
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
                requires: requirement_for(ToolName::TicketDelegate),
            },
            ToolSchema {
                name: ToolName::TicketSubmit.as_str(),
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
                requires: requirement_for(ToolName::TicketSubmit),
            },
            ToolSchema {
                name: ToolName::TicketComment.as_str(),
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
                requires: requirement_for(ToolName::TicketComment),
            },
            ToolSchema {
                name: ToolName::DecisionRecord.as_str(),
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
                requires: requirement_for(ToolName::DecisionRecord),
            },
            ToolSchema {
                name: ToolName::ArtifactStore.as_str(),
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
                requires: requirement_for(ToolName::ArtifactStore),
            },
            ToolSchema {
                name: ToolName::EvidenceAttach.as_str(),
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
                requires: requirement_for(ToolName::EvidenceAttach),
            },
            ToolSchema {
                name: ToolName::AskHuman.as_str(),
                description: "Suspend and ask a human a question.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"question": {"type": "string"}},
                    "required": ["question"]
                }),
                cost: CostClass::Blocking,
                requires: requirement_for(ToolName::AskHuman),
            },
        ]
    }

    fn to_action(&self, tool: &str, input: &Value) -> Result<Action> {
        let Some(name) = ToolName::parse(tool) else {
            return Err(TmError::parse(format!("unknown tool `{tool}`")));
        };
        match name {
            ToolName::SearchSemantic
            | ToolName::SearchExact
            | ToolName::SearchRegex
            | ToolName::SearchHybrid
            | ToolName::SymbolReferences
            | ToolName::SymbolCallers
            | ToolName::SymbolCallees
            | ToolName::SymbolRenamePreview
            | ToolName::HistorySearch
            | ToolName::HistoryDeleted => action_read_repo(input),
            ToolName::SymbolDefinition => action_read_from_path_field(input),
            ToolName::SymbolOutline
            | ToolName::HistoryWhy
            | ToolName::FsRead
            | ToolName::FsReadRange
            | ToolName::FsList
            | ToolName::FsStat => action_read_path_field(input),
            ToolName::EditApplyPatch
            | ToolName::EditWriteFile
            | ToolName::EditCreateFile
            | ToolName::EditDeleteFile => action_write_path_field(input),
            ToolName::ShellRun
            | ToolName::ShellQueryOutput
            | ToolName::TestRun
            | ToolName::BuildRun => action_run_argv(input),
            ToolName::GitStatus => action_git_status(input),
            ToolName::GitDiff => action_git_diff(input),
            ToolName::GitLog => action_git_log(input),
            ToolName::GitCommit => action_git_commit(input),
            ToolName::GitBranch | ToolName::GitWorktree => action_git_branch(input),
            ToolName::TicketCreateChild => action_ticket_create_child(input),
            ToolName::TicketDelegate => action_ticket_delegate(input),
            ToolName::TicketSubmit
            | ToolName::TicketComment
            | ToolName::DecisionRecord
            | ToolName::ArtifactStore
            | ToolName::EvidenceAttach => action_ticket_modify(input),
            ToolName::AskHuman => action_ask_human(input),
        }
    }

    fn requires(&self) -> AuthorityRequirement {
        // The builtin capability has no single coarse precondition of its own: its 39 tools
        // span nearly every `AuthorityRequirement` bucket, and each tool's own
        // `ToolSchema::requires` (see `requirement_for`) already carries the real gate. See
        // `ToolSchema`'s doc comment in `tm-types::capability` for why both checks exist.
        AuthorityRequirement::Always
    }

    async fn invoke(&self, tool: &str, input: Value, ctx: &CallContext<'_>) -> Result<Value> {
        let Some(name) = ToolName::parse(tool) else {
            return Err(TmError::parse(format!("unknown tool `{tool}`")));
        };
        let patch_engine = PatchEngine::new(ctx.root.to_path_buf(), ctx.authority.clone());
        self.execute(name, &input, ctx, &patch_engine)
    }
}

/// If `value`'s serialized size is at or under [`MAX_INLINE_RESULT_BYTES`], return it unchanged
/// with no artifact; otherwise store the full value as an artifact via `store` and return a
/// truncated preview referencing it.
///
/// A registry-wide policy applied to every provider's result in [`ToolRegistry::dispatch`]
/// (rather than something each [`CapabilityProvider::invoke`] does for itself): the
/// transcript-size budget this bounds is a property of the dispatch loop, not of any one
/// capability, so [`ToolRegistry`] — not [`BuiltinCapability`] — owns the `Store` handle this
/// needs.
/// The ticket a ticket-scoped tool acts on, or a clear refusal when the calling session has none
/// attached (`docs/decisions/D-017-session-ticket-executor-model.md`).
fn require_ticket<'a>(ctx: &CallContext<'a>, tool: &str) -> Result<&'a TicketId> {
    ctx.ticket.ok_or_else(|| {
        TmError::conflict(format!(
            "{tool} needs a ticket, but this session has none attached; pass an explicit \"ticket\", \
             or create one with ticket.create_child first"
        ))
    })
}

fn bound_result(
    value: Value,
    store: &Store,
    ticket: Option<&TicketId>,
    actor: &ParticipantId,
) -> Result<(Value, Option<ArtifactId>)> {
    let bytes = serde_json::to_vec(&value)?;
    if bytes.len() <= MAX_INLINE_RESULT_BYTES {
        return Ok((value, None));
    }
    let events = store.store_artifact(
        ArtifactKind::Report,
        "application/json".to_string(),
        bytes.clone(),
        json!({"tool_result": true}),
        ticket.cloned(),
        actor.clone(),
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

fn tool_def(schema: &ToolSchema) -> tm_provider::ToolDef {
    tm_provider::ToolDef {
        name: schema.name.to_string(),
        description: schema.description.to_string(),
        input_schema: schema.input_schema.clone(),
    }
}

/// The full catalog of tools an [`crate::agent_loop::AgentLoop`] offers a provider: whatever
/// [`CapabilityProvider`]s a binary assembled, indexed once by wire name.
///
/// `docs/audit-2026-09-18-fable.md` A-01: this is deliberately generic over however many
/// providers are registered — [`ToolRegistry::standard`] registers just [`BuiltinCapability`]
/// today, but nothing here special-cases it; a binary that also wants `tm-browser`/
/// `tm-computer`/... calls [`ToolRegistry::new`] with a longer `providers` list instead, with no
/// change to this struct or its methods.
pub struct ToolRegistry {
    providers: Vec<Arc<dyn CapabilityProvider>>,
    /// Every tool, in provider-registration order and then each provider's own
    /// [`CapabilityProvider::tools`] order — *not* alphabetical, so the tool surface sent to a
    /// model stays byte-stable across calls the way the old hardcoded `Vec`'s order did.
    entries: Vec<(usize, ToolSchema)>,
    /// Wire name -> index into `entries`.
    index: BTreeMap<&'static str, usize>,
    /// See [`bound_result`]'s doc comment for why this lives on the registry rather than on any
    /// one provider.
    store: Arc<Store>,
    /// `PreToolUse`/`PostToolUse` handlers from `hooks.toml` (`docs/audit-2026-09-18-fable.md`
    /// M-04), consulted in [`ToolRegistry::dispatch`] before/after every call. `None` (the
    /// default from every constructor below) means no `hooks.toml` was loaded — every existing
    /// caller of [`ToolRegistry::new`]/[`ToolRegistry::standard`]/
    /// [`ToolRegistry::with_capabilities`] gets this, so `dispatch`'s behavior for a caller that
    /// has not opted in via [`ToolRegistry::with_hooks`] is unchanged. See [`crate::hooks`] for
    /// the config shape and shell-hook execution contract.
    hooks: Option<crate::hooks::HookConfig>,
}

impl ToolRegistry {
    /// Build a registry over `providers`, indexing every tool they contribute once. Later
    /// providers' tools shadow earlier ones' in the wire-name index on a collision (none of the
    /// providers registered today collide).
    pub fn new(providers: Vec<Arc<dyn CapabilityProvider>>, store: Arc<Store>) -> Self {
        let mut entries = Vec::new();
        let mut index = BTreeMap::new();
        for (provider_idx, provider) in providers.iter().enumerate() {
            for schema in provider.tools() {
                index.insert(schema.name, entries.len());
                entries.push((provider_idx, schema));
            }
        }
        ToolRegistry {
            providers,
            entries,
            index,
            store,
            hooks: None,
        }
    }

    /// Attach `hooks.toml`-loaded [`crate::hooks::HookConfig`] to this registry, consulted by
    /// [`ToolRegistry::dispatch`] from this point on. A builder (not a constructor argument) so
    /// every existing call site keeps working unchanged; a caller that wants hooks calls this
    /// once after construction (see `tm-cli`'s `run_turn_streaming`).
    pub fn with_hooks(mut self, hooks: crate::hooks::HookConfig) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// The standard single-provider registry: [`BuiltinCapability`] alone (`SPEC.md` §11's 39
    /// tools), over already-open project infrastructure. Left builtin-only deliberately — several
    /// tests in this module assert exact admitted-tool counts against `ToolName::ALL.len()`
    /// (`standard_registers_every_tool_name_exactly_once`, the `tool_defs_for_*` `- N` counts) and
    /// would need rewriting the moment a second provider joined this constructor. A binary that
    /// also wants `tm-browser`/`tm-computer`/`tm-pty`/an MCP client calls
    /// [`ToolRegistry::with_capabilities`] instead (`docs/audit-2026-09-18-fable.md` A-01/B-02).
    pub fn standard(
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
    ) -> Self {
        let builtin: Arc<dyn CapabilityProvider> = Arc::new(BuiltinCapability::new(
            ci,
            store.clone(),
            command_cache,
            command_executor,
        ));
        ToolRegistry::new(vec![builtin], store)
    }

    /// [`ToolRegistry::standard`] plus whatever additional [`CapabilityProvider`]s a binary
    /// assembled (`docs/audit-2026-09-18-fable.md` B-02: `tm-browser`'s `BrowserCapability`,
    /// `tm-computer`'s `ComputerCapability`, later a PTY or MCP-client provider) — the builtin
    /// capability always comes first, in the same registration-order-preserving slot `standard`
    /// gives it, so a caller migrating from `standard` to this constructor sees byte-identical
    /// builtin tool ordering with the extra providers' tools appended after.
    pub fn with_capabilities(
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
        extra: Vec<Arc<dyn CapabilityProvider>>,
    ) -> Self {
        let builtin: Arc<dyn CapabilityProvider> = Arc::new(BuiltinCapability::new(
            ci,
            store.clone(),
            command_cache,
            command_executor,
        ));
        let mut providers = vec![builtin];
        providers.extend(extra);
        ToolRegistry::new(providers, store)
    }

    /// Look up a tool by its dotted wire name.
    pub fn get(&self, name: &str) -> Option<&ToolSchema> {
        self.index.get(name).map(|&i| &self.entries[i].1)
    }

    /// Every registered schema, in registration order.
    pub fn specs(&self) -> impl Iterator<Item = &ToolSchema> {
        self.entries.iter().map(|(_, schema)| schema)
    }

    /// Render every registered tool as a `tm_provider::ToolDef`, for
    /// `tm_provider::CompletionRequest::tools`.
    pub fn tool_defs(&self) -> Vec<tm_provider::ToolDef> {
        self.entries
            .iter()
            .map(|(_, schema)| tool_def(schema))
            .collect()
    }

    /// Schemas admitted for `authority`, in registration order: a tool is admitted only when
    /// both its provider's [`CapabilityProvider::requires`] and its own
    /// [`tm_types::ToolSchema::requires`] structurally admit `authority` (`SPEC.md` §30.1). See
    /// `tm-types::capability::ToolSchema`'s doc comment for why both checks exist.
    fn admitted<'a>(
        &'a self,
        authority: &'a Authority,
    ) -> impl Iterator<Item = &'a ToolSchema> + 'a {
        self.entries
            .iter()
            .filter_map(move |(provider_idx, schema)| {
                let provider = &self.providers[*provider_idx];
                if provider.requires().admits(authority) && schema.requires.admits(authority) {
                    Some(schema)
                } else {
                    None
                }
            })
    }

    /// [`ToolRegistry::tool_defs`], admitted by authority rather than availability
    /// (`SPEC.md` §30.1): only schemas [`ToolRegistry::admitted`] passes are rendered, so a tool
    /// a ticket's authority structurally cannot exercise never appears in the request's tool
    /// surface at all — not merely a disabled tool that would answer `Authority::permits` with a
    /// denial at dispatch time.
    pub fn tool_defs_for(&self, authority: &Authority) -> Vec<tm_provider::ToolDef> {
        self.admitted(authority).map(tool_def).collect()
    }

    /// The per-tool byte/token cost of exactly the tool surface [`ToolRegistry::tool_defs_for`]
    /// would send for `authority` (`SPEC.md` §30.2): the ecosystem-survey cost the section opens
    /// with — a dozen connected MCP servers' worth of schemas paid on every turn — made
    /// attributable as a line item, the same way [`tm_context::ContextPack`]'s sections are.
    pub fn tool_surface_cost_for(&self, authority: &Authority) -> Vec<tm_context::ToolSurfaceCost> {
        self.admitted(authority)
            .map(|schema| {
                tm_context::ToolSurfaceCost::compute(
                    schema.name,
                    schema.description,
                    &schema.input_schema,
                )
            })
            .collect()
    }

    /// Map a call's input to the [`Action`] `Authority::permits` gates it on, without executing
    /// it — the seam [`crate::agent_loop::AgentLoop`] uses to pre-check a pending call for
    /// `Decision::NeedsApproval` before it ever reaches [`ToolRegistry::dispatch`]. An unknown
    /// tool name is `Err`, never a panic or a silently skipped check — callers that want "skip
    /// on unknown" (as the pre-check does) get that for free from `if let Ok(..) = ...`.
    pub fn to_action(&self, name: &str, input: &Value) -> Result<Action> {
        let &idx = self
            .index
            .get(name)
            .ok_or_else(|| TmError::parse(format!("unknown tool `{name}`")))?;
        let (provider_idx, schema) = &self.entries[idx];
        self.providers[*provider_idx].to_action(schema.name, input)
    }

    /// Dispatch one model-issued call: resolve its provider, map to an [`Action`], check
    /// `ctx.authority`, and — if permitted — execute against `ctx`.
    ///
    /// Never returns `Err`: an unknown tool name, a malformed input, an authority denial, or a
    /// dispatch-time failure are all represented as a variant of [`ToolOutcome`] so the caller
    /// always has a tool result to hand back to the model.
    pub async fn dispatch(&self, call: &ToolCall, ctx: &CallContext<'_>) -> ToolOutcome {
        let Some(&idx) = self.index.get(call.name.as_str()) else {
            return ToolOutcome::Errored {
                detail: format!("unknown tool `{}`", call.name),
            };
        };
        let (provider_idx, schema) = &self.entries[idx];
        let provider = &self.providers[*provider_idx];

        // `PreToolUse` (`docs/audit-2026-09-18-fable.md` M-04): evaluated before `to_action`/
        // `permits` so a configured hook can deny or rewrite a call's input before authority is
        // even consulted — see `crate::hooks`'s module doc comment for the wire contract and
        // fail-closed behavior. `self.hooks` is `None` for every caller that hasn't opted in via
        // `ToolRegistry::with_hooks`, so this is a no-op for them.
        let mut effective_input = call.input.clone();
        if let Some(hooks) = &self.hooks {
            match hooks
                .evaluate_pre_tool_use(call.name.as_str(), &effective_input, ctx)
                .await
            {
                crate::hooks::HookDecision::Allow => {}
                crate::hooks::HookDecision::Deny(reason) => return ToolOutcome::Denied { reason },
                crate::hooks::HookDecision::Rewrite(new_input) => {
                    effective_input = new_input;
                }
            }
        }

        let action = match provider.to_action(schema.name, &effective_input) {
            Ok(action) => action,
            Err(e) => {
                return ToolOutcome::Errored {
                    detail: e.to_string(),
                }
            }
        };
        match ctx.authority.permits(&action) {
            Decision::Allow => {}
            Decision::Deny(reason) => return ToolOutcome::Denied { reason },
            Decision::NeedsApproval(reason) => {
                return ToolOutcome::Errored {
                    detail: format!(
                        "internal error: dispatch reached with an action needing approval ({reason}); \
                         the caller must suspend via AgentOutcome::AwaitingApproval before calling dispatch again"
                    ),
                };
            }
        }
        let outcome = match provider
            .invoke(schema.name, effective_input.clone(), ctx)
            .await
        {
            Ok(value) => match bound_result(value, self.store.as_ref(), ctx.ticket, ctx.actor) {
                Ok((result, artifact)) => ToolOutcome::Completed { result, artifact },
                Err(e) => ToolOutcome::Errored {
                    detail: e.to_string(),
                },
            },
            Err(e) => ToolOutcome::Errored {
                detail: e.to_string(),
            },
        };

        // `PostToolUse`: observational only (the call already happened) — see `crate::hooks`'s
        // module doc comment for why this consumes no decision from the hook.
        if let Some(hooks) = &self.hooks {
            let summary = match &outcome {
                ToolOutcome::Completed { .. } => "completed",
                ToolOutcome::Denied { .. } => "denied",
                ToolOutcome::Errored { .. } => "errored",
            };
            hooks
                .run_post_tool_use(call.name.as_str(), &effective_input, summary, ctx)
                .await;
        }

        outcome
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tm_types::{FixedClock, PatternSet, SessionId, TestIds, Timestamp};

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
        dir: TempDir,
        authority: Authority,
        registry: ToolRegistry,
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
            let ci = Arc::new(CodeIntel::open(dir.path()).expect("codeintel"));
            let store = Arc::new(
                Store::open_with(
                    dir.path(),
                    Arc::new(FixedClock::epoch()),
                    Arc::new(TestIds::new()),
                )
                .expect("store"),
            );
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
            let command_cache: Arc<dyn CommandCache + Send + Sync> = Arc::new(FakeCache::new());
            let command_executor: Arc<dyn CommandExecutor + Send + Sync> = Arc::new(FakeExecutor);
            let registry = ToolRegistry::standard(ci, store, command_cache, command_executor);
            Harness {
                dir,
                authority,
                registry,
                clock: FixedClock::epoch(),
                ids: TestIds::new(),
                ticket,
                session: "S-1".parse().unwrap(),
                actor,
            }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn ctx(&self) -> CallContext<'_> {
            CallContext {
                authority: &self.authority,
                ticket: Some(&self.ticket),
                session: &self.session,
                actor: &self.actor,
                clock: &self.clock,
                ids: &self.ids,
                root: self.dir.path(),
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
        let h = Harness::new();
        let names: Vec<&str> = h.registry.specs().map(|s| s.name).collect();
        assert_eq!(names.len(), ToolName::ALL.len());
        for t in ToolName::ALL {
            assert!(
                h.registry.get(t.as_str()).is_some(),
                "missing {}",
                t.as_str()
            );
        }
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "a tool name was registered twice"
        );
    }

    #[test]
    fn tool_defs_carries_every_schema_through() {
        let h = Harness::new();
        let defs = h.registry.tool_defs();
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
        let h = Harness::new();
        let defs = h.registry.tool_defs_for(&Authority::root());
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
        let mut h = Harness::new();
        h.authority.shell.enabled = false;
        let defs = h.registry.tool_defs_for(&h.authority);
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

        let h = Harness::new();
        let with_network = h.registry.tool_defs_for(&enabled);
        let without_network = h.registry.tool_defs_for(&disabled);

        assert_eq!(with_network.len(), without_network.len());
        assert_eq!(def_names(&with_network), def_names(&without_network));
    }

    #[test]
    fn tool_defs_for_write_disabled_omits_every_write_shaped_tool_entirely() {
        let mut h = Harness::new();
        h.authority.repository.write = PatternSet::empty();
        let defs = h.registry.tool_defs_for(&h.authority);
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
        let mut h = Harness::new();
        h.authority.repository.read = PatternSet::empty();
        let defs = h.registry.tool_defs_for(&h.authority);
        let names = def_names(&defs);

        assert!(!names.contains(&"fs.read"));
        assert!(!names.contains(&"search.exact"));
        assert!(!names.contains(&"symbol.outline"));
        assert!(names.contains(&"edit.write_file"));
        assert_eq!(defs.len(), ToolName::ALL.len() - 17);
    }

    #[test]
    fn tool_defs_for_none_authority_admits_nothing() {
        let h = Harness::new();
        let defs = h.registry.tool_defs_for(&Authority::none());
        assert!(
            defs.is_empty(),
            "no-authority should admit zero tools, got {:?}",
            def_names(&defs)
        );
    }

    #[tokio::test]
    async fn filter_is_an_economy_optimization_not_a_replacement_for_dispatch_time_enforcement() {
        // A tool can be admitted into the surface (its structural precondition holds) while a
        // *specific* call still gets denied by `Authority::permits` at dispatch time — the
        // filter narrows what's offered, it does not widen what's allowed.
        let mut h = Harness::new();
        h.authority.repository.read = PatternSet::parse(["src/**"]).unwrap();

        let defs = h.registry.tool_defs_for(&h.authority);
        assert!(
            def_names(&defs).contains(&"fs.read"),
            "fs.read should be admitted: repository.read is nonempty"
        );

        let outcome = h
            .registry
            .dispatch(
                &call("fs.read", json!({"path": "other/secret.txt"})),
                &h.ctx(),
            )
            .await;
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
        let h = Harness::new();
        let authority = Authority::root();
        let defs = h.registry.tool_defs_for(&authority);
        let costs = h.registry.tool_surface_cost_for(&authority);

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
        let h = Harness::new();
        let root_cost: usize = h
            .registry
            .tool_surface_cost_for(&Authority::root())
            .iter()
            .map(|c| c.bytes)
            .sum();

        let mut scoped = Authority::root();
        scoped.shell.enabled = false;
        scoped.repository.write = PatternSet::empty();
        let scoped_cost: usize = h
            .registry
            .tool_surface_cost_for(&scoped)
            .iter()
            .map(|c| c.bytes)
            .sum();

        assert!(
            scoped_cost < root_cost,
            "a narrower authority should pay less tool-surface rent: {scoped_cost} >= {root_cost}"
        );
    }

    #[tokio::test]
    async fn dispatch_reports_unknown_tool_as_errored_not_panic() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(&call("nope.nope", json!({})), &h.ctx())
            .await;
        assert!(matches!(outcome, ToolOutcome::Errored { .. }));
    }

    #[tokio::test]
    async fn dispatch_reports_malformed_input_as_errored() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(&call("fs.read", json!({})), &h.ctx())
            .await;
        assert!(matches!(outcome, ToolOutcome::Errored { .. }));
    }

    /// `docs/audit-2026-09-18-fable.md` M-04's required test: a real `hooks.toml` `PreToolUse`
    /// hook that denies a specific tool call actually blocks it through the real
    /// `ToolRegistry::dispatch` path (not just `HookConfig::evaluate_pre_tool_use` in isolation,
    /// which `crate::hooks`'s own test module already covers) — `Authority::permits` alone would
    /// allow this call (`Authority::root()`), so a `Denied` outcome here can only be the hook.
    #[tokio::test]
    async fn dispatch_pre_tool_use_hook_denies_a_matching_call() {
        let mut h = Harness::new();
        let mut hooks = crate::hooks::HookConfig::default();
        hooks.pre_tool_use.push(crate::hooks::HookEntry {
            matcher: Some("fs.read".to_string()),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '{"decision":"deny","reason":"blocked by hooks.toml"}'"#.to_string(),
            ],
        });
        h.registry = h.registry.with_hooks(hooks);

        std::fs::write(h.root().join("hello.py"), "print(1)\n").unwrap();
        let outcome = h
            .registry
            .dispatch(&call("fs.read", json!({"path": "hello.py"})), &h.ctx())
            .await;
        assert_eq!(
            outcome,
            ToolOutcome::Denied {
                reason: "blocked by hooks.toml".to_string()
            }
        );
    }

    /// ...and one that allows does not.
    #[tokio::test]
    async fn dispatch_pre_tool_use_hook_allow_does_not_block() {
        let mut h = Harness::new();
        let mut hooks = crate::hooks::HookConfig::default();
        hooks.pre_tool_use.push(crate::hooks::HookEntry {
            matcher: Some("fs.read".to_string()),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '{"decision":"allow"}'"#.to_string(),
            ],
        });
        h.registry = h.registry.with_hooks(hooks);

        std::fs::write(h.root().join("hello.py"), "print(1)\n").unwrap();
        let outcome = h
            .registry
            .dispatch(&call("fs.read", json!({"path": "hello.py"})), &h.ctx())
            .await;
        assert!(matches!(outcome, ToolOutcome::Completed { .. }));
    }

    #[tokio::test]
    async fn dispatch_denies_a_read_outside_authority_scope() {
        let mut h = Harness::new();
        h.authority.repository.read = PatternSet::empty();
        let outcome = h
            .registry
            .dispatch(&call("fs.read", json!({"path": "hello.py"})), &h.ctx())
            .await;
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
    }

    #[tokio::test]
    async fn a_denial_never_touches_the_filesystem() {
        let mut h = Harness::new();
        h.authority.repository.write = PatternSet::empty();
        let outcome = h
            .registry
            .dispatch(
                &call(
                    "edit.create_file",
                    json!({"path": "new.txt", "content": "hi"}),
                ),
                &h.ctx(),
            )
            .await;
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
        assert!(!h.root().join("new.txt").exists());
    }

    #[tokio::test]
    async fn fs_read_round_trips_written_content() {
        let h = Harness::new();
        std::fs::write(h.root().join("hello.txt"), "hi there").unwrap();
        let outcome = h
            .registry
            .dispatch(&call("fs.read", json!({"path": "hello.txt"})), &h.ctx())
            .await;
        match outcome {
            ToolOutcome::Completed { result, artifact } => {
                assert_eq!(result["content"], "hi there");
                assert!(artifact.is_none());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fs_read_rejects_a_path_escaping_the_root() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(
                &call("fs.read", json!({"path": "../outside.txt"})),
                &h.ctx(),
            )
            .await;
        assert!(matches!(outcome, ToolOutcome::Errored { .. }));
    }

    #[tokio::test]
    async fn fs_stat_reports_absence_without_erroring() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(&call("fs.stat", json!({"path": "missing.txt"})), &h.ctx())
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["exists"], false),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn edit_create_file_then_write_file_round_trips() {
        let h = Harness::new();
        let created = h
            .registry
            .dispatch(
                &call(
                    "edit.create_file",
                    json!({"path": "a.txt", "content": "one"}),
                ),
                &h.ctx(),
            )
            .await;
        let ToolOutcome::Completed { result, .. } = created else {
            panic!("expected Completed");
        };
        assert_eq!(result["applied"], true);
        assert_eq!(
            std::fs::read_to_string(h.root().join("a.txt")).unwrap(),
            "one"
        );
    }

    #[tokio::test]
    async fn edit_write_file_conflict_is_completed_not_errored() {
        let h = Harness::new();
        std::fs::write(h.root().join("a.txt"), "one").unwrap();
        let outcome = h
            .registry
            .dispatch(
                &call(
                    "edit.write_file",
                    json!({"path": "a.txt", "content": "two", "expected_hash": "deadbeef"}),
                ),
                &h.ctx(),
            )
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["applied"], false),
            other => panic!("expected Completed(applied=false), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_exact_finds_a_written_literal() {
        let h = Harness::new();
        std::fs::write(h.root().join("m.py"), "def unique_marker(): pass\n").unwrap();
        let outcome = h
            .registry
            .dispatch(
                &call("search.exact", json!({"query": "unique_marker"})),
                &h.ctx(),
            )
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => {
                assert_eq!(result["hits"].as_array().unwrap().len(), 1);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn symbol_outline_of_an_unindexed_file_is_empty() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(
                &call("symbol.outline", json!({"path": "nope.rs"})),
                &h.ctx(),
            )
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => assert!(result.as_array().unwrap().is_empty()),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_run_commits_command_started_and_completed_events() {
        // `docs/audit-2026-09-18-fable.md` B-07: `command::run`'s own drafts previously had no
        // `Store` entry point to land on and were silently dropped (see this method's own doc
        // comment); assert they now actually reach the durable log.
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(
                &call(
                    "shell.run",
                    json!({"argv": ["echo", "hi"], "cacheable": false}),
                ),
                &h.ctx(),
            )
            .await;
        assert!(matches!(outcome, ToolOutcome::Completed { .. }));

        let db_path = h.root().join(".tm").join("project.db");
        // A fresh clock instance is fine here: `read_from` never touches it, only `append` does.
        let clock: Arc<dyn tm_types::Clock> = Arc::new(FixedClock::epoch());
        let log = tm_events::EventLog::open_with_clock(&db_path, clock).expect("open log");
        let events = log.read_from(1, 1024).expect("read_from");

        let started = events
            .iter()
            .find(|e| e.kind == tm_events::EventKind::CommandStarted)
            .expect("command.started landed");
        let completed = events
            .iter()
            .find(|e| e.kind == tm_events::EventKind::CommandCompleted)
            .expect("command.completed landed");
        assert_eq!(
            started
                .payload
                .as_command_started()
                .expect("command.started payload")
                .command,
            "echo hi"
        );
        assert_eq!(
            completed
                .payload
                .as_command_completed()
                .expect("command.completed payload")
                .exit_code,
            0
        );
    }

    #[tokio::test]
    async fn shell_run_executes_via_the_injected_executor() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(
                &call(
                    "shell.run",
                    json!({"argv": ["echo", "hi"], "cacheable": true}),
                ),
                &h.ctx(),
            )
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["exit_code"], 0),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    // ---- git.commit idempotent-effect wrapping (SPEC.md §21.5, audit B-11) ----------------

    fn effects_rows(h: &Harness, kind: &str) -> Vec<(String, Option<String>)> {
        let db_path = h.root().join(".tm").join("project.db");
        let conn = tm_events::schema::open_read_connection(&db_path).expect("read connection");
        let mut stmt = conn
            .prepare("SELECT status, receipt_artifact FROM effects WHERE kind = ?1")
            .expect("prepare");
        stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query_map")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect rows")
    }

    #[tokio::test]
    async fn git_commit_journals_and_completes_an_effect_with_a_receipt() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(
                &call("git.commit", json!({"message": "add feature"})),
                &h.ctx(),
            )
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["exit_code"], 0),
            other => panic!("expected Completed, got {other:?}"),
        }

        let rows = effects_rows(&h, "git.commit");
        assert_eq!(rows.len(), 1, "exactly one journaled effect");
        assert_eq!(rows[0].0, "completed");
        assert!(
            rows[0].1.is_some(),
            "receipt_artifact recorded on completion"
        );
    }

    #[tokio::test]
    async fn git_commit_is_idempotent_across_repeated_calls_with_the_same_message() {
        let h = Harness::new();
        let commit_call = call("git.commit", json!({"message": "add feature"}));

        let first = h.registry.dispatch(&commit_call, &h.ctx()).await;
        assert!(matches!(first, ToolOutcome::Completed { .. }));
        let second = h.registry.dispatch(&commit_call, &h.ctx()).await;
        match second {
            ToolOutcome::Completed { result, .. } => {
                assert_eq!(result["skipped_already_completed"], json!(true));
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // Only one `effects` row -- the identical (ticket, attempt, kind, message) effect was
        // never re-journaled -- and only one `command.started`/`command.completed` pair, proving
        // the underlying `git commit` was genuinely not re-run the second time.
        let rows = effects_rows(&h, "git.commit");
        assert_eq!(rows.len(), 1);

        let db_path = h.root().join(".tm").join("project.db");
        let clock: Arc<dyn tm_types::Clock> = Arc::new(FixedClock::epoch());
        let log = tm_events::EventLog::open_with_clock(&db_path, clock).expect("open log");
        let events = log.read_from(1, 1024).expect("read_from");
        let started_count = events
            .iter()
            .filter(|e| e.kind == tm_events::EventKind::CommandStarted)
            .count();
        assert_eq!(
            started_count, 1,
            "the second, already-completed call must not re-run the command"
        );
    }

    #[tokio::test]
    async fn git_commit_with_a_different_message_is_a_distinct_effect() {
        let h = Harness::new();
        h.registry
            .dispatch(
                &call("git.commit", json!({"message": "first message"})),
                &h.ctx(),
            )
            .await;
        h.registry
            .dispatch(
                &call("git.commit", json!({"message": "second message"})),
                &h.ctx(),
            )
            .await;

        let rows = effects_rows(&h, "git.commit");
        assert_eq!(
            rows.len(),
            2,
            "a different commit message is a different effect"
        );
    }

    #[tokio::test]
    async fn shell_run_is_denied_without_shell_authority() {
        let mut h = Harness::new();
        h.authority.shell.enabled = false;
        let outcome = h
            .registry
            .dispatch(
                &call("shell.run", json!({"argv": ["echo", "hi"]})),
                &h.ctx(),
            )
            .await;
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
    }

    #[tokio::test]
    async fn shell_query_output_reports_not_found_before_any_run() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(
                &call(
                    "shell.query_output",
                    json!({"argv": ["echo", "hi"], "cacheable": true, "stream": "stdout", "query_type": "head", "n": 1}),
                ),
                &h.ctx(),
            )
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["found"], false),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_query_output_reads_back_a_cached_runs_stdout() {
        let h = Harness::new();
        h.registry
            .dispatch(
                &call(
                    "shell.run",
                    json!({"argv": ["echo", "hi"], "cacheable": true}),
                ),
                &h.ctx(),
            )
            .await;
        let outcome = h
            .registry
            .dispatch(
                &call(
                    "shell.query_output",
                    json!({"argv": ["echo", "hi"], "cacheable": true, "stream": "stdout", "query_type": "head", "n": 1}),
                ),
                &h.ctx(),
            )
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["found"], true),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ticket_create_child_is_denied_without_create_children_authority() {
        let mut h = Harness::new();
        h.authority.tickets.create_children = false;
        let input = json!({
            "kind": "work",
            "objective": "do a thing",
            "authority": Authority::default(),
        });
        let outcome = h
            .registry
            .dispatch(&call("ticket.create_child", input), &h.ctx())
            .await;
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
    }

    #[tokio::test]
    async fn ticket_create_child_succeeds_with_authority() {
        let h = Harness::new();
        let input = json!({
            "kind": "work",
            "objective": "do a thing",
            "authority": Authority::default(),
        });
        let outcome = h
            .registry
            .dispatch(&call("ticket.create_child", input), &h.ctx())
            .await;
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

    #[tokio::test]
    async fn ask_human_is_completed_when_delegate_authority_is_held() {
        let h = Harness::new();
        let outcome = h
            .registry
            .dispatch(&call("ask.human", json!({"question": "?"})), &h.ctx())
            .await;
        match outcome {
            ToolOutcome::Completed { result, .. } => assert_eq!(result["question"], "?"),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_human_is_denied_without_delegate_authority() {
        let mut h = Harness::new();
        h.authority.tickets.delegate_children = false;
        let outcome = h
            .registry
            .dispatch(&call("ask.human", json!({"question": "?"})), &h.ctx())
            .await;
        assert!(matches!(outcome, ToolOutcome::Denied { .. }));
    }

    #[test]
    fn bound_result_inlines_a_small_value() {
        let h = Harness::new();
        let ctx = h.ctx();
        let (result, artifact) = bound_result(
            json!({"ok": true}),
            h.registry.store.as_ref(),
            ctx.ticket,
            ctx.actor,
        )
        .unwrap();
        assert_eq!(result, json!({"ok": true}));
        assert!(artifact.is_none());
    }

    #[test]
    fn bound_result_spills_a_large_value_to_an_artifact() {
        let h = Harness::new();
        let ctx = h.ctx();
        let big = "x".repeat(MAX_INLINE_RESULT_BYTES + 1);
        let (result, artifact) = bound_result(
            json!({"text": big}),
            h.registry.store.as_ref(),
            ctx.ticket,
            ctx.actor,
        )
        .unwrap();
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

    // -----------------------------------------------------------------------------------------
    // docs/audit-2026-09-18-fable.md B-02: browser/computer providers are admitted by authority
    // exactly like the builtin one — mirrors the `tool_defs_for_*_disabled_omits_*` tests above,
    // now across providers instead of within one.
    // -----------------------------------------------------------------------------------------

    struct NoopBrowserProvider;

    #[async_trait::async_trait]
    impl tm_browser::provider::BrowserProvider for NoopBrowserProvider {
        fn id(&self) -> &str {
            "noop"
        }
        fn capabilities(&self) -> tm_browser::provider::BrowserCapabilities {
            tm_browser::provider::BrowserCapabilities::default()
        }
        async fn acquire(
            &self,
            _req: &tm_browser::provider::SessionRequest,
        ) -> Result<tm_browser::provider::BrowserEndpoint> {
            Err(TmError::invariant("not used in this test"))
        }
        async fn release(&self, _endpoint: &tm_browser::provider::BrowserEndpoint) -> Result<()> {
            Ok(())
        }
    }

    struct NullSink;

    impl tm_browser::session::ArtifactSink for NullSink {
        fn store(&self, _bytes: &[u8], _content_type: &str) -> Result<ArtifactId> {
            ArtifactId::new("A-test")
        }
    }

    fn test_browser_capability() -> tm_browser::BrowserCapability {
        let providers = Arc::new(
            tm_browser::ProviderRegistry::new(
                vec![Arc::new(NoopBrowserProvider)],
                vec!["noop".to_string()],
            )
            .expect("valid provider registry"),
        );
        let sessions = tm_browser::SessionRegistry::new(
            providers,
            Arc::new(NullSink),
            Arc::new(FixedClock::epoch()) as Arc<dyn tm_types::Clock>,
            1024,
        );
        tm_browser::BrowserCapability::new(sessions)
    }

    fn test_computer_capability() -> tm_computer::ComputerCapability {
        let sessions = tm_computer::ComputerSessionRegistry::new(
            tm_computer::SelectionEnv::default(),
            false,
            tm_computer::session::PanicStopConfig {
                abort_chord: None,
                mouse_move_threshold_px: 5.0,
            },
        );
        tm_computer::ComputerCapability::new(sessions)
    }

    /// A [`ToolRegistry`] over the builtin capability plus a real `tm-browser`/`tm-computer`
    /// provider pair, so admission can be exercised through the real dispatch-facing type
    /// (`ToolRegistry::tool_defs_for`) rather than each provider's own `requires()` in isolation.
    fn registry_with_browser_and_computer() -> (TempDir, ToolRegistry) {
        let dir = TempDir::new().expect("tempdir");
        let ci = Arc::new(CodeIntel::open(dir.path()).expect("codeintel"));
        let store = Arc::new(
            Store::open_with(
                dir.path(),
                Arc::new(FixedClock::epoch()),
                Arc::new(TestIds::new()),
            )
            .expect("store"),
        );
        let command_cache: Arc<dyn CommandCache + Send + Sync> = Arc::new(FakeCache::new());
        let command_executor: Arc<dyn CommandExecutor + Send + Sync> = Arc::new(FakeExecutor);

        let browser: Arc<dyn CapabilityProvider> = Arc::new(test_browser_capability());
        let computer: Arc<dyn CapabilityProvider> = Arc::new(test_computer_capability());

        let registry = ToolRegistry::with_capabilities(
            ci,
            store,
            command_cache,
            command_executor,
            vec![browser, computer],
        );
        (dir, registry)
    }

    #[test]
    fn no_authority_admits_neither_browser_nor_computer_tools() {
        let (_dir, registry) = registry_with_browser_and_computer();
        let defs = registry.tool_defs_for(&Authority::none());
        assert!(
            !defs.iter().any(|d| d.name.starts_with("browser.")),
            "browser.* tools must not be admitted without network authority: {:?}",
            defs.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
        assert!(
            !defs.iter().any(|d| d.name.starts_with("computer.")),
            "computer.* tools must not be admitted without any computer authority: {:?}",
            defs.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
        // The builtin capability's own admission behavior is unaffected by the extra providers.
        assert!(defs.is_empty());
    }

    #[test]
    fn root_authority_admits_browser_and_computer_tools_alongside_the_builtin_set() {
        let (_dir, registry) = registry_with_browser_and_computer();
        let defs = registry.tool_defs_for(&Authority::root());
        assert!(defs.iter().any(|d| d.name == "browser.navigate"));
        assert!(defs.iter().any(|d| d.name == "computer.click"));
        assert!(defs.iter().any(|d| d.name == "computer.clipboard_get"));
        assert!(defs.iter().any(|d| d.name == "fs.read"));
        let expected = ToolName::ALL.len()
            + test_browser_capability().tools().len()
            + test_computer_capability().tools().len();
        assert_eq!(defs.len(), expected);
    }

    #[test]
    fn computer_input_is_admitted_only_when_that_specific_authority_field_is_granted() {
        let (_dir, registry) = registry_with_browser_and_computer();
        let mut authority = Authority::none();
        authority.computer.capture = true; // a different computer power, not input
        let defs = registry.tool_defs_for(&authority);
        assert!(!defs.iter().any(|d| d.name == "computer.click"));
        assert!(defs.iter().any(|d| d.name == "computer.snapshot"));

        authority.computer.input = true;
        let defs = registry.tool_defs_for(&authority);
        assert!(defs.iter().any(|d| d.name == "computer.click"));
    }

    fn ctx_over<'a>(
        authority: &'a Authority,
        ticket: &'a TicketId,
        session: &'a SessionId,
        actor: &'a ParticipantId,
        clock: &'a FixedClock,
        ids: &'a TestIds,
        root: &'a Path,
    ) -> CallContext<'a> {
        CallContext {
            authority,
            ticket: Some(ticket),
            session,
            actor,
            clock,
            ids,
            root,
        }
    }

    // The two tests below exercise `ToolRegistry::dispatch` itself, not `to_action` +
    // `Authority::permits` composed by hand: `dispatch` is what actually guarantees `permits` is
    // checked *before* `provider.invoke` ever runs, mirroring
    // `filter_is_an_economy_optimization_not_a_replacement_for_dispatch_time_enforcement` above
    // for the builtin provider. Both fixture providers fail on `acquire`/backend-open, so a
    // `ToolOutcome::Denied` (not `Errored`) is proof the denial happened before invocation was
    // ever attempted — an `Errored` result here would mean `invoke` ran first and failed instead.

    #[tokio::test]
    async fn browser_navigate_to_a_disallowed_origin_is_denied_before_any_session_launches() {
        let (dir, registry) = registry_with_browser_and_computer();
        let mut authority = Authority::none();
        authority
            .network
            .allowlist
            .insert("allowed.example".to_string());
        let ticket: TicketId = "T-1".parse().unwrap();
        let session: SessionId = "S-1".parse().unwrap();
        let actor: ParticipantId = "agent:test/worker".parse().unwrap();
        let clock = FixedClock::epoch();
        let ids = TestIds::new();
        let ctx = ctx_over(
            &authority,
            &ticket,
            &session,
            &actor,
            &clock,
            &ids,
            dir.path(),
        );

        let outcome = registry
            .dispatch(
                &call("browser.navigate", json!({"url": "https://evil.example/x"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(outcome, ToolOutcome::Denied { .. }),
            "expected Denied (permits checked before invoke), got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn computer_click_without_input_authority_is_denied_before_any_backend_opens() {
        let (dir, registry) = registry_with_browser_and_computer();
        let authority = Authority::none();
        let ticket: TicketId = "T-1".parse().unwrap();
        let session: SessionId = "S-1".parse().unwrap();
        let actor: ParticipantId = "agent:test/worker".parse().unwrap();
        let clock = FixedClock::epoch();
        let ids = TestIds::new();
        let ctx = ctx_over(
            &authority,
            &ticket,
            &session,
            &actor,
            &clock,
            &ids,
            dir.path(),
        );

        let outcome = registry
            .dispatch(&call("computer.click", json!({"x": 1.0, "y": 2.0})), &ctx)
            .await;
        assert!(
            matches!(outcome, ToolOutcome::Denied { .. }),
            "expected Denied (permits checked before invoke), got {outcome:?}"
        );
    }
}
