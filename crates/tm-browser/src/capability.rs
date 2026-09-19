//! [`BrowserCapability`]: tm-browser's [`tm_types::CapabilityProvider`] impl
//! (`docs/audit-2026-09-18-fable.md` B-02, `SPEC.md` §19.3/§19.4).
//!
//! Wraps [`crate::session::BrowserSession`]'s methods as dotted `browser.*` tools. A capability
//! provider is constructed once and `invoke`d many times, possibly concurrently, across many
//! tickets/sessions — but a [`BrowserSession`] is not `Clone` and most of its methods take
//! `&mut self` — so [`SessionRegistry`] lazily launches and reuses one session per
//! `(ticket, session)` key (`CallContext` carries no lease id today, so this is the closest
//! substitute available at this layer; see this module's doc comment on [`SessionRegistry`] for
//! detail) and hands out `Arc<tokio::sync::Mutex<Option<BrowserSession>>>` so many concurrent
//! calls against the *same* session serialize instead of racing, while calls against *different*
//! sessions run independently.
//!
//! # Action mapping
//!
//! `browser.navigate`/`browser.open` map to [`tm_types::Action::BrowserNavigate`] with the
//! requested `url`, gated by `Authority.network` exactly like `Action::NetFetch`. Every other
//! browser tool (click/hover/type/select/press/eval/snapshot/...) acts on a page whose origin was
//! already checked at navigate time, and `to_action` is a pure function of `(tool, input)` with
//! no session state available to ask "what origin is this session on" — so those tools map to
//! `Action::BrowserNavigate { url: "about:blank" }`, which `Authority::permits` allows
//! unconditionally (an opaque URL reaches no network origin). See `Action::BrowserNavigate`'s doc
//! comment in `tm-types` for the same reasoning.
//!
//! # Session teardown
//!
//! [`SessionRegistry::close`]/[`SessionRegistry::close_all`] are the explicit teardown this
//! module provides — `SPEC.md` §19.1b's "the session dies with the lease" is only half
//! implemented here: nothing in this workspace yet fires a callback when a lease expires
//! (`tm-core::Store::expire_leases` emits `ticket.lease_expired` events, but no subscriber wakes
//! a `SessionRegistry`), so true expiry-triggered teardown is out of this change's reach. What
//! *is* real: `tm_agent::executor::BuiltinExecutor::execute` constructs a fresh
//! [`SessionRegistry`] per dispatched task and calls [`SessionRegistry::close_all`] once the task
//! finishes (success, failure, or panic-unwind-safe early return) — so a session's lifetime is
//! bounded by its dispatch, which is the real-world proxy for "the lease that authorized it" in
//! every call path this workspace has today. A process that crashes mid-task still leaks a
//! browser process, exactly as before this change; closing that gap needs the lease-expiry
//! subscriber hook M-16/B-01's follow-on work would add, not something reachable from here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use tm_types::{
    Action, AuthorityRequirement, CallContext, CapabilityProvider, Clock, CostClass, Result,
    SessionId, TicketId, TmError, ToolSchema,
};

use crate::provider::{ProviderRegistry, SessionRequest};
use crate::session::{ArtifactSink, BrowserSession, BrowserSessionConfig, Cookie, WaitCondition};
use crate::snapshot::AxRef;

fn missing(field: &str) -> TmError {
    TmError::parse(format!("missing or malformed field `{field}`"))
}

fn get_str<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| missing(field))
}

fn get_string(input: &Value, field: &str) -> Result<String> {
    Ok(get_str(input, field)?.to_string())
}

fn get_opt_string(input: &Value, field: &str) -> Option<String> {
    input.get(field).and_then(Value::as_str).map(str::to_string)
}

fn get_bool_or(input: &Value, field: &str, default: bool) -> bool {
    input.get(field).and_then(Value::as_bool).unwrap_or(default)
}

fn get_u64_or(input: &Value, field: &str, default: u64) -> u64 {
    input.get(field).and_then(Value::as_u64).unwrap_or(default)
}

fn ax_ref(input: &Value) -> Result<AxRef> {
    Ok(AxRef(get_string(input, "ref")?))
}

/// A key identifying one live browser session. `CallContext` does not carry a lease id, so
/// `(ticket, session)` is the closest available substitute — see this module's doc comment.
type SessionKey = (TicketId, SessionId);

