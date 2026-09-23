//! The complete `tm` command tree.
//!
//! This module is parsing only: every field here is data, nothing here touches a project,
//! a network, or a clock. [`crate::main`] parses argv into [`Cli`] and hands the result to the
//! execution modules (`project`, `tickets`, `sched`, `search`, `ops`, `drive`, `serve`, `agent`)
//! that actually do the work. Keeping parsing and execution apart is what makes both testable in
//! isolation: this module is tested by parsing argv and asserting the parsed structure, with no
//! project on disk required.
//!
//! Bare `tm` (no subcommand) opens the interactive coding agent; `tm -p <prompt>` is its
//! scriptable, non-interactive form. Every other invocation is one of the subcommands below.
//!
//! D-003: `--project` always means repo scope at that exact path (unchanged, creates-if-absent).
//! With no `--project`, every subcommand resolves a `.tm/` above the current directory the same
//! way it always has, but now falls back further, to a project kept entirely outside the
//! workspace under `$TM_HOME/projects/<key>/` (see [`crate::project::resolve_scope`]) — bare `tm`
//! may create that fallback silently; every other subcommand only ever opens it, erroring
//! `NotFound` when nothing exists yet in either scope. `tm project show|list` inspects that
//! resolution directly.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// `tm`: the Ticketmaster command line.
///
/// With no subcommand, `tm` opens the interactive coding agent in the current project (walking
/// up from the current directory for a `.tm` directory, per [`crate::project::locate`]). `tm -p
/// <prompt>` runs that same agent non-interactively for one prompt, suitable for scripting.
#[derive(Debug, Parser)]
#[command(name = "tm", version, about, long_about = None)]
pub struct Cli {
    /// Flags shared by every subcommand.
    #[command(flatten)]
    pub global: GlobalOpts,

    /// Run one prompt to completion and exit (scriptable; see `--json`). Exits 0 on a reply, 2
    /// when the agent failed the task, 4 when it ran out of budget.
    #[arg(short = 'p', long = "prompt", value_name = "TEXT")]
    pub prompt: Option<String>,

    /// Continue the most recent conversation in this project.
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    pub continue_session: bool,

    /// Resume a saved conversation by id (`S-12`). Without an id, list saved conversations.
    #[arg(
        short = 'r',
        long = "resume",
        value_name = "SESSION",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub resume: Option<String>,

    /// The subcommand to run; `None` means the bare-`tm` agent.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Flags every subcommand accepts, declared once and flattened everywhere via `global = true`.
#[derive(Debug, Clone, Args)]
pub struct GlobalOpts {
    /// Emit machine-readable JSON instead of human-formatted output. Every subcommand's JSON
    /// schema is stable and snapshot-tested; human output is not.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress non-essential output; errors and explicitly requested data still print.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Disable ANSI color, regardless of whether stdout is a terminal.
    #[arg(long = "no-color", global = true)]
    pub no_color: bool,

    /// Force the plain, linear bare-`tm` loop instead of the ratatui TUI, even on a real tty:
    /// same information, no cursor addressing, no alternate screen. For scripting, logging, and
    /// screen readers (D-002, "Terminal surface quality bar").
    #[arg(long, global = true)]
    pub plain: bool,

    /// The project root to operate on: always repo scope at exactly this path, creating it if
    /// absent (unchanged since before D-003). Defaults to resolving scope from the current
    /// directory instead (see [`crate::project::resolve_scope`]): walking up for a `.tm`
    /// directory (see [`crate::project::locate`]), then falling back to a project kept under
    /// `$TM_HOME` outside the workspace.
    #[arg(long, global = true, value_name = "PATH")]
    pub project: Option<PathBuf>,
}

/// Every `tm` subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new Ticketmaster project (`.tm/`) in the current directory.
    Init(InitArgs),

    /// Assimilate an existing repository into a new or existing project.
    Attach(AttachArgs),

    /// Turn a natural-language prompt into a running project via Genesis.
    Genesis(GenesisArgs),

    /// The "since you left" report: what changed, what needs attention, what's next.
    Status(StatusArgs),

    /// Run invariants, the hash-chain check, index health, and computer-use permission probes.
    Doctor(DoctorArgs),

    /// Ticket lifecycle: list, inspect, create, edit, and transition tickets.
    #[command(subcommand)]
    Ticket(TicketCommand),

    /// Dependency edges between tickets.
    #[command(subcommand)]
    Dep(DepCommand),

    /// Milestones: groupings of tickets with their own close/reopen lifecycle.
    #[command(subcommand)]
    Milestone(MilestoneCommand),

    /// Decisions: the durable record of choices made, and their supersession chain.
    #[command(subcommand)]
    Decision(DecisionCommand),

