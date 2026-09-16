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
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.origin.x
            && p.x <= self.origin.x + self.width
            && p.y >= self.origin.y
            && p.y <= self.origin.y + self.height
    }
}

/// Clamp `p` into `bounds`, so a synthesized event never targets off-screen coordinates.
pub fn clamp_point(p: Point, bounds: Rect) -> Point {
    let clamp = |v: f64, min: f64, max: f64| -> f64 {
        if v.is_nan() {
            v
        } else {
            v.max(min).min(max)
        }
    };

    Point {
        x: clamp(p.x, bounds.origin.x, bounds.origin.x + bounds.width),
        y: clamp(p.y, bounds.origin.y, bounds.origin.y + bounds.height),
    }
}

/// Interpolate `steps` intermediate points from `from` to `to`, inclusive of both ends, for
/// backends that synthesize a drag as a sequence of move events rather than a single jump.
pub fn lerp_drag_path(from: Point, to: Point, steps: usize) -> Vec<Point> {
    if steps <= 1 {
        return vec![from, to];
    }

    let mut path = vec![from];
    for i in 1..steps {
        let t = i as f64 / steps as f64;
        path.push(Point {
            x: from.x + t * (to.x - from.x),
            y: from.y + t * (to.y - from.y),
        });
    }
    path.push(to);
    path
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

/// True when `s` (already lowercased) names a modifier alias, so a trailing chord segment of
/// this shape is a missing key, not a key literally named e.g. `"shift"`.
fn is_modifier_name(s: &str) -> bool {
    matches!(
        s,
        "ctrl" | "control" | "alt" | "option" | "shift" | "cmd" | "command" | "super" | "meta"
    )
}

impl KeyChord {
    /// Parse a chord string of the form `mod+mod+...+key`, e.g. `"cmd+shift+4"`.
    pub fn parse(s: &str) -> Result<KeyChord, ComputerError> {
        if s.is_empty() {
            return Err(ComputerError::Parse("empty chord string".to_string()));
        }

        let parts: Vec<&str> = s.split('+').collect();
        if parts.is_empty() || parts.iter().all(|p| p.is_empty()) {
            return Err(ComputerError::Parse("empty chord string".to_string()));
        }

        if parts.len() < 2 {
            return Err(ComputerError::Parse(
                "chord must have at least a modifier and a key".to_string(),
            ));
        }

        let key = parts[parts.len() - 1].to_lowercase();
        if key.is_empty() {
            return Err(ComputerError::Parse("empty key segment".to_string()));
        }
        if is_modifier_name(&key) {
            return Err(ComputerError::Parse(
                "chord must have at least a modifier and a key".to_string(),
            ));
        }

        let mut modifiers = BTreeSet::new();

        for &part in &parts[..parts.len() - 1] {
            if part.is_empty() {
                return Err(ComputerError::Parse(
                    "empty segment in chord string".to_string(),
                ));
            }

            let lower = part.to_lowercase();
            let modifier = match lower.as_str() {
                "ctrl" | "control" => Modifier::Ctrl,
                "alt" | "option" => Modifier::Alt,
                "shift" => Modifier::Shift,
                "cmd" | "command" => Modifier::Cmd,
                "super" | "meta" => Modifier::Cmd,
                _ => return Err(ComputerError::Parse(format!("unknown modifier: {}", part))),
            };

            if !modifiers.insert(modifier) {
                return Err(ComputerError::Parse(format!(
                    "duplicate modifier: {}",
                    part
                )));
            }
        }

        Ok(KeyChord { modifiers, key })
    }

    /// Render this chord back to its canonical string form (modifiers in [`Modifier`] order,
    /// lowercase, `+`-joined), the inverse of [`KeyChord::parse`] for any chord it can produce.
    pub fn to_canonical_string(&self) -> String {
        let mut parts = Vec::new();

        for modifier in &self.modifiers {
            let name = match modifier {
                Modifier::Ctrl => "ctrl",
                Modifier::Alt => "alt",
                Modifier::Shift => "shift",
                Modifier::Cmd => "cmd",
            };
            parts.push(name);
        }

        parts.push(&self.key);
        parts.join("+")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_contains_inside_bounds() {
        let rect = Rect::new(Point::new(10.0, 20.0), 100.0, 50.0);
        assert!(rect.contains(Point::new(50.0, 45.0)));
    }

    #[test]
    fn point_contains_on_origin_edge() {
        let rect = Rect::new(Point::new(10.0, 20.0), 100.0, 50.0);
        assert!(rect.contains(Point::new(10.0, 20.0)));
    }

    #[test]
    fn point_contains_on_far_edge() {
        let rect = Rect::new(Point::new(10.0, 20.0), 100.0, 50.0);
        assert!(rect.contains(Point::new(110.0, 70.0)));
    }

    #[test]
    fn point_contains_outside_x_negative() {
        let rect = Rect::new(Point::new(10.0, 20.0), 100.0, 50.0);
        assert!(!rect.contains(Point::new(9.9, 45.0)));
    }

    #[test]
    fn point_contains_outside_x_positive() {
        let rect = Rect::new(Point::new(10.0, 20.0), 100.0, 50.0);
        assert!(!rect.contains(Point::new(110.1, 45.0)));
    }

    #[test]
    fn point_contains_outside_y_negative() {
        let rect = Rect::new(Point::new(10.0, 20.0), 100.0, 50.0);
        assert!(!rect.contains(Point::new(50.0, 19.9)));
    }

    #[test]
    fn point_contains_outside_y_positive() {
        let rect = Rect::new(Point::new(10.0, 20.0), 100.0, 50.0);
        assert!(!rect.contains(Point::new(50.0, 70.1)));
    }

    #[test]
    fn clamp_point_inside_bounds() {
        let bounds = Rect::new(Point::new(0.0, 0.0), 100.0, 100.0);
        let p = Point::new(50.0, 50.0);
        let clamped = clamp_point(p, bounds);
        assert_eq!(clamped, Point::new(50.0, 50.0));
    }

    #[test]
    fn clamp_point_outside_left() {
        let bounds = Rect::new(Point::new(10.0, 0.0), 100.0, 100.0);
        let p = Point::new(5.0, 50.0);
        let clamped = clamp_point(p, bounds);
        assert_eq!(clamped, Point::new(10.0, 50.0));
    }

    #[test]
    fn clamp_point_outside_right() {
        let bounds = Rect::new(Point::new(0.0, 0.0), 100.0, 100.0);
        let p = Point::new(150.0, 50.0);
        let clamped = clamp_point(p, bounds);
        assert_eq!(clamped, Point::new(100.0, 50.0));
    }

    #[test]
    fn clamp_point_outside_top() {
        let bounds = Rect::new(Point::new(0.0, 10.0), 100.0, 100.0);
        let p = Point::new(50.0, 5.0);
        let clamped = clamp_point(p, bounds);
        assert_eq!(clamped, Point::new(50.0, 10.0));
    }

    #[test]
    fn clamp_point_outside_bottom() {
        let bounds = Rect::new(Point::new(0.0, 0.0), 100.0, 100.0);
        let p = Point::new(50.0, 150.0);
        let clamped = clamp_point(p, bounds);
        assert_eq!(clamped, Point::new(50.0, 100.0));
    }

    #[test]
    fn clamp_point_preserves_nan() {
        let bounds = Rect::new(Point::new(0.0, 0.0), 100.0, 100.0);
        let p = Point::new(f64::NAN, 50.0);
        let clamped = clamp_point(p, bounds);
        assert!(clamped.x.is_nan());
        assert_eq!(clamped.y, 50.0);
    }

    #[test]
    fn clamp_point_nan_y() {
        let bounds = Rect::new(Point::new(0.0, 0.0), 100.0, 100.0);
        let p = Point::new(50.0, f64::NAN);
        let clamped = clamp_point(p, bounds);
        assert_eq!(clamped.x, 50.0);
        assert!(clamped.y.is_nan());
    }

    #[test]
    fn lerp_drag_path_zero_steps() {
        let from = Point::new(0.0, 0.0);
        let to = Point::new(10.0, 10.0);
        let path = lerp_drag_path(from, to, 0);
        assert_eq!(path, vec![from, to]);
        assert_eq!(path.len(), 2);
    }

    #[test]
    fn lerp_drag_path_one_step() {
        let from = Point::new(0.0, 0.0);
        let to = Point::new(10.0, 10.0);
        let path = lerp_drag_path(from, to, 1);
        assert_eq!(path, vec![from, to]);
        assert_eq!(path.len(), 2);
    }

    #[test]
    fn lerp_drag_path_two_steps() {
        let from = Point::new(0.0, 0.0);
        let to = Point::new(10.0, 10.0);
        let path = lerp_drag_path(from, to, 2);
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], from);
        assert_eq!(path[2], to);
        assert_eq!(path[1], Point::new(5.0, 5.0));
    }

    #[test]
    fn lerp_drag_path_three_steps() {
        let from = Point::new(0.0, 0.0);
        let to = Point::new(9.0, 9.0);
        let path = lerp_drag_path(from, to, 3);
        assert_eq!(path.len(), 4);
        assert_eq!(path[0], from);
        assert_eq!(path[3], to);
        assert_eq!(path[1], Point::new(3.0, 3.0));
        assert_eq!(path[2], Point::new(6.0, 6.0));
    }

    #[test]
    fn lerp_drag_path_includes_both_endpoints() {
        let from = Point::new(1.0, 2.0);
        let to = Point::new(11.0, 12.0);
        let path = lerp_drag_path(from, to, 10);
        assert_eq!(path[0], from);
        assert_eq!(path[path.len() - 1], to);
    }

    #[test]
    fn key_chord_parse_single_modifier() {
        let chord = KeyChord::parse("cmd+4").unwrap();
        assert_eq!(chord.key, "4");
        assert!(chord.modifiers.contains(&Modifier::Cmd));
    }

    #[test]
    fn key_chord_parse_multiple_modifiers() {
        let chord = KeyChord::parse("cmd+shift+4").unwrap();
        assert_eq!(chord.key, "4");
        assert!(chord.modifiers.contains(&Modifier::Cmd));
        assert!(chord.modifiers.contains(&Modifier::Shift));
    }

    #[test]
    fn key_chord_parse_all_modifiers() {
        let chord = KeyChord::parse("ctrl+alt+shift+cmd+a").unwrap();
        assert_eq!(chord.key, "a");
        assert!(chord.modifiers.contains(&Modifier::Ctrl));
        assert!(chord.modifiers.contains(&Modifier::Alt));
        assert!(chord.modifiers.contains(&Modifier::Shift));
        assert!(chord.modifiers.contains(&Modifier::Cmd));
    }

    #[test]
    fn key_chord_parse_lowercase_key() {
        let chord = KeyChord::parse("cmd+A").unwrap();
        assert_eq!(chord.key, "a");
    }

    #[test]
    fn key_chord_parse_case_insensitive_modifiers() {
        let chord1 = KeyChord::parse("CMD+t").unwrap();
        let chord2 = KeyChord::parse("cmd+t").unwrap();
        assert_eq!(chord1, chord2);
    }

    #[test]
    fn key_chord_parse_control_alias() {
        let chord1 = KeyChord::parse("ctrl+a").unwrap();
        let chord2 = KeyChord::parse("control+a").unwrap();
        assert_eq!(chord1, chord2);
    }

    #[test]
    fn key_chord_parse_option_alias() {
        let chord1 = KeyChord::parse("alt+a").unwrap();
        let chord2 = KeyChord::parse("option+a").unwrap();
        assert_eq!(chord1, chord2);
    }

    #[test]
    fn key_chord_parse_command_alias() {
        let chord1 = KeyChord::parse("cmd+a").unwrap();
        let chord2 = KeyChord::parse("command+a").unwrap();
        assert_eq!(chord1, chord2);
    }

    #[test]
    fn key_chord_parse_super_alias() {
        let chord1 = KeyChord::parse("super+a").unwrap();
        let chord2 = KeyChord::parse("meta+a").unwrap();
        assert_eq!(chord1, chord2);
        assert!(chord1.modifiers.contains(&Modifier::Cmd));
    }

    #[test]
    fn key_chord_parse_function_key() {
        let chord = KeyChord::parse("cmd+f5").unwrap();
        assert_eq!(chord.key, "f5");
    }

    #[test]
    fn key_chord_parse_return_key() {
        let chord = KeyChord::parse("shift+return").unwrap();
        assert_eq!(chord.key, "return");
    }

    #[test]
    fn key_chord_parse_empty_string() {
        assert!(KeyChord::parse("").is_err());
    }

    #[test]
    fn key_chord_parse_no_key_segment() {
        assert!(KeyChord::parse("cmd+shift").is_err());
    }

    #[test]
    fn key_chord_parse_unknown_modifier() {
        assert!(KeyChord::parse("cmd+unknown+a").is_err());
    }

    #[test]
    fn key_chord_parse_duplicate_modifier() {
        assert!(KeyChord::parse("cmd+cmd+a").is_err());
    }

    #[test]
    fn key_chord_parse_duplicate_modifier_different_case() {
        assert!(KeyChord::parse("cmd+CMD+a").is_err());
    }

    #[test]
    fn key_chord_to_canonical_single_modifier() {
        let chord = KeyChord {
            modifiers: {
                let mut set = BTreeSet::new();
                set.insert(Modifier::Cmd);
                set
            },
            key: "4".to_string(),
        };
        assert_eq!(chord.to_canonical_string(), "cmd+4");
    }

    #[test]
    fn key_chord_to_canonical_multiple_modifiers_sorted() {
        let chord = KeyChord {
            modifiers: {
                let mut set = BTreeSet::new();
                set.insert(Modifier::Shift);
                set.insert(Modifier::Cmd);
                set.insert(Modifier::Ctrl);
                set
            },
            key: "t".to_string(),
        };
        // BTreeSet is sorted, so order should be ctrl, shift, cmd (enum order)
        assert_eq!(chord.to_canonical_string(), "ctrl+shift+cmd+t");
    }

    #[test]
    fn key_chord_roundtrip_single_modifier() {
        let original = "cmd+a";
        let chord = KeyChord::parse(original).unwrap();
        let canonical = chord.to_canonical_string();
        assert_eq!(canonical, original);
    }

    #[test]
    fn key_chord_roundtrip_multiple_modifiers() {
        let chord = KeyChord::parse("cmd+shift+f5").unwrap();
        let canonical = chord.to_canonical_string();
        let reparsed = KeyChord::parse(&canonical).unwrap();
        assert_eq!(chord, reparsed);
    }

    #[test]
    fn key_chord_parse_numeric_key() {
        let chord = KeyChord::parse("alt+1").unwrap();
        assert_eq!(chord.key, "1");
    }

    #[test]
    fn key_chord_parse_special_chars_in_key() {
        let chord = KeyChord::parse("cmd+.").unwrap();
        assert_eq!(chord.key, ".");
    }

    #[test]
    fn key_chord_parse_plus_in_key_position() {
        // A key that is "+" by itself should fail because it splits on +
        // This tests the edge case where key is empty
        assert!(KeyChord::parse("cmd+").is_err());
    }

    #[test]
    fn key_chord_parse_triple_plus() {
        assert!(KeyChord::parse("cmd++a").is_err());
    }
}
