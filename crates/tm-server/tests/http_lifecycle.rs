//! Offline e2e test: `tm serve`'s HTTP API drives a ticket through creation, transitions and
//! error paths (`p1-e2e-http-api-lifecycle`).
//!
//! Regression coverage for what the `serve-api` probe verified by hand with `curl`: `POST
//! /tickets` creating a ticket, reading it back via `GET /tickets/{id}` and
//! `/tickets/{id}/events`, the `/schema`/`/health`/`/state` endpoints, the `activate` transition,
//! and the JSON `{error, message}` error shape (`p1-http-error-json-format`) for both a 404 and a
//! malformed `POST` body. This binds the real axum router to an ephemeral loopback port and
//! drives it with a real `reqwest` client, rather than calling handlers directly, so it exercises
//! the actual HTTP plumbing (routing, extractors, status codes) end to end. Everything runs
//! against a fresh tempdir project with no network involved: no provider is ever invoked, since
//! this only exercises ticket CRUD/transition endpoints.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tm_server::state::{AppState, ServerConfig};
use tm_types::{Clock, CounterIds, FixedClock, IdSource};

/// The operator token the test server is configured with (loopback requires one too).
const OPERATOR_TOKEN: &str = "test-operator-token";

/// A `reqwest::Client` that never consults `HTTP_PROXY`/`HTTPS_PROXY`. A sandboxed or CI shell
/// commonly sets one of those, and without this a loopback request can get silently routed
/// through it and fail or hang instead of hitting the server this test just bound.
fn http_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {OPERATOR_TOKEN}").parse().expect("header"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .no_proxy()
        .build()
        .expect("build a proxy-free reqwest client")
}

/// Bind `tm-server`'s real router to an ephemeral loopback port and spawn it serving in the
/// background, returning the base URL. The `TempDir` is returned alongside it (not dropped) so
/// the project's state directory stays alive for the caller's duration, same as `routes.rs`'s own
/// `test_state` helper.
async fn spawn_server() -> (TempDir, String) {
    let (dir, base, _state) = spawn_server_with_state().await;
    (dir, base)
}

async fn spawn_server_with_state() -> (TempDir, String, AppState) {
    let dir = TempDir::new().expect("tempdir");
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
    let config = ServerConfig {
        project_root: dir.path().to_path_buf(),
        state_dir: dir.path().join(".tm"),
        bind_addr: "127.0.0.1:0"
            .parse::<SocketAddr>()
            .expect("valid loopback addr"),
        token: Some(OPERATOR_TOKEN.to_string()),
        presence_ttl_seconds: 60,
        broadcast_poll_interval: Duration::from_millis(10),
        sse_replay_page_size: 100,
        sse_keep_alive: Duration::from_secs(15),
        workers: false,
    };
    let state = AppState::open(config, clock, ids).expect("open app state");
    let router = tm_server::routes::router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });

    (dir, format!("http://{addr}"), state)
}

fn create_ticket_body() -> Value {
    json!({
        "kind": "work",
        "objective": "drive the HTTP API end to end",
        "actor": "human:test",
    })
}

#[tokio::test]
async fn ticket_lifecycle_through_the_real_http_api() {
    let (_dir, base) = spawn_server().await;
    let client = http_client();

    // `GET /health`, `/state`, `/schema` all answer 200 against a freshly opened project.
    let health = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("health request");
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    let health_body: Value = health.json().await.expect("health json");
    assert_eq!(health_body["status"], "ok");

    let state_resp = client
        .get(format!("{base}/state"))
        .send()
        .await
        .expect("state request");
    assert_eq!(state_resp.status(), reqwest::StatusCode::OK);

    let schema_resp = client
        .get(format!("{base}/schema"))
        .send()
        .await
        .expect("schema request");
    assert_eq!(schema_resp.status(), reqwest::StatusCode::OK);

    // `POST /tickets` creates a ticket and returns it plus the events it produced.
    let create_resp = client
        .post(format!("{base}/tickets"))
        .json(&create_ticket_body())
        .send()
        .await
        .expect("create request");
    assert_eq!(create_resp.status(), reqwest::StatusCode::CREATED);
    let create_body: Value = create_resp.json().await.expect("create json");
    let ticket_id = create_body["ticket"]["id"]
        .as_str()
        .expect("ticket id in create response")
        .to_string();
    assert_eq!(
        create_body["ticket"]["objective"],
        "drive the HTTP API end to end"
    );
    assert!(
        !create_body["events"]
            .as_array()
            .expect("events array")
            .is_empty(),
        "creating a ticket should append at least one event"
    );

    // `GET /tickets/{id}` reads the same ticket back.
    let get_resp = client
        .get(format!("{base}/tickets/{ticket_id}"))
        .send()
        .await
        .expect("get ticket request");
    assert_eq!(get_resp.status(), reqwest::StatusCode::OK);
    let get_body: Value = get_resp.json().await.expect("get ticket json");
    assert_eq!(get_body["id"], ticket_id);
    assert_eq!(get_body["state"], "draft");

    // `GET /tickets/{id}/events` returns the ticket's own history, oldest first.
    let events_resp = client
        .get(format!("{base}/tickets/{ticket_id}/events"))
        .send()
        .await
        .expect("ticket events request");
    assert_eq!(events_resp.status(), reqwest::StatusCode::OK);
    let events_body: Value = events_resp.json().await.expect("ticket events json");
    assert!(
        !events_body["events"]
            .as_array()
            .expect("events array")
            .is_empty(),
        "a freshly created ticket should have at least one history event"
    );

    // `POST /tickets/{id}/transition` drives the draft -> ready `activate` transition.
    let activate_resp = client
        .post(format!("{base}/tickets/{ticket_id}/transition"))
        .json(&json!({"activate": {"actor": "human:test"}}))
        .send()
        .await
        .expect("activate request");
    assert_eq!(activate_resp.status(), reqwest::StatusCode::OK);

    let after_activate = client
        .get(format!("{base}/tickets/{ticket_id}"))
        .send()
        .await
        .expect("get ticket after activate")
        .json::<Value>()
        .await
        .expect("get ticket after activate json");
    assert_eq!(after_activate["state"], "ready");
}

