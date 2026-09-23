//! Theme, layout helpers and animation.
//!
//! ratatui gives us `Layout`/`Constraint` for splitting a `Rect` and `Style`/`Color` for painting
//! one; it makes no claim about *our* visual language. This module is where that language lives:
//! the palette and semantic styles components paint with ([`Theme`]), named layout slots built on
//! top of `ratatui_core::layout::Layout` ([`Slots`], [`split_vertical`], [`split_horizontal`]),
//! and time-based interpolation driven by the injected clock rather than a wall-clock read
//! ([`Animation`]). A component should never construct a raw `Color`/`Style` for something a
//! `Theme` field already names; extend `Theme` instead.

use std::collections::HashMap;
use std::time::Duration;

use ratatui_core::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui_core::style::{Color, Modifier, Style};

use crate::caps::{degrade_color, Capabilities};
use tm_types::Timestamp;

/// The palette and semantic text styles for one theme (e.g. "dark", "light", "high-contrast").
///
/// Fields are the semantic roles components style against, never a raw `Color` chosen ad hoc in
/// a widget. `Default` is a boring, fully-specified placeholder (`Color::Reset` everywhere) used
/// by tests and by any context with no real theme yet — it is intentionally not "the" tm look;
/// see [`Theme::dark`] for that, which is a genuine design decision left to this stub's
/// implementer, not something this scaffold should pre-decide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// A short identifier for the theme, e.g. `"dark"`.
    pub name: &'static str,
    /// The base background colour.
    pub background: Color,
    /// The base foreground (text) colour.
    pub foreground: Color,
    /// A raised surface one step off `background`, for content that should read as a distinct
    /// block (fenced code, the echoed user message band). Always paired with an explicit
    /// `foreground` by callers, never with the terminal's default colour, so the pair stays legible
    /// whatever the terminal's own background is.
    pub surface: Color,
    /// The accent colour used for primary emphasis (active tab, primary button, ...).
    pub accent: Color,
    /// A de-emphasized colour for secondary text (timestamps, hints, disabled items).
    pub muted: Color,
    /// The colour for positive/success states (verification passed, build green).
    pub success: Color,
    /// The colour for cautionary states (a ticket blocked, a stale lease).
    pub warning: Color,
    /// The colour for negative/error states (verification failed, a crashed session).
    pub danger: Color,
    /// The style applied to a component's border/indicator when it holds keyboard focus.
    pub focus_border: Style,
    /// The style applied to the selected row/item within a focused list-like widget.
    pub selection: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            name: "placeholder",
            background: Color::Reset,
            foreground: Color::Reset,
            surface: Color::Reset,
            accent: Color::Reset,
            muted: Color::Reset,
            success: Color::Reset,
            warning: Color::Reset,
            danger: Color::Reset,
            focus_border: Style::new(),
            selection: Style::new(),
        }
    }
}

impl Theme {
    /// The built-in dark theme, tm's default.
    ///
    /// A warm near-black background rather than pure `#000000` (which crushes anti-aliasing on
    /// most terminal fonts and reads as a hole rather than a surface), an off-white foreground
    /// (pure white vibrates against it), and a single violet accent that stays legible at every
    /// rung of [`crate::caps::ColorSupport`] once run through [`Theme::degraded`]. `focus_border`
    /// and `selection` each pair colour with a modifier (bold / underline) so which component
    /// holds focus, or which row is selected, is never encoded by colour alone — the same
    /// requirement [`Theme::degraded`] exists to preserve once colour itself is stripped away.
    pub fn dark() -> Self {
        let background = Color::Rgb(0x17, 0x18, 0x1c);
        let foreground = Color::Rgb(0xe6, 0xe6, 0xea);
        let surface = Color::Rgb(0x26, 0x28, 0x30);
        let accent = Color::Rgb(0x9d, 0x7c, 0xf5);
        let muted = Color::Rgb(0x7a, 0x7d, 0x8c);
        let success = Color::Rgb(0x4f, 0xd6, 0x9b);
        let warning = Color::Rgb(0xf0, 0xb4, 0x4c);
        let danger = Color::Rgb(0xf0, 0x6a, 0x6a);
        Theme {
            name: "dark",
            background,
            foreground,
            surface,
            accent,
            muted,
            success,
            warning,
            danger,
            focus_border: Style::new().fg(accent).add_modifier(Modifier::BOLD),
            selection: Style::new()
                .fg(background)
                .bg(accent)
                .add_modifier(Modifier::BOLD),
        }
    }

