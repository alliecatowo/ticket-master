//! A vertical form of labelled fields with tab-order navigation between them — the shape a
//! ticket-creation or settings screen needs, distinct from the read-only/navigation-only widgets
//! elsewhere in this module.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Modifier;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::{graphemes, truncate};
use crate::theme::split_horizontal;

/// One field's kind, which determines how it renders and what input it accepts.
///
/// IMPL: start with these two; extend with `Select { options: Vec<String>, chosen: usize }` and
/// `Checkbox { checked: bool }` once a screen actually needs them rather than speculatively.
#[derive(Debug, Clone)]
pub enum FieldKind {
    /// Free-text single-line input.
    Text {
        /// The current text content.
        value: String,
        /// The cursor's byte offset into `value`.
        cursor: usize,
    },
    /// Read-only display text (a computed/derived field shown inline with editable ones).
    ReadOnly(String),
}

/// One labelled field within a [`Form`].
#[derive(Debug, Clone)]
pub struct Field {
    /// The label shown to the left of (or above) the field.
    pub label: String,
    /// The field's kind and current value.
    pub kind: FieldKind,
}

impl Field {
    /// A text field starting empty.
    pub fn text(label: impl Into<String>) -> Self {
        Field {
            label: label.into(),
            kind: FieldKind::Text {
                value: String::new(),
                cursor: 0,
            },
        }
    }

    /// A read-only field.
    pub fn read_only(label: impl Into<String>, value: impl Into<String>) -> Self {
        Field {
            label: label.into(),
            kind: FieldKind::ReadOnly(value.into()),
        }
    }
}

/// A vertical list of [`Field`]s with one of them (`self.active`) accepting keyboard input at a
/// time, Tab/Shift+Tab (or Down/Up when no field is mid-edit) moving between them.
///
/// IMPL:
/// - `render`: one row per field (label, then value — use `crate::theme::split_horizontal` for
///   the label/value column split), styling the active field's row with `ctx.theme.focus_border`
///   only when `ctx.focus.is_focused(self.id())` (an unfocused form should show which field was
///   last active without the "you are typing here" emphasis). For a `Text` field, render a cursor
///   indicator at `cursor`'s grapheme position (via `crate::text::graphemes`, not a byte offset
///   directly — a byte offset can land inside a multi-byte character).
/// - `handle_event`: gate on focus like the other widgets in this module. Tab/Down moves
///   `self.active` forward (clamped/wrapped — pick one and document it, do not leave it
///   ambiguous); Shift+Tab/Up moves backward. When the active field is `Text`, printable-character
///   keys insert at `cursor` (advance `cursor` by the inserted grapheme's byte length, not always
///   `1`), Backspace removes the grapheme before `cursor`, Delete removes the one at `cursor`,
///   Left/Right move `cursor` by one grapheme (via `crate::text::graphemes`, never `cursor - 1`/
///   `cursor + 1` directly — that can split a multi-byte character). A `ReadOnly` field never
///   becomes `self.active`'s target for text input; skip it when computing Tab order.
#[derive(Debug, Clone)]
pub struct Form {
    id: ComponentId,
    fields: Vec<Field>,
    active: usize,
}

impl Form {
    /// A form over the given fields, starting with the first non-read-only field active.
    pub fn new(id: ComponentId, fields: Vec<Field>) -> Self {
        let active = fields
            .iter()
            .position(|f| matches!(f.kind, FieldKind::Text { .. }))
            .unwrap_or(0);
        Form { id, fields, active }
    }

    /// The current value of every `Text` field, in field order, for a screen to read out on
    /// submit. `ReadOnly` fields are skipped.
    pub fn values(&self) -> Vec<&str> {
        self.fields
            .iter()
            .filter_map(|f| match &f.kind {
                FieldKind::Text { value, .. } => Some(value.as_str()),
                FieldKind::ReadOnly(_) => None,
            })
            .collect()
    }

    /// The index of the `Text` field `delta` steps away from `self.active` in tab order, wrapping
    /// around and skipping `ReadOnly` fields. Returns `self.active` unchanged when there is no
    /// editable field to move to (an all-read-only form, or the empty form).
    fn step_active(&self, delta: isize) -> usize {
        let editable: Vec<usize> = self
            .fields
            .iter()
            .enumerate()
            .filter_map(|(i, f)| matches!(f.kind, FieldKind::Text { .. }).then_some(i))
            .collect();
        if editable.is_empty() {
            return self.active;
        }
        let current = editable.iter().position(|&i| i == self.active).unwrap_or(0);
        let len = editable.len() as isize;
        let next = (current as isize + delta).rem_euclid(len);
        editable[next as usize]
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Tab),
                "next field",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::BackTab),
                "previous field",
            ),
        ]
    }
}

