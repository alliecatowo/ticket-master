// The chat screen's unit tests: key handling, modes, and rendering through a real `Buffer`.
#[cfg(test)]
use super::*;
use crate::caps::{Capabilities, ColorSupport, UnicodeSupport};
use crate::chat::transcript::ToolBody;
use crate::component::FocusState;
use std::sync::Arc;
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

fn ctrl(c: char) -> Event {
    chord(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn press(chat: &mut ChatScreen, env: &Env, event: Event) {
    chat.handle_event(&event, &env.ctx());
}

fn type_text(chat: &mut ChatScreen, env: &Env, text: &str) {
    for c in text.chars() {
        press(chat, env, key(KeyCode::Char(c)));
    }
}

fn update(chat: &mut ChatScreen, env: &Env, update: TurnUpdate) {
    let session = chat.session().clone();
    press(chat, env, Event::App(AppMessage::Turn { session, update }));
}

fn finished() -> TurnUpdate {
    TurnUpdate::Finished {
        notice: None,
        failed: false,
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
    type_text(&mut chat, &env, "quit? q! t/ @");
    assert_eq!(chat.input_text(), "quit? q! t/ @");
    assert!(chat.take_actions().is_empty());
    assert!(!chat.is_help_open());
    assert!(!chat.is_shell_mode());
}

#[test]
fn enter_sends_echoes_and_marks_the_turn_running() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "fix the flaky test");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Send("fix the flaky test".to_string())]
    );
    assert!(chat.is_turn_running());
    assert_eq!(chat.input_text(), "");
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("› fix the flaky test"), "{text}");
    assert!(text.contains("Thinking…"), "{text}");
    assert!(text.contains("esc to interrupt"), "{text}");
}

#[test]
fn a_message_sent_mid_turn_is_queued_dimmed_and_sent_when_the_turn_ends() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "one");
    press(&mut chat, &env, key(KeyCode::Enter));
    chat.take_actions();
    type_text(&mut chat, &env, "two");
    press(&mut chat, &env, key(KeyCode::Enter));
    type_text(&mut chat, &env, "three");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert!(chat.take_actions().is_empty(), "nothing is sent mid-turn");
    assert_eq!(chat.queued(), 2);
    assert_eq!(chat.input_text(), "");
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("› two  (queued)"), "{text}");

    update(&mut chat, &env, finished());
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Send("two\n\nthree".to_string())],
        "queued prompts go out together when the turn ends"
    );
    assert!(chat.is_turn_running());
    assert_eq!(chat.queued(), 0);
}

#[test]
fn up_takes_queued_messages_back() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "one");
    press(&mut chat, &env, key(KeyCode::Enter));
    type_text(&mut chat, &env, "two");
    press(&mut chat, &env, key(KeyCode::Enter));
    press(&mut chat, &env, key(KeyCode::Up));
    assert_eq!(chat.input_text(), "two");
    assert_eq!(chat.queued(), 0);
}

#[test]
fn bang_on_an_empty_prompt_is_shell_mode_and_enter_runs_the_command() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "!");
    assert!(chat.is_shell_mode());
    assert_eq!(chat.input_text(), "", "the ! is the mode, not text");
    let rows = render(&chat, &env, 80, 24);
    assert!(rows[21].contains("! Run a shell command"), "{rows:#?}");
    assert!(rows[23].contains("! for bash mode"), "{}", rows[23]);

    type_text(&mut chat, &env, "ls -la");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Shell("ls -la".to_string())]
    );
    assert!(!chat.is_shell_mode());
    assert!(chat.is_turn_running());
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("! ls -la"), "{text}");
    assert!(text.contains("⎿  Running…"), "{text}");

    update(
        &mut chat,
        &env,
        TurnUpdate::ShellFinished(ToolCallView::command(
            "shell",
            "ls -la",
            0,
            "a.txt\nb.txt\n",
            "",
        )),
    );
    assert!(!chat.is_turn_running());
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("⎿  a.txt"), "{text}");
    assert!(!text.contains("Running…"), "{text}");

    // History remembers it as a shell command.
    press(&mut chat, &env, key(KeyCode::Up));
    assert_eq!(chat.input_text(), "!ls -la");
}

