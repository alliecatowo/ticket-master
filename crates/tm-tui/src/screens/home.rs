//! The home screen: the cross-session "agents" view (the `claude agents` analogue), one `←` away
//! from the chat.
//!
//! The chat ([`crate::screens::chat::ChatScreen`]) is where a human works; this is where they see
//! everything else at a glance — the conversations this project has had, the background tickets
//! the agent (or anyone) opened, and the workers currently executing them — and jump into any of
//! it. It is plain-data-in like every screen here: `tm-cli` builds [`HomeData`] from the project's
//! real state and hands it over; this module decides only how it looks and which keys do what.
//!
//! The current conversation is itself a selectable row (and the initial selection), which is
//! how "Enter returns to the chat" and "Enter opens a ticket" coexist without a mode.

use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{draw_line, draw_spans, truncate_spans, Line, Span};
use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::display_width;
use crate::theme::Theme;

/// How a ticket's state reads at a glance; decides its badge colour and its sort group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tone {
    /// Needs a human (escalated, blocked, in recovery or rework).
    Attention,
    /// Being worked on right now.
    Active,
    /// Waiting to be picked up.
    Queued,
    /// Finished successfully.
    Done,
    /// Cancelled.
    Inactive,
}

/// One ticket row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketRow {
    /// The ticket id (`T-4`).
    pub id: String,
    /// The real state name, shown verbatim as the badge (`Running`).
    pub state: String,
    /// How the state reads.
    pub tone: Tone,
    /// The objective.
    pub objective: String,
    /// A short note (`leased by worker-1`), if any.
    pub note: Option<String>,
}

/// One active worker (a live lease).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRow {
    /// Who holds the lease.
    pub holder: String,
    /// The ticket it holds.
    pub ticket: String,
    /// How long it has held it (`3m`).
    pub age: String,
}

/// One chat session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The session id (`S-3`).
    pub id: String,
    /// How many turns it ran.
    pub turns: usize,
    /// When it was last active, already relative (`2h ago`), if known.
    pub when: Option<String>,
    /// Whether this is the conversation the human came from.
    pub current: bool,
}

impl SessionRow {
    /// `4 turns · 2h ago`, joined with `sep`; empty for a session that has not run a turn.
    fn summary(&self, sep: &str) -> String {
        let mut parts = Vec::new();
        match self.turns {
            0 => {}
            1 => parts.push("1 turn".to_string()),
            n => parts.push(format!("{n} turns")),
        }
        if let Some(when) = &self.when {
            parts.push(when.clone());
        }
        parts.join(sep)
    }
}

/// Everything the home screen shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HomeData {
    /// Where the project is (`~/src/app (main)`).
    pub place: String,
    /// Every ticket, in any order (the screen groups and sorts them).
    pub tickets: Vec<TicketRow>,
    /// Live leases.
    pub workers: Vec<WorkerRow>,
    /// Sessions, most recent first; the current one is always present.
    pub sessions: Vec<SessionRow>,
}

/// What the human asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeAction {
    /// Go back to the conversation.
    BackToChat,
    /// Open a ticket's detail screen.
    OpenTicket(String),
    /// Open the Kanban board.
    OpenBoard,
}

/// How many finished tickets show before collapsing into a "+N more" row.
const DONE_SHOWN: usize = 3;

/// A row the selection can land on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Chat,
    Ticket(String),
}

/// The home screen.
#[derive(Debug)]
pub struct Home {
    id: ComponentId,
    data: HomeData,
    selected: usize,
    action: Option<HomeAction>,
}

impl Home {
    /// A home screen over `data`, with the current conversation selected.
    pub fn new(id: ComponentId, data: HomeData) -> Self {
        Home {
            id,
            data,
            selected: 0,
            action: None,
        }
    }

    /// Replace the data (fresh project state), keeping the selection on the same row if it still
    /// exists.
    pub fn set_data(&mut self, data: HomeData) {
        let current = self.targets().get(self.selected).cloned();
        self.data = data;
        let targets = self.targets();
        self.selected = current
            .and_then(|t| targets.iter().position(|x| *x == t))
            .unwrap_or(0)
            .min(targets.len().saturating_sub(1));
    }

