//! The stateful task session and the continuous Jev loop.
//!
//! One call to [`Session::run`] executes many browser actions. It returns only when something
//! genuinely needs a human-grade decision — a value the runtime cannot resolve, real ambiguity,
//! a consequential action, a budget, or the end of the task. That is the entire point of the
//! project: the host model plans and verifies, it does not sit in the click loop.

use crate::action_space::ActionSpace;
use crate::browser::BrowserBackend;
use crate::config::RuntimeConfig;
use crate::error::{RelayError, Result};
use crate::history::{ActionRecord, Bounded, MAX_ACTION_HISTORY, MAX_PAGE_STATES};
use crate::jev::{self, Decision, JevTransport};
use crate::metrics::Metrics;
use crate::safety::{ConsequenceFinding, SafetyPolicy};
use crate::snapshot::{ActionKind, FieldDescription, Operation, RawAction, Snapshot};
use crate::value_pool::{Resolution, ValuePool, ValueSource};
use crate::verification::{self, VerificationReport};
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Ready,
    Running,
    NeedsInput,
    NeedsReasoning,
    NeedsConfirmation,
    Done,
    Blocked,
    Error,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::NeedsInput => "needs_input",
            Self::NeedsReasoning => "needs_reasoning",
            Self::NeedsConfirmation => "needs_confirmation",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }

    /// Can `run` be called?
    pub fn is_runnable(self) -> bool {
        matches!(self, Self::Ready | Self::Running)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Blocked | Self::Error)
    }
}

/// Why the loop stopped. Every variant is actionable by the host.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunOutcome {
    Done {
        verification: VerificationReport,
        steps: u32,
    },
    NeedsInput {
        request_id: String,
        field: FieldDescription,
        question: String,
        /// The keys the runtime already holds, redacted — so the host can see what it tried.
        known_keys: Vec<String>,
    },
    NeedsReasoning {
        request_id: String,
        reason: String,
        page_summary: String,
        candidate_actions: Vec<CandidateAction>,
    },
    NeedsConfirmation {
        request_id: String,
        action: String,
        consequence: String,
        question: String,
        page_summary: String,
    },
    Blocked {
        reason: String,
    },
    BudgetExceeded {
        reason: String,
        steps: u32,
        elapsed_ms: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

impl RunOutcome {
    pub fn status(&self) -> SessionStatus {
        match self {
            Self::Done { .. } => SessionStatus::Done,
            Self::NeedsInput { .. } => SessionStatus::NeedsInput,
            Self::NeedsReasoning { .. } => SessionStatus::NeedsReasoning,
            Self::NeedsConfirmation { .. } => SessionStatus::NeedsConfirmation,
            Self::Blocked { .. } => SessionStatus::Blocked,
            // A budget stop is a pause, not a failure: the host may simply call `run` again.
            Self::BudgetExceeded { .. } => SessionStatus::Ready,
            Self::Error { .. } => SessionStatus::Error,
        }
    }

    /// Does this outcome require the host model to think before the task can continue?
    pub fn is_host_intervention(&self) -> bool {
        matches!(self, Self::NeedsInput { .. } | Self::NeedsReasoning { .. } | Self::NeedsConfirmation { .. })
    }
}

/// One plausible action offered to the host during `NEEDS_REASONING`.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateAction {
    pub index: String,
    pub label: String,
    pub operation: String,
    pub probability: f64,
}

/// What the loop is waiting for the host to supply.
#[derive(Debug, Clone)]
enum Pending {
    // After any intervention the loop re-observes and re-decides, so a pending request carries
    // only what the host's answer needs — never a stale action id.
    Input { request_id: String, field: FieldDescription },
    Reasoning { request_id: String },
    Confirmation { request_id: String, action_id: String, generation: u64, finding: ConsequenceFinding },
}

impl Pending {
    fn request_id(&self) -> &str {
        match self {
            Self::Input { request_id, .. }
            | Self::Reasoning { request_id, .. }
            | Self::Confirmation { request_id, .. } => request_id,
        }
    }
}

/// Budgets for one `run` call.
#[derive(Debug, Clone, Copy)]
pub struct RunBudget {
    pub max_steps: u32,
    pub max_duration_ms: u64,
}

