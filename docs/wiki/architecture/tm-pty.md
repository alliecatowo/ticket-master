+++
[doc]
id = "wiki/architecture/tm-pty"
mode = "generated"
derived_from = ["crates/tm-pty/src/**"]
+++

# Architecture: tm-pty

## Module tree

- `crates/tm-pty/src/capability.rs`
- `crates/tm-pty/src/lib.rs`
- `crates/tm-pty/src/session.rs`

## Public symbols

### `crates/tm-pty/src/capability.rs`

- `pub struct SessionRegistry`
- `impl SessionRegistry`
  - `pub fn new(clock: Arc<dyn Clock>, sink: Arc<dyn ArtifactSink>) -> Self`
  - `pub async fn close(&self, ticket: &TicketId, session: &SessionId) -> Result<()>`
  - `pub async fn close_all(&self) -> Result<()>`
  - `pub async fn live_count(&self) -> usize`
- `pub struct PtyCapability`
- `impl PtyCapability`
  - `pub fn new(sessions: SessionRegistry) -> Self`
  - `pub async fn close_all(&self) -> Result<()>`

### `crates/tm-pty/src/lib.rs`

- `pub mod capability;`
- `pub mod session;`

### `crates/tm-pty/src/session.rs`

- `pub trait ArtifactSink: Send + Sync`
- `pub const ASCIICAST_CONTENT_TYPE: &str = "application/x-asciicast";`
- `pub enum PtyExpectOutcome`
- `impl PtyExpectOutcome`
  - `pub fn screen(&self) -> &[String]`
  - `pub fn matched(&self) -> bool`
- `pub struct PtySession`
- `impl PtySession`
  - `pub fn spawn(
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        clock: Arc<dyn Clock>,
    ) -> Result<PtySession>`
  - `pub fn screen(&mut self) -> Vec<String>`
  - `pub fn diff(&mut self) -> Vec<(usize, String)>`
  - `pub fn write(&mut self, input: &[u8]) -> Result<()>`
  - `pub fn send(&mut self, text: &str) -> Result<()>`
  - `pub fn key(&mut self, chord: &str) -> Result<()>`
  - `pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()>`
  - `pub fn expect(&mut self, pattern: &str, timeout: Duration) -> PtyExpectOutcome`
  - `pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>>`
  - `pub fn wait_exit(&mut self) -> Result<portable_pty::ExitStatus>`
  - `pub fn wait_exit_bounded(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<portable_pty::ExitStatus>>`
  - `pub fn signal(&mut self, signal: i32) -> Result<()>`
  - `pub fn kill_process_group(&mut self) -> Result<()>`
  - `pub fn size(&self) -> (u16, u16)`
  - `pub fn recording_truncated(&self) -> bool`
  - `pub fn to_asciicast(&self) -> String`
  - `pub fn record(&self, sink: &dyn ArtifactSink) -> Result<tm_types::ArtifactId>`
- `pub(crate) struct FakeArtifactSink`
- `impl FakeArtifactSink`
  - `pub(crate) fn new() -> Self`
