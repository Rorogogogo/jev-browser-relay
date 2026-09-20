//! Comparative benchmark: host-driven browser agent (Mode A) vs jev-browser-relay (Mode B).
//!
//! # What is measured and what is modelled
//!
//! Both modes drive the **same** scripted page model and make the **same** decisions, so the
//! browser work is identical and measured for real. What differs is *who is consulted*:
//!
//! - **Mode A** — a host-driven agent. The host model participates in each meaningful browser
//!   action: observe, send the page to the model, get one action back, execute, repeat.
//! - **Mode B** — jev-browser-relay. Jev decides continuously; the host is involved only for
//!   planning, unresolvable values, real ambiguity, confirmation and final verification.
//!
//! Round-trip counts are exact and structural — they fall out of the architecture, not out of a
//! stopwatch. **Model latency is modelled, not measured**, unless `--live` is passed: a host turn
//! is assumed to cost `--host-turn-ms` and a Jev request `--jev-ms`. Both are stated in the
//! output, so nobody can mistake a projection for a measurement. The defaults are conservative
//! towards Mode A.
//!
//! Trials alternate A/B and B/A to keep ordering effects out of the result.

use anyhow::Result;
use jev_browser_relay_browser::ScriptedBackend;
use jev_browser_relay_core::action_space::ActionSpace;
use jev_browser_relay_core::browser::BrowserBackend;
use jev_browser_relay_core::config::RuntimeConfig;
use jev_browser_relay_core::jev::JevTransport;
use jev_browser_relay_core::session::{RunBudget, RunOutcome, Session};
use jev_browser_relay_core::snapshot::Operation;
use jev_browser_relay_core::testing::{Planned, ScriptedJev};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

pub const FLIGHTS: &str = include_str!("../../../fixtures/flights.json");
pub const DOCS: &str = include_str!("../../../fixtures/docs.json");
pub const SIGNUP: &str = include_str!("../../../fixtures/signup.json");
pub const AMBIGUOUS: &str = include_str!("../../../fixtures/ambiguous.json");
pub const CHECKOUT: &str = include_str!("../../../fixtures/checkout.json");

pub struct Scenario {
    pub name: &'static str,
    pub kind: &'static str,
    pub fixture: &'static str,
    pub goal: &'static str,
    pub context: fn() -> Option<Value>,
    pub plan: fn() -> Vec<Planned>,
    /// Host answers, consumed in order as the runtime pauses.
    pub answers: fn() -> Vec<HostAnswer>,
    /// A substring that must appear in the final url for the run to count as a success.
    pub success_url: &'static str,
}

