//! The `search`, `symbol`, and `history` command groups over [`tm_codeintel`].

use serde::Serialize;

use crate::args::{
    HistoryCommand, HistorySearchArgs, HistoryWhyArgs, SearchArgs, SearchMode, SymbolCommand,
    SymbolOutlineArgs, SymbolQueryArgs,
};
use crate::project::Project;
use crate::render::{Renderer, Table};

/// Wrapper for JSON serialization of a search hit from exact/regex search.
#[derive(Debug, Clone, Serialize)]
struct ExactSearchHit {
    /// Path of the matching file.
    path: String,
    /// 1-based line number.
    line: u32,
    /// Column offset within the line.
    col: u32,
    /// The full text of the matching line.
    line_text: String,
}

/// Wrapper for JSON serialization of a semantic search result.
#[derive(Debug, Clone, Serialize)]
struct SemanticSearchResult {
    /// Path of the owning file.
    path: String,
    /// 1-based start line.
    line_start: u32,
    /// 1-based end line.
    line_end: u32,
    /// Cosine similarity score.
    score: f32,
    /// The chunk's text.
    text: String,
}

/// Wrapper for JSON serialization of a signal contribution in hybrid search.
#[derive(Debug, Clone, Serialize)]
struct SignalBreakdown {
    /// Which signal this is.
    signal: String,
    /// That signal's rank, or null if not surfaced.
    rank: Option<u32>,
    /// Weighted score contribution.
    weighted_score: f32,
}

/// Wrapper for JSON serialization of a hybrid search result.
#[derive(Debug, Clone, Serialize)]
struct HybridSearchResult {
    /// Path of the hit.
    path: String,
    /// Start line (if line-addressable).
    line_start: Option<u32>,
    /// End line.
    line_end: Option<u32>,
    /// Text snippet.
    snippet: String,
    /// Final fused score.
    fused_score: f32,
    /// Per-signal breakdown.
    explain: Vec<SignalBreakdown>,
}

/// Wrapper for JSON serialization of a symbol.
#[derive(Debug, Clone, Serialize)]
struct SymbolInfo {
    /// Symbol name.
    name: String,
    /// What kind of construct.
    kind: String,
    /// Path of the owning file.
    path: String,
    /// Start line.
    line_start: u32,
    /// End line.
    line_end: u32,
}

/// Wrapper for JSON serialization of a symbol reference.
#[derive(Debug, Clone, Serialize)]
struct ReferenceInfo {
    /// Path of the file containing the reference.
    path: String,
    /// Line number.
    line: u32,
    /// Column offset.
    col: u32,
}

/// Wrapper for JSON serialization of an outline entry.
#[derive(Debug, Clone, Serialize)]
struct OutlineInfo {
    /// Indentation depth.
    depth: u32,
    /// Rendered line.
    rendered: String,
}

/// Wrapper for JSON serialization of a commit summary.
#[derive(Debug, Clone, Serialize)]
struct CommitInfo {
    /// Commit hash.
    hash: String,
    /// Author name.
    author: String,
    /// Message subject (first line).
    message: String,
}

/// Wrapper for JSON serialization of a history hit.
#[derive(Debug, Clone, Serialize)]
struct HistoryHitInfo {
    /// The matching commit.
    commit: CommitInfo,
    /// Path the match occurred in (if in a diff).
    path: Option<String>,
    /// Snippet of matching text.
    snippet: String,
}

/// Wrapper for JSON serialization of a deleted code hit.
#[derive(Debug, Clone, Serialize)]
struct DeletedCodeInfo {
    /// Path the removed code lived in.
    path: String,
    /// Commit that removed it.
    commit: CommitInfo,
    /// The removed text.
    removed_text: String,
}

/// Wrapper for JSON serialization of a why answer.
#[derive(Debug, Clone, Serialize)]
struct WhyInfo {
    /// Path queried.
    path: String,
    /// Line start.
    line_start: u32,
    /// Line end.
    line_end: u32,
    /// Commits that last touched the range.
    commits: Vec<CommitInfo>,
}

