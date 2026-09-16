//! Ticketmaster Browser: no-friction headless browser control.
//!
//! `tm-browser` speaks the Chrome DevTools Protocol directly over a websocket — no Node, no
//! Playwright, no driver binaries. It discovers a system Chromium-family browser
//! ([`discover`]) rather than downloading one, launches it headless with a throwaway profile,
//! and drives it over CDP ([`cdp`]). The default observation an agent reads is a structured
//! accessibility-tree snapshot with stable refs, not a screenshot ([`snapshot`]); agents act by
//! ref through [`session::BrowserSession`], and every navigation and download is gated by
//! `tm_types::Authority.network` with the session's actions recorded as replayable ticket
//! evidence ([`authority`]). See `SPEC.md` §19.
//!
//! Protocol encode/decode and snapshot construction are kept as pure functions
//! (`cdp::encode_request`/`decode_message`, `snapshot::Snapshot::from_ax_tree`) precisely so
//! they can be tested against recorded CDP JSON without a live browser; anything that needs an
//! actual Chromium process is `#[ignore]`d.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod authority;
pub mod cdp;
pub mod discover;
pub mod session;
pub mod snapshot;

pub use authority::{ActionKind, NavigationGuard, SessionTrace, TraceEvent};
pub use cdp::{CdpClient, CdpError, CdpEvent, CdpMessage, CdpRequest, CdpResponse, TargetInfo};
pub use discover::{BrowserKind, DiscoveredBrowser, LaunchConfig};
pub use session::{
    ArtifactRef, ArtifactSink, BrowserSession, BrowserSessionConfig, ConsoleMessage, Cookie,
    NetworkEntry, StorageState, TabId, TabInfo, WaitCondition,
};
pub use snapshot::{AxNode, AxNodeChange, AxRef, Snapshot, SnapshotDiff, SnapshotError};
