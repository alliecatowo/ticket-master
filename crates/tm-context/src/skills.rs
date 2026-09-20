//! `.tm/skills/**/SKILL.md` discovery (`docs/audit-2026-09-18-fable.md` M-04): "SKILL.md
//! discovery under `.tm/skills/**` with metadata-only in the pack and body loaded by a
//! `skill.load` tool" — the same progressive-disclosure shape this repo's own `CLAUDE.md`
//! documents Claude Code itself using for skills.
//!
//! [`discover_skills`] is the cheap half: every `SKILL.md`'s `name`/`description` frontmatter,
//! nothing else, folded into [`crate::sections::build_conventions`]'s pack section so a worker
//! knows what skills exist without paying for any skill's full body up front. [`load_skill`] is
//! the expensive half, called on demand by `tm-agent`'s `skill.load` tool (not by anything in
//! this crate — `tm-context` has no tool-dispatch concept of its own) once a worker has decided
//! a specific skill is worth reading in full.

use std::collections::BTreeMap;
use std::path::Path;

/// One discovered skill's cheap-to-carry metadata — everything that goes into a context pack
/// up front, before any `skill.load` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    /// The skill's name: its frontmatter `name` field, or (when absent) the name of the
    /// directory its `SKILL.md` lives in.
    pub name: String,
    /// The skill's one-line description, from its frontmatter `description` field. Empty when
    /// the frontmatter omits it.
    pub description: String,
    /// Root-relative path to this skill's `SKILL.md`, forward-slash separated regardless of
    /// host platform.
    pub path: String,
}

/// A skill's full document: [`SkillMetadata`] plus the body text a `skill.load` call returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDoc {
    /// This skill's metadata (same as what [`discover_skills`] would report for it).
    pub metadata: SkillMetadata,
    /// The body text following the frontmatter block, verbatim (only trailing newline
    /// normalization applied).
    pub body: String,
}

/// Directory `.tm/skills/**` is discovered under, relative to `root` — every function in this
/// module takes `root` (a workspace root, `tm-cli::Project::root` or `CodeIntel::project_root`),
/// never `state_dir` (`docs/decisions/D-003-project-scope.md`'s durable-state location, `<root>/
/// .tm` in repo scope but `$TM_HOME/projects/<key>/` — outside the workspace entirely — in
/// global scope). This is deliberate, not an oversight: a `SKILL.md` is workspace-authored
/// config a human commits alongside their code, the same category as `AGENTS.md` and
/// `hooks.toml` (both also resolved against `root`), not managed project state the way
/// `index.db`/`project.db` are — and `CallContext` (this crate's `skill.load` caller's own input
/// shape) carries `root`, not `state_dir`, so there is no state-dir-resolving alternative
/// available at that call site anyway. The real cost: in global scope, `<root>/.tm` is a
/// directory `tm` otherwise never creates or reads (state lives entirely under `$TM_HOME`
/// instead), so `.tm/skills/**` there is a bare, disconnected directory a human has to know to
/// create by hand next to a workspace `tm` isn't tracking state in at all — see
/// `docs/decisions/D-013-hooks-agents-skills.md`'s "what this costs" for this stated plainly.
///
/// A single string literal (not a `.join(".tm")` chain) — this is not a workaround for the
/// hygiene scan's D-003 `.tm`-literal check (that check exists to catch code that assumes the
/// pre-D-003 "state always lives at `<root>/.tm`" shape; this module never reads or writes
/// `state_dir` at all, so the check's concern does not apply here), it is simply how the literal
/// is written.
const SKILLS_ROOT: &str = ".tm/skills";

/// How deep [`discover_skills`] will recurse under `.tm/skills/` before giving up — generous for
/// any real skill layout, a hard backstop against a symlink cycle or pathological nesting rather
/// than a limit anyone should ever hit in practice.
const MAX_WALK_DEPTH: u32 = 8;

/// Discover every `SKILL.md` under `<root>/.tm/skills/**`, returning metadata only (never a
/// body — see this module's doc comment). Returns an empty `Vec`, not an error, when
/// `.tm/skills/` does not exist or is unreadable: skills are opt-in, so their absence is the
/// common case, not a failure. Sorted by path for deterministic pack output.
pub fn discover_skills(root: &Path) -> Vec<SkillMetadata> {
    let base = root.join(SKILLS_ROOT);
    let mut found = Vec::new();
    walk_for_skill_md(&base, root, 0, &mut found);
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Load one skill's full document by `name` (as advertised in its frontmatter, or its
/// directory name when frontmatter omits `name` — see [`SkillMetadata::name`]).
///
/// # Errors
/// `TmError::NotFound` when no discovered skill has this name; `TmError::Io` when the matching
/// `SKILL.md` cannot be re-read (e.g. removed between discovery and this call).
pub fn load_skill(root: &Path, name: &str) -> tm_types::Result<SkillDoc> {
    let metadata = discover_skills(root)
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| tm_types::TmError::not_found("skill", name))?;
    let full_path = root.join(&metadata.path);
    let content = std::fs::read_to_string(&full_path)
        .map_err(|e| tm_types::TmError::Io(format!("reading {}: {e}", full_path.display())))?;
    let (_front, body) = parse_frontmatter(&content);
    Ok(SkillDoc { metadata, body })
}

