//! Interactive processes: a real pseudo-terminal session for driving anything that behaves
//! differently under a real tty than a plain pipe — a REPL, a TUI installer, `git rebase -i`,
//! a password prompt (`docs/audit-2026-09-18-fable.md` B-16, `SPEC.md` §22).
//!
//! An agent limited to `cmd | cat`-style non-interactive execution cannot test `tm`'s own CLI or
//! TUI, cannot answer a prompt, and cannot drive a REPL. `SPEC.md` §22 makes a pseudo-terminal a
//! first-class executor capability for exactly that reason, with the same shape as §19
//! (`tm-browser`) and §20 (`tm-computer`) in a third medium: **give the agent the rendered
//! screen, not the raw byte stream.**
//!
//! [`session::PtySession`] is the session type itself, extracted and generalized from
//! `crates/tm-tui/src/testing.rs`'s `cfg(test)`-gated `PtyHarness` — the seed this crate replaces
//! as the one implementation (`crates/tm-tui`'s own tests now depend on this crate as a
//! dev-dependency rather than carrying a duplicate). [`capability::PtyCapability`] wraps it as a
//! [`tm_types::CapabilityProvider`], exposing `pty.*` tools the same way `tm-browser`/
//! `tm-computer` expose `browser.*`/`computer.*` (`docs/audit-2026-09-18-fable.md` A-01/B-02).
//!
//! # Authority (`SPEC.md` §22.4)
//!
//! `pty.spawn` is gated by `Authority.shell` exactly like `shell.run` (`Action::PtySpawn`) — a
//! pty session can run arbitrary commands just as `shell.run` can. Sending input into an
//! already-spawned session (`pty.write`/`pty.send`/`pty.key`, `Action::PtySend`) is a *distinct,
//! stricter* action class gated by the separate `Authority.shell.pty` grant
//! (`tm_types::authority::ShellAuthority::pty`), not merely `shell.enabled`: a live interactive
//! session can receive arbitrary keystrokes into an already-running process — answering a
//! destructive confirmation prompt, or driving a REPL that itself runs further commands — which
//! SPEC §22.4 names as a materially larger attack surface than one bounded, argv-checked command.
//! Observing or managing a session without injecting input (`pty.screen`/`pty.diff`/
//! `pty.resize`/`pty.wait_exit`, `Action::PtyControl`) needs only `shell.enabled`. See
//! `crates/tm-types/src/action.rs`'s doc comments on these three variants for the full reasoning,
//! and `crates/tm-pty/src/capability.rs`'s module doc for exactly which tool maps to which.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capability;
pub mod session;

pub use capability::{PtyCapability, SessionRegistry};
pub use session::{ArtifactSink, PtyExpectOutcome, PtySession, ASCIICAST_CONTENT_TYPE};
