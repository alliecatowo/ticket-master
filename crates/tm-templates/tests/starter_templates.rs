//! CI-runnable proof that every starter template (`templates/starter/*`, `SPEC.md` §27.3)
//! actually works: scaffold each into a fresh tempdir with representative parameter values, then
//! run its `verify.toml` for real, through the same `tm_context::command::run` path production
//! code uses. Deliberately not mocked — a template that does not build is a bug in the template,
//! and the only way to catch that is to actually build it (`SPEC.md` §27.2). `--offline` in each
//! `verify.toml` keeps this test network-free and deterministic, per this workspace's hygiene
//! rule against network access in tests: every dependency each starter template needs is already
//! warm in the local cargo registry cache, since this workspace's own crates already depend on
//! the same ratatui/axum/tokio/crossterm versions.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tm_templates::verify::{self, MemoryCommandCache, ProcessCommandExecutor};
use tm_templates::{apply, Template};
use tm_types::{Authority, CounterIds, FixedClock, ParticipantId, PatternSet, ShellAuthority};

/// `templates/starter/<name>`, resolved relative to this crate's own manifest directory so the
/// test works regardless of the caller's current directory.
fn starter_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("templates")
        .join("starter")
        .join(name)
}

/// Just enough authority for `verify.toml`'s `cargo build`/`cargo test` checks — mirrors the
/// `ShellAuthority` shape a real ticket's authority would carry, not `Authority::root()`, since
/// `SPEC.md` §27.4 treats a template's `verify.toml` as untrusted input that runs under whatever
/// authority the caller grants, not implicit full authority.
fn cargo_authority() -> Authority {
    Authority {
        shell: ShellAuthority {
            enabled: true,
            allow: PatternSet::parse(["cargo*"]).expect("valid shell pattern"),
            deny: PatternSet::empty(),
            pty: false,
        },
        ..Authority::none()
    }
}

/// Load `templates/starter/<name>`, apply it into a fresh tempdir with `params`, then run its
/// `verify.toml` for real and assert every check passed.
fn scaffold_and_verify(name: &str, params: BTreeMap<String, String>) {
    let dir = starter_dir(name);
    let template =
        Template::load(&dir).unwrap_or_else(|e| panic!("loading starter template {name}: {e}"));

    let dest = tempfile::tempdir().expect("tempdir");
    apply(&template, &params, dest.path())
        .unwrap_or_else(|e| panic!("applying starter template {name}: {e}"));

    let verify_spec = verify::load(&template.verify_path())
        .unwrap_or_else(|e| panic!("loading verify.toml for {name}: {e}"));
    assert!(
        !verify_spec.checks.is_empty(),
        "starter template {name} ships a verify.toml with zero checks"
    );

    let cache = MemoryCommandCache::new();
    let executor = ProcessCommandExecutor;
    let clock = FixedClock::epoch();
    let ids = CounterIds::new();
    let actor = ParticipantId::system();

    let report = verify::run(
        &verify_spec,
        dest.path(),
        &cargo_authority(),
        &cache,
        &executor,
        &clock,
        &ids,
        &actor,
    )
    .unwrap_or_else(|e| panic!("verifying starter template {name}: {e}"));

    assert_eq!(report.outcomes.len(), verify_spec.checks.len());
    for outcome in &report.outcomes {
        assert_eq!(
            outcome.result.exit_code, 0,
            "check {:?} of starter template {name} exited non-zero",
            outcome.name
        );
    }
}

#[test]
fn starter_ratatui_scaffolds_and_verifies() {
    let mut params = BTreeMap::new();
    params.insert("project_name".to_string(), "demo-ratatui".to_string());
    params.insert(
        "description".to_string(),
        "Demo ratatui scaffold from tm-templates' CI proof".to_string(),
    );
    scaffold_and_verify("ratatui", params);
}

#[test]
fn starter_axum_scaffolds_and_verifies() {
    let mut params = BTreeMap::new();
    params.insert("project_name".to_string(), "demo-axum".to_string());
    params.insert(
        "description".to_string(),
        "Demo axum scaffold from tm-templates' CI proof".to_string(),
    );
    params.insert("port".to_string(), "9090".to_string());
    scaffold_and_verify("axum", params);
}

#[test]
fn starter_rust_lib_scaffolds_and_verifies() {
    let mut params = BTreeMap::new();
    params.insert("project_name".to_string(), "demo-rust-lib".to_string());
    params.insert(
        "description".to_string(),
        "Demo rust-lib scaffold from tm-templates' CI proof".to_string(),
    );
    scaffold_and_verify("rust-lib", params);
}

/// The repo-root `templates/templates.toml` registry pins each starter template's content
/// checksum; this test is the tripwire that catches drift the moment a starter template's files
/// change without `templates.toml` being updated to match (`SPEC.md` §27.4).
#[test]
fn registry_pins_match_starter_template_contents() {
    let registry_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("templates")
        .join("templates.toml");
    let base_dir = registry_path.parent().expect("has a parent").to_path_buf();
    let registry =
        tm_templates::TemplateRegistry::load(&registry_path).expect("loads templates.toml");
    assert_eq!(
        registry.entries.len(),
        3,
        "expected all three starter templates registered"
    );

    for (id, result) in registry.resolve_all(&base_dir) {
        result.unwrap_or_else(|e| panic!("resolving registered template {id:?}: {e}"));
    }
}
