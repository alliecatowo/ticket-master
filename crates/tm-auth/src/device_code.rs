//! [`DeviceCodeOAuth`]: OAuth 2.0 device authorization grant (RFC 8628) with refresh.
//!
//! Built on `reqwest` (already a workspace dependency for every HTTP-speaking provider in
//! `tm-provider`) rather than a dedicated OAuth crate: the device-code flow is three HTTP calls
//! (request a device code, poll the token endpoint, refresh) plus a small state machine over the
//! poll responses (`authorization_pending` / `slow_down` / `access_denied` / `expired_token`),
//! which is well within what's reasonable to hand-roll against a generic client rather than
//! pulling in a crate shaped around a specific OAuth library's abstractions.
//!
//! The state machine over poll responses ([`interpret_token_response`], returning
//! [`PollOutcome`]) is a pure function of the HTTP status and parsed JSON body, kept separate
//! from the actual `reqwest` calls in [`DeviceCodeOAuth::poll_once`] so it's unit-testable
//! without constructing a network client — `crates/xtask/src/hygiene.rs`'s
//! `check_network_in_tests` forbids exactly that in test code, and the split also means the
//! interesting logic (five distinct server responses, mapped to five distinct outcomes) is
//! tested directly instead of through a mocked server.
//!
//! Storage: [`TokenStore`] is a trait so persistence is pluggable. This module ships only
//! [`InMemoryTokenStore`]. `SPEC.md` §28.2 says interactive login should store "to the OS
//! keychain by default", but [`crate::KeychainApiKey`]'s keychain integration is not implemented
//! yet (see its module docs) — writing a plaintext-file fallback here to compensate would be
//! exactly the "silently falling back to something insecure" this task's instructions warn
//! against, so [`DeviceCodeOAuth`] holds tokens in memory only until a keychain-backed
//! `TokenStore` lands.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use tm_types::{Clock, Timestamp};

use crate::{AuthAdapter, AuthError, Credential, CredentialKind};

/// Static configuration for one provider's device-code flow: where to ask for a device code,
/// where to poll/refresh, which client id, and what scope to request.
#[derive(Debug, Clone)]
pub struct DeviceCodeConfig {
    /// RFC 8628 §3.1 device authorization endpoint.
    pub device_authorization_endpoint: String,
    /// RFC 8628 §3.4 / RFC 6749 §4.1.3 token endpoint (also used for refresh).
    pub token_endpoint: String,
    /// The OAuth client id this flow authenticates as.
    pub client_id: String,
    /// Space-delimited scope to request, if any.
    pub scope: Option<String>,
}

/// RFC 8628 §3.2's device authorization response.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorizationResponse {
    /// The device verification code.
    pub device_code: String,
    /// The end-user verification code, short enough to type by hand.
    pub user_code: String,
    /// The URI the user visits to enter `user_code`.
    pub verification_uri: String,
    /// A URI that already embeds `user_code`, if the server provides one (RFC 8628 calls this
    /// `verification_uri_complete`).
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Seconds until `device_code` expires.
    pub expires_in: u64,
    /// Minimum seconds to wait between poll requests. RFC 8628 defaults this to 5 when absent.
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// The raw token-endpoint response shape (RFC 6749 §5.1/§5.2), used for both the success case
/// and the `{"error": "..."}` pending/denied/expired cases RFC 8628 §3.5 defines.
#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// What one poll of the token endpoint resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// Authorization succeeded; tokens are attached.
    Authorized {
        /// The bearer access token.
        access_token: String,
        /// A refresh token, if the server issued one.
        refresh_token: Option<String>,
        /// Seconds until `access_token` expires, if the server reported one.
        expires_in: Option<u64>,
    },
    /// The user has not completed authorization yet; keep polling at the configured interval.
    Pending,
    /// The server asked for a longer interval between polls (RFC 8628 §3.5's `slow_down`).
    SlowDown,
    /// The device code expired before authorization completed.
    Expired,
    /// The user (or policy) denied the request.
    Denied,
    /// Any other error the server reported.
    Failed {
        /// Safe-to-display detail: the OAuth `error`/`error_description` fields, or an HTTP
        /// status. Never derived from request/credential material.
        detail: String,
    },
}