    /// The built-in light theme: the same semantic roles and the same accent hue as
    /// [`Theme::dark`], re-balanced for a light surface rather than treated as an independent
    /// design. A soft off-white background (not pure white, for the same anti-aliasing reason
    /// [`Theme::dark`] avoids pure black) and a deep near-black foreground.
    pub fn light() -> Self {
        let background = Color::Rgb(0xfa, 0xf9, 0xfc);
        let foreground = Color::Rgb(0x1c, 0x1d, 0x22);
        let surface = Color::Rgb(0xee, 0xec, 0xf3);
        let accent = Color::Rgb(0x6d, 0x4a, 0xd6);
        let muted = Color::Rgb(0x6b, 0x6e, 0x7a);
        let success = Color::Rgb(0x1f, 0x8f, 0x63);
        let warning = Color::Rgb(0xa8, 0x6a, 0x00);
        let danger = Color::Rgb(0xc9, 0x37, 0x37);
        Theme {
            name: "light",
            background,
            foreground,
            surface,
            accent,
            muted,
            success,
            warning,
            danger,
            focus_border: Style::new().fg(accent).add_modifier(Modifier::BOLD),
            selection: Style::new()
                .fg(background)
                .bg(accent)
                .add_modifier(Modifier::BOLD),
        }
    }

    /// Degrade every colour in this theme to what `caps` can actually render.
    ///
    /// Every field is either a bare `Color` or a `Style` whose only colour components are
    /// `fg`/`bg` (this crate never sets `underline_color`), so degrading the theme is degrading
    /// each of those, unconditionally — [`crate::caps::degrade_color`] is already the identity
    /// for `ColorSupport::TrueColor`, so this is safe to call on every frame rather than only
    /// when `caps` is known to be constrained.
    pub fn degraded(&self, caps: &Capabilities) -> Theme {
        Theme {
            name: self.name,
            background: degrade_color(caps, self.background),
            foreground: degrade_color(caps, self.foreground),
            // Below 256 colours the nearest match for a one-step-off surface is plain black or
            // white, which reads as a hole in the screen rather than a subtle block — drop the
            // band entirely instead and let layout (indent, gutter) carry the distinction.
            surface: match caps.color {
                crate::caps::ColorSupport::TrueColor | crate::caps::ColorSupport::Ansi256 => {
                    degrade_color(caps, self.surface)
                }
                _ => Color::Reset,
            },
            accent: degrade_color(caps, self.accent),
            muted: degrade_color(caps, self.muted),
            success: degrade_color(caps, self.success),
            warning: degrade_color(caps, self.warning),
            danger: degrade_color(caps, self.danger),
            focus_border: degrade_style(caps, self.focus_border),
            selection: degrade_style(caps, self.selection),
        }
    }
}

/// Degrade a [`Style`]'s colour components through [`crate::caps::degrade_color`], leaving
/// modifiers (bold, underline, ...) untouched — those are exactly the channel meaning still
/// travels through once colour has been stripped to [`crate::caps::ColorSupport::NoColor`].
fn degrade_style(caps: &Capabilities, style: Style) -> Style {
    Style {
        fg: style.fg.map(|c| degrade_color(caps, c)),
        bg: style.bg.map(|c| degrade_color(caps, c)),
        ..style
    }
}

/// A rectangular split of an area into named regions: the app-level analogue of ratatui's
/// `Layout`/`Constraint`, but returning components' own named slots instead of an index-ordered
/// `Vec<Rect>` a caller has to remember the meaning of.
#[derive(Debug, Clone, Default)]
pub struct Slots {
    named: HashMap<&'static str, Rect>,
}

impl Slots {
    /// The `Rect` assigned to `name`, or a zero-sized `Rect` when `name` was not part of the
    /// split — a missing slot renders nothing rather than panicking.
    pub fn get(&self, name: &str) -> Rect {
        self.named.get(name).copied().unwrap_or_default()
    }
}

