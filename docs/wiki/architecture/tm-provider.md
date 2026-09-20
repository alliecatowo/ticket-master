+++
[doc]
id = "wiki/architecture/tm-provider"
mode = "generated"
derived_from = ["crates/tm-provider/src/**"]
+++

# Architecture: tm-provider

## Module tree

- `crates/tm-provider/src/anthropic.rs`
- `crates/tm-provider/src/fabric.rs`
- `crates/tm-provider/src/lib.rs`
- `crates/tm-provider/src/mock.rs`
- `crates/tm-provider/src/providers/cloud.rs`
- `crates/tm-provider/src/providers/cloudflare.rs`
- `crates/tm-provider/src/providers/compat.rs`
- `crates/tm-provider/src/providers/fast.rs`
- `crates/tm-provider/src/providers/frontier.rs`
- `crates/tm-provider/src/providers/gemini.rs`
- `crates/tm-provider/src/providers/local.rs`
- `crates/tm-provider/src/providers/mod.rs`
- `crates/tm-provider/src/providers/openai.rs`
- `crates/tm-provider/src/providers/openrouter.rs`
- `crates/tm-provider/src/providers/registry.rs`
- `crates/tm-provider/src/providers/serverless.rs`
- `crates/tm-provider/src/role_config.rs`
- `crates/tm-provider/src/route.rs`
- `crates/tm-provider/src/state.rs`
- `crates/tm-provider/src/types.rs`

## Public symbols

### `crates/tm-provider/src/anthropic.rs`

- `pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";`
- `pub const ANTHROPIC_VERSION: &str = "2023-06-01";`
- `pub const API_KEY_ENV_VAR: &str = "ANTHROPIC_API_KEY";`
- `pub struct AnthropicProvider`
- `impl AnthropicProvider`
  - `pub fn from_env(
        model: ModelId,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self, ProviderError>`
  - `pub fn with_config(
        model: ModelId,
        api_key: String,
        base_url: String,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self, ProviderError>`
  - `pub fn set_max_retries(&mut self, max_retries: u32)`
- `pub struct WireRequest`
- `pub struct WireMessage`
- `pub enum WireContentBlock`
- `pub struct WireTool`
- `pub struct WireResponse`
- `pub struct WireUsage`
- `pub struct WireErrorEnvelope`
- `pub struct WireErrorDetail`
- `pub fn build_wire_request(model: &ModelId, req: &CompletionRequest) -> WireRequest`
- `pub fn build_headers(api_key: &str) -> reqwest::header::HeaderMap`
- `pub fn parse_wire_response(
    body: &[u8],
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError>`
- `pub fn classify_status(
    status: reqwest::StatusCode,
    retry_after_header: Option<&str>,
    body: &[u8],
) -> Result<(), ProviderError>`

### `crates/tm-provider/src/fabric.rs`

- `pub trait Provider: Send + Sync`
- `pub enum FabricRecord`
- `pub struct ProviderRecord`
- `pub struct Fabric`
- `impl Fabric`
  - `pub fn new(table: RoleTable, clock: Arc<dyn Clock>) -> Self`
  - `pub fn register_provider(&self, provider: Arc<dyn Provider>)`
  - `pub fn route(&self, role: Role, need: &Need, now: Timestamp) -> RouteDecision`
  - `pub async fn execute(&self, role: Role, req: CompletionRequest) -> TmResult<Completion>`
  - `pub fn last_records(&self) -> Vec<FabricRecord>`
  - `pub fn state_snapshot(&self) -> FabricState`

### `crates/tm-provider/src/lib.rs`

- `pub mod anthropic;`
- `pub mod fabric;`
- `pub mod mock;`
- `pub mod providers;`
- `pub mod role_config;`
- `pub mod route;`
- `pub mod state;`
- `pub mod types;`

### `crates/tm-provider/src/mock.rs`

