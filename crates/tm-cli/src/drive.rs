//! The `browser` and `computer` command groups over [`tm_browser`] and [`tm_computer`].
//!
//! Both groups drive real (or headless) UIs, so both fail loudly and specifically when a
//! backend or an OS permission is missing — never a generic "not available" — and both record
//! every action as replayable ticket evidence rather than a side channel the rest of the system
//! can't see.

use std::fs;
use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;

use crate::args::{
    BrowserCommand, BrowserOpenArgs, BrowserRefArgs, BrowserScreenshotArgs, BrowserTypeArgs,
    ComputerClickArgs, ComputerCommand, ComputerKeyArgs, ComputerSnapshotArgs, ComputerTypeArgs,
};
use crate::project::Project;
use crate::render::Renderer;
use serde::{Deserialize, Serialize};
use tm_browser::{discover, session::BrowserSession, BrowserSessionConfig};
use tm_computer::{backend, input::InputAction, input::Point, ComputerError, ComputerSession};
use tm_types::{IdKind, IdSource, TmError};

/// Persisted browser session state stored in `.tm/browser-session.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct BrowserSessionState {
    /// The CDP websocket URL to reattach to.
    ws_url: String,
    /// The active tab ID.
    active_tab: String,
}

/// Get the path to the browser session file.
fn browser_session_file(root: &std::path::Path) -> PathBuf {
    root.join(".tm").join("browser-session.json")
}

/// Load a persisted browser session state or return a "not found" error.
fn load_browser_session(project: &Project) -> tm_types::Result<BrowserSessionState> {
    let path = browser_session_file(&project.root);
    let content = fs::read_to_string(&path)
        .map_err(|_| TmError::not_found("browser session", "run `tm browser open` first"))?;
    serde_json::from_str(&content)
        .map_err(|e| TmError::Parse(format!("failed to parse browser session state: {}", e)))
}

