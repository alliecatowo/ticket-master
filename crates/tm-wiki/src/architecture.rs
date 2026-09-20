//! `architecture/<crate>` pages (`SPEC.md` §26.2): module tree plus public symbol outlines, one
//! page per workspace crate. Assembled entirely from [`CodeIntel`]'s existing retrieval
//! ([`CodeIntel::outline`]) and a directory listing — no new static analysis lives here.

use std::path::Path;

use tm_codeintel::symbols::OutlineEntry;
use tm_codeintel::{walk::Language, CodeIntel, RepoWalker};
use tm_types::{Result, TmError};

use crate::page::WikiPage;

/// True when a rendered symbol signature looks public (`pub`/`pub(crate)`/...). Best-effort text
/// heuristic: [`tm_codeintel::Symbol`] carries no dedicated visibility field, and its rendered
/// signature is exactly the source's own declaration text, so this matches what a reader
/// scanning the file would call "public" without re-parsing anything itself.
fn looks_public(rendered: &str) -> bool {
    rendered.trim_start().starts_with("pub")
}

/// Reduce `outline` (in the source order [`CodeIntel::outline`] returns it — one entry per
/// symbol in the whole file, public or not) to the entries worth showing in a "public symbols"
/// section, each paired with its correct *render* depth.
///
/// An entry is kept when it [`looks_public`] itself, **or** when one of its descendants does:
/// e.g. `impl Project { pub fn code_intel(&self) -> .. }` has no `pub` of its own (Rust has no
/// `pub impl`), but the `pub fn` inside it does, so the `impl` header is kept too, as a plain
/// (not `pub`-prefixed) heading its kept children nest under. Dropping such a container outright
/// and rendering its children at their raw [`OutlineEntry::depth`] (this function's first version
/// did exactly that) visually re-parents them under whatever *unrelated* entry happens to precede
/// them at the same raw depth once their real container is gone — usually invisible, because
/// Rust idiom keeps an `impl Type` block immediately after `struct`/`enum Type`, so the
/// accidental parent (that preceding struct) is usually also the right one by coincidence, but
/// not always: `crates/tm-cli/src/project.rs`'s `pub fn doctor(..)` (a free function) happens to
/// immediately precede a *second, unrelated* `impl Project { pub(crate) fn for_test(..) }` block,
/// and the old code rendered `for_test` as if it were nested inside `doctor`.
///
/// Two passes: the first walks `outline` once to mark which entries have a kept descendant
/// (bottom-up, via a stack of currently-open ancestors); the second walks it again to assign each
/// kept entry the render depth given by its count of *kept* ancestors, using the same depth-keyed
/// stack technique, and reparents kept descendants of a dropped container one level up rather
/// than leaving them at a depth nothing kept still supports.
fn render_public_outline(outline: Vec<OutlineEntry>) -> Vec<(u32, String)> {
    let keep = mark_kept_entries(&outline);

    /// One stack frame per open ancestor (kept or not); `child_render_depth` is the depth a kept
    /// *child* of this frame should render at.
    struct Frame {
        raw_depth: i64,
        child_render_depth: u32,
    }

    // A virtual root below every real entry (`raw_depth: -1`) so depth-0 entries pop nothing and
    // start at render depth 0.
    let mut stack = vec![Frame {
        raw_depth: -1,
        child_render_depth: 0,
    }];
    let mut out = Vec::new();

    for (i, entry) in outline.into_iter().enumerate() {
        let raw_depth = i64::from(entry.depth);
        while stack.last().is_some_and(|f| f.raw_depth >= raw_depth) {
            stack.pop();
        }
        // `stack` is never actually empty here -- the virtual root's `raw_depth` (-1) is below
        // every real depth (>= 0), so the loop above always leaves at least the root behind --
        // but `unwrap_or(0)` costs nothing and avoids relying on that invariant to not panic.
        let parent_render_depth = stack.last().map_or(0, |f| f.child_render_depth);

        if keep[i] {
            out.push((parent_render_depth, entry.rendered));
            stack.push(Frame {
                raw_depth,
                child_render_depth: parent_render_depth + 1,
            });
        } else {
            // Dropped: any kept descendant should still attach at this frame's own level, so
            // pass `child_render_depth` through unchanged rather than incrementing it.
            stack.push(Frame {
                raw_depth,
                child_render_depth: parent_render_depth,
            });
        }
    }
    out
}