- `pub type RequestHash = u64;`
- `pub fn hash_request(req: &CompletionRequest) -> RequestHash`
- `pub struct ScriptedFailure`
- `pub enum Script`
- `pub struct MockProvider`
- `impl MockProvider`
  - `pub fn new(id: impl Into<String>, model: ModelId, clock: std::sync::Arc<dyn Clock>) -> Self`
  - `pub fn script_response(&self, req: &CompletionRequest, completion: Completion)`
  - `pub fn script_failure(&self, req: &CompletionRequest, failure: ScriptedFailure)`
  - `pub fn script_exhausted(&self, req: &CompletionRequest, retry_after: Duration)`
  - `pub fn script_default_response(&self, completion: Completion)`
  - `pub fn set_default_latency(&mut self, latency: Duration)`
  - `pub fn call_log(&self) -> Vec<CompletionRequest>`
  - `pub fn deterministic_completion(&self, req: &CompletionRequest) -> Completion`

### `crates/tm-provider/src/providers/cloud.rs`

- `pub struct AzureOpenAiProvider`
- `impl AzureOpenAiProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct BedrockProvider`
- `impl BedrockProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct VertexProvider`
- `impl VertexProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`

### `crates/tm-provider/src/providers/cloudflare.rs`

- `pub struct CloudflareWorkersAiProvider`
- `impl CloudflareWorkersAiProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`

### `crates/tm-provider/src/providers/compat.rs`

- `pub enum AuthStyle`
- `pub struct CompatConfig`
- `impl CompatConfig`
  - `pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self`
  - `pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self`
  - `pub fn with_auth_style(mut self, style: AuthStyle) -> Self`
  - `pub fn with_extra_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self`
  - `pub fn with_chat_path(mut self, path: impl Into<String>) -> Self`
  - `pub fn with_embeddings_path(mut self, path: impl Into<String>) -> Self`
  - `pub fn without_embeddings(mut self) -> Self`
  - `pub fn with_max_retries(mut self, max_retries: u32) -> Self`
  - `pub fn with_timeout(mut self, timeout: Duration) -> Self`
- `pub struct CompatProvider`
- `impl CompatProvider`
  - `pub fn new(config: CompatConfig, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn from_env_prefix(
        prefix: &str,
        id: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProviderError>`
- `pub struct DevPassProvider`
- `impl DevPassProvider`
  - `pub fn from_env(clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct WireRequest`
- `pub struct WireStreamOptions`
- `pub struct WireMessage`
- `pub struct WireTool`
- `pub struct WireFunctionDef`
- `pub struct WireToolCall`
- `pub struct WireFunctionCall`
- `pub struct WireResponse`
- `pub struct WireChoice`
- `pub struct WireResponseMessage`
- `pub struct WireUsage`
- `pub struct WirePromptTokensDetails`
- `pub struct WireErrorEnvelope`
- `pub struct WireErrorDetail`
- `pub struct WireStreamChunk`
- `pub struct WireStreamChoice`
- `pub struct WireStreamDelta`
- `pub struct WireToolCallDelta`
- `pub struct WireFunctionCallDelta`
- `pub struct WireEmbedRequest`
- `pub struct WireEmbedResponse`
- `pub struct WireEmbedDatum`
- `pub fn build_wire_messages(messages: &[Message]) -> Vec<WireMessage>`
- `pub fn build_wire_request(model: &str, req: &CompletionRequest) -> WireRequest`
- `pub fn map_finish_reason(reason: &str) -> Result<StopReason, ProviderError>`
- `pub fn parse_wire_response(
    body: &[u8],
    requested_model: &str,
    provider_id: &str,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError>`
- `pub fn parse_wire_embed_response(
    body: &[u8],
    requested_model: &str,
    provider_id: &str,
) -> Result<Embeddings, ProviderError>`
- `pub fn parse_sse_body(body: &[u8]) -> Result<Vec<WireStreamChunk>, ProviderError>`
- `pub fn assemble_streamed_completion(
    chunks: &[WireStreamChunk],
    requested_model: &str,
    provider_id: &str,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError>`
- `pub fn build_headers(config: &CompatConfig) -> Result<reqwest::header::HeaderMap, ProviderError>`
- `pub fn classify_status(
    status: reqwest::StatusCode,
    retry_after_header: Option<&str>,
    body: &[u8],
) -> Result<(), ProviderError>`

### `crates/tm-provider/src/providers/fast.rs`