    /// The scheduler: plan, tick, and run the admission/selection loop.
    #[command(subcommand)]
    Sched(SchedCommand),

    /// Leases: who currently holds authority over which ticket/resources.
    #[command(subcommand)]
    Lease(LeaseCommand),

    /// Execute one ticket to completion in the foreground.
    Run(RunArgs),

    /// Search the project's code index.
    Search(SearchArgs),

    /// Symbol-level code intelligence: definitions, references, callers, callees, outlines.
    #[command(subcommand)]
    Symbol(SymbolCommand),

    /// Git history queries: why a line changed, commit/message search, deleted-code search.
    #[command(subcommand)]
    History(HistoryCommand),

    /// Documentation as project state: list, staleness check, reconciliation.
    #[command(subcommand)]
    Docs(DocsCommand),

    /// The provider fabric: registered providers, their health, and a live smoke test.
    #[command(subcommand)]
    Provider(ProviderCommand),

    /// Interactive login for a provider's credential (`SPEC.md` §28.2's auth-adapter layer).
    Auth(AuthArgs),

    /// The project's harness configuration and epoch history.
    #[command(subcommand)]
    Harness(HarnessCommand),

    /// Repository-local benchmark tasks, scored against a seeded mock provider.
    #[command(subcommand)]
    Bench(BenchCommand),

    /// Workflow definitions (`SPEC.md` §25): reusable, parameterized recipes that expand into a
    /// ticket graph.
    #[command(subcommand)]
    Workflow(WorkflowCommand),

    /// External tracker mirrors (GitHub, Linear, Jira, GitLab).
    #[command(subcommand)]
    Mirror(MirrorCommand),

    /// Project scaffolding templates: the registry `tm-genesis` selects from by capability tag.
    #[command(subcommand)]
    Templates(TemplatesCommand),

    /// Serve the project over HTTP/SSE, optionally serving the built web client.
    Serve(ServeArgs),

    /// The durable event log: tail, inspect, replay, and verify the hash chain.
    #[command(subcommand)]
    Events(EventsCommand),

    /// Drive a headless browser via `tm-browser`.
    #[command(subcommand)]
    Browser(BrowserCommand),

    /// Drive the real desktop via `tm-computer`.
    #[command(subcommand)]
    Computer(ComputerCommand),

    /// Inspect D-003 scope resolution: where a project's state lives, and every global project
    /// `tm` has ever created under `$TM_HOME`.
    #[command(subcommand)]
    Project(ProjectCommand),

    /// The project wiki (`SPEC.md` §26): generated documentation pages assembled from live
    /// project state and written under `docs/wiki/` at the workspace root.
    #[command(subcommand)]
    Wiki(WikiCommand),
}

/// `tm wiki ...` (`SPEC.md` §26, B-14).
#[derive(Debug, Subcommand)]
pub enum WikiCommand {
    /// Assemble every wiki page family (architecture/<crate>, decisions, history/<path>,
    /// tickets, glossary) from the project's current state and write them under `docs/wiki/`,
    /// skipping any page a human has since marked `maintained`/`human`.
    Generate(WikiGenerateArgs),
}

/// `tm wiki generate`
#[derive(Debug, Args)]
pub struct WikiGenerateArgs {
    /// Report what would be written (and what would be skipped as human/maintained-owned)
    /// without touching disk or the project store.
    #[arg(long)]
    pub dry_run: bool,
}

/// `tm project ...` (D-003).
#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Show the resolved scope for the current directory (or `--project`): repo or global,
    /// workspace root, state directory, and whether it already exists. Creates nothing.
    Show,
    /// List every project `tm` has ever created under `$TM_HOME/projects/`.
    List,
}

/// `tm init`
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Directory to initialize the project in. Defaults to the current directory.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Create an empty project even if a global session exists for this workspace (D-003 Phase
    /// 1-C): by default, `tm init` promotes an existing `$TM_HOME` session for this workspace
    /// into `<path>/.tm` instead of starting over.
    #[arg(long)]
    pub fresh: bool,
}

/// `tm attach [path]`
#[derive(Debug, Args)]
pub struct AttachArgs {
    /// The repository to assimilate. Defaults to the current directory.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

/// `tm genesis [--prompt <text>|-]`
#[derive(Debug, Args)]
pub struct GenesisArgs {
    /// The seed prompt describing the project to build. Pass `-` to read the prompt from stdin.
    #[arg(long, value_name = "TEXT|-")]
    pub prompt: Option<String>,
}

/// `tm status`
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Only report activity since this many hours ago; defaults to since the caller's last
    /// recorded presence.
    #[arg(long, value_name = "HOURS")]
    pub since_hours: Option<u64>,
}

