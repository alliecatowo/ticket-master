//! `BrowserSession`: the agent-facing surface over a launched browser process — open/close,
//! tabs, navigation, ref-addressed actions, eval, waiting, console/network logs, cookies,
//! screenshots and PDF export.
//!
//! Every capability listed in `SPEC.md` §19.3 lives here as one method. Oversized outputs
//! (full-page screenshots, long network/console logs, PDFs) are written through an
//! [`ArtifactSink`] and returned as an [`ArtifactRef`] rather than inlined, per §8.2. Every
//! navigation and download is checked through [`crate::authority::NavigationGuard`] first, and
//! every navigation, action and denial is appended to this session's
//! [`crate::authority::SessionTrace`] as it happens.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tm_types::{ArtifactId, Authority, Clock, IdSource, Result, SessionId, TmError};
use tokio::io::AsyncBufReadExt;
use tokio::sync::broadcast;

use crate::authority::{ActionKind, NavigationGuard, SessionTrace, TraceEvent};
use crate::cdp::{CdpClient, CdpEvent};
use crate::discover::{self, DiscoveredBrowser, LaunchConfig};
use crate::snapshot::{AxRef, Snapshot};

/// How long [`BrowserSession::launch`] waits for the browser to print its DevTools websocket
/// URL before giving up.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// How long [`BrowserSession::navigate`] waits for `Page.loadEventFired` before giving up.
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval used by [`BrowserSession::wait_for`]'s selector/text conditions.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The quiet window with no in-flight request that counts as network-idle.
const NETWORK_IDLE_QUIET_WINDOW: Duration = Duration::from_millis(500);

/// Stores bytes that would be too large to return inline, and hands back an [`ArtifactId`].
/// Implemented against `tm-core`'s artifact tables by the binary that wires this crate up; a
/// fake in-memory implementation stands in for it in unit tests, mirroring
/// `tm_context::command::CommandCache`.
pub trait ArtifactSink: Send + Sync {
    /// Persist `bytes` (of the given MIME `content_type`) and return its new artifact id.
    fn store(&self, bytes: &[u8], content_type: &str) -> Result<ArtifactId>;
}

/// Either a value small enough to return inline, or a pointer to one stored as an artifact
/// because it exceeded [`BrowserSessionConfig::artifact_threshold_bytes`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ArtifactRef {
    /// Small enough to inline directly.
    Inline(Vec<u8>),
    /// Stored separately; fetch via the same [`ArtifactSink`] this session was configured with.
    Stored(ArtifactId),
}

/// The id of one open tab (a CDP page target), scoped to a single [`BrowserSession`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TabId(pub String);

/// Summary of one open tab, as returned by [`BrowserSession::list_tabs`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    /// This tab's id.
    pub id: TabId,
    /// The tab's current title.
    pub title: String,
    /// The tab's current URL.
    pub url: String,
}

/// What [`BrowserSession::wait_for`] blocks until.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    /// A CSS selector matches at least one element.
    Selector(String),
    /// The page's rendered text contains this substring.
    Text(String),
    /// No network request has been outstanding for a short quiet window.
    NetworkIdle,
}

/// One captured browser console message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleMessage {
    /// `"log"`, `"warn"`, `"error"`, etc.
    pub level: String,
    /// The rendered message text.
    pub text: String,
    /// Millisecond CDP timestamp, as reported by the browser.
    pub timestamp_ms: f64,
}

/// One logged network request/response pair. Bodies are never inlined here — fetch them
/// through the returned [`ArtifactRef`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkEntry {
    /// The request URL.
    pub url: String,
    /// The HTTP method.
    pub method: String,
    /// The response status code, once the response arrived.
    pub status: Option<u16>,
    /// The request body, when non-empty.
    pub request_body: Option<ArtifactRef>,
    /// The response body, when non-empty.
    pub response_body: Option<ArtifactRef>,
}

/// A browser cookie, in CDP's `Network.Cookie` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// The domain it applies to.
    pub domain: String,
    /// The path it applies to.
    pub path: String,
    /// `Secure` flag.
    pub secure: bool,
    /// `HttpOnly` flag.
    pub http_only: bool,
}

/// A full storage snapshot: cookies plus `localStorage`/`sessionStorage` for the current
/// origin, for cross-session state reuse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageState {
    /// All cookies visible to the current page.
    pub cookies: Vec<Cookie>,
    /// `localStorage` entries, keyed by key.
    pub local_storage: Vec<(String, String)>,
}

/// Configuration a [`BrowserSession`] is launched with.
pub struct BrowserSessionConfig {
    /// The browser executable to launch.
    pub browser: DiscoveredBrowser,
    /// The authority to gate navigation and downloads against.
    pub authority: Authority,
    /// Command outputs (screenshots, PDFs, logs) at or above this size become artifacts
    /// instead of inline bytes.
    pub artifact_threshold_bytes: usize,
    /// Extra Chromium flags beyond the fixed headless/port/profile set.
    pub extra_launch_args: Vec<String>,
}

