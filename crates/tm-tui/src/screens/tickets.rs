//! The tickets screen: Claude Code's agent view (`claude agents`) with Ticketmaster tickets as its
//! rows (`docs/decisions/D-019-claude-code-parity-shell.md` §2).
//!
//! It reads the way the agent view reads: a header (version, model, cwd, and counts), tickets
//! grouped by state (**Needs input**, **Working**, **Ready for review**, **Queued**,
//! **Completed**), one row per ticket (a status glyph, id and short title, a one-line summary, and
//! an age aligned right), a dispatch input between two rules at the bottom, and a footer of key
//! hints. Space opens a peek panel; Enter or `→` attaches; Ctrl+X cancels (twice); an open peek lists the
//! ticket's options numbered (accept or reject a submission, retry an escalation, queue a draft)
//! and `1`, `2`, ... pick one; Ctrl+B opens the board; `?` lists every
//! shortcut. Like Claude Code's agent view, no plain letter is a shortcut: typed text always goes
//! to the dispatch input.
//!
//! The header also carries the hub's tab strip (Tickets, Board, Milestones, Timeline, Graph):
//! Tab/Shift+Tab cycle [`TicketsAction::NextTab`]/[`TicketsAction::PrevTab`], which `tm-cli`
//! executes by moving its own `ScreenId`. This screen only ever draws itself while that
//! `ScreenId` is `Tickets`, so its own strip always shows "Tickets" as the active tab.
//!
//! Like every screen here it is plain data in: `tm-cli` builds [`TicketsData`] from the project's
//! real state and executes the [`TicketsAction`]s this screen records. This module decides only
//! how it looks and which keys do what.
//!
//! # Keys never steal typing
//!
//! The dispatch input always has the keyboard. Arrow keys, Enter, Esc, Space, and modifier chords
//! work anywhere, but a letter is only a shortcut when the input is empty (`?`, `b`), and `a`/`r`
//! only when, in addition, the selected row is ready for review. Anything else is text.

use std::cell::Cell;
use std::collections::{BTreeSet, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Modifier, Style};
use tm_types::Timestamp;

use crate::chat::glyphs::Glyphs;
use crate::chat::input::InputBox;
use crate::chat::lines::{clear, draw_box, draw_line, draw_spans, truncate_spans, Line, Span};
use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::{display_width, truncate, wrap};
use crate::theme::Theme;

/// A state group, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    /// Escalated, or a worker waiting on an approval.
    NeedsInput,
    /// A worker holds it.
    Working,
    /// Submitted, waiting for a human to accept or reject.
    Review,
    /// Waiting for a worker.
    Queued,
    /// Closed or cancelled.
    Completed,
}

impl Group {
    /// Every group, in display order.
    pub const ALL: [Group; 5] = [
        Group::NeedsInput,
        Group::Working,
        Group::Review,
        Group::Queued,
        Group::Completed,
    ];

    /// The heading.
    pub fn title(self) -> &'static str {
        match self {
            Group::NeedsInput => "Needs input",
            Group::Working => "Working",
            Group::Review => "Ready for review",
            Group::Queued => "Queued",
            Group::Completed => "Completed",
        }
    }

    /// The count phrase the header uses (`2 working`).
    fn count_word(self) -> &'static str {
        match self {
            Group::NeedsInput => "needs input",
            Group::Working => "working",
            Group::Review => "ready for review",
            Group::Queued => "queued",
            Group::Completed => "completed",
        }
    }

    /// What the group holds, shown under its empty heading before the first dispatch.
    fn description(self) -> &'static str {
        match self {
            Group::NeedsInput => {
                "Tickets waiting on you: an escalation, or an approval only you can give."
            }
            Group::Working => "Tickets a background worker is running right now.",
            Group::Review => {
                "Submitted work for you to review: space, then 1 to accept or 2 to reject."
            }
            Group::Queued => {
                "Tickets waiting for a worker: ready, blocked, or retrying after a failure."
            }
            Group::Completed => "Closed tickets, and cancelled ones as stopped.",
        }
    }
}

/// Whether a worker holds the ticket, which decides the glyph's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Worker {
    /// No live worker (`∙`).
    None,
    /// A worker holds it but is not working it right now, e.g. it is waiting for an approval (`✻`).
    Attached,
    /// A worker is actively working it (an animated `✽`).
    Working,
}

/// How a row's glyph and summary are tinted beyond their group's colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    /// The group's usual colour.
    Normal,
    /// A failure: red.
    Failure,
    /// Stopped (cancelled): grey.
    Stopped,
    /// Finished well: green.
    Success,
}

/// One labelled fact in the peek panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeekItem {
    /// The label column (`state`, `attempt 2`).
    pub label: String,
    /// The text (wrapped to the panel).
    pub text: String,
    /// Its colour.
    pub tint: Tint,
}

impl PeekItem {
    /// A plain item.
    pub fn new(label: impl Into<String>, text: impl Into<String>) -> Self {
        PeekItem {
            label: label.into(),
            text: text.into(),
            tint: Tint::Normal,
        }
    }

    /// The same item, tinted.
    pub fn tinted(mut self, tint: Tint) -> Self {
        self.tint = tint;
        self
    }
}

/// One ticket row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketRow {
    /// The ticket id (`T-4`).
    pub id: String,
    /// A short title (see [`short_title`]).
    pub title: String,
    /// Its group.
    pub group: Group,
    /// Whether a worker holds it.
    pub worker: Worker,
    /// The one-line summary.
    pub summary: String,
    /// How the glyph and summary are tinted.
    pub tint: Tint,
    /// The age, already compact (`12m`; see [`compact_age`]).
    pub age: String,
    /// Sort key within the group: higher sorts first.
    pub order: i64,
    /// What the peek panel shows, top to bottom.
    pub peek: Vec<PeekItem>,
    /// What the human can do about it from the peek, numbered `1`, `2`, ... in this order.
    pub choices: Vec<Choice>,
}

/// Something the human can do about a ticket from its peek panel, numbered like a Claude Code
/// permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Accept a submission.
    Accept,
    /// Reject a submission, with a reason.
    Reject,
    /// Send an escalated ticket back to work.
    Retry,
    /// The same, with guidance for the next attempt.
    RetryWithGuidance,
    /// Queue a draft for a background worker.
    Queue,
}

impl Choice {
    /// How the peek lists it.
    fn label(self) -> &'static str {
        match self {
            Choice::Accept => "Accept",
            Choice::Reject => "Reject, with a reason the next attempt sees",
            Choice::Retry => "Retry",
            Choice::RetryWithGuidance => "Retry, with guidance for the next attempt",
            Choice::Queue => "Queue it for a background worker",
        }
    }

    /// How the footer names it (`1 to accept`).
    fn verb(self) -> &'static str {
        match self {
            Choice::Accept => "accept",
            Choice::Reject => "reject",
            Choice::Retry => "retry",
            Choice::RetryWithGuidance => "retry with guidance",
            Choice::Queue => "queue it",
        }
    }

    /// Whether it needs words from the human first (typed into the input).
    fn asks(self) -> bool {
        matches!(self, Choice::Reject | Choice::RetryWithGuidance)
    }
}

/// Everything the tickets screen shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TicketsData {
    /// `tm`'s version (`0.1.0`).
    pub version: String,
    /// The model dispatched workers use.
    pub model: String,
    /// Where the project is (`~/src/app (main)`).
    pub place: String,
    /// Something the header must say, in the warning colour (e.g. the background worker could
    /// not start, so nothing will pick queued tickets up).
    pub notice: Option<String>,
    /// Every ticket to list, in any order.
    pub rows: Vec<TicketRow>,
}

/// What the human asked for. `tm-cli` executes these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketsAction {
    /// Go back to the conversation.
    BackToChat,
    /// Create a ticket for this task and queue it for a background worker.
    Dispatch(String),
    /// Attach the conversation to this ticket and go back to it.
    Attach(String),
    /// Cancel this ticket (already confirmed by a second Ctrl+X).
    Cancel(String),
    /// Accept this ticket's submission.
    Accept(String),
    /// Reject this ticket's submission.
    Reject {
        /// The ticket.
        id: String,
        /// Why; the next attempt's worker sees it.
        reason: String,
    },
    /// Send this escalated ticket back to work.
    Retry {
        /// The ticket.
        id: String,
        /// What the next attempt should do differently, if the human said.
        guidance: Option<String>,
    },
    /// Queue this draft for a background worker.
    Queue(String),
    /// Open the Kanban board.
    OpenBoard,
    /// Tab: move to the next tab in the hub's strip (Tickets, Board, Milestones, Timeline,
    /// Graph, wrapping).
    NextTab,
    /// Shift+Tab: move to the previous tab.
    PrevTab,
}

/// How a transient footer message reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashTone {
    /// Neutral.
    Info,
    /// It worked.
    Success,
    /// It did not.
    Error,
}

#[derive(Debug, Clone)]
struct Flash {
    text: String,
    tone: FlashTone,
    until: Timestamp,
}

/// How long Ctrl+X stays armed for its confirming second press.
pub const CANCEL_CONFIRM_MILLIS: i64 = 2_000;

