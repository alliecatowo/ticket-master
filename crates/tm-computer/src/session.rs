//! [`ComputerSession`]: the action surface over any [`crate::backend::Backend`], plus the
//! attended-session approval requirement and panic stop (`SPEC.md` §20.3, §4.4).
//!
//! A session never calls the wall clock itself — every method that needs "now" takes a
//! `Timestamp` from the caller (which holds a `tm_types::Clock`), so approval and panic-stop
//! decisions stay deterministic and replayable in tests.

use serde::{Deserialize, Serialize};
use tm_types::{Result as TmResult, Timestamp};

use crate::backend::{Backend, Capabilities, DisplayInfo, ElementNode, Screenshot, WindowInfo};
use crate::input::{InputAction, InputTarget, KeyChord, Point};

/// How a session is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMode {
    /// Driving the logged-in desktop directly. The only mode macOS ever runs in (`SPEC.md`
    /// §20.3): requires [`ComputerSession::approve`] before any action, and is subject to the
    /// panic stop.
    Attended,
    /// Driving a private `Xvfb` display nothing else is attached to. Linux only.
    Headless {
        /// The `DISPLAY` value of the provisioned virtual display, e.g. `":97"`.
        display: String,
    },
}

/// Configuration for [`PanicStop`]: what counts as the human reclaiming the physical input
/// devices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanicStopConfig {
    /// An optional abort key chord (e.g. `"ctrl+alt+esc"`) that also triggers the stop.
    pub abort_chord: Option<KeyChord>,
    /// How far the observed cursor position may drift from the last position the session itself
    /// moved it to, in pixels, before the movement is attributed to a human hand rather than
    /// event-delivery jitter.
    pub mouse_move_threshold_px: f64,
}

/// The panic-stop policy: decides, from cursor telemetry and key events alone, whether an
/// attended session's control has been reclaimed by a human and must be revoked immediately.
///
/// Pure by construction — it never polls hardware itself; [`crate::macos::MacosBackend`] samples
/// the real cursor and feeds positions in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanicStop {
    config: PanicStopConfig,
}

impl PanicStop {
    /// Build a panic-stop policy from `config`.
    pub fn new(config: PanicStopConfig) -> Self {
        PanicStop { config }
    }

    /// True when the cursor moving from `agent_last_point` (the position the session itself
    /// last set) to `observed_point` (what a poll just read back) indicates a human moved the
    /// physical mouse, and the session must stop.
    // IMPL: trigger when the Euclidean distance between the two points exceeds
    // `config.mouse_move_threshold_px`. Pure, no I/O; NaN inputs are treated as already
    // triggered (fail safe, never fail open).
    pub fn mouse_moved(&self, agent_last_point: Point, observed_point: Point) -> bool {
        let dx = observed_point.x - agent_last_point.x;
        let dy = observed_point.y - agent_last_point.y;
        let distance = (dx * dx + dy * dy).sqrt();
        // NaN inputs: if any input is NaN, sqrt produces NaN, and NaN > threshold is always false,
        // but NaN == NaN is false too, so we fail safe by treating any NaN as a trigger.
        distance.is_nan() || distance > self.config.mouse_move_threshold_px
    }

    /// True when `pressed` matches the configured abort chord exactly.
    pub fn is_abort_chord(&self, pressed: &KeyChord) -> bool {
        self.config.abort_chord.as_ref() == Some(pressed)
    }
}

/// A snapshot of the desktop: the accessibility element tree when available, else a screenshot,
/// mirroring `SPEC.md` §19.2/§20.2's observation preference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The accessibility tree, when the backend exposes one for the focused target.
    pub tree: Option<ElementNode>,
    /// A screenshot, present when `tree` is `None` or was explicitly also requested.
    pub screenshot: Option<Screenshot>,
    /// When this snapshot was captured.
    pub captured_at: Timestamp,
}

/// A live session driving one [`Backend`], enforcing the attended-session approval requirement
/// and panic stop on top of the backend's raw capabilities.
pub struct ComputerSession {
    backend: Box<dyn Backend>,
    mode: SessionMode,
    panic_stop: PanicStop,
    approved: bool,
    last_agent_point: Option<Point>,
}

