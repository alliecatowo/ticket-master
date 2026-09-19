//! The redaction test `SPEC.md` §28.2 and the audit both call out: prove a credential's string
//! forms cannot reach a `tm_core::Store::append`ed event, even when a caller deliberately tries.
//!
//! This crate's other legs of the redaction guarantee (`--json` CLI output, error `Display`
//! from the credential-resolution path itself) live in `crates/tm-cli/tests/auth_redaction.rs`
//! and `crates/tm-auth/src/{credential,error}.rs`'s own unit tests respectively — see this
//! workspace's tm-auth task report for why the guarantee is split across those three places
//! rather than one file (in short: `CARGO_BIN_EXE_tm` is only set for tests inside `tm-cli`
//! itself, so the CLI leg cannot live here).
//!
//! `crates/xtask/src/hygiene.rs`'s `check_network_in_tests` flags any test literal reading
//! `ANTHROPIC_API_KEY` (a real provider's credential env var) via `var(`/`set_var`, since that's
//! the "test depends on a real credential being present" hazard it exists to catch. This test
//! uses a synthetic, test-only variable name instead, planting a canary *value* rather than
//! depending on any real secret.

use std::sync::Arc;

use tempfile::TempDir;
use tm_auth::{AuthAdapter, AuthError, EnvApiKey};
use tm_core::Store;
use tm_events::payload::ProviderDegradedPayload;
use tm_events::{EventDraft, Payload};
use tm_types::{Clock, CounterIds, FixedClock, Id, IdSource, ParticipantId};

const CANARY: &str = "CANARY-SECRET-VALUE-4f8c1e9a";

#[tokio::test]
async fn credential_string_forms_never_reach_an_appended_event_payload() {
    // Resolve a real Credential the same way a migrated provider does, via EnvApiKey, so this
    // exercises the actual adapter path rather than constructing a Credential directly.
    let var = "TM_AUTH_REDACTION_TEST_CANARY_EVENT_PAYLOAD";
    std::env::set_var(var, CANARY);
    let adapter = EnvApiKey::new(var);
    let cred = adapter.credential().await.expect("var is set");
    std::env::remove_var(var);
    assert_eq!(cred.expose_secret(), CANARY, "sanity: canary resolved");

    // The deliberate leak attempt: exactly what a careless caller would write trying to explain
    // an auth failure in a free-text event field — interpolating the credential's Display,
    // Debug, and an AuthError built from its Display, all into one string.
    let err = AuthError::OAuthFailed {
        detail: format!("{cred}"),
    };
    let reason = format!("provider auth failed: {cred} {cred:?} {err}");
    assert!(
        !reason.contains(CANARY),
        "the leak attempt itself must already be inert: {reason}"
    );

    let dir = TempDir::new().expect("tempdir");
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
    let store = Store::open_with(dir.path(), clock, ids).expect("open store");

    let events = store
        .append(vec![EventDraft::new(
            ParticipantId::system(),
            Id::none(),
            Payload::from(ProviderDegradedPayload {
                provider: "openai".to_string(),
                reason,
            }),
        )])
        .expect("append event");
    assert_eq!(events.len(), 1);

    for event in &events {
        // The exact bytes `tm_core::Store` persists to `events.payload` (`Payload::to_json`).
        let json = event.payload.to_json().expect("serialize payload");
        let json_text = json.to_string();
        assert!(
            !json_text.contains(CANARY),
            "canary leaked into persisted event payload JSON: {json_text}"
        );

        // The shape `tm events show`/`tm events tail` would render (Event's Debug, which
        // includes the payload).
        let debug_text = format!("{event:?}");
        assert!(
            !debug_text.contains(CANARY),
            "canary leaked into Event Debug output: {debug_text}"
        );
    }
}
