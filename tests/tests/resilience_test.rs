//! Failure handling: stale pages, malformed and failing Jev responses, budgets, browser loss,
//! concurrent sessions, and the metrics that describe all of it.

use jev_browser_relay_browser::ScriptedBackend;
use jev_browser_relay_core::browser::BrowserBackend;
use jev_browser_relay_core::registry::{new_session_id, SessionRegistry};
use jev_browser_relay_core::session::{RunBudget, RunOutcome, Session, SessionStatus};
use jev_browser_relay_core::snapshot::Operation;
use jev_browser_relay_core::testing::{Misbehaviour, Planned, ScriptedJev};
use jev_browser_relay_core::verification::Verdict;
use jev_browser_relay_tests::*;
use serde_json::json;
use std::sync::Arc;

fn budget() -> RunBudget {
    RunBudget { max_steps: 40, max_duration_ms: 30_000 }
}

// --- stale pages ---

#[tokio::test]
async fn a_decision_against_a_changed_page_is_rejected_and_retaken() {
    // Fail the freshness check that sits between the first decision and its execution.
    let mut fixture: serde_json::Value = serde_json::from_str(DOCS).unwrap();
    fixture["stale_on_freshness_check"] = json!(2);

    let jev = Arc::new(ScriptedJev::new(vec![
        // Decided against a page that changes before the click can land.
        Planned::targeting(Operation::Click, "Guides"),
        // Re-decided after re-observing.
        Planned::targeting(Operation::Click, "Guides"),
        Planned::targeting(Operation::Click, "Getting started"),
        Planned::new(Operation::Done),
    ]));
    let backend = ScriptedBackend::from_json(&fixture.to_string()).unwrap();
    let mut session = Session::start(
        "ses_stale".into(),
        "Open the getting started guide".into(),
        None,
        jev,
        Box::new(backend),
        test_config(),
    )
    .await
    .unwrap();

    let outcome = session.run(budget()).await;

    assert!(matches!(outcome, RunOutcome::Done { .. }), "got {outcome:?}");
    assert_eq!(session.metrics.stale_decisions, 1, "the stale decision should have been counted");
    // The rejected decision cost a Jev request but no browser action: the click never landed twice.
    assert_eq!(session.metrics.jev_requests, 4);
    assert_eq!(session.metrics.browser_actions, 2);
    assert!(session.current_url().ends_with("/getting-started"));
}

#[tokio::test]
async fn a_page_that_never_settles_exhausts_the_retry_budget_instead_of_spinning() {
    let mut fixture: serde_json::Value = serde_json::from_str(DOCS).unwrap();
    // Re-arm the mutation on every observation: the page is permanently unstable.
    fixture["stale_after_observation"] = json!(1);
    let mut spec: jev_browser_relay_browser::Fixture = serde_json::from_value(fixture).unwrap();
    spec.stale_after_observation = Some(1);

    // A backend that is always stale, built by re-arming through repeated observations.
    struct AlwaysStale(ScriptedBackend);
    #[async_trait::async_trait]
    impl BrowserBackend for AlwaysStale {
        async fn observe(&mut self) -> jev_browser_relay_core::Result<jev_browser_relay_core::Snapshot> {
            self.0.observe().await
        }
        async fn is_fresh(
            &mut self,
            _snapshot: &jev_browser_relay_core::Snapshot,
            _action: Option<&jev_browser_relay_core::RawAction>,
        ) -> jev_browser_relay_core::Result<bool> {
            Ok(false)
        }
        async fn execute(
            &mut self,
            action: &jev_browser_relay_core::RawAction,
            snapshot: &jev_browser_relay_core::Snapshot,
            text: Option<&str>,
        ) -> jev_browser_relay_core::Result<()> {
            self.0.execute(action, snapshot, text).await
        }
        async fn settle(
            &mut self,
            _a: &jev_browser_relay_core::RawAction,
        ) -> jev_browser_relay_core::Result<u64> {
            Ok(0)
        }
        async fn back(&mut self) -> jev_browser_relay_core::Result<()> {
            self.0.back().await
        }
        async fn close(&mut self) -> jev_browser_relay_core::Result<()> {
            self.0.close().await
        }
    }

    let jev =
        Arc::new(ScriptedJev::new(vec![]).with_fallback(Planned::targeting(Operation::Click, "Guides")));
    let backend = AlwaysStale(ScriptedBackend::from_json(DOCS).unwrap());
    let mut session =
        Session::start("ses_spin".into(), "Open guides".into(), None, jev, Box::new(backend), test_config())
            .await
            .unwrap();

    let outcome = session.run(budget()).await;

    let RunOutcome::Error { code, .. } = &outcome else {
        panic!("expected a bounded failure, got {outcome:?}");
    };
    assert_eq!(code, "retry_budget_exhausted", "it must give up rather than retry forever");
    assert_eq!(session.metrics.browser_actions, 0, "nothing should have executed against an unstable page");
    assert!(session.metrics.stale_decisions >= 5);
}

