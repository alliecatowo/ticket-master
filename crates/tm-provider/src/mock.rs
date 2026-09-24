//! [`MockProvider`]: the deterministic provider every test in the workspace uses instead of a
//! network call.
//!
//! Responses are scripted by a hash of the request (so the same request always gets the same
//! answer without the caller threading through a request id), plus optional injected failures,
//! latency and quota-exhaustion behavior. No variant of this provider ever performs I/O.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tm_types::Clock;

use crate::cassette::{hash_normalized_request, Cassette, Divergence};
use crate::decide::{Answer, AnswerValue, DecideLimits, DecideRequest, DecideResponse, Question};
use crate::fabric::Provider;
use crate::types::{
    Candidate, Completion, CompletionRequest, ContentBlock, EmbedRequest, Embeddings, ModelId,
    ProviderError, StopReason, Usage,
};

/// A canonical hash of a [`CompletionRequest`], used as the script lookup key.
///
/// Two requests that are equal after normalizing away fields that don't affect the "meaning" of
/// the request (currently: none — every field is significant) hash equally.
pub type RequestHash = u64;

/// Hash a [`CompletionRequest`] into a [`RequestHash`].
///
/// Serializes `req` to canonical JSON via `serde_json::to_string` (field order is stable
/// because `CompletionRequest`'s fields are declared in a fixed order and serde_json preserves
/// struct field order), then hashes the bytes with `std::hash::DefaultHasher`.
pub fn hash_request(req: &CompletionRequest) -> RequestHash {
    // `model` is left out: `Fabric` fills it in on the way to the provider, and a script is keyed
    // on the request its caller built.
    let req = CompletionRequest {
        model: None,
        ..req.clone()
    };
    // Invariant: `CompletionRequest` contains no map with non-string keys and no type whose
    // `Serialize` impl can fail, so serialization to a `String` cannot error.
    let json =
        serde_json::to_string(&req).expect("CompletionRequest serialization should never fail");
    let mut hasher = std::hash::DefaultHasher::new();
    json.as_bytes().hash(&mut hasher);
    hasher.finish()
}

/// One scripted failure to inject for a matching request.
#[derive(Debug, Clone)]
pub struct ScriptedFailure {
    /// How many times this failure fires before falling through to the next script entry (or a
    /// generic "unscripted" error). `None` means it fires forever.
    pub times: Option<u32>,
    /// The error to return.
    pub error: ProviderError,
}

/// A scripted response or behavior for one request hash.
#[derive(Debug, Clone)]
pub enum Script {
    /// Return this completion.
    Respond(Completion),
    /// Fail with this error.
    Fail(ScriptedFailure),
    /// Report the candidate as quota-exhausted: a [`ProviderError::RateLimited`] with the given
    /// `retry_after`.
    Exhausted {
        /// How long the caller should wait before retrying.
        retry_after: Duration,
    },
}

/// A deterministic, network-free [`Provider`]. Every test in the workspace that needs a
/// completion or embedding uses this instead of [`crate::anthropic::AnthropicProvider`].
pub struct MockProvider {
    id: String,
    model: ModelId,
    clock: std::sync::Arc<dyn Clock>,
    scripts: Mutex<HashMap<RequestHash, Script>>,
    default_latency: Duration,
    embed_dim: usize,
    call_log: Mutex<Vec<CompletionRequest>>,
    /// The fallback [`Script`] served when a request's hash matches nothing in `scripts` — see
    /// [`MockProvider::script_default_response`]. `None` (the default) preserves this type's
    /// original behavior exactly: an unscripted request fails with [`ProviderError::Unscripted`].
    default: Mutex<Option<Script>>,
    /// A FIFO queue of completions to serve on successive `complete()` calls, regardless of
    /// request content. Exhausted after all items are served; the normal script lookup then takes over.
    sequence: Mutex<VecDeque<Completion>>,
    /// Set alongside `sequence` by [`MockProvider::script_from_cassette`]: the `(seq,
    /// request_hash)` recorded for each entry still in `sequence`, in the same order, so replay
    /// can compare what it's actually being asked against what was recorded and report a
    /// [`Divergence`] instead of failing. Empty when `sequence` was populated by
    /// [`MockProvider::script_sequence`] directly rather than from a cassette.
    replay_expected: Mutex<VecDeque<(u64, RequestHash)>>,
    /// The `root` a cassette was loaded with (see [`crate::cassette::normalize_request`]), used
    /// to normalize each served request the same way before comparing its hash.
    replay_root: Mutex<Option<PathBuf>>,
    /// Divergences recorded so far during cassette replay — see
    /// [`MockProvider::script_from_cassette`] and [`MockProvider::divergences`].
    divergences: Mutex<Vec<Divergence>>,
}

