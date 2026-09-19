//! Property-based tests proving the laws the authority algebra is supposed to satisfy
//! (`SPEC.md` §4.4, §4.7): containment is a preorder with `root` and `none` as top and bottom,
//! attenuation and intersection never widen, and pattern subset checks are a safe approximation
//! of real path matching.

use proptest::prelude::*;
use std::collections::BTreeSet;
use tm_types::authority::{
    Authority, ComputerAuthority, GitAuthority, NetworkAuthority, ProjectAuthority, RepoAuthority,
    ResourceAuthority, ShellAuthority, TicketAuthority,
};
use tm_types::budget::{Budget, Spend};
use tm_types::pattern::PatternSet;

/// A small, realistic alphabet of path segments. Deliberately narrow so that generated
/// patterns and paths collide often instead of talking past each other.
const SEGMENTS: &[&str] = &[
    "src", "tests", "docs", "auth", "parser", "mod.rs", "*", "**",
];

/// Build a strategy for one path-pattern source string of 1..=4 segments drawn from
/// [`SEGMENTS`], joined with `/`.
fn pattern_source() -> impl Strategy<Value = String> {
    proptest::collection::vec(proptest::sample::select(SEGMENTS), 1..=4)
        .prop_map(|segs| segs.join("/"))
}

/// A strategy for a [`PatternSet`] built from 0..=4 patterns drawn from [`pattern_source`].
///
/// Invalid sources (there should be none, given the alphabet) are filtered out rather than
/// panicking, so the generator can never produce a failure unrelated to the property.
fn pattern_set() -> impl Strategy<Value = PatternSet> {
    proptest::collection::vec(pattern_source(), 0..=4).prop_map(|srcs| {
        PatternSet::parse(srcs.into_iter().filter(|s| !s.is_empty()))
            .unwrap_or_else(|_| PatternSet::empty())
    })
}

/// A concrete repository-relative path drawn from the same alphabet (skipping the wildcard
/// segments, which are not valid literal path components), so pattern/path collisions are
/// frequent and adversarial.
fn concrete_path() -> impl Strategy<Value = String> {
    let literal =
        || proptest::sample::select(&["src", "tests", "docs", "auth", "parser", "mod.rs"][..]);
    proptest::collection::vec(literal(), 1..=4).prop_map(|segs| segs.join("/"))
}

fn repo_authority() -> impl Strategy<Value = RepoAuthority> {
    (pattern_set(), pattern_set()).prop_map(|(read, write)| RepoAuthority { read, write })
}

fn git_authority() -> impl Strategy<Value = GitAuthority> {
    any::<(bool, bool, bool, bool, bool)>().prop_map(|(commit, branch, merge, push, force)| {
        GitAuthority {
            commit,
            branch,
            merge,
            push,
            force,
        }
    })
}

fn ticket_authority() -> impl Strategy<Value = TicketAuthority> {
    any::<(bool, bool, bool, bool, bool, bool)>().prop_map(
        |(create_children, delegate_children, modify_siblings, close, cancel, reopen)| {
            TicketAuthority {
                create_children,
                delegate_children,
                modify_siblings,
                close,
                cancel,
                reopen,
            }
        },
    )
}

fn project_authority() -> impl Strategy<Value = ProjectAuthority> {
    any::<(bool, bool, bool, bool, bool, bool)>().prop_map(
        |(
            modify_spec,
            modify_vision,
            modify_milestones,
            close_milestone,
            reopen_milestone,
            modify_harness,
        )| {
            ProjectAuthority {
                modify_spec,
                modify_vision,
                modify_milestones,
                close_milestone,
                reopen_milestone,
                modify_harness,
            }
        },
    )
}

/// A small alphabet of hosts, including some covered by [`NetworkAuthority::DOC_HOSTS`], so
/// allowlist containment is exercised against both custom and doc hosts.
const HOSTS: &[&str] = &["docs.rs", "example.com", "internal.corp", "crates.io"];

