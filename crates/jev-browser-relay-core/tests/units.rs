//! Unit coverage for the pure parts: value resolution, the choice space, Jev response
//! validation, the safety classifier, and bounded history.

use jev_browser_relay_core::action_space::ActionSpace;
use jev_browser_relay_core::history::Bounded;
use jev_browser_relay_core::jev::{self, JevRawResponse};
use jev_browser_relay_core::safety::{ConsequenceClass, SafetyPolicy};
use jev_browser_relay_core::snapshot::{
    ActionKind, FieldDescription, Operation, RawAction, Scroll, Snapshot,
};
use jev_browser_relay_core::value_pool::{normalize, ValuePool, ValueSource};
use serde_json::json;
use std::collections::BTreeMap;

fn field(label: &str) -> FieldDescription {
    FieldDescription {
        element_index: None,
        label: label.into(),
        role: Some("textbox".into()),
        current_value: None,
    }
}

fn pool_with(context: serde_json::Value) -> ValuePool {
    let mut pool = ValuePool::new();
    pool.load_context(&context);
    pool
}

// --- value pool ---

#[test]
fn exact_and_alias_keys_resolve() {
    let pool = pool_with(json!({ "origin": "Sydney", "destination": "Tokyo", "email": "a@b.test" }));

    assert_eq!(pool.resolve(&field("origin")).unwrap().value, "Sydney");
    // The field label is nothing like the key, but they mean the same thing.
    assert_eq!(pool.resolve(&field("Where from?")).unwrap().value, "Sydney");
    assert_eq!(pool.resolve(&field("Leaving from")).unwrap().value, "Sydney");
    assert_eq!(pool.resolve(&field("Where to?")).unwrap().value, "Tokyo");
    assert_eq!(pool.resolve(&field("Work email")).unwrap().value, "a@b.test");
}

#[test]
fn distinct_meanings_never_cross_resolve() {
    let pool = pool_with(json!({ "departure_date": "2026-10-10" }));

    assert!(pool.resolve(&field("Departure")).is_some());
    // The two share a token but mean different things; typing the outbound date into the
    // return field would silently corrupt the task.
    assert!(pool.resolve(&field("Return date")).is_none(), "outbound date must not fill a return field");
}

#[test]
fn an_unknown_field_does_not_resolve() {
    let pool = pool_with(json!({ "origin": "Sydney" }));
    assert!(pool.resolve(&field("Seat preference")).is_none());
    assert!(pool.resolve(&field("Frequent flyer number")).is_none());
}

#[test]
fn nested_context_flattens_without_hardcoded_names() {
    let pool = pool_with(json!({
        "traveller": { "first_name": "Ada", "last_name": "Lovelace" },
        "tags": ["window", "aisle"]
    }));

    assert_eq!(pool.resolve(&field("First name")).unwrap().value, "Ada");
    assert_eq!(pool.resolve(&field("Surname")).unwrap().value, "Lovelace");
    assert_eq!(pool.get("tags").unwrap().value, "window, aisle");
}

#[test]
fn a_field_already_holding_the_value_is_left_alone() {
    let pool = pool_with(json!({ "origin": "Sydney" }));
    let mut already = field("Where from?");
    already.current_value = Some("Sydney".into());

    assert!(pool.resolve(&already).is_none(), "retyping an identical value is a wasted action");
}

#[test]
fn host_supplied_values_are_cached_under_their_meaning() {
    let mut pool = pool_with(json!({}));
    pool.insert("Destination", "Tokyo", ValueSource::HostInput, None);

    // Asked once about "Destination", answered for every synonym afterwards.
    assert_eq!(pool.resolve(&field("Where to?")).unwrap().value, "Tokyo");
    assert_eq!(pool.resolve(&field("Going to")).unwrap().value, "Tokyo");
}

#[test]
fn sensitive_values_are_flagged_and_redacted() {
    let mut pool = pool_with(json!({}));
    pool.insert("password", "hunter2", ValueSource::HostInput, None);
    pool.insert("card number", "4111111111111111", ValueSource::HostInput, None);

    let entry = pool.get("password").unwrap();
    assert!(entry.sensitive);
    assert_eq!(entry.redacted(), "<redacted>");
    assert!(pool.get("card number").unwrap().sensitive);

    // Nothing sensitive reaches the host-facing summary or the verification surface.
    let rendered = serde_json::to_string(&pool.summary()).unwrap();
    assert!(!rendered.contains("hunter2"));
    assert!(!rendered.contains("4111"));
    assert!(pool.assertable().iter().all(|e| !e.sensitive));
}

#[test]
fn derived_facts_lose_to_real_context() {
    let mut pool = pool_with(json!({ "query": "rust async" }));
    pool.load_session_facts("https://example.test", "Example", "2026-09-20");

    assert_eq!(pool.resolve(&field("Search")).unwrap().value, "rust async");
}

#[test]
fn normalization_keeps_meaningful_tokens() {
    assert_eq!(normalize("Where from?"), "where from");
    assert_eq!(normalize("  E-mail Address "), "e mail address");
    // "type" carries meaning; it must survive normalization.
    assert_eq!(normalize("trip_type"), "trip type");
}

