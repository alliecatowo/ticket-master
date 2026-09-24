//! [`SystemOneProvider`]: an HTTP [`crate::decide::DecisionProvider`] speaking Vercel's
//! TypeSafe-compatible `/v1/systemone` wire contract (D-020), for the Jev gateway, TypeSafe
//! direct, and jevmlx-local decider backends.
//!
//! This is deliberately **not** an OpenAI-chat-shaped client and does not target
//! `mlx_lm.server`: Jev/TypeSafe never speak chat-completions, and Laya (the model behind it)
//! is not a generative LM — it answers typed [`crate::decide::Question`]s, not free text. See
//! `docs/decisions/D-020-system-one-decision-providers.md` and
//! `docs/vision/system-one-decisions.md` §4 for the shape this follows.
//!
//! Env vars read by [`SystemOneProvider::from_env`]:
//! - `AI_GATEWAY_API_KEY` (checked first) or `TYPESAFE_API_KEY` (fallback) — Bearer token. At
//!   least one is required; see [`SystemOneProvider::with_token`] for a caller-supplied token
//!   instead (e.g. an OIDC token minted elsewhere).
//! - `SYSTEMONE_BASE_URL` (optional, default [`DEFAULT_BASE_URL`]) — override for a
//!   self-hosted/jevmlx-local endpoint.
//!
//! `model` (which concrete decider model, e.g. `"typesafe-ai/jev"`) is not read from the
//! environment: it comes in as the `model: ModelId` constructor argument, exactly like
//! `providers/openai.rs::OpenAiProvider::from_env`.
//!
//! # Wire shape — verified where the docs show it, inferred where they don't
//!
//! `POST {base_url}/v1/systemone` with `{"model": "...", "state": "...", "questions": {...}}`
//! and `Authorization: Bearer <token>` is directly confirmed, including a full `noul`-question
//! request/response pair and a `choice`-question request's `criteria: {name: description}`
//! shape (its `department` example), via WebFetch against
//! <https://vercel.com/docs/ai-gateway/sdks-and-apis/typesafe>. That page does not show a
//! `choice`/`score` *response* example, or any `score`-question request, for `/v1/systemone`
//! itself. Those come from <https://vercel.com/docs/ai-gateway/modalities/evaluation> instead —
//! "the same capability without TypeSafe-specific naming" per that page's own words, but its
//! worked examples are for the *generic* `/v1/evaluate` endpoint (`boolean`/`probability`,
//! camelCase `inputTokens`/`providerMetadata`), which is already confirmed to differ from
//! `/v1/systemone` in the boolean/noul type tag and field name and in casing. So the
//! `choice`/`score` *answer* shapes (`choice`, `score`, `probabilities`) and the `score`
//! *request*'s `criteria` array are a reasoned inference from that page, not a
//! `/v1/systemone`-specific example — a wrong guess here surfaces as
//! [`ProviderError::MalformedResponse`], not silent bad data.
//! The per-question wire shape either way is **not** this crate's abstract [`Question`]/
//! [`Answer`] (`decide.rs`, owned by `d20-decider-trait-and-mock`) shape:
//!
//! - Request: `criteria` (not `options`/`levels`) and `instructions` on every question type,
//!   including `noul` (whose abstract counterpart calls the same text `statement`). `choice`'s
//!   `criteria` is a `{name: description}` map; `score`'s is an ordered array of level
//!   descriptions, lowest to highest.
//! - Response answers:
//!   - `noul`: `{"type": "noul", "noul": 0.98}` — a single P(true). Confirmed directly.
//!   - `choice`: `{"type": "choice", "choice": "billing", "probabilities": {"billing": 1, ...}}`
//!     — `probabilities` keyed by option name, not an array. Inferred (see above).
//!   - `score`: `{"type": "score", "score": 2.97, "probabilities": {"0": 0, "1": 0, ...}}` —
//!     `score` is an interpolated float; `probabilities` keyed by level index as a string.
//!     Inferred (see above).
//!
//! So this module translates both directions: [`to_wire_question`] builds the outbound
//! `criteria`/`instructions` shape from a [`Question`], and [`wire_answer_into_answer`] folds an
//! inbound [`WireAnswer`] back into this crate's [`Answer`] shape (`probabilities` reordered to
//! match the original question's option/level order, `confidence` taken as the probability mass
//! on the chosen value — [`Answer`] has no field for TypeSafe's raw interpolated `score` or
//! `noul` probability beyond that, so a caller wanting the exact wire number needs the raw JSON,
//! not this typed path). `OptionSpec` (decide.rs) carries no per-option description, so an
//! option's own label doubles as its `criteria` description — a known simplification, not a
//! documented TypeSafe requirement.
//!
//! # IMPL: transport is injectable for tests
//!
//! The acceptance test for this module proves the outbound request body and inbound response
//! parsing without any real network call, so the actual `reqwest` send is behind a small private
//! [`Transport`] trait rather than called directly — a fake [`Transport`] in the test module
//! captures the request and returns a canned response.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_auth::EnvApiKey;