/// Split `area` vertically into rows sized by relative weight (`(name, weight)` pairs, akin to
/// `Constraint::Ratio(weight, total)`).
///
/// A `weight` of `0` collapses that slot to zero height rather than dividing by it — useful for
/// a name a caller wants to conditionally hide without restructuring the whole split. An empty
/// `names` (or one whose weights all collapse to zero, e.g. every weight `0`) returns an empty
/// `Slots` rather than dividing by a zero total.
pub fn split_vertical(area: Rect, names: &[(&'static str, u32)]) -> Slots {
    split(area, Direction::Vertical, names)
}

/// Split `area` horizontally by relative weight; see [`split_vertical`].
pub fn split_horizontal(area: Rect, names: &[(&'static str, u32)]) -> Slots {
    split(area, Direction::Horizontal, names)
}

/// Shared implementation of [`split_vertical`]/[`split_horizontal`]: ratatui's `Layout` already
/// solves proportional splits correctly (rounding, minimum-size clamping); this only adds the
/// name-preserving zip `Slots::get` depends on.
fn split(area: Rect, direction: Direction, names: &[(&'static str, u32)]) -> Slots {
    if names.is_empty() {
        return Slots::default();
    }
    let total: u32 = names.iter().map(|(_, weight)| *weight).sum();
    if total == 0 {
        return Slots::default();
    }
    let constraints: Vec<Constraint> = names
        .iter()
        .map(|(_, weight)| Constraint::Ratio(*weight, total))
        .collect();
    let rects = Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area);
    let named = names
        .iter()
        .zip(rects.iter())
        .map(|((name, _), rect)| (*name, *rect))
        .collect();
    Slots { named }
}

/// A named easing curve: the shape `t` (a linear `[0.0, 1.0]` fraction of an [`Animation`]'s
/// duration) is remapped through before it becomes the interpolation fraction, so "counting
/// toward a target" and "sliding a panel in" can feel like motion instead of a mechanical ramp.
///
/// Every variant is a pure function `[0.0, 1.0] -> [0.0, 1.0]` with `f(0) = 0` and `f(1) = 1`, so
/// composing one with [`Animation::value_at`]'s own clamping keeps the whole curve inside
/// `from..=to` — never overshooting past `to` the way a spring curve would, which is deliberate:
/// nothing in this crate's default look should imply a value passed its target and bounced back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Easing {
    /// No remapping: constant velocity. The right choice for anything mechanical — a progress
    /// bar tracking real, externally-measured progress rather than a decorative transition.
    #[default]
    Linear,
    /// Starts slow, accelerates. Reads as "departing" — good for a panel dismissing itself.
    EaseIn,
    /// Starts fast, decelerates into the target. Reads as "arriving" — the default for a value
    /// counting up to a new total, or a panel sliding into place: it should feel like it is
    /// settling, not still moving when it stops.
    EaseOut,
    /// Slow, fast, slow: symmetric acceleration then deceleration. The tasteful middle ground
    /// for anything that both enters and must not feel abrupt at either end.
    EaseInOut,
}

impl Easing {
    /// Remap `t` (already clamped to `[0.0, 1.0]` by the caller) through this curve.
    ///
    /// Quadratic power curves rather than cubic/spring: cheap to evaluate every frame, and
    /// smooth enough that the difference from a higher-order curve is not perceptible at
    /// terminal frame rates.
    fn apply(self, t: f64) -> f64 {
        match self {
            Easing::Linear => t,
            Easing::EaseIn => t * t,
            Easing::EaseOut => t * (2.0 - t),
            Easing::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    let u = -2.0 * t + 2.0;
                    1.0 - u * u / 2.0
                }
            }
        }
    }
}

/// A running animation: interpolates a value over time using the injected clock rather than a
/// direct wall-clock read, so replays and snapshot tests stay deterministic
/// (`tm_types::clock` is the only place allowed to call `SystemTime::now`/`Instant::now`). This
/// is the one primitive every visible motion in this crate is built from — a progress bar
/// filling in, a counter counting toward its target, a panel fading or sliding into place — so
/// that every one of them is, per the crate's brief, a pure function of `(from, to, elapsed)`
/// and therefore snapshot-testable with an injected clock rather than a live timer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Animation {
    /// The value at `started_at`.
    pub from: f64,
    /// The value at `started_at + duration`.
    pub to: f64,
    /// When this animation began, per the injected clock.
    pub started_at: Timestamp,
    /// How long the animation runs.
    pub duration: Duration,
    /// The curve `progress` is remapped through before interpolating `from..=to`.
    pub easing: Easing,
}

impl Animation {
    /// A linear animation from `from` to `to`, starting at `started_at` and running `duration`.
    pub fn new(from: f64, to: f64, started_at: Timestamp, duration: Duration) -> Self {
        Animation {
            from,
            to,
            started_at,
            duration,
            easing: Easing::Linear,
        }
    }

    /// This animation with `easing` substituted for [`Easing::Linear`].
    pub fn with_easing(self, easing: Easing) -> Self {
        Animation { easing, ..self }
    }

    /// How far through the animation `now` falls, as a linear (un-eased) fraction clamped to
    /// `[0.0, 1.0]`. A zero or negative `duration` is treated as already finished (`1.0`) rather
    /// than dividing by zero, since "instant" is the only sensible reading of a zero-length
    /// animation.
    pub fn progress(&self, now: Timestamp) -> f64 {
        let duration_millis = self.duration.as_millis();
        if duration_millis == 0 {
            return 1.0;
        }
        let elapsed = now.millis_since(self.started_at).max(0) as f64;
        (elapsed / duration_millis as f64).clamp(0.0, 1.0)
    }

    /// The interpolated value at `now`: `self.easing` applied to [`Animation::progress`], lerped
    /// between `from` and `to`. Clamped to `[from, to]` (or `[to, from]`, for a descending
    /// animation) outside `[started_at, started_at + duration]` by construction, since `progress`
    /// itself is already clamped.
    pub fn value_at(&self, now: Timestamp) -> f64 {
        let t = self.easing.apply(self.progress(now));
        self.from + (self.to - self.from) * t
    }

    /// True once `now` is at or past `started_at + duration`.
    pub fn is_finished(&self, now: Timestamp) -> bool {
        self.progress(now) >= 1.0
    }
}

/// The independent kill switches that must each disable animation, per this crate's brief:
/// a user's reduce-motion preference, a `NO_COLOR`-style environment opt-out (conventionally
/// `NO_ANIMATION`/`TM_NO_ANIMATION`; reading the actual environment is `runtime.rs`'s job, not
/// this pure module's), and a missed frame budget — falling behind is itself a reason to stop
/// spending frame time on decoration rather than the content it is decorating.
///
/// Threading this through as an explicit, inspectable value (rather than a bare `bool`) is
/// deliberate: a widget that wants to explain *why* it settled ("motion off") can, and a caller
/// building this from three unrelated sources cannot accidentally collapse them into an
/// unreadable single flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MotionPolicy {
    /// The user asked for reduced motion (an accessibility/OS-level or `tm`-config preference).
    pub reduce_motion: bool,
    /// The `NO_COLOR`-style environment opt-out was set.
    pub no_animation_env: bool,
    /// The runtime missed its frame budget on a recent frame.
    pub frame_budget_missed: bool,
}