/// `tm search <query> [--exact|--regex|--semantic|--hybrid]`
pub fn search(args: &SearchArgs, project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;

    match args.effective_mode() {
        SearchMode::Exact => {
            let result = code_intel.search_exact(&args.query)?;
            let hits: Vec<ExactSearchHit> = result
                .hits
                .iter()
                .take(args.limit)
                .map(|h| ExactSearchHit {
                    path: h.path.clone(),
                    line: h.line,
                    col: h.col,
                    line_text: h.line_text.clone(),
                })
                .collect();

            if result.truncated {
                renderer.note(&format!(
                    "Results truncated at {} hits; adjust --limit to see more",
                    args.limit
                ));
            }

            let human = if hits.is_empty() {
                "No matches found".to_string()
            } else {
                let rows: Vec<Vec<String>> = hits
                    .iter()
                    .map(|h| vec![format!("{}:{}", h.path, h.line), h.line_text.clone()])
                    .collect();
                let table = Table::new(vec!["Location".to_string(), "Text".to_string()], rows);
                table.render()
            };

            renderer.emit(&hits, &human)?;
        }

        SearchMode::Regex => {
            let result = code_intel.search_regex(&args.query)?;
            let hits: Vec<ExactSearchHit> = result
                .hits
                .iter()
                .take(args.limit)
                .map(|h| ExactSearchHit {
                    path: h.path.clone(),
                    line: h.line,
                    col: h.col,
                    line_text: h.line_text.clone(),
                })
                .collect();

            if result.truncated {
                renderer.note(&format!(
                    "Results truncated at {} hits; adjust --limit to see more",
                    args.limit
                ));
            }

            let human = if hits.is_empty() {
                "No matches found".to_string()
            } else {
                let rows: Vec<Vec<String>> = hits
                    .iter()
                    .map(|h| vec![format!("{}:{}", h.path, h.line), h.line_text.clone()])
                    .collect();
                let table = Table::new(vec!["Location".to_string(), "Text".to_string()], rows);
                table.render()
            };

            renderer.emit(&hits, &human)?;
        }

        SearchMode::Semantic => {
            let options = tm_codeintel::semantic::SemanticSearchOptions::default();
            let chunks = code_intel.search_semantic(&args.query, options)?;
            let results: Vec<SemanticSearchResult> = chunks
                .iter()
                .take(args.limit)
                .map(|c| SemanticSearchResult {
                    path: c.path.clone(),
                    line_start: c.line_start,
                    line_end: c.line_end,
                    score: c.score,
                    text: c.text.clone(),
                })
                .collect();

            let human = if results.is_empty() {
                "No similar content found".to_string()
            } else {
                let rows: Vec<Vec<String>> = results
                    .iter()
                    .map(|r| {
                        vec![
                            format!("{}:{}-{}", r.path, r.line_start, r.line_end),
                            format!("{:.3}", r.score),
                            r.text.lines().next().unwrap_or("").to_string(),
                        ]
                    })
                    .collect();
                let table = Table::new(
                    vec![
                        "Location".to_string(),
                        "Score".to_string(),
                        "Snippet".to_string(),
                    ],
                    rows,
                );
                table.render()
            };

            renderer.emit(&results, &human)?;
        }

        SearchMode::Hybrid => {
            let query = tm_codeintel::hybrid::Query {
                text: args.query.clone(),
                seed_symbols: vec![],
                seed_paths: vec![],
            };
            let context = tm_codeintel::hybrid::RetrievalContext {
                claimed_paths: vec![],
                recently_edited: vec![],
            };
            let weights = tm_codeintel::hybrid::SignalWeights::default();

            let ranked = code_intel.search_hybrid(&query, &context, weights)?;
            let results: Vec<HybridSearchResult> = ranked
                .iter()
                .take(args.limit)
                .map(|h| HybridSearchResult {
                    path: h.path.clone(),
                    line_start: h.line_start,
                    line_end: h.line_end,
                    snippet: h.snippet.clone(),
                    fused_score: h.fused_score,
                    explain: h
                        .explain
                        .iter()
                        .map(|sc| SignalBreakdown {
                            signal: format!("{:?}", sc.signal),
                            rank: sc.rank,
                            weighted_score: sc.weighted_score,
                        })
                        .collect(),
                })
                .collect();

            let human = if results.is_empty() {
                "No relevant content found".to_string()
            } else {
                let rows: Vec<Vec<String>> = results
                    .iter()
                    .map(|r| {
                        let loc = match r.line_start {
                            Some(line) => format!("{}:{}", r.path, line),
                            None => r.path.clone(),
                        };
                        vec![loc, format!("{:.3}", r.fused_score), r.snippet.clone()]
                    })
                    .collect();
                let table = Table::new(
                    vec![
                        "Location".to_string(),
                        "Score".to_string(),
                        "Snippet".to_string(),
                    ],
                    rows,
                );
                table.render()
            };

            renderer.emit(&results, &human)?;
        }
    }

    Ok(())
}

