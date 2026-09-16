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
// IMPL: find the first line that is exactly one of [`FRONT_MATTER_DELIMITERS`] (after trimming
// trailing whitespace); if the file doesn't start with one (allowing a leading blank/UTF-8-BOM
// line), return `Ok(None)`. Find the next line that is the *same* delimiter; if there is none,
// `Err(TmError::parse("unterminated front-matter block"))`. Feed the text strictly between the
// two delimiter lines to `toml::from_str::<FrontMatterFile>`, mapping a `toml::de::Error` to
// `TmError::parse(format!("front matter: {e}"))`. Return `Ok(Some(parsed.doc))`.
pub fn parse_front_matter(markdown: &str) -> tm_types::Result<Option<DocFrontMatter>> {
    todo!("parse and extract the [doc] front-matter block, see IMPL note above")
}

/// Parse a `docs/.tmdocs.toml` file's contents.
// IMPL: `toml::from_str::<TmDocsToml>(contents)`, mapping `toml::de::Error` to
// `TmError::parse(format!(".tmdocs.toml: {e}"))`. No other validation here — duplicate ids and
// dangling paths are a `DocRegistry`/caller concern, not a parse concern.
pub fn parse_tmdocs_toml(contents: &str) -> tm_types::Result<TmDocsToml> {
    todo!("parse docs/.tmdocs.toml, see IMPL note above")
}

/// Resolve a doc's front matter from the two possible sources: an embedded block, and a
/// `docs/.tmdocs.toml` entry keyed by `path`. Inline front matter always wins when both exist.
// IMPL: if `inline` is `Some`, return it unchanged (log via `tracing::warn!` if `fallback` is
// also `Some` and disagrees on `id`/`mode`, since that is almost certainly an authoring mistake,
// but it is not itself an error). If `inline` is `None`, build a [`DocFrontMatter`] from
// `fallback` (`Err(TmError::not_found("doc", path))` if neither source has an entry for `path`).
pub fn resolve_front_matter(
    path: &str,
    inline: Option<DocFrontMatter>,
    fallback: Option<&DocTomlEntry>,
) -> tm_types::Result<DocFrontMatter> {
    todo!("merge inline front matter with the .tmdocs.toml fallback, see IMPL note above")
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
// IMPL: for each record in `discovered` whose `id` is absent from `existing`, emit
// `tm_events::payload::DocRegisteredPayload { path: record.path.clone(), ticket: None }`.
// Iterate `discovered` in the order given (callers should pass a deterministically sorted slice,
// e.g. by path) rather than re-sorting here, so event order matches file-walk order.
pub fn registration_events(
    existing: &DocRegistry,
    discovered: &[DocRecord],
) -> Vec<tm_events::payload::DocRegisteredPayload> {
    todo!("diff discovered docs against the existing registry, see IMPL note above")
}