use crate::decide::{
    Answer, AnswerValue, DecideLimits, DecideRequest, DecideResponse, DecisionProvider, Question,
    QuestionId, QuestionKind,
};
use crate::types::{ModelId, ProviderError};

/// Vercel AI Gateway's TypeSafe-compatible base URL. `SYSTEMONE_BASE_URL` overrides this for a
/// jevmlx-local or self-hosted endpoint; the `/v1/systemone` path is always appended to whatever
/// base URL is in effect.
pub const DEFAULT_BASE_URL: &str = "https://ai-gateway.vercel.sh/typesafe";

/// The provider slug this backend registers under (see `providers/registry.rs`).
pub const PROVIDER_ID: &str = "systemone";

/// One outbound POST, abstracted so tests can inject a fake without a real network call. Takes
/// the already-serialized JSON body and returns the raw response status and body bytes on any
/// completed HTTP exchange; a transport-level failure (connect error, timeout) is a
/// [`ProviderError`] directly.
#[async_trait]
trait Transport: Send + Sync {
    async fn post(
        &self,
        url: &str,
        token: &str,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), ProviderError>;
}

/// The real [`Transport`]: a `reqwest::Client` POSTing with a Bearer token.
struct HttpTransport {
    client: reqwest::Client,
}

#[async_trait]
impl Transport for HttpTransport {
    async fn post(
        &self,
        url: &str,
        token: &str,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), ProviderError> {
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout(e.to_string())
                } else {
                    ProviderError::Unavailable(e.to_string())
                }
            })?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|e| {
            ProviderError::Unavailable(format!("failed to read response body: {e}"))
        })?;
        Ok((status, bytes.to_vec()))
    }
}

/// TypeSafe's own per-question wire shape (`criteria`/`instructions`), distinct from this
/// crate's abstract [`Question`] (`options`/`levels`). See the module docs' "Wire shape"
/// section for the evidence and [`to_wire_question`] for the conversion.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireQuestion<'a> {
    Noul {
        instructions: &'a str,
    },
    Choice {
        instructions: &'a str,
        criteria: BTreeMap<&'a str, &'a str>,
    },
    Score {
        instructions: &'a str,
        criteria: Vec<&'a str>,
    },
}

/// Build the outbound TypeSafe wire shape for one [`Question`]. See the module docs.
fn to_wire_question(question: &Question) -> WireQuestion<'_> {
    match question {
        Question::Noul { statement } => WireQuestion::Noul {
            instructions: statement,
        },
        Question::Choice {
            instructions,
            options,
        } => WireQuestion::Choice {
            instructions,
            // No per-option description in `OptionSpec` — the label doubles as its own
            // description. See the module docs.
            criteria: options
                .iter()
                .map(|o| (o.label.as_str(), o.label.as_str()))
                .collect(),
        },
        Question::Score {
            instructions,
            levels,
        } => WireQuestion::Score {
            instructions,
            criteria: levels.iter().map(|l| l.label.as_str()).collect(),
        },
    }
}