/// A live headless browser session: one launched browser process, its CDP connection, its open
/// tabs, and the [`SessionTrace`] recording everything it has done.
///
/// Not `Clone`: a session owns a real OS process and websocket connection.
pub struct BrowserSession {
    id: SessionId,
    config: BrowserSessionConfig,
    process: tokio::process::Child,
    profile_dir: PathBuf,
    cdp: CdpClient,
    active_tab: Option<TabId>,
    /// The attached CDP session id for each tab this session has opened or switched to.
    tab_sessions: HashMap<TabId, String>,
    /// Maps a stable [`AxRef`] handed out by the last [`BrowserSession::snapshot`] back to the
    /// CDP backend DOM node id it was derived from, so ref-addressed actions can resolve it.
    ref_to_backend_node: HashMap<AxRef, i64>,
    /// Lazily subscribed once [`BrowserSession::console`] is first called.
    console_rx: Option<broadcast::Receiver<CdpEvent>>,
    /// Lazily subscribed once [`BrowserSession::network`] is first called.
    network_rx: Option<broadcast::Receiver<CdpEvent>>,
    sink: Arc<dyn ArtifactSink>,
    clock: Arc<dyn Clock>,
    trace: SessionTrace,
}

/// Parse the `ws://...` DevTools URL out of a line of Chrome's startup stderr, e.g.
/// `DevTools listening on ws://127.0.0.1:9222/devtools/browser/<uuid>`.
fn parse_devtools_ws_url(line: &str) -> Option<String> {
    line.split_once("DevTools listening on ")
        .map(|(_, rest)| rest.trim().to_string())
        .filter(|url| url.starts_with("ws://") || url.starts_with("wss://"))
}

/// Check `url` against `authority`'s navigation grant, recording a
/// [`TraceEvent::NavigationBlocked`] on denial before propagating the error. Shared by
/// [`BrowserSession::open_tab`] and [`BrowserSession::navigate`], which both add their own
/// success-path trace event afterward.
fn guard_or_record_denial(
    authority: &Authority,
    trace: &mut SessionTrace,
    clock: &dyn Clock,
    url: &str,
) -> Result<()> {
    let guard = NavigationGuard::new(authority);
    match guard.check_navigate(url) {
        Ok(()) => Ok(()),
        Err(err) => {
            trace.record(TraceEvent::NavigationBlocked {
                url: url.to_string(),
                reason: err.to_string(),
                at: clock.now(),
            });
            Err(err)
        }
    }
}

/// Route `bytes` through `sink` as a stored artifact once it reaches `threshold`, else inline.
fn route_artifact(
    sink: &dyn ArtifactSink,
    bytes: Vec<u8>,
    content_type: &str,
    threshold: usize,
) -> Result<ArtifactRef> {
    if bytes.len() >= threshold {
        Ok(ArtifactRef::Stored(sink.store(&bytes, content_type)?))
    } else {
        Ok(ArtifactRef::Inline(bytes))
    }
}

/// Decode a standard (padded or unpadded) base64 string, as CDP returns for binary payloads.
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut reverse = [255u8; 256];
    for (i, &b) in ALPHABET.iter().enumerate() {
        reverse[b as usize] = i as u8;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut chunk = [0u8; 4];
    let mut chunk_len = 0usize;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            break;
        }
        let v = reverse[b as usize];
        if v == 255 {
            return Err(TmError::parse(format!("invalid base64 byte {b:#x}")));
        }
        chunk[chunk_len] = v;
        chunk_len += 1;
        if chunk_len == 4 {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
            out.push((chunk[2] << 6) | chunk[3]);
            chunk_len = 0;
        }
    }
    match chunk_len {
        0 => {}
        2 => out.push((chunk[0] << 2) | (chunk[1] >> 4)),
        3 => {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        _ => return Err(TmError::parse("truncated base64 input")),
    }
    Ok(out)
}

/// The CDP key code table entries [`BrowserSession::press`] recognizes:
/// `(code, windows_virtual_key_code, key)`.
fn key_definition(key: &str) -> Option<(&'static str, i64, &'static str)> {
    Some(match key {
        "Enter" => ("Enter", 13, "Enter"),
        "Tab" => ("Tab", 9, "Tab"),
        "Escape" => ("Escape", 27, "Escape"),
        "Backspace" => ("Backspace", 8, "Backspace"),
        "Delete" => ("Delete", 46, "Delete"),
        "Space" => ("Space", 32, " "),
        "ArrowUp" => ("ArrowUp", 38, "ArrowUp"),
        "ArrowDown" => ("ArrowDown", 40, "ArrowDown"),
        "ArrowLeft" => ("ArrowLeft", 37, "ArrowLeft"),
        "ArrowRight" => ("ArrowRight", 39, "ArrowRight"),
        _ => return None,
    })
}

/// The center point of a CDP box-model content quad: eight numbers, `[x1, y1, x2, y2, x3, y3,
/// x4, y4]`, one corner per pair.
fn center_of_quad(quad: &[Value]) -> Result<(f64, f64)> {
    let nums: Vec<f64> = quad.iter().filter_map(Value::as_f64).collect();
    if nums.len() != 8 {
        return Err(TmError::invariant(
            "content quad did not have exactly 8 coordinates",
        ));
    }
    let cx = (nums[0] + nums[2] + nums[4] + nums[6]) / 4.0;
    let cy = (nums[1] + nums[3] + nums[5] + nums[7]) / 4.0;
    Ok((cx, cy))
}

/// `true` when a JSON value counts as truthy under JS semantics, for [`WaitCondition`] polling.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Render one `Runtime.consoleAPICalled` event's params as a [`ConsoleMessage`].
fn parse_console_message(params: &Value) -> ConsoleMessage {
    let level = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("log")
        .to_string();
    let timestamp_ms = params
        .get("timestamp")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .map(render_remote_object)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    ConsoleMessage {
        level,
        text,
        timestamp_ms,
    }
}

