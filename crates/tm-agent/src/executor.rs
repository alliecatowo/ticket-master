//! [`BuiltinExecutor`] and [`HumanExecutor`]: `tm_core::Executor` implementations a
//! `tm_scheduler::ExecutorDispatcher` can drive (`SPEC.md` §24.3's `builtin` and `human`
//! adapters).
//!
//! `BuiltinExecutor` is the important one: it wraps [`crate::agent_loop::AgentLoop`] and, on
//! every [`Executor::execute`] call, constructs a *fresh* loop whose own authority/budget
//! ceiling is the task's own already-attenuated [`tm_types::Authority`]/[`tm_types::Budget`] —
//! never `Authority::root()`/`Budget::unlimited()`. That is the fix `SPEC.md` audit B-01/B-04
//! asks for: the one path that previously ran an agent (the interactive readline in
//! `tm-cli`) constructed the loop with an unlimited ceiling and relied on
//! `AgentTask::authority`/`budget` alone to narrow it back down on every call; this constructs
//! the ceiling *and* the per-call scope from the same ticket authority, so a future call site
//! that forgets to pass a per-task authority still fails safe instead of defaulting open.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tm_codeintel::CodeIntel;
use tm_context::command::{CommandCache, CommandExecutor};
use tm_core::executor::{
    CostClass, ExecutionHandle, Executor, ExecutorCapabilities, ExecutorFailure, ExecutorOutcome,
    ExecutorTask,
};
use tm_core::{ArtifactKind, FailureClass, Store};
use tm_provider::fabric::Fabric;
use tm_types::{CapabilityProvider, Clock, IdKind, IdSource, Oversight, ParticipantId, SessionId};

use crate::agent_loop::{AgentLoop, InvestigationSummary, NO_SUBMIT_DETAIL};
use crate::outcome::{AgentOutcome, AgentTask};
use crate::tools::ToolRegistry;

/// Tag stamped into a stored artifact's `meta.kind`
/// (`recover-without-repeating-agent-investigation`) to mark it as one of
/// [`BuiltinExecutor::persist_no_submit_investigation`]'s own records rather than any other
/// artifact a run might have produced — [`BuiltinExecutor::prior_investigation`] filters on this
/// exact value, plus `meta.no_submit_ticket`, when scanning `Store::view`'s artifacts.
const NO_SUBMIT_ARTIFACT_KIND: &str = "no_submit_investigation";

/// What [`BuiltinExecutor::prior_investigation`] recovers about a ticket's most recent no-submit
/// attempt, read back out of the artifact [`BuiltinExecutor::persist_no_submit_investigation`]
/// wrote for it — the cross-attempt memory this task adds, since neither `AgentTask` nor
/// `ExecutorTask` carries anything from one dispatch to the next on its own.
#[derive(Debug, Clone)]
struct PriorInvestigation {
    /// What that attempt's own investigation touched/concluded.
    summary: InvestigationSummary,
    /// How many consecutive no-submit attempts (including that one) this ticket has had in a row,
    /// as of when that attempt's summary was persisted.
    attempt_count: u32,
    /// Whether that attempt's own investigation already overlapped the one before it — i.e.
    /// whether the *previous* pair of attempts was already a repeat, not just this one.
    repeated: bool,
    /// Whether this round's chain was later marked resolved by a real submission
    /// ([`BuiltinExecutor::mark_investigation_cleared`]).
    cleared: bool,
}

impl PriorInvestigation {
    /// `self`, unless it is [`PriorInvestigation::cleared`] — the view every caller that wants to
    /// *steer* a following attempt (context augmentation, the overlap/`repeated` check) must go
    /// through, as opposed to [`BuiltinExecutor::prior_investigation`]'s raw winner, whose
    /// `attempt_count` numbering must stay continuous across a clearing (see that method's own
    /// doc comment on why it does not filter this out itself).
    fn effective(&self) -> Option<&Self> {
        (!self.cleared).then_some(self)
    }
}

/// Oversized `tm-browser` tool outputs (screenshots, network bodies, PDFs) at or above this size
/// are spilled to a stored artifact instead of inlined — the same threshold
/// [`crate::tools::MAX_INLINE_RESULT_BYTES`] uses for the builtin capability's own results, kept
/// as a separate constant because `tm_browser::session::BrowserSessionConfig` takes it as a raw
/// `usize` rather than importing this crate's constant.
const BROWSER_ARTIFACT_THRESHOLD_BYTES: usize = crate::tools::MAX_INLINE_RESULT_BYTES;

/// A [`tm_browser::session::ArtifactSink`] backed by `tm-core`'s `Store`, for wiring
/// [`tm_browser::BrowserCapability`] to durable storage the same way
/// [`crate::tools::bound_result`] does for the builtin capability's own oversized results.
///
/// Every artifact is stored under [`ParticipantId::system`] rather than the specific ticket/actor
/// a browser tool call happened on behalf of: `ArtifactSink::store` carries no ticket/actor
/// context (a `BrowserSession`'s sink is bound once at session-launch time, not threaded through
/// every call), and `Store::store_artifact` requires one. This is a real simplification, not an
/// oversight — an artifact stored this way is attributable to "the browser capability" but not to
/// the exact ticket that triggered it. Fixing that would need `ArtifactSink::store` itself to
/// grow ticket/actor parameters, a `tm-browser` API change out of this task's scope.
pub struct StoreArtifactSink {
    store: Arc<Store>,
}

impl StoreArtifactSink {
    /// Build a sink writing into `store`.
    pub fn new(store: Arc<Store>) -> Self {
        StoreArtifactSink { store }
    }
}

impl tm_browser::session::ArtifactSink for StoreArtifactSink {
    fn store(&self, bytes: &[u8], content_type: &str) -> tm_types::Result<tm_types::ArtifactId> {
        let events = self.store.store_artifact(
            ArtifactKind::Report,
            content_type.to_string(),
            bytes.to_vec(),
            serde_json::json!({"source": "browser"}),
            None,
            ParticipantId::system(),
        )?;
        events
            .iter()
            .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
            .ok_or_else(|| {
                tm_types::TmError::invariant("store_artifact did not emit artifact.created")
            })
    }
}

/// What [`BuiltinExecutor`] needs to register a [`tm_browser::BrowserCapability`] per dispatched
/// task: an already-constructed provider registry (from `browser.toml`) and an artifact sink.
/// `BuiltinExecutor::browser` being `None` means the project has no `browser.toml` configured (or
/// the binary assembling this executor chose not to load one) — browser tools are then simply
/// never registered for any task this executor runs, rather than every dispatch failing.
pub struct BrowserWiring {
    /// The provider fallback order `browser.toml` selected (`SPEC.md` §19.1a).
    pub providers: Arc<tm_browser::ProviderRegistry>,
    /// Where oversized browser tool outputs are stored; [`StoreArtifactSink`] is the ready-made
    /// choice when the caller already has this executor's own `Store`.
    pub sink: Arc<dyn tm_browser::session::ArtifactSink>,
}

/// What [`BuiltinExecutor`] needs to register a [`tm_computer::ComputerCapability`] per
/// dispatched task. Unlike [`BrowserWiring`], this is never optional: unlike a browser session, a
/// computer session needs no project-level config file to be worth registering — an unsupported
/// or permission-less backend fails clearly on first tool call
/// (`tm_computer::ComputerError::BackendUnavailable`/`PermissionMissing`) rather than at
/// construction, so there is no "silently no computer tools at all" state to choose here the way
/// there is for a missing `browser.toml`.
#[derive(Clone)]
pub struct ComputerWiring {
    /// Backend selection signals (`WAYLAND_DISPLAY`/`DISPLAY`/`TM_COMPUTER_BACKEND`), mirroring
    /// `tm computer`'s own `SelectionEnv::from_process`.
    pub env: tm_computer::SelectionEnv,
    /// Whether to request a headless (`Xvfb`) session — Linux only, per `SPEC.md` §20.3.
    pub headless: bool,
    /// The panic-stop policy (`SPEC.md` §20.3) attended sessions are configured with. Note this
    /// module's doc comment on why the panic stop is not actually polled on this path yet.
    pub panic_stop: tm_computer::session::PanicStopConfig,
}

