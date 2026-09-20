//! [`SkillCapability`]: the `skill.load` tool (`docs/audit-2026-09-18-fable.md` M-04 —
//! "SKILL.md discovery under `.tm/skills/**` with metadata-only in the pack and body loaded by
//! a `skill.load` tool").
//!
//! A standalone [`CapabilityProvider`] rather than a new [`crate::tools::ToolName`] variant: that
//! private enum's cardinality is load-bearing (`crate::tools::ToolRegistry::standard`'s own doc
//! comment: "several tests in this module assert exact admitted-tool counts against
//! `ToolName::ALL.len()`"), so adding to it would force updating every one of those counts and
//! every "39-tool" mention in this crate's prose for a single new tool. Registering a separate
//! provider is the same seam `tm-browser`'s [`tm_browser::BrowserCapability`] and `tm-computer`'s
//! `ComputerCapability` already use for exactly this reason (`docs/audit-2026-09-18-fable.md`
//! A-01/B-02) — this module follows their shape: an `id()` slug, one `ToolSchema`, `to_action`/
//! `requires`/`invoke`.
//!
//! [`crate::tools::ToolRegistry::with_capabilities`]'s `extra` argument is where a binary
//! registers this alongside the builtin/browser/computer providers (see `tm-cli`'s
//! `run_turn_streaming` and `tm-agent`'s `executor::BuiltinExecutor::build`).

use serde_json::{json, Value};

use tm_types::{
    Action, AuthorityRequirement, CallContext, CapabilityProvider, CostClass, Result, ToolSchema,
};

use crate::tools::get_str;

/// The `skill.load` tool's dotted wire name.
pub const SKILL_LOAD: &str = "skill.load";

/// A [`CapabilityProvider`] contributing exactly one tool, `skill.load`: given a skill `name`
/// (as advertised by [`tm_context::skills::discover_skills`]'s metadata, already folded into
/// every context pack's Conventions section — see `tm-context`'s `sections::build_conventions`),
/// returns that skill's full body. Stateless: everything it needs (the project root) comes from
/// [`CallContext::root`] on each call, so this holds no fields of its own.
#[derive(Debug, Default, Clone, Copy)]
pub struct SkillCapability;

impl SkillCapability {
    /// Build a new skill capability. Takes no arguments today (see this type's doc comment for
    /// why) but is a constructor, not a unit struct literal, so a future field (a cache, a
    /// configured skills root override) can be added without an API break.
    pub fn new() -> Self {
        SkillCapability
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for SkillCapability {
    fn id(&self) -> &str {
        "skill"
    }

    fn tools(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: SKILL_LOAD,
            description: "Load a discovered skill's full body by name. Skill names and \
                one-line descriptions already appear in this context's Conventions section; \
                call this only for a skill worth reading in full.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }),
            cost: CostClass::Cheap,
            requires: AuthorityRequirement::RepoRead,
        }]
    }

    fn to_action(&self, tool: &str, input: &Value) -> Result<Action> {
        if tool != SKILL_LOAD {
            return Err(tm_types::TmError::parse(format!("unknown tool `{tool}`")));
        }
        // `.tm/skills/**` is outside any ticket's ordinary source-tree resource claims (see
        // this module's doc comment); gating this read the same way `crate::tools`'s own module
        // doc comment describes for its closed-`Action`-vocabulary gaps — "the closest existing
        // governance switch, a safe (denies-by-default) choice rather than an exact semantic
        // match" — rather than inventing a bypass around `Authority::permits` for this one tool.
        // A worker whose authority does not grant read access under `.tm/skills/` (or broader)
        // cannot call this even though the tool is always advertised (`requires: RepoRead`
        // above only checks "some read authority exists", not this specific path); a project
        // that wants skills broadly available should include `.tm/skills/**` in a ticket's read
        // authority, the same way it would for any other shared resource.
        //
        // `to_action` is pure (no filesystem access, per the trait's own contract), so it cannot
        // resolve `name` against `tm_context::skills::discover_skills`'s actual nesting the way
        // `invoke` does; it assumes the primary, documented layout (`.tm/skills/<name>/SKILL.md`,
        // one directory deep) rather than the fully general recursive one `discover_skills`
        // supports. A skill nested deeper than one level still loads via `invoke` (which does
        // resolve the real path) but is checked against this shallower assumed path here — a
        // known, narrow approximation, not a security hole: it only ever makes the check
        // *stricter* than the real path would need for a project whose `.tm/skills/**` grant is
        // exactly that pattern, never looser.
        let name = get_str(input, "name")?;
        Ok(Action::ReadPath {
            path: format!(".tm/skills/{name}/SKILL.md"),
        })
    }