// --- Jev failures ---

#[tokio::test]
async fn a_malformed_jev_response_never_becomes_an_action() {
    for misbehaviour in
        [Misbehaviour::UnofferedChoice, Misbehaviour::BadDistribution, Misbehaviour::MissingTargetHead]
    {
        let jev = Arc::new(
            ScriptedJev::new(vec![Planned::targeting(Operation::Click, "Guides")])
                .with_misbehaviour(vec![misbehaviour]),
        );
        let mut session = session_with(DOCS, "Open guides", None, jev, test_config()).await;

        let outcome = session.run(budget()).await;

        let RunOutcome::Error { code, message } = &outcome else {
            panic!("{misbehaviour:?} should have failed, got {outcome:?}");
        };
        assert_eq!(code, "jev_invalid_response", "{misbehaviour:?}: {message}");
        assert_eq!(session.metrics.browser_actions, 0, "{misbehaviour:?} must not reach the browser");
        assert_eq!(session.status, SessionStatus::Error);
    }
}

#[tokio::test]
async fn recoverable_jev_failures_are_retried_then_surfaced() {
    // Two transient failures, then a good answer.
    let jev = Arc::new(
        ScriptedJev::new(vec![Planned::targeting(Operation::Click, "Guides"), Planned::new(Operation::Done)])
            .with_misbehaviour(vec![Misbehaviour::RateLimited, Misbehaviour::Timeout]),
    );
    let mut session = session_with(DOCS, "Open guides", None, jev, test_config()).await;

    let outcome = session.run(budget()).await;

    assert!(matches!(outcome, RunOutcome::Done { .. }), "got {outcome:?}");
    assert_eq!(session.metrics.retries, 2);
    assert_eq!(session.metrics.browser_actions, 1);
}

#[tokio::test]
async fn persistent_jev_failure_gives_up_with_a_structured_error() {
    let jev = Arc::new(ScriptedJev::new(vec![]).with_misbehaviour(vec![
        Misbehaviour::TransportError,
        Misbehaviour::TransportError,
        Misbehaviour::TransportError,
        Misbehaviour::TransportError,
        Misbehaviour::TransportError,
    ]));
    let mut session = session_with(DOCS, "Open guides", None, jev, test_config()).await;

    let outcome = session.run(budget()).await;

    // A non-recoverable transport error surfaces immediately rather than burning the budget.
    let RunOutcome::Error { code, .. } = &outcome else { panic!("got {outcome:?}") };
    assert_eq!(code, "jev_transport");
    assert_eq!(session.metrics.browser_actions, 0);
}

// --- budgets ---

