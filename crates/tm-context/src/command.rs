//! Command artifacts: an expensive command runs at most once per cache key (`SPEC.md` §8.2).
//!
//! [`CommandSpec`] describes a call; [`run`] gates it through `Authority`, consults a
//! [`CommandCache`] keyed by [`crate::fingerprint::cache_key`], and — on a miss — executes it
//! via an injected [`CommandExecutor`], stores stdout/stderr in full, and returns a
//! [`CommandResult`] plus the `command.started`/`command.completed` events to append (this
//! crate never writes to the event log itself; the caller owns the `tm_events::log::Tx`).
//! [`CommandResult::query`] answers head/tail/grep/range/JSON-pointer questions about the
//! stored output without ever re-running the command.

use tm_events::payload::{CommandCompletedPayload, CommandStartedPayload};
use tm_events::EventDraft;
use tm_types::{
    Action, ArtifactId, Authority, Clock, Id, ParticipantId, Result, SessionId, TicketId,
    Timestamp, TmError,
};

/// A single command invocation, as a cache-relevant, authority-checkable unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The command and its arguments, argv-style (never shell-interpolated).
    pub argv: Vec<String>,
    /// Working directory the command runs in.
    pub cwd: String,
    /// Names of environment variables relevant to this command's output (see
    /// [`crate::fingerprint::EnvAllowlist`]).
    pub env_allowlist: Vec<String>,
    /// Paths (beyond the repository fingerprint) this command's output depends on.
    pub declared_inputs: Vec<String>,
    /// When `false`, [`run`] always executes and never stores/looks up a cached result —
    /// for commands whose output is not a pure function of their declared inputs (e.g. ones
    /// that hit the network, read the wall clock, or use randomness).
    pub cacheable: bool,
    /// The ticket this command is being run on behalf of, if any (for authority checks and
    /// event subjects).
    pub ticket: Option<TicketId>,
    /// The session this command is being run inside, if any (for event attribution).
    pub session: Option<SessionId>,
}

/// The result of executing (or looking up) a command: metadata plus pointers to its stored
/// output, never the raw bytes inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// The cache key this result is stored/looked up under.
    pub key: String,
    /// The command that was run.
    pub argv: Vec<String>,
    /// Process exit code.
    pub exit_code: i32,
    /// Wall-clock duration of the execution, in milliseconds (0 for a cache hit).
    pub duration_ms: u64,
    /// Artifact holding the full captured stdout.
    pub stdout_artifact: ArtifactId,
    /// Artifact holding the full captured stderr.
    pub stderr_artifact: ArtifactId,
    /// When execution started (or, for a cache hit, when the original execution started).
    pub started: Timestamp,
    /// When execution completed (or, for a cache hit, when the original execution completed).
    pub completed: Timestamp,
    /// `true` if this result came from [`CommandCache::get`] rather than a fresh execution.
    pub from_cache: bool,
}

/// Which of a command's two output streams a [`Query`] targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// A follow-up question about a stored command artifact, answered without re-running the
/// command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// The first `n` lines.
    Head(usize),
    /// The last `n` lines.
    Tail(usize),
    /// Every line matching a regex pattern.
    Grep(String),
    /// Lines `start..=end`, 1-based inclusive.
    Range(usize, usize),
    /// A JSON Pointer (RFC 6901) into the artifact, parsed as JSON.
    Json(String),
}

/// The answer to a [`Query`] against a stored artifact.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryAnswer {
    /// Matching/selected lines, in original order.
    Lines(Vec<String>),
    /// The JSON value at a [`Query::Json`] pointer.
    Json(serde_json::Value),
    /// The query found nothing (empty grep match set, out-of-range line range, or a JSON
    /// pointer that resolves to nothing).
    NotFound,
}

