//! Maps an ACP `session/request_permission` call onto this codebase's own `Authority::permits`,
//! instead of the client auto-approving or auto-denying every tool call an external agent wants
//! to make (this crate's core deliverable — see the crate's top-level doc comment).
//!
//! Three deliberate fail-closed rules, each load-bearing:
//!
//! 1. A [`ToolCallUpdate`] that cannot be mapped to a known [`Action`] (an unrecognized
//!    [`ToolKind`], missing `locations`, or a `rawInput` shape this module doesn't recognize) is
//!    **denied**, never allowed by default — mirroring `tm_core::executor::validate_return_scope`
//!    returning `Unparseable` rather than trusting an unparseable diff.
//! 2. [`evaluate`] never invents a [`PermissionOption::option_id`]. [`choose_option`] only ever
//!    returns an id the agent itself offered in [`RequestPermissionRequest::options`]; if
//!    `Authority::permits` allows an action but the agent offered no `allow_*` option (or vice
//!    versa for a denial), the response is [`RequestPermissionOutcome::Cancelled`] rather than a
//!    fabricated selection.
//! 3. Among options of the wanted polarity, an `_once` option is always preferred over an
//!    `_always` one, even when both are offered. `Authority::permits` is meant to be re-checked
//!    on every action against the ticket's own attenuated grant; letting a remote agent cache
//!    "always" for a permission class would let it skip that re-check for every later call in
//!    the session that this process never sees again to re-authorize.

use std::path::{Component, Path};

use tm_types::{Action, Authority, Decision};

use crate::protocol::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, ToolCallUpdate, ToolKind,
};

/// The result of checking a tool call's mapped action(s) against an [`Authority`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Every mapped action was allowed.
    Allow,
    /// At least one mapped action was denied (first reason kept), or the call could not be
    /// mapped to a known action at all.
    Deny(String),
}

/// Rewrite an absolute path from the wire into a repository-relative path suitable for
/// `PatternSet` matching (`Authority.repository.{read,write}` patterns are matched against
/// repo-relative paths, e.g. `"src/lib.rs"`, never absolute ones).
///
/// Fails closed (`None`) rather than trusting the input when:
/// - `absolute` is not actually absolute (the schema guarantees `ToolCallLocation::path` is, so
///   a relative path here means either a non-conformant peer or `cwd` desync — either way, not
///   safe to guess at),
/// - `absolute` does not fall under `root` at all, or
/// - `absolute` contains a literal `..` component even after stripping the `root` prefix (a
///   lexical escape like `root/a/../../etc/passwd`, which would otherwise still lexically
///   "start with" `root` as a string).
///
/// This is a purely lexical check (no `fs::canonicalize`), matching the rest of this codebase's
/// own precedent for path/authority checks (`tm_core::executor::touched_paths` and
/// `validate_return_scope` are equally lexical) — and necessarily so here, since a tool wanting
/// to *create* a new file is checking a path that may not exist on disk yet.
fn relativize(root: &Path, absolute: &str) -> Option<String> {
    let candidate = Path::new(absolute);
    if !candidate.is_absolute() {
        return None;
    }
    let rel = candidate.strip_prefix(root).ok()?;
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let rendered = rel.to_str()?.replace('\\', "/");
    Some(rendered)
}

/// Recover an argv from an `execute` tool call's `rawInput`. Recognizes `{"command": [...]}`
/// (array of strings) and `{"command": "..."}` (whitespace-split as a heuristic fallback) —
/// nothing else. This is deliberately narrow: `rawInput` is tool-specific JSON with no schema
/// ACP itself standardizes, so a shape this function doesn't recognize is refused rather than
/// guessed at.
fn extract_argv(raw_input: &serde_json::Value) -> Option<Vec<String>> {
    let obj = raw_input.as_object()?;
    if let Some(arr) = obj.get("command").and_then(|v| v.as_array()) {
        let argv: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        return (!argv.is_empty() && argv.len() == arr.len()).then_some(argv);
    }
    if let Some(s) = obj.get("command").and_then(|v| v.as_str()) {
        let argv: Vec<String> = s.split_whitespace().map(String::from).collect();
        return (!argv.is_empty()).then_some(argv);
    }
    None
}

/// Recover a URL from a `fetch` tool call's `rawInput`: `{"url": "..."}`, nothing else.
fn extract_url(raw_input: &serde_json::Value) -> Option<String> {
    raw_input
        .as_object()?
        .get("url")?
        .as_str()
        .map(String::from)
}

