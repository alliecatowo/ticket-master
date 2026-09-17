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

use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Style};

use crate::caps::Capabilities;
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
    /// IMPL: choose concrete `Color::Rgb` values and `Style` combinations (bold/underline for
    /// `focus_border`/`selection` as well as colour) — this is a genuine visual design decision
    /// left to this stub's implementer, per the crate's brief to make the result "ours to make
    /// beautiful" (D-002), not encoded in the scaffold. Degrade through [`Theme::degraded`] for
    /// terminals that cannot render it rather than hard-coding a second low-colour theme.
    pub fn dark() -> Self {
        todo!("choose the dark palette per the IMPL note above")
    }

    /// Degrade every colour in this theme to what `caps` can actually render.
    ///
    /// IMPL: map each `Color` field (and the colour components of `focus_border`/`selection`)
    /// through `crate::caps::degrade_color(caps, ...)`; non-colour style attributes (bold,
    /// underline) pass through unchanged.
    pub fn degraded(&self, caps: &Capabilities) -> Theme {
        let _ = caps;
        todo!("call caps::degrade_color on every colour field per the IMPL note above")
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
/// IMPL: build a `ratatui_core::layout::Layout` with `Direction::Vertical` and one
/// `Constraint::Ratio(weight, total_weight)` per entry of `names`, split `area` with it, and zip
/// the resulting `Rect`s back onto each name into a `Slots`.
pub fn split_vertical(area: Rect, names: &[(&'static str, u32)]) -> Slots {
    let _ = (area, names);
    todo!("split `area` vertically by weight per the IMPL note above")
}

/// Split `area` horizontally by relative weight; see [`split_vertical`].
pub fn split_horizontal(area: Rect, names: &[(&'static str, u32)]) -> Slots {
    let _ = (area, names);
    todo!("split `area` horizontally by weight per the IMPL note above")
}

/// A running animation: interpolates a value over time using the injected clock rather than a
/// direct wall-clock read, so replays and snapshot tests stay deterministic
/// (`tm_types::clock` is the only place allowed to call `SystemTime::now`/`Instant::now`).
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
}

impl Animation {
    /// The interpolated value at `now`, linear between `from` and `to`, clamped to that range
    /// outside `[started_at, started_at + duration]`.
    ///
    /// IMPL: compute `elapsed = now.millis_since(self.started_at)` (see `tm_types::Timestamp`),
    /// clamp `elapsed / self.duration.as_millis()` to `[0.0, 1.0]`, and lerp `from..=to` by that
    /// fraction.
    pub fn value_at(&self, now: Timestamp) -> f64 {
        let _ = now;
        todo!("linearly interpolate per the IMPL note above")
    }

    /// True once `now` is at or past `started_at + duration`.
    pub fn is_finished(&self, now: Timestamp) -> bool {
        let _ = now;
        todo!("compare `now` to `self.started_at + self.duration` per the IMPL note above")
    }
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
}