#[test]
fn backspace_or_esc_on_an_empty_shell_prompt_leaves_shell_mode() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "!");
    press(&mut chat, &env, key(KeyCode::Backspace));
    assert!(!chat.is_shell_mode());
    type_text(&mut chat, &env, "!");
    press(&mut chat, &env, key(KeyCode::Esc));
    assert!(!chat.is_shell_mode());
    assert!(chat.take_actions().is_empty());
}

#[test]
fn pasting_a_bang_command_enters_shell_mode() {
    let env = Env::new();
    let mut chat = screen();
    press(
        &mut chat,
        &env,
        Event::Input(InputEvent::Paste("!git status".into())),
    );
    assert!(chat.is_shell_mode());
    assert_eq!(chat.input_text(), "git status");
}

#[test]
fn long_pastes_collapse_and_are_expanded_when_sent() {
    let env = Env::new();
    let mut chat = screen();
    let pasted: String = (1..=5).map(|i| format!("line {i}\n")).collect();
    type_text(&mut chat, &env, "explain ");
    press(
        &mut chat,
        &env,
        Event::Input(InputEvent::Paste(pasted.clone())),
    );
    assert_eq!(chat.input_text(), "explain [Pasted text #1 +4 lines]");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Send(format!("explain {}", pasted.trim_end()))]
    );
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("explain [Pasted text #1 +4 lines]"), "{text}");
}

#[test]
fn shift_tab_cycles_the_mode_and_the_status_line_shows_it() {
    let env = Env::new();
    let mut chat = screen();
    let status_row = |chat: &ChatScreen| render(chat, &env, 100, 24)[23].clone();
    assert!(status_row(&chat).contains("? for shortcuts"));
    press(
        &mut chat,
        &env,
        chord(KeyCode::BackTab, KeyModifiers::SHIFT),
    );
    assert_eq!(chat.mode(), PermissionMode::Plan);
    assert!(
        status_row(&chat).contains("⏸ plan mode on (shift+tab to cycle)"),
        "{}",
        status_row(&chat)
    );
    press(
        &mut chat,
        &env,
        chord(KeyCode::BackTab, KeyModifiers::SHIFT),
    );
    assert!(status_row(&chat).contains("⏵ ask mode on"));
    press(
        &mut chat,
        &env,
        chord(KeyCode::BackTab, KeyModifiers::SHIFT),
    );
    assert!(status_row(&chat).contains("? for shortcuts"));
    assert_eq!(
        chat.take_actions(),
        vec![
            ChatAction::SetMode(PermissionMode::Plan),
            ChatAction::SetMode(PermissionMode::Ask),
            ChatAction::SetMode(PermissionMode::Auto),
        ]
    );
}

#[test]
fn esc_interrupts_a_turn_and_esc_esc_clears_the_draft() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "go");
    press(&mut chat, &env, key(KeyCode::Enter));
    chat.take_actions();
    press(&mut chat, &env, key(KeyCode::Esc));
    assert_eq!(chat.take_actions(), vec![ChatAction::Interrupt]);
    press(&mut chat, &env, key(KeyCode::Esc));
    assert!(chat.take_actions().is_empty(), "one interrupt per turn");
    update(&mut chat, &env, finished());

    type_text(&mut chat, &env, "draft");
    press(&mut chat, &env, key(KeyCode::Esc));
    assert_eq!(chat.input_text(), "draft", "one Esc only warns");
    press(&mut chat, &env, key(KeyCode::Esc));
    assert_eq!(chat.input_text(), "", "the second clears");
    press(&mut chat, &env, key(KeyCode::Up));
    assert_eq!(chat.input_text(), "draft", "and ↑ brings it back");
}

#[test]
fn ctrl_c_closes_dialogs_first_then_interrupts_then_clears() {
    let env = Env::new();
    let now = env.clock.now();
    let mut chat = screen();
    press(&mut chat, &env, ctrl('o'));
    assert!(chat.is_viewer_open());
    assert!(
        !chat.on_ctrl_c(now, 1_000),
        "closing a dialog does not arm quit"
    );
    assert!(!chat.is_viewer_open());

    type_text(&mut chat, &env, "half written");
    assert!(chat.on_ctrl_c(now, 1_000));
    assert_eq!(chat.input_text(), "");
    let rows = render(&chat, &env, 100, 24);
    assert!(
        rows[23].contains("Press Ctrl+C again to quit"),
        "{}",
        rows[23]
    );
    press(&mut chat, &env, key(KeyCode::Up));
    assert_eq!(chat.input_text(), "half written");

    press(&mut chat, &env, key(KeyCode::Enter));
    chat.take_actions();
    assert!(chat.on_ctrl_c(now, 1_000));
    assert_eq!(chat.take_actions(), vec![ChatAction::Interrupt]);
}

