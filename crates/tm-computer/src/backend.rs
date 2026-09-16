//! The [`Backend`] trait, automatic backend selection, and capability probing.
//!
//! Selection reads three signals — `WAYLAND_DISPLAY`, `DISPLAY`, and the compile-time target —
//! with `TM_COMPUTER_BACKEND` as an explicit override, and is kept as a pure function
//! ([`select_backend`]) over a plain data struct ([`SelectionEnv`]) rather than reading the
//! process environment inline, so it is testable without a real desktop. Probing
//! ([`Backend::probe`]) is the live half: it actually checks TCC grants, `XTEST` availability,
//! or portal support, and returns a [`crate::ComputerError`] that names exactly what is missing
//! rather than a generic failure.

use crate::input::{InputAction, Point, Rect};
use crate::ComputerError;
use serde::{Deserialize, Serialize};
use tm_types::Result as TmResult;

/// Which concrete backend drives the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendKind {
    /// `CGEvent` input, Core Graphics / ScreenCaptureKit capture, `AXUIElement` tree. macOS only.
    Macos,
    /// `XTest` input, `XGetImage` capture, AT-SPI tree where available. Linux only.
    X11,
    /// Portal-based (`RemoteDesktop`, `ScreenCast`) input and capture, AT-SPI tree. Linux only,
    /// best effort: coverage depends on the compositor and installed portal implementation.
    Wayland,
}

impl BackendKind {
    /// The lowercase name used in `TM_COMPUTER_BACKEND` and diagnostics.
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendKind::Macos => "macos",
            BackendKind::X11 => "x11",
            BackendKind::Wayland => "wayland",
        }
    }

    /// Parse a `TM_COMPUTER_BACKEND` value. Case-insensitive; unknown names are `None` so the
    /// caller can render a precise error naming the value it rejected.
    pub fn parse(s: &str) -> Option<BackendKind> {
        match s.to_ascii_lowercase().as_str() {
            "macos" => Some(BackendKind::Macos),
            "x11" => Some(BackendKind::X11),
            "wayland" => Some(BackendKind::Wayland),
            _ => None,
        }
    }
}

/// The environment signals [`select_backend`] consults, captured as data rather than read
/// inline so selection stays a pure, unit-testable function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionEnv {
    /// The value of `TM_COMPUTER_BACKEND`, if set.
    pub tm_computer_backend: Option<String>,
    /// The value of `WAYLAND_DISPLAY`, if set.
    pub wayland_display: Option<String>,
    /// The value of `DISPLAY`, if set.
    pub display: Option<String>,
    /// Whether the compile target is macOS (`cfg!(target_os = "macos")`).
    pub is_macos: bool,
    /// Whether the compile target is Linux (`cfg!(target_os = "linux")`).
    pub is_linux: bool,
}

impl SelectionEnv {
    /// Capture the real process environment and compile target.
    pub fn from_process() -> SelectionEnv {
        SelectionEnv {
            tm_computer_backend: std::env::var("TM_COMPUTER_BACKEND").ok(),
            wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
            display: std::env::var("DISPLAY").ok(),
            is_macos: cfg!(target_os = "macos"),
            is_linux: cfg!(target_os = "linux"),
        }
    }
}

/// Select a [`BackendKind`] from `env`, honoring `TM_COMPUTER_BACKEND` as an override.
///
/// Pure: no env reads, no I/O, total over any [`SelectionEnv`].
pub fn select_backend(env: &SelectionEnv) -> Result<BackendKind, ComputerError> {
    if let Some(requested) = &env.tm_computer_backend {
        let kind =
            BackendKind::parse(requested).ok_or_else(|| ComputerError::UnsupportedBackend {
                requested: requested.clone(),
                reason: "not a recognized backend name (expected macos, x11 or wayland)"
                    .to_string(),
            })?;
        let unavailable_reason = match kind {
            BackendKind::Macos if !env.is_macos => {
                Some("macos is only available when compiled for macOS")
            }
            BackendKind::X11 | BackendKind::Wayland if !env.is_linux => {
                Some("x11 and wayland are only available when compiled for Linux")
            }
            _ => None,
        };
        return match unavailable_reason {
            Some(reason) => Err(ComputerError::UnsupportedBackend {
                requested: requested.clone(),
                reason: reason.to_string(),
            }),
            None => Ok(kind),
        };
    }

    if env.is_macos {
        return Ok(BackendKind::Macos);
    }

    if env.is_linux {
        return Ok(if env.wayland_display.is_some() {
            BackendKind::Wayland
        } else {
            // Also the fallback when neither is set: `linux::headless` can start its own Xvfb
            // and populate DISPLAY itself, so absence of a live display isn't a selection-time
            // failure — that's `Backend::probe`'s job.
            BackendKind::X11
        });
    }

    Err(ComputerError::BackendUnavailable {
        backend: "none".to_string(),
        reason: "this platform is neither macOS nor Linux; Windows is out of scope".to_string(),
    })
}

