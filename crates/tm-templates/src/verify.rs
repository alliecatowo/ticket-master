//! `verify.toml`: the commands that prove a scaffolded template actually works, run through
//! [`tm_context::command::run`] — the same gated path `tm-cli`'s agent turn-running already uses
//! for every shell command a worker issues. A third-party template's `verify.toml` is untrusted
//! input (`SPEC.md` §27.4): it runs under whatever [`Authority`] the caller passes in, exactly
//! like any other command, never with implicit full authority just because it came from a
//! template.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tm_context::command::{
    self, ArtifactStream, CommandCache, CommandExecutor, CommandResult, CommandSpec,
    ExecutionOutcome, Query,
};
use tm_events::EventDraft;
use tm_types::{
    ArtifactId, Authority, Clock, IdKind, IdSource, ParticipantId, Result, Timestamp, TmError,
};

/// One command `verify.toml` says proves the scaffold works.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyCheck {
    /// Short label, e.g. `"build"`, shown in [`VerifyOutcome`] and any failure message.
    pub name: String,
    /// argv, never shell-interpolated (mirrors [`CommandSpec::argv`]).
    pub argv: Vec<String>,
    /// Environment variable names this check's process may see (mirrors
    /// [`CommandSpec::env_allowlist`]) — e.g. `PATH`, `CARGO_HOME`, so `cargo` can find the
    /// toolchain and the local registry cache without inheriting the caller's whole environment.
    #[serde(default)]
    pub env_allowlist: Vec<String>,
}

/// The parsed shape of `verify.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VerifySpec {
    /// Every check, run in file order; the first non-zero exit fails the whole verification.
    #[serde(rename = "check", default)]
    pub checks: Vec<VerifyCheck>,
}

/// Parse `path` (typically [`crate::template::Template::verify_path`]) as a [`VerifySpec`]. A
/// missing file is not an error: it parses as zero checks, and [`run`] on zero checks is a
/// no-op — a caller that wants to *require* verification checks for that itself.
///
/// # Errors
/// [`TmError::Io`] if `path` exists but cannot be read; [`TmError::Parse`] if it is malformed.
pub fn load(path: &Path) -> Result<VerifySpec> {
    if !path.is_file() {
        return Ok(VerifySpec::default());
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|e| TmError::Io(format!("reading {}: {e}", path.display())))?;
    toml::from_str(&contents).map_err(|e| TmError::parse(format!("{}: {e}", path.display())))
}

/// One check's outcome: which check, and the [`CommandResult`] [`tm_context::command::run`]
/// returned for it.
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    /// The check's name.
    pub name: String,
    /// Its result.
    pub result: CommandResult,
}

/// Every check's outcome plus the `command.started`/`command.completed` event drafts
/// [`tm_context::command::run`] produced along the way — this crate holds no event log of its
/// own (same division of responsibility as `tm_context::command::run` itself), so a caller that
/// owns one can append these to it instead of them being silently dropped.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Outcomes, in `spec.checks` order.
    pub outcomes: Vec<VerifyOutcome>,
    /// Event drafts from every check that ran (a failed check's events are included too — a
    /// verification failure is itself worth a durable record).
    pub events: Vec<EventDraft>,
}