#[test]
fn ctrl_o_opens_the_viewer_with_full_output_and_q_closes_it() {
    let env = Env::new();
    let mut chat = screen();
    let stdout: String = (1..=12).map(|i| format!("row {i}\n")).collect();
    chat.push_entry(Entry::Tool(ToolCallView::command(
        "shell.run",
        "make test",
        0,
        &stdout,
        "",
    )));
    let compact = screen_text(&render(&chat, &env, 80, 24));
    assert!(
        compact.contains("… +8 lines (ctrl+o to expand)"),
        "{compact}"
    );
    assert!(!compact.contains("row 12"));

    press(&mut chat, &env, ctrl('o'));
    let full = screen_text(&render(&chat, &env, 80, 40));
    assert!(full.contains("Transcript"), "{full}");
    assert!(full.contains("row 12"), "{full}");
    assert!(full.contains("shell.run"), "{full}");
    press(&mut chat, &env, key(KeyCode::Char('q')));
    assert!(!chat.is_viewer_open());
    assert_eq!(
        chat.input_text(),
        "",
        "q closed the viewer, it was not typed"
    );
}

#[test]
fn question_mark_on_an_empty_prompt_toggles_the_shortcuts_panel() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "?");
    assert!(chat.is_help_open());
    let text = screen_text(&render(&chat, &env, 100, 30));
    for needle in [
        "! for bash mode",
        "/ for commands",
        "@ for file paths",
        "shift + tab to cycle modes",
        "ctrl + o for transcript",
        "double tap esc to clear input",
    ] {
        assert!(text.contains(needle), "{needle:?} missing:\n{text}");
    }
    type_text(&mut chat, &env, "?");
    assert!(!chat.is_help_open());
    assert_eq!(chat.input_text(), "", "? on an empty prompt is not typed");

    type_text(&mut chat, &env, "?");
    type_text(&mut chat, &env, "h");
    assert!(!chat.is_help_open(), "typing closes the panel");
    assert_eq!(chat.input_text(), "h", "and still types");
}

#[test]
fn slash_help_shows_the_panel_and_status_and_cost_answer_in_the_transcript() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "/help");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert!(chat.is_help_open());
    press(&mut chat, &env, key(KeyCode::Esc));

    type_text(&mut chat, &env, "/status");
    press(&mut chat, &env, key(KeyCode::Esc));
    press(&mut chat, &env, key(KeyCode::Enter));
    let text = screen_text(&render(&chat, &env, 100, 30));
    for needle in [
        "Model: mock/m1",
        "Provider: mock",
        "Directory: ~/proj (main)",
        "Mode: auto",
        "Ticket: none",
        "Tokens: 0",
    ] {
        assert!(text.contains(needle), "{needle:?} missing:\n{text}");
    }

    type_text(&mut chat, &env, "hello");
    press(&mut chat, &env, key(KeyCode::Enter));
    update(
        &mut chat,
        &env,
        TurnUpdate::Progress {
            entries: Vec::new(),
            served_by: None,
            tokens: 2_500,
            activity: None,
        },
    );
    update(&mut chat, &env, finished());
    type_text(&mut chat, &env, "/cost");
    press(&mut chat, &env, key(KeyCode::Esc));
    press(&mut chat, &env, key(KeyCode::Enter));
    let text = screen_text(&render(&chat, &env, 100, 30));
    assert!(text.contains("Tokens this session: 2.5k"), "{text}");
    assert!(text.contains("1.    2.5k  hello"), "{text}");
    assert!(chat
        .take_actions()
        .iter()
        .all(|a| !matches!(a, ChatAction::Command { .. })));
}

#[test]
fn new_commands_reach_the_application() {
    let env = Env::new();
    let mut chat = screen();
    for (typed, id, arg) in [
        ("/compact", CommandId::Compact, ""),
        ("/model", CommandId::Model, ""),
        ("/model devpass/x", CommandId::Model, "devpass/x"),
        ("/init", CommandId::Init, ""),
        ("/bg ship it", CommandId::Bg, "ship it"),
        ("/resume", CommandId::Resume, ""),
        ("/clear", CommandId::Clear, ""),
    ] {
        type_text(&mut chat, &env, typed);
        press(&mut chat, &env, key(KeyCode::Esc));
        press(&mut chat, &env, key(KeyCode::Enter));
        assert_eq!(
            chat.take_actions(),
            vec![ChatAction::Command {
                id,
                arg: arg.to_string()
            }],
            "{typed}"
        );
    }
}