/// Interpret one token-endpoint HTTP response into a [`PollOutcome`]. Pure — no I/O, no clock —
/// so it's unit-testable without a network client. `body` is the raw response bytes; `status` is
/// the HTTP status code.
pub fn interpret_token_response(status: u16, body: &[u8]) -> PollOutcome {
    let parsed: Result<TokenResponse, _> = serde_json::from_slice(body);
    let Ok(resp) = parsed else {
        return PollOutcome::Failed {
            detail: format!("http {status}: unparseable token response"),
        };
    };

    if let Some(access_token) = resp.access_token {
        return PollOutcome::Authorized {
            access_token,
            refresh_token: resp.refresh_token,
            expires_in: resp.expires_in,
        };
    }

    match resp.error.as_deref() {
        Some("authorization_pending") => PollOutcome::Pending,
        Some("slow_down") => PollOutcome::SlowDown,
        Some("expired_token") => PollOutcome::Expired,
        Some("access_denied") => PollOutcome::Denied,
        Some(other) => PollOutcome::Failed {
            detail: resp
                .error_description
                .unwrap_or_else(|| format!("{other} (http {status})")),
        },
        None => PollOutcome::Failed {
            detail: format!("http {status}: no access_token and no error field"),
        },
    }
}

/// A resolved OAuth token pair plus its expiry, as held by a [`TokenStore`].
///
/// Wraps a [`Credential`] for the access/refresh token material, so `Debug`/`Display` are the
/// same fixed `<redacted>` string and this type is likewise never `Serialize`.
#[derive(Clone)]
pub struct OAuthTokens {
    credential: Credential,
    expires_at: Option<Timestamp>,
}

impl OAuthTokens {
    /// Build a token pair, computing `expires_at` from `issued_at + expires_in` when the server
    /// reported a lifetime.
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
        issued_at: Timestamp,
    ) -> Self {
        OAuthTokens {
            credential: Credential::oauth(access_token, refresh_token),
            expires_at: expires_in.map(|secs| issued_at.plus_seconds(secs as i64)),
        }
    }

    /// The bearer access token.
    pub fn access_token(&self) -> &str {
        self.credential.expose_secret()
    }

    /// The refresh token, if the server issued one.
    pub fn refresh_token(&self) -> Option<&str> {
        self.credential.refresh_token()
    }

    /// Whether this token pair is expired (or expiring within `skew` seconds) as of `now`.
    /// A token with no known expiry is treated as never expiring.
    pub fn is_expired(&self, now: Timestamp, skew_seconds: i64) -> bool {
        match self.expires_at {
            Some(exp) => now.seconds_since(exp) >= -skew_seconds,
            None => false,
        }
    }
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("credential", &self.credential)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Where a [`DeviceCodeOAuth`] adapter persists resolved tokens between calls, keyed by adapter
/// id. Pluggable so a keychain-backed implementation can replace [`InMemoryTokenStore`] once
/// [`crate::KeychainApiKey`]'s platform integration lands, without changing
/// [`DeviceCodeOAuth`]'s API.
pub trait TokenStore: Send + Sync {
    /// Load previously stored tokens for `id`, if any.
    fn load(&self, id: &str) -> Option<OAuthTokens>;
    /// Persist `tokens` under `id`, replacing whatever was stored before.
    fn save(&self, id: &str, tokens: OAuthTokens);
}

/// The only [`TokenStore`] this crate ships: process-memory only, gone on exit. Deliberately not
/// a file-backed store — see this module's top-level docs for why.
#[derive(Default)]
pub struct InMemoryTokenStore {
    inner: Mutex<HashMap<String, OAuthTokens>>,
}

impl InMemoryTokenStore {
    /// An empty store.
    pub fn new() -> Self {
        InMemoryTokenStore::default()
    }
}

impl TokenStore for InMemoryTokenStore {
    fn load(&self, id: &str) -> Option<OAuthTokens> {
        self.inner.lock().get(id).cloned()
    }

    fn save(&self, id: &str, tokens: OAuthTokens) {
        self.inner.lock().insert(id.to_string(), tokens);
    }
}

