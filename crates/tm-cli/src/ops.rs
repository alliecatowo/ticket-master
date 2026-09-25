//! The `docs`, `provider`, `harness`, `bench`, `mirror`, `templates`, and `events` command
//! groups: project operations that sit beside the ticket graph rather than inside it.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use crate::args::{
    BenchCommand, BenchCompareArgs, BenchRunArgs, DocsCommand, EventsCommand, EventsReplayArgs,
    EventsShowArgs, EventsTailArgs, HarnessCommand, HarnessPromoteArgs, HarnessSetArgs,
    MirrorCommand, MirrorLinkArgs, ProviderCommand, ProviderDefaultArgs, ProviderTestArgs,
    TemplatesCommand, TemplatesShowArgs,
};
use crate::bench_report;
use crate::project::Project;
use crate::render::{Renderer, Table};
use crate::replay_diff;
use tm_types::Role;

/// Dispatch one [`DocsCommand`].
///
/// # IMPL
/// Match `cmd` to `docs_list`/`docs_check`/`docs_reconcile`.
pub fn dispatch_docs(
    cmd: &DocsCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        DocsCommand::List => docs_list(project, renderer),
        DocsCommand::Check => docs_check(project, renderer),
        DocsCommand::Reconcile => docs_reconcile(project, renderer),
    }
}

/// Load `docs/.tmdocs.toml` (if present) and build a [`tm_docs::DocRegistry`] from its declared
/// entries, registering any not-yet-seen doc into `tm-core`'s `docs`/`doc_provenance` tables via
/// [`tm_core::Store::register_doc`] as a side effect. Every `docs` verb below calls this, so
/// `Store`'s durable record of "what's registered" stays synced with the on-disk declaration by
/// the time any of them read it back — this is the "discovery" half of `SPEC.md` §9 that no
/// caller performed before B-06 (see that item's audit entry).
///
/// `register_doc` is idempotent (the materializer upserts, per its own doc comment), so calling
/// this from a read verb like `docs list`/`docs check` is safe to repeat; it is not a hidden
/// side effect either, since [`docs_list`]'s `--json` output reports how many docs this call
/// registered.
///
/// Returns the populated registry plus the number of docs registered by *this* call. When
/// `docs/.tmdocs.toml` does not exist, returns an empty registry and zero — legitimately empty,
/// not faked (see `SPEC.md` §9 / this crate's B-06 audit entry on why an empty result here is
/// correct rather than a bug to paper over).
fn load_and_sync_doc_registry(
    project: &Project,
) -> tm_types::Result<(tm_docs::DocRegistry, usize)> {
    let tmdocs_path = project
        .root
        .join("docs")
        .join(tm_docs::registry::TMDOCS_TOML);
    let mut registry = tm_docs::DocRegistry::new();
    if !tmdocs_path.is_file() {
        return Ok((registry, 0));
    }
    let contents = fs::read_to_string(&tmdocs_path).map_err(|e| {
        tm_types::TmError::storage(format!("Failed to read {}: {e}", tmdocs_path.display()))
    })?;
    let declared = tm_docs::registry::parse_tmdocs_toml(&contents)?;

    // `docs.id` in `tm-core`'s persisted table is the doc's *path* (`Store::register_doc`'s own
    // doc comment), not `tm-docs`' short slug id, so the already-registered check keys on path,
    // and doubles as the lookup this function needs to give an already-registered doc its
    // persisted state/last_verified back (docs-persist-state-across-invocations) instead of the
    // freshly-discovered `Unverified` default every declared doc got before.
    let persisted: std::collections::BTreeMap<String, tm_core::DocRow> = project
        .store
        .docs()?
        .into_iter()
        .map(|row| (row.id.clone(), row))
        .collect();

    let mut newly_registered = 0usize;
    for entry in declared.docs {
        let mut record = tm_docs::DocRecord::new(
            entry.id.clone(),
            entry.path.clone(),
            entry.mode,
            entry.derived_from.clone(),
        );
        match persisted.get(&entry.path) {
            Some(row) => {
                // Only a doc `tm-core` already has a row for gets its persisted standing;
                // `DocRecord::new`'s `Unverified` default above stands for anything not found
                // here, matching the "only newly discovered docs get Unverified" acceptance
                // check.
                if let Some(state) = tm_docs::registry::DocState::from_label(&row.state) {
                    record.state = state;
                }
                record.last_verified = row.last_verified;
            }
            None => {
                newly_registered += 1;
                // Only register docs `tm-core` has never seen — re-registering an
                // already-known doc on every `docs list`/`check`/`reconcile` call would spam
                // the event log with a `doc.registered` for no new information (the persisted
                // row above already reflects everything that event would carry).
                project
                    .store
                    .register_doc(entry.path.clone(), None, project.actor.clone())?;
            }
        }
        registry.insert(record);
    }
    Ok((registry, newly_registered))
}

/// Human/JSON label for a [`tm_docs::registry::DocMode`], purpose-built so `tm docs list` never
/// `{:?}`-debug-formats it directly.
fn doc_mode_label(mode: tm_docs::registry::DocMode) -> &'static str {
    use tm_docs::registry::DocMode;
    match mode {
        DocMode::Generated => "generated",
        DocMode::Maintained => "maintained",
        DocMode::Human => "human",
    }
}

/// Human/JSON label for a [`tm_docs::registry::DocState`], same reasoning as
/// [`doc_mode_label`].
fn doc_state_label(state: tm_docs::registry::DocState) -> &'static str {
    use tm_docs::registry::DocState;
    match state {
        DocState::Fresh => "fresh",
        DocState::Stale => "stale",
        DocState::Reconciling => "reconciling",
        DocState::Unverified => "unverified",
    }
}

/// `tm docs list`
///
/// # IMPL
/// Load the project's `tm_docs::registry::DocRegistry` from `docs/.tmdocs.toml`, syncing it into
/// `tm-core`'s `docs`/`doc_provenance` tables; render each `DocRecord`'s id, mode, and
/// `DocState` as a table or JSON.
pub fn docs_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let (registry, newly_registered) = load_and_sync_doc_registry(project)?;

    if renderer.is_json() {
        let docs: Vec<_> = registry
            .list()
            .iter()
            .map(|record| {
                serde_json::json!({
                    "id": record.id,
                    "mode": doc_mode_label(record.mode),
                    "state": doc_state_label(record.state),
                    "path": record.path,
                })
            })
            .collect();
        renderer.emit(
            &serde_json::json!({"docs": docs, "newly_registered": newly_registered}),
            "",
        )?;
    } else {
        let mut rows = Vec::new();
        for record in registry.list() {
            rows.push(vec![
                record.id.clone(),
                doc_mode_label(record.mode).to_string(),
                doc_state_label(record.state).to_string(),
            ]);
        }
        let table = Table::new(
            vec!["Id".to_string(), "Mode".to_string(), "State".to_string()],
            rows,
        );
        renderer.emit(&(), &table.render())?;
        if newly_registered > 0 {
            renderer.note(&format!(
                "Registered {newly_registered} doc(s) into the project store"
            ));
        }
    }
    Ok(())
}

/// `tm docs check`: exit non-zero when any doc is `Stale`.
///
/// # IMPL
/// Build a `tm_docs::assess::Assessor` over the registry and provenance index, run
/// `Assessor::check()`; on `Err`, this command's exit code must be non-zero — return the
/// `TmError` unchanged so `main.rs`'s exit-code mapping (domain failure, code 1) applies, this
/// function does not catch and swallow it.
///
/// No caller yet feeds this a real `ChangeSet` from a commit/index-update hook (`M-14`'s "nothing
/// triggers staleness" gap remains open), so a freshly discovered doc still starts `Unverified`
/// and this genuinely passes for it — not vacuously (the registry is real, non-empty, and would
/// fail the moment a doc were marked `Stale`), just with nothing yet driving that transition
/// automatically. An already-registered doc now reads back whatever state `Store::invalidate_doc`
/// (or a prior `tm docs reconcile`) persisted (docs-persist-state-across-invocations), so a doc
/// marked `Stale` by some other path does fail this check on a later invocation, even with no
/// live `ChangeSet` computed here yet.
pub fn docs_check(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let (registry, _) = load_and_sync_doc_registry(project)?;
    let records: Vec<tm_docs::DocRecord> = registry.list().into_iter().cloned().collect();
    let provenance = tm_docs::ProvenanceIndex::build(&records)?;
    let assessor = tm_docs::Assessor::new(registry, provenance);

    match assessor.check() {
        Ok(()) => {
            if !renderer.is_quiet() {
                renderer.note("All docs are fresh");
            }
            Ok(())
        }
        Err(e) => {
            if !renderer.is_quiet() {
                renderer.note(&format!("Doc staleness check failed: {e}"));
            }
            Err(e)
        }
    }
}

/// `tm docs reconcile`
///
/// # IMPL
/// For each `Stale` doc: `Generated` docs get a regeneration ticket via
/// `tm_docs::reconcile::ReconciliationTicket`/`Store::create_ticket`; `Maintained`/`Human` docs
/// get a review ticket, never a direct rewrite — attempting one is the hard
/// `TmError::AuthorityDenied` `tm-docs` documents, and this command must not attempt to work
/// around it. Render the tickets opened.
///
/// `tm_docs::reconcile::open_reconciliation` mints its own ticket id from the injected
/// `IdSource` for `tm-docs`' own bookkeeping, but `Store::create_ticket` always mints its *own*
/// id internally and accepts none from the caller (no shared batch-commit API between the two
/// crates — the same B-05/A-02 gap the audit names). Calling both would mint two different ids
/// for one ticket, so this creates the ticket via `Store::create_ticket` first and builds the
/// `ReconciliationKind`/state transition inline instead of calling `open_reconciliation`.
/// Human/JSON label for a [`tm_docs::reconcile::ReconciliationKind`], purpose-built so `tm docs
/// reconcile --json` never `{:?}`-debug-formats it directly.
fn reconciliation_kind_label(kind: tm_docs::reconcile::ReconciliationKind) -> &'static str {
    use tm_docs::reconcile::ReconciliationKind;
    match kind {
        ReconciliationKind::Regeneration => "regeneration",
        ReconciliationKind::Review => "review",
    }
}

/// Handle `tm docs reconcile`
pub fn docs_reconcile(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let (mut registry, _) = load_and_sync_doc_registry(project)?;
    let stale_ids: Vec<String> = registry
        .list()
        .iter()
        .filter(|r| r.state == tm_docs::DocState::Stale)
        .map(|r| r.id.clone())
        .collect();

    let mut opened = Vec::new();
    for doc_id in &stale_ids {
        let Some(record) = registry.get(doc_id) else {
            continue;
        };
        let kind = tm_docs::reconcile::ReconciliationKind::for_mode(record.mode);
        let verb = match kind {
            tm_docs::reconcile::ReconciliationKind::Regeneration => "Regenerate",
            tm_docs::reconcile::ReconciliationKind::Review => "Review",
        };
        let objective = format!("{verb} stale doc {doc_id} ({})", record.path);
        let doc_path = record.path.clone();

        // Only a `Review` ticket (a `Maintained`/`Human` doc, which the system may never
        // rewrite — see this function's own doc comment on `TmError::AuthorityDenied`) requires
        // a human executor; `Regeneration` keeps the ordinary fast-coder default, since
        // regenerating a `Generated` doc is exactly the kind of work an agent may do.
        let executor_requirements = match kind {
            tm_docs::reconcile::ReconciliationKind::Review => tm_core::ExecutorRequirements {
                human_required: true,
                ..crate::tickets::default_executor_requirements()
            },
            tm_docs::reconcile::ReconciliationKind::Regeneration => {
                crate::tickets::default_executor_requirements()
            }
        };

        let events = project.store.create_ticket(
            tm_core::TicketKind::Work,
            objective,
            None,
            None,
            tm_types::Authority::default(),
            Vec::new(),
            executor_requirements,
            Vec::new(),
            Vec::new(),
            tm_core::VerificationPolicy::Single,
            tm_types::Budget::unlimited(),
            crate::tickets::default_retry_policy(),
            0,
            project.actor.clone(),
        )?;

        let ticket_id = events
            .first()
            .and_then(|e| crate::tickets::event_ticket_id(&e.subject));

        // Persist the `Reconciling` transition through `Store::mark_doc_reconciling` — an event
        // plus its materialized `docs` row (`docs-persist-state-across-invocations`) — not just
        // the function-local `registry` below, so a fresh `tm docs list` in a later invocation
        // still reads `Reconciling` instead of re-deriving `Unverified`/`Stale` from scratch.
        project
            .store
            .mark_doc_reconciling(doc_path, ticket_id.clone(), project.actor.clone())?;

        if let Some(record) = registry.get_mut(doc_id) {
            record.state = tm_docs::DocState::Reconciling;
        }

        opened.push(serde_json::json!({
            "doc": doc_id,
            "kind": reconciliation_kind_label(kind),
            "ticket": ticket_id.map(|t| t.to_string()),
        }));
    }

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"opened": opened.len(), "tickets": opened}),
            "",
        )?;
    } else if opened.is_empty() {
        renderer.note("No stale docs to reconcile");
    } else {
        renderer.note(&format!("Opened {} reconciliation ticket(s)", opened.len()));
    }
    Ok(())
}

/// Dispatch one [`TemplatesCommand`].
///
/// # IMPL
/// Match `cmd` to `templates_list`/`templates_show`. Thin: this only surfaces `tm-templates`'
/// registry, it adds no logic of its own (mirrors `tm harness`/`tm bench`'s shape).
pub fn dispatch_templates(
    cmd: &TemplatesCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        TemplatesCommand::List => templates_list(project, renderer),
        TemplatesCommand::Show(args) => templates_show(args, project, renderer),
    }
}

/// The project's `templates.toml` and the directory `path` sources in it resolve relative to —
/// both the project root, mirroring `tm-docs`' `docs/.tmdocs.toml` convention: a project that
/// has not declared any templates gets an empty registry back, not an error.
fn templates_registry_path(project: &Project) -> std::path::PathBuf {
    project.root.join("templates.toml")
}

/// `tm templates list`
///
/// # IMPL
/// Load `<project root>/templates.toml` via [`tm_templates::TemplateRegistry::load`], resolve
/// every declared entry via [`tm_templates::TemplateRegistry::resolve_all`], and render each
/// entry's id, pinned version, and resolve status (`ok` or the error) as a table or JSON. A
/// resolution failure (drift, an unfetchable git/registry source, a missing directory) is shown
/// inline rather than silently dropping the entry from the list.
pub fn templates_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let registry_path = templates_registry_path(project);
    let registry = tm_templates::TemplateRegistry::load(&registry_path)?;
    let resolved = registry.resolve_all(&project.root);

    if renderer.is_json() {
        let templates: Vec<_> = registry
            .entries
            .iter()
            .zip(resolved.iter())
            .map(|(entry, (_, result))| {
                serde_json::json!({
                    "id": entry.id,
                    "version": entry.version,
                    "status": match result {
                        Ok(_) => "ok".to_string(),
                        Err(e) => format!("error: {e}"),
                    },
                })
            })
            .collect();
        renderer.emit(&serde_json::json!({"templates": templates}), "")?;
    } else if registry.entries.is_empty() {
        renderer.note(&format!(
            "No templates declared in {}",
            registry_path.display()
        ));
    } else {
        let mut rows = Vec::new();
        for (entry, (_, result)) in registry.entries.iter().zip(resolved.iter()) {
            let status = match result {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("error: {e}"),
            };
            rows.push(vec![entry.id.clone(), entry.version.clone(), status]);
        }
        let table = Table::new(
            vec![
                "Id".to_string(),
                "Version".to_string(),
                "Status".to_string(),
            ],
            rows,
        );
        renderer.emit(&(), &table.render())?;
    }
    Ok(())
}

/// Human/JSON label for a [`tm_templates::manifest::ParamType`], purpose-built so `tm templates
/// show` never `{:?}`-debug-formats it directly.
fn param_type_label(param_type: tm_templates::manifest::ParamType) -> &'static str {
    use tm_templates::manifest::ParamType;
    match param_type {
        ParamType::String => "string",
        ParamType::Bool => "bool",
        ParamType::Integer => "integer",
    }
}