#[tokio::test]
async fn a_malformed_ticket_id_returns_json_400_with_error_and_message() {
    let (_dir, base) = spawn_server().await;
    let client = http_client();

    let resp = client
        .get(format!("{base}/tickets/NOTFOUND"))
        .send()
        .await
        .expect("get malformed ticket id request");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("error body is JSON");
    assert!(
        !body["error"].as_str().unwrap_or_default().is_empty(),
        "error body must carry a non-empty error code: {body}"
    );
    assert!(
        !body["message"].as_str().unwrap_or_default().is_empty(),
        "error body must carry a non-empty message: {body}"
    );
}

#[tokio::test]
async fn get_a_well_formed_but_missing_ticket_id_returns_json_404() {
    let (_dir, base) = spawn_server().await;
    let client = http_client();

    let resp = client
        .get(format!("{base}/tickets/T-999999"))
        .send()
        .await
        .expect("get missing ticket request");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let body: Value = resp.json().await.expect("error body is JSON");
    assert_eq!(body["error"], "not_found");
    assert!(
        !body["message"].as_str().unwrap_or_default().is_empty(),
        "error body must carry a non-empty message: {body}"
    );
}

#[tokio::test]
async fn malformed_create_ticket_body_returns_json_400_naming_every_missing_field() {
    let (_dir, base) = spawn_server().await;
    let client = http_client();

    let resp = client
        .post(format!("{base}/tickets"))
        .json(&json!({}))
        .send()
        .await
        .expect("malformed create request");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("error body is JSON");
    assert!(
        !body["error"].as_str().unwrap_or_default().is_empty(),
        "error body must carry a non-empty error code: {body}"
    );
    let message = body["message"].as_str().unwrap_or_default();
    for field in ["objective", "kind", "actor"] {
        assert!(
            message.contains(field),
            "message should name missing field `{field}` together with the others: {message}"
        );
    }
}

/// `tm serve` on loopback has no bearer token, so it must refuse a request addressed to (or
/// issued by) a foreign host: the DNS-rebinding and cross-site-request cases.
#[tokio::test]
async fn loopback_server_rejects_foreign_host_and_origin() {
    let (_dir, base) = spawn_server().await;
    let client = http_client();

    let ok = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("health");
    assert_eq!(ok.status(), reqwest::StatusCode::OK);

    let rebound = client
        .get(format!("{base}/state"))
        .header("Host", "attacker.example:4477")
        .send()
        .await
        .expect("rebound host");
    assert_eq!(rebound.status(), reqwest::StatusCode::FORBIDDEN);
    let body: Value = rebound.json().await.expect("error json");
    assert_eq!(body["error"], "forbidden_origin");

    let cross_site = client
        .post(format!("{base}/tickets"))
        .header("Origin", "https://attacker.example")
        .json(&create_ticket_body())
        .send()
        .await
        .expect("cross-site post");
    assert_eq!(cross_site.status(), reqwest::StatusCode::FORBIDDEN);

    let same_origin = client
        .get(format!("{base}/health"))
        .header("Origin", "http://localhost:5173")
        .send()
        .await
        .expect("local dev origin");
    assert_eq!(same_origin.status(), reqwest::StatusCode::OK);
}

fn bare_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
}

/// Mint an agent token through the API as the operator.
async fn mint_agent(base: &str, name: &str) -> String {
    let resp = http_client()
        .post(format!("{base}/agent-tokens"))
        .json(&json!({"name": name}))
        .send()
        .await
        .expect("mint");
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let body: Value = resp.json().await.expect("json");
    body["token"].as_str().expect("token").to_string()
}

