//! The section builders, one per [`crate::tokens::SectionKind`], in priority order. Each is a
//! standalone, independently testable function: given a ticket (and, where needed, a
//! [`ProjectView`] snapshot or a [`CodeIntel`] handle), it renders one [`RawSection`] with
//! provenance but does not know about token budgets or dropping — [`crate::pack::compile`]
//! is the only place that reconciles sections against a [`crate::tokens::TokenBudget`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tm_codeintel::hybrid::{Query, RetrievalContext, SignalWeights};
use tm_codeintel::CodeIntel;
use tm_core::{ProjectView, Ticket};
use tm_provider::RoleTable;
use tm_types::{Predicate, Result, Role};

use crate::pack::ProvenanceRef;
use crate::tokens::SectionKind;

/// One rendered section before it's admitted (or dropped) by [`crate::pack::compile`]'s
/// budget accounting.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSection {
    /// Which section this is.
    pub kind: SectionKind,
    /// Short human-readable title, e.g. `"Objective"`.
    pub title: String,
    /// Rendered body text.
    pub body: String,
    /// What real-world things (decision ids, file paths, commit shas, artifact ids) this
    /// section's content was drawn from, for [`crate::pack::ContextPack::provenance`].
    pub provenance: Vec<ProvenanceRef>,
}

/// The repository paths a ticket has claimed, derived from its resource claims' underlying
/// glob patterns — the input every path-scoped section (decisions, symbol outlines) filters
/// against.
///
/// Flattens `ticket.resources[].paths.patterns()` into their `as_str()` strings,
/// deduplicated, in first-seen order. Pure, total, no error cases.
pub fn claimed_paths(ticket: &Ticket) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for resource in &ticket.resources {
        for pattern in resource.paths.patterns() {
            let pattern_str = pattern.as_str().to_string();
            if seen.insert(pattern_str.clone()) {
                paths.push(pattern_str);
            }
        }
    }
    paths
}

/// Recursively render a predicate to a string with indentation.
fn render_predicate(predicate: &Predicate, indent: usize) -> String {
    let indent_str = " ".repeat(indent);
    match predicate {
        Predicate::CommandSucceeds { command } => {
            format!("{}CommandSucceeds: {}", indent_str, command.join(" "))
        }
        Predicate::FileExists { path } => {
            format!("{}FileExists: {}", indent_str, path)
        }
        Predicate::FileMatches { path, regex } => {
            format!("{}FileMatches: {} ~ {}", indent_str, path, regex)
        }
        Predicate::TestsPass { suite } => match suite {
            Some(s) => format!("{}TestsPass: {}", indent_str, s),
            None => format!("{}TestsPass: default", indent_str),
        },
        Predicate::TicketClosed { ticket } => {
            format!("{}TicketClosed: {}", indent_str, ticket)
        }
        Predicate::AllOf(ps) => {
            let mut lines = vec![format!("{}AllOf:", indent_str)];
            for p in ps {
                lines.push(render_predicate(p, indent + 2));
            }
            lines.join("\n")
        }
        Predicate::AnyOf(ps) => {
            let mut lines = vec![format!("{}AnyOf:", indent_str)];
            for p in ps {
                lines.push(render_predicate(p, indent + 2));
            }
            lines.join("\n")
        }
        Predicate::Not(p) => {
            let mut lines = vec![format!("{}Not:", indent_str)];
            lines.push(render_predicate(p, indent + 2));
            lines.join("\n")
        }
        Predicate::HumanAttested { note } => {
            format!("{}HumanAttested: {}", indent_str, note)
        }
        Predicate::Judgment { claim } => {
            format!("{}Judgment: {}", indent_str, claim)
        }
    }
}

/// Section 1 (highest priority): the ticket's objective and success predicates.
///
/// Renders `ticket.objective` verbatim, then one line per `ticket.success` predicate via a
/// plain-text rendering of [`tm_types::Predicate`].
pub fn build_objective(ticket: &Ticket) -> RawSection {
    let mut body_lines = vec![ticket.objective.clone()];

    if !ticket.success.is_empty() {
        body_lines.push(String::new());
        for predicate in &ticket.success {
            body_lines.push(render_predicate(predicate, 0));
        }
    }

    let body = body_lines.join("\n");
    let provenance = vec![ProvenanceRef {
        locator: ticket.id.to_string(),
        detail: "ticket objective/success".to_string(),
    }];

    RawSection {
        kind: SectionKind::Objective,
        title: "Objective".to_string(),
        body,
        provenance,
    }
}

/// A ballpark token count for "one provider call", used only to turn a per-token price into a
/// human-legible "roughly how many calls" figure for [`build_budget`]'s tier menu — not an
/// attempt to predict any particular call's real size.
const NOMINAL_CALL_TOKENS: u64 = 2_000;

/// `limit == u64::MAX` means unlimited (`tm_types::Budget`'s convention); render that as the
/// word rather than a `u64::MAX`-sized number no one can act on.
fn fmt_remaining(remaining: u64, limit: u64) -> String {
    if limit == u64::MAX {
        "unlimited".to_string()
    } else {
        remaining.to_string()
    }
}

/// Same as [`fmt_remaining`] but for a micro-dollar amount, rendered as dollars to four decimal
/// places.
fn fmt_remaining_dollars(remaining_micros: u64, limit_micros: u64) -> String {
    if limit_micros == u64::MAX {
        "unlimited".to_string()
    } else {
        format!("{:.4}", remaining_micros as f64 / 1_000_000.0)
    }
}