/// First pass for [`render_public_outline`]: `keep[i]` is true when `outline[i]` itself
/// [`looks_public`], or when any of its descendants (transitively) do. Computed bottom-up in one
/// forward walk: `open` tracks the still-open ancestor chain by index, popped down to the current
/// entry's depth on each step; finding a public entry marks every index still on `open` (every
/// live ancestor), not just the immediate parent, so a public entry two non-public containers
/// deep still keeps both of them.
fn mark_kept_entries(outline: &[OutlineEntry]) -> Vec<bool> {
    let mut keep = vec![false; outline.len()];
    let mut open: Vec<usize> = Vec::new();

    for (i, entry) in outline.iter().enumerate() {
        while open
            .last()
            .is_some_and(|&a| outline[a].depth >= entry.depth)
        {
            open.pop();
        }
        if looks_public(&entry.rendered) {
            keep[i] = true;
            for &ancestor in &open {
                keep[ancestor] = true;
            }
        }
        open.push(i);
    }
    keep
}

/// Every workspace crate's directory name under `crates/`, sorted. A directory listing, not code
/// analysis — [`CodeIntel`] supplies the actual symbol content per file.
///
/// A project with no `crates/` directory at all (not a Cargo workspace shaped this way, or not a
/// Rust project) yields an empty list rather than an error — the same "absent directory means
/// nothing to report, not a failure" convention `tm-cli`'s `workflows_dir`/`discover_bench_tasks`
/// already use for their own optional directories. Erroring here would otherwise make `tm wiki
/// generate` fail outright for any project without that layout, instead of just generating no
/// `architecture/*` pages for it.
fn crate_names(project_root: &Path) -> Result<Vec<String>> {
    let crates_dir = project_root.join("crates");
    let entries = match std::fs::read_dir(&crates_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(TmError::storage(format!(
                "reading {}: {e}",
                crates_dir.display()
            )))
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| TmError::storage(format!("reading crates/: {e}")))?;
        if entry.path().join("Cargo.toml").is_file() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Build one `architecture/<crate>` page per workspace crate that has at least one indexed Rust
/// file under `crates/<name>/src/`: a module tree (from the live working-tree file list) plus
/// each file's public symbols ([`CodeIntel::symbol_index`]'s per-file `outline`, filtered by
/// [`looks_public`]).
///
/// Requires `ci` to already reflect the current tree ([`CodeIntel::update_incremental`]) — this
/// function only reads from the existing index, it never (re)indexes anything. [`CodeIntel::symbol_index`]
/// is called exactly once, up front, rather than once per file: it re-parses every indexed file
/// from disk on each call (see its own doc comment), so calling it inside the per-file loop
/// below would parse the whole workspace once per file instead of once total.
pub fn pages(project_root: &Path, ci: &CodeIntel) -> Result<Vec<WikiPage>> {
    let files = RepoWalker::new(project_root).walk()?;
    let names = crate_names(project_root)?;
    let index = ci.symbol_index()?;
    let mut out = Vec::with_capacity(names.len());

    for name in names {
        let prefix = format!("crates/{name}/src/");
        let mut crate_files: Vec<&str> = files
            .iter()
            .filter(|f| f.path.starts_with(&prefix) && f.lang == Some(Language::Rust))
            .map(|f| f.path.as_str())
            .collect();
        crate_files.sort_unstable();

        if crate_files.is_empty() {
            continue;
        }

        let mut body = format!("# Architecture: {name}\n\n## Module tree\n\n");
        for path in &crate_files {
            body.push_str(&format!("- `{path}`\n"));
        }

        body.push_str("\n## Public symbols\n");
        let mut any_symbols = false;
        for path in &crate_files {
            let outline = index.outline(path);
            let public = render_public_outline(outline);
            if public.is_empty() {
                continue;
            }
            any_symbols = true;
            body.push_str(&format!("\n### `{path}`\n\n"));
            for (depth, rendered) in public {
                let indent = "  ".repeat(depth as usize);
                body.push_str(&format!("{indent}- `{rendered}`\n"));
            }
        }
        if !any_symbols {
            body.push_str(
                "\n_No public symbols found in the current index; run `CodeIntel::update_incremental` \
                 first if this crate has changed since the last index update._\n",
            );
        }

        out.push(WikiPage::new(
            format!("architecture/{name}"),
            format!("architecture/{name}.md"),
            body,
            vec![format!("crates/{name}/src/**")],
        ));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_public_detects_pub_prefix() {
        assert!(looks_public("pub fn foo() -> Bar"));
        assert!(looks_public("pub struct Foo"));
        assert!(looks_public("  pub(crate) fn bar()"));
        assert!(!looks_public("fn private_fn()"));
        assert!(!looks_public("struct PrivateStruct"));
    }

    fn entry(depth: u32, rendered: &str) -> OutlineEntry {
        OutlineEntry {
            symbol_id: 0,
            depth,
            rendered: rendered.to_string(),
        }
    }

    #[test]
    fn render_public_outline_keeps_siblings_at_top_level() {
        let outline = vec![entry(0, "pub fn a()"), entry(0, "pub fn b()")];
        let rendered = render_public_outline(outline);
        assert_eq!(
            rendered,
            vec![(0, "pub fn a()".to_string()), (0, "pub fn b()".to_string())]
        );
    }

    #[test]
    fn render_public_outline_nests_a_real_child_under_its_pub_parent() {
        // `pub struct DoctorReport { pub fn all_ok(&self) -> bool }` -- a genuine nesting: the
        // container itself is public, so the child's raw depth (1) is already correct.
        let outline = vec![
            entry(0, "pub struct DoctorReport"),
            entry(1, "pub fn all_ok(&self) -> bool"),
        ];
        let rendered = render_public_outline(outline);
        assert_eq!(
            rendered,
            vec![
                (0, "pub struct DoctorReport".to_string()),
                (1, "pub fn all_ok(&self) -> bool".to_string()),
            ]
        );
    }

    /// Reproduces the real misfire this function exists to fix, found by reviewing this crate's
    /// own generated `docs/wiki/architecture/tm-cli.md`: `crates/tm-cli/src/project.rs` has one
    /// `impl Project { pub fn code_intel(..); pub fn scope_line(..); }` block, then a free
    /// `pub fn doctor(..)`, then a *second, unrelated* `impl Project { pub(crate) fn for_test(..) }`
    /// block (a `#[cfg(test)]` helper, physically separate from the first `impl Project`). Neither
    /// `impl Project` header is itself `looks_public` (Rust has no `pub impl`), so the old
    /// depth-only rendering dropped both headers and printed their `pub` children at raw depth 1
    /// directly after whatever depth-0 entry happened to render last -- `for_test` ended up
    /// printed as if nested inside `doctor`, which it has no relationship to at all.
    #[test]
    fn render_public_outline_promotes_a_container_that_has_a_kept_descendant() {
        let outline = vec![
            entry(0, "impl Project"),
            entry(1, "pub fn code_intel(&self) -> Result<CodeIntel>"),
            entry(1, "pub fn scope_line(&self) -> String"),
            entry(
                0,
                "pub fn doctor(project: &Project) -> Result<DoctorReport>",
            ),
            entry(0, "impl Project"),
            entry(1, "pub(crate) fn for_test(root: &Path) -> Project"),
        ];
        let rendered = render_public_outline(outline);
        assert_eq!(
            rendered,
            vec![
                (0, "impl Project".to_string()),
                (
                    1,
                    "pub fn code_intel(&self) -> Result<CodeIntel>".to_string()
                ),
                (1, "pub fn scope_line(&self) -> String".to_string()),
                (
                    0,
                    "pub fn doctor(project: &Project) -> Result<DoctorReport>".to_string()
                ),
                (0, "impl Project".to_string()),
                (
                    1,
                    "pub(crate) fn for_test(root: &Path) -> Project".to_string()
                ),
            ],
            "for_test must nest under its own (shown) impl Project header, not under doctor"
        );
    }

    #[test]
    fn render_public_outline_drops_a_container_with_no_kept_descendant() {
        // A private `impl` block whose only method is also private contributes nothing -- not
        // even a bare, childless "impl Foo" heading.
        let outline = vec![
            entry(0, "impl Foo"),
            entry(1, "fn private_helper()"),
            entry(0, "pub fn free_fn()"),
        ];
        let rendered = render_public_outline(outline);
        assert_eq!(rendered, vec![(0, "pub fn free_fn()".to_string())]);
    }

    #[test]
    fn render_public_outline_keeps_a_grandchild_at_its_true_depth_through_a_promoted_container() {
        // pub mod outer { struct Inner { pub fn deep() } } -- `Inner` is not itself `pub`, but it
        // has a kept descendant (`deep`), so it is promoted (shown, at depth 1) rather than
        // dropped, and `deep` renders at its real depth (2) underneath it.
        let outline = vec![
            entry(0, "pub mod outer"),
            entry(1, "struct Inner"),
            entry(2, "pub fn deep()"),
        ];
        let rendered = render_public_outline(outline);
        assert_eq!(
            rendered,
            vec![
                (0, "pub mod outer".to_string()),
                (1, "struct Inner".to_string()),
                (2, "pub fn deep()".to_string()),
            ]
        );
    }

    #[test]
    fn crate_names_lists_only_directories_with_a_cargo_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/tm-a")).unwrap();
        std::fs::write(
            dir.path().join("crates/tm-a/Cargo.toml"),
            "[package]\nname=\"tm-a\"",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/not-a-crate")).unwrap();

        let names = crate_names(dir.path()).unwrap();
        assert_eq!(names, vec!["tm-a".to_string()]);
    }

    #[test]
    fn crate_names_is_empty_not_an_error_when_crates_dir_is_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let names = crate_names(dir.path()).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn pages_returns_no_pages_for_a_project_with_no_crates_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let ci = CodeIntel::open(dir.path()).unwrap();
        let pages = pages(dir.path(), &ci).unwrap();
        assert!(pages.is_empty());
    }
}
