//! Owns exact retrieval: literal and regex search over the walked file set. This is the route
//! for a known identifier or error string — streaming, ignore-aware, and bounded so a query
//! that matches everywhere doesn't exhaust memory.
//!
//! This module walks files itself (via [`crate::walk::RepoWalker`]) rather than reading from
//! `index.db`, so exact search is always current even for files not yet (re)chunked/embedded.

use regex::Regex;
use tm_types::Result;

use crate::walk::RepoWalker;

/// One match location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Path of the matching file, relative to the project root.
    pub path: String,
    /// 1-based line number.
    pub line: u32,
    /// 0-based column (byte offset within the line) of the match start.
    pub col: u32,
    /// The full text of the matching line, for display without a re-read.
    pub line_text: String,
    /// Byte offset range of the match within the file.
    pub byte_range: (usize, usize),
}

/// Upper bound on hits returned by a single search call, past which
/// [`ExactSearchResult::truncated`] is set instead of continuing to scan.
pub const DEFAULT_HIT_CAP: usize = 1000;

/// The outcome of an exact or regex search: hits plus whether the cap was hit before the walk
/// finished.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExactSearchResult {
    /// Matches found, in file-then-line order, up to the configured cap.
    pub hits: Vec<Hit>,
    /// True if the search stopped early because `hits.len()` reached the cap; callers should
    /// treat the result as a sample, not an exhaustive list.
    pub truncated: bool,
}

/// Streaming literal and regex search over a project's walked file set.
pub struct ExactSearch {
    root: std::path::PathBuf,
    hit_cap: usize,
}

impl ExactSearch {
    /// A searcher rooted at `root`, using [`DEFAULT_HIT_CAP`].
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        ExactSearch {
            root: root.into(),
            hit_cap: DEFAULT_HIT_CAP,
        }
    }

    /// A searcher with an explicit hit cap, for tests and callers needing exhaustive results
    /// on a small tree.
    pub fn with_hit_cap(root: impl Into<std::path::PathBuf>, hit_cap: usize) -> Self {
        ExactSearch {
            root: root.into(),
            hit_cap,
        }
    }

    /// Search for the literal substring `needle` (case-sensitive) across every ignore-aware
    /// walked file, streaming line by line so memory use is bounded by one file's line at a
    /// time, not the whole tree.
    pub fn literal(&self, needle: &str) -> Result<ExactSearchResult> {
        // Escape the needle as a regex literal to use the same matching logic.
        let pattern = regex::escape(needle);
        let re = Regex::new(&pattern).map_err(|e| {
            tm_types::TmError::parse(format!("failed to create literal regex: {}", e))
        })?;

        self.search_with_pattern(&re)
    }

    /// Search for lines matching the regex `pattern` across every ignore-aware walked file,
    /// same streaming/cap/ordering contract as [`ExactSearch::literal`].
    pub fn regex(&self, pattern: &str) -> Result<ExactSearchResult> {
        // Compile the regex pattern, mapping compile error to TmError::parse.
        let re = Regex::new(pattern)
            .map_err(|e| tm_types::TmError::parse(format!("invalid regex: {}", e)))?;

        self.search_with_pattern(&re)
    }

    /// Internal helper that executes the search with a compiled regex pattern.
    fn search_with_pattern(&self, re: &Regex) -> Result<ExactSearchResult> {
        let mut result = ExactSearchResult::default();

        // Walk files in sorted order via RepoWalker.
        let walker = RepoWalker::new(&self.root);
        let files = walker.walk()?;

        for file_record in files {
            // Stop early if we've reached the hit cap.
            if result.hits.len() >= self.hit_cap {
                result.truncated = true;
                break;
            }

            // Read the file content, skip on read error and log warning.
            let content = match std::fs::read_to_string(self.root.join(&file_record.path)) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("failed to read file {}: {}", file_record.path, e);
                    continue;
                }
            };

            // Split into lines with byte offsets and search each line.
            let lines = Self::lines_with_offsets(&content);

            for (line_num, (line_byte_start, line_text)) in lines.iter().enumerate() {
                // Stop early if we've reached the hit cap.
                if result.hits.len() >= self.hit_cap {
                    result.truncated = true;
                    break;
                }

                // Find all matches in this line.
                let matches = matches_in_line(re, line_text);
                for (col, end_col) in matches {
                    // Stop early if we've reached the hit cap.
                    if result.hits.len() >= self.hit_cap {
                        result.truncated = true;
                        break;
                    }

                    // Calculate byte range of the match within the entire file.
                    let byte_range_start = line_byte_start + col;
                    let byte_range_end = line_byte_start + end_col;

                    result.hits.push(Hit {
                        path: file_record.path.clone(),
                        line: (line_num + 1) as u32,
                        col: col as u32,
                        line_text: line_text.to_string(),
                        byte_range: (byte_range_start, byte_range_end),
                    });
                }

                if result.truncated {
                    break;
                }
            }

            if result.truncated {
                break;
            }
        }

        Ok(result)
    }

    /// Shared line-streaming core: split `content` into (byte_start, line_text) pairs without
    /// allocating more than one file's worth of lines at a time.
    fn lines_with_offsets(content: &str) -> Vec<(usize, &str)> {
        let mut lines = Vec::new();
        let bytes = content.as_bytes();
        let mut line_start = 0;

        // Iterate through bytes and split on newlines while tracking byte offsets.
        for (byte_idx, &byte) in bytes.iter().enumerate() {
            if byte == b'\n' {
                // Extract the line content and trim trailing \r for Windows line endings.
                let line_content = &content[line_start..byte_idx];
                let line_text = line_content.trim_end_matches('\r');
                lines.push((line_start, line_text));
                line_start = byte_idx + 1;
            }
        }

        // Add the last line if there's remaining content after the last newline.
        if line_start < content.len() {
            let line_content = &content[line_start..];
            let line_text = line_content.trim_end_matches('\r');
            lines.push((line_start, line_text));
        }

        lines
    }
}

