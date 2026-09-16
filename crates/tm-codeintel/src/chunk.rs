//! Owns chunking: splitting a file's text into overlapping pieces sized for the embedder,
//! syntax-aware at function/class/impl boundaries when a tree-sitter grammar is available for
//! the file's [`crate::walk::Language`], falling back to a sliding window otherwise. Target
//! chunk size is 400 tokens (approximated by whitespace-delimited words; exactness is not the
//! point, boundedness is) with 15% overlap between adjacent chunks so a match spanning a
//! boundary is still findable.
//!
//! This module does not open the database or run tree-sitter itself for symbol *extraction*
//! (that's [`crate::symbols`]); it borrows symbol boundaries from a caller-supplied outline so
//! chunking and symbol extraction don't duplicate parsing.

use crate::walk::Language;

/// Target chunk size in whitespace-delimited words (a cheap proxy for tokens).
pub const TARGET_CHUNK_TOKENS: usize = 400;

/// Overlap between adjacent sliding-window chunks, as a fraction of [`TARGET_CHUNK_TOKENS`].
pub const OVERLAP_FRACTION: f32 = 0.15;

/// A byte-and-line span within one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRange {
    /// Start byte offset, inclusive.
    pub byte_start: usize,
    /// End byte offset, exclusive.
    pub byte_end: usize,
    /// Start line, 1-based, inclusive.
    pub line_start: u32,
    /// End line, 1-based, inclusive.
    pub line_end: u32,
}

/// One unit of chunked text, ready for embedding and storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Path of the owning file, relative to the project root.
    pub path: String,
    /// Location within the file.
    pub range: ChunkRange,
    /// The chunk's text.
    pub text: String,
    /// Name of the tightest enclosing symbol, if the chunk was cut at a syntax boundary
    /// rather than by the sliding-window fallback.
    pub symbol: Option<String>,
}

/// A boundary handed in by the caller (derived from [`crate::symbols::outline`]) that
/// syntax-aware chunking should prefer to split at, rather than mid-construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxBoundary {
    /// Start byte offset of the construct (function/class/impl/...), inclusive.
    pub byte_start: usize,
    /// End byte offset, exclusive.
    pub byte_end: usize,
    /// Name of the construct, used as the chunk's `symbol`.
    pub name: String,
}

/// Splits file text into [`Chunk`]s.
pub struct Chunker {
    target_tokens: usize,
    overlap_fraction: f32,
}

impl Chunker {
    /// A chunker using the crate defaults ([`TARGET_CHUNK_TOKENS`], [`OVERLAP_FRACTION`]).
    pub fn new() -> Self {
        Chunker {
            target_tokens: TARGET_CHUNK_TOKENS,
            overlap_fraction: OVERLAP_FRACTION,
        }
    }

    /// A chunker with explicit sizing, for tests and tuning.
    pub fn with_sizing(target_tokens: usize, overlap_fraction: f32) -> Self {
        Chunker {
            target_tokens,
            overlap_fraction,
        }
    }

    /// Chunk `text` (the full contents of `path`, a file of language `lang`).
    ///
    /// When `boundaries` is non-empty and `lang.has_grammar()`, splits are anchored at those
    /// boundaries (each becoming one or more chunks if the construct itself exceeds the
    /// target size, recursed via the sliding window); otherwise falls back to a uniform
    /// sliding window over the whole text.
    ///
    /// Sliding-window fallback: walk the target byte range splitting on whitespace-delimited
    /// word boundaries (`str::char_indices` + `is_whitespace`), group into windows of
    /// `target_tokens` words advancing by `target_tokens - overlap` words each step (at least
    /// one word of advance, to guarantee termination), each window's byte span being
    /// `[first_word.byte_start, last_word.byte_end)`; zero words in range yields no chunks.
    /// Line numbers come from counting `\n` bytes before a byte offset in `text` (1-based).
    pub fn chunk(
        &self,
        path: &str,
        text: &str,
        lang: Language,
        boundaries: &[SyntaxBoundary],
    ) -> Vec<Chunk> {
        if lang.has_grammar() && !boundaries.is_empty() {
            let mut sorted: Vec<&SyntaxBoundary> = boundaries.iter().collect();
            sorted.sort_by_key(|b| b.byte_start);

            let mut chunks = Vec::new();
            let mut cursor = 0usize;
            for boundary in &sorted {
                let start = boundary.byte_start.min(text.len());
                let end = boundary.byte_end.min(text.len());
                if start > cursor {
                    chunks.extend(self.sliding_window(path, text, cursor, start, None));
                }
                if end > start {
                    if self.word_count(&text[start..end]) <= self.target_tokens {
                        chunks.push(self.make_chunk(
                            path,
                            text,
                            start,
                            end,
                            Some(boundary.name.clone()),
                        ));
                    } else {
                        chunks.extend(self.sliding_window(
                            path,
                            text,
                            start,
                            end,
                            Some(boundary.name.clone()),
                        ));
                    }
                }
                cursor = cursor.max(end);
            }
            if cursor < text.len() {
                chunks.extend(self.sliding_window(path, text, cursor, text.len(), None));
            }
            chunks
        } else {
            self.sliding_window(path, text, 0, text.len(), None)
        }
    }

