//! [`CodexSubscriptionOAuth`]: reads the real, already-authenticated OpenAI Codex CLI's stored
//! ChatGPT subscription session (`$CODEX_HOME/auth.json`, default `$HOME/.codex/auth.json`) and
//! exposes it as a [`crate::AuthAdapter`] of kind [`crate::CredentialKind::SubscriptionOAuth`] —
//! the fourth adapter alongside [`crate::EnvApiKey`], [`crate::KeychainApiKey`] and
//! [`crate::device_code::DeviceCodeOAuth`], and the first one that reads a credential another
//! program (the `codex` CLI, not this workspace) already produced rather than running its own
//! OAuth flow.
//!
//! # What the resulting token actually authenticates against
//!
//! `auth.json`'s `tokens.access_token` is a JWT. Decoding its header/payload (base64url, not
//! encrypted — the signature segment is the only unforgeable part, so reading the claims this way
//! reveals *shape*, never bypasses verification) shows:
//!
//! - `iss: "https://auth.openai.com"`, `client_id: "app_EMoamEEZ73f0CkXaXp7hrann"` — the Codex
//!   CLI's own registered OAuth client, not a bring-your-own-app credential.
//! - `aud: ["https://api.openai.com/v1"]` and `scp` containing `openid`, `profile`, `email`,
//!   `offline_access`, `api.connectors.read`, `api.connectors.invoke` — notably **not** a scope
//!   that grants the public Chat Completions/Responses API at `api.openai.com`. This token does
//!   not work as a drop-in `OPENAI_API_KEY` replacement against the public API.
//!
//! Cross-referenced against public documentation and third-party reverse-engineering writeups
//! (`developers.openai.com/codex/auth/ci-cd-auth`, `simonw/llm-openai-via-codex`, and others — see
//! `docs/decisions/D-016-codex-chatgpt-session-auth-adapter.md` for the full source list), the
//! real backend this token is valid against is `POST https://chatgpt.com/backend-api/codex/responses`
//! — an undocumented, Codex-CLI-specific endpoint speaking the Responses API wire shape, reached
//! with `Authorization: Bearer <access_token>` plus a `ChatGPT-Account-ID` header carrying
//! `tokens.account_id` from the same file (see [`CodexSubscriptionOAuth::account_id`], read
//! directly rather than decoded out of the JWT since the file already carries it verbatim). See
//! `crates/tm-provider/src/providers/codex_chatgpt.rs` for the [`crate`]-external
//! [`tm_provider::fabric::Provider`] that actually calls it.
//!
//! **This module resolves and refreshes the credential; it deliberately does not decide whether
//! reusing a ChatGPT-subscription session this way is a good idea to ship broadly** — see the
//! decision doc's honest costs section for that half of the question.
//!
//! # Refresh (RFC 6749 §6)
//!
//! `POST https://auth.openai.com/oauth/token` with `grant_type=refresh_token`, the stored
//! `refresh_token`, and `client_id=app_EMoamEEZ73f0CkXaXp7hrann` — the same token endpoint and
//! client id the JWT's own `iss`/`client_id` claims name, corroborated by the same sources above.
//! [`CodexSubscriptionOAuth::credential`] reuses [`crate::device_code::interpret_token_response`]
//! and `crate::device_code::tokens_from_refresh` (the same RFC 6749 §6 "a refresh response may
//! omit `refresh_token`" fallback [`crate::device_code::DeviceCodeOAuth`] relies on) rather than
//! re-deriving that merge logic here.
//!
//! # The concurrent-writer hazard this module cannot fully close
//!
//! `auth.json` is not this adapter's own private state — it is the real `codex` CLI's login
//! state, and `codex` may be running concurrently and refresh it independently. A refreshed token
//! response can *rotate* the refresh token; if this adapter persisted a rotated token but `codex`
//! still holds (and later replays) the old one, `codex`'s own next refresh would fail with the
//! server's stored token now stale — or vice versa. [`CodexSubscriptionOAuth::refresh`] narrows
//! (does not eliminate) this window by re-reading `auth.json` immediately before writing back,
//! merging only the token fields into that fresh read (so an unrelated field `codex` wrote in the
//! interim — e.g. `last_refresh` from its own refresh — survives), and writing atomically (a
//! sibling temp file, `chmod 0600` on Unix, then `rename` over the target, so no reader ever
//! observes a partially-written file). There is no cross-process lock: two processes racing to
//! refresh at the same instant can still each write a token the other doesn't know about. See
//! `docs/decisions/D-016-codex-chatgpt-session-auth-adapter.md` for why a real lock (e.g. an
//! `flock` on a sibling `.lock` file) was left out of this change's scope.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tm_types::Clock;

