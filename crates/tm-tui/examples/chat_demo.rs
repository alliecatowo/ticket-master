//! A visual check for the chat screen: the real `ChatScreen` in the real `Runtime`, fed a
//! scripted turn so the transcript's Claude-Code-style rendering can be looked at in a terminal
//! without a model or a project: `Bash` with short, long and failing output, `Read`, `Update` with
//! an inline diff, `Write`, a denial, an edit error, Markdown, and a permission prompt.
//!
//! `cargo run -p tm-tui --example chat_demo` — type anything and press Enter to play the script;
//! `!cmd` fakes a shell command; Ctrl+O opens the transcript viewer; Ctrl+C twice quits.

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_tui::chat::approval::ApprovalRequest;
use tm_tui::chat::diff::parse_unified;
use tm_tui::chat::mention::FileIndex;
use tm_tui::chat::status::StatusInfo;
use tm_tui::chat::transcript::{Entry, NoticeLevel, ToolBody, ToolCallView, ToolStatus};
use tm_tui::component::{Component, ComponentId, ComponentParent, FrameContext};
use tm_tui::event::{AppMessage, Event, InputEvent, Propagation};
use tm_tui::runtime::{MessageSender, Runtime};
use tm_tui::screens::chat::{ChatAction, ChatScreen, TurnUpdate};
use tm_tui::theme::Theme;
use tm_types::{Clock, SessionId, SystemClock, Timestamp};
use tokio::sync::Notify;

#[derive(Debug)]
struct Demo {
    chat: ChatScreen,
    shutdown: Arc<Notify>,
    sender: MessageSender,
    ctrl_c_at: Option<Timestamp>,
}

fn tool(status: ToolStatus, name: &str, target: &str, body: ToolBody) -> Entry {
    Entry::Tool(ToolCallView {
        body,
        ..ToolCallView::new(status, name, target)
    })
}

const CALC_DIFF: &str = "--- a/calc.py\n+++ b/calc.py\n@@ -8,6 +8,8 @@ def mul(a, b):\n     return a * b\n \n \n def div(a, b):\n-    return a / b\n+    if b == 0:\n+        raise ValueError(\"cannot divide by zero\")\n+    return a / b\n \n \n";