/// The outbound `/v1/systemone` request body: [`DecideRequest`]'s three fields, with `model`
/// flattened to the plain wire string TypeSafe expects and each question translated through
/// [`to_wire_question`].
#[derive(Serialize)]
struct WireDecideRequest<'a> {
    model: &'a str,
    state: &'a str,
    questions: BTreeMap<&'a QuestionId, WireQuestion<'a>>,
}

/// TypeSafe's own per-question answer wire shape. See the module docs' "Wire shape" section for
/// the evidence and [`wire_answer_into_answer`] for the conversion back to [`Answer`].
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireAnswer {
    Noul {
        noul: f32,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f32>,
    },
    // No `score: f32` field: TypeSafe's interpolated `score` has nowhere to go on `Answer`
    // (`decide.rs`, not owned by this task), so only `probabilities` is read; `score` is
    // present on the wire but simply never deserialized into this variant.
    Score {
        probabilities: BTreeMap<String, f32>,
    },
}

/// Convert one [`WireAnswer`] into this crate's [`Answer`], reordering `probabilities` to match
/// the original [`Question`]'s option/level order where the wire keys by name/index instead.
/// `question` is `None` when the response names a question id the request never asked (a
/// backend bug, not something to panic on) — falls back to the wire's own key order.
fn wire_answer_into_answer(wire: WireAnswer, question: Option<&Question>) -> Answer {
    match wire {
        WireAnswer::Noul { noul } => Answer {
            value: AnswerValue::Noul(noul >= 0.5),
            probabilities: vec![],
            confidence: if noul >= 0.5 { noul } else { 1.0 - noul },
        },
        WireAnswer::Choice {
            choice,
            probabilities,
        } => {
            let order: Vec<String> = match question {
                Some(Question::Choice { options, .. }) => {
                    options.iter().map(|o| o.label.clone()).collect()
                }
                _ => probabilities.keys().cloned().collect(),
            };
            let ordered = order
                .iter()
                .map(|label| *probabilities.get(label).unwrap_or(&0.0))
                .collect();
            let confidence = *probabilities.get(&choice).unwrap_or(&0.0);
            Answer {
                value: AnswerValue::Choice(choice),
                probabilities: ordered,
                confidence,
            }
        }
        WireAnswer::Score { probabilities } => {
            let levels: Vec<String> = match question {
                Some(Question::Score { levels, .. }) => {
                    levels.iter().map(|l| l.label.clone()).collect()
                }
                _ => Vec::new(),
            };
            let len = levels.len().max(
                probabilities
                    .keys()
                    .filter_map(|k| k.parse::<usize>().ok())
                    .map(|i| i + 1)
                    .max()
                    .unwrap_or(0),
            );
            let mut ordered = vec![0.0f32; len];
            for (key, value) in &probabilities {
                if let Ok(idx) = key.parse::<usize>() {
                    if let Some(slot) = ordered.get_mut(idx) {
                        *slot = *value;
                    }
                }
            }
            // Seeded at 0.0, not `f32::MIN`: an empty `probabilities` map must not leave
            // `confidence` negative — [`Answer::confidence`] is documented as `[0.0, 1.0]`.
            let (best_idx, best_p) =
                ordered
                    .iter()
                    .copied()
                    .enumerate()
                    .fold(
                        (0usize, 0.0f32),
                        |acc, (i, p)| if p > acc.1 { (i, p) } else { acc },
                    );
            let label = levels
                .get(best_idx)
                .cloned()
                .unwrap_or_else(|| best_idx.to_string());
            Answer {
                value: AnswerValue::Score(label),
                probabilities: ordered,
                confidence: best_p,
            }
        }
    }
}

/// The inbound `/v1/systemone` success body. `usage`/`provider_metadata` are on the wire but not
/// declared here at all: serde ignores unrecognized fields on a struct with no
/// `#[serde(deny_unknown_fields)]`, and no [`DecideResponse`] field carries them yet, so there is
/// nothing to assign a read field's value to.
#[derive(Deserialize)]
struct WireDecideResponse {
    #[serde(default)]
    model: Option<String>,
    answers: BTreeMap<QuestionId, WireAnswer>,
}

