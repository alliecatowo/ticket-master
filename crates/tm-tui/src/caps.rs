//! Terminal capability detection and colour degradation.
//!
//! ratatui/crossterm tell us the terminal's size and let us write escape sequences; neither
//! tells us what the terminal on the *other end* of an SSH/tmux hop actually understands.
//! `COLORTERM` is routinely swallowed by `tmux` and `ssh`, so truecolor support cannot be
//! trusted from environment variables alone — Codex has an open bug for exactly this, and D-002
//! calls out not inheriting it. DEC 2026 synchronized output is available via crossterm's API,
//! but the API existing does not mean the terminal on the other end honours it; it must be
//! verified, not assumed.
//!
//! This module is the only place allowed to guess at what the terminal can do. Every other file
//! consumes the resulting [`Capabilities`] from [`crate::component::FrameContext`] and degrades
//! its own output accordingly, rather than assuming truecolor/Unicode/synchronized-output support.
//!
//! # The authoritative probe
//!
//! Environment variables describe what the *local* shell exported, not what the terminal at the
//! far end of an SSH hop or a tmux/screen multiplexer actually renders — a multiplexer routinely
//! starts its inner session without forwarding `COLORTERM` at all, which silently downgrades a
//! perfectly capable terminal (the open Codex bug D-002 names). The only way to ask the terminal
//! itself is to query it and read the reply: [`probe`] writes a batched OSC 11 (current
//! background colour) / kitty keyboard protocol (`CSI ? u`) / DA1 (`CSI c`) request and waits,
//! with a timeout, for whatever comes back. DA1 is answered by essentially every terminal
//! emulator ever shipped, including ones that silently drop the other two queries, so its reply
//! doubles as a sentinel: "no more probe output is coming". [`parse_probe_reply`] turns those
//! raw bytes into a [`ProbeReply`] with zero I/O, so the interesting logic is unit-testable
//! without a real terminal — only [`probe`] itself touches an actual read/write pair, and it
//! takes a plain `AsyncRead`/`AsyncWrite` so a test can hand it an in-memory duplex instead.
//!
//! [`detect`] is the pure assembly step: given an environment and whatever probe results are
//! available (including "none, it timed out"), produce a [`Capabilities`]. Colour prefers the
//! probe when it answered and falls back to env heuristics only when it did not — exactly the
//! rule D-002 asks for.

use std::collections::HashMap;
use std::time::Duration;

use ratatui_core::style::Color;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// How much colour the far end of the terminal connection actually renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    /// 24-bit RGB (`Color::Rgb`).
    TrueColor,
    /// The 256-colour palette (`Color::Indexed`).
    Ansi256,
    /// The original 16 ANSI colours.
    Ansi16,
    /// No colour at all: `NO_COLOR` is set, the terminal is not a tty, or detection could not
    /// confirm anything better.
    NoColor,
}

/// How much of Unicode the far end can be trusted to render at the width `text.rs` computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnicodeSupport {
    /// Full grapheme/emoji width support, including variation selectors.
    Full,
    /// Reliable narrow/wide CJK support, but emoji and variation selectors are not to be
    /// trusted at the widths `unicode-width` reports (D-002).
    NarrowOnly,
    /// Treat everything above ASCII as unreliable; render fallback glyphs.
    AsciiOnly,
}

/// What this run of `tm` can rely on the terminal to do.
///
/// Constructed once at startup by [`detect`] and carried in every
/// [`crate::component::FrameContext`] so components degrade their own output (see
/// [`degrade_color`]) instead of assuming the best case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The colour depth to render at.
    pub color: ColorSupport,
    /// The Unicode support level to render at.
    pub unicode: UnicodeSupport,
    /// DEC 2026 synchronized output was requested *and confirmed*, not merely available in
    /// crossterm's API. See [`probe_synchronized_output`].
    pub synchronized_output: bool,
    /// The terminal reported (via crossterm) that it accepts mouse sequences.
    pub mouse: bool,
    /// The terminal answered the kitty keyboard protocol query (`CSI ? u`), so
    /// `PushKeyboardEnhancementFlags` can be enabled to get real key-release/repeat events and
    /// unambiguous modifier reporting instead of crossterm's legacy-terminal guesses.
    pub kitty_keyboard: bool,
    /// The terminal answered *any* probe query, which is the only signal available that a live,
    /// modern terminal is on the other end rather than a pipe, a log file, or `TERM=dumb` — there
    /// is no query/response escape sequence for bracketed paste support itself, so this is the
    /// best-effort gate for enabling it (`CSI ?2004h`) rather than assuming a tty always wants it.
    pub bracketed_paste: bool,
}

