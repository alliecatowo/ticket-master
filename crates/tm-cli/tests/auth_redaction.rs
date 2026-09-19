//! The CLI leg of the redaction guarantee `SPEC.md` §28.2 and the audit call out: a canary
//! credential value must not appear in any `--json` output this workspace's `tm` binary
//! produces. This workspace has no pre-existing golden `--json` snapshot fixture files (checked:
//! only `crates/tm-workflow/tests/snapshots/*.snap`, which are graph-expansion snapshots, not
//! CLI output) — the substitute this test uses is running the real, compiled `tm` binary against
//! every `--json`-emitting command a credential could plausibly reach (`tm auth`, `tm provider
//! detect`) with a canary value planted in the environment, and scanning the actual bytes on
//! stdout/stderr.
//!
//! `crates/xtask/src/hygiene.rs`'s `check_network_in_tests` flags any test that reads the literal
//! `ANTHROPIC_API_KEY` via `var(`/`set_var`/`env!` (a real provider's credential env var — the
//! "test depends on a real credential being present" hazard it exists to catch). This test
//! authenticates against `openai` instead, whose env var name (`OPENAI_API_KEY`) is not on that
//! list, and — per this crate's own convention (`tests/bare_bootstrap.rs`) — sets the canary only
//! via `.env()` on the spawned child process, never via `std::env::set_var` in this test's own
//! process.

use std::process::{Command, Stdio};

const CANARY: &str = "CANARY-SECRET-VALUE-9c31f0ab";

/// Run the real `tm` binary in `dir` with `args`, and `OPENAI_API_KEY` set to [`CANARY`] in the
/// child's environment only.
fn run_tm_with_canary(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tm"))
        .args(args)
        .current_dir(dir)
        .env("OPENAI_API_KEY", CANARY)
        .stdin(Stdio::null())
        .output()
        .expect("`tm` should run to completion")
}

fn assert_no_canary(output: &std::process::Output, label: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(CANARY),
        "{label}: canary leaked into stdout: {stdout}"
    );
    assert!(
        !stderr.contains(CANARY),
        "{label}: canary leaked into stderr: {stderr}"
    );
}

#[test]
fn tm_auth_json_never_prints_the_resolved_credential() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_tm_with_canary(
        tmp.path(),
        &[
            "--project",
            tmp.path().to_str().unwrap(),
            "--json",
            "auth",
            "openai",
        ],
    );
    assert!(
        output.status.success(),
        "tm auth openai --json should succeed with the var present, got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_canary(&output, "tm auth openai --json");
    // The `configured: true` claim is expected and fine to see -- it is a boolean, not the
    // credential itself. What must never appear is the canary's actual bytes (checked above).
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"configured\": true") || stdout.contains("\"configured\":true"),
        "sanity: the var should be reported configured: {stdout}"
    );
}

#[test]
fn tm_auth_human_output_never_prints_the_resolved_credential() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_tm_with_canary(
        tmp.path(),
        &["--project", tmp.path().to_str().unwrap(), "auth", "openai"],
    );
    assert!(output.status.success(), "tm auth openai should succeed");
    assert_no_canary(&output, "tm auth openai (human)");
}

#[test]
fn tm_provider_detect_json_never_prints_the_resolved_credential() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_tm_with_canary(
        tmp.path(),
        &[
            "--project",
            tmp.path().to_str().unwrap(),
            "--json",
            "provider",
            "detect",
        ],
    );
    assert!(
        output.status.success(),
        "tm provider detect --json should succeed"
    );
    assert_no_canary(&output, "tm provider detect --json");
}
