//! Committing an already-[`tm_genesis::compile::validate_graph`]-clean [`GraphCompilation`] to a
//! [`Store`] as one [`Store::transaction`].
//!
//! `tm-genesis`'s own `commit_graph` (`crates/tm-genesis/src/compile.rs`) commits ticket
//! creation, milestone creation and dependency edges as separate `Store` command calls with a
//! best-effort compensating rollback on failure -- documented there as a gap left open only
//! because `Store` had no shared-transaction primitive to call instead (that gap is a separate,
//! not-yet-done audit item, M-08). `Store::transaction` (audit B-05) closes exactly that gap, so
//! this crate's own commit path uses it from the start: every draft this module builds is
//! appended through one [`tm_core::StoreTx`], and [`Store::transaction`]'s own post-write
//! `check_invariants` pass rejects and rolls back the *entire* attempt if anything is wrong,
//! rather than leaving some tickets created and others not.
//!
//! # Minting ticket ids without a `Store` accessor
//!
//! `Store` exposes no accessor for the [`tm_types::IdSource`] it mints ids from (deliberately --
//! see its own module doc on why external mutation goes through typed command methods, never raw
//! SQL). `tm-cli::project::open` already works around the identical gap for `tm-scheduler`/
//! `tm-server`, which also need to mint ids outside a `Store` command: it restores a fresh
//! [`tm_types::CounterIds`] from `Store::view()`'s persisted high-water marks and hands that
//! `Arc<dyn IdSource>` to its collaborators. [`commit`] follows the same pattern: it takes
//! `ids: &dyn IdSource` as a caller-supplied parameter rather than trying to reach into `Store`,
//! so `tm-cli`'s `tm workflow run` can pass the exact same `Project::ids` every other id-minting
//! collaborator in that process already uses.

use std::collections::BTreeMap;

use tm_core::ticket::{ContextRef, TicketKind};
use tm_core::{Store, TicketState};
use tm_events::payload::{
    TicketCreatedPayload, TicketDependencyAddedPayload, TicketUpdatedPayload,
};
use tm_events::{EventDraft, Payload};
use tm_genesis::compile::{GraphCompilation, Ref};
use tm_types::{Id, IdKind, IdSource, ParticipantId, Result as TmResult, TicketId, TmError};

use crate::def::WorkflowDef;
use crate::expand::node_id_of_ticket_ref;

/// What committing a [`GraphCompilation`] `crate::expand::expand` produced actually created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    /// Real ticket ids, keyed by the [`Ref`] `expand` proposed them under.
    pub tickets: BTreeMap<Ref, TicketId>,
    /// The blake3 hex digest `commit` computed over `source` and registered the definition under.
    pub content_hash: String,
    /// The version [`Store::register_workflow_def`] assigned this `content_hash` under `def.name`.
    pub version: u32,
}

/// The blake3 hex digest [`commit`] keys a workflow definition's registered version under and
/// stamps into every expanded ticket's pin (`workflow:<name>@<content_hash>`). Exposed so a
/// caller comparing an on-disk `.toml` against `Store::workflow_defs`' registered rows (e.g.
/// `tm workflow show`'s drift note) hashes it the exact same way `commit` did, rather than
/// duplicating the `blake3::hash(...).to_hex().to_string()` incantation at each call site.
pub fn content_hash(source: &str) -> String {
    blake3::hash(source.as_bytes()).to_hex().to_string()
}

