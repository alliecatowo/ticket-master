//! [`BrowserDownloader`]: the network seam [`crate::managed::ManagedProvider`] fetches the
//! Chrome for Testing version index and archive bytes through.
//!
//! Kept as a trait, mirroring [`crate::session::ArtifactSink`] ("a fake in-memory implementation
//! stands in for it in unit tests"), so the checksum-verification-failure path can be tested
//! against a fixture that returns deliberately wrong bytes without this crate's tests ever
//! constructing a real network client — `xtask`'s hygiene check forbids that.

use async_trait::async_trait;
use tm_types::{Result, TmError};

/// Fetches bytes over HTTP(S). The only network dependency [`crate::managed::ManagedProvider`]
/// has; production code uses [`ReqwestDownloader`], tests use an in-memory fixture.
#[async_trait]
pub trait BrowserDownloader: Send + Sync {
    /// `GET url` and return the response body as bytes.
    ///
    /// # Errors
    /// [`tm_types::TmError::Provider`] on a transport failure or a non-2xx response.
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>>;
}

/// The real [`BrowserDownloader`], backed by `reqwest`.
#[derive(Debug, Clone, Default)]
pub struct ReqwestDownloader {
    client: reqwest::Client,
}

impl ReqwestDownloader {
    /// Build a downloader with a fresh `reqwest` client.
    pub fn new() -> Self {
        ReqwestDownloader {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl BrowserDownloader for ReqwestDownloader {
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| TmError::Provider(format!("GET {url} failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(TmError::Provider(format!(
                "GET {url} returned HTTP {status}"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| TmError::Provider(format!("reading response body from {url}: {e}")))?;
        Ok(bytes.to_vec())
    }
}