/// OAuth 2.0 device authorization grant (RFC 8628), with transparent refresh.
///
/// [`AuthAdapter::credential`] resolves from the [`TokenStore`] if a still-valid (or
/// refreshable) token is held; otherwise it returns [`AuthError::OAuthFailed`] rather than
/// launching an interactive flow itself — starting the *interactive* device-code dance
/// (printing the user code, opening the verification URL) is the `tm auth <provider>` CLI verb's
/// job ([`DeviceCodeOAuth::authorize`]), not something a routing-path `credential()` call should
/// block on.
pub struct DeviceCodeOAuth {
    id: String,
    config: DeviceCodeConfig,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    store: Arc<dyn TokenStore>,
    /// Seconds of slack before expiry at which a token is treated as needing refresh.
    refresh_skew_seconds: i64,
}

impl DeviceCodeOAuth {
    /// Build an adapter for `id` against `config`, persisting tokens through `store`.
    pub fn new(
        id: impl Into<String>,
        config: DeviceCodeConfig,
        clock: Arc<dyn Clock>,
        store: Arc<dyn TokenStore>,
    ) -> Result<Self, AuthError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| AuthError::OAuthFailed {
                detail: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(DeviceCodeOAuth {
            id: id.into(),
            config,
            http,
            clock,
            store,
            refresh_skew_seconds: 60,
        })
    }

    /// Request a device code from [`DeviceCodeConfig::device_authorization_endpoint`] (RFC 8628
    /// §3.1). The caller (the `tm auth <provider>` CLI verb) is responsible for showing
    /// `user_code`/`verification_uri` to the user before calling [`DeviceCodeOAuth::poll_until_authorized`].
    pub async fn request_device_code(&self) -> Result<DeviceAuthorizationResponse, AuthError> {
        let mut form = vec![("client_id", self.config.client_id.as_str())];
        if let Some(scope) = &self.config.scope {
            form.push(("scope", scope.as_str()));
        }
        let resp = self
            .http
            .post(&self.config.device_authorization_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| AuthError::OAuthFailed {
                detail: format!("device authorization request failed: {e}"),
            })?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| AuthError::OAuthFailed {
            detail: format!("reading device authorization response failed: {e}"),
        })?;
        serde_json::from_slice(&body).map_err(|_| AuthError::OAuthFailed {
            detail: format!("http {status}: unparseable device authorization response"),
        })
    }

    /// One poll of the token endpoint (RFC 8628 §3.4), interpreted via
    /// [`interpret_token_response`].
    async fn poll_once(&self, device_code: &str) -> Result<PollOutcome, AuthError> {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("client_id", self.config.client_id.as_str()),
        ];
        let resp = self
            .http
            .post(&self.config.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| AuthError::OAuthFailed {
                detail: format!("token poll request failed: {e}"),
            })?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(|e| AuthError::OAuthFailed {
            detail: format!("reading token poll response failed: {e}"),
        })?;
        Ok(interpret_token_response(status, &body))
    }

    /// Poll until the user completes authorization, the device code expires, or the server
    /// denies the request — RFC 8628 §3.5's client polling loop, honoring `slow_down` by
    /// widening the interval and `authorization_pending` by waiting the configured interval.
    /// On success, persists the resulting tokens through this adapter's [`TokenStore`] and
    /// returns the resolved [`Credential`].
    pub async fn poll_until_authorized(
        &self,
        device_auth: &DeviceAuthorizationResponse,
    ) -> Result<Credential, AuthError> {
        let mut interval = Duration::from_secs(device_auth.interval.max(1));
        let deadline = self.clock.now().plus_seconds(device_auth.expires_in as i64);

        loop {
            if self.clock.now().seconds_since(deadline) >= 0 {
                return Err(AuthError::DeviceCodeExpired);
            }
            tokio::time::sleep(interval).await;
            match self.poll_once(&device_auth.device_code).await? {
                PollOutcome::Authorized {
                    access_token,
                    refresh_token,
                    expires_in,
                } => {
                    let tokens =
                        OAuthTokens::new(access_token, refresh_token, expires_in, self.clock.now());
                    let credential = Credential::oauth(
                        tokens.access_token().to_string(),
                        tokens.refresh_token().map(str::to_string),
                    );
                    self.store.save(&self.id, tokens);
                    return Ok(credential);
                }
                PollOutcome::Pending => continue,
                PollOutcome::SlowDown => {
                    interval += Duration::from_secs(5);
                    continue;
                }
                PollOutcome::Expired => return Err(AuthError::DeviceCodeExpired),
                PollOutcome::Denied => return Err(AuthError::AccessDenied),
                PollOutcome::Failed { detail } => return Err(AuthError::OAuthFailed { detail }),
            }
        }
    }

    /// Run the full interactive device-code flow: request a device code, hand
    /// `(user_code, verification_uri)` to `on_prompt` (the CLI verb's hook for printing
    /// instructions), then poll until authorized.
    pub async fn authorize(
        &self,
        on_prompt: impl FnOnce(&DeviceAuthorizationResponse),
    ) -> Result<Credential, AuthError> {
        let device_auth = self.request_device_code().await?;
        on_prompt(&device_auth);
        self.poll_until_authorized(&device_auth).await
    }

    /// Refresh using a held refresh token (RFC 6749 §6). Persists and returns the refreshed
    /// credential; does not consult or update `on_prompt`-style interactive state, since a
    /// refresh never requires the user to do anything.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Credential, AuthError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.config.client_id.as_str()),
        ];
        let resp = self
            .http
            .post(&self.config.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| AuthError::OAuthFailed {
                detail: format!("refresh request failed: {e}"),
            })?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(|e| AuthError::OAuthFailed {
            detail: format!("reading refresh response failed: {e}"),
        })?;
        let outcome = interpret_token_response(status, &body);
        let tokens = tokens_from_refresh(Some(refresh_token), outcome, status, self.clock.now())?;
        let credential = Credential::oauth(
            tokens.access_token().to_string(),
            tokens.refresh_token().map(str::to_string),
        );
        self.store.save(&self.id, tokens);
        Ok(credential)
    }
}