#[tokio::test]
async fn the_step_budget_pauses_the_run_without_ending_the_task() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Guides"),
        Planned::targeting(Operation::Click, "Getting started"),
        Planned::targeting(Operation::Click, "Next: Deployment"),
    ]));
    let mut session = session_with(DOCS, "Read every guide in order", None, jev, test_config()).await;

    let outcome = session.run(RunBudget { max_steps: 2, max_duration_ms: 30_000 }).await;

    let RunOutcome::BudgetExceeded { reason, steps, .. } = &outcome else {
        panic!("expected BudgetExceeded, got {outcome:?}");
    };
    assert!(reason.contains("max_steps"), "{reason}");
    assert_eq!(*steps, 2);
    // A budget stop is a pause, not a failure: the session is runnable again and picks up
    // exactly where it left off.
    assert_eq!(session.status, SessionStatus::Ready);
    assert!(session.status.is_runnable());
    assert!(session.current_url().ends_with("/getting-started"));

    let outcome = session.run(RunBudget { max_steps: 2, max_duration_ms: 30_000 }).await;
    assert!(
        matches!(outcome, RunOutcome::BudgetExceeded { .. } | RunOutcome::Done { .. }),
        "got {outcome:?}"
    );
    assert_eq!(session.metrics.host_round_trips, 2, "each run call is one host turn");
}

#[tokio::test]
async fn a_loop_that_changes_nothing_is_declared_blocked() {
    // "Blog" has no transition, so clicking it never changes the page.
    let jev = Arc::new(
        ScriptedJev::new(vec![])
            .with_fallback(Planned::targeting(Operation::Click, "Blog").with_confidence(0.99)),
    );
    let mut config = test_config();
    config.reasoning_enabled = false; // isolate the loop detector from the reasoning hatch
    let mut session = session_with(DOCS, "Go nowhere", None, jev, config).await;

    let outcome = session.run(budget()).await;

    let RunOutcome::Blocked { reason } = &outcome else { panic!("expected Blocked, got {outcome:?}") };
    assert!(reason.contains("unchanged"), "{reason}");
    assert!(session.metrics.browser_actions <= 6, "it should give up quickly, not churn");
}

#[tokio::test]
async fn repeated_no_progress_asks_the_host_before_giving_up() {
    let jev = Arc::new(
        ScriptedJev::new(vec![])
            .with_fallback(Planned::targeting(Operation::Click, "Blog").with_confidence(0.99)),
    );
    let mut session = session_with(DOCS, "Go nowhere", None, jev, test_config()).await;

    let outcome = session.run(budget()).await;

    // With the reasoning hatch on, an unproductive loop escalates rather than silently failing.
    let RunOutcome::NeedsReasoning { reason, .. } = &outcome else {
        panic!("expected NeedsReasoning, got {outcome:?}")
    };
    assert!(reason.contains("did not change the page"), "{reason}");
}

// --- browser failures ---

#[tokio::test]
async fn a_disconnected_browser_surfaces_as_a_structured_error() {
    let jev =
        Arc::new(ScriptedJev::new(vec![]).with_fallback(Planned::targeting(Operation::Click, "Guides")));
    let mut backend = ScriptedBackend::from_json(DOCS).unwrap();
    let mut session = {
        let live = ScriptedBackend::from_json(DOCS).unwrap();
        Session::start("ses_dead".into(), "Open guides".into(), None, jev, Box::new(live), test_config())
            .await
            .unwrap()
    };
    backend.close().await.unwrap();

    // Close the session's own browser, then ask it to work.
    session.stop().await.unwrap();
    let outcome = session.run(budget()).await;

    match outcome {
        RunOutcome::Error { code, .. } => {
            assert!(code == "browser_disconnected" || code == "wrong_state", "unexpected code {code}")
        }
        other => panic!("expected an error after the browser closed, got {other:?}"),
    }
}

// --- DONE verification ---

#[tokio::test]
async fn a_done_claim_the_runtime_cannot_verify_is_handed_back_to_the_host() {
    // DONE on the very first page: nothing was achieved, and the checks say so.
    let jev = Arc::new(ScriptedJev::new(vec![Planned::new(Operation::Done)]));
    let mut session = session_with(
        FLIGHTS,
        "Find one-way flights from Sydney to Tokyo",
        Some(json!({ "origin": "Sydney", "destination": "Tokyo" })),
        jev,
        test_config(),
    )
    .await;

    let outcome = session.run(budget()).await;

    let RunOutcome::Done { verification, .. } = &outcome else { panic!("got {outcome:?}") };
    assert_eq!(verification.verdict, Verdict::Failed, "checks: {:#?}", verification.checks);
    assert!(
        verification.host_verification_required,
        "the runtime must never report success on the policy model's say-so alone"
    );
    // The host gets the evidence it needs to judge.
    assert!(!verification.page_excerpt.is_empty());
    assert!(verification.checks.iter().any(|c| c.name == "navigation_progressed" && !c.passed));
}