/// Map `tool_call` to the [`Action`]s that must all be permitted for it to proceed, or `None`
/// if this module does not recognize the shape well enough to map it at all (see this module's
/// doc comment, rule 1). A `Some(vec)` conjunction is used (rather than a single `Action`)
/// because a tool call may touch multiple [`ToolCallLocation`]s (e.g. a `move`); every one of
/// them must be authorized.
fn candidate_actions(tool_call: &ToolCallUpdate, cwd: &Path) -> Option<Vec<Action>> {
    match tool_call.kind {
        Some(ToolKind::Read) | Some(ToolKind::Search) => {
            let locations = tool_call.locations.as_ref()?;
            if locations.is_empty() {
                return None;
            }
            locations
                .iter()
                .map(|loc| relativize(cwd, &loc.path).map(|path| Action::ReadPath { path }))
                .collect()
        }
        Some(ToolKind::Edit) | Some(ToolKind::Delete) | Some(ToolKind::Move) => {
            let locations = tool_call.locations.as_ref()?;
            if locations.is_empty() {
                return None;
            }
            locations
                .iter()
                .map(|loc| relativize(cwd, &loc.path).map(|path| Action::WritePath { path }))
                .collect()
        }
        Some(ToolKind::Execute) => {
            let argv = extract_argv(tool_call.raw_input.as_ref()?)?;
            Some(vec![Action::RunCommand { command: argv }])
        }
        Some(ToolKind::Fetch) => {
            let url = extract_url(tool_call.raw_input.as_ref()?)?;
            Some(vec![Action::NetFetch { url }])
        }
        // `Think` has no side effect to gate; `SwitchMode`/`Other`/absent `kind` carry no
        // information this module can safely turn into an `Action` — all fail closed via `None`.
        _ => None,
    }
}

/// Check `tool_call` (relative to session working directory `cwd`) against `authority`.
pub fn evaluate(
    authority: &Authority,
    tool_call: &ToolCallUpdate,
    cwd: &Path,
) -> PermissionDecision {
    let Some(actions) = candidate_actions(tool_call, cwd) else {
        return PermissionDecision::Deny(
            "tool call could not be mapped to a known action (unrecognized kind, missing \
             locations, or an unrecognized rawInput shape) — denied rather than assumed safe"
                .to_string(),
        );
    };
    for action in &actions {
        match authority.permits(action) {
            Decision::Allow => {}
            Decision::Deny(reason) => return PermissionDecision::Deny(reason),
            // `Authority::permits` never returns `NeedsApproval` today (see its doc comment:
            // "Oversight may escalate an Allow afterwards" is a separate, not-yet-wired layer —
            // `docs/audit-2026-09-18-fable.md` M-16), but treat it the same as a denial rather
            // than matching it exhaustively-by-omission if that ever changes: escalating to a
            // human is not something this synchronous callback can do.
            Decision::NeedsApproval(reason) => {
                return PermissionDecision::Deny(format!("needs human approval: {reason}"))
            }
        }
    }
    PermissionDecision::Allow
}

