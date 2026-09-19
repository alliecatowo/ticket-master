//! `architecture/<crate>` pages (`SPEC.md` §26.2): module tree plus public symbol outlines, one
//! page per workspace crate. Assembled entirely from [`CodeIntel`]'s existing retrieval
//! ([`CodeIntel::outline`]) and a directory listing — no new static analysis lives here.

use std::path::Path;

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

/// Every workspace crate's directory name under `crates/`, sorted. A directory listing, not code
/// analysis — [`CodeIntel`] supplies the actual symbol content per file.
fn crate_names(project_root: &Path) -> Result<Vec<String>> {
    let crates_dir = project_root.join("crates");
    let entries = std::fs::read_dir(&crates_dir)
        .map_err(|e| TmError::storage(format!("reading {}: {e}", crates_dir.display())))?;

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
            let public: Vec<_> = outline
                .into_iter()
                .filter(|e| looks_public(&e.rendered))
                .collect();
            if public.is_empty() {
                continue;
            }
            any_symbols = true;
            body.push_str(&format!("\n### `{path}`\n\n"));
            for entry in public {
                let indent = "  ".repeat(entry.depth as usize);
                body.push_str(&format!("{indent}- `{}`\n", entry.rendered));
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
}
