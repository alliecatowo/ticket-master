+++
[doc]
id = "wiki/architecture/tm-auth"
mode = "generated"
derived_from = ["crates/tm-auth/src/**"]
+++

# Architecture: tm-auth

## Module tree

- `crates/tm-auth/src/credential.rs`
- `crates/tm-auth/src/device_code.rs`
- `crates/tm-auth/src/entitlement.rs`
- `crates/tm-auth/src/env_api_key.rs`
- `crates/tm-auth/src/error.rs`
- `crates/tm-auth/src/keychain.rs`
- `crates/tm-auth/src/lib.rs`
- `crates/tm-auth/src/redact.rs`

## Public symbols

### `crates/tm-auth/src/credential.rs`

- `pub struct Credential`
- `impl Credential`
  - `pub fn api_key(secret: impl Into<String>) -> Self`
  - `pub fn oauth(access_token: impl Into<String>, refresh_token: Option<String>) -> Self`
  - `pub fn expose_secret(&self) -> &str`
  - `pub fn refresh_token(&self) -> Option<&str>`

### `crates/tm-auth/src/device_code.rs`

- `pub struct DeviceCodeConfig`
- `pub struct DeviceAuthorizationResponse`
- `pub enum PollOutcome`
- `pub fn interpret_token_response(status: u16, body: &[u8]) -> PollOutcome`
- `pub struct OAuthTokens`
- `impl OAuthTokens`
  - `pub fn new(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
        issued_at: Timestamp,
    ) -> Self`
  - `pub fn access_token(&self) -> &str`
  - `pub fn refresh_token(&self) -> Option<&str>`
  - `pub fn is_expired(&self, now: Timestamp, skew_seconds: i64) -> bool`
- `pub trait TokenStore: Send + Sync`
- `pub struct InMemoryTokenStore`
- `impl InMemoryTokenStore`
  - `pub fn new() -> Self`
- `pub struct DeviceCodeOAuth`
- `impl DeviceCodeOAuth`
  - `pub fn new(
        id: impl Into<String>,
        config: DeviceCodeConfig,
        clock: Arc<dyn Clock>,
        store: Arc<dyn TokenStore>,
    ) -> Result<Self, AuthError>`
  - `pub async fn request_device_code(&self) -> Result<DeviceAuthorizationResponse, AuthError>`
  - `pub async fn poll_until_authorized(
        &self,
        device_auth: &DeviceAuthorizationResponse,
    ) -> Result<Credential, AuthError>`
  - `pub async fn authorize(
        &self,
        on_prompt: impl FnOnce(&DeviceAuthorizationResponse),
    ) -> Result<Credential, AuthError>`
  - `pub async fn refresh(&self, refresh_token: &str) -> Result<Credential, AuthError>`

### `crates/tm-auth/src/entitlement.rs`

- `pub enum QuotaClass`
- `pub struct Entitlement`

### `crates/tm-auth/src/env_api_key.rs`

- `pub struct EnvApiKey`
- `impl EnvApiKey`
  - `pub fn new(var_name: impl Into<String>) -> Self`
  - `pub fn with_id(id: impl Into<String>, var_name: impl Into<String>) -> Self`
  - `pub fn var_name(&self) -> &str`
  - `pub fn resolve(&self) -> Result<Credential, AuthError>`

### `crates/tm-auth/src/error.rs`

- `pub enum AuthError`

### `crates/tm-auth/src/keychain.rs`

- `pub struct KeychainApiKey`
- `impl KeychainApiKey`
  - `pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self`
  - `pub fn service(&self) -> &str`
  - `pub fn account(&self) -> &str`

### `crates/tm-auth/src/lib.rs`

- `pub mod redact;`
- `pub enum CredentialKind`
- `pub trait AuthAdapter: Send + Sync`

### `crates/tm-auth/src/redact.rs`

- `pub fn redact(text: &str) -> String`
- `pub fn redact_json(value: &Value) -> Value`
- `pub struct SessionRedactor`
- `impl SessionRedactor`
  - `pub fn new() -> Self`
  - `pub fn redact(&self, text: &str) -> String`
  - `pub fn redact_json(&self, value: &Value) -> Value`
  - `pub fn restore(&self, text: &str) -> String`
  - `pub fn mapping_len(&self) -> usize`
