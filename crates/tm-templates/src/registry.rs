//! [`TemplateRegistry`]: `templates.toml` — sources (path/git/registry), pinned by version and a
//! blake3 checksum of the template's actual contents, so a template can't silently drift
//! (`SPEC.md` §27.4).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tm_types::{Result, TmError};

use crate::template::Template;

/// Where a template's contents come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TemplateSource {
    /// A directory on disk, relative to the registry's own directory unless absolute. The only
    /// source kind [`TemplateRegistry::resolve`] actually fetches.
    Path {
        /// The directory containing `manifest.toml`.
        path: String,
    },
    /// A git repository. Schema only, for `SPEC.md` §27.4's future community-template story:
    /// [`TemplateRegistry::resolve`] returns [`TmError::Invariant`] for this source kind today.
    Git {
        /// Clone URL.
        url: String,
        /// Branch, tag, or commit to pin to.
        #[serde(default)]
        git_ref: Option<String>,
    },
    /// A named entry in some future template registry service. Schema only, same status as
    /// [`TemplateSource::Git`].
    Registry {
        /// The id this template is known by in that registry.
        registry_id: String,
    },
}

/// One `templates.toml` entry: a template id, pinned version, pinned content checksum, and
/// where to fetch it from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// The template id, matching [`crate::manifest::TemplateManifest::id`].
    pub id: String,
    /// The pinned version, matching [`crate::manifest::TemplateManifest::version`].
    pub version: String,
    /// The pinned `"blake3:<hex>"` digest of the template's contents, checked against
    /// [`crate::manifest::checksum_dir`] on every [`TemplateRegistry::resolve`].
    pub checksum: String,
    /// Where to fetch this template's contents from.
    pub source: TemplateSource,
}

/// A parsed `templates.toml`: every declared [`RegistryEntry`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateRegistry {
    /// Every declared entry, in file order.
    #[serde(rename = "template", default)]
    pub entries: Vec<RegistryEntry>,
}

impl TemplateRegistry {
    /// Parse `path` as a `templates.toml`. A missing file is not an error: it parses as an empty
    /// registry, mirroring `tm-docs`' `docs/.tmdocs.toml` convention — no file declared yet is a
    /// legitimately empty answer, not a bug to paper over.
    ///
    /// # Errors
    /// [`TmError::Io`] if `path` exists but cannot be read; [`TmError::Parse`] if malformed.
    pub fn load(path: &Path) -> Result<TemplateRegistry> {
        if !path.is_file() {
            return Ok(TemplateRegistry::default());
        }
        let contents = std::fs::read_to_string(path)
            .map_err(|e| TmError::Io(format!("reading {}: {e}", path.display())))?;
        toml::from_str(&contents).map_err(|e| TmError::parse(format!("{}: {e}", path.display())))
    }

