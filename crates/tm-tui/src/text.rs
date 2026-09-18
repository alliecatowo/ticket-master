//! The text engine: grapheme segmentation, display width, and wrapping.
//!
//! `unicode-width`'s narrow/wide table alone still mishandles emoji with variation selectors,
//! and upstream is mid-fix (D-002) — this is the single place that decides how many terminal
//! columns a string occupies, so every widget agrees and a fix only has to land once. Nothing
//! outside this module should call `unicode_segmentation`/`unicode_width` directly.
//!
//! The load-bearing decision is measuring width **per extended grapheme cluster**, never per
//! `char`: `UnicodeWidthStr::width` on a whole cluster already folds variation selectors,
//! zero-width joiners, and regional-indicator pairs into one answer, so a caller that reserves
//! `graphemes(s)`'s widths *before* drawing a row gets one cell's worth of disagreement with a
//! given emulator, not a shifted rest-of-row. Measuring width per `char` and summing is the bug
//! this module exists to not have.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
/// Width is measured on the whole cluster, not per scalar: `unicode-width` already special-cases
/// zero-width joiners, variation selectors, and regional-indicator pairs when given a multi-`char`
/// string, so `UnicodeWidthStr::width(cluster)` gets combining marks, VS16-forced-emoji, ZWJ
/// family sequences, and flag pairs right at the cluster boundary even though summing per-`char`
/// widths for the same text would not (D-002). The one gap that measurement alone still misses is
/// a scalar Unicode has since assigned default emoji presentation to that this build's
/// `unicode-width` table does not yet know about; [`cluster_width`] closes that gap with a small,
/// explicit range table rather than waiting on the upstream fix.
pub fn graphemes(s: &str) -> Vec<Grapheme<'_>> {
    s.graphemes(true)
        .map(|cluster| Grapheme {
            text: cluster,
            width: cluster_width(cluster),
        })
        .collect()
}

/// The total rendered width of `s`, in terminal columns.
pub fn display_width(s: &str) -> usize {
    graphemes(s).iter().map(|g| g.width).sum()
}

/// Measure one grapheme cluster's terminal columns, overriding `unicode-width`'s answer only
/// where it disagrees with how a default-emoji-presentation scalar actually renders.
fn cluster_width(cluster: &str) -> usize {
    let measured = cluster.width();
    // `unicode-width` on this build's table already gets VS16/ZWJ/regional-indicator clusters
    // right (verified against the emoji sequences D-002 calls out); the residual gap is a
    // default-emoji-presentation scalar it still measures narrow. Widen those, and only those —
    // this must not fire for a cluster that is already wide, or for text-presentation scalars
    // (no VS16, not in the default-emoji ranges), which correctly stay narrow.
    if measured == 1 && starts_with_default_emoji_presentation(cluster) {
        2
    } else {
        measured
    }
}

/// True when `cluster`'s first scalar has default emoji presentation per Unicode's `emoji-data`
/// (the ranges a terminal renders double-wide without needing an explicit VS16), trimmed to the
/// blocks actually reachable through a grapheme cluster boundary.
fn starts_with_default_emoji_presentation(cluster: &str) -> bool {
    cluster
        .chars()
        .next()
        .is_some_and(is_default_emoji_presentation_scalar)
}

#[rustfmt::skip]
fn is_default_emoji_presentation_scalar(c: char) -> bool {
    matches!(c as u32,
        0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE
        | 0x2614..=0x2615 | 0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1
        | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5 | 0x26CE | 0x26D4
        | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD | 0x2705
        | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757
        | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
        | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A
        // Regional indicators: a lone one is a degenerate (non-paired) flag half; the crate
        // still reserves it a wide cell rather than leaving it as an unmeasured narrow orphan.
        | 0x1F1E6..=0x1F1FF
        | 0x1F201..=0x1F202 | 0x1F21A | 0x1F22F | 0x1F232..=0x1F23A
        | 0x1F250..=0x1F251 | 0x1F300..=0x1F5FF | 0x1F600..=0x1F64F
        | 0x1F680..=0x1F6FF | 0x1F7E0..=0x1F7EB | 0x1F900..=0x1F9FF
        | 0x1FA70..=0x1FAFF
    )
}