impl MockProvider {
    /// A mock provider identifying itself as `id`/`model`, with no scripted responses yet: every
    /// request will fail with [`ProviderError::Unscripted`] until [`MockProvider::script`] is
    /// called.
    pub fn new(id: impl Into<String>, model: ModelId, clock: std::sync::Arc<dyn Clock>) -> Self {
        MockProvider {
            id: id.into(),
            model,
            clock,
            scripts: Mutex::new(HashMap::new()),
            default_latency: Duration::from_millis(0),
            embed_dim: 8,
            call_log: Mutex::new(Vec::new()),
            default: Mutex::new(None),
            sequence: Mutex::new(VecDeque::new()),
            replay_expected: Mutex::new(VecDeque::new()),
            replay_root: Mutex::new(None),
            divergences: Mutex::new(Vec::new()),
        }
    }

    /// Script the response for any request that hashes equal to `req`'s hash.
    pub fn script_response(&self, req: &CompletionRequest, completion: Completion) {
        self.scripts
            .lock()
            .insert(hash_request(req), Script::Respond(completion));
    }

    /// Script a failure for any request that hashes equal to `req`'s hash.
    pub fn script_failure(&self, req: &CompletionRequest, failure: ScriptedFailure) {
        self.scripts
            .lock()
            .insert(hash_request(req), Script::Fail(failure));
    }

    /// Script quota exhaustion for any request that hashes equal to `req`'s hash.
    pub fn script_exhausted(&self, req: &CompletionRequest, retry_after: Duration) {
        self.scripts
            .lock()
            .insert(hash_request(req), Script::Exhausted { retry_after });
    }

    /// Script an ordered sequence of responses to serve on successive `complete()` calls,
    /// regardless of request content. Once exhausted, falls through to exact-hash script lookup,
    /// then default response, then `ProviderError::Unscripted`.
    ///
    /// All requests are still recorded in the call log, so a caller can inspect and verify
    /// them after the sequence is exhausted.
    pub fn script_sequence(&self, completions: Vec<Completion>) {
        *self.sequence.lock() = VecDeque::from(completions);
        // A prior script_from_cassette's expected hashes/root no longer apply to this sequence;
        // leaving them would compare this sequence's calls against a stale cassette's recorded
        // hashes and produce false divergences.
        self.replay_expected.lock().clear();
        *self.replay_root.lock() = None;
    }

    /// Return the number of completions remaining in the sequence set by [`MockProvider::script_sequence`].
    pub fn sequence_remaining(&self) -> usize {
        self.sequence.lock().len()
    }

    /// Load `cassette`'s entries into the ordered sequence API (same mechanism as
    /// [`MockProvider::script_sequence`]), for a deterministic replay of a real recording.
    ///
    /// Replay is ordered, not exact-hash: the entries are served strictly in their recorded
    /// order regardless of what each `complete()` call's request actually contains, because
    /// exact-hash replay into a fresh project diverges on the very first call whenever a prompt
    /// carries a path, timestamp or id that differs between the recording run and this one (see
    /// `docs/decisions/D-028-record-replay-harness.md`). Each served request's normalized hash
    /// (`root`-relative, same as [`CassetteEntry::request_hash`] was computed) is still compared
    /// against what was recorded at that position; a mismatch is recorded as a [`Divergence`]
    /// rather than failing the call, readable afterward through [`MockProvider::divergences`].
    /// Clears any previously recorded divergences.
    pub fn script_from_cassette(&self, cassette: &Cassette, root: &Path) {
        let mut sequence = VecDeque::new();
        let mut expected = VecDeque::new();
        for entry in &cassette.entries {
            sequence.push_back(entry.completion.clone());
            expected.push_back((entry.seq, entry.request_hash));
        }
        *self.sequence.lock() = sequence;
        *self.replay_expected.lock() = expected;
        *self.replay_root.lock() = Some(root.to_path_buf());
        self.divergences.lock().clear();
    }

    /// Every divergence recorded during cassette replay so far — a served request whose
    /// normalized hash didn't match what [`MockProvider::script_from_cassette`]'s cassette
    /// recorded at that position. Empty when nothing diverged, or when replay was never used.
    pub fn divergences(&self) -> Vec<Divergence> {
        self.divergences.lock().clone()
    }