/// Storage and lookup for command artifacts, keyed by [`crate::fingerprint::cache_key`].
/// Implemented against `tm-core`'s artifact tables by the binary that wires this crate up; a
/// fake in-memory implementation stands in for it in unit tests.
pub trait CommandCache {
    /// Look up a previously stored result by cache key. `Ok(None)` on a clean miss.
    fn get(&self, key: &str) -> Result<Option<CommandResult>>;

    /// Store a freshly executed command's full output as two artifacts and record the
    /// resulting [`CommandResult`] under `key`. Returns the stored result (with freshly
    /// minted artifact ids).
    #[allow(clippy::too_many_arguments)]
    fn put(
        &self,
        key: &str,
        argv: &[String],
        exit_code: i32,
        started: Timestamp,
        completed: Timestamp,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<CommandResult>;

    /// Read back a stored artifact's full bytes, for [`CommandResult::query`].
    fn read_artifact(&self, id: &ArtifactId) -> Result<Vec<u8>>;
}

/// Runs a [`CommandSpec`]'s process. Implemented against `std::process::Command` by the binary
/// that wires this crate up; a fake implementation stands in for it in unit tests so [`run`]'s
/// cache/authority/event logic is testable without spawning real processes.
pub trait CommandExecutor {
    /// Execute `spec` to completion and capture its outcome.
    fn execute(&self, spec: &CommandSpec) -> Result<ExecutionOutcome>;
}

/// The raw result of actually running a command's process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOutcome {
    /// Process exit code.
    pub exit_code: i32,
    /// Captured stdout, in full.
    pub stdout: Vec<u8>,
    /// Captured stderr, in full.
    pub stderr: Vec<u8>,
}

/// Run `cmd`, gated by `auth`, through `cache`: on a cache hit (when `cmd.cacheable`), return
/// the stored result without executing; otherwise execute via `executor`, store the full
/// output via `cache`, and return the fresh result. Returns the result plus the
/// `command.started`/`command.completed` event drafts to append (this crate does not hold an
/// `EventLog`; the caller commits them alongside its own transaction).
#[allow(clippy::too_many_arguments)]
pub fn run(
    cmd: &CommandSpec,
    key: &str,
    cache: &dyn CommandCache,
    auth: &Authority,
    executor: &dyn CommandExecutor,
    clock: &dyn Clock,
    actor: &ParticipantId,
) -> Result<(CommandResult, Vec<EventDraft>)> {
    let action = Action::RunCommand {
        command: cmd.argv.clone(),
    };
    if !auth.permits(&action).is_allowed() {
        return Err(TmError::AuthorityDenied(action.class()));
    }

    if cmd.cacheable {
        if let Some(result) = cache.get(key)? {
            let hit = CommandResult {
                from_cache: true,
                ..result
            };
            return Ok((hit, Vec::new()));
        }
    }

    let started = clock.now();
    let outcome = executor.execute(cmd)?;
    let completed = clock.now();

    let result = cache.put(
        key,
        &cmd.argv,
        outcome.exit_code,
        started,
        completed,
        &outcome.stdout,
        &outcome.stderr,
    )?;

    let subject = cmd.ticket.clone().map(Id::from).unwrap_or_else(Id::none);
    let command_line = cmd.argv.join(" ");

    let started_draft = EventDraft::new(
        actor.clone(),
        subject.clone(),
        CommandStartedPayload {
            command: command_line.clone(),
            ticket: cmd.ticket.clone(),
            session: cmd.session.clone(),
        }
        .into(),
    );
    let completed_draft = EventDraft::new(
        actor.clone(),
        subject,
        CommandCompletedPayload {
            command: command_line,
            ticket: cmd.ticket.clone(),
            session: cmd.session.clone(),
            exit_code: outcome.exit_code,
            duration_ms: completed.millis_since(started).max(0) as u64,
        }
        .into(),
    );

    Ok((result, vec![started_draft, completed_draft]))
}

