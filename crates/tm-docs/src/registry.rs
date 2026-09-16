//! Doc discovery and parsing (`SPEC.md` §9).
//!
//! A doc declares its basis in one of two places: an embedded front-matter block in the
//! markdown file itself, or a fallback entry in `docs/.tmdocs.toml` for docs that cannot or
//! should not carry inline metadata. Despite the common name "front matter", the block's
//! *syntax* is TOML (`[doc]` with `id`/`mode`/`derived_from` keys, exactly as `SPEC.md` §9
//! shows it) rather than YAML — `serde_yaml` is not a workspace dependency, `toml` is, and the
//! two front-matter sources share one wire format this way. This module only parses text; it
//! never touches the filesystem itself (the caller reads `fs::read_to_string` and hands this
//! module the contents), so every function here is unit-testable against string fixtures.
//!
//! [`DocRegistry`] is the in-memory catalogue those parsed records land in: keyed by doc id,
//! insert-or-replace, and the listing surface `tm docs list` reads from.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tm_types::Timestamp;

/// The delimiter line that opens and closes an embedded front-matter block, matching either of
/// the two common conventions (`+++` a la Zola/TOML, `---` a la Jekyll/YAML front matter,
/// accepted here even though the block content itself is always parsed as TOML).
pub const FRONT_MATTER_DELIMITERS: [&str; 2] = ["+++", "---"];

/// The filename, relative to `docs/`, of the front-matter fallback for docs that do not embed
/// their own `[doc]` block.
pub const TMDOCS_TOML: &str = ".tmdocs.toml";

/// Who — or what — is responsible for a doc's content, and therefore how it may be reconciled.
///
/// This is the load-bearing distinction `SPEC.md` §9 enforces by test: `Generated` docs may be
/// rewritten by the system; `Maintained` and `Human` docs never are (see [`crate::reconcile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocMode {
    /// Produced entirely by tooling from its `derived_from` sources; safe to regenerate.
    Generated,
    /// Hand-edited but expected to track its `derived_from` sources; only a human may edit it,
    /// but the system may flag it and open a review ticket.
    Maintained,
    /// Entirely human-owned prose (e.g. a vision doc); the system may only ever flag it.
    Human,
}

/// A doc's current standing with respect to its declared basis.
///
/// `Reconciling` and `Unverified` are distinct: `Reconciling` means a reconciliation ticket is
/// open against a `Stale` doc; `Unverified` means the doc was just discovered (or a dismissal
/// was accepted) and has not yet been checked against the live basis at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocState {
    /// The doc's basis has not changed since it was last verified.
    Fresh,
    /// A change touched the doc's `derived_from` basis and it has not yet been reconciled.
    Stale,
    /// A regeneration or review ticket is open for this doc.
    Reconciling,
    /// Newly registered or dismissed; standing with respect to the current basis is unknown.
    Unverified,
}

/// A doc's `derived_from` basis and identity, parsed from an embedded front-matter block.
///
/// This is the raw parsed shape, before [`DocRecord::new`] adds the path (known only to the
/// caller doing the file walk) and initial state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DocFrontMatter {
    /// The doc's stable id (e.g. `"architecture"`), unique within a project.
    pub id: String,
    /// [`DocMode`].
    pub mode: DocMode,
    /// Source globs, [`tm_types::DecisionId`]s and config file paths this doc was derived from,
    /// exactly as written (uncompiled — see [`crate::provenance`]).
    #[serde(default)]
    pub derived_from: Vec<String>,
}

/// The `[doc]`-table wrapper `toml` needs to deserialize an embedded front-matter block, whose
/// contents are exactly `SPEC.md` §9's example (`[doc]\nid = "..."\n...`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct FrontMatterFile {
    doc: DocFrontMatter,
}

/// One entry in `docs/.tmdocs.toml`: a [`DocFrontMatter`] plus the path it applies to, for docs
/// that do not embed their own front-matter block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DocTomlEntry {
    /// Path to the doc file, relative to the project root.
    pub path: String,
    /// The doc's stable id.
    pub id: String,
    /// [`DocMode`].
    pub mode: DocMode,
    /// Source globs, decision ids and config file paths, as written.
    #[serde(default)]
    pub derived_from: Vec<String>,
}

/// The parsed contents of `docs/.tmdocs.toml`: `[[doc]]` array-of-tables, one per fallback doc.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct TmDocsToml {
    /// Every fallback entry, in file order.
    #[serde(rename = "doc", default)]
    pub docs: Vec<DocTomlEntry>,
}

