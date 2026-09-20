+++
[doc]
id = "wiki/architecture/tm-notify"
mode = "generated"
derived_from = ["crates/tm-notify/src/**"]
+++

# Architecture: tm-notify

## Module tree

- `crates/tm-notify/src/decision.rs`
- `crates/tm-notify/src/lib.rs`
- `crates/tm-notify/src/notifier.rs`
- `crates/tm-notify/src/watch.rs`

## Public symbols

### `crates/tm-notify/src/decision.rs`

- `pub const TM_NOTIFY_ENV_VAR: &str = "TM_NOTIFY";`
- `pub fn notifications_enabled(value: Option<&str>) -> bool`
- `pub fn notifications_enabled_from_env() -> bool`
- `pub fn should_notify(kind: EventKind) -> bool`
- `pub fn format_notification(event: &Event) -> Option<Notification>`

### `crates/tm-notify/src/lib.rs`

- `pub mod decision;`
- `pub mod notifier;`
- `pub mod watch;`

### `crates/tm-notify/src/notifier.rs`

- `pub struct Notification`
- `pub enum NotifyError`
- `pub trait Notifier: Send + Sync`
- `pub struct SystemNotifier;`
- `impl SystemNotifier`
  - `pub fn new() -> Self`

### `crates/tm-notify/src/watch.rs`

- `pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);`
- `pub fn spawn_notification_watcher(
    state_dir: &Path,
    notifier: Arc<dyn Notifier>,
    poll_interval: Duration,
) -> tm_types::Result<tokio::task::JoinHandle<()>>`
