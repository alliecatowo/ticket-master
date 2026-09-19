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
    project.root.join(".tm").join("workflows")
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
        .map_err(|e| TmError::storage(format!("Failed to read {}: {e}", dir.display())))?
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
        TmError::storage(format!(
            "Failed to read workflow {name:?} at {}: {e}",
            path.display()
        ))
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
                "Invalid --param {entry:?}: expected KEY=VALUE"
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

/// `tm workflow list`
pub fn workflow_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let mut rows = Vec::new();
    for name in discover_names(project)? {
        let (_source, def) = load(project, &name)?;
        rows.push((name, def.nodes.len(), def.params.len(), def.is_one_by_one()));
    }

    if renderer.is_json() {
        let out: Vec<_> = rows
            .iter()
            .map(|(name, nodes, params, one_by_one)| {
                serde_json::json!({
                    "name": name,
                    "nodes": nodes,
                    "params": params,
                    "one_by_one": one_by_one,
                })
            })
            .collect();
        renderer.emit(&out, "")?;
    } else {
        let table = Table::new(
            vec![
                "NAME".to_string(),
                "NODES".to_string(),
                "PARAMS".to_string(),
                "1X1?".to_string(),
            ],
            rows.iter()
                .map(|(name, nodes, params, one_by_one)| {
                    vec![
                        name.clone(),
                        nodes.to_string(),
                        params.to_string(),
                        if *one_by_one { "yes" } else { "" }.to_string(),
                    ]
                })
                .collect(),
        );
        renderer.emit(&(), &table.render())?;
    }
    Ok(())
}

/// `tm workflow show <name>`
pub fn workflow_show(
    args: &WorkflowShowArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let (_source, def) = load(project, &args.name)?;
    if renderer.is_json() {
        renderer.emit(&def, "")?;
    } else {
        let pretty = toml::to_string_pretty(&def)
            .map_err(|e| TmError::storage(format!("Failed to format workflow: {e}")))?;
        let mut human = pretty;
        if def.is_one_by_one() {
            human.push_str(
                "\n# note: this workflow is one node wide and one node deep -- tm doctor flags \
                 this as a possible \"prompt wearing a costume\" (SPEC.md §25.3).\n",
            );
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
            "workflow {:?} expansion violates tm-core invariants: {detail}",
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
            Ok((_source, def)) if def.is_one_by_one() => flagged.push(name),
            _ => {}
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
}
