//! [`ManagedProvider`]: the default [`crate::provider::BrowserProvider`], per `SPEC.md` §19.1a.
//!
//! Downloads a pinned Chrome for Testing build into `~/.tm/browsers/<channel>-<version>/`,
//! verifies it against the `sha256` recorded in `browser.toml` (see [`crate::config`]),
//! launches it per session with a throwaway profile directory (reusing
//! [`crate::launch`]), and tears the process and profile back down on
//! [`BrowserProvider::release`]. The binary is downloaded once per machine and reused across
//! sessions — only the profile directory is per-session — per §19.1b.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tm_types::{IdSource, Result, TmError};
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;

use crate::config::ManagedConfig;
use crate::downloader::BrowserDownloader;
use crate::launch::{self, LaunchConfig};
use crate::provider::{BrowserCapabilities, BrowserEndpoint, BrowserProvider, SessionRequest};

/// The Chrome for Testing "known good versions with downloads" index.
const KNOWN_GOOD_VERSIONS_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json";

/// How long [`ManagedProvider::acquire`] waits for a freshly spawned browser to print its
/// DevTools websocket URL before giving up.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// The subset of the Chrome for Testing version index this crate reads.
#[derive(Debug, Deserialize)]
struct KnownGoodVersions {
    versions: Vec<VersionEntry>,
}

#[derive(Debug, Deserialize)]
struct VersionEntry {
    version: String,
    downloads: Downloads,
}

#[derive(Debug, Deserialize)]
struct Downloads {
    #[serde(default)]
    chrome: Vec<PlatformDownload>,
}

#[derive(Debug, Deserialize)]
struct PlatformDownload {
    platform: String,
    url: String,
}

/// This machine's Chrome for Testing platform string, e.g. `"mac-arm64"`, `"linux64"`.
///
/// # Errors
/// [`tm_types::TmError::invariant`] on a target this index has no build for.
fn current_platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("mac-arm64"),
        ("macos", "x86_64") => Ok("mac-x64"),
        ("linux", "x86_64") => Ok("linux64"),
        ("windows", "x86_64") => Ok("win64"),
        ("windows", "x86") => Ok("win32"),
        (os, arch) => Err(TmError::invariant(format!(
            "managed browser provider: no Chrome for Testing build for {os}/{arch}"
        ))),
    }
}

/// Where the extracted archive puts the executable, relative to the extraction root, for
/// `platform` (a Chrome for Testing platform string as returned by [`current_platform`]).
fn binary_relative_path(platform: &str) -> Result<PathBuf> {
    let path = match platform {
        "mac-arm64" | "mac-x64" => PathBuf::from(format!("chrome-{platform}"))
            .join("Google Chrome for Testing.app")
            .join("Contents")
            .join("MacOS")
            .join("Google Chrome for Testing"),
        "linux64" => PathBuf::from("chrome-linux64").join("chrome"),
        "win64" | "win32" => PathBuf::from(format!("chrome-{platform}")).join("chrome.exe"),
        other => {
            return Err(TmError::invariant(format!(
                "managed browser provider: unrecognized platform {other:?}"
            )));
        }
    };
    Ok(path)
}

/// `$HOME` on Unix, `%USERPROFILE%` on Windows.
fn home_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    std::env::var_os(var)
        .map(PathBuf::from)
        .ok_or_else(|| TmError::invariant(format!("managed browser provider: ${var} is not set")))
}

/// `~/.tm/browsers/<channel>-<version>/`.
fn install_dir(config: &ManagedConfig) -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".tm")
        .join("browsers")
        .join(format!("{}-{}", config.channel, config.version)))
}

