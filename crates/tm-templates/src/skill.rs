//! `skill.md`: conventions for a template's stack, loadable as a bounded context section a
//! worker can read — data, never instructions injected into a system prompt. This crate never
//! builds a prompt or talks to a provider; it only produces a [`Section`], the same currency
//! `tm-context` already hands workers for every other kind of context (decisions, retrieval,
//! git history, harness conventions). Whatever assembles a worker's actual prompt decides how
//! (or whether) a section's `body` text is shown — this module's only job is making that text
//! available in the same shape as everything else, not deciding its trust level.

use tm_context::pack::{ProvenanceRef, Section};
use tm_context::tokens::{estimate_tokens_prose, SectionKind};
use tm_types::{Result, TmError};

use crate::template::Template;

/// Load `template`'s `skill.md`, if it ships one, as a [`Section`] tagged
/// [`SectionKind::Conventions`] — the same kind `tm-context` already uses for harness/project
/// conventions, since a template's stack idioms are exactly that kind of thing. Returns
/// `Ok(None)` when the template has no `skill.md`: shipping one is optional.
///
/// # Errors
/// [`TmError::Io`] if `skill.md` exists but cannot be read.
pub fn load(template: &Template) -> Result<Option<Section>> {
    let path = template.skill_path();
    if !path.is_file() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path)
        .map_err(|e| TmError::Io(format!("reading {}: {e}", path.display())))?;
    let tokens = estimate_tokens_prose(&body);
    let bytes = body.len();
    Ok(Some(Section {
        kind: SectionKind::Conventions,
        title: format!("{} conventions", template.manifest.id),
        body,
        tokens,
        bytes,
        provenance: vec![ProvenanceRef {
            locator: path.to_string_lossy().into_owned(),
            detail: format!("skill.md shipped by template {}", template.manifest.id),
        }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::TemplateManifest;

    fn manifest() -> TemplateManifest {
        TemplateManifest {
            id: "fixture".to_string(),
            version: "0.1.0".to_string(),
            license: None,
            tags: vec![],
            params: vec![],
            checksum: String::new(),
        }
    }

    #[test]
    fn no_skill_md_is_none_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let template = Template {
            manifest: manifest(),
            root: dir.path().to_path_buf(),
        };
        assert!(load(&template).expect("loads").is_none());
    }

    #[test]
    fn skill_md_loads_as_a_conventions_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("skill.md"),
            "# Conventions\n\nUse `cargo fmt`.\n",
        )
        .expect("write");
        let template = Template {
            manifest: manifest(),
            root: dir.path().to_path_buf(),
        };
        let section = load(&template).expect("loads").expect("present");
        assert_eq!(section.kind, SectionKind::Conventions);
        assert!(section.body.contains("cargo fmt"));
        assert_eq!(section.bytes, section.body.len());
        assert!(section.tokens > 0);
        assert_eq!(section.provenance.len(), 1);
    }
}