/// `tm doctor`
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Skip the computer-use permission probes (they can be slow or require prompting on
    /// macOS).
    #[arg(long)]
    pub skip_computer_probe: bool,
}

/// `tm ticket ...`
#[derive(Debug, Subcommand)]
pub enum TicketCommand {
    /// List tickets, optionally filtered.
    List(TicketListArgs),
    /// Show one ticket in full.
    Show(TicketRefArgs),
    /// Create a new ticket.
    New(TicketNewArgs),
    /// Edit mutable fields of an existing ticket.
    Edit(TicketEditArgs),
    /// Make a draft ticket ready for a worker to pick up (`tm run` or `tm sched run`).
    Activate(TicketRefArgs),
    /// Accept a submitted ticket's work as done, closing it.
    Accept(TicketAcceptArgs),
    /// Reject a submitted ticket's work, sending it back for another attempt with your reason.
    Reject(TicketRejectArgs),
    /// Close a ticket (requires it to be verified, unless its kind permits an unverified
    /// close).
    Close(TicketRefArgs),
    /// Cancel a ticket.
    Cancel(TicketCancelArgs),
    /// Reopen a closed or cancelled ticket.
    Reopen(TicketRefArgs),
    /// Print a ticket's parent/child tree.
    Tree(TicketRefArgs),
    /// Create a child ticket delegating part of this ticket's authority/objective.
    Delegate(TicketDelegateArgs),
    /// Submit evidence for a ticket, moving it toward verification.
    Submit(TicketSubmitArgs),
    /// Fork a ticket's materialized state as of a past `seq` into a new ticket lineage
    /// (`docs/decisions/D-008-ticket-checkpoint-fork.md`).
    Fork(TicketForkArgs),
}

/// Identifies a ticket by its `T-...` id.
#[derive(Debug, Args)]
pub struct TicketRefArgs {
    /// The ticket id, e.g. `T-42`.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
}

/// `tm ticket list`
#[derive(Debug, Args)]
pub struct TicketListArgs {
    /// Only list tickets in this state.
    #[arg(long, value_enum)]
    pub state: Option<TicketStateArg>,
    /// Only list tickets in this milestone.
    #[arg(long, value_name = "MILESTONE")]
    pub milestone: Option<String>,
    /// Only list children of this ticket.
    #[arg(long, value_name = "TICKET")]
    pub parent: Option<String>,
}

/// The subset of `tm_core::ticket::TicketState` a user can filter on.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TicketStateArg {
    /// Not yet ready for admission.
    Draft,
    /// Ready to be leased.
    Ready,
    /// Currently leased to an executor.
    Active,
    /// Awaiting verification.
    Verification,
    /// Awaiting audit.
    Audit,
    /// Closed successfully.
    Closed,
    /// Cancelled.
    Cancelled,
    /// Blocked on a failure/retry decision.
    Blocked,
}

/// `tm ticket new`
#[derive(Debug, Args)]
pub struct TicketNewArgs {
    /// The ticket's objective, in natural language.
    #[arg(value_name = "OBJECTIVE")]
    pub objective: String,
    /// Ticket kind (task, investigation, verification, audit, recovery, ...).
    #[arg(long, default_value = "task")]
    pub kind: String,
    /// Parent ticket id, if this is a child.
    #[arg(long, value_name = "TICKET")]
    pub parent: Option<String>,
    /// Milestone id to attach this ticket to.
    #[arg(long, value_name = "MILESTONE")]
    pub milestone: Option<String>,
    /// Scheduling priority; higher runs first among otherwise-eligible tickets.
    #[arg(long, default_value_t = 0)]
    pub priority: i32,
    /// Path patterns this ticket's lease may write to (repeatable).
    #[arg(long = "resource", value_name = "GLOB")]
    pub resources: Vec<String>,
}

/// `tm ticket edit`
#[derive(Debug, Args)]
pub struct TicketEditArgs {
    /// The ticket id to edit.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// New objective text.
    #[arg(long)]
    pub objective: Option<String>,
    /// New scheduling priority.
    #[arg(long)]
    pub priority: Option<i32>,
}

/// `tm ticket accept`
#[derive(Debug, Args)]
pub struct TicketAcceptArgs {
    /// The submitted ticket to accept.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// An optional note recorded with the acceptance.
    #[arg(long)]
    pub note: Option<String>,
}

/// `tm ticket reject`
#[derive(Debug, Args)]
pub struct TicketRejectArgs {
    /// The submitted ticket to reject.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// What is wrong or missing; the next attempt's worker sees this.
    #[arg(long)]
    pub reason: String,
}

/// `tm ticket cancel`
#[derive(Debug, Args)]
pub struct TicketCancelArgs {
    /// The ticket id to cancel.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// Why this ticket is being cancelled.
    #[arg(long)]
    pub reason: Option<String>,
}

