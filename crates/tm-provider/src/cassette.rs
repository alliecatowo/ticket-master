//! Record/replay cassettes.
//!
//! [`RecordingProvider`] wraps a real [`Provider`] and appends one [`CassetteEntry`] per
//! successful [`Provider::complete`] call to an in-memory cassette, which [`Cassette::write_jsonl`]
//! persists as JSONL. [`crate::mock::MockProvider::script_from_cassette`] loads a cassette back
//! for a deterministic, network-free replay that serves entries in order (not by exact-hash
//! match — see [`normalize_request`]'s docs for why) and reports any mismatch through
//! [`crate::mock::MockProvider::divergences`] instead of failing outright.
//!
//! See `docs/decisions/D-028-record-replay-harness.md`.

use std::io::{self, BufRead, Write};
use std::path::Path;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tm_types::{Clock, Role, Timestamp};

use crate::fabric::Provider;
use crate::mock::{hash_request, RequestHash};
use crate::types::{
    Completion, CompletionRequest, ContentBlock, EmbedRequest, Embeddings, ProviderError,
};

/// The cassette JSONL format's current version.
pub const CASSETTE_FORMAT_VERSION: u32 = 1;

/// Placeholder substituted for a project-root or tempdir path prefix before a request is hashed
/// or recorded, so identical starting state under a different tempdir hashes equal. See
/// [`normalize_request`].
pub const PATH_PLACEHOLDER: &str = "<CASSETTE_ROOT>";

/// A cassette file's first line: metadata about the whole recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CassetteHeader {
    /// The cassette file format's own version, bumped on a breaking layout change.
    pub format_version: u32,
    /// The harness commit/build epoch the recording was made under, when known.
    pub harness_epoch: Option<u64>,
    /// When the recording was made.
    pub recorded_at: Timestamp,
}

/// One recorded provider call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CassetteEntry {
    /// This call's position in the recording, starting at 0.
    pub seq: u64,
    /// The capability role the call was made for.
    pub role: Role,
    /// The provider that served the call.
    pub provider_id: String,
    /// [`hash_request`] of the normalized request (see [`normalize_request`]), used by
    /// [`crate::mock::MockProvider::script_from_cassette`] replay to report divergences.
    pub request_hash: RequestHash,
    /// The request as it was actually sent (not normalized — it keeps whatever paths the real
    /// run had; only the hash is normalized).
    pub request: CompletionRequest,
    /// The completion the real provider returned.
    pub completion: Completion,
}

/// A parsed cassette: a header plus its entries, in recorded order.
#[derive(Debug, Clone, PartialEq)]
pub struct Cassette {
    /// The recording's metadata.
    pub header: CassetteHeader,
    /// The recorded calls, in order.
    pub entries: Vec<CassetteEntry>,
}

/// Everything that can go wrong reading or writing a cassette file.
#[derive(Debug, thiserror::Error)]
pub enum CassetteError {
    /// The underlying file I/O failed.
    #[error("cassette I/O error: {0}")]
    Io(#[from] io::Error),
    /// A line was not valid JSON, or valid JSON that didn't match the expected shape.
    #[error("cassette line {line} is malformed: {source}")]
    Malformed {
        /// The 1-indexed line number.
        line: usize,
        /// The underlying deserialization error.
        source: serde_json::Error,
    },
    /// The file had no header line at all.
    #[error("cassette file has no header line")]
    MissingHeader,
    /// A header or entry failed to serialize back to JSON.
    #[error("failed to serialize cassette line: {0}")]
    Serialize(serde_json::Error),
}

impl Cassette {
    /// Read a cassette from `path`: the first line is the [`CassetteHeader`], every following
    /// non-empty line one [`CassetteEntry`], in order.
    pub fn read_jsonl(path: &Path) -> Result<Cassette, CassetteError> {
        let file = std::fs::File::open(path)?;
        let reader = io::BufReader::new(file);
        let mut lines = reader.lines();

        let header_line = lines.next().ok_or(CassetteError::MissingHeader)??;
        let header: CassetteHeader = serde_json::from_str(&header_line)
            .map_err(|source| CassetteError::Malformed { line: 1, source })?;

        let mut entries = Vec::new();
        for (idx, line) in lines.enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: CassetteEntry =
                serde_json::from_str(&line).map_err(|source| CassetteError::Malformed {
                    line: idx + 2,
                    source,
                })?;
            entries.push(entry);
        }