#[derive(Debug, Clone)]
pub enum HostAnswer {
    Input(&'static str),
    Reasoning(&'static str),
    Confirm(bool),
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "click-heavy",
            kind: "Mostly navigation. Where Jev's advantage should be largest.",
            fixture: DOCS,
            goal: "Open the authentication guide and report how to rotate an API token",
            context: || Some(json!({ "topic": "rotating an API token" })),
            plan: || {
                vec![
                    Planned::targeting(Operation::Click, "Guides"),
                    Planned::targeting(Operation::Click, "Getting started"),
                    Planned::targeting(Operation::Click, "Next: Deployment"),
                    Planned::targeting(Operation::Click, "Next: Authentication"),
                    Planned::new(Operation::Done),
                ]
            },
            answers: Vec::new,
            success_url: "/authentication",
        },
        Scenario {
            name: "form-heavy",
            kind: "Eight fields, six resolvable from context. Tests value-pool reuse.",
            fixture: SIGNUP,
            goal: "Create an account for Ada Lovelace",
            context: || {
                Some(json!({
                    "given_name": "Ada",
                    "surname": "Lovelace",
                    "email": "ada@analytical.test",
                    "employer": "Analytical Engines",
                    "mobile": "+61 400 000 000",
                    "town": "Sydney"
                }))
            },
            plan: || {
                vec![
                    Planned::targeting(Operation::TypeText, "First name"),
                    Planned::targeting(Operation::TypeText, "Last name"),
                    Planned::targeting(Operation::TypeText, "Work email"),
                    Planned::targeting(Operation::TypeText, "Company"),
                    Planned::targeting(Operation::TypeText, "Phone number"),
                    Planned::targeting(Operation::TypeText, "City"),
                    Planned::targeting(Operation::Select, "Australia"),
                    Planned::targeting(Operation::TypeText, "How did you hear"),
                    Planned::targeting(Operation::TypeText, "How did you hear"),
                    Planned::targeting(Operation::Click, "Create account"),
                    Planned::new(Operation::Done),
                ]
            },
            answers: || vec![HostAnswer::Input("A colleague")],
            success_url: "/welcome",
        },
        Scenario {
            name: "mixed",
            kind: "Clicks, typing and a selection together.",
            fixture: FLIGHTS,
            goal: "Find one-way flights from Sydney to Tokyo on 10 October 2026",
            context: || {
                Some(json!({
                    "origin": "Sydney",
                    "destination": "Tokyo",
                    "date": "2026-10-10",
                    "trip_type": "one-way"
                }))
            },
            plan: || {
                vec![
                    Planned::targeting(Operation::Select, "One way"),
                    Planned::targeting(Operation::TypeText, "Where from?"),
                    Planned::targeting(Operation::TypeText, "Where to?"),
                    Planned::targeting(Operation::TypeText, "Departure"),
                    Planned::targeting(Operation::Click, "Search"),
                    Planned::new(Operation::Done),
                ]
            },
            answers: Vec::new,
            success_url: "/results",
        },
        Scenario {
            name: "reasoning-fallback",
            kind: "Forces one NEEDS_REASONING transition.",
            fixture: AMBIGUOUS,
            goal: "Open the invoice for the disputed order",
            context: || None,
            plan: || {
                vec![
                    Planned::targeting(Operation::Click, "Invoice 1041").ambiguous(),
                    Planned::targeting(Operation::Click, "Invoice 1042"),
                    Planned::new(Operation::Done),
                ]
            },
            answers: || vec![HostAnswer::Reasoning("Open invoice 1042; it is the disputed order.")],
            success_url: "/detail",
        },
        Scenario {
            name: "safety-gate",
            kind: "Reaches a consequential action and verifies confirmation gating.",
            fixture: CHECKOUT,
            goal: "Buy the item in the cart",
            context: || Some(json!({ "name on card": "Ada Lovelace" })),
            plan: || {
                vec![
                    Planned::targeting(Operation::Click, "Continue to checkout"),
                    Planned::targeting(Operation::TypeText, "Name on card"),
                    Planned::targeting(Operation::Click, "Place order"),
                    Planned::targeting(Operation::Click, "Place order"),
                    Planned::new(Operation::Done),
                ]
            },
            answers: || vec![HostAnswer::Confirm(true)],
            success_url: "/confirmed",
        },
    ]
}

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Assumed cost of one host-model turn, in ms.
    pub host_turn_ms: u64,
    /// Assumed cost of one Jev request, in ms. Ignored when the transport is live.
    pub jev_ms: u64,
    pub live_jev: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trial {
    pub scenario: String,
    pub mode: String,
    pub wall_clock_ms: u64,
    pub host_round_trips: u32,
    pub jev_requests: u32,
    pub browser_actions: u32,
    pub browser_protocol_calls: u32,
    pub fallbacks: u32,
    pub success: bool,
}