/// Parse the embedded front-matter block from a markdown file's contents, if it has one.
///
/// Returns `Ok(None)` when the file has no front-matter block at all — callers fall back to
/// `docs/.tmdocs.toml` in that case. Returns `Err` only when a block is present but malformed.
pub fn parse_front_matter(markdown: &str) -> tm_types::Result<Option<DocFrontMatter>> {
    let mut lines = markdown.lines();

    // Get first line and check if it's a blank/BOM line to skip
    let mut current_line = lines.next();

    // Skip one optional leading blank/UTF-8-BOM line
    if let Some(line) = current_line {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed == "\u{FEFF}" {
            current_line = lines.next();
        }
    } else {
        // Empty file
        return Ok(None);
    }

    // Now current_line should be the potential opening delimiter
    let opening_delimiter = if let Some(line) = current_line {
        let trimmed = line.trim_end();
        if FRONT_MATTER_DELIMITERS.contains(&trimmed) {
            trimmed
        } else {
            // No front matter block
            return Ok(None);
        }
    } else {
        return Ok(None);
    };

    // Collect lines until we find the closing delimiter
    let mut content = String::new();
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed == opening_delimiter {
            // Found closing delimiter, parse the content
            let front_matter: FrontMatterFile = toml::from_str(&content)
                .map_err(|e| tm_types::TmError::parse(format!("front matter: {e}")))?;
            return Ok(Some(front_matter.doc));
        }
        content.push_str(line);
        content.push('\n');
    }

    // If we get here, we didn't find a closing delimiter
    Err(tm_types::TmError::parse("unterminated front-matter block"))
}

/// Parse a `docs/.tmdocs.toml` file's contents.
pub fn parse_tmdocs_toml(contents: &str) -> tm_types::Result<TmDocsToml> {
    toml::from_str::<TmDocsToml>(contents)
        .map_err(|e| tm_types::TmError::parse(format!(".tmdocs.toml: {e}")))
}

/// Resolve a doc's front matter from the two possible sources: an embedded block, and a
/// `docs/.tmdocs.toml` entry keyed by `path`. Inline front matter always wins when both exist.
pub fn resolve_front_matter(
    path: &str,
    inline: Option<DocFrontMatter>,
    fallback: Option<&DocTomlEntry>,
) -> tm_types::Result<DocFrontMatter> {
    if let Some(inline_fm) = inline {
        if let Some(fallback_entry) = fallback {
            if inline_fm.id != fallback_entry.id || inline_fm.mode != fallback_entry.mode {
                tracing::warn!(
                    path = path,
                    inline_id = %inline_fm.id,
                    inline_mode = ?inline_fm.mode,
                    fallback_id = %fallback_entry.id,
                    fallback_mode = ?fallback_entry.mode,
                    "front matter mismatch between inline and .tmdocs.toml"
                );
            }
        }
        Ok(inline_fm)
    } else if let Some(fallback_entry) = fallback {
        Ok(DocFrontMatter {
            id: fallback_entry.id.clone(),
            mode: fallback_entry.mode,
            derived_from: fallback_entry.derived_from.clone(),
        })
    } else {
        Err(tm_types::TmError::not_found("doc", path))
    }
}

/// One doc, as tracked by the project: identity, basis, and current staleness standing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRecord {
    /// The doc's stable id, unique within a project.
    pub id: String,
    /// Path to the doc file, relative to the project root.
    pub path: String,
    /// [`DocMode`].
    pub mode: DocMode,
    /// Current [`DocState`].
    pub state: DocState,
    /// Source globs, decision ids and config file paths this doc was derived from.
    pub derived_from: Vec<String>,
    /// When a human (or, for `Generated` docs, the regeneration step) last verified this doc
    /// matched its basis. `None` for a doc that has never been verified.
    pub last_verified: Option<Timestamp>,
}

impl DocRecord {
    /// Build a freshly discovered doc record: [`DocState::Unverified`], no verification history.
    pub fn new(id: String, path: String, mode: DocMode, derived_from: Vec<String>) -> Self {
        DocRecord {
            id,
            path,
            mode,
            state: DocState::Unverified,
            derived_from,
            last_verified: None,
        }
    }

    /// True when this doc is [`DocMode::Human`] — the system may never rewrite its content.
    pub fn is_human(&self) -> bool {
        matches!(self.mode, DocMode::Human)
    }