impl Default for ComputerWiring {
    /// The same defaults `tm computer`'s CLI dispatcher uses (`crates/tm-cli/src/drive.rs`):
    /// backend selection from the real process environment, attended (not headless), a 5px
    /// panic-stop threshold and no configured abort chord.
    fn default() -> Self {
        ComputerWiring {
            env: tm_computer::SelectionEnv::from_process(),
            headless: false,
            panic_stop: tm_computer::session::PanicStopConfig {
                abort_chord: None,
                mouse_move_threshold_px: 5.0,
            },
        }
    }
}

/// Concrete handles to one call's browser/computer session registries, kept alongside their
/// registration as `Arc<dyn CapabilityProvider>` in the [`ToolRegistry`] so
/// [`Executor::execute`] can tear every session down once the dispatched task ends without
/// downcasting a trait object. See `tm_browser::capability`'s module doc comment for the honest
/// scope of what "torn down" means here: this is task-dispatch-scoped teardown, not true
/// lease-expiry-triggered teardown (`SPEC.md` §19.1b) — nothing in this workspace yet fires a
/// callback when a lease expires mid-task.
struct SessionHandles {
    browser: Option<Arc<tm_browser::BrowserCapability>>,
    computer: Arc<tm_computer::ComputerCapability>,
}

impl SessionHandles {
    async fn close_all(&self) {
        if let Some(browser) = &self.browser {
            if let Err(e) = browser.close_all().await {
                tracing::warn!(
                    error = %e,
                    "failed to close one or more browser sessions after task dispatch"
                );
            }
        }
        if let Err(e) = self.computer.close_all().await {
            tracing::warn!(
                error = %e,
                "failed to close one or more computer sessions after task dispatch"
            );
        }
    }
}

/// Wrap an [`ExecutorTask::context_pack`]'s rendered text into a single-section
/// `tm_context::ContextPack`, since `tm-core` (where `ExecutorTask` lives) cannot depend on
/// `tm-context`. All of `ContextPack`'s and `Section`'s fields are public, so this is a
/// mechanical wrap, not a re-derivation.
fn wrap_context_pack(rendered: &str) -> tm_context::ContextPack {
    let tokens = tm_context::estimate_tokens_prose(rendered);
    let bytes = rendered.len();
    let section = tm_context::Section {
        kind: tm_context::SectionKind::Objective,
        title: "Executor task".to_string(),
        body: rendered.to_string(),
        tokens,
        bytes,
        provenance: Vec::new(),
    };
    tm_context::ContextPack {
        sections: vec![section],
        tokens,
        bytes,
        provenance: Vec::new(),
        dropped: Vec::new(),
    }
}

/// `tm-agent`'s own loop, exposed as a `tm_core::Executor` a dispatcher can drive generically
/// (`SPEC.md` §24.3's `builtin` adapter — "the reference executor; cheapest, most parallel,
/// fully instrumented").
pub struct BuiltinExecutor {
    id: String,
    fabric: Arc<Fabric>,
    ci: Arc<CodeIntel>,
    store: Arc<Store>,
    command_cache: Arc<dyn CommandCache + Send + Sync>,
    command_executor: Arc<dyn CommandExecutor + Send + Sync>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    /// `docs/audit-2026-09-18-fable.md` B-02: `None` when no `browser.toml` is configured.
    browser: Option<BrowserWiring>,
    /// B-02: always present — see [`ComputerWiring`]'s doc comment for why this has no `None`
    /// state the way [`BuiltinExecutor::browser`] does.
    computer: ComputerWiring,
    /// The human-approval policy threaded into every [`AgentLoop`] this executor builds
    /// (`docs/audit-2026-09-18-fable.md` M-16). Always present, like [`BuiltinExecutor::computer`]
    /// — a project with no `oversight.toml` still has a concrete policy,
    /// [`Oversight::default`] (asks nothing), rather than a `None` state to branch on.
    oversight: Oversight,
    /// Threaded into every [`AgentLoop`] this executor builds via [`AgentLoop::with_root`], when
    /// set — see [`BuiltinExecutor::with_root`]. `None` (the default, via [`BuiltinExecutor::new`])
    /// preserves this executor's behavior before `--worktree` existed exactly: every tool call
    /// resolves against the process's own current directory, the same as before.
    root_override: Option<PathBuf>,
    /// Where every run's steps go as they happen (see [`BuiltinExecutor::with_step_sender`]).
    step_sender: Option<tokio::sync::mpsc::UnboundedSender<crate::outcome::StepRecord>>,
    /// Threaded into every [`AgentLoop`] this executor builds via
    /// [`AgentLoop::with_replay_tool_source`], when set — see
    /// [`BuiltinExecutor::with_replay_tool_source`]. `None` (the default) preserves this
    /// executor's behavior before tool replay existed exactly: every tool call dispatches for
    /// real, the same as before.
    replay_tool_source: Option<crate::agent_loop::ReplayToolSource>,
}