    /// Select the current conversation again (on entering the screen).
    pub fn select_chat(&mut self) {
        self.selected = 0;
    }

    /// The data on screen.
    pub fn data(&self) -> &HomeData {
        &self.data
    }

    /// Take the action the last key produced, if any.
    pub fn take_action(&mut self) -> Option<HomeAction> {
        self.action.take()
    }

    /// Tickets in display order: grouped by tone, newest (highest number) first within a group.
    fn sorted_tickets(&self) -> Vec<&TicketRow> {
        let mut tickets: Vec<&TicketRow> = self.data.tickets.iter().collect();
        tickets.sort_by(|a, b| {
            a.tone
                .cmp(&b.tone)
                .then_with(|| id_number(&b.id).cmp(&id_number(&a.id)))
        });
        tickets
    }

    /// Tickets actually listed (finished ones beyond [`DONE_SHOWN`] collapse), and how many were
    /// collapsed.
    fn listed_tickets(&self) -> (Vec<&TicketRow>, usize) {
        let mut listed = Vec::new();
        let mut done_seen = 0;
        let mut hidden = 0;
        for ticket in self.sorted_tickets() {
            if ticket.tone >= Tone::Done {
                done_seen += 1;
                if done_seen > DONE_SHOWN {
                    hidden += 1;
                    continue;
                }
            }
            listed.push(ticket);
        }
        (listed, hidden)
    }