    /// True when this doc is [`DocMode::Generated`] — the system may regenerate its content.
    pub fn is_generated(&self) -> bool {
        matches!(self.mode, DocMode::Generated)
    }
}

/// The in-memory catalogue of every doc the project knows about, keyed by doc id.
#[derive(Debug, Clone, Default)]
pub struct DocRegistry {
    docs: BTreeMap<String, DocRecord>,
}

impl DocRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        DocRegistry {
            docs: BTreeMap::new(),
        }
    }

    /// Insert or replace a doc record, keyed by [`DocRecord::id`]. Returns the previous record,
    /// if any (the caller decides whether that is a re-registration or an error).
    pub fn insert(&mut self, record: DocRecord) -> Option<DocRecord> {
        self.docs.insert(record.id.clone(), record)
    }

    /// Look up a doc by id.
    pub fn get(&self, id: &str) -> Option<&DocRecord> {
        self.docs.get(id)
    }

    /// Look up a doc by id, mutably — the entry point [`crate::assess::Assessor`] and
    /// [`crate::reconcile`] use to transition a doc's [`DocState`].
    pub fn get_mut(&mut self, id: &str) -> Option<&mut DocRecord> {
        self.docs.get_mut(id)
    }

    /// Every registered doc, in id order. What `tm docs list` reads from.
    pub fn list(&self) -> Vec<&DocRecord> {
        self.docs.values().collect()
    }

    /// Number of registered docs.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// True when no docs are registered.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

