//! The chat screen: bare `tm`'s default view. A conversation transcript, a prompt box, and a
//! status bar — the Claude-Code-shaped surface, with tickets kept in the background (one `←`
//! away, on [`crate::screens::home::Home`]).
//!
//! # Keys
//!
//! Every printable key goes into the prompt. Nothing a human can type is a global shortcut: the
//! only keys this screen does not treat as text are Enter, the editing/navigation keys, and
//! modifier chords. The screen-level shortcuts are deliberately gated on an *empty* prompt —
//! `/` opens the command popup, `?` toggles help, `←` goes to sessions & tickets — so they are
//! reachable without ever stealing a character from a message being written.
//!
//! Quitting (Ctrl+C twice, which also needs the root app's timing) and Ctrl+T are owned by the
//! application root in `tm-cli`; Ctrl+D on an empty prompt and `/exit` surface here as
//! [`ChatAction::Quit`].
//!
//! # Domain boundary
//!
//! Like every screen in this crate, `ChatScreen` runs nothing. Submitting records a
//! [`ChatAction`] for the application to take ([`ChatScreen::take_actions`]); the turn's progress
//! comes back as [`TurnUpdate`]s inside [`crate::event::AppMessage::Turn`].

use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};
use tm_types::{SessionId, Timestamp};

use crate::chat::commands::{self, Arg, CommandId, Parsed};
use crate::chat::glyphs::Glyphs;
use crate::chat::input::InputBox;
use crate::chat::lines::{
    clear, draw_box, draw_line, draw_spans, truncate_spans, wrap_with_prefix, Line, Span,
};
use crate::chat::status::{self, Hint, StatusInfo, TurnState};
use crate::chat::transcript::{Entry, NoticeLevel, Transcript};
use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{AppMessage, Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::display_width;
use crate::theme::{MotionPolicy, Spinner, Theme};

/// What the human asked the application to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatAction {
    /// Run a turn with this prompt. The screen has already echoed it and marked a turn running.
    Send(String),
    /// Run a slash command (every command except `/help`, which the screen handles itself).
    Command {
        /// Which command.
        id: CommandId,
        /// Its argument, trimmed (empty for commands that take none).
        arg: String,
    },
    /// Show sessions & tickets.
    GoHome,
    /// Quit tm.
    Quit,
}

/// Progress of a running turn, sent by the application as it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnUpdate {
    /// Everything the turn has done so far (cumulative, replacing the previous progress).
    Progress {
        /// The turn's entries so far: assistant text and tool calls, in order.
        entries: Vec<Entry>,
        /// The model that served the most recent step, if known.
        served_by: Option<String>,
        /// Tokens this turn has spent so far.
        tokens: u64,
        /// What the turn is doing now, for the spinner row ("Thinking", "Ran shell.run").
        activity: Option<String>,
    },
    /// The turn ended.
    Finished {
        /// A closing message (a failure summary, a budget stop), if the outcome warrants one.
        notice: Option<(NoticeLevel, String)>,
        /// Whether the turn failed (the status glyph turns red until the next turn).
        failed: bool,
    },
}

/// How long a transient status-bar hint stays up.
const HINT_MILLIS: i64 = 3_000;

/// A message in the status bar's right-hand slot that expires on its own.
#[derive(Debug, Clone)]
struct TransientHint {
    text: String,
    level: NoticeLevel,
    urgent: bool,
    until: Timestamp,
}

/// The running turn's clock and current activity.
#[derive(Debug, Clone)]
struct RunningTurn {
    started: Timestamp,
    activity: String,
}

/// The `/` popup's state.
#[derive(Debug, Default)]
struct Popup {
    selected: usize,
    /// The prompt text the popup was dismissed (Esc) at; it stays closed until the text changes.
    dismissed_at: Option<String>,
}

/// The chat screen.
#[derive(Debug)]
pub struct ChatScreen {
    id: ComponentId,
    session: SessionId,
    transcript: Transcript,
    input: InputBox,
    status: StatusInfo,
    served_by: Option<String>,
    settled_tokens: u64,
    live_tokens: u64,
    turn: Option<RunningTurn>,
    last_turn_failed: bool,
    popup: Popup,
    help_open: bool,
    /// How far the help overlay is scrolled (it is taller than a 24-row terminal).
    help_scroll: std::cell::Cell<usize>,
    hint: Option<TransientHint>,
    actions: VecDeque<ChatAction>,
}

/// The prompt box's placeholder.
pub const PLACEHOLDER: &str = "Ask tm anything… (? for help, / for commands)";
const PLACEHOLDER_ASCII: &str = "Ask tm anything... (? for help, / for commands)";

impl ChatScreen {
    /// An empty conversation for `session`, showing `status`.
    pub fn new(id: ComponentId, session: SessionId, status: StatusInfo) -> Self {
        ChatScreen {
            id,
            session,
            transcript: Transcript::new(),
            input: InputBox::new(),
            status,
            served_by: None,
            settled_tokens: 0,
            live_tokens: 0,
            turn: None,
            last_turn_failed: false,
            popup: Popup::default(),
            help_open: false,
            help_scroll: std::cell::Cell::new(0),
            hint: None,
            actions: VecDeque::new(),
        }
    }

    /// The session this screen's turns belong to.
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The status-bar facts the application owns (cwd, branch, ticket, counts, configured model).
    pub fn status_mut(&mut self) -> &mut StatusInfo {
        &mut self.status
    }