impl RunBudget {
    pub fn from_config(config: &RuntimeConfig) -> Self {
        Self { max_steps: config.default_max_steps, max_duration_ms: config.default_max_duration_ms }
    }
}

pub struct Session {
    pub id: String,
    pub goal: String,
    pub initial_url: String,
    pub status: SessionStatus,

    pub value_pool: ValuePool,
    pub metrics: Metrics,
    config: RuntimeConfig,
    safety: SafetyPolicy,

    jev: Arc<dyn JevTransport>,
    browser: Box<dyn BrowserBackend>,

    snapshot: Option<Snapshot>,
    generation: u64,
    action_history: Bounded<ActionRecord>,
    recent_page_states: Bounded<String>,

    pending: Option<Pending>,
    /// Guidance supplied by the host, consumed by the next Jev request.
    host_guidance: Option<String>,
    /// A confirmed action that may execute exactly once.
    approved_action: Option<(String, u64)>,

    steps: u32,
    consecutive_no_change: u32,
    consecutive_stale: u32,
    consecutive_jev_failures: u32,

    started_at: Instant,
    last_activity: Instant,
    last_verification: Option<VerificationReport>,
}

impl Session {
    pub async fn start(
        id: String,
        goal: String,
        context: Option<serde_json::Value>,
        jev: Arc<dyn JevTransport>,
        mut browser: Box<dyn BrowserBackend>,
        config: RuntimeConfig,
    ) -> Result<Self> {
        if goal.trim().is_empty() {
            return Err(RelayError::InvalidArgument("goal must not be empty".into()));
        }

        let mut value_pool = ValuePool::new();
        value_pool.threshold = config.value_confidence_threshold;
        if let Some(context) = &context {
            value_pool.load_context(context);
        }

        let observed_at = Instant::now();
        let mut snapshot = browser.observe().await?;
        let mut metrics = Metrics::default();
        metrics.record_snapshot(observed_at.elapsed().as_millis() as u64);
        snapshot.generation = 1;
        value_pool.load_session_facts(&snapshot.url, &snapshot.title, &config.today);

        let initial_url = snapshot.url.clone();
        info!(
            session = %id,
            url = %initial_url,
            values = value_pool.len(),
            actions = snapshot.actions.len(),
            "session started"
        );

        Ok(Self {
            id,
            goal,
            initial_url,
            status: SessionStatus::Ready,
            value_pool,
            metrics,
            safety: SafetyPolicy {
                enabled: config.safety_enabled,
                extra_phrases: config.extra_consequential_phrases.clone(),
            },
            config,
            jev,
            browser,
            snapshot: Some(snapshot),
            generation: 1,
            action_history: Bounded::new(MAX_ACTION_HISTORY),
            recent_page_states: Bounded::new(MAX_PAGE_STATES),
            pending: None,
            host_guidance: None,
            approved_action: None,
            steps: 0,
            consecutive_no_change: 0,
            consecutive_stale: 0,
            consecutive_jev_failures: 0,
            started_at: Instant::now(),
            last_activity: Instant::now(),
            last_verification: None,
        })
    }

    pub fn current_url(&self) -> &str {
        self.snapshot.as_ref().map(|s| s.url.as_str()).unwrap_or(&self.initial_url)
    }

    pub fn idle_ms(&self) -> u64 {
        self.last_activity.elapsed().as_millis() as u64
    }

    pub fn history(&self) -> Vec<ActionRecord> {
        self.action_history.to_vec()
    }

    pub fn verification(&self) -> Option<&VerificationReport> {
        self.last_verification.as_ref()
    }

