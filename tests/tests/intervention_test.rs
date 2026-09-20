//! Pause and resume: the three ways the runtime hands control back to the host, and how it
//! carries on from the same session afterwards.

use jev_browser_relay_core::session::SessionStatus;
use jev_browser_relay_core::session::{RunBudget, RunOutcome};
use jev_browser_relay_core::snapshot::Operation;
use jev_browser_relay_core::testing::{Planned, ScriptedJev};
use jev_browser_relay_tests::*;
use serde_json::json;
use std::sync::Arc;

fn budget() -> RunBudget {
    RunBudget { max_steps: 40, max_duration_ms: 30_000 }
}

// --- NEEDS_INPUT ---

#[tokio::test]
async fn an_unresolvable_field_pauses_for_the_host_then_resumes() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::TypeText, "Where from?"),
        Planned::targeting(Operation::TypeText, "Where to?"),
        // Re-asked after the host supplies the value: the interrupted decision spent a request.
        Planned::targeting(Operation::TypeText, "Where to?"),
        Planned::targeting(Operation::TypeText, "Departure"),
        Planned::targeting(Operation::Click, "Search"),
        Planned::new(Operation::Done),
    ]));
    // Deliberately missing the destination.
    let mut session = session_with(
        FLIGHTS,
        "Find one-way flights from Sydney on 10 October 2026",
        Some(json!({ "origin": "Sydney", "date": "2026-10-10" })),
        jev,
        test_config(),
    )
    .await;

    let outcome = session.run(budget()).await;
    let RunOutcome::NeedsInput { request_id, field, .. } = outcome else {
        panic!("expected NeedsInput, got {outcome:?}");
    };
    assert_eq!(field.label, "Where to?");
    assert_eq!(session.status, SessionStatus::NeedsInput);
    // It paused *before* typing anything into the unknown field.
    assert_eq!(session.metrics.browser_actions, 1, "only the origin should have been typed");

    session.provide_input(&request_id, "Tokyo", true).expect("input accepted");
    assert_eq!(session.status, SessionStatus::Ready);

    // The same session continues; the task is not restarted.
    let outcome = session.run(budget()).await;
    assert!(matches!(outcome, RunOutcome::Done { .. }), "got {outcome:?}");
    assert_eq!(session.metrics.browser_actions, 4, "the origin was not retyped");
    assert_eq!(session.metrics.host_input_requests, 1);

    let typed: Vec<_> = session
        .history()
        .iter()
        .filter(|r| r.operation == Operation::TypeText)
        .map(|r| r.text.clone().unwrap_or_default())
        .collect();
    assert_eq!(typed, vec!["Sydney", "Tokyo", "2026-10-10"]);
}

#[tokio::test]
async fn a_supplied_value_is_reused_for_every_synonym_of_the_same_field() {
    // Eight fields, only three of them in the task context.
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::TypeText, "First name"),
        Planned::targeting(Operation::TypeText, "Last name"),
        Planned::targeting(Operation::TypeText, "Work email"),
        Planned::targeting(Operation::TypeText, "Company"),
        Planned::targeting(Operation::TypeText, "Phone number"),
        Planned::targeting(Operation::TypeText, "City"),
        Planned::targeting(Operation::Select, "Australia"),
        Planned::targeting(Operation::TypeText, "How did you hear about us?"),
        Planned::targeting(Operation::TypeText, "How did you hear about us?"),
        Planned::targeting(Operation::Click, "Create account"),
        Planned::new(Operation::Done),
    ]));
    let mut session = session_with(
        SIGNUP,
        "Create an account for Ada Lovelace",
        Some(json!({
            "given_name": "Ada",
            "surname": "Lovelace",
            "email": "ada@analytical.test",
            "employer": "Analytical Engines",
            "mobile": "+61 400 000 000",
            "town": "Sydney"
        })),
        jev,
        test_config(),
    )
    .await;

    // Only the free-text "How did you hear about us?" has no semantic match.
    let outcome = session.run(budget()).await;
    let RunOutcome::NeedsInput { request_id, field, .. } = outcome else {
        panic!("expected one NeedsInput, got {outcome:?}");
    };
    assert_eq!(field.label, "How did you hear about us?");
    // Six fields resolved from keys that never matched a label literally.
    assert_eq!(session.metrics.values_resolved_locally, 6);

    session.provide_input(&request_id, "A colleague", true).unwrap();
    let outcome = session.run(budget()).await;

    assert!(matches!(outcome, RunOutcome::Done { .. }), "got {outcome:?}");
    assert_eq!(session.metrics.host_round_trips, 3, "start-run, one input, one resume");
    assert_eq!(session.metrics.host_input_requests, 1, "only the genuinely unknown field was asked about");
    // Eight fields filled in total: seven resolved locally, one supplied by the host.
    assert_eq!(session.metrics.browser_actions, 9, "eight fields plus the submit");
    let typed: Vec<String> = session
        .history()
        .iter()
        .filter(|r| r.operation == Operation::TypeText)
        .map(|r| r.text.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        typed,
        vec![
            "Ada",
            "Lovelace",
            "ada@analytical.test",
            "Analytical Engines",
            "+61 400 000 000",
            "Sydney",
            "A colleague"
        ]
    );
}

