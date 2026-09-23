//! `tm mcp`: serve the opened project to an MCP host (Claude Code) over stdio, so a Claude Code
//! session can read tickets, search the code index, and hand work to tm's workers with
//! `ticket_dispatch`. Set it up once per project with `claude mcp add --transport stdio tm -- tm
//! mcp`; Claude Code starts it in its own working directory, which is how the project resolves.
//!
//! The server itself is [`tm_mcp::server::McpServer`]. What this module adds over the standalone
//! `tm-mcp-server` binary:
//!
//! - The project resolves the way every other subcommand's does ([`crate::project`]), including
//!   `--project`, rather than taking two raw paths.
//! - The server shares the project's already-open [`tm_core::Store`], so tickets it creates and
//!   the in-process scheduler's own writes come from one id source.
//! - Unless `--no-workers`, the scheduler runs in-process, as it does for the TUI and `tm serve`,
//!   so a dispatched ticket actually gets worked.
//! - It speaks newline-delimited JSON-RPC, the MCP stdio transport real hosts use.
//!
//! # Stdout is the protocol
//!
//! Every byte on stdout must be a JSON-RPC message, so nothing here uses [`crate::render`]
//! (`Renderer::note` is a `println!`): diagnostics go to stderr through `tracing` or `eprintln!`.
//! The workers use [`crate::sched::spawn_headless_background_runner`], whose `human_required`
//! handling fails the attempt instead of prompting on stdout and reading the host's next request
//! from stdin as the answer.

use std::sync::Arc;
use std::time::Duration;

use tm_mcp::protocol::Framing;
use tm_mcp::server::McpServer;

use crate::args::McpArgs;
use crate::project::Project;

/// How often `tm mcp`'s workers look for ready tickets, matching `tm serve`.
const WORKER_INTERVAL: Duration = Duration::from_secs(2);

/// Run `tm mcp` until the host closes stdin.
pub async fn mcp(args: &McpArgs, project: Arc<Project>) -> tm_types::Result<()> {
    // The in-process workers resolve their file and shell tool calls against the process's
    // working directory, and their workspace snapshots against `project.root`. The host starts
    // `tm mcp` wherever it happens to be (a subdirectory of the repo, or anywhere at all with
    // `--project`), so pin the two together at the project root before any worker starts.
    std::env::set_current_dir(&project.root)?;

    let server = McpServer::with_store(
        Arc::clone(&project.store),
        project.root.clone(),
        project.state_dir.clone(),
    );

    let workers = if args.no_workers {
        None
    } else {
        match crate::sched::spawn_headless_background_runner(Arc::clone(&project), WORKER_INTERVAL)
        {
            Ok(handle) => Some(handle),
            Err(e) => {
                // stderr: Claude Code shows a server's stderr in its MCP logs, and stdout is the
                // protocol.
                eprintln!(
                    "tm mcp: not working tickets: {e}. Dispatched tickets wait for `tm sched run`."
                );
                None
            }
        }
    };

    let served = server.run_stdio_with(Framing::LineDelimited).await;
    if let Some(workers) = workers {
        workers.abort();
    }
    served
}