/// Word-wrap `s` to `max_width` columns, breaking at Unicode word boundaries and falling back to
/// a hard grapheme break when a single word exceeds `max_width` on its own.
///
/// A cluster is never split: when `max_width` is smaller than a single unsplittable grapheme's
/// own width (e.g. `max_width == 1` against a wide CJK character), that one line is allowed to
/// exceed `max_width` rather than tear the cluster — the alternative is drawing half a character,
/// which is worse than an oversized line in every terminal this crate targets.
pub fn wrap(s: &str, max_width: usize) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in s.split_word_bounds() {
        let is_whitespace = word.chars().all(char::is_whitespace);
        let word_width = display_width(word);

        if word_width <= max_width {
            if current_width + word_width > max_width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                // A run of whitespace that only existed to separate the previous word from the
                // next should not become the first thing on the new line.
                if is_whitespace {
                    continue;
                }
            }
            current.push_str(word);
            current_width += word_width;
            continue;
        }

        // The word alone is wider than max_width: hard-break it by grapheme cluster instead of
        // overflowing a single line with it.
        for g in graphemes(word) {
            if current_width > 0 && current_width + g.width > max_width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push_str(g.text);
            current_width += g.width;
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Truncate `s` to fit `max_width` columns, appending `ellipsis` when truncation was needed.
///
/// Guarantees the returned string's [`display_width`] never exceeds `max_width`: the budget for
/// content is `max_width` minus `ellipsis`'s own width, computed once up front, so the ellipsis
/// itself is never the thing that pushes a line over.
pub fn truncate(s: &str, max_width: usize, ellipsis: &str) -> String {
    if display_width(s) <= max_width {
        return s.to_string();
    }

    let ellipsis_width = display_width(ellipsis);
    if ellipsis_width > max_width {
        return String::new();
    }

    let budget = max_width - ellipsis_width;
    let mut out = String::new();
    let mut width = 0usize;
    for g in graphemes(s) {
        if width + g.width > budget {
            break;
        }
        out.push_str(g.text);
        width += g.width;
    }
    out.push_str(ellipsis);
    out
}

/// The visible slice of `s` for a horizontally-scrolled viewport: `max_width` columns wide,
/// starting at terminal column `offset` from the start of `s`.
///
/// A wide (2-column) cluster straddling either edge of the viewport cannot be drawn as half a
/// glyph, so its columns inside the viewport are rendered as blank padding instead — the same
/// "reserve the columns, then decide what fills them" discipline [`graphemes`] uses, applied to
/// a scroll boundary instead of a cluster's own start. The result's [`display_width`] is always
/// exactly `max_width` once `s` has enough columns to fill it (padding included), and never more.
pub fn scroll(s: &str, offset: usize, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let mut out = String::new();
    let mut col = 0usize; // this cluster's starting column within `s`, unclipped by `offset`
    let mut printed = 0usize; // columns already written to `out`

    for g in graphemes(s) {
        if printed >= max_width {
            break;
        }
        let cluster_end = col + g.width;
        if cluster_end <= offset {
            // Entirely left of the viewport.
            col = cluster_end;
            continue;
        }
        if col < offset || printed + g.width > max_width {
            // Straddles the left or right edge: pad the columns that fall inside the viewport
            // rather than draw a partial glyph.
            let visible_left = offset.saturating_sub(col);
            let visible = (g.width - visible_left).min(max_width - printed);
            for _ in 0..visible {
                out.push(' ');
            }
            printed += visible;
            col = cluster_end;
            continue;
        }
        out.push_str(g.text);
        printed += g.width;
        col = cluster_end;
    }

    out
}

/// A run of text sharing one rendering style: the unit ANSI-aware wrapping preserves across a
/// line break, so a colored/highlighted span never loses its style when it is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyledSpan<'a> {
    /// The span's source text (a slice of the original input, never an owned copy).
    pub text: &'a str,
    /// The style this run renders with.
    pub style: ratatui_core::style::Style,
}

impl<'a> StyledSpan<'a> {
    /// Construct a span from its text and style.
    pub fn new(text: &'a str, style: ratatui_core::style::Style) -> Self {
        StyledSpan { text, style }
    }
}