    /// The continuous loop. Runs many browser actions per call.
    pub async fn run(&mut self, budget: RunBudget) -> RunOutcome {
        if !self.status.is_runnable() {
            let outcome = RunOutcome::Error {
                code: "wrong_state".into(),
                message: format!(
                    "session is {}; answer the pending request before calling run",
                    self.status.as_str()
                ),
            };
            return outcome;
        }

        // `run` itself is a host round trip; interventions add more.
        self.metrics.host_round_trips += 1;
        self.status = SessionStatus::Running;
        let run_started = Instant::now();
        let mut steps_this_run = 0u32;

        let outcome = loop {
            if steps_this_run >= budget.max_steps {
                break RunOutcome::BudgetExceeded {
                    reason: format!("reached max_steps ({})", budget.max_steps),
                    steps: steps_this_run,
                    elapsed_ms: run_started.elapsed().as_millis() as u64,
                };
            }
            let elapsed = run_started.elapsed().as_millis() as u64;
            if elapsed >= budget.max_duration_ms {
                break RunOutcome::BudgetExceeded {
                    reason: format!("reached max_duration_ms ({})", budget.max_duration_ms),
                    steps: steps_this_run,
                    elapsed_ms: elapsed,
                };
            }
            if self.steps >= self.config.max_session_steps {
                break RunOutcome::Blocked {
                    reason: format!("session step ceiling reached ({})", self.config.max_session_steps),
                };
            }

            match self.step().await {
                Ok(StepOutcome::Continued) => {
                    steps_this_run += 1;
                    self.last_activity = Instant::now();
                }
                Ok(StepOutcome::Retried) => {
                    // A stale page or a recoverable Jev failure. Costs no step budget, but the
                    // retry budget is what stops this spinning.
                    self.last_activity = Instant::now();
                }
                Ok(StepOutcome::Stopped(outcome)) => break outcome,
                Err(error) => {
                    warn!(session = %self.id, error = %error, "step failed");
                    break RunOutcome::Error { code: error.code().into(), message: error.to_string() };
                }
            }
        };

        self.metrics.total_task_ms = self.started_at.elapsed().as_millis() as u64;
        self.status = outcome.status();
        if outcome.is_host_intervention() {
            match &outcome {
                RunOutcome::NeedsInput { .. } => self.metrics.host_input_requests += 1,
                RunOutcome::NeedsReasoning { .. } => self.metrics.host_reasoning_requests += 1,
                RunOutcome::NeedsConfirmation { .. } => self.metrics.host_confirmations += 1,
                _ => {}
            }
        }
        info!(
            session = %self.id,
            status = self.status.as_str(),
            steps = steps_this_run,
            browser_actions = self.metrics.browser_actions,
            host_round_trips = self.metrics.host_round_trips,
            "run finished"
        );
        outcome
    }

    async fn step(&mut self) -> Result<StepOutcome> {
        // 1. Observe — only if what we hold is no longer accurate.
        let snapshot = self.ensure_fresh_snapshot().await?;
        let space = ActionSpace::build(&snapshot);

        if space.is_empty() {
            return Ok(StepOutcome::Stopped(RunOutcome::Blocked {
                reason: "the page offers no supported action".into(),
            }));
        }

        // 2. Decide.
        let decision = match self.decide(&snapshot, &space).await {
            Ok(decision) => {
                self.consecutive_jev_failures = 0;
                decision
            }
            Err(error) if error.is_recoverable() => {
                self.consecutive_jev_failures += 1;
                self.metrics.retries += 1;
                if self.consecutive_jev_failures > self.config.max_jev_retries {
                    return Err(RelayError::RetryBudget(format!(
                        "{} consecutive recoverable Jev failures; last: {error}",
                        self.consecutive_jev_failures
                    )));
                }
                debug!(session = %self.id, attempt = self.consecutive_jev_failures, error = %error, "retrying jev");
                return Ok(StepOutcome::Retried);
            }
            Err(error) => return Err(error),
        };

        // 3. Terminal operations.
        match decision.operation {
            Operation::Done => return self.finish_done(&snapshot).await.map(StepOutcome::Stopped),
            Operation::Blocked => {
                return Ok(StepOutcome::Stopped(RunOutcome::Blocked {
                    reason: "the policy reports no supported operation can make progress".into(),
                }))
            }
            _ => {}
        }

        // 4. Escalate to the host when the decision is genuinely ambiguous.
        if let Some(reason) = self.ambiguity_reason(&decision) {
            return Ok(StepOutcome::Stopped(self.request_reasoning(reason, &snapshot, &space, &decision)));
        }

        // 5. Back and page controls need no element.
        if decision.operation == Operation::Back {
            let started = Instant::now();
            self.browser.back().await?;
            self.metrics.record_browser_action(started.elapsed().as_millis() as u64);
            self.record_action(&decision, "Back", None, None, started, &snapshot);
            self.invalidate_snapshot();
            return Ok(StepOutcome::Continued);
        }

        let action_id = decision
            .action_id
            .clone()
            .ok_or_else(|| RelayError::JevInvalidResponse("operation resolved to no action".into()))?;
        let action = snapshot
            .action(&action_id)
            .ok_or_else(|| RelayError::StalePage(format!("action {action_id} is no longer present")))?
            .clone();

        // 6. Safety gate — before any mutation, before any value resolution.
        if !crate::safety::always_safe(decision.operation)
            && !self.is_approved(&action_id, decision.generation)
        {
            if let Some(finding) = self.safety.classify(&action, &snapshot) {
                return Ok(StepOutcome::Stopped(self.request_confirmation(
                    finding,
                    &action,
                    decision.generation,
                    &snapshot,
                )));
            }
        }

        // 7. Resolve text locally, or pause for the host.
        let mut resolution: Option<Resolution> = None;
        if action.kind == ActionKind::Fill {
            let mut field = action.describe();
            field.element_index = decision.target.clone();
            match self.value_pool.resolve(&field) {
                Some(found) => {
                    self.metrics.values_resolved_locally += 1;
                    debug!(
                        session = %self.id,
                        key = %found.key,
                        score = found.score,
                        rationale = %found.rationale,
                        "resolved field value locally"
                    );
                    resolution = Some(found);
                }
                None => {
                    return Ok(StepOutcome::Stopped(self.request_input(field)));
                }
            }
        }

        // 8. Execute, with a final freshness check.
        self.execute_action(&decision, &action, resolution, &snapshot).await
    }