/// `tm ticket fork <TICKET> --at <SEQ>`
#[derive(Debug, Args)]
pub struct TicketForkArgs {
    /// The ticket to fork.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// The event-log `seq` to fork the ticket's materialized state as of (inclusive).
    #[arg(long)]
    pub at: u64,
}

/// `tm ticket delegate`
#[derive(Debug, Args)]
pub struct TicketDelegateArgs {
    /// The ticket delegating part of its work.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// The delegated child's objective.
    #[arg(value_name = "OBJECTIVE")]
    pub objective: String,
    /// Path patterns the delegated authority is narrowed to (repeatable); defaults to the
    /// parent's own authority when empty.
    #[arg(long = "resource", value_name = "GLOB")]
    pub resources: Vec<String>,
}

/// `tm ticket submit`
#[derive(Debug, Args)]
pub struct TicketSubmitArgs {
    /// The ticket being submitted.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// Free-form summary of the work done.
    #[arg(long)]
    pub summary: Option<String>,
    /// Evidence artifact paths to attach (repeatable).
    #[arg(long = "evidence", value_name = "PATH")]
    pub evidence: Vec<PathBuf>,
}

/// `tm dep ...`
#[derive(Debug, Subcommand)]
pub enum DepCommand {
    /// Add a dependency edge `ticket -> depends_on`.
    Add(DepEdgeArgs),
    /// Remove a dependency edge.
    Rm(DepEdgeArgs),
    /// Print the dependency graph, or the subgraph rooted at one ticket.
    Graph(DepGraphArgs),
}

/// `tm dep add|rm`
#[derive(Debug, Args)]
pub struct DepEdgeArgs {
    /// The dependent ticket.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// The ticket it depends on.
    #[arg(value_name = "DEPENDS_ON")]
    pub depends_on: String,
    /// Dependency kind: `blocks` (default, acyclic) or `loop` (cyclic, budgeted).
    #[arg(long, default_value = "blocks")]
    pub kind: String,
}

/// `tm dep graph`
#[derive(Debug, Args)]
pub struct DepGraphArgs {
    /// Root the graph at this ticket instead of printing the whole project.
    #[arg(value_name = "TICKET")]
    pub ticket: Option<String>,
}

/// `tm milestone ...`
#[derive(Debug, Subcommand)]
pub enum MilestoneCommand {
    /// List milestones.
    List,
    /// Close a milestone.
    Close(MilestoneRefArgs),
    /// Reopen a closed milestone.
    Reopen(MilestoneRefArgs),
}

/// Identifies a milestone by its `M-...` id.
#[derive(Debug, Args)]
pub struct MilestoneRefArgs {
    /// The milestone id, e.g. `M-3`.
    #[arg(value_name = "MILESTONE")]
    pub milestone: String,
}

/// `tm decision ...`
#[derive(Debug, Subcommand)]
pub enum DecisionCommand {
    /// List decisions.
    List,
    /// Show one decision in full.
    Show(DecisionRefArgs),
    /// Record a new decision.
    New(DecisionNewArgs),
    /// Supersede an existing decision with a new one.
    Supersede(DecisionSupersedeArgs),
}

/// Identifies a decision by its `D-...` id.
#[derive(Debug, Args)]
pub struct DecisionRefArgs {
    /// The decision id, e.g. `D-7`.
    #[arg(value_name = "DECISION")]
    pub decision: String,
}

/// `tm decision new`
#[derive(Debug, Args)]
pub struct DecisionNewArgs {
    /// The decision's summary text.
    #[arg(value_name = "SUMMARY")]
    pub summary: String,
    /// Full rationale.
    #[arg(long)]
    pub rationale: Option<String>,
}

/// `tm decision supersede`
#[derive(Debug, Args)]
pub struct DecisionSupersedeArgs {
    /// The decision being superseded.
    #[arg(value_name = "DECISION")]
    pub decision: String,
    /// The new decision's summary text.
    #[arg(value_name = "SUMMARY")]
    pub summary: String,
    /// Full rationale for the change.
    #[arg(long)]
    pub rationale: Option<String>,
}

/// `tm sched ...`
#[derive(Debug, Subcommand)]
pub enum SchedCommand {
    /// Print the actions the scheduler would take right now, without applying them.
    Plan,
    /// Run one scheduler tick, applying its actions.
    Tick,
    /// Run the scheduler loop continuously (ticks on interval and on event notification).
    Run(SchedRunArgs),
    /// Pause scheduling: admitted work continues, no new leases are granted.
    Pause,
    /// Resume scheduling after `tm sched pause`.
    Resume,
}

/// `tm sched run`
#[derive(Debug, Args)]
pub struct SchedRunArgs {
    /// Tick interval in seconds; defaults to the project's `SchedulingPolicy`.
    #[arg(long)]
    pub interval_secs: Option<u64>,
}