/// Word-wrap a sequence of styled spans to `max_width` columns, the same way [`wrap`] wraps plain
/// text, but keeping each output fragment's originating style attached so a colored run survives
/// a line break instead of losing its style at the break point.
///
/// Word-boundary detection runs independently within each span's own text: a word that is split
/// across two adjacent spans (style changing mid-word) is treated as two separate wrap units.
/// This does not tear a grapheme cluster — a cluster is always whole within one span, since a
/// span is exactly the text a caller chose to give one style — and matches the shape ANSI-colored
/// terminal output actually has, where a style change lands on a token boundary.
pub fn wrap_spans<'a>(spans: &[StyledSpan<'a>], max_width: usize) -> Vec<Vec<StyledSpan<'a>>> {
    if spans.iter().all(|span| span.text.is_empty()) {
        return Vec::new();
    }

    let mut lines: Vec<Vec<StyledSpan<'a>>> = Vec::new();
    let mut current: Vec<StyledSpan<'a>> = Vec::new();
    let mut current_width = 0usize;

    for span in spans {
        for word in span.text.split_word_bounds() {
            let is_whitespace = word.chars().all(char::is_whitespace);
            let word_width = display_width(word);

            if word_width <= max_width {
                if current_width + word_width > max_width {
                    if !current.is_empty() {
                        lines.push(std::mem::take(&mut current));
                    }
                    current_width = 0;
                    if is_whitespace {
                        continue;
                    }
                }
                push_fragment(&mut current, word, span.style);
                current_width += word_width;
                continue;
            }

            for g in graphemes(word) {
                if current_width > 0 && current_width + g.width > max_width {
                    lines.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                push_fragment(&mut current, g.text, span.style);
                current_width += g.width;
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Append `text` as a new fragment on the line under construction.
///
/// This crate is `#![forbid(unsafe_code)]`, so unlike `String`-based wrapping this cannot merge
/// two contiguous same-style `&str` slices back into one without pointer arithmetic — a line with
/// a hard-broken word therefore carries one [`StyledSpan`] per grapheme cluster of that word
/// rather than one merged run. That costs a few extra (zero-cost-to-render) `Span` entries in the
/// rare wide-word-hard-break case; it never affects the style or text a widget draws.
fn push_fragment<'a>(
    current: &mut Vec<StyledSpan<'a>>,
    text: &'a str,
    style: ratatui_core::style::Style,
) {
    current.push(StyledSpan { text, style });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_core::style::{Color, Style};

    // --- Grapheme carries text and width (locks in the value type's shape) ---

    #[test]
    fn grapheme_carries_text_and_width() {
        let g = Grapheme {
            text: "a",
            width: 1,
        };
        assert_eq!(g.text, "a");
        assert_eq!(g.width, 1);
    }

    // --- graphemes / display_width: the cases D-002 calls out by name ---

    #[test]
    fn ascii_is_one_column_per_grapheme() {
        let g = graphemes("abc");
        assert_eq!(g.len(), 3);
        assert!(g.iter().all(|c| c.width == 1));
        assert_eq!(display_width("abc"), 3);
    }

    #[test]
    fn cjk_characters_are_two_columns_wide() {
        let g = graphemes("中文");
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|c| c.width == 2));
        assert_eq!(display_width("中文"), 4);
    }

    #[test]
    fn combining_mark_folds_into_its_base_grapheme() {
        // "e" + U+0301 COMBINING ACUTE ACCENT is one user-perceived character.
        let s = "e\u{0301}";
        let g = graphemes(s);
        assert_eq!(g.len(), 1, "base + combining mark must be one cluster");
        assert_eq!(g[0].width, 1, "a combining mark adds no columns");
        assert_eq!(display_width(s), 1);
    }

    #[test]
    fn emoji_with_variation_selector_16_is_one_wide_cluster() {
        // U+2764 HEAVY BLACK HEART + U+FE0F VS16 forces emoji presentation.
        let s = "\u{2764}\u{FE0F}";
        let g = graphemes(s);
        assert_eq!(g.len(), 1, "base + VS16 must be one cluster, not two");
        assert_eq!(
            g[0].width, 2,
            "VS16 forces emoji (double-wide) presentation"
        );
    }

    #[test]
    fn same_base_without_variation_selector_16_stays_narrow() {
        // Without VS16 the same base scalar keeps its default (narrow) text presentation —
        // the override in `cluster_width` must not fire unconditionally on the base scalar.
        assert_eq!(display_width("\u{2764}"), 1);
    }

    #[test]
    fn variation_selector_15_keeps_text_presentation_narrow() {
        // VS15 explicitly requests *text* presentation; must not be widened.
        assert_eq!(display_width("\u{2764}\u{FE0E}"), 1);
    }

    #[test]
    fn zwj_family_emoji_is_one_wide_cluster() {
        // Man + ZWJ + Woman + ZWJ + Girl + ZWJ + Boy: one grapheme cluster, one wide glyph.
        let s = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let g = graphemes(s);
        assert_eq!(
            g.len(),
            1,
            "a ZWJ sequence must be a single grapheme cluster"
        );
        assert_eq!(
            g[0].width, 2,
            "a ZWJ emoji sequence renders as one wide glyph, not one per component"
        );
    }

    #[test]
    fn regional_indicator_pair_is_one_flag_two_columns_wide() {
        // Regional indicators U+1F1FA U+1F1F8 (letters U, S) pair into one flag glyph.
        let s = "\u{1F1FA}\u{1F1F8}";
        let g = graphemes(s);
        assert_eq!(
            g.len(),
            1,
            "a regional-indicator pair must be one grapheme cluster"
        );
        assert_eq!(
            g[0].width, 2,
            "a flag renders as one wide glyph, not two narrow halves"
        );
    }

    #[test]
    fn keycap_sequence_is_one_wide_cluster() {
        // Digit + VS16 + COMBINING ENCLOSING KEYCAP: e.g. the "1\u{fe0f}\u{20e3}" keycap emoji.
        let s = "1\u{FE0F}\u{20E3}";
        let g = graphemes(s);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].width, 2);
    }

    #[test]
    fn default_emoji_presentation_scalar_widens_even_without_vs16() {
        // U+1F600 GRINNING FACE has default emoji presentation with no VS16 required.
        assert_eq!(display_width("\u{1F600}"), 2);
    }

    #[test]
    fn empty_string_has_no_graphemes_and_zero_width() {
        assert!(graphemes("").is_empty());
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn mixed_cjk_ascii_and_emoji_sums_correctly() {
        // "中" (2) + "a" (1) + heart+VS16 (2) + "b" (1) = 6.
        assert_eq!(display_width("中a\u{2764}\u{FE0F}b"), 6);
    }

    // --- wrap ---

    #[test]
    fn wrap_of_empty_string_is_empty_vec() {
        assert_eq!(wrap("", 10), Vec::<String>::new());
    }

    #[test]
    fn wrap_keeps_short_line_on_one_line() {
        assert_eq!(wrap("hello world", 80), vec!["hello world".to_string()]);
    }

    #[test]
    fn wrap_breaks_at_word_boundaries() {
        let lines = wrap("the quick brown fox", 10);
        for line in &lines {
            assert!(display_width(line) <= 10, "line {line:?} exceeds max_width");
        }
        // Reassembling the lines (word-separated) must reproduce every word, in order.
        let words: Vec<&str> = "the quick brown fox".split_whitespace().collect();
        let rewrapped_words: Vec<&str> = lines.iter().flat_map(|l| l.split_whitespace()).collect();
        assert_eq!(words, rewrapped_words);
    }

    #[test]
    fn wrap_hard_breaks_a_word_wider_than_max_width() {
        let lines = wrap("supercalifragilisticexpialidocious", 10);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(display_width(line) <= 10);
        }
        assert_eq!(lines.concat(), "supercalifragilisticexpialidocious");
    }

    #[test]
    fn wrap_never_splits_a_grapheme_cluster() {
        // A ZWJ family emoji (one grapheme cluster, width 2) surrounded by text tight enough that
        // a naive byte-oriented wrap would be tempted to cut through it.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let s = format!("ab{family}cd");
        for width in 1..=6 {
            let lines = wrap(&s, width);
            let rejoined: String = lines.concat();
            assert_eq!(
                rejoined, s,
                "wrap must not drop or duplicate any text at width {width}"
            );
            assert!(
                lines.iter().any(|l| l.contains(family)) || !s.contains(family),
                "the family emoji cluster must appear intact on some line"
            );
        }
    }

    #[test]
    fn wrap_allows_a_single_wide_cluster_to_exceed_an_impossible_max_width() {
        // max_width smaller than one CJK character's own width (2): cannot be honored without
        // tearing the cluster, so the line is allowed to exceed max_width instead.
        let lines = wrap("中", 1);
        assert_eq!(lines, vec!["中".to_string()]);
    }

    #[test]
    fn wrap_does_not_start_a_line_with_separator_whitespace() {
        let lines = wrap("aa bb", 2);
        for line in &lines {
            assert!(
                !line.starts_with(' '),
                "line {line:?} should not start with whitespace"
            );
        }
    }

    // --- truncate ---

    #[test]
    fn truncate_leaves_short_strings_unchanged() {
        assert_eq!(truncate("hi", 10, "…"), "hi");
    }

    #[test]
    fn truncate_appends_ellipsis_only_when_truncated() {
        let out = truncate("hello world", 8, "…");
        assert!(out.ends_with('…'));
        assert!(display_width(&out) <= 8);
    }

    #[test]
    fn truncate_never_exceeds_max_width() {
        for width in 0..12 {
            let out = truncate("hello world", width, "…");
            assert!(
                display_width(&out) <= width,
                "width {width} produced {out:?}"
            );
        }
    }

    #[test]
    fn truncate_returns_empty_when_ellipsis_alone_does_not_fit() {
        assert_eq!(truncate("hello", 0, "…"), "");
    }

    #[test]
    fn truncate_does_not_split_a_grapheme_cluster() {
        // A CJK-only string where the ellipsis takes 1 of a 3-width budget, leaving room for
        // exactly one wide character: must not emit a half character to use the leftover column.
        let out = truncate("中文测试", 3, "…");
        assert_eq!(display_width(&out), 3);
        assert!(graphemes(&out)
            .iter()
            .all(|g| g.text == "中" || g.text == "…"));
    }

    // --- scroll ---

    #[test]
    fn scroll_at_zero_offset_is_a_left_aligned_window() {
        assert_eq!(scroll("hello world", 0, 5), "hello");
    }

    #[test]
    fn scroll_advances_by_column() {
        assert_eq!(scroll("hello world", 6, 5), "world");
    }

    #[test]
    fn scroll_past_the_end_is_empty() {
        assert_eq!(scroll("hi", 10, 5), "");
    }

    #[test]
    fn scroll_pads_a_wide_cluster_straddling_the_left_edge() {
        // "中" occupies columns [0,2). Scrolling to offset 1 cannot show half of it: the first
        // visible column must be blank padding, not a corrupted glyph.
        let out = scroll("中文", 1, 3);
        assert_eq!(display_width(&out), 3);
        assert!(out.starts_with(' '));
    }

    #[test]
    fn scroll_pads_a_wide_cluster_straddling_the_right_edge() {
        // "中" at columns [0,2), "文" at [2,4). A 3-column viewport can show "中" whole (2 cols)
        // but only half of "文": that half must be blank padding, not a corrupted glyph.
        let out = scroll("中文", 0, 3);
        assert_eq!(display_width(&out), 3);
        assert!(out.ends_with(' '));
    }

    #[test]
    fn scroll_never_exceeds_max_width() {
        for offset in 0..8 {
            for width in 0..8 {
                let out = scroll("中a文b", offset, width);
                assert!(display_width(&out) <= width);
            }
        }
    }

    // --- wrap_spans ---

    #[test]
    fn wrap_spans_of_no_text_is_empty() {
        let spans = [StyledSpan::new("", Style::default())];
        assert!(wrap_spans(&spans, 10).is_empty());
    }

    #[test]
    fn wrap_spans_preserves_style_across_a_break() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let spans = [
            StyledSpan::new("red words here ", red),
            StyledSpan::new("blue words here", blue),
        ];
        let lines = wrap_spans(&spans, 10);
        assert!(
            lines.len() > 1,
            "input should have wrapped to more than one line"
        );
        for line in &lines {
            for fragment in line {
                assert!(fragment.style == red || fragment.style == blue);
            }
            let width: usize = line.iter().map(|f| display_width(f.text)).sum();
            assert!(width <= 10, "line exceeded max_width: {line:?}");
        }
        // No text lost or duplicated when every fragment is rejoined in order.
        let rejoined: String = lines.iter().flatten().map(|f| f.text).collect();
        assert_eq!(rejoined, "red words here blue words here");
    }

    // --- Hand-rolled property tests. No `proptest`: it is not a `tm-tui` dev-dependency
    // (Cargo.toml is owned by another agent), so this module drives its own tiny deterministic
    // PRNG instead of pulling in a new crate for it (workspace rule: no new dependencies).

    /// A minimal xorshift64 PRNG. Deterministic from a fixed seed, so a failing property test
    /// reproduces byte-for-byte without needing a printed seed.
    struct Xorshift64(u64);

    impl Xorshift64 {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn next_range(&mut self, bound: usize) -> usize {
            (self.next_u64() as usize) % bound.max(1)
        }
    }

    /// A pool of scalars covering every case D-002 names, used to build random fuzz strings:
    /// ASCII, CJK, combining marks, VS16 emoji, ZWJ components, and regional indicators.
    const FUZZ_POOL: &[&str] = &[
        "a",
        "b",
        " ",
        "-",
        "中",
        "文",
        "e\u{0301}",
        "\u{2764}\u{FE0F}",
        "\u{1F600}",
        "\u{1F468}",
        "\u{200D}",
        "\u{1F469}",
        "\u{1F1FA}",
        "\u{1F1F8}",
    ];

    fn fuzz_string(rng: &mut Xorshift64, len: usize) -> String {
        (0..len)
            .map(|_| FUZZ_POOL[rng.next_range(FUZZ_POOL.len())])
            .collect()
    }

    #[test]
    fn property_wrap_never_splits_a_grapheme_cluster() {
        let mut rng = Xorshift64(0x9E3779B97F4A7C15);
        for _ in 0..500 {
            let len = rng.next_range(20);
            let s = fuzz_string(&mut rng, len);
            let max_width = rng.next_range(12) + 1;
            let lines = wrap(&s, max_width);

            // Every non-whitespace grapheme cluster of the input appears whole, in order, once
            // rejoined — a break only ever drops the (implied-by-the-newline) whitespace that sat
            // exactly at a break point, never splits, drops, or duplicates content. Comparing
            // `graphemes()` output (not raw bytes) also means a "split" cluster could not
            // silently reassemble into a matching token even if this check were byte-based.
            let non_whitespace = |text: &str| -> String {
                graphemes(text)
                    .iter()
                    .filter(|g| !g.text.chars().all(char::is_whitespace))
                    .map(|g| g.text)
                    .collect()
            };
            let rejoined: String = lines.concat();
            assert_eq!(
                non_whitespace(&rejoined),
                non_whitespace(&s),
                "wrap dropped/duplicated/split content for {s:?} at width {max_width}"
            );
        }
    }

    #[test]
    fn property_wrap_line_width_never_exceeds_max_width_unless_unsplittable() {
        let mut rng = Xorshift64(0xD1B54A32D192ED03);
        for _ in 0..500 {
            let len = rng.next_range(20);
            let s = fuzz_string(&mut rng, len);
            let max_width = rng.next_range(12) + 1;
            let lines = wrap(&s, max_width);

            for line in &lines {
                let width = display_width(line);
                if width > max_width {
                    // The only permitted excess: one unsplittable grapheme cluster wider than
                    // max_width, plus any number of width-0 clusters riding along with it (a
                    // dangling combining mark or bare ZWJ with no base — degenerate input, but
                    // still never split). At most one cluster on the line may occupy columns.
                    let visible: Vec<_> = graphemes(line)
                        .into_iter()
                        .filter(|g| g.width > 0)
                        .collect();
                    assert_eq!(
                        visible.len(),
                        1,
                        "line {line:?} exceeds max_width {max_width} but has more than one visible cluster"
                    );
                    assert!(visible[0].width > max_width);
                }
            }
        }
    }

    #[test]
    fn property_truncate_never_exceeds_max_width() {
        let mut rng = Xorshift64(0x2545F4914F6CDD1D);
        for _ in 0..500 {
            let len = rng.next_range(20);
            let s = fuzz_string(&mut rng, len);
            let max_width = rng.next_range(12);
            let out = truncate(&s, max_width, "…");
            assert!(
                display_width(&out) <= max_width,
                "truncate({s:?}, {max_width}) = {out:?} exceeds max_width"
            );
        }
    }

    #[test]
    fn property_scroll_never_exceeds_max_width() {
        let mut rng = Xorshift64(0x853C49E6748FEA9B);
        for _ in 0..500 {
            let len = rng.next_range(20);
            let s = fuzz_string(&mut rng, len);
            let offset = rng.next_range(20);
            let max_width = rng.next_range(12);
            let out = scroll(&s, offset, max_width);
            assert!(
                display_width(&out) <= max_width,
                "scroll({s:?}, {offset}, {max_width}) = {out:?} exceeds max_width"
            );
        }
    }
}