/// The retrieval mode a [`SearchArgs`] resolves to; kept distinct from [`SearchMode`] so this
/// module doesn't need `clap` in scope.
pub type Mode = SearchMode;

/// Dispatch one [`SymbolCommand`].
pub fn dispatch_symbol(
    cmd: &SymbolCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        SymbolCommand::Def(args) => symbol_def(args, project, renderer),
        SymbolCommand::Refs(args) => symbol_refs(args, project, renderer),
        SymbolCommand::Callers(args) => symbol_callers(args, project, renderer),
        SymbolCommand::Callees(args) => symbol_callees(args, project, renderer),
        SymbolCommand::Outline(args) => symbol_outline(args, project, renderer),
    }
}

/// `tm symbol def`
pub fn symbol_def(
    args: &SymbolQueryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;
    let from_path = args.from.as_ref().and_then(|p| p.to_str()).unwrap_or(".");

    let symbol_opt = code_intel.definition(&args.name, from_path)?;

    match symbol_opt {
        Some(sym) => {
            let info = SymbolInfo {
                name: sym.name.clone(),
                kind: format!("{:?}", sym.kind),
                path: sym.path.clone(),
                line_start: sym.range.line_start,
                line_end: sym.range.line_end,
            };

            let human = format!(
                "{} {} at {}:{}-{}",
                info.kind, info.name, info.path, info.line_start, info.line_end
            );

            renderer.emit(&info, &human)?;
        }
        None => {
            let json = serde_json::json!(null);
            renderer.emit(&json, "Symbol not found")?;
        }
    }

    Ok(())
}

/// `tm symbol refs`
pub fn symbol_refs(
    args: &SymbolQueryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;
    let from_path = args.from.as_ref().and_then(|p| p.to_str()).unwrap_or(".");

    let resolution = code_intel.resolve_symbol(&args.name, from_path)?;

    if let Some(sym_id) = resolution.symbol_id {
        let symbol_idx = code_intel.symbol_index()?;
        let references = symbol_idx.references(sym_id);

        let refs: Vec<ReferenceInfo> = references
            .iter()
            .map(|r| ReferenceInfo {
                path: r.path.clone(),
                line: r.range.line_start,
                col: r.range.byte_start as u32,
            })
            .collect();

        let human = if refs.is_empty() {
            "No references found".to_string()
        } else {
            let rows: Vec<Vec<String>> = refs
                .iter()
                .map(|r| vec![format!("{}:{}", r.path, r.line)])
                .collect();
            let table = Table::new(vec!["Location".to_string()], rows);
            table.render()
        };

        renderer.emit(&refs, &human)?;
    } else {
        let json = serde_json::json!(null);
        renderer.emit(&json, "Symbol not found")?;
    }

    Ok(())
}

/// `tm symbol callers`
pub fn symbol_callers(
    args: &SymbolQueryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;
    let from_path = args.from.as_ref().and_then(|p| p.to_str()).unwrap_or(".");

    let resolution = code_intel.resolve_symbol(&args.name, from_path)?;

    if let Some(sym_id) = resolution.symbol_id {
        let symbol_idx = code_intel.symbol_index()?;
        let callers = symbol_idx.callers(sym_id);

        let caller_info: Vec<ReferenceInfo> = callers
            .iter()
            .map(|s| ReferenceInfo {
                path: s.path.clone(),
                line: s.range.line_start,
                col: s.range.byte_start as u32,
            })
            .collect();

        let human = if caller_info.is_empty() {
            "No callers found".to_string()
        } else {
            let rows: Vec<Vec<String>> = caller_info
                .iter()
                .map(|c| vec![format!("{}:{}", c.path, c.line)])
                .collect();
            let table = Table::new(vec!["Location".to_string()], rows);
            table.render()
        };

        renderer.emit(&caller_info, &human)?;
    } else {
        let json = serde_json::json!(null);
        renderer.emit(&json, "Symbol not found")?;
    }

    Ok(())
}

