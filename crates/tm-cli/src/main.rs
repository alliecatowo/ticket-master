//! `tm`'s entry point: parse argv, install tracing, open the project when the subcommand needs
//! one, route to the matching execution module, render the result, and exit with the code
//! [`tm_cli::render::exit_code`] maps a [`tm_types::TmError`] to. Deliberately thin — every
//! decision beyond "which function do I call and how do I print what it returned" belongs in
//! `tm-cli`'s library modules, not here, so this file stays easy to read end to end.

use std::sync::Arc;

use clap::Parser;
use tm_cli::args::{Cli, Command};
use tm_cli::project;
use tm_cli::render::Renderer;
use tm_cli::{agent, auth, drive, ops, sched, search, serve, tickets, tui, workflow};

/// Parse argv, dispatch, and translate the outcome into a process exit code.
///
/// Install tracing first so `clap` errors print cleanly before running the command, route the
/// parsed CLI to its execution module, and exit with the code the result maps to.
#[tokio::main]
async fn main() {
    install_tracing();

    let cli = Cli::parse();
    let renderer = Renderer::from_flags(cli.global.json, cli.global.quiet, cli.global.no_color);

    let result = dispatch(cli, &renderer).await;
    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            renderer.error(&err);
            std::process::exit(tm_cli::render::exit_code(&err));
        }
    }
}

/// Install the process-wide tracing subscriber.
///
/// Routes logs to stderr so stdout stays clean for piped/JSON output and respects `RUST_LOG`.
fn install_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