        Ok(Cassette { header, entries })
    }

    /// Write this cassette to `path` as JSONL: the header, then one line per entry, in order.
    pub fn write_jsonl(&self, path: &Path) -> Result<(), CassetteError> {
        let mut file = std::fs::File::create(path)?;
        let header_json = serde_json::to_string(&self.header).map_err(CassetteError::Serialize)?;
        writeln!(file, "{header_json}")?;
        for entry in &self.entries {
            let entry_json = serde_json::to_string(entry).map_err(CassetteError::Serialize)?;
            writeln!(file, "{entry_json}")?;
        }
        Ok(())
    }
}

/// Replace every occurrence of `root`'s string form in `req`'s text content with
/// [`PATH_PLACEHOLDER`]. Exact-hash replay into a fresh project diverges on the very first call
/// whenever a prompt embeds the project root or a tempdir path (both routine: file paths in tool
/// results, a system prompt naming the working directory), since that path differs between the
/// recording run and any later run. Normalizing it out before hashing means two requests that
/// differ only in that path hash equal, without needing every caller to pre-scrub its prompts.
pub fn normalize_request(req: &CompletionRequest, root: &Path) -> CompletionRequest {
    let root_str = root.to_string_lossy();
    if root_str.is_empty() {
        return req.clone();
    }
    let mut normalized = req.clone();
    normalized.system = normalized
        .system
        .map(|s| s.replace(root_str.as_ref(), PATH_PLACEHOLDER));
    for message in &mut normalized.messages {
        for block in &mut message.content {
            normalize_content_block(block, root_str.as_ref());
        }
    }
    normalized
}

/// Normalize a single [`ContentBlock`] in place, recursing into a `ToolResult`'s nested content
/// and every string leaf of a `ToolUse`'s JSON `input`.
fn normalize_content_block(block: &mut ContentBlock, root_str: &str) {
    match block {
        ContentBlock::Text { text } => {
            *text = text.replace(root_str, PATH_PLACEHOLDER);
        }
        ContentBlock::ToolUse { input, .. } => normalize_json_strings(input, root_str),
        ContentBlock::ToolResult { content, .. } => {
            for inner in content {
                normalize_content_block(inner, root_str);
            }
        }
    }
}

/// Replace `root_str` in every string leaf of a JSON value, recursing through arrays and objects.
fn normalize_json_strings(value: &mut serde_json::Value, root_str: &str) {
    match value {
        serde_json::Value::String(s) => *s = s.replace(root_str, PATH_PLACEHOLDER),
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_json_strings(item, root_str);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                normalize_json_strings(v, root_str);
            }
        }
        _ => {}
    }
}

/// [`hash_request`] of `req` after [`normalize_request`] — the hash [`CassetteEntry::request_hash`]
/// records, and the one [`crate::mock::MockProvider::script_from_cassette`] replay compares
/// against.
pub fn hash_normalized_request(req: &CompletionRequest, root: &Path) -> RequestHash {
    hash_request(&normalize_request(req, root))
}

/// One request served during cassette replay whose normalized hash didn't match what was
/// recorded at that position — see [`crate::mock::MockProvider::script_from_cassette`].
#[derive(Debug, Clone, PartialEq)]
pub struct Divergence {
    /// The recorded entry's position ([`CassetteEntry::seq`]) this request replayed at.
    pub seq: u64,
    /// The hash recorded in the cassette for this position.
    pub expected_hash: RequestHash,
    /// The hash of the request actually served at this position.
    pub actual_hash: RequestHash,
}

/// Wraps a [`Provider`] and records every successful [`Provider::complete`] call into an
/// in-memory cassette, for later [`Cassette::write_jsonl`]. `embed()` delegates straight through
/// and is never recorded: replay only ever needs to reproduce completions, and an embedder is
/// already deterministic and network-free via [`crate::mock::MockProvider::embed`] when a test
/// needs one.
pub struct RecordingProvider<P: Provider> {
    inner: P,
    role: Role,
    root: std::path::PathBuf,
    clock: std::sync::Arc<dyn Clock>,
    entries: Mutex<Vec<CassetteEntry>>,
}