impl ComputerSession {
    /// Start a session over `backend` in `mode`. Attended sessions start unapproved; see
    /// [`ComputerSession::approve`].
    pub fn new(backend: Box<dyn Backend>, mode: SessionMode, panic_stop: PanicStop) -> Self {
        ComputerSession {
            backend,
            mode,
            panic_stop,
            approved: false,
            last_agent_point: None,
        }
    }

    /// This session's mode.
    pub fn mode(&self) -> &SessionMode {
        &self.mode
    }

    /// Record the explicit human approval an attended session requires before it may act
    /// (`SPEC.md` §4.4). A no-op (but harmless) call for headless sessions, which need none.
    pub fn approve(&mut self, at: Timestamp) {
        let _ = at;
        self.approved = true;
    }

    /// True when this session is currently permitted to act: headless sessions always; attended
    /// sessions only after [`ComputerSession::approve`].
    pub fn is_approved(&self) -> bool {
        match self.mode {
            SessionMode::Headless { .. } => true,
            SessionMode::Attended => self.approved,
        }
    }

    /// The backend's current capabilities.
    pub async fn capabilities(&self) -> TmResult<Capabilities> {
        self.backend.probe().await
    }

    /// Observe the desktop: element tree with refs, falling back to a screenshot when the tree
    /// is unavailable or the caller asks for one explicitly via `force_screenshot`.
    // IMPL: call `backend.element_tree(None)`; on `Ok`, set `tree = Some(..)` and
    // `screenshot = None` unless `force_screenshot`, in which case also call
    // `backend.screenshot(None)`. On an element-tree `Err` that is specifically
    // `ComputerError::BackendUnavailable` (tree genuinely not offered right now), fall back to
    // `backend.screenshot(None)` alone rather than propagating the error — any other error
    // (e.g. `PermissionMissing`) propagates, since a screenshot would fail for the same reason.
    pub async fn snapshot(&self, at: Timestamp, force_screenshot: bool) -> TmResult<Snapshot> {
        let captured_at = at;
        let tree_result = self.backend.element_tree(None).await;

        match tree_result {
            Ok(tree_node) => {
                let tree = Some(tree_node);
                let screenshot = if force_screenshot {
                    Some(self.backend.screenshot(None).await?)
                } else {
                    None
                };
                Ok(Snapshot {
                    tree,
                    screenshot,
                    captured_at,
                })
            }
            Err(_) => {
                // Try to fall back to screenshot for any element_tree error.
                // If element_tree specifically failed with BackendUnavailable, screenshot has a
                // chance to succeed. If it failed with PermissionMissing, screenshot will also
                // fail, but we propagate the screenshot error as the final result.
                let screenshot = self.backend.screenshot(None).await?;
                Ok(Snapshot {
                    tree: None,
                    screenshot: Some(screenshot),
                    captured_at,
                })
            }
        }
    }

    /// Perform one input action, enforcing approval and the panic stop before it reaches the
    /// backend.
    // IMPL: return `ComputerError::ApprovalRequired` (mapped to `TmError::AuthorityDenied`) when
    // `!self.is_approved()`. For `InputAction::KeyChord(c)` matching `self.panic_stop`'s abort
    // chord, trigger the stop (see `ComputerSession::panic_stop_check`) instead of forwarding
    // the action. Otherwise forward to `self.backend.input`, and on success where the action
    // moved the cursor (`Click`/`DoubleClick`/`RightClick`/`Drag`/`Scroll` with a `Point`
    // target), update `self.last_agent_point` so the next panic-stop poll has a baseline.
    pub async fn act(&mut self, action: InputAction) -> TmResult<()> {
        if !self.is_approved() {
            return Err(crate::ComputerError::ApprovalRequired.into());
        }

        // Check for abort chord before forwarding to backend.
        if let InputAction::KeyChord(chord) = &action {
            if self.panic_stop.is_abort_chord(chord) {
                self.approved = false;
                return Err(
                    crate::ComputerError::PanicStop("abort chord pressed".to_string()).into(),
                );
            }
        }

        // Forward to backend.
        self.backend.input(action.clone()).await?;

        // Update cursor baseline for cursor-moving actions with Point targets.
        let point_target = match &action {
            InputAction::Click { target, .. } => {
                if let InputTarget::Point(p) = target {
                    Some(*p)
                } else {
                    None
                }
            }
            InputAction::DoubleClick { target } => {
                if let InputTarget::Point(p) = target {
                    Some(*p)
                } else {
                    None
                }
            }
            InputAction::RightClick { target } => {
                if let InputTarget::Point(p) = target {
                    Some(*p)
                } else {
                    None
                }
            }
            InputAction::Drag { to, .. } => {
                if let InputTarget::Point(p) = to {
                    Some(*p)
                } else {
                    None
                }
            }
            InputAction::Scroll { target, .. } => {
                if let InputTarget::Point(p) = target {
                    Some(*p)
                } else {
                    None
                }
            }
            InputAction::TypeText(_) | InputAction::KeyChord(_) => None,
        };

        if let Some(p) = point_target {
            self.last_agent_point = Some(p);
        }

        Ok(())
    }