/// `tm templates show <id>`
///
/// # IMPL
/// Resolve `args.id` via [`tm_templates::TemplateRegistry::resolve`] (surfacing
/// [`tm_types::TmError::NotFound`] for an undeclared id, or a resolve failure such as checksum
/// drift, unchanged) and render its manifest: version, license, tags, and params.
pub fn templates_show(
    args: &TemplatesShowArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let registry_path = templates_registry_path(project);
    let registry = tm_templates::TemplateRegistry::load(&registry_path)?;
    let template = registry.resolve(&args.id, &project.root)?;
    let manifest = &template.manifest;

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({
                "id": manifest.id,
                "version": manifest.version,
                "license": manifest.license,
                "tags": manifest.tags,
                "checksum": manifest.checksum,
                "params": manifest.params.iter().map(|p| serde_json::json!({
                    "name": p.name,
                    "type": param_type_label(p.param_type),
                    "default": p.default,
                    "description": p.description,
                })).collect::<Vec<_>>(),
            }),
            "",
        )?;
    } else {
        renderer.note(&format!("{} v{}", manifest.id, manifest.version));
        if let Some(license) = &manifest.license {
            renderer.note(&format!("license: {license}"));
        }
        renderer.note(&format!("tags: {}", manifest.tags.join(", ")));
        renderer.note(&format!("checksum: {}", manifest.checksum));
        if manifest.params.is_empty() {
            renderer.note("params: none");
        } else {
            let rows = manifest
                .params
                .iter()
                .map(|p| {
                    vec![
                        p.name.clone(),
                        param_type_label(p.param_type).to_string(),
                        p.default
                            .clone()
                            .unwrap_or_else(|| "(required)".to_string()),
                        p.description.clone(),
                    ]
                })
                .collect();
            let table = Table::new(
                vec![
                    "Param".to_string(),
                    "Type".to_string(),
                    "Default".to_string(),
                    "Description".to_string(),
                ],
                rows,
            );
            renderer.emit(&(), &table.render())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod templates_tests {
    use std::path::Path;
    use std::sync::Arc;

    use tempfile::TempDir;
    use tm_types::{Clock, CounterIds, FixedClock, IdSource, Timestamp};

    use super::*;

    fn open_test_project(root: &Path) -> Project {
        std::fs::create_dir_all(root.join(".tm")).expect("mkdir .tm");
        let clock: Arc<dyn Clock> =
            Arc::new(FixedClock::new(Timestamp::from_unix_seconds(1_000_000)));
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store = Arc::new(
            tm_core::Store::open_with(root, clock.clone(), ids.clone()).expect("open store"),
        );
        Project::for_test(root, store, clock, ids)
    }

    fn write_fixture_registry(root: &Path) -> String {
        std::fs::create_dir_all(root.join("t/files")).expect("mkdir");
        std::fs::write(
            root.join("t/manifest.toml"),
            "id = \"t\"\nversion = \"0.1.0\"\ntags = [\"rust\"]\n",
        )
        .expect("write manifest");
        std::fs::write(root.join("t/files/lib.rs"), "// nothing\n").expect("write file");
        let checksum = tm_templates::manifest::checksum_dir(&root.join("t")).expect("checksum");
        std::fs::write(
            root.join("templates.toml"),
            format!(
                "[[template]]\nid = \"t\"\nversion = \"0.1.0\"\nchecksum = \"{checksum}\"\n\n\
                 [template.source]\nkind = \"path\"\npath = \"t\"\n"
            ),
        )
        .expect("write registry");
        checksum
    }

    #[test]
    fn templates_list_with_no_registry_file_is_empty_not_an_error() {
        let dir = TempDir::new().expect("tempdir");
        let project = open_test_project(dir.path());
        let renderer = Renderer::from_flags(true, true, true);
        templates_list(&project, &renderer).expect("lists an empty registry cleanly");
    }

    #[test]
    fn templates_list_and_show_surface_a_real_registry() {
        let dir = TempDir::new().expect("tempdir");
        let project = open_test_project(dir.path());
        write_fixture_registry(dir.path());

        let renderer = Renderer::from_flags(true, true, true);
        templates_list(&project, &renderer).expect("lists the registered template");
        templates_show(
            &TemplatesShowArgs {
                id: "t".to_string(),
            },
            &project,
            &renderer,
        )
        .expect("shows the registered template's manifest");
    }

    #[test]
    fn templates_show_unknown_id_is_not_found() {
        let dir = TempDir::new().expect("tempdir");
        let project = open_test_project(dir.path());
        write_fixture_registry(dir.path());
        let renderer = Renderer::from_flags(true, true, true);

        let err = templates_show(
            &TemplatesShowArgs {
                id: "nope".to_string(),
            },
            &project,
            &renderer,
        )
        .unwrap_err();
        assert!(matches!(err, tm_types::TmError::NotFound { .. }));
    }

    #[test]
    fn templates_show_reports_checksum_drift() {
        let dir = TempDir::new().expect("tempdir");
        let project = open_test_project(dir.path());
        write_fixture_registry(dir.path());
        // Drift the on-disk template after the registry pinned it.
        std::fs::write(dir.path().join("t/files/lib.rs"), "// changed\n").expect("rewrite file");

        let renderer = Renderer::from_flags(true, true, true);
        let err = templates_show(
            &TemplatesShowArgs {
                id: "t".to_string(),
            },
            &project,
            &renderer,
        )
        .unwrap_err();
        assert!(matches!(err, tm_types::TmError::Conflict(_)));
    }
}

/// Dispatch one [`ProviderCommand`].
///
/// Providers are about this machine's credentials, not any one project, so every verb works
/// without one; `project` only supplies a project-specific role table to `list`.
pub async fn dispatch_provider(
    cmd: &ProviderCommand,
    project: Option<&Project>,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        ProviderCommand::List => provider_list(project, renderer).await,
        ProviderCommand::Detect => provider_detect(renderer).await,
        ProviderCommand::Status => provider_status(project, renderer).await,
        ProviderCommand::Default(args) => provider_default(args, project, renderer),
        ProviderCommand::Test(args) => provider_test(args, project, renderer).await,
    }
}

fn provider_default(
    args: &ProviderDefaultArgs,
    project: Option<&Project>,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let project = project.ok_or_else(|| {
        tm_types::TmError::parse(
            "`tm provider default` requires a project; run `tm init` first".to_string(),
        )
    })?;
    let path = project.state_dir.join("default-model.json");
    match args.spec.as_deref().map(str::trim) {
        None => {
            let saved = crate::agent::load_default_model(project)
                .map(|model| model.to_string())
                .unwrap_or_else(|| "(not set)".to_string());
            renderer.emit(
                &serde_json::json!({"model": saved}),
                &format!("Default model: {saved}"),
            )
        }
        Some(spec) if spec.eq_ignore_ascii_case("clear") => {
            match fs::remove_file(&path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
            renderer.emit(
                &serde_json::json!({"model": null}),
                "Cleared project default model.",
            )
        }
        Some(spec) => {
            let (provider, model) = spec
                .split_once('/')
                .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
                .ok_or_else(|| {
                    tm_types::TmError::parse("Expected `provider/model` or `clear`".to_string())
                })?;
            let known_providers = tm_provider::Registry::known_providers();
            let provider_info = known_providers
                .iter()
                .find(|known| known.id == provider)
                .ok_or_else(|| {
                    tm_types::TmError::parse(format!("Unknown provider `{provider}`"))
                })?;
            if !provider_info.is_configured() {
                return Err(tm_types::TmError::parse(format!(
                    "Provider `{provider}` is not configured in this environment; configure its required credentials first (see `tm provider detect`)"
                )));
            }
            fs::create_dir_all(&project.state_dir)?;
            let value = serde_json::json!({"model": format!("{provider}/{model}")});
            fs::write(&path, serde_json::to_vec(&value)?)?;
            renderer.emit(
                &value,
                &format!("Saved project default model: {provider}/{model}"),
            )
        }
    }
}

/// Human/JSON label for [`tm_provider::Availability`], shared by [`provider_list`] and
/// [`provider_detect`] so both commands describe the same four states the same way: `"ready"`
/// (safe to route traffic to right now), `"unreachable"` (configured, but a reachability probe
/// found nothing listening — today only reachable for the three local backends), `"unusable"`
/// (either a cloud backend whose configuration exists but cannot currently construct a usable
/// provider, e.g. Bedrock before SigV4 exists, or a local backend that answered the reachability
/// probe but has no model pulled yet), or `"not-configured"` (no required env var set).
fn availability_label(availability: tm_provider::Availability) -> &'static str {
    match availability {
        tm_provider::Availability::NotConfigured => "not-configured",
        tm_provider::Availability::ConfiguredButUnreachable => "unreachable",
        tm_provider::Availability::Unusable => "unusable",
        tm_provider::Availability::Ready => "ready",
    }
}

/// Load provider routing from `providers.toml`; read legacy role-shaped `harness.toml` only
/// when the new file is absent.
pub(crate) fn load_role_table(
    project: Option<&Project>,
) -> tm_types::Result<tm_provider::RoleTable> {
    match project {
        Some(project) => load_role_table_for_state_dir(&project.state_dir),
        None => Ok(tm_provider::RoleTable::default_table()),
    }
}

pub(crate) fn load_role_table_for_state_dir(
    state_dir: &std::path::Path,
) -> tm_types::Result<tm_provider::RoleTable> {
    let providers_path = state_dir.join("providers.toml");
    match providers_path.is_file().then_some(providers_path) {
        Some(path) => {
            let content = fs::read_to_string(&path).map_err(|e| {
                tm_types::TmError::storage(format!("Failed to read providers.toml: {e}"))
            })?;
            tm_provider::RoleTable::parse(&content)
                .map_err(|e| tm_types::TmError::parse(format!("Invalid providers.toml: {e}")))
        }
        None => {
            let legacy = state_dir.join("harness.toml");
            if legacy.is_file() {
                let path = legacy;
                let content = fs::read_to_string(&path).map_err(|e| {
                    tm_types::TmError::storage(format!("Failed to read legacy role config: {e}"))
                })?;
                if let Ok(table) = tm_provider::RoleTable::parse(&content) {
                    return Ok(table);
                }
            }
            Ok(tm_provider::RoleTable::default_table())
        }
    }
}

/// `tm provider list`
///
/// # IMPL
/// Load the project's `tm_provider::role_config::RoleTable` from `harness.toml`; render each
/// role's configured `RoleCandidate`s (provider, model, priority) as a table or JSON, alongside
/// each candidate's [`tm_provider::Availability`] (see [`availability_label`]) — resolved once
/// per *distinct* provider slug the table references, not once per row, since a slug can repeat
/// across many roles and resolving a local backend's availability costs a real (short-timeout)
/// network probe.
///
/// With no project, or a project without its own `harness.toml`, this lists the default table,
/// which is what every turn actually routes through in that case.
pub async fn provider_list(project: Option<&Project>, renderer: &Renderer) -> tm_types::Result<()> {
    let role_table = load_role_table(project)?;

    let known = tm_provider::Registry::known_providers();
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let mut availability_by_slug: BTreeMap<String, tm_provider::Availability> = BTreeMap::new();
    for role in Role::ALL {
        for candidate in role_table.candidates_for(role) {
            if availability_by_slug.contains_key(&candidate.provider) {
                continue;
            }
            let availability = match known.iter().find(|info| info.id == candidate.provider) {
                Some(info) => tm_provider::Registry::availability(info, clock.clone()).await,
                // A slug harness.toml names that this build of the crate doesn't recognize at
                // all: report it as unconfigured rather than panicking or silently dropping the
                // row.
                None => tm_provider::Availability::NotConfigured,
            };
            availability_by_slug.insert(candidate.provider.clone(), availability);
        }
    }

    if renderer.is_json() {
        let mut roles = Vec::new();
        for role in Role::ALL {
            for candidate in role_table.candidates_for(role) {
                roles.push(serde_json::json!({
                    "role": role.as_str(),
                    "provider": candidate.provider,
                    "model": candidate.model,
                    "concurrency": candidate.max_concurrency,
                    "availability": availability_label(availability_by_slug[&candidate.provider]),
                }));
            }
        }
        renderer.emit(&roles, "")?;
    } else if !renderer.is_quiet() {
        let mut rows = Vec::new();
        for role in Role::ALL {
            for candidate in role_table.candidates_for(role) {
                rows.push(vec![
                    role.as_str().to_string(),
                    candidate.provider.to_string(),
                    candidate.model.to_string(),
                    candidate.max_concurrency.to_string(),
                    availability_label(availability_by_slug[&candidate.provider]).to_string(),
                ]);
            }
        }
        let table = Table::new(
            vec![
                "Role".to_string(),
                "Provider".to_string(),
                "Model".to_string(),
                "Concurrency".to_string(),
                "Availability".to_string(),
            ],
            rows,
        );
        renderer.emit(&(), &table.render())?;
    }
    Ok(())
}

/// `tm provider detect`
///
/// Inspects the environment (and, for the three zero-signup local backends, makes a short-timeout
/// reachability probe — see [`tm_provider::Registry::availability`]) and reports, per known
/// backend, its slug, display name, [`tm_provider::Availability`], and static capabilities —
/// never any env var's value, only which *names* [`tm_provider::ProviderInfo`] declares and
/// whether each is set. No key material is ever read out of the environment here:
/// [`tm_provider::ProviderInfo::is_configured`] only calls `std::env::var(..).is_ok()`, and the
/// reachability probe sends no credentials at all (the three local backends need none).
pub async fn provider_detect(renderer: &Renderer) -> tm_types::Result<()> {
    let known = tm_provider::Registry::known_providers();
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let mut with_availability = Vec::with_capacity(known.len());
    for info in &known {
        let availability = tm_provider::Registry::availability(info, clock.clone()).await;
        with_availability.push((info, availability));
    }

    if renderer.is_json() {
        let rows: Vec<_> = with_availability
            .iter()
            .map(|(info, availability)| {
                serde_json::json!({
                    "id": info.id,
                    "display_name": info.display_name,
                    // Renamed from a plain `"configured"` deliberately: for the three local
                    // backends this is `true` even with nothing listening (their env vars are
                    // all optional), so a name that could be misread as "usable" would
                    // reintroduce the exact lie `"availability"` exists to correct. This is
                    // env-var presence only -- see `"availability"` for the honest answer.
                    "env_vars_present": info.is_configured(),
                    "availability": availability_label(*availability),
                    "env_vars": info.env_vars.iter().map(|v| serde_json::json!({
                        "name": v.name,
                        "required": v.required,
                        "description": v.description,
                    })).collect::<Vec<_>>(),
                    "capabilities": {
                        "completion": info.capabilities.completion,
                        "embedding": info.capabilities.embedding,
                        "streaming": info.capabilities.streaming,
                        "tool_use": info.capabilities.tool_use,
                        "vision": info.capabilities.vision,
                    },
                })
            })
            .collect();
        renderer.emit(&rows, "")?;
    } else if !renderer.is_quiet() {
        let mut rows = Vec::new();
        for (info, availability) in &with_availability {
            rows.push(vec![
                info.id.to_string(),
                info.display_name.to_string(),
                availability_label(*availability).to_string(),
                info.env_vars
                    .iter()
                    .filter(|v| v.required)
                    .map(|v| v.name)
                    .collect::<Vec<_>>()
                    .join(", "),
            ]);
        }
        let table = Table::new(
            vec![
                "Id".to_string(),
                "Name".to_string(),
                "Availability".to_string(),
                "Required env".to_string(),
            ],
            rows,
        );
        renderer.emit(&(), &table.render())?;
    }
    Ok(())
}

/// What this process can honestly say about live fabric state. A `Fabric`'s breaker/quota/EWMA
/// state ([`tm_provider::FabricState`]) lives only in the memory of the process that built it and
/// is never persisted or shared, so a standalone `tm provider status` has no call history to
/// report — saying so is the truthful answer, not a placeholder.
const PROVIDER_LIVE_STATE_NOTE: &str = "breaker/quota/latency state lives only inside a running \
     tm process's fabric and is not persisted; this command has no call history to report. Use \
     `tm provider test` for a real round-trip";

/// Which roles in `table` name `provider_id` as a candidate, as role keys (`"coder.fast"`, ...).
fn roles_routed_to(table: &tm_provider::RoleTable, provider_id: &str) -> Vec<&'static str> {
    Role::ALL
        .into_iter()
        .filter(|role| {
            table
                .candidates_for(*role)
                .iter()
                .any(|c| c.provider == provider_id)
        })
        .map(|role| role.as_str())
        .collect()
}