/// Route a parsed [`Cli`] to its execution module.
///
/// When `cli.command` is `None`, open the project (via [`project::open_bare`], which
/// auto-bootstraps one when `--project` was not given and none exists yet anywhere above the
/// current directory -- bare `tm` must "just work" in a fresh directory the same way
/// `claude`/`codex` do, not require the user to already know to run `tm init`/`tm attach` first)
/// and either run one prompt to completion (`--prompt`), open the ratatui TUI (a real tty, per
/// [`tui::should_launch`] — D-002: "a mode of the existing binary, entered on the bare-`tm` TTY
/// path"), or fall back to the plain interactive agent loop (`--plain`, `--json`, `--quiet`,
/// `TERM=dumb`, or stdout/stdin not a tty). When `cli.command` is `Some`, delegate to the
/// matching execution module -- every one of those still requires an already-open project and
/// errors precisely as before when none exists, since a user who typed a specific subcommand
/// already knows enough to run `tm init` first. `Init` is the only explicit command that doesn't
/// need an already-open project.
async fn dispatch(cli: Cli, renderer: &Renderer) -> tm_types::Result<()> {
    match cli.command {
        None => {
            let opened = project::open_bare(cli.global.project.as_deref(), renderer)?;
            let project = Arc::new(opened);

            if let Some(prompt) = cli.prompt {
                let mut session = agent::AgentSession::new(project, *renderer);
                return session.run_prompt(&prompt).await;
            }

            if tui::should_launch(&cli.global) {
                tui::run(project).await
            } else {
                let mut session = agent::AgentSession::new(project, *renderer);
                session.run_interactive().await
            }
        }
        Some(Command::Init(args)) => project::init(&args, renderer),
        Some(Command::Attach(args)) => project::attach(&args, renderer),
        Some(Command::Genesis(args)) => project::genesis(&args, renderer),
        Some(Command::Status(args)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            project::status(&opened, &args, renderer)
        }
        Some(Command::Doctor(args)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            let report = project::doctor(&opened, &args, renderer)?;
            if report.all_ok() {
                Ok(())
            } else {
                Err(tm_types::TmError::invariant(
                    "doctor found at least one failing check",
                ))
            }
        }
        Some(Command::Ticket(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            tickets::dispatch_ticket(&cmd, &opened, renderer)
        }
        Some(Command::Dep(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            tickets::dispatch_dep(&cmd, &opened, renderer)
        }
        Some(Command::Milestone(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            tickets::dispatch_milestone(&cmd, &opened, renderer)
        }
        Some(Command::Decision(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            tickets::dispatch_decision(&cmd, &opened, renderer)
        }
        Some(Command::Sched(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            sched::dispatch_sched(&cmd, &opened, renderer)
        }
        Some(Command::Lease(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            sched::dispatch_lease(&cmd, &opened, renderer)
        }
        Some(Command::Run(args)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            sched::run_ticket(&args, &opened, renderer).await
        }
        Some(Command::Search(args)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            search::search(&args, &opened, renderer)
        }
        Some(Command::Symbol(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            search::dispatch_symbol(&cmd, &opened, renderer)
        }
        Some(Command::History(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            search::dispatch_history(&cmd, &opened, renderer)
        }
        Some(Command::Docs(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            ops::dispatch_docs(&cmd, &opened, renderer)
        }
        Some(Command::Provider(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            ops::dispatch_provider(&cmd, &opened, renderer).await
        }
        Some(Command::Auth(args)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let _opened = project::open(&project_dir)?;
            auth::auth(&args, renderer).await
        }
        Some(Command::Harness(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            ops::dispatch_harness(&cmd, &opened, renderer)
        }
        Some(Command::Bench(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            ops::dispatch_bench(&cmd, &opened, renderer).await
        }
        Some(Command::Workflow(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            workflow::dispatch_workflow(&cmd, &opened, renderer)
        }
        Some(Command::Mirror(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            ops::dispatch_mirror(&cmd, &opened, renderer).await
        }
        Some(Command::Templates(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            ops::dispatch_templates(&cmd, &opened, renderer)
        }
        Some(Command::Serve(args)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            serve::serve(&args, &opened, renderer).await
        }
        Some(Command::Events(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            ops::dispatch_events(&cmd, &opened, renderer).await
        }
        Some(Command::Browser(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            drive::dispatch_browser(&cmd, &opened, renderer).await
        }
        Some(Command::Computer(cmd)) => {
            let project_dir = project::resolve_project_dir(cli.global.project.as_deref())?;
            let opened = project::open(&project_dir)?;
            drive::dispatch_computer(&cmd, &opened, renderer).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracing_installs_without_panic() {
        install_tracing();
    }

    #[test]
    fn tracing_idempotent() {
        install_tracing();
        install_tracing();
    }

    #[tokio::test]
    async fn dispatch_init_creates_project() {
        // `path: None` would init `.tm` under the test binary's actual cwd, polluting every
        // other test in this process (and future runs) that expects no project to be found; an
        // explicit tempdir keeps this hermetic.
        let tmp = tempfile::tempdir().unwrap();
        let renderer = Renderer::from_flags(false, false, true);
        let cli = Cli {
            global: tm_cli::args::GlobalOpts {
                json: false,
                quiet: false,
                no_color: true,
                plain: false,
                project: None,
            },
            prompt: None,
            command: Some(Command::Init(tm_cli::args::InitArgs {
                path: Some(tmp.path().to_path_buf()),
            })),
        };

        dispatch(cli, &renderer).await.unwrap();
        assert!(tmp.path().join(".tm").is_dir());
    }

    // `dispatch_bare_agent_no_project_error` and `dispatch_prompt_no_project_error` used to
    // assert that bare `tm`/`tm --prompt` (`command: None`) errored when no project existed yet.
    // That was exactly the UX bug this auto-bootstrap fixes, so both assertions are gone.
    // `dispatch`'s `None` branch now delegates the whole resolve-or-bootstrap decision to
    // `project::open_bare`, which is covered directly and hermetically (no real cwd, no stdin, no
    // provider) by `project::tests::open_bare_*` in `src/project.rs`. Proving the fix by driving
    // `dispatch` itself in-process for the bare (no `--prompt`) arm isn't safe on top of that:
    // past the bootstrap it falls into `session.run_interactive()`, which blocks reading real
    // stdin, and this test binary doesn't control that the way a spawned subprocess with piped,
    // explicitly-closed stdin can. That end-to-end path (including the plain loop actually being
    // reached, and the git-history-assimilation decision) is covered by
    // `tests/bare_bootstrap.rs`'s `bare_tm_bootstraps_a_project_in_a_genuinely_empty_directory`
    // and `bare_tm_assimilates_an_existing_git_repository_with_commits`, which spawn the real
    // compiled `tm` binary the same way `tests/tui_launch.rs` already does.

    #[tokio::test]
    async fn dispatch_ticket_subcommand_no_project_error() {
        let renderer = Renderer::from_flags(false, false, true);
        let cli = Cli {
            global: tm_cli::args::GlobalOpts {
                json: false,
                quiet: false,
                no_color: true,
                plain: false,
                project: None,
            },
            prompt: None,
            command: Some(Command::Ticket(tm_cli::args::TicketCommand::List(
                tm_cli::args::TicketListArgs {
                    state: None,
                    milestone: None,
                    parent: None,
                },
            ))),
        };

        let result = dispatch(cli, &renderer).await;
        match result {
            Err(tm_types::TmError::NotFound { .. }) => (),
            other => panic!(
                "expected NotFound error when no project exists, got {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn dispatch_search_subcommand_no_project_error() {
        let renderer = Renderer::from_flags(false, false, true);
        let cli = Cli {
            global: tm_cli::args::GlobalOpts {
                json: false,
                quiet: false,
                no_color: true,
                plain: false,
                project: None,
            },
            prompt: None,
            command: Some(Command::Search(tm_cli::args::SearchArgs {
                query: "test".to_string(),
                mode: tm_cli::args::SearchMode::Hybrid,
                limit: 20,
            })),
        };

        let result = dispatch(cli, &renderer).await;
        match result {
            Err(tm_types::TmError::NotFound { .. }) => (),
            other => panic!(
                "expected NotFound error when no project exists, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn renderer_from_global_opts() {
        let opts = tm_cli::args::GlobalOpts {
            json: true,
            quiet: true,
            no_color: true,
            plain: false,
            project: None,
        };
        let renderer = Renderer::from_flags(opts.json, opts.quiet, opts.no_color);
        assert!(renderer.is_json());
        assert!(renderer.is_quiet());
        assert!(!renderer.color_enabled());
    }

    #[test]
    fn renderer_respects_no_color() {
        let renderer = Renderer::from_flags(false, false, true);
        assert!(!renderer.color_enabled());
    }

    #[test]
    fn renderer_json_mode() {
        let renderer = Renderer::from_flags(true, false, false);
        assert!(renderer.is_json());
        assert!(!renderer.is_quiet());
    }

    #[test]
    fn renderer_quiet_mode() {
        let renderer = Renderer::from_flags(false, true, false);
        assert!(!renderer.is_json());
        assert!(renderer.is_quiet());
    }
}
