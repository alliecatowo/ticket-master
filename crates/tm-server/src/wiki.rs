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
use axum::response::{Html, IntoResponse, Response};
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

/// Escape `&`, `<`, `>`, `"` and `'` for use in HTML text or a quoted attribute.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A link/image target is kept only when it is relative, an anchor, or `http(s)`/`mailto`;
/// anything else (`javascript:`, `data:`, `vbscript:`...) becomes `#`.
fn safe_url(url: &str) -> bool {
    let u = url.trim_start().to_ascii_lowercase();
    let u: String = u
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .collect();
    match u.split_once(':') {
        None => true,
        Some((scheme, _)) => {
            // A ':' after a '/', '?' or '#' belongs to a relative path, not a scheme.
            scheme.contains(['/', '?', '#']) || matches!(scheme, "http" | "https" | "mailto")
        }
    }
}

/// Render markdown with raw HTML neutralised: `<script>`, `<img onerror=...>` and the like are
/// shown as text, and non-web link schemes are dropped. The wiki is repo (and LLM) authored, so
/// it is untrusted input served from the API origin.
fn markdown_to_html(markdown: &str) -> String {
    use pulldown_cmark::{CowStr, Event, Tag};
    let events = pulldown_cmark::Parser::new(markdown).map(|event| match event {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) if !safe_url(&dest_url) => Event::Start(Tag::Link {
            link_type,
            dest_url: CowStr::Borrowed("#"),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) if !safe_url(&dest_url) => Event::Start(Tag::Image {
            link_type,
            dest_url: CowStr::Borrowed("#"),
            title,
            id,
        }),
        other => other,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

fn html_page(title: &str, body_html: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>{}</title></head>\n<body>\n{body_html}\n</body>\n</html>\n",
        escape_html(title)
    )
}

/// An HTML response that browsers will not run scripts from, even if sanitising missed
/// something: a locked-down CSP and no MIME sniffing.
fn locked_down_html(html: String) -> Response {
    (
        [
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Html(html),
    )
        .into_response()
}

/// `GET /wiki/{*path}`: render one wiki page as HTML.
pub async fn get_wiki_page(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Result<Response, ServerError> {
    let rel = resolve_rel_path(&path)?;
    let wiki_root = state.config.project_root.join(WIKI_DIR);
    let full = wiki_root.join(&rel);
    let not_found =
        || ServerError::Domain(TmError::not_found("wiki page", rel.display().to_string()));

    // Refuse symlinks (a committed `docs/wiki/x.md -> ~/.ssh/config` must not be served): the
    // canonical path has to stay under the canonical wiki root.
    let canonical_root = std::fs::canonicalize(&wiki_root).map_err(|_| not_found())?;
    let canonical = std::fs::canonicalize(&full).map_err(|_| not_found())?;
    if !canonical.starts_with(&canonical_root) {
        return Err(not_found());
    }
    let markdown = std::fs::read_to_string(&canonical).map_err(|_| not_found())?;

    let (body, meta) = strip_front_matter(&markdown);
    let linked = linkify_ticket_ids(&body);
    let mut body_html = markdown_to_html(&linked);
    if let Some(meta) = meta {
        body_html = format!("{}\n{body_html}", markdown_to_html(&meta));
    }

    Ok(locked_down_html(html_page(
        &rel.display().to_string(),
        &body_html,
    )))
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
        // Symlinks are neither followed (no cycles, no escaping the wiki) nor listed.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
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
pub async fn list_wiki_pages(State(state): State<AppState>) -> Result<Response, ServerError> {
    let root = state.config.project_root.join(WIKI_DIR);

    let mut pages = Vec::new();
    if root.is_dir() {
        collect_pages(&root, &root, &mut pages)?;
    }
    pages.sort();

    let mut body = String::from("<h1>Wiki</h1>\n<ul>\n");
    for page in &pages {
        let page = escape_html(page);
        body.push_str(&format!("<li><a href=\"/wiki/{page}\">{page}</a></li>\n"));
    }
    body.push_str("</ul>\n");

    Ok(locked_down_html(html_page("Wiki", &body)))
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

    #[test]
    fn markdown_to_html_neutralises_raw_html_and_script_links() {
        let html = markdown_to_html(
            "<script>alert(1)</script>\n\ntext <img src=x onerror=alert(2)>\n\n[a](javascript:alert(3)) [b](JaVa\tScRiPt:alert(4)) [ok](/tickets/T-1) [web](https://example.com)\n",
        );
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.to_lowercase().contains("href=\"javascript"), "{html}");
        assert!(html.contains("href=\"/tickets/T-1\""));
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn titles_and_page_names_are_escaped() {
        let page = html_page("\"><img src=x onerror=alert(1)>", "");
        assert!(!page.contains("<img"), "{page}");
        assert_eq!(escape_html("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&#39;");
    }
}
