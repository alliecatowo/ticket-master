//! `browser.toml` parsing and validation, per `SPEC.md` §19.1a.
//!
//! Selects one or more [`crate::provider::BrowserProvider`]s and the fallback order to try them
//! in. The pinned browser version and its expected checksum are recorded here as project state,
//! not discovered at run time: "checking out an older commit re-runs its e2e against the
//! browser that commit was verified with" only holds if the pin lives in version control.
//!
//! The Chrome for Testing version index
//! (`googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json`)
//! does not itself publish a checksum for each build, so `sha256` is required here rather than
//! trusted from the index: the project, not a third-party JSON feed, is the source of truth for
//! what "verified" means. Compute it once (e.g. `curl -sL <url> | shasum -a 256`) and commit it.

use serde::{Deserialize, Serialize};
use tm_types::{Result, TmError};

/// `[managed]`: the default provider. Downloads a pinned Chrome for Testing build into
/// `~/.tm/browsers/<channel>-<version>/`, verifies it against `sha256`, and launches it per
/// session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedConfig {
    /// The Chrome for Testing release channel, e.g. `"stable"`. Only affects which entry of the
    /// version index this pin is expected to have come from; the installed build is identified
    /// by `version`, not re-resolved from `channel` at run time.
    #[serde(default = "default_channel")]
    pub channel: String,
    /// The exact pinned version, e.g. `"131.0.6778.204"`.
    pub version: String,
    /// The expected SHA-256 digest (lowercase hex, 64 characters) of the downloaded archive for
    /// this platform. A mismatch fails closed rather than launching an unverified binary.
    pub sha256: String,
}

fn default_channel() -> String {
    "stable".to_string()
}

/// `[remote_cdp]`: connects to a CDP endpoint already running elsewhere (a container, a lab
/// machine). No download, no version pin this crate can enforce — the operator running that
/// endpoint owns its version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCdpConfig {
    /// The CDP websocket URL to connect to, e.g.
    /// `"ws://127.0.0.1:9222/devtools/browser/<uuid>"`.
    pub ws_url: String,
}

/// The parsed and validated contents of `browser.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserToml {
    /// Provider ids to try, in order. Every entry must name a provider this file also
    /// configures (`"managed"` requires `[managed]`, `"remote-cdp"` requires `[remote_cdp]`).
    #[serde(default)]
    pub fallback_order: Vec<String>,
    /// The `[managed]` table, when configured.
    #[serde(default)]
    pub managed: Option<ManagedConfig>,
    /// The `[remote_cdp]` table, when configured.
    #[serde(default)]
    pub remote_cdp: Option<RemoteCdpConfig>,
}

