//! The text engine: grapheme segmentation, display width, and wrapping.
//!
//! `unicode-width`'s narrow/wide table alone still mishandles emoji with variation selectors,
//! and upstream is mid-fix (D-002) — this is the single place that decides how many terminal
//! columns a string occupies, so every widget agrees and a fix only has to land once. Nothing
//! outside this module should call `unicode_segmentation`/`unicode_width` directly.

// IMPL: implementations in this module will need
// `use unicode_segmentation::UnicodeSegmentation;` and `use unicode_width::UnicodeWidthStr;`.
// Left out at the top level for now so an unfinished `todo!()` body does not warn about unused
// imports; add them back when `graphemes` is implemented.

/// One user-perceived character (an extended grapheme cluster) plus the terminal columns it
/// occupies once rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grapheme<'a> {
    /// The cluster's source text.
    pub text: &'a str,
    /// How many terminal columns this cluster occupies (0, 1, or 2).
    pub width: usize,
}

/// Split `s` into extended grapheme clusters, each with its rendered width, correcting
/// `unicode-width`'s known misses for emoji-with-variation-selector sequences.
///
/// IMPL: start from `s.graphemes(true)` (`UnicodeSegmentation`, extended clusters — `true`
/// requests the extended, not legacy, algorithm). For each cluster, compute
/// `UnicodeWidthStr::width(cluster)`. Clusters already fold combining marks and variation
/// selectors into a single unit, so `unicode-width` gets most emoji sequences right at the
/// cluster level even though it is unreliable per-scalar (D-002). Where a cluster begins with a
/// scalar in the emoji-presentation ranges and `unicode-width` reports width 1 for the whole
/// cluster, override to width 2 — emoji render double-wide in every terminal this crate targets.
/// Keep the override as a small table/range check here rather than patching `unicode-width`
/// itself; its upstream fix is in progress per D-002 and this module is exactly the seam meant
/// to absorb the gap until it lands. Add a `#[cfg(test)]` case per emulator-observed mismatch as
/// they are found, per D-002's "cross-emulator test matrix rather than trust".
pub fn graphemes(s: &str) -> Vec<Grapheme<'_>> {
    let _ = s;
    todo!("segment `s` into graphemes and width each cluster per the IMPL note above")
}

/// The total rendered width of `s`, in terminal columns.
pub fn display_width(s: &str) -> usize {
    graphemes(s).iter().map(|g| g.width).sum()
}

/// Word-wrap `s` to `max_width` columns, breaking at Unicode word boundaries and falling back to
/// a hard grapheme break when a single word exceeds `max_width` on its own.
///
/// IMPL: use `unicode_segmentation::UnicodeSegmentation::split_word_bounds` on `s` for candidate
/// break points; accumulate `display_width` per line, starting a new line at the last valid break
/// point once the next word would exceed `max_width`. When a single word's own width exceeds
/// `max_width`, hard-break it by walking `graphemes(word)` instead of overflowing the line. An
/// empty `s` returns an empty `Vec`, not a `Vec` with one empty line.
pub fn wrap(s: &str, max_width: usize) -> Vec<String> {
    let _ = (s, max_width);
    todo!("wrap `s` at `max_width` columns per the IMPL note above")
}

/// Truncate `s` to fit `max_width` columns, appending `ellipsis` when truncation was needed.
///
/// IMPL: walk `graphemes(s)` accumulating width; stop accepting clusters once the running total
/// plus `display_width(ellipsis)` would exceed `max_width`, then append `ellipsis`. If nothing
/// was truncated (the whole string already fits), return `s.to_string()` unchanged — do not
/// append `ellipsis` to a string that fit. If `max_width` is too small to fit even `ellipsis`,
/// return an empty string rather than panicking or overflowing.
pub fn truncate(s: &str, max_width: usize, ellipsis: &str) -> String {
    let _ = (s, max_width, ellipsis);
    todo!("truncate `s` to `max_width` columns per the IMPL note above")
}

#[cfg(test)]
mod tests {
    use super::*;

    // `graphemes`/`wrap`/`truncate` are todo!() until the implementing agent fills them in
    // (see the IMPL notes above); this only locks in the `Grapheme` value type's shape so
    // widget code written against it compiles today. Behavioural coverage — including the
    // cross-emulator width matrix D-002 asks for — belongs to that implementation.
    #[test]
    fn grapheme_carries_text_and_width() {
        let g = Grapheme { text: "a", width: 1 };
        assert_eq!(g.text, "a");
        assert_eq!(g.width, 1);
    }
}