    /// Return a snapshot that is known to describe the live page.
    async fn ensure_fresh_snapshot(&mut self) -> Result<Snapshot> {
        if let Some(existing) = &self.snapshot {
            if self.browser.is_fresh(existing, None).await? {
                return Ok(existing.clone());
            }
        }
        self.observe().await
    }

    async fn observe(&mut self) -> Result<Snapshot> {
        let started = Instant::now();
        let mut snapshot = self.browser.observe().await?;
        self.metrics.record_snapshot(started.elapsed().as_millis() as u64);
        self.generation += 1;
        snapshot.generation = self.generation;
        self.value_pool.load_session_facts(&snapshot.url, &snapshot.title, &self.config.today);
        self.recent_page_states.push(snapshot.summary(600));
        self.snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }

    fn invalidate_snapshot(&mut self) {
        self.snapshot = None;
    }

    async fn decide(&mut self, snapshot: &Snapshot, space: &ActionSpace) -> Result<Decision> {
        let request = jev::build_request(
            &self.config.jev_model,
            snapshot,
            space,
            &self.goal,
            &self.action_history.to_vec(),
            self.host_guidance.as_deref(),
        );
        let started = Instant::now();
        let response = self.jev.post(&request).await?;
        let latency = started.elapsed().as_millis() as u64;
        self.metrics.record_jev(latency);
        // Guidance steers exactly one decision; leaving it in place would re-bias every later
        // step against a page the host never saw.
        self.host_guidance = None;
        jev::validate_response(&response, space, snapshot.generation, latency)
    }

    /// Does this decision need the host's judgement rather than Jev's?
    ///
    /// This is an escape hatch, not normal execution: only real signals of trouble trigger it.
    fn ambiguity_reason(&self, decision: &Decision) -> Option<String> {
        if !self.config.reasoning_enabled {
            return None;
        }
        let confidence = decision.effective_confidence();
        if confidence < self.config.reasoning_confidence_threshold {
            return Some(format!(
                "policy confidence {confidence:.2} is below the {:.2} threshold",
                self.config.reasoning_confidence_threshold
            ));
        }
        if let Some(margin) = decision.target_margin() {
            if margin < self.config.reasoning_margin_threshold {
                return Some(format!(
                    "several targets are near-equally plausible (top-two margin {margin:.2})"
                ));
            }
        }
        if self.consecutive_no_change >= self.config.no_change_reasoning_threshold {
            return Some(format!("the last {} actions did not change the page", self.consecutive_no_change));
        }
        None
    }