/// Commit `proposal` (already produced by [`crate::expand::expand`] and already validated by
/// [`tm_genesis::compile::validate_graph`] -- this function does not re-validate) to `store`.
///
/// Two `Store` calls, not one: [`Store::register_workflow_def`] first (its own transaction --
/// `Store::transaction`'s closure may not call back into another `Store` method, so it cannot be
/// nested inside the ticket-commit transaction below), then the ticket/dependency commit as a
/// single [`Store::transaction`]. Registering the definition version before the tickets that pin
/// to it exist is not itself a partial-graph hazard: `workflows` is an idempotent lookup cache
/// (see `crate::schema`'s -- i.e. `tm_core::schema` -- module doc), not ticket-graph state, so a
/// registered-but-then-rolled-back-graph leaves nothing incoherent behind.
///
/// Every ticket [`crate::expand::expand`] proposed is created with `kind = Work`, and stamped
/// (in addition to whatever `context_refs` the proposal itself carries, which is none today --
/// see `crate::expand`) with one more [`ContextRef`]: `workflow:<name>@<content_hash>`. That
/// pointer is what makes a running instance stay pinned to the exact definition version it was
/// expanded from even if `.toml` on disk changes later (`SPEC.md` §25.2) -- it rides through
/// `ticket.updated`'s `fields` JSON into the `tickets` table and survives `Store::rebuild`
/// replay, unlike the `workflows` table itself (again, see `tm_core::schema`'s module doc).
///
/// # Errors
/// `TmError::invariant` if `proposal` names milestones/authority domains (`expand` never
/// produces either; a caller building a `GraphCompilation` by hand and passing it here is out of
/// this function's supported shape), a dangling `Ref`, or if the committed state violates a
/// `tm-core` invariant (surfaced by `Store::transaction`, which also rolls back in that case).
pub fn commit(
    store: &Store,
    ids: &dyn IdSource,
    actor: ParticipantId,
    def: &WorkflowDef,
    source: &str,
    proposal: &GraphCompilation,
) -> TmResult<CommitOutcome> {
    if !proposal.milestones.is_empty() || !proposal.authority_domains.is_empty() {
        return Err(TmError::invariant(
            "tm-workflow::commit only supports the tickets/dependencies shape crate::expand::expand produces \
             (no milestones, no authority_domains)",
        ));
    }

    let content_hash = content_hash(source);
    let version =
        store.register_workflow_def(def.name.clone(), content_hash.clone(), source.to_string())?;
    let pin = format!("workflow:{}@{content_hash}", def.name);

    let tickets = store.transaction(|tx| {
        let mut ticket_ids: BTreeMap<Ref, TicketId> = BTreeMap::new();
        let mut drafts: Vec<EventDraft> = Vec::new();

        for proposed in &proposal.tickets {
            let node_id = node_id_of_ticket_ref(&proposed.ticket_ref);
            let node = def.nodes.iter().find(|n| n.id == node_id).ok_or_else(|| {
                TmError::invariant(format!(
                    "ticket ref {:?} names no node in workflow {:?}",
                    proposed.ticket_ref, def.name
                ))
            })?;

            let id = TicketId::new(ids.next(IdKind::Ticket).as_str())?;

            drafts.push(EventDraft::new(
                actor.clone(),
                Id::from(id.clone()),
                Payload::from(TicketCreatedPayload {
                    ticket: id.clone(),
                    title: proposed.objective.clone(),
                    parent: None,
                }),
            ));

            let mut context_refs = proposed.context_refs.clone();
            context_refs.push(ContextRef {
                locator: pin.clone(),
                reason: format!(
                    "pinned to workflow {:?} version {version} ({content_hash})",
                    def.name
                ),
            });

            let mut fields = serde_json::json!({
                "kind": TicketKind::Work,
                "authority": proposed.authority,
                "resources": proposed.resources,
                "executor": proposed.executor,
                "context_refs": context_refs,
                "success": proposed.success,
                "verification": proposed.verification,
                "budget": proposed.budget,
                "retry": proposed.retry,
                "priority": proposed.priority,
            });
            if let Some(cycle) = node.cycle {
                fields["cycle"] = serde_json::to_value(cycle).map_err(TmError::from)?;
            }
            drafts.push(EventDraft::new(
                actor.clone(),
                Id::from(id.clone()),
                Payload::from(TicketUpdatedPayload {
                    ticket: id.clone(),
                    fields,
                }),
            ));

            ticket_ids.insert(proposed.ticket_ref.clone(), id);
        }

        for dep in &proposal.dependencies {
            let from = ticket_ids.get(&dep.from_ref).cloned().ok_or_else(|| {
                TmError::invariant(format!("dependency names unknown ref {:?}", dep.from_ref))
            })?;
            let to = ticket_ids.get(&dep.to_ref).cloned().ok_or_else(|| {
                TmError::invariant(format!("dependency names unknown ref {:?}", dep.to_ref))
            })?;
            drafts.push(EventDraft::new(
                actor.clone(),
                Id::from(from.clone()),
                Payload::from(TicketDependencyAddedPayload {
                    ticket: from,
                    depends_on: to,
                }),
            ));
        }

        tx.append_all(drafts)?;
        Ok(ticket_ids)
    })?;

    Ok(CommitOutcome {
        tickets,
        content_hash,
        version,
    })
}

