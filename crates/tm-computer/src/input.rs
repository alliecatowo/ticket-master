//! The platform-independent input model: clicks, drags, typed text, key chords and scroll.
//!
//! Everything in this module is pure data plus pure functions — chord parsing, coordinate math,
//! drag path interpolation — so it can be exercised on any machine, with no live display and no
//! backend at all. [`crate::backend::Backend::input`] is the only place these values ever touch
//! real OS event injection.

use crate::ComputerError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A point in screen (or window-relative) coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal offset in pixels.
    pub x: f64,
    /// Vertical offset in pixels.
    pub y: f64,
}

impl Point {
    /// Build a point.
    pub fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }
}

/// An axis-aligned rectangle, e.g. a display or window bound.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    /// Top-left corner.
    pub origin: Point,
    /// Width in pixels.
    pub width: f64,
    /// Height in pixels.
    pub height: f64,
}

impl Rect {
    /// Build a rect from origin and size.
    pub fn new(origin: Point, width: f64, height: f64) -> Self {
        Rect {
            origin,
            width,
            height,
        }
    }

    /// True when `p` falls within this rect, inclusive of its edges.
    // IMPL: p.x in [origin.x, origin.x + width] and p.y in [origin.y, origin.y + height].
    pub fn contains(&self, p: Point) -> bool {
        todo!("bounds check per the IMPL note")
    }
}

/// Clamp `p` into `bounds`, so a synthesized event never targets off-screen coordinates.
// IMPL: clamp p.x to [bounds.origin.x, bounds.origin.x + bounds.width], same for y. Pure, no
// panics, total over all finite inputs (NaN is left as NaN — callers must reject it upstream).
pub fn clamp_point(p: Point, bounds: Rect) -> Point {
    todo!("clamp per the IMPL note")
}

/// Interpolate `steps` intermediate points from `from` to `to`, inclusive of both ends, for
/// backends that synthesize a drag as a sequence of move events rather than a single jump.
// IMPL: linear interpolation; steps == 0 or 1 returns just [from, to]; steps >= 2 returns
// from, steps-1 evenly spaced midpoints, to (len == steps + 1). Pure, deterministic, no clock.
pub fn lerp_drag_path(from: Point, to: Point, steps: usize) -> Vec<Point> {
    todo!("linear interpolation per the IMPL note")
}

/// Which mouse button an action targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// The primary (usually left) button.
    Left,
    /// The secondary (usually right) button.
    Right,
    /// The middle button or wheel click.
    Middle,
}

/// A keyboard modifier, canonically ordered so a [`KeyChord`] always renders the same string
/// regardless of the order it was parsed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Modifier {
    /// Control.
    Ctrl,
    /// Alt / Option.
    Alt,
    /// Shift.
    Shift,
    /// Command (macOS) / Super / Windows key (X11, named for parity but never targeted there).
    Cmd,
}

/// A parsed key chord, e.g. `cmd+shift+4` or `ctrl+alt+t`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyChord {
    /// Every modifier held, in canonical (sorted) order.
    pub modifiers: BTreeSet<Modifier>,
    /// The non-modifier key, lowercased (e.g. `"4"`, `"t"`, `"return"`, `"f5"`).
    pub key: String,
}

impl KeyChord {
    /// Parse a chord string of the form `mod+mod+...+key`, e.g. `"cmd+shift+4"`.
    // IMPL: split on '+'; the last segment is the key, every prior segment must match a known
    // modifier name (case-insensitively; accept "command"/"cmd", "option"/"alt",
    // "control"/"ctrl", "shift", and on non-macOS also "super"/"meta" as aliases for Cmd).
    // Reject empty input, a chord with no key segment, an unknown modifier name, or a duplicate
    // modifier, each with a ComputerError::Parse naming the offending segment. The key segment
    // itself is not validated against a keycode table here — that mapping is backend-specific
    // and happens in `Backend::input`.
    pub fn parse(s: &str) -> Result<KeyChord, ComputerError> {
        todo!("chord parsing per the IMPL note")
    }

    /// Render this chord back to its canonical string form (modifiers in [`Modifier`] order,
    /// lowercase, `+`-joined), the inverse of [`KeyChord::parse`] for any chord it can produce.
    pub fn to_canonical_string(&self) -> String {
        todo!("canonical rendering: join modifier names in enum order then the key, '+'-joined")
    }
}

/// A scroll or pan delta, in platform-agnostic "lines" (backends translate to pixels or wheel
/// notches as appropriate).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScrollDelta {
    /// Horizontal delta; positive scrolls right.
    pub dx: f64,
    /// Vertical delta; positive scrolls down.
    pub dy: f64,
}

/// A target for an input action: either an absolute point or a stable element ref from a prior
/// [`crate::session::ComputerSession::snapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputTarget {
    /// A raw screen coordinate.
    Point(Point),
    /// A ref into the last observed accessibility element tree.
    ElementRef(String),
}

/// One platform-independent input action. [`crate::backend::Backend::input`] translates each
/// variant into the platform's native event injection calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputAction {
    /// A single click of `button` at `target`.
    Click {
        /// Where to click.
        target: InputTarget,
        /// Which button.
        button: MouseButton,
    },
    /// A double click at `target`.
    DoubleClick {
        /// Where to click.
        target: InputTarget,
    },
    /// A right click at `target`. Equivalent to `Click { button: Right, .. }`, offered
    /// separately because it is by far the most common non-left action.
    RightClick {
        /// Where to click.
        target: InputTarget,
    },
    /// A press-move-release drag from one target to another.
    Drag {
        /// The press point.
        from: InputTarget,
        /// The release point.
        to: InputTarget,
    },
    /// Type literal text via the platform's Unicode input path (not per-key synthesis).
    TypeText(String),
    /// Press and release a parsed key chord.
    KeyChord(KeyChord),
    /// Scroll or pan at `target` by `delta`.
    Scroll {
        /// Where to scroll.
        target: InputTarget,
        /// How much, and in which direction.
        delta: ScrollDelta,
    },
}