#[test]
fn at_opens_file_completion_and_tab_inserts_the_path() {
    let env = Env::new();
    let mut chat = screen();
    let files: FileIndex = Arc::default();
    let _ = files.set(vec![
        "src/main.rs".to_string(),
        "src/chat/input.rs".to_string(),
        "README.md".to_string(),
    ]);
    chat.set_file_index(files);
    type_text(&mut chat, &env, "look at @inp");
    assert!(chat.is_mention_open());
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(
        text.contains("input.rs") && text.contains("src/chat/"),
        "{text}"
    );
    press(&mut chat, &env, key(KeyCode::Tab));
    assert_eq!(chat.input_text(), "look at @src/chat/input.rs ");
    assert!(!chat.is_mention_open());

    type_text(&mut chat, &env, "and @READ");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(
        chat.input_text(),
        "look at @src/chat/input.rs and @README.md "
    );
    assert!(
        chat.take_actions().is_empty(),
        "Enter inserted, it did not send"
    );
}

#[test]
fn at_while_the_index_builds_says_so() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "@x");
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("Indexing files"), "{text}");
}

#[test]
fn ctrl_r_searches_history_and_tab_accepts() {
    let env = Env::new();
    let mut chat = screen();
    chat.load_history(vec![
        "run the tests".to_string(),
        "fix the build".to_string(),
        "run the linter".to_string(),
    ]);
    press(&mut chat, &env, ctrl('r'));
    type_text(&mut chat, &env, "run");
    assert_eq!(chat.input_text(), "run the linter", "newest match first");
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("search history: run"), "{text}");
    press(&mut chat, &env, ctrl('r'));
    assert_eq!(
        chat.input_text(),
        "run the tests",
        "ctrl+r steps to older matches"
    );
    press(&mut chat, &env, key(KeyCode::Tab));
    assert_eq!(chat.input_text(), "run the tests");
    type_text(&mut chat, &env, "!");
    assert_eq!(chat.input_text(), "run the tests!", "editing resumes");

    press(&mut chat, &env, ctrl('u'));
    press(&mut chat, &env, ctrl('r'));
    type_text(&mut chat, &env, "zzz");
    assert!(!chat.on_ctrl_c(env.clock.now(), 1_000));
    assert_eq!(chat.input_text(), "", "ctrl+c cancels and restores");
}

#[test]
fn emacs_keys_edit_the_prompt() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "hello brave world");
    press(&mut chat, &env, ctrl('a'));
    press(&mut chat, &env, ctrl('k'));
    assert_eq!(chat.input_text(), "");
    press(&mut chat, &env, ctrl('y'));
    assert_eq!(chat.input_text(), "hello brave world");
    press(
        &mut chat,
        &env,
        chord(KeyCode::Char('b'), KeyModifiers::ALT),
    );
    press(
        &mut chat,
        &env,
        chord(KeyCode::Char('d'), KeyModifiers::ALT),
    );
    assert_eq!(chat.input_text(), "hello brave ");
    press(&mut chat, &env, ctrl('7'));
    assert_eq!(chat.input_text(), "hello brave world", "ctrl+_ undoes");
    press(&mut chat, &env, ctrl('e'));
    press(&mut chat, &env, ctrl('w'));
    assert_eq!(chat.input_text(), "hello brave ");
}

#[test]
fn newline_chords_insert_instead_of_sending() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "a");
    press(&mut chat, &env, chord(KeyCode::Enter, KeyModifiers::SHIFT));
    type_text(&mut chat, &env, "b");
    press(&mut chat, &env, ctrl('j'));
    type_text(&mut chat, &env, "c\\");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(chat.input_text(), "a\nb\nc\n");
    assert!(chat.take_actions().is_empty());
}

#[test]
fn ctrl_g_asks_for_the_external_editor_with_pastes_expanded() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "draft ");
    press(
        &mut chat,
        &env,
        Event::Input(InputEvent::Paste("x\ny\nz\nw".into())),
    );
    press(&mut chat, &env, ctrl('g'));
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::OpenEditor("draft x\ny\nz\nw".into())]
    );
    chat.set_input("edited");
    assert_eq!(chat.input_text(), "edited");
}