    async fn execute_action(
        &mut self,
        decision: &Decision,
        action: &RawAction,
        resolution: Option<Resolution>,
        snapshot: &Snapshot,
    ) -> Result<StepOutcome> {
        // The decision must still refer to this page. Between observation and here we may have
        // made a network call, so this check is not redundant with the one in `step`.
        if decision.generation != snapshot.generation {
            self.metrics.stale_decisions += 1;
            self.invalidate_snapshot();
            return Ok(StepOutcome::Retried);
        }
        if !self.browser.is_fresh(snapshot, Some(action)).await? {
            self.metrics.stale_decisions += 1;
            self.consecutive_stale += 1;
            if self.consecutive_stale > self.config.max_stale_retries {
                return Err(RelayError::RetryBudget(format!(
                    "{} consecutive stale decisions; the page will not settle",
                    self.consecutive_stale
                )));
            }
            debug!(session = %self.id, "rejected stale decision, re-observing");
            self.invalidate_snapshot();
            return Ok(StepOutcome::Retried);
        }
        self.consecutive_stale = 0;

        let text = resolution.as_ref().map(|r| r.value.clone());
        let started = Instant::now();
        match self.browser.execute(action, snapshot, text.as_deref()).await {
            Ok(()) => {}
            Err(RelayError::StalePage(reason)) => {
                self.metrics.stale_decisions += 1;
                self.consecutive_stale += 1;
                if self.consecutive_stale > self.config.max_stale_retries {
                    return Err(RelayError::RetryBudget(format!(
                        "executor kept rejecting stale targets: {reason}"
                    )));
                }
                self.invalidate_snapshot();
                return Ok(StepOutcome::Retried);
            }
            Err(error) => return Err(error),
        }
        let action_ms = started.elapsed().as_millis() as u64;
        self.metrics.record_browser_action(action_ms);
        self.steps += 1;
        self.approved_action = None;

        if let Some(found) = &resolution {
            self.value_pool.mark_used(&found.key);
        }

        // Log the action before observing: a navigation during settle must not erase the fact
        // that we executed.
        self.record_action(
            decision,
            action.element_label(),
            resolution.as_ref(),
            Some(action.kind),
            started,
            snapshot,
        );

        let waited = self.browser.settle(action).await.unwrap_or(0);
        self.metrics.wait_ms += waited;

        let previous_marker = snapshot.marker.clone();
        self.invalidate_snapshot();
        let fresh = self.observe().await?;
        let changed = fresh.marker != previous_marker;
        if let Some(record) = self.action_history.last_mut() {
            record.page_changed = Some(changed);
            record.url = fresh.url.clone();
            record.elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        }

        // A WAIT that changes nothing is expected; a click that changes nothing is a symptom.
        if changed || action.kind == ActionKind::Wait {
            self.consecutive_no_change = 0;
        } else {
            self.consecutive_no_change += 1;
            if self.consecutive_no_change >= self.config.no_change_blocked_threshold {
                return Ok(StepOutcome::Stopped(RunOutcome::Blocked {
                    reason: format!(
                        "{} consecutive actions left the page unchanged",
                        self.consecutive_no_change
                    ),
                }));
            }
        }

        Ok(StepOutcome::Continued)
    }

    fn record_action(
        &mut self,
        decision: &Decision,
        label: &str,
        resolution: Option<&Resolution>,
        _kind: Option<ActionKind>,
        started: Instant,
        snapshot: &Snapshot,
    ) {
        self.action_history.push(ActionRecord {
            step: self.steps,
            operation: decision.operation,
            label: label.to_string(),
            target: decision.target.clone(),
            text: resolution.map(|r| r.value.clone()),
            text_sensitive: resolution.map(|r| r.sensitive).unwrap_or(false),
            value_key: resolution.map(|r| r.key.clone()),
            confidence: decision.effective_confidence(),
            jev_latency_ms: decision.latency_ms,
            browser_latency_ms: started.elapsed().as_millis() as u64,
            page_changed: None,
            url: snapshot.url.clone(),
            elapsed_ms: self.started_at.elapsed().as_millis() as u64,
        });
    }