/// The dispatch input's placeholder.
pub const PLACEHOLDER: &str = "Describe a task for a background worker and press Enter";

/// What the selection is on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sel {
    Header(Group),
    Row(String),
    More,
}

/// One line of the list.
#[derive(Debug, Clone, Copy)]
enum Item<'a> {
    Header(Group, usize),
    Description(Group),
    Row(&'a TicketRow),
    More(usize),
    Blank,
}

impl Item<'_> {
    fn sel(&self) -> Option<Sel> {
        match self {
            Item::Header(g, _) => Some(Sel::Header(*g)),
            Item::Row(r) => Some(Sel::Row(r.id.clone())),
            Item::More(_) => Some(Sel::More),
            Item::Description(_) | Item::Blank => None,
        }
    }
}

/// A row's column widths: the whole row, the name column, and the age column.
#[derive(Debug, Clone, Copy)]
struct Columns {
    width: usize,
    name_w: usize,
    age_w: usize,
}

/// The glyphs this screen adds to [`Glyphs`].
struct Marks {
    working: &'static [&'static str],
    attached: &'static str,
    idle: &'static str,
    prompt: &'static str,
    more: &'static str,
}

impl Marks {
    fn for_glyphs(glyphs: &Glyphs) -> Marks {
        if glyphs.unicode {
            Marks {
                // The agent view's `✽`, breathing: it grows and shrinks rather than spinning.
                working: &["✽", "✽", "✻", "✶", "✳", "✢", "✳", "✶", "✻"],
                attached: "✻",
                idle: "∙",
                prompt: "❯",
                more: "…",
            }
        } else {
            Marks {
                working: &["*", "+", "x", "+"],
                attached: "*",
                idle: ".",
                prompt: ">",
                more: "...",
            }
        }
    }
}

/// A short title for a ticket: the first clause of its objective (up to the first sentence end,
/// `;`, `:`, `,`, dash, or bracket), trimmed to a few words. Width-safe for any text.
pub fn short_title(objective: &str) -> String {
    const MAX_WIDTH: usize = 32;
    let line = objective
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let mut end = line.len();
    for (i, c) in line.char_indices() {
        let clause_end = match c {
            ';' | ':' | ',' | '(' | '[' | '—' | '–' => true,
            // A sentence end, not a dot inside `foo.rs` or `v1.2`.
            '.' | '!' | '?' => line[i + c.len_utf8()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace),
            '-' => line[..i].ends_with(' ') && line[i + 1..].starts_with(' '),
            _ => false,
        };
        if clause_end && !line[..i].trim().is_empty() {
            end = i;
            break;
        }
    }
    let clause = line[..end].trim();
    if display_width(clause) <= MAX_WIDTH {
        return clause.to_string();
    }
    // Too long: whole words up to the budget, then an ellipsis.
    let mut out = String::new();
    for word in clause.split_whitespace() {
        let next = if out.is_empty() {
            word.to_string()
        } else {
            format!("{out} {word}")
        };
        if display_width(&next) > MAX_WIDTH - 1 {
            break;
        }
        out = next;
    }
    if out.is_empty() {
        out = truncate(clause, MAX_WIDTH - 1, "");
    }
    format!("{out}…")
}

