//! `tm`'s entry point: parse argv, install tracing, open the project when the subcommand needs
//! one, route to the matching execution module, render the result, and exit with the code
//! [`tm_cli::render::exit_code`] maps a [`tm_types::TmError`] to. Deliberately thin — every
//! decision beyond "which function do I call and how do I print what it returned" belongs in
//! `tm-cli`'s library modules, not here, so this file stays easy to read end to end.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tm_cli::args::{Cli, Command};
use tm_cli::project;
use tm_cli::render::Renderer;
use tm_cli::{
    acp, agent, auth, drive, mcp, ops, sched, search, serve, stats, tickets, tui, wiki, workflow,
};

#[cfg(feature = "otel")]
mod otel;

/// Parse argv, dispatch, and translate the outcome into a process exit code.
///
/// Loads a `.env` file from the current directory first, if one exists, so provider credentials
/// (`DEVPASS_API_KEY` and friends) don't need to be exported by hand every session -- see
/// [`load_dotenv`]. Then installs tracing so `clap` errors print cleanly before running the
/// command, routes the parsed CLI to its execution module, and exits with the code the result
/// maps to.
#[tokio::main]
async fn main() {
    load_dotenv();
    let tracing_guard = install_tracing();

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // `err.print()` matches `clap::Error::exit`'s own formatting (colored, routed to
            // stdout for `--help`/`--version`, stderr otherwise) without `exit`'s hardcoded exit
            // code -- `args::usage_exit_code` picks the code instead, so a real usage error (an
            // unknown flag, a missing value, and so on) exits distinctly from
            // `TmError::TurnFailed`'s 2 (a real, failed agent turn) or `TmError::BudgetExhausted`'s
            // 4, letting a caller scripting against `tm -p`'s exit codes tell them apart.
            let _ = err.print();
            std::process::exit(tm_cli::args::usage_exit_code(&err));
        }
    };
    let renderer = Renderer::from_flags(cli.global.json, cli.global.quiet, cli.global.no_color);

    let result = dispatch(cli, &renderer).await;
    let code = match result {
        Ok(()) => 0,
        Err(err) => {
            renderer.error(&err);
            tm_cli::render::exit_code(&err)
        }
    };
    // `std::process::exit` below skips `Drop`, so flush explicitly rather than relying on a
    // guard's destructor -- a no-op when the `otel` feature is off or `TM_OTEL_ENDPOINT` unset.
    drop(tracing_guard);
    std::process::exit(code);
}

/// Guard returned by [`install_tracing`], dropped explicitly (not via RAII -- see `main`'s
/// comment) right before `std::process::exit`. Flushes the OpenTelemetry tracer provider, if one
/// was installed, so batched spans reach the collector before the process ends; a no-op
/// otherwise.
#[derive(Default)]
struct TracingGuard {
    #[cfg(feature = "otel")]
    otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.otel_provider.take() {
            // Bounded, not `shutdown()`'s unbounded wait: a stale/unreachable `TM_OTEL_ENDPOINT`
            // should cost this exit a few seconds at most, not hang it (see
            // `otel::EXPORT_TIMEOUT`'s doc comment).
            if let Err(err) = provider.shutdown_with_timeout(otel::EXPORT_TIMEOUT) {
                eprintln!("tm: failed to flush OpenTelemetry tracer provider on shutdown: {err}");
            }
        }
    }
}

/// Load a `.env` file from the current directory into this process's environment, if one exists.
///
/// Never overrides a variable already set in the real environment (`dotenvy::dotenv`'s own
/// default) -- an explicit `export`/`env FOO=bar tm ...` always wins over `.env`, matching every
/// other dotenv tool's convention. Looks only in the current directory, not upward through parent
/// directories the way [`crate::project::locate`] walks for `.tm/` -- conflating the two search
/// rules would make `.env` resolution as surprising as the `$HOME/.tm` collision D-003 exists to
/// prevent, for a feature that is supposed to remove surprise, not add it. Absent file, a
/// permission error, or malformed content are all silently ignored: `.env` support must never be
/// the reason a real invocation (which has no need for one) fails to start.
fn load_dotenv() {
    let _ = dotenvy::dotenv();
}