    /// Script the response served for *any* request that does not match a hash-keyed script
    /// from [`MockProvider::script_response`]/[`MockProvider::script_failure`]/
    /// [`MockProvider::script_exhausted`].
    ///
    /// For a caller that cannot compute the exact [`CompletionRequest`] a request-under-test
    /// will build (e.g. an out-of-process integration test driving a compiled binary, which has
    /// no way to reproduce that binary's internal prompt rendering to get an exact hash match),
    /// this is the only way to get a deterministic, network-free reply at all. Overwrites any
    /// previously scripted default.
    pub fn script_default_response(&self, completion: Completion) {
        *self.default.lock() = Some(Script::Respond(completion));
    }

    /// Set the latency reported for unscripted-latency responses (scripted `Completion::latency`
    /// values, when present, take precedence).
    pub fn set_default_latency(&mut self, latency: Duration) {
        self.default_latency = latency;
    }

    /// Every request this provider has received, in order, for test assertions.
    pub fn call_log(&self) -> Vec<CompletionRequest> {
        self.call_log.lock().clone()
    }

    /// Build a deterministic completion for `req` without registering it as a script: a
    /// convenience for tests that don't care about exact content, only that a call succeeded.
    ///
    /// Derives a short deterministic text body from `hash_request(req)`, wraps it in a single
    /// `Text` content block with `StopReason::EndTurn`, estimates `Usage` from input message
    /// byte lengths plus a fixed output token count, and uses the request's model (else `self.model`),
    /// `self.default_latency`, and `received_at: self.clock.now()`.
    pub fn deterministic_completion(&self, req: &CompletionRequest) -> Completion {
        let hash = hash_request(req);
        let text = format!("mock-{:x}", hash);

        let input_tokens = req
            .messages
            .iter()
            .flat_map(|msg| &msg.content)
            .map(|block| match block {
                ContentBlock::Text { text } => (text.len() / 4) as u32,
                ContentBlock::ToolUse { input, .. } => (input.to_string().len() / 4) as u32,
                ContentBlock::ToolResult { content, .. } => content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => (text.len() / 4) as u32,
                        _ => 0,
                    })
                    .sum(),
            })
            .sum();

        let output_tokens = 10u32;

        Completion {
            model: req.model_or(&self.model),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text { text }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: self.default_latency,
            received_at: self.clock.now(),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    /// Serve `req` from the script sequence first, then the script table, falling back to [`ProviderError::Unscripted`].
    ///
    /// Pushes `req.clone()` onto `call_log`, then:
    /// 1. If a sequence was set via [`MockProvider::script_sequence`] or
    ///    [`MockProvider::script_from_cassette`], pops and returns the next completion from the
    ///    front, regardless of request content. When the sequence came from a cassette, also
    ///    compares `req`'s normalized hash to what was recorded at that position
    ///    ([`crate::cassette::CassetteEntry::request_hash`]) and records a [`Divergence`] on a
    ///    mismatch, rather than failing the call.
    /// 2. Looks up `hash_request(req)` in `scripts`. If `Respond(completion)`, returns
    ///    `Ok(completion.clone())`. If `Fail(f)`, decrements `f.times` if `Some` (removing the
    ///    script entry once it reaches zero so the next call falls through), and returns
    ///    `Err(f.error.clone())`. If `Exhausted { retry_after }`, returns `Err(ProviderError::RateLimited { ... })`.
    /// 3. Falls back to whatever `script_default_response` set, if anything.
    /// 4. Returns `Err(ProviderError::Unscripted)` if no script or sequence is found.
    ///
    /// This method never sleeps or awaits real time — latency is data on the returned
    /// `Completion`, not an actual delay, since tests must stay fast and deterministic.
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        self.call_log.lock().push(req.clone());

        // Check the sequence first: if populated, serve the next completion regardless of request.
        let popped = self.sequence.lock().pop_front();
        if let Some(completion) = popped {
            // If this sequence came from a cassette (script_from_cassette), compare this
            // request's normalized hash against what was recorded at this position and record
            // any mismatch as a Divergence rather than failing the call.
            if let Some(root) = self.replay_root.lock().clone() {
                if let Some((seq, expected_hash)) = self.replay_expected.lock().pop_front() {
                    let actual_hash = hash_normalized_request(&req, &root);
                    if actual_hash != expected_hash {
                        self.divergences.lock().push(Divergence {
                            seq,
                            expected_hash,
                            actual_hash,
                        });
                    }
                }
            }
            return Ok(completion);
        }

        let hash = hash_request(&req);
        {
            let mut scripts = self.scripts.lock();
            match scripts.get_mut(&hash) {
                Some(Script::Respond(completion)) => return Ok(completion.clone()),
                Some(Script::Fail(failure)) => {
                    let error = failure.error.clone();
                    if let Some(times) = &mut failure.times {
                        *times -= 1;
                        if *times == 0 {
                            scripts.remove(&hash);
                        }
                    }
                    return Err(error);
                }
                Some(Script::Exhausted { retry_after }) => {
                    return Err(ProviderError::RateLimited {
                        message: "mock quota exhausted".into(),
                        retry_after: Some(*retry_after),
                    })
                }
                None => {}
            }
        }

        // No sequence item or exact-hash script matched: fall back to whatever
        // `script_default_response` set, if anything, before finally giving up as unscripted.
        // The default never expires (unlike a `Script::Fail`'s `times`) — it exists precisely
        // for a caller that cannot script by exact hash at all, so there is no notion of it
        // being "used up".
        match self.default.lock().as_ref() {
            Some(Script::Respond(completion)) => Ok(completion.clone()),
            Some(Script::Fail(failure)) => Err(failure.error.clone()),
            Some(Script::Exhausted { retry_after }) => Err(ProviderError::RateLimited {
                message: "mock quota exhausted".into(),
                retry_after: Some(*retry_after),
            }),
            None => Err(ProviderError::Unscripted(format!(
                "no script for hash {hash:x}"
            ))),
        }
    }

    /// Deterministically embed `req.inputs` without any scripting: every text maps to a fixed
    /// small vector derived from its own hash, so identical inputs always produce identical
    /// vectors and tests can assert on embedding equality/inequality without real model calls.
    ///
    /// For each input string, hashes it with a stable hasher, then expands the hash into
    /// `self.embed_dim` floats in `[-1.0, 1.0]` via a deterministic mixing function (splitmix64).
    /// Usage is `input_tokens` estimated from total input byte length / 4, with everything else zero.
    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let mut vectors = Vec::with_capacity(req.inputs.len());
        let mut total_bytes = 0u32;

        for input in &req.inputs {
            total_bytes = total_bytes.saturating_add(input.len() as u32);

            let mut hasher = std::hash::DefaultHasher::new();
            input.hash(&mut hasher);
            let seed = hasher.finish();

            let mut vector = Vec::with_capacity(self.embed_dim);
            let mut state = seed;

            for _ in 0..self.embed_dim {
                state ^= state >> 30;
                state = state.wrapping_mul(0xbf58476d1ce4e5b9);
                state ^= state >> 27;

                let bits = (state >> 11) & 0xfffff;
                let normalized = (bits as f32 / 0xfffff as f32) * 2.0 - 1.0;
                vector.push(normalized);
            }

            vectors.push(vector);
        }

        let input_tokens = total_bytes / 4;

        Ok(Embeddings {
            model: self.model.clone(),
            vectors,
            usage: Usage {
                input_tokens,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        })
    }
}

