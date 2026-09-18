//! [`RemoteCdpProvider`]: connects to a CDP endpoint you already run — a container, a lab
//! machine, a self-hosted fleet — instead of acquiring a browser process locally. Per
//! `SPEC.md` §19.1a: "Self-hosted fleets, air-gapped networks".
//!
//! This provider owns no process and downloads nothing, so it is intentionally the smallest one
//! in this crate: `acquire` hands back the configured websocket URL, `release` is a no-op
//! because the operator running that endpoint owns its lifecycle, not this session.

use async_trait::async_trait;
use tm_types::Result;

use crate::provider::{BrowserCapabilities, BrowserEndpoint, BrowserProvider, SessionRequest};

/// A [`BrowserProvider`] backed by an already-running CDP endpoint.
#[derive(Debug, Clone)]
pub struct RemoteCdpProvider {
    ws_url: String,
}

impl RemoteCdpProvider {
    /// Build a provider that always hands back `ws_url`.
    pub fn new(ws_url: impl Into<String>) -> Self {
        RemoteCdpProvider {
            ws_url: ws_url.into(),
        }
    }
}

#[async_trait]
impl BrowserProvider for RemoteCdpProvider {
    fn id(&self) -> &str {
        "remote-cdp"
    }

    fn capabilities(&self) -> BrowserCapabilities {
        BrowserCapabilities {
            headless: true,
            // This provider does not control what build is listening at `ws_url`, so it can
            // never *guarantee* a pin the way `managed` can — a request that requires
            // `pinned_version` must not be routed here.
            pinned_version: false,
            persistent_context: true,
            video_recording: false,
            proxy: false,
            stealth: false,
            max_concurrent: None,
        }
    }

    async fn acquire(&self, _req: &SessionRequest) -> Result<BrowserEndpoint> {
        Ok(BrowserEndpoint {
            provider_id: self.id().to_string(),
            endpoint_id: self.ws_url.clone(),
            ws_url: self.ws_url.clone(),
            browser_version: None,
        })
    }

    async fn release(&self, _endpoint: &BrowserEndpoint) -> Result<()> {
        // Nothing to tear down: the endpoint's process and profile belong to whatever operator
        // stood it up, not to this session.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_no_pinned_version_capability() {
        let provider = RemoteCdpProvider::new("ws://127.0.0.1:9222/devtools/browser/x");
        assert!(!provider.capabilities().pinned_version);
        assert!(provider.capabilities().headless);
    }

    #[tokio::test]
    async fn acquire_returns_the_configured_ws_url() {
        let provider = RemoteCdpProvider::new("ws://127.0.0.1:9222/devtools/browser/x");
        let endpoint = provider.acquire(&SessionRequest::default()).await.unwrap();
        assert_eq!(endpoint.provider_id, "remote-cdp");
        assert_eq!(endpoint.ws_url, "ws://127.0.0.1:9222/devtools/browser/x");
        assert_eq!(endpoint.browser_version, None);
    }

    #[tokio::test]
    async fn release_is_a_no_op() {
        let provider = RemoteCdpProvider::new("ws://127.0.0.1:9222/devtools/browser/x");
        let endpoint = provider.acquire(&SessionRequest::default()).await.unwrap();
        assert!(provider.release(&endpoint).await.is_ok());
    }
}