/// `tm provider status`
///
/// Reports, per provider [`tm_provider::Registry::known_providers`] lists (the same set and the
/// same [`tm_provider::Availability`] logic as [`provider_detect`]): its availability, whether
/// its required env vars are present, whether the fabric tm's real turn path builds
/// ([`crate::agent::build_fabric`], over [`tm_provider::RoleTable::default_table`]) actually
/// registers it, and which roles in that table route to it. Any provider the turn-path fabric
/// registers that isn't in the known list (e.g. the test-only `mock`) gets a row too. If building
/// that fabric fails (typically: neither DevPass nor `ANTHROPIC_API_KEY` configured), the error
/// is reported rather than hidden.
///
/// It does **not** report breaker/quota/latency state: that lives only in a running process's
/// in-memory fabric and is never persisted (see [`PROVIDER_LIVE_STATE_NOTE`]). No completion is
/// sent; the only network I/O is [`tm_provider::Registry::availability`]'s short-timeout probe of
/// the three local backends, exactly as `tm provider detect` does.
pub async fn provider_status(
    project: Option<&Project>,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let table = load_role_table(project)?;
    let (registered, fabric_error) = match project.map_or_else(
        || crate::agent::build_fabric(clock.clone()),
        |project| crate::agent::build_fabric_for_project(project, clock.clone()),
    ) {
        Ok(fabric) => (fabric.provider_ids(), None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };

    let known = tm_provider::Registry::known_providers();
    let mut rows = Vec::with_capacity(known.len());
    for info in &known {
        let availability = tm_provider::Registry::availability(info, clock.clone()).await;
        rows.push(serde_json::json!({
            "id": info.id,
            "display_name": info.display_name,
            "availability": availability_label(availability),
            "env_vars_present": info.is_configured(),
            "in_turn_fabric": registered.iter().any(|id| id == info.id),
            "roles": roles_routed_to(&table, info.id),
        }));
    }
    for id in registered
        .iter()
        .filter(|id| !known.iter().any(|info| info.id == id.as_str()))
    {
        rows.push(serde_json::json!({
            "id": id,
            "display_name": id,
            "availability": "ready",
            "env_vars_present": serde_json::Value::Null,
            "in_turn_fabric": true,
            "roles": roles_routed_to(&table, id),
        }));
    }

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({
                "providers": rows,
                "turn_fabric_error": fabric_error,
                "live_state": serde_json::Value::Null,
                "live_state_note": PROVIDER_LIVE_STATE_NOTE,
            }),
            "",
        )?;
        return Ok(());
    }

    if !renderer.is_quiet() {
        let table_rows = rows
            .iter()
            .map(|row| {
                let roles = row["roles"]
                    .as_array()
                    .map(|r| {
                        r.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                vec![
                    row["id"].as_str().unwrap_or_default().to_string(),
                    row["availability"].as_str().unwrap_or_default().to_string(),
                    if row["in_turn_fabric"].as_bool() == Some(true) {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                    if roles.is_empty() {
                        "-".to_string()
                    } else {
                        roles
                    },
                ]
            })
            .collect();
        let rendered = Table::new(
            vec![
                "Id".to_string(),
                "Availability".to_string(),
                "In turn fabric".to_string(),
                "Default-table roles".to_string(),
            ],
            table_rows,
        )
        .render();
        let mut human = rendered;
        if let Some(e) = &fabric_error {
            human.push_str(&format!("\nturn-path fabric could not be built: {e}"));
        }
        human.push_str(&format!("\nnote: {PROVIDER_LIVE_STATE_NOTE}"));
        renderer.emit(&(), &human)?;
    }
    Ok(())
}

/// The fixed, tiny request `tm provider test` sends to each provider. Non-streaming, matching
/// what `tm-agent`'s turn loop sends, so a passing test exercises the same response-parsing path
/// a real turn does. `max_tokens` leaves a reasoning model room to think and still say "OK" (at 64
/// it spent everything thinking and replied with nothing); one that runs out anyway comes back as
/// `stop_reason: max_tokens`, which still counts as a successful round-trip (the provider
/// answered and the reply parsed).
fn provider_probe_request() -> tm_provider::CompletionRequest {
    tm_provider::CompletionRequest {
        system: None,
        messages: vec![tm_provider::Message {
            role: tm_provider::MessageRole::User,
            content: vec![tm_provider::ContentBlock::Text {
                text: "Reply with the single word OK.".to_string(),
            }],
        }],
        tools: vec![],
        max_tokens: 1_024,
        temperature: None,
        stop_sequences: vec![],
        stream: false,
        n: 1,
        model: None,
    }
}

/// One provider's `tm provider test` result: the `--json` array element shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct ProviderTestOutcome {
    /// The provider slug tested.
    pub provider: String,
    /// `"ok"` or `"error"`.
    pub status: &'static str,
    /// Wall-clock time for the call, measured with the project's clock.
    pub latency_ms: u64,
    /// The model the provider reports actually serving the request (`Completion::model`); `None`
    /// on failure.
    pub model: Option<String>,
    /// Why the reply stopped (`end_turn`, `max_tokens`, ...); `None` on failure.
    pub stop_reason: Option<tm_provider::StopReason>,
    /// The reply's text (possibly empty, e.g. a reasoning model cut off by `max_tokens`); `None`
    /// on failure.
    pub reply: Option<String>,
    /// The provider error on failure; `None` on success.
    pub error: Option<String>,
}

impl ProviderTestOutcome {
    fn is_ok(&self) -> bool {
        self.status == "ok"
    }

    /// One human-readable line for plain (non-`--json`) output.
    fn human_line(&self) -> String {
        match &self.error {
            None => {
                let stop = self
                    .stop_reason
                    .and_then(|s| serde_json::to_value(s).ok())
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".to_string());
                format!(
                    "{}: ok in {} ms (model {}, stop {}, reply {:?})",
                    self.provider,
                    self.latency_ms,
                    self.model.as_deref().unwrap_or("unknown"),
                    stop,
                    self.reply.as_deref().unwrap_or_default()
                )
            }
            Some(error) => format!(
                "{}: error after {} ms: {}",
                self.provider, self.latency_ms, error
            ),
        }
    }
}

/// Send [`provider_probe_request`] to one provider and time it with `clock`.
async fn test_one_provider(
    id: &str,
    provider: &dyn tm_provider::Provider,
    clock: &dyn tm_types::Clock,
) -> ProviderTestOutcome {
    let started = clock.now();
    let result = provider.complete(provider_probe_request()).await;
    let latency_ms = u64::try_from(clock.now().millis_since(started).max(0)).unwrap_or(0);
    match result {
        Ok(completion) => {
            let first = completion.candidates.first();
            let reply = first
                .map(|c| {
                    c.content
                        .iter()
                        .filter_map(|b| match b {
                            tm_provider::ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            ProviderTestOutcome {
                provider: id.to_string(),
                status: "ok",
                latency_ms,
                model: Some(completion.model.model.clone()),
                stop_reason: first.map(|c| c.stop_reason),
                reply: Some(reply.trim().to_string()),
                error: None,
            }
        }
        Err(e) => ProviderTestOutcome {
            provider: id.to_string(),
            status: "error",
            latency_ms,
            model: None,
            stop_reason: None,
            reply: None,
            error: Some(e.to_string()),
        },
    }
}

/// Why `name` can't be tested: unknown to this build, known but not configured (naming the
/// missing env vars, never their values), or configured but not something tm's turn-path fabric
/// registers. `registered` is the turn-path fabric's provider ids (empty if it couldn't be built).
fn untestable_provider_error(
    name: &str,
    known: &[tm_provider::ProviderInfo],
    registered: &[String],
    fabric_error: Option<&str>,
) -> tm_types::TmError {
    let Some(info) = known.iter().find(|info| info.id.eq_ignore_ascii_case(name)) else {
        let names = known.iter().map(|info| info.id).collect::<Vec<_>>();
        return tm_types::TmError::Provider(format!(
            "unknown provider `{name}`; known providers: {}",
            names.join(", ")
        ));
    };
    if !info.is_configured() {
        let missing = info
            .env_vars
            .iter()
            .filter(|v| v.required && std::env::var(v.name).is_err())
            .map(|v| v.name)
            .collect::<Vec<_>>();
        return tm_types::TmError::Provider(format!(
            "provider `{}` is not configured: {} not set",
            info.id,
            missing.join(", ")
        ));
    }
    let wired = if registered.is_empty() {
        "none".to_string()
    } else {
        registered.join(", ")
    };
    let why = match fabric_error {
        Some(e) => format!("the turn-path fabric could not be built: {e}"),
        None => format!(
            "tm's turn path does not use it (the turn-path fabric registers: {wired}), so there \
             is nothing real to test"
        ),
    };
    tm_types::TmError::Provider(format!("provider `{}` is configured but {why}", info.id))
}

/// Test every provider `fabric` has registered, or only `only`. Calls each [`tm_provider::Provider`]
/// directly via [`tm_provider::Fabric::provider`], deliberately bypassing routing, breakers and
/// quota: this tests the provider, not the role table. Per-provider failures are results, not
/// errors; the `Err` cases are "nothing to test" (`only` not registered, or an empty fabric).
/// `only` matches case-insensitively, like `tm auth`.
pub(crate) async fn run_provider_tests(
    fabric: &tm_provider::Fabric,
    only: Option<&str>,
    known: &[tm_provider::ProviderInfo],
    clock: &dyn tm_types::Clock,
) -> tm_types::Result<Vec<ProviderTestOutcome>> {
    let registered = fabric.provider_ids();
    let targets: Vec<String> = match only {
        Some(name) => match registered.iter().find(|id| id.eq_ignore_ascii_case(name)) {
            Some(id) => vec![id.clone()],
            None => return Err(untestable_provider_error(name, known, &registered, None)),
        },
        None if registered.is_empty() => {
            return Err(tm_types::TmError::Provider(
                "no provider is registered in tm's turn-path fabric; nothing to test".to_string(),
            ))
        }
        None => registered.clone(),
    };

    let mut outcomes = Vec::with_capacity(targets.len());
    for id in &targets {
        let Some(provider) = fabric.provider(id) else {
            continue;
        };
        outcomes.push(test_one_provider(id, provider.as_ref(), clock).await);
    }
    Ok(outcomes)
}

/// `Err` naming how many of `outcomes` failed, if any did — what makes `tm provider test` exit
/// non-zero after it has already printed every result.
fn provider_test_verdict(outcomes: &[ProviderTestOutcome]) -> tm_types::Result<()> {
    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|o| !o.is_ok())
        .map(|o| o.provider.as_str())
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(tm_types::TmError::Provider(format!(
            "{} of {} provider test(s) failed: {}",
            failed.len(),
            outcomes.len(),
            failed.join(", ")
        )))
    }
}

/// `tm provider test [<provider>]`
///
/// Sends one tiny real completion ([`provider_probe_request`]) through each provider tm's real
/// turn path registers ([`crate::agent::build_fabric`] — DevPass when fully configured, Anthropic
/// when `ANTHROPIC_API_KEY` is set), or only `<provider>`, and reports per provider: ok/error, the
/// measured latency, the model that actually served the reply, the stop reason, the reply text,
/// and the error text on failure. `--json` emits an array of [`ProviderTestOutcome`]s.
///
/// This makes real, billed network calls. It exits non-zero if any tested provider failed, and
/// errors (non-zero, nothing tested) for a provider name that is unknown, not configured, or not
/// used by the turn path — never reporting such a name as reachable.
pub async fn provider_test(
    args: &ProviderTestArgs,
    project: Option<&Project>,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let known = tm_provider::Registry::known_providers();
    let fabric = match project.map_or_else(
        || crate::agent::build_fabric(clock.clone()),
        |project| crate::agent::build_fabric_for_project(project, clock.clone()),
    ) {
        Ok(fabric) => fabric,
        Err(e) => {
            return Err(match &args.provider {
                // Explain the named provider specifically (unknown / which env vars are missing)
                // rather than only surfacing the turn path's generic construction error.
                Some(name) => untestable_provider_error(name, &known, &[], Some(&e.to_string())),
                None => tm_types::TmError::Provider(format!(
                    "no provider is configured for tm's turn path, nothing to test: {e}"
                )),
            });
        }
    };

    let outcomes =
        run_provider_tests(&fabric, args.provider.as_deref(), &known, clock.as_ref()).await?;
    let human = provider_test_human(&outcomes, renderer.is_quiet());
    if renderer.is_json() || human.is_some() {
        renderer.emit(&outcomes, human.as_deref().unwrap_or(""))?;
    }
    provider_test_verdict(&outcomes)
}

/// Format human-readable output for `provider test`, filtering "ok" lines in quiet mode.
/// Returns `None` if there's nothing to show (all "ok" in quiet mode).
fn provider_test_human(outcomes: &[ProviderTestOutcome], quiet: bool) -> Option<String> {
    let lines: Vec<String> = if quiet {
        // In quiet mode, suppress "ok" lines but keep errors.
        outcomes
            .iter()
            .filter(|o| !o.is_ok())
            .map(ProviderTestOutcome::human_line)
            .collect()
    } else {
        outcomes
            .iter()
            .map(ProviderTestOutcome::human_line)
            .collect()
    };

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Dispatch one [`HarnessCommand`].
pub fn dispatch_harness(
    cmd: &HarnessCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        HarnessCommand::Show => harness_show(project, renderer),
        HarnessCommand::Set(args) => harness_set(args, project, renderer),
        HarnessCommand::Epochs => harness_epochs(project, renderer),
        HarnessCommand::Promote(args) => harness_promote(args, project, renderer),
        HarnessCommand::ReplayDiff(args) => replay_diff::run_replay_diff(args, renderer),
    }
}

/// `tm harness show`
///
/// # IMPL
/// Load the current `HarnessEpoch`'s `HarnessConfig` from `EpochRegistry::current`; render its
/// sections (tool preferences, routing weights, context policy, role mapping, verification
/// defaults, command policy, editing policy) as labeled blocks or JSON.
pub fn harness_show(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _view = project.store.view()?;
    let harness_path = project.state_dir.join("harness.toml");
    let config = if harness_path.is_file() {
        let content = fs::read_to_string(&harness_path)
            .map_err(|e| tm_types::TmError::storage(format!("Failed to read harness.toml: {e}")))?;
        tm_harness::HarnessConfig::parse(&content)
            .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e}")))?
    } else {
        tm_harness::HarnessConfig::default()
    };

    if renderer.is_json() {
        renderer.emit(&config, "")?;
    } else {
        let output = toml::to_string_pretty(&config)
            .map_err(|e| tm_types::TmError::parse(format!("Failed to format config: {e}")))?;
        renderer.emit(&(), &output)?;
    }
    Ok(())
}

/// `tm harness set`
///
/// # IMPL
/// Parse `args.value` as TOML, apply it at `args.key`'s dotted path onto a clone of the current
/// `HarnessConfig`, validate it (`HarnessConfig::validate`), then propose it as a new
/// `HarnessEpoch` (not applied live — promotion is a separate, explicit step per
/// `tm-harness`'s pinned-epoch safety property). Render the pending epoch number.
pub fn harness_set(
    args: &HarnessSetArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let _view = project.store.view()?;
    let harness_path = project.state_dir.join("harness.toml");
    let harness_content = if harness_path.is_file() {
        fs::read_to_string(&harness_path)
            .map_err(|e| tm_types::TmError::storage(format!("Failed to read harness.toml: {e}")))?
    } else {
        toml::to_string_pretty(&tm_harness::HarnessConfig::default())
            .map_err(|e| tm_types::TmError::parse(format!("Failed to serialize defaults: {e}")))?
    };
    let mut config = tm_harness::HarnessConfig::parse(&harness_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e}")))?;

    let mut wrapped: toml::Table = toml::from_str(&format!("value = {}", args.value))
        .map_err(|e| tm_types::TmError::parse(format!("Invalid TOML value: {e}")))?;
    let value = wrapped
        .remove("value")
        .ok_or_else(|| tm_types::TmError::parse("Invalid TOML value".to_string()))?;

    let config_str = toml::to_string(&config)
        .map_err(|e| tm_types::TmError::parse(format!("Failed to serialize config: {e}")))?;
    let mut doc = toml::from_str::<toml::Table>(&config_str).unwrap_or_default();

    let keys: Vec<&str> = args.key.split('.').collect();
    if !keys.is_empty() {
        let mut current = &mut doc;
        for key in &keys[..keys.len() - 1] {
            current = current
                .entry(key.to_string())
                .or_insert_with(|| toml::Value::Table(Default::default()))
                .as_table_mut()
                .ok_or_else(|| tm_types::TmError::parse("Invalid path".to_string()))?;
        }
        if let Some(last_key) = keys.last() {
            current.insert(last_key.to_string(), value);
        }
    }

    let new_config_str = toml::to_string_pretty(&doc)
        .map_err(|e| tm_types::TmError::parse(format!("Failed to serialize config: {e}")))?;

    config = tm_harness::HarnessConfig::parse(&new_config_str)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid updated config: {e}")))?;

    config
        .validate()
        .map_err(|e| tm_types::TmError::parse(format!("Validation failed: {e}")))?;

    let next_epoch = project
        .store
        .harness_epochs()?
        .iter()
        .map(|e| e.epoch)
        .max()
        .unwrap_or(0)
        + 1;
    fs::write(&harness_path, &new_config_str)?;
    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"status": "saved", "next_epoch": next_epoch}),
            "",
        )?;
    } else {
        renderer.note(&format!("Saved validated harness config. Promote with 'tm harness promote {next_epoch} --force' (or provide a benchmark report)."));
    }
    Ok(())
}

/// `tm harness epochs`
///
/// # IMPL
/// Read back every promoted epoch via [`tm_core::Store::harness_epochs`]; render each epoch's
/// number, promotion timestamp, and config hash as a table or JSON.
///
/// The genesis epoch (`0`) is never persisted (see [`tm_core::Store::harness_epochs`]'s doc
/// comment), so an empty result means nothing has ever been promoted — reported as such, not
/// synthesized as a fake `epoch 0` row.
pub fn harness_epochs(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let epochs = project.store.harness_epochs()?;

    if renderer.is_json() {
        let out: Vec<_> = epochs
            .iter()
            .map(|e| {
                let hash = tm_harness::HarnessConfig::parse(&e.harness_config)
                    .map(|c| c.config_hash().to_string())
                    .unwrap_or_else(|err| format!("unparseable: {err}"));
                serde_json::json!({
                    "epoch": e.epoch,
                    "promoted_at": e.ts.to_rfc3339(),
                    "hash": hash,
                })
            })
            .collect();
        renderer.emit(&out, "")?;
    } else if epochs.is_empty() {
        renderer.note("No epoch has ever been promoted (genesis epoch 0 is implicit)");
    } else {
        let rows = epochs
            .iter()
            .map(|e| {
                let hash = tm_harness::HarnessConfig::parse(&e.harness_config)
                    .map(|c| c.config_hash().to_string())
                    .unwrap_or_else(|err| format!("unparseable: {err}"));
                vec![e.epoch.to_string(), e.ts.to_rfc3339(), hash]
            })
            .collect();
        let table = Table::new(
            vec![
                "Epoch".to_string(),
                "Promoted at".to_string(),
                "Hash".to_string(),
            ],
            rows,
        );
        renderer.emit(&(), &table.render())?;
    }
    Ok(())
}

/// `tm harness promote`
///
/// # IMPL
/// Reconstruct the current [`tm_harness::EpochRegistry`] state from
/// [`tm_core::Store::harness_epochs`]'s last row (or the genesis default when none exists),
/// promote the on-disk `.tm/harness.toml` as the candidate via `EpochRegistry::promote` (which
/// consults `PromotionGate::evaluate`), then persist the outcome via
/// [`tm_core::Store::promote_epoch`]. `args.force` relaxes the gate to "no benchmark-gain check"
/// rather than silently bypassing it — this command has no authority-check mechanism of its own
/// to surface `TmError::AuthorityDenied` from (the CLI runs as whatever OS user invoked it, not
/// under a checked `Authority`), so `--force` is documented, not hidden, in its effect.
pub fn harness_promote(
    args: &HarnessPromoteArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let existing = project.store.harness_epochs()?;
    let current_number = existing.iter().map(|e| e.epoch).max().unwrap_or(0);
    let next_number = current_number + 1;
    if args.epoch != next_number {
        return Err(tm_types::TmError::invariant(format!(
            "cannot promote epoch {}: the next promotable epoch is {next_number}",
            args.epoch
        )));
    }

    let harness_path = project.state_dir.join("harness.toml");
    if !harness_path.is_file() {
        return Err(tm_types::TmError::parse(
            "No harness configuration found. Use `tm harness set <key> <value>` to create one before promoting an epoch.".to_string(),
        ));
    }
    let harness_content = fs::read_to_string(&harness_path)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to read harness.toml: {e}")))?;
    let candidate = tm_harness::HarnessConfig::parse(&harness_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e}")))?;

    // The current epoch this candidate is promoted from, reconstructed just well enough for
    // `EpochRegistry::promote` to compute `next_number`/apply the gate. `harness_epochs` has no
    // benchmark column (see `Store::harness_epochs`'s doc comment), so the reconstructed current
    // epoch's `benchmark` is always `None` — meaning the gate never has an automatic baseline;
    // one must be supplied via `--baseline` or the gate (unless `--force`) rejects.
    let current_epoch = match existing.last() {
        Some(row) => {
            let config = tm_harness::HarnessConfig::parse(&row.harness_config).map_err(|e| {
                tm_types::TmError::parse(format!("Invalid persisted epoch {}: {e}", row.epoch))
            })?;
            tm_harness::HarnessEpoch {
                number: row.epoch,
                config_hash: config.config_hash(),
                config,
                promoted_at: row.ts,
                promoted_by: tm_types::ParticipantId::system(),
                benchmark: None,
            }
        }
        None => tm_harness::HarnessEpoch {
            number: 0,
            config_hash: tm_harness::HarnessConfig::default().config_hash(),
            config: tm_harness::HarnessConfig::default(),
            promoted_at: project.clock.now(),
            promoted_by: tm_types::ParticipantId::system(),
            benchmark: None,
        },
    };
    let mut registry = tm_harness::EpochRegistry::new(current_epoch);
    if let Some(baseline_path) = &args.baseline {
        let baseline_content = fs::read_to_string(baseline_path).map_err(|e| {
            tm_types::TmError::storage(format!("Failed to read baseline report: {e}"))
        })?;
        let baseline: tm_harness::BenchmarkReport = serde_json::from_str(&baseline_content)
            .map_err(|e| tm_types::TmError::parse(format!("Invalid baseline report JSON: {e}")))?;
        // `EpochRegistry` only exposes a baseline through the epoch it thinks is current, so a
        // caller-supplied baseline is folded onto the reconstructed current epoch here rather
        // than passed to `promote` directly (which has no baseline parameter of its own).
        registry = tm_harness::EpochRegistry::new(tm_harness::HarnessEpoch {
            benchmark: Some(baseline),
            ..registry.current().clone()
        });
    }

    let report_path = match &args.report {
        Some(p) => Some(p.clone()),
        None => latest_bench_report(project),
    };
    let benchmark = match &report_path {
        Some(p) => {
            let content = fs::read_to_string(p).map_err(|e| {
                tm_types::TmError::storage(format!("Failed to read {}: {e}", p.display()))
            })?;
            Some(
                serde_json::from_str::<tm_harness::BenchmarkReport>(&content)
                    .map_err(|e| tm_types::TmError::parse(format!("Invalid report JSON: {e}")))?,
            )
        }
        None => None,
    };
    if benchmark.is_none() && !args.force {
        return Err(tm_types::TmError::invariant(
            "no benchmark report found; run `tm bench run --out <path>` first, pass --report, or use --force".to_string(),
        ));
    }

    let gate = tm_harness::PromotionGate {
        require_benchmark_gain: !args.force,
        min_gain: 0.0,
    };

    let outcome = registry
        .promote(
            candidate.clone(),
            project.clock.now(),
            project.actor.clone(),
            benchmark,
            &gate,
        )
        .map_err(|e| tm_types::TmError::invariant(e.to_string()))?;

    match outcome {
        tm_harness::PromotionOutcome::Promoted(epoch) => {
            let config_toml = toml::to_string_pretty(&candidate).map_err(|e| {
                tm_types::TmError::storage(format!("Failed to serialize harness config: {e}"))
            })?;
            project
                .store
                .promote_epoch(config_toml, project.actor.clone())?;

            if renderer.is_json() {
                renderer.emit(
                    &serde_json::json!({
                        "epoch": epoch.number,
                        "promoted": true,
                        "hash": epoch.config_hash.to_string(),
                    }),
                    "",
                )?;
            } else {
                renderer.note(&format!("Promoted epoch {}", epoch.number));
            }
            Ok(())
        }
        tm_harness::PromotionOutcome::Rejected(decision) => {
            let reason = match decision {
                tm_harness::PromotionDecision::Rejected(reason) => reason,
                tm_harness::PromotionDecision::Approved => unreachable!(
                    "PromotionOutcome::Rejected always wraps PromotionDecision::Rejected"
                ),
            };
            if renderer.is_json() {
                renderer.emit(
                    &serde_json::json!({"epoch": args.epoch, "promoted": false, "reason": reason}),
                    "",
                )?;
            } else {
                renderer.note(&format!(
                    "Epoch {} promotion rejected: {reason}",
                    args.epoch
                ));
            }
            Err(tm_types::TmError::invariant(format!(
                "epoch {} promotion rejected: {reason}",
                args.epoch
            )))
        }
    }
}

