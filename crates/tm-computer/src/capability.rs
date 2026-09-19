//! [`ComputerCapability`]: tm-computer's [`tm_types::CapabilityProvider`] impl
//! (`docs/audit-2026-09-18-fable.md` B-02, `SPEC.md` §20.2/§20.5).
//!
//! Wraps [`crate::session::ComputerSession`]'s methods as dotted `computer.*` tools, mirroring
//! `tm_browser::capability::BrowserCapability`'s shape exactly (same `SessionRegistry` pattern,
//! same "one session per `(ticket, session)` key, `Arc<Mutex<Option<Session>>>` to route around a
//! by-value `close`" idiom — see that module's doc comment for the reasoning this one reuses
//! rather than re-deriving; `tm-computer` has no by-value `close` to route around since
//! [`crate::session::ComputerSession`] has no async teardown of its own, but the `Option` slot
//! still exists so [`SessionRegistry::close`] can drop it cleanly under the lock).
//!
//! # Action mapping (`SPEC.md` §20.5)
//!
//! `Action::ComputerCapture` for read/observation tools (`capabilities`, `snapshot`,
//! `screenshot`, `windows`); `Action::ComputerClipboard` for `clipboard_get`/`clipboard_set`;
//! `Action::ComputerInput` for everything that synthesizes input or mutates desktop/window state
//! (click family, type, key, scroll, focus/move/resize window, launch/quit) — §20.5 names only
//! three computer action classes, so window management and app launch/quit — which have no
//! dedicated class — join `ComputerInput` as the closest-blast-radius bucket, the same "no exact
//! match, use the nearest existing governance switch" convention `tm-agent::tools`'s module doc
//! documents for `git.worktree`/`ticket.comment`.
//!
//! # Approval is not faked here
//!
//! On macOS every [`crate::session::ComputerSession`] is [`crate::session::SessionMode::Attended`]
//! and starts unapproved (`SPEC.md` §20.3/§4.4): [`crate::session::ComputerSession::act`] returns
//! [`crate::ComputerError::ApprovalRequired`] until [`crate::session::ComputerSession::approve`]
//! is called. This module never calls `approve` on an agent's behalf — doing so would fake the
//! human-in-the-loop consent §20.3 requires. An attended session's `computer.click`/`type`/...
//! calls surface `ApprovalRequired` as a clear tool error instead; wiring a real approval
//! hand-off (`Store`'s `approval.*` events, B-07) is out of this change's scope. The panic-stop
//! policy ([`crate::session::PanicStop`]) is likewise not polled here: it needs a background
//! cursor-sampling loop this module does not run, so only the approval gate — not the panic
//! stop — is enforced on this path today.
//!
//! # Idempotent effects (`SPEC.md` §21.5, audit B-11) — deliberately not wired here
//!
//! B-11's mechanism (`tm_core::Store::begin_effect`/`EffectGuard`) is keyed on `(ticket, attempt,
//! kind, canonical_args)` and exists for effects that are individually irreversible and
//! individually re-runnable in a well-defined way (a git push, a mirror write). There is no
//! `computer.input` tool here to wrap — [`TOOLS`] instead has a dozen granular primitives
//! (`computer.click`/`double_click`/`right_click`/`drag`/`type`/`key`/`scroll`/`focus`/
//! `move_window`/`resize_window`/`launch`/`quit`), any one of which might be a single meaningless
//! step inside a longer sequence (an agent typing a password field one keystroke at a time) or,
//! rarely, an effect with real consequence (clicking "Pay Now" in a native app) — and this module
//! has no way to tell which from the tool call alone: `computer.click`'s input is a screen point
//! or an accessibility-tree ref with no semantic label, `computer.key` is a raw key chord, and
//! neither has a receipt-shaped, replayable outcome the way a `git commit`'s SHA or a GitHub
//! issue's number does (re-issuing the identical click/keystroke against a desktop that has since
//! changed state has no reliable "was this already done" answer to give). Wrapping arbitrary
//! input in a "one effect, one receipt" guard would not fit the model; per B-11's own guidance
//! this is scoped down rather than forced, and computer wrapping is skipped entirely on this
//! pass — same call as `tm_browser::capability` makes for the analogous `browser.click`/`type`
//! case, see that module's doc comment.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use tm_types::{
    Action, AuthorityRequirement, CallContext, CapabilityProvider, CostClass, Result, SessionId,
    TicketId, TmError, ToolSchema,
};