/// Merge a refresh-endpoint [`PollOutcome`] into a stored [`OAuthTokens`], falling back to
/// `prior_refresh` when the server's response omits `refresh_token` — RFC 6749 §6 explicitly
/// permits a refresh response to omit it when the token isn't rotated, and servers (Google's
/// among them) routinely do this. Without the fallback, the *first* refresh after the initial
/// grant would silently drop the refresh token, and the *second* expiry would report
/// [`AuthError::NoRefreshToken`] instead of refreshing — exactly the "refresh interrupts
/// in-flight work" failure `SPEC.md` §28.2 says an auth adapter must not produce. Pure (no I/O,
/// no clock read beyond the `now` it's handed), so it's unit-testable without a mocked server —
/// [`DeviceCodeOAuth::refresh`] itself can't be, since it makes a real HTTP call.
///
/// `pub(crate)` (not private) so `crate::codex_subscription::CodexSubscriptionOAuth::refresh` can
/// reuse the exact same RFC 6749 §6 merge logic instead of re-deriving it.
pub(crate) fn tokens_from_refresh(
    prior_refresh: Option<&str>,
    outcome: PollOutcome,
    status: u16,
    now: Timestamp,
) -> Result<OAuthTokens, AuthError> {
    match outcome {
        PollOutcome::Authorized {
            access_token,
            refresh_token,
            expires_in,
        } => {
            let refresh_token = refresh_token.or_else(|| prior_refresh.map(str::to_string));
            Ok(OAuthTokens::new(
                access_token,
                refresh_token,
                expires_in,
                now,
            ))
        }
        PollOutcome::Failed { detail } => Err(AuthError::OAuthFailed { detail }),
        // A refresh attempt cannot itself be "pending"/"slow down"/"denied" in the RFC 6749
        // sense; a server returning one of those error codes here is still just a failure.
        _ => Err(AuthError::OAuthFailed {
            detail: format!("http {status}: unexpected refresh response"),
        }),
    }
}

#[async_trait::async_trait]
impl AuthAdapter for DeviceCodeOAuth {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> CredentialKind {
        CredentialKind::SubscriptionOAuth
    }