/// The byte offset of the grapheme boundary immediately before `cursor` in `value` — used by
/// Backspace/Left so a multi-byte character is never split.
fn prev_boundary(value: &str, cursor: usize) -> usize {
    let mut boundary = 0;
    for g in graphemes(value) {
        let end = boundary + g.text.len();
        if end >= cursor {
            return boundary;
        }
        boundary = end;
    }
    boundary
}

/// The byte offset of the grapheme boundary immediately after `cursor` in `value` — used by
/// Delete/Right so a multi-byte character is never split.
fn next_boundary(value: &str, cursor: usize) -> usize {
    let mut byte = 0;
    for g in graphemes(value) {
        let end = byte + g.text.len();
        if byte >= cursor {
            return end;
        }
        byte = end;
    }
    value.len()
}

/// The terminal column `cursor` (a byte offset into `value`) lands at, measured in this field's
/// own display width from column 0 — i.e. the sum of the widths of every whole grapheme cluster
/// before `cursor`.
fn cursor_column(value: &str, cursor: usize) -> u16 {
    let mut column = 0usize;
    let mut byte = 0usize;
    for g in graphemes(value) {
        if byte >= cursor {
            break;
        }
        byte += g.text.len();
        column += g.width;
    }
    column as u16
}

/// This field's row style: full emphasis when it is both active and the form holds keyboard
/// focus, a dimmer bold-only indicator when active but unfocused (the form remembers where it
/// was without claiming to be receiving input), plain text otherwise.
fn row_style(ctx: &FrameContext<'_>, is_active: bool, focused: bool) -> ratatui_core::style::Style {
    if is_active && focused {
        ctx.theme.focus_border
    } else if is_active {
        ratatui_core::style::Style::default()
            .fg(ctx.theme.foreground)
            .add_modifier(Modifier::BOLD)
    } else {
        ratatui_core::style::Style::default().fg(ctx.theme.foreground)
    }
}

impl Component for Form {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = ctx.focus.is_focused(self.id);

