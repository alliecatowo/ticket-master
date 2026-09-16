//! Locate a usable system Chromium-family browser and build its launch argv.
//!
//! We never download a browser. We probe, in order, `$TM_BROWSER`, then Chrome, Chromium,
//! Brave and Edge at their platform-conventional install locations; if none exists we fail
//! with [`install_hint`], the exact command to install one, rather than reaching for the
//! network (`SPEC.md` §19.1). Launch config is always headless (`--headless=new`), a random
//! debugging port (`--remote-debugging-port=0`, discovered after launch — see
//! `session.rs`), and a throwaway profile directory so sessions never collide and never touch
//! the user's real Chrome profile.

use std::path::PathBuf;

use tm_types::{IdSource, Result};

/// Which Chromium-family browser was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserKind {
    /// Google Chrome.
    Chrome,
    /// Open-source Chromium.
    Chromium,
    /// Brave.
    Brave,
    /// Microsoft Edge.
    Edge,
}

impl BrowserKind {
    /// All kinds, in the order they are probed.
    pub const ALL: [BrowserKind; 4] = [
        BrowserKind::Chrome,
        BrowserKind::Chromium,
        BrowserKind::Brave,
        BrowserKind::Edge,
    ];

    /// The human-facing name, used in error messages and traces.
    pub fn display_name(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Google Chrome",
            BrowserKind::Chromium => "Chromium",
            BrowserKind::Brave => "Brave",
            BrowserKind::Edge => "Microsoft Edge",
        }
    }
}

/// A browser found on this machine, ready to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredBrowser {
    /// Which browser family this is.
    pub kind: BrowserKind,
    /// Absolute path to the executable.
    pub path: PathBuf,
    /// `true` when the path came from `$TM_BROWSER` rather than a platform default location.
    pub from_env_override: bool,
}

/// Probe `$TM_BROWSER` and then the platform default locations, in [`BrowserKind::ALL`] order,
/// for the first executable that exists.
///
/// # Errors
/// [`TmError::NotFound`] with [`install_hint`] appended to the message when nothing is found,
/// and when `$TM_BROWSER` is set but does not point at an existing file.
// IMPL: read `TM_BROWSER` via `std::env::var_os`; if set, `Path::new(..).is_file()` it and
// return it as `from_env_override: true` on success, or a `NotFound` error naming the bad path
// on failure (never silently fall through an explicit override). Otherwise iterate
// `BrowserKind::ALL`, call `candidate_paths(kind)` for each, and return the first path that
// `is_file()`. Do not check executability beyond existence here; a non-executable match still
// surfaces as a clear launch failure later rather than a confusing discovery failure.
pub fn discover() -> Result<DiscoveredBrowser> {
    todo!("probe $TM_BROWSER then platform candidate paths for BrowserKind::ALL")
}

/// The platform-conventional install locations to probe for `kind`, most-likely-first.
// IMPL: cfg-dispatch to `macos_candidate_paths` / `linux_candidate_paths` /
// `windows_candidate_paths`; each returns absolute paths only (no PATH search — a bare `chrome`
// on `$PATH` is covered by `$TM_BROWSER` if a user wants that). Never touches the filesystem
// itself; `discover` does the `is_file()` check so this stays a pure, testable mapping.
pub fn candidate_paths(kind: BrowserKind) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos_candidate_paths(kind)
    }
    #[cfg(target_os = "linux")]
    {
        linux_candidate_paths(kind)
    }
    #[cfg(target_os = "windows")]
    {
        windows_candidate_paths(kind)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = kind;
        Vec::new()
    }
}

/// macOS `/Applications` bundle paths for `kind`.
// IMPL: `/Applications/<App>.app/Contents/MacOS/<Binary>` for each of the four kinds, plus the
// `~/Applications` per-user variant (via `$HOME`, never a directories crate — this module has
// no such dependency).
#[cfg(target_os = "macos")]
fn macos_candidate_paths(kind: BrowserKind) -> Vec<PathBuf> {
    let _ = kind;
    todo!("Contents/MacOS/<Binary> under /Applications and ~/Applications for this kind")
}