/// Run every check in `spec` against `dest` (the scaffolded project's root), in order, stopping
/// at the first failure. Each check goes through [`tm_context::command::run`], gated by `auth`
/// exactly like any other command a worker runs (`SPEC.md` §8.2, §27.4).
///
/// # Errors
/// [`TmError::AuthorityDenied`] if `auth` does not permit a check's argv (surfaced by
/// `tm_context::command::run` itself); otherwise [`TmError::Invariant`] naming the failing check,
/// its exit code, and a stderr tail, on the first check that exits non-zero.
#[allow(clippy::too_many_arguments)]
pub fn run(
    spec: &VerifySpec,
    dest: &Path,
    auth: &Authority,
    cache: &dyn CommandCache,
    executor: &dyn CommandExecutor,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    actor: &ParticipantId,
) -> Result<VerifyReport> {
    let mut outcomes = Vec::with_capacity(spec.checks.len());
    let mut events = Vec::new();
    for check in &spec.checks {
        let cmd = CommandSpec {
            argv: check.argv.clone(),
            cwd: dest.to_string_lossy().into_owned(),
            env_allowlist: check.env_allowlist.clone(),
            declared_inputs: Vec::new(),
            // A build/test run is not a pure function of declared inputs alone (toolchain
            // version, registry cache state) and each check should genuinely re-run rather than
            // silently reuse a stale result from a different destination that happened to hash
            // the same key.
            cacheable: false,
            ticket: None,
            session: None,
        };
        let key = format!("template-verify:{}:{}", check.name, ids.random_hex(16));
        let (result, drafts) = command::run(&cmd, &key, cache, auth, executor, clock, actor)?;
        events.extend(drafts);
        if result.exit_code != 0 {
            let stderr_tail = result
                .query(ArtifactStream::Stderr, cache, Query::Tail(40))
                .ok()
                .map(|answer| format!("{answer:?}"))
                .unwrap_or_default();
            return Err(TmError::invariant(format!(
                "verify check {:?} ({}) exited {}: {stderr_tail}",
                check.name,
                check.argv.join(" "),
                result.exit_code
            )));
        }
        outcomes.push(VerifyOutcome {
            name: check.name.clone(),
            result,
        });
    }
    Ok(VerifyReport { outcomes, events })
}

/// An in-process, non-durable [`CommandCache`]: enough to satisfy
/// [`tm_context::command::run`]'s API (it always stores a result, even for a non-cacheable
/// command) without needing `tm-core`'s artifact tables — template verification is a one-shot
/// check, not a session other commands come back to query later. Mirrors `tm-cli`'s
/// `MemoryCommandCache`.
#[derive(Default)]
pub struct MemoryCommandCache {
    results: Mutex<BTreeMap<String, CommandResult>>,
    artifacts: Mutex<BTreeMap<ArtifactId, Vec<u8>>>,
    next: Mutex<u64>,
}

impl MemoryCommandCache {
    /// A fresh, empty cache.
    pub fn new() -> Self {
        MemoryCommandCache::default()
    }

    fn mint_artifact(&self) -> Result<ArtifactId> {
        let mut next = self.next.lock().expect("command cache mutex poisoned");
        *next += 1;
        ArtifactId::new(format!("{}{:012x}", IdKind::Artifact.prefix(), *next))
    }
}

impl CommandCache for MemoryCommandCache {
    fn get(&self, key: &str) -> Result<Option<CommandResult>> {
        Ok(self
            .results
            .lock()
            .expect("command cache mutex poisoned")
            .get(key)
            .cloned())
    }