impl BuiltinExecutor {
    /// Build a `BuiltinExecutor` identified as `id` (used in the dispatcher's
    /// `agent:<id>/<ticket>` lease holder), sharing the given infrastructure across every
    /// [`Executor::execute`] call. A fresh [`ToolRegistry`], [`AgentLoop`] and pair of
    /// browser/computer session registries are built per call (`ToolRegistry` is not `Clone`, a
    /// fresh loop per run keeps concurrent executions from sharing mutable prompt-cache state,
    /// and a fresh `tm_browser`/`tm_computer` `SessionRegistry` per run is what lets
    /// [`Executor::execute`] tear every session it opened down before returning — see
    /// [`SessionHandles`]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        fabric: Arc<Fabric>,
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        browser: Option<BrowserWiring>,
        computer: ComputerWiring,
        oversight: Oversight,
    ) -> Self {
        BuiltinExecutor {
            id: id.into(),
            fabric,
            ci,
            store,
            command_cache,
            command_executor,
            clock,
            ids,
            browser,
            computer,
            oversight,
            root_override: None,
            step_sender: None,
            replay_tool_source: None,
        }
    }

    /// Send every step of every run this executor drives to `sender` as it happens
    /// ([`AgentLoop::with_step_sender`]), so `tm run` can show a ticket's progress live.
    pub fn with_step_sender(
        mut self,
        sender: tokio::sync::mpsc::UnboundedSender<crate::outcome::StepRecord>,
    ) -> Self {
        self.step_sender = Some(sender);
        self
    }

    /// Replay recorded tool-call resolutions instead of dispatching real tool calls for every run
    /// this executor drives (`replay-tool-replay-mode-design`,
    /// `docs/decisions/D-028-record-replay-harness.md`'s "Tool replay" section) — the plumbing a
    /// future `tm run <ticket> --replay <path>` caller opts a dispatch into, mirroring
    /// [`BuiltinExecutor::with_root`]'s builder shape. `source` is cloned per [`Executor::execute`]
    /// call (see [`crate::agent_loop::ReplayToolSource`]'s own doc comment on why it's `Clone`),
    /// so this executor can be reused across more than one dispatch without a first replay
    /// draining the recording a second one would also need.
    pub fn with_replay_tool_source(mut self, source: crate::agent_loop::ReplayToolSource) -> Self {
        self.replay_tool_source = Some(source);
        self
    }

    /// Point every run this executor drives at `root` instead of the process's own current
    /// directory — the seam `tm-cli`'s `build_dispatcher` uses for `tm run <ticket> --worktree`
    /// (`docs/decisions/D-012-run-worktree-isolation.md`) to run a ticket's `fs.*`/`edit.*`/
    /// `git.*`/`shell.*` tool calls against an isolated `git worktree` checkout rather than the
    /// main working tree. A builder rather than a `new()` parameter so every existing call site
    /// (real and test) is unaffected — the default is `None`, byte-identical to this executor's
    /// behavior before `--worktree` existed.
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root_override = Some(root);
        self
    }

    /// Read back this ticket's most recent [`PriorInvestigation`], if
    /// [`BuiltinExecutor::persist_no_submit_investigation`] ever stored one for it *within the
    /// same round* — `objective` (the current dispatch's own [`ExecutorTask::objective`]) must
    /// match exactly what was recorded. A human's `tm ticket retry --guidance "..."` appends the
    /// guidance onto the ticket's objective (`Store::retry`), so a stale investigation from
    /// before that guidance was given is deliberately invisible to this lookup once the wording
    /// changes: the human just redirected the work, and "stop investigating, you already tried
    /// this" would be actively wrong advice to carry into a round they just re-scoped. This is
    /// the cross-attempt memory `recover-without-repeating-agent-investigation` adds: a fresh
    /// dispatch of the same ticket *in the same round* (an ordinary scheduler retry after a
    /// no-submit attempt) has no other way to learn what the last attempt already tried before
    /// this. Best-effort: a store read failure is treated as "nothing to recall" rather than
    /// propagated, matching [`AgentLoop::ensure_goal_set`]'s own reasoning for why this kind of
    /// lookup must never turn an otherwise-fine dispatch into an infrastructure error.
    ///
    /// [`AgentLoop::ensure_goal_set`]: crate::agent_loop::AgentLoop
    ///
    /// Scoped to the current round by *two* independent signals, both required to match, because
    /// either alone is fragile against a real production path: `objective` alone misses a bare
    /// `tm ticket retry T` with no `--guidance`, which leaves the objective untouched
    /// (`Store::retry`) but always bumps `RetryPolicy::max_attempts` (`t.attempts +
    /// t.retry.max_attempts.max(1)`, every retry, guided or not) — so `max_attempts` alone catches
    /// that case. `max_attempts` alone would instead miss a same-round dispatch whose objective
    /// *did* just change (a guided retry, or a future caller that edits the objective without
    /// bumping `max_attempts`). Requiring both still correctly matches the overwhelmingly common
    /// case — an ordinary automatic scheduler retry within one round changes neither.
    fn prior_investigation(
        &self,
        ticket: &tm_types::TicketId,
        objective: &str,
    ) -> Option<PriorInvestigation> {
        let view = self.store.view().ok()?;
        let max_attempts = view.tickets.get(ticket)?.retry.max_attempts;
        view.artifacts
            .values()
            .filter_map(|artifact| {
                let meta = artifact.meta.as_object()?;
                if meta.get("kind").and_then(|v| v.as_str()) != Some(NO_SUBMIT_ARTIFACT_KIND) {
                    return None;
                }
                if meta.get("no_submit_ticket").and_then(|v| v.as_str()) != Some(ticket.as_str()) {
                    return None;
                }
                if meta.get("objective").and_then(|v| v.as_str()) != Some(objective) {
                    return None;
                }
                if meta.get("max_attempts").and_then(|v| v.as_u64())
                    != Some(u64::from(max_attempts))
                {
                    return None;
                }
                let attempt_count = meta.get("attempt_count")?.as_u64()? as u32;
                let cleared = meta
                    .get("cleared")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let repeated = meta
                    .get("repeated")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let tool_signatures = meta
                    .get("tool_signatures")?
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                let conclusion = meta
                    .get("conclusion")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                Some(PriorInvestigation {
                    summary: InvestigationSummary {
                        tool_signatures,
                        conclusion,
                    },
                    attempt_count,
                    repeated,
                    cleared,
                })
            })
            // The most recent record is the one with the highest `attempt_count`; ties can't
            // occur since each persisted count is one more than the previous one it was compared
            // against. Deliberately the raw winner, `cleared` or not: a caller that only wants to
            // steer a *following* attempt should filter through `PriorInvestigation::effective`
            // first, but the raw winner's own `attempt_count` is what keeps this round's
            // numbering continuous across a clearing (`BuiltinExecutor::mark_investigation_cleared`)
            // — restarting the count at a cleared record would silently disconnect the next
            // no-submit attempt's numbering (and, if it reads `Store::view()` at a moment where
            // `objective`/`max_attempts` still match, its overlap check) from everything before
            // the clearing, which defeats detecting a *later* repeat within the same round.
            .max_by_key(|p| p.attempt_count)
    }

    /// Durably record `summary` as this ticket's latest no-submit attempt within the current
    /// round (see [`BuiltinExecutor::prior_investigation`]'s doc comment for what scopes "current
    /// round"). Best-effort: a store write failure is logged, not propagated — losing this memory
    /// only means the next attempt starts cold, exactly this task's pre-existing behavior, not a
    /// new way for a dispatch to fail.
    fn persist_no_submit_investigation(
        &self,
        ticket: &tm_types::TicketId,
        objective: &str,
        summary: &InvestigationSummary,
        attempt_count: u32,
        repeated: bool,
    ) {
        self.persist_investigation_record(
            ticket,
            objective,
            summary,
            attempt_count,
            repeated,
            false,
        );
    }

    /// Mark this ticket's round as resolved as of `attempt_count`
    /// (`recover-without-repeating-agent-investigation`): a submission landed, so any earlier
    /// no-submit history in this same round (same `objective`/`max_attempts`) must stop being
    /// read back as "this round already repeated" by a later dispatch that follows a human
    /// rejection — see [`BuiltinExecutor::prior_investigation`]'s own doc comment on why
    /// `objective`/`max_attempts` alone can't tell a post-reject redispatch apart from an
    /// ordinary retry.
    fn mark_investigation_cleared(
        &self,
        ticket: &tm_types::TicketId,
        objective: &str,
        attempt_count: u32,
    ) {
        self.persist_investigation_record(
            ticket,
            objective,
            &InvestigationSummary::default(),
            attempt_count,
            false,
            true,
        );
    }

    /// Shared write path for [`BuiltinExecutor::persist_no_submit_investigation`] and
    /// [`BuiltinExecutor::mark_investigation_cleared`] — the only difference between the two is
    /// `cleared`.
    fn persist_investigation_record(
        &self,
        ticket: &tm_types::TicketId,
        objective: &str,
        summary: &InvestigationSummary,
        attempt_count: u32,
        repeated: bool,
        cleared: bool,
    ) {
        let max_attempts = self
            .store
            .view()
            .ok()
            .and_then(|view| view.tickets.get(ticket).map(|t| t.retry.max_attempts));
        let Some(max_attempts) = max_attempts else {
            tracing::warn!(
                ticket = %ticket,
                "failed to read this ticket's retry policy; the next attempt will start cold"
            );
            return;
        };
        let meta = serde_json::json!({
            "kind": NO_SUBMIT_ARTIFACT_KIND,
            "no_submit_ticket": ticket.as_str(),
            "objective": objective,
            "max_attempts": max_attempts,
            "attempt_count": attempt_count,
            "repeated": repeated,
            "cleared": cleared,
            "tool_signatures": summary.tool_signatures,
            "conclusion": summary.conclusion,
        });
        if let Err(e) = self.store.store_artifact(
            ArtifactKind::Report,
            "text/plain".to_string(),
            summary.render().into_bytes(),
            meta,
            Some(ticket.clone()),
            ParticipantId::system(),
        ) {
            tracing::warn!(
                ticket = %ticket,
                error = %e,
                "failed to persist no-submit investigation summary; the next attempt will start cold"
            );
        }
    }

    /// Fold `prior`'s steering note (if any) onto `base` (the caller's already-compiled
    /// [`ExecutorTask::context_pack`] text), so the model driving a following attempt sees what
    /// its predecessor already tried instead of starting cold
    /// (`recover-without-repeating-agent-investigation`). `prior.repeated` escalates the wording
    /// from "here's what was already tried" to an explicit instruction to stop investigating and
    /// either name a diagnosis or ask for a targeted repro, since that attempt's own investigation
    /// already repeated the one before it.
    fn augmented_context(base: &str, prior: Option<&PriorInvestigation>) -> String {
        let Some(prior) = prior else {
            return base.to_string();
        };
        let note = if prior.repeated {
            format!(
                "\n\n# Prior attempts made no progress\n{} consecutive attempts on this ticket \
                 have now ended without calling ticket.submit, repeating the same investigation \
                 each time ({}). Do not repeat those same reads/searches again. Stop \
                 investigating: either state a specific diagnosis of the root cause in your reply, \
                 or ask (in your final reply, without any further tool calls) for a targeted \
                 reproduction case from a human before continuing.",
                prior.attempt_count,
                prior.summary.render()
            )
        } else {
            format!(
                "\n\n# Prior attempt\nA previous attempt on this ticket ended without calling \
                 ticket.submit ({}). Avoid repeating identical reads/searches from that attempt \
                 unless something has changed; build on what it already found, or take a \
                 genuinely different action, rather than starting the investigation over.",
                prior.summary.render()
            )
        };
        format!("{base}{note}")
    }

    /// [`BuiltinExecutor::build`] (test-only; see `mod tests`'s own `impl BuiltinExecutor` block),
    /// taking an already-looked-up [`PriorInvestigation`] instead of
    /// fetching its own — the seam [`Executor::execute`] uses so it can fetch `prior` once and
    /// reuse it both for this attempt's context augmentation and, once the attempt finishes, for
    /// scoring this attempt's own investigation against it.
    fn build_with_prior(
        &self,
        task: &ExecutorTask,
        prior: Option<&PriorInvestigation>,
    ) -> (AgentLoop, AgentTask, SessionHandles) {
        let session = task
            .session
            .clone()
            .unwrap_or_else(|| session_id_from(self.ids.next(IdKind::Session).as_str()));

        let mut extra: Vec<Arc<dyn CapabilityProvider>> = Vec::new();
        let browser_handle = self.browser.as_ref().map(|wiring| {
            let registry = tm_browser::SessionRegistry::new(
                wiring.providers.clone(),
                wiring.sink.clone(),
                self.clock.clone(),
                BROWSER_ARTIFACT_THRESHOLD_BYTES,
            );
            let capability = Arc::new(tm_browser::BrowserCapability::new(registry));
            extra.push(capability.clone() as Arc<dyn CapabilityProvider>);
            capability
        });

        let computer_registry = tm_computer::ComputerSessionRegistry::new(
            self.computer.env.clone(),
            self.computer.headless,
            self.computer.panic_stop.clone(),
        );
        let computer_handle = Arc::new(tm_computer::ComputerCapability::new(computer_registry));
        extra.push(computer_handle.clone() as Arc<dyn CapabilityProvider>);

        // `skill.load` (`docs/audit-2026-09-18-fable.md` M-04): registered here too, not just
        // `tm-cli`'s interactive path, so a scheduler-dispatched worker (`tm run`/`tm sched run`)
        // sees it as well — see `crate::skill_capability`'s module doc comment.
        extra.push(Arc::new(crate::skill_capability::SkillCapability::new())
            as Arc<dyn CapabilityProvider>);

        let tools = ToolRegistry::with_capabilities(
            self.ci.clone(),
            self.store.clone(),
            self.command_cache.clone(),
            self.command_executor.clone(),
            extra,
        );
        // `hooks.toml`, loaded from the project root `self.ci` was opened against (see
        // `tm_codeintel::CodeIntel::project_root`). This method's signature (this trait's
        // `Executor::execute` contract, transitively) is infallible — unlike `tm-cli`'s
        // interactive path (`AgentSession::run_turn_streaming`), which surfaces a malformed
        // `hooks.toml` as a turn-ending `Err` a human sees immediately, there is no `Result`
        // here to propagate one through, so a parse failure logs a warning and falls back to no
        // hooks configured rather than being silently swallowed or requiring a broader signature
        // change to this trait to fix properly.
        let hooks = match crate::hooks::load_hooks_toml(self.ci.project_root()) {
            Ok(hooks) => hooks,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load hooks.toml; proceeding with no hooks configured");
                crate::hooks::HookConfig::default()
            }
        };
        let tools = tools.with_hooks(hooks);
        let mut agent_loop = AgentLoop::new(
            self.fabric.clone(),
            tools,
            // The loop's own ceiling is the ticket's own authority/budget, not root()/
            // unlimited() — see this module's doc comment.
            task.authority.clone(),
            task.budget,
            self.clock.clone(),
            self.ids.clone(),
            task.role,
            task.actor.clone(),
            self.store.clone(),
        )
        .with_oversight(self.oversight.clone());
        if let Some(root) = self.root_override.clone() {
            agent_loop = agent_loop.with_root(root);
        }
        if let Some(sender) = self.step_sender.clone() {
            agent_loop = agent_loop.with_step_sender(sender);
        }
        if let Some(source) = self.replay_tool_source.clone() {
            agent_loop = agent_loop.with_replay_tool_source(source);
        }
        let date = self.clock.now().to_rfc3339();
        let root = agent_loop.root().display().to_string();
        agent_loop = agent_loop.with_prompt_fragments(crate::prompt::worker_fragments(
            &crate::prompt::PromptEnvironment {
                root,
                platform: std::env::consts::OS.to_string(),
                date: date.get(..10).unwrap_or(&date).to_string(),
                scope: String::new(),
                attached_ticket: Some(task.ticket.to_string()),
            },
        ));

        // Steering only ever comes from an *unresolved* chain — a `cleared` record (this round
        // already reached a real submission since its no-submit history) has nothing left to
        // steer this attempt away from.
        let context_text = Self::augmented_context(
            &task.context_pack,
            prior.and_then(PriorInvestigation::effective),
        );
        let agent_task = AgentTask {
            ticket: Some(task.ticket.clone()),
            context_pack: wrap_context_pack(&context_text),
            authority: task.authority.clone(),
            budget: task.budget,
            harness_epoch: task.harness_epoch,
            session,
            conversation: None,
        };

        let handles = SessionHandles {
            browser: browser_handle,
            computer: computer_handle,
        };

        (agent_loop, agent_task, handles)
    }
}

