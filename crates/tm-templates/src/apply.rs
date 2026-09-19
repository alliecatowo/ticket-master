//! [`apply`]: copy a [`Template`]'s `files/` into a destination directory, substituting
//! `{{param}}` placeholders in file contents.

use std::collections::BTreeMap;
use std::path::Path;

use tm_types::{Result, TmError};

use crate::template::Template;

/// Copy `template`'s `files/` tree into `dest` (created if it does not already exist),
/// substituting every `{{name}}` occurrence in each file's *contents* with the effective value
/// for `name` — the caller-supplied value in `params` if present, else the parameter's declared
/// default. Filenames are copied verbatim: the starter set never needs a parameterized path,
/// and this keeps `apply` simple without ruling out filename substitution being added later
/// without a signature change.
///
/// # Errors
/// [`TmError::Invariant`] if `template` has no `files/` directory, or if a declared parameter
/// with no default is missing from `params`. [`TmError::Io`] on a filesystem failure reading
/// `template`'s files or writing `dest`.
pub fn apply(template: &Template, params: &BTreeMap<String, String>, dest: &Path) -> Result<()> {
    let effective = effective_params(template, params)?;
    let files_dir = template.files_dir();
    if !files_dir.is_dir() {
        return Err(TmError::invariant(format!(
            "template {:?} has no files/ directory at {}",
            template.manifest.id,
            files_dir.display()
        )));
    }
    std::fs::create_dir_all(dest)
        .map_err(|e| TmError::Io(format!("creating {}: {e}", dest.display())))?;
    copy_and_substitute(&files_dir, &files_dir, dest, &effective)
}

/// Resolve every declared [`crate::manifest::ParamSpec`] to its effective value: the caller's
/// value if supplied, else the declared default. A parameter with neither is a hard error —
/// `apply` never silently substitutes an empty string for a value the template says it needs.
fn effective_params(
    template: &Template,
    params: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut effective = BTreeMap::new();
    for spec in &template.manifest.params {
        let value = params
            .get(&spec.name)
            .cloned()
            .or_else(|| spec.default.clone());
        match value {
            Some(v) => {
                effective.insert(spec.name.clone(), v);
            }
            None => {
                return Err(TmError::invariant(format!(
                    "template {:?} requires param {:?}, which was not supplied and has no default",
                    template.manifest.id, spec.name
                )));
            }
        }
    }
    Ok(effective)
}

/// Replace every `{{name}}` in `text` with `params[name]`, for every declared param — a plain
/// literal-substring replace, not a template engine: the starter set's needs (a crate name, a
/// port, a description) don't warrant pulling one in.
fn substitute(text: &str, params: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for (name, value) in params {
        out = out.replace(&format!("{{{{{name}}}}}"), value);
    }
    out
}

fn copy_and_substitute(
    root: &Path,
    dir: &Path,
    dest_root: &Path,
    params: &BTreeMap<String, String>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| TmError::Io(format!("reading {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| TmError::Io(e.to_string()))?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| TmError::invariant("walked file escaped its template root"))?;
        let out_path = dest_root.join(rel);
        if path.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| TmError::Io(format!("creating {}: {e}", out_path.display())))?;
            copy_and_substitute(root, &path, dest_root, params)?;
        } else {
            let bytes = std::fs::read(&path)
                .map_err(|e| TmError::Io(format!("reading {}: {e}", path.display())))?;
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| TmError::Io(format!("creating {}: {e}", parent.display())))?;
            }
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    let substituted = substitute(text, params);
                    std::fs::write(&out_path, substituted)
                        .map_err(|e| TmError::Io(format!("writing {}: {e}", out_path.display())))?;
                }
                Err(_) => {
                    // Binary content (none in the starter set, but a future template might ship
                    // one): copied verbatim, since substitution only makes sense for text.
                    std::fs::write(&out_path, &bytes)
                        .map_err(|e| TmError::Io(format!("writing {}: {e}", out_path.display())))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ParamSpec, ParamType, TemplateManifest};

    fn fixture_template(dir: &Path) -> Template {
        std::fs::create_dir_all(dir.join("files/src")).expect("mkdir");
        std::fs::write(
            dir.join("files/Cargo.toml"),
            "[package]\nname = \"{{project_name}}\"\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("files/src/main.rs"),
            "// {{description}}\nfn main() {}\n",
        )
        .expect("write");
        Template {
            manifest: TemplateManifest {
                id: "fixture".to_string(),
                version: "0.1.0".to_string(),
                license: None,
                tags: vec![],
                params: vec![
                    ParamSpec {
                        name: "project_name".to_string(),
                        param_type: ParamType::String,
                        default: None,
                        description: String::new(),
                    },
                    ParamSpec {
                        name: "description".to_string(),
                        param_type: ParamType::String,
                        default: Some("a scaffolded crate".to_string()),
                        description: String::new(),
                    },
                ],
                checksum: String::new(),
            },
            root: dir.to_path_buf(),
        }
    }

    #[test]
    fn substitutes_params_and_falls_back_to_defaults() {
        let src = tempfile::tempdir().expect("tempdir");
        let template = fixture_template(src.path());
        let dest = tempfile::tempdir().expect("tempdir");

        let mut params = BTreeMap::new();
        params.insert("project_name".to_string(), "demo".to_string());
        apply(&template, &params, dest.path()).expect("applies");

        let cargo_toml = std::fs::read_to_string(dest.path().join("Cargo.toml")).expect("read");
        assert!(cargo_toml.contains("name = \"demo\""));
        let main_rs = std::fs::read_to_string(dest.path().join("src/main.rs")).expect("read");
        assert!(main_rs.contains("a scaffolded crate"));
    }

    #[test]
    fn missing_required_param_is_an_error() {
        let src = tempfile::tempdir().expect("tempdir");
        let template = fixture_template(src.path());
        let dest = tempfile::tempdir().expect("tempdir");

        let err = apply(&template, &BTreeMap::new(), dest.path()).unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn missing_files_dir_is_an_error() {
        let src = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            src.path().join("manifest.toml"),
            "id = \"t\"\nversion = \"0.1.0\"\n",
        )
        .expect("write");
        let template = Template::load(src.path()).expect("loads");
        let dest = tempfile::tempdir().expect("tempdir");
        let err = apply(&template, &BTreeMap::new(), dest.path()).unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }
}
