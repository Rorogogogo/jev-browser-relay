//! The compact structured page state the browser layer returns, and the typed actions
//! derived from it. Deliberately screenshot-free: the Jev loop never needs pixels.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a raw observed action does. Mirrors the `kind` field emitted by `snapshot.js`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Click,
    Fill,
    Select,
    Scroll,
    Wait,
}

impl ActionKind {
    /// The Jev-facing operation name this kind is offered under, if any.
    /// `Scroll`/`Wait` are page-level controls rather than element targets.
    pub fn operation(self) -> Option<Operation> {
        match self {
            Self::Click => Some(Operation::Click),
            Self::Fill => Some(Operation::TypeText),
            Self::Select => Some(Operation::Select),
            Self::Scroll | Self::Wait => None,
        }
    }

    /// Does executing this mutate the page?
    pub fn mutates(self) -> bool {
        !matches!(self, Self::Wait)
    }
}

/// The bounded operation vocabulary offered to Jev. Nothing outside this enum can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Operation {
    Click,
    TypeText,
    Select,
    ScrollUp,
    ScrollDown,
    Wait,
    Back,
    Done,
    Blocked,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "CLICK",
            Self::TypeText => "TYPE_TEXT",
            Self::Select => "SELECT",
            Self::ScrollUp => "SCROLL_UP",
            Self::ScrollDown => "SCROLL_DOWN",
            Self::Wait => "WAIT",
            Self::Back => "BACK",
            Self::Done => "DONE",
            Self::Blocked => "BLOCKED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "CLICK" => Self::Click,
            "TYPE_TEXT" => Self::TypeText,
            "SELECT" => Self::Select,
            "SCROLL_UP" => Self::ScrollUp,
            "SCROLL_DOWN" => Self::ScrollDown,
            "WAIT" => Self::Wait,
            "BACK" => Self::Back,
            "DONE" => Self::Done,
            "BLOCKED" => Self::Blocked,
            _ => return None,
        })
    }

    /// Operations that take an element target and therefore get their own Jev choice head.
    pub fn takes_target(self) -> bool {
        matches!(self, Self::Click | Self::TypeText | Self::Select)
    }

    /// Operations that end the run.
    pub fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Blocked)
    }
}

/// One executable action observed on the page. `node` is a *code-owned* DOM identity minted
/// inside the page; the model only ever picks an index that maps back to one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawAction {
    /// **Unique within a snapshot**: `e1`.., or `scroll_up` / `scroll_down` / `wait`. Each
    /// `<select>` option is its own action with its own id, so a chosen target always resolves
    /// to exactly one option.
    pub id: String,
    pub kind: ActionKind,
    pub label: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// For `select`, the option value to apply. Otherwise the field's current text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// For `select`, the currently selected option label(s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<String>,
    /// Scroll delta in CSS pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<i64>,
}

impl RawAction {
    /// The label with any `parent → option` suffix removed, on one line.
    pub fn element_label(&self) -> String {
        collapse_whitespace(self.label.split(" → ").next().unwrap_or(&self.label))
    }

    /// The full label, including any option suffix, on one line.
    pub fn display_label(&self) -> String {
        collapse_whitespace(&self.label)
    }

    /// A best-effort placeholder/label pair for value resolution and host prompts.
    pub fn describe(&self) -> FieldDescription {
        FieldDescription {
            element_index: None,
            label: self.element_label(),
            role: self.role.clone(),
            current_value: self.value.clone().filter(|v| !v.is_empty()),
        }
    }
}

/// How a paused field is described to the host in a `NEEDS_INPUT` reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldDescription {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element_index: Option<String>,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_value: Option<String>,
}

/// Fold every run of whitespace — including the newlines real accessible names are full of —
/// into single spaces. An unflattened label costs tokens in every Jev request, matches worse,
/// and makes any table of elements unreadable.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scroll {
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub height: f64,
}

/// One atomic observation of the page.
///
/// `marker`, `page_key` and `guards` are opaque freshness tokens produced in-page. The runtime
/// never interprets them; it only compares them, which is what makes stale-decision rejection
/// cheap and total.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub scroll: Scroll,
    #[serde(default)]
    pub actions: Vec<RawAction>,

    #[serde(default)]
    pub marker: serde_json::Value,
    #[serde(default)]
    pub page_key: serde_json::Value,
    #[serde(default)]
    pub guards: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub omitted_actions: u32,

    /// Whether this tab has somewhere to go back to. Backends report it cheaply; the runtime
    /// offers `BACK` only when it is true.
    #[serde(default)]
    pub can_go_back: bool,

    /// Monotonic per-session observation counter, assigned by the runtime. Decisions carry the
    /// generation they were made against so a resumed decision can never execute against a
    /// later page.
    #[serde(default)]
    pub generation: u64,

    /// Optional, never populated in the normal loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

impl Snapshot {
    pub fn action(&self, id: &str) -> Option<&RawAction> {
        self.actions.iter().find(|a| a.id == id)
    }

    /// A short, log-safe description used in traces and host summaries.
    pub fn summary(&self, text_budget: usize) -> String {
        let text: String = self.text.chars().take(text_budget).collect();
        format!("{} — {}\n{}", self.title, self.url, text)
    }
}