impl Capabilities {
    /// A conservative fallback: no colour, ASCII-only, no synchronized output, no mouse, no
    /// kitty keyboard protocol, no bracketed paste.
    ///
    /// Used before detection completes, and by any host that cannot probe a real terminal (a
    /// snapshot test, a log file, a CI runner without a tty).
    pub fn minimal() -> Self {
        Capabilities {
            color: ColorSupport::NoColor,
            unicode: UnicodeSupport::AsciiOnly,
            synchronized_output: false,
            mouse: false,
            kitty_keyboard: false,
            bracketed_paste: false,
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities::minimal()
    }
}

/// The subset of the process environment capability detection reads, injected so tests do not
/// depend on the real environment (and so detection stays a pure function of its input).
///
/// The real call site (`runtime.rs`, at startup) builds this from `std::env::vars()`.
pub type Environment = HashMap<String, String>;

/// What the OSC 11 / kitty-keyboard / DA1 probe learned, or that it learned nothing before its
/// timeout expired.
///
/// This is the authoritative half of colour detection: when the probe answered, its `color` (if
/// any) overrides the env heuristic outright, because it reflects what the terminal at the far
/// end of the connection actually does rather than what a shell many hops away happened to
/// export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProbeReply {
    /// The terminal answered the OSC 11 query, confirming it understands colour queries and can
    /// be trusted for 24-bit colour.
    pub truecolor_confirmed: bool,
    /// The terminal answered the kitty keyboard protocol query (`CSI ? u`).
    pub kitty_keyboard: bool,
    /// The terminal answered *something* (at minimum the DA1 sentinel) before the timeout, i.e.
    /// there is a live terminal on the other end at all.
    pub responded: bool,
}

/// The bytes `probe` writes: an OSC 11 query for the current background colour (terminated with
/// BEL, which is more broadly honoured than the ST terminator for old parsers), a kitty keyboard
/// protocol query, and a DA1 (Device Attributes) request last.
///
/// DA1 is answered by effectively every terminal emulator shipped in the last three decades,
/// including ones that silently ignore the two queries ahead of it, so its reply is what lets
/// [`probe`] know "there is nothing more coming" instead of waiting out the full timeout on a
/// perfectly responsive terminal that just does not support OSC 11 or the kitty protocol.
pub const PROBE_REQUEST: &[u8] = b"\x1b]11;?\x07\x1b[?u\x1b[c";

/// How long [`probe`] waits for a reply before assuming the far end will not answer at all (a
/// pipe, a log file, `TERM=dumb`, or a terminal too old to know any of these sequences).
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Write [`PROBE_REQUEST`] to `writer` and read whatever comes back from `reader` until the DA1
/// reply's terminating `c` is seen or `timeout` elapses, then parse it with
/// [`parse_probe_reply`].
///
/// This is the one function in this module that performs I/O; everything it delegates to
/// (`parse_probe_reply`, `detect`, `degrade_color`) is pure and unit-tested without a terminal.
/// Callers (`runtime.rs`, once raw mode is entered) pass the real stdin/stdout pair; tests pass a
/// `tokio::io::duplex` so the whole round trip is exercised against canned bytes instead of a
/// live tty.
pub async fn probe<R, W>(reader: &mut R, writer: &mut W, timeout: Duration) -> ProbeReply
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if writer.write_all(PROBE_REQUEST).await.is_err() || writer.flush().await.is_err() {
        // Nothing we can do if the terminal write itself fails; treat exactly like a timeout so
        // detection falls back to env heuristics rather than panicking on a broken pipe.
        return ProbeReply::default();
    }

    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => break,
            read = reader.read(&mut chunk) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        // The DA1 reply's final byte is `c`; once we have seen it there is
                        // nothing left in flight worth waiting the rest of the timeout out for.
                        if buf.contains(&b'c') && has_csi_reply_ending_in(&buf, b'c') {
                            break;
                        }
                    }
                }
            }
        }
    }

    parse_probe_reply(&buf)
}

