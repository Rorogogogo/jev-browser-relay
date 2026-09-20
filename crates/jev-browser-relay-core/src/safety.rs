//! Consequential-action gating.
//!
//! The runtime must never perform an irreversible action just because the policy model chose it.
//! Classification is deterministic and conservative: a keyword match on the control's accessible
//! name (with its role and the page context as corroboration) is enough to pause. False
//! positives cost one host confirmation; false negatives cost real money or real data, so the
//! bias is deliberate.

use crate::snapshot::{ActionKind, Operation, RawAction, Snapshot};
use serde::{Deserialize, Serialize};

/// Why an action was gated, so the host can decide quickly.
#[derive(Debug, Clone, PartialEq, Eq, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsequenceClass {
    Purchase,
    Order,
    Payment,
    Send,
    Delete,
    Publish,
    AccountChange,
    Submit,
}

impl ConsequenceClass {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Purchase => "completes a purchase",
            Self::Order => "places an order or booking",
            Self::Payment => "moves money or changes payment details",
            Self::Send => "sends a message that cannot be recalled",
            Self::Delete => "deletes data",
            Self::Publish => "publishes content publicly",
            Self::AccountChange => "changes account or security settings",
            Self::Submit => "submits a consequential form",
        }
    }
}

/// Phrases that gate an action, paired with what they mean. Matched against the normalized
/// accessible name of the control.
const CONSEQUENTIAL: &[(&str, ConsequenceClass)] = &[
    ("buy now", ConsequenceClass::Purchase),
    ("buy", ConsequenceClass::Purchase),
    ("purchase", ConsequenceClass::Purchase),
    ("complete purchase", ConsequenceClass::Purchase),
    ("place order", ConsequenceClass::Order),
    ("place your order", ConsequenceClass::Order),
    ("confirm order", ConsequenceClass::Order),
    ("confirm and book", ConsequenceClass::Order),
    ("book now", ConsequenceClass::Order),
    ("reserve", ConsequenceClass::Order),
    // "Continue to checkout" and "Proceed to payment" only *navigate* to a form. Gating them
    // costs a host round trip and buys nothing, because the irreversible control is the one on
    // the page they lead to. The gate belongs on the finalizing verb, not on the approach to it.
    ("pay now", ConsequenceClass::Payment),
    ("pay", ConsequenceClass::Payment),
    ("confirm payment", ConsequenceClass::Payment),
    ("transfer", ConsequenceClass::Payment),
    ("send money", ConsequenceClass::Payment),
    ("subscribe", ConsequenceClass::Payment),
    ("start subscription", ConsequenceClass::Payment),
    ("send", ConsequenceClass::Send),
    ("send message", ConsequenceClass::Send),
    ("send email", ConsequenceClass::Send),
    ("post", ConsequenceClass::Publish),
    ("publish", ConsequenceClass::Publish),
    ("tweet", ConsequenceClass::Publish),
    ("share publicly", ConsequenceClass::Publish),
    ("delete", ConsequenceClass::Delete),
    ("delete account", ConsequenceClass::Delete),
    ("remove permanently", ConsequenceClass::Delete),
    ("erase", ConsequenceClass::Delete),
    ("destroy", ConsequenceClass::Delete),
    ("change password", ConsequenceClass::AccountChange),
    ("update password", ConsequenceClass::AccountChange),
    ("disable two factor", ConsequenceClass::AccountChange),
    ("revoke", ConsequenceClass::AccountChange),
    ("close account", ConsequenceClass::AccountChange),
    ("cancel subscription", ConsequenceClass::AccountChange),
];

/// Page-context words that corroborate a weak match. "Submit" on a billing page is gated;
/// "Submit" on a search page is not.
///
/// These are finalization signals, not commerce signals: "order summary" and "total due" appear
/// on ordinary cart pages, so including them would gate every "Continue" in a shopping flow.
const CONSEQUENTIAL_CONTEXT: &[&str] =
    &["billing", "credit card", "card number", "payment details", "confirm and pay", "cvv", "security code"];

