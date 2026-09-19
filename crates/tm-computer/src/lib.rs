//! Computer use: driving a real desktop on macOS and Linux (`SPEC.md` §20).
//!
//! Beyond the browser (`SPEC.md` §19), this crate synthesizes real input and reads a real
//! screen: mouse and keyboard events land on whatever window has focus, and observation prefers
//! the platform accessibility tree ([`AXUIElement`][macos] on macOS, AT-SPI on Linux) with a
//! screenshot fallback when the tree is unavailable or the target is a canvas-style app.
//!
//! **Windows is out of scope.** There is no Windows backend, none is planned, and nothing in
//! this crate should be written as though one might appear.
//!
//! # Backends
//!
//! [`crate::backend::Backend`] is implemented by [`crate::macos::MacosBackend`] (compiled only
//! on macOS) and [`crate::linux::X11Backend`] / a best-effort Wayland portal path (compiled only
//! on Linux). [`crate::backend::select_backend`] picks one automatically from
//! `WAYLAND_DISPLAY` / `DISPLAY` / the compile-time target, overridable with
//! `TM_COMPUTER_BACKEND`. A backend that cannot run fails [`crate::backend::Backend::probe`]
//! with a [`ComputerError`] that names exactly what is missing — a TCC permission, a package, an
//! environment variable — never a generic error or a silently empty screenshot.
//!
//! # Headless, honestly
//!
//! On Linux, `--headless` genuinely works: [`crate::linux::headless`] starts a private `Xvfb`
//! display, runs the session against it, and tears it down, and many sessions can run
//! concurrently on separate display numbers. On macOS there is no supported virtual display —
//! Quartz event injection and screen capture both target the active login session — so macOS
//! sessions are always **attended**: they visibly move the real cursor, they require an explicit
//! approval before they start (see [`crate::session::ComputerSession`]), and they support a
//! **panic stop** ([`crate::session::PanicStop`]) that revokes the session's lease the instant a
//! human touches the physical mouse. Nothing in this crate should suggest otherwise.
//!
//! [macos]: crate::macos

// `unsafe` is denied crate-wide and by default forbidden; `crate::macos` carries a narrow,
// locally-scoped `#![allow(unsafe_code)]` because `AXUIElement*`, `CGWindowListCopyWindowInfo`
// and ImageIO's image destination have no safe Rust wrapper — see that module's doc comment.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod backend;
pub mod capability;
pub mod input;
pub mod session;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

pub use backend::{
    open_selected, select_backend, Backend, BackendKind, Capabilities, SelectionEnv,
};
pub use capability::{ComputerCapability, SessionRegistry as ComputerSessionRegistry};
pub use input::{InputAction, KeyChord, Modifier, MouseButton, Point, ScrollDelta};
pub use session::{ComputerSession, PanicStop, SessionMode};

/// Errors specific to computer-use backends and sessions, before they cross into
/// [`tm_types::TmError`] at the boundary where this crate is called from `tm-agent` or the CLI.
#[derive(Debug, thiserror::Error)]
pub enum ComputerError {
    /// The selected or requested backend cannot run on this machine right now.
    #[error("backend {backend} unavailable: {reason}")]
    BackendUnavailable {
        /// The backend that was requested or selected.
        backend: String,
        /// A precise, actionable explanation — never a generic "not available".
        reason: String,
    },

    /// A required OS permission has not been granted.
    #[error("permission missing: {permission} — grant it at {fix_path}")]
    PermissionMissing {
        /// The permission's name, e.g. "Accessibility" or "Screen Recording".
        permission: String,
        /// The exact System Settings (or equivalent) path that grants it.
        fix_path: String,
    },

    /// `TM_COMPUTER_BACKEND` named a backend that does not exist or cannot run on this OS.
    #[error("unsupported backend override {requested:?} on this platform: {reason}")]
    UnsupportedBackend {
        /// The value of `TM_COMPUTER_BACKEND` that was rejected.
        requested: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A key-chord or coordinate string could not be parsed.
    #[error("parse error: {0}")]
    Parse(String),

    /// An attended session's panic stop fired: the physical mouse moved during an agent-driven
    /// session, or the configured abort chord was pressed. The session's lease is revoked.
    #[error("panic stop: {0}")]
    PanicStop(String),

    /// An attended (macOS) session was started or continued without the required explicit
    /// approval (`SPEC.md` §4.4, §20.3).
    #[error("attended session requires approval before it may act")]
    ApprovalRequired,

    /// The referenced window, display, or element ref does not exist or has gone stale.
    #[error("not found: {0}")]
    NotFound(String),

    /// A live backend operation (input injection, capture, tree walk) failed at the OS level.
    #[error("backend operation failed: {0}")]
    Operation(String),
}

impl From<ComputerError> for tm_types::TmError {
    fn from(e: ComputerError) -> Self {
        match &e {
            ComputerError::BackendUnavailable { .. }
            | ComputerError::PermissionMissing { .. }
            | ComputerError::UnsupportedBackend { .. } => {
                tm_types::TmError::Provider(e.to_string())
            }
            ComputerError::Parse(_) => tm_types::TmError::Parse(e.to_string()),
            ComputerError::PanicStop(_) => tm_types::TmError::InvalidTransition(e.to_string()),
            ComputerError::ApprovalRequired => tm_types::TmError::AuthorityDenied(e.to_string()),
            ComputerError::NotFound(_) => tm_types::TmError::NotFound {
                kind: "computer-element",
                id: e.to_string(),
            },
            ComputerError::Operation(_) => tm_types::TmError::Invariant(e.to_string()),
        }
    }
}