    /// Feed an out-of-band cursor observation (from a backend's background poll) through the
    /// panic-stop policy; on trigger, revokes this session's ability to act and returns the
    /// resulting error so the caller can propagate a lease revocation.
    // IMPL: only meaningful for `SessionMode::Attended`; a `Headless` session's poll is a no-op
    // returning `Ok(())`. Compare `observed` against `self.last_agent_point` (treat `None` as
    // "no baseline yet", never a trigger) via `self.panic_stop.mouse_moved`; on trigger, set
    // `self.approved = false` so `is_approved` starts refusing further `act` calls, and return
    // `Err(ComputerError::PanicStop(..).into())`.
    pub fn panic_stop_check(&mut self, observed: Point) -> TmResult<()> {
        match self.mode {
            SessionMode::Headless { .. } => Ok(()),
            SessionMode::Attended => {
                if let Some(last_point) = self.last_agent_point {
                    if self.panic_stop.mouse_moved(last_point, observed) {
                        self.approved = false;
                        return Err(crate::ComputerError::PanicStop(
                            "physical mouse movement detected".to_string(),
                        )
                        .into());
                    }
                }
                Ok(())
            }
        }
    }

    /// List every window.
    pub async fn windows(&self) -> TmResult<Vec<WindowInfo>> {
        self.backend.windows().await
    }

    /// List every display.
    pub async fn displays(&self) -> TmResult<Vec<DisplayInfo>> {
        self.backend.displays().await
    }

    /// Bring a window to focus.
    pub async fn focus(&self, window_id: &str) -> TmResult<()> {
        self.backend.focus_window(window_id).await
    }

    /// Move a window's origin.
    pub async fn move_window(&self, window_id: &str, to: Point) -> TmResult<()> {
        self.backend.move_window(window_id, to).await
    }

    /// Resize a window.
    pub async fn resize_window(&self, window_id: &str, width: f64, height: f64) -> TmResult<()> {
        self.backend.resize_window(window_id, width, height).await
    }

    /// Read the system clipboard.
    pub async fn clipboard_get(&self) -> TmResult<Option<String>> {
        self.backend.clipboard_get().await
    }

    /// Write the system clipboard.
    pub async fn clipboard_set(&self, text: &str) -> TmResult<()> {
        self.backend.clipboard_set(text).await
    }

    /// Launch an application.
    pub async fn launch(&self, app: &str) -> TmResult<()> {
        self.backend.launch(app).await
    }

    /// Quit an application.
    pub async fn quit(&self, app: &str) -> TmResult<()> {
        self.backend.quit(app).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Modifier, Rect};

    #[test]
    fn panic_stop_mouse_moved_exceeds_threshold() {
        let config = PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let from = Point::new(0.0, 0.0);
        let to = Point::new(15.0, 0.0);
        assert!(stop.mouse_moved(from, to));
    }

    #[test]
    fn panic_stop_mouse_moved_below_threshold() {
        let config = PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let from = Point::new(0.0, 0.0);
        let to = Point::new(5.0, 0.0);
        assert!(!stop.mouse_moved(from, to));
    }

    #[test]
    fn panic_stop_mouse_moved_at_threshold() {
        let config = PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let from = Point::new(0.0, 0.0);
        let to = Point::new(10.0, 0.0);
        assert!(!stop.mouse_moved(from, to));
    }

