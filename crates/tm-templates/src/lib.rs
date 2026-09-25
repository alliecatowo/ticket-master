#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Project scaffolding templates (`SPEC.md` §27): deterministic, reviewable, versioned starting
//! points a project can be seeded from, distinct from Genesis's spec-to-graph compilation
//! (`tm-genesis`). A template is ordinary software — a file tree plus a manifest, a
//! `verify.toml` proving it builds, and a `skill.md` teaching its stack's idioms — not something
//! re-derived by a model on every use.
//!
//! - [`manifest`]: [`manifest::TemplateManifest`], parsed from `manifest.toml`, plus the blake3
//!   content checksum a registry pin is checked against.
//! - [`template`]: [`template::Template`], a manifest plus the on-disk directory it came from.
//! - [`apply`]: copy a template's `files/` into a destination, substituting `{{param}}`
//!   placeholders.
//! - [`verify`]: run a template's `verify.toml` through `tm_context::command::run` — the same
//!   gated, authority-checked path every other command in this codebase runs through.
//! - [`skill`]: load a template's `skill.md` as a `tm_context::pack::Section` — data a worker
//!   can read, never instructions injected into a system prompt.
//! - [`registry`]: [`registry::TemplateRegistry`], `templates.toml`'s source/version/checksum
//!   pins, so a template can't silently drift out from under a project that depends on it.
//! - [`bundled_catalog`]: this workspace's own starter set, embedded into the compiled binary at
//!   compile time (so an installed `tm` — which ships no `templates/` directory next to it —
//!   still has a real, non-empty catalog), `TM_TEMPLATES_DIR`-overridable to an on-disk
//!   [`registry::TemplateRegistry`] instead. `tm-genesis`'s `GenesisDriver` compiles a spec's
//!   graph against this catalog's `&[TemplateManifest]`.

use std::path::{Path, PathBuf};

use tm_types::{Result, TmError};

pub mod apply;
pub mod manifest;
pub mod registry;
pub mod skill;
pub mod template;
pub mod verify;

pub use apply::apply;
pub use manifest::{ParamSpec, ParamType, TemplateManifest};
pub use registry::{RegistryEntry, TemplateRegistry, TemplateSource};
pub use template::Template;
pub use verify::{VerifyCheck, VerifyOutcome, VerifyReport, VerifySpec};

/// `(manifest.toml text, blake3 checksum pin)` for each starter template under
/// `templates/starter/`, embedded at compile time via `include_str!` so this crate carries its
/// own bundled catalog without a runtime file read. The checksum pins are copied from
/// `templates/templates.toml`'s own entries; this module's own
/// `bundled_manifests_match_the_templates_toml_pins` test (below) is what keeps the two from
/// drifting apart — `crates/tm-templates/tests/starter_templates.rs`'s
/// `registry_pins_match_starter_template_contents` only checks `templates.toml` against the
/// starter fixtures on disk, not against this array.
const BUNDLED_STARTER_MANIFESTS: &[(&str, &str)] = &[
    (
        include_str!("../../../templates/starter/ratatui/manifest.toml"),
        "blake3:109355f88cac6b5149ef5738fea65bf343d433d383ff7c7541cadc60814db24e",
    ),
    (
        include_str!("../../../templates/starter/axum/manifest.toml"),
        "blake3:858d4e475f140e1b3d83ced446250b2c4ed1bb8189f9e0b1a992d0a35e9dec93",
    ),
    (
        include_str!("../../../templates/starter/rust-lib/manifest.toml"),
        "blake3:c284351c5518e4f820a20f81cf8b766e32fa1c4aab3b47524dfc2d199de38e64",
    ),
];

/// The template catalog `tm-genesis`'s `GenesisDriver` compiles a spec's graph against
/// (`SPEC.md` §27.1): by default, this crate's own bundled starter set — parsed directly from
/// [`BUNDLED_STARTER_MANIFESTS`]'s embedded text, so it works from a compiled binary with no
/// `templates/` directory anywhere nearby. Set `TM_TEMPLATES_DIR` to a directory containing its
/// own `templates.toml` ([`registry::TemplateRegistry`]'s format) to load from disk instead —
/// e.g. an installed layout that ships the starter set alongside the `tm` binary at a known path.
///
/// # Errors
/// [`TmError::Parse`] if a bundled manifest fails to parse — a bug in this crate's own fixtures,
/// not a caller error. With `TM_TEMPLATES_DIR` set, whatever
/// [`registry::TemplateRegistry::load`]/[`registry::TemplateRegistry::resolve_all`] return for
/// that directory (a malformed `templates.toml`, or the first entry that fails to resolve —
/// missing directory or checksum drift).
pub fn bundled_catalog() -> Result<Vec<TemplateManifest>> {
    match std::env::var("TM_TEMPLATES_DIR") {
        Ok(dir) => catalog_from_registry_dir(&PathBuf::from(dir)),
        Err(_) => embedded_catalog(),
    }
}