    /// Slides a fixed-size, overlapping window of whitespace-delimited words over
    /// `text[range_start..range_end]`, tagging every produced chunk with `symbol`.
    fn sliding_window(
        &self,
        path: &str,
        text: &str,
        range_start: usize,
        range_end: usize,
        symbol: Option<String>,
    ) -> Vec<Chunk> {
        let range_end = range_end.min(text.len());
        if range_start >= range_end {
            return Vec::new();
        }

        // Collect (byte_start, byte_end) of each whitespace-delimited word within the range.
        let slice = &text[range_start..range_end];
        let mut words: Vec<(usize, usize)> = Vec::new();
        let mut word_start: Option<usize> = None;
        for (idx, ch) in slice.char_indices() {
            let abs = range_start + idx;
            if ch.is_whitespace() {
                if let Some(ws) = word_start.take() {
                    words.push((ws, abs));
                }
            } else if word_start.is_none() {
                word_start = Some(abs);
            }
        }
        if let Some(ws) = word_start {
            words.push((ws, range_end));
        }

        if words.is_empty() {
            return Vec::new();
        }

        let overlap = ((self.target_tokens as f32) * self.overlap_fraction).round() as usize;
        let overlap = overlap.min(self.target_tokens.saturating_sub(1));
        let advance = self.target_tokens.saturating_sub(overlap).max(1);

        let mut chunks = Vec::new();
        let mut i = 0usize;
        while i < words.len() {
            let window_end_idx = (i + self.target_tokens).min(words.len());
            let byte_start = words[i].0;
            let byte_end = words[window_end_idx - 1].1;
            chunks.push(self.make_chunk(path, text, byte_start, byte_end, symbol.clone()));
            if window_end_idx >= words.len() {
                break;
            }
            i += advance;
        }
        chunks
    }

    /// Counts whitespace-delimited words in `s` (the token-count proxy used throughout).
    fn word_count(&self, s: &str) -> usize {
        s.split_whitespace().count()
    }

    /// Builds a [`Chunk`] for `text[byte_start..byte_end]`, deriving 1-based line numbers by
    /// counting newlines preceding each offset.
    fn make_chunk(
        &self,
        path: &str,
        text: &str,
        byte_start: usize,
        byte_end: usize,
        symbol: Option<String>,
    ) -> Chunk {
        let line_start = 1 + text.as_bytes()[..byte_start]
            .iter()
            .filter(|&&b| b == b'\n')
            .count() as u32;
        let line_end = 1 + text.as_bytes()[..byte_end]
            .iter()
            .filter(|&&b| b == b'\n')
            .count() as u32;
        Chunk {
            path: path.to_string(),
            range: ChunkRange {
                byte_start,
                byte_end,
                line_start,
                line_end,
            },
            text: text[byte_start..byte_end].to_string(),
            symbol,
        }
    }
}

impl Default for Chunker {
    fn default() -> Self {
        Chunker::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_yields_no_chunks() {
        let chunker = Chunker::new();
        let chunks = chunker.chunk("f.rs", "", Language::Rust, &[]);
        assert!(chunks.is_empty());
    }

    #[test]
    fn short_text_yields_single_chunk_covering_whole_text() {
        let chunker = Chunker::new();
        let text = "fn main() { println!(\"hi\"); }";
        let chunks = chunker.chunk("f.rs", text, Language::Rust, &[]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].range.byte_start, 0);
        assert_eq!(chunks[0].range.byte_end, text.len());
        assert_eq!(chunks[0].text, text);
        assert_eq!(chunks[0].symbol, None);
        assert_eq!(chunks[0].path, "f.rs");
    }

    #[test]
    fn sliding_window_overlaps_adjacent_chunks() {
        let chunker = Chunker::with_sizing(10, 0.2);
        let words: Vec<String> = (0..35).map(|i| format!("w{i}")).collect();
        let text = words.join(" ");
        let chunks = chunker.chunk("f.txt", &text, Language::Other, &[]);
        assert!(chunks.len() > 1);
        for pair in chunks.windows(2) {
            assert!(
                pair[1].range.byte_start < pair[0].range.byte_end,
                "adjacent chunks should overlap"
            );
            assert!(
                pair[1].range.byte_start > pair[0].range.byte_start,
                "windows should advance"
            );
        }
        // last chunk should reach the end of the text
        assert_eq!(chunks.last().unwrap().range.byte_end, text.len());
    }

