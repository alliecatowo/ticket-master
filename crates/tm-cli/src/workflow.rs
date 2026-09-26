//! `tm workflow list|show|run`: `SPEC.md` §25's workflow definitions.
//!
//! Definitions are discovered from `.tm/workflows/*.toml`, one file per workflow (see
//! `tm_workflow::WorkflowDef`'s own doc comment for why one file is one workflow rather than
//! §25.2's illustrative multi-workflow-per-file shape). This module owns discovery/parsing/
//! rendering only; the actual expand-validate-commit pipeline is `tm_workflow::expand` +
//! `tm_genesis::compile::validate_graph` + `tm_workflow::commit`, called in that order by
//! [`workflow_run`] below and nowhere reimplemented.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use tm_types::TmError;

use crate::args::{WorkflowCommand, WorkflowRunArgs, WorkflowShowArgs};
use crate::project::Project;
use crate::render::{Renderer, Table};

/// The directory `.tm/workflows/*.toml` definitions live under.
fn workflows_dir(project: &Project) -> PathBuf {
    project.state_dir.join("workflows")
}

/// Every `.tm/workflows/*.toml` file's stem (the name `tm workflow show|run` takes), sorted.
/// Empty (not an error) when the directory doesn't exist yet -- a project with no workflows
/// defined is a legitimate, common state, not a misconfiguration.
fn discover_names(project: &Project) -> tm_types::Result<Vec<String>> {
    let dir = workflows_dir(project);
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| {
            TmError::storage(format!(
                "Couldn't read workflows directory {}: {e}",
                dir.display()
            ))
        })?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    Ok(names)
}

/// Read and parse `.tm/workflows/<name>.toml`, returning its raw source alongside the parsed
/// definition -- [`crate::workflow::workflow_run`] needs the raw source to hash and register via
/// [`tm_workflow::commit`], which pins every expanded ticket to it.
fn load(project: &Project, name: &str) -> tm_types::Result<(String, tm_workflow::WorkflowDef)> {
    let path = workflows_dir(project).join(format!("{name}.toml"));
    let source = fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            TmError::storage(format!(
                "Workflow '{name}' not found. Define it in .tm/workflows/{name}.toml"
            ))
        } else {
            TmError::storage(format!(
                "Couldn't read workflow {name:?} at {}: {e}",
                path.display()
            ))
        }
    })?;
    let def = tm_workflow::WorkflowDef::parse(&source)?;
    Ok((source, def))
}

/// Parse `k=v` CLI parameter arguments into a map, per [`crate::args::WorkflowRunArgs::param`]'s
/// documented `KEY=VALUE` shape.
fn parse_params(entries: &[String]) -> tm_types::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for entry in entries {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(TmError::parse(format!(
                "Invalid --param {entry:?}: expected KEY=VALUE, e.g. --param target=src/lib.rs"
            )));
        };
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

/// Dispatch one [`WorkflowCommand`].
pub fn dispatch_workflow(
    cmd: &WorkflowCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        WorkflowCommand::List => workflow_list(project, renderer),
        WorkflowCommand::Show(args) => workflow_show(args, project, renderer),
        WorkflowCommand::Run(args) => workflow_run(args, project, renderer),
    }
}

/// One `tm workflow list` row: either a successfully parsed definition's summary, or a
/// discovered `.toml` that failed to parse. A single malformed definition does not hide every
/// other, valid one -- it is surfaced as its own row instead (the same "don't abort discovery
/// over one bad file" policy [`one_by_one_workflow_names`] follows for `tm doctor`).
enum ListedWorkflow {
    Ok {
        name: String,
        nodes: usize,
        params: usize,
        one_by_one: bool,
    },
    Unparseable {
        name: String,
        error: String,
    },
}

/// `tm workflow list`
pub fn workflow_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let mut rows = Vec::new();
    for name in discover_names(project)? {
        rows.push(match load(project, &name) {
            Ok((_source, def)) => ListedWorkflow::Ok {
                name,
                nodes: def.nodes.len(),
                params: def.params.len(),
                one_by_one: def.is_one_by_one(),
            },
            Err(e) => ListedWorkflow::Unparseable {
                name,
                error: e.to_string(),
            },
        });
    }

    if renderer.is_json() {
        let out: Vec<_> = rows
            .iter()
            .map(|row| match row {
                ListedWorkflow::Ok {
                    name,
                    nodes,
                    params,
                    one_by_one,
                } => serde_json::json!({
                    "name": name,
                    "nodes": nodes,
                    "params": params,
                    "one_by_one": one_by_one,
                }),
                ListedWorkflow::Unparseable { name, error } => serde_json::json!({
                    "name": name,
                    "error": error,
                }),
            })
            .collect();
        renderer.emit(&out, "")?;
    } else {
        if rows.is_empty() {
            renderer.emit(&(), &workflow_empty_state())?;
            return Ok(());
        }
        let table = Table::new(
            vec![
                "NAME".to_string(),
                "NODES".to_string(),
                "PARAMS".to_string(),
                "Single-ticket".to_string(),
            ],
            rows.iter()
                .map(|row| match row {
                    ListedWorkflow::Ok {
                        name,
                        nodes,
                        params,
                        one_by_one,
                    } => vec![
                        name.clone(),
                        nodes.to_string(),
                        params.to_string(),
                        if *one_by_one { "yes" } else { "" }.to_string(),
                    ],
                    ListedWorkflow::Unparseable { name, error } => {
                        vec![
                            name.clone(),
                            "ERROR".to_string(),
                            error.clone(),
                            String::new(),
                        ]
                    }
                })
                .collect(),
        );
        renderer.emit(&(), &table.render_colored(renderer.color_enabled()))?;
    }
    Ok(())
}

