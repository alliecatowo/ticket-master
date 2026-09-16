//! [`Fabric`]: the registry of [`Provider`] implementations that ties [`crate::route`] and
//! [`crate::state`] to actual (or mocked) calls.
//!
//! `execute()` is the only place in this crate that both makes a routing decision *and* performs
//! I/O: it calls [`crate::route::route`] (pure), invokes the chosen [`Provider`], and folds the
//! outcome back into [`crate::state::FabricState`] via [`crate::state::FabricEvent`]. It also
//! turns each step into a structured [`FabricRecord`] the caller can adapt into workspace events
//! (`provider.selected` / `provider.exhausted` / `provider.degraded` / `provider.recovered`).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use tm_types::{Clock, Result as TmResult, Role, Timestamp, TmError};

use crate::role_config::RoleTable;
use crate::route::{Need, RouteDecision};
use crate::state::{BreakerState, FabricEvent, FabricState};
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// A backend the fabric can route to. Implemented by [`crate::anthropic::AnthropicProvider`] and
/// [`crate::mock::MockProvider`].
#[async_trait]
pub trait Provider: Send + Sync {
    /// The provider slug this implementation registers under, e.g. `"anthropic"`.
    fn id(&self) -> &str;

    /// Generate a completion.
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError>;

    /// Generate embeddings.
    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError>;
}

/// One structured record of a fabric decision, for the caller to turn into a workspace event.
#[derive(Debug, Clone, PartialEq)]
pub enum FabricRecord {
    /// A role was routed to its primary candidate.
    Selected {
        /// The role that was routed.
        role: Role,
        /// The candidate chosen.
        candidate: ModelId,
    },
    /// A role was routed to a lower tier than its primary candidate.
    Degraded {
        /// The role that was routed.
        role: Role,
        /// The candidate chosen instead of the primary.
        candidate: ModelId,
        /// Why the primary was skipped.
        reason: String,
    },
    /// No candidate could serve the role at all.
    Exhausted {
        /// The role that could not be routed.
        role: Role,
    },
    /// A candidate's circuit breaker closed again after a successful probe.
    Recovered {
        /// The candidate that recovered.
        candidate: ModelId,
    },
}

/// A registered provider plus the [`crate::role_config::RoleCandidate`] metadata it was
/// registered with, so [`Fabric`] can build breaker/limit defaults when first seeing a candidate.
pub struct ProviderRecord {
    /// The provider implementation.
    pub provider: Arc<dyn Provider>,
}

/// The provider registry: owns every [`Provider`] implementation, the [`RoleTable`] mapping
/// roles to candidates, and the live [`FabricState`].
pub struct Fabric {
    providers: RwLock<BTreeMap<String, ProviderRecord>>,
    table: RwLock<RoleTable>,
    state: RwLock<FabricState>,
    clock: Arc<dyn Clock>,
    failure_threshold: u32,
    breaker_window: std::time::Duration,
    breaker_cooldown: std::time::Duration,
    ewma_alpha: f64,
    records: RwLock<Vec<FabricRecord>>,
}

impl Fabric {
    /// Build a fabric with the given role table and injected clock, and default breaker/EWMA
    /// tuning (5 failures per 60s window trips the breaker, 30s cooldown, EWMA alpha 0.3).
    pub fn new(table: RoleTable, clock: Arc<dyn Clock>) -> Self {
        Fabric {
            providers: RwLock::new(BTreeMap::new()),
            table: RwLock::new(table),
            state: RwLock::new(FabricState::new()),
            clock,
            failure_threshold: 5,
            breaker_window: std::time::Duration::from_secs(60),
            breaker_cooldown: std::time::Duration::from_secs(30),
            ewma_alpha: 0.3,
            records: RwLock::new(Vec::new()),
        }
    }

    /// Register a [`Provider`] implementation under its own [`Provider::id`].
    pub fn register_provider(&self, provider: Arc<dyn Provider>) {
        let id = provider.id().to_string();
        let now = self.clock.now();

        {
            let table = self.table.read();
            let mut state = self.state.write();
            for role in Role::ALL {
                for candidate in table.candidates_for(role) {
                    if candidate.provider == id {
                        let key = ModelId::new(candidate.provider.clone(), candidate.model.clone());
                        state.register(
                            key,
                            now,
                            self.failure_threshold,
                            self.breaker_window,
                            self.breaker_cooldown,
                            self.ewma_alpha,
                        );
                    }
                }
            }
        }

        self.providers
            .write()
            .insert(id, ProviderRecord { provider });
    }