fn script() -> Vec<Vec<Entry>> {
    let mut steps: Vec<Vec<Entry>> = Vec::new();
    let mut so_far = vec![
        Entry::Assistant("Let me look at the division code and run the tests.".to_string()),
        tool(
            ToolStatus::Ok,
            "fs.read",
            "calc.py",
            ToolBody::Summary("Read 42 lines".to_string()),
        ),
        tool(
            ToolStatus::Ok,
            "search.hybrid",
            "\"divide by zero\"",
            ToolBody::Summary("Found 3 results".to_string()),
        ),
    ];
    steps.push(so_far.clone());
    so_far.push(Entry::Tool(ToolCallView::command(
        "test.run",
        "python3 -m pytest -q",
        1,
        "",
        "..F..\n=== FAILURES ===\n___ test_div_zero ___\n    def test_div_zero():\n>       div(1, 0)\nE       ZeroDivisionError: division by zero\nFAILED tests/test_calc.py::test_div_zero - ZeroDivisionError\n\x1b[31m1 failed\x1b[0m, 4 passed in 0.03s\n",
    )));
    steps.push(so_far.clone());
    so_far.push(Entry::Tool(ToolCallView {
        detail: Some("patch did not apply: context mismatch at line 2".to_string()),
        ..ToolCallView::new(ToolStatus::Failed, "edit.apply_patch", "calc.py")
    }));
    let lines = parse_unified(CALC_DIFF);
    so_far.push(tool(
        ToolStatus::Ok,
        "edit.apply_patch",
        "calc.py",
        ToolBody::Diff {
            summary: "Updated calc.py with 3 additions and 1 removal".to_string(),
            lines,
        },
    ));
    so_far.push(tool(
        ToolStatus::Ok,
        "edit.create_file",
        "tests/test_zero.py",
        ToolBody::Diff {
            summary: "Wrote 4 lines to tests/test_zero.py".to_string(),
            lines: parse_unified(
                "@@ -0,0 +1,4 @@\n+import pytest\n+from calc import div\n+def test_zero():\n+    pytest.raises(ValueError, div, 1, 0)\n",
            ),
        },
    ));
    let long: String = (1..=40)
        .map(|i| format!("tests/test_calc.py::test_case_{i:02} PASSED\n"))
        .collect();
    so_far.push(Entry::Tool(ToolCallView::command(
        "shell.run",
        "python3 -m pytest -v",
        0,
        &long,
        "",
    )));
    so_far.push(Entry::Tool(ToolCallView {
        detail: Some("network access needs approval".to_string()),
        ..ToolCallView::new(ToolStatus::Denied, "shell.run", "git push origin main")
    }));
    steps.push(so_far.clone());
    so_far.push(Entry::Assistant(
        "## Fixed\n\n`div` now raises a **clear error** instead of crashing:\n\n```python\ndef div(a, b):\n    if b == 0:\n        raise ValueError(\"cannot divide by zero\")\n    return a / b\n```\n\n- All **41 tests** pass (`pytest -v`)\n- I did *not* push; that needs your approval\n\n> Tip: `/bg` hands follow-up work to a background worker."
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
                if i == 2 {
                    sender.send(AppMessage::Turn {
                        session: session.clone(),
                        update: TurnUpdate::AwaitingApproval(ApprovalRequest {
                            tool: "shell.run".to_string(),
                            target: "git push origin main".to_string(),
                            reason: "network access needs approval".to_string(),
                        }),
                    });
                }
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

    fn fake_shell(&self, command: String) {
        let sender = self.sender.clone();
        let session = self.chat.session().clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            let stdout: String = (1..=7).map(|i| format!("{command}: line {i}\n")).collect();
            sender.send(AppMessage::Turn {
                session,
                update: TurnUpdate::ShellFinished(ToolCallView::command(
                    "shell", &command, 0, &stdout, "",
                )),
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
        let now = ctx.clock.now();
        if let Event::Input(InputEvent::Key(key)) = event {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                if self
                    .ctrl_c_at
                    .is_some_and(|at| now.millis_since(at) <= 1_200)
                {
                    self.shutdown.notify_one();
                    return Propagation::Consumed;
                }
                let arm = self.chat.on_ctrl_c(now, 1_200);
                self.ctrl_c_at = arm.then_some(now);
            }
        }
        let propagation = self.chat.handle_event(event, ctx);
        for action in self.chat.take_actions() {
            match action {
                ChatAction::Send(_) => self.play(),
                ChatAction::Shell(command) => self.fake_shell(command),
                ChatAction::Quit => self.shutdown.notify_one(),
                ChatAction::Approve(choice) => self
                    .chat
                    .push_notice(NoticeLevel::Info, format!("You chose {choice:?}.")),
                _ => self
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
    let clock = Arc::new(SystemClock);
    let _ = clock.now();
    let (mut runtime, sender) = Runtime::start(clock, Theme::dark()).await?;
    let mut chat = ChatScreen::new(
        ComponentId::new("demo.chat"),
        SessionId::new("S-1")?,
        StatusInfo {
            model: "devpass/muse-spark-1.3".to_string(),
            cwd: "~/src/calc".to_string(),
            branch: Some("main".to_string()),
            open_tickets: 3,
            ..StatusInfo::default()
        },
    );
    let files: FileIndex = Arc::default();
    let _ = files.set(vec![
        "calc.py".to_string(),
        "tests/test_calc.py".to_string(),
        "README.md".to_string(),
    ]);
    chat.set_file_index(files);
    let mut demo = Demo {
        chat,
        shutdown: runtime.shutdown_handle(),
        sender,
        ctrl_c_at: None,
    };
    runtime.run(&mut demo).await?;
    Ok(())
}