/// Lazily launches, reuses, and explicitly tears down one [`BrowserSession`] per
/// `(ticket, session)` key, via the existing [`ProviderRegistry`]/`browser.toml` machinery.
///
/// A session is stored as `Arc<AsyncMutex<Option<BrowserSession>>>` rather than
/// `Arc<AsyncMutex<BrowserSession>>` because [`BrowserSession::close`] consumes `self` — the
/// `Option` is what lets a shared, lock-guarded slot be closed by value (`Option::take`) instead
/// of needing sole ownership of the `Arc`, which concurrent in-flight calls would make
/// unavailable via `Arc::try_unwrap`.
pub struct SessionRegistry {
    providers: Arc<ProviderRegistry>,
    sink: Arc<dyn ArtifactSink>,
    clock: Arc<dyn Clock>,
    artifact_threshold_bytes: usize,
    sessions: AsyncMutex<HashMap<SessionKey, Arc<AsyncMutex<Option<BrowserSession>>>>>,
}

impl SessionRegistry {
    /// Build an empty registry over already-constructed provider/artifact/clock infrastructure.
    pub fn new(
        providers: Arc<ProviderRegistry>,
        sink: Arc<dyn ArtifactSink>,
        clock: Arc<dyn Clock>,
        artifact_threshold_bytes: usize,
    ) -> Self {
        SessionRegistry {
            providers,
            sink,
            clock,
            artifact_threshold_bytes,
            sessions: AsyncMutex::new(HashMap::new()),
        }
    }

    /// Return the session for `(ctx.ticket, ctx.session)`, launching one via
    /// [`ProviderRegistry::acquire`] on first use. The registry's own lock is held across
    /// acquisition and launch: coarse, but it guarantees this key (and every other key
    /// contending at the same instant) never launches two browsers for one call site and leaks
    /// one — a correctness trade this module accepts over finer-grained per-key locking.
    async fn get_or_launch(
        &self,
        ctx: &CallContext<'_>,
    ) -> Result<Arc<AsyncMutex<Option<BrowserSession>>>> {
        let key = (ctx.ticket.clone(), ctx.session.clone());
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(&key) {
            return Ok(existing.clone());
        }

        let config = BrowserSessionConfig {
            authority: ctx.authority.clone(),
            artifact_threshold_bytes: self.artifact_threshold_bytes,
        };
        let request = SessionRequest::default();
        let (provider, endpoint) = self.providers.acquire(&request).await?;
        let session = BrowserSession::launch(
            provider,
            endpoint,
            config,
            self.sink.clone(),
            self.clock.clone(),
            ctx.session.clone(),
        )
        .await?;

        let slot = Arc::new(AsyncMutex::new(Some(session)));
        sessions.insert(key, slot.clone());
        Ok(slot)
    }