/// Rewrite a bare filesystem [`TmError::Io`] from [`project::attach`] into a plain-English
/// message naming the path that was tried, instead of the raw OS errno text (e.g. "No such
/// file or directory (os error 2)"). `attempted_path` is the argument the user actually typed
/// (or `.` if none), which `attach`'s own `std::io::Error` never carries.
///
/// Only rewrites when `attempted_path` itself doesn't exist on disk: `attach` does plenty of
/// I/O *after* the path resolves (canonicalizing a subdirectory, creating `.tm/`, promoting a
/// global session, indexing the repo), and a failure there — permission denied, disk full, a
/// half-written project after `.tm/` was already created — would be actively misleading if
/// reported as "the path does not exist" when it plainly does. `From<std::io::Error>` has
/// already collapsed the original error into a string by the time it reaches here, so the
/// path's own existence is the only signal left to gate on; any other `TmError::Io`, and every
/// other variant, passes through unchanged.
fn wrap_attach_io_error(
    err: tm_types::TmError,
    attempted_path: &std::path::Path,
) -> tm_types::TmError {
    match err {
        tm_types::TmError::Io(_) if !attempted_path.exists() => tm_types::TmError::Io(format!(
            "The path {} does not exist. Check the path and try again.",
            attempted_path.display()
        )),
        other => other,
    }
}