- `pub struct RateLimitSnapshot`
- `pub fn parse_rate_limit_headers<'a, I>(headers: I) -> RateLimitSnapshot
where
    I: IntoIterator<Item = (&'a str, &'a str)>,`
- `pub fn suggested_backoff(snapshot: &RateLimitSnapshot) -> Option<Duration>`
- `pub struct GroqProvider`
- `impl GroqProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct CerebrasProvider`
- `impl CerebrasProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`

### `crates/tm-provider/src/providers/frontier.rs`

- `pub struct DeepSeekProvider`
- `impl DeepSeekProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct MistralProvider`
- `impl MistralProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct XaiProvider`
- `impl XaiProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`

### `crates/tm-provider/src/providers/gemini.rs`

- `pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";`
- `pub struct GeminiProvider`
- `impl GeminiProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn with_config(
        model: ModelId,
        api_key: String,
        base_url: String,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProviderError>`
  - `pub fn set_max_retries(&mut self, max_retries: u32)`
  - `pub fn info() -> ProviderInfo`
- `pub struct WireRequest`
- `pub struct WireGenerationConfig`
- `pub struct WireContent`
- `pub struct WirePart`
- `pub struct WireFunctionCall`
- `pub struct WireFunctionResponse`
- `pub struct WireTool`
- `pub struct WireFunctionDeclaration`
- `pub struct WireResponse`
- `pub struct WireCandidate`
- `pub struct WireUsageMetadata`
- `pub struct WireErrorEnvelope`
- `pub struct WireErrorDetail`
- `pub struct WireEmbedBatchRequest`
- `pub struct WireEmbedRequest`
- `pub struct WireEmbedBatchResponse`
- `pub struct WireEmbedding`
- `pub fn build_wire_request(model: &ModelId, req: &CompletionRequest) -> WireRequest`
- `pub fn parse_wire_response(
    body: &[u8],
    model: &ModelId,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError>`
- `pub fn parse_sse_body(body: &[u8]) -> Result<Vec<WireResponse>, ProviderError>`
- `pub fn assemble_streamed_completion(
    chunks: &[WireResponse],
    model: &ModelId,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError>`
- `pub fn classify_status(
    status: reqwest::StatusCode,
    retry_after_header: Option<&str>,
    body: &[u8],
) -> Result<(), ProviderError>`

### `crates/tm-provider/src/providers/local.rs`

- `pub struct OllamaProvider`
- `impl OllamaProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
  - `pub async fn probe(&self) -> bool`
  - `pub async fn list_models(&self) -> Result<Vec<String>, ProviderError>`
- `pub struct LmStudioProvider`
- `impl LmStudioProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
  - `pub async fn probe(&self) -> bool`
  - `pub async fn list_models(&self) -> Result<Vec<String>, ProviderError>`