    /// The status-bar facts, as last set.
    pub fn status(&self) -> &StatusInfo {
        &self.status
    }

    /// The transcript, read-only.
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The prompt's current text.
    pub fn input_text(&self) -> &str {
        self.input.text()
    }

    /// Whether a turn is running.
    pub fn is_turn_running(&self) -> bool {
        self.turn.is_some()
    }

    /// Whether the help overlay is open.
    pub fn is_help_open(&self) -> bool {
        self.help_open
    }

    /// Open or close the help overlay.
    pub fn toggle_help(&mut self) {
        self.help_open = !self.help_open;
        self.help_scroll.set(0);
    }

    /// Drain every action recorded since the last call.
    pub fn take_actions(&mut self) -> Vec<ChatAction> {
        self.actions.drain(..).collect()
    }

    /// Add a message from tm to the conversation.
    pub fn push_notice(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.transcript.push(Entry::Notice {
            level,
            text: text.into(),
        });
        self.transcript.follow();
    }

    /// Start a fresh conversation under `session`: the transcript, token count, and failure mark
    /// reset; the prompt's history survives.
    pub fn reset(&mut self, session: SessionId) {
        self.session = session;
        self.transcript.clear();
        self.settled_tokens = 0;
        self.live_tokens = 0;
        self.turn = None;
        self.last_turn_failed = false;
    }

    /// Show `text` in the status bar's hint slot until `until`.
    pub fn show_hint(
        &mut self,
        text: impl Into<String>,
        level: NoticeLevel,
        urgent: bool,
        until: Timestamp,
    ) {
        self.hint = Some(TransientHint {
            text: text.into(),
            level,
            urgent,
            until,
        });
    }

    /// The first Ctrl+C: close any overlay, stash and clear a half-written prompt (Up brings it
    /// back), and warn that a second press quits.
    pub fn interrupt(&mut self, now: Timestamp, window_millis: i64) {
        self.help_open = false;
        if !self.input.is_empty() {
            let text = self.input.text().to_string();
            self.input.remember(&text);
            self.input.clear();
        }
        self.show_hint(
            "Press Ctrl+C again to quit",
            NoticeLevel::Warning,
            true,
            now.plus_millis(window_millis),
        );
    }

    /// Apply a [`TurnUpdate`] for this screen's session.
    pub fn apply_turn_update(&mut self, update: TurnUpdate) {
        match update {
            TurnUpdate::Progress {
                entries,
                served_by,
                tokens,
                activity,
            } => {
                self.transcript.set_live(entries);
                if let Some(model) = served_by {
                    self.served_by = Some(model);
                }
                self.live_tokens = tokens;
                if let (Some(turn), Some(activity)) = (&mut self.turn, activity) {
                    turn.activity = activity;
                }
            }
            TurnUpdate::Finished { notice, failed } => {
                self.transcript.commit_live();
                self.settled_tokens += self.live_tokens;
                self.live_tokens = 0;
                self.turn = None;
                self.last_turn_failed = failed;
                if let Some((level, text)) = notice {
                    self.transcript.push(Entry::Notice { level, text });
                }
            }
        }
    }

    fn popup_query(&self) -> Option<&str> {
        let text = self.input.text();
        let query = text.strip_prefix('/')?;
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        if self.popup.dismissed_at.as_deref() == Some(text) {
            return None;
        }
        Some(query)
    }

    /// Whether the `/` command popup is showing.
    pub fn is_popup_open(&self) -> bool {
        self.popup_query().is_some()
    }

    fn popup_matches(&self) -> Vec<commands::Match> {
        self.popup_query().map(commands::filter).unwrap_or_default()
    }

    /// Put `/name ` (with a trailing space for commands that take an argument) in the prompt.
    fn complete(&mut self, command: &'static commands::SlashCommand) {
        let text = match command.arg {
            Arg::None => format!("/{}", command.name),
            Arg::Required(_) => format!("/{} ", command.name),
        };
        self.input.set_text(text);
        self.popup.selected = 0;
    }

    /// Enter: newline after a trailing backslash, otherwise run a command or send a prompt.
    fn submit(&mut self, now: Timestamp) {
        if self.input.text().ends_with('\\') && self.input.cursor() == self.input.text().len() {
            self.input.backspace();
            self.input.insert("\n");
            return;
        }
        let text = self.input.text().to_string();
        if text.trim().is_empty() {
            return;
        }
        if let Some(parsed) = commands::parse(&text) {
            self.run_parsed(parsed, now);
            return;
        }
        if self.turn.is_some() {
            self.show_hint(
                "Still working — send again when this turn finishes",
                NoticeLevel::Warning,
                true,
                now.plus_millis(HINT_MILLIS),
            );
            return;
        }
        let prompt = self.input.take();
        let prompt = prompt.trim_end().to_string();
        self.transcript.push(Entry::User(prompt.clone()));
        self.transcript.follow();
        self.turn = Some(RunningTurn {
            started: now,
            activity: "Thinking".to_string(),
        });
        self.last_turn_failed = false;
        self.actions.push_back(ChatAction::Send(prompt));
    }