/// Find a `CSI ? ... <final>` reply (`\x1b[?...<final>`) in `bytes` ending in the byte
/// `final_byte`, e.g. `b'u'` for the kitty keyboard query or `b'c'` for DA1.
fn has_csi_reply_ending_in(bytes: &[u8], final_byte: u8) -> bool {
    let mut i = 0;
    while let Some(start) = find_subslice(&bytes[i..], b"\x1b[?") {
        let seq_start = i + start + 2; // position of `?`
        if let Some(end_offset) = bytes[seq_start..]
            .iter()
            .position(|b| b.is_ascii_alphabetic())
        {
            let end = seq_start + end_offset;
            if bytes[end] == final_byte {
                return true;
            }
            i = end + 1;
        } else {
            return false;
        }
    }
    false
}

/// Find `needle` in `haystack`, returning the byte offset of the first match.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse whatever bytes came back from [`probe`] (or an empty slice, on timeout) into a
/// [`ProbeReply`].
///
/// Pure: no I/O, so every reply shape (a full answer, a partial one, garbage, nothing) is unit
/// testable without a terminal.
pub fn parse_probe_reply(bytes: &[u8]) -> ProbeReply {
    ProbeReply {
        truecolor_confirmed: find_subslice(bytes, b"\x1b]11;rgb:").is_some(),
        kitty_keyboard: has_csi_reply_ending_in(bytes, b'u'),
        responded: has_csi_reply_ending_in(bytes, b'c')
            || find_subslice(bytes, b"\x1b]11;").is_some(),
    }
}

/// Detect what this terminal supports from its environment alone, treating the probe as having
/// timed out (`ProbeReply::default()`).
///
/// This is the degraded-but-still-correct path: a host that never runs [`probe`] at all (a
/// snapshot test building `Capabilities` directly from a canned `Environment`, or a genuinely
/// non-interactive run) still gets an honest, env-derived answer rather than a `todo!()`. Prefer
/// [`detect_with_probe`] whenever a probe was actually run.
pub fn detect(env: &Environment, synchronized_output: bool, mouse: bool) -> Capabilities {
    detect_with_probe(env, ProbeReply::default(), synchronized_output, mouse)
}

/// Detect what this terminal supports from its environment plus an authoritative [`ProbeReply`]
/// obtained by calling [`probe`] once, before the first frame is drawn.
///
/// - `color`: `env["NO_COLOR"]` present (any value, per the `NO_COLOR` convention) or
///   `env["TERM"] == "dumb"` => `NoColor`, unconditionally — D-002 calls these out as absolute,
///   not overridable by a probe that happens to answer. Otherwise, when `probe.truecolor_confirmed`
///   is set, trust it and answer `TrueColor` outright: it reflects the terminal that actually
///   replied, not a possibly-stale `COLORTERM` a multiplexer or SSH hop failed to forward (the
///   open Codex bug D-002 names). Only when the probe did not confirm truecolor do we fall back
///   to env heuristics: `COLORTERM` containing `"truecolor"`/`"24bit"` counts *unless*
///   `TERM`/`TERM_PROGRAM` indicate a multiplexer (`tmux`, `screen`) known to swallow it, in which
///   case prefer `Ansi256` over trusting it; otherwise `TERM` containing `"256color"` => `Ansi256`;
///   otherwise `Ansi16`.
/// - `unicode`: `LANG`/`LC_ALL`/`LC_CTYPE` ending in `"UTF-8"` (case insensitive) => `NarrowOnly`;
///   anything else => `AsciiOnly`. `TERM == "dumb"` forces `AsciiOnly` regardless of locale.
///   Upgrading `NarrowOnly` to `Full` needs emulator identification beyond what env vars or this
///   probe's DA1/OSC replies distinguish reliably — left as a later enhancement rather than
///   guessing from `TERM_PROGRAM` alone, which is easy to spoof and easy to get wrong.
/// - `synchronized_output` and `mouse` are threaded through unchanged: both need crossterm
///   round trips this module does not perform ([`probe_synchronized_output`] for the former is an
///   env allow-list, mouse capture is crossterm's own `EnableMouseCapture` result).
/// - `kitty_keyboard` is `probe.kitty_keyboard` verbatim.
/// - `bracketed_paste` is `probe.responded`: bracketed paste has no query/response escape
///   sequence of its own to test, so "the terminal answered something" is the best available
///   signal that a live, modern terminal — rather than a pipe, a log file, or `TERM=dumb` — is on
///   the other end.
pub fn detect_with_probe(
    env: &Environment,
    probe: ProbeReply,
    synchronized_output: bool,
    mouse: bool,
) -> Capabilities {
    let term = env.get("TERM").map(String::as_str).unwrap_or_default();
    let is_dumb = term.eq_ignore_ascii_case("dumb");
    let no_color = env.contains_key("NO_COLOR") || is_dumb;

    let color = if no_color {
        ColorSupport::NoColor
    } else if probe.truecolor_confirmed {
        ColorSupport::TrueColor
    } else {
        detect_color_from_env(env, term)
    };

    let unicode = if is_dumb {
        UnicodeSupport::AsciiOnly
    } else if locale_is_utf8(env) {
        UnicodeSupport::NarrowOnly
    } else {
        UnicodeSupport::AsciiOnly
    };

    Capabilities {
        color,
        unicode,
        synchronized_output,
        mouse,
        kitty_keyboard: probe.kitty_keyboard,
        bracketed_paste: probe.responded,
    }
}