#[test]
fn the_permission_prompt_replaces_the_input_and_numbers_answer_it() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "push it");
    press(&mut chat, &env, key(KeyCode::Enter));
    chat.take_actions();
    update(
        &mut chat,
        &env,
        TurnUpdate::AwaitingApproval(ApprovalRequest {
            tool: "shell.run".into(),
            target: "git push".into(),
            reason: "network access".into(),
        }),
    );
    assert!(chat.is_awaiting_approval());
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("Do you want to proceed?"), "{text}");
    assert!(
        text.contains("2. Yes, and don't ask again this session"),
        "{text}"
    );
    assert!(!text.contains("Ask tm anything"), "{text}");
    type_text(&mut chat, &env, "2");
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Approve(ApprovalChoice::YesForSession)]
    );
    assert!(!chat.is_awaiting_approval());
    assert_eq!(chat.input_text(), "", "the 2 answered, it was not typed");
}

#[test]
fn the_resume_picker_lists_conversations_and_enter_resumes() {
    let env = Env::new();
    let mut chat = screen();
    chat.open_resume_picker(vec![ConversationRow {
        id: "S-7".into(),
        first_message: "refactor the parser".into(),
        age: "3h ago".into(),
        turns: 4,
    }]);
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("refactor the parser"), "{text}");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(chat.take_actions(), vec![ChatAction::Resume("S-7".into())]);
    assert!(!chat.is_picker_open());
}

#[test]
fn slash_opens_the_popup_and_filtering_then_enter_runs_the_command() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "/");
    assert!(chat.is_popup_open());
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("/help") && text.contains("/clear"), "{text}");

    type_text(&mut chat, &env, "cle");
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("/clear"));
    assert!(!text.contains("/attach"), "{text}");
    press(&mut chat, &env, key(KeyCode::Enter));
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
    type_text(&mut chat, &env, "/at");
    press(&mut chat, &env, key(KeyCode::Tab));
    assert_eq!(chat.input_text(), "/attach ");
    assert!(!chat.is_popup_open());
    type_text(&mut chat, &env, "T-4");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Command {
            id: CommandId::Attach,
            arg: "T-4".to_string()
        }]
    );

    type_text(&mut chat, &env, "/");
    press(&mut chat, &env, key(KeyCode::Down));
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Command {
            id: CommandId::Clear,
            arg: String::new()
        }]
    );
}

#[test]
fn shell_mode_never_opens_the_command_popup() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "!/usr/bin/env");
    assert!(chat.is_shell_mode());
    assert!(!chat.is_popup_open());
    assert!(!chat.is_mention_open());
}

#[test]
fn esc_dismisses_the_popup_until_the_query_changes() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "/he");
    press(&mut chat, &env, key(KeyCode::Esc));
    assert!(!chat.is_popup_open());
    assert_eq!(chat.input_text(), "/he");
    type_text(&mut chat, &env, "l");
    assert!(chat.is_popup_open());
}

#[test]
fn left_goes_to_tickets_only_on_an_empty_prompt_with_nothing_open() {
    let env = Env::new();
    let mut chat = screen();
    assert!(chat.left_opens_tickets());
    type_text(&mut chat, &env, "?");
    assert!(
        !chat.left_opens_tickets(),
        "the shortcuts panel owns the key"
    );
    type_text(&mut chat, &env, "?");
    type_text(&mut chat, &env, "!");
    assert!(!chat.left_opens_tickets(), "shell mode");
    press(&mut chat, &env, key(KeyCode::Backspace));
    press(&mut chat, &env, ctrl('o'));
    assert!(!chat.left_opens_tickets(), "the viewer");
    press(&mut chat, &env, key(KeyCode::Esc));
    press(&mut chat, &env, ctrl('r'));
    assert!(!chat.left_opens_tickets(), "history search");
    press(&mut chat, &env, key(KeyCode::Esc));
    type_text(&mut chat, &env, "ab");
    assert!(!chat.left_opens_tickets(), "text in the prompt");
    press(&mut chat, &env, key(KeyCode::Left));
    assert!(chat.take_actions().is_empty(), "← only moves the cursor");
}