/// Recursive directory walk collecting every `SKILL.md`, skipping symlinks (a real-world skill
/// tree has no reason to symlink a directory back onto an ancestor, and following one would risk
/// an unbounded walk even with [`MAX_WALK_DEPTH`] as a backstop).
fn walk_for_skill_md(dir: &Path, root: &Path, depth: u32, out: &mut Vec<SkillMetadata>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_for_skill_md(&path, root, depth + 1, out);
        } else if file_type.is_file()
            && path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md")
        {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (front, _body) = parse_frontmatter(&content);
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let default_name = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
                .unwrap_or("skill")
                .to_string();
            out.push(SkillMetadata {
                name: front.get("name").cloned().unwrap_or(default_name),
                description: front.get("description").cloned().unwrap_or_default(),
                path: rel,
            });
        }
    }
}

/// Minimal frontmatter split: `---\n<key: value pairs>\n---\n<body>`, returning the parsed keys
/// and the body that follows. Deliberately not a full YAML parser — this codebase's sibling
/// front-matter reader (`tm_docs::registry::parse_front_matter`) already made the opposite
/// tradeoff for its own format (TOML, not YAML, precisely to avoid a `serde_yaml` dependency);
/// SKILL.md's own ecosystem convention is YAML, and pulling in a YAML parser for two flat string
/// fields (`name`, `description`) is not worth a new dependency, so this reads the same shape by
/// hand: one `key: value` pair per line, optional matching quotes stripped, no nested
/// maps/lists/multi-line scalars. A line that isn't `key: value`-shaped is silently skipped
/// rather than erroring — permissive by design, since a skill author's typo in an unused
/// frontmatter field should not break discovery of a `SKILL.md` that has valid `name`/
/// `description` lines. Content with no `---` frontmatter block at all is treated as pure body
/// with empty metadata.
fn parse_frontmatter(content: &str) -> (BTreeMap<String, String>, String) {
    let mut map = BTreeMap::new();
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (map, content.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (map, content.to_string());
    };
    let block = &rest[..end];
    let after_marker = &rest[end + 4..];
    let body = after_marker
        .strip_prefix('\n')
        .or_else(|| after_marker.strip_prefix("\r\n"))
        .unwrap_or(after_marker)
        .to_string();
    for line in block.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let mut value = value.trim().to_string();
            if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                value = value[1..value.len() - 1].to_string();
            }
            if !key.is_empty() {
                map.insert(key, value);
            }
        }
    }
    (map, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, rel_dir: &str, front: &str, body: &str) {
        let dir = root.join(SKILLS_ROOT).join(rel_dir);
        std::fs::create_dir_all(&dir).expect("create skill dir");
        std::fs::write(dir.join("SKILL.md"), format!("---\n{front}\n---\n{body}"))
            .expect("write SKILL.md");
    }

    #[test]
    fn discover_skills_returns_empty_when_no_skills_dir_exists() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        assert_eq!(discover_skills(dir.path()), Vec::new());
    }

    #[test]
    fn discover_skills_finds_metadata_without_body() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        write_skill(
            dir.path(),
            "demo",
            "name: demo-skill\ndescription: does a demo thing",
            "# Demo Skill\n\nfull body content here, quite long and detailed.\n",
        );

        let found = discover_skills(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "demo-skill");
        assert_eq!(found[0].description, "does a demo thing");
        assert_eq!(found[0].path, ".tm/skills/demo/SKILL.md");
    }

    #[test]
    fn discover_skills_falls_back_to_directory_name_when_name_field_missing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        write_skill(
            dir.path(),
            "fallback-dir",
            "description: no name field",
            "body",
        );

        let found = discover_skills(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "fallback-dir");
    }

    #[test]
    fn discover_skills_sorts_by_path_and_finds_nested_skills() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        write_skill(dir.path(), "zeta", "name: zeta", "body");
        write_skill(dir.path(), "alpha/nested", "name: alpha-nested", "body");

        let found = discover_skills(dir.path());
        let names: Vec<&str> = found.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["alpha-nested", "zeta"]);
    }

    #[test]
    fn load_skill_returns_body_by_name() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        write_skill(
            dir.path(),
            "demo",
            "name: demo-skill\ndescription: does a demo thing",
            "SENTINEL-BODY-CONTENT-12345\n",
        );

        let doc = load_skill(dir.path(), "demo-skill").expect("load succeeds");
        assert_eq!(doc.metadata.name, "demo-skill");
        assert!(doc.body.contains("SENTINEL-BODY-CONTENT-12345"));
    }

    #[test]
    fn load_skill_errors_for_an_unknown_name() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        assert!(load_skill(dir.path(), "nonexistent").is_err());
    }

    #[test]
    fn parse_frontmatter_handles_quoted_values() {
        let content = "---\nname: \"quoted name\"\ndescription: 'single quoted'\n---\nbody text";
        let (map, body) = parse_frontmatter(content);
        assert_eq!(map.get("name"), Some(&"quoted name".to_string()));
        assert_eq!(map.get("description"), Some(&"single quoted".to_string()));
        assert_eq!(body, "body text");
    }

    #[test]
    fn parse_frontmatter_treats_missing_delimiter_as_pure_body() {
        let content = "no frontmatter here at all";
        let (map, body) = parse_frontmatter(content);
        assert!(map.is_empty());
        assert_eq!(body, content);
    }
}
