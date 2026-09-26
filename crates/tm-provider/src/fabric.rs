//! [`Fabric`]: the registry of [`Provider`] implementations that ties [`crate::route`] and
//! [`crate::state`] to actual (or mocked) calls.
//!
//! `execute()` is the only place in this crate that both makes a routing decision *and* performs
//! I/O: it calls [`crate::route::route`] (pure), invokes the chosen [`Provider`], and folds the
//! outcome back into [`crate::state::FabricState`] via [`crate::state::FabricEvent`]. It also
//! turns each step into a structured [`FabricRecord`] the caller can adapt into workspace events
//! (`provider.selected` / `provider.exhausted` / `provider.degraded` / `provider.recovered`).
//!
//! `execute()` is also the one real boundary every outbound model call goes through, which is
//! why it is where `docs/audit-2026-09-18-fable.md`'s "M-04" item wires secret redaction: before
//! `req` reaches [`Provider::complete`], [`Fabric`]'s own [`tm_auth::SessionRedactor`] scans its
//! text/JSON content for secret-shaped substrings and replaces them with a placeholder (see
//! [`redact_completion_request`]). [`Fabric::restore_local`] is the one sanctioned way back, for
//! a human's own local view only — see `tm_auth::redact`'s docs and
//! `docs/decisions/D-011-secret-redaction.md`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use tm_auth::SessionRedactor;
use tm_types::{Clock, Result as TmResult, Role, Timestamp, TmError};

use crate::role_config::{Price, RoleTable};
use crate::route::{Need, RouteDecision};
use crate::state::{BreakerState, FabricEvent, FabricState};
use crate::types::{
    Completion, CompletionRequest, ContentBlock, EmbedRequest, Embeddings, Message, ModelId,
    ProviderError, Usage,
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

/// Releases a candidate's concurrency slot if the call holding it is dropped before it settles
/// (a turn interrupted mid-request): `RequestStarted` without a matching finish would otherwise
/// leave the slot taken for the fabric's whole life. Cancelling isn't the provider's fault, so it
/// never counts against the breaker.
struct InFlight<'a> {
    fabric: &'a Fabric,
    candidate: Option<ModelId>,
}

impl InFlight<'_> {
    /// The call finished; its own success or failure event releases the slot instead.
    fn settle(mut self) {
        self.candidate = None;
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if let Some(candidate) = self.candidate.take() {
            self.fabric.state.write().apply(
                FabricEvent::RequestFailed {
                    candidate,
                    counts_against_breaker: false,
                },
                self.fabric.clock.now(),
            );
        }
    }
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

/// One entry of [`Fabric::affordable_candidates`]: a candidate that can afford the estimated
/// call, plus what that estimate came out to.
#[derive(Debug, Clone, PartialEq)]
pub struct AffordableCandidate {
    /// The candidate itself.
    pub model: ModelId,
    /// The estimated cost in micro-dollars, or `None` for an unpriced (subscription/unmetered)
    /// candidate.
    pub estimated_cost_micros: Option<u64>,
}

/// The real priced cost of a completion, in micro-dollars, given the candidate's
/// per-1,000,000-token [`Price`] and the [`Completion`]'s actual token [`Usage`]
/// (`tel-completion-cost-field`). Cache tokens are not separately priced (no candidate configures
/// a cache rate today), so only `input_tokens`/`output_tokens` count.
///
/// [`Price`] is denominated per 1,000,000 tokens, not per token
/// (`critic-real-prices-and-price-unit`): a per-token micro-dollar unit truncates to 0 for any
/// model priced under $1/M tokens, silently zeroing this for exactly the cheap models this repo
/// actually wants costed. The intermediate multiply runs in `u128` (a `u64 * u64` product can
/// itself already approach `u64::MAX` for a large token count against a normal price) before
/// dividing back down by 1,000,000, and the final result saturates to `u64::MAX` rather than
/// wrapping, so an absurd token count from a misbehaving provider clamps instead of silently
/// wrapping around to a tiny number that would then under-charge the ticket's budget.
pub fn cost_micros(price: &Price, usage: &Usage) -> u64 {
    let input = (usage.input_tokens as u128) * (price.input_micros_per_million_tokens as u128);
    let output = (usage.output_tokens as u128) * (price.output_micros_per_million_tokens as u128);
    ((input + output) / 1_000_000).min(u64::MAX as u128) as u64
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
    /// The table as configured; [`Fabric::prefer`] layers its choice over this, so it can be
    /// taken back.
    configured: RoleTable,
    /// The table routing uses: `configured` plus any preference.
    table: RwLock<RoleTable>,
    state: RwLock<FabricState>,
    clock: Arc<dyn Clock>,
    failure_threshold: u32,
    breaker_window: std::time::Duration,
    breaker_cooldown: std::time::Duration,
    ewma_alpha: f64,
    records: RwLock<Vec<FabricRecord>>,
    redactor: SessionRedactor,
}

