//! ACP wire types: the subset of the Agent Client Protocol schema this crate implements.
//!
//! Field shapes (names, casing, required-ness) were taken from the real schema —
//! `https://github.com/agentclientprotocol/agent-client-protocol`'s latest release
//! `schema.json` — not guessed, for `initialize`, `session/new`, `session/prompt`,
//! `session/request_permission` and `session/update`. Per this crate's scope (a real client +
//! server for those methods, not full spec coverage), types this crate doesn't act on are
//! either omitted, modeled loosely as `serde_json::Value`, or captured via `#[serde(flatten)]`
//! into a catch-all map so round-tripping never silently drops fields this crate doesn't
//! understand — see [`ContentBlock`] and [`ToolCallUpdate`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The wire method/notification names this crate speaks, exactly as the schema spells them.
pub mod methods {
    /// `x-side: agent` — sent by the client, handled by the agent.
    pub const INITIALIZE: &str = "initialize";
    /// `x-side: agent`.
    pub const SESSION_NEW: &str = "session/new";
    /// `x-side: agent`.
    pub const SESSION_PROMPT: &str = "session/prompt";
    /// `x-side: client` — sent by the agent, handled by the client.
    pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
    /// `x-side: client`, a notification (no response).
    pub const SESSION_UPDATE: &str = "session/update";
}

/// The ACP protocol version this crate implements (an integer, "only bumped for breaking
/// changes" per the schema's own doc comment on `ProtocolVersion`). Confirmed as the current
/// value against `agentclientprotocol.com`'s initialization docs (example payloads show
/// `"protocolVersion": 1`).
pub const PROTOCOL_VERSION: u16 = 1;

/// Metadata about the implementation on either side of the connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    /// Programmatic/logical name.
    pub name: String,
    /// Human-readable display name, if different from `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Implementation version string.
    pub version: String,
}

/// File-system capabilities a client advertises.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileSystemCapabilities {
    /// Whether the client can serve `fs/read_text_file`.
    pub read_text_file: bool,
    /// Whether the client can serve `fs/write_text_file`.
    pub write_text_file: bool,
}

/// Capabilities [`InitializeRequest::client_capabilities`] advertises. Modeled to the depth this
/// crate actually needs (`fs`/`terminal`); other real schema fields (`session`, `auth`,
/// `elicitation`) are accepted-and-ignored via `extra` rather than causing a deserialize failure
/// on a real agent's richer request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientCapabilities {
    /// File system capabilities.
    pub fs: FileSystemCapabilities,
    /// Whether the client supports `terminal/*` methods.
    pub terminal: bool,
    /// Every other field a real client/agent may send, preserved rather than dropped.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Prompt content-type capabilities an agent advertises.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptCapabilities {
    /// Whether `ContentBlock::Image` may appear in a prompt.
    pub image: bool,
    /// Whether `ContentBlock::Audio` may appear in a prompt.
    pub audio: bool,
    /// Whether `ContentBlock::Resource` (embedded, not linked) may appear in a prompt.
    pub embedded_context: bool,
}

/// Capabilities [`InitializeResponse::agent_capabilities`] advertises. As with
/// [`ClientCapabilities`], modeled to the depth this crate needs; everything else round-trips
/// through `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentCapabilities {
    /// Whether the agent supports `session/load`.
    pub load_session: bool,
    /// Prompt content-type capabilities.
    pub prompt_capabilities: PromptCapabilities,
    /// Every other field, preserved rather than dropped.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `initialize` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    /// The latest protocol version the sender supports.
    pub protocol_version: u16,
    /// Capabilities the client supports.
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
    /// Client name/version, if supplied.
    #[serde(default)]
    pub client_info: Option<Implementation>,
}

/// `initialize` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    /// The version the agent negotiated (same as requested, or the agent's own latest — see
    /// [`PROTOCOL_VERSION`]'s doc comment and this crate's negotiation logic in
    /// `crate::client::AcpClient::initialize`).
    pub protocol_version: u16,
    /// Capabilities the agent supports.
    #[serde(default)]
    pub agent_capabilities: AgentCapabilities,
    /// Authentication methods the agent supports (not acted on by this crate's MVP — no
    /// `authenticate` call is made; see this crate's top-level doc comment on scope).
    #[serde(default)]
    pub auth_methods: Vec<Value>,
    /// Agent name/version, if supplied.
    #[serde(default)]
    pub agent_info: Option<Implementation>,
}

