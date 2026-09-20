//! Test doubles for the Jev transport.
//!
//! These are not canned-response stubs: [`ScriptedJev`] reads the real request body, resolves the
//! planned target against the *actual* offered choice space, and emits a well-formed probability
//! distribution over exactly the offered ids. So every test exercises `build_request` and
//! `validate_response` for real — only the network is absent.
//!
//! This is what lets normal `cargo test` run with no paid API key.

use crate::error::{RelayError, Result};
use crate::jev::{JevRawResponse, JevRequest, JevTransport};
use crate::snapshot::Operation;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One planned decision, written the way a test wants to read it.
#[derive(Debug, Clone)]
pub struct Planned {
    pub operation: Operation,
    /// Case-insensitive substring of the target's rendered label. `None` for operations that
    /// take no target.
    pub target_label: Option<String>,
    pub confidence: f64,
    /// Probability given to the chosen target; the remainder is spread over the others. Lower
    /// values create a thin margin, which is how the reasoning escape hatch is tested.
    pub target_probability: f64,
}

impl Planned {
    pub fn new(operation: Operation) -> Self {
        Self { operation, target_label: None, confidence: 0.95, target_probability: 0.9 }
    }

    pub fn targeting(operation: Operation, label: &str) -> Self {
        Self { target_label: Some(label.to_string()), ..Self::new(operation) }
    }

    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }

    /// Make the top two targets near-identical, so the decision reads as ambiguous.
    pub fn ambiguous(mut self) -> Self {
        self.target_probability = 0.34;
        self
    }
}

/// How the transport should misbehave, for error-path tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misbehaviour {
    None,
    /// Answer with a choice that was never offered.
    UnofferedChoice,
    /// Probabilities that do not sum to 1.
    BadDistribution,
    /// Omit the target head the chosen operation needs.
    MissingTargetHead,
    /// Fail the transport outright.
    TransportError,
    /// Time out.
    Timeout,
    /// Rate limit.
    RateLimited,
}

/// A deterministic Jev stand-in driven by a plan.
///
/// One plan entry is consumed per **request**, not per executed action. A decision that the
/// runtime discards — because the page went stale, or because it paused for host input — still
/// spends its entry, exactly as a real request would have been spent. Plans therefore repeat an
/// entry wherever the loop legitimately re-asks.
pub struct ScriptedJev {
    plan: Mutex<std::collections::VecDeque<Planned>>,
    /// Returned once the plan is exhausted. Defaults to DONE.
    fallback: Planned,
    misbehave: Mutex<Vec<Misbehaviour>>,
    /// Every request the loop made, for assertions.
    pub requests: Mutex<Vec<JevRequest>>,
}