#[tokio::test]
async fn answering_the_wrong_request_id_is_refused() {
    let jev = Arc::new(ScriptedJev::new(vec![Planned::targeting(Operation::TypeText, "Where to?")]));
    let mut session =
        session_with(FLIGHTS, "Find flights", Some(json!({ "unrelated": "x" })), jev, test_config()).await;

    let outcome = session.run(budget()).await;
    let RunOutcome::NeedsInput { request_id, .. } = outcome else { panic!("expected NeedsInput") };

    let error = session.provide_input("req_wrong", "Tokyo", true).expect_err("must be refused");
    assert_eq!(error.code(), "unknown_request");
    // Answering the wrong kind of request is refused too.
    assert_eq!(session.provide_reasoning(&request_id, "go left").unwrap_err().code(), "wrong_state");
    // The real request still works.
    session.provide_input(&request_id, "Tokyo", true).expect("correct id accepted");
}

// --- NEEDS_REASONING ---

#[tokio::test]
async fn a_genuinely_ambiguous_choice_escalates_to_the_host() {
    // Three identical-looking invoices: the target distribution is nearly flat.
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Invoice 1041").ambiguous(),
        Planned::targeting(Operation::Click, "Invoice 1042"),
        Planned::new(Operation::Done),
    ]));
    let mut session =
        session_with(AMBIGUOUS, "Open the correct outstanding invoice", None, jev, test_config()).await;

    let outcome = session.run(budget()).await;
    let RunOutcome::NeedsReasoning { request_id, reason, candidate_actions, page_summary } = outcome else {
        panic!("expected NeedsReasoning, got {outcome:?}");
    };
    assert!(reason.contains("near-equally plausible"), "reason was: {reason}");
    assert_eq!(candidate_actions.len(), 3, "the host should see the real alternatives");
    assert!(page_summary.contains("Invoices"));
    // Nothing was clicked while it was ambiguous.
    assert_eq!(session.metrics.browser_actions, 0);

    session
        .provide_reasoning(&request_id, "Use invoice 1042; it is the one for the disputed order.")
        .unwrap();
    let outcome = session.run(budget()).await;

    assert!(matches!(outcome, RunOutcome::Done { .. }), "got {outcome:?}");
    assert_eq!(session.metrics.host_reasoning_requests, 1);
    assert!(session.current_url().ends_with("/detail"));
}

#[tokio::test]
async fn host_guidance_reaches_jev_once_and_only_once() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Invoice 1041").ambiguous(),
        Planned::targeting(Operation::Click, "Invoice 1042"),
        Planned::targeting(Operation::Click, "Back to invoices"),
        Planned::new(Operation::Done),
    ]));
    let mut session = session_with(AMBIGUOUS, "Open an invoice", None, jev.clone(), test_config()).await;

    let RunOutcome::NeedsReasoning { request_id, .. } = session.run(budget()).await else {
        panic!("expected NeedsReasoning");
    };
    session.provide_reasoning(&request_id, "Prefer invoice 1042").unwrap();
    session.run(budget()).await;

    let requests = jev.requests.lock().unwrap();
    let carrying: Vec<usize> = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| {
            serde_json::to_string(&request.questions).unwrap().contains("Prefer invoice 1042")
        })
        .map(|(index, _)| index)
        .collect();
    // Guidance steers the next decision only. Leaving it in place would bias every later step
    // against a page the host never saw.
    assert_eq!(carrying.len(), 1, "guidance leaked into {} requests", carrying.len());
}