    fn put(
        &self,
        key: &str,
        argv: &[String],
        exit_code: i32,
        started: Timestamp,
        completed: Timestamp,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<CommandResult> {
        let stdout_artifact = self.mint_artifact()?;
        let stderr_artifact = self.mint_artifact()?;
        {
            let mut artifacts = self.artifacts.lock().expect("command cache mutex poisoned");
            artifacts.insert(stdout_artifact.clone(), stdout.to_vec());
            artifacts.insert(stderr_artifact.clone(), stderr.to_vec());
        }
        let result = CommandResult {
            key: key.to_string(),
            argv: argv.to_vec(),
            exit_code,
            duration_ms: completed.millis_since(started).max(0) as u64,
            stdout_artifact,
            stderr_artifact,
            started,
            completed,
            from_cache: false,
        };
        self.results
            .lock()
            .expect("command cache mutex poisoned")
            .insert(key.to_string(), result.clone());
        Ok(result)
    }

    fn read_artifact(&self, id: &ArtifactId) -> Result<Vec<u8>> {
        self.artifacts
            .lock()
            .expect("command cache mutex poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| TmError::not_found("artifact", id.as_str()))
    }
}

/// Runs a [`CommandSpec`]'s process via `std::process::Command`, honoring its working directory
/// and environment allowlist (no other ambient environment leaks into the child). Mirrors
/// `tm-cli`'s `ProcessCommandExecutor`.
#[derive(Debug, Default)]
pub struct ProcessCommandExecutor;

impl CommandExecutor for ProcessCommandExecutor {
    fn execute(&self, spec: &CommandSpec) -> Result<ExecutionOutcome> {
        let program = spec
            .argv
            .first()
            .ok_or_else(|| TmError::Invariant("command spec has an empty argv".to_string()))?;
        let mut cmd = StdCommand::new(program);
        cmd.args(&spec.argv[1..]);
        cmd.current_dir(&spec.cwd);
        cmd.env_clear();
        for key in &spec.env_allowlist {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        let output = cmd.output().map_err(TmError::from)?;
        Ok(ExecutionOutcome {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{CounterIds, FixedClock, PatternSet};

    fn shell_authority() -> Authority {
        Authority {
            shell: tm_types::ShellAuthority {
                enabled: true,
                allow: PatternSet::parse(["true*", "false*", "echo*"]).expect("valid patterns"),
                deny: PatternSet::empty(),
                pty: false,
            },
            ..Authority::none()
        }
    }

    #[test]
    fn load_missing_verify_toml_is_zero_checks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = load(&dir.path().join("verify.toml")).expect("loads");
        assert!(spec.checks.is_empty());
    }

    #[test]
    fn run_executes_every_check_and_reports_success() {
        let spec = VerifySpec {
            checks: vec![VerifyCheck {
                name: "ok".to_string(),
                argv: vec!["true".to_string()],
                env_allowlist: vec!["PATH".to_string()],
            }],
        };
        let dest = tempfile::tempdir().expect("tempdir");
        let cache = MemoryCommandCache::new();
        let executor = ProcessCommandExecutor;
        let clock = FixedClock::epoch();
        let ids = CounterIds::new();
        let actor = ParticipantId::system();

        let report = run(
            &spec,
            dest.path(),
            &shell_authority(),
            &cache,
            &executor,
            &clock,
            &ids,
            &actor,
        )
        .expect("verifies");
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].result.exit_code, 0);
        assert_eq!(
            report.events.len(),
            2,
            "command.started + command.completed"
        );
    }

    #[test]
    fn run_stops_and_errors_on_the_first_failing_check() {
        let spec = VerifySpec {
            checks: vec![
                VerifyCheck {
                    name: "fails".to_string(),
                    argv: vec!["false".to_string()],
                    env_allowlist: vec!["PATH".to_string()],
                },
                VerifyCheck {
                    name: "never-runs".to_string(),
                    argv: vec!["true".to_string()],
                    env_allowlist: vec!["PATH".to_string()],
                },
            ],
        };
        let dest = tempfile::tempdir().expect("tempdir");
        let cache = MemoryCommandCache::new();
        let executor = ProcessCommandExecutor;
        let clock = FixedClock::epoch();
        let ids = CounterIds::new();
        let actor = ParticipantId::system();

        let err = run(
            &spec,
            dest.path(),
            &shell_authority(),
            &cache,
            &executor,
            &clock,
            &ids,
            &actor,
        )
        .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn run_denies_a_check_not_covered_by_authority() {
        let spec = VerifySpec {
            checks: vec![VerifyCheck {
                name: "curl".to_string(),
                argv: vec!["curl".to_string(), "evil".to_string()],
                env_allowlist: vec![],
            }],
        };
        let dest = tempfile::tempdir().expect("tempdir");
        let cache = MemoryCommandCache::new();
        let executor = ProcessCommandExecutor;
        let clock = FixedClock::epoch();
        let ids = CounterIds::new();
        let actor = ParticipantId::system();

        let err = run(
            &spec,
            dest.path(),
            &shell_authority(),
            &cache,
            &executor,
            &clock,
            &ids,
            &actor,
        )
        .unwrap_err();
        assert!(matches!(err, TmError::AuthorityDenied(_)));
    }
}