    fn run_parsed(&mut self, parsed: Parsed, now: Timestamp) {
        match parsed {
            Parsed::Known { command, arg } => {
                self.input.take();
                self.popup = Popup::default();
                match command.id {
                    CommandId::Help => self.toggle_help(),
                    CommandId::Home => self.actions.push_back(ChatAction::GoHome),
                    CommandId::Exit => self.actions.push_back(ChatAction::Quit),
                    id => self.actions.push_back(ChatAction::Command { id, arg }),
                }
            }
            Parsed::MissingArg(command) => {
                self.complete(command);
                self.show_hint(
                    format!("usage: {}", commands::usage(command)),
                    NoticeLevel::Info,
                    false,
                    now.plus_millis(HINT_MILLIS),
                );
            }
            Parsed::Unknown(name) => {
                let text = self.input.take();
                self.input.remember(&text);
                self.push_notice(
                    NoticeLevel::Warning,
                    format!("Unknown command /{name}. Type / to see the commands."),
                );
            }
        }
    }

    fn handle_popup_key(&mut self, key: &KeyEvent, now: Timestamp) -> bool {
        let matches = self.popup_matches();
        let count = matches.len();
        match key.code {
            KeyCode::Up if count > 0 => {
                self.popup.selected = (self.popup.selected + count - 1) % count;
                true
            }
            KeyCode::Down if count > 0 => {
                self.popup.selected = (self.popup.selected + 1) % count;
                true
            }
            KeyCode::Tab | KeyCode::BackTab if count > 0 => {
                let chosen = matches[self.popup.selected.min(count - 1)].command;
                self.complete(chosen);
                true
            }
            KeyCode::Enter if key.modifiers.is_empty() && count > 0 => {
                let chosen = matches[self.popup.selected.min(count - 1)].command;
                match chosen.arg {
                    Arg::None => self.run_parsed(
                        Parsed::Known {
                            command: chosen,
                            arg: String::new(),
                        },
                        now,
                    ),
                    Arg::Required(_) => self.run_parsed(Parsed::MissingArg(chosen), now),
                }
                true
            }
            KeyCode::Esc => {
                self.popup.dismissed_at = Some(self.input.text().to_string());
                true
            }
            _ => false,
        }
    }

