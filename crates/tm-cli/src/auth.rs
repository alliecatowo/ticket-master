//! `tm auth <provider>`: interactive login for a provider's credential, per `SPEC.md` §28.2 —
//! "Interactive login is a `tm auth` verb per provider."
//!
//! Every provider `tm_provider::Registry::known_providers()` lists authenticates via
//! [`tm_auth::CredentialKind::ApiKey`] today (see [`credential_kind_for`]'s doc comment for why
//! this is a deliberate placeholder rather than something read off `ProviderInfo`), so this verb
//! prints setup instructions and reports current configuration status — never a credential's
//! value — for the [`tm_auth::EnvApiKey`] case. The [`tm_auth::CredentialKind::SubscriptionOAuth`]
//! arm exists and is where [`tm_auth::DeviceCodeOAuth`]'s interactive device-code UX belongs the
//! moment a provider needs it, but is unreachable today: no provider in the registry is
//! configured as OAuth-authenticated yet.

use tm_auth::EnvApiKey;

use crate::args::AuthArgs;
use crate::render::Renderer;

/// Which [`tm_auth::CredentialKind`] `tm auth` should use for `provider_id`.
///
/// `tm_provider::ProviderInfo` (the provider fabric's own registry) carries no auth-adapter
/// metadata — it only knows env var names, not *how* a credential should be obtained. Every
/// provider this workspace ships reads a bare API key today (the ~18 sites `SPEC.md` §28.2's
/// audit found), so this returns [`tm_auth::CredentialKind::ApiKey`] unconditionally. The moment
/// a provider grows a subscription-OAuth path (a Claude Pro/Max plan, a ChatGPT Plus/Pro plan,
/// ...), it gets a match arm here routing to [`tm_auth::CredentialKind::SubscriptionOAuth`]
/// instead — this function is the one place that decision should be made, rather than each call
/// site guessing.
fn credential_kind_for(_provider_id: &str) -> tm_auth::CredentialKind {
    tm_auth::CredentialKind::ApiKey
}

/// `tm auth <provider>`
pub async fn auth(args: &AuthArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let requested = args.provider.to_lowercase();
    let known = tm_provider::Registry::known_providers();
    let info = known
        .into_iter()
        .find(|info| info.id.eq_ignore_ascii_case(&requested))
        .ok_or_else(|| {
            tm_types::TmError::parse(format!(
                "Unknown provider: {}. Run `tm provider detect` to list known providers.",
                args.provider
            ))
        })?;

    match credential_kind_for(info.id) {
        tm_auth::CredentialKind::ApiKey => auth_api_key(&info, renderer),
        tm_auth::CredentialKind::SubscriptionOAuth => Err(tm_types::TmError::Provider(format!(
            "{} authenticates via subscription OAuth, but no provider is wired to \
             tm_auth::DeviceCodeOAuth yet — this is a library capability (see crates/tm-auth) \
             without a CLI-reachable caller today",
            info.display_name
        ))),
        tm_auth::CredentialKind::CloudIam
        | tm_auth::CredentialKind::PlatformEphemeral
        | tm_auth::CredentialKind::Delegated => Err(tm_types::TmError::Provider(format!(
            "{} does not have a `tm auth` flow yet for this credential kind",
            info.display_name
        ))),
    }
}

/// One env var's setup row: name, whether it is required, and whether it is set.
/// Values are never carried here — only presence — so this is safe to render anywhere,
/// including the TUI transcript.
pub struct EnvSetupRow {
    /// The variable's name (`ANTHROPIC_API_KEY`).
    pub name: &'static str,
    /// Whether the provider needs it.
    pub required: bool,
    /// Whether it is set in the current environment.
    pub configured: bool,
}

/// The current per-variable setup rows for `info` (D-021): the same rows `tm auth`
/// prints, factored out so the chat's `/connect` renders byte-identical instructions.
pub fn env_setup_rows(info: &tm_provider::ProviderInfo) -> Vec<EnvSetupRow> {
    info.env_vars
        .iter()
        .map(|v| EnvSetupRow {
            name: v.name,
            required: v.required,
            configured: EnvApiKey::new(v.name).resolve().is_ok(),
        })
        .collect()
}

