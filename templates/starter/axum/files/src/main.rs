//! {{description}}
//!
//! Minimal Axum scaffold: one `/health` route, a `router()` function factored out from `main()`
//! so it's testable without a real socket. See `skill.md` for this stack's conventions.

use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

const PROJECT_NAME: &str = "{{project_name}}";
const PORT: u16 = {{port}};

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": PROJECT_NAME }))
}

/// Build the app's router. Kept separate from `main()` so tests can drive it directly, with no
/// running process or real TCP socket involved — see `skill.md`.
fn router() -> Router {
    Router::new().route("/health", get(health))
}

#[tokio::main]
async fn main() {
    let app = router();
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT))
        .await
        .unwrap_or_else(|e| panic!("{PROJECT_NAME}: failed to bind port {PORT}: {e}"));
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| panic!("{PROJECT_NAME}: server error: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_returns_ok() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router serves the request");
        assert_eq!(response.status(), StatusCode::OK);
    }
}
