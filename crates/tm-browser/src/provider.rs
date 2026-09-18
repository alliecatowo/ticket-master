//! [`BrowserProvider`]: the plural acquisition trait behind `browser.toml`, per `SPEC.md`
//! §19.1a. Browser acquisition is an integration detail, exactly like task trackers (§13.1), so
//! it lives behind one trait with several implementations rather than one hardcoded path.
//!
//! [`ProviderRegistry`] is the fallback-order selector: it tries each configured provider in
//! order, skipping one whose declared [`BrowserCapabilities`] cannot satisfy a
//! [`SessionRequest`], and reports [`tm_types::TmError::Provider`] only once every configured
//! provider has been tried. Skipping to the *next configured provider* that can honour a
//! request is legitimate fallback; no provider here is ever allowed to silently substitute a
//! different browser build for a pinned one within itself — that is exactly the failure mode
//! §19.1a calls out ("it passed on some other browser is not evidence").

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_types::{IdSource, Result, TmError};

use crate::config::BrowserToml;
use crate::downloader::BrowserDownloader;
use crate::managed::ManagedProvider;
use crate::remote_cdp::RemoteCdpProvider;

/// What a [`BrowserProvider`] declares it can do. Declared, not assumed: [`ProviderRegistry`]
/// refuses to route a [`SessionRequest`] to a provider whose capabilities do not cover it,
/// instead of trying and degrading quietly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserCapabilities {
    /// Can launch/connect without a visible window.
    pub headless: bool,
    /// Can honour an exact, project-pinned browser version rather than "whatever is current".
    pub pinned_version: bool,
    /// Can keep a session's cookies/storage alive across more than one navigation within that
    /// session (every provider here does; declared explicitly since a future provider might
    /// hand back a fresh context per navigation).
    pub persistent_context: bool,
    /// Can record video of a session.
    pub video_recording: bool,
    /// Can route a session's traffic through a configured proxy.
    pub proxy: bool,
    /// Can reduce automation fingerprinting.
    pub stealth: bool,
    /// The most sessions this provider can serve at once, when bounded. `None` means the
    /// provider itself imposes no bound (concurrency is still bounded by
    /// `Authority.resources` per §19.1b regardless).
    pub max_concurrent: Option<u32>,
}

/// What one session needs from whichever provider serves it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionRequest {
    /// Capabilities this session cannot do without. Every `true`/`Some` field here must be
    /// matched by the chosen provider's declared [`BrowserCapabilities`]; unset fields are not
    /// required.
    pub required: BrowserCapabilities,
    /// Extra Chromium flags beyond a provider's fixed headless/port/profile set, e.g.
    /// `--disable-gpu` on some Linux CI images. Providers that do not launch a local process
    /// (e.g. `remote-cdp`) ignore this.
    pub extra_launch_args: Vec<String>,
}

/// A live, connectable browser: a CDP websocket URL plus enough bookkeeping for the issuing
/// provider to tear it back down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserEndpoint {
    /// [`BrowserProvider::id`] of the provider that issued this endpoint.
    pub provider_id: String,
    /// Opaque id the issuing provider uses to look this endpoint back up in
    /// [`BrowserProvider::release`]. Not meaningful to any other provider.
    pub endpoint_id: String,
    /// The CDP websocket URL to connect to.
    pub ws_url: String,
    /// The browser's version string, when the provider can report one (the `managed` provider
    /// always can, since it downloaded a named version; `remote-cdp` may not).
    pub browser_version: Option<String>,
}

/// A backend that can hand out a CDP-speaking browser session. Implemented by
/// [`crate::managed::ManagedProvider`] (the default: downloads a pinned Chrome for Testing
/// build) and [`crate::remote_cdp::RemoteCdpProvider`] (connects to a CDP endpoint you already
/// run). See `SPEC.md` §19.1a for the full provider list this trait is designed to admit later
/// (`docker`, `browserbase`, `steel`, `browserless`, `hyperbrowser`) — none of those are
/// implemented here, since they need external service credentials and infrastructure this
/// workspace cannot provision or test; the trait and [`ProviderRegistry`]'s string-keyed lookup
/// are the seam a later change adds them through.
#[async_trait]
pub trait BrowserProvider: Send + Sync {
    /// The slug this provider registers under in `browser.toml`, e.g. `"managed"`.
    fn id(&self) -> &str;

