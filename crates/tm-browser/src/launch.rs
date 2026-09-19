//! Shared launch-argv-building helpers: a throwaway profile directory and the fixed
//! headless/port/profile flag set every local browser process is started with.
//!
//! This module owns no notion of *which* browser binary to run or how it got onto the machine —
//! that is [`crate::provider::BrowserProvider`]'s job (`SPEC.md` §19.1a). Only
//! [`crate::managed::ManagedProvider::acquire`] calls into this module today, once it has
//! resolved a binary path for the pinned build it downloaded; it is kept separate rather than
//! folded into `managed.rs` because argv construction is pure and worth testing without a
//! provider, a download, or a process in the loop.

use std::path::PathBuf;

use tm_types::{IdSource, Result, TmError};

/// Launch-time configuration for a local browser process: which binary, a throwaway profile,
/// and extra flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchConfig {
    /// Absolute path to the browser executable to spawn.
    pub binary_path: PathBuf,
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

    fn sample_config(profile_dir: PathBuf, extra_args: Vec<String>) -> LaunchConfig {
        LaunchConfig {
            binary_path: PathBuf::from("/tmp/fake-chrome"),
            profile_dir,
            extra_args,
        }
    }

    #[test]
    fn ephemeral_profile_dir_creates_directory() {
        let ids = TestIds::seeded(1001);
        let result = ephemeral_profile_dir(&ids);
        assert!(result.is_ok());

        let profile_dir = result.unwrap();
        assert!(profile_dir.exists());
        assert!(profile_dir.is_dir());

        let _ = fs::remove_dir_all(&profile_dir);
    }

    #[test]
    fn ephemeral_profile_dir_unique_paths() {
        let ids1 = TestIds::seeded(1002);
        let ids2 = TestIds::seeded(1003);

        let dir1 = ephemeral_profile_dir(&ids1).unwrap();
        let dir2 = ephemeral_profile_dir(&ids2).unwrap();

        assert_ne!(dir1, dir2);

        let _ = fs::remove_dir_all(dir1);
        let _ = fs::remove_dir_all(dir2);
    }

    #[test]
    fn build_argv_contains_required_flags() {
        let ids = TestIds::seeded(1004);
        let profile_dir = ephemeral_profile_dir(&ids).unwrap();
        let config = sample_config(profile_dir.clone(), vec![]);

        let argv = build_argv(&config);

        assert!(argv.contains(&"--headless=new".to_string()));
        assert!(argv.contains(&"--remote-debugging-port=0".to_string()));
        assert!(argv.iter().any(|arg| arg.contains("--user-data-dir=")));
        assert!(argv.contains(&"--no-first-run".to_string()));
        assert!(argv.contains(&"--no-default-browser-check".to_string()));

        let _ = fs::remove_dir_all(&profile_dir);
    }

    #[test]
    fn build_argv_includes_extra_args() {
        let ids = TestIds::seeded(1005);
        let profile_dir = ephemeral_profile_dir(&ids).unwrap();
        let config = sample_config(
            profile_dir.clone(),
            vec!["--disable-gpu".to_string(), "--single-process".to_string()],
        );

        let argv = build_argv(&config);

        assert!(argv.contains(&"--disable-gpu".to_string()));
        assert!(argv.contains(&"--single-process".to_string()));

        let _ = fs::remove_dir_all(&profile_dir);
    }

    #[test]
    fn build_argv_flags_order() {
        let ids = TestIds::seeded(1006);
        let profile_dir = ephemeral_profile_dir(&ids).unwrap();
        let config = sample_config(profile_dir.clone(), vec!["--extra".to_string()]);

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

        let _ = fs::remove_dir_all(&profile_dir);
    }
}
