//! The diff viewer screen: a scrollable list of changed paths beside the unified diff for
//! whichever one is selected.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, Propagation};
use crate::theme::split_horizontal;
use crate::widgets_data::list::List;
use crate::widgets_viz::diff::Diff;

/// Which of the two panes currently has internal focus; see `Dashboard`'s identical field for why
/// this is screen-local rather than routed through `component::FocusTree`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Paths,
    Diff,
}

/// The diff viewer: a [`List`] of changed paths on the left, the selected path's [`Diff`] on the
/// right.
///
/// IMPL:
/// - `render`: `crate::theme::split_horizontal(area, &[("paths", 1), ("diff", 3)])`, render
///   `self.paths` into `"paths"` and `self.diff` into `"diff"`. Draw a heading above `"diff"`
///   showing `self.diff.path()`, styled with `ctx.theme.accent`.
/// - `handle_event`: Tab switches `self.active` between `Pane::Paths`/`Pane::Diff`, gated on
///   `ctx.focus.is_focused(self.id())` (mirrors `Dashboard::handle_event`). Otherwise forwards to
///   whichever child `self.active` names. When `self.paths.handle_event` consumes a selection
///   change (Enter, or the widget's own selection-moved signal — whichever shape `List` settles
///   on), the screen is responsible for calling `self.diff.set_diff(..)` with the newly selected
///   path's lines; that data comes from whatever wires this screen up to `tm-core`/`tm-harness`'s
///   diff computation (out of scope for this crate, per its dependency list), so this stub leaves
///   the actual fetch as a caller-supplied concern rather than guessing at one.
#[derive(Debug)]
pub struct DiffViewerScreen {
    id: ComponentId,
    paths: List,
    diff: Diff,
    active: Pane,
}

impl DiffViewerScreen {
    /// A diff viewer over the given path list and diff view, starting with the path list active.
    pub fn new(id: ComponentId, paths: List, diff: Diff) -> Self {
        DiffViewerScreen {
            id,
            paths,
            diff,
            active: Pane::Paths,
        }
    }
}

impl Component for DiffViewerScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let slots = split_horizontal(area, &[("paths", 1), ("diff", 3)]);

        self.paths.render(slots.get("paths"), buf, ctx);

        let diff_area = slots.get("diff");
        if diff_area.height == 0 {
            return;
        }
        let heading_row = Rect {
            height: 1,
            ..diff_area
        };
        buf.set_stringn(
            heading_row.x,
            heading_row.y,
            self.diff.path(),
            heading_row.width as usize,
            ctx.theme.accent,
        );

        let diff_body = Rect {
            y: diff_area.y + 1,
            height: diff_area.height.saturating_sub(1),
            ..diff_area
        };
        self.diff.render(diff_body, buf, ctx);
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if ctx.focus.is_focused(self.id) {
            if let Event::Input(InputEvent::Key(key)) = event {
                if key.code == crossterm::event::KeyCode::Tab {
                    self.active = match self.active {
                        Pane::Paths => Pane::Diff,
                        Pane::Diff => Pane::Paths,
                    };
                    return Propagation::Consumed;
                }
            }
        }

        match self.active {
            Pane::Paths => self.paths.handle_event(event, ctx),
            Pane::Diff => self.diff.handle_event(event, ctx),
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        match self.active {
            Pane::Paths => self.paths.keybindings(ctx),
            Pane::Diff => self.diff.keybindings(ctx),
        }
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        vec![self.paths.id(), self.diff.id()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_viewer_reports_both_panes_as_focusable() {
        let screen = DiffViewerScreen::new(
            ComponentId::new("diff_viewer"),
            List::new(ComponentId::new("diff_viewer.paths")),
            Diff::new(ComponentId::new("diff_viewer.diff"), "a.rs"),
        );
        assert_eq!(
            screen.focusable_children(),
            vec![
                ComponentId::new("diff_viewer.paths"),
                ComponentId::new("diff_viewer.diff")
            ]
        );
    }
}
