//! Wiki page identity and the on-disk write gate (`SPEC.md` §9, §26).
//!
//! A [`WikiPage`] is a generated page's content before it touches disk: its identity, its
//! `derived_from` basis, and its rendered markdown body (without front matter — [`WikiPage::render`]
//! adds that). Writing a page to disk always goes through [`write_page`], the one function in
//! this crate allowed to touch a file under `docs/wiki/`. It enforces the human/maintained
//! protection by reusing `tm_docs::reconcile::apply_regeneration` — the exact function `SPEC.md`
//! §9 already hard-gates to `DocMode::Generated` — rather than re-implementing the check.

use std::fs;
use std::path::Path;

use tm_docs::reconcile::apply_regeneration;
use tm_docs::registry::{
    parse_front_matter, parse_tmdocs_toml, resolve_front_matter, DocMode, DocRecord, TMDOCS_TOML,
};
use tm_types::{Clock, Result, TmError};

/// The subdirectory, relative to the project root, every generated wiki page lives under.
pub const WIKI_DIR: &str = "docs/wiki";

/// One page's content, ready to be written by [`write_page`], before front matter is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiPage {
    /// This page's front-matter `[doc] id`. Always prefixed `wiki/` by [`WikiPage::new`] so it
    /// cannot collide with a hand-declared id in `docs/.tmdocs.toml` (which uses bare names like
    /// `"spec"`) or another doc's inline front matter.
    pub id: String,
    /// Path relative to [`WIKI_DIR`], e.g. `"architecture/tm-core.md"`.
    pub rel_path: String,
    /// Rendered markdown body, not including the front-matter block.
    pub body: String,
    /// `derived_from` basis: source globs and/or decision ids this page was assembled from, in
    /// exactly the free-text shape `tm_docs::provenance::compile` expects.
    pub derived_from: Vec<String>,
}

impl WikiPage {
    /// Build a page. `id` is the family-relative id (e.g. `"architecture/tm-core"`); the stored
    /// [`WikiPage::id`] is `wiki/<id>`.
    pub fn new(
        id: impl Into<String>,
        rel_path: impl Into<String>,
        body: impl Into<String>,
        derived_from: Vec<String>,
    ) -> Self {
        WikiPage {
            id: format!("wiki/{}", id.into()),
            rel_path: rel_path.into(),
            body: body.into(),
            derived_from,
        }
    }

    /// The path relative to the project root (`docs/wiki/<rel_path>`).
    pub fn project_path(&self) -> String {
        format!("{WIKI_DIR}/{}", self.rel_path)
    }

    /// Render the full file contents: an embedded front-matter block (`SPEC.md` §9's format,
    /// `mode = "generated"`) followed by the markdown body. Every page this crate writes
    /// declares itself `Generated` so a later regeneration pass can resolve its own mode and
    /// (unless a human has since hand-edited the file and changed its front matter to
    /// `maintained`/`human`) safely overwrite it again.
    pub fn render(&self) -> String {
        let derived = self
            .derived_from
            .iter()
            .map(|d| format!("\"{}\"", d.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "+++\n[doc]\nid = \"{}\"\nmode = \"generated\"\nderived_from = [{derived}]\n+++\n\n{}\n",
            self.id,
            self.body.trim_end()
        )
    }
}

/// What happened when [`write_page`] considered one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// No file existed at this path, or one existed and declared `mode = "generated"`: written
    /// (or overwritten).
    Written,
    /// A file already exists at this path and is not provably system-writable — either it
    /// declares `mode = "human"`/`"maintained"`, or it has no resolvable doc declaration at all
    /// (see [`resolve_existing_mode`]'s doc comment for why the latter is treated as protected
    /// too). Left completely untouched.
    Skipped {
        /// Why this page was left alone (from `tm_docs::reconcile::ReconcileError`'s message, or
        /// this module's own "undeclared" message).
        reason: String,
    },
}