#[tokio::test]
async fn loopback_requires_a_bearer_token() {
    let (_dir, base) = spawn_server().await;
    let none = bare_client()
        .get(format!("{base}/state"))
        .send()
        .await
        .expect("req");
    assert_eq!(none.status(), reqwest::StatusCode::UNAUTHORIZED);
    let wrong = bare_client()
        .get(format!("{base}/state"))
        .bearer_auth("nope")
        .send()
        .await
        .expect("req");
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
    let empty = bare_client()
        .get(format!("{base}/state"))
        .header("authorization", "Bearer ")
        .send()
        .await
        .expect("req");
    assert_eq!(empty.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unconfigured_loopback_server_generates_a_random_operator_token() {
    let dir = TempDir::new().expect("tempdir");
    let mut config = ServerConfig {
        project_root: dir.path().to_path_buf(),
        state_dir: dir.path().join(".tm"),
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().expect("addr"),
        token: None,
        presence_ttl_seconds: 60,
        broadcast_poll_interval: Duration::from_millis(10),
        sse_replay_page_size: 100,
        sse_keep_alive: Duration::from_secs(15),
        workers: false,
    };
    let a = AppState::open(
        config.clone(),
        Arc::new(FixedClock::epoch()),
        Arc::new(CounterIds::new()),
    )
    .expect("open");
    config.state_dir = dir.path().join(".tm2");
    let b = AppState::open(
        config,
        Arc::new(FixedClock::epoch()),
        Arc::new(CounterIds::new()),
    )
    .expect("open");
    let (ta, tb) = (
        a.credentials.operator_token(),
        b.credentials.operator_token(),
    );
    assert!(
        ta.len() >= 32 && ta != tb,
        "tokens must be long and unpredictable"
    );
}

#[tokio::test]
async fn body_cannot_claim_a_different_actor() {
    let (_dir, base) = spawn_server().await;
    let client = http_client();
    let resp = client
        .post(format!("{base}/tickets"))
        .json(&json!({"kind": "work", "objective": "x", "actor": "human:allie"}))
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let body: Value = resp.json().await.expect("json");
    let id = body["ticket"]["id"].as_str().expect("id").to_string();
    let events: Value = client
        .get(format!("{base}/tickets/{id}/events"))
        .send()
        .await
        .expect("events")
        .json()
        .await
        .expect("json");
    let text = events.to_string();
    assert!(
        text.contains("human:operator"),
        "actor bound to the credential: {text}"
    );
    assert!(
        !text.contains("human:allie"),
        "body claim must be ignored: {text}"
    );
}

#[tokio::test]
async fn agent_token_cannot_impersonate_a_human_or_decide_approvals() {
    let (_dir, base) = spawn_server().await;
    let agent = mint_agent(&base, "bot").await;
    let agent_client = || bare_client();

    // Agent decides an approval claiming to be a human: refused before the lookup.
    let decide = agent_client()
        .post(format!("{base}/approvals/AP-x/decide"))
        .bearer_auth(&agent)
        .json(&json!({"decision": {"approve": {}}, "decided_by": "human:allie"}))
        .send()
        .await
        .expect("decide");
    assert_eq!(decide.status(), reqwest::StatusCode::FORBIDDEN);

    // Agent cannot mint further tokens.
    let mint = agent_client()
        .post(format!("{base}/agent-tokens"))
        .bearer_auth(&agent)
        .json(&json!({"name": "sub"}))
        .send()
        .await
        .expect("mint");
    assert_eq!(mint.status(), reqwest::StatusCode::FORBIDDEN);

    // Agent creates a ticket "as" a human with a huge authority grant: actor is the agent and
    // the authority is clamped to the worker default.
    let resp = agent_client()
        .post(format!("{base}/tickets"))
        .bearer_auth(&agent)
        .json(&json!({
            "kind": "work", "objective": "x", "actor": "human:allie",
            "authority": tm_types::Authority::root(),
        }))
        .send()
        .await
        .expect("create");
    let status = resp.status();
    let text = resp.text().await.expect("text");
    assert_eq!(status, reqwest::StatusCode::CREATED, "{text}");
    let body: Value = serde_json::from_str(&text).expect("json");
    let id = body["ticket"]["id"].as_str().expect("id").to_string();
    let worker =
        serde_json::to_value(tm_types::Authority::root().intersect(&tm_types::Authority::worker()))
            .expect("authority json");
    assert_eq!(body["ticket"]["authority"], worker);
    let events = http_client()
        .get(format!("{base}/tickets/{id}/events"))
        .send()
        .await
        .expect("events")
        .text()
        .await
        .expect("text");
    assert!(
        events.contains("agent:api/bot") && !events.contains("human:allie"),
        "{events}"
    );

    // Accept (human-only) with a human claim in the body is still the agent, so it is refused.
    let accept = agent_client()
        .post(format!("{base}/tickets/{id}/transition"))
        .bearer_auth(&agent)
        .json(&json!({"accept": {"actor": "human:allie"}}))
        .send()
        .await
        .expect("accept");
    assert!(
        accept.status().is_client_error(),
        "an agent must not accept work, got {}",
        accept.status()
    );
}