/// `session/new` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    /// Absolute working directory for the session.
    pub cwd: String,
    /// Extra workspace roots beyond `cwd`.
    #[serde(default)]
    pub additional_directories: Vec<String>,
    /// MCP servers the agent should connect to. This crate's MVP always sends an empty list
    /// (it is not an MCP client — see `crates/tm-mcp`, a sibling crate, for that) but still
    /// includes the field since the schema marks it required.
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
}

/// `session/new` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    /// The session id to use in subsequent `session/*` calls. Opaque per the schema (`type:
    /// string`) — unrelated to `tm_types::SessionId`'s own `S-<n>` format, deliberately: this is
    /// the *ACP* session's identity, not Ticketmaster's.
    pub session_id: String,
    /// Initial mode state, if the agent supports session modes. Not acted on by this crate.
    #[serde(default)]
    pub modes: Option<Value>,
    /// Initial session configuration options, if any. Not acted on by this crate.
    #[serde(default)]
    pub config_options: Option<Value>,
}

/// A displayable content block. Only the `text` variant (which every agent MUST support per the
/// spec) is modeled with a typed field; every other real variant (`image`/`audio`/
/// `resource_link`/`resource`) still round-trips because `extra` captures whatever fields
/// accompany a `type` this crate does not otherwise interpret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContentBlock {
    /// The discriminator: `"text"`, `"image"`, `"audio"`, `"resource_link"`, or `"resource"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Present when `kind == "text"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Every field this type doesn't otherwise model (`uri`/`mimeType`/`data`/... for the other
    /// variants), preserved rather than dropped.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ContentBlock {
    /// Build a `"text"` content block — the baseline variant both sides of ACP must support.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock {
            kind: "text".to_string(),
            text: Some(text.into()),
            extra: Map::new(),
        }
    }
}

/// `session/prompt` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    /// Which session this prompt belongs to.
    pub session_id: String,
    /// The content blocks composing the user's message.
    pub prompt: Vec<ContentBlock>,
}

/// Why an agent stopped processing a prompt turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The turn ended successfully.
    EndTurn,
    /// The agent hit its token ceiling.
    MaxTokens,
    /// The agent hit its allowed-request ceiling for the turn.
    MaxTurnRequests,
    /// The agent refused to continue.
    Refusal,
    /// The client cancelled the turn via `session/cancel` (not implemented by this crate's
    /// client — see this crate's top-level scope note).
    Cancelled,
}

/// `session/prompt` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    /// Why the agent stopped.
    pub stop_reason: StopReason,
}

/// Categories of tool a `ToolCallUpdate` may describe. Used by [`crate::permission`] to decide
/// which [`tm_types::Action`] (if any) a permission request maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reading files or data.
    Read,
    /// Modifying files or content.
    Edit,
    /// Removing files or data.
    Delete,
    /// Moving or renaming files.
    Move,
    /// Searching for information.
    Search,
    /// Running commands or code.
    Execute,
    /// Internal reasoning or planning — never authority-gated (no side effect).
    Think,
    /// Retrieving external data.
    Fetch,
    /// Switching the current session mode.
    SwitchMode,
    /// Anything else.
    Other,
}

/// Execution status of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Not yet started (streaming input, or awaiting approval).
    Pending,
    /// Currently running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
}

/// A file location a tool call touches. `path` is always absolute per the schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallLocation {
    /// Absolute path being accessed or modified.
    pub path: String,
    /// Optional line number within the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Details about a tool call, as carried by [`RequestPermissionRequest::tool_call`]. This is the
