//! A visual check for the chat screen: the real `ChatScreen` in the real `Runtime`, fed a
//! scripted turn (tool calls, a failing command, Markdown with code), so the transcript's
//! rendering can be looked at in a terminal without a model or a project.
//!
//! `cargo run -p tm-tui --example chat_demo` — type anything and press Enter to play the script;
//! Ctrl+C quits.

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_tui::chat::status::StatusInfo;
use tm_tui::chat::transcript::{Entry, NoticeLevel, ToolCallView, ToolStatus};
use tm_tui::component::{Component, ComponentId, ComponentParent, FrameContext};
use tm_tui::event::{AppMessage, Event, InputEvent, Propagation};
use tm_tui::runtime::{MessageSender, Runtime};
use tm_tui::screens::chat::{ChatAction, ChatScreen, TurnUpdate};
use tm_tui::theme::Theme;
use tm_types::{SessionId, SystemClock};
use tokio::sync::Notify;

#[derive(Debug)]
struct Demo {
    chat: ChatScreen,
    shutdown: Arc<Notify>,
    sender: MessageSender,
}

fn tool(status: ToolStatus, name: &str, target: &str, detail: &str) -> Entry {
    Entry::Tool(ToolCallView {
        status,
        name: name.to_string(),
        target: target.to_string(),
        detail: (!detail.is_empty()).then(|| detail.to_string()),
        preview: Vec::new(),
    })
}

fn script() -> Vec<Vec<Entry>> {
    let mut steps: Vec<Vec<Entry>> = Vec::new();
    let mut so_far = vec![
        Entry::Assistant("Let me look at the division code and run the tests.".to_string()),
        tool(ToolStatus::Ok, "fs.read", "calc.py", ""),
        tool(
            ToolStatus::Ok,
            "search.hybrid",
            "\"divide by zero\"",
            "3 results",
        ),
    ];
    steps.push(so_far.clone());
    so_far.push(Entry::Tool(ToolCallView {
        status: ToolStatus::Failed,
        name: "test.run".to_string(),
        target: "python3 -m pytest -q".to_string(),
        detail: Some("exit 1".to_string()),
        preview: vec![
            "FAILED tests/test_calc.py::test_div_zero - ZeroDivisionError".to_string(),
            "\x1b[31m1 failed\x1b[0m, 4 passed in 0.03s".to_string(),
        ],
    }));
    steps.push(so_far.clone());
    so_far.push(tool(
        ToolStatus::Failed,
        "edit.apply_patch",
        "calc.py",
        "patch did not apply: context mismatch at line 2",
    ));
    so_far.push(tool(ToolStatus::Ok, "edit.write_file", "calc.py", ""));
    so_far.push(tool(
        ToolStatus::Ok,
        "shell.run",
        "python3 -m pytest -q",
        "exit 0",
    ));
    so_far.push(tool(
        ToolStatus::Denied,
        "shell.run",
        "git push origin main",
        "network access needs approval",
    ));
    steps.push(so_far.clone());
    so_far.push(Entry::Assistant(
        "## Fixed\n\n`div` now raises a **clear error** instead of crashing:\n\n```python\ndef div(a, b):\n    if b == 0:\n        raise ValueError(\"cannot divide by zero\")\n    return a / b\n```\n\n- All **5 tests** pass (`pytest -q`)\n- I did *not* push; that needs your approval\n\n> Tip: `/attach T-1` to track this as ticket work."
            .to_string(),
    ));
    steps.push(so_far);
    steps
}

impl Demo {
    fn play(&self) {
        let sender = self.sender.clone();
        let session = self.chat.session().clone();
        tokio::spawn(async move {
            for (i, entries) in script().into_iter().enumerate() {
                tokio::time::sleep(Duration::from_millis(900)).await;
                sender.send(AppMessage::Turn {
                    session: session.clone(),
                    update: TurnUpdate::Progress {
                        entries,
                        served_by: Some("devpass/muse-spark-1.3".to_string()),
                        tokens: 4_200 * (i as u64 + 1),
                        activity: Some("Thinking".to_string()),
                    },
                });
            }
            sender.send(AppMessage::Turn {
                session,
                update: TurnUpdate::Finished {
                    notice: None,
                    failed: false,
                },
            });
        });
    }
}

impl Component for Demo {
    fn id(&self) -> ComponentId {
        ComponentId::new("demo")
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        self.chat.render(area, buf, ctx);
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if let Event::Input(InputEvent::Key(key)) = event {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.shutdown.notify_one();
                return Propagation::Consumed;
            }
        }
        let propagation = self.chat.handle_event(event, ctx);
        for action in self.chat.take_actions() {
            match action {
                ChatAction::Send(_) => self.play(),
                ChatAction::Quit => self.shutdown.notify_one(),
                ChatAction::GoHome | ChatAction::Command { .. } => self
                    .chat
                    .push_notice(NoticeLevel::Info, "Not wired in this demo."),
            }
        }
        propagation
    }
}

impl ComponentParent for Demo {
    fn resolve(&self, id: ComponentId) -> Option<&dyn Component> {
        (id == self.id()).then_some(self as &dyn Component)
    }

    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if id == self.id() {
            Some(self)
        } else {
            None
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut runtime, sender) = Runtime::start(Arc::new(SystemClock), Theme::dark()).await?;
    let mut demo = Demo {
        chat: ChatScreen::new(
            ComponentId::new("demo.chat"),
            SessionId::new("S-1")?,
            StatusInfo {
                model: "devpass/muse-spark-1.3".to_string(),
                cwd: "~/src/calc".to_string(),
                branch: Some("main".to_string()),
                open_tickets: 3,
                ..StatusInfo::default()
            },
        ),
        shutdown: runtime.shutdown_handle(),
        sender,
    };
    runtime.run(&mut demo).await?;
    Ok(())
}