- `pub struct LlamaCppProvider`
- `impl LlamaCppProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
  - `pub async fn probe(&self) -> bool`
  - `pub async fn list_models(&self) -> Result<Vec<String>, ProviderError>`

### `crates/tm-provider/src/providers/mod.rs`

- `pub mod cloud;`
- `pub mod cloudflare;`
- `pub mod compat;`
- `pub mod fast;`
- `pub mod frontier;`
- `pub mod gemini;`
- `pub mod local;`
- `pub mod openai;`
- `pub mod openrouter;`
- `pub mod registry;`
- `pub mod serverless;`
- `pub struct Capabilities`
- `pub struct EnvVarRequirement`
- `pub struct ProviderInfo`
- `impl ProviderInfo`
  - `pub fn is_configured(&self) -> bool`
- `pub const LOCAL_PROVIDER_IDS: [&str; 3] = ["ollama", "lm-studio", "llama-cpp"];`
- `pub enum Availability`
- `impl Availability`
  - `pub fn derive(is_configured: bool, probed: Option<bool>) -> Availability`
- `pub enum LocalProbe`
- `impl LocalProbe`
  - `pub fn reachable(&self) -> bool`
- `pub(crate) fn missing_env_var(name: &str) -> ProviderError`

### `crates/tm-provider/src/providers/openai.rs`

- `pub struct OpenAiProvider`
- `impl OpenAiProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`

### `crates/tm-provider/src/providers/openrouter.rs`

- `pub struct OpenRouterProvider`
- `impl OpenRouterProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct GithubModelsProvider`
- `impl GithubModelsProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`

### `crates/tm-provider/src/providers/registry.rs`

- `pub struct Registry;`
- `impl Registry`
  - `pub fn known_providers() -> Vec<ProviderInfo>`
  - `pub fn autodetect() -> Vec<ProviderInfo>`
  - `pub async fn local_models(
        id: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Vec<String>, ProviderError>`
  - `pub async fn probe_local(id: &str, clock: Arc<dyn Clock>) -> LocalProbe`
  - `pub async fn availability(info: &ProviderInfo, clock: Arc<dyn Clock>) -> Availability`
  - `pub fn build_provider(
        candidate: &RoleCandidate,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<dyn Provider>, ProviderError>`
  - `pub fn build_fabric(table: RoleTable, clock: Arc<dyn Clock>) -> Result<Fabric, ProviderError>`

### `crates/tm-provider/src/providers/serverless.rs`

- `pub struct TogetherProvider`
- `impl TogetherProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct FireworksProvider`
- `impl FireworksProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`
- `pub struct HuggingFaceProvider`
- `impl HuggingFaceProvider`
  - `pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError>`
  - `pub fn info() -> ProviderInfo`

### `crates/tm-provider/src/role_config.rs`

- `pub struct Limits`
- `impl Limits`
  - `pub fn unlimited() -> Self`
- `pub struct Price`
- `pub struct RoleCandidate`
- `pub struct RoleTable`
- `pub enum RoleConfigError`
- `impl RoleTable`
  - `pub fn parse(toml_str: &str) -> Result<Self, RoleConfigError>`
  - `pub fn to_toml_string(&self) -> Result<String, RoleConfigError>`
  - `pub fn validate(&self) -> Result<(), RoleConfigError>`
  - `pub fn candidates_for(&self, role: Role) -> &[RoleCandidate]`
  - `pub fn default_table() -> Self`

### `crates/tm-provider/src/route.rs`

- `pub struct Need`
- `pub enum RouteDecision`
- `pub fn route(
    table: &RoleTable,
    state: &FabricState,
    role: Role,
    need: &Need,
    now: Timestamp,
) -> RouteDecision`

### `crates/tm-provider/src/state.rs`

- `pub type CandidateKey = ModelId;`
- `pub struct Window`
- `pub enum BreakerState`
- `pub struct Breaker`
- `impl Breaker`
  - `pub fn new(
        failure_threshold: u32,
        window: std::time::Duration,
        cooldown: std::time::Duration,
        now: Timestamp,
    ) -> Self`
- `pub struct CandidateState`
- `pub struct LedgerEntry`
- `pub enum FabricEvent`
- `pub struct FabricState`
- `impl FabricState`
  - `pub fn new() -> Self`
  - `pub fn register(
        &mut self,
        key: CandidateKey,
        now: Timestamp,
        failure_threshold: u32,
        breaker_window: std::time::Duration,
        breaker_cooldown: std::time::Duration,
        ewma_alpha: f64,
    )`
  - `pub fn candidate(&self, key: &CandidateKey) -> Option<&CandidateState>`
  - `pub fn candidates(&self) -> impl Iterator<Item = (&CandidateKey, &CandidateState)>`
  - `pub fn ledger(&self) -> &[LedgerEntry]`
  - `pub fn apply(&mut self, event: FabricEvent, now: Timestamp)`
  - `pub fn roll_windows(&mut self, now: Timestamp)`

### `crates/tm-provider/src/types.rs`

- `pub enum MessageRole`
- `pub struct Message`
- `pub enum ContentBlock`
- `pub struct ToolDef`
- `pub struct CompletionRequest`
- `pub enum StopReason`
- `pub struct Usage`
- `pub struct Candidate`
- `pub struct Completion`
- `pub struct EmbedRequest`
- `pub struct Embeddings`
- `pub struct ModelId`
- `impl ModelId`
  - `pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self`
- `pub enum ProviderError`
- `impl ProviderError`
  - `pub fn is_retryable(&self) -> bool`
  - `pub fn retry_after(&self) -> Option<Duration>`