#[test]
fn ctrl_t_toggles_the_task_checklist() {
    use crate::chat::tasks::{TaskItem, TaskState};
    let env = Env::new();
    let mut chat = screen();
    press(&mut chat, &env, ctrl('t'));
    assert_eq!(chat.take_actions(), vec![ChatAction::ShowTasks]);
    chat.show_tasks(vec![TaskItem {
        state: TaskState::Active,
        text: "T-5 fix the flaky test".into(),
    }]);
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("Tasks (ctrl+t to hide)"), "{text}");
    assert!(text.contains("☐ T-5 fix the flaky test"), "{text}");
    press(&mut chat, &env, ctrl('t'));
    assert!(!chat.is_tasks_open());
    assert!(chat.take_actions().is_empty());
}

#[test]
fn ctrl_d_quits_only_on_an_empty_prompt() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "x");
    press(&mut chat, &env, ctrl('d'));
    assert!(chat.take_actions().is_empty());
    assert_eq!(chat.input_text(), "x");
    press(&mut chat, &env, ctrl('u'));
    press(&mut chat, &env, ctrl('d'));
    assert_eq!(chat.take_actions(), vec![ChatAction::Quit]);
}

#[test]
fn unknown_commands_are_reported_not_sent() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "/frobnicate");
    press(&mut chat, &env, key(KeyCode::Esc));
    press(&mut chat, &env, key(KeyCode::Enter));
    assert!(chat.take_actions().is_empty());
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("Unknown command /frobnicate"), "{text}");
}

#[test]
fn a_path_like_message_is_sent_not_parsed_as_a_command() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "/usr/bin is missing python");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Send("/usr/bin is missing python".to_string())]
    );
}

#[test]
fn turn_progress_renders_claude_code_style_tools_and_settles_tokens() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "run the tests");
    press(&mut chat, &env, key(KeyCode::Enter));
    update(
        &mut chat,
        &env,
        TurnUpdate::Progress {
            entries: vec![
                Entry::Tool(ToolCallView::command(
                    "shell.run",
                    "python3 -m pytest -q",
                    0,
                    "5 passed\n",
                    "",
                )),
                Entry::Tool(ToolCallView {
                    body: ToolBody::Summary("Read 12 lines".into()),
                    ..ToolCallView::new(ToolStatus::Ok, "fs.read", "calc.py")
                }),
                Entry::Assistant("All **green**.".to_string()),
            ],
            served_by: Some("devpass/muse".to_string()),
            tokens: 1_500,
            activity: Some("Thinking".to_string()),
        },
    );
    let text = screen_text(&render(&chat, &env, 100, 30));
    assert!(text.contains("⏺ Bash(python3 -m pytest -q)"), "{text}");
    assert!(text.contains("⎿  5 passed"), "{text}");
    assert!(text.contains("⏺ Read(calc.py)"), "{text}");
    assert!(text.contains("⎿  Read 12 lines"), "{text}");
    assert!(text.contains("⏺ All green."), "{text}");
    assert!(text.contains("devpass/muse"), "{text}");

    update(&mut chat, &env, finished());
    assert!(!chat.is_turn_running());
    let text = screen_text(&render(&chat, &env, 100, 30));
    assert!(text.contains("1.5k tokens"), "{text}");
    assert!(!text.contains("Thinking…"));
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
fn the_welcome_box_reads_like_claude_codes() {
    let env = Env::new();
    let chat = screen();
    let text = screen_text(&render(&chat, &env, 80, 24));
    for needle in [
        "✻ Welcome to tm!",
        "/help for help, /status for your current setup",
        "cwd: ~/proj (main)",
        "model: mock/m1",
        "Tips for getting started:",
        "Ask tm anything",
    ] {
        assert!(text.contains(needle), "{needle:?} missing:\n{text}");
    }
}

#[test]
fn the_status_line_shows_shortcuts_left_and_model_right() {
    let env = Env::new();
    let chat = screen();
    let rows = render(&chat, &env, 100, 24);
    let status = &rows[23];
    assert!(status.starts_with(" ? for shortcuts"), "{status}");
    assert!(status.contains("mock/m1"), "{status}");
    assert!(status.contains("no ticket"), "{status}");
}

#[test]
fn it_degrades_to_ascii_and_small_terminals_without_panicking() {
    let mut env = Env::new();
    env.caps = Capabilities::minimal();
    let mut chat = screen();
    type_text(&mut chat, &env, "hello");
    press(&mut chat, &env, key(KeyCode::Enter));
    chat.push_entry(Entry::Tool(ToolCallView::command(
        "shell.run",
        "ls",
        1,
        "a\nb\n",
        "boom",
    )));
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
    for opener in ["?", "/", "@"] {
        press(&mut chat, &env, ctrl('u'));
        type_text(&mut chat, &env, opener);
        let _ = render(&chat, &env, 30, 10);
        let _ = render(&chat, &env, 12, 4);
    }
    press(&mut chat, &env, ctrl('o'));
    let _ = render(&chat, &env, 12, 4);
}