/// The newest `.tm/bench/*.json` report on disk, by filename (bench report filenames are
/// timestamp-derived and sort chronologically — see `bench_run`'s `out_path` default), or `None`
/// if `.tm/bench/` doesn't exist or has no reports yet.
fn latest_bench_report(project: &Project) -> Option<std::path::PathBuf> {
    let dir = project.state_dir.join("bench");
    let mut candidates: Vec<_> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    candidates.sort();
    candidates.pop()
}

/// Dispatch one [`BenchCommand`].
pub async fn dispatch_bench(
    cmd: &BenchCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        BenchCommand::List => bench_list(project, renderer),
        BenchCommand::Run(args) => bench_run(args, project, renderer).await,
        BenchCommand::Compare(args) => bench_compare(args, renderer),
        BenchCommand::Report(args) => bench_report::run_bench_report(args, renderer),
    }
}

/// Discover every `bench/tasks/*.toml` under the project root, parsed via
/// [`tm_harness::BenchTask::parse`], in filename order. An absent `bench/tasks/` directory is
/// not an error — it means no tasks are declared yet, matching `tm docs list`'s "legitimately
/// empty" convention for a not-yet-populated registry.
fn discover_bench_tasks(project: &Project) -> tm_types::Result<Vec<tm_harness::BenchTask>> {
    let tasks_dir = project.root.join("bench").join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<_> = fs::read_dir(&tasks_dir)
        .map_err(|e| {
            tm_types::TmError::storage(format!("Failed to read {}: {e}", tasks_dir.display()))
        })?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    let mut tasks = Vec::with_capacity(paths.len());
    for path in paths {
        let contents = fs::read_to_string(&path).map_err(|e| {
            tm_types::TmError::storage(format!("Failed to read {}: {e}", path.display()))
        })?;
        tasks.push(tm_harness::BenchTask::parse(&contents).map_err(|e| {
            tm_types::TmError::parse(format!("Invalid bench task {}: {e}", path.display()))
        })?);
    }
    Ok(tasks)
}

/// A [`tm_harness::SeededProvider`] that replays a fixed, on-disk transcript per task: one
/// scripted step per non-empty line of `bench/<fixture.path>/script.txt`.
///
/// This crate has no live agent loop to drive a bench task through (`tm-harness` deliberately
/// does not depend on `tm-provider`/`tm-agent`, per its own module docs, to avoid a dependency
/// cycle), so a task's pass/fail outcome is driven entirely by the fixture author's script —
/// edit the script and the task's [`tm_types::Predicate`] genuinely stops being attested. This
/// is deterministic replay, not live agent execution, matching `BenchRunner`'s own documented
/// contract.
struct FixtureScriptProvider {
    scripts: BTreeMap<String, Vec<String>>,
}

impl FixtureScriptProvider {
    fn load(
        bench_root: &std::path::Path,
        tasks: &[tm_harness::BenchTask],
    ) -> tm_types::Result<Self> {
        let mut scripts = BTreeMap::new();
        for task in tasks {
            let script_path = bench_root.join(&task.fixture.path).join("script.txt");
            let contents = fs::read_to_string(&script_path).map_err(|e| {
                tm_types::TmError::storage(format!(
                    "bench task {}: failed to read fixture script {}: {e}",
                    task.id,
                    script_path.display()
                ))
            })?;
            let lines: Vec<String> = contents
                .lines()
                .map(|l| l.to_string())
                .filter(|l| !l.is_empty())
                .collect();
            scripts.insert(task.id.clone(), lines);
        }
        Ok(FixtureScriptProvider { scripts })
    }
}

impl tm_harness::SeededProvider for FixtureScriptProvider {
    fn step(&self, task: &tm_harness::BenchTask, step_index: u32) -> tm_types::Result<String> {
        self.scripts
            .get(&task.id)
            .and_then(|lines| lines.get(step_index as usize))
            .cloned()
            .ok_or_else(|| {
                tm_types::TmError::not_found(
                    "bench script step",
                    format!("{}:{step_index}", task.id),
                )
            })
    }

    fn is_finished(&self, task: &tm_harness::BenchTask, step_index: u32) -> bool {
        self.scripts
            .get(&task.id)
            .map(|lines| step_index as usize >= lines.len())
            .unwrap_or(true)
    }
}

/// `tm bench list`
///
/// # IMPL
/// Discover `BenchTask`s under the project's bench fixture directory (parsed via
/// `BenchTask::parse`); render name and `ScoringSpec` summary as a table or JSON.
pub fn bench_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let tasks = discover_bench_tasks(project)?;

    if renderer.is_json() {
        let out: Vec<_> = tasks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "task": t.task,
                    "fixture": t.fixture.path,
                    "description": t.fixture.description,
                })
            })
            .collect();
        renderer.emit(&out, "")?;
    } else {
        let rows = tasks
            .iter()
            .map(|t| vec![t.id.clone(), t.fixture.description.clone()])
            .collect();
        let table = Table::new(vec!["Id".to_string(), "Description".to_string()], rows);
        renderer.emit(&(), &table.render())?;
    }
    Ok(())
}

/// `tm bench run`
///
/// # IMPL
/// Build a `BenchRunner` over a `SeededProvider`, run every task matching `args.filter` via
/// `BenchRunner::run_all`, write the resulting `BenchmarkReport` to `args.out` if given (else
/// `.tm/bench/<timestamp>.json`), render a summary table (task, pass/fail, score) or the full
/// report as JSON. `args.live` swaps the scripted `FixtureScriptProvider` for
/// `crate::bench_live::LiveSeededProvider` — a real ticket run per task instead of a replayed
/// transcript — and patches the resulting report's cost/tool-call numbers with what that run
/// actually recorded.
pub async fn bench_run(
    args: &BenchRunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let tasks = discover_bench_tasks(project)?;
    let filtered: Vec<tm_harness::BenchTask> = tasks
        .into_iter()
        .filter(|t| {
            args.filter
                .as_ref()
                .map(|f| t.id.contains(f.as_str()))
                .unwrap_or(true)
        })
        .collect();

    let bench_root = project.root.join("bench");

    // The epoch a run is scored against is the highest epoch actually promoted so far (0, the
    // never-persisted genesis epoch, when nothing has been promoted yet — see
    // `Store::harness_epochs`'s own doc comment on why an empty result doesn't mean "epoch 0
    // doesn't exist").
    let epoch = project
        .store
        .harness_epochs()?
        .into_iter()
        .map(|e| e.epoch)
        .max()
        .unwrap_or(0);

    let runner = tm_harness::BenchRunner {
        clock: project.clock.as_ref(),
        ids: project.ids.as_ref(),
    };
    let report = if args.live {
        // `--live` drives every matched task through a real ticket run instead of replaying
        // `bench/<fixture>/script.txt` — see `crate::bench_live`'s own module doc comment.
        let provider = crate::bench_live::LiveSeededProvider::new(project, renderer);
        let mut report = runner.run_all(&filtered, &provider, epoch)?;
        provider.apply_live_metrics(&mut report);
        report
    } else {
        let provider = FixtureScriptProvider::load(&bench_root, &filtered)?;
        runner.run_all(&filtered, &provider, epoch)?
    };

    let out_path = args.out.clone().unwrap_or_else(|| {
        project.state_dir.join("bench").join(format!(
            "{}.json",
            report.generated_at.to_rfc3339().replace(':', "-")
        ))
    });
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            tm_types::TmError::storage(format!("Failed to create {}: {e}", parent.display()))
        })?;
    }
    let json_str = serde_json::to_string_pretty(&report).map_err(tm_types::TmError::from)?;
    fs::write(&out_path, json_str)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to write report: {e}")))?;

    if renderer.is_json() {
        renderer.emit(&report, "")?;
    } else {
        let rows = report
            .tasks
            .iter()
            .map(|t| {
                vec![
                    t.task_id.clone(),
                    if t.passed { "pass" } else { "fail" }.to_string(),
                    format!("{:.2}", t.score),
                ]
            })
            .collect();
        let table = Table::new(
            vec![
                "Task".to_string(),
                "Result".to_string(),
                "Score".to_string(),
            ],
            rows,
        );
        renderer.emit(&(), &table.render())?;
        renderer.note(&format!(
            "Benchmark run completed: {} task(s), aggregate score {:.2} (report: {})",
            report.tasks.len(),
            report.aggregate_score,
            out_path.display()
        ));
    }
    Ok(())
}

/// Format a benchmark comparison report as human-readable text.
fn format_bench_comparison(report: &tm_harness::PromotionReport) -> String {
    let improved_status = if report.candidate_improved {
        "yes"
    } else {
        "no"
    };
    let mut output = format!(
        "Baseline epoch {} vs candidate epoch {}\nCandidate improved: {}\nAggregate gain: {:+.2}",
        report.baseline_epoch, report.candidate_epoch, improved_status, report.aggregate_gain
    );

    if !report.task_deltas.is_empty() {
        let improved_count = report
            .task_deltas
            .iter()
            .filter(|(_, delta)| *delta > 0.0)
            .count();
        let regressed_count = report
            .task_deltas
            .iter()
            .filter(|(_, delta)| *delta < 0.0)
            .count();
        let unchanged_count = report.task_deltas.len() - improved_count - regressed_count;
        output.push_str(&format!(
            "\nTasks: {} improved, {} regressed, {} unchanged",
            improved_count, regressed_count, unchanged_count
        ));
    }

    output
}

/// `tm bench compare`
///
/// # IMPL
/// Read `args.baseline`/`args.candidate` as `BenchmarkReport` JSON, call
/// `tm_harness::bench::compare`, render the resulting `PromotionReport` (net gain, regressions)
/// as prose or JSON.
pub fn bench_compare(args: &BenchCompareArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let baseline_content = fs::read_to_string(&args.baseline)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to read baseline: {e}")))?;
    let baseline: tm_harness::BenchmarkReport = serde_json::from_str(&baseline_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid baseline JSON: {e}")))?;

    let candidate_content = fs::read_to_string(&args.candidate)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to read candidate: {e}")))?;
    let candidate: tm_harness::BenchmarkReport = serde_json::from_str(&candidate_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid candidate JSON: {e}")))?;

    let report = tm_harness::compare(&baseline, &candidate);
    renderer.emit(&report, &format_bench_comparison(&report))?;

    Ok(())
}

/// Dispatch one [`MirrorCommand`].
pub async fn dispatch_mirror(
    cmd: &MirrorCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        MirrorCommand::Link(args) => mirror_link(args, project, renderer),
        MirrorCommand::Push => mirror_push(project, renderer).await,
        MirrorCommand::Pull => mirror_pull(project, renderer).await,
        MirrorCommand::Status => mirror_status(project, renderer),
    }
}

/// Turn `(field, env_var)` pairs an unset environment variable was found for into the
/// non-fatal warning lines `mirror link` prints — so a credential misconfiguration surfaces at
/// link time rather than first at push/pull time.
fn format_unset_credential_warnings(unset: &[(String, String)]) -> Vec<String> {
    unset
        .iter()
        .map(|(field, env_var)| {
            format!("credential field \"{field}\" references env var {env_var} which is not set")
        })
        .collect()
}

/// `tm mirror link`
///
/// # IMPL
/// Parse `args.adapter` into `tm_mirror::config::AdapterKind`, persist the resulting
/// `AdapterConfig` into the project's `mirror.toml`. Credentials are never resolved here (that's
/// `CredentialEnv::resolve`, first called at push/pull time) — only the env var *names* an
/// explicitly given `--credential field=ENV_VAR` references are checked for existence, and only
/// as a non-fatal warning: an unset one still lets the link succeed, so the misconfiguration
/// surfaces here rather than first at push/pull time.
pub fn mirror_link(
    args: &MirrorLinkArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let _view = project.store.view()?;
    let mirror_path = project.state_dir.join("mirror.toml");

    let (adapter_kind, credential_var) = match args.adapter.to_lowercase().as_str() {
        "github" => (tm_mirror::AdapterKind::GitHub, "GITHUB_TOKEN"),
        "gitlab" => (tm_mirror::AdapterKind::GitLab, "GITLAB_TOKEN"),
        "linear" => (tm_mirror::AdapterKind::Linear, "LINEAR_API_KEY"),
        "jira" => (tm_mirror::AdapterKind::Jira, "JIRA_API_TOKEN"),
        _ => {
            return Err(tm_types::TmError::parse(format!(
                "Unknown adapter: {}",
                args.adapter
            )))
        }
    };

    let mut config = if mirror_path.is_file() {
        let existing = fs::read_to_string(&mirror_path)?;
        tm_mirror::MirrorConfig::parse(&existing)?
    } else {
        tm_mirror::MirrorConfig::default()
    };

    let mut credentials = std::collections::BTreeMap::new();
    credentials.insert("token".to_string(), credential_var.to_string());
    // Fields the caller explicitly named with `--credential field=ENV_VAR`, checked below for an
    // unset env var so the misconfiguration surfaces here rather than first at push/pull time.
    let mut unset_credential_vars: Vec<(String, String)> = Vec::new();
    for entry in &args.credentials {
        let Some((field, env_var)) = entry.split_once('=') else {
            return Err(tm_types::TmError::parse(format!(
                "Invalid --credential {entry}: expected FIELD=ENV_VAR"
            )));
        };
        if std::env::var(env_var).is_err() {
            unset_credential_vars.push((field.to_string(), env_var.to_string()));
        }
        credentials.insert(field.to_string(), env_var.to_string());
    }
    config.adapters.insert(
        args.adapter.to_lowercase(),
        tm_mirror::AdapterConfig {
            name: args.adapter.to_lowercase(),
            kind: adapter_kind,
            enabled: true,
            credentials: tm_mirror::CredentialEnv { vars: credentials },
            projection: Default::default(),
            state_mapping: Default::default(),
        },
    );

    let serialized =
        toml::to_string(&config).map_err(|e| tm_types::TmError::storage(e.to_string()))?;
    if let Some(parent) = mirror_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&mirror_path, serialized)?;

    let warnings = format_unset_credential_warnings(&unset_credential_vars);

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"adapter": args.adapter, "linked": true, "warnings": warnings}),
            "",
        )?;
    } else {
        renderer.note(&format!("Linked {} mirror", args.adapter));
        for warning in &warnings {
            renderer.note(&format!("warning: {warning}"));
        }
    }
    Ok(())
}

/// The result of trying to build a live [`tm_mirror::Tracker`] for one configured adapter.
enum TrackerBuildOutcome {
    /// The tracker was built and is ready to push/pull.
    Built(Box<dyn tm_mirror::Tracker>),
    /// Built nothing, for a documented reason (missing credential env var(s)) — not an error:
    /// `tm mirror push`/`pull` must not fail the whole command because one adapter isn't fully
    /// configured yet, only report it as skipped.
    Skipped(String),
}

/// Build the live [`tm_mirror::Tracker`] for `adapter`, reading every credential field it needs
/// from the environment variables `adapter.credentials.vars` names (never a hardcoded env var
/// name — see [`tm_mirror::config::CredentialEnv::resolve`]'s own contract). Each adapter kind
/// needs more than a bare token to know *which* external project/team to talk to; the field
/// names below (`owner`/`repo`, `project_id`, `email`/`api_token`/`project_key`,
/// `api_key`/`team_id`) are exactly what `tm mirror link --credential FIELD=ENV_VAR` documents.
fn build_tracker(
    adapter: &tm_mirror::AdapterConfig,
    clock: Arc<dyn tm_types::Clock>,
) -> tm_types::Result<TrackerBuildOutcome> {
    let resolved = match adapter.credentials.resolve() {
        Ok(r) => r,
        Err(e) => return Ok(TrackerBuildOutcome::Skipped(e.to_string())),
    };
    let field = |name: &str| resolved.get(name).cloned();

    match adapter.kind {
        tm_mirror::AdapterKind::Null => Ok(TrackerBuildOutcome::Built(Box::new(
            tm_mirror::NullTracker::new(adapter.name.clone()),
        ))),
        tm_mirror::AdapterKind::GitHub => match (field("token"), field("owner"), field("repo")) {
            (Some(token), Some(owner), Some(repo)) => {
                let tracker = tm_mirror::GitHubTracker::with_config(
                    adapter.name.clone(),
                    owner,
                    repo,
                    token,
                    tm_mirror::github::DEFAULT_BASE_URL.to_string(),
                    clock,
                )?;
                Ok(TrackerBuildOutcome::Built(Box::new(tracker)))
            }
            _ => Ok(TrackerBuildOutcome::Skipped(format!(
                "adapter {}: github requires credential fields token, owner, repo",
                adapter.name
            ))),
        },
        tm_mirror::AdapterKind::GitLab => match (field("token"), field("project_id")) {
            (Some(token), Some(project_id)) => {
                let tracker =
                    tm_mirror::GitLabTracker::with_config(adapter.name.clone(), project_id, token)?;
                Ok(TrackerBuildOutcome::Built(Box::new(tracker)))
            }
            _ => Ok(TrackerBuildOutcome::Skipped(format!(
                "adapter {}: gitlab requires credential fields token, project_id",
                adapter.name
            ))),
        },
        tm_mirror::AdapterKind::Jira => {
            match (field("email"), field("api_token"), field("project_key")) {
                (Some(email), Some(api_token), Some(project_key)) => {
                    let tracker = tm_mirror::JiraTracker::with_config(
                        adapter.name.clone(),
                        project_key,
                        email,
                        api_token,
                        tm_mirror::jira::DEFAULT_BASE_URL.to_string(),
                    )?;
                    Ok(TrackerBuildOutcome::Built(Box::new(tracker)))
                }
                _ => Ok(TrackerBuildOutcome::Skipped(format!(
                    "adapter {}: jira requires credential fields email, api_token, project_key",
                    adapter.name
                ))),
            }
        }
        tm_mirror::AdapterKind::Linear => match (field("api_key"), field("team_id")) {
            (Some(api_key), Some(team_id)) => {
                let tracker = tm_mirror::LinearTracker::with_config(
                    adapter.name.clone(),
                    team_id,
                    api_key,
                    tm_mirror::linear::DEFAULT_BASE_URL.to_string(),
                )?;
                Ok(TrackerBuildOutcome::Built(Box::new(tracker)))
            }
            _ => Ok(TrackerBuildOutcome::Skipped(format!(
                "adapter {}: linear requires credential fields api_key, team_id",
                adapter.name
            ))),
        },
    }
}