impl Fabric {
    /// Build a fabric with the given role table and injected clock, and default breaker/EWMA
    /// tuning (5 failures per 60s window trips the breaker, 30s cooldown, EWMA alpha 0.3).
    pub fn new(table: RoleTable, clock: Arc<dyn Clock>) -> Self {
        Fabric {
            providers: RwLock::new(BTreeMap::new()),
            configured: table.clone(),
            table: RwLock::new(table),
            state: RwLock::new(FabricState::new()),
            clock,
            failure_threshold: 5,
            breaker_window: std::time::Duration::from_secs(60),
            breaker_cooldown: std::time::Duration::from_secs(30),
            ewma_alpha: 0.3,
            records: RwLock::new(Vec::new()),
            redactor: SessionRedactor::new(),
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

    /// Route `role` to `model` before anything else (a session's `/model`), keeping the role's
    /// configured candidates behind it as fallbacks (see [`RoleTable::prefer`]); `None` goes back
    /// to the configured order. Replaces any earlier preference.
    pub fn prefer(&self, role: Role, model: Option<&ModelId>) {
        let mut table = self.configured.clone();
        if let Some(model) = model {
            table.prefer(role, &model.provider, &model.model);
        }
        *self.table.write() = table;
        let Some(model) = model else {
            return;
        };
        if self.providers.read().contains_key(&model.provider) {
            self.state.write().register(
                model.clone(),
                self.clock.now(),
                self.failure_threshold,
                self.breaker_window,
                self.breaker_cooldown,
                self.ewma_alpha,
            );
        }
    }

    /// `role`'s candidates, in the order routing tries them.
    pub fn candidates(&self, role: Role) -> Vec<ModelId> {
        self.table
            .read()
            .candidates_for(role)
            .iter()
            .map(|c| ModelId::new(c.provider.clone(), c.model.clone()))
            .collect()
    }

    /// `role`'s candidates, in table order, that can afford one more call estimated at
    /// `prompt_tokens` input tokens plus up to `max_output_tokens` generated tokens, given
    /// `remaining_dollars_micros` left in the ticket's dollar budget
    /// (`budget-affordability-menu-in-context-pack`, `SPEC.md` §31.2).
    ///
    /// Each candidate's estimated cost is `prompt_tokens * candidate.price.input +
    /// max_output_tokens * candidate.price.output`, computed via [`cost_micros`] against a
    /// synthetic [`Usage`] --
    /// the same "input tokens times input price plus output tokens times output price" formula
    /// [`cost_micros`]'s own doc comment describes, just against an upper-bound estimate instead
    /// of a completion's real, already-known usage. A candidate with no
    /// [`crate::role_config::RoleCandidate::price`]
    /// (subscription/unmetered capacity) is always affordable, since its dollar cost isn't
    /// tracked; `remaining_dollars_micros == u64::MAX` (an unmetered dollar budget) affords every
    /// priced candidate too. The primary candidate (index 0 of [`Fabric::candidates`]) is still
    /// first here when it is itself affordable -- this never reorders by price, only filters.
    pub fn affordable_candidates(
        &self,
        role: Role,
        remaining_dollars_micros: u64,
        prompt_tokens: u64,
        max_output_tokens: u32,
    ) -> Vec<AffordableCandidate> {
        let usage = Usage {
            input_tokens: u32::try_from(prompt_tokens).unwrap_or(u32::MAX),
            output_tokens: max_output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        self.table
            .read()
            .candidates_for(role)
            .iter()
            .filter_map(|c| {
                let model = ModelId::new(c.provider.clone(), c.model.clone());
                match c.price {
                    None => Some(AffordableCandidate {
                        model,
                        estimated_cost_micros: None,
                    }),
                    Some(price) => {
                        let cost = cost_micros(&price, &usage);
                        (remaining_dollars_micros == u64::MAX || cost <= remaining_dollars_micros)
                            .then_some(AffordableCandidate {
                                model,
                                estimated_cost_micros: Some(cost),
                            })
                    }
                }
            })
            .collect()
    }

    /// The ids of every registered provider, in sorted order.
    pub fn provider_ids(&self) -> Vec<String> {
        self.providers.read().keys().cloned().collect()
    }

    /// The registered provider with id `id`, if any. This bypasses routing, breakers, quota
    /// accounting and redaction entirely: it is for diagnostics that want to address one
    /// concrete backend (e.g. `tm provider test`), not for issuing real turn traffic, which must
    /// go through [`Fabric::execute`].
    pub fn provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers
            .read()
            .get(id)
            .map(|record| record.provider.clone())
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

    /// Preflight check for a role's *configuration*, deliberately blind to capacity/breaker
    /// state: "can anything possibly ever serve this role", not [`Fabric::route`]'s "can
    /// something serve it right now". Meant to run before a ticket is leased or dispatched at
    /// all, so a `providers.toml` candidate naming a provider that was never registered (no
    /// credential, never wired up) fails fast with a specific diagnosis instead of only
    /// surfacing after a lease has already been granted and burned, once
    /// [`Fabric::execute_priced`]'s [`RouteDecision::Exhausted`] path reports the same problem
    /// mid-run.
    ///
    /// Returns `Ok(())` as soon as at least one of `role`'s candidates is backed by a registered
    /// provider — routing may still fail that call later for capacity/breaker/quota reasons, and
    /// this check does not attempt to predict that; those are transient, not configuration,
    /// problems, and are exactly what `route`/`execute` already handle. `Err` names the
    /// unregistered provider and points at the concrete fix.
    pub fn preflight_role(&self, role: Role) -> Result<(), String> {
        let candidates = self.candidates(role);
        if candidates.is_empty() {
            return Err(format!(
                "no candidate is configured for role {}. Add one under `[{}]` in providers.toml.",
                role.as_str(),
                role.config_key()
            ));
        }
        {
            let providers = self.providers.read();
            if candidates
                .iter()
                .any(|c| providers.contains_key(&c.provider))
            {
                return Ok(());
            }
        }
        let unregistered = self
            .first_unregistered_provider(role)
            .unwrap_or_else(|| candidates[0].provider.clone());
        Err(format!(
            "no candidate can serve role {}: provider not registered: {unregistered}. Check \
             `[{}]`'s candidates in providers.toml and run `tm provider list` to see which \
             providers are actually registered and credentialed, then fix the candidate (or its \
             credential) before retrying — rerunning with this same configuration will fail \
             identically every time.",
            role.as_str(),
            role.config_key()
        ))
    }

    /// The pure routing decision for `role` right now, with no side effects beyond rolling
    /// windows forward (which is itself a pure function of `now`, done for accuracy of reads).
    pub fn route(&self, role: Role, need: &Need, now: Timestamp) -> RouteDecision {
        let table = self.table.read();
        let state = self.state.read();
        crate::route::route(&table, &state, role, need, now)
    }

    /// Route `role`, call the chosen provider, record the outcome, and return the completion.
    /// Delegates to [`Fabric::execute_priced`], dropping the priced cost that call also
    /// computes -- callers that need `usage.recorded`'s real `dollars_micros` (`SPEC.md`
    /// §31/telemetry) should call `execute_priced` directly instead of recovering the price by
    /// hand from [`Completion::usage`].
    pub async fn execute(&self, role: Role, req: CompletionRequest) -> TmResult<Completion> {
        self.execute_priced(role, req).await.map(|(c, _)| c)
    }

    /// Same as [`Fabric::execute`], but also returns the call's priced cost in micro-dollars
    /// (`None` when the candidate that actually served the request has no [`Price`] configured
    /// -- `docs/tasks/TASKS.md`'s `tel-completion-cost-field`). The candidate looked up for
    /// pricing is the one that actually served the call (`candidate_key`), not merely the one
    /// [`Fabric::route`] initially selected, so a mid-flight [`RouteDecision::Degrade`] prices
    /// correctly off the candidate that was actually billed.
    pub async fn execute_priced(
        &self,
        role: Role,
        req: CompletionRequest,
    ) -> TmResult<(Completion, Option<u64>)> {
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

        self.dispatch_to_candidate(candidate_key, record, was_half_open, req)
            .await
    }

    /// Same as [`Fabric::execute_priced`], but bypasses [`Fabric::route`]'s own candidate choice
    /// and always calls `candidate` (`budget-affordability-menu-in-context-pack`, `SPEC.md`
    /// §31.2): the caller (`tm_agent::agent_loop::AgentLoop::drive`) already picked `candidate`
    /// itself, via [`Fabric::affordable_candidates`], as the cheapest tier that still affords the
    /// step's estimated dollar cost after the role's primary candidate did not. Concurrency/rpm/
    /// tpm/breaker admission is intentionally *not* re-checked here (unlike [`Fabric::route`]'s
    /// normal path): `candidate` was chosen purely for its price, and re-running admission would
    /// need a second, redundant routing pass over the same [`RoleTable`] the caller already
    /// consulted. A candidate that turns out to be unhealthy still fails cleanly through the
    /// ordinary provider-error path below (recorded, breaker-counted if retryable) rather than
    /// silently falling back again -- a tier-down is already a deliberate one-step decision, not
    /// something to keep degrading past.
    ///
    /// Always recorded as [`FabricRecord::Degraded`] (`reason` explains this was a budget
    /// tier-down, not a health-driven fallback), so the caller's `provider.degraded` event and
    /// `tm stats` both see it the same way any other degrade shows up.
    pub async fn execute_priced_tier_down(
        &self,
        role: Role,
        candidate: ModelId,
        req: CompletionRequest,
    ) -> TmResult<(Completion, Option<u64>)> {
        self.state.write().roll_windows(self.clock.now());
        let was_half_open = self.is_half_open(&candidate);
        let record = FabricRecord::Degraded {
            role,
            candidate: candidate.clone(),
            reason: "budget tier-down: primary candidate's estimated cost exceeds the ticket's \
                     remaining dollar budget"
                .to_string(),
        };
        self.dispatch_to_candidate(candidate, record, was_half_open, req)
            .await
    }

    /// The shared tail of [`Fabric::execute_priced`] and [`Fabric::execute_priced_tier_down`]:
    /// redact, dispatch to `candidate_key`'s provider, price the result and fold the outcome into
    /// [`FabricState`]. `record` is published via [`Fabric::last_records`] before the call, same
    /// as it always was inline in `execute_priced`.
    async fn dispatch_to_candidate(
        &self,
        candidate_key: ModelId,
        record: FabricRecord,
        was_half_open: bool,
        req: CompletionRequest,
    ) -> TmResult<(Completion, Option<u64>)> {
        let role = match &record {
            FabricRecord::Selected { role, .. } | FabricRecord::Degraded { role, .. } => *role,
            _ => unreachable!("dispatch_to_candidate is only ever called with Selected/Degraded"),
        };
        let now = self.clock.now();
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

        // Redact secret-shaped substrings out of the outbound request before it reaches the
        // provider -- see this module's top docs and `tm_auth::redact`'s.
        let req = redact_completion_request(&self.redactor, req);
        // The candidate names a model, not just a provider; the provider serves that one.
        let req = CompletionRequest {
            model: Some(candidate_key.model.clone()),
            ..req
        };
        let in_flight = InFlight {
            fabric: self,
            candidate: Some(candidate_key.clone()),
        };
        let call_result = provider.complete(req).await;
        in_flight.settle();
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
                        .map(|price| cost_micros(&price, &completion.usage))
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

                Ok((completion, cost_micros))
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

    /// Restore any redaction placeholders in `text` back to the secret-shaped value they
    /// replaced, using this fabric's own [`tm_auth::SessionRedactor`]. The one sanctioned local
    /// restore: `text` must be something about to be shown to a human directly (a CLI/TUI
    /// render), never forwarded to a model or a persisted store -- see `tm_auth::redact`'s docs.
    pub fn restore_local(&self, text: &str) -> String {
        self.redactor.restore(text)
    }
}

/// Redact secret-shaped substrings out of every text/JSON-bearing part of `req` before it
/// reaches a [`Provider`]: the system prompt, every message's text/tool-use-input/tool-result
/// content (recursively, since [`ContentBlock::ToolResult`] can itself carry further content
/// blocks), via `redactor`.
fn redact_completion_request(
    redactor: &SessionRedactor,
    req: CompletionRequest,
) -> CompletionRequest {
    CompletionRequest {
        system: req.system.map(|s| redactor.redact(&s)),
        messages: req
            .messages
            .into_iter()
            .map(|m| redact_message(redactor, m))
            .collect(),
        ..req
    }
}

fn redact_message(redactor: &SessionRedactor, message: Message) -> Message {
    Message {
        role: message.role,
        content: message
            .content
            .into_iter()
            .map(|b| redact_content_block(redactor, b))
            .collect(),
    }
}

fn redact_content_block(redactor: &SessionRedactor, block: ContentBlock) -> ContentBlock {
    match block {
        ContentBlock::Text { text } => ContentBlock::Text {
            text: redactor.redact(&text),
        },
        ContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse {
            id,
            name,
            input: redactor.redact_json(&input),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ContentBlock::ToolResult {
            tool_use_id,
            content: content
                .into_iter()
                .map(|b| redact_content_block(redactor, b))
                .collect(),
            is_error,
        },
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
            model: None,
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
    async fn execute_priced_returns_the_priced_cost_for_a_priced_candidate() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 1, price = { input_micros_per_million_tokens = 42000000, output_micros_per_million_tokens = 84000000 } }"#,
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

        let (_, cost_micros) = fabric
            .execute_priced(Role::CoderFast, req())
            .await
            .expect("call succeeds");
        // completion()'s scripted usage is 10 input + 5 output tokens:
        // (10*42_000_000 + 5*84_000_000) / 1_000_000 = 840.
        assert_eq!(cost_micros, Some(840));
    }

    /// `critic-real-prices-and-price-unit`: a sub-$1/M-token model priced over a realistic token
    /// count must produce a nonzero cost. A per-token micro-dollar unit would have floored this
    /// to 0 for any single-token usage, silently zeroing `tel-completion-cost-field`/`tm
    /// stats`/budget tier-down for exactly the cheap models this repo actually wants costed.
    #[test]
    fn cost_micros_prices_a_sub_dollar_per_million_model_over_realistic_usage() {
        let price = Price {
            input_micros_per_million_tokens: 42_000,
            output_micros_per_million_tokens: 84_000,
        };
        let usage = Usage {
            input_tokens: 1_500,
            output_tokens: 400,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        // (1500*42_000 + 400*84_000) / 1_000_000 = (63_000_000 + 33_600_000) / 1_000_000 = 96.
        let priced = cost_micros(&price, &usage);
        assert_eq!(priced, 96);
        assert!(
            priced > 0,
            "a sub-$1/M model over realistic usage must not truncate to zero"
        );
    }

    #[tokio::test]
    async fn execute_priced_returns_none_for_an_unpriced_candidate() {
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

        let (_, cost_micros) = fabric
            .execute_priced(Role::CoderFast, req())
            .await
            .expect("call succeeds");
        assert_eq!(cost_micros, None);
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

    // ---- preflight_role (`t20260925-1314-sindresorhus-ky-878-provider-retry-guidance`) --------

    #[test]
    fn preflight_role_ok_when_a_candidate_is_registered() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);
        fabric.register_provider(Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock as Arc<dyn Clock>,
        )));

        assert_eq!(fabric.preflight_role(Role::CoderFast), Ok(()));
    }

    #[test]
    fn preflight_role_names_the_unregistered_provider_and_a_repair_path() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "devpass", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock as Arc<dyn Clock>);
        // No provider registered under "devpass" at all.

        let err = fabric
            .preflight_role(Role::CoderFast)
            .expect_err("no candidate is registered");
        assert!(
            err.contains("provider not registered: devpass"),
            "names the specific missing provider: {err}"
        );
        assert!(
            err.contains("tm provider list"),
            "points at a real repair path, not a blind retry: {err}"
        );
        assert!(
            !err.to_lowercase().contains("run `tm run"),
            "must not recommend rerunning the same broken config unchanged: {err}"
        );
    }

