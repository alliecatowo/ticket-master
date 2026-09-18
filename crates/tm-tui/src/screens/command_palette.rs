//! The command palette: a fuzzy-searchable overlay of actions and navigation targets, summoned
//! over whatever screen is currently showing.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::symbols::line;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, Propagation};
use crate::text::graphemes;
use crate::widgets_data::form::{Field, FieldKind};
use crate::widgets_data::list::List;

/// One entry the palette can run or navigate to.
#[derive(Debug, Clone)]
pub struct Action {
    /// The text matched against the query and shown in the results list.
    pub label: &'static str,
    /// A stable identifier the caller matches on when the action is chosen, distinct from
    /// [`ComponentId`] (which names tree nodes, not application commands).
    pub id: &'static str,
}

impl Action {
    /// An action with the given label and id.
    pub const fn new(label: &'static str, id: &'static str) -> Self {
        Action { label, id }
    }
}

/// The command palette overlay: a single-line query [`Field`] above a [`List`] of fuzzy-matched
/// [`Action`]s.
///
/// IMPL:
/// - `render`: draw as a centered floating box over `area` (compute a smaller centered `Rect`,
///   e.g. 60% width / 40% height, rather than filling the whole screen — this is an overlay, not
///   a screen that replaces the one beneath it) with a border styled `ctx.theme.accent`. The
///   query field renders in the top row, `self.results` fills the remainder.
/// - `filter`: on every query edit, recompute `self.results`' items from `self.actions` filtered
///   by a fuzzy/substring match against `self.query`'s current text (see `Field`'s `FieldKind::
///   Text::value`) and call `self.results.set_items(..)` with the matching labels. Keep the
///   matching algorithm here rather than in `List`, which stays a generic, non-fuzzy widget.
/// - `handle_event`: forward text input to `self.query`, re-deriving `self.results` via `filter`
///   after every edit; forward Up/Down to `self.results`. Enter resolves the currently selected
///   result back to its `Action`; how that resolved action reaches whatever opened the palette
///   (a return value from `handle_event`, an `AppMessage` variant, a callback) is a design
///   decision for the implementing agent, since `Component::handle_event` today only returns
///   [`Propagation`] — extending that return type is a contract change on `component.rs` and must
///   be coordinated rather than done unilaterally here. Escape should close the palette; how
///   "closed" is signaled to the caller has the same open question as Enter.
#[derive(Debug)]
pub struct CommandPaletteScreen {
    id: ComponentId,
    query: Field,
    results: List,
    actions: Vec<Action>,
    /// The id of the action last resolved by Enter, taken (and cleared) by
    /// [`CommandPaletteScreen::take_resolved`]. `Component::handle_event` only returns
    /// [`Propagation`], not an application-level result, so a resolved action is surfaced this
    /// way rather than by extending that frozen contract; see the `handle_event` IMPL note above.
    resolved: Option<&'static str>,
    /// Set by Escape, taken (and cleared) by [`CommandPaletteScreen::take_closed`]; same
    /// reasoning as `resolved`.
    closed: bool,
}

impl CommandPaletteScreen {
    /// A palette over `actions`, starting with an empty query.
    ///
    /// IMPL: this constructor deliberately does not populate `results` (that requires
    /// `List::set_items`, itself a stub in `widgets_data::list` — see that module) — call
    /// [`CommandPaletteScreen::filter`] once after construction, once `List::set_items` is
    /// implemented, to show every action before the user types anything.
    pub fn new(id: ComponentId, actions: Vec<Action>) -> Self {
        CommandPaletteScreen {
            id,
            query: Field::text("Command"),
            results: List::new(ComponentId::new("command_palette.results")),
            actions,
            resolved: None,
            closed: false,
        }
    }

    /// Recompute `self.results` from `self.query`'s current text against `self.actions`.
    pub fn filter(&mut self) {
        let query = query_text(&self.query).to_lowercase();
        let matches: Vec<String> = self
            .actions
            .iter()
            .filter(|action| query.is_empty() || action.label.to_lowercase().contains(&query))
            .map(|action| action.label.to_string())
            .collect();
        self.results.set_items(matches);
    }

    /// The action last resolved by Enter, if any, clearing it so it is only reported once.
    pub fn take_resolved(&mut self) -> Option<&'static str> {
        self.resolved.take()
    }

    /// Whether Escape closed the palette since the last check, clearing the flag so it is only
    /// reported once.
    pub fn take_closed(&mut self) -> bool {
        std::mem::take(&mut self.closed)
    }

    /// Insert `text` (typically one grapheme, from a printable key press) at the query's cursor.
    fn insert(&mut self, text: &str) {
        if let FieldKind::Text { value, cursor } = &mut self.query.kind {
            value.insert_str(*cursor, text);
            *cursor += text.len();
        }
        self.filter();
    }

    /// Remove the grapheme immediately before the cursor (Backspace).
    fn backspace(&mut self) {
        if let FieldKind::Text { value, cursor } = &mut self.query.kind {
            if let Some(prev) = graphemes(&value[..*cursor]).last() {
                let start = *cursor - prev.text.len();
                value.replace_range(start..*cursor, "");
                *cursor = start;
            }
        }
        self.filter();
    }

    /// Remove the grapheme at the cursor (Delete).
    fn delete_forward(&mut self) {
        if let FieldKind::Text { value, cursor } = &mut self.query.kind {
            if let Some(next) = graphemes(&value[*cursor..]).first() {
                let end = *cursor + next.text.len();
                value.replace_range(*cursor..end, "");
            }
        }
        self.filter();
    }

    /// Move the cursor back one grapheme.
    fn cursor_left(&mut self) {
        if let FieldKind::Text { value, cursor } = &mut self.query.kind {
            if let Some(prev) = graphemes(&value[..*cursor]).last() {
                *cursor -= prev.text.len();
            }
        }
    }

    /// Move the cursor forward one grapheme.
    fn cursor_right(&mut self) {
        if let FieldKind::Text { value, cursor } = &mut self.query.kind {
            if let Some(next) = graphemes(&value[*cursor..]).first() {
                *cursor += next.text.len();
            }
        }
    }
}