    /// What this provider can do. Consulted by [`ProviderRegistry::acquire`] before it is ever
    /// asked to actually acquire a session.
    fn capabilities(&self) -> BrowserCapabilities;

    /// Acquire a live, isolated browser session satisfying `req`.
    ///
    /// # Errors
    /// [`tm_types::TmError::Provider`] naming the unsatisfiable capability when `req` needs
    /// something this provider's [`BrowserCapabilities`] does not declare (callers should check
    /// [`BrowserProvider::capabilities`] first — [`ProviderRegistry`] always does — so reaching
    /// this is a caller bug, not routing); other [`tm_types::TmError`] variants for acquisition
    /// failures proper (download, checksum, spawn, connect).
    async fn acquire(&self, req: &SessionRequest) -> Result<BrowserEndpoint>;

    /// Release a session this provider previously acquired: tear down whatever process/context
    /// it owns and delete its throwaway profile, per §19.1b ("the session dies with the
    /// lease"). Best-effort from the caller's point of view — a provider should still make a
    /// reasonable attempt to reclaim resources even when a step fails, and report the first
    /// failure rather than silently swallowing it.
    async fn release(&self, endpoint: &BrowserEndpoint) -> Result<()>;
}

/// `true` when `have` covers every capability `required` asks for.
fn satisfies(have: BrowserCapabilities, required: BrowserCapabilities) -> bool {
    (!required.headless || have.headless)
        && (!required.pinned_version || have.pinned_version)
        && (!required.persistent_context || have.persistent_context)
        && (!required.video_recording || have.video_recording)
        && (!required.proxy || have.proxy)
        && (!required.stealth || have.stealth)
        && required
            .max_concurrent
            .is_none_or(|need| have.max_concurrent.is_none_or(|has| has >= need))
}

/// The provider registry: every configured [`BrowserProvider`], keyed by id, plus the fallback
/// order `browser.toml` selected them in.
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn BrowserProvider>>,
    fallback_order: Vec<String>,
}

impl ProviderRegistry {
    /// Build a registry from already-constructed providers and an explicit fallback order.
    ///
    /// # Errors
    /// [`tm_types::TmError::invariant`] when `fallback_order` names a provider id that is not
    /// in `providers`, or is empty.
    pub fn new(
        providers: Vec<Arc<dyn BrowserProvider>>,
        fallback_order: Vec<String>,
    ) -> Result<Self> {
        if fallback_order.is_empty() {
            return Err(TmError::invariant(
                "browser provider registry: fallback_order is empty",
            ));
        }
        let providers: BTreeMap<String, Arc<dyn BrowserProvider>> = providers
            .into_iter()
            .map(|p| (p.id().to_string(), p))
            .collect();
        for id in &fallback_order {
            if !providers.contains_key(id) {
                return Err(TmError::invariant(format!(
                    "browser provider registry: fallback_order names {id:?}, which was not registered"
                )));
            }
        }
        Ok(ProviderRegistry {
            providers,
            fallback_order,
        })
    }

    /// The fallback order this registry was built with.
    pub fn fallback_order(&self) -> &[String] {
        &self.fallback_order
    }

    /// Look up a registered provider by id.
    pub fn get(&self, id: &str) -> Option<&Arc<dyn BrowserProvider>> {
        self.providers.get(id)
    }

    /// Build every provider `config` names and wire them into a registry with `config`'s
    /// fallback order, per `SPEC.md` §19.1a. `downloader` and `ids` are shared by every
    /// [`crate::managed::ManagedProvider`] this constructs (there is at most one, since
    /// `browser.toml` has a single `[managed]` table).
    pub fn from_config(
        config: &BrowserToml,
        downloader: Arc<dyn BrowserDownloader>,
        ids: Arc<dyn IdSource>,
    ) -> Result<Self> {
        let mut providers: Vec<Arc<dyn BrowserProvider>> = Vec::new();
        if let Some(managed) = &config.managed {
            providers.push(Arc::new(ManagedProvider::new(
                managed.clone(),
                Arc::clone(&downloader),
                Arc::clone(&ids),
            )));
        }
        if let Some(remote_cdp) = &config.remote_cdp {
            providers.push(Arc::new(RemoteCdpProvider::new(remote_cdp.ws_url.clone())));
        }
        ProviderRegistry::new(providers, config.fallback_order.clone())
    }

