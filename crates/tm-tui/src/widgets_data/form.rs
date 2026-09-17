//! A vertical form of labelled fields with tab-order navigation between them — the shape a
//! ticket-creation or settings screen needs, distinct from the read-only/navigation-only widgets
//! elsewhere in this module.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, Propagation};

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
            kind: FieldKind::Text { value: String::new(), cursor: 0 },
        }
    }

    /// A read-only field.
    pub fn read_only(label: impl Into<String>, value: impl Into<String>) -> Self {
        Field { label: label.into(), kind: FieldKind::ReadOnly(value.into()) }
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
    ///
    /// IMPL: `active` should be the index of the first `FieldKind::Text` field, or `0` if there
    /// are none (an all-read-only form has nothing to make "active" in a meaningful sense).
    pub fn new(id: ComponentId, fields: Vec<Field>) -> Self {
        Form { id, fields, active: 0 }
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
}

impl Component for Form {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("draw each field's label/value and the active field's cursor per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let _: &InputEvent = match event {
            Event::Input(input) => input,
            _ => return Propagation::Propagate,
        };
        todo!("move `self.active` or edit the active field's text per the IMPL note above")
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        let _ = ctx;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_form_starts_with_no_field_values_set() {
        let form = Form::new(
            ComponentId::new("test.form"),
            vec![Field::text("Title"), Field::read_only("Id", "T-1")],
        );
        assert_eq!(form.values(), vec![""]);
    }
}