/// `tm symbol callees`
pub fn symbol_callees(
    args: &SymbolQueryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;
    let from_path = args.from.as_ref().and_then(|p| p.to_str()).unwrap_or(".");

    let resolution = code_intel.resolve_symbol(&args.name, from_path)?;

    if let Some(sym_id) = resolution.symbol_id {
        let symbol_idx = code_intel.symbol_index()?;
        let callees = symbol_idx.callees(sym_id);

        let info: Vec<SymbolInfo> = callees
            .iter()
            .map(|s| SymbolInfo {
                name: s.name.clone(),
                kind: format!("{:?}", s.kind),
                path: s.path.clone(),
                line_start: s.range.line_start,
                line_end: s.range.line_end,
            })
            .collect();

        let human = if info.is_empty() {
            "No callees found".to_string()
        } else {
            let rows: Vec<Vec<String>> = info
                .iter()
                .map(|s| vec![format!("{}:{} ({})", s.name, s.line_start, s.kind)])
                .collect();
            let table = Table::new(vec!["Callee".to_string()], rows);
            table.render()
        };

        renderer.emit(&info, &human)?;
    } else {
        let json = serde_json::json!(null);
        renderer.emit(&json, "Symbol not found")?;
    }

    Ok(())
}

/// `tm symbol outline`
pub fn symbol_outline(
    args: &SymbolOutlineArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;
    let path_str = args.path.to_string_lossy();

    let entries = code_intel.outline(&path_str)?;

    let outlines: Vec<OutlineInfo> = entries
        .iter()
        .map(|e| OutlineInfo {
            depth: e.depth,
            rendered: e.rendered.clone(),
        })
        .collect();

    let human = if outlines.is_empty() {
        "No outline entries found".to_string()
    } else {
        let rows: Vec<Vec<String>> = outlines
            .iter()
            .map(|o| {
                let indent = "  ".repeat(o.depth as usize);
                vec![format!("{}{}", indent, o.rendered)]
            })
            .collect();
        let table = Table::new(vec!["Definition".to_string()], rows);
        table.render()
    };

    renderer.emit(&outlines, &human)?;

    Ok(())
}

/// Dispatch one [`HistoryCommand`].
pub fn dispatch_history(
    cmd: &HistoryCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        HistoryCommand::Why(args) => history_why(args, project, renderer),
        HistoryCommand::Search(args) => history_search(args, project, renderer),
        HistoryCommand::Deleted(args) => history_deleted(args, project, renderer),
    }
}

/// `tm history why <path>[:line]`
pub fn history_why(
    args: &HistoryWhyArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;

    let (path, line) = if let Some(idx) = args.locator.rfind(':') {
        let (p, l_str) = args.locator.split_at(idx);
        let line_num: u32 = l_str[1..].parse().map_err(|_| {
            tm_types::TmError::parse(format!(
                "\"{}\" isn't a valid path:line — try something like src/main.rs:42",
                args.locator
            ))
        })?;
        (p, line_num)
    } else {
        (&args.locator[..], 1)
    };

    let answer = code_intel.history_why(path, line, line)?;

    let commits: Vec<CommitInfo> = answer
        .commits
        .iter()
        .map(|c| CommitInfo {
            hash: c.sha.clone(),
            author: c.author.clone(),
            message: c.message.clone(),
        })
        .collect();

    let info = WhyInfo {
        path: answer.path.clone(),
        line_start: answer.line_start,
        line_end: answer.line_end,
        commits: commits.clone(),
    };

    let human = if commits.is_empty() {
        format!("No history found for {}:{}", path, line)
    } else {
        let mut text = format!("{}:{}\n\n", path, line);
        for commit in commits {
            text.push_str(&format!(
                "{} by {} - {}\n",
                &commit.hash[..8.min(commit.hash.len())],
                commit.author,
                commit.message
            ));
        }
        text
    };

    renderer.emit(&info, &human)?;

    Ok(())
}

