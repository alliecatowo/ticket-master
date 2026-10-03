//! `tm serve`: build the [`tm_server`] router over the opened project and bind to it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};

use crate::args::ServeArgs;
use crate::project::Project;
use crate::render::Renderer;

/// Where the web client lives on the server. A prefix of its own keeps its routes (`/graph`,
/// `/decisions`, ...) from colliding with the API's.
const WEB_PREFIX: &str = "/app";

/// How often `tm serve`'s workers look for ready tickets.
const WORKER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Run `tm serve`: bind `tm-server`'s axum router to `args.addr` (default: loopback, ephemeral
/// port), serve the web client at `/app/` when it's built, work tickets in the background unless
/// `--no-workers`, print the URL, and shut down cleanly on ctrl-c.
pub async fn serve(
    args: &ServeArgs,
    project: Arc<Project>,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let bind_addr = parse_bind_addr(args.addr.as_deref())?;

    // Started before the config is built so `/health` and `/state` report whether workers are
    // actually running, not just whether `--no-workers` was left off.
    let workers = if args.no_workers {
        None
    } else {
        match crate::sched::spawn_background_runner(Arc::clone(&project), WORKER_INTERVAL) {
            Ok(handle) => Some(handle),
            Err(e) => {
                renderer.note(&format!(
                    "Couldn't start working tickets: {e}. You can still create and review them."
                ));
                None
            }
        }
    };

    let configured_token = match &args.token_file {
        Some(path) => Some(read_token_file(path)?),
        None => None,
    };
    let config = tm_server::state::ServerConfig {
        project_root: project.root.clone(),
        state_dir: project.state_dir.clone(),
        bind_addr,
        token: configured_token,
        presence_ttl_seconds: 3600,
        broadcast_poll_interval: std::time::Duration::from_millis(100),
        sse_replay_page_size: 100,
        sse_keep_alive: std::time::Duration::from_secs(15),
        workers: workers.is_some(),
    };

    let state = Arc::new(tm_server::state::AppState::open(
        config,
        project.clock.clone(),
        project.ids.clone(),
    )?);

    let _poller = state.spawn_broadcast_poller();

    // Every request needs the operator bearer token, loopback included. It lives in a 0600 file
    // under the state dir (never printed) so the `tm` tooling and the user can read it, and the
    // browser gets it in a URL fragment, which is never sent over the wire.
    let token_path = project.state_dir.join(TOKEN_FILE);
    write_token_file(&token_path, state.credentials.operator_token())?;

    let web_dir = find_web_client_dir(args.web_dir.as_deref());
    let router = compose_app(
        tm_server::routes::router((*state).clone()),
        (*state).clone(),
        web_dir.as_deref(),
    );

    let listener = TcpListener::bind(bind_addr).await?;
    let resolved_addr = listener.local_addr()?;
    let base = format!("http://{resolved_addr}");

    if !renderer.is_quiet() {
        renderer.note(&format!("Serving the API at {base}"));
        if !bind_addr.ip().is_loopback() {
            renderer.note(
                "warning: this address is reachable from other machines over plain HTTP, so the \
                 bearer token travels in the clear. Prefer a loopback address behind an SSH tunnel \
                 or a TLS reverse proxy.",
            );
        }
        renderer.note(&format!(
            "Every request needs `Authorization: Bearer <token>`; the token is in {}.",
            token_path.display()
        ));
        match &web_dir {
            Some(_) => renderer.note(&format!("Web client: {base}{WEB_PREFIX}/")),
            None => renderer.note(
                "Web client not found; build it with `pnpm -C clients/web install && pnpm -C \
                 clients/web build`, or point --web-dir / TM_WEB_DIR at a build.",
            ),
        }
        if workers.is_some() {
            renderer.note("Working ready tickets in the background (--no-workers to stop).");
        }
    }
    if args.open {
        let fragment = format!("#token={}", state.credentials.operator_token());
        let url = match &web_dir {
            Some(_) => format!("{base}{WEB_PREFIX}/{fragment}"),
            None => base.clone(),
        };
        if let Err(e) = open_in_browser(&url) {
            renderer.note(&format!(
                "Couldn't open a browser ({e}); visit {} and paste the token from {}",
                url.split('#').next().unwrap_or(&url),
                token_path.display()
            ));
        }
    }

    let served = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Some(workers) = workers {
        workers.abort();
    }
    served?;
    Ok(())
}