#[test]
fn reset_clears_the_conversation_but_keeps_history() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "remember me");
    press(&mut chat, &env, key(KeyCode::Enter));
    chat.reset(SessionId::new("S-2").expect("valid"));
    assert!(chat.transcript().is_empty());
    assert!(!chat.is_turn_running());
    press(&mut chat, &env, key(KeyCode::Up));
    assert_eq!(chat.input_text(), "remember me");
}

#[test]
fn history_additions_are_reported_for_persisting() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "first");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(chat.take_unsaved_history(), vec!["first".to_string()]);
    assert!(chat.take_unsaved_history().is_empty());
}

#[test]
fn a_forced_repaint_draws_one_blank_frame_then_the_real_one() {
    let env = Env::new();
    let mut chat = screen();
    chat.force_full_repaint();
    let area = Rect::new(0, 0, 40, 10);
    let mut buf = Buffer::empty(area);
    chat.render(area, &mut buf, &env.ctx());
    assert!(buf[(0, 0)].modifier.contains(Modifier::HIDDEN));
    assert!(!screen_text(&render(&chat, &env, 40, 10)).trim().is_empty());
}

#[test]
fn a_permission_prompt_closes_the_viewer_so_esc_cannot_answer_it_unseen() {
    let env = Env::new();
    let mut chat = screen();
    type_text(&mut chat, &env, "go");
    press(&mut chat, &env, key(KeyCode::Enter));
    chat.take_actions();
    press(&mut chat, &env, ctrl('o'));
    assert!(chat.is_viewer_open());
    update(
        &mut chat,
        &env,
        TurnUpdate::AwaitingApproval(ApprovalRequest {
            tool: "shell.run".into(),
            target: "rm -rf build\n\x1b[2J".into(),
            reason: "ask mode".into(),
        }),
    );
    assert!(!chat.is_viewer_open());
    let text = screen_text(&render(&chat, &env, 80, 24));
    assert!(text.contains("Do you want to proceed?"), "{text}");
    assert!(text.contains("rm -rf build"), "{text}");
}

#[test]
fn a_recalled_bang_command_in_shell_mode_runs_once_not_as_bang_bang() {
    let env = Env::new();
    let mut chat = screen();
    chat.load_history(vec!["!ls -la".to_string()]);
    type_text(&mut chat, &env, "!");
    press(&mut chat, &env, key(KeyCode::Up));
    assert_eq!(chat.input_text(), "!ls -la");
    press(&mut chat, &env, key(KeyCode::Enter));
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::Shell("ls -la".into())]
    );
}

#[test]
fn every_overlay_survives_tiny_terminals() {
    use crate::chat::tasks::{TaskItem, TaskState};
    let env = Env::new();
    let sizes = [(20u16, 6u16), (10, 3), (4, 1), (80, 24)];
    let mut chat = screen();
    type_text(&mut chat, &env, "go");
    press(&mut chat, &env, key(KeyCode::Enter));
    update(
        &mut chat,
        &env,
        TurnUpdate::AwaitingApproval(ApprovalRequest {
            tool: "edit.write_file".into(),
            target: "src/main.rs".into(),
            reason: "ask mode".into(),
        }),
    );
    for (w, h) in sizes {
        assert_eq!(render(&chat, &env, w, h).len(), h as usize);
    }
    let mut chat = screen();
    chat.show_tasks(vec![TaskItem {
        state: TaskState::Pending,
        text: "T-1 x".into(),
    }]);
    chat.open_resume_picker(vec![ConversationRow {
        id: "S-2".into(),
        first_message: "中文".repeat(40),
        age: "1m ago".into(),
        turns: 2,
    }]);
    for (w, h) in sizes {
        assert_eq!(render(&chat, &env, w, h).len(), h as usize);
    }
    press(&mut chat, &env, key(KeyCode::Esc));
    press(&mut chat, &env, ctrl('o'));
    for (w, h) in sizes {
        assert_eq!(render(&chat, &env, w, h).len(), h as usize);
    }
}