/// Match a compiled pattern against `text`, used identically by both [`ExactSearch::literal`]
/// (via a trivial escaped-literal regex) and [`ExactSearch::regex`] — kept as a free function
/// so both paths share one matching/hit-construction code path instead of duplicating it.
fn matches_in_line(re: &Regex, line: &str) -> Vec<(usize, usize)> {
    re.find_iter(line).map(|m| (m.start(), m.end())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_with_offsets_single_line() {
        let content = "hello world";
        let lines = ExactSearch::lines_with_offsets(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], (0, "hello world"));
    }

    #[test]
    fn lines_with_offsets_multiple_lines() {
        let content = "hello\nworld\nfoo";
        let lines = ExactSearch::lines_with_offsets(content);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], (0, "hello"));
        assert_eq!(lines[1], (6, "world"));
        assert_eq!(lines[2], (12, "foo"));
    }

    #[test]
    fn lines_with_offsets_empty_string() {
        let content = "";
        let lines = ExactSearch::lines_with_offsets(content);
        assert_eq!(lines.len(), 0);
    }

    #[test]
    fn lines_with_offsets_empty_lines() {
        let content = "hello\n\nworld";
        let lines = ExactSearch::lines_with_offsets(content);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], (0, "hello"));
        assert_eq!(lines[1], (6, ""));
        assert_eq!(lines[2], (7, "world"));
    }

    #[test]
    fn lines_with_offsets_trailing_newline() {
        let content = "hello\nworld\n";
        let lines = ExactSearch::lines_with_offsets(content);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], (0, "hello"));
        assert_eq!(lines[1], (6, "world"));
    }

    #[test]
    fn lines_with_offsets_carriage_return() {
        let content = "hello\r\nworld\r\n";
        let lines = ExactSearch::lines_with_offsets(content);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], (0, "hello"));
        assert_eq!(lines[1], (7, "world"));
    }

    #[test]
    fn literal_simple_match() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "hello world\nhello again").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("hello").unwrap();

        assert_eq!(result.hits.len(), 2);
        assert!(!result.truncated);
        assert_eq!(result.hits[0].path, "test.txt");
        assert_eq!(result.hits[0].line, 1);
        assert_eq!(result.hits[0].col, 0);
        assert_eq!(result.hits[0].line_text, "hello world");
        assert_eq!(result.hits[1].line, 2);
        assert_eq!(result.hits[1].col, 0);
    }

    #[test]
    fn literal_no_match() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "hello world").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("xyz").unwrap();

        assert_eq!(result.hits.len(), 0);
        assert!(!result.truncated);
    }

    #[test]
    fn literal_multiple_per_line() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "foo foo foo").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("foo").unwrap();

        assert_eq!(result.hits.len(), 3);
        assert_eq!(result.hits[0].col, 0);
        assert_eq!(result.hits[1].col, 4);
        assert_eq!(result.hits[2].col, 8);
    }

    #[test]
    fn literal_byte_ranges() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "hello world").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("world").unwrap();

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].byte_range, (6, 11));
    }

    #[test]
    fn literal_case_sensitive() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "Hello hello HELLO").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("hello").unwrap();

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].col, 6);
    }

    #[test]
    fn regex_simple_match() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "hello123\nworld456").unwrap();

        let search = ExactSearch::new(root);
        let result = search.regex(r"\d+").unwrap();

        assert_eq!(result.hits.len(), 2);
        assert!(!result.truncated);
        assert_eq!(result.hits[0].line, 1);
        assert_eq!(result.hits[0].col, 5);
        assert_eq!(result.hits[1].line, 2);
        assert_eq!(result.hits[1].col, 5);
    }

    #[test]
    fn regex_invalid_pattern() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "hello").unwrap();

        let search = ExactSearch::new(root);
        let result = search.regex("[invalid");

        assert!(result.is_err());
    }

    #[test]
    fn regex_complex_pattern() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "fn main() { }\nfn test() { }").unwrap();

        let search = ExactSearch::new(root);
        let result = search.regex(r"fn\s+\w+").unwrap();

        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.hits[0].col, 0);
        assert_eq!(result.hits[1].col, 0);
    }

    #[test]
    fn hit_cap_truncation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create a file with many matches.
        let mut content = String::new();
        for i in 0..20 {
            content.push_str(&format!("match {} hello\n", i));
        }
        std::fs::write(root.join("test.txt"), content).unwrap();

        // Use a small cap.
        let search = ExactSearch::with_hit_cap(root, 10);
        let result = search.literal("hello").unwrap();

        assert_eq!(result.hits.len(), 10);
        assert!(result.truncated);
    }

    #[test]
    fn multiple_files_sorted_order() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("b.txt"), "match").unwrap();
        std::fs::write(root.join("a.txt"), "match").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("match").unwrap();

        assert_eq!(result.hits.len(), 2);
        // Files should be in sorted order: a.txt comes before b.txt
        assert_eq!(result.hits[0].path, "a.txt");
        assert_eq!(result.hits[1].path, "b.txt");
    }

    #[test]
    fn read_error_skipped() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create one readable file and then try to create an unreadable one
        // (this is difficult on all platforms, so we test that readable files are found)
        std::fs::write(root.join("readable.txt"), "match").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("match").unwrap();

        // Should find at least the readable file, and not crash on any unreadable files
        assert!(!result.hits.is_empty());
    }

    #[test]
    fn empty_needle_matches_all_positions() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "hi").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("").unwrap();

        // Empty string matches at positions 0, 1, and 2 (before/after each char and at end)
        assert!(!result.hits.is_empty());
    }

    #[test]
    fn matches_in_line_basic() {
        let re = Regex::new("test").unwrap();
        let matches = matches_in_line(&re, "this is a test string");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], (10, 14));
    }

    #[test]
    fn matches_in_line_multiple() {
        let re = Regex::new("a").unwrap();
        let matches = matches_in_line(&re, "banana");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].0, 1);
        assert_eq!(matches[1].0, 3);
        assert_eq!(matches[2].0, 5);
    }

    #[test]
    fn matches_in_line_no_match() {
        let re = Regex::new("xyz").unwrap();
        let matches = matches_in_line(&re, "hello world");
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn newline_column_tracking() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("test.txt"), "abc\ndefg").unwrap();

        let search = ExactSearch::new(root);
        let result = search.literal("fg").unwrap();

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].line, 2);
        assert_eq!(result.hits[0].col, 2);
        // "abc\ndefg": line 2 ("defg") starts at byte 4, and "fg" starts at byte 6.
        assert_eq!(result.hits[0].byte_range, (6, 8));
    }
}