// --- action space ---

fn snapshot_with(actions: Vec<RawAction>) -> Snapshot {
    Snapshot {
        url: "https://example.test".into(),
        title: "Example".into(),
        text: String::new(),
        scroll: Scroll::default(),
        actions,
        marker: json!("m"),
        page_key: json!("k"),
        guards: BTreeMap::new(),
        omitted_actions: 0,
        can_go_back: false,
        generation: 1,
        screenshot: None,
    }
}

fn action(id: &str, kind: ActionKind, label: &str, node: i64) -> RawAction {
    RawAction {
        id: id.into(),
        kind,
        label: label.into(),
        node: Some(node),
        role: None,
        value: None,
        current_value: None,
        checked: None,
        selected: None,
        expanded: None,
        delta: None,
    }
}

#[test]
fn each_operation_offers_only_compatible_targets() {
    let snapshot = snapshot_with(vec![
        action("e1", ActionKind::Fill, "Email", 1),
        action("e2", ActionKind::Click, "Open Email", 1),
        action("e3", ActionKind::Click, "Submit", 2),
    ]);
    let space = ActionSpace::build(&snapshot);

    // Two actions on one node collapse to one element row.
    assert_eq!(space.elements.len(), 2);
    let click_targets = &space.targets[&Operation::Click];
    let type_targets = &space.targets[&Operation::TypeText];

    assert_eq!(click_targets.len(), 2, "both clickables offered");
    assert_eq!(type_targets.len(), 1, "only the editable field is typeable");
    assert!(type_targets.values().all(|id| id == "e1"));
}

#[test]
fn every_select_option_is_its_own_target() {
    let mut a = action("e1o1", ActionKind::Select, "Country → Australia", 1);
    a.value = Some("AU".into());
    let mut b = action("e1o2", ActionKind::Select, "Country → Japan", 1);
    b.value = Some("JP".into());
    let space = ActionSpace::build(&snapshot_with(vec![a, b]));

    let targets = &space.targets[&Operation::Select];
    assert_eq!(targets.len(), 2);
    // A chosen target resolves to exactly one option, never merely to the element.
    assert_eq!(space.resolve(Operation::Select, Some("1:1")), Some("e1o1"));
    assert_eq!(space.resolve(Operation::Select, Some("1:2")), Some("e1o2"));
    assert_eq!(space.elements[0].options.as_ref().unwrap().len(), 2);
}

#[test]
fn back_is_offered_only_when_there_is_history() {
    let mut snapshot = snapshot_with(vec![action("e1", ActionKind::Click, "Next", 1)]);
    assert!(!ActionSpace::build(&snapshot).offered_operations().contains(&Operation::Back));

    snapshot.can_go_back = true;
    assert!(ActionSpace::build(&snapshot).offered_operations().contains(&Operation::Back));
}

// --- Jev response validation ---

fn click_space() -> ActionSpace {
    ActionSpace::build(&snapshot_with(vec![
        action("e1", ActionKind::Click, "Alpha", 1),
        action("e2", ActionKind::Click, "Beta", 2),
    ]))
}

fn response(answers: serde_json::Value) -> JevRawResponse {
    JevRawResponse {
        answers: serde_json::from_value(answers).unwrap(),
        model: "test".into(),
        usage: json!(null),
    }
}

#[test]
fn a_well_formed_answer_resolves_to_an_observed_action() {
    let space = click_space();
    let decision = jev::validate_response(
        &response(json!({
            "operation": { "choice": "CLICK", "confidence": 0.9,
                           "probabilities": { "CLICK": 0.9, "DONE": 0.05, "BLOCKED": 0.05 } },
            "click_target": { "choice": "2", "confidence": 0.8, "probabilities": { "1": 0.2, "2": 0.8 } }
        })),
        &space,
        7,
        12,
    )
    .expect("valid response");

    assert_eq!(decision.operation, Operation::Click);
    assert_eq!(decision.action_id.as_deref(), Some("e2"));
    assert_eq!(decision.generation, 7);
    assert!((decision.target_margin().unwrap() - 0.6).abs() < 1e-9, "top-two margin should be 0.6");
}

#[test]
fn answers_outside_the_offered_space_are_rejected() {
    let space = click_space();
    let cases = vec![
        // A choice that was never offered — the path by which a selector or a URL could sneak in.
        json!({ "operation": { "choice": "EXECUTE_JS", "confidence": 0.9,
                               "probabilities": { "CLICK": 0.9, "DONE": 0.05, "BLOCKED": 0.05 } } }),
        // Probabilities over the wrong key set.
        json!({ "operation": { "choice": "CLICK", "confidence": 0.9, "probabilities": { "CLICK": 1.0 } } }),
        // A distribution that does not sum to 1.
        json!({ "operation": { "choice": "CLICK", "confidence": 0.9,
                               "probabilities": { "CLICK": 0.9, "DONE": 0.9, "BLOCKED": 0.9 } } }),
        // The choice is not the argmax.
        json!({ "operation": { "choice": "DONE", "confidence": 0.9,
                               "probabilities": { "CLICK": 0.9, "DONE": 0.05, "BLOCKED": 0.05 } } }),
        // Confidence out of range.
        json!({ "operation": { "choice": "CLICK", "confidence": 4.2,
                               "probabilities": { "CLICK": 0.9, "DONE": 0.05, "BLOCKED": 0.05 } } }),
    ];

    for case in cases {
        let error = jev::validate_response(&response(case.clone()), &space, 1, 1)
            .expect_err(&format!("should have rejected {case}"));
        assert_eq!(error.code(), "jev_invalid_response");
        assert!(error.to_string().contains("no action executed"), "{error}");
    }
}