/// `tm lease ...`
#[derive(Debug, Subcommand)]
pub enum LeaseCommand {
    /// List leases.
    List(LeaseListArgs),
    /// Acquire a lease on a ticket for an actor.
    Acquire(LeaseAcquireArgs),
    /// Release a held lease.
    Release(LeaseRefArgs),
    /// Sweep and revert expired leases.
    Expire,
}

/// `tm lease list`
#[derive(Debug, Args)]
pub struct LeaseListArgs {
    /// Only list live leases (default: all).
    #[arg(long)]
    pub live_only: bool,
}

/// `tm lease acquire`
#[derive(Debug, Args)]
pub struct LeaseAcquireArgs {
    /// The ticket to lease.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// The participant acquiring the lease, e.g. `agent:worker-1`.
    #[arg(long)]
    pub actor: String,
}

/// Identifies a lease by its `L-...` id.
#[derive(Debug, Args)]
pub struct LeaseRefArgs {
    /// The lease id, e.g. `L-9`.
    #[arg(value_name = "LEASE")]
    pub lease: String,
}

/// `tm run <ticket>`
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The ticket to execute to completion in the foreground.
    #[arg(value_name = "TICKET")]
    pub ticket: String,
    /// Role to execute as, overriding the ticket's `ExecutorRequirements`.
    #[arg(long)]
    pub role: Option<String>,
    /// Isolate this run in a fresh `git worktree` (a new branch off `HEAD`, under
    /// `<state_dir>/worktrees/<ticket>-<suffix>/`) instead of executing against the main
    /// checkout — requires a repo-scoped project backed by a real git repository with at least
    /// one commit (`docs/decisions/D-012-run-worktree-isolation.md`).
    #[arg(long)]
    pub worktree: bool,
}

/// The retrieval mode for `tm search`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SearchMode {
    /// Literal substring search.
    Exact,
    /// Regular-expression search.
    Regex,
    /// Embedding-vector search.
    Semantic,
    /// Weighted reciprocal-rank fusion across exact, semantic, and symbol signals.
    Hybrid,
}

/// `tm search <query> [--exact|--regex|--semantic|--hybrid]`
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// The search query.
    #[arg(value_name = "QUERY")]
    pub query: String,
    /// Retrieval mode; defaults to hybrid.
    #[arg(long, value_enum, default_value_t = SearchMode::Hybrid)]
    pub mode: SearchMode,
    /// Maximum number of hits to print.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

/// `tm symbol ...`
#[derive(Debug, Subcommand)]
pub enum SymbolCommand {
    /// Find a symbol's definition.
    Def(SymbolQueryArgs),
    /// Find references to a symbol.
    Refs(SymbolQueryArgs),
    /// Find callers of a symbol.
    Callers(SymbolQueryArgs),
    /// Find callees of a symbol.
    Callees(SymbolQueryArgs),
    /// Print the outline (top-level definitions) of a file.
    Outline(SymbolOutlineArgs),
}

/// `tm symbol def|refs|callers|callees`
#[derive(Debug, Args)]
pub struct SymbolQueryArgs {
    /// The symbol name.
    #[arg(value_name = "NAME")]
    pub name: String,
    /// The file the query is issued from, for scope resolution.
    #[arg(long, value_name = "PATH")]
    pub from: Option<PathBuf>,
}

/// `tm symbol outline`
#[derive(Debug, Args)]
pub struct SymbolOutlineArgs {
    /// The file to outline.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

/// `tm history ...`
#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// Explain why a file (optionally at a specific line) is the way it is.
    Why(HistoryWhyArgs),
    /// Search commit messages and diffs.
    Search(HistorySearchArgs),
    /// Search code that used to exist but has since been deleted.
    Deleted(HistorySearchArgs),
}

/// `tm history why <path>[:line]`
#[derive(Debug, Args)]
pub struct HistoryWhyArgs {
    /// `path` or `path:line`.
    #[arg(value_name = "PATH[:LINE]")]
    pub locator: String,
}

/// `tm history search|deleted <query>`
#[derive(Debug, Args)]
pub struct HistorySearchArgs {
    /// The search query.
    #[arg(value_name = "QUERY")]
    pub query: String,
}

/// `tm docs ...`
#[derive(Debug, Subcommand)]
pub enum DocsCommand {
    /// List registered docs and their staleness state.
    List,
    /// Fail (non-zero exit) if any doc is `Stale`.
    Check,
    /// Regenerate `Generated` docs and open review tickets for `Maintained`/`Human` docs whose
    /// basis changed.
    Reconcile,
}