/// Section 2 (`SPEC.md` §31.1 "legible remaining budget", `docs/audit-2026-09-18-fable.md`
/// B-10): what this ticket can still afford, expressed in terms a model can act on rather than a
/// bare number — absolute remaining spend per dimension, a burn rate/projection derived from
/// usage so far, and `roles`' tier-cost menu (`SPEC.md` §31.2 "tier down before running out") so
/// a worker can see what switching to a cheaper role for a mechanical step would cost.
///
/// Ranked immediately after [`SectionKind::Objective`] (see [`SectionKind::PRIORITY_ORDER`]):
/// budget awareness is cheap to render and operationally load-bearing enough that it should be
/// one of the last sections dropped under a tight context budget, not one of the first.
pub fn build_budget(ticket: &Ticket, roles: &RoleTable) -> RawSection {
    let budget = &ticket.budget;
    let remaining = budget.remaining();

    let mut lines = vec![format!(
        "Remaining: {} tokens, ${}, {} wall-seconds",
        fmt_remaining(remaining.tokens, budget.tokens),
        fmt_remaining_dollars(remaining.dollars_micros, budget.dollars_micros),
        fmt_remaining(remaining.wall_seconds, budget.wall_seconds),
    )];

    if ticket.attempts > 0 && !budget.spent.is_zero() {
        let tokens_per_attempt = budget.spent.tokens / u64::from(ticket.attempts);
        lines.push(format!(
            "Burn rate: ~{tokens_per_attempt} tokens/attempt over {} attempt(s) so far",
            ticket.attempts
        ));
        if tokens_per_attempt > 0 && budget.tokens != u64::MAX {
            let projected = remaining.tokens / tokens_per_attempt;
            lines.push(format!(
                "Projection: roughly {projected} more attempt(s) affordable at this rate"
            ));
        }
    } else {
        lines.push("Burn rate: no usage recorded yet".to_string());
    }

    lines.push(String::new());
    lines.push("Tier menu (primary candidate per role):".to_string());
    for role in Role::ALL {
        let Some(candidate) = roles.candidates_for(role).first() else {
            continue;
        };
        let label = format!(
            "{} ({}/{})",
            role.as_str(),
            candidate.provider,
            candidate.model
        );
        match candidate.price {
            None => lines.push(format!("  {label}: subscription/unmetered capacity")),
            Some(price) => {
                let call_micros = NOMINAL_CALL_TOKENS
                    * (price.input_micros_per_token + price.output_micros_per_token)
                    / 2;
                if call_micros == 0 {
                    lines.push(format!("  {label}: effectively free"));
                } else if budget.dollars_micros == u64::MAX {
                    lines.push(format!("  {label}: unmetered dollar budget"));
                } else {
                    let calls = remaining.dollars_micros / call_micros;
                    lines.push(format!(
                        "  {label}: ~${:.4}/call of ~{NOMINAL_CALL_TOKENS} tokens — ~{calls} call(s) remain affordable",
                        call_micros as f64 / 1_000_000.0
                    ));
                }
            }
        }
    }

    RawSection {
        kind: SectionKind::Budget,
        title: "Budget".to_string(),
        body: lines.join("\n"),
        provenance: Vec::new(),
    }
}

/// Section 3: active decisions affecting the ticket's claimed paths.
///
/// Renders active decisions overlapping claimed_paths(ticket) or naming ticket.id.
pub fn build_decisions(ticket: &Ticket, view: &ProjectView) -> RawSection {
    let paths = claimed_paths(ticket);
    let decisions_vec: Vec<_> = view.decisions.values().collect();

    let mut seen_ids = HashSet::new();
    let mut all_decisions = Vec::new();

    for path in &paths {
        for decision in decisions_vec.iter() {
            if decision.is_active() {
                let pattern_set =
                    tm_types::PatternSet::parse(decision.affected_paths.iter().cloned())
                        .unwrap_or_else(|_| tm_types::PatternSet::empty());
                if pattern_set.matches(path) && seen_ids.insert(decision.id.clone()) {
                    all_decisions.push(*decision);
                }
            }
        }
    }

    for decision in decisions_vec.iter() {
        if decision.is_active()
            && decision.affected_tickets.contains(&ticket.id)
            && seen_ids.insert(decision.id.clone())
        {
            all_decisions.push(*decision);
        }
    }

    all_decisions.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));

    let mut body_lines = Vec::new();
    let mut provenance = Vec::new();

    for decision in all_decisions {
        body_lines.push(format!(
            "{}: {} (why: {})",
            decision.subject, decision.decision, decision.reason
        ));
        provenance.push(ProvenanceRef {
            locator: decision.id.to_string(),
            detail: decision.subject.clone(),
        });
    }

    let body = body_lines.join("\n");

    RawSection {
        kind: SectionKind::Decisions,
        title: "Active Decisions".to_string(),
        body,
        provenance,
    }
}

/// Most open tickets listed in [`build_open_tickets`]; the rest are summarized as a count.
const OPEN_TICKETS_LISTED: usize = 30;