/// Build the `doc.registered` payloads for docs discovered by a fresh walk that the registry did
/// not already know about. Does not mutate `existing` — the caller inserts the newly discovered
/// records afterward (typically via [`DocRegistry::insert`]) in the same transaction that
/// appends these events.
pub fn registration_events(
    existing: &DocRegistry,
    discovered: &[DocRecord],
) -> Vec<tm_events::payload::DocRegisteredPayload> {
    discovered
        .iter()
        .filter(|record| existing.get(&record.id).is_none())
        .map(|record| tm_events::payload::DocRegisteredPayload {
            path: record.path.clone(),
            ticket: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_front_matter_valid_toml_delimiter() {
        let markdown = r#"+++
[doc]
id = "architecture"
mode = "generated"
derived_from = ["README.md"]
+++

# Content here
"#;
        let result = parse_front_matter(markdown).unwrap();
        assert!(result.is_some());
        let fm = result.unwrap();
        assert_eq!(fm.id, "architecture");
        assert_eq!(fm.mode, DocMode::Generated);
        assert_eq!(fm.derived_from, vec!["README.md"]);
    }

    #[test]
    fn parse_front_matter_valid_yaml_delimiter() {
        let markdown = r#"---
[doc]
id = "design"
mode = "maintained"
derived_from = ["src/"]
---

Content
"#;
        let result = parse_front_matter(markdown).unwrap();
        assert!(result.is_some());
        let fm = result.unwrap();
        assert_eq!(fm.id, "design");
        assert_eq!(fm.mode, DocMode::Maintained);
        assert_eq!(fm.derived_from, vec!["src/"]);
    }

    #[test]
    fn parse_front_matter_with_leading_blank_line() {
        let markdown = r#"
+++
[doc]
id = "vision"
mode = "human"
+++

Content
"#;
        let result = parse_front_matter(markdown).unwrap();
        assert!(result.is_some());
        let fm = result.unwrap();
        assert_eq!(fm.id, "vision");
        assert_eq!(fm.mode, DocMode::Human);
    }

    #[test]
    fn parse_front_matter_with_empty_derived_from() {
        let markdown = r#"+++
[doc]
id = "test"
mode = "generated"
+++

Content
"#;
        let result = parse_front_matter(markdown).unwrap();
        assert!(result.is_some());
        let fm = result.unwrap();
        assert_eq!(fm.derived_from, Vec::<String>::new());
    }

    #[test]
    fn parse_front_matter_no_front_matter() {
        let markdown = "# Just markdown\n\nNo front matter here";
        let result = parse_front_matter(markdown).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parse_front_matter_empty_file() {
        let result = parse_front_matter("").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parse_front_matter_only_whitespace() {
        let result = parse_front_matter("   \n  \n  ").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parse_front_matter_unterminated_block() {
        let markdown = r#"+++
id = "unterminated"
mode = "generated"
# Missing closing delimiter
"#;
        let result = parse_front_matter(markdown);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("unterminated"));
    }

    #[test]
    fn parse_front_matter_malformed_toml() {
        let markdown = r#"+++
id = "broken"
this is not valid toml [[ [ [
+++
"#;
        let result = parse_front_matter(markdown);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("front matter"));
    }

    #[test]
    fn parse_front_matter_missing_required_field() {
        let markdown = r#"+++
mode = "generated"
+++
"#;
        let result = parse_front_matter(markdown);
        assert!(result.is_err());
    }

    #[test]
    fn parse_front_matter_content_with_empty_lines() {
        let markdown = r#"+++
[doc]
id = "doc_id"
mode = "generated"

derived_from = ["source"]
+++

Content
"#;
        let result = parse_front_matter(markdown).unwrap();
        assert!(result.is_some());
        let fm = result.unwrap();
        assert_eq!(fm.id, "doc_id");
        assert_eq!(fm.derived_from, vec!["source"]);
    }

    #[test]
    fn parse_tmdocs_toml_valid() {
        let toml_content = r#"[[doc]]
path = "docs/arch.md"
id = "architecture"
mode = "maintained"
derived_from = ["src/core/"]

[[doc]]
path = "docs/design.md"
id = "design"
mode = "generated"
derived_from = ["spec.md"]
"#;
        let result = parse_tmdocs_toml(toml_content).unwrap();
        assert_eq!(result.docs.len(), 2);
        assert_eq!(result.docs[0].id, "architecture");
        assert_eq!(result.docs[0].path, "docs/arch.md");
        assert_eq!(result.docs[1].id, "design");
    }

    #[test]
    fn parse_tmdocs_toml_empty() {
        let result = parse_tmdocs_toml("").unwrap();
        assert!(result.docs.is_empty());
    }

    #[test]
    fn parse_tmdocs_toml_malformed() {
        let invalid = "[[doc]]\ninvalid toml [[ ]]";
        let result = parse_tmdocs_toml(invalid);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains(".tmdocs.toml"));
    }

    #[test]
    fn parse_tmdocs_toml_with_default_derived_from() {
        let toml_content = r#"[[doc]]
path = "docs/simple.md"
id = "simple"
mode = "human"
"#;
        let result = parse_tmdocs_toml(toml_content).unwrap();
        assert_eq!(result.docs[0].derived_from, Vec::<String>::new());
    }

    #[test]
    fn resolve_front_matter_inline_only() {
        let inline = Some(DocFrontMatter {
            id: "inline_id".to_string(),
            mode: DocMode::Generated,
            derived_from: vec!["source.txt".to_string()],
        });
        let result = resolve_front_matter("path/to/doc.md", inline, None).unwrap();
        assert_eq!(result.id, "inline_id");
        assert_eq!(result.mode, DocMode::Generated);
    }

    #[test]
    fn resolve_front_matter_fallback_only() {
        let fallback = Some(DocTomlEntry {
            path: "docs/fallback.md".to_string(),
            id: "fallback_id".to_string(),
            mode: DocMode::Maintained,
            derived_from: vec!["config.toml".to_string()],
        });
        let result = resolve_front_matter("docs/fallback.md", None, fallback.as_ref()).unwrap();
        assert_eq!(result.id, "fallback_id");
        assert_eq!(result.mode, DocMode::Maintained);
        assert_eq!(result.derived_from, vec!["config.toml"]);
    }

    #[test]
    fn resolve_front_matter_inline_wins_over_fallback() {
        let inline = Some(DocFrontMatter {
            id: "inline_id".to_string(),
            mode: DocMode::Generated,
            derived_from: vec!["inline_source".to_string()],
        });
        let fallback = Some(DocTomlEntry {
            path: "docs/doc.md".to_string(),
            id: "fallback_id".to_string(),
            mode: DocMode::Human,
            derived_from: vec!["fallback_source".to_string()],
        });
        let result = resolve_front_matter("docs/doc.md", inline, fallback.as_ref()).unwrap();
        assert_eq!(result.id, "inline_id");
        assert_eq!(result.mode, DocMode::Generated);
        assert_eq!(result.derived_from, vec!["inline_source"]);
    }

    #[test]
    fn resolve_front_matter_neither_source_errors() {
        let result = resolve_front_matter("docs/missing.md", None, None);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("missing.md") || err_msg.contains("not found"));
    }

    #[test]
    fn resolve_front_matter_matching_inline_and_fallback() {
        let inline = Some(DocFrontMatter {
            id: "same_id".to_string(),
            mode: DocMode::Maintained,
            derived_from: vec![],
        });
        let fallback = Some(DocTomlEntry {
            path: "docs/doc.md".to_string(),
            id: "same_id".to_string(),
            mode: DocMode::Maintained,
            derived_from: vec![],
        });
        let result = resolve_front_matter("docs/doc.md", inline, fallback.as_ref()).unwrap();
        assert_eq!(result.id, "same_id");
        assert_eq!(result.mode, DocMode::Maintained);
    }

    #[test]
    fn registration_events_new_docs_only() {
        let mut existing = DocRegistry::new();
        existing.insert(DocRecord::new(
            "existing".to_string(),
            "docs/existing.md".to_string(),
            DocMode::Generated,
            vec![],
        ));

        let discovered = vec![
            DocRecord::new(
                "new1".to_string(),
                "docs/new1.md".to_string(),
                DocMode::Generated,
                vec![],
            ),
            DocRecord::new(
                "new2".to_string(),
                "docs/new2.md".to_string(),
                DocMode::Maintained,
                vec![],
            ),
        ];

        let events = registration_events(&existing, &discovered);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, "docs/new1.md");
        assert_eq!(events[1].path, "docs/new2.md");
    }

    #[test]
    fn registration_events_no_new_docs() {
        let mut existing = DocRegistry::new();
        existing.insert(DocRecord::new(
            "existing".to_string(),
            "docs/existing.md".to_string(),
            DocMode::Generated,
            vec![],
        ));

        let discovered = vec![DocRecord::new(
            "existing".to_string(),
            "docs/existing.md".to_string(),
            DocMode::Generated,
            vec![],
        )];

        let events = registration_events(&existing, &discovered);
        assert!(events.is_empty());
    }

    #[test]
    fn registration_events_empty_discovered() {
        let existing = DocRegistry::new();
        let discovered = vec![];

        let events = registration_events(&existing, &discovered);
        assert!(events.is_empty());
    }

    #[test]
    fn registration_events_empty_existing() {
        let existing = DocRegistry::new();
        let discovered = vec![DocRecord::new(
            "doc1".to_string(),
            "docs/doc1.md".to_string(),
            DocMode::Human,
            vec![],
        )];

        let events = registration_events(&existing, &discovered);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "docs/doc1.md");
        assert!(events[0].ticket.is_none());
    }

    #[test]
    fn registration_events_maintains_order() {
        let existing = DocRegistry::new();
        let discovered = vec![
            DocRecord::new(
                "z_doc".to_string(),
                "docs/z.md".to_string(),
                DocMode::Generated,
                vec![],
            ),
            DocRecord::new(
                "a_doc".to_string(),
                "docs/a.md".to_string(),
                DocMode::Maintained,
                vec![],
            ),
            DocRecord::new(
                "m_doc".to_string(),
                "docs/m.md".to_string(),
                DocMode::Human,
                vec![],
            ),
        ];

        let events = registration_events(&existing, &discovered);
        assert_eq!(events.len(), 3);
        // Order should match input order, not sorted order
        assert_eq!(events[0].path, "docs/z.md");
        assert_eq!(events[1].path, "docs/a.md");
        assert_eq!(events[2].path, "docs/m.md");
    }

    #[test]
    fn registration_events_mixed_new_and_existing() {
        let mut existing = DocRegistry::new();
        existing.insert(DocRecord::new(
            "existing1".to_string(),
            "docs/existing1.md".to_string(),
            DocMode::Generated,
            vec![],
        ));
        existing.insert(DocRecord::new(
            "existing2".to_string(),
            "docs/existing2.md".to_string(),
            DocMode::Maintained,
            vec![],
        ));

        let discovered = vec![
            DocRecord::new(
                "new1".to_string(),
                "docs/new1.md".to_string(),
                DocMode::Generated,
                vec![],
            ),
            DocRecord::new(
                "existing1".to_string(),
                "docs/existing1.md".to_string(),
                DocMode::Generated,
                vec![],
            ),
            DocRecord::new(
                "new2".to_string(),
                "docs/new2.md".to_string(),
                DocMode::Human,
                vec![],
            ),
            DocRecord::new(
                "existing2".to_string(),
                "docs/existing2.md".to_string(),
                DocMode::Maintained,
                vec![],
            ),
        ];

        let events = registration_events(&existing, &discovered);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, "docs/new1.md");
        assert_eq!(events[1].path, "docs/new2.md");
    }
}