        for (index, field) in self.fields.iter().enumerate() {
            if index as u16 >= area.height {
                break;
            }
            let row = Rect::new(area.x, area.y + index as u16, area.width, 1);
            let slots = split_horizontal(row, &[("label", 1), ("value", 2)]);
            let label_area = slots.get("label");
            let value_area = slots.get("value");

            let is_active = index == self.active;
            let style = row_style(ctx, is_active, focused);

            let label_text = truncate(&field.label, label_area.width as usize, "…");
            buf.set_stringn(
                label_area.x,
                label_area.y,
                &label_text,
                label_area.width as usize,
                style,
            );

            match &field.kind {
                FieldKind::Text { value, cursor } => {
                    let text = truncate(value, value_area.width as usize, "…");
                    buf.set_stringn(
                        value_area.x,
                        value_area.y,
                        &text,
                        value_area.width as usize,
                        style,
                    );
                    if is_active && focused && value_area.width > 0 {
                        let column =
                            cursor_column(value, *cursor).min(value_area.width.saturating_sub(1));
                        let cursor_x = value_area.x + column;
                        if let Some(cell) = buf.cell_mut((cursor_x, value_area.y)) {
                            cell.set_style(style.add_modifier(Modifier::REVERSED));
                        }
                    }
                }
                FieldKind::ReadOnly(text) => {
                    let rendered = truncate(text, value_area.width as usize, "…");
                    let read_only_style = ratatui_core::style::Style::default().fg(ctx.theme.muted);
                    buf.set_stringn(
                        value_area.x,
                        value_area.y,
                        &rendered,
                        value_area.width as usize,
                        read_only_style,
                    );
                }
            }
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let input = match event {
            Event::Input(input) => input,
            _ => return Propagation::Propagate,
        };
        let InputEvent::Key(key) = input else {
            return Propagation::Propagate;
        };

        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.active = self.step_active(1);
                return Propagation::Consumed;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.active = self.step_active(-1);
                return Propagation::Consumed;
            }
            _ => {}
        }

        let Some(field) = self.fields.get_mut(self.active) else {
            return Propagation::Propagate;
        };
        let FieldKind::Text { value, cursor } = &mut field.kind else {
            return Propagation::Propagate;
        };

        match key.code {
            KeyCode::Char(c) => {
                value.insert(*cursor, c);
                *cursor += c.len_utf8();
                Propagation::Consumed
            }
            KeyCode::Backspace => {
                let start = prev_boundary(value, *cursor);
                value.drain(start..*cursor);
                *cursor = start;
                Propagation::Consumed
            }
            KeyCode::Delete => {
                let end = next_boundary(value, *cursor);
                value.drain(*cursor..end);
                Propagation::Consumed
            }
            KeyCode::Left => {
                *cursor = prev_boundary(value, *cursor);
                Propagation::Consumed
            }
            KeyCode::Right => {
                *cursor = next_boundary(value, *cursor);
                Propagation::Consumed
            }
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            Form::bindings()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::component::FocusState;
    use crate::theme::Theme;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tm_types::FixedClock;

    #[test]
    fn new_form_starts_with_no_field_values_set() {
        let form = Form::new(
            ComponentId::new("test.form"),
            vec![Field::text("Title"), Field::read_only("Id", "T-1")],
        );
        assert_eq!(form.values(), vec![""]);
    }

    #[test]
    fn new_form_skips_leading_read_only_fields_for_active() {
        let form = Form::new(
            ComponentId::new("test.form"),
            vec![Field::read_only("Id", "T-1"), Field::text("Title")],
        );
        assert_eq!(form.active, 1);
    }

    #[test]
    fn all_read_only_form_defaults_active_to_zero() {
        let form = Form::new(
            ComponentId::new("test.form"),
            vec![Field::read_only("Id", "T-1")],
        );
        assert_eq!(form.active, 0);
    }

    fn ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
    ) -> FrameContext<'a> {
        FrameContext {
            theme,
            caps,
            clock,
            focus: FocusState::new(Some(ComponentId::new("test.form"))),
        }
    }

    fn key(event: Event, form: &mut Form, context: &FrameContext<'_>) -> Propagation {
        form.handle_event(&event, context)
    }

    fn char_event(c: char) -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
    }

    #[test]
    fn typing_inserts_at_cursor_and_advances_it() {
        let mut form = Form::new(ComponentId::new("test.form"), vec![Field::text("Title")]);
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock);

        assert_eq!(
            key(char_event('a'), &mut form, &context),
            Propagation::Consumed
        );
        assert_eq!(
            key(char_event('b'), &mut form, &context),
            Propagation::Consumed
        );
        assert_eq!(form.values(), vec!["ab"]);
    }

    #[test]
    fn backspace_removes_the_grapheme_before_the_cursor() {
        let mut form = Form::new(ComponentId::new("test.form"), vec![Field::text("Title")]);
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock);

        key(char_event('a'), &mut form, &context);
        key(char_event('b'), &mut form, &context);
        let backspace = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        assert_eq!(key(backspace, &mut form, &context), Propagation::Consumed);
        assert_eq!(form.values(), vec!["a"]);
    }

    #[test]
    fn tab_skips_read_only_fields_and_wraps() {
        let mut form = Form::new(
            ComponentId::new("test.form"),
            vec![
                Field::text("First"),
                Field::read_only("Id", "T-1"),
                Field::text("Second"),
            ],
        );
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock);

        let tab = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )));
        assert_eq!(form.active, 0);
        key(tab.clone(), &mut form, &context);
        assert_eq!(form.active, 2, "tab must skip the read-only field");
        key(tab, &mut form, &context);
        assert_eq!(form.active, 0, "tab order must wrap");
    }

    #[test]
    fn unfocused_form_ignores_input() {
        let mut form = Form::new(ComponentId::new("test.form"), vec![Field::text("Title")]);
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = FrameContext {
            theme: &theme,
            caps: &caps,
            clock: &clock,
            focus: FocusState::default(),
        };
        assert_eq!(
            key(char_event('a'), &mut form, &context),
            Propagation::Propagate
        );
        assert_eq!(form.values(), vec![""]);
    }

    #[test]
    fn cursor_column_counts_display_width_not_bytes() {
        assert_eq!(cursor_column("中文", 0), 0);
        assert_eq!(cursor_column("中文", "中".len()), 2);
        assert_eq!(cursor_column("中文", "中文".len()), 4);
    }

    #[test]
    fn render_does_not_panic_on_zero_sized_area() {
        let form = Form::new(ComponentId::new("test.form"), vec![Field::text("Title")]);
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock);
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        form.render(Rect::new(0, 0, 0, 0), &mut buf, &context);
    }
}