/// An index of the project's open tickets (everything not `Closed`/`Cancelled`), for a chat turn
/// with no ticket of its own: the work running in the background that the session can inspect,
/// steer, attach to, or add to. Highest priority first, then by id; each line is
/// `- <id> [<state>] <first line of objective>`. Empty when there are no open tickets, in which
/// case `tm_context::compile_session` leaves the section out entirely.
pub fn build_open_tickets(view: &ProjectView) -> RawSection {
    use tm_core::TicketState;
    let mut open: Vec<&Ticket> = view
        .tickets
        .values()
        .filter(|t| !matches!(t.state, TicketState::Closed | TicketState::Cancelled))
        .collect();
    open.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id)));

    let mut body_lines = Vec::new();
    let mut provenance = Vec::new();
    for ticket in open.iter().take(OPEN_TICKETS_LISTED) {
        let summary = ticket.objective.lines().next().unwrap_or_default();
        body_lines.push(format!("- {} [{:?}] {}", ticket.id, ticket.state, summary));
        provenance.push(ProvenanceRef {
            locator: ticket.id.to_string(),
            detail: String::new(),
        });
    }
    if open.len() > OPEN_TICKETS_LISTED {
        body_lines.push(format!(
            "- ... and {} more (see ticket.list)",
            open.len() - OPEN_TICKETS_LISTED
        ));
    }

    RawSection {
        kind: SectionKind::Dependencies,
        title: "Open tickets".to_string(),
        body: body_lines.join("\n"),
        provenance,
    }
}

/// Section 4: parent/dependency outputs and evidence.
///
/// Renders dependency/parent ticket summaries + matching evidence from view.evidence.
pub fn build_dependencies(ticket: &Ticket, view: &ProjectView) -> RawSection {
    let mut deps_to_show = Vec::new();

    if let Some(parent_id) = &ticket.parent {
        deps_to_show.push(parent_id.clone());
    }

    deps_to_show.extend(ticket.dependencies.iter().cloned());

    let mut body_lines = Vec::new();
    let mut provenance = Vec::new();

    for dep_id in deps_to_show {
        if let Some(dep_ticket) = view.tickets.get(&dep_id) {
            body_lines.push(format!("**{}**: {}", dep_id, dep_ticket.objective));
            body_lines.push(format!("State: {:?}", dep_ticket.state));

            let dep_evidence: Vec<_> = view
                .evidence
                .iter()
                .filter(|e| e.ticket == dep_id)
                .collect();

            for evidence in dep_evidence {
                body_lines.push(format!(
                    "Evidence: {} ({})",
                    evidence.summary, evidence.artifact
                ));
            }

            provenance.push(ProvenanceRef {
                locator: dep_id.to_string(),
                detail: String::new(),
            });

            body_lines.push(String::new());
        }
    }

    let body = body_lines.join("\n").trim_end().to_string();

    RawSection {
        kind: SectionKind::Dependencies,
        title: "Dependencies & Evidence".to_string(),
        body,
        provenance,
    }
}

/// True when `path` is a wiki page under `docs/wiki/` (`SPEC.md` §26). [`build_retrieval`]
/// excludes hits under this prefix and [`build_wiki`] keeps only them, so the same
/// `search_hybrid` call's fused ranking feeds two disjoint sections instead of billing the same
/// content twice against the token budget.
pub fn is_wiki_path(path: &str) -> bool {
    path.starts_with("docs/wiki/")
}

/// Section 4: hybrid retrieval results for the ticket's objective (code and non-wiki text; see
/// [`is_wiki_path`] and [`build_wiki`] for why `docs/wiki/` hits are excluded here).
///
/// Renders hybrid search results for the ticket's objective, seeded with claimed paths.
pub fn build_retrieval(
    ticket: &Ticket,
    ci: &CodeIntel,
    weights: SignalWeights,
) -> Result<RawSection> {
    let paths = claimed_paths(ticket);
    let query = Query {
        text: ticket.objective.clone(),
        seed_symbols: Vec::new(),
        seed_paths: paths.clone(),
    };

    let ctx = RetrievalContext {
        claimed_paths: paths,
        recently_edited: Vec::new(),
    };

    let hits = ci.search_hybrid(&query, &ctx, weights)?;

    let mut body_lines = Vec::new();
    let mut provenance = Vec::new();

    for hit in hits.into_iter().filter(|h| !is_wiki_path(&h.path)) {
        let line_range = match (hit.line_start, hit.line_end) {
            (Some(start), Some(end)) => format!("{}-{}", start, end),
            (Some(start), None) => start.to_string(),
            _ => "?".to_string(),
        };

        body_lines.push(format!("{}:{}: {}", hit.path, line_range, hit.snippet));

        let locator = match hit.line_start {
            Some(start) => format!("{}:{}", hit.path, start),
            None => hit.path.clone(),
        };

        provenance.push(ProvenanceRef {
            locator,
            detail: "hybrid retrieval hit".to_string(),
        });
    }

    let body = body_lines.join("\n");

    Ok(RawSection {
        kind: SectionKind::Retrieval,
        title: "Retrieval Results".to_string(),
        body,
        provenance,
    })
}

