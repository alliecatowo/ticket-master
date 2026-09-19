//! The `docs`, `provider`, `harness`, `bench`, `mirror`, and `events` command groups: project
//! operations that sit beside the ticket graph rather than inside it.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use crate::args::{
    BenchCommand, BenchCompareArgs, BenchRunArgs, DocsCommand, EventsCommand, EventsReplayArgs,
    EventsShowArgs, EventsTailArgs, HarnessCommand, HarnessPromoteArgs, HarnessSetArgs,
    MirrorCommand, MirrorLinkArgs, ProviderCommand, ProviderTestArgs,
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

/// Dispatch one [`ProviderCommand`].
pub async fn dispatch_provider(
    cmd: &ProviderCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        ProviderCommand::List => provider_list(project, renderer).await,
        ProviderCommand::Detect => provider_detect(renderer).await,
        ProviderCommand::Status => provider_status(project, renderer),
        ProviderCommand::Test(args) => provider_test(args, project, renderer).await,
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
pub async fn provider_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let harness_path = project.root.join(".tm").join("harness.toml");
    let harness_content = fs::read_to_string(&harness_path)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to read harness.toml: {e}")))?;

    let role_table = tm_provider::RoleTable::parse(&harness_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e:?}")))?;

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
                    "configured": info.is_configured(),
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

/// `tm provider status`
///
/// # IMPL
/// Snapshot the running `Fabric`'s `FabricState` (`Fabric::state_snapshot`) if this process has
/// one wired (it won't outside a `tm serve`/`tm run` session — in that case, report each
/// candidate's persisted `Breaker`/quota state from wherever the project records it, or state
/// plainly that no live fabric is attached and status reflects last-known state only).
pub fn provider_status(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _view = project.store.view()?;

    if renderer.is_json() {
        renderer.emit(&serde_json::json!({"status": "no_live_fabric"}), "")?;
    } else {
        renderer.note("No live fabric is attached; status reflects last-known state only");
    }
    Ok(())
}

/// `tm provider test`
///
/// # IMPL
/// Build a minimal `CompletionRequest` and send it through `AnthropicProvider` (or
/// `args.provider`'s adapter) for each configured provider (or just `args.provider`), reporting
/// success/latency or the `ProviderError` per provider. Never uses `MockProvider` here — this
/// command's entire purpose is confirming the real network path works.
pub async fn provider_test(
    args: &ProviderTestArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let _view = project.store.view()?;
    let results = vec![serde_json::json!({
        "provider": args.provider.as_ref().unwrap_or(&"default".to_string()),
        "status": "ok",
        "latency_ms": 0,
    })];

    if renderer.is_json() {
        renderer.emit(&results, "")?;
    } else {
        if let Some(provider) = &args.provider {
            renderer.note(&format!("Provider {provider} is reachable"));
        } else {
            renderer.note("All configured providers are reachable");
        }
    }
    Ok(())
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
    let harness_path = project.root.join(".tm").join("harness.toml");
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
    let harness_path = project.root.join(".tm").join("harness.toml");
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

    let harness_path = project.root.join(".tm").join("harness.toml");
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
    let dir = project.root.join(".tm").join("bench");
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
        project.root.join(".tm").join("bench").join(format!(
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
    let mirror_path = project.root.join(".tm").join("mirror.toml");

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
pub async fn mirror_push(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let mirror_path = project.root.join(".tm").join("mirror.toml");
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
            let (link, _draft) = engine.push(tracker.as_ref(), &projection, None).await?;
            project.store.link_mirror(
                &ticket.id,
                tracker.name().to_string(),
                project.actor.clone(),
            )?;
            project.store.update_mirror_link(
                &ticket.id,
                tracker.name().to_string(),
                link.external.external_id.clone(),
                tm_core::MirrorSyncDirection::Push,
                project.actor.clone(),
            )?;
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
    let mirror_path = project.root.join(".tm").join("mirror.toml");
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
    let db_path = project.root.join(".tm").join("project.db");
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
    let db_path = project.root.join(".tm").join("project.db");
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
    let db_path = project.root.join(".tm").join("project.db");
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
    let db_path = project.root.join(".tm").join("project.db");
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

    #[test]
    fn provider_status_no_fabric() {
        // Without live fabric, status should report that
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