/// Render API-key setup instructions from a display name, one row per variable, and the
/// row data — the exact text `tm auth <provider>` prints, shared with `/connect` so the
/// two surfaces can never disagree. Descriptions come from the registry; statuses are
/// present/absent only, never a value.
pub fn format_auth_instructions(
    display_name: &str,
    descriptions: &[(&'static str, &'static str)],
    rows: &[EnvSetupRow],
) -> String {
    let mut lines = vec![format!(
        "{display_name} authenticates via API key. No interactive login needed — set the \
         environment variable(s) below and `tm` reads them directly."
    )];
    for row in rows {
        let description = descriptions
            .iter()
            .find(|(name, _)| *name == row.name)
            .map(|(_, d)| *d)
            .unwrap_or("");
        let status = if row.configured {
            "configured"
        } else {
            "not set"
        };
        let req = if row.required { "required" } else { "optional" };
        lines.push(format!("  {} ({req}, {status}): {description}", row.name));
    }
    lines.join("\n")
}

/// The [`tm_auth::CredentialKind::ApiKey`] case: no interactive flow, just instructions plus
/// current per-variable configuration status (present/absent only — never a value).
fn auth_api_key(info: &tm_provider::ProviderInfo, renderer: &Renderer) -> tm_types::Result<()> {
    let rows = env_setup_rows(info);
    let descriptions: Vec<(&'static str, &'static str)> = info
        .env_vars
        .iter()
        .map(|v| (v.name, v.description))
        .collect();

    if renderer.is_json() {
        let json = serde_json::json!({
            "provider": info.id,
            "kind": "api_key",
            "env_vars": rows.iter().map(|row| {
                let description = descriptions
                    .iter()
                    .find(|(name, _)| *name == row.name)
                    .map(|(_, d)| *d)
                    .unwrap_or("");
                serde_json::json!({
                    "name": row.name,
                    "required": row.required,
                    "description": description,
                    "configured": row.configured,
                })
            }).collect::<Vec<_>>(),
        });
        renderer.emit(&json, "")?;
    } else {
        let text = format_auth_instructions(info.display_name, &descriptions, &rows);
        renderer.emit(&(), &text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_instructions_render_names_and_status_without_values() {
        let rows = vec![
            EnvSetupRow {
                name: "ANTHROPIC_API_KEY",
                required: true,
                configured: false,
            },
            EnvSetupRow {
                name: "ANTHROPIC_BASE_URL",
                required: false,
                configured: true,
            },
        ];
        let text = format_auth_instructions(
            "Anthropic",
            &[
                ("ANTHROPIC_API_KEY", "Bearer API key"),
                ("ANTHROPIC_BASE_URL", "Override the default"),
            ],
            &rows,
        );
        assert!(
            text.contains("Anthropic authenticates via API key."),
            "{text}"
        );
        assert!(
            text.contains("ANTHROPIC_API_KEY (required, not set): Bearer API key"),
            "{text}"
        );
        assert!(
            text.contains("ANTHROPIC_BASE_URL (optional, configured): Override the default"),
            "{text}"
        );
    }

    #[test]
    fn credential_kind_for_every_known_provider_is_api_key_today() {
        // Documents the current state this module's docs describe: nothing in the registry is
        // OAuth-authenticated yet, so the SubscriptionOAuth arm in `auth()` is reachable by
        // construction (the match is exhaustive) but not by any provider id today.
        for info in tm_provider::Registry::known_providers() {
            assert_eq!(
                credential_kind_for(info.id),
                tm_auth::CredentialKind::ApiKey,
                "{} unexpectedly routed to a non-API-key credential kind",
                info.id
            );
        }
    }

    #[tokio::test]
    async fn unknown_provider_is_a_parse_error() {
        let renderer = Renderer::from_flags(false, true, true);
        let err = auth(
            &AuthArgs {
                provider: "not-a-real-provider".to_string(),
            },
            &renderer,
        )
        .await
        .expect_err("unknown provider must error");
        assert!(matches!(err, tm_types::TmError::Parse(_)));
    }

    #[tokio::test]
    async fn known_provider_reports_instructions_without_erroring() {
        let renderer = Renderer::from_flags(true, true, true);
        // "anthropic" is always present in `Registry::known_providers()`; this must succeed
        // (print instructions/status) regardless of whether ANTHROPIC_API_KEY happens to be set
        // in this test process, so it deliberately does not touch that variable at all.
        auth(
            &AuthArgs {
                provider: "anthropic".to_string(),
            },
            &renderer,
        )
        .await
        .expect("known provider must not error");
    }
}
