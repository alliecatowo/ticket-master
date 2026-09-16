//! Wire-independent request/response types.
//!
//! Nothing here knows about any particular provider's JSON shape. [`crate::anthropic`] maps
//! these to and from the Messages API; [`crate::mock`] produces them directly. Keeping this
//! boundary means [`crate::route`] and [`crate::fabric`] never depend on a specific wire format.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use tm_types::Timestamp;

/// Who authored a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// System / developer instructions.
    System,
    /// The human or calling agent's turn.
    User,
    /// The model's turn.
    Assistant,
}

/// One turn of a conversation, made up of one or more content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who authored this turn.
    pub role: MessageRole,
    /// The turn's content, in order.
    pub content: Vec<ContentBlock>,
}

/// A unit of message content. Externally tagged (see `SPEC.md` note on recursive enums).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// The text itself.
        text: String,
    },
    /// A model-issued request to invoke a tool.
    ToolUse {
        /// Provider-assigned id correlating this call to its [`ContentBlock::ToolResult`].
        id: String,
        /// The tool's name, matching a [`ToolDef::name`] from the request.
        name: String,
        /// The tool call's arguments.
        input: serde_json::Value,
    },
    /// The caller's answer to a prior [`ContentBlock::ToolUse`].
    ToolResult {
        /// The [`ContentBlock::ToolUse::id`] this result answers.
        tool_use_id: String,
        /// The tool's output, as content blocks (usually a single `Text`).
        content: Vec<ContentBlock>,
        /// Whether the tool invocation itself failed.
        is_error: bool,
    },
}

/// A tool a model may call during a [`CompletionRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// The tool's name, referenced by [`ContentBlock::ToolUse::name`].
    pub name: String,
    /// A description shown to the model to decide when to call this tool.
    pub description: String,
    /// The JSON Schema describing the tool's input.
    pub input_schema: serde_json::Value,
}

/// A request for one or more completions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Optional system prompt, kept separate from `messages` per the Anthropic wire shape.
    pub system: Option<String>,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolDef>,
    /// Upper bound on generated tokens.
    pub max_tokens: u32,
    /// Sampling temperature, `0.0` for deterministic-as-possible output.
    pub temperature: Option<f32>,
    /// Sequences that stop generation when produced.
    pub stop_sequences: Vec<String>,
    /// Whether to stream the response incrementally.
    pub stream: bool,
    /// How many independent completions to request.
    pub n: u32,
}

/// Why a completion stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model produced a natural end-of-turn.
    EndTurn,
    /// `max_tokens` was reached.
    MaxTokens,
    /// A stop sequence was produced.
    StopSequence,
    /// The model chose to call a tool and is waiting on [`ContentBlock::ToolResult`].
    ToolUse,
}

/// Token accounting for one completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed at full price.
    pub input_tokens: u32,
    /// Output tokens generated.
    pub output_tokens: u32,
    /// Input tokens served from the prompt cache, billed at a discount.
    pub cache_read_tokens: u32,
    /// Input tokens newly written to the prompt cache.
    pub cache_write_tokens: u32,
}

/// One candidate completion (`n > 1` requests produce several).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// The generated content.
    pub content: Vec<ContentBlock>,
    /// Why this candidate stopped.
    pub stop_reason: StopReason,
}

/// The full response to a [`CompletionRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    /// The model that actually served this request.
    pub model: ModelId,
    /// One entry per requested candidate.
    pub candidates: Vec<Candidate>,
    /// Token accounting, summed across candidates.
    pub usage: Usage,
    /// Wall-clock time the provider took to respond.
    pub latency: Duration,
    /// When the response was received, per the injected clock.
    pub received_at: Timestamp,
}

/// A request to embed one or more pieces of text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedRequest {
    /// The texts to embed, in order.
    pub inputs: Vec<String>,
}

/// The response to an [`EmbedRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Embeddings {
    /// The model that produced these embeddings.
    pub model: ModelId,
    /// One vector per input, in the same order as [`EmbedRequest::inputs`].
    pub vectors: Vec<Vec<f32>>,
    /// Token accounting for the embedding call.
    pub usage: Usage,
}

/// A `(provider, model)` pair identifying a concrete backend, as opposed to a [`tm_types::Role`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelId {
    /// The provider slug, e.g. `"anthropic"`.
    pub provider: String,
    /// The provider's model identifier, e.g. `"claude-sonnet-5"`.
    pub model: String,
}