/// Render a CDP `RemoteObject` (from a console arg or an eval result) as display text.
fn render_remote_object(v: &Value) -> String {
    if let Some(desc) = v.get("description").and_then(Value::as_str) {
        return desc.to_string();
    }
    match v.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Pull the decoded bytes out of a `Network.getResponseBody` result.
fn extract_response_body(result: &Value) -> Option<Vec<u8>> {
    let body = result.get("body")?.as_str()?;
    let base64_encoded = result
        .get("base64Encoded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if base64_encoded {
        base64_decode(body).ok()
    } else {
        Some(body.as_bytes().to_vec())
    }
}

/// One request/response pair accumulated while draining buffered `Network.*` events.
#[derive(Default)]
struct PendingRequest {
    request_id: String,
    url: String,
    method: String,
    status: Option<u16>,
    request_body_text: Option<String>,
    finished: bool,
}

impl BrowserSession {
    /// Launch a fresh browser process from `config` and connect to it over CDP.
    ///
    /// # Errors
    /// A storage-layer [`tm_types::TmError`] when the process fails to start, when its
    /// DevTools websocket URL cannot be discovered within a bounded startup deadline, or when
    /// the initial CDP handshake fails.
    pub async fn launch(
        config: BrowserSessionConfig,
        sink: Arc<dyn ArtifactSink>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        session_id: SessionId,
    ) -> Result<Self> {
        let profile_dir = discover::ephemeral_profile_dir(ids.as_ref())?;
        let launch_config = LaunchConfig {
            browser: config.browser.clone(),
            profile_dir: profile_dir.clone(),
            extra_args: config.extra_launch_args.clone(),
        };
        let argv = discover::build_argv(&launch_config);

        let mut command = tokio::process::Command::new(&config.browser.path);
        command
            .args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut process = command
            .spawn()
            .map_err(|e| TmError::Io(format!("failed to spawn browser process: {e}")))?;

        let stderr = process
            .stderr
            .take()
            .ok_or_else(|| TmError::invariant("piped browser stderr was not captured"))?;
        let mut lines = tokio::io::BufReader::new(stderr).lines();

        let ws_url = tokio::time::timeout(LAUNCH_TIMEOUT, async {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Some(url) = parse_devtools_ws_url(&line) {
                            return Some(url);
                        }
                    }
                    Ok(None) | Err(_) => return None,
                }
            }
        })
        .await
        .map_err(|_| {
            TmError::Provider(
                "timed out waiting for the browser to print its DevTools websocket URL".into(),
            )
        })?
        .ok_or_else(|| {
            TmError::Provider(
                "browser process exited before printing its DevTools websocket URL".into(),
            )
        })?;

        let cdp = CdpClient::connect(&ws_url).await?;

        Ok(BrowserSession {
            id: session_id.clone(),
            config,
            process,
            profile_dir,
            cdp,
            active_tab: None,
            tab_sessions: HashMap::new(),
            ref_to_backend_node: HashMap::new(),
            console_rx: None,
            network_rx: None,
            sink,
            clock,
            trace: SessionTrace::new(session_id),
        })
    }

    /// This session's id, as recorded in its [`SessionTrace`].
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// The trace recorded so far: every navigation, action and authority decision.
    pub fn trace(&self) -> &SessionTrace {
        &self.trace
    }

    /// Close every tab, disconnect CDP, and terminate the browser process.
    ///
    /// # Errors
    /// Best-effort: a failure tearing down CDP is logged (via `tracing`) rather than aborting
    /// process termination, since a session that cannot be cleanly closed must not leak an OS
    /// process either way.
    pub async fn close(mut self) -> Result<()> {
        if let Err(e) = self.cdp.close().await {
            tracing::warn!(error = %e, "failed to cleanly close the CDP connection");
        }
        if let Err(e) = self.process.kill().await {
            tracing::warn!(error = %e, "failed to kill the browser process");
        }
        if let Err(e) = self.process.wait().await {
            tracing::warn!(error = %e, "failed waiting for the browser process to exit");
        }
        if let Err(e) = std::fs::remove_dir_all(&self.profile_dir) {
            tracing::warn!(
                error = %e,
                path = %self.profile_dir.display(),
                "failed to remove the browser profile directory",
            );
        }
        Ok(())
    }

    /// Open a new tab at `url`, subject to [`crate::authority::NavigationGuard`], and make it
    /// the active tab.
    ///
    /// # Errors
    /// [`tm_types::TmError::AuthorityDenied`] when `url` is not permitted; also records a
    /// [`TraceEvent::NavigationBlocked`] in that case rather than a
    /// [`TraceEvent::Navigated`].
    pub async fn open_tab(&mut self, url: &str) -> Result<TabId> {
        guard_or_record_denial(
            &self.config.authority,
            &mut self.trace,
            self.clock.as_ref(),
            url,
        )?;

        let target = self.cdp.create_target(url).await?;
        let session_id = self.cdp.attach_to_target(&target.target_id).await?;
        let tab_id = TabId(target.target_id);
        self.tab_sessions.insert(tab_id.clone(), session_id);
        self.active_tab = Some(tab_id.clone());

        self.trace.record(TraceEvent::Navigated {
            url: url.to_string(),
            at: self.clock.now(),
        });
        Ok(tab_id)
    }

    /// List every open tab.
    pub async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        let targets = self.cdp.list_targets().await?;
        Ok(targets
            .into_iter()
            .filter(|t| t.target_type == "page")
            .map(|t| TabInfo {
                id: TabId(t.target_id),
                title: t.title,
                url: t.url,
            })
            .collect())
    }

    /// Make `tab` the active tab for subsequent ref-addressed actions.
    ///
    /// # Errors
    /// [`tm_types::TmError::NotFound`] when `tab` is not among this session's open tabs.
    pub async fn switch_tab(&mut self, tab: TabId) -> Result<()> {
        let exists = self.list_tabs().await?.into_iter().any(|t| t.id == tab);
        if !exists {
            return Err(TmError::not_found("tab", &tab.0));
        }
        if !self.tab_sessions.contains_key(&tab) {
            let session_id = self.cdp.attach_to_target(&tab.0).await?;
            self.tab_sessions.insert(tab.clone(), session_id);
        }
        self.active_tab = Some(tab);
        Ok(())
    }

    /// Navigate the active tab to `url`, subject to [`crate::authority::NavigationGuard`].
    ///
    /// # Errors
    /// Same authority-denial behavior as [`BrowserSession::open_tab`].
    pub async fn navigate(&mut self, url: &str) -> Result<()> {
        guard_or_record_denial(
            &self.config.authority,
            &mut self.trace,
            self.clock.as_ref(),
            url,
        )?;

        let session = self.active_session()?.to_string();
        let mut events = self.cdp.subscribe();
        self.cdp
            .call_in_session("Page.navigate", json!({ "url": url }), Some(&session))
            .await?;

        let wait_for_load = async {
            loop {
                match events.recv().await {
                    Ok(event)
                        if event.method == "Page.loadEventFired"
                            && event.session_id.as_deref() == Some(session.as_str()) =>
                    {
                        return;
                    }
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
        };
        tokio::time::timeout(NAVIGATION_TIMEOUT, wait_for_load)
            .await
            .map_err(|_| TmError::Provider(format!("navigation to {url} timed out")))?;

        self.trace.record(TraceEvent::Navigated {
            url: url.to_string(),
            at: self.clock.now(),
        });
        Ok(())
    }

    /// Take an accessibility-tree snapshot of the active tab — the default observation an
    /// agent reads before its next action.
    pub async fn snapshot(&mut self) -> Result<Snapshot> {
        let session = self.active_session()?.to_string();
        self.cdp
            .call_in_session("Accessibility.enable", Value::Null, Some(&session))
            .await?;
        let result = self
            .cdp
            .call_in_session("Accessibility.getFullAXTree", Value::Null, Some(&session))
            .await?;
        let nodes = result
            .get("nodes")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));

        self.ref_to_backend_node.clear();
        if let Some(entries) = nodes.as_array() {
            for entry in entries {
                if let Some(id) = entry.get("backendDOMNodeId").and_then(Value::as_i64) {
                    self.ref_to_backend_node
                        .insert(AxRef::from_backend_node_id(id), id);
                }
            }
        }

        Snapshot::from_ax_tree(&nodes, false).map_err(|e| TmError::Parse(e.to_string()))
    }

    /// Click the element at `reference`.
    pub async fn click(&mut self, reference: &AxRef) -> Result<()> {
        let (x, y) = self.resolve_point(reference).await?;
        self.dispatch_click(x, y).await?;
        self.trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Click,
            reference: Some(reference.0.clone()),
            at: self.clock.now(),
        });
        Ok(())
    }

    /// Hover the pointer over the element at `reference`.
    pub async fn hover(&mut self, reference: &AxRef) -> Result<()> {
        let (x, y) = self.resolve_point(reference).await?;
        let session = self.active_session()?.to_string();
        self.cdp
            .call_in_session(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": x, "y": y }),
                Some(&session),
            )
            .await?;
        self.trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Hover,
            reference: Some(reference.0.clone()),
            at: self.clock.now(),
        });
        Ok(())
    }

    /// Type `text` into the focusable element at `reference`.
    ///
    /// # Errors
    /// [`tm_types::TmError::InvalidTransition`] when `reference` is not a focusable, editable
    /// node.
    pub async fn type_text(&mut self, reference: &AxRef, text: &str) -> Result<()> {
        let (x, y) = self.resolve_point(reference).await?;
        self.dispatch_click(x, y).await?;
        let session = self.active_session()?.to_string();
        self.cdp
            .call_in_session("Input.insertText", json!({ "text": text }), Some(&session))
            .await
            .map_err(|e| {
                TmError::InvalidTransition(format!("cannot type into {}: {e}", reference.0))
            })?;
        self.trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Type,
            reference: Some(reference.0.clone()),
            at: self.clock.now(),
        });
        Ok(())
    }

    /// Select `value` in the `<select>` element at `reference`.
    pub async fn select(&mut self, reference: &AxRef, value: &str) -> Result<()> {
        let backend_node_id = self.backend_node_id(reference)?;
        let session = self.active_session()?.to_string();
        let resolved = self
            .cdp
            .call_in_session(
                "DOM.resolveNode",
                json!({ "backendNodeId": backend_node_id }),
                Some(&session),
            )
            .await?;
        let object_id = resolved
            .get("object")
            .and_then(|o| o.get("objectId"))
            .and_then(Value::as_str)
            .ok_or_else(|| TmError::invariant("DOM.resolveNode returned no objectId"))?
            .to_string();

        self.cdp
            .call_in_session(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration":
                        "function(v) { this.value = v; \
                         this.dispatchEvent(new Event('change', { bubbles: true })); }",
                    "arguments": [{ "value": value }],
                }),
                Some(&session),
            )
            .await?;

        self.trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Select,
            reference: Some(reference.0.clone()),
            at: self.clock.now(),
        });
        Ok(())
    }

    /// Press a keyboard key (e.g. `"Enter"`, `"Tab"`) on the currently focused element.
    ///
    /// # Errors
    /// [`tm_types::TmError::Parse`] when `key` is not a recognized key name.
    pub async fn press(&mut self, key: &str) -> Result<()> {
        let (code, windows_virtual_key_code, key_value) = key_definition(key)
            .ok_or_else(|| TmError::parse(format!("unrecognized key name {key:?}")))?;
        let session = self.active_session()?.to_string();
        for event_type in ["keyDown", "keyUp"] {
            self.cdp
                .call_in_session(
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": event_type,
                        "code": code,
                        "key": key_value,
                        "windowsVirtualKeyCode": windows_virtual_key_code,
                    }),
                    Some(&session),
                )
                .await?;
        }
        self.trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Press,
            reference: None,
            at: self.clock.now(),
        });
        Ok(())
    }

    /// Evaluate `js` in the active tab's page context and return its JSON-serializable result.
    ///
    /// # Errors
    /// The evaluated expression's own thrown error surfaces as `TmError::Provider`; a result
    /// that cannot round-trip through JSON (e.g. a DOM node reference) surfaces as
    /// `TmError::Parse`.
    pub async fn eval(&mut self, js: &str) -> Result<Value> {
        let session = self.active_session()?.to_string();
        let result = self
            .cdp
            .call_in_session(
                "Runtime.evaluate",
                json!({ "expression": js, "returnByValue": true, "awaitPromise": true }),
                Some(&session),
            )
            .await?;

        if let Some(exception) = result.get("exceptionDetails") {
            let message = exception
                .get("exception")
                .map(render_remote_object)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    exception
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "evaluation threw".to_string());
            return Err(TmError::Provider(message));
        }

        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .ok_or_else(|| TmError::parse("evaluation result was not JSON-representable"))?;

        self.trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Eval,
            reference: None,
            at: self.clock.now(),
        });
        Ok(value)
    }

    /// Block until `condition` holds, or return a timeout error after `timeout`.
    ///
    /// # Errors
    /// A storage-layer [`tm_types::TmError`] (not a distinct timeout variant — callers
    /// distinguish by message, matching how other bounded waits in this workspace surface)
    /// when `timeout` elapses first.
    pub async fn wait_for(&mut self, condition: WaitCondition, timeout: Duration) -> Result<()> {
        let session = self.active_session()?.to_string();
        match condition {
            WaitCondition::Selector(selector) => {
                let expr = format!(
                    "!!document.querySelector({})",
                    serde_json::to_string(&selector).unwrap_or_else(|_| "\"\"".to_string())
                );
                self.poll_until_truthy(&session, &expr, timeout).await
            }
            WaitCondition::Text(text) => {
                let expr = format!(
                    "!!(document.body && document.body.innerText.includes({}))",
                    serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".to_string())
                );
                self.poll_until_truthy(&session, &expr, timeout).await
            }
            WaitCondition::NetworkIdle => self.wait_for_network_idle(&session, timeout).await,
        }
    }

    /// Every console message collected since the last call to this method.
    pub async fn console(&mut self) -> Result<Vec<ConsoleMessage>> {
        let session = self.active_session()?.to_string();
        if self.console_rx.is_none() {
            self.cdp
                .call_in_session("Runtime.enable", Value::Null, Some(&session))
                .await?;
            self.console_rx = Some(self.cdp.subscribe());
        }

        let mut out = Vec::new();
        if let Some(rx) = self.console_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(event) if event.method == "Runtime.consoleAPICalled" => {
                        out.push(parse_console_message(&event.params));
                    }
                    Ok(_) => {}
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
        Ok(out)
    }

    /// The request/response log since the last call, with bodies stored as artifacts when they
    /// exceed [`BrowserSessionConfig::artifact_threshold_bytes`].
    pub async fn network(&mut self) -> Result<Vec<NetworkEntry>> {
        let session = self.active_session()?.to_string();
        if self.network_rx.is_none() {
            self.cdp
                .call_in_session("Network.enable", Value::Null, Some(&session))
                .await?;
            self.network_rx = Some(self.cdp.subscribe());
        }

        let mut pending: HashMap<String, PendingRequest> = HashMap::new();
        if let Some(rx) = self.network_rx.as_mut() {
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let Some(request_id) = event.params.get("requestId").and_then(Value::as_str) else {
                    continue;
                };
                match event.method.as_str() {
                    "Network.requestWillBeSent" => {
                        let entry = pending.entry(request_id.to_string()).or_default();
                        entry.request_id = request_id.to_string();
                        if let Some(request) = event.params.get("request") {
                            entry.url = request
                                .get("url")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            entry.method = request
                                .get("method")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            entry.request_body_text = request
                                .get("postData")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                        }
                    }
                    "Network.responseReceived" => {
                        if let Some(entry) = pending.get_mut(request_id) {
                            entry.status = event
                                .params
                                .get("response")
                                .and_then(|r| r.get("status"))
                                .and_then(Value::as_u64)
                                .map(|s| s as u16);
                        }
                    }
                    "Network.loadingFinished" => {
                        if let Some(entry) = pending.get_mut(request_id) {
                            entry.finished = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        for entry in pending.into_values() {
            let request_body = entry
                .request_body_text
                .map(|text| self.route_body(text.into_bytes(), "text/plain"))
                .transpose()?;

            let response_body = if entry.finished {
                match self
                    .cdp
                    .call_in_session(
                        "Network.getResponseBody",
                        json!({ "requestId": entry.request_id }),
                        Some(&session),
                    )
                    .await
                {
                    Ok(result) => match extract_response_body(&result) {
                        Some(bytes) if !bytes.is_empty() => {
                            Some(self.route_body(bytes, "application/octet-stream")?)
                        }
                        _ => None,
                    },
                    Err(_) => None,
                }
            } else {
                None
            };

            out.push(NetworkEntry {
                url: entry.url,
                method: entry.method,
                status: entry.status,
                request_body,
                response_body,
            });
        }
        Ok(out)
    }

    /// All cookies visible to the active tab.
    pub async fn cookies(&mut self) -> Result<Vec<Cookie>> {
        let session = self.active_session()?.to_string();
        let result = self
            .cdp
            .call_in_session("Network.getCookies", Value::Null, Some(&session))
            .await?;
        let raw = result
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        raw.iter().map(parse_cookie).collect()
    }

    /// Set a cookie.
    pub async fn set_cookie(&mut self, cookie: Cookie) -> Result<()> {
        let session = self.active_session()?.to_string();
        self.cdp
            .call_in_session(
                "Network.setCookie",
                json!({
                    "name": cookie.name,
                    "value": cookie.value,
                    "domain": cookie.domain,
                    "path": cookie.path,
                    "secure": cookie.secure,
                    "httpOnly": cookie.http_only,
                }),
                Some(&session),
            )
            .await?;
        Ok(())
    }

    /// Full storage state (cookies plus `localStorage`) for the active tab's origin, for
    /// reuse across sessions.
    pub async fn storage_state(&mut self) -> Result<StorageState> {
        let cookies = self.cookies().await?;
        let session = self.active_session()?.to_string();
        let result = self
            .cdp
            .call_in_session(
                "Runtime.evaluate",
                json!({
                    "expression": "JSON.stringify(Object.entries(localStorage))",
                    "returnByValue": true,
                }),
                Some(&session),
            )
            .await?;
        let raw = result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("[]");
        let local_storage: Vec<(String, String)> = serde_json::from_str(raw)
            .map_err(|e| TmError::parse(format!("localStorage dump was not valid JSON: {e}")))?;
        Ok(StorageState {
            cookies,
            local_storage,
        })
    }

    /// Capture a screenshot of the active tab as a PNG [`ArtifactRef`].
    pub async fn screenshot(&mut self, full_page: bool) -> Result<ArtifactRef> {
        let session = self.active_session()?.to_string();
        let result = self
            .cdp
            .call_in_session(
                "Page.captureScreenshot",
                json!({ "format": "png", "captureBeyondViewport": full_page }),
                Some(&session),
            )
            .await?;
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| TmError::invariant("Page.captureScreenshot returned no data"))?;
        let bytes = base64_decode(data)?;
        self.route_body(bytes, "image/png")
    }

    /// Render the active tab to a PDF [`ArtifactRef`].
    pub async fn pdf(&mut self) -> Result<ArtifactRef> {
        let session = self.active_session()?.to_string();
        let result = self
            .cdp
            .call_in_session("Page.printToPDF", Value::Null, Some(&session))
            .await?;
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| TmError::invariant("Page.printToPDF returned no data"))?;
        let bytes = base64_decode(data)?;
        self.route_body(bytes, "application/pdf")
    }

    /// The CDP session id attached to the active tab.
    ///
    /// # Errors
    /// [`tm_types::TmError::InvalidTransition`] when no tab has been opened or switched to yet.
    fn active_session(&self) -> Result<&str> {
        let tab = self.active_tab.as_ref().ok_or_else(|| {
            TmError::InvalidTransition("no active tab; call open_tab first".into())
        })?;
        self.tab_sessions
            .get(tab)
            .map(String::as_str)
            .ok_or_else(|| TmError::invariant(format!("tab {} has no attached CDP session", tab.0)))
    }

    /// The backend DOM node id `reference` was derived from in the last [`Self::snapshot`].
    ///
    /// # Errors
    /// [`tm_types::TmError::NotFound`] when `reference` was never seen in a snapshot (or is
    /// stale — a snapshot has been taken since without re-observing that node).
    fn backend_node_id(&self, reference: &AxRef) -> Result<i64> {
        self.ref_to_backend_node
            .get(reference)
            .copied()
            .ok_or_else(|| TmError::not_found("ax-ref", &reference.0))
    }

    /// Resolve `reference` to its on-screen center point via `DOM.getBoxModel`.
    async fn resolve_point(&mut self, reference: &AxRef) -> Result<(f64, f64)> {
        let backend_node_id = self.backend_node_id(reference)?;
        let session = self.active_session()?.to_string();
        let box_model = self
            .cdp
            .call_in_session(
                "DOM.getBoxModel",
                json!({ "backendNodeId": backend_node_id }),
                Some(&session),
            )
            .await?;
        let quad = box_model
            .get("model")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .ok_or_else(|| TmError::invariant("DOM.getBoxModel returned no content quad"))?;
        center_of_quad(quad)
    }

    /// Dispatch a left-button press-then-release at `(x, y)` on the active tab.
    async fn dispatch_click(&mut self, x: f64, y: f64) -> Result<()> {
        let session = self.active_session()?.to_string();
        for event_type in ["mousePressed", "mouseReleased"] {
            self.cdp
                .call_in_session(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": event_type,
                        "x": x,
                        "y": y,
                        "button": "left",
                        "clickCount": 1,
                    }),
                    Some(&session),
                )
                .await?;
        }
        Ok(())
    }

    /// Route `bytes` through this session's [`ArtifactSink`] per its configured threshold.
    fn route_body(&self, bytes: Vec<u8>, content_type: &str) -> Result<ArtifactRef> {
        route_artifact(
            self.sink.as_ref(),
            bytes,
            content_type,
            self.config.artifact_threshold_bytes,
        )
    }

    /// Poll `expr` (a boolean-returning JS expression) until it evaluates truthy, or `timeout`
    /// elapses.
    async fn poll_until_truthy(&self, session: &str, expr: &str, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                let result = self
                    .cdp
                    .call_in_session(
                        "Runtime.evaluate",
                        json!({ "expression": expr, "returnByValue": true }),
                        Some(session),
                    )
                    .await?;
                let truthy = result
                    .get("result")
                    .and_then(|r| r.get("value"))
                    .map(is_truthy)
                    .unwrap_or(false);
                if truthy {
                    return Ok(());
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .map_err(|_| {
            TmError::Provider(format!("timed out after {timeout:?} waiting for condition"))
        })?
    }

    /// Wait for a quiet window with no in-flight network request, or `timeout` elapses.
    async fn wait_for_network_idle(&mut self, session: &str, timeout: Duration) -> Result<()> {
        self.cdp
            .call_in_session("Network.enable", Value::Null, Some(session))
            .await?;
        let mut rx = self.cdp.subscribe();
        let mut in_flight: std::collections::HashSet<String> = std::collections::HashSet::new();

        tokio::time::timeout(timeout, async {
            loop {
                let quiet = tokio::time::sleep(NETWORK_IDLE_QUIET_WINDOW);
                tokio::select! {
                    biased;
                    _ = quiet, if in_flight.is_empty() => return,
                    event = rx.recv() => {
                        let Ok(event) = event else { return };
                        if event.session_id.as_deref() != Some(session) {
                            continue;
                        }
                        let Some(id) = event.params.get("requestId").and_then(Value::as_str) else {
                            continue;
                        };
                        match event.method.as_str() {
                            "Network.requestWillBeSent" => {
                                in_flight.insert(id.to_string());
                            }
                            "Network.loadingFinished" | "Network.loadingFailed" => {
                                in_flight.remove(id);
                            }
                            _ => {}
                        }
                    }
                }
            }
        })
        .await
        .map_err(|_| {
            TmError::Provider(format!(
                "timed out after {timeout:?} waiting for network idle"
            ))
        })
    }
}