fn network_authority() -> impl Strategy<Value = NetworkAuthority> {
    (
        any::<bool>(),
        any::<bool>(),
        proptest::collection::vec(proptest::sample::select(HOSTS), 0..=3),
    )
        .prop_map(|(docs, arbitrary, hosts)| NetworkAuthority {
            docs,
            arbitrary,
            allowlist: hosts.into_iter().map(String::from).collect::<BTreeSet<_>>(),
        })
}

fn shell_authority() -> impl Strategy<Value = ShellAuthority> {
    (any::<bool>(), pattern_set(), pattern_set()).prop_map(|(enabled, allow, deny)| {
        ShellAuthority {
            enabled,
            allow,
            deny,
        }
    })
}

fn computer_authority() -> impl Strategy<Value = ComputerAuthority> {
    any::<(bool, bool, bool)>().prop_map(|(input, capture, clipboard)| ComputerAuthority {
        input,
        capture,
        clipboard,
    })
}

fn resource_authority() -> impl Strategy<Value = ResourceAuthority> {
    (
        prop_oneof![Just(0u32), Just(1u32), 0u32..8, Just(u32::MAX)],
        prop_oneof![Just(0u32), Just(1u32), 0u32..8, Just(u32::MAX)],
    )
        .prop_map(|(max_workers, max_concurrent_commands)| ResourceAuthority {
            max_workers,
            max_concurrent_commands,
        })
}

/// A budget strategy that always exercises the `u64::MAX` unlimited sentinel alongside small
/// finite limits and a nonzero already-spent amount (still valid: spent may exceed a limit set
/// after the fact, and `remaining` saturates).
fn budget() -> impl Strategy<Value = Budget> {
    let limit = || prop_oneof![Just(0u64), Just(u64::MAX), 0u64..1_000];
    (
        limit(),
        limit(),
        limit(),
        0u64..1_000,
        0u64..1_000,
        0u64..1_000,
    )
        .prop_map(|(tokens, dollars_micros, wall_seconds, st, sd, sw)| {
            let mut b = Budget::new(tokens, dollars_micros, wall_seconds);
            b.spent = Spend {
                tokens: st,
                dollars_micros: sd,
                wall_seconds: sw,
            };
            b
        })
}

/// A strategy for a small spend, used to probe budget's `try_spend`.
fn spend() -> impl Strategy<Value = Spend> {
    (0u64..2_000, 0u64..2_000, 0u64..2_000).prop_map(|(tokens, dollars_micros, wall_seconds)| {
        Spend {
            tokens,
            dollars_micros,
            wall_seconds,
        }
    })
}