/// Extract the zip archive at `archive_path` into `dest_dir`, which must already exist.
///
/// Shells out to a system archive tool rather than adding a zip-decoding dependency: `unzip` on
/// Unix (present on every macOS and virtually every Linux install), `tar` on Windows (bundled
/// since Windows 10 1803 and capable of reading zip archives).
///
/// # Errors
/// [`tm_types::TmError::Io`] when the tool is missing or exits non-zero.
async fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<()> {
    #[cfg(windows)]
    let (tool, args): (&str, Vec<String>) = (
        "tar",
        vec![
            "-xf".to_string(),
            archive_path.display().to_string(),
            "-C".to_string(),
            dest_dir.display().to_string(),
        ],
    );
    #[cfg(not(windows))]
    let (tool, args): (&str, Vec<String>) = (
        "unzip",
        vec![
            "-o".to_string(),
            "-q".to_string(),
            archive_path.display().to_string(),
            "-d".to_string(),
            dest_dir.display().to_string(),
        ],
    );

    let status = tokio::process::Command::new(tool)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .await
        .map_err(|e| {
            TmError::Io(format!(
                "failed to run `{tool}` to extract the browser archive: {e}"
            ))
        })?;
    if !status.success() {
        return Err(TmError::Io(format!(
            "`{tool}` exited with {status} extracting the browser archive"
        )));
    }
    Ok(())
}

/// Parse the `ws://...` DevTools URL out of a line of Chrome's startup stderr, e.g.
/// `DevTools listening on ws://127.0.0.1:9222/devtools/browser/<uuid>`.
fn parse_devtools_ws_url(line: &str) -> Option<String> {
    line.split_once("DevTools listening on ")
        .map(|(_, rest)| rest.trim().to_string())
        .filter(|url| url.starts_with("ws://") || url.starts_with("wss://"))
}

/// One browser process this provider has acquired and not yet released.
struct LiveProcess {
    process: tokio::process::Child,
    profile_dir: PathBuf,
}

/// The default [`BrowserProvider`]: a pinned, checksummed, per-machine-cached Chrome for
/// Testing build.
pub struct ManagedProvider {
    config: ManagedConfig,
    downloader: Arc<dyn BrowserDownloader>,
    ids: Arc<dyn IdSource>,
    live: Mutex<HashMap<String, LiveProcess>>,
}