    /// Resolves from the [`TokenStore`]: a still-valid token is returned as-is, an expired one
    /// with a refresh token is refreshed transparently (§28.2: "Tokens refresh without
    /// interrupting in-flight work"), and no stored token at all is reported as a routing
    /// failure rather than launching an interactive flow from inside a `credential()` call.
    async fn credential(&self) -> Result<Credential, AuthError> {
        let Some(tokens) = self.store.load(&self.id) else {
            return Err(AuthError::OAuthFailed {
                detail: format!(
                    "no stored token for {}; run `tm auth <provider>` to authorize",
                    self.id
                ),
            });
        };
        if !tokens.is_expired(self.clock.now(), self.refresh_skew_seconds) {
            return Ok(Credential::oauth(
                tokens.access_token().to_string(),
                tokens.refresh_token().map(str::to_string),
            ));
        }
        match tokens.refresh_token() {
            Some(refresh_token) => self.refresh(refresh_token).await,
            None => Err(AuthError::NoRefreshToken),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::FixedClock;

    // -- interpret_token_response: pure, no network client, safe under hygiene's
    // check_network_in_tests. --

    #[test]
    fn interpret_authorized_response() {
        let body = br#"{"access_token":"tok-123","refresh_token":"ref-456","expires_in":3600}"#;
        let outcome = interpret_token_response(200, body);
        assert_eq!(
            outcome,
            PollOutcome::Authorized {
                access_token: "tok-123".to_string(),
                refresh_token: Some("ref-456".to_string()),
                expires_in: Some(3600),
            }
        );
    }

    #[test]
    fn interpret_pending_slow_down_expired_denied() {
        let cases = [
            (r#"{"error":"authorization_pending"}"#, PollOutcome::Pending),
            (r#"{"error":"slow_down"}"#, PollOutcome::SlowDown),
            (r#"{"error":"expired_token"}"#, PollOutcome::Expired),
            (r#"{"error":"access_denied"}"#, PollOutcome::Denied),
        ];
        for (body, expected) in cases {
            assert_eq!(interpret_token_response(400, body.as_bytes()), expected);
        }
    }

    #[test]
    fn interpret_unknown_error_becomes_failed_with_detail() {
        let body = br#"{"error":"invalid_grant","error_description":"grant expired"}"#;
        match interpret_token_response(400, body) {
            PollOutcome::Failed { detail } => assert_eq!(detail, "grant expired"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn interpret_unparseable_body_becomes_failed() {
        let outcome = interpret_token_response(502, b"not json");
        assert!(matches!(outcome, PollOutcome::Failed { .. }));
    }

    // -- tokens_from_refresh: the merge logic behind DeviceCodeOAuth::refresh. --

    #[test]
    fn refresh_response_omitting_refresh_token_falls_back_to_the_prior_one() {
        // RFC 6749 §6 permits a refresh response to omit `refresh_token` when it isn't rotated;
        // losing it here would mean the *next* expiry has nothing to refresh with.
        let outcome = PollOutcome::Authorized {
            access_token: "new-access".to_string(),
            refresh_token: None,
            expires_in: Some(60),
        };
        let tokens = tokens_from_refresh(Some("prior-refresh"), outcome, 200, Timestamp::EPOCH)
            .expect("authorized outcome");
        assert_eq!(tokens.access_token(), "new-access");
        assert_eq!(tokens.refresh_token(), Some("prior-refresh"));
    }

    #[test]
    fn refresh_response_with_a_new_refresh_token_rotates_it() {
        let outcome = PollOutcome::Authorized {
            access_token: "new-access".to_string(),
            refresh_token: Some("rotated-refresh".to_string()),
            expires_in: Some(60),
        };
        let tokens = tokens_from_refresh(Some("prior-refresh"), outcome, 200, Timestamp::EPOCH)
            .expect("authorized outcome");
        assert_eq!(tokens.refresh_token(), Some("rotated-refresh"));
    }

    #[test]
    fn refresh_failed_outcome_propagates_its_detail() {
        let outcome = PollOutcome::Failed {
            detail: "invalid_grant".to_string(),
        };
        let err = tokens_from_refresh(Some("prior-refresh"), outcome, 400, Timestamp::EPOCH)
            .expect_err("failed outcome");
        assert_eq!(
            err,
            AuthError::OAuthFailed {
                detail: "invalid_grant".to_string()
            }
        );
    }

    #[test]
    fn refresh_unexpected_outcome_reports_the_http_status() {
        let err = tokens_from_refresh(
            Some("prior-refresh"),
            PollOutcome::Pending,
            202,
            Timestamp::EPOCH,
        )
        .expect_err("pending is not a valid refresh outcome");
        match err {
            AuthError::OAuthFailed { detail } => assert!(detail.contains("202")),
            other => panic!("expected OAuthFailed, got {other:?}"),
        }
    }

    // -- OAuthTokens / InMemoryTokenStore --

    #[test]
    fn oauth_tokens_expiry_uses_injected_clock_not_wall_time() {
        let issued = Timestamp::from_unix_seconds(1_000);
        let tokens = OAuthTokens::new("tok", None, Some(60), issued);
        assert!(!tokens.is_expired(Timestamp::from_unix_seconds(1_030), 0));
        assert!(tokens.is_expired(Timestamp::from_unix_seconds(1_061), 0));
    }

    #[test]
    fn oauth_tokens_with_no_expiry_never_expires() {
        let tokens = OAuthTokens::new("tok", None, None, Timestamp::EPOCH);
        assert!(!tokens.is_expired(Timestamp::from_unix_seconds(10_000_000), 0));
    }

    #[test]
    fn oauth_tokens_debug_redacts_credential_but_shows_expiry() {
        let tokens = OAuthTokens::new("tok-secret", None, Some(60), Timestamp::EPOCH);
        let rendered = format!("{tokens:?}");
        assert!(!rendered.contains("tok-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn in_memory_token_store_round_trips() {
        let store = InMemoryTokenStore::new();
        assert!(store.load("adapter-a").is_none());
        let tokens = OAuthTokens::new("tok", Some("ref".to_string()), Some(60), Timestamp::EPOCH);
        store.save("adapter-a", tokens);
        let loaded = store.load("adapter-a").expect("just saved");
        assert_eq!(loaded.access_token(), "tok");
        assert_eq!(loaded.refresh_token(), Some("ref"));
        assert!(store.load("adapter-b").is_none());
    }

    // -- AuthAdapter::credential, driven purely off an injected clock + in-memory store --

    #[tokio::test]
    async fn credential_with_no_stored_token_reports_oauth_failed_without_starting_a_flow() {
        let adapter = DeviceCodeOAuth::new(
            "test-provider",
            DeviceCodeConfig {
                device_authorization_endpoint: "https://example.invalid/device".to_string(),
                token_endpoint: "https://example.invalid/token".to_string(),
                client_id: "client".to_string(),
                scope: None,
            },
            Arc::new(FixedClock::epoch()),
            Arc::new(InMemoryTokenStore::new()),
        )
        .expect("build adapter");
        let err = adapter.credential().await.expect_err("nothing stored yet");
        assert!(matches!(err, AuthError::OAuthFailed { .. }));
    }

    #[tokio::test]
    async fn credential_returns_stored_token_when_not_expired() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let store: Arc<dyn TokenStore> = Arc::new(InMemoryTokenStore::new());
        store.save(
            "test-provider",
            OAuthTokens::new("still-valid", None, Some(3600), clock.now()),
        );
        let adapter = DeviceCodeOAuth::new(
            "test-provider",
            DeviceCodeConfig {
                device_authorization_endpoint: "https://example.invalid/device".to_string(),
                token_endpoint: "https://example.invalid/token".to_string(),
                client_id: "client".to_string(),
                scope: None,
            },
            clock,
            store,
        )
        .expect("build adapter");
        let cred = adapter.credential().await.expect("token is fresh");
        assert_eq!(cred.expose_secret(), "still-valid");
    }

    #[tokio::test]
    async fn credential_with_expired_token_and_no_refresh_token_reports_no_refresh_token() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let store: Arc<dyn TokenStore> = Arc::new(InMemoryTokenStore::new());
        // expires_in: 0 with skew 60 makes this immediately expired relative to `clock.now()`.
        store.save(
            "test-provider",
            OAuthTokens::new("stale", None, Some(0), clock.now()),
        );
        let adapter = DeviceCodeOAuth::new(
            "test-provider",
            DeviceCodeConfig {
                device_authorization_endpoint: "https://example.invalid/device".to_string(),
                token_endpoint: "https://example.invalid/token".to_string(),
                client_id: "client".to_string(),
                scope: None,
            },
            clock,
            store,
        )
        .expect("build adapter");
        let err = adapter
            .credential()
            .await
            .expect_err("expired, no refresh token");
        assert_eq!(err, AuthError::NoRefreshToken);
    }
}