/// Explain a bare "0 pushed"/"0 pulled" `mirror push`/`mirror pull` result: why there was nothing
/// to do, rather than a bare count with no reason attached.
///
/// `eligible_count` (tickets, for push; linked tickets with something to pull, for pull) is
/// computed independently of whether any adapter's tracker actually built, so "no tickets
/// eligible" is reserved for the case where there is genuinely nothing that qualifies, regardless
/// of adapter health. `any_tracker_built` is checked next, ahead of "all already synced": a run
/// where every configured adapter failed to build (e.g. a missing credential env var) must not be
/// misreported as "already synced" — nothing was actually attempted.
fn mirror_zero_result_reason(
    enabled_adapter_count: usize,
    eligible_count: u64,
    any_tracker_built: bool,
) -> &'static str {
    if enabled_adapter_count == 0 {
        "no mirrors configured"
    } else if eligible_count == 0 {
        "no tickets eligible"
    } else if !any_tracker_built {
        "no adapter could authenticate, see the skipped adapters below"
    } else {
        "all already synced"
    }
}

/// `tm mirror push`
///
/// # IMPL
/// For each enabled `AdapterConfig`, build its `Tracker` (`GitHubTracker`/`GitLabTracker`/
/// `JiraTracker`/`LinearTracker`), run `SyncEngine`'s push path over
/// `ProjectionPolicy`-filtered tickets; render pushed/degraded counts per adapter.
///
/// `SyncEngine::push`'s `existing` parameter (the prior `tm_mirror::MirrorLink`, used for
/// content-hash idempotency) is reconstructed from the persisted `mirror_links` row's
/// `content_hash` (see [`tm_core::MirrorLinkRow`]'s doc comment) when one is already linked to
/// this adapter, so a `tm mirror push` in a fresh process still recognizes an unchanged
/// projection and skips the tracker call entirely rather than re-pushing every eligible ticket
/// every time (mirror-persist-content-hash-for-idempotency). [`tm_core::Store::
/// set_mirror_content_hash`] records the hash after a real push completes.
///
/// Each ticket/adapter push is wrapped in a `SPEC.md` §21.5 idempotent-effect guard
/// (`tm_core::Store::begin_effect`, audit B-11): the effect key is
/// `(ticket, ticket.attempts, "mirror.push:<adapter>", <projection content hash>)`, so an
/// already-completed push for the exact same projection short-circuits (no network call, no
/// duplicate issue) rather than trusting the removed per-adapter in-memory caches this replaces.
/// A guard resumed from a prior, never-completed journal entry (the crash window between the
/// external write landing and the receipt being recorded) tries the adapter's
/// [`tm_mirror::Tracker::confirm`] probe first, so a lost receipt does not necessarily cost a
/// duplicate external write.
pub async fn mirror_push(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let mirror_path = project.state_dir.join("mirror.toml");
    if !mirror_path.is_file() {
        if renderer.is_json() {
            renderer.emit(
                &serde_json::json!({"pushed": 0, "already_synced": 0, "degraded": 0, "skipped_adapters": [], "reason": "no mirrors configured"}),
                "",
            )?;
        } else {
            renderer.note("Mirror push completed: 0 pushed, 0 degraded (no mirror.toml)");
        }
        return Ok(());
    }
    let config = tm_mirror::MirrorConfig::parse(&fs::read_to_string(&mirror_path)?)?;
    let view = project.store.view()?;
    let policy = tm_mirror::ProjectionPolicy::default_policy();
    let engine = tm_mirror::SyncEngine::new(
        project.clock.clone(),
        project.ids.clone(),
        project.actor.clone(),
    );
    // Keyed by (ticket, adapter name), so `SyncEngine::push` can be handed the prior push's
    // content hash and skip a no-op re-push instead of always calling the tracker — see this
    // function's doc comment.
    let existing_links: BTreeMap<(String, String), tm_core::MirrorLinkRow> = project
        .store
        .mirror_links()?
        .into_iter()
        .map(|link| {
            (
                (link.ticket.as_str().to_string(), link.remote_system.clone()),
                link,
            )
        })
        .collect();

    let enabled_adapter_count = config.enabled_adapters().len();
    // Tickets that qualify to be mirrored at all, independent of whether any adapter's tracker
    // actually built — so "no tickets eligible" stays reserved for the case where nothing
    // qualifies, not the unrelated case where a credential is missing (see
    // `mirror_zero_result_reason`).
    let eligible_tickets = view
        .tickets
        .values()
        .filter(|t| policy.should_mirror(t, None))
        .count() as u64;
    let mut pushed = 0u64;
    let mut already_synced = 0u64;
    let mut degraded = 0u64;
    let mut skipped_adapters = Vec::new();
    let mut any_tracker_built = false;

    for adapter in config.enabled_adapters() {
        let tracker = match build_tracker(adapter, project.clock.clone())? {
            TrackerBuildOutcome::Built(t) => t,
            TrackerBuildOutcome::Skipped(reason) => {
                skipped_adapters
                    .push(serde_json::json!({"adapter": adapter.name, "reason": reason}));
                continue;
            }
        };
        any_tracker_built = true;
        let caps = tracker.capabilities();
        for ticket in view.tickets.values() {
            if !policy.should_mirror(ticket, None) {
                continue;
            }
            let descendants: Vec<tm_core::Ticket> = ticket
                .children
                .iter()
                .filter_map(|id| view.tickets.get(id).cloned())
                .collect();
            let projection = policy.project(ticket, &descendants, &caps);
            if !projection.degradations.is_empty() {
                degraded += 1;
            }

            let effect_kind = format!("mirror.push:{}", tracker.name());
            let canonical_args = tm_mirror::SyncEngine::projection_hash(&projection);
            let key = tm_core::EffectKey::compute(
                &ticket.id,
                ticket.attempts,
                &effect_kind,
                &canonical_args,
            );
            let guard = project.store.begin_effect(
                key,
                ticket.id.clone(),
                ticket.attempts,
                effect_kind,
                project.actor.clone(),
            )?;

            if guard.already_completed() {
                // The exact same projection was already pushed to this adapter under this
                // ticket attempt — the idempotency guarantee this mechanism exists for. No
                // network call, no re-linking; counted separately from `pushed` so a run that
                // did nothing but confirm existing pushes is reported as "all already synced"
                // rather than as a fresh push.
                already_synced += 1;
                continue;
            }

            // A prior journal entry existed but never completed: the process may have crashed
            // between the external write landing and the receipt being recorded. Ask the
            // adapter whether it already happened before assuming it didn't.
            let external = if guard.resumed() {
                tracker.confirm(&ticket.id).await?
            } else {
                None
            };
            // Reconstruct `SyncEngine::push`'s `existing` from the persisted row's
            // `content_hash`, when there is one, so an unchanged projection is recognized as a
            // no-op even across a process restart or a bumped `ticket.attempts` (which changes
            // the effect-guard key above but not the projection itself).
            let existing_link = existing_links
                .get(&(ticket.id.as_str().to_string(), tracker.name().to_string()))
                .and_then(|row| row.content_hash.as_ref().map(|hash| (row, hash)))
                .map(|(row, hash)| tm_mirror::MirrorLink {
                    ticket: ticket.id.clone(),
                    adapter: tracker.name().to_string(),
                    external: tm_mirror::ExternalRef {
                        adapter: tracker.name().to_string(),
                        external_id: row.remote_id.clone(),
                        url: None,
                    },
                    degradations: Vec::new(),
                    content_hash: hash.clone(),
                    pushed_at: row.last_synced,
                    last_pulled_at: None,
                });
            let (link_external, new_content_hash) = match external {
                // The adapter confirmed the write already happened (a resumed guard), so this
                // push's projection is now the one recorded externally -- record its hash too,
                // the same as a fresh tracker call below.
                Some(external) => (external, canonical_args.clone()),
                None => {
                    let (link, draft) = engine
                        .push(tracker.as_ref(), &projection, existing_link.as_ref())
                        .await?;
                    if draft.is_none() {
                        // `existing_link`'s content hash already matched `projection`'s hash:
                        // `SyncEngine::push` returned it unchanged, appended no event, and never
                        // called `tracker.push` -- the idempotency guarantee this reconstruction
                        // exists for, reachable even when the effect guard above can't help (a
                        // process restart, or a `ticket.attempts` bump between calls). Nothing
                        // changed, so there is nothing new to re-link, re-record, or re-hash;
                        // just close out the guard with the link this projection already has.
                        already_synced += 1;
                        guard.complete(&project.store, Some(&link.external.external_id))?;
                        continue;
                    }
                    (link.external, link.content_hash)
                }
            };

            project.store.link_mirror(
                &ticket.id,
                tracker.name().to_string(),
                project.actor.clone(),
            )?;
            project.store.update_mirror_link(
                &ticket.id,
                tracker.name().to_string(),
                link_external.external_id.clone(),
                tm_core::MirrorSyncDirection::Push,
                project.actor.clone(),
            )?;
            project
                .store
                .set_mirror_content_hash(&ticket.id, tracker.name(), &new_content_hash)?;
            guard.complete(&project.store, Some(&link_external.external_id))?;
            pushed += 1;
        }
    }

    let reason = if pushed == 0 && degraded == 0 {
        Some(mirror_zero_result_reason(
            enabled_adapter_count,
            eligible_tickets,
            any_tracker_built,
        ))
    } else {
        None
    };

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"pushed": pushed, "already_synced": already_synced, "degraded": degraded, "skipped_adapters": skipped_adapters, "reason": reason}),
            "",
        )?;
    } else {
        match reason {
            Some(reason) => renderer.note(&format!(
                "Mirror push completed: {pushed} pushed, {degraded} degraded ({reason})"
            )),
            None => renderer.note(&format!(
                "Mirror push completed: {pushed} pushed, {degraded} degraded"
            )),
        }
        for s in &skipped_adapters {
            renderer.note(&format!(
                "Skipped adapter {}: {}",
                s["adapter"], s["reason"]
            ));
        }
    }
    Ok(())
}

/// `tm mirror pull`
///
/// # IMPL
/// For each enabled adapter, fetch `ExternalChange`s and translate them via
/// `SyncEngine::translate`/`to_event_drafts`, appending only the fixed allowlisted event kinds
/// `tm-mirror`'s module docs describe — never writing ticket state directly from an inbound
/// change.
///
/// The thin persisted [`tm_core::MirrorLinkRow`] carries no `last_pulled_at`, so every pull asks
/// each tracker for changes since [`tm_types::Timestamp::EPOCH`] rather than incrementally since
/// the last pull — a B-05 schema limitation `content_hash` (mirror-persist-content-hash-for-
/// idempotency) does not address, since pull has no analogous content hash to reconstruct from.
pub async fn mirror_pull(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let mirror_path = project.state_dir.join("mirror.toml");
    if !mirror_path.is_file() {
        if renderer.is_json() {
            renderer.emit(
                &serde_json::json!({"pulled": 0, "applied": 0, "skipped_adapters": [], "reason": "no mirrors configured"}),
                "",
            )?;
        } else {
            renderer.note("Mirror pull completed: 0 pulled, 0 applied (no mirror.toml)");
        }
        return Ok(());
    }
    let config = tm_mirror::MirrorConfig::parse(&fs::read_to_string(&mirror_path)?)?;
    let engine = tm_mirror::SyncEngine::new(
        project.clock.clone(),
        project.ids.clone(),
        project.actor.clone(),
    );
    let links = project.store.mirror_links()?;
    let adapters_by_name: BTreeMap<&str, &tm_mirror::AdapterConfig> = config
        .enabled_adapters()
        .into_iter()
        .map(|a| (a.name.as_str(), a))
        .collect();

    let enabled_adapter_count = adapters_by_name.len();
    let mut pulled = 0u64;
    let mut applied = 0u64;
    let mut skipped_adapters = Vec::new();
    // Links that actually have something to pull from (a non-empty `remote_id` and a still
    // configured/enabled adapter), independent of whether that adapter's tracker actually built
    // — see `mirror_zero_result_reason`'s doc comment.
    let mut eligible_links = 0u64;
    let mut any_tracker_built = false;

    for link in &links {
        // A link with no `remote_id` was `mirror.linked` but never actually pushed or pulled
        // (see `mirror.linked`'s materializer arm) — nothing external to pull from yet.
        if link.remote_id.is_empty() {
            continue;
        }
        let Some(adapter) = adapters_by_name.get(link.remote_system.as_str()) else {
            continue; // linked to an adapter no longer configured/enabled
        };
        eligible_links += 1;
        let tracker = match build_tracker(adapter, project.clock.clone())? {
            TrackerBuildOutcome::Built(t) => t,
            TrackerBuildOutcome::Skipped(reason) => {
                skipped_adapters
                    .push(serde_json::json!({"adapter": adapter.name, "reason": reason}));
                continue;
            }
        };
        any_tracker_built = true;
        let stub_link = tm_mirror::MirrorLink {
            ticket: link.ticket.clone(),
            adapter: link.remote_system.clone(),
            external: tm_mirror::ExternalRef {
                adapter: link.remote_system.clone(),
                external_id: link.remote_id.clone(),
                url: None,
            },
            degradations: Vec::new(),
            content_hash: String::new(),
            pushed_at: link.last_synced,
            last_pulled_at: None,
        };
        let (_updated, drafts) = engine.pull(tracker.as_ref(), &stub_link).await?;
        pulled += 1;
        // `drafts` always includes the trailing `mirror.pulled` freshness event even when no
        // inbound change translated to anything (see `SyncEngine::pull`'s own doc comment);
        // exclude it from `applied`'s count of real ticket-facing changes.
        applied += drafts.len().saturating_sub(1) as u64;
        project.store.append(drafts)?;
        project.store.update_mirror_link(
            &link.ticket,
            link.remote_system.clone(),
            link.remote_id.clone(),
            tm_core::MirrorSyncDirection::Pull,
            project.actor.clone(),
        )?;
    }

    let reason = if pulled == 0 && applied == 0 {
        Some(mirror_zero_result_reason(
            enabled_adapter_count,
            eligible_links,
            any_tracker_built,
        ))
    } else {
        None
    };

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"pulled": pulled, "applied": applied, "skipped_adapters": skipped_adapters, "reason": reason}),
            "",
        )?;
    } else {
        match reason {
            Some(reason) => renderer.note(&format!(
                "Mirror pull completed: {pulled} pulled, {applied} applied ({reason})"
            )),
            None => renderer.note(&format!(
                "Mirror pull completed: {pulled} pulled, {applied} applied"
            )),
        }
        for s in &skipped_adapters {
            renderer.note(&format!(
                "Skipped adapter {}: {}",
                s["adapter"], s["reason"]
            ));
        }
    }
    Ok(())
}

/// One configured adapter as a `--json` entry, distinct from ticket-level mirror-link status.
fn mirror_status_adapter_json(adapter: &tm_mirror::AdapterConfig) -> serde_json::Value {
    serde_json::json!({
        "name": adapter.name,
        "kind": adapter.kind,
        "enabled": adapter.enabled,
    })
}

/// The product's own name for an adapter kind, for human-readable output — never the internal
/// enum variant spelling (`{:?}` debug output is against this codebase's voice rules).
fn adapter_kind_label(kind: tm_mirror::AdapterKind) -> &'static str {
    match kind {
        tm_mirror::AdapterKind::GitHub => "GitHub",
        tm_mirror::AdapterKind::GitLab => "GitLab",
        tm_mirror::AdapterKind::Jira => "Jira",
        tm_mirror::AdapterKind::Linear => "Linear",
        tm_mirror::AdapterKind::Null => "none",
    }
}

/// One configured adapter as a human-readable table row.
fn mirror_status_adapter_row(adapter: &tm_mirror::AdapterConfig) -> Vec<String> {
    vec![
        adapter.name.clone(),
        adapter_kind_label(adapter.kind).to_string(),
        if adapter.enabled {
            "enabled"
        } else {
            "disabled"
        }
        .to_string(),
    ]
}

/// `tm mirror status`
///
/// # IMPL
/// Render each configured adapter (from `mirror.toml`, distinct from ticket sync status —
/// linking an adapter is enough to show up here, whether or not any ticket has been pushed or
/// pulled yet) alongside each `MirrorLink`'s last sync time and any recorded `Degradation`s.
///
/// `tm_core::MirrorLinkRow` carries no `Degradation`s (the thin persisted table has no column
/// for them — see its doc comment), so the tickets section reports what's actually durable:
/// ticket, remote system, remote id, and last-synced time.
pub fn mirror_status(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let links = project.store.mirror_links()?;
    let mirror_path = project.state_dir.join("mirror.toml");
    let adapters: Vec<tm_mirror::AdapterConfig> = if mirror_path.is_file() {
        tm_mirror::MirrorConfig::parse(&fs::read_to_string(&mirror_path)?)?
            .adapters
            .into_values()
            .collect()
    } else {
        Vec::new()
    };

    if renderer.is_json() {
        let adapters_out: Vec<_> = adapters.iter().map(mirror_status_adapter_json).collect();
        let out: Vec<_> = links
            .iter()
            .map(|l| {
                serde_json::json!({
                    "ticket": l.ticket.to_string(),
                    "remote_system": l.remote_system,
                    "remote_id": l.remote_id,
                    "last_synced": l.last_synced.to_rfc3339(),
                })
            })
            .collect();
        renderer.emit(
            &serde_json::json!({"adapters": adapters_out, "mirrors": out}),
            "",
        )?;
    } else {
        if adapters.is_empty() {
            renderer.note("No mirror adapters configured");
        } else {
            let rows = adapters.iter().map(mirror_status_adapter_row).collect();
            let table = Table::new(
                vec![
                    "Adapter".to_string(),
                    "Kind".to_string(),
                    "State".to_string(),
                ],
                rows,
            );
            renderer.emit(&(), &table.render())?;
        }
        if links.is_empty() {
            renderer.note("No active mirror links");
        } else {
            let rows = links
                .iter()
                .map(|l| {
                    vec![
                        l.ticket.to_string(),
                        l.remote_system.clone(),
                        l.remote_id.clone(),
                        l.last_synced.to_rfc3339(),
                    ]
                })
                .collect();
            let table = Table::new(
                vec![
                    "Ticket".to_string(),
                    "Remote".to_string(),
                    "Remote id".to_string(),
                    "Last synced".to_string(),
                ],
                rows,
            );
            renderer.emit(&(), &table.render())?;
        }
    }
    Ok(())
}