    fn handle_key(&mut self, key: &KeyEvent, now: Timestamp) {
        if self.help_open {
            let scroll = self.help_scroll.get();
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.help_open = false;
                    return;
                }
                KeyCode::Up => return self.help_scroll.set(scroll.saturating_sub(1)),
                KeyCode::Down => return self.help_scroll.set(scroll + 1),
                KeyCode::PageUp => return self.help_scroll.set(scroll.saturating_sub(10)),
                KeyCode::PageDown => return self.help_scroll.set(scroll + 10),
                // Anything else closes help and then does what it normally does, so starting to
                // type is never blocked by an overlay.
                _ => self.help_open = false,
            }
        }

        if self.is_popup_open() && self.handle_popup_key(key, now) {
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let before = self.input.text().to_string();

        match key.code {
            KeyCode::Enter if shift || alt || ctrl => self.input.insert("\n"),
            KeyCode::Enter => self.submit(now),
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char('w') if ctrl => self.input.delete_word_back(),
            KeyCode::Char('h') if ctrl => self.input.backspace(),
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('b') if ctrl => self.input.left(),
            KeyCode::Char('f') if ctrl => self.input.right(),
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    self.actions.push_back(ChatAction::Quit);
                } else {
                    self.input.delete();
                }
            }
            KeyCode::Char('b') if alt => self.input.word_left(),
            KeyCode::Char('f') if alt => self.input.word_right(),
            // Any other chord is swallowed rather than typed or propagated.
            KeyCode::Char(_) if ctrl || alt => {}
            KeyCode::Char('?') if self.input.is_empty() => self.toggle_help(),
            KeyCode::Char(c) => self.input.insert_char(c),
            KeyCode::Backspace if alt || ctrl => self.input.delete_word_back(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left if self.input.is_empty() => self.actions.push_back(ChatAction::GoHome),
            KeyCode::Left if ctrl || alt => self.input.word_left(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right if ctrl || alt => self.input.word_right(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up if shift => self.transcript.scroll_up(1),
            KeyCode::Down if shift => self.transcript.scroll_down(1),
            KeyCode::Up => {
                if self.input.is_empty() || self.input.on_first_row() {
                    self.input.history_prev();
                } else {
                    self.input.move_vertical(-1);
                }
            }
            KeyCode::Down => {
                if self.input.is_browsing() && self.input.on_last_row() {
                    self.input.history_next();
                } else {
                    self.input.move_vertical(1);
                }
            }
            KeyCode::PageUp => self.transcript.page_up(),
            KeyCode::PageDown => self.transcript.page_down(),
            KeyCode::Esc => self.transcript.follow(),
            _ => {}
        }

        if self.input.text() != before {
            // A new query reopens a dismissed popup and restarts its selection.
            self.popup.selected = 0;
            if self.popup.dismissed_at.as_deref() != Some(self.input.text()) {
                self.popup.dismissed_at = None;
            }
        }
    }

    fn model(&self) -> &str {
        self.served_by.as_deref().unwrap_or(&self.status.model)
    }

    fn turn_state(&self) -> TurnState {
        if self.turn.is_some() {
            TurnState::Running
        } else if self.last_turn_failed {
            TurnState::Failed
        } else {
            TurnState::Idle
        }
    }

    fn spinner_frame(&self, glyphs: &Glyphs, now: Timestamp) -> &'static str {
        let started = self.turn.as_ref().map_or(now, |t| t.started);
        Spinner {
            frames: glyphs.spinner,
            frame_duration: Duration::from_millis(120),
        }
        .frame(started, now, MotionPolicy::enabled())
    }

    /// The spinner row shown under the transcript while a turn runs.
    fn spinner_lines(&self, theme: &Theme, glyphs: &Glyphs, now: Timestamp) -> Vec<Line> {
        let Some(turn) = &self.turn else {
            return Vec::new();
        };
        let elapsed = now.millis_since(turn.started).max(0) / 1000;
        let clock = if elapsed >= 60 {
            format!("{}m {:02}s", elapsed / 60, elapsed % 60)
        } else {
            format!("{elapsed}s")
        };
        let mut facts = clock;
        if self.live_tokens > 0 {
            facts.push_str(&format!(
                "{}{} tokens",
                glyphs.sep,
                status::format_tokens(self.live_tokens)
            ));
        }
        let dots = if glyphs.unicode { "…" } else { "..." };
        let mut rows = Vec::new();
        if !self.transcript.is_empty() {
            rows.push(Line::blank());
        }
        rows.push(Line::from_spans(vec![
            Span::new(
                self.spinner_frame(glyphs, now),
                Style::default().fg(theme.accent),
            ),
            Span::new(" ", Style::default()),
            Span::new(
                format!("{}{dots}", turn.activity),
                Style::default().fg(theme.accent),
            ),
            Span::new(format!("  ({facts})"), Style::default().fg(theme.muted)),
        ]));
        rows
    }

    fn current_hint(&self, theme: &Theme, glyphs: &Glyphs, now: Timestamp) -> Option<Hint> {
        if let Some(hint) = &self.hint {
            if now.millis_since(hint.until) < 0 {
                let color = match hint.level {
                    NoticeLevel::Info => theme.muted,
                    NoticeLevel::Success => theme.success,
                    NoticeLevel::Warning => theme.warning,
                    NoticeLevel::Error => theme.danger,
                };
                return Some(Hint {
                    span: Span::new(hint.text.clone(), Style::default().fg(color)),
                    urgent: hint.urgent,
                });
            }
        }
        let muted = Style::default().fg(theme.muted);
        let text = if self.is_popup_open() {
            format!(
                "{} select{}tab complete{}enter run{}esc close",
                glyphs.updown, glyphs.sep, glyphs.sep, glyphs.sep
            )
        } else if !self.transcript.is_following() {
            format!("pgup/pgdn scroll{}esc jump to latest", glyphs.sep)
        } else if self.input.is_empty() && self.turn.is_none() {
            "? for shortcuts".to_string()
        } else {
            return None;
        };
        Some(Hint {
            span: Span::new(text, muted),
            urgent: false,
        })
    }

    fn render_welcome(&self, area: Rect, buf: &mut Buffer, theme: &Theme, glyphs: &Glyphs) {
        if area.height < 3 || area.width < 8 {
            return;
        }
        let muted = Style::default().fg(theme.muted);
        let accent = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let mut place = self.status.cwd.clone();
        if let Some(branch) = &self.status.branch {
            place.push_str(&format!(" ({branch})"));
        }
        let hints = vec![
            Span::new("?", accent),
            Span::new(" help", muted),
            Span::new(glyphs.sep, muted),
            Span::new("/", accent),
            Span::new(" commands", muted),
            Span::new(glyphs.sep, muted),
            Span::new(glyphs.left, accent),
            Span::new(" sessions & tickets", muted),
        ];
        let hints_width: usize = hints.iter().map(|s| display_width(&s.text)).sum();

        let facts = [
            vec![Span::new(self.model().to_string(), Style::default())],
            vec![Span::new(place, muted)],
        ];
        let (mark_rows, mark_width): (Vec<&str>, usize) = if glyphs.unicode {
            (vec!["◆ tm", ""], 4)
        } else {
            (vec!["tm", ""], 2)
        };

        let content_width = (mark_width
            + 3
            + facts
                .iter()
                .map(|f| display_width(&f[0].text))
                .max()
                .unwrap_or(0))
        .max(hints_width);
        let box_width = (content_width + 6).min(area.width as usize) as u16;
        let box_height = 7u16.min(area.height);
        let outer = Rect::new(area.x, area.y, box_width, box_height);
        draw_box(buf, outer, &glyphs.border, muted);
        let inner_x = outer.x + 3;
        let inner_width = box_width.saturating_sub(6);
        if box_height < 7 {
            // Too short for the full card: just the facts line.
            let line = Line::from_spans(vec![
                Span::new("tm ", accent),
                Span::new(self.model().to_string(), Style::default()),
            ]);
            draw_line(buf, inner_x, outer.y + 1, inner_width, &line);
            return;
        }
        let text_x = inner_x + mark_width as u16 + 3;
        let text_width = inner_width.saturating_sub(mark_width as u16 + 3);
        for (i, mark) in mark_rows.iter().enumerate() {
            let y = outer.y + 2 + i as u16;
            buf.set_stringn(inner_x, y, mark, inner_width as usize, accent);
            draw_spans(
                buf,
                text_x,
                y,
                text_width,
                &truncate_spans(&facts[i], text_width as usize, glyphs.ellipsis),
            );
        }
        draw_spans(
            buf,
            inner_x,
            outer.y + 5,
            inner_width,
            &truncate_spans(&hints, inner_width as usize, glyphs.ellipsis),
        );
    }

    fn render_popup(
        &self,
        input_area: Rect,
        bounds: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        glyphs: &Glyphs,
    ) {
        let matches = self.popup_matches();
        let shown = matches.len().clamp(1, 8);
        let height = shown as u16 + 2;
        if input_area.y < bounds.y + height || bounds.width < 20 {
            return;
        }
        let width = bounds.width.min(76);
        let area = Rect::new(input_area.x, input_area.y - height, width, height);
        clear(buf, area);
        let muted = Style::default().fg(theme.muted);
        draw_box(buf, area, &glyphs.border, muted);
        let inner_x = area.x + 2;
        let inner_width = width.saturating_sub(4) as usize;

        if matches.is_empty() {
            buf.set_stringn(
                inner_x,
                area.y + 1,
                "No matching commands",
                inner_width,
                muted,
            );
            return;
        }
        let selected = self.popup.selected.min(matches.len() - 1);
        let first = selected.saturating_sub(shown - 1);
        let name_width = matches
            .iter()
            .map(|m| display_width(&commands::usage(m.command)))
            .max()
            .unwrap_or(0);
        for (row, (i, m)) in matches
            .iter()
            .enumerate()
            .skip(first)
            .take(shown)
            .enumerate()
        {
            let y = area.y + 1 + row as u16;
            let is_selected = i == selected;
            let usage = commands::usage(m.command);
            let pad = " ".repeat(name_width.saturating_sub(display_width(&usage)) + 3);
            let mut description = m.command.description.to_string();
            if let Some(alias) = m.via_alias {
                description.push_str(&format!("{}/{alias}", glyphs.sep));
            }
            let (pointer, name_style) = if is_selected {
                (
                    glyphs.pointer,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (" ", Style::default())
            };
            let spans = vec![
                Span::new(pointer, Style::default().fg(theme.accent)),
                Span::new(" ", Style::default()),
                Span::new(usage, name_style),
                Span::new(pad, Style::default()),
                Span::new(
                    description,
                    if is_selected { Style::default() } else { muted },
                ),
            ];
            draw_spans(
                buf,
                inner_x,
                y,
                inner_width as u16,
                &truncate_spans(&spans, inner_width, glyphs.ellipsis),
            );
        }
    }

    fn render_help(&self, area: Rect, buf: &mut Buffer, theme: &Theme, glyphs: &Glyphs) {
        let width = area.width.saturating_sub(4).min(80);
        let lines = help_lines(theme, glyphs, width.saturating_sub(4) as usize);
        let height = (lines.len() as u16 + 4).min(area.height.saturating_sub(1));
        if width < 24 || height < 5 {
            return;
        }
        let outer = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        clear(buf, outer);
        draw_box(
            buf,
            outer,
            &glyphs.border,
            Style::default().fg(theme.accent),
        );
        let title = " tm help ";
        buf.set_string(
            outer.x + 2,
            outer.y,
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        );
        let footer = " esc to close ";
        let footer_x = outer.x + outer.width.saturating_sub(display_width(footer) as u16 + 2);
        buf.set_string(
            footer_x,
            outer.y + outer.height - 1,
            footer,
            Style::default().fg(theme.muted),
        );
        let inner_width = width.saturating_sub(4);
        let visible = (height - 4) as usize;
        let max_scroll = lines.len().saturating_sub(visible);
        let scroll = self.help_scroll.get().min(max_scroll);
        self.help_scroll.set(scroll);
        for (row, line) in lines.iter().skip(scroll).take(visible).enumerate() {
            let y = outer.y + 2 + row as u16;
            let spans = truncate_spans(&line.spans, inner_width as usize, glyphs.ellipsis);
            draw_spans(buf, outer.x + 2, y, inner_width, &spans);
        }
        if max_scroll > 0 {
            let more = if scroll < max_scroll {
                format!(" {} more ", glyphs.updown)
            } else {
                format!(" {} ", glyphs.updown)
            };
            buf.set_string(
                outer.x + 2,
                outer.y + outer.height - 1,
                more,
                Style::default().fg(theme.muted),
            );
        }
    }
}

