//! The symbols the chat views draw with, chosen once per frame from what the terminal can render.
//!
//! Every non-ASCII symbol the chat screen, the agents home screen, and their overlays use lives
//! here, next to its ASCII stand-in, so a terminal whose locale is not UTF-8
//! ([`crate::caps::UnicodeSupport::AsciiOnly`]) never gets a row of replacement characters.

use crate::caps::{Capabilities, UnicodeSupport};

/// A rounded (or ASCII) box-drawing set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Border {
    /// Top-left corner.
    pub top_left: &'static str,
    /// Top-right corner.
    pub top_right: &'static str,
    /// Bottom-left corner.
    pub bottom_left: &'static str,
    /// Bottom-right corner.
    pub bottom_right: &'static str,
    /// Horizontal edge.
    pub horizontal: &'static str,
    /// Vertical edge.
    pub vertical: &'static str,
}

/// The chat views' symbol table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Marks the first line of an assistant reply.
    pub assistant: &'static str,
    /// Prompt marker in the input box and before an echoed user message.
    pub prompt: &'static str,
    /// A tool call that completed successfully.
    pub ok: &'static str,
    /// A tool call that errored, or a command that exited non-zero.
    pub fail: &'static str,
    /// A tool call the authority check denied.
    pub denied: &'static str,
    /// An informational notice.
    pub info: &'static str,
    /// A warning notice.
    pub warn: &'static str,
    /// A filled dot: turn-state indicator, list bullet for workers.
    pub dot: &'static str,
    /// Bullet for unordered Markdown lists.
    pub bullet: &'static str,
    /// Separator between status-bar segments and inline hints.
    pub sep: &'static str,
    /// Truncation marker.
    pub ellipsis: &'static str,
    /// Continuation marker under a tool line (error detail, output preview).
    pub elbow: &'static str,
    /// Left gutter for block quotes and tool output previews.
    pub gutter: &'static str,
    /// Horizontal rule segment.
    pub rule: &'static str,
    /// Selection marker in lists and popups.
    pub pointer: &'static str,
    /// "Go left" arrow, used in hints (`← sessions & tickets`).
    pub left: &'static str,
    /// Up/down arrows pair, used in hints.
    pub updown: &'static str,
    /// Leads every assistant message and tool call in the transcript (Claude Code's `⏺`).
    pub record: &'static str,
    /// Hangs a tool's result under its call (Claude Code's `⎿`).
    pub result: &'static str,
    /// The welcome box's mark (`✻ Welcome to tm!`).
    pub star: &'static str,
    /// The plan-mode indicator (`⏸ plan mode on`).
    pub pause: &'static str,
    /// The ask-mode indicator (`⏵ ask mode on`).
    pub play: &'static str,
    /// Skipped lines between two diff hunks.
    pub vellipsis: &'static str,
    /// Spinner frames for a running turn.
    pub spinner: &'static [&'static str],
    /// Box borders.
    pub border: Border,
    /// Whether this is the Unicode set (decorations with no ASCII stand-in are skipped otherwise).
    pub unicode: bool,
}

/// A breathing asterisk rather than the braille dots most spinners use: braille is missing from a
/// surprising number of terminal fonts (it renders as a row of replacement boxes), while these
/// dingbats are in every font that has `✓`/`✗`.
const SPINNER_UNICODE: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const SPINNER_ASCII: &[&str] = &["|", "/", "-", "\\"];

impl Glyphs {
    /// The full Unicode set.
    pub const UNICODE: Glyphs = Glyphs {
        assistant: "●",
        prompt: "›",
        ok: "✓",
        fail: "✗",
        denied: "⊘",
        info: "·",
        warn: "!",
        dot: "●",
        bullet: "•",
        sep: " · ",
        ellipsis: "…",
        elbow: "└",
        gutter: "│",
        rule: "─",
        pointer: "›",
        left: "←",
        updown: "↑↓",
        record: "⏺",
        result: "⎿",
        star: "✻",
        pause: "⏸",
        play: "⏵",
        vellipsis: "⋮",
        spinner: SPINNER_UNICODE,
        border: Border {
            top_left: "╭",
            top_right: "╮",
            bottom_left: "╰",
            bottom_right: "╯",
            horizontal: "─",
            vertical: "│",
        },
        unicode: true,
    };

    /// The plain-ASCII fallback set.
    pub const ASCII: Glyphs = Glyphs {
        assistant: "*",
        prompt: ">",
        ok: "+",
        fail: "x",
        denied: "-",
        info: "-",
        warn: "!",
        dot: "*",
        bullet: "-",
        sep: " | ",
        ellipsis: "...",
        elbow: "`",
        gutter: "|",
        rule: "-",
        pointer: ">",
        left: "<-",
        updown: "up/down",
        record: "*",
        result: "L",
        star: "*",
        pause: "||",
        play: ">",
        vellipsis: ":",
        spinner: SPINNER_ASCII,
        border: Border {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            horizontal: "-",
            vertical: "|",
        },
        unicode: false,
    };

    /// The set `caps` can be trusted to render.
    pub fn for_caps(caps: &Capabilities) -> Glyphs {
        match caps.unicode {
            UnicodeSupport::AsciiOnly => Glyphs::ASCII,
            UnicodeSupport::NarrowOnly | UnicodeSupport::Full => Glyphs::UNICODE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::display_width;

    #[test]
    fn ascii_set_is_really_ascii() {
        let g = Glyphs::ASCII;
        for s in [
            g.assistant,
            g.prompt,
            g.ok,
            g.fail,
            g.denied,
            g.info,
            g.warn,
            g.dot,
            g.bullet,
            g.sep,
            g.ellipsis,
            g.elbow,
            g.gutter,
            g.rule,
            g.pointer,
            g.left,
            g.updown,
            g.record,
            g.result,
            g.star,
            g.pause,
            g.play,
            g.vellipsis,
            g.border.top_left,
            g.border.horizontal,
            g.border.vertical,
        ] {
            assert!(s.is_ascii(), "{s:?} is not ASCII");
        }
        assert!(g.spinner.iter().all(|f| f.is_ascii()));
    }

    #[test]
    fn single_cell_glyphs_are_one_column_in_both_sets() {
        for g in [Glyphs::UNICODE, Glyphs::ASCII] {
            for s in [
                g.assistant,
                g.prompt,
                g.ok,
                g.fail,
                g.denied,
                g.dot,
                g.gutter,
                g.record,
                g.result,
                g.star,
                g.play,
                g.vellipsis,
            ] {
                assert_eq!(display_width(s), 1, "{s:?} must occupy exactly one column");
            }
            // A spinner frame of a different width would make the text after it jitter.
            for frame in g.spinner {
                assert_eq!(display_width(frame), 1, "spinner frame {frame:?}");
            }
        }
    }

    #[test]
    fn for_caps_follows_unicode_support() {
        let mut caps = Capabilities::minimal();
        assert_eq!(Glyphs::for_caps(&caps), Glyphs::ASCII);
        caps.unicode = UnicodeSupport::NarrowOnly;
        assert_eq!(Glyphs::for_caps(&caps), Glyphs::UNICODE);
    }
}