/// Section 5: wiki pages (`SPEC.md` §26) matching the objective, ranked alongside code in the
/// exact same hybrid retrieval fusion [`build_retrieval`] draws from — the same `search_hybrid`
/// call, filtered to [`is_wiki_path`] hits instead of everything else. So a worker's context pack
/// can cite a compiled wiki page as source material rather than only raw code/history
/// (`SPEC.md` §26.4).
pub fn build_wiki(ticket: &Ticket, ci: &CodeIntel, weights: SignalWeights) -> Result<RawSection> {
    let paths = claimed_paths(ticket);
    let query = Query {
        text: ticket.objective.clone(),
        seed_symbols: Vec::new(),
        seed_paths: paths.clone(),
    };

    let ctx = RetrievalContext {
        claimed_paths: paths,
        recently_edited: Vec::new(),
    };

    let hits = ci.search_hybrid(&query, &ctx, weights)?;

    let mut body_lines = Vec::new();
    let mut provenance = Vec::new();

    for hit in hits.into_iter().filter(|h| is_wiki_path(&h.path)) {
        body_lines.push(format!("{}: {}", hit.path, hit.snippet));
        provenance.push(ProvenanceRef {
            locator: hit.path.clone(),
            detail: "wiki page".to_string(),
        });
    }

    let body = body_lines.join("\n");

    Ok(RawSection {
        kind: SectionKind::Wiki,
        title: "Wiki".to_string(),
        body,
        provenance,
    })
}
/// Section 6: symbol outlines for the ticket's claimed paths.
///
/// Renders symbol outlines for each claimed path, indented by depth.
pub fn build_symbol_outlines(ticket: &Ticket, ci: &CodeIntel) -> Result<RawSection> {
    let paths = claimed_paths(ticket);

    let mut body_lines = Vec::new();
    let mut provenance = Vec::new();

    for path in paths {
        let outline = ci.outline(&path)?;

        if !outline.is_empty() {
            body_lines.push(format!("## {}", path));

            for entry in outline {
                let indent = " ".repeat((entry.depth as usize) * 2);
                body_lines.push(format!("{}{}", indent, entry.rendered));
            }

            body_lines.push(String::new());

            provenance.push(ProvenanceRef {
                locator: path,
                detail: String::new(),
            });
        }
    }

    let body = body_lines.join("\n").trim_end().to_string();

    Ok(RawSection {
        kind: SectionKind::SymbolOutlines,
        title: "Symbol Outlines".to_string(),
        body,
        provenance,
    })
}

/// Section 7: relevant git history.
///
/// Renders relevant git history for each claimed path.
pub fn build_git_history(ticket: &Ticket, ci: &CodeIntel) -> Result<RawSection> {
    let paths = claimed_paths(ticket);

    let mut body_lines = Vec::new();
    let mut provenance = Vec::new();

    const MAX_HISTORY_HITS_PER_PATH: usize = 5;

    for path in paths {
        let hits = ci.history_search(&path)?;

        let top_hits = hits.iter().take(MAX_HISTORY_HITS_PER_PATH);

        for hit in top_hits {
            let sha_short = if hit.commit.sha.len() >= 7 {
                &hit.commit.sha[..7]
            } else {
                &hit.commit.sha
            };

            let first_line = hit.commit.message.lines().next().unwrap_or("");

            body_lines.push(format!("{} {}: {}", sha_short, first_line, hit.snippet));

            provenance.push(ProvenanceRef {
                locator: hit.commit.sha.clone(),
                detail: String::new(),
            });
        }
    }

    let body = body_lines.join("\n");

    Ok(RawSection {
        kind: SectionKind::GitHistory,
        title: "Git History".to_string(),
        body,
        provenance,
    })
}

/// Section 8: prior failures recorded on this ticket.
///
/// Renders failures from the ticket's failure history.
pub fn build_prior_failures(ticket: &Ticket) -> RawSection {
    let mut body_lines = Vec::new();

    for failure in &ticket.failures {
        body_lines.push(format!(
            "attempt {} ({:?}): {}",
            failure.attempt, failure.class, failure.detail
        ));
    }

    let body = body_lines.join("\n");

    RawSection {
        kind: SectionKind::PriorFailures,
        title: "Prior Failures".to_string(),
        body,
        provenance: Vec::new(),
    }
}

/// Section 9 (lowest priority, dropped first on overflow): project conventions.
///
/// Renders `extra` (any caller-supplied conventions, e.g. from a future `providers.toml`-shaped
/// source) followed by two discovered sources, `docs/audit-2026-09-18-fable.md` M-04's
/// "Ecosystem table stakes":
///
/// - Every `AGENTS.md` found walking up from each of `ticket`'s claimed paths to `ci`'s project
///   root (inclusive), root-to-leaf, so a more specific `AGENTS.md` reads after — and can be
///   read as refining — a more general one closer to the repo root. A claimed path with no
///   `AGENTS.md` anywhere in its ancestry contributes nothing.
/// - Every discovered skill's metadata (name + one-line description only, never a body — see
///   [`crate::skills`]'s doc comment) from `.tm/skills/**`, so a worker knows what skills exist
///   and can call `skill.load` for the one it wants, without every skill's full body being paid
///   for on every turn regardless of whether it's used.
pub fn build_conventions(ticket: &Ticket, ci: &CodeIntel, extra: &[String]) -> RawSection {
    let root = ci.project_root();
    let mut body_lines: Vec<String> = extra.to_vec();
    let mut provenance = Vec::new();

    for (path, content) in discover_agents_md(root, &claimed_paths(ticket)) {
        body_lines.push(format!("## {}\n{}", path, content.trim_end()));
        provenance.push(ProvenanceRef {
            locator: path,
            detail: String::new(),
        });
    }

    let skills = crate::skills::discover_skills(root);
    if !skills.is_empty() {
        body_lines
            .push("## Skills (call skill.load with `name` to read the full body)".to_string());
        for skill in &skills {
            body_lines.push(format!("- {}: {}", skill.name, skill.description));
            provenance.push(ProvenanceRef {
                locator: skill.path.clone(),
                detail: String::new(),
            });
        }
    }

    let body = body_lines.join("\n");

    RawSection {
        kind: SectionKind::Conventions,
        title: "Conventions".to_string(),
        body,
        provenance,
    }
}