/// A strategy for an arbitrary [`Authority`], composing all the sub-strategies above.
fn authority() -> impl Strategy<Value = Authority> {
    (
        repo_authority(),
        git_authority(),
        ticket_authority(),
        project_authority(),
        network_authority(),
        shell_authority(),
        computer_authority(),
        resource_authority(),
        budget(),
    )
        .prop_map(
            |(repository, git, tickets, project, network, shell, computer, resources, budget)| {
                Authority {
                    repository,
                    git,
                    tickets,
                    project,
                    network,
                    shell,
                    computer,
                    resources,
                    budget,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// 1. Reflexivity: every authority contains itself.
    #[test]
    fn reflexivity(a in authority()) {
        prop_assert!(
            a.contains(&a),
            "authority does not contain itself: {a:?}\nfailures: {:?}",
            a.containment_failures(&a)
        );
    }

    /// 2. Transitivity: a.contains(b) && b.contains(c) implies a.contains(c).
    #[test]
    fn transitivity(a in authority(), b in authority(), c in authority()) {
        if a.contains(&b) && b.contains(&c) {
            prop_assert!(
                a.contains(&c),
                "containment is not transitive:\na = {a:?}\nb = {b:?}\nc = {c:?}\na.contains(c) failures: {:?}",
                a.containment_failures(&c)
            );
        }
    }

    /// 3. Attenuation never widens: if a.attenuate(&r) is Ok(g) then a.contains(&g).
    #[test]
    fn attenuate_never_widens(a in authority(), r in authority()) {
        if let Ok(g) = a.attenuate(&r) {
            prop_assert!(
                a.contains(&g),
                "attenuate produced something wider than the grantor:\na = {a:?}\nr = {r:?}\ng = {g:?}\nfailures: {:?}",
                a.containment_failures(&g)
            );
        }
    }

    /// 4. Lossy attenuation never widens: a.contains(&a.attenuate_lossy(&r)) for every r.
    #[test]
    fn attenuate_lossy_never_widens(a in authority(), r in authority()) {
        let g = a.attenuate_lossy(&r);
        prop_assert!(
            a.contains(&g),
            "attenuate_lossy produced something wider than the grantor:\na = {a:?}\nr = {r:?}\ng = {g:?}\nfailures: {:?}",
            a.containment_failures(&g)
        );
    }

    /// 5. Intersection is a lower bound: both operands contain it.
    #[test]
    fn intersection_is_a_lower_bound(a in authority(), b in authority()) {
        let i = a.intersect(&b);
        prop_assert!(
            a.contains(&i),
            "a does not contain a.intersect(b):\na = {a:?}\nb = {b:?}\ni = {i:?}\nfailures: {:?}",
            a.containment_failures(&i)
        );
        prop_assert!(
            b.contains(&i),
            "b does not contain a.intersect(b):\na = {a:?}\nb = {b:?}\ni = {i:?}\nfailures: {:?}",
            b.containment_failures(&i)
        );
    }

    /// 6. Root is a top element: Authority::root().contains(&a) for every a.
    #[test]
    fn root_is_top(a in authority()) {
        let root = Authority::root();
        prop_assert!(
            root.contains(&a),
            "root does not contain: {a:?}\nfailures: {:?}",
            root.containment_failures(&a)
        );
    }

    /// 7. None is a bottom element: a.contains(&Authority::none()) for every a.
    #[test]
    fn none_is_bottom(a in authority()) {
        let none = Authority::none();
        prop_assert!(
            a.contains(&none),
            "authority does not contain none: {a:?}\nfailures: {:?}",
            a.containment_failures(&none)
        );
    }

    /// 8. Pattern subset safety: if p.is_subset_of(&q) then for every generated path,
    /// p.matches(path) implies q.matches(path). This is the property that actually matters for
    /// security: a false positive here means a delegatee could read or write outside its scope.
    #[test]
    fn pattern_subset_is_matching_safe(p in pattern_set(), q in pattern_set(), path in concrete_path()) {
        if p.is_subset_of(&q) && p.matches(&path) {
            prop_assert!(
                q.matches(&path),
                "is_subset_of lied: p = {p:?} is_subset_of q = {q:?}, but p matches {path:?} and q does not"
            );
        }
    }

    /// 9. Delegation chains: attenuating repeatedly down a chain yields something the original
    /// still contains, at every step.
    #[test]
    fn delegation_chain_stays_contained(
        a in authority(),
        r1 in authority(),
        r2 in authority(),
        r3 in authority(),
    ) {
        let mut chain = vec![a.clone()];
        for r in [&r1, &r2, &r3] {
            let prev = chain.last().unwrap();
            let next = prev.attenuate_lossy(r);
            prop_assert!(
                a.contains(&next),
                "chain escaped the root grantor:\na = {a:?}\nchain so far = {chain:?}\nnext = {next:?}\nfailures: {:?}",
                a.containment_failures(&next)
            );
            chain.push(next);
        }
    }

    /// 10. Budget: try_spend never lets spent exceed the limit, and a refused spend changes
    /// nothing.
    #[test]
    fn budget_try_spend_is_safe(mut b in budget(), s in spend()) {
        let before = b;
        match b.try_spend(s) {
            Ok(()) => {
                prop_assert!(
                    b.tokens == u64::MAX || b.spent.tokens <= b.tokens,
                    "token spend exceeded the limit: before = {before:?}, spend = {s:?}, after = {b:?}"
                );
                prop_assert!(
                    b.dollars_micros == u64::MAX || b.spent.dollars_micros <= b.dollars_micros,
                    "dollar spend exceeded the limit: before = {before:?}, spend = {s:?}, after = {b:?}"
                );
                prop_assert!(
                    b.wall_seconds == u64::MAX || b.spent.wall_seconds <= b.wall_seconds,
                    "time spend exceeded the limit: before = {before:?}, spend = {s:?}, after = {b:?}"
                );
            }
            Err(_) => {
                prop_assert_eq!(
                    b, before,
                    "a refused spend mutated the budget: before = {:?}, spend = {:?}, after = {:?}",
                    before, s, b
                );
            }
        }
    }

    /// 11. Intersection is idempotent: `a.intersect(&a)` leaves every non-budget field
    /// unchanged, and leaves the budget's *remaining* capacity unchanged even though
    /// `Budget::intersect` documents that it resets `spent` to zero (so the limit fields
    /// themselves collapse to whatever was remaining, not the original limit).
    #[test]
    fn intersect_is_idempotent(a in authority()) {
        let i = a.intersect(&a);
        prop_assert_eq!(&i.repository, &a.repository, "repository changed under self-intersection");
        prop_assert_eq!(&i.git, &a.git, "git changed under self-intersection");
        prop_assert_eq!(&i.tickets, &a.tickets, "tickets changed under self-intersection");
        prop_assert_eq!(&i.project, &a.project, "project changed under self-intersection");
        prop_assert_eq!(&i.network, &a.network, "network changed under self-intersection");
        prop_assert_eq!(&i.shell, &a.shell, "shell changed under self-intersection");
        prop_assert_eq!(&i.resources, &a.resources, "resources changed under self-intersection");
        prop_assert_eq!(
            i.budget.remaining(), a.budget.remaining(),
            "self-intersection changed remaining budget: a = {:?}, a.intersect(&a) = {:?}", a, i
        );
    }

    /// 12. Budget: a sequence of arbitrary spends never drives `spent` past the limit on any
    /// component, and every refused spend in the sequence leaves the budget bit-identical to
    /// what it was immediately before that call (no partial application, no underflow).
    #[test]
    fn repeated_spends_never_exceed_the_limit(
        tokens in 0u64..1_000,
        dollars_micros in 0u64..1_000,
        wall_seconds in 0u64..1_000,
        spends in proptest::collection::vec(spend(), 1..=8),
    ) {
        let mut b = Budget::new(tokens, dollars_micros, wall_seconds);
        for s in spends {
            let before = b;
            match b.try_spend(s) {
                Ok(()) => {
                    prop_assert!(
                        b.tokens == u64::MAX || b.spent.tokens <= b.tokens,
                        "token spend exceeded the limit: before = {before:?}, spend = {s:?}, after = {b:?}"
                    );
                    prop_assert!(
                        b.dollars_micros == u64::MAX || b.spent.dollars_micros <= b.dollars_micros,
                        "dollar spend exceeded the limit: before = {before:?}, spend = {s:?}, after = {b:?}"
                    );
                    prop_assert!(
                        b.wall_seconds == u64::MAX || b.spent.wall_seconds <= b.wall_seconds,
                        "time spend exceeded the limit: before = {before:?}, spend = {s:?}, after = {b:?}"
                    );
                }
                Err(_) => {
                    prop_assert_eq!(
                        b, before,
                        "a refused spend in the sequence mutated the budget: before = {:?}, spend = {:?}, after = {:?}",
                        before, s, b
                    );
                }
            }
        }
    }
}
