//! The vertical slice: one `run` call drives a whole multi-step browser task, with the host
//! model involved exactly once.

use jev_browser_relay_core::session::{RunBudget, RunOutcome};
use jev_browser_relay_core::snapshot::Operation;
use jev_browser_relay_core::testing::{Planned, ScriptedJev};
use jev_browser_relay_core::verification::Verdict;
use jev_browser_relay_tests::*;
use serde_json::json;
use std::sync::Arc;

fn flights_plan() -> Vec<Planned> {
    vec![
        Planned::targeting(Operation::Select, "One way"),
        Planned::targeting(Operation::TypeText, "Where from?"),
        Planned::targeting(Operation::TypeText, "Where to?"),
        Planned::targeting(Operation::TypeText, "Departure"),
        Planned::targeting(Operation::Click, "Search"),
        Planned::new(Operation::Done),
    ]
}

#[tokio::test]
async fn one_run_call_executes_many_browser_actions() {
    let jev = Arc::new(ScriptedJev::new(flights_plan()));
    let mut session = session_with(
        FLIGHTS,
        "Find one-way flights from Sydney to Tokyo on 10 October 2026",
        Some(json!({
            "origin": "Sydney",
            "destination": "Tokyo",
            "date": "2026-10-10",
            "trip_type": "one-way"
        })),
        jev.clone(),
        test_config(),
    )
    .await;

    let outcome = session.run(RunBudget { max_steps: 30, max_duration_ms: 15_000 }).await;

    let RunOutcome::Done { verification, steps } = &outcome else {
        panic!("expected Done, got {outcome:?}");
    };

    // The whole point: many browser actions, one host turn.
    assert_eq!(*steps, 5, "expected 5 browser actions, got {steps}");
    assert_eq!(session.metrics.browser_actions, 5);
    assert_eq!(session.metrics.host_round_trips, 1, "the host should have been called exactly once");
    assert_eq!(session.metrics.host_input_requests, 0);
    assert_eq!(session.metrics.host_reasoning_requests, 0);

    // Three text fields resolved from the task context, with no second model API.
    assert_eq!(session.metrics.values_resolved_locally, 3);

    assert_eq!(verification.verdict, Verdict::Verified, "checks: {:#?}", verification.checks);
    assert!(!verification.host_verification_required);
    assert!(verification.final_url.ends_with("/results"));
}

#[tokio::test]
async fn the_runtime_types_the_right_value_into_each_field() {
    let jev = Arc::new(ScriptedJev::new(flights_plan()));
    let mut session = session_with(
        FLIGHTS,
        "Find one-way flights from Sydney to Tokyo on 10 October 2026",
        Some(json!({ "origin": "Sydney", "destination": "Tokyo", "date": "2026-10-10" })),
        jev,
        test_config(),
    )
    .await;

    session.run(RunBudget { max_steps: 30, max_duration_ms: 15_000 }).await;

    let history = session.history();
    let typed: Vec<(String, Option<String>)> = history
        .iter()
        .filter(|record| record.operation == Operation::TypeText)
        .map(|record| (record.label.clone(), record.text.clone()))
        .collect();

    assert_eq!(
        typed,
        vec![
            ("Where from?".to_string(), Some("Sydney".to_string())),
            ("Where to?".to_string(), Some("Tokyo".to_string())),
            ("Departure".to_string(), Some("2026-10-10".to_string())),
        ],
        "each field should have been matched to its semantic value"
    );
}

#[tokio::test]
async fn a_click_heavy_task_needs_no_host_intervention() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Guides"),
        Planned::targeting(Operation::Click, "Getting started"),
        Planned::targeting(Operation::Click, "Next: Deployment"),
        Planned::targeting(Operation::Click, "Next: Authentication"),
        Planned::new(Operation::Done),
    ]));
    let mut session = session_with(
        DOCS,
        "Open the authentication guide and report how to rotate an API token",
        Some(json!({ "topic": "rotating an API token" })),
        jev,
        test_config(),
    )
    .await;

    let outcome = session.run(RunBudget { max_steps: 30, max_duration_ms: 15_000 }).await;

    assert!(matches!(outcome, RunOutcome::Done { .. }), "got {outcome:?}");
    assert_eq!(session.metrics.browser_actions, 4);
    assert_eq!(session.metrics.host_round_trips, 1);
    assert!(
        session.metrics.actions_per_host_round_trip() >= 4.0,
        "expected at least 4 actions per host turn"
    );
}

#[tokio::test]
async fn back_returns_to_the_previous_page() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Guides"),
        Planned::targeting(Operation::Click, "Deployment"),
        Planned::new(Operation::Back),
        Planned::new(Operation::Done),
    ]));
    let mut session = session_with(DOCS, "Look at deployment then come back", None, jev, test_config()).await;

    let outcome = session.run(RunBudget { max_steps: 10, max_duration_ms: 15_000 }).await;

    assert!(matches!(outcome, RunOutcome::Done { .. }), "got {outcome:?}");
    assert!(session.current_url().ends_with("/docs/guides"), "url was {}", session.current_url());
}