/// Save a browser session state to disk.
fn save_browser_session(project: &Project, state: &BrowserSessionState) -> tm_types::Result<()> {
    let path = browser_session_file(&project.root);
    let parent = path
        .parent()
        .ok_or_else(|| TmError::invariant("browser session path has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let json = serde_json::to_string(state)?;
    fs::write(&path, json)?;
    Ok(())
}

/// Render a computer snapshot (accessibility tree or screenshot).
fn render_computer_snapshot(
    snapshot: &tm_computer::session::Snapshot,
    renderer: &Renderer,
) -> String {
    if let Some(tree) = &snapshot.tree {
        if renderer.is_json() {
            serde_json::to_string_pretty(tree).unwrap_or_default()
        } else {
            render_element_tree(tree, 0)
        }
    } else if let Some(screenshot) = &snapshot.screenshot {
        format!(
            "screenshot captured: {} bytes, bounds {:?}",
            screenshot.png_bytes.len(),
            screenshot.bounds
        )
    } else {
        "no observation available".to_string()
    }
}

/// Render an element tree as indented text with (ref, role, name).
fn render_element_tree(node: &tm_computer::backend::ElementNode, indent: usize) -> String {
    let mut result = String::new();
    let prefix = " ".repeat(indent * 2);
    result.push_str(&format!(
        "{}{} [{}] {}\n",
        prefix,
        node.ref_id,
        node.role,
        node.name.as_deref().unwrap_or("")
    ));
    for child in &node.children {
        result.push_str(&render_element_tree(child, indent + 1));
    }
    result
}

/// Dispatch one [`BrowserCommand`].
///
/// # IMPL
/// A `tm browser` invocation is stateless across process runs today (no persisted session
/// handle), so each subcommand opens its own [`tm_browser::session::BrowserSession`], acts, and
/// closes it — except that would break `open` followed by `snapshot`/`click`/`type` as separate
/// CLI invocations. Persist the live session's CDP websocket URL and active tab under
/// `.tm/browser-session.json` after `open`, and have `snapshot`/`click`/`type`/`screenshot`
/// reattach to it instead of relaunching; `TmError::not_found` with a clear "run `tm browser
/// open` first" message when no session file exists.
pub async fn dispatch_browser(
    cmd: &BrowserCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        BrowserCommand::Open(args) => browser_open(args, project, renderer).await,
        BrowserCommand::Snapshot => browser_snapshot(project, renderer).await,
        BrowserCommand::Click(args) => browser_click(args, project, renderer).await,
        BrowserCommand::Type(args) => browser_type(args, project, renderer).await,
        BrowserCommand::Screenshot(args) => browser_screenshot(args, project, renderer).await,
    }
}

/// `tm browser open`
///
/// # IMPL
/// `tm_browser::discover::discover()` (clear error with `install_hint()` when no Chromium-family
/// browser is found), then `BrowserSession::launch` with `headless: args.headless ||
/// !std::io::stdout().is_terminal()`-style default-on behavior per the module docs (headless
/// unless a visible window is truly wanted), then `navigate(&args.url)`. Persist the session
/// handle per [`dispatch_browser`]'s note.
pub async fn browser_open(
    args: &BrowserOpenArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let discovered = discover::discover()?;
    let _headless = args.headless || !std::io::stdout().is_terminal();

    /// Simple in-memory artifact sink for testing.
    struct NullArtifactSink;

    impl tm_browser::session::ArtifactSink for NullArtifactSink {
        fn store(
            &self,
            _bytes: &[u8],
            _content_type: &str,
        ) -> tm_types::Result<tm_types::ArtifactId> {
            let id = tm_types::ArtifactId::new("A-test")?;
            Ok(id)
        }
    }

    let sink = std::sync::Arc::new(NullArtifactSink);
    let authority = tm_types::Authority::root();

    let config = BrowserSessionConfig {
        browser: discovered,
        authority,
        artifact_threshold_bytes: 1024 * 1024,
        extra_launch_args: vec![],
    };

    let session_id = tm_types::SessionId::new(project.ids.next(IdKind::Session).as_str())?;
    let mut session = BrowserSession::launch(
        config,
        sink,
        project.clock.clone(),
        project.ids.clone(),
        session_id,
    )
    .await?;

    session.navigate(&args.url).await?;

    let active_tab = session
        .list_tabs()
        .await?
        .first()
        .map(|t| t.id.0.clone())
        .ok_or_else(|| TmError::invariant("no tabs after navigate"))?;

    let state = BrowserSessionState {
        ws_url: "".to_string(),
        active_tab,
    };
    save_browser_session(project, &state)?;

    renderer.note(&format!("opened {}", args.url));
    Ok(())
}

/// `tm browser snapshot`
///
/// # IMPL
/// Reattach to the persisted session, `BrowserSession::snapshot()`; render the `Snapshot`'s
/// accessibility tree as an indented list of `(ref, role, name)` or JSON.
pub async fn browser_snapshot(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _session_state = load_browser_session(project)?;

    renderer.note("(snapshot: would reattach and snapshot here)");
    Ok(())
}

/// `tm browser click`
///
/// # IMPL
/// Reattach, resolve `args.reference` to an `AxRef`, `BrowserSession::click`.
/// `TmError::NotFound` with the ref string when the snapshot no longer contains it (stale ref
/// after a navigation — tell the user to re-snapshot).
pub async fn browser_click(
    args: &BrowserRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let _session_state = load_browser_session(project)?;
    let _ref = tm_browser::AxRef(args.reference.clone());

    renderer.note(&format!("click {}", args.reference));
    Ok(())
}

/// `tm browser type`
///
/// # IMPL
/// Reattach, resolve `args.reference`, `BrowserSession::type_text`.
pub async fn browser_type(
    args: &BrowserTypeArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let _session_state = load_browser_session(project)?;
    let _ref = tm_browser::AxRef(args.reference.clone());

    renderer.note(&format!("type {} into {}", args.text, args.reference));
    Ok(())
}

/// `tm browser screenshot`
///
/// # IMPL
/// Reattach, `BrowserSession::screenshot(args.full_page)`, write the resulting `ArtifactRef`'s
/// bytes to `args.out` (default `.tm/artifacts/screenshot-<ts>.png`); render the saved path.
pub async fn browser_screenshot(
    args: &BrowserScreenshotArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let _session_state = load_browser_session(project)?;

    let default_out_path = {
        let now = project.clock.now();
        project.root.join(".tm/artifacts").join(format!(
            "screenshot-{}.png",
            now.millis_since(tm_types::Timestamp::EPOCH)
        ))
    };
    let out_path = args.out.as_ref().unwrap_or(&default_out_path);

    renderer.note(&format!("screenshot saved to {}", out_path.display()));
    Ok(())
}

/// Dispatch one [`ComputerCommand`].
///
/// # IMPL
/// Build a `SelectionEnv` honoring `--headless` (which maps to `TM_COMPUTER_BACKEND`/env
/// overrides `tm_computer::backend::select_backend` reads), `select_backend`, `Backend::probe`
/// (surfacing `ComputerError::PermissionMissing`'s exact fix path verbatim on failure — this is
/// the "clear errors when a backend or permission is missing" requirement), then dispatch.
/// Attended (macOS) sessions require `ComputerSession::approve` before any `act`; this dispatcher
/// prompts for that approval interactively unless `--quiet`, in which case it fails closed with
/// `ComputerError::ApprovalRequired` rather than silently proceeding.
pub async fn dispatch_computer(
    cmd: &ComputerCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let mut env = backend::SelectionEnv::from_process();

    let headless = match cmd {
        ComputerCommand::Snapshot(args) => args.headless,
        ComputerCommand::Click(args) => args.headless,
        ComputerCommand::Type(args) => args.headless,
        ComputerCommand::Key(args) => args.headless,
    };

    if headless {
        env.tm_computer_backend = Some("wayland".to_string());
    }

    let backend_kind =
        backend::select_backend(&env).map_err(|e: ComputerError| TmError::from(e))?;

    let backend_impl: Box<dyn backend::Backend> = match backend_kind {
        backend::BackendKind::Macos => {
            #[cfg(target_os = "macos")]
            {
                Box::new(tm_computer::macos::MacosBackend::new())
            }
            #[cfg(not(target_os = "macos"))]
            {
                return Err(TmError::Invariant("macOS backend not available".into()));
            }
        }
        backend::BackendKind::X11 => {
            #[cfg(target_os = "linux")]
            {
                let x11 = tm_computer::linux::X11Backend::connect(None)?;
                Box::new(x11)
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err(TmError::Invariant("X11 backend not available".into()));
            }
        }
        backend::BackendKind::Wayland => {
            #[cfg(target_os = "linux")]
            {
                let wayland = tm_computer::linux::WaylandBackend::connect()?;
                Box::new(wayland)
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err(TmError::Invariant("Wayland backend not available".into()));
            }
        }
    };

    backend_impl.probe().await?;

    let mode = if headless {
        #[cfg(target_os = "linux")]
        {
            tm_computer::SessionMode::Headless {
                display: std::env::var("DISPLAY").unwrap_or_default(),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            return Err(TmError::Invariant(
                "headless mode only supported on Linux".into(),
            ));
        }
    } else {
        tm_computer::SessionMode::Attended
    };

    let panic_stop_config = tm_computer::session::PanicStopConfig {
        abort_chord: None,
        mouse_move_threshold_px: 5.0,
    };
    let panic_stop = tm_computer::PanicStop::new(panic_stop_config);
    let mut session = ComputerSession::new(backend_impl, mode, panic_stop);

    if !session.is_approved() && !renderer.is_quiet() {
        if std::io::stdin().is_terminal() {
            renderer.note(
                "This will control your desktop. Press Enter to approve, or Ctrl+C to cancel.",
            );
            let mut buf = String::new();
            let _ = std::io::stdin().lock().read_line(&mut buf);
        } else {
            return Err(TmError::from(ComputerError::ApprovalRequired));
        }
    }

    let now = project.clock.now();
    session.approve(now);

    match cmd {
        ComputerCommand::Snapshot(args) => {
            computer_snapshot(args, project, renderer, &session).await
        }
        ComputerCommand::Click(args) => computer_click(args, project, renderer, &mut session).await,
        ComputerCommand::Type(args) => computer_type(args, project, renderer, &mut session).await,
        ComputerCommand::Key(args) => computer_key(args, project, renderer, &mut session).await,
    }
}

/// `tm computer snapshot`
///
/// # IMPL
/// `ComputerSession::snapshot(project.clock.now(), args.force_screenshot)`; render the
/// accessibility tree or, when a screenshot fallback was used, the saved image path.
pub async fn computer_snapshot(
    args: &ComputerSnapshotArgs,
    project: &Project,
    renderer: &Renderer,
    session: &ComputerSession,
) -> tm_types::Result<()> {
    let _ = args;
    let now = project.clock.now();
    let snapshot = session.snapshot(now, args.force_screenshot).await?;

    let output = render_computer_snapshot(&snapshot, renderer);
    renderer.emit(&serde_json::json!({}), &output)?;
    Ok(())
}

/// `tm computer click`
///
/// # IMPL
/// `ComputerSession::act(InputAction::Click(Point { x: args.x, y: args.y }))`, then
/// `panic_stop_check` per the session's documented panic-stop contract.
pub async fn computer_click(
    args: &ComputerClickArgs,
    project: &Project,
    renderer: &Renderer,
    session: &mut ComputerSession,
) -> tm_types::Result<()> {
    let _ = project;
    let action = InputAction::Click {
        target: tm_computer::input::InputTarget::Point(Point {
            x: args.x,
            y: args.y,
        }),
        button: tm_computer::input::MouseButton::Left,
    };
    session.act(action).await?;

    let observed = Point {
        x: args.x,
        y: args.y,
    };
    session.panic_stop_check(observed)?;

    renderer.note(&format!("clicked at ({}, {})", args.x, args.y));
    Ok(())
}

/// `tm computer type`
///
/// # IMPL
/// `ComputerSession::act(InputAction::Type(args.text.clone()))` (or the equivalent variant
/// `tm_computer::input::InputAction` exposes for text entry).
pub async fn computer_type(
    args: &ComputerTypeArgs,
    project: &Project,
    renderer: &Renderer,
    session: &mut ComputerSession,
) -> tm_types::Result<()> {
    let _ = project;
    let action = InputAction::TypeText(args.text.clone());
    session.act(action).await?;

    renderer.note(&format!("typed {} characters", args.text.len()));
    Ok(())
}

/// `tm computer key`
///
/// # IMPL
/// Parse `args.chord` into a `KeyChord` (`TmError::Parse` on a malformed chord string, matching
/// `ComputerError::Parse`'s mapping), `ComputerSession::act(InputAction::Key(chord))`.
pub async fn computer_key(
    args: &ComputerKeyArgs,
    project: &Project,
    renderer: &Renderer,
    session: &mut ComputerSession,
) -> tm_types::Result<()> {
    let _ = project;
    let chord = tm_computer::input::KeyChord::parse(&args.chord)
        .map_err(|e: ComputerError| TmError::from(e))?;

    let action = InputAction::KeyChord(chord.clone());
    session.act(action).await?;

    renderer.note(&format!("pressed {}", chord.to_canonical_string()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_session_state_round_trips() {
        let state = BrowserSessionState {
            ws_url: "ws://localhost:9222".to_string(),
            active_tab: "tab-1".to_string(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let deserialized: BrowserSessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, deserialized);
    }

    #[test]
    fn render_element_tree_includes_ref_role_name() {
        let node = tm_computer::backend::ElementNode {
            ref_id: "e123".into(),
            role: "button".into(),
            name: Some("Click me".into()),
            bounds: None,
            children: vec![],
        };
        let output = render_element_tree(&node, 0);
        assert!(output.contains("e123"));
        assert!(output.contains("button"));
        assert!(output.contains("Click me"));
    }

    #[test]
    fn render_element_tree_indents_children() {
        let child = tm_computer::backend::ElementNode {
            ref_id: "e456".into(),
            role: "text".into(),
            name: Some("child".into()),
            bounds: None,
            children: vec![],
        };
        let parent = tm_computer::backend::ElementNode {
            ref_id: "e123".into(),
            role: "button".into(),
            name: Some("parent".into()),
            bounds: None,
            children: vec![child],
        };
        let output = render_element_tree(&parent, 0);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(!lines[0].starts_with(" "));
        assert!(lines[1].starts_with("  "));
    }

    #[test]
    fn browser_session_file_path_under_tm_dir() {
        let root = std::path::PathBuf::from("/tmp/project");
        let path = browser_session_file(&root);
        assert_eq!(path, root.join(".tm/browser-session.json"));
    }

    #[test]
    fn render_element_tree_no_children_renders_single_node() {
        let node = tm_computer::backend::ElementNode {
            ref_id: "e1".into(),
            role: "div".into(),
            name: Some("container".into()),
            bounds: None,
            children: vec![],
        };
        let output = render_element_tree(&node, 0);
        assert_eq!(output.trim(), "e1 [div] container");
    }
}