    /// Find a declared entry by id.
    pub fn find(&self, id: &str) -> Option<&RegistryEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Resolve `id` to a loaded [`Template`], relative to `base_dir` (typically the directory
    /// containing the `templates.toml` this registry was loaded from) for [`TemplateSource::Path`]
    /// sources.
    ///
    /// # Errors
    /// [`TmError::NotFound`] if `id` is not declared. [`TmError::Invariant`] for a `Git`/
    /// `Registry` source (not fetchable yet). [`TmError::Conflict`] if the resolved template's
    /// computed content checksum or manifest version does not match this entry's pin (drift).
    /// Otherwise, whatever [`Template::load`] returns.
    pub fn resolve(&self, id: &str, base_dir: &Path) -> Result<Template> {
        let entry = self
            .find(id)
            .ok_or_else(|| TmError::not_found("template", id))?;
        let dir = match &entry.source {
            TemplateSource::Path { path } => {
                let p = PathBuf::from(path);
                if p.is_absolute() {
                    p
                } else {
                    base_dir.join(p)
                }
            }
            TemplateSource::Git { .. } => {
                return Err(TmError::invariant(format!(
                    "template {id:?} uses a git source, which this crate does not fetch yet"
                )));
            }
            TemplateSource::Registry { .. } => {
                return Err(TmError::invariant(format!(
                    "template {id:?} uses a registry source, which this crate does not fetch yet"
                )));
            }
        };
        let template = Template::load(&dir)?;
        if template.manifest.checksum != entry.checksum {
            return Err(TmError::conflict(format!(
                "template {id:?} content checksum {} does not match the templates.toml pin {} \
                 (drift)",
                template.manifest.checksum, entry.checksum
            )));
        }
        if template.manifest.version != entry.version {
            return Err(TmError::conflict(format!(
                "template {id:?} manifest version {:?} does not match the templates.toml pin \
                 {:?}",
                template.manifest.version, entry.version
            )));
        }
        Ok(template)
    }

    /// Resolve every declared entry against `base_dir`, in file order, pairing each id with its
    /// [`TemplateRegistry::resolve`] outcome. Unlike `resolve`, a single entry's failure (an
    /// unsupported source, drift, a missing directory) does not stop the others — but it is not
    /// swallowed either: it comes back as this entry's `Err`, for a caller like `tm templates
    /// list` to show inline rather than silently omitting the entry.
    pub fn resolve_all(&self, base_dir: &Path) -> Vec<(String, Result<Template>)> {
        self.entries
            .iter()
            .map(|e| (e.id.clone(), self.resolve(&e.id, base_dir)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::checksum_dir;

    fn write_fixture_template(dir: &Path) {
        std::fs::create_dir_all(dir.join("files")).expect("mkdir");
        std::fs::write(
            dir.join("manifest.toml"),
            "id = \"t\"\nversion = \"0.1.0\"\n",
        )
        .expect("write");
        std::fs::write(dir.join("files/lib.rs"), "// nothing\n").expect("write");
    }

    #[test]
    fn missing_registry_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = TemplateRegistry::load(&dir.path().join("templates.toml")).expect("loads");
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn resolves_a_path_source_and_checks_the_pin() {
        let base = tempfile::tempdir().expect("tempdir");
        let template_dir = base.path().join("t");
        write_fixture_template(&template_dir);
        let checksum = checksum_dir(&template_dir).expect("checksum");

        let registry = TemplateRegistry {
            entries: vec![RegistryEntry {
                id: "t".to_string(),
                version: "0.1.0".to_string(),
                checksum,
                source: TemplateSource::Path {
                    path: "t".to_string(),
                },
            }],
        };
        let template = registry.resolve("t", base.path()).expect("resolves");
        assert_eq!(template.manifest.id, "t");
    }

    #[test]
    fn checksum_drift_is_a_conflict() {
        let base = tempfile::tempdir().expect("tempdir");
        let template_dir = base.path().join("t");
        write_fixture_template(&template_dir);

        let registry = TemplateRegistry {
            entries: vec![RegistryEntry {
                id: "t".to_string(),
                version: "0.1.0".to_string(),
                checksum: "blake3:not-the-real-checksum".to_string(),
                source: TemplateSource::Path {
                    path: "t".to_string(),
                },
            }],
        };
        let err = registry.resolve("t", base.path()).unwrap_err();
        assert!(matches!(err, TmError::Conflict(_)));
    }

    #[test]
    fn unknown_id_is_not_found() {
        let base = tempfile::tempdir().expect("tempdir");
        let registry = TemplateRegistry::default();
        let err = registry.resolve("nope", base.path()).unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn git_source_is_invariant_not_fetched() {
        let base = tempfile::tempdir().expect("tempdir");
        let registry = TemplateRegistry {
            entries: vec![RegistryEntry {
                id: "g".to_string(),
                version: "0.1.0".to_string(),
                checksum: "blake3:whatever".to_string(),
                source: TemplateSource::Git {
                    url: "https://example.invalid/t.git".to_string(),
                    git_ref: Some("main".to_string()),
                },
            }],
        };
        let err = registry.resolve("g", base.path()).unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn parses_a_toml_registry_with_all_three_source_kinds() {
        let toml_text = r#"
[[template]]
id = "starter-ratatui"
version = "0.1.0"
checksum = "blake3:aaaa"

[template.source]
kind = "path"
path = "templates/starter/ratatui"

[[template]]
id = "community-example"
version = "1.0.0"
checksum = "blake3:bbbb"

[template.source]
kind = "git"
url = "https://example.invalid/example.git"
git_ref = "v1.0.0"

[[template]]
id = "hub-example"
version = "2.0.0"
checksum = "blake3:cccc"

[template.source]
kind = "registry"
registry_id = "example-hub/example"
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("templates.toml");
        std::fs::write(&path, toml_text).expect("write");
        let registry = TemplateRegistry::load(&path).expect("parses");
        assert_eq!(registry.entries.len(), 3);
        assert!(matches!(
            registry.entries[0].source,
            TemplateSource::Path { .. }
        ));
        assert!(matches!(
            registry.entries[1].source,
            TemplateSource::Git { .. }
        ));
        assert!(matches!(
            registry.entries[2].source,
            TemplateSource::Registry { .. }
        ));
    }
}