    // When `role`'s candidates are all `Exhausted`, distinguish "every provider is registered
    // but out of quota/breaker-tripped" from "a candidate names a provider that was never
    // registered at all" (a caller configuration bug), so `execute`'s error is actionable.
    fn first_unregistered_provider(&self, role: Role) -> Option<String> {
        let table = self.table.read();
        let providers = self.providers.read();
        table
            .candidates_for(role)
            .iter()
            .map(|c| &c.provider)
            .find(|p| !providers.contains_key(p.as_str()))
            .cloned()
    }

    /// The pure routing decision for `role` right now, with no side effects beyond rolling
    /// windows forward (which is itself a pure function of `now`, done for accuracy of reads).
    pub fn route(&self, role: Role, need: &Need, now: Timestamp) -> RouteDecision {
        let table = self.table.read();
        let state = self.state.read();
        crate::route::route(&table, &state, role, need, now)
    }

    /// Route `role`, call the chosen provider, record the outcome, and return the completion.
    pub async fn execute(&self, role: Role, req: CompletionRequest) -> TmResult<Completion> {
        let now = self.clock.now();
        self.state.write().roll_windows(now);

        let need = Need {
            tolerance: role.default_tolerance(),
            estimated_tokens: req.max_tokens,
            max_cost_micros: None,
        };

        let decision = self.route(role, &need, now);

        let (candidate_key, record, was_half_open) = match &decision {
            RouteDecision::Use(model_id) => {
                let half_open = self.is_half_open(model_id);
                (
                    model_id.clone(),
                    FabricRecord::Selected {
                        role,
                        candidate: model_id.clone(),
                    },
                    half_open,
                )
            }
            RouteDecision::Degrade(model_id, reason) => {
                let half_open = self.is_half_open(model_id);
                (
                    model_id.clone(),
                    FabricRecord::Degraded {
                        role,
                        candidate: model_id.clone(),
                        reason: reason.clone(),
                    },
                    half_open,
                )
            }
            RouteDecision::Wait(until) => {
                *self.records.write() = vec![FabricRecord::Exhausted { role }];
                return Err(TmError::Provider(format!(
                    "no candidate available for role {} until {}",
                    role.as_str(),
                    until.to_rfc3339()
                )));
            }
            RouteDecision::Exhausted => {
                *self.records.write() = vec![FabricRecord::Exhausted { role }];
                if let Some(unregistered) = self.first_unregistered_provider(role) {
                    return Err(TmError::Provider(format!(
                        "no candidate can serve role {}: provider not registered: {}",
                        role.as_str(),
                        unregistered
                    )));
                }
                return Err(TmError::Provider(format!(
                    "no candidate can serve role {}",
                    role.as_str()
                )));
            }
        };

        *self.records.write() = vec![record];

        let provider = {
            let providers = self.providers.read();
            providers
                .get(&candidate_key.provider)
                .map(|r| Arc::clone(&r.provider))
        };
        let provider = match provider {
            Some(p) => p,
            None => {
                return Err(TmError::Provider(format!(
                    "provider not registered: {}",
                    candidate_key.provider
                )));
            }
        };

        self.state.write().apply(
            FabricEvent::RequestStarted {
                candidate: candidate_key.clone(),
            },
            now,
        );

        let call_result = provider.complete(req).await;
        let finished_at = self.clock.now();
        let latency = std::time::Duration::from_millis(finished_at.millis_since(now).max(0) as u64);

        match call_result {
            Ok(completion) => {
                let cost_micros = {
                    let table = self.table.read();
                    table
                        .candidates_for(role)
                        .iter()
                        .find(|c| {
                            c.provider == candidate_key.provider && c.model == candidate_key.model
                        })
                        .and_then(|c| c.price)
                        .map(|price| {
                            completion.usage.input_tokens as u64 * price.input_micros_per_token
                                + completion.usage.output_tokens as u64
                                    * price.output_micros_per_token
                        })
                };

                self.state.write().apply(
                    FabricEvent::RequestSucceeded {
                        candidate: candidate_key.clone(),
                        latency,
                        usage: completion.usage,
                        cost_micros,
                    },
                    finished_at,
                );

                if was_half_open {
                    self.records.write().push(FabricRecord::Recovered {
                        candidate: candidate_key,
                    });
                }

                Ok(completion)
            }
            Err(err) => {
                let counts_against_breaker = err.is_retryable();
                self.state.write().apply(
                    FabricEvent::RequestFailed {
                        candidate: candidate_key,
                        counts_against_breaker,
                    },
                    finished_at,
                );
                Err(TmError::Provider(err.to_string()))
            }
        }
    }