    /// Explicitly tear down the session for `(ticket, session)`, if one is live. A no-op when
    /// none exists (closing twice, or closing a key that never launched, is not an error).
    pub async fn close(&self, ticket: &TicketId, session: &SessionId) -> Result<()> {
        let slot = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(&(ticket.clone(), session.clone()))
        };
        let Some(slot) = slot else {
            return Ok(());
        };
        let mut guard = slot.lock().await;
        if let Some(session) = guard.take() {
            session.close().await?;
        }
        Ok(())
    }

    /// Tear down every session this registry currently holds — the registry owner's explicit
    /// "I am done dispatching, reclaim everything" call (see this module's doc comment on why
    /// this stands in for true lease-expiry teardown).
    pub async fn close_all(&self) -> Result<()> {
        let slots: Vec<_> = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().map(|(_, slot)| slot).collect()
        };
        let mut first_err = None;
        for slot in slots {
            let mut guard = slot.lock().await;
            if let Some(session) = guard.take() {
                if let Err(e) = session.close().await {
                    tracing::warn!(error = %e, "failed to close a browser session during close_all");
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// How many sessions are currently live. Test/diagnostic use.
    pub async fn live_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

/// tm-browser's [`CapabilityProvider`]: every `SPEC.md` §19.3 tool over [`SessionRegistry`].
pub struct BrowserCapability {
    sessions: SessionRegistry,
}

impl BrowserCapability {
    /// Build the browser capability over an already-constructed [`SessionRegistry`].
    pub fn new(sessions: SessionRegistry) -> Self {
        BrowserCapability { sessions }
    }

    /// Passthrough to [`SessionRegistry::close_all`] — the handle a registry's owner (e.g.
    /// `tm_agent::executor::BuiltinExecutor::execute`) keeps a concrete `Arc<BrowserCapability>`
    /// around for, alongside registering the same `Arc` as a `dyn CapabilityProvider`, so it can
    /// tear every session down once its dispatched task ends without downcasting a trait object.
    pub async fn close_all(&self) -> Result<()> {
        self.sessions.close_all().await
    }
}

fn wait_condition(input: &Value) -> Result<WaitCondition> {
    if let Some(selector) = get_opt_string(input, "selector") {
        return Ok(WaitCondition::Selector(selector));
    }
    if let Some(text) = get_opt_string(input, "text") {
        return Ok(WaitCondition::Text(text));
    }
    if get_bool_or(input, "network_idle", false) {
        return Ok(WaitCondition::NetworkIdle);
    }
    Err(missing("selector, text, or network_idle"))
}

/// A browser-tool wire name and the `AuthorityRequirement`/cost it's registered with — kept as
/// one small table so [`BrowserCapability::tools`] and its `to_action`/`invoke` counterparts stay
/// obviously in sync rather than drifting across three separate lists.
struct ToolSpec {
    name: &'static str,
    description: &'static str,
    input_schema: fn() -> Value,
    cost: CostClass,
}

fn schema_url() -> Value {
    json!({"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]})
}
fn schema_none() -> Value {
    json!({"type": "object", "properties": {}})
}
fn schema_tab() -> Value {
    json!({"type": "object", "properties": {"tab": {"type": "string"}}, "required": ["tab"]})
}
fn schema_ref() -> Value {
    json!({"type": "object", "properties": {"ref": {"type": "string"}}, "required": ["ref"]})
}
fn schema_type() -> Value {
    json!({
        "type": "object",
        "properties": {"ref": {"type": "string"}, "text": {"type": "string"}},
        "required": ["ref", "text"]
    })
}
fn schema_select() -> Value {
    json!({
        "type": "object",
        "properties": {"ref": {"type": "string"}, "value": {"type": "string"}},
        "required": ["ref", "value"]
    })
}
fn schema_press() -> Value {
    json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]})
}
fn schema_eval() -> Value {
    json!({"type": "object", "properties": {"js": {"type": "string"}}, "required": ["js"]})
}
fn schema_wait_for() -> Value {
    json!({
        "type": "object",
        "properties": {
            "selector": {"type": "string"},
            "text": {"type": "string"},
            "network_idle": {"type": "boolean"},
            "timeout_ms": {"type": "integer"}
        }
    })
}
fn schema_screenshot() -> Value {
    json!({"type": "object", "properties": {"full_page": {"type": "boolean"}}})
}
fn schema_set_cookie() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"}, "value": {"type": "string"},
            "domain": {"type": "string"}, "path": {"type": "string"},
            "secure": {"type": "boolean"}, "http_only": {"type": "boolean"}
        },
        "required": ["name", "domain"]
    })
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "browser.navigate",
        description: "Navigate the active tab to a URL.",
        input_schema: schema_url,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.open",
        description: "Open a new tab at a URL and make it active.",
        input_schema: schema_url,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.close",
        description: "Close this session's browser and release it.",
        input_schema: schema_none,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.list_tabs",
        description: "List every open tab.",
        input_schema: schema_none,
        cost: CostClass::Cheap,
    },
    ToolSpec {
        name: "browser.switch_tab",
        description: "Make a tab the active tab.",
        input_schema: schema_tab,
        cost: CostClass::Cheap,
    },
    ToolSpec {
        name: "browser.snapshot",
        description: "Accessibility-tree snapshot of the active tab, with stable refs.",
        input_schema: schema_none,
        cost: CostClass::Moderate,
    },
    ToolSpec {
        name: "browser.screenshot",
        description: "PNG screenshot of the active tab.",
        input_schema: schema_screenshot,
        cost: CostClass::Moderate,
    },
    ToolSpec {
        name: "browser.click",
        description: "Click the element at a ref from the last snapshot.",
        input_schema: schema_ref,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.hover",
        description: "Hover the element at a ref from the last snapshot.",
        input_schema: schema_ref,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.type",
        description: "Type text into the focusable element at a ref.",
        input_schema: schema_type,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.select",
        description: "Select a value in the <select> element at a ref.",
        input_schema: schema_select,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.press",
        description: "Press a keyboard key on the focused element.",
        input_schema: schema_press,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.eval",
        description: "Evaluate JavaScript in the active tab and return its JSON result.",
        input_schema: schema_eval,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.wait_for",
        description: "Block until a selector matches, text appears, or the network goes idle.",
        input_schema: schema_wait_for,
        cost: CostClass::Moderate,
    },
    ToolSpec {
        name: "browser.console",
        description: "Console messages collected since the last call.",
        input_schema: schema_none,
        cost: CostClass::Cheap,
    },
    ToolSpec {
        name: "browser.network",
        description: "Request/response log since the last call.",
        input_schema: schema_none,
        cost: CostClass::Moderate,
    },
    ToolSpec {
        name: "browser.cookies",
        description: "All cookies visible to the active tab.",
        input_schema: schema_none,
        cost: CostClass::Cheap,
    },
    ToolSpec {
        name: "browser.set_cookie",
        description: "Set a cookie.",
        input_schema: schema_set_cookie,
        cost: CostClass::Mutating,
    },
    ToolSpec {
        name: "browser.storage_state",
        description: "Cookies plus localStorage for the active tab's origin.",
        input_schema: schema_none,
        cost: CostClass::Moderate,
    },
    ToolSpec {
        name: "browser.pdf",
        description: "Render the active tab to a PDF.",
        input_schema: schema_none,
        cost: CostClass::Moderate,
    },
];

