//! The durable goal loop (`SPEC.md` §29, `docs/audit-2026-09-18-fable.md` B-09).
//!
//! A ticket is the unit of durable *project* work; a goal is the unit of durable *intent* inside
//! one execution of that work — the worker's own live decomposition of how it is getting there.
//! Unlike a competitor's todo list (scratch state that lives in a transcript and dies with it),
//! goal state is a projection of [`tm_events::EventKind::GoalSet`]/`GoalStepAdded`/
//! `GoalStepCompleted`/`GoalReoriented`/`GoalClaimedComplete` events: it survives process death,
//! compaction and handoff, and it replays exactly like every other materialized table
//! ([`crate::materialize::apply`]'s `goal.*` arms are the only writer).
//!
//! [`crate::store::Store::goal_state`] is the read path a loop uses for re-orientation — re-reading
//! this against observed state at the start of every step, rather than trusting its own
//! in-memory conversation history, is the actual mechanism `SPEC.md` §29 says keeps a long run on
//! target. [`GoalState::claimed_complete`] is a claim only: `SPEC.md` §16's verification ladder
//! (not this module, not the loop that set it) decides whether the claim held.

use serde::{Deserialize, Serialize};
use tm_types::{TicketId, Timestamp};

/// One step in a goal's live decomposition, as materialized in the `goals` table's `steps` JSON
/// column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalStep {
    /// The caller-chosen id this step was added under (`goal.step_added`'s `step_id`), stable
    /// across a later `goal.step_completed` for the same step.
    pub id: String,
    /// The step's own description.
    pub text: String,
    /// Whether `goal.step_completed` has been recorded for this step.
    pub done: bool,
}

/// A materialized `goals` row: one ticket's current durable goal, as read back by
/// [`crate::store::Store::goal_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalState {
    /// The ticket this goal belongs to.
    pub ticket: TicketId,
    /// The goal's own text, as last set by `goal.set`.
    pub text: String,
    /// The live decomposition, in the order steps were added.
    pub steps: Vec<GoalStep>,
    /// True once `goal.claimed_complete` has been recorded for this goal. A claim only — see this
    /// module's doc comment.
    pub claimed_complete: bool,
    /// The step index (`goal.reoriented`'s `at_step`) the loop most recently re-oriented at, or 0
    /// if it never has. A plain last-write-wins field (not a counter) so re-applying the same
    /// `goal.reoriented` event during replay is idempotent by construction.
    pub last_reoriented_step: u32,
    /// When `goal.set` most recently (re-)established this goal.
    pub set_at: Timestamp,
    /// When any `goal.*` event for this ticket was most recently applied.
    pub updated_at: Timestamp,
}
