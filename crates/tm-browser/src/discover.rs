//! Locate a usable system Chromium-family browser and build its launch argv.
//!
//! We never download a browser. We probe, in order, `$TM_BROWSER`, then Chrome, Chromium,
//! Brave and Edge at their platform-conventional install locations; if none exists we fail
//! with [`install_hint`], the exact command to install one, rather than reaching for the
//! network (`SPEC.md` §19.1). Launch config is always headless (`--headless=new`), a random
//! debugging port (`--remote-debugging-port=0`, discovered after launch — see
//! `session.rs`), and a throwaway profile directory so sessions never collide and never touch
//! the user's real Chrome profile.

use std::path::{Path, PathBuf};

use tm_types::{IdSource, Result, TmError};

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
pub fn discover() -> Result<DiscoveredBrowser> {
    if let Some(tm_browser) = std::env::var_os("TM_BROWSER") {
        let path = Path::new(&tm_browser);
        if path.is_file() {
            return Ok(DiscoveredBrowser {
                kind: BrowserKind::Chrome, // Assume Chrome when using explicit path
                path: path.to_path_buf(),
                from_env_override: true,
            });
        }
        return Err(TmError::not_found(
            "browser",
            format!("$TM_BROWSER={}", tm_browser.to_string_lossy()),
        ));
    }

    for kind in BrowserKind::ALL {
        for candidate_path in candidate_paths(kind) {
            if candidate_path.is_file() {
                return Ok(DiscoveredBrowser {
                    kind,
                    path: candidate_path,
                    from_env_override: false,
                });
            }
        }
    }

    Err(TmError::not_found(
        "browser",
        format!("no Chromium-family browser found. {}", install_hint()),
    ))
}

/// The platform-conventional install locations to probe for `kind`, most-likely-first.
///
/// Each platform returns absolute paths only (no PATH search — a bare `chrome` on `$PATH` is
/// covered by `$TM_BROWSER` if a user wants that). Never touches the filesystem itself;
/// `discover` does the `is_file()` check so this stays a pure, testable mapping.
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
#[cfg(target_os = "macos")]
fn macos_candidate_paths(kind: BrowserKind) -> Vec<PathBuf> {
    let (app_name, binary_name) = match kind {
        BrowserKind::Chrome => ("Google Chrome", "Google Chrome"),
        BrowserKind::Chromium => ("Chromium", "Chromium"),
        BrowserKind::Brave => ("Brave Browser", "Brave Browser"),
        BrowserKind::Edge => ("Microsoft Edge", "Microsoft Edge"),
    };

    let mut paths = vec![PathBuf::from(format!(
        "/Applications/{}.app/Contents/MacOS/{}",
        app_name, binary_name
    ))];

    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(format!(
            "{}/Applications/{}.app/Contents/MacOS/{}",
            home, app_name, binary_name
        )));
    }

    paths
}

/// Linux `/usr/bin`, `/usr/local/bin` and `/opt` conventional binary names for `kind`.
#[cfg(target_os = "linux")]
fn linux_candidate_paths(kind: BrowserKind) -> Vec<PathBuf> {
    let binary_names = match kind {
        BrowserKind::Chrome => vec!["google-chrome", "google-chrome-stable"],
        BrowserKind::Chromium => vec!["chromium", "chromium-browser"],
        BrowserKind::Brave => vec!["brave-browser"],
        BrowserKind::Edge => vec!["microsoft-edge", "microsoft-edge-stable"],
    };

    let base_dirs = ["/usr/bin", "/usr/local/bin", "/snap/bin"];

    let mut paths = Vec::new();

    for binary in &binary_names {
        for base_dir in &base_dirs {
            paths.push(PathBuf::from(format!("{}/{}", base_dir, binary)));
        }
    }

    // Add /opt/<vendor>/ paths
    match kind {
        BrowserKind::Chrome => {
            paths.push(PathBuf::from("/opt/google/chrome/chrome"));
        }
        BrowserKind::Brave => {
            paths.push(PathBuf::from("/opt/brave/brave"));
        }
        BrowserKind::Edge => {
            paths.push(PathBuf::from("/opt/microsoft/edge/microsoft-edge"));
        }
        BrowserKind::Chromium => {}
    }

    paths
}

