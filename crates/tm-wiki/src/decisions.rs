//! `decisions/` pages (`SPEC.md` §26.2): one page per decision, rendering its full supersession
//! chain, plus a `decisions.md` index. Assembled entirely by reading `docs/decisions/D-NNN-*.md`
//! directly under the project root — this repo's one real, exclusively-used decision-doc
//! convention (`CLAUDE.md`: "A new/changed decision gets a `docs/decisions/D-NNN-*.md`"), not
//! `Store::view()`'s `decisions` map. Nothing in this repo calls `tm decision new` to populate
//! that map for an architecture decision record — doing so would require a real `.tm/` project in
//! this repo's own checkout, which this repo's own dispatch-agent playbook treats as a
//! contamination incident to avoid, not a normal workflow step. `tm_core::Decision`/`DecisionId`
//! and `tm decision new`/`tm decision supersede` still exist and still work; they are just no
//! longer this page family's source (see `docs/backlog.md`'s resolved "`docs/decisions/*.md` vs.
//! `tm-wiki`'s `Decision` model" entry for the full reasoning and what this trade costs).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use tm_types::{Result, TmError};

use crate::page::WikiPage;

/// One real `docs/decisions/D-NNN-*.md` file's parsed identity and metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DecisionDoc {
    /// `D-NNN`, parsed from the file name (not the title line — the file name is what every other
    /// cross-reference in this workspace, including `xtask`'s own dangling-`D-NNN` hygiene check,
    /// already treats as authoritative).
    id: String,
    /// The file name without its `.md` extension, e.g. `"D-003-project-scope"` — this page's own
    /// identity, and every other page's link target when it points at this decision.
    file_stem: String,
    /// The text after the title line's em dash, e.g. `"Project scope: workspace vs. state, and
    /// where bare \`tm\` writes"`.
    title: String,
    /// The `**Status:**` field's raw value, rendered verbatim (this repo's docs use `accepted`
    /// today; nothing here assumes a closed set of status strings).
    status: String,
    /// The `**Date:**` field's raw value, rendered verbatim.
    date: String,
    /// The decision this one supersedes, if its `**Supersedes:**` field names one — `None` for a
    /// literal `nothing` value, for free text that isn't itself a `D-NNN` reference (e.g. D-001's
    /// "the implicit assumption in SPEC.md §11 that ..."), and for a `nothing (extends D-NNN)`-
    /// shaped aside (this repo's real D-006) that mentions another decision without actually
    /// superseding it.
    supersedes: Option<String>,
}

impl DecisionDoc {
    /// This doc's real source path, relative to the project root — used as the `derived_from`
    /// basis so a page goes stale exactly when its own source file (or another file in its chain)
    /// changes, the same "derived_from is a real path" contract every other page family in this
    /// crate honours (see `architecture::pages`, `history::pages`).
    fn source_path(&self) -> String {
        format!("docs/decisions/{}.md", self.file_stem)
    }
}

/// `D-NNN`, from the start of a decision doc's file name — `None` for a file that doesn't look
/// like a decision doc at all (wrong prefix, or not exactly 3 digits). Deliberately permissive
/// about what comes after the number (the slug varies); this only needs the number to be real,
/// matching `xtask::hygiene`'s own `existing_decision_numbers` convention for the same file set.
fn decision_id_from_file_name(file_name: &str) -> Option<String> {
    let rest = file_name.strip_prefix("D-")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() == 3 {
        Some(format!("D-{digits}"))
    } else {
        None
    }
}