impl CommandResult {
    /// Answer `query` against this result's `stream`, reading the backing artifact through
    /// `cache` (so the caller never needs to re-run the command to see more of its output).
    pub fn query(
        &self,
        stream: ArtifactStream,
        cache: &dyn CommandCache,
        query: Query,
    ) -> Result<QueryAnswer> {
        let artifact = match stream {
            ArtifactStream::Stdout => &self.stdout_artifact,
            ArtifactStream::Stderr => &self.stderr_artifact,
        };
        let bytes = cache.read_artifact(artifact)?;
        query_output(&bytes, query)
    }
}

/// Answer `query` against one stream's raw captured bytes — what [`CommandResult::query`] does
/// once it has read the artifact, exposed so a caller holding only an artifact id (e.g. one a
/// previous tool result reported) can query it directly.
pub fn query_output(bytes: &[u8], query: Query) -> Result<QueryAnswer> {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.split('\n').collect();

    match query {
        Query::Head(n) => {
            if n == 0 {
                return Ok(QueryAnswer::NotFound);
            }
            let take = n.min(lines.len());
            if take == 0 {
                return Ok(QueryAnswer::NotFound);
            }
            Ok(QueryAnswer::Lines(
                lines[..take].iter().map(|s| s.to_string()).collect(),
            ))
        }
        Query::Tail(n) => {
            if n == 0 || lines.is_empty() {
                return Ok(QueryAnswer::NotFound);
            }
            let take = n.min(lines.len());
            Ok(QueryAnswer::Lines(
                lines[lines.len() - take..]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ))
        }
        Query::Range(start, end) => {
            if start == 0 || start > end || start > lines.len() {
                return Ok(QueryAnswer::NotFound);
            }
            let end = end.min(lines.len());
            Ok(QueryAnswer::Lines(
                lines[start - 1..end]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ))
        }
        Query::Grep(pattern) => {
            let re = regex::Regex::new(&pattern).map_err(|e| TmError::parse(e.to_string()))?;
            let matches: Vec<String> = lines
                .iter()
                .filter(|line| re.is_match(line))
                .map(|s| s.to_string())
                .collect();
            if matches.is_empty() {
                Ok(QueryAnswer::NotFound)
            } else {
                Ok(QueryAnswer::Lines(matches))
            }
        }
        Query::Json(pointer) => {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| TmError::parse(e.to_string()))?;
            match value.pointer(&pointer) {
                Some(v) => Ok(QueryAnswer::Json(v.clone())),
                None => Ok(QueryAnswer::NotFound),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tm_types::{Authority, FixedClock};

    struct FakeCache {
        store: Mutex<std::collections::HashMap<String, CommandResult>>,
        artifacts: Mutex<std::collections::HashMap<ArtifactId, Vec<u8>>>,
        next: Mutex<u32>,
    }

    impl FakeCache {
        fn new() -> Self {
            FakeCache {
                store: Mutex::new(std::collections::HashMap::new()),
                artifacts: Mutex::new(std::collections::HashMap::new()),
                next: Mutex::new(0),
            }
        }

        fn mint(&self) -> ArtifactId {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            ArtifactId::new(format!("ART-{:012x}", *n)).expect("well-formed test artifact id")
        }
    }

    impl CommandCache for FakeCache {
        fn get(&self, key: &str) -> Result<Option<CommandResult>> {
            Ok(self.store.lock().unwrap().get(key).cloned())
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
            let stdout_artifact = self.mint();
            let stderr_artifact = self.mint();
            self.artifacts
                .lock()
                .unwrap()
                .insert(stdout_artifact.clone(), stdout.to_vec());
            self.artifacts
                .lock()
                .unwrap()
                .insert(stderr_artifact.clone(), stderr.to_vec());
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
            self.store
                .lock()
                .unwrap()
                .insert(key.to_string(), result.clone());
            Ok(result)
        }

        fn read_artifact(&self, id: &ArtifactId) -> Result<Vec<u8>> {
            self.artifacts
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| TmError::not_found("artifact", id.as_str()))
        }
    }

    struct FakeExecutor {
        outcome: ExecutionOutcome,
        calls: Mutex<u32>,
    }

    impl FakeExecutor {
        fn new(outcome: ExecutionOutcome) -> Self {
            FakeExecutor {
                outcome,
                calls: Mutex::new(0),
            }
        }
    }

    impl CommandExecutor for FakeExecutor {
        fn execute(&self, _spec: &CommandSpec) -> Result<ExecutionOutcome> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.outcome.clone())
        }
    }

    fn spec(cacheable: bool) -> CommandSpec {
        CommandSpec {
            argv: vec!["echo".to_string(), "hi".to_string()],
            cwd: "/tmp".to_string(),
            env_allowlist: Vec::new(),
            declared_inputs: Vec::new(),
            cacheable,
            ticket: None,
            session: None,
        }
    }

    fn actor() -> ParticipantId {
        ParticipantId::system()
    }

    #[test]
    fn cache_miss_executes_and_records_start_and_completion_events() {
        let cache = FakeCache::new();
        let executor = FakeExecutor::new(ExecutionOutcome {
            exit_code: 0,
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
        });
        let clock = FixedClock::epoch();
        let (result, events) = run(
            &spec(true),
            "key-1",
            &cache,
            &Authority::root(),
            &executor,
            &clock,
            &actor(),
        )
        .unwrap();

        assert!(!result.from_cache);
        assert_eq!(result.exit_code, 0);
        assert_eq!(events.len(), 2);
        assert_eq!(*executor.calls.lock().unwrap(), 1);
    }

    #[test]
    fn cache_hit_skips_execution_and_emits_no_events() {
        let cache = FakeCache::new();
        let executor = FakeExecutor::new(ExecutionOutcome {
            exit_code: 0,
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
        });
        let clock = FixedClock::epoch();
        let auth = Authority::root();

        let (_, _) = run(
            &spec(true),
            "key-1",
            &cache,
            &auth,
            &executor,
            &clock,
            &actor(),
        )
        .unwrap();
        let (result, events) = run(
            &spec(true),
            "key-1",
            &cache,
            &auth,
            &executor,
            &clock,
            &actor(),
        )
        .unwrap();

        assert!(result.from_cache);
        assert!(events.is_empty());
        assert_eq!(*executor.calls.lock().unwrap(), 1);
    }

    #[test]
    fn non_cacheable_command_always_executes() {
        let cache = FakeCache::new();
        let executor = FakeExecutor::new(ExecutionOutcome {
            exit_code: 0,
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
        });
        let clock = FixedClock::epoch();
        let auth = Authority::root();

        run(
            &spec(false),
            "key-2",
            &cache,
            &auth,
            &executor,
            &clock,
            &actor(),
        )
        .unwrap();
        run(
            &spec(false),
            "key-2",
            &cache,
            &auth,
            &executor,
            &clock,
            &actor(),
        )
        .unwrap();

        assert_eq!(*executor.calls.lock().unwrap(), 2);
    }

    #[test]
    fn authority_denied_blocks_execution_and_cache() {
        let cache = FakeCache::new();
        let executor = FakeExecutor::new(ExecutionOutcome {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        let clock = FixedClock::epoch();
        let err = run(
            &spec(true),
            "key-3",
            &cache,
            &Authority::none(),
            &executor,
            &clock,
            &actor(),
        )
        .unwrap_err();

        assert!(matches!(err, TmError::AuthorityDenied(_)));
        assert_eq!(*executor.calls.lock().unwrap(), 0);
        assert!(cache.get("key-3").unwrap().is_none());
    }

    #[test]
    fn executor_error_leaves_no_cache_entry() {
        struct FailingExecutor;
        impl CommandExecutor for FailingExecutor {
            fn execute(&self, _spec: &CommandSpec) -> Result<ExecutionOutcome> {
                Err(TmError::storage("boom"))
            }
        }
        let cache = FakeCache::new();
        let clock = FixedClock::epoch();
        let err = run(
            &spec(true),
            "key-4",
            &cache,
            &Authority::root(),
            &FailingExecutor,
            &clock,
            &actor(),
        )
        .unwrap_err();

        assert!(matches!(err, TmError::Storage(_)));
        assert!(cache.get("key-4").unwrap().is_none());
    }

    fn stored_result(cache: &FakeCache, text: &str) -> CommandResult {
        cache
            .put(
                "q",
                &["echo".to_string()],
                0,
                Timestamp::EPOCH,
                Timestamp::EPOCH,
                text.as_bytes(),
                b"errline1\nerrline2\n",
            )
            .unwrap()
    }

    #[test]
    fn query_head_returns_first_n_lines() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "a\nb\nc\n");
        let answer = result
            .query(ArtifactStream::Stdout, &cache, Query::Head(2))
            .unwrap();
        assert_eq!(
            answer,
            QueryAnswer::Lines(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn query_head_zero_is_not_found() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "a\nb\n");
        let answer = result
            .query(ArtifactStream::Stdout, &cache, Query::Head(0))
            .unwrap();
        assert_eq!(answer, QueryAnswer::NotFound);
    }

    #[test]
    fn query_tail_returns_last_n_lines() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "a\nb\nc\nd\n");
        let answer = result
            .query(ArtifactStream::Stdout, &cache, Query::Tail(2))
            .unwrap();
        assert_eq!(
            answer,
            QueryAnswer::Lines(vec!["d".to_string(), "".to_string()])
        );
    }

    #[test]
    fn query_range_is_one_based_inclusive() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "a\nb\nc\nd\n");
        let answer = result
            .query(ArtifactStream::Stdout, &cache, Query::Range(2, 3))
            .unwrap();
        assert_eq!(
            answer,
            QueryAnswer::Lines(vec!["b".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn query_range_past_end_is_not_found() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "a\nb\n");
        let answer = result
            .query(ArtifactStream::Stdout, &cache, Query::Range(5, 10))
            .unwrap();
        assert_eq!(answer, QueryAnswer::NotFound);
    }

    #[test]
    fn query_grep_returns_matching_lines_from_stderr() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "a\nb\n");
        let answer = result
            .query(
                ArtifactStream::Stderr,
                &cache,
                Query::Grep("line1".to_string()),
            )
            .unwrap();
        assert_eq!(answer, QueryAnswer::Lines(vec!["errline1".to_string()]));
    }

    #[test]
    fn query_grep_no_match_is_not_found() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "a\nb\n");
        let answer = result
            .query(
                ArtifactStream::Stdout,
                &cache,
                Query::Grep("zzz".to_string()),
            )
            .unwrap();
        assert_eq!(answer, QueryAnswer::NotFound);
    }

    #[test]
    fn query_grep_invalid_pattern_is_parse_error() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "a\nb\n");
        let err = result
            .query(ArtifactStream::Stdout, &cache, Query::Grep("(".to_string()))
            .unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }

    #[test]
    fn query_json_pointer_resolves_value() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, r#"{"a": {"b": 42}}"#);
        let answer = result
            .query(
                ArtifactStream::Stdout,
                &cache,
                Query::Json("/a/b".to_string()),
            )
            .unwrap();
        assert_eq!(answer, QueryAnswer::Json(serde_json::json!(42)));
    }

    #[test]
    fn query_json_pointer_missing_is_not_found() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, r#"{"a": 1}"#);
        let answer = result
            .query(
                ArtifactStream::Stdout,
                &cache,
                Query::Json("/missing".to_string()),
            )
            .unwrap();
        assert_eq!(answer, QueryAnswer::NotFound);
    }

    #[test]
    fn query_json_invalid_body_is_parse_error() {
        let cache = FakeCache::new();
        let result = stored_result(&cache, "not json");
        let err = result
            .query(
                ArtifactStream::Stdout,
                &cache,
                Query::Json("/a".to_string()),
            )
            .unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }
}