/// Linux `/usr/bin`, `/usr/local/bin` and `/opt` conventional binary names for `kind`.
// IMPL: well-known binary names per kind (`google-chrome`, `google-chrome-stable`, `chromium`,
// `chromium-browser`, `brave-browser`, `microsoft-edge`, `microsoft-edge-stable`) under
// `/usr/bin`, `/usr/local/bin`, `/snap/bin` and `/opt/<vendor>/`.
#[cfg(target_os = "linux")]
fn linux_candidate_paths(kind: BrowserKind) -> Vec<PathBuf> {
    let _ = kind;
    todo!("well-known binary names for this kind under /usr/bin, /usr/local/bin, /opt")
}

/// Windows `Program Files` locations for `kind`, plus an App Paths registry lookup.
// IMPL: `%ProgramFiles%`, `%ProgramFiles(x86)%` and `%LocalAppData%` conventional install
// subpaths per kind, followed by a best-effort read of
// `HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\<exe>` via `winreg`
// (swallow registry errors — it is a bonus probe, not a required one).
#[cfg(target_os = "windows")]
fn windows_candidate_paths(kind: BrowserKind) -> Vec<PathBuf> {
    let _ = kind;
    todo!("Program Files locations per kind, then an App Paths registry probe via winreg")
}

/// The exact command to install a Chromium-family browser on this platform, used to make
/// [`discover`]'s failure actionable instead of a dead end.
// IMPL: cfg-dispatch on `target_os`: macOS -> `brew install --cask google-chrome`, Linux ->
// a distro-agnostic-ish `sudo apt install chromium-browser` (with a note that other package
// managers use an equivalent), Windows -> `winget install Google.Chrome`. Falls back to a
// generic "install Google Chrome, Chromium, Brave or Microsoft Edge" message on other targets.
pub fn install_hint() -> &'static str {
    todo!("the platform-specific install command, or a generic fallback")
}

/// Launch-time configuration for a discovered browser: headless, a throwaway profile, and a
/// random debugging port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchConfig {
    /// The browser to launch.
    pub browser: DiscoveredBrowser,
    /// The ephemeral profile directory (`--user-data-dir`). Never the user's real profile.
    pub profile_dir: PathBuf,
    /// Extra CDP-relevant flags beyond the fixed headless/port/profile set, e.g.
    /// `--disable-gpu` on some Linux CI images.
    pub extra_args: Vec<String>,
}

/// Allocate a fresh, empty, process-unique profile directory under the system temp dir.
///
/// # Invariants
/// Never returns the same path twice for the same `ids` source, and never collides with a
/// concurrently running session's directory.
// IMPL: `std::env::temp_dir().join(format!("tm-browser-{}", ids.random_hex(16)))`; create the
// directory here (`std::fs::create_dir_all`) so callers can rely on it existing, mapping any
// I/O failure through `TmError::from`. Uses the injected `IdSource` rather than `rand` directly
// per the workspace determinism rule.
pub fn ephemeral_profile_dir(ids: &dyn IdSource) -> Result<PathBuf> {
    let _ = ids;
    todo!("system temp dir joined with a random-hex-suffixed directory, created on disk")
}

/// The fixed argv (browser flags only, not the executable path — the caller supplies that as
/// `argv[0]` when spawning) for launching `config` headless with CDP enabled.
// IMPL: always emits `--headless=new`, `--remote-debugging-port=0`,
// `--user-data-dir=<profile_dir>`, `--no-first-run`, `--no-default-browser-check`, then
// `config.extra_args` verbatim, in that order (order matters for some flags' last-wins
// semantics, notably `--user-data-dir`).
pub fn build_argv(config: &LaunchConfig) -> Vec<String> {
    let _ = config;
    todo!("fixed headless/port/profile flags followed by config.extra_args")
}