/// True when `ticket` (looked up in `store`'s current view) is in a terminal, non-`Cancelled`
/// state and therefore has whatever evidence it is going to produce -- the precondition a caller
/// driving `crate::expand::expand_fan_out` off a `FromOutput` coordinator's result should check
/// before reading that evidence. Exposed here (rather than in `crate::expand`, which stays
/// store-free) since it is the one place this crate looks at live project state at all.
pub fn ticket_has_settled(store: &Store, ticket: &TicketId) -> TmResult<bool> {
    let view = store.view()?;
    Ok(view
        .tickets
        .get(ticket)
        .map(|t| matches!(t.state, TicketState::Closed | TicketState::Cancelled))
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;
    use tm_core::view::ProjectView;
    use tm_types::{CounterIds, FixedClock, IdSource, ParticipantId};

    use super::*;
    use crate::def::WorkflowDef;
    use crate::expand::expand;

    fn open_store() -> (TempDir, Store) {
        let dir = TempDir::new().expect("tempdir");
        let clock: Arc<dyn tm_types::Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, store)
    }

    fn two_step_def() -> WorkflowDef {
        WorkflowDef::parse(
            r#"
name = "lint-then-test"

[[node]]
id = "lint"
role = "coder_fast"
objective = "Lint {{target}}."
budget = { tokens = 100, dollars_micros = 0, wall_seconds = 60 }
verification = "none"

[[node]]
id = "test"
role = "coder_fast"
objective = "Test {{target}}."
depends = ["lint"]
budget = { tokens = 100, dollars_micros = 0, wall_seconds = 60 }
verification = "single"

[params.target]
default = "src/lib.rs"
"#,
        )
        .expect("valid definition")
    }

    #[test]
    fn commits_a_validated_expansion_and_pins_every_ticket() {
        let (_dir, store) = open_store();
        let def = two_step_def();
        let params = BTreeMap::new();
        let view = store.view().expect("view");
        let proposal = expand(&def, &params, &view).expect("expand succeeds");
        let violations = tm_genesis::compile::validate_graph(&proposal, &view);
        assert!(
            violations.is_empty(),
            "unexpected violations: {violations:?}"
        );

        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let outcome = commit(
            &store,
            ids.as_ref(),
            ParticipantId::system(),
            &def,
            "name = \"lint-then-test\"\n# fixture source",
            &proposal,
        )
        .expect("commit succeeds");

        assert_eq!(outcome.tickets.len(), 2);
        assert_eq!(outcome.version, 1);

        let post = store.view().expect("post-commit view");
        for ticket_id in outcome.tickets.values() {
            let ticket = post.tickets.get(ticket_id).expect("ticket materialized");
            assert!(ticket
                .context_refs
                .iter()
                .any(|c| c.locator == format!("workflow:lint-then-test@{}", outcome.content_hash)));
        }

        let test_id = &outcome.tickets["test"];
        let lint_id = &outcome.tickets["lint"];
        let test_ticket = post.tickets.get(test_id).expect("test ticket");
        assert_eq!(test_ticket.dependencies, vec![lint_id.clone()]);
    }

    #[test]
    fn registering_the_same_source_twice_is_idempotent_and_keeps_the_same_version() {
        let (_dir, store) = open_store();
        let source = "name = \"x\"\n";
        let v1 = store
            .register_workflow_def("x".to_string(), "hash-a".to_string(), source.to_string())
            .expect("register once");
        let v2 = store
            .register_workflow_def("x".to_string(), "hash-a".to_string(), source.to_string())
            .expect("register again");
        assert_eq!(v1, v2);

        let v3 = store
            .register_workflow_def("x".to_string(), "hash-b".to_string(), source.to_string())
            .expect("register a second content hash under the same name");
        assert_eq!(v3, v1 + 1);
    }

    #[test]
    fn two_workflow_commits_on_the_same_store_never_collide_ticket_ids() {
        let (_dir, store) = open_store();
        let def = two_step_def();
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());

        let view1 = store.view().expect("view");
        let proposal1 = expand(&def, &BTreeMap::new(), &view1).expect("expand 1");
        let outcome1 = commit(
            &store,
            ids.as_ref(),
            ParticipantId::system(),
            &def,
            "source-a",
            &proposal1,
        )
        .expect("commit 1");

        let view2 = store.view().expect("view");
        let proposal2 = expand(&def, &BTreeMap::new(), &view2).expect("expand 2");
        let outcome2 = commit(
            &store,
            ids.as_ref(),
            ParticipantId::system(),
            &def,
            "source-a",
            &proposal2,
        )
        .expect("commit 2");

        let ids1: std::collections::BTreeSet<_> = outcome1.tickets.values().cloned().collect();
        let ids2: std::collections::BTreeSet<_> = outcome2.tickets.values().cloned().collect();
        assert!(
            ids1.is_disjoint(&ids2),
            "expected disjoint ticket ids across two commits, got {ids1:?} and {ids2:?}"
        );
        // Same source registered twice: same content hash, same version.
        assert_eq!(outcome1.content_hash, outcome2.content_hash);
        assert_eq!(outcome1.version, outcome2.version);
    }

    #[test]
    fn commit_rejects_a_proposal_with_milestones() {
        let (_dir, store) = open_store();
        let def = two_step_def();
        let mut proposal = expand(&def, &BTreeMap::new(), &ProjectView::empty()).expect("expand");
        proposal
            .milestones
            .push(tm_genesis::compile::ProposedMilestone {
                milestone_ref: "m".to_string(),
                title: "unsupported".to_string(),
                ticket_refs: vec![],
                releases: vec![],
            });
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let err = commit(
            &store,
            ids.as_ref(),
            ParticipantId::system(),
            &def,
            "source",
            &proposal,
        )
        .unwrap_err();
        assert!(err.to_string().contains("only supports"));
    }
}