/// Parse one CDP `Network.Cookie` JSON object into a [`Cookie`].
///
/// # Errors
/// [`tm_types::TmError::Parse`] when the object is missing its required `name` field.
fn parse_cookie(v: &Value) -> Result<Cookie> {
    Ok(Cookie {
        name: v
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| TmError::parse("cookie JSON missing name"))?
            .to_string(),
        value: v
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        domain: v
            .get("domain")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        path: v
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        secure: v.get("secure").and_then(Value::as_bool).unwrap_or(false),
        http_only: v.get("httpOnly").and_then(Value::as_bool).unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use tm_types::{ArtifactId, FixedClock, IdKind};

    struct FakeSink {
        stored: Mutex<Vec<(Vec<u8>, String)>>,
        counter: AtomicU64,
    }

    impl FakeSink {
        fn new() -> Self {
            FakeSink {
                stored: Mutex::new(Vec::new()),
                counter: AtomicU64::new(0),
            }
        }
    }

    impl ArtifactSink for FakeSink {
        fn store(&self, bytes: &[u8], content_type: &str) -> Result<ArtifactId> {
            let n = self.counter.fetch_add(1, Ordering::SeqCst);
            self.stored
                .lock()
                .expect("fake sink mutex is never poisoned in tests")
                .push((bytes.to_vec(), content_type.to_string()));
            ArtifactId::new(format!("ART-{n:012x}"))
        }
    }

    #[test]
    fn parses_devtools_ws_url_out_of_a_startup_line() {
        let line = "DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc-123";
        assert_eq!(
            parse_devtools_ws_url(line).as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc-123")
        );
    }

    #[test]
    fn ignores_unrelated_stderr_lines() {
        assert_eq!(
            parse_devtools_ws_url("[1234:5678] some other log line"),
            None
        );
    }

    #[test]
    fn small_bytes_are_routed_inline() {
        let sink = FakeSink::new();
        let out = route_artifact(&sink, vec![1, 2, 3], "image/png", 10).expect("routes inline");
        assert_eq!(out, ArtifactRef::Inline(vec![1, 2, 3]));
        assert!(sink.stored.lock().unwrap().is_empty());
    }

    #[test]
    fn oversized_bytes_are_routed_through_the_sink() {
        let sink = FakeSink::new();
        let bytes = vec![9u8; 10];
        let out = route_artifact(&sink, bytes.clone(), "image/png", 10).expect("routes stored");
        match out {
            ArtifactRef::Stored(_) => {}
            ArtifactRef::Inline(_) => panic!("expected a stored artifact"),
        }
        assert_eq!(
            sink.stored.lock().unwrap()[0],
            (bytes, "image/png".to_string())
        );
    }

    #[test]
    fn bytes_exactly_at_the_threshold_are_stored() {
        let sink = FakeSink::new();
        let out = route_artifact(&sink, vec![0u8; 5], "text/plain", 5).unwrap();
        assert!(matches!(out, ArtifactRef::Stored(_)));
    }

    #[test]
    fn base64_decodes_a_padded_string() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello".to_vec());
    }

    #[test]
    fn base64_decodes_an_unpadded_string() {
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello".to_vec());
    }

    #[test]
    fn base64_rejects_an_invalid_byte() {
        assert!(base64_decode("not valid base64 !!!").is_err());
    }

    #[test]
    fn base64_round_trips_empty_input() {
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn known_keys_resolve_to_a_definition() {
        assert_eq!(key_definition("Enter"), Some(("Enter", 13, "Enter")));
        assert_eq!(key_definition("Tab"), Some(("Tab", 9, "Tab")));
    }

    #[test]
    fn unknown_keys_have_no_definition() {
        assert_eq!(key_definition("Fnord"), None);
    }

    #[test]
    fn center_of_quad_averages_the_four_corners() {
        let quad = vec![
            json!(0.0),
            json!(0.0),
            json!(10.0),
            json!(0.0),
            json!(10.0),
            json!(20.0),
            json!(0.0),
            json!(20.0),
        ];
        let (x, y) = center_of_quad(&quad).unwrap();
        assert_eq!((x, y), (5.0, 10.0));
    }

    #[test]
    fn center_of_quad_rejects_a_malformed_quad() {
        assert!(center_of_quad(&[json!(1.0), json!(2.0)]).is_err());
    }

    #[test]
    fn truthiness_matches_js_semantics() {
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&json!(false)));
        assert!(!is_truthy(&json!(0)));
        assert!(!is_truthy(&json!("")));
        assert!(is_truthy(&json!("hi")));
        assert!(is_truthy(&json!(1)));
        assert!(is_truthy(&json!([])));
    }

    #[test]
    fn parses_a_console_message_with_a_string_arg() {
        let params = json!({
            "type": "warn",
            "timestamp": 1234.5,
            "args": [{ "type": "string", "value": "careful" }],
        });
        let msg = parse_console_message(&params);
        assert_eq!(msg.level, "warn");
        assert_eq!(msg.text, "careful");
        assert_eq!(msg.timestamp_ms, 1234.5);
    }

    #[test]
    fn console_message_defaults_to_log_level_when_untyped() {
        let msg = parse_console_message(&json!({ "args": [] }));
        assert_eq!(msg.level, "log");
        assert_eq!(msg.text, "");
    }

    #[test]
    fn extracts_a_plain_text_response_body() {
        let result = json!({ "body": "hello", "base64Encoded": false });
        assert_eq!(extract_response_body(&result), Some(b"hello".to_vec()));
    }

    #[test]
    fn extracts_a_base64_response_body() {
        let result = json!({ "body": "aGVsbG8=", "base64Encoded": true });
        assert_eq!(extract_response_body(&result), Some(b"hello".to_vec()));
    }

    #[test]
    fn missing_body_field_yields_none() {
        assert_eq!(extract_response_body(&json!({})), None);
    }

    #[test]
    fn parses_a_well_formed_cookie() {
        let raw = json!({
            "name": "session",
            "value": "abc",
            "domain": "example.com",
            "path": "/",
            "secure": true,
            "httpOnly": true,
        });
        let cookie = parse_cookie(&raw).unwrap();
        assert_eq!(cookie.name, "session");
        assert_eq!(cookie.value, "abc");
        assert!(cookie.secure);
        assert!(cookie.http_only);
    }

    #[test]
    fn cookie_without_a_name_is_rejected() {
        assert!(parse_cookie(&json!({ "value": "abc" })).is_err());
    }

    #[test]
    fn cookie_missing_optional_fields_defaults_them() {
        let cookie = parse_cookie(&json!({ "name": "n" })).unwrap();
        assert_eq!(cookie.value, "");
        assert!(!cookie.secure);
        assert!(!cookie.http_only);
    }

    #[test]
    fn guard_records_a_denial_and_returns_the_error() {
        let authority = Authority::default();
        let clock = FixedClock::epoch();
        let session_id = SessionId::new("S-1").unwrap();
        let mut trace = SessionTrace::new(session_id);

        let result = guard_or_record_denial(&authority, &mut trace, &clock, "https://example.com");

        assert!(result.is_err());
        assert_eq!(trace.events.len(), 1);
        match &trace.events[0] {
            TraceEvent::NavigationBlocked { url, .. } => assert_eq!(url, "https://example.com"),
            other => panic!("expected NavigationBlocked, got {other:?}"),
        }
    }

    #[test]
    fn guard_permits_and_records_nothing_itself() {
        let mut authority = Authority::default();
        authority.network.arbitrary = true;
        let clock = FixedClock::epoch();
        let session_id = SessionId::new("S-1").unwrap();
        let mut trace = SessionTrace::new(session_id);

        let result = guard_or_record_denial(&authority, &mut trace, &clock, "https://example.com");

        assert!(result.is_ok());
        assert!(trace.events.is_empty());
    }

    #[test]
    fn tab_id_serializes_as_a_bare_string() {
        let tab = TabId("target-1".to_string());
        let json = serde_json::to_string(&tab).unwrap();
        assert_eq!(json, "\"target-1\"");
    }

    #[test]
    fn artifact_ref_round_trips_through_json() {
        let inline = ArtifactRef::Inline(vec![1, 2, 3]);
        let round_tripped: ArtifactRef =
            serde_json::from_str(&serde_json::to_string(&inline).unwrap()).unwrap();
        assert_eq!(inline, round_tripped);
    }

    // A stray reference to IdKind so the import above is exercised without pulling in a whole
    // id-allocation test: SessionId::new already covers construction, this just documents that
    // the id module's kind tagging agrees with what this crate expects of a SessionId.
    #[test]
    fn session_ids_this_crate_constructs_are_tagged_as_sessions() {
        let id = SessionId::new("S-1").unwrap();
        assert_eq!(tm_types::Id::from(id).kind(), Some(IdKind::Session));
    }
}
