//! Record/replay cassettes.
//!
//! [`RecordingProvider`] wraps a real [`Provider`] and appends one [`CassetteEntry`] per
//! successful [`Provider::complete`] call, both to an in-memory list (for
//! [`RecordingProvider::into_cassette`]) and, when opened with [`RecordingProvider::create`], to a
//! [`CassetteWriter`] that fsyncs each entry straight to disk as it happens. That incremental write
//! is what makes a recording survive a kill mid-run (a wall-clock bound, `SIGTERM`, a crash): the
//! header lands on disk before the first provider call is even made, and every completed entry
//! after it is durable the moment that call returns, rather than only ever written once at the very
//! end via [`Cassette::write_jsonl`]. [`Cassette::read_jsonl`] reads either shape back — a whole
//! file written by `write_jsonl`, or one left behind by a killed [`CassetteWriter`] — tolerating a
//! last line cut off mid-write (no trailing newline) by treating the recording as having ended at
//! the last complete entry instead of failing the whole read.
//! [`crate::mock::MockProvider::script_from_cassette`] loads a cassette back for a deterministic,
//! network-free replay that serves entries in order (not by exact-hash match — see
//! [`normalize_request`]'s docs for why) and reports any mismatch through
//! [`crate::mock::MockProvider::divergences`] instead of failing outright.
//!
//! See `docs/decisions/D-028-record-replay-harness.md`.

use std::io::{self, Write};
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
    ///
    /// Tolerant of a file left behind by a [`CassetteWriter`] killed mid-write: reads raw bytes
    /// (not [`BufRead::lines`], which errors on invalid UTF-8 — a real risk here, since a
    /// completion cut off mid-write can land inside a multi-byte character) and, only when the
    /// file's last byte is not a newline (meaning the very last line was never finished), treats a
    /// parse failure on that last line as truncation rather than corruption: the recording is read
    /// as having ended at the last complete entry instead of failing the whole read. A parse
    /// failure anywhere else — including the header — is still a real, reported error, since the
    /// file has a complete line there that simply isn't valid JSON in the expected shape.
    pub fn read_jsonl(path: &Path) -> Result<Cassette, CassetteError> {
        let bytes = std::fs::read(path)?;
        if bytes.is_empty() {
            return Err(CassetteError::MissingHeader);
        }
        // Every fsynced write from `CassetteWriter`/`write_jsonl` ends in `\n`; a file whose last
        // byte is not `\n` was cut off mid-write to its final line.
        let complete = bytes.last() == Some(&b'\n');
        let mut raw_lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
        if complete {
            // `split` leaves a trailing empty slice after the final newline; drop it so the last
            // real line is what `.peek()` below sees as "last".
            raw_lines.pop();
        }

        let mut lines = raw_lines.into_iter().enumerate().peekable();
        let (_, header_bytes) = lines.next().ok_or(CassetteError::MissingHeader)?;
        let header: CassetteHeader = match serde_json::from_slice(header_bytes) {
            Ok(header) => header,
            Err(source) => {
                if !complete && lines.peek().is_none() {
                    // The header itself was the file's only, unfinished line: nothing was ever
                    // durably recorded, so there is nothing to salvage as a partial read.
                    return Err(CassetteError::MissingHeader);
                }
                return Err(CassetteError::Malformed { line: 1, source });
            }
        };

        let mut entries = Vec::new();
        while let Some((idx, line_bytes)) = lines.next() {
            if line_bytes.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            match serde_json::from_slice::<CassetteEntry>(line_bytes) {
                Ok(entry) => entries.push(entry),
                Err(source) => {
                    if !complete && lines.peek().is_none() {
                        // Cut off mid-write to this, the file's last line: stop here rather than
                        // erroring the whole read.
                        break;
                    }
                    return Err(CassetteError::Malformed {
                        line: idx + 1,
                        source,
                    });
                }
            }
        }

        Ok(Cassette { header, entries })
    }

    /// Write this cassette to `path` as JSONL in one shot: the header, then one line per entry, in
    /// order. Prefer [`CassetteWriter`] for a recording made incrementally over time (e.g. during a
    /// live `tm run --record`), since this truncates and rewrites the whole file and so offers no
    /// protection against a kill mid-write; this method mainly serves tests and any one-shot
    /// caller that already has every entry in hand.
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