impl MotionPolicy {
    /// A policy with every kill switch off: animation runs normally.
    pub fn enabled() -> Self {
        MotionPolicy::default()
    }

    /// True only when none of the three kill switches are set.
    pub fn motion_enabled(&self) -> bool {
        !(self.reduce_motion || self.no_animation_env || self.frame_budget_missed)
    }
}

impl Animation {
    /// [`Animation::value_at`], unless `policy` disables motion, in which case the animation is
    /// skipped straight to `to` — a value that counts toward its target rather than jumping
    /// still has to land instantly the moment motion is off, per this crate's hard rule that
    /// meaning (here: the final value) must never depend on an animation actually playing.
    pub fn settle(&self, now: Timestamp, policy: MotionPolicy) -> f64 {
        if policy.motion_enabled() {
            self.value_at(now)
        } else {
            self.to
        }
    }
}

/// Linearly blend two colours by `t` (clamped to `[0.0, 1.0]`), for a fading panel or a
/// highlight that should ease toward a target colour rather than cutting to it.
///
/// Only `Color::Rgb` can be blended meaningfully; every other `Color` variant (the named ANSI
/// colours, `Reset`, `Indexed`) has no interpolatable channel to blend, so this snaps at the
/// curve's midpoint (`t < 0.5` keeps `from`, otherwise takes `to`) rather than inventing an RGB
/// value for a named colour a degraded [`Theme`] chose specifically because it is representable
/// at a low [`crate::caps::ColorSupport`] level.
pub fn blend(from: Color, to: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) => {
            let lerp = |a: u8, b: u8| -> u8 {
                (a as f64 + (b as f64 - a as f64) * t)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            Color::Rgb(lerp(fr, tr), lerp(fg, tg), lerp(fb, tb))
        }
        _ => {
            if t < 0.5 {
                from
            } else {
                to
            }
        }
    }
}

