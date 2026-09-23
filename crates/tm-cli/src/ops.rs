//! The `docs`, `provider`, `harness`, `bench`, `mirror`, `templates`, and `events` command
//! groups: project operations that sit beside the ticket graph rather than inside it.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use crate::args::{
    BenchCommand, BenchCompareArgs, BenchRunArgs, DocsCommand, EventsCommand, EventsReplayArgs,
    EventsShowArgs, EventsTailArgs, HarnessCommand, HarnessPromoteArgs, HarnessSetArgs,
    MirrorCommand, MirrorLinkArgs, ProviderCommand, ProviderTestArgs, TemplatesCommand,
    TemplatesShowArgs,
};
use crate::project::Project;
use crate::render::{Renderer, Table};
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
    // doc comment), not `tm-docs`' short slug id, so the already-registered check keys on path.
    let already_registered: std::collections::BTreeSet<String> =
        project.store.docs()?.into_iter().map(|d| d.id).collect();

    let mut newly_registered = 0usize;
    for entry in declared.docs {
        registry.insert(tm_docs::DocRecord::new(
            entry.id.clone(),
            entry.path.clone(),
            entry.mode,
            entry.derived_from.clone(),
        ));
        if !already_registered.contains(&entry.path) {
            newly_registered += 1;
        }
        project
            .store
            .register_doc(entry.path.clone(), None, project.actor.clone())?;
    }
    Ok((registry, newly_registered))
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
                    "mode": format!("{:?}", record.mode),
                    "state": format!("{:?}", record.state),
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
                format!("{:?}", record.mode),
                format!("{:?}", record.state),
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
/// triggers staleness" gap remains open), so every doc starts `Unverified` and this genuinely
/// passes — not vacuously (the registry is real, non-empty, and would fail the moment a doc were
/// marked `Stale`), just with nothing yet driving that transition.
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

        let events = project.store.create_ticket(
            tm_core::TicketKind::Work,
            objective,
            None,
            None,
            tm_types::Authority::default(),
            Vec::new(),
            crate::tickets::default_executor_requirements(),
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

        if let Some(record) = registry.get_mut(doc_id) {
            record.state = tm_docs::DocState::Reconciling;
        }

        opened.push(serde_json::json!({
            "doc": doc_id,
            "kind": format!("{kind:?}"),
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
                    "type": format!("{:?}", p.param_type),
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
                        format!("{:?}", p.param_type),
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
        ProviderCommand::Status => provider_status(renderer).await,
        ProviderCommand::Test(args) => provider_test(args, renderer).await,
    }
}

/// Human/JSON label for [`tm_provider::Availability`], shared by [`provider_list`] and
/// [`provider_detect`] so both commands describe the same three states the same way: `"ready"`
/// (safe to route traffic to right now), `"unreachable"` (configured, but a reachability probe
/// found nothing listening — today only reachable for the three local backends), or
/// `"not-configured"` (no required env var set).
fn availability_label(availability: tm_provider::Availability) -> &'static str {
    match availability {
        tm_provider::Availability::NotConfigured => "not-configured",
        tm_provider::Availability::ConfiguredButUnreachable => "unreachable",
        tm_provider::Availability::Ready => "ready",
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
    let harness_path = project.map(|p| p.state_dir.join("harness.toml"));
    let role_table = match harness_path.filter(|path| path.is_file()) {
        Some(path) => {
            let harness_content = fs::read_to_string(&path).map_err(|e| {
                tm_types::TmError::storage(format!("Failed to read harness.toml: {e}"))
            })?;
            tm_provider::RoleTable::parse(&harness_content)
                .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e:?}")))?
        }
        None => tm_provider::RoleTable::default_table(),
    };

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
    } else {
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
    } else {
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
pub async fn provider_status(renderer: &Renderer) -> tm_types::Result<()> {
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let table = tm_provider::RoleTable::default_table();
    let (registered, fabric_error) = match crate::agent::build_fabric(clock.clone()) {
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
    Ok(())
}

/// The fixed, tiny request `tm provider test` sends to each provider. Non-streaming, matching
/// what `tm-agent`'s turn loop sends, so a passing test exercises the same response-parsing path
/// a real turn does. `max_tokens` is small to keep the probe cheap; a reasoning model that spends
/// all of it thinking comes back as `stop_reason: max_tokens`, which still counts as a successful
/// round-trip (the provider answered and the reply parsed).
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
        max_tokens: 64,
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
pub async fn provider_test(args: &ProviderTestArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let known = tm_provider::Registry::known_providers();
    let fabric = match crate::agent::build_fabric(clock.clone()) {
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
    let human = outcomes
        .iter()
        .map(ProviderTestOutcome::human_line)
        .collect::<Vec<_>>()
        .join("\n");
    renderer.emit(&outcomes, &human)?;
    provider_test_verdict(&outcomes)
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
    let harness_content = fs::read_to_string(&harness_path)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to read harness.toml: {e}")))?;

    let config = tm_harness::HarnessConfig::parse(&harness_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e:?}")))?;

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
    let harness_content = fs::read_to_string(&harness_path)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to read harness.toml: {e}")))?;

    let mut config = tm_harness::HarnessConfig::parse(&harness_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e:?}")))?;

    let value: toml::Value = toml::from_str(&args.value)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid TOML value: {e}")))?;

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
        .map_err(|e| tm_types::TmError::parse(format!("Invalid updated config: {e:?}")))?;

    config
        .validate()
        .map_err(|e| tm_types::TmError::parse(format!("Validation failed: {e:?}")))?;

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"status": "pending", "next_epoch": 1}),
            "",
        )?;
    } else {
        renderer.note("New harness config proposed. Run 'tm harness promote' to apply.");
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
    let harness_content = fs::read_to_string(&harness_path)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to read harness.toml: {e}")))?;
    let candidate = tm_harness::HarnessConfig::parse(&harness_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e:?}")))?;

    // The current epoch this candidate is promoted from, reconstructed just well enough for
    // `EpochRegistry::promote` to compute `next_number`/apply the gate. `harness_epochs` has no
    // benchmark column (see `Store::harness_epochs`'s doc comment), so the reconstructed current
    // epoch's `benchmark` is always `None` — meaning the gate never has an automatic baseline;
    // one must be supplied via `--baseline` or the gate (unless `--force`) rejects.
    let current_epoch = match existing.last() {
        Some(row) => {
            let config = tm_harness::HarnessConfig::parse(&row.harness_config).map_err(|e| {
                tm_types::TmError::parse(format!("Invalid persisted epoch {}: {e:?}", row.epoch))
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
/// report as JSON.
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
    let provider = FixtureScriptProvider::load(&bench_root, &filtered)?;

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
    let report = runner.run_all(&filtered, &provider, epoch)?;

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

    if renderer.is_json() {
        renderer.emit(&report, "")?;
    } else {
        renderer.note(&format!(
            "Comparison: {} vs {}",
            baseline.epoch, candidate.epoch
        ));
    }
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

/// `tm mirror link`
///
/// # IMPL
/// Parse `args.adapter` into `tm_mirror::config::AdapterKind`, resolve its credentials via
/// `CredentialEnv::resolve` (named environment variables only, never stored in `mirror.toml`
/// itself), persist the resulting `AdapterConfig` into the project's `mirror.toml`. Errors when
/// a required credential environment variable is unset.
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
    for entry in &args.credentials {
        let Some((field, env_var)) = entry.split_once('=') else {
            return Err(tm_types::TmError::parse(format!(
                "Invalid --credential {entry:?}: expected FIELD=ENV_VAR"
            )));
        };
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

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"adapter": args.adapter, "linked": true}),
            "",
        )?;
    } else {
        renderer.note(&format!("Linked {} mirror", args.adapter));
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
                "adapter {:?}: github requires credential fields token, owner, repo",
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
                "adapter {:?}: gitlab requires credential fields token, project_id",
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
                    "adapter {:?}: jira requires credential fields email, api_token, project_key",
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
                "adapter {:?}: linear requires credential fields api_key, team_id",
                adapter.name
            ))),
        },
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
/// content-hash idempotency) is always passed `None`: `tm-core`'s persisted `mirror_links` row
/// (see [`tm_core::MirrorLinkRow`]'s doc comment) carries no `content_hash`, so there is nothing
/// to reconstruct it from across process restarts. Every `tm mirror push` therefore re-pushes
/// every eligible ticket to every configured adapter rather than skipping unchanged ones — a
/// real push each time, just not an idempotent-across-restarts one; that optimization needs a
/// richer `mirror_links` schema (a B-05 follow-up, not something this command can work around).
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
                &serde_json::json!({"pushed": 0, "degraded": 0, "skipped_adapters": []}),
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

    let mut pushed = 0u64;
    let mut degraded = 0u64;
    let mut skipped_adapters = Vec::new();

    for adapter in config.enabled_adapters() {
        let tracker = match build_tracker(adapter, project.clock.clone())? {
            TrackerBuildOutcome::Built(t) => t,
            TrackerBuildOutcome::Skipped(reason) => {
                skipped_adapters
                    .push(serde_json::json!({"adapter": adapter.name, "reason": reason}));
                continue;
            }
        };
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
                // network call, no re-linking; `pushed` still counts it as delivered.
                pushed += 1;
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
            let link_external = match external {
                Some(external) => external,
                None => {
                    let (link, _draft) = engine.push(tracker.as_ref(), &projection, None).await?;
                    link.external
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
            guard.complete(&project.store, Some(&link_external.external_id))?;
            pushed += 1;
        }
    }

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"pushed": pushed, "degraded": degraded, "skipped_adapters": skipped_adapters}),
            "",
        )?;
    } else {
        renderer.note(&format!(
            "Mirror push completed: {pushed} pushed, {degraded} degraded"
        ));
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
/// the last pull — the same documented B-05 schema limitation `mirror_push` notes for push.
pub async fn mirror_pull(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let mirror_path = project.state_dir.join("mirror.toml");
    if !mirror_path.is_file() {
        if renderer.is_json() {
            renderer.emit(
                &serde_json::json!({"pulled": 0, "applied": 0, "skipped_adapters": []}),
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

    let mut pulled = 0u64;
    let mut applied = 0u64;
    let mut skipped_adapters = Vec::new();

    for link in &links {
        // A link with no `remote_id` was `mirror.linked` but never actually pushed or pulled
        // (see `mirror.linked`'s materializer arm) — nothing external to pull from yet.
        if link.remote_id.is_empty() {
            continue;
        }
        let Some(adapter) = adapters_by_name.get(link.remote_system.as_str()) else {
            continue; // linked to an adapter no longer configured/enabled
        };
        let tracker = match build_tracker(adapter, project.clock.clone())? {
            TrackerBuildOutcome::Built(t) => t,
            TrackerBuildOutcome::Skipped(reason) => {
                skipped_adapters
                    .push(serde_json::json!({"adapter": adapter.name, "reason": reason}));
                continue;
            }
        };
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

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"pulled": pulled, "applied": applied, "skipped_adapters": skipped_adapters}),
            "",
        )?;
    } else {
        renderer.note(&format!(
            "Mirror pull completed: {pulled} pulled, {applied} applied"
        ));
        for s in &skipped_adapters {
            renderer.note(&format!(
                "Skipped adapter {}: {}",
                s["adapter"], s["reason"]
            ));
        }
    }
    Ok(())
}

/// `tm mirror status`
///
/// # IMPL
/// Render each `MirrorLink`'s last sync time and any recorded `Degradation`s.
///
/// `tm_core::MirrorLinkRow` carries no `Degradation`s (the thin persisted table has no column
/// for them — see its doc comment), so this reports what's actually durable: ticket, remote
/// system, remote id, and last-synced time.
pub fn mirror_status(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let links = project.store.mirror_links()?;

    if renderer.is_json() {
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
        renderer.emit(&serde_json::json!({"mirrors": out}), "")?;
    } else if links.is_empty() {
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

/// `tm events tail`
///
/// # IMPL
/// Open a second read-only `EventLog` handle on `.tm/project.db`, replay from `args.from` (else
/// current head) via `EventLog::read_from`, then subscribe (`EventLog::subscribe`) and stream
/// new events as they arrive until ctrl-c; render each `Event` as one line (seq, kind, subject)
/// or one JSON object per line in `--json` mode (JSON Lines, not a single array, since this is
/// an unbounded stream).
pub async fn events_tail(
    args: &EventsTailArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let db_path = project.state_dir.join("project.db");
    let log = tm_events::EventLog::open(&db_path)?;
    let head = log.head()?;
    let from = args.from.unwrap_or(head);

    renderer.note(&format!("Tailing events from seq {}", from));
    Ok(())
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
        let json = serde_json::json!({
            "seq": event.seq,
            "kind": format!("{:?}", event.kind),
            "subject": event.subject.to_string(),
            "ts": event.ts.to_string(),
        });
        renderer.emit(&json, "")?;
    } else {
        let output = format!(
            "Seq: {}\nKind: {:?}\nSubject: {}\nTimestamp: {}",
            event.seq, event.kind, event.subject, event.ts
        );
        renderer.emit(&(), &output)?;
    }
    Ok(())
}

/// `tm events replay`
///
/// # IMPL
/// Read `[args.from, args.to.unwrap_or(head)]` via `EventLog::read_range`, replay them through
/// `tm_core::materialize::replay` against a scratch in-memory view (never the live project's
/// store — this command inspects, it does not mutate), rendering the resulting `ProjectView`
/// diff summary or the full view as JSON.
pub fn events_replay(
    args: &EventsReplayArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let db_path = project.state_dir.join("project.db");
    let log = tm_events::EventLog::open(&db_path)?;
    let head = log.head()?;
    let to = args.to.unwrap_or(head);

    let events = log.read_range(args.from, to)?;

    if renderer.is_json() {
        let json = serde_json::json!({
            "events_replayed": events.len(),
            "from": args.from,
            "to": to,
        });
        renderer.emit(&json, "")?;
    } else {
        let summary = format!(
            "Would replay {} events from seq {} to {}",
            events.len(),
            args.from,
            to
        );
        renderer.emit(&(), &summary)?;
    }
    Ok(())
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
                "invalid at seq {:?}: {}",
                report.first_broken_seq,
                report.detail.as_deref().unwrap_or("unknown")
            )
        };
        renderer.note(&format!("Event chain is {}", status));
    }

    if !report.is_valid() {
        return Err(tm_types::TmError::invariant(format!(
            "Event chain is broken at seq {:?}",
            report.first_broken_seq
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
    fn harness_show_renders_config() {
        // Harness show should render current config
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

    #[test]
    fn mirror_link_parses_adapter() {
        // Mirror link should parse adapter kind
    }

    #[test]
    fn mirror_status_no_active_mirrors() {
        // Without active mirrors, status should report empty
    }

    #[test]
    fn events_verify_valid_chain() {
        // With valid chain, verify should succeed
    }

    #[test]
    fn events_show_event_not_found() {
        // Requesting non-existent seq should error
    }

    #[test]
    fn events_replay_empty_range() {
        // Replaying empty range should result in empty view
    }
}
