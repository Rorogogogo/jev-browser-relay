//! Builds the bounded choice space offered to Jev.
//!
//! Two invariants make the policy safe:
//!
//! 1. Every offered target is an index into *this* snapshot's observed actions. Jev cannot name
//!    anything the runtime did not observe, so selectors, XPath, coordinates and code are not
//!    expressible in the answer format at all.
//! 2. Each operation gets its own target head containing only compatible elements — `CLICK` sees
//!    clickables, `TYPE_TEXT` sees editables, `SELECT` sees select options. Operation and target
//!    are answered in one request; only the head matching the chosen operation can execute.
//!
//! Ported from the action-space construction in `browser-use/jev-ultrafast` (MIT).

use crate::snapshot::{ActionKind, Operation, RawAction, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One row of the element table Jev sees. Indices are 1-based and stringified.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Element {
    pub index: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<String>,
    /// Which operations this element supports, in the Jev vocabulary.
    pub operations: Vec<String>,
    /// Present only for `<select>` elements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<SelectOption>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    /// `"<element index>:<option ordinal>"`.
    pub index: String,
    pub label: String,
    pub value: String,
}

/// The indexed, per-operation choice space for one snapshot.
#[derive(Debug, Clone, Default)]
pub struct ActionSpace {
    pub elements: Vec<Element>,
    /// operation → (target index → the raw action id that executes it)
    pub targets: BTreeMap<Operation, BTreeMap<String, String>>,
    /// Page-level controls keyed by their Jev operation name (`SCROLL_DOWN`, `WAIT`, …).
    pub controls: BTreeMap<Operation, String>,
}

impl ActionSpace {
    pub fn build(snapshot: &Snapshot) -> Self {
        let mut space = ActionSpace::default();
        // node id → element index, so repeated actions on one node collapse to one row.
        let mut indices: BTreeMap<i64, String> = BTreeMap::new();

        for action in &snapshot.actions {
            let Some(operation) = action.kind.operation() else {
                // A page-level control: scroll_up / scroll_down / wait.
                if let Some(op) = control_operation(action) {
                    space.controls.insert(op, action.id.clone());
                }
                continue;
            };

            let Some(node) = action.node else { continue };

            let index = match indices.get(&node) {
                Some(existing) => existing.clone(),
                None => {
                    let index = (space.elements.len() + 1).to_string();
                    indices.insert(node, index.clone());
                    space.elements.push(Element {
                        index: index.clone(),
                        label: action.element_label().to_string(),
                        role: action.role.clone(),
                        value: if action.kind == ActionKind::Select {
                            Some(action.current_value.clone().unwrap_or_default())
                        } else {
                            action.value.clone()
                        },
                        checked: action.checked.clone(),
                        selected: action.selected.clone(),
                        expanded: action.expanded.clone(),
                        operations: Vec::new(),
                        options: if action.kind == ActionKind::Select { Some(Vec::new()) } else { None },
                    });
                    index
                }
            };

            let position = index.parse::<usize>().unwrap_or(1) - 1;
            let element = &mut space.elements[position];
            let op_name = operation.as_str().to_string();
            if !element.operations.contains(&op_name) {
                element.operations.push(op_name);
            }

            // A select target addresses an observed *option*, not just the element, so the
            // executor always has a concrete option value that it re-verifies in the DOM.
            let target = if action.kind == ActionKind::Select {
                let options = element.options.get_or_insert_with(Vec::new);
                let target = format!("{index}:{}", options.len() + 1);
                options.push(SelectOption {
                    index: target.clone(),
                    label: action.label.clone(),
                    value: action.value.clone().unwrap_or_default(),
                });
                target
            } else {
                index.clone()
            };

            space.targets.entry(operation).or_default().insert(target, action.id.clone());
        }

        // BACK is a browser-level operation with no element behind it, so it is offered from the
        // snapshot's own navigation state rather than from an observed control.
        if snapshot.can_go_back {
            space.controls.entry(Operation::Back).or_insert_with(|| "back".to_string());
        }

        space
    }

    /// Resolve a chosen (operation, target) pair back to a raw action id.
    pub fn resolve(&self, operation: Operation, target: Option<&str>) -> Option<&str> {
        if operation.takes_target() {
            self.targets.get(&operation)?.get(target?).map(String::as_str)
        } else {
            self.controls.get(&operation).map(String::as_str)
        }
    }

    /// Every operation Jev may pick for this page: the target-bearing ones that actually have
    /// targets, the available page controls, plus the always-available terminals.
    pub fn offered_operations(&self) -> Vec<Operation> {
        let mut ops: Vec<Operation> = self.targets.keys().copied().collect();
        ops.extend(self.controls.keys().copied());
        ops.push(Operation::Done);
        ops.push(Operation::Blocked);
        ops.sort();
        ops.dedup();
        ops
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.controls.is_empty()
    }
}

fn control_operation(action: &RawAction) -> Option<Operation> {
    match action.id.as_str() {
        "scroll_up" => Some(Operation::ScrollUp),
        "scroll_down" => Some(Operation::ScrollDown),
        "wait" => Some(Operation::Wait),
        "back" => Some(Operation::Back),
        _ => match action.kind {
            ActionKind::Wait => Some(Operation::Wait),
            ActionKind::Scroll => {
                if action.delta.unwrap_or(0) < 0 {
                    Some(Operation::ScrollUp)
                } else {
                    Some(Operation::ScrollDown)
                }
            }
            _ => None,
        },
    }
}

/// Human-readable descriptions for each operation, sent as the Jev choice criteria.
pub fn operation_label(operation: Operation) -> &'static str {
    match operation {
        Operation::Click => "Click an element, button, menu option, autocomplete suggestion, or calendar day.",
        Operation::TypeText => "Enter or replace text in an editable field. The runtime supplies the value from the task context.",
        Operation::Select => "Select an observed dropdown value.",
        Operation::ScrollUp => "Scroll up.",
        Operation::ScrollDown => "Scroll down.",
        Operation::Wait => "Wait for the page to update.",
        Operation::Back => "Go back to the previous page.",
        Operation::Done => "Every requirement is visibly satisfied.",
        Operation::Blocked => "No supported operation can make progress.",
    }
}