/// Linearly interpolate a `Rect`'s position and size by `t` (clamped to `[0.0, 1.0]`), for a
/// panel sliding or resizing into place. Each field rounds independently to the nearest cell, so
/// this is a snapshot-testable pure function of `(from, to, t)` rather than something that
/// depends on the order fields happen to be visited in.
pub fn slide_rect(from: Rect, to: Rect, t: f64) -> Rect {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: u16, b: u16| -> u16 { (a as f64 + (b as f64 - a as f64) * t).round() as u16 };
    Rect {
        x: lerp(from.x, to.x),
        y: lerp(from.y, to.y),
        width: lerp(from.width, to.width),
        height: lerp(from.height, to.height),
    }
}

/// A looping sequence of frames driven by the same injected-clock discipline as [`Animation`]:
/// which frame is "current" is a pure function of elapsed time, never a mutable counter ticked
/// once per render call (which would desync from real elapsed time the instant a frame is
/// skipped or a render is slow — exactly the flaky-connection case this crate is built for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spinner {
    /// The glyphs to cycle through, in order.
    pub frames: &'static [&'static str],
    /// How long each frame is shown before advancing to the next.
    pub frame_duration: Duration,
}

impl Spinner {
    /// The classic braille dot spinner: dense, legible even at one cell wide, and ASCII-safe to
    /// degrade to (see [`Spinner::frame`]'s caller, which owns the ASCII fallback decision since
    /// that depends on [`crate::caps::UnicodeSupport`], not on this module).
    pub const BRAILLE: &'static [&'static str] =
        &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    /// The glyph to show at `now`, given the spinner started at `started_at`.
    ///
    /// Under a [`MotionPolicy`] that disables motion, this always returns the first frame:
    /// a spinner that cannot move should still communicate "in progress" by being present, just
    /// not by moving — the same "settle, do not disappear" rule [`Animation::settle`] follows.
    pub fn frame(
        &self,
        started_at: Timestamp,
        now: Timestamp,
        policy: MotionPolicy,
    ) -> &'static str {
        let empty = self.frames.is_empty();
        if empty {
            return "";
        }
        if !policy.motion_enabled() {
            return self.frames[0];
        }
        let frame_millis = self.frame_duration.as_millis().max(1) as i64;
        let elapsed = now.millis_since(started_at).max(0);
        let index = ((elapsed / frame_millis) as usize) % self.frames.len();
        self.frames[index]
    }
}

/// Shrink `area` by `horizontal` columns on each side and `vertical` rows on each side —
/// the "give this panel some breathing room" primitive every other layout helper in this module
/// builds on. Saturates to a zero-sized `Rect` rather than underflowing when the padding exceeds
/// `area`'s size, the same "render nothing, never panic" contract as [`Slots::get`].
pub fn pad(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    area.inner(Margin::new(horizontal, vertical))
}

/// One track of a [`flex_vertical`]/[`flex_horizontal`] split: either an exact size in cells, or
/// a share of whatever space remains once every [`Track::Fixed`] track has been subtracted.
///
/// This is the "flex-ish helper for the common cases" the layout above `ratatui::Layout` needs:
/// a sidebar/header/status-line sized in cells alongside a body that should simply take the
/// rest, without the caller hand-computing percentages the way raw [`Constraint::Ratio`] would
/// force through [`split_vertical`]/[`split_horizontal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// Exactly this many cells, regardless of how much space is available.
    Fixed(u16),
    /// This many parts of whatever space is left after every [`Track::Fixed`] track is
    /// subtracted, distributed the way `Constraint::Fill` distributes remaining space.
    Fill(u16),
}

impl From<Track> for Constraint {
    fn from(track: Track) -> Constraint {
        match track {
            Track::Fixed(cells) => Constraint::Length(cells),
            Track::Fill(weight) => Constraint::Fill(weight),
        }
    }
}