#[test]
fn a_missing_target_head_is_rejected_but_an_unused_one_is_ignored() {
    let space = click_space();

    // CLICK was chosen, so its target head must be present and valid.
    assert!(jev::validate_response(
        &response(json!({
            "operation": { "choice": "CLICK", "confidence": 0.9,
                           "probabilities": { "CLICK": 0.9, "DONE": 0.05, "BLOCKED": 0.05 } }
        })),
        &space,
        1,
        1
    )
    .is_err());

    // DONE was chosen, so a malformed click_target cannot cause an action and must not fail it.
    let decision = jev::validate_response(
        &response(json!({
            "operation": { "choice": "DONE", "confidence": 0.9,
                           "probabilities": { "CLICK": 0.05, "DONE": 0.9, "BLOCKED": 0.05 } },
            "click_target": { "choice": "nonsense", "confidence": 12.0, "probabilities": {} }
        })),
        &space,
        1,
        1,
    )
    .expect("unused heads are speculative");
    assert_eq!(decision.operation, Operation::Done);
}

// --- safety ---

fn gate(label: &str, page_text: &str) -> Option<ConsequenceClass> {
    let policy = SafetyPolicy::default();
    let mut snapshot = snapshot_with(vec![]);
    snapshot.text = page_text.into();
    policy.classify(&action("e1", ActionKind::Click, label, 1), &snapshot).map(|f| f.class)
}

#[test]
fn consequential_controls_are_gated() {
    assert_eq!(gate("Place order", ""), Some(ConsequenceClass::Order));
    assert_eq!(gate("Buy now", ""), Some(ConsequenceClass::Purchase));
    assert_eq!(gate("Pay now", ""), Some(ConsequenceClass::Payment));
    assert_eq!(gate("Delete account", ""), Some(ConsequenceClass::Delete));
    assert_eq!(gate("Send message", ""), Some(ConsequenceClass::Send));
    assert_eq!(gate("Publish", ""), Some(ConsequenceClass::Publish));
}

#[test]
fn ordinary_navigation_is_not_gated() {
    assert_eq!(gate("Search", ""), None);
    assert_eq!(gate("Next page", ""), None);
    assert_eq!(gate("View receipt", ""), None);
    // Whole-word matching: "repay" must not trip the "pay" rule.
    assert_eq!(gate("Repayment schedule", ""), None);
}

#[test]
fn ambiguous_verbs_are_gated_only_where_the_page_says_they_matter() {
    assert_eq!(gate("Submit", "Search our documentation"), None);
    // A real finalization signal on the page corroborates the weak verb.
    assert_eq!(gate("Submit", "Billing details. Card number ending 4242."), Some(ConsequenceClass::Submit));
    assert_eq!(gate("Confirm", "Enter your card number and CVV"), Some(ConsequenceClass::Submit));
}

#[test]
fn navigating_towards_a_purchase_is_not_gated() {
    // "Order summary" and "total due" appear on ordinary cart pages. Treating them as
    // finalization signals would gate every "Continue" in a shopping flow and spend a host round
    // trip on a reversible click. The gate belongs on the finalizing control itself.
    assert_eq!(gate("Continue to checkout", "Your cart. Order summary: total due $89.00"), None);
    assert_eq!(gate("Proceed to payment", "Order summary: total due $89.00"), None);
    // The control it leads to is gated.
    assert_eq!(gate("Place order", "Billing details"), Some(ConsequenceClass::Order));
}

#[test]
fn typing_and_waiting_are_never_gated() {
    let policy = SafetyPolicy::default();
    let mut snapshot = snapshot_with(vec![]);
    snapshot.text = "Order summary: total due $89.00".into();

    // The gate belongs on the submit, not on filling the field before it.
    assert!(policy.classify(&action("e1", ActionKind::Fill, "Buy quantity", 1), &snapshot).is_none());
    assert!(policy.classify(&action("e2", ActionKind::Wait, "Wait", 2), &snapshot).is_none());
}

// --- bounded history ---

#[test]
fn history_never_grows_without_bound() {
    let mut bounded = Bounded::new(3);
    for n in 0..100 {
        bounded.push(n);
    }
    assert_eq!(bounded.len(), 3);
    assert_eq!(bounded.to_vec(), vec![97, 98, 99]);
    assert_eq!(bounded.recent(2), vec![&98, &99]);
}