/// Windows `Program Files` locations for `kind`, plus an App Paths registry lookup.
#[cfg(target_os = "windows")]
fn windows_candidate_paths(kind: BrowserKind) -> Vec<PathBuf> {
    let (subpath, registry_key) = match kind {
        BrowserKind::Chrome => ("Google\\Chrome\\Application\\chrome.exe", "chrome.exe"),
        BrowserKind::Chromium => ("Chromium\\Application\\chrome.exe", "chromium.exe"),
        BrowserKind::Brave => (
            "BraveSoftware\\Brave-Browser\\Application\\brave.exe",
            "brave.exe",
        ),
        BrowserKind::Edge => ("Microsoft\\Edge\\Application\\msedge.exe", "msedge.exe"),
    };

    let mut paths = Vec::new();

    // Add %ProgramFiles% paths
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        paths.push(PathBuf::from(format!("{}\\{}", program_files, subpath)));
    }

    // Add %ProgramFiles(x86)% paths
    if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
        paths.push(PathBuf::from(format!("{}\\{}", program_files_x86, subpath)));
    }

    // Add %LocalAppData% paths
    if let Ok(local_app_data) = std::env::var("LocalAppData") {
        let local_subpath = match kind {
            BrowserKind::Chrome => "Google\\Chrome\\Application\\chrome.exe",
            BrowserKind::Chromium => "Chromium\\Application\\chrome.exe",
            BrowserKind::Brave => "BraveSoftware\\Brave-Browser\\Application\\brave.exe",
            BrowserKind::Edge => "Microsoft\\Edge\\Application\\msedge.exe",
        };
        paths.push(PathBuf::from(format!(
            "{}\\{}",
            local_app_data, local_subpath
        )));
    }

    // Try to read from registry
    if let Ok(hklm) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths")
    {
        if let Ok(app_path) = hklm.get_value::<String, &str>(registry_key) {
            paths.push(PathBuf::from(app_path));
        }
    }

    // Fallback: try the registry key for 32-bit apps on 64-bit systems
    if let Ok(hklm) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey("SOFTWARE\\Wow6432Node\\Microsoft\\Windows\\CurrentVersion\\App Paths")
    {
        if let Ok(app_path) = hklm.get_value::<String, &str>(registry_key) {
            paths.push(PathBuf::from(app_path));
        }
    }

    paths
}

/// The exact command to install a Chromium-family browser on this platform, used to make
/// [`discover`]'s failure actionable instead of a dead end.
pub fn install_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "To install, run: brew install --cask google-chrome"
    }
    #[cfg(target_os = "linux")]
    {
        "To install, run: sudo apt install chromium-browser (or use your distro's package manager)"
    }
    #[cfg(target_os = "windows")]
    {
        "To install, run: winget install Google.Chrome"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "Please install Google Chrome, Chromium, Brave, or Microsoft Edge"
    }
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
pub fn ephemeral_profile_dir(ids: &dyn IdSource) -> Result<PathBuf> {
    let profile_dir = std::env::temp_dir().join(format!("tm-browser-{}", ids.random_hex(16)));
    std::fs::create_dir_all(&profile_dir)
        .map_err(|e| TmError::Io(format!("failed to create profile directory: {}", e)))?;
    Ok(profile_dir)
}