/// Appends a cassette to disk one entry at a time, fsyncing each write so the file on disk never
/// falls behind what's actually been recorded — the fix for a recording that a kill (wall-clock
/// bound, `SIGTERM`, crash) mid-run used to lose entirely, since [`Cassette::write_jsonl`] only
/// ever wrote the whole file once, at the very end. Every write is `write_all` immediately followed
/// by `File::sync_data`, so a completed [`Provider::complete`] call is durable before the caller
/// moves on to the next one. That fsync-per-entry cost is negligible in practice: a provider call
/// it follows already took whole seconds over the network, so an extra sub-millisecond fsync is
/// noise, not a bottleneck — the pattern optimizes for "never lose a finished call", not raw
/// append throughput.
pub struct CassetteWriter {
    file: std::fs::File,
}

impl CassetteWriter {
    /// Create `path` and durably write `header` as its first line before returning, so even a kill
    /// immediately after this call still leaves a valid (if entry-less) cassette on disk instead of
    /// no file at all.
    pub fn create(path: &Path, header: &CassetteHeader) -> Result<Self, CassetteError> {
        let mut file = std::fs::File::create(path)?;
        let header_json = serde_json::to_string(header).map_err(CassetteError::Serialize)?;
        file.write_all(header_json.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(CassetteWriter { file })
    }

    /// Append one entry and fsync before returning, so it's durable on disk the instant this call
    /// completes.
    pub fn append(&mut self, entry: &CassetteEntry) -> Result<(), CassetteError> {
        let entry_json = serde_json::to_string(entry).map_err(CassetteError::Serialize)?;
        self.file.write_all(entry_json.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.sync_data()?;
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
/// in-memory cassette, for later [`Cassette::write_jsonl`], and — when built with
/// [`RecordingProvider::create`] — also incrementally to a [`CassetteWriter`], so the recording
/// survives a kill mid-run instead of existing only in memory until [`RecordingProvider::into_cassette`]
/// is called. `embed()` delegates straight through and is never recorded: replay only ever needs
/// to reproduce completions, and an embedder is already deterministic and network-free via
/// [`crate::mock::MockProvider::embed`] when a test needs one.
pub struct RecordingProvider<P: Provider> {
    inner: P,
    role: Role,
    root: std::path::PathBuf,
    clock: std::sync::Arc<dyn Clock>,
    entries: Mutex<Vec<CassetteEntry>>,
    writer: Option<Mutex<CassetteWriter>>,
}

impl<P: Provider> RecordingProvider<P> {
    /// Wrap `inner`, recording every successful completion as made for `role`, with `root`
    /// normalized out of the recorded hash (see [`normalize_request`]), in memory only. Prefer
    /// [`RecordingProvider::create`] for any real (non-test) recording, since a process killed
    /// before [`RecordingProvider::into_cassette`] runs loses everything recorded this way.
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
            writer: None,
        }
    }

    /// Wrap `inner` like [`RecordingProvider::new`], additionally opening a [`CassetteWriter`] at
    /// `path` up front (its header is durably written before this returns) and appending each
    /// entry to it as it's recorded, so a kill at any point after this call still leaves a valid,
    /// readable-by-[`Cassette::read_jsonl`] cassette on disk with every entry recorded so far.
    pub fn create(
        inner: P,
        role: Role,
        root: impl Into<std::path::PathBuf>,
        clock: std::sync::Arc<dyn Clock>,
        path: &Path,
        harness_epoch: Option<u64>,
    ) -> Result<Self, CassetteError> {
        let header = CassetteHeader {
            format_version: CASSETTE_FORMAT_VERSION,
            harness_epoch,
            recorded_at: clock.now(),
        };
        let writer = CassetteWriter::create(path, &header)?;
        Ok(RecordingProvider {
            inner,
            role,
            root: root.into(),
            clock,
            entries: Mutex::new(Vec::new()),
            writer: Some(Mutex::new(writer)),
        })
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
            let entry = CassetteEntry {
                seq,
                role: self.role,
                provider_id: self.inner.id().to_string(),
                request_hash: hash,
                request: req,
                completion: completion.clone(),
            };
            // Best-effort: a disk write failure here (e.g. the cassette's directory disappeared
            // mid-run) shouldn't fail the completion the caller is actually waiting on. It just
            // means this one entry, and any after it, won't be durable on disk — the in-memory
            // `entries` list below still has it for a normal, non-killed finish.
            if let Some(writer) = &self.writer {
                let _ = writer.lock().append(&entry);
            }
            entries.push(entry);
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

    #[test]
    fn cassette_writer_survives_being_dropped_mid_recording() {
        // Proves the first half of u1-record-flush-incremental's acceptance criteria: a writer
        // that is dropped (simulating a kill) without ever calling `Cassette::write_jsonl`
        // still leaves every entry it appended readable back.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cassette.jsonl");
        let header = CassetteHeader {
            format_version: CASSETTE_FORMAT_VERSION,
            harness_epoch: Some(3),
            recorded_at: Timestamp::EPOCH,
        };
        let mut writer = CassetteWriter::create(&path, &header).expect("create");
        let entries = vec![
            CassetteEntry {
                seq: 0,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: 1,
                request: make_request("/tmp/a"),
                completion: make_completion("one"),
            },
            CassetteEntry {
                seq: 1,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: 2,
                request: make_request("/tmp/a"),
                completion: make_completion("two"),
            },
            CassetteEntry {
                seq: 2,
                role: Role::CoderFast,
                provider_id: "test".to_string(),
                request_hash: 3,
                request: make_request("/tmp/a"),
                completion: make_completion("three"),
            },
        ];
        for entry in &entries {
            writer.append(entry).expect("append");
        }
        drop(writer); // never call write_jsonl -- this is the point of the test

        let read_back = Cassette::read_jsonl(&path).expect("read a writer-only file");
        assert_eq!(read_back.header, header);
        assert_eq!(read_back.entries, entries);
    }

    #[test]
    fn read_jsonl_tolerates_a_last_entry_cut_off_mid_write() {
        // The other half: a file left behind by a `CassetteWriter` killed mid-`append` (no
        // trailing newline on its last line) reads back every complete entry before the cut,
        // instead of failing the whole read.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cassette.jsonl");
        let header = CassetteHeader {
            format_version: CASSETTE_FORMAT_VERSION,
            harness_epoch: Some(3),
            recorded_at: Timestamp::EPOCH,
        };
        let complete_entry = CassetteEntry {
            seq: 0,
            role: Role::CoderFast,
            provider_id: "test".to_string(),
            request_hash: 1,
            request: make_request("/tmp/a"),
            completion: make_completion("one"),
        };

        let mut writer = CassetteWriter::create(&path, &header).expect("create");
        writer.append(&complete_entry).expect("append");
        drop(writer);

        // Simulate a kill mid-`append` of a *second* entry: append raw, truncated JSON with no
        // trailing newline directly, bypassing `CassetteWriter` (which always writes a complete
        // line before returning).
        let mut raw = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for raw append");
        raw.write_all(br#"{"seq":1,"role":"coder.fast","incomple"#)
            .expect("write truncated bytes");
        drop(raw);

        let read_back = Cassette::read_jsonl(&path).expect("tolerate the truncated last line");
        assert_eq!(read_back.header, header);
        assert_eq!(read_back.entries, vec![complete_entry]);
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