/// The help overlay's content.
fn help_lines(theme: &Theme, glyphs: &Glyphs, width: usize) -> Vec<Line> {
    let heading = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let key = Style::default().add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted);
    let row = |k: &str, d: &str| {
        let pad = " ".repeat(23usize.saturating_sub(display_width(k)).max(2));
        Line::from_spans(vec![
            Span::new(format!("  {k}"), key),
            Span::new(pad, Style::default()),
            Span::new(d.to_string(), muted),
        ])
    };
    let left = glyphs.left;
    let mut lines = vec![
        Line::plain("Keys", heading),
        row("enter", "send the message"),
        row("shift+enter  ctrl+j", "new line (or end a line with \\)"),
        row(
            &format!("{}  (first/last line)", glyphs.updown),
            "prompt history",
        ),
        row("pgup  pgdn  wheel", "scroll the conversation"),
        row(
            &format!("{left}  ctrl+t"),
            "sessions & tickets (empty prompt)",
        ),
        row("/", "commands (empty prompt)"),
        row("?", "this help (empty prompt)"),
        row("ctrl+u  ctrl+w", "clear the prompt / delete a word"),
        row("ctrl+c twice  ctrl+d", "quit"),
        Line::blank(),
        Line::plain("Commands", heading),
    ];
    for command in commands::COMMANDS {
        let mut names = commands::usage(command);
        for alias in command.aliases {
            names.push_str(&format!(" /{alias}"));
        }
        lines.push(row(&names, command.description));
    }
    lines.push(Line::blank());
    lines.push(Line::plain("Sessions & tickets", heading));
    for text in [
        "This conversation is a session: it runs here, in the foreground, and never creates a \
         ticket just because you asked something.",
        "Tickets are background work. The agent opens one when a job deserves tracking, and \
         workers (tm sched run) execute them while you keep chatting.",
        "/attach links this chat to a ticket; ← (or ctrl+t) shows every session and ticket.",
    ] {
        let text = if glyphs.unicode {
            text.to_string()
        } else {
            text.replace('←', "<-")
        };
        lines.extend(wrap_with_prefix(
            &[Span::new(text, muted)],
            width,
            vec![Span::new("  ", Style::default())],
            "  ",
        ));
    }
    lines
}