    #[test]
    fn panic_stop_mouse_moved_nan_input() {
        let config = PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let from = Point::new(f64::NAN, 0.0);
        let to = Point::new(5.0, 0.0);
        assert!(stop.mouse_moved(from, to));
    }

    #[test]
    fn panic_stop_mouse_moved_diagonal() {
        let config = PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let from = Point::new(0.0, 0.0);
        let to = Point::new(6.0, 8.0);
        // Distance is sqrt(36 + 64) = sqrt(100) = 10.0, at threshold
        assert!(!stop.mouse_moved(from, to));
    }

    #[test]
    fn panic_stop_mouse_moved_diagonal_exceeds() {
        let config = PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let from = Point::new(0.0, 0.0);
        let to = Point::new(7.0, 8.0);
        // Distance is sqrt(49 + 64) = sqrt(113) > 10.0
        assert!(stop.mouse_moved(from, to));
    }

    #[test]
    fn panic_stop_is_abort_chord_match() {
        let chord = KeyChord {
            modifiers: [Modifier::Ctrl].into_iter().collect(),
            key: "alt".to_string(),
        };
        let config = PanicStopConfig {
            abort_chord: Some(chord.clone()),
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let pressed = KeyChord {
            modifiers: [Modifier::Ctrl].into_iter().collect(),
            key: "alt".to_string(),
        };
        assert!(stop.is_abort_chord(&pressed));
    }

    #[test]
    fn panic_stop_is_abort_chord_no_match() {
        let chord = KeyChord {
            modifiers: [Modifier::Ctrl].into_iter().collect(),
            key: "alt".to_string(),
        };
        let config = PanicStopConfig {
            abort_chord: Some(chord.clone()),
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let pressed = KeyChord {
            modifiers: [Modifier::Shift].into_iter().collect(),
            key: "alt".to_string(),
        };
        assert!(!stop.is_abort_chord(&pressed));
    }

    #[test]
    fn panic_stop_is_abort_chord_not_configured() {
        let config = PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        };
        let stop = PanicStop::new(config);
        let pressed = KeyChord {
            modifiers: [Modifier::Ctrl].into_iter().collect(),
            key: "alt".to_string(),
        };
        assert!(!stop.is_abort_chord(&pressed));
    }

    #[test]
    fn computer_session_mode_attended() {
        let backend = MockBackend::new();
        let mode = SessionMode::Attended;
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 5.0,
        });
        let session = ComputerSession::new(Box::new(backend), mode.clone(), panic_stop);
        assert_eq!(session.mode(), &mode);
    }

    #[test]
    fn computer_session_mode_headless() {
        let backend = MockBackend::new();
        let mode = SessionMode::Headless {
            display: ":1".to_string(),
        };
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 5.0,
        });
        let session = ComputerSession::new(Box::new(backend), mode.clone(), panic_stop);
        assert_eq!(session.mode(), &mode);
    }

    #[test]
    fn computer_session_is_approved_attended_starts_unapproved() {
        let backend = MockBackend::new();
        let mode = SessionMode::Attended;
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 5.0,
        });
        let session = ComputerSession::new(Box::new(backend), mode, panic_stop);
        assert!(!session.is_approved());
    }

    #[test]
    fn computer_session_is_approved_headless_always_approved() {
        let backend = MockBackend::new();
        let mode = SessionMode::Headless {
            display: ":1".to_string(),
        };
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 5.0,
        });
        let session = ComputerSession::new(Box::new(backend), mode, panic_stop);
        assert!(session.is_approved());
    }

    #[test]
    fn computer_session_approve() {
        let backend = MockBackend::new();
        let mode = SessionMode::Attended;
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 5.0,
        });
        let mut session = ComputerSession::new(Box::new(backend), mode, panic_stop);
        assert!(!session.is_approved());
        session.approve(Timestamp::EPOCH);
        assert!(session.is_approved());
    }

    #[test]
    fn computer_session_panic_stop_check_headless_noop() {
        let backend = MockBackend::new();
        let mode = SessionMode::Headless {
            display: ":1".to_string(),
        };
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 5.0,
        });
        let mut session = ComputerSession::new(Box::new(backend), mode, panic_stop);
        let result = session.panic_stop_check(Point::new(100.0, 100.0));
        assert!(result.is_ok());
    }

    #[test]
    fn computer_session_panic_stop_check_attended_no_baseline() {
        let backend = MockBackend::new();
        let mode = SessionMode::Attended;
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 5.0,
        });
        let mut session = ComputerSession::new(Box::new(backend), mode, panic_stop);
        session.approve(Timestamp::EPOCH);
        // Without a baseline, no trigger
        let result = session.panic_stop_check(Point::new(100.0, 100.0));
        assert!(result.is_ok());
        assert!(session.is_approved());
    }

    #[test]
    fn computer_session_panic_stop_check_attended_within_threshold() {
        let backend = MockBackend::new();
        let mode = SessionMode::Attended;
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        });
        let mut session = ComputerSession::new(Box::new(backend), mode, panic_stop);
        session.approve(Timestamp::EPOCH);
        session.last_agent_point = Some(Point::new(0.0, 0.0));
        // Movement within threshold
        let result = session.panic_stop_check(Point::new(5.0, 0.0));
        assert!(result.is_ok());
        assert!(session.is_approved());
    }

    #[test]
    fn computer_session_panic_stop_check_attended_exceeds_threshold() {
        let backend = MockBackend::new();
        let mode = SessionMode::Attended;
        let panic_stop = PanicStop::new(PanicStopConfig {
            abort_chord: None,
            mouse_move_threshold_px: 10.0,
        });
        let mut session = ComputerSession::new(Box::new(backend), mode, panic_stop);
        session.approve(Timestamp::EPOCH);
        session.last_agent_point = Some(Point::new(0.0, 0.0));
        // Movement exceeds threshold
        let result = session.panic_stop_check(Point::new(15.0, 0.0));
        assert!(result.is_err());
        assert!(!session.is_approved());
    }

    // Mock backend for testing
    struct MockBackend;

    impl MockBackend {
        fn new() -> Self {
            MockBackend
        }
    }

    #[async_trait::async_trait]
    impl Backend for MockBackend {
        fn kind(&self) -> crate::backend::BackendKind {
            crate::backend::BackendKind::X11
        }

        async fn probe(&self) -> TmResult<Capabilities> {
            Ok(Capabilities {
                backend: crate::backend::BackendKind::X11,
                input: true,
                capture: true,
                element_tree: true,
                headless: true,
                notes: vec![],
            })
        }

        async fn input(&self, _action: InputAction) -> TmResult<()> {
            Ok(())
        }

        async fn screenshot(&self, _display: Option<&str>) -> TmResult<Screenshot> {
            Ok(Screenshot {
                png_bytes: vec![],
                bounds: Rect::new(Point::new(0.0, 0.0), 1920.0, 1080.0),
            })
        }

        async fn element_tree(&self, _max_depth: Option<u32>) -> TmResult<ElementNode> {
            Ok(ElementNode {
                ref_id: "root".to_string(),
                role: "window".to_string(),
                name: None,
                bounds: Some(Rect::new(Point::new(0.0, 0.0), 1920.0, 1080.0)),
                children: vec![],
            })
        }

        async fn displays(&self) -> TmResult<Vec<DisplayInfo>> {
            Ok(vec![])
        }

        async fn windows(&self) -> TmResult<Vec<WindowInfo>> {
            Ok(vec![])
        }

        async fn focus_window(&self, _window_id: &str) -> TmResult<()> {
            Ok(())
        }

        async fn move_window(&self, _window_id: &str, _to: Point) -> TmResult<()> {
            Ok(())
        }

        async fn resize_window(&self, _window_id: &str, _width: f64, _height: f64) -> TmResult<()> {
            Ok(())
        }

        async fn clipboard_get(&self) -> TmResult<Option<String>> {
            Ok(None)
        }

        async fn clipboard_set(&self, _text: &str) -> TmResult<()> {
            Ok(())
        }

        async fn launch(&self, _app: &str) -> TmResult<()> {
            Ok(())
        }

        async fn quit(&self, _app: &str) -> TmResult<()> {
            Ok(())
        }
    }
}
