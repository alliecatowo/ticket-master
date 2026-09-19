//! `tm serve`: build the [`tm_server`] router over the opened project and bind to it.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio::net::TcpListener;
use tower_http::services::ServeDir;

use crate::args::ServeArgs;
use crate::project::Project;
use crate::render::Renderer;

/// Run `tm serve`: bind `tm-server`'s axum router to `args.addr` (default: loopback, ephemeral
/// port), print the URL a human or the web client should hit, serve the built web client's
/// static assets when present, and shut down cleanly on ctrl-c.
pub async fn serve(
    args: &ServeArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let bind_addr = parse_bind_addr(args.addr.as_deref())?;

    let config = tm_server::state::ServerConfig {
        project_root: project.root.clone(),
        // TODO(D-003 scope-resolution track): replace with `project.state_dir` once `Project`
        // gains that field; this hardcodes today's repo-scoped layout.
        state_dir: project.root.join(".tm"),
        bind_addr,
        token: None,
        presence_ttl_seconds: 3600,
        broadcast_poll_interval: std::time::Duration::from_millis(100),
        sse_replay_page_size: 100,
    };

    let state = Arc::new(tm_server::state::AppState::open(
        config,
        project.clock.clone(),
        project.ids.clone(),
    )?);

    let _poller = state.spawn_broadcast_poller();

    let mut router = tm_server::routes::router((*state).clone());

    if let Some(web_dir) = find_web_client_dir(&project.root) {
        router = router.nest_service("/", ServeDir::new(&web_dir));
    }

    let listener = TcpListener::bind(bind_addr).await?;
    let resolved_addr = listener.local_addr()?;

    if !renderer.is_quiet() {
        renderer.note(&format!("Serving at http://{}", resolved_addr));
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
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

/// Check for web client build directories in common locations.
fn find_web_client_dir(project_root: &Path) -> Option<std::path::PathBuf> {
    let candidates = [
        project_root.join("web").join("dist"),
        project_root.join("web").join("build"),
        project_root.join("apps").join("web").join("dist"),
        project_root.join("apps").join("web").join("build"),
    ];

    for candidate in &candidates {
        if candidate.is_dir() {
            return Some(candidate.clone());
        }
    }
    None
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

    #[test]
    fn find_web_client_dir_none() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let result = find_web_client_dir(root.path());
        assert!(result.is_none());
    }

    #[test]
    fn find_web_client_dir_web_dist() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let web_dist = root.path().join("web").join("dist");
        std::fs::create_dir_all(&web_dist).expect("create dirs");
        let result = find_web_client_dir(root.path());
        assert_eq!(result, Some(web_dist));
    }

    #[test]
    fn find_web_client_dir_web_build() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let web_build = root.path().join("web").join("build");
        std::fs::create_dir_all(&web_build).expect("create dirs");
        let result = find_web_client_dir(root.path());
        assert_eq!(result, Some(web_build));
    }

    #[test]
    fn find_web_client_dir_apps_web_dist() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let apps_web_dist = root.path().join("apps").join("web").join("dist");
        std::fs::create_dir_all(&apps_web_dist).expect("create dirs");
        let result = find_web_client_dir(root.path());
        assert_eq!(result, Some(apps_web_dist));
    }

    #[test]
    fn find_web_client_dir_precedence_web_dist_over_web_build() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let web_dist = root.path().join("web").join("dist");
        let web_build = root.path().join("web").join("build");
        std::fs::create_dir_all(&web_dist).expect("create web/dist");
        std::fs::create_dir_all(&web_build).expect("create web/build");
        let result = find_web_client_dir(root.path());
        assert_eq!(result, Some(web_dist));
    }
}