/// The inbound `/v1/systemone` error body: `{"message": "...", "error_type": "..."}`, confirmed
/// against the real docs (see the module docs).
#[derive(Deserialize)]
struct WireErrorBody {
    message: String,
    #[serde(default)]
    error_type: Option<String>,
}

/// Maps a non-2xx `/v1/systemone` response to a typed [`ProviderError`], the same
/// status-code-first classification `providers/compat.rs::classify_status` uses for the
/// completion providers.
fn classify_error(status: u16, body: &[u8]) -> ProviderError {
    let parsed: Option<WireErrorBody> = serde_json::from_slice(body).ok();
    let message = match &parsed {
        Some(WireErrorBody {
            message,
            error_type: Some(error_type),
        }) => format!("{message} ({error_type})"),
        Some(WireErrorBody { message, .. }) => message.clone(),
        None => String::from_utf8_lossy(body).to_string(),
    };
    match status {
        401 | 403 => ProviderError::AuthFailed(message),
        429 => ProviderError::RateLimited {
            message,
            retry_after: None,
        },
        400..=499 => ProviderError::InvalidRequest(message),
        500..=599 => ProviderError::Unavailable(message),
        _ => ProviderError::Unavailable(message),
    }
}

/// Reads the Bearer token from `AI_GATEWAY_API_KEY` first, then `TYPESAFE_API_KEY`. A caller
/// that already has a token from elsewhere (e.g. a minted Vercel OIDC token) should use
/// [`SystemOneProvider::with_token`] instead of this env-only path.
fn resolve_token_from_env() -> Result<String, ProviderError> {
    if let Ok(cred) = EnvApiKey::new("AI_GATEWAY_API_KEY").resolve() {
        return Ok(cred.expose_secret().to_string());
    }
    if let Ok(cred) = EnvApiKey::new("TYPESAFE_API_KEY").resolve() {
        return Ok(cred.expose_secret().to_string());
    }
    Err(ProviderError::AuthFailed(
        "AI_GATEWAY_API_KEY or TYPESAFE_API_KEY is not set".to_string(),
    ))
}

/// An HTTP [`DecisionProvider`] against the `/v1/systemone` wire contract. See the module docs.
pub struct SystemOneProvider {
    base_url: String,
    token: String,
    default_model: String,
    transport: Arc<dyn Transport>,
}

impl SystemOneProvider {
    /// Build a provider for `model`, reading the Bearer token and base URL from the environment.
    pub fn from_env(model: ModelId) -> Result<Self, ProviderError> {
        Self::from_env_with_base_url(model, None)
    }

    /// [`SystemOneProvider::from_env`], but with a per-config `base_url` override — the token
    /// still comes from `AI_GATEWAY_API_KEY`/`TYPESAFE_API_KEY` either way. Exists so a caller
    /// that wants the environment's token but a config-supplied endpoint (e.g.
    /// `d20-decider-config-selection`'s role config naming a jevmlx-local URL while the token
    /// stays in the environment) doesn't have to give up env-var token resolution just to set
    /// `base_url`, the way routing both through [`SystemOneProvider::with_token`] alone would.
    pub fn from_env_with_base_url(
        model: ModelId,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        let token = resolve_token_from_env()?;
        Self::with_token(model, token, base_url)
    }