/// Mode A — a host-driven browser agent. The host model is consulted for every action.
async fn run_mode_a(scenario: &Scenario, timing: Timing) -> Result<Trial> {
    let mut backend = ScriptedBackend::from_json(scenario.fixture)?;
    // A plan may repeat an entry where Mode B re-asks after a pause. Mode B's pauses do not
    // exist here, so a host-driven agent would never re-issue the same decision — collapsing
    // them keeps the comparison fair to Mode A.
    let mut plan = (scenario.plan)();
    plan.dedup_by(|a, b| a.operation == b.operation && a.target_label == b.target_label);
    let started = Instant::now();
    let mut modelled_ms = 0u64;
    let mut host_round_trips = 0u32;
    let mut browser_actions = 0u32;

    // A host-driven agent needs one turn to plan before it starts.
    host_round_trips += 1;
    modelled_ms += timing.host_turn_ms;

    // The context values a host-driven agent must supply are known up front here too, so the
    // comparison is about *architecture*, not about who knows the task.
    let mut pool = jev_browser_relay_core::value_pool::ValuePool::new();
    if let Some(context) = (scenario.context)() {
        pool.load_context(&context);
    }
    for answer in (scenario.answers)() {
        if let HostAnswer::Input(value) = answer {
            pool.insert(
                "how did you hear about us",
                value,
                jev_browser_relay_core::ValueSource::HostInput,
                None,
            );
        }
    }

    for planned in plan {
        let snapshot = backend.observe().await?;
        let space = ActionSpace::build(&snapshot);

        // Every decision is a host-model turn: page in, one action out.
        host_round_trips += 1;
        modelled_ms += timing.host_turn_ms;

        if planned.operation.terminal() {
            break;
        }
        let Some(wanted) = planned.target_label.as_deref() else { continue };
        let Some(targets) = space.targets.get(&planned.operation) else { continue };
        let Some(action_id) = targets
            .iter()
            .find(|(index, id)| {
                snapshot
                    .action(id)
                    .map(|a| {
                        let rendered = format!("[{index}] {}", a.label).to_lowercase();
                        rendered.contains(&wanted.to_lowercase())
                    })
                    .unwrap_or(false)
            })
            .map(|(_, id)| id.clone())
        else {
            continue;
        };
        let action = snapshot.action(&action_id).expect("resolved above").clone();

        // A host-driven agent also writes the text itself — another turn's worth of work, but it
        // is folded into the same turn here, which is generous to Mode A.
        let text = pool.resolve(&action.describe()).map(|r| r.value);
        backend.execute(&action, &snapshot, text.as_deref()).await?;
        browser_actions += 1;
    }

    // And one final turn to verify the outcome.
    host_round_trips += 1;
    modelled_ms += timing.host_turn_ms;

    let final_url = backend.observe().await.map(|s| s.url).unwrap_or_default();
    Ok(Trial {
        scenario: scenario.name.into(),
        mode: "A: host-driven".into(),
        wall_clock_ms: started.elapsed().as_millis() as u64 + modelled_ms,
        host_round_trips,
        jev_requests: 0,
        browser_actions,
        browser_protocol_calls: backend.protocol_calls(),
        fallbacks: 0,
        success: final_url.contains(scenario.success_url),
    })
}