/// Every non-navigate browser tool acts on the already-navigated page; see this module's doc
/// comment on why they all map to the same opaque-origin `Action::BrowserNavigate`.
fn browser_action(tool: &str, input: &Value) -> Result<Action> {
    match tool {
        "browser.navigate" | "browser.open" => Ok(Action::BrowserNavigate {
            url: get_string(input, "url")?,
        }),
        _ => Ok(Action::BrowserNavigate {
            url: "about:blank".to_string(),
        }),
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for BrowserCapability {
    fn id(&self) -> &str {
        "browser"
    }

    fn tools(&self) -> Vec<ToolSchema> {
        TOOLS
            .iter()
            .map(|t| ToolSchema {
                name: t.name,
                description: t.description,
                input_schema: (t.input_schema)(),
                cost: t.cost,
                // A hermetic browser needs network reach to be worth anything at all — the same
                // structural gate `CapabilityProvider::requires` below declares for the whole
                // capability, restated per tool so admission (`SPEC.md` §30.1) and dispatch-time
                // `Authority::permits` agree on what "browser tools need" means.
                requires: AuthorityRequirement::Network,
            })
            .collect()
    }

    fn to_action(&self, tool: &str, input: &Value) -> Result<Action> {
        if !TOOLS.iter().any(|t| t.name == tool) {
            return Err(TmError::parse(format!("unknown tool `{tool}`")));
        }
        browser_action(tool, input)
    }

    fn requires(&self) -> AuthorityRequirement {
        AuthorityRequirement::Network
    }

    async fn invoke(&self, tool: &str, input: Value, ctx: &CallContext<'_>) -> Result<Value> {
        if tool == "browser.close" {
            self.sessions.close(ctx.ticket, ctx.session).await?;
            return Ok(json!({"closed": true}));
        }
        if !TOOLS.iter().any(|t| t.name == tool) {
            return Err(TmError::parse(format!("unknown tool `{tool}`")));
        }

        let slot = self.sessions.get_or_launch(ctx).await?;
        let mut guard = slot.lock().await;
        let s = guard.as_mut().ok_or_else(|| {
            TmError::invariant("browser session for this ticket/session was already closed")
        })?;

        match tool {
            "browser.navigate" => {
                let url = get_string(&input, "url")?;
                s.navigate(&url).await?;
                Ok(json!({"navigated": url}))
            }
            "browser.open" => {
                let url = get_string(&input, "url")?;
                let tab = s.open_tab(&url).await?;
                Ok(json!({"tab": tab.0}))
            }
            "browser.list_tabs" => {
                let tabs = s.list_tabs().await?;
                Ok(serde_json::to_value(&tabs)?)
            }
            "browser.switch_tab" => {
                let tab = crate::session::TabId(get_string(&input, "tab")?);
                s.switch_tab(tab).await?;
                Ok(json!({"switched": true}))
            }
            "browser.snapshot" => {
                let snapshot = s.snapshot().await?;
                Ok(serde_json::to_value(&snapshot)?)
            }
            "browser.screenshot" => {
                let full_page = get_bool_or(&input, "full_page", false);
                let r = s.screenshot(full_page).await?;
                Ok(serde_json::to_value(&r)?)
            }
            "browser.click" => {
                let reference = ax_ref(&input)?;
                s.click(&reference).await?;
                Ok(json!({"clicked": reference.0}))
            }
            "browser.hover" => {
                let reference = ax_ref(&input)?;
                s.hover(&reference).await?;
                Ok(json!({"hovered": reference.0}))
            }
            "browser.type" => {
                let reference = ax_ref(&input)?;
                let text = get_string(&input, "text")?;
                s.type_text(&reference, &text).await?;
                Ok(json!({"typed": text}))
            }
            "browser.select" => {
                let reference = ax_ref(&input)?;
                let value = get_string(&input, "value")?;
                s.select(&reference, &value).await?;
                Ok(json!({"selected": value}))
            }
            "browser.press" => {
                let key = get_string(&input, "key")?;
                s.press(&key).await?;
                Ok(json!({"pressed": key}))
            }
            "browser.eval" => s.eval(&get_string(&input, "js")?).await,
            "browser.wait_for" => {
                let condition = wait_condition(&input)?;
                let timeout = Duration::from_millis(get_u64_or(&input, "timeout_ms", 5_000));
                s.wait_for(condition, timeout).await?;
                Ok(json!({"satisfied": true}))
            }
            "browser.console" => {
                let messages = s.console().await?;
                Ok(serde_json::to_value(&messages)?)
            }
            "browser.network" => {
                let entries = s.network().await?;
                Ok(serde_json::to_value(&entries)?)
            }
            "browser.cookies" => {
                let cookies = s.cookies().await?;
                Ok(serde_json::to_value(&cookies)?)
            }
            "browser.set_cookie" => {
                let cookie = Cookie {
                    name: get_string(&input, "name")?,
                    value: get_opt_string(&input, "value").unwrap_or_default(),
                    domain: get_string(&input, "domain")?,
                    path: get_opt_string(&input, "path").unwrap_or_else(|| "/".to_string()),
                    secure: get_bool_or(&input, "secure", false),
                    http_only: get_bool_or(&input, "http_only", false),
                };
                s.set_cookie(cookie).await?;
                Ok(json!({"set": true}))
            }
            "browser.storage_state" => {
                let state = s.storage_state().await?;
                Ok(serde_json::to_value(&state)?)
            }
            "browser.pdf" => {
                let r = s.pdf().await?;
                Ok(serde_json::to_value(&r)?)
            }
            other => Err(TmError::parse(format!("unknown tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{BrowserCapabilities, BrowserEndpoint, BrowserProvider};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tm_types::{Authority, Decision, FixedClock, ParticipantId, SessionId as TmSessionId};

    struct NullSink;
    impl ArtifactSink for NullSink {
        fn store(&self, _bytes: &[u8], _content_type: &str) -> Result<tm_types::ArtifactId> {
            tm_types::ArtifactId::new("A-test")
        }
    }

    /// A provider whose acquisitions always fail to connect (port 0), so [`SessionRegistry`]
    /// tests exercise the launch-failure path without a real browser.
    struct UnreachableProvider {
        acquire_calls: AtomicUsize,
    }

    impl UnreachableProvider {
        fn new() -> Self {
            UnreachableProvider {
                acquire_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl BrowserProvider for UnreachableProvider {
        fn id(&self) -> &str {
            "unreachable"
        }

        fn capabilities(&self) -> BrowserCapabilities {
            BrowserCapabilities::default()
        }

        async fn acquire(&self, _req: &SessionRequest) -> Result<BrowserEndpoint> {
            self.acquire_calls.fetch_add(1, Ordering::SeqCst);
            Ok(BrowserEndpoint {
                provider_id: self.id().to_string(),
                endpoint_id: "E-unreachable".to_string(),
                ws_url: "ws://127.0.0.1:0/devtools/browser/does-not-exist".to_string(),
                browser_version: None,
            })
        }

        async fn release(&self, _endpoint: &BrowserEndpoint) -> Result<()> {
            Ok(())
        }
    }

    fn test_registry() -> SessionRegistry {
        SessionRegistry::new(
            Arc::new(
                ProviderRegistry::new(
                    vec![Arc::new(UnreachableProvider::new())],
                    vec!["unreachable".to_string()],
                )
                .unwrap(),
            ),
            Arc::new(NullSink),
            Arc::new(FixedClock::epoch()),
            1024,
        )
    }

    fn test_ctx<'a>(
        authority: &'a Authority,
        ticket: &'a TicketId,
        session: &'a TmSessionId,
        actor: &'a ParticipantId,
        clock: &'a FixedClock,
        ids: &'a tm_types::TestIds,
        root: &'a std::path::Path,
    ) -> CallContext<'a> {
        CallContext {
            authority,
            ticket,
            session,
            actor,
            clock,
            ids,
            root,
        }
    }

    #[test]
    fn every_tool_has_a_unique_name_and_a_real_schema() {
        let cap = BrowserCapability::new(test_registry());
        let tools = cap.tools();
        assert_eq!(tools.len(), TOOLS.len());
        let unique: std::collections::BTreeSet<&str> = tools.iter().map(|t| t.name).collect();
        assert_eq!(unique.len(), tools.len());
        for t in &tools {
            assert!(t.name.starts_with("browser."));
            assert!(t.input_schema.is_object());
        }
    }

    #[test]
    fn navigate_and_open_map_to_browser_navigate_with_the_requested_url() {
        let cap = BrowserCapability::new(test_registry());
        let action = cap
            .to_action("browser.navigate", &json!({"url": "https://example.com"}))
            .unwrap();
        assert_eq!(
            action,
            Action::BrowserNavigate {
                url: "https://example.com".to_string()
            }
        );
        let action = cap
            .to_action("browser.open", &json!({"url": "https://example.com"}))
            .unwrap();
        assert_eq!(
            action,
            Action::BrowserNavigate {
                url: "https://example.com".to_string()
            }
        );
    }

    #[test]
    fn interaction_tools_map_to_an_opaque_origin() {
        let cap = BrowserCapability::new(test_registry());
        for tool in ["browser.click", "browser.snapshot", "browser.eval"] {
            let action = cap
                .to_action(tool, &json!({"ref": "e1", "js": "1"}))
                .unwrap();
            assert_eq!(
                action,
                Action::BrowserNavigate {
                    url: "about:blank".to_string()
                }
            );
        }
    }

    #[test]
    fn browser_navigate_is_denied_when_authority_network_does_not_permit_the_origin() {
        // The requirement the task calls out explicitly: a `browser.navigate` call must be
        // denied by `Authority::permits` when the target origin is outside what
        // `Authority.network` allows — not merely that the tool exists in the registry.
        let cap = BrowserCapability::new(test_registry());
        let mut authority = Authority::none();
        authority
            .network
            .allowlist
            .insert("allowed.example".to_string());

        let denied_action = cap
            .to_action(
                "browser.navigate",
                &json!({"url": "https://evil.example/x"}),
            )
            .unwrap();
        assert!(matches!(
            authority.permits(&denied_action),
            Decision::Deny(_)
        ));

        let allowed_action = cap
            .to_action(
                "browser.navigate",
                &json!({"url": "https://allowed.example/x"}),
            )
            .unwrap();
        assert_eq!(authority.permits(&allowed_action), Decision::Allow);
    }

    #[test]
    fn provider_requires_network_authority() {
        let cap = BrowserCapability::new(test_registry());
        assert!(!cap.requires().admits(&Authority::none()));
        let mut with_network = Authority::none();
        with_network.network.arbitrary = true;
        assert!(cap.requires().admits(&with_network));
    }

    #[tokio::test]
    async fn session_registry_fails_to_launch_against_an_unreachable_endpoint() {
        let registry = test_registry();
        let authority = Authority::root();
        let ticket: TicketId = "T-1".parse().unwrap();
        let session: TmSessionId = "S-1".parse().unwrap();
        let actor: ParticipantId = "agent:test/worker".parse().unwrap();
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::new();
        let root = std::env::temp_dir();
        let ctx = test_ctx(&authority, &ticket, &session, &actor, &clock, &ids, &root);

        let result = registry.get_or_launch(&ctx).await;
        assert!(
            result.is_err(),
            "an unreachable CDP endpoint must fail launch"
        );
        assert_eq!(registry.live_count().await, 0);
    }

    #[tokio::test]
    async fn close_all_on_an_empty_registry_is_a_harmless_no_op() {
        let registry = test_registry();
        registry.close_all().await.unwrap();
        assert_eq!(registry.live_count().await, 0);
    }

    #[tokio::test]
    async fn close_on_a_key_that_never_launched_is_a_harmless_no_op() {
        let registry = test_registry();
        let ticket: TicketId = "T-1".parse().unwrap();
        let session: TmSessionId = "S-1".parse().unwrap();
        registry.close(&ticket, &session).await.unwrap();
    }
}