impl<P: Provider> RecordingProvider<P> {
    /// Wrap `inner`, recording every successful completion as made for `role`, with `root`
    /// normalized out of the recorded hash (see [`normalize_request`]).
    pub fn new(
        inner: P,
        role: Role,
        root: impl Into<std::path::PathBuf>,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Self {
        RecordingProvider {
            inner,
            role,
            root: root.into(),
            clock,
            entries: Mutex::new(Vec::new()),
        }
    }

    /// The entries recorded so far, in order.
    pub fn entries(&self) -> Vec<CassetteEntry> {
        self.entries.lock().clone()
    }

    /// Consume this recorder into a [`Cassette`] ready to [`Cassette::write_jsonl`].
    pub fn into_cassette(self, harness_epoch: Option<u64>) -> Cassette {
        Cassette {
            header: CassetteHeader {
                format_version: CASSETTE_FORMAT_VERSION,
                harness_epoch,
                recorded_at: self.clock.now(),
            },
            entries: self.entries.into_inner(),
        }
    }
}

#[async_trait]
impl<P: Provider> Provider for RecordingProvider<P> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    /// Delegates to `inner`; on success, appends one [`CassetteEntry`] recording the call before
    /// returning it. A failed call is never recorded — a cassette only ever replays completions.
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let result = self.inner.complete(req.clone()).await;
        if let Ok(completion) = &result {
            let hash = hash_normalized_request(&req, &self.root);
            let mut entries = self.entries.lock();
            let seq = entries.len() as u64;
            entries.push(CassetteEntry {
                seq,
                role: self.role,
                provider_id: self.inner.id().to_string(),
                request_hash: hash,
                request: req,
                completion: completion.clone(),
            });
        }
        result
    }

    /// Delegates straight through; embeddings are never recorded (see this type's own docs).
    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        self.inner.embed(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::types::{Candidate, ContentBlock, Message, MessageRole, ModelId, StopReason, Usage};
    use std::time::Duration;
    use tm_types::FixedClock;

    fn make_request(root: &str) -> CompletionRequest {
        CompletionRequest {
            system: Some(format!("working in {root}/project")),
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: vec![],
            max_tokens: 1024,
            temperature: Some(0.0),
            stop_sequences: vec![],
            stream: false,
            n: 1,
            model: None,
        }
    }

    fn make_completion(text: &str) -> Completion {
        Completion {
            model: ModelId::new("test", "model"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: Duration::from_millis(5),
            received_at: Timestamp::EPOCH,
        }
    }

    #[test]
    fn header_and_entries_round_trip_through_jsonl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cassette.jsonl");

        let header = CassetteHeader {
            format_version: CASSETTE_FORMAT_VERSION,
            harness_epoch: Some(7),
            recorded_at: Timestamp::EPOCH,
        };
        let entries = vec![
            CassetteEntry {
                seq: 0,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: 111,
                request: make_request("/tmp/a"),
                completion: make_completion("one"),
            },
            CassetteEntry {
                seq: 1,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: 222,
                request: make_request("/tmp/a"),
                completion: make_completion("two"),
            },
            CassetteEntry {
                seq: 2,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: 333,
                request: make_request("/tmp/a"),
                completion: make_completion("three"),
            },
        ];
        let cassette = Cassette {
            header: header.clone(),
            entries: entries.clone(),
        };

        cassette.write_jsonl(&path).expect("write");
        let read_back = Cassette::read_jsonl(&path).expect("read");

        assert_eq!(read_back.header, header);
        assert_eq!(read_back.entries, entries);
    }

    #[tokio::test]
    async fn recording_provider_writes_lines_that_read_back_matching() {
        let clock = std::sync::Arc::new(FixedClock::epoch());
        let inner = MockProvider::new("test", ModelId::new("test", "model"), clock.clone());
        inner.script_sequence(vec![make_completion("first"), make_completion("second")]);

        let recorder = RecordingProvider::new(inner, Role::CoderFast, "/tmp/proj", clock);

        let result1 = recorder.complete(make_request("/tmp/proj")).await;
        assert!(result1.is_ok());
        let result2 = recorder.complete(make_request("/tmp/proj")).await;
        assert!(result2.is_ok());

        let cassette = recorder.into_cassette(None);
        assert_eq!(cassette.entries.len(), 2);
        assert_eq!(
            cassette.entries[0].request_hash,
            hash_normalized_request(&make_request("/tmp/proj"), Path::new("/tmp/proj"))
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cassette.jsonl");
        cassette.write_jsonl(&path).expect("write");

        let read_back = Cassette::read_jsonl(&path).expect("read");
        assert_eq!(read_back.entries, cassette.entries);
    }

    #[test]
    fn requests_differing_only_in_root_path_hash_equal() {
        let req_a = make_request("/tmp/one");
        let req_b = make_request("/tmp/two");

        let hash_a = hash_normalized_request(&req_a, Path::new("/tmp/one"));
        let hash_b = hash_normalized_request(&req_b, Path::new("/tmp/two"));

        assert_eq!(hash_a, hash_b);
    }

    #[tokio::test]
    async fn replay_serves_in_order_reports_one_divergence_then_unscripted() {
        let clock = std::sync::Arc::new(FixedClock::epoch());
        let root = Path::new("/tmp/proj");

        // Three distinct recorded requests (distinct `max_tokens`), so a wrong-order or
        // wrong-position replay would be observable, not just "some completion came back".
        let mut req0 = make_request("/tmp/proj");
        req0.max_tokens = 100;
        let mut req1 = make_request("/tmp/proj");
        req1.max_tokens = 200;
        let mut req2 = make_request("/tmp/proj");
        req2.max_tokens = 300;

        let mut req1_mutated = req1.clone();
        req1_mutated.max_tokens = 9999;

        let entry1_hash = hash_normalized_request(&req1, root);
        let entries = vec![
            CassetteEntry {
                seq: 0,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: hash_normalized_request(&req0, root),
                request: req0.clone(),
                completion: make_completion("first"),
            },
            CassetteEntry {
                seq: 1,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: entry1_hash,
                request: req1.clone(),
                completion: make_completion("second"),
            },
            CassetteEntry {
                seq: 2,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: hash_normalized_request(&req2, root),
                request: req2.clone(),
                completion: make_completion("third"),
            },
        ];
        let cassette = Cassette {
            header: CassetteHeader {
                format_version: CASSETTE_FORMAT_VERSION,
                harness_epoch: None,
                recorded_at: Timestamp::EPOCH,
            },
            entries,
        };

        let replayer = MockProvider::new("test", ModelId::new("test", "model"), clock);
        replayer.script_from_cassette(&cassette, root);

        // First call matches the recorded request exactly, and gets entry 0's completion.
        let result0 = replayer.complete(req0).await.expect("first replay call");
        assert_eq!(result0, make_completion("first"));
        assert_eq!(replayer.divergences().len(), 0);

        // Second call's request was mutated relative to what was recorded at position 1: still
        // served position 1's completion (order holds), but flagged as a divergence whose
        // expected_hash ties back to entry 1 specifically, proving the comparison is positional.
        let result1 = replayer
            .complete(req1_mutated)
            .await
            .expect("second replay call");
        assert_eq!(result1, make_completion("second"));
        let divergences = replayer.divergences();
        assert_eq!(divergences.len(), 1);
        assert_eq!(divergences[0].seq, 1);
        assert_eq!(divergences[0].expected_hash, entry1_hash);
        assert_ne!(divergences[0].expected_hash, divergences[0].actual_hash);

        // Third call matches its recorded request again and gets entry 2's completion, still in
        // order and with no new divergence.
        let result2 = replayer.complete(req2).await.expect("third replay call");
        assert_eq!(result2, make_completion("third"));
        assert_eq!(replayer.divergences().len(), 1);

        // Cassette exhausted: falls through to Unscripted.
        let result3 = replayer.complete(make_request("/tmp/proj")).await;
        assert!(matches!(result3, Err(ProviderError::Unscripted(_))));
    }
}
