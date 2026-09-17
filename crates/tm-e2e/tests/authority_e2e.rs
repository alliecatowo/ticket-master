//! `SPEC.md` §16 / §17 invariant 4: authority is explicit, scoped, leased, attenuating and
//! revocable; a child can never exceed its parent; and a denial reaches the executor as an
//! ordinary tool result, never a crash.

mod common;

use tempfile::TempDir;
use tm_types::{
    Action, Authority, Decision, GitOp, Oversight, PatternSet, Result as TmResult, TicketOp,
    TmError,
};

/// A four-link delegation chain (`root -> planner -> coder -> reviewer`), each link requesting
/// only what it needs and never more than its grantor holds. `Authority::attenuate` is the only
/// legal way to mint a child's authority; every step here goes through it.
#[test]
fn a_delegation_chain_never_lets_a_child_exceed_its_parent() {
    let root = Authority::root();

    let planner_request = Authority {
        repository: tm_types::RepoAuthority {
            read: PatternSet::all(),
            write: PatternSet::parse(["src/**"]).unwrap(),
        },
        tickets: tm_types::TicketAuthority {
            create_children: true,
            delegate_children: true,
            ..Default::default()
        },
        git: tm_types::GitAuthority {
            commit: true,
            branch: true,
            ..Default::default()
        },
        ..Authority::none()
    };
    let planner = root
        .attenuate(&planner_request)
        .expect("root may grant anything it holds");
    assert!(root.contains(&planner));

    let coder_request = Authority {
        repository: tm_types::RepoAuthority {
            read: PatternSet::all(),
            write: PatternSet::parse(["src/lib.rs"]).unwrap(),
        },
        git: tm_types::GitAuthority {
            commit: true,
            ..Default::default()
        },
        ..Authority::none()
    };
    let coder = planner
        .attenuate(&coder_request)
        .expect("coder's ask is within the planner's grant");
    assert!(planner.contains(&coder));
    assert!(
        root.contains(&coder),
        "containment is transitive down the whole chain"
    );

    let reviewer_request = Authority {
        repository: tm_types::RepoAuthority {
            read: PatternSet::parse(["src/lib.rs"]).unwrap(),
            write: PatternSet::empty(),
        },
        ..Authority::none()
    };
    let reviewer = coder
        .attenuate(&reviewer_request)
        .expect("a read-only ask is within anything that can write the same path");
    assert!(coder.contains(&reviewer));
    assert!(
        !reviewer.git.commit,
        "the reviewer never asked for commit and so never has it"
    );

    // The chain only ever narrows: nobody downstream can commit outside `src/`, delegate
    // further, or branch, none of which `coder`/`reviewer` were ever granted.
    assert!(!reviewer.tickets.delegate_children);
    assert!(!reviewer.git.branch);
}

/// The pure algebra: a child requesting something its grantor never held is refused as a typed
/// `Err`, not silently widened and not a panic.
#[test]
fn attenuation_refuses_to_exceed_the_grantor_rather_than_widen_or_panic() {
    let narrow = Authority {
        repository: tm_types::RepoAuthority {
            read: PatternSet::parse(["docs/**"]).unwrap(),
            write: PatternSet::empty(),
        },
        ..Authority::none()
    };
    let overreaching_request = Authority {
        git: tm_types::GitAuthority {
            push: true,
            force: true,
            ..Default::default()
        },
        ..Authority::none()
    };

    let err = narrow
        .attenuate(&overreaching_request)
        .expect_err("a grantor with no git authority at all cannot grant force-push");
    assert!(
        err.reasons.iter().any(|r| r.contains("git.force")),
        "the denial must name what exceeded the grant: {:?}",
        err.reasons
    );

    // The lossy variant never errors: it silently narrows to the intersection instead, which is
    // the API `tm-core`'s `GraphCompilation::effective_authority` actually uses.
    let granted = narrow.attenuate_lossy(&overreaching_request);
    assert!(narrow.contains(&granted));
    assert!(!granted.git.push);
}

/// `Authority::contains`/`containment_failures` treat shell denials specially: a grantor's
/// denials are *inherited*, not merely checked, so a child cannot silently drop a restriction
/// its parent imposed.
#[test]
fn a_child_cannot_drop_a_denial_its_parent_imposed() {
    let mut parent = Authority::none();
    parent.shell.enabled = true;
    parent.shell.allow = PatternSet::parse(["cargo *"]).unwrap();
    parent.shell.deny = PatternSet::parse(["cargo publish*"]).unwrap();

    let mut requested = Authority::none();
    requested.shell.enabled = true;
    requested.shell.allow = PatternSet::parse(["cargo *"]).unwrap();
    // Deliberately silent about `cargo publish`, not re-permitting it.

    let granted = parent
        .attenuate(&requested)
        .expect("plain request is within the parent's grant");
    assert!(
        granted.shell.deny.matches_text("cargo publish --dry-run"),
        "the parent's denial must be inherited even though the child never restated it"
    );
    assert!(!granted.shell.permits(&["cargo".into(), "publish".into()]));
    assert!(granted.shell.permits(&["cargo".into(), "build".into()]));
}

/// A local stand-in for the boundary between authority and an executing worker: exactly what
/// `tm-agent`'s tool dispatch does with `Authority::permits`'s verdict, kept here so this test
/// doesn't have to depend on a crate outside this suite's remit.
#[allow(dead_code)] // the `Ran`/`NeedsApproval` payloads document the full 3-way shape a real
                    // tool dispatch matches on; this test only asserts the `Denied` arm.