use crate::backend::{self, SelectionEnv};
use crate::input::{InputAction, InputTarget, KeyChord, MouseButton, Point, ScrollDelta};
use crate::session::{ComputerSession, PanicStop, PanicStopConfig, SessionMode};

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

fn get_f64(input: &Value, field: &str) -> Result<f64> {
    input
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| missing(field))
}

fn get_bool_or(input: &Value, field: &str, default: bool) -> bool {
    input.get(field).and_then(Value::as_bool).unwrap_or(default)
}

/// Parse `{"ref": "..."}` or `{"x": .., "y": ..}` into an [`InputTarget`].
fn parse_target(v: &Value) -> Result<InputTarget> {
    if let Some(r) = v.get("ref").and_then(Value::as_str) {
        return Ok(InputTarget::ElementRef(r.to_string()));
    }
    Ok(InputTarget::Point(Point::new(
        get_f64(v, "x")?,
        get_f64(v, "y")?,
    )))
}

fn parse_button(input: &Value) -> MouseButton {
    match input.get("button").and_then(Value::as_str) {
        Some("right") => MouseButton::Right,
        Some("middle") => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

/// A key identifying one live computer session. `CallContext` does not carry a lease id, so
/// `(ticket, session)` is the closest available substitute, matching
/// `tm_browser::capability::SessionRegistry`'s same choice.
type SessionKey = (TicketId, SessionId);

/// Lazily launches, reuses, and explicitly tears down one [`ComputerSession`] per
/// `(ticket, session)` key, via [`crate::backend::open_selected`].
pub struct SessionRegistry {
    env: SelectionEnv,
    headless: bool,
    panic_stop_config: PanicStopConfig,
    sessions: AsyncMutex<HashMap<SessionKey, Arc<AsyncMutex<Option<ComputerSession>>>>>,
}

impl SessionRegistry {
    /// Build an empty registry. `env` selects the backend (mirrors `tm computer`'s own
    /// `SelectionEnv::from_process` plus `TM_COMPUTER_BACKEND` override); `headless` requests a
    /// private `Xvfb` display where the platform supports one (Linux only — see
    /// `SPEC.md` §20.3, `crate::session::SessionMode::Headless`).
    pub fn new(env: SelectionEnv, headless: bool, panic_stop_config: PanicStopConfig) -> Self {
        SessionRegistry {
            env,
            headless,
            panic_stop_config,
            sessions: AsyncMutex::new(HashMap::new()),
        }
    }

    fn session_mode(&self) -> Result<SessionMode> {
        if !self.headless {
            return Ok(SessionMode::Attended);
        }
        #[cfg(target_os = "linux")]
        {
            Ok(SessionMode::Headless {
                display: std::env::var("DISPLAY").unwrap_or_default(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(TmError::Invariant(
                "headless computer sessions are only supported on Linux".to_string(),
            ))
        }
    }

    /// Return the session for `(ctx.ticket, ctx.session)`, constructing one on first use. The
    /// registry's lock is held across backend construction, matching
    /// `tm_browser::capability::SessionRegistry::get_or_launch`'s same trade (never launch two
    /// backends for one key).
    async fn get_or_launch(
        &self,
        ctx: &CallContext<'_>,
    ) -> Result<Arc<AsyncMutex<Option<ComputerSession>>>> {
        let key = (ctx.ticket.clone(), ctx.session.clone());
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(&key) {
            return Ok(existing.clone());
        }

        let backend = backend::open_selected(&self.env)?;
        let mode = self.session_mode()?;
        let panic_stop = PanicStop::new(self.panic_stop_config.clone());
        let session = ComputerSession::new(backend, mode, panic_stop);

        let slot = Arc::new(AsyncMutex::new(Some(session)));
        sessions.insert(key, slot.clone());
        Ok(slot)
    }

    /// Explicitly tear down the session for `(ticket, session)`, if one is live. A no-op
    /// otherwise.
    pub async fn close(&self, ticket: &TicketId, session: &SessionId) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        sessions.remove(&(ticket.clone(), session.clone()));
        Ok(())
    }

    /// Drop every session this registry currently holds. See this module's doc comment (and
    /// `tm_browser::capability::SessionRegistry`'s, which this mirrors) for why this — called by
    /// the owning executor once its dispatched task ends — stands in for true
    /// lease-expiry-triggered teardown rather than faking one.
    pub async fn close_all(&self) -> Result<()> {
        self.sessions.lock().await.clear();
        Ok(())
    }

    /// How many sessions are currently live. Test/diagnostic use.
    pub async fn live_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

/// tm-computer's [`CapabilityProvider`]: every `SPEC.md` §20.2 tool over [`SessionRegistry`].
pub struct ComputerCapability {
    sessions: SessionRegistry,
}

impl ComputerCapability {
    /// Build the computer capability over an already-constructed [`SessionRegistry`].
    pub fn new(sessions: SessionRegistry) -> Self {
        ComputerCapability { sessions }
    }

    /// Passthrough to [`SessionRegistry::close_all`] — see
    /// `tm_browser::capability::BrowserCapability::close_all`'s doc comment for why this exists
    /// alongside the trait-object registration rather than only on `SessionRegistry`.
    pub async fn close_all(&self) -> Result<()> {
        self.sessions.close_all().await
    }
}

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    input_schema: fn() -> Value,
    cost: CostClass,
    requires: AuthorityRequirement,
}

fn schema_none() -> Value {
    json!({"type": "object", "properties": {}})
}
fn schema_force_screenshot() -> Value {
    json!({"type": "object", "properties": {"force_screenshot": {"type": "boolean"}}})
}
fn schema_target_button() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ref": {"type": "string"}, "x": {"type": "number"}, "y": {"type": "number"},
            "button": {"type": "string", "enum": ["left", "right", "middle"]}
        }
    })
}
fn schema_target() -> Value {
    json!({
        "type": "object",
        "properties": {"ref": {"type": "string"}, "x": {"type": "number"}, "y": {"type": "number"}}
    })
}
fn schema_drag() -> Value {
    json!({
        "type": "object",
        "properties": {
            "from": {"type": "object"}, "to": {"type": "object"}
        },
        "required": ["from", "to"]
    })
}
fn schema_type() -> Value {
    json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
}
fn schema_key() -> Value {
    json!({"type": "object", "properties": {"chord": {"type": "string"}}, "required": ["chord"]})
}
fn schema_scroll() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ref": {"type": "string"}, "x": {"type": "number"}, "y": {"type": "number"},
            "dx": {"type": "number"}, "dy": {"type": "number"}
        },
        "required": ["dx", "dy"]
    })
}
fn schema_window() -> Value {
    json!({"type": "object", "properties": {"window": {"type": "string"}}, "required": ["window"]})
}
fn schema_move_window() -> Value {
    json!({
        "type": "object",
        "properties": {
            "window": {"type": "string"}, "x": {"type": "number"}, "y": {"type": "number"}
        },
        "required": ["window", "x", "y"]
    })
}
fn schema_resize_window() -> Value {
    json!({
        "type": "object",
        "properties": {
            "window": {"type": "string"}, "width": {"type": "number"}, "height": {"type": "number"}
        },
        "required": ["window", "width", "height"]
    })
}
fn schema_clipboard_set() -> Value {
    json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
}
fn schema_app() -> Value {
    json!({"type": "object", "properties": {"app": {"type": "string"}}, "required": ["app"]})
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "computer.capabilities",
        description: "What this backend can currently do, and what's missing if anything.",
        input_schema: schema_none,
        cost: CostClass::Cheap,
        requires: AuthorityRequirement::ComputerCapture,
    },
    ToolSpec {
        name: "computer.snapshot",
        description: "Element tree with refs, or a screenshot when the tree is unavailable.",
        input_schema: schema_force_screenshot,
        cost: CostClass::Moderate,
        requires: AuthorityRequirement::ComputerCapture,
    },
    ToolSpec {
        name: "computer.screenshot",
        description: "Screenshot of the desktop.",
        input_schema: schema_none,
        cost: CostClass::Moderate,
        requires: AuthorityRequirement::ComputerCapture,
    },
    ToolSpec {
        name: "computer.windows",
        description: "List every top-level window.",
        input_schema: schema_none,
        cost: CostClass::Cheap,
        requires: AuthorityRequirement::ComputerCapture,
    },
    ToolSpec {
        name: "computer.click",
        description: "Click at a ref or a point.",
        input_schema: schema_target_button,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.double_click",
        description: "Double-click at a ref or a point.",
        input_schema: schema_target,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.right_click",
        description: "Right-click at a ref or a point.",
        input_schema: schema_target,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.drag",
        description: "Press-move-release drag from one target to another.",
        input_schema: schema_drag,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.type",
        description: "Type literal text via the platform's Unicode input path.",
        input_schema: schema_type,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.key",
        description: "Press and release a key chord, e.g. \"cmd+shift+4\".",
        input_schema: schema_key,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.scroll",
        description: "Scroll or pan at a ref or a point.",
        input_schema: schema_scroll,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.focus",
        description: "Bring a window to focus.",
        input_schema: schema_window,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.move_window",
        description: "Move a window's origin.",
        input_schema: schema_move_window,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.resize_window",
        description: "Resize a window.",
        input_schema: schema_resize_window,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.clipboard_get",
        description: "Read the system clipboard as text.",
        input_schema: schema_none,
        cost: CostClass::Cheap,
        requires: AuthorityRequirement::ComputerClipboard,
    },
    ToolSpec {
        name: "computer.clipboard_set",
        description: "Write the system clipboard.",
        input_schema: schema_clipboard_set,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerClipboard,
    },
    ToolSpec {
        name: "computer.launch",
        description: "Launch an application.",
        input_schema: schema_app,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
    ToolSpec {
        name: "computer.quit",
        description: "Quit a running application.",
        input_schema: schema_app,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::ComputerInput,
    },
];