impl Component for ChatScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width < 4 || area.height == 0 {
            return;
        }
        let theme = ctx.theme;
        let glyphs = Glyphs::for_caps(ctx.caps);
        let now = ctx.clock.now();

        let status_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        let rest = Rect::new(area.x, area.y, area.width, area.height - 1);

        let max_input = (rest.height / 3).clamp(3, 10);
        let input_height = self.input.height(rest.width, max_input).min(rest.height);
        let input_area = Rect::new(
            rest.x,
            rest.y + rest.height - input_height,
            rest.width,
            input_height,
        );
        let transcript_area = Rect::new(
            rest.x + 1,
            rest.y,
            rest.width.saturating_sub(2),
            rest.height - input_height,
        );

        if transcript_area.height > 0 {
            if self.transcript.is_empty() && self.turn.is_none() {
                self.render_welcome(transcript_area, buf, theme, &glyphs);
            } else {
                let tail = self.spinner_lines(theme, &glyphs, now);
                self.transcript
                    .render(transcript_area, buf, theme, &glyphs, &tail);
            }
        }

        let placeholder = if glyphs.unicode {
            PLACEHOLDER
        } else {
            PLACEHOLDER_ASCII
        };
        self.input.render(
            input_area,
            buf,
            theme,
            &glyphs,
            placeholder,
            self.turn.is_some(),
        );

        let segments = status::segments(
            &StatusInfo {
                model: self.model().to_string(),
                tokens: self.settled_tokens + self.live_tokens,
                ..self.status.clone()
            },
            self.turn_state(),
            self.spinner_frame(&glyphs, now),
            theme,
            &glyphs,
        );
        let hint = self.current_hint(theme, &glyphs, now);
        status::render(buf, status_area, &segments, hint.as_ref(), theme, &glyphs);

        if self.is_popup_open() {
            self.render_popup(input_area, rest, buf, theme, &glyphs);
        }
        if self.help_open {
            self.render_help(rest, buf, theme, &glyphs);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        let now = ctx.clock.now();
        match event {
            Event::App(AppMessage::Turn { session, update }) => {
                if session != &self.session {
                    return Propagation::Propagate;
                }
                self.apply_turn_update(update.clone());
                Propagation::Consumed
            }
            Event::Input(InputEvent::Key(key)) => {
                if key.kind == crossterm::event::KeyEventKind::Release {
                    return Propagation::Consumed;
                }
                self.handle_key(key, now);
                Propagation::Consumed
            }
            Event::Input(InputEvent::Paste(text)) => {
                self.help_open = false;
                self.input.insert(text);
                Propagation::Consumed
            }
            Event::Input(InputEvent::Mouse(mouse)) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.transcript.scroll_up(3);
                    Propagation::Consumed
                }
                MouseEventKind::ScrollDown => {
                    self.transcript.scroll_down(3);
                    Propagation::Consumed
                }
                _ => Propagation::Propagate,
            },
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, _ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(KeyCode::Enter), "send"),
            KeyBinding::new(
                KeyChord {
                    code: KeyCode::Char('j'),
                    modifiers: KeyModifiers::CONTROL,
                },
                "new line",
            ),
            KeyBinding::new(KeyChord::plain(KeyCode::PageUp), "scroll up"),
            KeyBinding::new(KeyChord::plain(KeyCode::PageDown), "scroll down"),
            KeyBinding::new(
                KeyChord {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                },
                "clear the prompt",
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{Capabilities, ColorSupport, UnicodeSupport};
    use crate::chat::transcript::{ToolCallView, ToolStatus};
    use crate::component::FocusState;
    use tm_types::{Clock, FixedClock};

    fn screen() -> ChatScreen {
        ChatScreen::new(
            ComponentId::new("test.chat"),
            SessionId::new("S-1").expect("S-1 is a valid SessionId"),
            StatusInfo {
                model: "mock/m1".to_string(),
                cwd: "~/proj".to_string(),
                branch: Some("main".to_string()),
                ..StatusInfo::default()
            },
        )
    }

    fn caps() -> Capabilities {
        Capabilities {
            color: ColorSupport::TrueColor,
            unicode: UnicodeSupport::NarrowOnly,
            ..Capabilities::minimal()
        }
    }

    struct Env {
        theme: Theme,
        caps: Capabilities,
        clock: FixedClock,
    }

    impl Env {
        fn new() -> Self {
            Env {
                theme: Theme::dark(),
                caps: caps(),
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

    fn key(code: KeyCode) -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn chord(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(code, modifiers)))
    }

    fn type_text(chat: &mut ChatScreen, env: &Env, text: &str) {
        for c in text.chars() {
            chat.handle_event(&key(KeyCode::Char(c)), &env.ctx());
        }
    }

    fn render(chat: &ChatScreen, env: &Env, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        chat.render(area, &mut buf, &env.ctx());
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

    fn screen_text(rows: &[String]) -> String {
        rows.join("\n")
    }

    #[test]
    fn typing_q_and_other_letters_only_ever_edits_the_prompt() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "quit? q! t/");
        assert_eq!(chat.input_text(), "quit? q! t/");
        assert!(chat.take_actions().is_empty());
        assert!(!chat.is_help_open());
    }

    #[test]
    fn enter_sends_echoes_and_marks_the_turn_running() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "fix the flaky test");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        assert_eq!(
            chat.take_actions(),
            vec![ChatAction::Send("fix the flaky test".to_string())]
        );
        assert!(chat.is_turn_running());
        assert_eq!(chat.input_text(), "");
        let rows = render(&chat, &env, 80, 24);
        assert!(screen_text(&rows).contains("› fix the flaky test"));
        assert!(screen_text(&rows).contains("Thinking…"), "{rows:#?}");
    }

    #[test]
    fn a_second_submit_mid_turn_is_refused_and_keeps_the_text() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "one");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        chat.take_actions();
        type_text(&mut chat, &env, "two");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        assert!(chat.take_actions().is_empty());
        assert_eq!(chat.input_text(), "two");
        let rows = render(&chat, &env, 100, 24);
        assert!(screen_text(&rows).contains("Still working"), "{rows:#?}");
    }

    #[test]
    fn newline_chords_insert_instead_of_sending() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "a");
        chat.handle_event(&chord(KeyCode::Enter, KeyModifiers::SHIFT), &env.ctx());
        type_text(&mut chat, &env, "b");
        chat.handle_event(
            &chord(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &env.ctx(),
        );
        type_text(&mut chat, &env, "c\\");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        assert_eq!(chat.input_text(), "a\nb\nc\n");
        assert!(chat.take_actions().is_empty());
    }

    #[test]
    fn slash_opens_the_popup_and_filtering_then_enter_runs_the_command() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "/");
        assert!(chat.is_popup_open());
        let rows = render(&chat, &env, 80, 24);
        let text = screen_text(&rows);
        assert!(
            text.contains("/help") && text.contains("/attach <ticket>"),
            "{text}"
        );

        type_text(&mut chat, &env, "cl");
        let text = screen_text(&render(&chat, &env, 80, 24));
        assert!(text.contains("/clear"));
        assert!(!text.contains("/attach"), "{text}");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        assert_eq!(
            chat.take_actions(),
            vec![ChatAction::Command {
                id: CommandId::Clear,
                arg: String::new()
            }]
        );
        assert!(!chat.is_popup_open());
    }

    #[test]
    fn popup_arrows_select_and_tab_completes_argument_commands() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "/a");
        // "attach" (name prefix) ranks before "home" (alias "agents").
        chat.handle_event(&key(KeyCode::Tab), &env.ctx());
        assert_eq!(chat.input_text(), "/attach ");
        assert!(!chat.is_popup_open());
        type_text(&mut chat, &env, "T-4");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        assert_eq!(
            chat.take_actions(),
            vec![ChatAction::Command {
                id: CommandId::Attach,
                arg: "T-4".to_string()
            }]
        );

        type_text(&mut chat, &env, "/");
        chat.handle_event(&key(KeyCode::Down), &env.ctx());
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        assert_eq!(chat.take_actions(), vec![ChatAction::GoHome]);
    }

    #[test]
    fn esc_dismisses_the_popup_until_the_query_changes() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "/he");
        chat.handle_event(&key(KeyCode::Esc), &env.ctx());
        assert!(!chat.is_popup_open());
        assert_eq!(chat.input_text(), "/he");
        type_text(&mut chat, &env, "l");
        assert!(chat.is_popup_open());
    }

    #[test]
    fn question_mark_on_an_empty_prompt_toggles_help() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "?");
        assert!(chat.is_help_open());
        let text = screen_text(&render(&chat, &env, 100, 40));
        assert!(text.contains("tm help"));
        assert!(text.contains("Sessions & tickets"));
        chat.handle_event(&key(KeyCode::Esc), &env.ctx());
        assert!(!chat.is_help_open());
        assert_eq!(chat.input_text(), "", "? on an empty prompt is not typed");
    }

    #[test]
    fn left_on_an_empty_prompt_goes_home_but_moves_the_cursor_otherwise() {
        let env = Env::new();
        let mut chat = screen();
        chat.handle_event(&key(KeyCode::Left), &env.ctx());
        assert_eq!(chat.take_actions(), vec![ChatAction::GoHome]);
        type_text(&mut chat, &env, "ab");
        chat.handle_event(&key(KeyCode::Left), &env.ctx());
        assert!(chat.take_actions().is_empty());
    }

    #[test]
    fn ctrl_d_quits_only_on_an_empty_prompt() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "x");
        chat.handle_event(
            &chord(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &env.ctx(),
        );
        assert!(chat.take_actions().is_empty());
        assert_eq!(chat.input_text(), "x");
        chat.handle_event(
            &chord(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &env.ctx(),
        );
        chat.handle_event(
            &chord(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &env.ctx(),
        );
        assert_eq!(chat.take_actions(), vec![ChatAction::Quit]);
    }

    #[test]
    fn unknown_commands_are_reported_not_sent() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "/frobnicate");
        chat.handle_event(&key(KeyCode::Esc), &env.ctx());
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        assert!(chat.take_actions().is_empty());
        let text = screen_text(&render(&chat, &env, 80, 24));
        assert!(text.contains("Unknown command /frobnicate"), "{text}");
    }

    #[test]
    fn a_path_like_message_is_sent_not_parsed_as_a_command() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "/usr/bin is missing python");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        assert_eq!(
            chat.take_actions(),
            vec![ChatAction::Send("/usr/bin is missing python".to_string())]
        );
    }

    #[test]
    fn turn_progress_renders_tools_and_finishing_settles_tokens() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "run the tests");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        let session = chat.session().clone();
        let progress = TurnUpdate::Progress {
            entries: vec![
                Entry::Tool(ToolCallView {
                    status: ToolStatus::Ok,
                    name: "shell.run".to_string(),
                    target: "python3 -m pytest -q".to_string(),
                    detail: Some("exit 0".to_string()),
                    preview: Vec::new(),
                }),
                Entry::Assistant("All **green**.".to_string()),
            ],
            served_by: Some("devpass/muse".to_string()),
            tokens: 1_500,
            activity: Some("Ran shell.run".to_string()),
        };
        chat.handle_event(
            &Event::App(AppMessage::Turn {
                session: session.clone(),
                update: progress,
            }),
            &env.ctx(),
        );
        let text = screen_text(&render(&chat, &env, 100, 24));
        assert!(
            text.contains("✓ shell.run  python3 -m pytest -q  (exit 0)"),
            "{text}"
        );
        assert!(text.contains("● All green."), "{text}");
        assert!(text.contains("Ran shell.run…"), "{text}");
        assert!(text.contains("devpass/muse"), "{text}");

        chat.handle_event(
            &Event::App(AppMessage::Turn {
                session,
                update: TurnUpdate::Finished {
                    notice: None,
                    failed: false,
                },
            }),
            &env.ctx(),
        );
        assert!(!chat.is_turn_running());
        let text = screen_text(&render(&chat, &env, 100, 24));
        assert!(text.contains("1.5k tokens"), "{text}");
        assert!(!text.contains("Ran shell.run…"));
    }

    #[test]
    fn updates_for_another_session_are_ignored() {
        let env = Env::new();
        let mut chat = screen();
        let propagation = chat.handle_event(
            &Event::App(AppMessage::Turn {
                session: SessionId::new("S-99").expect("valid"),
                update: TurnUpdate::Finished {
                    notice: Some((NoticeLevel::Error, "stale".to_string())),
                    failed: true,
                },
            }),
            &env.ctx(),
        );
        assert_eq!(propagation, Propagation::Propagate);
        assert!(chat.transcript().is_empty());
    }

    #[test]
    fn the_welcome_card_shows_model_place_and_hints() {
        let env = Env::new();
        let chat = screen();
        let text = screen_text(&render(&chat, &env, 80, 24));
        assert!(text.contains("mock/m1"), "{text}");
        assert!(text.contains("~/proj (main)"));
        assert!(text.contains("? help"));
        assert!(text.contains("/ commands"));
        assert!(text.contains("← sessions & tickets"));
        assert!(text.contains("Ask tm anything"));
    }

    #[test]
    fn the_status_bar_shows_the_model_and_no_ticket() {
        let env = Env::new();
        let chat = screen();
        let rows = render(&chat, &env, 100, 24);
        let status = &rows[23];
        assert!(status.contains("mock/m1"), "{status}");
        assert!(status.contains("no ticket"), "{status}");
        assert!(status.contains("? for shortcuts"), "{status}");
    }

    #[test]
    fn interrupt_stashes_the_prompt_and_warns() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "half written");
        chat.interrupt(env.clock.now(), 1_000);
        assert_eq!(chat.input_text(), "");
        let rows = render(&chat, &env, 100, 24);
        assert!(
            rows[23].contains("Press Ctrl+C again to quit"),
            "{}",
            rows[23]
        );
        chat.handle_event(&key(KeyCode::Up), &env.ctx());
        assert_eq!(chat.input_text(), "half written");
    }

    #[test]
    fn it_degrades_to_ascii_and_small_terminals_without_panicking() {
        let mut env = Env::new();
        env.caps = Capabilities::minimal();
        let mut chat = screen();
        type_text(&mut chat, &env, "hello");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        for (w, h) in [
            (80u16, 24u16),
            (40, 12),
            (20, 6),
            (10, 3),
            (4, 1),
            (200, 60),
        ] {
            let rows = render(&chat, &env, w, h);
            assert_eq!(rows.len(), h as usize);
            for row in &rows {
                assert!(row.is_ascii(), "{w}x{h}: non-ASCII in {row:?}");
            }
        }
        type_text(&mut chat, &env, "?");
        let _ = render(&chat, &env, 30, 10);
        type_text(&mut chat, &env, "/");
        let _ = render(&chat, &env, 30, 10);
    }

    #[test]
    fn paste_inserts_multiline_text_without_sending() {
        let env = Env::new();
        let mut chat = screen();
        chat.handle_event(
            &Event::Input(InputEvent::Paste("line one\r\nline two".to_string())),
            &env.ctx(),
        );
        assert_eq!(chat.input_text(), "line one\nline two");
        assert!(chat.take_actions().is_empty());
    }

    #[test]
    fn reset_clears_the_conversation_but_keeps_history() {
        let env = Env::new();
        let mut chat = screen();
        type_text(&mut chat, &env, "remember me");
        chat.handle_event(&key(KeyCode::Enter), &env.ctx());
        chat.reset(SessionId::new("S-2").expect("valid"));
        assert!(chat.transcript().is_empty());
        assert!(!chat.is_turn_running());
        chat.handle_event(&key(KeyCode::Up), &env.ctx());
        assert_eq!(chat.input_text(), "remember me");
    }
}