enum ToolOutcome {
    Ran(&'static str),
    Denied(String),
    NeedsApproval(String),
}

/// The actual denial-reaches-the-executor test: an authority that plainly does not permit an
/// action must produce a normal, matchable value the calling code can branch on — never a
/// panic, never an `unwrap` — exactly the way a tool call's result would reach an executor.
#[test]
fn a_denial_reaches_the_executor_as_a_tool_result_not_a_crash() {
    let restricted = Authority::read_only();
    let write_attempt = Action::WritePath {
        path: "src/lib.rs".to_string(),
    };

    let decision = restricted.permits(&write_attempt);
    let outcome = match decision {
        Decision::Allow => ToolOutcome::Ran("wrote src/lib.rs"),
        Decision::Deny(reason) => ToolOutcome::Denied(reason),
        Decision::NeedsApproval(scope) => ToolOutcome::NeedsApproval(scope),
    };

    match outcome {
        ToolOutcome::Denied(reason) => assert!(reason.contains("src/lib.rs")),
        _ => panic!("a read-only authority must deny a write, not run it or crash"),
    }

    // Oversight can escalate an allowed action to a human, but never soften a denial: the same
    // denial a worker sees is the same denial a human review would have seen.
    let oversight = Oversight::conservative();
    let reviewed = oversight.review(&write_attempt, restricted.permits(&write_attempt));
    assert!(matches!(reviewed, Decision::Deny(_)));

    // A ticket-graph action is denied the same structural way, not just repository actions.
    let cancel_attempt = Action::Ticket {
        op: TicketOp::Cancel,
    };
    assert!(matches!(
        restricted.permits(&cancel_attempt),
        Decision::Deny(_)
    ));
    let push_attempt = Action::Git { op: GitOp::Push };
    assert!(matches!(
        restricted.permits(&push_attempt),
        Decision::Deny(_)
    ));
}

/// End to end through `tm-core::Store`: a denial at the ticket-graph boundary (child authority
/// exceeding its parent's, or an action gated on authority the ticket lacks) surfaces as an
/// ordinary `Err`, the caller's normal control flow, never a panic — `Store`'s own
/// `#![forbid(unsafe_code)]`/no-panics contract depends on exactly this.
#[test]
fn store_level_denials_are_ordinary_results() -> TmResult<()> {
    let dir = TempDir::new().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());

    let root_id = common::ready_ticket(&store, "root", Authority::root());
    let narrow = Authority {
        repository: tm_types::RepoAuthority {
            read: PatternSet::all(),
            write: PatternSet::parse(["src/**"]).unwrap(),
        },
        ..Authority::none()
    };
    let child_events = store.create_ticket(
        tm_core::TicketKind::Work,
        "child".to_string(),
        Some(root_id.clone()),
        None,
        narrow.clone(),
        vec![],
        common::executor_reqs(),
        vec![],
        vec![],
        tm_core::VerificationPolicy::None,
        tm_types::Budget::unlimited(),
        common::retry_policy(),
        0,
        common::system(),
    )?;
    let child_id = tm_types::TicketId::new(child_events[0].subject.as_str())?;

    // A grandchild asking for more than its parent (`child`, not `root`) held is refused, even
    // though `root` itself would have permitted it.
    let overreaching = Authority::root();
    let err = store
        .create_ticket(
            tm_core::TicketKind::Work,
            "grandchild".to_string(),
            Some(child_id.clone()),
            None,
            overreaching,
            vec![],
            common::executor_reqs(),
            vec![],
            vec![],
            tm_core::VerificationPolicy::None,
            tm_types::Budget::unlimited(),
            common::retry_policy(),
            0,
            common::system(),
        )
        .unwrap_err();
    assert!(
        matches!(err, TmError::Invariant(_)),
        "denial must be a typed Err: {err}"
    );

    // A legitimately narrower grandchild is fine.
    let fine = Authority {
        repository: tm_types::RepoAuthority {
            read: PatternSet::all(),
            write: PatternSet::parse(["src/lib.rs"]).unwrap(),
        },
        ..Authority::none()
    };
    store.create_ticket(
        tm_core::TicketKind::Work,
        "grandchild-ok".to_string(),
        Some(child_id.clone()),
        None,
        fine,
        vec![],
        common::executor_reqs(),
        vec![],
        vec![],
        tm_core::VerificationPolicy::None,
        tm_types::Budget::unlimited(),
        common::retry_policy(),
        0,
        common::system(),
    )?;

    // An authority-gated command (`cancel`, gated on `tickets.cancel`) on a ticket that never
    // held that power is refused the same way — `Err`, not a crash.
    let no_cancel_authority = store.cancel(&child_id, None, common::system()).unwrap_err();
    assert!(matches!(no_cancel_authority, TmError::AuthorityDenied(_)));

    // A lease requesting authority the ticket doesn't hold is refused too.
    let lease_err = store
        .acquire_lease(
            &child_id,
            common::agent("worker"),
            Authority::root(),
            vec![],
            60,
            common::agent("worker"),
        )
        .unwrap_err();
    assert!(matches!(lease_err, TmError::Conflict(_)));

    Ok(())
}