/// The env-only colour heuristic, used when [`ProbeReply::truecolor_confirmed`] is false: either
/// the probe genuinely timed out, or it answered without confirming truecolor, both of which mean
/// env is the best evidence left.
fn detect_color_from_env(env: &Environment, term: &str) -> ColorSupport {
    let colorterm = env
        .get("COLORTERM")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let claims_truecolor = colorterm.contains("truecolor") || colorterm.contains("24bit");

    let term_program = env
        .get("TERM_PROGRAM")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let inside_swallowing_multiplexer = term.contains("tmux")
        || term.contains("screen")
        || term_program.contains("tmux")
        || term_program.contains("screen");

    if claims_truecolor && !inside_swallowing_multiplexer {
        ColorSupport::TrueColor
    } else if term.contains("256color") {
        ColorSupport::Ansi256
    } else if claims_truecolor {
        // COLORTERM claimed truecolor but we are behind a multiplexer known to swallow it from
        // the inner session — the Codex bug D-002 names. Do not trust it outright; a probe would
        // have confirmed it above if the actual terminal supports it, so treat this as merely
        // "at least 256-colour", not a downgrade to Ansi16.
        ColorSupport::Ansi256
    } else {
        ColorSupport::Ansi16
    }
}

/// True when `LC_ALL`/`LC_CTYPE`/`LANG` (checked in that priority order, matching glibc's own
/// locale resolution) ends in `UTF-8` (case-insensitive, so `utf8`/`UTF-8`/`en_US.utf-8` all
/// count).
fn locale_is_utf8(env: &Environment) -> bool {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(value) = env.get(key) {
            if !value.is_empty() {
                let normalized = value.to_ascii_uppercase().replace('-', "");
                return normalized.ends_with("UTF8");
            }
        }
    }
    false
}

/// Determine whether to ask crossterm to enable DEC 2026 synchronized output, and whether the
/// terminal can be trusted to honour it.
///
/// There is no query/response handshake for mode 2026 the way there is for the colour/keyboard
/// queries [`probe`] performs, so this is an allow-list keyed off `TERM_PROGRAM`/`TERM` of
/// emulators known to support it, rather than a runtime probe — update it as the ecosystem's
/// support matrix changes. Call this once at startup in `runtime.rs` before `detect_with_probe`.
pub fn probe_synchronized_output(env: &Environment) -> bool {
    let term = env
        .get("TERM")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let term_program = env
        .get("TERM_PROGRAM")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();

    const SYNCHRONIZED_OUTPUT_TERM_PROGRAMS: &[&str] =
        &["iterm.app", "wezterm", "vscode", "ghostty", "tabby", "rio"];
    const SYNCHRONIZED_OUTPUT_TERMS: &[&str] = &["kitty", "xterm-kitty", "foot", "contour"];

    SYNCHRONIZED_OUTPUT_TERM_PROGRAMS
        .iter()
        .any(|known| term_program.contains(known))
        || SYNCHRONIZED_OUTPUT_TERMS
            .iter()
            .any(|known| term.contains(known))
}