    /// Build a provider for `model` with a caller-supplied token — e.g. one already resolved by
    /// `d20-decider-config-selection`'s role config, or a Vercel OIDC token minted by the
    /// caller rather than read from `AI_GATEWAY_API_KEY`/`TYPESAFE_API_KEY`. `base_url` reads
    /// `SYSTEMONE_BASE_URL` when `None`, falling back to [`DEFAULT_BASE_URL`] — the same
    /// precedence [`SystemOneProvider::from_env`] uses, so a caller only needs to pass this
    /// explicitly for a per-config override (e.g. a jevmlx-local endpoint named in
    /// `providers.toml` rather than the environment).
    pub fn with_token(
        model: ModelId,
        token: String,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url
            .or_else(|| std::env::var("SYSTEMONE_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let base_url = base_url.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| ProviderError::Unavailable(format!("failed to build HTTP client: {e}")))?;
        Ok(SystemOneProvider {
            base_url,
            token,
            default_model: model.model,
            transport: Arc::new(HttpTransport { client }),
        })
    }

    fn url(&self) -> String {
        format!("{}/v1/systemone", self.base_url)
    }
}

#[async_trait]
impl DecisionProvider for SystemOneProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn limits(&self) -> DecideLimits {
        DecideLimits {
            // No published hard context limit as of D-020's evidence; 8k tokens is a
            // conservative floor for a bounded `state` (triage/routing questions, not full
            // transcripts) rather than a documented Jev/TypeSafe number.
            max_context_tokens: 8_000,
            max_options: 255,
            kinds: vec![
                QuestionKind::Choice,
                QuestionKind::Score,
                QuestionKind::Noul,
            ],
        }
    }

    async fn decide(&self, req: DecideRequest) -> Result<DecideResponse, ProviderError> {
        let model = if req.model.model.is_empty() {
            self.default_model.as_str()
        } else {
            req.model.model.as_str()
        };
        let questions: BTreeMap<&QuestionId, WireQuestion<'_>> = req
            .questions
            .iter()
            .map(|(qid, question)| (qid, to_wire_question(question)))
            .collect();
        let wire_request = WireDecideRequest {
            model,
            state: &req.state,
            questions,
        };
        let body = serde_json::to_vec(&wire_request)
            .map_err(|e| ProviderError::InvalidRequest(format!("failed to encode request: {e}")))?;

        let (status, response_body) = self.transport.post(&self.url(), &self.token, body).await?;

        if (200..300).contains(&status) {
            let wire_response: WireDecideResponse = serde_json::from_slice(&response_body)
                .map_err(|e| {
                    ProviderError::MalformedResponse(format!(
                        "systemone response: {e} (body: {})",
                        String::from_utf8_lossy(&response_body)
                    ))
                })?;
            let answers = wire_response
                .answers
                .into_iter()
                .map(|(qid, wire_answer)| {
                    let question = req.questions.get(&qid);
                    let answer = wire_answer_into_answer(wire_answer, question);
                    (qid, answer)
                })
                .collect();
            Ok(DecideResponse {
                model: ModelId::new(
                    PROVIDER_ID,
                    wire_response.model.unwrap_or_else(|| model.to_string()),
                ),
                answers,
            })
        } else {
            Err(classify_error(status, &response_body))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::decide::{LevelSpec, OptionSpec};

    /// A fake [`Transport`] that records the request it was sent and returns a canned response.
    struct FakeTransport {
        sent: Mutex<Option<(String, String, Vec<u8>)>>,
        status: u16,
        response_body: Vec<u8>,
    }

    impl FakeTransport {
        fn new(status: u16, response_body: impl Into<Vec<u8>>) -> Self {
            FakeTransport {
                sent: Mutex::new(None),
                status,
                response_body: response_body.into(),
            }
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn post(
            &self,
            url: &str,
            token: &str,
            body: Vec<u8>,
        ) -> Result<(u16, Vec<u8>), ProviderError> {
            *self.sent.lock().unwrap() = Some((url.to_string(), token.to_string(), body));
            Ok((self.status, self.response_body.clone()))
        }
    }

    /// Builds a [`SystemOneProvider`] straight from its (private) fields — this test module is
    /// a child of `systemone`, so it can see them — instead of a `#[cfg(test)]` constructor on
    /// the production `impl SystemOneProvider` block. Keeping test-only construction entirely
    /// inside `mod tests` (at the bottom of the file) avoids the hygiene `TestRegionTracker`
    /// one-way latch: a `#[cfg(test)]` item above `decide()` would silently exempt everything
    /// below it from the hygiene checks (see this repo's CLAUDE.md).
    fn test_provider(
        base_url: &str,
        token: &str,
        default_model: &str,
        transport: Arc<dyn Transport>,
    ) -> SystemOneProvider {
        SystemOneProvider {
            base_url: base_url.to_string(),
            token: token.to_string(),
            default_model: default_model.to_string(),
            transport,
        }
    }

    fn choice_request() -> DecideRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            "route".to_string(),
            Question::Choice {
                instructions: "route this support ticket".into(),
                options: vec![
                    OptionSpec {
                        label: "billing".into(),
                    },
                    OptionSpec {
                        label: "technical".into(),
                    },
                ],
            },
        );
        DecideRequest {
            model: ModelId::new("systemone", "typesafe-ai/jev"),
            state: "my card was charged twice for one order".into(),
            questions,
        }
    }

    fn noul_request() -> DecideRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            "refund".to_string(),
            Question::Noul {
                statement: "the customer is asking for money back".into(),
            },
        );
        DecideRequest {
            model: ModelId::new("systemone", "typesafe-ai/jev"),
            state: "I was charged twice for my subscription.".into(),
            questions,
        }
    }

