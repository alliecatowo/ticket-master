//! `GET /wiki/*` (`SPEC.md` §26.4): wiki pages under `docs/wiki/*.md`, rendered from markdown to
//! HTML.
//!
//! Serving reads straight off disk via `AppState::config::project_root` — `tm-wiki` (the
//! generator) is a separate, offline write path (`SPEC.md` §26.2-26.3: pages are written to
//! `docs/wiki/*.md`, then served), so this module depends only on `tm-docs`'s pure front-matter
//! parser (to show a page's declared mode/basis and to strip the block before rendering), not on
//! `tm-wiki`, `tm-codeintel` or `tm-core`'s heavier assembly machinery.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use axum::extract::{Path as AxumPath, State};
use axum::response::Html;
use regex::Regex;

use tm_docs::registry::parse_front_matter;
use tm_types::TmError;

use crate::state::{AppState, ServerError};

/// Where generated and human wiki pages live, relative to the project root (`SPEC.md` §26.2-4).
pub const WIKI_DIR: &str = "docs/wiki";

/// Resolve a requested wiki path (e.g. `architecture/tm-core` or `architecture/tm-core.md`) to a
/// path relative to [`WIKI_DIR`], appending `.md` when the request omits an extension.
///
/// Rejects any path with a `..`, root, or prefix component — a request can never escape
/// `docs/wiki/` no matter how the wildcard segment is crafted.
fn resolve_rel_path(requested: &str) -> Result<PathBuf, ServerError> {
    let requested = requested.trim_start_matches('/');
    if requested.is_empty() {
        return Err(ServerError::BadRequest(
            "Wiki path is required.".to_string(),
        ));
    }

    let mut rel = PathBuf::new();
    for component in Path::new(requested).components() {
        match component {
            Component::Normal(part) => rel.push(part),
            _ => {
                return Err(ServerError::BadRequest(format!(
                    "Invalid wiki path: {requested}"
                )))
            }
        }
    }

    if rel.extension().is_none() {
        rel.set_extension("md");
    }
    Ok(rel)
}

static TICKET_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[TVA]-[0-9]+\b")
        .expect("literal regex pattern is compile-time constant and always valid")
});

/// Turn bare ticket ids (`T-123`, matching the same pattern `GET /schema` documents for
/// `TicketId`) into markdown links to `GET /tickets/:id` before handing the source to the
/// markdown renderer — so the rendered HTML cross-links tickets (`SPEC.md` §26.4) without every
/// page builder in `tm-wiki` needing to know the server's URL scheme.
fn linkify_ticket_ids(markdown: &str) -> String {
    TICKET_ID
        .replace_all(markdown, |caps: &regex::Captures<'_>| {
            let id = &caps[0];
            format!("[{id}](/tickets/{id})")
        })
        .into_owned()
}

/// Split an optional embedded `SPEC.md` §9 front-matter block off the top of `markdown`,
/// returning `(body, metadata_line)`. `metadata_line` is `None` when there is no front matter
/// (a plain page with no declaration at all) so the caller can render body-only.
///
/// This shows a page's declared mode/basis on every request; it does not recompute live
/// staleness (`tm_docs::Assessor::assess` against the project's current basis) — that is a
/// project-wide operation this per-page read does not have the state to perform.
fn strip_front_matter(markdown: &str) -> (String, Option<String>) {
    let meta = match parse_front_matter(markdown) {
        Ok(Some(fm)) => {
            let basis = if fm.derived_from.is_empty() {
                "(none)".to_string()
            } else {
                fm.derived_from.join(", ")
            };
            Some(format!("*{:?} · derived from: {basis}*", fm.mode))
        }
        _ => None,
    };

    if meta.is_none() {
        return (markdown.to_string(), None);
    }

    let mut body_lines = Vec::new();
    let mut opened = false;
    let mut in_block = false;
    for line in markdown.lines() {
        let trimmed = line.trim_end();
        if !opened && (trimmed == "+++" || trimmed == "---") {
            opened = true;
            in_block = true;
            continue;
        }
        if in_block {
            if trimmed == "+++" || trimmed == "---" {
                in_block = false;
            }
            continue;
        }
        body_lines.push(line);
    }

    (body_lines.join("\n"), meta)
}

fn markdown_to_html(markdown: &str) -> String {
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(markdown));
    html
}

fn html_page(title: &str, body_html: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>{title}</title></head>\n<body>\n{body_html}\n</body>\n</html>\n"
    )
}