// --- NEEDS_CONFIRMATION ---

#[tokio::test]
async fn a_consequential_action_is_gated_until_the_host_approves() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Continue to checkout"),
        Planned::targeting(Operation::TypeText, "Name on card"),
        Planned::targeting(Operation::Click, "Place order"),
        // Re-asked after the host approves: the gated decision spent a request.
        Planned::targeting(Operation::Click, "Place order"),
        Planned::new(Operation::Done),
    ]));
    let mut session = session_with(
        CHECKOUT,
        "Buy the item in the cart",
        Some(json!({ "name on card": "Ada Lovelace" })),
        jev,
        test_config(),
    )
    .await;

    let outcome = session.run(budget()).await;
    let RunOutcome::NeedsConfirmation { request_id, action, consequence, question, .. } = outcome else {
        panic!("expected NeedsConfirmation, got {outcome:?}");
    };
    assert!(action.contains("Place order"), "{action}");
    assert_eq!(consequence, "places an order or booking");
    assert!(question.contains("Approve this action?"));
    assert_eq!(session.status, SessionStatus::NeedsConfirmation);
    // The order was NOT placed.
    assert!(!session.current_url().ends_with("/confirmed"));

    session.confirm(&request_id, true).expect("approval accepted");
    let outcome = session.run(budget()).await;

    assert!(matches!(outcome, RunOutcome::Done { .. }), "got {outcome:?}");
    assert!(session.current_url().ends_with("/confirmed"), "the approved order should have gone through");
    assert_eq!(session.metrics.host_confirmations, 1);
}

#[tokio::test]
async fn declining_a_consequential_action_blocks_the_session() {
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Continue to checkout"),
        Planned::targeting(Operation::Click, "Place order"),
    ]));
    let mut session = session_with(CHECKOUT, "Buy the item", None, jev, test_config()).await;

    let RunOutcome::NeedsConfirmation { request_id, .. } = session.run(budget()).await else {
        panic!("expected NeedsConfirmation");
    };
    session.confirm(&request_id, false).expect("refusal accepted");

    assert_eq!(session.status, SessionStatus::Blocked);
    assert!(!session.current_url().ends_with("/confirmed"));
    // A blocked session refuses to run on.
    let outcome = session.run(budget()).await;
    assert!(matches!(outcome, RunOutcome::Error { .. }), "got {outcome:?}");
}

#[tokio::test]
async fn an_approval_does_not_carry_to_a_second_consequential_action() {
    // Approving "Place order" must not silently approve the next gated control.
    let jev = Arc::new(ScriptedJev::new(vec![
        Planned::targeting(Operation::Click, "Continue to checkout"),
        Planned::targeting(Operation::Click, "Place order"),
        Planned::targeting(Operation::Click, "Place order"),
        Planned::targeting(Operation::Click, "Delete this order"),
    ]));
    let mut session =
        session_with(CHECKOUT, "Buy the item, then remove the order", None, jev, test_config()).await;

    let RunOutcome::NeedsConfirmation { request_id, .. } = session.run(budget()).await else {
        panic!("expected the first NeedsConfirmation");
    };
    session.confirm(&request_id, true).unwrap();

    // The approved order goes through, and the *next* consequential control gates again.
    let outcome = session.run(budget()).await;
    let RunOutcome::NeedsConfirmation { action, consequence, .. } = outcome else {
        panic!("a second consequential action must be gated again, got {outcome:?}");
    };
    assert!(action.contains("Delete this order"), "{action}");
    assert_eq!(consequence, "deletes data");
    assert!(session.current_url().ends_with("/confirmed"), "the first, approved order did go through");
    assert_eq!(session.metrics.host_confirmations, 2);
}
