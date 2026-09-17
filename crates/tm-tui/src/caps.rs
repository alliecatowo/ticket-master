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

use std::collections::HashMap;

use ratatui_core::style::Color;

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
}

impl Capabilities {
    /// A conservative fallback: no colour, ASCII-only, no synchronized output, no mouse.
    ///
    /// Used before detection completes, and by any host that cannot probe a real terminal (a
    /// snapshot test, a log file, a CI runner without a tty).
    pub fn minimal() -> Self {
        Capabilities {
            color: ColorSupport::NoColor,
            unicode: UnicodeSupport::AsciiOnly,
            synchronized_output: false,
            mouse: false,
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
/// IMPL: the real call site (`runtime.rs`, at startup) builds this from `std::env::vars()`.
pub type Environment = HashMap<String, String>;

/// Detect what this terminal supports from its environment plus the two probes that cannot be
/// answered by environment variables alone.
///
/// IMPL:
/// - `color`: `env["NO_COLOR"]` present (any value, per the `NO_COLOR` convention) => `NoColor`.
///   Else `env["COLORTERM"]` containing `"truecolor"` or `"24bit"` => `TrueColor` — but only
///   trust this when `env["TERM_PROGRAM"]`/`env["TERM"]` do not indicate a multiplexer (`tmux`,
///   `screen`) that is known to swallow `COLORTERM` from the inner session; in that case prefer
///   `Ansi256` over trusting a possibly-stale `COLORTERM` (this is the Codex bug D-002 names).
///   Else `env["TERM"]` containing `"256color"` => `Ansi256`. Else `Ansi16`. A `NO_COLOR` check
///   should short-circuit before any of the above.
/// - `unicode`: `env["LANG"]`/`env["LC_ALL"]`/`env["LC_CTYPE"]` ending in `"UTF-8"` (case
///   insensitive) => at least `NarrowOnly`; anything else => `AsciiOnly`. Upgrading `NarrowOnly`
///   to `Full` needs emulator identification (DA1/DA2 device attribute queries) that environment
///   variables cannot provide — leave that as a later enhancement rather than guessing from
///   `TERM_PROGRAM` alone, which is easy to spoof and easy to get wrong.
/// - `synchronized_output` and `mouse` are passed in rather than detected here because both need
///   an actual round trip with the terminal (see [`probe_synchronized_output`]); `detect` just
///   assembles the final value.
pub fn detect(env: &Environment, synchronized_output: bool, mouse: bool) -> Capabilities {
    let _ = (env, synchronized_output, mouse);
    todo!("derive color/unicode support from `env` per the IMPL note above")
}

/// Determine whether to ask crossterm to enable DEC 2026 synchronized output, and whether the
/// terminal can be trusted to honour it.
///
/// IMPL: there is no query/response handshake for mode 2026 the way there is for some other DEC
/// private modes, so the practical approach is an allow-list keyed off `env["TERM_PROGRAM"]` /
/// `env["TERM"]` of emulators known to support it, updated as the ecosystem's support matrix
/// changes, rather than a runtime probe. Call this once at startup in `runtime.rs` before
/// `detect`, and thread its result into `detect`'s `synchronized_output` parameter.
pub fn probe_synchronized_output(env: &Environment) -> bool {
    let _ = env;
    todo!("allow-list synchronized-output-capable terminals per the IMPL note above")
}

/// Degrade `color` to whatever `caps.color` can actually render.
///
/// IMPL:
/// - `TrueColor`: pass `color` through unchanged.
/// - `Ansi256`: map `Color::Rgb(r, g, b)` to the nearest `Color::Indexed` in the 256-colour cube
///   by squared Euclidean distance in RGB space; named `Color` variants (e.g. `Color::Red`) pass
///   through unchanged since they are already representable.
/// - `Ansi16`: map to the nearest of the 16 named `Color` variants, by the same distance metric
///   for `Rgb`/`Indexed` inputs.
/// - `NoColor`: map everything to `Color::Reset`.
///
/// Keep this pure (no I/O, no reference to a live terminal) so every input colour is unit
/// testable against every `ColorSupport` level without a terminal.
pub fn degrade_color(caps: &Capabilities, color: Color) -> Color {
    let _ = caps;
    todo!("map `color` down to `caps.color`'s palette per the IMPL note above")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_capabilities_assume_nothing() {
        let caps = Capabilities::minimal();
        assert_eq!(caps.color, ColorSupport::NoColor);
        assert_eq!(caps.unicode, UnicodeSupport::AsciiOnly);
        assert!(!caps.synchronized_output);
        assert!(!caps.mouse);
    }

    #[test]
    fn default_matches_minimal() {
        assert_eq!(Capabilities::default(), Capabilities::minimal());
    }
}
