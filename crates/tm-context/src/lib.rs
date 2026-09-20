//! Ticketmaster Context: the crate that stops the rest of the system from paying twice for
//! what it already knows.
//!
//! Two jobs, per `SPEC.md` §8:
//!
//! 1. **Context compilation** ([`pack`], [`sections`]): build a bounded, deterministic context
//!    pack for a ticket out of retrieval, decisions, dependency outputs, symbol outlines, git
//!    history and prior failures, instead of dumping raw files at a worker. The pack has a
//!    fixed token budget ([`tokens`]); when the assembled sections don't fit, the
//!    lowest-priority section is dropped first and the drop is recorded in the pack's
//!    provenance, never silently truncated.
//! 2. **Command artifacts** ([`command`]): an expensive command runs at most once per distinct
//!    `(argv, cwd, env, repo state, declared inputs)` key ([`fingerprint`]). Its full stdout
//!    and stderr are stored as artifacts; later questions about the output (head, tail, grep,
//!    line range, JSON pointer) are answered by querying the stored artifact, never by
//!    re-executing.
//!
//! Both jobs are pure functions of their inputs wherever possible: no wall-clock reads, no
//! random ids. Time comes from an injected `&dyn tm_types::Clock`, ids from an injected
//! `&dyn tm_types::IdSource`, so compilation and caching are exactly reproducible in tests.
//!
//! See `SPEC.md` §8.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod command;
pub mod fingerprint;
pub mod pack;
pub mod sections;
pub mod skills;
pub mod tokens;

pub use command::{
    ArtifactStream, CommandCache, CommandExecutor, CommandResult, CommandSpec, ExecutionOutcome,
    Query as CommandQuery, QueryAnswer,
};
pub use fingerprint::{
    cache_key, repo_fingerprint, CacheKeyInputs, DeclaredInput, EnvAllowlist, GitInspector,
    RepoFingerprint,
};
pub use pack::{compile, ContextPack, DroppedSection, ProvenanceRef, Section, ToolSurfaceCost};
pub use sections::{
    build_conventions, build_decisions, build_dependencies, build_git_history, build_objective,
    build_prior_failures, build_retrieval, build_symbol_outlines, RawSection,
};
pub use skills::{discover_skills, load_skill, SkillDoc, SkillMetadata};
pub use tokens::{
    estimate_tokens_prose, estimate_tokens_source, BudgetLedger, SectionAccount, SectionKind,
    TokenBudget,
};