/// The API router plus, when a web client build exists, `/app/` and a `/` redirect to it. The
/// origin guard wraps the whole composed app, so the static client is covered too (it carries no
/// secrets, but it should not be reachable through a rebound hostname either). The bearer-token
/// layer stays on the API routes only: the browser has to load the client before it has a token.
pub(crate) fn compose_app(
    router: axum::Router,
    state: tm_server::state::AppState,
    web_dir: Option<&Path>,
) -> axum::Router {
    let mut router = router;
    if let Some(web_dir) = web_dir {
        // Any path under the prefix that isn't a built file is a client route: hand it
        // `index.html` so a reload or a pasted link works.
        let app = ServeDir::new(web_dir).fallback(ServeFile::new(web_dir.join("index.html")));
        router = router.nest_service(WEB_PREFIX, app).route(
            "/",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/app/") }),
        );
    }
    router.layer(axum::middleware::from_fn_with_state(
        state,
        tm_server::auth::guard_local_origin,
    ))
}

/// Read a bearer token from `path`: trimmed, and at least 16 characters (an empty or tiny token
/// is as good as none).
pub(crate) fn read_token_file(path: &Path) -> tm_types::Result<String> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        tm_types::TmError::Io(format!("reading token file {}: {e}", path.display()))
    })?;
    let token = raw.trim().to_string();
    if token.chars().count() < 16 {
        return Err(tm_types::TmError::parse(format!(
            "the token in {} is too short; use at least 16 characters (for example `openssl rand -hex 32`)",
            path.display()
        )));
    }
    Ok(token)
}

/// File (under the project state dir) holding the operator bearer token of the running server.
const TOKEN_FILE: &str = "serve.token";

/// Write `token` to `path` readable only by the current user, replacing any stale file.
pub(crate) fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(token.as_bytes())
}

/// Open `url` with the platform's opener.
fn open_in_browser(url: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Parse a bind address string or return the default loopback with ephemeral port.
fn parse_bind_addr(addr: Option<&str>) -> tm_types::Result<SocketAddr> {
    match addr {
        Some(s) => s.parse().map_err(|_e| {
            tm_types::TmError::parse(format!(
                "\"{s}\" isn't a valid address to bind to; try something like 127.0.0.1:4477"
            ))
        }),
        None => "127.0.0.1:0".parse().map_err(|_e| {
            tm_types::TmError::invariant(
                "internal error: the default loopback address failed to parse",
            )
        }),
    }
}

/// The built web client to serve: `explicit` (`--web-dir`), else `TM_WEB_DIR`, else the
/// *installed* layout next to the running executable (`scripts/install.sh` and `mise run
/// release`'s tarball both lay out `<prefix>/bin/tm` + `<prefix>/share/tm/web/`), else
/// `clients/web/dist` in the source checkout this binary was built from. Only a directory with an
/// `index.html` counts. The project being served is never searched: its own `web/dist` is its
/// app, not tm's.
fn find_web_client_dir(explicit: Option<&Path>) -> Option<PathBuf> {
    let built_from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../clients/web/dist");
    // `current_exe()` can hand back a symlink (e.g. a `$PREFIX/bin/tm` that is itself a symlink
    // into a version-pinned install dir); canonicalize so the `../share/tm/web` walk resolves
    // against the real install layout rather than the symlink's own parent.
    let installed = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok()
        .and_then(|exe| installed_web_dir(&exe));
    web_client_dir_from(
        explicit,
        std::env::var_os("TM_WEB_DIR").map(PathBuf::from),
        installed.as_deref(),
        &built_from,
    )
}

/// The installed layout's web-client dir for a running executable at `exe`:
/// `<dir of exe>/../share/tm/web`. Pure path math, no filesystem or environment access — whether
/// that directory actually has a build lives in [`web_client_dir_from`], alongside the other
/// three sources, so all four are judged by the same `index.html` check.
fn installed_web_dir(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.join("share/tm/web"))
}

/// [`find_web_client_dir`]'s choice, given its four sources, in priority order.
fn web_client_dir_from(
    explicit: Option<&Path>,
    env: Option<PathBuf>,
    installed: Option<&Path>,
    built_from: &Path,
) -> Option<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .into_iter()
        .chain(env)
        .chain(installed.map(Path::to_path_buf))
        .chain(std::iter::once(built_from.to_path_buf()))
        .find(|dir| dir.join("index.html").is_file())
}