/// `42s`, `12m`, `3h`, `2d`: the agent view's age column.
pub fn compact_age(millis: i64) -> String {
    let secs = millis.max(0) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The tickets screen.
#[derive(Debug)]
pub struct TicketsScreen {
    id: ComponentId,
    data: TicketsData,
    selected: Option<Sel>,
    /// Where the selection last sat in the list, to land nearby if its row disappears.
    selected_index: usize,
    collapsed: BTreeSet<Group>,
    show_all_completed: bool,
    peek_open: bool,
    help_open: bool,
    input: InputBox,
    /// The ticket and choice whose words (a reject reason, retry guidance) are being typed.
    asking: Option<(String, Choice)>,
    cancel_armed: Option<(String, Timestamp)>,
    flash: Option<Flash>,
    actions: VecDeque<TicketsAction>,
    /// Rows the list had at the last render, so key handling folds the list the way it was drawn.
    list_height: Cell<usize>,
    /// First list line drawn at the last render.
    scroll: Cell<usize>,
}

impl TicketsScreen {
    /// A tickets screen over `data`, with the first ticket selected.
    pub fn new(id: ComponentId, data: TicketsData) -> Self {
        let mut screen = TicketsScreen {
            id,
            data,
            selected: None,
            selected_index: 0,
            collapsed: BTreeSet::new(),
            show_all_completed: false,
            peek_open: false,
            help_open: false,
            input: InputBox::new(),
            asking: None,
            cancel_armed: None,
            flash: None,
            actions: VecDeque::new(),
            list_height: Cell::new(usize::MAX / 4),
            scroll: Cell::new(0),
        };
        screen.select_first();
        screen
    }

    /// The data on screen.
    pub fn data(&self) -> &TicketsData {
        &self.data
    }

    /// Replace the data (fresh project state). The selection follows its ticket, even into
    /// another group; if the ticket is gone it lands on whatever now sits where it was.
    pub fn set_data(&mut self, data: TicketsData) {
        self.data = data;
        self.reconcile_selection();
    }

    /// Select `ticket`'s row, if it is listed (expanding its group if it was collapsed).
    pub fn select_ticket(&mut self, ticket: &str) -> bool {
        let Some(group) = self
            .data
            .rows
            .iter()
            .find(|r| r.id == ticket)
            .map(|r| r.group)
        else {
            return false;
        };
        self.collapsed.remove(&group);
        self.selected = Some(Sel::Row(ticket.to_string()));
        self.reconcile_selection();
        true
    }

    /// The selected ticket's id, if a ticket row is selected.
    pub fn selected_ticket(&self) -> Option<&str> {
        match &self.selected {
            Some(Sel::Row(id)) => Some(id),
            _ => None,
        }
    }

    /// The dispatch input's text.
    pub fn input_text(&self) -> &str {
        self.input.text()
    }

    /// Whether the peek panel is open.
    pub fn is_peek_open(&self) -> bool {
        self.peek_open
    }

    /// Clear the dispatch input (Ctrl+C's first press). Returns whether there was anything to
    /// clear, so a second press on an empty input can mean "exit" instead.
    pub fn clear_input(&mut self) -> bool {
        let had = !self.input.is_empty() || self.asking.is_some();
        self.input.clear();
        self.asking = None;
        had
    }

    /// Show `text` in the footer until `until`.
    pub fn flash(&mut self, text: impl Into<String>, tone: FlashTone, until: Timestamp) {
        self.flash = Some(Flash {
            text: text.into(),
            tone,
            until,
        });
    }

    /// Drain every action recorded since the last call.
    pub fn take_actions(&mut self) -> Vec<TicketsAction> {
        self.actions.drain(..).collect()
    }

    // ----- the list -----

    /// Rows of `group`, sorted.
    fn rows_of(&self, group: Group) -> Vec<&TicketRow> {
        let mut rows: Vec<&TicketRow> =
            self.data.rows.iter().filter(|r| r.group == group).collect();
        rows.sort_by(|a, b| b.order.cmp(&a.order).then_with(|| a.id.cmp(&b.id)));
        rows
    }

    /// The list as drawn in `height` lines: non-empty groups in order, collapsed ones as a
    /// heading only, and Completed folded into `… N more` when it does not fit (never hiding the
    /// selected row). With no tickets at all, every heading with its description.
    fn items(&self, height: usize) -> Vec<Item<'_>> {
        let mut items = Vec::new();
        if self.data.rows.is_empty() {
            for group in Group::ALL {
                if !items.is_empty() {
                    items.push(Item::Blank);
                }
                items.push(Item::Header(group, 0));
                items.push(Item::Description(group));
            }
            return items;
        }
        let groups: Vec<(Group, Vec<&TicketRow>)> = Group::ALL
            .iter()
            .map(|g| (*g, self.rows_of(*g)))
            .filter(|(_, rows)| !rows.is_empty())
            .collect();
        for (group, rows) in &groups {
            if !items.is_empty() {
                items.push(Item::Blank);
            }
            items.push(Item::Header(*group, rows.len()));
            if self.collapsed.contains(group) {
                continue;
            }
            if *group != Group::Completed || self.show_all_completed {
                items.extend(rows.iter().map(|r| Item::Row(r)));
                continue;
            }
            // Completed fills whatever the live groups leave.
            let room = height.saturating_sub(items.len());
            if rows.len() <= room {
                items.extend(rows.iter().map(|r| Item::Row(r)));
                continue;
            }
            let selected_here =
                matches!(&self.selected, Some(Sel::Row(id)) if rows.iter().any(|r| &r.id == id));
            if room == 0 && !selected_here {
                // Not even a line for `… N more`: the heading says it instead.
                items.pop();
                items.push(Item::More(rows.len()));
                continue;
            }
            let shown = room.saturating_sub(1);
            let mut keep: Vec<&TicketRow> = rows.iter().take(shown).copied().collect();
            if let Some(Sel::Row(id)) = &self.selected {
                if let Some(sel) = rows.iter().skip(shown).find(|r| &r.id == id) {
                    if keep.pop().is_none() {
                        // No room even for one row: the selection still shows.
                    }
                    keep.push(sel);
                }
            }
            let hidden = rows.len() - keep.len();
            items.extend(keep.into_iter().map(Item::Row));
            if hidden > 0 {
                items.push(Item::More(hidden));
            }
        }
        items
    }

    fn selectable(&self) -> Vec<Sel> {
        self.items(self.list_height.get())
            .iter()
            .filter_map(Item::sel)
            .collect()
    }

    fn select_first(&mut self) {
        let items = self.selectable();
        self.selected = items
            .iter()
            .find(|s| matches!(s, Sel::Row(_)))
            .or_else(|| items.first())
            .cloned();
        self.selected_index = self
            .selected
            .as_ref()
            .and_then(|s| items.iter().position(|x| x == s))
            .unwrap_or(0);
    }

    /// Keep the selection on something that is listed: its ticket (wherever it moved), the
    /// heading of the group it collapsed into, or failing both the item at its old position.
    fn reconcile_selection(&mut self) {
        if let Some(Sel::Row(id)) = &self.selected {
            if let Some(group) = self.data.rows.iter().find(|r| &r.id == id).map(|r| r.group) {
                if self.collapsed.contains(&group) {
                    self.selected = Some(Sel::Header(group));
                }
            }
        }
        let items = self.selectable();
        if items.is_empty() {
            self.selected = None;
            self.selected_index = 0;
            return;
        }
        match self
            .selected
            .as_ref()
            .and_then(|s| items.iter().position(|x| x == s))
        {
            Some(i) => self.selected_index = i,
            None => {
                let i = self.selected_index.min(items.len() - 1);
                self.selected = items.get(i).cloned();
                self.selected_index = i;
            }
        }
        if self.selected_ticket().is_none() {
            self.peek_open = false;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let items = self.selectable();
        if items.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|s| items.iter().position(|x| x == s))
            .unwrap_or(0) as isize;
        let mut next = (current + delta).clamp(0, items.len() as isize - 1) as usize;
        if self.peek_open {
            // Peeking walks ticket to ticket, skipping headings, so the panel stays open.
            let step = if delta < 0 { -1 } else { 1 };
            let mut i = next as isize;
            while i >= 0 && (i as usize) < items.len() && !matches!(items[i as usize], Sel::Row(_))
            {
                i += step;
            }
            if i < 0 || i as usize >= items.len() {
                return;
            }
            next = i as usize;
        }
        self.selected = items.get(next).cloned();
        self.selected_index = next;
        if self.selected_ticket().is_none() {
            self.peek_open = false;
        }
    }

    fn selected_row(&self) -> Option<&TicketRow> {
        let id = self.selected_ticket()?;
        self.data.rows.iter().find(|r| r.id == id)
    }

    // ----- keys -----

    fn handle_key(&mut self, key: &KeyEvent, now: Timestamp) -> Propagation {
        if self.help_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.help_open = false;
                    return Propagation::Consumed;
                }
                _ => self.help_open = false,
            }
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let empty = self.input.is_empty() && self.asking.is_none();

        if ctrl {
            match key.code {
                // The application's own chords (Ctrl+C clear/exit, Ctrl+T back to chat).
                KeyCode::Char('c') | KeyCode::Char('t') => return Propagation::Propagate,
                KeyCode::Char('x') => self.cancel_pressed(now),
                KeyCode::Char('b') => self.actions.push_back(TicketsAction::OpenBoard),
                KeyCode::Char('j') => self.input.insert("\n"),
                KeyCode::Char('u') => self.input.clear(),
                KeyCode::Char('w') => self.input.delete_word_back(),
                KeyCode::Char('a') => self.input.home(),
                KeyCode::Char('e') => self.input.end(),
                KeyCode::Char('h') => self.input.backspace(),
                KeyCode::Enter => self.input.insert("\n"),
                KeyCode::Left => self.input.word_left(),
                KeyCode::Right => self.input.word_right(),
                _ => {}
            }
            return Propagation::Consumed;
        }

        match key.code {
            KeyCode::Enter if shift || alt => self.input.insert("\n"),
            KeyCode::Enter => self.enter(now),
            KeyCode::Esc => self.escape(),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-5),
            KeyCode::PageDown => self.move_selection(5),
            KeyCode::Right if empty => self.attach_selected(),
            KeyCode::Right if alt => self.input.word_right(),
            KeyCode::Right => self.input.right(),
            KeyCode::Left if alt => self.input.word_left(),
            KeyCode::Left => self.input.left(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Backspace if alt => self.input.delete_word_back(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Char(' ') if empty && self.selected_ticket().is_some() => {
                self.peek_open = !self.peek_open;
            }
            // Space on a heading does nothing rather than start a task with a blank.
            KeyCode::Char(' ') if empty => {}
            KeyCode::Char('?') if empty => self.help_open = true,
            // An open peek offers the ticket's choices numbered, like a permission prompt.
            KeyCode::Char(c @ '1'..='9') if empty && self.choice(c).is_some() => {
                if let (Some(choice), Some(id)) =
                    (self.choice(c), self.selected_ticket().map(str::to_string))
                {
                    self.choose(id, choice);
                }
            }
            KeyCode::Char(_) if alt => {}
            KeyCode::Char(c) => {
                self.cancel_armed = None;
                self.input.insert_char(c);
            }
            KeyCode::Tab => self.actions.push_back(TicketsAction::NextTab),
            KeyCode::BackTab => self.actions.push_back(TicketsAction::PrevTab),
            _ => {}
        }
        Propagation::Consumed
    }

    /// The choice digit `c` picks in the open peek, if any.
    fn choice(&self, c: char) -> Option<Choice> {
        if !self.peek_open {
            return None;
        }
        let n = c.to_digit(10)? as usize;
        self.selected_row()?.choices.get(n.checked_sub(1)?).copied()
    }

    fn choose(&mut self, id: String, choice: Choice) {
        if choice.asks() {
            self.asking = Some((id, choice));
            return;
        }
        self.actions.push_back(match choice {
            Choice::Accept => TicketsAction::Accept(id),
            Choice::Retry => TicketsAction::Retry { id, guidance: None },
            Choice::Queue => TicketsAction::Queue(id),
            Choice::Reject | Choice::RetryWithGuidance => return,
        });
    }

    fn enter(&mut self, now: Timestamp) {
        if let Some((id, choice)) = self.asking.clone() {
            let words = self.input.text().trim().to_string();
            if words.is_empty() {
                let ask = match choice {
                    Choice::Reject => format!("Type why you are rejecting {id}"),
                    _ => format!("Type what {id}'s next attempt should do differently"),
                };
                self.flash(
                    format!("{ask}; the next attempt sees it."),
                    FlashTone::Error,
                    now.plus_millis(3_000),
                );
                return;
            }
            self.input.clear();
            self.asking = None;
            self.actions.push_back(match choice {
                Choice::Reject => TicketsAction::Reject { id, reason: words },
                _ => TicketsAction::Retry {
                    id,
                    guidance: Some(words),
                },
            });
            return;
        }
        if !self.input.is_empty() {
            let task = self.input.text().trim().to_string();
            self.input.clear();
            if !task.is_empty() {
                self.actions.push_back(TicketsAction::Dispatch(task));
            }
            return;
        }
        match self.selected.clone() {
            Some(Sel::Row(_)) => self.attach_selected(),
            Some(Sel::Header(group)) => {
                if !self.collapsed.remove(&group) {
                    self.collapsed.insert(group);
                }
                self.reconcile_selection();
            }
            Some(Sel::More) => {
                self.show_all_completed = true;
                // Land on the first row that was hidden.
                self.reconcile_selection();
            }
            None => {}
        }
    }

    fn attach_selected(&mut self) {
        if let Some(id) = self.selected_ticket().map(str::to_string) {
            self.actions.push_back(TicketsAction::Attach(id));
        }
    }

    fn escape(&mut self) {
        if self.cancel_armed.take().is_some() {
            return;
        }
        if self.asking.take().is_some() {
            self.input.clear();
            return;
        }
        if self.peek_open {
            self.peek_open = false;
            return;
        }
        if !self.input.is_empty() {
            self.input.clear();
            return;
        }
        self.actions.push_back(TicketsAction::BackToChat);
    }

    fn cancel_pressed(&mut self, now: Timestamp) {
        let Some(row) = self.selected_row() else {
            self.flash(
                "Select a ticket to cancel it.",
                FlashTone::Info,
                now.plus_millis(2_000),
            );
            return;
        };
        let id = row.id.clone();
        if row.group == Group::Completed {
            self.flash(
                format!("{id} is already finished."),
                FlashTone::Info,
                now.plus_millis(2_000),
            );
            return;
        }
        match &self.cancel_armed {
            Some((armed, at)) if *armed == id && now.millis_since(*at) <= CANCEL_CONFIRM_MILLIS => {
                self.cancel_armed = None;
                self.actions.push_back(TicketsAction::Cancel(id));
            }
            _ => self.cancel_armed = Some((id, now)),
        }
    }

    // ----- drawing -----

    fn group_color(group: Group, tint: Tint, theme: &Theme) -> Color {
        match tint {
            Tint::Failure => return theme.danger,
            Tint::Stopped => return theme.muted,
            Tint::Success => return theme.success,
            Tint::Normal => {}
        }
        match group {
            Group::NeedsInput => theme.warning,
            Group::Working => theme.accent,
            Group::Review => theme.success,
            Group::Queued => theme.muted,
            Group::Completed => theme.success,
        }
    }

    fn glyph(row: &TicketRow, marks: &Marks, now: Timestamp) -> &'static str {
        match row.worker {
            Worker::Working => {
                let frame =
                    (now.unix_nanos() / 150_000_000).rem_euclid(marks.working.len() as i128);
                marks.working[frame as usize]
            }
            Worker::Attached => marks.attached,
            Worker::None => marks.idle,
        }
    }

    fn counts(&self, sep: &str) -> String {
        let parts: Vec<String> = Group::ALL
            .iter()
            .filter_map(|g| {
                let n = self.data.rows.iter().filter(|r| r.group == *g).count();
                (n > 0).then(|| format!("{n} {}", g.count_word()))
            })
            .collect();
        if parts.is_empty() {
            "no tickets yet".to_string()
        } else {
            parts.join(sep)
        }
    }

    /// The hub's tab strip (Tickets, Board, Milestones, Timeline, Graph). This screen only ever
    /// renders while it is itself the active tab, so "Tickets" is always the highlighted one.
    const TABS: [&'static str; 5] = ["Tickets", "Board", "Milestones", "Timeline", "Graph"];

    fn tab_strip_spans(theme: &Theme, sep: &str) -> Vec<Span> {
        let mut spans = Vec::new();
        for (i, tab) in Self::TABS.iter().enumerate() {
            if i > 0 {
                spans.push(Span::new(
                    format!(" {sep} "),
                    Style::default().fg(theme.muted),
                ));
            }
            let style = if i == 0 {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.muted)
            };
            spans.push(Span::new(*tab, style));
        }
        spans
    }

    /// The header: a ticket-stub mark beside three lines (name and version; model and place;
    /// counts), or one compact line on a short or narrow terminal. Returns the rows it used.
    fn render_header(
        &self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        glyphs: &Glyphs,
        compact: bool,
    ) -> u16 {
        let muted = Style::default().fg(theme.muted);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let sep = glyphs.sep;
        let width = area.width;
        if compact {
            let mut spans = vec![
                Span::new("tm", bold),
                Span::new(format!(" v{}", self.data.version), muted),
                Span::new(sep, muted),
                Span::new(self.counts(sep), Style::default()),
            ];
            if let Some(notice) = &self.data.notice {
                spans.push(Span::new(sep, muted));
                spans.push(Span::new(
                    notice.clone(),
                    Style::default().fg(theme.warning),
                ));
            }
            spans.extend([
                Span::new(sep, muted),
                Span::new(self.data.model.clone(), muted),
                Span::new(sep, muted),
                Span::new(self.data.place.clone(), muted),
            ]);
            draw_spans(
                buf,
                area.x,
                area.y,
                width,
                &truncate_spans(&spans, width as usize, glyphs.ellipsis),
            );
            let tabs = Self::tab_strip_spans(theme, sep);
            draw_spans(
                buf,
                area.x,
                area.y + 1,
                width,
                &truncate_spans(&tabs, width as usize, glyphs.ellipsis),
            );
            return 2;
        }
        let has_colour = theme.accent != Color::Reset;
        let logo = glyphs.unicode && has_colour && width >= 40;
        let text_x = if logo { area.x + 9 } else { area.x };
        let text_w = width.saturating_sub(text_x - area.x);
        if logo {
            let edge = Style::default().fg(theme.accent);
            let face = Style::default()
                .fg(theme.background)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD);
            buf.set_string(area.x, area.y, "▗▄▄▄▄▄▖", edge);
            buf.set_string(area.x, area.y + 1, "▐", edge);
            buf.set_string(area.x + 1, area.y + 1, " tm  ", face);
            buf.set_string(area.x + 6, area.y + 1, "▌", edge);
            buf.set_string(area.x, area.y + 2, "▝▀▀▀▀▀▘", edge);
        }
        let lines = [
            vec![
                Span::new("Ticketmaster", bold),
                Span::new(format!(" v{}", self.data.version), muted),
            ],
            vec![
                Span::new(self.data.model.clone(), muted),
                Span::new(sep, muted),
                Span::new(self.data.place.clone(), muted),
            ],
            {
                let mut counts = vec![Span::new(self.counts(sep), muted)];
                if let Some(notice) = &self.data.notice {
                    counts.push(Span::new(sep, muted));
                    counts.push(Span::new(
                        notice.clone(),
                        Style::default().fg(theme.warning),
                    ));
                }
                counts
            },
        ];
        for (i, spans) in lines.iter().enumerate() {
            draw_spans(
                buf,
                text_x,
                area.y + i as u16,
                text_w,
                &truncate_spans(spans, text_w as usize, glyphs.ellipsis),
            );
        }
        let tabs = Self::tab_strip_spans(theme, sep);
        draw_spans(
            buf,
            text_x,
            area.y + 3,
            text_w,
            &truncate_spans(&tabs, text_w as usize, glyphs.ellipsis),
        );
        4
    }

    fn row_line(
        &self,
        row: &TicketRow,
        cols: Columns,
        selected: bool,
        theme: &Theme,
        glyphs: &Glyphs,
        now: Timestamp,
    ) -> Line {
        let Columns {
            width,
            name_w,
            age_w,
        } = cols;
        let marks = Marks::for_glyphs(glyphs);
        let colour = Self::group_color(row.group, row.tint, theme);
        let muted = Style::default().fg(theme.muted);
        let finished = row.group == Group::Completed;
        let mut name_style = if finished { muted } else { Style::default() };
        if selected {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        let id_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            muted
        };
        let name = truncate(
            &format!("{} {}", row.id, row.title),
            name_w,
            glyphs.ellipsis,
        );
        let (id_part, title_part) = match name.split_once(' ') {
            Some((id, title)) => (id.to_string(), format!(" {title}")),
            None => (name.clone(), String::new()),
        };
        let pad = name_w.saturating_sub(display_width(&name));
        let summary_style = match row.tint {
            Tint::Failure => Style::default().fg(theme.danger),
            _ if finished || row.group == Group::Queued => muted,
            _ => Style::default(),
        };
        // glyph + space + name + gap + summary + gap + age
        let summary_w = width.saturating_sub(2 + name_w + 2 + 2 + age_w);
        let summary = truncate(&row.summary, summary_w, glyphs.ellipsis);
        let summary_pad = summary_w.saturating_sub(display_width(&summary));
        let mut spans = vec![
            Span::new(Self::glyph(row, &marks, now), Style::default().fg(colour)),
            Span::new(" ", Style::default()),
            Span::new(id_part, id_style),
            Span::new(title_part, name_style),
            Span::new(" ".repeat(pad + 2), Style::default()),
        ];
        if summary_w > 0 {
            spans.push(Span::new(summary, summary_style));
            spans.push(Span::new(" ".repeat(summary_pad + 2), Style::default()));
        }
        spans.push(Span::new(format!("{:>age_w$}", row.age), muted));
        Line::from_spans(truncate_spans(&spans, width, glyphs.ellipsis))
    }

    fn render_list(
        &self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        glyphs: &Glyphs,
        now: Timestamp,
    ) {
        let height = area.height as usize;
        self.list_height.set(height);
        let items = self.items(height);
        let width = area.width as usize;
        let rows: Vec<&TicketRow> = items
            .iter()
            .filter_map(|i| match i {
                Item::Row(r) => Some(*r),
                _ => None,
            })
            .collect();
        let name_w = rows
            .iter()
            .map(|r| display_width(&r.id) + 1 + display_width(&r.title))
            .max()
            .unwrap_or(0)
            .clamp(12, (width / 3).clamp(12, 40));
        let age_w = rows
            .iter()
            .map(|r| display_width(&r.age))
            .max()
            .unwrap_or(0);

        // Scroll just enough to keep the selection in view.
        let selected_line = items
            .iter()
            .position(|i| i.sel().is_some() && i.sel() == self.selected)
            .unwrap_or(0);
        let mut first = self.scroll.get().min(items.len().saturating_sub(height));
        if selected_line < first {
            first = selected_line.saturating_sub(1);
        } else if selected_line >= first + height {
            first = selected_line + 1 - height;
        }
        self.scroll.set(first);

        let band = (theme.surface != Color::Reset).then(|| Style::default().bg(theme.surface));
        let muted = Style::default().fg(theme.muted);
        for (i, item) in items.iter().enumerate().skip(first).take(height) {
            let y = area.y + (i - first) as u16;
            let selected = item.sel().is_some() && item.sel() == self.selected;
            let mut line = match item {
                Item::Blank => continue,
                Item::Header(group, count) => {
                    let mut spans = vec![Span::new(
                        group.title(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )];
                    if self.collapsed.contains(group) {
                        spans.push(Span::new(format!("{}{count} hidden", glyphs.sep), muted));
                    }
                    Line::from_spans(spans)
                }
                Item::Description(group) => {
                    Line::from_spans(vec![Span::new(format!("  {}", group.description()), muted)])
                }
                Item::Row(row) => {
                    let cols = Columns {
                        width,
                        name_w,
                        age_w,
                    };
                    self.row_line(row, cols, selected, theme, glyphs, now)
                }
                Item::More(n)
                    if matches!(items.get(i.wrapping_sub(1)), Some(Item::Blank) | None) =>
                {
                    Line::from_spans(vec![
                        Span::new(
                            Group::Completed.title(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::new(
                            format!("{}{} {n} more", glyphs.sep, Marks::for_glyphs(glyphs).more),
                            muted,
                        ),
                    ])
                }
                Item::More(n) => Line::from_spans(vec![Span::new(
                    format!("  {} {n} more", Marks::for_glyphs(glyphs).more),
                    muted,
                )]),
            };
            line = Line::from_spans(truncate_spans(&line.spans, width, glyphs.ellipsis));
            if selected {
                match band {
                    Some(band) => line = line.with_fill(band),
                    // No colour to band with: the selection is reverse video instead.
                    None => {
                        line = line.with_fill(Style::default().add_modifier(Modifier::REVERSED))
                    }
                }
            }
            draw_line(buf, area.x, y, area.width, &line);
        }
    }

    fn peek_lines(&self, row: &TicketRow, width: usize, theme: &Theme) -> Vec<Line> {
        let label_w = row
            .peek
            .iter()
            .map(|p| display_width(&p.label))
            .max()
            .unwrap_or(0)
            .min(14);
        let text_w = width.saturating_sub(label_w + 2).max(8);
        let muted = Style::default().fg(theme.muted);
        let mut lines = Vec::new();
        // First, so a long peek never scrolls the choices out of view.
        for (i, choice) in row.choices.iter().enumerate() {
            let style = if i == 0 {
                Style::default().fg(theme.success)
            } else {
                Style::default()
            };
            lines.push(Line::from_spans(vec![Span::new(
                format!("{}. {}", i + 1, choice.label()),
                style,
            )]));
        }
        if !row.choices.is_empty() {
            lines.push(Line::from_spans(vec![Span::new(String::new(), muted)]));
        }
        for item in &row.peek {
            let style = match item.tint {
                Tint::Failure => Style::default().fg(theme.danger),
                Tint::Stopped => muted,
                Tint::Success => Style::default().fg(theme.success),
                Tint::Normal => Style::default(),
            };
            let wrapped = if item.text.is_empty() {
                vec![String::new()]
            } else {
                wrap(&item.text, text_w)
            };
            for (i, text) in wrapped.into_iter().enumerate() {
                let label = if i == 0 {
                    format!("{:<label_w$}  ", truncate(&item.label, label_w, ""))
                } else {
                    " ".repeat(label_w + 2)
                };
                lines.push(Line::from_spans(vec![
                    Span::new(label, muted),
                    Span::new(text, style),
                ]));
            }
        }
        lines
    }

    fn render_peek(
        &self,
        area: Rect,
        buf: &mut Buffer,
        row: &TicketRow,
        theme: &Theme,
        glyphs: &Glyphs,
    ) {
        if area.width < 12 || area.height < 3 {
            return;
        }
        clear(buf, area);
        let border = Style::default().fg(Self::group_color(row.group, row.tint, theme));
        draw_box(buf, area, &glyphs.border, border);
        let title = format!(" {} {} ", row.id, row.title);
        let title = truncate(
            &title,
            (area.width as usize).saturating_sub(4),
            glyphs.ellipsis,
        );
        buf.set_string(
            area.x + 2,
            area.y,
            &title,
            Style::default().add_modifier(Modifier::BOLD),
        );
        let inner_w = area.width.saturating_sub(4);
        let inner_h = area.height.saturating_sub(2) as usize;
        let lines = self.peek_lines(row, inner_w as usize, theme);
        let overflow = lines.len() > inner_h;
        for (i, line) in lines.iter().take(inner_h).enumerate() {
            let line = if overflow && i + 1 == inner_h {
                Line::from_spans(vec![Span::new(
                    format!("{} more in the chat: enter to attach", glyphs.ellipsis),
                    Style::default().fg(theme.muted),
                )])
            } else {
                line.clone()
            };
            draw_line(buf, area.x + 2, area.y + 1 + i as u16, inner_w, &line);
        }
    }

    fn peek_height(&self, row: &TicketRow, width: u16, theme: &Theme) -> u16 {
        self.peek_lines(row, width.saturating_sub(4) as usize, theme)
            .len() as u16
            + 2
    }

    fn render_help(&self, area: Rect, buf: &mut Buffer, theme: &Theme, glyphs: &Glyphs) {
        let keys: &[(&str, &str)] = &[
            ("↑ ↓", "move between tickets"),
            (
                "enter",
                "attach: open the chat on the ticket, or dispatch the typed task",
            ),
            ("→", "attach to the selected ticket"),
            (
                "space",
                "peek: objective, attempts, activity, failures, submission",
            ),
            (
                "ctrl+x",
                "cancel the ticket (press again within 2s to confirm)",
            ),
            (
                "space, then 1 2 3",
                "a ticket's options: accept or reject a submission, retry an escalated ticket, \
                 queue a draft",
            ),
            ("ctrl+b", "open the Kanban board"),
            ("enter on a heading", "collapse or expand the group"),
            ("shift+enter  ctrl+j", "newline in the dispatch input"),
            ("esc", "close peek, clear the input, or go back to the chat"),
            ("ctrl+t", "back to the chat"),
            ("ctrl+c", "clear the input; twice to exit"),
            ("?", "this list"),
        ];
        let key_w = keys
            .iter()
            .map(|(k, _)| display_width(k))
            .max()
            .unwrap_or(0);
        let height = (keys.len() as u16 + 4).min(area.height);
        let rect = Rect::new(area.x, area.y, area.width, height);
        if rect.width < 12 || rect.height < 3 {
            return;
        }
        clear(buf, rect);
        draw_box(buf, rect, &glyphs.border, Style::default().fg(theme.accent));
        buf.set_string(
            rect.x + 2,
            rect.y,
            " Shortcuts ",
            Style::default().add_modifier(Modifier::BOLD),
        );
        let inner_w = rect.width.saturating_sub(4);
        for (i, (key, what)) in keys.iter().enumerate() {
            let y = rect.y + 1 + i as u16;
            if y + 1 >= rect.y + rect.height {
                break;
            }
            let key = if glyphs.unicode {
                key.to_string()
            } else {
                key.replace("↑ ↓", "up down").replace('→', "right")
            };
            let spans = vec![
                Span::new(
                    format!("{key:<key_w$}  "),
                    Style::default().fg(theme.accent),
                ),
                Span::new(*what, Style::default()),
            ];
            draw_spans(
                buf,
                rect.x + 2,
                y,
                inner_w,
                &truncate_spans(&spans, inner_w as usize, glyphs.ellipsis),
            );
        }
        let close = "esc or ? to close";
        if rect.height >= 3 && (display_width(close) as u16) + 4 < rect.width {
            buf.set_string(
                rect.x + rect.width - display_width(close) as u16 - 2,
                rect.y + rect.height - 1,
                close,
                Style::default().fg(theme.muted),
            );
        }
    }

    fn footer(&self, glyphs: &Glyphs, theme: &Theme, now: Timestamp) -> Vec<Span> {
        let muted = Style::default().fg(theme.muted);
        if let Some(flash) = self
            .flash
            .as_ref()
            .filter(|f| now.millis_since(f.until) < 0)
        {
            let colour = match flash.tone {
                FlashTone::Info => theme.foreground,
                FlashTone::Success => theme.success,
                FlashTone::Error => theme.danger,
            };
            return vec![Span::new(flash.text.clone(), Style::default().fg(colour))];
        }
        if let Some((id, at)) = &self.cancel_armed {
            if now.millis_since(*at) <= CANCEL_CONFIRM_MILLIS {
                return vec![Span::new(
                    format!(
                        "Press ctrl+x again to cancel {id}{}esc to keep it",
                        glyphs.sep
                    ),
                    Style::default().fg(theme.warning),
                )];
            }
        }
        let hints: Vec<&str> = if let Some((id, choice)) = &self.asking {
            let what = match choice {
                Choice::Reject => format!("enter to reject {id} with this reason"),
                _ => format!("enter to retry {id} with this guidance"),
            };
            return vec![Span::new(
                format!("{what}{}esc to keep it as it is", glyphs.sep),
                Style::default().fg(theme.warning),
            )];
        } else if let Some(row) = self
            .selected_row()
            .filter(|r| self.peek_open && self.input.is_empty() && !r.choices.is_empty())
        {
            let mut hints: Vec<String> = row
                .choices
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{} to {}", i + 1, c.verb()))
                .collect();
            hints.push("enter to open".into());
            hints.push("space to close".into());
            return vec![Span::new(hints.join(glyphs.sep), muted)];
        } else if !self.input.is_empty() {
            vec![
                "enter to dispatch",
                "shift+enter for a newline",
                "esc to clear",
            ]
        } else {
            match (&self.selected, self.selected_row()) {
                (_, Some(row)) if row.group == Group::Review => vec![
                    "space to review",
                    "enter to open",
                    "ctrl+x to cancel",
                    "? for shortcuts",
                ],
                (_, Some(_)) if self.peek_open => vec![
                    "enter to open",
                    "space to close",
                    "↑↓ to peek at others",
                    "? for shortcuts",
                ],
                (_, Some(row)) if !row.choices.is_empty() => vec![
                    "enter to open",
                    "space for options",
                    "ctrl+x to cancel",
                    "? for shortcuts",
                ],
                (_, Some(_)) => vec![
                    "enter to open",
                    "space to peek",
                    "ctrl+x to cancel",
                    "? for shortcuts",
                ],
                (Some(Sel::Header(g)), _) if self.collapsed.contains(g) => {
                    vec!["enter to expand", "esc to go back", "? for shortcuts"]
                }
                (Some(Sel::Header(_)), _) => {
                    vec!["enter to collapse", "esc to go back", "? for shortcuts"]
                }
                (Some(Sel::More), _) => {
                    vec![
                        "enter to show all",
                        "ctrl+b for the board",
                        "? for shortcuts",
                    ]
                }
                _ => vec![
                    "type a task",
                    "ctrl+b for the board",
                    "esc to go back",
                    "? for shortcuts",
                ],
            }
        };
        let text = hints
            .iter()
            .map(|h| {
                if glyphs.unicode {
                    h.to_string()
                } else {
                    h.replace("↑↓", "up/down")
                }
            })
            .collect::<Vec<_>>()
            .join(glyphs.sep);
        vec![Span::new(text, muted)]
    }

    fn render_input(&self, area: Rect, buf: &mut Buffer, theme: &Theme, glyphs: &Glyphs) {
        let marks = Marks::for_glyphs(glyphs);
        let rejecting = self.asking.is_some();
        let marker_style = Style::default()
            .fg(if rejecting {
                theme.warning
            } else {
                theme.muted
            })
            .add_modifier(Modifier::BOLD);
        buf.set_string(area.x, area.y, marks.prompt, marker_style);
        let text_x = area.x + 2;
        let width = area.width.saturating_sub(3) as usize;
        let cursor_style = if theme.foreground == Color::Reset {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.background).bg(theme.foreground)
        };
        if width == 0 {
            return;
        }
        if self.input.is_empty() {
            let placeholder = match &self.asking {
                Some((id, Choice::Reject)) => {
                    format!("Why reject {id}? The next attempt sees your reason")
                }
                Some((id, _)) => format!("What should {id}'s next attempt do differently?"),
                None if glyphs.unicode => PLACEHOLDER.to_string(),
                None => PLACEHOLDER.to_string(),
            };
            let shown = truncate(&placeholder, width.saturating_sub(1), glyphs.ellipsis);
            buf.set_stringn(
                text_x,
                area.y,
                &shown,
                width,
                Style::default().fg(theme.muted),
            );
            buf.set_style(Rect::new(text_x, area.y, 1, 1), cursor_style);
            return;
        }
        let text = self.input.text();
        let rows = self.input.rows(width);
        let cursor = self.input.cursor();
        let cursor_row = rows.iter().rposition(|r| r.start <= cursor).unwrap_or(0);
        let visible = area.height as usize;
        let first = cursor_row.saturating_sub(visible.saturating_sub(1));
        for (i, row) in rows.iter().enumerate().skip(first).take(visible) {
            let y = area.y + (i - first) as u16;
            let slice = text.get(row.start..row.end).unwrap_or("");
            buf.set_stringn(text_x, y, slice, width, Style::default());
            if i == cursor_row {
                let before = text.get(row.start..cursor.max(row.start)).unwrap_or("");
                let x = text_x + display_width(before).min(width) as u16;
                if x < area.x + area.width {
                    buf.set_style(Rect::new(x, y, 1, 1), cursor_style);
                }
            }
        }
    }

    fn input_height(&self, width: u16) -> u16 {
        if self.input.is_empty() {
            return 1;
        }
        (self
            .input
            .rows(width.saturating_sub(3).max(1) as usize)
            .len() as u16)
            .clamp(1, 4)
    }
}

impl Component for TicketsScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width < 16 || area.height < 6 {
            // Too small to be useful: say so rather than draw a broken frame.
            buf.set_stringn(
                area.x,
                area.y,
                "tm tickets: enlarge the terminal",
                area.width as usize,
                Style::default().fg(ctx.theme.muted),
            );
            return;
        }
        let theme = ctx.theme;
        let glyphs = Glyphs::for_caps(ctx.caps);
        let now = ctx.clock.now();
        let muted = Style::default().fg(theme.muted);
        let x = area.x + 2;
        let width = area.width.saturating_sub(4);
        let bottom = area.y + area.height;

        // Bottom block, drawn upward: footer, rule, input, rule.
        let footer_y = bottom - 1;
        let input_h = self.input_height(width);
        let rule_below = footer_y.saturating_sub(1);
        let input_y = rule_below.saturating_sub(input_h);
        let rule_above = input_y.saturating_sub(1);
        let rule = glyphs.rule.repeat(width as usize);
        let footer = self.footer(&glyphs, theme, now);
        draw_spans(
            buf,
            x + 2,
            footer_y,
            width.saturating_sub(2),
            &truncate_spans(&footer, width.saturating_sub(2) as usize, glyphs.ellipsis),
        );
        buf.set_stringn(x, rule_below, &rule, width as usize, muted);
        self.render_input(Rect::new(x, input_y, width, input_h), buf, theme, &glyphs);
        buf.set_stringn(x, rule_above, &rule, width as usize, muted);

        // Header.
        let compact = area.height < 18 || width < 40;
        let header_h = self.render_header(
            Rect::new(x, area.y + u16::from(!compact), width, 4),
            buf,
            theme,
            &glyphs,
            compact,
        );
        let mut top = area.y + u16::from(!compact) + header_h + 1;

        // The empty state's one-line explanation sits just above the input.
        let mut list_bottom = rule_above.saturating_sub(1);
        if self.data.rows.is_empty() && list_bottom > top + 2 {
            let hint = "Dispatch a task to get started: describe it below and press Enter.";
            buf.set_stringn(x, list_bottom - 1, hint, width as usize, muted);
            list_bottom -= 2;
        }
        if top >= list_bottom {
            top = area.y + header_h;
        }
        let list_area = Rect::new(x, top, width, list_bottom.saturating_sub(top));

        if self.help_open {
            self.render_help(list_area, buf, theme, &glyphs);
            return;
        }

        match self.selected_row().filter(|_| self.peek_open) {
            Some(row) if list_area.height >= 8 => {
                let peek_h = self
                    .peek_height(row, width, theme)
                    .min(list_area.height / 2)
                    .max(4);
                let list_h = list_area.height - peek_h;
                self.render_list(
                    Rect::new(x, list_area.y, width, list_h.saturating_sub(1)),
                    buf,
                    theme,
                    &glyphs,
                    now,
                );
                self.render_peek(
                    Rect::new(x, list_area.y + list_h, width, peek_h),
                    buf,
                    row,
                    theme,
                    &glyphs,
                );
            }
            Some(row) => {
                // No room for both: the peek panel takes the list's place.
                self.render_peek(list_area, buf, row, theme, &glyphs);
            }
            None => self.render_list(list_area, buf, theme, &glyphs, now),
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        let now = ctx.clock.now();
        match event {
            Event::Input(InputEvent::Key(key)) => {
                if key.kind == KeyEventKind::Release {
                    return Propagation::Consumed;
                }
                self.handle_key(key, now)
            }
            Event::Input(InputEvent::Paste(text)) => {
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                self.input.insert(&text);
                Propagation::Consumed
            }
            Event::Input(InputEvent::Mouse(mouse)) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.move_selection(-1);
                    Propagation::Consumed
                }
                MouseEventKind::ScrollDown => {
                    self.move_selection(1);
                    Propagation::Consumed
                }
                _ => Propagation::Propagate,
            },
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, _ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(KeyCode::Up), "select previous"),
            KeyBinding::new(KeyChord::plain(KeyCode::Down), "select next"),
            KeyBinding::new(
                KeyChord::plain(KeyCode::Enter),
                "attach, or dispatch the task",
            ),
            KeyBinding::new(KeyChord::plain(KeyCode::Char(' ')), "peek"),
            KeyBinding::new(
                KeyChord {
                    code: KeyCode::Char('x'),
                    modifiers: KeyModifiers::CONTROL,
                },
                "cancel (press twice)",
            ),
            KeyBinding::new(KeyChord::plain(KeyCode::Tab), "next tab"),
            KeyBinding::new(KeyChord::plain(KeyCode::BackTab), "previous tab"),
            KeyBinding::new(KeyChord::plain(KeyCode::Esc), "back to chat"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{Capabilities, ColorSupport, UnicodeSupport};
    use crate::component::FocusState;
    use tm_types::FixedClock;

    fn row(id: &str, group: Group, summary: &str) -> TicketRow {
        TicketRow {
            id: id.to_string(),
            title: format!("title of {id}"),
            group,
            worker: if group == Group::Working {
                Worker::Working
            } else {
                Worker::None
            },
            summary: summary.to_string(),
            tint: Tint::Normal,
            age: "3m".to_string(),
            order: id.trim_start_matches("T-").parse().unwrap_or(0),
            peek: vec![
                PeekItem::new("objective", format!("the whole objective of {id}")),
                PeekItem::new("state", "running"),
            ],
            choices: match group {
                Group::Review => vec![Choice::Accept, Choice::Reject],
                Group::NeedsInput => vec![Choice::Retry, Choice::RetryWithGuidance],
                _ => Vec::new(),
            },
        }
    }

    fn data() -> TicketsData {
        TicketsData {
            version: "0.1.0".to_string(),
            model: "mock/m1".to_string(),
            place: "~/proj (main)".to_string(),
            notice: None,
            rows: vec![
                row("T-1", Group::Completed, "result: set up CI"),
                row("T-2", Group::Working, "$ cargo test"),
                row(
                    "T-3",
                    Group::Queued,
                    "queued, no worker running (tm sched run)",
                ),
                row("T-4", Group::NeedsInput, "retry or give up?"),
                row("T-5", Group::Review, "all tests pass"),
            ],
        }
    }

    struct Env {
        theme: Theme,
        caps: Capabilities,
        clock: FixedClock,
    }

    impl Env {
        fn new(unicode: bool) -> Self {
            Env {
                theme: Theme::dark(),
                caps: Capabilities {
                    color: ColorSupport::TrueColor,
                    unicode: if unicode {
                        UnicodeSupport::NarrowOnly
                    } else {
                        UnicodeSupport::AsciiOnly
                    },
                    ..Capabilities::minimal()
                },
                clock: FixedClock::epoch(),
            }
        }

        fn ctx(&self) -> FrameContext<'_> {
            FrameContext {
                theme: &self.theme,
                caps: &self.caps,
                clock: &self.clock,
                focus: FocusState::default(),
            }
        }
    }

    fn render(screen: &TicketsScreen, width: u16, height: u16, unicode: bool) -> Vec<String> {
        let env = Env::new(unicode);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, &env.ctx());
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn key_with(screen: &mut TicketsScreen, env: &Env, code: KeyCode, mods: KeyModifiers) {
        screen.handle_event(
            &Event::Input(InputEvent::Key(KeyEvent::new(code, mods))),
            &env.ctx(),
        );
    }

    fn press(screen: &mut TicketsScreen, env: &Env, code: KeyCode) {
        key_with(screen, env, code, KeyModifiers::NONE);
    }

    fn type_text(screen: &mut TicketsScreen, env: &Env, text: &str) {
        for c in text.chars() {
            press(screen, env, KeyCode::Char(c));
        }
    }

    fn screen() -> TicketsScreen {
        TicketsScreen::new(ComponentId::new("test.tickets"), data())
    }

    #[test]
    fn groups_render_in_the_agent_view_order_with_counts_in_the_header() {
        let s = screen();
        let text = render(&s, 100, 40, true).join("\n");
        let pos = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from:\n{text}"))
        };
        assert!(pos("Needs input") < pos("Working"));
        assert!(pos("Working") < pos("Ready for review"));
        assert!(pos("Ready for review") < pos("Queued"));
        assert!(pos("Queued") < pos("Completed"));
        assert!(pos("T-4 title of T-4") < pos("T-2 title of T-2"));
        assert!(text
            .contains("1 needs input · 1 working · 1 ready for review · 1 queued · 1 completed"));
        assert!(text.contains("Ticketmaster v0.1.0"));
        assert!(text.contains("mock/m1 · ~/proj (main)"));
        assert!(text.contains(PLACEHOLDER));
        // The needs-input row is first, so it is selected and its hints show.
        assert!(text.contains("enter to open · space for options · ctrl+x to cancel"));
    }

    #[test]
    fn a_row_has_glyph_name_summary_and_a_right_aligned_age() {
        let s = screen();
        let rows = render(&s, 100, 40, true);
        let line = rows
            .iter()
            .find(|l| l.contains("T-3 title of T-3"))
            .expect("T-3 row");
        assert!(line.trim_start().starts_with('∙'), "{line:?}");
        assert!(line.contains("queued, no worker running (tm sched run)"));
        assert!(line.ends_with("3m"), "{line:?}");
        // Right-aligned: every row's age ends in the same column.
        let ends: BTreeSet<usize> = rows
            .iter()
            .filter(|l| l.contains(" title of T-"))
            .map(|l| l.chars().count())
            .collect();
        assert_eq!(ends.len(), 1, "{rows:#?}");
    }

    #[test]
    fn working_rows_animate_and_idle_rows_do_not() {
        let s = screen();
        let env = Env::new(true);
        let mut frames = BTreeSet::new();
        for step in 0..12 {
            env.clock.advance_millis(150);
            let area = Rect::new(0, 0, 100, 40);
            let mut buf = Buffer::empty(area);
            s.render(area, &mut buf, &env.ctx());
            let y = (0..40)
                .find(|y| {
                    (0..100)
                        .map(|x| buf[(x, *y)].symbol().to_string())
                        .collect::<String>()
                        .contains("T-2 title")
                })
                .unwrap_or_else(|| panic!("frame {step}: T-2 row missing"));
            frames.insert(buf[(2, y)].symbol().to_string());
        }
        assert!(
            frames.len() > 1,
            "the working glyph must animate: {frames:?}"
        );
    }

    #[test]
    fn enter_and_right_attach_the_selected_ticket() {
        let env = Env::new(true);
        let mut s = screen();
        assert_eq!(s.selected_ticket(), Some("T-4"));
        press(&mut s, &env, KeyCode::Enter);
        assert_eq!(s.take_actions(), vec![TicketsAction::Attach("T-4".into())]);
        press(&mut s, &env, KeyCode::Down); // Working heading
        press(&mut s, &env, KeyCode::Down); // T-2
        press(&mut s, &env, KeyCode::Right);
        assert_eq!(s.take_actions(), vec![TicketsAction::Attach("T-2".into())]);
    }

    #[test]
    fn typing_then_enter_dispatches_and_letters_are_text() {
        let env = Env::new(true);
        let mut s = screen();
        // Once anything is typed, every letter (the shortcut ones too) is text.
        type_text(&mut s, &env, "fix the board: a b r ? and space");
        assert_eq!(s.input_text(), "fix the board: a b r ? and space");
        key_with(&mut s, &env, KeyCode::Enter, KeyModifiers::SHIFT);
        type_text(&mut s, &env, "second line");
        press(&mut s, &env, KeyCode::Enter);
        assert_eq!(
            s.take_actions(),
            vec![TicketsAction::Dispatch(
                "fix the board: a b r ? and space\nsecond line".into()
            )]
        );
        assert_eq!(s.input_text(), "");
        key_with(&mut s, &env, KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(s.input_text(), "\n");
        press(&mut s, &env, KeyCode::Enter);
        assert!(
            s.take_actions().is_empty(),
            "blank tasks are not dispatched"
        );
    }

    #[test]
    fn space_toggles_the_peek_panel_with_the_selected_tickets_detail() {
        let env = Env::new(true);
        let mut s = screen();
        press(&mut s, &env, KeyCode::Char(' '));
        assert!(s.is_peek_open());
        let text = render(&s, 100, 40, true).join("\n");
        assert!(text.contains("the whole objective of T-4"), "{text}");
        // Arrows peek at the next ticket without closing, skipping the heading between them.
        press(&mut s, &env, KeyCode::Down);
        assert_eq!(s.selected_ticket(), Some("T-2"));
        let text = render(&s, 100, 40, true).join("\n");
        assert!(text.contains("the whole objective of T-2"), "{text}");
        press(&mut s, &env, KeyCode::Esc);
        assert!(!s.is_peek_open());
        assert!(
            s.take_actions().is_empty(),
            "Esc closed the peek, not the screen"
        );
    }

    #[test]
    fn ctrl_x_twice_within_the_window_cancels() {
        let env = Env::new(true);
        let mut s = screen();
        key_with(&mut s, &env, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(s.take_actions().is_empty());
        let text = render(&s, 100, 40, true).join("\n");
        assert!(text.contains("Press ctrl+x again to cancel T-4"), "{text}");
        env.clock.advance_millis(500);
        key_with(&mut s, &env, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(s.take_actions(), vec![TicketsAction::Cancel("T-4".into())]);

        // Too slow: the second press only re-arms.
        key_with(&mut s, &env, KeyCode::Char('x'), KeyModifiers::CONTROL);
        env.clock.advance_millis(CANCEL_CONFIRM_MILLIS + 1);
        key_with(&mut s, &env, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(s.take_actions().is_empty());
    }

    #[test]
    fn a_review_tickets_peek_answers_1_and_2_and_letters_are_always_text() {
        let env = Env::new(true);
        let mut s = screen();
        // On a review row with the peek closed, `1` and letters are text.
        assert!(s.select_ticket("T-5"));
        for c in ['a', 'r', 'b', '1'] {
            press(&mut s, &env, KeyCode::Char(c));
        }
        assert_eq!(s.input_text(), "arb1");
        assert!(s.take_actions().is_empty());
        press(&mut s, &env, KeyCode::Esc);

        press(&mut s, &env, KeyCode::Char(' '));
        let text = render(&s, 100, 40, true).join("\n");
        assert!(text.contains("1. Accept"), "{text}");
        press(&mut s, &env, KeyCode::Char('1'));
        assert_eq!(s.take_actions(), vec![TicketsAction::Accept("T-5".into())]);

        press(&mut s, &env, KeyCode::Char('2'));
        let text = render(&s, 100, 40, true).join("\n");
        assert!(text.contains("Why reject T-5?"), "{text}");
        press(&mut s, &env, KeyCode::Enter);
        assert!(s.take_actions().is_empty(), "a reason is required");
        type_text(&mut s, &env, "tests do not cover the bug");
        press(&mut s, &env, KeyCode::Enter);
        assert_eq!(
            s.take_actions(),
            vec![TicketsAction::Reject {
                id: "T-5".into(),
                reason: "tests do not cover the bug".into()
            }]
        );
    }

    #[test]
    fn an_escalated_tickets_peek_retries_with_or_without_guidance() {
        let env = Env::new(true);
        let mut s = screen();
        assert!(s.select_ticket("T-4"));
        press(&mut s, &env, KeyCode::Char(' '));
        let text = render(&s, 100, 40, true).join("\n");
        assert!(text.contains("1. Retry"), "{text}");
        assert!(text.contains("1 to retry"), "{text}");
        press(&mut s, &env, KeyCode::Char('1'));
        assert_eq!(
            s.take_actions(),
            vec![TicketsAction::Retry {
                id: "T-4".into(),
                guidance: None
            }]
        );
        press(&mut s, &env, KeyCode::Char('2'));
        let text = render(&s, 100, 40, true).join("\n");
        assert!(
            text.contains("What should T-4's next attempt do differently?"),
            "{text}"
        );
        type_text(&mut s, &env, "use the fixture in tests/data");
        press(&mut s, &env, KeyCode::Enter);
        assert_eq!(
            s.take_actions(),
            vec![TicketsAction::Retry {
                id: "T-4".into(),
                guidance: Some("use the fixture in tests/data".into())
            }]
        );
        // There is no third option, so `3` is just text.
        press(&mut s, &env, KeyCode::Char('3'));
        assert_eq!(s.input_text(), "3");
    }

    #[test]
    fn esc_unwinds_one_layer_at_a_time_then_goes_back() {
        let env = Env::new(true);
        let mut s = screen();
        type_text(&mut s, &env, "half a task");
        press(&mut s, &env, KeyCode::Esc);
        assert_eq!(s.input_text(), "");
        assert!(s.take_actions().is_empty());
        press(&mut s, &env, KeyCode::Esc);
        assert_eq!(s.take_actions(), vec![TicketsAction::BackToChat]);
        key_with(&mut s, &env, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(s.take_actions(), vec![TicketsAction::OpenBoard]);
        press(&mut s, &env, KeyCode::Char('?'));
        let text = render(&s, 100, 40, true).join("\n");
        assert!(text.contains("Shortcuts"), "{text}");
    }

    #[test]
    fn tab_and_shift_tab_ask_for_the_next_and_previous_tab() {
        let env = Env::new(true);
        let mut s = screen();
        press(&mut s, &env, KeyCode::Tab);
        assert_eq!(s.take_actions(), vec![TicketsAction::NextTab]);
        press(&mut s, &env, KeyCode::BackTab);
        assert_eq!(s.take_actions(), vec![TicketsAction::PrevTab]);
    }

    #[test]
    fn header_shows_the_tab_strip_with_tickets_highlighted() {
        let s = screen();
        let text = render(&s, 100, 40, true).join("\n");
        for tab in TicketsScreen::TABS {
            assert!(text.contains(tab), "{tab} missing from header:\n{text}");
        }
        let pos = |needle: &str| text.find(needle).unwrap();
        assert!(pos("Tickets") < pos("Board"));
        assert!(pos("Board") < pos("Milestones"));
        assert!(pos("Milestones") < pos("Timeline"));
        assert!(pos("Timeline") < pos("Graph"));
    }

    #[test]
    fn enter_on_a_heading_collapses_and_expands_its_group() {
        let env = Env::new(true);
        let mut s = screen();
        press(&mut s, &env, KeyCode::Up); // Needs input heading
        press(&mut s, &env, KeyCode::Enter);
        let text = render(&s, 100, 40, true).join("\n");
        assert!(!text.contains("T-4 title"), "{text}");
        assert!(text.contains("Needs input · 1 hidden"), "{text}");
        press(&mut s, &env, KeyCode::Enter);
        let text = render(&s, 100, 40, true).join("\n");
        assert!(text.contains("T-4 title"), "{text}");
    }

    #[test]
    fn selection_follows_its_ticket_into_another_group() {
        let env = Env::new(true);
        let mut s = screen();
        assert!(s.select_ticket("T-2"));
        let mut d = data();
        for r in &mut d.rows {
            if r.id == "T-2" {
                r.group = Group::Review;
            }
        }
        s.set_data(d);
        assert_eq!(s.selected_ticket(), Some("T-2"));
        // A ticket that disappears leaves the selection nearby, never nowhere.
        let mut d = data();
        d.rows.retain(|r| r.id != "T-2");
        s.set_data(d);
        assert!(s.selected.is_some());
        press(&mut s, &env, KeyCode::Down);
        press(&mut s, &env, KeyCode::Right);
        assert_eq!(s.take_actions().len(), 1, "{:?}", s.selected);
    }

    #[test]
    fn completed_folds_into_more_on_a_short_terminal_but_keeps_the_selection() {
        let mut d = data();
        for n in 10..40 {
            d.rows
                .push(row(&format!("T-{n}"), Group::Completed, "result: done"));
        }
        let mut s = TicketsScreen::new(ComponentId::new("t"), d);
        let text = render(&s, 80, 24, true).join("\n");
        assert!(text.contains("more"), "{text}");
        assert!(
            text.contains("Needs input"),
            "live groups stay visible: {text}"
        );
        // The oldest completed ticket, if selected, is shown even though it is folded.
        assert!(s.select_ticket("T-10"));
        let text = render(&s, 80, 24, true).join("\n");
        assert!(text.contains("T-10 title"), "{text}");
        // Enter on the fold shows everything (and the list scrolls).
        let env = Env::new(true);
        press(&mut s, &env, KeyCode::Down);
        let _ = render(&s, 80, 24, true);
        press(&mut s, &env, KeyCode::Enter);
        let _ = render(&s, 80, 24, true);
        assert!(s.show_all_completed);
    }

    #[test]
    fn the_empty_state_explains_every_group_and_how_to_start() {
        let s = TicketsScreen::new(
            ComponentId::new("t"),
            TicketsData {
                version: "0.1.0".into(),
                model: "mock/m1".into(),
                place: "~/new".into(),
                notice: Some("background worker off: no provider".into()),
                rows: vec![],
            },
        );
        let text = render(&s, 100, 40, true).join("\n");
        for g in Group::ALL {
            assert!(text.contains(g.title()), "{text}");
            assert!(text.contains(g.description()), "{text}");
        }
        assert!(text.contains("Dispatch a task to get started"), "{text}");
        assert!(
            text.contains("no tickets yet · background worker off"),
            "{text}"
        );
    }

    #[test]
    fn every_size_and_ascii_stays_in_bounds() {
        let mut s = screen();
        let env = Env::new(false);
        press(&mut s, &env, KeyCode::Char(' '));
        for (w, h) in [
            (80u16, 24u16),
            (40, 10),
            (16, 6),
            (12, 4),
            (200, 50),
            (80, 12),
        ] {
            for unicode in [true, false] {
                let rows = render(&s, w, h, unicode);
                assert_eq!(rows.len(), h as usize);
                if !unicode {
                    for r in &rows {
                        assert!(r.is_ascii(), "{w}x{h}: {r:?}");
                    }
                }
            }
        }
        let rows = render(&screen(), 80, 24, true);
        assert!(rows.iter().any(|r| r.contains("T-4")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains(PLACEHOLDER)), "{rows:#?}");
    }

    #[test]
    fn short_titles_take_the_first_clause_and_stay_short() {
        assert_eq!(
            short_title("Fix the flaky checkout test, it times out on CI"),
            "Fix the flaky checkout test"
        );
        assert_eq!(
            short_title("Update main.rs to v1.2. Then ship"),
            "Update main.rs to v1.2"
        );
        assert_eq!(
            short_title("api client retries: 429 handling"),
            "api client retries"
        );
        assert_eq!(short_title("\n\n  second line wins\n"), "second line wins");
        let long = short_title(
            "Rewrite the entire persistence layer on top of a brand new storage engine",
        );
        assert!(display_width(&long) <= 32, "{long}");
        assert!(long.ends_with('…'));
        let wide =
            short_title("修复结帐测试超时问题并且确保所有的测试都能在持续集成中稳定通过没有例外");
        assert!(display_width(&wide) <= 32, "{wide}");
        assert_eq!(short_title(""), "");
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(compact_age(5_000), "5s");
        assert_eq!(compact_age(12 * 60_000), "12m");
        assert_eq!(compact_age(3 * 3_600_000), "3h");
        assert_eq!(compact_age(2 * 86_400_000), "2d");
        assert_eq!(compact_age(-1), "0s");
    }
}
