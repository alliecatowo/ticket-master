//! `history/<path>` pages (`SPEC.md` §26.2): "why is this like this", assembled from
//! [`CodeIntel::history_why`] — the git-blame-shaped query `tm-codeintel` already indexes. No new
//! git access lives here.

use std::path::Path;

use tm_codeintel::CodeIntel;
use tm_types::Result;

use crate::page::WikiPage;

/// Turn a repository path into a filesystem-safe page name (`/` -> `__`).
fn sanitize(path: &str) -> String {
    path.replace('/', "__")
}

/// Build one `history/<path>` page per entry in `paths`: [`CodeIntel::history_why`] blamed over
/// the file's full current line range, rendered as the commits (most recent first) that produced
/// it. A path with no git history at all (not yet committed, or not found by `git2`) is skipped
/// rather than producing an empty page.
pub fn pages(project_root: &Path, ci: &CodeIntel, paths: &[String]) -> Result<Vec<WikiPage>> {
    let mut out = Vec::new();

    for path in paths {
        let full = project_root.join(path);
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        let line_count = text.lines().count().max(1) as u32;

        let answer = match ci.history_why(path, 1, line_count) {
            Ok(a) => a,
            Err(_) => continue,
        };
        if answer.commits.is_empty() {
            continue;
        }

        let mut body = format!(
            "# History: {path}\n\nWhy this file looks the way it does — {} commit(s) touched lines 1-{line_count}:\n\n",
            answer.commits.len()
        );
        for commit in &answer.commits {
            let sha_short = commit.sha.get(..7).unwrap_or(&commit.sha);
            let first_line = commit.message.lines().next().unwrap_or("");
            body.push_str(&format!(
                "- `{sha_short}` {first_line} — {}\n",
                commit.author
            ));
        }

        out.push(WikiPage::new(
            format!("history/{path}"),
            format!("history/{}.md", sanitize(path)),
            body,
            vec![path.clone()],
        ));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_path_separators() {
        assert_eq!(
            sanitize("crates/tm-core/src/lib.rs"),
            "crates__tm-core__src__lib.rs"
        );
    }

    #[test]
    fn pages_skips_paths_with_no_readable_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let ci = CodeIntel::open(dir.path()).unwrap();
        let pages = pages(dir.path(), &ci, &["does/not/exist.rs".to_string()]).unwrap();
        assert!(pages.is_empty());
    }
}