/// Every `AGENTS.md` found for `claimed` paths, walking up from each path's directory to `root`
/// (inclusive), deduplicated across paths that share an ancestor, in first-seen order. Pure
/// aside from the filesystem reads themselves.
/// Instruction files read from each directory, in this order: `AGENTS.md` (the cross-tool
/// convention) and `CLAUDE.md`, so a repository set up for Claude Code works in `tm` unchanged
/// (D-019).
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

fn discover_agents_md(root: &Path, claimed: &[String]) -> Vec<(String, String)> {
    // Nothing claimed (a chat turn with no ticket) still means the project root's instructions.
    let root_only = [String::new()];
    let patterns: &[String] = if claimed.is_empty() {
        &root_only
    } else {
        claimed
    };
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for pattern in patterns {
        for dir in ancestor_dirs(root, pattern) {
            for name in INSTRUCTION_FILES {
                let candidate = dir.join(name);
                let Ok(content) = std::fs::read_to_string(&candidate) else {
                    continue;
                };
                let rel = candidate
                    .strip_prefix(root)
                    .unwrap_or(&candidate)
                    .to_string_lossy()
                    .replace('\\', "/");
                if seen.insert(rel.clone()) {
                    found.push((rel, content));
                }
            }
        }
    }
    found
}

/// Directories to check for an `AGENTS.md` for one claimed path pattern, root-to-leaf: the
/// pattern's starting directory (see [`claimed_path_start_dir`]) walked up to `root` inclusive.
fn ancestor_dirs(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let start_rel = claimed_path_start_dir(root, pattern);
    let start = if start_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&start_rel)
    };
    let mut dirs: Vec<PathBuf> = start
        .ancestors()
        .take_while(|a| *a == root || a.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

/// The directory a claimed path pattern's `AGENTS.md` walk should start from, root-relative
/// (empty string means `root` itself).
///
/// Claimed paths are technically glob patterns (`PatternSet`), not always concrete paths, but in
/// practice (and per `docs/audit-2026-09-18-fable.md` M-04's own example) are usually a literal
/// file path. This takes the pattern's non-wildcard prefix and:
/// - if it names a real directory on disk (e.g. `crates/tm-foo`, no wildcard, no trailing
///   slash), starts there — the single most likely place a crate-scoped `AGENTS.md` lives;
/// - otherwise (a literal file path, or a wildcard prefix like `crates/tm-foo/**` or
///   `crates/tm-foo/src/*.rs`) starts at its parent directory.
fn claimed_path_start_dir(root: &Path, pattern: &str) -> String {
    let end = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    let prefix = &pattern[..end];
    if end == pattern.len() && root.join(prefix).is_dir() {
        return prefix.trim_end_matches('/').to_string();
    }
    match prefix.rfind('/') {
        Some(idx) => prefix[..idx].to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{Ticket, TicketKind, TicketState};
    use tm_types::{Authority, Budget, Predicate, TicketId};

    fn minimal_ticket(id: &str, objective: &str) -> Ticket {
        Ticket {
            id: TicketId::new(id).expect("valid ticket id"),
            kind: TicketKind::Work,
            objective: objective.to_string(),
            state: TicketState::Ready,
            parent: None,
            children: Vec::new(),
            dependencies: Vec::new(),
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: Vec::new(),
            executor: tm_core::ExecutorRequirements {
                role: tm_types::Role::CoderFast,
                human_required: false,
                min_capability: tm_types::Tolerance::Any,
            },
            context_refs: Vec::new(),
            success: Vec::new(),
            verification: tm_core::VerificationPolicy::None,
            budget: Budget::unlimited(),
            retry: tm_core::RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 120,
            },
            cycle: None,
            attempts: 0,
            failures: Vec::new(),
            priority: 0,
            created: tm_types::Timestamp::EPOCH,
            updated: tm_types::Timestamp::EPOCH,
        }
    }

    #[test]
    fn claimed_paths_empty_resources() {
        let ticket = minimal_ticket("T-1", "Test objective");
        assert_eq!(claimed_paths(&ticket), Vec::<String>::new());
    }

    #[test]
    fn claimed_paths_deduplicates_first_seen_order() {
        let mut ticket = minimal_ticket("T-1", "Test objective");
        let patterns1 =
            tm_types::PatternSet::parse(vec!["src/*.rs".to_string(), "src/lib.rs".to_string()])
                .expect("valid patterns");
        let patterns2 =
            tm_types::PatternSet::parse(vec!["src/*.rs".to_string()]).expect("valid patterns");

        ticket.resources = vec![
            tm_core::ResourceClaim {
                paths: patterns1,
                mode: tm_core::ResourceMode::Shared,
            },
            tm_core::ResourceClaim {
                paths: patterns2,
                mode: tm_core::ResourceMode::Shared,
            },
        ];

        let paths = claimed_paths(&ticket);
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().any(|p| p == "src/*.rs"));
        assert!(paths.iter().any(|p| p == "src/lib.rs"));
    }

    #[test]
    fn build_objective_renders_objective_only() {
        let ticket = minimal_ticket("T-1", "Fix the bug");
        let section = build_objective(&ticket);

        assert_eq!(section.kind, SectionKind::Objective);
        assert_eq!(section.title, "Objective");
        assert!(section.body.contains("Fix the bug"));
        assert_eq!(section.provenance.len(), 1);
        assert_eq!(section.provenance[0].locator, "T-1");
        assert_eq!(section.provenance[0].detail, "ticket objective/success");
    }

    #[test]
    fn build_objective_renders_predicates() {
        let mut ticket = minimal_ticket("T-1", "Implement feature");
        ticket.success = vec![
            Predicate::FileExists {
                path: "src/feature.rs".to_string(),
            },
            Predicate::TestsPass {
                suite: Some("unit".to_string()),
            },
        ];

        let section = build_objective(&ticket);
        assert!(section.body.contains("Implement feature"));
        assert!(section.body.contains("FileExists: src/feature.rs"));
        assert!(section.body.contains("TestsPass: unit"));
    }

    #[test]
    fn build_objective_renders_nested_predicates() {
        let mut ticket = minimal_ticket("T-1", "Complex task");
        ticket.success = vec![Predicate::AllOf(vec![
            Predicate::FileExists {
                path: "a.txt".to_string(),
            },
            Predicate::FileExists {
                path: "b.txt".to_string(),
            },
        ])];

        let section = build_objective(&ticket);
        assert!(section.body.contains("AllOf:"));
        assert!(section.body.contains("FileExists: a.txt"));
        assert!(section.body.contains("FileExists: b.txt"));
    }

    #[test]
    fn build_decisions_empty_view() {
        let ticket = minimal_ticket("T-1", "Task");
        let view = ProjectView::empty();

        let section = build_decisions(&ticket, &view);
        assert_eq!(section.kind, SectionKind::Decisions);
        assert_eq!(section.title, "Active Decisions");
        assert_eq!(section.body, "");
        assert_eq!(section.provenance.len(), 0);
    }

    #[test]
    fn build_prior_failures_empty() {
        let ticket = minimal_ticket("T-1", "Task");
        let section = build_prior_failures(&ticket);

        assert_eq!(section.kind, SectionKind::PriorFailures);
        assert_eq!(section.title, "Prior Failures");
        assert_eq!(section.body, "");
        assert_eq!(section.provenance.len(), 0);
    }

    #[test]
    fn build_prior_failures_with_records() {
        let mut ticket = minimal_ticket("T-1", "Task");
        ticket.failures = vec![
            tm_core::FailureRecord {
                class: tm_core::FailureClass::ExecutorCrash,
                detail: "Process crashed".to_string(),
                at: tm_types::Timestamp::EPOCH,
                attempt: 1,
            },
            tm_core::FailureRecord {
                class: tm_core::FailureClass::VerificationFailed,
                detail: "Test failed".to_string(),
                at: tm_types::Timestamp::EPOCH,
                attempt: 2,
            },
        ];

        let section = build_prior_failures(&ticket);
        assert!(section.body.contains("attempt 1"));
        assert!(section.body.contains("ExecutorCrash"));
        assert!(section.body.contains("Process crashed"));
        assert!(section.body.contains("attempt 2"));
        assert!(section.body.contains("VerificationFailed"));
        assert!(section.body.contains("Test failed"));
    }

    #[test]
    fn build_conventions_empty() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let ci = CodeIntel::open(dir.path()).expect("open index");
        let ticket = minimal_ticket("T-1", "Task");
        let conventions: Vec<String> = Vec::new();
        let section = build_conventions(&ticket, &ci, &conventions);

        assert_eq!(section.kind, SectionKind::Conventions);
        assert_eq!(section.title, "Conventions");
        assert_eq!(section.body, "");
        assert_eq!(section.provenance.len(), 0);
    }

    #[test]
    fn build_conventions_with_items() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let ci = CodeIntel::open(dir.path()).expect("open index");
        let ticket = minimal_ticket("T-1", "Task");
        let conventions = vec![
            "Use snake_case for variables".to_string(),
            "Always add doc comments".to_string(),
            "Write tests for public functions".to_string(),
        ];

        let section = build_conventions(&ticket, &ci, &conventions);
        assert!(section.body.contains("Use snake_case for variables"));
        assert!(section.body.contains("Always add doc comments"));
        assert!(section.body.contains("Write tests for public functions"));
    }

    /// `docs/audit-2026-09-18-fable.md` M-04: "AGENTS.md read as a SectionKind::Conventions
    /// source walking up from the ticket's claimed paths" — a real `AGENTS.md` a couple of
    /// directories above a ticket's claimed path must actually appear in the assembled section.
    #[test]
    fn build_conventions_includes_agents_md_walking_up_from_claimed_paths() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("crates/tm-foo/src")).expect("mkdir");
        std::fs::write(
            dir.path().join("crates/tm-foo/AGENTS.md"),
            "SENTINEL-AGENTS-MD-CONTENT: use tabs in this crate\n",
        )
        .expect("write AGENTS.md");

        let ci = CodeIntel::open(dir.path()).expect("open index");
        let mut ticket = minimal_ticket("T-1", "Fix bar.rs");
        ticket.resources = vec![tm_core::ResourceClaim {
            paths: tm_types::PatternSet::parse(["crates/tm-foo/src/bar.rs".to_string()])
                .expect("valid pattern"),
            mode: tm_core::ResourceMode::Shared,
        }];

        let section = build_conventions(&ticket, &ci, &[]);
        assert!(
            section.body.contains("SENTINEL-AGENTS-MD-CONTENT"),
            "expected AGENTS.md content in body, got: {}",
            section.body
        );
        assert!(section
            .provenance
            .iter()
            .any(|p| p.locator == "crates/tm-foo/AGENTS.md"));
    }

    #[test]
    fn build_conventions_reads_root_agents_and_claude_md_even_with_no_claimed_paths() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("AGENTS.md"), "ROOT-AGENTS: run cargo fmt\n")
            .expect("write AGENTS.md");
        std::fs::write(
            dir.path().join("CLAUDE.md"),
            "ROOT-CLAUDE: prefer small commits\n",
        )
        .expect("write CLAUDE.md");

        let ci = CodeIntel::open(dir.path()).expect("open index");
        // A chat turn with no ticket claims nothing at all.
        let ticket = minimal_ticket("T-0", "what does this repo do?");
        let section = build_conventions(&ticket, &ci, &[]);

        assert!(section.body.contains("ROOT-AGENTS"), "{}", section.body);
        assert!(
            section.body.contains("ROOT-CLAUDE"),
            "a Claude Code repository's CLAUDE.md applies too: {}",
            section.body
        );
        let agents = section.body.find("ROOT-AGENTS").expect("agents");
        let claude = section.body.find("ROOT-CLAUDE").expect("claude");
        assert!(agents < claude, "AGENTS.md first, then CLAUDE.md");
    }

    #[test]
    fn build_conventions_finds_no_agents_md_when_none_exists() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("crates/tm-foo/src")).expect("mkdir");

        let ci = CodeIntel::open(dir.path()).expect("open index");
        let mut ticket = minimal_ticket("T-1", "Fix bar.rs");
        ticket.resources = vec![tm_core::ResourceClaim {
            paths: tm_types::PatternSet::parse(["crates/tm-foo/src/bar.rs".to_string()])
                .expect("valid pattern"),
            mode: tm_core::ResourceMode::Shared,
        }];

        let section = build_conventions(&ticket, &ci, &[]);
        assert_eq!(section.body, "");
        assert_eq!(section.provenance.len(), 0);
    }

    /// `docs/audit-2026-09-18-fable.md` M-04: skill metadata (name/description) belongs in the
    /// pack; the body does not, until `skill.load` is called (tested at the `tm-agent` layer,
    /// which owns that tool).
    #[test]
    fn build_conventions_includes_skill_metadata_but_not_body() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".tm/skills/demo")).expect("mkdir");
        std::fs::write(
            dir.path().join(".tm/skills/demo/SKILL.md"),
            "---\nname: demo-skill\ndescription: does a demo thing\n---\nSENTINEL-SKILL-BODY-SHOULD-NOT-APPEAR\n",
        )
        .expect("write SKILL.md");

        let ci = CodeIntel::open(dir.path()).expect("open index");
        let ticket = minimal_ticket("T-1", "Task");

        let section = build_conventions(&ticket, &ci, &[]);
        assert!(section.body.contains("demo-skill"));
        assert!(section.body.contains("does a demo thing"));
        assert!(!section
            .body
            .contains("SENTINEL-SKILL-BODY-SHOULD-NOT-APPEAR"));
    }

    #[test]
    fn build_dependencies_empty() {
        let ticket = minimal_ticket("T-1", "Task");
        let view = ProjectView::empty();

        let section = build_dependencies(&ticket, &view);
        assert_eq!(section.kind, SectionKind::Dependencies);
        assert_eq!(section.title, "Dependencies & Evidence");
        assert_eq!(section.body, "");
    }

    #[test]
    fn render_predicate_command_succeeds() {
        let pred = Predicate::CommandSucceeds {
            command: vec!["make".to_string(), "test".to_string()],
        };
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("CommandSucceeds"));
        assert!(rendered.contains("make test"));
    }

    #[test]
    fn render_predicate_file_exists() {
        let pred = Predicate::FileExists {
            path: "Cargo.toml".to_string(),
        };
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("FileExists: Cargo.toml"));
    }

    #[test]
    fn render_predicate_file_matches() {
        let pred = Predicate::FileMatches {
            path: "src/main.rs".to_string(),
            regex: r#"fn main\(\)"#.to_string(),
        };
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("FileMatches"));
        assert!(rendered.contains("src/main.rs"));
    }

    #[test]
    fn render_predicate_tests_pass_with_suite() {
        let pred = Predicate::TestsPass {
            suite: Some("integration".to_string()),
        };
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("TestsPass: integration"));
    }

    #[test]
    fn render_predicate_tests_pass_default() {
        let pred = Predicate::TestsPass { suite: None };
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("TestsPass: default"));
    }

    #[test]
    fn render_predicate_ticket_closed() {
        let pred = Predicate::TicketClosed {
            ticket: TicketId::new("T-5").expect("valid"),
        };
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("TicketClosed: T-5"));
    }

    #[test]
    fn render_predicate_human_attested() {
        let pred = Predicate::HumanAttested {
            note: "Code review approved".to_string(),
        };
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("HumanAttested: Code review approved"));
    }

    #[test]
    fn render_predicate_judgment() {
        let pred = Predicate::Judgment {
            claim: "Design is sound".to_string(),
        };
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("Judgment: Design is sound"));
    }

    #[test]
    fn render_predicate_indentation() {
        let pred = Predicate::FileExists {
            path: "test.rs".to_string(),
        };
        let rendered = render_predicate(&pred, 4);
        assert!(rendered.starts_with("    FileExists"));
    }

    #[test]
    fn render_predicate_all_of() {
        let pred = Predicate::AllOf(vec![
            Predicate::FileExists {
                path: "a.rs".to_string(),
            },
            Predicate::FileExists {
                path: "b.rs".to_string(),
            },
        ]);
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("AllOf:"));
        assert!(rendered.contains("FileExists: a.rs"));
        assert!(rendered.contains("FileExists: b.rs"));
    }

    #[test]
    fn render_predicate_any_of() {
        let pred = Predicate::AnyOf(vec![
            Predicate::TestsPass {
                suite: Some("unit".to_string()),
            },
            Predicate::TestsPass {
                suite: Some("integration".to_string()),
            },
        ]);
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("AnyOf:"));
        assert!(rendered.contains("TestsPass: unit"));
        assert!(rendered.contains("TestsPass: integration"));
    }

    #[test]
    fn render_predicate_not() {
        let pred = Predicate::Not(Box::new(Predicate::FileExists {
            path: "forbidden.rs".to_string(),
        }));
        let rendered = render_predicate(&pred, 0);
        assert!(rendered.contains("Not:"));
        assert!(rendered.contains("FileExists: forbidden.rs"));
    }

    // ---- build_budget (SPEC.md §31.1, docs/audit-2026-09-18-fable.md B-10) ------------------

    #[test]
    fn build_budget_reports_unlimited_and_no_usage_yet() {
        let ticket = minimal_ticket("T-1", "Task"); // fixture uses Budget::unlimited()
        let section = build_budget(&ticket, &RoleTable::default_table());

        assert_eq!(section.kind, SectionKind::Budget);
        assert_eq!(section.title, "Budget");
        assert!(section.body.contains("unlimited tokens"));
        assert!(section.body.contains("$unlimited"));
        assert!(section.body.contains("unlimited wall-seconds"));
        assert!(section.body.contains("Burn rate: no usage recorded yet"));
    }

    #[test]
    fn build_budget_reports_remaining_spend_and_burn_rate() {
        let mut ticket = minimal_ticket("T-1", "Task");
        ticket.budget = Budget::new(1_000, 5_000_000, 3_600);
        ticket
            .budget
            .try_spend(tm_types::Spend {
                tokens: 400,
                dollars_micros: 0,
                wall_seconds: 0,
            })
            .expect("spend within the budget");
        ticket.attempts = 2;

        let section = build_budget(&ticket, &RoleTable::default_table());

        assert!(section.body.contains("Remaining: 600 tokens"));
        assert!(section
            .body
            .contains("Burn rate: ~200 tokens/attempt over 2 attempt(s)"));
        assert!(section
            .body
            .contains("Projection: roughly 3 more attempt(s)"));
    }

    #[test]
    fn build_budget_tier_menu_lists_an_unpriced_role_as_unmetered() {
        let ticket = minimal_ticket("T-1", "Task");
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("valid table");

        let section = build_budget(&ticket, &table);
        assert!(section
            .body
            .contains("coder.fast (mock/m1): subscription/unmetered capacity"));
    }

    #[test]
    fn build_budget_tier_menu_prices_a_metered_role() {
        let mut ticket = minimal_ticket("T-1", "Task");
        ticket.budget = Budget::new(u64::MAX, 10_000_000, u64::MAX);
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"anthropic\", model = \"claude\", max_concurrency = 1, price = { input_micros_per_token = 10, output_micros_per_token = 10 } }]\n",
        )
        .expect("valid table");

        let section = build_budget(&ticket, &table);
        assert!(section
            .body
            .contains("coder.fast (anthropic/claude): ~$0.0200/call"));
        assert!(section.body.contains("call(s) remain affordable"));
    }

    // ---- build_wiki (SPEC.md §26, docs/audit-2026-09-18-fable.md B-14) ----------------------

    #[test]
    fn is_wiki_path_matches_only_docs_wiki_prefix() {
        assert!(is_wiki_path("docs/wiki/architecture/tm-core.md"));
        assert!(!is_wiki_path("docs/backlog.md"));
        assert!(!is_wiki_path("crates/tm-core/src/lib.rs"));
    }

    /// Init a git repo with one empty commit, matching `tm-codeintel`'s own test convention
    /// (`CodeIntel::update_incremental` always runs a history ingest, which needs a valid HEAD).
    fn init_git_repo(path: &std::path::Path) {
        let repo = git2::Repository::init(path).expect("git init");
        let sig = git2::Signature::new("Test", "test@example.com", &git2::Time::new(0, 0))
            .expect("signature");
        let tree_id = repo
            .index()
            .expect("repo index")
            .write_tree()
            .expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .expect("initial commit");
    }

    #[test]
    fn build_wiki_and_build_retrieval_partition_the_same_hybrid_search() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        init_git_repo(dir.path());

        std::fs::create_dir_all(dir.path().join("docs/wiki")).unwrap();
        std::fs::write(
            dir.path().join("docs/wiki/glossary.md"),
            "# Glossary\n\nwidget: a small reusable component.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("widget.rs"),
            "// widget implementation\nfn widget() {}\n",
        )
        .unwrap();

        let ci = CodeIntel::open(dir.path()).expect("open index");
        ci.update_incremental(&tm_types::FixedClock::epoch())
            .expect("index the fixture files");

        let ticket = minimal_ticket("T-1", "widget");
        let weights = SignalWeights::default();

        let retrieval = build_retrieval(&ticket, &ci, weights).expect("build_retrieval");
        let wiki = build_wiki(&ticket, &ci, weights).expect("build_wiki");

        // Every hit `build_wiki` admits is a wiki page; every hit `build_retrieval` admits is
        // not — the same `search_hybrid` call, disjointly partitioned, so no content is billed
        // against the token budget twice.
        for prov in &wiki.provenance {
            assert!(
                is_wiki_path(&prov.locator),
                "{} should be a wiki path",
                prov.locator
            );
        }
        for prov in &retrieval.provenance {
            assert!(
                !is_wiki_path(&prov.locator),
                "{} should not be a wiki path",
                prov.locator
            );
        }
        assert_eq!(wiki.kind, SectionKind::Wiki);
    }
}