/// Dispatch one [`EventsCommand`].
pub async fn dispatch_events(
    cmd: &EventsCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        EventsCommand::Tail(args) => events_tail(args, project, renderer).await,
        EventsCommand::Show(args) => events_show(args, project, renderer),
        EventsCommand::Replay(args) => events_replay(args, project, renderer),
        EventsCommand::Verify => events_verify(project, renderer),
    }
}

/// How often `tm events tail`'s follow mode re-reads the log for new events. `EventLog::subscribe`
/// is deliberately not used here even though it exists: its `EventHub` is in-process only
/// (`tm_events::stream`'s own doc comment), so it would only ever see events appended by *this*
/// `tm events tail` process, never the ones that matter — a `tm sched run`/`tm serve`/`tm run`
/// running as a separate process, which is the whole reason to tail in the first place. Polling
/// `EventLog::read_from` (a cheap indexed SQLite query) sees every process's writes.
const TAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// `tm events tail`
///
/// # IMPL
/// Open a second read-only `EventLog` handle on `.tm/project.db`, replay from `args.from` (else
/// current head) via `EventLog::read_from`, then poll for new events (see
/// [`TAIL_POLL_INTERVAL`]'s doc comment for why not `EventLog::subscribe`) until ctrl-c; render
/// each `Event` as one line (seq, kind, subject) or one JSON object per line in `--json` mode
/// (JSON Lines, not a single array, since this is an unbounded stream).
pub async fn events_tail(
    args: &EventsTailArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    // Validate --kind before touching the database: a bad argument should fail immediately, not
    // after a wasted open.
    let kind = args
        .kind
        .as_deref()
        .map(|s| {
            s.parse::<tm_events::EventKind>().map_err(|_| {
                tm_types::TmError::parse(format!(
                    "`{s}` isn't an event type. Event types look like `ticket.closed`; run `tm \
                     events tail --no-follow --from 1` to see the ones in this project."
                ))
            })
        })
        .transpose()?;
    let filter = EventFilter {
        kind,
        ticket: args.ticket.clone(),
    };

    let db_path = project.state_dir.join("project.db");
    let log = tm_events::EventLog::open(&db_path)?;

    let head = log.head()?;
    let from = args.from.unwrap_or(head);
    let (backlog, mut last_seen) = tail_backlog(&log, from, &filter)?;
    for event in &backlog {
        emit_event(renderer, event)?;
    }

    if !args.no_follow {
        let mut stop = std::pin::pin!(tokio::signal::ctrl_c());
        loop {
            tokio::select! {
                _ = tokio::time::sleep(TAIL_POLL_INTERVAL) => {
                    let (page, seen) = tail_backlog(&log, last_seen + 1, &filter)?;
                    for event in &page {
                        emit_event(renderer, event)?;
                    }
                    last_seen = seen;
                }
                _ = &mut stop => {
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Which events `tm events tail` admits. Built once from [`EventsTailArgs`]; kept separate from
/// the CLI args type so [`event_matches`] is testable without constructing one.
#[derive(Debug, Clone, Default)]
struct EventFilter {
    kind: Option<tm_events::EventKind>,
    ticket: Option<String>,
}

/// Pure predicate for `tm events tail --kind`/`--ticket`: does `event` pass `filter`? Kept
/// separate from the streaming loop so both the kind and ticket filters (and their combination)
/// are unit-testable without a live `EventLog`.
fn event_matches(event: &tm_events::Event, filter: &EventFilter) -> bool {
    if let Some(kind) = filter.kind {
        if event.kind != kind {
            return false;
        }
    }
    if let Some(ticket) = &filter.ticket {
        if event.subject.as_str() != ticket {
            return false;
        }
    }
    true
}

/// Read every event matching `filter` from `from` to the log's current head, paging through
/// `EventLog::read_from` rather than one unbounded query. Returns the matched events and the
/// highest `seq` seen (matching or not), so callers streaming live afterward know where the
/// backlog left off. Split out from [`events_tail`] so `--no-follow`'s behavior is testable
/// directly, without a renderer.
fn tail_backlog(
    log: &tm_events::EventLog,
    from: u64,
    filter: &EventFilter,
) -> tm_types::Result<(Vec<tm_events::Event>, u64)> {
    const PAGE: usize = 500;
    let mut cursor = from;
    let mut last_seen = from.saturating_sub(1);
    let mut matched = Vec::new();
    loop {
        let page = log.read_from(cursor, PAGE)?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        for event in page {
            last_seen = event.seq;
            if event_matches(&event, filter) {
                matched.push(event);
            }
        }
        if page_len < PAGE {
            break;
        }
        cursor = last_seen + 1;
    }
    Ok((matched, last_seen))
}

/// One line of `tm events tail` output: a compact JSON object in `--json` mode (JSON Lines, not
/// a pretty-printed array — `Renderer::emit`'s pretty printer would split one event across
/// several lines, which breaks JSON Lines framing for an unbounded stream), or `seq  kind
/// subject` in human mode.
fn emit_event(renderer: &Renderer, event: &tm_events::Event) -> tm_types::Result<()> {
    if renderer.is_json() {
        println!("{}", serde_json::to_string(&event_show_json(event)?)?);
    } else {
        println!("{}", event_tail_human(event));
    }
    Ok(())
}

/// Pure formatter for `tm events tail`'s human output.
fn event_tail_human(event: &tm_events::Event) -> String {
    format!("{}  {}  {}", event.seq, event.kind, event.subject)
}

/// `tm events show`
///
/// # IMPL
/// `EventLog::read_range(args.seq, args.seq)`, `TmError::not_found` if empty; render the full
/// `Event` (payload included) as prose or JSON.
pub fn events_show(
    args: &EventsShowArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let db_path = project.state_dir.join("project.db");
    let log = tm_events::EventLog::open(&db_path)?;
    let events = log.read_range(args.seq, args.seq)?;

    if events.is_empty() {
        return Err(tm_types::TmError::not_found(
            "event",
            args.seq.to_string().as_str(),
        ));
    }

    let event = &events[0];
    if renderer.is_json() {
        renderer.emit(&event_show_json(event)?, "")?;
    } else {
        renderer.emit(&(), &event_show_human(event)?)?;
    }
    Ok(())
}

/// Pure formatter for `tm events show`'s human output, kept separate from [`events_show`] so the
/// wording (field labels, event-kind Display vs. debug) is unit-testable without a renderer.
/// Renders `event.payload` as `field: value` pairs rather than `{:?}`-debug-printing the typed
/// [`tm_events::payload::Payload`] enum, per this module's `events_show` doc comment's promise
/// that the payload is included.
fn event_show_human(event: &tm_events::Event) -> tm_types::Result<String> {
    let payload = event.payload.to_json()?;
    Ok(format!(
        "Sequence: {}\nType: {}\nRelated to: {}\nWhen: {}\nDetails: {}",
        event.seq,
        event.kind,
        event.subject,
        event.ts,
        format_payload_human(&payload)
    ))
}

/// Render a payload's JSON object as a comma-separated, plain-English `field: value` list
/// (`"none"` for a field with no value) instead of raw JSON syntax — used only in human mode;
/// `--json` gets the real JSON via [`event_show_json`].
fn format_payload_human(payload: &serde_json::Value) -> String {
    match payload.as_object() {
        Some(fields) if !fields.is_empty() => fields
            .iter()
            .map(|(name, value)| format!("{name}: {}", format_payload_field_human(value)))
            .collect::<Vec<_>>()
            .join(", "),
        _ => "none".to_string(),
    }
}

/// One payload field's plain-text rendering: a JSON string prints unquoted, `null` prints
/// `"none"`, and anything else (a number, bool, or nested object/array) falls back to compact
/// JSON syntax for just that one value.
fn format_payload_field_human(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "none".to_string(),
        other => other.to_string(),
    }
}

/// Pure formatter for `tm events show --json`'s payload.
fn event_show_json(event: &tm_events::Event) -> tm_types::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "seq": event.seq,
        "kind": event.kind.to_string(),
        "subject": event.subject.to_string(),
        "ts": event.ts.to_string(),
        "payload": event.payload.to_json()?,
    }))
}

/// `tm events replay`
///
/// # IMPL
/// Delegate to [`tm_core::Store::view_as_of`], which replays `[1, to]` through
/// `tm_core::materialize::replay` against a throwaway scratch schema (never the live project's
/// own `project.db` — this command inspects, it does not mutate), then render the resulting
/// `ProjectView`'s ticket counts and states in human mode, or every replayed ticket in `--json`.
/// `args.from` is not itself a replay bound (materializing a ticket's state always requires
/// replaying from the log's start); it is carried through into the rendered output as context on
/// what range the caller asked about.
pub fn events_replay(
    args: &EventsReplayArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let db_path = project.state_dir.join("project.db");
    let log = tm_events::EventLog::open(&db_path)?;
    let head = log.head()?;
    let to = args.to.unwrap_or(head);

    if to == 0 {
        if renderer.is_json() {
            renderer.emit(
                &serde_json::json!({"from": args.from, "to": to, "tickets": {}}),
                "",
            )?;
        } else {
            renderer.emit(
                &(),
                "Nothing to replay yet: this project has no events at step 1 or later.",
            )?;
        }
        return Ok(());
    }

    let view = project.store.view_as_of(to)?;

    if renderer.is_json() {
        let json = serde_json::json!({
            "from": args.from,
            "to": to,
            "tickets": view.tickets,
        });
        renderer.emit(&json, "")?;
    } else {
        renderer.emit(&(), &events_replay_human(&view, to))?;
    }
    Ok(())
}

/// Pure formatter for `tm events replay`'s human output, kept separate from [`events_replay`] so
/// the rendered ticket counts and states are unit-testable without a live `Store` write path or
/// a renderer — mirrors [`event_show_human`]'s split for the same reason.
fn events_replay_human(view: &tm_core::ProjectView, to: u64) -> String {
    let mut by_state: BTreeMap<&'static str, usize> = BTreeMap::new();
    for ticket in view.tickets.values() {
        *by_state
            .entry(crate::render::state_label(ticket.state))
            .or_insert(0) += 1;
    }
    let mut lines = vec![format!(
        "Replayed to step {to}: {} ticket(s)",
        view.tickets.len()
    )];
    for (label, count) in &by_state {
        lines.push(format!("  {label}: {count}"));
    }
    lines.join("\n")
}