/// data [`crate::permission::evaluate`] maps onto a `tm_types::Action` before consulting
/// `Authority::permits`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    /// The tool call this update describes.
    pub tool_call_id: String,
    /// The tool's category, if known.
    #[serde(default)]
    pub kind: Option<ToolKind>,
    /// Execution status, if known.
    #[serde(default)]
    pub status: Option<ToolCallStatus>,
    /// Human-readable title.
    #[serde(default)]
    pub title: Option<String>,
    /// Programmatic tool name, if known.
    #[serde(default)]
    pub name: Option<String>,
    /// File locations this call touches, if any.
    #[serde(default)]
    pub locations: Option<Vec<ToolCallLocation>>,
    /// Raw, tool-specific input (e.g. `{"command": [...]}` for an `execute` call, `{"url": ..}`
    /// for a `fetch` call) — the only place an argv or a URL can be recovered from for those
    /// kinds; see [`crate::permission`] for exactly which shapes are recognized.
    #[serde(default)]
    pub raw_input: Option<Value>,
    /// Every other field (`content`, `rawOutput`, ...), preserved rather than dropped.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The kind of a [`PermissionOption`], hinting at its polarity (allow vs. reject) and whether
/// the choice should be remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow, this time only.
    AllowOnce,
    /// Allow, and remember the choice.
    AllowAlways,
    /// Reject, this time only.
    RejectOnce,
    /// Reject, and remember the choice.
    RejectAlways,
}

impl PermissionOptionKind {
    /// True for the two `Allow*` variants.
    pub fn is_allow(self) -> bool {
        matches!(
            self,
            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
        )
    }
}

/// One option the agent offers the client for resolving a permission request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// Opaque id the client echoes back in [`RequestPermissionOutcome::Selected`].
    pub option_id: String,
    /// Human-readable label.
    pub name: String,
    /// The option's polarity/persistence.
    pub kind: PermissionOptionKind,
}

/// `session/request_permission` request params (agent -> client).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    /// Which session this request belongs to.
    pub session_id: String,
    /// The tool call needing authorization.
    pub tool_call: ToolCallUpdate,
    /// The options the client must choose from (never invent an id outside this list — see
    /// `crate::permission::choose_option`).
    pub options: Vec<PermissionOption>,
}

/// The client's decision on a permission request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RequestPermissionOutcome {
    /// The turn was cancelled before a decision was made (also used, in this crate, when the
    /// agent offered no option matching what `Authority::permits` decided — see
    /// `crate::permission::choose_option`'s doc comment for why that is the fail-closed choice
    /// rather than fabricating an option id).
    Cancelled,
    /// One of the offered options was chosen.
    Selected {
        /// Must be one of the `option_id`s from [`RequestPermissionRequest::options`].
        #[serde(rename = "optionId")]
        option_id: String,
    },
}

/// `session/request_permission` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionResponse {
    /// The decision.
    pub outcome: RequestPermissionOutcome,
}

/// A streamed content chunk, shared shape for the `*_message_chunk` [`SessionUpdate`] variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentChunk {
    /// The streamed content.
    pub content: ContentBlock,
    /// Groups chunks belonging to the same logical message, if the agent sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// One `session/update` payload. Only the three `*_message_chunk` variants (user/agent/thought)
/// are modeled with real data; every other real variant (`tool_call`, `tool_call_update`,
/// `plan`, ...) is classified as [`SessionUpdate::Unknown`] rather than causing a deserialize
/// failure or silently discarding a shape this crate doesn't act on yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// A chunk of the user's own message being echoed back.
    UserMessageChunk(ContentChunk),
    /// A chunk of the agent's reply.
    AgentMessageChunk(ContentChunk),
    /// A chunk of the agent's internal reasoning.
    AgentThoughtChunk(ContentChunk),
    /// Any other `sessionUpdate` variant this crate does not act on.
    #[serde(other)]
    Unknown,
}

