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
                    "Not working tickets: {e}. Tickets can still be created and reviewed."
                ));
                None
            }
        }
    };

    let config = tm_server::state::ServerConfig {
        project_root: project.root.clone(),
        state_dir: project.state_dir.clone(),
        bind_addr,
        token: None,
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

    let mut router = tm_server::routes::router((*state).clone());

    let web_dir = find_web_client_dir(args.web_dir.as_deref());
    if let Some(web_dir) = &web_dir {
        // Any path under the prefix that isn't a built file is a client route: hand it
        // `index.html` so a reload or a pasted link works.
        let app = ServeDir::new(web_dir).fallback(ServeFile::new(web_dir.join("index.html")));
        router = router.nest_service(WEB_PREFIX, app).route(
            "/",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/app/") }),
        );
    }

    let listener = TcpListener::bind(bind_addr).await?;
    let resolved_addr = listener.local_addr()?;
    let base = format!("http://{resolved_addr}");

    if !renderer.is_quiet() {
        renderer.note(&format!("Serving the API at {base}"));
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
        let url = match &web_dir {
            Some(_) => format!("{base}{WEB_PREFIX}/"),
            None => base.clone(),
        };
        if let Err(e) = open_in_browser(&url) {
            renderer.note(&format!("Couldn't open a browser ({e}); visit {url}"));
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
                "bind address {s:?}: expected a socket address like 127.0.0.1:4477"
            ))
        }),
        None => "127.0.0.1:0".parse().map_err(|_e| {
            tm_types::TmError::invariant(
                "default loopback:ephemeral socket address should always parse",
            )
        }),
    }
}

/// The built web client to serve: `explicit` (`--web-dir`), else `TM_WEB_DIR`, else
/// `clients/web/dist` in the source checkout this binary was built from. Only a directory with an
/// `index.html` counts. The project being served is never searched: its own `web/dist` is its
/// app, not tm's.
fn find_web_client_dir(explicit: Option<&Path>) -> Option<PathBuf> {
    let built_from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../clients/web/dist");
    web_client_dir_from(
        explicit,
        std::env::var_os("TM_WEB_DIR").map(PathBuf::from),
        &built_from,
    )
}

/// [`find_web_client_dir`]'s choice, given its three sources.
fn web_client_dir_from(
    explicit: Option<&Path>,
    env: Option<PathBuf>,
    built_from: &Path,
) -> Option<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .into_iter()
        .chain(env)
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
    fn the_web_client_is_found_by_flag_then_env_then_the_build_checkout() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let flag = built(&root.path().join("flag"));
        let env = built(&root.path().join("env"));
        let checkout = built(&root.path().join("checkout"));
        assert_eq!(
            web_client_dir_from(Some(&flag), Some(env.clone()), &checkout),
            Some(flag)
        );
        assert_eq!(
            web_client_dir_from(None, Some(env.clone()), &checkout),
            Some(env)
        );
        assert_eq!(
            web_client_dir_from(None, None, &checkout),
            Some(checkout.clone())
        );
        let unbuilt = root.path().join("unbuilt");
        std::fs::create_dir_all(&unbuilt).expect("create dir");
        assert_eq!(
            web_client_dir_from(Some(&unbuilt), None, &checkout),
            Some(checkout),
            "a directory without index.html isn't a build"
        );
        assert_eq!(web_client_dir_from(None, None, &unbuilt), None);
    }
}