fn workflow_empty_state() -> String {
    let starters = [
        "harness-benchmark",
        "migrate-sites",
        "research-and-synthesize",
        "review-change",
    ];
    format!(
        "No workflows yet. Starters: {}",
        starters
            .iter()
            .map(|starter| format!("`tm workflow new --from {starter}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The version-drift relationship between the on-disk `.toml` currently being shown and whatever
/// this project has previously registered (via `tm workflow run`) under the same name.
enum RegistrationDrift {
    /// Nothing under this name has ever been registered -- `tm workflow run` has never run it.
    NeverRun,
    /// The on-disk content hash matches the latest registered version: nothing has drifted.
    UpToDate { version: u32 },
    /// The on-disk content hash matches a previously registered, non-latest version: something
    /// else registered a newer version since (or `run` was invoked against an older on-disk copy
    /// after this one).
    Superseded { version: u32, latest: u32 },
    /// The on-disk content hash matches no registered version at all: `.toml` has changed since
    /// the last `tm workflow run` -- exactly the drift `SPEC.md` §25.2's version pin exists to
    /// make visible rather than silently reinterpreted.
    Changed { latest: u32 },
}

fn registration_drift(
    project: &Project,
    name: &str,
    source: &str,
) -> tm_types::Result<RegistrationDrift> {
    let hash = tm_workflow::content_hash(source);
    let registered = project.store.workflow_defs(Some(name))?; // newest first, per Store::workflow_defs
    let Some(latest) = registered.first() else {
        return Ok(RegistrationDrift::NeverRun);
    };
    Ok(
        match registered.iter().find(|row| row.content_hash == hash) {
            Some(row) if row.version == latest.version => RegistrationDrift::UpToDate {
                version: row.version,
            },
            Some(row) => RegistrationDrift::Superseded {
                version: row.version,
                latest: latest.version,
            },
            None => RegistrationDrift::Changed {
                latest: latest.version,
            },
        },
    )
}

/// `tm workflow show <name>`
pub fn workflow_show(
    args: &WorkflowShowArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let (source, def) = load(project, &args.name)?;
    let drift = registration_drift(project, &args.name, &source)?;
    if renderer.is_json() {
        let drift_json = match &drift {
            RegistrationDrift::NeverRun => serde_json::json!({"status": "never_run"}),
            RegistrationDrift::UpToDate { version } => {
                serde_json::json!({"status": "up_to_date", "version": version})
            }
            RegistrationDrift::Superseded { version, latest } => {
                serde_json::json!({"status": "superseded", "version": version, "latest": latest})
            }
            RegistrationDrift::Changed { latest } => {
                serde_json::json!({"status": "changed_on_disk", "latest_registered": latest})
            }
        };
        renderer.emit(
            &serde_json::json!({ "definition": &def, "registration": drift_json }),
            "",
        )?;
    } else {
        let pretty = toml::to_string_pretty(&def)
            .map_err(|e| TmError::storage(format!("Couldn't format workflow: {e}")))?;
        let mut human = pretty;
        if def.is_one_by_one() {
            human.push_str(
                "\n# note: this workflow is one node wide and one node deep -- tm doctor flags \
                 this as a possible \"prompt wearing a costume\".\n",
            );
        }
        match drift {
            RegistrationDrift::NeverRun => {
                human.push_str("\n# note: this workflow has never been run in this project.\n")
            }
            RegistrationDrift::UpToDate { version } => human.push_str(&format!(
                "\n# on-disk matches the latest registered version ({version}).\n"
            )),
            RegistrationDrift::Superseded { version, latest } => human.push_str(&format!(
                "\n# note: on-disk matches registered version {version}, but version {latest} is \
                 the latest registered under this name.\n"
            )),
            RegistrationDrift::Changed { latest } => human.push_str(&format!(
                "\n# note: on-disk content does not match any registered version (latest \
                 registered: {latest}) -- `.toml` changed since the last `tm workflow run`; a \
                 currently-running instance stays pinned to the version it actually expanded \
                 from, not to this file.\n"
            )),
        }
        renderer.emit(&(), &human)?;
    }
    Ok(())
}

/// `tm workflow run <name> --param k=v ...`
///
/// # IMPL
/// Load and parse the definition, resolve `--param` values, `tm_workflow::expand` against the
/// current `Store::view()`, validate the proposal with `tm_genesis::compile::validate_graph`
/// (reusing genesis's own validator rather than re-checking invariants here), and -- only if
/// validation is clean -- `tm_workflow::commit` it as one `Store::transaction`. `project.ids` is
/// the same `CounterIds`-backed `IdSource` every other id-minting collaborator in this process
/// uses (see `tm_workflow::commit`'s own doc comment on why that, not a `Store` accessor, is the
/// sanctioned way to mint ids outside a `Store` command).
pub fn workflow_run(
    args: &WorkflowRunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let (source, def) = load(project, &args.name)?;
    let params = parse_params(&args.param)?;

    let view = project.store.view()?;
    let proposal = tm_workflow::expand(&def, &params, &view)?;

    let violations = tm_genesis::compile::validate_graph(&proposal, &view);
    if !violations.is_empty() {
        let detail = violations
            .iter()
            .map(|v| format!("{} ({}): {}", v.invariant, v.subject, v.detail))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(TmError::invariant(format!(
            "Workflow {:?} can't run: {detail}",
            def.name
        )));
    }

    let outcome = tm_workflow::commit(
        &project.store,
        project.ids.as_ref(),
        project.actor.clone(),
        &def,
        &source,
        &proposal,
    )?;

    if renderer.is_json() {
        let tickets: BTreeMap<_, _> = outcome
            .tickets
            .iter()
            .map(|(r, id)| (r.clone(), id.as_str().to_string()))
            .collect();
        renderer.emit(
            &serde_json::json!({
                "name": def.name,
                "version": outcome.version,
                "content_hash": outcome.content_hash,
                "tickets": tickets,
            }),
            "",
        )?;
    } else {
        renderer.note(&format!(
            "Ran workflow {:?} (version {}, {}): {} ticket(s) created.",
            def.name,
            outcome.version,
            &outcome.content_hash[..12.min(outcome.content_hash.len())],
            outcome.tickets.len()
        ));
        let mut refs: Vec<_> = outcome.tickets.iter().collect();
        refs.sort_by(|a, b| a.0.cmp(b.0));
        for (ticket_ref, id) in refs {
            renderer.note(&format!("  {ticket_ref} -> {id}"));
        }
    }
    Ok(())
}

/// Every discovered workflow definition that is a "1x1" smell (`SPEC.md` §25.3): one node wide,
/// one node deep, no fan-out. Used by `tm doctor`'s advisory workflow check. A parse failure for
/// one definition is folded into the returned list as a synthetic name so `tm doctor` surfaces it
/// rather than silently skipping a broken `.toml`; it does not abort discovery of the rest.
pub fn one_by_one_workflow_names(project: &Project) -> Vec<String> {
    let names = match discover_names(project) {
        Ok(names) => names,
        Err(_) => return vec![],
    };
    let mut flagged = Vec::new();
    for name in names {
        match load(project, &name) {
            Ok((_source, def)) => {
                if def.is_one_by_one() {
                    flagged.push(name);
                }
            }
            Err(e) => flagged.push(format!("{name} (unparseable: {e})")),
        }
    }
    flagged
}

/// True when `.tm/workflows/` exists and has at least one `.toml` file in it, so `tm doctor` can
/// distinguish "no workflows defined yet" (nothing to warn about) from "workflows defined, none
/// of them 1x1" (also nothing to warn about, but worth a different detail message).
pub fn has_any_workflow(project: &Project) -> bool {
    workflows_dir(project).is_dir() && !discover_names(project).unwrap_or_default().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_params_splits_on_first_equals() {
        let out = parse_params(&["target=src/lib.rs=x".to_string()]).expect("parses");
        assert_eq!(out.get("target").unwrap(), "src/lib.rs=x");
    }

    #[test]
    fn parse_params_rejects_a_bare_key() {
        let err = parse_params(&["no-equals-sign".to_string()]).unwrap_err();
        assert!(err.to_string().contains("expected KEY=VALUE"));
    }

    #[test]
    fn parse_params_empty_is_empty() {
        assert!(parse_params(&[]).expect("parses").is_empty());
    }

    #[test]
    fn workflow_empty_state_snapshot() {
        assert_eq!(
            workflow_empty_state(),
            "No workflows yet. Starters: `tm workflow new --from harness-benchmark`, `tm workflow new --from migrate-sites`, `tm workflow new --from research-and-synthesize`, `tm workflow new --from review-change`"
        );
    }

    #[test]
    fn load_missing_workflow_gives_friendly_error() {
        use std::sync::Arc;
        use tm_core::Store;
        use tm_types::{Clock, CounterIds, FixedClock, IdSource};

        let tmpdir = tempfile::tempdir().expect("tempdir created");
        let root = tmpdir.path();

        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store =
            Arc::new(Store::open_with(root, clock.clone(), ids.clone()).expect("open store"));
        let project = Project::for_test(root, store, clock, ids);

        let err = load(&project, "nonexistent").unwrap_err();
        let err_msg = err.to_string();

        // Error message should be friendly and not include filesystem path
        assert!(err_msg.contains("Workflow 'nonexistent' not found"));
        assert!(err_msg.contains(".tm/workflows/nonexistent.toml"));
        // Should NOT contain filesystem path or os error details
        assert!(!err_msg.contains("at "));
        assert!(!err_msg.contains("os error"));
        assert!(!err_msg.contains("No such file"));
    }
}