fn session_id_from(rendered: &str) -> SessionId {
    SessionId::new(rendered).unwrap_or_else(|_| {
        SessionId::new("S-0").expect(
            "the literal S-0 matches SessionId's fixed S-<n> numeric format and can never fail",
        )
    })
}

#[async_trait]
impl Executor for BuiltinExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            streaming: false,
            tool_use: true,
            patch_output: false,
            interactive: false,
            accepts_context_pack: true,
            sandboxed: false,
            max_context_tokens: None,
            cost_class: CostClass::Standard,
        }
    }

    async fn execute(&self, task: ExecutorTask) -> tm_types::Result<ExecutorOutcome> {
        let ticket = task.ticket.clone();
        // Looked up once here rather than inside `build` (which does its own lookup for callers
        // that only want the built loop, e.g. this module's own tests): this attempt's own
        // no-submit outcome, below, needs the same `prior` to score itself against.
        let prior = self.prior_investigation(&ticket, &task.objective);
        let (mut agent_loop, agent_task, handles) = self.build_with_prior(&task, prior.as_ref());

        // Tear every browser/computer session this run opened down before returning, on every
        // path — success, failure, or a mid-run `?` — not just the happy path. This is the
        // task-dispatch-scoped teardown `SessionHandles`'s doc comment describes.
        let outcome = agent_loop.run(agent_task).await;
        handles.close_all().await;
        let outcome = outcome?;

        Ok(match outcome {
            AgentOutcome::Submitted { evidence, steps } => {
                // A real submission resolves this round's no-submit chain, if it had one (and
                // isn't already marked resolved) — see `BuiltinExecutor::mark_investigation_cleared`'s
                // doc comment for why this can't just be inferred later from `objective`/
                // `max_attempts` staying the same across a human rejection.
                if let Some(p) = prior.as_ref().and_then(PriorInvestigation::effective) {
                    self.mark_investigation_cleared(&ticket, &task.objective, p.attempt_count + 1);
                }
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: evidence.summary,
                    evidence: evidence.artifacts,
                    patch: None,
                    usage,
                    decisions: evidence.decisions,
                    failure: None,
                }
            }
            AgentOutcome::BudgetExhausted { steps, exhausted } => {
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage,
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure {
                        class: FailureClass::BudgetExhausted,
                        detail: format!("budget exhausted: {exhausted:?}"),
                    }),
                }
            }
            AgentOutcome::AwaitingApproval {
                steps,
                pending_call,
            } => {
                // `BuiltinExecutor::execute` runs to completion or failure, never suspends —
                // there is no synchronous human on the far side of a dispatcher-driven run to
                // resume it. `FailureClass::AuthorityDenied` is the closed-vocabulary class
                // closest to what actually happened (the model's action needed an approval
                // this unattended run could not grant); it is retryable
                // (`FailureClass::is_retryable`), so the scheduler's ordinary retry/escalation
                // path gets a chance to route the ticket to a `human_required` recovery ticket
                // rather than the run silently vanishing. A full mid-run approval hand-off to
                // `HumanExecutor` is out of this scope (`SPEC.md` B-07 owns durable
                // `approval.*` events).
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage,
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure {
                        class: FailureClass::AuthorityDenied,
                        detail: format!(
                            "run suspended awaiting approval for tool {} ({}); no unattended \
                             resolution available",
                            pending_call.tool_name, pending_call.reason
                        ),
                    }),
                }
            }
            AgentOutcome::Replied { steps, .. } | AgentOutcome::Interrupted { steps } => {
                // Unreachable in practice: this executor never sets `AgentTask::conversation`, the
                // only way the loop produces `Replied`. Mapped to the same failure a ticketed run
                // that stops talking without submitting has always produced.
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage,
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure {
                        class: FailureClass::Other,
                        detail: NO_SUBMIT_DETAIL.to_string(),
                    }),
                }
            }
            // The no-submit failure specifically (`recover-without-repeating-agent-investigation`):
            // persist what this attempt itself investigated, so a following dispatch of the same
            // ticket can build on it instead of starting cold, and — once this round has already
            // repeated once — replace the generic detail with a user-actionable one instead of
            // handing the scheduler another identical retry to schedule blindly.
            AgentOutcome::Failed {
                steps,
                class,
                detail,
            } if class == FailureClass::Other && detail == NO_SUBMIT_DETAIL => {
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                let summary = InvestigationSummary::from_steps(&steps);
                // Steering/overlap comparisons only ever look at an *unresolved* chain: a
                // `cleared` winner (this round already reached a real submission since its
                // no-submit history was recorded — e.g. a human rejected it and asked for
                // another attempt) has nothing left to compare this attempt against, and must
                // not resurrect the old `repeated` flag. `attempt_count`, below, still continues
                // from the raw (possibly-cleared) winner: numbering stays a plain count of
                // no-submit attempts in this round, independent of whether the round was ever
                // resolved partway through.
                let effective = prior.as_ref().and_then(PriorInvestigation::effective);
                let overlapped_this_attempt =
                    effective.is_some_and(|p| p.summary.overlaps(&summary));
                // Sticky, not re-derived fresh each time: once a round has been flagged as
                // making no forward progress, a *later* attempt in that same round that heeded
                // the steering in `BuiltinExecutor::augmented_context` and stopped calling tools
                // (e.g. it just names a diagnosis or asks for a repro instead) has nothing left
                // to overlap by definition — `InvestigationSummary::overlaps` always returns
                // `false` once one side has no tool signatures at all. Without this, that
                // attempt would silently fall back to the generic, uninformative detail below,
                // discarding exactly the diagnosis/repro-ask this mechanism exists to surface.
                let repeated = overlapped_this_attempt || effective.is_some_and(|p| p.repeated);
                let attempt_count = prior.as_ref().map_or(1, |p| p.attempt_count + 1);
                self.persist_no_submit_investigation(
                    &ticket,
                    &task.objective,
                    &summary,
                    attempt_count,
                    repeated,
                );
                let detail = if repeated {
                    format!(
                        "no forward progress: attempt {attempt_count} in this round ended \
                         without calling ticket.submit ({}). This needs a targeted reproduction \
                         case or a manual diagnosis rather than another automatic retry",
                        summary.render()
                    )
                } else {
                    detail
                };
                ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage,
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure { class, detail }),
                }
            }
            AgentOutcome::Failed {
                steps,
                class,
                detail,
            } => {
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage,
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure { class, detail }),
                }
            }
        })
    }

    async fn cancel(&self, _handle: &ExecutionHandle) -> tm_types::Result<()> {
        // `AgentLoop` has no in-flight cancellation hook today (a run either completes or is
        // dropped with its future); best-effort no-op until one exists.
        Ok(())
    }
}