/// `session/update` notification params (agent -> client, no response).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    /// Which session this update pertains to.
    pub session_id: String,
    /// The update itself.
    pub update: SessionUpdate,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_request_uses_camel_case_field_names() {
        let req = InitializeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities::default(),
            client_info: Some(Implementation {
                name: "tm-acp".into(),
                title: None,
                version: "0.1.0".into(),
            }),
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["protocolVersion"], 1);
        assert_eq!(value["clientInfo"]["name"], "tm-acp");
    }

    #[test]
    fn initialize_response_deserializes_a_real_shaped_payload() {
        let payload = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false}
            },
            "authMethods": [],
            "agentInfo": {"name": "example-agent", "version": "1.2.3"}
        });
        let resp: InitializeResponse = serde_json::from_value(payload).unwrap();
        assert_eq!(resp.protocol_version, 1);
        assert!(!resp.agent_capabilities.load_session);
        assert_eq!(resp.agent_info.unwrap().name, "example-agent");
    }

    #[test]
    fn content_block_text_round_trips() {
        let block = ContentBlock::text("hello");
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(value, json!({"type": "text", "text": "hello"}));
        let back: ContentBlock = serde_json::from_value(value).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn content_block_preserves_unknown_variant_fields() {
        let payload = json!({"type": "resource_link", "uri": "file:///a.txt", "name": "a.txt"});
        let block: ContentBlock = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(block.kind, "resource_link");
        assert_eq!(block.text, None);
        // Round-trips the fields this type doesn't otherwise model.
        let back = serde_json::to_value(&block).unwrap();
        assert_eq!(back["uri"], "file:///a.txt");
        assert_eq!(back["name"], "a.txt");
    }

    #[test]
    fn permission_option_kind_matches_real_wire_values() {
        assert_eq!(
            serde_json::to_value(PermissionOptionKind::AllowOnce).unwrap(),
            json!("allow_once")
        );
        assert_eq!(
            serde_json::to_value(PermissionOptionKind::RejectAlways).unwrap(),
            json!("reject_always")
        );
        assert!(PermissionOptionKind::AllowAlways.is_allow());
        assert!(!PermissionOptionKind::RejectOnce.is_allow());
    }

    #[test]
    fn request_permission_outcome_tags_by_outcome_field() {
        let cancelled = RequestPermissionOutcome::Cancelled;
        assert_eq!(
            serde_json::to_value(&cancelled).unwrap(),
            json!({"outcome": "cancelled"})
        );
        let selected = RequestPermissionOutcome::Selected {
            option_id: "opt-1".into(),
        };
        assert_eq!(
            serde_json::to_value(&selected).unwrap(),
            json!({"outcome": "selected", "optionId": "opt-1"})
        );
    }

    #[test]
    fn tool_call_update_deserializes_an_execute_call_with_raw_input() {
        let payload = json!({
            "toolCallId": "call-1",
            "kind": "execute",
            "title": "run tests",
            "rawInput": {"command": ["cargo", "test"]}
        });
        let update: ToolCallUpdate = serde_json::from_value(payload).unwrap();
        assert_eq!(update.tool_call_id, "call-1");
        assert_eq!(update.kind, Some(ToolKind::Execute));
        assert_eq!(
            update.raw_input.unwrap()["command"],
            json!(["cargo", "test"])
        );
    }

    #[test]
    fn session_update_agent_message_chunk_round_trips() {
        let note = SessionNotification {
            session_id: "s-1".into(),
            update: SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::text("hi"),
                message_id: None,
            }),
        };
        let value = serde_json::to_value(&note).unwrap();
        assert_eq!(value["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(value["update"]["content"]["text"], "hi");
        let back: SessionNotification = serde_json::from_value(value).unwrap();
        assert_eq!(back, note);
    }

    #[test]
    fn session_update_classifies_unmodeled_variants_as_unknown_instead_of_failing() {
        let payload = json!({
            "sessionId": "s-1",
            "update": {"sessionUpdate": "plan", "entries": []}
        });
        let note: SessionNotification = serde_json::from_value(payload).unwrap();
        assert_eq!(note.update, SessionUpdate::Unknown);
    }

    #[test]
    fn stop_reason_matches_real_wire_values() {
        assert_eq!(
            serde_json::to_value(StopReason::EndTurn).unwrap(),
            json!("end_turn")
        );
        assert_eq!(
            serde_json::to_value(StopReason::MaxTurnRequests).unwrap(),
            json!("max_turn_requests")
        );
    }
}