use crate::{AuthAdapter, AuthError, Credential, CredentialKind, Entitlement, QuotaClass};

/// RFC 6749 §6 token endpoint the real `codex` CLI's own login/refresh flow uses — see this
/// module's docs for how this was determined.
pub const CODEX_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

/// The Codex CLI's own OAuth client id, read directly off this machine's real stored access
/// token's `client_id` claim and corroborated against public documentation — see this module's
/// docs.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Seconds of slack before a token's `exp` claim at which it is treated as needing refresh,
/// mirroring [`crate::device_code::DeviceCodeOAuth`]'s own skew.
const REFRESH_SKEW_SECONDS: i64 = 60;

/// The filename this adapter reads/writes under [`resolve_codex_home`]'s result.
const AUTH_FILE_NAME: &str = "auth.json";

/// Pure resolution of the Codex CLI's config home from already-read environment values:
/// `codex_home_env` (`$CODEX_HOME`, non-empty) if present, else `$HOME/.codex`. Split out from
/// [`default_codex_home`] so the decision itself is unit-testable without mutating process-global
/// environment state (`crates/tm-auth/src/env_api_key.rs`'s tests already document why that's
/// worth avoiding: env vars are shared across every test thread in this binary).
fn resolve_codex_home(
    codex_home_env: Option<&str>,
    home_env: Option<&str>,
) -> Result<PathBuf, AuthError> {
    if let Some(dir) = codex_home_env {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = home_env.ok_or_else(|| {
        AuthError::Unavailable(
            "neither CODEX_HOME nor HOME is set; cannot locate the Codex CLI's auth.json"
                .to_string(),
        )
    })?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// Resolve the real process environment's Codex CLI config home: `$CODEX_HOME` if set to a
/// non-empty value, else `$HOME/.codex` — mirroring the real `codex` binary's own resolution
/// (`codex doctor` reports exactly this path under its "disk" section).
pub fn default_codex_home() -> Result<PathBuf, AuthError> {
    let codex_home = std::env::var("CODEX_HOME").ok();
    let home = std::env::var("HOME").ok();
    resolve_codex_home(codex_home.as_deref(), home.as_deref())
}

/// Reads `$CODEX_HOME/auth.json` (see [`default_codex_home`]) — a real Codex CLI ChatGPT
/// subscription session — and exposes it as a [`crate::AuthAdapter`]. See this module's docs for
/// what the resulting token authenticates against, refresh handling, and the concurrent-writer
/// hazard this adapter narrows but cannot fully close.
pub struct CodexSubscriptionOAuth {
    id: String,
    home: PathBuf,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
}

impl CodexSubscriptionOAuth {
    /// Build an adapter over the real `$CODEX_HOME`/`$HOME/.codex` (see [`default_codex_home`]).
    pub fn new(clock: Arc<dyn Clock>) -> Result<Self, AuthError> {
        let home = default_codex_home()?;
        Self::with_home(home, clock)
    }

    /// Build an adapter over an explicit `home` directory holding `auth.json` — the seam tests
    /// (and any caller that wants to point at a non-default `$CODEX_HOME`) use instead of
    /// touching the real, machine-local Codex session.
    pub fn with_home(home: PathBuf, clock: Arc<dyn Clock>) -> Result<Self, AuthError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| AuthError::OAuthFailed {
                detail: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(CodexSubscriptionOAuth {
            id: "codex-chatgpt-subscription".to_string(),
            home,
            http,
            clock,
        })
    }

    /// Override this adapter's [`crate::AuthAdapter::id`] (default
    /// `"codex-chatgpt-subscription"`) — mirrors [`crate::EnvApiKey::with_id`]'s ergonomics.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    fn auth_file_path(&self) -> PathBuf {
        self.home.join(AUTH_FILE_NAME)
    }

    fn read_auth_file(&self) -> Result<Value, AuthError> {
        let path = self.auth_file_path();
        let bytes = std::fs::read(&path).map_err(|e| {
            AuthError::Unavailable(format!("failed to read {}: {e}", path.display()))
        })?;
        serde_json::from_slice(&bytes).map_err(|e| {
            AuthError::Unavailable(format!("{} is not valid JSON: {e}", path.display()))
        })
    }

    /// Atomically overwrite `auth.json` with `value`: write to a sibling temp file (`chmod 0600`
    /// on Unix), then `rename` over the original — `rename` is atomic within one filesystem, and
    /// the temp file lives in the same directory as the target for exactly that reason, so a
    /// concurrently-running real `codex` process (or a second instance of this adapter) never
    /// observes a partially-written file. See this module's docs for the narrower race this does
    /// *not* close (a concurrent refresh racing this one).
    fn write_auth_file(&self, value: &Value) -> Result<(), AuthError> {
        let path = self.auth_file_path();
        let tmp_path = self
            .home
            .join(format!("{AUTH_FILE_NAME}.tmp-{}", std::process::id()));
        let body = serde_json::to_vec_pretty(value).map_err(|e| {
            AuthError::Unavailable(format!("failed to serialize refreshed auth.json: {e}"))
        })?;
        std::fs::write(&tmp_path, &body).map_err(|e| {
            AuthError::Unavailable(format!("failed to write {}: {e}", tmp_path.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
            {
                // Not fatal: the rename below still lands the refreshed tokens, just with
                // whatever permissions the temp file inherited from the process umask. Losing
                // the 0600 tightening is worth surfacing, not worth aborting a successful
                // refresh over.
                tracing::warn!(
                    path = %tmp_path.display(),
                    error = %e,
                    "failed to set 0600 permissions on refreshed auth.json temp file"
                );
            }
        }
        std::fs::rename(&tmp_path, &path).map_err(|e| {
            AuthError::Unavailable(format!(
                "failed to move refreshed auth.json into place: {e}"
            ))
        })?;
        Ok(())
    }

    /// The `tokens.account_id` field straight off `auth.json` — non-secret (an opaque
    /// account/org identifier, not a credential) and read directly rather than decoded out of
    /// the JWT, since the file already carries it verbatim. This is what
    /// `crates/tm-provider/src/providers/codex_chatgpt.rs`'s `ChatGPT-Account-ID` header needs;
    /// [`Credential`] deliberately has no field for it (see that type's docs on why it carries
    /// only `{secret, refresh}`), so a caller threads this through separately at provider
    /// construction time rather than through the credential itself.
    ///
    /// `Ok(None)` if the file has no such field; `Err` only on a read/parse failure.
    pub fn account_id(&self) -> Result<Option<String>, AuthError> {
        let value = self.read_auth_file()?;
        Ok(value
            .get("tokens")
            .and_then(|t| t.get("account_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }

    async fn refresh(&self, refresh_token: &str) -> Result<Credential, AuthError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CODEX_CLIENT_ID),
        ];
        let resp = self
            .http
            .post(CODEX_TOKEN_ENDPOINT)
            .form(&form)
            .send()
            .await
            .map_err(|e| AuthError::OAuthFailed {
                detail: format!("codex token refresh request failed: {e}"),
            })?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(|e| AuthError::OAuthFailed {
            detail: format!("reading codex token refresh response failed: {e}"),
        })?;
        let outcome = crate::device_code::interpret_token_response(status, &body);
        let tokens = crate::device_code::tokens_from_refresh(
            Some(refresh_token),
            outcome,
            status,
            self.clock.now(),
        )?;

        // Re-read immediately before writing back rather than reusing the snapshot `credential`
        // read earlier — narrows (does not close) the race against a concurrently-refreshing real
        // `codex` process. See this module's top docs, "concurrent-writer hazard".
        let mut fresh = self
            .read_auth_file()
            .unwrap_or_else(|_| serde_json::json!({}));
        if !fresh.is_object() {
            fresh = serde_json::json!({});
        }
        // Both `.as_object_mut()` calls below are on values this function just constructed as
        // `Value::Object` (either the fresh read, when it parsed to an object, or the `json!({})`
        // fallback above / inserted immediately below), so they cannot fail.
        let obj = fresh
            .as_object_mut()
            .expect("fresh is always an object: ensured above");
        let tokens_slot = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
        if !tokens_slot.is_object() {
            *tokens_slot = serde_json::json!({});
        }
        let tokens_obj = tokens_slot
            .as_object_mut()
            .expect("tokens_slot is always an object: ensured above");
        tokens_obj.insert(
            "access_token".to_string(),
            Value::String(tokens.access_token().to_string()),
        );
        if let Some(refresh) = tokens.refresh_token() {
            tokens_obj.insert(
                "refresh_token".to_string(),
                Value::String(refresh.to_string()),
            );
        }
        obj.insert(
            "last_refresh".to_string(),
            Value::String(self.clock.now().to_rfc3339()),
        );

        self.write_auth_file(&fresh)?;

        Ok(Credential::oauth(
            tokens.access_token().to_string(),
            tokens.refresh_token().map(str::to_string),
        ))
    }
}

#[async_trait]
impl AuthAdapter for CodexSubscriptionOAuth {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> CredentialKind {
        CredentialKind::SubscriptionOAuth
    }

    /// Reads `auth.json` fresh on every call (the real `codex` CLI may have refreshed it
    /// independently since the last call, so caching in-process would risk using a token `codex`
    /// itself has already rotated past). A still-valid token is returned as-is; one at or past
    /// its `exp` claim (minus [`REFRESH_SKEW_SECONDS`]) is refreshed transparently via
    /// [`CodexSubscriptionOAuth::refresh`], mirroring
    /// [`crate::device_code::DeviceCodeOAuth::credential`]'s same contract.
    async fn credential(&self) -> Result<Credential, AuthError> {
        let file = self.read_auth_file()?;
        let tokens = file.get("tokens").ok_or_else(|| {
            AuthError::Unavailable(format!(
                "{} has no \"tokens\" field",
                self.auth_file_path().display()
            ))
        })?;
        let access_token = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AuthError::Unavailable(format!(
                    "{} has no tokens.access_token",
                    self.auth_file_path().display()
                ))
            })?
            .to_string();
        let refresh_token = tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // A token with no decodable `exp` claim is treated as not expired, mirroring
        // `OAuthTokens::is_expired`'s own convention for a token with no known expiry.
        let needs_refresh = match decode_jwt_exp(&access_token) {
            Some(exp) => self.clock.now().unix_seconds() >= exp - REFRESH_SKEW_SECONDS,
            None => false,
        };

        if !needs_refresh {
            return Ok(Credential::oauth(access_token, refresh_token));
        }

        match refresh_token {
            Some(rt) => self.refresh(&rt).await,
            None => Err(AuthError::NoRefreshToken),
        }
    }

    /// ChatGPT Plus/Pro subscription capacity, not metered API spend (`SPEC.md` §31.1) — see
    /// [`crate::CredentialKind::SubscriptionOAuth`]'s own docs for why this is a distinct bucket
    /// from [`crate::CredentialKind::ApiKey`]'s conservative metered default.
    fn entitlement(&self) -> Entitlement {
        Entitlement {
            quota_class: QuotaClass::Subscription,
            rpm: None,
            tpm: None,
            metered: false,
            daily_ceiling: None,
        }
    }
}

// ---- JWT payload decode: `exp` claim only, no signature verification -----------------------
//
// A JWT's header and payload segments are base64url-encoded JSON, not encrypted — only the third
// (signature) segment is unforgeable. Reading `exp` out of the payload this way is exactly what
// every OAuth client does locally to decide "should I refresh yet" without a network round trip;
// it is not a substitute for the issuer verifying the signature server-side on every real API
// call, which this adapter never attempts to do itself (it holds no public key to verify against,
// and does not need one for this purpose).

/// Decode a JWT's `exp` claim (seconds since the Unix epoch) without verifying its signature.
/// Returns `None` if `token` is not JWT-shaped (not exactly 3 dot-separated segments), the
/// payload segment is not valid base64url/JSON, or the JSON has no numeric `exp` field —
/// [`CodexSubscriptionOAuth::credential`] treats `None` as "no expiry known", never as an error.
fn decode_jwt_exp(token: &str) -> Option<i64> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None; // more than 3 segments: not a JWT this decoder understands
    }
    let bytes = base64url_decode(payload)?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp")?.as_i64()
}

/// Decode a base64url (RFC 4648 §5) string into bytes, tolerating (but not requiring) `=`
/// padding. Hand-rolled rather than adding a `base64` crate dependency for one small decode used
/// only to read a JWT's `exp` claim locally — `crates/tm-auth/src/keychain.rs`'s own doc comment
/// is this crate's standing precedent that a new dependency is a real decision, not a mechanical
/// one, and this one is easily avoided.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        if byte == b'=' {
            continue;
        }
        let v = sextet(byte)?;
        buffer = (buffer << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::FixedClock;

    // ---- resolve_codex_home: pure, no env mutation ------------------------------------------

    #[test]
    fn resolve_codex_home_prefers_a_nonempty_codex_home() {
        let home = resolve_codex_home(Some("/custom/codex"), Some("/Users/x")).expect("resolves");
        assert_eq!(home, PathBuf::from("/custom/codex"));
    }

    #[test]
    fn resolve_codex_home_falls_back_to_home_dot_codex_when_codex_home_is_empty() {
        let home = resolve_codex_home(Some(""), Some("/Users/x")).expect("resolves");
        assert_eq!(home, PathBuf::from("/Users/x/.codex"));
    }

    #[test]
    fn resolve_codex_home_falls_back_to_home_dot_codex_when_codex_home_is_unset() {
        let home = resolve_codex_home(None, Some("/Users/x")).expect("resolves");
        assert_eq!(home, PathBuf::from("/Users/x/.codex"));
    }

    #[test]
    fn resolve_codex_home_errors_when_neither_var_is_set() {
        let err = resolve_codex_home(None, None).expect_err("nothing to resolve against");
        assert!(matches!(err, AuthError::Unavailable(_)));
    }

    // ---- base64url_decode / decode_jwt_exp: pure ---------------------------------------------

    /// Test-only mirror of `base64url_decode`, used to build fixture JWTs — production code
    /// never needs to *encode* a JWT, only decode one it received.
    fn base64url_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied();
            let b2 = chunk.get(2).copied();
            out.push(ALPHABET[(b0 >> 2) as usize] as char);
            out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
            if let Some(b1) = b1 {
                out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char);
            }
            if let Some(b2) = b2 {
                out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
            }
        }
        out
    }

    fn fake_jwt(payload_json: &serde_json::Value) -> String {
        let header = base64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = base64url_encode(payload_json.to_string().as_bytes());
        format!("{header}.{payload}.fake-signature-not-real")
    }

    #[test]
    fn base64url_roundtrips_through_the_test_encoder() {
        for sample in [
            b"".as_slice(),
            b"f".as_slice(),
            b"fo".as_slice(),
            b"foo".as_slice(),
            b"foob".as_slice(),
            b"fooba".as_slice(),
            b"foobar".as_slice(),
        ] {
            let encoded = base64url_encode(sample);
            let decoded = base64url_decode(&encoded).expect("decodes");
            assert_eq!(decoded, sample, "roundtrip failed for {sample:?}");
        }
    }

    #[test]
    fn decode_jwt_exp_reads_the_numeric_claim() {
        let token =
            fake_jwt(&serde_json::json!({"exp": 1_700_000_000, "iss": "https://auth.openai.com"}));
        assert_eq!(decode_jwt_exp(&token), Some(1_700_000_000));
    }

    #[test]
    fn decode_jwt_exp_is_none_for_malformed_or_missing_claim() {
        assert_eq!(decode_jwt_exp("not-a-jwt"), None);
        assert_eq!(decode_jwt_exp("only.two"), None);
        assert_eq!(decode_jwt_exp("a.b.c.d"), None);
        let token = fake_jwt(&serde_json::json!({"iss": "https://auth.openai.com"}));
        assert_eq!(decode_jwt_exp(&token), None);
    }

    // ---- CodexSubscriptionOAuth over a tempdir fixture: real file I/O, zero network ----------

    fn write_fixture(dir: &std::path::Path, access_token: &str, refresh_token: Option<&str>) {
        let mut tokens = serde_json::json!({
            "id_token": "unused-in-these-tests",
            "access_token": access_token,
            "account_id": "acct-fixture-0123",
        });
        if let Some(rt) = refresh_token {
            tokens["refresh_token"] = serde_json::Value::String(rt.to_string());
        }
        let file = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": serde_json::Value::Null,
            "tokens": tokens,
            "last_refresh": "2026-09-13T05:57:43.039696Z",
        });
        std::fs::write(
            dir.join("auth.json"),
            serde_json::to_vec_pretty(&file).expect("serializes"),
        )
        .expect("write fixture");
    }

    #[test]
    fn default_codex_home_is_not_exercised_directly_here() {
        // `default_codex_home` itself reads real process env vars (`CODEX_HOME`/`HOME`), which
        // this crate's tests deliberately avoid mutating (see `env_api_key.rs`'s own tests'
        // rationale) -- `resolve_codex_home` above carries the entire decision as a pure
        // function, so there is nothing further to test here beyond "it compiles and exists".
        let _ = default_codex_home;
    }

    #[tokio::test]
    async fn credential_reads_a_still_valid_token_without_refreshing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(
            tm_types::Timestamp::from_unix_seconds(1_000),
        ));
        let token =
            fake_jwt(&serde_json::json!({"exp": 1_000_000, "iss": "https://auth.openai.com"}));
        write_fixture(dir.path(), &token, Some("refresh-fixture-value"));

        let adapter = CodexSubscriptionOAuth::with_home(dir.path().to_path_buf(), clock)
            .expect("build adapter");
        let cred = adapter.credential().await.expect("token is fresh");
        assert_eq!(cred.expose_secret(), token);
        assert_eq!(cred.refresh_token(), Some("refresh-fixture-value"));
        assert_eq!(adapter.kind(), CredentialKind::SubscriptionOAuth);
    }

    #[tokio::test]
    async fn credential_with_expired_token_and_no_refresh_token_reports_no_refresh_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(
            tm_types::Timestamp::from_unix_seconds(2_000_000),
        ));
        let token =
            fake_jwt(&serde_json::json!({"exp": 1_000_000, "iss": "https://auth.openai.com"}));
        write_fixture(dir.path(), &token, None);

        let adapter = CodexSubscriptionOAuth::with_home(dir.path().to_path_buf(), clock)
            .expect("build adapter");
        let err = adapter
            .credential()
            .await
            .expect_err("expired with no refresh token");
        assert_eq!(err, AuthError::NoRefreshToken);
    }

    #[tokio::test]
    async fn credential_with_no_exp_claim_is_treated_as_not_expired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(
            tm_types::Timestamp::from_unix_seconds(9_999_999),
        ));
        let token = fake_jwt(&serde_json::json!({"iss": "https://auth.openai.com"}));
        write_fixture(dir.path(), &token, None);

        let adapter = CodexSubscriptionOAuth::with_home(dir.path().to_path_buf(), clock)
            .expect("build adapter");
        let cred = adapter
            .credential()
            .await
            .expect("no exp claim treated as never expiring");
        assert_eq!(cred.expose_secret(), token);
    }

    #[tokio::test]
    async fn credential_errors_clearly_when_auth_file_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let adapter = CodexSubscriptionOAuth::with_home(dir.path().to_path_buf(), clock)
            .expect("build adapter");
        let err = adapter
            .credential()
            .await
            .expect_err("no auth.json written");
        assert!(matches!(err, AuthError::Unavailable(_)));
    }

    #[test]
    fn account_id_reads_the_field_directly_without_decoding_the_jwt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        // Deliberately not a valid JWT: `account_id` must not need to decode the access token at
        // all to read this field.
        write_fixture(dir.path(), "not-a-jwt-at-all", None);

        let adapter = CodexSubscriptionOAuth::with_home(dir.path().to_path_buf(), clock)
            .expect("build adapter");
        assert_eq!(
            adapter.account_id().expect("reads"),
            Some("acct-fixture-0123".to_string())
        );
    }

    #[test]
    fn with_id_overrides_the_default_adapter_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let adapter = CodexSubscriptionOAuth::with_home(dir.path().to_path_buf(), clock)
            .expect("build adapter")
            .with_id("custom-id");
        assert_eq!(adapter.id(), "custom-id");
    }

    #[test]
    fn default_adapter_id_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let adapter = CodexSubscriptionOAuth::with_home(dir.path().to_path_buf(), clock)
            .expect("build adapter");
        assert_eq!(adapter.id(), "codex-chatgpt-subscription");
    }

    #[test]
    fn entitlement_reports_subscription_capacity_not_metered_spend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let adapter = CodexSubscriptionOAuth::with_home(dir.path().to_path_buf(), clock)
            .expect("build adapter");
        let e = adapter.entitlement();
        assert_eq!(e.quota_class, QuotaClass::Subscription);
        assert!(
            !e.metered,
            "a ChatGPT subscription session is not metered API spend"
        );
    }
}