impl ScriptedJev {
    pub fn new(plan: Vec<Planned>) -> Self {
        Self {
            plan: Mutex::new(plan.into()),
            fallback: Planned::new(Operation::Done),
            misbehave: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn with_fallback(mut self, fallback: Planned) -> Self {
        self.fallback = fallback;
        self
    }

    /// Queue misbehaviours, applied one per request before the plan is consulted.
    pub fn with_misbehaviour(self, misbehave: Vec<Misbehaviour>) -> Self {
        *self.misbehave.lock().unwrap() = misbehave.into_iter().rev().collect();
        self
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// The operation heads offered by a request, in the order the runtime built them.
    fn offered_operations(request: &JevRequest) -> Vec<String> {
        request
            .questions
            .get("operation")
            .and_then(|q| q.get("criteria"))
            .and_then(Value::as_object)
            .map(|c| c.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn offered_targets(request: &JevRequest, operation: Operation) -> BTreeMap<String, String> {
        let head = crate::jev::target_head(operation);
        request
            .questions
            .get(&head)
            .and_then(|q| q.get("criteria"))
            .and_then(Value::as_object)
            .map(|criteria| {
                criteria
                    .iter()
                    .map(|(index, entry)| {
                        let label =
                            entry.get("element").and_then(Value::as_str).unwrap_or_default().to_string();
                        (index.clone(), label)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Spread probability mass so `chosen` is the argmax and the total is exactly 1.
    fn distribution(ids: &[String], chosen: &str, top: f64) -> Value {
        let mut map = serde_json::Map::new();
        if ids.len() == 1 {
            map.insert(chosen.to_string(), json!(1.0));
            return Value::Object(map);
        }
        let top = top.clamp(1.0 / ids.len() as f64, 1.0);
        let rest = (1.0 - top) / (ids.len() - 1) as f64;
        let mut total = 0.0;
        for id in ids {
            let p = if id == chosen { top } else { rest };
            total += p;
            map.insert(id.clone(), json!(p));
        }
        // Absorb float drift into the chosen entry so the sum check passes exactly.
        if let Some(entry) = map.get_mut(chosen) {
            *entry = json!(top + (1.0 - total));
        }
        Value::Object(map)
    }
}

#[async_trait::async_trait]
impl JevTransport for ScriptedJev {
    async fn post(&self, request: &JevRequest) -> Result<JevRawResponse> {
        self.requests.lock().unwrap().push(request.clone());

        let misbehaviour = self.misbehave.lock().unwrap().pop().unwrap_or(Misbehaviour::None);
        match misbehaviour {
            Misbehaviour::TransportError => {
                return Err(RelayError::JevTransport("scripted transport failure".into()))
            }
            Misbehaviour::Timeout => return Err(RelayError::JevTimeout(25_000)),
            Misbehaviour::RateLimited => {
                return Err(RelayError::JevRateLimited { status: 429, retry_after_ms: Some(1) })
            }
            _ => {}
        }

        let planned = self.plan.lock().unwrap().pop_front().unwrap_or_else(|| self.fallback.clone());
        let operations = Self::offered_operations(request);
        if operations.is_empty() {
            return Err(RelayError::JevInvalidResponse("request offered no operations".into()));
        }

        let mut operation_name = planned.operation.as_str().to_string();
        if misbehaviour == Misbehaviour::UnofferedChoice {
            operation_name = "TELEPORT".into();
            let mut answers = BTreeMap::new();
            answers.insert(
                "operation".to_string(),
                json!({
                    "choice": operation_name,
                    "confidence": 0.9,
                    "probabilities": Self::distribution(&operations, operations.first().unwrap(), 0.9),
                }),
            );
            return Ok(JevRawResponse { answers, model: "scripted".into(), usage: Value::Null });
        }

        if !operations.contains(&operation_name) {
            return Err(RelayError::JevInvalidResponse(format!(
                "test plan asked for {operation_name}, which this page does not offer; offered: {operations:?}"
            )));
        }

        let mut answers = BTreeMap::new();
        let operation_probabilities = if misbehaviour == Misbehaviour::BadDistribution {
            let mut map = serde_json::Map::new();
            for id in &operations {
                map.insert(id.clone(), json!(0.9));
            }
            Value::Object(map)
        } else {
            Self::distribution(&operations, &operation_name, 0.9)
        };
        answers.insert(
            "operation".to_string(),
            json!({ "choice": operation_name, "confidence": planned.confidence, "probabilities": operation_probabilities }),
        );

        if planned.operation.takes_target() && misbehaviour != Misbehaviour::MissingTargetHead {
            let targets = Self::offered_targets(request, planned.operation);
            if targets.is_empty() {
                return Err(RelayError::JevInvalidResponse(format!(
                    "test plan asked for {} but the page offers no such targets",
                    planned.operation.as_str()
                )));
            }
            let wanted = planned.target_label.as_deref().unwrap_or_default().to_lowercase();
            let chosen = targets
                .iter()
                .find(|(_, label)| label.to_lowercase().contains(&wanted))
                .map(|(index, _)| index.clone())
                .ok_or_else(|| {
                    RelayError::JevInvalidResponse(format!(
                        "test plan target \"{wanted}\" matched none of: {:?}",
                        targets.values().collect::<Vec<_>>()
                    ))
                })?;
            let ids: Vec<String> = targets.keys().cloned().collect();
            answers.insert(
                crate::jev::target_head(planned.operation),
                json!({
                    "choice": chosen,
                    "confidence": planned.confidence,
                    "probabilities": Self::distribution(&ids, &chosen, planned.target_probability),
                }),
            );
        }

        Ok(JevRawResponse { answers, model: "scripted-jev".into(), usage: json!({ "input_tokens": 0 }) })
    }

    fn describe(&self) -> String {
        "scripted-jev".into()
    }
}