/// Wait for ctrl-c signal. Ignores errors installing the handler; graceful shutdown fails
/// silently if signal handling isn't available (e.g., on platforms without signals).
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_file_rejects_empty_and_short_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t");
        std::fs::write(&path, "\n").expect("write");
        assert!(super::read_token_file(&path).is_err());
        std::fs::write(&path, "short").expect("write");
        assert!(super::read_token_file(&path).is_err());
        std::fs::write(&path, "  0123456789abcdef0123  \n").expect("write");
        assert_eq!(
            super::read_token_file(&path).expect("ok"),
            "0123456789abcdef0123"
        );
    }

    #[tokio::test]
    async fn static_app_is_covered_by_the_origin_guard() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let web = dir.path().join("web");
        std::fs::create_dir_all(&web).expect("mkdir");
        std::fs::write(web.join("index.html"), "<html>app</html>").expect("write");
        let config = tm_server::state::ServerConfig {
            project_root: dir.path().to_path_buf(),
            state_dir: dir.path().join("state"),
            bind_addr: "127.0.0.1:0".parse().expect("addr"),
            token: None,
            presence_ttl_seconds: 60,
            broadcast_poll_interval: std::time::Duration::from_millis(10),
            sse_replay_page_size: 10,
            sse_keep_alive: std::time::Duration::from_secs(15),
            workers: false,
        };
        let state = tm_server::state::AppState::open(
            config,
            std::sync::Arc::new(tm_types::FixedClock::epoch()),
            std::sync::Arc::new(tm_types::CounterIds::new()),
        )
        .expect("state");
        let app = super::compose_app(tm_server::routes::router(state.clone()), state, Some(&web));
        let get = |host: &str| {
            Request::builder()
                .uri("/app/")
                .header("host", host)
                .body(Body::empty())
                .expect("request")
        };
        let ok = app
            .clone()
            .oneshot(get("localhost:4477"))
            .await
            .expect("resp");
        assert_eq!(ok.status(), StatusCode::OK);
        let rebound = app.oneshot(get("attacker.example")).await.expect("resp");
        assert_eq!(rebound.status(), StatusCode::FORBIDDEN);
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only_and_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.token");
        std::fs::write(&path, "stale").expect("seed");
        super::write_token_file(&path, "fresh-token").expect("write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "fresh-token");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    use super::*;

    #[test]
    fn parse_bind_addr_default() {
        let addr = parse_bind_addr(None).expect("default should parse");
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn parse_bind_addr_custom() {
        let addr = parse_bind_addr(Some("127.0.0.1:4477")).expect("custom should parse");
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 4477);
    }

    #[test]
    fn parse_bind_addr_invalid() {
        let result = parse_bind_addr(Some("not-a-valid-address"));
        assert!(result.is_err());
    }

    fn built(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create dir");
        std::fs::write(dir.join("index.html"), "<html></html>").expect("write index");
        dir.to_path_buf()
    }

    #[test]
    fn the_web_client_is_found_by_flag_then_env_then_installed_then_the_build_checkout() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let flag = built(&root.path().join("flag"));
        let env = built(&root.path().join("env"));
        let installed = built(&root.path().join("installed"));
        let checkout = built(&root.path().join("checkout"));
        assert_eq!(
            web_client_dir_from(Some(&flag), Some(env.clone()), Some(&installed), &checkout),
            Some(flag),
            "--web-dir wins over everything else"
        );
        assert_eq!(
            web_client_dir_from(None, Some(env.clone()), Some(&installed), &checkout),
            Some(env),
            "TM_WEB_DIR wins over the installed layout and the build checkout"
        );
        assert_eq!(
            web_client_dir_from(None, None, Some(&installed), &checkout),
            Some(installed.clone()),
            "the installed layout wins over the build checkout"
        );
        assert_eq!(
            web_client_dir_from(None, None, None, &checkout),
            Some(checkout.clone()),
            "the build checkout is the last resort"
        );
        let unbuilt = root.path().join("unbuilt");
        std::fs::create_dir_all(&unbuilt).expect("create dir");
        assert_eq!(
            web_client_dir_from(Some(&unbuilt), None, None, &checkout),
            Some(checkout.clone()),
            "a directory without index.html isn't a build"
        );
        assert_eq!(
            web_client_dir_from(None, None, Some(&unbuilt), &checkout),
            Some(checkout),
            "an installed dir without index.html falls through to the build checkout too"
        );
        assert_eq!(web_client_dir_from(None, None, None, &unbuilt), None);
    }

    #[test]
    fn installed_web_dir_is_share_tm_web_next_to_the_exe() {
        assert_eq!(
            installed_web_dir(Path::new("/opt/tm/bin/tm")),
            Some(PathBuf::from("/opt/tm/share/tm/web"))
        );
        assert_eq!(
            installed_web_dir(Path::new("/home/allie/.local/bin/tm")),
            Some(PathBuf::from("/home/allie/.local/share/tm/web"))
        );
        // No grandparent to walk up to: stays None rather than panicking.
        assert_eq!(installed_web_dir(Path::new("tm")), None);
        assert_eq!(installed_web_dir(Path::new("/tm")), None);
    }
}
