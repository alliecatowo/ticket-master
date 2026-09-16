//! Workspace task runner: `cargo run -p xtask -- <verify|hygiene|fmt>`.
//!
//! This is the `cargo xtask verify` gate described in SPEC.md section 16: a
//! single command that runs the whole formatting/lint/test/hygiene pipeline
//! so CI and local dev agree on one source of truth.

mod hygiene;

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};

fn main() -> ExitCode {
    let subcommand = env::args().nth(1).unwrap_or_default();
    let result = match subcommand.as_str() {
        "verify" => verify(),
        "hygiene" => run_hygiene(),
        "fmt" => run_fmt(),
        other => {
            eprintln!("usage: xtask <verify|hygiene|fmt>");
            if !other.is_empty() {
                eprintln!("unknown subcommand: {other}");
            }
            Err(anyhow::anyhow!("no such subcommand"))
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:?}");
            ExitCode::FAILURE
        }
    }
}

/// Walks up from `CARGO_MANIFEST_DIR` to find the directory containing the
/// workspace's root `Cargo.toml` (the one with a `[workspace]` table).
fn workspace_root() -> Result<PathBuf> {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR is not set")?;
    let mut dir = PathBuf::from(manifest_dir);
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.exists() {
            let contents = std::fs::read_to_string(&candidate)?;
            if contents.contains("[workspace]") {
                return Ok(dir);
            }
        }
        if !dir.pop() {
            bail!("could not locate workspace root above CARGO_MANIFEST_DIR");
        }
    }
}

/// Runs a cargo subcommand from the workspace root, streaming its output,
/// and errors out if it exits non-zero.
fn run_cargo(root: &Path, step: &str, args: &[&str]) -> Result<()> {
    println!("==> {step}: cargo {}", args.join(" "));
    let status = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("failed to spawn `cargo {}`", args.join(" ")))?;
    if !status.success() {
        bail!("{step} failed (cargo {})", args.join(" "));
    }
    Ok(())
}

fn verify() -> Result<()> {
    let root = workspace_root()?;
    run_cargo(&root, "fmt check", &["fmt", "--check"])?;
    run_cargo(
        &root,
        "clippy",
        &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
    )?;
    run_cargo(&root, "test", &["test", "--workspace"])?;
    hygiene_gate(&root)?;
    println!("==> verify: all checks passed");
    Ok(())
}

fn run_hygiene() -> Result<()> {
    let root = workspace_root()?;
    hygiene_gate(&root)
}

fn hygiene_gate(root: &Path) -> Result<()> {
    println!("==> hygiene: scanning workspace");
    let violations = hygiene::run_all(root);
    if violations.is_empty() {
        println!("==> hygiene: clean");
        return Ok(());
    }
    for v in &violations {
        println!("{v}");
    }
    bail!("hygiene: {} violation(s) found", violations.len());
}

fn run_fmt() -> Result<()> {
    let root = workspace_root()?;
    run_cargo(&root, "fmt", &["fmt", "--all"])
}