impl ManagedProvider {
    /// Build a provider pinned to `config`, fetching the version index and archive through
    /// `downloader` and drawing profile/endpoint ids from `ids`.
    pub fn new(
        config: ManagedConfig,
        downloader: Arc<dyn BrowserDownloader>,
        ids: Arc<dyn IdSource>,
    ) -> Self {
        ManagedProvider {
            config,
            downloader,
            ids,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve the local binary path for the pinned version, downloading and verifying it
    /// first if this machine has not installed it yet.
    ///
    /// # Known gap
    /// Two sessions racing a first-use download on the same machine both observe
    /// `binary_path.is_file() == false`, both download, and both write the same
    /// `archive.download.zip` inside `install_dir`, corrupting one another mid-extract. §19.1a's
    /// "downloaded once per machine" wants a lock (or a download-to-unique-temp-path,
    /// atomic-rename-into-place scheme) here; this provider does not implement one yet. A
    /// caller that needs concurrent first-use safety should pre-warm with a single `acquire`
    /// (or a future `tm browser install`) before spawning concurrent sessions.
    async fn resolve_binary(&self) -> Result<PathBuf> {
        let platform = current_platform()?;
        let install_dir = install_dir(&self.config)?;
        let binary_path = install_dir.join(binary_relative_path(platform)?);

        if binary_path.is_file() {
            return Ok(binary_path);
        }

        tracing::info!(
            channel = %self.config.channel,
            version = %self.config.version,
            "downloading Chrome for Testing (first use on this machine)",
        );

        let index_bytes = self.downloader.fetch_bytes(KNOWN_GOOD_VERSIONS_URL).await?;
        let index: KnownGoodVersions = serde_json::from_slice(&index_bytes)
            .map_err(|e| TmError::parse(format!("Chrome for Testing version index: {e}")))?;
        let version_entry = index
            .versions
            .iter()
            .find(|v| v.version == self.config.version)
            .ok_or_else(|| {
                TmError::not_found("chrome-for-testing-version", self.config.version.clone())
            })?;
        let download = version_entry
            .downloads
            .chrome
            .iter()
            .find(|d| d.platform == platform)
            .ok_or_else(|| {
                TmError::not_found(
                    "chrome-for-testing-platform-build",
                    format!("{} for {platform}", self.config.version),
                )
            })?;

        let archive_bytes = self.downloader.fetch_bytes(&download.url).await?;
        verify_checksum(&archive_bytes, &self.config.sha256)?;

        std::fs::create_dir_all(&install_dir)
            .map_err(|e| TmError::Io(format!("creating {}: {e}", install_dir.display())))?;
        // Inside `install_dir` itself (not `with_extension`, which would mangle a dotted
        // version like `131.0.6778.204` by replacing its trailing component).
        let archive_path = install_dir.join("archive.download.zip");
        std::fs::write(&archive_path, &archive_bytes)
            .map_err(|e| TmError::Io(format!("writing {}: {e}", archive_path.display())))?;
        let extract_result = extract_archive(&archive_path, &install_dir).await;
        let _ = std::fs::remove_file(&archive_path);
        extract_result?;

        if !binary_path.is_file() {
            return Err(TmError::invariant(format!(
                "managed browser provider: expected {} to exist after extracting the archive, it does not",
                binary_path.display()
            )));
        }
        Ok(binary_path)
    }

    /// Spawn `binary_path` headless with a throwaway profile and wait for its DevTools
    /// websocket URL.
    async fn spawn(
        &self,
        binary_path: &Path,
        extra_args: Vec<String>,
    ) -> Result<(tokio::process::Child, PathBuf, String)> {
        let profile_dir = launch::ephemeral_profile_dir(self.ids.as_ref())?;
        let launch_config = LaunchConfig {
            binary_path: binary_path.to_path_buf(),
            profile_dir: profile_dir.clone(),
            extra_args,
        };
        let argv = launch::build_argv(&launch_config);

        let mut command = tokio::process::Command::new(binary_path);
        command
            .args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut process = command
            .spawn()
            .map_err(|e| TmError::Io(format!("failed to spawn browser process: {e}")))?;

        let stderr = process
            .stderr
            .take()
            .ok_or_else(|| TmError::invariant("piped browser stderr was not captured"))?;
        let mut lines = tokio::io::BufReader::new(stderr).lines();

        let ws_url = tokio::time::timeout(LAUNCH_TIMEOUT, async {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Some(url) = parse_devtools_ws_url(&line) {
                            return Some(url);
                        }
                    }
                    Ok(None) | Err(_) => return None,
                }
            }
        })
        .await
        .map_err(|_| {
            TmError::Provider(
                "timed out waiting for the browser to print its DevTools websocket URL".into(),
            )
        })?
        .ok_or_else(|| {
            TmError::Provider(
                "browser process exited before printing its DevTools websocket URL".into(),
            )
        })?;

        Ok((process, profile_dir, ws_url))
    }
}

/// Compare the SHA-256 digest of `bytes` against `expected_hex` (lowercase hex).
///
/// # Errors
/// [`tm_types::TmError::Provider`] on mismatch, naming both digests so a failure is
/// diagnosable without re-downloading.
fn verify_checksum(bytes: &[u8], expected_hex: &str) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex::encode(hasher.finalize());
    if actual != expected_hex.to_ascii_lowercase() {
        return Err(TmError::Provider(format!(
            "managed browser provider: checksum mismatch (expected {expected_hex}, got {actual})"
        )));
    }
    Ok(())
}

#[async_trait]
impl BrowserProvider for ManagedProvider {
    fn id(&self) -> &str {
        "managed"
    }

    fn capabilities(&self) -> BrowserCapabilities {
        BrowserCapabilities {
            headless: true,
            pinned_version: true,
            persistent_context: true,
            video_recording: false,
            proxy: false,
            stealth: false,
            max_concurrent: None,
        }
    }