/// `GET /wiki/{*path}`: render one wiki page as HTML.
pub async fn get_wiki_page(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Result<Html<String>, ServerError> {
    let rel = resolve_rel_path(&path)?;
    let full = state.config.project_root.join(WIKI_DIR).join(&rel);

    let markdown = std::fs::read_to_string(&full).map_err(|_| {
        ServerError::Domain(TmError::not_found("wiki page", rel.display().to_string()))
    })?;

    let (body, meta) = strip_front_matter(&markdown);
    let linked = linkify_ticket_ids(&body);
    let mut body_html = markdown_to_html(&linked);
    if let Some(meta) = meta {
        body_html = format!("{}\n{body_html}", markdown_to_html(&meta));
    }

    Ok(Html(html_page(&rel.display().to_string(), &body_html)))
}

fn collect_pages(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), ServerError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ServerError::Domain(TmError::storage(format!("reading {}: {e}", dir.display())))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            ServerError::Domain(TmError::storage(format!("reading {}: {e}", dir.display())))
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_pages(root, &path, out)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.with_extension("").to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

/// `GET /wiki`: an index of every page found under `docs/wiki/`, linking to each one.
pub async fn list_wiki_pages(State(state): State<AppState>) -> Result<Html<String>, ServerError> {
    let root = state.config.project_root.join(WIKI_DIR);

    let mut pages = Vec::new();
    if root.is_dir() {
        collect_pages(&root, &root, &mut pages)?;
    }
    pages.sort();

    let mut body = String::from("<h1>Wiki</h1>\n<ul>\n");
    for page in &pages {
        body.push_str(&format!("<li><a href=\"/wiki/{page}\">{page}</a></li>\n"));
    }
    body.push_str("</ul>\n");

    Ok(Html(html_page("Wiki", &body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rel_path_appends_md_extension() {
        assert_eq!(
            resolve_rel_path("architecture/tm-core").unwrap(),
            PathBuf::from("architecture/tm-core.md")
        );
    }

    #[test]
    fn resolve_rel_path_leaves_existing_extension_alone() {
        assert_eq!(
            resolve_rel_path("architecture/tm-core.md").unwrap(),
            PathBuf::from("architecture/tm-core.md")
        );
    }

    #[test]
    fn resolve_rel_path_rejects_traversal() {
        assert!(resolve_rel_path("../../../etc/passwd").is_err());
        assert!(resolve_rel_path("foo/../../bar").is_err());
    }

    #[test]
    fn resolve_rel_path_strips_a_leading_slash_rather_than_failing() {
        // The axum wildcard extractor never actually hands us a leading slash, but stripping it
        // defensively (rather than rejecting) means a leading slash is not itself a way to
        // smuggle an otherwise-normal path past validation.
        assert_eq!(
            resolve_rel_path("/glossary").unwrap(),
            PathBuf::from("glossary.md")
        );
    }

    #[test]
    fn resolve_rel_path_rejects_empty() {
        assert!(resolve_rel_path("").is_err());
    }

    #[test]
    fn linkify_ticket_ids_wraps_bare_ids() {
        let out = linkify_ticket_ids("See T-42 and V-7 for details.");
        assert_eq!(
            out,
            "See [T-42](/tickets/T-42) and [V-7](/tickets/V-7) for details."
        );
    }

    #[test]
    fn linkify_ticket_ids_leaves_non_ids_alone() {
        let out = linkify_ticket_ids("no ticket ids here, just X-9 which is not T/V/A prefixed");
        assert_eq!(
            out,
            "no ticket ids here, just X-9 which is not T/V/A prefixed"
        );
    }

    #[test]
    fn strip_front_matter_removes_the_block_and_returns_metadata() {
        let markdown = "+++\n[doc]\nid = \"wiki/glossary\"\nmode = \"generated\"\nderived_from = [\"D-1\"]\n+++\n\n# Glossary\n\nbody text\n";
        let (body, meta) = strip_front_matter(markdown);
        assert!(!body.contains("+++"));
        assert!(!body.contains("[doc]"));
        assert!(body.contains("# Glossary"));
        assert!(body.contains("body text"));
        let meta = meta.unwrap();
        assert!(meta.contains("Generated"));
        assert!(meta.contains("D-1"));
    }

    #[test]
    fn strip_front_matter_passes_through_plain_markdown() {
        let markdown = "# No front matter\n\njust a page\n";
        let (body, meta) = strip_front_matter(markdown);
        assert_eq!(body, markdown);
        assert!(meta.is_none());
    }

    #[test]
    fn markdown_to_html_renders_a_heading() {
        let html = markdown_to_html("# Hello\n");
        assert!(html.contains("<h1>Hello</h1>"));
    }
}