/// Mode B — jev-browser-relay. Jev decides continuously; the host answers only what it is asked.
async fn run_mode_b(scenario: &Scenario, timing: Timing, jev: Arc<dyn JevTransport>) -> Result<Trial> {
    let backend = ScriptedBackend::from_json(scenario.fixture)?;
    let config = RuntimeConfig { today: "2026-09-20".into(), ..RuntimeConfig::default() };
    let mut session = Session::start(
        "bench".into(),
        scenario.goal.into(),
        (scenario.context)(),
        jev,
        Box::new(backend),
        config,
    )
    .await?;

    let started = Instant::now();
    let mut answers = (scenario.answers)().into_iter();
    let mut fallbacks = 0u32;
    let budget = RunBudget { max_steps: 60, max_duration_ms: 60_000 };

    loop {
        let outcome = session.run(budget).await;
        match outcome {
            RunOutcome::Done { .. } | RunOutcome::Blocked { .. } | RunOutcome::Error { .. } => break,
            RunOutcome::BudgetExceeded { .. } => continue,
            RunOutcome::NeedsInput { request_id, .. } => {
                fallbacks += 1;
                let Some(HostAnswer::Input(value)) = answers.next() else {
                    anyhow::bail!("{}: unexpected needs_input with no scripted answer", scenario.name)
                };
                session.provide_input(&request_id, value, true)?;
            }
            RunOutcome::NeedsReasoning { request_id, .. } => {
                fallbacks += 1;
                let Some(HostAnswer::Reasoning(guidance)) = answers.next() else {
                    anyhow::bail!("{}: unexpected needs_reasoning with no scripted answer", scenario.name)
                };
                session.provide_reasoning(&request_id, guidance)?;
            }
            RunOutcome::NeedsConfirmation { request_id, .. } => {
                fallbacks += 1;
                let Some(HostAnswer::Confirm(approved)) = answers.next() else {
                    anyhow::bail!("{}: unexpected needs_confirmation with no scripted answer", scenario.name)
                };
                session.confirm(&request_id, approved)?;
            }
        }
    }

    let metrics = session.metrics_report();
    let host_round_trips = metrics["host_round_trips"].as_u64().unwrap_or(0) as u32;
    let jev_requests = metrics["jev_requests"].as_u64().unwrap_or(0) as u32;

    // Modelled model time: each host turn, plus each Jev request when the transport is scripted.
    let mut modelled_ms = host_round_trips as u64 * timing.host_turn_ms;
    if !timing.live_jev {
        modelled_ms += jev_requests as u64 * timing.jev_ms;
    }

    let success = session.current_url().contains(scenario.success_url);
    Ok(Trial {
        scenario: scenario.name.into(),
        mode: "B: jev-browser-relay".into(),
        wall_clock_ms: started.elapsed().as_millis() as u64 + modelled_ms,
        host_round_trips,
        jev_requests,
        browser_actions: metrics["browser_actions"].as_u64().unwrap_or(0) as u32,
        browser_protocol_calls: metrics["browser_protocol_calls"].as_u64().unwrap_or(0) as u32,
        fallbacks,
        success,
    })
}

pub async fn run(
    trials: u32,
    timing: Timing,
    only: Option<&str>,
    live_jev: Option<Arc<dyn JevTransport>>,
) -> Result<Vec<Trial>> {
    let mut results = Vec::new();
    for scenario in scenarios() {
        if only.is_some_and(|name| name != scenario.name) {
            continue;
        }
        for trial in 0..trials {
            // Alternate the order so neither mode always runs on a warm process.
            let a_first = trial % 2 == 0;
            let make_jev = || -> Arc<dyn JevTransport> {
                match &live_jev {
                    Some(client) => client.clone(),
                    None => Arc::new(ScriptedJev::new((scenario.plan)())),
                }
            };
            if a_first {
                results.push(run_mode_a(&scenario, timing).await?);
                results.push(run_mode_b(&scenario, timing, make_jev()).await?);
            } else {
                results.push(run_mode_b(&scenario, timing, make_jev()).await?);
                results.push(run_mode_a(&scenario, timing).await?);
            }
        }
    }
    Ok(results)
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub scenario: String,
    pub mode: String,
    pub trials: u32,
    pub median_wall_clock_ms: u64,
    pub mean_host_round_trips: f64,
    pub mean_jev_requests: f64,
    pub mean_browser_actions: f64,
    pub mean_browser_protocol_calls: f64,
    pub mean_fallbacks: f64,
    pub success_rate: f64,
}