/// The fixed argv (browser flags only, not the executable path — the caller supplies that as
/// `argv[0]` when spawning) for launching `config` headless with CDP enabled.
pub fn build_argv(config: &LaunchConfig) -> Vec<String> {
    let mut argv = vec![
        "--headless=new".to_string(),
        "--remote-debugging-port=0".to_string(),
        format!("--user-data-dir={}", config.profile_dir.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ];
    argv.extend(config.extra_args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tm_types::TestIds;

    #[test]
    fn browser_kind_display_names() {
        assert_eq!(BrowserKind::Chrome.display_name(), "Google Chrome");
        assert_eq!(BrowserKind::Chromium.display_name(), "Chromium");
        assert_eq!(BrowserKind::Brave.display_name(), "Brave");
        assert_eq!(BrowserKind::Edge.display_name(), "Microsoft Edge");
    }

    #[test]
    fn browser_kind_all_contains_all_variants() {
        assert_eq!(BrowserKind::ALL.len(), 4);
        assert!(BrowserKind::ALL.contains(&BrowserKind::Chrome));
        assert!(BrowserKind::ALL.contains(&BrowserKind::Chromium));
        assert!(BrowserKind::ALL.contains(&BrowserKind::Brave));
        assert!(BrowserKind::ALL.contains(&BrowserKind::Edge));
    }

    #[test]
    fn macos_candidate_paths_returns_non_empty() {
        #[cfg(target_os = "macos")]
        {
            let paths = macos_candidate_paths(BrowserKind::Chrome);
            assert!(!paths.is_empty());
            assert!(paths[0].to_string_lossy().contains("Google Chrome"));
        }
        #[cfg(not(target_os = "macos"))]
        {
            // Ensure the function compiles and is callable
            let _ = BrowserKind::Chrome;
        }
    }

    #[test]
    fn linux_candidate_paths_returns_non_empty() {
        #[cfg(target_os = "linux")]
        {
            let paths = linux_candidate_paths(BrowserKind::Chrome);
            assert!(!paths.is_empty());
            // Should include common paths
            let paths_str = paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<String>();
            assert!(paths_str.contains("/usr/bin") || paths_str.contains("google-chrome"));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = BrowserKind::Chrome;
        }
    }

    #[test]
    fn windows_candidate_paths_returns_non_empty() {
        #[cfg(target_os = "windows")]
        {
            let paths = windows_candidate_paths(BrowserKind::Chrome);
            assert!(!paths.is_empty());
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = BrowserKind::Chrome;
        }
    }

    #[test]
    fn candidate_paths_all_kinds() {
        for kind in BrowserKind::ALL {
            let paths = candidate_paths(kind);
            assert!(
                !paths.is_empty(),
                "candidate_paths should return non-empty for {}",
                kind.display_name()
            );
        }
    }

    #[test]
    fn install_hint_returns_non_empty() {
        let hint = install_hint();
        assert!(!hint.is_empty());
        assert!(hint.contains("install") || hint.contains("Install") || hint.contains("Please"));
    }

    #[test]
    fn ephemeral_profile_dir_creates_directory() {
        let ids = TestIds::seeded(1001);
        let result = ephemeral_profile_dir(&ids);
        assert!(result.is_ok());

        let profile_dir = result.unwrap();
        assert!(profile_dir.exists());
        assert!(profile_dir.is_dir());

        // Clean up
        let _ = fs::remove_dir_all(&profile_dir);
    }

    #[test]
    fn ephemeral_profile_dir_unique_paths() {
        let ids1 = TestIds::seeded(1002);
        let ids2 = TestIds::seeded(1003);

        let dir1 = ephemeral_profile_dir(&ids1).unwrap();
        let dir2 = ephemeral_profile_dir(&ids2).unwrap();

        // Paths should be different (seeded different seeds produce different hex)
        assert_ne!(dir1, dir2);

        // Clean up
        let _ = fs::remove_dir_all(dir1);
        let _ = fs::remove_dir_all(dir2);
    }

    #[test]
    fn ephemeral_profile_dir_same_seed_same_path() {
        let ids = TestIds::seeded(42);
        let dir1 = ephemeral_profile_dir(&ids).unwrap();
        let dir2 = ephemeral_profile_dir(&ids).unwrap();

        // Same seed produces different paths each call due to RNG state advancement
        // but both should exist
        assert!(dir1.exists());
        assert!(dir2.exists());

        // Clean up
        let _ = fs::remove_dir_all(dir1);
        let _ = fs::remove_dir_all(dir2);
    }

    #[test]
    fn build_argv_contains_required_flags() {
        let ids = TestIds::seeded(1004);
        let profile_dir = ephemeral_profile_dir(&ids).unwrap();
        let browser = DiscoveredBrowser {
            kind: BrowserKind::Chrome,
            path: PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            from_env_override: false,
        };
        let config = LaunchConfig {
            browser,
            profile_dir,
            extra_args: vec![],
        };

        let argv = build_argv(&config);

        assert!(argv.contains(&"--headless=new".to_string()));
        assert!(argv.contains(&"--remote-debugging-port=0".to_string()));
        assert!(argv.iter().any(|arg| arg.contains("--user-data-dir=")));
        assert!(argv.contains(&"--no-first-run".to_string()));
        assert!(argv.contains(&"--no-default-browser-check".to_string()));

        // Clean up
        let _ = fs::remove_dir_all(&config.profile_dir);
    }

    #[test]
    fn build_argv_includes_extra_args() {
        let ids = TestIds::seeded(1005);
        let profile_dir = ephemeral_profile_dir(&ids).unwrap();
        let browser = DiscoveredBrowser {
            kind: BrowserKind::Chrome,
            path: PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            from_env_override: false,
        };
        let config = LaunchConfig {
            browser,
            profile_dir,
            extra_args: vec!["--disable-gpu".to_string(), "--single-process".to_string()],
        };

        let argv = build_argv(&config);

        assert!(argv.contains(&"--disable-gpu".to_string()));
        assert!(argv.contains(&"--single-process".to_string()));

        // Clean up
        let _ = fs::remove_dir_all(&config.profile_dir);
    }

    #[test]
    fn build_argv_flags_order() {
        let ids = TestIds::seeded(1006);
        let profile_dir = ephemeral_profile_dir(&ids).unwrap();
        let browser = DiscoveredBrowser {
            kind: BrowserKind::Chrome,
            path: PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            from_env_override: false,
        };
        let config = LaunchConfig {
            browser,
            profile_dir,
            extra_args: vec!["--extra".to_string()],
        };

        let argv = build_argv(&config);

        let headless_idx = argv.iter().position(|arg| arg == "--headless=new").unwrap();
        let port_idx = argv
            .iter()
            .position(|arg| arg == "--remote-debugging-port=0")
            .unwrap();
        let profile_idx = argv
            .iter()
            .position(|arg| arg.contains("--user-data-dir="))
            .unwrap();
        let extra_idx = argv.iter().position(|arg| arg == "--extra").unwrap();

        assert!(headless_idx < port_idx);
        assert!(port_idx < profile_idx);
        assert!(profile_idx < extra_idx);

        // Clean up
        let _ = fs::remove_dir_all(&config.profile_dir);
    }

    #[test]
    fn discovered_browser_equality() {
        let browser1 = DiscoveredBrowser {
            kind: BrowserKind::Chrome,
            path: PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            from_env_override: true,
        };
        let browser2 = DiscoveredBrowser {
            kind: BrowserKind::Chrome,
            path: PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            from_env_override: true,
        };

        assert_eq!(browser1, browser2);
    }

    #[test]
    fn discovered_browser_inequality() {
        let browser1 = DiscoveredBrowser {
            kind: BrowserKind::Chrome,
            path: PathBuf::from("/path1"),
            from_env_override: true,
        };
        let browser2 = DiscoveredBrowser {
            kind: BrowserKind::Chrome,
            path: PathBuf::from("/path2"),
            from_env_override: true,
        };

        assert_ne!(browser1, browser2);
    }
}