/// What a backend can currently do on this machine, and — when something is missing — exactly
/// why, so a caller never has to guess between "not implemented" and "not permitted".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The backend these capabilities describe.
    pub backend: BackendKind,
    /// Input injection (click, type, key chords, scroll) is available.
    pub input: bool,
    /// Screen or window capture is available.
    pub capture: bool,
    /// The accessibility element tree (`AXUIElement` / AT-SPI) is available; when `false`,
    /// [`crate::session::ComputerSession::snapshot`] falls back to a screenshot.
    pub element_tree: bool,
    /// A fully headless session (no attached physical display, no attended human) is possible.
    /// Always `false` on macOS per `SPEC.md` §20.3, regardless of any other capability.
    pub headless: bool,
    /// Human-readable notes on anything missing and how to fix it — TCC grants, packages,
    /// environment variables — one entry per missing piece, each ending with the exact fix.
    pub notes: Vec<String>,
}

/// A display, for [`Backend::screenshot`] and [`Backend::windows`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// A stable identifier for this display within the session.
    pub id: String,
    /// Its bounds in screen coordinates.
    pub bounds: Rect,
    /// Whether this is the primary display.
    pub primary: bool,
}

/// A top-level window, for [`Backend::windows`] and window management.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// A stable identifier for this window within the session.
    pub id: String,
    /// The owning application's name.
    pub app_name: String,
    /// The window title, if any.
    pub title: Option<String>,
    /// Its bounds in screen coordinates.
    pub bounds: Rect,
    /// Whether this window currently has focus.
    pub focused: bool,
}

/// One node of the accessibility element tree (`AXUIElement` on macOS, AT-SPI on Linux).
///
/// Nodes carry a `ref_id` stable for the lifetime of a [`crate::session::ComputerSession`]
/// snapshot, so an [`InputAction`] can target `InputTarget::ElementRef` instead of raw
/// coordinates that go stale the moment a window moves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementNode {
    /// Stable ref for this snapshot; used by `InputTarget::ElementRef`.
    pub ref_id: String,
    /// The platform accessibility role (e.g. `AXButton`, `push button`).
    pub role: String,
    /// The accessible name/label, if any.
    pub name: Option<String>,
    /// The element's bounds in screen coordinates, if it has geometry.
    pub bounds: Option<Rect>,
    /// Child elements.
    pub children: Vec<ElementNode>,
}

/// Image bytes returned by [`Backend::screenshot`], already PNG-encoded so callers never need a
/// platform-specific decoder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screenshot {
    /// PNG-encoded image bytes.
    pub png_bytes: Vec<u8>,
    /// The captured region's bounds in screen coordinates.
    pub bounds: Rect,
}