/// The pure, no-I/O half of [`bundled_catalog`]: parse every [`BUNDLED_STARTER_MANIFESTS`] entry.
fn embedded_catalog() -> Result<Vec<TemplateManifest>> {
    BUNDLED_STARTER_MANIFESTS
        .iter()
        .map(|(text, checksum)| {
            let mut manifest: TemplateManifest = toml::from_str(text)
                .map_err(|e| TmError::parse(format!("bundled template manifest: {e}")))?;
            manifest.checksum = (*checksum).to_string();
            Ok(manifest)
        })
        .collect()
}

/// The pure(-ish: reads `dir`, never the process environment) half of [`bundled_catalog`]'s
/// `TM_TEMPLATES_DIR` path: load `dir/templates.toml` and resolve every declared entry against
/// `dir`, checksum-verified by [`registry::TemplateRegistry::resolve`].
fn catalog_from_registry_dir(dir: &Path) -> Result<Vec<TemplateManifest>> {
    let registry_path = dir.join("templates.toml");
    if !registry_path.is_file() {
        // `TemplateRegistry::load` treats a missing file as an empty registry (the right default
        // for a project's own optional `templates.toml`), but `TM_TEMPLATES_DIR` is an explicit
        // override: silently returning an empty catalog here would make a wrong path look
        // identical to "this override deliberately has no templates".
        return Err(TmError::not_found(
            "templates.toml",
            format!(
                "TM_TEMPLATES_DIR is set to {}, but there's no templates.toml there",
                dir.display()
            ),
        ));
    }
    let registry = TemplateRegistry::load(&registry_path)?;
    registry
        .resolve_all(dir)
        .into_iter()
        .map(|(id, resolved)| {
            resolved
                .map(|template| template.manifest)
                .map_err(|e| TmError::invariant(format!("template {id:?}: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_parses_every_bundled_manifest() {
        let catalog = embedded_catalog().expect("embedded catalog parses");
        assert_eq!(catalog.len(), BUNDLED_STARTER_MANIFESTS.len());
        assert!(catalog.iter().any(|m| m.id == "starter-ratatui"));
        assert!(catalog.iter().all(|m| !m.checksum.is_empty()));
    }

    #[test]
    fn catalog_from_registry_dir_resolves_an_on_disk_override() {
        let base = tempfile::tempdir().expect("tempdir");
        let template_dir = base.path().join("t");
        std::fs::create_dir_all(template_dir.join("files")).expect("mkdir");
        std::fs::write(
            template_dir.join("manifest.toml"),
            "id = \"t\"\nversion = \"0.1.0\"\ntags = [\"widget\"]\n",
        )
        .expect("write manifest");
        let checksum = manifest::checksum_dir(&template_dir).expect("checksum");
        std::fs::write(
            base.path().join("templates.toml"),
            format!(
                "[[template]]\nid = \"t\"\nversion = \"0.1.0\"\nchecksum = \"{checksum}\"\n\n\
                 [template.source]\nkind = \"path\"\npath = \"t\"\n"
            ),
        )
        .expect("write templates.toml");

        let catalog = catalog_from_registry_dir(base.path()).expect("resolves override dir");
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "t");
        assert_eq!(catalog[0].tags, vec!["widget".to_string()]);
    }

    #[test]
    fn bundled_manifests_match_the_templates_toml_pins() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
        let registry = TemplateRegistry::load(&dir.join("templates.toml")).expect("load");
        let catalog = embedded_catalog().expect("parse");
        // A missing/empty `templates.toml` would make this pass vacuously; guard against that
        // rather than only checking a subset match.
        assert_eq!(catalog.len(), registry.entries.len());
        for entry in &registry.entries {
            let manifest = catalog
                .iter()
                .find(|m| m.id == entry.id)
                .unwrap_or_else(|| panic!("every templates.toml entry is embedded: {}", entry.id));
            assert_eq!(manifest.version, entry.version);
            assert_eq!(manifest.checksum, entry.checksum);
        }
    }

    #[test]
    fn catalog_from_registry_dir_errors_clearly_when_the_override_has_no_templates_toml() {
        let base = tempfile::tempdir().expect("tempdir");
        let err = catalog_from_registry_dir(base.path()).unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn bundled_catalog_defaults_to_the_embedded_set_without_the_env_override() {
        // No `TM_TEMPLATES_DIR` set here or anywhere else in this crate's tests, so this exercises
        // `bundled_catalog`'s no-override branch deterministically without touching the process
        // environment.
        let catalog = bundled_catalog().expect("bundled catalog resolves");
        assert_eq!(catalog.len(), BUNDLED_STARTER_MANIFESTS.len());
    }
}