/// A canonical hash of a [`DecideRequest`], used as [`MockDecisionProvider`]'s script lookup
/// key — same purpose as [`hash_request`], for the decider request shape instead.
pub type DecideRequestHash = u64;

/// Hash a [`DecideRequest`] into a [`DecideRequestHash`] by serializing it to canonical JSON
/// (stable because `DecideRequest`'s fields are declared in a fixed order and `questions` is a
/// `BTreeMap`, so key order is stable too) and hashing the bytes.
pub fn hash_decide_request(req: &DecideRequest) -> DecideRequestHash {
    // Invariant: `DecideRequest` contains no type whose `Serialize` impl can fail.
    let json = serde_json::to_string(req).expect("DecideRequest serialization should never fail");
    let mut hasher = std::hash::DefaultHasher::new();
    json.as_bytes().hash(&mut hasher);
    hasher.finish()
}

/// A deterministic, network-free [`crate::decide::DecisionProvider`]: every test that needs a
/// decision uses this instead of a real System One backend. Responses are scripted by a hash of
/// the request, same pattern as [`MockProvider`] — no variant of this provider ever performs I/O.
/// `decide` returns [`ProviderError`], the same error type [`Provider::complete`] uses, so a
/// caller doesn't need a second error path just because the candidate is a decider.
pub struct MockDecisionProvider {
    id: String,
    model: ModelId,
    limits: DecideLimits,
    scripts: Mutex<HashMap<DecideRequestHash, Result<DecideResponse, ProviderError>>>,
    call_log: Mutex<Vec<DecideRequest>>,
}