/// `tm templates ...`
#[derive(Debug, Subcommand)]
pub enum TemplatesCommand {
    /// List every template declared in the project's `templates.toml` registry, and whether it
    /// currently resolves (source reachable, checksum pin matches).
    List,
    /// Show one template's manifest: version, tags, params, and checksum status.
    Show(TemplatesShowArgs),
}

/// `tm templates show`
#[derive(Debug, Args)]
pub struct TemplatesShowArgs {
    /// The template id, as declared in `templates.toml`.
    #[arg(value_name = "ID")]
    pub id: String,
}

/// `tm provider ...`
#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// List configured providers and their role routing.
    List,
    /// Inspect the environment and report which of this crate's known provider backends are
    /// actually configured right now, without printing any key material.
    Detect,
    /// Show each provider's availability and whether tm's turn path uses it. No live breaker or
    /// quota state is reported: that exists only inside a running process.
    Status,
    /// Send one tiny real (billed) completion through each provider tm's turn path uses, or just
    /// the named one; exits non-zero if any fails.
    Test(ProviderTestArgs),
}

/// `tm provider test`
#[derive(Debug, Args)]
pub struct ProviderTestArgs {
    /// The provider to test; defaults to every provider tm's turn path registers.
    #[arg(value_name = "PROVIDER")]
    pub provider: Option<String>,
}

/// `tm auth <provider>`
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// The provider to authenticate, e.g. `anthropic`, `openai`, `gemini` — any id `tm provider
    /// detect` lists.
    #[arg(value_name = "PROVIDER")]
    pub provider: String,
}

/// `tm harness ...`
#[derive(Debug, Subcommand)]
pub enum HarnessCommand {
    /// Show the current harness configuration and epoch.
    Show,
    /// Set a harness configuration key, proposing a new epoch.
    Set(HarnessSetArgs),
    /// List the epoch history.
    Epochs,
    /// Promote a pending epoch to current.
    Promote(HarnessPromoteArgs),
}

/// `tm harness set`
#[derive(Debug, Args)]
pub struct HarnessSetArgs {
    /// Dotted config key, e.g. `routing_weights.recency`.
    #[arg(value_name = "KEY")]
    pub key: String,
    /// The new value, parsed as TOML.
    #[arg(value_name = "VALUE")]
    pub value: String,
}

/// `tm harness promote`
#[derive(Debug, Args)]
pub struct HarnessPromoteArgs {
    /// The epoch number to promote. Must equal the current highest persisted epoch (0 if none
    /// has ever been promoted) plus one — a safety check against a stale/racy invocation, since
    /// promotion is append-only.
    #[arg(value_name = "EPOCH")]
    pub epoch: u64,
    /// Skip the benchmark-gain gate (requires explicit human authority).
    #[arg(long)]
    pub force: bool,
    /// The `BenchmarkReport` JSON to promote with (e.g. from `tm bench run --out`). Defaults to
    /// the newest file under `.tm/bench/*.json`. Required unless `--force` and none exists.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,
    /// A prior `BenchmarkReport` JSON to gate the candidate's benchmark against. `harness_epochs`
    /// does not persist a benchmark per epoch, so there is no baseline to compare against unless
    /// one is supplied here; without it (and without `--force`), the gate rejects.
    #[arg(long, value_name = "PATH")]
    pub baseline: Option<PathBuf>,
}

/// `tm bench ...`
#[derive(Debug, Subcommand)]
pub enum BenchCommand {
    /// List available benchmark tasks.
    List,
    /// Run the benchmark suite against the current harness epoch.
    Run(BenchRunArgs),
    /// Compare two benchmark reports.
    Compare(BenchCompareArgs),
}