/// What a human decided when [`HumanApprovalSink::escalate`] returns `Some`: a submission
/// summary plus whatever evidence the sink already stored on the human's behalf (e.g. via
/// `Store::store_artifact`) — a human executor never submits without evidence any more than a
/// model-backed one does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanDecision {
    /// One-line submission summary, as the human phrased it.
    pub summary: String,
    /// Evidence artifacts already stored for this decision.
    pub evidence: Vec<tm_types::ArtifactId>,
}

/// Asks a human to resolve a ticket and waits for the answer, matching [`Executor`] rather than
/// being special-cased by the dispatcher (`SPEC.md` §24.3's `human` adapter).
#[async_trait]
pub trait HumanApprovalSink: Send + Sync {
    /// Present `task` to a human and wait for their decision. `Ok(Some(decision))` means the
    /// human did the work; `Ok(None)` means the human declined; `Err` is an infrastructure
    /// failure (e.g. no terminal attached).
    async fn escalate(&self, task: &ExecutorTask) -> tm_types::Result<Option<HumanDecision>>;
}

/// The dedicated human executor: `execute` opens an escalation via a [`HumanApprovalSink`] and
/// waits on it, rather than the scheduler special-casing `human_required` tickets.
pub struct HumanExecutor {
    id: String,
    sink: Arc<dyn HumanApprovalSink>,
}

impl HumanExecutor {
    /// Build a human executor that escalates through `sink`.
    pub fn new(id: impl Into<String>, sink: Arc<dyn HumanApprovalSink>) -> Self {
        HumanExecutor {
            id: id.into(),
            sink,
        }
    }
}