/// Split `area` vertically into named tracks mixing fixed sizes and flexible fills; see
/// [`Track`]. `spacing` inserts that many blank cells between adjacent tracks (not before the
/// first or after the last), matching `ratatui::Layout::spacing`.
pub fn flex_vertical(area: Rect, spacing: u16, tracks: &[(&'static str, Track)]) -> Slots {
    flex(area, Direction::Vertical, spacing, tracks)
}

/// Split `area` horizontally into named tracks mixing fixed sizes and flexible fills; see
/// [`flex_vertical`].
pub fn flex_horizontal(area: Rect, spacing: u16, tracks: &[(&'static str, Track)]) -> Slots {
    flex(area, Direction::Horizontal, spacing, tracks)
}

fn flex(area: Rect, direction: Direction, spacing: u16, tracks: &[(&'static str, Track)]) -> Slots {
    if tracks.is_empty() {
        return Slots::default();
    }
    let constraints: Vec<Constraint> = tracks.iter().map(|(_, track)| (*track).into()).collect();
    let rects = Layout::default()
        .direction(direction)
        .constraints(constraints)
        .spacing(spacing)
        .split(area);
    let named = tracks
        .iter()
        .zip(rects.iter())
        .map(|((name, _), rect)| (*name, *rect))
        .collect();
    Slots { named }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_is_fully_specified() {
        let theme = Theme::default();
        assert_eq!(theme.name, "placeholder");
        assert_eq!(theme.background, Color::Reset);
    }

    #[test]
    fn slots_missing_name_is_zero_sized_not_a_panic() {
        let slots = Slots::default();
        assert_eq!(slots.get("nope"), Rect::default());
    }

    #[test]
    fn dark_and_light_differ_but_share_the_accent_hue() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert_eq!(dark.name, "dark");
        assert_eq!(light.name, "light");
        assert_ne!(dark.background, light.background);
        assert_ne!(dark.foreground, light.foreground);
    }

    #[test]
    fn split_vertical_preserves_names_and_covers_the_area() {
        let area = Rect::new(0, 0, 40, 30);
        let slots = split_vertical(area, &[("header", 1), ("body", 2)]);
        let header = slots.get("header");
        let body = slots.get("body");
        assert_eq!(header.x, 0);
        assert_eq!(header.width, 40);
        assert_eq!(body.width, 40);
        assert_eq!(header.height + body.height, area.height);
        assert_eq!(body.y, header.y + header.height);
    }

    #[test]
    fn split_with_zero_total_weight_is_empty_not_a_panic() {
        let area = Rect::new(0, 0, 10, 10);
        let slots = split_horizontal(area, &[("a", 0), ("b", 0)]);
        assert_eq!(slots.get("a"), Rect::default());
    }

    #[test]
    fn pad_shrinks_symmetrically_and_saturates() {
        let area = Rect::new(0, 0, 10, 10);
        let padded = pad(area, 2, 1);
        assert_eq!(padded, Rect::new(2, 1, 6, 8));
        let over_padded = pad(Rect::new(0, 0, 1, 1), 5, 5);
        assert_eq!(
            over_padded.width, 0,
            "padding past the area's size saturates, not panics"
        );
        assert_eq!(over_padded.height, 0);
    }

    #[test]
    fn flex_vertical_gives_fixed_tracks_exactly_their_size() {
        let area = Rect::new(0, 0, 10, 20);
        let slots = flex_vertical(
            area,
            0,
            &[("header", Track::Fixed(3)), ("body", Track::Fill(1))],
        );
        assert_eq!(slots.get("header").height, 3);
        assert_eq!(slots.get("body").height, 17);
    }

    #[test]
    fn animation_lerps_and_clamps() {
        let start = Timestamp::EPOCH;
        let anim = Animation::new(0.0, 10.0, start, Duration::from_millis(1000));
        assert_eq!(anim.value_at(start), 0.0);
        assert_eq!(anim.value_at(start.plus_millis(500)), 5.0);
        assert_eq!(anim.value_at(start.plus_millis(1000)), 10.0);
        assert_eq!(
            anim.value_at(start.plus_millis(5000)),
            10.0,
            "clamps past the end"
        );
        assert_eq!(
            anim.value_at(start.plus_millis(-500)),
            0.0,
            "clamps before the start"
        );
        assert!(!anim.is_finished(start.plus_millis(999)));
        assert!(anim.is_finished(start.plus_millis(1000)));
    }

    #[test]
    fn zero_duration_animation_is_instantly_finished() {
        let start = Timestamp::EPOCH;
        let anim = Animation::new(0.0, 1.0, start, Duration::from_millis(0));
        assert!(anim.is_finished(start));
        assert_eq!(anim.value_at(start), 1.0);
    }

    #[test]
    fn easing_curves_agree_at_the_endpoints() {
        let start = Timestamp::EPOCH;
        for easing in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            let anim =
                Animation::new(0.0, 1.0, start, Duration::from_millis(1000)).with_easing(easing);
            assert_eq!(anim.value_at(start), 0.0, "{easing:?} must start at `from`");
            assert_eq!(
                anim.value_at(start.plus_millis(1000)),
                1.0,
                "{easing:?} must end at `to`"
            );
        }
    }

    #[test]
    fn ease_out_is_ahead_of_linear_at_the_midpoint() {
        let start = Timestamp::EPOCH;
        let linear = Animation::new(0.0, 1.0, start, Duration::from_millis(1000));
        let eased = linear.with_easing(Easing::EaseOut);
        let now = start.plus_millis(500);
        assert!(eased.value_at(now) > linear.value_at(now));
    }

    #[test]
    fn motion_policy_disables_on_any_kill_switch() {
        assert!(MotionPolicy::enabled().motion_enabled());
        assert!(!MotionPolicy {
            reduce_motion: true,
            ..MotionPolicy::default()
        }
        .motion_enabled());
        assert!(!MotionPolicy {
            no_animation_env: true,
            ..MotionPolicy::default()
        }
        .motion_enabled());
        assert!(!MotionPolicy {
            frame_budget_missed: true,
            ..MotionPolicy::default()
        }
        .motion_enabled());
    }

    #[test]
    fn settle_snaps_to_target_when_motion_disabled() {
        let start = Timestamp::EPOCH;
        let anim = Animation::new(0.0, 10.0, start, Duration::from_millis(1000));
        let now = start.plus_millis(1); // barely started
        assert_eq!(
            anim.settle(now, MotionPolicy::enabled()),
            anim.value_at(now)
        );
        let disabled = MotionPolicy {
            reduce_motion: true,
            ..MotionPolicy::default()
        };
        assert_eq!(anim.settle(now, disabled), 10.0);
    }

    #[test]
    fn blend_interpolates_rgb_and_snaps_named_colors() {
        let from = Color::Rgb(0, 0, 0);
        let to = Color::Rgb(100, 200, 50);
        assert_eq!(blend(from, to, 0.0), from);
        assert_eq!(blend(from, to, 1.0), to);
        assert_eq!(blend(from, to, 0.5), Color::Rgb(50, 100, 25));
        assert_eq!(blend(Color::Red, Color::Blue, 0.0), Color::Red);
        assert_eq!(blend(Color::Red, Color::Blue, 1.0), Color::Blue);
    }

    #[test]
    fn slide_rect_interpolates_every_field() {
        let from = Rect::new(0, 0, 0, 10);
        let to = Rect::new(10, 20, 30, 10);
        assert_eq!(slide_rect(from, to, 0.0), from);
        assert_eq!(slide_rect(from, to, 1.0), to);
        assert_eq!(slide_rect(from, to, 0.5), Rect::new(5, 10, 15, 10));
    }

    #[test]
    fn spinner_advances_deterministically_with_the_clock() {
        let start = Timestamp::EPOCH;
        let spinner = Spinner {
            frames: Spinner::BRAILLE,
            frame_duration: Duration::from_millis(80),
        };
        assert_eq!(
            spinner.frame(start, start, MotionPolicy::enabled()),
            Spinner::BRAILLE[0]
        );
        assert_eq!(
            spinner.frame(start, start.plus_millis(80), MotionPolicy::enabled()),
            Spinner::BRAILLE[1]
        );
    }

    #[test]
    fn spinner_freezes_on_first_frame_when_motion_disabled() {
        let start = Timestamp::EPOCH;
        let spinner = Spinner {
            frames: Spinner::BRAILLE,
            frame_duration: Duration::from_millis(80),
        };
        let disabled = MotionPolicy {
            reduce_motion: true,
            ..MotionPolicy::default()
        };
        assert_eq!(
            spinner.frame(start, start.plus_millis(5000), disabled),
            Spinner::BRAILLE[0]
        );
    }
}