pub fn summarize(trials: &[Trial]) -> Vec<Summary> {
    let mut keys: Vec<(String, String)> =
        trials.iter().map(|t| (t.scenario.clone(), t.mode.clone())).collect();
    keys.dedup();
    keys.sort();
    keys.dedup();

    let mut summaries: Vec<Summary> = keys
        .into_iter()
        .map(|(scenario, mode)| {
            let group: Vec<&Trial> =
                trials.iter().filter(|t| t.scenario == scenario && t.mode == mode).collect();
            let n = group.len() as f64;
            let mut wall: Vec<u64> = group.iter().map(|t| t.wall_clock_ms).collect();
            wall.sort_unstable();
            Summary {
                scenario,
                mode,
                trials: group.len() as u32,
                median_wall_clock_ms: wall[wall.len() / 2],
                mean_host_round_trips: group.iter().map(|t| t.host_round_trips as f64).sum::<f64>() / n,
                mean_jev_requests: group.iter().map(|t| t.jev_requests as f64).sum::<f64>() / n,
                mean_browser_actions: group.iter().map(|t| t.browser_actions as f64).sum::<f64>() / n,
                mean_browser_protocol_calls: group
                    .iter()
                    .map(|t| t.browser_protocol_calls as f64)
                    .sum::<f64>()
                    / n,
                mean_fallbacks: group.iter().map(|t| t.fallbacks as f64).sum::<f64>() / n,
                success_rate: group.iter().filter(|t| t.success).count() as f64 / n,
            }
        })
        .collect();
    // Keep scenarios in declaration order, Mode A before Mode B.
    let order: Vec<&str> = scenarios().iter().map(|s| s.name).collect();
    summaries.sort_by_key(|s| (order.iter().position(|n| *n == s.scenario).unwrap_or(99), s.mode.clone()));
    summaries
}

pub fn print_report(summaries: &[Summary], timing: Timing) {
    println!("\njev-browser-relay benchmark\n");
    println!(
        "  Browser work and round-trip counts are measured. Model time is modelled: a host turn\n  \
         costs {} ms, a Jev request {} ms{}.\n",
        timing.host_turn_ms,
        timing.jev_ms,
        if timing.live_jev { " (Jev measured live)" } else { "" }
    );
    println!(
        "  {:<20} {:<22} {:>7} {:>6} {:>6} {:>7} {:>6} {:>5}",
        "scenario", "mode", "ms", "host", "jev", "actions", "cdp", "ok"
    );
    println!("  {}", "-".repeat(84));

    let mut previous = String::new();
    for summary in summaries {
        if summary.scenario != previous {
            previous = summary.scenario.clone();
        }
        println!(
            "  {:<20} {:<22} {:>7} {:>6.1} {:>6.1} {:>7.1} {:>6.0} {:>4.0}%",
            summary.scenario,
            summary.mode,
            summary.median_wall_clock_ms,
            summary.mean_host_round_trips,
            summary.mean_jev_requests,
            summary.mean_browser_actions,
            summary.mean_browser_protocol_calls,
            summary.success_rate * 100.0,
        );
    }

    // The headline: host turns eliminated.
    let a: f64 = summaries.iter().filter(|s| s.mode.starts_with('A')).map(|s| s.mean_host_round_trips).sum();
    let b: f64 = summaries.iter().filter(|s| s.mode.starts_with('B')).map(|s| s.mean_host_round_trips).sum();
    println!("\n  scenarios");
    for scenario in scenarios() {
        if summaries.iter().any(|s| s.scenario == scenario.name) {
            println!("    {:<20} {}", scenario.name, scenario.kind);
        }
    }

    println!("\n  Host-model turns across all scenarios: Mode A {a:.0}, Mode B {b:.0}");
    if a > 0.0 {
        println!("  Host turns eliminated: {:.0}%", (a - b) / a * 100.0);
    }
    println!();
}

pub fn report_json(summaries: &[Summary], trials: &[Trial], timing: Timing) -> Value {
    json!({
        "measured": ["browser actions", "browser protocol calls", "host round trips", "jev requests", "success"],
        "modelled": {
            "host_turn_ms": timing.host_turn_ms,
            "jev_request_ms": if timing.live_jev { Value::Null } else { json!(timing.jev_ms) },
            "note": "Round-trip counts are structural and exact. Model latency is a stated assumption, \
                     not a measurement, unless --live was used."
        },
        "summaries": summaries,
        "trials": trials,
    })
}