#[async_trait]
impl Executor for HumanExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            streaming: false,
            tool_use: false,
            patch_output: false,
            interactive: true,
            accepts_context_pack: true,
            sandboxed: false,
            max_context_tokens: None,
            cost_class: CostClass::Free,
        }
    }

    async fn execute(&self, task: ExecutorTask) -> tm_types::Result<ExecutorOutcome> {
        let ticket = task.ticket.clone();
        match self.sink.escalate(&task).await? {
            Some(decision) => Ok(ExecutorOutcome {
                ticket,
                summary: decision.summary,
                evidence: decision.evidence,
                patch: None,
                usage: tm_types::Spend::default(),
                decisions: Vec::new(),
                failure: None,
            }),
            None => Ok(ExecutorOutcome {
                ticket,
                summary: String::new(),
                evidence: Vec::new(),
                patch: None,
                usage: tm_types::Spend::default(),
                decisions: Vec::new(),
                failure: Some(ExecutorFailure {
                    class: FailureClass::Other,
                    detail: "human declined the escalation".to_string(),
                }),
            }),
        }
    }

    async fn cancel(&self, _handle: &ExecutionHandle) -> tm_types::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_provider::RoleTable;
    use tm_types::{Authority, Budget, CounterIds, FixedClock, ParticipantId, Role, TicketId};

    impl BuiltinExecutor {
        /// Build the [`AgentLoop`], [`AgentTask`] and [`SessionHandles`] `execute` would drive
        /// for `task`, without running anything — the seam a test uses to observe what ceiling
        /// the loop was actually constructed with (see
        /// `builtin_executor_passes_ticket_authority` below). Looks up
        /// [`BuiltinExecutor::prior_investigation`] itself, so a test that only wants the built
        /// loop (and doesn't care about scoring an attempt against it afterward, the way
        /// [`Executor::execute`] does) doesn't have to. Test-only, defined in this `mod tests`
        /// rather than the main `impl BuiltinExecutor` block above: this crate's own
        /// `mise run hygiene` check has a one-way `#[cfg(test)]`/`#[test]` latch per file (see
        /// this repo's own `CLAUDE.md`, "The hygiene checker's `#[cfg(test)]` region tracker is a
        /// one-way latch") — putting a lone `#[cfg(test)]` fn inside the main `impl` block would
        /// have silently exempted everything below it in the file (`build_with_prior`,
        /// `Executor::execute` and all its no-submit logic, `HumanExecutor`) from hygiene's other
        /// checks for the rest of the file.
        fn build(&self, task: &ExecutorTask) -> (AgentLoop, AgentTask, SessionHandles) {
            let prior = self.prior_investigation(&task.ticket, &task.objective);
            self.build_with_prior(task, prior.as_ref())
        }
    }

    struct NoopCommandCache;
    impl CommandCache for NoopCommandCache {
        fn get(&self, _key: &str) -> tm_types::Result<Option<tm_context::CommandResult>> {
            Ok(None)
        }
        fn put(
            &self,
            _key: &str,
            _argv: &[String],
            _exit_code: i32,
            _started: tm_types::Timestamp,
            _completed: tm_types::Timestamp,
            _stdout: &[u8],
            _stderr: &[u8],
        ) -> tm_types::Result<tm_context::CommandResult> {
            Err(tm_types::TmError::Provider(
                "not used in this test".to_string(),
            ))
        }
        fn read_artifact(&self, _id: &tm_types::ArtifactId) -> tm_types::Result<Vec<u8>> {
            Err(tm_types::TmError::Provider(
                "not used in this test".to_string(),
            ))
        }
    }

    struct NoopCommandExecutor;
    impl CommandExecutor for NoopCommandExecutor {
        fn execute(
            &self,
            _spec: &tm_context::CommandSpec,
        ) -> tm_types::Result<tm_context::ExecutionOutcome> {
            Err(tm_types::TmError::Provider(
                "not used in this test".to_string(),
            ))
        }
    }

    fn restricted_authority() -> Authority {
        Authority {
            repository: tm_types::RepoAuthority {
                read: tm_types::PatternSet::parse(["src/**"]).unwrap(),
                write: tm_types::PatternSet::parse(["src/only/**"]).unwrap(),
            },
            ..Authority::none()
        }
    }

    fn test_executor(dir: &std::path::Path) -> BuiltinExecutor {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let fabric = Arc::new(Fabric::new(RoleTable::default_table(), clock.clone()));
        let ci = Arc::new(CodeIntel::open(dir).expect("open codeintel"));
        let store = Arc::new(
            tm_core::Store::open_with(dir, clock.clone(), ids.clone()).expect("open store"),
        );
        BuiltinExecutor::new(
            "test-builtin",
            fabric,
            ci,
            store,
            Arc::new(NoopCommandCache),
            Arc::new(NoopCommandExecutor),
            clock,
            ids,
            None,
            ComputerWiring::default(),
            Oversight::default(),
        )
    }

    #[test]
    fn builtin_executor_constructs_the_loop_with_the_tasks_authority_and_budget_not_root_or_unlimited(
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = test_executor(dir.path());

        let restricted = restricted_authority();
        let scoped_budget = Budget::new(1_000, 0, 60);
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: restricted.clone(),
            budget: scoped_budget,
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, agent_task, _handles) = executor.build(&task);

        assert_eq!(agent_loop.authority(), &restricted);
        assert_ne!(agent_loop.authority(), &Authority::root());
        assert_eq!(agent_loop.budget(), &scoped_budget);
        assert_ne!(agent_loop.budget(), &Budget::unlimited());
        assert_eq!(agent_task.authority, restricted);
        assert_eq!(agent_task.budget, scoped_budget);
    }

    #[test]
    fn with_root_overrides_the_built_loops_root_instead_of_the_process_cwd() {
        // `docs/decisions/D-012-run-worktree-isolation.md`: `tm run <ticket> --worktree` needs
        // `BuiltinExecutor::with_root` to actually reach the `AgentLoop` it constructs, not just
        // be stored and ignored — this is the seam `AgentLoop::root()`'s own doc comment says it
        // exists for.
        let dir = tempfile::tempdir().expect("tempdir");
        let worktree_dir = tempfile::tempdir().expect("worktree tempdir");
        let executor = test_executor(dir.path()).with_root(worktree_dir.path().to_path_buf());
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, _agent_task, _handles) = executor.build(&task);

        assert_eq!(agent_loop.root(), worktree_dir.path());
        assert_ne!(
            agent_loop.root(),
            std::env::current_dir().expect("cwd"),
            "the override must actually take effect, not silently fall back to the process cwd"
        );
    }

    #[test]
    fn with_replay_tool_source_reaches_the_built_loop() {
        // `replay-tool-replay-mode-design`: `BuiltinExecutor::with_replay_tool_source` needs to
        // actually reach the `AgentLoop` it constructs, not just be stored and ignored — mirrors
        // `with_root_overrides_the_built_loops_root_instead_of_the_process_cwd` above for
        // `AgentLoop::has_replay_tool_source`.
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = test_executor(dir.path())
            .with_replay_tool_source(crate::agent_loop::ReplayToolSource::from_steps(&[]));
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, _agent_task, _handles) = executor.build(&task);

        assert!(agent_loop.has_replay_tool_source());
    }

    #[test]
    fn without_with_replay_tool_source_the_built_loop_has_none() {
        // The zero-replay default path must stay byte-identical to this executor's behavior
        // before tool replay existed.
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = test_executor(dir.path());
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, _agent_task, _handles) = executor.build(&task);

        assert!(!agent_loop.has_replay_tool_source());
    }

    #[test]
    fn without_with_root_the_built_loop_falls_back_to_the_process_cwd_unchanged() {
        // The zero-`--worktree` default path must stay byte-identical to this executor's
        // behavior before `with_root` existed.
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = test_executor(dir.path());
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, _agent_task, _handles) = executor.build(&task);

        assert_eq!(
            agent_loop.root(),
            std::env::current_dir().expect("cwd"),
            "with no override, the loop's root must be exactly the process cwd, unchanged"
        );
    }

    #[test]
    fn build_registers_the_computer_capability_and_no_browser_capability_by_default() {
        // `test_executor` passes `browser: None` and the default `ComputerWiring`: this asserts
        // the resulting tool surface reflects exactly that — computer.* tools present,
        // browser.* tools absent — rather than merely that `build` doesn't panic.
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = test_executor(dir.path());
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, _agent_task, handles) = executor.build(&task);
        assert!(handles.browser.is_none());
        let defs = agent_loop.tools().tool_defs();
        assert!(defs.iter().any(|d| d.name == "computer.snapshot"));
        assert!(!defs.iter().any(|d| d.name.starts_with("browser.")));
        // `skill.load` (`docs/audit-2026-09-18-fable.md` M-04): registered on this path too,
        // not just `tm-cli`'s interactive one — see `crate::skill_capability`'s doc comment.
        assert!(defs.iter().any(|d| d.name == "skill.load"));
    }

    // -----------------------------------------------------------------------------------------
    // `recover-without-repeating-agent-investigation`: a no-submit attempt must not send the
    // next dispatch of the same ticket back in cold, and two consecutive no-submit attempts that
    // repeat the same investigation must stop being reported as the same generic failure.
    // -----------------------------------------------------------------------------------------

    use tm_provider::{
        Candidate, Completion, ContentBlock, MockProvider, ModelId, StopReason, Usage,
    };

    /// A [`BuiltinExecutor`] wired to a scripted, in-memory [`MockProvider`] instead of
    /// [`RoleTable::default_table`]'s real candidates — [`test_executor`] can't drive a real
    /// `execute()` call at all (no network, no credentials), so these tests need their own
    /// fabric.
    fn test_executor_with_mock(dir: &std::path::Path) -> (BuiltinExecutor, Arc<MockProvider>) {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, clock.clone()));
        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            clock.clone(),
        ));
        fabric.register_provider(provider.clone());
        let ci = Arc::new(CodeIntel::open(dir).expect("open codeintel"));
        let store = Arc::new(
            tm_core::Store::open_with(dir, clock.clone(), ids.clone()).expect("open store"),
        );
        let executor = BuiltinExecutor::new(
            "test-builtin",
            fabric,
            ci,
            store,
            Arc::new(NoopCommandCache),
            Arc::new(NoopCommandExecutor),
            clock,
            ids,
            None,
            ComputerWiring::default(),
            Oversight::default(),
        );
        (executor, provider)
    }

    /// Seed a real ticket in `executor`'s own store, matching [`no_submit_task`]'s objective —
    /// `execute()` (unlike [`BuiltinExecutor::build`] alone) drives the loop's own
    /// `ensure_goal_set`/`claim_goal_complete` bookkeeping, which needs the ticket to actually
    /// exist rather than being purely synthetic the way `build`-only tests can get away with.
    fn seed_ticket(executor: &BuiltinExecutor) -> TicketId {
        let events = executor
            .store
            .create_ticket(
                tm_core::TicketKind::Work,
                "investigate the flaky test".to_string(),
                None,
                None,
                Authority::root(),
                Vec::new(),
                tm_core::ExecutorRequirements {
                    role: Role::CoderFast,
                    human_required: false,
                    min_capability: tm_types::Tolerance::Preferred,
                },
                Vec::new(),
                Vec::new(),
                tm_core::VerificationPolicy::None,
                Budget::unlimited(),
                tm_core::RetryPolicy {
                    max_attempts: 3,
                    base_delay_seconds: 30,
                    backoff_multiplier: 2.0,
                    max_delay_seconds: 600,
                },
                0,
                ParticipantId::system(),
            )
            .expect("seed ticket");
        events
            .iter()
            .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
            .expect("ticket.created payload")
    }

    const NO_SUBMIT_TEST_OBJECTIVE: &str = "investigate the flaky test";

    fn no_submit_task(ticket: &TicketId) -> ExecutorTask {
        no_submit_task_with_objective(ticket, NO_SUBMIT_TEST_OBJECTIVE)
    }

    fn no_submit_task_with_objective(ticket: &TicketId, objective: &str) -> ExecutorTask {
        ExecutorTask {
            ticket: ticket.clone(),
            role: Role::CoderFast,
            objective: objective.to_string(),
            context_pack: format!("## Objective\n{objective}\n"),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new(format!("agent:test-builtin/{ticket}")).expect("participant"),
            session: None,
        }
    }

    fn tool_use_completion(
        tool_use_id: &str,
        tool_name: &str,
        input: serde_json::Value,
    ) -> Completion {
        Completion {
            model: ModelId::new("mock", "m1"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::ToolUse {
                    id: tool_use_id.to_string(),
                    name: tool_name.to_string(),
                    input,
                }],
                stop_reason: StopReason::ToolUse,
            }],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(0),
            received_at: tm_types::Timestamp::from_unix_nanos(0),
        }
    }

    fn text_only_completion(text: &str) -> Completion {
        Completion {
            model: ModelId::new("mock", "m1"),
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
            latency: std::time::Duration::from_millis(0),
            received_at: tm_types::Timestamp::from_unix_nanos(0),
        }
    }

    /// One dispatch's worth of scripted turns for a run that reads the same file and then ends
    /// its turn twice in a row without calling `ticket.submit` (one nudge, per
    /// `DEFAULT_MAX_SUBMIT_NUDGES`, then the real failure).
    fn no_submit_sequence_reading(path: &str) -> Vec<Completion> {
        vec![
            tool_use_completion("call-1", "fs.read", serde_json::json!({ "path": path })),
            text_only_completion("still looking into it"),
            text_only_completion("still not sure what's wrong"),
        ]
    }

    #[tokio::test]
    async fn a_first_no_submit_attempt_persists_its_investigation_and_reports_the_plain_detail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (executor, provider) = test_executor_with_mock(dir.path());
        let ticket = seed_ticket(&executor);
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));

        let outcome = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("a no-submit run reports a failure");

        assert_eq!(outcome.class, FailureClass::Other);
        // The very first attempt has nothing to compare itself against, so the failure this
        // task's own caller (the scheduler) already understands is untouched — normal retries
        // keep working exactly as before this change.
        assert_eq!(outcome.detail, NO_SUBMIT_DETAIL);

        // But the attempt's own investigation is now durable, for the *next* dispatch to read
        // back.
        let prior = executor
            .prior_investigation(&ticket, NO_SUBMIT_TEST_OBJECTIVE)
            .expect("a no-submit attempt persists an investigation summary");
        assert_eq!(prior.attempt_count, 1);
        assert!(!prior.repeated);
        assert!(
            prior
                .summary
                .tool_signatures
                .iter()
                .any(|s| s.contains("crates/foo/src/walk.rs")),
            "expected the read path to survive into the persisted summary: {:?}",
            prior.summary.tool_signatures
        );
    }

    #[tokio::test]
    async fn a_second_dispatch_of_the_same_ticket_is_told_what_the_first_already_tried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (executor, provider) = test_executor_with_mock(dir.path());
        let ticket = seed_ticket(&executor);

        // Attempt 1: reads a file, never submits.
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");

        // Attempt 2, a fresh dispatch of the same ticket (exactly what a scheduler retry does):
        // its own request must already carry a note about what attempt 1 tried, not start cold.
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");

        let calls = provider.call_log();
        let first_request_of_attempt_two = &calls[3];
        let task_text: String = first_request_of_attempt_two
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            task_text.contains("Prior attempt"),
            "expected attempt 2's own request to be told about attempt 1's investigation: \
             {task_text}"
        );
        assert!(
            task_text.contains("crates/foo/src/walk.rs"),
            "expected the specific prior investigation to be named, not just a generic notice: \
             {task_text}"
        );
    }

    #[tokio::test]
    async fn a_second_consecutive_no_submit_attempt_that_repeats_the_first_gets_an_actionable_detail(
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (executor, provider) = test_executor_with_mock(dir.path());
        let ticket = seed_ticket(&executor);

        // Attempt 1: reads a file, never submits.
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");

        // Attempt 2 reads the exact same file and also never submits -- the real, previously
        // observed failure mode (BurntSushi/ripgrep-3376 et al.): a fresh attempt with no memory
        // of the last one repeats the same investigation and burns another full run for nothing.
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        let outcome = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");

        assert_ne!(
            outcome.detail, NO_SUBMIT_DETAIL,
            "a second attempt that repeated the first's own investigation must not be reported \
             as the same generic, uninformative failure"
        );
        assert!(
            outcome.detail.contains("no forward progress"),
            "expected a user-actionable explanation naming the lack of progress: {}",
            outcome.detail
        );
        assert!(
            outcome.detail.contains("crates/foo/src/walk.rs"),
            "expected the explanation to name what was actually repeated: {}",
            outcome.detail
        );

        // Still `FailureClass::Other` and still retryable at the type level: the scheduler's
        // ordinary retry/escalation machinery is untouched by this change, only the message it
        // has to show a human is better.
        assert_eq!(outcome.class, FailureClass::Other);
    }

    #[tokio::test]
    async fn a_second_attempt_that_investigates_something_different_is_not_flagged_as_repeated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (executor, provider) = test_executor_with_mock(dir.path());
        let ticket = seed_ticket(&executor);

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");

        // A genuinely different investigation (a different file entirely) must not be treated as
        // "the same one repeated" just because it also ended without a submission.
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/dir.rs"));
        let outcome = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");

        assert_eq!(
            outcome.detail, NO_SUBMIT_DETAIL,
            "different investigations must not be reported as repeated no-progress"
        );
    }

    #[tokio::test]
    async fn a_third_attempt_that_heeds_the_steering_and_stops_calling_tools_still_gets_the_actionable_detail(
    ) {
        // The steering text `BuiltinExecutor::augmented_context` sends once a round is flagged
        // `repeated` explicitly tells the model to stop calling tools and just name a diagnosis
        // or ask for a repro. If it does exactly that, its own investigation has no tool
        // signatures at all, so it can never "overlap" anything by
        // `InvestigationSummary::overlaps`'s own definition -- this attempt must still be
        // reported as no forward progress, carrying its own conclusion, rather than silently
        // falling back to the generic detail and losing what the model just said.
        let dir = tempfile::tempdir().expect("tempdir");
        let (executor, provider) = test_executor_with_mock(dir.path());
        let ticket = seed_ticket(&executor);

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");

        // Attempt 3: no tool calls at all, just two text-only turns (one nudge, then the real
        // failure) stating a diagnosis instead of investigating further.
        provider.script_sequence(vec![
            text_only_completion("still thinking"),
            text_only_completion(
                "diagnosis: walk.rs's ignore-pattern cache is never invalidated on rename",
            ),
        ]);
        let outcome = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");

        assert_ne!(
            outcome.detail, NO_SUBMIT_DETAIL,
            "a third attempt in an already-repeated round must stay actionable even once it \
             stops calling tools"
        );
        assert!(
            outcome.detail.contains("no forward progress"),
            "expected the sticky, actionable detail: {}",
            outcome.detail
        );
        assert!(
            outcome
                .detail
                .contains("ignore-pattern cache is never invalidated"),
            "expected the model's own diagnosis to survive into the failure detail: {}",
            outcome.detail
        );
    }

    #[tokio::test]
    async fn a_human_guided_retry_that_changes_the_objective_is_not_told_about_the_stale_round() {
        // `tm ticket retry --guidance "..."` appends guidance onto the ticket's objective
        // (`Store::retry`) before the ticket is dispatched again. A prior investigation
        // recorded under the *old* objective must not bleed into this new, human-redirected
        // round: "stop investigating, you already tried this" would be actively wrong advice
        // once a human just told the ticket to try something different.
        let dir = tempfile::tempdir().expect("tempdir");
        let (executor, provider) = test_executor_with_mock(dir.path());
        let ticket = seed_ticket(&executor);

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");
        assert!(
            executor
                .prior_investigation(&ticket, NO_SUBMIT_TEST_OBJECTIVE)
                .is_some(),
            "sanity: the first round's investigation is recorded under its own objective"
        );

        let guided_objective = format!(
            "{NO_SUBMIT_TEST_OBJECTIVE}\n\n(guidance from a human retry: check dir.rs instead)"
        );
        assert!(
            executor
                .prior_investigation(&ticket, &guided_objective)
                .is_none(),
            "a differently-worded (human-guided) round must not see the old round's investigation"
        );

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        let outcome = executor
            .execute(no_submit_task_with_objective(&ticket, &guided_objective))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");

        assert_eq!(
            outcome.detail, NO_SUBMIT_DETAIL,
            "a fresh, human-redirected round must not inherit the stale round's repeated flag"
        );
    }

    /// Drive `ticket` (already `Ready`, per [`seed_ticket`]) through the real
    /// `Store::acquire_lease`/`Store::record_failure` cycle until it escalates — the same recipe
    /// `tm_core::store::tests::invariant_5_repeated_failure_escalates_instead_of_looping` uses —
    /// so the round-scoping tests below exercise the real `Store::retry` this task's
    /// `BuiltinExecutor::prior_investigation` doc comment claims to be robust against, not a
    /// hand-built stand-in for it.
    fn escalate(executor: &BuiltinExecutor, ticket: &TicketId) {
        for _ in 0..10 {
            executor
                .store
                .acquire_lease(
                    ticket,
                    ParticipantId::system(),
                    Authority::none(),
                    Vec::new(),
                    60,
                    ParticipantId::system(),
                )
                .expect("acquire lease");
            executor
                .store
                .record_failure(
                    ticket,
                    FailureClass::ExecutorCrash,
                    "worker died".to_string(),
                    ParticipantId::system(),
                )
                .expect("record failure");
            let state = executor
                .store
                .view()
                .expect("view")
                .tickets
                .get(ticket)
                .expect("ticket")
                .state;
            if state == tm_core::TicketState::Escalated {
                return;
            }
            if state == tm_core::TicketState::Recovery {
                executor
                    .store
                    .transition(
                        ticket,
                        tm_core::Trigger::RetryScheduled,
                        ParticipantId::system(),
                    )
                    .expect("recovery back to ready");
            }
        }
        panic!("ticket never escalated within 10 rounds");
    }

    #[tokio::test]
    async fn a_bare_retry_with_no_guidance_still_starts_a_fresh_round() {
        // `tm ticket retry T` with no `--guidance` at all leaves the ticket's objective
        // byte-identical (`Store::retry` only touches `fields["objective"]` when guidance is
        // given), so the objective-only half of the round key can't tell this apart from an
        // ordinary automatic scheduler retry. `RetryPolicy::max_attempts`, which `Store::retry`
        // always bumps, is what has to catch it instead.
        let dir = tempfile::tempdir().expect("tempdir");
        let (executor, provider) = test_executor_with_mock(dir.path());
        let ticket = seed_ticket(&executor);
        executor.store.activate(&ticket, actor_for_test()).unwrap();

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        let first_round_second_attempt = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");
        assert!(
            first_round_second_attempt
                .detail
                .contains("no forward progress"),
            "sanity: this round is flagged repeated before the retry: {}",
            first_round_second_attempt.detail
        );

        escalate(&executor, &ticket);
        let human: ParticipantId = "human:owner".parse().unwrap();
        executor
            .store
            .retry(&ticket, None, human)
            .expect("bare retry with no guidance");

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        let after_retry = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");

        assert_eq!(
            after_retry.detail, NO_SUBMIT_DETAIL,
            "a bare retry (no guidance) must still start a fresh round, even though it leaves \
             the objective untouched: {}",
            after_retry.detail
        );
    }

    #[tokio::test]
    async fn a_rejected_resubmission_does_not_inherit_a_stale_repeated_flag() {
        // no-submit, no-submit (repeated), then a real submission, then a human rejection and a
        // fresh dispatch: `Store::reject`'s `Recovery -> Ready` path changes neither the
        // objective nor `RetryPolicy::max_attempts`, so without an explicit "this round already
        // resolved" marker the post-reject dispatch would incorrectly inherit the earlier
        // no-submit chain's `repeated` flag.
        let dir = tempfile::tempdir().expect("tempdir");
        let (executor, provider) = test_executor_with_mock(dir.path());
        let ticket = seed_ticket(&executor);
        executor.store.activate(&ticket, actor_for_test()).unwrap();

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        let repeated = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");
        assert!(
            repeated.detail.contains("no forward progress"),
            "sanity: this round is flagged repeated before the submission: {}",
            repeated.detail
        );

        // `ticket.submit` needs real evidence (`Store::submit` refuses empty evidence) and the
        // ticket to already be `Running` (the scheduler's own `acquire_lease`/`WorkStarted` pair
        // before `Executor::execute`, per `tm_scheduler::dispatch`) -- neither of which the
        // no-submit attempts above needed, since they never called `ticket.submit` at all.
        let artifact_events = executor
            .store
            .store_artifact(
                ArtifactKind::Report,
                "text/plain".to_string(),
                b"looks fixed".to_vec(),
                serde_json::json!({}),
                Some(ticket.clone()),
                ParticipantId::system(),
            )
            .expect("store evidence artifact");
        let artifact_id = artifact_events
            .iter()
            .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
            .expect("artifact.created payload");
        executor
            .store
            .acquire_lease(
                &ticket,
                ParticipantId::system(),
                Authority::root(),
                Vec::new(),
                60,
                ParticipantId::system(),
            )
            .expect("acquire lease");
        executor
            .store
            .transition(
                &ticket,
                tm_core::Trigger::WorkStarted,
                ParticipantId::system(),
            )
            .expect("work started");

        provider.script_sequence(vec![tool_use_completion(
            "call-submit",
            "ticket.submit",
            serde_json::json!({ "summary": "fixed it", "evidence": [artifact_id.as_str()] }),
        )]);
        let submitted = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes");
        assert!(
            submitted.failure.is_none(),
            "expected a real submission: {:?}",
            submitted.failure
        );

        let max_attempts_before_reject = executor.store.view().unwrap().tickets[&ticket]
            .retry
            .max_attempts;

        let human: ParticipantId = "human:owner".parse().unwrap();
        executor
            .store
            .reject(&ticket, "wrong approach".to_string(), human)
            .expect("reject");
        // `Store::reject` drives the ticket all the way back to `Ready` on its own (`Submitted
        // -> Verifying -> Recovery -> Ready`), touching neither `objective` nor
        // `RetryPolicy::max_attempts` along the way (unlike `Store::retry`) -- confirmed here so
        // the assertions below are known to be exercising the `cleared` marker itself, not some
        // other side effect of `reject` that happens to also reset the round.
        let after_reject_view = executor.store.view().unwrap();
        assert_eq!(
            after_reject_view.tickets[&ticket].state,
            tm_core::TicketState::Ready,
            "sanity: reject returns the ticket straight to Ready"
        );
        assert_eq!(
            after_reject_view.tickets[&ticket].retry.max_attempts, max_attempts_before_reject,
            "sanity: reject must not itself bump max_attempts the way Store::retry does"
        );
        assert_eq!(
            after_reject_view.tickets[&ticket].objective, NO_SUBMIT_TEST_OBJECTIVE,
            "sanity: reject must not itself touch the objective the way a guided retry does"
        );

        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        let after_reject = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");
        assert_eq!(
            after_reject.detail, NO_SUBMIT_DETAIL,
            "a dispatch following a rejected-and-resubmitted ticket must not inherit the earlier \
             chain's repeated flag: {}",
            after_reject.detail
        );

        // Memory must also actually *recover* after the reset, not just avoid a false positive
        // once: a second no-submit attempt reading the same file again should still be caught.
        provider.script_sequence(no_submit_sequence_reading("crates/foo/src/walk.rs"));
        let repeated_again = executor
            .execute(no_submit_task(&ticket))
            .await
            .expect("execute completes")
            .failure
            .expect("still no submission");
        assert!(
            repeated_again.detail.contains("no forward progress"),
            "expected the post-reject round to start remembering again, not stay permanently \
             reset: {}",
            repeated_again.detail
        );
    }

    fn actor_for_test() -> ParticipantId {
        ParticipantId::system()
    }
}