/// Degrade `color` to whatever `caps.color` can actually render.
///
/// - `TrueColor`: pass `color` through unchanged.
/// - `Ansi256`: map `Color::Rgb(r, g, b)` to the nearest `Color::Indexed` in the xterm 256-colour
///   palette by squared Euclidean distance in RGB space; every other `Color` variant is already
///   representable and passes through unchanged.
/// - `Ansi16`: map `Color::Rgb`/`Color::Indexed` to the nearest of the 16 named `Color` variants
///   by the same distance metric; named variants and `Color::Reset` pass through unchanged.
/// - `NoColor`: map everything to `Color::Reset`.
///
/// Pure (no I/O, no reference to a live terminal) so every input colour is unit testable against
/// every `ColorSupport` level without a terminal.
pub fn degrade_color(caps: &Capabilities, color: Color) -> Color {
    match caps.color {
        ColorSupport::TrueColor => color,
        ColorSupport::Ansi256 => match color {
            Color::Rgb(r, g, b) => Color::Indexed(nearest_ansi256_index(r, g, b)),
            other => other,
        },
        ColorSupport::Ansi16 => match color {
            Color::Rgb(r, g, b) => nearest_ansi16_color(r, g, b),
            Color::Indexed(i) => {
                let (r, g, b) = ansi256_index_to_rgb(i);
                nearest_ansi16_color(r, g, b)
            }
            other => other,
        },
        ColorSupport::NoColor => Color::Reset,
    }
}

/// Squared Euclidean distance between two RGB triples; sufficient for nearest-colour matching
/// since we only ever compare, never need the true (rooted) distance.
fn squared_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let dr = i32::from(a.0) - i32::from(b.0);
    let dg = i32::from(a.1) - i32::from(b.1);
    let db = i32::from(a.2) - i32::from(b.2);
    (dr * dr + dg * dg + db * db) as u32
}

/// The RGB value the xterm 256-colour palette assigns to index `i`: the 16 standard ANSI colours
/// (0-15), the 6x6x6 colour cube (16-231), then a 24-step greyscale ramp (232-255).
fn ansi256_index_to_rgb(i: u8) -> (u8, u8, u8) {
    const BASIC_16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];

    match i {
        0..=15 => BASIC_16[i as usize],
        16..=231 => {
            let n = i - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let component = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            (component(r), component(g), component(b))
        }
        232..=255 => {
            let level = 8 + 10 * (i - 232);
            (level, level, level)
        }
    }
}

/// Map an RGB triple to the nearest of the 256 xterm palette entries by squared distance.
fn nearest_ansi256_index(r: u8, g: u8, b: u8) -> u8 {
    (0u16..=255)
        .map(|i| i as u8)
        .min_by_key(|&i| squared_distance((r, g, b), ansi256_index_to_rgb(i)))
        .unwrap_or(0)
}

/// The 16 named `Color` variants alongside the RGB value they render as, used both to degrade an
/// arbitrary colour down to the basic palette and as the source of truth `nearest_ansi16_color`
/// searches.
const NAMED_16: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (128, 0, 0)),
    (Color::Green, (0, 128, 0)),
    (Color::Yellow, (128, 128, 0)),
    (Color::Blue, (0, 0, 128)),
    (Color::Magenta, (128, 0, 128)),
    (Color::Cyan, (0, 128, 128)),
    (Color::Gray, (192, 192, 192)),
    (Color::DarkGray, (128, 128, 128)),
    (Color::LightRed, (255, 0, 0)),
    (Color::LightGreen, (0, 255, 0)),
    (Color::LightYellow, (255, 255, 0)),
    (Color::LightBlue, (0, 0, 255)),
    (Color::LightMagenta, (255, 0, 255)),
    (Color::LightCyan, (0, 255, 255)),
    (Color::White, (255, 255, 255)),
];