/// Extract a `**<label>:**` field's value from `region`, whether it is the only bold field on its
/// own bullet line (`D-001`'s format: `- **Status:** accepted`) or one of several on one inline
/// line separated by `·` (every other existing doc's format:
/// `**Status:** accepted · **Date:** ... · **Supersedes:** ...`). The value runs from just after
/// the marker to whichever comes first: the next `**` (another bold field starting, inline
/// format) or the end of the line (bulleted format) — then trims a leftover ` · ` separator, if
/// any, off the end.
fn extract_field(region: &str, label: &str) -> Option<String> {
    let marker = format!("**{label}:**");
    let start = region.find(&marker)? + marker.len();
    let rest = &region[start..];
    let end = [rest.find("**"), rest.find('\n')]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    let value = rest[..end].trim().trim_end_matches('·').trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Parse a `**Supersedes:**` field's raw value into a real chain link, or `None`. Only a value
/// that literally starts with `D-<digits>` counts as a real supersession — `"nothing"` (this
/// repo's convention for "supersedes nothing") and free text that merely *mentions* a `D-NNN`
/// later in the sentence (`"nothing (extends D-002)"`, this repo's real D-006) both parse as
/// `None`, matching what these docs actually mean: D-006 does not supersede D-002, it just says so
/// in passing.
fn parse_supersedes_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.to_ascii_lowercase().starts_with("nothing") {
        return None;
    }
    let rest = trimmed.strip_prefix("D-")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    Some(format!("D-{n:03}"))
}