    fn score_request() -> DecideRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            "quality".to_string(),
            Question::Score {
                instructions: "rate the quality of this pull request".into(),
                levels: vec![
                    LevelSpec {
                        label: "poor".into(),
                    },
                    LevelSpec {
                        label: "fair".into(),
                    },
                    LevelSpec {
                        label: "good".into(),
                    },
                    LevelSpec {
                        label: "excellent".into(),
                    },
                ],
            },
        );
        DecideRequest {
            model: ModelId::new("systemone", "typesafe-ai/jev"),
            state: "the PR adds tests, updates docs, and has a clear description".into(),
            questions,
        }
    }

    #[tokio::test]
    async fn decide_sends_the_documented_request_body() {
        // Real response shape, verified via WebFetch against
        // https://vercel.com/docs/ai-gateway/modalities/evaluation's choice example.
        let response_body = br#"{"model":"typesafe-ai/jev","answers":{"route":{"type":"choice","choice":"billing","probabilities":{"billing":1,"technical":0}}},"usage":{"input_tokens":275,"output_tokens":20},"provider_metadata":{}}"#;
        let transport = Arc::new(FakeTransport::new(200, response_body.to_vec()));
        let provider = test_provider(
            "https://fake.example/typesafe",
            "test-token",
            "typesafe-ai/jev",
            transport.clone(),
        );

        provider.decide(choice_request()).await.unwrap();

        let (url, token, body) = transport.sent.lock().unwrap().clone().unwrap();
        assert_eq!(url, "https://fake.example/typesafe/v1/systemone");
        assert_eq!(token, "test-token");

        let sent: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(sent["model"], "typesafe-ai/jev");
        assert_eq!(sent["state"], "my card was charged twice for one order");
        // Exactly {model, state, questions}, per the TypeSafe wire contract — no extra fields.
        assert_eq!(sent.as_object().unwrap().len(), 3);
        // The nested question uses TypeSafe's own field names, not `Question`'s.
        let wire_question = &sent["questions"]["route"];
        assert_eq!(wire_question["type"], "choice");
        assert_eq!(wire_question["instructions"], "route this support ticket");
        assert_eq!(wire_question["criteria"]["billing"], "billing");
        assert_eq!(wire_question["criteria"]["technical"], "technical");
    }

    #[tokio::test]
    async fn decide_sends_noul_questions_with_instructions_not_statement() {
        let response_body = br#"{"answers":{"refund":{"type":"noul","noul":0.98}}}"#;
        let transport = Arc::new(FakeTransport::new(200, response_body.to_vec()));
        let provider = test_provider(
            "https://fake.example/typesafe",
            "test-token",
            "typesafe-ai/jev",
            transport.clone(),
        );

        provider.decide(noul_request()).await.unwrap();

        let (_, _, body) = transport.sent.lock().unwrap().clone().unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let wire_question = &sent["questions"]["refund"];
        assert_eq!(wire_question["type"], "noul");
        assert_eq!(
            wire_question["instructions"],
            "the customer is asking for money back"
        );
        assert!(wire_question.get("statement").is_none());
    }

    #[tokio::test]
    async fn decide_parses_a_choice_response_into_decide_response() {
        let response_body = br#"{"model":"typesafe-ai/jev","answers":{"route":{"type":"choice","choice":"billing","probabilities":{"billing":1,"technical":0}}}}"#;
        let transport = Arc::new(FakeTransport::new(200, response_body.to_vec()));
        let provider = test_provider(
            "https://fake.example/typesafe",
            "test-token",
            "typesafe-ai/jev",
            transport,
        );

        let response = provider.decide(choice_request()).await.unwrap();

        assert_eq!(response.model, ModelId::new("systemone", "typesafe-ai/jev"));
        let answer = response.answers.get("route").expect("answer for 'route'");
        assert_eq!(answer.value, AnswerValue::Choice("billing".to_string()));
        // Reordered to match the request's option order: billing, technical.
        assert_eq!(answer.probabilities, vec![1.0, 0.0]);
        assert!((answer.confidence - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn decide_parses_a_noul_response_into_decide_response() {
        let response_body = br#"{"answers":{"refund":{"type":"noul","noul":0.98}}}"#;
        let transport = Arc::new(FakeTransport::new(200, response_body.to_vec()));
        let provider = test_provider(
            "https://fake.example/typesafe",
            "test-token",
            "typesafe-ai/jev",
            transport,
        );

        let response = provider.decide(noul_request()).await.unwrap();

        let answer = response.answers.get("refund").expect("answer for 'refund'");
        assert_eq!(answer.value, AnswerValue::Noul(true));
        assert!((answer.confidence - 0.98).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn decide_parses_a_score_response_into_decide_response() {
        let response_body = br#"{"answers":{"quality":{"type":"score","score":2.97,"probabilities":{"0":0,"1":0,"2":0.02,"3":0.98}}}}"#;
        let transport = Arc::new(FakeTransport::new(200, response_body.to_vec()));
        let provider = test_provider(
            "https://fake.example/typesafe",
            "test-token",
            "typesafe-ai/jev",
            transport,
        );

        let response = provider.decide(score_request()).await.unwrap();

        let answer = response
            .answers
            .get("quality")
            .expect("answer for 'quality'");
        // Index 3 ("excellent") has the highest probability (0.98).
        assert_eq!(answer.value, AnswerValue::Score("excellent".to_string()));
        assert_eq!(answer.probabilities, vec![0.0, 0.0, 0.02, 0.98]);
        assert!((answer.confidence - 0.98).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn decide_maps_an_error_body_to_a_typed_provider_error() {
        let error_body = br#"{"message":"model is overloaded","error_type":"rate_limited"}"#;
        let transport = Arc::new(FakeTransport::new(429, error_body.to_vec()));
        let provider = test_provider(
            "https://fake.example/typesafe",
            "test-token",
            "typesafe-ai/jev",
            transport,
        );

        let err = provider.decide(choice_request()).await.unwrap_err();
        match err {
            ProviderError::RateLimited { message, .. } => {
                assert!(message.contains("model is overloaded"));
                assert!(message.contains("rate_limited"));
            }
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    #[tokio::test]
    async fn decide_maps_an_auth_error_status_to_auth_failed() {
        let error_body = br#"{"message":"invalid bearer token","error_type":"unauthorized"}"#;
        let transport = Arc::new(FakeTransport::new(401, error_body.to_vec()));
        let provider = test_provider(
            "https://fake.example/typesafe",
            "test-token",
            "typesafe-ai/jev",
            transport,
        );

        let err = provider.decide(choice_request()).await.unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed(_)));
    }

    #[test]
    fn id_and_limits_report_the_documented_shape() {
        let provider = test_provider(
            "https://fake.example/typesafe",
            "test-token",
            "typesafe-ai/jev",
            Arc::new(FakeTransport::new(200, b"{}".to_vec())),
        );
        assert_eq!(provider.id(), "systemone");
        let limits = provider.limits();
        assert_eq!(limits.max_options, 255);
        assert_eq!(limits.kinds.len(), 3);
    }

    #[test]
    fn with_token_honors_a_caller_supplied_base_url_and_trims_a_trailing_slash() {
        let provider = SystemOneProvider::with_token(
            ModelId::new("systemone", "typesafe-ai/jev"),
            "test-token".to_string(),
            Some("https://jevmlx.local/".to_string()),
        )
        .expect("with_token builds with no env vars set");
        assert_eq!(provider.url(), "https://jevmlx.local/v1/systemone");
    }
}
