//! Desktop notifications on `approval.requested`/`ticket.escalated`
//! (`docs/audit-2026-09-18-fable.md` M-04, `docs/decisions/D-007-desktop-notifications.md`).
//!
//! This crate is split the way `SPEC.md` §0 asks anything that touches real I/O to be split:
//!
//! * [`decision`] is pure, deterministic, and fully unit-tested: which [`tm_events::EventKind`]s
//!   are worth a notification ([`decision::should_notify`]), how to render one
//!   ([`decision::format_notification`]), and the opt-out gate
//!   ([`decision::notifications_enabled`]).
//! * [`notifier`] defines the [`notifier::Notifier`] trait tests dispatch against (a recording
//!   fake in every test in this crate) plus [`notifier::SystemNotifier`], the real fallback
//!   chain — `notify-rust` on Linux/Windows, `terminal-notifier` on macOS, an OSC 9 escape
//!   sequence to the controlling terminal when neither is available. That real chain is a thin,
//!   deliberately untested edge: see its own module docs for why.
//! * [`watch`] is the only piece that touches an actual project's event log: it polls a
//!   [`tm_events::EventLog`] for newly appended events (the same "second read-only `EventLog`,
//!   polled" pattern `tm-server`'s `AppState::spawn_broadcast_poller` already uses, since
//!   `tm_core::Store` does not expose its internal log or a subscribe hook) and fires
//!   [`notifier::Notifier::notify`] for everything [`decision::should_notify`] selects.

pub mod decision;
pub mod notifier;
pub mod watch;

pub use decision::{
    format_notification, notifications_enabled, notifications_enabled_from_env, should_notify,
    TM_NOTIFY_ENV_VAR,
};
pub use notifier::{Notification, Notifier, NotifyError, SystemNotifier};
pub use watch::{spawn_notification_watcher, DEFAULT_POLL_INTERVAL};
