//! [`TemplateManifest`]: a template's identity, tunable parameters, and capability tags, parsed
//! from `manifest.toml`, plus the blake3 checksum of everything the template actually ships.
//!
//! The checksum is always computed from what's on disk by [`load`], never trusted from a value
//! written inside `manifest.toml` itself: `SPEC.md` §27.4 treats a third-party template as
//! untrusted input, and a self-reported checksum proves nothing about drift — only
//! [`crate::registry::TemplateRegistry`]'s independently pinned value, checked against this
//! computed one, can.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tm_types::{Result, TmError};

/// The declared shape of a [`ParamSpec`]'s value. [`crate::apply::apply`] only ever substitutes
/// text regardless of this — it exists for a caller's own validation and for `tm templates
/// show` to render something more useful than "string".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    /// Free text (the default).
    #[default]
    String,
    /// `"true"`/`"false"`.
    Bool,
    /// A base-10 integer.
    Integer,
}

/// One parameter a template exposes for `{{name}}` substitution into its scaffolded files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamSpec {
    /// The `{{name}}` placeholder this parameter fills.
    pub name: String,
    /// The parameter's declared shape.
    #[serde(rename = "type", default)]
    pub param_type: ParamType,
    /// The value used when a caller does not supply this parameter via
    /// [`crate::apply::apply`]'s `params`. `None` makes the parameter required.
    #[serde(default)]
    pub default: Option<String>,
    /// What this parameter controls, shown by `tm templates show`.
    #[serde(default)]
    pub description: String,
}

/// The wire shape of `manifest.toml`: everything [`TemplateManifest`] carries except
/// `checksum`, which this crate never reads from the file (see the module docs).
#[derive(Debug, Clone, Deserialize)]
struct ManifestFile {
    id: String,
    version: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    params: Vec<ParamSpec>,
}

/// A template's identity, tunable parameters, capability tags, and content checksum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateManifest {
    /// Stable id, e.g. `"starter-ratatui"`. Matches a
    /// [`crate::registry::RegistryEntry::id`] that pins it.
    pub id: String,
    /// Version string this manifest describes, pinned by a registry entry.
    pub version: String,
    /// SPDX license expression, if declared.
    #[serde(default)]
    pub license: Option<String>,
    /// Capability tags used to match a template against a spec's named stack (e.g. `"rust"`,
    /// `"tui"`, `"ratatui"`, `"web-api"`). [`tm_genesis::compile::select_template`] matches
    /// these against a `Specification`'s prose.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Parameters this template accepts.
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    /// `"blake3:<hex>"` digest of every file under the template's own directory (`manifest.toml`
    /// itself, `files/`, `verify.toml`, `skill.md`), computed by [`load`] from what is actually
    /// on disk right now. Compared against a [`crate::registry::RegistryEntry::checksum`] pin by
    /// [`crate::registry::TemplateRegistry::resolve`] to catch drift. Empty until [`load`] fills
    /// it in — never populated by deserializing `manifest.toml`.
    #[serde(default)]
    pub checksum: String,
}

/// Parse `dir/manifest.toml` and compute the checksum of everything under `dir`.
///
/// # Errors
/// [`TmError::Io`] if `manifest.toml` cannot be read; [`TmError::Parse`] if it is malformed TOML
/// or missing a required field (`id`, `version`).
pub fn load(dir: &Path) -> Result<TemplateManifest> {
    let manifest_path = dir.join("manifest.toml");
    let contents = std::fs::read_to_string(&manifest_path).map_err(|_e| {
        TmError::Io(format!(
            "template source not found: {} (manifest.toml)",
            dir.display()
        ))
    })?;
    let file: ManifestFile = toml::from_str(&contents)
        .map_err(|e| TmError::parse(format!("{}: {e}", manifest_path.display())))?;
    let checksum = checksum_dir(dir)?;
    Ok(TemplateManifest {
        id: file.id,
        version: file.version,
        license: file.license,
        tags: file.tags,
        params: file.params,
        checksum,
    })
}

/// Blake3 checksum over every regular file under `dir`, keyed by its path relative to `dir`
/// (forward-slash separated, sorted lexicographically), so the digest is stable across
/// platforms and independent of directory-listing order.
///
/// # Errors
/// [`TmError::Io`] on a filesystem failure walking or reading `dir`.
pub fn checksum_dir(dir: &Path) -> Result<String> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort();
    let mut hasher = blake3::Hasher::new();
    for (rel, path) in &files {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        let bytes = std::fs::read(path)
            .map_err(|e| TmError::Io(format!("reading {}: {e}", path.display())))?;
        hasher.update(&bytes);
        hasher.update(b"\0");
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// Recursively collect `(path relative to root, absolute path)` for every regular file under
/// `dir`, appending into `out`.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| TmError::Io(format!("reading {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| TmError::Io(e.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| TmError::invariant("walked file escaped its own root"))?
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write fixture");
    }

    #[test]
    fn loads_a_minimal_manifest_and_computes_a_checksum() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "manifest.toml",
            "id = \"t\"\nversion = \"0.1.0\"\ntags = [\"rust\"]\n",
        );
        write(dir.path(), "files/src/main.rs", "fn main() {}\n");

        let manifest = load(dir.path()).expect("loads");
        assert_eq!(manifest.id, "t");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.tags, vec!["rust".to_string()]);
        assert!(manifest.checksum.starts_with("blake3:"));
    }

    #[test]
    fn checksum_changes_when_a_file_changes_and_is_independent_of_listing_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "manifest.toml",
            "id = \"t\"\nversion = \"0.1.0\"\n",
        );
        write(dir.path(), "files/a.txt", "one");
        write(dir.path(), "files/b.txt", "two");
        let first = checksum_dir(dir.path()).expect("checksum");

        // Same content, re-hashed: deterministic.
        let again = checksum_dir(dir.path()).expect("checksum");
        assert_eq!(first, again);

        write(dir.path(), "files/a.txt", "one-changed");
        let changed = checksum_dir(dir.path()).expect("checksum");
        assert_ne!(first, changed);
    }

    #[test]
    fn missing_manifest_is_an_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load(dir.path()).unwrap_err();
        assert!(matches!(err, TmError::Io(_)));
        // Verify the error message is human-friendly and doesn't include OS error codes
        let msg = err.to_string();
        assert!(msg.contains("template source not found"));
        assert!(msg.contains("manifest.toml"));
        // Should not contain OS error noise
        assert!(!msg.contains("No such file"));
        assert!(!msg.contains("os error"));
    }

    #[test]
    fn malformed_manifest_is_a_parse_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "manifest.toml", "not valid toml {{{");
        let err = load(dir.path()).unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }
}