/// `tm events verify`
///
/// # IMPL
/// `EventLog::verify_chain()`; render `ChainReport` (`is_valid`, first broken link if any) and
/// return a non-zero-mapping error when invalid.
pub fn events_verify(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let db_path = project.state_dir.join("project.db");
    let log = tm_events::EventLog::open(&db_path)?;
    let report = log.verify_chain()?;

    if renderer.is_json() {
        let json = serde_json::json!({
            "valid": report.is_valid(),
            "events_checked": report.events_checked,
            "first_broken_seq": report.first_broken_seq,
        });
        renderer.emit(&json, "")?;
    } else {
        let status = if report.is_valid() {
            "valid".to_string()
        } else {
            format!(
                "invalid at seq {}: {}",
                report
                    .first_broken_seq
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                report.detail.as_deref().unwrap_or("unknown")
            )
        };
        renderer.note(&format!("Event chain is {}", status));
    }

    if !report.is_valid() {
        return Err(tm_types::TmError::invariant(format!(
            "Event chain is broken at seq {}",
            report
                .first_broken_seq
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{Authority, Budget, Clock, CounterIds, IdSource, Timestamp};

    fn test_project(root: &std::path::Path) -> Project {
        std::fs::create_dir_all(root.join(".tm")).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(
            Timestamp::from_unix_seconds(1_000_000),
        ));
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store = Arc::new(tm_core::Store::open_with(root, clock.clone(), ids.clone()).unwrap());
        Project::for_test(root, store, clock, ids)
    }

    fn test_renderer() -> Renderer {
        Renderer::new(true, true, true, false)
    }

    /// A ticket the default `ProjectionPolicy` will actually mirror: `should_mirror` requires a
    /// `Work` ticket to carry a milestone (`tm_mirror::projection::ProjectionPolicy::should_mirror`).
    fn mirrorable_ticket(project: &Project) -> tm_types::TicketId {
        let events = project
            .store
            .create_ticket(
                tm_core::TicketKind::Work,
                "do the thing".into(),
                None,
                None,
                Authority::root(),
                vec![],
                tm_core::ExecutorRequirements {
                    role: tm_types::Role::CoderFast,
                    human_required: false,
                    min_capability: tm_types::Tolerance::Any,
                },
                vec![],
                vec![],
                tm_core::VerificationPolicy::None,
                Budget::unlimited(),
                tm_core::RetryPolicy {
                    max_attempts: 3,
                    base_delay_seconds: 1,
                    backoff_multiplier: 2.0,
                    max_delay_seconds: 60,
                },
                0,
                project.actor.clone(),
            )
            .unwrap();
        let ticket_id = tm_types::TicketId::new(events[0].subject.as_str()).unwrap();
        project
            .store
            .create_milestone(
                "M1".into(),
                vec![ticket_id.clone()],
                vec![],
                project.actor.clone(),
            )
            .unwrap();
        ticket_id
    }

    #[tokio::test]
    async fn mirror_push_is_idempotent_across_repeated_calls_via_the_effect_guard() {
        // SPEC.md §21.5 / audit B-11: two `tm mirror push` invocations against unchanged state
        // must not journal a second effect for the same ticket/adapter/attempt/projection — the
        // whole point of wrapping this call site in `Store::begin_effect`.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let ticket_id = mirrorable_ticket(&project);

        std::fs::write(
            root.join(".tm").join("mirror.toml"),
            "[adapters.testnull]\nkind = \"null\"\nenabled = true\n",
        )
        .unwrap();

        let renderer = test_renderer();
        mirror_push(&project, &renderer).await.expect("first push");
        mirror_push(&project, &renderer).await.expect("second push");

        let conn =
            tm_events::schema::open_read_connection(&root.join(".tm").join("project.db")).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM effects WHERE ticket = ?1 AND kind = 'mirror.push:testnull'",
                [ticket_id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "two mirror_push calls against unchanged state must share one effect row"
        );
        let status: String = conn
            .query_row(
                "SELECT status FROM effects WHERE ticket = ?1 AND kind = 'mirror.push:testnull'",
                [ticket_id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    #[tokio::test]
    async fn mirror_push_reuses_the_persisted_content_hash_and_skips_the_tracker_call_after_the_effect_journal_is_cleared(
    ) {
        // mirror-persist-content-hash-for-idempotency: the effect guard above already makes a
        // second `mirror_push` over unchanged state a no-op, but it does so by keying on
        // `ticket.attempts`, which changes on a retry. Force exactly that gap open by deleting
        // the journaled effect row between the two pushes (as if the effects table were pruned,
        // or the ticket had a fresh attempt) while leaving `mirror_links` alone, so the second
        // push cannot lean on the effect guard and must instead reconstruct
        // `tm_mirror::sync::MirrorLink::content_hash` from the persisted row to recognize the
        // unchanged projection and skip `Tracker::push` a second time.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let ticket_id = mirrorable_ticket(&project);

        std::fs::write(
            root.join(".tm").join("mirror.toml"),
            "[adapters.testnull]\nkind = \"null\"\nenabled = true\n",
        )
        .unwrap();

        let renderer = test_renderer();
        mirror_push(&project, &renderer).await.expect("first push");

        let db_path = root.join(".tm").join("project.db");
        let raw = rusqlite::Connection::open(&db_path).unwrap();
        raw.execute(
            "DELETE FROM effects WHERE ticket = ?1 AND kind = 'mirror.push:testnull'",
            [ticket_id.as_str()],
        )
        .unwrap();
        let content_hash_before: Option<String> = raw
            .query_row(
                "SELECT content_hash FROM mirror_links WHERE ticket = ?1 AND remote_system = 'testnull'",
                [ticket_id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            content_hash_before.is_some(),
            "the first push must have persisted a content hash"
        );
        drop(raw);

        mirror_push(&project, &renderer)
            .await
            .expect("second push, with the effect journal cleared");

        let conn = tm_events::schema::open_read_connection(&db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM effects WHERE ticket = ?1 AND kind = 'mirror.push:testnull'",
                [ticket_id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "the second push journals a fresh effect row (the old one was deleted), not zero"
        );
        // The whole point: no `mirror.pushed` event landed for the second push, because
        // `SyncEngine::push` recognized the unchanged content hash and returned no draft --
        // `Tracker::push` (a `NullTracker` here, but the contract holds for any tracker) was
        // never called a second time.
        let pushed_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'mirror.pushed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            pushed_events, 1,
            "a second push over an unchanged projection must not append another mirror.pushed event"
        );
    }

    #[tokio::test]
    async fn mirror_push_journals_a_distinct_effect_per_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let _ticket_id = mirrorable_ticket(&project);

        std::fs::write(
            root.join(".tm").join("mirror.toml"),
            "[adapters.a]\nkind = \"null\"\nenabled = true\n\n[adapters.b]\nkind = \"null\"\nenabled = true\n",
        )
        .unwrap();

        let renderer = test_renderer();
        mirror_push(&project, &renderer).await.expect("push");

        let conn =
            tm_events::schema::open_read_connection(&root.join(".tm").join("project.db")).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM effects WHERE kind LIKE 'mirror.push:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "one effect per (ticket, adapter) pair");
    }

    #[test]
    fn docs_list_empty_registry() {
        // With empty registry, rendering should succeed
    }

    #[test]
    fn docs_check_no_stale_docs() {
        // With empty registry, check should pass
    }

    #[test]
    fn docs_reconcile_no_stale_docs() {
        // With empty registry, reconcile should succeed with no tickets opened
    }

    /// Write a minimal `docs/.tmdocs.toml` declaring one doc, for the
    /// `docs-persist-state-across-invocations` tests below.
    fn write_tmdocs_toml(root: &std::path::Path, path: &str, id: &str, mode: &str) {
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(
            root.join("docs").join(tm_docs::registry::TMDOCS_TOML),
            format!(
                "[[doc]]\npath = \"{path}\"\nid = \"{id}\"\nmode = \"{mode}\"\nderived_from = []\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_doc_marked_stale_through_the_store_still_reads_stale_from_a_fresh_docs_list_call() {
        // docs-persist-state-across-invocations acceptance: "a doc whose state is persisted as
        // Stale still reads Stale in a fresh docs_list or docs_check call" — i.e. the state
        // survives across separate `load_and_sync_doc_registry` calls (each one simulating a
        // fresh process invocation), not just within one.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        write_tmdocs_toml(root, "docs/architecture.md", "architecture", "generated");

        // First call registers the doc (fresh -> Unverified).
        let (registry, newly_registered) = load_and_sync_doc_registry(&project).unwrap();
        assert_eq!(newly_registered, 1);
        assert_eq!(
            registry.get("architecture").unwrap().state,
            tm_docs::DocState::Unverified
        );

        project
            .store
            .invalidate_doc(
                "docs/architecture.md".into(),
                "source changed".into(),
                project.actor.clone(),
            )
            .unwrap();

        // A second, independent call (simulating a fresh `tm docs check` invocation) must read
        // the persisted Stale state back, not re-derive Unverified.
        let (registry2, newly_registered2) = load_and_sync_doc_registry(&project).unwrap();
        assert_eq!(
            newly_registered2, 0,
            "an already-registered doc must not be re-registered"
        );
        assert_eq!(
            registry2.get("architecture").unwrap().state,
            tm_docs::DocState::Stale
        );

        let renderer = test_renderer();
        assert!(
            docs_check(&project, &renderer).is_err(),
            "docs check must fail once the persisted state is Stale"
        );
    }

    #[test]
    fn docs_reconcile_persists_reconciling_state_for_a_later_fresh_registry_load() {
        // Acceptance: "After docs_reconcile, a fresh docs list shows Reconciling."
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        write_tmdocs_toml(root, "docs/architecture.md", "architecture", "generated");
        load_and_sync_doc_registry(&project).unwrap();
        project
            .store
            .invalidate_doc(
                "docs/architecture.md".into(),
                "source changed".into(),
                project.actor.clone(),
            )
            .unwrap();

        let renderer = test_renderer();
        docs_reconcile(&project, &renderer).unwrap();

        let (registry, _) = load_and_sync_doc_registry(&project).unwrap();
        assert_eq!(
            registry.get("architecture").unwrap().state,
            tm_docs::DocState::Reconciling
        );
    }

    #[test]
    fn docs_reconcile_sets_human_required_only_for_review_not_regeneration() {
        // Acceptance: "With one Generated and one Maintained doc both stale, reconcile produces
        // tickets with human_required false and true respectively."
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(
            root.join("docs").join(tm_docs::registry::TMDOCS_TOML),
            "[[doc]]\npath = \"docs/generated.md\"\nid = \"generated\"\nmode = \"generated\"\nderived_from = []\n\
             \n\
             [[doc]]\npath = \"docs/maintained.md\"\nid = \"maintained\"\nmode = \"maintained\"\nderived_from = []\n",
        )
        .unwrap();
        load_and_sync_doc_registry(&project).unwrap();
        project
            .store
            .invalidate_doc(
                "docs/generated.md".into(),
                "source changed".into(),
                project.actor.clone(),
            )
            .unwrap();
        project
            .store
            .invalidate_doc(
                "docs/maintained.md".into(),
                "source changed".into(),
                project.actor.clone(),
            )
            .unwrap();

        let renderer = test_renderer();
        docs_reconcile(&project, &renderer).unwrap();

        let view = project.store.view().unwrap();
        let mut human_required_by_objective: Vec<(String, bool)> = view
            .tickets
            .values()
            .map(|t| (t.objective.clone(), t.executor.human_required))
            .collect();
        human_required_by_objective.sort();

        let generated_ticket = human_required_by_objective
            .iter()
            .find(|(objective, _)| objective.contains("generated.md"))
            .expect("a ticket was opened for the generated doc");
        assert!(
            !generated_ticket.1,
            "a Generated doc's regeneration ticket must not require a human"
        );

        let maintained_ticket = human_required_by_objective
            .iter()
            .find(|(objective, _)| objective.contains("maintained.md"))
            .expect("a ticket was opened for the maintained doc");
        assert!(
            maintained_ticket.1,
            "a Maintained doc's review ticket must require a human"
        );
    }

    #[test]
    fn provider_list_renders_table() {
        // Provider list should render candidates
    }

    // ---- tm provider test: per-provider logic against MockProvider, no network ----

    fn mock_fabric(
        clock: std::sync::Arc<dyn Clock>,
        ids: &[&str],
    ) -> (
        tm_provider::Fabric,
        Vec<std::sync::Arc<tm_provider::MockProvider>>,
    ) {
        let table = tm_provider::RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("static role table parses");
        let fabric = tm_provider::Fabric::new(table, clock.clone());
        let mut providers = Vec::new();
        for id in ids {
            let provider = std::sync::Arc::new(tm_provider::MockProvider::new(
                *id,
                tm_provider::ModelId::new(*id, "served-model"),
                clock.clone(),
            ));
            fabric.register_provider(provider.clone());
            providers.push(provider);
        }
        (fabric, providers)
    }

    fn scripted_ok(provider: &tm_provider::MockProvider, clock: &dyn Clock, text: &str) {
        provider.script_default_response(tm_provider::Completion {
            model: tm_provider::ModelId::new("mock", "served-model"),
            candidates: vec![tm_provider::Candidate {
                content: vec![tm_provider::ContentBlock::Text {
                    text: text.to_string(),
                }],
                stop_reason: tm_provider::StopReason::EndTurn,
            }],
            usage: tm_provider::Usage::default(),
            latency: std::time::Duration::ZERO,
            received_at: clock.now(),
        });
    }

    #[tokio::test]
    async fn provider_test_reports_ok_with_served_model_and_reply() {
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(tm_types::FixedClock::epoch());
        let (fabric, providers) = mock_fabric(clock.clone(), &["mock"]);
        scripted_ok(&providers[0], clock.as_ref(), " OK\n");

        let outcomes = run_provider_tests(&fabric, None, &[], clock.as_ref())
            .await
            .expect("one registered provider to test");
        assert_eq!(
            outcomes,
            vec![ProviderTestOutcome {
                provider: "mock".to_string(),
                status: "ok",
                latency_ms: 0,
                model: Some("served-model".to_string()),
                stop_reason: Some(tm_provider::StopReason::EndTurn),
                reply: Some("OK".to_string()),
                error: None,
            }]
        );
        assert!(provider_test_verdict(&outcomes).is_ok());
        // The provider really received the probe, exactly once.
        assert_eq!(providers[0].call_log(), vec![provider_probe_request()]);
    }

    #[tokio::test]
    async fn provider_test_reports_a_failing_provider_and_the_verdict_fails() {
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(tm_types::FixedClock::epoch());
        let (fabric, providers) = mock_fabric(clock.clone(), &["bad", "good"]);
        providers[0].script_failure(
            &provider_probe_request(),
            tm_provider::mock::ScriptedFailure {
                times: None,
                error: tm_provider::ProviderError::AuthFailed("key rejected".to_string()),
            },
        );
        scripted_ok(&providers[1], clock.as_ref(), "OK");

        let outcomes = run_provider_tests(&fabric, None, &[], clock.as_ref())
            .await
            .expect("two registered providers to test");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].provider, "bad");
        assert_eq!(outcomes[0].status, "error");
        assert_eq!(outcomes[0].model, None);
        assert!(
            outcomes[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("key rejected")),
            "{:?}",
            outcomes[0].error
        );
        assert_eq!(outcomes[1].status, "ok");

        let err = provider_test_verdict(&outcomes).expect_err("one failure must fail the run");
        assert_ne!(crate::render::exit_code(&err), 0);
        assert!(err.to_string().contains("1 of 2"), "{err}");

        let json = serde_json::to_value(&outcomes).expect("serializes");
        assert_eq!(json[0]["status"], "error");
        assert_eq!(json[0]["error"], "authentication failed: key rejected");
        assert_eq!(json[1]["stop_reason"], "end_turn");
        assert_eq!(json[1]["model"], "served-model");
    }

    #[tokio::test]
    async fn provider_test_named_provider_tests_only_that_one_case_insensitively() {
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(tm_types::FixedClock::epoch());
        let (fabric, providers) = mock_fabric(clock.clone(), &["alpha", "beta"]);
        scripted_ok(&providers[0], clock.as_ref(), "OK");
        scripted_ok(&providers[1], clock.as_ref(), "OK");

        let outcomes = run_provider_tests(&fabric, Some("BETA"), &[], clock.as_ref())
            .await
            .expect("beta is registered");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].provider, "beta");
        assert!(
            providers[0].call_log().is_empty(),
            "alpha must not be called"
        );
    }

    #[tokio::test]
    async fn provider_test_unknown_name_is_an_error_never_reachable() {
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(tm_types::FixedClock::epoch());
        let (fabric, providers) = mock_fabric(clock.clone(), &["mock"]);
        let known = tm_provider::Registry::known_providers();

        let err = run_provider_tests(&fabric, Some("no-such-provider"), &known, clock.as_ref())
            .await
            .expect_err("an unknown name must not be reported as reachable");
        assert_ne!(crate::render::exit_code(&err), 0);
        let msg = err.to_string();
        assert!(msg.contains("unknown provider `no-such-provider`"), "{msg}");
        assert!(
            msg.contains("devpass"),
            "should list the known providers: {msg}"
        );
        assert!(providers[0].call_log().is_empty(), "nothing may be called");
    }

    #[tokio::test]
    async fn provider_test_with_an_empty_fabric_is_an_error() {
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(tm_types::FixedClock::epoch());
        let (fabric, _) = mock_fabric(clock.clone(), &[]);
        let err = run_provider_tests(&fabric, None, &[], clock.as_ref())
            .await
            .expect_err("nothing to test is not success");
        assert!(err.to_string().contains("nothing to test"), "{err}");
    }

    #[test]
    fn provider_test_human_lines_are_one_per_provider_and_readable() {
        let ok = ProviderTestOutcome {
            provider: "devpass".to_string(),
            status: "ok",
            latency_ms: 812,
            model: Some("m".to_string()),
            stop_reason: Some(tm_provider::StopReason::MaxTokens),
            reply: Some(String::new()),
            error: None,
        };
        assert_eq!(
            ok.human_line(),
            "devpass: ok in 812 ms (model m, stop max_tokens, reply \"\")"
        );
        let bad = ProviderTestOutcome {
            provider: "anthropic".to_string(),
            status: "error",
            latency_ms: 5,
            model: None,
            stop_reason: None,
            reply: None,
            error: Some("authentication failed: nope".to_string()),
        };
        assert_eq!(
            bad.human_line(),
            "anthropic: error after 5 ms: authentication failed: nope"
        );
    }

    #[test]
    fn provider_test_human_non_quiet_shows_all_lines() {
        let ok = ProviderTestOutcome {
            provider: "devpass".to_string(),
            status: "ok",
            latency_ms: 812,
            model: Some("m".to_string()),
            stop_reason: Some(tm_provider::StopReason::MaxTokens),
            reply: Some(String::new()),
            error: None,
        };
        let bad = ProviderTestOutcome {
            provider: "anthropic".to_string(),
            status: "error",
            latency_ms: 5,
            model: None,
            stop_reason: None,
            reply: None,
            error: Some("auth failed".to_string()),
        };
        let outcomes = vec![ok, bad];

        let human = provider_test_human(&outcomes, false).expect("non-quiet has output");
        assert!(human.contains("devpass: ok"));
        assert!(human.contains("anthropic: error"));
    }

    #[test]
    fn provider_test_human_quiet_suppresses_ok_lines() {
        let ok = ProviderTestOutcome {
            provider: "devpass".to_string(),
            status: "ok",
            latency_ms: 812,
            model: Some("m".to_string()),
            stop_reason: Some(tm_provider::StopReason::MaxTokens),
            reply: Some(String::new()),
            error: None,
        };
        let bad = ProviderTestOutcome {
            provider: "anthropic".to_string(),
            status: "error",
            latency_ms: 5,
            model: None,
            stop_reason: None,
            reply: None,
            error: Some("auth failed".to_string()),
        };
        let outcomes = vec![ok, bad];

        let human = provider_test_human(&outcomes, true).expect("quiet has errors");
        assert!(!human.contains("devpass: ok"));
        assert!(human.contains("anthropic: error"));
    }

    #[test]
    fn provider_test_human_quiet_all_ok_returns_none() {
        let ok1 = ProviderTestOutcome {
            provider: "devpass".to_string(),
            status: "ok",
            latency_ms: 812,
            model: Some("m".to_string()),
            stop_reason: Some(tm_provider::StopReason::MaxTokens),
            reply: Some(String::new()),
            error: None,
        };
        let ok2 = ProviderTestOutcome {
            provider: "anthropic".to_string(),
            status: "ok",
            latency_ms: 5,
            model: Some("m".to_string()),
            stop_reason: Some(tm_provider::StopReason::EndTurn),
            reply: Some("OK".to_string()),
            error: None,
        };
        let outcomes = vec![ok1, ok2];

        let human = provider_test_human(&outcomes, true);
        assert_eq!(human, None, "all-ok quiet returns None");
    }

    #[test]
    fn roles_routed_to_reads_the_table() {
        let table = tm_provider::RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("static role table parses");
        assert_eq!(
            roles_routed_to(&table, "mock"),
            vec![Role::CoderFast.as_str()]
        );
        assert!(roles_routed_to(&table, "anthropic").is_empty());
    }

    #[test]
    fn harness_show_renders_defaults_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();

        harness_show(&project, &renderer).expect("harness_show succeeds on missing file");
        // The test verifies no error is returned and defaults are rendered.
    }

    #[test]
    fn harness_set_creates_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();
        let args = HarnessSetArgs {
            key: "routing.recency_weight".to_string(),
            value: "0.5".to_string(),
        };

        harness_set(&args, &project, &renderer).expect("harness_set succeeds on missing file");
        let harness_path = project.state_dir.join("harness.toml");
        assert!(
            harness_path.is_file(),
            "harness.toml should be created after harness_set"
        );
        // Verify the value was set correctly by parsing the written file
        let content = fs::read_to_string(&harness_path).expect("read written harness.toml");
        let config =
            tm_harness::HarnessConfig::parse(&content).expect("parse written harness.toml");
        assert!(
            (config.routing.recency_weight - 0.5).abs() < 0.001,
            "routing.recency_weight should be set to 0.5, got: {}",
            config.routing.recency_weight
        );
    }

    #[test]
    fn harness_promote_errors_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();
        let args = HarnessPromoteArgs {
            epoch: 1,
            baseline: None,
            report: None,
            force: false,
        };

        let err = harness_promote(&args, &project, &renderer)
            .expect_err("harness_promote should error on missing file");
        let msg = err.to_string();
        assert!(
            msg.contains("No harness configuration found"),
            "Error message should mention missing configuration, got: {msg}"
        );
        assert!(
            msg.contains("tm harness set"),
            "Error message should suggest using `tm harness set`, got: {msg}"
        );
        assert!(
            !msg.contains("invariant"),
            "Error message should not use jargon like 'invariant', got: {msg}"
        );
    }

    #[test]
    fn harness_epochs_lists_epochs() {
        // Harness epochs should list all epochs
    }

    #[test]
    fn bench_list_discovers_tasks() {
        // Bench list should discover and list tasks
    }

    #[test]
    fn bench_run_creates_report() {
        // Bench run should create a report
    }

    /// No other test in this crate mutates `TEST_MOCK_PROVIDER_ENV` inside `ops.rs`, but this
    /// guards against a future one racing this test's transient env mutation under `cargo test`'s
    /// default multi-threaded runner -- same convention as `agent.rs`'s
    /// `devpass_build_fabric_env_lock`/`sched.rs`'s `record_test_env_lock`.
    fn live_bench_test_env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// `bench-live-seeded-provider`'s acceptance check: under `TM_TEST_MOCK_PROVIDER=1`,
    /// `tm bench run --live --filter live-smoke` drives a real ticket to completion through
    /// `crate::sched::run_ticket` (never a reimplementation of it), actually runs the task's
    /// `test_command`, records a cassette next to the report, and folds real numbers into the
    /// resulting `TaskResult`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bench_run_live_produces_a_real_task_result_from_a_real_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = test_renderer();

        let fixture_dir = dir.path().join("bench").join("fixtures").join("live-smoke");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        std::fs::write(fixture_dir.join("NOTE.md"), "live-smoke fixture\n").unwrap();
        std::fs::write(fixture_dir.join("script.txt"), "").unwrap();

        let tasks_dir = dir.path().join("bench").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("live-smoke.toml"),
            r#"
id = "live-smoke"
task = "Reply to confirm you're ready; no code changes are needed."

[fixture]
path = "fixtures/live-smoke"
description = "smoke test fixture"
test_command = ["true"]

[scoring]
success_weight = 1.0
cost_weight = 0.0
latency_weight = 0.0
tool_count_weight = 0.0
context_weight = 0.0
unnecessary_ops_weight = 0.0

[expected]
max_cost_micros = 10000000
max_wall_seconds = 600
max_tool_calls = 50
max_context_bytes = 10000000

[expected.predicate]
tests_pass = {}
"#,
        )
        .unwrap();

        let out_path = dir.path().join("report.json");
        let args = BenchRunArgs {
            filter: Some("live-smoke".to_string()),
            out: Some(out_path.clone()),
            live: true,
        };

        {
            let _guard = live_bench_test_env_lock().lock().await;
            std::env::set_var(crate::agent::TEST_MOCK_PROVIDER_ENV, "1");
            let result = bench_run(&args, &project, &renderer).await;
            std::env::remove_var(crate::agent::TEST_MOCK_PROVIDER_ENV);
            result.expect("a live bench run of the smoke task should succeed");
        }

        let report: tm_harness::BenchmarkReport =
            serde_json::from_str(&std::fs::read_to_string(&out_path).expect("read report"))
                .expect("parse report");
        assert_eq!(
            report.tasks.len(),
            1,
            "the live-smoke task should have run, not been skipped"
        );
        let task_result = &report.tasks[0];
        assert_eq!(task_result.task_id, "live-smoke");
        assert!(
            task_result.passed,
            "the smoke task's `true` test command always exits zero"
        );
        assert!(
            task_result.context_bytes > 0,
            "a real ticket run should record a nonzero token count"
        );

        let cassette_path = project
            .state_dir
            .join("bench")
            .join("live")
            .join("live-smoke.cassette.jsonl");
        assert!(
            cassette_path.is_file(),
            "a cassette should have been recorded next to the report"
        );
    }

    #[test]
    fn mirror_link_parses_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();

        mirror_link(
            &MirrorLinkArgs {
                adapter: "github".to_string(),
                credentials: vec![],
            },
            &project,
            &renderer,
        )
        .expect("link github");

        let mirror_path = project.state_dir.join("mirror.toml");
        let config =
            tm_mirror::MirrorConfig::parse(&fs::read_to_string(&mirror_path).unwrap()).unwrap();
        let adapter = config
            .adapters
            .get("github")
            .expect("github adapter persisted");
        assert_eq!(adapter.kind, tm_mirror::AdapterKind::GitHub);
        assert!(adapter.enabled);
    }

    #[test]
    fn mirror_status_no_active_mirrors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();

        // A fresh project has no mirror.toml at all; status must still succeed, not error on
        // the missing file, and report nothing configured or synced.
        mirror_status(&project, &renderer).expect("status on a fresh project");
        assert!(project.store.mirror_links().unwrap().is_empty());
        assert!(!project.state_dir.join("mirror.toml").is_file());
    }

    #[test]
    fn mirror_status_shows_configured_adapter_before_any_ticket_synced() {
        // s1-mirror-status-and-push-clarity: `tm mirror status` must show a configured adapter
        // (from `mirror.toml`) even when no ticket has ever been pushed/pulled through it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();

        mirror_link(
            &MirrorLinkArgs {
                adapter: "github".to_string(),
                credentials: vec![],
            },
            &project,
            &renderer,
        )
        .expect("link github");

        // No ticket-level sync has happened yet — the old "No active mirror links" report alone
        // would hide the adapter that was just configured.
        assert!(project.store.mirror_links().unwrap().is_empty());

        let mirror_path = project.state_dir.join("mirror.toml");
        let config =
            tm_mirror::MirrorConfig::parse(&fs::read_to_string(&mirror_path).unwrap()).unwrap();
        let adapters: Vec<_> = config.adapters.into_values().collect();
        assert_eq!(adapters.len(), 1);

        let json = mirror_status_adapter_json(&adapters[0]);
        assert_eq!(json["name"], "github");
        assert_eq!(json["enabled"], true);

        let row = mirror_status_adapter_row(&adapters[0]);
        assert_eq!(row[0], "github");
        assert_eq!(row[2], "enabled");
    }

    #[test]
    fn mirror_zero_result_reason_names_no_mirrors_configured() {
        assert_eq!(
            mirror_zero_result_reason(0, 0, false),
            "no mirrors configured"
        );
    }

    #[test]
    fn mirror_zero_result_reason_names_no_tickets_eligible() {
        assert_eq!(mirror_zero_result_reason(1, 0, true), "no tickets eligible");
    }

    #[test]
    fn mirror_zero_result_reason_names_no_adapter_could_authenticate() {
        // s1-mirror-status-and-push-clarity's own evidence scenario: an adapter is configured and
        // there are tickets that would qualify, but every adapter's tracker failed to build (e.g.
        // a missing credential env var) — this must not be misreported as "all already synced".
        assert_eq!(
            mirror_zero_result_reason(1, 3, false),
            "no adapter could authenticate, see the skipped adapters below"
        );
    }

    #[test]
    fn mirror_zero_result_reason_names_all_already_synced() {
        assert_eq!(mirror_zero_result_reason(1, 3, true), "all already synced");
    }

    #[tokio::test]
    async fn mirror_push_with_no_eligible_tickets_does_not_push_anything() {
        // No ticket in the project qualifies under the default `ProjectionPolicy` (no milestone
        // assigned), so a real `tm mirror push` against a configured adapter must still succeed
        // with nothing pushed, rather than erroring or silently pushing something ineligible.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        std::fs::write(
            root.join(".tm").join("mirror.toml"),
            "[adapters.testnull]\nkind = \"null\"\nenabled = true\n",
        )
        .unwrap();

        let renderer = test_renderer();
        mirror_push(&project, &renderer)
            .await
            .expect("push with nothing eligible still succeeds");

        let conn =
            tm_events::schema::open_read_connection(&root.join(".tm").join("project.db")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM effects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "nothing eligible must journal no push effect");
    }

    #[tokio::test]
    async fn mirror_push_with_an_eligible_ticket_but_no_adapter_could_build_pushes_nothing() {
        // s1-mirror-status-and-push-clarity's own evidence scenario: `tm mirror link github`
        // succeeds but leaves `owner`/`repo` unresolved (no env var set for them), so
        // `build_tracker` skips the adapter for every ticket. An eligible ticket does exist here
        // (unlike the sibling test above) — this must still succeed with nothing pushed, and the
        // journal must stay empty, since no adapter ever got the chance to push anything.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        mirrorable_ticket(&project);
        std::fs::write(
            root.join(".tm").join("mirror.toml"),
            "[adapters.github]\nkind = \"github\"\nenabled = true\n[adapters.github.credentials]\ntoken = \"TM_TEST_MIRROR_PUSH_UNSET_TOKEN\"\n",
        )
        .unwrap();
        assert!(
            std::env::var("TM_TEST_MIRROR_PUSH_UNSET_TOKEN").is_err(),
            "test var must not already be set"
        );

        let renderer = test_renderer();
        mirror_push(&project, &renderer)
            .await
            .expect("push must still succeed when every adapter is skipped");

        let conn =
            tm_events::schema::open_read_connection(&root.join(".tm").join("project.db")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM effects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "an all-skipped adapter run must journal no push effect"
        );
    }

    #[test]
    fn mirror_link_still_succeeds_with_an_unset_credential_env_var() {
        // s1-mirror-status-and-push-clarity acceptance: `--credential owner=TM_GITHUB_OWNER`
        // with `TM_GITHUB_OWNER` unset must still link (non-fatal), warning at link time rather
        // than first surfacing the misconfiguration later at push.
        let unset_var = "TM_TEST_MIRROR_LINK_UNSET_XYZ";
        assert!(
            std::env::var(unset_var).is_err(),
            "test var must not already be set"
        );

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();

        mirror_link(
            &MirrorLinkArgs {
                adapter: "github".to_string(),
                credentials: vec![format!("owner={unset_var}")],
            },
            &project,
            &renderer,
        )
        .expect("link must still succeed despite the unset credential env var");

        let mirror_path = project.state_dir.join("mirror.toml");
        assert!(mirror_path.is_file(), "mirror.toml must still be written");
    }

    #[test]
    fn format_unset_credential_warnings_names_field_and_env_var() {
        let warnings = format_unset_credential_warnings(&[(
            "owner".to_string(),
            "TM_GITHUB_OWNER".to_string(),
        )]);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("\"owner\"") && warnings[0].contains("TM_GITHUB_OWNER"),
            "{}",
            warnings[0]
        );
        assert!(warnings[0].contains("not set"), "{}", warnings[0]);
    }

    #[test]
    fn format_unset_credential_warnings_empty_when_nothing_unset() {
        assert!(format_unset_credential_warnings(&[]).is_empty());
    }

    #[test]
    fn events_verify_valid_chain() {
        // With valid chain, verify should succeed
    }

    #[test]
    fn events_show_event_not_found() {
        // Requesting non-existent seq should error
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();

        let err = events_show(&EventsShowArgs { seq: 999 }, &project, &renderer)
            .expect_err("no event at seq 999");
        assert!(matches!(err, tm_types::TmError::NotFound { .. }));
    }

    #[test]
    fn events_show_uses_dotted_kind_and_humanized_labels() {
        // s1-events-sched-copy-and-quiet: `tm events show` must render the event kind via its
        // `Display` (dotted, e.g. "ticket.created"), not `{:?}` debug (e.g. "TicketCreated"), and
        // must use humanized field labels rather than internal shorthand.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        mirrorable_ticket(&project);

        let log =
            tm_events::EventLog::open(&root.join(".tm").join("project.db")).expect("open log");
        let events = log.read_range(1, 1).expect("read seq 1");
        let event = &events[0];
        assert_eq!(event.kind, tm_events::EventKind::TicketCreated);

        let human = event_show_human(event).expect("format human");
        assert!(
            human.contains("Type: ticket.created"),
            "expected dotted event kind in output, got: {human}"
        );
        assert!(!human.contains("TicketCreated"), "got: {human}");
        assert!(human.starts_with("Sequence: "), "got: {human}");
        assert!(human.contains("Related to: "), "got: {human}");
        assert!(human.contains("When: "), "got: {human}");
        assert!(
            human.contains("Details: "),
            "expected the payload to be rendered, got: {human}"
        );
        assert!(
            !human.contains("Payload("),
            "payload should be plain field:value pairs, not a Debug enum repr: {human}"
        );

        let json = event_show_json(event).expect("format json");
        assert_eq!(json["kind"], "ticket.created");
        assert!(
            json["payload"].is_object(),
            "expected a structured payload object, got: {json}"
        );
    }

    #[test]
    fn param_type_label_is_not_debug_formatted() {
        // critic-format-debug-strings-cleanup: `tm templates show` must render a param's type as
        // plain text ("string"/"bool"/"integer"), not `{:?}` debug output (which happens to match
        // here, but the point is this call site no longer depends on the enum's Debug spelling).
        assert_eq!(
            param_type_label(tm_templates::manifest::ParamType::String),
            "string"
        );
        assert_eq!(
            param_type_label(tm_templates::manifest::ParamType::Bool),
            "bool"
        );
        assert_eq!(
            param_type_label(tm_templates::manifest::ParamType::Integer),
            "integer"
        );
    }

    #[test]
    fn events_replay_empty_range() {
        // `to` at or before the log's very first step (nothing durable yet) renders an empty,
        // not a panicking, view.
        let view = tm_core::ProjectView::empty();
        let rendered = events_replay_human(&view, 0);
        assert_eq!(rendered, "Replayed to step 0: 0 ticket(s)");
    }

    #[test]
    fn events_replay_renders_ticket_states_not_an_event_count() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let ticket_id = mirrorable_ticket(&project);

        // Capture the seq right before activating, then activate. `to` at that earlier seq must
        // still show `draft`, proving replay is bounded by seq rather than replaying to HEAD.
        let db_path = project.state_dir.join("project.db");
        let log = tm_events::EventLog::open(&db_path).unwrap();
        let seq_before_activate = log.head().unwrap();
        project
            .store
            .activate(&ticket_id, project.actor.clone())
            .expect("activate");

        let view_before = project
            .store
            .view_as_of(seq_before_activate)
            .expect("view_as_of");
        let rendered_before = events_replay_human(&view_before, seq_before_activate);
        assert!(
            rendered_before.contains("draft: 1"),
            "expected the pre-activation state, got: {rendered_before}"
        );
        assert!(
            !rendered_before.contains("ready:"),
            "must not show the post-activation state, got: {rendered_before}"
        );

        let head = log.head().unwrap();
        let view_after = project.store.view_as_of(head).expect("view_as_of head");
        let rendered_after = events_replay_human(&view_after, head);
        assert!(
            rendered_after.contains("ready: 1"),
            "expected the post-activation state at HEAD, got: {rendered_after}"
        );
    }

    #[test]
    fn format_bench_comparison_candidate_improved_yes() {
        // s1-bench-compare-human-output: format_bench_comparison must show candidate-improved
        // status and aggregate gain for improved candidates.
        let report = tm_harness::PromotionReport {
            baseline_epoch: 0,
            candidate_epoch: 1,
            candidate_improved: true,
            aggregate_gain: 0.15,
            task_deltas: vec![("task1".to_string(), 0.1), ("task2".to_string(), 0.05)],
        };
        let formatted = format_bench_comparison(&report);
        assert!(formatted.contains("Baseline epoch 0 vs candidate epoch 1"));
        assert!(formatted.contains("Candidate improved: yes"));
        assert!(formatted.contains("Aggregate gain: +0.15"));
        assert!(formatted.contains("Tasks: 2 improved, 0 regressed, 0 unchanged"));
    }

    #[test]
    fn format_bench_comparison_candidate_not_improved() {
        // Candidate not improved must show "no" and negative/zero aggregate gain.
        let report = tm_harness::PromotionReport {
            baseline_epoch: 0,
            candidate_epoch: 1,
            candidate_improved: false,
            aggregate_gain: 0.0,
            task_deltas: vec![("hello-world".to_string(), 0.0)],
        };
        let formatted = format_bench_comparison(&report);
        assert!(formatted.contains("Candidate improved: no"));
        assert!(formatted.contains("Aggregate gain: +0.00"));
    }

    #[test]
    fn format_bench_comparison_mixed_task_deltas() {
        // Task deltas with improvements, regressions, and unchanged tasks must be counted
        // correctly.
        let report = tm_harness::PromotionReport {
            baseline_epoch: 0,
            candidate_epoch: 1,
            candidate_improved: false,
            aggregate_gain: -0.05,
            task_deltas: vec![
                ("task1".to_string(), 0.1),   // improved
                ("task2".to_string(), -0.05), // regressed
                ("task3".to_string(), -0.1),  // regressed
                ("task4".to_string(), 0.0),   // unchanged
            ],
        };
        let formatted = format_bench_comparison(&report);
        assert!(formatted.contains("Tasks: 1 improved, 2 regressed, 1 unchanged"));
    }

    #[test]
    fn format_bench_comparison_no_task_deltas() {
        // When task_deltas is empty, the task summary line must not be present.
        let report = tm_harness::PromotionReport {
            baseline_epoch: 0,
            candidate_epoch: 1,
            candidate_improved: false,
            aggregate_gain: 0.0,
            task_deltas: vec![],
        };
        let formatted = format_bench_comparison(&report);
        assert!(!formatted.contains("Tasks:"));
        assert!(formatted.contains("Candidate improved: no"));
        assert!(formatted.contains("Aggregate gain: +0.00"));
    }

    #[test]
    fn format_bench_comparison_negative_aggregate_gain() {
        // Negative aggregate gain must be formatted with a minus sign.
        let report = tm_harness::PromotionReport {
            baseline_epoch: 0,
            candidate_epoch: 1,
            candidate_improved: false,
            aggregate_gain: -0.25,
            task_deltas: vec![],
        };
        let formatted = format_bench_comparison(&report);
        assert!(formatted.contains("Aggregate gain: -0.25"));
    }

    /// A minimal, hash-unverified `Event` for exercising [`event_matches`], which only reads
    /// `kind` and `subject` — mirrors `tm_events::stream::tests::make_test_event`.
    fn make_event(seq: u64, kind: tm_events::EventKind, subject: &str) -> tm_events::Event {
        tm_events::Event {
            seq,
            ts: Timestamp::EPOCH,
            kind,
            subject: tm_types::Id::new(subject),
            actor: tm_types::ParticipantId::system(),
            session: None,
            causation: None,
            correlation: None,
            payload: tm_events::Payload::SessionStarted(
                tm_events::payload::SessionStartedPayload {
                    session: tm_types::SessionId::new("S-1").unwrap(),
                    participant: tm_types::ParticipantId::system(),
                },
            ),
            hash: "hash".to_string(),
        }
    }

    #[test]
    fn event_matches_with_no_filter_admits_everything() {
        let event = make_event(1, tm_events::EventKind::TicketClosed, "T-1");
        assert!(event_matches(&event, &EventFilter::default()));
    }

    #[test]
    fn event_matches_filters_by_kind() {
        let closed = make_event(1, tm_events::EventKind::TicketClosed, "T-1");
        let created = make_event(2, tm_events::EventKind::TicketCreated, "T-1");
        let filter = EventFilter {
            kind: Some(tm_events::EventKind::TicketClosed),
            ticket: None,
        };
        assert!(event_matches(&closed, &filter));
        assert!(!event_matches(&created, &filter));
    }

    #[test]
    fn event_matches_filters_by_ticket() {
        let t1 = make_event(1, tm_events::EventKind::TicketClosed, "T-1");
        let t2 = make_event(2, tm_events::EventKind::TicketClosed, "T-2");
        let filter = EventFilter {
            kind: None,
            ticket: Some("T-1".to_string()),
        };
        assert!(event_matches(&t1, &filter));
        assert!(!event_matches(&t2, &filter));
    }

    #[test]
    fn event_matches_combined_filter_requires_both() {
        let matches_both = make_event(1, tm_events::EventKind::TicketClosed, "T-1");
        let wrong_kind = make_event(2, tm_events::EventKind::TicketCreated, "T-1");
        let wrong_ticket = make_event(3, tm_events::EventKind::TicketClosed, "T-2");
        let filter = EventFilter {
            kind: Some(tm_events::EventKind::TicketClosed),
            ticket: Some("T-1".to_string()),
        };
        assert!(event_matches(&matches_both, &filter));
        assert!(!event_matches(&wrong_kind, &filter));
        assert!(!event_matches(&wrong_ticket, &filter));
    }

    fn append_test_events(log: &tm_events::EventLog) {
        use tm_events::payload::{ProjectCreatedPayload, TicketCreatedPayload};

        log.append(tm_events::EventDraft::new(
            tm_types::ParticipantId::system(),
            tm_types::Id::none(),
            tm_events::Payload::from(ProjectCreatedPayload {
                name: "demo".into(),
                root: "/tmp/demo".into(),
            }),
        ))
        .unwrap();
        let ticket = tm_types::TicketId::new("T-1").unwrap();
        log.append(tm_events::EventDraft::new(
            tm_types::ParticipantId::system(),
            tm_types::Id::from(ticket.clone()),
            tm_events::Payload::from(TicketCreatedPayload {
                ticket: ticket.clone(),
                title: "do it".into(),
                parent: None,
            }),
        ))
        .unwrap();
        let other_ticket = tm_types::TicketId::new("T-2").unwrap();
        log.append(tm_events::EventDraft::new(
            tm_types::ParticipantId::system(),
            tm_types::Id::from(other_ticket.clone()),
            tm_events::Payload::from(TicketCreatedPayload {
                ticket: other_ticket,
                title: "do the other thing".into(),
                parent: None,
            }),
        ))
        .unwrap();
    }

    #[test]
    fn tail_backlog_no_follow_returns_only_matching_events_over_a_scratch_store() {
        // Mixed events (project.created, two ticket.created for different tickets); filtering by
        // --ticket must return exactly the one event about that ticket, per `tel-events-kind-
        // ticket-filter`'s acceptance check.
        let dir = tempfile::tempdir().unwrap();
        let log = tm_events::EventLog::open(&dir.path().join("project.db")).unwrap();
        append_test_events(&log);
        assert_eq!(log.head().unwrap(), 3);

        let filter = EventFilter {
            kind: None,
            ticket: Some("T-1".to_string()),
        };
        let (matched, last_seen) = tail_backlog(&log, 1, &filter).unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].subject.as_str(), "T-1");
        assert_eq!(
            last_seen, 3,
            "last_seen tracks the head regardless of filtering"
        );
    }

    #[test]
    fn tail_backlog_kind_filter_matches_across_both_tickets() {
        let dir = tempfile::tempdir().unwrap();
        let log = tm_events::EventLog::open(&dir.path().join("project.db")).unwrap();
        append_test_events(&log);

        let filter = EventFilter {
            kind: Some(tm_events::EventKind::TicketCreated),
            ticket: None,
        };
        let (matched, _) = tail_backlog(&log, 1, &filter).unwrap();
        assert_eq!(matched.len(), 2);
    }

    #[tokio::test]
    async fn events_tail_rejects_an_unrecognized_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = test_project(root);
        let renderer = test_renderer();

        let args = EventsTailArgs {
            from: None,
            kind: Some("not.a.real.kind".to_string()),
            ticket: None,
            no_follow: true,
        };
        let err = events_tail(&args, &project, &renderer)
            .await
            .expect_err("unknown kind must be rejected");
        assert!(err.to_string().contains("not.a.real.kind"));
    }
}