    async fn finish_done(&mut self, snapshot: &Snapshot) -> Result<RunOutcome> {
        // Re-observe: DONE must be judged against the live page, not the one Jev looked at.
        let started = Instant::now();
        let fresh = match self.browser.is_fresh(snapshot, None).await {
            Ok(true) => snapshot.clone(),
            _ => self.observe().await?,
        };
        let report = verification::verify(&fresh, &self.value_pool, &self.initial_url, &self.goal);
        self.metrics.verification_ms += started.elapsed().as_millis() as u64;
        info!(
            session = %self.id,
            verdict = ?report.verdict,
            host_verification_required = report.host_verification_required,
            "verified DONE claim"
        );
        self.last_verification = Some(report.clone());
        Ok(RunOutcome::Done { verification: report, steps: self.steps })
    }

    // --- host intervention protocol ---

    fn request_input(&mut self, field: FieldDescription) -> RunOutcome {
        let request_id = new_request_id();
        let question = format!(
            "What text should be entered into \"{}\"? The task context did not contain a confident match.",
            field.label
        );
        self.pending = Some(Pending::Input { request_id: request_id.clone(), field: field.clone() });
        RunOutcome::NeedsInput {
            request_id,
            field,
            question,
            known_keys: self.value_pool.assertable().iter().map(|e| e.key.clone()).collect(),
        }
    }

    fn request_reasoning(
        &mut self,
        reason: String,
        snapshot: &Snapshot,
        space: &ActionSpace,
        decision: &Decision,
    ) -> RunOutcome {
        let request_id = new_request_id();
        let mut candidates: Vec<CandidateAction> = decision
            .target_probabilities
            .iter()
            .filter_map(|(index, probability)| {
                let action_id = space.targets.get(&decision.operation)?.get(index)?;
                let action = snapshot.action(action_id)?;
                Some(CandidateAction {
                    index: index.clone(),
                    label: action.label.clone(),
                    operation: decision.operation.as_str().to_string(),
                    probability: *probability,
                })
            })
            .collect();
        candidates
            .sort_by(|a, b| b.probability.partial_cmp(&a.probability).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(6);

        self.pending = Some(Pending::Reasoning { request_id: request_id.clone() });
        RunOutcome::NeedsReasoning {
            request_id,
            reason,
            page_summary: snapshot.summary(1200),
            candidate_actions: candidates,
        }
    }

    fn request_confirmation(
        &mut self,
        finding: ConsequenceFinding,
        action: &RawAction,
        generation: u64,
        snapshot: &Snapshot,
    ) -> RunOutcome {
        let request_id = new_request_id();
        let outcome = RunOutcome::NeedsConfirmation {
            request_id: request_id.clone(),
            action: format!("{} \"{}\"", action.kind_verb(), action.element_label()),
            consequence: finding.class.describe().to_string(),
            question: finding.question(),
            page_summary: snapshot.summary(1200),
        };
        self.pending =
            Some(Pending::Confirmation { request_id, action_id: action.id.clone(), generation, finding });
        outcome
    }

    fn take_pending(&mut self, request_id: &str, expected: &str) -> Result<Pending> {
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| RelayError::UnknownRequest(format!("{request_id} (nothing is pending)")))?;
        if pending.request_id() != request_id {
            return Err(RelayError::UnknownRequest(format!(
                "{request_id}; the open request is {}",
                pending.request_id()
            )));
        }
        let matches = matches!(
            (pending, expected),
            (Pending::Input { .. }, "input")
                | (Pending::Reasoning { .. }, "reasoning")
                | (Pending::Confirmation { .. }, "confirmation")
        );
        if !matches {
            return Err(RelayError::WrongState {
                session: self.id.clone(),
                status: self.status.as_str().to_string(),
                attempted: expected.to_string(),
            });
        }
        Ok(self.pending.take().expect("checked above"))
    }