/// `tm bench run`
#[derive(Debug, Args)]
pub struct BenchRunArgs {
    /// Only run tasks matching this name substring.
    #[arg(long)]
    pub filter: Option<String>,
    /// Where to write the resulting `BenchmarkReport` JSON.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

/// `tm bench compare`
#[derive(Debug, Args)]
pub struct BenchCompareArgs {
    /// The baseline report file.
    #[arg(value_name = "BASELINE")]
    pub baseline: PathBuf,
    /// The candidate report file.
    #[arg(value_name = "CANDIDATE")]
    pub candidate: PathBuf,
}

/// `tm workflow ...`
#[derive(Debug, Subcommand)]
pub enum WorkflowCommand {
    /// List every workflow definition discovered under `.tm/workflows/*.toml`.
    List,
    /// Show one workflow definition's parsed shape.
    Show(WorkflowShowArgs),
    /// Expand a workflow definition against concrete parameters, validate the result, and commit
    /// it as a ticket graph.
    Run(WorkflowRunArgs),
}

/// `tm workflow show`
#[derive(Debug, Args)]
pub struct WorkflowShowArgs {
    /// The workflow's name (its filename under `.tm/workflows/`, without `.toml`).
    #[arg(value_name = "NAME")]
    pub name: String,
}

/// `tm workflow run`
#[derive(Debug, Args)]
pub struct WorkflowRunArgs {
    /// The workflow's name.
    #[arg(value_name = "NAME")]
    pub name: String,
    /// A parameter value as `key=value`, repeatable. Overrides the definition's own declared
    /// default for `key`, if any; a required parameter with no default and no `--param` here is
    /// an error.
    #[arg(long = "param", value_name = "KEY=VALUE")]
    pub param: Vec<String>,
}

/// `tm mirror ...`
#[derive(Debug, Subcommand)]
pub enum MirrorCommand {
    /// Link the project to an external tracker adapter.
    Link(MirrorLinkArgs),
    /// Push local changes to the linked tracker(s).
    Push,
    /// Pull external changes from the linked tracker(s).
    Pull,
    /// Show mirror link status and any recorded degradations.
    Status,
}

/// `tm mirror link`
#[derive(Debug, Args)]
pub struct MirrorLinkArgs {
    /// The adapter to link: `github`, `gitlab`, `jira`, or `linear`.
    #[arg(value_name = "ADAPTER")]
    pub adapter: String,
    /// Additional named credential fields beyond the adapter's default token, as
    /// `field=ENV_VAR_NAME` (the value is an environment variable *name*, never a secret itself
    /// — matching `tm_mirror::CredentialEnv`'s contract). Each adapter's tracker needs more than
    /// a bare token to identify *which* external project/team it talks to: `github` needs
    /// `owner`+`repo`; `gitlab` needs `project_id`; `jira` needs `email`+`api_token` (in place of
    /// the default `token` field) +`project_key`; `linear` needs `api_key` (in place of `token`)
    /// +`team_id`. Repeatable, e.g. `--credential owner=TM_GITHUB_OWNER --credential
    /// repo=TM_GITHUB_REPO`.
    #[arg(long = "credential", value_name = "FIELD=ENV_VAR")]
    pub credentials: Vec<String>,
}

/// `tm serve [--addr]`
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Bind address, e.g. `127.0.0.1:4477`. Defaults to loopback on an ephemeral port.
    #[arg(long, value_name = "ADDR")]
    pub addr: Option<String>,
}

/// `tm events ...`
#[derive(Debug, Subcommand)]
pub enum EventsCommand {
    /// Stream events live, starting from the current head (or `--from`).
    Tail(EventsTailArgs),
    /// Show a single event by sequence number.
    Show(EventsShowArgs),
    /// Replay a range of events through materialization, without appending anything new.
    Replay(EventsReplayArgs),
    /// Verify the hash chain end to end.
    Verify,
}

/// `tm events tail`
#[derive(Debug, Args)]
pub struct EventsTailArgs {
    /// Start from this sequence number instead of the current head.
    #[arg(long)]
    pub from: Option<u64>,
}

/// `tm events show`
#[derive(Debug, Args)]
pub struct EventsShowArgs {
    /// The event's sequence number.
    #[arg(value_name = "SEQ")]
    pub seq: u64,
}

/// `tm events replay`
#[derive(Debug, Args)]
pub struct EventsReplayArgs {
    /// Replay starting from this sequence number (inclusive).
    #[arg(long, default_value_t = 0)]
    pub from: u64,
    /// Replay up to and including this sequence number; defaults to the current head.
    #[arg(long)]
    pub to: Option<u64>,
}

/// `tm browser ...`
#[derive(Debug, Subcommand)]
pub enum BrowserCommand {
    /// Launch a browser session and navigate to a URL.
    Open(BrowserOpenArgs),
    /// Print the accessibility-tree snapshot of the active tab.
    Snapshot,
    /// Click an element by its snapshot ref.
    Click(BrowserRefArgs),
    /// Type text into an element by its snapshot ref.
    Type(BrowserTypeArgs),
    /// Save a screenshot of the active tab.
    Screenshot(BrowserScreenshotArgs),
}

/// `tm browser open`
#[derive(Debug, Args)]
pub struct BrowserOpenArgs {
    /// The URL to navigate to.
    #[arg(value_name = "URL")]
    pub url: String,
    /// Run without a visible browser window. Currently a no-op: the `managed` provider
    /// (§19.1a) always launches headless, and there is no provider yet that can honor a
    /// headed request, so this flag is accepted but ignored rather than silently degraded.
    #[arg(long)]
    pub headless: bool,
}

/// `tm browser click`
#[derive(Debug, Args)]
pub struct BrowserRefArgs {
    /// The snapshot ref of the element, e.g. `ref-12`.
    #[arg(value_name = "REF")]
    pub reference: String,
}