    async fn acquire(&self, req: &SessionRequest) -> Result<BrowserEndpoint> {
        let binary_path = self.resolve_binary().await?;
        let (process, profile_dir, ws_url) = self
            .spawn(&binary_path, req.extra_launch_args.clone())
            .await?;

        let endpoint_id = format!("managed-{}", self.ids.random_hex(16));
        self.live.lock().await.insert(
            endpoint_id.clone(),
            LiveProcess {
                process,
                profile_dir,
            },
        );

        Ok(BrowserEndpoint {
            provider_id: self.id().to_string(),
            endpoint_id,
            ws_url,
            browser_version: Some(self.config.version.clone()),
        })
    }

    async fn release(&self, endpoint: &BrowserEndpoint) -> Result<()> {
        let mut live = self.live.lock().await;
        let Some(mut entry) = live.remove(&endpoint.endpoint_id) else {
            return Err(TmError::not_found(
                "browser-endpoint",
                &endpoint.endpoint_id,
            ));
        };
        drop(live);

        if let Err(e) = entry.process.kill().await {
            tracing::warn!(error = %e, "failed to kill the browser process");
        }
        if let Err(e) = entry.process.wait().await {
            tracing::warn!(error = %e, "failed waiting for the browser process to exit");
        }
        if let Err(e) = std::fs::remove_dir_all(&entry.profile_dir) {
            tracing::warn!(
                error = %e,
                path = %entry.profile_dir.display(),
                "failed to remove the browser profile directory",
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tm_types::TestIds;

    /// An in-memory [`BrowserDownloader`] fixture keyed by URL, so tests never construct a real
    /// network client.
    struct FixtureDownloader {
        responses: HashMap<String, Vec<u8>>,
        calls: AtomicUsize,
    }

    impl FixtureDownloader {
        fn new(responses: Vec<(&str, Vec<u8>)>) -> Self {
            FixtureDownloader {
                responses: responses
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl BrowserDownloader for FixtureDownloader {
        async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| TmError::not_found("fixture-url", url))
        }
    }

    fn index_json(version: &str, platform: &str, url: &str) -> Vec<u8> {
        serde_json::json!({
            "versions": [
                {
                    "version": version,
                    "downloads": {
                        "chrome": [
                            { "platform": platform, "url": url }
                        ]
                    }
                }
            ]
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn current_platform_resolves_on_supported_targets() {
        // This test runs on whatever CI/dev platform executes it; it only asserts the mapping
        // does not error on a target this workspace actually ships for, and that mac/linux/
        // windows all resolve to a platform string when reached directly.
        assert_eq!(
            current_platform().is_ok(),
            matches!(
                (std::env::consts::OS, std::env::consts::ARCH),
                ("macos", "aarch64")
                    | ("macos", "x86_64")
                    | ("linux", "x86_64")
                    | ("windows", "x86_64")
                    | ("windows", "x86")
            )
        );
    }

    #[test]
    fn binary_relative_path_is_known_for_every_supported_platform() {
        for platform in ["mac-arm64", "mac-x64", "linux64", "win64", "win32"] {
            assert!(binary_relative_path(platform).is_ok(), "{platform}");
        }
        assert!(binary_relative_path("plan9-risc").is_err());
    }

    #[test]
    fn verify_checksum_accepts_a_matching_digest() {
        let bytes = b"chrome archive bytes";
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hex::encode(hasher.finalize());
        assert!(verify_checksum(bytes, &digest).is_ok());
    }

    #[test]
    fn verify_checksum_rejects_a_mismatched_digest() {
        let bytes = b"chrome archive bytes";
        let wrong = "0".repeat(64);
        let err = verify_checksum(bytes, &wrong).unwrap_err();
        assert!(matches!(err, TmError::Provider(_)));
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn parses_devtools_ws_url_out_of_a_startup_line() {
        let line = "DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc-123";
        assert_eq!(
            parse_devtools_ws_url(line).as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc-123")
        );
    }

    #[test]
    fn ignores_unrelated_stderr_lines() {
        assert_eq!(
            parse_devtools_ws_url("[1234:5678] some other log line"),
            None
        );
    }

    /// The end-to-end failure path required by this crate's test suite: a downloaded archive
    /// whose bytes do not match `browser.toml`'s pinned `sha256` must be rejected before
    /// anything is written to disk or extracted — never silently accepted as "close enough".
    #[tokio::test]
    async fn resolve_binary_fails_closed_on_a_checksum_mismatch() {
        let platform = current_platform().expect("test runs on a supported platform");
        let archive_url = "https://example.invalid/chrome-for-testing/archive.zip";
        let downloader = Arc::new(FixtureDownloader::new(vec![
            (
                KNOWN_GOOD_VERSIONS_URL,
                index_json("131.0.6778.204", platform, archive_url),
            ),
            (archive_url, b"not actually a chrome archive".to_vec()),
        ]));
        let config = ManagedConfig {
            channel: "stable".to_string(),
            version: "131.0.6778.204".to_string(),
            // A well-formed but wrong digest, so this exercises the mismatch branch rather than
            // the "malformed sha256" branch ([`crate::config`] already covers that).
            sha256: "0".repeat(64),
        };
        let ids: Arc<dyn IdSource> = Arc::new(TestIds::seeded(1));
        let provider = ManagedProvider::new(config, downloader.clone(), ids);

        let err = provider.resolve_binary().await.unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        // Both the index and the archive were fetched — the mismatch is caught after download,
        // not by skipping the download.
        assert_eq!(downloader.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn resolve_binary_reports_not_found_for_an_unlisted_version() {
        let platform = current_platform().expect("test runs on a supported platform");
        let downloader = Arc::new(FixtureDownloader::new(vec![(
            KNOWN_GOOD_VERSIONS_URL,
            index_json("999.0.0.0", platform, "https://example.invalid/x.zip"),
        )]));
        let config = ManagedConfig {
            channel: "stable".to_string(),
            version: "131.0.6778.204".to_string(),
            sha256: "0".repeat(64),
        };
        let ids: Arc<dyn IdSource> = Arc::new(TestIds::seeded(1));
        let provider = ManagedProvider::new(config, downloader, ids);

        let err = provider.resolve_binary().await.unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn capabilities_declare_a_pinned_local_headless_provider() {
        let downloader: Arc<dyn BrowserDownloader> = Arc::new(FixtureDownloader::new(vec![]));
        let ids: Arc<dyn IdSource> = Arc::new(TestIds::seeded(1));
        let provider = ManagedProvider::new(
            ManagedConfig {
                channel: "stable".to_string(),
                version: "131.0.6778.204".to_string(),
                sha256: "0".repeat(64),
            },
            downloader,
            ids,
        );
        let caps = provider.capabilities();
        assert!(caps.headless);
        assert!(caps.pinned_version);
        assert!(caps.persistent_context);
        assert!(!caps.video_recording);
    }

    #[tokio::test]
    async fn release_reports_not_found_for_an_unknown_endpoint() {
        let downloader: Arc<dyn BrowserDownloader> = Arc::new(FixtureDownloader::new(vec![]));
        let ids: Arc<dyn IdSource> = Arc::new(TestIds::seeded(1));
        let provider = ManagedProvider::new(
            ManagedConfig {
                channel: "stable".to_string(),
                version: "131.0.6778.204".to_string(),
                sha256: "0".repeat(64),
            },
            downloader,
            ids,
        );
        let ghost = BrowserEndpoint {
            provider_id: "managed".to_string(),
            endpoint_id: "does-not-exist".to_string(),
            ws_url: "ws://127.0.0.1:0/x".to_string(),
            browser_version: None,
        };
        let err = provider.release(&ghost).await.unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }
}