    /// Supply a value the runtime could not resolve. Caches it under the field's own label so
    /// every later field with that meaning resolves without asking again.
    pub fn provide_input(&mut self, request_id: &str, value: &str, remember: bool) -> Result<()> {
        let pending = self.take_pending(request_id, "input")?;
        let Pending::Input { field, .. } = pending else { unreachable!() };
        if value.is_empty() {
            return Err(RelayError::InvalidArgument("value must not be empty".into()));
        }
        if remember {
            self.value_pool.insert(
                &field.label,
                value,
                ValueSource::HostInput,
                Some(format!("supplied by the host for \"{}\"", field.label)),
            );
        } else {
            // Still needed for this one step, but not cached for reuse.
            self.value_pool.insert(&field.label, value, ValueSource::HostInput, None);
        }
        self.metrics.host_round_trips += 1;
        self.status = SessionStatus::Ready;
        self.last_activity = Instant::now();
        Ok(())
    }

    /// Supply guidance for one ambiguous decision.
    pub fn provide_reasoning(&mut self, request_id: &str, guidance: &str) -> Result<()> {
        let _ = self.take_pending(request_id, "reasoning")?;
        if guidance.trim().is_empty() {
            return Err(RelayError::InvalidArgument("guidance must not be empty".into()));
        }
        self.host_guidance = Some(guidance.trim().to_string());
        self.metrics.host_round_trips += 1;
        self.status = SessionStatus::Ready;
        self.last_activity = Instant::now();
        Ok(())
    }

    /// Approve or reject a gated consequential action.
    pub fn confirm(&mut self, request_id: &str, approved: bool) -> Result<()> {
        let pending = self.take_pending(request_id, "confirmation")?;
        let Pending::Confirmation { action_id, generation, finding, .. } = pending else { unreachable!() };
        self.metrics.host_round_trips += 1;
        self.last_activity = Instant::now();
        if approved {
            // Bound to this exact action *and* the page it was observed on, so an approval
            // cannot be replayed against a page that has since changed.
            self.approved_action = Some((action_id, generation));
            self.status = SessionStatus::Ready;
            info!(session = %self.id, class = ?finding.class, "host approved a consequential action");
        } else {
            self.status = SessionStatus::Blocked;
            info!(session = %self.id, class = ?finding.class, "host declined a consequential action");
        }
        Ok(())
    }

    fn is_approved(&self, action_id: &str, generation: u64) -> bool {
        self.approved_action.as_ref().is_some_and(|(approved_id, approved_generation)| {
            approved_id == action_id && *approved_generation == generation
        })
    }

    /// Read-only current state.
    pub async fn observe_now(&mut self, want_screenshot: bool) -> Result<serde_json::Value> {
        let snapshot = self.ensure_fresh_snapshot().await?;
        let space = ActionSpace::build(&snapshot);
        let screenshot = if want_screenshot { self.browser.screenshot().await.unwrap_or(None) } else { None };
        Ok(serde_json::json!({
            "session_id": self.id,
            "status": self.status.as_str(),
            "url": snapshot.url,
            "title": snapshot.title,
            "text": snapshot.text,
            "elements": space.elements,
            "omitted_actions": snapshot.omitted_actions,
            "recent_actions": self.action_history.recent(10),
            "value_pool": self.value_pool.summary(),
            "screenshot": screenshot,
        }))
    }

    pub async fn stop(&mut self) -> Result<serde_json::Value> {
        self.metrics.total_task_ms = self.started_at.elapsed().as_millis() as u64;
        self.metrics.browser_protocol_calls = self.browser.protocol_calls();
        let report = self.metrics.report();
        let _ = self.browser.close().await;
        if !self.status.is_terminal() {
            self.status = SessionStatus::Done;
        }
        Ok(report)
    }

    pub fn metrics_report(&mut self) -> serde_json::Value {
        self.metrics.browser_protocol_calls = self.browser.protocol_calls();
        self.metrics.total_task_ms = self.started_at.elapsed().as_millis() as u64;
        self.metrics.report()
    }
}

enum StepOutcome {
    /// One browser action executed.
    Continued,
    /// Nothing executed; the loop should try again without spending step budget.
    Retried,
    Stopped(RunOutcome),
}

impl RawAction {
    fn kind_verb(&self) -> &'static str {
        match self.kind {
            ActionKind::Click => "click",
            ActionKind::Fill => "type into",
            ActionKind::Select => "select",
            ActionKind::Scroll => "scroll",
            ActionKind::Wait => "wait on",
        }
    }
}

fn new_request_id() -> String {
    format!("req_{}", uuid::Uuid::new_v4().simple())
}