fn computer_action(tool: &str) -> Result<Action> {
    match tool {
        "computer.capabilities"
        | "computer.snapshot"
        | "computer.screenshot"
        | "computer.windows" => Ok(Action::ComputerCapture),
        "computer.clipboard_get" | "computer.clipboard_set" => Ok(Action::ComputerClipboard),
        "computer.click"
        | "computer.double_click"
        | "computer.right_click"
        | "computer.drag"
        | "computer.type"
        | "computer.key"
        | "computer.scroll"
        | "computer.focus"
        | "computer.move_window"
        | "computer.resize_window"
        | "computer.launch"
        | "computer.quit" => Ok(Action::ComputerInput),
        other => Err(TmError::parse(format!("unknown tool `{other}`"))),
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for ComputerCapability {
    fn id(&self) -> &str {
        "computer"
    }

    fn tools(&self) -> Vec<ToolSchema> {
        TOOLS
            .iter()
            .map(|t| ToolSchema {
                name: t.name,
                description: t.description,
                input_schema: (t.input_schema)(),
                cost: t.cost,
                requires: t.requires.clone(),
            })
            .collect()
    }

    fn to_action(&self, tool: &str, _input: &Value) -> Result<Action> {
        computer_action(tool)
    }

    fn requires(&self) -> AuthorityRequirement {
        AuthorityRequirement::Any(vec![
            AuthorityRequirement::ComputerInput,
            AuthorityRequirement::ComputerCapture,
            AuthorityRequirement::ComputerClipboard,
        ])
    }

    async fn invoke(&self, tool: &str, input: Value, ctx: &CallContext<'_>) -> Result<Value> {
        if !TOOLS.iter().any(|t| t.name == tool) {
            return Err(TmError::parse(format!("unknown tool `{tool}`")));
        }

        let slot = self.sessions.get_or_launch(ctx).await?;
        let mut guard = slot.lock().await;
        let s = guard.as_mut().ok_or_else(|| {
            TmError::invariant("computer session for this ticket/session was already closed")
        })?;
        let now = ctx.clock.now();

        match tool {
            "computer.capabilities" => {
                let caps = s.capabilities().await?;
                Ok(serde_json::to_value(&caps)?)
            }
            "computer.snapshot" => {
                let force_screenshot = get_bool_or(&input, "force_screenshot", false);
                let snapshot = s.snapshot(now, force_screenshot).await?;
                Ok(serde_json::to_value(&snapshot)?)
            }
            "computer.screenshot" => {
                let snapshot = s.snapshot(now, true).await?;
                match snapshot.screenshot {
                    Some(shot) => Ok(serde_json::to_value(&shot)?),
                    None => Err(TmError::invariant(
                        "force_screenshot snapshot returned no screenshot",
                    )),
                }
            }
            "computer.windows" => {
                let windows = s.windows().await?;
                Ok(serde_json::to_value(&windows)?)
            }
            "computer.click" => {
                let target = parse_target(&input)?;
                let button = parse_button(&input);
                s.act(InputAction::Click { target, button }).await?;
                Ok(json!({"clicked": true}))
            }
            "computer.double_click" => {
                let target = parse_target(&input)?;
                s.act(InputAction::DoubleClick { target }).await?;
                Ok(json!({"double_clicked": true}))
            }
            "computer.right_click" => {
                let target = parse_target(&input)?;
                s.act(InputAction::RightClick { target }).await?;
                Ok(json!({"right_clicked": true}))
            }
            "computer.drag" => {
                let from = parse_target(input.get("from").ok_or_else(|| missing("from"))?)?;
                let to = parse_target(input.get("to").ok_or_else(|| missing("to"))?)?;
                s.act(InputAction::Drag { from, to }).await?;
                Ok(json!({"dragged": true}))
            }
            "computer.type" => {
                let text = get_string(&input, "text")?;
                s.act(InputAction::TypeText(text.clone())).await?;
                Ok(json!({"typed": text}))
            }
            "computer.key" => {
                let chord_str = get_string(&input, "chord")?;
                let chord = KeyChord::parse(&chord_str)?;
                s.act(InputAction::KeyChord(chord)).await?;
                Ok(json!({"pressed": chord_str}))
            }
            "computer.scroll" => {
                let target = parse_target(&input)?;
                let delta = ScrollDelta {
                    dx: get_f64(&input, "dx")?,
                    dy: get_f64(&input, "dy")?,
                };
                s.act(InputAction::Scroll { target, delta }).await?;
                Ok(json!({"scrolled": true}))
            }
            "computer.focus" => {
                let window = get_string(&input, "window")?;
                s.focus(&window).await?;
                Ok(json!({"focused": window}))
            }
            "computer.move_window" => {
                let window = get_string(&input, "window")?;
                let to = Point::new(get_f64(&input, "x")?, get_f64(&input, "y")?);
                s.move_window(&window, to).await?;
                Ok(json!({"moved": window}))
            }
            "computer.resize_window" => {
                let window = get_string(&input, "window")?;
                let width = get_f64(&input, "width")?;
                let height = get_f64(&input, "height")?;
                s.resize_window(&window, width, height).await?;
                Ok(json!({"resized": window}))
            }
            "computer.clipboard_get" => {
                let text = s.clipboard_get().await?;
                Ok(json!({"text": text}))
            }
            "computer.clipboard_set" => {
                let text = get_string(&input, "text")?;
                s.clipboard_set(&text).await?;
                Ok(json!({"set": true}))
            }
            "computer.launch" => {
                let app = get_string(&input, "app")?;
                s.launch(&app).await?;
                Ok(json!({"launched": app}))
            }
            "computer.quit" => {
                let app = get_string(&input, "app")?;
                s.quit(&app).await?;
                Ok(json!({"quit": app}))
            }
            other => Err(TmError::parse(format!("unknown tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{Authority, Decision, FixedClock, ParticipantId};

    fn test_registry() -> SessionRegistry {
        SessionRegistry::new(
            SelectionEnv {
                tm_computer_backend: Some("does-not-exist".to_string()),
                ..SelectionEnv::default()
            },
            false,
            PanicStopConfig {
                abort_chord: None,
                mouse_move_threshold_px: 5.0,
            },
        )
    }

    #[test]
    fn every_tool_has_a_unique_name_and_a_real_schema() {
        let cap = ComputerCapability::new(test_registry());
        let tools = cap.tools();
        assert_eq!(tools.len(), TOOLS.len());
        let unique: std::collections::BTreeSet<&str> = tools.iter().map(|t| t.name).collect();
        assert_eq!(unique.len(), tools.len());
        for t in &tools {
            assert!(t.name.starts_with("computer."));
            assert!(t.input_schema.is_object());
        }
    }

    #[test]
    fn action_mapping_covers_every_tool_and_uses_the_right_bucket() {
        let cap = ComputerCapability::new(test_registry());
        for t in TOOLS {
            let action = cap.to_action(t.name, &json!({})).unwrap();
            match t.name {
                "computer.capabilities"
                | "computer.snapshot"
                | "computer.screenshot"
                | "computer.windows" => assert_eq!(action, Action::ComputerCapture),
                "computer.clipboard_get" | "computer.clipboard_set" => {
                    assert_eq!(action, Action::ComputerClipboard)
                }
                _ => assert_eq!(action, Action::ComputerInput),
            }
        }
    }

    #[test]
    fn computer_input_and_clipboard_tools_are_denied_without_the_matching_authority() {
        // The requirement the task calls out explicitly: computer.input/computer.clipboard
        // calls must be denied by `Authority::permits` when the matching authority field is
        // absent, mirroring the browser.navigate authority-denial test.
        let cap = ComputerCapability::new(test_registry());
        let none = Authority::none();

        let click_action = cap.to_action("computer.click", &json!({})).unwrap();
        assert!(matches!(none.permits(&click_action), Decision::Deny(_)));

        let clipboard_action = cap.to_action("computer.clipboard_set", &json!({})).unwrap();
        assert!(matches!(none.permits(&clipboard_action), Decision::Deny(_)));

        let mut granted = Authority::none();
        granted.computer.input = true;
        granted.computer.clipboard = true;
        assert_eq!(granted.permits(&click_action), Decision::Allow);
        assert_eq!(granted.permits(&clipboard_action), Decision::Allow);
    }

    #[test]
    fn provider_requires_any_computer_authority() {
        let cap = ComputerCapability::new(test_registry());
        assert!(!cap.requires().admits(&Authority::none()));
        let mut with_capture = Authority::none();
        with_capture.computer.capture = true;
        assert!(cap.requires().admits(&with_capture));
    }

    #[test]
    fn per_tool_requirement_excludes_the_tool_when_its_authority_is_absent() {
        let cap = ComputerCapability::new(test_registry());
        let none = Authority::none();
        let tools = cap.tools();
        let click = tools.iter().find(|t| t.name == "computer.click").unwrap();
        assert!(!click.requires.admits(&none));

        let mut with_input = Authority::none();
        with_input.computer.input = true;
        assert!(click.requires.admits(&with_input));

        let clipboard = tools
            .iter()
            .find(|t| t.name == "computer.clipboard_get")
            .unwrap();
        assert!(!clipboard.requires.admits(&with_input));
        let mut with_clipboard = Authority::none();
        with_clipboard.computer.clipboard = true;
        assert!(clipboard.requires.admits(&with_clipboard));
    }

    #[tokio::test]
    async fn session_registry_fails_to_launch_against_an_unsupported_backend_override() {
        let registry = test_registry();
        let authority = Authority::root();
        let ticket: TicketId = "T-1".parse().unwrap();
        let session: SessionId = "S-1".parse().unwrap();
        let actor: ParticipantId = "agent:test/worker".parse().unwrap();
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::new();
        let root = std::env::temp_dir();
        let ctx = CallContext {
            authority: &authority,
            ticket: &ticket,
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: &root,
        };

        let result = registry.get_or_launch(&ctx).await;
        assert!(
            result.is_err(),
            "an unrecognized TM_COMPUTER_BACKEND override must fail to select, not panic"
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
        let session: SessionId = "S-1".parse().unwrap();
        registry.close(&ticket, &session).await.unwrap();
    }
}