/// `tm browser type`
#[derive(Debug, Args)]
pub struct BrowserTypeArgs {
    /// The snapshot ref of the element to type into.
    #[arg(value_name = "REF")]
    pub reference: String,
    /// The text to type.
    #[arg(value_name = "TEXT")]
    pub text: String,
}

/// `tm browser screenshot`
#[derive(Debug, Args)]
pub struct BrowserScreenshotArgs {
    /// Where to save the screenshot; defaults to an artifact under `.tm/`.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Capture the full scrollable page, not just the viewport.
    #[arg(long)]
    pub full_page: bool,
}

/// `tm computer ...`
#[derive(Debug, Subcommand)]
pub enum ComputerCommand {
    /// Print an observation of the current desktop (accessibility tree, screenshot fallback).
    Snapshot(ComputerSnapshotArgs),
    /// Click at a screen coordinate.
    Click(ComputerClickArgs),
    /// Type text at the current focus.
    Type(ComputerTypeArgs),
    /// Press a key chord, e.g. `cmd+shift+4`.
    Key(ComputerKeyArgs),
}

/// `tm computer snapshot`
#[derive(Debug, Args)]
pub struct ComputerSnapshotArgs {
    /// Force a screenshot even when the accessibility tree is available.
    #[arg(long)]
    pub force_screenshot: bool,
    /// Run against a headless virtual display (Linux only; honestly refused on macOS).
    #[arg(long)]
    pub headless: bool,
}

/// `tm computer click`
#[derive(Debug, Args)]
pub struct ComputerClickArgs {
    /// The x coordinate.
    #[arg(value_name = "X")]
    pub x: f64,
    /// The y coordinate.
    #[arg(value_name = "Y")]
    pub y: f64,
    /// Run against a headless virtual display (Linux only; honestly refused on macOS).
    #[arg(long)]
    pub headless: bool,
}

/// `tm computer type`
#[derive(Debug, Args)]
pub struct ComputerTypeArgs {
    /// The text to type at the current focus.
    #[arg(value_name = "TEXT")]
    pub text: String,
    /// Run against a headless virtual display (Linux only; honestly refused on macOS).
    #[arg(long)]
    pub headless: bool,
}

/// `tm computer key`
#[derive(Debug, Args)]
pub struct ComputerKeyArgs {
    /// The key chord, e.g. `cmd+shift+4` or `ctrl+c`.
    #[arg(value_name = "CHORD")]
    pub chord: String,
    /// Run against a headless virtual display (Linux only; honestly refused on macOS).
    #[arg(long)]
    pub headless: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_tm_means_the_agent() {
        let cli = Cli::parse_from(["tm"]);
        assert!(cli.command.is_none());
        assert!(cli.prompt.is_none());
    }

    #[test]
    fn bare_tm_with_prompt_is_the_scriptable_agent() {
        let cli = Cli::parse_from(["tm", "-p", "fix the build"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.prompt.as_deref(), Some("fix the build"));
    }

    #[test]
    fn ticket_list_parses() {
        let cli = Cli::parse_from(["tm", "ticket", "list", "--state", "ready"]);
        match cli.command {
            Some(Command::Ticket(TicketCommand::List(args))) => {
                assert!(matches!(args.state, Some(TicketStateArg::Ready)));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn global_flags_parse_after_subcommand() {
        let cli = Cli::parse_from(["tm", "status", "--json", "--quiet", "--no-color"]);
        assert!(cli.global.json);
        assert!(cli.global.quiet);
        assert!(cli.global.no_color);
    }

    #[test]
    fn run_requires_a_ticket() {
        let result = Cli::try_parse_from(["tm", "run"]);
        assert!(result.is_err());
    }

    #[test]
    fn workflow_run_parses_repeated_params() {
        let cli = Cli::parse_from([
            "tm",
            "workflow",
            "run",
            "review-change",
            "--param",
            "target=src/lib.rs",
            "--param",
            "reviewer=alice",
        ]);
        match cli.command {
            Some(Command::Workflow(WorkflowCommand::Run(args))) => {
                assert_eq!(args.name, "review-change");
                assert_eq!(
                    args.param,
                    vec![
                        "target=src/lib.rs".to_string(),
                        "reviewer=alice".to_string()
                    ]
                );
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn workflow_list_and_show_parse() {
        let cli = Cli::parse_from(["tm", "workflow", "list"]);
        assert!(matches!(
            cli.command,
            Some(Command::Workflow(WorkflowCommand::List))
        ));

        let cli = Cli::parse_from(["tm", "workflow", "show", "harness-benchmark"]);
        match cli.command {
            Some(Command::Workflow(WorkflowCommand::Show(args))) => {
                assert_eq!(args.name, "harness-benchmark");
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }
}