/// Parse one decision doc's title and metadata out of its raw file contents. Returns `None` for a
/// file that doesn't have what a decision doc needs — no `# ` title line, or no parseable
/// `**Status:**`/`**Date:**` field — rather than erroring the whole generation run over one
/// malformed file, the same "doesn't have what we need, move on" convention `history::pages`
/// already uses for a path with no git history. The cost of that choice is real and stated
/// plainly in this repo's own decision doc for this design: a typo'd metadata line silently drops
/// that one decision from the wiki with no warning, rather than failing loudly.
fn parse_decision_doc(file_name: &str, contents: &str) -> Option<DecisionDoc> {
    let id = decision_id_from_file_name(file_name)?;
    let file_stem = file_name.strip_suffix(".md")?.to_string();

    let title_line = contents.lines().find(|l| l.starts_with("# "))?;
    let title = title_line
        .trim_start_matches("# ")
        .split_once('—')
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_else(|| title_line.trim_start_matches("# ").trim().to_string());
    if title.is_empty() {
        return None;
    }

    // The metadata block: every line after the title, up to (not including) the first `## `
    // section heading. Bounded this way, rather than scanning the whole file, so a `**Status:**`-
    // shaped string appearing later in a decision's prose could never be misread as its own
    // header.
    let after_title = contents.split_once(title_line)?.1;
    let meta_region: String = after_title
        .lines()
        .take_while(|l| !l.starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");

    let status = extract_field(&meta_region, "Status")?;
    let date = extract_field(&meta_region, "Date")?;
    let supersedes = extract_field(&meta_region, "Supersedes")
        .as_deref()
        .and_then(parse_supersedes_value);

    Some(DecisionDoc {
        id,
        file_stem,
        title,
        status,
        date,
        supersedes,
    })
}

/// Load every real decision doc under `decisions_dir`, sorted by id. A missing directory yields
/// an empty list, not an error (a project need not have any decisions recorded yet); a `.md` file
/// whose name doesn't start `D-<3 digits>` is silently not a decision doc (`docs/decisions/` could
/// hold other reference material); a file that matches the naming convention but fails to parse is
/// silently skipped too (see [`parse_decision_doc`]'s doc comment).
fn load_decisions(decisions_dir: &Path) -> Result<Vec<DecisionDoc>> {
    let entries = match fs::read_dir(decisions_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(TmError::storage(format!(
                "reading {}: {e}",
                decisions_dir.display()
            )))
        }
    };

    let mut out = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|e| TmError::storage(format!("reading {}: {e}", decisions_dir.display())))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let contents = fs::read_to_string(&path)
            .map_err(|e| TmError::storage(format!("reading {}: {e}", path.display())))?;
        if let Some(doc) = parse_decision_doc(file_name, &contents) {
            out.push(doc);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// The reverse of every doc's own `supersedes` field: `id -> the decision that names id in its own
/// Supersedes field`, if any. A doc only records what it supersedes, never what (if anything) went
/// on to supersede it, so this has to be computed once over the whole set rather than read off any
/// single doc.
fn superseded_by_map(docs: &[DecisionDoc]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for d in docs {
        if let Some(prev) = &d.supersedes {
            map.insert(prev.clone(), d.id.clone());
        }
    }
    map
}

/// Walk one decision's full chain: predecessors (via `supersedes`, oldest first), then `id`
/// itself, then successors (via the computed `superseded_by` map, newest last). Bounded by `seen`
/// in case a hand-authored chain (nothing stops two decision docs from naming each other) cycles —
/// unlike a `Store`-backed `Decision`, where `Store::supersede` enforces a decision may only be
/// superseded once, a hand-edited `Supersedes:` field has no such structural guarantee.
fn chain(
    by_id: &HashMap<String, &DecisionDoc>,
    superseded_by: &HashMap<String, String>,
    id: &str,
) -> Vec<String> {
    let mut seen = HashSet::new();
    seen.insert(id.to_string());

    let mut before = Vec::new();
    let mut cursor = by_id.get(id).and_then(|d| d.supersedes.clone());
    while let Some(prev_id) = cursor {
        if !seen.insert(prev_id.clone()) {
            break;
        }
        before.push(prev_id.clone());
        cursor = by_id
            .get(prev_id.as_str())
            .and_then(|d| d.supersedes.clone());
    }
    before.reverse();

    let mut after = Vec::new();
    let mut cursor = superseded_by.get(id).cloned();
    while let Some(next_id) = cursor {
        if !seen.insert(next_id.clone()) {
            break;
        }
        after.push(next_id.clone());
        cursor = superseded_by.get(&next_id).cloned();
    }

    let mut full = before;
    full.push(id.to_string());
    full.extend(after);
    full
}

fn render_page(
    by_id: &HashMap<String, &DecisionDoc>,
    superseded_by: &HashMap<String, String>,
    id: &str,
) -> Option<WikiPage> {
    let d = *by_id.get(id)?;
    let chain_ids = chain(by_id, superseded_by, id);

    let mut body = format!("# {}: {}\n\n", d.id, d.title);
    body.push_str(&format!("**Status:** {}\n\n", d.status));
    body.push_str(&format!("**Date:** {}\n\n", d.date));

    if let Some(prev) = &d.supersedes {
        if let Some(prev_doc) = by_id.get(prev.as_str()) {
            body.push_str(&format!(
                "**Supersedes:** [{prev}]({}.md) — {}\n\n",
                prev_doc.file_stem, prev_doc.title
            ));
        } else {
            body.push_str(&format!(
                "**Supersedes:** {prev} (no matching docs/decisions/{prev}-*.md file found)\n\n"
            ));
        }
    }
    if let Some(next) = superseded_by.get(id) {
        if let Some(next_doc) = by_id.get(next.as_str()) {
            body.push_str(&format!(
                "**Superseded by:** [{next}]({}.md) — {}\n\n",
                next_doc.file_stem, next_doc.title
            ));
        }
    }

    if chain_ids.len() > 1 {
        body.push_str("## Supersession chain\n\n");
        for chain_id in &chain_ids {
            if let Some(cd) = by_id.get(chain_id.as_str()) {
                let marker = if chain_id == id { "->" } else { "  " };
                body.push_str(&format!(
                    "{marker} [{}]({}.md) — {}\n",
                    cd.id, cd.file_stem, cd.title
                ));
            }
        }
        body.push('\n');
    }

    body.push_str(&format!(
        "Full text: [`{}`](../../decisions/{}.md)\n",
        d.source_path(),
        d.file_stem
    ));

    // Staleness basis: every doc in the chain, not just this one — a change anywhere in the chain
    // (e.g. a new decision naming this one in its own Supersedes field) should invalidate every
    // page in it, matching the old Store-backed behaviour's `derived_from`.
    let derived_from: Vec<String> = chain_ids
        .iter()
        .filter_map(|c| by_id.get(c.as_str()))
        .map(|cd| cd.source_path())
        .collect();

    Some(WikiPage::new(
        format!("decisions/{}", d.file_stem),
        format!("decisions/{}.md", d.file_stem),
        body,
        derived_from,
    ))
}

/// One page per decision (with its chain) plus a `decisions.md` index listing every decision and
/// its status, assembled by reading `docs/decisions/D-NNN-*.md` under `project_root` — this
/// module's whole point (see this file's module doc). Purely filesystem-driven: no `Store`/
/// `ProjectView` parameter, unlike [`crate::tickets::page`]/[`crate::glossary::page`], because
/// decisions no longer come from either.
pub fn pages(project_root: &Path) -> Result<Vec<WikiPage>> {
    let decisions_dir = project_root.join("docs/decisions");
    let docs = load_decisions(&decisions_dir)?;

    let by_id: HashMap<String, &DecisionDoc> = docs.iter().map(|d| (d.id.clone(), d)).collect();
    let superseded_by = superseded_by_map(&docs);

    let mut out = Vec::new();
    let mut index_body =
        String::from("# Decisions\n\n| Decision | Title | Status |\n|---|---|---|\n");
    for d in &docs {
        index_body.push_str(&format!(
            "| [{}](decisions/{}.md) | {} | {} |\n",
            d.id, d.file_stem, d.title, d.status
        ));
        if let Some(page) = render_page(&by_id, &superseded_by, &d.id) {
            out.push(page);
        }
    }
    if docs.is_empty() {
        index_body.push_str(
            "\n_No decisions recorded yet; add a `docs/decisions/D-NNN-*.md` file (see \
             `CLAUDE.md`'s decision-doc convention) and run `tm wiki generate` again._\n",
        );
    }

    out.push(WikiPage::new(
        "decisions",
        "decisions.md",
        index_body,
        vec!["docs/decisions/**".to_string()],
    ));

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D1_INLINE: &str =
        "# D-001 — Use SQLite\n\n**Status:** accepted · **Date:** 2026-01-01 · \
                              **Supersedes:** nothing\n\n## Context\n\nBody.\n";

    fn write_doc(dir: &Path, file_name: &str, contents: &str) {
        fs::write(dir.join(file_name), contents).unwrap();
    }

    /// `<root>/docs/decisions`, created if absent — the directory [`pages`] (unlike
    /// [`load_decisions`], which tests call directly against whatever directory they pass it)
    /// always reads relative to a project root.
    fn decisions_dir(root: &Path) -> std::path::PathBuf {
        let dir = root.join("docs/decisions");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_decision_doc_reads_inline_metadata_format() {
        let doc = parse_decision_doc("D-001-use-sqlite.md", D1_INLINE).unwrap();
        assert_eq!(doc.id, "D-001");
        assert_eq!(doc.file_stem, "D-001-use-sqlite");
        assert_eq!(doc.title, "Use SQLite");
        assert_eq!(doc.status, "accepted");
        assert_eq!(doc.date, "2026-01-01");
        assert_eq!(doc.supersedes, None);
    }

    #[test]
    fn parse_decision_doc_reads_bulleted_metadata_format() {
        // D-001's real, exceptional format: three separate bullet lines instead of one inline
        // line — the parser has to accept both shapes since both are real, in-use shapes.
        let content = "# D-002 — Use SQLite\n\n- **Status:** accepted\n- **Date:** 2026-01-01\n- \
                        **Supersedes:** an old assumption in SPEC.md\n\n## Context\n\nBody.\n";
        let doc = parse_decision_doc("D-002-use-sqlite.md", content).unwrap();
        assert_eq!(doc.status, "accepted");
        assert_eq!(doc.date, "2026-01-01");
        assert_eq!(
            doc.supersedes, None,
            "free-text Supersedes with no leading D-NNN token is not a real chain link"
        );
    }

    #[test]
    fn parse_decision_doc_reads_a_real_supersedes_reference() {
        let content = "# D-002 — Use SQLite (revised)\n\n**Status:** accepted · **Date:** \
                        2026-01-02 · **Supersedes:** D-001\n\n## Context\n\nBody.\n";
        let doc = parse_decision_doc("D-002-use-sqlite-revised.md", content).unwrap();
        assert_eq!(doc.supersedes, Some("D-001".to_string()));
    }

    #[test]
    fn parse_decision_doc_treats_incidental_d_nnn_mention_as_no_supersession() {
        // Matches this repo's real D-006: "nothing (extends D-002)" mentions another decision in
        // passing but does not supersede it — the value must actually *start with* a D-NNN token
        // (or be "nothing") to count, or this would falsely chain D-006 under D-002.
        let content = "# D-006 — Extends something\n\n**Status:** accepted · **Date:** \
                        2026-01-06 · **Supersedes:** nothing (extends D-002)\n\n## Context\n\nBody.\n";
        let doc = parse_decision_doc("D-006-extends-something.md", content).unwrap();
        assert_eq!(doc.supersedes, None);
    }

    #[test]
    fn parse_decision_doc_skips_a_file_with_no_metadata_line() {
        let content = "# D-009 — No metadata\n\n## Context\n\nNothing to parse here.\n";
        assert!(parse_decision_doc("D-009-no-metadata.md", content).is_none());
    }

    #[test]
    fn parse_decision_doc_skips_a_file_with_no_title_line() {
        let content = "Just prose, no heading.\n\n**Status:** accepted\n";
        assert!(parse_decision_doc("D-009-no-title.md", content).is_none());
    }

    #[test]
    fn load_decisions_skips_non_decision_markdown_files() {
        let dir = tempfile::TempDir::new().unwrap();
        write_doc(dir.path(), "D-001-use-sqlite.md", D1_INLINE);
        write_doc(dir.path(), "README.md", "# Not a decision\n");
        let docs = load_decisions(dir.path()).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].id, "D-001");
    }

    #[test]
    fn load_decisions_is_empty_not_an_error_when_dir_is_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let docs = load_decisions(&dir.path().join("nope")).unwrap();
        assert!(docs.is_empty());
    }

    /// A synthetic three-link chain: D-001 <- D-002 <- D-003. None of this repo's 14 real
    /// decision docs supersede one another yet (checked by hand before writing this parser), so
    /// this fixture is the only thing that exercises multi-link chain ordering at all.
    fn write_multi_link_fixture(dir: &Path) {
        write_doc(
            dir,
            "D-001-use-sqlite.md",
            "# D-001 — Use SQLite\n\n**Status:** superseded · **Date:** 2026-01-01 · \
             **Supersedes:** nothing\n\n## Context\n\nBody.\n",
        );
        write_doc(
            dir,
            "D-002-use-sqlite-revised.md",
            "# D-002 — Use SQLite (revised)\n\n**Status:** superseded · **Date:** 2026-01-02 · \
             **Supersedes:** D-001\n\n## Context\n\nBody.\n",
        );
        write_doc(
            dir,
            "D-003-use-postgres.md",
            "# D-003 — Use Postgres\n\n**Status:** accepted · **Date:** 2026-01-03 · \
             **Supersedes:** D-002\n\n## Context\n\nBody.\n",
        );
    }

    #[test]
    fn chain_orders_a_synthetic_multi_link_chain_oldest_to_newest() {
        let dir = tempfile::TempDir::new().unwrap();
        write_multi_link_fixture(dir.path());
        let docs = load_decisions(dir.path()).unwrap();
        assert_eq!(docs.len(), 3);
        let by_id: HashMap<String, &DecisionDoc> = docs.iter().map(|d| (d.id.clone(), d)).collect();
        let superseded_by = superseded_by_map(&docs);

        let expected = vec![
            "D-001".to_string(),
            "D-002".to_string(),
            "D-003".to_string(),
        ];
        assert_eq!(chain(&by_id, &superseded_by, "D-001"), expected);
        assert_eq!(chain(&by_id, &superseded_by, "D-002"), expected);
        assert_eq!(chain(&by_id, &superseded_by, "D-003"), expected);
    }

    #[test]
    fn pages_renders_full_chain_on_every_page_in_a_synthetic_multi_link_chain() {
        let dir = tempfile::TempDir::new().unwrap();
        write_multi_link_fixture(&decisions_dir(dir.path()));
        let pages = pages(dir.path()).unwrap();

        let middle = pages
            .iter()
            .find(|p| p.rel_path == "decisions/D-002-use-sqlite-revised.md")
            .expect("a page for D-002 was generated");
        assert!(middle.body.contains("## Supersession chain"));
        let chain_section = middle.body.split("## Supersession chain").nth(1).unwrap();
        let d1_pos = chain_section.find("D-001").unwrap();
        let d2_pos = chain_section.find("D-002").unwrap();
        let d3_pos = chain_section.find("D-003").unwrap();
        assert!(
            d1_pos < d2_pos && d2_pos < d3_pos,
            "chain must render oldest to newest"
        );
        assert!(chain_section.contains("-> [D-002]"));
        assert!(middle.body.contains("**Supersedes:** [D-001]"));
        assert!(middle.body.contains("**Superseded by:** [D-003]"));

        assert_eq!(
            middle.derived_from,
            vec![
                "docs/decisions/D-001-use-sqlite.md".to_string(),
                "docs/decisions/D-002-use-sqlite-revised.md".to_string(),
                "docs/decisions/D-003-use-postgres.md".to_string(),
            ],
            "every page in a chain must go stale when any doc in the chain changes"
        );

        // 3 per-decision pages + 1 index.
        assert_eq!(pages.len(), 4);
    }

    #[test]
    fn pages_index_lists_every_decision_with_status_and_links_to_its_page() {
        let dir = tempfile::TempDir::new().unwrap();
        write_doc(&decisions_dir(dir.path()), "D-001-use-sqlite.md", D1_INLINE);
        let pages = pages(dir.path()).unwrap();
        let index = pages.iter().find(|p| p.rel_path == "decisions.md").unwrap();
        assert!(index.body.contains("D-001"));
        assert!(index.body.contains("Use SQLite"));
        assert!(index.body.contains("accepted"));
        assert!(index.body.contains("(decisions/D-001-use-sqlite.md)"));
        assert_eq!(index.derived_from, vec!["docs/decisions/**".to_string()]);
    }

    #[test]
    fn pages_index_explains_itself_when_no_decisions_exist() {
        let dir = tempfile::TempDir::new().unwrap();
        let pages = pages(dir.path()).unwrap();
        assert_eq!(pages.len(), 1, "only the index page, no per-decision pages");
        let index = &pages[0];
        assert!(
            index.body.contains("No decisions recorded yet"),
            "an empty decisions page should explain why it's empty, not just show a bare table \
             header:\n{}",
            index.body
        );
        assert!(
            !index.body.contains("tm decision new"),
            "the empty-state message must not point at the unrelated ticket-level Decision \
             entity's CLI verb"
        );
    }

    /// The real proof this module exists for: run the actual generator against this actual
    /// workspace's real `docs/decisions/*.md` files. Presence-only assertions (not a hardcoded
    /// count) — this repo's own `CLAUDE.md` names exactly this shape of assumption (a hardcoded
    /// count derived from a collection that grows over time) as the recurring, real drift bug to
    /// avoid.
    #[test]
    fn pages_reads_this_real_repositorys_own_decision_docs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("crates/tm-wiki has two parent directories under the workspace root");
        assert!(
            root.join("SPEC.md").is_file(),
            "resolved workspace root looks wrong: {}",
            root.display()
        );

        let pages = pages(root).unwrap();

        let index = pages.iter().find(|p| p.rel_path == "decisions.md").unwrap();
        assert!(index.body.contains("D-003"));
        assert!(index.body.contains("Project scope"));

        let d3_page = pages
            .iter()
            .find(|p| p.rel_path.starts_with("decisions/D-003-"))
            .expect("a page for D-003 was generated");
        assert!(d3_page.body.contains("Project scope"));
        assert!(d3_page.body.contains("**Status:** accepted"));
    }
}
