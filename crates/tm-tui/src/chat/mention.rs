//! `@` file mentions: finding the `@token` under the cursor and ranking the project's files
//! against it, fuzzily, the way Claude Code's `@` autocomplete does.
//!
//! The file list itself is the application's to supply (it walks the project, honouring
//! `.gitignore`, off the render path) through a shared [`FileIndex`] that fills in once the walk
//! finishes; until then the popup says it is still indexing.

use std::sync::{Arc, OnceLock};

/// The project's files, relative to its root with `/` separators, filled in once by the
/// application.
pub type FileIndex = Arc<OnceLock<Vec<String>>>;

/// The `@` mention the cursor is in: the byte offset of its `@` and the text typed after it.
/// A mention starts the prompt or follows whitespace, and runs to the cursor.
pub fn token_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let start = before
        .rfind(char::is_whitespace)
        .map_or(0, |i| i + before[i..].chars().next().map_or(1, char::len_utf8));
    let token = &before[start..];
    let query = token.strip_prefix('@')?;
    Some((start, query))
}

/// How well `query` matches `path`, higher is better; `None` when it does not match at all.
///
/// Every query character must appear in order (case-insensitively). Matches score for landing in
/// the file name, at the start of a path segment or word, and in consecutive runs; shorter paths
/// win ties.
pub fn score(query: &str, path: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(-(path.len() as i64));
    }
    let lower = path.to_lowercase();
    let lower_path: Vec<char> = lower.chars().collect();
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let name_byte = lower.rfind('/').map_or(0, |i| i + 1);
    let name_start = lower[..name_byte].chars().count();
    let mut score: i64 = 0;
    let mut qi = 0;
    let mut prev_match: Option<usize> = None;
    for (pi, &c) in lower_path.iter().enumerate() {
        if qi == query.len() {
            break;
        }
        if c != query[qi] {
            continue;
        }
        score += 1;
        if pi >= name_start {
            score += 2;
        }
        let at_boundary = pi == 0 || matches!(lower_path[pi - 1], '/' | '_' | '-' | '.' | ' ');
        if at_boundary {
            score += 3;
        }
        if prev_match == Some(pi.wrapping_sub(1)) {
            score += 4;
        }
        prev_match = Some(pi);
        qi += 1;
    }
    if qi < query.len() {
        return None;
    }
    // A query that is a substring of the file name is almost certainly what was meant.
    let query_text: String = query.iter().collect();
    if lower[name_byte..].contains(&query_text) {
        score += 10;
    }
    Some(score * 1000 - path.len() as i64)
}

/// The best `limit` matches for `query` among `files`, best first.
pub fn rank<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    let mut scored: Vec<(i64, &str)> = files
        .iter()
        .filter_map(|f| score(query, f).map(|s| (s, f.as_str())))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, f)| f).collect()
}

/// What completing a mention inserts: `@path ` (quoted when the path has a space).
pub fn completion(path: &str) -> String {
    if path.contains(' ') {
        format!("@\"{path}\" ")
    } else {
        format!("@{path} ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_found_only_at_a_word_start() {
        assert_eq!(token_at("@src", 4), Some((0, "src")));
        assert_eq!(token_at("look at @src/ma", 15), Some((8, "src/ma")));
        assert_eq!(token_at("mail me@x.com", 13), None);
        assert_eq!(token_at("plain words", 11), None);
        assert_eq!(token_at("@", 1), Some((0, "")));
        assert_eq!(token_at("@a b", 4), None, "the cursor left the token");
    }

    #[test]
    fn ranking_prefers_file_names_and_contiguous_runs() {
        let files: Vec<String> = [
            "src/main.rs",
            "src/chat/input.rs",
            "docs/maintaining.md",
            "crates/tm-tui/src/chat/mention.rs",
            "Cargo.toml",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(rank(&files, "main", 2)[0], "src/main.rs");
        assert_eq!(rank(&files, "mention", 3), vec!["crates/tm-tui/src/chat/mention.rs"]);
        assert_eq!(rank(&files, "cargo", 1), vec!["Cargo.toml"]);
        assert!(rank(&files, "zzz", 5).is_empty());
        assert_eq!(rank(&files, "", 10).len(), 5, "an empty query lists files");
        assert_eq!(rank(&files, "sci", 5)[0], "src/chat/input.rs");
    }

    #[test]
    fn completions_quote_paths_with_spaces() {
        assert_eq!(completion("src/a.rs"), "@src/a.rs ");
        assert_eq!(completion("my notes.md"), "@\"my notes.md\" ");
    }
}
