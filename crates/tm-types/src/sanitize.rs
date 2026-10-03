//! Make untrusted text safe to put into a terminal cell.
//!
//! Tool output (a test runner's colored progress, a `curl` progress bar, a binary file read as
//! lossy UTF-8) and even model text can carry ANSI escape sequences and control characters. Written
//! verbatim into a ratatui `Buffer`, an ESC byte is passed straight through to the real terminal on
//! flush, where it can recolor, move the cursor, clear the screen, or retitle the window — the
//! frame corrupts and the diff renderer's idea of what is on screen stops being true. Everything
//! the chat views display from outside this process goes through [`sanitize`] first.

/// Strip escape sequences and control characters from `input`, keeping only printable text and
/// newlines.
///
/// - CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL|ST`), DCS/SOS/PM/APC strings, and two-byte
///   escapes are removed entirely, as are 8-bit C1 controls.
/// - A tab becomes four spaces (the chat views have no tab stops).
/// - `\r\n` is a newline; a lone `\r` (a progress bar redrawing its own line) keeps only what was
///   drawn after the last carriage return on that line, which is what a terminal would show.
/// - Every other C0 control, DEL, and the bidirectional-override characters (which can make a
///   displayed command read differently from what actually ran) are dropped.
pub fn sanitize(input: &str) -> String {
    let stripped = strip_escapes(input);
    let mut out = String::with_capacity(stripped.len());
    for (i, line) in stripped.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let visible = match line.rfind('\r') {
            Some(pos) => &line[pos + 1..],
            None => line,
        };
        for c in visible.chars() {
            match c {
                '\t' => out.push_str("    "),
                c if is_dropped(c) => {}
                c => out.push(c),
            }
        }
    }
    out
}

/// True for a character that must never reach a cell (after escapes are already gone).
fn is_dropped(c: char) -> bool {
    let code = c as u32;
    (code < 0x20 && c != '\n' && c != '\r')
        || code == 0x7f
        || (0x80..=0x9f).contains(&code)
        // Bidirectional embeddings/overrides/isolates.
        || (0x202a..=0x202e).contains(&code)
        || (0x2066..=0x2069).contains(&code)
}

/// Remove every ESC-introduced sequence, leaving other characters (including `\r`) untouched for
/// [`sanitize`]'s line pass.
fn strip_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' && c != '\u{9b}' {
            out.push(c);
            continue;
        }
        // 8-bit CSI behaves like `ESC [`.
        let kind = if c == '\u{9b}' {
            Some('[')
        } else {
            chars.next()
        };
        match kind {
            Some('[') => {
                // Parameters and intermediates, then one final byte in 0x40..=0x7e.
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                // A string terminated by BEL or ST (`ESC \`).
                while let Some(next) = chars.next() {
                    if next == '\u{7}' || next == '\u{9c}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // A two-byte escape (`ESC 7`, `ESC c`, ...) or a trailing lone ESC: drop it.
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(sanitize("hello world\nsecond"), "hello world\nsecond");
    }

    #[test]
    fn sgr_color_codes_are_removed() {
        assert_eq!(sanitize("\x1b[31mFAIL\x1b[0m test_div"), "FAIL test_div");
    }

    #[test]
    fn cursor_movement_and_clear_screen_are_removed() {
        assert_eq!(sanitize("a\x1b[2J\x1b[H\x1b[10;20Hb"), "ab");
    }

    #[test]
    fn osc_title_and_hyperlinks_are_removed() {
        assert_eq!(sanitize("\x1b]0;evil title\x07ok"), "ok");
        assert_eq!(
            sanitize("\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
    }

    #[test]
    fn carriage_return_progress_keeps_the_last_redraw() {
        assert_eq!(sanitize("10%\r50%\r100% done"), "100% done");
        assert_eq!(sanitize("line one\r\nline two\r\n"), "line one\nline two\n");
    }

    #[test]
    fn tabs_expand_and_other_controls_vanish() {
        assert_eq!(sanitize("a\tb"), "a    b");
        assert_eq!(sanitize("bell\x07 null\x00 del\x7f"), "bell null del");
    }

    #[test]
    fn bidi_overrides_are_dropped() {
        assert_eq!(sanitize("rm \u{202e}fdp.exe"), "rm fdp.exe");
    }

    #[test]
    fn eight_bit_csi_is_removed() {
        assert_eq!(sanitize("x\u{9b}1mz"), "xz");
    }

    #[test]
    fn a_trailing_lone_escape_is_dropped() {
        assert_eq!(sanitize("done\x1b"), "done");
    }

    #[test]
    fn unicode_text_survives() {
        assert_eq!(sanitize("中文 ✓ café"), "中文 ✓ café");
    }
}