/// Map an RGB triple to the nearest of the 16 named ANSI `Color` variants by squared distance.
fn nearest_ansi16_color(r: u8, g: u8, b: u8) -> Color {
    NAMED_16
        .iter()
        .min_by_key(|(_, rgb)| squared_distance((r, g, b), *rgb))
        .map(|(color, _)| *color)
        .unwrap_or(Color::Reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Environment {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // -- Capabilities -------------------------------------------------------------------------

    #[test]
    fn minimal_capabilities_assume_nothing() {
        let caps = Capabilities::minimal();
        assert_eq!(caps.color, ColorSupport::NoColor);
        assert_eq!(caps.unicode, UnicodeSupport::AsciiOnly);
        assert!(!caps.synchronized_output);
        assert!(!caps.mouse);
        assert!(!caps.kitty_keyboard);
        assert!(!caps.bracketed_paste);
    }

    #[test]
    fn default_matches_minimal() {
        assert_eq!(Capabilities::default(), Capabilities::minimal());
    }

    // -- detect: NO_COLOR / TERM=dumb are absolute --------------------------------------------

    #[test]
    fn no_color_env_var_wins_even_with_truecolor_colorterm() {
        let e = env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")]);
        let caps = detect(&e, false, false);
        assert_eq!(caps.color, ColorSupport::NoColor);
    }

    #[test]
    fn no_color_wins_over_a_confirming_probe() {
        let e = env(&[("NO_COLOR", "1")]);
        let probe = ProbeReply {
            truecolor_confirmed: true,
            ..ProbeReply::default()
        };
        let caps = detect_with_probe(&e, probe, false, false);
        assert_eq!(
            caps.color,
            ColorSupport::NoColor,
            "NO_COLOR must be absolute, even when the probe confirms truecolor"
        );
    }

    #[test]
    fn term_dumb_forces_no_color_and_ascii() {
        let e = env(&[("TERM", "dumb"), ("LANG", "en_US.UTF-8")]);
        let caps = detect(&e, false, false);
        assert_eq!(caps.color, ColorSupport::NoColor);
        assert_eq!(caps.unicode, UnicodeSupport::AsciiOnly);
    }

    // -- detect: env-only colour heuristics (probe timed out) ---------------------------------

    #[test]
    fn colorterm_truecolor_is_trusted_outside_a_multiplexer() {
        let e = env(&[("COLORTERM", "truecolor"), ("TERM", "xterm")]);
        let caps = detect(&e, false, false);
        assert_eq!(caps.color, ColorSupport::TrueColor);
    }

    #[test]
    fn colorterm_truecolor_inside_tmux_is_not_trusted_without_a_probe() {
        let e = env(&[("COLORTERM", "truecolor"), ("TERM", "tmux-256color")]);
        let caps = detect(&e, false, false);
        assert_eq!(
            caps.color,
            ColorSupport::Ansi256,
            "a multiplexer known to swallow COLORTERM must not upgrade to TrueColor on env alone"
        );
    }

    #[test]
    fn term_256color_without_colorterm_is_ansi256() {
        let e = env(&[("TERM", "screen-256color")]);
        let caps = detect(&e, false, false);
        assert_eq!(caps.color, ColorSupport::Ansi256);
    }

    #[test]
    fn bare_term_falls_back_to_ansi16() {
        let e = env(&[("TERM", "xterm")]);
        let caps = detect(&e, false, false);
        assert_eq!(caps.color, ColorSupport::Ansi16);
    }

    // -- detect_with_probe: the probe is authoritative when it answers ------------------------

    #[test]
    fn probe_confirmed_truecolor_overrides_a_stale_ansi16_env() {
        // Exactly the tmux/SSH trap D-002 names: COLORTERM never made it through, but the probe
        // talked to the real terminal and it answered OSC 11.
        let e = env(&[("TERM", "tmux-256color")]);
        let probe = ProbeReply {
            truecolor_confirmed: true,
            ..ProbeReply::default()
        };
        let caps = detect_with_probe(&e, probe, false, false);
        assert_eq!(caps.color, ColorSupport::TrueColor);
    }

    #[test]
    fn probe_timeout_falls_back_to_env_heuristics() {
        let e = env(&[("TERM", "xterm-256color")]);
        let caps = detect_with_probe(&e, ProbeReply::default(), false, false);
        assert_eq!(caps.color, ColorSupport::Ansi256);
    }

    #[test]
    fn probe_kitty_keyboard_and_responded_flow_through() {
        let e = env(&[]);
        let probe = ProbeReply {
            truecolor_confirmed: false,
            kitty_keyboard: true,
            responded: true,
        };
        let caps = detect_with_probe(&e, probe, true, true);
        assert!(caps.kitty_keyboard);
        assert!(caps.bracketed_paste);
        assert!(caps.synchronized_output);
        assert!(caps.mouse);
    }

    #[test]
    fn no_reply_disables_kitty_keyboard_and_bracketed_paste() {
        let caps = detect(&env(&[]), false, false);
        assert!(!caps.kitty_keyboard);
        assert!(!caps.bracketed_paste);
    }

    // -- detect: unicode ------------------------------------------------------------------------

    #[test]
    fn utf8_locale_upgrades_unicode_support() {
        for value in ["en_US.UTF-8", "C.utf8", "en_GB.utf-8"] {
            let e = env(&[("LANG", value)]);
            let caps = detect(&e, false, false);
            assert_eq!(caps.unicode, UnicodeSupport::NarrowOnly, "LANG={value}");
        }
    }

    #[test]
    fn non_utf8_locale_stays_ascii_only() {
        let e = env(&[("LANG", "C")]);
        let caps = detect(&e, false, false);
        assert_eq!(caps.unicode, UnicodeSupport::AsciiOnly);
    }

    #[test]
    fn lc_all_takes_priority_over_lang() {
        let e = env(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")]);
        let caps = detect(&e, false, false);
        assert_eq!(
            caps.unicode,
            UnicodeSupport::AsciiOnly,
            "LC_ALL must win over LANG, matching glibc's own resolution order"
        );
    }

    // -- probe_synchronized_output ---------------------------------------------------------------

    #[test]
    fn synchronized_output_allow_lists_known_terminals() {
        assert!(probe_synchronized_output(&env(&[("TERM", "xterm-kitty")])));
        assert!(probe_synchronized_output(&env(&[(
            "TERM_PROGRAM",
            "iTerm.app"
        )])));
        assert!(!probe_synchronized_output(&env(&[("TERM", "xterm")])));
    }

    // -- parse_probe_reply ------------------------------------------------------------------------

    #[test]
    fn parse_probe_reply_reads_all_three_answers() {
        let reply = b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07\x1b[?1u\x1b[?62;1;6c";
        let parsed = parse_probe_reply(reply);
        assert!(parsed.truecolor_confirmed);
        assert!(parsed.kitty_keyboard);
        assert!(parsed.responded);
    }

    #[test]
    fn parse_probe_reply_on_empty_bytes_is_all_false() {
        let parsed = parse_probe_reply(b"");
        assert_eq!(parsed, ProbeReply::default());
    }

    #[test]
    fn parse_probe_reply_da1_only_confirms_responded_but_not_truecolor() {
        let reply = b"\x1b[?6c";
        let parsed = parse_probe_reply(reply);
        assert!(!parsed.truecolor_confirmed);
        assert!(!parsed.kitty_keyboard);
        assert!(parsed.responded);
    }

    #[test]
    fn parse_probe_reply_ignores_unrelated_garbage() {
        let parsed = parse_probe_reply(b"not an escape sequence at all");
        assert_eq!(parsed, ProbeReply::default());
    }

    // -- probe: real (in-memory) I/O round trip ---------------------------------------------------

    #[tokio::test]
    async fn probe_writes_the_request_and_parses_a_prompt_reply() {
        let (client, mut server) = tokio::io::duplex(1024);

        let probing = tokio::spawn(async move {
            let (mut read_half, mut write_half) = tokio::io::split(client);
            probe(&mut read_half, &mut write_half, Duration::from_millis(500)).await
        });

        // Act as "the terminal": read the request, then answer as a kitty+truecolor terminal
        // would.
        let mut request = [0u8; PROBE_REQUEST.len()];
        server
            .read_exact(&mut request)
            .await
            .expect("test double must receive the full probe request");
        assert_eq!(&request, PROBE_REQUEST);

        server
            .write_all(b"\x1b]11;rgb:2222/2222/2222\x07\x1b[?1u\x1b[?62c")
            .await
            .expect("test double write must succeed");
        server
            .flush()
            .await
            .expect("test double flush must succeed");

        let reply = probing.await.expect("probe task must not panic");
        assert!(reply.truecolor_confirmed);
        assert!(reply.kitty_keyboard);
        assert!(reply.responded);
    }

    #[tokio::test]
    async fn probe_times_out_when_nothing_answers() {
        let (client, _server_kept_alive_but_silent) = tokio::io::duplex(1024);
        let (mut read_half, mut write_half) = tokio::io::split(client);

        let reply = probe(&mut read_half, &mut write_half, Duration::from_millis(30)).await;
        assert_eq!(reply, ProbeReply::default());
    }

    // -- degrade_color --------------------------------------------------------------------------

    #[test]
    fn truecolor_support_passes_colours_through_unchanged() {
        let caps = Capabilities {
            color: ColorSupport::TrueColor,
            ..Capabilities::minimal()
        };
        assert_eq!(
            degrade_color(&caps, Color::Rgb(12, 34, 56)),
            Color::Rgb(12, 34, 56)
        );
        assert_eq!(
            degrade_color(&caps, Color::Indexed(200)),
            Color::Indexed(200)
        );
    }

    #[test]
    fn no_color_maps_everything_to_reset() {
        let caps = Capabilities {
            color: ColorSupport::NoColor,
            ..Capabilities::minimal()
        };
        assert_eq!(degrade_color(&caps, Color::Rgb(255, 0, 0)), Color::Reset);
        assert_eq!(degrade_color(&caps, Color::Red), Color::Reset);
        assert_eq!(degrade_color(&caps, Color::Indexed(42)), Color::Reset);
    }

    #[test]
    fn ansi256_maps_rgb_to_nearest_indexed_and_passes_named_through() {
        let caps = Capabilities {
            color: ColorSupport::Ansi256,
            ..Capabilities::minimal()
        };
        // Pure white must land on a palette entry that *is* pure white, not somewhere arbitrary.
        // The xterm palette contains (255,255,255) twice — index 15 in the basic 16 and index
        // 231 at the top of the colour cube — so asserting one specific index would pin an
        // arbitrary choice between two exactly-correct answers. Assert the property instead.
        let white = degrade_color(&caps, Color::Rgb(255, 255, 255));
        let Color::Indexed(index) = white else {
            panic!("an Rgb colour must degrade to an Indexed one under Ansi256, got {white:?}");
        };
        assert_eq!(
            ansi256_index_to_rgb(index),
            (255, 255, 255),
            "index {index} is not pure white"
        );
        assert_eq!(degrade_color(&caps, Color::Red), Color::Red);
    }

    #[test]
    fn ansi16_maps_rgb_and_indexed_to_nearest_named_color() {
        let caps = Capabilities {
            color: ColorSupport::Ansi16,
            ..Capabilities::minimal()
        };
        assert_eq!(degrade_color(&caps, Color::Rgb(250, 5, 5)), Color::LightRed);
        assert_eq!(degrade_color(&caps, Color::Rgb(0, 0, 0)), Color::Black);
    }

    #[test]
    fn degrade_color_is_pure_same_input_same_output() {
        let caps = Capabilities {
            color: ColorSupport::Ansi256,
            ..Capabilities::minimal()
        };
        let a = degrade_color(&caps, Color::Rgb(90, 140, 210));
        let b = degrade_color(&caps, Color::Rgb(90, 140, 210));
        assert_eq!(a, b);
    }

    // A property-style sweep: every RGB corner/edge colour must degrade to *some* valid entry in
    // the target palette, never panic, and never silently move under repeated calls.
    #[test]
    fn degrade_color_never_panics_across_the_rgb_cube_corners() {
        let caps_256 = Capabilities {
            color: ColorSupport::Ansi256,
            ..Capabilities::minimal()
        };
        let caps_16 = Capabilities {
            color: ColorSupport::Ansi16,
            ..Capabilities::minimal()
        };
        for r in [0u8, 128, 255] {
            for g in [0u8, 128, 255] {
                for b in [0u8, 128, 255] {
                    let color = Color::Rgb(r, g, b);
                    assert!(matches!(degrade_color(&caps_256, color), Color::Indexed(_)));
                    assert!(NAMED_16
                        .iter()
                        .any(|(named, _)| degrade_color(&caps_16, color) == *named));
                }
            }
        }
    }
}