    fn targets(&self) -> Vec<Target> {
        let mut targets = vec![Target::Chat];
        targets.extend(
            self.listed_tickets()
                .0
                .into_iter()
                .map(|t| Target::Ticket(t.id.clone())),
        );
        targets
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.targets().len() as isize;
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, count - 1) as usize;
    }

    fn activate(&mut self) {
        self.action = match self.targets().get(self.selected) {
            Some(Target::Ticket(id)) => Some(HomeAction::OpenTicket(id.clone())),
            _ => Some(HomeAction::BackToChat),
        };
    }

    /// Build the scrollable body: every row, plus which target (if any) each row is.
    fn body(&self, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<(Line, Option<usize>)> {
        let muted = Style::default().fg(theme.muted);
        let heading = Style::default().add_modifier(Modifier::BOLD);
        let mut rows: Vec<(Line, Option<usize>)> = Vec::new();
        let selected = self.selected;
        let pointer = |target: usize| -> Span {
            if target == selected {
                Span::new(
                    format!("{} ", glyphs.pointer),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::new("  ", Style::default())
            }
        };
        let section = |title: &str, aside: String| -> Line {
            let mut spans = vec![Span::new(title.to_string(), heading)];
            if !aside.is_empty() {
                spans.push(Span::new(format!("  {aside}"), muted));
            }
            Line::from_spans(spans)
        };

        // Sessions.
        let past = self.data.sessions.iter().filter(|s| !s.current).count();
        rows.push((
            section(
                "Sessions",
                if past > 0 {
                    format!("{} earlier", past)
                } else {
                    String::new()
                },
            ),
            None,
        ));
        let current = self.data.sessions.iter().find(|s| s.current);
        let current_line = {
            let is_sel = selected == 0;
            let mut spans = vec![
                pointer(0),
                Span::new(glyphs.dot, Style::default().fg(theme.success)),
                Span::new(" ", Style::default()),
                Span::new(
                    current.map_or("this session".to_string(), |s| s.id.clone()),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::new("  this conversation", Style::default()),
            ];
            if let Some(s) = current {
                let summary = SessionRow {
                    when: None,
                    ..s.clone()
                }
                .summary(glyphs.sep);
                if !summary.is_empty() {
                    spans.push(Span::new(format!("{}{summary}", glyphs.sep), muted));
                }
            }
            if is_sel {
                spans.push(Span::new(
                    format!("{}enter to return", glyphs.sep),
                    Style::default().fg(theme.accent),
                ));
            }
            Line::from_spans(spans)
        };
        rows.push((current_line, Some(0)));
        for session in self.data.sessions.iter().filter(|s| !s.current).take(5) {
            rows.push((
                Line::from_spans(vec![
                    Span::new("    ", Style::default()),
                    Span::new(session.id.clone(), muted),
                    Span::new(format!("  {}", session.summary(glyphs.sep)), muted),
                ]),
                None,
            ));
        }
        rows.push((Line::blank(), None));

        // Background work.
        let (listed, hidden) = self.listed_tickets();
        let count = |tone: Tone| self.data.tickets.iter().filter(|t| t.tone == tone).count();
        let mut summary = Vec::new();
        for (tone, word) in [
            (Tone::Attention, "need attention"),
            (Tone::Active, "active"),
            (Tone::Queued, "queued"),
            (Tone::Done, "done"),
        ] {
            let n = count(tone);
            if n > 0 {
                summary.push(format!("{n} {word}"));
            }
        }
        rows.push((section("Background work", summary.join(glyphs.sep)), None));
        if listed.is_empty() {
            rows.push((
                Line::from_spans(vec![
                    Span::new(
                        "  No background work yet. Ask tm to plan something, or ",
                        muted,
                    ),
                    Span::new("tm ticket new", Style::default().fg(theme.accent)),
                    Span::new(".", muted),
                ]),
                None,
            ));
        }
        let badge_width = listed
            .iter()
            .map(|t| display_width(&t.state))
            .max()
            .unwrap_or(0)
            .max(5);
        let id_width = listed
            .iter()
            .map(|t| display_width(&t.id))
            .max()
            .unwrap_or(0);
        for (i, ticket) in listed.iter().enumerate() {
            let target = i + 1;
            let tone_style = tone_style(ticket.tone, theme);
            let dim = ticket.tone >= Tone::Done;
            let state = ticket.state.to_uppercase();
            let mut spans = vec![
                pointer(target),
                Span::new(
                    format!("{state:<badge_width$}"),
                    tone_style.add_modifier(Modifier::BOLD),
                ),
                Span::new("  ", Style::default()),
                Span::new(format!("{:<id_width$}", ticket.id), muted),
                Span::new("  ", Style::default()),
                Span::new(
                    ticket.objective.clone(),
                    if dim { muted } else { Style::default() },
                ),
            ];
            if let Some(note) = &ticket.note {
                spans.push(Span::new(format!("{}{note}", glyphs.sep), muted));
            }
            rows.push((
                Line::from_spans(truncate_spans(&spans, width, glyphs.ellipsis)),
                Some(target),
            ));
        }
        if hidden > 0 {
            rows.push((
                Line::from_spans(vec![
                    Span::new(format!("  + {hidden} more finished{}", glyphs.sep), muted),
                    Span::new("b", Style::default().fg(theme.accent)),
                    Span::new(" opens the board", muted),
                ]),
                None,
            ));
        }
        rows.push((Line::blank(), None));

        // Workers.
        rows.push((
            section(
                "Workers",
                if self.data.workers.is_empty() {
                    String::new()
                } else {
                    format!("{} running", self.data.workers.len())
                },
            ),
            None,
        ));
        if self.data.workers.is_empty() {
            rows.push((
                Line::from_spans(vec![
                    Span::new("  No workers running. ", muted),
                    Span::new("tm sched run", Style::default().fg(theme.accent)),
                    Span::new(" starts one to pick up ready tickets.", muted),
                ]),
                None,
            ));
        }
        for worker in &self.data.workers {
            rows.push((
                Line::from_spans(vec![
                    Span::new("  ", Style::default()),
                    Span::new(glyphs.dot, Style::default().fg(theme.accent)),
                    Span::new(format!(" {}", worker.holder), Style::default()),
                    Span::new(
                        format!("  on {}{}{}", worker.ticket, glyphs.sep, worker.age),
                        muted,
                    ),
                ]),
                None,
            ));
        }

        // The selected row sits on a raised band (when the terminal has the colours for one), so
        // the selection reads at a glance rather than only through the pointer glyph.
        let band = (theme.surface != ratatui_core::style::Color::Reset)
            .then(|| Style::default().bg(theme.surface));
        rows.into_iter()
            .map(|(line, target)| {
                let mut row = Line::from_spans(truncate_spans(&line.spans, width, glyphs.ellipsis));
                if let (Some(band), true) = (band, target == Some(selected)) {
                    row = row.with_fill(band);
                }
                (row, target)
            })
            .collect()
    }
}

/// The colour a tone's badge is drawn in.
pub fn tone_style(tone: Tone, theme: &Theme) -> Style {
    let color = match tone {
        Tone::Attention => theme.warning,
        Tone::Active => theme.accent,
        // Waiting is the unremarkable case: the terminal's own text colour.
        Tone::Queued => return Style::default(),
        Tone::Done => theme.success,
        Tone::Inactive => theme.muted,
    };
    Style::default().fg(color)
}

/// The numeric part of an id like `T-12`, for newest-first ordering (non-numeric ids sort last).
fn id_number(id: &str) -> u64 {
    id.rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

impl Component for Home {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width < 10 || area.height < 3 {
            return;
        }
        let theme = ctx.theme;
        let glyphs = Glyphs::for_caps(ctx.caps);
        let muted = Style::default().fg(theme.muted);
        let accent_bold = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let x = area.x + 2;
        let width = area.width.saturating_sub(4);

        // Header.
        let title = vec![
            Span::new("tm", accent_bold),
            Span::new(
                "  sessions & tickets",
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        let title_width: usize = title.iter().map(|s| display_width(&s.text)).sum();
        draw_spans(buf, x, area.y, width, &title);
        let place_room = (width as usize).saturating_sub(title_width + 2);
        if place_room > 8 {
            let place = truncate_spans(
                &[Span::new(self.data.place.clone(), muted)],
                place_room,
                glyphs.ellipsis,
            );
            let place_width: usize = place.iter().map(|s| display_width(&s.text)).sum();
            draw_spans(
                buf,
                x + width - place_width as u16,
                area.y,
                place_width as u16,
                &place,
            );
        }
        let rule = glyphs.rule.repeat(width as usize);
        buf.set_stringn(x, area.y + 1, &rule, width as usize, muted);

        // Footer.
        let footer_y = area.y + area.height - 1;
        let key = Style::default().fg(theme.accent);
        let footer = vec![
            Span::new(glyphs.updown, key),
            Span::new(" select", muted),
            Span::new(glyphs.sep, muted),
            Span::new("enter", key),
            Span::new(" open", muted),
            Span::new(glyphs.sep, muted),
            Span::new("b", key),
            Span::new(" board", muted),
            Span::new(glyphs.sep, muted),
            Span::new("esc", key),
            Span::new(" back to chat", muted),
            Span::new(glyphs.sep, muted),
            Span::new("ctrl+c", key),
            Span::new(" quit", muted),
        ];
        draw_spans(
            buf,
            x,
            footer_y,
            width,
            &truncate_spans(&footer, width as usize, glyphs.ellipsis),
        );

        // Body, scrolled so the selection stays visible.
        let body_top = area.y + 3;
        if footer_y <= body_top + 1 {
            return;
        }
        let body_height = (footer_y - 1 - body_top) as usize;
        let rows = self.body(width as usize, theme, &glyphs);
        let selected_row = rows
            .iter()
            .position(|(_, t)| *t == Some(self.selected))
            .unwrap_or(0);
        let first = if selected_row + 2 > body_height {
            (selected_row + 2 - body_height).min(rows.len().saturating_sub(body_height))
        } else {
            0
        };
        for (i, (line, _)) in rows.iter().skip(first).take(body_height).enumerate() {
            draw_line(buf, x, body_top + i as u16, width, line);
        }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &FrameContext<'_>) -> Propagation {
        match event {
            Event::Input(InputEvent::Key(key)) => {
                if key.kind == crossterm::event::KeyEventKind::Release {
                    return Propagation::Consumed;
                }
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    return Propagation::Propagate;
                }
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                    KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
                    KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
                    KeyCode::PageUp => self.move_selection(-10),
                    KeyCode::PageDown => self.move_selection(10),
                    KeyCode::Enter => self.activate(),
                    KeyCode::Esc | KeyCode::Right => self.action = Some(HomeAction::BackToChat),
                    KeyCode::Char('b') => self.action = Some(HomeAction::OpenBoard),
                    _ => {}
                }
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
            KeyBinding::new(KeyChord::plain(KeyCode::Enter), "open"),
            KeyBinding::new(KeyChord::plain(KeyCode::Char('b')), "open the board"),
            KeyBinding::new(KeyChord::plain(KeyCode::Esc), "back to chat"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{Capabilities, ColorSupport, UnicodeSupport};
    use crate::component::FocusState;
    use crossterm::event::KeyEvent;
    use tm_types::FixedClock;

    fn ticket(id: &str, state: &str, tone: Tone, objective: &str) -> TicketRow {
        TicketRow {
            id: id.to_string(),
            state: state.to_string(),
            tone,
            objective: objective.to_string(),
            note: None,
        }
    }

    fn data() -> HomeData {
        HomeData {
            place: "~/proj (main)".to_string(),
            tickets: vec![
                ticket("T-1", "Closed", Tone::Done, "set up CI"),
                ticket("T-2", "Running", Tone::Active, "fix the flaky test"),
                ticket("T-3", "Draft", Tone::Queued, "write docs"),
                ticket("T-4", "Escalated", Tone::Attention, "decide on the schema"),
            ],
            workers: vec![WorkerRow {
                holder: "worker:sched-1".to_string(),
                ticket: "T-2".to_string(),
                age: "3m".to_string(),
            }],
            sessions: vec![
                SessionRow {
                    id: "S-7".to_string(),
                    turns: 2,
                    when: Some("just now".to_string()),
                    current: true,
                },
                SessionRow {
                    id: "S-5".to_string(),
                    turns: 4,
                    when: Some("1h ago".to_string()),
                    current: false,
                },
            ],
        }
    }

    fn render(home: &Home, width: u16, height: u16, unicode: bool) -> Vec<String> {
        let theme = Theme::dark();
        let caps = Capabilities {
            color: ColorSupport::TrueColor,
            unicode: if unicode {
                UnicodeSupport::NarrowOnly
            } else {
                UnicodeSupport::AsciiOnly
            },
            ..Capabilities::minimal()
        };
        let clock = FixedClock::epoch();
        let ctx = FrameContext {
            theme: &theme,
            caps: &caps,
            clock: &clock,
            focus: FocusState::default(),
        };
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        home.render(area, &mut buf, &ctx);
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

    fn press(home: &mut Home, code: KeyCode) {
        let theme = Theme::dark();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let ctx = FrameContext {
            theme: &theme,
            caps: &caps,
            clock: &clock,
            focus: FocusState::default(),
        };
        home.handle_event(
            &Event::Input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))),
            &ctx,
        );
    }

    #[test]
    fn tickets_are_grouped_attention_first_with_state_badges() {
        let home = Home::new(ComponentId::new("test.home"), data());
        let rows = render(&home, 100, 30, true);
        let text = rows.join("\n");
        let pos = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from:\n{text}"))
        };
        assert!(pos("ESCALATED") < pos("RUNNING"));
        assert!(pos("RUNNING") < pos("DRAFT"));
        assert!(pos("DRAFT") < pos("CLOSED"));
        assert!(text.contains("1 need attention · 1 active · 1 queued · 1 done"));
        assert!(text.contains("worker:sched-1  on T-2 · 3m"));
        assert!(text.contains("S-5  4 turns · 1h ago"));
    }

    #[test]
    fn enter_on_the_current_session_returns_to_chat() {
        let mut home = Home::new(ComponentId::new("test.home"), data());
        let text = render(&home, 100, 30, true).join("\n");
        assert!(
            text.contains("› ● S-7  this conversation · 2 turns · enter to return"),
            "{text}"
        );
        press(&mut home, KeyCode::Enter);
        assert_eq!(home.take_action(), Some(HomeAction::BackToChat));
    }

    #[test]
    fn arrows_then_enter_open_the_first_ticket_in_display_order() {
        let mut home = Home::new(ComponentId::new("test.home"), data());
        press(&mut home, KeyCode::Down);
        press(&mut home, KeyCode::Enter);
        assert_eq!(
            home.take_action(),
            Some(HomeAction::OpenTicket("T-4".to_string()))
        );
        press(&mut home, KeyCode::Char('j'));
        press(&mut home, KeyCode::Enter);
        assert_eq!(
            home.take_action(),
            Some(HomeAction::OpenTicket("T-2".to_string()))
        );
    }

    #[test]
    fn esc_right_and_b_produce_their_actions() {
        let mut home = Home::new(ComponentId::new("test.home"), data());
        press(&mut home, KeyCode::Esc);
        assert_eq!(home.take_action(), Some(HomeAction::BackToChat));
        press(&mut home, KeyCode::Right);
        assert_eq!(home.take_action(), Some(HomeAction::BackToChat));
        press(&mut home, KeyCode::Char('b'));
        assert_eq!(home.take_action(), Some(HomeAction::OpenBoard));
        press(&mut home, KeyCode::Char('q'));
        assert_eq!(home.take_action(), None, "q is not a key here either");
    }

    #[test]
    fn empty_project_shows_helpful_empty_states() {
        let home = Home::new(
            ComponentId::new("test.home"),
            HomeData {
                place: "~/new".to_string(),
                sessions: vec![SessionRow {
                    id: "S-1".to_string(),
                    turns: 0,
                    when: None,
                    current: true,
                }],
                ..HomeData::default()
            },
        );
        let text = render(&home, 100, 20, true).join("\n");
        assert!(
            text.contains("No background work yet. Ask tm to plan something, or tm ticket new.")
        );
        assert!(text.contains("No workers running. tm sched run starts one"));
    }

    #[test]
    fn finished_tickets_collapse_beyond_a_few() {
        let mut d = data();
        for n in 10..20 {
            d.tickets
                .push(ticket(&format!("T-{n}"), "Closed", Tone::Done, "old work"));
        }
        let home = Home::new(ComponentId::new("test.home"), d);
        let text = render(&home, 100, 40, true).join("\n");
        assert_eq!(text.matches("CLOSED").count(), DONE_SHOWN);
        assert!(text.contains("+ 8 more finished"), "{text}");
    }

    #[test]
    fn selection_survives_a_data_refresh() {
        let mut home = Home::new(ComponentId::new("test.home"), data());
        press(&mut home, KeyCode::Down);
        press(&mut home, KeyCode::Down); // T-2
        let mut d = data();
        d.tickets
            .push(ticket("T-9", "Escalated", Tone::Attention, "new trouble"));
        home.set_data(d);
        press(&mut home, KeyCode::Enter);
        assert_eq!(
            home.take_action(),
            Some(HomeAction::OpenTicket("T-2".to_string()))
        );
    }

    #[test]
    fn a_short_terminal_scrolls_to_keep_the_selection_visible() {
        let mut d = data();
        for n in 20..40 {
            d.tickets.push(ticket(
                &format!("T-{n}"),
                "Ready",
                Tone::Queued,
                &format!("task {n}"),
            ));
        }
        let mut home = Home::new(ComponentId::new("test.home"), d);
        for _ in 0..18 {
            press(&mut home, KeyCode::Down);
        }
        let rows = render(&home, 80, 14, true);
        assert!(rows.iter().any(|r| r.contains('›')), "{rows:#?}");
    }

    #[test]
    fn ascii_and_narrow_rendering_stay_in_bounds() {
        let home = Home::new(ComponentId::new("test.home"), data());
        for (w, h) in [(80u16, 24u16), (40, 10), (12, 4), (200, 50)] {
            let rows = render(&home, w, h, false);
            for row in &rows {
                assert!(row.is_ascii(), "{w}x{h}: {row:?}");
            }
        }
    }
}
