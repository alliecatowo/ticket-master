//! `tm-mcp-server`: run [`tm_mcp::server::McpServer`] over real process stdio.
//!
//! Takes an already-resolved project root and state directory directly as flags rather than
//! rediscovering them (see `tm_mcp::server`'s module doc comment for why this crate does not
//! depend on `tm-cli::project`'s scope-resolution logic). Speaks `Content-Length` framing and
//! runs no workers; `tm mcp` (in `tm-cli`) is the form for Claude Code and other real MCP hosts:
//! it resolves the project itself, speaks newline-delimited JSON-RPC, and works dispatched
//! tickets in-process.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Serve one Ticketmaster project as an MCP server over stdio (`Content-Length` framing): ticket,
/// search and symbol reads, plus `ticket_dispatch`. Runs no workers; see `tm mcp` for that.
#[derive(Parser, Debug)]
#[command(name = "tm-mcp-server")]
struct Args {
    /// The project workspace root (what `search.*`/`symbol.*` index and query).
    #[arg(long)]
    project_root: PathBuf,

    /// Where this project's durable state lives (`project.db`/`index.db`). Pass the same path as
    /// `--project-root` for a project whose state was opened directly at that directory (as this
    /// crate's own integration tests do); pass an already-resolved `<root>/.tm` (or
    /// `$TM_HOME/projects/<key>`) path for a real repo/global-scope project.
    #[arg(long)]
    state_dir: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let server = match tm_mcp::server::McpServer::new(args.project_root, args.state_dir) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("tm-mcp-server: failed to open project: {e}");
            return ExitCode::FAILURE;
        }
    };

    match server.run_stdio().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tm-mcp-server: {e}");
            ExitCode::FAILURE
        }
    }
}
