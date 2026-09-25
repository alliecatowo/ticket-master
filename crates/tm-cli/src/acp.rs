//! `tm acp`: serve the opened project as an ACP agent over stdio, so an ACP-speaking client (Zed,
//! or any other) can connect and hold a conversation against real project state.
//!
//! Modeled on [`crate::mcp`]: the project resolves the way every other subcommand's does
//! ([`crate::project`]), including `--project`, and the server answers from the project's
//! already-open [`tm_core::Store`] through [`tm_acp::ProjectAgentBackend`] rather than a
//! standalone instance. `tm-cli` already depends on `tm-acp` (`crate::dispatch` uses
//! `AcpExecutor`, the client half); this module wires the server half.
//!
//! # Stdout is the protocol
//!
//! Every byte on stdout must be a JSON-RPC message, so nothing here uses [`crate::render`]:
//! diagnostics go to stderr through `tracing`/`eprintln!`.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::oneshot;

use tm_acp::{AcpServer, ProjectAgentBackend};

use crate::args::AcpArgs;
use crate::project::Project;

/// Wraps an [`AsyncRead`] so the first poll that observes EOF (a successful read that fills no
/// bytes) fires `on_eof` once. [`tm_acp::server::Connection`] owns its reader task outright and
/// exposes no way to be notified when it finishes (see that type's own doc comment: dropping is
/// what stops it, not something this crate can await) — this wrapper is `tm-cli`'s side of
/// detecting "stdin closed" without needing a change to `tm-acp` itself.
struct EofNotifyingReader<R> {
    inner: R,
    on_eof: Option<oneshot::Sender<()>>,
}

impl<R: AsyncRead + Unpin> AsyncRead for EofNotifyingReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            if buf.filled().len() == before {
                if let Some(tx) = this.on_eof.take() {
                    let _ = tx.send(());
                }
            }
        }
        poll
    }
}

/// Run `tm acp` until the client closes stdin.
pub async fn acp(_args: &AcpArgs, project: Arc<Project>) -> tm_types::Result<()> {
    // Unlike `crate::mcp::mcp`, this doesn't `set_current_dir`: `tm acp` runs no in-process
    // workers of its own (`ProjectAgentBackend` only reads `project.store.view()`), so there is
    // no tool call or worktree snapshot whose resolution depends on the process's cwd.
    let backend = Arc::new(ProjectAgentBackend::new(
        Arc::clone(&project.store),
        Arc::clone(&project.ids),
    ));
    let server = AcpServer::new(backend);

    let (eof_tx, eof_rx) = oneshot::channel();
    let stdin = EofNotifyingReader {
        inner: tokio::io::stdin(),
        on_eof: Some(eof_tx),
    };
    let _connection = server.attach(stdin, tokio::io::stdout());

    // Wait until the client closes stdin (or the reader task dies for some other reason, in
    // which case the sender simply drops and `eof_rx.await` still resolves).
    let _ = eof_rx.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::EofNotifyingReader;

    /// `EofNotifyingReader` fires its channel on a genuine EOF read (zero bytes filled), and not
    /// before — a partial read that still returns some bytes must not fire it. Exercised over an
    /// in-memory duplex stream standing in for real stdin, which a fast unit test can drive
    /// without spawning the real binary the way [`tm_acp_answers_initialize_over_stdio`] does.
    #[tokio::test]
    async fn eof_notifying_reader_fires_only_on_a_true_end_of_stream_read() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let (eof_tx, mut eof_rx) = tokio::sync::oneshot::channel();
        let mut wrapped = EofNotifyingReader {
            inner: reader,
            on_eof: Some(eof_tx),
        };

        writer.write_all(b"hello").await.expect("write");
        let mut buf = [0u8; 5];
        wrapped.read_exact(&mut buf).await.expect("read some bytes");
        assert_eq!(&buf, b"hello");
        assert!(
            eof_rx.try_recv().is_err(),
            "a partial read must not fire the EOF signal"
        );

        drop(writer);
        let mut trailing = [0u8; 8];
        let n = wrapped.read(&mut trailing).await.expect("read at eof");
        assert_eq!(n, 0, "the peer closing should read as a genuine EOF");

        tokio::time::timeout(Duration::from_secs(1), eof_rx)
            .await
            .expect("no timeout")
            .expect("eof signal fired");
    }

    // The real end-to-end test that spawns the compiled `tm acp` binary lives in
    // `tests/acp_serve.rs`: `CARGO_BIN_EXE_tm` is only defined for integration test targets
    // under `tests/`, not for a unit test module inside `src/`.
}