    #[test]
    fn preflight_role_reports_no_candidate_configured_at_all() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = RoleTable::parse("").expect("empty table parses");
        let fabric = Fabric::new(table, clock as Arc<dyn Clock>);

        let err = fabric
            .preflight_role(Role::CoderFast)
            .expect_err("no candidate configured");
        assert!(err.contains("providers.toml"));
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

    /// A provider whose calls never finish, standing in for a request a human interrupts.
    struct NeverAnswers;

    #[async_trait]
    impl Provider for NeverAnswers {
        fn id(&self) -> &str {
            "mock"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<Completion, ProviderError> {
            std::future::pending().await
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Embeddings, ProviderError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn the_provider_is_asked_for_the_model_the_table_routed_to() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        // The provider was built for `m1`; the table names `m2`.
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m2", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);
        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock.clone() as Arc<dyn Clock>,
        ));
        let reply = provider.deterministic_completion(&CompletionRequest {
            model: Some("m2".into()),
            ..req()
        });
        provider.script_response(&req(), reply);
        fabric.register_provider(provider.clone());

        let out = fabric
            .execute(Role::CoderFast, req())
            .await
            .expect("call succeeds");
        assert_eq!(out.model, ModelId::new("mock", "m2"));
        assert_eq!(provider.call_log()[0].model.as_deref(), Some("m2"));
    }