/// Ambiguous verbs that only gate when the page context agrees.
const WEAK: &[(&str, ConsequenceClass)] = &[
    ("submit", ConsequenceClass::Submit),
    ("confirm", ConsequenceClass::Submit),
    ("continue", ConsequenceClass::Submit),
    ("agree", ConsequenceClass::Submit),
];

#[derive(Debug, Clone, Serialize)]
pub struct ConsequenceFinding {
    pub class: ConsequenceClass,
    /// The phrase that triggered the gate.
    pub matched: String,
    pub control_label: String,
    pub url: String,
}

impl ConsequenceFinding {
    pub fn question(&self) -> String {
        format!("\"{}\" {} on {}. Approve this action?", self.control_label, self.class.describe(), self.url)
    }
}

#[derive(Debug, Clone)]
pub struct SafetyPolicy {
    /// When false, the gate is inert. Only ever set by explicit configuration.
    pub enabled: bool,
    /// Extra phrases supplied by configuration, all treated as `Submit`-class.
    pub extra_phrases: Vec<String>,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self { enabled: true, extra_phrases: Vec::new() }
    }
}

impl SafetyPolicy {
    /// Classify an action about to execute. `None` means it may proceed unattended.
    pub fn classify(&self, action: &RawAction, snapshot: &Snapshot) -> Option<ConsequenceFinding> {
        if !self.enabled {
            return None;
        }
        // Typing, scrolling and waiting are not consequential in themselves; the submit that
        // follows is where the gate belongs.
        if !matches!(action.kind, ActionKind::Click | ActionKind::Select) {
            return None;
        }

        let label = crate::value_pool::normalize(action.element_label());
        if label.is_empty() {
            return None;
        }
        let page =
            crate::value_pool::normalize(&format!("{} {} {}", snapshot.title, snapshot.url, snapshot.text));
        let risky_page =
            CONSEQUENTIAL_CONTEXT.iter().any(|c| page.contains(&crate::value_pool::normalize(c)));

        for phrase in &self.extra_phrases {
            let needle = crate::value_pool::normalize(phrase);
            if !needle.is_empty() && contains_phrase(&label, &needle) {
                return Some(self.finding(ConsequenceClass::Submit, &needle, action, snapshot));
            }
        }

        // Longest phrase first so "place order" reports better than "order".
        let mut matches: Vec<&(&str, ConsequenceClass)> = CONSEQUENTIAL
            .iter()
            .filter(|(phrase, _)| contains_phrase(&label, &crate::value_pool::normalize(phrase)))
            .collect();
        matches.sort_by_key(|(phrase, _)| std::cmp::Reverse(phrase.len()));
        if let Some((phrase, class)) = matches.first() {
            return Some(self.finding(*class, phrase, action, snapshot));
        }

        if risky_page {
            for (phrase, class) in WEAK {
                if contains_phrase(&label, &crate::value_pool::normalize(phrase)) {
                    return Some(self.finding(*class, phrase, action, snapshot));
                }
            }
        }

        None
    }

    fn finding(
        &self,
        class: ConsequenceClass,
        matched: &str,
        action: &RawAction,
        snapshot: &Snapshot,
    ) -> ConsequenceFinding {
        ConsequenceFinding {
            class,
            matched: matched.to_string(),
            control_label: action.element_label().to_string(),
            url: snapshot.url.clone(),
        }
    }
}

/// Whole-word containment, so "repay" does not match "pay" and "undelete" does not match
/// "delete".
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hay: Vec<&str> = haystack.split_whitespace().collect();
    let pin: Vec<&str> = needle.split_whitespace().collect();
    if pin.is_empty() || pin.len() > hay.len() {
        return false;
    }
    hay.windows(pin.len()).any(|window| window == pin.as_slice())
}

/// Operations that can never be gated, because they cannot change anything.
pub fn always_safe(operation: Operation) -> bool {
    matches!(
        operation,
        Operation::Wait | Operation::ScrollUp | Operation::ScrollDown | Operation::Done | Operation::Blocked
    )
}