/// `tm history search <query>`
pub fn history_search(
    args: &HistorySearchArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;

    let hits = code_intel.history_search(&args.query)?;

    let results: Vec<HistoryHitInfo> = hits
        .iter()
        .map(|h| HistoryHitInfo {
            commit: CommitInfo {
                hash: h.commit.sha.clone(),
                author: h.commit.author.clone(),
                message: h.commit.message.clone(),
            },
            path: h.path.clone(),
            snippet: h.snippet.clone(),
        })
        .collect();

    let human = if results.is_empty() {
        "No matching commits found".to_string()
    } else {
        let rows: Vec<Vec<String>> = results
            .iter()
            .map(|r| {
                vec![
                    format!(
                        "{} by {}",
                        &r.commit.hash[..8.min(r.commit.hash.len())],
                        r.commit.author
                    ),
                    r.commit.message.clone(),
                    r.path.as_deref().unwrap_or("(message)").to_string(),
                ]
            })
            .collect();
        let table = Table::new(
            vec![
                "Commit".to_string(),
                "Message".to_string(),
                "Path".to_string(),
            ],
            rows,
        );
        table.render()
    };

    renderer.emit(&results, &human)?;

    Ok(())
}

/// `tm history deleted <query>`
pub fn history_deleted(
    args: &HistorySearchArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let code_intel = project.code_intel()?;

    let deleted = code_intel.history_deleted(&args.query)?;

    let results: Vec<DeletedCodeInfo> = deleted
        .iter()
        .map(|d| DeletedCodeInfo {
            path: d.path.clone(),
            commit: CommitInfo {
                hash: d.commit.sha.clone(),
                author: d.commit.author.clone(),
                message: d.commit.message.clone(),
            },
            removed_text: d.removed_text.clone(),
        })
        .collect();

    let human = if results.is_empty() {
        "No deleted code found".to_string()
    } else {
        let rows: Vec<Vec<String>> = results
            .iter()
            .map(|r| {
                vec![
                    r.path.clone(),
                    format!(
                        "{} by {}",
                        &r.commit.hash[..8.min(r.commit.hash.len())],
                        r.commit.author
                    ),
                    r.removed_text.lines().next().unwrap_or("").to_string(),
                ]
            })
            .collect();
        let table = Table::new(
            vec![
                "Path".to_string(),
                "Removed in Commit".to_string(),
                "Snippet".to_string(),
            ],
            rows,
        );
        table.render()
    };

    renderer.emit(&results, &human)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_search_hit_serialization() {
        let hit = ExactSearchHit {
            path: "src/main.rs".to_string(),
            line: 42,
            col: 10,
            line_text: "    let x = 5;".to_string(),
        };
        let json = serde_json::to_string(&hit).expect("should serialize");
        assert!(json.contains("\"path\":\"src/main.rs\""));
        assert!(json.contains("\"line\":42"));
    }

    #[test]
    fn test_semantic_search_result_serialization() {
        let result = SemanticSearchResult {
            path: "src/lib.rs".to_string(),
            line_start: 10,
            line_end: 15,
            score: 0.95,
            text: "fn parse(input: &str) {}".to_string(),
        };
        let json = serde_json::to_string(&result).expect("should serialize");
        assert!(json.contains("\"score\":0.95"));
    }

    #[test]
    fn test_symbol_info_serialization() {
        let info = SymbolInfo {
            name: "main".to_string(),
            kind: "Function".to_string(),
            path: "src/main.rs".to_string(),
            line_start: 1,
            line_end: 10,
        };
        let json = serde_json::to_string(&info).expect("should serialize");
        assert!(json.contains("\"name\":\"main\""));
        assert!(json.contains("\"kind\":\"Function\""));
    }

    #[test]
    fn test_reference_info_serialization() {
        let ref_info = ReferenceInfo {
            path: "src/lib.rs".to_string(),
            line: 20,
            col: 5,
        };
        let json = serde_json::to_string(&ref_info).expect("should serialize");
        assert!(json.contains("\"line\":20"));
    }

    #[test]
    fn test_commit_info_serialization() {
        let commit = CommitInfo {
            hash: "abc123def456".to_string(),
            author: "Alice".to_string(),
            message: "Fix bug".to_string(),
        };
        let json = serde_json::to_string(&commit).expect("should serialize");
        assert!(json.contains("\"hash\":\"abc123def456\""));
        assert!(json.contains("\"author\":\"Alice\""));
    }

    #[test]
    fn test_history_hit_info_serialization() {
        let hit = HistoryHitInfo {
            commit: CommitInfo {
                hash: "abc123".to_string(),
                author: "Bob".to_string(),
                message: "Refactor".to_string(),
            },
            path: Some("src/lib.rs".to_string()),
            snippet: "fn helper() {}".to_string(),
        };
        let json = serde_json::to_string(&hit).expect("should serialize");
        assert!(json.contains("\"snippet\":\"fn helper() {}\""));
    }

    #[test]
    fn test_deleted_code_info_serialization() {
        let deleted = DeletedCodeInfo {
            path: "src/old.rs".to_string(),
            commit: CommitInfo {
                hash: "def789".to_string(),
                author: "Charlie".to_string(),
                message: "Remove unused".to_string(),
            },
            removed_text: "deprecated_function()".to_string(),
        };
        let json = serde_json::to_string(&deleted).expect("should serialize");
        assert!(json.contains("\"removed_text\":\"deprecated_function()\""));
    }

    #[test]
    fn test_why_info_serialization() {
        let why = WhyInfo {
            path: "src/main.rs".to_string(),
            line_start: 1,
            line_end: 1,
            commits: vec![CommitInfo {
                hash: "abc123".to_string(),
                author: "Alice".to_string(),
                message: "Initial commit".to_string(),
            }],
        };
        let json = serde_json::to_string(&why).expect("should serialize");
        assert!(json.contains("\"path\":\"src/main.rs\""));
        assert!(json.contains("\"line_start\":1"));
    }

    #[test]
    fn test_outline_info_serialization() {
        let outline = OutlineInfo {
            depth: 0,
            rendered: "fn main() -> Result<()>".to_string(),
        };
        let json = serde_json::to_string(&outline).expect("should serialize");
        assert!(json.contains("\"depth\":0"));
    }

    #[test]
    fn test_signal_breakdown_serialization() {
        let signal = SignalBreakdown {
            signal: "Semantic".to_string(),
            rank: Some(1),
            weighted_score: 0.8,
        };
        let json = serde_json::to_string(&signal).expect("should serialize");
        assert!(json.contains("\"rank\":1"));
    }

    #[test]
    fn test_hybrid_search_result_serialization() {
        let result = HybridSearchResult {
            path: "src/lib.rs".to_string(),
            line_start: Some(5),
            line_end: Some(10),
            snippet: "let x = important();".to_string(),
            fused_score: 0.92,
            explain: vec![SignalBreakdown {
                signal: "Lexical".to_string(),
                rank: Some(2),
                weighted_score: 0.3,
            }],
        };
        let json = serde_json::to_string(&result).expect("should serialize");
        assert!(json.contains("\"fused_score\":0.92"));
        // Verify snippet is included and non-empty in JSON
        assert!(json.contains("\"snippet\":\"let x = important();\""));
    }

    #[test]
    fn test_hybrid_search_result_snippet_non_empty() {
        // Test that hybrid search results properly include non-empty snippets
        let result = HybridSearchResult {
            path: "src/example.rs".to_string(),
            line_start: Some(10),
            line_end: Some(10),
            snippet: "fn calculate() -> i32 { 42 }".to_string(),
            fused_score: 0.85,
            explain: vec![SignalBreakdown {
                signal: "Semantic".to_string(),
                rank: Some(1),
                weighted_score: 0.85,
            }],
        };

        // Verify snippet is non-empty and serializable
        assert!(!result.snippet.is_empty());
        let json = serde_json::to_string(&result).expect("should serialize");
        assert!(json.contains("\"snippet\":\"fn calculate() -> i32 { 42 }\""));
    }
}