    /// Whether `key`'s breaker is currently `HalfOpen`, i.e. a successful call would count as
    /// recovery rather than routine success.
    fn is_half_open(&self, key: &ModelId) -> bool {
        matches!(
            self.state.read().candidate(key).map(|c| c.breaker.state),
            Some(BreakerState::HalfOpen)
        )
    }

    /// The records emitted by the most recent [`Fabric::execute`] call's routing decision, for
    /// callers that want them without threading a channel through. Cleared and repopulated on
    /// every `execute`/`route` call.
    pub fn last_records(&self) -> Vec<FabricRecord> {
        self.records.read().clone()
    }

    /// A snapshot of current fabric state, for diagnostics/CLI reporting.
    pub fn state_snapshot(&self) -> FabricState {
        self.state.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::types::{Message, MessageRole, Usage};
    use std::sync::Arc;
    use tm_types::FixedClock;

    fn table_with_candidates(role_key: &str, candidates_toml: &str) -> RoleTable {
        let toml_str = format!("[{role_key}]\ncandidates = [{candidates_toml}]\n");
        RoleTable::parse(&toml_str).expect("test table parses")
    }

    fn req() -> CompletionRequest {
        CompletionRequest {
            system: None,
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![crate::types::ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            max_tokens: 100,
            temperature: None,
            stop_sequences: vec![],
            stream: false,
            n: 1,
        }
    }

    fn completion(model: ModelId, clock: &Arc<FixedClock>) -> Completion {
        Completion {
            model,
            candidates: vec![],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(0),
            received_at: clock.now(),
        }
    }

    #[tokio::test]
    async fn execute_selects_primary_and_records_selected() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);

        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock.clone() as Arc<dyn Clock>,
        ));
        let request = req();
        provider.script_response(&request, completion(ModelId::new("mock", "m1"), &clock));
        fabric.register_provider(provider);

        let out = fabric
            .execute(Role::CoderFast, req())
            .await
            .expect("call succeeds");
        assert_eq!(out.model, ModelId::new("mock", "m1"));

        let records = fabric.last_records();
        assert_eq!(
            records,
            vec![FabricRecord::Selected {
                role: Role::CoderFast,
                candidate: ModelId::new("mock", "m1")
            }]
        );
    }

    #[tokio::test]
    async fn execute_falls_back_to_degraded_candidate_and_records_reason() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "primary", max_concurrency = 1 },
               { provider = "mock", model = "fallback", max_concurrency = 1, degraded_ok = true }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);

        let primary = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "primary"),
            clock.clone() as Arc<dyn Clock>,
        ));
        // Unscripted: leave primary un-callable; it should never be invoked once its window is exhausted.
        fabric.register_provider(primary);

        // Saturate the primary's single concurrency slot by driving its live_concurrency up via a
        // manual RequestStarted event, so route() sees it blocked and degrades to the fallback.
        {
            let mut state = fabric.state.write();
            state.apply(
                FabricEvent::RequestStarted {
                    candidate: ModelId::new("mock", "primary"),
                },
                clock.now(),
            );
        }

        let fallback = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "fallback"),
            clock.clone() as Arc<dyn Clock>,
        ));
        let request = req();
        fallback.script_response(
            &request,
            completion(ModelId::new("mock", "fallback"), &clock),
        );
        // Re-register under the same provider id is fine; MockProvider for "fallback" model routes
        // through provider id "mock" too, so swap the registration to the fallback-capable instance.
        fabric.register_provider(fallback);

        let out = fabric
            .execute(Role::CoderFast, req())
            .await
            .expect("degrades and succeeds");
        assert_eq!(out.model, ModelId::new("mock", "fallback"));

        let records = fabric.last_records();
        assert!(
            matches!(records[0], FabricRecord::Degraded { candidate: ref c, .. } if *c == ModelId::new("mock", "fallback"))
        );
    }

    #[tokio::test]
    async fn execute_reports_exhausted_when_no_candidate_registered() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = RoleTable::parse("").expect("empty table parses");
        let fabric = Fabric::new(table, clock as Arc<dyn Clock>);

        let err = fabric.execute(Role::CoderFast, req()).await.unwrap_err();
        assert!(matches!(err, TmError::Provider(_)));
        assert_eq!(
            fabric.last_records(),
            vec![FabricRecord::Exhausted {
                role: Role::CoderFast
            }]
        );
    }

    #[tokio::test]
    async fn execute_errors_when_candidate_provider_not_registered() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "ghost", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock as Arc<dyn Clock>);
        // No provider registered under "ghost" at all: register_provider is never called.

        let err = fabric.execute(Role::CoderFast, req()).await.unwrap_err();
        match err {
            TmError::Provider(msg) => assert!(msg.contains("not registered")),
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_converts_provider_failure_to_tm_error() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);

        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock.clone() as Arc<dyn Clock>,
        ));
        let request = req();
        provider.script_failure(
            &request,
            crate::mock::ScriptedFailure {
                times: None,
                error: ProviderError::Unavailable("boom".into()),
            },
        );
        fabric.register_provider(provider);

        let err = fabric.execute(Role::CoderFast, req()).await.unwrap_err();
        assert!(matches!(err, TmError::Provider(msg) if msg.contains("boom")));
    }

    #[tokio::test]
    async fn failed_calls_do_not_trip_breaker_below_threshold() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 5 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);

        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock.clone() as Arc<dyn Clock>,
        ));
        let request = req();
        provider.script_failure(
            &request,
            crate::mock::ScriptedFailure {
                times: None,
                error: ProviderError::Unavailable("transient".into()),
            },
        );
        fabric.register_provider(provider);

        // Threshold is 5; a single failure must not open the breaker.
        let _ = fabric.execute(Role::CoderFast, req()).await;

        let snapshot = fabric.state_snapshot();
        let (_, state) = snapshot
            .candidates()
            .find(|(k, _)| **k == ModelId::new("mock", "m1"))
            .expect("registered");
        assert_eq!(state.breaker.state, BreakerState::Closed);
    }

    #[tokio::test]
    async fn invalid_request_failure_does_not_count_against_breaker() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 5 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);

        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock.clone() as Arc<dyn Clock>,
        ));
        let request = req();
        provider.script_failure(
            &request,
            crate::mock::ScriptedFailure {
                times: None,
                error: ProviderError::InvalidRequest("bad payload".into()),
            },
        );
        fabric.register_provider(provider);

        let _ = fabric.execute(Role::CoderFast, req()).await;

        let snapshot = fabric.state_snapshot();
        let (_, state) = snapshot
            .candidates()
            .find(|(k, _)| **k == ModelId::new("mock", "m1"))
            .expect("registered");
        assert_eq!(state.breaker.failure_count, 0);
    }

    #[tokio::test]
    async fn register_provider_is_idempotent_for_counters() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);

        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock.clone() as Arc<dyn Clock>,
        ));
        let request = req();
        provider.script_response(&request, completion(ModelId::new("mock", "m1"), &clock));
        fabric.register_provider(provider.clone());

        let _ = fabric
            .execute(Role::CoderFast, req())
            .await
            .expect("first call succeeds");

        // Re-registering the same provider must not reset the live counters gathered above.
        fabric.register_provider(provider);
        let snapshot = fabric.state_snapshot();
        let (_, state) = snapshot
            .candidates()
            .find(|(k, _)| **k == ModelId::new("mock", "m1"))
            .expect("registered");
        assert_eq!(state.tpm.used, 15);
    }

    #[tokio::test]
    async fn last_records_reflects_only_the_most_recent_call() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);

        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock.clone() as Arc<dyn Clock>,
        ));
        provider.script_response(&req(), completion(ModelId::new("mock", "m1"), &clock));
        fabric.register_provider(provider);

        let _ = fabric
            .execute(Role::CoderFast, req())
            .await
            .expect("succeeds");
        assert_eq!(fabric.last_records().len(), 1);

        let _ = fabric.execute(Role::SummarizerCheap, req()).await;
        assert_eq!(
            fabric.last_records(),
            vec![FabricRecord::Exhausted {
                role: Role::SummarizerCheap
            }]
        );
    }
}