    #[tokio::test]
    async fn a_preferred_model_is_routed_to_first() {
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
        let reply = provider.deterministic_completion(&CompletionRequest {
            model: Some("m9".into()),
            ..req()
        });
        provider.script_response(&req(), reply);
        fabric.register_provider(provider.clone());

        fabric.prefer(Role::CoderFast, Some(&ModelId::new("mock", "m9")));
        assert_eq!(
            fabric.candidates(Role::CoderFast),
            [ModelId::new("mock", "m9"), ModelId::new("mock", "m1")]
        );
        let out = fabric
            .execute(Role::CoderFast, req())
            .await
            .expect("the preferred model is routable");
        assert_eq!(out.model, ModelId::new("mock", "m9"));

        fabric.prefer(Role::CoderFast, None);
        assert_eq!(
            fabric.candidates(Role::CoderFast),
            [ModelId::new("mock", "m1")],
            "taken back"
        );
    }

    #[tokio::test]
    async fn a_cancelled_call_gives_its_concurrency_slot_back() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);
        fabric.register_provider(Arc::new(NeverAnswers));

        tokio::select! {
            _ = fabric.execute(Role::CoderFast, req()) => panic!("NeverAnswers never answers"),
            _ = tokio::task::yield_now() => {}
        }

        let snapshot = fabric.state_snapshot();
        let (_, state) = snapshot
            .candidates()
            .find(|(k, _)| **k == ModelId::new("mock", "m1"))
            .expect("registered");
        assert_eq!(
            state.live_concurrency, 0,
            "the dropped call released its slot"
        );
        assert_eq!(
            state.breaker.failure_count, 0,
            "cancelling is not a provider failure"
        );
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

    /// The regression gate for M-04's "secret redaction at `Fabric::execute` boundary": builds a
    /// real request through the real `execute()` call, with a known fake-secret-shaped canary
    /// planted in a system prompt, a message's text, a tool call's input, and a tool result's
    /// nested content -- everywhere [`ContentBlock`] can carry text -- and asserts the plaintext
    /// canary never reaches [`MockProvider::complete`] (inspected via its real `call_log`, i.e.
    /// what actually left the process, not the redaction function called in isolation).
    /// [`Fabric::restore_local`] then proves the human-local restore path still recovers it.
    #[tokio::test]
    async fn execute_redacts_secret_shaped_content_before_it_reaches_the_provider() {
        const CANARY: &str = "sk-CANARY0123456789abcdefghijklmnopqrstuv";

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
        // The request's own hash changes once redacted, so script a default response rather than
        // one keyed to the (pre-redaction) request this test builds.
        provider.script_default_response(completion(ModelId::new("mock", "m1"), &clock));
        fabric.register_provider(Arc::clone(&provider) as Arc<dyn Provider>);

        let request = CompletionRequest {
            system: Some(format!("You may need this key: {CANARY}")),
            messages: vec![
                Message {
                    role: MessageRole::User,
                    content: vec![crate::types::ContentBlock::Text {
                        text: format!("here's my key {CANARY}, please use it"),
                    }],
                },
                Message {
                    role: MessageRole::Assistant,
                    content: vec![crate::types::ContentBlock::ToolUse {
                        id: "call-1".to_string(),
                        name: "shell.run".to_string(),
                        input: serde_json::json!({ "command": format!("curl -H 'key: {CANARY}'") }),
                    }],
                },
                Message {
                    role: MessageRole::User,
                    content: vec![crate::types::ContentBlock::ToolResult {
                        tool_use_id: "call-1".to_string(),
                        content: vec![crate::types::ContentBlock::Text {
                            text: format!("output leaked {CANARY} from env"),
                        }],
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            temperature: None,
            stop_sequences: vec![],
            stream: false,
            n: 1,
            model: None,
        };

        let _ = fabric
            .execute(Role::CoderFast, request)
            .await
            .expect("call succeeds");

        let received = provider.call_log();
        assert_eq!(received.len(), 1);
        let sent = &received[0];
        let sent_json = serde_json::to_string(sent).expect("request serializes for inspection");
        assert!(
            !sent_json.contains(CANARY),
            "canary reached the provider unredacted: {sent_json}"
        );
        assert!(
            sent_json.contains("<redacted:api_key:"),
            "expected a redaction placeholder in what was actually sent: {sent_json}"
        );

        // Local restore recovers the original for a human's own view; it must never itself be
        // sent anywhere -- this call only proves the mapping still holds it.
        let restored = fabric.restore_local(&sent_json);
        assert!(
            restored.contains(CANARY),
            "local restore should recover the canary: {restored}"
        );
    }

    /// The false-positive half of the same gate: legitimate long identifiers this codebase (and
    /// its own git history) already generates -- a blake3 content hash and a git commit sha --
    /// must both survive `execute()` untouched, not get mistaken for a secret.
    #[tokio::test]
    async fn execute_does_not_redact_a_real_content_hash_or_a_git_sha() {
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
        provider.script_default_response(completion(ModelId::new("mock", "m1"), &clock));
        fabric.register_provider(Arc::clone(&provider) as Arc<dyn Provider>);

        let content_hash = blake3::hash(b"some file contents").to_hex().to_string();
        let git_sha = "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3";
        let request = CompletionRequest {
            system: None,
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![crate::types::ContentBlock::Text {
                    text: format!("artifact hash: {content_hash}, commit: {git_sha}"),
                }],
            }],
            tools: vec![],
            max_tokens: 100,
            temperature: None,
            stop_sequences: vec![],
            stream: false,
            n: 1,
            model: None,
        };

        let _ = fabric
            .execute(Role::CoderFast, request)
            .await
            .expect("call succeeds");

        let received = provider.call_log();
        let sent_json = serde_json::to_string(&received[0]).expect("serializes");
        assert!(
            sent_json.contains(&content_hash),
            "a real content hash must survive redaction unchanged: {sent_json}"
        );
        assert!(
            sent_json.contains(git_sha),
            "a real git commit sha must survive redaction unchanged: {sent_json}"
        );
    }

    #[test]
    fn provider_accessors_expose_exactly_what_was_registered() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "m1", max_concurrency = 1 }"#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);
        assert!(fabric.provider_ids().is_empty());
        assert!(fabric.provider("mock").is_none());

        fabric.register_provider(Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock as Arc<dyn Clock>,
        )));
        assert_eq!(fabric.provider_ids(), vec!["mock".to_string()]);
        assert_eq!(
            fabric.provider("mock").map(|p| p.id().to_string()),
            Some("mock".into())
        );
        assert!(fabric.provider("anthropic").is_none());
    }

    // ---- affordable_candidates / execute_priced_tier_down (`budget-affordability-menu-in-
    // context-pack`, `SPEC.md` §31.2) -----------------------------------------------------------

    #[test]
    fn affordable_candidates_keeps_the_unpriced_and_cheap_and_drops_the_expensive_one() {
        let table = table_with_candidates(
            "coder_fast",
            r#"
            { provider = "mock", model = "expensive", max_concurrency = 1, price = { input_micros_per_million_tokens = 10_000_000, output_micros_per_million_tokens = 10_000_000 } },
            { provider = "mock", model = "cheap", max_concurrency = 1, degraded_ok = true, price = { input_micros_per_million_tokens = 100, output_micros_per_million_tokens = 100 } },
            { provider = "mock", model = "unmetered", max_concurrency = 1, degraded_ok = true }
            "#,
        );
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let fabric = Fabric::new(table, clock as Arc<dyn Clock>);

        // 1,000 prompt tokens + 1,000 output tokens against "expensive"'s $10/M-token price on
        // both dimensions prices the call at $0.02 -- more than the $0.001 left in budget, so it
        // must not appear; "cheap" and "unmetered" both still fit.
        let affordable = fabric.affordable_candidates(Role::CoderFast, 1_000, 1_000, 1_000);

        let models: Vec<&str> = affordable.iter().map(|a| a.model.model.as_str()).collect();
        assert_eq!(models, vec!["cheap", "unmetered"]);
        assert_eq!(
            affordable[1].estimated_cost_micros, None,
            "an unpriced candidate is always affordable and untracked"
        );
        assert!(affordable[0].estimated_cost_micros.unwrap() <= 1_000);
    }

    #[test]
    fn affordable_candidates_treats_an_unlimited_remaining_budget_as_affording_everything() {
        let table = table_with_candidates(
            "coder_fast",
            r#"{ provider = "mock", model = "pricey", max_concurrency = 1, price = { input_micros_per_million_tokens = 1_000_000_000, output_micros_per_million_tokens = 1_000_000_000 } }"#,
        );
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let fabric = Fabric::new(table, clock as Arc<dyn Clock>);

        let affordable = fabric.affordable_candidates(Role::CoderFast, u64::MAX, 1_000_000, 4_096);
        assert_eq!(affordable.len(), 1);
    }

    #[tokio::test]
    async fn execute_priced_tier_down_calls_the_forced_candidate_and_records_a_budget_degrade() {
        let clock: Arc<FixedClock> = Arc::new(FixedClock::epoch());
        let table = table_with_candidates(
            "coder_fast",
            r#"
            { provider = "mock", model = "expensive", max_concurrency = 1, price = { input_micros_per_million_tokens = 10_000_000, output_micros_per_million_tokens = 10_000_000 } },
            { provider = "mock", model = "cheap", max_concurrency = 1, degraded_ok = true, price = { input_micros_per_million_tokens = 100, output_micros_per_million_tokens = 100 } }
            "#,
        );
        let fabric = Fabric::new(table, clock.clone() as Arc<dyn Clock>);

        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "cheap"),
            clock.clone() as Arc<dyn Clock>,
        ));
        let request = req();
        provider.script_response(&request, completion(ModelId::new("mock", "cheap"), &clock));
        fabric.register_provider(provider);

        let (out, cost_micros) = fabric
            .execute_priced_tier_down(Role::CoderFast, ModelId::new("mock", "cheap"), req())
            .await
            .expect("the cheap candidate serves the call");
        assert_eq!(out.model, ModelId::new("mock", "cheap"));
        // 10 input + 5 output tokens (`completion`'s fixed `Usage`) at $0.0001/M tokens both
        // ways: (10*100 + 5*100) / 1_000_000 truncates to 0 micro-dollars -- real, just tiny.
        assert_eq!(cost_micros, Some(0));

        let records = fabric.last_records();
        assert_eq!(
            records,
            vec![FabricRecord::Degraded {
                role: Role::CoderFast,
                candidate: ModelId::new("mock", "cheap"),
                reason: "budget tier-down: primary candidate's estimated cost exceeds the \
                         ticket's remaining dollar budget"
                    .to_string(),
            }]
        );
    }
}