/// A live backend: the seam between [`crate::session::ComputerSession`] and one platform's real
/// input, capture and accessibility APIs.
///
/// Every method that touches the OS is `async` because capture and portal round-trips are not
/// instantaneous; the pure input model in [`crate::input`] stays synchronous and is validated
/// before any of these are called.
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    /// Which backend this is.
    fn kind(&self) -> BackendKind;

    /// Check what this backend can currently do, and explain anything it can't.
    // IMPL: for Macos, check the Accessibility and Screen Recording TCC grants (see
    // `macos::check_accessibility_permission` / `check_screen_recording_permission`) and set
    // `headless: false` unconditionally with a note explaining why (SPEC.md §20.3). For X11,
    // check `XTEST` extension presence and `DISPLAY` connectivity; `headless: true` because
    // `linux::headless` can always provision an `Xvfb`. For Wayland, check portal availability
    // via the session bus and note that support is compositor-dependent. Never returns an Err:
    // an unavailable capability is represented in the struct, not as a probe failure.
    async fn probe(&self) -> TmResult<Capabilities>;

    /// Synthesize one input action.
    // IMPL: validate the action's target is within a known display/window bound (via
    // `input::clamp_point` for `Point` targets, or resolving `ElementRef` against the last
    // snapshot), then translate to the platform call (CGEvent post / XTestFake*Event / portal
    // RemoteDesktop request). Returns `ComputerError::NotFound` for a stale element ref,
    // `ComputerError::Operation` for an injection failure, `ComputerError::PermissionMissing`
    // when the OS refuses the injection outright.
    async fn input(&self, action: InputAction) -> TmResult<()>;

    /// Capture `display` (by [`DisplayInfo::id`]), or the primary display when `None`.
    // IMPL: ScreenCaptureKit with CGDisplayCreateImage fallback on macOS; XGetImage/SHM on X11;
    // portal ScreenCast (PipeWire) on Wayland. A missing Screen Recording grant or portal denial
    // must come back as `ComputerError::PermissionMissing`, never an empty or black image.
    async fn screenshot(&self, display: Option<&str>) -> TmResult<Screenshot>;

    /// Walk the platform accessibility tree from the root, to the given depth (`None` =
    /// unbounded).
    // IMPL: AXUIElement tree walk from the system-wide or focused-app root on macOS; AT-SPI
    // registry walk on Linux where available. Returns `ComputerError::BackendUnavailable` when
    // the platform has no accessibility tree reachable right now (e.g. AT-SPI not running) so
    // `ComputerSession::snapshot` can fall back to a screenshot instead of erroring the caller.
    async fn element_tree(&self, max_depth: Option<u32>) -> TmResult<ElementNode>;

    /// List every display.
    async fn displays(&self) -> TmResult<Vec<DisplayInfo>>;

    /// List every top-level window.
    async fn windows(&self) -> TmResult<Vec<WindowInfo>>;

    /// Bring `window_id` to focus.
    async fn focus_window(&self, window_id: &str) -> TmResult<()>;

    /// Move `window_id` so its origin is at `to`.
    async fn move_window(&self, window_id: &str, to: Point) -> TmResult<()>;

    /// Resize `window_id` to `width` x `height`.
    async fn resize_window(&self, window_id: &str, width: f64, height: f64) -> TmResult<()>;

    /// Read the system clipboard as text.
    async fn clipboard_get(&self) -> TmResult<Option<String>>;

    /// Write `text` to the system clipboard.
    async fn clipboard_set(&self, text: &str) -> TmResult<()>;

    /// Launch an application by name or bundle/executable identifier.
    async fn launch(&self, app: &str) -> TmResult<()>;

    /// Quit a running application by name.
    async fn quit(&self, app: &str) -> TmResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        tm_computer_backend: Option<&str>,
        wayland_display: Option<&str>,
        display: Option<&str>,
        is_macos: bool,
        is_linux: bool,
    ) -> SelectionEnv {
        SelectionEnv {
            tm_computer_backend: tm_computer_backend.map(String::from),
            wayland_display: wayland_display.map(String::from),
            display: display.map(String::from),
            is_macos,
            is_linux,
        }
    }

    #[test]
    fn backend_kind_as_str_round_trips_through_parse() {
        for kind in [BackendKind::Macos, BackendKind::X11, BackendKind::Wayland] {
            assert_eq!(BackendKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn backend_kind_parse_is_case_insensitive() {
        assert_eq!(BackendKind::parse("X11"), Some(BackendKind::X11));
        assert_eq!(BackendKind::parse("WAYLAND"), Some(BackendKind::Wayland));
    }

    #[test]
    fn backend_kind_parse_rejects_unknown_name() {
        assert_eq!(BackendKind::parse("windows"), None);
    }

    #[test]
    fn macos_target_always_selects_macos_backend() {
        let e = env(None, None, None, true, false);
        assert_eq!(select_backend(&e).unwrap(), BackendKind::Macos);
    }

    #[test]
    fn linux_with_wayland_display_selects_wayland() {
        let e = env(None, Some(":0"), Some(":1"), false, true);
        assert_eq!(select_backend(&e).unwrap(), BackendKind::Wayland);
    }

    #[test]
    fn linux_with_only_display_selects_x11() {
        let e = env(None, None, Some(":1"), false, true);
        assert_eq!(select_backend(&e).unwrap(), BackendKind::X11);
    }

    #[test]
    fn linux_with_neither_display_still_selects_x11_for_headless_xvfb() {
        let e = env(None, None, None, false, true);
        assert_eq!(select_backend(&e).unwrap(), BackendKind::X11);
    }

    #[test]
    fn non_macos_non_linux_is_backend_unavailable() {
        let e = env(None, None, None, false, false);
        assert!(matches!(
            select_backend(&e),
            Err(ComputerError::BackendUnavailable { .. })
        ));
    }

    #[test]
    fn override_selects_named_backend_when_valid_on_platform() {
        let e = env(Some("x11"), None, None, false, true);
        assert_eq!(select_backend(&e).unwrap(), BackendKind::X11);
    }

    #[test]
    fn override_with_unknown_name_is_unsupported_backend() {
        let e = env(Some("windows"), None, None, true, false);
        let err = select_backend(&e).unwrap_err();
        assert!(
            matches!(err, ComputerError::UnsupportedBackend { requested, .. } if requested == "windows")
        );
    }

    #[test]
    fn override_requesting_macos_on_linux_is_unsupported_backend() {
        let e = env(Some("macos"), None, None, false, true);
        assert!(matches!(
            select_backend(&e),
            Err(ComputerError::UnsupportedBackend { .. })
        ));
    }

    #[test]
    fn override_requesting_x11_on_macos_is_unsupported_backend() {
        let e = env(Some("x11"), None, None, true, false);
        assert!(matches!(
            select_backend(&e),
            Err(ComputerError::UnsupportedBackend { .. })
        ));
    }

    #[test]
    fn override_takes_precedence_over_wayland_display() {
        let e = env(Some("x11"), Some(":0"), None, false, true);
        assert_eq!(select_backend(&e).unwrap(), BackendKind::X11);
    }
}