/// The query field's current text, whether it is (as constructed) a `Text` field or, in
/// principle, a caller-substituted `ReadOnly` one.
fn query_text(field: &Field) -> &str {
    match &field.kind {
        FieldKind::Text { value, .. } => value.as_str(),
        FieldKind::ReadOnly(value) => value.as_str(),
    }
}

/// A `Rect` centered within `area`, `percent_width`/`percent_height` of its size — how this
/// overlay distinguishes itself from a full-screen screen (see the `render` IMPL note above).
fn centered_rect(area: Rect, percent_width: u16, percent_height: u16) -> Rect {
    let width = area.width.saturating_mul(percent_width) / 100;
    let height = area.height.saturating_mul(percent_height) / 100;
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// Draw a one-cell-thick box border around `area` in `style`, falling back to plain ASCII when
/// `unicode` cannot be trusted.
fn draw_border(area: Rect, buf: &mut Buffer, style: ratatui_core::style::Style, ascii: bool) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let (tl, tr, bl, br, h, v) = if ascii {
        ("+", "+", "+", "+", "-", "|")
    } else {
        (
            line::TOP_LEFT,
            line::TOP_RIGHT,
            line::BOTTOM_LEFT,
            line::BOTTOM_RIGHT,
            line::HORIZONTAL,
            line::VERTICAL,
        )
    };
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    buf.set_string(area.x, area.y, tl, style);
    buf.set_string(right, area.y, tr, style);
    buf.set_string(area.x, bottom, bl, style);
    buf.set_string(right, bottom, br, style);
    for x in (area.x + 1)..right {
        buf.set_string(x, area.y, h, style);
        buf.set_string(x, bottom, h, style);
    }
    for y in (area.y + 1)..bottom {
        buf.set_string(area.x, y, v, style);
        buf.set_string(right, y, v, style);
    }
}

impl Component for CommandPaletteScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let outer = centered_rect(area, 60, 40);
        if outer.width < 2 || outer.height < 2 {
            return;
        }
        let ascii = matches!(ctx.caps.unicode, crate::caps::UnicodeSupport::AsciiOnly);
        draw_border(outer, buf, ctx.theme.accent.into(), ascii);

        let inner = Rect {
            x: outer.x + 1,
            y: outer.y + 1,
            width: outer.width.saturating_sub(2),
            height: outer.height.saturating_sub(2),
        };
        if inner.height == 0 {
            return;
        }

        let query_row = Rect { height: 1, ..inner };
        let prompt = "> ";
        buf.set_stringn(
            query_row.x,
            query_row.y,
            prompt,
            query_row.width as usize,
            ctx.theme.muted,
        );
        let text_x = query_row.x + prompt.len() as u16;
        let text_width = query_row.width.saturating_sub(prompt.len() as u16);
        let value = query_text(&self.query);
        buf.set_stringn(
            text_x,
            query_row.y,
            value,
            text_width as usize,
            ctx.theme.foreground,
        );

        if let FieldKind::Text { cursor, .. } = &self.query.kind {
            let column: usize = graphemes(&value[..*cursor]).iter().map(|g| g.width).sum();
            let cursor_x = text_x + column as u16;
            if cursor_x < query_row.x + query_row.width {
                buf.set_style(
                    Rect::new(cursor_x, query_row.y, 1, 1),
                    ctx.theme.focus_border,
                );
            }
        }

        let results_area = Rect {
            y: inner.y + 1,
            height: inner.height.saturating_sub(1),
            ..inner
        };
        self.results.render(results_area, buf, ctx);
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let key = match event {
            Event::Input(InputEvent::Key(key)) => key,
            _ => return self.results.handle_event(event, ctx),
        };
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char(c) => {
                let mut buf4 = [0u8; 4];
                self.insert(c.encode_utf8(&mut buf4));
                Propagation::Consumed
            }
            KeyCode::Backspace => {
                self.backspace();
                Propagation::Consumed
            }
            KeyCode::Delete => {
                self.delete_forward();
                Propagation::Consumed
            }
            KeyCode::Left => {
                self.cursor_left();
                Propagation::Consumed
            }
            KeyCode::Right => {
                self.cursor_right();
                Propagation::Consumed
            }
            KeyCode::Up | KeyCode::Down => self.results.handle_event(event, ctx),
            KeyCode::Enter => {
                if let Some(label) = self.results.selected() {
                    self.resolved = self
                        .actions
                        .iter()
                        .find(|action| action.label == label)
                        .map(|action| action.id);
                }
                Propagation::Consumed
            }
            KeyCode::Esc => {
                self.closed = true;
                Propagation::Consumed
            }
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        self.results.keybindings(ctx)
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        vec![self.results.id()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_remembers_its_actions() {
        let screen = CommandPaletteScreen::new(
            ComponentId::new("command_palette"),
            vec![
                Action::new("Open ticket", "open_ticket"),
                Action::new("Quit", "quit"),
            ],
        );
        assert_eq!(screen.actions.len(), 2);
        assert_eq!(screen.actions[0].id, "open_ticket");
    }

    #[test]
    fn palette_reports_results_as_its_only_focusable_child() {
        let screen = CommandPaletteScreen::new(ComponentId::new("command_palette"), Vec::new());
        assert_eq!(
            screen.focusable_children(),
            vec![ComponentId::new("command_palette.results")]
        );
    }
}