impl MockDecisionProvider {
    /// A mock decider identifying itself as `id`/`model`, with no scripted responses yet: every
    /// request will fail with [`ProviderError::Unscripted`] until
    /// [`MockDecisionProvider::script`] is called.
    pub fn new(id: impl Into<String>, model: ModelId) -> Self {
        MockDecisionProvider {
            id: id.into(),
            model,
            limits: DecideLimits {
                max_context_tokens: 1024,
                max_options: 10,
                kinds: vec![
                    crate::decide::QuestionKind::Choice,
                    crate::decide::QuestionKind::Score,
                    crate::decide::QuestionKind::Noul,
                ],
            },
            scripts: Mutex::new(HashMap::new()),
            call_log: Mutex::new(Vec::new()),
        }
    }

    /// Override the [`DecideLimits`] this mock reports.
    pub fn set_limits(&mut self, limits: DecideLimits) {
        self.limits = limits;
    }

    /// Script the response for any request that hashes equal to `req`'s hash.
    pub fn script(&self, req: &DecideRequest, response: DecideResponse) {
        self.scripts
            .lock()
            .insert(hash_decide_request(req), Ok(response));
    }

    /// Script a failure for any request that hashes equal to `req`'s hash.
    pub fn script_failure(&self, req: &DecideRequest, error: ProviderError) {
        self.scripts
            .lock()
            .insert(hash_decide_request(req), Err(error));
    }

    /// Every request this provider has received, in order, for test assertions.
    pub fn call_log(&self) -> Vec<DecideRequest> {
        self.call_log.lock().clone()
    }

    /// Build a deterministic [`DecideResponse`] for `req` without registering it as a script: a
    /// convenience for tests that don't care about exact content, only that a call succeeded and
    /// is stable across repeated calls with the same request.
    ///
    /// For each question, picks an answer derived from `hash_decide_request(req)` mixed with the
    /// question id, so the same request always answers the same way and a different request
    /// answers differently.
    pub fn deterministic_response(&self, req: &DecideRequest) -> DecideResponse {
        let base_hash = hash_decide_request(req);
        let mut answers = std::collections::BTreeMap::new();

        for (qid, question) in &req.questions {
            let mut hasher = std::hash::DefaultHasher::new();
            (base_hash, qid).hash(&mut hasher);
            let mixed = hasher.finish();

            let answer = match question {
                Question::Choice { options, .. } => {
                    let n = options.len().max(1);
                    let idx = (mixed as usize) % n;
                    let mut probabilities = vec![0.0f32; options.len()];
                    if let Some(slot) = probabilities.get_mut(idx) {
                        *slot = 1.0;
                    }
                    Answer {
                        value: AnswerValue::Choice(
                            options
                                .get(idx)
                                .map(|o| o.label.clone())
                                .unwrap_or_else(|| "none".to_string()),
                        ),
                        confidence: 0.9,
                        probabilities,
                    }
                }
                Question::Score { levels, .. } => {
                    let n = levels.len().max(1);
                    let idx = (mixed as usize) % n;
                    let mut probabilities = vec![0.0f32; levels.len()];
                    if let Some(slot) = probabilities.get_mut(idx) {
                        *slot = 1.0;
                    }
                    Answer {
                        value: AnswerValue::Score(
                            levels
                                .get(idx)
                                .map(|l| l.label.clone())
                                .unwrap_or_else(|| "none".to_string()),
                        ),
                        confidence: 0.9,
                        probabilities,
                    }
                }
                Question::Noul { .. } => Answer {
                    value: AnswerValue::Noul(mixed % 2 == 0),
                    confidence: 0.9,
                    probabilities: vec![],
                },
            };

            answers.insert(qid.clone(), answer);
        }

        DecideResponse {
            model: self.model.clone(),
            answers,
        }
    }
}