/// Pick one of `options` consistent with `decision`, per this module's doc comment rules 2 and
/// 3: never fabricate an id, and prefer `_once` over `_always` among options of the wanted
/// polarity.
pub fn choose_option(
    decision: &PermissionDecision,
    options: &[PermissionOption],
) -> RequestPermissionOutcome {
    let (once_kind, always_kind) = match decision {
        PermissionDecision::Allow => (
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ),
        PermissionDecision::Deny(_) => (
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ),
    };
    for wanted in [once_kind, always_kind] {
        if let Some(opt) = options.iter().find(|o| o.kind == wanted) {
            return RequestPermissionOutcome::Selected {
                option_id: opt.option_id.clone(),
            };
        }
    }
    RequestPermissionOutcome::Cancelled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ToolCallLocation, ToolCallStatus};
    use serde_json::json;
    use tm_types::{PatternSet, RepoAuthority};

    fn tool_call(kind: Option<ToolKind>) -> ToolCallUpdate {
        ToolCallUpdate {
            tool_call_id: "call-1".to_string(),
            kind,
            status: Some(ToolCallStatus::Pending),
            title: Some("test call".to_string()),
            name: None,
            locations: None,
            raw_input: None,
            extra: Default::default(),
        }
    }

    fn scoped_authority() -> Authority {
        Authority {
            repository: RepoAuthority {
                read: PatternSet::parse(["src/**"]).unwrap(),
                write: PatternSet::parse(["src/widget/**"]).unwrap(),
            },
            shell: tm_types::ShellAuthority {
                enabled: true,
                allow: PatternSet::parse(["cargo test*"]).unwrap(),
                deny: PatternSet::empty(),
                pty: false,
            },
            network: tm_types::NetworkAuthority {
                docs: true,
                ..Default::default()
            },
            ..Authority::none()
        }
    }

    #[test]
    fn relativize_strips_the_root_prefix() {
        let root = Path::new("/repo");
        assert_eq!(
            relativize(root, "/repo/src/lib.rs"),
            Some("src/lib.rs".to_string())
        );
    }

    #[test]
    fn relativize_denies_a_relative_path() {
        let root = Path::new("/repo");
        assert_eq!(relativize(root, "src/lib.rs"), None);
    }

    #[test]
    fn relativize_denies_a_path_outside_root() {
        let root = Path::new("/repo");
        assert_eq!(relativize(root, "/etc/passwd"), None);
    }

    #[test]
    fn relativize_denies_a_lexical_parent_dir_escape() {
        let root = Path::new("/repo");
        // Lexically starts with "/repo" as a string, but escapes it once "../.." is applied.
        assert_eq!(relativize(root, "/repo/a/../../etc/passwd"), None);
    }

    #[test]
    fn evaluate_allows_a_read_within_scope() {
        let authority = scoped_authority();
        let mut call = tool_call(Some(ToolKind::Read));
        call.locations = Some(vec![ToolCallLocation {
            path: "/repo/src/lib.rs".to_string(),
            line: None,
        }]);
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[test]
    fn evaluate_denies_a_write_outside_scope() {
        let authority = scoped_authority();
        let mut call = tool_call(Some(ToolKind::Edit));
        call.locations = Some(vec![ToolCallLocation {
            path: "/repo/src/other/lib.rs".to_string(),
            line: None,
        }]);
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn evaluate_conjunction_requires_every_location_to_be_permitted() {
        let authority = scoped_authority();
        let mut call = tool_call(Some(ToolKind::Edit));
        // First location is in scope, second is not — the whole call must be denied.
        call.locations = Some(vec![
            ToolCallLocation {
                path: "/repo/src/widget/a.rs".to_string(),
                line: None,
            },
            ToolCallLocation {
                path: "/repo/src/other/b.rs".to_string(),
                line: None,
            },
        ]);
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn evaluate_denies_when_locations_are_missing_for_a_path_shaped_kind() {
        let authority = scoped_authority();
        let call = tool_call(Some(ToolKind::Edit)); // no locations set
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn evaluate_denies_an_unmappable_kind_rather_than_defaulting_to_allow() {
        let authority = Authority::root(); // even full authority must not rescue an unmapped kind
        let call = tool_call(Some(ToolKind::Think));
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn evaluate_denies_a_missing_kind() {
        let authority = Authority::root();
        let call = tool_call(None);
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn evaluate_allows_an_execute_call_with_a_recognized_command_array() {
        let authority = scoped_authority();
        let mut call = tool_call(Some(ToolKind::Execute));
        call.raw_input = Some(json!({"command": ["cargo", "test"]}));
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[test]
    fn evaluate_denies_an_execute_call_not_in_the_shell_allowlist() {
        let authority = scoped_authority();
        let mut call = tool_call(Some(ToolKind::Execute));
        call.raw_input = Some(json!({"command": ["rm", "-rf", "/"]}));
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn evaluate_denies_an_execute_call_with_an_unrecognized_raw_input_shape() {
        let authority = Authority::root();
        let mut call = tool_call(Some(ToolKind::Execute));
        call.raw_input = Some(json!({"argv": ["cargo", "test"]})); // wrong key
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn evaluate_allows_a_fetch_within_network_authority() {
        let authority = scoped_authority();
        let mut call = tool_call(Some(ToolKind::Fetch));
        call.raw_input = Some(json!({"url": "https://docs.rs/serde"}));
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[test]
    fn evaluate_denies_a_fetch_outside_network_authority() {
        let authority = scoped_authority();
        let mut call = tool_call(Some(ToolKind::Fetch));
        call.raw_input = Some(json!({"url": "https://evil.example/x"}));
        let decision = evaluate(&authority, &call, Path::new("/repo"));
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption {
            option_id: id.to_string(),
            name: id.to_string(),
            kind,
        }
    }

    #[test]
    fn choose_option_prefers_allow_once_over_allow_always() {
        let options = vec![
            option("always", PermissionOptionKind::AllowAlways),
            option("once", PermissionOptionKind::AllowOnce),
        ];
        let outcome = choose_option(&PermissionDecision::Allow, &options);
        assert_eq!(
            outcome,
            RequestPermissionOutcome::Selected {
                option_id: "once".to_string()
            }
        );
    }

    #[test]
    fn choose_option_falls_back_to_allow_always_when_once_is_not_offered() {
        let options = vec![option("always", PermissionOptionKind::AllowAlways)];
        let outcome = choose_option(&PermissionDecision::Allow, &options);
        assert_eq!(
            outcome,
            RequestPermissionOutcome::Selected {
                option_id: "always".to_string()
            }
        );
    }

    #[test]
    fn choose_option_prefers_reject_once_for_a_denial() {
        let options = vec![
            option("reject-always", PermissionOptionKind::RejectAlways),
            option("reject-once", PermissionOptionKind::RejectOnce),
            option("allow-once", PermissionOptionKind::AllowOnce),
        ];
        let outcome = choose_option(&PermissionDecision::Deny("no".into()), &options);
        assert_eq!(
            outcome,
            RequestPermissionOutcome::Selected {
                option_id: "reject-once".to_string()
            }
        );
    }

    #[test]
    fn choose_option_cancels_rather_than_fabricating_an_id_when_no_matching_polarity_is_offered() {
        // The agent only offered allow options, but the decision is a denial.
        let options = vec![option("allow-once", PermissionOptionKind::AllowOnce)];
        let outcome = choose_option(&PermissionDecision::Deny("no".into()), &options);
        assert_eq!(outcome, RequestPermissionOutcome::Cancelled);
    }

    #[test]
    fn choose_option_cancels_on_an_empty_options_list() {
        let outcome = choose_option(&PermissionDecision::Allow, &[]);
        assert_eq!(outcome, RequestPermissionOutcome::Cancelled);
    }
}