    /// Walk the fallback order, skipping any provider whose declared capabilities cannot
    /// satisfy `req.required`, and return the first successful acquisition.
    ///
    /// # Errors
    /// [`tm_types::TmError::Provider`] once every configured provider has either failed the
    /// capability check or failed to acquire, naming the last reason. Trying the next
    /// configured provider after one fails is the only fallback this performs — no provider is
    /// ever asked to substitute a different build than the one it was configured for.
    pub async fn acquire(
        &self,
        req: &SessionRequest,
    ) -> Result<(Arc<dyn BrowserProvider>, BrowserEndpoint)> {
        let mut last_err: Option<TmError> = None;
        for id in &self.fallback_order {
            let provider = self
                .providers
                .get(id)
                .expect("fallback_order was validated against providers in ProviderRegistry::new");
            if !satisfies(provider.capabilities(), req.required) {
                last_err = Some(TmError::Provider(format!(
                    "browser provider {id:?} does not satisfy the requested capabilities"
                )));
                continue;
            }
            match provider.acquire(req).await {
                Ok(endpoint) => return Ok((Arc::clone(provider), endpoint)),
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            TmError::Provider("no browser provider configured in fallback_order".to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedProvider {
        provider_id: &'static str,
        caps: BrowserCapabilities,
        acquire_calls: AtomicUsize,
        release_calls: AtomicUsize,
        fail_acquire: bool,
    }

    impl FixedProvider {
        fn new(id: &'static str, caps: BrowserCapabilities) -> Self {
            FixedProvider {
                provider_id: id,
                caps,
                acquire_calls: AtomicUsize::new(0),
                release_calls: AtomicUsize::new(0),
                fail_acquire: false,
            }
        }

        fn failing(mut self) -> Self {
            self.fail_acquire = true;
            self
        }
    }

    #[async_trait]
    impl BrowserProvider for FixedProvider {
        fn id(&self) -> &str {
            self.provider_id
        }

        fn capabilities(&self) -> BrowserCapabilities {
            self.caps
        }

        async fn acquire(&self, _req: &SessionRequest) -> Result<BrowserEndpoint> {
            self.acquire_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_acquire {
                return Err(TmError::Provider(format!("{} refused", self.provider_id)));
            }
            Ok(BrowserEndpoint {
                provider_id: self.provider_id.to_string(),
                endpoint_id: "E-1".to_string(),
                ws_url: "ws://127.0.0.1:0/devtools/browser/fixed".to_string(),
                browser_version: Some("131.0.0.0".to_string()),
            })
        }

        async fn release(&self, _endpoint: &BrowserEndpoint) -> Result<()> {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Unwrap the `Err` side of a `Result` whose `Ok` side is not `Debug` (e.g. it holds a
    /// `dyn BrowserProvider`), so tests can assert on the error without `.unwrap_err()`'s
    /// `T: Debug` bound.
    fn expect_err<T>(result: Result<T>) -> TmError {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    fn caps(headless: bool, pinned_version: bool) -> BrowserCapabilities {
        BrowserCapabilities {
            headless,
            pinned_version,
            persistent_context: true,
            ..Default::default()
        }
    }

    #[test]
    fn satisfies_requires_every_flag_the_request_sets() {
        let have = caps(true, false);
        assert!(satisfies(have, BrowserCapabilities::default()));
        assert!(satisfies(
            have,
            BrowserCapabilities {
                headless: true,
                ..Default::default()
            }
        ));
        assert!(!satisfies(
            have,
            BrowserCapabilities {
                pinned_version: true,
                ..Default::default()
            }
        ));
    }

    #[test]
    fn satisfies_checks_max_concurrent_as_a_minimum() {
        let have = BrowserCapabilities {
            max_concurrent: Some(4),
            ..Default::default()
        };
        let need_two = BrowserCapabilities {
            max_concurrent: Some(2),
            ..Default::default()
        };
        let need_ten = BrowserCapabilities {
            max_concurrent: Some(10),
            ..Default::default()
        };
        assert!(satisfies(have, need_two));
        assert!(!satisfies(have, need_ten));
    }

    #[tokio::test]
    async fn acquire_uses_the_first_provider_in_fallback_order() {
        let a = Arc::new(FixedProvider::new("a", caps(true, true)));
        let b = Arc::new(FixedProvider::new("b", caps(true, true)));
        let registry = ProviderRegistry::new(
            vec![a.clone(), b.clone()],
            vec!["a".to_string(), "b".to_string()],
        )
        .unwrap();

        let (provider, endpoint) = registry.acquire(&SessionRequest::default()).await.unwrap();

        assert_eq!(provider.id(), "a");
        assert_eq!(endpoint.provider_id, "a");
        assert_eq!(a.acquire_calls.load(Ordering::SeqCst), 1);
        assert_eq!(b.acquire_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn acquire_skips_a_provider_that_cannot_satisfy_capabilities() {
        let cannot_pin = Arc::new(FixedProvider::new("remote-cdp", caps(true, false)));
        let can_pin = Arc::new(FixedProvider::new("managed", caps(true, true)));
        let registry = ProviderRegistry::new(
            vec![cannot_pin.clone(), can_pin.clone()],
            vec!["remote-cdp".to_string(), "managed".to_string()],
        )
        .unwrap();

        let req = SessionRequest {
            required: caps(true, true),
            extra_launch_args: vec![],
        };
        let (provider, _endpoint) = registry.acquire(&req).await.unwrap();

        assert_eq!(provider.id(), "managed");
        // The capability check short-circuits before ever calling the unsuitable provider.
        assert_eq!(cannot_pin.acquire_calls.load(Ordering::SeqCst), 0);
        assert_eq!(can_pin.acquire_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn acquire_falls_back_to_the_next_provider_when_one_fails() {
        let flaky = Arc::new(FixedProvider::new("flaky", caps(true, true)).failing());
        let steady = Arc::new(FixedProvider::new("steady", caps(true, true)));
        let registry = ProviderRegistry::new(
            vec![flaky.clone(), steady.clone()],
            vec!["flaky".to_string(), "steady".to_string()],
        )
        .unwrap();

        let (provider, _endpoint) = registry.acquire(&SessionRequest::default()).await.unwrap();

        assert_eq!(provider.id(), "steady");
        assert_eq!(flaky.acquire_calls.load(Ordering::SeqCst), 1);
        assert_eq!(steady.acquire_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn acquire_reports_capability_mismatch_once_every_provider_is_exhausted() {
        let a = Arc::new(FixedProvider::new("a", caps(true, false)));
        let registry = ProviderRegistry::new(vec![a], vec!["a".to_string()]).unwrap();

        let req = SessionRequest {
            required: caps(true, true),
            extra_launch_args: vec![],
        };
        let err = expect_err(registry.acquire(&req).await);
        assert!(matches!(err, TmError::Provider(_)));
        assert!(err.to_string().contains("capabilities"));
    }

    #[test]
    fn new_rejects_an_empty_fallback_order() {
        let err = expect_err(ProviderRegistry::new(vec![], vec![]));
        assert!(err.to_string().contains("fallback_order"));
    }

    #[test]
    fn new_rejects_a_fallback_order_naming_an_unregistered_provider() {
        let a = Arc::new(FixedProvider::new("a", caps(true, true)));
        let err = expect_err(ProviderRegistry::new(vec![a], vec!["ghost".to_string()]));
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn from_config_builds_both_providers_from_browser_toml() {
        use crate::config::{ManagedConfig, RemoteCdpConfig};
        use crate::downloader::BrowserDownloader;
        use tm_types::TestIds;

        struct NoopDownloader;
        #[async_trait]
        impl BrowserDownloader for NoopDownloader {
            async fn fetch_bytes(&self, _url: &str) -> Result<Vec<u8>> {
                Err(TmError::invariant("not used by this test"))
            }
        }

        let config = crate::config::BrowserToml {
            fallback_order: vec!["remote-cdp".to_string(), "managed".to_string()],
            managed: Some(ManagedConfig {
                channel: "stable".to_string(),
                version: "131.0.6778.204".to_string(),
                sha256: "0".repeat(64),
            }),
            remote_cdp: Some(RemoteCdpConfig {
                ws_url: "ws://127.0.0.1:9222/devtools/browser/x".to_string(),
            }),
        };
        let downloader: Arc<dyn BrowserDownloader> = Arc::new(NoopDownloader);
        let ids: Arc<dyn tm_types::IdSource> = Arc::new(TestIds::seeded(1));

        let registry = ProviderRegistry::from_config(&config, downloader, ids).unwrap();
        assert!(registry.get("managed").is_some());
        assert!(registry.get("remote-cdp").is_some());
        assert_eq!(
            registry.fallback_order(),
            &["remote-cdp".to_string(), "managed".to_string()]
        );
    }
}