impl ModelId {
    /// Build a `ModelId` from its parts.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        ModelId {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// Everything that can go wrong calling a [`crate::fabric::Provider`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    /// The provider rejected the request as malformed (HTTP 4xx other than 429).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Authentication failed, e.g. a missing or rejected API key.
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// The provider's rate limit was hit (HTTP 429).
    #[error("rate limited: {message}")]
    RateLimited {
        /// Human-readable detail from the provider.
        message: String,
        /// The `Retry-After` the provider reported, if any.
        retry_after: Option<Duration>,
    },

    /// The provider had a transient server-side failure (HTTP 5xx, connection reset, ...).
    #[error("provider unavailable: {0}")]
    Unavailable(String),

    /// The request or response exceeded a size or token limit.
    #[error("too large: {0}")]
    TooLarge(String),

    /// The call exceeded its deadline.
    #[error("timed out: {0}")]
    Timeout(String),

    /// A response could not be parsed into the expected shape.
    #[error("malformed response: {0}")]
    MalformedResponse(String),

    /// A [`crate::mock::MockProvider`] had no scripted response for the request.
    #[error("no scripted response: {0}")]
    Unscripted(String),
}

impl ProviderError {
    /// Whether retrying the same request later has any chance of succeeding.
    ///
    /// `RateLimited`, `Unavailable` and `Timeout` are transient and retryable;
    /// `InvalidRequest`, `AuthFailed`, `TooLarge`, `MalformedResponse` and `Unscripted` are not,
    /// since retrying without changing the request would fail identically.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProviderError::RateLimited { .. }
                | ProviderError::Unavailable(_)
                | ProviderError::Timeout(_)
        )
    }

    /// The duration to wait before retrying, if the provider specified one.
    ///
    /// Only `RateLimited` carries a provider-supplied `Retry-After`; every other variant
    /// returns `None` and callers fall back to their own backoff policy.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ProviderError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_retryable_rate_limited() {
        let err = ProviderError::RateLimited {
            message: "Too many requests".to_string(),
            retry_after: None,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_unavailable() {
        let err = ProviderError::Unavailable("Server error".to_string());
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_timeout() {
        let err = ProviderError::Timeout("Request timed out".to_string());
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_invalid_request() {
        let err = ProviderError::InvalidRequest("Invalid input".to_string());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_auth_failed() {
        let err = ProviderError::AuthFailed("Invalid API key".to_string());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_too_large() {
        let err = ProviderError::TooLarge("Request body too large".to_string());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_malformed_response() {
        let err = ProviderError::MalformedResponse("Invalid JSON".to_string());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_unscripted() {
        let err = ProviderError::Unscripted("No mock response".to_string());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_retry_after_with_duration() {
        let duration = Duration::from_secs(60);
        let err = ProviderError::RateLimited {
            message: "Rate limited".to_string(),
            retry_after: Some(duration),
        };
        assert_eq!(err.retry_after(), Some(duration));
    }

    #[test]
    fn test_retry_after_without_duration() {
        let err = ProviderError::RateLimited {
            message: "Rate limited".to_string(),
            retry_after: None,
        };
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_retry_after_none_for_unavailable() {
        let err = ProviderError::Unavailable("Server error".to_string());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_retry_after_none_for_timeout() {
        let err = ProviderError::Timeout("Timed out".to_string());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_retry_after_none_for_invalid_request() {
        let err = ProviderError::InvalidRequest("Bad input".to_string());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_retry_after_none_for_auth_failed() {
        let err = ProviderError::AuthFailed("Auth error".to_string());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_retry_after_none_for_too_large() {
        let err = ProviderError::TooLarge("Too large".to_string());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_retry_after_none_for_malformed_response() {
        let err = ProviderError::MalformedResponse("Bad JSON".to_string());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_retry_after_none_for_unscripted() {
        let err = ProviderError::Unscripted("No script".to_string());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_message_role_serialize_deserialize() {
        let roles = vec![
            MessageRole::System,
            MessageRole::User,
            MessageRole::Assistant,
        ];
        for role in roles {
            let json = serde_json::to_string(&role).expect("serialize");
            let deserialized: MessageRole = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(role, deserialized);
        }
    }

    #[test]
    fn test_message_with_text_content() {
        let msg = Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        };
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content.len(), 1);
    }

    #[test]
    fn test_message_with_multiple_blocks() {
        let msg = Message {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Response".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "search".to_string(),
                    input: serde_json::json!({"query": "test"}),
                },
            ],
        };
        assert_eq!(msg.content.len(), 2);
    }

    #[test]
    fn test_tool_result_with_error() {
        let tool_result = ContentBlock::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text {
                text: "Error occurred".to_string(),
            }],
            is_error: true,
        };
        match tool_result {
            ContentBlock::ToolResult { is_error, .. } => assert!(is_error),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_tool_result_success() {
        let tool_result = ContentBlock::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text {
                text: "Result".to_string(),
            }],
            is_error: false,
        };
        match tool_result {
            ContentBlock::ToolResult { is_error, .. } => assert!(!is_error),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_tool_def() {
        let tool = ToolDef {
            name: "search".to_string(),
            description: "Search the web".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        };
        assert_eq!(tool.name, "search");
        assert!(tool.input_schema.is_object());
    }

    #[test]
    fn test_completion_request_defaults() {
        let req = CompletionRequest {
            system: Some("You are helpful".to_string()),
            messages: vec![],
            tools: vec![],
            max_tokens: 1024,
            temperature: Some(0.5),
            stop_sequences: vec![],
            stream: false,
            n: 1,
        };
        assert_eq!(req.max_tokens, 1024);
        assert_eq!(req.n, 1);
        assert!(!req.stream);
    }

    #[test]
    fn test_stop_reason_serialize() {
        let reasons = vec![
            StopReason::EndTurn,
            StopReason::MaxTokens,
            StopReason::StopSequence,
            StopReason::ToolUse,
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).expect("serialize");
            let deserialized: StopReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(reason, deserialized);
        }
    }

    #[test]
    fn test_usage() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
        };
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn test_usage_default() {
        let usage = Usage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    #[test]
    fn test_candidate() {
        let candidate = Candidate {
            content: vec![ContentBlock::Text {
                text: "Test".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
        };
        assert_eq!(candidate.content.len(), 1);
        assert_eq!(candidate.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn test_completion() {
        let completion = Completion {
            model: ModelId::new("anthropic", "claude-3"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "Response".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: Duration::from_millis(500),
            received_at: Timestamp::EPOCH,
        };
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(completion.latency.as_millis(), 500);
    }

    #[test]
    fn test_embed_request() {
        let req = EmbedRequest {
            inputs: vec!["text1".to_string(), "text2".to_string()],
        };
        assert_eq!(req.inputs.len(), 2);
    }

    #[test]
    fn test_embeddings() {
        let embeddings = Embeddings {
            model: ModelId::new("anthropic", "embedding-model"),
            vectors: vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        };
        assert_eq!(embeddings.vectors.len(), 2);
        assert_eq!(embeddings.vectors[0].len(), 3);
    }

    #[test]
    fn test_model_id_display() {
        let model_id = ModelId::new("anthropic", "claude-sonnet");
        assert_eq!(model_id.to_string(), "anthropic/claude-sonnet");
    }

    #[test]
    fn test_model_id_ordering() {
        let model1 = ModelId::new("anthropic", "claude-haiku");
        let model2 = ModelId::new("anthropic", "claude-sonnet");
        let model3 = ModelId::new("openai", "gpt-4");
        assert!(model1 < model2);
        assert!(model2 < model3);
    }

    #[test]
    fn test_model_id_equality() {
        let model1 = ModelId::new("anthropic", "claude-3");
        let model2 = ModelId::new("anthropic", "claude-3");
        assert_eq!(model1, model2);
    }

    #[test]
    fn test_content_block_text_serialize() {
        let block = ContentBlock::Text {
            text: "Hello".to_string(),
        };
        let json = serde_json::to_string(&block).expect("serialize");
        let deserialized: ContentBlock = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(block, deserialized);
    }

    #[test]
    fn test_content_block_tool_use_serialize() {
        let block = ContentBlock::ToolUse {
            id: "123".to_string(),
            name: "search".to_string(),
            input: serde_json::json!({"q": "test"}),
        };
        let json = serde_json::to_string(&block).expect("serialize");
        let deserialized: ContentBlock = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(block, deserialized);
    }

    #[test]
    fn test_content_block_tool_result_serialize() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "123".to_string(),
            content: vec![ContentBlock::Text {
                text: "result".to_string(),
            }],
            is_error: false,
        };
        let json = serde_json::to_string(&block).expect("serialize");
        let deserialized: ContentBlock = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(block, deserialized);
    }
}