    fn requires(&self) -> AuthorityRequirement {
        AuthorityRequirement::RepoRead
    }

    async fn invoke(&self, tool: &str, input: Value, ctx: &CallContext<'_>) -> Result<Value> {
        if tool != SKILL_LOAD {
            return Err(tm_types::TmError::parse(format!("unknown tool `{tool}`")));
        }
        let name = get_str(&input, "name")?;
        let doc = tm_context::skills::load_skill(ctx.root, name)?;
        Ok(json!({
            "name": doc.metadata.name,
            "description": doc.metadata.description,
            "path": doc.metadata.path,
            "body": doc.body,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{
        Authority, FixedClock, ParticipantId, PatternSet, SessionId, TestIds, TicketId,
    };

    fn test_actor() -> ParticipantId {
        "agent:test/worker".parse().expect("valid participant id")
    }

    fn write_skill(root: &std::path::Path, rel_dir: &str, front: &str, body: &str) {
        let dir = root.join(".tm/skills").join(rel_dir);
        std::fs::create_dir_all(&dir).expect("create skill dir");
        std::fs::write(dir.join("SKILL.md"), format!("---\n{front}\n---\n{body}"))
            .expect("write SKILL.md");
    }

    fn root_authority() -> Authority {
        Authority::root()
    }

    #[tokio::test]
    async fn invoke_returns_the_full_body_for_a_discovered_skill() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        write_skill(
            dir.path(),
            "demo",
            "name: demo-skill\ndescription: does a demo thing",
            "SENTINEL-BODY-CONTENT\n",
        );

        let authority = root_authority();
        let ticket: TicketId = "T-1".parse().expect("valid ticket id");
        let session: SessionId = "S-1".parse().expect("valid session id");
        let actor = test_actor();
        let clock = FixedClock::epoch();
        let ids = TestIds::new();
        let ctx = CallContext {
            authority: &authority,
            ticket: &ticket,
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: dir.path(),
        };

        let capability = SkillCapability::new();
        let result = capability
            .invoke(SKILL_LOAD, json!({"name": "demo-skill"}), &ctx)
            .await
            .expect("invoke succeeds");
        assert_eq!(result["name"], "demo-skill");
        assert!(result["body"]
            .as_str()
            .expect("body is a string")
            .contains("SENTINEL-BODY-CONTENT"));
    }

    #[tokio::test]
    async fn invoke_errors_for_an_unknown_skill_name() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let authority = root_authority();
        let ticket: TicketId = "T-1".parse().expect("valid ticket id");
        let session: SessionId = "S-1".parse().expect("valid session id");
        let actor = test_actor();
        let clock = FixedClock::epoch();
        let ids = TestIds::new();
        let ctx = CallContext {
            authority: &authority,
            ticket: &ticket,
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: dir.path(),
        };

        let capability = SkillCapability::new();
        let result = capability
            .invoke(SKILL_LOAD, json!({"name": "nonexistent"}), &ctx)
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn to_action_maps_to_a_read_path_action_under_dot_tm_skills() {
        let capability = SkillCapability::new();
        let action = capability
            .to_action(SKILL_LOAD, &json!({"name": "demo-skill"}))
            .expect("to_action succeeds");
        match action {
            Action::ReadPath { path } => assert!(path.contains("demo-skill")),
            other => panic!("expected ReadPath, got {other:?}"),
        }
    }

    #[test]
    fn scoped_authority_without_dot_tm_skills_denies_the_mapped_action() {
        let capability = SkillCapability::new();
        let action = capability
            .to_action(SKILL_LOAD, &json!({"name": "demo-skill"}))
            .expect("to_action succeeds");

        let mut scoped = Authority::none();
        scoped.repository.read =
            PatternSet::parse(["crates/tm-foo/**".to_string()]).expect("valid pattern");
        assert!(matches!(
            scoped.permits(&action),
            tm_types::Decision::Deny(_)
        ));

        let mut broad = Authority::none();
        broad.repository.read =
            PatternSet::parse([".tm/skills/**".to_string()]).expect("valid pattern");
        assert!(matches!(broad.permits(&action), tm_types::Decision::Allow));
    }
}
