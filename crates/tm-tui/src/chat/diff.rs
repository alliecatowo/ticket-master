//! Unified diffs, reduced to numbered lines the transcript can draw the way Claude Code draws an
//! edit: context lines plain, removed lines on a red band, added lines on a green band, each with
//! the line number it has in the file (the old number for a removal, the new one otherwise).
//!
//! Parsing is driven by each hunk header's line counts rather than by the `+`/`-` prefix alone,
//! so a removed line whose text itself starts with `--` is never mistaken for a file header.

/// What a diff line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// Unchanged context.
    Context,
    /// Added in the new version.
    Added,
    /// Removed from the old version.
    Removed,
    /// Lines skipped between two hunks.
    Gap,
}

/// One displayable diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// What it is.
    pub kind: DiffLineKind,
    /// Its line number: the old file's for a removal, the new file's otherwise. `None` for a gap.
    pub number: Option<usize>,
    /// The line's text, without the `+`/`-`/` ` marker.
    pub text: String,
}

impl DiffLine {
    fn new(kind: DiffLineKind, number: Option<usize>, text: &str) -> Self {
        DiffLine {
            kind,
            number,
            text: text.to_string(),
        }
    }
}

/// `@@ -12,3 +12,4 @@` → `(12, 3, 12, 4)`. A missing count means 1.
fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(usize, usize)> {
        match r.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some((old_start, old_count, new_start, new_count))
}

/// Parse `diff` (unified format, any number of hunks, with or without `---`/`+++` headers) into
/// display lines, with a [`DiffLineKind::Gap`] between hunks. Text that is not a diff yields
/// nothing.
pub fn parse_unified(diff: &str) -> Vec<DiffLine> {
    let mut out = Vec::new();
    let mut lines = diff.lines();
    while let Some(line) = lines.next() {
        let Some((old_start, old_count, new_start, new_count)) = parse_hunk_header(line) else {
            continue;
        };
        if !out.is_empty() {
            out.push(DiffLine::new(DiffLineKind::Gap, None, ""));
        }
        let (mut old_left, mut new_left) = (old_count, new_count);
        let (mut old_no, mut new_no) = (old_start, new_start);
        while old_left > 0 || new_left > 0 {
            let Some(body) = lines.next() else {
                break;
            };
            if body.starts_with('\\') {
                // "\ No newline at end of file".
                continue;
            }
            let (marker, text) = match body.char_indices().nth(1) {
                Some((i, _)) => (&body[..i], &body[i..]),
                None => (body, ""),
            };
            match marker {
                "-" if old_left > 0 => {
                    out.push(DiffLine::new(DiffLineKind::Removed, Some(old_no), text));
                    old_no += 1;
                    old_left -= 1;
                }
                "+" if new_left > 0 => {
                    out.push(DiffLine::new(DiffLineKind::Added, Some(new_no), text));
                    new_no += 1;
                    new_left -= 1;
                }
                _ => {
                    // Context (a leading space, or an empty line some tools emit for an empty
                    // context line).
                    out.push(DiffLine::new(DiffLineKind::Context, Some(new_no), text));
                    old_no += 1;
                    new_no += 1;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
        }
    }
    out
}

/// `(additions, removals)` in `lines`.
pub fn counts(lines: &[DiffLine]) -> (usize, usize) {
    lines.iter().fold((0, 0), |(a, r), l| match l.kind {
        DiffLineKind::Added => (a + 1, r),
        DiffLineKind::Removed => (a, r + 1),
        _ => (a, r),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "--- a/calc.py\n+++ b/calc.py\n@@ -1,4 +1,6 @@\n def div(a, b):\n-    return a / b\n+    if b == 0:\n+        raise ValueError(\"no\")\n+    return a / b\n \n x = 1\n@@ -20,2 +22,2 @@\n----old\n+new\n ctx\n";

    #[test]
    fn hunks_become_numbered_lines_with_a_gap_between() {
        let lines = parse_unified(DIFF);
        let shown: Vec<(DiffLineKind, Option<usize>, &str)> = lines
            .iter()
            .map(|l| (l.kind, l.number, l.text.as_str()))
            .collect();
        use DiffLineKind::*;
        assert_eq!(
            shown,
            vec![
                (Context, Some(1), "def div(a, b):"),
                (Removed, Some(2), "    return a / b"),
                (Added, Some(2), "    if b == 0:"),
                (Added, Some(3), "        raise ValueError(\"no\")"),
                (Added, Some(4), "    return a / b"),
                (Context, Some(5), ""),
                (Context, Some(6), "x = 1"),
                (Gap, None, ""),
                (Removed, Some(20), "---old"),
                (Added, Some(22), "new"),
                (Context, Some(23), "ctx"),
            ]
        );
        assert_eq!(counts(&lines), (4, 2));
    }

    #[test]
    fn a_created_file_is_all_additions() {
        let lines = parse_unified("--- a/x\n+++ b/x\n@@ -0,0 +1,2 @@\n+one\n+two\n");
        assert_eq!(counts(&lines), (2, 0));
        assert_eq!(lines[1].number, Some(2));
    }

    #[test]
    fn non_diff_text_yields_nothing_and_truncated_hunks_do_not_panic() {
        assert!(parse_unified("hello\nworld").is_empty());
        let lines = parse_unified("@@ -1,5 +1,5 @@\n a\n");
        assert_eq!(lines.len(), 1);
        assert!(parse_unified("@@ garbage @@\n+x").is_empty());
    }

    #[test]
    fn no_newline_markers_are_skipped() {
        let lines = parse_unified("@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n");
        assert_eq!(counts(&lines), (1, 1));
    }
}