impl BrowserToml {
    /// Parse and validate `browser.toml` source text.
    pub fn parse(source: &str) -> Result<Self> {
        let config = toml::from_str::<BrowserToml>(source)
            .map_err(|e| TmError::parse(format!("browser.toml: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    /// Structural validation beyond what serde already enforces: a non-empty fallback order,
    /// every named provider has a matching table, and a configured `sha256` is well-formed.
    pub fn validate(&self) -> Result<()> {
        if self.fallback_order.is_empty() {
            return Err(TmError::invariant(
                "browser.toml: fallback_order is empty; configure at least one provider",
            ));
        }
        for id in &self.fallback_order {
            match id.as_str() {
                "managed" => {
                    if self.managed.is_none() {
                        return Err(TmError::invariant(
                            "browser.toml: fallback_order names \"managed\" but no [managed] table is configured",
                        ));
                    }
                }
                "remote-cdp" => {
                    if self.remote_cdp.is_none() {
                        return Err(TmError::invariant(
                            "browser.toml: fallback_order names \"remote-cdp\" but no [remote_cdp] table is configured",
                        ));
                    }
                }
                other => {
                    return Err(TmError::invariant(format!(
                        "browser.toml: fallback_order names unknown provider {other:?} (known: \"managed\", \"remote-cdp\")"
                    )));
                }
            }
        }
        if let Some(managed) = &self.managed {
            if managed.version.trim().is_empty() {
                return Err(TmError::invariant(
                    "browser.toml: [managed].version must not be empty",
                ));
            }
            let digest = managed.sha256.trim();
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(TmError::invariant(format!(
                    "browser.toml: [managed].sha256 must be 64 lowercase hex characters, got {:?}",
                    managed.sha256
                )));
            }
        }
        if let Some(remote_cdp) = &self.remote_cdp {
            if remote_cdp.ws_url.trim().is_empty() {
                return Err(TmError::invariant(
                    "browser.toml: [remote_cdp].ws_url must not be empty",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn parses_a_managed_only_config() {
        let source = format!(
            r#"
fallback_order = ["managed"]

[managed]
version = "131.0.6778.204"
sha256 = "{VALID_SHA256}"
"#
        );
        let config = BrowserToml::parse(&source).expect("valid config parses");
        assert_eq!(config.fallback_order, vec!["managed".to_string()]);
        let managed = config.managed.expect("managed table present");
        assert_eq!(managed.channel, "stable");
        assert_eq!(managed.version, "131.0.6778.204");
        assert_eq!(managed.sha256, VALID_SHA256);
    }

    #[test]
    fn parses_managed_and_remote_cdp_with_a_fallback_order() {
        let source = format!(
            r#"
fallback_order = ["remote-cdp", "managed"]

[managed]
channel = "beta"
version = "132.0.0.1"
sha256 = "{VALID_SHA256}"

[remote_cdp]
ws_url = "ws://127.0.0.1:9222/devtools/browser/abc"
"#
        );
        let config = BrowserToml::parse(&source).expect("valid config parses");
        assert_eq!(
            config.fallback_order,
            vec!["remote-cdp".to_string(), "managed".to_string()]
        );
        assert_eq!(config.managed.unwrap().channel, "beta");
        assert_eq!(
            config.remote_cdp.unwrap().ws_url,
            "ws://127.0.0.1:9222/devtools/browser/abc"
        );
    }

    #[test]
    fn rejects_empty_fallback_order() {
        let err = BrowserToml::parse("fallback_order = []").unwrap_err();
        assert!(err.to_string().contains("fallback_order is empty"));
    }

    #[test]
    fn rejects_fallback_order_naming_an_unconfigured_provider() {
        let source = r#"fallback_order = ["managed"]"#;
        let err = BrowserToml::parse(source).unwrap_err();
        assert!(err.to_string().contains("no [managed] table"));
    }

    #[test]
    fn rejects_fallback_order_naming_an_unknown_provider() {
        let source = r#"fallback_order = ["docker"]"#;
        let err = BrowserToml::parse(source).unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }

    #[test]
    fn rejects_a_malformed_sha256() {
        let source = r#"
fallback_order = ["managed"]

[managed]
version = "131.0.6778.204"
sha256 = "not-hex"
"#;
        let err = BrowserToml::parse(source).unwrap_err();
        assert!(err.to_string().contains("sha256"));
    }

    #[test]
    fn rejects_an_empty_version() {
        let source = format!(
            r#"
fallback_order = ["managed"]

[managed]
version = ""
sha256 = "{VALID_SHA256}"
"#
        );
        let err = BrowserToml::parse(&source).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn rejects_an_empty_remote_cdp_ws_url() {
        let source = r#"
fallback_order = ["remote-cdp"]

[remote_cdp]
ws_url = ""
"#;
        let err = BrowserToml::parse(source).unwrap_err();
        assert!(err.to_string().contains("ws_url"));
    }

    #[test]
    fn rejects_malformed_toml() {
        let err = BrowserToml::parse("this is not valid toml {{{").unwrap_err();
        assert!(err.to_string().contains("parse"));
    }
}