    #[test]
    fn sliding_window_line_numbers_track_newlines() {
        let chunker = Chunker::with_sizing(3, 0.0);
        let text = "one two\nthree four\nfive six";
        let chunks = chunker.chunk("f.txt", text, Language::Other, &[]);
        assert_eq!(chunks[0].range.line_start, 1);
        assert!(chunks.last().unwrap().range.line_end >= chunks[0].range.line_start);
    }

    #[test]
    fn no_grammar_language_ignores_boundaries() {
        let chunker = Chunker::new();
        let text = "some plain text content here";
        let boundaries = vec![SyntaxBoundary {
            byte_start: 0,
            byte_end: 4,
            name: "x".into(),
        }];
        let chunks = chunker.chunk("f.md", text, Language::Other, &boundaries);
        // Other has no grammar, so boundaries are ignored entirely: whole-text sliding window.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].symbol, None);
    }

    #[test]
    fn grammar_language_with_no_boundaries_falls_back_to_sliding_window() {
        let chunker = Chunker::new();
        let text = "fn a() {}\nfn b() {}";
        let chunks = chunker.chunk("f.rs", text, Language::Rust, &[]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].symbol, None);
    }

    #[test]
    fn small_boundary_yields_single_chunk_tagged_with_symbol_name() {
        let chunker = Chunker::new();
        let text = "prefix stuff\nfn hello() { body(); }\nsuffix stuff";
        let start = text.find("fn hello").unwrap();
        let end = start + "fn hello() { body(); }".len();
        let boundaries = vec![SyntaxBoundary {
            byte_start: start,
            byte_end: end,
            name: "hello".into(),
        }];
        let chunks = chunker.chunk("f.rs", text, Language::Rust, &boundaries);

        // Expect a gap chunk before, the boundary chunk, and a gap chunk after.
        assert!(chunks.iter().any(|c| c.symbol.as_deref() == Some("hello")
            && c.range.byte_start == start
            && c.range.byte_end == end));
        // Chunks must be in ascending byte_start order.
        for pair in chunks.windows(2) {
            assert!(pair[0].range.byte_start <= pair[1].range.byte_start);
        }
    }

    #[test]
    fn oversized_boundary_is_sliding_windowed_but_keeps_symbol_name() {
        let chunker = Chunker::with_sizing(5, 0.2);
        let words: Vec<String> = (0..20).map(|i| format!("t{i}")).collect();
        let body = words.join(" ");
        let text = format!("fn big() {{ {body} }}");
        let start = 0usize;
        let end = text.len();
        let boundaries = vec![SyntaxBoundary {
            byte_start: start,
            byte_end: end,
            name: "big".into(),
        }];
        let chunks = chunker.chunk("f.rs", &text, Language::Rust, &boundaries);

        assert!(
            chunks.len() > 1,
            "oversized boundary should be split into multiple chunks"
        );
        assert!(chunks.iter().all(|c| c.symbol.as_deref() == Some("big")));
    }

    #[test]
    fn boundaries_are_sorted_before_processing() {
        let chunker = Chunker::new();
        let text = "fn a() { aa } fn b() { bb }";
        let a_start = text.find("fn a").unwrap();
        let a_end = text.find('}').unwrap() + 1;
        let b_start = text.find("fn b").unwrap();
        let b_end = text.rfind('}').unwrap() + 1;
        // Deliberately out of order.
        let boundaries = vec![
            SyntaxBoundary {
                byte_start: b_start,
                byte_end: b_end,
                name: "b".into(),
            },
            SyntaxBoundary {
                byte_start: a_start,
                byte_end: a_end,
                name: "a".into(),
            },
        ];
        let chunks = chunker.chunk("f.rs", text, Language::Rust, &boundaries);
        for pair in chunks.windows(2) {
            assert!(pair[0].range.byte_start <= pair[1].range.byte_start);
        }
        let names: Vec<Option<String>> = chunks.iter().map(|c| c.symbol.clone()).collect();
        assert!(names.contains(&Some("a".to_string())));
        assert!(names.contains(&Some("b".to_string())));
    }

    #[test]
    fn default_chunker_matches_new() {
        let a = Chunker::default();
        let b = Chunker::new();
        assert_eq!(a.target_tokens, b.target_tokens);
        assert_eq!(a.overlap_fraction, b.overlap_fraction);
    }
}