/// Resolve the on-disk [`DocMode`] that governs `path` (relative to `project_root`), consulting
/// both an embedded front-matter block and `docs/.tmdocs.toml`'s fallback entries — the same two
/// sources `tm_docs::registry::resolve_front_matter` expects, checked in the same order.
///
/// Returns `Ok(None)` only when no file exists at `path` yet (nothing to protect: a fresh page is
/// always writable). When a file *does* exist but neither source declares it, this returns
/// `Ok(Some(DocMode::Human))` rather than defaulting to `Generated` — an undeclared file cannot
/// be proven safe to overwrite, and refusing (the [`DocMode::Human`] gate already refuses
/// unconditionally) is the safe default, not a guess.
fn resolve_existing_mode(project_root: &Path, path: &str) -> Result<Option<DocMode>> {
    let full = project_root.join(path);
    if !full.exists() {
        return Ok(None);
    }

    let contents =
        fs::read_to_string(&full).map_err(|e| TmError::storage(format!("reading {path}: {e}")))?;
    let inline = parse_front_matter(&contents)?;

    let tmdocs_path = project_root.join("docs").join(TMDOCS_TOML);
    let fallback_entry = if tmdocs_path.exists() {
        let toml_contents = fs::read_to_string(&tmdocs_path)
            .map_err(|e| TmError::storage(format!("reading {TMDOCS_TOML}: {e}")))?;
        let parsed = parse_tmdocs_toml(&toml_contents)?;
        parsed.docs.into_iter().find(|d| d.path == path)
    } else {
        None
    };

    match resolve_front_matter(path, inline, fallback_entry.as_ref()) {
        Ok(fm) => Ok(Some(fm.mode)),
        Err(_) => Ok(Some(DocMode::Human)),
    }
}

