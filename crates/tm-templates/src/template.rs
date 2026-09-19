//! [`Template`]: a loaded template — its [`TemplateManifest`] plus the on-disk directory it was
//! loaded from (`files/`, `verify.toml`, an optional `skill.md`).

use std::path::{Path, PathBuf};

use tm_types::Result;

use crate::manifest::{self, TemplateManifest};

/// A template loaded from disk: its parsed [`TemplateManifest`] plus the directory it lives in.
#[derive(Debug, Clone)]
pub struct Template {
    /// The template's manifest, including its computed content checksum.
    pub manifest: TemplateManifest,
    /// The template's own directory: `manifest.toml`, `files/`, `verify.toml`, and an optional
    /// `skill.md` all live directly under this path.
    pub root: PathBuf,
}

impl Template {
    /// Load a template from `root`, a directory containing `manifest.toml`.
    ///
    /// # Errors
    /// Whatever [`manifest::load`] returns.
    pub fn load(root: &Path) -> Result<Template> {
        let manifest = manifest::load(root)?;
        Ok(Template {
            manifest,
            root: root.to_path_buf(),
        })
    }

    /// The directory [`crate::apply::apply`] copies into a destination.
    pub fn files_dir(&self) -> PathBuf {
        self.root.join("files")
    }

    /// `verify.toml`'s path (may not exist — [`crate::verify::load`] treats that as "no
    /// checks").
    pub fn verify_path(&self) -> PathBuf {
        self.root.join("verify.toml")
    }

    /// `skill.md`'s path (may not exist — [`crate::skill::load`] treats that as "no skill").
    pub fn skill_path(&self) -> PathBuf {
        self.root.join("skill.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reports_paths_relative_to_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("manifest.toml"),
            "id = \"t\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        let template = Template::load(dir.path()).expect("loads");
        assert_eq!(template.files_dir(), dir.path().join("files"));
        assert_eq!(template.verify_path(), dir.path().join("verify.toml"));
        assert_eq!(template.skill_path(), dir.path().join("skill.md"));
    }
}