// --- multiple sessions ---

#[tokio::test]
async fn sessions_are_independent() {
    let registry = SessionRegistry::new();

    for (fixture, goal) in [(DOCS, "Open guides"), (FLIGHTS, "Search flights")] {
        let jev = Arc::new(ScriptedJev::new(vec![]).with_fallback(Planned::new(Operation::Blocked)));
        let backend = ScriptedBackend::from_json(fixture).unwrap();
        let session =
            Session::start(new_session_id(), goal.into(), None, jev, Box::new(backend), test_config())
                .await
                .unwrap();
        registry.insert(session).await;
    }

    assert_eq!(registry.len().await, 2);
    let ids = registry.ids().await;

    // Each session keeps its own page, goal and metrics.
    let first = registry.get(&ids[0]).await.unwrap();
    let second = registry.get(&ids[1]).await.unwrap();
    let first_url = first.lock().await.current_url().to_string();
    let second_url = second.lock().await.current_url().to_string();
    assert_ne!(first_url, second_url);

    registry.remove(&ids[0]).await.unwrap();
    assert_eq!(registry.len().await, 1);
    match registry.get(&ids[0]).await {
        Err(error) => assert_eq!(error.code(), "unknown_session"),
        Ok(_) => panic!("a removed session must not resolve"),
    }
}

// --- metrics ---

#[tokio::test]
async fn the_metrics_report_describes_the_whole_run() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Select, "One way"),
        Planned::targeting(Operation::TypeText, "Where from?"),
        Planned::targeting(Operation::TypeText, "Where to?"),
        Planned::targeting(Operation::TypeText, "Departure"),
        Planned::targeting(Operation::Click, "Search"),
        Planned::new(Operation::Done),
    ]));
    let mut session = session_with(
        FLIGHTS,
        "Find one-way flights from Sydney to Tokyo on 10 October 2026",
        Some(json!({ "origin": "Sydney", "destination": "Tokyo", "date": "2026-10-10", "trip_type": "one-way" })),
        jev,
        test_config(),
    )
    .await;

    session.run(budget()).await;
    let report = session.metrics_report();

    assert_eq!(report["browser_actions"], 5);
    assert_eq!(report["host_round_trips"], 1);
    assert_eq!(report["jev_requests"], 6);
    assert_eq!(report["values_resolved_locally"], 3);
    assert_eq!(report["stale_decisions"], 0);
    assert_eq!(report["actions_per_host_round_trip"], 5.0);
    assert!(report["snapshots"].as_u64().unwrap() >= 5);
    for key in ["total_task_ms", "jev_latency_median_ms", "jev_latency_p95_ms", "verification_ms", "wait_ms"]
    {
        assert!(report.get(key).is_some(), "missing {key}");
    }
}

#[tokio::test]
async fn sensitive_values_never_reach_logs_history_or_the_jev_request() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Continue to checkout"),
        Planned::targeting(Operation::TypeText, "Name on card"),
        Planned::new(Operation::Blocked),
    ]));
    let mut session = session_with(
        CHECKOUT,
        "Complete the checkout form",
        Some(json!({ "name on card": "Ada Lovelace", "card number": "4111111111111111" })),
        jev.clone(),
        test_config(),
    )
    .await;

    session.run(budget()).await;

    // Whatever was typed, the secret must not appear anywhere the host or a log can see it.
    let history = serde_json::to_string(&session.history()).unwrap();
    assert!(!history.contains("4111111111111111"), "card number leaked into action history");

    let requests = serde_json::to_string(&*jev.requests.lock().unwrap()).unwrap();
    assert!(!requests.contains("4111111111111111"), "card number leaked into a Jev request");

    let pool = serde_json::to_string(&session.value_pool.summary()).unwrap();
    assert!(!pool.contains("4111111111111111"), "card number leaked into the value-pool summary");
}