/// Write `page` to disk under `project_root`, unless a pre-existing file at its path is not
/// system-writable — in which case it is left byte-for-byte untouched and [`WriteOutcome::Skipped`]
/// is returned instead of an error, since "a human owns this page" is an expected, routine
/// outcome of regeneration, not a failure.
pub fn write_page(project_root: &Path, page: &WikiPage, clock: &dyn Clock) -> Result<WriteOutcome> {
    let path = page.project_path();
    let existing_mode = resolve_existing_mode(project_root, &path)?;
    // No file yet: always `Generated` (nothing to protect). Otherwise: whatever mode the file
    // itself (or `.tmdocs.toml`) declares.
    let gate_mode = existing_mode.unwrap_or(DocMode::Generated);

    let mut doc = DocRecord::new(
        page.id.clone(),
        path.clone(),
        gate_mode,
        page.derived_from.clone(),
    );
    let content = page.render();
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();

    match apply_regeneration(&mut doc, &hash, clock.now()) {
        Ok(()) => {
            let full = project_root.join(&path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| TmError::storage(format!("creating {}: {e}", parent.display())))?;
            }
            fs::write(&full, content)
                .map_err(|e| TmError::storage(format!("writing {path}: {e}")))?;
            Ok(WriteOutcome::Written)
        }
        Err(TmError::AuthorityDenied(msg)) => Ok(WriteOutcome::Skipped { reason: msg }),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tm_types::FixedClock;

    #[test]
    fn render_includes_front_matter_and_body() {
        let page = WikiPage::new(
            "architecture/tm-core",
            "architecture/tm-core.md",
            "# Architecture: tm-core\n",
            vec!["crates/tm-core/src/**".to_string()],
        );
        let rendered = page.render();
        assert!(rendered.starts_with("+++\n[doc]\n"));
        assert!(rendered.contains("id = \"wiki/architecture/tm-core\""));
        assert!(rendered.contains("mode = \"generated\""));
        assert!(rendered.contains("derived_from = [\"crates/tm-core/src/**\"]"));
        assert!(rendered.contains("# Architecture: tm-core"));
    }

    #[test]
    fn rendered_front_matter_round_trips_through_parse_front_matter() {
        let page = WikiPage::new("glossary", "glossary.md", "# Glossary\n", vec![]);
        let rendered = page.render();
        let parsed = parse_front_matter(&rendered).unwrap().unwrap();
        assert_eq!(parsed.id, "wiki/glossary");
        assert_eq!(parsed.mode, DocMode::Generated);
    }

    #[test]
    fn write_page_creates_new_page() {
        let dir = TempDir::new().unwrap();
        let clock = FixedClock::epoch();
        let page = WikiPage::new("glossary", "glossary.md", "# Glossary\n", vec![]);

        let outcome = write_page(dir.path(), &page, &clock).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        let written = fs::read_to_string(dir.path().join("docs/wiki/glossary.md")).unwrap();
        assert!(written.contains("# Glossary"));
    }

    #[test]
    fn write_page_overwrites_its_own_prior_generated_output() {
        let dir = TempDir::new().unwrap();
        let clock = FixedClock::epoch();
        let page_v1 = WikiPage::new("glossary", "glossary.md", "# Glossary v1\n", vec![]);
        write_page(dir.path(), &page_v1, &clock).unwrap();

        let page_v2 = WikiPage::new("glossary", "glossary.md", "# Glossary v2\n", vec![]);
        let outcome = write_page(dir.path(), &page_v2, &clock).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        let written = fs::read_to_string(dir.path().join("docs/wiki/glossary.md")).unwrap();
        assert!(written.contains("# Glossary v2"));
        assert!(!written.contains("v1"));
    }

    #[test]
    fn write_page_never_overwrites_a_human_mode_page() {
        let dir = TempDir::new().unwrap();
        let clock = FixedClock::epoch();
        let path = dir.path().join("docs/wiki/glossary.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let human_content = "+++\n[doc]\nid = \"wiki/glossary\"\nmode = \"human\"\n+++\n\n# A human wrote this glossary by hand.\n";
        fs::write(&path, human_content).unwrap();

        let page = WikiPage::new(
            "glossary",
            "glossary.md",
            "# Regenerated glossary\n",
            vec![],
        );
        let outcome = write_page(dir.path(), &page, &clock).unwrap();

        match outcome {
            WriteOutcome::Skipped { reason } => {
                assert!(reason.contains("wiki/glossary"));
                assert!(reason.contains("Human"));
            }
            WriteOutcome::Written => panic!("human-mode page must never be overwritten"),
        }

        let unchanged = fs::read_to_string(&path).unwrap();
        assert_eq!(unchanged, human_content, "byte-for-byte untouched");
    }

    #[test]
    fn write_page_never_overwrites_a_maintained_mode_page() {
        let dir = TempDir::new().unwrap();
        let clock = FixedClock::epoch();
        let path = dir.path().join("docs/wiki/glossary.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let maintained_content =
            "+++\n[doc]\nid = \"wiki/glossary\"\nmode = \"maintained\"\n+++\n\nHand-curated.\n";
        fs::write(&path, maintained_content).unwrap();

        let page = WikiPage::new("glossary", "glossary.md", "# Regenerated\n", vec![]);
        let outcome = write_page(dir.path(), &page, &clock).unwrap();

        assert!(matches!(outcome, WriteOutcome::Skipped { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), maintained_content);
    }

    #[test]
    fn write_page_never_overwrites_an_undeclared_existing_file() {
        let dir = TempDir::new().unwrap();
        let clock = FixedClock::epoch();
        let path = dir.path().join("docs/wiki/glossary.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let plain_content = "Just some markdown someone dropped here, no front matter.\n";
        fs::write(&path, plain_content).unwrap();

        let page = WikiPage::new("glossary", "glossary.md", "# Regenerated\n", vec![]);
        let outcome = write_page(dir.path(), &page, &clock).unwrap();

        assert!(matches!(outcome, WriteOutcome::Skipped { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), plain_content);
    }

    #[test]
    fn write_page_honours_tmdocs_toml_fallback_declaration() {
        let dir = TempDir::new().unwrap();
        let clock = FixedClock::epoch();
        fs::create_dir_all(dir.path().join("docs/wiki")).unwrap();
        fs::write(
            dir.path().join("docs/wiki/glossary.md"),
            "No inline front matter, declared via .tmdocs.toml instead.\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("docs/.tmdocs.toml"),
            "[[doc]]\npath = \"docs/wiki/glossary.md\"\nid = \"glossary\"\nmode = \"human\"\n",
        )
        .unwrap();

        let page = WikiPage::new("glossary", "glossary.md", "# Regenerated\n", vec![]);
        let outcome = write_page(dir.path(), &page, &clock).unwrap();

        assert!(matches!(outcome, WriteOutcome::Skipped { .. }));
        assert_eq!(
            fs::read_to_string(dir.path().join("docs/wiki/glossary.md")).unwrap(),
            "No inline front matter, declared via .tmdocs.toml instead.\n"
        );
    }
}