#[async_trait]
impl crate::decide::DecisionProvider for MockDecisionProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn limits(&self) -> DecideLimits {
        self.limits.clone()
    }

    /// Serve `req` from the script table, falling back to [`ProviderError::Unscripted`].
    async fn decide(&self, req: DecideRequest) -> Result<DecideResponse, ProviderError> {
        self.call_log.lock().push(req.clone());

        let hash = hash_decide_request(&req);
        match self.scripts.lock().get(&hash) {
            Some(Ok(response)) => Ok(response.clone()),
            Some(Err(error)) => Err(error.clone()),
            None => Err(ProviderError::Unscripted(format!(
                "no script for hash {hash:x}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::FixedClock;

    fn make_test_request() -> CompletionRequest {
        CompletionRequest {
            system: Some("test system".to_string()),
            messages: vec![],
            tools: vec![],
            max_tokens: 1024,
            temperature: Some(0.5),
            stop_sequences: vec![],
            stream: false,
            n: 1,
            model: None,
        }
    }

    fn make_test_clock() -> std::sync::Arc<dyn Clock> {
        std::sync::Arc::new(FixedClock::epoch())
    }

    #[test]
    fn hash_request_deterministic() {
        let req = make_test_request();
        let hash1 = hash_request(&req);
        let hash2 = hash_request(&req);
        assert_eq!(hash1, hash2, "same request should hash identically");
    }

    #[test]
    fn hash_request_different_for_different_requests() {
        let req1 = make_test_request();
        let mut req2 = make_test_request();
        req2.max_tokens = 2048;

        let hash1 = hash_request(&req1);
        let hash2 = hash_request(&req2);
        assert_ne!(hash1, hash2, "different requests should hash differently");
    }

    #[test]
    fn deterministic_completion_has_correct_structure() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock.clone());
        let req = make_test_request();

        let completion = provider.deterministic_completion(&req);

        assert_eq!(completion.model.provider, "test");
        assert_eq!(completion.model.model, "model");
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage.output_tokens, 10);
        assert_eq!(completion.received_at, tm_types::Timestamp::EPOCH);
    }

    #[test]
    fn deterministic_completion_contains_hash_in_text() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock.clone());
        let req = make_test_request();

        let completion = provider.deterministic_completion(&req);
        let content = &completion.candidates[0].content;
        assert_eq!(content.len(), 1);

        if let ContentBlock::Text { text } = &content[0] {
            assert!(text.starts_with("mock-"));
        } else {
            panic!("expected text content block");
        }
    }

    #[tokio::test]
    async fn complete_with_scripted_response() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = make_test_request();
        let scripted_completion = Completion {
            model: ModelId::new("test", "model"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "scripted response".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: Duration::from_millis(100),
            received_at: tm_types::Timestamp::EPOCH,
        };

        provider.script_response(&req, scripted_completion.clone());

        let result = provider.complete(req.clone()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), scripted_completion);
        assert_eq!(provider.call_log().len(), 1);
    }

    #[tokio::test]
    async fn complete_with_scripted_failure() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = make_test_request();
        let error = ProviderError::InvalidRequest("bad input".to_string());
        provider.script_failure(
            &req,
            ScriptedFailure {
                times: Some(1),
                error: error.clone(),
            },
        );

        let result = provider.complete(req.clone()).await;
        assert!(result.is_err());
        assert_eq!(provider.call_log().len(), 1);
    }

    #[tokio::test]
    async fn complete_with_failure_that_fires_multiple_times() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = make_test_request();
        let error = ProviderError::InvalidRequest("bad input".to_string());
        provider.script_failure(
            &req,
            ScriptedFailure {
                times: Some(2),
                error: error.clone(),
            },
        );

        let result1 = provider.complete(req.clone()).await;
        assert!(result1.is_err());

        let result2 = provider.complete(req.clone()).await;
        assert!(result2.is_err());

        let result3 = provider.complete(req.clone()).await;
        assert!(matches!(result3, Err(ProviderError::Unscripted(_))));
    }

    #[tokio::test]
    async fn complete_with_failure_that_fires_forever() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = make_test_request();
        let error = ProviderError::InvalidRequest("bad input".to_string());
        provider.script_failure(
            &req,
            ScriptedFailure {
                times: None,
                error: error.clone(),
            },
        );

        for _ in 0..5 {
            let result = provider.complete(req.clone()).await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn complete_with_exhausted_quota() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = make_test_request();
        let retry_after = Duration::from_secs(60);
        provider.script_exhausted(&req, retry_after);

        let result = provider.complete(req.clone()).await;
        assert!(result.is_err());

        match result.unwrap_err() {
            ProviderError::RateLimited {
                message,
                retry_after: Some(dur),
            } => {
                assert_eq!(message, "mock quota exhausted");
                assert_eq!(dur, Duration::from_secs(60));
            }
            _ => panic!("expected RateLimited error"),
        }
    }

    #[tokio::test]
    async fn complete_unscripted_error() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = make_test_request();

        let result = provider.complete(req.clone()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ProviderError::Unscripted(_)));
    }

    #[tokio::test]
    async fn complete_falls_back_to_the_default_response_when_no_hash_matches() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let default_completion = Completion {
            model: ModelId::new("test", "model"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "default reply".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: Duration::from_millis(0),
            received_at: tm_types::Timestamp::EPOCH,
        };
        provider.script_default_response(default_completion.clone());

        // Two different requests: an exact-hash script was never registered for either, so both
        // fall through to the same default — the whole point for a caller that cannot predict
        // the exact request shape.
        let mut other_req = make_test_request();
        other_req.max_tokens = 9999;

        let result1 = provider.complete(make_test_request()).await;
        let result2 = provider.complete(other_req).await;
        assert_eq!(result1.unwrap(), default_completion);
        assert_eq!(result2.unwrap(), default_completion);
    }

    #[tokio::test]
    async fn complete_prefers_an_exact_hash_script_over_the_default() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = make_test_request();
        let specific = provider.deterministic_completion(&req);
        provider.script_response(&req, specific.clone());
        provider.script_default_response(Completion {
            model: ModelId::new("test", "model"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "default reply".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: Duration::from_millis(0),
            received_at: tm_types::Timestamp::EPOCH,
        });

        let result = provider.complete(req).await.unwrap();
        assert_eq!(result, specific);
    }

    #[tokio::test]
    async fn complete_tracks_call_log() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = make_test_request();
        provider.script_response(&req, provider.deterministic_completion(&req));

        let _ = provider.complete(req.clone()).await;
        let _ = provider.complete(req.clone()).await;

        let log = provider.call_log();
        assert_eq!(log.len(), 2);
    }

    #[tokio::test]
    async fn embed_deterministic_output() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = EmbedRequest {
            inputs: vec!["hello".to_string(), "world".to_string()],
        };

        let result = provider.embed(req).await;
        assert!(result.is_ok());

        let embeddings = result.unwrap();
        assert_eq!(embeddings.vectors.len(), 2);
        assert_eq!(embeddings.vectors[0].len(), 8);
        assert_eq!(embeddings.vectors[1].len(), 8);
    }

    #[tokio::test]
    async fn embed_same_input_produces_same_vector() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req1 = EmbedRequest {
            inputs: vec!["test".to_string()],
        };

        let req2 = EmbedRequest {
            inputs: vec!["test".to_string()],
        };

        let result1 = provider.embed(req1).await.unwrap();
        let result2 = provider.embed(req2).await.unwrap();

        assert_eq!(result1.vectors[0], result2.vectors[0]);
    }

    #[tokio::test]
    async fn embed_different_inputs_produce_different_vectors() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = EmbedRequest {
            inputs: vec!["hello".to_string(), "world".to_string()],
        };

        let result = provider.embed(req).await.unwrap();

        assert_ne!(result.vectors[0], result.vectors[1]);
    }

    #[tokio::test]
    async fn embed_vectors_in_range() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = EmbedRequest {
            inputs: vec!["test".to_string()],
        };

        let result = provider.embed(req).await.unwrap();

        for &value in &result.vectors[0] {
            assert!(
                (-1.0..=1.0).contains(&value),
                "vector values should be in [-1.0, 1.0]"
            );
        }
    }

    #[tokio::test]
    async fn embed_usage_calculation() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        let req = EmbedRequest {
            inputs: vec!["hello".to_string()],
        };

        let result = provider.embed(req).await.unwrap();

        assert_eq!(result.usage.input_tokens, 5 / 4);
        assert_eq!(result.usage.output_tokens, 0);
    }

    #[tokio::test]
    async fn sequence_serves_completions_in_order() {
        let clock = make_test_clock();
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock);

        // Create three distinct completions with different text content.
        let completion1 = Completion {
            model: ModelId::new("test", "model"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "first response".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: Duration::from_millis(10),
            received_at: tm_types::Timestamp::EPOCH,
        };

        let completion2 = Completion {
            model: ModelId::new("test", "model"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "second response".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 20,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: Duration::from_millis(20),
            received_at: tm_types::Timestamp::EPOCH,
        };

        let completion3 = Completion {
            model: ModelId::new("test", "model"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "third response".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 30,
                output_tokens: 15,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: Duration::from_millis(30),
            received_at: tm_types::Timestamp::EPOCH,
        };

        provider.script_sequence(vec![
            completion1.clone(),
            completion2.clone(),
            completion3.clone(),
        ]);
        assert_eq!(provider.sequence_remaining(), 3);

        // Issue three non-matching requests (different max_tokens each time).
        let mut req1 = make_test_request();
        req1.max_tokens = 100;

        let mut req2 = make_test_request();
        req2.max_tokens = 200;

        let mut req3 = make_test_request();
        req3.max_tokens = 300;

        // First request gets first completion, regardless of request content.
        let result1 = provider.complete(req1).await;
        assert!(result1.is_ok());
        assert_eq!(result1.unwrap(), completion1);
        assert_eq!(provider.sequence_remaining(), 2);

        // Second request gets second completion.
        let result2 = provider.complete(req2).await;
        assert!(result2.is_ok());
        assert_eq!(result2.unwrap(), completion2);
        assert_eq!(provider.sequence_remaining(), 1);

        // Third request gets third completion.
        let result3 = provider.complete(req3).await;
        assert!(result3.is_ok());
        assert_eq!(result3.unwrap(), completion3);
        assert_eq!(provider.sequence_remaining(), 0);

        // Fourth request: sequence is exhausted, so it falls through to Unscripted.
        let result4 = provider.complete(make_test_request()).await;
        assert!(result4.is_err());
        assert!(matches!(result4, Err(ProviderError::Unscripted(_))));

        // Verify all four requests were logged.
        let log = provider.call_log();
        assert_eq!(log.len(), 4);
    }

    fn make_test_decide_request() -> DecideRequest {
        let mut questions = std::collections::BTreeMap::new();
        questions.insert(
            "family".to_string(),
            Question::Choice {
                instructions: "which tool family?".into(),
                options: vec![
                    crate::decide::OptionSpec { label: "fs".into() },
                    crate::decide::OptionSpec {
                        label: "shell".into(),
                    },
                ],
            },
        );
        DecideRequest {
            model: ModelId::new("mock", "decider"),
            state: "run `ls -la`".into(),
            questions,
        }
    }

    #[tokio::test]
    async fn decide_deterministic_response_is_stable_across_calls() {
        use crate::decide::DecisionProvider;

        let provider = MockDecisionProvider::new("mock-decider", ModelId::new("mock", "decider"));
        let req = make_test_decide_request();
        provider.script(&req, provider.deterministic_response(&req));

        let first = provider.decide(req.clone()).await.unwrap();
        let second = provider.decide(req.clone()).await.unwrap();
        assert_eq!(first, second, "same request should decide identically");
        assert_eq!(provider.call_log().len(), 2);
    }

    #[tokio::test]
    async fn decide_differs_for_a_different_request() {
        use crate::decide::DecisionProvider;

        let provider = MockDecisionProvider::new("mock-decider", ModelId::new("mock", "decider"));
        let req1 = make_test_decide_request();
        let mut req2 = make_test_decide_request();
        req2.state = "run `rm -rf /`".into();

        assert_ne!(
            hash_decide_request(&req1),
            hash_decide_request(&req2),
            "different requests should hash differently"
        );

        let response1 = DecideResponse {
            model: ModelId::new("mock", "decider"),
            answers: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(
                    "family".to_string(),
                    Answer {
                        value: AnswerValue::Choice("fs".into()),
                        confidence: 0.9,
                        probabilities: vec![1.0, 0.0],
                    },
                );
                m
            },
        };
        let response2 = DecideResponse {
            model: ModelId::new("mock", "decider"),
            answers: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(
                    "family".to_string(),
                    Answer {
                        value: AnswerValue::Choice("shell".into()),
                        confidence: 0.9,
                        probabilities: vec![0.0, 1.0],
                    },
                );
                m
            },
        };
        provider.script(&req1, response1.clone());
        provider.script(&req2, response2.clone());

        let decided1 = provider.decide(req1).await.unwrap();
        let decided2 = provider.decide(req2).await.unwrap();
        assert_eq!(decided1, response1);
        assert_eq!(decided2, response2);
        assert_ne!(
            decided1, decided2,
            "different requests should decide differently"
        );
    }

    #[tokio::test]
    async fn decide_unscripted_error() {
        use crate::decide::DecisionProvider;

        let provider = MockDecisionProvider::new("mock-decider", ModelId::new("mock", "decider"));
        let req = make_test_decide_request();

        let result = provider.decide(req).await;
        assert!(matches!(result, Err(ProviderError::Unscripted(_))));
    }

    #[tokio::test]
    async fn decide_with_scripted_failure() {
        use crate::decide::DecisionProvider;

        let provider = MockDecisionProvider::new("mock-decider", ModelId::new("mock", "decider"));
        let req = make_test_decide_request();
        provider.script_failure(
            &req,
            ProviderError::Unavailable("simulated outage".to_string()),
        );

        let result = provider.decide(req).await;
        assert!(matches!(result, Err(ProviderError::Unavailable(_))));
    }

    #[test]
    fn limits_are_reportable_and_overridable() {
        use crate::decide::DecisionProvider;

        let mut provider =
            MockDecisionProvider::new("mock-decider", ModelId::new("mock", "decider"));
        assert_eq!(provider.limits().max_options, 10);

        provider.set_limits(DecideLimits {
            max_context_tokens: 512,
            max_options: 4,
            kinds: vec![crate::decide::QuestionKind::Noul],
        });
        assert_eq!(provider.limits().max_options, 4);
    }
}
