# Product Invariants

This document maps the twelve invariants stated in SPEC.md §17 to test functions that prove them.

| # | Invariant | Crates | Test | Proof |
|---|-----------|--------|------|-------|
| 1 | The project—not any agent or session—is the persistent entity. | tm-types, tm-core | `invariant_1_project_is_persistent_entity` | Verifies a project survives multiple sessions and agents. |
| 2 | All durable truth is derivable from the event log by replay. | tm-types, tm-core | `invariant_2_replay_equivalence` | Confirms replay produces identical state to incremental updates on a ≥200-event fixture. |
| 3 | No model decides a transition that software can decide. | tm-core | `invariant_3_software_decided_transitions` | Asserts all automatic state transitions are verified by code. |
| 4 | Authority is explicit, scoped, leased, attenuating, revocable, auditable; a child can never exceed its parent. | tm-types, tm-core | `invariant_4_authority_hierarchy_enforced` | Validates scope, lease duration, attenuation, and child-parent constraints. |
| 5 | A dead worker cannot block the project: leases expire and authority reverts. | tm-scheduler, tm-core | `invariant_5_lease_expiry_unblocks_workers` | Simulates worker crash and confirms leases expire to release blocked resources. |
| 6 | Workers submit evidence; a different executor verifies; a different one audits. | tm-core | `invariant_6_evidence_verify_audit_separation` | Checks that submission, verification, and audit are performed by distinct logical actors. |
| 7 | Expensive commands run once; their full output is durable and queryable. | tm-codeintel, tm-core | `invariant_7_expensive_commands_idempotent` | Verifies re-running expensive operations yields cached results without re-execution. |
| 8 | Documentation knows when the facts beneath it changed; human prose is never overwritten. | tm-docs, tm-mirror | `invariant_8_docs_versioning_preserved` | Asserts documentation timestamps track changes and manual edits persist across updates. |
| 9 | External trackers are mirrors, never the orchestration database. | tm-mirror | `invariant_9_mirror_consistency_with_source` | Validates external tracker state remains consistent with the event log. |
| 10 | Provider exhaustion is routable state, not an exception. | tm-provider, tm-core | `invariant_10_provider_exhaustion_routable` | Confirms rate limits and quota exhaustion are state transitions, not exceptions. |
| 11 | Harness changes are benchmarked, promoted as epochs, and never mutate a live session. | tm-harness, tm-core | `invariant_11_harness_epochs_immutable` | Ensures harness changes are versioned as epochs and existing sessions continue unchanged. |
| 12 | Sessions are views; a fresh worker can always continue from durable state alone. | tm-context, tm-core | `invariant_12_session_recovery_from_state` | Verifies a new worker can reconstruct full session state from the event log. |

## How to Add an Invariant

1. **State it in SPEC.md §17**: Add a numbered clause describing the guarantee in one sentence.
2. **Name a test here**: Choose a test function name following `invariant_<n>_<snake_case_name>` and add a row to this table with the crate(s) responsible for enforcement and a one-line proof description.
3. **Write that test**: In the appropriate crate's test module, implement the test function to verify the invariant holds under representative conditions.