/// Install the process-wide tracing subscriber.
///
/// Routes logs to stderr so stdout stays clean for piped/JSON output and respects `RUST_LOG`.
/// When compiled with the `otel` feature and `TM_OTEL_ENDPOINT` is set, also adds an OTLP-over-
/// HTTP export layer alongside the stderr one (see `otel::install`, including that module's doc
/// comment on what actually reaches a collector today) -- otherwise (feature off, or the feature
/// on but the env var unset) this is exactly the stderr-only subscriber it has always been.
fn install_tracing() -> TracingGuard {
    #[cfg(feature = "otel")]
    if let Some(endpoint) = otel::endpoint_from_env() {
        match otel::install(&endpoint) {
            Ok(provider) => {
                return TracingGuard {
                    otel_provider: Some(provider),
                }
            }
            Err(err) => eprintln!(
                "tm: {var}={endpoint:?} set but OpenTelemetry exporter failed to initialize: \
                 {err}; continuing with stderr logging only",
                var = otel::TM_OTEL_ENDPOINT_VAR
            ),
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    TracingGuard::default()
}

/// Route a parsed [`Cli`] to its execution module.
///
/// When `cli.command` is `None`, open the project via [`project::open_bare`] (D-003:
/// [`project::resolve_scope`] against the real current directory, silently creating an empty
/// project under `$TM_HOME` — never in the workspace, never by assimilating a git repo — the one
/// time it resolves to global scope with nothing there yet) and either run one prompt to
/// completion (`--prompt`, or a positional prompt without a TTY), open the ratatui TUI (a real tty, per [`tui::should_launch`] — D-002:
/// "a mode of the existing binary, entered on the bare-`tm` TTY path"), or fall back to the plain
/// interactive agent loop (`--plain`, `--json`, `--quiet`, `TERM=dumb`, or stdout/stdin not a
/// tty). When `cli.command` is `Some`, delegate to the matching execution module -- every one of
/// those (via [`project::open_for_command`]) still requires an already-open project in either
/// scope and errors `NotFound` precisely as before when none exists, since a user who typed a
/// specific subcommand already knows enough to run `tm init` first; subcommands never create
/// state on their own. `Init` and `Project` are the only commands that don't need an
/// already-open project (`Project::Show` reads scope resolution directly; `Project::List` reads
/// `$TM_HOME` directly).
async fn dispatch(cli: Cli, renderer: &Renderer) -> tm_types::Result<()> {
    match cli.command {
        None => {
            let opened = project::open_bare(cli.global.project.as_deref(), renderer)?;
            let project = Arc::new(opened);

            let resumed = match cli.resume.as_deref() {
                Some("") => return agent::print_sessions(&project, renderer),
                Some(id) => Some(agent::AgentSession::resume(
                    project.clone(),
                    *renderer,
                    &tm_types::SessionId::new(id)?,
                )?),
                None if cli.continue_session => Some(agent::AgentSession::resume_latest(
                    project.clone(),
                    *renderer,
                )?),
                None => None,
            };

            if cli.prompt {
                let prompt = cli.prompt_text.unwrap_or_default();
                let mut session =
                    resumed.unwrap_or_else(|| agent::AgentSession::new(project.clone(), *renderer));
                return session.run_prompt(&prompt).await;
            }

            if tui::should_launch(&cli.global) {
                tui::run(project, resumed, cli.prompt_text).await
            } else if let Some(prompt) = cli.prompt_text {
                let mut session =
                    resumed.unwrap_or_else(|| agent::AgentSession::new(project.clone(), *renderer));
                session.run_prompt(&prompt).await
            } else {
                let mut session =
                    resumed.unwrap_or_else(|| agent::AgentSession::new(project.clone(), *renderer));
                session.run_interactive().await
            }
        }
        Some(Command::Init(args)) => project::init(&args, renderer),
        Some(Command::Attach(args)) => {
            let attempted_path = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
            project::attach(&args, renderer)
                .map_err(|err| wrap_attach_io_error(err, &attempted_path))
        }
        Some(Command::Genesis(args)) => project::genesis(&args, renderer),
        Some(Command::Status(args)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            project::status(&opened, &args, renderer)
        }
        Some(Command::Stats(args)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            stats::dispatch_stats(&args, &opened, renderer)
        }
        Some(Command::Doctor(args)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            let report = project::doctor(&opened, &args, renderer)?;
            if report.all_ok() {
                Ok(())
            } else {
                let failed: Vec<&str> = report
                    .checks
                    .iter()
                    .filter(|c| !c.ok && c.required)
                    .map(|c| c.name.as_str())
                    .collect();
                Err(tm_types::TmError::CheckFailed(format!(
                    "Some checks failed: {}",
                    failed.join(", ")
                )))
            }
        }
        Some(Command::Ticket(cmd)) => {
            // Creating a ticket is an explicit request to have a project, so it starts one the
            // way bare `tm` does (global scope, nothing written into the workspace; D-003).
            // Every other ticket verb needs an existing project with tickets in it.
            let opened = if matches!(cmd, tm_cli::args::TicketCommand::New(_)) {
                project::open_bare(cli.global.project.as_deref(), renderer)?
            } else {
                project::open_for_command(cli.global.project.as_deref())?
            };
            tickets::dispatch_ticket(&cmd, &opened, renderer)
        }
        Some(Command::Tickets(args)) => {
            // `tm tickets` is `claude agents` (D-019 §2): on a real terminal it opens the TUI on
            // the tickets screen, starting a project the way bare `tm` does. `--json` (or a
            // non-tty) only lists, and never creates a project just to say there are no tickets.
            if !cli.global.json && tui::should_launch(&cli.global) {
                let opened = project::open_bare(cli.global.project.as_deref(), renderer)?;
                return tui::run_tickets(Arc::new(opened)).await;
            }
            let opened = match project::open_for_command(cli.global.project.as_deref()) {
                Ok(project) => Some(project),
                Err(tm_types::TmError::NotFound { .. }) => None,
                Err(e) => return Err(e),
            };
            tickets::tickets_list(&args, opened.as_ref(), renderer)
        }
        Some(Command::Dep(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            tickets::dispatch_dep(&cmd, &opened, renderer)
        }
        Some(Command::Milestone(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            tickets::dispatch_milestone(&cmd, &opened, renderer)
        }
        Some(Command::Decision(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            tickets::dispatch_decision(&cmd, &opened, renderer)
        }
        Some(Command::Sched(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            sched::dispatch_sched(&cmd, &opened, renderer).await
        }
        Some(Command::Lease(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            sched::dispatch_lease(&cmd, &opened, renderer)
        }
        Some(Command::Run(args)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            sched::run_ticket(&args, &opened, renderer).await
        }
        Some(Command::Search(args)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            search::search(&args, &opened, renderer)
        }
        Some(Command::Symbol(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            search::dispatch_symbol(&cmd, &opened, renderer)
        }
        Some(Command::History(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            search::dispatch_history(&cmd, &opened, renderer)
        }
        Some(Command::Docs(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            ops::dispatch_docs(&cmd, &opened, renderer)
        }
        Some(Command::Provider(cmd)) => {
            // Providers are machine-level; a project, if there is one, only adds its role table.
            let opened = match project::open_for_command(cli.global.project.as_deref()) {
                Ok(project) => Some(project),
                Err(tm_types::TmError::NotFound { .. }) => None,
                Err(e) => return Err(e),
            };
            ops::dispatch_provider(&cmd, opened.as_ref(), renderer).await
        }
        Some(Command::Auth(args)) => auth::auth(&args, renderer).await,
        Some(Command::Harness(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            ops::dispatch_harness(&cmd, &opened, renderer)
        }
        Some(Command::Bench(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            ops::dispatch_bench(&cmd, &opened, renderer).await
        }
        Some(Command::Workflow(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            workflow::dispatch_workflow(&cmd, &opened, renderer)
        }
        Some(Command::Mirror(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            ops::dispatch_mirror(&cmd, &opened, renderer).await
        }
        Some(Command::Templates(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            ops::dispatch_templates(&cmd, &opened, renderer)
        }
        Some(Command::Serve(args)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            serve::serve(&args, std::sync::Arc::new(opened), renderer).await
        }
        Some(Command::Mcp(args)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            mcp::mcp(&args, std::sync::Arc::new(opened)).await
        }
        Some(Command::Acp(args)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            acp::acp(&args, std::sync::Arc::new(opened)).await
        }
        Some(Command::Events(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            ops::dispatch_events(&cmd, &opened, renderer).await
        }
        Some(Command::Browser(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            drive::dispatch_browser(&cmd, &opened, renderer).await
        }
        Some(Command::Computer(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            drive::dispatch_computer(&cmd, &opened, renderer).await
        }
        Some(Command::Project(cmd)) => {
            project::dispatch_project(&cmd, cli.global.project.as_deref(), renderer)
        }
        Some(Command::Wiki(cmd)) => {
            let opened = project::open_for_command(cli.global.project.as_deref())?;
            wiki::dispatch_wiki(&cmd, &opened, renderer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TM_HOME` is process-global; serialize every test that touches it so it can't race a
    /// sibling test in this binary reading or changing it concurrently. A `tokio::sync::Mutex`,
    /// not `std::sync::Mutex`: the guard is held across `dispatch(..).await` below, and holding a
    /// std mutex guard across an await point is a real bug (it can block the executor thread),
    /// not just a clippy nit.
    static ENV_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn tracing_installs_without_panic() {
        install_tracing();
    }

    #[test]
    fn tracing_idempotent() {
        install_tracing();
        install_tracing();
    }

    #[test]
    fn wrap_attach_io_error_names_the_path_in_plain_english() {
        let raw = tm_types::TmError::Io("No such file or directory (os error 2)".to_string());
        let wrapped = wrap_attach_io_error(raw, std::path::Path::new("/nonexistent/path"));
        let message = wrapped.to_string();
        assert!(
            message.contains("The path /nonexistent/path does not exist"),
            "unexpected message: {message}"
        );
        assert!(
            !message.contains("os error"),
            "raw errno text leaked into the message: {message}"
        );
    }

    #[test]
    fn wrap_attach_io_error_leaves_an_existing_path_untouched() {
        // A raw `Io` error surfaced after the path itself resolved (permission denied mid-
        // indexing, for example) must not be reported as "the path does not exist" when it
        // plainly does.
        let tmp = tempfile::tempdir().unwrap();
        let raw = tm_types::TmError::Io("permission denied (os error 13)".to_string());
        let wrapped = wrap_attach_io_error(raw, tmp.path());
        assert!(
            wrapped.to_string().contains("permission denied"),
            "an existing path's Io error should pass through unchanged: {wrapped}"
        );
    }

    #[test]
    fn wrap_attach_io_error_leaves_non_io_errors_untouched() {
        let original = tm_types::TmError::not_found("project", "P-1");
        let message_before = original.to_string();
        let wrapped = wrap_attach_io_error(original, std::path::Path::new("/wherever"));
        assert_eq!(wrapped.to_string(), message_before);
    }

    #[tokio::test]
    async fn dispatch_attach_on_a_nonexistent_path_gives_a_plain_english_error() {
        let renderer = Renderer::from_flags(false, false, true);
        let cli = Cli {
            global: tm_cli::args::GlobalOpts {
                json: false,
                quiet: false,
                no_color: true,
                plain: false,
                project: None,
            },
            prompt: false,
            prompt_text: None,
            continue_session: false,
            resume: None,
            command: Some(Command::Attach(tm_cli::args::AttachArgs {
                path: Some(PathBuf::from("/nonexistent/path/that/does/not/exist")),
            })),
        };

        let err = dispatch(cli, &renderer).await.unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("does not exist"),
            "unexpected message: {message}"
        );
        assert!(
            !message.contains("os error"),
            "raw errno text leaked into the message: {message}"
        );
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
            prompt: false,
            prompt_text: None,
            continue_session: false,
            resume: None,
            // `fresh: true` bypasses the D-003 Phase 1-C promotion check entirely (it never reads
            // `$TM_HOME`), keeping this test hermetic without an explicit `TM_HOME` override —
            // promotion itself is covered by `project.rs`'s own tests.
            command: Some(Command::Init(tm_cli::args::InitArgs {
                path: Some(tmp.path().to_path_buf()),
                fresh: true,
                no_index: true,
            })),
        };

        dispatch(cli, &renderer).await.unwrap();
        assert!(tmp.path().join(".tm").is_dir());
    }

    // `dispatch_bare_agent_no_project_error` and `dispatch_prompt_no_project_error` used to
    // assert that bare `tm`/`tm --prompt` (`command: None`) errored when no project existed yet.
    // D-003 replaced that error with a silent global-scope bootstrap, so both assertions are
    // gone. `dispatch`'s `None` branch now delegates entirely to `project::open_bare`, which is
    // covered directly and hermetically (no real cwd, no stdin, no provider) by
    // `project::tests::open_bare_*` in `src/project.rs`. Proving the fix by driving `dispatch`
    // itself in-process for the bare (no `--prompt`) arm isn't safe on top of that: past the
    // open it falls into `session.run_interactive()`, which blocks reading real stdin, and this
    // test binary doesn't control that the way a spawned subprocess with piped, explicitly-closed
    // stdin can. That end-to-end path (including the plain loop actually being reached, and that
    // bare `tm` genuinely never assimilates a git repo anymore) is covered by
    // `tests/bare_scope.rs`, which spawns the real compiled `tm` binary the same way
    // `tests/tui_launch.rs` already does.

    #[tokio::test]
    async fn dispatch_ticket_subcommand_no_project_error() {
        // D-003: with no `--project` and no located `.tm/`, `open_for_command` falls back to
        // checking `$TM_HOME` for an existing global project before erroring `NotFound` — so this
        // needs a real, empty `TM_HOME` of its own rather than whatever the real developer
        // running this test happens to have at `~/.tm`.
        let _guard = ENV_GUARD.lock().await;
        let tm_home = tempfile::tempdir().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tm_home.path()).unwrap();
        std::env::set_var("TM_HOME", tm_home.path());

        let renderer = Renderer::from_flags(false, false, true);
        let cli = Cli {
            global: tm_cli::args::GlobalOpts {
                json: false,
                quiet: false,
                no_color: true,
                plain: false,
                project: None,
            },
            prompt: false,
            prompt_text: None,
            continue_session: false,
            resume: None,
            command: Some(Command::Ticket(tm_cli::args::TicketCommand::List(
                tm_cli::args::TicketListArgs {
                    state: None,
                    milestone: None,
                    parent: None,
                },
            ))),
        };

        let result = dispatch(cli, &renderer).await;
        std::env::remove_var("TM_HOME");
        std::env::set_current_dir(original_dir).unwrap();
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
        let _guard = ENV_GUARD.lock().await;
        let tm_home = tempfile::tempdir().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tm_home.path()).unwrap();
        std::env::set_var("TM_HOME", tm_home.path());

        let renderer = Renderer::from_flags(false, false, true);
        let cli = Cli {
            global: tm_cli::args::GlobalOpts {
                json: false,
                quiet: false,
                no_color: true,
                plain: false,
                project: None,
            },
            prompt: false,
            prompt_text: None,
            continue_session: false,
            resume: None,
            command: Some(Command::Search(tm_cli::args::SearchArgs {
                query: "test".to_string(),
                exact: false,
                semantic: false,
                mode: tm_cli::args::SearchMode::Hybrid,
                limit: 20,
            })),
        };

        let result = dispatch(cli, &renderer).await;
        std::env::remove_var("TM_HOME");
        std::env::set_current_dir(original_dir).unwrap();
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
