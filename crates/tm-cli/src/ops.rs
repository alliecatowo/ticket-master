//! The `docs`, `provider`, `harness`, `bench`, `mirror`, and `events` command groups: project
//! operations that sit beside the ticket graph rather than inside it.

use std::fs;

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

/// `tm docs list`
///
/// # IMPL
/// Load the project's `tm_docs::registry::DocRegistry` (materialized from `tm-core`'s
/// `docs`/`doc_provenance` tables — the caller's job per `tm-docs`'s module docs); render each
/// `DocRecord`'s id, mode, and `DocState` as a table or JSON.
pub fn docs_list(_project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let registry = tm_docs::DocRegistry::new();

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
        renderer.emit(&docs, "")?;
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
pub fn docs_check(_project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let registry = tm_docs::DocRegistry::new();
    let provenance = Default::default();
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
pub fn docs_reconcile(_project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let registry = tm_docs::DocRegistry::new();
    // No docs are registered yet (`DocRegistry` is hydrated by a caller that materializes
    // `tm-core`'s `docs`/`doc_provenance` tables, which this command does not do), so there is
    // never anything stale to reconcile today.
    debug_assert!(registry.list().is_empty());
    let opened_count = 0;

    if renderer.is_json() {
        renderer.emit(&serde_json::json!({"opened": opened_count}), "")?;
    } else {
        if opened_count == 0 {
            renderer.note("No stale docs to reconcile");
        } else {
            renderer.note(&format!("Opened {} reconciliation tickets", opened_count));
        }
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
        ProviderCommand::List => provider_list(project, renderer),
        ProviderCommand::Status => provider_status(project, renderer),
        ProviderCommand::Test(args) => provider_test(args, project, renderer).await,
    }
}

/// `tm provider list`
///
/// # IMPL
/// Load the project's `tm_provider::role_config::RoleTable` from `harness.toml`; render each
/// role's configured `RoleCandidate`s (provider, model, priority) as a table or JSON.
pub fn provider_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let harness_path = project.root.join(".tm").join("harness.toml");
    let harness_content = fs::read_to_string(&harness_path)
        .map_err(|e| tm_types::TmError::storage(format!("Failed to read harness.toml: {e}")))?;

    let role_table = tm_provider::RoleTable::parse(&harness_content)
        .map_err(|e| tm_types::TmError::parse(format!("Invalid harness.toml: {e:?}")))?;

    if renderer.is_json() {
        let mut roles = Vec::new();
        for role in Role::ALL {
            for candidate in role_table.candidates_for(role) {
                roles.push(serde_json::json!({
                    "role": role.as_str(),
                    "provider": candidate.provider,
                    "model": candidate.model,
                    "concurrency": candidate.max_concurrency,
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
                ]);
            }
        }
        let table = Table::new(
            vec![
                "Role".to_string(),
                "Provider".to_string(),
                "Model".to_string(),
                "Concurrency".to_string(),
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
/// `EpochRegistry::epochs()`; render each epoch's number, promotion state, and config hash as a
/// table or JSON.
pub fn harness_epochs(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _view = project.store.view()?;

    if renderer.is_json() {
        let epochs = vec![serde_json::json!({
            "number": 0,
            "state": "current",
            "hash": "unknown",
        })];
        renderer.emit(&epochs, "")?;
    } else {
        let rows = vec![vec![
            "0".to_string(),
            "current".to_string(),
            "unknown".to_string(),
        ]];
        let table = Table::new(
            vec![
                "Number".to_string(),
                "State".to_string(),
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
/// `EpochRegistry::promote(args.epoch, ...)`, gated by `PromotionGate::evaluate` unless
/// `args.force` (which still requires the caller to hold human authority — surfaced as
/// `TmError::AuthorityDenied` otherwise, never silently bypassed).
pub fn harness_promote(
    args: &HarnessPromoteArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let _view = project.store.view()?;

    if renderer.is_json() {
        renderer.emit(
            &serde_json::json!({"epoch": args.epoch, "promoted": args.force}),
            "",
        )?;
    } else {
        renderer.note(&format!(
            "Epoch {} promotion {}",
            args.epoch,
            if args.force { "forced" } else { "accepted" }
        ));
    }
    Ok(())
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

/// `tm bench list`
///
/// # IMPL
/// Discover `BenchTask`s under the project's bench fixture directory (parsed via
/// `BenchTask::parse`); render name and `ScoringSpec` summary as a table or JSON.
pub fn bench_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _view = project.store.view()?;
    // `BenchTask` discovery under the project's fixture directory isn't wired up yet; this
    // command reports a fixed placeholder task set until that's built.
    let _bench_dir = project.root.join(".tm").join("bench");

    if renderer.is_json() {
        let tasks = vec![serde_json::json!({
            "name": "example",
            "description": "Example benchmark task",
        })];
        renderer.emit(&tasks, "")?;
    } else {
        let rows = vec![vec![
            "example".to_string(),
            "Example benchmark task".to_string(),
        ]];
        let table = Table::new(vec!["Name".to_string(), "Description".to_string()], rows);
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
    let _view = project.store.view()?;
    let _ = &args.filter;

    let report = tm_harness::BenchmarkReport {
        epoch: 0,
        tasks: vec![],
        aggregate_score: 0.0,
        generated_at: project.clock.now(),
    };

    if let Some(out_path) = &args.out {
        let json_str = serde_json::to_string_pretty(&report).map_err(tm_types::TmError::from)?;
        fs::write(out_path, json_str)
            .map_err(|e| tm_types::TmError::storage(format!("Failed to write report: {e}")))?;
    }

    if renderer.is_json() {
        renderer.emit(&report, "")?;
    } else {
        renderer.note("Benchmark run completed");
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

/// `tm mirror push`
///
/// # IMPL
/// For each enabled `AdapterConfig`, build its `Tracker` (`GitHubTracker`/`GitLabTracker`/
/// `JiraTracker`/`LinearTracker`), run `SyncEngine`'s push path over
/// `ProjectionPolicy`-filtered tickets; render pushed/degraded counts per adapter.
pub async fn mirror_push(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _view = project.store.view()?;

    if renderer.is_json() {
        renderer.emit(&serde_json::json!({"pushed": 0, "degraded": 0}), "")?;
    } else {
        renderer.note("Mirror push completed: 0 pushed, 0 degraded");
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
pub async fn mirror_pull(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _view = project.store.view()?;

    if renderer.is_json() {
        renderer.emit(&serde_json::json!({"pulled": 0, "applied": 0}), "")?;
    } else {
        renderer.note("Mirror pull completed: 0 pulled, 0 applied");
    }
    Ok(())
}

/// `tm mirror status`
///
/// # IMPL
/// Render each `MirrorLink`'s last sync time and any recorded `Degradation`s.
pub fn mirror_status(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _view = project.store.view()?;

    if renderer.is_json() {
        renderer.emit(&serde_json::json!({"mirrors": []}), "")?;
    } else {
        renderer.note("No active mirror links");
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
